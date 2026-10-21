//! Cross-ship waits: `shared_with`, subscription rows, and the runtime
//! notices (`wait_subscribe`, `wait_unsubscribe`, `task_end`, `wait_closed`)
//! that never land in any inbox. The owner side answers subscriptions; the
//! waiting side defers on a `remote` wait, verifies each report, and resumes
//! or faults the waiting task.
use privateer::{dispatch, store::Store};
use serde_json::{Value, json};
use std::fs;

mod support;

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
    // The waiting home schedules its own work, so its ship has a first mate
    // profile, and it knows a second machine's ship for the routing checks.
    let manifest = root.join("manifest.json");
    fs::write(&manifest, "{}").unwrap();
    for store in [&mut a, &mut b] {
        dispatch(
            store,
            "charter.put",
            json!({"id":"project","objective":"Deliver","repo":repo,"min_free_bytes":0}),
        )
        .unwrap();
    }
    dispatch(
        &mut b,
        "task.create",
        json!({"id":"infra-duty","charter_id":"project","owner":"infra","title":"Duty"}),
    )
    .unwrap();
    dispatch(
        &mut b,
        "peer.put",
        json!({"name":"machine-studio","transport":"collected"}),
    )
    .unwrap();
    for (name, location) in [
        ("apps", json!({"peer":"home-a"})),
        ("research", json!({"peer":"home-a"})),
        ("storefront", json!({"peer":"machine-studio"})),
    ] {
        dispatch(
            &mut b,
            "ship.put",
            json!({"name":name,"scope":"work","location":location}),
        )
        .unwrap();
    }
    dispatch(
        &mut b,
        "ship.put",
        json!({"name":"infra","scope":"work","location":"here",
            "first_mate":{"manifest":manifest,"duty_task":"infra-duty"}}),
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

/// The notices the owner's home queued for the subscriber, read from its
/// outbox after a flush has carried them over the link. The waiting home
/// applies them before any inbox, so its inbox never shows them; each must
/// still have been delivered, not refused.
fn notices_to_subscriber(pair: &mut Pair) -> Vec<Value> {
    dispatch(&mut pair.a, "mail.flush", json!({})).unwrap();
    outbox(&pair.a, "infra")
}

/// The mail a home holds for a ship behind a peer, oldest first.
fn outbox(store: &Store, to: &str) -> Vec<Value> {
    let mut statement = store
        .connection()
        .prepare("SELECT sender,recipient,kind,payload,delivery FROM mail WHERE recipient=?1 ORDER BY seq")
        .unwrap();
    statement
        .query_map([to], |r| {
            Ok(
                json!({"from": r.get::<_, String>(0)?, "to": r.get::<_, String>(1)?,
                "kind": r.get::<_, String>(2)?,
                "payload": serde_json::from_str::<Value>(&r.get::<_, String>(3)?).unwrap(),
                "delivery": r.get::<_, Option<String>>(4)?}),
            )
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
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
fn a_final_status_sends_one_task_end_per_subscriber_and_deletes_the_rows() {
    for status in ["done", "cancelled"] {
        let mut pair = pair();
        share(&mut pair.a, "far-work", json!(["infra", "research"]));
        deliver_wait(&mut pair.a, "sub-1", "wait_subscribe", "far-work");
        // A second waiting task of the same ship is its own row, but the
        // notice goes once per subscriber ship, since it names no waiting task.
        dispatch(
            &mut pair.a,
            "mail.deliver",
            json!({"id":"sub-2","from":"infra","to":"apps","kind":"notice","payload":{
                "wait_subscribe": {"task":"far-work","subscriber":"infra","subscriber_task":"waiting-2"}}}),
        )
        .unwrap();
        assert_eq!(rows(&pair.a, "far-work"), 2);
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
        assert_eq!(
            notices_to_subscriber(&mut pair)
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

// The waiting side: `infra` in home `b` defers its own scheduled task on a
// `remote` wait naming `apps`'s task in home `a`.

/// Create and enqueue a task of `infra` in the waiting home.
fn waiter(store: &mut Store, id: &str) {
    dispatch(
        store,
        "task.create",
        json!({"id":id,"charter_id":"project","owner":"infra","title":"Waits on another ship"}),
    )
    .unwrap();
    dispatch(
        store,
        "task.enqueue",
        json!({"id":id,"brief":"Continue once the dependency is done."}),
    )
    .unwrap();
}

fn defer_remote(store: &mut Store, id: &str, reference: Value) -> anyhow::Result<Value> {
    dispatch(
        store,
        "task.defer",
        json!({"id":id,"wait":{"kind":"remote","reason":"needs the far work","ref":reference}}),
    )
}

fn far() -> Value {
    json!({"ship":"apps","task":"far-work"})
}

/// One task's schedule in the waiting home.
fn work(store: &mut Store, id: &str) -> Value {
    dispatch(store, "work.status", json!({"ship":"infra"})).unwrap()["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["task_id"] == id)
        .cloned()
        .unwrap_or(Value::Null)
}

/// A scheduler pass for the waiting ship, as its attended session polls
/// while idle: every live schedule is checked, and ready work is offered.
fn pass(store: &mut Store) -> Value {
    dispatch(
        store,
        "ship.attend",
        json!({"ship":"infra","holder":"host:wait-test","ttl_secs":90}),
    )
    .unwrap();
    dispatch(
        store,
        "work.poll",
        json!({"ship":"infra","holder":"host:wait-test","idle":true,
            "attention":{"tokens":1000,"window":100000}}),
    )
    .unwrap()
}

/// Carry the waiting home's subscriptions to the owner, then the owner's
/// answers back.
fn exchange(pair: &mut Pair) {
    dispatch(&mut pair.b, "mail.flush", json!({})).unwrap();
    dispatch(&mut pair.a, "mail.flush", json!({})).unwrap();
}

/// The wait notices of one kind a home queued for `to`, without the
/// subscription nonce each carries (`nonces` reads those).
fn wait_payloads(store: &Store, to: &str, key: &str) -> Vec<Value> {
    outbox(store, to)
        .into_iter()
        .filter_map(|m| m["payload"].get(key).cloned())
        .map(|mut body| {
            body.as_object_mut().unwrap().remove("nonce");
            body
        })
        .collect()
}

fn nonces(store: &Store, to: &str, key: &str) -> Vec<Value> {
    outbox(store, to)
        .into_iter()
        .filter_map(|m| m["payload"].get(key).map(|body| body["nonce"].clone()))
        .collect()
}

/// The nonce of the subscription a waiting task holds, if any.
fn nonce_of(store: &Store, task: &str) -> Option<String> {
    store
        .connection()
        .query_row(
            "SELECT json_extract(record,'$.subscription.nonce') FROM task_schedule WHERE task_id=?1",
            [task],
            |r| r.get(0),
        )
        .unwrap()
}

fn audited(store: &mut Store, kind: &str) -> Vec<Value> {
    dispatch(store, "audit.list", json!({})).unwrap()["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == kind)
        .map(|event| event["payload"].clone())
        .collect()
}

/// A report delivered straight into the waiting home, as a peer would.
fn report(store: &mut Store, id: &str, from: &str, payload: Value) -> Value {
    dispatch(
        store,
        "mail.deliver",
        json!({"id":id,"from":from,"to":"infra","kind":"notice","payload":payload}),
    )
    .unwrap()
}

/// Nothing a wait notice carries reaches the waiting ship's inbox.
fn no_wait_mail(store: &mut Store) {
    let inbox = dispatch(store, "mail.inbox", json!({"address":"infra"})).unwrap();
    assert!(
        inbox["messages"].as_array().unwrap().iter().all(|m| {
            ["task_end", "wait_closed"]
                .iter()
                .all(|key| m["payload"].get(*key).is_none())
        }),
        "{inbox}"
    );
}

/// A shared far task, a waiting task deferred on it, and the subscription
/// carried to the owner.
fn subscribed() -> Pair {
    let mut pair = pair();
    share(&mut pair.a, "far-work", json!(["infra"]));
    waiter(&mut pair.b, "waiting-1");
    defer_remote(&mut pair.b, "waiting-1", far()).unwrap();
    exchange(&mut pair);
    assert_eq!(rows(&pair.a, "far-work"), 1);
    pair
}

#[test]
fn a_remote_defer_subscribes_and_the_owner_stores_the_row() {
    let mut pair = pair();
    share(&mut pair.a, "far-work", json!(["infra"]));
    waiter(&mut pair.b, "waiting-1");
    let deferred = defer_remote(&mut pair.b, "waiting-1", far()).unwrap();
    // Waiting with no clock: nothing polls the far end.
    assert_eq!(deferred["state"], "waiting");
    assert_eq!(
        deferred["wait"],
        json!({"kind":"remote","reason":"needs the far work","ref":{"ship":"apps","task":"far-work"}})
    );
    assert!(deferred["next_check_ms"].is_null());
    // The subscribe was queued in the defer's own transaction.
    assert_eq!(
        wait_payloads(&pair.b, "apps", "wait_subscribe"),
        vec![json!({"task":"far-work","subscriber":"infra","subscriber_task":"waiting-1"})]
    );
    // It carries the subscription's nonce, which the schedule records.
    let nonce = nonce_of(&pair.b, "waiting-1").unwrap();
    assert_eq!(
        nonces(&pair.b, "apps", "wait_subscribe"),
        vec![json!(nonce)]
    );
    exchange(&mut pair);
    let row = pair
        .a
        .connection()
        .query_row(
            "SELECT subscriber,subscriber_task,nonce FROM task_subscriptions WHERE task_id='far-work'",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
        )
        .unwrap();
    assert_eq!(row, ("infra".to_owned(), "waiting-1".to_owned(), nonce));
    // A dispatcher pass leaves it waiting and sends nothing more.
    pass(&mut pair.b);
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "waiting");
    assert_eq!(wait_payloads(&pair.b, "apps", "wait_subscribe").len(), 1);
}

#[test]
fn a_far_done_resumes_the_waiting_task_with_its_status() {
    let mut pair = subscribed();
    dispatch(
        &mut pair.a,
        "task.update",
        json!({"id":"far-work","actor":"apps","status":"done"}),
    )
    .unwrap();
    dispatch(&mut pair.a, "mail.flush", json!({})).unwrap();
    let resumed = work(&mut pair.b, "waiting-1");
    assert_eq!(resumed["state"], "ready", "{resumed}");
    assert!(resumed["wait"].is_null());
    let reason = resumed["resumed_by"].as_str().unwrap();
    assert!(
        reason.contains("far-work") && reason.contains("apps") && reason.contains("done"),
        "{reason}"
    );
    assert_eq!(resumed["reason"], resumed["resumed_by"]);
    // The offer that follows tells the first mate why it resumed.
    let offered = pass(&mut pair.b);
    assert!(
        offered["offer"]["prompt"]
            .as_str()
            .is_some_and(|prompt| prompt.contains(&format!("Resumed: {reason}"))),
        "{offered}"
    );
    // The report was applied, never mailed, and asks for no unsubscribe.
    no_wait_mail(&mut pair.b);
    assert!(wait_payloads(&pair.b, "apps", "wait_unsubscribe").is_empty());
    assert_eq!(audited(&mut pair.b, "wait.resolved").len(), 1);
}

#[test]
fn a_far_cancel_faults_the_waiting_task_and_surfaces_it() {
    let mut pair = subscribed();
    dispatch(
        &mut pair.a,
        "task.update",
        json!({"id":"far-work","actor":"apps","status":"cancelled"}),
    )
    .unwrap();
    dispatch(&mut pair.a, "mail.flush", json!({})).unwrap();
    let faulted = work(&mut pair.b, "waiting-1");
    assert_eq!(faulted["state"], "fault", "{faulted}");
    assert!(faulted["reason"].as_str().unwrap().contains("cancelled"));
    assert!(faulted["resumed_by"].is_null());
    // A later pass never resumes it as if the dependency succeeded.
    pass(&mut pair.b);
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "fault");
    // The fault is a captain's need, as every scheduling fault is.
    let needs = dispatch(&mut pair.b, "runtime.needs", json!({})).unwrap();
    assert!(
        needs["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item.to_string().contains("waiting-1")
                && item.to_string().contains("cancelled")),
        "{needs}"
    );
    assert!(wait_payloads(&pair.b, "apps", "wait_unsubscribe").is_empty());
}

#[test]
fn an_unshared_far_task_is_refused_and_the_wait_faults() {
    let mut pair = pair();
    waiter(&mut pair.b, "waiting-1");
    defer_remote(&mut pair.b, "waiting-1", far()).unwrap();
    exchange(&mut pair);
    assert_eq!(rows(&pair.a, "far-work"), 0);
    let faulted = work(&mut pair.b, "waiting-1");
    assert_eq!(faulted["state"], "fault");
    assert!(
        faulted["reason"].as_str().unwrap().contains("refused"),
        "{faulted}"
    );
    // wait_closed is answered with nothing: the owner holds no row.
    assert!(wait_payloads(&pair.b, "apps", "wait_unsubscribe").is_empty());
    no_wait_mail(&mut pair.b);
}

#[test]
fn a_forged_end_from_a_third_ship_is_dropped_and_audited() {
    let mut pair = subscribed();
    // `research` is a real ship behind the same peer, but the wait names apps.
    let forged = report(
        &mut pair.b,
        "forged-1",
        "research",
        json!({"task_end":{"task":"far-work","status":"done","reason":"done"}}),
    );
    assert_eq!(forged["wait"]["state"], "dropped");
    // The right sender with a status that is not final is dropped too, and
    // so is a report carried by a machine the ref does not name.
    let open = report(
        &mut pair.b,
        "forged-2",
        "apps",
        json!({"task_end":{"task":"far-work","status":"open"}}),
    );
    assert_eq!(open["wait"]["why"], "status");
    let carried = dispatch(
        &mut pair.b,
        "mail.deliver",
        json!({"id":"forged-3","from":"apps","to":"infra","kind":"notice","via":"studio",
            "payload":{"task_end":{"task":"far-work","status":"done"}}}),
    )
    .unwrap();
    assert_eq!(carried["wait"]["why"], "via");
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "waiting");
    let drops = audited(&mut pair.b, "wait.dropped");
    assert_eq!(drops.len(), 3, "{drops:?}");
    assert_eq!(drops[0]["from"], "research");
    assert_eq!(drops[0]["why"], "no_wait");
    no_wait_mail(&mut pair.b);
    // The runtime alone authors these keys.
    assert!(
        dispatch(
            &mut pair.b,
            "mail.send",
            json!({"from":"infra","to":"apps","kind":"notice","body":"x",
                "payload":{"task_end":{"task":"far-work","status":"done"}}}),
        )
        .is_err()
    );
}

#[test]
fn an_end_for_a_far_task_nobody_waits_on_resumes_nothing() {
    let mut pair = subscribed();
    let other = report(
        &mut pair.b,
        "end-1",
        "apps",
        json!({"task_end":{"task":"other-work","status":"done"}}),
    );
    assert_eq!(other["wait"]["state"], "dropped");
    let closed = report(
        &mut pair.b,
        "end-2",
        "apps",
        json!({"wait_closed":{"task":"other-work","reason":"retired"}}),
    );
    assert_eq!(closed["wait"]["state"], "dropped");
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "waiting");
    // Malformed or doubled reports are refused on the wire.
    for bad in [
        json!({"task_end":{"status":"done"}}),
        json!({"task_end":{"task":"far-work","status":"done"},"wait_closed":{"task":"far-work"}}),
        json!({"task_end":{"task":"far-work","status":3}}),
    ] {
        assert!(
            dispatch(
                &mut pair.b,
                "mail.deliver",
                json!({"id":"bad","from":"apps","to":"infra","kind":"notice","payload":bad}),
            )
            .is_err(),
            "{bad}"
        );
    }
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "waiting");
}

#[test]
fn a_duplicate_end_is_harmless() {
    let mut pair = subscribed();
    let end = json!({"task_end":{"task":"far-work","status":"done","reason":"done"}});
    assert_eq!(
        report(&mut pair.b, "dup-1", "apps", end.clone())["wait"]["state"],
        "applied"
    );
    let first = work(&mut pair.b, "waiting-1");
    assert_eq!(first["state"], "ready");
    // The same report again, under its id or a new one, changes nothing.
    for id in ["dup-1", "dup-2"] {
        assert_eq!(
            report(&mut pair.b, id, "apps", end.clone())["wait"]["state"],
            "dropped"
        );
    }
    // A closure after the end cannot fault what already resumed.
    report(
        &mut pair.b,
        "dup-3",
        "apps",
        json!({"wait_closed":{"task":"far-work","reason":"retired"}}),
    );
    assert_eq!(work(&mut pair.b, "waiting-1"), first);
    assert_eq!(audited(&mut pair.b, "wait.resolved").len(), 1);
    assert!(wait_payloads(&pair.b, "apps", "wait_unsubscribe").is_empty());
}

#[test]
fn two_local_tasks_waiting_on_one_far_task_both_resume() {
    let mut pair = subscribed();
    waiter(&mut pair.b, "waiting-2");
    defer_remote(&mut pair.b, "waiting-2", far()).unwrap();
    exchange(&mut pair);
    assert_eq!(rows(&pair.a, "far-work"), 2);
    dispatch(
        &mut pair.a,
        "task.update",
        json!({"id":"far-work","actor":"apps","status":"done"}),
    )
    .unwrap();
    // One task_end per subscription, each naming no waiting task but echoing
    // its own nonce, resumes both.
    let mut sent: Vec<Value> = nonces(&pair.a, "infra", "task_end");
    let mut held = vec![
        json!(nonce_of(&pair.b, "waiting-1").unwrap()),
        json!(nonce_of(&pair.b, "waiting-2").unwrap()),
    ];
    sent.sort_by_key(Value::to_string);
    held.sort_by_key(Value::to_string);
    assert_eq!(sent, held);
    dispatch(&mut pair.a, "mail.flush", json!({})).unwrap();
    for id in ["waiting-1", "waiting-2"] {
        assert_eq!(work(&mut pair.b, id)["state"], "ready", "{id}");
    }
}

#[test]
fn re_deferring_elsewhere_unsubscribes_and_the_owner_row_goes() {
    let mut pair = subscribed();
    // Re-deferring on the same far task keeps the one subscription.
    defer_remote(&mut pair.b, "waiting-1", far()).unwrap();
    assert_eq!(wait_payloads(&pair.b, "apps", "wait_subscribe").len(), 1);
    assert!(wait_payloads(&pair.b, "apps", "wait_unsubscribe").is_empty());
    // Re-deferring on anything else retracts it in the same transaction.
    dispatch(
        &mut pair.b,
        "task.defer",
        json!({"id":"waiting-1","wait":{"kind":"time","reason":"later","until_ms":i64::MAX}}),
    )
    .unwrap();
    assert_eq!(
        wait_payloads(&pair.b, "apps", "wait_unsubscribe"),
        vec![json!({"task":"far-work","subscriber":"infra","subscriber_task":"waiting-1"})]
    );
    exchange(&mut pair);
    assert_eq!(rows(&pair.a, "far-work"), 0);
    // Every other way off a live wait retracts it too: a manual stop, and
    // enqueueing the task afresh.
    for leave in [
        json!({"kind":"manual","reason":"stop"}),
        json!({"kind":"enqueue"}),
    ] {
        defer_remote(&mut pair.b, "waiting-1", far()).unwrap();
        exchange(&mut pair);
        assert_eq!(rows(&pair.a, "far-work"), 1);
        if leave["kind"] == "enqueue" {
            dispatch(
                &mut pair.b,
                "task.enqueue",
                json!({"id":"waiting-1","brief":"Start again."}),
            )
            .unwrap();
        } else {
            dispatch(
                &mut pair.b,
                "task.defer",
                json!({"id":"waiting-1","wait":leave}),
            )
            .unwrap();
        }
        exchange(&mut pair);
        assert_eq!(rows(&pair.a, "far-work"), 0, "{leave}");
    }
}

#[test]
fn cancelling_the_waiting_task_unsubscribes() {
    let mut pair = subscribed();
    dispatch(
        &mut pair.b,
        "task.update",
        json!({"id":"waiting-1","actor":"infra","status":"cancelled"}),
    )
    .unwrap();
    // Sent with the cancel, not left for a dispatcher pass.
    assert_eq!(
        wait_payloads(&pair.b, "apps", "wait_unsubscribe"),
        vec![json!({"task":"far-work","subscriber":"infra","subscriber_task":"waiting-1"})]
    );
    exchange(&mut pair);
    assert_eq!(rows(&pair.a, "far-work"), 0);
    // A later pass sends nothing more.
    pass(&mut pair.b);
    assert_eq!(wait_payloads(&pair.b, "apps", "wait_unsubscribe").len(), 1);
}

#[test]
fn a_handoff_closes_the_wait_and_faults_naming_the_new_owner() {
    let mut pair = subscribed();
    dispatch(
        &mut pair.a,
        "task.handoff",
        json!({"id":"far-work","actor":"apps","target":"research"}),
    )
    .unwrap();
    // An offer alone changes nothing: apps may still finish the task.
    assert_eq!(rows(&pair.a, "far-work"), 1);
    dispatch(
        &mut pair.a,
        "task.accept",
        json!({"id":"far-work","actor":"research"}),
    )
    .unwrap();
    assert_eq!(rows(&pair.a, "far-work"), 0);
    assert_eq!(
        wait_payloads(&pair.a, "infra", "wait_closed"),
        vec![json!({"task":"far-work","reason":"handed_off","owner":"research"})]
    );
    assert_eq!(
        outbox(&pair.a, "infra")
            .iter()
            .find(|m| m["payload"]["wait_closed"].is_object())
            .unwrap()["from"],
        "apps"
    );
    dispatch(&mut pair.a, "mail.flush", json!({})).unwrap();
    let faulted = work(&mut pair.b, "waiting-1");
    assert_eq!(faulted["state"], "fault");
    let reason = faulted["reason"].as_str().unwrap();
    assert!(
        reason.contains("handed_off") && reason.contains("research"),
        "{reason}"
    );
    assert!(wait_payloads(&pair.b, "apps", "wait_unsubscribe").is_empty());
}

#[test]
fn removing_a_ship_from_shared_with_closes_its_wait() {
    let mut pair = subscribed();
    // Narrowing to a list that still holds infra keeps the row.
    share(&mut pair.a, "far-work", json!(["infra", "research"]));
    assert_eq!(rows(&pair.a, "far-work"), 1);
    share(&mut pair.a, "far-work", json!(["research"]));
    assert_eq!(rows(&pair.a, "far-work"), 0);
    assert_eq!(
        wait_payloads(&pair.a, "infra", "wait_closed"),
        vec![json!({"task":"far-work","reason":"refused"})]
    );
    assert_eq!(audited(&mut pair.a, "wait.unshared").len(), 1);
    dispatch(&mut pair.a, "mail.flush", json!({})).unwrap();
    let faulted = work(&mut pair.b, "waiting-1");
    assert_eq!(faulted["state"], "fault");
    assert!(faulted["reason"].as_str().unwrap().contains("refused"));
}

#[test]
fn a_self_ref_and_bad_refs_are_refused() {
    let mut pair = pair();
    waiter(&mut pair.b, "waiting-1");
    for (reference, why) in [
        (json!({"ship":"infra","task":"waiting-1"}), "itself"),
        (json!({"ship":"infra","task":"infra-duty"}), "kind task"),
        (json!({"ship":"nobody","task":"far-work"}), "unknown ship"),
        (json!({"ship":"apps","task":"Far_Work"}), "lowercase"),
        (json!({"ship":"apps","task":"bad task"}), "identifier"),
        (json!({"ship":"captain","task":"far-work"}), "reserved"),
        (json!({"ship":"apps"}), "task"),
        (
            json!({"ship":"apps","task":"far-work","machine":"studio"}),
            "not reached",
        ),
        (
            json!({"ship":"storefront","task":"icons"}),
            "another machine",
        ),
        (
            json!({"ship":"apps","task":"far-work","status":"done"}),
            "only",
        ),
        (json!("apps/far-work"), "ref"),
    ] {
        let error = defer_remote(&mut pair.b, "waiting-1", reference.clone()).unwrap_err();
        assert!(format!("{error:#}").contains(why), "{reference}: {error:#}");
    }
    assert!(outbox(&pair.b, "apps").is_empty());
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "ready");
    // A resolution the caller writes into the wait is not stored.
    let deferred = dispatch(
        &mut pair.b,
        "task.defer",
        json!({"id":"waiting-1","wait":{"kind":"remote","reason":"r","ref":far(),
            "end":{"notice":"task_end","status":"done"},"until_ms":1}}),
    )
    .unwrap();
    assert_eq!(
        deferred["wait"],
        json!({"kind":"remote","reason":"r","ref":{"ship":"apps","task":"far-work"}})
    );
    pass(&mut pair.b);
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "waiting");
}

#[test]
fn a_machine_ref_routes_through_that_machine_and_checks_via() {
    let Pair { _temp, mut b, .. } = pair();
    waiter(&mut b, "waiting-1");
    let reference = json!({"ship":"storefront","task":"icons","machine":"studio"});
    defer_remote(&mut b, "waiting-1", reference.clone()).unwrap();
    // Queued for the machine's peer, as mail to that ship would be.
    let queued = outbox(&b, "storefront");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0]["delivery"], "pending");
    let peer: String = b
        .connection()
        .query_row(
            "SELECT json_extract(record,'$.location.peer') FROM ships WHERE name='storefront'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(peer, "machine-studio");
    let end = |via: Option<&str>| {
        let mut message = json!({"id":format!("end-{}", via.unwrap_or("none")),"from":"storefront",
            "to":"infra","kind":"notice","payload":{"task_end":{"task":"icons","status":"done"}}});
        if let Some(via) = via {
            message["via"] = json!(via);
        }
        message
    };
    // No via, or another machine's, is not the machine the ref names.
    for via in [None, Some("elsewhere")] {
        let dropped = dispatch(&mut b, "mail.deliver", end(via)).unwrap();
        assert_eq!(dropped["wait"]["why"], "via", "{via:?}");
    }
    assert_eq!(work(&mut b, "waiting-1")["state"], "waiting");
    let applied = dispatch(&mut b, "mail.deliver", end(Some("studio"))).unwrap();
    assert_eq!(applied["wait"]["state"], "applied");
    let resumed = work(&mut b, "waiting-1");
    assert_eq!(resumed["state"], "ready");
    assert!(
        resumed["resumed_by"]
            .as_str()
            .unwrap()
            .contains("machine studio")
    );
}

#[test]
fn an_already_ended_far_task_resumes_at_once() {
    let mut pair = pair();
    dispatch(
        &mut pair.a,
        "task.update",
        json!({"id":"far-work","actor":"apps","shared_with":["infra"],"status":"done"}),
    )
    .unwrap();
    waiter(&mut pair.b, "waiting-1");
    defer_remote(&mut pair.b, "waiting-1", far()).unwrap();
    exchange(&mut pair);
    assert_eq!(rows(&pair.a, "far-work"), 0);
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "ready");
}

