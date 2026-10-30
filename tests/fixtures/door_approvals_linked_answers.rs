// Answer fixtures use the real supervised door and operator gateway. Faults
// discard real acknowledgments or edit authority after the origin has committed.
fn linked_chat_setup(linked: &LinkedChat) -> String {
    delegated_adapter(&linked.fleet);
    let (grant, token) = delegated_issue(&linked.fleet, "beta", "approval:answer", Some("beta"));
    linked.wrapper(grant["clients"][0]["id"].as_str().unwrap());
    token
}
fn linked_native(linked: &LinkedChat, prepared: &Value) -> Value {
    let listed = linked.remote_call("captain_approval.list", json!({"ship":"beta"}));
    let rows = listed["approvals"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{listed}");
    assert_eq!(rows[0]["sentence"], prepared["grant"]["sentence"]);
    assert!(rows[0]["sentence"].as_str().unwrap().contains("via the linked chat door"));
    assert!(rows[0]["provenance"]["link"].is_object());
    rows[0].clone()
}

#[test]
fn chat_linked_approve_lost_reply_and_restart_recover_the_same_receipt() {
    let linked = LinkedChat::new(); let fleet = &linked.fleet;
    let token = linked_chat_setup(&linked);
    let supervisor = fleet.supervise(); fleet.wait("linked answer hello", |e| e.first().cloned());
    let prepared = delegated_ok(fleet, &token, "approval", linked.prepare());
    let mut answer = chat_answer(&prepared, "approve");
    answer["note"] = json!("note-canary-do-not-deliver");
    fs::write(&linked.mode, "lost").unwrap();
    chat_refused(fleet, &token, answer.clone(), "outcome_unknown");
    let native = linked_native(&linked, &prepared);
    let raw: String = linked.remote_store().query_row("SELECT receipt FROM door_linked_approval_receipts", [], |r| r.get(0)).unwrap();
    let saved: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(saved["receipt"]["approval_id"], native["id"]);
    // A new door and new gateway process recover after commit, without needing
    // the original mail reply (receipts have independent durable custody).
    stop(supervisor);
    linked.remote_store().execute("DELETE FROM mail WHERE kind='reply'", []).unwrap();
    fs::write(fleet.root.join("requests.jsonl"), "").unwrap(); fs::write(&fleet.log, "").unwrap();
    fs::write(&linked.mode, "live").unwrap();
    let supervisor = fleet.supervise(); fleet.wait("linked recovered hello", |e| e.first().cloned());
    let recovered = delegated_ok(fleet, &token, "approval", answer.clone());
    assert_eq!(recovered["receipt"], saved["receipt"]);
    // An internal receiver error recovering an existing receipt must not be
    // presented as a definite refusal of an already committed approval.
    linked.remote_store().execute("UPDATE door_linked_approval_receipts SET receipt='malformed'", []).unwrap();
    chat_refused(fleet, &token, answer.clone(), "outcome_unknown");
    linked.remote_store().execute("UPDATE door_linked_approval_receipts SET receipt=?1", [&raw]).unwrap();
    answer["note"] = json!("different ignored commentary");
    assert_eq!(delegated_ok(fleet, &token, "approval", answer)["receipt"], saved["receipt"]);
    chat_refused(fleet, &token, chat_answer(&prepared, "deny"), "approval_stale");
    linked_native(&linked, &prepared);
    for text in [fs::read_to_string(&linked.calls).unwrap(), fs::read_to_string(&fleet.log).unwrap(), raw] {
        assert!(!text.contains(&token));
        // The explicit note is in the request wire only, never receipts/audits.
    }
    let peer = linked.remote_store();
    for table in ["mail", "audit", "captain_approval_door_provenance"] {
        let mut stmt = peer.prepare(&format!("SELECT * FROM {table}")).unwrap();
        let columns = stmt.column_count();
        let rows = stmt.query_map([], |r| Ok((0..columns).filter_map(|i| r.get::<_, String>(i).ok()).collect::<Vec<_>>().join(" "))).unwrap();
        for row in rows { let row = row.unwrap(); assert!(!row.contains("note-canary")); assert!(!row.contains(&token)); }
    }
    stop(supervisor);
}

#[test]
fn chat_linked_answer_revocation_pause_rotation_and_route_change_after_dispatch_are_unknown() {
    for mode in ["revoke", "pause", "rotate", "restrict", "link", "directory", "revoke_before_commit"] {
        let linked = LinkedChat::new(); let fleet = &linked.fleet;
        let token = linked_chat_setup(&linked);
        let supervisor = fleet.supervise(); fleet.wait("linked revoke hello", |e| e.first().cloned());
        let prepared = delegated_ok(fleet, &token, "approval", linked.prepare());
        fs::write(&linked.mode, mode).unwrap();
        chat_refused(fleet, &token, chat_answer(&prepared, "approve"), "outcome_unknown");
        linked_native(&linked, &prepared);
        let conn = linked.registry();
        for event in ["approval.dispatch", "approval.release"] {
            assert!(conn.query_row("SELECT EXISTS(SELECT 1 FROM door_client_audit WHERE event=?1)", [event], |r| r.get::<_,bool>(0)).unwrap(), "{mode}: {event}");
        }
        if matches!(mode, "revoke"|"pause"|"rotate"|"restrict"|"revoke_before_commit") {
            assert!(conn.query_row("SELECT EXISTS(SELECT 1 FROM door_client_audit WHERE event='approval.revoked_after_dispatch')", [], |r| r.get::<_,bool>(0)).unwrap());
        }
        stop(supervisor);
    }
}

#[test]
fn chat_linked_refusal_after_dispatch_is_hidden_if_release_authority_was_lost() {
    let linked = LinkedChat::new(); let fleet = &linked.fleet;
    let token = linked_chat_setup(&linked);
    let supervisor = fleet.supervise(); fleet.wait("linked refusal release hello", |e| e.first().cloned());
    let p = delegated_ok(fleet, &token, "approval", linked.prepare());
    linked.remote_store().execute("UPDATE mail SET request_kind='production_approval' WHERE id='ask'",[]).unwrap();
    fs::write(&linked.mode,"revoke_before_commit").unwrap();
    chat_refused(fleet,&token,chat_answer(&p,"approve"),"outcome_unknown");
    linked.assert_open();
    assert!(linked.registry().query_row("SELECT EXISTS(SELECT 1 FROM door_client_audit WHERE event='approval.revoked_after_dispatch')",[],|r|r.get::<_,bool>(0)).unwrap());
    stop(supervisor);
}

#[test]
fn chat_linked_answer_down_or_changed_link_before_dispatch_records_nothing() {
    for mode in ["down", "link", "revoke_before_dispatch"] {
        let linked = LinkedChat::new(); let fleet = &linked.fleet;
        let token = linked_chat_setup(&linked);
        let supervisor = fleet.supervise(); fleet.wait("linked unavailable hello", |e| e.first().cloned());
        let prepared = delegated_ok(fleet, &token, "approval", linked.prepare());
        if mode != "link" { fs::write(&linked.mode, mode).unwrap(); }
        else { linked.registry().execute("UPDATE machines SET incarnation='gamma' WHERE name='beta'", []).unwrap(); }
        chat_refused(fleet, &token, chat_answer(&prepared, "approve"),
            if mode == "revoke_before_dispatch" { "revoked_grant" } else { "approval_unavailable" });
        linked.assert_open();
        assert!(!linked.registry().query_row("SELECT EXISTS(SELECT 1 FROM door_client_audit WHERE event='approval.dispatch')", [], |r| r.get::<_,bool>(0)).unwrap());
        stop(supervisor);
    }
}

#[test]
fn chat_linked_two_gateway_clients_race_same_and_different_choices() {
    for different in [false, true] {
        let linked = LinkedChat::new(); let fleet = &linked.fleet;
        let token = linked_chat_setup(&linked);
        let supervisor = fleet.supervise(); fleet.wait("linked concurrency hello", |e| e.first().cloned());
        let prepared = delegated_ok(fleet, &token, "approval", linked.prepare());
        let calls = fs::read_to_string(&linked.calls).unwrap();
        let mut request: Value = serde_json::from_str(calls.lines().next().unwrap()).unwrap();
        request["args"]["args"] = chat_answer(&prepared, "approve");
        let mut other = request.clone();
        if different { other["args"]["args"]["action"] = json!("deny"); }
        let barrier = std::sync::Barrier::new(2);
        let (one,two) = std::thread::scope(|scope| {
            let a = scope.spawn(|| { barrier.wait(); linked.gateway(&request, false) });
            let b = scope.spawn(|| { barrier.wait(); linked.gateway(&other, false) });
            (a.join().unwrap(),b.join().unwrap())
        });
        if different {
            assert_ne!(one["ok"], two["ok"], "{one} {two}");
            let refused = if one["ok"] == false { &one } else { &two };
            assert_eq!(refused["denial_code"], "approval_stale");
        } else {
            assert_eq!(one["ok"], true, "{one}"); assert_eq!(two["ok"], true, "{two}");
            assert_eq!(one["result"]["answered"]["receipt"],two["result"]["answered"]["receipt"]);
            linked_native(&linked, &prepared);
        }
        assert_eq!(linked.remote_store().query_row("SELECT count(*) FROM door_linked_approval_receipts", [], |r| r.get::<_,i64>(0)).unwrap(), 1);
        assert_eq!(linked.remote_store().query_row("SELECT count(*) FROM mail WHERE kind='reply'", [], |r| r.get::<_,i64>(0)).unwrap(), 1);
        stop(supervisor);
    }
}

#[test]
fn chat_linked_host_restart_between_origin_commit_and_host_release_recovers() {
    use std::io::Write;
    let linked = LinkedChat::new(); let fleet = &linked.fleet;
    let token = linked_chat_setup(&linked);
    let mut supervisor = fleet.supervise(); fleet.wait("linked restart hello", |e| e.first().cloned());
    let p = delegated_ok(fleet, &token, "approval", linked.prepare());
    fs::write(fleet.root.join("chat-supervisor"), supervisor.id().to_string()).unwrap();
    fs::write(&linked.mode,"restart").unwrap();
    let mut requests = fs::OpenOptions::new().append(true).open(fleet.root.join("requests.jsonl")).unwrap();
    writeln!(requests,"{}",json!({"type":"request","id":uuid::Uuid::new_v4().to_string(),"command":"approval",
        "args":chat_answer(&p,"approve"),"auth":{"token":token}})).unwrap();
    // The real gateway commits first. Its wrapper then kills only the held,
    // unreaped fixture supervisor, before returning the acknowledgment.
    let status = fleet.wait("host killed after origin commit", |_| supervisor.try_wait().unwrap());
    assert!(!status.success());
    linked_native(&linked,&p);
    let registry = linked.registry();
    assert!(registry.query_row("SELECT EXISTS(SELECT 1 FROM door_client_audit WHERE event='approval.dispatch')",[],|r|r.get::<_,bool>(0)).unwrap());
    assert!(!registry.query_row("SELECT EXISTS(SELECT 1 FROM door_client_audit WHERE event='approval.release')",[],|r|r.get::<_,bool>(0)).unwrap());
    fs::write(fleet.root.join("requests.jsonl"), "").unwrap(); fs::write(&fleet.log, "").unwrap();
    fs::write(&linked.mode,"live").unwrap();
    let supervisor = fleet.supervise(); fleet.wait("linked new host hello", |e| e.first().cloned());
    let result = delegated_ok(fleet,&token,"approval",chat_answer(&p,"approve"));
    assert_eq!(result["receipt"]["approved"],true);
    linked_native(&linked,&p);
    assert_eq!(linked.remote_store().query_row("SELECT count(*) FROM door_linked_approval_receipts",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    stop(supervisor);
}

#[test]
fn chat_linked_loss_before_commit_and_malformed_reply_recover_without_duplicate_approval() {
    for mode in ["drop_before", "malformed"] {
        let linked = LinkedChat::new(); let fleet = &linked.fleet;
        let token = linked_chat_setup(&linked);
        let supervisor = fleet.supervise(); fleet.wait("linked loss hello", |e| e.first().cloned());
        let p = delegated_ok(fleet, &token, "approval", linked.prepare());
        fs::write(&linked.mode, mode).unwrap();
        chat_refused(fleet, &token, chat_answer(&p, "approve"), "outcome_unknown");
        if mode == "drop_before" { linked.assert_open(); }
        else { linked_native(&linked, &p); }
        fs::write(&linked.mode, "live").unwrap();
        let recovered = delegated_ok(fleet, &token, "approval", chat_answer(&p, "approve"));
        assert_eq!(recovered["receipt"]["approved"], true);
        linked_native(&linked, &p);
        stop(supervisor);
    }
}

#[test]
fn chat_linked_distinct_grants_race_one_authoritative_native_answer() {
    let linked = LinkedChat::new(); let fleet = &linked.fleet;
    let token = linked_chat_setup(&linked);
    let (_, other_token) = delegated_issue(fleet, "gamma", "approval:answer", Some("beta"));
    let supervisor = fleet.supervise(); fleet.wait("linked grants hello", |e| e.first().cloned());
    let first = delegated_ok(fleet, &token, "approval", linked.prepare());
    let second = delegated_ok(fleet, &other_token, "approval", linked.prepare());
    let calls = fs::read_to_string(&linked.calls).unwrap();
    let requests: Vec<Value> = calls.lines().map(|s| serde_json::from_str(s).unwrap()).collect();
    let mut a = requests[0].clone(); let mut b = requests[1].clone();
    a["args"]["args"] = chat_answer(&first, "approve");
    b["args"]["args"] = chat_answer(&second, "approve");
    let barrier = std::sync::Barrier::new(2);
    let (one,two) = std::thread::scope(|scope| {
        let one = scope.spawn(|| { barrier.wait(); linked.gateway(&a,false) });
        let two = scope.spawn(|| { barrier.wait(); linked.gateway(&b,false) });
        (one.join().unwrap(),two.join().unwrap())
    });
    assert_ne!(one["ok"], two["ok"], "{one} {two}");
    linked_native(&linked, if one["ok"] == true { &first } else { &second });
    assert_eq!(linked.remote_store().query_row("SELECT count(*) FROM door_linked_approval_receipts",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    stop(supervisor);
}

#[test]
fn chat_linked_decision_and_oversized_prefix_never_grant_more_than_shown() {
    let linked = LinkedChat::new(); let fleet = &linked.fleet;
    let token = linked_chat_setup(&linked);
    let supervisor = fleet.supervise(); fleet.wait("linked prefix hello", |e| e.first().cloned());
    // A legacy ask which could not be sent today still exposes its entire
    // prefix, refuses approval atomically, and permits an explicit denial.
    let prefix = format!("make {}", "x".repeat(115));
    linked.remote_store().execute("UPDATE mail SET payload=?1 WHERE id='ask'",
        [json!({"approves":{"action":"x".repeat(300),"command_prefix":prefix}}).to_string()]).unwrap();
    let p = delegated_ok(fleet, &token, "approval", linked.prepare());
    assert_eq!(p["approve_unavailable"], true);
    assert!(p["grant"]["sentence"].as_str().unwrap().ends_with(&prefix));
    chat_refused(fleet, &token, chat_answer(&p, "approve"), "approval_unavailable");
    linked.assert_open();
    assert_eq!(delegated_ok(fleet, &token, "approval", chat_answer(&p, "deny"))["receipt"]["approved"], false);
    linked.remote_call("mail.send", json!({"id":"decision","from":"beta","to":"captain","kind":"decision",
        "task_id":"beta-duty","body":"Choose build","payload":{"approves":{"action":"Submit build 55"},
        "decision":{"question":"Submit now?","options":[
            {"id":"submit","label":"Submit","what_happens":"Submit build 55","approves":true},
            {"id":"hold","label":"Hold","what_happens":"Wait","approves":false}]}}}));
    let p = delegated_ok(fleet, &token, "approval", json!({"mode":"prepare","ship":"beta","id":"decision"}));
    let answer = json!({"mode":"answer","ship":"beta","id":"decision","digest":p["digest"],"version":p["version"],"option_id":"submit"});
    assert_eq!(delegated_ok(fleet, &token, "approval", answer)["receipt"]["approved"], true);
    linked_native(&linked, &p);
    stop(supervisor);
}
