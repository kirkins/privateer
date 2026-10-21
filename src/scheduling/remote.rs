//! The waiting side of a wait on another ship's task (`kind: remote`). See
//! `docs/design/cross-ship-waits.md`. The waiting home stores only the ref;
//! it subscribes through the outbox when the wait becomes live and
//! unsubscribes when the task leaves it, and it learns the far end's state
//! only from a `task_end` or `wait_closed` the owner chose to send.
use super::*;
use crate::store::identifier;

/// The far task a remote wait names. `machine` is None for this machine.
#[derive(Clone, PartialEq)]
struct Far {
    ship: String,
    task: String,
    machine: Option<String>,
}

impl Far {
    fn of(wait: &Value) -> Option<Far> {
        if wait["kind"] != "remote" {
            return None;
        }
        let reference = &wait["ref"];
        Some(Far {
            ship: reference["ship"].as_str()?.to_owned(),
            task: reference["task"].as_str()?.to_owned(),
            machine: reference["machine"].as_str().map(str::to_owned),
        })
    }

    fn name(&self) -> String {
        match &self.machine {
            Some(machine) => format!("task {} of {} on machine {machine}", self.task, self.ship),
            None => format!("task {} of {}", self.task, self.ship),
        }
    }
}

/// The subscription a schedule holds open with an owner: a remote wait that
/// is still `waiting` and that no `task_end` or `wait_closed` has resolved.
fn live(work: &Scheduled) -> Option<Far> {
    let wait = work.wait.as_ref()?;
    (work.state == "waiting" && wait.get("end").is_none_or(Value::is_null))
        .then(|| Far::of(wait))
        .flatten()
}

/// Validate a `remote` wait for `task.defer` and return it as stored: kind,
/// reason and ref alone, so a caller cannot supply a resolution. The far
/// ship must be one this home reaches through a peer, by the route the ref
/// names: no machine for a home on this machine, else that machine's link.
pub(super) fn wait(store: &Store, id: &str, owner: &str, wait: &Value) -> Result<Value> {
    let reference = wait
        .get("ref")
        .and_then(Value::as_object)
        .context("a remote wait needs ref {ship, task, machine?}")?;
    ensure!(
        reference
            .keys()
            .all(|key| matches!(key.as_str(), "ship" | "task" | "machine")),
        "a remote ref holds ship, task and machine only"
    );
    let text = |key: &str| {
        reference
            .get(key)
            .and_then(Value::as_str)
            .with_context(|| format!("a remote ref needs a string {key}"))
    };
    let (ship, task) = (text("ship")?, text("task")?);
    identifier(ship)?;
    crate::mail::unreserved(ship)?;
    identifier(task)?;
    ensure!(
        super::commands::is_id_shape(task),
        "a remote task id is lowercase letters, digits and hyphens"
    );
    let machine = match reference.get("machine") {
        None => None,
        Some(machine) => {
            let machine = machine.as_str().context("ref.machine is a machine name")?;
            identifier(machine)?;
            Some(machine)
        }
    };
    ensure!(
        !(ship == owner && task == id),
        "a task cannot wait on itself"
    );
    let record = crate::mail::ship(store, ship)?
        .with_context(|| format!("unknown ship {ship}; add it with ship.put"))?;
    let Some(peer) = record["location"]["peer"].as_str() else {
        bail!("{ship} is hosted by this home; wait on its task with kind task");
    };
    match machine {
        Some(machine) => ensure!(
            peer == crate::machines::peer_name(machine),
            "{ship} is not reached through machine {machine}"
        ),
        None => ensure!(
            !crate::peers::machine(store, peer)?,
            "{ship} is on another machine; name it in ref.machine"
        ),
    }
    let mut reference = json!({"ship": ship, "task": task});
    if let Some(machine) = machine {
        reference["machine"] = json!(machine);
    }
    Ok(json!({"kind": "remote", "reason": wait["reason"], "ref": reference}))
}

