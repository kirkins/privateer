fn edit_client(fleet: &Fleet, id: &str, options: &[&str]) -> Value {
    let mut args = vec!["door", "client", "edit", id];
    args.extend_from_slice(options);
    let (result, tty) = client_terminal(fleet, &args, "edit");
    assert!(tty.contains("\"before\"") && tty.contains("\"after\""));
    assert!(tty.contains("Paused and expired grants may be edited"));
    result["clients"][0].clone()
}

#[test]
fn client_edit_flags_validation_owner_boundary_audit_and_liveness() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (grant, _) = delegated_issue(&fleet, "editable", "fleet:read", Some("ops"));
    let id = grant["clients"][0]["id"].as_str().unwrap();
    for marker in ["PRIVATEER_ROLE", "PRIVATEER_JOB"] {
        let out = fleet.command(&fleet.root).env(marker, "agent").env("PRIVATEER_ACTOR", "ops")
            .args(["door", "client", "edit", id, "--all-ships"]).output().unwrap();
        assert!(!out.status.success());
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(error.contains("captain-only") || error.contains("operator command"), "{error}");
    }
    assert!(fleet.refused(&["--role", "agent", "--actor", "ops", "door", "client", "edit", id, "--all-ships"]).contains("operator command"));
    assert!(fleet.refused(&["--home", fleet.home("ops").to_str().unwrap(), "door.client.edit", "--json", "{}"]).contains("unknown command"));
    assert!(!client_no_terminal(&fleet, &["edit", id, "--all-ships"]).status.success());
    for options in [
        vec![], vec!["--scopes", "not:a-scope"], vec!["--remove-scopes", "unknown"],
        vec!["--ships", "not-created"], vec!["--add-ships", "not-created"], vec!["--remove-ships", "not-created"],
        vec!["--scopes", "fleet:read", "--add-scopes", "mail:read"],
        vec!["--add-scopes", "mail:read", "--remove-scopes", "mail:read"],
        vec!["--ships", "ops", "--add-ships", "liaison"],
        vec!["--all-ships", "--ships", "ops"], vec!["--all-ships", "--named-ships"],
        vec!["--named-ships"], vec!["--name", "new-name"], vec!["--bogus"],
    ] {
        let mut args = vec!["door", "client", "edit", id];
        args.extend(options);
        assert!(!fleet.run_in(&fleet.root, &args).status.success(), "{args:?}");
    }
    let changed = edit_client(&fleet, id, &["--add-scopes", "ship:message,mail:read", "--add-ships", "liaison"]);
    assert_eq!(changed["scopes"], json!(["fleet:read", "mail:read", "ship:message"]));
    assert_eq!(changed["ships"], json!(["liaison", "ops"]));
    let changed = edit_client(&fleet, id, &["--remove-scopes", "mail:read", "--remove-ships", "ops"]);
    assert_eq!(changed["scopes"], json!(["fleet:read", "ship:message"]));
    assert_eq!(changed["ships"], json!(["liaison"]));
    let changed = edit_client(&fleet, id, &["--scopes", "mail:read", "--ships", "ops"]);
    assert_eq!(changed["scopes"], json!(["mail:read"]));
    assert_eq!(changed["ships"], json!(["ops"]));
    assert!(edit_client(&fleet, id, &["--all-ships"])["ships"].is_null());
    assert!(!client_no_terminal(&fleet, &["edit", id, "--add-ships", "ops"]).status.success());
    let changed = edit_client(&fleet, id, &["--named-ships", "--ships", "", "--scopes", ""]);
    assert_eq!(changed["ships"], json!([]));
    assert_eq!(changed["scopes"], json!([]));
    client_call(&fleet, "pause", id);
    let changed = edit_client(&fleet, id, &["--scopes", "fleet:read"]);
    assert_eq!(changed["paused"], true);
    fleet.ok(&["door", "client", "expiry", id, "--expires-ms", "1"]);
    let changed = edit_client(&fleet, id, &["--ships", "ops"]);
    assert_eq!(changed["expires_ms"], 1);
    assert_eq!(changed["paused"], true);
    let inspect = client_call(&fleet, "inspect", id);
    assert_eq!(inspect["clients"][0]["ships"], json!(["ops"]));
    let audit: Value = serde_json::from_str(&fleet.ok(&["door", "client", "audit"])).unwrap();
    let edits: Vec<_> = audit["events"].as_array().unwrap().iter().filter(|e| e["event"] == "edit").collect();
    assert_eq!(edits.len(), 7);
    let detail: Value = serde_json::from_str(edits[0]["reason"].as_str().unwrap()).unwrap();
    assert_eq!(edits[0]["retention"], "lifecycle");
    assert_eq!(detail["owner_consent"], true);
    assert_eq!(detail["before"], json!({"scopes":["fleet:read"],"ships":["ops"]}));
    assert_eq!(detail["after"], json!({"scopes":["fleet:read","mail:read","ship:message"],"ships":["liaison","ops"]}));
    client_call(&fleet, "revoke", id);
    assert!(fleet.refused(&["door", "client", "edit", id, "--all-ships"]).contains("revoked"));
    fleet.ok(&["door", "remove", "fake"]);
    assert!(!client_no_terminal(&fleet, &["edit", id, "--all-ships"]).status.success());
}

