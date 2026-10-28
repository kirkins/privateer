use super::*;
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicUsize, Ordering},
};

fn fixture() -> (tempfile::TempDir, Store, DelegatedContext) {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path()).unwrap();
    s.connection().execute_batch("INSERT INTO ships(name,record) VALUES('survey','{\"location\":\"here\",\"first_mate\":{}}'),('relay','{\"location\":\"here\"}');
        INSERT INTO charters(id,data) VALUES('survey','{\"id\":\"survey\",\"active\":true,\"actions\":[]}');
        INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('alpha','survey','survey','alpha','closed',0),('beta','survey','relay','hidden','open',0);").unwrap();
    let c = DelegatedContext::resolved(
        uuid::Uuid::new_v4().to_string(),
        "door:alpha".into(),
        vec!["work:manage".into()],
        Some(vec!["survey".into()]),
    );
    insert(&s, "gamma", "alpha", "failed");
    (dir, s, c)
}
fn insert(s: &Store, id: &str, task: &str, status: &str) {
    let job = json!({"id":id,"task_id":task,"incarnation":id,"status":status,"command":["false"],"cwd":s.home(),"action":"test","heavy":false,"heavy_declared":false,"timeout_secs":60,"ledger":s.home().join("resources.sqlite"),"created_ms":0,"finished_ms":1,"supervisor":null,"payload":null,"cancel_requested":false,"exit_code":1,"reason":null,"retry_of":null});
    s.connection()
        .execute(
            "INSERT INTO jobs(id,record) VALUES(?1,?2)",
            params![id, job.to_string()],
        )
        .unwrap();
}
fn args(s: &Store, key: &str) -> Value {
    json!({"ship":"survey","reference":{"origin":inspection::origin(s).unwrap(),"ship":"survey","task_id":"alpha","id":"gamma","incarnation":"gamma"},"idempotency_key":key})
}
fn snapshot(s: &Store) -> Vec<String> {
    let tables = s.connection().prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").unwrap().query_map([], |r|r.get::<_,String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    let mut out = vec![];
    for table in tables {
        let mut stmt = s
            .connection()
            .prepare(&format!("SELECT * FROM \"{table}\" ORDER BY rowid"))
            .unwrap();
        let n = stmt.column_count();
        let mut rows = stmt.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            out.push(format!(
                "{table}:{:?}",
                (0..n)
                    .map(|i| format!("{:?}", row.get_ref(i).unwrap()))
                    .collect::<Vec<_>>()
            ));
        }
    }
    out
}
fn denied(s: &Store, a: &Value, c: &DelegatedContext) -> String {
    let before = snapshot(s);
    let error = apply_at(s, a, c, 100).unwrap_err().to_string();
    assert_eq!(snapshot(s), before, "denied action mutated home");
    error
}
fn changed(c: &DelegatedContext, k: &str, v: Value) -> DelegatedContext {
    let mut wire = c.gateway();
    wire[k] = v;
    DelegatedContext::from_gateway(&wire).unwrap()
}

#[test]
fn closed_arguments_and_unequal_grants_no_native_or_scope_escape() {
    let (_dir, s, c) = fixture();
    let a = args(&s, "alpha");
    assert!(validate_args(&a).is_ok());
    for field in [
        "command", "argv", "route", "policy", "estimate", "pid", "pgid", "force", "new_id",
        "operator", "payload", "wait", "rank", "brief",
    ] {
        let mut bad = a.clone();
        bad[field] = json!("canary");
        assert_eq!(denied(&s, &bad, &c), Denial::Arguments.to_string());
    }
    for field in ["ship", "reference", "idempotency_key"] {
        let mut bad = a.clone();
        bad.as_object_mut().unwrap().remove(field);
        assert_eq!(denied(&s, &bad, &c), Denial::Arguments.to_string());
    }
    for field in ["origin", "incarnation", "ship", "task_id", "id"] {
        for value in [Value::Null, json!(1), json!(""), json!("../alpha")] {
            let mut bad = a.clone();
            bad["reference"][field] = value;
            assert_eq!(denied(&s, &bad, &c), Denial::Arguments.to_string());
        }
    }
    for key in [
        json!(""),
        json!("a\nb"),
        json!("é"),
        json!("a".repeat(129)),
        Value::Null,
    ] {
        let mut bad = a.clone();
        bad["idempotency_key"] = key;
        assert_eq!(denied(&s, &bad, &c), Denial::Arguments.to_string());
    }
    let weak = changed(
        &c,
        "scopes",
        json!(["fleet:read", "approval:answer", "ship:message"]),
    );
    assert_eq!(denied(&s, &a, &weak), Denial::Scope.to_string());
    let other = changed(&c, "ships", json!(["relay"]));
    assert_eq!(denied(&s, &a, &other), Denial::Ship.to_string());
    // Cancellation never needs launch/recovery charter authority, including
    // when a task closed or the charter was revoked. It grants none of them.
    assert_eq!(apply_at(&s, &a, &c, 100).unwrap()["outcome"], "stopped");
}

