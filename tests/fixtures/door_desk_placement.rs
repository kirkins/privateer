#[test]
fn placement_decision_control_card_survives_legacy_fence_under_real_supervisor() {
    let fleet = Fleet::new();
    fs::write(fleet.root.join("adapter/door.py"), r#"import json, os, sys
log = open(os.environ['FAKE_DOOR_LOG'], 'a')
def note(v):
    log.write(json.dumps(v)+'\n'); log.flush()
note({'hello':json.loads(sys.stdin.readline())})
def ordinary(ident):
    print(json.dumps({'type':'request','id':ident,'command':'send','args':{
        'to':'ops','kind':'notice','body':'Ordinary captain message'}}),flush=True)
ordinary('staged-send')
for line in sys.stdin:
    event=json.loads(line); note(event)
    if event.get('id') == 'confirm' and event.get('ok'):
        ordinary('active-send')
    if event.get('type') == 'mail' and event.get('control') != 'desk_placement':
        print(json.dumps({'type':'delivered','ref':event['ref']}),flush=True)
    if event.get('control') == 'desk_placement':
        mail=event['mail']
        print(json.dumps({'type':'delivered','ref':event['ref']}),flush=True)
        print(json.dumps({'type':'request','id':'confirm','command':'send','args':{
            'to':mail['from'],'kind':'reply','correlation':mail['id'],'body':'Confirm this exact manifest',
            'payload':{'answer':{'option_id':'accept'}}}}),flush=True)
"#).unwrap();
    fleet.add();
    let run = |args: &[&str]| fleet.ok_in(&fleet.root.join("ships/ops"),args);
    run(&["mail.send", "--json", &json!({"id":"retained-unread","from":"ops","to":"captain","kind":"notice","body":"Must not be acknowledged by the control card"}).to_string()]);
    // The supervisor must also work when no v1 cursor has ever existed. The
    // control receipt itself is not a mail ack. Captain decision A (2026-10-31)
    // resumes independent ordinary delivery only after owner certification.
    run(&["machine.desk.propose", "--json", &json!({"domain":"survey-domain","owner":"here","sources":["here"]}).to_string()]);
    let supervisor = fleet.supervise();
    fleet.wait("placement card confirmation", |events| events.iter().find(|e|e["id"] == "confirm" && e["ok"] == true).cloned());
    for id in ["staged-send", "active-send"] {
        let sent = fleet.wait("ordinary send behind placement fence", |events| response(events, id).cloned());
        assert_eq!(sent["ok"], true, "{sent}");
        assert!(sent["result"]["returned"].is_null());
    }
    let status: Value = serde_json::from_str(&run(&["machine.desk.status","--json","{}"])) .unwrap();
    assert_eq!(status["records"][0]["state"],"active");
    assert_eq!(status["desk_here"],true);
    let mail = fleet.wait("owner ordinary mail after certification", |events| events.iter().find(|e|e["mail"]["id"] == "retained-unread").cloned());
    fleet.wait_delivered(&[mail]);
    stop(supervisor);
    let conn = rusqlite::Connection::open(fleet.home("ops").join("privateer.sqlite")).unwrap();
    let through: i64 = conn.query_row("SELECT through_seq FROM door_cursor WHERE door='fake'",[],|r|r.get(0)).unwrap();
    assert!(through > 0);
}

#[test]
fn generation_one_owner_keeps_legacy_away_digest_and_cursors_across_restart() {
    let fleet = Fleet::new();
    fleet.digest_door(json!({"digest_every_mins":0}));
    let staged = fleet.call("ops", "machine.desk.propose", json!({"domain":"beta-domain","owner":"here","sources":["here"]}));
    let p = &staged["records"][0]["proposal"];
    fleet.call("ops", "machine.desk.accept", json!({"proposal_id":p["proposal_id"],"manifest_hash":p["manifest_hash"]}));
    // Decision A (2026-10-31): these are independent legacy channels, not desk reads.
    let sent = fleet.tell("ops", "ordinary owner mail", "notice", "normal");
    let supervisor = fleet.supervise();
    let ordinary = fleet.wait("owner captain mail", |entries| entries.iter().find(|e| e["mail"]["id"] == sent["id"]).cloned());
    fleet.wait_delivered(&[ordinary]);
    let hello = &fleet.entries()[0]["hello"]["mail_delivery"];
    assert!(hello["summary"].as_str().unwrap().contains("reads captain mail stored on this machine"));
    assert!(!hello.to_string().contains("controls only"));
    fleet.ok(&["afk", "owner away"]);
    fleet.tell("ops", "owner held notice", "notice", "normal");
    let digest = fleet.wait("owner legacy digest", |entries| entries.iter().find(|e| e["type"] == "digest" && text(e).contains("owner held notice")).cloned());
    assert!(digest["ref"].as_str().unwrap().starts_with("digest:"));
    fleet.ok(&["back"]);
    stop(supervisor);
    let conn = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM away WHERE returned_ms IS NULL", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    assert!(conn.query_row("SELECT EXISTS(SELECT 1 FROM away_digest WHERE kind='digest')", [], |r| r.get::<_, bool>(0)).unwrap());
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM desk_subscription", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    let supervisor = fleet.supervise();
    fleet.wait("restarted owner door", |entries| (entries.iter().filter(|e| e.get("hello").is_some()).count() >= 2).then_some(()));
    let fresh = fleet.tell("ops", "after owner restart", "notice", "normal");
    let after = fleet.wait("mail after owner restart", |entries| entries.iter().find(|e| e["mail"]["id"] == fresh["id"]).cloned());
    fleet.wait_delivered(&[after]);
    assert!(!events(&fleet.entries(), 1, "mail").iter().any(|e| e["mail"]["id"] == sent["id"]));
    stop(supervisor);
}

#[test]
fn pending_local_bootstrap_keeps_scoped_refusal_typed_without_effects() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (_, token) = delegated_issue(&fleet, "beta", "mail:read,captain:presence", None);
    let (_, read_only) = delegated_issue(&fleet, "read", "mail:read", None);
    let staged = fleet.call("ops", "machine.desk.propose", json!({"domain":"beta-domain","owner":"here","sources":["here"]}));
    let supervisor = fleet.supervise();
    fleet.wait("pending scoped hello", |e| e.first().cloned());
    for (command, args) in [
        ("away", json!({"action":"start","words":"must not enter"})),
        ("away", json!({"action":"back"})),
        ("digest", json!({})),
        ("ack", json!({"refs":["unserved"]})),
    ] {
        let response = delegated_exchange(&fleet, &token, command, args);
        assert_eq!(response["error"], json!({"code":"legacy_writer_fenced","message":"Legacy channel fenced by desk placement; migration required (incomplete feature)"}));
    }
    credential_request(&fleet, &read_only, "digest", json!({}), "insufficient_scope");
    // These read-only commands keep their existing pending-route refusal;
    // they must not be relabelled as fenced writer operations.
    credential_request(&fleet, &token, "away", json!({"action":"status"}), "execution_failed");
    credential_request(&fleet, &token, "held", json!({}), "execution_failed");
    let conn = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    // Read-only status can create empty schema, but no presence/digest facts.
    for table in ["away", "away_digest"] {
        let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)", [table], |r| r.get(0)).unwrap();
        if exists { assert_eq!(conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get::<_, i64>(0)).unwrap(), 0); }
    }
    let p = &staged["records"][0]["proposal"];
    fleet.call("ops", "machine.desk.accept", json!({"proposal_id":p["proposal_id"],"manifest_hash":p["manifest_hash"]}));
    assert!(delegated_ok(&fleet, &token, "away", json!({"action":"status"}))["away"].is_null());
    assert!(delegated_ok(&fleet, &token, "held", json!({}))["held"].as_array().unwrap().is_empty());
    assert!(delegated_ok(&fleet, &token, "away", json!({"action":"start"}))["away"].is_object());
    assert_eq!(delegated_ok(&fleet, &token, "away", json!({"action":"back"}))["returned"], true);
    // Not a fence failure: unrelated unserved digest refs keep their old error.
    credential_request(&fleet, &token, "ack", json!({"refs":["unserved"]}), "execution_failed");
    stop(supervisor);
}

