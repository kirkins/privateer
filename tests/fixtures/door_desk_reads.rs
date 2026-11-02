// PR5 real supervisor/core protocol acceptance. Projection fixtures intentionally
// have no native newness proof; only incoming open requests may stream.
#[allow(clippy::too_many_arguments)]
fn desk_read_fact(fleet: &Fleet, id: &str, order: i64, ship: &str, incoming: bool, kind: &str, state: &str, body: &str) {
    let conn = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    let raw: String = conn.query_row("SELECT record FROM desk_placement_record", [], |r| r.get(0)).unwrap();
    let record: Value = serde_json::from_str(&raw).unwrap();
    let m = &record["proposal"]["manifest"];
    conn.execute("INSERT INTO desk_intake_fact(domain,owner_core,generation,incarnation,source,core,mailbox,id,source_order,content,state,unread,pending) VALUES(?1,?2,1,?3,?2,?2,'00000000-0000-4000-8000-000000000001',?4,?5,?6,?7,1,1)",rusqlite::params![m["domain"].as_str(),m["owner_core"].as_str(),m["incarnation"].as_str(),id,order,
        json!({"id":id,"from":if incoming {ship} else {"captain"},"to":if incoming {"captain"} else {ship},"kind":kind,"body":body,"correlation":"alpha"}).to_string(),state]).unwrap();
}

fn desk_read_exchange_id(fleet: &Fleet, token: &str, id: Value) -> Value {
    use std::io::Write;
    let mut file = fs::OpenOptions::new().create(true).append(true).open(fleet.root.join("requests.jsonl")).unwrap();
    writeln!(file,"{}",json!({"type":"request","id":id,"command":"desk.subscription.open","args":{},"auth":{"token":token}})).unwrap();
    let response_id = if id.to_string().len() > 256 { Value::Null } else { id };
    fleet.wait("bounded desk id", |entries| entries.iter().find(|e| e["type"] == "response" && e["id"] == response_id).cloned())
}

