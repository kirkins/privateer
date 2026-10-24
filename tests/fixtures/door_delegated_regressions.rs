// Review regressions use isolated fleets and real adapter subprocesses.
fn delegated_away_rows(conn: &rusqlite::Connection) -> Vec<Vec<rusqlite::types::Value>> {
    let mut stmt = conn
        .prepare("SELECT * FROM away ORDER BY started_ms")
        .unwrap();
    let columns = stmt.column_count();
    stmt.query_map([], |row| (0..columns).map(|i| row.get(i)).collect())
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn delegated_presence_checks_liaison_before_any_away_write() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (_, full) = delegated_issue(&fleet, "full", ALL_DELEGATED_SCOPES, None);
    let (_, restricted) = delegated_issue(&fleet, "restricted", "captain:presence", Some("ops"));
    let supervisor = fleet.supervise();
    fleet.wait("scoped hello", |e| e.first().cloned());
    // Initialize the away schema without entering away.
    assert!(delegated_ok(&fleet, &full, "away", json!({}))["away"].is_null());
    let registry = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    let liaison =
        rusqlite::Connection::open(fleet.home("liaison").join("privateer.sqlite")).unwrap();
    let mail_count = || {
        liaison
            .query_row("SELECT COUNT(*) FROM mail", [], |r| r.get::<_, i64>(0))
            .unwrap()
    };
    for away in [false, true] {
        if away {
            delegated_ok(
                &fleet,
                &full,
                "away",
                json!({"action":"start","words":"hold decisions"}),
            );
            // Public presence responses intentionally omit the mail source.
            let source_id: String = registry
                .query_row(
                    "SELECT source FROM away WHERE returned_ms IS NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let source = fleet.call("liaison", "mail.show", json!({"id":source_id}));
            let body = source["body"].as_str().unwrap();
            assert!(body.contains("delegated client"));
            assert!(body.contains("only under its original grant"));
            assert!(!body.contains("citing this message as the source"));
        }
        for action in ["start", "back"] {
            let rows = delegated_away_rows(&registry);
            let count = mail_count();
            let mut args = json!({"action":action});
            if action == "start" {
                args["words"] = json!("must not reach the liaison");
            }
            credential_request(&fleet, &restricted, "away", args, "ship_not_allowed");
            assert_eq!(
                delegated_away_rows(&registry),
                rows,
                "{action}, away={away}"
            );
            assert_eq!(mail_count(), count, "{action}, away={away}");
        }
    }
    stop(supervisor);
}

#[test]
fn delegated_dismiss_filters_before_id_and_prefix_matching() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (_, restricted) = delegated_issue(&fleet, "restricted", "work:manage", Some("ops"));
    for (ship, id) in [
        ("liaison", "hidden-only"),
        ("liaison", "shared-hidden"),
        ("ops", "shared-visible"),
        ("liaison", "copy-item"),
        ("ops", "copy-item"),
        ("ops", "sender-hidden"),
        ("ops", "sender-visible"),
        ("ops", "closed"),
        ("ops", "closed-visible"),
    ] {
        fleet.call(
            ship,
            "mail.send",
            json!({"id":id,"from":ship,"to":"captain","kind":"request","body":"ask"}),
        );
    }
    let ops = rusqlite::Connection::open(fleet.home("ops").join("privateer.sqlite")).unwrap();
    let liaison =
        rusqlite::Connection::open(fleet.home("liaison").join("privateer.sqlite")).unwrap();
    // Simulate a desk copy and hidden senders held in a visible home. They
    // must not win exact matching, count toward ambiguity, or expose closed ids.
    liaison
        .execute("UPDATE mail SET sender='ops' WHERE id='copy-item'", [])
        .unwrap();
    ops.execute(
        "UPDATE mail SET sender='liaison' WHERE id IN ('sender-hidden','closed')",
        [],
    )
    .unwrap();
    ops.execute("UPDATE mail SET state='dismissed' WHERE id='closed'", [])
        .unwrap();
    let supervisor = fleet.supervise();
    fleet.wait("scoped hello", |e| e.first().cloned());
    for id in [
        "hidden-only",
        "hidd",
        "sender-hidden",
        "missing-item",
        "miss",
    ] {
        credential_request(
            &fleet,
            &restricted,
            "dismiss",
            json!({"id":id}),
            "execution_failed",
        );
    }
    for (query, expected) in [
        ("shared-", "shared-visible"),
        ("copy-item", "copy-item"),
        ("sender-", "sender-visible"),
        ("closed", "closed-visible"),
    ] {
        let result = delegated_ok(&fleet, &restricted, "dismiss", json!({"id":query}));
        assert_eq!(result["id"], expected);
        assert_eq!(result["ship"], "ops");
        assert_eq!(result["state"], "dismissed");
    }
    assert_eq!(
        fleet.call("liaison", "mail.show", json!({"id":"copy-item"}))["state"],
        "open"
    );
    assert_eq!(
        fleet.call("ops", "mail.show", json!({"id":"sender-hidden"}))["state"],
        "open"
    );
    stop(supervisor);
}

