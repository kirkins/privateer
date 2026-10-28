use super::*;

#[test]
fn event_and_home_retry_proof_rollback_together_before_any_reclamation() {
    let dir = tempfile::tempdir().unwrap();
    let hp = dir.path().join("home");
    let rp = dir.path().join("registry.sqlite");
    let mut h = home(&hp);
    let r = registry(&rp);
    let m = manifest();
    crate::mail::dispatch(&mut h,"mail.send",json!({"id":"safe-import","from":"captain","to":"survey","kind":"notice","body":"retained"})).unwrap();
    source::snapshot(h.connection(), &format!("{}:{ALPHA}", binding(&m).unwrap())).unwrap();
    let (intent, event): (String, String) = h
        .connection()
        .query_row("SELECT intent,event FROM mail_source_import", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    // Deterministically interrupt AFTER event insertion, at receipt insertion.
    r.execute_batch("CREATE TRIGGER fail_receipt BEFORE INSERT ON desk_home_receipt BEGIN SELECT RAISE(ABORT,'receipt interrupted'); END").unwrap();
    assert!(
        import_one(&r, &m, ALPHA, &intent, &event)
            .unwrap_err()
            .to_string()
            .contains("receipt interrupted")
    );
    assert_eq!(count(&r, "desk_source_event"), 0);
    assert_eq!(count(&r, "desk_home_receipt"), 0);
    // A final receipt and reclamation cannot turn a failed import into a
    // forgotten intent. Simulate process death/reopen before the retry.
    let cp = checkpoint(&r, &m, ALPHA, &uuid::Uuid::new_v4().to_string()).unwrap();
    let p = page(&r, &m, &cp, 0, ITEMS, BYTES).unwrap();
    let desk = registry(&dir.path().join("desk.sqlite"));
    accept(
        &desk,
        &m,
        &p,
        &Clock {
            session: "desk",
            wall_ms: 100,
            tick_ms: 1,
        },
    )
    .unwrap();
    super::super::super::retention::acknowledged(
        &r,
        &m,
        &p,
        std::time::Instant::now() + std::time::Duration::from_secs(10),
    )
    .unwrap();
    drop(r);
    let r = registry(&rp);
    r.execute_batch("DROP TRIGGER fail_receipt").unwrap();
    let receipt = import_one(&r, &m, ALPHA, &intent, &event).unwrap();
    assert_eq!(import_one(&r, &m, ALPHA, &intent, &event).unwrap(), receipt);
    assert_eq!(count(&r, "desk_source_event"), 1);
    assert_eq!(count(&r, "desk_home_receipt"), 1);
    let seq: i64 = r
        .query_row("SELECT seq FROM desk_source_event", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        seq, 1,
        "rolled back insertion must not consume/reappend an order"
    );
}

#[test]
fn cleanup_filters_skipped_provenance_before_limit_and_expiration_is_quiet() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = home(&dir.path().join("home"));
    let r = registry(&dir.path().join("registry.sqlite"));
    let m = manifest();
    capture(h.connection(), &r, &m, ALPHA).unwrap();
    let mailbox: String = h
        .connection()
        .query_row("SELECT mailbox FROM mail_source_health", [], |r| r.get(0))
        .unwrap();
    for n in 0..60 {
        let event = serde_json::to_string(&HomeFact {
            mailbox: mailbox.clone(),
            id: format!("unresolved-{n}"),
            provenance: Provenance::Unresolved,
            content: json!({}),
            state: None,
            unread: false,
            pending: false,
        })
        .unwrap();
        let intent = uuid::Uuid::new_v4().to_string();
        let receipt = import_one(&r, &m, ALPHA, &intent, &event).unwrap();
        h.connection()
            .execute(
                "INSERT INTO mail_source_import(intent,event,receipt) VALUES(?1,?2,?3)",
                params![intent, event, receipt],
            )
            .unwrap();
    }
    crate::mail::dispatch(&mut h,"mail.send",json!({"id":"after-skipped","from":"captain","to":"survey","kind":"notice","body":"retained"})).unwrap();
    assert_eq!(
        capture(h.connection(), &r, &m, ALPHA).unwrap()["imported"],
        1
    );
    let upper: i64 = h
        .connection()
        .query_row("SELECT MAX(seq) FROM mail_source_import", [], |r| r.get(0))
        .unwrap();
    reclaim_imports(
        h.connection(),
        &r,
        &m,
        ALPHA,
        upper,
        Some(std::time::Instant::now()),
    )
    .unwrap();
    assert_eq!(count(h.connection(), "mail_source_import"), 61);
    capture(h.connection(), &r, &m, ALPHA).unwrap();
    assert_eq!(count(h.connection(), "mail_source_import"), 60);
    assert_eq!(count(&r, "desk_home_receipt"), 60);
    assert_eq!(count(h.connection(), "mail_source_outbox"), 0);
}

#[test]
fn pending_imports_run_before_cleanup_and_cleanup_batches_both_commits() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = home(&dir.path().join("home"));
    let r = registry(&dir.path().join("registry.sqlite"));
    let m = manifest();
    for n in 0..50 {
        crate::mail::dispatch(&mut h,"mail.send",json!({"id":format!("safe-{n}"),"from":"captain","to":"survey","kind":"notice","body":"retained"})).unwrap();
    }
    capture(h.connection(), &r, &m, ALPHA).unwrap();
    crate::mail::dispatch(&mut h,"mail.send",json!({"id":"new-pending","from":"captain","to":"survey","kind":"notice","body":"retained"})).unwrap();
    // Cleanup fails halfway through either store. Each store's batch must be
    // atomic, while the NEW import must already have acquired custody.
    r.execute_batch("CREATE TRIGGER fail_batch BEFORE DELETE ON desk_home_receipt WHEN OLD.intent=(SELECT intent FROM desk_home_receipt ORDER BY rowid LIMIT 1 OFFSET 20) BEGIN SELECT RAISE(ABORT,'batch interrupted'); END").unwrap();
    assert!(capture(h.connection(), &r, &m, ALPHA).is_err());
    assert_eq!(count(&r, "desk_source_event"), 51);
    assert_eq!(count(&r, "desk_home_receipt"), 51);
    assert_eq!(count(h.connection(), "mail_source_import"), 51);
    r.execute_batch("DROP TRIGGER fail_batch").unwrap();
    h.connection().execute_batch("CREATE TRIGGER fail_home_batch BEFORE DELETE ON mail_source_import WHEN OLD.seq=21 BEGIN SELECT RAISE(ABORT,'home batch interrupted'); END").unwrap();
    assert!(capture(h.connection(), &r, &m, ALPHA).is_err());
    assert_eq!(count(&r, "desk_home_receipt"), 1);
    assert_eq!(count(h.connection(), "mail_source_import"), 51);
    h.connection()
        .execute_batch("DROP TRIGGER fail_home_batch")
        .unwrap();
    capture(h.connection(), &r, &m, ALPHA).unwrap();
    capture(h.connection(), &r, &m, ALPHA).unwrap();
    assert_eq!(count(&r, "desk_source_event"), 51);
    assert_eq!(count(&r, "desk_home_receipt"), 0);
    assert_eq!(count(h.connection(), "mail_source_import"), 0);
}
