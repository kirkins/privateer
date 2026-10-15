//! Bounded fleet observations. Never open a runtime Store: even fleet discovery
//! must work before the first home exists, without initializing state.
use super::model::{ROW_LIMIT, Ship, Snapshot};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ShipState {
    pub home: PathBuf,
    pub harness: String,
    pub attended: bool,
    pub dispatcher: bool,
    pub requests: usize,
    pub unread: usize,
    pub wakes: Vec<Wake>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Wake {
    pub mode: String,
    pub status: String,
}

pub fn connection(path: &Path) -> Result<Connection> {
    ensure!(path.is_absolute(), "dashboard path must be absolute");
    crate::store::reject_symlinks(path)?;
    ensure!(path.is_file(), "{} does not exist", path.display());
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_millis(100))?;
    let deadline = Instant::now() + Duration::from_millis(500);
    conn.progress_handler(10_000, Some(move || Instant::now() >= deadline));
    Ok(conn)
}

fn state(home: &Path, name: &str) -> Result<ShipState> {
    ensure!(home.is_dir(), "ship home is missing: {}", home.display());
    let conn = connection(&home.join("privateer.sqlite"))?;
    let description: Option<String> = conn.query_row(
        "SELECT json_extract(record,'$.first_mate.manifest') FROM ships WHERE name=?1 AND json_extract(record,'$.location')='here'",
        [name], |r| r.get(0)).context("registered ship is not hosted here")?;
    let harness = description
        .and_then(|p| {
            Path::new(&p)
                .parent()?
                .file_name()?
                .to_str()
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let attended = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM presence WHERE ship=?1 AND expires_ms>?2)",
        params![name, crate::store::Store::now_ms()],
        |r| r.get(0),
    )?;
    let requests = conn.query_row(
        "SELECT count(*) FROM mail WHERE (sender=?1 OR recipient=?1) AND state='open'",
        [name],
        |r| r.get(0),
    )?;
    let unread = conn.query_row("SELECT count(*) FROM mail WHERE recipient=?1 AND COALESCE(delivery,'received')='received' AND seq>COALESCE((SELECT through_seq FROM mail_cursor WHERE address=?1),0)", [name], |r| r.get(0))?;
    let wakes = conn.prepare("SELECT substr(mode,1,40),substr(status,1,40) FROM wakes WHERE ship=?1 ORDER BY created_ms DESC LIMIT 3")?.query_map([name], |r| Ok(Wake {mode:r.get(0)?, status:r.get(1)?}))?.collect::<rusqlite::Result<_>>()?;
    Ok(ShipState {
        home: home.into(),
        harness,
        attended,
        dispatcher: crate::service::running(home)?,
        requests,
        unread,
        wakes,
        error: None,
    })
}

/// Registry schema is the read-only equivalent of fleet.list. Missing registry
/// means an empty fleet, not permission to create a runtime home.
pub fn read(registry: &Path, fallback: Option<&Path>) -> Result<Snapshot> {
    crate::store::reject_symlinks(registry)?;
    ensure!(registry.is_absolute(), "fleet registry must be absolute");
    let members: Vec<(String, PathBuf)> = if registry.exists() {
        let conn = connection(registry)?;
        conn.prepare(
            "SELECT name,home FROM ships WHERE retired_ms IS NULL ORDER BY name LIMIT 201",
        )?
        .query_map([], |r| {
            Ok((r.get(0)?, PathBuf::from(r.get::<_, String>(1)?)))
        })?
        .collect::<rusqlite::Result<_>>()?
    } else {
        Vec::new()
    };
    if members.is_empty() {
        return fallback.map_or(Ok(Snapshot::default()), Snapshot::read);
    }
    let mut out = Snapshot {
        truncated: members.len() > ROW_LIMIT,
        ..Snapshot::default()
    };
    for (name, home) in members.into_iter().take(ROW_LIMIT) {
        crate::store::identifier(&name)?;
        let mut ship = Ship {
            id: name.clone(),
            name: name.clone(),
            live: Some(ShipState {
                home: home.clone(),
                ..ShipState::default()
            }),
            ..Ship::default()
        };
        let observation = (|| -> Result<_> {
            // READ_ONLY and the existence check both matter: never resurrect a
            // deleted ship, even when its registry entry survives.
            ensure!(home.is_dir(), "ship home is missing: {}", home.display());
            let snapshot = Snapshot::read(&home)?;
            let live = state(&home, &name)?;
            let conn = connection(&home.join("privateer.sqlite"))?;
            let scope: Option<String> = conn
                .query_row(
                    "SELECT substr(json_extract(record,'$.scope'),1,512) FROM ships WHERE name=?1",
                    [&name],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            Ok((snapshot, live, scope.unwrap_or_default()))
        })();
        match observation {
            Ok((local, live, scope)) => {
                if let Some(config) = local.ships.iter().find(|s| s.id == name) {
                    ship = config.clone();
                }
                ship.description = scope;
                ship.live = Some(live);
                out.truncated |= local.truncated;
                // These lists describe the registered ship's work in its own
                // home; remote directory identities cannot establish custody.
                out.tasks.extend(
                    local
                        .tasks
                        .into_iter()
                        .filter(|t| t.owner == name || t.handoff_to.as_deref() == Some(&name)),
                );
                out.pending
                    .extend(local.pending.into_iter().filter(|n| n.owner == name));
                out.activity
                    .extend(local.activity.into_iter().filter(|n| n.owner == name));
                out.jobs
                    .extend(local.jobs.into_iter().filter(|j| j.owner == name));
                out.custody.extend(
                    local
                        .custody
                        .into_iter()
                        .filter(|c| c.owner == name || c.actor == name),
                );
            }
            Err(e) => ship.live.as_mut().unwrap().error = Some(e.to_string()),
        }
        out.ships.push(ship);
    }
    out.truncated |= out.tasks.len() > ROW_LIMIT
        || out.pending.len() > ROW_LIMIT
        || out.activity.len() > ROW_LIMIT
        || out.jobs.len() > ROW_LIMIT
        || out.custody.len() > ROW_LIMIT;
    out.tasks.truncate(ROW_LIMIT);
    out.pending.truncate(ROW_LIMIT);
    out.activity.truncate(ROW_LIMIT);
    out.jobs.truncate(ROW_LIMIT);
    out.custody.truncate(ROW_LIMIT);
    Ok(out)
}

pub fn lines(live: &ShipState) -> Vec<ratatui::text::Line<'static>> {
    use super::model::clean;
    use ratatui::text::Line;
    if let Some(error) = &live.error {
        return vec![Line::from(format!("Unavailable: {}", clean(error, 512)))];
    }
    let mut out = vec![
        Line::from(format!(
            "{} · harness {} · dispatcher {}",
            if live.attended {
                "Aboard"
            } else {
                "Not aboard"
            },
            clean(&live.harness, 80),
            if live.dispatcher {
                "running"
            } else {
                "stopped"
            }
        )),
        Line::from(format!(
            "{} open requests · {} unread mail",
            live.requests, live.unread
        )),
    ];
    for wake in &live.wakes {
        out.push(Line::from(format!(
            "Wake: {} · {}",
            clean(&wake.mode, 40),
            clean(&wake.status, 40)
        )));
    }
    out
}
