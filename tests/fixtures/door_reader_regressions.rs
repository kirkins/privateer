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
    stop(supervisor);
}
