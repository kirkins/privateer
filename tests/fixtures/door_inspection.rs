// Real supervisor and one-way gateway subprocesses; no mock operator dispatch.
#[test]
fn inspection_linked_pages_detail_native_parity_and_release_restriction() {
    let fleet=Fleet::with_ships(&[("survey",true)]);
    let ssh=fleet.root.join("fake-ssh");
    fs::write(&ssh,"#!/bin/sh\nfor argument do remote=$argument; done\nexec /bin/sh -c \"$remote\"\n").unwrap();
    fs::set_permissions(&ssh,fs::Permissions::from_mode(0o755)).unwrap();
    let root=fleet.root.join("beta");let home=root.join("relay");fs::create_dir_all(&home).unwrap();
    let peer_call=|args:&[&str]| {
        let out=fleet.command(&home).env("HOME",&root).env("PRIVATEER_FLEET",root.join("fleet.sqlite")).env("PRIVATEER_RESOURCE_HOME",root.join("ledger")).args(args).output().unwrap();
        assert!(out.status.success(),"{args:?}: {}",String::from_utf8_lossy(&out.stderr));String::from_utf8(out.stdout).unwrap()
    };
    peer_call(&["init","--ship","relay","--harness","codex"]);
    fleet.ok(&["machine","add","beta","fixture","--executable",BIN,"--remote-fleet",root.join("fleet.sqlite").to_str().unwrap(),"--as","alpha"]);
    let peer=rusqlite::Connection::open(home.join(".privateer/privateer.sqlite")).unwrap();
    peer.execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('relay-task','relay','relay','title-canary','open',0)",[]).unwrap();
    for n in 0..3 {
        peer.execute("INSERT INTO jobs(id,record) VALUES(?1,?2)",rusqlite::params![format!("relay-{n}"),json!({"id":format!("relay-{n}"),"incarnation":"alpha","task_id":"relay-task","status":if n==0 {"resumed"} else {"running"},"created_ms":100,"finished_ms":if n==0 {Some(200)} else {None},"command":["argv-canary"],"cwd":"/absolute/cwd-canary","ledger":"/absolute/ledger-canary","action":"command","heavy":false,"timeout_secs":60,"cancel_requested":false}).to_string()]).unwrap();
    }
    peer.execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('hidden-task','relay','lookout','hidden-title','open',0)",[]).unwrap();
    peer.execute("INSERT INTO jobs(id,record) VALUES('hidden-job',?1)",[json!({"id":"hidden-job","task_id":"hidden-task","incarnation":"alpha","status":"running"}).to_string()]).unwrap();
    fs::write(home.join(".privateer/config.json"),json!({"resource_home":root.join("ledger")}).to_string()).unwrap();
    peer_call(&["resource.configure","--json",r#"{"max_heavy_jobs":3}"#]);
    let native_capacity:Value=serde_json::from_str(&peer_call(&["resource.status","--json","{}"]) ).unwrap();
    peer.execute("INSERT INTO mail(id,sender,recipient,kind,task_id,body,payload,urgency,created_ms,state) VALUES('relay-ask','relay','captain','request','relay-task','Question token=secret-canary','{}','high',100,'open')",[]).unwrap();
    let native_needs:Value=serde_json::from_str(&peer_call(&["runtime.needs","--json","{}"]) ).unwrap();
    let native:Value=serde_json::from_str(&peer_call(&["job.status","--json",r#"{"id":"relay-0"}"#])).unwrap();
    delegated_adapter(&fleet);
    let (grant,token)=delegated_issue(&fleet,"relay","fleet:read",Some("relay"));
    let (_,local)=delegated_issue(&fleet,"survey","fleet:read",Some("survey"));
    let (_,allships)=delegated_issue(&fleet,"lookout","fleet:read",None);
    let mode=fleet.root.join("inspection-mode");fs::write(&mode,"live").unwrap();
    let calls=fleet.root.join("inspection-calls");let registry=fleet.root.join("state/fleet.sqlite");
    fs::write(&ssh,format!(r#"#!/usr/bin/env python3
import json, pathlib, subprocess, sys, sqlite3
r=json.loads(sys.stdin.readline())
with open({calls:?},'a') as f: f.write(r['command']+'\n')
if r['command']!='fleet.inspection.v1': sys.exit(73)
mode=pathlib.Path({mode:?}).read_text()
if mode=='unsupported':
 print(json.dumps({{'id':r['id'],'ok':False,'error':'unknown gateway command fleet.inspection.v1'}})); sys.exit()
if mode=='scope': r['args']['context']['scopes']=[]
if mode=='ships': r['args']['context']['ships']=[]
p=subprocess.run(['/bin/sh','-c',sys.argv[-1]],input=json.dumps(r)+'\n',text=True,capture_output=True)
if p.returncode: sys.exit(p.returncode)
if mode=='release':
 c=sqlite3.connect({registry:?}); c.execute("UPDATE door_clients SET ships='[\"survey\"]' WHERE id=?",({grant:?},)); c.commit()
print(p.stdout,end='')
"#,calls=calls.to_str().unwrap(),mode=mode.to_str().unwrap(),registry=registry.to_str().unwrap(),grant=grant["clients"][0]["id"].as_str().unwrap())).unwrap();
    let supervisor=fleet.supervise();fleet.wait("inspection hello",|e|e.first().cloned());
    let before=fs::read(home.join(".privateer/privateer.sqlite")).unwrap();
    let args=json!({"ship":"relay","limit":1});
    let first=delegated_ok(&fleet,&token,"ship_jobs",args.clone());assert_eq!(first["jobs"].as_array().unwrap().len(),1);assert_eq!(first["counts"]["completed"],1);assert_eq!(first["counts"]["running"],2);assert_eq!(first["coverage"]["mode"],"remote_live");
    let detail=delegated_ok(&fleet,&token,"job_inspect",json!({"ship":"relay","reference":first["jobs"][0]["reference"]}));assert_eq!(detail["job"]["state"],native["status"]);assert_eq!(detail["job"]["reference"]["task_id"],native["task_id"]);assert_eq!(detail["job"]["exit"],"resumed");
    let second=delegated_ok(&fleet,&token,"ship_jobs",json!({"ship":"relay","limit":1,"continuation":first["continuation"]}));assert_ne!(first["jobs"][0]["reference"],second["jobs"][0]["reference"]);
    let work=delegated_ok(&fleet,&token,"ship_work",json!({"ship":"relay"}));assert!(work["counts"]["backlog"].as_u64().unwrap()>=1);assert_eq!(work["coverage"]["facet"],"work");
    let capacity_args=json!({"machine":"beta","ship":"relay"});
    let capacity=delegated_ok(&fleet,&token,"machine_capacity",capacity_args.clone());
    assert_eq!(capacity["capacity"]["shared_host"],"withheld");assert!(capacity["capacity"]["policy"].is_null());
    let shared=delegated_ok(&fleet,&allships,"machine_capacity",capacity_args.clone());
    assert_eq!(shared["capacity"]["policy"]["resolved_host_heavy_cap"],native_capacity["settings"]["max_heavy_jobs"]);
    assert_eq!(shared["capacity"]["occupied_heavy_slots"],native_capacity["heavy_reserved"]);
    assert_eq!(shared["coverage"]["mode"],"remote_live");
    let needs=delegated_ok(&fleet,&token,"runtime_needs",json!({"ship":"relay"}));
    assert_eq!(needs["needs"][0]["source_id"],native_needs["items"][0]["source_id"]);assert!(needs["needs"][0]["question"].is_null());assert_eq!(needs["coverage"]["complete"],false);
    for value in [&first,&detail,&second,&work,&capacity,&shared,&needs] {for canary in ["argv-canary","cwd-canary","ledger-canary","hidden-task","hidden-job","hidden-title","lookout"] {assert!(!value.to_string().contains(canary),"{canary}: {value}");}}
    let calls_before=fs::read_to_string(&calls).unwrap();credential_request(&fleet,&local,"ship_jobs",args.clone(),"ship_not_allowed");assert_eq!(fs::read_to_string(&calls).unwrap(),calls_before);
    fs::write(&mode,"scope").unwrap();credential_request(&fleet,&token,"ship_jobs",args.clone(),"insufficient_scope");
    credential_request(&fleet,&token,"machine_capacity",capacity_args.clone(),"insufficient_scope");
    credential_request(&fleet,&token,"runtime_needs",json!({"ship":"relay"}),"insufficient_scope");
    fs::write(&mode,"ships").unwrap();credential_request(&fleet,&token,"ship_jobs",args.clone(),"ship_not_allowed");
    credential_request(&fleet,&token,"machine_capacity",capacity_args,"ship_not_allowed");
    credential_request(&fleet,&token,"runtime_needs",json!({"ship":"relay"}),"ship_not_allowed");
    fs::write(&mode,"unsupported").unwrap();let old=delegated_ok(&fleet,&token,"ship_jobs",args.clone());assert_eq!(old["coverage"]["state"],"unsupported");assert!(old["jobs"].as_array().unwrap().is_empty());
    fs::write(&mode,"release").unwrap();credential_request(&fleet,&token,"ship_jobs",args,"insufficient_scope");
    assert_eq!(fs::read(home.join(".privateer/privateer.sqlite")).unwrap(),before,"no home writes from delegated reads");
    assert!(fs::read_to_string(&calls).unwrap().lines().all(|c|c=="fleet.inspection.v1"));
    stop(supervisor);
}
