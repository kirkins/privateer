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
        format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{0}.calls'\nfor argument do remote=$argument; done\nIFS= read -r request\nprintf '%s\\n' \"$request\" >> '{0}.requests'\nprintf '%s\\n' \"$request\" | /bin/sh -c \"$remote\"\n", ssh.display()),
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
    for args in [json!({"action":"status","machine":"remote"}), json!({"action":"status","all":true})] {
        credential_request(&fleet, &full, "stow", args, "linked_provenance_unavailable");
    }
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
    }
    credential_request(&fleet, &messaging, "send", json!({"to":"harbor","kind":"reply","correlation":"remote-approval","body":"yes"}), "linked_provenance_unavailable");
    let conn = rusqlite::Connection::open(harbor.join(".privateer/privateer.sqlite")).unwrap();
    let state: String = conn.query_row("SELECT state FROM mail WHERE id='remote-approval'", [], |r| r.get(0)).unwrap();
    assert_eq!(state, "open");
    remote("mail.send", json!({"id":"remote-sensitive","from":"harbor","to":"captain","kind":"request","body":"Sensitive"}));
    conn.execute("UPDATE mail SET request_kind='production_approval' WHERE id='remote-sensitive'", []).unwrap();
    credential_request(&fleet, &full, "send", json!({"to":"harbor","kind":"reply","correlation":"remote-sensitive","body":"yes"}), "linked_provenance_unavailable");
    let state: String = conn.query_row("SELECT state FROM mail WHERE id='remote-sensitive'", [], |r| r.get(0)).unwrap();
    assert_eq!(state, "open");

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

    // Simulate version skew, then a broken transport. No legacy send appears
    // in either trace and neither failure writes captain mail remotely.
    fs::write(&ssh, format!("#!/bin/sh\nIFS= read -r request\nprintf '%s\\n' \"$request\" >> '{}.requests'\nprintf '%s\\n' '{{\"ok\":false,\"error\":\"unknown gateway command fleet.delegated.v1\"}}'\n", ssh.display())).unwrap();
    credential_request(&fleet, &full, "send", json!({"to":"harbor","kind":"notice","body":"Must not downgrade"}), "linked_provenance_unavailable");
    fs::write(&ssh, format!("#!/bin/sh\nIFS= read -r request\nprintf '%s\\n' \"$request\" >> '{}.requests'\nexit 73\n", ssh.display())).unwrap();
    credential_request(&fleet, &full, "send", json!({"to":"harbor","kind":"notice","body":"Must not downgrade"}), "linked_provenance_unavailable");
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
    stop(supervisor);
}
