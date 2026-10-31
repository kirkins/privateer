//! Operator opt-in only. No scoped dispatch, tools, reader admission or network
//! path is enabled here. Keep the immutable switch-on evidence across disable,
//! restart and identical re-enable; later replay eligibility consumes it.
use crate::machines::desk_placement::{self, ExposureRefusal, Manifest};
use crate::store::{Store, identifier, required};
use anyhow::{Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const ACCEPTED: crate::Accepted = &[
    (
        "door.desk.enable",
        &["door", "domain", "mode", "replay_policy"],
    ),
    ("door.desk.status", &["door"]),
    ("door.desk.disable", &["door"]),
];
const TABLE: &str = "door_desk_exposure";
const SLOT_RELEASE: &str = "Revoking the owning grant is the only supported operator action to free a slot. Pause, disable, token rotation and abandonment do not free slots; terminal expiry is retired by engine admission, not by status.";
const WARNING: &str = "Independent fresh replay: open requests plus mail from switch-on, including outgoing mail; older history only by explicit thread lookup. No client exposure yet. Selected-mailbox unsupported read direction / incomplete feature is unchanged.";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Binding {
    domain: String,
    owner_core: String,
    generation: u64,
    incarnation: String,
    manifest_revision: i64,
    manifest_hash: String,
    mode: String,
    replay_policy: String,
    switch_on_ms: i64,
    switch_on: Vec<Value>,
}
impl Binding {
    fn matches(&self, m: &Manifest) -> Result<bool> {
        Ok(self.mode == "fresh_replay"
            && self.replay_policy == "open_and_new"
            && self.domain == m.domain
            && self.owner_core == m.owner_core
            && self.generation == m.generation
            && self.incarnation == m.incarnation
            && self.manifest_revision == m.manifest_revision
            && self.manifest_hash == crate::core::hash(&serde_json::to_value(m)?))
    }
    fn manifest_changed(&self, m: &Manifest) -> Result<bool> {
        Ok(self.domain == m.domain
            && self.owner_core == m.owner_core
            && self.generation == m.generation
            && self.incarnation == m.incarnation
            && (self.manifest_revision != m.manifest_revision
                || self.manifest_hash != crate::core::hash(&serde_json::to_value(m)?)))
    }
}
fn switch_on(m: &Manifest, evidence: &[Value]) -> Vec<Value> {
    m.sources.iter().map(|s| {
        let e = evidence.iter().find(|e| e["id"] == s.core);
        json!({"source":s.core,
            "state":if e.is_some_and(|e| e["complete"] == true) {"established"} else {"switch_on_pending"},
            "through":e.map(|e| &e["through"]),
            "barrier_mode":e.map(|e| &e["barrier_mode"]),
            "received_at":e.map(|e| &e["received_at"]),
            "evidence_state":e.map(|e| &e["state"])})
    }).collect()
}
fn exists(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |r| r.get(0),
    )?)
}
fn install(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS door_desk_exposure(
        door TEXT PRIMARY KEY, enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
        binding TEXT NOT NULL);",
    )?;
    Ok(())
}
fn stored(conn: &Connection, door: &str) -> Result<Option<(bool, Binding)>> {
    if !exists(conn, TABLE)? {
        return Ok(None);
    }
    let row: Option<(bool, String)> = conn
        .query_row(
            "SELECT enabled,binding FROM door_desk_exposure WHERE door=?1",
            [door],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(enabled, binding)| Ok((enabled, serde_json::from_str(&binding)?)))
        .transpose()
}
fn registered(conn: &Connection, door: &str) -> Result<()> {
    ensure!(exists(conn, "doors")?, "no registered door");
    let protocol: Option<u32> = conn
        .query_row("SELECT protocol FROM doors WHERE name=?1", [door], |r| {
            r.get(0)
        })
        .optional()?;
    ensure!(
        protocol == Some(2),
        "desk exposure requires a registered scoped v2 door"
    );
    Ok(())
}

