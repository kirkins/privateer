#[test]
fn delegated_linked_send_is_refused_even_with_every_scope() {
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
        "#!/bin/sh\nfor argument do remote=$argument; done\nexec /bin/sh -c \"$remote\"\n",
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
    for kind in ["request", "notice", "reply"] {
        let mut args = json!({"to":"harbor","kind":kind,"body":"Must stay local"});
        if kind == "reply" {
            args["correlation"] = json!("remote-ask");
        }
        credential_request(&fleet, &full, "send", args, "linked_provenance_unavailable");
    }
    let conn = rusqlite::Connection::open(harbor.join(".privateer/privateer.sqlite")).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM mail WHERE sender='captain' AND body='Must stay local'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
    stop(supervisor);
}