#[test]
fn a_home_without_the_shared_with_column_reads_it_as_empty() {
    let Pair { _temp, a, .. } = pair();
    a.connection()
        .execute_batch("ALTER TABLE tasks DROP COLUMN shared_with;")
        .unwrap();
    let task = a.task("far-work").unwrap();
    assert_eq!(task["shared_with"], json!([]));
    assert_eq!(task["owner"], "apps");
    assert!(a.task("no-such-task").is_err());
}

// Step 3: resync on reconnect, staleness, and the step 2 review follow-ups.

/// Point a home's peer at an executable that cannot start, so every call to
/// it fails as an unreachable link does, or back at the real one.
fn link(store: &mut Store, peer: &str, home: &std::path::Path, up: bool) {
    let executable = if up {
        env!("CARGO_BIN_EXE_privateer")
    } else {
        "/nonexistent/privateer"
    };
    dispatch(
        store,
        "peer.put",
        json!({"name":peer,"transport":"local","executable":executable,"home":home}),
    )
    .unwrap();
}

fn homes(pair: &Pair) -> (std::path::PathBuf, std::path::PathBuf) {
    (pair.a.home().to_path_buf(), pair.b.home().to_path_buf())
}

/// Ordinary mail from one ship to another behind a peer: recorded, then one
/// delivery attempt, as mail.send makes.
fn ping(store: &mut Store, id: &str, from: &str, to: &str) {
    dispatch(
        store,
        "mail.send",
        json!({"id":id,"from":from,"to":to,"kind":"notice","body":"ping"}),
    )
    .unwrap();
}

