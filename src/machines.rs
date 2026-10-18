//! Machines linked into one fleet.
//!
//! `privateer machine add NAME SSH_TARGET` links this machine's fleet with
//! another machine's, once, from the side that can connect. Each side records
//! the other in its registry and gives every one of its homes a single peer,
//! `machine-NAME`, behind which all of the other machine's ships live, so a
//! ship created on either side is reachable without further setup. Ship names
//! stay bare mail addresses and are unique across linked machines.
//!
//! A link is one-way: the machine that added it reaches the other machine's
//! gateway (`privateer gateway`, one JSON request per line over SSH), and the
//! other machine never connects back. Mail sent this way is delivered through
//! the gateway into the recipient's home. Mail sent the other way waits as
//! pending behind a `collected` peer, and the linking machine's dispatchers
//! collect it each pass, deliver it locally by its id and acknowledge it, so
//! a crash between the two collects it again and redelivery stays idempotent.
//! Directories are exchanged when the link is made and refreshed at a bounded
//! interval. The same exchange states each machine's stows, in both directions
//! and on both commands, so a request bound to a peer's stow learns of its
//! resume without a connection or a remote-control channel of its own.
//!
//! A ship made, moved or retired here is exchanged at once with every machine
//! this one reaches. Mail collected from a machine that names a ship this one
//! does not know yet is held, the directory is exchanged, and delivery is
//! retried; only after `UNKNOWN_EXCHANGES` passes is it refused.
//!
//! The captain's mail gathers on one machine, the captain's desk, where the
//! front door runs. By default that is the linking machine: mail the other
//! machine's ships write to the captain after the link is made is collected
//! with the rest and delivered into the captain's home here, the fleet's
//! liaison (or its first ship); it stays in the writing home's captain
//! mailbox too. The captain's own mail to a ship on the other machine is
//! written in that same home and goes out through the gateway, so requests
//! and replies close on both machines.
//!
//! `privateer machine captain NAME` moves the desk to a machine this one
//! reaches, such as an always-on server while this one sleeps: each pass then
//! pushes this machine's captain mail into the captain's home there instead,
//! and leaves the other machine's captain mail where it is. The captain's
//! answers come back as ordinary collected mail.
use crate::store::{Caller, Store, identifier, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, Read, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::Duration;

/// How often a linking machine re-exchanges ship directories.
const REFRESH_MS: i64 = 300_000;
/// Dispatchers pass every few seconds; a machine is contacted at most this
/// often, and after failures (a sleeping Mac, a tunnel down) backs off to
/// at most `MAX_BACKOFF_MS`.
const CONTACT_MS: i64 = 30_000;
const MAX_BACKOFF_MS: i64 = 600_000;
/// Mail collected in one call: a bounded count and, roughly, size.
const COLLECT_LIMIT: usize = 50;
const COLLECT_BYTES: usize = 512 * 1024;
/// SSH through a tunnel can be slow to start; links get more than a peer's
/// ten-second default.
const LINK_TIMEOUT_MS: u64 = 30_000;
/// A directory exchange made as a ship is created, moved or retired waits
/// this long, so a sleeping machine does not hold up the change.
const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(10);
/// Collected mail that names a ship this machine does not yet know is held
/// across this many directory exchanges, then refused.
const UNKNOWN_EXCHANGES: i64 = 3;
/// The error mail gives for a sender this machine does not know.
const UNKNOWN_SHIP: &str = "unknown ship ";

pub(crate) fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS machines(name TEXT PRIMARY KEY,
           role TEXT NOT NULL CHECK(role IN ('collects','collected')),
           peer TEXT NOT NULL, known_as TEXT, added_ms INTEGER NOT NULL, refreshed_ms INTEGER,
           next_contact_ms INTEGER, failures INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE IF NOT EXISTS machine_ships(machine TEXT NOT NULL, name TEXT NOT NULL,
           scope TEXT NOT NULL, PRIMARY KEY(machine, name));
         CREATE TABLE IF NOT EXISTS collect_waits(machine TEXT NOT NULL, id TEXT NOT NULL,
           passes INTEGER NOT NULL, PRIMARY KEY(machine, id));
         CREATE TABLE IF NOT EXISTS machine_closures_unsupported(machine TEXT PRIMARY KEY,
           refreshed_ms INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS machine_owner(only INTEGER PRIMARY KEY CHECK(only=1),
           ship TEXT NOT NULL, set_ms INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS captain_desk(only INTEGER PRIMARY KEY CHECK(only=1),
           machine TEXT NOT NULL, set_ms INTEGER NOT NULL);",
    )?;
    // Columns added after the table was first created. Check and add them
    // under the write lock so concurrent opens cannot both add one.
    let migrate =
        rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    for (table, column, declaration) in [
        ("machines", "next_contact_ms", "INTEGER"),
        ("machines", "failures", "INTEGER NOT NULL DEFAULT 0"),
        ("machines", "stows", "TEXT NOT NULL DEFAULT '{}'"),
        ("machine_ships", "consumes", "TEXT NOT NULL DEFAULT '[]'"),
    ] {
        let present: bool = migrate.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name=?1)"),
            [column],
            |r| r.get(0),
        )?;
        if !present {
            migrate.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} {declaration}"
            ))?;
        }
    }
    migrate.commit()?;
    Ok(())
}

/// Whether this registry has ever been linked with another machine, read
/// without opening it for writing: a dispatcher pass in a fleet with no
/// links never touches the registry.
fn ever_linked(path: &std::path::Path) -> bool {
    Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .and_then(|conn| {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='machines')",
                [],
                |r| r.get(0),
            )
        })
        .unwrap_or(false)
}

/// One linked machine as this registry records it. `collects`: this machine
/// reaches it and collects its mail. `collected`: it reaches this machine.
struct Machine {
    name: String,
    role: String,
    peer: Value,
    /// The name the other machine knows this one by (`collects` only).
    known_as: Option<String>,
    refreshed_ms: Option<i64>,
    next_contact_ms: Option<i64>,
    failures: u32,
}

fn peer_name(machine: &str) -> String {
    format!("machine-{machine}")
}

fn machines(conn: &Connection) -> Result<Vec<Machine>> {
    let mut statement = conn
        .prepare("SELECT name, role, peer, known_as, refreshed_ms, next_contact_ms, failures FROM machines ORDER BY name")?;
    let rows = statement.query_map([], |r| {
        Ok((
            (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            (r.get::<_, Option<i64>>(4)?, r.get::<_, Option<i64>>(5)?),
            r.get::<_, u32>(6)?,
        ))
    })?;
    rows.map(|row| {
        let ((name, role), peer, known_as, (refreshed_ms, next_contact_ms), failures) = row?;
        Ok(Machine {
            name,
            role,
            peer: serde_json::from_str(&peer).context("corrupt machine peer")?,
            known_as,
            refreshed_ms,
            next_contact_ms,
            failures,
        })
    })
    .collect()
}

fn machine(conn: &Connection, name: &str) -> Result<Machine> {
    machines(conn)?
        .into_iter()
        .find(|m| m.name == name)
        .with_context(|| format!("no linked machine named {name}"))
}

/// A ship as a directory carries it between machines: its name, its scope
/// and the ships it consumes.
type Entry = (String, String, Value);

/// The other machine's ships, as last exchanged.
fn cached(conn: &Connection, machine: &str) -> Result<Vec<Entry>> {
    let mut statement = conn.prepare(
        "SELECT name, scope, consumes FROM machine_ships WHERE machine=?1 ORDER BY name",
    )?;
    let rows = statement.query_map([machine], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get::<_, String>(2)?))
    })?;
    rows.map(|row| {
        let (name, scope, consumes) = row?;
        Ok((
            name,
            scope,
            serde_json::from_str(&consumes).unwrap_or_else(|_| json!([])),
        ))
    })
    .collect()
}

