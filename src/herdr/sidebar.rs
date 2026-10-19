//! Read-only recovery of sidebar identity from live hosted presence leases.
//! Labels are never evidence of ownership, nor is an expired lease.
use super::Binding;
use crate::store::Store;
use anyhow::{Result, ensure};
use rusqlite::{Connection, OpenFlags, params};
use std::{
    path::Path,
    time::{Duration, Instant},
};

// Presentation must not hold up local repair, mail or a launch behind a
// fleet's worth of slow databases. Registry and homes share this deadline.
const FLEET_BUDGET: Duration = Duration::from_millis(200);

pub(crate) fn mates(conn: &Connection) -> Result<Vec<(String, Binding)>> {
    let mut query = conn.prepare(
        "SELECT p.ship, p.binding FROM presence p JOIN ships s ON s.name=p.ship
         WHERE p.expires_ms>?1 AND p.binding IS NOT NULL
           AND json_extract(s.record,'$.location')='here'
           AND json_extract(s.record,'$.first_mate.host')='herdr'
         ORDER BY p.ship LIMIT 200",
    )?;
    let rows = query.query_map(params![Store::now_ms()], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    Ok(rows
        .filter_map(|row| {
            let (ship, raw) = row.ok()?;
            Some((ship, serde_json::from_str(&raw).ok()?))
        })
        .collect())
}

fn connection(path: &Path, deadline: Instant) -> Result<Connection> {
    ensure!(Instant::now() < deadline, "sidebar fleet budget spent");
    ensure!(path.is_absolute(), "sidebar path must be absolute");
    crate::store::reject_symlinks(path)?;
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // Skip contention rather than repeatedly spending the remaining budget
    // on busy retries. This also bounds waits during prepare/schema reads,
    // where SQLite need not invoke the VM progress callback.
    conn.busy_timeout(Duration::ZERO)?;
    conn.progress_handler(1_000, Some(move || Instant::now() >= deadline));
    ensure!(Instant::now() < deadline, "sidebar fleet budget spent");
    Ok(conn)
}

/// Do not create homes, migrate schemas or let a broken fleet member prevent
/// recovery for the rest. The dispatcher's own connection is a separate fallback.
pub(super) fn fleet_mates() -> Vec<(String, Binding)> {
    let deadline = Instant::now() + FLEET_BUDGET;
    let read = || -> Result<Vec<(String, std::path::PathBuf)>> {
        let conn = connection(&crate::fleet::registry_path()?, deadline)?;
        Ok(conn
            .prepare(
                "SELECT name,home FROM ships WHERE retired_ms IS NULL ORDER BY name LIMIT 200",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get::<_, String>(1)?.into())))?
            .collect::<rusqlite::Result<_>>()?)
    };
    let mut found = vec![];
    for (name, home) in read().unwrap_or_default() {
        if Instant::now() >= deadline {
            break;
        }
        if let Ok(conn) = connection(&home.join("privateer.sqlite"), deadline) {
            found.extend(
                mates(&conn)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|(ship, _)| ship == &name),
            );
        }
    }
    found
}

/// Each hosted mate's pane status and recent screen, for the status and for
/// auto-mode lockout detection. A pane whose screen cannot be read has an
/// empty screen.
pub(crate) fn repair(store: &Store, socket: &Path) -> Result<Vec<(String, String, String)>> {
    let mates = mates(store.connection())?;
    if mates.is_empty() {
        return Ok(vec![]);
    }
    let mut client = super::Client::connect(socket)?;
    let Some(view) = client.arrange_observed_with_mates(&mates)? else {
        return Ok(vec![]);
    };
    let mut observed = vec![];
    for (ship, binding) in mates.iter().filter(|(_, binding)| binding.socket == socket) {
        let Some(pane) = view.panes.iter().find(|pane| binding.matches(&view, pane)) else {
            continue;
        };
        let screen = client.read_visible_text(&pane.pane_id).unwrap_or_default();
        observed.push((ship.clone(), pane.agent_status.clone(), screen));
    }
    Ok(observed)
}
