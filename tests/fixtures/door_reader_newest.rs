// Real delegated admission and durable registry transitions, not a mock query.
#[test]
fn scoped_reader_newest_snapshot_filters_and_sparse_ack_gaps() {
    let fleet = Fleet::with_ships(&[("alpha", true), ("beta", false)]);
    delegated_adapter(&fleet);
    let (_, token) = delegated_issue(&fleet, "reader", "mail:read", None);
    let (_, other) = delegated_issue(&fleet, "other", "mail:read", None);
    let (_, restricted) = delegated_issue(&fleet, "restricted", "mail:read", Some("alpha"));
    let db = rusqlite::Connection::open(fleet.home("alpha").join("privateer.sqlite")).unwrap();
    for (id, sender, kind, created) in [
        ("alpha-first", "alpha", "notice", 300),
        ("beta-middle", "beta", "reply", 200),
        ("alpha-last", "alpha", "reply", 100),
    ] {
        db.execute("INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms) VALUES(?1,?2,'captain',?3,'Fixture','{}','normal',?4)",
            rusqlite::params![id,sender,kind,created]).unwrap();
    }
    let supervisor = fleet.supervise();
    fleet.wait("newest reader hello", |e| e.first().cloned());
    let oldest = delegated_ok(&fleet, &token, "inbox", json!({"ship":"alpha"}));
    let explicit = delegated_ok(&fleet, &token, "inbox", json!({"ship":"alpha","order":"oldest"}));
    for key in ["items", "continuation", "origin", "coverage"] {
        assert_eq!(oldest[key], explicit[key], "default response {key} stays identical");
    }
    assert_eq!(oldest["provenance"]["grant"], explicit["provenance"]["grant"]);
    let first = delegated_ok(&fleet, &token, "inbox", json!({"ship":"alpha","order":"newest","limit":1}));
    assert_eq!(first["items"][0], oldest["items"][2], "canonical mail and ref survive reordering");
    assert_eq!(first["origin"], oldest["origin"]);
    assert_eq!(first["coverage"], oldest["coverage"]);
    assert!(first["continuation"].is_string());
    for (bearer, args) in [
        (&other, json!({"ship":"alpha","order":"newest"})),
        (&token, json!({"ship":"beta","order":"newest"})),
        (&token, json!({"ship":"alpha","order":"oldest"})),
        (&token, json!({"ship":"alpha","order":"newest","from":"alpha"})),
        (&token, json!({"ship":"alpha","order":"newest","kind":"reply"})),
    ] {
        let mut args = args;
        args["continuation"] = first["continuation"].clone();
        credential_request(&fleet, bearer, "inbox", args, "continuation_invalid");
    }
    credential_request(&fleet, &token, "inbox", json!({"ship":"alpha","order":"newest","continuation":format!("{}x",first["continuation"].as_str().unwrap())}), "continuation_invalid");
    reader_mail(&fleet, "alpha", "after-snapshot", None);
    let next_args = json!({"ship":"alpha","order":"newest","limit":1,"continuation":first["continuation"]});
    let second = delegated_ok(&fleet, &token, "inbox", next_args.clone());
    assert_eq!(second["items"][0]["mail"]["id"], "beta-middle");
    credential_request(&fleet, &token, "inbox", next_args, "continuation_invalid");
    let third = delegated_ok(&fleet, &token, "inbox", json!({"ship":"alpha","order":"newest","continuation":second["continuation"]}));
    assert_eq!(third["items"].as_array().unwrap().len(), 1);
    assert_eq!(third["items"][0]["mail"]["id"], "alpha-first");
    assert!(third["continuation"].is_null());
    for (extra, expected) in [
        (json!({"from":"alpha","kind":"reply"}), vec!["alpha-last"]),
        (json!({"from":"beta"}), vec!["beta-middle"]),
        (json!({"kind":"reply"}), vec!["alpha-last", "beta-middle"]),
        (json!({"from":"gamma"}), vec![]),
    ] {
        let mut args = json!({"ship":"alpha","order":"newest"});
        args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let page = delegated_ok(&fleet, &token, "inbox", args);
        let ids: Vec<_> = page["items"].as_array().unwrap().iter().map(|i| i["mail"]["id"].as_str().unwrap()).collect();
        assert_eq!(ids, expected);
    }
    // Filtering cannot bypass either selected-home or sender visibility.
    let hidden = delegated_ok(&fleet, &restricted, "inbox", json!({"ship":"alpha","order":"newest","from":"beta"}));
    assert!(hidden["items"].as_array().unwrap().is_empty());
    let denied = delegated_exchange(&fleet, &restricted, "inbox", json!({"ship":"beta","order":"newest","from":"beta"}));
    let missing = delegated_exchange(&fleet, &restricted, "inbox", json!({"ship":"gamma","order":"newest","from":"beta"}));
    assert_eq!(denied["error"], missing["error"]);
    for extra in [json!({"order":"sideways"}), json!({"from":""}), json!({"kind":4})] {
        let mut args = json!({"ship":"alpha"});
        args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        credential_request(&fleet, &token, "inbox", args, "unsupported_arguments");
    }
    credential_request(&fleet, &token, "thread", json!({"ship":"alpha","id":"alpha-first","order":"newest"}), "unsupported_arguments");
    let registry = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    assert_eq!(registry.query_row("SELECT COALESCE(MAX(through_seq),0) FROM door_reader_stream", [], |r| r.get::<_,i64>(0)).unwrap(), 0);
    assert_eq!(registry.query_row("SELECT COUNT(*) FROM door_reader_served WHERE acked=1", [], |r| r.get::<_,i64>(0)).unwrap(), 0);
    assert_eq!(db.query_row("SELECT COALESCE(MAX(through_seq),0) FROM mail_cursor WHERE address='captain'", [], |r| r.get::<_,i64>(0)).unwrap(), 0);
    // Explicitly acknowledging the newest served reply leaves the older gap.
    delegated_ok(&fleet, &token, "ack", json!({"ship":"alpha","refs":reader_refs(&first)}));
    let remaining = delegated_ok(&fleet, &token, "inbox", json!({"ship":"alpha","order":"newest","kind":"reply"}));
    assert_eq!(remaining["items"].as_array().unwrap().len(), 1);
    assert_eq!(remaining["items"][0]["mail"]["id"], "beta-middle");
    assert_eq!(delegated_ok(&fleet, &token, "inbox", json!({"ship":"alpha"}))["items"][0]["mail"]["id"], "alpha-first");
    stop(supervisor);
}

