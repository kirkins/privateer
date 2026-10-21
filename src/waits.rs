//! The owner side of waits on another ship's task: the ships a task is
//! shared with, the subscriptions they hold, and the runtime notices that
//! answer them. See `docs/design/cross-ship-waits.md`. The waiting side (the
//! `remote` wait, notice verification and resume) is in
//! `scheduling::remote`. These notices are runtime-to-runtime: `mail.deliver`
//! applies them without writing any inbox, and `mail.send` refuses their
//! payload keys, so only the runtime authors them.
use crate::store::{Store, identifier};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

/// A task's audience stays small: `shared_with` is one task's list, not a
/// fleet directory.
const MAX_SHARED: usize = 100;

/// A link heard again after this long a silence counts as come back, for a
/// peer that only ever calls this home (a machine that collects from this
/// one): a healthy one calls every `CONTACT_MS`.
const RESYNC_GAP_MS: i64 = 4 * crate::machines::CONTACT_MS;
/// A link heard again within this long is not recorded again.
const HEARD_EVERY_MS: i64 = 10_000;

pub(crate) fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS task_subscriptions(
           task_id TEXT NOT NULL REFERENCES tasks(id),
           subscriber TEXT NOT NULL,
           subscriber_task TEXT NOT NULL,
           created_ms INTEGER NOT NULL,
           PRIMARY KEY(subscriber,subscriber_task,task_id));
         CREATE TABLE IF NOT EXISTS wait_links(peer TEXT PRIMARY KEY, heard_ms INTEGER NOT NULL,
           down_ms INTEGER, resynced_ms INTEGER);",
    )?;
    // The nonce a subscriber sent, echoed in every answer; rows from before
    // nonces have none.
    let tx = store.write_transaction()?;
    let has_nonce: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('task_subscriptions') WHERE name='nonce')",
        [],
        |r| r.get(0),
    )?;
    if !has_nonce {
        tx.execute_batch("ALTER TABLE task_subscriptions ADD COLUMN nonce TEXT;")?;
    }
    tx.commit()?;
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
pub(crate) fn notify(store: &Store, from: &str, to: &str, payload: &Value) -> Result<()> {
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

/// The `{task, subscriber, subscriber_task, nonce?}` a wait notice names. A
/// subscriber from before nonces sends none.
struct WaitRef<'a> {
    task: &'a str,
    subscriber: &'a str,
    subscriber_task: &'a str,
    nonce: Option<&'a str>,
}

fn wait_ref(wait: &Value) -> Result<WaitRef<'_>> {
    let text = |key: &str| {
        wait.get(key)
            .and_then(Value::as_str)
            .with_context(|| format!("wait notice needs a string {key}"))
    };
    let nonce = match wait.get("nonce") {
        None | Some(Value::Null) => None,
        Some(nonce) => Some(
            nonce
                .as_str()
                .context("wait notice nonce must be a string")?,
        ),
    };
    let reference = WaitRef {
        task: text("task")?,
        subscriber: text("subscriber")?,
        subscriber_task: text("subscriber_task")?,
        nonce,
    };
    identifier(reference.task)?;
    identifier(reference.subscriber)?;
    identifier(reference.subscriber_task)?;
    if let Some(nonce) = reference.nonce {
        identifier(nonce)?;
    }
    Ok(reference)
}