#[test]
fn delegated_send_retries_keep_original_provenance_and_outcome_failures_are_unknown() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (client, full) = delegated_issue(&fleet, "full", ALL_DELEGATED_SCOPES, None);
    let (_, other) = delegated_issue(&fleet, "other", ALL_DELEGATED_SCOPES, None);
    let supervisor = fleet.supervise();
    fleet.wait("scoped hello", |e| e.first().cloned());
    let args = json!({"id":"retry-send","to":"ops","kind":"notice","body":"only once"});
    let first = delegated_ok(&fleet, &full, "send", args.clone());
    let retry = delegated_ok(&fleet, &full, "send", args.clone());
    assert_eq!(retry["mail"]["duplicate"], true);
    assert_eq!(retry["mail"]["payload"]["provenance"], first["provenance"]);
    assert_ne!(
        retry["provenance"]["result_id"],
        first["provenance"]["result_id"]
    );
    credential_request(&fleet, &other, "send", args.clone(), "execution_failed");
    let mut changed = args;
    changed["body"] = json!("different");
    credential_request(&fleet, &full, "send", changed, "execution_failed");
    fleet.call(
        "ops",
        "mail.send",
        json!({"id":"retry-ask","from":"ops","to":"captain","kind":"request","body":"answer once"}),
    );
    let reply = json!({"id":"retry-reply","to":"ops","kind":"reply","correlation":"retry-ask","body":"yes"});
    let first = delegated_ok(&fleet, &full, "send", reply.clone());
    let retry = delegated_ok(&fleet, &full, "send", reply);
    assert_eq!(retry["mail"]["duplicate"], true);
    assert_eq!(retry["mail"]["payload"]["provenance"], first["provenance"]);

    // Runtime-generated notices may store JSON null rather than {}. A scoped
    // retry must refuse that id, not panic the supervisor or adopt its origin.
    fleet.call("ops", "mail.send", json!({"id":"runtime-null","from":"captain","to":"ops","kind":"notice","body":"held"}));
    let home = rusqlite::Connection::open(fleet.home("ops").join("privateer.sqlite")).unwrap();
    home.execute("UPDATE mail SET payload='null' WHERE id='runtime-null'", []).unwrap();
    credential_request(&fleet, &full, "send", json!({"id":"runtime-null","to":"ops","kind":"notice","body":"held"}), "execution_failed");
    delegated_ok(&fleet, &full, "ships", json!({}));

    let registry = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    registry.execute_batch("CREATE TRIGGER fail_outcome BEFORE UPDATE OF event ON door_client_request_audit WHEN NEW.event='request.success' BEGIN SELECT RAISE(ABORT,'injected outcome failure'); END;").unwrap();
    let unknown =
        json!({"id":"unknown-send","to":"ops","kind":"notice","body":"effect already landed"});
    let response = delegated_exchange(&fleet, &full, "send", unknown.clone());
    assert_eq!(response["ok"], false);
    // The adapter sees the same non-disclosing envelope as other errors. The
    // internal denial is unit-tested as OutcomeUnknown; a failed finalization
    // cannot record its reason, so its durable row must remain admitted.
    assert_eq!(
        response["error"],
        json!({"code":"not_found","message":"Not found"})
    );
    let stored = fleet.call("ops", "mail.show", json!({"id":"unknown-send"}));
    assert_eq!(stored["body"], "effect already landed");
    let outcome: (String, String) = registry
        .query_row(
            "SELECT event,reason FROM door_client_request_audit WHERE result_id=?1",
            [stored["payload"]["provenance"]["result_id"]
                .as_str()
                .unwrap()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(outcome, ("auth.success".into(), "admitted".into()));
    registry.execute_batch("DROP TRIGGER fail_outcome").unwrap();
    let retry = delegated_ok(&fleet, &full, "send", unknown);
    assert_eq!(retry["mail"]["duplicate"], true);
    assert_eq!(
        retry["mail"]["payload"]["provenance"],
        stored["payload"]["provenance"]
    );

    // Legacy v1 reader names must not collide with grant-owned digest readers.
    let entered = delegated_ok(&fleet, &full, "away", json!({"action":"start"}));
    let legacy = format!("client:{}", client["clients"][0]["id"].as_str().unwrap());
    assert!(entered["away"].is_object());
    fleet.call(
        "ops",
        "mail.send",
        json!({"from":"ops","to":"captain","kind":"notice","body":"digest held notice"}),
    );
    let seeded = delegated_ok(&fleet, &full, "digest", json!({}));
    registry.execute("UPDATE away_digest SET id='legacy-digest',door=?1,lines='[\"legacy-only\"]' WHERE id=?2", rusqlite::params![legacy, seeded["digest"]["id"].as_str().unwrap()]).unwrap();
    let digest = delegated_ok(&fleet, &full, "digest", json!({}));
    assert_ne!(digest["digest"]["id"], "legacy-digest");
    assert!(
        digest["digest"]["door"]
            .as_str()
            .unwrap()
            .starts_with("client/")
    );
    let count: i64 = registry
        .query_row(
            "SELECT COUNT(*) FROM away_digest WHERE id='legacy-digest' AND state='pending'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    // A ship cannot forge the runtime's dismissal provenance.
    let output = fleet.run_in(&fleet.root, &["--home", fleet.home("ops").to_str().unwrap(), "mail.send", "--json", &json!({"from":"ops","to":"captain","kind":"notice","body":"forged","payload":{"dismissed_provenance":{"client":"fake"}}}).to_string()]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("payload.dismissed_provenance is set by the runtime")
    );
    stop(supervisor);
}