#[test]
fn scoped_desk_reads_recover_lost_pages_and_preserve_visibility_and_legacy_state() {
    let fleet = Fleet::with_ships(&[("alpha",true),("beta",false)]);
    delegated_adapter(&fleet);
    desk_exposure_owner(&fleet);
    fleet.call("alpha","door.desk.enable",desk_exposure_args());
    desk_read_fact(&fleet,"alpha",1,"alpha",true,"request","open","alpha request");
    desk_read_fact(&fleet,"beta",2,"beta",true,"decision","open","beta decision");
    desk_read_fact(&fleet,"gamma",3,"alpha",false,"request","open","old outgoing must not stream");
    desk_read_fact(&fleet,"alpha-notice",4,"alpha",true,"notice","closed","unknown notice must not stream");
    let (grant,token) = delegated_issue(&fleet,"alpha","mail:read",Some("alpha"));
    let (_,other) = delegated_issue(&fleet,"beta","mail:read",None);
    let (_,metadata) = delegated_issue(&fleet,"gamma","fleet:read,captain:presence",None);
    let conn = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    let legacy = || {
        let home = rusqlite::Connection::open(fleet.home("alpha").join("privateer.sqlite")).unwrap();
        home.prepare("SELECT name,sql FROM sqlite_master WHERE name LIKE 'door_%' ORDER BY name").unwrap().query_map([], |r| Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()
    };
    let before = legacy();
    let supervisor = fleet.supervise();
    fleet.wait("desk reads hello",|e|e.first().cloned());
    assert_eq!(delegated_exchange(&fleet,&metadata,"desk.subscription.open",json!({}))["ok"],false);
    let open = delegated_ok(&fleet,&token,"desk.subscription.open",json!({}));
    assert_eq!(open["schema"],"desk.subscription.v1");
    let sub = open["subscription"].as_str().unwrap();
    assert_eq!(delegated_ok(&fleet,&token,"desk.subscription.open",json!({}))["subscription"],sub);
    let other_open = delegated_ok(&fleet,&other,"desk.subscription.open",json!({}));
    assert_ne!(other_open["subscription"],sub);
    let foreign = delegated_ok(&fleet,&other,"desk.subscription.pull",json!({"subscription":sub}));
    assert_eq!(foreign["error"]["code"],"stale_or_unknown");
    let page = delegated_ok(&fleet,&token,"desk.subscription.pull",json!({"subscription":sub,"limit":1}));
    assert_eq!(page["items"][0]["origin"]["id"],"alpha");
    assert_eq!(page["items"][0]["delivery_class"],"unknown");
    assert_eq!(page["items"][0]["state"],"open");
    assert_eq!(page["coverage"]["complete"],false);
    assert_eq!(page["coverage"]["owner"],Value::Null);
    assert!(!page.to_string().contains("beta decision"));
    // Lost output/core restart: durable start tokens and refs, not a client
    // cursor or the adapter log, are recovery authority.
    stop(supervisor);
    fs::write(fleet.root.join("requests.jsonl"),"").unwrap();
    fs::write(&fleet.log,"").unwrap();
    let supervisor = fleet.supervise();
    fleet.wait("desk reads restarted hello",|e|e.first().cloned());
    let resume = delegated_ok(&fleet,&token,"desk.subscription.resume",json!({"subscription":sub}));
    let start = resume["pending_snapshots"][0]["continuation"].clone();
    let replay = delegated_ok(&fleet,&token,"desk.subscription.pull",json!({"subscription":sub,"continuation":start}));
    assert_eq!(replay["items"],page["items"]);
    assert_eq!(replay["coverage"],page["coverage"]);
    let mut hidden = page["items"][0]["origin"].clone(); hidden["id"] = json!("beta");
    for command in ["desk.subscription.show","desk.subscription.thread"] {
        assert_eq!(delegated_ok(&fleet,&token,command,json!({"subscription":sub,"origin":hidden}))["error"]["code"],"stale_or_unknown");
    }
    let mut excluded = hidden.clone(); excluded["id"] = json!("gamma");
    assert_eq!(delegated_ok(&fleet,&token,"desk.subscription.show",json!({"subscription":sub,"origin":excluded}))["error"]["code"],"stale_or_unknown");
    let thread = delegated_ok(&fleet,&token,"desk.subscription.thread",json!({"subscription":sub,"origin":page["items"][0]["origin"],"limit":50}));
    assert!(thread["items"].as_array().unwrap().iter().any(|i| i["origin"]["id"] == "gamma"));
    assert!(thread["items"].as_array().unwrap().iter().all(|i| i["delivery_class"] == "history"));
    let id = grant["clients"][0]["id"].as_str().unwrap();
    let (_,rotated) = client_secret(&fleet,&["door","client","rotate",id],"rotate");
    assert_eq!(delegated_ok(&fleet,&rotated,"desk.subscription.open",json!({}))["subscription"],sub);
    assert_eq!(delegated_exchange(&fleet,&token,"desk.subscription.open",json!({}))["ok"],false);
    fleet.call("alpha","door.desk.disable",json!({"door":"fake"}));
    assert_eq!(delegated_ok(&fleet,&rotated,"desk.subscription.resume",json!({"subscription":sub}))["error"]["code"],"unavailable");
    fleet.call("alpha","door.desk.enable",desk_exposure_args());
    assert_eq!(delegated_ok(&fleet,&rotated,"desk.subscription.open",json!({}))["subscription"],sub);
    assert_eq!(delegated_exchange(&fleet,&rotated,"desk.subscription.ack",json!({"subscription":sub,"ack_key":page["ack_key"],"refs":[page["items"][0]["ref"]]}))["ok"],false);
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM desk_subscription_progress",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    assert_eq!(before,legacy());
    let capabilities = delegated_ok(&fleet,&rotated,"capabilities",json!({}));
    assert!(capabilities.get("desk_subscriptions").is_none());
    stop(supervisor);
}

#[test]
fn scoped_desk_read_final_envelope_bounds_and_closed_raw_arguments() {
    let fleet = Fleet::with_ships(&[("alpha",true)]);
    delegated_adapter(&fleet); desk_exposure_owner(&fleet);
    fleet.call("alpha","door.desk.enable",desk_exposure_args());
    for i in 0..96 { desk_read_fact(&fleet,&format!("alpha-{i:03}"),i+1,"alpha",true,"request","open",&"\"\\".repeat(1750)); }
    let (_,token) = delegated_issue(&fleet,"alpha","mail:read",None);
    let supervisor = fleet.supervise(); fleet.wait("desk size hello",|e|e.first().cloned());
    let refused = desk_read_exchange_id(&fleet,&token,json!("a".repeat(255)));
    assert_eq!(refused["id"],Value::Null); assert_eq!(refused["ok"],false);
    let conn = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM desk_subscription",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    let open = desk_read_exchange_id(&fleet,&token,json!("a".repeat(254)));
    assert_eq!(open["ok"],true,"{open}");
    let sub = open["result"]["subscription"].as_str().unwrap();
    let origin = json!({"core":"00000000-0000-4000-8000-000000000001","mailbox":"00000000-0000-4000-8000-000000000001","id":"alpha"});
    for (command,args) in [
        ("desk.subscription.open",json!({"schema":"bypass"})),
        ("desk.subscription.open",json!({"idempotency_key":"alpha"})),
        ("desk.subscription.pull",json!({"subscription":sub,"limit":51})),
        ("desk.subscription.pull",json!({"subscription":sub,"continuation":""})),
        ("desk.subscription.show",json!({"subscription":sub,"origin":{"core":origin["core"],"mailbox":origin["mailbox"],"id":"alpha","owner":"alpha"}})),
    ] { assert_eq!(delegated_exchange(&fleet,&token,command,args)["ok"],false); }
    // The largest admitted ID accompanies a near-cap page, not just open.
    use std::io::Write;
    let id = json!("b".repeat(254));
    let mut file = fs::OpenOptions::new().append(true).open(fleet.root.join("requests.jsonl")).unwrap();
    writeln!(file,"{}",json!({"type":"request","id":id,"command":"desk.subscription.pull","args":{"subscription":sub},"auth":{"token":token}})).unwrap();
    let page = fleet.wait("maximal framed page",|e|e.iter().find(|e|e["id"] == id).cloned());
    assert_eq!(page["result"]["ok"],true,"{page}");
    let bytes = serde_json::to_vec(&page).unwrap().len();
    assert!(bytes <= 192*1024 && bytes > 170*1024,"{bytes}");
    let stale = delegated_ok(&fleet,&token,"desk.subscription.pull",json!({"subscription":sub,"continuation":"alpha"}));
    assert_eq!(stale["error"]["code"],"stale_or_unknown");
    delegated_ok(&fleet,&token,"desk.subscription.pull",json!({"subscription":sub}));
    let pressure = delegated_ok(&fleet,&token,"desk.subscription.pull",json!({"subscription":sub}));
    assert_eq!(pressure["error"]["code"],"backpressure");
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM desk_subscription_snapshot",[],|r|r.get::<_,i64>(0)).unwrap(),2);
    stop(supervisor);
}
