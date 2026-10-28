use super::*;

#[test]
fn dormant_retained_home_upgrades_on_raw_transport_capture_without_store_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("retired");
    let mut h = crate::store::Store::open(&path).unwrap();
    crate::mail::source::bind(h.connection(), SOURCE).unwrap();
    crate::mail::dispatch(
        &mut h,
        "ship.put",
        json!({"name":"survey","location":"here"}),
    )
    .unwrap();
    crate::mail::dispatch(
        &mut h,
        "mail.send",
        json!({
            "id":"old-unread","from":"survey","to":"captain","kind":"notice","body":"retained"
        }),
    )
    .unwrap();
    // Exactly the pre-cleanup schema: even an enrolled home may never have
    // been opened by a normal command since the executable was upgraded.
    let source = db();
    let c = contact_for_test(false);
    crate::mail::source::snapshot(
        h.connection(),
        &format!("{}:{SOURCE}", binding(&c.manifest).unwrap()),
    )
    .unwrap();
    assert!(crate::mail::source::missing(h.connection(), &["mail_source_cleanup"]).unwrap());
    drop(h);
    source
        .execute(
            "INSERT INTO desk_home_inventory(home) VALUES(?1)",
            [path.to_str().unwrap()],
        )
        .unwrap();
    let f = source_frame(&source, &c, true, Instant::now() + Duration::from_secs(10)).unwrap();
    let page = f.page.unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].fact.origin.id, "old-unread");
    let raw = Connection::open(path.join("privateer.sqlite")).unwrap();
    assert!(!crate::mail::source::missing(&raw, &["mail_source_cleanup"]).unwrap());
    assert_eq!(count(&raw, "mail"), 1);
    assert_eq!(
        source
            .query_row(
                "SELECT COUNT(*) FROM desk_intake_gap WHERE state='unreadable'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn optional_source_reclamation_busy_and_interrupt_are_quiet_before_next_barrier() {
    for failure in ["busy", "interrupt"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.sqlite");
        let source = Connection::open(&path).unwrap();
        init(&source).unwrap();
        source
            .execute_batch("CREATE TABLE ships(home TEXT)")
            .unwrap();
        source.busy_timeout(Duration::ZERO).unwrap();
        let desk = db();
        let c = contact_for_test(false);
        let owner = Contact {
            manifest: c.manifest.clone(),
            ..contact_for_test(true)
        };
        append_test_fact(&source, &c, 0);
        let deadline = Instant::now() + Duration::from_secs(10);
        let f = source_frame(&source, &c, true, deadline).unwrap();
        let old = f.page.clone().unwrap();
        receive(&desk, &owner, f).unwrap();
        receive(&source, &c, owner_frame(&desk, &owner).unwrap()).unwrap();
        let prior_gap: String = source
            .query_row("SELECT state FROM desk_intake_gap", [], |r| r.get(0))
            .unwrap();
        let lock = Connection::open(&path).unwrap();
        if failure == "busy" {
            lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        } else {
            source.progress_handler(1, Some(|| true));
        }
        retention::acknowledged(&source, &c.manifest, &old, deadline).unwrap();
        source.progress_handler(0, None::<fn() -> bool>);
        if failure == "busy" {
            lock.execute_batch("ROLLBACK").unwrap();
        }
        assert_eq!(count(&source, "desk_source_event"), 1);
        assert_eq!(
            source
                .query_row("SELECT state FROM desk_intake_gap", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            prior_gap,
            "optional cleanup must not replace the intentional membership gap"
        );
        append_test_fact(&source, &c, 1);
        let f = source_frame(&source, &c, true, deadline).unwrap();
        assert_ne!(f.page.as_ref().unwrap().checkpoint.id, old.checkpoint.id);
        receive(&desk, &owner, f).unwrap();
        assert_eq!(count(&desk, "desk_intake_fact"), 2);
    }
}
