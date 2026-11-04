//! Dormant append-only use ledger. No dispatcher or adapter execution.
use crate::store::Store;
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

pub(super) fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS deployment_uses(
            id TEXT PRIMARY KEY, grant_id TEXT NOT NULL REFERENCES standing_deploy_grants(id),
            grant_revision INTEGER NOT NULL, grantee TEXT NOT NULL, task_id TEXT NOT NULL,
            idempotency_key TEXT NOT NULL, request_digest TEXT NOT NULL,
            grant_snapshot TEXT NOT NULL, evidence TEXT NOT NULL, created_ms INTEGER NOT NULL,
            UNIQUE(grantee,idempotency_key));
         CREATE INDEX IF NOT EXISTS deployment_uses_grant ON deployment_uses(grant_id);
         CREATE TRIGGER IF NOT EXISTS deployment_uses_no_update BEFORE UPDATE ON deployment_uses
            BEGIN SELECT RAISE(ABORT,'deployment use is immutable'); END;
         CREATE TRIGGER IF NOT EXISTS deployment_uses_no_delete BEFORE DELETE ON deployment_uses
            BEGIN SELECT RAISE(ABORT,'deployment use is immutable'); END;
         CREATE TABLE IF NOT EXISTS deployment_intents(
            use_id TEXT PRIMARY KEY REFERENCES deployment_uses(id),
            state TEXT NOT NULL CHECK(state IN ('pending','claimed','started','denied','completed','unknown')),
            request_thread TEXT NOT NULL UNIQUE, updated_ms INTEGER NOT NULL,
            claim_identity TEXT, outcome TEXT);
         CREATE TABLE IF NOT EXISTS deployment_payloads(
            use_id TEXT PRIMARY KEY REFERENCES deployment_uses(id),record TEXT NOT NULL);
         CREATE TRIGGER IF NOT EXISTS deployment_payloads_no_update BEFORE UPDATE ON deployment_payloads
            BEGIN SELECT RAISE(ABORT,'deployment custody is immutable'); END;
         CREATE TRIGGER IF NOT EXISTS deployment_payloads_no_delete BEFORE DELETE ON deployment_payloads
            BEGIN SELECT RAISE(ABORT,'deployment custody is immutable'); END;",
    )?;
    Ok(())
}

/// Not a dispatcher: the future admitted entry point supplies a sealed verifier
/// proof. There is no RPC, CLI, door, relay or adapter path to this function.
#[allow(
    dead_code,
    reason = "Dormant until admitted deployment.start in slice 2b-ii-b"
)]
pub(super) fn consume(store: &Store, proof: super::verifier::Verified, key: &str) -> Result<Value> {
    consume_with(store, proof, key, |p| super::verifier::recheck(store, p))
}

fn existing(store: &Store, grantee: &str, key: &str, digest: &str) -> Result<Option<Value>> {
    let row = store
        .connection()
        .query_row(
            "SELECT u.id,u.request_digest,i.state,i.request_thread FROM deployment_uses u
         JOIN deployment_intents i ON i.use_id=u.id WHERE u.grantee=?1 AND u.idempotency_key=?2",
            params![grantee, key],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    row.map(|(id, old, state, thread)| {
        ensure!(
            old == digest,
            "deployment idempotency key reused with changed inputs"
        );
        // A replay is historical status, never permission to launch. A claim
        // must re-run the full current grant/charter/task/pause/budget checks;
        // neither this response nor the immutable use's evidence is authority.
        Ok(json!({"id":id,"state":state,"request_thread":thread,"dispatch_enabled":false}))
    })
    .transpose()
}

#[cfg(test)]
pub(super) fn consume_for_test(
    store: &Store,
    proof: super::verifier::Verified,
    key: &str,
    recheck: impl FnOnce(&super::verifier::Verified) -> Result<()>,
) -> Result<Value> {
    consume_with(store, proof, key, recheck)
}

fn consume_with(
    store: &Store,
    proof: super::verifier::Verified,
    key: &str,
    recheck: impl FnOnce(&super::verifier::Verified) -> Result<()>,
) -> Result<Value> {
    super::text(key, "deployment idempotency key", 160)?;
    crate::store::identifier(key)?;
    let value = proof.value();
    let grant = &value["grant_snapshot"];
    super::visible(store, grant)?;
    let grantee = crate::store::required(grant, "grantee")?;
    let request_digest = crate::core::hash(&json!({"grant_id":value["grant_id"],
        "task_id":value["task_id"],"environment":value["environment"],"commit":value["commit"],
        "artifact_sha256":value["artifact"]["sha256"]}));
    let tx = store.write_transaction()?;
    if let Some(old) = existing(store, grantee, key, &request_digest)? {
        tx.commit()?;
        return Ok(old);
    }
    // No caller-supplied green booleans or deserialized proof can reach this
    // boundary. All checks share the lifecycle writer lock with pause/revoke.
    recheck(&proof)?;
    let pending: i64 = tx.query_row(
        "SELECT count(*) FROM deployment_intents WHERE state IN ('pending','claimed','started')",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        pending < 20,
        "too many pending or in-flight deployment intents; inspect outstanding attempts"
    );
    let id = uuid::Uuid::new_v4().to_string();
    let thread = format!("deployment-{id}");
    let now = Store::now_ms();
    let mut authorization = value.clone();
    authorization["deployment_authorized"] = json!(true);
    tx.execute(
        "INSERT INTO deployment_uses(id,grant_id,grant_revision,grantee,task_id,idempotency_key,
        request_digest,grant_snapshot,evidence,created_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            id,
            crate::store::required(value, "grant_id")?,
            value["grant_revision"]
                .as_i64()
                .context("missing grant revision")?,
            grantee,
            crate::store::required(value, "task_id")?,
            key,
            request_digest,
            grant.to_string(),
            authorization.to_string(),
            now
        ],
    )?;
    tx.execute("INSERT INTO deployment_intents(use_id,state,request_thread,updated_ms) VALUES(?1,'pending',?2,?3)",
        params![id,thread,now])?;
    store.audit("deploy_grant.used", &json!({"deployment_id":id,"grant_id":value["grant_id"],
        "grant_revision":value["grant_revision"],"grantee":grantee,"task_id":value["task_id"],
        "environment":value["environment"],"commit":value["commit"],"artifact":value["artifact"],
        "bindings":value["bindings"],"evidence_digest":value["evidence_digest"],"request_thread":thread}))?;
    let result =
        existing(store, grantee, key, &request_digest)?.context("deployment intent missing")?;
    tx.commit()?;
    Ok(result)
}

