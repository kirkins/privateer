//! Round-three review regressions, using real WAL homes and exact legacy SQL.
use super::*;

// #514's baseline_exists query, deliberately ignorant of enrollment metadata.
fn legacy_tracking(home: &Connection) -> bool {
    home.query_row(
        "SELECT EXISTS(SELECT 1 FROM mail_source_snapshot WHERE only=1)",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

fn legacy_contact(home: &Connection, bound: &str) -> Result<()> {
    // The exact #514 snapshot_until fast path. Reaching the inline capture,
    // staging or reclamation after this check would be a test failure.
    let row = home
        .query_row(
            "SELECT binding,upper_seq FROM mail_source_snapshot WHERE only=1",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?;
    if let Some((old, _upper)) = row {
        ensure!(old == bound, "stale_generation");
    }
    panic!("legacy contact reached inline capture or staging");
}

#[test]
fn placeholder_fences_legacy_contacts_and_legacy_writer_predicate_tracks_read_publication_gap() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("home");
    let mut h = setup(&path);
    let r = registry();
    let m = manifest();
    let bound = format!("{}:{SOURCE}", binding(&m).unwrap());
    send(&mut h, "before-activation");
    // This test drives the state transitions directly; real idle work installs
    // the optional import indexes before beginning enrollment.
    source::init_intake(h.connection()).unwrap();
    enrollment::begin(h.connection(), &bound).unwrap();
    assert!(legacy_tracking(h.connection()));
    assert!(!enrollment::published(h.connection()).unwrap());
    let (fence, upper): (String, i64) = h
        .connection()
        .query_row(
            "SELECT binding,upper_seq FROM mail_source_snapshot",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        fence,
        format!(
            "enrolling:{}",
            enrollment::nonce(h.connection(), &bound).unwrap()
        )
    );
    assert_eq!(upper, -1);
    let changes = h.connection().total_changes();
    assert_eq!(
        legacy_contact(h.connection(), &bound)
            .unwrap_err()
            .to_string(),
        "stale_generation"
    );
    assert_eq!(
        source::snapshot(h.connection(), &bound)
            .unwrap_err()
            .to_string(),
        "stale_generation"
    );
    assert!(
        home::capture_ready_until(
            &path,
            h.connection(),
            &r,
            &m,
            SOURCE,
            Instant::now() + Duration::from_secs(10)
        )
        .unwrap_err()
        .to_string()
        .contains("pending off contact")
    );
    assert_eq!(h.connection().total_changes(), changes);
    assert_eq!(count(h.connection(), "mail_source_import"), 0);
    send(&mut h, "before-read");
    assert_eq!(count(h.connection(), "mail_source_outbox"), 1);
    let baseline = enrollment::open(&path, true).unwrap();
    enrollment::init_sidecar(&baseline).unwrap();
    let upper = enrollment::freeze_after(h.connection(), &baseline, &bound, Instant::now() + Duration::from_secs(10), || {
        let writer = Store::open(&path).unwrap();
        writer.connection().busy_timeout(Duration::ZERO).unwrap();
        // Simulate a #514 writer's real transaction and hook, with its exact
        // snapshot-only activation predicate, not the new enrollment predicate.
        writer.connection().execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(legacy_tracking(writer.connection()));
        writer.connection().execute("INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,state) VALUES('inside-read','survey','captain','request','Retained survey report','{}','normal',1,'open')", []).unwrap();
        source::observe_legacy(writer.connection(), "inside-read", Some(&source::Provenance::Local));
        writer.connection().execute_batch("COMMIT").unwrap();
        assert_eq!(legacy_contact(writer.connection(), &bound).unwrap_err().to_string(), "stale_generation");
        assert_eq!(count(writer.connection(), "mail_source_outbox"), 2);
    }).unwrap();
    assert_eq!(upper, 1);
    send(&mut h, "after-freeze-before-publish");
    assert!(legacy_tracking(h.connection()));
    assert_eq!(count(h.connection(), "mail_source_outbox"), 3);
    enrollment::reclaim_covered_deltas(h.connection()).unwrap();
    assert_eq!(
        count(h.connection(), "mail_source_outbox"),
        3,
        "placeholder never authorizes reclaim"
    );
    enrollment::publish(h.connection(), &baseline, &bound).unwrap();
    enrollment::reclaim_covered_deltas(h.connection()).unwrap();
    assert_eq!(count(h.connection(), "mail_source_outbox"), 2);
    drain(&path, h.connection(), &r, &m);
    let ids: Vec<String> = r
        .prepare("SELECT json_extract(event,'$.origin.id') FROM desk_source_event ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        ids,
        [
            "before-activation",
            "before-read",
            "inside-read",
            "after-freeze-before-publish"
        ]
    );
    assert_eq!(count(h.connection(), "mail"), 4);
}

#[test]
fn pre_attempt_provenance_conflict_loss_and_fallback_survive_failed_enrollment() {
    for fallback in [false, true] {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("home");
        let mut h = setup(&path);
        let r = registry();
        let m = manifest();
        send(&mut h, "conflicting-origin");
        if fallback {
            h.connection().execute_batch("CREATE TRIGGER refuse_loss BEFORE UPDATE OF lost ON mail_source_health BEGIN SELECT RAISE(ABORT,'health unavailable'); END;").unwrap();
        }
        source::observe(
            h.connection(),
            "conflicting-origin",
            Some(&source::Provenance::Unresolved),
        );
        let loss = |c: &Connection| -> (i64, Option<String>) {
            c.query_row("SELECT lost,(SELECT value FROM metadata WHERE key='mail_source_loss') FROM mail_source_health", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap()
        };
        let before = loss(h.connection());
        assert_eq!(
            before,
            if fallback {
                (0, Some("1".into()))
            } else {
                (1, None)
            }
        );
        h.connection()
            .execute_batch("DROP TRIGGER IF EXISTS refuse_loss")
            .unwrap();
        let e = enroll_home(
            &path,
            &r,
            &m,
            SOURCE,
            Instant::now() + Duration::from_secs(10),
            100,
            || -> Result<()> { anyhow::bail!("stale_generation: injected publication refusal") },
        )
        .unwrap_err();
        assert!(e.to_string().contains("injected publication refusal"));
        assert_eq!(count(h.connection(), "mail_source_enrollment"), 0);
        assert_eq!(count(h.connection(), "mail_source_snapshot"), 0);
        assert_eq!(loss(h.connection()), before);
        enroll(&path, &r, &m, 100 + enrollment::RETRY_MS).unwrap();
        drain(&path, h.connection(), &r, &m);
        let result = home::capture_ready_until(
            &path,
            h.connection(),
            &r,
            &m,
            SOURCE,
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(result["history_gap"], true);
        assert_eq!(
            r.query_row("SELECT state FROM desk_intake_gap", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "history_gap"
        );
    }
}

#[test]
fn failed_cleanup_and_home_diagnostic_insert_preserve_original_error_and_source_gap() {
    for fail_insert in [false, true] {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("home");
        let mut h = setup(&path);
        send(&mut h, "report");
        let r = registry();
        let m = manifest();
        if fail_insert {
            // Pending admission succeeds; only the failure-path upsert fails.
            r.execute_batch("CREATE TRIGGER refuse_diagnostic BEFORE INSERT ON desk_baseline_home WHEN NEW.state='baseline_too_large' BEGIN SELECT RAISE(ABORT,'diagnostic unavailable'); END;").unwrap();
        }
        let writer = Connection::open(path.join("privateer.sqlite")).unwrap();
        let e = enroll_home(
            &path,
            &r,
            &m,
            SOURCE,
            Instant::now() + Duration::from_secs(10),
            100,
            || -> Result<()> {
                if !fail_insert {
                    writer.execute_batch("BEGIN IMMEDIATE")?;
                }
                anyhow::bail!("baseline_too_large: original failure")
            },
        )
        .unwrap_err();
        assert_eq!(e.to_string(), "baseline_too_large: original failure");
        assert_eq!(
            r.query_row("SELECT state FROM desk_intake_gap", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "baseline_too_large"
        );
        if !fail_insert {
            assert_eq!(
                r.query_row("SELECT state FROM desk_baseline_home", [], |r| r
                    .get::<_, String>(0))
                    .unwrap(),
                "baseline_too_large"
            );
            writer.execute_batch("ROLLBACK").unwrap();
            // A failed cleanup is retried at the next unit, before backoff.
            enroll(&path, &r, &m, 101).unwrap();
            // The frozen attempt can resume; whether it publishes or remains
            // backed off, ordinary mail must still succeed.
            send(&mut h, "mail-after-contention");
        }
    }
}

#[test]
fn departed_homes_do_not_keep_baseline_obstacle_but_retained_inventory_does() {
    let r = registry();
    let m = manifest();
    r.execute("INSERT INTO desk_baseline_home(domain,source,home,state,attempted_ms) VALUES(?1,?2,'survey','baseline_too_large',1)", params![m.domain,SOURCE]).unwrap();
    r.execute("INSERT INTO ships(home) VALUES('survey')", [])
        .unwrap();
    gap(&r, &m, SOURCE, "baseline_too_large").unwrap();
    home::baseline_committed(&r, &m, SOURCE).unwrap();
    assert_eq!(
        r.query_row("SELECT state FROM desk_intake_gap", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "baseline_too_large"
    );
    r.execute("INSERT INTO desk_home_inventory(home) VALUES('survey')", [])
        .unwrap();
    r.execute("DELETE FROM ships", []).unwrap();
    home::baseline_committed(&r, &m, SOURCE).unwrap();
    assert_eq!(
        r.query_row("SELECT state FROM desk_intake_gap", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "baseline_too_large"
    );
    r.execute("DELETE FROM desk_home_inventory", []).unwrap();
    home::baseline_committed(&r, &m, SOURCE).unwrap();
    assert_eq!(
        r.query_row("SELECT state FROM desk_intake_gap", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "history_gap"
    );
}

#[test]
fn abandoned_empty_outbox_preserves_sequence_high_water_for_next_baseline_and_delta() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("home");
    let mut h = setup(&path);
    let r = registry();
    let m = manifest();
    let bound = format!("{}:{SOURCE}", binding(&m).unwrap());
    enrollment::begin(h.connection(), &bound).unwrap();
    send(&mut h, "abandoned-before-read");
    assert_eq!(count(h.connection(), "mail_source_outbox"), 1);
    // No sidecar was frozen: abandon, reclaim, then recapture in the next unit.
    enroll(&path, &r, &m, 100).unwrap();
    assert_eq!(count(h.connection(), "mail_source_outbox"), 0);
    assert_eq!(
        h.connection()
            .query_row("SELECT upper_seq FROM mail_source_snapshot", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    send(&mut h, "after-new-baseline");
    assert_eq!(
        h.connection()
            .query_row("SELECT seq FROM mail_source_outbox", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    drain(&path, h.connection(), &r, &m);
    assert_eq!(count(&r, "desk_source_event"), 2);
    assert_eq!(count(&r, "desk_intake_gap"), 0);
}

#[test]
fn crash_before_freeze_rotates_nonce_before_later_sidecar_and_restore_cannot_reclaim() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("home");
    let backup = t.path().join("tracking-backup.sqlite");
    let h = setup(&path);
    let r = registry();
    let m = manifest();
    let bound = format!("{}:{SOURCE}", binding(&m).unwrap());
    enrollment::begin(h.connection(), &bound).unwrap();
    let old = enrollment::nonce(h.connection(), &bound).unwrap();
    h.connection()
        .execute("VACUUM INTO ?1", [backup.to_str().unwrap()])
        .unwrap();
    // Crash before freeze. Backoff must not retain this stale activation.
    drop(h);
    r.execute("INSERT INTO desk_baseline_home(domain,source,home,state,attempted_ms) VALUES(?1,?2,?3,'pending',100)", params![m.domain,SOURCE,path.to_str().unwrap()]).unwrap();
    enroll(&path, &r, &m, 101).unwrap();
    let mut h = setup(&path);
    assert_eq!(count(h.connection(), "mail_source_enrollment"), 0);
    assert_eq!(count(h.connection(), "mail_source_snapshot"), 0);
    enrollment::begin(h.connection(), &bound).unwrap();
    let current = enrollment::nonce(h.connection(), &bound).unwrap();
    assert_ne!(old, current);
    for n in 0..10 {
        send(&mut h, &format!("other-timeline-{n}"));
    }
    let baseline = enrollment::open(&path, true).unwrap();
    enrollment::init_sidecar(&baseline).unwrap();
    enrollment::freeze(
        h.connection(),
        &baseline,
        &bound,
        Instant::now() + Duration::from_secs(10),
    )
    .unwrap();
    drop(h);
    for suffix in ["", "-wal", "-shm"] {
        let file = path.join(format!("privateer.sqlite{suffix}"));
        if file.exists() {
            fs::remove_file(file).unwrap();
        }
    }
    fs::copy(backup, path.join("privateer.sqlite")).unwrap();
    let mut h = setup(&path);
    send(&mut h, "post-restore-reused-seq");
    assert!(enrollment::publish(h.connection(), &baseline, &bound).is_err());
    enrollment::reclaim_covered_deltas(h.connection()).unwrap();
    assert_eq!(count(h.connection(), "mail_source_outbox"), 1);
    drop(baseline);
    enroll(&path, &r, &m, 100 + enrollment::RETRY_MS).unwrap();
    drain(&path, h.connection(), &r, &m);
    assert_eq!(count(&r, "desk_source_event"), 1);
    assert_eq!(
        r.query_row(
            "SELECT json_extract(event,'$.origin.id') FROM desk_source_event",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "post-restore-reused-seq"
    );
}
