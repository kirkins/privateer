// PR6: real supervisor/core boundary, independent of adapter-side replay.
fn desk_ack_rows(conn: &rusqlite::Connection, table: &str, predicate: &str) -> Vec<Vec<String>> {
    let mut stmt = conn.prepare(&format!("SELECT * FROM {table} {predicate} ORDER BY rowid")).unwrap();
    let columns = stmt.column_count();
    stmt.query_map([], |r| (0..columns).map(|i| r.get_ref(i).map(|v|format!("{v:?}"))).collect::<rusqlite::Result<Vec<_>>>()).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()
}

#[test]
fn scoped_desk_ack_is_atomic_replays_after_restart_and_releases_only_its_own_custody() {
    let fleet = Fleet::with_ships(&[("alpha",true)]);
    delegated_adapter(&fleet); desk_exposure_owner(&fleet);
    fleet.call("alpha","door.desk.enable",desk_exposure_args());
    desk_read_fact(&fleet,"alpha",1,"alpha",true,"request","open","alpha request");
    desk_read_fact(&fleet,"beta",2,"alpha",true,"decision","open","beta decision");
    let (grant,token) = delegated_issue(&fleet,"alpha","mail:read",None);
    let (_,other) = delegated_issue(&fleet,"beta","mail:read",None);
    let (_,metadata) = delegated_issue(&fleet,"gamma","fleet:read,captain:presence",None);
    let conn = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    let home = rusqlite::Connection::open(fleet.home("alpha").join("privateer.sqlite")).unwrap();
    home.execute_batch("CREATE TABLE IF NOT EXISTS door_cursor(door TEXT PRIMARY KEY,through_seq INTEGER NOT NULL); INSERT INTO door_cursor VALUES('alpha',7); INSERT OR REPLACE INTO mail_cursor(address,through_seq) VALUES('captain',3);").unwrap();
    let legacy = || ["door_cursor","mail_cursor","mail"].map(|table|desk_ack_rows(&home,table,""));
    let before = legacy();
    let supervisor = fleet.supervise(); fleet.wait("desk ack hello",|e|e.first().cloned());
    let opened = delegated_ok(&fleet,&token,"desk.subscription.open",json!({}));
    let sub = opened["subscription"].as_str().unwrap();
    let other_open = delegated_ok(&fleet,&other,"desk.subscription.open",json!({}));
    let other_sub = other_open["subscription"].as_str().unwrap();
    let other_page = delegated_ok(&fleet,&other,"desk.subscription.pull",json!({"subscription":other_sub}));
    let other_state = desk_ack_rows(&conn,"desk_subscription_exception",&format!("WHERE subscription='{other_sub}'"));
    let page = delegated_ok(&fleet,&token,"desk.subscription.pull",json!({"subscription":sub}));
    let refs: Vec<_> = page["items"].as_array().unwrap().iter().map(|i|i["ref"].clone()).collect();
    assert_eq!(refs.len(),2);
    let ack = json!({"subscription":sub,"ack_key":page["ack_key"],"refs":refs});
    assert_eq!(delegated_exchange(&fleet,&metadata,"desk.subscription.ack",ack.clone())["ok"],false);
    assert_eq!(delegated_exchange(&fleet,&token,"desk.subscription.ack",json!({"subscription":sub,"ack_key":page["ack_key"],"refs":refs,"idempotency_key":"alpha"}))["ok"],false);
    // Foreign reader, foreign receipt, forged refs, mixed history/automatic and
    // selected-mailbox-looking refs must have no partial automatic effect.
    let history = delegated_ok(&fleet,&token,"desk.subscription.thread",json!({"subscription":sub,"origin":page["items"][0]["origin"]}));
    let automatic = || ["desk_subscription_ref","desk_subscription_progress","desk_subscription_exception","desk_subscription_pin"].map(|table|desk_ack_rows(&conn,table,&format!("WHERE subscription='{sub}'")));
    let custody = automatic();
    for (key,batch) in [
        (page["ack_key"].clone(),json!([refs[0],"forged"])),
        (page["ack_key"].clone(),json!([refs[0],history["items"][0]["ref"]])),
        (page["ack_key"].clone(),json!([other_page["items"][0]["ref"]])),
        (page["ack_key"].clone(),json!(["mail:alpha:1"])),
        (other_page["ack_key"].clone(),json!([refs[0]])),
    ] {
        let refusal = delegated_ok(&fleet,&token,"desk.subscription.ack",json!({"subscription":sub,"ack_key":key,"refs":batch}));
        assert_eq!(refusal["ok"],false,"{refusal}");
        assert_eq!(custody,automatic());
    }
    // Explicit history ack cannot advance the automatic stream or release pins.
    let history_refs: Vec<_> = history["items"].as_array().unwrap().iter().map(|i|i["ref"].clone()).collect();
    assert_eq!(delegated_ok(&fleet,&token,"desk.subscription.ack",json!({"subscription":sub,"ack_key":history["ack_key"],"refs":history_refs}))["operation_complete"],true);
    assert_eq!(custody,automatic());
    // Duplicate refs and batch ordering normalize to one retained outcome.
    let mut duplicate = ack.clone(); duplicate["refs"] = json!([refs[1],refs[0],refs[1]]);
    let applied = delegated_ok(&fleet,&token,"desk.subscription.ack",duplicate);
    assert_eq!(applied["acknowledged"],2); assert_eq!(applied["operation_complete"],true);
    assert!(desk_ack_rows(&conn,"desk_subscription_exception",&format!("WHERE subscription='{sub}'")).is_empty());
    // Open-offer pins remain for closures; ack does not close the source request.
    assert_eq!(desk_ack_rows(&conn,"desk_subscription_pin",&format!("WHERE subscription='{sub}'")).len(),2);
    assert_eq!(other_state,desk_ack_rows(&conn,"desk_subscription_exception",&format!("WHERE subscription='{other_sub}'")));
    assert_eq!(before,legacy());
    // Treat success output as lost. A new core process replays only the retained
    // server key/batch; the generic write-results table must remain untouched.
    stop(supervisor); fs::write(fleet.root.join("requests.jsonl"),"").unwrap(); fs::write(&fleet.log,"").unwrap();
    let supervisor = fleet.supervise(); fleet.wait("desk ack restart",|e|e.first().cloned());
    let replay = delegated_ok(&fleet,&token,"desk.subscription.ack",ack.clone());
    assert_eq!(replay["acknowledged"],2);
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM door_write_results WHERE command='desk.subscription.ack'",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    assert_eq!(delegated_ok(&fleet,&token,"desk.subscription.ack",json!({"subscription":sub,"ack_key":page["ack_key"],"refs":[refs[0]]}))["error"]["code"],"invalid_ack");
    conn.execute("UPDATE desk_subscription_receipt SET expires=0 WHERE key=?1",[page["ack_key"].as_str().unwrap()]).unwrap();
    assert_eq!(delegated_ok(&fleet,&token,"desk.subscription.ack",ack.clone())["error"]["code"],"stale_or_unknown");
    // Closure observation follows the offered-open pin; its ack retires that pin.
    conn.execute("UPDATE desk_intake_fact SET state='closed',source_order=3 WHERE id='alpha'",[]).unwrap();
    // Expire snapshots explicitly to free the two-snapshot admission bound.
    conn.execute("UPDATE desk_subscription_snapshot SET expires=0 WHERE subscription=?1",[sub]).unwrap();
    let closure = delegated_ok(&fleet,&token,"desk.subscription.pull",json!({"subscription":sub}));
    assert_eq!(closure["items"][0]["state"],"closed");
    assert_eq!(delegated_ok(&fleet,&token,"desk.subscription.ack",json!({"subscription":sub,"ack_key":closure["ack_key"],"refs":[closure["items"][0]["ref"]]}))["acknowledged"],1);
    assert_eq!(desk_ack_rows(&conn,"desk_subscription_pin",&format!("WHERE subscription='{sub}'")).len(),1);
    assert_eq!(other_state,desk_ack_rows(&conn,"desk_subscription_exception",&format!("WHERE subscription='{other_sub}'")));
    assert_eq!(before,legacy());
    // A visibility incarnation change never translates stale refs into an ack.
    let id = grant["clients"][0]["id"].as_str().unwrap();
    conn.execute("UPDATE door_clients SET ships='[\"alpha\"]' WHERE id=?1",[id]).unwrap();
    let resumed = delegated_ok(&fleet,&token,"desk.subscription.resume",json!({"subscription":sub}));
    assert_ne!(resumed["incarnation"],opened["incarnation"]);
    assert_eq!(delegated_ok(&fleet,&token,"desk.subscription.ack",ack)["error"]["code"],"stale_or_unknown");
    assert!(delegated_ok(&fleet,&token,"capabilities",json!({})).get("desk_subscriptions").is_none());
    stop(supervisor);
}