#[test]
fn hidden_stale_worker_ancestry_and_every_lineage_are_nonrevealing() {
    for field in [
        "started_by",
        "retry_of",
        "resume_of",
        "relaunch_of",
        "agent_actor",
    ] {
        let (_dir, s, c) = fixture();
        let a = args(&s, "alpha");
        if field != "agent_actor" {
            insert(&s, "hidden", "beta", "failed");
        }
        s.connection()
            .execute(
                "UPDATE jobs SET record=json_set(record,?1,?2) WHERE id='gamma'",
                params![
                    format!("$.{field}"),
                    if field == "agent_actor" {
                        "relay"
                    } else {
                        "hidden"
                    }
                ],
            )
            .unwrap();
        assert_eq!(denied(&s, &a, &c), Denial::Ship.to_string(), "{field}");
    }
    for field in ["started_by", "retry_of", "resume_of", "relaunch_of"] {
        let (_dir, s, c) = fixture();
        let a = args(&s, "alpha");
        insert(&s, "hidden", "beta", "failed");
        s.connection()
            .execute(
                "UPDATE jobs SET record=json_set(record,?1,'gamma') WHERE id='hidden'",
                [format!("$.{field}")],
            )
            .unwrap();
        assert_eq!(denied(&s, &a, &c), Denial::Ship.to_string());
    }
    for sql in [
        "UPDATE tasks SET parent='beta' WHERE id='alpha'",
        "UPDATE tasks SET owner='relay' WHERE id='alpha'",
        "UPDATE jobs SET record=json_set(record,'$.incarnation','replaced') WHERE id='gamma'",
        "UPDATE jobs SET record=json_set(record,'$.task_id','beta') WHERE id='gamma'",
        "DELETE FROM jobs WHERE id='gamma'",
    ] {
        let (_dir, s, c) = fixture();
        let a = args(&s, "alpha");
        s.connection().execute_batch(sql).unwrap();
        assert_eq!(denied(&s, &a, &c), Denial::Ship.to_string());
    }
}

#[test]
fn parked_children_and_park_ship_are_checked_before_intent() {
    for (task, ship) in [("beta", "survey"), ("alpha", "relay")] {
        let (_dir, s, c) = fixture();
        let a = args(&s, "alpha");
        insert(&s, "child", task, "running");
        s.connection().execute_batch("UPDATE jobs SET record=json_set(record,'$.status','parked','$.park','park-alpha') WHERE id='gamma';").unwrap();
        s.connection().execute("INSERT INTO crew_parks(id,root,crew_job_id,child_job_id,task_id,ship,state,next,created_ms,updated_ms) VALUES('park-alpha','alpha','gamma','child','alpha',?1,'parked','hidden body',0,0)",[ship]).unwrap();
        assert_eq!(denied(&s, &a, &c), Denial::Ship.to_string());
    }
}

#[test]
fn durable_intent_and_audit_rollback_precede_irreversible_native_boundary() {
    let (_dir, s, c) = fixture();
    let a = args(&s, "alpha");
    s.connection().execute_batch("CREATE TRIGGER fail_cancel BEFORE INSERT ON audit WHEN NEW.kind='door.work' BEGIN SELECT RAISE(ABORT,'audit unavailable'); END;").unwrap();
    let before = snapshot(&s);
    assert!(apply_with(&s, &a, &c, 100, || panic!("must not signal")).is_err());
    assert_eq!(snapshot(&s), before);
    s.connection()
        .execute_batch("DROP TRIGGER fail_cancel")
        .unwrap();
    // Simulate a process crash or signal error after the committed intent.
    assert_eq!(
        apply_with(&s, &a, &c, 100, || {
            assert!(s.connection().is_autocommit());
            assert_eq!(
                crate::jobs::record(&s, "gamma").unwrap()["cancel_requested"],
                false
            ); // already terminal
            anyhow::bail!("signal outcome uncertain")
        })
        .unwrap_err()
        .to_string(),
        Denial::OutcomeUnknown.to_string()
    );
    let before = snapshot(&s);
    assert_eq!(
        apply_with(&s, &a, &c, 100, || panic!("replay must not signal"))
            .unwrap_err()
            .to_string(),
        Denial::OutcomeUnknown.to_string()
    );
    assert_eq!(snapshot(&s), before);
}

