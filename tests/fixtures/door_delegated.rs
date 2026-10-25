// All requests cross a real adapter subprocess and the supervisor admission
// boundary. The feeder never logs bearer envelopes; only responses are logged.
fn delegated_adapter(fleet: &Fleet) {
    manifest_protocol(fleet, 2);
    fs::write(
        fleet.root.join("adapter/door.py"),
        r#"import json, os, sys, time, threading
root = os.path.dirname(os.environ['FAKE_DOOR_LOG'])
log = open(os.environ['FAKE_DOOR_LOG'], 'a')
def note(m):
    log.write(json.dumps(m)+'\n'); log.flush()
note({'hello':json.loads(sys.stdin.readline())})
def requests():
    seen = 0
    while True:
        try:
            lines = open(os.path.join(root, 'requests.jsonl')).readlines()
            for line in lines[seen:]:
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

fn delegated_issue(
    fleet: &Fleet,
    name: &str,
    scopes: &str,
    ships: Option<&str>,
) -> (Value, String) {
    let mut args = vec![
        "door", "client", "issue", "fake", "--name", name, "--scopes", scopes,
    ];
    if let Some(ships) = ships {
        args.extend(["--ships", ships]);
    }
    client_secret(fleet, &args, "issue")
}

fn delegated_exchange(fleet: &Fleet, token: &str, command: &str, args: Value) -> Value {
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
        json!({"type":"request","id":id,"command":command,"args":args,"auth":{"token":token}})
    )
    .unwrap();
    fleet.wait("delegated response", |entries| {
        response(entries, &id).cloned()
    })
}

fn delegated_ok(fleet: &Fleet, token: &str, command: &str, args: Value) -> Value {
    let response = delegated_exchange(fleet, token, command, args);
    assert_eq!(response["ok"], true, "{command}: {response}");
    response["result"].clone()
}

const ALL_DELEGATED_SCOPES: &str = "fleet:read,mail:read,ship:message,work:manage,file:transfer,approval:answer,fleet:admin,captain:presence";

#[test]
fn delegated_mail_size_refusal_is_typed_audited_and_has_no_effects() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (_, token) = delegated_issue(&fleet, "size-test", "ship:message", Some("ops"));
    let supervisor = fleet.supervise();
    fleet.wait("scoped hello", |e| e.first().cloned());
    for body in ["é".repeat(32_768), "\u{0001}".repeat(12_000)] {
        let before = request_audit_counts(&fleet, Some("message_too_large"));
        let response = delegated_exchange(&fleet, &token, "send", json!({
            "to":"ops", "kind":"notice", "body":body, "idempotency_key":"size-retry"
        }));
        assert_eq!(response["ok"], false);
        assert_eq!(response["error"]["code"], "message_too_large");
        assert_eq!(request_audit_counts(&fleet, Some("message_too_large")), before + 1);
    }
    let sent = delegated_ok(&fleet, &token, "send", json!({
        "to":"ops", "kind":"notice", "body":"corrected", "idempotency_key":"size-retry"
    }));
    assert_eq!(sent["mail"]["body"], "corrected");
    let conn = rusqlite::Connection::open(fleet.home("ops").join("privateer.sqlite")).unwrap();
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM mail WHERE sender='captain'", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 1, "oversized sends must leave no mailbox rows");
    stop(supervisor);
}

