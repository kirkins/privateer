//! Durable task generations for guarded scheduling. Never reset generations on
//! deletion/recreation or on a change followed by a restoration. Reads of older
//! homes omit references until the normal writable-store migration has run.
use crate::store::Store;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskRef {
    pub origin: String,
    pub ship: String,
    pub task_id: String,
    pub revision: u64,
}

pub(super) fn init(store: &Store) -> Result<()> {
    // Repeated startup is concurrent with supervisors. Take the write lock
    // before any schema read: a deferred savepoint can acquire a stale WAL
    // snapshot and fail its first write immediately despite busy_timeout.
    let tx = store.write_transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_generations(id TEXT PRIMARY KEY,revision INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS door_work_results(
           grant_id TEXT NOT NULL, resource TEXT NOT NULL, key_hash TEXT NOT NULL,
           args_hash TEXT NOT NULL, result TEXT NOT NULL,
           PRIMARY KEY(grant_id,resource,key_hash));
         CREATE TRIGGER IF NOT EXISTS task_generation_insert AFTER INSERT ON tasks
         BEGIN INSERT INTO task_generations VALUES(NEW.id,1) ON CONFLICT(id) DO UPDATE SET revision=revision+1; END;
         CREATE TRIGGER IF NOT EXISTS task_generation_update AFTER UPDATE ON tasks
         BEGIN INSERT INTO task_generations VALUES(NEW.id,1) ON CONFLICT(id) DO UPDATE SET revision=revision+1; END;
         CREATE TRIGGER IF NOT EXISTS task_generation_delete AFTER DELETE ON tasks
         BEGIN INSERT INTO task_generations VALUES(OLD.id,1) ON CONFLICT(id) DO UPDATE SET revision=revision+1; END;
         CREATE TRIGGER IF NOT EXISTS task_generation_schedule_insert AFTER INSERT ON task_schedule
         BEGIN INSERT INTO task_generations VALUES(NEW.task_id,1) ON CONFLICT(id) DO UPDATE SET revision=revision+1; END;
         CREATE TRIGGER IF NOT EXISTS task_generation_schedule_update AFTER UPDATE ON task_schedule
         BEGIN INSERT INTO task_generations VALUES(NEW.task_id,1) ON CONFLICT(id) DO UPDATE SET revision=revision+1; END;
         CREATE TRIGGER IF NOT EXISTS task_generation_schedule_delete AFTER DELETE ON task_schedule
         BEGIN INSERT INTO task_generations VALUES(OLD.task_id,1) ON CONFLICT(id) DO UPDATE SET revision=revision+1; END;
         CREATE TRIGGER IF NOT EXISTS task_generation_charter AFTER UPDATE ON charters
         BEGIN INSERT INTO task_generations SELECT id,1 FROM tasks WHERE charter_id=NEW.id
           ON CONFLICT(id) DO UPDATE SET revision=revision+1; END;
         INSERT OR IGNORE INTO metadata(key,value) VALUES('work_action_schema','1');",
    )?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema_installation_uses_immediate_locking_and_is_structurally_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let version: i64 = store
            .connection()
            .pragma_query_value(None, "schema_version", |r| r.get(0))
            .unwrap();
        for _ in 0..2 {
            let before = crate::store::WRITE_TRANSACTIONS.with(|count| count.get());
            init(&store).unwrap();
            assert_eq!(
                crate::store::WRITE_TRANSACTIONS.with(|count| count.get()),
                before + 1,
                "startup must use the Store's IMMEDIATE admission, not a deferred read/write upgrade"
            );
            assert_eq!(
                store
                    .connection()
                    .pragma_query_value::<i64, _>(None, "schema_version", |r| r.get(0))
                    .unwrap(),
                version
            );
        }
    }
}

pub(crate) fn get(store: &Store, ship: &str, id: &str) -> Result<Option<TaskRef>> {
    let available: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='work_action_schema' AND value='1') AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='task_generations')",
        [],
        |r| r.get(0),
    )?;
    if !available {
        return Ok(None);
    }
    let revision: Option<u64> = store.connection().query_row(
        "SELECT COALESCE(g.revision,0) FROM tasks t LEFT JOIN task_generations g ON g.id=t.id WHERE t.id=?1 AND t.owner=?2",
        params![id, ship], |r| r.get(0)).optional()?;
    revision
        .map(|revision| {
            Ok(TaskRef {
                origin: crate::snapshot::inspection::origin(store)?,
                ship: ship.into(),
                task_id: id.into(),
                revision,
            })
        })
        .transpose()
}