#[test]
fn native_unknown_keeps_cancel_request_and_never_claims_stopped_or_frees_custody() {
    let (_dir, s, c) = fixture();
    let a = args(&s, "alpha");
    s.connection().execute("UPDATE jobs SET record=json_set(record,'$.status','orphaned','$.command',json('[\"false\"]'),'$.model_route','route-canary') WHERE id='gamma'",[]).unwrap();
    assert_eq!(
        apply_at(&s, &a, &c, 100).unwrap_err().to_string(),
        Denial::OutcomeUnknown.to_string()
    );
    let raw = crate::jobs::record(&s, "gamma").unwrap();
    assert_eq!(raw["status"], "orphaned");
    assert_eq!(raw["cancel_requested"], true);
    assert_eq!(raw["cancelled_by"], format!("door:{}", c.client()));
    assert_ne!(raw["cancelled_by"], "operator");
    assert_eq!(raw["command"], json!(["false"]));
    assert_eq!(raw["model_route"], "route-canary");
    assert_eq!(
        apply_at(&s, &a, &c, 100).unwrap_err().to_string(),
        Denial::OutcomeUnknown.to_string()
    );
}

#[test]
fn terminal_and_requested_outcomes_replay_before_visibility_and_after_restart() {
    for status in [
        "succeeded",
        "failed",
        "cancelled",
        "timed_out",
        "resumed",
        "running",
    ] {
        let (dir, s, c) = fixture();
        let a = args(&s, "alpha");
        s.connection()
            .execute(
                "UPDATE jobs SET record=json_set(record,'$.status',?1) WHERE id='gamma'",
                [status],
            )
            .unwrap();
        let result = apply_with(&s, &a, &c, 100, || {
            assert!(s.connection().is_autocommit());
            if status == "running" {
                assert_eq!(
                    crate::jobs::record(&s, "gamma").unwrap()["cancel_requested"],
                    true
                );
            }
            Ok(json!({"status":status}))
        })
        .unwrap();
        assert_eq!(
            result["outcome"],
            if matches!(status, "running" | "resumed") {
                "cancel_requested"
            } else {
                "stopped"
            }
        );
        s.connection()
            .execute_batch("DELETE FROM jobs; UPDATE tasks SET owner='relay' WHERE id='alpha';")
            .unwrap();
        drop(s);
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(
            apply_with(&s, &a, &c, 100, || panic!("replay must not signal")).unwrap(),
            result
        );
        let mut conflict = a.clone();
        conflict["reference"]["id"] = json!("different");
        assert_eq!(denied(&s, &conflict, &c), Denial::Arguments.to_string());
        let revised = changed(&c, "visibility_revision", json!(1));
        assert_eq!(denied(&s, &a, &revised), Denial::Arguments.to_string());
    }
}

#[test]
fn finish_retry_and_incarnation_races_never_cancel_replacement_work() {
    let (_dir, s, c) = fixture();
    let a = args(&s, "finished");
    // Finishing in the same incarnation is not stale custody: cancel precisely
    // acknowledges this already-stopped job, never its newly retried successor.
    insert(&s, "retry", "alpha", "running");
    s.connection()
        .execute_batch(
            "UPDATE jobs SET record=json_set(record,'$.retry_of','gamma') WHERE id='retry';",
        )
        .unwrap();
    assert_eq!(apply_at(&s, &a, &c, 100).unwrap()["outcome"], "stopped");
    assert_eq!(
        crate::jobs::record(&s, "retry").unwrap()["cancel_requested"],
        false
    );
    let a = args(&s, "replaced-after-intent");
    assert_eq!(apply_with(&s,&a,&c,100,|| {
        s.connection().execute_batch("UPDATE jobs SET record=json_set(record,'$.incarnation','replacement','$.status','running') WHERE id='gamma';").unwrap();
        crate::jobs::cancel_exact(&s,"gamma","gamma")
    }).unwrap_err().to_string(),Denial::OutcomeUnknown.to_string());
    assert_eq!(
        crate::jobs::record(&s, "gamma").unwrap()["status"],
        "running"
    );
    assert_eq!(
        apply_with(&s, &a, &c, 100, || panic!("never cancel replacement"))
            .unwrap_err()
            .to_string(),
        Denial::OutcomeUnknown.to_string()
    );
}