#[test]
fn scoped_reader_lookup_acks_reserve_oldest_progress() {
    for lookup in [json!({"order":"newest"}), json!({"from":"alpha"}), json!({"kind":"notice"})] {
        let fleet = Fleet::with_ships(&[("alpha", true), ("beta", false)]);
        delegated_adapter(&fleet);
        let (_, token) = delegated_issue(&fleet, "reader", "mail:read", None);
        let db = rusqlite::Connection::open(fleet.home("alpha").join("privateer.sqlite")).unwrap();
        let supervisor = fleet.supervise();
        fleet.wait("lookup reserve hello", |e| e.first().cloned());
        let mut acknowledged = 0;
        // Two cycles acknowledge 256 lookup rows in total, recovering the
        // reserve between cycles rather than permanently filling sparse state.
        for round in 0..2 {
            let gap = format!("gap-{round}");
            db.execute("INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms)
                VALUES(?1,'beta','captain','reply','Fixture','{}','normal',0)", [&gap]).unwrap();
            db.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<129)
                INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms)
                SELECT ?1||x,'alpha','captain','notice','Fixture','{}','normal',0 FROM n;",
                [format!("tail-{round}-")]).unwrap();
            let mut args = json!({"ship":"alpha"});
            args.as_object_mut().unwrap().extend(lookup.as_object().unwrap().clone());
            for limit in [50, 50, 28] {
                args["limit"] = json!(limit);
                let page = delegated_ok(&fleet, &token, "inbox", args.clone());
                assert_eq!(page["items"].as_array().unwrap().len(), limit);
                assert!(page["items"].as_array().unwrap().iter().all(|i| i["mail"]["id"] != gap));
                delegated_ok(&fleet, &token, "ack", json!({"ship":"alpha","refs":reader_refs(&page)}));
                acknowledged += limit;
                args["continuation"] = page["continuation"].clone();
                assert!(args["continuation"].is_string());
            }
            // Further sparse acknowledgments must stop at the half-window,
            // not consume the slots needed by an oldest read (even elsewhere).
            args["limit"] = json!(1);
            credential_request(&fleet, &token, "inbox", args, "reader_window_full");
            let other_id = format!("other-{round}");
            reader_mail(&fleet, "beta", &other_id, None);
            let other = delegated_ok(&fleet, &token, "inbox", json!({"ship":"beta"}));
            assert_eq!(other["items"].as_array().unwrap().len(), 1);
            assert_eq!(other["items"][0]["mail"]["id"], other_id);
            delegated_ok(&fleet, &token, "ack", json!({"ship":"beta","refs":reader_refs(&other)}));
            // Oldest reads must still serve both unread rows, without
            // ReaderFull, gaps or duplicates, then compact the sparse acks.
            let remaining = if lookup["order"] == "newest" { 1 } else { 129 };
            for id in [gap, format!("tail-{round}-{remaining}")] {
                let page = delegated_ok(&fleet, &token, "inbox", json!({"ship":"alpha","limit":1}));
                assert_eq!(page["items"].as_array().unwrap().len(), 1);
                assert_eq!(page["items"][0]["mail"]["id"], id);
                delegated_ok(&fleet, &token, "ack", json!({"ship":"alpha","refs":reader_refs(&page)}));
            }
            let empty = delegated_ok(&fleet, &token, "inbox", json!({"ship":"alpha"}));
            assert!(empty["items"].as_array().unwrap().is_empty());
        }
        assert_eq!(acknowledged, 256);
        stop(supervisor);
    }
}

