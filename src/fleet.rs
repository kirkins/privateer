//! The captain's fleet on this machine: every ship folder, linked so ships
//! can mail each other without manual peer setup, and ships creating and
//! retiring other ships.
//!
//! The registry is a small per-user SQLite file (`PRIVATEER_FLEET`, or
//! `$XDG_STATE_HOME/privateer/fleet.sqlite`, default
//! `~/.local/state/privateer/fleet.sqlite`). Linking gives each home a
//! local-transport peer for every other active ship and a directory entry
//! behind it. A ship may create another ship, but only with a charter whose
//! actions its own charter already has: delegation never widens authority.
//! It may retire only ships it created. Retiring revokes the ship's charter,
//! stops its dispatcher and unlinks it, and keeps its folder.
use crate::store::{Caller, Store, identifier, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub fn registry_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PRIVATEER_FLEET") {
        return Ok(PathBuf::from(path));
    }
    let state = match std::env::var_os("XDG_STATE_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => {
            PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".local/state")
        }
    };
    Ok(state.join("privateer/fleet.sqlite"))
}

fn registry() -> Result<Connection> {
    let path = registry_path()?;
    ensure!(
        path.is_absolute(),
        "the fleet registry path must be absolute"
    );
    let dir = path.parent().context("fleet registry directory")?;
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let conn = Connection::open(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ships(name TEXT PRIMARY KEY, home TEXT NOT NULL,
           created_by TEXT NOT NULL, created_ms INTEGER NOT NULL, retired_ms INTEGER);",
    )?;
    Ok(conn)
}

struct Member {
    name: String,
    home: PathBuf,
    created_by: String,
}

