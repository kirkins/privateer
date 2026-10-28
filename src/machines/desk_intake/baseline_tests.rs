use super::*;
use crate::store::Store;
use std::os::unix::fs::PermissionsExt;

const SOURCE: &str = "00000000-0000-4000-8000-00000000000a";
const OWNER: &str = "00000000-0000-4000-8000-00000000000b";
fn manifest() -> Manifest {
    serde_json::from_value(json!({"domain":"survey","manifest_revision":1,"max_source_age_ms":900000,
        "owner_core":OWNER,"generation":1,"incarnation":uuid::Uuid::new_v4().to_string(),"coordinator":SOURCE,
        "sources":[{"core":SOURCE,"name":"alpha","ships":["survey"],"intake_direction":"push"},
        {"core":OWNER,"name":"beta","ships":["relay"],"intake_direction":"local"}]})).unwrap()
}
fn setup(path: &Path) -> Store {
    let mut h = Store::open(path).unwrap();
    source::bind(h.connection(), SOURCE).unwrap();
    crate::mail::dispatch(
        &mut h,
        "ship.put",
        json!({"name":"survey","location":"here"}),
    )
    .unwrap();
    h
}
fn send(h: &mut Store, id: &str) {
    crate::mail::dispatch(h, "mail.send", json!({"id":id,"from":"survey","to":"captain","kind":"request","body":"Retained survey report"})).unwrap();
}
fn registry() -> Connection {
    let r = Connection::open_in_memory().unwrap();
    super::super::init(&r).unwrap();
    r
}
fn count(c: &Connection, table: &str) -> i64 {
    c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}
fn enroll(path: &Path, r: &Connection, m: &Manifest, now: i64) -> Result<()> {
    enroll_home(
        path,
        r,
        m,
        SOURCE,
        Instant::now() + Duration::from_secs(10),
        now,
        || Ok(()),
    )
}
fn drain(path: &Path, h: &Connection, r: &Connection, m: &Manifest) {
    for _ in 0..2000 {
        let deadline = Instant::now() + Duration::from_millis(75);
        let _rb = transport::Budget::new(r, deadline).unwrap();
        let _hb = transport::Budget::new(h, deadline).unwrap();
        let result = home::capture_ready_until(path, h, r, m, SOURCE, deadline);
        if let Err(e) = result {
            assert!(retention::interrupted_or_busy(&e), "{e:#}");
            continue;
        }
        let baseline = enrollment::open(path, false).unwrap();
        let pending: bool = baseline
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM mail_source_import WHERE receipt IS NULL)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let deltas: bool = h.query_row("SELECT EXISTS(SELECT 1 FROM mail_source_outbox WHERE seq>(SELECT copied_through FROM mail_source_snapshot)) OR EXISTS(SELECT 1 FROM mail_source_import WHERE receipt IS NULL)", [], |r| r.get(0)).unwrap();
        if !pending && !deltas {
            return;
        }
    }
    panic!("published baseline did not drain through bounded contacts");
}

