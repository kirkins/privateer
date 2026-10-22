//! Standing grants: authority the captain gives one ship over others.
//!
//! A ship may manage the ships it created. Anything beyond that comes from a
//! grant the captain creates: a grantee ship, the actions it may take, and
//! the ships it may take them on, recorded in this machine's fleet registry.
//! A grant can be paused, revoked or left to expire, and every use of one is
//! audited in the grantee's home and, when the captain asked for it, told to
//! the captain. Creating, pausing and revoking grants are the operator's;
//! a ship may list the grants it holds. A grant never widens what a ship may
//! do inside its own charter; it only lets it act on other ships.
use crate::store::{Caller, Store, identifier, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

/// The actions a grant can carry: the commands that act on another ship.
pub const ACTIONS: &[&str] = &[
    "ship.retire",
    "ship.scope",
    "ship.actions",
    "ship.consumes",
    "ship.requires",
    "project.add",
];

/// The most ships one grant may cover.
const MAX_SHIPS: usize = 64;

pub(crate) fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS grants(id TEXT PRIMARY KEY, grantee TEXT NOT NULL,
           actions TEXT NOT NULL, ships TEXT NOT NULL, notify INTEGER NOT NULL DEFAULT 0,
           note TEXT, created_ms INTEGER NOT NULL, expires_ms INTEGER, paused_ms INTEGER,
           revoked_ms INTEGER);",
    )?;
    Ok(())
}

fn grant(row: &rusqlite::Row) -> rusqlite::Result<Value> {
    let parse = |text: String| serde_json::from_str::<Value>(&text).unwrap_or(json!([]));
    Ok(json!({
        "id": row.get::<_, String>(0)?,
        "grantee": row.get::<_, String>(1)?,
        "actions": parse(row.get(2)?),
        "ships": parse(row.get(3)?),
        "notify": row.get::<_, bool>(4)?,
        "note": row.get::<_, Option<String>>(5)?,
        "created_ms": row.get::<_, i64>(6)?,
        "expires_ms": row.get::<_, Option<i64>>(7)?,
        "paused_ms": row.get::<_, Option<i64>>(8)?,
        "revoked_ms": row.get::<_, Option<i64>>(9)?,
    }))
}

const COLUMNS: &str =
    "id, grantee, actions, ships, notify, note, created_ms, expires_ms, paused_ms, revoked_ms";

fn find(conn: &Connection, id: &str) -> Result<Value> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM grants WHERE id=?1"),
        [id],
        grant,
    )
    .optional()?
    .with_context(|| format!("no grant {id}"))
}

