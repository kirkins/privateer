#[test]
fn delegated_carried_ask_reply_refuses_wrong_mailbox_without_answering_owner() {
    let fleet = Fleet::with_ships(&[("alpha", true), ("beta", false)]);
    delegated_adapter(&fleet);
    let (grant, full) = delegated_issue(&fleet, "reader-writer", "fleet:read,mail:read,ship:message", None);
    let (_, narrow) = delegated_issue(&fleet, "alpha-only", "fleet:read,mail:read,ship:message", Some("alpha"));
    let (_, writer) = delegated_issue(&fleet, "writer", "ship:message", None);
    for ship in ["alpha", "beta"] {
        let conn = rusqlite::Connection::open(fleet.home(ship).join("privateer.sqlite")).unwrap();
        conn.execute("INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,state) VALUES('carried-ask','beta','captain','request','Question from original owner','{}','normal',1,'open')", []).unwrap();
    }
    let supervisor = fleet.supervise();
    fleet.wait("copy reply hello", |e| e.first().cloned());
    let thread = delegated_ok(&fleet, &full, "thread", json!({"ship":"alpha","id":"carried-ask"}));
    assert_eq!(thread["items"][0]["mail"]["from"], "beta");
    let waiting = delegated_ok(&fleet, &full, "waiting", json!({}));
    let copy = waiting["waiting"].as_array().unwrap().iter().find(|r| r["ship"] == "alpha").unwrap();
    assert_eq!(copy["recipient"], "alpha");
    assert_eq!(copy["reply_available"], false);
    assert_eq!(copy["reply_unavailable_reason"], "carried_copy_use_original_owner");
    let hidden = delegated_ok(&fleet, &narrow, "waiting", json!({}));
    assert!(!hidden.to_string().contains("carried-ask"));
    assert!(!hidden.to_string().contains("beta"));
    let args = json!({"kind":"reply","correlation":"carried-ask","body":"Answer","idempotency_key":"copy-reply"});
    let refused = delegated_exchange(&fleet, &full, "send", args.clone());
    assert_eq!(refused["ok"], false, "{refused}");
    assert_eq!(refused["error"]["code"], "reply_wrong_mailbox");
    assert_eq!(refused["error"]["recipient"], "beta");
    assert!(refused["error"]["message"].as_str().unwrap().contains("Pass recipient beta"));
    // Hidden sender, unseen ask, and absent read authority share a value-free refusal.
    for (token, attempt) in [
        (&narrow, args.clone()),
        (&writer, args.clone()),
        (&full, json!({"kind":"reply","correlation":"absent","body":"Answer"})),
    ] {
        let hidden = delegated_exchange(&fleet, token, "send", attempt);
        assert_eq!(hidden["ok"], false);
        assert_eq!(hidden["error"]["code"], "reply_not_visible");
        assert!(hidden["error"].get("recipient").is_none());
    }
    for ship in ["alpha", "beta"] {
        let conn = rusqlite::Connection::open(fleet.home(ship).join("privateer.sqlite")).unwrap();
        assert!(!conn.query_row("SELECT EXISTS(SELECT 1 FROM mail WHERE correlation='carried-ask' AND kind='reply')", [], |r| r.get::<_, bool>(0)).unwrap());
        assert_eq!(conn.query_row("SELECT state FROM mail WHERE id='carried-ask'", [], |r| r.get::<_, String>(0)).unwrap(), "open");
    }
    let registry = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    assert!(!registry.query_row("SELECT EXISTS(SELECT 1 FROM door_write_results WHERE command='send' AND grant_id=?1)", [grant["clients"][0]["id"].as_str().unwrap()], |r| r.get::<_, bool>(0)).unwrap(), "a no-effect refusal must not retain a write receipt");
    // Stale asks must not become actionable wrong-mailbox refusals, even when
    // reusing the key from the earlier no-effect refusal.
    let copy = rusqlite::Connection::open(fleet.home("alpha").join("privateer.sqlite")).unwrap();
    copy.execute("UPDATE mail SET state='answered' WHERE id='carried-ask'", []).unwrap();
    let stale = delegated_exchange(&fleet, &full, "send", args);
    assert_eq!(stale["ok"], false);
    assert_eq!(stale["error"]["code"], "reply_closed");
    // The existing explicit runtime recipient route still answers only at the owner.
    let sent = delegated_ok(&fleet, &full, "send", json!({"to":"beta","kind":"reply","correlation":"carried-ask","body":"Answer at owner"}));
    assert_eq!(sent["ship"], "beta");
    assert_eq!(sent["mail"]["to"], "beta");
    stop(supervisor);
}

