//! Guarded retry: native admission and an uncertain result commit before launch.
//! Recovery never consumes a second prepared launch, even after a crash.
use super::*;
use crate::{
    jobs,
    snapshot::inspection::{self, JobRef},
};

pub(super) use super::cancel::validate_args;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    schema: String,
    result_id: String,
    operation: String,
    ship: String,
    task_id: String,
    job_id: String,
    retry_job_id: String,
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
            && r.operation == "job_retry"
            && r.applied
            && r.ship == a["ship"]
            && r.task_id == a["reference"]["task_id"]
            && r.job_id == a["reference"]["id"]
            && inspection::handle(&r.retry_job_id)
            && r.retry_job_id != r.job_id
            && matches!(r.outcome.as_str(), "launch_requested" | "outcome_unknown")
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

// Must run under the home IMMEDIATE lock, both before workspace preparation
// and again before admission. Recovery precedes fresh visibility and eligibility.
fn lookup(
    store: &Store,
    a: &Value,
    c: &DelegatedContext,
    now: i64,
    key: &str,
    hash: &str,
) -> Result<Option<Value>> {
    let wanted: JobRef = serde_json::from_value(a["reference"].clone())?;
    ensure!(
        c.allows(&wanted.ship)
            && inspection::owned(store, &wanted.ship)?
            && inspection::origin(store)? == wanted.origin,
        Denial::Ship
    );
    let supported: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='work_action_schema' AND value='2')",
        [],
        |r| r.get(0),
    )?;
    ensure!(supported, Denial::Ship);
    store.connection().execute(
        "DELETE FROM door_work_results WHERE created_ms < ?1",
        [now - super::super::idempotency::RETENTION_MS],
    )?;
    let old: Option<(String,String)> = store.connection().query_row("SELECT args_hash,result FROM door_work_results WHERE grant_id=?1 AND resource=?2 AND key_hash=?3", params![c.client(),c.resource(),key], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((old_hash, result)) = old {
        ensure!(old_hash == hash, Denial::Arguments);
        let v = serde_json::from_str(&result)?;
        validate_reply(&v, a, c)?;
        return Ok(Some(v));
    }
    cancel::visible_graph(store, &wanted, c)?;
    let (total, own): (u64, u64) = store.connection().query_row(
        "SELECT count(*),COALESCE(sum(grant_id=?1),0) FROM door_work_results",
        [c.client()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(total < 4096 && own < 256, Denial::RetryCapacity);
    Ok(None)
}

pub(super) fn apply_at(store: &Store, a: &Value, c: &DelegatedContext, now: i64) -> Result<Value> {
    apply_with(store, a, c, now, |p| jobs::launch_start(store, p))
}
fn apply_with(
    store: &Store,
    a: &Value,
    c: &DelegatedContext,
    now: i64,
    launch: impl FnOnce(jobs::PreparedStart) -> Result<Value>,
) -> Result<Value> {
    apply_phases(store, a, c, now, || Ok(()), launch)
}
fn apply_phases(
    store: &Store,
    a: &Value,
    c: &DelegatedContext,
    now: i64,
    before_admit: impl FnOnce() -> Result<()>,
    launch: impl FnOnce(jobs::PreparedStart) -> Result<Value>,
) -> Result<Value> {
    validate_args(a)?;
    ensure!(c.has("work:manage"), Denial::Scope);
    let wanted: JobRef = serde_json::from_value(a["reference"].clone())?;
    let key = crate::core::hash(&a["idempotency_key"]);
    let hash = crate::core::hash(&json!([
        "job_retry",
        a,
        c.capabilities(),
        c.visibility_revision()
    ]));
    let native_args = json!({"id":wanted.id,"new_id":uuid::Uuid::new_v4().to_string()});
    let tx = store.write_transaction()?;
    if let Some(v) = lookup(store, a, c, now, &key, &hash)? {
        tx.commit()?;
        return recover(v, a, c);
    }
    let request = jobs::retry::request(store, &native_args).map_err(|_| Denial::ExecutionFailed)?;
    tx.commit()?;
    // Workspace preparation may do native filesystem work, but cannot spawn or
    // reserve capacity. Do not hold a home lock while taking a workspace lock.
    let mut prepared = match jobs::prepare_start(
        store,
        request.args.clone(),
        Some(request.old_id.clone()),
        request.agent,
    )
    .map_err(|_| Denial::ExecutionFailed)?
    {
        jobs::StartPreparation::Ready(p) => *p,
        jobs::StartPreparation::Replay(_) => return Err(Denial::OutcomeUnknown.into()),
    };
    before_admit()?;
    let tx = store.write_transaction()?;
    if let Some(v) = lookup(store, a, c, now, &key, &hash)? {
        tx.commit()?;
        return recover(v, a, c);
    }
    let fresh = jobs::retry::request(store, &native_args).map_err(|_| Denial::ExecutionFailed)?;
    ensure!(
        fresh.args == request.args && fresh.agent == request.agent,
        Denial::ExecutionFailed
    );
    jobs::retry::revalidate_prepared(store, &prepared).map_err(|_| Denial::ExecutionFailed)?;
    let mut reply = Reply {
        schema: "work.action.v1".into(),
        result_id: c.provenance()["result_id"]
            .as_str()
            .ok_or(Denial::Arguments)?
            .into(),
        operation: "job_retry".into(),
        ship: wanted.ship.clone(),
        task_id: wanted.task_id.clone(),
        job_id: wanted.id.clone(),
        retry_job_id: native_args["new_id"]
            .as_str()
            .ok_or(Denial::Arguments)?
            .into(),
        applied: true,
        outcome: "outcome_unknown".into(),
        provenance: c.provenance(),
    };
    let admitted = (|| -> Result<()> {
        ensure!(
            jobs::admit_start_in(store, &tx, &mut prepared)?.is_none(),
            Denial::OutcomeUnknown
        );
        tx.execute("INSERT INTO door_work_results(grant_id,resource,key_hash,args_hash,result,created_ms) VALUES(?1,?2,?3,?4,?5,?6)", params![c.client(),c.resource(),key,hash,serde_json::to_string(&reply)?,now])?;
        store.with_delegated_audit(c, || store.audit("door.work", &json!({"tool":"job_retry","reference":wanted,"retry_job_id":reply.retry_job_id,"result_id":reply.result_id,"idempotency_key_hash":key,"result":"retry_intent"})))?;
        ensure!(inspection::origin(store)? == wanted.origin, Denial::Ship);
        Ok(())
    })();
    if let Err(error) = admitted {
        tx.rollback()?;
        // Explicit compensation includes light/demandless reservations, which
        // stale_reservations does not enumerate. Never rely on that inspection.
        jobs::compensate_start(store, &prepared).map_err(|_| Denial::OutcomeUnknown)?;
        return Err(error
            .downcast::<Denial>()
            .unwrap_or(Denial::ExecutionFailed)
            .into());
    }
    if tx.commit().is_err() {
        // A possibly committed admission must retain custody. Only compensate
        // when this connection proves that its new home row was rolled back.
        let exists: bool = store
            .connection()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM jobs WHERE id=?1)",
                [&reply.retry_job_id],
                |r| r.get(0),
            )
            .map_err(|_| Denial::OutcomeUnknown)?;
        if !exists {
            jobs::compensate_start(store, &prepared).map_err(|_| Denial::OutcomeUnknown)?;
        }
        return Err(Denial::OutcomeUnknown.into());
    }
    store
        .with_delegated_audit(c, || launch(prepared))
        .map_err(|_| Denial::OutcomeUnknown)?;
    reply.outcome = "launch_requested".into();
    (|| -> Result<Value> {
        let tx = store.write_transaction()?;
        ensure!(inspection::origin(store)? == wanted.origin, Denial::OutcomeUnknown);
        let updated = tx.execute("UPDATE door_work_results SET result=?4 WHERE grant_id=?1 AND resource=?2 AND key_hash=?3 AND args_hash=?5 AND json_extract(result,'$.result_id')=?6", params![c.client(),c.resource(),key,serde_json::to_string(&reply)?,hash,reply.result_id])?;
        ensure!(updated == 1, Denial::OutcomeUnknown);
        store.with_delegated_audit(c, || store.audit("door.work", &json!({"tool":"job_retry","reference":wanted,"retry_job_id":reply.retry_job_id,"result_id":reply.result_id,"idempotency_key_hash":key,"result":reply.outcome})))?;
        tx.commit()?;
        Ok(serde_json::to_value(reply)?)
    })().map_err(|_| Denial::OutcomeUnknown.into())
}

#[cfg(test)]
mod tests;
