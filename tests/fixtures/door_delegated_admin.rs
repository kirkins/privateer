#[test]
fn delegated_admin_local_scope_provenance_and_stow_custody() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (client, admin) = delegated_issue(&fleet, "admin", "fleet:admin", None);
    let (_, observer) = delegated_issue(&fleet, "observer", "fleet:read", None);
    let (_, restricted) = delegated_issue(&fleet, "restricted", "fleet:admin,fleet:read", Some("ops"));
    let supervisor = fleet.supervise();
    fleet.wait("scoped hello", |e| e.first().cloned());
    for (command, args) in [
        ("permissions", json!({"mode":"yolo"})),
        ("stow", json!({"action":"start"})),
        ("stow", json!({"action":"resume"})),
        ("upgrade", json!({"commit":"b".repeat(40)})),
        ("upgrade", json!({"action":"confirm","confirmation":"unknown"})),
    ] {
        credential_request(&fleet, &observer, command, args, "insufficient_scope");
    }
    credential_request(&fleet, &restricted, "stow", json!({"action":"start"}), "ship_not_allowed");
    credential_request(&fleet, &restricted, "upgrade", json!({"commit":"b".repeat(40)}), "ship_not_allowed");
    let policy = fleet.root.join("state/fleet.permissions.json");
    let before = fs::read(&policy).ok();
    for mode in ["yolo", "standard"] {
        credential_request(&fleet, &restricted, "permissions", json!({"mode":mode}), "ship_not_allowed");
        assert_eq!(fs::read(&policy).ok(), before);
    }
    let changed = delegated_ok(&fleet, &admin, "permissions", json!({"mode":"yolo"}));
    assert_eq!(changed["partial"], false);
    assert_eq!(changed["machines"][0]["result"]["mode"], "yolo");
    let started = delegated_ok(&fleet, &admin, "stow", json!({"action":"start"}));
    assert_eq!(started["held"], true);
    let visible = delegated_ok(&fleet, &restricted, "stow", json!({"action":"status"}));
    assert_eq!(visible["ships"].as_array().unwrap().len(), 1);
    assert!(visible["ready"].is_null(), "hidden ships cannot influence readiness: {visible}");
    credential_request(&fleet, &restricted, "stow", json!({"action":"resume"}), "ship_not_allowed");
    for ship in ["ops", "liaison"] {
        let mail = fleet.call(ship, "mail.show", json!({"id":format!("{}-{ship}", started["stow"].as_str().unwrap())}));
        assert_eq!(mail["payload"]["provenance"]["client"], client["clients"][0]["id"]);
        assert!(mail["payload"]["operator"].is_null());
        let conn = rusqlite::Connection::open(fleet.home(ship).join("privateer.sqlite")).unwrap();
        if ship == "liaison" {
            let audit: String = conn.query_row("SELECT payload FROM audit WHERE kind='stow.start' ORDER BY seq DESC LIMIT 1", [], |r| r.get(0)).unwrap();
            let audit: Value = serde_json::from_str(&audit).unwrap();
            assert_eq!(audit["provenance"], started["provenance"]);
        }
    }
    let resumed = delegated_ok(&fleet, &admin, "stow", json!({"action":"resume"}));
    assert_eq!(resumed["held"], false);
    assert_eq!(resumed["stow"], started["stow"]);
    let result = delegated_ok(&fleet, &admin, "permissions", json!({"mode":"standard"}));
    assert_eq!(result["machines"][0]["result"]["mode"], "standard");
    let conn = rusqlite::Connection::open(fleet.home("liaison").join("privateer.sqlite")).unwrap();
    let audit: String = conn.query_row("SELECT payload FROM audit WHERE kind='permissions.mode' ORDER BY seq DESC LIMIT 1", [], |r| r.get(0)).unwrap();
    let audit: Value = serde_json::from_str(&audit).unwrap();
    assert_eq!(audit["provenance"], result["provenance"]);
    stop(supervisor);
}

