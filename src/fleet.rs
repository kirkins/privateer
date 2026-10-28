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
//! created within its own. Retiring revokes the ship's charter, ends its
//! cross-ship waits, stops its dispatcher and unlinks it, and keeps its
//! folder.
use crate::store::{Caller, Store, identifier, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests;

static REGISTRY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Serve another registry than the environment names, for the whole process:
/// a machine's gateway is told which fleet it answers for.
pub fn use_registry(path: PathBuf) -> Result<()> {
    ensure!(
        path.is_absolute(),
        "the fleet registry path must be absolute"
    );
    REGISTRY
        .set(path)
        .map_err(|_| anyhow::anyhow!("the fleet registry is already chosen"))
}

pub fn registry_path() -> Result<PathBuf> {
    if let Some(path) = REGISTRY.get() {
        return Ok(path.clone());
    }
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
    open_registry(&registry_path()?, std::time::Duration::from_secs(10))
}

fn open_registry(path: &Path, busy_timeout: std::time::Duration) -> Result<Connection> {
    ensure!(
        path.is_absolute(),
        "the fleet registry path must be absolute"
    );
    let dir = path.parent().context("fleet registry directory")?;
    crate::store::private_dirs(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let conn = Connection::open(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    conn.busy_timeout(busy_timeout)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ships(name TEXT PRIMARY KEY, home TEXT NOT NULL,
           created_by TEXT NOT NULL, created_ms INTEGER NOT NULL, retired_ms INTEGER);",
    )?;
    crate::project_permissions::init(&conn)?;
    crate::releases::init(&conn)?;
    crate::machines::init(&conn)?;
    crate::grants::init(&conn)?;
    crate::moves::init(&conn)?;
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
    crate::machines::refuse_remote_name(&conn, ship)?;
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
            let tx = rusqlite::Transaction::new_unchecked(
                &conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            // Retain old paths before replacing a registration. Retired and
            // pre-move history cannot disappear from dormant capture's census.
            if let Some((old, _)) = &existing {
                crate::machines::desk_intake::home::retain_home(&tx, Path::new(old))?;
            }
            crate::machines::desk_intake::home::retain_home(&tx, home)?;
            tx.execute(
                "INSERT INTO ships(name, home, created_by, created_ms) VALUES(?1, ?2, ?3, ?4)
                 ON CONFLICT(name) DO UPDATE SET home=excluded.home, created_by=excluded.created_by,
                   created_ms=excluded.created_ms, retired_ms=NULL",
                params![ship, home_text, created_by, Store::now_ms()],
            )?;
            tx.commit()?;
            Ok(())
        }
    }
}

/// Make `other` reachable from `store`: a local-transport peer to its home
/// and a directory entry behind it. Ships this home hosts are never touched.
fn reach(store: &mut Store, other: &str, other_home: &Path, listing: &Listing) -> Result<bool> {
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
        listing.entry(other, json!({"peer": peer})),
    )?;
    Ok(true)
}

/// A ship as the fleet's directories carry it: what it is for, the ships it
/// consumes, what it requires of a machine and why it should stay put.
#[derive(Clone, Debug)]
pub(crate) struct Listing {
    pub(crate) scope: String,
    pub(crate) consumes: Value,
    pub(crate) requires: Value,
    pub(crate) pinned: Value,
}

impl Listing {
    /// A `ship.put` for this ship behind `location`.
    pub(crate) fn entry(&self, name: &str, location: Value) -> Value {
        json!({"name": name, "scope": self.scope, "location": location,
            "consumes": self.consumes, "requires": self.requires, "pinned": self.pinned})
    }
}

/// What a ship's own home says it is for, what it consumes and requires,
/// and its pin.
pub(crate) fn directory_entry(store: &mut Store, ship: &str) -> Listing {
    let record = crate::mail::dispatch(store, "ship.show", json!({"name": ship})).ok();
    let field = |key: &str, empty: Value| {
        record
            .as_ref()
            .and_then(|s| s.get(key).cloned())
            .unwrap_or(empty)
    };
    Listing {
        scope: record
            .as_ref()
            .and_then(|s| s["scope"].as_str().map(str::to_owned))
            .unwrap_or_default(),
        consumes: field("consumes", json!([])),
        requires: field("requires", json!([])),
        pinned: field("pinned", Value::Null),
    }
}