#[test]
fn delegated_execution_scope_matrix_presence_and_core_provenance() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (all, full) = delegated_issue(&fleet, "full", ALL_DELEGATED_SCOPES, None);
    let (_, observer) = delegated_issue(&fleet, "observer", "fleet:read,mail:read", Some("ops"));
    let (_, operator) =
        delegated_issue(&fleet, "operator", "ship:message,work:manage", Some("ops"));
    let (_, presence) = delegated_issue(&fleet, "presence", "captain:presence", None);
    let (_, empty) = client_secret(
        &fleet,
        &["door", "client", "issue", "fake", "--name", "empty"],
        "issue",
    );
    let supervisor = fleet.supervise();
    fleet.wait("scoped hello", |e| e.first().cloned());
    for (command, args) in [
        ("ships", json!({})),
        ("status", json!({})),
        ("waiting", json!({})),
        ("held", json!({})),
        ("permissions", json!({})),
        ("stow", json!({"action":"status"})),
        ("away", json!({"action":"status"})),
    ] {
        credential_request(&fleet, &empty, command, args.clone(), "insufficient_scope");
        delegated_ok(&fleet, &observer, command, args);
    }
    credential_request(
        &fleet,
        &observer,
        "away",
        json!({"action":"start"}),
        "insufficient_scope",
    );
    credential_request(
        &fleet,
        &observer,
        "away",
        json!({"action":"back"}),
        "insufficient_scope",
    );
    credential_request(&fleet, &observer, "digest", json!({}), "insufficient_scope");
    credential_request(&fleet, &presence, "digest", json!({}), "insufficient_scope");
    delegated_ok(
        &fleet,
        &full,
        "away",
        json!({"action":"start","words":"out"}),
    );
    for kind in ["request", "notice"] {
        let args = json!({"to":"ops","body":"Scoped words","kind":kind});
        credential_request(
            &fleet,
            &observer,
            "send",
            args.clone(),
            "insufficient_scope",
        );
        let result = delegated_ok(&fleet, &full, "send", args);
        assert!(result["returned"].is_null());
        assert_eq!(
            result["mail"]["payload"]["provenance"],
            result["provenance"]
        );
        assert_eq!(result["provenance"]["client"], all["clients"][0]["id"]);
        assert_eq!(result["provenance"]["grant"], all["clients"][0]["id"]);
        assert!(result["provenance"]["result_id"].is_string());
    }
    credential_request(
        &fleet,
        &operator,
        "send",
        json!({"to":"missing","body":"lost"}),
        "ship_not_allowed",
    );
    credential_request(
        &fleet,
        &full,
        "send",
        json!({"to":"missing","body":"lost"}),
        "execution_not_started",
    );
    credential_request(
        &fleet,
        &full,
        "send",
        json!({"to":"ops","body":""}),
        "execution_not_started",
    );
    for (id, command) in [("reply-me", "send"), ("dismiss-me", "dismiss")] {
        fleet.call(
            "ops",
            "mail.send",
            json!({"id":id,"from":"ops","to":"captain","kind":"request","body":"Ordinary ask"}),
        );
        let args = if command == "send" {
            json!({"to":"ops","kind":"reply","correlation":id,"body":"Answer"})
        } else {
            json!({"id":id,"note":"closed"})
        };
        credential_request(
            &fleet,
            &observer,
            command,
            args.clone(),
            "insufficient_scope",
        );
        let result = delegated_ok(&fleet, &operator, command, args);
        let mail = &result["mail"];
        if command == "send" {
            assert!(mail["payload"]["operator"].is_null());
        } else {
            assert_eq!(
                mail["payload"]["dismissed_provenance"],
                result["provenance"]
            );
        }
    }
    assert!(delegated_ok(&fleet, &observer, "away", json!({}))["away"].is_object());
    fleet.call(
        "ops",
        "mail.send",
        json!({"from":"ops","to":"captain","kind":"notice","body":"Digest notice"}),
    );
    let first = delegated_ok(&fleet, &full, "digest", json!({}));
    assert!(first["digest"]["id"].is_string(), "{first}");
    assert!(first["digest"]["provenance"]["grant"].is_string());
    let retry = delegated_ok(&fleet, &full, "digest", json!({}));
    assert_eq!(first["digest"], retry["digest"]);
    delegated_ok(&fleet, &full, "away", json!({"action":"back"}));
    assert!(delegated_ok(&fleet, &observer, "away", json!({}))["away"].is_null());
    let audit: Value = serde_json::from_str(&fleet.ok(&["door", "client", "audit"])).unwrap();
    assert!(
        audit["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["event"] == "request.success"
                && r["client"] == all["clients"][0]["id"]
                && r["grant"] == r["client"]
                && r["result_id"].is_string())
    );
    let registry = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    let (entered, returned): (String, String) = registry.query_row(
        "SELECT entered_provenance,returned_provenance FROM away ORDER BY started_ms DESC LIMIT 1", [],
        |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    for recorded in [entered, returned] {
        let provenance: Value = serde_json::from_str(&recorded).unwrap();
        assert_eq!(provenance["client"], all["clients"][0]["id"]);
        assert_eq!(provenance["grant"], all["clients"][0]["id"]);
    }
    // Owner revoke is observed by the running supervisor's very next call.
    client_call(&fleet, "revoke", all["clients"][0]["id"].as_str().unwrap());
    credential_request(&fleet, &full, "ships", json!({}), "revoked_grant");
    delegated_ok(&fleet, &observer, "ships", json!({}));
    stop(supervisor);
}

