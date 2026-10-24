#[test]
fn scoped_remote_reader_subprocess_refs_generations_restart_and_coverage() {
    linked_reader(false);
}

#[test]
fn scoped_remote_reader_reverse_only_is_explicitly_unavailable() {
    linked_reader(true);
}

fn linked_reader(reverse: bool) {
    let fleet = Fleet::new();
    let server = fleet.root.join("reader-peer");
    let harbor = server.join("harbor");
    fs::create_dir_all(&harbor).unwrap();
    let remote = |args: &[&str]| {
        let output = fleet.command(&harbor).env("HOME",&server)
            .env("PRIVATEER_FLEET",server.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME",server.join("ledger"))
            .args(args).output().unwrap();
        assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
    };
    remote(&["init","--ship","harbor","--harness","codex"]);
    let ssh = fleet.root.join("fake-ssh");
    let transport = "#!/bin/sh\nfor argument do remote=$argument; done\nexec /bin/sh -c \"$remote\"\n";
    fs::write(&ssh,transport).unwrap();
    fs::set_permissions(&ssh,fs::Permissions::from_mode(0o755)).unwrap();
    if reverse {
        remote(&["machine","add","adapter-core","test-host","--executable",BIN,"--remote-fleet",fleet.root.join("state/fleet.sqlite").to_str().unwrap(),"--as","peer"]);
    } else {
        fleet.ok(&["machine","add","peer","test-host","--executable",BIN,"--remote-fleet",server.join("fleet.sqlite").to_str().unwrap(),"--as","adapter-core"]);
    }
    for (id, correlation) in [("root",None),("reply",Some("root")),("tail",None)] {
        let mut mail = json!({"id":id,"from":"harbor","to":"captain","kind":"notice","body":id});
        if let Some(correlation) = correlation { mail["correlation"]=json!(correlation); }
        remote(&["mail.send","--json",&mail.to_string()]);
    }
    delegated_adapter(&fleet);
    let (_, first) = delegated_issue(&fleet,"reader","mail:read",Some("harbor"));
    let (_, other) = delegated_issue(&fleet,"other","mail:read",None);
    let (_, restricted) = delegated_issue(&fleet,"local-only","mail:read",Some("ops"));
    let supervisor = fleet.supervise();
    fleet.wait("linked reader hello",|e|e.first().cloned());
    credential_request(&fleet,&restricted,"inbox",json!({"ship":"harbor"}),"execution_failed");
    let page = delegated_ok(&fleet,&first,"inbox",json!({"ship":"harbor","limit":1}));
    if reverse {
        assert_eq!(page["coverage"]["complete"],false);
        assert_eq!(page["coverage"]["machines"][0]["state"],"unavailable");
        assert_eq!(page["coverage"]["machines"][0]["retry"],"reverse_only");
        assert!(page["items"].as_array().unwrap().is_empty());
        stop(supervisor);
        return;
    }
    assert_eq!(page["coverage"]["machines"][0]["machine"],"peer");
    assert_eq!(page["coverage"]["machines"][0]["state"],"complete");
    assert_eq!(page["items"][0]["mail"]["id"],"root");
    assert!(page["origin"]["machine"].is_string());
    assert!(page["continuation"].is_string());
    let shown = delegated_ok(&fleet,&first,"show",json!({"ship":"harbor","id":"root"}));
    assert_eq!(shown["ref"],page["items"][0]["ref"]);
    let thread = delegated_ok(&fleet,&first,"thread",json!({"ship":"harbor","id":"root"}));
    assert_eq!(thread["items"].as_array().unwrap().len(),2);
    credential_request(&fleet,&other,"ack",json!({"ship":"harbor","refs":reader_refs(&page)}),"execution_failed");
    credential_request(&fleet,&other,"ack",json!({"ship":"ops","refs":reader_refs(&page)}),"execution_failed");
    delegated_ok(&fleet,&first,"ack",json!({"ship":"harbor","refs":reader_refs(&page)}));
    let independent = delegated_ok(&fleet,&other,"inbox",json!({"ship":"harbor"}));
    assert_eq!(independent["items"].as_array().unwrap().len(),3);
    stop(supervisor);
    fs::write(fleet.root.join("requests.jsonl"),"").unwrap();
    fs::write(&fleet.log,"").unwrap();
    let supervisor = fleet.supervise();
    fleet.wait("restarted linked reader hello",|e|e.first().cloned());
    let resumed = delegated_ok(&fleet,&first,"inbox",json!({"ship":"harbor","continuation":page["continuation"]}));
    assert_eq!(resumed["items"][0]["mail"]["id"],"reply");
    assert_eq!(resumed["items"][0]["ref"],thread["items"][1]["ref"]);
    let remote_registry = rusqlite::Connection::open(server.join("fleet.sqlite")).unwrap();
    remote_registry.execute("UPDATE door_reader_stream SET generation='replacement-generation'",[]).unwrap();
    credential_request(&fleet,&first,"ack",json!({"ship":"harbor","refs":reader_refs(&resumed)}),"execution_failed");
    credential_request(&fleet,&first,"inbox",json!({"ship":"harbor","continuation":page["continuation"]}),"execution_failed");
    // Another authenticated origin core cannot consume this origin's refs,
    // even if its client UUID and resource happen to match.
    let fresh = delegated_ok(&fleet,&first,"inbox",json!({"ship":"harbor"}));
    fs::write(&ssh,"#!/usr/bin/python3\nimport json, subprocess, sys\nr=json.loads(sys.stdin.readline())\nr['args']['origin']='00000000-0000-4000-8000-000000000001'\np=subprocess.run(['/bin/sh','-c',sys.argv[-1]],input=json.dumps(r)+'\\n',text=True)\nsys.exit(p.returncode)\n").unwrap();
    credential_request(&fleet,&first,"ack",json!({"ship":"harbor","refs":reader_refs(&fresh)}),"execution_failed");
    // Receiving-side restriction enforcement, independent of origin preflight.
    fs::write(&ssh,"#!/usr/bin/python3\nimport json, subprocess, sys\nr=json.loads(sys.stdin.readline())\nr['args']['context']['ships']=[]\np=subprocess.run(['/bin/sh','-c',sys.argv[-1]],input=json.dumps(r)+'\\n',text=True)\nsys.exit(p.returncode)\n").unwrap();
    credential_request(&fleet,&first,"inbox",json!({"ship":"harbor"}),"ship_not_allowed");
    fs::write(&ssh,"#!/usr/bin/python3\nimport json,sys\nr=json.loads(sys.stdin.readline())\nprint(json.dumps({'id':r['id'],'ok':False,'error':'unknown gateway command fleet.reader.v1'}))\n").unwrap();
    let old = delegated_ok(&fleet,&first,"inbox",json!({"ship":"harbor"}));
    assert_eq!(old["coverage"]["complete"],false);
    assert_eq!(old["coverage"]["machines"][0]["state"],"unsupported");
    fs::write(&ssh,"#!/bin/sh\nexit 73\n").unwrap();
    let down = delegated_ok(&fleet,&first,"inbox",json!({"ship":"harbor"}));
    assert_eq!(down["coverage"]["complete"],false);
    assert_eq!(down["coverage"]["machines"][0]["state"],"unavailable");
    assert_eq!(down["coverage"]["machines"][0]["retry"],"reconnect_then_list");
    let ack = delegated_ok(&fleet,&first,"ack",json!({"ship":"harbor","refs":reader_refs(&fresh)}));
    assert_eq!(ack["acknowledged"],0);
    assert_eq!(ack["outcome"],"unconfirmed");
    stop(supervisor);
}
