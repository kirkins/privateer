//! Tightening-only shared policy. No operator dispatch, launch, reconciliation,
//! recovery or ledger initialization is reachable here.
use super::scoped::{DelegatedContext, Denial};
use crate::{
    admission::resource_actions as durable,
    store::{Caller, Store},
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{os::unix::fs::MetadataExt, path::PathBuf, time::Duration};

fn keys(v: &Value, allowed: &[&str], required: &[&str]) -> bool {
    v.as_object().is_some_and(|o| {
        o.keys().all(|k| allowed.contains(&k.as_str()))
            && required.iter().all(|k| o.contains_key(*k))
    })
}
fn handle(v: &Value) -> bool {
    v.as_str()
        .is_some_and(|s| s.len() <= 128 && crate::store::identifier(s).is_ok())
}
pub(super) fn validate_args(a: &Value) -> Result<(), Denial> {
    if !keys(
        a,
        &["machine", "ship", "changes", "idempotency_key"],
        &["machine", "changes", "idempotency_key"],
    ) || !handle(&a["machine"])
        || a.get("ship").is_some_and(|s| !handle(s))
        || a["idempotency_key"].as_str().is_none_or(|k| {
            k.is_empty() || k.len() > 128 || !k.bytes().all(|b| (32..=126).contains(&b))
        })
        || a["changes"].as_object().is_none_or(|o| {
            o.is_empty()
                || o.iter().any(|(k, v)| match k.as_str() {
                    "max_heavy_jobs" => v.as_u64().is_none_or(|n| !(1..=1024).contains(&n)),
                    "min_available_memory_bytes" | "min_free_disk_bytes" => v.as_u64().is_none(),
                    _ => true,
                })
        })
    {
        return Err(Denial::Arguments);
    }
    Ok(())
}
fn authority(c: &DelegatedContext) -> Result<()> {
    ensure!(c.has("fleet:admin"), Denial::Scope);
    ensure!(c.ships().is_none(), Denial::Ship);
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    ledger: String,
    incarnation: Option<String>,
}

struct Target {
    conn: Connection,
    path: PathBuf,
    identity: String,
}
impl Target {
    fn identity(conn: &Connection, path: &std::path::Path) -> Result<String> {
        let m = std::fs::metadata(path)?;
        ensure!(
            m.is_file() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o022 == 0,
            Denial::ExecutionFailed
        );
        // Random native ledger origin, never a plain hash of a path/hostname.
        Ok(crate::core::hash(&json!([
            durable::identity(conn)?,
            m.dev(),
            m.ino()
        ])))
    }
    fn recheck(&self, registry: &Connection, a: &Value) -> Result<()> {
        ensure!(
            ledger_path(registry, a)? == self.path
                && Self::identity(&self.conn, &self.path)? == self.identity,
            Denial::ExecutionFailed
        );
        Ok(())
    }
}
fn ledger_path(registry: &Connection, a: &Value) -> Result<PathBuf> {
    let home: Option<String> = match a["ship"].as_str() {
        Some(ship) => Some(
            registry
                .query_row(
                    "SELECT home FROM ships WHERE name=?1 AND retired_ms IS NULL",
                    [ship],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or(Denial::Ship)?,
        ),
        None => None,
    };
    if let (Some(ship), Some(home)) = (a["ship"].as_str(), home.as_ref()) {
        let store = Store::open_readonly(std::path::Path::new(home), Caller::Operator)?;
        let owned: bool = store.connection().query_row("SELECT EXISTS(SELECT 1 FROM ships WHERE name=?1 AND json_extract(record,'$.location')='here')",[ship],|r| r.get(0))?;
        ensure!(owned, Denial::Ship);
    }
    let path = crate::jobs::ledger_dir_at(home.as_deref().map(std::path::Path::new))?
        .join("resources.sqlite");
    ensure!(path.is_absolute(), Denial::ExecutionFailed);
    crate::store::reject_symlinks(&path)?;
    Ok(path)
}
fn open(registry: &Connection, a: &Value, write: bool) -> Result<Target> {
    let path = ledger_path(registry, a)?;
    let before = std::fs::metadata(&path)?;
    let conn = Connection::open_with_flags(
        &path,
        if write {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        },
    )?;
    conn.busy_timeout(Duration::from_millis(250))?;
    if !write {
        conn.execute_batch("PRAGMA query_only=ON")?;
    }
    let after = std::fs::metadata(&path)?;
    ensure!(
        before.dev() == after.dev() && before.ino() == after.ino(),
        Denial::ExecutionFailed
    );
    let identity = Target::identity(&conn, &path)?;
    Ok(Target {
        conn,
        path,
        identity,
    })
}

/// Runs after external validation/authentication. The internal binding cannot be
/// supplied by a client and is hashed into originating durable key admission.
pub(super) fn prepare(a: &mut Value, c: &DelegatedContext) -> Result<()> {
    validate_args(a)?;
    authority(c)?;
    let registry = super::runtime::registry_readonly()?;
    let machine = a["machine"].as_str().ok_or(Denial::Arguments)?;
    let binding = if machine == "local" {
        Binding {
            ledger: open(&registry, a, false)?.identity,
            incarnation: None,
        }
    } else {
        let callable: bool = registry.query_row(
            "SELECT EXISTS(SELECT 1 FROM machines WHERE name=?1 AND role='collects')",
            [machine],
            |r| r.get(0),
        )?;
        ensure!(callable, Denial::LinkedUnavailable);
        if let Some(ship) = a["ship"].as_str() {
            let present: bool = registry.query_row(
                "SELECT EXISTS(SELECT 1 FROM machine_ships WHERE name=?1 AND machine=?2)",
                params![ship, machine],
                |r| r.get(0),
            )?;
            ensure!(present, Denial::Ship);
        }
        let incarnation = crate::machines::link_incarnation(&registry, machine)?;
        let answer = crate::machines::resource_action_of(
            machine,
            &incarnation,
            &json!({"context":c.gateway(),"operation":"target","args":a}),
            false,
        )?;
        ensure!(
            keys(&answer, &["ledger"], &["ledger"]) && opaque(&answer["ledger"]),
            Denial::ExecutionFailed
        );
        Binding {
            ledger: answer["ledger"].as_str().unwrap().into(),
            incarnation: Some(incarnation),
        }
    };
    a["_binding"] = serde_json::to_value(binding)?;
    Ok(())
}
fn opaque(v: &Value) -> bool {
    v.as_str()
        .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub(super) fn request(a: &Value, c: &DelegatedContext) -> Result<Value> {
    authority(c)?;
    let mut public = a.clone();
    let binding: Binding = serde_json::from_value(
        public
            .as_object_mut()
            .ok_or(Denial::Arguments)?
            .remove("_binding")
            .ok_or(Denial::Arguments)?,
    )?;
    validate_args(&public)?;
    let result = if let Some(incarnation) = binding.incarnation {
        crate::machines::resource_action_of(
            public["machine"].as_str().ok_or(Denial::Arguments)?,
            &incarnation,
            &json!({"context":c.gateway(),"operation":"resource_configure","args":public,"target":binding.ledger}),
            true,
        )?
    } else {
        ensure!(public["machine"] == "local", Denial::Arguments);
        apply(
            &super::runtime::registry_readonly()?,
            &public,
            c,
            &binding.ledger,
        )?
    };
    validate_reply(&result, &public, c).map_err(|_| Denial::OutcomeUnknown)?;
    Ok(result)
}

pub(crate) fn serve(body: &Value) -> Result<Value> {
    ensure!(
        keys(
            body,
            &["context", "operation", "args", "target"],
            &["context", "operation", "args"]
        ),
        Denial::Arguments
    );
    let c = DelegatedContext::from_gateway(&body["context"]).map_err(|_| Denial::Arguments)?;
    authority(&c)?;
    validate_args(&body["args"])?;
    let registry = super::runtime::registry_readonly()?;
    match body["operation"].as_str() {
        Some("target") => {
            ensure!(body.get("target").is_none(), Denial::Arguments);
            Ok(json!({"ledger":open(&registry,&body["args"],false)?.identity}))
        }
        Some("resource_configure") => {
            ensure!(opaque(&body["target"]), Denial::Arguments);
            apply(
                &registry,
                &body["args"],
                &c,
                body["target"].as_str().unwrap(),
            )
        }
        _ => Err(Denial::Unsupported.into()),
    }
}

fn validate_reply(v: &Value, a: &Value, c: &DelegatedContext) -> Result<()> {
    ensure!(
        keys(
            v,
            &[
                "schema",
                "operation",
                "machine",
                "ship",
                "result_id",
                "applied",
                "outcome",
                "changed",
                "provenance"
            ],
            &[
                "schema",
                "operation",
                "machine",
                "ship",
                "result_id",
                "applied",
                "outcome",
                "changed",
                "provenance"
            ]
        ) && v["schema"] == "resource.action.v1"
            && v["operation"] == "resource_configure"
            && v["machine"] == a["machine"]
            && v["ship"] == a["ship"]
            && v["applied"] == true
            && v["outcome"] == "configured"
            && v["changed"].is_boolean(),
        Denial::Arguments
    );
    let id = v["result_id"].as_str().ok_or(Denial::Arguments)?;
    uuid::Uuid::parse_str(id)?;
    let mut provenance = c.provenance();
    provenance["result_id"] = json!(id);
    ensure!(v["provenance"] == provenance, Denial::Arguments);
    Ok(())
}
fn apply(registry: &Connection, a: &Value, c: &DelegatedContext, wanted: &str) -> Result<Value> {
    authority(c)?;
    validate_args(a)?;
    let target = open(registry, a, true).map_err(|e| {
        e.downcast_ref::<Denial>()
            .copied()
            .unwrap_or(Denial::ExecutionFailed)
    })?;
    let tx = rusqlite::Transaction::new_unchecked(
        &target.conn,
        rusqlite::TransactionBehavior::Immediate,
    )
    .map_err(|_| Denial::ExecutionFailed)?;
    ensure!(target.identity == wanted, Denial::ExecutionFailed);
    target
        .recheck(registry, a)
        .map_err(|_| Denial::ExecutionFailed)?;
    let key = crate::core::hash(&a["idempotency_key"]);
    let hash = crate::core::hash(&json!([
        a,
        wanted,
        c.capabilities(),
        c.visibility_revision()
    ]));
    let now = Store::now_ms();
    if let Some((old, result)) = durable::lookup(&tx, c.client(), c.resource(), &key, now)
        .map_err(|_| Denial::ExecutionFailed)?
    {
        ensure!(old == hash, Denial::Arguments);
        let result: Value = serde_json::from_str(&result).map_err(|_| Denial::OutcomeUnknown)?;
        validate_reply(&result, a, c).map_err(|_| Denial::OutcomeUnknown)?;
        tx.commit().map_err(|_| Denial::OutcomeUnknown)?;
        return Ok(result);
    }
    ensure!(
        durable::has_capacity(&tx, c.client()).map_err(|_| Denial::ExecutionFailed)?,
        Denial::RetryCapacity
    );
    let (before, after) =
        crate::admission::tighten_in(&tx, &a["changes"]).map_err(|_| Denial::ExecutionFailed)?;
    let before = serde_json::to_value(before)?;
    let after = serde_json::to_value(after)?;
    let id = c.provenance()["result_id"].clone();
    let result = json!({"schema":"resource.action.v1","operation":"resource_configure","machine":a["machine"],"ship":a["ship"],"result_id":id,"applied":true,"outcome":"configured","changed":before!=after,"provenance":c.provenance()});
    let fields = a["changes"]
        .as_object()
        .unwrap()
        .keys()
        .map(|k| (k.clone(), json!({"before":before[k],"after":after[k]})))
        .collect::<serde_json::Map<_, _>>();
    let audit = json!({"operation":"resource_configure","grant":c.client(),"resource":c.resource(),"ledger":wanted,"changes":fields,"result_id":id,"idempotency_key_hash":key});
    tx.execute("INSERT INTO resource_action_results(grant_id,resource,key_hash,args_hash,created_ms,result,audit) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![c.client(),c.resource(),key,hash,now,result.to_string(),audit.to_string()]).map_err(|_|Denial::ExecutionFailed)?;
    target
        .recheck(registry, a)
        .map_err(|_| Denial::ExecutionFailed)?;
    tx.commit().map_err(|_| Denial::OutcomeUnknown)?;
    Ok(result)
}

#[cfg(test)]
mod tests;
