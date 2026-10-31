// Configure-only resource policy crosses real v2 admission and real gateways.
#[test]
fn resource_floor_tightening_keeps_a_real_heavy_supervisor_queued() {
    let fleet = Fleet::with_ships(&[("survey", true)]);
    let db = rusqlite::Connection::open(fleet.home("survey").join("privateer.sqlite")).unwrap();
    db.execute("UPDATE charters SET data=json_set(data,'$.actions',json('[\"test\",\"recovery\"]'),'$.min_free_bytes',0) WHERE id='survey'", []).unwrap();
    db.execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('alpha','survey','survey','alpha','open',0)", []).unwrap();
    fleet.call("survey", "resource.configure", json!({"min_free_disk_bytes":1,"min_available_memory_bytes":0,"max_load_per_cpu":1024}));
    let host = fleet.call("survey", "resource.status", json!({}))["host"].clone();
    let total = host["disk_total_bytes"].as_u64().unwrap();
    delegated_adapter(&fleet);
    let (_, admin) = delegated_issue(&fleet, "alpha", "fleet:admin", None);
    let supervisor = fleet.supervise();
    fleet.wait("resource queue hello", |e| e.first().cloned());
    let queued = fleet.call("survey", "job.start", json!({"task_id":"alpha","id":"beta","command":["true"],"action":"test","heavy":true,"estimate":{"disk_bytes":total},"wait_for_capacity":true}));
    assert_eq!(queued["status"], "queued");
    let args = json!({"machine":"local","ship":"survey","changes":{"min_free_disk_bytes":total-1},"idempotency_key":"floor"});
    assert_eq!(delegated_ok(&fleet, &admin, "resource_configure", args.clone())["applied"], true);
    // Observe a new native queue evaluation, not an arbitrary delay followed
    // by reading the unchanged initial queued row.
    let checked = fleet.wait("native supervisor re-evaluated tightened floor", |_| {
        let job = fleet.call("survey", "job.status", json!({"id":"beta"}));
        assert_eq!(job["status"], "queued", "tightening must not hard-fail queued work: {job}");
        (job["reason"] != queued["reason"]).then_some(job)
    });
    assert!(checked["reason"].as_str().unwrap().contains("disk admission refused"));
    for (key, total) in [("min_free_disk_bytes", total), ("min_available_memory_bytes", host["memory_total_bytes"].as_u64().unwrap())] {
        let mut denied = args.clone();
        denied["changes"] = json!({key:total});
        denied["idempotency_key"] = json!(key);
        credential_request(&fleet, &admin, "resource_configure", denied, "resource_change_refused_exceeds_capacity");
    }
    fleet.call("survey", "job.cancel", json!({"id":"beta"}));
    stop(supervisor);
}
#[test]
fn resource_configure_supervised_scopes_atomicity_replay_and_replaced_ledger() {
    let fleet=Fleet::with_ships(&[("survey",true)]);
    fleet.call("survey","resource.status",json!({})); // native schema installation
    delegated_adapter(&fleet);
    let (_,admin)=delegated_issue(&fleet,"alpha","fleet:admin",None);
    let (_,named)=delegated_issue(&fleet,"beta","fleet:admin",Some("survey"));
    let (_,read)=delegated_issue(&fleet,"gamma","fleet:read",None);
    let supervisor=fleet.supervise();
    fleet.wait("resource hello",|e|e.first().cloned());
    let args=json!({"machine":"local","ship":"survey","changes":{"max_heavy_jobs":2},"idempotency_key":"alpha"});
    credential_request(&fleet,&read,"resource_configure",args.clone(),"insufficient_scope");
    credential_request(&fleet,&named,"resource_configure",args.clone(),"ship_not_allowed");
    for command in ["resource.configure","resource.recover","resource.recovery.apply","fleet.resource-action.v1","dispatch","rpc"] {
        credential_request(&fleet,&admin,command,args.clone(),"unknown_command");
    }
    for field in ["path","force","estimate","argv","_binding","target"] {
        let mut bad=args.clone();bad[field]=json!("secret-canary");
        credential_request(&fleet,&admin,"resource_configure",bad,"unsupported_arguments");
    }
    let home=fleet.home("survey").join("privateer.sqlite");let home_before=fs::read(&home).unwrap();
    let first=delegated_ok(&fleet,&admin,"resource_configure",args.clone());
    assert_eq!(first["schema"],"resource.action.v1");assert_eq!(first["changed"],true);
    assert_eq!(delegated_ok(&fleet,&admin,"resource_configure",args.clone()),first);
    let bad=json!({"machine":"local","changes":{"max_heavy_jobs":1,"min_free_disk_bytes":0},"idempotency_key":"bad"});
    credential_request(&fleet,&admin,"resource_configure",bad,"resource_change_refused_relaxes");
    let current=fleet.call("survey","resource.status",json!({}));
    assert!(current.to_string().contains("max_heavy_jobs"));
    let ledger=rusqlite::Connection::open(fleet.root.join("ledger/resources.sqlite")).unwrap();
    let raw:String=ledger.query_row("SELECT value FROM settings WHERE key='admission'",[],|r|r.get(0)).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&raw).unwrap()["max_heavy_jobs"],2);
    let no_op=delegated_ok(&fleet,&admin,"resource_configure",json!({"machine":"local","changes":{"max_heavy_jobs":2},"idempotency_key":"noop"}));
    assert_eq!(no_op["changed"],false);assert_eq!(no_op["applied"],true);
    assert_eq!(fs::read(&home).unwrap(),home_before,"no home initialization or writes");
    drop(ledger);stop(supervisor);
    let supervisor=fleet.supervise();
    assert_eq!(delegated_ok(&fleet,&admin,"resource_configure",args.clone()),first);
    // Retire the entire old ledger, then initialize a genuinely new database
    // through the native path. No old key may apply to the replacement.
    fs::rename(fleet.root.join("ledger"),fleet.root.join("old-ledger")).unwrap();
    fleet.call("survey","resource.status",json!({}));
    credential_request(&fleet,&admin,"resource_configure",args,"unsupported_arguments");
    let replacement=rusqlite::Connection::open(fleet.root.join("ledger/resources.sqlite")).unwrap();
    assert!(replacement.query_row::<String,_,_>("SELECT value FROM settings WHERE key='admission'",[],|r|r.get(0)).optional().unwrap().is_none());
    stop(supervisor);
}

