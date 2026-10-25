fn edit_client(fleet: &Fleet, id: &str, options: &[&str]) -> Value {
    let mut args = vec!["door", "client", "edit", id];
    args.extend_from_slice(options);
    let (result, tty) = client_terminal(fleet, &args, "edit");
    assert!(tty.contains("\"before\"") && tty.contains("\"after\""));
    assert!(tty.contains("Paused and expired grants may be edited"));
    result["clients"][0].clone()
}

fn edit_refused_without_terminal(fleet: &Fleet, args: &[&str], expected: &str) -> String {
    let out = client_no_terminal(fleet, args);
    assert!(!out.status.success(), "{args:?}");
    let error = String::from_utf8(out.stderr).unwrap();
    assert!(error.contains(expected), "{args:?}: {error}");
    error
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
    edit_refused_without_terminal(&fleet, &["edit", id, "--all-ships"], "local credentials require the captain at a controlling terminal");
    for (options, expected) in [
        (vec![], "edit needs an authority change"),
        (vec!["--scopes", "not:a-scope"], "unknown client scope"),
        (vec!["--remove-scopes", "unknown"], "unknown client scope"),
        (vec!["--ships", "not-created"], "unknown fleet ship: not-created"),
        (vec!["--add-ships", "not-created"], "unknown fleet ship: not-created"),
        (vec!["--remove-ships", "bad/name"], "identifier must contain 1–160 ASCII letters, digits, or -_.:@"),
        (vec!["--scopes", "fleet:read", "--add-scopes", "mail:read"], "cannot combine set with add/remove"),
        (vec!["--add-scopes", "mail:read", "--remove-scopes", "mail:read"], "cannot add and remove the same value"),
        (vec!["--ships", "ops", "--add-ships", "liaison"], "cannot combine set with add/remove"),
        (vec!["--all-ships", "--ships", "ops"], "--all-ships conflicts with named ship edits"),
        (vec!["--all-ships", "--named-ships"], "--all-ships conflicts with named ship edits"),
        (vec!["--named-ships"], "--named-ships requires --ships"),
        (vec!["--name", "new-name"], "usage: privateer door client"),
        (vec!["--bogus"], "usage: privateer door client"),
    ] {
        let mut args = vec!["edit", id];
        args.extend(options);
        let error = edit_refused_without_terminal(&fleet, &args, expected);
        assert!(!error.contains("controlling terminal"), "{error}");
        if expected.starts_with("unknown fleet ship") {
            assert!(!error.contains("--all-ships"), "{error}");
            assert!(error.contains("--ships") && error.contains("--remove-ships"), "{error}");
        }
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
    edit_refused_without_terminal(&fleet, &["edit", id, "--add-ships", "ops"], "all-ships has no explicit list; use --named-ships --ships CSV");
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
    edit_refused_without_terminal(&fleet, &["edit", id, "--all-ships"], "client is revoked");
}

#[test]
fn client_edit_preserves_and_removes_retired_ships() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (grant, _) = delegated_issue(&fleet, "retired-ship", "fleet:read", Some("ops"));
    let id = grant["clients"][0]["id"].as_str().unwrap();
    let db = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    db.execute("UPDATE ships SET retired_ms=1 WHERE name='ops'", []).unwrap();
    // Neither scope edits nor adding a live ship should be blocked by a stale
    // selection already in the grant. None of these edits should widen to all.
    let changed = edit_client(&fleet, id, &["--add-scopes", "mail:read"]);
    assert_eq!(changed["ships"], json!(["ops"]));
    let changed = edit_client(&fleet, id, &["--add-ships", "liaison"]);
    assert_eq!(changed["ships"], json!(["liaison", "ops"]));
    let changed = edit_client(&fleet, id, &["--remove-ships", "ops"]);
    assert_eq!(changed["ships"], json!(["liaison"]));
    // A retired name cannot be newly granted through either set or add.
    for flag in ["--ships", "--add-ships"] {
        let error = edit_refused_without_terminal(&fleet, &["edit", id, flag, "ops"], "unknown fleet ship: ops");
        assert!(!error.contains("--all-ships"), "{error}");
        assert!(!error.contains("controlling terminal"), "{error}");
    }
    assert_eq!(client_call(&fleet, "inspect", id)["clients"][0]["ships"], json!(["liaison"]));
}

