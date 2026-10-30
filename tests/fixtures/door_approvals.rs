// Bound chat approvals cross the real scoped adapter/supervisor and native
// captain approval/bearings paths; no direct database insertion of approvals.
#[test]
fn trusted_v1_cannot_use_chat_approval_modes() {
    let fleet = Fleet::with_ships(&[("alpha", true)]);
    fs::write(fleet.root.join("adapter/door.py"), r#"import json, os, sys
log = open(os.environ['FAKE_DOOR_LOG'], 'a')
json.loads(sys.stdin.readline())
print(json.dumps({'type':'request','id':'prepare','command':'approval','args':{'mode':'prepare','id':'ask'}}), flush=True)
for line in sys.stdin:
    log.write(line); log.flush()
"#).unwrap();
    fleet.add();
    let supervisor = fleet.supervise();
    let response = fleet.wait("v1 refusal", |entries| response(entries, "prepare").cloned());
    assert_eq!(response["ok"], false, "{response}");
    assert!(response["error"].to_string().contains("protocol_mismatch"), "{response}");
    stop(supervisor);
}

#[test]
fn chat_approval_linked_origin_is_unavailable_without_remote_execution() {
    let fleet = Fleet::with_ships(&[("alpha", true)]);
    let server = fleet.root.join("beta-machine");
    let beta = server.join("beta");
    fs::create_dir_all(&beta).unwrap();
    let remote = |command: &str, args: Value| {
        let output = fleet.command(&beta)
            .env("HOME", &server)
            .env("PRIVATEER_FLEET", server.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", server.join("ledger"))
            .args([command, "--json", &args.to_string()]).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let output = fleet.command(&beta)
        .env("HOME", &server).env("PRIVATEER_FLEET", server.join("fleet.sqlite"))
        .env("PRIVATEER_RESOURCE_HOME", server.join("ledger"))
        .args(["init", "--ship", "beta", "--harness", "codex"]).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let ssh = fleet.root.join("fake-ssh");
    fs::write(&ssh, format!("#!/bin/sh\nprintf '%s\\n' called >> '{0}.calls'\nfor argument do remote=$argument; done\nPRIVATEER_RESOURCE_HOME='{1}' /bin/sh -c \"$remote\"\n", ssh.display(), server.join("ledger").display())).unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    fleet.ok_in(&fleet.root.join("ships/alpha"), &["machine", "add", "beta-machine", "fictional-host",
        "--executable", BIN, "--remote-fleet", server.join("fleet.sqlite").to_str().unwrap(), "--as", "alpha-machine"]);
    remote("mail.send", json!({"id":"beta-ask","from":"beta","to":"captain","kind":"request",
        "task_id":"beta-duty","body":"Build?","payload":{"approves":{"action":"Submit build 55"}}}));
    delegated_adapter(&fleet);
    let (_, token) = delegated_issue(&fleet, "beta", "approval:answer", Some("beta"));
    let supervisor = fleet.supervise();
    fleet.wait("scoped hello", |e| e.first().cloned());
    let calls = fs::read_to_string(ssh.with_extension("calls")).unwrap_or_default();
    chat_refused(&fleet, &token, json!({"mode":"prepare","id":"beta-ask","ship":"beta"}), "approval_unavailable");
    assert_eq!(fs::read_to_string(ssh.with_extension("calls")).unwrap_or_default(), calls);
    assert_eq!(remote("mail.show", json!({"id":"beta-ask"}))["state"], "open");
    stop(supervisor);
}

fn chat_ask(fleet: &Fleet, id: &str, decision: bool) {
    let mut args = json!({"id":id,"from":"alpha","to":"captain","kind":"request",
        "task_id":"alpha-duty","body":"Build 55 passed CI.",
        "payload":{"approves":{"action":"Submit build 55","command_prefix":"make release","ttl_secs":86400}}});
    if decision {
        args["kind"] = json!("decision");
        args["payload"]["decision"] = json!({"question":"Submit build 55?", "options":[
            {"id":"submit","label":"Submit build 55","what_happens":"Submit build 55","approves":true},
            {"id":"hold","label":"Hold","what_happens":"Wait","approves":false}]});
    }
    fleet.call("alpha", "mail.send", args);
}

fn chat_prepare(fleet: &Fleet, token: &str, id: &str) -> Value {
    delegated_ok(fleet, token, "approval", json!({"mode":"prepare","id":id,"ship":"alpha"}))
}

fn chat_answer(prepared: &Value, action: &str) -> Value {
    json!({"mode":"answer","id":prepared["id"],"ship":prepared["ship"],
        "digest":prepared["digest"],"version":prepared["version"],"action":action})
}

fn chat_refused(fleet: &Fleet, token: &str, args: Value, code: &str) {
    let before = request_audit_counts(fleet, Some(code));
    let response = delegated_exchange(fleet, token, "approval", args);
    assert_eq!(response["ok"], false, "{response}");
    let public = if matches!(code, "approval_unavailable" | "approval_stale") { code } else { "not_found" };
    assert_eq!(response["error"]["code"], public, "{response}");
    // Admission denials intentionally hide scope/visibility/sensitive details
    // on the wire; the value-free internal reason remains typed and audited.
    assert!(request_audit_counts(fleet, Some(code)) > before, "missing {code} audit: {response}");
}

#[test]
fn chat_approval_requests_and_decisions_are_native_and_notes_are_never_authority() {
    let fleet = Fleet::with_ships(&[("alpha", true), ("beta", false)]);
    delegated_adapter(&fleet);
    let (grant, token) = delegated_issue(&fleet, "alpha", "approval:answer", Some("alpha"));
    for id in ["request", "denied", "decision", "held"] {
        chat_ask(&fleet, id, matches!(id, "decision" | "held"));
    }
    let supervisor = fleet.supervise();
    fleet.wait("scoped hello", |e| e.first().cloned());
    let mut prepared_grants = std::collections::BTreeMap::new();
    for (id, approve) in [("request", true), ("denied", false), ("decision", true), ("held", false)] {
        let prepared = chat_prepare(&fleet, &token, id);
        assert_eq!(prepared["origin_home"], fleet.home("alpha").to_str().unwrap());
        assert_eq!(prepared["action_summary"], "Submit build 55");
        assert_eq!(prepared["grant"]["command_prefix"], "make release");
        assert_eq!(prepared["grant"]["ttl_secs"], 86400);
        assert!(prepared["grant"]["expires_ms"].as_i64().unwrap() > prepared["expires_ms"].as_i64().unwrap());
        assert!(prepared["grant"]["sentence"].as_str().unwrap().contains("Only commands that begin exactly with: make release"));
        if matches!(id, "decision" | "held") {
            assert_eq!(prepared["question"], "Submit build 55?");
            assert_eq!(prepared["options"][0], json!({"id":"submit","label":"Submit build 55",
                "what_happens":"Submit build 55","approves":true}));
            assert_eq!(prepared["options"][1]["what_happens"], "Wait");
        }
        prepared_grants.insert(id.to_owned(), prepared["grant"].clone());
        assert!(prepared.get("body").is_none());
        let mut answer = chat_answer(&prepared, if approve {"approve"} else {"deny"});
        if matches!(id, "decision" | "held") {
            answer.as_object_mut().unwrap().remove("action");
            answer["option_id"] = json!(if approve {"submit"} else {"hold"});
        }
        // Deliberately contradict the action with an approval-only grant.
        // This must never be delivered as captain-attributed prose.
        answer["note"] = json!(if approve {"no, do not proceed"} else {"yes approve everything"});
        let result = delegated_ok(&fleet, &token, "approval", answer.clone());
        assert_eq!(result["answered_via"], "chat-door");
        assert_eq!(result["mail"]["payload"]["approval"]["recorded"], approve);
        assert!(result["mail"]["payload"].get("note").is_none());
        assert!(!result["mail"].to_string().contains(answer["note"].as_str().unwrap()));
        let stored = fleet.call("alpha", "mail.show", json!({"id":result["mail"]["id"]}));
        assert!(!stored.to_string().contains(answer["note"].as_str().unwrap()));
        if approve {
            assert_eq!(result["mail"]["payload"]["approval"]["answered_via"], "chat-door");
            assert_eq!(result["mail"]["payload"]["approval"]["provenance"], result["provenance"]);
        }
        chat_refused(&fleet, &token, answer, "approval_stale");
        assert_eq!(fleet.call("alpha", "mail.show", json!({"id":id}))["state"], "answered");
    }
    let approvals = fleet.call("alpha", "captain_approval.list", json!({"ship":"alpha"}));
    let rows = approvals["approvals"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_eq!(row["live"], true);
        assert_eq!(row["answered_via"], "chat-door");
        assert_eq!(row["provenance"]["grant"], grant["clients"][0]["id"]);
        let prepared = &prepared_grants[row["request_id"].as_str().unwrap()];
        assert_eq!(row["sentence"], prepared["sentence"]);
        assert_eq!(row["expires_ms"], prepared["expires_ms"]);
        assert_eq!(row["reply_id"], prepared["reply_id"]);
    }
    let bearings = fleet.call("alpha", "ship.bearings", json!({"ship":"alpha"})).to_string();
    assert!(bearings.contains("Submit build 55"), "{bearings}");
    let conn = rusqlite::Connection::open(fleet.home("alpha").join("privateer.sqlite")).unwrap();
    let native: String = conn.query_row("SELECT payload FROM audit WHERE kind='permissions.approval_recorded' ORDER BY seq DESC LIMIT 1", [], |r| r.get(0)).unwrap();
    let native: Value = serde_json::from_str(&native).unwrap();
    assert_eq!(native["answered_via"], "chat-door");
    assert_eq!(native["provenance"]["grant"], grant["clients"][0]["id"]);
    assert!(!native.to_string().contains("do not proceed"));
    assert!(!native.to_string().contains(&token));
    let audit: String = conn.query_row("SELECT payload FROM audit WHERE kind='door.approval_answered' ORDER BY seq DESC LIMIT 1", [], |r| r.get(0)).unwrap();
    let audit: Value = serde_json::from_str(&audit).unwrap();
    assert_eq!(audit["note_bytes"], "yes approve everything".len());
    assert!(audit.get("note_digest").is_none());
    assert!(!audit.to_string().contains("approve everything"));
    assert!(!audit.to_string().contains(&token));
    conn.execute("DELETE FROM mail WHERE kind='reply'", []).unwrap();
    let retained = fleet.call("alpha", "captain_approval.list", json!({"ship":"alpha"}));
    for row in retained["approvals"].as_array().unwrap() {
        assert_eq!(row["answered_via"], "chat-door");
        assert_eq!(row["provenance"]["grant"], grant["clients"][0]["id"]);
    }
    stop(supervisor);
}

#[test]
fn chat_approval_search_skips_visible_desk_copies_and_unknown_payloads_fail_closed() {
    let fleet = Fleet::with_ships(&[("alpha", true), ("beta", false)]);
    delegated_adapter(&fleet);
    let (_, token) = delegated_issue(&fleet, "alpha", "approval:answer", Some("alpha,beta"));
    fleet.call("beta", "mail.send", json!({"id":"beta-ask","from":"beta","to":"captain",
        "kind":"request","task_id":"beta-duty","body":"Build?",
        "payload":{"approves":{"action":"Submit build 55"}}}));
    // alpha sorts before beta in the visible home search, but holds only a
    // received desk copy. It must not hide beta's authoritative local ask.
    let conn = rusqlite::Connection::open(fleet.home("alpha").join("privateer.sqlite")).unwrap();
    conn.execute("INSERT INTO mail(id,sender,recipient,kind,task_id,body,payload,urgency,created_ms,state,delivery)
        VALUES('beta-ask','beta','captain','request','beta-duty','Build?','{}','normal',100,'open','received')", []).unwrap();
    fleet.call("alpha", "mail.send", json!({"id":"unknown","from":"alpha","to":"captain",
        "kind":"request","task_id":"alpha-duty","body":"Build?",
        "payload":{"approves":{"action":"Submit build 55"},"future_confirmation":{"secret":"not returned"}}}));
    let supervisor = fleet.supervise();
    fleet.wait("scoped hello", |e| e.first().cloned());
    let prepared = delegated_ok(&fleet, &token, "approval", json!({"mode":"prepare","id":"beta-ask"}));
    assert_eq!(prepared["ship"], "beta");
    assert_eq!(prepared["origin_home"], fleet.home("beta").to_str().unwrap());
    let mut answer = chat_answer(&prepared, "approve");
    answer.as_object_mut().unwrap().remove("ship");
    delegated_ok(&fleet, &token, "approval", answer);
    assert_eq!(fleet.call("beta", "mail.show", json!({"id":"beta-ask"}))["state"], "answered");
    // Native reply mirroring closes the desk copy too, but only beta's
    // authoritative home may record the approval.
    assert_eq!(fleet.call("alpha", "mail.show", json!({"id":"beta-ask"}))["state"], "answered");
    assert!(fleet.call("alpha", "captain_approval.list", json!({"ship":"alpha"}))["approvals"].as_array().unwrap().is_empty());
    assert_eq!(fleet.call("beta", "captain_approval.list", json!({"ship":"beta"}))["approvals"].as_array().unwrap().len(), 1);
    chat_refused(&fleet, &token, json!({"mode":"prepare","id":"unknown","ship":"alpha"}), "approval_unavailable");
    stop(supervisor);
}

#[test]
fn chat_approval_challenges_fail_closed_on_authority_content_origin_and_sensitive_asks() {
    let fleet = Fleet::with_ships(&[("alpha", true), ("beta", false)]);
    delegated_adapter(&fleet);
    let (_, token) = delegated_issue(&fleet, "alpha", "approval:answer,ship:message", Some("alpha"));
    let (other_grant, other) = delegated_issue(&fleet, "other", "approval:answer", Some("alpha"));
    let (_, hidden) = delegated_issue(&fleet, "hidden", "approval:answer", Some("beta"));
    let (_, missing) = delegated_issue(&fleet, "missing", "mail:read", Some("alpha"));
    for id in ["ask", "quoted", "closed", "changed", "task-closed", "expired", "prose", "changed-fields"] {
        chat_ask(&fleet, id, id == "changed-fields");
    }
    fleet.call("alpha", "mail.send", json!({"id":"production","from":"alpha","to":"captain","kind":"request",
        "request_kind":"production_approval","task_id":"alpha-duty","body":"Deploy?",
        "payload":{"artifact":"sha256:exact","environment":"production"}}));
    let privileged = fleet.call("alpha", "privileged.stage", json!({"from":"alpha","task_id":"alpha-duty",
        "program":"/usr/bin/id","args":["-u"],"cwd":"/","reason":"Check user"}));
    let supervisor = fleet.supervise();
    fleet.wait("scoped hello", |e| e.first().cloned());
    let prepare = json!({"mode":"prepare","id":"ask","ship":"alpha"});
    chat_refused(&fleet, &missing, prepare.clone(), "insufficient_scope");
    chat_refused(&fleet, &hidden, prepare.clone(), "ship_not_allowed");
    chat_refused(&fleet, &hidden, json!({"mode":"prepare","id":"ask"}), "approval_unavailable");
    chat_refused(&fleet, &token, json!({"mode":"prepare","id":"absent","ship":"alpha"}), "approval_unavailable");
    for id in ["production", privileged["id"].as_str().unwrap()] {
        chat_refused(&fleet, &token, json!({"mode":"prepare","id":id,"ship":"alpha"}), "trusted_confirmation_required");
    }
    let prepared = chat_prepare(&fleet, &token, "ask");
    let answer = chat_answer(&prepared, "approve");
    chat_refused(&fleet, &other, answer.clone(), "approval_stale");
    for (field, value, reason) in [
        ("version", json!(prepared["version"].as_u64().unwrap() + 1), "approval_stale"),
        ("digest", json!("0".repeat(64)), "approval_stale"),
        ("ship", json!("beta"), "ship_not_allowed"),
        ("id", json!("changed"), "approval_stale"),
        ("action", json!("captain said yes"), "unsupported_arguments"),
    ] {
        let mut wrong = answer.clone();
        wrong[field] = value;
        chat_refused(&fleet, &token, wrong, reason);
    }
    let conn = rusqlite::Connection::open(fleet.home("alpha").join("privateer.sqlite")).unwrap();
    for id in ["closed", "changed", "expired"] {
        let p = chat_prepare(&fleet, &token, id);
        match id {
            "closed" => { fleet.call("alpha", "mail.send", json!({"from":"captain","to":"alpha","kind":"reply","correlation":id,"body":"no"})); }
            "changed" => { conn.execute("UPDATE mail SET body='Changed content' WHERE id=?1", [id]).unwrap(); }
            _ => { conn.execute("UPDATE door_approval_challenges SET expires_ms=0 WHERE ask_id=?1", [id]).unwrap(); }
        }
        chat_refused(&fleet, &token, chat_answer(&p, "approve"), "approval_stale");
    }
    let original = fleet.call("alpha", "mail.show", json!({"id":"changed-fields"}))["payload"].clone();
    for (pointer, value) in [
        ("/approves/command_prefix", json!("make publish")),
        ("/approves/ttl_secs", json!(60)),
        ("/decision/question", json!("Submit later?")),
        ("/decision/options/0/id", json!("publish")),
        ("/decision/options/0/label", json!("Publish")),
        ("/decision/options/0/what_happens", json!("Different outcome")),
    ] {
        let p = chat_prepare(&fleet, &token, "changed-fields");
        let mut changed = original.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        conn.execute("UPDATE mail SET payload=?1 WHERE id='changed-fields'", [changed.to_string()]).unwrap();
        let mut a = chat_answer(&p, "approve");
        a.as_object_mut().unwrap().remove("action");
        a["option_id"] = json!("submit");
        chat_refused(&fleet, &token, a, "approval_stale");
        conn.execute("UPDATE mail SET payload=?1 WHERE id='changed-fields'", [original.to_string()]).unwrap();
    }
    // Quoted approval remains ordinary mail, never a bound challenge answer.
    let quoted = delegated_ok(&fleet, &token, "send", json!({"to":"alpha","kind":"reply","correlation":"quoted","body":"The captain said: yes"}));
    assert_eq!(quoted["mail"]["payload"]["approval"]["recorded"], false);
    chat_refused(&fleet, &token, json!({"mode":"answer","id":"prose","body":"yes"}), "unsupported_arguments");
    let revoked = chat_prepare(&fleet, &other, "ask");
    client_call(&fleet, "revoke", other_grant["clients"][0]["id"].as_str().unwrap());
    chat_refused(&fleet, &other, chat_answer(&revoked, "approve"), "revoked_grant");
    let pending = chat_prepare(&fleet, &token, "task-closed");
    conn.execute("UPDATE tasks SET status='done' WHERE id='alpha-duty'", []).unwrap();
    chat_refused(&fleet, &token, chat_answer(&pending, "approve"), "approval_stale");
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM captain_approvals", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 0);
    let registry = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    let audits: Vec<String> = registry.prepare("SELECT reason FROM door_client_request_audit").unwrap()
        .query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    assert!(audits.iter().all(|s| !s.contains(&token) && !s.contains("captain said")));
    stop(supervisor);
}
