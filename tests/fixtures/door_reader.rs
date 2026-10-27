// Real subprocess/supervisor coverage for the local grant-owned pull reader.
fn reader_mail(fleet: &Fleet, ship: &str, id: &str, correlation: Option<&str>) {
    let mut args = json!({"id":id,"from":ship,"to":"captain","kind":"notice","body":format!("Mail {id}")});
    if let Some(correlation) = correlation { args["correlation"] = json!(correlation); }
    fleet.call(ship, "mail.send", args);
}

fn reader_refs(page: &Value) -> Value {
    json!(page["items"].as_array().unwrap().iter().filter_map(|i| i["ref"].as_str()).collect::<Vec<_>>())
}

#[test]
fn scoped_pull_mail_paging_binding_independent_grants_and_restart() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (client, first) = delegated_issue(&fleet, "first", "mail:read", Some("ops"));
    let (_, second) = delegated_issue(&fleet, "second", "mail:read,fleet:read", None);
    let (_, empty) = delegated_issue(&fleet, "no-mail", "fleet:read", Some("ops"));
    for id in ["m1","m2","m3","hidden-sender"] { reader_mail(&fleet,"ops",id,None); }
    reader_mail(&fleet,"liaison","hidden-home",None);
    let ops = rusqlite::Connection::open(fleet.home("ops").join("privateer.sqlite")).unwrap();
    ops.execute("UPDATE mail SET sender='liaison' WHERE id='hidden-sender'", []).unwrap();
    let supervisor = fleet.supervise();
    fleet.wait("reader hello", |e| e.first().cloned());
    for (command,args) in [
        ("inbox",json!({"ship":"ops"})), ("show",json!({"ship":"ops","id":"m1"})),
        ("thread",json!({"ship":"ops","id":"m1"})), ("ack",json!({"ship":"ops","refs":["forged"]}))
    ] { credential_request(&fleet,&empty,command,args,"insufficient_scope"); }
    for command in ["show","thread"] {
        let hidden = delegated_exchange(&fleet,&first,command,json!({"ship":"ops","id":"hidden-sender"}));
        let missing = delegated_exchange(&fleet,&first,command,json!({"ship":"ops","id":"missing"}));
        assert_eq!(hidden["error"],missing["error"]);
        credential_request(&fleet,&first,command,json!({"ship":"liaison","id":"hidden-home"}),"execution_failed");
    }
    let page1 = delegated_ok(&fleet,&first,"inbox",json!({"ship":"ops","limit":2}));
    assert_eq!(page1["items"][0]["mail"]["id"],"m1");
    assert_eq!(page1["items"][1]["mail"]["id"],"m2");
    assert!(page1["continuation"].is_string());
    assert_eq!(page1["coverage"],json!({"schema":"coverage.v1","scope":"selected-mailbox","complete":true,
        "machines":[{"machine":"here","state":"complete","complete":true,"retry":null}],
        "other_machines":"not_queried"}));
    let again = delegated_ok(&fleet,&first,"inbox",json!({"ship":"ops","limit":2}));
    assert_eq!(reader_refs(&page1),reader_refs(&again),"list never acknowledges or reallocates refs");
    let registry = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    let cursor = || registry.query_row("SELECT through_seq FROM door_reader_stream WHERE client=?1", [client["clients"][0]["id"].as_str().unwrap()], |r| r.get::<_,i64>(0)).unwrap();
    assert_eq!(cursor(),0);
    let no_legacy: i64 = ops.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name IN ('door_cursor')",[],|r|r.get(0)).unwrap();
    assert_eq!(no_legacy,0);
    // A real future/unserved mail ID is not a carried ref. A forged batch must
    // roll back its valid first ref too. Scope, home and token binding persist.
    for bad in ["forged","m3","999999999999"] {
        credential_request(&fleet,&first,"ack",json!({"ship":"ops","refs":[page1["items"][0]["ref"],bad]}),"execution_failed");
        assert_eq!(cursor(),0);
    }
    credential_request(&fleet,&second,"ack",json!({"ship":"ops","refs":reader_refs(&page1)}),"execution_failed");
    credential_request(&fleet,&second,"ack",json!({"ship":"liaison","refs":reader_refs(&page1)}),"execution_failed");
    credential_request(&fleet,&second,"inbox",json!({"ship":"ops","continuation":page1["continuation"]}),"continuation_invalid");
    credential_request(&fleet,&first,"inbox",json!({"ship":"ops","limit":51}),"unsupported_arguments");
    credential_request(&fleet,&first,"inbox",json!({"ship":"ops","continuation":"forged"}),"continuation_invalid");
    // New arrivals cannot change a continuation's frozen upper boundary.
    reader_mail(&fleet,"ops","m4",None);
    let page2 = delegated_ok(&fleet,&first,"inbox",json!({"ship":"ops","limit":2,"continuation":page1["continuation"]}));
    assert_eq!(page2["items"].as_array().unwrap().len(),1);
    assert_eq!(page2["items"][0]["mail"]["id"],"m3");
    assert!(page2["continuation"].is_null());
    // Acking a shown future mail cannot consume m1/m2/m3.
    let shown = delegated_ok(&fleet,&first,"show",json!({"ship":"ops","id":"m4"}));
    delegated_ok(&fleet,&first,"ack",json!({"ship":"ops","refs":[shown["ref"]]}));
    assert_eq!(cursor(),0);
    credential_request(&fleet,&first,"ack",json!({"ship":"ops","refs":[shown["ref"]]}),"execution_failed");
    delegated_ok(&fleet,&first,"ack",json!({"ship":"ops","refs":reader_refs(&page1)}));
    assert!(cursor()>0);
    let independent = delegated_ok(&fleet,&second,"inbox",json!({"ship":"ops"}));
    assert_eq!(independent["items"].as_array().unwrap().len(),5);
    stop(supervisor);
    fs::write(fleet.root.join("requests.jsonl"),"").unwrap();
    fs::write(&fleet.log,"").unwrap();
    let supervisor = fleet.supervise();
    fleet.wait("restarted reader hello", |e| e.first().cloned());
    let resumed_page = delegated_ok(&fleet,&first,"inbox",json!({"ship":"ops","continuation":page1["continuation"]}));
    assert_eq!(resumed_page["items"][0]["ref"],page2["items"][0]["ref"]);
    let recovered = delegated_ok(&fleet,&first,"inbox",json!({"ship":"ops"}));
    assert_eq!(recovered["items"].as_array().unwrap().len(),1);
    assert_eq!(recovered["items"][0]["ref"],page2["items"][0]["ref"]);
    delegated_ok(&fleet,&first,"ack",json!({"ship":"ops","refs":reader_refs(&page2)}));
    assert!(delegated_ok(&fleet,&first,"inbox",json!({"ship":"ops"}))["items"].as_array().unwrap().is_empty());
    credential_request(&fleet,&first,"ack",json!({"ship":"ops","refs":reader_refs(&page1)}),"execution_failed");
    // Historical show is still readable but cannot produce a stale ack ref.
    assert!(delegated_ok(&fleet,&first,"show",json!({"ship":"ops","id":"m1"}))["ref"].is_null());
    // Continuations also survive a restart (and are not acknowledgments).
    let tail = delegated_ok(&fleet,&second,"inbox",json!({"ship":"ops","limit":1}));
    credential_request(&fleet,&second,"inbox",json!({"ship":"liaison","continuation":tail["continuation"]}),"continuation_invalid");
    registry.execute("UPDATE door_reader_pages SET created_ms=0",[]).unwrap();
    credential_request(&fleet,&second,"inbox",json!({"ship":"ops","continuation":tail["continuation"]}),"continuation_invalid");
    stop(supervisor);
}

