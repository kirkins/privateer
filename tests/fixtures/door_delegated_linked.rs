#[test]
fn delegated_linked_gateway_preserves_provenance_and_never_falls_back() {
    let fleet = Fleet::new();
    let server = fleet.root.join("remote-fleet");
    let harbor = server.join("harbor");
    fs::create_dir_all(&harbor).unwrap();
    let mut init = fleet.command(&harbor);
    let output = init
        .env("HOME", &server)
        .env("PRIVATEER_FLEET", server.join("fleet.sqlite"))
        .env("PRIVATEER_RESOURCE_HOME", server.join("ledger"))
        .args(["init", "--ship", "harbor", "--harness", "codex"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ssh = fleet.root.join("fake-ssh");
    fs::write(
        &ssh,
        format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{0}.calls'\nfor argument do remote=$argument; done\nIFS= read -r request\nprintf '%s\\n' \"$request\" >> '{0}.requests'\nprintf '%s\\n' \"$request\" | PRIVATEER_RESOURCE_HOME='{1}' /bin/sh -c \"$remote\"\n", ssh.display(), server.join("ledger").display()),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    fleet.ok_in(
        &fleet.root.join("ships/liaison"),
        &[
            "machine",
            "add",
            "remote",
            "test-host",
            "--executable",
            BIN,
            "--remote-fleet",
            server.join("fleet.sqlite").to_str().unwrap(),
            "--as",
            "local-desk",
        ],
    );
    delegated_adapter(&fleet);
    let (_, full) = delegated_issue(&fleet, "full", ALL_DELEGATED_SCOPES, None);
    let (_, local) = delegated_issue(&fleet, "local", "fleet:read", Some("ops"));
    let supervisor = fleet.supervise();
    fleet.wait("scoped hello", |e| e.first().cloned());
    let ships = delegated_ok(&fleet, &full, "ships", json!({}));
    assert!(
        ships["ships"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "harbor")
    );
    let ships = delegated_ok(&fleet, &local, "ships", json!({}));
    assert!(!ships.to_string().contains("harbor"));
    for (command, args) in [
        ("permissions", json!({})),
        ("stow", json!({"action":"status"})),
    ] {
        let result = delegated_ok(&fleet, &full, command, args);
        assert!(!result.to_string().contains("remote"), "{result}");
    }
    delegated_ok(&fleet, &full, "stow", json!({"action":"status","machine":"remote"}));
    credential_request(&fleet, &full, "stow", json!({"action":"status","all":true}), "unsupported_arguments");
    let calls = fs::read_to_string(ssh.with_extension("calls")).unwrap_or_default();
    assert!(!calls.contains("permissions.status"), "{calls}");
    assert!(!calls.contains("stow.status"), "{calls}");
    let remote = |command: &str, args: Value| {
        let output = fleet.command(&harbor)
            .env("HOME", &server)
            .env("PRIVATEER_FLEET", server.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", server.join("ledger"))
            .args([command, "--json", &args.to_string()]).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    };
    remote("mail.send", json!({"id":"remote-ask","from":"harbor","to":"captain","kind":"request","body":"Question"}));
    remote("mail.send", json!({"id":"remote-approval","from":"harbor","to":"captain","kind":"request","task_id":"harbor-duty","body":"Decision", "payload":{"approves":{"action":"Submit build 55","ttl_secs":60}}}));
    let (_, messaging) = delegated_issue(&fleet, "messaging", "ship:message", Some("harbor"));
    for kind in ["request", "notice", "reply"] {
        let mut args = json!({"to":"harbor","kind":kind,"body":"Delegated linked words"});
        if kind == "reply" { args["correlation"] = json!("remote-ask"); }
        let sent = delegated_ok(&fleet, &messaging, "send", args);
        assert_eq!(sent["mail"]["payload"]["provenance"], sent["provenance"]);
        assert!(sent["mail"]["payload"]["operator"].is_null());
        let local = rusqlite::Connection::open(fleet.home("liaison").join("privateer.sqlite")).unwrap();
        let audit: String = local.query_row("SELECT payload FROM audit WHERE kind='door.send' AND json_extract(payload,'$.mail')=?1", [sent["mail"]["id"].as_str().unwrap()], |r| r.get(0)).unwrap();
        let audit: Value = serde_json::from_str(&audit).unwrap();
        assert_eq!(audit["provenance"], sent["provenance"]);
        assert_eq!(audit["to"], "harbor");
        assert_eq!(audit["kind"], kind);
    }
    credential_request(&fleet, &messaging, "send", json!({"to":"harbor","kind":"reply","correlation":"remote-approval","body":"yes"}), "insufficient_scope");
    let conn = rusqlite::Connection::open(harbor.join(".privateer/privateer.sqlite")).unwrap();
    let state: String = conn.query_row("SELECT state FROM mail WHERE id='remote-approval'", [], |r| r.get(0)).unwrap();
    assert_eq!(state, "open");
    remote("mail.send", json!({"id":"remote-sensitive","from":"harbor","to":"captain","kind":"request","body":"Sensitive"}));
    conn.execute("UPDATE mail SET request_kind='production_approval' WHERE id='remote-sensitive'", []).unwrap();
    credential_request(&fleet, &full, "send", json!({"to":"harbor","kind":"reply","correlation":"remote-sensitive","body":"yes"}), "trusted_confirmation_required");
    let state: String = conn.query_row("SELECT state FROM mail WHERE id='remote-sensitive'", [], |r| r.get(0)).unwrap();
    assert_eq!(state, "open");

    // Admin-only (no implicit read/message scope) mutations retain receiver
    // provenance and use the dedicated gateway for each machine.
    let (_, admin) = delegated_issue(&fleet, "admin", "fleet:admin", None);
    credential_request(&fleet, &messaging, "permissions", json!({"mode":"yolo"}), "insufficient_scope");
    credential_request(&fleet, &messaging, "stow", json!({"action":"start","machine":"remote"}), "insufficient_scope");
    for mode in ["yolo", "standard"] {
        let result = delegated_ok(&fleet, &admin, "permissions", json!({"mode":mode}));
        assert_eq!(result["partial"], false, "{result}");
        assert_eq!(result["machines"].as_array().unwrap().len(), 2);
        for row in result["machines"].as_array().unwrap() {
            assert_eq!(row["result"]["mode"], mode);
            assert_eq!(row["result"]["provenance"], result["provenance"]);
        }
    }
    let local_stow = delegated_ok(&fleet, &admin, "stow", json!({"action":"start"}));
    let remote_stow = delegated_ok(&fleet, &admin, "stow", json!({"action":"start","machine":"remote"}));
    assert_eq!(local_stow["held"], true);
    assert_eq!(remote_stow["held"], true);
    assert_ne!(local_stow["stow"], remote_stow["stow"]);
    let payload: String = conn.query_row("SELECT payload FROM mail WHERE id=?1", [format!("{}-harbor", remote_stow["stow"].as_str().unwrap())], |r| r.get(0)).unwrap();
    let payload: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(payload["provenance"], remote_stow["provenance"]);
    let resumed = delegated_ok(&fleet, &admin, "stow", json!({"action":"resume","all":true}));
    assert_eq!(resumed["partial"], false, "{resumed}");
    for row in resumed["machines"].as_array().unwrap() {
        assert_eq!(row["result"]["held"], false);
    }

    let plan = delegated_ok(&fleet, &admin, "upgrade", json!({"commit":"b".repeat(40)}));
    let registry = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    registry.execute("UPDATE machines SET peer=json_set(peer,'$.fixture_revision',1) WHERE name='remote'", []).unwrap();
    let confirm = json!({"action":"confirm","confirmation":plan["confirmation"]});
    credential_request(&fleet, &admin, "upgrade", confirm.clone(), "execution_failed");
    credential_request(&fleet, &admin, "upgrade", confirm, "grant_bound_upgrade_unavailable");
    registry.execute("UPDATE machines SET peer=json_remove(peer,'$.fixture_revision') WHERE name='remote'", []).unwrap();

    // Upload uses the same descriptor through the dedicated gateway.
    let state_dir = fleet.root.join("state/doors/fake/state");
    fs::create_dir_all(&state_dir).unwrap();
    let source = state_dir.join("upload.txt");
    fs::write(&source, "descriptor bytes").unwrap();
    let uploaded = delegated_ok(&fleet, &full, "file.put", json!({"to":"harbor","name":"upload.txt","content_file":source}));
    assert_eq!(fs::read_to_string(uploaded["path"].as_str().unwrap()).unwrap(), "descriptor bytes");
    let audit: String = conn.query_row("SELECT payload FROM audit WHERE kind='file.commit' ORDER BY seq DESC LIMIT 1", [], |r| r.get(0)).unwrap();
    let audit: Value = serde_json::from_str(&audit).unwrap();
    assert_eq!(audit["provenance"], uploaded["provenance"]);

    // A receiver audit failure after commit crosses the gateway as a typed
    // outcome_unknown, not execution_failed or a gateway refusal.
    conn.execute_batch("CREATE TRIGGER fail_delegated_commit_audit BEFORE INSERT ON audit WHEN NEW.kind='file.delegated' AND json_extract(NEW.payload,'$.command')='file.commit' BEGIN SELECT RAISE(FAIL, 'test audit failure'); END;").unwrap();
    credential_request(&fleet, &full, "file.put", json!({"to":"harbor","name":"audit-unknown.txt","content_file":source}), "outcome_unknown");
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM files WHERE name='audit-unknown.txt'", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 1);
    conn.execute_batch("DROP TRIGGER fail_delegated_commit_audit;").unwrap();
    conn.execute_batch("CREATE TRIGGER fail_commit_audit BEFORE INSERT ON audit WHEN NEW.kind='file.commit' BEGIN SELECT RAISE(FAIL, 'test audit failure'); END;").unwrap();
    credential_request(&fleet, &full, "file.put", json!({"to":"harbor","name":"commit-audit-unknown.txt","content_file":source}), "outcome_unknown");
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM files WHERE name='commit-audit-unknown.txt'", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 1);
    conn.execute_batch("DROP TRIGGER fail_commit_audit;").unwrap();

    // Corrupt only the just-inserted message's read representation, after
    // INSERT succeeds. The committed row survives a receiver read-back error.
    conn.execute_batch("CREATE TRIGGER fail_send_readback AFTER INSERT ON mail WHEN NEW.id='readback-unknown' BEGIN UPDATE mail SET payload='not-json' WHERE id=NEW.id; END;").unwrap();
    credential_request(&fleet, &full, "send", json!({"id":"readback-unknown","to":"harbor","kind":"notice","body":"Committed before read-back failed"}), "outcome_unknown");
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM mail WHERE id='readback-unknown'", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 1);
    conn.execute_batch("DROP TRIGGER fail_send_readback; UPDATE mail SET payload='{}' WHERE id='readback-unknown';").unwrap();

    // The receiver, not just local admission, enforces the carried ship set.
    fs::write(&ssh, "#!/usr/bin/python3\nimport json, subprocess, sys\nr = json.loads(sys.stdin.readline())\nr['args']['context']['ships'] = []\np = subprocess.run(['/bin/sh', '-c', sys.argv[-1]], input=json.dumps(r)+'\\n', text=True)\nsys.exit(p.returncode)\n").unwrap();
    credential_request(&fleet, &full, "send", json!({"to":"harbor","kind":"notice","body":"Remote ship denial"}), "ship_not_allowed");

    // Simulate version skew, then a broken transport. No legacy send appears
    // in either trace and neither failure writes captain mail remotely.
    fs::write(&ssh, "#!/usr/bin/python3\nimport json, sys\nr = json.loads(sys.stdin.readline())\nprint(json.dumps({'id':r['id'],'ok':False,'error':'unknown gateway command fleet.delegated.v1'}))\n").unwrap();
    credential_request(&fleet, &full, "send", json!({"to":"harbor","kind":"notice","body":"Must not downgrade"}), "linked_provenance_unavailable");
    // Authentication-stage codes cannot be asserted by an admitted receiver.
    fs::write(&ssh, "#!/usr/bin/python3\nimport json, sys\nr = json.loads(sys.stdin.readline())\nprint(json.dumps({'id':r['id'],'ok':False,'denial_code':'revoked_grant'}))\n").unwrap();
    credential_request(&fleet, &full, "send", json!({"to":"harbor","kind":"notice","body":"Must not downgrade"}), "linked_provenance_unavailable");
    // Unknown structured codes and plain-text error strings are not decoded.
    fs::write(&ssh, "#!/usr/bin/python3\nimport json, sys\nr = json.loads(sys.stdin.readline())\nprint(json.dumps({'id':r['id'],'ok':False,'error':'outcome_unknown','denial_code':'future_authority'}))\n").unwrap();
    credential_request(&fleet, &full, "send", json!({"to":"harbor","kind":"notice","body":"Must not downgrade"}), "linked_provenance_unavailable");
    fs::write(&ssh, format!("#!/bin/sh\nIFS= read -r request\nprintf '%s\\n' \"$request\" >> '{}.requests'\nexit 73\n", ssh.display())).unwrap();
    credential_request(&fleet, &full, "send", json!({"to":"harbor","kind":"notice","body":"Must not downgrade"}), "outcome_unknown");
    credential_request(&fleet, &full, "file.put", json!({"to":"harbor","name":"failed.txt","content_file":source}), "execution_failed");
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM mail WHERE body='Must not downgrade'", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 0);
    // Fail after begin, not merely before the transfer. A partial upload must
    // never acquire committed file metadata, and is left for TTL cleanup.
    fs::write(&ssh, "#!/bin/sh\nfor argument do remote=$argument; done\nIFS= read -r request\ncase \"$request\" in *file.append*) exit 73;; esac\nprintf '%s\\n' \"$request\" | /bin/sh -c \"$remote\"\n").unwrap();
    credential_request(&fleet, &full, "file.put", json!({"to":"harbor","name":"partial.txt","content_file":source}), "execution_failed");
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM files WHERE name='partial.txt'", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 0);
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM file_uploads WHERE name='partial.txt'", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 1);

    // Real remote commits, then ssh loses the response and exits nonzero.
    // Nothing is retried, and neither committed outcome is called a refusal.
    fs::write(&ssh, "#!/bin/sh\nfor argument do remote=$argument; done\nIFS= read -r request\ncase \"$request\" in *file.commit*|*\\\"send\\\"*) printf '%s\\n' \"$request\" | /bin/sh -c \"$remote\" >/dev/null; exit 73;; esac\nprintf '%s\\n' \"$request\" | /bin/sh -c \"$remote\"\n").unwrap();
    credential_request(&fleet, &full, "send", json!({"to":"harbor","kind":"notice","body":"Committed before transport failed"}), "outcome_unknown");
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM mail WHERE body='Committed before transport failed'", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 1);
    credential_request(&fleet, &full, "file.put", json!({"to":"harbor","name":"committed-unknown.txt","content_file":source}), "outcome_unknown");
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM files WHERE name='committed-unknown.txt'", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 1);
    // Admin transport loss after a real remote commit is uncertain too.
    fs::write(&ssh, format!("#!/bin/sh\nfor argument do remote=$argument; done\nIFS= read -r request\nprintf '%s\\n' \"$request\" | PRIVATEER_RESOURCE_HOME='{}' /bin/sh -c \"$remote\" >/dev/null\nexit 73\n", server.join("ledger").display())).unwrap();
    let result = delegated_ok(&fleet, &admin, "permissions", json!({"mode":"yolo"}));
    assert_eq!(result["partial"], true);
    assert_eq!(result["machines"][0]["confirmed"], true);
    assert_eq!(result["machines"][1]["denial_code"], "outcome_unknown");
    credential_request(&fleet, &admin, "stow", json!({"action":"start","machine":"remote"}), "outcome_unknown");
    let ledger = rusqlite::Connection::open(server.join("ledger/resources.sqlite")).unwrap();
    let count: i64 = ledger.query_row("SELECT COUNT(*) FROM stows WHERE resumed_ms IS NULL", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 1);
    let calls = fs::read_to_string(ssh.with_extension("requests")).unwrap();
    assert!(calls.contains("fleet.delegated.v1"), "{calls}");
    assert!(!calls.contains("captain.send"), "{calls}");
    assert!(!calls.contains("mail.deliver"), "{calls}");
    stop(supervisor);
}

#[test]
fn delegated_reverse_only_link_refuses_without_queuing_operator_mail() {
    let fleet = Fleet::new();
    let server = fleet.root.join("reverse-fleet");
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
    remote(&["init", "--ship", "harbor", "--harness", "codex"]);
    let ssh = fleet.root.join("fake-ssh");
    fs::write(&ssh, r#"#!/bin/sh
for argument do remote=$argument; done
exec /bin/sh -c "$remote"
"#).unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    remote(&["machine", "add", "receiver", "test-host", "--executable", BIN,
        "--remote-fleet", fleet.root.join("state/fleet.sqlite").to_str().unwrap(),
        "--as", "source-machine"]);
    delegated_adapter(&fleet);
    let (_, full) = delegated_issue(&fleet, "full", ALL_DELEGATED_SCOPES, None);
    let supervisor = fleet.supervise();
    fleet.wait("scoped hello", |e| e.first().cloned());
    for kind in ["request", "notice", "reply"] {
        let mut args = json!({"to":"harbor","kind":kind,"body":"Never queue as operator"});
        if kind == "reply" { args["correlation"] = json!("remote-ask"); }
        credential_request(&fleet, &full, "send", args, "linked_provenance_unavailable");
    }
    for ship in ["ops", "liaison"] {
        let conn = rusqlite::Connection::open(fleet.home(ship).join("privateer.sqlite")).unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM mail WHERE body='Never queue as operator'", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0);
    }
    let result = delegated_ok(&fleet, &full, "permissions", json!({"mode":"standard"}));
    assert_eq!(result["partial"], true);
    assert_eq!(result["machines"][1]["denial_code"], "linked_provenance_unavailable");
    credential_request(&fleet, &full, "stow", json!({"action":"start","machine":"source-machine"}), "linked_provenance_unavailable");
    let result = delegated_ok(&fleet, &full, "stow", json!({"action":"resume","all":true}));
    assert_eq!(result["partial"], true);
    assert_eq!(result["machines"][0]["denial_code"], "linked_provenance_unavailable");
    stop(supervisor);
}
