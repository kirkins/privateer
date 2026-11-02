//! Seed historical shapes, never an installed registry.
use super::*;
use std::collections::BTreeMap;

type Column = (String, String, bool, Option<String>, i64);

fn desk_columns(c: &Connection) -> BTreeMap<String, Vec<Column>> {
    let tables: Vec<String> = c
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name GLOB 'desk_*' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    tables
        .into_iter()
        .map(|table| {
            // ALTER appends columns, so compare by name rather than cid.
            let columns = c
                .prepare("SELECT name,type,\"notnull\",dflt_value,pk FROM pragma_table_info(?1) ORDER BY name")
                .unwrap()
                .query_map([&table], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            (table, columns)
        })
        .collect()
}

fn seed_legacy(c: &Connection, complete: bool) {
    if complete {
        crate::machines::init(c).unwrap();
        c.execute_batch("DROP TABLE desk_replay_link_retry; DROP TABLE desk_source_cleanup;")
            .unwrap();
    }
    c.execute_batch(
        "CREATE TABLE desk_replay_link_retry(
         binding TEXT NOT NULL,source TEXT NOT NULL,cursor INTEGER NOT NULL,
         PRIMARY KEY(binding,source));
         INSERT INTO desk_replay_link_retry(binding,source,cursor) VALUES('alpha','beta',17);
         CREATE TABLE desk_source_cleanup(
         domain TEXT NOT NULL,source TEXT NOT NULL,scan_seq INTEGER NOT NULL,
         pruned_through INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(domain,source));
         INSERT INTO desk_source_cleanup(domain,source,scan_seq,pruned_through) VALUES('alpha','beta',7,3);",
    )
    .unwrap();
}

fn assert_upgraded(c: &Connection) {
    assert!(retention::replay::ready(c).unwrap());
    let progress: (i64, i64, i64) = c
        .query_row(
            "SELECT scan_seq,pruned_through,upper_seq FROM desk_source_cleanup
             WHERE domain='alpha' AND source='beta'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(progress, (7, 3, 0));
    assert_eq!(count(c, "desk_replay_link_retry"), 0);
    // Exercise the current named-column write/key, not just table existence.
    c.execute_batch(
        "INSERT INTO desk_replay_link_retry(domain,binding,exposure,source,cursor)
         VALUES('alpha','beta','gamma','beta',11);",
    )
    .unwrap();
}

#[test]
fn legacy_desk_shapes_upgrade_match_fresh_and_reopen_without_writes() {
    let fresh = Connection::open_in_memory().unwrap();
    crate::machines::init(&fresh).unwrap();
    let expected = desk_columns(&fresh);
    // Both a partial historical install and an otherwise-current registry
    // must migrate. The latter catches readiness checks that only count tables.
    for (complete, current_table) in [false, true].into_iter().flat_map(|complete| {
        [
            None,
            Some("desk_source_cleanup"),
            Some("desk_replay_link_retry"),
        ]
        .into_iter()
        .map(move |table| (complete, table))
    }) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alpha.sqlite");
        let c = Connection::open(&path).unwrap();
        seed_legacy(&c, complete);
        // Upgrade each old table alone as well as both together, so one
        // readiness guard cannot accidentally hide the other's absence.
        if let Some(table) = current_table {
            let sql: String = fresh
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE name=?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            c.execute_batch(&format!("DROP TABLE {table}; {sql};"))
                .unwrap();
            if table == "desk_source_cleanup" {
                c.execute_batch("INSERT INTO desk_source_cleanup(domain,source,scan_seq,pruned_through) VALUES('alpha','beta',7,3)").unwrap();
            }
        }
        crate::machines::init(&c).unwrap();
        assert_eq!(desk_columns(&c), expected);
        assert_upgraded(&c);
        drop(c);

        let c = Connection::open(&path).unwrap();
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        c.busy_timeout(std::time::Duration::ZERO).unwrap();
        let version: i64 = c
            .query_row("PRAGMA schema_version", [], |r| r.get(0))
            .unwrap();
        let changes = c.total_changes();
        // This succeeds only if the second init takes the read-only fast path.
        init(&c).unwrap();
        assert_eq!(c.total_changes(), changes);
        assert_eq!(
            c.query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            version
        );
        assert_eq!(desk_columns(&c), expected);
        assert_eq!(count(&c, "desk_replay_link_retry"), 1);
        writer.execute_batch("ROLLBACK").unwrap();
    }
}