/// Move a home's last resyncs a minute and more into the past, as if that
/// long had gone by: a link resyncs at most once a minute.
fn later(store: &Store) {
    store
        .connection()
        .execute(
            "UPDATE wait_links SET resynced_ms=resynced_ms-61000 WHERE resynced_ms IS NOT NULL",
            [],
        )
        .unwrap();
}

/// Take the waiting home's link to the owner down and bring it back: a
/// message fails to reach the owner, then the next flush reaches it, which
/// is the link heard again. Each outage comes after the resync spacing.
fn outage(pair: &mut Pair, id: &str) {
    later(&pair.b);
    let (a_home, _) = homes(pair);
    link(&mut pair.b, "home-a", &a_home, false);
    ping(&mut pair.b, id, "infra", "apps");
    assert_eq!(
        outbox(&pair.b, "apps").last().unwrap()["delivery"],
        "pending"
    );
    link(&mut pair.b, "home-a", &a_home, true);
    dispatch(&mut pair.b, "mail.flush", json!({})).unwrap();
}

#[test]
fn a_link_back_resubscribes_and_the_owner_answers_a_missed_end() {
    let mut pair = subscribed();
    let nonce = nonce_of(&pair.b, "waiting-1").unwrap();
    // While the far task is open, each outage ends with the live subscription
    // sent again, with the nonce it holds; the owner keeps its one row.
    for round in 0..2 {
        outage(&mut pair, &format!("ping-{round}"));
        assert_eq!(rows(&pair.a, "far-work"), 1);
        assert_eq!(work(&mut pair.b, "waiting-1")["state"], "waiting");
    }
    assert_eq!(
        nonces(&pair.b, "apps", "wait_subscribe"),
        vec![json!(nonce); 3]
    );
    let resyncs = audited(&mut pair.b, "wait.resync");
    assert_eq!(resyncs.len(), 2, "{resyncs:?}");
    assert!(
        resyncs
            .iter()
            .all(|r| r["peer"] == "home-a" && r["resubscribed"] == 1)
    );
    let row_nonce: String = pair
        .a
        .connection()
        .query_row("SELECT nonce FROM task_subscriptions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(row_nonce, nonce);
    // The far task ends while the owner cannot reach the waiting home, and
    // its task_end is lost (refused on a machine link, it is quarantined).
    let (_, b_home) = homes(&pair);
    link(&mut pair.a, "home-b", &b_home, false);
    dispatch(
        &mut pair.a,
        "task.update",
        json!({"id":"far-work","actor":"apps","status":"done"}),
    )
    .unwrap();
    dispatch(&mut pair.a, "mail.flush", json!({})).unwrap();
    assert_eq!(rows(&pair.a, "far-work"), 0);
    pair.a
        .connection()
        .execute(
            "UPDATE mail SET delivery='quarantined' WHERE json_extract(payload,'$.task_end') IS NOT NULL",
            [],
        )
        .unwrap();
    link(&mut pair.a, "home-b", &b_home, true);
    // The waiting home's link comes back: it subscribes again, and the owner
    // answers at once with the final status it missed.
    outage(&mut pair, "ping-2");
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "waiting");
    let answered: Vec<Value> = outbox(&pair.a, "infra")
        .into_iter()
        .filter(|m| m["payload"]["task_end"].is_object() && m["delivery"] == "pending")
        .map(|m| m["payload"]["task_end"].clone())
        .collect();
    assert_eq!(
        answered,
        vec![json!({"task":"far-work","status":"done","reason":"done","nonce":nonce})]
    );
    dispatch(&mut pair.a, "mail.flush", json!({})).unwrap();
    let resumed = work(&mut pair.b, "waiting-1");
    assert_eq!(resumed["state"], "ready", "{resumed}");
    assert!(resumed["resumed_by"].as_str().unwrap().contains("done"));
    // A replay changes nothing: the resolved wait is not sent again, a
    // resync that sends nothing is not audited, and a repeated answer finds
    // no wait.
    let sent = outbox(&pair.b, "apps").len();
    let resynced = audited(&mut pair.b, "wait.resync").len();
    outage(&mut pair, "ping-3");
    let after = outbox(&pair.b, "apps");
    assert_eq!(after.len(), sent + 1, "only the ping: {after:?}");
    assert_eq!(audited(&mut pair.b, "wait.resync").len(), resynced);
    let repeated = report(
        &mut pair.b,
        "end-again",
        "apps",
        json!({"task_end":{"task":"far-work","status":"done","nonce":nonce}}),
    );
    assert_eq!(repeated["wait"]["why"], "no_wait");
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "ready");
    no_wait_mail(&mut pair.b);
}

