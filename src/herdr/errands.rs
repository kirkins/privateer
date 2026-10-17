//! Observational captions only. Presence/launch custody, never display metadata,
//! selects the endpoint. No lifecycle authority, input, navigation or fleet writes.
use super::{Binding, Client};
use crate::store::Store;
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    os::{fd::AsRawFd, unix::fs::FileTypeExt},
    time::{Duration, Instant},
};

const SOURCE: &str = "privateer-errands";
const BUDGET: Duration = Duration::from_secs(2);

pub(crate) fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS errand_captions(ship TEXT PRIMARY KEY, record TEXT NOT NULL);",
    )?;
    Ok(())
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Owner {
    holder: String,
    started_ms: i64,
    harness: String,
    binding: String,
}

/// What this presenter last sent a ship's pane, written before the call, and
/// whether Herdr confirmed it. An unconfirmed caption is sent again next pass.
#[derive(Serialize, Deserialize)]
struct Caption {
    owner: Owner,
    sent: Option<String>,
    delivered: bool,
}

struct Target {
    owner: Owner,
    binding: Binding,
    eligible: bool,
    expires_ms: i64,
}

fn target(store: &Store, ship: &str) -> Result<Option<Target>> {
    let row = store
        .connection()
        .query_row(
            "SELECT p.holder,p.started_ms,p.harness,p.binding,p.expires_ms,s.record,h.holder
         FROM presence p JOIN ships s ON s.name=p.ship
         JOIN host_launches h ON h.ship=p.ship WHERE p.ship=?1",
            [ship],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((holder, started_ms, Some(harness), Some(raw), expires, profile, launched)) = row
    else {
        return Ok(None);
    };
    ensure!(
        holder == launched && expires > Store::now_ms(),
        "host lease expired or replaced"
    );
    let id = holder
        .strip_prefix(crate::presence::HOSTED)
        .ok_or_else(|| anyhow::anyhow!("not a hosted holder"))?;
    uuid::Uuid::parse_str(id)?;
    ensure!(
        crate::presence::folder_ship(store).as_deref() == Some(ship),
        "not this home's ship"
    );
    let profile: Value = serde_json::from_str(&profile)?;
    ensure!(profile["location"] == "here", "ship is not local");
    let binding: Binding = serde_json::from_str(&raw)?;
    let owner = Owner {
        holder,
        started_ms,
        harness,
        binding: raw,
    };
    let first = &profile["first_mate"];
    let native = first["manifest"].as_str().and_then(crate::bundled::harness);
    let duty = first["duty_task"].as_str().unwrap_or("");
    // Captions are observational, but they still describe a live first mate.
    // Use the same effective task authority check as a launch: the duty task
    // must be nonterminal, owned by this ship, under open ancestors, and on an
    // active charter. This read happens inside plan(); it does not launch or
    // mutate anything.
    let active = store.task(duty).is_ok_and(|task| task["owner"] == ship)
        && store.active_charter(duty).is_ok();
    let eligible = active
        && crate::host::hosted(first)
        && first.get("config").is_none()
        && native.as_deref() == Some(owner.harness.as_str());
    Ok(Some(Target {
        owner,
        binding,
        eligible,
        expires_ms: expires,
    }))
}

fn text(value: &str, limit: usize) -> String {
    value
        .chars()
        .take(limit)
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn desired(store: &Store, ship: &str) -> Result<Option<String>> {
    let mut query = store.connection().prepare(
        "SELECT focus FROM errands WHERE ship=?1 AND status='running' ORDER BY created_ms,id",
    )?;
    let focuses = query
        .query_map([ship], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let Some(focus) = focuses.first() else {
        return Ok(None);
    };
    let focus = focus.split_once(':').map_or(focus.as_str(), |(_, id)| id);
    let more = match focuses.len() {
        1 => String::new(),
        n => format!(" +{}", n - 1),
    };
    Ok(Some(format!(
        "{} · reviewing {}{}",
        text(ship, 64),
        text(focus, 64),
        more
    )))
}

fn save(store: &Store, ship: &str, caption: &Caption) -> Result<()> {
    store.connection().execute(
        "INSERT INTO errand_captions(ship,record) VALUES(?1,?2)
         ON CONFLICT(ship) DO UPDATE SET record=excluded.record",
        params![ship, serde_json::to_string(caption)?],
    )?;
    Ok(())
}

fn discard(store: &Store, ship: &str) -> Result<()> {
    store
        .connection()
        .execute("DELETE FROM errand_captions WHERE ship=?1", [ship])?;
    Ok(())
}

/// Called after start/settle and on every tick. Refusals never break errands.
/// A nonblocking per-home lock serializes presenters (including other CLI
/// processes); short SQLite lock acquisition bounds contention. Herdr calls run
/// outside any transaction, so they never hold the home's write lock.
pub(crate) fn reconcile(store: &Store) {
    let run = || -> Result<()> {
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(store.home().join("errand-captions.lock"))?;
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "another caption presenter is active"
        );
        let deadline = Instant::now() + BUDGET;
        let mut query = store.connection().prepare(
            "SELECT ship FROM errand_captions UNION SELECT ship FROM errands WHERE status='running' LIMIT 200",
        )?;
        let ships = query
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(query);
        for ship in ships {
            if Instant::now() >= deadline {
                break;
            }
            // Individual malformed/expired targets must not prevent others.
            let _ = reconcile_one(store, &ship);
        }
        Ok(())
    };
    // Restore even after failure. This is this Store's private connection, not a
    // fleet connection; no foreign home is opened or repaired.
    let conn = store.connection();
    let previous = conn.query_row("PRAGMA busy_timeout", [], |r| r.get::<_, i64>(0));
    if conn.busy_timeout(Duration::from_millis(25)).is_ok() {
        let _ = run();
    }
    if let Ok(previous) = previous {
        let _ = conn.busy_timeout(Duration::from_millis(previous as u64));
    }
}

/// What one pass decided. Only `Publish` sends a caption to Herdr.
enum Step {
    Done,
    Verify(Target),
    Publish(Target, Option<String>),
}

fn reconcile_one(store: &Store, ship: &str) -> Result<()> {
    match plan(store, ship)? {
        Step::Done => Ok(()),
        // An unchanged caption only re-checks its endpoint; refusal drops custody.
        Step::Verify(current) => {
            if matches!(validated_client(store, &current), Err(PublishError::Unsafe)) {
                discard(store, ship)?;
            }
            Ok(())
        }
        Step::Publish(current, want) => {
            let result = publish(store, &current, want.as_deref());
            record(store, ship, &current.owner, want, result)
        }
    }
}

/// Decide from current lifecycle and lease facts, never a stored label, and
/// record the caption as sent before the call: a process that dies mid-call
/// leaves it unconfirmed, so a later pass still clears or resends it.
fn plan(store: &Store, ship: &str) -> Result<Step> {
    let tx = store.write_transaction()?;
    let Some(current) = target(store, ship).ok().flatten() else {
        discard(store, ship)?;
        tx.commit()?;
        return Ok(Step::Done);
    };
    let cached = tx
        .query_row(
            "SELECT record FROM errand_captions WHERE ship=?1",
            [ship],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .and_then(|raw| serde_json::from_str::<Caption>(&raw).ok())
        .filter(|cached| cached.owner == current.owner);
    let want = if current.eligible {
        desired(store, ship)?
    } else {
        None
    };
    let step = match cached {
        None if want.is_none() => {
            discard(store, ship)?;
            Step::Done
        }
        Some(cached) if cached.delivered && cached.sent == want => Step::Verify(current),
        _ => {
            let caption = Caption {
                owner: current.owner.clone(),
                sent: want.clone(),
                delivered: false,
            };
            save(store, ship, &caption)?;
            Step::Publish(current, want)
        }
    };
    tx.commit()?;
    Ok(step)
}

/// Confirm a delivered caption only while the endpoint still belongs to the
/// same owner. A finish during the call is caught by the next pass, which
/// recomputes the caption; a rebind or refusal drops custody.
fn record(
    store: &Store,
    ship: &str,
    owner: &Owner,
    sent: Option<String>,
    result: std::result::Result<(), PublishError>,
) -> Result<()> {
    let tx = store.write_transaction()?;
    let live = target(store, ship)
        .ok()
        .flatten()
        .is_some_and(|current| current.owner == *owner);
    match result {
        Ok(()) if live && sent.is_some() => {
            let caption = Caption {
                owner: owner.clone(),
                sent,
                delivered: true,
            };
            save(store, ship, &caption)?;
        }
        Err(PublishError::Unavailable) if live => {}
        _ => discard(store, ship)?,
    }
    tx.commit()?;
    Ok(())
}

enum PublishError {
    Unsafe,
    Unavailable,
}

fn validated_client(store: &Store, target: &Target) -> std::result::Result<Client, PublishError> {
    let binding = &target.binding;
    crate::store::reject_symlinks(&binding.socket).map_err(|_| PublishError::Unsafe)?;
    let metadata = std::fs::metadata(&binding.socket).map_err(|_| PublishError::Unavailable)?;
    if !metadata.file_type().is_socket() {
        return Err(PublishError::Unsafe);
    }
    let mut client = Client::connect(&binding.socket).map_err(|_| PublishError::Unavailable)?;
    if client.identity != binding.socket_identity {
        return Err(PublishError::Unsafe);
    }
    let view = client.snapshot().map_err(|_| PublishError::Unavailable)?;
    let validate = || -> Result<()> {
        // The snapshot already refuses duplicate pane and terminal identities.
        let pane = view
            .panes
            .iter()
            .find(|p| p.pane_id == binding.pane.pane_id)
            .ok_or_else(|| anyhow::anyhow!("missing pane"))?;
        ensure!(binding.matches(&view, pane), "endpoint replaced");
        ensure!(
            pane.agent
                .as_deref()
                .is_none_or(|a| a == target.owner.harness),
            "incompatible occupant"
        );
        // Refuse any second local presence claiming this endpoint, even if its
        // display labels look convincing.
        let mut query = store
            .connection()
            .prepare("SELECT binding FROM presence WHERE binding IS NOT NULL")?;
        let mut claims = 0;
        for raw in query.query_map([], |r| r.get::<_, String>(0))? {
            let raw = raw?;
            let other: Binding = serde_json::from_str(&raw)?;
            if other.socket == binding.socket
                && other.socket_identity == binding.socket_identity
                && (other.pane.pane_id == pane.pane_id
                    || other.pane.terminal_id == pane.terminal_id)
            {
                claims += 1;
            }
        }
        ensure!(claims == 1, "ambiguous presence");
        Ok(())
    };
    validate().map_err(|_| PublishError::Unsafe)?;
    Ok(client)
}

fn publish(
    store: &Store,
    target: &Target,
    caption: Option<&str>,
) -> std::result::Result<(), PublishError> {
    let binding = &target.binding;
    let mut client = validated_client(store, target)?;
    // A snapshot may consume most of its deadline: never publish after expiry.
    if target.expires_ms <= Store::now_ms() {
        return Err(PublishError::Unsafe);
    }
    let params = match caption {
        Some(text) => json!({"pane_id":binding.pane.pane_id,"source":SOURCE,"display_agent":text}),
        None => json!({"pane_id":binding.pane.pane_id,"source":SOURCE,"clear_display_agent":true}),
    };
    client
        .request("pane.report_metadata", params)
        .map_err(|_| PublishError::Unavailable)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::text;

    #[test]
    fn caption_text_bounds_and_neutralizes_terminal_controls_and_bidi() {
        assert_eq!(text("ok\n\r\u{1b}[31m\u{202e}é", 64), "ok____31m__");
        assert_eq!(text(&"a".repeat(200), 64), "a".repeat(64));
        assert_eq!(text("dashboard-102_v2.rs", 64), "dashboard-102_v2.rs");
    }
}