#[test]
fn source_telegram_stays_cards_only_but_existing_scoped_route_runs_at_owner() {
    let source = Fleet::with_ships(&[("survey", true)]);
    let owner = Fleet::with_ships(&[("relay", true)]);
    source.call("survey", "machine.add", json!({"name":"beta","target":"local-test","executable":BIN,
        "as":"alpha","fleet":owner.root.join("state/fleet.sqlite"),"transport":"local"}));
    fs::write(source.root.join("adapter/door.py"), r#"import json, os, sys
log = open(os.environ['FAKE_DOOR_LOG'], 'a')
def note(m):
    log.write(json.dumps(m)+'\n'); log.flush()
note({'hello':json.loads(sys.stdin.readline())})
print(json.dumps({'type':'request','id':'poll-barrier','command':'status','args':{}}),flush=True)
for line in sys.stdin:
    m=json.loads(line); note(m)
    if m.get('type') == 'mail':
        print(json.dumps({'type':'delivered','ref':m['ref']}),flush=True)
"#).unwrap();
    source.add();
    source.tell("survey", "retained source mail", "notice", "normal");
    let staged = source.call("survey", "machine.desk.propose", json!({"domain":"beta-domain","owner":"beta","sources":["alpha","beta"]}));
    let supervisor = source.supervise();
    source.wait("pending source poll completed", |e| response(e, "poll-barrier").cloned());
    source.wait("source placement control", |e| e.iter().find(|v| v["control"] == "desk_placement").cloned());
    assert!(source.entries().iter().filter(|e| e["type"] == "mail").all(|e| e["control"] == "desk_placement"));
    let p = &staged["records"][0]["proposal"];
    source.call("survey", "machine.desk.accept", json!({"proposal_id":p["proposal_id"],"manifest_hash":p["manifest_hash"]}));
    source.call("survey", "machine.desk.status", json!({}));
    assert_eq!(owner.call("relay", "machine.desk.status", json!({}))["desk_here"], true);
    stop(supervisor);
    let supervisor = source.supervise();
    source.wait("active source poll completed after restart", |e| {
        response(&events(e, 1, "response"), "poll-barrier").cloned()
    });
    assert!(source.entries().iter().filter(|e| e["type"] == "mail").all(|e| e["control"] == "desk_placement"));
    let mail = rusqlite::Connection::open(source.home("survey").join("privateer.sqlite")).unwrap();
    assert_eq!(mail.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name='door_cursor'", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    assert!(source.refused(&["afk", "source local"]).contains("legacy_writer_fenced"));
    stop(supervisor);
    // Existing routing is not a new subscription route: the actual legacy
    // writer runs on beta, where decision A permits it. Alpha never unfences.
    // Existing v1 registrations require an explicit operator protocol change.
    manifest_protocol(&source, 2);
    source.ok(&["door", "add", "fake", "--manifest", source.root.join("adapter/manifest.json").to_str().unwrap(),
        "--config", source.root.join("door.toml").to_str().unwrap(), "--protocol", "2"]);
    delegated_adapter(&source);
    let (_, token) = delegated_issue(&source, "routed", "mail:read,captain:presence", None);
    let supervisor = source.supervise();
    source.wait("source scoped hello", |e| (e.iter().filter(|v| v.get("hello").is_some()).count() >= 3).then_some(()));
    let away = delegated_ok(&source, &token, "away", json!({"action":"start"}));
    assert_eq!(away["desk"]["machine"], "beta");
    owner.tell("relay", "held at owner", "notice", "normal");
    let digest = delegated_ok(&source, &token, "digest", json!({}));
    assert!(digest["ref"].is_string());
    assert!(text(&digest["digest"]).contains("held at owner"));
    assert_eq!(delegated_ok(&source, &token, "away", json!({"action":"back"}))["returned"], true);
    assert!(source.refused(&["afk", "still source local"]).contains("legacy_writer_fenced"));
    stop(supervisor);
}