/// Refuse a ship name already taken on a linked machine.
pub(crate) fn refuse_remote_name(conn: &Connection, name: &str) -> Result<()> {
    let owner: Option<String> = conn
        .query_row(
            "SELECT machine FROM machine_ships WHERE name=?1",
            [name],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(machine) = owner {
        bail!(
            "{name} is a ship on the linked machine {machine}; ship names are unique across linked machines"
        );
    }
    Ok(())
}

/// This machine's active ships and their scopes, the directory it offers, and
/// whether every home was read. A home that cannot be opened may hold a live
/// ship, so the directory is then partial. A home whose folder is gone has
/// departed, as `register` treats it.
fn local_ships(conn: &Connection) -> Result<(Vec<Entry>, bool)> {
    let mut ships = vec![];
    let mut complete = true;
    for member in crate::fleet::members(conn)? {
        // Opening a home creates it; never resurrect a deleted ship folder.
        if !member.home.is_dir() {
            continue;
        }
        let Ok(mut store) = Store::open(&member.home) else {
            complete = false;
            continue;
        };
        let (scope, consumes) = crate::fleet::directory_entry(&mut store, &member.name);
        ships.push((member.name, scope, consumes));
    }
    Ok((ships, complete))
}

/// Whether a directory message says every home on its machine was read.
fn is_complete(message: &Value) -> bool {
    message["complete"].as_bool() == Some(true)
}

fn directory(ships: &[Entry]) -> Value {
    json!(
        ships
            .iter()
            .map(|(name, scope, consumes)| json!({"name": name, "scope": scope, "consumes": consumes}))
            .collect::<Vec<_>>()
    )
}

fn parse_directory(value: &Value) -> Result<Vec<Entry>> {
    value
        .as_array()
        .context("ships must be a list")?
        .iter()
        .map(|ship| {
            let name = required(ship, "name")?;
            identifier(name)?;
            crate::mail::unreserved(name)?;
            let scope = ship["scope"].as_str().unwrap_or("");
            // An earlier peer sends no consumes.
            let consumes = ship.get("consumes").cloned().unwrap_or_else(|| json!([]));
            for other in consumes.as_array().context("consumes is a list")? {
                identifier(other.as_str().context("consumes holds ship names")?)?;
            }
            Ok((name.to_owned(), scope.to_owned(), consumes))
        })
        .collect()
}

/// Names both machines would hold.
fn clashes(conn: &Connection, theirs: &[Entry]) -> Result<Vec<String>> {
    let ours: Vec<String> = crate::fleet::members(conn)?
        .into_iter()
        .map(|m| m.name)
        .collect();
    Ok(theirs
        .iter()
        .filter(|(name, _, _)| ours.contains(name))
        .map(|(name, _, _)| name.clone())
        .collect())
}

fn record(conn: &Connection, machine: &Machine, ships: &[Entry]) -> Result<()> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO machines(name, role, peer, known_as, added_ms, refreshed_ms)
         VALUES(?1, ?2, ?3, ?4, ?5, ?5) ON CONFLICT(name) DO UPDATE SET role=excluded.role,
           peer=excluded.peer, known_as=excluded.known_as, refreshed_ms=excluded.refreshed_ms",
        params![
            machine.name,
            machine.role,
            machine.peer.to_string(),
            machine.known_as,
            Store::now_ms()
        ],
    )?;
    tx.execute(
        "DELETE FROM machine_ships WHERE machine=?1",
        [&machine.name],
    )?;
    for (name, scope, consumes) in ships {
        tx.execute(
            "INSERT INTO machine_ships(machine, name, scope, consumes) VALUES(?1, ?2, ?3, ?4)",
            params![machine.name, name, scope, consumes.to_string()],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Give one home the machine's peer and a directory entry for each of its
/// ships. Ships the home hosts are never touched. When `prune` is set, entries
/// behind this machine's peer that its complete directory no longer lists have
/// left it (retired or moved), so they are dropped and audited; entries behind
/// other peers stay.
fn install(store: &mut Store, machine: &Machine, ships: &[Entry], prune: bool) -> Result<usize> {
    crate::peers::dispatch(store, "peer.put", machine.peer.clone())?;
    let peer = peer_name(&machine.name);
    let mut installed = 0;
    for (name, scope, consumes) in ships {
        let current = crate::mail::dispatch(store, "ship.show", json!({"name": name})).ok();
        if current.as_ref().is_some_and(|s| s["location"] == "here") {
            continue;
        }
        crate::mail::dispatch(
            store,
            "ship.put",
            json!({"name": name, "scope": scope, "location": {"peer": peer}, "consumes": consumes}),
        )?;
        installed += 1;
    }
    if prune {
        let listed: Vec<&str> = ships.iter().map(|(name, _, _)| name.as_str()).collect();
        let gone: Vec<String> = store
            .connection()
            .prepare(
                "DELETE FROM ships WHERE json_extract(record,'$.location.peer')=?1
                   AND name NOT IN (SELECT value FROM json_each(?2)) RETURNING name",
            )?
            .query_map(params![peer, json!(listed).to_string()], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for name in gone {
            store.audit(
                "ship.dropped",
                &json!({"ship": name, "peer": peer, "machine": machine.name}),
            )?;
        }
    }
    Ok(installed)
}

fn install_everywhere(
    conn: &Connection,
    machine: &Machine,
    ships: &[Entry],
    prune: bool,
) -> Result<usize> {
    let mut homes = 0;
    for member in crate::fleet::members(conn)? {
        if !member.home.is_dir() {
            continue;
        }
        // One home that cannot be opened must not stop every other home's
        // refresh; it catches up on a later exchange.
        let Ok(mut store) = Store::open(&member.home) else {
            continue;
        };
        install(&mut store, machine, ships, prune)?;
        homes += 1;
    }
    Ok(homes)
}

/// The home on this machine that holds the captain's mail from ships on
/// other machines: the fleet's one liaison, else its first ship.
pub(crate) fn captain_home(conn: &Connection) -> Result<Option<(String, PathBuf)>> {
    let members: Vec<_> = crate::fleet::members(conn)?
        .into_iter()
        .filter(|m| m.home.is_dir())
        .collect();
    let liaisons: Vec<_> = members
        .iter()
        .filter(|m| crate::door::is_liaison(&m.home))
        .collect();
    let chosen = match liaisons.as_slice() {
        [one] => Some(*one),
        _ => members.first(),
    };
    Ok(chosen.map(|m| (m.name.clone(), m.home.clone())))
}

/// The ship that looks after this machine, if the captain named one.
pub(crate) fn owner(conn: &Connection) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT ship FROM machine_owner", [], |r| r.get(0))
        .optional()?)
}

/// Whether `ship` is this machine's owner.
pub fn is_owner(ship: &str) -> bool {
    crate::fleet::registry()
        .and_then(|conn| owner(&conn))
        .is_ok_and(|owner| owner.as_deref() == Some(ship))
}

/// Name the ship that looks after this machine, or clear it. The owner
/// receives the machine's host-level problems and may stow the machine;
/// it gains no authority over other ships' work.
fn set_owner(ship: Option<&str>) -> Result<Value> {
    let conn = crate::fleet::registry()?;
    match ship {
        Some(ship) => {
            ensure!(
                crate::fleet::members(&conn)?
                    .iter()
                    .any(|m| m.name == ship && m.home.is_dir()),
                "{ship} is not a ship on this machine"
            );
            conn.execute(
                "INSERT INTO machine_owner(only, ship, set_ms) VALUES(1, ?1, ?2)
                 ON CONFLICT(only) DO UPDATE SET ship=excluded.ship, set_ms=excluded.set_ms",
                params![ship, Store::now_ms()],
            )?;
        }
        None => {
            conn.execute("DELETE FROM machine_owner", [])?;
        }
    }
    Ok(json!({"owner": owner(&conn)?}))
}

/// The linked machine holding the captain's desk, and since when, if not
/// this one.
fn desk(conn: &Connection) -> Result<Option<(String, i64)>> {
    Ok(conn
        .query_row("SELECT machine, set_ms FROM captain_desk", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?)
}

/// Whether `name` is the linked machine that holds this machine's captain
/// desk: the one named by `machine captain`, else a machine that links to
/// this one and collects its captain mail.
pub(crate) fn holds_desk(name: &str) -> bool {
    let Ok(conn) = crate::fleet::registry() else {
        return false;
    };
    match desk(&conn) {
        Ok(Some((desk, _))) => desk == name,
        Ok(None) => machine(&conn, name).is_ok_and(|m| m.role == "collected"),
        Err(_) => false,
    }
}

/// The name a machine this one reaches knows this one by, for its peer.
pub(crate) fn known_as_by_peer(peer: &str) -> Option<String> {
    let conn = crate::fleet::registry().ok()?;
    machines(&conn)
        .ok()?
        .into_iter()
        .find(|m| m.role == "collects" && peer_name(&m.name) == peer)
        .and_then(|m| m.known_as)
}

/// Keep the captain's mail on a machine this one reaches, or here again.
/// Captain mail written before the change stays where it is.
fn set_desk(machine: Option<&str>) -> Result<Value> {
    let conn = crate::fleet::registry()?;
    match machine {
        Some(name) => {
            identifier(name)?;
            ensure!(
                self::machine(&conn, name)?.role == "collects",
                "{name} reaches this machine; the captain's desk can move only to a machine this one reaches"
            );
            conn.execute(
                "INSERT INTO captain_desk(only, machine, set_ms) VALUES(1, ?1, ?2)
                 ON CONFLICT(only) DO UPDATE SET machine=excluded.machine, set_ms=excluded.set_ms",
                params![name, Store::now_ms()],
            )?;
        }
        None => {
            conn.execute("DELETE FROM captain_desk", [])?;
        }
    }
    Ok(json!({"captain": desk(&conn)?.map(|(m, _)| m).unwrap_or_else(|| "here".into())}))
}

/// Host-level problems are checked at most this often.
const ALERT_EVERY_MS: i64 = 60_000;
/// A capacity queue whose head has waited this long, and whose holder looks
/// unhealthy, or a route backed off for at least this long, is worth the
/// owner's attention. A long wait behind a healthy heavy job is ordinary
/// serialization: the waiting ship sees it in its own bearings.
const LONG_WAIT_MS: i64 = 600_000;
const LONG_BACKOFF_MS: i64 = 1_800_000;

/// The machine's problems right now, each with a stable key and a sentence.
/// `raised` holds the keys already raised, which the falling-space warning
/// reads its hysteresis from.
fn host_problems(store: &Store, raised: &[String]) -> Result<Vec<(String, String)>> {
    let mut problems = vec![];
    let ledger = crate::jobs::host_ledger(store)?;
    let settings = crate::admission::settings(&ledger)?;
    let disk_key = crate::admission::disk_key(store.home())?;
    let free = crate::admission::disk(store.home())?.0;
    let now = Store::now_ms();
    crate::admission::note_disk(&ledger, &disk_key, free, now)?;
    let trend_raised = raised.iter().any(|key| key == "disk_trend");
    if free < settings.min_free_disk_bytes {
        problems.push((
            "disk_floor".into(),
            format!(
                "Free disk is below the machine's floor: {} MiB free, {} MiB must stay free, so new jobs wait. Propose a cleanup to the captain.",
                free / (1024 * 1024),
                settings.min_free_disk_bytes / (1024 * 1024)
            ),
        ));
        // The falling-space warning outlives the floor's arrival: it stays
        // raised until the rate clears, so the floor alarm never comes with a
        // "Cleared" for the warning in the same pass.
        if trend_raised {
            problems.push((
                "disk_trend".into(),
                "Free disk is still falling, under the floor alarm.".into(),
            ));
        }
    } else if let Some(forecast) = crate::admission::disk_trend(&ledger, &disk_key)?
        .as_ref()
        .and_then(|trend| {
            crate::admission::disk_warning(
                settings.min_free_disk_bytes,
                settings.disk_warn_horizon_secs,
                trend_raised,
                trend,
            )
        })
    {
        // Still above the floor, but falling toward it fast enough that the
        // owner can act before builds start failing rather than after.
        problems.push((
            "disk_trend".into(),
            format!(
                "Free disk is falling: {} MiB now, {} MiB must stay free, and free space has fallen {:.1} GiB per hour over the last {:.1} hours, so the floor arrives in about {} hours. Propose a cleanup to the captain.",
                free / (1024 * 1024),
                settings.min_free_disk_bytes / (1024 * 1024),
                forecast.gib_per_hour,
                forecast.span_ms as f64 / 3_600_000.0,
                forecast.hours_to_floor.ceil().max(1.0) as i64
            ),
        ));
    }
    // A capacity queue is only a problem when the job holding capacity looks
    // unhealthy, or when nothing holds it at all. A queue behind a healthy
    // heavy job is what the cap is for, and the waiting ship's bearings
    // already say what it waits behind.
    if let Some(waited) = crate::jobs::oldest_wait_ms(store)?.filter(|w| *w >= LONG_WAIT_MS) {
        let holders = crate::jobs::capacity_holders(store)?;
        let sick: Vec<&str> = holders
            .iter()
            .filter_map(|h| h.unhealthy.as_deref())
            .collect();
        if !sick.is_empty() {
            problems.push((
                "capacity_wait".into(),
                format!(
                    "A job has waited {} minutes in this machine's capacity queue behind a job that looks unhealthy: {}. Check resource.status for what holds capacity, and runtime.needs for stuck reservations.",
                    waited / 60_000,
                    sick.join("; ")
                ),
            ));
        } else if holders.is_empty() || !holders.iter().any(|h| h.live()) {
            problems.push((
                "capacity_wait".into(),
                format!(
                    "A job has waited {} minutes in this machine's capacity queue and nothing live holds capacity. Check runtime.needs for stuck reservations, and resource.status for what holds capacity.",
                    waited / 60_000
                ),
            ));
        }
    }
    for route in crate::routes::list(&ledger)? {
        let until = route["until_ms"].as_i64().unwrap_or(0);
        let name = route["route"].as_str().unwrap_or("");
        if until - Store::now_ms() >= LONG_BACKOFF_MS {
            problems.push((
                format!("route:{name}"),
                format!(
                    "The model route {name} is backing off for another {} minutes after: {}.",
                    (until - Store::now_ms()) / 60_000,
                    route["last_error"].as_str().unwrap_or("repeated failures")
                ),
            ));
        }
    }
    Ok(problems)
}

/// When this process last looked for host-level problems.
static LAST_ALERT_CHECK: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// Tell this machine's owner about host-level problems as they start and
/// when they clear, as runtime notices on its duty task. Only the owner's
/// own dispatcher does this, at most once a minute.
pub(crate) fn alert_owner(store: &Store) -> Value {
    if let Ok(mut last) = LAST_ALERT_CHECK.lock() {
        if last.is_some_and(|at| at.elapsed().as_millis() < ALERT_EVERY_MS as u128) {
            return Value::Null;
        }
        *last = Some(std::time::Instant::now());
    }
    let work = || -> Result<Value> {
        let path = crate::fleet::registry_path()?;
        if !path.is_file() || !ever_linked(&path) {
            return Ok(Value::Null);
        }
        let conn = crate::fleet::registry()?;
        let Some(ship) = owner(&conn)? else {
            return Ok(Value::Null);
        };
        if !crate::fleet::members(&conn)?
            .iter()
            .any(|m| m.name == ship && m.home == store.home())
        {
            return Ok(Value::Null);
        }
        let db = store.connection();
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS machine_alerts(key TEXT PRIMARY KEY, raised_ms INTEGER NOT NULL);",
        )?;
        let profile = crate::mail::dispatch(
            &mut Store::open(store.home())?,
            "ship.show",
            json!({"name": ship}),
        )?;
        let duty = profile["first_mate"]["duty_task"].as_str().unwrap_or("");
        let raised_before: Vec<String> = db
            .prepare("SELECT key FROM machine_alerts")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let problems = host_problems(store, &raised_before)?;
        let tx = store.write_transaction()?;
        let mut statement = tx.prepare("SELECT key FROM machine_alerts")?;
        let raised: Vec<String> = statement
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        drop(statement);
        let notice = |key: &str, body: String| -> Result<()> {
            if duty.is_empty() {
                return Ok(());
            }
            crate::mail::record_crew(
                &tx,
                &json!({"id": format!("machine-{key}-{}", Store::now_ms()), "to": ship,
                    "kind": "notice", "task_id": duty, "urgency": "normal", "body": body,
                    "payload": {"machine_alert": key}}),
            )?;
            Ok(())
        };
        let mut sent = vec![];
        for (key, body) in &problems {
            if !raised.contains(key) {
                tx.execute(
                    "INSERT INTO machine_alerts(key, raised_ms) VALUES(?1, ?2)",
                    params![key, Store::now_ms()],
                )?;
                notice(key, format!("This machine needs you: {body}"))?;
                sent.push(json!({"raised": key}));
            }
        }
        for key in raised {
            if !problems.iter().any(|(k, _)| *k == key) {
                tx.execute("DELETE FROM machine_alerts WHERE key=?1", [&key])?;
                notice(
                    &key,
                    format!("Cleared on this machine: {key} is no longer a problem."),
                )?;
                sent.push(json!({"cleared": key}));
            }
        }
        tx.commit()?;
        Ok(json!(sent))
    };
    work().unwrap_or_else(
        |error| json!({"error": format!("{error:#}").chars().take(2_000).collect::<String>()}),
    )
}

/// Ships on linked machines, with the machine each is on and its scope.
pub(crate) fn remote_ships() -> Result<Vec<(String, String, String)>> {
    let path = crate::fleet::registry_path()?;
    if !path.is_file() || !ever_linked(&path) {
        return Ok(vec![]);
    }
    let conn = crate::fleet::registry()?;
    let mut statement =
        conn.prepare("SELECT name, machine, scope FROM machine_ships ORDER BY name")?;
    let rows = statement.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// A machine this one reaches, with the ships its last directory exchange
/// listed, as the dashboard shows them.
pub(crate) struct Reached {
    pub name: String,
    pub refreshed_ms: Option<i64>,
    /// (ship name, scope), in name order.
    pub ships: Vec<(String, String)>,
}

/// Machines this one reaches and their cached ship directories, read from a
/// connection the caller opened read-only. A registry never linked has no
/// `machines` table and reads as no machines. Machines that only reach this one
/// have no directory here and are left out.
pub(crate) fn reached_for_dashboard(conn: &Connection) -> Result<Vec<Reached>> {
    let linked: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='machines')",
        [],
        |r| r.get(0),
    )?;
    if !linked {
        return Ok(vec![]);
    }
    let mut machines: Vec<Reached> = conn
        .prepare("SELECT name, refreshed_ms FROM machines WHERE role='collects' ORDER BY name")?
        .query_map([], |r| {
            Ok(Reached {
                name: r.get(0)?,
                refreshed_ms: r.get(1)?,
                ships: vec![],
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let listed = conn
        .prepare(
            "SELECT machine, substr(name,1,160), substr(scope,1,512) FROM machine_ships ORDER BY machine, name",
        )?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (machine, name, scope) in listed {
        if let Some(reached) = machines.iter_mut().find(|m| m.name == machine) {
            reached.ships.push((name, scope));
        }
    }
    Ok(machines)
}

/// Make every linked machine's ships reachable from a newly linked home.
pub(crate) fn reach_remote(store: &mut Store, conn: &Connection) -> Result<usize> {
    let mut installed = 0;
    for machine in machines(conn)? {
        // The cache does not record whether its directory was complete.
        installed += install(store, &machine, &cached(conn, &machine.name)?, false)?;
    }
    Ok(installed)
}

/// Whether this fleet is linked with a machine by that name, in either
/// direction: a request may bind to a stow on a machine this one reaches or on
/// one that reaches it.
pub(crate) fn linked(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM machines WHERE name=?1)",
        [name],
        |r| r.get(0),
    )?)
}

/// One machine's stows, as this registry reads them: ids in force, and ids
/// resumed with the time each resumed.
type Stows = (Vec<String>, Vec<(String, i64)>);

/// One machine's stows as the exchange carries them.
fn carried(active: &[String], resumed: &[(String, i64)]) -> Value {
    json!({"active": active,
        "resumed": resumed.iter().map(|(id, at)| json!({"id": id, "resumed_ms": at}))
            .collect::<Vec<_>>()})
}

/// The stows a machine reported, as this registry last recorded them.
fn stows_of(conn: &Connection, machine: &str) -> Result<Value> {
    let raw: Option<String> = conn
        .query_row("SELECT stows FROM machines WHERE name=?1", [machine], |r| {
            r.get(0)
        })
        .optional()?;
    let (active, resumed) = parse_stows(
        &raw.and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or(Value::Null),
    )?;
    Ok(carried(&active, &resumed))
}

/// This machine's stows for an exchange, from this machine's own ledger. `None`
/// when no home or ledger can be read: the exchange then omits the field, which
/// a peer reads as nothing to act on, rather than as no stows in force.
fn stows_here(conn: &Connection) -> Option<Value> {
    let member = crate::fleet::members(conn)
        .ok()?
        .into_iter()
        .find(|m| m.home.is_dir())?;
    crate::stow::state(&Store::open(&member.home).ok()?)
}

/// An exchange body, with this machine's stows added when they can be read.
fn with_stows(mut body: Value, conn: &Connection) -> Value {
    if let Some(stows) = stows_here(conn) {
        body["stows"] = stows;
    }
    body
}

/// A peer's reported stows, bounded and no wider than one stow id each. A peer
/// too old to report them reports nothing, which reads as none known.
fn parse_stows(reported: &Value) -> Result<Stows> {
    let ids = |key: &str, limit: usize| -> Vec<String> {
        reported[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
            .filter(|id| !id.is_empty() && id.len() <= 200)
            .take(limit)
            .map(str::to_owned)
            .collect()
    };
    let mut resumed = vec![];
    for stow in reported["resumed"]
        .as_array()
        .into_iter()
        .flatten()
        .take(64)
    {
        let (Some(id), Some(at)) = (
            stow["id"]
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= 200),
            stow["resumed_ms"].as_i64(),
        ) else {
            continue;
        };
        resumed.push((id.to_owned(), at));
    }
    Ok((ids("active", 8), resumed))
}

/// Record what a machine reports about its stows, then end the requests here
/// bound to a stow this machine can now resolve: one the peer reports as
/// resumed, or one it reports neither in force nor recently resumed, which is
/// a typo or a stow that ended before we heard of it. A request bound to a
/// stow still in force stays open. Returns how many requests were ended.
fn absorb(conn: &Connection, machine: &str, reported: &Value) -> Result<usize> {
    // A peer too old to report stows sends no field: record nothing, end nothing.
    if !reported.is_object() {
        return Ok(0);
    }
    let (active, resumed) = parse_stows(reported)?;
    conn.execute(
        "UPDATE machines SET stows=?2 WHERE name=?1",
        params![machine, carried(&active, &resumed).to_string()],
    )?;
    let reason = |stow: &str| -> Option<String> {
        resumed
            .iter()
            .find(|(id, _)| id == stow)
            .map(|(_, at)| format!("stow {stow} on {machine} resumed at {at}"))
            .or_else(|| {
                (!active.iter().any(|id| id == stow))
                    .then(|| format!("stow {stow} not known on {machine}"))
            })
    };
    let mut ended = 0;
    for member in crate::fleet::members(conn)? {
        if !member.home.is_dir() {
            continue;
        }
        // A home that cannot be opened now is resolved on the next exchange.
        let Ok(home) = Store::open(&member.home) else {
            continue;
        };
        ended += crate::mail::end_stow_bound(&home, Some(machine), reason)?;
    }
    Ok(ended)
}

fn call(machine: &Machine, command: &str, args: Value, timeout: Option<Duration>) -> Result<Value> {
    let response = crate::peers::call_config(
        &machine.peer,
        &format!("{command}-{}", uuid::Uuid::new_v4()),
        command,
        args,
        &Caller::Operator,
        timeout,
    )?;
    match response["ok"].as_bool() {
        Some(true) => Ok(response["result"].clone()),
        _ => bail!(
            "machine {}: {}",
            machine.name,
            response["error"].as_str().unwrap_or("gateway refused")
        ),
    }
}

/// Exchange directories with a machine this one reaches, and install its
/// ships in every home here. Clashing names are refused on both sides.
fn exchange(conn: &Connection, machine: &Machine, timeout: Option<Duration>) -> Result<Vec<Entry>> {
    let known_as = machine
        .known_as
        .as_deref()
        .context("link has no local name")?;
    let (ours, complete) = local_ships(conn)?;
    let result = call(
        machine,
        "fleet.link",
        with_stows(
            json!({"machine": known_as, "ships": directory(&ours), "complete": complete}),
            conn,
        ),
        timeout,
    )?;
    let theirs = parse_directory(&result["ships"])?;
    let clashing = clashes(conn, &theirs)?;
    ensure!(
        clashing.is_empty(),
        "ships named {} exist on both machines; rename one before linking",
        clashing.join(", ")
    );
    record(conn, machine, &theirs)?;
    absorb(conn, &machine.name, &result["stows"])?;
    // An older peer sends no flag, so its directory only adds ships.
    install_everywhere(conn, machine, &theirs, is_complete(&result))?;
    Ok(theirs)
}

/// Run a stow command on a linked machine over its gateway. The machine
/// stows its own ships under its own ledger; this one only asks.
pub(crate) fn stow_linked(name: &str, command: &str) -> Result<Value> {
    identifier(name)?;
    let conn = crate::fleet::registry()?;
    let machine = machine(&conn, name)?;
    ensure!(
        machine.role == "collects",
        "{name} reaches this machine; stow it from there"
    );
    call(&machine, command, json!({}), None)
}

/// Asking a linked machine about its own stow, when it may be asleep.
const STOW_TIMEOUT: Duration = Duration::from_secs(10);

/// The name this machine is known by: what it chose when it made a link, or
/// the host's own name in a fleet that has never been linked. A stow is one
/// machine's hold, so a notice about it must say which machine.
pub(crate) fn this_machine() -> String {
    let names = || -> Vec<String> {
        let Ok(path) = crate::fleet::registry_path() else {
            return vec![];
        };
        if !path.is_file() || !ever_linked(&path) {
            return vec![];
        }
        let Ok(conn) = crate::fleet::registry() else {
            return vec![];
        };
        let Ok(rows) = machines(&conn) else {
            return vec![];
        };
        let mut names: Vec<String> = rows
            .into_iter()
            // Only a machine this one reaches carries the name this side chose.
            .filter(|m| m.role == "collects")
            .filter_map(|m| m.known_as)
            .collect();
        names.sort();
        names.dedup();
        names
    };
    let host = || match hostname() {
        name if name.is_empty() => "unknown".to_owned(),
        name => name,
    };
    // Links that disagree about this machine's name leave it to the host.
    match names().as_slice() {
        [one] => one.clone(),
        _ => host(),
    }
}

/// A machine's hold, as that machine reports it. `held` is null where nothing
/// here can know it, never false.
pub(crate) struct Stow {
    pub(crate) name: String,
    /// Whether this machine reached the other one to ask it.
    pub(crate) asked: bool,
    pub(crate) held: Option<bool>,
    pub(crate) error: Option<String>,
}

/// What each machine linked to this one says about its own stow. A machine
/// this one reaches is asked over its link, and a machine that reaches this
/// one is named and left unknown: a link is one-way, so nothing here can ask
/// it, and an unknown hold is never read as a lifted one.
pub(crate) fn elsewhere(command: &str) -> Result<Vec<Stow>> {
    let Ok(path) = crate::fleet::registry_path() else {
        return Ok(vec![]);
    };
    if !path.is_file() || !ever_linked(&path) {
        return Ok(vec![]);
    }
    let conn = crate::fleet::registry()?;
    let mut holds = vec![];
    for machine in machines(&conn)? {
        if machine.role != "collects" {
            holds.push(Stow {
                name: machine.name,
                asked: false,
                held: None,
                error: None,
            });
            continue;
        }
        // A sleeping machine must not hold the captain behind the link's full
        // timeout: an unknown answer is reported, not waited out.
        holds.push(
            match call(&machine, command, json!({}), Some(STOW_TIMEOUT)) {
                Ok(result) => Stow {
                    name: machine.name,
                    asked: true,
                    // A machine that answers without saying whether it is
                    // stowed has not said it is running: keep it unknown.
                    held: result["held"].as_bool(),
                    error: None,
                },
                Err(error) => Stow {
                    name: machine.name,
                    asked: true,
                    held: None,
                    error: Some(format!("{error:#}")),
                },
            },
        );
    }
    Ok(holds)
}

/// The linked machine other than `except` whose directory lists `name`.
pub(crate) fn ship_elsewhere(
    conn: &Connection,
    name: &str,
    except: &str,
) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT machine FROM machine_ships WHERE name=?1 AND machine<>?2 LIMIT 1",
            [name, except],
            |r| r.get(0),
        )
        .optional()?)
}

