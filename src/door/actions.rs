//! V6 guarded scheduling and exact cancellation mutations. This allowlist is deliberately disjoint from
//! fleet.inspection.v1. No RPC/argv/operator dispatch and no scheduling DTOs in
//! replies: work:manage authorizes an acknowledgment, not additional reads.
use super::scoped::{Command, DelegatedContext, Denial};
use crate::{
    scheduling::references::{self, TaskRef},
    store::{Caller, Store},
};
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;

pub(super) fn is_action(c: Command) -> bool {
    matches!(
        c,
        Command::WorkRank
            | Command::WorkEnqueue
            | Command::WorkDefer
            | Command::JobCancel
            | Command::JobRetry
    )
}
pub(super) fn name(c: Command) -> &'static str {
    match c {
        Command::WorkRank => "work_rank",
        Command::WorkEnqueue => "work_enqueue",
        Command::WorkDefer => "work_defer",
        Command::JobCancel => "job_cancel",
        Command::JobRetry => "job_retry",
        _ => unreachable!("fixed work action"),
    }
}
fn parse(s: &str) -> Result<Command> {
    match s {
        "work_rank" => Ok(Command::WorkRank),
        "work_enqueue" => Ok(Command::WorkEnqueue),
        "work_defer" => Ok(Command::WorkDefer),
        "job_cancel" => Ok(Command::JobCancel),
        "job_retry" => Ok(Command::JobRetry),
        _ => Err(Denial::Unsupported.into()),
    }
}
fn handle(v: &Value) -> bool {
    v.as_str().is_some_and(crate::snapshot::inspection::handle)
}
fn keys(v: &Value, allowed: &[&str], required: &[&str]) -> bool {
    v.as_object().is_some_and(|o| {
        o.keys().all(|k| allowed.contains(&k.as_str()))
            && required.iter().all(|k| o.contains_key(*k))
    })
}
fn wait_shape(w: &Value) -> bool {
    if w["reason"]
        .as_str()
        .is_none_or(|s| s.trim().is_empty() || s.len() > 1000)
    {
        return false;
    }
    match w["kind"].as_str() {
        Some("time" | "attention") => {
            keys(
                w,
                &["kind", "reason", "until_ms"],
                &["kind", "reason", "until_ms"],
            ) && w["until_ms"].as_i64().is_some()
        }
        Some("task" | "request") => {
            keys(w, &["kind", "reason", "ref"], &["kind", "reason", "ref"]) && handle(&w["ref"])
        }
        Some("remote") => {
            keys(w, &["kind", "reason", "ref"], &["kind", "reason", "ref"])
                && keys(&w["ref"], &["ship", "task", "machine"], &["ship", "task"])
                && handle(&w["ref"]["ship"])
                && handle(&w["ref"]["task"])
                && w["ref"].get("machine").is_none_or(handle)
        }
        Some("manual") => keys(w, &["kind", "reason"], &["kind", "reason"]),
        _ => false,
    }
}
pub(super) fn validate_args(c: Command, a: &Value) -> Result<(), Denial> {
    if matches!(c, Command::JobCancel | Command::JobRetry) {
        return cancel::validate_args(a);
    }
    let extra: &[&str] = match c {
        Command::WorkRank => &["rank"],
        Command::WorkEnqueue => &["brief"],
        Command::WorkDefer => &["wait", "offer_id"],
        _ => return Err(Denial::Arguments),
    };
    let Some(o) = a.as_object() else {
        return Err(Denial::Arguments);
    };
    if o.keys().any(|k| {
        !["ship", "reference", "idempotency_key"].contains(&k.as_str())
            && !extra.contains(&k.as_str())
    }) || !handle(&a["ship"])
        || a["ship"].as_str().is_none_or(|s| s.len() > 128)
        || a["idempotency_key"].as_str().is_none_or(|s| {
            s.is_empty() || s.len() > 128 || !s.bytes().all(|b| (0x20..=0x7e).contains(&b))
        })
    {
        return Err(Denial::Arguments);
    }
    let r: TaskRef =
        serde_json::from_value(a["reference"].clone()).map_err(|_| Denial::Arguments)?;
    if r.ship != a["ship"]
        || [&r.origin, &r.ship, &r.task_id]
            .iter()
            .any(|s| !crate::snapshot::inspection::handle(s))
        || r.revision > i64::MAX as u64
    {
        return Err(Denial::Arguments);
    }
    let valid = match c {
        Command::WorkRank => a
            .get("rank")
            .is_some_and(|v| v.is_null() || v.as_i64().is_some_and(|n| (1..=10_000).contains(&n))),
        Command::WorkEnqueue => a.get("brief").is_none_or(|v| {
            v.as_str()
                .is_some_and(|s| !s.trim().is_empty() && s.len() <= 16_384)
        }),
        Command::WorkDefer => wait_shape(&a["wait"]) && a.get("offer_id").is_none_or(handle),
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(Denial::Arguments)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    schema: String,
    result_id: String,
    operation: String,
    ship: String,
    task_id: String,
    applied: bool,
    provenance: Value,
}
impl Reply {
    fn validate(&self, command: Command, a: &Value, c: &DelegatedContext) -> Result<()> {
        ensure!(
            self.schema == "work.action.v1"
                && self.operation == name(command)
                && self.applied
                && self.ship == a["ship"]
                && self.task_id == a["reference"]["task_id"]
                && c.has("work:manage")
                && c.allows(&self.ship)
                && uuid::Uuid::parse_str(&self.result_id).is_ok(),
            Denial::Ship
        );
        // Only core-owned identity fields, no peer-supplied authored text.
        let mut expected = c.provenance();
        expected["result_id"] = json!(self.result_id);
        ensure!(self.provenance == expected, Denial::Arguments);
        Ok(())
    }
}

pub(super) fn request(command: Command, a: &Value, c: &DelegatedContext) -> Result<Value> {
    validate_args(command, a)?;
    ensure!(c.has("work:manage"), Denial::Scope);
    let ship = a["ship"].as_str().ok_or(Denial::Arguments)?;
    ensure!(c.allows(ship), Denial::Ship);
    let conn = super::runtime::registry_readonly()?;
    let local: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM ships WHERE name=?1 AND retired_ms IS NULL)",
        [ship],
        |r| r.get(0),
    )?;
    if local {
        return local_apply(&conn, command, a, c);
    }
    let machine: Option<String> = conn
        .query_row(
            "SELECT machine FROM machine_ships WHERE name=?1 ORDER BY machine LIMIT 1",
            [ship],
            |r| r.get(0),
        )
        .optional()?;
    let machine = machine.ok_or(Denial::Ship)?;
    let callable: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM machines WHERE name=?1 AND role='collects')",
        [&machine],
        |r| r.get(0),
    )?;
    ensure!(callable, Denial::LinkedUnavailable);
    let value = crate::machines::work_action_of(
        &machine,
        &json!({"context":c.gateway(),"operation":name(command),"args":a}),
    )?;
    if command == Command::JobCancel {
        cancel::validate_reply(&value, a, c).map_err(|_| Denial::OutcomeUnknown)?;
        return Ok(value);
    }
    if command == Command::JobRetry {
        retry::validate_reply(&value, a, c).map_err(|_| Denial::OutcomeUnknown)?;
        return Ok(value);
    }
    let reply: Reply = serde_json::from_value(value).map_err(|_| Denial::OutcomeUnknown)?;
    reply
        .validate(command, a, c)
        .map_err(|_| Denial::OutcomeUnknown)?;
    serde_json::to_value(reply).map_err(|_| Denial::OutcomeUnknown.into())
}

