// V6a crosses the actual scoped adapter, core admission and linked subprocess.
fn work_reference(fleet: &Fleet, token: &str, ship: &str, task: &str) -> Value {
    let page=delegated_ok(fleet,token,"ship_work",json!({"ship":ship,"limit":50}));
    page["work"].as_array().unwrap().iter().find(|w|w["task_id"]==task).unwrap()["reference"].clone()
}
#[test]
fn guarded_work_actions_real_v2_matrix_native_parity_and_restart_replay() {
    let fleet=Fleet::with_ships(&[("survey",true)]);
    let home=fleet.home("survey");
    let db=rusqlite::Connection::open(home.join("privateer.sqlite")).unwrap();
    db.execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('alpha','survey','survey','alpha','open',0)",[]).unwrap();
    delegated_adapter(&fleet);
    let (_,read)=delegated_issue(&fleet,"alpha","fleet:read",Some("survey"));
    let (_,manage)=delegated_issue(&fleet,"beta","work:manage",Some("survey"));
    let (_,other)=delegated_issue(&fleet,"gamma","work:manage",Some("relay"));
    let supervisor=fleet.supervise();fleet.wait("work hello",|e|e.first().cloned());
    let reference=work_reference(&fleet,&read,"survey","alpha");
    assert!(reference.is_object());
    for (command,extra) in [("work_rank",json!({"rank":1})),("work_enqueue",json!({"brief":"assignment"})),("work_defer",json!({"wait":{"kind":"manual","reason":"stop"}}))] {
        let mut a=json!({"ship":"survey","reference":reference,"idempotency_key":command});
        for (k,v) in extra.as_object().unwrap(){a[k]=v.clone();}
        credential_request(&fleet,&read,command,a.clone(),"insufficient_scope");
        credential_request(&fleet,&other,command,a.clone(),"ship_not_allowed");
        for field in ["argv","command","route","force","payload","holder"] {
            let mut bad=a.clone();bad[field]=json!("canary");credential_request(&fleet,&manage,command,bad,"unsupported_arguments");
        }
        for field in ["owner","operator","delegated_context","grant"] {
            let mut bad=a.clone();bad[field]=json!("forged");credential_request(&fleet,&manage,command,bad,"reserved_identity");
        }
        let mut missing=a;missing.as_object_mut().unwrap().remove("idempotency_key");credential_request(&fleet,&manage,command,missing,"unsupported_arguments");
    }
    // Raw native commands, CLI-style aliases, generic RPCs and every gateway
    // family stay outside the external door allowlist even with work authority.
    for command in ["task.rank","task.enqueue","task.defer","task_rank","task_enqueue","task_defer","work.rank","work.enqueue","work.defer","dispatch","rpc","machine.call","peer.call","fleet.work-action.v1","fleet.inspection.v1","fleet.runtime.v1","door.delegated.v1","door.desk.v1","door.admin.v1","work.poll","work.started","work.settled","work.release","job.start","agent.start","approval.grant","routes.set"] {
        credential_request(&fleet,&manage,command,json!({}),"unknown_command");
    }
    let rank=json!({"ship":"survey","reference":reference,"rank":5,"idempotency_key":"rank"});
    let first=delegated_ok(&fleet,&manage,"work_rank",rank.clone());
    assert_eq!(first["schema"],"work.action.v1");assert_eq!(first["applied"],true);assert_eq!(first["provenance"]["result_id"],first["result_id"]);
    assert!(first.get("backlog").is_none());
    let native=fleet.call("survey","task.backlog",json!({"ship":"survey"}));assert!(native.to_string().contains("alpha"));
    assert_eq!(db.query_row("SELECT rank FROM tasks WHERE id='alpha'",[],|r|r.get::<_,i64>(0)).unwrap(),5);
    assert_eq!(delegated_ok(&fleet,&manage,"work_rank",rank.clone()),first);
    // A supervisor restart cannot change the committed result or reapply rank.
    stop(supervisor);
    let supervisor=fleet.supervise();
    assert_eq!(delegated_ok(&fleet,&manage,"work_rank",rank.clone()),first);
    assert_eq!(db.query_row("SELECT count(*) FROM audit WHERE kind='task.rank'",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    let mut different=rank.clone();different["rank"]=json!(6);credential_request(&fleet,&manage,"work_rank",different,"unsupported_arguments");
    let enqueue=json!({"ship":"survey","reference":work_reference(&fleet,&read,"survey","alpha"),"brief":"prepared","idempotency_key":"enqueue"});
    let enqueued=delegated_ok(&fleet,&manage,"work_enqueue",enqueue.clone());assert_eq!(delegated_ok(&fleet,&manage,"work_enqueue",enqueue),enqueued);
    let defer=json!({"ship":"survey","reference":work_reference(&fleet,&read,"survey","alpha"),"wait":{"kind":"manual","reason":"hold"},"idempotency_key":"defer"});
    let deferred=delegated_ok(&fleet,&manage,"work_defer",defer.clone());assert_eq!(delegated_ok(&fleet,&manage,"work_defer",defer),deferred);
    let schedule:Value=serde_json::from_str(&db.query_row::<String,_,_>("SELECT record FROM task_schedule WHERE task_id='alpha'",[],|r|r.get(0)).unwrap()).unwrap();assert_eq!(schedule["state"],"stopped");
    let audit:String=db.query_row("SELECT payload FROM audit WHERE kind='door.work' ORDER BY seq DESC LIMIT 1",[],|r|r.get(0)).unwrap();assert!(audit.contains("idempotency_key_hash"));assert!(!audit.contains("prepared"));
    stop(supervisor);
}

#[test]
fn guarded_work_linked_receiver_allowlist_revoke_release_and_unsupported_peer() {
    let fleet=Fleet::with_ships(&[("survey",true)]);
    let ssh=fleet.root.join("fake-ssh");fs::write(&ssh,"#!/bin/sh\nfor argument do remote=$argument; done\nexec /bin/sh -c \"$remote\"\n").unwrap();fs::set_permissions(&ssh,fs::Permissions::from_mode(0o755)).unwrap();
    let root=fleet.root.join("beta");let home=root.join("relay");fs::create_dir_all(&home).unwrap();
    let output=fleet.command(&home).env("HOME",&root).env("PRIVATEER_FLEET",root.join("fleet.sqlite")).args(["init","--ship","relay","--harness","codex"]).output().unwrap();assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
    fleet.ok(&["machine","add","beta","fixture","--executable",BIN,"--remote-fleet",root.join("fleet.sqlite").to_str().unwrap(),"--as","alpha"]);
    let db=rusqlite::Connection::open(home.join(".privateer/privateer.sqlite")).unwrap();db.execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('alpha','relay','relay','alpha','open',0)",[]).unwrap();
    delegated_adapter(&fleet);
    let (grant,token)=delegated_issue(&fleet,"alpha","fleet:read,work:manage",Some("relay"));
    let supervisor=fleet.supervise();fleet.wait("linked work hello",|e|e.first().cloned());
    let reference=work_reference(&fleet,&token,"relay","alpha");
    let calls=fleet.root.join("action-calls");let mode=fleet.root.join("action-mode");fs::write(&mode,"live").unwrap();
    let registry=fleet.root.join("state/fleet.sqlite");
    fs::write(&ssh,format!(r#"#!/usr/bin/env python3
import json,pathlib,subprocess,sys,sqlite3
r=json.loads(sys.stdin.readline())
with open({calls:?},'a') as f:f.write(r['command']+'\n')
if r['command']!='fleet.work-action.v1':sys.exit(73)
mode=pathlib.Path({mode:?}).read_text()
if mode=='unsupported':
 print(json.dumps({{'id':r['id'],'ok':False,'error':'unknown gateway command fleet.work-action.v1'}}));sys.exit()
if mode=='scope':r['args']['context']['scopes']=[]
if mode=='ships':r['args']['context']['ships']=[]
if mode=='read-gateway':r['command']='fleet.inspection.v1'
p=subprocess.run(['/bin/sh','-c',sys.argv[-1]],input=json.dumps(r)+'\n',text=True,capture_output=True)
if p.returncode:sys.exit(p.returncode)
if mode=='release':
 c=sqlite3.connect({registry:?});c.execute('UPDATE door_clients SET paused=1 WHERE id=?',({grant:?},));c.commit()
print(p.stdout,end='')
"#,calls=calls.to_str().unwrap(),mode=mode.to_str().unwrap(),registry=registry.to_str().unwrap(),grant=grant["clients"][0]["id"].as_str().unwrap())).unwrap();
    let rank=json!({"ship":"relay","reference":reference,"rank":1,"idempotency_key":"rank"});
    for (setting,reason) in [("scope","insufficient_scope"),("ships","ship_not_allowed"),("unsupported","unsupported"),("read-gateway","insufficient_scope")] {
        fs::write(&mode,setting).unwrap();
        // The read gateway refuses actions too; its fleet scope happens to be
        // present, so that mode is checked separately below.
        if setting=="read-gateway" {continue;}
        credential_request(&fleet,&token,"work_rank",rank.clone(),reason);
        assert!(db.query_row::<Option<i64>,_,_>("SELECT rank FROM tasks WHERE id='alpha'",[],|r|r.get(0)).unwrap().is_none());
    }
    fs::write(&mode,"read-gateway").unwrap();credential_request(&fleet,&token,"work_rank",rank.clone(),"unsupported_arguments");
    fs::write(&mode,"live").unwrap();let first=delegated_ok(&fleet,&token,"work_rank",rank.clone());assert_eq!(delegated_ok(&fleet,&token,"work_rank",rank),first);
    // Before-admission revocation makes no receiving call.
    let conn=rusqlite::Connection::open(&registry).unwrap();let id=grant["clients"][0]["id"].as_str().unwrap();conn.execute("UPDATE door_clients SET paused=1 WHERE id=?1",[id]).unwrap();let before=fs::read_to_string(&calls).unwrap();
    let mut next=json!({"ship":"relay","reference":reference,"rank":2,"idempotency_key":"next"});credential_request(&fleet,&token,"work_rank",next.clone(),"paused_grant");assert_eq!(fs::read_to_string(&calls).unwrap(),before);
    conn.execute("UPDATE door_clients SET paused=0 WHERE id=?1",[id]).unwrap();
    // Fresh ref is obtained through the unmodified real transport.
    fs::write(&ssh,"#!/bin/sh\nfor argument do remote=$argument; done\nexec /bin/sh -c \"$remote\"\n").unwrap();
    next["reference"]=work_reference(&fleet,&token,"relay","alpha");
    // Pause after receiver commit but before host release. The side effect is
    // admitted and durable; the host must discard its acknowledgment.
    fs::write(&ssh,format!(r#"#!/usr/bin/env python3
import json,subprocess,sys,sqlite3
r=json.loads(sys.stdin.readline())
p=subprocess.run(['/bin/sh','-c',sys.argv[-1]],input=json.dumps(r)+'\n',text=True,capture_output=True)
c=sqlite3.connect({registry:?});c.execute('UPDATE door_clients SET paused=1 WHERE id=?',({id:?},));c.commit()
print(p.stdout,end='')
"#,registry=registry.to_str().unwrap())).unwrap();
    credential_request(&fleet,&token,"work_rank",next,"paused_grant");assert_eq!(db.query_row("SELECT rank FROM tasks WHERE id='alpha'",[],|r|r.get::<_,i64>(0)).unwrap(),2);
    stop(supervisor);
}
