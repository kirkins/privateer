//! Dormant operator-only claim/recovery. No admission, dispatcher or uploader.
//! Status (including a replay saying `claimed`) is never permission to launch.
use super::{ledger, verifier};
use crate::store::Store;
use anyhow::{Context, Result};
use rusqlite::params;
use serde_json::{Value, json};

/// Only a newly committed claim produces this non-deserializable token. A later
/// admitted executor must require it, not a JSON status or saved authorization.
pub(super) struct Claim {
    pub(super) id: String,
    pub(super) identity: String,
}

pub(super) struct Outcome {
    pub(super) status: Value,
    pub(super) claim: Option<Claim>,
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

pub(super) fn claim_with(
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
