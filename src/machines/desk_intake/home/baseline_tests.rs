use super::*;
use std::time::{Duration, Instant};

#[test]
fn atomic_baseline_budget_gap_is_specific_and_retry_preserves_unread_open_facts() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = home(&dir.path().join("home"));
    let r = registry(&dir.path().join("registry.sqlite"));
    let m = manifest();
    send(&mut h, "old-open");
    send(&mut h, "old-unread");
    let now = Instant::now();
    let e = {
        let _clock = source::SlowClock::start(now, Duration::from_millis(80));
        capture_until(
            h.connection(),
            &r,
            &m,
            ALPHA,
            Some(now + Duration::from_millis(75)),
        )
        .unwrap_err()
    };
    assert!(e.to_string().starts_with("baseline_too_large"), "{e:#}");
    assert_eq!(
        r.query_row("SELECT state FROM desk_intake_gap", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "baseline_too_large"
    );
    assert_eq!(count(h.connection(), "mail_source_snapshot"), 0);
    assert_eq!(count(h.connection(), "mail_source_import"), 0);
    assert_eq!(count(h.connection(), "mail"), 2);
    assert_eq!(count(&r, "desk_source_event"), 0);
    for state in ["backpressure", "unsupported", "history_gap"] {
        gap(&r, &m, ALPHA, state).unwrap();
    }
    let (coverage, eligible) = super::super::super::source_evidence(
        &r,
        &m,
        ALPHA,
        &Clock {
            session: "test",
            wall_ms: 100,
            tick_ms: 100,
        },
    )
    .unwrap();
    assert_eq!(coverage.state, "backpressure");
    assert!(!eligible);
    // Retry with a viable budget: retained facts import, but the specific
    // coverage gap remains sticky until future explicit reconciliation.
    capture(h.connection(), &r, &m, ALPHA).unwrap();
    assert_eq!(count(&r, "desk_source_event"), 2);
    assert_eq!(
        r.query_row("SELECT state FROM desk_intake_gap", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "baseline_too_large"
    );
}

#[test]
fn measure_atomic_baseline_for_five_thousand_retained_messages() {
    let dir = tempfile::tempdir().unwrap();
    let h = home(&dir.path().join("home"));
    // Real on-disk home, 5000 retained conversations, including 300 unread
    // inbound messages (150 also open). Direct fixture insertion avoids timing
    // mail generation and deliberately measures the whole first baseline.
    h.connection()
        .execute_batch(
            "WITH RECURSIVE n(x) AS (
        VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<5000)
        INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,state)
        SELECT 'retained-'||x,
            CASE WHEN x>4700 THEN 'survey' ELSE 'captain' END,
            CASE WHEN x>4700 THEN 'captain' ELSE 'survey' END,
            CASE WHEN x>4850 THEN 'request' ELSE 'notice' END,
            'Retained survey report: observations and next steps.','{}','normal',1,
            CASE WHEN x>4850 THEN 'open' ELSE NULL END FROM n;",
        )
        .unwrap();
    let start = Instant::now();
    source::snapshot(h.connection(), "measurement").unwrap();
    let elapsed = start.elapsed();
    println!(
        "BASELINE_MEASUREMENT rows=5000 unread=300 open=150 elapsed_ms={:.3} exchange_budget_ms=75",
        elapsed.as_secs_f64() * 1000.0
    );
    assert_eq!(count(h.connection(), "mail_source_import"), 5000);
    assert_eq!(
        h.connection()
            .query_row(
                "SELECT COUNT(*) FROM mail_source_import WHERE json_extract(event,'$.unread')",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        300
    );
    assert_eq!(h.connection().query_row("SELECT COUNT(*) FROM mail_source_import WHERE json_extract(event,'$.state')='open'", [], |r| r.get::<_,i64>(0)).unwrap(), 150);
}
