#[test]
fn scoped_reader_overdue_is_derived_consistently_for_inbox_thread_and_show() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (_, token) = delegated_issue(&fleet, "reader", "mail:read", Some("ops"));
    let ops = rusqlite::Connection::open(fleet.home("ops").join("privateer.sqlite")).unwrap();
    let now = privateer::store::Store::now_ms();
    let cases = [
        ("overdue-root", "notice", None, None, false),
        ("future", "request", Some("open"), Some(now + 86_400_000), false),
        ("expired", "request", Some("open"), Some(now - 86_400_000), true),
        ("no-deadline", "request", Some("open"), None, false),
        ("answered", "request", Some("answered"), Some(now - 86_400_000), false),
        ("withdrawn", "request", Some("withdrawn"), Some(now - 86_400_000), false),
        ("task-ended", "request", Some("task_done"), Some(now - 86_400_000), false),
    ];
    for (id, kind, state, deadline, _) in cases {
        ops.execute(
            "INSERT INTO mail(id,sender,recipient,kind,correlation,body,payload,urgency,deadline_ms,created_ms,state)
             VALUES(?1,'ops','captain',?2,'overdue-root','Deadline fixture','{}','normal',?3,?4,?5)",
            rusqlite::params![id, kind, deadline, now, state],
        ).unwrap();
    }
    let lifecycle = || ops.query_row(
        "SELECT json_group_array(json_object('id',id,'state',state,'deadline',deadline_ms,'answered_by',answered_by,'ended_by',ended_by)) FROM mail",
        [], |r| r.get::<_, String>(0),
    ).unwrap();
    let before = lifecycle();
    let supervisor = fleet.supervise();
    fleet.wait("deadline reader hello", |e| e.first().cloned());
    let inbox = delegated_ok(&fleet, &token, "inbox", json!({"ship":"ops"}));
    let thread = delegated_ok(&fleet, &token, "thread", json!({"ship":"ops","id":"overdue-root"}));
    for (id, _, _, _, expected) in cases {
        let shown = delegated_ok(&fleet, &token, "show", json!({"ship":"ops","id":id}));
        assert_eq!(shown["mail"]["overdue"], expected, "show {id}");
        for (name, page) in [("inbox", &inbox), ("thread", &thread)] {
            let item = page["items"].as_array().unwrap().iter()
                .find(|item| item["mail"]["id"] == id).unwrap();
            assert_eq!(item["mail"]["overdue"], expected, "{name} {id}");
            assert_eq!(item["mail"], shown["mail"], "same mail projection for {id}");
        }
    }
    assert_eq!(lifecycle(), before, "reader projections never change request lifecycle");
    let captain_cursor: i64 = ops.query_row(
        "SELECT COALESCE(MAX(through_seq),0) FROM mail_cursor WHERE address='captain'", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(captain_cursor, 0, "listing never acknowledges the captain's inbox");
    // A continuation freezes the sequence boundary, not read-time deadline facts.
    let first = delegated_ok(&fleet, &token, "inbox", json!({"ship":"ops","limit":1}));
    assert!(first["continuation"].is_string());
    ops.execute("UPDATE mail SET deadline_ms=?1 WHERE id='future'", [now - 1]).unwrap();
    let next = delegated_ok(&fleet, &token, "inbox", json!({"ship":"ops","limit":1,"continuation":first["continuation"]}));
    assert_eq!(next["items"][0]["mail"]["id"], "future");
    assert_eq!(next["items"][0]["mail"]["overdue"], true);
    assert_eq!(next["items"][0]["ref"], inbox["items"][1]["ref"], "annotation never reallocates served refs");
    stop(supervisor);
}

#[test]
fn scoped_reader_removed_home_releases_window_and_copy_proofs() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (_, token) = delegated_issue(&fleet,"reader","mail:read",None);
    reader_mail(&fleet,"ops","same-durable-id",None);
    reader_mail(&fleet,"liaison","same-durable-id",None);
    let supervisor = fleet.supervise();
    fleet.wait("reader hello", |e| e.first().cloned());
    let first = delegated_ok(&fleet,&token,"show",json!({"ship":"ops","id":"same-durable-id"}));
    let registry = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    registry.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<255)
        INSERT INTO door_reader_served(client,resource,mailbox,id,seq,ref,generation)
        SELECT s.client,s.resource,s.mailbox,'fixture-'||x,1000+x,'fixture-'||x,s.generation FROM door_reader_stream s,n;
        UPDATE ships SET retired_ms=1 WHERE name='ops';").unwrap();
    let copy = delegated_ok(&fleet,&token,"show",json!({"ship":"liaison","id":"same-durable-id"}));
    assert!(copy["ref"].is_string(),"removed homes must not suppress copies");
    assert_ne!(copy["ref"],first["ref"]);
    assert_eq!(registry.query_row("SELECT COUNT(*) FROM door_reader_served",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    delegated_ok(&fleet,&token,"ack",json!({"ship":"liaison","refs":[copy["ref"]]}));
    assert!(delegated_ok(&fleet,&token,"inbox",json!({"ship":"liaison"}))["items"].as_array().unwrap().is_empty());
    stop(supervisor);
}

#[test]
fn scoped_reader_admits_oversize_first_item_and_refuses_cloned_mailboxes() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (_, token) = delegated_issue(&fleet,"reader","mail:read",None);
    reader_mail(&fleet,"ops","large",None);
    reader_mail(&fleet,"liaison","identity",None);
    let ops = rusqlite::Connection::open(fleet.home("ops").join("privateer.sqlite")).unwrap();
    ops.execute("UPDATE mail SET body=?1 WHERE id='large'",["x".repeat(600*1024)]).unwrap();
    let supervisor = fleet.supervise();
    fleet.wait("reader hello", |e| e.first().cloned());
    let page = delegated_ok(&fleet,&token,"inbox",json!({"ship":"ops"}));
    assert_eq!(page["items"][0]["mail"]["id"],"large");
    assert!(page["continuation"].is_null());
    delegated_ok(&fleet,&token,"ack",json!({"ship":"ops","refs":reader_refs(&page)}));
    delegated_ok(&fleet,&token,"show",json!({"ship":"liaison","id":"identity"}));
    let identity: String = ops.query_row("SELECT value FROM metadata WHERE key='door_reader_mailbox'",[],|r|r.get(0)).unwrap();
    let liaison = rusqlite::Connection::open(fleet.home("liaison").join("privateer.sqlite")).unwrap();
    liaison.execute("UPDATE metadata SET value=?1 WHERE key='door_reader_mailbox'",[identity]).unwrap();
    credential_request(&fleet,&token,"inbox",json!({"ship":"ops"}),"execution_failed");
    credential_request(&fleet,&token,"inbox",json!({"ship":"liaison"}),"execution_failed");
    liaison.execute("UPDATE metadata SET value='distinct-mailbox' WHERE key='door_reader_mailbox'",[]).unwrap();
    let registry = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    registry.execute("INSERT INTO ships(name,home,created_by,created_ms) SELECT 'alias',home,created_by,created_ms FROM ships WHERE name='ops'",[]).unwrap();
    credential_request(&fleet,&token,"inbox",json!({"ship":"ops"}),"execution_failed");
    stop(supervisor);
}
