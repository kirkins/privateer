use privateer::{
    dispatch,
    store::{Caller, Store},
};
use serde_json::{Value, json};
use std::fs;

fn fleet() -> (tempfile::TempDir, Store) {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    let mut store = Store::open(&temp.path().join("home")).unwrap();
    dispatch(
        &mut store,
        "charter.put",
        json!({"id":"project","objective":"Deliver","repo":repo,"min_free_bytes":0}),
    )
    .unwrap();
    dispatch(
        &mut store,
        "task.create",
        json!({"id":"main","charter_id":"project","owner":"apps","title":"Deliver"}),
    )
    .unwrap();
    for (name, scope, accepts) in [
        ("apps", "Application work", json!([])),
        ("infra", "Deploys and build servers", json!(["deploy"])),
        ("research", "Investigations", json!([])),
    ] {
        dispatch(
            &mut store,
            "ship.put",
            json!({"name":name,"scope":scope,"accepts":accepts}),
        )
        .unwrap();
    }
    (temp, store)
}

fn send(store: &mut Store, args: Value) -> anyhow::Result<Value> {
    dispatch(store, "mail.send", args)
}

fn err(result: anyhow::Result<Value>) -> String {
    result.unwrap_err().to_string()
}

#[test]
fn pending_coalesces_a_burst_without_reading_or_acknowledging_bodies() {
    let (_temp, mut s) = fleet();
    let mut through = json!(0);
    for n in 0..1000 {
        through = send(
            &mut s,
            json!({"id":format!("burst-{n}"),"from":"apps","to":"infra",
            "kind":"notice","urgency":"low","body":"durable body, never in notification"}),
        )
        .unwrap()["seq"]
            .clone();
    }
    let snapshot = dispatch(&mut s, "mail.pending", json!({"address":"infra"})).unwrap();
    assert_eq!(snapshot["count"], 1000);
    assert_eq!(snapshot["actionable"], 0);
    assert_eq!(snapshot["through_seq"], through);
    assert_eq!(snapshot["headers"].as_array().unwrap().len(), 6);
    assert_eq!(snapshot["headers"][0]["id"], "burst-999");
    assert!(!snapshot.to_string().contains("durable body"));
    assert!(snapshot.to_string().len() < 2048);
    assert_eq!(snapshot["acknowledged_through"], 0);
    assert_eq!(
        dispatch(&mut s, "mail.show", json!({"id":"burst-0"})).unwrap()["body"],
        "durable body, never in notification"
    );
    let request = send(
        &mut s,
        json!({"id":"fresh","from":"apps","to":"infra",
        "kind":"request","urgency":"low","body":"Act on this"}),
    )
    .unwrap();
    let next = dispatch(
        &mut s,
        "mail.pending",
        json!({"address":"infra","after_seq":through}),
    )
    .unwrap();
    assert_eq!(next["count"], 1);
    assert_eq!(next["actionable"], 1);
    assert_eq!(next["through_seq"], request["seq"]);
    assert_eq!(next["acknowledged_through"], 0);
    dispatch(
        &mut s,
        "mail.ack",
        json!({"address":"infra","through_seq":through}),
    )
    .unwrap();
    let acknowledged = dispatch(
        &mut s,
        "mail.pending",
        json!({"address":"infra","after_seq":0}),
    )
    .unwrap();
    assert_eq!(acknowledged["after_seq"], through);
    assert_eq!(acknowledged["count"], 1);
    s.set_caller(Caller::Agent {
        actor: "apps".into(),
    });
    assert!(dispatch(&mut s, "mail.pending", json!({"address":"infra"})).is_err());
}

#[test]
fn directory_records_ships_and_reserves_the_captain() {
    let (_temp, mut s) = fleet();
    let ships = dispatch(&mut s, "ship.list", json!({})).unwrap();
    let names: Vec<_> = ships["ships"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["apps", "infra", "research"]);
    let infra = dispatch(&mut s, "ship.show", json!({"name":"infra"})).unwrap();
    assert_eq!(infra["location"], "here");
    assert_eq!(infra["accepts"], json!(["deploy"]));
    for bad in [
        json!({"name":"captain"}),
        json!({"name":"bad name"}),
        json!({"name":"remote","location":{"peer":"elsewhere"}}),
        json!({"name":"typed","accepts":["bad kind"]}),
    ] {
        assert!(dispatch(&mut s, "ship.put", bad.clone()).is_err(), "{bad}");
    }
}

