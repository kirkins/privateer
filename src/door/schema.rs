//! Registry schema writes must reserve the writer before reading the schema.
use anyhow::{Context, Result};
use rusqlite::{Connection, Transaction, TransactionBehavior};

/// Top-level migrations wait through the connection's busy handler instead of
/// attempting a SHARED -> RESERVED upgrade (which can fail without waiting).
/// Production init callers currently use autocommit connections. Nested
/// schema helpers reuse the migration's writer lock. Any other caller inside
/// a transaction must acquire the writer first (e.g. BEGIN IMMEDIATE), never
/// enter here from a deferred read transaction that would need a lock upgrade.
/// A savepoint keeps a failed migration from leaking partial DDL into that
/// caller's transaction; neither success nor failure commits the caller's work.
pub(super) fn write(conn: &Connection, migrate: impl FnOnce() -> Result<()>) -> Result<()> {
    let transaction = conn
        .is_autocommit()
        .then(|| Transaction::new_unchecked(conn, TransactionBehavior::Immediate))
        .transpose()
        .context("acquiring door schema writer")?;
    conn.execute_batch("SAVEPOINT door_schema_migration")?;
    let mut savepoint = Migration {
        conn,
        released: false,
    };
    migrate()?;
    conn.execute_batch("RELEASE door_schema_migration")?;
    savepoint.released = true;
    if let Some(transaction) = transaction {
        transaction.commit()?;
    }
    Ok(())
}

struct Migration<'a> {
    conn: &'a Connection,
    released: bool,
}
impl Drop for Migration<'_> {
    fn drop(&mut self) {
        if !self.released {
            let _ = self
                .conn
                .execute_batch("ROLLBACK TO door_schema_migration; RELEASE door_schema_migration");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn immediate_migration_uses_busy_handler_unlike_deferred_upgrade() {
        static BUSY_CALLS: AtomicUsize = AtomicUsize::new(0);
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("registry.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE clients(id);
             CREATE TRIGGER legacy AFTER DELETE ON clients BEGIN SELECT 1; END;",
        )
        .unwrap();
        conn.busy_handler(Some(|_| {
            BUSY_CALLS.fetch_add(1, Ordering::SeqCst);
            false
        }))
        .unwrap();
        let writer = Connection::open(&path).unwrap();
        // This is the old cleanup pattern: a no-op CREATE takes SHARED, then
        // an actual legacy DROP needs RESERVED while another writer owns it.
        conn.execute_batch(
            "SAVEPOINT cleanup;
             CREATE TRIGGER IF NOT EXISTS legacy AFTER DELETE ON clients BEGIN SELECT 1; END;",
        )
        .unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let error = conn.execute_batch("DROP TRIGGER legacy").unwrap_err();
        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy)
        );
        assert_eq!(BUSY_CALLS.load(Ordering::SeqCst), 0);
        conn.execute_batch("ROLLBACK").unwrap();

        // The replacement contends before acquiring SHARED or running any
        // migration statements, so the configured busy handler gets control.
        let error = write(&conn, || panic!("migration ran without writer lock")).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<rusqlite::Error>()
                .unwrap()
                .sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy)
        );
        assert_eq!(BUSY_CALLS.load(Ordering::SeqCst), 1);
        assert!(conn.is_autocommit());
        writer.execute_batch("ROLLBACK").unwrap();
        write(&conn, || {
            conn.execute_batch("DROP TRIGGER legacy")?;
            Ok(())
        })
        .unwrap();
        assert!(conn.is_autocommit());
    }
}
