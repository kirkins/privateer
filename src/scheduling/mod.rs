//! Prepared work, durable waits and bounded delivery independent of mailbox traffic.
//! Legacy tasks have no schedule. A claim is a delivery/execution record, never
//! evidence that a payload stopped; uncertain execution becomes a visible fault.
use crate::store::{Caller, Store, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const BRIEF_BYTES: usize = 16_384;
const CLAIM_MS: i64 = 120_000;
const ADAPTER_MS: i64 = 60_000;
/// Adapters report every five seconds; a longer silence breaks an idle spell.
const IDLE_GAP_MS: i64 = 30_000;
const MAX_ATTEMPTS: u32 = 3;
const ACTIVE: &str = "'starting','queued','launching','running','orphaned'";

#[derive(Clone, Serialize, Deserialize)]
struct Scheduled {
    task_id: String,
    owner: String,
    revision: u64,
    brief: String,
    state: String,
    wait: Option<Value>,
    not_before_ms: i64,
    offer_id: Option<String>,
    holder: Option<String>,
    job_id: Option<String>,
    profile: Option<Value>,
    charter_hash: Option<String>,
    attempts: u32,
    created_ms: i64,
    last_checked_ms: i64,
    lease_until_ms: i64,
    reason: Option<String>,
    /// Why the runtime last resumed this work, when an event said so: a
    /// remote task's final status. Shown in the next offer's prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resumed_by: Option<String>,
    /// What made this work ready again (its wait resolving, its children
    /// finishing, a retry or an enqueue), for the next offer's wake record.
    /// Taken by the claim; None means it was enqueued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    woke_by: Option<Value>,
    /// The subscription a live remote wait holds with the far owner: its
    /// nonce, carried in `wait_subscribe` and echoed in the owner's answers,
    /// and when it began, for staleness. None for every other wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subscription: Option<Subscription>,
    /// Faults recorded in this revision, so each one has its own notice.
    #[serde(default, skip_serializing_if = "is_zero")]
    faults: u32,
    /// When this work last became ready: enqueued or reopened. A hosted
    /// session's adapter has its grace from then, not from the enqueue. Zero
    /// in a record from before it was kept.
    #[serde(default)]
    ready_ms: i64,
}

/// One generation of a remote wait's subscription. A wait that is re-deferred
/// onto the same far task gets a new nonce, so a late answer to the old one
/// cannot resolve the new one.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct Subscription {
    nonce: String,
    since_ms: i64,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

mod backlog;
mod commands;
mod delivery;
mod observation;
mod remote;
pub(crate) use backlog::{
    DEFAULT_SECS as BACKLOG_ALARM_SECS, bearings_line as backlog_line, tick as backlog_tick,
};
pub(crate) use commands::ACCEPTED;
pub use commands::dispatch;
pub(crate) use delivery::tick;
pub(crate) use observation::{fault_item, snapshot};
pub(crate) use remote::{
    STALE_SECS as REMOTE_WAIT_STALE_SECS, arrived, deliver_notice as remote_notice, resubscribe,
    settle, stale_item as remote_wait_item, stale_where as remote_wait_stale_where,
    unsubscribe_all,
};

pub(crate) fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS task_schedule(task_id TEXT PRIMARY KEY REFERENCES tasks(id),record TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS work_sessions(ship TEXT PRIMARY KEY,holder TEXT NOT NULL,idle INTEGER NOT NULL,checked_ms INTEGER NOT NULL);",
    )?;
    let tx = store.write_transaction()?;
    let has_idle_since: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('work_sessions') WHERE name='idle_since_ms')",
        [],
        |r| r.get(0),
    )?;
    if !has_idle_since {
        tx.execute_batch("ALTER TABLE work_sessions ADD COLUMN idle_since_ms INTEGER;")?;
    }
    tx.commit()?;
    backlog::init(store)
}

/// Whether a ship has scheduled work ready for its first mate now. A task
/// whose crew still runs is not offered, so it wakes and holds no one.
pub(crate) fn has_ready(store: &Store, ship: &str) -> Result<bool> {
    for id in delivery::ready_ids(store, ship)? {
        if !live_jobs(store, &id)? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn available(store: &Store) -> Result<bool> {
    Ok(store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='task_schedule')",
        [],
        |r| r.get(0),
    )?)
}

