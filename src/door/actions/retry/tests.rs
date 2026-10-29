use super::*;

fn fixture() -> (tempfile::TempDir, Store, DelegatedContext, Value) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let s = Store::open(&root.join("alpha")).unwrap();
    let repo = root.join("beta");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(
        s.home().join("config.json"),
        json!({"resource_home":root.join("gamma")}).to_string(),
    )
    .unwrap();
    s.connection().execute("INSERT INTO charters(id,data) VALUES('alpha',?1)", [json!({"id":"alpha","active":true,"repo":repo,"actions":["test","recovery"],"job_timeout_secs":60}).to_string()]).unwrap();
    s.connection().execute_batch("INSERT INTO ships(name,record) VALUES('alpha','{\"location\":\"here\",\"first_mate\":{}}'),('beta','{\"location\":\"here\"}'); INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('alpha','alpha','alpha','alpha','open',0),('beta','alpha','beta','beta','open',0);").unwrap();
    let job = json!({"id":"gamma","task_id":"alpha","incarnation":"gamma","status":"failed","command":["false"],"cwd":repo,"action":"test","heavy":false,"heavy_declared":false,"timeout_secs":60,"ledger":root.join("gamma/resources.sqlite"),"created_ms":0,"finished_ms":1,"supervisor":null,"payload":null,"cancel_requested":false,"exit_code":1,"reason":null,"retry_of":null});
    s.connection()
        .execute(
            "INSERT INTO jobs(id,record) VALUES('gamma',?1)",
            [job.to_string()],
        )
        .unwrap();
    let c = DelegatedContext::resolved(
        uuid::Uuid::new_v4().to_string(),
        "door:alpha".into(),
        vec!["work:manage".into()],
        Some(vec!["alpha".into()]),
    );
    let a = json!({"ship":"alpha","reference":{"origin":inspection::origin(&s).unwrap(),"ship":"alpha","task_id":"alpha","id":"gamma","incarnation":"gamma"},"idempotency_key":"alpha"});
    (dir, s, c, a)
}
fn no_launch(_: jobs::PreparedStart) -> Result<Value> {
    panic!("must not launch")
}
fn result_rows(s: &Store) -> Vec<String> {
    s.connection()
        .prepare("SELECT result FROM door_work_results ORDER BY key_hash")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}
