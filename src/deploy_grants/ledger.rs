//! Dormant append-only use ledger. No dispatcher or adapter execution.
use crate::store::Store;
use anyhow::{Result, ensure};
use serde_json::Value;

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
            BEGIN SELECT RAISE(ABORT,'deployment use is immutable'); END;",
    )?;
    Ok(())
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