/// Keep the owner's subscription in step with a schedule change, in the
/// caller's transaction. Every schedule write passes here, so a task that
/// leaves a live remote wait by any path (re-deferred elsewhere, enqueued
/// again, parked, held, stopped, faulted, ended or handed to a new owner)
/// queues `wait_unsubscribe` for the old subscription, and a wait that
/// becomes live queues `wait_subscribe` with a fresh nonce, recorded on the
/// schedule. A wait resolved by the owner's own `task_end` or `wait_closed`
/// sends nothing back: the owner has already deleted its row.
pub(super) fn sync(store: &Store, old: Option<&Scheduled>, new: &mut Scheduled) -> Result<()> {
    let before = old.and_then(|old| live(old).map(|far| (old.owner.clone(), far)));
    let after = live(new).map(|far| (new.owner.clone(), far));
    if before == after {
        // The same live subscription: keep its nonce, whichever copy the
        // caller saved from.
        if after.is_none() {
            new.subscription = None;
        } else if new.subscription.is_none() {
            new.subscription = old.and_then(|old| old.subscription.clone());
        }
        return Ok(());
    }
    if let (Some((subscriber, far)), Some(old)) = (&before, old) {
        let resolved = new.owner == *subscriber
            && new.wait.as_ref().is_some_and(|wait| {
                Far::of(wait).as_ref() == Some(far) && wait.get("end").is_some_and(Value::is_object)
            });
        if !resolved {
            notice(
                store,
                subscriber,
                &new.task_id,
                far,
                "wait_unsubscribe",
                old.subscription.as_ref(),
                false,
            )?;
        }
    }
    new.subscription = None;
    if let Some((subscriber, far)) = &after {
        let subscription = Subscription {
            nonce: uuid::Uuid::new_v4().simple().to_string(),
            since_ms: Store::now_ms(),
        };
        notice(
            store,
            subscriber,
            &new.task_id,
            far,
            "wait_subscribe",
            Some(&subscription),
            false,
        )?;
        new.subscription = Some(subscription);
    }
    Ok(())
}

fn notice(
    store: &Store,
    subscriber: &str,
    task: &str,
    far: &Far,
    key: &str,
    subscription: Option<&Subscription>,
    resync: bool,
) -> Result<()> {
    let mut body = json!({"task": far.task, "subscriber": subscriber, "subscriber_task": task});
    if let Some(subscription) = subscription {
        body["nonce"] = json!(subscription.nonce);
    }
    crate::waits::notify(store, subscriber, &far.ship, &json!({key: body}))?;
    let mut audit =
        json!({"task": task, "ship": far.ship, "far_task": far.task, "machine": far.machine});
    if resync {
        audit["resync"] = json!(true);
    }
    store.audit(
        if key == "wait_subscribe" {
            "wait.subscribe_sent"
        } else {
            "wait.unsubscribe_sent"
        },
        &audit,
    )?;
    Ok(())
}

/// Matching schedules are read a page at a time; every page is resolved.
const PAGE: i64 = 100;

