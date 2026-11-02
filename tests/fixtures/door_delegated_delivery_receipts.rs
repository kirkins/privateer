#[test]
fn delegated_reverse_delivery_keeps_success_evidence_without_captain_receipts() {
    use sha2::{Digest, Sha256};
    use std::io::Write;

    let fleet = Fleet::new();
    let server = fleet.root.join("quiet-reverse-fleet");
    let harbor = server.join("harbor");
    fs::create_dir_all(&harbor).unwrap();
    let remote = |args: &[&str]| {
        let output = fleet.command(&harbor)
            .env("HOME", &server)
            .env("PRIVATEER_FLEET", server.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", server.join("ledger"))
            .args(args).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    };
    remote(&["init", "--ship", "harbor", "--harness", "codex", "--liaison"]);
    let ssh = fleet.root.join("fake-ssh");
    fs::write(&ssh, "#!/bin/sh\nfor argument do remote=$argument; done\nexec /bin/sh -c \"$remote\"\n").unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    remote(&["machine", "add", "receiver", "test-host", "--executable", BIN,
        "--remote-fleet", fleet.root.join("state/fleet.sqlite").to_str().unwrap(),
        "--as", "source-machine"]);
    delegated_adapter(&fleet);
    let (_, full) = delegated_issue(&fleet, "full", ALL_DELEGATED_SCOPES, None);
    let (_, messaging) = delegated_issue(&fleet, "messaging", "ship:message", Some("harbor"));
    for id in ["remote-ask", "remote-approval", "remote-sensitive", "remote-privileged"] {
        remote(&["mail.send", "--json", &json!({"id":id,"from":"harbor","to":"captain","kind":"request","body":"Question"}).to_string()]);
    }
    let receiver = rusqlite::Connection::open(harbor.join(".privateer/privateer.sqlite")).unwrap();
    receiver.execute("UPDATE mail SET payload=json_set(payload,'$.approves',json('{\"action\":\"Submit build\",\"ttl_secs\":60}')) WHERE id='remote-approval'", []).unwrap();
    receiver.execute("UPDATE mail SET request_kind='production_approval' WHERE id='remote-sensitive'", []).unwrap();
    receiver.execute("UPDATE mail SET request_kind='privileged_action' WHERE id='remote-privileged'", []).unwrap();
    let supervisor = fleet.supervise();
    fleet.wait("quiet delivery scoped hello", |entries| entries.first().cloned());
    let mut sent = vec![];
    for (kind, id) in [("request", "quiet-request".to_owned()), ("notice", "n".repeat(160)), ("reply", "quiet-reply".to_owned())] {
        let mut args = json!({"id":id,"to":"harbor","kind":kind,"body":"Quiet delegated words"});
        if kind == "reply" { args["correlation"] = json!("remote-ask"); }
        let result = delegated_ok(&fleet, &messaging, "send", args);
        assert_eq!(result["queued"], true, "{result}");
        sent.push(result);
    }
    for (credential, correlation) in [(&messaging,"remote-approval"), (&full,"remote-sensitive"), (&full,"remote-privileged")] {
        assert_eq!(delegated_ok(&fleet, credential, "send", json!({"to":"harbor","kind":"reply","body":"yes","correlation":correlation}))["queued"], true);
    }
    stop(supervisor);

    // Real reverse collection survives the door restart, retains durable facts,
    // and emits only the three terminal failures to the captain's mailbox.
    remote(&["machine", "collect", "receiver"]);
    let origin = rusqlite::Connection::open(fleet.home("liaison").join("privateer.sqlite")).unwrap();
    let outcome = |id: &str| origin.query_row("SELECT outcome FROM delegated_outbox WHERE id=?1", [id], |r| r.get::<_, Option<String>>(0)).unwrap();
    let notice_count = |id: &str| origin.query_row("SELECT COUNT(*) FROM mail WHERE json_extract(payload,'$.delegated_delivery.id')=?1", [id], |r| r.get::<_, i64>(0)).unwrap();
    let audit_count = |id: &str| origin.query_row("SELECT COUNT(*) FROM audit WHERE kind='mail.delegated.outcome' AND json_extract(payload,'$.id')=?1", [id], |r| r.get::<_, i64>(0)).unwrap();
    for result in &sent {
        let id = result["mail"]["id"].as_str().unwrap();
        assert_eq!(outcome(id).as_deref(), Some("delivered"));
        let completed: Option<i64> = origin.query_row("SELECT completed_ms FROM delegated_outbox WHERE id=?1", [id], |r| r.get(0)).unwrap();
        assert!(completed.is_some());
        let payload: String = origin.query_row("SELECT payload FROM audit WHERE kind='mail.delegated.outcome' AND json_extract(payload,'$.id')=?1", [id], |r| r.get(0)).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&payload).unwrap(), json!({"id":id,"machine":"source-machine","outcome":"delivered"}));
        assert_eq!(audit_count(id), 1);
        assert_eq!(notice_count(id), 0, "successful forwarding must not create captain mail");
        let stored: String = receiver.query_row("SELECT payload FROM mail WHERE id=?1", [id], |r| r.get(0)).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&stored).unwrap()["provenance"], result["provenance"]);
        let summary = result["delivery_summary"].as_str().unwrap();
        for phrase in ["if it is refused", "Successful delivery is retained", "without a captain notification"] {
            assert!(summary.contains(phrase), "{summary}");
        }
        assert!(!summary.contains("collected or refused"), "{summary}");
    }
    let failures: Vec<(String, String)> = origin.prepare("SELECT id,outcome FROM delegated_outbox WHERE outcome!='delivered' ORDER BY id").unwrap()
        .query_map([], |r| Ok((r.get(0)?,r.get(1)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    assert_eq!(failures.len(), 3);
    for (id, error) in &failures {
        assert!(error.contains("insufficient_scope") || error.contains("trusted_confirmation_required"), "{error}");
        assert_eq!(notice_count(id), 1);
        let stored: String = origin.query_row("SELECT payload FROM mail WHERE json_extract(payload,'$.delegated_delivery.id')=?1", [id], |r| r.get(0)).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&stored).unwrap()["delegated_delivery"]["outcome"], *error);
    }
    remote(&["machine", "collect", "receiver"]);
    for result in &sent {
        let id = result["mail"]["id"].as_str().unwrap();
        assert_eq!(audit_count(id), 1, "completed replay does not duplicate outcome audit");
        assert_eq!(notice_count(id), 0);
    }
    // Lost completion acknowledgments may replay remote delivery, but cannot
    // duplicate the recipient's mail or create a successful captain receipt.
    origin.execute("UPDATE delegated_outbox SET outcome=NULL,completed_ms=NULL WHERE outcome='delivered'", []).unwrap();
    remote(&["machine", "collect", "receiver"]);
    let deliveries: i64 = receiver.query_row("SELECT COUNT(*) FROM mail WHERE body='Quiet delegated words'", [], |r| r.get(0)).unwrap();
    assert_eq!(deliveries, 3);
    for result in &sent { assert_eq!(notice_count(result["mail"]["id"].as_str().unwrap()), 0); }

    // Scan through actual captain feed and a real door adapter subprocess.
    // Material results, decisions and ordinary authored receipt-like content
    // remain visible; successful forwarding supplies no text or speech input.
    for (id, kind, body, payload) in [
        ("material-result", "notice", "The requested result is ready", json!({"result":{"ready":true}})),
        ("material-decision", "decision", "Choose the release window", json!({})),
        ("ordinary-receipt-words", "notice", "Delegated queued mail ordinary to harbor: delivered", json!({"delegated_delivery":{"id":"ordinary","outcome":"delivered"}})),
        ("outcome-scan-barrier", "notice", "Outcome scan complete", json!({})),
    ] {
        fleet.call("liaison", "mail.send", json!({"id":id,"from":"liaison","to":"captain","kind":kind,"body":body,"payload":payload}));
    }
    let feed = fleet.call("liaison", "captain.feed", json!({"limit":200}));
    assert_eq!(feed["more"], false, "{feed}");
    for result in &sent {
        assert!(!feed["entries"].as_array().unwrap().iter().any(|e| e["payload"]["delegated_delivery"]["id"]==result["mail"]["id"]), "{feed}");
    }
    fs::write(fleet.root.join("requests.jsonl"), "").unwrap();
    fs::write(&fleet.log, "").unwrap();
    // Scoped v2 never accepts unauthenticated pushes. Explicitly register
    // this scratch adapter as a trusted v1 captain door for the push phase.
    manifest_protocol(&fleet, 1);
    fleet.ok(&["door", "add", "fake", "--manifest",
        fleet.root.join("adapter/manifest.json").to_str().unwrap(), "--protocol", "1"]);
    let supervisor = fleet.supervise();
    fleet.wait("captain outcome scan", |entries| entries.iter().find(|e| e["type"]=="mail" && e["mail"]["id"]=="outcome-scan-barrier").cloned());
    let pushed = fleet.entries().into_iter().filter(|e| e["type"]=="mail").collect::<Vec<_>>();
    stop(supervisor);
    for result in &sent {
        assert!(!pushed.iter().any(|e| e["mail"]["payload"]["delegated_delivery"]["id"]==result["mail"]["id"]), "{pushed:?}");
    }
    for (id, _) in &failures {
        assert_eq!(pushed.iter().filter(|e| e["mail"]["payload"]["delegated_delivery"]["id"]==*id).count(), 1, "{pushed:?}");
    }
    for id in ["material-result", "material-decision", "ordinary-receipt-words"] {
        assert!(pushed.iter().any(|e| e["mail"]["id"]==id), "material and ordinary authored mail still reaches the captain: {id}");
    }

    let collected = |args: Value| {
        let mut child = fleet.command(&fleet.root).arg("gateway")
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().unwrap();
        writeln!(child.stdin.take().unwrap(), "{}", json!({"id":"quiet-outcome-test","command":"fleet.collected","args":args})).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["ok"], true, "{response}");
    };
    let ordinary = |id: &str| origin.execute("INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,delivery) VALUES(?1,'liaison','harbor','notice','Ordinary queued mail','{}','normal',1,'pending')", [id]).unwrap();
    let delivery = |id: &str| origin.query_row("SELECT delivery FROM mail WHERE id=?1", [id], |r| r.get::<_, String>(0)).unwrap();
    let notice_id = |id: &str| format!("delegated-result-{:x}", Sha256::digest(json!(id).to_string().as_bytes()));
    let id = sent[0]["mail"]["id"].as_str().unwrap();

    // A pre-atomic successful notice remains authoritative on a conflicting
    // replay; upgrade does not delete that durable captain history.
    origin.execute("INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms) VALUES(?1,'liaison','captain','notice',?2,?3,'normal',1)", rusqlite::params![notice_id(id),
        format!("Delegated queued mail {id} to harbor: delivered"),
        json!({"delegated_delivery":{"id":id,"outcome":"delivered","provenance":sent[0]["provenance"]}}).to_string()]).unwrap();
    origin.execute("UPDATE delegated_outbox SET outcome=NULL,completed_ms=NULL WHERE id=?1", [id]).unwrap();
    collected(json!({"machine":"source-machine","refused":[{"id":id,"error":"conflicting replay"}]}));
    assert_eq!(outcome(id).as_deref(), Some("delivered"));
    assert_eq!(notice_count(id), 1);
    origin.execute("DELETE FROM mail WHERE id=?1", [notice_id(id)]).unwrap();

    // Success completion and its audit commit atomically. A real gateway
    // retry after the injected crash recovers without any receipt mail.
    let before = audit_count(id);
    origin.execute("UPDATE delegated_outbox SET outcome=NULL,completed_ms=NULL WHERE id=?1", [id]).unwrap();
    origin.execute_batch("CREATE TRIGGER fail_quiet_audit BEFORE INSERT ON audit WHEN NEW.kind='mail.delegated.outcome' AND json_extract(NEW.payload,'$.id')='quiet-request' BEGIN SELECT RAISE(ABORT,'injected outcome audit failure'); END;").unwrap();
    ordinary("ordinary-audit-fault");
    let batch = json!({"machine":"source-machine","delivered":[{"id":id},{"id":"ordinary-audit-fault"}]});
    collected(batch.clone());
    assert!(outcome(id).is_none());
    assert_eq!(audit_count(id), before);
    assert_eq!(notice_count(id), 0);
    assert_eq!(delivery("ordinary-audit-fault"), "delivered");
    origin.execute_batch("DROP TRIGGER fail_quiet_audit;").unwrap();
    collected(batch);
    assert_eq!(outcome(id).as_deref(), Some("delivered"));
    assert_eq!(audit_count(id), before+1);
    assert_eq!(notice_count(id), 0);

    // An existing unrelated row at the deterministic notice ID is still a
    // collision, even when the incoming outcome is a quiet success.
    origin.execute("INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms) VALUES(?1,'liaison','captain','notice','Unrelated durable mail','{}','normal',1)", [notice_id(id)]).unwrap();
    origin.execute("UPDATE delegated_outbox SET outcome=NULL,completed_ms=NULL WHERE id=?1", [id]).unwrap();
    collected(json!({"machine":"source-machine","delivered":[{"id":id}]}));
    assert!(outcome(id).is_none());
    let collision: String = origin.query_row("SELECT last_error FROM delegated_outcome_retry WHERE id=?1", [id], |r| r.get(0)).unwrap();
    assert!(collision.contains("outcome notice id collision"), "{collision}");
    origin.execute("DELETE FROM mail WHERE id=?1", [notice_id(id)]).unwrap();
    collected(json!({"machine":"source-machine","delivered":[{"id":id}]}));
    assert_eq!(outcome(id).as_deref(), Some("delivered"));
    assert_eq!(notice_count(id), 0);
}