#[test]
fn delegated_permissions_ship_restrictions_preflight_links_and_receiver() {
    let fleet = Fleet::new();
    let server = fleet.root.join("remote-fleet");
    let harbor = server.join("harbor");
    fs::create_dir_all(&harbor).unwrap();
    let output = fleet.command(&harbor)
        .env("HOME", &server)
        .env("PRIVATEER_FLEET", server.join("fleet.sqlite"))
        .env("PRIVATEER_RESOURCE_HOME", server.join("ledger"))
        .args(["init", "--ship", "harbor", "--harness", "codex"])
        .output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let ssh = fleet.root.join("fake-ssh");
    fs::write(&ssh, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{0}.calls'\nfor argument do remote=$argument; done\nIFS= read -r request\nprintf '%s\\n' \"$request\" | PRIVATEER_RESOURCE_HOME='{1}' /bin/sh -c \"$remote\"\n", ssh.display(), server.join("ledger").display())).unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    fleet.ok_in(&fleet.root.join("ships/liaison"), &[
        "machine", "add", "remote", "test-host", "--executable", BIN,
        "--remote-fleet", server.join("fleet.sqlite").to_str().unwrap(),
        "--as", "local-desk",
    ]);
    delegated_adapter(&fleet);
    let (_, full) = delegated_issue(&fleet, "full", "fleet:admin", None);
    let (_, restricted) = delegated_issue(&fleet, "restricted", "fleet:admin", Some("ops"));
    let (_, local_only) = delegated_issue(&fleet, "local-only", "fleet:admin", Some("ops,liaison"));
    let supervisor = fleet.supervise();
    fleet.wait("scoped hello", |e| e.first().cloned());
    let local_policy = fleet.root.join("state/fleet.permissions.json");
    let remote_policy = server.join("fleet.permissions.json");
    for mode in ["standard", "yolo"] {
        let result = delegated_ok(&fleet, &full, "permissions", json!({"mode":mode}));
        assert_eq!(result["partial"], false, "{result}");
        assert_eq!(result["machines"].as_array().unwrap().len(), 2);
        for row in result["machines"].as_array().unwrap() {
            assert_eq!(row["result"]["mode"], mode);
        }
        let before_local = fs::read(&local_policy).unwrap();
        let before_remote = fs::read(&remote_policy).unwrap();
        let before_calls = fs::read(ssh.with_extension("calls")).unwrap();
        let denied_mode = if mode == "standard" { "yolo" } else { "standard" };
        // One grant excludes a local home; the other covers every local ship
        // but excludes the known remote ship. Neither may change any machine.
        for credential in [&restricted, &local_only] {
            credential_request(&fleet, credential, "permissions", json!({"mode":denied_mode}), "ship_not_allowed");
            assert_eq!(fs::read(&local_policy).unwrap(), before_local);
            assert_eq!(fs::read(&remote_policy).unwrap(), before_remote);
            assert_eq!(fs::read(ssh.with_extension("calls")).unwrap(), before_calls);
        }
    }
    // Narrow the carried grant at the real subprocess boundary, bypassing
    // origin admission to exercise the receiver's independent home check.
    fs::write(&ssh, format!("#!/usr/bin/python3\nimport json, os, subprocess, sys\nr = json.loads(sys.stdin.readline())\nr['args']['context']['ships'] = ['ops', 'liaison']\nos.environ['PRIVATEER_RESOURCE_HOME'] = '{}'\np = subprocess.run(['/bin/sh', '-c', sys.argv[-1]], input=json.dumps(r)+'\\n', text=True)\nsys.exit(p.returncode)\n", server.join("ledger").display())).unwrap();
    let before_local = fs::read(&local_policy).unwrap();
    let before_remote = fs::read(&remote_policy).unwrap();
    let result = delegated_ok(&fleet, &full, "permissions", json!({"mode":"yolo"}));
    assert_eq!(result["partial"], true, "{result}");
    assert_eq!(result["machines"][0]["confirmed"], true);
    assert_eq!(result["machines"][1]["denial_code"], "ship_not_allowed");
    assert_eq!(fs::read(&local_policy).unwrap(), before_local);
    assert_eq!(fs::read(&remote_policy).unwrap(), before_remote);
    stop(supervisor);
}