#[test]
fn concurrent_equal_calls_do_not_resend_signals_and_recover_final_result() {
    let (dir, s, c) = fixture();
    let a = args(&s, "alpha");
    let barrier = Arc::new(Barrier::new(2));
    let count = Arc::new(AtomicUsize::new(0));
    let path = dir.path().to_path_buf();
    let aa = a.clone();
    let cc = DelegatedContext::from_gateway(&c.gateway()).unwrap();
    let bb = barrier.clone();
    let n = count.clone();
    let worker = std::thread::spawn(move || {
        let s = Store::open(&path).unwrap();
        apply_with(&s, &aa, &cc, 100, || {
            n.fetch_add(1, Ordering::SeqCst);
            bb.wait();
            bb.wait();
            Ok(json!({"status":"failed"}))
        })
        .unwrap()
    });
    barrier.wait();
    assert_eq!(
        apply_with(&s, &a, &c, 100, || panic!("duplicate signal"))
            .unwrap_err()
            .to_string(),
        Denial::OutcomeUnknown.to_string()
    );
    barrier.wait();
    let result = worker.join().unwrap();
    assert_eq!(apply_at(&s, &a, &c, 100).unwrap(), result);
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[test]
fn delayed_native_completion_never_overwrites_new_admission_after_retention() {
    let (_dir, s, c) = fixture();
    let a = args(&s, "delayed");
    let newer = changed(&c, "result_id", json!(uuid::Uuid::new_v4().to_string()));
    let mut new_result = None;
    let late = apply_with(&s, &a, &c, 100, || {
        new_result = Some(
            apply_with(
                &s,
                &a,
                &newer,
                101 + super::super::super::idempotency::RETENTION_MS,
                || Ok(json!({"status":"running"})),
            )
            .unwrap(),
        );
        Ok(json!({"status":"failed"}))
    });
    assert_eq!(
        late.unwrap_err().to_string(),
        Denial::OutcomeUnknown.to_string()
    );
    let recovered = apply_at(
        &s,
        &a,
        &newer,
        101 + super::super::super::idempotency::RETENTION_MS,
    )
    .unwrap();
    assert_eq!(recovered, new_result.unwrap());
    assert_eq!(recovered["outcome"], "cancel_requested");
    assert_eq!(recovered["result_id"], newer.provenance()["result_id"]);
}

#[test]
fn long_lived_home_and_bounded_retention_share_v6a_store() {
    let (_dir, s, c) = fixture();
    {
        let tx = s.write_transaction().unwrap();
        for n in 0..3000 {
            insert(&s, &format!("history-{n}"), "alpha", "failed");
        }
        tx.commit().unwrap();
    }
    let a = args(&s, "alpha");
    let first = apply_at(&s, &a, &c, 100).unwrap();
    assert_eq!(apply_at(&s, &a, &c, 100).unwrap(), first);
    let second = changed(&c, "client", json!(uuid::Uuid::new_v4().to_string()));
    let mut missing = a.clone();
    missing["reference"]["id"] = json!("absent");
    assert_eq!(denied(&s, &missing, &second), Denial::Ship.to_string());
    for n in 1..256 {
        let a = args(&s, &format!("key-{n}"));
        apply_at(&s, &a, &c, 100).unwrap();
    }
    assert_eq!(
        denied(&s, &args(&s, "full"), &c),
        Denial::RetryCapacity.to_string()
    );
    assert_eq!(apply_at(&s, &a, &c, 100).unwrap(), first);
    let fresh = apply_at(
        &s,
        &args(&s, "fresh"),
        &c,
        101 + super::super::super::idempotency::RETENTION_MS,
    )
    .unwrap();
    assert_eq!(fresh["outcome"], "stopped");
}