#[test]
fn delegated_waiting_explicit_reply_crosses_homes_without_widening_grant() {
    let fleet = Fleet::with_ships(&[("alpha", true), ("beta", false)]);
    delegated_adapter(&fleet);
    let (_, full) = delegated_issue(&fleet, "reader-writer", "fleet:read,mail:read,ship:message", None);
    let (_, narrow) = delegated_issue(&fleet, "alpha-only", "fleet:read,mail:read,ship:message", Some("alpha"));
    for ship in ["alpha", "beta"] {
        fleet.call(ship, "mail.send", json!({"id":format!("{ship}-ask"),"from":ship,"to":"captain","kind":"request","body":"Question"}));
    }
    let supervisor = fleet.supervise();
    fleet.wait("reply routing hello", |e| e.first().cloned());
    let capabilities = delegated_ok(&fleet, &full, "capabilities", json!({}));
    assert!(capabilities["reply_routing"]["recipient"].is_string());
    let waiting = delegated_ok(&fleet, &full, "waiting", json!({}));
    let row = waiting["waiting"].as_array().unwrap().iter().find(|r| r["ship"] == "beta").unwrap();
    assert_eq!(row["recipient"], "beta");
    assert_eq!(row["reply_available"], true);
    let default = delegated_exchange(&fleet, &full, "send", json!({"kind":"reply","correlation":row["mail"]["id"],"body":"Answer"}));
    assert_eq!(default["error"]["code"], "reply_wrong_mailbox");
    assert_eq!(default["error"]["recipient"], "beta");
    let denied = delegated_exchange(&fleet, &narrow, "send", json!({"to":row["recipient"],"kind":"reply","correlation":row["mail"]["id"],"body":"No"}));
    assert_eq!(denied["error"]["code"], "reply_not_visible");
    assert!(!denied["error"].to_string().contains("beta"));
    let args = json!({"to":row["recipient"],"kind":"reply","correlation":row["mail"]["id"],"body":"Answer","idempotency_key":"cross-home-reply"});
    let first = delegated_ok(&fleet, &full, "send", args.clone());
    assert_eq!(first["mail"]["to"], "beta");
    assert_eq!(delegated_ok(&fleet, &full, "send", args), first);
    let closed = delegated_exchange(&fleet, &full, "send", json!({"to":"beta","kind":"reply","correlation":"beta-ask","body":"Another"}));
    assert_eq!(closed["error"]["code"], "reply_closed");
    let waiting = delegated_ok(&fleet, &full, "waiting", json!({}));
    assert!(!waiting["waiting"].as_array().unwrap().iter().any(|r| r["mail"]["id"] == "beta-ask"));
    // Omitting to still answers the door default, without consulting other homes.
    let default = delegated_ok(&fleet, &full, "send", json!({"kind":"reply","correlation":"alpha-ask","body":"Default answer"}));
    assert_eq!(default["ship"], "alpha");
    for ship in ["alpha", "beta"] {
        let ask = fleet.call(ship, "mail.show", json!({"id":format!("{ship}-ask")}));
        assert_eq!(ask["state"], "answered");
    }
    stop(supervisor);
}