/// Ask a linked machine this one reaches to create a ship in its fleet.
/// The caller has already decided the creator's authority.
pub(crate) fn create_linked(name: &str, mut request: Value) -> Result<Value> {
    identifier(name)?;
    let conn = crate::fleet::registry()?;
    let machine = machine(&conn, name)?;
    ensure!(
        machine.role == "collects",
        "{name} reaches this machine; create ships on {name} from there"
    );
    request["from_machine"] = json!(
        machine
            .known_as
            .as_deref()
            .context("link has no local name")?
    );
    call(&machine, "ship.create", request, None)
}

/// The name a machine this one reaches knows this one by.
pub(crate) fn known_as(conn: &Connection, name: &str) -> Result<String> {
    identifier(name)?;
    let machine = machine(conn, name)?;
    ensure!(
        machine.role == "collects",
        "{name} reaches this machine; run this from {name}"
    );
    machine.known_as.context("link has no local name")
}

/// A gateway call to a machine this one reaches. A slow call may take up to
/// `timeout`, longer than the link's own limit.
pub(crate) fn call_reached(
    name: &str,
    command: &str,
    args: Value,
    timeout: Option<Duration>,
) -> Result<Value> {
    let conn = crate::fleet::registry()?;
    known_as(&conn, name)?;
    let mut machine = machine(&conn, name)?;
    if let Some(timeout) = timeout {
        let limit = machine.peer["timeout_ms"]
            .as_u64()
            .unwrap_or(LINK_TIMEOUT_MS);
        machine.peer["timeout_ms"] = json!(limit.max(timeout.as_millis() as u64));
    }
    call(&machine, command, args, timeout)
}

