use super::*;

#[test]
fn current_active_home_enrollment_needs_no_home_writer_lock() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("alpha");
    let mut h = setup(&path);
    send(&mut h, "retained");
    let r = registry();
    let m = manifest();
    enroll(&path, &r, &m, 100).unwrap();
    drain(&path, h.connection(), &r, &m);
    let changes = h.connection().total_changes();
    let writer = Connection::open(path.join("privateer.sqlite")).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    // Keep a real competing writer locked for the whole call. Success proves
    // no home write lock was requested, without an elapsed-time assertion.
    enroll(&path, &r, &m, 101).unwrap();
    assert!(enrollment::published(h.connection()).unwrap());
    assert_eq!(h.connection().total_changes(), changes);
    assert_eq!(count(h.connection(), "mail"), 1);
    writer.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn another_homes_unresolved_receipt_does_not_rotate_quiet_baseline_cleanup() {
    let t = tempfile::tempdir().unwrap();
    let alpha = t.path().join("alpha");
    let beta = t.path().join("beta");
    let mut a = setup(&alpha);
    let mut b = setup(&beta);
    for n in 0..61 {
        send(&mut a, &format!("retained-{n}"));
    }
    send(&mut b, "unresolved");
    b.connection()
        .execute(
            "UPDATE mail_source_origin SET provenance='\"Unresolved\"' WHERE id='unresolved'",
            [],
        )
        .unwrap();
    let r = registry();
    let m = manifest();
    for (path, h) in [(&alpha, a.connection()), (&beta, b.connection())] {
        enroll(path, &r, &m, 100).unwrap();
        drain(path, h, &r, &m);
    }
    let pending = || -> i64 {
        r.query_row("SELECT COUNT(*) FROM desk_home_receipt WHERE domain=?1 AND source=?2 AND json_extract(event,'$.provenance')='Unresolved'", params![m.domain,SOURCE], |r| r.get(0)).unwrap()
    };
    assert_eq!(pending(), 1);
    let baseline = enrollment::open(&alpha, false).unwrap();
    assert_eq!(count(&baseline, "mail_source_import"), 61);
    // Put the cursor inside a retained live prefix so an unnecessary sweep
    // would advance it and write the sidecar, even though nothing is deleted.
    baseline
        .execute_batch(
            "INSERT OR REPLACE INTO mail_source_cleanup(only,scan_seq,upper_seq) VALUES(1,1,61)",
        )
        .unwrap();
    let version: i64 = baseline
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap();
    for _ in 0..3 {
        home::capture_ready_until(
            &alpha,
            a.connection(),
            &r,
            &m,
            SOURCE,
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(
            baseline
                .query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            version,
            "another home's receipt must not cause sidecar writes"
        );
        assert_eq!(
            baseline
                .query_row(
                    "SELECT scan_seq,upper_seq FROM mail_source_cleanup",
                    [],
                    |r| { Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)) }
                )
                .unwrap(),
            (1, 61)
        );
        assert_eq!(pending(), 1, "the other home's blocker must remain");
    }
}