/// An answer about `task`: its payload body, with the subscription's nonce
/// echoed when it had one.
fn answer(task: &str, nonce: Option<&str>, mut body: Value) -> Value {
    body["task"] = json!(task);
    if let Some(nonce) = nonce {
        body["nonce"] = json!(nonce);
    }
    body
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
    // An unsubscribe for an older generation leaves a newer one in place.
    let removed = tx.execute(
        "DELETE FROM task_subscriptions WHERE task_id=?1 AND subscriber=?2 AND subscriber_task=?3
           AND (?4 IS NULL OR nonce IS NULL OR nonce=?4)",
        params![
            reference.task,
            reference.subscriber,
            reference.subscriber_task,
            reference.nonce
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
            &json!({"wait_closed": answer(reference.task, reference.nonce, json!({"reason": "refused"}))}),
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
            &json!({"task_end": answer(reference.task, reference.nonce,
                json!({"status": status, "reason": status}))}),
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
        "INSERT OR IGNORE INTO task_subscriptions(task_id,subscriber,subscriber_task,created_ms,nonce)
         VALUES(?1,?2,?3,?4,?5)",
        params![
            reference.task,
            reference.subscriber,
            reference.subscriber_task,
            Store::now_ms(),
            reference.nonce
        ],
    )?;
    // A new generation of the same subscription keeps the row and takes its
    // nonce; a replay of the same one changes nothing.
    if stored == 0 && reference.nonce.is_some() {
        tx.execute(
            "UPDATE task_subscriptions SET nonce=?4
             WHERE task_id=?1 AND subscriber=?2 AND subscriber_task=?3 AND nonce IS NOT ?4",
            params![
                reference.task,
                reference.subscriber,
                reference.subscriber_task,
                reference.nonce
            ],
        )?;
    }
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
    let generations = generations(store, task, None)?;
    if generations.is_empty() {
        return Ok(());
    }
    let record = store.task(task)?;
    let owner = record["owner"]
        .as_str()
        .context("task owner missing")?
        .to_owned();
    for (subscriber, nonce) in &generations {
        notify(
            store,
            &owner,
            subscriber,
            &json!({"task_end": answer(task, nonce.as_deref(),
                json!({"status": status, "reason": status}))}),
        )?;
    }
    let mut subscribers: Vec<&String> = generations.iter().map(|(ship, _)| ship).collect();
    subscribers.dedup();
    store
        .connection()
        .execute("DELETE FROM task_subscriptions WHERE task_id=?1", [task])?;
    store.audit(
        "wait.ended",
        &json!({"task":task,"status":status,"subscribers":subscribers.len()}),
    )?;
    Ok(())
}

/// Close the subscriptions on `task` held by the ships `closing` selects:
/// each such subscriber ship gets one `wait_closed` from `owner` with
/// `closed` as its payload, and its rows go. Runs in the caller's
/// transaction. Returns the ships told.
fn close(
    store: &Store,
    task: &str,
    owner: &str,
    closing: impl Fn(&str) -> bool,
    closed: &Value,
) -> Result<Vec<String>> {
    let generations: Vec<_> = generations(store, task, None)?
        .into_iter()
        .filter(|(ship, _)| closing(ship))
        .collect();
    let mut subscribers = Vec::new();
    for (subscriber, nonce) in &generations {
        notify(
            store,
            owner,
            subscriber,
            &json!({"wait_closed": answer(task, nonce.as_deref(), closed.clone())}),
        )?;
        store.connection().execute(
            "DELETE FROM task_subscriptions WHERE task_id=?1 AND subscriber=?2",
            params![task, subscriber],
        )?;
        if !subscribers.contains(subscriber) {
            subscribers.push(subscriber.clone());
        }
    }
    Ok(subscribers)
}

/// The distinct (subscriber, nonce) generations subscribed to `task`, or only
/// `subscriber`'s: each gets its own answer, so a nonce resolves exactly its
/// own wait. Rows from before nonces share one answer per ship, as before.
fn generations(
    store: &Store,
    task: &str,
    subscriber: Option<&str>,
) -> Result<Vec<(String, Option<String>)>> {
    let mut statement = store.connection().prepare(
        "SELECT DISTINCT subscriber, nonce FROM task_subscriptions
         WHERE task_id=?1 AND (?2 IS NULL OR subscriber=?2) ORDER BY subscriber, nonce",
    )?;
    let rows = statement
        .query_map(params![task, subscriber], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The owner handed its task to another ship: a subscriber's ref names the
/// old owner, which will never report this task's end, so each subscriber
/// is told `wait_closed` `handed_off`, naming the new owner, and the rows go.
/// Runs in `task.accept`'s transaction.
pub(crate) fn handed_off(store: &Store, task: &str, owner: &str, new_owner: &str) -> Result<()> {
    let told = close(
        store,
        task,
        owner,
        |_| true,
        &json!({"task": task, "reason": "handed_off", "owner": new_owner}),
    )?;
    if !told.is_empty() {
        store.audit(
            "wait.handed_off",
            &json!({"task":task,"owner":owner,"new_owner":new_owner,"subscribers":told}),
        )?;
    }
    Ok(())
}

/// The owner narrowed a task's `shared_with`: a subscriber no longer on the
/// list loses its subscription with the same `wait_closed` `refused` a new
/// subscribe would get. Runs in `task.update`'s transaction.
pub(crate) fn unshared(store: &Store, task: &str, owner: &str, shared: &Value) -> Result<()> {
    let allowed = |ship: &str| {
        shared
            .as_array()
            .is_some_and(|ships| ships.iter().any(|s| s == ship))
    };
    let told = close(
        store,
        task,
        owner,
        |ship| !allowed(ship),
        &json!({"task": task, "reason": "refused"}),
    )?;
    if !told.is_empty() {
        store.audit(
            "wait.unshared",
            &json!({"task":task,"owner":owner,"subscribers":told}),
        )?;
    }
    Ok(())
}

/// When `peer` was last heard from, if this home has recorded it. Read-only;
/// a home from before link tracking has heard nothing.
pub(crate) fn last_heard(store: &Store, peer: &str) -> Result<Option<i64>> {
    let tracked: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='wait_links')",
        [],
        |r| r.get(0),
    )?;
    if !tracked {
        return Ok(None);
    }
    Ok(store
        .connection()
        .query_row(
            "SELECT heard_ms FROM wait_links WHERE peer=?1",
            [peer],
            |r| r.get(0),
        )
        .optional()?)
}

/// A call to `peer` could not reach it: the link is down from now, unless it
/// already was. Its own transaction.
pub(crate) fn unheard(store: &Store, peer: &str) -> Result<()> {
    let tx = store.write_transaction()?;
    tx.execute(
        "INSERT INTO wait_links(peer, heard_ms, down_ms) VALUES(?1, 0, ?2)
         ON CONFLICT(peer) DO UPDATE SET down_ms=COALESCE(down_ms, excluded.down_ms)",
        params![peer, Store::now_ms()],
    )?;
    tx.commit()?;
    Ok(())
}

/// `peer` answered a call or called this home. When the link was down (a call
/// to it failed since it was last heard) or, for a peer that only calls this
/// home, silent past `RESYNC_GAP_MS`, it has come back, and the waits riding
/// it are resynced. The first time a peer is heard only records it. Its own
/// transaction; returns the resync, if one ran.
pub(crate) fn heard(store: &Store, peer: &str) -> Result<Option<Value>> {
    link(store, peer, false)
}

/// A machine link was contacted again after failing: resync whatever this
/// home's own calls saw.
pub(crate) fn link_back(store: &Store, peer: &str) -> Result<Option<Value>> {
    link(store, peer, true)
}

fn link(store: &Store, peer: &str, back: bool) -> Result<Option<Value>> {
    let now = Store::now_ms();
    let row: Option<(i64, Option<i64>)> = store
        .connection()
        .query_row(
            "SELECT heard_ms, down_ms FROM wait_links WHERE peer=?1",
            [peer],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let collected =
        || -> Result<bool> { Ok(crate::peers::collected(store)?.iter().any(|p| p == peer)) };
    let back = back
        || match row {
            None => false,
            Some((_, Some(_))) => true,
            Some((heard, None)) => now - heard > RESYNC_GAP_MS && collected()?,
        };
    if !back && row.is_some_and(|(heard, _)| now - heard < HEARD_EVERY_MS) {
        return Ok(None);
    }
    let tx = store.write_transaction()?;
    tx.execute(
        "INSERT INTO wait_links(peer, heard_ms) VALUES(?1, ?2)
         ON CONFLICT(peer) DO UPDATE SET heard_ms=excluded.heard_ms, down_ms=NULL",
        params![peer, now],
    )?;
    let resync = if back {
        tx.execute(
            "UPDATE wait_links SET resynced_ms=?2 WHERE peer=?1",
            params![peer, now],
        )?;
        Some(resync(store, peer)?)
    } else {
        None
    };
    tx.commit()?;
    Ok(resync)
}

/// Resync the waits that ride `peer`, in the caller's transaction. The
/// waiting side sends `wait_subscribe` again for each live remote wait on a
/// ship behind it, and an owner answers one for a far task that already
/// ended with its final status. The owning side checks each subscription a
/// ship behind it holds here: one whose task ended without its `task_end`
/// leaving gets it now, and one no longer shared is closed `refused`. Every
/// step is idempotent, so a replay is harmless.
pub(crate) fn resync(store: &Store, peer: &str) -> Result<Value> {
    let resubscribed = crate::scheduling::resubscribe(store, peer)?;
    let mut statement = store.connection().prepare(
        "SELECT ts.task_id, ts.subscriber, ts.subscriber_task, ts.nonce
         FROM task_subscriptions ts JOIN ships s ON s.name=ts.subscriber
         WHERE json_extract(s.record,'$.location.peer')=?1
         ORDER BY ts.task_id, ts.subscriber, ts.subscriber_task",
    )?;
    let rows = statement
        .query_map([peer], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    let mut answered = 0;
    for (task, subscriber, subscriber_task, nonce) in rows {
        let record = store.task(&task)?;
        let owner = record["owner"].as_str().context("task owner missing")?;
        let ended = record["status"]
            .as_str()
            .filter(|s| matches!(*s, "done" | "cancelled"));
        let shared = record["shared_with"]
            .as_array()
            .is_some_and(|ships| ships.iter().any(|ship| ship == subscriber.as_str()));
        let payload = match ended {
            Some(status) => json!({"task_end": answer(&task, nonce.as_deref(),
                json!({"status": status, "reason": status}))}),
            None if !shared => {
                json!({"wait_closed": answer(&task, nonce.as_deref(), json!({"reason": "refused"}))})
            }
            None => continue,
        };
        notify(store, owner, &subscriber, &payload)?;
        store.connection().execute(
            "DELETE FROM task_subscriptions WHERE task_id=?1 AND subscriber=?2 AND subscriber_task=?3",
            params![task, subscriber, subscriber_task],
        )?;
        answered += 1;
    }
    let outcome = json!({"peer": peer, "resubscribed": resubscribed, "answered": answered});
    store.audit("wait.resync", &outcome)?;
    Ok(outcome)
}
