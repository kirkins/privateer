#[test]
fn delegated_admin_local_scope_provenance_and_stow_custody() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (client, admin) = delegated_issue(&fleet, "admin", "fleet:admin", None);
    let (_, observer) = delegated_issue(&fleet, "observer", "fleet:read", None);
    let (_, restricted) = delegated_issue(&fleet, "restricted", "fleet:admin", Some("ops"));
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
    let changed = delegated_ok(&fleet, &admin, "permissions", json!({"mode":"yolo"}));
    assert_eq!(changed["partial"], false);
    assert_eq!(changed["machines"][0]["result"]["mode"], "yolo");
    let started = delegated_ok(&fleet, &admin, "stow", json!({"action":"start"}));
    assert_eq!(started["held"], true);
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
    let consumed: i64 = conn.query_row("SELECT COUNT(*) FROM approvals WHERE consumed=1", [], |r| r.get(0)).unwrap();
    assert_eq!(consumed, 0, "scoped upgrades never consume production approvals");
    let stored: String = conn.query_row("SELECT value FROM metadata WHERE key=?1", [format!("upgrade-request:{}", confirmed["id"].as_str().unwrap())], |r| r.get(0)).unwrap();
    let stored: Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(stored["provenance"], confirmed["provenance"]);
    stop(supervisor);
}