/// Re-exchange directories with one machine this one reaches.
pub(crate) fn refresh_one(name: &str) -> Result<()> {
    refresh(Some(name)).map(|_| ())
}

/// Exchange directories now with every machine this one reaches, after a ship
/// was created, moved or retired here, so the change is known there without
/// waiting for the next refresh. Best effort: a machine that cannot be reached
/// is skipped (and audited in `store`, when given), and never fails the change.
pub(crate) fn announce(store: Option<&Store>) -> Vec<Value> {
    let Ok(path) = crate::fleet::registry_path() else {
        return vec![];
    };
    if !path.is_file() || !ever_linked(&path) {
        return vec![];
    }
    let Ok(conn) = crate::fleet::registry() else {
        return vec![];
    };
    let Ok(rows) = machines(&conn) else {
        return vec![];
    };
    rows.into_iter()
        .filter(|m| m.role == "collects")
        .map(
            |machine| match exchange(&conn, &machine, Some(ANNOUNCE_TIMEOUT)) {
                Ok(ships) => json!({"machine": machine.name, "ships": ships.len()}),
                Err(error) => {
                    // Due at the next pass: its exchange runs before mail is collected or pushed.
                    let _ = conn.execute(
                        "UPDATE machines SET refreshed_ms=NULL WHERE name=?1",
                        [&machine.name],
                    );
                    let error = format!("{error:#}");
                    let mut skipped = json!({"machine": machine.name, "skipped": &error});
                    if let Some(store) = store {
                        let audit = json!({"machine": machine.name, "error": &error});
                        if let Err(recorded) = store.audit("machine.skipped", &audit) {
                            skipped["audit"] = json!(format!("{recorded:#}"));
                        }
                    }
                    skipped
                }
            },
        )
        .collect()
}