fn load(store: &Store, task: &str) -> Result<Option<Scheduled>> {
    let raw: Option<String> = store.connection().query_row(
        "SELECT record FROM task_schedule WHERE task_id=?1 AND length(CAST(record AS BLOB))<=65536", [task], |r| r.get(0)).optional()?;
    raw.map(|raw| serde_json::from_str(&raw).context("invalid task schedule"))
        .transpose()
}

fn save(store: &Store, work: &Scheduled) -> Result<()> {
    // The stored subscription follows the wait, whatever copy the caller holds.
    let mut work = work.clone();
    remote::sync(store, load(store, &work.task_id)?.as_ref(), &mut work)?;
    let raw = serde_json::to_string(&work)?;
    ensure!(raw.len() <= 65_536, "task schedule exceeds byte bound");
    store.connection().execute(
        "INSERT INTO task_schedule(task_id,record) VALUES(?1,?2) ON CONFLICT(task_id) DO UPDATE SET record=excluded.record",
        params![work.task_id, raw])?;
    Ok(())
}

fn owner(store: &Store, task: &str) -> Result<Value> {
    let record = store.task(task)?;
    if let Caller::Agent { actor } = store.caller() {
        ensure!(
            record["owner"] == *actor,
            "only the task owner can schedule it"
        );
    }
    store.authorize(task, "spawn")?;
    Ok(record)
}

