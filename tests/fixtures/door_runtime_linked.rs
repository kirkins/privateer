// Real one-way local transport, supervised v2 door and answering gateways.
#[test]
fn runtime_linked_gateway_reuse_timeout_carry_restart_and_grant_boundaries() {
    let fleet = Fleet::with_ships(&[("survey", true)]);
    let ssh = fleet.root.join("fake-ssh");
    let transport = "#!/bin/sh\nfor argument do remote=$argument; done\nexec /bin/sh -c \"$remote\"\n";
    fs::write(&ssh,transport).unwrap();
    fs::set_permissions(&ssh,fs::Permissions::from_mode(0o755)).unwrap();
    let remote = |machine: &str, ship: &str, args: &[&str]| {
        let root = fleet.root.join(machine);
        let home = root.join(ship);
        fs::create_dir_all(&home).unwrap();
        let output = fleet.command(&home).env("HOME",&root)
            .env("PRIVATEER_FLEET",root.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME",root.join("ledger"))
            .args(args).output().unwrap();
        assert!(output.status.success(),"{args:?}: {}",String::from_utf8_lossy(&output.stderr));
    };
    remote("beta","relay", &["init","--ship","relay","--harness","codex"]);
    remote("gamma","lookout", &["init","--ship","lookout","--harness","codex"]);
    fleet.ok(&["machine","add","beta","fixture","--executable",BIN,"--remote-fleet",fleet.root.join("beta/fleet.sqlite").to_str().unwrap(),"--as","alpha"]);
    remote("gamma","lookout", &["machine","add","alpha","fixture","--executable",BIN,"--remote-fleet",fleet.root.join("state/fleet.sqlite").to_str().unwrap(),"--as","gamma"]);
    let peer = rusqlite::Connection::open(fleet.root.join("beta/relay/.privateer/privateer.sqlite")).unwrap();
    // Each expansion is bounded at twenty; the total is twenty-one. Prose and
    // raw worker/route material never enter the DTO, including with mail:read.
    for n in 0..21 {
        peer.execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES(?1,'relay','relay','title-canary','open',0)",[format!("relay-task-{n}")]).unwrap();
        peer.execute("INSERT INTO jobs(id,record) VALUES(?1,?2)",rusqlite::params![format!("relay-job-{n}"),json!({"id":format!("relay-job-{n}"),"task_id":format!("relay-task-{n}"),"status":"queued","created_ms":n,"command":["argv-canary"],"cwd":"cwd-canary","reason":"secret-canary"}).to_string()]).unwrap();
    }
    peer.execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('hidden-task','relay','lookout','hidden-canary','open',0)",[]).unwrap();
    peer.execute("INSERT INTO jobs(id,record) VALUES('hidden-job',?1)",[json!({"id":"hidden-job","task_id":"hidden-task","status":"running"}).to_string()]).unwrap();
    let peer_registry = rusqlite::Connection::open(fleet.root.join("beta/fleet.sqlite")).unwrap();
    peer_registry.execute("INSERT INTO machines(name,role,peer,added_ms) VALUES('unrelated','collected','{}',0)",[]).unwrap();
    peer_registry.execute("INSERT INTO machine_ships(machine,name,scope) VALUES('unrelated','hidden-ship','hidden-canary')",[]).unwrap();
    delegated_adapter(&fleet);
    let (grant,token) = delegated_issue(&fleet,"relay","fleet:read,mail:read",Some("relay"));
    let (_,all) = delegated_issue(&fleet,"survey","fleet:read",None);
    let (_,local_only) = delegated_issue(&fleet,"lookout","fleet:read",Some("survey"));
    let registry_path = fleet.root.join("state/fleet.sqlite");
    let registry = rusqlite::Connection::open(&registry_path).unwrap();
    let mode = fleet.root.join("runtime-mode");
    let calls = fleet.root.join("runtime-calls");
    fs::write(&mode,"live").unwrap();
    // The wrapper only disturbs real gateway requests/replies. It rejects any
    // attempt at operator fallback or recursive traversal by this observation.
    fs::write(&ssh,format!(r#"#!/usr/bin/env python3
import json, subprocess, sys, pathlib, time, sqlite3
r=json.loads(sys.stdin.readline())
with open({calls:?},'a') as f: f.write(r['command']+'\n')
if r['command'] != 'fleet.runtime.v1': sys.exit(73)
mode=pathlib.Path({mode:?}).read_text()
if mode == 'down': sys.exit(73)
if mode == 'timeout': time.sleep(20); sys.exit(73)
if mode == 'unsupported':
 print(json.dumps({{'id':r['id'],'ok':False,'error':'unknown gateway command fleet.runtime.v1'}})); sys.exit()
if mode == 'receiver-denied': r['args']['context']['ships']=[]
if mode == 'receiver-scope': r['args']['context']['scopes']=[]
p=subprocess.run(['/bin/sh','-c',sys.argv[-1]],input=json.dumps(r)+'\n',text=True,capture_output=True)
if p.returncode: sys.exit(p.returncode)
v=json.loads(p.stdout)
if mode == 'skew' and v['ok']: v['result']['observed_at']=8000000000000
if mode == 'release':
 c=sqlite3.connect({registry:?}); c.execute("UPDATE door_clients SET ships='[\"survey\"]' WHERE id=?",({grant:?},)); c.commit()
print(json.dumps(v))
"#,calls=calls.to_str().unwrap(),mode=mode.to_str().unwrap(),registry=registry_path.to_str().unwrap(),grant=grant["clients"][0]["id"].as_str().unwrap())).unwrap();
    let supervisor = fleet.supervise();
    fleet.wait("runtime linked hello",|e|e.first().cloned());
    let args = json!({"schema":"runtime.v1","ship":"relay"});
    let live = delegated_ok(&fleet,&token,"status",args.clone());
    assert_eq!(live["ships"][0]["runtime"]["mode"],"remote_live","{live}");
    assert_eq!(live["ships"][0]["runtime"]["facts"]["jobs_total"],21);
    assert_eq!(live["ships"][0]["runtime"]["facts"]["jobs"].as_array().unwrap().len(),20);
    assert_eq!(live["observed_totals"]["nonterminal_jobs"]["value"],21);
    assert_eq!(live["observed_totals"]["job_states"]["value"]["queued"],20);
    assert_eq!(live["observed_totals"]["job_states"]["complete"],false);
    assert_eq!(live["ships"][0]["recorded_running_jobs"],Value::Null,"truncated states cannot establish a running total");
    assert_eq!(live["ships"][0]["working"],"unknown");
    for secret in ["argv-canary","cwd-canary","secret-canary","hidden-canary","hidden-job","lookout","unrelated","hidden-ship","survey"] { assert!(!live.to_string().contains(secret),"{secret}: {live}"); }
    let count = || fs::read_to_string(&calls).unwrap_or_default().lines().count();
    let before = count();
    let reused = delegated_ok(&fleet,&token,"ships",args.clone());
    assert_eq!(reused["ships"][0]["runtime"]["mode"],"reused");
    assert_eq!(count(),before);
    assert_eq!(reused["ships"][0]["runtime"]["received_at"],live["ships"][0]["runtime"]["received_at"]);
    let restricted = delegated_ok(&fleet,&local_only,"status",json!({"schema":"runtime.v1"}));
    assert_eq!(count(),before,"hidden machine never queried");
    assert!(!restricted.to_string().contains("beta"));
    // Callable vs reverse-only: no gamma call before or after carrying.
    let reverse = delegated_ok(&fleet,&all,"status",json!({"schema":"runtime.v1","ship":"lookout"}));
    assert_eq!(count(),before);
    assert_eq!(reverse["ships"][0]["working"],"unknown");
    assert!(reverse["coverage"]["sources"].as_array().unwrap().iter().any(|s|s["reason"] == "reverse_only"));
    // Timeout preserves last successful contact without renewing current totals.
    std::thread::sleep(Duration::from_millis(5100));
    fs::write(&mode,"timeout").unwrap();
    let timeout = delegated_ok(&fleet,&token,"status",args.clone());
    assert_eq!(timeout["ships"][0]["runtime"]["mode"],"cache");
    assert_eq!(timeout["ships"][0]["runtime"]["current"],false);
    assert_eq!(timeout["ships"][0]["runtime"]["received_at"],live["ships"][0]["runtime"]["received_at"]);
    assert_eq!(timeout["observed_totals"]["nonterminal_jobs"]["value"],0);
    assert_eq!(timeout["observed_totals"]["nonterminal_jobs"]["complete"],false);
    fs::write(&mode,"skew").unwrap();
    let skew = delegated_ok(&fleet,&token,"status",args.clone());
    assert_eq!(skew["ships"][0]["runtime"]["observed_at"],8000000000000_i64);
    assert_eq!(skew["ships"][0]["runtime"]["age_ms"],0,"receipt clock, not remote clock");
    let (_,other) = delegated_issue(&fleet,"relay-other","fleet:read",Some("relay"));
    fs::write(&mode,"unsupported").unwrap();
    let old = delegated_ok(&fleet,&other,"status",args.clone());
    assert!(old["coverage"]["sources"].as_array().unwrap().iter().any(|s|s["state"] == "unsupported"));
    fs::write(&mode,"receiver-denied").unwrap();
    let denied = delegated_ok(&fleet,&other,"status",args.clone());
    assert!(denied["ships"][0].get("runtime").is_none_or(|r|r["facts"]["jobs_total"].is_null()));
    let (_,scope_test) = delegated_issue(&fleet,"relay-scope","fleet:read",Some("relay"));
    fs::write(&mode,"receiver-scope").unwrap();
    let denied = delegated_ok(&fleet,&scope_test,"status",args.clone());
    assert!(denied["ships"][0].get("runtime").is_none());
    // A host release restriction discards an admitted response. Use a different
    // selector cache partition so this request must cross the gateway.
    std::thread::sleep(Duration::from_millis(5100));
    fs::write(&mode,"release").unwrap();
    credential_request(&fleet,&token,"status",args.clone(),"insufficient_scope");
    let before = count();
    credential_request(&fleet,&token,"status",args.clone(),"ship_not_allowed");
    assert_eq!(count(),before,"changed grant denies before remote admission");
    registry.execute("UPDATE door_clients SET ships='[\"relay\"]' WHERE id=?1",[grant["clients"][0]["id"].as_str().unwrap()]).unwrap();
    let strict_wrapper = fs::read_to_string(&ssh).unwrap();
    fs::write(&ssh,transport).unwrap();
    remote("gamma","lookout", &["machine","collect"]);
    // Populate the existing operator cache too; on restart it must be filtered
    // and dated, never renewed by a scoped read or treated as current.
    fleet.ok(&["fleet.snapshot","--json","{}"]);
    fs::write(&ssh,&strict_wrapper).unwrap();
    let carried = delegated_ok(&fleet,&all,"status",json!({"schema":"runtime.v1","ship":"lookout"}));
    assert_eq!(carried["ships"][0]["runtime"]["mode"],"carried","{carried}");
    assert_eq!(carried["ships"][0]["runtime"]["current"],false);
    assert_eq!(carried["ships"][0]["runtime"]["age_ms"],Value::Null,"persistent receipt has no proven clock continuity");
    assert_eq!(carried["observed_totals"]["nonterminal_jobs"]["complete"],false);
    stop(supervisor);
    fs::write(fleet.root.join("requests.jsonl"),"").unwrap();
    fs::write(&fleet.log,"").unwrap();
    fs::write(&mode,"down").unwrap();
    let supervisor = fleet.supervise();
    fleet.wait("restarted runtime hello",|e|e.first().cloned());
    let restarted = delegated_ok(&fleet,&token,"status",args);
    assert_eq!(restarted["ships"][0]["runtime"]["mode"],"cache","{restarted}");
    assert_eq!(restarted["ships"][0]["runtime"]["current"],false);
    assert_eq!(restarted["ships"][0]["runtime"]["age_ms"],Value::Null);
    assert_eq!(restarted["observed_totals"]["nonterminal_jobs"]["value"],0);
    for hidden in ["argv-canary","cwd-canary","hidden-job","hidden-canary","unrelated","lookout"] { assert!(!restarted.to_string().contains(hidden)); }
    assert!(fs::read_to_string(&calls).unwrap().lines().all(|c|c == "fleet.runtime.v1"),"no operator fallback/recursive query");
    stop(supervisor);
}
