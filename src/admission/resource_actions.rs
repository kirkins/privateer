//! Ledger-owned outcome recovery. Schema is installed only by native ledger
//! initialization, never by the delegated probe/action or a capacity reader.
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};

pub(crate) const RETENTION_MS: i64 = 86_400_000;
pub(crate) const PER_GRANT: i64 = 256;
pub(crate) const GLOBAL: i64 = 4096;
/// Separate count-bounded policy history, never removed by result expiry.
pub(crate) const AUDIT_KEEP: i64 = 4096;

pub(super) fn init(conn: &Connection) -> Result<()> {
    if identity(conn).is_ok() {
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS resource_action_identity(id INTEGER PRIMARY KEY CHECK(id=1), origin TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS resource_action_results(
           grant_id TEXT NOT NULL, resource TEXT NOT NULL, key_hash TEXT NOT NULL,
           args_hash TEXT NOT NULL, created_ms INTEGER NOT NULL, result TEXT NOT NULL,
           audit TEXT NOT NULL, PRIMARY KEY(grant_id,resource,key_hash));
         CREATE TABLE IF NOT EXISTS resource_policy_audit(
           seq INTEGER PRIMARY KEY, created_ms INTEGER NOT NULL, record TEXT NOT NULL);
         INSERT INTO resource_policy_audit(created_ms,record)
           SELECT created_ms,audit FROM
             (SELECT created_ms,audit FROM resource_action_results ORDER BY created_ms DESC LIMIT 4096)
           WHERE NOT EXISTS(SELECT 1 FROM resource_policy_audit)
           ORDER BY created_ms;",
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO resource_action_identity(id,origin) VALUES(1,?1)",
        [uuid::Uuid::new_v4().to_string()],
    )?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn identity(conn: &Connection) -> Result<String> {
    // All tables must exist: interrupted/legacy initialization fails closed.
    conn.prepare("SELECT grant_id FROM resource_action_results LIMIT 0")?;
    conn.prepare("SELECT seq,created_ms,record FROM resource_policy_audit LIMIT 0")?;
    let origin: String = conn.query_row(
        "SELECT origin FROM resource_action_identity WHERE id=1",
        [],
        |r| r.get(0),
    )?;
    uuid::Uuid::parse_str(&origin)?;
    Ok(origin)
}

/// Caller holds the ledger writer. Pruning and lookup are in the same effect
/// transaction; a matching replay works even at capacity.
pub(crate) fn lookup(
    conn: &Connection,
    grant: &str,
    resource: &str,
    key: &str,
    now: i64,
) -> Result<Option<(String, String)>> {
    ensure!(!conn.is_autocommit(), "writer required");
    conn.execute(
        "DELETE FROM resource_action_results WHERE created_ms < ?1",
        [now.saturating_sub(RETENTION_MS)],
    )?;
    Ok(conn.query_row("SELECT args_hash,result FROM resource_action_results WHERE grant_id=?1 AND resource=?2 AND key_hash=?3", params![grant,resource,key], |r| Ok((r.get(0)?,r.get(1)?))).optional()?)
}

/// Must commit with the setting and outcome. A count bound, not a time expiry:
/// the last policy changes survive even years with no subsequent action.
pub(crate) fn audit(conn: &Connection, now: i64, record: &str) -> Result<()> {
    ensure!(!conn.is_autocommit(), "writer required");
    conn.execute(
        "INSERT INTO resource_policy_audit(created_ms,record) VALUES(?1,?2)",
        params![now, record],
    )?;
    conn.execute(
        "DELETE FROM resource_policy_audit WHERE seq NOT IN
         (SELECT seq FROM resource_policy_audit ORDER BY seq DESC LIMIT ?1)",
        [AUDIT_KEEP],
    )?;
    Ok(())
}

pub(crate) fn has_capacity(conn: &Connection, grant: &str) -> Result<bool> {
    let (all, own): (i64, i64) = conn.query_row(
        "SELECT COUNT(*),COALESCE(SUM(grant_id=?1),0) FROM resource_action_results",
        [grant],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(all < GLOBAL && own < PER_GRANT)
}