#[test]
fn an_owner_link_back_reports_a_subscribed_task_that_ended_without_its_report() {
    let mut pair = subscribed();
    let nonce = nonce_of(&pair.b, "waiting-1").unwrap();
    // A row outlived its task: it ended by a path that sent no report.
    pair.a
        .connection()
        .execute(
            "UPDATE tasks SET status='cancelled' WHERE id='far-work'",
            [],
        )
        .unwrap();
    let (_, b_home) = homes(&pair);
    link(&mut pair.a, "home-b", &b_home, false);
    ping(&mut pair.a, "owner-ping", "apps", "infra");
    link(&mut pair.a, "home-b", &b_home, true);
    dispatch(&mut pair.a, "mail.flush", json!({})).unwrap();
    // Hearing the link again, the owner reports the final status and drops
    // the row; the waiting task faults on the cancel.
    assert_eq!(rows(&pair.a, "far-work"), 0);
    assert_eq!(audited(&mut pair.a, "wait.resync")[0]["answered"], 1);
    assert_eq!(nonces(&pair.a, "infra", "task_end"), vec![json!(nonce)]);
    dispatch(&mut pair.a, "mail.flush", json!({})).unwrap();
    let faulted = work(&mut pair.b, "waiting-1");
    assert_eq!(faulted["state"], "fault", "{faulted}");
    assert!(faulted["reason"].as_str().unwrap().contains("cancelled"));
    // Again: nothing more to report, and nothing audited.
    later(&pair.a);
    link(&mut pair.a, "home-b", &b_home, false);
    ping(&mut pair.a, "owner-ping-2", "apps", "infra");
    link(&mut pair.a, "home-b", &b_home, true);
    dispatch(&mut pair.a, "mail.flush", json!({})).unwrap();
    assert_eq!(audited(&mut pair.a, "wait.resync").len(), 1);
    assert_eq!(nonces(&pair.a, "infra", "task_end").len(), 1);
}

