//! Exact cancellation is a durable intent, not a rollbackable process signal.
//! The intent, uncertain recovery result and delegated audit commit together.
//! Native cancellation then runs outside that transaction; a crash/error never
//! resends signals on replay or claims a payload stopped. The native supervisor
//! may independently honor the durable request. No retry/launch path is exposed.
use super::*;
use crate::snapshot::inspection::{self, JobRef};

pub(super) fn validate_args(a: &Value) -> Result<(), Denial> {
    if !keys(
        a,
        &["ship", "reference", "idempotency_key"],
        &["ship", "reference", "idempotency_key"],
    ) || !handle(&a["ship"])
        || a["ship"].as_str().is_none_or(|s| s.len() > 128)
        || a["idempotency_key"].as_str().is_none_or(|s| {
            s.is_empty() || s.len() > 128 || !s.bytes().all(|b| (0x20..=0x7e).contains(&b))
        })
    {
        return Err(Denial::Arguments);
    }
    let r: JobRef =
        serde_json::from_value(a["reference"].clone()).map_err(|_| Denial::Arguments)?;
    if r.ship != a["ship"]
        || [&r.origin, &r.ship, &r.task_id, &r.id, &r.incarnation]
            .iter()
            .any(|s| !inspection::handle(s))
    {
        return Err(Denial::Arguments);
    }
    Ok(())
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    schema: String,
    result_id: String,
    operation: String,
    ship: String,
    task_id: String,
    job_id: String,
    // Applied means the durable request, not that the payload stopped.
    applied: bool,
    outcome: String,
    provenance: Value,
}
pub(super) fn validate_reply(v: &Value, a: &Value, c: &DelegatedContext) -> Result<()> {
    let r: Reply = serde_json::from_value(v.clone())?;
    let mut expected = c.provenance();
    expected["result_id"] = json!(r.result_id);
    ensure!(
        r.schema == "work.action.v1"
            && r.operation == "job_cancel"
            && r.applied
            && r.ship == a["ship"]
            && r.task_id == a["reference"]["task_id"]
            && r.job_id == a["reference"]["id"]
            && matches!(
                r.outcome.as_str(),
                "cancel_requested" | "stopped" | "outcome_unknown"
            )
            && uuid::Uuid::parse_str(&r.result_id).is_ok()
            && r.provenance == expected
            && c.has("work:manage")
            && c.allows(&r.ship),
        Denial::Ship
    );
    Ok(())
}
fn recover(v: Value, a: &Value, c: &DelegatedContext) -> Result<Value> {
    validate_reply(&v, a, c)?;
    ensure!(v["outcome"] != "outcome_unknown", Denial::OutcomeUnknown);
    Ok(v)
}

