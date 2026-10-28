use super::*;

fn fixture() -> (tempfile::TempDir, Store, DelegatedContext) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.connection().execute_batch("INSERT INTO ships(name,record) VALUES('survey','{\"location\":\"here\",\"first_mate\":{}}');
        INSERT INTO charters(id,data) VALUES('survey','{\"id\":\"survey\",\"active\":true,\"actions\":[\"spawn\"]}');
        INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('survey','survey','survey','survey','open',0),('relay','survey','relay','hidden','open',0),('alpha','survey','survey','alpha','open',0);").unwrap();
    let c = DelegatedContext::resolved(
        uuid::Uuid::new_v4().to_string(),
        "door:alpha".into(),
        vec!["work:manage".into()],
        Some(vec!["survey".into()]),
    );
    (dir, store, c)
}
fn args(store: &Store, id: &str, key: &str) -> Value {
    json!({"ship":"survey","reference":references::get(store,"survey",id).unwrap().unwrap(),"idempotency_key":key})
}
fn run(s: &Store, command: Command, a: &Value, c: &DelegatedContext) -> Result<Value> {
    validate_args(command, a)?;
    apply(s, command, a, c)
}
fn snapshot(s: &Store) -> Vec<String> {
    let tables: Vec<String> = s.connection().prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").unwrap().query_map([],|r|r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
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
fn denied(s: &Store, command: Command, a: &Value, c: &DelegatedContext) -> String {
    let before = snapshot(s);
    let err = run(s, command, a, c).unwrap_err().to_string();
    assert_eq!(snapshot(s), before, "denial changed home: {err}");
    err
}
fn enqueue(s: &Store, id: &str, c: &DelegatedContext) {
    let mut a = args(s, id, &format!("enqueue-{id}"));
    a["brief"] = json!("Prepared assignment");
    run(s, Command::WorkEnqueue, &a, c).unwrap();
}
fn changed(c: &DelegatedContext, key: &str, value: Value) -> DelegatedContext {
    let mut wire = c.gateway();
    wire[key] = value;
    DelegatedContext::from_gateway(&wire).unwrap()
}
#[test]
fn fixed_schemas_exhaustively_map_native_variants_and_refuse_escapes() {
    let (_dir, s, _c) = fixture();
    for command in [Command::WorkRank, Command::WorkEnqueue, Command::WorkDefer] {
        let mut a = args(&s, "survey", "alpha");
        match command {
            Command::WorkRank => a["rank"] = json!(1),
            Command::WorkDefer => a["wait"] = json!({"kind":"manual","reason":"stop"}),
            _ => {}
        }
        assert!(validate_args(command, &a).is_ok());
        for field in [
            "command", "args", "argv", "owner", "operator", "payload", "id", "force", "route",
            "holder", "revision",
        ] {
            let mut b = a.clone();
            b[field] = json!("canary");
            assert!(validate_args(command, &b).is_err(), "{field}");
        }
        for field in ["ship", "reference", "idempotency_key"] {
            let mut b = a.clone();
            b.as_object_mut().unwrap().remove(field);
            assert!(validate_args(command, &b).is_err());
        }
        for value in [
            Value::Null,
            json!(1),
            json!(""),
            json!("x".repeat(129)),
            json!("a\nb"),
            json!("é"),
        ] {
            let mut b = a.clone();
            b["idempotency_key"] = value;
            assert!(validate_args(command, &b).is_err());
        }
        for (field, value) in [
            ("revision", json!(-1)),
            ("revision", json!(1.5)),
            ("origin", json!("/tmp")),
            ("ship", json!("relay")),
            ("extra", json!(1)),
        ] {
            let mut b = a.clone();
            b["reference"][field] = value;
            assert!(validate_args(command, &b).is_err());
        }
    }
    let mut a = args(&s, "survey", "wait");
    for wait in [
        json!({"kind":"task","reason":"dependency","ref":"alpha"}),
        json!({"kind":"request","reason":"answer","ref":"alpha"}),
        json!({"kind":"remote","reason":"far","ref":{"ship":"relay","task":"alpha","machine":"beta"}}),
        json!({"kind":"time","reason":"deadline","until_ms":0}),
        json!({"kind":"attention","reason":"reading","until_ms":0}),
        json!({"kind":"manual","reason":"stop"}),
    ] {
        a["wait"] = wait;
        assert!(validate_args(Command::WorkDefer, &a).is_ok());
        let mut b = a.clone();
        b["wait"]["end"] = json!({"status":"done"});
        assert!(validate_args(Command::WorkDefer, &b).is_err());
    }
    for wait in [
        json!({"kind":"manual","reason":" "}),
        json!({"kind":"task","reason":"dep"}),
        json!({"kind":"remote","reason":"far","ref":{"ship":"relay","task":"alpha","owner":"survey"}}),
        json!({"kind":"time","reason":"clock","until_ms":"later"}),
        json!({"kind":"rpc","reason":"escape"}),
    ] {
        a["wait"] = wait;
        assert!(validate_args(Command::WorkDefer, &a).is_err());
    }
}
#[test]
fn native_parity_atomic_result_restart_replay_and_cross_operation_key_conflicts() {
    let (dir, s, c) = fixture();
    // Retained history must not become a pre-filter mutation budget.
    let tx = s.write_transaction().unwrap();
    for n in 0..3000 {
        tx.execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES(?1,'survey','relay','history','done',0)",[format!("beta-{n}")]).unwrap();
    }
    tx.commit().unwrap();
    let mut a = args(&s, "survey", "rank");
    a["rank"] = json!(7);
    let first = run(&s, Command::WorkRank, &a, &c).unwrap();
    assert_eq!(first["result_id"], c.provenance()["result_id"]);
    assert_eq!(
        s.connection()
            .query_row("SELECT rank FROM tasks WHERE id='survey'", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        7
    );
    assert!(first.get("backlog").is_none());
    let after = snapshot(&s);
    // Drop/reopen simulates commit succeeded but response was lost. A new
    // admitted request/result ID must return the original durable reply.
    drop(s);
    let s = Store::open(dir.path()).unwrap();
    let retry = changed(&c, "result_id", json!(uuid::Uuid::new_v4().to_string()));
    assert_eq!(run(&s, Command::WorkRank, &a, &retry).unwrap(), first);
    assert_eq!(snapshot(&s), after);
    let mut b = a.clone();
    b["rank"] = Value::Null;
    assert_eq!(
        denied(&s, Command::WorkRank, &b, &c),
        Denial::Arguments.to_string()
    );
    b = a.clone();
    b.as_object_mut().unwrap().remove("rank");
    b["brief"] = json!("assignment");
    assert_eq!(
        denied(&s, Command::WorkEnqueue, &b, &c),
        Denial::Arguments.to_string()
    );
    let revised = changed(&c, "visibility_revision", json!(1));
    assert_eq!(
        denied(&s, Command::WorkRank, &a, &revised),
        Denial::Arguments.to_string()
    );
    enqueue(&s, "survey", &c);
    let mut defer = args(&s, "survey", "defer");
    defer["wait"] = json!({"kind":"manual","reason":"stop"});
    run(&s, Command::WorkDefer, &defer, &c).unwrap();
    let raw: String = s
        .connection()
        .query_row(
            "SELECT record FROM task_schedule WHERE task_id='survey'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&raw).unwrap()["state"],
        "stopped"
    );
    let native = json!({"id":"alpha","rank":3});
    crate::core::task_rank(&s, &native).unwrap();
    assert_eq!(
        s.connection()
            .query_row("SELECT rank FROM tasks WHERE id='alpha'", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        3
    );
    let audit: String = s
        .connection()
        .query_row(
            "SELECT payload FROM audit WHERE kind='door.work' ORDER BY seq LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        audit.contains("idempotency_key_hash")
            && audit.contains("reference")
            && audit.contains(c.client())
    );
}
#[test]
fn stale_custody_offer_and_policy_changes_never_mutate() {
    let (_dir, s, c) = fixture();
    let mut a = args(&s, "survey", "rank");
    a["rank"] = json!(1);
    // A change and restoration in the same millisecond is still stale.
    s.connection().execute_batch("UPDATE tasks SET owner='relay' WHERE id='survey';UPDATE tasks SET owner='survey' WHERE id='survey';").unwrap();
    assert_eq!(
        denied(&s, Command::WorkRank, &a, &c),
        Denial::Ship.to_string()
    );
    a = args(&s, "survey", "rank");
    a["rank"] = json!(1);
    s.connection()
        .execute("UPDATE charters SET data=data", [])
        .unwrap();
    assert_eq!(
        denied(&s, Command::WorkRank, &a, &c),
        Denial::Ship.to_string()
    );
    enqueue(&s, "survey", &c);
    a = args(&s, "survey", "defer");
    a["wait"] = json!({"kind":"manual","reason":"hold"});
    s.connection().execute("UPDATE task_schedule SET record=json_set(record,'$.state','claimed','$.offer_id','alpha') WHERE task_id='survey'",[]).unwrap();
    assert_eq!(
        denied(&s, Command::WorkDefer, &a, &c),
        Denial::Ship.to_string()
    );
    a = args(&s, "survey", "defer");
    a["wait"] = json!({"kind":"manual","reason":"hold"});
    assert_eq!(
        denied(&s, Command::WorkDefer, &a, &c),
        Denial::ExecutionFailed.to_string()
    );
    a["offer_id"] = json!("wrong");
    denied(&s, Command::WorkDefer, &a, &c);
    a["offer_id"] = json!("alpha");
    run(&s, Command::WorkDefer, &a, &c).unwrap();
    let mut old = args(&s, "alpha", "old-incarnation");
    old["rank"] = Value::Null;
    s.connection().execute_batch("DELETE FROM tasks WHERE id='alpha'; INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('alpha','survey','survey','alpha','open',0);").unwrap();
    assert_eq!(
        denied(&s, Command::WorkRank, &old, &c),
        Denial::Ship.to_string()
    );
    let mut clear = args(&s, "alpha", "clear");
    clear["rank"] = Value::Null;
    run(&s, Command::WorkRank, &clear, &c).unwrap();
}
#[test]
fn hidden_targets_dependencies_and_unequal_grants_are_nonrevealing() {
    let (_dir, s, c) = fixture();
    let mut a = args(&s, "survey", "rank");
    a["rank"] = json!(1);
    let weak = changed(&c, "scopes", json!(["fleet:read"]));
    assert_eq!(
        denied(&s, Command::WorkRank, &a, &weak),
        Denial::Scope.to_string()
    );
    let narrow = changed(&c, "ships", json!(["relay"]));
    assert_eq!(
        denied(&s, Command::WorkRank, &a, &narrow),
        Denial::Ship.to_string()
    );
    let mut hidden = a.clone();
    hidden["reference"]["task_id"] = json!("relay");
    let h = denied(&s, Command::WorkRank, &hidden, &c);
    hidden["reference"]["task_id"] = json!("missing");
    assert_eq!(h, denied(&s, Command::WorkRank, &hidden, &c));
    enqueue(&s, "survey", &c);
    let mut a = args(&s, "survey", "defer");
    a["wait"] = json!({"kind":"task","reason":"dependency","ref":"relay"});
    let h = denied(&s, Command::WorkDefer, &a, &c);
    a["wait"]["ref"] = json!("missing");
    assert_eq!(h, denied(&s, Command::WorkDefer, &a, &c));
    a["wait"] = json!({"kind":"remote","reason":"far","ref":{"ship":"relay","task":"alpha"}});
    assert_eq!(h, denied(&s, Command::WorkDefer, &a, &c));
    enqueue(&s, "alpha", &c);
    s.connection().execute("UPDATE task_schedule SET record=json_set(record,'$.wait',json(?1)) WHERE task_id='alpha'", [json!({"kind":"remote","reason":"leaf","ref":{"ship":"relay","task":"alpha"}}).to_string()]).unwrap();
    a["wait"] = json!({"kind":"task","reason":"indirect","ref":"alpha"});
    assert_eq!(
        h,
        denied(&s, Command::WorkDefer, &a, &c),
        "task chains must check remote/request leaves too"
    );
    let strong = changed(&c, "ships", json!(["survey", "relay"]));
    a["wait"] = json!({"kind":"task","reason":"visible dependency","ref":"relay"});
    run(&s, Command::WorkDefer, &a, &strong).unwrap();
    s.connection()
        .execute("UPDATE tasks SET parent='relay' WHERE id='survey'", [])
        .unwrap();
    let mut a = args(&s, "survey", "rank-parent");
    a["rank"] = json!(1);
    assert_eq!(h, denied(&s, Command::WorkRank, &a, &c));
}
#[test]
fn native_cycles_stopped_ancestors_live_direct_agents_and_wait_rules_survive() {
    let (_dir, s, c) = fixture();
    enqueue(&s, "survey", &c);
    enqueue(&s, "alpha", &c);
    let mut a = args(&s, "survey", "wait-alpha");
    a["wait"] = json!({"kind":"task","reason":"dep","ref":"alpha"});
    run(&s, Command::WorkDefer, &a, &c).unwrap();
    let mut a = args(&s, "alpha", "cycle");
    a["wait"] = json!({"kind":"task","reason":"dep","ref":"survey"});
    assert_eq!(
        denied(&s, Command::WorkDefer, &a, &c),
        Denial::ExecutionFailed.to_string()
    );
    a["wait"] = json!({"kind":"time","reason":"clock","until_ms":0});
    denied(&s, Command::WorkDefer, &a, &c);
    a["wait"] = json!({"kind":"manual","reason":"stop"});
    run(&s, Command::WorkDefer, &a, &c).unwrap();
    s.connection()
        .execute("UPDATE tasks SET parent='alpha' WHERE id='survey'", [])
        .unwrap();
    let mut a = args(&s, "survey", "stopped-parent");
    a["brief"] = json!("assignment");
    denied(&s, Command::WorkEnqueue, &a, &c);
    s.connection()
        .execute("UPDATE tasks SET parent=NULL WHERE id='survey'", [])
        .unwrap();
    for status in ["starting", "queued", "launching", "running", "orphaned"] {
        s.connection().execute("INSERT OR REPLACE INTO jobs(id,record) VALUES('direct',?1)",[json!({"id":"direct","task_id":"survey","agent_actor":"survey","status":status}).to_string()]).unwrap();
        let mut a = args(&s, "survey", status);
        a["brief"] = json!("assignment");
        denied(&s, Command::WorkEnqueue, &a, &c);
        a.as_object_mut().unwrap().remove("brief");
        a["wait"] = json!({"kind":"manual","reason":"stop"});
        denied(&s, Command::WorkDefer, &a, &c);
    }
    s.connection().execute("DELETE FROM jobs", []).unwrap();
    s.connection().execute("INSERT INTO mail(id,sender,recipient,kind,task_id,body,payload,urgency,created_ms,state) VALUES('ask','survey','captain','request','survey','question','{}','normal',0,'open')",[]).unwrap();
    let mut a = args(&s, "survey", "request");
    a["wait"] = json!({"kind":"request","reason":"answer","ref":"ask"});
    run(&s, Command::WorkDefer, &a, &c).unwrap();
    let mut a = args(&s, "alpha", "wrong-request");
    a["wait"] = json!({"kind":"request","reason":"answer","ref":"ask"});
    denied(&s, Command::WorkDefer, &a, &c);
}

#[test]
fn concurrent_equal_keys_apply_once_and_post_native_audit_failure_rolls_back() {
    let (dir, s, c) = fixture();
    let mut a = args(&s, "survey", "rank");
    a["rank"] = json!(1);
    s.connection().execute_batch("CREATE TRIGGER fail_work_audit BEFORE INSERT ON audit WHEN NEW.kind='door.work' BEGIN SELECT RAISE(ABORT,'fixture failure');END;").unwrap();
    denied(&s, Command::WorkRank, &a, &c);
    s.connection()
        .execute_batch("DROP TRIGGER fail_work_audit")
        .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let workers = (0..2)
        .map(|_| {
            let path = dir.path().to_owned();
            let a = a.clone();
            let barrier = barrier.clone();
            let mut wire = c.gateway();
            wire["result_id"] = json!(uuid::Uuid::new_v4().to_string());
            std::thread::spawn(move || {
                let s = Store::open(&path).unwrap();
                let c = DelegatedContext::from_gateway(&wire).unwrap();
                barrier.wait();
                run(&s, Command::WorkRank, &a, &c).unwrap()
            })
        })
        .collect::<Vec<_>>();
    let results = workers
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results[0], results[1]);
    assert_eq!(
        s.connection()
            .query_row(
                "SELECT count(*) FROM audit WHERE kind='task.rank'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(
        s.connection()
            .query_row("SELECT count(*) FROM door_work_results", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn native_assignment_fallback_and_time_attention_remote_waits_keep_native_contract() {
    let (_dir, mut s, c) = fixture();
    let a = args(&s, "survey", "no-note");
    denied(&s, Command::WorkEnqueue, &a, &c);
    crate::context::dispatch(
        &mut s,
        "note.put",
        json!({"ship":"survey","focus":"assignment:survey","body":"Prepared note"}),
    )
    .unwrap();
    run(&s, Command::WorkEnqueue, &a, &c).unwrap();
    for kind in ["time", "attention"] {
        let mut a = args(&s, "survey", kind);
        a["wait"] = json!({"kind":kind,"reason":"bounded hold","until_ms":Store::now_ms().saturating_add(86_400_000)});
        run(&s, Command::WorkDefer, &a, &c).unwrap();
    }
    // A remote wait validates the configured route and subscribes through the
    // native outbox; it does not claim the far task has been observed.
    s.connection()
        .execute(
            "INSERT INTO ships(name,record) VALUES('relay',?1)",
            [json!({"location":{"peer":"beta"}}).to_string()],
        )
        .unwrap();
    s.connection()
        .execute(
            "INSERT INTO peer_endpoints(name,config_json) VALUES('beta',?1)",
            [json!({"name":"beta","command":["false"]}).to_string()],
        )
        .unwrap();
    let all = changed(&c, "ships", Value::Null);
    let mut a = args(&s, "survey", "remote");
    a["wait"] = json!({"kind":"remote","reason":"far work","ref":{"ship":"relay","task":"alpha"}});
    run(&s, Command::WorkDefer, &a, &all).unwrap();
    let raw: String = s
        .connection()
        .query_row(
            "SELECT record FROM task_schedule WHERE task_id='survey'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let schedule: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(schedule["wait"]["ref"]["ship"], "relay");
    assert!(schedule["subscription"].is_object());
    for kind in ["task.enqueue", "task.defer"] {
        let mut stmt = s
            .connection()
            .prepare("SELECT payload FROM audit WHERE kind=?1")
            .unwrap();
        for row in stmt.query_map([kind], |r| r.get::<_, String>(0)).unwrap() {
            let row = row.unwrap();
            assert!(
                !row.contains("Prepared note")
                    && !row.contains("bounded hold")
                    && !row.contains("far work")
            );
        }
    }
}

#[test]
fn receiving_action_and_read_gateways_have_disjoint_fixed_allowlists() {
    let (_dir, _s, c) = fixture();
    for operation in [
        "task.rank",
        "task.enqueue",
        "task.defer",
        "ship_work",
        "job.start",
        "work.poll",
        "work_rank;task.enqueue",
    ] {
        let err =
            serve(&json!({"context":c.gateway(),"operation":operation,"args":{}})).unwrap_err();
        assert_eq!(err.downcast_ref::<Denial>(), Some(&Denial::Arguments));
    }
    for operation in ["work_rank", "work_enqueue", "work_defer"] {
        let mut wire = c.gateway();
        wire["scopes"] = json!(["fleet:read"]);
        let err = super::super::inspection::serve(
            &json!({"context":wire,"operation":operation,"args":{}}),
        )
        .unwrap_err();
        assert_eq!(err.downcast_ref::<Denial>(), Some(&Denial::Arguments));
    }
}
