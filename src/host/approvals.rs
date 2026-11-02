//! Captain approvals and standing authorizations reaching a running hosted
//! Claude session. Claude reads its inline settings, and so the approvals and
//! authorizations its auto-mode classifier is told, only at launch
//! (tests/fixtures/claude/settings-reload.md). When they change, a pane that
//! can refresh stops its harness at a safe point and starts it again in the
//! same pane on the same conversation, as a wake does, without asking: the
//! captain allowed this routinely.
//!
//! A narrowing (a revoked approval or authorization, an expiry, or the
//! approval's task ending) refreshes the same way, since a sentence left in a
//! running classifier's allow list is an authorization the captain believes
//! is gone. It still waits for the safe point: a turn in progress keeps the
//! old list until it ends.
//!
//! Only this ship's own launch inputs count: its approvals and its standing
//! authorizations, plus the fleet-wide mode and tool rules its launch reads.
//! Another ship's authorization is no input of this launch, so it never
//! restarts this pane.
use crate::store::Store;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde_json::json;

/// How long the session must have been idle, with an empty prompt box.
pub(crate) const APPROVALS_IDLE_SECS: u64 = 15;

/// The least time between two refreshes of one pane.
pub(crate) const APPROVALS_GAP_MS: i64 = 300_000;

/// The approval ids and standing authorizations a refresh adds to and removes
/// from the session's launch.
struct Change {
    added: Vec<String>,
    removed: Vec<String>,
    authorized: Vec<String>,
    unauthorized: Vec<String>,
}

/// Whether this pane should restart its session now to apply changed
/// approvals or launch policy. Read only; `fence` decides. All of these hold:
/// - the approvals, or the launch policy, a launch now would carry differ
///   from the session's;
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
    let pane = store
        .connection()
        .query_row(
            "SELECT approvals_digest, approvals_ids, authorizations_digest, authorizations,
           approvals_refreshed_ms FROM hosted_panes
         WHERE ship=?1 AND pid=?2 AND approvals_refresh_pid=?2",
            params![ship, pid],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((Some(launched), ids, policy_digest, texts, refreshed)) = pane else {
        return Ok(None);
    };
    let now = Store::now_ms();
    let current = crate::captain_approvals::current_set(store, ship, now)?;
    // A pane recorded before its launch policy was tracked (a NULL digest)
    // has nothing to compare for it, so only approvals move it; a policy
    // that cannot be read says nothing, so it moves nothing either.
    let Ok(current_policy) = crate::permissions::launch_digest(store, ship) else {
        return Ok(None);
    };
    let policy_moved = policy_digest.is_some_and(|launched| launched != current_policy);
    if (crate::captain_approvals::set_digest(&current) == launched && !policy_moved)
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
        .and_then(|ids| serde_json::from_str(ids.as_str()).ok())
        .unwrap_or_default();
    let after: Vec<String> = current.into_iter().map(|(id, _)| id).collect();
    let before_texts: Vec<String> = texts
        .and_then(|texts| serde_json::from_str(texts.as_str()).ok())
        .unwrap_or_default();
    let after_texts = crate::permissions::authorizations_for(store, ship)?;
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
        authorized: after_texts
            .iter()
            .filter(|text| !before_texts.contains(text))
            .cloned()
            .collect(),
        unauthorized: before_texts
            .iter()
            .filter(|text| !after_texts.contains(text))
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
        &json!({"ship": ship, "added": change.added, "removed": change.removed,
            "authorized": change.authorized, "unauthorized": change.unauthorized}),
    )?;
    tx.commit()?;
    Ok(Some(fresh))
}
