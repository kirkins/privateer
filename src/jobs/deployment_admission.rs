//! Dormant host-admission/custody bridge. No job, grant use, command, adapter or
//! process is created here. The future dispatcher must admit BEFORE consuming a use.
//! A dropped token deliberately retains capacity: a vanished owner does not
//! prove that an unrecorded payload never started. Captain-only resource.recover
//! can explicitly release pre-handoff holds after inspection, even if their
//! admitting process lives. Handoff/payload custody is never released here.
use super::*;

/// Process-local, nonserializable pre-launch custody. No Drop-based release.
/// Only admit can create this token outside this module's tests. A future
/// executor must consume it through handoff before spawning anything;
/// release_unlaunched is only compensation while that handoff has NOT occurred.
#[derive(Debug)]
pub(crate) struct Admitted {
    conn: Connection,
    reservation: Reservation,
    record: String,
}
/// Process-local custody after a committed host handoff, NOT upload authority.
/// Only Admitted::handoff creates this token; no status/JSON reconstruction or
/// Drop-based release. The future sole spawn method must consume this token,
/// CAS its exact record again and bind a fresh claim before releasing authority
/// bytes. No spawn method exists in this slice.
#[derive(Debug)]
pub(crate) struct Handoff {
    conn: Connection,
    reservation: Reservation,
    record: String,
}

/// A failure before the host writer lock was acquired returns custody for
/// explicit retry/compensation. Once a write is attempted, no reusable token is
/// returned, even if rollback succeeds: callers must inspect the durable hold.
#[derive(Debug)]
pub(crate) struct HandoffError {
    cause: anyhow::Error,
    admitted: Option<Box<Admitted>>,
}

impl HandoffError {
    pub(crate) fn into_admitted(self) -> Option<Admitted> {
        self.admitted.map(|token| *token)
    }
}

impl std::fmt::Display for HandoffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "deployment handoff failed: {}", self.cause)
    }
}

impl std::error::Error for HandoffError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

impl Admitted {
    pub(crate) fn key(&self) -> &str {
        &self.reservation.key
    }

    /// Irreversible here: record ambiguous launch custody before any future
    /// spawn. Recovery winning the host writer race prevents token creation;
    /// handoff winning prevents pre-handoff recovery. This is host custody only,
    /// not a grant/claim transition or cross-database atomicity guarantee.
    /// BEGIN failure returns unchanged Admitted custody, never spawn authority.
    pub(crate) fn handoff(mut self) -> std::result::Result<Handoff, HandoffError> {
        if let Err(cause) = self.conn.execute_batch("BEGIN IMMEDIATE") {
            // A failed BEGIN on this private, autocommit connection wrote
            // nothing. Do not mutate the in-memory reservation before this.
            return Err(HandoffError {
                cause: cause.into(),
                admitted: Some(Box::new(self)),
            });
        }
        let result = (|| -> Result<String> {
            self.reservation.state = "deployment-handoff".into();
            let record = serde_json::to_string(&self.reservation)?;
            ensure!(
                self.conn.execute(
                    "UPDATE reservations SET record=?3 WHERE key=?1 AND record=?2",
                    params![self.reservation.key, self.record, record],
                )? == 1,
                "deployment reservation custody changed; handoff refused"
            );
            self.conn.execute_batch("COMMIT")?;
            Ok(record)
        })();
        match result {
            Ok(record) => Ok(Handoff {
                conn: self.conn,
                reservation: self.reservation,
                record,
            }),
            Err(cause) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(HandoffError {
                    cause,
                    admitted: None,
                })
            }
        }
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
        CapacityReading::measure,
    )
}

// Private observation seam: tests and live admission share ALL reserve args.
// No caller can inject a reserve bypass into admit.
fn admit_with(
    store: &Store,
    task: &str,
    estimate: Estimate,
    timeout_secs: u64,
    measure: impl FnOnce(&Path) -> Result<CapacityReading>,
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
    let timeout_secs = timeout_secs.min(charter["job_timeout_secs"].as_u64().unwrap_or(900));
    ensure!(
        timeout_secs > 0,
        "deployment charter timeout must be positive"
    );
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
    reserve_observed(
        &conn,
        &mut reservation,
        Some(&estimate),
        true,
        charter["max_heavy_jobs"].as_u64(),
        (
            DiskFloor {
                path: store.home(),
                charter_min_free_bytes: charter["min_free_bytes"]
                    .as_u64()
                    .unwrap_or(2 * 1024 * 1024 * 1024),
            },
            false,
        ),
        measure,
    )?;
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

/// Explicit, operator-only compensation for a PRE-handoff deployment hold.
/// Unlike an ordinary job, its supervisor is merely the admitting process and
/// may still be alive after losing the token. This is manual capacity recovery,
/// NOT proof of payload absence. Force acknowledges inspection, not permission
/// to release a handoff or payload. Future executors must CAS the original hold
/// before handing off; recovery winning that race must prevent their launch.
/// The host ledger owns both this deletion and its atomic, bounded audit.
pub(super) fn recover_reserved(
    store: &Store,
    conn: &Connection,
    raw: &str,
    held: &Reservation,
    force: bool,
) -> Result<Value> {
    ensure!(
        matches!(store.caller(), Caller::Operator),
        "deployment recovery is operator-only"
    );
    ensure!(
        held.home == store.home()
            && held.key == reservation_key(store.home(), &held.job_id, &held.incarnation)
            && held.job_id.starts_with("deployment-")
            && held.state == "deployment-reserved"
            && held.payload.is_none()
            && held.escaped.is_empty()
            && held.model_route.is_none(),
        "deployment handoff or payload custody cannot be released here"
    );
    ensure!(
        Store::now_ms().saturating_sub(held.created_ms) > START_GRACE_MS,
        "reservation is still within its start grace period"
    );
    ensure!(
        force,
        "inspect for unrecorded deployment payloads, then pass force:true; payload absence is not proven"
    );
    // Same lock order as admission/future handoff. Do not recover a job-owned
    // hold; the native job cancellation/recovery path must retain that custody.
    let home = store.write_transaction()?;
    ensure!(
        !home.query_row(
            "SELECT EXISTS(SELECT 1 FROM jobs WHERE id=?1)",
            [&held.job_id],
            |r| r.get::<_, bool>(0)
        )?,
        "deployment reservation has a job record; capacity retained"
    );
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    ensure!(
        tx.execute(
            "DELETE FROM reservations WHERE key=?1 AND record=?2",
            params![held.key, raw]
        )? == 1,
        "deployment reservation custody changed; capacity retained"
    );
    let audit = json!({"event":"deployment_reserved_released","by":"operator",
        "reservation":held,"forced":true,"payload_absence_proven":false});
    tx.execute(
        "INSERT INTO resource_recovery_audit(created_ms,record) VALUES(?1,?2)",
        params![Store::now_ms(), audit.to_string()],
    )?;
    tx.execute("DELETE FROM resource_recovery_audit WHERE seq NOT IN (SELECT seq FROM resource_recovery_audit ORDER BY seq DESC LIMIT ?1)",
        [admission::resource_actions::AUDIT_KEEP])?;
    // No home writes: unlock first; the host CAS still serializes handoff.
    home.commit()?;
    tx.commit()?;
    Ok(
        json!({"released":held.key,"job_id":held.job_id,"forced":true,
        "payload_absence_proven":false,"audit":"resource_recovery_audit"}),
    )
}

#[cfg(test)]
mod tests;