pub(crate) fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    store.caller().require_operator(command)?;
    match command {
        "door.desk.enable" | "door.desk.status" | "door.desk.disable" => run(command, &args),
        _ => bail!("unknown desk exposure command"),
    }
}
fn run(command: &str, args: &Value) -> Result<Value> {
    let door = required(args, "door")?;
    identifier(door)?;
    let enabling = command == "door.desk.enable";
    ensure!(
        matches!(
            command,
            "door.desk.enable" | "door.desk.status" | "door.desk.disable"
        ),
        "unknown desk exposure command"
    );
    if enabling {
        identifier(required(args, "domain")?)?;
        ensure!(
            required(args, "mode")? == "fresh_replay",
            "mode must be fresh_replay"
        );
        ensure!(
            required(args, "replay_policy")? == "open_and_new",
            "replay_policy must be open_and_new"
        );
    }
    let conn = crate::fleet::registry()?;
    // Lock order agrees with placement/intake/subscriptions. No network call
    // occurs here; a read-only status does not initialize any door schema.
    let (_fence, _) = desk_placement::exposure_placement(&conn)?;
    let tx = rusqlite::Transaction::new_unchecked(
        &conn,
        if command == "door.desk.status" {
            TransactionBehavior::Deferred
        } else {
            TransactionBehavior::Immediate
        },
    )?;
    registered(&tx, door)?;
    let (_check, placement) = desk_placement::exposure_placement(&tx)?;
    if enabling {
        let m = match &placement {
            Ok(m) => m,
            Err(reason) => bail!(reason.code()),
        };
        ensure!(
            required(args, "domain")? == m.domain,
            "stale_generation: domain differs from certified placement"
        );
        if let Some((_, old)) = stored(&tx, door)? {
            ensure!(
                old.matches(m)?,
                "stale_generation: non-identical binding requires reconciliation (manifest_changed if manifest differs)"
            );
        } else {
            // Journal/barrier evidence, never remote wall time or dashboard
            // progress. Unproved sources stay pending, not 'new on arrival'.
            let evidence = crate::machines::desk_intake::subscription::source_vector(&tx, m);
            let switch_on = switch_on(m, &evidence);
            let binding = Binding {
                domain: m.domain.clone(),
                owner_core: m.owner_core.clone(),
                generation: m.generation,
                incarnation: m.incarnation.clone(),
                manifest_revision: m.manifest_revision,
                manifest_hash: crate::core::hash(&serde_json::to_value(m)?),
                mode: "fresh_replay".into(),
                replay_policy: "open_and_new".into(),
                switch_on_ms: Store::now_ms(),
                switch_on,
            };
            install(&tx)?;
            tx.execute(
                "INSERT INTO door_desk_exposure(door,enabled,binding) VALUES(?1,1,?2)",
                params![door, serde_json::to_string(&binding)?],
            )?;
        }
        tx.execute(
            "UPDATE door_desk_exposure SET enabled=1 WHERE door=?1",
            [door],
        )?;
    } else if command == "door.desk.disable" && exists(&tx, TABLE)? {
        tx.execute(
            "UPDATE door_desk_exposure SET enabled=0 WHERE door=?1",
            [door],
        )?;
    }
    let result = status(&tx, door, placement)?;
    tx.commit()?;
    Ok(result)
}

