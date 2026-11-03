//! Dormant host-admission bridge. No job, grant use, command, adapter or process
//! is created here. The future dispatcher must admit BEFORE consuming a use.
//! A dropped token deliberately retains capacity: a vanished owner does not
//! prove that an unrecorded payload never started. Existing captain-only manual
//! resource.recover (with force for missing payload identity) remains available.
use super::*;

/// Process-local, nonserializable pre-launch custody. No Drop-based release.
/// Only admit can create this token outside this module's tests. A future
/// executor must consume it in a durable handoff before spawning anything;
/// release_unlaunched is only compensation while that handoff has NOT occurred.
#[derive(Debug)]
pub(crate) struct Admitted {
    conn: Connection,
    reservation: Reservation,
    record: String,
}
impl Admitted {
    pub(crate) fn key(&self) -> &str {
        &self.reservation.key
    }

    /// Explicit compensation, not proof of process death. The exact original
    /// reservation must still match; any payload/owner/state change refuses.
    pub(crate) fn release_unlaunched(self) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            ensure!(
                self.conn.execute(
                    "DELETE FROM reservations WHERE key=?1 AND record=?2",
                    params![self.key(), self.record],
                )? == 1,
                "deployment reservation custody changed; capacity retained"
            );
            Ok(())
        })();
        if result.is_ok() {
            self.conn.execute_batch("COMMIT")?;
        } else {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
        result
    }
}

/// Fail-fast native admission, never a queue wait holding deployment authority.
/// A capacity refusal consumes no grant use. No production admission is exposed.
/// Estimates describe the bounded future verifier/adapter and retained upload;
/// the caller must include incremental disk space, not just final artifact size.
pub(crate) fn admit(
    store: &Store,
    task: &str,
    estimate: Estimate,
    timeout_secs: u64,
) -> Result<Admitted> {
    admit_with(
        store,
        task,
        estimate,
        timeout_secs,
        |conn, reservation, estimate, charter| {
            reserve(
                conn,
                reservation,
                Some(estimate),
                true,
                charter["max_heavy_jobs"].as_u64(),
                DiskFloor {
                    path: store.home(),
                    charter_min_free_bytes: charter["min_free_bytes"]
                        .as_u64()
                        .unwrap_or(2 * 1024 * 1024 * 1024),
                },
                false,
            )
        },
    )
}

// Private test seam. No caller can inject a reserve bypass into admit.
fn admit_with(
    store: &Store,
    task: &str,
    estimate: Estimate,
    timeout_secs: u64,
    reserve: impl FnOnce(&Connection, &mut Reservation, &Estimate, &Value) -> Result<()>,
) -> Result<Admitted> {
    ensure!(
        matches!(store.caller(), Caller::Operator),
        "deployment admission is operator-only"
    );
    ensure!(
        store.connection().is_autocommit(),
        "deployment admission must precede consumption transaction"
    );
    ensure_dispatcher_namespace(store)?;
    crate::store::identifier(task)?;
    ensure!(
        (1..=3600).contains(&timeout_secs),
        "deployment timeout must be 1..3600 seconds"
    );
    let estimate = admission::estimate(&json!(estimate))?;
    ensure!(
        estimate.cpus.is_some() && estimate.memory_bytes.is_some() && estimate.disk_bytes.is_some(),
        "deployment requires explicit CPU, memory and incremental disk estimates"
    );
    let conn = shared_ledger(store)?;
    // Native lock order: home authority first, then the host ledger writer.
    // Return only after both commit. No cross-database atomicity is claimed:
    // if home commit fails, this function (which cannot launch) compensates;
    // if it crashes first, the durable hold remains for manual inspection.
    let tx = store.write_transaction()?;
    let charter = store.authorize(task, "deploy_nonproduction")?;
    let owner = store.task(task)?["owner"]
        .as_str()
        .context("deployment task owner missing")?
        .to_owned();
    crate::context::local_ship(store, &owner)?;
    let id = format!("deployment-{}", Uuid::new_v4());
    let incarnation = Uuid::new_v4().to_string();
    let mut reservation = Reservation {
        key: reservation_key(store.home(), &id, &incarnation),
        home: store.home().to_owned(),
        job_id: id,
        incarnation,
        heavy: true,
        state: "deployment-reserved".into(),
        created_ms: Store::now_ms(),
        supervisor: Some(
            identity(std::process::id())?.context("deployment admission owner unavailable")?,
        ),
        payload: None,
        model_route: None,
        route_first_mate: false,
        route_policy: None,
        route_predecessor: None,
        charter: Some(
            charter["id"]
                .as_str()
                .context("charter id missing")?
                .to_owned(),
        ),
        started_by: None,
        demand: None,
        disk_key: None,
        disk_free_bytes: None,
        disk_trimmed_at_admit: None,
        escaped: vec![],
        wake: false,
        timeout_ms: Some(timeout_secs as i64 * 1000),
        ship: Some(owner),
        work: None,
        task: Some(task.to_owned()),
    };
    reserve(&conn, &mut reservation, &estimate, &charter)?;
    let record = serde_json::to_string(&reservation)?;
    let admitted = Admitted {
        conn,
        reservation,
        record,
    };
    if let Err(error) = tx.commit() {
        admitted.release_unlaunched().context(
            "home commit failed; deployment admission compensation failed; inspect capacity",
        )?;
        return Err(error.into());
    }
    Ok(admitted)
}

#[cfg(test)]
mod tests;
