use super::*;
use std::sync::{Arc, Barrier};
use std::time::Duration;

fn fixture(path: &Path, old_schema: bool) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.busy_timeout(Duration::from_secs(10)).unwrap();
    if old_schema {
        conn.execute_batch(
            "CREATE TABLE machines(name TEXT PRIMARY KEY,
               role TEXT NOT NULL CHECK(role IN ('collects','collected')),
               peer TEXT NOT NULL, known_as TEXT, added_ms INTEGER NOT NULL, refreshed_ms INTEGER);
             CREATE TABLE machine_ships(machine TEXT NOT NULL, name TEXT NOT NULL,
               scope TEXT NOT NULL, PRIMARY KEY(machine, name));
             CREATE TABLE fleet_snapshots(machine TEXT PRIMARY KEY,
               digest TEXT NOT NULL, changed_ms INTEGER NOT NULL, as_of_ms INTEGER NOT NULL,
               via TEXT NOT NULL, body TEXT NOT NULL);
             INSERT INTO machines(name, role, peer, added_ms) VALUES('peer', 'collects', '{}', 1);
             INSERT INTO machine_ships VALUES('peer', 'sample', 'fixture');
             INSERT INTO fleet_snapshots VALUES('peer', 'digest', 1, 1, 'fixture', '{}');",
        )
        .unwrap();
    }
    conn
}

fn version(conn: &Connection) -> i64 {
    conn.query_row("PRAGMA schema_version", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn registration_survives_full_or_failing_home_inventory_with_durable_gap() {
    for fail in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("registry.sqlite");
        let conn = open_registry(&path, Duration::ZERO).unwrap();
        let old = tmp.path().join("old");
        let new = tmp.path().join("new");
        // Retired/replaced paths must also use best-effort capture.
        conn.execute(
            "INSERT INTO ships VALUES('survey',?1,'alpha',1,1)",
            [old.to_string_lossy().as_ref()],
        )
        .unwrap();
        if fail {
            conn.execute_batch("DROP TABLE desk_home_inventory; CREATE TABLE desk_home_inventory(home TEXT PRIMARY KEY CHECK(0));").unwrap();
        } else {
            conn.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10000) INSERT INTO desk_home_inventory SELECT 'retained-'||x FROM n;").unwrap();
        }
        register_in(&conn, "survey", &new, "alpha").unwrap();
        // A fresh registration must succeed too, not just replacement.
        register_in(&conn, "relay", &tmp.path().join("relay"), "alpha").unwrap();
        drop(conn);
        let reader =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let home: String = reader
            .query_row(
                "SELECT home FROM ships WHERE name='survey' AND retired_ms IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(home, new.to_string_lossy());
        let count: i64 = reader
            .query_row(
                "SELECT COUNT(*) FROM ships WHERE retired_ms IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
        let gap: String = reader
            .query_row(
                "SELECT state FROM desk_home_inventory_gap WHERE only=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(gap, "history_gap");
    }
}

#[test]
fn migrated_registry_open_needs_no_write_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("registry.sqlite");
    let mut writer = open_registry(&path, Duration::ZERO).unwrap();
    let before = version(&writer);
    let _tx = writer
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let reader = open_registry(&path, Duration::ZERO).unwrap();
    assert_eq!(version(&reader), before);
}

#[test]
fn concurrent_registry_opens_create_or_migrate_exactly_once() {
    for old_schema in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        // A serial open of the same starting schema defines exactly one set
        // of schema changes, including all initializers on the open path.
        let serial_path = tmp.path().join("serial.sqlite");
        let serial = fixture(&serial_path, old_schema);
        let initial_version = version(&serial);
        let migrated = open_registry(&serial_path, Duration::from_secs(10)).unwrap();
        let changes = version(&migrated) - initial_version;
        assert!(changes > 0);

        let path = tmp.path().join("concurrent.sqlite");
        let conn = fixture(&path, old_schema);
        assert_eq!(version(&conn), initial_version);
        let barrier = Arc::new(Barrier::new(8));
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let barrier = Arc::clone(&barrier);
                    let path = &path;
                    scope.spawn(move || {
                        barrier.wait();
                        open_registry(path, Duration::from_secs(10)).unwrap();
                    })
                })
                .collect();
            for handle in handles {
                handle.join().unwrap();
            }
        });
        assert_eq!(version(&conn), initial_version + changes);
        let reopened = open_registry(&path, Duration::ZERO).unwrap();
        assert_eq!(version(&reopened), initial_version + changes);
        if old_schema {
            let defaults: (Option<i64>, i64, String, String) = conn
                .query_row(
                    "SELECT next_contact_ms, failures, stows, machine_ships.requires
                     FROM machines JOIN machine_ships ON machines.name=machine_ships.machine",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .unwrap();
            assert_eq!(defaults, (None, 0, "{}".into(), "[]".into()));
            let received: Option<i64> = conn
                .query_row("SELECT received_ms FROM fleet_snapshots", [], |r| r.get(0))
                .unwrap();
            assert_eq!(received, None);
        }
    }
}