#[test]
fn five_thousand_row_home_enrolls_off_contact_and_imports_all_retained_facts() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("home");
    let h = setup(&path);
    let r = registry();
    let m = manifest();
    h.connection().execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<5000)
        INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,state)
        SELECT 'retained-'||x,CASE WHEN x>4700 THEN 'survey' ELSE 'captain' END,
        CASE WHEN x>4700 THEN 'captain' ELSE 'survey' END,CASE WHEN x>4850 THEN 'request' ELSE 'notice' END,
        'Retained survey report: observations and next steps.','{}','normal',1,
        CASE WHEN x>4850 THEN 'open' ELSE NULL END FROM n;").unwrap();
    // Pin fictional origins explicitly; fixture SQL must not invent implicit
    // transport provenance. Payload is still frozen by the real read snapshot.
    let mailbox: String = h
        .connection()
        .query_row("SELECT mailbox FROM mail_source_health", [], |r| r.get(0))
        .unwrap();
    h.connection().execute("INSERT INTO mail_source_origin(id,provenance,content)
        SELECT id,json_object('Origin',json_object('core',?1,'mailbox',?2,'id',id)),'null' FROM mail", params![SOURCE,mailbox]).unwrap();
    let before = h.connection().total_changes();
    let work = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = work.clone();
    h.connection().progress_handler(
        1,
        Some(move || {
            observed.fetch_add(1, Ordering::Relaxed);
            false
        }),
    );
    for _ in 0..5 {
        let deadline = Instant::now() + Duration::from_millis(75);
        assert!(
            home::capture_ready_until(&path, h.connection(), &r, &m, SOURCE, deadline)
                .unwrap_err()
                .to_string()
                .contains("pending off contact")
        );
    }
    h.connection().progress_handler(0, None::<fn() -> bool>);
    assert!(
        work.load(Ordering::Relaxed) < 1000,
        "pending contacts must not scan history"
    );
    assert_eq!(
        h.connection().total_changes(),
        before,
        "contact cannot activate or attempt a baseline"
    );
    assert_eq!(count(h.connection(), "mail_source_import"), 0);
    let started = Instant::now();
    enroll(&path, &r, &m, 100).unwrap();
    println!(
        "OFF_PATH_BASELINE rows=5000 unread=300 open=150 elapsed_ms={:.3} exchange_budget_ms=75",
        started.elapsed().as_secs_f64() * 1000.0
    );
    let baseline = enrollment::open(&path, false).unwrap();
    assert_eq!(count(&baseline, "mail_source_import"), 5000);
    assert_eq!(
        count(h.connection(), "mail_source_import"),
        0,
        "no full-history insert lock in the home"
    );
    assert_eq!(
        fs::metadata(enrollment::path(&path))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    drain(&path, h.connection(), &r, &m);
    assert_eq!(count(&r, "desk_source_event"), 5000);
    let (unread,open): (i64,i64) = r.query_row("SELECT SUM(json_extract(event,'$.unread')),SUM(json_extract(event,'$.pending')) FROM desk_source_event", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!((unread, open), (300, 150));
    // All published events are page-able through one immutable proved upper.
    let cp = checkpoint(&r, &m, SOURCE, &uuid::Uuid::new_v4().to_string()).unwrap();
    let desk = registry();
    let mut after = 0;
    while after < cp.upper {
        let p = page(&r, &m, &cp, after, 50, 192 * 1024).unwrap();
        accept(
            &desk,
            &m,
            &p,
            &Clock {
                session: "test",
                wall_ms: 100,
                tick_ms: 100,
            },
        )
        .unwrap();
        after = p.through;
    }
    assert_eq!(count(&desk, "desk_intake_fact"), 5000);
}

#[test]
fn read_snapshot_allows_real_writer_commit_and_replays_only_post_upper_deltas() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("home");
    let mut h = setup(&path);
    let r = registry();
    let m = manifest();
    let bound = format!("{}:{SOURCE}", binding(&m).unwrap());
    send(&mut h, "before-tracking");
    enrollment::begin(h.connection(), &bound).unwrap();
    send(&mut h, "before-read");
    let baseline = enrollment::open(&path, true).unwrap();
    enrollment::init_sidecar(&baseline).unwrap();
    let upper = enrollment::freeze_after(
        h.connection(),
        &baseline,
        &bound,
        Instant::now() + Duration::from_secs(10),
        || {
            // Executed after the snapshot upper was read, before enumeration. A
            // zero-wait real mail writer must COMMIT while that read remains open.
            let mut writer = Store::open(&path).unwrap();
            writer.connection().busy_timeout(Duration::ZERO).unwrap();
            send(&mut writer, "inside-read");
        },
    )
    .unwrap();
    assert_eq!(upper, 1);
    assert_eq!(count(&baseline, "mail_source_import"), 2);
    drop(baseline);
    drop(h); // frozen, not published: simulate process death
    let mut h = setup(&path);
    send(&mut h, "after-freeze");
    enroll(&path, &r, &m, 100).unwrap();
    let baseline = enrollment::open(&path, false).unwrap();
    assert_eq!(
        enrollment::frozen(
            &baseline,
            &bound,
            &enrollment::nonce(h.connection(), &bound).unwrap()
        )
        .unwrap(),
        Some(upper)
    );
    assert_eq!(
        count(&baseline, "mail_source_import"),
        2,
        "restart must reuse the exact frozen snapshot"
    );
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
            "before-tracking",
            "before-read",
            "inside-read",
            "after-freeze"
        ]
    );
    drop(h);
    let h = setup(&path);
    let changes = h.connection().total_changes();
    let baseline = enrollment::open(&path, false).unwrap();
    let version: i64 = baseline
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap();
    drain(&path, h.connection(), &r, &m);
    assert_eq!(
        baseline
            .query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        version,
        "quiet contact writes neither the home nor the frozen sidecar"
    );
    assert_eq!(
        h.connection().total_changes(),
        changes,
        "published quiet contact writes no home data"
    );
    assert_eq!(count(&r, "desk_source_event"), 4);
}