// Native parked cancellation can cancel its child; lineage and task ancestry
// are authority-bearing references even when they are not returned in the ack.
// Check both directions (including resumed-as) before setting any intent.
fn visible_graph(store: &Store, wanted: &JobRef, c: &DelegatedContext) -> Result<()> {
    inspection::exact_record(store, &wanted.ship, wanted, c)?;
    let mut pending = vec![wanted.id.clone()];
    let mut seen = HashSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        ensure!(seen.len() <= 128, Denial::Ship);
        let row: Option<(String,String)> = store.connection().query_row(
            "SELECT j.record,t.owner FROM jobs j JOIN tasks t ON t.id=json_extract(j.record,'$.task_id') WHERE j.id=?1 AND length(CAST(j.record AS BLOB))<=65536",
            [&id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let (raw, owner) = row.ok_or(Denial::Ship)?;
        ensure!(c.allows(&owner), Denial::Ship);
        let raw: Value = serde_json::from_str(&raw).map_err(|_| Denial::Ship)?;
        ensure!(inspection::worker_visible(store, &raw, c)?, Denial::Ship);
        visible_ancestry(store, raw["task_id"].as_str().ok_or(Denial::Ship)?, c)?;
        for field in ["started_by", "retry_of", "resume_of", "relaunch_of"] {
            if let Some(other) = raw[field].as_str() {
                ensure!(inspection::handle(other), Denial::Ship);
                pending.push(other.into());
            }
        }
        let mut stmt = store.connection().prepare("SELECT id FROM jobs WHERE json_extract(record,'$.started_by')=?1 OR json_extract(record,'$.retry_of')=?1 OR json_extract(record,'$.resume_of')=?1 OR json_extract(record,'$.relaunch_of')=?1 LIMIT 129")?;
        let related = stmt
            .query_map([&id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(related.len() <= 128, Denial::Ship);
        pending.extend(related);
        let mut stmt = store.connection().prepare("SELECT crew_job_id,child_job_id,resumed_job_id,task_id,ship FROM crew_parks WHERE crew_job_id=?1 OR child_job_id=?1 OR resumed_job_id=?1 OR id=?2 LIMIT 129")?;
        let parks = stmt
            .query_map(params![id, raw["park"].as_str().unwrap_or_default()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(parks.len() <= 128, Denial::Ship);
        for (crew, child, resumed, task, ship) in parks {
            ensure!(c.allows(&ship), Denial::Ship);
            visible_ancestry(store, &task, c)?;
            pending.extend([crew, child]);
            pending.extend(resumed);
        }
        ensure!(pending.len() <= 1024, Denial::Ship);
    }
    Ok(())
}

pub(super) fn apply_at(store: &Store, a: &Value, c: &DelegatedContext, now: i64) -> Result<Value> {
    apply_with(store, a, c, now, || {
        crate::jobs::cancel_exact(
            store,
            a["reference"]["id"].as_str().ok_or(Denial::Arguments)?,
            a["reference"]["incarnation"]
                .as_str()
                .ok_or(Denial::Arguments)?,
        )
    })
}
// Injectable boundary for crash, commit-loss and custody-race fixtures. The
// native effect is not inside a rollbackable SQLite transaction.
fn apply_with(
    store: &Store,
    a: &Value,
    c: &DelegatedContext,
    now: i64,
    native: impl FnOnce() -> Result<Value>,
) -> Result<Value> {
    validate_args(a)?;
    ensure!(c.has("work:manage"), Denial::Scope);
    let wanted: JobRef = serde_json::from_value(a["reference"].clone())?;
    let tx = store.write_transaction()?;
    ensure!(
        c.allows(&wanted.ship)
            && inspection::owned(store, &wanted.ship)?
            && inspection::origin(store)? == wanted.origin,
        Denial::Ship
    );
    let supported: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='work_action_schema' AND value='2')",
        [],
        |r| r.get(0),
    )?;
    ensure!(supported, Denial::Ship);
    tx.execute(
        "DELETE FROM door_work_results WHERE created_ms < ?1",
        [now - super::super::idempotency::RETENTION_MS],
    )?;
    let key = crate::core::hash(&a["idempotency_key"]);
    let hash = crate::core::hash(&json!([
        "job_cancel",
        a,
        c.capabilities(),
        c.visibility_revision()
    ]));
    let old: Option<(String,String)> = tx.query_row("SELECT args_hash,result FROM door_work_results WHERE grant_id=?1 AND resource=?2 AND key_hash=?3",params![c.client(),c.resource(),key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((old_hash, result)) = old {
        ensure!(old_hash == hash, Denial::Arguments);
        let value = serde_json::from_str(&result)?;
        validate_reply(&value, a, c)?;
        tx.commit()?;
        return recover(value, a, c);
    }
    visible_graph(store, &wanted, c)?;
    let (total, own): (u64, u64) = tx.query_row(
        "SELECT count(*),COALESCE(sum(grant_id=?1),0) FROM door_work_results",
        [c.client()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(total < 4096 && own < 256, Denial::RetryCapacity);
    let mut reply = Reply {
        schema: "work.action.v1".into(),
        result_id: c.provenance()["result_id"]
            .as_str()
            .ok_or(Denial::Arguments)?
            .into(),
        operation: "job_cancel".into(),
        ship: wanted.ship.clone(),
        task_id: wanted.task_id.clone(),
        job_id: wanted.id.clone(),
        applied: true,
        outcome: "outcome_unknown".into(),
        provenance: c.provenance(),
    };
    // A failed transaction cannot have sent a signal. Once committed, even an
    // error in native cancellation means uncertainty, never execution_failed.
    crate::jobs::request_cancel_in(store, &wanted.id, &wanted.incarnation)?;
    tx.execute("INSERT INTO door_work_results(grant_id,resource,key_hash,args_hash,result,created_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![c.client(),c.resource(),key,hash,serde_json::to_string(&reply)?,now])?;
    store.with_delegated_audit(c, || store.audit("door.work", &json!({"tool":"job_cancel","reference":wanted,"result_id":reply.result_id,"idempotency_key_hash":key,"result":"cancel_intent"})))?;
    ensure!(inspection::origin(store)? == wanted.origin, Denial::Ship);
    tx.commit().map_err(|_| Denial::OutcomeUnknown)?;
    let native = store
        .with_delegated_audit(c, native)
        .map_err(|_| Denial::OutcomeUnknown)?;
    reply.outcome = if matches!(
        native["status"].as_str(),
        Some("succeeded" | "failed" | "cancelled" | "timed_out" | "resumed")
    ) {
        "stopped"
    } else {
        "cancel_requested"
    }
    .into();
    let completed = (|| -> Result<Value> {
        let tx = store.write_transaction()?;
        ensure!(
            inspection::origin(store)? == wanted.origin,
            Denial::OutcomeUnknown
        );
        // A delayed caller must not overwrite a newer admission after its own
        // 24-hour recovery row expired, even if key/arguments were reused.
        let updated = tx.execute("UPDATE door_work_results SET result=?4 WHERE grant_id=?1 AND resource=?2 AND key_hash=?3 AND args_hash=?5 AND json_extract(result,'$.result_id')=?6",params![c.client(),c.resource(),key,serde_json::to_string(&reply)?,hash,reply.result_id])?;
        ensure!(updated == 1, Denial::OutcomeUnknown);
        store.with_delegated_audit(c, || store.audit("door.work", &json!({"tool":"job_cancel","reference":wanted,"result_id":reply.result_id,"idempotency_key_hash":key,"result":reply.outcome})))?;
        tx.commit()?;
        Ok(serde_json::to_value(reply)?)
    })();
    completed.map_err(|_| Denial::OutcomeUnknown.into())
}

#[cfg(test)]
mod tests;
