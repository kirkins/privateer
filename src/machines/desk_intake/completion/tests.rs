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