fn reservations(s: &Store) -> i64 {
    let root = s.home().parent().unwrap();
    let path = root.join("gamma/resources.sqlite");
    if !path.exists() {
        return 0;
    }
    let db = rusqlite::Connection::open(path).unwrap();
    db.query_row(
        "SELECT (SELECT count(*) FROM reservations)+(SELECT count(*) FROM queue)",
        [],
        |r| r.get(0),
    )
    .unwrap()
}
#[test]
fn closed_schema_scope_and_every_hidden_graph_edge_refuse_before_preparation() {
    for field in [
        "command", "cwd", "env", "route", "policy", "approval", "force", "estimate", "new_id",
        "pid",
    ] {
        let (_d, s, c, mut a) = fixture();
        a[field] = json!("canary");
        assert_eq!(
            apply_with(&s, &a, &c, 100, no_launch)
                .unwrap_err()
                .to_string(),
            Denial::Arguments.to_string()
        );
        assert!(result_rows(&s).is_empty());
        assert_eq!(reservations(&s), 0);
    }
    for sql in [
        "UPDATE tasks SET owner='beta' WHERE id='alpha'",
        "UPDATE tasks SET parent='beta' WHERE id='alpha'",
        "UPDATE jobs SET record=json_set(record,'$.incarnation','replaced') WHERE id='gamma'",
        "UPDATE jobs SET record=json_set(record,'$.agent_actor','beta') WHERE id='gamma'",
        "DELETE FROM jobs WHERE id='gamma'",
    ] {
        let (_d, s, c, a) = fixture();
        s.connection().execute_batch(sql).unwrap();
        assert_eq!(
            apply_with(&s, &a, &c, 100, no_launch)
                .unwrap_err()
                .to_string(),
            Denial::Ship.to_string()
        );
        assert!(result_rows(&s).is_empty());
        assert_eq!(reservations(&s), 0);
    }
    for field in ["started_by", "retry_of", "resume_of", "relaunch_of"] {
        for reverse in [false, true] {
            let (_d, s, c, a) = fixture();
            let mut hidden = jobs::record(&s, "gamma").unwrap();
            hidden["id"] = json!("hidden");
            hidden["task_id"] = json!("beta");
            if reverse {
                hidden[field] = json!("gamma");
            } else {
                s.connection()
                    .execute(
                        "UPDATE jobs SET record=json_set(record,?1,'hidden') WHERE id='gamma'",
                        [format!("$.{field}")],
                    )
                    .unwrap();
            }
            s.connection()
                .execute(
                    "INSERT INTO jobs(id,record) VALUES('hidden',?1)",
                    [hidden.to_string()],
                )
                .unwrap();
            assert_eq!(
                apply_with(&s, &a, &c, 100, no_launch)
                    .unwrap_err()
                    .to_string(),
                Denial::Ship.to_string()
            );
        }
    }
    let (_d, s, c, a) = fixture();
    let mut wire = c.gateway();
    wire["scopes"] = json!(["fleet:read", "approval:answer"]);
    let weak = DelegatedContext::from_gateway(&wire).unwrap();
    assert_eq!(
        apply_with(&s, &a, &weak, 100, no_launch)
            .unwrap_err()
            .to_string(),
        Denial::Scope.to_string()
    );
}
#[test]
fn native_eligibility_recovery_and_original_route_are_not_bypassed() {
    for status in [
        "starting",
        "queued",
        "running",
        "launching",
        "orphaned",
        "parked",
        "resumed",
        "succeeded",
    ] {
        let (_d, s, c, a) = fixture();
        s.connection()
            .execute(
                "UPDATE jobs SET record=json_set(record,'$.status',?1) WHERE id='gamma'",
                [status],
            )
            .unwrap();
        assert_eq!(
            apply_with(&s, &a, &c, 100, no_launch)
                .unwrap_err()
                .to_string(),
            Denial::ExecutionFailed.to_string()
        );
        assert!(result_rows(&s).is_empty());
        assert_eq!(reservations(&s), 0);
    }
    for sql in [
        "UPDATE charters SET data=json_set(data,'$.actions',json('[\"test\"]'))",
        "UPDATE charters SET data=json_set(data,'$.active',json('false'))",
        "UPDATE tasks SET status='done' WHERE id='alpha'",
    ] {
        let (_d, s, c, a) = fixture();
        s.connection().execute_batch(sql).unwrap();
        assert!(apply_with(&s, &a, &c, 100, no_launch).is_err());
        assert!(result_rows(&s).is_empty());
    }
}
#[test]
fn original_route_backoff_native_heavy_cap_and_launch_limits_are_preserved() {
    let (_d, s, c, a) = fixture();
    s.connection().execute_batch("UPDATE jobs SET record=json_set(record,'$.action','spawn','$.agent_actor','alpha','$.model_route','gamma') WHERE id='gamma'; UPDATE charters SET data=json_set(data,'$.actions',json('[\"spawn\",\"recovery\"]'));").unwrap();
    let ledger = jobs::host_ledger(&s).unwrap();
    let until = Store::now_ms() + 86_400_000;
    ledger.execute("INSERT INTO model_routes(route,failures,until_ms,last_error,updated_ms) VALUES('gamma',1,?1,'fixture refusal',?2)", params![until, Store::now_ms()]).unwrap();
    assert_eq!(
        apply_with(&s, &a, &c, 100, no_launch)
            .unwrap_err()
            .to_string(),
        Denial::ExecutionFailed.to_string()
    );
    assert!(result_rows(&s).is_empty());
    assert_eq!(
        ledger
            .query_row(
                "SELECT until_ms FROM model_routes WHERE route='gamma'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        until
    );
    for sql in [
        "UPDATE jobs SET record=json_set(record,'$.heavy',json('true'),'$.estimate',json('{\"cpus\":1,\"memory_bytes\":1048576}')) WHERE id='gamma'; UPDATE charters SET data=json_set(data,'$.max_heavy_jobs',0)",
        "UPDATE charters SET data=json_set(data,'$.job_timeout_secs',1)",
        "UPDATE jobs SET record=json_set(record,'$.action','deploy_production') WHERE id='gamma'",
    ] {
        let (_d, s, c, a) = fixture();
        s.connection().execute_batch(sql).unwrap();
        assert_eq!(
            apply_with(&s, &a, &c, 100, no_launch)
                .unwrap_err()
                .to_string(),
            Denial::ExecutionFailed.to_string()
        );
        assert!(result_rows(&s).is_empty());
        assert_eq!(reservations(&s), 0);
    }
}
#[test]
fn all_native_retryable_statuses_keep_payload_and_route_without_overrides() {
    for status in ["failed", "cancelled", "timed_out"] {
        let (_d, s, c, a) = fixture();
        // Route metadata belongs to native agent launches, not ordinary
        // command jobs. Exercise that native distinction rather than forging
        // route metadata on a non-agent record.
        s.connection().execute_batch("UPDATE charters SET data=json_set(data,'$.actions',json('[\"spawn\",\"recovery\"]'));").unwrap();
        s.connection().execute("UPDATE jobs SET record=json_set(record,'$.status',?1,'$.action','spawn','$.agent_actor','alpha','$.model_route','gamma','$.route_name','beta','$.route_why','alpha') WHERE id='gamma'",[status]).unwrap();
        let reply = apply_with(&s, &a, &c, 100, |_p| Ok(json!({}))).unwrap();
        let next = jobs::record(&s, reply["retry_job_id"].as_str().unwrap()).unwrap();
        assert_eq!(next["command"], json!(["false"]));
        assert_eq!(next["retry_of"], "gamma");
        assert_eq!(next["model_route"], "gamma");
        assert_eq!(next["route_name"], "beta");
        assert_eq!(next["route_why"], "alpha");
    }
}
#[test]
fn audit_failure_rolls_back_intent_and_explicitly_compensates_light_reservation() {
    let (_d, s, c, a) = fixture();
    s.connection().execute_batch("CREATE TRIGGER fail_retry BEFORE INSERT ON audit WHEN NEW.kind='door.work' BEGIN SELECT RAISE(ABORT,'audit unavailable'); END;").unwrap();
    assert!(apply_with(&s, &a, &c, 100, no_launch).is_err());
    assert!(result_rows(&s).is_empty());
    assert_eq!(reservations(&s), 0);
    assert!(
        !s.connection()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM jobs WHERE id!='gamma')",
                [],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
    );
}
#[test]
fn commit_before_launch_crash_restart_replay_and_hidden_history() {
    for crash in [false, true] {
        let (_d, s, c, a) = fixture();
        let first = apply_with(&s, &a, &c, 100, |_p| {
            assert!(s.connection().is_autocommit());
            let rows = result_rows(&s);
            assert_eq!(rows.len(), 1);
            let intent: Value = serde_json::from_str(&rows[0]).unwrap();
            assert_eq!(intent["outcome"], "outcome_unknown");
            let job = jobs::record(&s, intent["retry_job_id"].as_str().unwrap()).unwrap();
            assert_eq!(job["retry_of"], "gamma");
            assert_eq!(job["command"], json!(["false"]));
            assert!(
                s.connection()
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM audit WHERE kind='door.work')",
                        [],
                        |r| r.get::<_, bool>(0)
                    )
                    .unwrap()
            );
            if crash {
                anyhow::bail!("crash after admission");
            }
            Ok(json!({}))
        });
        let before = result_rows(&s);
        let home = s.home().to_owned();
        s.connection()
            .execute_batch("UPDATE tasks SET owner='beta' WHERE id='alpha'; DELETE FROM jobs;")
            .unwrap();
        drop(s);
        let s = Store::open(&home).unwrap();
        let replay = apply_with(&s, &a, &c, 100, no_launch);
        if crash {
            assert_eq!(
                first.unwrap_err().to_string(),
                Denial::OutcomeUnknown.to_string()
            );
            assert_eq!(
                replay.unwrap_err().to_string(),
                Denial::OutcomeUnknown.to_string()
            );
        } else {
            assert_eq!(first.unwrap(), replay.unwrap());
        }
        assert_eq!(result_rows(&s), before);
    }
}
#[test]
fn result_id_guards_late_completion() {
    let (_d, s, c, a) = fixture();
    let error = apply_with(&s, &a, &c, 100, |_p| {
        s.connection()
            .execute(
                "UPDATE door_work_results SET result=json_set(result,'$.result_id',?1)",
                [uuid::Uuid::new_v4().to_string()],
            )
            .unwrap();
        Ok(json!({}))
    })
    .unwrap_err();
    assert_eq!(error.to_string(), Denial::OutcomeUnknown.to_string());
    let v: Value = serde_json::from_str(&result_rows(&s)[0]).unwrap();
    assert_eq!(v["outcome"], "outcome_unknown");
}