/// A link to `peer` came back: send `wait_subscribe` again for every live
/// remote wait of this home whose far ship is behind it, with the nonce it
/// already holds, in the caller's transaction. The owner keeps one row per
/// subscription, so a replay adds nothing, and answers at once for a far
/// task that ended meanwhile. A wait from before nonces gains one here.
/// Returns how many were sent.
pub(crate) fn resubscribe(store: &Store, peer: &str) -> Result<usize> {
    if !available(store)? {
        return Ok(0);
    }
    let mut sent = 0;
    let mut after = String::new();
    loop {
        let mut statement = store.connection().prepare(
            "SELECT s.task_id FROM task_schedule s
             JOIN ships far ON far.name=json_extract(s.record,'$.wait.ref.ship')
             WHERE json_extract(s.record,'$.state')='waiting'
               AND json_extract(s.record,'$.wait.kind')='remote'
               AND json_extract(s.record,'$.wait.end') IS NULL
               AND json_extract(far.record,'$.location.peer')=?1 AND s.task_id>?2
             ORDER BY s.task_id LIMIT ?3",
        )?;
        let ids = statement
            .query_map(params![peer, after, PAGE], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for id in &ids {
            let Some(mut work) = load(store, id)? else {
                continue;
            };
            let Some(far) = live(&work) else {
                continue;
            };
            if work.subscription.is_none() {
                work.subscription = Some(Subscription {
                    nonce: uuid::Uuid::new_v4().simple().to_string(),
                    since_ms: work.created_ms,
                });
                save(store, &work)?;
            }
            notice(
                store,
                &work.owner,
                &work.task_id,
                &far,
                "wait_subscribe",
                work.subscription.as_ref(),
                true,
            )?;
            sent += 1;
        }
        match ids.last() {
            Some(last) if ids.len() as i64 == PAGE => after.clone_from(last),
            _ => return Ok(sent),
        }
    }
}

/// Whether a resolved remote wait lets its task resume. A `task_end` with
/// status `done` does; a `cancelled` one or any `wait_closed` faults the
/// task, so it never resumes as if the dependency succeeded. Unresolved, it
/// keeps waiting: there is no clock to poll.
pub(super) fn ready(store: &Store, work: &mut Scheduled, wait: &Value, now: i64) -> Result<bool> {
    let end = &wait["end"];
    if !end.is_object() {
        return Ok(false);
    }
    let far = Far::of(wait).context("invalid remote wait")?;
    let reason = end["reason"].as_str().unwrap_or("");
    if end["notice"] == "task_end" && end["status"] == "done" {
        work.resumed_by = Some(format!(
            "Remote {} ended done{}",
            far.name(),
            if reason.is_empty() || reason == "done" {
                String::new()
            } else {
                format!(": {reason}")
            }
        ));
        return Ok(true);
    }
    let why = if end["notice"] == "task_end" {
        format!(
            "Remote {} ended {}; re-evaluate the assignment",
            far.name(),
            end["status"].as_str().unwrap_or("unsuccessfully")
        )
    } else if let Some(new_owner) = end["owner"].as_str().filter(|_| reason == "handed_off") {
        format!(
            "Remote wait on {} was closed: handed_off to {new_owner}; re-wait with ref ship {new_owner} if it still applies",
            far.name()
        )
    } else {
        format!(
            "Remote wait on {} was closed: {reason}; re-evaluate the assignment",
            far.name()
        )
    };
    fault(store, work, why, now)?;
    Ok(false)
}

/// Apply a `task_end` or `wait_closed` that arrived for ship `to` from ship
/// `from` (`via` the machine that carried it, if any), before any inbox is
/// written. It is accepted only when the sender is the ship a waiting remote
/// wait of `to` names, the carrying machine is the one that ref names (none
/// for this machine), and, for `task_end`, the status is final. Then every
/// such wait records the resolution and is checked at once. Anything else,
/// including a repeat for a wait already resolved, is dropped and audited.
pub(crate) fn deliver_notice(
    store: &Store,
    id: &str,
    from: &str,
    to: &str,
    via: Option<&str>,
    payload: &Value,
) -> Result<Value> {
    let (key, body) = match (payload.get("task_end"), payload.get("wait_closed")) {
        (Some(body), None) => ("task_end", body),
        (None, Some(body)) => ("wait_closed", body),
        _ => bail!("a notice carries one wait instruction"),
    };
    let task = body
        .get("task")
        .and_then(Value::as_str)
        .context("a wait notice needs a string task")?;
    identifier(task)?;
    let text = |field: &str| -> Result<Option<String>> {
        match body.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => Ok(Some(
                value
                    .as_str()
                    .with_context(|| format!("{key}.{field} must be a string"))?
                    .chars()
                    .take(500)
                    .collect(),
            )),
        }
    };
    let status = text("status")?;
    let reason = text("reason")?;
    let new_owner = text("owner")?;
    if let Some(owner) = &new_owner {
        identifier(owner)?;
    }
    // The subscription the owner answers, when it echoes one. An owner from
    // before nonces sends none, and its answer covers every matching wait.
    let nonce = text("nonce")?;
    if let Some(nonce) = &nonce {
        identifier(nonce)?;
    }
    let tx = store.write_transaction()?;
    let mut named = Vec::new();
    let mut after = String::new();
    loop {
        let mut statement = store.connection().prepare(
            "SELECT task_id FROM task_schedule
             WHERE json_extract(record,'$.owner')=?1 AND json_extract(record,'$.state')='waiting'
               AND json_extract(record,'$.wait.kind')='remote'
               AND json_extract(record,'$.wait.ref.ship')=?2 AND json_extract(record,'$.wait.ref.task')=?3
               AND json_extract(record,'$.wait.end') IS NULL AND task_id>?4
             ORDER BY task_id LIMIT ?5",
        )?;
        let ids = statement
            .query_map(params![to, from, task, after, PAGE], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for id in &ids {
            if let Some(work) = load(store, id)?
                && live(&work).is_some()
            {
                named.push(work);
            }
        }
        match ids.last() {
            Some(last) if ids.len() as i64 == PAGE => after.clone_from(last),
            _ => break,
        }
    }
    let routed: Vec<&Scheduled> = named
        .iter()
        .filter(|work| {
            Far::of(work.wait.as_ref().unwrap()).and_then(|far| far.machine)
                == via.map(str::to_owned)
        })
        .collect();
    // A wait holding no nonce predates them and takes any answer.
    let carried: Vec<Scheduled> = routed
        .iter()
        .filter(|work| match (&nonce, &work.subscription) {
            (Some(nonce), Some(subscription)) => subscription.nonce == *nonce,
            _ => true,
        })
        .map(|work| (*work).clone())
        .collect();
    let dropped = if key == "task_end" && !matches!(status.as_deref(), Some("done" | "cancelled")) {
        Some("status")
    } else if named.is_empty() {
        Some("no_wait")
    } else if routed.is_empty() {
        Some("via")
    } else if carried.is_empty() {
        Some("nonce")
    } else {
        None
    };
    if let Some(why) = dropped {
        store.audit(
            "wait.dropped",
            &json!({"id": id, "notice": key, "from": from, "to": to, "via": via,
                "task": task, "nonce": nonce, "why": why}),
        )?;
        tx.commit()?;
        return Ok(json!({"id": id, "wait": {"task": task, "state": "dropped", "why": why}}));
    }
    let now = Store::now_ms();
    let mut end = json!({"notice": key, "reason": reason, "ms": now});
    if key == "task_end" {
        end["status"] = json!(status);
    } else if let Some(owner) = &new_owner {
        end["owner"] = json!(owner);
    }
    let mut resolved = Vec::new();
    for mut work in carried {
        if let Some(wait) = work.wait.as_mut() {
            wait["end"] = end.clone();
        }
        save(store, &work)?;
        check(store, &mut work, now)?;
        save(store, &work)?;
        resolved.push(json!({"task": work.task_id, "state": work.state}));
    }
    store.audit(
        "wait.resolved",
        &json!({"id": id, "notice": key, "from": from, "to": to, "via": via, "task": task,
            "status": status, "reason": reason, "tasks": resolved}),
    )?;
    tx.commit()?;
    Ok(json!({"id": id, "wait": {"task": task, "state": "applied", "tasks": resolved}}))
}

/// Settle a schedule whose task changed under it (cancelled, finished or
/// handed to a new owner) in the caller's transaction, when it holds a live
/// remote subscription, so the unsubscribe goes now rather than on the next
/// dispatcher pass. Other schedules keep settling on that pass.
pub(crate) fn settle(store: &Store, task: &str) -> Result<()> {
    if !available(store)? {
        return Ok(());
    }
    let Some(mut work) = load(store, task)? else {
        return Ok(());
    };
    if live(&work).is_none() {
        return Ok(());
    }
    check(store, &mut work, Store::now_ms())?;
    save(store, &work)
}

/// How long a remote wait may go without a `task_end` before it is shown as
/// stale, in seconds, unless the waiting ship's profile says otherwise.
pub(crate) const STALE_SECS: u64 = 3_600;

/// The SQL condition, over a schedule `s`, its task `t` and its owning ship
/// `sh`, under which a live remote wait is stale at time `now` (a bound
/// parameter): it has waited past its ship's `remote_wait_stale_secs`, which
/// is 0 to show none. A wait from before subscriptions carried their start
/// counts from its schedule's creation.
pub(crate) fn stale_where(now: &str) -> String {
    let secs = format!(
        "COALESCE(json_extract(sh.record,'$.first_mate.remote_wait_stale_secs'),{STALE_SECS})"
    );
    format!(
        "json_extract(s.record,'$.state')='waiting' AND json_extract(s.record,'$.wait.kind')='remote'
         AND json_extract(s.record,'$.wait.end') IS NULL AND t.status NOT IN ('done','cancelled')
         AND {secs}>0 AND COALESCE(json_extract(s.record,'$.subscription.since_ms'),
           json_extract(s.record,'$.created_ms'))+{secs}*1000<={now}"
    )
}

/// What the captain and the first mate are told about a stale remote wait:
/// the far task, ship and machine, how long it has waited with no
/// `task_end`, and when the link it rides was last heard. Read-only. It
/// reports and never resolves: the wait stays `waiting`.
pub(crate) fn stale_item(store: &Store, id: &str, now: i64) -> Result<Value> {
    let work = load(store, id)?.context("remote wait schedule missing")?;
    let far = live(&work).context("schedule holds no live remote wait")?;
    let since = work
        .subscription
        .as_ref()
        .map_or(work.created_ms, |s| s.since_ms);
    let peer = crate::mail::ship(store, &far.ship)?
        .and_then(|ship| ship["location"]["peer"].as_str().map(str::to_owned));
    let heard = match &peer {
        Some(peer) => crate::waits::last_heard(store, peer)?,
        None => None,
    };
    let minutes = |ms: i64| now.saturating_sub(ms).max(0) / 60_000;
    let link = match (&peer, heard) {
        (Some(peer), Some(at)) => format!("the link {peer} was last heard {} min ago", minutes(at)),
        (Some(peer), None) => {
            format!("the link {peer} has not been heard since this home began tracking it")
        }
        (None, _) => format!("{} is no longer reached through a peer", far.ship),
    };
    let summary = format!(
        "Waiting {} min on remote {} with no task_end; {link}. The wait stays waiting: it resumes or faults only on the owner's report. Add a time wait for a deadline.",
        minutes(since),
        far.name()
    );
    Ok(
        json!({"ship": work.owner, "task_id": work.task_id, "status": "waiting",
        "summary": summary, "created_ms": since, "waited_ms": now.saturating_sub(since),
        "far": {"ship": far.ship, "task": far.task, "machine": far.machine},
        "link": {"peer": peer, "heard_ms": heard},
        "revision": work.revision,
        "version": crate::core::hash(&json!([work.revision, since,
            work.subscription.as_ref().map(|s| &s.nonce)]))}),
    )
}