#[test]
fn scoped_reader_newest_pages_past_window_without_ack_and_survives_restart() {
    let fleet = Fleet::with_ships(&[("alpha", true), ("beta", false)]);
    delegated_adapter(&fleet);
    let (_, token) = delegated_issue(&fleet, "reader", "mail:read", None);
    let db = rusqlite::Connection::open(fleet.home("alpha").join("privateer.sqlite")).unwrap();
    db.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<300)
        INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms)
        SELECT 'alpha-'||x,'alpha','captain','notice','Fixture','{}','normal',0 FROM n;").unwrap();
    let supervisor = fleet.supervise();
    fleet.wait("newest window hello", |e| e.first().cloned());
    let response = delegated_exchange(&fleet, &token, "inbox", json!({"ship":"alpha","order":"newest","from":"alpha","limit":50}));
    assert_eq!(response["ok"], true, "initial page: {response}");
    let first = response["result"].clone();
    stop(supervisor);
    fs::write(fleet.root.join("requests.jsonl"), "").unwrap();
    fs::write(&fleet.log, "").unwrap();
    let supervisor = fleet.supervise();
    fleet.wait("newest restart hello", |e| e.first().cloned());
    reader_mail(&fleet, "alpha", "after-snapshot", None);
    let mut page = first;
    let mut ids = vec![];
    loop {
        ids.extend(page["items"].as_array().unwrap().iter().map(|i| i["mail"]["id"].as_str().unwrap().to_owned()));
        if page["continuation"].is_null() { break; }
        let response = delegated_exchange(&fleet, &token, "inbox", json!({"ship":"alpha","order":"newest","from":"alpha","limit":50,"continuation":page["continuation"]}));
        assert_eq!(response["ok"], true, "page after {} rows: {response}; audit: {}", ids.len(), fs::read_to_string(fleet.root.join("state/doors/fake/door.log")).unwrap());
        page = response["result"].clone();
    }
    assert_eq!(ids, (1..=300).rev().map(|n| format!("alpha-{n}")).collect::<Vec<_>>());
    let registry = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    assert!(registry.query_row("SELECT COUNT(*) FROM door_reader_served", [], |r| r.get::<_,i64>(0)).unwrap() <= 256);
    assert_eq!(registry.query_row("SELECT MAX(through_seq) FROM door_reader_stream", [], |r| r.get::<_,i64>(0)).unwrap(), 0);
    let fresh = delegated_ok(&fleet, &token, "inbox", json!({"ship":"alpha","order":"newest","limit":1}));
    assert_eq!(fresh["items"][0]["mail"]["id"], "after-snapshot");
    registry.execute("UPDATE door_reader_stream SET generation='changed'", []).unwrap();
    credential_request(&fleet, &token, "inbox", json!({"ship":"alpha","order":"newest","continuation":fresh["continuation"]}), "continuation_invalid");
    stop(supervisor);
}