/// A ledger-owned consumed use. Only a database read can construct it; no
/// dispatched caller can provide a JSON permission or a budget-exemption flag.
pub(super) struct Recorded {
    id: String,
    value: Value,
}

impl Recorded {
    pub(super) fn id(&self) -> &str {
        &self.id
    }
    pub(super) fn value(&self) -> &Value {
        &self.value
    }
    pub(super) fn matches(&self, fresh: &Value) -> Result<()> {
        for key in [
            "grant_id",
            "grant_revision",
            "grant_snapshot",
            "task_id",
            "authority",
            "environment",
            "installation_id",
            "home",
            "rule_digest",
            "bindings",
            "artifact",
            "commit",
        ] {
            ensure!(
                fresh[key] == self.value[key],
                "deployment use binding changed: {key}"
            );
        }
        Ok(())
    }
    pub(super) fn budget(&self, store: &Store, grant: &Value, task: &str) -> Result<()> {
        ensure!(
            grant == &self.value["grant_snapshot"] && self.value["task_id"] == task,
            "deployment use grant or task changed"
        );
        let present: bool = store.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM deployment_uses WHERE id=?1 AND grant_id=?2
             AND grant_revision=?3 AND grantee=?4 AND task_id=?5)",
            params![
                self.id,
                crate::store::required(grant, "id")?,
                grant["revision"].as_i64(),
                crate::store::required(grant, "grantee")?,
                task
            ],
            |r| r.get(0),
        )?;
        ensure!(present, "deployment use binding missing");
        if let Some(max) = grant["max_uses"].as_i64() {
            let used: i64 = store.connection().query_row(
                "SELECT count(*) FROM deployment_uses WHERE grant_id=?1",
                [crate::store::required(grant, "id")?],
                |r| r.get(0),
            )?;
            // This attempt is already counted. No refund and no second debit.
            ensure!(used <= max, "deployment grant use budget exhausted");
        }
        Ok(())
    }
}

pub(super) fn recorded(store: &Store, id: &str) -> Result<Recorded> {
    crate::store::identifier(id)?;
    let evidence: String = store
        .connection()
        .query_row(
            "SELECT evidence FROM deployment_uses WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?
        .context("unknown deployment use")?;
    let value: Value = serde_json::from_str(&evidence)?;
    super::visible(store, &value["grant_snapshot"])?;
    Ok(Recorded {
        id: id.to_owned(),
        value,
    })
}

/// Count attempts across all revisions, not just successful uploads.
pub(super) fn budget(store: &Store, grant: &Value) -> Result<()> {
    if let Some(max) = grant["max_uses"].as_i64() {
        let used: i64 = store.connection().query_row(
            "SELECT count(*) FROM deployment_uses WHERE grant_id=?1",
            [crate::store::required(grant, "id")?],
            |r| r.get(0),
        )?;
        ensure!(used < max, "deployment grant use budget exhausted");
    }
    Ok(())
}