fn live_jobs(store: &Store, task: &str) -> Result<bool> {
    Ok(store.connection().query_row(&format!(
        "WITH RECURSIVE tree(id) AS (SELECT ?1 UNION SELECT t.id FROM tasks t JOIN tree ON t.parent=tree.id)
         SELECT EXISTS(SELECT 1 FROM tree JOIN jobs j ON json_extract(j.record,'$.task_id')=tree.id
         WHERE json_extract(j.record,'$.status') IN ({ACTIVE}))"), [task], |r| r.get(0))?)
}

fn finish_wake(store: &Store, work: &Scheduled, status: &str, now: i64) -> Result<()> {
    if let Some(id) = &work.offer_id {
        store.connection().execute(
            "UPDATE wakes SET status=?2,finished_ms=?3,error=?4 WHERE id=?1 AND reason='work' AND status IN ('offered','running')",
            params![id, status, now, if status == "failed" { work.reason.as_deref() } else { None }])?;
    }
    Ok(())
}

/// Faults are recorded in the caller's transaction. Each fault has its own
/// notice to the owner, even a second one in the same revision; the same
/// fault recorded again on a schedule already faulted with that reason sends
/// nothing new. The first fault of a revision keeps the original id.
fn fault(store: &Store, work: &mut Scheduled, reason: impl Into<String>, now: i64) -> Result<()> {
    let reason: String = reason.into().chars().take(1000).collect();
    let id = |faults: u32| match faults {
        1 => format!("scheduling-fault-{}-{}", work.task_id, work.revision),
        n => format!("scheduling-fault-{}-{}-{n}", work.task_id, work.revision),
    };
    // A schedule that faulted before faults were counted stores none, though
    // its first notice was sent: count that one, or the next fault would
    // reuse its id and be discarded.
    if work.faults == 0 {
        let sent: bool = store.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM mail WHERE id=?1)",
            [id(1)],
            |r| r.get(0),
        )?;
        if sent {
            work.faults = 1;
        }
    }
    let repeated = work.state == "fault" && work.reason.as_deref() == Some(reason.as_str());
    if !repeated || work.faults == 0 {
        work.faults = work.faults.saturating_add(1);
    }
    let id = id(work.faults);
    work.state = "fault".into();
    work.reason = Some(reason);
    finish_wake(store, work, "failed", now)?;
    crate::mail::record_crew(
        store.connection(),
        &json!({"id": id,
            "to": work.owner, "kind": "notice", "task_id": work.task_id, "urgency": "high",
            "body": format!("Scheduled task {} faulted: {}", work.task_id, work.reason.as_deref().unwrap_or("")),
            "payload": {"scheduling_fault": work.task_id, "revision": work.revision, "fault": work.faults}}),
    )?;
    Ok(())
}

fn reopen(store: &Store, work: &mut Scheduled) -> Result<()> {
    work.state = "ready".into();
    work.ready_ms = Store::now_ms();
    work.wait = None;
    work.reason = None;
    work.resumed_by = None;
    // The caller says what reopened it.
    work.woke_by = None;
    work.offer_id = None;
    work.holder = None;
    work.job_id = None;
    work.profile = None;
    work.charter_hash = None;
    store.connection().execute(
        "UPDATE tasks SET status='open' WHERE id=?1 AND status NOT IN ('done','cancelled')",
        [&work.task_id],
    )?;
    Ok(())
}

/// Whether a task has a scheduling row.
pub(crate) fn scheduled(store: &Store, task: &str) -> Result<bool> {
    Ok(available(store)? && load(store, task)?.is_some())
}

/// Park a scheduled task on its own open block ask, as `task.defer` with a
/// request wait would, in the caller's transaction. An unscheduled task gains
/// no row, and finished or stopped work is left alone. Returns whether the
/// task was parked.
pub(crate) fn park_on_ask(store: &Store, task: &str, ask: &str, reason: &str) -> Result<bool> {
    if !available(store)? {
        return Ok(false);
    }
    let Some(mut work) = load(store, task)? else {
        return Ok(false);
    };
    if matches!(work.state.as_str(), "done" | "stopped" | "fault") {
        return Ok(false);
    }
    let now = Store::now_ms();
    if matches!(work.state.as_str(), "offered" | "claimed" | "executing") {
        work.attempts = 0;
        finish_wake(store, &work, "succeeded", now)?;
    }
    work.state = "waiting".into();
    work.wait = Some(json!({"kind": "request", "ref": ask, "reason": reason}));
    work.reason = Some(reason.chars().take(1000).collect());
    // An earlier resume's reason must not be shown when this wait ends.
    work.resumed_by = None;
    work.not_before_ms = 0;
    work.last_checked_ms = now;
    save(store, &work)?;
    store.connection().execute(
        "UPDATE tasks SET status='blocked' WHERE id=?1 AND status NOT IN ('done','cancelled')",
        [task],
    )?;
    Ok(true)
}

/// Hold a scheduled task until its owner says otherwise, as a manual
/// `task.defer` does, in the caller's transaction.
pub(crate) fn hold(store: &Store, task: &str, reason: &str) -> Result<bool> {
    if !available(store)? {
        return Ok(false);
    }
    let Some(mut work) = load(store, task)? else {
        return Ok(false);
    };
    if work.state == "done" {
        return Ok(false);
    }
    work.state = "stopped".into();
    work.wait = Some(json!({"kind": "manual", "reason": reason}));
    work.reason = Some(reason.chars().take(1000).collect());
    work.resumed_by = None;
    work.last_checked_ms = Store::now_ms();
    save(store, &work)?;
    store.connection().execute(
        "UPDATE tasks SET status='blocked' WHERE id=?1 AND status NOT IN ('done','cancelled')",
        [task],
    )?;
    Ok(true)
}

fn advance(store: &Store, work: &mut Scheduled, now: i64) -> Result<()> {
    work.last_checked_ms = now;
    let task = store.task(&work.task_id)?;
    if let Some(ancestor) = stopped_ancestor(store, &work.task_id)? {
        work.state = "stopped".into();
        work.reason = Some(format!(
            "Scheduled ancestor {ancestor} is explicitly stopped"
        ));
        finish_wake(store, work, "succeeded", now)?;
    } else if task["owner"] != work.owner {
        fault(
            store,
            work,
            "Task ownership changed; the new owner must enqueue its assignment",
            now,
        )?;
    } else if let Some(status @ ("done" | "cancelled")) = task["status"].as_str() {
        work.state = if status == "done" { "done" } else { "stopped" }.into();
        work.reason = Some(format!("Task is {status}"));
        finish_wake(store, work, "succeeded", now)?;
    } else if let Err(error) = store.authorize(&work.task_id, "spawn") {
        fault(
            store,
            work,
            format!("Scheduling authority unavailable: {error}"),
            now,
        )?;
    } else if work.state == "waiting" {
        // A deferred coordinator may still be recording its own result.
        if live_jobs(store, &work.task_id)? {
            return Ok(());
        }
        let wait = work
            .wait
            .clone()
            .context("waiting schedule has no condition")?;
        let ready = match wait["kind"].as_str() {
            Some("time") => now >= work.not_before_ms,
            Some("attention") => {
                let reading = crate::attention::summary_for(store, &work.owner)?;
                (reading["fresh"] == true && reading["pressure"] == "normal")
                    || now >= work.not_before_ms
            }
            Some("task") => {
                let dependency = store.task(required(&wait, "ref")?)?;
                if dependency["status"] == "cancelled" {
                    fault(
                        store,
                        work,
                        "Dependency was cancelled; re-evaluate the assignment",
                        now,
                    )?;
                }
                dependency["status"] == "done"
            }
            Some("request") => {
                let request = crate::mail::find(store, required(&wait, "ref")?)?
                    .context("wait request vanished")?;
                // A block ask that a policy change ended is resolved for one
                // retry: the resume says whether the policy granted it.
                let ended_by_policy = crate::block_ask::resumed_by_policy(&request);
                let covered = ended_by_policy.is_some();
                if covered {
                    work.resumed_by = ended_by_policy;
                }
                if !covered
                    && matches!(
                        request["state"].as_str(),
                        Some("denied" | "withdrawn" | "closed")
                    )
                {
                    fault(
                        store,
                        work,
                        "Request closed without acceptance; re-evaluate the assignment",
                        now,
                    )?;
                }
                covered || matches!(request["state"].as_str(), Some("answered" | "approved"))
            }
            Some("remote") => remote::ready(store, work, &wait, now)?,
            _ => bail!("invalid scheduling wait"),
        };
        if ready {
            let resumed_by = work.resumed_by.take();
            reopen(store, work)?;
            work.reason.clone_from(&resumed_by);
            work.woke_by = Some(json!({
                "kind": if wait["kind"] == "time" { "timer" } else { "wait" },
                "wait_kind": wait["kind"], "ref": wait.get("ref"), "resumed_by": resumed_by,
            }));
            work.resumed_by = resumed_by;
        }
    } else if matches!(work.state.as_str(), "offered" | "claimed" | "executing") {
        let live = live_jobs(store, &work.task_id)?;
        let coordinator_finished = match &work.job_id {
            Some(id) => store
                .connection()
                .query_row(
                    "SELECT json_extract(record,'$.status') FROM jobs WHERE id=?1",
                    [id],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
                .is_some_and(|status| {
                    matches!(
                        status.as_str(),
                        "succeeded" | "failed" | "cancelled" | "timed_out"
                    )
                }),
            None => false,
        };
        if live {
            // Live payloads retain ownership even after delivery expiry. Only
            // an ended coordinator hands off to its already-running children.
            if coordinator_finished {
                work.state = "executing".into();
            }
        } else if work.state == "executing" {
            finish_wake(store, work, "succeeded", now)?;
            reopen(store, work)?;
            work.woke_by = Some(json!({"kind": "children_done"}));
            work.attempts = 0;
        } else if coordinator_finished {
            fault(
                store,
                work,
                "Coordinator ended without completion, live child work or a typed wait",
                now,
            )?;
        } else if work.state == "offered" && work.job_id.is_some() {
            // No job row means no supervised payload was admitted. Recover the
            // same durable launch id even after a long dispatcher outage.
        } else if now >= work.lease_until_ms {
            fault(
                store,
                work,
                "Execution claim expired without proven completion or a typed wait",
                now,
            )?;
        }
    } else if work.state == "ready" && task["status"] == "blocked" {
        fault(
            store,
            work,
            "Blocked scheduled task has no typed wait; use task.defer",
            now,
        )?;
    }
    Ok(())
}

/// One unreadable schedule (say, a wait whose request was pruned) must not
/// stop the dispatcher for every ship; it becomes that schedule's fault.
fn check(store: &Store, work: &mut Scheduled, now: i64) -> Result<()> {
    if let Err(error) = advance(store, work, now) {
        fault(
            store,
            work,
            format!("Scheduling check failed: {error:#}"),
            now,
        )?;
    }
    Ok(())
}

fn reconcile_for(store: &Store, ship: Option<&str>) -> Result<()> {
    let tx = store.write_transaction()?;
    let mut statement = store.connection().prepare(
        "SELECT task_id FROM task_schedule WHERE (?1 IS NULL OR json_extract(record,'$.owner')=?1) AND json_extract(record,'$.state') IN ('ready','waiting','offered','claimed','executing')
         ORDER BY json_extract(record,'$.last_checked_ms'),task_id LIMIT 100")?;
    let ids = statement
        .query_map([ship], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for id in ids {
        let mut work = load(store, &id)?.context("schedule vanished")?;
        check(store, &mut work, Store::now_ms())?;
        save(store, &work)?;
    }
    tx.commit()?;
    Ok(())
}

fn view(work: &Scheduled) -> Value {
    let next_check = match work.state.as_str() {
        "offered" | "claimed" => Some(work.lease_until_ms),
        "ready" => Some(work.not_before_ms),
        "waiting" if work.not_before_ms > 0 => Some(work.not_before_ms),
        _ => None,
    };
    json!({"task_id":work.task_id,"owner":work.owner,"revision":work.revision,"state":work.state,
        "reason":work.reason,"wait":work.wait,"next_check_ms":next_check,
        "last_checked_ms":work.last_checked_ms,"created_ms":work.created_ms,"offer_id":work.offer_id,
        "lease_until_ms":work.lease_until_ms,"attempts":work.attempts,"resumed_by":work.resumed_by})
}

/// Serialize this check with job insertion: an unrelated launcher cannot race
/// a durable offer. Descendants of the recorded coordinator, or of a job
/// admitted under the offer (a hosted coordinator's crew), inherit its claim.
/// A runtime resume (`resume_of`, the parked crew) continues work already
/// admitted: it inherits the claim through the crew's lineage, or, when the
/// crew started before the live claim was offered, through the same task.
pub(crate) fn admit_job(
    store: &Store,
    task: &str,
    job: &str,
    parent: Option<&str>,
    offer: Option<&str>,
    resume_of: Option<&str>,
) -> Result<()> {
    if !available(store)? {
        return Ok(());
    }
    let mut matched_offer = offer.is_none();
    let origin = task.to_owned();
    let mut task = Some(task.to_owned());
    for _ in 0..128 {
        let Some(id) = task else {
            ensure!(
                matched_offer,
                "work_offer is no longer an active claim for this task"
            );
            return Ok(());
        };
        if let Some(work) = load(store, &id)? {
            ensure!(
                work.offer_id.as_deref() != Some(job)
                    || matches!(work.state.as_str(), "offered" | "claimed" | "executing"),
                "scheduled launch intent was withdrawn before admission"
            );
            ensure!(
                work.state != "stopped",
                "a scheduled ancestor is explicitly stopped"
            );
            if matches!(work.state.as_str(), "offered" | "claimed" | "executing") {
                let expected = work
                    .offer_id
                    .as_deref()
                    .context("active work has no offer")?;
                matched_offer |= offer == Some(expected);
                let mut allowed = job == expected || offer == Some(expected);
                if !allowed && id == origin {
                    allowed = resume_predates(store, parent.or(resume_of), &id, expected)?;
                }
                let mut ancestor = parent.or(resume_of).map(str::to_owned);
                for _ in 0..128 {
                    let Some(id) = ancestor else {
                        break;
                    };
                    if id == expected {
                        allowed = true;
                        break;
                    }
                    let raw: Option<String> = store
                        .connection()
                        .query_row("SELECT record FROM jobs WHERE id=?1", [id], |r| r.get(0))
                        .optional()?;
                    let record = raw.map(|r| serde_json::from_str::<Value>(&r)).transpose()?;
                    if record.as_ref().is_some_and(|r| r["work_offer"] == expected) {
                        allowed = true;
                        break;
                    }
                    // A resume continues the crew it names, as a started job
                    // continues the job that started it.
                    ancestor = record.and_then(|r| {
                        r["started_by"]
                            .as_str()
                            .or(r["resume_of"].as_str())
                            .map(str::to_owned)
                    });
                }
                ensure!(
                    allowed,
                    "task has a scheduled execution claim; use its current work_offer or coordinator ancestry"
                );
            }
        }
        task = store.task(&id)?["parent"].as_str().map(str::to_owned);
    }
    bail!("task ancestry exceeds scheduling limit")
}

/// Whether the crew a launch continues began before the live claim was
/// offered, on the claimed task. Its work was in flight first, so the claim
/// must not orphan its resume. The crew is judged by its lineage root (the
/// crew that first began the work), as `crew::lineage` gives it; a root that
/// is a first mate's wake never qualifies.
fn resume_predates(store: &Store, crew: Option<&str>, task: &str, offer: &str) -> Result<bool> {
    let Some(crew) = crew else {
        return Ok(false);
    };
    let root = crate::crew::lineage(store, crew)?
        .last()
        .context("crew lineage")?
        .clone();
    // A first mate's wake is not a crew: a wake that predates the claim does
    // not make its work in flight.
    let woken: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM wakes WHERE job_id=?1)",
        [&root],
        |r| r.get(0),
    )?;
    if woken {
        return Ok(false);
    }
    let job_of = |id: &str| -> Result<Option<(Option<String>, Option<i64>)>> {
        Ok(store
            .connection()
            .query_row(
                "SELECT json_extract(record,'$.task_id'), json_extract(record,'$.created_ms')
                 FROM jobs WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    };
    let Some((crew_task, _)) = job_of(crew)? else {
        return Ok(false);
    };
    let Some((Some(root_task), Some(started))) = job_of(&root)? else {
        return Ok(false);
    };
    let offered: Option<i64> = store
        .connection()
        .query_row("SELECT created_ms FROM wakes WHERE id=?1", [offer], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(crew_task.as_deref() == Some(task)
        && root_task == task
        && offered.is_some_and(|o| started < o))
}

fn stopped_ancestor(store: &Store, task: &str) -> Result<Option<String>> {
    let mut next = store.task(task)?["parent"].as_str().map(str::to_owned);
    for _ in 0..128 {
        let Some(id) = next else {
            return Ok(None);
        };
        if load(store, &id)?.is_some_and(|w| w.state == "stopped") {
            return Ok(Some(id));
        }
        next = store.task(&id)?["parent"].as_str().map(str::to_owned);
    }
    bail!("task ancestry exceeds scheduling limit")
}

#[cfg(test)]
pub(super) mod fixture {
    use super::*;

    /// A home with a charter and one task of `infra`, `work-1`.
    pub(in crate::scheduling) fn home() -> (tempfile::TempDir, Store) {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        let mut store = Store::open(&root.join("home")).unwrap();
        crate::dispatch(
            &mut store,
            "charter.put",
            json!({"id":"project","objective":"Deliver","repo":repo,"min_free_bytes":0}),
        )
        .unwrap();
        crate::dispatch(
            &mut store,
            "task.create",
            json!({"id":"work-1","charter_id":"project","owner":"infra","title":"Work"}),
        )
        .unwrap();
        (temp, store)
    }

    /// `work-1` scheduled in `state` with `wait`, saved as the runtime would.
    pub(in crate::scheduling) fn schedule(store: &Store, state: &str, wait: Option<Value>) {
        let now = Store::now_ms();
        let work = Scheduled {
            task_id: "work-1".into(),
            owner: "infra".into(),
            revision: 1,
            brief: "Continue.".into(),
            state: state.into(),
            wait,
            not_before_ms: 0,
            offer_id: None,
            holder: None,
            job_id: None,
            profile: None,
            charter_hash: None,
            attempts: 0,
            created_ms: now,
            last_checked_ms: now,
            lease_until_ms: 0,
            reason: None,
            resumed_by: None,
            woke_by: None,
            subscription: None,
            faults: 0,
            ready_ms: now,
        };
        save(store, &work).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `work-1` as a far task's `done` reopened it: ready, with the resume's
    /// reason for its next offer.
    fn resumed(store: &Store) {
        fixture::schedule(store, "ready", None);
        let mut work = load(store, "work-1").unwrap().unwrap();
        work.resumed_by = Some("Remote task far-work of apps ended done".into());
        work.reason.clone_from(&work.resumed_by);
        save(store, &work).unwrap();
    }

    #[test]
    fn parking_on_an_ask_forgets_an_earlier_resume() {
        let (_temp, store) = fixture::home();
        resumed(&store);
        assert!(park_on_ask(&store, "work-1", "ask-1", "tool permission: Bash(x)").unwrap());
        let work = load(&store, "work-1").unwrap().unwrap();
        assert_eq!(work.state, "waiting");
        assert!(work.resumed_by.is_none(), "{:?}", work.resumed_by);
    }

    #[test]
    fn the_captains_hold_forgets_an_earlier_resume() {
        let (_temp, store) = fixture::home();
        resumed(&store);
        assert!(hold(&store, "work-1", "the captain stopped it").unwrap());
        let work = load(&store, "work-1").unwrap().unwrap();
        assert_eq!(work.state, "stopped");
        assert!(work.resumed_by.is_none(), "{:?}", work.resumed_by);
    }
}