#[test]
fn resource_configure_real_gateway_receiver_and_incarnation_release_fences() {
    let fleet=Fleet::with_ships(&[("survey",true)]);
    let ssh=fleet.root.join("fake-ssh");
    fs::write(&ssh,"#!/bin/sh\nfor argument do remote=$argument; done\nexec /bin/sh -c \"$remote\"\n").unwrap();
    fs::set_permissions(&ssh,fs::Permissions::from_mode(0o755)).unwrap();
    let root=fleet.root.join("beta");let folder=root.join("relay");fs::create_dir_all(&folder).unwrap();
    let remote=fleet.command(&folder).env("HOME",&root).env("PRIVATEER_FLEET",root.join("fleet.sqlite")).env("PRIVATEER_RESOURCE_HOME",root.join("ledger")).args(["init","--ship","relay","--harness","codex"]).output().unwrap();
    assert!(remote.status.success(),"{}",String::from_utf8_lossy(&remote.stderr));
    let remote_home=folder.join(".privateer");
    fs::write(remote_home.join("config.json"),json!({"resource_home":root.join("ledger")}).to_string()).unwrap();
    let remote=fleet.command(&folder).env("HOME",&root).env("PRIVATEER_FLEET",root.join("fleet.sqlite")).args(["--home",remote_home.to_str().unwrap(),"resource.status","--json","{}"]).output().unwrap();
    assert!(remote.status.success(),"{}",String::from_utf8_lossy(&remote.stderr));
    fleet.ok(&["machine","add","beta","fixture","--executable",BIN,"--remote-fleet",root.join("fleet.sqlite").to_str().unwrap(),"--as","alpha"]);
    delegated_adapter(&fleet);
    let (grant,token)=delegated_issue(&fleet,"alpha","fleet:admin",None);
    let supervisor=fleet.supervise();fleet.wait("resource linked hello",|e|e.first().cloned());
    let mode=fleet.root.join("resource-mode");let calls=fleet.root.join("resource-calls");fs::write(&mode,"live").unwrap();
    let registry_path=fleet.root.join("state/fleet.sqlite");
    fs::write(&ssh,format!(r#"#!/usr/bin/env python3
import json,pathlib,subprocess,sys,sqlite3
r=json.loads(sys.stdin.readline())
with open({calls:?},'a') as f:f.write(json.dumps(r)+'\n')
mode=pathlib.Path({mode:?}).read_text()
if r['command']!='fleet.resource-action.v1':sys.exit(73)
if mode=='unsupported':
 print(json.dumps({{'id':r['id'],'ok':False,'error':'unknown gateway command fleet.resource-action.v1'}}));sys.exit()
if mode=='scope':r['args']['context']['scopes']=[]
if mode=='named':r['args']['context']['ships']=['relay']
if mode=='read':r['command']='fleet.inspection.v1'
p=subprocess.run(['/bin/sh','-c',sys.argv[-1]],input=json.dumps(r)+'\n',text=True,capture_output=True)
if p.returncode:sys.stderr.write(p.stderr);sys.exit(p.returncode)
if r['args']['operation']=='resource_configure' and mode in ('replace','release'):
 c=sqlite3.connect({registry:?})
 if mode=='replace':c.execute("UPDATE machines SET incarnation='gamma' WHERE name='beta'")
 else:c.execute('UPDATE door_clients SET revoked_ms=0 WHERE id=?',({grant:?},))
 c.commit()
print(p.stdout,end='')
"#,calls=calls.to_str().unwrap(),mode=mode.to_str().unwrap(),registry=registry_path.to_str().unwrap(),grant=grant["clients"][0]["id"].as_str().unwrap())).unwrap();
    let mut a=json!({"machine":"beta","ship":"relay","changes":{"max_heavy_jobs":4},"idempotency_key":"alpha"});
    for (setting,reason) in [("scope","insufficient_scope"),("named","ship_not_allowed"),("unsupported","unsupported"),("read","insufficient_scope")] {
        fs::write(&mode,setting).unwrap();credential_request(&fleet,&token,"resource_configure",a.clone(),reason);
    }
    fs::write(&mode,"live").unwrap();
    for (changes, reason) in [(json!({"min_free_disk_bytes":0}), "resource_change_refused_relaxes"), (json!({"min_available_memory_bytes":u64::MAX}), "resource_change_refused_exceeds_capacity")] {
        let mut denied = a.clone(); denied["changes"] = changes; denied["idempotency_key"] = json!(reason);
        credential_request(&fleet,&token,"resource_configure",denied,reason);
    }
    let first=delegated_ok(&fleet,&token,"resource_configure",a.clone());assert_eq!(delegated_ok(&fleet,&token,"resource_configure",a.clone()),first);
    let registry=rusqlite::Connection::open(&registry_path).unwrap();
    registry.execute("UPDATE machines SET incarnation='beta' WHERE name='beta'",[]).unwrap();
    credential_request(&fleet,&token,"resource_configure",a.clone(),"unsupported_arguments");
    a["idempotency_key"]=json!("held");a["changes"]["max_heavy_jobs"]=json!(2);fs::write(&mode,"replace").unwrap();
    credential_request(&fleet,&token,"resource_configure",a.clone(),"outcome_unknown");
    fs::write(&mode,"live").unwrap();credential_request(&fleet,&token,"resource_configure",a,"unsupported_arguments");
    // Receiver committed, then origin grant revoked: acknowledgment discarded,
    // never a definite denial/no-effect claim.
    fs::write(&mode,"release").unwrap();
    credential_request(&fleet,&token,"resource_configure",json!({"machine":"beta","ship":"relay","changes":{"max_heavy_jobs":1},"idempotency_key":"release"}),"outcome_unknown");
    let ledger=rusqlite::Connection::open(root.join("ledger/resources.sqlite")).unwrap();
    let raw:String=ledger.query_row("SELECT value FROM settings WHERE key='admission'",[],|r|r.get(0)).unwrap();assert_eq!(serde_json::from_str::<Value>(&raw).unwrap()["max_heavy_jobs"],1);
    let calls:Vec<Value>=fs::read_to_string(&calls).unwrap().lines().map(|l|serde_json::from_str(l).unwrap()).collect();assert!(calls.iter().all(|r|r["command"]=="fleet.resource-action.v1"));
    stop(supervisor);
}
