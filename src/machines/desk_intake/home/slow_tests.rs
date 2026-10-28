use super::*;
use std::time::{Duration, Instant};

#[test]
fn slow_enrolled_home_stages_imports_and_reclaims_durable_prefixes() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = home(&dir.path().join("home"));
    let r = registry(&dir.path().join("registry.sqlite"));
    let m = manifest();
    // The consistent baseline is deliberately atomic. This test exercises an
    // enrolled home with enough new work that no fixed 50-row batch can fit.
    capture(h.connection(), &r, &m, ALPHA).unwrap();
    for n in 0..120 {
        crate::mail::dispatch(
            &mut h,
            "mail.send",
            json!({
                "id":format!("slow-{n}"),"from":"captain","to":"survey",
                "kind":"notice","body":"retained"
            }),
        )
        .unwrap();
    }
    let mut previous = 0;
    let mut partial_batches = 0;
    for _ in 0..180 {
        let now = Instant::now();
        let result = {
            // Each cooperative checkpoint costs 10 ms: a 50-row unit cannot
            // finish in 75 ms. No sleeps, retry-until-lucky or boot assumptions.
            let _clock = source::SlowClock::start(now, Duration::from_millis(10));
            capture_until(
                h.connection(),
                &r,
                &m,
                ALPHA,
                Some(now + Duration::from_millis(75)),
            )
            .unwrap()
        };
        assert_eq!(result["complete"], false);
        let head = super::super::super::retention::head(&r, &m, ALPHA).unwrap();
        assert!(head >= previous);
        if result["imported"].as_u64().unwrap() > 0 {
            assert!(head > previous);
            assert!(result["imported"].as_u64().unwrap() < 50);
            partial_batches += 1;
        }
        previous = head;
        if head == 120 && count(h.connection(), "mail_source_import") == 0 {
            break;
        }
    }
    assert!(partial_batches > 10);
    assert_eq!(previous, 120);
    assert_eq!(count(&r, "desk_source_event"), 120, "no duplicate imports");
    assert_eq!(count(&r, "desk_home_receipt"), 0);
    assert_eq!(count(h.connection(), "mail_source_import"), 0);
    assert_eq!(count(h.connection(), "mail_source_outbox"), 0);
    assert_eq!(count(h.connection(), "mail"), 120);
    assert_eq!(count(&r, "desk_intake_gap"), 0, "budget stops are not gaps");
}

#[test]
fn malformed_import_disables_only_intake_not_mail_open_or_send() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("home");
    let h = home(&path);
    h.connection().execute_batch("DROP TABLE mail_source_cleanup;
        DROP INDEX mail_source_import_origin_seq;
        DROP INDEX mail_source_import_local_seq;
        DROP INDEX mail_source_import_pending;
        DROP INDEX mail_source_import_receipted;
        INSERT INTO mail_source_import(intent,event,receipt) VALUES('old','malformed JSON','old receipt');").unwrap();
    drop(h);
    // A normal writable Store open must never try to index that intake row.
    let mut h = Store::open(&path).unwrap();
    crate::mail::dispatch(&mut h, "mail.send", json!({
        "id":"available","from":"captain","to":"survey","kind":"notice","body":"mail still works"
    })).unwrap();
    assert_eq!(count(h.connection(), "mail"), 1);
    let r = registry(&dir.path().join("registry.sqlite"));
    let m = manifest();
    assert!(capture(h.connection(), &r, &m, ALPHA).is_err());
    assert!(
        source::missing(h.connection(), &["mail_source_cleanup"]).unwrap(),
        "upgrade rolled back"
    );
    assert_eq!(
        r.query_row("SELECT state FROM desk_intake_gap", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "unreadable"
    );
    drop(h);
    let mut h = Store::open(&path).unwrap();
    crate::mail::dispatch(&mut h, "mail.send", json!({
        "id":"still-available","from":"captain","to":"survey","kind":"notice","body":"after failed intake"
    })).unwrap();
    assert_eq!(count(h.connection(), "mail"), 2);
}