fn names(value: &Value, what: &str) -> Result<Vec<String>> {
    let list = value
        .as_array()
        .with_context(|| format!("{what} is a list"))?;
    let mut names: Vec<String> = vec![];
    for item in list {
        let name = item
            .as_str()
            .with_context(|| format!("{what} holds names"))?;
        if !names.iter().any(|n| n == name) {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// Record a grant write in the grantee's home, beside its uses. A grantee
/// whose home is gone keeps the write; the result says it was not recorded.
fn record(mut grant: Value, kind: &str, members: &[crate::fleet::Member]) -> Value {
    let grantee = grant["grantee"].as_str().unwrap_or_default();
    let recorded = (|| -> Result<()> {
        let member = members
            .iter()
            .find(|m| m.name == grantee)
            .filter(|m| m.home.is_dir())
            .with_context(|| format!("{grantee} has no home on this machine"))?;
        Store::open(&member.home)?.audit(
            kind,
            &json!({"grant": grant["id"], "grantee": grantee, "actions": grant["actions"],
                "ships": grant["ships"], "expires_ms": grant["expires_ms"]}),
        )?;
        Ok(())
    })();
    if let Err(error) = recorded {
        grant["audit"] = json!(format!("{kind} done, but not recorded: {error:#}"));
    }
    grant
}

/// Create a grant, or replace an active one by id. A paused or revoked grant
/// stays as it is: resume it, or make a new one.
fn put(args: &Value) -> Result<Value> {
    let mut conn = crate::fleet::registry()?;
    let grantee = required(args, "grantee")?;
    identifier(grantee)?;
    let members = crate::fleet::members(&conn)?;
    ensure!(
        members.iter().any(|m| m.name == grantee),
        "{grantee} is not a ship on this machine"
    );
    let actions = names(&args["actions"], "actions")?;
    ensure!(!actions.is_empty(), "a grant names at least one action");
    for action in &actions {
        ensure!(
            ACTIONS.contains(&action.as_str()),
            "a grant cannot carry {action}; it can carry {}",
            ACTIONS.join(", ")
        );
    }
    let ships = names(&args["ships"], "ships")?;
    ensure!(!ships.is_empty(), "a grant names the ships it covers");
    ensure!(
        ships.len() <= MAX_SHIPS,
        "a grant covers at most {MAX_SHIPS} ships"
    );
    for ship in &ships {
        identifier(ship)?;
        ensure!(ship != grantee, "{grantee} needs no grant over itself");
        ensure!(
            members.iter().any(|m| &m.name == ship),
            "{ship} is not a ship on this machine"
        );
    }
    let expires_ms = args.get("expires_ms").and_then(Value::as_i64);
    if let Some(expires) = expires_ms {
        ensure!(
            expires > Store::now_ms(),
            "the grant would already be expired"
        );
    }
    let id = match args.get("id").and_then(Value::as_str) {
        Some(id) => {
            identifier(id)?;
            id.to_owned()
        }
        None => format!("grant-{grantee}-{}", Store::now_ms()),
    };
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Ok(existing) = find(&tx, &id) {
        ensure!(
            existing["revoked_ms"].is_null(),
            "grant {id} was revoked; make a new grant"
        );
        ensure!(
            existing["paused_ms"].is_null(),
            "grant {id} is paused; resume it, or make a new grant"
        );
    }
    tx.execute(
        "INSERT INTO grants(id, grantee, actions, ships, notify, note, created_ms, expires_ms)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT(id) DO UPDATE SET
           grantee=excluded.grantee, actions=excluded.actions, ships=excluded.ships,
           notify=excluded.notify, note=excluded.note, expires_ms=excluded.expires_ms",
        params![
            id,
            grantee,
            json!(actions).to_string(),
            json!(ships).to_string(),
            args.get("notify").and_then(Value::as_bool).unwrap_or(false),
            args.get("note").and_then(Value::as_str),
            Store::now_ms(),
            expires_ms
        ],
    )?;
    let grant = find(&tx, &id)?;
    tx.commit()?;
    Ok(record(grant, "grant.put", &members))
}

fn list(caller: &Caller, args: &Value) -> Result<Value> {
    let conn = crate::fleet::registry()?;
    // A ship sees the grants it holds; the operator sees them all.
    let grantee = match caller {
        Caller::Agent { actor } => Some(actor.clone()),
        Caller::Operator => args
            .get("grantee")
            .and_then(Value::as_str)
            .map(str::to_owned),
    };
    let mut statement = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM grants WHERE (?1 IS NULL OR grantee=?1) ORDER BY created_ms"
    ))?;
    let grants = statement
        .query_map([grantee], grant)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"grants": grants}))
}

fn set(id: &str, kind: &str, column: &str, value: Option<i64>) -> Result<Value> {
    identifier(id)?;
    let conn = crate::fleet::registry()?;
    find(&conn, id)?;
    conn.execute(
        &format!("UPDATE grants SET {column}=?2 WHERE id=?1"),
        params![id, value],
    )?;
    let grant = find(&conn, id)?;
    Ok(record(grant, kind, &crate::fleet::members(&conn)?))
}

fn pause(id: &str) -> Result<Value> {
    set(id, "grant.pause", "paused_ms", Some(Store::now_ms()))
}

fn resume(id: &str) -> Result<Value> {
    set(id, "grant.resume", "paused_ms", None)
}

fn revoke(id: &str) -> Result<Value> {
    set(id, "grant.revoke", "revoked_ms", Some(Store::now_ms()))
}

