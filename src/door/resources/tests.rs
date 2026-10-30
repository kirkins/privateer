use super::*;

fn fixture() -> (
    tempfile::TempDir,
    Store,
    Connection,
    Connection,
    DelegatedContext,
    Value,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("survey")).unwrap();
    std::fs::write(
        store.home().join("config.json"),
        json!({"resource_home":dir.path().join("ledger")}).to_string(),
    )
    .unwrap();
    store
        .connection()
        .execute(
            "INSERT INTO ships(name,record) VALUES('survey','{\"location\":\"here\"}')",
            [],
        )
        .unwrap();
    let ledger = crate::jobs::shared_ledger(&store).unwrap();
    let registry = Connection::open_in_memory().unwrap();
    registry
        .execute_batch("CREATE TABLE ships(name TEXT,home TEXT,retired_ms INTEGER)")
        .unwrap();
    registry
        .execute(
            "INSERT INTO ships VALUES('survey',?1,NULL)",
            [store.home().to_str().unwrap()],
        )
        .unwrap();
    let c = DelegatedContext::resolved(
        uuid::Uuid::new_v4().to_string(),
        "door:alpha".into(),
        vec!["fleet:admin".into()],
        None,
    );
    let a = json!({"machine":"local","ship":"survey","changes":{"max_heavy_jobs":2},"idempotency_key":"alpha"});
    (dir, store, ledger, registry, c, a)
}
fn state(ledger: &Connection) -> Vec<String> {
    ["SELECT key||value FROM settings ORDER BY key","SELECT grant_id||resource||key_hash||args_hash||created_ms||result||audit FROM resource_action_results ORDER BY key_hash"].iter().flat_map(|sql|ledger.prepare(sql).unwrap().query_map([],|r|r.get(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()).collect()
}
#[test]
fn schemas_authority_and_probe_are_closed_noninitializing_and_lock_free() {
    let (_dir, store, ledger, registry, c, a) = fixture();
    assert!(validate_args(&a).is_ok());
    for field in [
        "path",
        "force",
        "argv",
        "estimate",
        "owner",
        "_binding",
        "target",
        "operation",
    ] {
        let mut bad = a.clone();
        bad[field] = json!("canary");
        assert_eq!(validate_args(&bad), Err(Denial::Arguments));
    }
    for changes in [
        json!({}),
        json!({"max_heavy_jobs":null}),
        json!({"max_heavy_jobs":0}),
        json!({"max_heavy_jobs":1025}),
        json!({"max_load_per_cpu":1}),
        json!({"min_free_disk_bytes":-1}),
        json!({"min_available_memory_bytes":1.5}),
    ] {
        let mut bad = a.clone();
        bad["changes"] = changes;
        assert!(validate_args(&bad).is_err());
    }
    for scopes in [
        vec![],
        vec!["fleet:read".into()],
        vec!["work:manage".into()],
    ] {
        let other = DelegatedContext::resolved(
            uuid::Uuid::new_v4().to_string(),
            "door:alpha".into(),
            scopes,
            None,
        );
        assert_eq!(
            authority(&other).unwrap_err().to_string(),
            "insufficient_scope"
        );
    }
    for ships in [json!([]), json!(["survey"]), json!(["survey", "relay"])] {
        let mut wire = c.gateway();
        wire["ships"] = ships;
        assert_eq!(
            authority(&DelegatedContext::from_gateway(&wire).unwrap())
                .unwrap_err()
                .to_string(),
            "ship_not_allowed"
        );
    }
    let target = open(&registry, &a, false).unwrap();
    let identity = target.identity.clone();
    let bytes = std::fs::read(&target.path).unwrap();
    let wal = std::fs::read(target.path.with_extension("sqlite-wal")).unwrap();
    let before = state(&ledger);
    // A held ledger writer does not prevent the probe, which takes no writer.
    ledger.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert_eq!(open(&registry, &a, false).unwrap().identity, identity);
    ledger.execute_batch("ROLLBACK").unwrap();
    assert_eq!(state(&ledger), before);
    assert_eq!(std::fs::read(&target.path).unwrap(), bytes);
    assert_eq!(
        std::fs::read(target.path.with_extension("sqlite-wal")).unwrap(),
        wal
    );
    crate::admission::init(&ledger).unwrap();
    assert_eq!(open(&registry, &a, false).unwrap().identity, identity);
    assert!(!identity.contains(store.home().to_str().unwrap()));
    ledger
        .execute_batch("DROP TABLE resource_action_identity")
        .unwrap();
    assert!(open(&registry, &a, false).is_err()); // legacy probe does not migrate
    assert!(
        ledger
            .prepare("SELECT origin FROM resource_action_identity")
            .is_err()
    );
}
#[test]
fn native_tightening_is_atomic_and_noop_holds_and_cli_relaxation_are_preserved() {
    let (_dir, _store, ledger, registry, c, mut a) = fixture();
    let target = open(&registry, &a, false).unwrap().identity;
    ledger.execute_batch("INSERT INTO reservations VALUES('alpha','{\"heavy\":true}'); INSERT INTO queue VALUES('beta','{}')").unwrap();
    let first = apply(&registry, &a, &c, &target).unwrap();
    assert_eq!(first["changed"], true);
    for (i, changes) in [
        json!({"min_free_disk_bytes":0,"max_heavy_jobs":1}),
        json!({"min_available_memory_bytes":0,"max_heavy_jobs":1}),
        json!({"max_heavy_jobs":3}),
        json!({"max_heavy_jobs":null}),
        json!({"heavy_estimate":{"memory_bytes":0}}),
    ]
    .into_iter()
    .enumerate()
    {
        a["idempotency_key"] = json!(format!("deny-{i}"));
        a["changes"] = changes;
        let before = state(&ledger);
        assert!(apply(&registry, &a, &c, &target).is_err());
        assert_eq!(state(&ledger), before);
    }
    a["idempotency_key"] = json!("noop");
    a["changes"] = json!({"max_heavy_jobs":2});
    let settings = crate::admission::settings(&ledger).unwrap();
    let noop = apply(&registry, &a, &c, &target).unwrap();
    assert_eq!(noop["changed"], false);
    assert_eq!(noop["applied"], true);
    a["idempotency_key"] = json!("tighten");
    a["changes"] = json!({"max_heavy_jobs":1,"min_available_memory_bytes":settings.min_available_memory_bytes+1,"min_free_disk_bytes":settings.min_free_disk_bytes+1});
    assert_eq!(apply(&registry, &a, &c, &target).unwrap()["changed"], true);
    assert_eq!(
        ledger
            .query_row::<String, _, _>(
                "SELECT record FROM reservations WHERE key='alpha'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        "{\"heavy\":true}"
    );
    assert_eq!(
        ledger
            .query_row::<String, _, _>("SELECT record FROM queue WHERE key='beta'", [], |r| r
                .get(0))
            .unwrap(),
        "{}"
    );
    // The native CLI still permits intentional loosening/reset; door cannot.
    crate::admission::configure(&ledger, &json!({"max_heavy_jobs":null})).unwrap();
    assert_eq!(
        crate::admission::settings(&ledger).unwrap().max_heavy_jobs,
        None
    );
}
#[test]
fn transaction_audit_failure_rolls_back_and_replay_precedes_current_policy() {
    let (_dir, _store, ledger, registry, c, mut a) = fixture();
    let target = open(&registry, &a, false).unwrap().identity;
    ledger.execute_batch("CREATE TRIGGER fail_resource BEFORE INSERT ON resource_action_results BEGIN SELECT RAISE(ABORT,'audit failure'); END").unwrap();
    let before = state(&ledger);
    assert!(apply(&registry, &a, &c, &target).is_err());
    assert_eq!(state(&ledger), before);
    ledger.execute_batch("DROP TRIGGER fail_resource").unwrap();
    let first = apply(&registry, &a, &c, &target).unwrap();
    let after = state(&ledger);
    assert_eq!(apply(&registry, &a, &c, &target).unwrap(), first);
    assert_eq!(state(&ledger), after);
    // Native operator later tightens further; recovery is historical, not a
    // new relaxation attempt. A newly opened connection recovers the same row.
    crate::admission::configure(&ledger, &json!({"max_heavy_jobs":1})).unwrap();
    assert_eq!(apply(&registry, &a, &c, &target).unwrap(), first);
    a["changes"]["max_heavy_jobs"] = json!(1);
    assert_eq!(
        apply(&registry, &a, &c, &target).unwrap_err().to_string(),
        "unsupported_arguments"
    );
    let audit: String = ledger
        .query_row("SELECT audit FROM resource_action_results", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(audit.contains("idempotency_key_hash"));
    assert!(!audit.contains("resources.sqlite"));
    assert!(!audit.contains("idempotency_key\":"));
    // Replacing a ledger origin invalidates a previously admitted target.
    ledger
        .execute(
            "UPDATE resource_action_identity SET origin=?1",
            [uuid::Uuid::new_v4().to_string()],
        )
        .unwrap();
    assert_eq!(
        apply(&registry, &a, &c, &target).unwrap_err().to_string(),
        "execution_failed"
    );
}
#[test]
fn native_layer_rejects_every_relaxing_patch_before_any_write() {
    let (_dir, _store, ledger, _registry, _c, _a) = fixture();
    crate::admission::configure(&ledger, &json!({"max_heavy_jobs":4})).unwrap();
    for patch in [
        json!({"max_heavy_jobs":5,"min_free_disk_bytes":u64::MAX}),
        json!({"min_free_disk_bytes":0,"max_heavy_jobs":1}),
        json!({"min_available_memory_bytes":0}),
        json!({"max_heavy_jobs":null}),
        json!({"ramp_secs":0}),
    ] {
        let before = state(&ledger);
        let tx =
            rusqlite::Transaction::new_unchecked(&ledger, rusqlite::TransactionBehavior::Immediate)
                .unwrap();
        assert!(crate::admission::tighten_in(&tx, &patch).is_err());
        tx.commit().unwrap(); // Even a caller committing after refusal sees no partial patch.
        assert_eq!(state(&ledger), before);
    }
}

#[test]
fn concurrent_equal_calls_record_one_outcome_without_reapplying() {
    let (dir, store, _ledger, registry, c, a) = fixture();
    let target = open(&registry, &a, false).unwrap().identity;
    let registry_file = dir.path().join("registry.sqlite");
    let file = Connection::open(&registry_file).unwrap();
    file.execute_batch("CREATE TABLE ships(name TEXT,home TEXT,retired_ms INTEGER)")
        .unwrap();
    file.execute(
        "INSERT INTO ships VALUES('survey',?1,NULL)",
        [store.home().to_str().unwrap()],
    )
    .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let calls: Vec<_> = (0..2)
        .map(|_| {
            let (path, wire, a, target, barrier) = (
                registry_file.clone(),
                c.gateway(),
                a.clone(),
                target.clone(),
                barrier.clone(),
            );
            std::thread::spawn(move || {
                let conn = Connection::open(path).unwrap();
                let c = DelegatedContext::from_gateway(&wire).unwrap();
                barrier.wait();
                apply(&conn, &a, &c, &target).map_err(|e| e.to_string())
            })
        })
        .collect();
    let results: Vec<_> = calls.into_iter().map(|h| h.join().unwrap()).collect();
    let first = apply(&registry, &a, &c, &target).unwrap();
    for result in results {
        match result {
            Ok(value) => assert_eq!(value, first),
            // A bounded writer wait can refuse before effects under host load;
            // recovery uses the same key, not a timing/sleep expectation.
            Err(reason) => assert_eq!(reason, "execution_failed"),
        }
    }
    let after = open(&registry, &a, false).unwrap();
    let rows: Vec<String> = after
        .conn
        .prepare("SELECT result FROM resource_action_results")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(rows, vec![first.to_string()]);
}

#[test]
fn bounded_retention_and_replay_at_capacity_without_clock_epoch_assumptions() {
    let (_dir, _store, ledger, registry, c, mut a) = fixture();
    let target = open(&registry, &a, false).unwrap().identity;
    let first = apply(&registry, &a, &c, &target).unwrap();
    let now = Store::now_ms();
    ledger.execute("WITH RECURSIVE n(x) AS(VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<255) INSERT INTO resource_action_results SELECT ?1,?2,'extra-'||x,'hash',?3,'{}','{}' FROM n",params![c.client(),c.resource(),now]).unwrap();
    assert_eq!(apply(&registry, &a, &c, &target).unwrap(), first);
    a["idempotency_key"] = json!("full");
    assert_eq!(
        apply(&registry, &a, &c, &target).unwrap_err().to_string(),
        "retry_capacity"
    );
    ledger
        .execute(
            "UPDATE resource_action_results SET created_ms=?1 WHERE key_hash LIKE 'extra-%'",
            [now - durable::RETENTION_MS - 1],
        )
        .unwrap();
    assert!(apply(&registry, &a, &c, &target).is_ok());
    ledger.execute("WITH RECURSIVE n(x) AS(VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<4094) INSERT INTO resource_action_results SELECT 'other-'||x,'door:alpha','global','hash',?1,'{}','{}' FROM n",[Store::now_ms()]).unwrap();
    a["idempotency_key"] = json!("global");
    assert_eq!(
        apply(&registry, &a, &c, &target).unwrap_err().to_string(),
        "retry_capacity"
    );
}