fn status(
    conn: &Connection,
    door: &str,
    placement: std::result::Result<Manifest, ExposureRefusal>,
) -> Result<Value> {
    let stored = stored(conn, door)?;
    let enabled = stored.as_ref().is_some_and(|(enabled, _)| *enabled);
    let binding = stored.as_ref().map(|(_, b)| b);
    let domain = binding
        .map(|b| b.domain.as_str())
        .or_else(|| placement.as_ref().ok().map(|m| m.domain.as_str()));
    let mut detail = if let Some(b) = binding {
        desk_placement::exposure_manifest_changed(
            conn,
            &b.domain,
            b.manifest_revision,
            &b.manifest_hash,
        )?
        .then_some("manifest_changed")
    } else {
        None
    };
    let (state, code) = match &placement {
        Err(reason) => (
            match reason {
                ExposureRefusal::NoPlacement => "no placement",
                ExposureRefusal::RemoteOwner { reverse_only: true } => "reverse-only",
                ExposureRefusal::RemoteOwner { .. } => "remote owner",
                _ => "unavailable",
            }
            .to_string(),
            Some(reason.code()),
        ),
        Ok(m) => {
            if let Some(b) = binding
                && !b.matches(m)?
            {
                if b.manifest_changed(m)? {
                    detail = Some("manifest_changed");
                }
                ("unavailable".into(), Some("stale_generation"))
            } else if enabled {
                (format!("enabled at generation {}", m.generation), None)
            } else {
                ("disabled".into(), Some("disabled"))
            }
        }
    };
    let used: Option<i64> = if exists(conn, "desk_subscription")? {
        domain
            .map(|d| {
                conn.query_row(
                    "SELECT COUNT(*) FROM desk_subscription WHERE domain=?1",
                    [d],
                    |r| r.get(0),
                )
            })
            .transpose()?
    } else {
        domain.map(|_| 0)
    };
    let mut grants = Vec::new();
    if exists(conn, "door_clients")? {
        let mut stmt = conn.prepare("SELECT id,resource,scopes,ships,paused,revoked_ms,expires_ms FROM door_clients WHERE door=?1 ORDER BY id")?;
        for row in stmt.query_map([door], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, bool>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, Option<i64>>(6)?,
            ))
        })? {
            let (id, resource, scopes, ships, paused, revoked, expires) = row?;
            let scopes: Vec<String> = serde_json::from_str(&scopes)?;
            let ships: Option<Vec<String>> = ships.map(|s| serde_json::from_str(&s)).transpose()?;
            let lifecycle = if revoked.is_some() {
                "revoked"
            } else if expires.is_some_and(|t| t <= Store::now_ms()) {
                "expired"
            } else if paused {
                "paused"
            } else {
                "active"
            };
            let eligible = lifecycle == "active"
                && resource == format!("door:{door}")
                && scopes.iter().any(|s| s == "mail:read");
            let slots: i64 = if exists(conn, "desk_subscription")? {
                conn.query_row("SELECT COUNT(*) FROM desk_subscription WHERE domain=?1 AND json_extract(reader,'$[1]')=?2 AND json_extract(reader,'$[3]')=?3", params![domain,door,id], |r|r.get(0))?
            } else {
                0
            };
            grants.push(json!({"id":id,"scopes":scopes,"ships":ships,"lifecycle":lifecycle,"eligible":eligible,"slots_held":slots}));
        }
    }
    Ok(
        json!({"door":door,"enabled":enabled,"state":state,"readiness_code":code,"detail":detail,
        "binding":binding,"domain":domain,"grants":grants,"slots_used":used,"slots_free":used.map(|n|(16-n).max(0)),"slots_limit":16,
        "slot_release":SLOT_RELEASE,"client_exposed":false,"legacy_fence_scope":"certified generation-1 local owner only; sources, pending and restored placements remain fenced until migration",
        "warning":WARNING}),
    )
}

pub(super) fn cli(args: &[String]) -> Result<()> {
    for marker in ["PRIVATEER_ROLE", "PRIVATEER_JOB"] {
        ensure!(
            std::env::var_os(marker).is_none(),
            "desk exposure is operator-only, outside agent/job sessions"
        );
    }
    let usage = "usage: privateer door desk enable DOOR --domain DOMAIN --mode fresh_replay --replay-policy open_and_new | status DOOR | disable DOOR";
    let flags = crate::cli::flags(args, &["domain", "mode", "replay-policy"])?;
    ensure!(flags.positional.len() == 2, usage);
    let command = match flags.positional[0].as_str() {
        "enable" => "door.desk.enable",
        "status" => "door.desk.status",
        "disable" => "door.desk.disable",
        _ => bail!(usage),
    };
    let mut args = json!({"door":flags.positional[1]});
    for (key, value) in flags.values {
        args[key.replace('-', "_")] = json!(value);
    }
    for key in args.as_object().unwrap().keys() {
        ensure!(
            crate::accepted_args(command).is_some_and(|keys| keys.contains(&key.as_str())),
            "unknown argument {key} for {command}"
        );
    }
    println!("{}", serde_json::to_string_pretty(&run(command, &args)?)?);
    Ok(())
}

#[cfg(test)]
mod tests;