#[test]
fn delegated_refused_variants_identity_and_filtered_targets() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (_, full) = delegated_issue(&fleet, "full", ALL_DELEGATED_SCOPES, None);
    let (_, restricted) = delegated_issue(&fleet, "restricted", ALL_DELEGATED_SCOPES, Some("ops"));
    let supervisor = fleet.supervise();
    fleet.wait("scoped hello", |e| e.first().cloned());
    for (command, args, reason) in [
        (
            "upgrade",
            json!({"commit":"abc"}),
            "execution_failed",
        ),
        (
            "upgrade",
            json!({"action":"confirm","confirmation":"token"}),
            "grant_bound_upgrade_unavailable",
        ),
        (
            "file.put",
            json!({"to":"ops","name":"x","content_file":"/not-opened"}),
            "execution_failed",
        ),
        (
            "approval",
            json!({"id":"x","action":"grant","hash":"12345678"}),
            "trusted_confirmation_required",
        ),
        (
            "approval",
            json!({"id":"x","action":"deny","hash":"12345678"}),
            "trusted_confirmation_required",
        ),
    ] {
        credential_request(&fleet, &full, command, args, reason);
    }
    for field in [
        "from",
        "captain",
        "operator",
        "grant",
        "liaison",
        "actor",
        "provenance",
        "delegated_context",
        "client_id",
        "grant_id",
        "approver",
    ] {
        let mut args = json!({"to":"ops","body":"Forged"});
        args[field] = json!("claimed");
        credential_request(&fleet, &full, "send", args.clone(), "reserved_identity");
        credential_request(
            &fleet,
            &full,
            "send",
            json!({"body":"nested","payload":{"x":[args.clone()]}}),
            "reserved_identity",
        );
    }
    for ship in ["ops", "liaison"] {
        fleet.call(ship, "mail.send", json!({"id":format!("ask-{ship}"),"from":ship,"to":"captain","kind":"request","body":format!("secret-{ship}")}));
    }
    delegated_ok(&fleet, &full, "away", json!({"action":"start"}));
    for ship in ["ops", "liaison"] {
        fleet.call(
            ship,
            "mail.send",
            json!({"from":ship,"to":"captain","kind":"notice","body":format!("held-{ship}")}),
        );
    }
    for command in ["ships", "status", "waiting", "held"] {
        let result = delegated_ok(&fleet, &restricted, command, json!({}));
        let text = result.to_string();
        assert!(
            !text.contains("secret-liaison") && !text.contains("held-liaison"),
            "{result}"
        );
        let key = if ["ships", "status"].contains(&command) {
            "ships"
        } else {
            command
        };
        for row in result[key].as_array().unwrap() {
            let name = row["name"].as_str().or(row["ship"].as_str()).unwrap();
            assert_eq!(name, "ops");
        }
    }
    credential_request(
        &fleet,
        &restricted,
        "send",
        json!({"to":"liaison","body":"no"}),
        "ship_not_allowed",
    );
    credential_request(
        &fleet,
        &restricted,
        "send",
        json!({"to":"liaison","kind":"reply","correlation":"ask-liaison","body":"no"}),
        "ship_not_allowed",
    );
    credential_request(
        &fleet,
        &restricted,
        "dismiss",
        json!({"id":"ask-liaison"}),
        "execution_failed",
    );
    let digest = delegated_ok(&fleet, &restricted, "digest", json!({}));
    assert!(!digest.to_string().contains("held-liaison"));
    // Source-path re-registration revokes, while unchanged source preserves.
    let same = fleet.ok(&[
        "door",
        "add",
        "fake",
        "--manifest",
        fleet.root.join("adapter/manifest.json").to_str().unwrap(),
    ]);
    assert!(same.contains("carry over"));
    delegated_ok(&fleet, &restricted, "ships", json!({}));
    let other = fleet.root.join("other.json");
    fs::copy(fleet.root.join("adapter/manifest.json"), &other).unwrap();
    let notice = fleet.ok(&["door", "add", "fake", "--manifest", other.to_str().unwrap()]);
    assert!(notice.contains("existing client grants revoked"));
    credential_request(&fleet, &restricted, "ships", json!({}), "revoked_grant");
    stop(supervisor);
}

