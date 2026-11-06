// A trusted adapter, real supervisor and real gateway subprocesses, with an
// ask carried into a non-default home on the reverse-only side of the link.
fn legacy_mail_adapter(fleet: &Fleet) {
    fs::write(
        fleet.root.join("adapter/door.py"),
        r#"import json, os, sys, threading, time
root = os.path.dirname(os.environ['FAKE_DOOR_LOG'])
log = open(os.environ['FAKE_DOOR_LOG'], 'a')
def note(m):
    log.write(json.dumps(m)+'\n'); log.flush()
note({'hello':json.loads(sys.stdin.readline())})
def requests():
    seen = 0
    while True:
        try:
            for line in open(os.path.join(root, 'requests.jsonl')).readlines()[seen:]:
                if not line.endswith('\n'): break
                print(line.strip(), flush=True)
                seen += 1
        except FileNotFoundError: pass
        time.sleep(.02)
threading.Thread(target=requests, daemon=True).start()
for line in sys.stdin: note(json.loads(line))
"#,
    )
    .unwrap();
    fleet.add();
}

fn legacy_mail_send(fleet: &Fleet, args: Value) -> Value {
    use std::io::Write;
    let id = uuid::Uuid::new_v4().to_string();
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(fleet.root.join("requests.jsonl"))
        .unwrap();
    writeln!(
        file,
        "{}",
        json!({"type":"request","id":id,"command":"send","args":args})
    )
    .unwrap();
    fleet.wait("trusted mail response", |events| {
        response(events, &id).cloned()
    })
}

