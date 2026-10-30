use super::*;

fn context() -> DelegatedContext {
    DelegatedContext::resolved(
        uuid::Uuid::new_v4().to_string(),
        "door:alpha".into(),
        vec!["approval:answer".into()],
        Some(vec!["beta".into()]),
    )
}
fn binding() -> Value {
    json!({"version":1,"origin":uuid::Uuid::new_v4().to_string(),"link":"a".repeat(64),"authority":"b".repeat(64)})
}
fn prepared() -> Value {
    json!({"digest":"c".repeat(64),"version":4,"expires_ms":1234,
        "ship":"beta","id":"ask","kind":"request","action_summary":"Build",
        "grant":{"reply_id":uuid::Uuid::new_v4().to_string(),"command_prefix":null,
            "ttl_secs":60,"expires_ms":1234,"sentence":"Build via the chat door"},
        "approve_unavailable":false,"question":null,"options":[]})
}
fn wire(c: &DelegatedContext) -> Value {
    let mut b = binding();
    b["authority"] = json!(crate::core::hash(&json!([
        c.client(),
        c.resource(),
        authority(c)
    ])));
    json!({"version":1,"context":c.gateway(),"binding":b,"receive_budget_ms":5000,
        "args":{"mode":"prepare","ship":"beta","id":"ask"}})
}
#[test]
fn receiving_gateway_is_narrow_and_checks_both_modes() {
    let c = context();
    let valid = wire(&c);
    for (pointer, value, denial) in [
        ("/version", json!(2), Denial::Arguments),
        ("/receive_budget_ms", json!(0), Denial::Arguments),
        ("/receive_budget_ms", json!(5001), Denial::Arguments),
        ("/binding/origin", json!("alpha"), Denial::Arguments),
        ("/binding/link", json!("gamma"), Denial::Arguments),
        ("/context/scopes", json!([]), Denial::Scope),
        ("/context/ships", json!(["gamma"]), Denial::Ship),
    ] {
        let mut body = valid.clone();
        *body.pointer_mut(pointer).unwrap() = value;
        assert_eq!(
            serve(&body).unwrap_err().downcast_ref::<Denial>(),
            Some(&denial),
            "{pointer}"
        );
    }
    for field in ["command", "actor", "payload", "captain"] {
        let mut body = valid.clone();
        body[field] = json!("gamma");
        assert_eq!(
            serve(&body).unwrap_err().downcast_ref::<Denial>(),
            Some(&Denial::Arguments)
        );
    }
    let mut answer = valid;
    answer["args"] = json!({"mode":"answer","ship":"beta","id":"ask","version":4,"digest":"a".repeat(64),"action":"approve"});
    assert_eq!(
        serve(&answer).unwrap_err().downcast_ref::<Denial>(),
        Some(&Denial::ApprovalUnavailable)
    );
}
#[test]
fn elapsed_budget_is_relative_not_machine_uptime_or_cross_machine_wall_time() {
    assert_eq!(check_budget(Duration::from_millis(4999), 5000), Ok(()));
    assert_eq!(
        check_budget(Duration::from_millis(5000), 5000),
        Err(Denial::ApprovalUnavailable)
    );
    assert_eq!(
        check_budget(Duration::from_secs(u64::MAX), 5000),
        Err(Denial::ApprovalUnavailable)
    );
}
#[test]
fn projection_refuses_extra_content_bad_binding_and_malformed_grants() {
    let binding = binding();
    let args = json!({"id":"ask","ship":"beta"});
    let wire = json!({"version":1,"binding":binding,"prepared":prepared()});
    assert!(validate_reply(&wire, &binding, &args).is_ok());
    for (pointer, value) in [
        ("/binding", json!({})),
        ("/prepared/ship", json!("gamma")),
        ("/prepared/grant/reply_id", json!("gamma")),
        (
            "/prepared/options",
            json!([{"id":"gamma","label":"gamma","what_happens":"gamma","approves":true,"body":"secret"}]),
        ),
    ] {
        let mut bad = wire.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert!(validate_reply(&bad, &binding, &args).is_err());
    }
    for field in ["body", "payload", "note"] {
        let mut bad = wire.clone();
        bad["prepared"][field] = json!("secret");
        assert!(validate_reply(&bad, &binding, &args).is_err());
    }
}
#[test]
fn answer_projection_requires_the_same_binding_choice_and_closed_receipt() {
    let b = binding();
    let args =
        json!({"id":"ask","ship":"beta","digest":"c".repeat(64),"version":4,"action":"approve"});
    let wire = json!({"version":1,"binding":b,"answered":{"id":"ask","ship":"beta",
        "answered_via":"linked-chat-door","receipt":{"reply_id":uuid::Uuid::new_v4().to_string(),
        "digest":args["digest"],"version":4,"choice":{"action":"approve","option_id":null},
        "approved":true,"approval_id":"approval-alpha"}}});
    assert!(validate_answer(&wire, &b, &args).is_ok());
    for (pointer, value) in [
        ("/binding", json!({})),
        ("/answered/receipt/approved", json!(false)),
        ("/answered/receipt/choice/action", json!("deny")),
        ("/answered/receipt/version", json!(3)),
    ] {
        let mut bad = wire.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert!(validate_answer(&bad, &b, &args).is_err());
    }
    for pointer in ["/answered", "/answered/receipt"] {
        let mut bad = wire.clone();
        bad.pointer_mut(pointer).unwrap()["body"] = json!("secret-canary");
        assert!(validate_answer(&bad, &b, &args).is_err());
    }
}

