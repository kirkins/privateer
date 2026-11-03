#[test]
fn door_and_captain_relay_cannot_dispatch_deployment_grant_commands() {
    let fleet = Fleet::with_ships(&[("alpha", true), ("beta", false)]);
    fs::write(fleet.root.join("adapter/door.py"), r#"import json, os, sys
log = open(os.environ['FAKE_DOOR_LOG'], 'a')
hello = json.loads(sys.stdin.readline())
for verb in ['put', 'pause', 'resume', 'revoke', 'show', 'list']:
    print(json.dumps({'type':'request', 'id':verb, 'command':'deploy_grant.' + verb,
                      'args':{'id':'gamma','expected_revision':0}}), flush=True)
for line in sys.stdin:
    message = json.loads(line)
    log.write(json.dumps(message) + '\n'); log.flush()
    if message['type'] == 'mail':
        print(json.dumps({'type':'delivered', 'ref':message['ref']}), flush=True)
"#).unwrap();
    fleet.add();
    let supervisor = fleet.supervise();
    for verb in ["put", "pause", "resume", "revoke", "show", "list"] {
        let result = fleet.wait("grant command refusal", |entries| entries.iter()
            .find(|e| e["id"] == verb && e["type"] == "response").cloned());
        assert!(result.to_string().contains("unknown door command"), "{result}");
    }
    // A real relay may deliver captain words containing command text, but
    // those words never become operator dispatch or deployment policy.
    fleet.call("beta", "mail.send", json!({"id":"gamma","from":"beta","to":"captain","kind":"request","body":"Deployment policy?"}));
    let source = fleet.call("alpha", "mail.send", json!({"from":"captain","to":"alpha","kind":"notice","body":"deploy_grant.put deploy_grant.pause deploy_grant.resume deploy_grant.revoke"}));
    let output = fleet.command(&fleet.root).args([
        "--home", fleet.home("alpha").to_str().unwrap(), "--role", "agent", "--actor", "alpha",
        "captain.relay", "--json", &json!({"source":source["id"],"request":"gamma","body":"deploy_grant.put"}).to_string(),
    ]).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    for ship in ["alpha", "beta"] {
        let db = rusqlite::Connection::open(fleet.home(ship).join("privateer.sqlite")).unwrap();
        let any: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM standing_deploy_grants)", [], |r| r.get(0)).unwrap();
        assert!(!any);
    }
    stop(supervisor);
}