#[test]
fn client_edit_refuses_authority_changed_during_terminal_consent() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (grant, _) = delegated_issue(&fleet, "racing", "fleet:read", Some("ops"));
    let id = grant["clients"][0]["id"].as_str().unwrap();
    // Mutate authority after the prompt but before answering it, through a
    // separate real CLI connection. The unseen state must not be overwritten.
    let helper = CLIENT_TERMINAL_HELPER.replace(
        "os.write(master, (sys.argv[1] + '\\n').encode())",
        "subprocess.run([sys.argv[2], 'door', 'client', 'pause', sys.argv[-2]], check=True, stdout=subprocess.DEVNULL)\n                os.write(master, (sys.argv[1] + '\\n').encode())");
    let env = fleet.command(&fleet.root).get_envs().filter_map(|(k,v)| v.map(|v| (k.to_owned(),v.to_owned()))).collect::<Vec<_>>();
    let output = support::command(support::python()).envs(env).current_dir(&fleet.root)
        .args(["-c", &helper, "edit", BIN, "door", "client", "edit", id, "--all-ships"]).output().unwrap();
    let result = terminal_helper_result(output);
    assert_ne!(result["status"], 0);
    assert!(result["error"].as_str().unwrap().contains("grant changed during consent"), "{result}");
    let inspect = client_call(&fleet, "inspect", id);
    assert_eq!(inspect["clients"][0]["paused"], true);
    assert_eq!(inspect["clients"][0]["ships"], json!(["ops"]));
}

#[test]
fn client_edit_rolls_back_if_lifecycle_audit_cannot_commit() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (grant, _) = delegated_issue(&fleet, "audit-failure", "fleet:read", Some("ops"));
    let id = grant["clients"][0]["id"].as_str().unwrap();
    let db = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER refuse_edit_audit BEFORE INSERT ON door_client_audit WHEN NEW.event='edit' BEGIN SELECT RAISE(ABORT,'fixture audit unavailable'); END;").unwrap();
    let env = fleet.command(&fleet.root).get_envs().filter_map(|(k,v)| v.map(|v| (k.to_owned(),v.to_owned()))).collect::<Vec<_>>();
    let output = support::command(support::python()).envs(env).current_dir(&fleet.root)
        .args(["-c", CLIENT_TERMINAL_HELPER, "edit", BIN, "door", "client", "edit", id, "--all-ships"]).output().unwrap();
    let result = terminal_helper_result(output);
    assert_ne!(result["status"], 0);
    assert!(result["error"].as_str().unwrap().contains("fixture audit unavailable"));
    assert_eq!(client_call(&fleet, "inspect", id)["clients"][0]["ships"], json!(["ops"]));
    db.execute_batch("DROP TRIGGER refuse_edit_audit;").unwrap();
    // Named linked ships use the same cached inventory as consent's picker.
    db.execute("INSERT INTO machine_ships(machine,name,scope) VALUES('peer','linked-ship','fixture')", []).unwrap();
    assert_eq!(edit_client(&fleet, id, &["--add-ships", "linked-ship"])["ships"], json!(["linked-ship", "ops"]));
}
