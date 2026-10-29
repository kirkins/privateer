// End-to-end predicates are exercised with real gateways in desk_placement.
// The fixture helpers in proof/tests additionally test this evaluator directly.
use super::*;

#[test]
fn freshness_rejects_discontinuities_without_uptime_assumptions() {
    let conn = Connection::open_in_memory().unwrap();
    init(&conn).unwrap();
    let m: Manifest = serde_json::from_value(json!({"domain":"alpha","manifest_revision":1,"max_source_age_ms":5000,"owner_core":"00000000-0000-4000-8000-00000000000a","generation":1,"incarnation":"00000000-0000-4000-8000-00000000000b","coordinator":"00000000-0000-4000-8000-00000000000a","sources":[]})).unwrap();
    conn.execute("INSERT INTO desk_intake_checkpoint(domain,source,binding,barrier,upper_seq,through_seq,receipt,received_at,session,tick_ms,clock_valid,mode) VALUES('alpha','alpha','alpha','alpha',0,0,'alpha',10000,'alpha',700,1,'last_received_push')",[]).unwrap();
    for (wall, tick, session, expected) in [
        (10000, 700, "alpha", true),
        (15000, 5700, "alpha", true),
        (15001, 5701, "alpha", false),
        (9999, 700, "alpha", false),
        (10000, 699, "alpha", false),
        (13000, 700, "alpha", false),
        (10000, 700, "beta", false),
        (10000, 700, "", false),
    ] {
        let before = conn.total_changes();
        assert_eq!(
            fresh(
                &conn,
                &m,
                "alpha",
                &Clock {
                    session,
                    wall_ms: wall,
                    tick_ms: tick
                }
            )
            .unwrap(),
            expected
        );
        assert_eq!(conn.total_changes(), before);
    }
}

#[test]
fn status_evaluation_is_read_only_and_isolates_source_errors_with_retained_counts() {
    let t = tempfile::tempdir().unwrap();
    let db = t.path().join("alpha.sqlite");
    let conn = Connection::open(&db).unwrap();
    crate::machines::init(&conn).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; CREATE TABLE ships(name TEXT,home TEXT,retired_ms INTEGER)",
    )
    .unwrap();
    let m: Manifest = serde_json::from_value(json!({"domain":"alpha","manifest_revision":1,"max_source_age_ms":5000,"owner_core":"alpha","generation":1,"incarnation":"alpha","coordinator":"alpha","sources":[{"core":"alpha","name":"alpha","ships":[],"intake_direction":"local"},{"core":"beta","name":"beta","ships":[],"intake_direction":"collect"}]})).unwrap();
    for source in ["alpha", "beta"] {
        conn.execute("INSERT INTO desk_intake_checkpoint(domain,source,binding,barrier,upper_seq,through_seq,receipt,received_at,session,tick_ms,clock_valid,mode) VALUES('alpha',?1,?2,'alpha',0,0,'alpha',10000,'alpha',700,1,'last_received_push')",params![source,binding(&m).unwrap()]).unwrap();
    }
    // One source has malformed persisted clock evidence; the other can still
    // report its actual barrier state instead of a fleet-wide error fallback.
    conn.execute(
        "UPDATE desk_intake_checkpoint SET tick_ms='unreadable' WHERE source='beta'",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO desk_intake_proof(domain,source,body,valid) VALUES('alpha','beta',?1,0)",
        [json!({"vector":{"homes":[{"home":"beta","unread":7,"open":3}]}}).to_string()],
    )
    .unwrap();
    let before = conn.total_changes();
    let writer = Connection::open(&db).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let clock = Clock {
        session: "beta",
        wall_ms: 10000,
        tick_ms: 700,
    };
    {
        let _read_only = ReadOnly::new(&conn).unwrap();
        let read =
            rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Deferred).unwrap();
        let out = evaluate_sources(&read, &m, &clock, transport::contact_deadline());
        assert_eq!(out[0]["state"], "cached");
        assert_eq!(out[0]["reason"], "proof_unavailable");
        assert_eq!(out[1]["reason"], "source_status_unavailable");
        assert_eq!(out[1]["homes"][0]["unread"], 7);
        assert_eq!(out[1]["homes"][0]["open"], 3);
        assert_eq!(out[1]["homes"][0]["facts_cached"], true);
        read.commit().unwrap();
        // Expiry also keeps each source's retained facts, never a generic
        // fleet-wide collapse. The injected clock is independent of uptime.
        let now = Instant::now();
        let _clock = source::SlowClock::start(
            now + std::time::Duration::from_secs(2),
            std::time::Duration::ZERO,
        );
        let out = evaluate_sources(&conn, &m, &clock, now);
        assert_eq!(out[1]["reason"], "status_budget_exhausted");
        assert_eq!(out[1]["homes"][0]["open"], 3);
    }
    assert!(
        !conn
            .query_row("PRAGMA query_only", [], |r| r.get::<_, bool>(0))
            .unwrap()
    );
    assert_eq!(
        conn.total_changes(),
        before,
        "status writes no rows even on discontinuity"
    );
    assert!(
        conn.query_row(
            "SELECT clock_valid FROM desk_intake_checkpoint WHERE source='alpha'",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap()
    );
    writer.execute_batch("ROLLBACK").unwrap();
}