fn delegated_wait_upgrade(fleet: &Fleet, result: &Value) {
    let state = fleet.root.join("data/privateer/upgrade-linked")
        .join(format!("{}.json", result["id"].as_str().unwrap()));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if fs::read(&state).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .is_some_and(|s| !s["outcome"].is_null()) { break; }
        assert!(Instant::now() < deadline, "fixture round did not finish");
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn delegated_upgrade_plans_are_grant_bound_durable_expiring_and_one_shot() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (a, first) = delegated_issue(&fleet, "first", "fleet:admin", None);
    let (_, second) = delegated_issue(&fleet, "second", "fleet:admin", None);
    let supervisor = fleet.supervise();
    fleet.wait("scoped hello", |e| e.first().cloned());
    let plan = delegated_ok(&fleet, &first, "upgrade", json!({"commit":"b".repeat(40)}));
    let other = delegated_ok(&fleet, &second, "upgrade", json!({"commit":"c".repeat(40)}));
    assert_ne!(plan["confirmation"], other["confirmation"]);
    let confirm = json!({"action":"confirm","confirmation":plan["confirmation"]});
    credential_request(&fleet, &second, "upgrade", confirm.clone(), "grant_bound_upgrade_unavailable");
    let registry = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    registry.execute("UPDATE door_upgrade_plans SET expires_ms=0 WHERE id=?1", [other["confirmation"].as_str().unwrap()]).unwrap();
    credential_request(&fleet, &second, "upgrade", json!({"action":"confirm","confirmation":other["confirmation"]}), "grant_bound_upgrade_unavailable");
    stop(supervisor);
    // Confirmations survive the supervisor, without using its trusted v1 slot.
    fs::write(fleet.root.join("requests.jsonl"), "").unwrap();
    fs::write(&fleet.log, "").unwrap();
    let supervisor = fleet.supervise();
    fleet.wait("restarted scoped hello", |e| e.first().cloned());
    let confirmed = delegated_ok(&fleet, &first, "upgrade", confirm.clone());
    assert_eq!(confirmed["commit"], plan["commit"]);
    assert_eq!(confirmed["environment"], plan["environment"]);
    assert_eq!(confirmed["provenance"]["client"], a["clients"][0]["id"]);
    credential_request(&fleet, &first, "upgrade", confirm, "grant_bound_upgrade_unavailable");
    delegated_wait_upgrade(&fleet, &confirmed);
    let conn = rusqlite::Connection::open(fleet.home("liaison").join("privateer.sqlite")).unwrap();
    let approvals: i64 = conn.query_row("SELECT COUNT(*) FROM approvals", [], |r| r.get(0)).unwrap();
    assert_eq!(approvals, 0, "scoped upgrades neither create nor consume production approvals");
    // Completed request metadata may already have been removed by notify.
    // Its immutable audit and round state retain the original provenance.
    let stored: String = conn.query_row("SELECT payload FROM audit WHERE kind='upgrade.requested' AND json_extract(payload,'$.id')=?1", [confirmed["id"].as_str().unwrap()], |r| r.get(0)).unwrap();
    let stored: Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(stored["provenance"], confirmed["provenance"]);
    let ghost: i64 = conn.query_row("SELECT COUNT(*) FROM mail WHERE recipient='delegated-client'", [], |r| r.get(0)).unwrap();
    assert_eq!(ghost, 0, "a scoped client is not an agent mailbox");
    stop(supervisor);
}