/// Link a ship with every other active ship in the fleet, both ways.
pub fn link(store: &Store, ship: &str) -> Result<usize> {
    let conn = registry()?;
    let mut ours = Store::open(store.home())?;
    let ours_entry = directory_entry(&mut ours, ship);
    let mut linked = 0;
    for member in members(&conn)? {
        // Opening a home creates it; never resurrect a deleted ship folder.
        if member.name == ship || member.home == store.home() || !member.home.is_dir() {
            continue;
        }
        let Ok(mut theirs) = Store::open(&member.home) else {
            continue;
        };
        let theirs_entry = directory_entry(&mut theirs, &member.name);
        if reach(&mut ours, &member.name, &member.home, &theirs_entry)? {
            linked += 1;
        }
        reach(&mut theirs, ship, store.home(), &ours_entry)?;
    }
    linked += crate::machines::reach_remote(&mut ours, &conn)?;
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
pub(crate) fn managed(store: &Store, name: &str, action: &str, verb: &str) -> Result<Member> {
    let member = members(&registry()?)?
        .into_iter()
        .find(|m| m.name == name)
        .with_context(|| format!("no active ship named {name} in the fleet"))?;
    if let Caller::Agent { actor } = store.caller() {
        // A ship manages what it created; anything more is the captain's grant.
        if &member.created_by != actor {
            crate::grants::require(store, actor, action, name).with_context(|| {
                format!(
                    "{actor} can {verb} only ships it created or a grant covers; {name} was created by {}",
                    member.created_by
                )
            })?;
        }
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
    if let Some(machine) = args.get("machine").filter(|v| !v.is_null()) {
        let machine = machine.as_str().context("machine must be a name")?;
        return create_on_machine(store, machine, args);
    }
    // The ship whose home this runs in lends a new ship its harness, model
    // and hosting. Only an agent creating a ship becomes its creator; when
    // the operator runs `ship new` in a ship's folder, the captain is.
    let template = caller_ship(store);
    let creator = match store.caller() {
        Caller::Agent { actor } => Some(actor.clone()),
        Caller::Operator => None,
    };
    create_here(store, args, template, creator, None)
}

/// Ask a linked machine to create a ship in its own fleet. Authority is
/// decided here, exactly as for a ship created here, in a short write
/// transaction before the machine is contacted, so a revoke committed first
/// wins. No lock is held across the call: it can outlast every other
/// writer's wait, and the remote creation could not be undone by rolling
/// back anyway. A revoke committed during the call is noted in the audit
/// entry. The machine applies its own defaults: the requester's harness,
/// model and hosting may not exist there.
fn create_on_machine(store: &mut Store, machine: &str, args: &Value) -> Result<Value> {
    let name = required(args, "name")?;
    identifier(name)?;
    crate::mail::unreserved(name)?;
    let conn = registry()?;
    ensure!(
        !members(&conn)?.iter().any(|m| m.name == name),
        "the fleet already has a ship named {name} on this machine; ship names are unique across linked machines"
    );
    crate::machines::refuse_remote_name(&conn, name)?;
    let mut request = json!({"name": name});
    for key in ["scope", "objective", "harness", "host", "folder", "actions"] {
        if let Some(value) = args.get(key).filter(|v| !v.is_null()) {
            request[key] = value.clone();
        }
    }
    let created_by = match store.caller() {
        Caller::Agent { actor } => {
            let authority = store.write_transaction()?;
            let own = own_actions(store, actor)?;
            ensure!(
                own.iter().any(|a| a == "spawn"),
                "ship.create requires spawn in the creating ship's charter"
            );
            match request.get("actions") {
                Some(requested) => within(&own, actor, requested)?,
                // The machine would otherwise give the captain's defaults.
                None => request["actions"] = json!(own),
            }
            store.audit(
                "ship.create.requested",
                &json!({"ship": name, "machine": machine, "actions": request["actions"], "by": actor}),
            )?;
            authority.commit()?;
            actor.clone()
        }
        Caller::Operator => {
            store.audit(
                "ship.create.requested",
                &json!({"ship": name, "machine": machine, "by": "captain"}),
            )?;
            "captain".into()
        }
    };
    request["created_by"] = json!(created_by);
    let mut made = match crate::machines::create_linked(machine, request) {
        Ok(made) => made,
        Err(error) => {
            // A call that timed out here may still have created the ship there.
            let failed = store.audit(
                "ship.create.failed",
                &json!({"ship": name, "machine": machine, "by": created_by, "error": format!("{error:#}")}),
            );
            return Err(match failed {
                Ok(_) => error,
                Err(audit) => {
                    error.context(format!("recording the failure also failed: {audit:#}"))
                }
            });
        }
    };
    let mut created =
        json!({"ship": name, "machine": machine, "folder": made["folder"], "by": created_by});
    if let Caller::Agent { actor } = store.caller() {
        // The decision stands, since the ship exists there; say it was overtaken.
        let active = store.charter(actor).is_ok_and(|c| c["active"] == true);
        if !active {
            created["revoked_during_call"] = json!(true);
        }
    }
    // The ship exists there either way; a reply saying otherwise would invite a retry.
    if let Err(error) = store.audit("ship.created", &created) {
        made["audit"] = json!(format!("created, but not recorded here: {error:#}"));
    }
    // Learn the new ship now rather than at the next refresh.
    if let Err(error) = crate::machines::refresh_one(machine) {
        made["directory"] = json!(format!(
            "reachable from here after the next refresh: {error:#}"
        ));
    }
    made["machine"] = json!(machine);
    Ok(made)
}

/// Create a ship that a linked machine asked for, in this machine's fleet.
/// The requesting machine already decided the creator's authority; the
/// gateway accepts it from the captain's SSH access. `created_by` is the
/// captain or a ship on `from`, which ship-name uniqueness makes unambiguous.
pub(crate) fn create_for_machine(conn: &Connection, from: &str, args: &Value) -> Result<Value> {
    let created_by = required(args, "created_by")?;
    if created_by != "captain" {
        identifier(created_by)?;
        ensure!(
            !members(conn)?.iter().any(|m| m.name == created_by),
            "{created_by} is a ship on this machine, not on {from}"
        );
        // Another linked machine's ship; `from`'s own cached directory may
        // not list a ship made there since the last refresh.
        if let Some(machine) = crate::machines::ship_elsewhere(conn, created_by, from)? {
            bail!("{created_by} is a ship on the linked machine {machine}, not on {from}");
        }
    }
    // A new ship goes beside the ship that holds the captain's mail here,
    // the fleet's liaison or else its first ship: the requester's folders
    // mean nothing on this machine, and this keeps one predictable place
    // for ships made from elsewhere, as a ship made here goes beside its
    // creator. An explicit folder is used as given.
    let (_, home) = crate::machines::captain_home(conn)?
        .context("this machine has no ships yet; create its first ship there")?;
    let mut store = Store::open(&home)?;
    let mut args = args.clone();
    if let Some(object) = args.as_object_mut() {
        object.remove("machine");
        object.remove("from_machine");
        object.remove("created_by");
    }
    let creator = (created_by != "captain").then(|| created_by.to_owned());
    // The gateway is reached over SSH, where this process's PATH is bare;
    // the unit records the login shell's, as a move's dispatcher does.
    let (path, why) = crate::service::login_path_to_record();
    let mut made = create_here(&mut store, &args, None, creator, Some(path))?;
    if let Some(why) = why {
        made["login_shell"] = json!(why);
    }
    Ok(made)
}

/// Create a ship in this machine's fleet. `template` lends the new ship its
/// harness, model and hosting; `creator` is recorded, or the captain.
/// `service_path` is the PATH the dispatcher's unit records, or this
/// process's own when `None`.
fn create_here(
    store: &mut Store,
    args: &Value,
    template: Option<String>,
    creator: Option<String>,
    service_path: Option<String>,
) -> Result<Value> {
    let name = required(args, "name")?;
    identifier(name)?;
    crate::mail::unreserved(name)?;
    crate::machines::refuse_remote_name(&registry()?, name)?;
    let mut actions = args.get("actions").filter(|v| !v.is_null()).cloned();
    let parent = template.as_ref().and_then(|c| {
        let profile =
            crate::mail::dispatch(store, "ship.show", json!({"name": c})).ok()?["first_mate"]
                .take();
        let harness = profile["manifest"]
            .as_str()
            .and_then(crate::bundled::harness)?;
        Some((harness, profile))
    });
    let harness = args
        .get("harness")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| parent.as_ref().map(|(h, _)| h.clone()));
    // A child crewed like its creator keeps the creator's model and provider
    // route: the captain chose it, and the creator cannot change it.
    let inherited = parent
        .as_ref()
        .filter(|(h, _)| Some(h) == harness.as_ref())
        .map(|(_, profile)| profile);
    let options = inherited
        .map(|profile| profile["options"].clone())
        .filter(Value::is_object);
    // Hosting follows exactly the same harness eligibility as the native
    // route. An explicit null opts out; no bundles, other settings or authority
    // are copied. Validate before creating any child state.
    let host = args
        .get("host")
        .or_else(|| inherited.and_then(|profile| profile.get("host")))
        .filter(|host| !host.is_null())
        .map(crate::host::validate)
        .transpose()?;
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
    let hosted_here = host.is_some();
    if let Some(host) = host {
        let mut child = Store::open(&home)?;
        let mut profile =
            crate::mail::dispatch(&mut child, "ship.show", json!({"name": ship}))?["first_mate"]
                .take();
        profile["host"] = host;
        crate::mail::dispatch(
            &mut child,
            "ship.put",
            json!({"name": ship, "first_mate": profile}),
        )?;
    }
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
    // A hosted Claude ship's folder needs the captain's trust before its first mate starts.
    let untrusted = if harness == "claude" && hosted_here {
        crate::host::raise_untrusted_at_create(&child, &ship, &folder)?.then(|| folder.clone())
    } else {
        None
    };
    // Linked machines learn the ship now, not at their next refresh.
    let directory = crate::machines::announce(Some(&*store));
    let dispatcher = match service_path {
        Some(path) => crate::service::ensure_with_path(&home, &ship, &path)?,
        None => crate::service::ensure(&home, &ship)?,
    };
    let charter = child.charter(&ship)?;
    Ok(json!({"ship": ship, "folder": folder, "harness": harness,
        "actions": charter["actions"], "created_by": created_by, "dispatcher": dispatcher,
        "directory": directory, "untrusted_folder": untrusted}))
}

/// Retire a ship: revoke its charter, close and withdraw its cross-ship waits
/// and let its outbox drain, stop its dispatcher, unlink it from the fleet.
/// Its folder is kept for the captain to archive or delete.
fn retire(store: &mut Store, args: &Value) -> Result<Value> {
    let name = required(args, "name")?;
    let conn = registry()?;
    let member = managed(store, name, "ship.retire", "retire")?;
    if let Some(expected) = args.get("expected_home").and_then(Value::as_str) {
        ensure!(
            member.home == Path::new(expected),
            "ship home changed since selection; refresh before retiring"
        );
    }
    let mut waits = Value::Null;
    let dispatcher = if member.home.is_dir() {
        let mut target = Store::open(&member.home)?;
        if target.charter(name).is_ok() {
            crate::core::dispatch(&mut target, "charter.revoke", json!({"id": name}))?;
        }
        // Its cross-ship waits end before it stops delivering: subscribers
        // are told it retired, its own subscriptions are withdrawn, and the
        // outbox gets a bounded chance to carry them. The operator delivers
        // them from here. An agent cannot carry another ship's mail (the
        // peer would refuse it, and a job cannot drop its role), so it waits
        // while the retiring ship's own dispatcher does. What a peer cannot
        // take in time stays queued; the far side then sees a stale wait.
        let deliver = matches!(store.caller(), Caller::Operator);
        waits = match crate::waits::retire(&target, name) {
            Ok(mut retired) => {
                retired["drain"] = crate::mail::drain(&target, name, RETIRE_DRAIN, deliver)
                    .unwrap_or_else(|error| json!({"error": format!("{error:#}")}));
                retired
            }
            Err(error) => json!({"error": format!("{error:#}")}),
        };
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
    let directory = crate::machines::announce(Some(&*store));
    Ok(
        json!({"ship": name, "retired": true, "dispatcher": dispatcher,
        "folder_kept": member.home.parent(), "directory": directory, "waits": waits}),
    )
}

/// How long a retiring ship's outbox may take to drain.
const RETIRE_DRAIN: std::time::Duration = std::time::Duration::from_secs(15);

/// Set the actions of a ship's charter. An agent may change only ships it
/// created, and only to actions its own charter has: granting never widens
/// authority beyond the granter's. The captain may change any ship.
fn set_actions(store: &mut Store, args: &Value) -> Result<Value> {
    let name = required(args, "name")?;
    let requested = args.get("actions").context("actions is required")?;
    let member = managed(store, name, "ship.actions", "change the actions of")?;
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

/// Change what a ship is for, everywhere the fleet reads it: its own record,
/// every other ship's directory entry and its generated AGENTS.md. A scope
/// routes work and grants nothing, so a ship may restate its own; otherwise
/// only its creator or the captain may. A ship whose scope someone else
/// changed is told.
fn set_scope(store: &mut Store, args: &Value) -> Result<Value> {
    let name = required(args, "name")?;
    let scope = args
        .get("scope")
        .and_then(Value::as_str)
        .context("scope is required")?;
    let limit = crate::mail::MAX_SCOPE_CHARS;
    ensure!(
        scope.chars().count() <= limit,
        "scope is limited to {limit} characters"
    );
    let by = match store.caller() {
        Caller::Agent { actor } => actor.clone(),
        Caller::Operator => crate::mail::CAPTAIN.into(),
    };
    let member = if by == name {
        members(&registry()?)?
            .into_iter()
            .find(|m| m.name == name)
            .with_context(|| format!("no active ship named {name} in the fleet"))?
    } else {
        managed(store, name, "ship.scope", "change the scope of")?
    };
    ensure!(member.home.is_dir(), "{name}'s folder is gone");
    let set = "UPDATE ships SET record=json_set(record,'$.scope',?2) WHERE name=?1";
    let own = Store::open(&member.home)?;
    ensure!(
        own.connection().execute(
            &format!("{set} AND json_extract(record,'$.location')='here'"),
            params![name, scope],
        )? == 1,
        "{name} is not hosted in its fleet home"
    );
    let peer = format!("fleet-{name}");
    for other in members(&registry()?)? {
        if other.name != name && other.home.is_dir() {
            Store::open(&other.home)?.connection().execute(
                &format!("{set} AND json_extract(record,'$.location.peer')=?3"),
                params![name, scope, peer],
            )?;
        }
    }
    crate::cli::write_instructions(own.home(), name, scope, crate::door::is_liaison(own.home()))?;
    if by != name {
        crate::mail::dispatch(
            store,
            "mail.send",
            json!({"from": by, "to": name, "kind": "notice",
                "body": format!("{by} changed your scope to: {scope}")}),
        )?;
    }
    store.audit(
        "ship.scope",
        &json!({"ship": name, "scope": scope, "by": by}),
    )?;
    Ok(json!({"ship": name, "scope": scope, "by": by}))
}

/// The most ships one ship may record that it consumes.
const MAX_CONSUMES: usize = 32;

/// Record which ships a ship consumes, such as an app and the server it
/// uses, so the server can find its consumers. Like a scope it routes and
/// grants nothing: a ship may set its own, otherwise its creator or the
/// operator. It reaches every home on this machine at once, and linked
/// machines at their next directory exchange.
fn set_consumes(store: &mut Store, args: &Value) -> Result<Value> {
    let name = required(args, "name")?;
    let consumes = args
        .get("consumes")
        .and_then(Value::as_array)
        .context("consumes is a list of ship names")?;
    ensure!(
        consumes.len() <= MAX_CONSUMES,
        "a ship consumes at most {MAX_CONSUMES} ships"
    );
    let mut names: Vec<String> = vec![];
    for other in consumes {
        let other = other.as_str().context("consumes holds ship names")?;
        identifier(other)?;
        ensure!(other != name, "{name} cannot consume itself");
        if !names.iter().any(|n| n == other) {
            names.push(other.to_owned());
        }
    }
    let by = match store.caller() {
        Caller::Agent { actor } => actor.clone(),
        Caller::Operator => crate::mail::CAPTAIN.into(),
    };
    let member = if by == name {
        members(&registry()?)?
            .into_iter()
            .find(|m| m.name == name)
            .with_context(|| format!("no active ship named {name} in the fleet"))?
    } else {
        managed(store, name, "ship.consumes", "change what is consumed by")?
    };
    ensure!(member.home.is_dir(), "{name}'s folder is gone");
    let own = Store::open(&member.home)?;
    for other in &names {
        ensure!(
            crate::mail::dispatch(
                &mut Store::open(&member.home)?,
                "ship.show",
                json!({"name": other})
            )
            .is_ok(),
            "{name} does not know a ship named {other}"
        );
    }
    let list = json!(names).to_string();
    let set = "UPDATE ships SET record=json_set(record,'$.consumes',json(?2)) WHERE name=?1";
    ensure!(
        own.connection().execute(
            &format!("{set} AND json_extract(record,'$.location')='here'"),
            params![name, list],
        )? == 1,
        "{name} is not hosted in its fleet home"
    );
    let peer = format!("fleet-{name}");
    for other in members(&registry()?)? {
        if other.name != name && other.home.is_dir() {
            Store::open(&other.home)?.connection().execute(
                &format!("{set} AND json_extract(record,'$.location.peer')=?3"),
                params![name, list, peer],
            )?;
        }
    }
    store.audit(
        "ship.consumes",
        &json!({"ship": name, "consumes": names, "by": by}),
    )?;
    Ok(json!({"ship": name, "consumes": names, "by": by}))
}

/// The longest reason a pin may give.
const MAX_PIN_REASON: usize = 200;

/// A pin as a ship's record carries it: `{reason, by, set_ms}`, where `by` is
/// the ship that set it or the captain.
pub(crate) fn check_pin(pin: &Value) -> Result<()> {
    let reason = pin["reason"].as_str().context("a pin gives a reason")?;
    ensure!(
        !reason.trim().is_empty() && reason.chars().count() <= MAX_PIN_REASON,
        "a pin's reason is 1 to {MAX_PIN_REASON} characters"
    );
    let by = pin["by"].as_str().context("a pin names who set it")?;
    if by != crate::mail::CAPTAIN {
        identifier(by)?;
    }
    ensure!(pin["set_ms"].is_i64(), "a pin records when it was set");
    Ok(())
}

/// Record what a ship requires of a machine, from the fleet's vocabulary, and
/// pin it where it is with a reason. Like what it consumes, a requirement
/// grants nothing: a ship may set its own, otherwise its creator, a grantee
/// or the operator. A pin the captain set is changed or cleared only by the
/// operator; any other pin by the ship itself or by whoever set it. It
/// reaches every home on this machine at once, and linked machines at their
/// next directory exchange.
fn set_requires(store: &mut Store, args: &Value) -> Result<Value> {
    let name = required(args, "name")?;
    let requires = args
        .get("requires")
        .map(crate::capabilities::names)
        .transpose()?;
    // Absent keeps the pin, null clears it, a reason sets it.
    let pin_change: Option<Option<String>> = match args.get("pinned") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::String(reason)) => Some(Some(reason.clone())),
        Some(_) => bail!("pinned is a reason, or null to clear the pin"),
    };
    ensure!(
        requires.is_some() || pin_change.is_some(),
        "give requires, pinned or both"
    );
    let conn = registry()?;
    for need in requires.iter().flatten() {
        ensure!(
            crate::capabilities::known(&conn, need)?,
            "the fleet's vocabulary has no capability {need}; see capability.list"
        );
    }
    let operator = matches!(store.caller(), Caller::Operator);
    let by = match store.caller() {
        Caller::Agent { actor } => actor.clone(),
        Caller::Operator => crate::mail::CAPTAIN.into(),
    };
    let member = if by == name {
        members(&conn)?
            .into_iter()
            .find(|m| m.name == name)
            .with_context(|| format!("no active ship named {name} in the fleet"))?
    } else {
        managed(store, name, "ship.requires", "change the requirements of")?
    };
    ensure!(member.home.is_dir(), "{name}'s folder is gone");
    let own = Store::open(&member.home)?;
    let tx = own.write_transaction()?;
    let record: Value = tx
        .query_row(
            "SELECT record FROM ships WHERE name=?1 AND json_extract(record,'$.location')='here'",
            [name],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(|raw| serde_json::from_str(&raw))
        .transpose()?
        .with_context(|| format!("{name} is not hosted in its fleet home"))?;
    let current = record.get("pinned").cloned().filter(|p| !p.is_null());
    let pinned = match pin_change {
        None => current,
        Some(next) => {
            if let Some(pin) = &current {
                let setter = pin["by"].as_str().unwrap_or(crate::mail::CAPTAIN);
                if !operator {
                    ensure!(
                        setter != crate::mail::CAPTAIN,
                        "the captain pinned this ship; only the captain unpins it"
                    );
                    ensure!(
                        setter == by || by == name,
                        "{setter} pinned {name}; only {setter}, {name} or the captain unpins it"
                    );
                }
            }
            match next {
                Some(reason) => {
                    let pin = json!({"reason": reason, "by": by, "set_ms": Store::now_ms()});
                    check_pin(&pin)?;
                    Some(pin)
                }
                None => None,
            }
        }
    };
    let requires = match requires {
        Some(names) => json!(names),
        None => record.get("requires").cloned().unwrap_or_else(|| json!([])),
    };
    let mut updated = record;
    updated["requires"] = requires.clone();
    match &pinned {
        Some(pin) => updated["pinned"] = pin.clone(),
        None => {
            if let Some(fields) = updated.as_object_mut() {
                fields.remove("pinned");
            }
        }
    }
    tx.execute(
        "UPDATE ships SET record=?2 WHERE name=?1",
        params![name, updated.to_string()],
    )?;
    tx.commit()?;
    let peer = format!("fleet-{name}");
    let (list, pin) = (requires.to_string(), pinned.as_ref().map(Value::to_string));
    for other in members(&conn)? {
        if other.name != name && other.home.is_dir() {
            Store::open(&other.home)?.connection().execute(
                "UPDATE ships SET record=CASE WHEN ?4 IS NULL
                   THEN json_remove(json_set(record,'$.requires',json(?2)),'$.pinned')
                   ELSE json_set(record,'$.requires',json(?2),'$.pinned',json(?4)) END
                 WHERE name=?1 AND json_extract(record,'$.location.peer')=?3",
                params![name, list, peer, pin],
            )?;
        }
    }
    let result = json!({"ship": name, "requires": requires, "pinned": pinned, "by": by});
    store.audit("ship.requires", &result)?;
    Ok(result)
}

/// The ships this home knows to consume `name`, on this machine or a linked
/// one: who to tell when a contract they rely on changes.
fn consumers(store: &Store, args: &Value) -> Result<Value> {
    let name = required(args, "name")?;
    identifier(name)?;
    let mut statement = store.connection().prepare(
        "SELECT s.name, json_extract(s.record,'$.scope') FROM ships s
         WHERE EXISTS (SELECT 1 FROM json_each(COALESCE(json_extract(s.record,'$.consumes'),'[]'))
                       WHERE value=?1)
         ORDER BY s.name",
    )?;
    let rows = statement.query_map([name], |r| {
        Ok(json!({"name": r.get::<_, String>(0)?, "scope": r.get::<_, Option<String>>(1)?}))
    })?;
    let consumers = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"ship": name, "consumers": consumers}))
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

/// The argument keys of each command here; see `crate::Accepted`.
pub(crate) const ACCEPTED: crate::Accepted = &[
    ("fleet.link", &[]),
    ("fleet.list", &[]),
    (
        "ship.create",
        &[
            "name",
            "machine",
            "scope",
            "objective",
            "harness",
            "host",
            "folder",
            "actions",
        ],
    ),
    ("ship.retire", &["name", "expected_home"]),
    ("ship.actions", &["name", "actions"]),
    ("ship.scope", &["name", "scope"]),
    ("ship.consumes", &["name", "consumes"]),
    ("ship.requires", &["name", "requires", "pinned"]),
    ("ship.consumers", &["name"]),
];

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
        "ship.scope" => set_scope(store, &args),
        "ship.consumes" => set_consumes(store, &args),
        "ship.requires" => set_requires(store, &args),
        "ship.consumers" => consumers(store, &args),
        _ => bail!("unknown fleet command {command}"),
    }
}