#[test]
fn a_machine_heard_after_a_silence_resubscribes_and_a_steady_link_does_not() {
    let Pair { _temp, mut b, .. } = pair();
    waiter(&mut b, "waiting-1");
    defer_remote(
        &mut b,
        "waiting-1",
        json!({"ship":"storefront","task":"icons","machine":"studio"}),
    )
    .unwrap();
    let hello = |b: &mut Store, id: &str| {
        dispatch(
            b,
            "mail.deliver",
            json!({"id":id,"from":"storefront","to":"infra","kind":"notice","body":"hello",
                "via":"studio"}),
        )
        .unwrap();
    };
    // The first mail heard over the link only records it; more within the
    // contact spacing changes nothing.
    hello(&mut b, "hello-1");
    hello(&mut b, "hello-2");
    assert!(audited(&mut b, "wait.resync").is_empty());
    assert_eq!(wait_payloads(&b, "storefront", "wait_subscribe").len(), 1);
    // That machine calls this one; after a silence well past its contact
    // spacing, it has come back.
    b.connection()
        .execute(
            "UPDATE wait_links SET heard_ms=heard_ms-600000 WHERE peer='machine-studio'",
            [],
        )
        .unwrap();
    hello(&mut b, "hello-3");
    assert_eq!(audited(&mut b, "wait.resync")[0]["resubscribed"], 1);
    assert_eq!(
        nonces(&b, "storefront", "wait_subscribe"),
        vec![json!(nonce_of(&b, "waiting-1").unwrap()); 2]
    );
}

/// The captain's needs, as runtime.needs lists them for the waiting home.
fn stale_needs(store: &mut Store) -> Vec<Value> {
    dispatch(store, "runtime.needs", json!({})).unwrap()["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["evidence"] == "stale_remote_wait")
        .cloned()
        .collect()
}

fn age_wait(store: &Store, task: &str, ms: i64) {
    store
        .connection()
        .execute(
            "UPDATE task_schedule SET record=json_set(record,'$.subscription.since_ms',
               json_extract(record,'$.subscription.since_ms')-?2) WHERE task_id=?1",
            rusqlite::params![task, ms],
        )
        .unwrap();
}

#[test]
fn a_stale_remote_wait_shows_in_needs_and_never_resolves_itself() {
    let mut pair = subscribed();
    // Under the threshold (an hour by default) it is not a need.
    age_wait(&pair.b, "waiting-1", 3_500_000);
    assert!(stale_needs(&mut pair.b).is_empty());
    age_wait(&pair.b, "waiting-1", 200_000);
    let needs = stale_needs(&mut pair.b);
    assert_eq!(needs.len(), 1, "{needs:?}");
    let item = &needs[0];
    assert_eq!(item["kind"], "blocker");
    assert_eq!(item["ship"], "infra");
    assert_eq!(item["task_id"], "waiting-1");
    assert_eq!(item["status"], "waiting");
    assert_eq!(
        item["far"],
        json!({"ship":"apps","task":"far-work","machine":null})
    );
    assert_eq!(item["link"]["peer"], "home-a");
    assert!(item["link"]["heard_ms"].as_i64().unwrap() > 0, "{item}");
    assert!(item["waited_ms"].as_i64().unwrap() >= 3_700_000);
    let summary = item["summary"].as_str().unwrap();
    assert!(
        summary.contains("far-work")
            && summary.contains("apps")
            && summary.contains("home-a")
            && summary.contains("last heard"),
        "{summary}"
    );
    // It never resumes or faults by itself, and sends nothing.
    let sent = outbox(&pair.b, "apps").len();
    for _ in 0..3 {
        pass(&mut pair.b);
    }
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "waiting");
    assert_eq!(outbox(&pair.b, "apps").len(), sent);
    assert_eq!(stale_needs(&mut pair.b).len(), 1);
    // The threshold is the waiting ship's setting; 0 shows none.
    let manifest = pair
        ._temp
        .path()
        .canonicalize()
        .unwrap()
        .join("manifest.json");
    for (secs, shown) in [(86_400, 0), (60, 1), (0, 0)] {
        dispatch(
            &mut pair.b,
            "ship.put",
            json!({"name":"infra","scope":"work","location":"here",
                "first_mate":{"manifest":manifest,"duty_task":"infra-duty",
                    "remote_wait_stale_secs":secs}}),
        )
        .unwrap();
        assert_eq!(stale_needs(&mut pair.b).len(), shown, "{secs}");
    }
    assert!(
        dispatch(
            &mut pair.b,
            "ship.put",
            json!({"name":"infra","scope":"work","location":"here",
                "first_mate":{"manifest":manifest,"duty_task":"infra-duty",
                    "remote_wait_stale_secs":604_801}}),
        )
        .is_err()
    );
    // The far end's report still ends it as usual.
    dispatch(
        &mut pair.a,
        "task.update",
        json!({"id":"far-work","actor":"apps","status":"done"}),
    )
    .unwrap();
    dispatch(&mut pair.a, "mail.flush", json!({})).unwrap();
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "ready");
    assert!(stale_needs(&mut pair.b).is_empty());
}

/// The scheduling-fault notices the waiting ship has been sent for a task.
fn fault_notices(store: &Store, task: &str) -> Vec<String> {
    let mut statement = store
        .connection()
        .prepare(
            "SELECT id FROM mail WHERE recipient='infra' AND task_id=?1
               AND json_extract(payload,'$.scheduling_fault') IS NOT NULL ORDER BY seq",
        )
        .unwrap();
    statement
        .query_map([task], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

#[test]
fn a_second_fault_in_one_revision_wakes_the_first_mate_again() {
    let mut pair = pair();
    waiter(&mut pair.b, "waiting-1");
    // Not shared: the owner refuses, and the wait faults.
    defer_remote(&mut pair.b, "waiting-1", far()).unwrap();
    exchange(&mut pair);
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "fault");
    assert_eq!(
        fault_notices(&pair.b, "waiting-1"),
        ["scheduling-fault-waiting-1-1"]
    );
    // The first mate re-defers in the same revision on another far task,
    // which is refused too: a second fault, with its own notice.
    defer_remote(
        &mut pair.b,
        "waiting-1",
        json!({"ship":"research","task":"other-work"}),
    )
    .unwrap();
    assert_eq!(work(&mut pair.b, "waiting-1")["revision"], 1);
    exchange(&mut pair);
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "fault");
    assert_eq!(
        fault_notices(&pair.b, "waiting-1"),
        [
            "scheduling-fault-waiting-1-1",
            "scheduling-fault-waiting-1-1-2"
        ]
    );
    // The same fault reported again on the faulted schedule sends nothing.
    pass(&mut pair.b);
    report(
        &mut pair.b,
        "closed-again",
        "research",
        json!({"wait_closed":{"task":"other-work","reason":"refused"}}),
    );
    assert_eq!(fault_notices(&pair.b, "waiting-1").len(), 2);
}

#[test]
fn a_late_refusal_for_an_older_subscription_does_not_fault_a_newer_wait() {
    let mut pair = pair();
    waiter(&mut pair.b, "waiting-1");
    // The first subscription reaches the owner before the task is shared,
    // and its refusal is still on its way back.
    defer_remote(&mut pair.b, "waiting-1", far()).unwrap();
    let old = nonce_of(&pair.b, "waiting-1").unwrap();
    dispatch(&mut pair.b, "mail.flush", json!({})).unwrap();
    assert_eq!(nonces(&pair.a, "infra", "wait_closed"), vec![json!(old)]);
    // Meanwhile the owner shares it, and the waiting task waits on it anew.
    share(&mut pair.a, "far-work", json!(["infra"]));
    dispatch(
        &mut pair.b,
        "task.defer",
        json!({"id":"waiting-1","wait":{"kind":"manual","reason":"pause"}}),
    )
    .unwrap();
    defer_remote(&mut pair.b, "waiting-1", far()).unwrap();
    let new = nonce_of(&pair.b, "waiting-1").unwrap();
    assert_ne!(old, new);
    dispatch(&mut pair.b, "mail.flush", json!({})).unwrap();
    assert_eq!(rows(&pair.a, "far-work"), 1);
    // The old refusal arrives: it names the old nonce and is dropped.
    dispatch(&mut pair.a, "mail.flush", json!({})).unwrap();
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "waiting");
    let dropped = audited(&mut pair.b, "wait.dropped");
    assert_eq!(dropped.len(), 1, "{dropped:?}");
    assert_eq!(dropped[0]["why"], "nonce");
    assert_eq!(dropped[0]["nonce"], json!(old));
    // An answer from an owner that echoes no nonce still applies, as one from
    // a step 1 owner would.
    let applied = report(
        &mut pair.b,
        "end-without-nonce",
        "apps",
        json!({"task_end":{"task":"far-work","status":"done"}}),
    );
    assert_eq!(applied["wait"]["state"], "applied");
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "ready");
}

