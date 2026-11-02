use super::super::tests::Fixture;
use super::*;
const ALPHA: &str = "00000000-0000-4000-8000-00000000000a";

fn saved(c: &Connection) -> (String, String, String, i64, String, i64) {
    c.query_row(
        "SELECT binding,support,sidecar,observed_at,session,tick_ms FROM desk_replay_observation",
        [],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        },
    )
    .unwrap()
}

#[test]
fn observations_are_bound_desk_dated_and_never_substitute_for_proof() {
    let f = Fixture::remote();
    install(&f.desk).unwrap();
    let clock = Clock {
        session: "alpha",
        wall_ms: 100,
        tick_ms: 20,
    };
    record_at(&f.desk, &f.m, ALPHA, Sidecar::Absent, &clock).unwrap();
    assert_eq!(
        saved(&f.desk),
        (
            binding(&f.m).unwrap(),
            "unknown".into(),
            "absent".into(),
            100,
            "alpha".into(),
            20
        )
    );
    for state in [
        Sidecar::Proof,
        Sidecar::Marker,
        Sidecar::Absent,
        Sidecar::Invalid,
        Sidecar::Oversized,
    ] {
        record_at(&f.desk, &f.m, ALPHA, state, &clock).unwrap();
        assert_eq!(saved(&f.desk).1, "supported");
        assert_eq!(saved(&f.desk).2, state.as_str());
    }
    record_at(&f.desk, &f.m, ALPHA, Sidecar::Unsupported, &clock).unwrap();
    assert_eq!(saved(&f.desk).1, "unsupported");
    assert_eq!(
        f.desk
            .query_row("SELECT COUNT(*) FROM desk_replay_vector", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let mut changed = f.m.clone();
    changed.manifest_revision += 1;
    // Binding change forgets old support but does not insert another key.
    record_at(
        &f.desk,
        &changed,
        ALPHA,
        Sidecar::Absent,
        &Clock {
            session: "beta",
            wall_ms: 1,
            tick_ms: 0,
        },
    )
    .unwrap();
    assert_eq!(
        saved(&f.desk),
        (
            binding(&changed).unwrap(),
            "unknown".into(),
            "absent".into(),
            1,
            "beta".into(),
            0
        )
    );
    assert_eq!(
        f.desk
            .query_row("SELECT COUNT(*) FROM desk_replay_observation", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let before = saved(&f.desk);
    assert!(record_at(&f.desk, &f.m, "unknown-source", Sidecar::Marker, &clock).is_err());
    assert_eq!(saved(&f.desk), before);
}

#[test]
fn recording_is_optional_transactional_and_bounded_without_eviction() {
    let f = Fixture::remote();
    // Model an older registry without v6. The fixture's machines initializer
    // installs the latest shared subscription schema, so remove only this
    // optional table before checking that contacts never migrate it.
    f.desk
        .execute_batch("DROP TABLE desk_replay_observation")
        .unwrap();
    let schema: i64 = f
        .desk
        .pragma_query_value(None, "schema_version", |r| r.get(0))
        .unwrap();
    record(&f.desk, &f.m, ALPHA, Sidecar::Marker).unwrap();
    assert_eq!(
        f.desk
            .pragma_query_value::<i64, _>(None, "schema_version", |r| r.get(0))
            .unwrap(),
        schema
    );
    install(&f.desk).unwrap();
    let tx = rusqlite::Transaction::new_unchecked(&f.desk, TransactionBehavior::Immediate).unwrap();
    record(&tx, &f.m, ALPHA, Sidecar::Proof).unwrap();
    tx.rollback().unwrap();
    assert_eq!(
        f.desk
            .query_row("SELECT COUNT(*) FROM desk_replay_observation", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    // Fill through the real bounded writer with distinct fictional domains.
    // No historical bindings or contacts allocate additional rows for a key.
    let tx = rusqlite::Transaction::new_unchecked(&f.desk, TransactionBehavior::Immediate).unwrap();
    let mut m = f.m.clone();
    for n in 0..ROWS {
        m.domain = format!("alpha-{n}");
        record(&tx, &m, ALPHA, Sidecar::Absent).unwrap();
    }
    tx.commit().unwrap();
    let before: i64 = f
        .desk
        .query_row(
            "SELECT COUNT(*) FROM desk_replay_observation WHERE sidecar='absent'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(before, ROWS);
    assert!(
        record(&f.desk, &f.m, ALPHA, Sidecar::Marker)
            .unwrap_err()
            .to_string()
            .contains("backpressure")
    );
    m.domain = "alpha-0".into();
    record(&f.desk, &m, ALPHA, Sidecar::Marker).unwrap();
    assert_eq!(
        f.desk
            .query_row("SELECT COUNT(*) FROM desk_replay_observation", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        ROWS
    );
    assert_eq!(
        f.desk
            .query_row(
                "SELECT COUNT(*) FROM desk_replay_observation WHERE sidecar='absent'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        ROWS - 1
    );
}
