//! Bounded fleet observations. Never open a runtime Store: even fleet discovery
//! must work before the first home exists, without initializing state.
use super::model::{
    ROW_LIMIT, Ship, Snapshot, read_registry, validate_navigation, validate_registry,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ShipState {
    pub home: PathBuf,
    #[serde(default)]
    pub identity: Option<super::bridge::Identity>,
    pub liaison: bool,
    pub binding: Option<crate::herdr::Binding>,
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
    use std::io::Read;
    let config = home.join("config.json");
    crate::store::reject_symlinks(&config)?;
    let mut raw = Vec::new();
    if let Ok(file) = std::fs::File::open(config) {
        file.take(65_537).read_to_end(&mut raw)?;
    }
    ensure!(raw.len() <= 65_536, "ship config exceeds 64 KiB");
    let liaison =
        serde_json::from_slice::<serde_json::Value>(&raw).is_ok_and(|c| c["preset"] == "liaison");
    let conn = connection(&home.join("privateer.sqlite"))?;
    let description: Option<String> = conn.query_row(
        "SELECT json_extract(record,'$.first_mate.manifest') FROM ships WHERE name=?1 AND json_extract(record,'$.location')='here'",
        [name], |r| r.get(0)).context("registered ship is not hosted here")?;
    let harness = description
        .as_deref()
        .and_then(crate::bundled::harness)
        .unwrap_or_default();
    let attended = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM presence WHERE ship=?1 AND expires_ms>?2)",
        params![name, crate::store::Store::now_ms()],
        |r| r.get(0),
    )?;
    let has_binding: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('presence') WHERE name='binding')",
        [],
        |r| r.get(0),
    )?;
    let binding = if has_binding {
        conn.query_row(
            "SELECT binding FROM presence WHERE ship=?1 AND expires_ms>?2",
            params![name, crate::store::Store::now_ms()],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
    } else {
        None
    };
    let requests = conn.query_row(
        "SELECT count(*) FROM mail WHERE (sender=?1 OR recipient=?1) AND state='open'",
        [name],
        |r| r.get(0),
    )?;
    let unread = conn.query_row("SELECT count(*) FROM mail WHERE recipient=?1 AND COALESCE(delivery,'received')='received' AND seq>COALESCE((SELECT through_seq FROM mail_cursor WHERE address=?1),0)", [name], |r| r.get(0))?;
    let wakes = conn.prepare("SELECT substr(mode,1,40),substr(status,1,40) FROM wakes WHERE ship=?1 ORDER BY created_ms DESC LIMIT 3")?.query_map([name], |r| Ok(Wake {mode:r.get(0)?, status:r.get(1)?}))?.collect::<rusqlite::Result<_>>()?;
    Ok(ShipState {
        home: home.into(),
        identity: Some(super::bridge::Identity::read(home)?),
        liaison,
        harness,
        binding,
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
        return fallback.map_or(Ok(Snapshot::default()), |home| {
            let mut snapshot = Snapshot::read(home)?;
            let identity = super::bridge::Identity::read(home)?;
            for ship in &mut snapshot.ships {
                ship.live = Some(ShipState {
                    home: home.to_path_buf(),
                    identity: Some(identity.clone()),
                    ..ShipState::default()
                });
            }
            Ok(snapshot)
        });
    }
    let mut out = Snapshot {
        truncated: members.len() > ROW_LIMIT,
        ..Snapshot::default()
    };
    let mut observed_homes = BTreeSet::new();
    let mut claims = Vec::new();
    for (name, home) in members.into_iter().take(ROW_LIMIT) {
        crate::store::identifier(&name)?;
        let config = read_registry(&home);
        let validation = config
            .as_ref()
            .map_err(|error| error.to_string())
            .and_then(|ships| validate_registry(ships).map_err(|error| error.to_string()));
        if let Err(error) = validation {
            out.presentation_error = Some(error);
        }
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
            let snapshot = Snapshot::read_work(&home)?;
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
                if let Some(config) = config
                    .as_ref()
                    .ok()
                    .and_then(|ships| ships.iter().find(|s| s.id == name))
                {
                    ship = config.clone();
                }
                ship.description = scope;
                ship.live = Some(live);
                if observed_homes.insert(home) {
                    out.truncated |= local.truncated;
                    out.recent.extend(local.recent);
                    out.tasks.extend(local.tasks);
                    out.pending.extend(local.pending);
                    out.activity.extend(local.activity);
                    out.jobs.extend(local.jobs);
                    out.custody.extend(local.custody);
                }
            }
            Err(e) => ship.live.as_mut().unwrap().error = Some(e.to_string()),
        }
        out.ships.push(ship);
        if let Ok(config) = config {
            claims.extend(config);
        }
    }
    for mut claim in claims {
        if let Some(ship) = out.ships.iter_mut().find(|ship| ship.id == claim.id) {
            ship.liaison |= claim.liaison;
            for terminal in claim.terminals {
                if !ship.terminals.iter().any(|binding| {
                    binding.pane_id == terminal.pane_id
                        && binding.terminal_id == terminal.terminal_id
                }) {
                    ship.terminals.push(terminal);
                }
            }
        } else {
            claim.live = None;
            out.ships.push(claim);
        }
    }
    out.truncated |= out.recent.len() > ROW_LIMIT
        || out.tasks.len() > ROW_LIMIT
        || out.pending.len() > ROW_LIMIT
        || out.activity.len() > ROW_LIMIT
        || out.jobs.len() > ROW_LIMIT
        || out.custody.len() > ROW_LIMIT;
    out.recent
        .sort_by_key(|r| std::cmp::Reverse(r.completed_ms));
    out.recent.truncate(ROW_LIMIT);
    out.tasks.truncate(ROW_LIMIT);
    out.pending.truncate(ROW_LIMIT);
    out.activity.truncate(ROW_LIMIT);
    out.jobs.truncate(ROW_LIMIT);
    out.custody.truncate(ROW_LIMIT);
    out.include_observed_ships();
    if !out.ships.iter().any(|ship| ship.liaison) {
        let mut candidates = out
            .ships
            .iter_mut()
            .filter(|ship| ship.live.as_ref().is_some_and(|live| live.liaison));
        if let (Some(candidate), None) = (candidates.next(), candidates.next()) {
            candidate.liaison = true;
        }
    }
    if let Err(error) = validate_navigation(&out.ships) {
        out.presentation_error = Some(error.to_string());
    }
    if out.presentation_error.is_some() {
        for ship in &mut out.ships {
            ship.liaison = false;
        }
    }
    // The captain's contact leads the list; the rest keep registry order.
    out.ships.sort_by_key(|ship| !ship.liaison);
    out.truncated |= out.ships.len() > ROW_LIMIT;
    out.ships.truncate(ROW_LIMIT);
    Ok(out)
}

pub fn lines(live: &ShipState) -> Vec<ratatui::text::Line<'static>> {
    use super::model::{clean, count};
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
            "{} · {} unread mail",
            count(live.requests, "open request", "open requests"),
            live.unread
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
