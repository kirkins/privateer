//! Phase-boundary tests: no supervisor or payload is started by these tests.
use super::*;

fn fixture() -> (tempfile::TempDir, Store, Value) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let store = Store::open(&root.join("alpha")).unwrap();
    let repo = root.join("beta");
    fs::create_dir(&repo).unwrap();
    fs::write(
        store.home().join("config.json"),
        json!({"resource_home":root.join("gamma")}).to_string(),
    )
    .unwrap();
    let charter = json!({"id":"alpha","active":true,"repo":repo,"actions":["test","recovery"],"job_timeout_secs":60});
    store
        .connection()
        .execute(
            "INSERT INTO charters(id,data) VALUES('alpha',?1)",
            [charter.to_string()],
        )
        .unwrap();
    store.connection().execute_batch("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('alpha','alpha','alpha','alpha','open',0)").unwrap();
    let args =
        json!({"task_id":"alpha","id":"beta","command":["false"],"action":"test","heavy":false});
    (dir, store, args)
}
fn prepared(store: &Store, args: &Value) -> PreparedStart {
    match prepare_start(store, args.clone(), None, false).unwrap() {
        StartPreparation::Ready(p) => *p,
        StartPreparation::Replay(_) => panic!("unexpected replay"),
    }
}
fn held(store: &Store, p: &PreparedStart) -> bool {
    ledger(&p.job.ledger)
        .unwrap()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM reservations WHERE key=?1)",
            [reservation_key(store.home(), &p.job.id, &p.job.incarnation)],
            |r| r.get(0),
        )
        .unwrap()
}
fn exists(conn: &Connection, id: &str) -> bool {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE id=?1)", [id], |r| {
        r.get(0)
    })
    .unwrap()
}

#[test]
fn queued_start_records_resolved_demand_even_when_it_is_not_the_head() {
    let (_dir, store, mut args) = fixture();
    args["estimate"] = json!({"memory_bytes":64,"disk_bytes":0});
    args["wait_for_capacity"] = json!(true);
    let mut p = prepared(&store, &args);
    let conn = ledger(&p.job.ledger).unwrap();
    // Tighten only this isolated fixture's floor after preparation. No live
    // capacity reading, process scheduling, or clock decides this refusal.
    admission::configure(&conn, &json!({"min_free_disk_bytes":u64::MAX})).unwrap();
    let tx = store.write_transaction().unwrap();
    admit_start_in(&store, &tx, &mut p).unwrap();
    assert_eq!(p.job.status, "queued");
    let raw: String = conn
        .query_row(
            "SELECT record FROM queue WHERE key=?1",
            [&p.reservation.key],
            |r| r.get(0),
        )
        .unwrap();
    let mut entry: Queued = serde_json::from_str(&raw).unwrap();
    let capacity = entry.capacity.unwrap();
    assert!(!capacity.heavy);
    assert_eq!(
        capacity.demand.unwrap(),
        Demand {
            cpus: 0.0,
            memory_bytes: 64,
            disk_bytes: 0
        }
    );
    assert_eq!(entry.blocked, Some(Blocker::Disk));
    // Reuse the head's known refusal as an earlier, process-owned entry.
    entry.key = "alpha".into();
    entry.created_ms = 0;
    entry.supervisor = identity(std::process::id()).unwrap();
    conn.execute("DELETE FROM queue", []).unwrap();
    conn.execute(
        "INSERT INTO queue(key,record) VALUES(?1,?2)",
        params![entry.key, serde_json::to_string(&entry).unwrap()],
    )
    .unwrap();
    p.job.status = "starting".into();
    tx.execute("DELETE FROM jobs WHERE id=?1", [&p.job.id])
        .unwrap();
    admit_start_in(&store, &tx, &mut p).unwrap();
    let raw: String = conn
        .query_row(
            "SELECT record FROM queue WHERE key=?1",
            [&p.reservation.key],
            |r| r.get(0),
        )
        .unwrap();
    let entry: Queued = serde_json::from_str(&raw).unwrap();
    assert!(entry.blocked.is_none()); // not the head
    assert_eq!(entry.capacity.unwrap().demand, capacity.demand);
    tx.rollback().unwrap();
    compensate_start(&store, &p).unwrap();
}

