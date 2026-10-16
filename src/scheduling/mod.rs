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
}

mod commands;
mod delivery;
mod observation;
pub use commands::dispatch;
pub(crate) use delivery::tick;
pub(crate) use observation::{fault_item, snapshot};

pub(crate) fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS task_schedule(task_id TEXT PRIMARY KEY REFERENCES tasks(id),record TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS work_sessions(ship TEXT PRIMARY KEY,holder TEXT NOT NULL,idle INTEGER NOT NULL,checked_ms INTEGER NOT NULL);",
    )?;
    Ok(())
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
    let raw = serde_json::to_string(work)?;
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

fn fault(store: &Store, work: &mut Scheduled, reason: impl Into<String>, now: i64) -> Result<()> {
    work.state = "fault".into();
    work.reason = Some(reason.into().chars().take(1000).collect());
    finish_wake(store, work, "failed", now)
}

fn reopen(store: &Store, work: &mut Scheduled) -> Result<()> {
    work.state = "ready".into();
    work.wait = None;
    work.reason = None;
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
    } else if matches!(task["status"].as_str(), Some("done" | "cancelled")) {
        work.state = if task["status"] == "done" {
            "done"
        } else {
            "stopped"
        }
        .into();
        work.reason = Some(format!("Task is {}", task["status"].as_str().unwrap()));
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
                if matches!(
                    request["state"].as_str(),
                    Some("denied" | "withdrawn" | "closed")
                ) {
                    fault(
                        store,
                        work,
                        "Request closed without acceptance; re-evaluate the assignment",
                        now,
                    )?;
                }
                matches!(request["state"].as_str(), Some("answered" | "approved"))
            }
            _ => bail!("invalid scheduling wait"),
        };
        if ready {
            reopen(store, work)?;
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
        "lease_until_ms":work.lease_until_ms,"attempts":work.attempts})
}

/// Serialize this check with job insertion: an unrelated launcher cannot race
/// a durable offer. Descendants of the recorded coordinator, or of a job
/// admitted under the offer (a hosted coordinator's crew), inherit its claim.
pub(crate) fn admit_job(
    store: &Store,
    task: &str,
    job: &str,
    parent: Option<&str>,
    offer: Option<&str>,
) -> Result<()> {
    if !available(store)? {
        return Ok(());
    }
    let mut matched_offer = offer.is_none();
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
                let mut ancestor = parent.map(str::to_owned);
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
                    ancestor = record.and_then(|r| r["started_by"].as_str().map(str::to_owned));
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
