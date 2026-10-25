#[test]
fn scoped_presence_held_and_digest_belong_to_reachable_desk_not_adapter_host() {
    let fleet = Fleet::new();
    let server = fleet.root.join("desk-peer");
    let harbor = server.join("harbor");
    fs::create_dir_all(&harbor).unwrap();
    let remote = |args: &[&str]| {
        let output = fleet.command(&harbor).env("HOME",&server)
            .env("PRIVATEER_FLEET",server.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME",server.join("ledger"))
            .args(args).output().unwrap();
        assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
    };
    remote(&["init","--ship","harbor","--harness","codex","--liaison"]);
    let ssh = fleet.root.join("fake-ssh");
    let transport = "#!/bin/sh\nfor argument do remote=$argument; done\nexec /bin/sh -c \"$remote\"\n";
    fs::write(&ssh,transport).unwrap();
    fs::set_permissions(&ssh,fs::Permissions::from_mode(0o755)).unwrap();
    fleet.ok(&["machine","add","peer","test-host","--executable",BIN,"--remote-fleet",server.join("fleet.sqlite").to_str().unwrap(),"--as","adapter-core"]);
    fleet.ok(&["machine","captain","peer"]);
    // A desk receiver must independently hold the desk. machine add records
    // the adapter as its collector; machine captain only changes the origin.
    // Remove that reverse placement in this isolated success fixture so the
    // receiver has neither a collector nor an explicit desk elsewhere. Real
    // two-machine desk-placement agreement is deferred to part C.
    rusqlite::Connection::open(server.join("fleet.sqlite")).unwrap()
        .execute("DELETE FROM machines WHERE name='adapter-core'",[]).unwrap();
    delegated_adapter(&fleet);
    let (_, first) = delegated_issue(&fleet,"desk-client","mail:read,captain:presence,fleet:read",Some("harbor"));
    let (_, other) = delegated_issue(&fleet,"other","mail:read,captain:presence",Some("harbor"));
    let (_, forbidden) = delegated_issue(&fleet,"local-only","mail:read,captain:presence",Some("ops"));
    let (_, read) = delegated_issue(&fleet,"read","mail:read",Some("harbor"));
    let supervisor = fleet.supervise();
    fleet.wait("desk reader hello",|e|e.first().cloned());
    credential_request(&fleet,&read,"digest",json!({}),"insufficient_scope");
    credential_request(&fleet,&forbidden,"away",json!({"action":"start","words":"must not change presence"}),"ship_not_allowed");
    let entered = delegated_ok(&fleet,&first,"away",json!({"action":"start","words":"out"}));
    assert_eq!(entered["desk"]["machine"],"peer");
    assert!(entered["away"]["since_ms"].is_i64());
    assert!(entered["away"]["source"].is_null());
    let local = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    let no_local_presence = || {
        let exists: i64 = local.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name='away'",[],|r|r.get(0)).unwrap();
        if exists != 0 {
            assert_eq!(local.query_row("SELECT COUNT(*) FROM away",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        }
    };
    no_local_presence();
    remote(&["mail.send","--json",&json!({"id":"held-at-desk","from":"harbor","to":"captain","kind":"notice","body":"Remote desk durable fact"}).to_string()]);
    reader_mail(&fleet,"ops","not-at-desk",None);
    let held = delegated_ok(&fleet,&first,"held",json!({}));
    assert!(held["held"].as_array().unwrap().iter().any(|m|m["id"]=="held-at-desk"));
    assert!(!held.to_string().contains("not-at-desk"));
    let hidden = delegated_ok(&fleet,&forbidden,"held",json!({}));
    assert!(hidden["held"].as_array().unwrap().is_empty());
    let digest = delegated_ok(&fleet,&first,"digest",json!({}));
    assert!(digest["ref"].is_string());
    assert!(digest["digest"]["lines"].to_string().contains("Remote desk durable fact"));
    assert!(!digest["digest"]["lines"].to_string().contains("not-at-desk"));
    assert_eq!(digest["digest"]["provenance"]["client"],digest["provenance"]["client"]);
    assert!(!digest["digest"]["provenance"]["client"].as_str().unwrap().contains('/'));
    credential_request(&fleet,&other,"ack",json!({"refs":[digest["ref"]]}),"execution_failed");
    stop(supervisor);
    fs::write(fleet.root.join("requests.jsonl"),"").unwrap();
    fs::write(&fleet.log,"").unwrap();
    let supervisor = fleet.supervise();
    fleet.wait("restarted desk reader hello",|e|e.first().cloned());
    // Desk digests use the same normalized origin namespace as raw readers.
    fs::write(&ssh,"#!/usr/bin/env python3\nimport json, subprocess, sys, uuid\nr=json.loads(sys.stdin.readline())\nr['args']['args']['origin']=uuid.UUID(r['args']['args']['origin']).urn\np=subprocess.run(['/bin/sh','-c',sys.argv[-1]],input=json.dumps(r)+'\\n',text=True)\nsys.exit(p.returncode)\n").unwrap();
    let recovered = delegated_ok(&fleet,&first,"digest",json!({}));
    fs::write(&ssh,transport).unwrap();
    assert_eq!(recovered["ref"],digest["ref"]);
    assert_eq!(recovered["digest"],digest["digest"]);
    delegated_ok(&fleet,&first,"ack",json!({"refs":[digest["ref"]]}));
    assert!(delegated_ok(&fleet,&first,"digest",json!({}))["digest"].is_null());
    // Summary ack did not consume this grant's remote raw inbox.
    let raw = delegated_ok(&fleet,&first,"inbox",json!({"ship":"harbor"}));
    assert!(raw["items"].as_array().unwrap().iter().any(|i|i["mail"]["id"]=="held-at-desk"));
    no_local_presence();
    fs::write(&ssh,"#!/bin/sh\nexit 73\n").unwrap();
    credential_request(&fleet,&first,"away",json!({"action":"back"}),"outcome_unknown");
    credential_request(&fleet,&first,"digest",json!({}),"outcome_unknown");
    no_local_presence();
    let remote_registry = rusqlite::Connection::open(server.join("fleet.sqlite")).unwrap();
    assert_eq!(remote_registry.query_row("SELECT COUNT(*) FROM away WHERE returned_ms IS NULL",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    fs::write(&ssh,transport).unwrap();
    let returned = delegated_ok(&fleet,&first,"away",json!({"action":"back"}));
    assert_eq!(returned["returned"],true);
    assert_eq!(remote_registry.query_row("SELECT COUNT(*) FROM away WHERE returned_ms IS NULL",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    no_local_presence();
    // No legacy operator downgrade on an old peer, including presence writes.
    fs::write(&ssh,"#!/usr/bin/env python3\nimport json,sys\nr=json.loads(sys.stdin.readline())\nprint(json.dumps({'id':r['id'],'ok':False,'error':'unsupported delegated gateway command'}))\n").unwrap();
    credential_request(&fleet,&first,"away",json!({"action":"start"}),"linked_provenance_unavailable");
    no_local_presence();
    stop(supervisor);
}