#[test]
fn caller_transaction_can_atomically_add_intent_or_rollback_and_compensate() {
    let (_dir, store, args) = fixture();
    let mut p = prepared(&store, &args);
    assert!(!exists(store.connection(), "beta"));
    assert!(!held(&store, &p));
    let observer = Connection::open(store.home().join("privateer.sqlite")).unwrap();
    let tx = store.write_transaction().unwrap();
    assert!(admit_start_in(&store, &tx, &mut p).unwrap().is_none());
    assert!(exists(&tx, "beta"));
    assert!(!exists(&observer, "beta"));
    assert!(held(&store, &p)); // unchanged native reserve-before-home-commit
    tx.execute(
        "INSERT INTO metadata(key,value) VALUES('alpha-intent','uncertain')",
        [],
    )
    .unwrap();
    store
        .audit("alpha.intent", &json!({"job_id":"beta"}))
        .unwrap();
    tx.rollback().unwrap();
    compensate_start(&store, &p).unwrap();
    assert!(!held(&store, &p));
    assert!(!exists(store.connection(), "beta"));
    assert!(!store.connection().query_row("SELECT EXISTS(SELECT 1 FROM metadata WHERE key='alpha-intent') OR EXISTS(SELECT 1 FROM audit WHERE kind='alpha.intent')", [], |r| r.get::<_, bool>(0)).unwrap());
}

