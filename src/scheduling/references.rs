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
           args_hash TEXT NOT NULL, result TEXT NOT NULL, created_ms INTEGER NOT NULL,
           PRIMARY KEY(grant_id,resource,key_hash));
         CREATE TRIGGER IF NOT EXISTS task_generation_insert AFTER INSERT ON tasks
         BEGIN INSERT INTO task_generations VALUES(NEW.id,1) ON CONFLICT(id) DO UPDATE SET revision=revision+1; END;
         CREATE TRIGGER IF NOT EXISTS task_generation_update AFTER UPDATE ON tasks
         BEGIN INSERT INTO task_generations VALUES(NEW.id,1) ON CONFLICT(id) DO UPDATE SET revision=revision+1; END;
         CREATE TRIGGER IF NOT EXISTS task_generation_delete AFTER DELETE ON tasks
         BEGIN INSERT INTO task_generations VALUES(OLD.id,1) ON CONFLICT(id) DO UPDATE SET revision=revision+1; END;
         CREATE TRIGGER IF NOT EXISTS task_generation_schedule_insert AFTER INSERT ON task_schedule
         BEGIN INSERT INTO task_generations VALUES(NEW.task_id,1) ON CONFLICT(id) DO UPDATE SET revision=revision+1; END;
         CREATE TRIGGER IF NOT EXISTS task_generation_schedule_delete AFTER DELETE ON task_schedule
         BEGIN INSERT INTO task_generations VALUES(OLD.task_id,1) ON CONFLICT(id) DO UPDATE SET revision=revision+1; END;
         CREATE TRIGGER IF NOT EXISTS task_generation_charter AFTER UPDATE ON charters
         BEGIN INSERT INTO task_generations SELECT id,1 FROM tasks WHERE charter_id=NEW.id
           ON CONFLICT(id) DO UPDATE SET revision=revision+1; END;
         CREATE TRIGGER IF NOT EXISTS task_generation_schedule_update_v2 AFTER UPDATE ON task_schedule
         WHEN OLD.task_id IS NOT NEW.task_id
           OR json_extract(OLD.record,'$.state') IS NOT json_extract(NEW.record,'$.state')
           OR json_extract(OLD.record,'$.wait') IS NOT json_extract(NEW.record,'$.wait')
           OR json_extract(OLD.record,'$.offer_id') IS NOT json_extract(NEW.record,'$.offer_id')
           OR json_extract(OLD.record,'$.holder') IS NOT json_extract(NEW.record,'$.holder')
           OR json_extract(OLD.record,'$.job_id') IS NOT json_extract(NEW.record,'$.job_id')
           OR json_extract(OLD.record,'$.owner') IS NOT json_extract(NEW.record,'$.owner')
           OR json_extract(OLD.record,'$.revision') IS NOT json_extract(NEW.record,'$.revision')
         BEGIN INSERT INTO task_generations VALUES(NEW.task_id,1) ON CONFLICT(id) DO UPDATE SET revision=revision+1; END;
         DROP TRIGGER IF EXISTS task_generation_schedule_update;",
    )?;
    let has_created: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('door_work_results') WHERE name='created_ms')",
        [], |r| r.get(0))?;
    if !has_created {
        // Legacy successes receive a full retry window starting at migration,
        // not an invented pre-upgrade age. Normal startup alone migrates homes.
        tx.execute_batch(&format!(
            "ALTER TABLE door_work_results ADD COLUMN created_ms INTEGER NOT NULL DEFAULT {}",
            Store::now_ms()
        ))?;
    }
    tx.execute("INSERT INTO metadata(key,value) VALUES('work_action_schema','2') ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE value!=excluded.value", [])?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn get(store: &Store, ship: &str, id: &str) -> Result<Option<TaskRef>> {
    let available: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='work_action_schema' AND value='2') AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='task_generations')",
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn existing_homes_replace_legacy_trigger_and_preserve_retry_results_on_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.connection().execute_batch("DROP TRIGGER task_generation_schedule_update_v2;
            CREATE TRIGGER task_generation_schedule_update AFTER UPDATE ON task_schedule
            BEGIN INSERT INTO task_generations VALUES(NEW.task_id,1) ON CONFLICT(id) DO UPDATE SET revision=revision+1; END;
            ALTER TABLE door_work_results DROP COLUMN created_ms;
            INSERT INTO door_work_results VALUES('alpha','door:alpha','key','hash','saved');
            UPDATE metadata SET value='1' WHERE key='work_action_schema';").unwrap();
        let before = Store::now_ms();
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        let created: i64 = store
            .connection()
            .query_row(
                "SELECT created_ms FROM door_work_results WHERE result='saved'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            created >= before,
            "legacy records need a full retry window after migration"
        );
        let triggers: (bool, bool) = store
            .connection()
            .query_row(
                "SELECT
            EXISTS(SELECT 1 FROM sqlite_master WHERE name='task_generation_schedule_update'),
            EXISTS(SELECT 1 FROM sqlite_master WHERE name='task_generation_schedule_update_v2')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(triggers, (false, true));
        store.connection().execute_batch("INSERT INTO charters(id,data) VALUES('survey','{}');
            INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('alpha','survey','survey','alpha','open',0);
            INSERT INTO task_schedule VALUES('alpha','{\"state\":\"ready\",\"owner\":\"survey\",\"revision\":1}');").unwrap();
        let original = get(&store, "survey", "alpha").unwrap();
        store.connection().execute("UPDATE task_schedule SET record=json_set(record,'$.last_checked_ms',123,'$.lease_until_ms',456,'$.reason','poll')", []).unwrap();
        assert_eq!(get(&store, "survey", "alpha").unwrap(), original);
        for (field, value) in [
            ("state", serde_json::json!("claimed")),
            ("wait", serde_json::json!({"kind":"task","ref":"beta"})),
            ("offer_id", serde_json::json!("beta")),
            ("holder", serde_json::json!("gamma")),
            ("job_id", serde_json::json!("alpha")),
            ("owner", serde_json::json!("relay")),
            ("revision", serde_json::json!(2)),
        ] {
            let before = get(&store, "survey", "alpha").unwrap().unwrap();
            store
                .connection()
                .execute(
                    "UPDATE task_schedule SET record=json_set(record,?1,json(?2))",
                    params![format!("$.{field}"), value.to_string()],
                )
                .unwrap();
            assert!(
                get(&store, "survey", "alpha").unwrap().unwrap().revision > before.revision,
                "{field}"
            );
        }
        let version: i64 = store
            .connection()
            .pragma_query_value(None, "schema_version", |r| r.get(0))
            .unwrap();
        init(&store).unwrap();
        assert_eq!(
            store
                .connection()
                .pragma_query_value::<i64, _>(None, "schema_version", |r| r.get(0))
                .unwrap(),
            version
        );
    }

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
