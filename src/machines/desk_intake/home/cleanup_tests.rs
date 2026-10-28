use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn indexed_home_cleanup_scans_a_fixed_batch_even_with_thousands_of_live_facts() {
    for size in [600, 3000] {
        let dir = tempfile::tempdir().unwrap();
        let h = home(&dir.path().join("home"));
        let r = registry(&dir.path().join("registry.sqlite"));
        let m = manifest();
        let tx = h.connection().unchecked_transaction().unwrap();
        for n in 0..size {
            let f = HomeFact {
                mailbox: "00000000-0000-4000-8000-00000000000c".into(),
                id: format!("fact-{n}"),
                provenance: Provenance::Origin(
                    MailIdentity::new(
                        ALPHA,
                        "00000000-0000-4000-8000-00000000000c",
                        &format!("fact-{n}"),
                    )
                    .unwrap(),
                ),
                content: json!({}),
                state: Some("open".into()),
                unread: true,
                pending: true,
            };
            tx.execute(
                "INSERT INTO mail_source_import(intent,event,receipt) VALUES(?1,?2,'proof')",
                params![
                    uuid::Uuid::new_v4().to_string(),
                    serde_json::to_string(&f).unwrap()
                ],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let steps = Arc::new(AtomicUsize::new(0));
        let counter = steps.clone();
        h.connection().progress_handler(
            1,
            Some(move || {
                counter.fetch_add(1, Ordering::Relaxed);
                false
            }),
        );
        reclaim_imports(h.connection(), &r, &m, ALPHA, size, None).unwrap();
        h.connection().progress_handler(0, None::<fn() -> bool>);
        assert!(
            steps.load(Ordering::Relaxed) < 15_000,
            "size={size}, VM steps={}",
            steps.load(Ordering::Relaxed)
        );
        assert_eq!(
            h.connection()
                .query_row("SELECT scan_seq FROM mail_source_cleanup", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            50
        );
        assert_eq!(count(h.connection(), "mail_source_import"), size);
        assert_eq!(count(&r, "desk_intake_gap"), 0);
    }
}

#[test]
fn cleanup_busy_and_vm_interrupt_are_quiet_and_retry_without_losing_custody() {
    for failure in ["home", "registry", "interrupt"] {
        let dir = tempfile::tempdir().unwrap();
        let hp = dir.path().join("home");
        let rp = dir.path().join("registry.sqlite");
        let mut h = home(&hp);
        let r = registry(&rp);
        let m = manifest();
        crate::mail::dispatch(
            &mut h,
            "mail.send",
            json!({"id":"safe","from":"captain","to":"survey","kind":"notice","body":"safe"}),
        )
        .unwrap();
        capture(h.connection(), &r, &m, ALPHA).unwrap();
        h.connection()
            .busy_timeout(std::time::Duration::ZERO)
            .unwrap();
        r.busy_timeout(std::time::Duration::ZERO).unwrap();
        let lock = Connection::open(if failure == "home" {
            hp.join("privateer.sqlite")
        } else {
            rp.clone()
        })
        .unwrap();
        if failure == "interrupt" {
            // Force SQLITE_INTERRUPT inside cleanup, independently of clocks.
            h.connection().progress_handler(1, Some(|| true));
        } else {
            lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        }
        reclaim_imports(h.connection(), &r, &m, ALPHA, 1, None).unwrap();
        h.connection().progress_handler(0, None::<fn() -> bool>);
        if failure != "interrupt" {
            lock.execute_batch("ROLLBACK").unwrap();
        }
        assert_eq!(count(h.connection(), "mail_source_import"), 1);
        assert_eq!(count(&r, "desk_source_event"), 1);
        assert_eq!(
            r.query_row(
                "SELECT COUNT(*) FROM desk_intake_gap WHERE state='unreadable'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        reclaim_imports(h.connection(), &r, &m, ALPHA, 1, None).unwrap();
        assert_eq!(count(h.connection(), "mail_source_import"), 0);
        assert_eq!(count(&r, "desk_home_receipt"), 0);
    }
}

#[test]
fn cursor_keeps_later_supersession_proof_until_the_older_live_row_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = home(&dir.path().join("home"));
    let r = registry(&dir.path().join("registry.sqlite"));
    let m = manifest();
    crate::mail::dispatch(
        &mut h,
        "mail.send",
        json!({"id":"notice","from":"survey","to":"captain","kind":"notice","body":"held"}),
    )
    .unwrap();
    capture(h.connection(), &r, &m, ALPHA).unwrap();
    // Visit the live row before the acknowledgement arrives.
    capture(h.connection(), &r, &m, ALPHA).unwrap();
    crate::mail::dismiss(&h, "notice", "finished").unwrap();
    capture(h.connection(), &r, &m, ALPHA).unwrap();
    // Cursor reaches the later disposable row first. It is still needed as
    // the supersession proof for the older live observation.
    h.connection()
        .execute("UPDATE mail_source_cleanup SET scan_seq=1", [])
        .unwrap();
    capture(h.connection(), &r, &m, ALPHA).unwrap();
    assert_eq!(count(h.connection(), "mail_source_import"), 2);
    for _ in 0..5 {
        capture(h.connection(), &r, &m, ALPHA).unwrap();
    }
    assert_eq!(count(h.connection(), "mail_source_import"), 0);
    assert_eq!(count(h.connection(), "mail"), 1);
}

#[test]
fn writable_startup_upgrades_existing_cleanup_schema_without_touching_mail() {
    let dir = tempfile::tempdir().unwrap();
    let h = home(&dir.path().join("home"));
    let r = registry(&dir.path().join("registry.sqlite"));
    h.connection()
        .execute_batch(
            "DROP TABLE mail_source_cleanup;
        DROP INDEX mail_source_import_origin_seq; DROP INDEX mail_source_import_local_seq;
        DROP INDEX mail_source_import_pending; DROP INDEX mail_source_import_receipted;",
        )
        .unwrap();
    r.execute_batch("DROP TABLE desk_source_cleanup; DROP INDEX desk_source_origin_seq;")
        .unwrap();
    source::init(h.connection()).unwrap();
    super::super::super::init(&r).unwrap();
    assert_eq!(count(h.connection(), "mail"), 0);
    assert_eq!(
        r.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='desk_source_origin_seq'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(h.connection().query_row("SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'mail_source_import_%' AND type='index'", [], |r| r.get::<_,i64>(0)).unwrap(), 4);
}