/// The active grant, if any, that lets `grantee` take `action` on `ship`.
fn allowing(conn: &Connection, grantee: &str, action: &str, ship: &str) -> Result<Option<Value>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM grants WHERE grantee=?1 AND revoked_ms IS NULL
           AND paused_ms IS NULL AND (expires_ms IS NULL OR expires_ms>?2)
           AND EXISTS (SELECT 1 FROM json_each(actions) WHERE value=?3)
           AND EXISTS (SELECT 1 FROM json_each(ships) WHERE value=?4)
         ORDER BY created_ms LIMIT 1"
    ))?;
    Ok(statement
        .query_row(params![grantee, Store::now_ms(), action, ship], grant)
        .optional()?)
}

/// Let an agent take `action` on `ship` under a grant, recording the use in
/// its home, and telling the captain when the grant asks for that. Refuses
/// with the reason when no active grant covers it.
pub(crate) fn require(store: &Store, actor: &str, action: &str, ship: &str) -> Result<String> {
    let conn = crate::fleet::registry()?;
    let Some(grant) = allowing(&conn, actor, action, ship)? else {
        bail!("{actor} holds no active grant for {action} on {ship}")
    };
    let id = grant["id"].as_str().unwrap_or_default().to_owned();
    store.audit(
        "grant.used",
        &json!({"grant": id, "ship": actor, "action": action, "on": ship}),
    )?;
    if grant["notify"] == true {
        crate::mail::dispatch(
            &mut Store::open(store.home())?,
            "mail.send",
            json!({"from": actor, "to": crate::mail::CAPTAIN, "kind": "notice",
                "body": format!("{actor} used your grant {id} for {action} on {ship}.")}),
        )?;
    }
    Ok(id)
}

pub fn dispatch(store: &Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "grant.list" => list(store.caller(), &args),
        _ => {
            store.caller().require_operator(command)?;
            match command {
                "grant.put" => put(&args),
                "grant.pause" => pause(required(&args, "id")?),
                "grant.resume" => resume(required(&args, "id")?),
                "grant.revoke" => revoke(required(&args, "id")?),
                _ => bail!("unknown grant command {command}"),
            }
        }
    }
}

/// `privateer grant add GRANTEE --actions A,B --ships X,Y [--notify]
/// [--expires-hours N] [--note TEXT] | list [GRANTEE] | pause ID | resume ID
/// | revoke ID`
pub fn cli(args: &[String]) -> Result<()> {
    let usage = "usage: privateer grant add GRANTEE --actions ship.scope,ship.retire --ships SHIP,SHIP [--notify] [--expires-hours N] [--note TEXT] | list [GRANTEE] | pause ID | resume ID | revoke ID";
    let flags = crate::cli::flags(args, &["actions", "ships", "expires-hours", "note"])?;
    let split = |key: &str| -> Value {
        json!(
            flags
                .values
                .get(key)
                .map(|v| v
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>())
                .unwrap_or_default()
        )
    };
    let operator = Caller::Operator;
    let result = match flags.positional.first().map(String::as_str) {
        Some("add") => {
            let mut request = json!({
                "grantee": flags.positional.get(1).context(usage)?,
                "actions": split("actions"),
                "ships": split("ships"),
                "notify": flags.switches.iter().any(|s| s == "notify"),
            });
            if let Some(hours) = flags.values.get("expires-hours") {
                let hours: i64 = hours.parse().context("--expires-hours takes a number")?;
                request["expires_ms"] = json!(Store::now_ms() + hours * 3_600_000);
            }
            if let Some(note) = flags.values.get("note") {
                request["note"] = json!(note);
            }
            put(&request)?
        }
        Some("list") => list(&operator, &json!({"grantee": flags.positional.get(1)}))?,
        Some("pause") => pause(flags.positional.get(1).context(usage)?)?,
        Some("resume") => resume(flags.positional.get(1).context(usage)?)?,
        Some("revoke") => revoke(flags.positional.get(1).context(usage)?)?,
        _ => bail!(usage),
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