/// Authenticated linked-operator entry only; the external v2 parser never
/// admits this command or a caller-supplied delegated context. No recursion.
pub(crate) fn serve(body: &Value) -> Result<Value> {
    ensure!(
        keys(
            body,
            &["context", "operation", "args"],
            &["context", "operation", "args"]
        ),
        Denial::Arguments
    );
    let c = DelegatedContext::from_gateway(&body["context"])?;
    if body["operation"] == "capabilities" {
        ensure!(c.has("work:manage"), Denial::Scope);
        ensure!(
            keys(&body["args"], &["ship"], &["ship"]) && handle(&body["args"]["ship"]),
            Denial::Arguments
        );
        ensure!(
            c.allows(body["args"]["ship"].as_str().ok_or(Denial::Arguments)?),
            Denial::Ship
        );
        return Ok(
            json!({"schema":"work.action.capabilities.v1","operations":["work_rank","work_enqueue","work_defer","job_cancel","job_retry"]}),
        );
    }
    let command = parse(body["operation"].as_str().ok_or(Denial::Arguments)?)?;
    validate_args(command, &body["args"])?;
    ensure!(c.has("work:manage"), Denial::Scope);
    local_apply(&crate::fleet::registry()?, command, &body["args"], &c)
}

fn visible_task(store: &Store, id: &str, c: &DelegatedContext) -> Result<Value> {
    let owner: Option<String> = store
        .connection()
        .query_row("SELECT owner FROM tasks WHERE id=?1", [id], |r| r.get(0))
        .optional()?;
    ensure!(owner.as_deref().is_some_and(|s| c.allows(s)), Denial::Ship);
    store.task(id)
}
// Validate visibility before letting the native code walk a dependency chain.
// Native validation/cycle detection still decides whether the wait is legal.
fn visible_wait(store: &Store, w: &Value, c: &DelegatedContext) -> Result<()> {
    match w["kind"].as_str() {
        Some("task") => {
            let mut next = w["ref"].as_str().ok_or(Denial::Ship)?.to_owned();
            let mut seen = HashSet::new();
            for _ in 0..128 {
                if !seen.insert(next.clone()) {
                    return Ok(());
                }
                visible_task(store, &next, c)?;
                let raw: Option<String> = store.connection().query_row("SELECT record FROM task_schedule WHERE task_id=?1 AND length(CAST(record AS BLOB))<=65536",[&next],|r|r.get(0)).optional()?;
                let Some(raw) = raw else {
                    return Ok(());
                };
                let schedule: Value = serde_json::from_str(&raw)?;
                if schedule["wait"]["kind"] != "task" {
                    // A task chain can end at a request or remote dependency;
                    // that leaf is still a referenced wait under this grant.
                    return visible_wait(store, &schedule["wait"], c);
                }
                next = schedule["wait"]["ref"].as_str().ok_or(Denial::Ship)?.into();
            }
            return Err(Denial::Ship.into());
        }
        Some("request") => {
            let m = crate::mail::find(store, w["ref"].as_str().ok_or(Denial::Ship)?)?
                .ok_or(Denial::Ship)?;
            for field in ["from", "to"] {
                ensure!(
                    m[field]
                        .as_str()
                        .is_some_and(|s| s == crate::mail::CAPTAIN || c.allows(s)),
                    Denial::Ship
                );
            }
            if let Some(id) = m["task_id"].as_str() {
                visible_task(store, id, c)?;
            }
        }
        Some("remote") => ensure!(
            w["ref"]["ship"].as_str().is_some_and(|s| c.allows(s)),
            Denial::Ship
        ),
        _ => {}
    }
    Ok(())
}
fn visible_ancestry(store: &Store, id: &str, c: &DelegatedContext) -> Result<()> {
    let mut next = Some(id.to_owned());
    let mut seen = HashSet::new();
    while let Some(id) = next {
        ensure!(seen.len() < 128 && seen.insert(id.clone()), Denial::Ship);
        let task = visible_task(store, &id, c)?;
        next = task["parent"].as_str().map(str::to_owned);
    }
    Ok(())
}
fn local_apply(
    conn: &rusqlite::Connection,
    command: Command,
    a: &Value,
    c: &DelegatedContext,
) -> Result<Value> {
    validate_args(command, a)?;
    ensure!(c.has("work:manage"), Denial::Scope);
    let ship = a["ship"].as_str().ok_or(Denial::Arguments)?;
    ensure!(c.allows(ship), Denial::Ship);
    let home: Option<String> = conn
        .query_row(
            "SELECT home FROM ships WHERE name=?1 AND retired_ms IS NULL",
            [ship],
            |r| r.get(0),
        )
        .optional()?;
    let home = home.ok_or(Denial::Ship)?;
    let path = std::path::Path::new(&home).join("privateer.sqlite");
    crate::store::reject_symlinks(&path)?;
    use std::os::unix::fs::MetadataExt;
    let before = std::fs::metadata(&path)?;
    // A denied action must not initialize/migrate/repair/chmod its target.
    // Reuse the existing captain-action open and require the complete V6a
    // schema marker below; normal operator startup owns migrations.
    let store = Store::open_existing_action(std::path::Path::new(&home), Caller::Operator)?;
    let after = std::fs::metadata(&path)?;
    ensure!(
        (before.dev(), before.ino()) == (after.dev(), after.ino()),
        Denial::Ship
    );
    apply(&store, command, a, c)
}
fn apply(store: &Store, command: Command, a: &Value, c: &DelegatedContext) -> Result<Value> {
    apply_at(store, command, a, c, Store::now_ms())
}
fn apply_at(
    store: &Store,
    command: Command,
    a: &Value,
    c: &DelegatedContext,
    now: i64,
) -> Result<Value> {
    if command == Command::JobCancel {
        return cancel::apply_at(store, a, c, now);
    }
    if command == Command::JobRetry {
        return retry::apply_at(store, a, c, now);
    }
    // All validation, native effects, audit and durable result share the SAME
    // write lock/commit. There is no committed-but-unrecorded mutation window.
    let tx = store.write_transaction()?;
    let wanted: TaskRef = serde_json::from_value(a["reference"].clone())?;
    ensure!(c.has("work:manage"), Denial::Scope);
    ensure!(
        c.allows(&wanted.ship) && crate::snapshot::inspection::owned(store, &wanted.ship)?,
        Denial::Ship
    );
    ensure!(
        crate::snapshot::inspection::origin(store)? == wanted.origin,
        Denial::Ship
    );
    let supported: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='work_action_schema' AND value='2')",
        [],
        |r| r.get(0),
    )?;
    ensure!(supported, Denial::Ship);
    // Result recovery precedes task/dependency existence and visibility checks.
    // This is only the already-authorized acknowledgment, never a fresh read.
    // Current grant/resource/authority and home incarnation remain bound.
    tx.execute(
        "DELETE FROM door_work_results WHERE created_ms < ?1",
        [now - super::idempotency::RETENTION_MS],
    )?;
    let key = crate::core::hash(&a["idempotency_key"]);
    let hash = crate::core::hash(&json!([
        name(command),
        a,
        c.capabilities(),
        c.visibility_revision()
    ]));
    let previous: Option<(String,String)> = tx.query_row("SELECT args_hash,result FROM door_work_results WHERE grant_id=?1 AND resource=?2 AND key_hash=?3",params![c.client(),c.resource(),key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((previous_hash, result)) = previous {
        ensure!(previous_hash == hash, Denial::Arguments);
        let reply: Reply = serde_json::from_str(&result)?;
        reply.validate(command, a, c)?;
        tx.commit()?;
        return Ok(serde_json::to_value(reply)?);
    }
    let current = references::get(store, &wanted.ship, &wanted.task_id)?.ok_or(Denial::Ship)?;
    ensure!(current == wanted, Denial::Ship);
    visible_ancestry(store, &wanted.task_id, c)?;
    if matches!(command, Command::WorkEnqueue | Command::WorkDefer) {
        // Native live-job admission walks descendants. Refuse an unobservable
        // tree before its jobs/lifecycle can become an inference channel.
        let mut stmt = tx.prepare("WITH RECURSIVE tree(id) AS (SELECT ?1 UNION SELECT t.id FROM tasks t JOIN tree ON t.parent=tree.id) SELECT t.owner FROM tasks t JOIN tree ON tree.id=t.id LIMIT 2001")?;
        let owners = stmt
            .query_map([&wanted.task_id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(
            owners.len() <= 2000 && owners.iter().all(|owner| c.allows(owner)),
            Denial::Ship
        );
    }
    if command == Command::WorkDefer {
        visible_wait(store, &a["wait"], c)?;
    }
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM task_schedule WHERE task_id=?1",
            [&wanted.task_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(raw) = raw {
        ensure!(raw.len() <= 65536, Denial::ExecutionFailed);
        let old: Value = serde_json::from_str(&raw)?;
        visible_wait(store, &old["wait"], c)?;
    }
    let (total, own): (u64, u64) = tx.query_row(
        "SELECT count(*),COALESCE(sum(grant_id=?1),0) FROM door_work_results",
        [c.client()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    // Only completed results older than the retry window are pruned, under
    // this same IMMEDIATE transaction. In-window saturation fails closed.
    ensure!(total < 4096 && own < 256, Denial::RetryCapacity);
    let mut native = json!({"id":wanted.task_id});
    match command {
        Command::WorkRank => {
            native["rank"] = a["rank"].clone();
        }
        Command::WorkEnqueue => {
            if let Some(v) = a.get("brief") {
                native["brief"] = v.clone();
            }
        }
        Command::WorkDefer => {
            native["wait"] = a["wait"].clone();
            if let Some(v) = a.get("offer_id") {
                native["offer_id"] = v.clone();
            }
            // Native crew parking may defer its own executing work. An external
            // scheduler cannot take that custody from a live/uncertain payload.
            ensure!(
                !crate::scheduling::live_jobs(store, &wanted.task_id)?,
                Denial::ExecutionFailed
            );
        }
        _ => return Err(Denial::Arguments.into()),
    }
    store
        .with_delegated_audit(c, || -> Result<()> {
            match command {
                Command::WorkRank => {
                    crate::core::task_rank(store, &native)?;
                }
                Command::WorkEnqueue => {
                    crate::scheduling::enqueue_in(store, &native)?;
                }
                Command::WorkDefer => {
                    crate::scheduling::defer_in(store, &native)?;
                }
                _ => unreachable!("fixed work action"),
            }
            Ok(())
        })
        .map_err(|_| Denial::ExecutionFailed)?;
    let result_id = c.provenance()["result_id"]
        .as_str()
        .ok_or(Denial::Arguments)?
        .to_owned();
    let reply = Reply {
        schema: "work.action.v1".into(),
        result_id: result_id.clone(),
        operation: name(command).into(),
        ship: wanted.ship.clone(),
        task_id: wanted.task_id.clone(),
        applied: true,
        provenance: c.provenance(),
    };
    let result = serde_json::to_string(&reply)?;
    tx.execute(
        "INSERT INTO door_work_results(grant_id,resource,key_hash,args_hash,result,created_ms) VALUES(?1,?2,?3,?4,?5,?6)",
        params![c.client(), c.resource(), key, hash, result, now],
    )?;
    store.with_delegated_audit(c, || store.audit("door.work", &json!({"tool":name(command),"reference":wanted,"result_id":result_id,"idempotency_key_hash":key,"result":"applied"})))?;
    let value = serde_json::to_value(reply)?;
    ensure!(
        crate::snapshot::inspection::origin(store)? == wanted.origin,
        Denial::Ship
    );
    // A commit I/O failure is uncertain, never a pre-effect denial. The result
    // row shares this commit, so a same-key retry can recover it if committed.
    tx.commit().map_err(|_| Denial::OutcomeUnknown)?;
    Ok(value)
}

mod cancel;
mod retry;

#[cfg(test)]
mod tests;