fn registry(conn: &Connection) -> Route {
    crate::machines::init(conn).unwrap();
    conn.execute_batch("CREATE TABLE ships(name TEXT PRIMARY KEY, retired_ms INTEGER);
        INSERT INTO machines(name,role,peer,known_as,added_ms,incarnation) VALUES('beta','collects','{}','alpha',0,'beta');
        INSERT INTO machine_ships(machine,name,scope) VALUES('beta','beta','Build');").unwrap();
    schema(conn).unwrap();
    Route {
        machine: "beta".into(),
        ship: "beta".into(),
        incarnation: crate::machines::link_incarnation(conn, "beta").unwrap(),
        binding: None,
    }
}
#[test]
fn durable_pin_is_bounded_replaced_and_never_revives_after_link_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("alpha.sqlite");
    let conn = Connection::open(&path).unwrap();
    let route = registry(&conn);
    let c = context();
    let b = binding();
    let p = prepared();
    pin(&conn, &route, &c, &b, &p).unwrap();
    drop(conn);
    let conn = Connection::open(&path).unwrap();
    let (incarnation, saved, expiry): (String, String, i64) = conn
        .query_row(
            "SELECT incarnation,binding,challenge_expires_ms FROM door_linked_approval_routes",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(incarnation, route.incarnation);
    assert_eq!(saved, b.to_string());
    // Peer expiry is preserved, even if it is earlier than this host's clock.
    assert_eq!(expiry, 1234);
    let mut next = p.clone();
    next["digest"] = json!("d".repeat(64));
    pin(&conn, &route, &c, &b, &next).unwrap();
    assert!(
        !conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM door_linked_approval_routes WHERE digest=?1)",
                [p["digest"].as_str()],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
    );
    conn.execute(
        "UPDATE machines SET incarnation='gamma' WHERE name='beta'",
        [],
    )
    .unwrap();
    assert!(!current(&conn, &route));
    assert_eq!(
        pin(&conn, &route, &c, &b, &p)
            .unwrap_err()
            .downcast_ref::<Denial>(),
        Some(&Denial::ApprovalUnavailable)
    );
    conn.execute(
        "UPDATE machines SET incarnation='beta',peer='{}' WHERE name='beta'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE machines SET peer='{\"executable\":\"gamma\"}' WHERE name='beta'",
        [],
    )
    .unwrap();
    assert!(!current(&conn, &route));
}
#[test]
fn pin_capacity_cleanup_and_directory_custody_fail_closed() {
    let conn = Connection::open_in_memory().unwrap();
    let route = registry(&conn);
    conn.execute_batch("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<4096)
        INSERT INTO door_linked_approval_routes SELECT 'other','door:alpha',CAST(x AS TEXT),'gamma','ask','beta','link','{}','{}',0,9223372036854775807 FROM n;").unwrap();
    let c = context();
    // Another grant's full share cannot crowd out this one.
    pin(&conn, &route, &c, &binding(), &prepared()).unwrap();
    conn.execute(
        "UPDATE door_linked_approval_routes SET grant_id=?1 WHERE grant_id='other'",
        [c.client()],
    )
    .unwrap();
    let mut another = prepared();
    another["id"] = json!("another");
    another["digest"] = json!("e".repeat(64));
    assert_eq!(
        pin(&conn, &route, &c, &binding(), &another)
            .unwrap_err()
            .downcast_ref::<Denial>(),
        Some(&Denial::RetryCapacity)
    );
    conn.execute(
        "UPDATE door_linked_approval_routes SET retain_until_ms=0",
        [],
    )
    .unwrap();
    pin(&conn, &route, &context(), &binding(), &prepared()).unwrap();
    conn.execute(
        "INSERT INTO machine_ships(machine,name,scope) VALUES('gamma','beta','Build')",
        [],
    )
    .unwrap();
    assert!(!current(&conn, &route));
    conn.execute("DELETE FROM machine_ships WHERE machine='gamma'", [])
        .unwrap();
    conn.execute("INSERT INTO ships(name) VALUES('beta')", [])
        .unwrap();
    assert!(!current(&conn, &route));
}