#[test]
fn delegated_ordinary_decision_requires_exact_typed_answer_and_never_approves() {
    let fleet = Fleet::with_ships(&[("alpha", true), ("beta", false)]);
    delegated_adapter(&fleet);
    let (_, token) = delegated_issue(&fleet, "ordinary", "fleet:read,mail:read,ship:message,approval:answer", None);
    let (_, writer) = delegated_issue(&fleet, "ordinary-writer", "ship:message", Some("beta"));
    let decision = json!({"question":"Which chart?", "options":[
        {"id":"east","label":"East","what_happens":"Read east chart"},
        {"id":"west","label":"West","what_happens":"Read west chart"}]});
    fleet.call("beta", "mail.send", json!({"id":"ordinary-decision","from":"beta","to":"captain","kind":"decision","body":"Choose a chart","payload":{"decision":decision}}));
    fleet.call("alpha", "mail.send", json!({"id":"ordinary-question","from":"alpha","to":"captain","kind":"request","body":"How is the weather?"}));
    // A separately declared approval can never use this ordinary typed path.
    fleet.call("beta", "mail.send", json!({"id":"declared-decision","from":"beta","to":"captain","kind":"decision","task_id":"beta-duty","body":"Choose", "payload":{"approves":{"action":"Read east chart"},"decision":{"question":"Read east chart?","options":[{"id":"east","label":"Read","what_happens":"Read chart","approves":true},{"id":"west","label":"Hold","what_happens":"Wait","approves":false}]}}}));
    let supervisor = fleet.supervise();
    fleet.wait("ordinary decision hello", |e| e.first().cloned());
    for (ship, id) in [("alpha", "ordinary-question"), ("beta", "ordinary-decision")] {
        let response = delegated_exchange(&fleet, &token, "approval", json!({"mode":"prepare","ship":ship,"id":id}));
        assert_eq!(response["error"]["code"], "approval_not_required", "{response}");
        assert!(response["error"]["message"].as_str().unwrap().contains("reply"));
        assert!(response["error"]["message"].as_str().unwrap().contains("option_id"));
    }
    let args = json!({"to":"beta","kind":"reply","correlation":"ordinary-decision","body":"East please","idempotency_key":"ordinary-choice"});
    for payload in [json!({}), json!({"answer":{"option_id":"absent"}})] {
        let mut bad = args.clone();
        bad["payload"] = payload;
        let response = delegated_exchange(&fleet, &writer, "send", bad);
        assert_eq!(response["error"]["code"], "reply_invalid_answer", "{response}");
    }
    assert_eq!(fleet.call("beta", "mail.show", json!({"id":"ordinary-decision"}))["state"], "open");
    let mut args = args;
    args["payload"] = json!({"answer":{"option_id":"east","text":"Please use the east chart"}});
    let sent = delegated_ok(&fleet, &writer, "send", args.clone());
    assert_eq!(sent["mail"]["payload"]["answer"], args["payload"]["answer"]);
    assert!(sent["mail"]["payload"].get("approval").is_none());
    assert_eq!(delegated_ok(&fleet, &writer, "send", args), sent);
    let refused = delegated_exchange(&fleet, &token, "send", json!({"to":"beta","kind":"reply","correlation":"declared-decision","body":"yes","payload":{"answer":{"option_id":"east"}}}));
    assert_eq!(refused["error"]["code"], "not_found", "sensitive reply refusals are opaque at the core: {refused}");
    let prose = delegated_ok(&fleet, &token, "send", json!({"kind":"reply","correlation":"ordinary-question","body":"Fine"}));
    assert!(prose["mail"]["payload"].get("approval").is_none());
    let conn = rusqlite::Connection::open(fleet.home("beta").join("privateer.sqlite")).unwrap();
    assert_eq!(conn.query_row("SELECT count(*) FROM captain_approvals", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    assert_eq!(fleet.call("beta", "mail.show", json!({"id":"ordinary-decision"}))["state"], "answered");
    assert_eq!(fleet.call("beta", "mail.show", json!({"id":"declared-decision"}))["state"], "open");
    stop(supervisor);
}
