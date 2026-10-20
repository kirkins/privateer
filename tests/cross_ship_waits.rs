//! Cross-ship waits, owner side: `shared_with`, subscription rows, and the
//! runtime notices (`wait_subscribe`, `wait_unsubscribe`, `task_end`,
//! `wait_closed`) that never land in any inbox. The waiting side (the
//! `remote` wait and resume) is a later step of the design and is not here:
//! a `task_end` that reaches the waiting home is, for now, ordinary mail.
use privateer::{dispatch, store::Store};
use serde_json::{Value, json};
use std::fs;

/// Two homes on one machine: `apps` in `a` owns the waited-on task, `infra`
/// in `b` is the subscribing ship. Each home reaches the other as a peer.
struct Pair {
    _temp: tempfile::TempDir,
    a: Store,
    b: Store,
}

fn pair() -> Pair {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let repo = root.join("repo");
    fs::create_dir(&repo).unwrap();
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
    for (name, location) in [
        ("apps", json!("here")),
        ("infra", json!({"peer":"home-b"})),
        ("research", json!("here")),
    ] {
        dispatch(
            &mut a,
            "ship.put",
            json!({"name":name,"scope":"work","location":location}),
        )
        .unwrap();
    }
    for (name, location) in [("infra", json!("here")), ("apps", json!({"peer":"home-a"}))] {
        dispatch(
            &mut b,
            "ship.put",
            json!({"name":name,"scope":"work","location":location}),
        )
        .unwrap();
    }
    dispatch(
        &mut a,
        "charter.put",
        json!({"id":"project","objective":"Deliver","repo":repo,"min_free_bytes":0}),
    )
    .unwrap();
    for (id, owner, title) in [
        ("far-work", "apps", "The dependency"),
        ("other-work", "research", "Another ship's task"),
    ] {
        dispatch(
            &mut a,
            "task.create",
            json!({"id":id,"charter_id":"project","owner":owner,"title":title}),
        )
        .unwrap();
    }
    Pair { _temp: temp, a, b }
}

/// Deliver a runtime wait notice into the owner's home, as its peer would.
fn deliver_wait(store: &mut Store, id: &str, key: &str, task: &str) -> Value {
    dispatch(
        store,
        "mail.deliver",
        json!({"id":id,"from":"infra","to":"apps","kind":"notice","payload":{
            key: {"task":task,"subscriber":"infra","subscriber_task":"waiting-1"}}}),
    )
    .unwrap()
}

fn rows(store: &Store, task: &str) -> i64 {
    store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM task_subscriptions WHERE task_id=?1",
            [task],
            |r| r.get(0),
        )
        .unwrap()
}

/// The notices the owner's home queued for the subscriber, delivered over the
/// link and read from the waiting home's inbox.
fn notices_to_subscriber(pair: &mut Pair) -> Vec<Value> {
    dispatch(&mut pair.a, "mail.flush", json!({})).unwrap();
    dispatch(&mut pair.b, "mail.inbox", json!({"address":"infra"})).unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone()
}

fn share(store: &mut Store, task: &str, ships: Value) {
    dispatch(
        store,
        "task.update",
        json!({"id":task,"actor":"apps","shared_with":ships}),
    )
    .unwrap();
}

