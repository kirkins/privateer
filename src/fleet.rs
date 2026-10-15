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
//! It may retire only ships it created, and change the actions of ships it
//! created within its own. Retiring revokes the ship's charter, stops its
//! dispatcher and unlinks it, and keeps its folder.
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

pub(crate) fn registry() -> Result<Connection> {
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

pub(crate) struct Member {
    pub name: String,
    pub home: PathBuf,
    pub created_by: String,
}

pub(crate) fn members(conn: &Connection) -> Result<Vec<Member>> {
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
    let binary = crate::executable::installed()?;
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

pub(crate) fn caller_ship(store: &Store) -> Option<String> {
    match store.caller() {
        Caller::Agent { actor } => Some(actor.clone()),
        Caller::Operator => crate::presence::folder_ship(store),
    }
}

/// An active fleet ship the caller may manage: any ship for the captain, only
/// ships it created for an agent. `verb` completes "can VERB only ships it created".
pub(crate) fn managed(store: &Store, name: &str, verb: &str) -> Result<Member> {
    let member = members(&registry()?)?
        .into_iter()
        .find(|m| m.name == name)
        .with_context(|| format!("no active ship named {name} in the fleet"))?;
    if let Caller::Agent { actor } = store.caller() {
        ensure!(
            &member.created_by == actor,
            "{actor} can {verb} only ships it created; {name} was created by {}",
            member.created_by
        );
    }
    Ok(member)
}

/// The actions of an agent's own active charter. Callers deciding on another
/// ship's authority hold this home's write lock around the read.
pub(crate) fn own_actions(store: &Store, actor: &str) -> Result<Vec<String>> {
    let charter = store.charter(actor)?;
    ensure!(charter["active"] == true, "{actor}'s charter was revoked");
    Ok(charter["actions"]
        .as_array()
        .with_context(|| format!("{actor}'s charter has no actions"))?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect())
}

/// Every requested action must be one the granting agent holds.
fn within(own: &[String], actor: &str, requested: &Value) -> Result<()> {
    for action in requested.as_array().context("actions must be a list")? {
        let action = action.as_str().context("actions are names")?;
        ensure!(
            own.iter().any(|a| a == action),
            "{actor} cannot give a ship the {action} action its own charter lacks"
        );
    }
    Ok(())
}

/// Create a ship from inside a ship. An agent's new ship gets at most its
/// own active charter's actions, including authority to spawn. Creation and
/// revocation serialize on the creating home's write lock.
fn create(store: &mut Store, args: &Value) -> Result<Value> {
    let name = required(args, "name")?;
    identifier(name)?;
    crate::mail::unreserved(name)?;
    let creator = caller_ship(store);
    let mut actions = args.get("actions").filter(|v| !v.is_null()).cloned();
    let parent = creator.as_ref().and_then(|c| {
        let profile =
            crate::mail::dispatch(store, "ship.show", json!({"name": c})).ok()?["first_mate"]
                .take();
        let harness = profile["manifest"].as_str().and_then(|m| {
            Path::new(m)
                .parent()?
                .file_name()?
                .to_str()
                .map(str::to_owned)
        })?;
        Some((harness, profile["options"].clone()))
    });
    let harness = args
        .get("harness")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| parent.as_ref().map(|(h, _)| h.clone()));
    // A child crewed like its creator keeps the creator's model and provider
    // route: the captain chose it, and the creator cannot change it.
    let options = parent
        .filter(|(h, _)| Some(h) == harness.as_ref())
        .map(|(_, options)| options)
        .filter(Value::is_object);
    // Keep authority stable through local setup, before the child is published
    // or a dispatcher can run it. A previously committed revoke wins this lock
    // and leaves no child folder, registry entry or service behind.
    let authority = match store.caller() {
        Caller::Agent { .. } => Some(store.write_transaction()?),
        Caller::Operator => None,
    };
    if let Caller::Agent { actor } = store.caller() {
        let own = own_actions(store, actor)?;
        ensure!(
            own.iter().any(|a| a == "spawn"),
            "ship.create requires spawn in the creating ship's charter"
        );
        match &actions {
            Some(requested) => within(&own, actor, requested)?,
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
    fs::create_dir_all(&folder)?;
    let home = folder.join(crate::cli::STATE_DIR);
    let created_by = creator.clone().unwrap_or_else(|| "captain".into());
    let (ship, harness, _) = crate::cli::setup_unlinked(
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
            options,
        },
    )?;
    store.audit(
        "ship.created",
        &json!({"ship": ship, "folder": folder, "by": created_by}),
    )?;
    if let Some(authority) = authority {
        authority.commit()?;
    }
    // Linking writes both homes through their own connections, after the
    // creation's authority decision has committed.
    register(&ship, &home, &created_by)?;
    let child = Store::open(&home)?;
    link(&child, &ship)?;
    let dispatcher = crate::service::ensure(&home, &ship)?;
    let charter = child.charter(&ship)?;
    Ok(json!({"ship": ship, "folder": folder, "harness": harness,
        "actions": charter["actions"], "created_by": created_by, "dispatcher": dispatcher}))
}

/// Retire a ship: revoke its charter, stop its dispatcher, unlink it from
/// the fleet. Its folder is kept for the captain to archive or delete.
fn retire(store: &mut Store, args: &Value) -> Result<Value> {
    let name = required(args, "name")?;
    let conn = registry()?;
    let member = managed(store, name, "retire")?;
    if let Some(expected) = args.get("expected_home").and_then(Value::as_str) {
        ensure!(
            member.home == Path::new(expected),
            "ship home changed since selection; refresh before retiring"
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
    let changed = conn.execute(
        "UPDATE ships SET retired_ms=?2 WHERE name=?1 AND home=?3 AND retired_ms IS NULL",
        params![name, Store::now_ms(), member.home.to_string_lossy()],
    )?;
    ensure!(
        changed == 1,
        "fleet entry changed during retirement; refresh before retrying"
    );
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

/// Set the actions of a ship's charter. An agent may change only ships it
/// created, and only to actions its own charter has: granting never widens
/// authority beyond the granter's. The captain may change any ship.
fn set_actions(store: &mut Store, args: &Value) -> Result<Value> {
    let name = required(args, "name")?;
    let requested = args.get("actions").context("actions is required")?;
    let member = managed(store, name, "change the actions of")?;
    // Hold this home's write lock through the grant: a revoke committed
    // first wins, and one committed later cannot interleave with the check.
    let authority = match store.caller() {
        Caller::Agent { .. } => Some(store.write_transaction()?),
        Caller::Operator => None,
    };
    if let Caller::Agent { actor } = store.caller() {
        within(&own_actions(store, actor)?, actor, requested)?;
    }
    ensure!(member.home.is_dir(), "{name}'s folder is gone");
    let mut target = Store::open(&member.home)?;
    ensure!(
        target.charter(name)?["active"] == true,
        "{name}'s charter was revoked"
    );
    let charter = crate::core::dispatch(
        &mut target,
        "charter.put",
        json!({"id": name, "actions": requested}),
    )?;
    let by = caller_ship(store).unwrap_or_else(|| "captain".into());
    store.audit(
        "ship.actions",
        &json!({"ship": name, "actions": charter["actions"], "by": by}),
    )?;
    if let Some(authority) = authority {
        authority.commit()?;
    }
    Ok(json!({"ship": name, "actions": charter["actions"], "by": by}))
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
        "ship.actions" => set_actions(store, &args),
        _ => bail!("unknown fleet command {command}"),
    }
}