#[test]
fn a_rewait_after_a_handoff_subscribes_as_the_owner_to_the_refs_ship() {
    let mut pair = subscribed();
    // The far task is handed to research: the wait faults naming it, and the
    // re-wait goes to the ship its ref names now.
    dispatch(
        &mut pair.a,
        "task.handoff",
        json!({"id":"far-work","actor":"apps","target":"research"}),
    )
    .unwrap();
    dispatch(
        &mut pair.a,
        "task.accept",
        json!({"id":"far-work","actor":"research"}),
    )
    .unwrap();
    dispatch(&mut pair.a, "mail.flush", json!({})).unwrap();
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "fault");
    defer_remote(
        &mut pair.b,
        "waiting-1",
        json!({"ship":"research","task":"far-work"}),
    )
    .unwrap();
    let sent: Vec<Value> = outbox(&pair.b, "research")
        .into_iter()
        .filter(|m| m["payload"]["wait_subscribe"].is_object())
        .collect();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["from"], "infra");
    assert_eq!(sent[0]["payload"]["wait_subscribe"]["subscriber"], "infra");
    // The waiting task itself is handed to another ship here: its schedule
    // still names infra, so the new owner cannot defer it as infra.
    let manifest = pair
        ._temp
        .path()
        .canonicalize()
        .unwrap()
        .join("manifest.json");
    dispatch(
        &mut pair.b,
        "task.create",
        json!({"id":"ops-duty","charter_id":"project","owner":"ops","title":"Duty"}),
    )
    .unwrap();
    dispatch(
        &mut pair.b,
        "ship.put",
        json!({"name":"ops","scope":"work","location":"here",
            "first_mate":{"manifest":manifest,"duty_task":"ops-duty"}}),
    )
    .unwrap();
    dispatch(
        &mut pair.b,
        "task.handoff",
        json!({"id":"waiting-1","actor":"infra","target":"ops"}),
    )
    .unwrap();
    dispatch(
        &mut pair.b,
        "task.accept",
        json!({"id":"waiting-1","actor":"ops"}),
    )
    .unwrap();
    let error = defer_remote(
        &mut pair.b,
        "waiting-1",
        json!({"ship":"research","task":"far-work"}),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("enqueue"), "{error:#}");
    // Once ops enqueues its own assignment, the subscribe is from ops.
    dispatch(
        &mut pair.b,
        "task.enqueue",
        json!({"id":"waiting-1","brief":"Continue for ops."}),
    )
    .unwrap();
    defer_remote(
        &mut pair.b,
        "waiting-1",
        json!({"ship":"research","task":"far-work"}),
    )
    .unwrap();
    let latest = outbox(&pair.b, "research")
        .into_iter()
        .rev()
        .find(|m| m["payload"]["wait_subscribe"].is_object())
        .unwrap();
    assert_eq!(latest["from"], "ops");
    assert_eq!(latest["payload"]["wait_subscribe"]["subscriber"], "ops");
}

#[test]
fn one_end_resolves_every_wait_on_the_far_task_past_a_page() {
    let mut pair = pair();
    let ids: Vec<String> = (0..105).map(|n| format!("waiting-{n:03}")).collect();
    for id in &ids {
        waiter(&mut pair.b, id);
        defer_remote(&mut pair.b, id, far()).unwrap();
    }
    let applied = report(
        &mut pair.b,
        "end-all",
        "apps",
        json!({"task_end":{"task":"far-work","status":"done"}}),
    );
    assert_eq!(applied["wait"]["tasks"].as_array().unwrap().len(), 105);
    // work.status lists a bounded page, so read the schedules themselves.
    let ready: i64 = pair
        .b
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM task_schedule WHERE task_id LIKE 'waiting-%'
               AND json_extract(record,'$.state')='ready'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ready, 105);
}

#[test]
fn an_agents_own_flush_carries_its_ships_wait_notices() {
    // Why mail.deliver does not refuse wait notices from an agent caller: an
    // agent's flush (mail.flush, or the attempt mail.send makes) delivers its
    // own ship's pending mail, runtime notices included, and the peer call
    // carries the agent's identity, so the owner's home sees an agent.
    let mut pair = pair();
    share(&mut pair.a, "far-work", json!(["infra"]));
    waiter(&mut pair.b, "waiting-1");
    defer_remote(&mut pair.b, "waiting-1", far()).unwrap();
    pair.b.set_caller(privateer::store::Caller::Agent {
        actor: "infra".into(),
    });
    let flushed = dispatch(&mut pair.b, "mail.flush", json!({})).unwrap();
    assert_eq!(flushed["delivered"], 1, "{flushed}");
    assert_eq!(rows(&pair.a, "far-work"), 1);
    // An agent still cannot author one.
    assert!(
        dispatch(
            &mut pair.b,
            "mail.send",
            json!({"id":"agent-spoof","from":"infra","to":"apps","kind":"notice","body":"x",
                "payload":{"wait_subscribe":{"task":"far-work","subscriber":"infra",
                    "subscriber_task":"other"}}}),
        )
        .is_err()
    );
}

// Step 4: retire, refused wait notices, and the step 3 review follow-ups.

/// Retire `ship` as the captain does, in a fleet that lists both homes of
/// the pair, through the real CLI.
fn retire(pair: &Pair, ship: &str) -> Value {
    retire_as(pair, ship, None)
}

/// Retire `ship` through the real CLI, as the captain or as `agent`, the
/// ship that created both ships of the pair.
fn retire_as(pair: &Pair, ship: &str, agent: Option<&str>) -> Value {
    let root = pair._temp.path().canonicalize().unwrap();
    let (a_home, b_home) = homes(pair);
    let fleet = rusqlite::Connection::open(root.join("fleet.sqlite")).unwrap();
    fleet
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS ships(name TEXT PRIMARY KEY, home TEXT NOT NULL,
               created_by TEXT NOT NULL, created_ms INTEGER NOT NULL, retired_ms INTEGER);",
        )
        .unwrap();
    for (name, home) in [("apps", &a_home), ("infra", &b_home)] {
        fleet
            .execute(
                "INSERT OR IGNORE INTO ships(name, home, created_by, created_ms)
                 VALUES(?1, ?2, ?3, 1)",
                rusqlite::params![name, home.to_string_lossy(), agent.unwrap_or("captain")],
            )
            .unwrap();
    }
    let home = if ship == "apps" { &a_home } else { &b_home };
    let mut command = support::isolated(&root);
    command
        .args(["--home", &home.to_string_lossy(), "ship.retire", "--json"])
        .arg(json!({"name": ship}).to_string());
    if let Some(agent) = agent {
        command
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", agent);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn retiring_the_owner_closes_its_waits_as_retired_and_faults_the_waiters() {
    let mut pair = subscribed();
    let nonce = nonce_of(&pair.b, "waiting-1").unwrap();
    let retired = retire(&pair, "apps");
    assert_eq!(retired["retired"], true, "{retired}");
    let waits = &retired["waits"];
    assert_eq!(waits["closed"], 1, "{retired}");
    assert_eq!(waits["tasks"], json!(["far-work"]));
    // Its notices left before it stopped: the drain carried them.
    assert_eq!(waits["drain"], json!({"delivered": 1, "pending": 0}));
    assert_eq!(rows(&pair.a, "far-work"), 0);
    let closed: Vec<Value> = outbox(&pair.a, "infra")
        .into_iter()
        .filter(|m| m["payload"]["wait_closed"].is_object())
        .collect();
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(
        closed[0]["payload"]["wait_closed"],
        json!({"task":"far-work","reason":"retired","nonce":nonce})
    );
    assert_eq!(closed[0]["delivery"], "delivered");
    // The waiting task faults with that reason and never resumes.
    let faulted = work(&mut pair.b, "waiting-1");
    assert_eq!(faulted["state"], "fault", "{faulted}");
    assert!(
        faulted["reason"]
            .as_str()
            .unwrap()
            .contains("was closed: retired"),
        "{faulted}"
    );
    pass(&mut pair.b);
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "fault");
    // The owner removed the row itself: nothing is sent back.
    assert!(wait_payloads(&pair.b, "apps", "wait_unsubscribe").is_empty());
    assert_eq!(audited(&mut pair.a, "wait.retired").len(), 1);
    no_wait_mail(&mut pair.b);
}

