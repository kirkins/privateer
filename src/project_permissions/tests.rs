use super::*;
use rusqlite::{Connection, TransactionBehavior};
use std::{sync::Arc, sync::Barrier, time::Duration};

fn open(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn
}

fn fixture() -> (tempfile::TempDir, Connection, Connection) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("registry.sqlite");
    let conn = open(&path);
    init(&conn).unwrap();
    let root = tmp.path().join("project");
    fs::create_dir(&root).unwrap();
    let meta = root.metadata().unwrap();
    conn.execute(
        "INSERT INTO project_permissions(project,root,device,inode,identity_time,proposal,accepted,state,ask,home)
         VALUES('sample/project',?1,?2,?3,?4,?5,?5,'accepted','consent-card','fixture')",
        params![root.to_str(), meta.dev().to_string(), meta.ino().to_string(), identity_time(&meta), json!([CARGO[0]]).to_string()],
    ).unwrap();
    let other = open(&path);
    (tmp, conn, other)
}

#[test]
fn concurrent_opens_create_or_migrate_exactly_once() {
    for old_schema in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("registry.sqlite");
        let conn = open(&path);
        if old_schema {
            init(&conn).unwrap();
            conn.execute_batch("ALTER TABLE project_permissions DROP COLUMN identity_time")
                .unwrap();
        }
        let version: i64 = conn
            .query_row("PRAGMA schema_version", [], |r| r.get(0))
            .unwrap();
        let barrier = Arc::new(Barrier::new(8));
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let barrier = Arc::clone(&barrier);
                    let path = &path;
                    scope.spawn(move || {
                        let conn = open(path);
                        barrier.wait();
                        init(&conn).unwrap();
                        assert!(has_identity_time(&conn).unwrap());
                    })
                })
                .collect();
            for handle in handles {
                handle.join().unwrap();
            }
        });
        let migrated: i64 = conn
            .query_row("PRAGMA schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            migrated,
            version + 1,
            "only one schema change per migration"
        );
    }
}

#[test]
fn migrated_open_needs_no_write_lock() {
    let (_tmp, conn, mut other) = fixture();
    conn.busy_timeout(Duration::ZERO).unwrap();
    let _writer = other
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    init(&conn).unwrap();
}

#[test]
fn launch_reads_metadata_and_grants_while_another_writer_holds_the_lock() {
    let (tmp, mut conn, mut other) = fixture();
    conn.busy_timeout(Duration::ZERO).unwrap();
    let _writer = other
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let mut checked = false;
    let rules = registry_rules(&mut conn, &tmp.path().join("project"), |root| {
        checked = true;
        fs::symlink_metadata(root)
    })
    .unwrap();
    assert!(checked);
    assert_eq!(rules, vec![CARGO[0]]);
}

#[test]
fn metadata_checks_hold_neither_read_nor_write_transactions() {
    let (tmp, mut conn, mut other) = fixture();
    other.busy_timeout(Duration::ZERO).unwrap();
    // Rollback journal mode makes commit fail if even a read transaction is
    // held by the launch. The callback is the potentially slow metadata seam.
    assert_eq!(
        other
            .query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "delete"
    );
    let rules = registry_rules(&mut conn, &tmp.path().join("project"), |root| {
        let tx = other
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        tx.execute("UPDATE project_permissions SET home='updated'", [])
            .unwrap();
        tx.commit().unwrap();
        fs::symlink_metadata(root)
    })
    .unwrap();
    assert_eq!(rules, vec![CARGO[0]]);
}

#[test]
fn changed_identity_clears_grant_and_card_and_does_not_write_again() {
    let (tmp, mut conn, mut other) = fixture();
    conn.execute(
        "UPDATE project_permissions SET identity_time='birth:old'",
        [],
    )
    .unwrap();
    let cwd = tmp.path().join("project");
    assert!(
        registry_rules(&mut conn, &cwd, |root| fs::symlink_metadata(root))
            .unwrap()
            .is_empty()
    );
    let row = ProjectGrant::get(&conn, "sample/project").unwrap().unwrap();
    assert_eq!(row.accepted, "[]");
    assert_eq!(row.state, "clear");
    assert_eq!(row.ask, None);
    conn.busy_timeout(Duration::ZERO).unwrap();
    let _writer = other
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    assert!(
        registry_rules(&mut conn, &cwd, |root| fs::symlink_metadata(root))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn concurrent_accept_is_not_cleared_or_deleted_by_stale_metadata() {
    for removed in [false, true] {
        let (tmp, mut conn, other) = fixture();
        conn.execute(
            "UPDATE project_permissions SET identity_time='birth:old'",
            [],
        )
        .unwrap();
        let cwd = tmp.path().join("project");
        let rules = registry_rules(&mut conn, &cwd, |root| {
            let meta = fs::symlink_metadata(root)?;
            // An operator accepts the current identity after the launch's
            // snapshot, before its stale observation can clear/delete it.
            other
                .execute(
                    "UPDATE project_permissions SET identity_time=?1,ask=NULL",
                    [identity_time(&meta)],
                )
                .unwrap();
            if removed {
                Err(std::io::Error::from(std::io::ErrorKind::NotFound))
            } else {
                Ok(meta)
            }
        })
        .unwrap();
        assert!(rules.is_empty(), "a stale launch must still fail closed");
        let row = ProjectGrant::get(&conn, "sample/project").unwrap().unwrap();
        assert_eq!(row.state, "accepted");
        assert_eq!(row.accepted, json!([CARGO[0]]).to_string());
        assert_eq!(row.ask, None);
    }
}

#[test]
fn observed_removal_deletes_the_unchanged_row() {
    let (tmp, mut conn, _other) = fixture();
    let cwd = tmp.path().join("project");
    fs::remove_dir(&cwd).unwrap();
    assert!(
        registry_rules(&mut conn, tmp.path(), |root| fs::symlink_metadata(root))
            .unwrap()
            .is_empty()
    );
    assert!(
        ProjectGrant::get(&conn, "sample/project")
            .unwrap()
            .is_none()
    );
}

#[test]
fn show_explains_ctime_fallback_without_weakening_identity_checks() {
    let (_tmp, conn, _other) = fixture();
    assert_eq!(identity_note(None), None);
    assert_eq!(identity_note(Some("birth:1:2")), None);
    conn.execute(
        "UPDATE project_permissions SET identity_time='change:1:2'",
        [],
    )
    .unwrap();
    let shown = show(&conn, "sample/project").unwrap();
    let note = shown["identity_note"].as_str().unwrap();
    for text in [
        "Birth time is unavailable",
        "Creating, removing or renaming",
        "project root",
        "clears consent",
        "fails closed",
        "accept again",
    ] {
        assert!(note.contains(text), "{note}");
    }
}
