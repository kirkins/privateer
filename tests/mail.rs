mod support;

use privateer::{
    dispatch,
    store::{Caller, Store},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;

use support::write_isolated_peer;

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

fn legacy_outcome_id(prefix: &str, request: &str) -> String {
    let digest = Sha256::digest(json!(request).to_string().as_bytes());
    format!("{prefix}-{digest:x}")
}

fn send(store: &mut Store, args: Value) -> anyhow::Result<Value> {
    dispatch(store, "mail.send", args)
}

fn err(result: anyhow::Result<Value>) -> String {
    result.unwrap_err().to_string()
}

#[test]
fn delivered_captain_bodies_are_validated_and_audited_once() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = Store::open(&temp.path().join("home")).unwrap();
    dispatch(&mut store, "ship.put", json!({"name":"alpha"})).unwrap();
    for body in ["\u{1b}", "hello\u{1b}more", "text\u{9f}", "\n\t\r"] {
        assert!(
            dispatch(
                &mut store,
                "mail.deliver",
                json!({"id":"incoming-mail",
            "from":"captain","to":"alpha","kind":"notice","body":body})
            )
            .is_err()
        );
    }
    assert!(
        !store
            .connection()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM mail WHERE id='incoming-mail')",
                [],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
    );
    let body = "船 🌊 e\u{301}\n\tline\r\nnext";
    let args =
        json!({"id":"incoming-mail","from":"captain","to":"alpha","kind":"notice","body":body});
    dispatch(&mut store, "mail.deliver", args.clone()).unwrap();
    assert_eq!(
        dispatch(&mut store, "mail.deliver", args).unwrap()["duplicate"],
        true
    );
    let stored = dispatch(&mut store, "mail.show", json!({"id":"incoming-mail"})).unwrap();
    assert_eq!(stored["body"], body);
    let audits: Vec<String> = store.connection().prepare("SELECT payload FROM audit WHERE kind='captain.send' AND json_extract(payload,'$.id')='incoming-mail'")
        .unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(audits.len(), 1);
    assert_eq!(
        serde_json::from_str::<Value>(&audits[0]).unwrap(),
        json!({
        "id":"incoming-mail","from":"captain","to":"alpha","kind":"notice",
        "origin":"mail.deliver","body_bytes":body.len()})
    );
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
fn a_low_notice_in_the_thread_of_an_open_follow_up_is_actionable_until_that_follow_up_is_closed() {
    let (_temp, mut s) = fleet();
    // The incident: a ship asks, this home answers and raises a follow-up on
    // the answer, and the asking ship corrects itself later in that thread.
    send(
        &mut s,
        json!({"id":"baf32096","from":"apps","to":"infra","kind":"request","body":"Which machine?"}),
    )
    .unwrap();
    send(
        &mut s,
        json!({"from":"infra","to":"apps","kind":"reply","correlation":"baf32096","body":"The laptop"}),
    )
    .unwrap();
    let follow_up = send(
        &mut s,
        json!({"id":"e376ceec","from":"infra","to":"captain","kind":"request",
        "because_of":"baf32096","body":"Can you release the laptop?"}),
    )
    .unwrap();
    assert_eq!(follow_up["because_of"], "baf32096");
    // The question is answered and read; only the follow-up and any later
    // correction in its thread are left in this mailbox.
    let answered = dispatch(&mut s, "mail.show", json!({"id":"baf32096"})).unwrap();
    dispatch(
        &mut s,
        "mail.ack",
        json!({"address":"infra","through_seq":answered["seq"]}),
    )
    .unwrap();
    assert_eq!(
        dispatch(&mut s, "mail.pending", json!({"address":"infra"})).unwrap()["actionable"],
        0
    );
    send(
        &mut s,
        json!({"id":"late","from":"apps","to":"infra","kind":"notice","urgency":"low",
        "correlation":"baf32096","body":"Correction: the desktop is free"}),
    )
    .unwrap();
    // A low notice that corrects an open follow-up is not routine.
    let pending = dispatch(&mut s, "mail.pending", json!({"address":"infra"})).unwrap();
    assert_eq!(pending["actionable"], 1);
    assert_eq!(pending["count"], 1);
    // Once the follow-up is closed, the same notice is background again.
    dispatch(
        &mut s,
        "mail.withdraw",
        json!({"id":"e376ceec","actor":"infra","reason":"the correction settled it"}),
    )
    .unwrap();
    assert_eq!(
        dispatch(&mut s, "mail.pending", json!({"address":"infra"})).unwrap()["actionable"],
        0
    );
    // A low notice in the thread of an open request wakes its recipient (or sender).
    send(
        &mut s,
        json!({"id":"other","from":"apps","to":"infra","kind":"request","body":"Ship it?"}),
    )
    .unwrap();
    send(
        &mut s,
        json!({"id":"fyi","from":"apps","to":"infra","kind":"notice","urgency":"low",
        "correlation":"other","body":"Unrelated heads-up"}),
    )
    .unwrap();
    let pending = dispatch(&mut s, "mail.pending", json!({"address":"infra"})).unwrap();
    assert_eq!(pending["actionable"], 2);
    assert_eq!(pending["headers"][0]["id"], "fyi");
}

#[test]
fn because_of_names_one_request_and_only_a_request_carries_it() {
    let (_temp, mut s) = fleet();
    send(
        &mut s,
        json!({"id":"parent","from":"apps","to":"infra","kind":"request","body":"Which machine?"}),
    )
    .unwrap();
    send(
        &mut s,
        json!({"id":"child","from":"infra","to":"captain","kind":"request","because_of":"parent",
        "body":"Release the laptop"}),
    )
    .unwrap();
    // The parent may be unknown here: it can live in another home.
    assert_eq!(
        send(
            &mut s,
            json!({"id":"elsewhere","from":"infra","to":"captain","kind":"request",
            "because_of":"baf32096","body":"Release the desktop"})
        )
        .unwrap()["because_of"],
        "baf32096"
    );
    let grandchild = send(
        &mut s,
        json!({"id":"grandchild","from":"infra","to":"captain","kind":"request","because_of":"child","body":"?"}),
    )
    .unwrap();
    assert_eq!(grandchild["because_of"], "child");
    for (args, expected) in [
        (
            json!({"id":"self","from":"infra","to":"captain","kind":"request","because_of":"self","body":"?"}),
            "because of itself",
        ),
        (
            json!({"id":"bad id","from":"infra","to":"captain","kind":"request","because_of":"bad id","body":"?"}),
            "identifier",
        ),
        (
            json!({"id":"headline","from":"infra","to":"captain","kind":"notice","because_of":"parent","body":"?"}),
            "only a request carries it",
        ),
        (
            json!({"id":"typed","from":"infra","to":"captain","kind":"decision","because_of":"parent",
        "body":"?","payload":{"decision":{"question":"?","options":[
          {"id":"a","label":"A","what_happens":"a"},{"id":"b","label":"B","what_happens":"b"}]}}}),
            "only a request carries it",
        ),
    ] {
        let message = err(send(&mut s, args.clone()));
        assert!(message.contains(expected), "{args}: {message}");
    }
    // The id is the idempotency key: a retry may not change the link.
    send(
        &mut s,
        json!({"id":"parent-2","from":"apps","to":"infra","kind":"request","body":"Or the desktop?"}),
    )
    .unwrap();
    assert!(
        err(send(
            &mut s,
            json!({"id":"child","from":"infra","to":"captain","kind":"request","because_of":"parent-2",
            "body":"Release the laptop"})
        ))
        .contains("already holds a different message")
    );
    // The same retry with the same link is the message that is already there.
    assert_eq!(
        dispatch(&mut s, "mail.show", json!({"id":"child"})).unwrap()["because_of"],
        "parent"
    );
}

#[test]
fn a_notice_may_correct_a_thread_this_home_never_stored() {
    let Pair { _temp, mut a, .. } = pair();
    // The parent lives in another home, so this one never stored the id: the
    // correction to a peer is exactly the mail that cannot be refused for naming it.
    let correction = send(
        &mut a,
        json!({"id":"correction","from":"apps","to":"infra","kind":"notice","urgency":"low",
        "correlation":"baf32096","body":"Correction: the desktop is free"}),
    )
    .unwrap();
    assert_eq!(correction["correlation"], "baf32096");
    // A reply or a request naming an unknown thread is still refused here: it
    // answers or asks for something only this home can judge.
    for kind in ["reply", "request"] {
        let refused = err(send(
            &mut a,
            json!({"id":format!("{kind}-nowhere"),"from":"apps","to":"infra","kind":kind,
            "correlation":"baf32096","body":"?"}),
        ));
        assert!(refused.contains("unknown correlated message"), "{refused}");
    }
    // A local typo (notice to a local recipient with unknown correlation) is still refused.
    let (_temp_local, mut s) = fleet();
    let local_typo = err(send(
        &mut s,
        json!({"id":"local-typo","from":"apps","to":"infra","kind":"notice","urgency":"low",
        "correlation":"baf32096","body":"Correction: the desktop is free"}),
    ));
    assert!(
        local_typo.contains("unknown correlated message"),
        "{local_typo}"
    );
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
    /// The isolated wrapper around the real binary every local peer runs.
    peer: std::path::PathBuf,
}
fn pair() -> Pair {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let (a_home, b_home) = (root.join("a"), root.join("b"));
    let mut a = Store::open(&a_home).unwrap();
    let mut b = Store::open(&b_home).unwrap();
    // The local peers run the real binary, but through the isolated wrapper,
    // so they inherit neither a supervising crew job's identity and fleet
    // nor this process's HOME.
    let peer = root.join("peer");
    let binary = json!(env!("CARGO_BIN_EXE_privateer"));
    write_isolated_peer(
        &peer,
        &format!("os.execv({binary}, [{binary}] + sys.argv[1:])"),
    );
    for (store, name, home) in [(&mut a, "home-b", &b_home), (&mut b, "home-a", &a_home)] {
        dispatch(
            store,
            "peer.put",
            json!({"name":name,"transport":"local","executable":peer,"home":home}),
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
        peer,
    }
}

/// Seed an outgoing record as a previous binary could have committed it,
/// without asking the new send-time validator to accept invalid wire bytes.
fn seed_pending(store: &Store, args: &Value, created_ms: i64) {
    store.connection().execute(
        "INSERT INTO mail(id,sender,recipient,kind,correlation,body,payload,urgency,created_ms,state,delivery)
         VALUES(?1,?2,?3,?4,?5,?6,?7,'normal',?8,?9,'pending')",
        rusqlite::params![args["id"].as_str().unwrap(),args["from"].as_str().unwrap(),
            args["to"].as_str().unwrap(),args["kind"].as_str().unwrap(),args["correlation"].as_str(),
            args["body"].as_str().unwrap(),args["payload"].to_string(),created_ms,
            (args["kind"] == "request").then_some("open")],
    ).unwrap();
}

fn seed_legacy_oversize(store: &Store, mut args: Value) -> Value {
    let now = Store::now_ms();
    args["body"] = json!("");
    args["payload"] = json!({"text":"\"\\\n船"});
    let mut wire = json!({"id":args["id"],"from":args["from"],"to":args["to"],"kind":args["kind"],
        "correlation":args["correlation"],"request_kind":null,"body":"","payload":args["payload"],
        "urgency":"normal","deadline_ms":null,"created_ms":now,"because_of":null});
    args["body"] = json!("x".repeat(65_537 - wire.to_string().len()));
    wire["body"] = args["body"].clone();
    assert_eq!(wire.to_string().len(), 65_537);
    assert!(
        args.to_string().len() <= 65_536,
        "the old send accepted these arguments"
    );
    seed_pending(store, &args, now);
    args
}

fn original_facts(store: &Store, id: &str) -> String {
    store.connection().query_row(
        "SELECT json_array(seq,id,sender,recipient,kind,correlation,task_id,request_kind,body,payload,
           urgency,deadline_ms,created_ms,state,answered_by,answered_ms) FROM mail WHERE id=?1",
        [id], |r| r.get(0),
    ).unwrap()
}