#[test]
fn client_edit_noops_skip_terminal_writes_and_audit() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (named, _) = delegated_issue(&fleet, "unchanged-named", "ship:message,fleet:read", Some("ops,liaison"));
    let named_id = named["clients"][0]["id"].as_str().unwrap();
    let (all, _) = delegated_issue(&fleet, "unchanged-all", "fleet:read", None);
    let all_id = all["clients"][0]["id"].as_str().unwrap();
    let audit_before = fleet.ok(&["door", "client", "audit"]);
    let db = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER refuse_noop_update BEFORE UPDATE ON door_clients BEGIN SELECT RAISE(ABORT,'no-op must not update'); END;
        CREATE TRIGGER refuse_noop_audit BEFORE INSERT ON door_client_audit BEGIN SELECT RAISE(ABORT,'no-op must not audit'); END;").unwrap();
    for (id, options) in [
        (named_id, vec!["--add-scopes", "fleet:read"]),
        (named_id, vec!["--add-ships", "ops"]),
        (named_id, vec!["--remove-ships", "not-created"]),
        (named_id, vec!["--remove-scopes", "mail:read"]),
        (named_id, vec!["--scopes", "fleet:read,ship:message", "--ships", "liaison,ops"]),
        (all_id, vec!["--all-ships"]),
        (all_id, vec!["--add-scopes", "fleet:read"]),
    ] {
        let before = client_call(&fleet, "inspect", id);
        let mut args = vec!["edit", id];
        args.extend(options);
        // No controlling terminal: success proves consent was not requested.
        let out = client_no_terminal(&fleet, &args);
        let error = String::from_utf8(out.stderr).unwrap();
        assert!(out.status.success(), "{args:?}: {error}");
        assert_eq!(error.trim(), "nothing to change", "{args:?}");
        let result: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(result, before, "{args:?}");
        assert_eq!(client_call(&fleet, "inspect", id), before);
    }
    assert_eq!(fleet.ok(&["door", "client", "audit"]), audit_before);
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
fn client_edit_revalidates_new_ships_after_terminal_consent() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (grant, _) = delegated_issue(&fleet, "fleet-racing", "fleet:read", Some("ops"));
    let id = grant["clients"][0]["id"].as_str().unwrap();
    // Retire the newly selected ship after displaying the consent summary.
    // Preserving old names must not bypass revalidation of new authority.
    let helper = CLIENT_TERMINAL_HELPER.replace(
        "os.write(master, (sys.argv[1] + '\\n').encode())",
        "import sqlite3\n                with sqlite3.connect('state/fleet.sqlite') as db:\n                    db.execute(\"UPDATE ships SET retired_ms=1 WHERE name='liaison'\")\n                os.write(master, (sys.argv[1] + '\\n').encode())");
    let env = fleet.command(&fleet.root).get_envs().filter_map(|(k,v)| v.map(|v| (k.to_owned(),v.to_owned()))).collect::<Vec<_>>();
    let output = support::command(support::python()).envs(env).current_dir(&fleet.root)
        .args(["-c", &helper, "edit", BIN, "door", "client", "edit", id, "--add-ships", "liaison"]).output().unwrap();
    let result = terminal_helper_result(output);
    assert_ne!(result["status"], 0);
    let error = result["error"].as_str().unwrap();
    assert!(error.contains("unknown fleet ship: liaison"), "{result}");
    assert!(!error.contains("--all-ships"), "{result}");
    assert_eq!(client_call(&fleet, "inspect", id)["clients"][0]["ships"], json!(["ops"]));
    let audit: Value = serde_json::from_str(&fleet.ok(&["door", "client", "audit"])).unwrap();
    assert!(audit["events"].as_array().unwrap().iter().all(|e| e["event"] != "edit"));
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
