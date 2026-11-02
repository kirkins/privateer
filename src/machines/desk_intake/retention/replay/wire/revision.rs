//! Atomically notify the inactive selection ledger of actual proof changes.
//! Optional on source-only/v4 registries: never install door tables here. A
//! repeated immutable insert or identical vector does not consume a revision.
use super::*;

pub(in crate::machines::desk_intake) fn write(
    c: &Connection,
    mutate: impl FnOnce() -> Result<usize>,
) -> Result<usize> {
    let tx = c
        .is_autocommit()
        .then(|| rusqlite::Transaction::new_unchecked(c, TransactionBehavior::Immediate))
        .transpose()?;
    c.execute_batch("SAVEPOINT replay_selection_revision")?;
    let mut guard = Change {
        conn: c,
        released: false,
    };
    let changed = mutate()?;
    if changed > 0 {
        let installed: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='desk_replay_revision')", [], |r|r.get(0))?;
        if installed {
            ensure!(c.execute("UPDATE desk_replay_revision SET revision=revision+1 WHERE singleton=1 AND revision<?1", [i64::MAX])? == 1, "backpressure");
        }
    }
    c.execute_batch("RELEASE replay_selection_revision")?;
    guard.released = true;
    if let Some(tx) = tx {
        tx.commit()?;
    }
    Ok(changed)
}
struct Change<'a> {
    conn: &'a Connection,
    released: bool,
}
impl Drop for Change<'_> {
    fn drop(&mut self) {
        if !self.released {
            let _ = self.conn.execute_batch(
                "ROLLBACK TO replay_selection_revision; RELEASE replay_selection_revision",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn revision_and_proof_change_share_outer_rollback_and_exhaustion_backpressure() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE proof_body(id TEXT PRIMARY KEY); CREATE TABLE desk_replay_revision(singleton INTEGER PRIMARY KEY,revision INTEGER); INSERT INTO desk_replay_revision VALUES(1,0)").unwrap();
        let tx = rusqlite::Transaction::new_unchecked(&c, TransactionBehavior::Immediate).unwrap();
        write(&tx, || {
            Ok(tx.execute("INSERT OR IGNORE INTO proof_body VALUES('new')", [])?)
        })
        .unwrap();
        write(&tx, || {
            Ok(tx.execute("INSERT OR IGNORE INTO proof_body VALUES('new')", [])?)
        })
        .unwrap();
        assert_eq!(
            tx.query_row("SELECT revision FROM desk_replay_revision", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        tx.rollback().unwrap();
        assert_eq!(
            c.query_row("SELECT COUNT(*) FROM proof_body", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        c.execute("UPDATE desk_replay_revision SET revision=?1", [i64::MAX])
            .unwrap();
        assert_eq!(
            write(&c, || Ok(
                c.execute("INSERT INTO proof_body VALUES('blocked')", [])?
            ))
            .unwrap_err()
            .to_string(),
            "backpressure"
        );
        assert_eq!(
            c.query_row("SELECT COUNT(*) FROM proof_body", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(c.is_autocommit());
        c.execute_batch("DROP TABLE desk_replay_revision").unwrap();
        write(&c, || {
            Ok(c.execute("INSERT INTO proof_body VALUES('source-only')", [])?)
        })
        .unwrap();
        assert_eq!(
            c.query_row("SELECT COUNT(*) FROM proof_body", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}
