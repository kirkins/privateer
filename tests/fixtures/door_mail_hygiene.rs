#[test]
fn trusted_door_rejects_control_bodies_and_audits_only_committed_sends() {
    let fleet = Fleet::with_ships(&[("alpha", true), ("beta", false)]);
    let requests = json!([
        {"type":"request","id":"bad-control","command":"send","args":{"to":"beta","body":"\u{1b}\u{06}"}},
        {"type":"request","id":"bad-mixed","command":"send","args":{"to":"beta","body":"text\u{1b}more"}},
        {"type":"request","id":"good","command":"send","args":{"id":"safe-mail","to":"beta","kind":"notice","body":"船 🌊 e\u{301}\n\tline\r\nnext"}},
        {"type":"request","id":"retry","command":"send","args":{"id":"safe-mail","to":"beta","kind":"notice","body":"船 🌊 e\u{301}\n\tline\r\nnext"}}
    ]);
    fs::write(fleet.root.join("adapter/door.py"), format!(r#"import json, os, sys
log = open(os.environ['FAKE_DOOR_LOG'], 'a')
json.loads(sys.stdin.readline())
for request in json.loads({encoded}):
    print(json.dumps(request), flush=True)
for line in sys.stdin:
    log.write(line); log.flush()
"#, encoded=json!(requests.to_string()))).unwrap();
    fleet.add();
    let supervisor = fleet.supervise();
    fleet.wait("trusted retry", |entries| response(entries, "retry").cloned());
    let entries = fleet.entries();
    for id in ["bad-control", "bad-mixed"] {
        let result = response(&entries, id).unwrap();
        assert_eq!(result["ok"], false);
        assert!(result["error"].as_str().unwrap().contains("U+001B"), "{result}");
    }
    for id in ["good", "retry"] {
        assert_eq!(response(&entries, id).unwrap()["ok"], true);
    }
    let conn = rusqlite::Connection::open(fleet.home("beta").join("privateer.sqlite")).unwrap();
    let body: String = conn.query_row("SELECT body FROM mail WHERE id='safe-mail'", [], |r| r.get(0)).unwrap();
    assert_eq!(body, "船 🌊 e\u{301}\n\tline\r\nnext");
    let audits: Vec<String> = conn.prepare("SELECT payload FROM audit WHERE kind='captain.send' AND json_extract(payload,'$.id')='safe-mail'")
        .unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(audits.len(), 1);
    assert_eq!(serde_json::from_str::<Value>(&audits[0]).unwrap()["body_bytes"], body.len());
    assert!(!audits[0].contains(&body));
    assert!(!conn.query_row("SELECT EXISTS(SELECT 1 FROM mail WHERE sender='captain' AND body LIKE '%' || char(27) || '%')", [], |r| r.get::<_, bool>(0)).unwrap());
    stop(supervisor);
}

#[test]
fn scoped_door_rejects_control_bodies_before_storage() {
    let fleet = Fleet::with_ships(&[("alpha", true), ("beta", false)]);
    delegated_adapter(&fleet);
    let (_, token) = delegated_issue(&fleet, "alpha-writer", "ship:message", None);
    let supervisor = fleet.supervise();
    fleet.wait("scoped hello", |entries| entries.first().cloned());
    for body in ["\u{1b}\u{06}", "text\u{1b}more", "text\u{85}", " \t\r\n"] {
        let result = delegated_exchange(&fleet, &token, "send", json!({"to":"beta","kind":"notice","body":body,"idempotency_key":"safe-retry"}));
        assert_eq!(result["ok"], false);
        // Scoped wire errors remain redacted; body validation is an internal
        // unsupported_arguments refusal, not reflected client text.
        assert_eq!(result["error"]["code"], "not_found", "{result}");
    }
    let args = json!({"to":"beta","kind":"notice","body":"船 🌊 e\u{301}\n\tline\r\nnext","idempotency_key":"safe-retry"});
    let sent = delegated_ok(&fleet, &token, "send", args.clone());
    let retry = delegated_ok(&fleet, &token, "send", args);
    assert_eq!(sent["mail"]["id"], retry["mail"]["id"]);
    let id = sent["mail"]["id"].as_str().unwrap();
    let conn = rusqlite::Connection::open(fleet.home("beta").join("privateer.sqlite")).unwrap();
    let audits: Vec<String> = conn.prepare("SELECT payload FROM audit WHERE kind='captain.send' AND json_extract(payload,'$.id')=?1")
        .unwrap().query_map([id], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(audits.len(), 1);
    let metadata: Value = serde_json::from_str(&audits[0]).unwrap();
    assert_eq!(metadata["origin"], "delegated");
    assert_eq!(metadata["body_bytes"], sent["mail"]["body"].as_str().unwrap().len());
    assert!(metadata.get("body").is_none());
    assert!(!conn.query_row("SELECT EXISTS(SELECT 1 FROM mail WHERE sender='captain' AND (body LIKE '%' || char(27) || '%' OR body LIKE '%' || char(133) || '%'))", [], |r| r.get::<_, bool>(0)).unwrap());
    stop(supervisor);
}
