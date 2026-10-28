//! Regression coverage for the b2a second review. No timing/uptime assertions.
use super::*;

#[test]
fn restored_and_recreated_homes_discard_sidecars_from_other_enrollment_instances() {
    for restore in [true, false] {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("home");
        let backup = t.path().join("pre-enrollment.sqlite");
        let mut h = setup(&path);
        h.connection()
            .execute("VACUUM INTO ?1", [backup.to_str().unwrap()])
            .unwrap();
        let r = registry();
        let m = manifest();
        let bound = format!("{}:{SOURCE}", binding(&m).unwrap());
        enrollment::begin(h.connection(), &bound).unwrap();
        let old = enrollment::nonce(h.connection(), &bound).unwrap();
        for n in 0..10 {
            send(&mut h, &format!("old-{n}"));
        }
        let baseline = enrollment::open(&path, true).unwrap();
        enrollment::init_sidecar(&baseline).unwrap();
        let upper = enrollment::freeze(
            h.connection(),
            &baseline,
            &bound,
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(upper, 10);
        drop(baseline);
        drop(h); // checkpoint/close WAL before replacing the home database
        for suffix in ["", "-wal", "-shm"] {
            let file = path.join(format!("privateer.sqlite{suffix}"));
            if file.exists() {
                fs::remove_file(file).unwrap();
            }
        }
        if restore {
            fs::copy(&backup, path.join("privateer.sqlite")).unwrap();
        }
        let mut h = setup(&path); // same path and placement, different instance
        enrollment::begin(h.connection(), &bound).unwrap();
        let instance = enrollment::nonce(h.connection(), &bound).unwrap();
        assert_ne!(old, instance);
        send(&mut h, "new-before-snapshot");
        let baseline = enrollment::open(&path, false).unwrap();
        assert_eq!(
            enrollment::frozen(&baseline, &bound, &instance).unwrap(),
            None
        );
        assert!(enrollment::publish(h.connection(), &baseline, &bound).is_err());
        enrollment::reclaim_covered_deltas(h.connection()).unwrap();
        assert_eq!(
            count(h.connection(), "mail_source_outbox"),
            1,
            "old upper must never reclaim new deltas"
        );
        drop(baseline);
        enroll(&path, &r, &m, 100).unwrap();
        send(&mut h, "new-after-snapshot");
        drain(&path, h.connection(), &r, &m);
        let ids: Vec<String> = r
            .prepare("SELECT json_extract(event,'$.origin.id') FROM desk_source_event ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, ["new-before-snapshot", "new-after-snapshot"]);
        let baseline = enrollment::open(&path, false).unwrap();
        assert_eq!(enrollment::frozen(&baseline, &bound, &old).unwrap(), None);
        assert_eq!(
            enrollment::published_nonce(h.connection(), &bound).unwrap(),
            Some(enrollment::nonce(h.connection(), &bound).unwrap())
        );
    }
}

#[test]
fn aborted_tracking_cleans_fifty_rows_per_unit_and_recaptures_after_ack_overflow() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("home");
    let mut h = setup(&path);
    let r = registry();
    let m = manifest();
    let bound = format!("{}:{SOURCE}", binding(&m).unwrap());
    enrollment::begin(h.connection(), &bound).unwrap();
    let old = enrollment::nonce(h.connection(), &bound).unwrap();
    for n in 0..120 {
        send(&mut h, &format!("tracked-{n}"));
    }
    crate::mail::dispatch(
        &mut h,
        "mail.ack",
        json!({"address":"captain","through_seq":120}),
    )
    .unwrap();
    assert!(
        h.connection()
            .query_row("SELECT lost FROM mail_source_health", [], |r| r
                .get::<_, bool>(0))
            .unwrap()
    );
    // Commit the sidecar so the next unit resumes this attempt; an activation
    // without frozen history is now deliberately abandoned at unit start.
    let baseline = enrollment::open(&path, true).unwrap();
    enrollment::init_sidecar(&baseline).unwrap();
    enrollment::freeze(
        h.connection(),
        &baseline,
        &bound,
        Instant::now() + Duration::from_secs(10),
    )
    .unwrap();
    drop(baseline);
    // The placement fence changes after the read snapshot, before publication.
    let error = enroll_home(
        &path,
        &r,
        &m,
        SOURCE,
        Instant::now() + Duration::from_secs(10),
        100,
        || -> Result<()> { anyhow::bail!("stale_generation") },
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "stale_generation");
    assert_eq!(count(h.connection(), "mail_source_enrollment"), 0);
    assert_eq!(count(h.connection(), "mail_source_outbox"), 70);
    send(&mut h, "mail-while-deactivated");
    crate::mail::dispatch(
        &mut h,
        "mail.ack",
        json!({"address":"captain","through_seq":121}),
    )
    .unwrap();
    assert_eq!(count(h.connection(), "mail_source_outbox"), 70);
    enroll(&path, &r, &m, 101).unwrap();
    assert_eq!(count(h.connection(), "mail_source_outbox"), 20);
    enroll(&path, &r, &m, 102).unwrap();
    assert_eq!(count(h.connection(), "mail_source_outbox"), 0);
    assert_eq!(
        count(h.connection(), "mail_source_enrollment"),
        0,
        "backoff never reactivates tracking"
    );
    let counters: (i64, i64, bool) = h
        .connection()
        .query_row(
            "SELECT entries,bytes,lost FROM mail_source_health",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(counters, (0, 0, false));
    enroll(&path, &r, &m, 100 + enrollment::RETRY_MS).unwrap();
    assert_ne!(old, enrollment::nonce(h.connection(), &bound).unwrap());
    drain(&path, h.connection(), &r, &m);
    assert_eq!(count(&r, "desk_source_event"), 121);
    let result = home::capture_ready_until(
        &path,
        h.connection(),
        &r,
        &m,
        SOURCE,
        Instant::now() + Duration::from_secs(10),
    )
    .unwrap();
    assert_eq!(result["history_gap"], false);
}

#[test]
fn registry_diagnostic_contention_cannot_keep_failed_tracking_enabled() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("home");
    let mut h = setup(&path);
    let database = t.path().join("registry.sqlite");
    let r = Connection::open(&database).unwrap();
    r.execute_batch("PRAGMA journal_mode=WAL").unwrap();
    super::super::super::init(&r).unwrap();
    let m = manifest();
    let bound = format!("{}:{SOURCE}", binding(&m).unwrap());
    enrollment::begin(h.connection(), &bound).unwrap();
    send(&mut h, "tracked-before-registry-busy");
    let other = Connection::open(&database).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    r.busy_timeout(Duration::ZERO).unwrap();
    assert!(enroll(&path, &r, &m, 100).is_err());
    assert_eq!(count(h.connection(), "mail_source_enrollment"), 0);
    assert_eq!(count(h.connection(), "mail_source_outbox"), 0);
    send(&mut h, "mail-while-registry-busy");
    assert_eq!(count(h.connection(), "mail"), 2);
    other.execute_batch("ROLLBACK").unwrap();
    enroll(&path, &r, &m, 101).unwrap();
    drain(&path, h.connection(), &r, &m);
    assert_eq!(count(&r, "desk_source_event"), 2);
}

#[test]
fn published_home_cannot_hide_another_homes_baseline_obstacle() {
    let t = tempfile::tempdir().unwrap();
    let small = t.path().join("small");
    let large = t.path().join("large");
    let mut h = setup(&small);
    send(&mut h, "small-report");
    let oversized = setup(&large);
    oversized.connection().execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10001) INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms) SELECT 'retained-'||x,'survey','captain','notice','Report','{}','normal',1 FROM n;").unwrap();
    let r = registry();
    for path in [&small, &large] {
        r.execute(
            "INSERT INTO desk_home_inventory(home) VALUES(?1)",
            [path.to_str().unwrap()],
        )
        .unwrap();
    }
    let m = manifest();
    assert!(enroll(&large, &r, &m, 100).is_err());
    enroll(&small, &r, &m, 100).unwrap();
    drain(&small, h.connection(), &r, &m);
    for _ in 0..5 {
        enroll(&small, &r, &m, 101).unwrap();
        home::baseline_committed(&r, &m, SOURCE).unwrap();
        assert_eq!(
            r.query_row("SELECT state FROM desk_intake_gap", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "baseline_too_large"
        );
    }
    oversized
        .connection()
        .execute("DELETE FROM mail", [])
        .unwrap();
    enroll(&large, &r, &m, 100 + enrollment::RETRY_MS).unwrap();
    assert_eq!(
        r.query_row("SELECT state FROM desk_intake_gap", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "history_gap"
    );
}

#[test]
fn quiet_multi_home_idle_and_committed_checks_need_no_registry_writer_lock() {
    let t = tempfile::tempdir().unwrap();
    let database = t.path().join("registry.sqlite");
    let r = Connection::open(&database).unwrap();
    r.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE ships(home TEXT);")
        .unwrap();
    super::super::super::init(&r).unwrap();
    let m = manifest();
    for name in ["alpha", "beta"] {
        let path = t.path().join(name);
        let mut h = setup(&path);
        send(&mut h, name);
        r.execute(
            "INSERT INTO ships(home) VALUES(?1)",
            [path.to_str().unwrap()],
        )
        .unwrap();
        enroll(&path, &r, &m, 100).unwrap();
    }
    let other = Connection::open(&database).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    r.busy_timeout(Duration::ZERO).unwrap();
    let changes = r.total_changes();
    for _ in 0..20 {
        assert!(
            next_work(
                &r,
                &m.domain,
                SOURCE,
                Instant::now() + Duration::from_secs(10)
            )
            .unwrap()
            .is_none()
        );
        home::baseline_committed(&r, &m, SOURCE).unwrap();
    }
    assert_eq!(r.total_changes(), changes);
    assert_eq!(count(&r, "desk_baseline_cursor"), 0);
    other.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn pre_nonce_unpublished_sidecar_is_not_reused_and_schema_upgrades_are_quiet() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("home");
    let mut h = setup(&path);
    send(&mut h, "current-report");
    // Simulate the additive schema before the round-2 nonce columns existed.
    h.connection().execute_batch("ALTER TABLE mail_source_enrollment DROP COLUMN nonce; ALTER TABLE mail_source_snapshot DROP COLUMN nonce;").unwrap();
    source::init(h.connection()).unwrap();
    let changes = h.connection().total_changes();
    source::init(h.connection()).unwrap();
    assert_eq!(h.connection().total_changes(), changes);
    let r = registry();
    let m = manifest();
    let bound = format!("{}:{SOURCE}", binding(&m).unwrap());
    let baseline = enrollment::open(&path, true).unwrap();
    enrollment::init_sidecar(&baseline).unwrap();
    baseline.execute("INSERT INTO mail_source_snapshot(only,binding,upper_seq,copied_through) VALUES(1,?1,50,50)", [&bound]).unwrap();
    baseline
        .execute(
            "INSERT INTO mail_source_import(intent,event) VALUES('old-unbound','{}')",
            [],
        )
        .unwrap();
    drop(baseline);
    enroll(&path, &r, &m, 100).unwrap();
    drain(&path, h.connection(), &r, &m);
    assert_eq!(count(&r, "desk_source_event"), 1);
}

#[test]
fn pre_nonce_published_enrollment_refuses_intake_and_reclamation_but_not_mail() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("home");
    let mut h = setup(&path);
    let r = registry();
    let m = manifest();
    send(&mut h, "old-report");
    enroll(&path, &r, &m, 100).unwrap();
    h.connection().execute_batch("ALTER TABLE mail_source_enrollment DROP COLUMN nonce; ALTER TABLE mail_source_snapshot DROP COLUMN nonce;").unwrap();
    source::init(h.connection()).unwrap();
    send(&mut h, "new-mail-after-upgrade");
    // The unbound upper must not authorize deletion, even if it covers this seq.
    h.connection()
        .execute("UPDATE mail_source_snapshot SET upper_seq=100", [])
        .unwrap();
    enrollment::reclaim_covered_deltas(h.connection()).unwrap();
    assert_eq!(count(h.connection(), "mail_source_outbox"), 1);
    let e = home::capture_ready_until(
        &path,
        h.connection(),
        &r,
        &m,
        SOURCE,
        Instant::now() + Duration::from_secs(10),
    )
    .unwrap_err();
    assert!(
        e.to_string().contains("explicit recovery required"),
        "{e:#}"
    );
    send(&mut h, "mail-still-flows");
    assert_eq!(count(h.connection(), "mail"), 3);
    assert_eq!(count(&r, "desk_source_event"), 0);
}