#[test]
fn a_request_stays_open_until_its_single_reply_from_the_recipient() {
    let (_temp, mut s) = fleet();
    let request = send(
        &mut s,
        json!({"id":"why-deploy-failed","from":"apps","to":"infra","kind":"request",
            "body":"Why did the staging deploy fail?","task_id":"main","reply_within_secs":3600}),
    )
    .unwrap();
    assert_eq!(request["state"], "open");
    assert_eq!(request["overdue"], false);
    let inbox = dispatch(&mut s, "mail.inbox", json!({"address":"infra"})).unwrap();
    assert_eq!(inbox["messages"][0]["id"], "why-deploy-failed");
    let outgoing = dispatch(
        &mut s,
        "mail.requests",
        json!({"address":"apps","direction":"outgoing","state":"open"}),
    )
    .unwrap();
    assert_eq!(outgoing["requests"].as_array().unwrap().len(), 1);

    // Only the recipient may answer, and only to the sender.
    let wrong = send(
        &mut s,
        json!({"from":"research","to":"apps","kind":"reply","correlation":"why-deploy-failed","body":"guess"}),
    );
    assert!(err(wrong).contains("only the recipient"));
    let reply = send(
        &mut s,
        json!({"id":"disk-full","from":"infra","to":"apps","kind":"reply",
            "correlation":"why-deploy-failed","body":"The build host ran out of disk."}),
    )
    .unwrap();
    assert_eq!(reply["state"], Value::Null);
    let answered = dispatch(&mut s, "mail.show", json!({"id":"why-deploy-failed"})).unwrap();
    assert_eq!(answered["state"], "answered");
    assert_eq!(answered["answered_by"], "disk-full");
    let again = send(
        &mut s,
        json!({"from":"infra","to":"apps","kind":"reply","correlation":"why-deploy-failed","body":"also"}),
    );
    assert!(err(again).contains("already answered"));
    // Notices can continue the thread without reopening it.
    send(
        &mut s,
        json!({"from":"infra","to":"apps","kind":"notice","correlation":"why-deploy-failed","body":"Disk cleaned."}),
    )
    .unwrap();
    let apps = dispatch(&mut s, "mail.inbox", json!({"address":"apps"})).unwrap();
    assert_eq!(apps["messages"].as_array().unwrap().len(), 2);
}

