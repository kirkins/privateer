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