#[test]
fn legacy_wire_oversize_is_preserved_and_unblocks_every_delivery_path() {
    for mode in ["immediate", "manual", "dispatcher"] {
        let Pair {
            _temp,
            mut a,
            mut b,
            ..
        } = pair();
        let originals: Vec<_> = [("legacy-notice", "notice"), ("legacy-request", "request")]
            .into_iter()
            .map(|(id, kind)| {
                let args = seed_legacy_oversize(
                    &a,
                    json!({"id":id,"from":"apps","to":"infra","kind":kind}),
                );
                let facts = original_facts(&a, id);
                (args, facts)
            })
            .collect();
        let valid = json!({"id":"valid","from":"apps","to":"infra","kind":"notice",
            "body":"valid later mail","payload":{}});
        if mode == "immediate" {
            let sent = send(&mut a, valid).unwrap();
            let actual_delivery: String = a
                .connection()
                .query_row("SELECT delivery FROM mail WHERE id='valid'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(
                actual_delivery, "delivered",
                "{mode}: valid mail must pass the legacy head"
            );
            assert_eq!(sent["delivery"], "delivered", "{mode}: {sent}");
        } else {
            seed_pending(&a, &valid, Store::now_ms());
            let report = if mode == "manual" {
                dispatch(&mut a, "mail.flush", json!({"limit":1})).unwrap()
            } else {
                privateer::activation::tick(&mut a).unwrap()["delivered"].clone()
            };
            assert_eq!(report["delivered"], 1, "{mode}: {report}");
            assert_eq!(report["attempted"], 1);
            assert_eq!(report["quarantined"], 2);
            assert_eq!(report["pending"], 0);
        }
        let inbox = dispatch(&mut b, "mail.inbox", json!({"address":"infra"})).unwrap();
        assert_eq!(inbox["messages"].as_array().unwrap().len(), 1, "{mode}");
        assert_eq!(inbox["messages"][0]["id"], "valid");
        let calls: i64 = a
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM audit WHERE kind='peer.called'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(calls, 1, "only valid mail may reach the transport");
        let status = dispatch(&mut a, "status", json!({})).unwrap();
        assert_eq!(status["undelivered_mail"], 2);
        assert_eq!(status["quarantined_mail"], 2);
        assert_eq!(status["open_requests"], 1);
        let home = a.home().to_owned();
        drop(a);
        let mut a = Store::open(&home).unwrap();
        let again = dispatch(&mut a, "mail.flush", json!({})).unwrap();
        assert_eq!(again["attempted"], 0);
        assert_eq!(again["quarantined"], 2);
        dispatch(
            &mut a,
            "history.prune",
            json!({"before_ms":Store::now_ms()+1000}),
        )
        .unwrap();
        for (args, facts) in originals {
            let id = args["id"].as_str().unwrap();
            assert_eq!(
                original_facts(&a, id),
                facts,
                "{mode}: immutable facts for {id}"
            );
            let stored = dispatch(&mut a, "mail.show", json!({"id":id})).unwrap();
            assert_eq!(stored["delivery"], "quarantined");
            assert_eq!(stored["delivery_attempts"], 0);
            assert!(stored["delivery_error"].as_str().unwrap().contains("65537"));
            let replay = send(&mut a, args.clone()).unwrap();
            assert_eq!(replay["duplicate"], true);
            assert_eq!(replay["delivery"], "quarantined");
            let mut changed = args;
            changed["body"] = json!("replacement text requires a new id");
            assert!(err(send(&mut a, changed)).contains("different message"));
        }
    }
}

#[test]
fn legacy_quarantine_work_is_bounded_separately_from_transport_attempts() {
    let Pair {
        _temp,
        mut a,
        mut b,
        ..
    } = pair();
    for n in 0..120 {
        seed_legacy_oversize(
            &a,
            json!({"id":format!("poison-{n}"),"from":"apps","to":"infra","kind":"notice"}),
        );
    }
    let valid = send(
        &mut a,
        json!({"id":"after-poison","from":"apps","to":"infra",
        "kind":"notice","body":"eventual progress"}),
    )
    .unwrap();
    assert_eq!(valid["delivery"], "pending");
    let status = dispatch(&mut a, "status", json!({})).unwrap();
    assert_eq!(
        status["quarantined_mail"], 51,
        "immediate send examines at most 1+50 records"
    );
    assert_eq!(status["undelivered_mail"], 121);
    let second = dispatch(&mut a, "mail.flush", json!({"limit":1})).unwrap();
    assert_eq!(second["attempted"], 0);
    assert_eq!(second["quarantined"], 102);
    assert_eq!(second["pending"], 19);
    let third = privateer::activation::tick(&mut a).unwrap()["delivered"].clone();
    assert_eq!(third["attempted"], 1);
    assert_eq!(third["delivered"], 1);
    assert_eq!(third["quarantined"], 120);
    assert_eq!(third["pending"], 0);
    let inbox = dispatch(&mut b, "mail.inbox", json!({"address":"infra"})).unwrap();
    assert_eq!(inbox["messages"][0]["id"], "after-poison");
    let page = dispatch(
        &mut a,
        "mail.outbox",
        json!({"delivery":"quarantined","limit":1000}),
    )
    .unwrap();
    assert_eq!(page["messages"].as_array().unwrap().len(), 100);
    let rest = dispatch(
        &mut a,
        "mail.outbox",
        json!({"delivery":"quarantined","after_seq":page["next_after_seq"]}),
    )
    .unwrap();
    assert_eq!(rest["messages"].as_array().unwrap().len(), 20);
    assert!(rest["next_after_seq"].is_null());
}