#[test]
fn requests_past_their_deadline_read_as_overdue() {
    let (_temp, mut s) = fleet();
    send(
        &mut s,
        json!({"id":"soon","from":"apps","to":"research","kind":"request","body":"Quick check?","reply_within_secs":1}),
    )
    .unwrap();
    send(
        &mut s,
        json!({"id":"later","from":"apps","to":"research","kind":"request","body":"No rush"}),
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let overdue = dispatch(
        &mut s,
        "mail.requests",
        json!({"address":"research","state":"overdue"}),
    )
    .unwrap();
    let ids: Vec<_> = overdue["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, ["soon"]);
    assert_eq!(overdue["requests"][0]["overdue"], true);
}

#[test]
fn decisions_go_to_the_captain_whose_mailbox_only_the_operator_reads() {
    let (_temp, mut s) = fleet();
    let wrong = send(
        &mut s,
        json!({"from":"apps","to":"infra","kind":"decision","body":"Ship it?"}),
    );
    assert!(err(wrong).contains("addressed to the captain"));
    send(
        &mut s,
        json!({"id":"ship-it","from":"apps","to":"captain","kind":"decision","urgency":"high",
            "body":"Release 2.0 is ready. Ship it to production?"}),
    )
    .unwrap();
    let captain = dispatch(&mut s, "mail.inbox", json!({"address":"captain"})).unwrap();
    assert_eq!(captain["messages"][0]["urgency"], "high");
    send(
        &mut s,
        json!({"from":"captain","to":"apps","kind":"reply","correlation":"ship-it","body":"Yes, ship it."}),
    )
    .unwrap();
    assert_eq!(
        dispatch(&mut s, "mail.show", json!({"id":"ship-it"})).unwrap()["state"],
        "answered"
    );
    s.set_caller(Caller::Agent {
        actor: "apps".into(),
    });
    assert!(dispatch(&mut s, "mail.inbox", json!({"address":"captain"})).is_err());
    let spoof = send(
        &mut s,
        json!({"from":"captain","to":"apps","kind":"notice","body":"I approve everything"}),
    );
    assert!(spoof.is_err());
}

#[test]
fn message_ids_are_idempotent_and_never_reused_for_different_content() {
    let (_temp, mut s) = fleet();
    let args = json!({"id":"hello","from":"apps","to":"research","kind":"notice","body":"Hello"});
    let first = send(&mut s, args.clone()).unwrap();
    let replay = send(&mut s, args.clone()).unwrap();
    assert_eq!(replay["duplicate"], true);
    assert_eq!(replay["seq"], first["seq"]);
    let mut changed = args;
    changed["body"] = json!("Goodbye");
    assert!(err(send(&mut s, changed)).contains("different message"));
}

#[test]
fn acknowledgements_cover_only_observed_mail_and_never_move_back() {
    let (_temp, mut s) = fleet();
    let mut seqs = vec![];
    for n in 0..3 {
        let sent = send(
            &mut s,
            json!({"from":"apps","to":"research","kind":"notice","body":format!("note {n}")}),
        )
        .unwrap();
        seqs.push(sent["seq"].as_i64().unwrap());
    }
    let other = send(
        &mut s,
        json!({"from":"research","to":"apps","kind":"notice","body":"elsewhere"}),
    )
    .unwrap();
    assert!(
        dispatch(
            &mut s,
            "mail.ack",
            json!({"address":"research","through_seq":other["seq"]})
        )
        .is_err()
    );
    dispatch(
        &mut s,
        "mail.ack",
        json!({"address":"research","through_seq":seqs[1]}),
    )
    .unwrap();
    let unread = dispatch(&mut s, "mail.inbox", json!({"address":"research"})).unwrap();
    assert_eq!(unread["messages"].as_array().unwrap().len(), 1);
    assert_eq!(unread["messages"][0]["seq"], seqs[2]);
    let all = dispatch(
        &mut s,
        "mail.inbox",
        json!({"address":"research","all":true}),
    )
    .unwrap();
    assert_eq!(all["messages"].as_array().unwrap().len(), 3);
    let back = dispatch(
        &mut s,
        "mail.ack",
        json!({"address":"research","through_seq":seqs[0]}),
    )
    .unwrap();
    assert_eq!(back["acknowledged_through"], seqs[1]);
}

#[test]
fn typed_requests_respect_what_the_recipient_accepts() {
    let (_temp, mut s) = fleet();
    let deploy = json!({"from":"apps","to":"infra","kind":"request","request_kind":"deploy",
        "payload":{"project":"billing","artifact":"abc123","environment":"staging"}});
    send(&mut s, deploy).unwrap();
    let dns = send(
        &mut s,
        json!({"from":"apps","to":"infra","kind":"request","request_kind":"dns","body":"add a record"}),
    );
    assert!(err(dns).contains("does not accept dns"));
    send(
        &mut s,
        json!({"from":"apps","to":"infra","kind":"request","body":"Untyped question"}),
    )
    .unwrap();
    let typed_notice = send(
        &mut s,
        json!({"from":"apps","to":"infra","kind":"notice","request_kind":"deploy","body":"x"}),
    );
    assert!(typed_notice.is_err());
}

#[test]
fn agents_send_and_read_only_as_their_own_ship() {
    let (_temp, mut s) = fleet();
    send(
        &mut s,
        json!({"id":"private","from":"infra","to":"research","kind":"notice","body":"between us"}),
    )
    .unwrap();
    s.set_caller(Caller::Agent {
        actor: "apps".into(),
    });
    send(
        &mut s,
        json!({"from":"apps","to":"infra","kind":"notice","body":"hello"}),
    )
    .unwrap();
    assert!(
        err(send(
            &mut s,
            json!({"from":"infra","to":"apps","kind":"notice","body":"spoofed"})
        ))
        .contains("cannot act as")
    );
    assert!(dispatch(&mut s, "mail.inbox", json!({"address":"infra"})).is_err());
    assert!(dispatch(&mut s, "mail.show", json!({"id":"private"})).is_err());
    assert!(dispatch(&mut s, "ship.put", json!({"name":"rogue"})).is_err());
    dispatch(&mut s, "mail.inbox", json!({"address":"apps"})).unwrap();
}

#[test]
fn unknown_or_invalid_addresses_and_fields_are_refused() {
    let (_temp, mut s) = fleet();
    for bad in [
        json!({"from":"apps","to":"nobody","kind":"notice","body":"x"}),
        json!({"from":"nobody","to":"apps","kind":"notice","body":"x"}),
        json!({"from":"apps","to":"apps","kind":"notice","body":"x"}),
        json!({"from":"apps","to":"infra","kind":"gossip","body":"x"}),
        json!({"from":"apps","to":"infra","kind":"notice"}),
        json!({"from":"apps","to":"infra","kind":"notice","body":"x","task_id":"missing"}),
        json!({"from":"apps","to":"infra","kind":"notice","body":"x","urgency":"panic"}),
        json!({"from":"apps","to":"infra","kind":"notice","body":"x","reply_within_secs":5}),
        json!({"from":"apps","to":"infra","kind":"reply","body":"x"}),
        json!({"from":"apps","to":"infra","kind":"notice","body":"x".repeat(70_000)}),
    ] {
        assert!(send(&mut s, bad.clone()).is_err(), "{bad}");
    }
}

#[test]
fn status_counts_mail_and_prune_keeps_unread_mail_and_open_requests() {
    let (_temp, mut s) = fleet();
    let read = send(
        &mut s,
        json!({"id":"read","from":"apps","to":"research","kind":"notice","body":"seen"}),
    )
    .unwrap();
    dispatch(
        &mut s,
        "mail.ack",
        json!({"address":"research","through_seq":read["seq"]}),
    )
    .unwrap();
    send(
        &mut s,
        json!({"id":"unread","from":"apps","to":"infra","kind":"notice","body":"not yet seen"}),
    )
    .unwrap();
    let open = send(
        &mut s,
        json!({"id":"owed","from":"research","to":"apps","kind":"request","body":"still owed"}),
    )
    .unwrap();
    dispatch(
        &mut s,
        "mail.ack",
        json!({"address":"apps","through_seq":open["seq"]}),
    )
    .unwrap();
    let status = dispatch(&mut s, "status", json!({})).unwrap();
    assert_eq!(status["unacknowledged_mail"], 1);
    assert_eq!(status["open_requests"], 1);
    let cutoff = Store::now_ms() + 1000;
    let pruned = dispatch(&mut s, "history.prune", json!({"before_ms":cutoff})).unwrap();
    assert_eq!(pruned["mail"], 1);
    assert!(dispatch(&mut s, "mail.show", json!({"id":"read"})).is_err());
    dispatch(&mut s, "mail.show", json!({"id":"unread"})).unwrap();
    dispatch(&mut s, "mail.show", json!({"id":"owed"})).unwrap();
}

/// Two homes on this machine, each hosting one ship and knowing the other's
/// ship behind a local-transport peer that runs the real binary.
struct Pair {
    _temp: tempfile::TempDir,
    a: Store,
    b: Store,
    b_home: std::path::PathBuf,
}
fn pair() -> Pair {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let (a_home, b_home) = (root.join("a"), root.join("b"));
    let mut a = Store::open(&a_home).unwrap();
    let mut b = Store::open(&b_home).unwrap();
    let binary = env!("CARGO_BIN_EXE_privateer");
    for (store, peer, home) in [(&mut a, "home-b", &b_home), (&mut b, "home-a", &a_home)] {
        dispatch(
            store,
            "peer.put",
            json!({"name":peer,"transport":"local","executable":binary,"home":home}),
        )
        .unwrap();
    }
    dispatch(&mut a, "ship.put", json!({"name":"apps","scope":"apps"})).unwrap();
    dispatch(
        &mut a,
        "ship.put",
        json!({"name":"infra","scope":"deploys","location":{"peer":"home-b"}}),
    )
    .unwrap();
    dispatch(
        &mut b,
        "ship.put",
        json!({"name":"infra","scope":"deploys"}),
    )
    .unwrap();
    dispatch(
        &mut b,
        "ship.put",
        json!({"name":"apps","scope":"apps","location":{"peer":"home-a"}}),
    )
    .unwrap();
    Pair {
        _temp: temp,
        a,
        b,
        b_home,
    }
}

#[test]
fn requests_and_replies_cross_homes_and_close_both_copies() {
    let Pair {
        _temp,
        mut a,
        mut b,
        ..
    } = pair();
    let sent = send(
        &mut a,
        json!({"id":"staging-health","from":"apps","to":"infra","kind":"request","body":"Is staging healthy?"}),
    )
    .unwrap();
    assert_eq!(sent["state"], "open");
    let inbox = dispatch(&mut b, "mail.inbox", json!({"address":"infra"})).unwrap();
    assert_eq!(inbox["messages"][0]["id"], "staging-health");
    assert_eq!(inbox["messages"][0]["from"], "apps");
    send(
        &mut b,
        json!({"id":"all-green","from":"infra","to":"apps","kind":"reply","correlation":"staging-health","body":"All green."}),
    )
    .unwrap();
    // Both homes' copies of the request close, and the reply reaches apps.
    for store in [&mut a, &mut b] {
        let request = dispatch(store, "mail.show", json!({"id":"staging-health"})).unwrap();
        assert_eq!(request["state"], "answered");
        assert_eq!(request["answered_by"], "all-green");
    }
    let replies = dispatch(&mut a, "mail.inbox", json!({"address":"apps"})).unwrap();
    assert_eq!(replies["messages"][0]["body"], "All green.");
    assert_eq!(
        dispatch(&mut a, "status", json!({})).unwrap()["undelivered_mail"],
        0
    );
    // A home cannot read a mailbox it does not hold.
    assert!(dispatch(&mut a, "mail.inbox", json!({"address":"infra"})).is_err());
}

#[test]
fn undeliverable_mail_stays_pending_until_flushed() {
    let Pair {
        _temp,
        mut a,
        mut b,
        b_home,
    } = pair();
    // Point the peer at a missing executable: the transport fails.
    dispatch(
        &mut a,
        "peer.put",
        json!({"name":"home-b","transport":"local","executable":"/nonexistent/privateer","home":b_home}),
    )
    .unwrap();
    let sent = send(
        &mut a,
        json!({"id":"queued","from":"apps","to":"infra","kind":"notice","body":"Deploy window moved"}),
    )
    .unwrap();
    let stored = dispatch(&mut a, "mail.show", json!({"id":"queued"})).unwrap();
    assert_eq!(sent["id"], "queued");
    assert_eq!(
        dispatch(&mut a, "status", json!({})).unwrap()["undelivered_mail"],
        1
    );
    assert!(stored.get("seq").is_some());
    assert_eq!(
        dispatch(&mut a, "mail.flush", json!({})).unwrap()["pending"],
        1
    );
    dispatch(
        &mut a,
        "peer.put",
        json!({"name":"home-b","transport":"local","executable":env!("CARGO_BIN_EXE_privateer"),"home":b_home}),
    )
    .unwrap();
    assert_eq!(
        dispatch(&mut a, "mail.flush", json!({})).unwrap()["delivered"],
        1
    );
    assert_eq!(
        dispatch(&mut a, "status", json!({})).unwrap()["undelivered_mail"],
        0
    );
    let inbox = dispatch(&mut b, "mail.inbox", json!({"address":"infra"})).unwrap();
    assert_eq!(inbox["messages"][0]["id"], "queued");
    // Redelivering the same message is a no-op at the receiving home.
    let again = dispatch(
        &mut b,
        "mail.deliver",
        json!({"id":"queued","from":"apps","to":"infra","kind":"notice","body":"Deploy window moved"}),
    )
    .unwrap();
    assert_eq!(again["duplicate"], true);
}

#[test]
fn agents_mail_other_homes_as_themselves() {
    let Pair {
        _temp,
        mut a,
        mut b,
        ..
    } = pair();
    a.set_caller(Caller::Agent {
        actor: "apps".into(),
    });
    send(
        &mut a,
        json!({"id":"from-agent","from":"apps","to":"infra","kind":"notice","body":"hello from an agent"}),
    )
    .unwrap();
    let delivered = dispatch(&mut b, "mail.show", json!({"id":"from-agent"})).unwrap();
    assert_eq!(delivered["from"], "apps");
}

#[test]
fn delivery_accepts_only_known_remote_senders_for_local_ships() {
    let Pair { _temp, mut b, .. } = pair();
    dispatch(&mut b, "ship.put", json!({"name":"local-only","scope":"x"})).unwrap();
    for bad in [
        // Unknown sender.
        json!({"id":"m1","from":"stranger","to":"infra","kind":"notice","body":"x"}),
        // A sender this home hosts cannot arrive from a peer.
        json!({"id":"m2","from":"local-only","to":"infra","kind":"notice","body":"x"}),
        // Recipient not hosted here.
        json!({"id":"m3","from":"apps","to":"apps","kind":"notice","body":"x"}),
        // The captain's mail stays in its own home for now.
        json!({"id":"m4","from":"apps","to":"captain","kind":"decision","body":"x"}),
        // A reply to a request this home never sent.
        json!({"id":"m5","from":"apps","to":"infra","kind":"reply","correlation":"nothing","body":"x"}),
    ] {
        assert!(
            dispatch(&mut b, "mail.deliver", bad.clone()).is_err(),
            "{bad}"
        );
    }
}