#[test]
fn tracking_restart_and_failed_freeze_are_atomic_backed_off_and_clear_only_baseline_gap() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("home");
    let mut h = setup(&path);
    let r = registry();
    let m = manifest();
    let bound = format!("{}:{SOURCE}", binding(&m).unwrap());
    enrollment::begin(h.connection(), &bound).unwrap();
    send(&mut h, "tracked-before-restart");
    send(&mut h, "second-tracked-before-restart");
    drop(h);
    let h = setup(&path);
    let now = Instant::now();
    {
        // Freeze one row, then expire at the second: rollback must remove
        // real partial sidecar work, not merely reject before any insert.
        let _clock = source::SlowClock::start(now, Duration::from_secs(20));
        let error = enroll(&path, &r, &m, 100).unwrap_err();
        assert!(
            error.to_string().starts_with("baseline_too_large"),
            "{error:#}"
        );
    }
    let baseline = enrollment::open(&path, false).unwrap();
    assert_eq!(count(&baseline, "mail_source_import"), 0);
    assert_eq!(count(&baseline, "mail_source_snapshot"), 0);
    assert_eq!(count(h.connection(), "mail_source_snapshot"), 0);
    assert_eq!(count(h.connection(), "mail_source_outbox"), 0);
    assert_eq!(
        count(h.connection(), "mail_source_enrollment"),
        0,
        "failed capture turns tracking off"
    );
    enroll(&path, &r, &m, 101).unwrap(); // stable failure is not a hot retry
    assert_eq!(count(h.connection(), "mail_source_snapshot"), 0);
    enroll(&path, &r, &m, 100 + enrollment::RETRY_MS).unwrap();
    assert_eq!(count(h.connection(), "mail_source_snapshot"), 1);
    let state: String = r
        .query_row("SELECT state FROM desk_intake_gap", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        state, "history_gap",
        "successful publication downgrades only the fixed baseline obstacle"
    );
    drain(&path, h.connection(), &r, &m);
    assert_eq!(count(&r, "desk_source_event"), 2);
}

#[test]
fn oversized_home_stops_tracking_and_can_shrink_then_enroll_without_sticky_loss() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("home");
    let mut h = setup(&path);
    let r = registry();
    let m = manifest();
    h.connection()
        .execute_batch(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10001)
        INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms)
        SELECT 'retained-'||x,'survey','captain','notice','Report','{}','normal',1 FROM n;",
        )
        .unwrap();
    assert!(
        enroll(&path, &r, &m, 100)
            .unwrap_err()
            .to_string()
            .starts_with("baseline_too_large")
    );
    let baseline = enrollment::open(&path, false).unwrap();
    assert_eq!(count(&baseline, "mail_source_import"), 0);
    assert_eq!(count(h.connection(), "mail_source_snapshot"), 0);
    assert_eq!(count(h.connection(), "mail_source_enrollment"), 0);
    send(&mut h, "mail-survives-oversized-baseline");
    crate::mail::dispatch(
        &mut h,
        "mail.ack",
        json!({"address":"captain","through_seq":10002}),
    )
    .unwrap();
    assert_eq!(count(h.connection(), "mail"), 10002);
    assert_eq!(count(h.connection(), "mail_source_outbox"), 0);
    assert!(
        !h.connection()
            .query_row("SELECT lost FROM mail_source_health", [], |r| r
                .get::<_, bool>(0))
            .unwrap()
    );
    // Retention shrinks the home; retry uses a fresh nonce and a fresh snapshot.
    h.connection()
        .execute("DELETE FROM mail WHERE id LIKE 'retained-%'", [])
        .unwrap();
    enroll(&path, &r, &m, 100 + enrollment::RETRY_MS).unwrap();
    drain(&path, h.connection(), &r, &m);
    assert_eq!(count(&r, "desk_source_event"), 1);
    let result = home::capture_ready_until(
        &path,
        h.connection(),
        &r,
        &m,
        SOURCE,
        Instant::now() + Duration::from_secs(10),
    )
    .unwrap();
    assert_eq!(
        result["history_gap"], false,
        "discarded pre-publication deltas cannot leave permanent loss"
    );
}

#[cfg(test)]
#[path = "baseline_review_tests.rs"]
mod review_tests;

#[test]
fn invalid_index_row_is_not_rebuilt_until_that_exact_row_changes() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("home");
    let mut h = setup(&path);
    h.connection().execute("INSERT INTO mail_source_import(intent,event) VALUES('earlier-unreceipted','also-not-json')", []).unwrap();
    h.connection()
        .execute(
            "INSERT INTO mail_source_import(intent,event,receipt) VALUES(?1,'not-json','retained')",
            [uuid::Uuid::new_v4().to_string()],
        )
        .unwrap();
    assert!(source::init_intake(h.connection()).is_err());
    assert_eq!(count(h.connection(), "mail_source_upgrade_failure"), 1);
    let other = Connection::open(path.join("privateer.sqlite")).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    for _ in 0..5 {
        let error = source::init_intake(h.connection()).unwrap_err();
        assert!(
            error.to_string().contains("row unchanged"),
            "must skip without attempting a write lock: {error:#}"
        );
    }
    other.execute_batch("ROLLBACK").unwrap();
    send(&mut h, "ordinary-mail-survives-index-failure");
    h.connection()
        .execute(
            "UPDATE mail_source_import SET event='{}' WHERE receipt IS NOT NULL",
            [],
        )
        .unwrap();
    source::init_intake(h.connection()).unwrap();
    assert_eq!(count(h.connection(), "mail_source_upgrade_failure"), 0);
    assert!(source::intake_ready(h.connection()).unwrap());
}
