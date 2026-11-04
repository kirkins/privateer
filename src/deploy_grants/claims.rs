//! Dormant operator-only claim/recovery. No admission, dispatcher or uploader.
//! Status (including a replay saying `claimed`) is never permission to launch.
use super::{ledger, verifier};
use crate::store::Store;
use anyhow::{Context, Result};
use rusqlite::params;
use serde_json::{Value, json};

/// Only a newly committed claim produces this non-deserializable token. A later
/// admitted executor must require it, not a JSON status or saved authorization,
/// and may move claimed -> started only while BOTH state and claim_identity
/// still match. Possession alone does not override a changed durable claim.
pub(crate) struct Claim {
    id: String,
    identity: String,
    proof: verifier::Verified,
    used: ledger::Recorded,
}

impl Claim {
    pub(super) fn id(&self) -> &str {
        &self.id
    }
    pub(super) fn identity(&self) -> &str {
        &self.identity
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn adapter(&self) -> Result<verifier::adapter::PreparedAdapter> {
        self.proof.adapter()
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn value(&self) -> &Value {
        self.used.value()
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn binding(&self) -> Value {
        json!({"deployment_id":self.id, "claim_identity":self.identity,
            "use_sha256":crate::core::hash(self.used.value())})
    }

    // Only the consuming spawn calls this, inside its home transaction. Neither
    // a historical started row nor a saved JSON binding reconstructs a token.
    #[cfg(target_os = "linux")]
    pub(crate) fn record_started(
        &self,
        store: &Store,
        custody: &Value,
        _witness: &crate::jobs::deployment_admission::containment::SpawnWitness,
    ) -> Result<()> {
        super::captain(store, "deployment spawn")?;
        anyhow::ensure!(
            !store.connection().is_autocommit(),
            "spawn requires home writer lock"
        );
        let dispatcher = serde_json::from_str::<Value>(&self.identity)?;
        anyhow::ensure!(
            dispatcher["dispatcher"]
                == serde_json::from_str::<Value>(&crate::jobs::own_identity()?)?,
            "claim dispatcher changed"
        );
        anyhow::ensure!(
            custody["claim"] == self.binding(),
            "deployment custody claim mismatch"
        );
        self.used
            .matches(ledger::recorded(store, &self.id)?.value())?;
        verifier::recheck_claim(store, &self.proof, &self.used)?;
        anyhow::ensure!(
            store.connection().execute(
                "UPDATE deployment_intents SET state='started',updated_ms=?3
             WHERE use_id=?1 AND state='claimed' AND claim_identity=?2",
                params![self.id, self.identity, Store::now_ms()]
            )? == 1,
            "deployment claim no longer matches"
        );
        store.connection().execute(
            "INSERT INTO deployment_payloads(use_id,record) VALUES(?1,?2)",
            params![self.id, custody.to_string()],
        )?;
        audit(store, &self.used, "deployment.started", custody.clone())
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn check_pause(&self) -> Result<()> {
        self.proof.check_pause()
    }

    // Called only when the final pause read refused BEFORE any release write.
    // No current authority is needed to preserve an abort after revocation.
    // Failure leaves the conservative started custody, never a replay token.
    #[cfg(target_os = "linux")]
    pub(crate) fn record_not_started(
        &self,
        store: &Store,
        custody: &Value,
        empty: bool,
        _witness: &crate::jobs::deployment_admission::containment::SpawnWitness,
    ) -> Result<()> {
        let tx = store.write_transaction()?;
        let saved: String = tx.query_row(
            "SELECT record FROM deployment_payloads WHERE use_id=?1",
            [&self.id],
            |r| r.get(0),
        )?;
        let expected = custody.to_string();
        anyhow::ensure!(
            custody["claim"] == self.binding() && saved == expected,
            "deployment abort custody mismatch"
        );
        let outcome = if empty { "not_started" } else { "unknown" };
        anyhow::ensure!(
            tx.execute(
                "UPDATE deployment_intents SET state=?3,outcome=?4,updated_ms=?5
                 WHERE use_id=?1 AND state='started' AND claim_identity=?2",
                params![
                    self.id,
                    self.identity,
                    if empty { "denied" } else { "unknown" },
                    outcome,
                    Store::now_ms()
                ],
            )? == 1,
            "deployment abort claim no longer matches"
        );
        audit(
            store,
            &self.used,
            "deployment.release_refused",
            json!({
            "claim":self.binding(), "outcome":outcome,
            "reason":"final emergency pause check refused", "release_attempted":false,
            "cleanup_proven_empty":empty, "capacity_retained":true,
            "containment_scope":"attempt_cgroup_only",
            "limitation":"outside_ipc_delegation_not_excluded"}),
        )?;
        tx.commit()?;
        Ok(())
    }
}

pub(super) struct Outcome {
    pub(super) status: Value,
    pub(super) claim: Option<Claim>,
}

#[cfg(test)]
pub(super) fn claim_for_test(
    store: &Store,
    id: &str,
    verify: impl FnOnce(&ledger::Recorded) -> Result<verifier::Verified>,
    recheck: impl FnOnce(&verifier::Verified, &ledger::Recorded) -> Result<()>,
) -> Result<Outcome> {
    claim_with(store, id, verify, recheck)
}

fn status(store: &Store, id: &str) -> Result<Value> {
    store
        .connection()
        .query_row(
            "SELECT state,request_thread,outcome FROM deployment_intents WHERE use_id=?1",
            [id],
            |r| {
                Ok(json!({"id":id,"state":r.get::<_,String>(0)?,
            "request_thread":r.get::<_,String>(1)?,"outcome":r.get::<_,Option<String>>(2)?,
            "dispatch_enabled":false}))
            },
        )
        .context("deployment intent missing")
}

fn audit(store: &Store, used: &ledger::Recorded, kind: &str, detail: Value) -> Result<()> {
    let value = used.value();
    store.audit(
        kind,
        &json!({"deployment_id":used.id(),"grant_id":value["grant_id"],
        "grant_revision":value["grant_revision"],"task_id":value["task_id"],
        "environment":value["environment"],"commit":value["commit"],
        "artifact":value["artifact"],"detail":detail}),
    )?;
    Ok(())
}

pub(super) fn claim(store: &Store, id: &str) -> Result<Outcome> {
    claim_with(
        store,
        id,
        |used| verifier::verify_claim(store, used),
        |proof, used| verifier::recheck_claim(store, proof, used),
    )
}

fn claim_with(
    store: &Store,
    id: &str,
    verify: impl FnOnce(&ledger::Recorded) -> Result<verifier::Verified>,
    recheck: impl FnOnce(&verifier::Verified, &ledger::Recorded) -> Result<()>,
) -> Result<Outcome> {
    super::captain(store, "deployment claim")?;
    let used = ledger::recorded(store, id)?;
    if status(store, id)?["state"] != "pending" {
        return Ok(Outcome {
            status: status(store, id)?,
            claim: None,
        });
    }
    // Fresh external evidence reads run without SQLite locks. The claim uses
    // the immutable retained artifact and checks the ALREADY consumed use.
    let proof = verify(&used);
    let tx = store.write_transaction()?;
    if status(store, id)?["state"] != "pending" {
        tx.commit()?;
        return Ok(Outcome {
            status: status(store, id)?,
            claim: None,
        });
    }
    // A status replay skips no authorization here: this is a full independent
    // current grant/revision/lifecycle/expiry, hosted ship, owned open task,
    // charter, budget, tool/installation, evidence age and emergency-pause
    // check under the lifecycle writer lock. Unreadable pause refuses.
    let checked = proof.and_then(|proof| {
        used.matches(proof.value())?;
        recheck(&proof, &used)?;
        Ok(proof)
    });
    let claim = if let Ok(proof) = checked {
        let identity = json!({"claim_id":uuid::Uuid::new_v4().to_string(),
            "dispatcher":serde_json::from_str::<Value>(&crate::jobs::own_identity()?)?})
        .to_string();
        tx.execute("UPDATE deployment_intents SET state='claimed',claim_identity=?2,updated_ms=?3 WHERE use_id=?1 AND state='pending'",
            params![id, identity, Store::now_ms()])?;
        audit(
            store,
            &used,
            "deployment.claimed",
            json!({"claim_identity":identity,
            "evidence_digest":proof.value()["evidence_digest"],"evidence":proof.value()["evidence"],
            "verified_ms":proof.value()["verified_ms"]}),
        )?;
        Some(Claim {
            id: id.to_owned(),
            identity,
            proof,
            used,
        })
    } else {
        // Never persist arbitrary verifier diagnostics or credential-bearing
        // errors. The immutable use remains consumed and inspectable.
        tx.execute("UPDATE deployment_intents SET state='denied',outcome='claim revalidation failed',updated_ms=?2 WHERE use_id=?1 AND state='pending'",
            params![id, Store::now_ms()])?;
        audit(
            store,
            &used,
            "deployment.denied",
            json!({"reason":"claim revalidation failed"}),
        )?;
        None
    };
    let result = Outcome {
        status: status(store, id)?,
        claim,
    };
    tx.commit()?;
    Ok(result)
}

/// Dormant executor boundary: a stale token cannot start a different claim or
/// repeat a start. This transition alone never launches or proves a payload.
#[cfg(test)]
pub(super) fn mark_started(store: &Store, claim: Claim) -> Result<()> {
    super::captain(store, "deployment start transition")?;
    let tx = store.write_transaction()?;
    anyhow::ensure!(
        tx.execute(
            "UPDATE deployment_intents SET state='started',updated_ms=?3
             WHERE use_id=?1 AND state='claimed' AND claim_identity=?2",
            params![claim.id(), claim.identity(), Store::now_ms()],
        )? == 1,
        "deployment claim no longer matches"
    );
    audit(
        store,
        &ledger::recorded(store, claim.id())?,
        "deployment.started",
        json!({"claim_identity":claim.identity()}),
    )?;
    tx.commit()?;
    Ok(())
}

/// Reconcile at most 20 ambiguous handoffs per pass. No pending work is claimed
/// and no process is launched, signalled, declared completed or refunded. Even
/// a proven-dead dispatcher says nothing about an escaped/surviving payload.
pub(super) fn recover(store: &Store) -> Result<Vec<Value>> {
    recover_with(store, crate::jobs::identity_alive)
}

pub(super) fn recover_with(
    store: &Store,
    alive: impl Fn(&str) -> Result<bool>,
) -> Result<Vec<Value>> {
    super::captain(store, "deployment recovery")?;
    let tx = store.write_transaction()?;
    let rows = {
        let mut query = tx.prepare("SELECT use_id,claim_identity FROM deployment_intents WHERE state IN ('claimed','started') ORDER BY updated_ms,use_id LIMIT 20")?;
        query
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut outcomes = Vec::new();
    for (id, identity) in rows {
        let live = identity
            .as_deref()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .and_then(|v| v.get("dispatcher").cloned())
            .is_some_and(|v| alive(&v.to_string()).unwrap_or(false));
        if live {
            continue;
        }
        let used = ledger::recorded(store, &id)?;
        tx.execute("UPDATE deployment_intents SET state='unknown',outcome='claim owner absent or unverifiable; inspect payload and remote outcome',updated_ms=?2 WHERE use_id=?1 AND state IN ('claimed','started')",
            params![id, Store::now_ms()])?;
        // Preserve claim_identity and immutable evidence for manual custody
        // inspection. An unknown attempt can never return to pending here.
        audit(
            store,
            &used,
            "deployment.unknown",
            json!({"reason":"claim owner absent or unverifiable"}),
        )?;
        outcomes.push(status(store, &id)?);
    }
    tx.commit()?;
    Ok(outcomes)
}
