//! The owner side of waits on another ship's task: the ships a task is
//! shared with, the subscriptions they hold, and the runtime notices that
//! answer them. See `docs/design/cross-ship-waits.md`. The waiting side of
//! that design (the `remote` wait, notice verification and resume) is a later
//! step and is not here. These notices are runtime-to-runtime: `mail.deliver`
//! applies them without writing any inbox, and `mail.send` refuses their
//! payload keys, so only the runtime authors them.
use crate::store::{Store, identifier};
use anyhow::{Context, Result, ensure};
use rusqlite::params;
use serde_json::{Value, json};

/// A task's audience stays small: `shared_with` is one task's list, not a
/// fleet directory.
const MAX_SHARED: usize = 100;

pub(crate) fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS task_subscriptions(
           task_id TEXT NOT NULL REFERENCES tasks(id),
           subscriber TEXT NOT NULL,
           subscriber_task TEXT NOT NULL,
           created_ms INTEGER NOT NULL,
           PRIMARY KEY(subscriber,subscriber_task,task_id));",
    )?;
    Ok(())
}

/// The ships a task is shared with, as `task.update` records them: a list of
/// ship names under the rule `ship.put` enforces, with duplicates removed.
pub(crate) fn shared_names(value: &Value) -> Result<Value> {
    let names = value
        .as_array()
        .context("shared_with is a list of ship names")?;
    ensure!(
        names.len() <= MAX_SHARED,
        "shared_with is limited to {MAX_SHARED} ships"
    );
    let mut list: Vec<&str> = Vec::new();
    for name in names {
        let name = name.as_str().context("shared_with holds ship names")?;
        identifier(name)?;
        crate::mail::unreserved(name)?;
        if !list.contains(&name) {
            list.push(name);
        }
    }
    Ok(json!(list))
}

/// Queue one runtime notice from a ship of this home to a ship behind a
/// peer, through the existing outbox. Runs in the caller's transaction.
fn notify(store: &Store, from: &str, to: &str, payload: &Value) -> Result<()> {
    crate::mail::record_runtime(
        store,
        &json!({
            "id": crate::mail::outcome_id(store, "wait")?,
            "from": from,
            "to": to,
            "kind": "notice",
            "urgency": "low",
            "body": "",
            "payload": payload,
        }),
    )?;
    Ok(())
}

/// The `{task, subscriber, subscriber_task}` a wait notice names.
struct WaitRef<'a> {
    task: &'a str,
    subscriber: &'a str,
    subscriber_task: &'a str,
}

fn wait_ref(wait: &Value) -> Result<WaitRef<'_>> {
    let text = |key: &str| {
        wait.get(key)
            .and_then(Value::as_str)
            .with_context(|| format!("wait notice needs a string {key}"))
    };
    let reference = WaitRef {
        task: text("task")?,
        subscriber: text("subscriber")?,
        subscriber_task: text("subscriber_task")?,
    };
    identifier(reference.task)?;
    identifier(reference.subscriber)?;
    identifier(reference.subscriber_task)?;
    Ok(reference)
}

/// Apply an incoming runtime wait notice from a ship behind a peer,
/// addressed to a ship hosted here: `wait_subscribe` records one
/// subscription, `wait_unsubscribe` removes one. Nothing is written to any
/// inbox, so nothing here wakes a first mate. Both are idempotent: a
/// redelivered subscribe adds no second row, and an unsubscribe for a row
/// that is not there is silent.
pub(crate) fn deliver_notice(
    store: &Store,
    id: &str,
    from: &str,
    to: &str,
    payload: &Value,
) -> Result<Value> {
    let wait = if let Some(wait) = payload.get("wait_subscribe") {
        subscribe(store, from, to, wait)
    } else {
        unsubscribe(store, from, &payload["wait_unsubscribe"])
    }?;
    Ok(json!({"id": id, "wait": wait}))
}

fn unsubscribe(store: &Store, from: &str, wait: &Value) -> Result<Value> {
    let reference = wait_ref(wait)?;
    // Only the subscribing ship's own home retracts its subscription; a
    // notice from any other ship is ignored, and reveals nothing.
    if from != reference.subscriber {
        return Ok(json!({"task": reference.task, "state": "ignored"}));
    }
    let tx = store.write_transaction()?;
    let removed = tx.execute(
        "DELETE FROM task_subscriptions WHERE task_id=?1 AND subscriber=?2 AND subscriber_task=?3",
        params![
            reference.task,
            reference.subscriber,
            reference.subscriber_task
        ],
    )?;
    if removed > 0 {
        store.audit(
            "wait.unsubscribe",
            &json!({"task":reference.task,"subscriber":reference.subscriber,
                "subscriber_task":reference.subscriber_task}),
        )?;
    }
    tx.commit()?;
    Ok(json!({"task": reference.task, "state": "unsubscribed"}))
}