pub(crate) fn hostname() -> String {
    let mut buffer = [0u8; 256];
    let ok = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0;
    let raw = if ok {
        let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
        String::from_utf8_lossy(&buffer[..end]).into_owned()
    } else {
        String::new()
    };
    raw.split('.')
        .next()
        .unwrap_or("")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

fn add(args: &Value) -> Result<Value> {
    let name = required(args, "name")?;
    identifier(name)?;
    let target = required(args, "target")?;
    let executable = required(args, "executable")?;
    let known_as = match args.get("as").and_then(Value::as_str) {
        Some(known_as) => known_as.to_owned(),
        None => hostname(),
    };
    identifier(&known_as).context("name this machine with --as NAME")?;
    ensure!(known_as != name, "both machines cannot be named {name}");
    let mut peer = json!({"name": peer_name(name), "transport": "ssh", "host": target,
        "executable": executable, "gateway": true, "timeout_ms": LINK_TIMEOUT_MS});
    if let Some(fleet) = args.get("fleet").and_then(Value::as_str) {
        peer["fleet"] = json!(fleet);
    }
    if let Some(transport) = args.get("transport").and_then(Value::as_str) {
        // Tests reach a gateway on the same machine without SSH.
        peer["transport"] = json!(transport);
        peer.as_object_mut().unwrap().remove("host");
    }
    let conn = crate::fleet::registry()?;
    if let Some(existing) = machines(&conn)?.into_iter().find(|m| m.name == name) {
        ensure!(
            existing.role == "collects",
            "{name} already reaches this machine; a link is made from one side"
        );
    }
    let machine = Machine {
        name: name.to_owned(),
        role: "collects".into(),
        peer,
        known_as: Some(known_as.clone()),
        refreshed_ms: None,
        next_contact_ms: None,
        failures: 0,
    };
    let theirs = exchange(&conn, &machine, None)?;
    Ok(json!({"machine": name, "known_as": known_as,
        "ships": theirs.iter().map(|(n, _, _)| n).collect::<Vec<_>>()}))
}

fn list() -> Result<Value> {
    let conn = crate::fleet::registry()?;
    let mut out = vec![];
    for machine in machines(&conn)? {
        out.push(json!({
            "name": machine.name,
            "direction": if machine.role == "collects" { "reached from here" } else { "reaches here" },
            "target": machine.peer["host"],
            "known_as": machine.known_as,
            "refreshed_ms": machine.refreshed_ms,
            "stows": stows_of(&conn, &machine.name)?,
            "ships": cached(&conn, &machine.name)?.into_iter().map(|(n, _, _)| n).collect::<Vec<_>>(),
        }));
    }
    Ok(json!({"owner": owner(&conn)?, "machines": out}))
}

/// Forget a link here. The other machine keeps its side until removed there.
fn remove(name: &str) -> Result<Value> {
    let conn = crate::fleet::registry()?;
    machine(&conn, name)?;
    for member in crate::fleet::members(&conn)? {
        if member.home.is_dir() {
            crate::peers::forget(&Store::open(&member.home)?, &peer_name(name))?;
        }
    }
    conn.execute("DELETE FROM machine_ships WHERE machine=?1", [name])?;
    conn.execute("DELETE FROM collect_waits WHERE machine=?1", [name])?;
    conn.execute("DELETE FROM machines WHERE name=?1", [name])?;
    Ok(json!({"removed": name}))
}

/// Collect now from machines this one reaches, outside the schedule.
fn collect_now(name: Option<&str>) -> Result<Value> {
    let conn = crate::fleet::registry()?;
    let mut out = vec![];
    for machine in machines(&conn)?
        .into_iter()
        .filter(|m| m.role == "collects" && name.is_none_or(|n| n == m.name))
    {
        out.push(collect(
            &conn,
            &machine,
            Duration::from_millis(LINK_TIMEOUT_MS),
        )?);
    }
    Ok(json!({"collected": out}))
}

/// Record a contact's outcome and when the machine is next due.
fn schedule(conn: &Connection, machine: &Machine, succeeded: bool) -> Result<()> {
    let failures = if succeeded {
        0
    } else {
        machine.failures.saturating_add(1).min(10)
    };
    let delay = if succeeded {
        CONTACT_MS
    } else {
        CONTACT_MS
            .saturating_mul(1 << failures.min(5))
            .min(MAX_BACKOFF_MS)
    };
    conn.execute(
        "UPDATE machines SET next_contact_ms=?2, failures=?3 WHERE name=?1",
        params![
            machine.name,
            Store::now_ms().saturating_add(delay),
            failures
        ],
    )?;
    Ok(())
}

fn refresh(name: Option<&str>) -> Result<Value> {
    let conn = crate::fleet::registry()?;
    let mut out = vec![];
    for machine in machines(&conn)?
        .into_iter()
        .filter(|m| m.role == "collects" && name.is_none_or(|n| n == m.name))
    {
        let ships = exchange(&conn, &machine, None)?;
        out.push(json!({"machine": machine.name, "ships": ships.len()}));
    }
    Ok(json!({"refreshed": out}))
}

/// Collect mail waiting on a machine this one reaches, deliver it into the
/// homes here, and acknowledge what was delivered or refused.
fn collect(conn: &Connection, machine: &Machine, timeout: Duration) -> Result<Value> {
    let known_as = machine
        .known_as
        .as_deref()
        .context("link has no local name")?;
    // The machine holding the captain's desk keeps its own captain mail.
    let captain = desk(conn)?.is_none_or(|(desk, _)| desk != machine.name);
    let result = call(
        machine,
        "fleet.collect",
        with_stows(json!({"machine": known_as, "captain": captain}), conn),
        Some(timeout),
    )?;
    // What the peer reports about its stows travels with the mail: a request
    // bound to a stow there ends on this exchange, not a later one.
    let ended = absorb(conn, &machine.name, &result["stows"])?;
    let messages = result["messages"]
        .as_array()
        .context("collected messages")?;
    // Mail naming a ship this machine does not know yet waits for a directory
    // exchange: one is made here, in this pass, before any of it is placed.
    let mut stale = false;
    for envelope in messages {
        stale |= unknown_party(conn, envelope)?.is_some();
    }
    let refresh = if stale {
        exchange(conn, machine, Some(timeout))
            .err()
            .map(|error| format!("{error:#}"))
    } else {
        None
    };
    let homes: Vec<_> = crate::fleet::members(conn)?;
    let (mut delivered, mut refused) = (vec![], vec![]);
    for envelope in messages {
        let id = required(envelope, "id")?.to_owned();
        let to = envelope["to"].as_str().unwrap_or("");
        let mut envelope = envelope.clone();
        if let Some(reason) = unknown_party(conn, &envelope)? {
            refused.extend(hold_or_refuse(
                conn,
                machine,
                &id,
                &envelope,
                &reason,
                UNKNOWN_EXCHANGES,
            )?);
            continue;
        }
        let home = if to == crate::mail::CAPTAIN {
            // A home that cannot be read now may hold the request; it is retried
            // on the next passes, then refused.
            match captain_destination(conn, &envelope) {
                Ok((home, held)) => {
                    // Only this machine says whether it holds the request.
                    envelope["unmatched"] = json!(!held);
                    home
                }
                Err(error) => {
                    refused.extend(hold_or_refuse(
                        conn,
                        machine,
                        &id,
                        &envelope,
                        &format!("{error:#}"),
                        WAIT_PASSES,
                    )?);
                    continue;
                }
            }
        } else {
            homes
                .iter()
                .find(|m| m.name == to && m.home.is_dir())
                .map(|m| m.home.clone())
        };
        let Some(home) = home else {
            refused.push(json!({"id": id, "error": format!("no ship named {to} on this machine")}));
            continue;
        };
        // A home that cannot be opened now is retried on the next passes too.
        let mut store = match Store::open(&home) {
            Ok(store) => store,
            Err(error) => {
                if to == crate::mail::CAPTAIN {
                    refused.extend(hold_or_refuse(
                        conn,
                        machine,
                        &id,
                        &envelope,
                        &format!("{error:#}"),
                        WAIT_PASSES,
                    )?);
                }
                continue;
            }
        };
        forget_wait(conn, machine, &id)?;
        // The runtime names the machine it collected from, for the desk check.
        let mut stamped = envelope.clone();
        stamped["via"] = json!(machine.name);
        match crate::dispatch(&mut store, "mail.deliver", stamped) {
            Ok(result) => delivered
                .push(json!({"id": id, "end_state": crate::mail::applied_end(&envelope, &result)})),
            Err(error) => {
                let reason = format!("{error:#}");
                if to == crate::mail::CAPTAIN || envelope["from"] == crate::mail::CAPTAIN {
                    surface_captain_refusal(conn, &id, &envelope, &reason)?;
                }
                refused.push(json!({"id": id, "error": reason}));
            }
        }
    }
    if !delivered.is_empty() || !refused.is_empty() {
        call(
            machine,
            "fleet.collected",
            json!({"machine": known_as, "delivered": delivered, "refused": refused}),
            Some(timeout),
        )?;
    }
    Ok(
        json!({"machine": machine.name, "delivered": delivered.len(),
        "refused": refused.len(), "requests_ended": ended, "refresh": refresh}),
    )
}

/// Why a collected message cannot be placed yet: a ship it names is not in
/// this machine's directory. A ship made, moved or retired elsewhere reaches
/// the directory at the next exchange, so such mail waits for it.
fn unknown_party(conn: &Connection, envelope: &Value) -> Result<Option<String>> {
    let to = envelope["to"].as_str().unwrap_or("");
    let from = envelope["from"].as_str().unwrap_or("");
    let home = if to == crate::mail::CAPTAIN {
        captain_home(conn)?.map(|(_, home)| home)
    } else {
        match crate::fleet::members(conn)?
            .into_iter()
            .find(|m| m.name == to && m.home.is_dir())
        {
            Some(member) => Some(member.home),
            None => return Ok(Some(format!("no ship named {to} on this machine"))),
        }
    };
    // A home that cannot be opened now is placed, or held, on a later pass.
    let Some(home) = home.filter(|home| home.is_dir()) else {
        return Ok(None);
    };
    if from == crate::mail::CAPTAIN {
        return Ok(None);
    }
    let Ok(mut store) = Store::open(&home) else {
        return Ok(None);
    };
    Ok(
        match crate::mail::dispatch(&mut store, "ship.show", json!({"name": from})) {
            Ok(_) => None,
            Err(_) => Some(format!("unknown ship {from}")),
        },
    )
}

/// Where a captain's message collected here is delivered: the home that
/// holds the request it answers, so the request closes where it was written
/// (and the captain's door reads the reply from that home's captain mailbox),
/// or else the captain's home. Also says whether a home here holds that
/// request: one asked on the sending machine is closed there, not here.
fn captain_destination(conn: &Connection, envelope: &Value) -> Result<(Option<PathBuf>, bool)> {
    if let Some(request) = envelope["correlation"].as_str() {
        let captain = captain_home(conn)?.map(|(_, home)| home);
        let members = crate::fleet::members(conn)?
            .into_iter()
            .filter(|m| m.home.is_dir())
            .map(|m| m.home);
        // A home that cannot be read may hold the request, so when it is not
        // found elsewhere the message waits for the next pass.
        let mut unreadable = false;
        for home in captain.into_iter().chain(members) {
            let Ok(store) = Store::open(&home) else {
                unreadable = true;
                continue;
            };
            if crate::mail::find(&store, request)?.is_some() {
                return Ok((Some(home), true));
            }
        }
        ensure!(
            !unreadable,
            "a fleet home cannot be read; the request is not yet located"
        );
        return Ok((captain_home(conn)?.map(|(_, home)| home), false));
    }
    Ok((captain_home(conn)?.map(|(_, home)| home), true))
}

/// A captain's message that this machine refused is put before the captain,
/// not left only in the collection record: a notice in the captain's home.
fn surface_captain_refusal(
    conn: &Connection,
    id: &str,
    envelope: &Value,
    reason: &str,
) -> Result<()> {
    let Some((_, home)) = captain_home(conn)? else {
        return Ok(());
    };
    let from = envelope["from"].as_str().unwrap_or("unknown");
    let to = envelope["to"].as_str().unwrap_or("unknown");
    crate::mail::surface_refusal(&Store::open(&home)?, id, "runtime", from, to, reason)
}

/// Collect passes a captain's message may wait for the home it needs to be
/// readable before it is refused.
const WAIT_PASSES: i64 = 10;

/// A collected message this pass cannot place. It waits for the next passes
/// until `limit` have gone by, then is refused; a captain's message is put
/// before the captain. Returns the refusal, if it was made now.
fn hold_or_refuse(
    conn: &Connection,
    machine: &Machine,
    id: &str,
    envelope: &Value,
    reason: &str,
    limit: i64,
) -> Result<Option<Value>> {
    conn.execute(
        "INSERT INTO collect_waits(machine, id, passes) VALUES(?1, ?2, 1)
         ON CONFLICT(machine, id) DO UPDATE SET passes=passes+1",
        params![machine.name, id],
    )?;
    let passes: i64 = conn.query_row(
        "SELECT passes FROM collect_waits WHERE machine=?1 AND id=?2",
        params![machine.name, id],
        |r| r.get(0),
    )?;
    if passes < limit {
        return Ok(None);
    }
    forget_wait(conn, machine, id)?;
    if envelope["to"] == crate::mail::CAPTAIN || envelope["from"] == crate::mail::CAPTAIN {
        surface_captain_refusal(conn, id, envelope, reason)?;
    }
    Ok(Some(json!({"id": id, "error": reason})))
}

/// Forget a captain's message's waiting once it has been placed or refused.
fn forget_wait(conn: &Connection, machine: &Machine, id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM collect_waits WHERE machine=?1 AND id=?2",
        params![machine.name, id],
    )?;
    Ok(())
}