#[test]
fn retiring_the_waiting_ship_unsubscribes_and_the_owner_rows_go() {
    let mut pair = subscribed();
    let nonce = nonce_of(&pair.b, "waiting-1").unwrap();
    let retired = retire(&pair, "infra");
    let waits = &retired["waits"];
    assert_eq!(waits["unsubscribed"], 1, "{retired}");
    assert_eq!(waits["closed"], 0, "{retired}");
    assert_eq!(waits["drain"], json!({"delivered": 1, "pending": 0}));
    assert_eq!(rows(&pair.a, "far-work"), 0);
    assert_eq!(
        nonces(&pair.b, "apps", "wait_unsubscribe"),
        vec![json!(nonce)]
    );
    // Its waiting task is not cancelled; its schedule is left as it was.
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "waiting");
    assert_eq!(
        dispatch(&mut pair.b, "task.show", json!({"id":"waiting-1"})).unwrap()["status"],
        "blocked"
    );
    // Its charter revoked, a link heard again does not subscribe it anew.
    dispatch(&mut pair.b, "charter.revoke", json!({"id":"project"})).unwrap();
    let subscribes = wait_payloads(&pair.b, "apps", "wait_subscribe").len();
    outage(&mut pair, "after-retire");
    assert_eq!(
        wait_payloads(&pair.b, "apps", "wait_subscribe").len(),
        subscribes
    );
    assert_eq!(rows(&pair.a, "far-work"), 0);
}

#[test]
fn an_agents_retire_leaves_the_notices_to_the_retiring_ships_dispatcher() {
    let mut pair = subscribed();
    // An agent cannot carry another ship's mail, so its retire only waits
    // while the retiring ship's dispatcher delivers; a thread flushing that
    // home as the runtime stands in for the dispatcher here.
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (_, b_home) = homes(&pair);
    let dispatcher = {
        let done = done.clone();
        std::thread::spawn(move || {
            let mut store = Store::open(&b_home).unwrap();
            while !done.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(300));
                dispatch(&mut store, "mail.flush", json!({})).unwrap();
            }
        })
    };
    let retired = retire_as(&pair, "infra", Some("research"));
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    dispatcher.join().unwrap();
    assert_eq!(retired["waits"]["unsubscribed"], 1, "{retired}");
    // It delivered nothing itself, and found the outbox empty.
    assert_eq!(
        retired["waits"]["drain"],
        json!({"delivered": 0, "pending": 0})
    );
    assert_eq!(rows(&pair.a, "far-work"), 0);
    assert!(audited(&mut pair.b, "wait.notice_dropped").is_empty());
}

/// How often a peer refused one message, and its delivery.
fn refusals(store: &Store, id_like: &str) -> (i64, String) {
    store
        .connection()
        .query_row(
            "SELECT delivery_refusals, delivery FROM mail WHERE payload LIKE ?1 ORDER BY seq LIMIT 1",
            [format!("%{id_like}%")],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

#[test]
fn a_wait_notice_refused_three_times_is_dropped_and_later_mail_to_that_peer_still_flows() {
    let mut pair = pair();
    share(&mut pair.a, "far-work", json!(["infra"]));
    // The owner's home no longer hosts apps (retired there, say): it refuses
    // anything addressed to it, every time.
    pair.a
        .connection()
        .execute("DELETE FROM ships WHERE name='apps'", [])
        .unwrap();
    waiter(&mut pair.b, "waiting-1");
    defer_remote(&mut pair.b, "waiting-1", far()).unwrap();
    // Mail behind the refused subscription waits while it is retried.
    ping(&mut pair.b, "after-refusal", "infra", "research");
    assert_eq!(refusals(&pair.b, "wait_subscribe"), (1, "pending".into()));
    assert_eq!(outbox(&pair.b, "research")[0]["delivery"], "pending");
    dispatch(&mut pair.b, "mail.flush", json!({})).unwrap();
    assert_eq!(refusals(&pair.b, "wait_subscribe"), (2, "pending".into()));
    assert!(audited(&mut pair.b, "wait.notice_dropped").is_empty());
    // The third refusal drops it, and the mail behind it goes in that pass.
    dispatch(&mut pair.b, "mail.flush", json!({})).unwrap();
    let sent = outbox(&pair.b, "apps");
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0]["delivery"], "dropped");
    assert_eq!(outbox(&pair.b, "research")[0]["delivery"], "delivered");
    let dropped = audited(&mut pair.b, "wait.notice_dropped");
    assert_eq!(dropped.len(), 1, "{dropped:?}");
    assert_eq!(dropped[0]["notice"], "wait_subscribe");
    assert_eq!(dropped[0]["to"], "apps");
    assert_eq!(dropped[0]["refusals"], 3);
    // It is never retried again, and the peer is not left backed off.
    for _ in 0..2 {
        dispatch(&mut pair.b, "mail.flush", json!({})).unwrap();
    }
    assert_eq!(audited(&mut pair.b, "wait.notice_dropped").len(), 1);
    let failures: i64 = pair
        .b
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM mail_delivery_retry WHERE peer='home-a' AND failures>0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(failures, 0);
    ping(&mut pair.b, "later", "infra", "research");
    assert_eq!(
        outbox(&pair.b, "research").last().unwrap()["delivery"],
        "delivered"
    );
    let listed = dispatch(&mut pair.b, "mail.outbox", json!({"delivery":"dropped"})).unwrap();
    assert_eq!(listed["messages"].as_array().unwrap().len(), 1, "{listed}");
    // Nothing quarantined, so nothing for the captain to chase.
    assert_eq!(
        dispatch(&mut pair.b, "mail.flush", json!({})).unwrap()["quarantined"],
        0
    );
}

#[test]
fn a_refused_report_is_dropped_on_its_third_refusal_and_only_then_the_owner_deletes_its_row() {
    let mut pair = subscribed();
    let nonce = nonce_of(&pair.b, "waiting-1").unwrap();
    // The waiting home no longer knows apps, so it refuses apps's mail.
    pair.b
        .connection()
        .execute("DELETE FROM ships WHERE name='apps'", [])
        .unwrap();
    dispatch(
        &mut pair.a,
        "task.update",
        json!({"id":"far-work","actor":"apps","status":"done"}),
    )
    .unwrap();
    // A replay of the same subscription was stored before the answer came
    // back refused: that generation's row goes with the dropped report.
    pair.a
        .connection()
        .execute(
            "INSERT INTO task_subscriptions(task_id,subscriber,subscriber_task,created_ms,nonce)
             VALUES('far-work','infra','waiting-1',1,?1),('far-work','infra','waiting-2',1,'other')",
            [&nonce],
        )
        .unwrap();
    ping(&mut pair.a, "later", "research", "infra");
    dispatch(&mut pair.a, "mail.flush", json!({})).unwrap();
    // Refused twice: still owed, and the row stays.
    assert_eq!(refusals(&pair.a, "task_end"), (2, "pending".into()));
    assert_eq!(rows(&pair.a, "far-work"), 2);
    dispatch(&mut pair.a, "mail.flush", json!({})).unwrap();
    let ends: Vec<Value> = outbox(&pair.a, "infra")
        .into_iter()
        .filter(|m| m["payload"]["task_end"].is_object())
        .collect();
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0]["delivery"], "dropped");
    assert_eq!(
        outbox(&pair.a, "infra").last().unwrap()["delivery"],
        "delivered"
    );
    let dropped = audited(&mut pair.a, "wait.notice_dropped");
    assert_eq!(dropped[0]["notice"], "task_end");
    assert_eq!(dropped[0]["rows"], 1);
    // Another generation's row is not the refused one's.
    assert_eq!(rows(&pair.a, "far-work"), 1);
}