#[test]
fn legacy_door_remote_correlations_use_the_holding_mailbox() {
    let fleet = Fleet::new();
    let server = fleet.root.join("remote");
    let harbor = server.join("harbor");
    fs::create_dir_all(&harbor).unwrap();
    let remote = |args: &[&str]| {
        let output = fleet
            .command(&harbor)
            .env("HOME", &server)
            .env("PRIVATEER_FLEET", server.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", server.join("ledger"))
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    remote(&["init", "--ship", "harbor", "--harness", "codex"]);
    let ssh = fleet.root.join("fake-ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\nfor argument do remote=$argument; done\nexec /bin/sh -c \"$remote\"\n",
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    remote(&[
        "machine",
        "add",
        "desk",
        "test-host",
        "--executable",
        BIN,
        "--remote-fleet",
        fleet.root.join("state/fleet.sqlite").to_str().unwrap(),
        "--as",
        "peer",
    ]);
    let decision = json!({"question":"Which chart?","options":[
        {"id":"east","label":"East","what_happens":"Read east chart"},
        {"id":"west","label":"West","what_happens":"Read west chart"}]});
    remote(&[
        "mail.send",
        "--json",
        &json!({"id":"carried-decision","from":"harbor",
        "to":"captain","kind":"decision","body":"Which chart?","payload":{"decision":decision}})
        .to_string(),
    ]);
    // Model the retained desk copy the door actually presented. Its source
    // ship is remote; the containing local home is ops, not liaison.
    let copy = rusqlite::Connection::open(fleet.home("ops").join("privateer.sqlite")).unwrap();
    copy.execute("INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,state)
        VALUES('carried-decision','harbor','captain','decision','Which chart?',?1,'normal',1,'open')",
        [json!({"decision":decision}).to_string()]).unwrap();
    legacy_mail_adapter(&fleet);
    let supervisor = fleet.supervise();
    let heard = fleet.wait("carried decision", |events| {
        events
            .iter()
            .find(|e| e["type"] == "mail" && e["mail"]["id"] == "carried-decision")
            .cloned()
    });
    assert_eq!(heard["ship"], "ops");
    assert_eq!(heard["mail"]["from"], "harbor");
    let mut answer = json!({"to":"harbor","kind":"reply","correlation":"carried-decision",
        "body":"East please","payload":{"answer":{"option_id":"absent"}}});
    let invalid = legacy_mail_send(&fleet, answer.clone());
    assert_eq!(invalid["ok"], false, "{invalid}");
    assert!(
        !invalid["error"]
            .as_str()
            .unwrap()
            .contains("unknown correlated message"),
        "{invalid}"
    );
    assert_eq!(
        fleet.call("ops", "mail.show", json!({"id":"carried-decision"}))["state"],
        "open"
    );
    answer["payload"]["answer"]["option_id"] = json!("east");
    let sent = legacy_mail_send(&fleet, answer.clone());
    assert_eq!(sent["ok"], true, "{sent}");
    assert_eq!(sent["result"]["ship"], "harbor");
    assert_eq!(sent["result"]["mail"]["delivery"], "pending");
    assert_eq!(
        fleet.call("ops", "mail.show", json!({"id":"carried-decision"}))["state"],
        "answered"
    );
    let stale = legacy_mail_send(&fleet, answer);
    assert_eq!(stale["ok"], false, "{stale}");
    assert!(
        stale["error"]
            .as_str()
            .unwrap()
            .contains("already answered"),
        "{stale}"
    );
    stop(supervisor);
    // Delivery survives the door restart and the origin closes only when it
    // collects the exact typed reply from the holding home.
    let origin = || -> Value {
        serde_json::from_str(&remote(&[
            "mail.show",
            "--json",
            r#"{"id":"carried-decision"}"#,
        ]))
        .unwrap()
    };
    assert_eq!(origin()["state"], "open");
    remote(&["machine", "collect"]);
    assert_eq!(origin()["state"], "answered");
    let reply: Value = serde_json::from_str(&remote(&[
        "mail.show",
        "--json",
        &json!({"id":sent["result"]["mail"]["id"]}).to_string(),
    ]))
    .unwrap();
    assert_eq!(reply["payload"]["answer"]["option_id"], "east");
    fs::write(fleet.root.join("requests.jsonl"), "").unwrap();
    fs::write(&fleet.log, "").unwrap();
    let supervisor = fleet.supervise();
    fleet.wait("restarted trusted adapter", |events| {
        events.first().cloned()
    });
    let followup = legacy_mail_send(
        &fleet,
        json!({"to":"harbor","kind":"request",
        "correlation":"carried-decision","body":"Read the scale too"}),
    );
    assert_eq!(followup["ok"], true, "{followup}");
    assert_eq!(
        followup["result"]["mail"]["correlation"],
        "carried-decision"
    );
    // Replies to the captain's own outgoing follow-up retain that home too.
    let own = legacy_mail_send(
        &fleet,
        json!({"to":"harbor","kind":"request",
        "correlation":followup["result"]["mail"]["id"],"body":"And the legend"}),
    );
    assert_eq!(own["ok"], true, "{own}");
    let missing = legacy_mail_send(
        &fleet,
        json!({"to":"harbor","kind":"reply",
        "correlation":"missing","body":"No matching conversation"}),
    );
    assert_eq!(missing["ok"], false, "{missing}");
    assert!(
        missing["error"]
            .as_str()
            .unwrap()
            .contains("unknown correlated message"),
        "{missing}"
    );
    copy.execute(
        "INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,state)
        VALUES('unrelated','ops','captain','request','Local question','{}','normal',2,'open')",
        [],
    )
    .unwrap();
    let unrelated = legacy_mail_send(
        &fleet,
        json!({"to":"harbor","kind":"request",
        "correlation":"unrelated","body":"Must not attach to another ship"}),
    );
    assert_eq!(unrelated["ok"], false, "{unrelated}");
    for ship in ["liaison", "ops"] {
        let db = rusqlite::Connection::open(fleet.home(ship).join("privateer.sqlite")).unwrap();
        db.execute(
            "INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,state)
            VALUES('ambiguous','harbor','captain','request','Which copy?','{}','normal',3,'open')",
            [],
        )
        .unwrap();
    }
    let ambiguous = legacy_mail_send(
        &fleet,
        json!({"to":"harbor","kind":"reply",
        "correlation":"ambiguous","body":"Must not guess"}),
    );
    assert_eq!(ambiguous["ok"], false, "{ambiguous}");
    assert!(
        ambiguous["error"]
            .as_str()
            .unwrap()
            .contains("multiple mailboxes"),
        "{ambiguous}"
    );
    for ship in ["liaison", "ops"] {
        assert_eq!(
            fleet.call(ship, "mail.show", json!({"id":"ambiguous"}))["state"],
            "open"
        );
    }
    stop(supervisor);
    remote(&["machine", "collect"]);
    for sent in [followup, own] {
        let delivered: Value = serde_json::from_str(&remote(&[
            "mail.show",
            "--json",
            &json!({"id":sent["result"]["mail"]["id"]}).to_string(),
        ]))
        .unwrap();
        assert_eq!(delivered["body"], sent["result"]["mail"]["body"]);
    }
    assert_eq!(
        copy.query_row(
            "SELECT count(*) FROM mail WHERE kind='reply' AND correlation='carried-decision'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}