#[test]
fn scoped_reader_thread_digest_and_bounded_sparse_windows() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (_, full) = delegated_issue(&fleet,"full",ALL_DELEGATED_SCOPES,None);
    let (other_client, other) = delegated_issue(&fleet,"other",ALL_DELEGATED_SCOPES,Some("ops"));
    reader_mail(&fleet,"ops","root",None);
    for id in ["t1","t2","t3"] { reader_mail(&fleet,"ops",id,Some("root")); }
    let supervisor = fleet.supervise();
    fleet.wait("thread reader hello", |e| e.first().cloned());
    let first = delegated_ok(&fleet,&full,"thread",json!({"ship":"ops","id":"root","limit":2}));
    let next = delegated_ok(&fleet,&full,"thread",json!({"ship":"ops","id":"root","limit":2,"continuation":first["continuation"]}));
    assert_eq!(first["items"][0]["mail"]["id"],"root");
    assert_eq!(next["items"][0]["mail"]["id"],"t2");
    assert!(next["continuation"].is_null());
    credential_request(&fleet,&full,"thread",json!({"ship":"ops","id":"t1","continuation":"forged"}),"continuation_invalid");
    delegated_ok(&fleet,&full,"ack",json!({"ship":"ops","refs":reader_refs(&next)}));
    assert_eq!(delegated_ok(&fleet,&full,"inbox",json!({"ship":"ops"}))["items"].as_array().unwrap().len(),2);
    delegated_ok(&fleet,&full,"ack",json!({"ship":"ops","refs":reader_refs(&first)}));
    // Summary and raw inbox delivery are independent even for the same grant.
    delegated_ok(&fleet,&full,"away",json!({"action":"start","words":"out"}));
    reader_mail(&fleet,"ops","digest-me",None);
    let digest = delegated_ok(&fleet,&full,"digest",json!({}));
    assert!(digest["ref"].is_string());
    credential_request(&fleet,&other,"ack",json!({"refs":[digest["ref"]]}),"execution_failed");
    credential_request(&fleet,&full,"ack",json!({"ship":"ops","refs":[digest["ref"]]}),"execution_failed");
    credential_request(&fleet,&full,"ack",json!({"refs":[digest["digest"]["id"]]}),"execution_failed");
    delegated_ok(&fleet,&full,"ack",json!({"refs":[digest["ref"]]}));
    credential_request(&fleet,&full,"ack",json!({"refs":[digest["ref"]]}),"execution_failed");
    assert!(delegated_ok(&fleet,&full,"digest",json!({}))["digest"].is_null());
    // An unacked digest remains retrievable after return/new periods rather
    // than becoming inaccessible pending state that fills the reader window.
    reader_mail(&fleet,"ops","old-digest",None);
    let old = delegated_ok(&fleet,&full,"digest",json!({}));
    delegated_ok(&fleet,&full,"away",json!({"action":"back"}));
    let after_return = delegated_ok(&fleet,&full,"digest",json!({}));
    assert_eq!(after_return["away"],false);
    assert_eq!(after_return["digest"],old["digest"]);
    assert_eq!(after_return["ref"],old["ref"]);
    delegated_ok(&fleet,&full,"ack",json!({"refs":[old["ref"]]}));
    let raw = delegated_ok(&fleet,&full,"inbox",json!({"ship":"ops"}));
    assert_eq!(raw["items"][0]["mail"]["id"],"digest-me");
    delegated_ok(&fleet,&full,"ack",json!({"ship":"ops","refs":reader_refs(&raw)}));
    // Populate immutable records efficiently; all reads/acks still cross the
    // subprocess boundary. Sparse threads can use only half the grant window.
    let ops = rusqlite::Connection::open(fleet.home("ops").join("privateer.sqlite")).unwrap();
    for n in 0..300 {
        ops.execute("INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,correlation) VALUES(?1,'ops','captain','notice','Window fixture','{}','normal',1,'sparse-root')",[format!("window-{n}")]).unwrap();
    }
    let mut token = Value::Null;
    for limit in [50,50,28] {
        let mut args = json!({"ship":"ops","id":"window-0","limit":limit});
        if token.is_string() { args["continuation"] = token; }
        let page = delegated_ok(&fleet,&full,"thread",args);
        token = page["continuation"].clone();
        delegated_ok(&fleet,&full,"ack",json!({"ship":"ops","refs":reader_refs(&page)}));
    }
    // Those were a contiguous prefix, so they compacted. Fill sparse state
    // with future show reads and leave the earliest unseen record as a hole.
    for n in 129..257 {
        let shown = delegated_ok(&fleet,&full,"show",json!({"ship":"ops","id":format!("window-{n}")}));
        delegated_ok(&fleet,&full,"ack",json!({"ship":"ops","refs":[shown["ref"]]}));
    }
    credential_request(&fleet,&full,"show",json!({"ship":"ops","id":"window-257"}),"reader_window_full");
    // Reserved inbox headroom allows the gap to close despite sparse pressure.
    let gap = delegated_ok(&fleet,&full,"inbox",json!({"ship":"ops","limit":1}));
    assert_eq!(gap["items"][0]["mail"]["id"],"window-128");
    delegated_ok(&fleet,&full,"ack",json!({"ship":"ops","refs":reader_refs(&gap)}));
    let after = delegated_ok(&fleet,&full,"inbox",json!({"ship":"ops","limit":50}));
    assert_eq!(after["items"][0]["mail"]["id"],"window-257");
    // Bounded continuation retention, independent of mailbox content size.
    for _ in 0..36 { delegated_ok(&fleet,&other,"inbox",json!({"ship":"ops","limit":1})); }
    let registry = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    let max: i64 = registry.query_row("SELECT MAX(n) FROM (SELECT COUNT(*) n FROM door_reader_pages GROUP BY client,resource)",[],|r|r.get(0)).unwrap();
    assert!(max<=32);
    // A reader that never acknowledges is not capped at 256 items (#490): the
    // oldest unacknowledged refs are forgotten instead. Such mail stays
    // unacknowledged and a fresh list serves it again under a new ref.
    let incoming: i64 = ops.query_row("SELECT COUNT(*) FROM mail WHERE recipient='captain'",[],|r|r.get(0)).unwrap();
    let mut token = Value::Null;
    let mut first_page = Value::Null;
    let mut ids = Vec::new();
    loop {
        let mut args = json!({"ship":"ops","limit":50});
        if token.is_string() { args["continuation"] = token; }
        let page = delegated_ok(&fleet,&other,"inbox",args);
        if first_page.is_null() { first_page = page.clone(); }
        ids.extend(page["items"].as_array().unwrap().iter().map(|i| i["mail"]["id"].as_str().unwrap().to_owned()));
        token = page["continuation"].clone();
        if token.is_null() { break; }
    }
    assert_eq!(ids.len() as i64,incoming,"a long mailbox pages to its end without acks");
    assert_eq!(ids.iter().collect::<std::collections::HashSet<_>>().len(),ids.len(),"no repeats across pages");
    let outstanding: i64 = registry.query_row("SELECT COUNT(*) FROM door_reader_served WHERE client=?1",[other_client["clients"][0]["id"].as_str().unwrap()],|r|r.get(0)).unwrap();
    assert!(outstanding<=256,"window stays bounded: {outstanding}");
    credential_request(&fleet,&other,"ack",json!({"ship":"ops","refs":reader_refs(&first_page)}),"execution_failed");
    let relisted = delegated_ok(&fleet,&other,"inbox",json!({"ship":"ops","limit":1}));
    assert_eq!(relisted["items"][0]["mail"]["id"],first_page["items"][0]["mail"]["id"],"evicted mail is served again");
    assert_ne!(relisted["items"][0]["ref"],first_page["items"][0]["ref"],"under a new ref, never a revived one");
    delegated_ok(&fleet,&other,"ack",json!({"ship":"ops","refs":reader_refs(&relisted)}));
    stop(supervisor);
}