/// Hand this machine's captain mail written since the desk moved to the
/// machine holding it, and record what it took or refused, so nothing is
/// sent twice and a refusal is not retried.
fn push_captain(
    conn: &Connection,
    machine: &Machine,
    since_ms: i64,
    timeout: Duration,
) -> Result<Value> {
    let known_as = machine
        .known_as
        .as_deref()
        .context("link has no local name")?;
    let peer = peer_name(&machine.name);
    let mut messages = vec![];
    let mut bytes = 0;
    for member in crate::fleet::members(conn)? {
        if !member.home.is_dir() || messages.len() >= COLLECT_LIMIT || bytes >= COLLECT_BYTES {
            continue;
        }
        let store = Store::open(&member.home)?;
        let (limit, room) = (COLLECT_LIMIT - messages.len(), COLLECT_BYTES - bytes);
        for envelope in crate::mail::awaiting_captain(&store, &peer, since_ms, limit, room)? {
            bytes += envelope.to_string().len();
            messages.push(envelope);
        }
    }
    let mut pushed = 0;
    if !messages.is_empty() {
        pushed = push_messages(conn, machine, &peer, known_as, &messages, timeout)?;
    }
    let closed = push_closures(conn, machine, &peer, known_as, timeout)?;
    Ok(json!({"machine": machine.name, "captain_pushed": pushed, "captain_closed": closed}))
}

/// Send captain messages to the desk and record what it took or refused.
fn push_messages(
    conn: &Connection,
    machine: &Machine,
    peer: &str,
    known_as: &str,
    messages: &[Value],
    timeout: Duration,
) -> Result<usize> {
    let result = call(
        machine,
        "fleet.captain",
        json!({"machine": known_as, "messages": messages}),
        Some(timeout),
    )?;
    let mut taken: Vec<(String, Option<String>)> = result["delivered"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|id| Some((id.as_str()?.to_owned(), None)))
        .collect();
    for refusal in result["refused"].as_array().into_iter().flatten() {
        let (Some(id), error) = (refusal["id"].as_str(), refusal["error"].as_str()) else {
            continue;
        };
        let error = error.unwrap_or("refused");
        // A sender the desk does not know yet waits for a directory exchange,
        // as collected mail does; it is refused only once that has had its passes.
        if error.starts_with(UNKNOWN_SHIP) {
            let envelope = messages
                .iter()
                .find(|e| e["id"] == id)
                .unwrap_or(&Value::Null);
            if hold_or_refuse(conn, machine, id, envelope, error, UNKNOWN_EXCHANGES)?.is_none() {
                continue;
            }
        }
        taken.push((id.to_owned(), Some(error.to_owned())));
    }
    for member in crate::fleet::members(conn)? {
        if member.home.is_dir() {
            crate::mail::collected_captain(&Store::open(&member.home)?, peer, &taken)?;
        }
    }
    Ok(taken.len())
}

