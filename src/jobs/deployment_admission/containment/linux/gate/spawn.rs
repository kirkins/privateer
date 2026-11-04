//! Sole dormant consuming spawn with bounded protocol IO. Capacity retained.
use super::*;
use crate::{deploy_grants::claims::Claim, jobs::deployment_admission::Handoff, store::Store};
use rusqlite::params;

/// Only this module constructs the witness for claim start/abort transitions.
/// It cannot be deserialized or reconstructed from durable custody JSON.
pub(crate) struct SpawnWitness(());

#[cfg(test)]
type ReleaseObserver = Box<dyn FnMut(&str)>;
#[cfg(test)]
thread_local! {
    pub(crate) static RELEASE_OBSERVER: std::cell::RefCell<Option<ReleaseObserver>> =
        const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
fn observe(stage: &str) {
    RELEASE_OBSERVER.with(|observer| {
        if let Some(observer) = observer.borrow_mut().as_mut() {
            observer(stage);
        }
    });
}

#[cfg(test)]
pub(crate) fn record_started_for_test(claim: &Claim, store: &Store, custody: &Value) -> Result<()> {
    claim.record_started(store, custody, &SpawnWitness(()))
}

#[cfg(test)]
pub(crate) fn record_not_started_for_test(
    claim: &Claim,
    store: &Store,
    custody: &Value,
    empty: bool,
) -> Result<()> {
    claim.record_not_started(store, custody, empty, &SpawnWitness(()))
}

impl Handoff {
    /// Consumes both tokens, even on failure. The caller retains the held
    /// boundary/identity for inspection. Capacity is never discharged here.
    /// A late pause records not_started only with proven cleanup; a returned
    /// protocol report describes local IO/cleanup, never remote success.
    /// No dispatched caller or terminal ledger/capacity release exists.
    pub(crate) fn spawn(
        self,
        store: &Store,
        claim: Claim,
        boundary: &mut Qualified,
    ) -> Result<io::ProtocolReport> {
        ensure!(
            store.connection().is_autocommit(),
            "spawn preparation requires no home transaction"
        );
        let timeout = self.check_binding(store, &claim)?;
        init_host(&self.conn)?;
        let adapter = claim.adapter()?;
        let (artifact, metadata) = claim.protocol_input()?;
        let input = io::Input::new(artifact, &metadata)?;
        let (streams, child_streams) = io::streams()?;
        let inputs = adapter
            .files()
            .map(|file| duplicate(file.as_raw_fd()))
            .collect::<Result<Vec<_>>>()?;
        let mut pending = boundary.gate_with(
            adapter.executable(),
            adapter.args(),
            adapter.env(),
            &inputs,
            Some(&child_streams),
            timeout,
        )?;
        drop(child_streams);
        let custody = json!({"schema_version":1, "claim":claim.binding(),
            "hold":serde_json::from_str::<Value>(&self.record)?,
            "admitted_charter_digest":self.charter_digest, "timeout_ms":timeout,
            "adapter":adapter.binding(),
            "protocol":{"schema_version":1,"metadata_sha256":crate::core::hash(&metadata)},
            "cgroup":pending.held.identity(),
            "payload":pending.held.identity()["gate_process"],
            "containment_scope":"attempt_cgroup_only",
            "limitation":"outside_ipc_delegation_not_excluded"});
        ensure!(
            custody.to_string().len() <= 65_536,
            "deployment custody exceeds bound"
        );
        // Lock order is home then host. Authority and pause are rechecked while
        // the child remains gated. Host commits first, home second. Any partial
        // failure rolls back what is possible and Drop kills the gated child;
        // the durable handoff/capacity is NEVER compensated or replayed here.
        let home = store.write_transaction()?;
        let host = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        #[cfg(test)]
        observe("host_locked");
        self.check_binding(store, &claim)?;
        let witness = SpawnWitness(());
        claim.record_started(store, &custody, &witness)?;
        let mut launched = self.reservation.clone();
        launched.state = "deployment-started".into();
        launched.payload = Some(serde_json::from_value(custody["payload"].clone())?);
        record_host(&host, &self.record, &launched, &custody)?;
        host.commit()?;
        home.commit()?;
        // The DB lifecycle read can wait for a writer. Recheck it BEFORE the
        // final membership/deadline checks so that wait cannot silently age the
        // release decision. The emergency file remains the LAST observation.
        #[cfg(test)]
        observe("before_release");
        let lifecycle = claim.check_grant_lifecycle(store);
        let mut pause_refused = lifecycle.is_err();
        let release = lifecycle.and_then(|()| {
            pending.release_with(|| {
                let result = claim.check_pause();
                pause_refused = result.is_err();
                result
            })
        });
        if let Err(error) = release {
            if pause_refused {
                // No release byte was attempted. Even proven cleanup retains
                // both custody records and capacity; it never refunds the use.
                let empty = pending.abort().is_ok();
                claim
                    .record_not_started(store, &custody, empty, &witness)
                    .with_context(|| format!("deployment release refused: {error:#}"))?;
            }
            return Err(error);
        }
        pending.supervise(streams, input)
    }

    #[cfg(test)]
    pub(crate) fn validate_for_test(&self, store: &Store, claim: &Claim) -> Result<u64> {
        self.check_binding(store, claim)
    }

    fn check_binding(&self, store: &Store, claim: &Claim) -> Result<u64> {
        let held = &self.reservation;
        let value = claim.value();
        ensure!(
            held.home == store.home()
                && value["home"] == json!(std::fs::canonicalize(store.home())?)
                && held.key
                    == crate::jobs::reservation_key(store.home(), &held.job_id, &held.incarnation)
                && held.job_id.starts_with("deployment-")
                && !held.incarnation.is_empty()
                && held.state == "deployment-handoff"
                && held.payload.is_none()
                && held.escaped.is_empty()
                && held.model_route.is_none()
                && held.ship.as_deref() == value["grant_snapshot"]["grantee"].as_str()
                && held.task.as_deref() == value["task_id"].as_str(),
            "deployment claim/hold binding mismatch"
        );
        ensure!(
            serde_json::to_value(
                held.supervisor
                    .as_ref()
                    .context("handoff dispatcher missing")?
            )? == serde_json::from_str::<Value>(&crate::jobs::own_identity()?)?,
            "handoff dispatcher changed"
        );
        let task = held.task.as_deref().context("deployment task missing")?;
        let charter = store.authorize(task, "deploy_nonproduction")?;
        ensure!(
            held.charter.as_deref() == charter["id"].as_str()
                && self.charter_digest == crate::core::hash(&charter)
                && value["authority"]["charter_digest"] == self.charter_digest,
            "admitted deployment charter changed"
        );
        let timeout = u64::try_from(held.timeout_ms.context("deployment timeout missing")?)?;
        ensure!(
            (1..=3_600_000).contains(&timeout)
                && timeout
                    <= charter["job_timeout_secs"]
                        .as_u64()
                        .unwrap_or(900)
                        .saturating_mul(1000),
            "admitted deployment timeout changed"
        );
        let record: String = self.conn.query_row(
            "SELECT record FROM reservations WHERE key=?1",
            [&held.key],
            |r| r.get(0),
        )?;
        ensure!(
            record == self.record,
            "deployment reservation custody changed"
        );
        Ok(timeout)
    }
}

#[cfg(test)]
mod tests;

fn init_host(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS deployment_payloads(key TEXT PRIMARY KEY,record TEXT NOT NULL);
        CREATE TRIGGER IF NOT EXISTS deployment_payloads_no_update BEFORE UPDATE ON deployment_payloads
            BEGIN SELECT RAISE(ABORT,'deployment custody is immutable'); END;
        CREATE TRIGGER IF NOT EXISTS deployment_payloads_no_delete BEFORE DELETE ON deployment_payloads
            BEGIN SELECT RAISE(ABORT,'deployment custody is immutable'); END;")?;
    Ok(())
}

fn record_host(
    conn: &rusqlite::Connection,
    original: &str,
    launched: &crate::jobs::Reservation,
    custody: &Value,
) -> Result<()> {
    ensure!(
        conn.execute(
            "UPDATE reservations SET record=?3 WHERE key=?1 AND record=?2",
            params![launched.key, original, serde_json::to_string(launched)?]
        )? == 1,
        "deployment reservation custody changed before release"
    );
    conn.execute(
        "INSERT INTO deployment_payloads(key,record) VALUES(?1,?2)",
        params![launched.key, custody.to_string()],
    )?;
    Ok(())
}
