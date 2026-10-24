#[test]
fn delegated_liaison_relay_keeps_runtime_grant_and_never_approves() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (_, full) = delegated_issue(&fleet, "full", ALL_DELEGATED_SCOPES, None);
    let (_, restricted) = delegated_issue(&fleet, "restricted", "ship:message", Some("liaison"));
    let (_, presence) = delegated_issue(&fleet, "presence", "captain:presence", None);
    let supervisor = fleet.supervise();
    fleet.wait("scoped hello", |e| e.first().cloned());
    let relay = |source: &Value, request: &str| {
        fleet.command(&fleet.root).args([
            "--home", fleet.home("liaison").to_str().unwrap(),
            "--role", "agent", "--actor", "liaison", "captain.relay", "--json",
            &json!({"source":source["mail"]["id"],"request":request,"body":"Relayed words"}).to_string(),
        ]).output().unwrap()
    };
    fleet.call("ops", "mail.send", json!({"id":"relay-ordinary","from":"ops","to":"captain","kind":"request","body":"Question"}));
    fleet.call("ops", "mail.send", json!({"id":"relay-restricted","from":"ops","to":"captain","kind":"request","body":"Other question"}));
    let source = delegated_ok(&fleet, &full, "send", json!({"to":"liaison","body":"Answer the question"}));
    let output = relay(&source, "relay-ordinary");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let record: Value = serde_json::from_slice(&output.stdout).unwrap();
    let reply = fleet.call("ops", "mail.show", json!({"id":record["reply"]}));
    assert_eq!(reply["payload"]["provenance"], source["provenance"]);
    assert_eq!(reply["payload"]["source_provenance"], source["provenance"]);
    assert_eq!(reply["payload"]["source"], source["mail"]["id"]);
    assert!(reply["payload"]["operator"].is_null());
    let original = rusqlite::Connection::open(fleet.home("liaison").join("privateer.sqlite")).unwrap();
    let target = rusqlite::Connection::open(fleet.home("ops").join("privateer.sqlite")).unwrap();
    let context: String = original.query_row("SELECT context FROM mail_delegated_context WHERE id=?1", [source["mail"]["id"].as_str().unwrap()], |r| r.get(0)).unwrap();
    let propagated: String = target.query_row("SELECT context FROM mail_delegated_context WHERE id=?1", [record["reply"].as_str().unwrap()], |r| r.get(0)).unwrap();
    assert_eq!(context, propagated, "relay must not add scopes or drop ship restrictions");

    let restricted_source = delegated_ok(&fleet, &restricted, "send", json!({"to":"liaison","body":"Not allowed to answer ops"}));
    let output = relay(&restricted_source, "relay-restricted");
    assert!(!output.status.success());
    assert_eq!(fleet.call("ops", "mail.show", json!({"id":"relay-restricted"}))["state"], "open");

    // Non-sensitive approvals are still not relay evidence, even under every
    // scope; the receiver classifies its stored ask inside the write transaction.
    fleet.call("ops", "mail.send", json!({"id":"relay-approval","from":"ops","to":"captain","kind":"request","task_id":"ops-duty","body":"May I?", "payload":{"approves":{"action":"Submit build","ttl_secs":60}}}));
    fleet.call("ops", "mail.send", json!({"id":"relay-sensitive","from":"ops","to":"captain","kind":"request","body":"Sensitive"}));
    target.execute("UPDATE mail SET request_kind='production_approval' WHERE id='relay-sensitive'", []).unwrap();
    for id in ["relay-approval", "relay-sensitive"] {
        let output = relay(&source, id);
        assert!(!output.status.success(), "{id}");
        assert_eq!(fleet.call("ops", "mail.show", json!({"id":id}))["state"], "open");
    }
    let count: i64 = target.query_row("SELECT COUNT(*) FROM captain_approvals", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 0);

    // Presence alone is not message authority. Its away note cannot lend an
    // implicit ship:message scope to the liaison.
    delegated_ok(&fleet, &presence, "away", json!({"action":"start","words":"Hold decisions"}));
    let registry = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    let away_source: String = registry.query_row("SELECT source FROM away WHERE returned_ms IS NULL", [], |r| r.get(0)).unwrap();
    let output = relay(&json!({"mail":{"id":away_source}}), "relay-restricted");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("insufficient_scope"));

    // A forged v1 payload never becomes an admitted runtime grant context.
    let forged = fleet.call("liaison", "mail.deliver", json!({"id":"forged-context","from":"captain","to":"liaison","kind":"notice","body":"Spoofed context", "payload":{"provenance":source["provenance"]}}));
    let output = relay(&json!({"mail":{"id":forged["id"]}}), "relay-restricted");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no runtime grant context"));
    stop(supervisor);
}