#[test]
fn delegated_replies_resolve_stored_asks_and_cannot_launder_approval() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (_, operator) = delegated_issue(&fleet, "operator", "ship:message,work:manage", None);
    let (_, full) = delegated_issue(&fleet, "full", ALL_DELEGATED_SCOPES, None);
    fleet.call("ops", "mail.send", json!({"id":"approve-step","from":"ops","to":"captain","kind":"request","task_id":"ops-duty","body":"Please approve",
        "payload":{"approves":{"action":"Submit build 55","ttl_secs":60}}}));
    fleet.call("ops", "mail.send", json!({"id":"production-ask","from":"ops","to":"captain","kind":"request","task_id":"ops-duty","body":"Ship it?",
        "request_kind":"production_approval","payload":{"artifact":"sha256:exact","environment":"production"}}));
    let privileged = fleet.call("ops", "privileged.stage", json!({"from":"ops","task_id":"ops-duty","program":"/usr/bin/id","args":["-u"],"cwd":"/","reason":"Check user"}));
    let supervisor = fleet.supervise();
    fleet.wait("scoped hello", |e| e.first().cloned());
    delegated_ok(&fleet, &full, "away", json!({"action":"start"}));
    credential_request(
        &fleet,
        &operator,
        "send",
        json!({"to":"ops","kind":"reply","correlation":"approve-step","body":"yes"}),
        "insufficient_scope",
    );
    assert_eq!(
        fleet.call("ops", "mail.show", json!({"id":"approve-step"}))["state"],
        "open"
    );
    for token in [&operator, &full] {
        for id in ["production-ask", privileged["id"].as_str().unwrap()] {
            credential_request(
                &fleet,
                token,
                "send",
                json!({"to":"ops","kind":"reply","correlation":id,"body":"yes"}),
                "trusted_confirmation_required",
            );
            assert_eq!(
                fleet.call("ops", "mail.show", json!({"id":id}))["state"],
                "open"
            );
        }
    }
    let conn = rusqlite::Connection::open(fleet.home("ops").join("privateer.sqlite")).unwrap();
    let approvals: i64 = conn
        .query_row("SELECT COUNT(*) FROM captain_approvals", [], |r| r.get(0))
        .unwrap();
    assert_eq!(approvals, 0);
    let markers: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM mail WHERE json_extract(payload,'$.operator')=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(markers, 0);
    let reply = delegated_ok(
        &fleet,
        &full,
        "send",
        json!({"to":"ops","kind":"reply","correlation":"approve-step","body":"yes"}),
    );
    assert_eq!(reply["mail"]["payload"]["operator"], true);
    let approvals: i64 = conn
        .query_row("SELECT COUNT(*) FROM captain_approvals", [], |r| r.get(0))
        .unwrap();
    assert_eq!(approvals, 1);
    let recorded: String = conn.query_row("SELECT payload FROM audit WHERE kind='permissions.approval_recorded' ORDER BY seq DESC LIMIT 1", [], |r| r.get(0)).unwrap();
    let recorded: Value = serde_json::from_str(&recorded).unwrap();
    assert_eq!(recorded["provenance"], reply["provenance"]);
    // A scoped message to the liaison is not a fresh captain authority source.
    let source = delegated_ok(
        &fleet,
        &operator,
        "send",
        json!({"to":"liaison","body":"relay this answer"}),
    );
    let output = fleet
        .command(&fleet.root)
        .args([
            "--home",
            fleet.home("liaison").to_str().unwrap(),
            "--role",
            "agent",
            "--actor",
            "liaison",
            "captain.relay",
            "--json",
            &json!({"source":source["mail"]["id"],"request":"production-ask","body":"yes"})
                .to_string(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be relayed"));
    assert!(delegated_ok(&fleet, &full, "away", json!({}))["away"].is_object());
    stop(supervisor);
}
