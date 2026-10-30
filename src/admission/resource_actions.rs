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

/// Installed only by native ledger initialization. Recovery readers/actions
/// deliberately do not call this migration.
pub(crate) fn init_recovery(conn: &Connection) -> Result<()> {
    if recovery_identity(conn).is_ok() {
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS resource_recovery_identity(id INTEGER PRIMARY KEY CHECK(id=1), incarnation TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS reservation_generations(key TEXT PRIMARY KEY, generation TEXT NOT NULL);
         INSERT OR IGNORE INTO reservation_generations SELECT key,lower(hex(randomblob(16))) FROM reservations;
         CREATE TRIGGER IF NOT EXISTS recovery_reservation_insert AFTER INSERT ON reservations BEGIN
           INSERT OR REPLACE INTO reservation_generations VALUES(NEW.key,lower(hex(randomblob(16)))); END;
         CREATE TRIGGER IF NOT EXISTS recovery_reservation_delete AFTER DELETE ON reservations BEGIN
           DELETE FROM reservation_generations WHERE key=OLD.key; END;
         CREATE TABLE IF NOT EXISTS resource_recovery_permits(
           id TEXT PRIMARY KEY, created_ms INTEGER NOT NULL, expires_ms INTEGER NOT NULL,
           binding TEXT NOT NULL, provenance TEXT NOT NULL, revoked_ms INTEGER, consumed_result TEXT);
         CREATE TABLE IF NOT EXISTS resource_recovery_results(
           key_hash TEXT PRIMARY KEY, grant_id TEXT NOT NULL, request_hash TEXT NOT NULL,
           created_ms INTEGER NOT NULL, result TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS resource_recovery_audit(
           seq INTEGER PRIMARY KEY, created_ms INTEGER NOT NULL, record TEXT NOT NULL);",
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO resource_recovery_identity VALUES(1,?1)",
        [uuid::Uuid::new_v4().to_string()],
    )?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn recovery_identity(conn: &Connection) -> Result<String> {
    conn.prepare("SELECT key,generation FROM reservation_generations LIMIT 0")?;
    let triggers: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN ('recovery_reservation_insert','recovery_reservation_delete')", [], |r| r.get(0))?;
    ensure!(triggers == 2, "recovery generation triggers missing");
    conn.prepare("SELECT id,created_ms,expires_ms,binding,provenance,revoked_ms,consumed_result FROM resource_recovery_permits LIMIT 0")?;
    conn.prepare("SELECT key_hash,grant_id,request_hash,created_ms,result FROM resource_recovery_results LIMIT 0")?;
    conn.prepare("SELECT seq,created_ms,record FROM resource_recovery_audit LIMIT 0")?;
    let id: String = conn.query_row(
        "SELECT incarnation FROM resource_recovery_identity WHERE id=1",
        [],
        |r| r.get(0),
    )?;
    uuid::Uuid::parse_str(&id)?;
    Ok(id)
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
    if recovery_results_present(conn)? {
        conn.execute(
            "DELETE FROM resource_recovery_results WHERE created_ms < ?1",
            [now.saturating_sub(RETENTION_MS)],
        )?;
    }
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

fn recovery_results_present(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='resource_recovery_results')", [], |r| r.get(0))?)
}

pub(crate) fn has_capacity(conn: &Connection, grant: &str) -> Result<bool> {
    let (all, own): (i64, i64) = conn.query_row(
        "SELECT COUNT(*),COALESCE(SUM(grant_id=?1),0) FROM resource_action_results",
        [grant],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    // Policy and recovery results share admission bounds. Legacy policy-only
    // readers remain compatible without creating or migrating recovery tables.
    let (recovery_all, recovery_own): (i64, i64) = if recovery_results_present(conn)? {
        conn.query_row(
            "SELECT COUNT(*),COALESCE(SUM(grant_id=?1),0) FROM resource_recovery_results",
            [grant],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?
    } else {
        (0, 0)
    };
    Ok(all + recovery_all < GLOBAL && own + recovery_own < PER_GRANT)
}