#[test]
fn committed_unspawned_start_is_reconciled_not_relaunched_and_replay_is_historical() {
    let (_dir, store, args) = fixture();
    let mut p = prepared(&store, &args);
    let tx = store.write_transaction().unwrap();
    admit_start_in(&store, &tx, &mut p).unwrap();
    tx.execute(
        "INSERT INTO metadata(key,value) VALUES('alpha-intent','uncertain')",
        [],
    )
    .unwrap();
    store
        .audit("alpha.intent", &json!({"job_id":"beta"}))
        .unwrap();
    tx.commit().unwrap();
    assert!(held(&store, &p));
    let job = load(&store, "beta").unwrap();
    assert_eq!(job.status, "starting");
    assert!(job.supervisor.is_none() && job.payload.is_none());
    // Explicitly age this fixture, not a sleep or boot/wall-clock uptime oracle.
    store
        .connection()
        .execute(
            "UPDATE jobs SET record=json_set(record,'$.created_ms',?1) WHERE id='beta'",
            [Store::now_ms() - START_GRACE_MS - 1],
        )
        .unwrap();
    let settled = reconcile(
        &store,
        load(&store, "beta").unwrap(),
        &mut Reserved::default(),
    )
    .unwrap();
    assert_eq!(settled.status, "failed");
    assert_eq!(settled.reason.as_deref(), Some("supervisor never started"));
    assert!(!held(&store, &p));
    drop(p); // release preparation's workspace lock before the historical read
    match prepare_start(&store, args, None, false).unwrap() {
        StartPreparation::Replay(v) => {
            assert_eq!(v["incarnation"], settled.incarnation);
            assert_eq!(v["status"], "failed");
        }
        StartPreparation::Ready(_) => panic!("must not prepare a second launch"),
    }
    assert_eq!(
        store
            .connection()
            .query_row(
                "SELECT value FROM metadata WHERE key='alpha-intent'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "uncertain"
    );
}

#[test]
fn insert_failure_compensates_and_authority_changes_still_refuse_at_admission() {
    for fail_insert in [false, true] {
        let (_dir, store, args) = fixture();
        let mut p = prepared(&store, &args);
        if fail_insert {
            store.connection().execute_batch("CREATE TRIGGER alpha_fail BEFORE INSERT ON jobs BEGIN SELECT RAISE(ABORT,'alpha insert refused'); END;").unwrap();
        } else {
            store.connection().execute_batch("UPDATE charters SET data=json_set(data,'$.active',json('false')) WHERE id='alpha'").unwrap();
        }
        let tx = store.write_transaction().unwrap();
        let error = admit_start_in(&store, &tx, &mut p).unwrap_err().to_string();
        assert!(
            error.contains(if fail_insert {
                "alpha insert refused"
            } else {
                "charter was revoked"
            }),
            "{error}"
        );
        tx.rollback().unwrap();
        assert!(!exists(store.connection(), "beta"));
        assert!(!held(&store, &p));
    }
}

#[test]
fn launch_requires_commit_and_spawn_failure_uses_native_cleanup() {
    let (_dir, store, args) = fixture();
    let p = prepared(&store, &args);
    let tx = store.write_transaction().unwrap();
    let error = launch_start(&store, p).unwrap_err();
    assert_eq!(error.to_string(), "job launch needs committed admission");
    tx.rollback().unwrap();
    let mut p = prepared(&store, &args);
    p.executable = store.home().join("missing-alpha-executable");
    let job = p.job.clone();
    let tx = store.write_transaction().unwrap();
    admit_start_in(&store, &tx, &mut p).unwrap();
    tx.commit().unwrap();
    assert!(held(&store, &p));
    assert!(
        launch_start(&store, p)
            .unwrap_err()
            .to_string()
            .contains("spawn Privateer supervisor using")
    );
    let failed = load(&store, &job.id).unwrap();
    assert_eq!(failed.status, "failed");
    assert!(failed.supervisor.is_none() && failed.payload.is_none());
    assert!(
        failed
            .reason
            .unwrap()
            .contains("supervisor spawn failed using")
    );
    assert!(!ledger(&job.ledger).unwrap().query_row("SELECT EXISTS(SELECT 1 FROM reservations WHERE json_extract(record,'$.job_id')='beta') OR EXISTS(SELECT 1 FROM queue WHERE json_extract(record,'$.job_id')='beta')", [], |r| r.get::<_, bool>(0)).unwrap());
}

#[test]
fn retry_request_under_caller_lock_keeps_payload_and_refuses_live_or_uncertain_statuses() {
    let (_dir, store, args) = fixture();
    let mut p = prepared(&store, &args);
    p.job.status = "failed".into();
    p.job.route_name = Some("alpha".into());
    p.job.route_why = Some("beta".into());
    p.job.model_route = Some("gamma".into());
    p.job.cleanup_descendants = false;
    store
        .connection()
        .execute(
            "INSERT INTO jobs(id,record) VALUES(?1,?2)",
            params![p.job.id, serde_json::to_string(&p.job).unwrap()],
        )
        .unwrap();
    let tx = store.write_transaction().unwrap();
    for status in [
        "starting",
        "queued",
        "launching",
        "running",
        "orphaned",
        "parked",
        "resumed",
        "succeeded",
        "failed",
        "cancelled",
        "timed_out",
    ] {
        tx.execute(
            "UPDATE jobs SET record=json_set(record,'$.status',?1) WHERE id='beta'",
            [status],
        )
        .unwrap();
        let request = retry::request(
            &store,
            &json!({"id":"beta","new_id":"gamma","command":["true"],"cwd":"/ignored","model_route":"ignored"}),
        );
        if matches!(status, "failed" | "cancelled" | "timed_out") {
            let request = request.unwrap();
            assert!(!request.agent);
            assert_eq!(request.old_id, "beta");
            assert_eq!(request.args["id"], "gamma");
            assert_eq!(request.args["command"], json!(["false"]));
            assert_eq!(request.args["cwd"], json!(p.job.cwd));
            assert_eq!(request.args["model_route"], "gamma");
            assert_eq!(request.args["route_name"], "alpha");
            assert_eq!(request.args["cleanup_descendants"], false);
        } else {
            assert_eq!(
                request.err().unwrap().to_string(),
                "retry requires a stopped unsuccessful job; no automatic retry of live/orphan work"
            );
        }
    }
    tx.execute_batch(
        "UPDATE charters SET data=json_set(data,'$.actions',json('[\"test\"]')) WHERE id='alpha'",
    )
    .unwrap();
    assert_eq!(
        retry::request(&store, &json!({"id":"beta"}))
            .err()
            .unwrap()
            .to_string(),
        "action recovery is outside this charter"
    );
    tx.rollback().unwrap();
    assert!(!held(&store, &p));
}