#[test]
fn quarantine_and_audit_commit_together_without_changing_peer_backoff() {
    let Pair { _temp, mut a, .. } = pair();
    seed_legacy_oversize(
        &a,
        json!({"id":"atomic","from":"apps","to":"infra","kind":"request"}),
    );
    a.connection()
        .execute(
            "INSERT INTO mail_delivery_retry VALUES('home-b',2,?1,123)",
            [i64::MAX],
        )
        .unwrap();
    a.connection()
        .execute_batch(
            "CREATE TRIGGER reject_quarantine_audit BEFORE INSERT ON audit
         WHEN NEW.kind='mail.quarantined' BEGIN SELECT RAISE(ABORT,'fixture audit failure'); END;",
        )
        .unwrap();
    assert!(err(dispatch(&mut a, "mail.flush", json!({}))).contains("fixture audit failure"));
    let pending = dispatch(&mut a, "mail.show", json!({"id":"atomic"})).unwrap();
    assert_eq!(pending["delivery"], "pending");
    assert!(pending["delivery_error"].is_null());
    a.connection()
        .execute_batch("DROP TRIGGER reject_quarantine_audit;")
        .unwrap();
    let flushed = dispatch(&mut a, "mail.flush", json!({})).unwrap();
    assert_eq!(flushed["quarantined"], 1);
    assert_eq!(flushed["attempted"], 0);
    let retry: (i64,i64,i64) = a.connection().query_row(
        "SELECT failures,next_attempt_ms,last_attempt_ms FROM mail_delivery_retry WHERE peer='home-b'",
        [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    ).unwrap();
    assert_eq!(retry, (2, i64::MAX, 123));
    let audits: i64 = a
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM audit WHERE kind='mail.quarantined'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(audits, 1);
    assert_eq!(
        dispatch(&mut a, "mail.show", json!({"id":"atomic"})).unwrap()["state"],
        "open"
    );
}

#[test]
fn outbox_pages_only_the_senders_headers_and_bounds_error_display() {
    let Pair { _temp, mut a, .. } = pair();
    for (id, from) in [
        ("apps-first", "apps"),
        ("apps-second", "apps"),
        ("research-private", "research"),
    ] {
        seed_legacy_oversize(
            &a,
            json!({"id":id,"from":from,"to":"infra","kind":"notice"}),
        );
    }
    dispatch(&mut a, "mail.flush", json!({})).unwrap();
    let full_error = "private diagnostic ".repeat(1000);
    a.connection()
        .execute(
            "UPDATE mail SET delivery_error=?1 WHERE id='research-private'",
            [&full_error],
        )
        .unwrap();
    a.set_caller(Caller::Agent {
        actor: "apps".into(),
    });
    let first = dispatch(
        &mut a,
        "mail.outbox",
        json!({"delivery":"quarantined","limit":1}),
    )
    .unwrap();
    assert_eq!(first["messages"][0]["id"], "apps-first");
    assert_eq!(first["messages"].as_array().unwrap().len(), 1);
    assert!(first["next_after_seq"].as_i64().is_some());
    let second = dispatch(
        &mut a,
        "mail.outbox",
        json!({"after_seq":first["next_after_seq"]}),
    )
    .unwrap();
    assert_eq!(second["messages"].as_array().unwrap().len(), 1);
    assert_eq!(second["messages"][0]["id"], "apps-second");
    assert!(second["next_after_seq"].is_null());
    assert!(
        !second["messages"][0]
            .as_object()
            .unwrap()
            .contains_key("body")
    );
    assert!(
        !second["messages"][0]
            .as_object()
            .unwrap()
            .contains_key("payload")
    );
    assert!(dispatch(&mut a, "mail.outbox", json!({"from":"research"})).is_err());
    assert!(dispatch(&mut a, "mail.outbox", json!({"from":"unknown"})).is_err());
    assert!(dispatch(&mut a, "mail.show", json!({"id":"research-private"})).is_err());
    assert_eq!(
        dispatch(&mut a, "mail.flush", json!({})).unwrap()["quarantined"],
        2
    );
    a.set_caller(Caller::Agent {
        actor: "unknown".into(),
    });
    assert!(
        dispatch(&mut a, "mail.outbox", json!({})).unwrap()["messages"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    a.set_caller(Caller::Operator);
    assert!(
        dispatch(&mut a, "mail.outbox", json!({"from":"unknown"})).unwrap()["messages"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let research = dispatch(&mut a, "mail.outbox", json!({"from":"research"})).unwrap();
    assert_eq!(research["messages"].as_array().unwrap().len(), 1);
    assert_eq!(
        research["messages"][0]["delivery_error"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        1024
    );
    assert_eq!(
        dispatch(&mut a, "mail.show", json!({"id":"research-private"})).unwrap()["delivery_error"],
        full_error
    );
    assert!(dispatch(&mut a, "mail.outbox", json!({"delivery":"received"})).is_err());
}

#[test]
fn quarantining_a_legacy_reply_preserves_the_existing_conversation_states() {
    let Pair {
        _temp,
        mut a,
        mut b,
        ..
    } = pair();
    send(
        &mut a,
        json!({"id":"legacy-thread","from":"apps","to":"infra","kind":"request","body":"question"}),
    )
    .unwrap();
    let reply = seed_legacy_oversize(
        &b,
        json!({"id":"legacy-reply","from":"infra","to":"apps",
        "kind":"reply","correlation":"legacy-thread"}),
    );
    b.connection().execute(
        "UPDATE mail SET state='answered',answered_by='legacy-reply',answered_ms=?1 WHERE id='legacy-thread'",
        [Store::now_ms()],
    ).unwrap();
    let thread_facts = original_facts(&b, "legacy-thread");
    let reply_facts = original_facts(&b, "legacy-reply");
    let valid = send(&mut b, json!({"id":"later-notice","from":"infra","to":"apps","kind":"notice","body":"coordination"})).unwrap();
    assert_eq!(valid["delivery"], "delivered");
    assert_eq!(original_facts(&b, "legacy-thread"), thread_facts);
    assert_eq!(original_facts(&b, "legacy-reply"), reply_facts);
    let quarantined = dispatch(&mut b, "mail.show", json!({"id":"legacy-reply"})).unwrap();
    assert_eq!(quarantined["delivery"], "quarantined");
    assert_eq!(quarantined["correlation"], "legacy-thread");
    assert_eq!(
        dispatch(&mut a, "mail.show", json!({"id":"legacy-thread"})).unwrap()["state"],
        "open"
    );
    assert_eq!(
        dispatch(&mut b, "mail.show", json!({"id":"legacy-thread"})).unwrap()["state"],
        "answered"
    );
    assert!(dispatch(&mut a, "mail.show", json!({"id":"legacy-reply"})).is_err());
    assert_eq!(send(&mut b, reply).unwrap()["duplicate"], true);
    assert!(
        err(send(
            &mut b,
            json!({"id":"replacement-reply","from":"infra","to":"apps",
        "kind":"reply","correlation":"legacy-thread","body":"short replacement"})
        ))
        .contains("already answered")
    );
}

/// An isolated peer executable starts with none of the job identity or gh
/// state of whoever runs the tests, and keeps the isolation settings the
/// wrapper itself chooses. A crew job's gate carries PRIVATEER_JOB, _ROLE,
/// _ACTOR, _HOME and GH_CONFIG_DIR; a peer transport spawned from the test's
/// own process would hand them to its executable unchanged.
#[test]
fn an_isolated_peer_executable_inherits_no_job_or_gh_environment() {
    let temp = tempfile::TempDir::new().unwrap();
    let peer = temp.path().join("probe-peer");
    write_isolated_peer(&peer, "print(json.dumps(dict(sorted(os.environ.items()))))");
    let output = std::process::Command::new(&peer)
        .env("PRIVATEER_JOB", "gate-job")
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "gate-crew")
        .env("PRIVATEER_HOME", "/nonexistent/privateer-home")
        .env("GH_CONFIG_DIR", "/nonexistent/gh")
        .env("GH_TOKEN", "gate-token")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let environment: std::collections::BTreeMap<String, String> =
        serde_json::from_slice(&output.stdout).unwrap();
    for name in [
        "PRIVATEER_JOB",
        "PRIVATEER_ROLE",
        "PRIVATEER_ACTOR",
        "PRIVATEER_HOME",
        "GH_CONFIG_DIR",
        "GH_TOKEN",
    ] {
        assert!(!environment.contains_key(name), "{name} reached the peer");
    }
    // The settings the wrapper sets on purpose are all that remain of them.
    assert_eq!(environment["PRIVATEER_SERVICE"], "none");
    assert!(
        environment["PRIVATEER_FLEET"].ends_with("fleet.sqlite"),
        "{:?}",
        environment["PRIVATEER_FLEET"]
    );
}

#[test]
fn valid_wire_with_a_remote_size_error_stays_pending_with_backoff() {
    let Pair {
        _temp,
        mut a,
        b_home,
        ..
    } = pair();
    let executable = _temp.path().join("refuse.py");
    write_isolated_peer(
        &executable,
        "r=json.loads(sys.stdin.readline())\nprint(json.dumps({'id':r['id'],'ok':False,'error':'temporary remote error: message exceeds 64 KiB'}))",
    );
    dispatch(
        &mut a,
        "peer.put",
        json!({"name":"home-b","transport":"local","executable":executable,"home":b_home}),
    )
    .unwrap();
    let sent = send(
        &mut a,
        json!({"id":"transient","from":"apps","to":"infra","kind":"notice","body":"valid wire"}),
    )
    .unwrap();
    assert_eq!(sent["delivery"], "pending");
    assert_eq!(sent["delivery_attempts"], 1);
    let report = dispatch(&mut a, "mail.flush", json!({})).unwrap();
    assert_eq!(report["pending"], 1);
    assert_eq!(report["quarantined"], 0);
    assert_eq!(report["attempted"], 1);
    let retry: (i64, i64) = a
        .connection()
        .query_row(
            "SELECT failures,next_attempt_ms FROM mail_delivery_retry WHERE peer='home-b'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(retry.0, 2);
    assert!(retry.1 > 0);
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
fn long_message_ids_cross_homes_on_every_delivery_path() {
    for mode in ["immediate", "manual", "dispatcher"] {
        let Pair {
            _temp,
            mut a,
            mut b,
            b_home,
            peer,
        } = pair();
        if mode != "immediate" {
            dispatch(
                &mut a,
                "peer.put",
                json!({"name":"home-b","transport":"local",
                    "executable":"/nonexistent/privateer","home":b_home}),
            )
            .unwrap();
        }
        let request_id = "a".repeat(121);
        let notice_id = "b".repeat(160);
        for (id, kind) in [(&request_id, "request"), (&notice_id, "notice")] {
            send(
                &mut a,
                json!({"id":id,"from":"apps","to":"infra","kind":kind,
                    "body":"preserve identity","payload":{"text":"\"\\\n船"}}),
            )
            .unwrap();
        }
        if mode != "immediate" {
            dispatch(
                &mut a,
                "peer.put",
                json!({"name":"home-b","transport":"local",
                    "executable":peer,"home":b_home}),
            )
            .unwrap();
            let report = if mode == "manual" {
                dispatch(&mut a, "mail.flush", json!({})).unwrap()
            } else {
                a.connection()
                    .execute("UPDATE mail_delivery_retry SET next_attempt_ms=0", [])
                    .unwrap();
                privateer::activation::tick(&mut a).unwrap()["delivered"].clone()
            };
            assert_eq!(report["delivered"], 2, "{mode}: {report}");
            assert_eq!(report["pending"], 0, "{mode}: {report}");
        }
        let inbox = dispatch(&mut b, "mail.inbox", json!({"address":"infra"})).unwrap();
        assert_eq!(inbox["messages"][0]["id"], request_id, "{mode}");
        assert_eq!(inbox["messages"][1]["id"], notice_id, "{mode}");
        assert_eq!(inbox["messages"][0]["payload"]["text"], "\"\\\n船");
        let reply_id = "r".repeat(160);
        send(
            &mut b,
            json!({"id":reply_id,"from":"infra","to":"apps","kind":"reply",
                "correlation":request_id,"body":"answered"}),
        )
        .unwrap();
        for store in [&mut a, &mut b] {
            let request = dispatch(store, "mail.show", json!({"id":request_id})).unwrap();
            assert_eq!(request["answered_by"], reply_id, "{mode}");
            assert_eq!(request["state"], "answered", "{mode}");
        }
    }
}

#[test]
fn outgoing_mail_is_limited_by_its_normalized_wire_bytes() {
    let Pair {
        _temp,
        mut a,
        mut b,
        ..
    } = pair();
    let oversized = json!({"id":"large","from":"apps","to":"infra",
        "kind":"notice","body":"x".repeat(65_450)});
    assert!(oversized.to_string().len() < 65_536);
    assert!(err(send(&mut a, oversized)).contains("64 KiB"));
    assert!(dispatch(&mut a, "mail.show", json!({"id":"large"})).is_err());
    send(
        &mut a,
        json!({"id":"thread","from":"apps","to":"infra","kind":"request","body":"context"}),
    )
    .unwrap();
    for mode in ["body", "payload", "generated-id", "metadata"] {
        let now = Store::now_ms();
        let mut args = json!({"id":mode,"from":"apps","to":"infra",
            "kind":"notice","body":"\"\\\n船","payload":{}});
        let mut wire = json!({"id":mode,"from":"apps","to":"infra",
            "kind":"notice","body":"\"\\\n船","payload":{},"correlation":null,
            "request_kind":null,"urgency":"normal","deadline_ms":null,"created_ms":now,
            "because_of":null});
        if mode == "generated-id" {
            args.as_object_mut().unwrap().remove("id");
            wire["id"] = json!("00000000-0000-0000-0000-000000000000");
        }
        if mode == "metadata" {
            for (key, value) in [
                ("kind", json!("request")),
                ("correlation", json!("thread")),
                ("request_kind", json!("deploy")),
                ("urgency", json!("high")),
            ] {
                args[key] = value.clone();
                wire[key] = value;
            }
            args["reply_within_secs"] = json!(3600);
            wire["deadline_ms"] = json!(now + 3_600_000);
        }
        if mode == "payload" {
            args["payload"] = json!({"text":""});
            wire["payload"] = args["payload"].clone();
        }
        let padding = "x".repeat(65_537 - wire.to_string().len());
        let key = if mode == "payload" { "payload" } else { "body" };
        let content = if mode == "payload" {
            json!({"text":padding})
        } else {
            json!(format!("\"\\\n船{padding}"))
        };
        args[key] = content.clone();
        wire[key] = content;
        assert_eq!(wire.to_string().len(), 65_537, "{mode}");
        assert!(args.to_string().len() <= 65_536, "{mode}");
        assert!(err(send(&mut a, args.clone())).contains("64 KiB"), "{mode}");
        assert_eq!(
            dispatch(&mut a, "status", json!({})).unwrap()["undelivered_mail"],
            0
        );
        if mode == "payload" {
            let text = args["payload"]["text"].as_str().unwrap();
            args["payload"]["text"] = json!(&text[..text.len() - 1]);
        } else {
            let text = args["body"].as_str().unwrap();
            args["body"] = json!(&text[..text.len() - 1]);
        }
        let sent = send(&mut a, args).unwrap();
        let received = dispatch(&mut b, "mail.show", json!({"id":sent["id"]})).unwrap();
        for key in [
            "id",
            "body",
            "payload",
            "correlation",
            "request_kind",
            "urgency",
            "deadline_ms",
        ] {
            assert_eq!(received[key], sent[key], "{mode}: {key}");
        }
        let duplicate = dispatch(
            &mut a,
            "mail.send",
            json!({"id":sent["id"],
            "from":"apps","to":"infra","kind":sent["kind"],"body":sent["body"],
            "payload":sent["payload"],"correlation":sent["correlation"],
            "request_kind":sent["request_kind"]}),
        )
        .unwrap();
        assert_eq!(duplicate["duplicate"], true);
    }
}

#[test]
fn rejecting_an_oversized_remote_reply_keeps_the_request_open() {
    let Pair {
        _temp,
        mut a,
        mut b,
        ..
    } = pair();
    send(
        &mut a,
        json!({"id":"question","from":"apps","to":"infra",
        "kind":"request","body":"answer this"}),
    )
    .unwrap();
    let oversized = json!({"id":"answer","from":"infra","to":"apps",
        "kind":"reply","correlation":"question","body":"x".repeat(65_400)});
    assert!(oversized.to_string().len() < 65_536);
    assert!(err(send(&mut b, oversized)).contains("64 KiB"));
    for store in [&mut a, &mut b] {
        assert!(dispatch(store, "mail.show", json!({"id":"answer"})).is_err());
        let request = dispatch(store, "mail.show", json!({"id":"question"})).unwrap();
        assert_eq!(request["state"], "open");
        assert_eq!(request["answered_by"], Value::Null);
    }
    send(
        &mut b,
        json!({"id":"answer","from":"infra","to":"apps",
        "kind":"reply","correlation":"question","body":"answer"}),
    )
    .unwrap();
    assert_eq!(
        dispatch(&mut a, "mail.show", json!({"id":"question"})).unwrap()["state"],
        "answered"
    );
}

#[test]
fn undeliverable_mail_stays_pending_until_flushed() {
    let Pair {
        _temp,
        mut a,
        mut b,
        b_home,
        peer,
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
        json!({"name":"home-b","transport":"local","executable":peer,"home":b_home}),
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
fn an_unavailable_peers_backlog_does_not_hide_another_peers_mail() {
    let Pair {
        _temp,
        mut a,
        mut b,
        b_home,
        peer,
    } = pair();
    dispatch(
        &mut a,
        "peer.put",
        json!({"name":"broken","transport":"local",
        "executable":"/nonexistent/privateer","home":b_home}),
    )
    .unwrap();
    dispatch(
        &mut a,
        "ship.put",
        json!({"name":"blocked","location":{"peer":"broken"}}),
    )
    .unwrap();
    dispatch(
        &mut a,
        "peer.put",
        json!({"name":"home-b","transport":"local",
        "executable":"/nonexistent/privateer","home":b_home}),
    )
    .unwrap();
    for n in 0..60 {
        send(
            &mut a,
            json!({"id":format!("blocked-{n}"),"from":"apps","to":"blocked",
            "kind":"notice","body":"durable pending mail"}),
        )
        .unwrap();
    }
    send(
        &mut a,
        json!({"id":"reachable","from":"apps","to":"infra",
        "kind":"notice","body":"this peer recovered"}),
    )
    .unwrap();
    dispatch(
        &mut a,
        "peer.put",
        json!({"name":"home-b","transport":"local",
        "executable":peer,"home":b_home}),
    )
    .unwrap();
    // A manual flush retries a failed peer only once, then reaches the other
    // peer even though the unavailable peer owns more than the page limit.
    let report = dispatch(&mut a, "mail.flush", json!({})).unwrap();
    assert_eq!(report["delivered"], 1, "{report}");
    assert_eq!(report["attempted"], 2);
    assert_eq!(report["pending"], 60);
    let inbox = dispatch(&mut b, "mail.inbox", json!({"address":"infra"})).unwrap();
    assert_eq!(inbox["messages"][0]["id"], "reachable");
    let facts: i64 = a
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM mail WHERE delivery='pending' AND recipient='blocked'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(facts, 60);
}

#[test]
fn automatic_delivery_backoff_survives_restart_and_preserves_peer_order() {
    let Pair {
        _temp,
        mut a,
        mut b,
        b_home,
        peer,
    } = pair();
    dispatch(
        &mut a,
        "peer.put",
        json!({"name":"home-b","transport":"local",
        "executable":"/nonexistent/privateer","home":b_home}),
    )
    .unwrap();
    for id in ["first", "second"] {
        send(
            &mut a,
            json!({"id":id,"from":"apps","to":"infra",
            "kind":"notice","body":id}),
        )
        .unwrap();
    }
    // Do not depend on finishing restart/configuration within the real
    // one-second retry delay on a loaded machine.
    a.connection()
        .execute(
            "UPDATE mail_delivery_retry SET next_attempt_ms=?1",
            [i64::MAX],
        )
        .unwrap();
    let home = a.home().to_owned();
    drop(a);
    let mut a = Store::open(&home).unwrap();
    dispatch(
        &mut a,
        "peer.put",
        json!({"name":"home-b","transport":"local",
        "executable":peer,"home":b_home}),
    )
    .unwrap();
    let paused = privateer::activation::tick(&mut a).unwrap();
    assert_eq!(paused["delivered"]["attempted"], 0, "{paused}");
    let attempts: Vec<i64> = a
        .connection()
        .prepare("SELECT delivery_attempts FROM mail ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(attempts, vec![1, 0]);
    // Advance the durable retry clock without sleeping in the test.
    a.connection()
        .execute("UPDATE mail_delivery_retry SET next_attempt_ms=0", [])
        .unwrap();
    let delivered = privateer::activation::tick(&mut a).unwrap();
    assert_eq!(delivered["delivered"]["delivered"], 2, "{delivered}");
    assert_eq!(delivered["delivered"]["pending"], 0);
    let inbox = dispatch(&mut b, "mail.inbox", json!({"address":"infra"})).unwrap();
    let ids: Vec<_> = inbox["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["first", "second"]);
}

#[test]
fn peer_order_does_not_allow_an_agent_to_deliver_or_bypass_another_senders_mail() {
    let Pair {
        _temp,
        mut a,
        mut b,
        b_home,
        peer,
    } = pair();
    dispatch(&mut a, "ship.put", json!({"name":"research"})).unwrap();
    dispatch(
        &mut b,
        "ship.put",
        json!({"name":"research","location":{"peer":"home-a"}}),
    )
    .unwrap();
    dispatch(
        &mut a,
        "peer.put",
        json!({"name":"home-b","transport":"local",
        "executable":"/nonexistent/privateer","home":b_home}),
    )
    .unwrap();
    send(
        &mut a,
        json!({"id":"apps-first","from":"apps","to":"infra",
        "kind":"notice","body":"first"}),
    )
    .unwrap();
    a.set_caller(Caller::Agent {
        actor: "research".into(),
    });
    send(
        &mut a,
        json!({"id":"research-second","from":"research","to":"infra",
        "kind":"notice","body":"second"}),
    )
    .unwrap();
    a.set_caller(Caller::Operator);
    dispatch(
        &mut a,
        "peer.put",
        json!({"name":"home-b","transport":"local",
        "executable":peer,"home":b_home}),
    )
    .unwrap();
    a.set_caller(Caller::Agent {
        actor: "research".into(),
    });
    let blocked = dispatch(&mut a, "mail.flush", json!({})).unwrap();
    assert_eq!(blocked["attempted"], 0);
    assert_eq!(
        blocked["pending"], 1,
        "only this agent's pending mail is counted"
    );
    assert!(
        dispatch(&mut b, "mail.inbox", json!({"address":"infra"})).unwrap()["messages"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    a.set_caller(Caller::Operator);
    let delivered = dispatch(&mut a, "mail.flush", json!({})).unwrap();
    assert_eq!(delivered["delivered"], 2);
    let inbox = dispatch(&mut b, "mail.inbox", json!({"address":"infra"})).unwrap();
    let ids: Vec<_> = inbox["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["apps-first", "research-second"]);
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

#[test]
fn naming_another_ships_task_or_peer_points_to_the_cross_ship_link() {
    let (_temp, mut store) = fleet();
    let wrong = err(send(
        &mut store,
        json!({"from":"apps","to":"infra","kind":"notice","task_id":"their-task","body":"done"}),
    ));
    assert!(wrong.contains("unknown task their-task"), "{wrong}");
    assert!(wrong.contains("correlation"), "{wrong}");
    let peer = err(dispatch(
        &mut store,
        "peer.call",
        json!({"name":"infra","command":"status","args":{}}),
    ));
    assert!(peer.contains("peer infra is not configured"), "{peer}");
    assert!(peer.contains("fleet-<ship>"), "{peer}");
}

#[test]
fn withdrawal_is_sender_only_durable_and_not_an_ack_or_reply() {
    let (temp, mut s) = fleet();
    let request = send(&mut s, json!({"id":"question","from":"apps","to":"captain","kind":"decision","body":"Choose?","task_id":"main"})).unwrap();
    dispatch(
        &mut s,
        "mail.ack",
        json!({"address":"captain","through_seq":request["seq"]}),
    )
    .unwrap();
    assert_eq!(
        dispatch(&mut s, "runtime.needs", json!({})).unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        dispatch(
            &mut s,
            "mail.withdraw",
            json!({"actor":"infra","id":"question","reason":"obsolete"})
        )
        .is_err()
    );
    s.set_caller(Caller::Agent {
        actor: "apps".into(),
    });
    assert!(dispatch(&mut s, "runtime.needs", json!({})).is_err());
    assert!(
        dispatch(
            &mut s,
            "mail.withdraw",
            json!({"actor":"infra","id":"question","reason":"obsolete"})
        )
        .is_err()
    );
    // A client may legitimately occupy the old predictable runtime ID.
    let occupied_id = legacy_outcome_id("end", "question");
    let occupied = send(&mut s, json!({"id":occupied_id,"from":"apps","to":"captain","kind":"notice","body":"unrelated history"})).unwrap();
    // Existing actor-omitted callers resolve to the actual agent, not the
    // request sender; explicit form remains an idempotent retry.
    let args = json!({"actor":"apps","id":"question","reason":"obsolete"});
    let closed = dispatch(
        &mut s,
        "mail.withdraw",
        json!({"id":"question","reason":"obsolete"}),
    )
    .unwrap();
    assert_eq!(closed["state"], "withdrawn");
    assert!(closed["answered_by"].is_null());
    assert!(!closed["ended_by"].is_null());
    assert_ne!(closed["ended_by"], occupied_id);
    assert_eq!(
        dispatch(&mut s, "mail.show", json!({"id":occupied_id})).unwrap(),
        occupied
    );
    assert_eq!(dispatch(&mut s, "mail.withdraw", args).unwrap(), closed);
    assert!(
        dispatch(
            &mut s,
            "mail.withdraw",
            json!({"actor":"apps","id":"question","reason":"changed"})
        )
        .is_err()
    );
    drop(s);
    let mut s = Store::open(&temp.path().join("home")).unwrap();
    assert_eq!(
        dispatch(&mut s, "mail.show", json!({"id":"question"})).unwrap()["state"],
        "withdrawn"
    );
    assert!(
        dispatch(&mut s, "runtime.needs", json!({})).unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(send(&mut s,json!({"from":"captain","to":"apps","kind":"reply","correlation":"question","body":"late"})).is_err());
}

#[test]
fn omitted_withdraw_actor_never_impersonates_sender_or_captain() {
    let (_temp, mut s) = fleet();
    send(&mut s,json!({"id":"captains-question","from":"captain","to":"apps","kind":"request","body":"Ready?"})).unwrap();
    send(&mut s,json!({"id":"apps-question","from":"apps","to":"captain","kind":"request","body":"Choose?"})).unwrap();
    s.set_caller(Caller::Agent {
        actor: "infra".into(),
    });
    for id in ["captains-question", "apps-question"] {
        assert!(
            dispatch(
                &mut s,
                "mail.withdraw",
                json!({"id":id,"reason":"not mine"})
            )
            .is_err()
        );
        assert!(
            dispatch(
                &mut s,
                "mail.withdraw",
                json!({"id":id,"actor":"captain","reason":"not mine"})
            )
            .is_err()
        );
    }
    s.set_caller(Caller::Operator);
    assert!(
        dispatch(
            &mut s,
            "mail.withdraw",
            json!({"id":"apps-question","reason":"x".repeat(2001)})
        )
        .is_err()
    );
    // Captain-desk operator selection, including the legacy omitted reason.
    for id in ["captains-question", "apps-question"] {
        let closed = dispatch(&mut s, "mail.withdraw", json!({"id":id})).unwrap();
        assert_eq!(closed["state"], "withdrawn");
        assert_eq!(closed["end_reason"], "");
        assert!(closed["answered_by"].is_null());
        assert!(closed["remote"].is_null());
        if id == "captains-question" {
            let notice_id = closed["ended_by"].as_str().unwrap();
            let notice = dispatch(&mut s, "mail.show", json!({"id":notice_id})).unwrap();
            assert_eq!(
                notice["body"],
                "The captain ended captains-question (withdrawn)."
            );
            assert_eq!(notice["payload"]["request_end"]["reason"], "");
            assert!(s.connection().query_row("SELECT EXISTS(SELECT 1 FROM audit WHERE kind='captain.send' AND json_extract(payload,'$.id')=?1)", [notice_id], |r| r.get::<_, bool>(0)).unwrap());
        }
    }
    assert_eq!(s.task("main").unwrap()["owner"], "apps");
}

#[test]
fn withdrawal_outbox_recovers_after_transport_failure_without_new_closure() {
    let Pair {
        _temp,
        mut a,
        mut b,
        b_home,
        peer,
    } = pair();
    send(&mut a,json!({"id":"retry-end","from":"apps","to":"infra","kind":"request","body":"Still needed?"})).unwrap();
    dispatch(&mut a,"peer.put",json!({"name":"home-b","transport":"local","executable":"/nonexistent/privateer","home":b_home})).unwrap();
    a.set_caller(Caller::Agent {
        actor: "apps".into(),
    });
    let args = json!({"id":"retry-end","reason":"obsolete"});
    let closed = dispatch(&mut a, "mail.withdraw", args.clone()).unwrap();
    assert_eq!(closed["state"], "withdrawn");
    assert_ne!(closed["remote"], "withdrawn");
    assert_ne!(closed["remote"], "unknown");
    assert_eq!(
        dispatch(&mut b, "mail.show", json!({"id":"retry-end"})).unwrap()["state"],
        "open"
    );
    let home = a.home().to_owned();
    drop(a);
    let mut a = Store::open(&home).unwrap();
    a.set_caller(Caller::Agent {
        actor: "apps".into(),
    });
    // Retry after restart retains the same closure even while transport fails.
    let waiting = dispatch(&mut a, "mail.withdraw", args.clone()).unwrap();
    assert_eq!(waiting["ended_by"], closed["ended_by"]);
    assert_ne!(waiting["remote"], "withdrawn");
    a.set_caller(Caller::Operator);
    dispatch(
        &mut a,
        "peer.put",
        json!({"name":"home-b","transport":"local","executable":peer,"home":b_home}),
    )
    .unwrap();
    a.set_caller(Caller::Agent {
        actor: "apps".into(),
    });
    assert_eq!(
        dispatch(&mut a, "mail.flush", json!({})).unwrap()["delivered"],
        1
    );
    let recovered = dispatch(&mut a, "mail.withdraw", args).unwrap();
    assert_eq!(recovered["remote"], "withdrawn");
    assert_eq!(recovered["ended_by"], closed["ended_by"]);
    let theirs = dispatch(&mut b, "mail.show", json!({"id":"retry-end"})).unwrap();
    assert_eq!(theirs["ended_by"], closed["ended_by"]);
    let notice = dispatch(&mut a, "mail.show", json!({"id":closed["ended_by"]})).unwrap();
    let wire = json!({"id":notice["id"],"from":"apps","to":"infra","kind":"notice","correlation":"retry-end","body":notice["body"],"payload":notice["payload"]});
    let duplicate = dispatch(&mut b, "mail.deliver", wire.clone()).unwrap();
    assert_eq!(duplicate["duplicate"], true);
    assert_eq!(duplicate["request_end"]["ended_by"], closed["ended_by"]);
    assert_eq!(duplicate["request_end"]["state"], "withdrawn");
    send(
        &mut a,
        json!({"id":"empty-reason","from":"apps","to":"infra","kind":"request","body":"Needed?"}),
    )
    .unwrap();
    let legacy = dispatch(&mut a, "mail.withdraw", json!({"id":"empty-reason"})).unwrap();
    assert_eq!(legacy["remote"], "withdrawn");
    assert_eq!(
        dispatch(&mut b, "mail.show", json!({"id":"empty-reason"})).unwrap()["end_reason"],
        ""
    );
    assert!(recovered["answered_by"].is_null());
    assert_eq!(
        dispatch(&mut b, "mail.inbox", json!({"address":"infra"})).unwrap()["acknowledged_through"],
        0
    );
    // Ordinary retention can remove the acknowledged closed request while
    // its unread closure remains. Redelivery must still succeed as a no-op,
    // with unknown application evidence rather than blocking the peer FIFO.
    dispatch(
        &mut b,
        "mail.ack",
        json!({"address":"infra","through_seq":theirs["seq"]}),
    )
    .unwrap();
    assert_eq!(
        privateer::mail::prune(&b, Store::now_ms() + 1, false).unwrap(),
        1
    );
    assert!(dispatch(&mut b, "mail.show", json!({"id":"retry-end"})).is_err());
    let retained = dispatch(&mut b, "mail.show", json!({"id":closed["ended_by"]})).unwrap();
    let redelivered = dispatch(&mut b, "mail.deliver", wire).unwrap();
    assert_eq!(redelivered["duplicate"], true);
    assert!(redelivered["request_end"].is_null());
    assert_eq!(
        dispatch(&mut b, "mail.show", json!({"id":closed["ended_by"]})).unwrap(),
        retained
    );
}

#[test]
fn transport_results_require_matching_closure_receipt_for_remote_state() {
    for (response, error) in [
        ("{'ok':True,'result':{'request_end':None}}", None),
        (
            "{'ok':True,'result':{'request_end':{'id':'wrong-request','state':'withdrawn','ended_by':r['args']['id']}}}",
            None,
        ),
        (
            "{'ok':True,'result':{'request_end':{'id':r['args'].get('correlation'),'state':'withdrawn','ended_by':'wrong-closure'}}}",
            None,
        ),
        ("{'ok':False,'error':'withdrawn'}", Some("withdrawn")),
        ("{'ok':False,'error':'answered'}", Some("answered")),
    ] {
        let Pair {
            _temp,
            mut a,
            mut b,
            b_home,
            ..
        } = pair();
        // Deliver the initial request to the real disposable receiver without
        // inheriting the job runner's identity or machine-wide fleet registry.
        let peer = _temp.path().join("legacy-peer");
        let binary = json!(env!("CARGO_BIN_EXE_privateer"));
        write_isolated_peer(
            &peer,
            &format!("os.execv({binary}, [{binary}] + sys.argv[1:])"),
        );
        dispatch(
            &mut a,
            "peer.put",
            json!({"name":"home-b","transport":"local","executable":peer,"home":b_home}),
        )
        .unwrap();
        let request = send(
            &mut a,
            json!({"id":"legacy-end","from":"apps","to":"infra","kind":"request","body":"Needed?"}),
        )
        .unwrap();
        assert_eq!(request["delivery"], "delivered", "{request}");
        // Then either acknowledge transport without applying the closure, or
        // fail with state-like error text. No real home or config is reached.
        write_isolated_peer(
            &peer,
            &format!(
                "r=json.loads(sys.stdin.readline())\nresponse={response}\nresponse['id']=r['id']\nprint(json.dumps(response))"
            ),
        );
        let result = dispatch(
            &mut a,
            "mail.withdraw",
            json!({"id":"legacy-end","reason":"obsolete"}),
        )
        .unwrap();
        let expected_remote = error
            .map(|e| format!("error: {e}"))
            .unwrap_or_else(|| "unknown".to_owned());
        assert_eq!(result["remote"], expected_remote, "{result}");
        assert_ne!(result["remote"], "withdrawn");
        assert_eq!(result["state"], "withdrawn");
        assert!(result["answered_by"].is_null());
        let notice = dispatch(&mut a, "mail.show", json!({"id":result["ended_by"]})).unwrap();
        assert_eq!(
            notice["delivery"],
            if error.is_some() {
                "pending"
            } else {
                "delivered"
            }
        );
        assert_eq!(notice["delivery_error"], json!(error));
        assert_eq!(notice["delivery_attempts"], 1);
        assert_eq!(
            dispatch(&mut b, "mail.show", json!({"id":"legacy-end"})).unwrap()["state"],
            "open"
        );
        // Closure, raw transport failure and lack of application evidence
        // survive reopening the disposable sender home.
        let durable = dispatch(&mut a, "mail.show", json!({"id":"legacy-end"})).unwrap();
        let home = a.home().to_owned();
        drop(a);
        let mut a = Store::open(&home).unwrap();
        assert_eq!(
            dispatch(&mut a, "mail.show", json!({"id":"legacy-end"})).unwrap(),
            durable
        );
        assert_eq!(
            dispatch(&mut a, "mail.show", json!({"id":result["ended_by"]})).unwrap(),
            notice
        );
        let applied: Option<String> = a
            .connection()
            .query_row(
                "SELECT remote_end_state FROM mail WHERE id=?1",
                [result["ended_by"].as_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(applied, None);
    }
}

#[test]
fn terminal_task_closes_linked_questions_and_preserves_history() {
    for status in ["done", "cancelled"] {
        let (_temp, mut s) = fleet();
        send(&mut s,json!({"id":"linked","from":"apps","to":"captain","kind":"request","body":"question","task_id":"main"})).unwrap();
        send(&mut s,json!({"id":"unlinked","from":"apps","to":"captain","kind":"request","body":"question"})).unwrap();
        let occupied_id = legacy_outcome_id("end", "linked");
        let occupied = send(&mut s,json!({"id":occupied_id,"from":"apps","to":"captain","kind":"notice","body":"prior history"})).unwrap();
        dispatch(
            &mut s,
            "task.handoff",
            json!({"actor":"apps","id":"main","target":"infra"}),
        )
        .unwrap();
        dispatch(&mut s, "task.accept", json!({"actor":"infra","id":"main"})).unwrap();
        // Custody does not confer authorship of the old owner's requests.
        assert!(
            dispatch(
                &mut s,
                "mail.withdraw",
                json!({"actor":"infra","id":"linked","reason":"not mine"})
            )
            .is_err()
        );
        dispatch(
            &mut s,
            "task.update",
            json!({"actor":"infra","id":"main","status":status}),
        )
        .unwrap();
        let linked = dispatch(&mut s, "mail.show", json!({"id":"linked"})).unwrap();
        assert_eq!(linked["state"], format!("task_{status}"));
        assert_eq!(s.task("main").unwrap()["owner"], "infra");
        assert_ne!(linked["ended_by"], occupied_id);
        assert_eq!(
            dispatch(&mut s, "mail.show", json!({"id":occupied_id})).unwrap(),
            occupied
        );
        assert!(linked["answered_by"].is_null());
        assert_eq!(
            dispatch(&mut s, "runtime.needs", json!({})).unwrap()["items"][0]["source_id"],
            "unlinked"
        );
        assert!(
            send(
                &mut s,
                json!({"from":"apps","to":"captain","kind":"request","body":"new","task_id":"main"})
            )
            .is_err()
        );
    }
}

#[test]
fn closure_crosses_homes_and_inflight_reply_wins_without_reopening() {
    let Pair {
        _temp,
        mut a,
        mut b,
        ..
    } = pair();
    send(
        &mut a,
        json!({"id":"withdraw-me","from":"apps","to":"infra","kind":"request","body":"question"}),
    )
    .unwrap();
    // Local clients cannot forge closures; malformed/unauthored remote
    // closures roll back both the notice and the request change.
    assert!(send(&mut a,json!({"id":"local-spoof","from":"apps","to":"infra","kind":"notice","correlation":"withdraw-me","payload":{"request_end":{"version":1,"state":"withdrawn","reason":"fake"}}})).is_err());
    dispatch(
        &mut b,
        "ship.put",
        json!({"name":"research","location":{"peer":"home-a"}}),
    )
    .unwrap();
    let wire = json!({"id":"bad-closure","from":"apps","to":"infra","kind":"notice","correlation":"withdraw-me","payload":{"request_end":{"version":1,"state":"withdrawn","reason":"obsolete"}}});
    let before: i64 = b
        .connection()
        .query_row("SELECT COUNT(*) FROM mail", [], |r| r.get(0))
        .unwrap();
    for (field, value) in [
        ("from", json!("research")),
        ("to", json!("captain")),
        ("kind", json!("request")),
        ("correlation", Value::Null),
        ("correlation", json!("unknown")),
    ] {
        let mut bad = wire.clone();
        bad[field] = value;
        assert!(dispatch(&mut b, "mail.deliver", bad).is_err());
    }
    for end in [
        json!({"version":2,"state":"withdrawn","reason":"obsolete"}),
        json!({"version":1,"state":"approved","reason":"fake"}),
        json!({"version":1,"state":"withdrawn","reason":null}),
        json!({"version":1,"state":"withdrawn","reason":"x".repeat(2001)}),
    ] {
        let mut bad = wire.clone();
        bad["payload"]["request_end"] = end;
        assert!(dispatch(&mut b, "mail.deliver", bad).is_err());
    }
    assert_eq!(
        b.connection()
            .query_row("SELECT COUNT(*) FROM mail", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        before
    );
    assert_eq!(
        dispatch(&mut b, "mail.show", json!({"id":"withdraw-me"})).unwrap()["state"],
        "open"
    );
    let occupied_id = legacy_outcome_id("end", "withdraw-me");
    let occupied = send(&mut a,json!({"id":occupied_id,"from":"apps","to":"infra","kind":"notice","body":"old unrelated wire history"})).unwrap();
    let result = dispatch(
        &mut a,
        "mail.withdraw",
        json!({"actor":"apps","id":"withdraw-me","reason":"no longer needed"}),
    )
    .unwrap();
    assert_eq!(result["remote"], "withdrawn");
    assert_eq!(
        dispatch(&mut b, "mail.show", json!({"id":"withdraw-me"})).unwrap()["state"],
        "withdrawn"
    );
    assert_ne!(
        dispatch(&mut b, "mail.show", json!({"id":"withdraw-me"})).unwrap()["ended_by"],
        occupied_id
    );
    let historical = dispatch(&mut b, "mail.show", json!({"id":occupied_id})).unwrap();
    assert_eq!(historical["body"], occupied["body"]);
    // A new reply after observing closure is refused.
    assert!(send(&mut b,json!({"from":"infra","to":"apps","kind":"reply","correlation":"withdraw-me","body":"late"})).is_err());
    // Hold only B's fixture outbox, so its real reply is committed before A's
    // withdrawal but crosses the subprocess boundary afterwards.
    send(
        &mut a,
        json!({"id":"race-wire","from":"apps","to":"infra","kind":"request","body":"question"}),
    )
    .unwrap();
    b.connection()
        .execute(
            "INSERT INTO mail_delivery_retry VALUES('home-a',1,?1,0)",
            [Store::now_ms() + 60_000],
        )
        .unwrap();
    let reply = send(&mut b,json!({"id":"inflight","from":"infra","to":"apps","kind":"reply","correlation":"race-wire","body":"already sent"})).unwrap();
    assert_eq!(reply["delivery"], "pending");
    let crossed = dispatch(
        &mut a,
        "mail.withdraw",
        json!({"actor":"apps","id":"race-wire","reason":"obsolete"}),
    )
    .unwrap();
    assert_ne!(
        crossed["remote"], "withdrawn",
        "an inflight answer was not withdrawn"
    );
    assert_eq!(
        dispatch(&mut b, "mail.show", json!({"id":"race-wire"})).unwrap()["state"],
        "answered"
    );
    assert_eq!(
        dispatch(&mut b, "mail.flush", json!({})).unwrap()["delivered"],
        1
    );
    for store in [&mut a, &mut b] {
        assert_eq!(
            dispatch(store, "mail.show", json!({"id":"race-wire"})).unwrap()["state"],
            "answered"
        );
    }
    let wire = json!({"id":"inflight","from":"infra","to":"apps","kind":"reply","correlation":"race-wire","body":"already sent"});
    assert_eq!(
        dispatch(&mut a, "mail.deliver", wire).unwrap()["duplicate"],
        true
    );
}

#[test]
fn captain_needs_stable_ids_pagination_and_exact_approval_actions() {
    let (temp, mut s) = fleet();
    send(&mut s,json!({"id":"decision","from":"apps","to":"captain","kind":"decision","body":"Which?","payload":{"choices":["a","b"]}})).unwrap();
    send(&mut s,json!({"id":"release","from":"apps","to":"captain","kind":"request","request_kind":"production_approval","task_id":"main","body":"Release?","payload":{"artifact":"sha256:exact","environment":"production"}})).unwrap();
    let first = dispatch(&mut s, "runtime.needs", json!({"limit":1})).unwrap();
    assert_eq!(first["items"][0]["source_id"], "decision");
    assert_eq!(first["items"][0]["choices"], json!(["a", "b"]));
    let second = dispatch(
        &mut s,
        "runtime.needs",
        json!({"after":first["next_after"]}),
    )
    .unwrap();
    let item = &second["items"][0];
    assert_eq!(item["kind"], "approval");
    let mut approval = json!({"id":"grant-exact","request_id":"release","expected_version":item["version"],"task_id":"main","artifact":"sha256:exact","environment":"production","approver":"captain"});
    let stable = item["id"].clone();
    drop(s);
    let mut s = Store::open(&temp.path().join("home")).unwrap();
    assert_eq!(
        dispatch(
            &mut s,
            "runtime.needs",
            json!({"after":first["next_after"]})
        )
        .unwrap()["items"][0]["id"],
        stable
    );
    // Every supplied correlation field must belong to a complete typed pair.
    let before_mail: i64 = s
        .connection()
        .query_row("SELECT COUNT(*) FROM mail", [], |r| r.get(0))
        .unwrap();
    for (request, version) in [
        (None, Some(approval["expected_version"].clone())),
        (
            Some(Value::Null),
            Some(approval["expected_version"].clone()),
        ),
        (Some(json!(42)), Some(approval["expected_version"].clone())),
        (Some(json!({})), Some(approval["expected_version"].clone())),
        (Some(json!([])), Some(approval["expected_version"].clone())),
        (
            Some(json!(false)),
            Some(approval["expected_version"].clone()),
        ),
        (
            Some(json!("not a valid id")),
            Some(approval["expected_version"].clone()),
        ),
        (Some(json!("release")), Some(json!({}))),
        (Some(json!("release")), Some(json!([]))),
        (Some(json!("release")), Some(json!(false))),
        (Some(json!("release")), None),
        (Some(json!("release")), Some(Value::Null)),
        (Some(json!("release")), Some(json!(42))),
        (Some(json!("")), Some(approval["expected_version"].clone())),
        (Some(json!("release")), Some(json!(""))),
    ] {
        let mut invalid = approval.clone();
        invalid.as_object_mut().unwrap().remove("request_id");
        invalid.as_object_mut().unwrap().remove("expected_version");
        if let Some(v) = request {
            invalid["request_id"] = v;
        }
        if let Some(v) = version {
            invalid["expected_version"] = v;
        }
        assert!(
            dispatch(&mut s, "approval.grant", invalid.clone()).is_err(),
            "{invalid}"
        );
        assert!(dispatch(&mut s, "approval.deny", invalid.clone()).is_err());
        assert!(dispatch(&mut s, "approval.consume", invalid).is_err());
        assert_eq!(
            dispatch(&mut s, "mail.show", json!({"id":"release"})).unwrap()["state"],
            "open"
        );
        assert_eq!(
            s.connection()
                .query_row("SELECT COUNT(*) FROM approvals", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            s.connection()
                .query_row("SELECT COUNT(*) FROM mail", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            before_mail
        );
    }
    // Consume cannot accept correlation, even if correctly bound.
    assert!(dispatch(&mut s, "approval.consume", approval.clone()).is_err());
    let occupied_id = legacy_outcome_id("approval-answer", "release");
    let occupied = send(&mut s, json!({"id":occupied_id,"from":"apps","to":"captain","kind":"notice","body":"unrelated approval history"})).unwrap();
    approval["artifact"] = json!("different");
    assert!(dispatch(&mut s, "approval.grant", approval.clone()).is_err());
    approval["artifact"] = json!("sha256:exact");
    s.set_caller(Caller::Agent {
        actor: "apps".into(),
    });
    assert!(dispatch(&mut s, "approval.grant", approval.clone()).is_err());
    s.set_caller(Caller::Operator);
    dispatch(&mut s, "approval.grant", approval.clone()).unwrap();
    let granted_request = dispatch(&mut s, "mail.show", json!({"id":"release"})).unwrap();
    drop(s);
    let mut s = Store::open(&temp.path().join("home")).unwrap();
    dispatch(&mut s, "approval.grant", approval.clone()).unwrap();
    assert_eq!(
        dispatch(&mut s, "mail.show", json!({"id":"release"})).unwrap(),
        granted_request
    );
    assert_eq!(
        dispatch(&mut s, "mail.show", json!({"id":"release"})).unwrap()["state"],
        "approved"
    );
    assert_ne!(
        dispatch(&mut s, "mail.show", json!({"id":"release"})).unwrap()["answered_by"],
        occupied_id
    );
    assert_eq!(
        dispatch(&mut s, "mail.show", json!({"id":occupied_id})).unwrap(),
        occupied
    );
    assert!(dispatch(&mut s, "approval.deny", approval.clone()).is_err());
    assert!(dispatch(&mut s, "approval.consume", approval.clone()).is_err());
    let mut version_only = approval.clone();
    version_only.as_object_mut().unwrap().remove("request_id");
    assert!(dispatch(&mut s, "approval.consume", version_only).is_err());
    assert_eq!(
        s.connection()
            .query_row(
                "SELECT consumed FROM approvals WHERE id='grant-exact'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    dispatch(&mut s, "approval.consume", json!({"id":"grant-exact","task_id":"main","artifact":"sha256:exact","environment":"production"})).unwrap();
    assert!(dispatch(&mut s, "approval.grant", approval).is_err());
    assert_eq!(
        dispatch(&mut s, "runtime.needs", json!({})).unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn denial_is_distinct_and_ordinary_reply_cannot_close_an_approval() {
    let (temp, mut s) = fleet();
    for id in ["deny", "plain"] {
        send(&mut s,json!({"id":id,"from":"apps","to":"captain","kind":"request","request_kind":"production_approval","task_id":"main","body":"Release?","payload":{"artifact":"abc","environment":"production"}})).unwrap();
    }
    let version =
        dispatch(&mut s, "runtime.needs", json!({})).unwrap()["items"][0]["version"].clone();
    let args = json!({"request_id":"deny","expected_version":version,"task_id":"main","artifact":"abc","environment":"production","approver":"captain"});
    // Even captain-authored ordinary mail with the right-looking payload
    // cannot masquerade as the authoritative response to an open request.
    let occupied_id = legacy_outcome_id("approval-answer", "deny");
    let occupied = send(&mut s,json!({"id":occupied_id,"from":"captain","to":"apps","kind":"notice","correlation":"deny","body":"not a runtime response","payload":{"approval_outcome":"denied","approval_id":null,"artifact":"abc","environment":"production"}})).unwrap();
    let answer = dispatch(&mut s, "approval.deny", args.clone()).unwrap();
    assert_eq!(answer["kind"], "reply");
    assert_ne!(answer["id"], occupied_id);
    drop(s);
    let mut s = Store::open(&temp.path().join("home")).unwrap();
    assert_eq!(dispatch(&mut s, "approval.deny", args).unwrap(), answer);
    assert_eq!(
        dispatch(&mut s, "mail.show", json!({"id":occupied_id})).unwrap(),
        occupied
    );
    let refused = send(&mut s,json!({"from":"captain","to":"apps","kind":"reply","correlation":"plain","body":"approved"})).unwrap_err();
    assert!(refused.to_string().contains("approval.grant"), "{refused}");
    assert_eq!(
        dispatch(&mut s, "mail.show", json!({"id":"plain"})).unwrap()["state"],
        "open"
    );
    assert_eq!(
        s.connection()
            .query_row("SELECT COUNT(*) FROM approvals", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        dispatch(&mut s, "mail.show", json!({"id":"deny"})).unwrap()["state"],
        "denied"
    );
    let needs = dispatch(&mut s, "runtime.needs", json!({})).unwrap();
    assert_eq!(needs["items"].as_array().unwrap().len(), 1);
    assert_eq!(needs["items"][0]["source_id"], "plain");
}

#[test]
fn needs_uses_typed_blockers_not_mail_text_and_is_read_only() {
    let (_temp, mut s) = fleet();
    send(&mut s,json!({"from":"apps","to":"captain","kind":"notice","body":"BLOCKED CI red, production needs approval"})).unwrap();
    dispatch(&mut s,"task.create",json!({"id":"dependency-wait","charter_id":"project","owner":"apps","title":"Await runtime review before UI implementation (intentional scheduling)"})).unwrap();
    dispatch(
        &mut s,
        "task.update",
        json!({"actor":"apps","id":"dependency-wait","status":"blocked"}),
    )
    .unwrap();
    dispatch(
        &mut s,
        "task.update",
        json!({"actor":"apps","id":"main","status":"blocked"}),
    )
    .unwrap();
    let now = Store::now_ms();
    for (id, status) in [("queued", "queued"), ("orphan", "orphaned")] {
        s.connection().execute("INSERT INTO jobs(id,record) VALUES(?1,?2)",rusqlite::params![id,json!({"id":id,"status":status,"task_id":"main","created_ms":now-61_000,"incarnation":"inc","reason":"typed reason"}).to_string()]).unwrap();
    }
    let before: i64 = s
        .connection()
        .query_row("SELECT total_changes()", [], |r| r.get(0))
        .unwrap();
    let result = dispatch(&mut s, "runtime.needs", json!({})).unwrap();
    let after: i64 = s
        .connection()
        .query_row("SELECT total_changes()", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(result["items"].as_array().unwrap().len(), 2);
    assert!(
        result["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["source"] == "job")
    );
    assert!(
        result["unknown"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "task_captain_actionability")
    );
    assert!(
        result["unknown"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "ci")
    );
    dispatch(
        &mut s,
        "task.update",
        json!({"actor":"apps","id":"main","status":"done"}),
    )
    .unwrap();
    let result = dispatch(&mut s, "runtime.needs", json!({})).unwrap();
    assert_eq!(result["items"].as_array().unwrap().len(), 1);
    assert_eq!(result["items"][0]["evidence"], "orphaned_job");
    s.connection()
        .execute(
            "UPDATE jobs SET record=json_set(record,'$.status','succeeded')",
            [],
        )
        .unwrap();
    assert!(
        dispatch(&mut s, "runtime.needs", json!({})).unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let bounded_unavailable = |store: &mut Store| {
        let started = std::time::Instant::now();
        let value = dispatch(store, "runtime.needs", json!({"limit":1})).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(value["available"], false, "{value}");
        assert!(value["items"].as_array().unwrap().is_empty());
        assert!(!value["unknown"].as_array().unwrap().is_empty());
        // No permanent timeout/progress changes to ordinary command connections.
        assert_eq!(
            store
                .connection()
                .query_row("PRAGMA busy_timeout", [], |r| r.get::<_, u64>(0))
                .unwrap(),
            10_000
        );
        assert_eq!(store.connection().query_row("WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<200000) SELECT sum(x) FROM n",[],|r| r.get::<_,i64>(0)).unwrap(),20_000_100_000);
    };
    // Oversized JSON is refused before an uninterruptible Rust parse/JSON opcode.
    s.connection()
        .execute(
            "UPDATE jobs SET record=?1 WHERE id='queued'",
            [json!({"status":"succeeded","large":"x".repeat(70_000)}).to_string()],
        )
        .unwrap();
    bounded_unavailable(&mut s);
    s.connection().execute("DELETE FROM jobs", []).unwrap();
    // A real large table forces scan/JSON/sort work despite limit:1. It cannot
    // quietly return [] after the work budget is exhausted.
    s.connection().execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<100000)
        INSERT INTO jobs SELECT 'large-'||x,'{\"task_id\":\"main\",\"status\":\"succeeded\",\"created_ms\":0}' FROM n;").unwrap();
    bounded_unavailable(&mut s);
    s.connection().execute("DELETE FROM jobs", []).unwrap();
    // Pathological metadata CPU work is covered before any item query as well.
    s.connection().execute_batch("ALTER TABLE metadata RENAME TO saved_metadata;
        CREATE VIEW metadata AS SELECT key,value FROM saved_metadata WHERE
        (WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<100000000) SELECT sum(x) FROM n)>0;").unwrap();
    bounded_unavailable(&mut s);
}

#[test]
fn reply_withdraw_and_terminal_update_serialize_without_lost_facts() {
    for _ in 0..8 {
        let (temp, mut s) = fleet();
        send(&mut s,json!({"id":"race","from":"apps","to":"captain","kind":"request","body":"question","task_id":"main"})).unwrap();
        let home = temp.path().join("home");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let handles: Vec<_> = [("mail.withdraw",json!({"actor":"apps","id":"race","reason":"obsolete"})),("mail.send",json!({"id":"race-answer","from":"captain","to":"apps","kind":"reply","correlation":"race","body":"answer"})),("task.update",json!({"actor":"apps","id":"main","status":"done"}))].into_iter().map(|(cmd,args)| {
            let barrier = barrier.clone(); let home = home.clone();
            std::thread::spawn(move || { let mut s = Store::open(&home).unwrap(); barrier.wait(); dispatch(&mut s,cmd,args).is_ok() })
        }).collect();
        for h in handles {
            h.join().unwrap();
        }
        let request = dispatch(&mut s, "mail.show", json!({"id":"race"})).unwrap();
        assert!(matches!(
            request["state"].as_str(),
            Some("answered" | "withdrawn" | "task_done")
        ));
        let replies: i64 = s
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM mail WHERE kind='reply' AND correlation='race'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(replies, if request["state"] == "answered" { 1 } else { 0 });
        assert!(
            dispatch(&mut s, "runtime.needs", json!({})).unwrap()["items"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn stale_approval_version_is_not_grantable_and_disappears() {
    let (_temp, mut s) = fleet();
    send(&mut s,json!({"id":"stale","from":"apps","to":"captain","kind":"request","request_kind":"production_approval","task_id":"main","body":"Release?","payload":{"artifact":"abc","environment":"production"}})).unwrap();
    let version =
        dispatch(&mut s, "runtime.needs", json!({})).unwrap()["items"][0]["version"].clone();
    let original = s.charter("project").unwrap();
    let origin: String = s
        .connection()
        .query_row(
            "SELECT value FROM metadata WHERE key='runtime_origin'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let request = dispatch(&mut s, "mail.show", json!({"id":"stale"})).unwrap();
    let task = s.task("main").unwrap();
    // Corrupt JSON and structurally invalid authority are NOT revoked/stale.
    // In particular, neither may turn this exact actionable approval into an
    // available:true empty page. No read may repair, ack or change custody.
    let mut invalid = original.clone();
    invalid["active"] = json!("not-a-boolean");
    for source in ["not-json".to_owned(), invalid.to_string()] {
        s.connection()
            .execute("UPDATE charters SET data=?1 WHERE id='project'", [&source])
            .unwrap();
        let before = s.connection().total_changes();
        let value = dispatch(&mut s, "runtime.needs", json!({})).unwrap();
        assert_eq!(value["available"], false, "{value}");
        assert!(value["items"].as_array().unwrap().is_empty());
        assert!(value["next_after"].is_null());
        assert_eq!(value["unknown"], json!(["runtime_observation"]));
        assert_eq!(s.connection().total_changes(), before);
        assert_eq!(s.task("main").unwrap(), task);
        assert_eq!(
            dispatch(&mut s, "mail.show", json!({"id":"stale"})).unwrap(),
            request
        );
        assert_eq!(
            s.connection()
                .query_row(
                    "SELECT value FROM metadata WHERE key='runtime_origin'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            origin
        );
        assert_eq!(
            s.connection()
                .query_row("PRAGMA busy_timeout", [], |r| r.get::<_, u64>(0))
                .unwrap(),
            10_000
        );
        assert_eq!(s.connection().query_row("WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<200000) SELECT sum(x) FROM n", [], |r| r.get::<_,i64>(0)).unwrap(), 20_000_100_000);
    }
    let mut revoked = original.clone();
    revoked["active"] = json!(false);
    s.connection()
        .execute(
            "UPDATE charters SET data=?1 WHERE id='project'",
            [revoked.to_string()],
        )
        .unwrap();
    let value = dispatch(&mut s, "runtime.needs", json!({})).unwrap();
    assert_eq!(value["available"], true);
    assert!(value["items"].as_array().unwrap().is_empty());
    s.connection()
        .execute(
            "UPDATE charters SET data=?1 WHERE id='project'",
            [original.to_string()],
        )
        .unwrap();
    let value = dispatch(&mut s, "runtime.needs", json!({})).unwrap();
    assert_eq!(value["available"], true);
    assert_eq!(value["items"][0]["version"], version);
    assert_eq!(value["items"][0]["source_id"], "stale");
    s.connection()
        .execute(
            "UPDATE charters SET data=json_set(data,'$.objective','changed') WHERE id='project'",
            [],
        )
        .unwrap();
    assert!(dispatch(&mut s,"approval.grant",json!({"id":"stale-grant","request_id":"stale","expected_version":version,"task_id":"main","artifact":"abc","environment":"production","approver":"captain"})).is_err());
    assert!(
        dispatch(&mut s, "runtime.needs", json!({})).unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn terminal_task_closure_recovers_outbox_across_restart_and_redelivery() {
    let Pair {
        _temp,
        mut a,
        mut b,
        ..
    } = pair();
    let home = a.home().to_owned();
    dispatch(
        &mut a,
        "charter.put",
        json!({"id":"project","repo":_temp.path(),"objective":"Fixture","min_free_bytes":0}),
    )
    .unwrap();
    dispatch(
        &mut a,
        "task.create",
        json!({"id":"main","charter_id":"project","owner":"apps","title":"Fixture"}),
    )
    .unwrap();
    send(&mut a,json!({"id":"linked-remote","from":"apps","to":"infra","kind":"request","body":"question","task_id":"main"})).unwrap();
    dispatch(
        &mut a,
        "task.update",
        json!({"id":"main","actor":"apps","status":"done"}),
    )
    .unwrap();
    assert_eq!(
        dispatch(&mut b, "mail.show", json!({"id":"linked-remote"})).unwrap()["state"],
        "open"
    );
    let local = dispatch(&mut a, "mail.show", json!({"id":"linked-remote"})).unwrap();
    let closure_id = local["ended_by"].clone();
    drop(a);
    let mut a = Store::open(&home).unwrap();
    assert_eq!(
        dispatch(&mut a, "mail.flush", json!({})).unwrap()["delivered"],
        1
    );
    let remote = dispatch(&mut b, "mail.show", json!({"id":"linked-remote"})).unwrap();
    assert_eq!(remote["state"], "task_done");
    assert!(remote["task_id"].is_null());
    assert_eq!(remote["ended_by"], closure_id);
    let closure = dispatch(&mut b, "mail.show", json!({"id":closure_id})).unwrap();
    assert_eq!(closure["kind"], "notice");
    assert_eq!(closure["correlation"], "linked-remote");
    assert_eq!(
        dispatch(&mut b, "mail.deliver", closure).unwrap()["duplicate"],
        true
    );
    assert_eq!(
        dispatch(&mut b, "mail.show", json!({"id":"linked-remote"})).unwrap()["state"],
        "task_done"
    );
    // Historical notices may still reference the terminal task.
    send(
        &mut a,
        json!({"from":"apps","to":"infra","kind":"notice","body":"finished","task_id":"main"}),
    )
    .unwrap();
}

#[test]
fn needs_names_capacity_held_by_no_live_process_and_leaves_ordinary_waits_alone() {
    let (temp, mut s) = fleet();
    let ledger = temp.path().join("ledger");
    std::fs::write(
        temp.path().join("home/config.json"),
        json!({"resource_home": ledger}).to_string(),
    )
    .unwrap();
    let now = Store::now_ms();
    let job = json!({"id":"build","status":"queued","heavy":true,"task_id":"main",
        "created_ms":now-61_000,"incarnation":"inc","reason":"waiting for capacity"});
    s.connection()
        .execute(
            "INSERT INTO jobs(id,record) VALUES('build',?1)",
            [job.to_string()],
        )
        .unwrap();
    let evidence = |s: &mut Store| {
        let needs = dispatch(s, "runtime.needs", json!({})).unwrap();
        needs["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["source_id"] == "build")
            .unwrap()
            .clone()
    };
    // Waiting behind live work, or nothing at all, is an ordinary wait.
    assert_eq!(evidence(&mut s)["evidence"], "capacity_wait");
    dispatch(&mut s, "resource.status", json!({})).unwrap();
    let key = "/elsewhere:gone:inc";
    let held = json!({"key":key,"home":"/elsewhere","job_id":"gone","incarnation":"inc","heavy":true,
        "state":"orphaned","created_ms":now-120_000,
        "supervisor":{"pid":1_073_741_823,"started":"linux:test:1","zombie":false},"payload":null});
    let conn = rusqlite::Connection::open(ledger.join("resources.sqlite")).unwrap();
    conn.execute(
        "INSERT INTO reservations(key,record) VALUES(?1,?2)",
        rusqlite::params![key, held.to_string()],
    )
    .unwrap();
    let item = evidence(&mut s);
    assert_eq!(item["evidence"], "stuck_heavy_capacity", "{item}");
    let recover = item["blocking"][0]["recover"].as_str().unwrap();
    assert!(
        recover.contains("resource.recover")
            && recover.contains(key)
            && recover.contains("\"force\":true"),
        "{recover}"
    );
    // Observing never releases anything.
    let kept: i64 = conn
        .query_row("SELECT count(*) FROM reservations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(kept, 1);
}

/// The captain's reply to a decision, carrying one typed answer.
fn answer(store: &mut Store, id: &str, correlation: &str, answer: Value) -> anyhow::Result<Value> {
    dispatch(
        store,
        "mail.send",
        json!({"id":id,"from":"captain","to":"apps","kind":"reply",
            "correlation":correlation,"body":"Answered","payload":{"answer":answer}}),
    )
}

fn two_options() -> Value {
    json!([
        {"id":"ship","label":"Ship it","what_happens":"The release goes out today."},
        {"id":"wait","label":"Wait","what_happens":"Nothing ships until review.","cost":"A day","recommended":true}
    ])
}

fn decision_with(id: &str, decision: Value) -> Value {
    json!({"id":id,"from":"apps","to":"captain","kind":"decision","body":"Which path?",
        "payload":{"decision":decision}})
}

fn valid_decision() -> Value {
    json!({"question":"Which path?","options":two_options()})
}

#[test]
fn a_typed_decision_round_trips_and_its_labels_fill_the_plain_choices() {
    let (_temp, mut s) = fleet();
    let sent = send(&mut s, decision_with("typed", valid_decision())).unwrap();
    assert_eq!(sent["payload"]["choices"], json!(["Ship it", "Wait"]));
    let shown = dispatch(&mut s, "mail.show", json!({"id":"typed"})).unwrap();
    assert_eq!(shown["payload"]["decision"], valid_decision());
    assert_eq!(shown["payload"]["choices"], json!(["Ship it", "Wait"]));
    assert_eq!(shown["state"], "open");
    let needs = dispatch(&mut s, "runtime.needs", json!({})).unwrap();
    let item = needs["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["source_id"] == "typed")
        .expect("the open decision is an item");
    assert_eq!(item["decision"], valid_decision());
    assert_eq!(item["choices"], json!(["Ship it", "Wait"]));
}

#[test]
fn explicit_choices_stay_as_given_beside_a_typed_decision() {
    let (_temp, mut s) = fleet();
    let sent = send(
        &mut s,
        json!({"id":"explicit","from":"apps","to":"captain","kind":"decision","body":"Which?",
            "payload":{"choices":["Ship","Hold"],"decision":valid_decision()}}),
    )
    .unwrap();
    assert_eq!(sent["payload"]["choices"], json!(["Ship", "Hold"]));
}

/// The valid decision with one field edited.
fn with(edit: impl FnOnce(&mut Value)) -> Value {
    let mut decision = valid_decision();
    edit(&mut decision);
    decision
}

#[test]
fn each_malformed_decision_field_is_refused_by_name_and_never_echoed() {
    let (_temp, mut s) = fleet();
    let marker = "SECRETVALUE";
    let cases: Vec<(Value, &str)> = vec![
        (
            with(|d| d["question"] = json!("")),
            "decision.question must be 1 to 200",
        ),
        (
            with(|d| d["question"] = json!("q".repeat(201))),
            "decision.question must be 1 to 200",
        ),
        (
            with(|d| d["question"] = json!(5)),
            "decision.question must be a string",
        ),
        (
            with(|d| {
                d.as_object_mut().unwrap().remove("question");
            }),
            "decision.question is required",
        ),
        (
            with(|d| d["why"] = json!("w".repeat(501))),
            "decision.why must be 0 to 500",
        ),
        (
            with(|d| d["blocked"] = json!(vec!["b"; 11])),
            "decision.blocked takes at most 10",
        ),
        (
            with(|d| d["blocked"] = json!(["b".repeat(201)])),
            "decision.blocked items must be 0 to 200",
        ),
        (
            with(|d| d["free_text_ok"] = json!("yes")),
            "decision.free_text_ok must be true or false",
        ),
        (
            with(|d| d["group"] = json!("bad group")),
            "decision.group takes letters",
        ),
        (
            with(|d| d["group"] = json!("g".repeat(65))),
            "decision.group must be 1 to 64",
        ),
        (
            with(|d| d["extra"] = json!(marker)),
            "decision takes only question",
        ),
        (
            with(|d| d["options"] = json!([two_options()[0]])),
            "decision.options takes 2 to 8",
        ),
        (
            with(|d| d["options"] = json!(vec![two_options()[0].clone(); 9])),
            "decision.options takes 2 to 8",
        ),
        (
            with(|d| d["options"][1]["id"] = json!("ship")),
            "decision.options ids must be unique",
        ),
        (
            with(|d| d["options"][1]["id"] = json!(format!("{marker} x"))),
            "decision.options id takes letters",
        ),
        (
            with(|d| d["options"][1]["id"] = json!("i".repeat(33))),
            "decision.options id must be 1 to 32",
        ),
        (
            with(|d| d["options"][1]["label"] = json!("")),
            "decision.options label must be 1 to 80",
        ),
        (
            with(|d| d["options"][1]["label"] = json!("l".repeat(81))),
            "decision.options label must be 1 to 80",
        ),
        (
            with(|d| d["options"][1]["what_happens"] = json!("h".repeat(401))),
            "decision.options what_happens must be 1 to 400",
        ),
        (
            with(|d| d["options"][1]["cost"] = json!("c".repeat(81))),
            "decision.options cost must be 0 to 80",
        ),
        (
            with(|d| d["options"][1]["recommended"] = json!("yes")),
            "decision.options recommended must be true or false",
        ),
        (
            with(|d| d["options"][0]["recommended"] = json!(true)),
            "decision.options takes at most one recommended",
        ),
        (
            with(|d| d["options"][1]["colour"] = json!(marker)),
            "decision.options items take only id",
        ),
        (
            with(|d| d["options"] = json!("ship")),
            "decision.options must be a list",
        ),
    ];
    for (n, (decision, expected)) in cases.into_iter().enumerate() {
        let message = err(send(&mut s, decision_with(&format!("bad-{n}"), decision)));
        assert!(message.contains(expected), "case {n}: {message}");
        assert!(
            !message.contains(marker),
            "case {n} echoed a value: {message}"
        );
    }
    // Other mail keeps a payload key named decision as it was: only a
    // decision message carries a typed decision.
    let plain = send(
        &mut s,
        json!({"id":"not-a-decision","from":"apps","to":"captain","kind":"request",
        "body":"Which?","payload":{"decision":"repair-plan"}}),
    )
    .unwrap();
    assert_eq!(
        plain["payload"],
        json!({"decision":"repair-plan"}),
        "{plain}"
    );
    let notice = send(
        &mut s,
        json!({"id":"fyi","from":"apps","to":"infra","kind":"notice","body":"done",
        "payload":{"decision":"repair-plan","tested_head":"abc"}}),
    )
    .unwrap();
    assert_eq!(notice["payload"]["decision"], "repair-plan", "{notice}");
}

#[test]
fn a_captain_answers_by_option_text_or_both_and_a_bad_answer_leaves_the_decision_open() {
    let (_temp, mut s) = fleet();
    send(&mut s, decision_with("choose", valid_decision())).unwrap();
    let refused = err(answer(
        &mut s,
        "unknown",
        "choose",
        json!({"option_id":"nope"}),
    ));
    assert!(
        refused.contains("answer.option_id must be one of the decision's option ids"),
        "{refused}"
    );
    assert!(!refused.contains("nope"), "{refused}");
    for bad in [
        json!({}),
        json!({"option_id":5}),
        json!({"option_id":"ship","extra":1}),
        json!({"text":""}),
        json!({"text":"t".repeat(4097)}),
        json!("ship"),
    ] {
        assert!(answer(&mut s, "bad", "choose", bad).is_err());
    }
    assert_eq!(
        dispatch(&mut s, "mail.show", json!({"id":"choose"})).unwrap()["state"],
        "open",
        "a refused answer closes nothing"
    );
    assert!(answer(&mut s, "ok", "choose", json!({"option_id":"ship"})).is_ok());
    assert_eq!(
        dispatch(&mut s, "mail.show", json!({"id":"choose"})).unwrap()["state"],
        "answered"
    );

    send(&mut s, decision_with("free", valid_decision())).unwrap();
    let text_only = dispatch(
        &mut s,
        "mail.send",
        json!({"from":"captain","to":"apps","kind":"reply","correlation":"free",
            "body":"Your call, ship the smaller part.","payload":{"answer":{"text":"Ship the smaller part."}}}),
    );
    assert!(text_only.is_ok(), "{text_only:?}");
    send(&mut s, decision_with("both", valid_decision())).unwrap();
    let both = dispatch(
        &mut s,
        "mail.send",
        json!({"from":"captain","to":"apps","kind":"reply","correlation":"both",
            "body":"Wait, with a note.","payload":{"answer":{"option_id":"wait","text":"Until Monday."}}}),
    );
    assert!(both.is_ok(), "{both:?}");
    for id in ["free", "both"] {
        assert_eq!(
            dispatch(&mut s, "mail.show", json!({"id":id})).unwrap()["state"],
            "answered",
            "{id} is closed by its answer"
        );
    }
}

#[test]
fn an_answer_is_checked_only_against_a_typed_decision() {
    let (_temp, mut s) = fleet();
    send(
        &mut s,
        json!({"id":"plain","from":"apps","to":"captain","kind":"decision","body":"Ship?"}),
    )
    .unwrap();
    // A plain decision has no options to check against: the answer rides
    // along and the reply closes it as before.
    dispatch(
        &mut s,
        "mail.send",
        json!({"from":"captain","to":"apps","kind":"reply","correlation":"plain",
            "body":"Yes","payload":{"answer":{"text":"Yes"}}}),
    )
    .unwrap();
    assert_eq!(
        dispatch(&mut s, "mail.show", json!({"id":"plain"})).unwrap()["state"],
        "answered"
    );
    // Other mail keeps a payload key named answer as it was.
    send(
        &mut s,
        json!({"from":"apps","to":"infra","kind":"notice","body":"fyi","payload":{"answer":{"text":"x"}}}),
    )
    .unwrap();
}

#[test]
fn a_choice_is_bounded_in_characters_so_a_full_label_is_not_refused() {
    let (_temp, mut s) = fleet();
    // Eighty characters of CJK text: 240 bytes, which a byte count would refuse.
    let label = "漢".repeat(80);
    let plain = send(
        &mut s,
        json!({"id":"cjk-plain","from":"apps","to":"captain","kind":"decision","body":"Which?",
            "payload":{"choices":[label.clone(), "Hold"]}}),
    );
    assert!(plain.is_ok(), "{plain:?}");
    let typed = send(
        &mut s,
        decision_with(
            "cjk-typed",
            json!({"question":"Which?","options":[
                {"id":"long","label":label,"what_happens":"Stays."},
                {"id":"short","label":"Hold","what_happens":"Stays."}]}),
        ),
    );
    assert!(typed.is_ok(), "{typed:?}");
    let too_long = err(send(
        &mut s,
        json!({"id":"too-long","from":"apps","to":"captain","kind":"decision","body":"Which?",
            "payload":{"choices":["x".repeat(201)]}}),
    ));
    assert!(
        too_long.contains("choices must contain at most 20 bounded strings"),
        "{too_long}"
    );
}

/// A home whose recipient's directory row was pruned, as a directory
/// exchange does when the ship retires on another machine. The peer cannot
/// be reached, so the mail is still pending when the row disappears.
fn pruned_recipient() -> (tempfile::TempDir, Store) {
    let (temp, mut s) = fleet();
    dispatch(
        &mut s,
        "peer.put",
        json!({"name":"machine-far","transport":"local",
            "executable":"/nonexistent/privateer","home":"/nonexistent/far-home"}),
    )
    .unwrap();
    for name in ["beta", "gamma"] {
        dispatch(
            &mut s,
            "ship.put",
            json!({"name":name,"scope":"far side","location":{"peer":"machine-far"}}),
        )
        .unwrap();
    }
    (temp, s)
}

#[test]
fn a_missing_recipient_row_stays_pending_and_recovers_when_relearned() {
    let (_temp, mut s) = pruned_recipient();
    let sent = send(
        &mut s,
        json!({"id":"survey","from":"apps","to":"beta","kind":"request",
            "body":"Send the survey results"}),
    )
    .unwrap();
    assert_eq!(sent["delivery"], "pending", "{sent}");
    assert_eq!(
        dispatch(&mut s, "status", json!({})).unwrap()["undelivered_mail"],
        1
    );
    // The directory exchange pruned the recipient's row while the request
    // was still waiting for the machine to take it.
    s.connection()
        .execute("DELETE FROM ships WHERE name='beta'", [])
        .unwrap();

    let report = dispatch(&mut s, "mail.flush", json!({})).unwrap();
    assert_eq!(report["attempted"], 0, "{report}");
    assert_eq!(report["pending"], 1, "{report}");
    assert_eq!(report["undeliverable"], 0, "{report}");

    // Absence is reversible: a flush preserves the message and open request.
    let waiting = dispatch(&mut s, "mail.show", json!({"id":"survey"})).unwrap();
    assert_eq!(waiting["delivery"], "pending", "{waiting}");
    assert_eq!(
        waiting["delivery_error"], sent["delivery_error"],
        "{waiting}"
    );
    let outbox = dispatch(
        &mut s,
        "mail.outbox",
        json!({"from":"apps","delivery":"undeliverable"}),
    )
    .unwrap();
    assert_eq!(outbox["messages"].as_array().unwrap().len(), 0, "{outbox}");
    assert_eq!(
        dispatch(&mut s, "status", json!({})).unwrap()["undeliverable_mail"],
        0
    );
    let bearings = dispatch(&mut s, "ship.bearings", json!({"ship":"apps"})).unwrap();
    assert_eq!(bearings["refused_mail"].as_array().unwrap().len(), 0);
    assert_eq!(waiting["state"], "open", "{waiting}");
    let open = dispatch(
        &mut s,
        "mail.requests",
        json!({"address":"apps","direction":"outgoing","state":"open"}),
    )
    .unwrap();
    assert_eq!(open["requests"].as_array().unwrap().len(), 1, "{open}");
    assert_eq!(waiting["ended_by"], Value::Null);

    // Flushing again preserves it, and mail to a live local recipient still flows.
    let again = dispatch(&mut s, "mail.flush", json!({})).unwrap();
    assert_eq!(again["pending"], 1, "{again}");
    assert_eq!(again["undeliverable"], 0, "{again}");
    send(
        &mut s,
        json!({"id":"local","from":"apps","to":"infra","kind":"request","body":"Deploy today"}),
    )
    .unwrap();
    assert_eq!(
        dispatch(&mut s, "mail.show", json!({"id":"local"})).unwrap()["state"],
        "open"
    );
    assert_eq!(
        dispatch(&mut s, "mail.flush", json!({})).unwrap()["pending"],
        1
    );
    assert!(dispatch(&mut s, "mail.outbox", json!({"delivery":"banana"})).is_err());

    // Re-learning the address (for example after re-link) recovers the mail.
    dispatch(
        &mut s,
        "ship.put",
        json!({"name":"beta","scope":"far side, again","location":{"peer":"machine-far"}}),
    )
    .unwrap();
    dispatch(&mut s, "mail.flush", json!({})).unwrap();
    let still = dispatch(&mut s, "mail.show", json!({"id":"survey"})).unwrap();
    assert_eq!(still["delivery"], "pending", "{still}");
    assert_eq!(still["delivered_ms"], Value::Null, "{still}");
    assert_eq!(still["state"], "open", "{still}");
    assert_eq!(
        dispatch(&mut s, "mail.flush", json!({})).unwrap()["attempted"],
        1,
        "the re-learned address is eligible for delivery again"
    );
    // New mail to the re-created ship is ordinary pending mail again.
    send(
        &mut s,
        json!({"id":"fresh","from":"apps","to":"beta","kind":"notice","body":"afresh"}),
    )
    .unwrap();
    assert_eq!(
        dispatch(&mut s, "mail.show", json!({"id":"fresh"})).unwrap()["delivery"],
        "pending"
    );
}

#[test]
fn mail_to_a_live_remote_recipient_stays_pending_when_transport_is_unreachable() {
    let (_temp, mut s) = pruned_recipient();
    send(
        &mut s,
        json!({"id":"held","from":"apps","to":"beta","kind":"notice","body":"still waiting"}),
    )
    .unwrap();
    send(
        &mut s,
        json!({"id":"also-held","from":"apps","to":"gamma","kind":"notice","body":"same"}),
    )
    .unwrap();
    // Neither directory row is pruned, so a flush attempts delivery rather
    // than refusing: the peer is unreachable, and the mail stays pending.
    let report = dispatch(&mut s, "mail.flush", json!({})).unwrap();
    assert_eq!(report["undeliverable"], 0, "{report}");
    assert_eq!(report["pending"], 2, "{report}");
    for id in ["held", "also-held"] {
        assert_eq!(
            dispatch(&mut s, "mail.show", json!({"id":id})).unwrap()["delivery"],
            "pending"
        );
    }
}