#[test]
fn shared_with_is_set_by_the_owner_validated_and_audited() {
    let Pair { _temp, mut a, .. } = pair();
    // An existing task shares with nobody until its owner says so.
    assert_eq!(
        dispatch(&mut a, "task.show", json!({"id":"far-work"})).unwrap()["shared_with"],
        json!([])
    );
    share(&mut a, "far-work", json!(["infra", "infra", "research"]));
    assert_eq!(
        dispatch(&mut a, "task.show", json!({"id":"far-work"})).unwrap()["shared_with"],
        json!(["infra", "research"])
    );
    // Clearing the list is the same operation with a null (or empty) list.
    share(&mut a, "far-work", Value::Null);
    assert_eq!(
        dispatch(&mut a, "task.show", json!({"id":"far-work"})).unwrap()["shared_with"],
        json!([])
    );
    // Ship names follow the rule ship.put enforces, and the list stays a list.
    for bad in [
        json!("infra"),
        json!(["infra", 3]),
        json!(["bad name"]),
        json!(["captain"]),
        json!(["crew"]),
        json!([Value::Null]),
    ] {
        assert!(
            dispatch(
                &mut a,
                "task.update",
                json!({"id":"far-work","actor":"apps","shared_with":bad}),
            )
            .is_err(),
            "accepted {bad}"
        );
    }
    assert!(
        dispatch(
            &mut a,
            "task.update",
            json!({"id":"far-work","actor":"apps","shared_with":vec![json!("ship"); 101]}),
        )
        .is_err()
    );
    // Only the owner shares its task, and a task.update still needs a status
    // or a shared_with.
    assert!(
        dispatch(
            &mut a,
            "task.update",
            json!({"id":"far-work","actor":"research","shared_with":["research"]}),
        )
        .is_err()
    );
    assert!(
        dispatch(
            &mut a,
            "task.update",
            json!({"id":"far-work","actor":"apps"}),
        )
        .is_err()
    );
    // A terminal task accepts no update at all, including a shared_with one.
    dispatch(
        &mut a,
        "task.update",
        json!({"id":"far-work","actor":"apps","status":"done"}),
    )
    .unwrap();
    assert!(
        dispatch(
            &mut a,
            "task.update",
            json!({"id":"far-work","actor":"apps","shared_with":["infra"]}),
        )
        .is_err()
    );
    // Every shared_with change is audited.
    let audit = dispatch(&mut a, "audit.list", json!({})).unwrap();
    let shares: Vec<&Value> = audit["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "task.share")
        .collect();
    assert_eq!(shares.len(), 2);
    assert_eq!(
        shares[0]["payload"]["shared_with"],
        json!(["infra", "research"])
    );
    assert_eq!(shares[1]["payload"]["shared_with"], json!([]));
}

#[test]
fn subscribe_stores_one_row_and_a_duplicate_adds_nothing() {
    let mut pair = pair();
    share(&mut pair.a, "far-work", json!(["infra"]));
    let first = deliver_wait(&mut pair.a, "sub-1", "wait_subscribe", "far-work");
    assert_eq!(first["wait"]["state"], "subscribed");
    assert_eq!(first["wait"]["duplicate"], false);
    assert_eq!(rows(&pair.a, "far-work"), 1);
    let row = pair
        .a
        .connection()
        .query_row(
            "SELECT subscriber,subscriber_task FROM task_subscriptions WHERE task_id='far-work'",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .unwrap();
    assert_eq!(row, ("infra".to_owned(), "waiting-1".to_owned()));
    // A repeated subscribe is idempotent and adds nothing.
    let again = deliver_wait(&mut pair.a, "sub-2", "wait_subscribe", "far-work");
    assert_eq!(again["wait"]["state"], "subscribed");
    assert_eq!(again["wait"]["duplicate"], true);
    assert_eq!(rows(&pair.a, "far-work"), 1);
    // A different waiting task of the same ship is its own subscription.
    dispatch(
        &mut pair.a,
        "mail.deliver",
        json!({"id":"sub-3","from":"infra","to":"apps","kind":"notice","payload":{
            "wait_subscribe": {"task":"far-work","subscriber":"infra","subscriber_task":"waiting-2"}}}),
    )
    .unwrap();
    assert_eq!(rows(&pair.a, "far-work"), 2);
    // None of it is mail: the owner's inbox and notifications stay empty.
    let inbox = dispatch(&mut pair.a, "mail.inbox", json!({"address":"apps"})).unwrap();
    assert_eq!(inbox["messages"].as_array().unwrap().len(), 0);
    let pending = dispatch(&mut pair.a, "mail.pending", json!({"address":"apps"})).unwrap();
    assert_eq!(pending["count"], 0);
    for id in ["sub-1", "sub-2", "sub-3"] {
        assert!(dispatch(&mut pair.a, "mail.show", json!({"id":id})).is_err());
    }
    // The audit records the subscription.
    let audit = dispatch(&mut pair.a, "audit.list", json!({})).unwrap();
    assert!(
        audit["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "wait.subscribe"
                && event["payload"]["task"] == "far-work"
                && event["payload"]["subscriber"] == "infra")
    );
}

#[test]
fn unshared_and_missing_tasks_get_the_same_refusal() {
    let mut pair = pair();
    // Three refusals that must be indistinguishable: a task that exists but
    // is shared with nobody, a task this home does not hold, and a task that
    // belongs to another ship than the one the notice addressed.
    for (id, task) in [
        ("ref-1", "far-work"),
        ("ref-2", "no-such-task"),
        ("ref-3", "other-work"),
    ] {
        let refused = deliver_wait(&mut pair.a, id, "wait_subscribe", task);
        assert_eq!(refused["wait"]["state"], "refused", "{task}");
        assert_eq!(rows(&pair.a, task), 0);
    }
    // A sender that is not the subscriber it names is refused the same way.
    let forged = dispatch(
        &mut pair.a,
        "mail.deliver",
        json!({"id":"ref-4","from":"infra","to":"apps","kind":"notice","payload":{
            "wait_subscribe": {"task":"far-work","subscriber":"research","subscriber_task":"waiting-1"}}}),
    )
    .unwrap();
    assert_eq!(forged["wait"]["state"], "refused");
    let notices = notices_to_subscriber(&mut pair);
    let refusals: Vec<&Value> = notices
        .iter()
        .filter(|m| m["payload"]["wait_closed"].is_object())
        .collect();
    assert_eq!(refusals.len(), 4, "{notices:?}");
    for notice in &refusals {
        assert_eq!(notice["from"], "apps");
        assert_eq!(notice["to"], "infra");
        assert_eq!(notice["kind"], "notice");
    }
    let payloads: Vec<Value> = refusals
        .iter()
        .map(|m| m["payload"]["wait_closed"].clone())
        .collect();
    assert!(
        payloads.contains(&json!({"task":"far-work","reason":"refused"})),
        "{payloads:?}"
    );
    assert!(payloads.contains(&json!({"task":"no-such-task","reason":"refused"})));
    // The payload holds task and reason only: nothing else about the task.
    for payload in &payloads {
        assert_eq!(
            payload.as_object().unwrap().len(),
            2,
            "refusal reveals more than task and reason: {payload}"
        );
    }
    // None of it reached the owner's inbox.
    let inbox = dispatch(&mut pair.a, "mail.inbox", json!({"address":"apps"})).unwrap();
    assert_eq!(inbox["messages"].as_array().unwrap().len(), 0);
}

#[test]
fn a_subscribe_for_an_ended_task_answers_with_its_final_status() {
    let mut pair = pair();
    dispatch(
        &mut pair.a,
        "task.update",
        json!({"id":"far-work","actor":"apps","shared_with":["infra"],"status":"done"}),
    )
    .unwrap();
    let answered = deliver_wait(&mut pair.a, "late-1", "wait_subscribe", "far-work");
    assert_eq!(answered["wait"]["state"], "ended");
    assert_eq!(rows(&pair.a, "far-work"), 0);
    let notices = notices_to_subscriber(&mut pair);
    let ends: Vec<&Value> = notices
        .iter()
        .filter(|m| m["payload"]["task_end"].is_object())
        .collect();
    assert_eq!(ends.len(), 1, "{notices:?}");
    assert_eq!(ends[0]["from"], "apps");
    assert_eq!(
        ends[0]["payload"]["task_end"],
        json!({"task":"far-work","status":"done","reason":"done"})
    );
    // The owner's inbox stays empty; the answer went to the subscriber.
    let inbox = dispatch(&mut pair.a, "mail.inbox", json!({"address":"apps"})).unwrap();
    assert_eq!(inbox["messages"].as_array().unwrap().len(), 0);
}

#[test]
fn a_final_status_ends_each_subscription_once_and_deletes_the_rows() {
    for status in ["done", "cancelled"] {
        let mut pair = pair();
        share(&mut pair.a, "far-work", json!(["infra", "research"]));
        deliver_wait(&mut pair.a, "sub-1", "wait_subscribe", "far-work");
        assert_eq!(rows(&pair.a, "far-work"), 1);
        dispatch(
            &mut pair.a,
            "task.update",
            json!({"id":"far-work","actor":"apps","status":status}),
        )
        .unwrap();
        // The rows are gone in the same transaction that ended the task.
        assert_eq!(rows(&pair.a, "far-work"), 0);
        let notices = notices_to_subscriber(&mut pair);
        let ends: Vec<&Value> = notices
            .iter()
            .filter(|m| m["payload"]["task_end"]["task"] == "far-work")
            .collect();
        assert_eq!(ends.len(), 1, "{status}: {notices:?}");
        assert_eq!(ends[0]["payload"]["task_end"]["status"], json!(status));
        assert_eq!(ends[0]["payload"]["task_end"]["reason"], json!(status));
        assert_eq!(ends[0]["payload"]["task_end"].as_object().unwrap().len(), 3);
        // No second notice follows: the rows no longer exist.
        dispatch(&mut pair.a, "mail.flush", json!({})).unwrap();
        let inbox = dispatch(&mut pair.b, "mail.inbox", json!({"address":"infra"})).unwrap();
        assert_eq!(
            inbox["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["payload"]["task_end"].is_object())
                .count(),
            1
        );
        // A subscriber that arrives after the end is answered, not stored.
        let late = deliver_wait(&mut pair.a, "late-1", "wait_subscribe", "far-work");
        assert_eq!(late["wait"]["state"], "ended");
        assert_eq!(rows(&pair.a, "far-work"), 0);
    }
}

#[test]
fn an_unsubscribe_removes_the_row_and_is_silent_without_one() {
    let mut pair = pair();
    share(&mut pair.a, "far-work", json!(["infra"]));
    deliver_wait(&mut pair.a, "sub-1", "wait_subscribe", "far-work");
    assert_eq!(rows(&pair.a, "far-work"), 1);
    let removed = deliver_wait(&mut pair.a, "unsub-1", "wait_unsubscribe", "far-work");
    assert_eq!(removed["wait"]["state"], "unsubscribed");
    assert_eq!(rows(&pair.a, "far-work"), 0);
    // Unsubscribing again, or for a task never subscribed, stays silent.
    for id in ["unsub-2", "unsub-3"] {
        let silent = deliver_wait(&mut pair.a, id, "wait_unsubscribe", "far-work");
        assert_eq!(silent["wait"]["state"], "unsubscribed");
    }
    // An unsubscribe naming a subscriber other than its sender touches
    // nothing, and never answers.
    deliver_wait(&mut pair.a, "sub-2", "wait_subscribe", "far-work");
    for id in ["unsub-4", "unsub-5"] {
        dispatch(
            &mut pair.a,
            "mail.deliver",
            json!({"id":id,"from":"infra","to":"apps","kind":"notice","payload":{
                "wait_unsubscribe": {"task":"far-work","subscriber":"research","subscriber_task":"waiting-1"}}}),
        )
        .unwrap();
    }
    assert_eq!(rows(&pair.a, "far-work"), 1);
    let notices = notices_to_subscriber(&mut pair);
    assert!(notices.is_empty(), "{notices:?}");
    let inbox = dispatch(&mut pair.a, "mail.inbox", json!({"address":"apps"})).unwrap();
    assert_eq!(inbox["messages"].as_array().unwrap().len(), 0);
}

#[test]
fn wait_payload_keys_are_reserved_for_the_runtime() {
    let Pair { _temp, mut a, .. } = pair();
    for key in [
        "wait_subscribe",
        "wait_unsubscribe",
        "task_end",
        "wait_closed",
        "request_end",
    ] {
        let refused = dispatch(
            &mut a,
            "mail.send",
            json!({"id":format!("spoof-{key}"),"from":"apps","to":"infra","kind":"notice",
                "body":"forged","payload":{key:{"task":"far-work","subscriber":"infra",
                "subscriber_task":"waiting-1","status":"done","reason":"forged"}}}),
        );
        assert!(refused.is_err(), "mail.send accepted payload.{key}");
        assert!(dispatch(&mut a, "mail.show", json!({"id":format!("spoof-{key}")})).is_err());
    }
    // A wait notice that is not a notice, carries both instructions, or is
    // malformed is refused on the wire too, and stores no row.
    share(&mut a, "far-work", json!(["infra"]));
    for bad in [
        json!({"id":"bad-kind","from":"infra","to":"apps","kind":"request","payload":{
            "wait_subscribe": {"task":"far-work","subscriber":"infra","subscriber_task":"w"}}}),
        json!({"id":"both","from":"infra","to":"apps","kind":"notice","payload":{
            "wait_subscribe": {"task":"far-work","subscriber":"infra","subscriber_task":"w"},
            "wait_unsubscribe": {"task":"far-work","subscriber":"infra","subscriber_task":"w"}}}),
        json!({"id":"malformed","from":"infra","to":"apps","kind":"notice","payload":{
            "wait_subscribe": {"task":"far-work"}}}),
    ] {
        assert!(
            dispatch(&mut a, "mail.deliver", bad.clone()).is_err(),
            "{bad}"
        );
    }
    assert_eq!(rows(&a, "far-work"), 0);
}