fn members(conn: &Connection) -> Result<Vec<Member>> {
    let mut statement = conn.prepare(
        "SELECT name, home, created_by FROM ships WHERE retired_ms IS NULL ORDER BY name",
    )?;
    let rows = statement.query_map([], |r| {
        Ok(Member {
            name: r.get(0)?,
            home: PathBuf::from(r.get::<_, String>(1)?),
            created_by: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Add a ship to the fleet. A name belongs to one home while it is active.
pub fn register(ship: &str, home: &Path, created_by: &str) -> Result<()> {
    let conn = registry()?;
    let existing: Option<(String, Option<i64>)> = conn
        .query_row(
            "SELECT home, retired_ms FROM ships WHERE name=?1",
            [ship],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let home_text = home.to_string_lossy();
    match existing {
        // A ship whose folder was deleted no longer holds its name.
        Some((other, None)) if other != home_text && Path::new(&other).exists() => bail!(
            "the fleet already has a ship named {ship} at {other}; choose another name with --ship"
        ),
        Some((other, None)) if other == home_text => Ok(()),
        _ => {
            conn.execute(
                "INSERT INTO ships(name, home, created_by, created_ms) VALUES(?1, ?2, ?3, ?4)
                 ON CONFLICT(name) DO UPDATE SET home=excluded.home, created_by=excluded.created_by,
                   created_ms=excluded.created_ms, retired_ms=NULL",
                params![ship, home_text, created_by, Store::now_ms()],
            )?;
            Ok(())
        }
    }
}

/// Make `other` reachable from `store`: a local-transport peer to its home
/// and a directory entry behind it. Ships this home hosts are never touched.
fn reach(store: &mut Store, other: &str, other_home: &Path, scope: &str) -> Result<bool> {
    let current = crate::mail::dispatch(store, "ship.show", json!({"name": other})).ok();
    if current.as_ref().is_some_and(|s| s["location"] == "here") {
        return Ok(false);
    }
    let peer = format!("fleet-{other}");
    let binary = std::env::current_exe()?;
    crate::peers::dispatch(
        store,
        "peer.put",
        json!({"name": peer, "transport": "local", "executable": binary, "home": other_home}),
    )?;
    crate::mail::dispatch(
        store,
        "ship.put",
        json!({"name": other, "scope": scope, "location": {"peer": peer}}),
    )?;
    Ok(true)
}

fn scope_of(store: &mut Store, ship: &str) -> String {
    crate::mail::dispatch(store, "ship.show", json!({"name": ship}))
        .ok()
        .and_then(|s| s["scope"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Link a ship with every other active ship in the fleet, both ways.
pub fn link(store: &Store, ship: &str) -> Result<usize> {
    let conn = registry()?;
    let mut ours = Store::open(store.home())?;
    let our_scope = scope_of(&mut ours, ship);
    let mut linked = 0;
    for member in members(&conn)? {
        // Opening a home creates it; never resurrect a deleted ship folder.
        if member.name == ship || member.home == store.home() || !member.home.is_dir() {
            continue;
        }
        let Ok(mut theirs) = Store::open(&member.home) else {
            continue;
        };
        let their_scope = scope_of(&mut theirs, &member.name);
        if reach(&mut ours, &member.name, &member.home, &their_scope)? {
            linked += 1;
        }
        reach(&mut theirs, ship, store.home(), &our_scope)?;
    }
    Ok(linked)
}

fn caller_ship(store: &Store) -> Option<String> {
    match store.caller() {
        Caller::Agent { actor } => Some(actor.clone()),
        Caller::Operator => crate::presence::folder_ship(store),
    }
}

/// Create a ship from inside a ship. An agent's new ship gets at most its
/// own charter's actions.
fn create(store: &mut Store, args: &Value) -> Result<Value> {
    let name = required(args, "name")?;
    identifier(name)?;
    ensure!(
        name != crate::mail::CAPTAIN,
        "captain is reserved for the person"
    );
    let creator = caller_ship(store);
    let mut actions = args.get("actions").filter(|v| !v.is_null()).cloned();
    if let Caller::Agent { actor } = store.caller() {
        let own = store.charter(actor)?["actions"].clone();
        let own: Vec<&str> = own
            .as_array()
            .context("creating ship's charter has no actions")?
            .iter()
            .filter_map(Value::as_str)
            .collect();
        match &actions {
            Some(requested) => {
                let requested = requested.as_array().context("actions must be a list")?;
                for action in requested {
                    let action = action.as_str().context("actions are names")?;
                    ensure!(
                        own.contains(&action),
                        "{actor} cannot give a new ship the {action} action its own charter lacks"
                    );
                }
            }
            None => actions = Some(json!(own)),
        }
    }
    let folder = match args.get("folder").filter(|v| !v.is_null()) {
        Some(path) => PathBuf::from(path.as_str().context("folder must be a path")?),
        None => store
            .home()
            .parent()
            .and_then(Path::parent)
            .context("cannot place the new ship beside this one; pass folder")?
            .join(name),
    };
    ensure!(folder.is_absolute(), "folder must be absolute");
    let user_home = std::env::var_os("HOME").map(PathBuf::from);
    ensure!(
        Some(&folder) != user_home.as_ref() && folder.parent().is_some(),
        "a ship needs a dedicated folder"
    );
    ensure!(
        !folder.exists() || fs::read_dir(&folder)?.next().is_none(),
        "{} already exists and is not empty",
        folder.display()
    );
    let harness = match args.get("harness").and_then(Value::as_str) {
        Some(h) => Some(h.to_owned()),
        None => creator.as_ref().and_then(|c| {
            crate::mail::dispatch(store, "ship.show", json!({"name": c}))
                .ok()?["first_mate"]["manifest"]
                .as_str()
                .and_then(|m| Path::new(m).parent()?.file_name()?.to_str().map(str::to_owned))
        }),
    };
    fs::create_dir_all(&folder)?;
    let home = folder.join(crate::cli::STATE_DIR);
    let created_by = creator.clone().unwrap_or_else(|| "captain".into());
    let (ship, harness, _) = crate::cli::setup(
        &home,
        &crate::cli::Setup {
            ship: Some(name.to_owned()),
            scope: args.get("scope").and_then(Value::as_str).map(str::to_owned),
            harness,
            actions,
            objective: args
                .get("objective")
                .and_then(Value::as_str)
                .map(str::to_owned),
            created_by: created_by.clone(),
            liaison: false,
        },
    )?;
    let dispatcher = crate::service::ensure(&home, &ship)?;
    store.audit(
        "ship.created",
        &json!({"ship": ship, "folder": folder, "by": created_by}),
    )?;
    let charter = Store::open(&home)?.charter(&ship)?;
    Ok(json!({"ship": ship, "folder": folder, "harness": harness,
        "actions": charter["actions"], "created_by": created_by, "dispatcher": dispatcher}))
}

/// Retire a ship: revoke its charter, stop its dispatcher, unlink it from
/// the fleet. Its folder is kept for the captain to archive or delete.
fn retire(store: &mut Store, args: &Value) -> Result<Value> {
    let name = required(args, "name")?;
    let conn = registry()?;
    let member = members(&conn)?
        .into_iter()
        .find(|m| m.name == name)
        .with_context(|| format!("no active ship named {name} in the fleet"))?;
    if let Some(expected) = args.get("expected_home").and_then(Value::as_str) {
        ensure!(
            member.home == Path::new(expected),
            "ship home changed since selection; refresh before retiring"
        );
    }
    if let Caller::Agent { actor } = store.caller() {
        ensure!(
            &member.created_by == actor,
            "{actor} can retire only ships it created; {name} was created by {}",
            member.created_by
        );
    }
    let dispatcher = if member.home.is_dir() {
        let mut target = Store::open(&member.home)?;
        if target.charter(name).is_ok() {
            crate::core::dispatch(&mut target, "charter.revoke", json!({"id": name}))?;
        }
        crate::service::stop(&member.home, name)
            .unwrap_or_else(|error| format!("could not stop its dispatcher: {error:#}"))
    } else {
        "its folder was already gone".into()
    };
    conn.execute(
        "UPDATE ships SET retired_ms=?2 WHERE name=?1",
        params![name, Store::now_ms()],
    )?;
    let peer = format!("fleet-{name}");
    for other in members(&conn)? {
        if !other.home.is_dir() {
            continue;
        }
        if let Ok(home) = Store::open(&other.home) {
            home.connection().execute(
                "DELETE FROM ships WHERE name=?1 AND json_extract(record,'$.location.peer')=?2",
                params![name, peer],
            )?;
        }
    }
    store.audit("ship.retired", &json!({"ship": name}))?;
    Ok(
        json!({"ship": name, "retired": true, "dispatcher": dispatcher,
        "folder_kept": member.home.parent()}),
    )
}

fn list() -> Result<Value> {
    let conn = registry()?;
    let mut statement = conn.prepare(
        "SELECT name, home, created_by, created_ms, retired_ms FROM ships ORDER BY name",
    )?;
    let ships = statement
        .query_map([], |r| {
            Ok(
                json!({"name": r.get::<_, String>(0)?, "home": r.get::<_, String>(1)?,
                "created_by": r.get::<_, String>(2)?, "created_ms": r.get::<_, i64>(3)?,
                "retired_ms": r.get::<_, Option<i64>>(4)?}),
            )
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"registry": registry_path()?, "ships": ships}))
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "fleet.list" => list(),
        "fleet.link" => {
            let ship = crate::presence::folder_ship(store)
                .context("this home's ship is unknown; run `privateer init`")?;
            Ok(json!({"ship": ship, "linked": link(store, &ship)?}))
        }
        "ship.create" => create(store, &args),
        "ship.retire" => retire(store, &args),
        _ => bail!("unknown fleet command {command}"),
    }
}