/// Tell the desk which captain requests this machine's ships asked and were
/// answered, withdrawn or ended here, so its copies close too. Each closure
/// goes once, after its request has reached the desk.
fn push_closures(
    conn: &Connection,
    machine: &Machine,
    peer: &str,
    known_as: &str,
    timeout: Duration,
) -> Result<usize> {
    // Read the refresh epoch fresh: an exchange earlier in this pass may have moved it.
    let epoch = self::machine(conn, &machine.name)?
        .refreshed_ms
        .unwrap_or(0);
    let gap: Option<i64> = conn
        .query_row(
            "SELECT refreshed_ms FROM machine_closures_unsupported WHERE machine=?1",
            [&machine.name],
            |r| r.get(0),
        )
        .optional()?;
    if gap == Some(epoch) {
        return Ok(0);
    }
    let mut closures = vec![];
    for member in crate::fleet::members(conn)? {
        if !member.home.is_dir() || closures.len() >= COLLECT_LIMIT {
            continue;
        }
        let store = Store::open(&member.home)?;
        let room = COLLECT_LIMIT - closures.len();
        closures.extend(crate::mail::closed_captain(&store, peer, room)?);
    }
    if closures.is_empty() {
        return Ok(0);
    }
    let result = call(
        machine,
        "fleet.captain",
        json!({"machine": known_as, "messages": [], "closures": &closures}),
        Some(timeout),
    )?;
    let Some(reported) = result.get("closed").and_then(Value::as_array) else {
        // A desk that predates closures answers without a closed list: none
        // goes to it until its link refreshes, when it may have been updated.
        conn.execute(
            "INSERT INTO machine_closures_unsupported(machine, refreshed_ms) VALUES(?1, ?2)
             ON CONFLICT(machine) DO UPDATE SET refreshed_ms=excluded.refreshed_ms",
            params![machine.name, epoch],
        )?;
        for member in crate::fleet::members(conn)? {
            if member.home.is_dir() {
                Store::open(&member.home)?.audit(
                    "machine.closures_unsupported",
                    &json!({"machine": machine.name, "count": closures.len()}),
                )?;
            }
        }
        return Ok(0);
    };
    let closed: Vec<String> = reported
        .iter()
        .filter_map(|id| Some(id.as_str()?.to_owned()))
        .collect();
    // A closure the desk refused is permanently unusable, so it is not sent again.
    let refused: Vec<Value> = result["closures_refused"]
        .as_array()
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    let refused_ids: Vec<String> = refused
        .iter()
        .filter_map(|r| Some(r["id"].as_str()?.to_owned()))
        .collect();
    let offered: Vec<String> = closed.iter().chain(&refused_ids).cloned().collect();
    for member in crate::fleet::members(conn)? {
        if member.home.is_dir() {
            let store = Store::open(&member.home)?;
            crate::mail::collected_closed(&store, peer, &offered)?;
            for refusal in &refused {
                store.audit(
                    "machine.closure_refused",
                    &json!({"machine": machine.name, "closure": refusal}),
                )?;
            }
        }
    }
    Ok(closed.len())
}

/// End the desk's copies of one closure in every home that holds them. A home
/// that cannot be opened or written does not stop the others, but the closure
/// then counts as not applied, so the sender offers it again.
fn apply_closure(conn: &Connection, peer: &str, closure: &Value) -> Result<()> {
    let mut failure = None;
    for member in crate::fleet::members(conn)? {
        if member.home.is_dir() {
            let applied = Store::open(&member.home)
                .and_then(|store| crate::mail::close_captain_request(&store, peer, closure));
            if let Err(error) = applied {
                failure.get_or_insert(error);
            }
        }
    }
    failure.map_or(Ok(()), Err)
}

/// One dispatcher pass: for each machine this one reaches, refresh the
/// directories when due and collect waiting mail. One dispatcher on this
/// machine does each link's work at a time, and only a dispatcher of a home
/// in this registry does it at all.
pub(crate) fn tick(home: &std::path::Path, timeout: Duration) -> Value {
    let work = || -> Result<Vec<Value>> {
        let path = crate::fleet::registry_path()?;
        if !path.is_file() || !ever_linked(&path) {
            return Ok(vec![]);
        }
        let conn = crate::fleet::registry()?;
        let reaching: Vec<Machine> = machines(&conn)?
            .into_iter()
            .filter(|m| m.role == "collects")
            .collect();
        if reaching.is_empty() || !crate::fleet::members(&conn)?.iter().any(|m| m.home == home) {
            return Ok(vec![]);
        }
        let mut out = vec![];
        for machine in reaching {
            let lock_path = path.with_file_name(format!("{}.lock", peer_name(&machine.name)));
            let lock = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock_path)?;
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                continue;
            }
            // Another dispatcher may have contacted it while this one waited.
            let machine = self::machine(&conn, &machine.name)?;
            if machine
                .next_contact_ms
                .is_some_and(|at| Store::now_ms() < at)
            {
                continue;
            }
            let due = machine
                .refreshed_ms
                .is_none_or(|at| Store::now_ms() - at >= REFRESH_MS);
            // A machine that timed out on its exchange is not collected from in
            // the same pass, so an unreachable one stalls a pass only once. Any
            // other exchange failure (an older peer, a refused directory) still
            // lets the pass collect: a failed exchange stays due every pass and
            // must not stop the mail.
            let refreshed = if due {
                exchange(&conn, &machine, Some(timeout)).map(|_| ())
            } else {
                Ok(())
            };
            let outcome = match refreshed {
                Err(error) if format!("{error:#}").contains("timed out") => Err(error),
                _ => Ok(()),
            }
            .and_then(|()| collect(&conn, &machine, timeout))
            .and_then(|collected| match desk(&conn)? {
                Some((name, since)) if name == machine.name => {
                    let pushed = push_captain(&conn, &machine, since, timeout)?;
                    Ok(
                        json!({"machine": machine.name, "delivered": collected["delivered"],
                        "refused": collected["refused"], "captain_pushed": pushed["captain_pushed"],
                        "captain_closed": pushed["captain_closed"]}),
                    )
                }
                _ => Ok(collected),
            });
            schedule(&conn, &machine, outcome.is_ok())?;
            out.push(outcome.unwrap_or_else(
                |error| json!({"machine": machine.name, "error": format!("{error:#}")}),
            ));
        }
        Ok(out)
    };
    work().map(Value::from).unwrap_or_else(
        |error| json!([{"error": format!("{error:#}").chars().take(2_000).collect::<String>()}]),
    )
}

/// The home here that hosts a ship.
fn home_of(conn: &Connection, ship: &str) -> Result<PathBuf> {
    crate::fleet::members(conn)?
        .into_iter()
        .find(|m| m.name == ship && m.home.is_dir())
        .map(|m| m.home)
        .with_context(|| format!("no ship named {ship} on this machine"))
}

/// A machine that reaches this one, by the name it gave.
fn reaching_here(conn: &Connection, name: &str) -> Result<Machine> {
    identifier(name)?;
    let machine = machine(conn, name)?;
    ensure!(
        machine.role == "collected",
        "{name} is reached from here, not linked to it"
    );
    Ok(machine)
}

