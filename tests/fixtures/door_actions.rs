// V6a crosses the actual scoped adapter, core admission and linked subprocess.
fn work_reference(fleet: &Fleet, token: &str, ship: &str, task: &str) -> Value {
    let page = delegated_ok(fleet, token, "ship_work", json!({"ship":ship,"limit":50}));
    page["work"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["task_id"] == task)
        .unwrap()["reference"]
        .clone()
}
#[test]
fn guarded_work_actions_real_v2_matrix_native_parity_and_restart_replay() {
    let fleet = Fleet::with_ships(&[("survey", true)]);
    let home = fleet.home("survey");
    let db = rusqlite::Connection::open(home.join("privateer.sqlite")).unwrap();
    db.execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('alpha','survey','survey','alpha','open',0)",[]).unwrap();
    delegated_adapter(&fleet);
    let (_, read) = delegated_issue(&fleet, "alpha", "fleet:read", Some("survey"));
    let (_, manage) = delegated_issue(&fleet, "beta", "work:manage", Some("survey"));
    let (_, other) = delegated_issue(&fleet, "gamma", "work:manage", Some("relay"));
    let (_, full) = delegated_issue(&fleet, "lookout", ALL_DELEGATED_SCOPES, Some("survey"));
    let supervisor = fleet.supervise();
    fleet.wait("work hello", |e| e.first().cloned());
    let reference = work_reference(&fleet, &read, "survey", "alpha");
    assert!(reference.is_object());
    for (command, extra) in [
        ("work_rank", json!({"rank":1})),
        ("work_enqueue", json!({"brief":"assignment"})),
        (
            "work_defer",
            json!({"wait":{"kind":"manual","reason":"stop"}}),
        ),
    ] {
        let mut a = json!({"ship":"survey","reference":reference,"idempotency_key":command});
        for (k, v) in extra.as_object().unwrap() {
            a[k] = v.clone();
        }
        credential_request(&fleet, &read, command, a.clone(), "insufficient_scope");
        credential_request(&fleet, &other, command, a.clone(), "ship_not_allowed");
        for field in ["argv", "command", "route", "force", "payload", "holder"] {
            let mut bad = a.clone();
            bad[field] = json!("canary");
            credential_request(&fleet, &manage, command, bad, "unsupported_arguments");
        }
        for field in ["owner", "operator", "delegated_context", "grant"] {
            let mut bad = a.clone();
            bad[field] = json!("forged");
            credential_request(&fleet, &manage, command, bad, "reserved_identity");
        }
        let mut missing = a;
        missing.as_object_mut().unwrap().remove("idempotency_key");
        credential_request(&fleet, &manage, command, missing, "unsupported_arguments");
    }
    // Raw native commands, CLI-style aliases, generic RPCs and every gateway
    // family stay outside the external door allowlist even with work authority.
    for command in [
        "task.rank",
        "task.enqueue",
        "task.defer",
        "task_rank",
        "task_enqueue",
        "task_defer",
        "work.rank",
        "work.enqueue",
        "work.defer",
        "dispatch",
        "rpc",
        "machine.call",
        "peer.call",
        "fleet.work-action.v1",
        "fleet.inspection.v1",
        "fleet.runtime.v1",
        "door.delegated.v1",
        "door.desk.v1",
        "door.admin.v1",
        "work.poll",
        "work.started",
        "work.settled",
        "work.release",
        "job.start",
        "job.cancel",
        "job.retry",
        "job_retry",
        "job.cancel_exact",
        "agent.start",
        "approval.grant",
        "routes.set",
    ] {
        for token in [&manage, &full] {
            credential_request(&fleet, token, command, json!({}), "unknown_command");
        }
    }
    let rank = json!({"ship":"survey","reference":reference,"rank":5,"idempotency_key":"rank"});
    let first = delegated_ok(&fleet, &manage, "work_rank", rank.clone());
    assert_eq!(first["schema"], "work.action.v1");
    assert_eq!(first["applied"], true);
    assert_eq!(first["provenance"]["result_id"], first["result_id"]);
    assert!(first.get("backlog").is_none());
    let native = fleet.call("survey", "task.backlog", json!({"ship":"survey"}));
    assert!(native.to_string().contains("alpha"));
    assert_eq!(
        db.query_row("SELECT rank FROM tasks WHERE id='alpha'", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        5
    );
    assert_eq!(
        delegated_ok(&fleet, &manage, "work_rank", rank.clone()),
        first
    );
    // A supervisor restart cannot change the committed result or reapply rank.
    stop(supervisor);
    let supervisor = fleet.supervise();
    assert_eq!(
        delegated_ok(&fleet, &manage, "work_rank", rank.clone()),
        first
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM audit WHERE kind='task.rank'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    let mut different = rank.clone();
    different["rank"] = json!(6);
    credential_request(
        &fleet,
        &manage,
        "work_rank",
        different,
        "unsupported_arguments",
    );
    let enqueue = json!({"ship":"survey","reference":work_reference(&fleet,&read,"survey","alpha"),"brief":"prepared","idempotency_key":"enqueue"});
    let enqueued = delegated_ok(&fleet, &manage, "work_enqueue", enqueue.clone());
    assert_eq!(
        delegated_ok(&fleet, &manage, "work_enqueue", enqueue),
        enqueued
    );
    let defer = json!({"ship":"survey","reference":work_reference(&fleet,&read,"survey","alpha"),"wait":{"kind":"manual","reason":"hold"},"idempotency_key":"defer"});
    let deferred = delegated_ok(&fleet, &manage, "work_defer", defer.clone());
    assert_eq!(delegated_ok(&fleet, &manage, "work_defer", defer), deferred);
    let schedule: Value = serde_json::from_str(
        &db.query_row::<String, _, _>(
            "SELECT record FROM task_schedule WHERE task_id='alpha'",
            [],
            |r| r.get(0),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(schedule["state"], "stopped");
    let audit: String = db
        .query_row(
            "SELECT payload FROM audit WHERE kind='door.work' ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(audit.contains("idempotency_key_hash"));
    assert!(!audit.contains("prepared"));
    stop(supervisor);
}

#[test]
fn guarded_enqueue_refuses_live_agent_and_exact_cancel_stops_real_escaped_descendant() {
    let fleet = Fleet::with_ships(&[("survey", true)]);
    let db = rusqlite::Connection::open(fleet.home("survey").join("privateer.sqlite")).unwrap();
    db.execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('alpha','survey','survey','alpha','open',0)",[]).unwrap();
    delegated_adapter(&fleet);
    let (_, token) = delegated_issue(&fleet, "alpha", "fleet:read,work:manage", Some("survey"));
    let supervisor = fleet.supervise();
    fleet.wait("direct work hello", |e| e.first().cloned());
    // A scripted planner through native agent.start, with a real sleeping
    // payload. No provider route is configured/changed; this is subprocess
    // custody evidence, not a claim of autonomous agent behavior.
    let planner = fleet.root.join("lookout.py");
    let payload = fleet.root.join("payload.py");
    fs::write(&payload, "import subprocess,sys,time\np=subprocess.Popen([sys.executable,'-c',\"import os,signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); open('descendant.pid','w').write(str(os.getpid())); time.sleep(60)\"],start_new_session=True)\ntime.sleep(60)\n").unwrap();
    let argv = json!([support::python(),payload]);
    fs::write(&planner, format!("import json,sys\nr=json.loads(sys.stdin.readline())\nprint(json.dumps({{'id':r['id'],'version':1,'ok':True,'result':{{'argv':{argv},'cwd':r['context']['repo'],'env_names':[]}}}}))\n")).unwrap();
    let manifest = fleet.root.join("lookout.json");
    fs::write(&manifest, json!({"schema_version":1,"id":"lookout","executable":[support::python(),planner],"timeout_ms":2000,"max_output_bytes":65536}).to_string()).unwrap();
    fleet.call("survey", "resource.configure", json!({"min_available_memory_bytes":0,"min_free_disk_bytes":0,"crew_default_memory_bytes":1048576}));
    let direct = fleet.call("survey", "agent.start", json!({"id":"direct","task_id":"alpha","manifest":manifest,"heavy":false,"timeout_secs":60}));
    assert_eq!(direct["action"], "spawn");
    assert_eq!(direct["agent_actor"], "survey");
    struct Cleanup<'a>(&'a Fleet);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let home = self.0.home("survey");
            let _ = self
                .0
                .command(&self.0.root)
                .args([
                    "--home",
                    home.to_str().unwrap(),
                    "job.cancel",
                    "--json",
                    r#"{"id":"direct"}"#,
                ])
                .output();
        }
    }
    let _cleanup = Cleanup(&fleet);
    let deadline = Instant::now() + Duration::from_secs(15);
    let payload_pid = loop {
        let record: Value = serde_json::from_str(
            &db.query_row::<String, _, _>("SELECT record FROM jobs WHERE id='direct'", [], |r| {
                r.get(0)
            })
            .unwrap(),
        )
        .unwrap();
        if record["status"] == "running" && record["payload"].is_object() {
            break record["payload"]["pid"].as_u64().unwrap() as i32;
        }
        assert!(
            Instant::now() < deadline,
            "direct payload never ran: {record}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let a = json!({"ship":"survey","reference":work_reference(&fleet,&token,"survey","alpha"),"brief":"assignment","idempotency_key":"direct-enqueue"});
    credential_request(&fleet, &token, "work_enqueue", a, "execution_failed");
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM task_schedule WHERE task_id='alpha'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let descendant = loop {
        let record = fleet.call("survey", "job.status", json!({"id":"direct"}));
        let path = std::path::Path::new(record["cwd"].as_str().unwrap()).join("descendant.pid");
        if let Ok(pid) = fs::read_to_string(path)
            && record["escaped"].as_array().is_some_and(|v| !v.is_empty()) {
            break pid.parse::<i32>().unwrap();
        }
        assert!(Instant::now()<deadline,"escaped descendant not recorded: {record}");
        std::thread::sleep(Duration::from_millis(20));
    };
    let (_, manage) = delegated_issue(&fleet, "beta", "work:manage", Some("survey"));
    let (_, other) = delegated_issue(&fleet, "gamma", "work:manage", Some("relay"));
    let page = delegated_ok(&fleet,&token,"ship_jobs",json!({"ship":"survey"}));
    let reference = page["jobs"].as_array().unwrap().iter().find(|j|j["reference"]["id"]=="direct").unwrap()["reference"].clone();
    let cancel = json!({"ship":"survey","reference":reference,"idempotency_key":"cancel-direct"});
    credential_request(&fleet,&other,"job_cancel",cancel.clone(),"ship_not_allowed");
    for field in ["pid","pgid","argv","route","command","force","new_id"] {
        let mut invalid=cancel.clone();invalid[field]=json!(1);
        credential_request(&fleet,&manage,"job_cancel",invalid,"unsupported_arguments");
    }
    assert_eq!(fleet.call("survey","job.status",json!({"id":"direct"}))["cancel_requested"],false);
    let first = delegated_ok(&fleet,&manage,"job_cancel",cancel.clone());
    assert!(matches!(first["outcome"].as_str(),Some("cancel_requested"|"stopped")));
    assert_eq!(delegated_ok(&fleet,&manage,"job_cancel",cancel.clone()),first);
    let ledger=rusqlite::Connection::open(fleet.root.join("ledger/resources.sqlite")).unwrap();
    let current=fleet.call("survey","job.status",json!({"id":"direct"}));
    if !["cancelled","failed","succeeded","timed_out"].iter().any(|s|current["status"]==*s)
        && ledger.query_row("SELECT count(*) FROM reservations WHERE json_extract(record,'$.job_id')='direct'",[],|r|r.get::<_,i64>(0)).unwrap()==0 {
        let settled=fleet.call("survey","job.status",json!({"id":"direct"}));
        assert!(["cancelled","failed","succeeded","timed_out"].iter().any(|s|settled["status"]==*s),"nonterminal descendant custody lost its hold");
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let status = fleet.call("survey", "job.status", json!({"id":"direct"}));
        if ["cancelled", "succeeded", "failed", "timed_out"]
            .iter()
            .any(|s| status["status"] == *s)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "direct payload not settled: {status}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while unsafe { libc::kill(payload_pid, 0) } == 0 {
        assert!(
            Instant::now() < deadline,
            "direct payload survived native cancellation"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let deadline=Instant::now()+Duration::from_secs(30);
    while std::process::Command::new("ps").args(["-o","stat=","-p",&descendant.to_string()]).output().is_ok_and(|o|o.status.success() && !o.stdout.is_empty() && !o.stdout.starts_with(b"Z")) {
        assert!(Instant::now()<deadline,"escaped descendant survived cancellation");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(ledger.query_row("SELECT count(*) FROM reservations WHERE json_extract(record,'$.job_id')='direct'",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    assert_eq!(delegated_ok(&fleet,&manage,"job_cancel",cancel.clone()),first,"replay is historical, not a fresh stopped claim");
    stop(supervisor);
    let supervisor=fleet.supervise();
    assert_eq!(delegated_ok(&fleet,&manage,"job_cancel",cancel),first);
    stop(supervisor);
}

#[test]
fn guarded_work_linked_receiver_allowlist_revoke_release_and_unsupported_peer() {
    let fleet = Fleet::with_ships(&[("survey", true)]);
    let ssh = fleet.root.join("fake-ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\nfor argument do remote=$argument; done\nexec /bin/sh -c \"$remote\"\n",
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    let root = fleet.root.join("beta");
    let home = root.join("relay");
    fs::create_dir_all(&home).unwrap();
    let output = fleet
        .command(&home)
        .env("HOME", &root)
        .env("PRIVATEER_FLEET", root.join("fleet.sqlite"))
        .args(["init", "--ship", "relay", "--harness", "codex"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fleet.ok(&[
        "machine",
        "add",
        "beta",
        "fixture",
        "--executable",
        BIN,
        "--remote-fleet",
        root.join("fleet.sqlite").to_str().unwrap(),
        "--as",
        "alpha",
    ]);
    let db = rusqlite::Connection::open(home.join(".privateer/privateer.sqlite")).unwrap();
    db.execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('alpha','relay','relay','alpha','open',0)",[]).unwrap();
    delegated_adapter(&fleet);
    let (grant, token) = delegated_issue(&fleet, "alpha", "fleet:read,work:manage", Some("relay"));
    let supervisor = fleet.supervise();
    fleet.wait("linked work hello", |e| e.first().cloned());
    let reference = work_reference(&fleet, &token, "relay", "alpha");
    let calls = fleet.root.join("action-calls");
    let mode = fleet.root.join("action-mode");
    fs::write(&mode, "live").unwrap();
    let registry = fleet.root.join("state/fleet.sqlite");
    fs::write(&ssh,format!(r#"#!/usr/bin/env python3
import json,pathlib,subprocess,sys,sqlite3
r=json.loads(sys.stdin.readline())
with open({calls:?},'a') as f:f.write(r['command']+'\n')
if r['command']!='fleet.work-action.v1':sys.exit(73)
mode=pathlib.Path({mode:?}).read_text()
if mode=='unsupported':
 print(json.dumps({{'id':r['id'],'ok':False,'error':'unknown gateway command fleet.work-action.v1'}}));sys.exit()
if mode=='v6a':
 print(json.dumps({{'id':r['id'],'ok':False,'error':'unsupported_arguments'}}));sys.exit()
if mode=='scope':r['args']['context']['scopes']=[]
if mode=='ships':r['args']['context']['ships']=[]
if mode=='read-gateway':r['command']='fleet.inspection.v1'
p=subprocess.run(['/bin/sh','-c',sys.argv[-1]],input=json.dumps(r)+'\n',text=True,capture_output=True)
if p.returncode:sys.exit(p.returncode)
if mode=='release':
 c=sqlite3.connect({registry:?});c.execute('UPDATE door_clients SET revoked_ms=0 WHERE id=?',({grant:?},));c.commit()
print(p.stdout,end='')
"#,calls=calls.to_str().unwrap(),mode=mode.to_str().unwrap(),registry=registry.to_str().unwrap(),grant=grant["clients"][0]["id"].as_str().unwrap())).unwrap();
    let rank = json!({"ship":"relay","reference":reference,"rank":1,"idempotency_key":"rank"});
    for (setting, reason) in [
        ("scope", "insufficient_scope"),
        ("ships", "ship_not_allowed"),
        ("unsupported", "unsupported"),
        ("read-gateway", "insufficient_scope"),
    ] {
        fs::write(&mode, setting).unwrap();
        // The read gateway refuses actions too; its fleet scope happens to be
        // present, so that mode is checked separately below.
        if setting == "read-gateway" {
            continue;
        }
        credential_request(&fleet, &token, "work_rank", rank.clone(), reason);
        assert!(
            db.query_row::<Option<i64>, _, _>("SELECT rank FROM tasks WHERE id='alpha'", [], |r| r
                .get(0))
                .unwrap()
                .is_none()
        );
    }
    assert!(
        fs::read_to_string(&calls)
            .unwrap()
            .lines()
            .all(|command| command == "fleet.work-action.v1"),
        "no unsupported/denied action may retry as operator"
    );
    fs::write(&mode, "read-gateway").unwrap();
    credential_request(
        &fleet,
        &token,
        "work_rank",
        rank.clone(),
        "unsupported_arguments",
    );
    fs::write(&mode, "live").unwrap();
    let first = delegated_ok(&fleet, &token, "work_rank", rank.clone());
    assert_eq!(delegated_ok(&fleet, &token, "work_rank", rank), first);
    // The same receiving allowlist admits only exact job_cancel, not raw
    // native cancellation or retry. No operator fallback on denied peers.
    let job=json!({"id":"gamma","incarnation":"gamma","task_id":"alpha","status":"failed","command":["false"],"cwd":home,"action":"test","heavy":false,"heavy_declared":false,"timeout_secs":60,"ledger":root.join("ledger/resources.sqlite"),"created_ms":0,"finished_ms":1,"supervisor":null,"payload":null,"cancel_requested":false,"exit_code":1,"reason":null,"retry_of":null});
    db.execute("INSERT INTO jobs(id,record) VALUES('gamma',?1)",[job.to_string()]).unwrap();
    let cancel=json!({"ship":"relay","reference":{"origin":reference["origin"],"ship":"relay","task_id":"alpha","id":"gamma","incarnation":"gamma"},"idempotency_key":"cancel-linked"});
    for (setting,reason) in [("scope","insufficient_scope"),("ships","ship_not_allowed"),("unsupported","unsupported"),("v6a","unsupported"),("read-gateway","unsupported")] {
        fs::write(&mode,setting).unwrap();
        credential_request(&fleet,&token,"job_cancel",cancel.clone(),reason);
        assert_eq!(db.query_row("SELECT count(*) FROM door_work_results WHERE json_extract(result,'$.operation')='job_cancel'",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    }
    fs::write(&mode,"live").unwrap();
    let stopped=delegated_ok(&fleet,&token,"job_cancel",cancel.clone());
    assert_eq!(stopped["outcome"],"stopped");
    assert_eq!(delegated_ok(&fleet,&token,"job_cancel",cancel),stopped);
    // Before-admission revocation makes no receiving call.
    let conn = rusqlite::Connection::open(&registry).unwrap();
    let id = grant["clients"][0]["id"].as_str().unwrap();
    conn.execute("UPDATE door_clients SET revoked_ms=0 WHERE id=?1", [id])
        .unwrap();
    let before = fs::read_to_string(&calls).unwrap();
    let mut next = json!({"ship":"relay","reference":reference,"rank":2,"idempotency_key":"next"});
    credential_request(&fleet, &token, "work_rank", next.clone(), "revoked_grant");
    assert_eq!(fs::read_to_string(&calls).unwrap(), before);
    conn.execute("UPDATE door_clients SET revoked_ms=NULL WHERE id=?1", [id])
        .unwrap();
    // Fresh ref is obtained through the unmodified real transport.
    fs::write(
        &ssh,
        "#!/bin/sh\nfor argument do remote=$argument; done\nexec /bin/sh -c \"$remote\"\n",
    )
    .unwrap();
    next["reference"] = work_reference(&fleet, &token, "relay", "alpha");
    // Revoke after receiver commit but before host release. The side effect is
    // admitted and durable; the host must discard its acknowledgment.
    fs::write(&ssh,format!(r#"#!/usr/bin/env python3
import json,subprocess,sys,sqlite3
r=json.loads(sys.stdin.readline())
p=subprocess.run(['/bin/sh','-c',sys.argv[-1]],input=json.dumps(r)+'\n',text=True,capture_output=True)
c=sqlite3.connect({registry:?});c.execute('UPDATE door_clients SET revoked_ms=0 WHERE id=?',({id:?},));c.commit()
print(p.stdout,end='')
"#,registry=registry.to_str().unwrap())).unwrap();
    credential_request(&fleet, &token, "work_rank", next, "outcome_unknown");
    let event: (String,String)=conn.query_row("SELECT event,reason FROM door_client_request_audit WHERE client=?1 ORDER BY seq DESC LIMIT 1",[id],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!(event,("request.unknown".into(),"outcome_unknown".into()));
    assert_eq!(
        db.query_row("SELECT rank FROM tasks WHERE id='alpha'", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    stop(supervisor);
}
