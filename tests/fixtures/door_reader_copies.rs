#[test]
fn scoped_reader_deduplicates_local_desk_copies_without_unbounded_tombstones() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (_, full) = delegated_issue(&fleet,"reader","mail:read",None);
    let (_, other) = delegated_issue(&fleet,"independent","mail:read",None);
    let ops = rusqlite::Connection::open(fleet.home("ops").join("privateer.sqlite")).unwrap();
    let liaison = rusqlite::Connection::open(fleet.home("liaison").join("privateer.sqlite")).unwrap();
    for n in 0..350 {
        for conn in [&ops,&liaison] {
            conn.execute("INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms) VALUES(?1,'ops','captain','notice','Same durable mail','{}','normal',1)",[format!("copy-{n}")]).unwrap();
        }
    }
    let supervisor = fleet.supervise();
    fleet.wait("copy reader hello", |e| e.first().cloned());
    let mut token = Value::Null;
    for _ in 0..7 {
        let mut args = json!({"ship":"ops","limit":50});
        if token.is_string() { args["continuation"] = token; }
        let page = delegated_ok(&fleet,&full,"inbox",args);
        assert_eq!(page["items"].as_array().unwrap().len(),50);
        token = page["continuation"].clone();
        delegated_ok(&fleet,&full,"ack",json!({"ship":"ops","refs":reader_refs(&page)}));
    }
    assert!(token.is_null());
    // Sparse served rows have compacted, but the origin mailbox cursor and
    // immutable IDs still prove these copies were acknowledged by this grant.
    let registry = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    assert_eq!(registry.query_row("SELECT COUNT(*) FROM door_reader_served",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    let suppressed = delegated_ok(&fleet,&full,"inbox",json!({"ship":"liaison","limit":25}));
    assert!(suppressed["items"].as_array().unwrap().is_empty());
    assert!(suppressed["continuation"].is_string(),"bounded duplicate-only pages must explicitly continue");
    let tail = delegated_ok(&fleet,&full,"inbox",json!({"ship":"liaison","limit":25,"continuation":suppressed["continuation"]}));
    assert!(tail["items"].as_array().unwrap().is_empty());
    assert!(tail["continuation"].is_null());
    let independent = delegated_ok(&fleet,&other,"inbox",json!({"ship":"liaison","limit":1}));
    assert_eq!(independent["items"][0]["mail"]["id"],"copy-0");
    // References are also deduplicated before acknowledgment across homes.
    for ship in ["ops","liaison"] { reader_mail(&fleet,ship,"unacked-copy",None); }
    liaison.execute("UPDATE mail SET sender='ops' WHERE id='unacked-copy'",[]).unwrap();
    let primary = delegated_ok(&fleet,&full,"show",json!({"ship":"ops","id":"unacked-copy"}));
    assert!(primary["ref"].is_string());
    assert!(delegated_ok(&fleet,&full,"show",json!({"ship":"liaison","id":"unacked-copy"}))["ref"].is_null());
    credential_request(&fleet,&full,"ack",json!({"ship":"liaison","refs":[primary["ref"]]}),"execution_failed");
    reader_mail(&fleet,"liaison","after-copies",None);
    let mut page = delegated_ok(&fleet,&full,"inbox",json!({"ship":"liaison","limit":25}));
    while page["continuation"].is_string() {
        page = delegated_ok(&fleet,&full,"inbox",json!({"ship":"liaison","limit":25,"continuation":page["continuation"]}));
    }
    assert_eq!(page["items"][0]["mail"]["id"],"after-copies");
    delegated_ok(&fleet,&full,"ack",json!({"ship":"liaison","refs":reader_refs(&page)}));
    delegated_ok(&fleet,&full,"ack",json!({"ship":"ops","refs":[primary["ref"]]}));
    // Compacting a prior cross-home acknowledgment during a later read does
    // not acknowledge unseen mail, and must not strand acked sparse state.
    assert!(delegated_ok(&fleet,&full,"inbox",json!({"ship":"liaison"}))["items"].as_array().unwrap().is_empty());
    let rows: i64 = registry.query_row("SELECT COUNT(*) FROM door_reader_served WHERE client IN (SELECT client FROM door_reader_stream WHERE through_seq>350)",[],|r|r.get(0)).unwrap();
    assert_eq!(rows,0);
    stop(supervisor);
}