#[test]
fn preparation_races_recheck_status_payload_graph_and_charter_under_admission_lock() {
    for sql in [
        "UPDATE jobs SET record=json_set(record,'$.status','orphaned') WHERE id='gamma'",
        "UPDATE jobs SET record=json_set(record,'$.command',json('[\"true\"]')) WHERE id='gamma'",
        "UPDATE tasks SET owner='beta' WHERE id='alpha'",
        "UPDATE charters SET data=json_set(data,'$.max_heavy_jobs',0) WHERE id='alpha'",
        "UPDATE charters SET data=json_set(data,'$.actions',json('[\"test\"]')) WHERE id='alpha'",
    ] {
        let (_d, s, c, a) = fixture();
        assert!(
            apply_phases(
                &s,
                &a,
                &c,
                100,
                || {
                    s.connection().execute_batch(sql)?;
                    Ok(())
                },
                no_launch
            )
            .is_err()
        );
        assert!(result_rows(&s).is_empty());
        assert_eq!(reservations(&s), 0);
        assert!(
            !s.connection()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM jobs WHERE id!='gamma')",
                    [],
                    |r| r.get::<_, bool>(0)
                )
                .unwrap()
        );
    }
}
#[test]
fn concurrent_equal_calls_commit_one_intent_and_launch_once() {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };
    let (_d, s, c, a) = fixture();
    let gate = Arc::new(Barrier::new(2));
    let launches = Arc::new(AtomicUsize::new(0));
    let home = s.home().to_owned();
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let gate = gate.clone();
            let launches = launches.clone();
            let home = home.clone();
            let a = a.clone();
            let wire = c.gateway();
            std::thread::spawn(move || {
                let s = Store::open(&home).unwrap();
                let c = DelegatedContext::from_gateway(&wire).unwrap();
                gate.wait();
                apply_with(&s, &a, &c, 100, |_p| {
                    launches.fetch_add(1, Ordering::SeqCst);
                    Ok(json!({}))
                })
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(launches.load(Ordering::SeqCst), 1);
    let good = results.iter().find_map(|r| r.as_ref().ok()).unwrap();
    for r in &results {
        match r {
            Ok(v) => assert_eq!(v, good),
            Err(e) => assert_eq!(e.to_string(), Denial::OutcomeUnknown.to_string()),
        }
    }
    assert_eq!(apply_with(&s, &a, &c, 100, no_launch).unwrap(), *good);
}
#[test]
fn in_window_caps_fail_closed_but_recorded_retry_still_recovers() {
    let (_d, s, c, a) = fixture();
    let first = apply_with(&s, &a, &c, 100, |_p| Ok(json!({}))).unwrap();
    for n in 0..255 {
        s.connection().execute("INSERT INTO door_work_results(grant_id,resource,key_hash,args_hash,result,created_ms) VALUES(?1,?2,?3,'alpha','{}',100)",params![c.client(),c.resource(),format!("fill-{n}")]).unwrap();
    }
    assert_eq!(apply_with(&s, &a, &c, 100, no_launch).unwrap(), first);
    let mut fresh = a.clone();
    fresh["idempotency_key"] = json!("beta");
    assert_eq!(
        apply_with(&s, &fresh, &c, 100, no_launch)
            .unwrap_err()
            .to_string(),
        Denial::RetryCapacity.to_string()
    );
}
