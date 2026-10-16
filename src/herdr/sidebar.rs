//! Read-only recovery of sidebar identity from live hosted presence leases.
//! Labels are never evidence of ownership, nor is an expired lease.
use super::Binding;
use crate::store::Store;
use anyhow::{Result, ensure};
use rusqlite::{Connection, OpenFlags, params};
use std::{path::Path, time::Duration};

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

fn connection(path: &Path) -> Result<Connection> {
    ensure!(path.is_absolute(), "sidebar path must be absolute");
    crate::store::reject_symlinks(path)?;
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_millis(100))?;
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    conn.progress_handler(10_000, Some(move || std::time::Instant::now() >= deadline));
    Ok(conn)
}

/// Do not create homes, migrate schemas or let a broken fleet member prevent
/// recovery for the rest. The dispatcher's own connection is a separate fallback.
pub(super) fn fleet_mates() -> Vec<(String, Binding)> {
    let read = || -> Result<Vec<(String, std::path::PathBuf)>> {
        let conn = connection(&crate::fleet::registry_path()?)?;
        Ok(conn
            .prepare(
                "SELECT name,home FROM ships WHERE retired_ms IS NULL ORDER BY name LIMIT 200",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get::<_, String>(1)?.into())))?
            .collect::<rusqlite::Result<_>>()?)
    };
    let mut found = vec![];
    for (name, home) in read().unwrap_or_default() {
        if let Ok(conn) = connection(&home.join("privateer.sqlite")) {
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

pub(crate) fn repair(store: &Store, socket: &Path) -> Result<()> {
    let mates = mates(store.connection())?;
    if mates.is_empty() {
        return Ok(());
    }
    super::Client::connect(socket)?.arrange_with_mates(&mates)
}