/// What a gateway answers for its machine's fleet. Linking and collecting
/// are the operator's, whose SSH access to this machine is real authority
/// here; an agent may only deliver mail as itself.
fn serve_request(caller: &Caller, command: &str, args: &Value) -> Result<Value> {
    ensure!(args.is_object(), "command arguments must be a JSON object");
    // Delivery through a machine peer arrives as an ordinary mail.deliver.
    let delivery = matches!(command, "fleet.deliver" | "mail.deliver");
    if !delivery {
        caller.require_operator(command)?;
    }
    let conn = crate::fleet::registry()?;
    match command {
        "fleet.link" => {
            let name = required(args, "machine")?;
            identifier(name)?;
            if let Some(existing) = machines(&conn)?.into_iter().find(|m| m.name == name) {
                ensure!(
                    existing.role == "collected",
                    "this machine already reaches {name}; a link is made from one side"
                );
            }
            let theirs = parse_directory(&args["ships"])?;
            let clashing = clashes(&conn, &theirs)?;
            ensure!(
                clashing.is_empty(),
                "ships named {} exist on both machines; rename one before linking",
                clashing.join(", ")
            );
            let machine = Machine {
                name: name.to_owned(),
                role: "collected".into(),
                peer: json!({"name": peer_name(name), "transport": "collected"}),
                known_as: None,
                refreshed_ms: None,
                next_contact_ms: None,
                failures: 0,
            };
            record(&conn, &machine, &theirs)?;
            install_everywhere(&conn, &machine, &theirs, is_complete(args))?;
            // The machine that reached us carries its stow state in the same
            // exchange, so a request here bound to a stow there resolves now.
            let ended = absorb(&conn, name, args.get("stows").unwrap_or(&Value::Null))?;
            let (ours, complete) = local_ships(&conn)?;
            Ok(with_stows(
                json!({"ships": directory(&ours), "complete": complete, "requests_ended": ended}),
                &conn,
            ))
        }
        _ if delivery => {
            let home = home_of(&conn, required(args, "to")?)?;
            let mut store = Store::open_as(&home, caller.clone())?;
            // A delivering machine names itself; only a machine linked to this
            // one, as it gave its name, is kept for the desk check.
            let mut args = args.clone();
            let linked = args["via"]
                .as_str()
                .is_some_and(|via| reaching_here(&conn, via).is_ok());
            if !linked && let Some(fields) = args.as_object_mut() {
                fields.remove("via");
            }
            crate::dispatch(&mut store, "mail.deliver", args)
        }
        "fleet.collect" => {
            let machine = reaching_here(&conn, required(args, "machine")?)?;
            let peer = peer_name(&machine.name);
            // The collecting machine's stow state rides along with its mail.
            let ended = absorb(
                &conn,
                &machine.name,
                args.get("stows").unwrap_or(&Value::Null),
            )?;
            // Mail to the captain from before the link stays here.
            let linked_ms: i64 = conn.query_row(
                "SELECT added_ms FROM machines WHERE name=?1",
                [&machine.name],
                |r| r.get(0),
            )?;
            // The linking machine may keep the captain's desk elsewhere, and a
            // machine whose desk is elsewhere hands its captain mail only there.
            let wants_captain = args["captain"].as_bool().unwrap_or(true)
                && desk(&conn)?.is_none_or(|(desk, _)| desk == machine.name);
            let mut messages = vec![];
            let mut bytes = 0;
            for member in crate::fleet::members(&conn)? {
                if !member.home.is_dir() {
                    continue;
                }
                let store = Store::open(&member.home)?;
                for captain in [false, true] {
                    if captain && !wants_captain {
                        continue;
                    }
                    if messages.len() >= COLLECT_LIMIT || bytes >= COLLECT_BYTES {
                        break;
                    }
                    let (limit, room) = (COLLECT_LIMIT - messages.len(), COLLECT_BYTES - bytes);
                    let found = if captain {
                        crate::mail::awaiting_captain(&store, &peer, linked_ms, limit, room)?
                    } else {
                        crate::mail::awaiting(&store, &peer, limit, room)?
                    };
                    for envelope in found {
                        bytes += envelope.to_string().len();
                        messages.push(envelope);
                    }
                }
            }
            Ok(with_stows(
                json!({"messages": messages, "requests_ended": ended}),
                &conn,
            ))
        }
        "fleet.collected" => {
            let machine = reaching_here(&conn, required(args, "machine")?)?;
            let peer = peer_name(&machine.name);
            let list = |key: &str| args[key].as_array().cloned().unwrap_or_default();
            let delivered: Vec<(String, Option<String>)> = list("delivered")
                .iter()
                .filter_map(|d| {
                    let id = d["id"].as_str()?.to_owned();
                    Some((id, d["end_state"].as_str().map(str::to_owned)))
                })
                .collect();
            let refused: Vec<(String, String)> = list("refused")
                .iter()
                .filter_map(|r| {
                    Some((
                        r["id"].as_str()?.to_owned(),
                        r["error"].as_str().unwrap_or("refused").to_owned(),
                    ))
                })
                .collect();
            let captain: Vec<(String, Option<String>)> = delivered
                .iter()
                .map(|(id, _)| (id.clone(), None))
                .chain(refused.iter().map(|(id, e)| (id.clone(), Some(e.clone()))))
                .collect();
            let mut recorded = 0;
            for member in crate::fleet::members(&conn)? {
                if member.home.is_dir() {
                    let store = Store::open(&member.home)?;
                    recorded += crate::mail::collected(&store, &peer, &delivered, &refused)?;
                    recorded += crate::mail::collected_captain(&store, &peer, &captain)?;
                }
            }
            Ok(json!({"recorded": recorded}))
        }
        // Captain mail from the linking machine, whose captain's desk is here.
        "fleet.captain" => {
            let from = reaching_here(&conn, required(args, "machine")?)?;
            let (mut delivered, mut refused) = (vec![], vec![]);
            for envelope in args["messages"].as_array().into_iter().flatten() {
                let id = required(envelope, "id")?;
                ensure!(
                    envelope["to"] == crate::mail::CAPTAIN,
                    "only the captain's mail is handed over this way"
                );
                // Into the home holding the request it answers, as collected
                // captain mail is; a home that cannot be read now is offered
                // again on the next pass.
                let Ok((Some(home), held)) = captain_destination(&conn, envelope) else {
                    continue;
                };
                let mut envelope = envelope.clone();
                envelope["unmatched"] = json!(!held);
                let outcome = Store::open(&home).and_then(|mut store| {
                    crate::dispatch(&mut store, "mail.deliver", envelope.clone())
                });
                match outcome {
                    Ok(_) => delivered.push(json!(id)),
                    Err(error) => {
                        let reason = format!("{error:#}");
                        // The sending machine holds a sender this one does not know yet.
                        if !reason.starts_with(UNKNOWN_SHIP) {
                            surface_captain_refusal(&conn, id, &envelope, &reason)?;
                        }
                        refused.push(json!({"id": id, "error": reason}));
                    }
                }
            }
            // A closure ends the desk's copy of a request it holds from a ship
            // of the sending machine; a copy it never held has nothing to end.
            // A malformed one is refused for good and reported. Any other one
            // that fails is left out of both lists, so the sender offers it again.
            // Neither kind can fail the closures after it.
            let peer = peer_name(&from.name);
            let mut closed = vec![];
            let mut closures_refused = vec![];
            for closure in args["closures"].as_array().into_iter().flatten() {
                if let Err(error) = crate::mail::check_closure(closure) {
                    closures_refused
                        .push(json!({"id": closure["id"], "error": format!("{error:#}")}));
                } else if apply_closure(&conn, &peer, closure).is_ok() {
                    closed.push(closure["id"].clone());
                }
            }
            Ok(
                json!({"delivered": delivered, "refused": refused, "closed": closed,
                "closures_refused": closures_refused}),
            )
        }
        // A ship the linking machine asked for, in this machine's fleet. Only
        // ship.create crosses this way; managing that ship stays here.
        "ship.create" => {
            let from = reaching_here(&conn, required(args, "from_machine")?)?;
            crate::fleet::create_for_machine(&conn, &from.name, args)
        }
        // One step of moving a ship to or from this machine.
        _ if command.starts_with("move.") => crate::moves::serve(command, args),
        // A stow runs under this machine's own ledger, from any home here.
        _ if command.starts_with("stow.") => {
            let home = crate::fleet::members(&conn)?
                .into_iter()
                .find(|m| m.home.is_dir())
                .context("this machine has no ships to stow")?
                .home;
            let mut store = Store::open_as(&home, caller.clone())?;
            crate::dispatch(&mut store, command, args.clone())
        }
        _ => bail!("unknown gateway command {command}"),
    }
}

/// Run a machine's gateway: one JSON request per line on stdin, one response
/// per line on stdout, like a home's RPC.
pub fn gateway(caller: Caller) -> Result<()> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut out = std::io::stdout().lock();
    loop {
        let mut line = Vec::new();
        let read = input
            .by_ref()
            .take(1024 * 1024 + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(());
        }
        ensure!(line.len() <= 1024 * 1024, "gateway line exceeds 1 MiB");
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let response = match serde_json::from_slice::<Value>(&line) {
            Ok(request) => {
                let id = request.get("id").cloned().unwrap_or(Value::Null);
                let command = request["command"].as_str().unwrap_or("");
                let args = request.get("args").cloned().unwrap_or_else(|| json!({}));
                match serve_request(&caller, command, &args) {
                    Ok(result) => json!({"id": id, "ok": true, "result": result}),
                    Err(error) => json!({"id": id, "ok": false, "error": error.to_string()}),
                }
            }
            Err(_) => json!({"id": null, "ok": false, "error": "invalid JSON request"}),
        };
        serde_json::to_writer(&mut out, &response)?;
        out.write_all(b"\n")?;
        out.flush()?;
    }
}

/// `privateer machine add NAME SSH_TARGET --executable PATH [--remote-fleet
/// PATH] [--as NAME] | list | refresh [NAME] | collect [NAME] | remove NAME |
/// owner [SHIP|--clear] | captain [MACHINE|--here]`
pub fn cli(args: &[String]) -> Result<()> {
    let usage = "usage: privateer machine add NAME SSH_TARGET --executable /remote/privateer [--remote-fleet /remote/fleet.sqlite] [--as THIS_MACHINE] | list | refresh [NAME] | collect [NAME] | remove NAME | owner [SHIP|--clear] | captain [MACHINE|--here]";
    let flags = crate::cli::flags(args, &["executable", "remote-fleet", "as"])?;
    let result = match flags.positional.first().map(String::as_str) {
        Some("add") => {
            let mut request = json!({
                "name": flags.positional.get(1).context(usage)?,
                "target": flags.positional.get(2).context(usage)?,
                "executable": flags.values.get("executable").context(usage)?,
            });
            if let Some(fleet) = flags.values.get("remote-fleet") {
                request["fleet"] = json!(fleet);
            }
            if let Some(known_as) = flags.values.get("as") {
                request["as"] = json!(known_as);
            }
            add(&request)?
        }
        Some("list") => list()?,
        Some("refresh") => refresh(flags.positional.get(1).map(String::as_str))?,
        Some("collect") => collect_now(flags.positional.get(1).map(String::as_str))?,
        Some("remove") => remove(flags.positional.get(1).context(usage)?)?,
        Some("captain") if flags.switches.iter().any(|s| s == "here") => set_desk(None)?,
        Some("captain") => match flags.positional.get(1) {
            Some(machine) => set_desk(Some(machine))?,
            None => {
                let conn = crate::fleet::registry()?;
                json!({"captain": desk(&conn)?.map(|(m, _)| m).unwrap_or_else(|| "here".into())})
            }
        },
        Some("owner") if flags.switches.iter().any(|s| s == "clear") => set_owner(None)?,
        Some("owner") => match flags.positional.get(1) {
            Some(ship) => set_owner(Some(ship))?,
            None => json!({"owner": owner(&crate::fleet::registry()?)?}),
        },
        _ => bail!(usage),
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

/// The `machine.*` JSON commands, for the operator.
pub fn dispatch(store: &Store, command: &str, args: Value) -> Result<Value> {
    store.caller().require_operator(command)?;
    match command {
        "machine.add" => add(&args),
        "machine.list" => list(),
        "machine.refresh" => refresh(args.get("name").and_then(Value::as_str)),
        "machine.collect" => collect_now(args.get("name").and_then(Value::as_str)),
        "machine.remove" => remove(required(&args, "name")?),
        "machine.captain" => match args.get("machine") {
            None => {
                let conn = crate::fleet::registry()?;
                Ok(
                    json!({"captain": desk(&conn)?.map(|(m, _)| m).unwrap_or_else(|| "here".into())}),
                )
            }
            Some(Value::Null) => set_desk(None),
            Some(machine) => set_desk(Some(machine.as_str().context("machine must be a name")?)),
        },
        "machine.owner" => match args.get("ship") {
            None => Ok(json!({"owner": owner(&crate::fleet::registry()?)?})),
            Some(Value::Null) => set_owner(None),
            Some(ship) => set_owner(Some(ship.as_str().context("ship must be a name")?)),
        },
        _ => bail!("unknown machine command {command}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machines_table_from_an_earlier_build_gains_its_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE machines(name TEXT PRIMARY KEY,
               role TEXT NOT NULL CHECK(role IN ('collects','collected')),
               peer TEXT NOT NULL, known_as TEXT, added_ms INTEGER NOT NULL, refreshed_ms INTEGER);
             INSERT INTO machines(name, role, peer, added_ms) VALUES('mac', 'collects', '{}', 1);",
        )
        .unwrap();
        init(&conn).unwrap();
        init(&conn).unwrap();
        let listed = machines(&conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].next_contact_ms, listed[0].failures), (None, 0));
    }
}
