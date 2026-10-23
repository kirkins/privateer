//! Captain approvals reaching a running hosted Claude session. Claude reads
//! its inline settings, and so the approvals its auto-mode classifier is
//! told, only at launch (tests/fixtures/claude/settings-reload.md). When the
//! ship's approvals change, a pane that can refresh stops its harness at a
//! safe point and starts it again in the same pane on the same conversation,
//! as a wake does, without asking: the captain allowed this routinely.
//!
//! A narrowing (a revoke, an expiry, or the approval's task ending) refreshes
//! the same way, since a sentence left in a running classifier's allow list
//! is an authorization the captain believes is gone. It still waits for the
//! safe point: a turn in progress keeps the old list until it ends.
use crate::store::Store;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde_json::json;

/// How long the session must have been idle, with an empty prompt box.
pub(crate) const APPROVALS_IDLE_SECS: u64 = 15;

/// The least time between two refreshes of one pane.
pub(crate) const APPROVALS_GAP_MS: i64 = 300_000;

/// The approval ids a refresh adds to and removes from the session's launch.
struct Change {
    added: Vec<String>,
    removed: Vec<String>,
}

/// Whether this pane should restart its session now to apply changed
/// approvals. Read only; `fence` decides. All of these hold:
/// - the approvals a launch now would carry differ from the session's;
/// - this pane process recorded that it can refresh (a hosted Claude session
///   with no bounded plan, continued with `--continue`);
/// - no turn runs, and the adapter has reported idle for
///   `APPROVALS_IDLE_SECS` with nothing ready; a permission prompt or an
///   auto-mode lockout is mid-turn, so it is never safe;
/// - the ship is not held on a tool-permission question;
/// - the pane has not refreshed in the last `APPROVALS_GAP_MS`;
/// - the folder has an interactive conversation `--continue` resumes, and it
///   is this pane's: a pane that started fresh (on a new route) has none
///   until its session writes one, and the refresh would start fresh again.
///   Context is never traded for an approval.
fn due(store: &Store, ship: &str, holder: &str) -> Result<Option<Change>> {
    let pid = std::process::id();
    let pane: Option<(Option<String>, Option<String>, Option<i64>)> = store
        .connection()
        .query_row(
            "SELECT approvals_digest, approvals_ids, approvals_refreshed_ms FROM hosted_panes
             WHERE ship=?1 AND pid=?2 AND approvals_refresh_pid=?2",
            params![ship, pid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((Some(launched), ids, refreshed)) = pane else {
        return Ok(None);
    };
    let now = Store::now_ms();
    let current = crate::captain_approvals::current_set(store, ship, now)?;
    if crate::captain_approvals::set_digest(&current) == launched
        || super::upgrade::busy(store, ship)?
        || !super::sleep::idle_for(store, ship, holder, APPROVALS_IDLE_SECS)?
        || crate::permissions::paused(store, ship)?
        || refreshed.is_some_and(|at| now - at < APPROVALS_GAP_MS)
    {
        return Ok(None);
    }
    let Some(folder) = store.home().parent() else {
        return Ok(None);
    };
    if !super::claude_conversation(&folder.canonicalize()?).continues()
        || super::upgrade::fresh_unwritten(store, ship, folder)?
    {
        return Ok(None);
    }
    let before: Vec<String> = ids
        .and_then(|ids| serde_json::from_str(&ids).ok())
        .unwrap_or_default();
    let after: Vec<String> = current.into_iter().map(|(id, _)| id).collect();
    Ok(Some(Change {
        added: after
            .iter()
            .filter(|id| !before.contains(id))
            .cloned()
            .collect(),
        removed: before
            .iter()
            .filter(|id| !after.contains(id))
            .cloned()
            .collect(),
    }))
}

/// Move the ship's lease from a running harness to a new hosted holder for
/// its refreshed session, in one transaction with the last check, before the
/// harness is stopped. The adapter checks ownership before each delivery, so
/// once this commits the old session starts no turn. Returns the new holder,
/// or None if a refresh is not due.
pub(crate) fn fence(store: &Store, ship: &str, holder: &str) -> Result<Option<String>> {
    let tx = store.write_transaction()?;
    let presence = crate::presence::presence(store, ship)?;
    if presence["attended"] != true || presence["lease"]["holder"] != holder {
        return Ok(None);
    }
    let Some(change) = due(store, ship, holder)? else {
        return Ok(None);
    };
    let fresh = format!("{}{}", crate::presence::HOSTED, uuid::Uuid::new_v4());
    crate::presence::hand_over(store, ship, holder, &fresh)?;
    // Launch backoff and a pending relaunch request follow the session.
    tx.execute(
        "UPDATE host_launches SET holder=?3 WHERE ship=?1 AND holder=?2",
        params![ship, holder, fresh],
    )?;
    tx.execute(
        "UPDATE hosted_relaunches SET holder=?3 WHERE ship=?1 AND holder=?2",
        params![ship, holder, fresh],
    )?;
    tx.execute("DELETE FROM work_sessions WHERE ship=?1", [ship])?;
    tx.execute(
        "UPDATE hosted_panes SET approvals_refreshed_ms=?3 WHERE ship=?1 AND pid=?2",
        params![ship, std::process::id(), Store::now_ms()],
    )?;
    store.audit(
        "host.approvals_refresh",
        &json!({"ship": ship, "added": change.added, "removed": change.removed}),
    )?;
    tx.commit()?;
    Ok(Some(fresh))
}