#[test]
fn a_refusal_that_clears_delivers_the_report_and_the_wait_resumes() {
    let mut pair = subscribed();
    let apps: String = pair
        .b
        .connection()
        .query_row("SELECT record FROM ships WHERE name='apps'", [], |r| {
            r.get(0)
        })
        .unwrap();
    pair.b
        .connection()
        .execute("DELETE FROM ships WHERE name='apps'", [])
        .unwrap();
    dispatch(
        &mut pair.a,
        "task.update",
        json!({"id":"far-work","actor":"apps","status":"done"}),
    )
    .unwrap();
    dispatch(&mut pair.a, "mail.flush", json!({})).unwrap();
    assert_eq!(refusals(&pair.a, "task_end"), (1, "pending".into()));
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "waiting");
    // The waiting home knows apps again: the retry is delivered.
    pair.b
        .connection()
        .execute(
            "INSERT INTO ships(name, record) VALUES('apps', ?1)",
            [&apps],
        )
        .unwrap();
    dispatch(&mut pair.a, "mail.flush", json!({})).unwrap();
    assert_eq!(refusals(&pair.a, "task_end"), (1, "delivered".into()));
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "ready");
    assert!(audited(&mut pair.a, "wait.notice_dropped").is_empty());
}

#[test]
fn a_busy_peer_is_retried_with_backoff_and_never_dropped() {
    let mut pair = pair();
    share(&mut pair.a, "far-work", json!(["infra"]));
    // The owner's home answers every subscription with the error a write
    // past its busy timeout gives.
    pair.a
        .connection()
        .execute_batch(
            "CREATE TRIGGER busy BEFORE INSERT ON task_subscriptions
             BEGIN SELECT RAISE(ABORT, 'database is locked'); END;",
        )
        .unwrap();
    waiter(&mut pair.b, "waiting-1");
    defer_remote(&mut pair.b, "waiting-1", far()).unwrap();
    for _ in 0..4 {
        dispatch(&mut pair.b, "mail.flush", json!({})).unwrap();
    }
    assert_eq!(refusals(&pair.b, "wait_subscribe"), (0, "pending".into()));
    let (attempts, error): (i64, String) = pair
        .b
        .connection()
        .query_row(
            "SELECT delivery_attempts, delivery_error FROM mail WHERE recipient='apps'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(attempts, 4);
    assert!(error.contains("database is locked"), "{error}");
    assert!(audited(&mut pair.b, "wait.notice_dropped").is_empty());
    // Backed off as an unreachable peer is, though the link is heard.
    let (failures, down): (i64, Option<i64>) = pair
        .b
        .connection()
        .query_row(
            "SELECT r.failures, l.down_ms FROM mail_delivery_retry r
             JOIN wait_links l ON l.peer=r.peer WHERE r.peer='home-a'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(failures, 4);
    assert_eq!(down, None);
    // Once the owner's home can write again, the subscription arrives.
    pair.a
        .connection()
        .execute_batch("DROP TRIGGER busy;")
        .unwrap();
    dispatch(&mut pair.b, "mail.flush", json!({})).unwrap();
    assert_eq!(refusals(&pair.b, "wait_subscribe"), (0, "delivered".into()));
    assert_eq!(rows(&pair.a, "far-work"), 1);
}

#[test]
fn a_link_back_resyncs_at_most_once_a_minute_and_stays_owed_one() {
    let mut pair = subscribed();
    outage(&mut pair, "ping-0");
    assert_eq!(audited(&mut pair.b, "wait.resync").len(), 1);
    // Down and back again at once: no second resync yet, but the link is
    // still marked as owing one.
    let (a_home, _) = homes(&pair);
    link(&mut pair.b, "home-a", &a_home, false);
    ping(&mut pair.b, "ping-1", "infra", "apps");
    link(&mut pair.b, "home-a", &a_home, true);
    dispatch(&mut pair.b, "mail.flush", json!({})).unwrap();
    assert_eq!(audited(&mut pair.b, "wait.resync").len(), 1);
    let down: Option<i64> = pair
        .b
        .connection()
        .query_row(
            "SELECT down_ms FROM wait_links WHERE peer='home-a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(down.is_some());
    // The first hearing after the spacing pays it.
    later(&pair.b);
    ping(&mut pair.b, "ping-2", "infra", "apps");
    assert_eq!(audited(&mut pair.b, "wait.resync").len(), 2);
}

#[test]
fn a_failed_resync_still_records_the_link_heard_and_sends_nothing_half_done() {
    let mut pair = subscribed();
    // A second waiting task whose schedule cannot be read: the resync fails
    // on it after it has already resubscribed waiting-1.
    waiter(&mut pair.b, "zz-broken");
    defer_remote(&mut pair.b, "zz-broken", far()).unwrap();
    exchange(&mut pair);
    pair.b
        .connection()
        .execute(
            "UPDATE task_schedule SET record=json_set(record,'$.revision','x') WHERE task_id='zz-broken'",
            [],
        )
        .unwrap();
    let subscribes = wait_payloads(&pair.b, "apps", "wait_subscribe").len();
    outage(&mut pair, "ping-0");
    let failed = audited(&mut pair.b, "wait.resync_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["peer"], "home-a");
    assert!(audited(&mut pair.b, "wait.resync").is_empty());
    assert_eq!(
        wait_payloads(&pair.b, "apps", "wait_subscribe").len(),
        subscribes
    );
    // The link is heard, not left down, so the next delivery does not try
    // the whole resync again.
    let (heard, down): (i64, Option<i64>) = pair
        .b
        .connection()
        .query_row(
            "SELECT heard_ms, down_ms FROM wait_links WHERE peer='home-a'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(heard > 0);
    assert_eq!(down, None);
    ping(&mut pair.b, "ping-1", "infra", "apps");
    assert_eq!(audited(&mut pair.b, "wait.resync_failed").len(), 1);
}

#[test]
fn two_processes_hearing_a_link_come_back_resync_it_once() {
    let mut pair = subscribed();
    let (a_home, b_home) = homes(&pair);
    link(&mut pair.b, "home-a", &a_home, false);
    ping(&mut pair.b, "ping-0", "infra", "apps");
    link(&mut pair.b, "home-a", &a_home, true);
    // Several deliveries from the owner's home arrive at once, each in its
    // own process, and each hears the link that was down.
    let start = std::sync::Arc::new(std::sync::Barrier::new(6));
    let threads: Vec<_> = (0..6)
        .map(|n| {
            let (start, home) = (start.clone(), b_home.clone());
            std::thread::spawn(move || {
                let mut store = Store::open(&home).unwrap();
                start.wait();
                dispatch(
                    &mut store,
                    "mail.deliver",
                    json!({"id":format!("hello-{n}"),"from":"apps","to":"infra",
                        "kind":"notice","body":"hello"}),
                )
                .unwrap();
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let resyncs = audited(&mut pair.b, "wait.resync");
    assert_eq!(resyncs.len(), 1, "{resyncs:?}");
    assert_eq!(wait_payloads(&pair.b, "apps", "wait_subscribe").len(), 2);
}

#[test]
fn a_resync_that_sends_nothing_is_not_audited() {
    let mut pair = pair();
    // The link is known, then down and back, with no wait riding it.
    ping(&mut pair.b, "ping-0", "infra", "apps");
    outage(&mut pair, "ping-1");
    let resynced: Option<i64> = pair
        .b
        .connection()
        .query_row(
            "SELECT resynced_ms FROM wait_links WHERE peer='home-a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(resynced.is_some(), "the resync ran");
    assert!(audited(&mut pair.b, "wait.resync").is_empty());
}

#[test]
fn a_fault_from_before_fault_counts_still_lets_the_next_fault_wake_the_first_mate() {
    let mut pair = pair();
    waiter(&mut pair.b, "waiting-1");
    defer_remote(&mut pair.b, "waiting-1", far()).unwrap();
    exchange(&mut pair);
    assert_eq!(
        fault_notices(&pair.b, "waiting-1"),
        ["scheduling-fault-waiting-1-1"]
    );
    // As a schedule faulted under step 2 stored it: no fault count.
    pair.b
        .connection()
        .execute(
            "UPDATE task_schedule SET record=json_remove(record,'$.faults') WHERE task_id='waiting-1'",
            [],
        )
        .unwrap();
    // The same fault again still sends nothing new.
    report(
        &mut pair.b,
        "closed-again",
        "apps",
        json!({"wait_closed":{"task":"far-work","reason":"refused"}}),
    );
    pass(&mut pair.b);
    assert_eq!(fault_notices(&pair.b, "waiting-1").len(), 1);
    // A different fault in the same revision has its own notice.
    defer_remote(
        &mut pair.b,
        "waiting-1",
        json!({"ship":"research","task":"other-work"}),
    )
    .unwrap();
    assert_eq!(work(&mut pair.b, "waiting-1")["revision"], 1);
    exchange(&mut pair);
    assert_eq!(work(&mut pair.b, "waiting-1")["state"], "fault");
    assert_eq!(
        fault_notices(&pair.b, "waiting-1"),
        [
            "scheduling-fault-waiting-1-1",
            "scheduling-fault-waiting-1-1-2"
        ]
    );
}