fn subscribe(store: &Store, from: &str, to: &str, wait: &Value) -> Result<Value> {
    let reference = wait_ref(wait)?;
    // Check, answer and store under one write lock: a task ending between the
    // visibility check and the insert would otherwise leave a row that no
    // `task_end` ever clears.
    let tx = store.write_transaction()?;
    // Accept a subscribe only for a task this home holds, owned by the ship
    // the notice was addressed to, and shared with the sender, who must be
    // the subscriber the notice names. Every other case gets the same
    // refusal, so a missing task is indistinguishable from an unshared one.
    let task = store.task(reference.task).ok();
    let accepted = from == reference.subscriber
        && task.as_ref().is_some_and(|task| {
            task["owner"] == json!(to)
                && task["shared_with"]
                    .as_array()
                    .is_some_and(|ships| ships.iter().any(|ship| ship == from))
        });
    if !accepted {
        notify(
            store,
            to,
            from,
            &json!({"wait_closed": {"task": reference.task, "reason": "refused"}}),
        )?;
        store.audit(
            "wait.refused",
            &json!({"task":reference.task,"subscriber":reference.subscriber,
                "subscriber_task":reference.subscriber_task}),
        )?;
        tx.commit()?;
        return Ok(json!({"task": reference.task, "state": "refused"}));
    }
    let task = task.context("accepted task vanished")?;
    if let Some(status) = task["status"]
        .as_str()
        .filter(|s| matches!(*s, "done" | "cancelled"))
    {
        // A task that already ended answers at once with its final status,
        // and stores no row: a late defer must not wait forever.
        notify(
            store,
            to,
            from,
            &json!({"task_end": {"task": reference.task, "status": status, "reason": status}}),
        )?;
        store.audit(
            "wait.answered",
            &json!({"task":reference.task,"subscriber":reference.subscriber,
                "subscriber_task":reference.subscriber_task,"status":status}),
        )?;
        tx.commit()?;
        return Ok(json!({"task": reference.task, "state": "ended"}));
    }
    let stored = tx.execute(
        "INSERT OR IGNORE INTO task_subscriptions(task_id,subscriber,subscriber_task,created_ms)
         VALUES(?1,?2,?3,?4)",
        params![
            reference.task,
            reference.subscriber,
            reference.subscriber_task,
            Store::now_ms()
        ],
    )?;
    if stored == 1 {
        store.audit(
            "wait.subscribe",
            &json!({"task":reference.task,"subscriber":reference.subscriber,
                "subscriber_task":reference.subscriber_task}),
        )?;
    }
    tx.commit()?;
    Ok(json!({"task": reference.task, "state": "subscribed", "duplicate": stored == 0}))
}

/// A task reached a final status (`done` or `cancelled`): tell each
/// subscriber once, as a runtime notice through the outbox, and delete the
/// subscription rows in the same transaction, so none outlive the task.
/// Runs inside `task.update`'s transaction, like `mail::close_task`. The
/// notice carries the status alone, never notes, brief or logs.
pub(crate) fn task_ended(store: &Store, task: &str, status: &str) -> Result<()> {
    let mut statement = store
        .connection()
        .prepare("SELECT DISTINCT subscriber FROM task_subscriptions WHERE task_id=?1")?;
    let subscribers = statement
        .query_map([task], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    if subscribers.is_empty() {
        return Ok(());
    }
    let record = store.task(task)?;
    let owner = record["owner"]
        .as_str()
        .context("task owner missing")?
        .to_owned();
    let payload = json!({"task_end": {"task": task, "status": status, "reason": status}});
    for subscriber in &subscribers {
        notify(store, &owner, subscriber, &payload)?;
    }
    store
        .connection()
        .execute("DELETE FROM task_subscriptions WHERE task_id=?1", [task])?;
    store.audit(
        "wait.ended",
        &json!({"task":task,"status":status,"subscribers":subscribers.len()}),
    )?;
    Ok(())
}
