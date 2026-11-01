use super::*;

#[test]
fn concurrent_acks_have_one_durable_outcome_and_revocation_wins_admission() {
    let f = Fixture::new();
    f.add("old-unread", 1, "alpha", None);
    let sub = f.open();
    let page = f.pull(&sub, None, 1);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap().join("alpha.sqlite");
    f.conn
        .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
        .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            let m = f.m.clone();
            let c = DelegatedContext::from_gateway(&f.c.gateway()).unwrap();
            let op = f.ack(&sub, &page, references(&page));
            std::thread::spawn(move || {
                let conn = Connection::open(path).unwrap();
                conn.busy_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
                let f = Fixture { conn, m, c };
                barrier.wait();
                f.run_as(&f.c, op, 100).unwrap()
            })
        })
        .collect();
    let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert_eq!(results[0], results[1]);
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT through_order FROM desk_subscription_progress",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    tx.execute(
        "UPDATE door_clients SET revoked_ms=1 WHERE id=?1",
        [f.c.client()],
    )
    .unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let c = DelegatedContext::from_gateway(&f.c.gateway()).unwrap();
    let m = f.m.clone();
    let worker = std::thread::spawn(move || {
        let conn = Connection::open(path).unwrap();
        conn.busy_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let f = Fixture { conn, m, c };
        ready_tx.send(()).unwrap();
        f.run_as(&f.c, Operation::Resume { subscription: sub }, 100)
    });
    ready_rx.recv().unwrap();
    tx.commit().unwrap();
    assert!(worker.join().unwrap().is_err());
}

#[test]
fn proved_aliases_suppress_only_identity_not_equal_text() {
    let f = Fixture::new();
    f.add("origin-mail", 1, "alpha", None);
    f.add("different-mail", 2, "alpha", None);
    f.conn
        .execute(
            "UPDATE desk_intake_fact SET content=json_set(content,'$.body','same')",
            [],
        )
        .unwrap();
    let origin = MailIdentity::new(CORE, BOX, "origin-mail").unwrap();
    let copy =
        MailIdentity::new(CORE, "00000000-0000-4000-8000-0000000000b1", "copy-mail").unwrap();
    coverage::record_alias(&f.conn, &copy, &origin).unwrap();
    let sub = f.open();
    let page = f.pull(&sub, None, 50);
    assert_eq!(
        ids(&page),
        BTreeSet::from_iter(["origin-mail", "different-mail"].map(str::to_owned))
    );
    f.run(f.ack(&sub, &page, references(&page)));
    assert_eq!(f.count("desk_subscription_ref"), 0);
    assert!(ids(&f.pull(&sub, None, 50)).is_empty());
    assert_eq!(coverage::origin_of(&f.conn, &copy).unwrap(), origin);
}

#[test]
fn coverage_filters_diagnostics_and_dates_push_policy() {
    let f = Fixture::new();
    let vector = vec![
        json!({"id":CORE,"complete":true,"state":"covered","through":"opaque-checkpoint",
        "barrier_mode":"last_received_push","received_at":10,"age_ms":20,"max_source_age_ms":900000,
        "homes":[{"path":"/hidden/alpha","count":100}],"reason":"hidden-topology"}),
    ];
    let covered = coverage::subscription_coverage(&f.conn, &f.m, &f.c, &vector).unwrap();
    assert_eq!(covered["complete"], true);
    assert_eq!(covered["sources"][0]["barrier_mode"], "last_received_push");
    assert_eq!(covered["sources"][0]["age_ms"], 20);
    let rendered = covered.to_string();
    for hidden in ["/hidden/alpha", "hidden-topology", "homes", "count"] {
        assert!(!rendered.contains(hidden));
    }
    let restricted = DelegatedContext::resolved(
        uuid(),
        "door:alpha".into(),
        vec!["mail:read".into()],
        Some(vec!["alpha".into()]),
    );
    let restricted = coverage::subscription_coverage(&f.conn, &f.m, &restricted, &vector).unwrap();
    assert_eq!(restricted["complete"], false);
    assert_eq!(restricted["owner"], Value::Null);
    assert!(!restricted.to_string().contains(CORE));
}

#[test]
fn schema_upgrade_and_read_only_accessors_do_not_migrate_or_mutate() {
    let f = Fixture::new();
    f.conn
        .execute_batch(
            "DROP TABLE door_client_schema_v4;
      CREATE TABLE door_client_schema_v1(singleton INTEGER PRIMARY KEY CHECK(singleton=1));
      DROP TABLE desk_subscription; DROP TABLE desk_subscription_ref;
      DROP TABLE desk_subscription_progress; DROP TABLE desk_subscription_snapshot;
      DROP TABLE desk_subscription_receipt; DROP TABLE desk_subscription_replay;",
        )
        .unwrap();
    super::super::super::clients::init(&f.conn).unwrap();
    let schema: i64 = f
        .conn
        .pragma_query_value(None, "schema_version", |r| r.get(0))
        .unwrap();
    let changes = f.conn.total_changes();
    super::init(&f.conn).unwrap();
    assert_eq!(
        schema,
        f.conn
            .pragma_query_value::<i64, _>(None, "schema_version", |r| r.get(0))
            .unwrap()
    );
    f.conn.pragma_update(None, "query_only", true).unwrap();
    intake::visit_observations(&f.conn, &f.m, None, None, None, |_| Ok(())).unwrap();
    intake::source_vector(&f.conn, &f.m);
    assert_eq!(changes, f.conn.total_changes());
    assert!(
        f.conn
            .pragma_query_value::<bool, _>(None, "query_only", |r| r.get(0))
            .unwrap()
    );
}