#[test]
fn legacy_desk_shape_repairs_roll_back_together_on_later_failure() {
    let c = Connection::open_in_memory().unwrap();
    seed_legacy(&c, true);
    let before = desk_columns(&c);
    // returns::init runs after both repairs. Fail there to prove the ALTER,
    // DROP/CREATE and discarded retry rows share the outer transaction.
    c.execute_batch(
        "CREATE TRIGGER alpha_fail BEFORE INSERT ON desk_replay_budget
         WHEN NEW.name='desk_replay_cut_return'
         BEGIN SELECT RAISE(ABORT,'alpha injected failure'); END;",
    )
    .unwrap();
    assert!(
        init(&c)
            .unwrap_err()
            .to_string()
            .contains("alpha injected failure")
    );
    assert_eq!(desk_columns(&c), before);
    assert_eq!(count(&c, "desk_replay_link_retry"), 1);
    assert_eq!(
        c.query_row("SELECT cursor FROM desk_replay_link_retry", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        17
    );
    c.execute_batch("DROP TRIGGER alpha_fail").unwrap();
    init(&c).unwrap();
    assert_upgraded(&c);
}

#[test]
fn concurrent_legacy_shape_upgrade_rechecks_under_writer_lock() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
    use std::sync::{Arc, Barrier};

    let dir = tempfile::tempdir().unwrap();
    let version = |c: &Connection| {
        c.query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0))
            .unwrap()
    };
    let serial = Connection::open_in_memory().unwrap();
    seed_legacy(&serial, true);
    let before = version(&serial);
    init(&serial).unwrap();
    let delta = version(&serial) - before;

    let path = dir.path().join("alpha.sqlite");
    let c = Connection::open(&path).unwrap();
    seed_legacy(&c, true);
    let before = version(&c);
    let barrier = Arc::new(Barrier::new(2));
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let barrier = barrier.clone();
                let path = &path;
                scope.spawn(move || {
                    let c = Connection::open(path).unwrap();
                    c.busy_timeout(std::time::Duration::from_secs(10)).unwrap();
                    // Both have observed the old shape before either BEGIN.
                    c.authorizer(Some(move |ctx: AuthContext<'_>| {
                        if matches!(
                            ctx.action,
                            AuthAction::Transaction {
                                operation: TransactionOperation::Begin
                            }
                        ) {
                            barrier.wait();
                        }
                        Authorization::Allow
                    }));
                    init(&c).unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
    });
    // Repair DDL runs once, not once per opener that saw the old shape.
    assert_eq!(version(&c), before + delta);
    assert_eq!(desk_columns(&c), desk_columns(&serial));
    assert_upgraded(&c);
}

#[test]
fn migrated_cleanup_zero_uses_only_the_acknowledged_upper() {
    let c = db();
    let m = manifest();
    capture(&c, &m, &fact(0));
    let mut closed = fact(1);
    closed.unread = false;
    closed.pending = false;
    capture(&c, &m, &closed);
    capture(&c, &m, &fact(2));
    let p = one_page(&c, &m);
    let mut later = fact(3);
    later.unread = false;
    later.pending = false;
    capture(&c, &m, &later);
    c.execute_batch(
        "DROP TABLE desk_source_cleanup;
         CREATE TABLE desk_source_cleanup(domain TEXT NOT NULL,source TEXT NOT NULL,
         scan_seq INTEGER NOT NULL,pruned_through INTEGER NOT NULL DEFAULT 0,
         PRIMARY KEY(domain,source));",
    )
    .unwrap();
    c.execute(
        "INSERT INTO desk_source_cleanup(domain,source,scan_seq,pruned_through) VALUES(?1,?2,1,0)",
        params![m.domain, ALPHA],
    )
    .unwrap();
    init(&c).unwrap();
    retention::acknowledged(
        &c,
        &m,
        &p,
        std::time::Instant::now() + std::time::Duration::from_secs(3600),
    )
    .unwrap();
    let retained: Vec<i64> = c
        .prepare("SELECT seq FROM desk_source_event ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    // Live facts survive, as does the disposable row beyond the checkpoint.
    assert_eq!(retained, [1, 3, 4]);
}

#[test]
fn retry_shape_guard_checks_all_columns_and_primary_key() {
    for shape in [
        // Contains both newer columns, but is still missing cursor.
        "domain TEXT,binding TEXT,exposure TEXT,source TEXT,PRIMARY KEY(domain,exposure,source)",
        // All five names, but a development key can alias different exposures.
        "domain TEXT,binding TEXT,exposure TEXT,source TEXT,cursor INTEGER,PRIMARY KEY(binding,source)",
        // Extra columns also mean this is not the expected shape.
        "domain TEXT,binding TEXT,exposure TEXT,source TEXT,cursor INTEGER,alpha TEXT,PRIMARY KEY(domain,exposure,source)",
    ] {
        let c = db();
        c.execute_batch(&format!(
            "DROP TABLE desk_replay_link_retry; CREATE TABLE desk_replay_link_retry({shape});"
        ))
        .unwrap();
        assert!(!retention::replay::ready(&c).unwrap());
        init(&c).unwrap();
        assert!(retention::replay::ready(&c).unwrap());
        let fresh = db();
        assert_eq!(desk_columns(&c), desk_columns(&fresh));
    }
}
