//! Selected-home, read-only job/task inspection. No public_job, reconciliation,
//! ledger lifecycle, scheduler observation side effects, or operator payloads.
use super::*;
use crate::door::scoped::{DelegatedContext, Denial};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(crate) const SCAN: usize = 2000;
pub(crate) const STATES: &[&str] = &[
    "starting",
    "queued",
    "launching",
    "running",
    "parked",
    "orphaned",
    "succeeded",
    "failed",
    "cancelled",
    "timed_out",
    "resumed",
];
pub(crate) const KINDS: &[&str] = &["first_mate", "crew", "errand", "command"];
pub(crate) const HARNESSES: &[&str] = &[
    "pi",
    "claude",
    "codex",
    "gemini",
    "opencode",
    "opencode-v2",
    "omp",
    "cursor",
    "agy",
    "kimi",
    "grok",
    "devin",
    "muse",
    "rovo",
];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct JobRef {
    pub origin: String,
    pub incarnation: String,
    pub ship: String,
    pub task_id: String,
    pub id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Age {
    pub at_ms: Option<i64>,
    pub age_ms: Option<i64>,
    pub basis: String,
}
impl Age {
    fn new(at: Option<i64>, basis: &str, now: i64) -> Self {
        Self {
            at_ms: at,
            age_ms: at.filter(|a| *a <= now).map(|a| now.saturating_sub(a)),
            basis: basis.into(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Text {
    pub category: String,
    pub id: Option<String>,
    pub text: Option<String>,
    pub withheld: bool,
    pub at: Age,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Wait {
    pub kind: String,
    pub job: Option<JobRef>,
    pub task_id: Option<String>,
    pub reference_withheld: bool,
    pub since: Age,
    pub next: Option<Text>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Job {
    pub reference: JobRef,
    pub owner: String,
    pub worker: Option<String>,
    pub harness: Option<String>,
    pub kind: String,
    pub state: String,
    pub completed: bool,
    pub liveness: String,
    pub wait: Option<Wait>,
    pub created: Age,
    pub spawned: Age,
    pub finished: Age,
    pub state_since: Age,
    pub exit: String,
    pub progress: Option<Text>,
    pub error: Option<Text>,
    pub parent: Option<JobRef>,
    pub retry_of: Option<JobRef>,
    pub resume_of: Option<JobRef>,
    pub relaunch_of: Option<JobRef>,
    pub resumed_as: Option<JobRef>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Work {
    pub task_id: String,
    pub owner: String,
    pub task_state: String,
    pub state: String,
    pub scheduled: bool,
    pub backlog: bool,
    pub revision: Option<u64>,
    pub wait: Option<Wait>,
    pub reason: Text,
    pub created: Age,
    pub last_checked: Age,
    pub next_check: Age,
    pub lease_until_ms: Option<i64>,
    pub claimed: bool,
    pub offered: bool,
    pub job: Option<JobRef>,
    pub handoff_to: Option<String>,
}

pub(crate) fn handle(s: &str) -> bool {
    s.len() <= 160 && crate::store::identifier(s).is_ok()
}
pub(crate) fn origin(store: &Store) -> Result<String> {
    let value: String = store.connection().query_row(
        "SELECT value FROM metadata WHERE key='runtime_origin'",
        [],
        |r| r.get(0),
    )?;
    ensure!(handle(&value), "invalid origin");
    use std::os::unix::fs::MetadataExt;
    let path = store.home().join("privateer.sqlite");
    crate::store::reject_symlinks(&path)?;
    let meta = std::fs::metadata(path)?;
    Ok(crate::core::hash(&json!([value, meta.dev(), meta.ino()])))
}
pub(crate) fn owned(store: &Store, ship: &str) -> Result<bool> {
    Ok(store.connection().query_row("SELECT EXISTS(SELECT 1 FROM ships WHERE name=?1 AND json_extract(record,'$.location')='here')", [ship], |r| r.get(0))?)
}
fn worker_visible(store: &Store, raw: &Value, c: &DelegatedContext) -> Result<bool> {
    if let Some(actor) = raw["agent_actor"].as_str() {
        if !handle(actor)
            || !c.allows(actor)
            || !store.connection().query_row(
                "SELECT EXISTS(SELECT 1 FROM ships WHERE name=?1)",
                [actor],
                |r| r.get::<_, bool>(0),
            )?
        {
            return Ok(false);
        }
    }
    let id = raw["id"].as_str().unwrap_or_default();
    for sql in [
        "SELECT ship FROM wakes WHERE job_id=?1 LIMIT 129",
        "SELECT ship FROM errands WHERE id=?1 LIMIT 129",
    ] {
        let mut stmt = store.connection().prepare(sql)?;
        for (n, ship) in stmt.query_map([id], |r| r.get::<_, String>(0))?.enumerate() {
            let ship = ship?;
            if n == 128 || !handle(&ship) || !c.allows(&ship) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}
fn record(store: &Store, ship: &str, id: &str, c: &DelegatedContext) -> Result<Option<Value>> {
    if !handle(id) {
        return Ok(None);
    }
    let raw: Option<String> = store.connection().query_row("SELECT j.record FROM jobs j JOIN tasks t ON t.id=json_extract(j.record,'$.task_id') WHERE j.id=?1 AND t.owner=?2 AND length(j.record)<=65536", params![id,ship], |r| r.get(0)).optional()?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let raw: Value = serde_json::from_str(&raw)?;
    Ok(worker_visible(store, &raw, c)?.then_some(raw))
}
fn reference(raw: &Value, ship: &str, origin: &str) -> Option<JobRef> {
    let id = raw["id"].as_str()?;
    let task = raw["task_id"].as_str()?;
    let incarnation = raw["incarnation"].as_str()?;
    (handle(id) && handle(task) && handle(incarnation)).then(|| JobRef {
        id: id.into(),
        task_id: task.into(),
        incarnation: incarnation.into(),
        ship: ship.into(),
        origin: origin.into(),
    })
}
fn related(
    store: &Store,
    ship: &str,
    origin: &str,
    id: Option<&str>,
    c: &DelegatedContext,
) -> Result<Option<JobRef>> {
    Ok(match id {
        Some(id) => record(store, ship, id, c)?.and_then(|v| reference(&v, ship, origin)),
        None => None,
    })
}

/// Best-effort masking is not a transcript entitlement. Strip complete CSI/OSC
/// sequences and controls *before* masking so terminal escapes cannot split a
/// secret key; redact path/endpoint-shaped words, then bound the result.
pub(crate) fn safe_text(raw: &str) -> String {
    let mut plain = String::new();
    let mut chars = raw.chars().take(16_384).peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.next() == Some('\\')) {
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if !ch.is_control()
            && !matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            plain.push(ch);
        } else if matches!(ch, '\n' | '\r' | '\t') {
            plain.push(' ');
        }
    }
    let masked = crate::transcript::mask_text(&plain);
    masked
        .split_whitespace()
        .map(|word| {
            if word.contains('/')
                || word.contains('\\')
                || word.contains('@')
                || word.contains("://")
            {
                "[withheld]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(512)
        .collect()
}
fn text(
    category: &str,
    id: Option<String>,
    raw: Option<&str>,
    at: Option<i64>,
    show: bool,
    now: i64,
) -> Text {
    Text {
        category: category.into(),
        id,
        text: raw.filter(|_| show).map(safe_text),
        withheld: raw.is_some() && !show,
        at: Age::new(at, "recorded_text", now),
    }
}
fn kind(store: &Store, raw: &Value) -> Result<&'static str> {
    let id = raw["id"].as_str().unwrap_or_default();
    Ok(
        if store
            .connection()
            .query_row(super::WAKE_OF_JOB, [id], |r| r.get::<_, bool>(0))?
        {
            "first_mate"
        } else if store.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM errands WHERE id=?1)",
            [id],
            |r| r.get::<_, bool>(0),
        )? {
            "errand"
        } else if raw["action"] == "spawn" && raw["agent_actor"].is_string() {
            "crew"
        } else {
            "command"
        },
    )
}
fn job(
    store: &Store,
    ship: &str,
    source: &str,
    raw: &Value,
    c: &DelegatedContext,
    show: bool,
    now: i64,
) -> Result<Option<Job>> {
    let Some(reference) = reference(raw, ship, source) else {
        return Ok(None);
    };
    let Some(state) = raw["status"].as_str().filter(|s| STATES.contains(s)) else {
        return Ok(None);
    };
    if !worker_visible(store, raw, c)? {
        return Ok(None);
    }
    let rel = |id: Option<&str>| related(store, ship, source, id, c);
    let created = raw["created_ms"].as_i64();
    let finished = raw["finished_ms"].as_i64();
    let mut wait = None;
    if state == "queued" {
        wait = Some(Wait {
            kind: if raw["reason"]
                .as_str()
                .is_some_and(|s| s.contains("disk available"))
            {
                "disk"
            } else {
                "capacity"
            }
            .into(),
            job: None,
            task_id: None,
            reference_withheld: true,
            since: Age::new(created, "creation", now),
            next: None,
        });
    } else if state == "parked" {
        let park: Option<(String,String,i64)> = store.connection().query_row("SELECT child_job_id,substr(next,1,16384),updated_ms FROM crew_parks WHERE crew_job_id=?1 ORDER BY updated_ms DESC LIMIT 1", [&reference.id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let child = rel(park.as_ref().map(|p| p.0.as_str()))?;
        wait = Some(Wait {
            kind: "job".into(),
            reference_withheld: child.is_none(),
            job: child,
            task_id: None,
            since: Age::new(park.as_ref().map(|p| p.2), "park_updated", now),
            next: park
                .as_ref()
                .map(|p| text("park_next", None, Some(&p.1), Some(p.2), show, now)),
        });
    }
    let progress: Option<(String,String,i64)> = store.connection().query_row("SELECT id,substr(body,1,16384),created_ms FROM mail WHERE recipient=?1 AND json_extract(payload,'$.job_id')=?2 AND json_extract(payload,'$.incarnation')=?3 AND json_extract(payload,'$.report')='progress' ORDER BY seq DESC LIMIT 1", params![ship,reference.id,reference.incarnation], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let resumed: Option<String> = store.connection().query_row("SELECT id FROM jobs WHERE json_extract(record,'$.resume_of')=?1 AND EXISTS(SELECT 1 FROM tasks WHERE id=json_extract(jobs.record,'$.task_id') AND owner=?2) ORDER BY rowid DESC LIMIT 1", params![reference.id,ship], |r| r.get(0)).optional()?;
    let completed = ["succeeded", "failed", "cancelled", "timed_out", "resumed"].contains(&state);
    Ok(Some(Job {
        owner: ship.into(),
        worker: raw["agent_actor"].as_str().map(str::to_owned),
        harness: raw["launch"]["manifest"]
            .as_str()
            .and_then(crate::bundled::harness)
            .filter(|h| HARNESSES.contains(&h.as_str())),
        kind: kind(store, raw)?.into(),
        state: state.into(),
        completed,
        liveness: "unknown_recorded_state".into(),
        wait,
        created: Age::new(created, "creation", now),
        spawned: Age::new(raw["spawned_ms"].as_i64(), "payload_spawn", now),
        finished: Age::new(finished, "finish", now),
        state_since: Age::new(
            if completed { finished } else { None },
            "state_transition",
            now,
        ),
        exit: if completed { state } else { "not_recorded" }.into(),
        progress: progress.map(|p| {
            text(
                "progress",
                Some(p.0).filter(|s| handle(s)),
                Some(&p.1),
                Some(p.2),
                show,
                now,
            )
        }),
        // Raw runtime errors are not known-safe prose, even with mail scope.
        error: raw["reason"].as_str().map(|s| {
            text(
                if completed {
                    "job_outcome"
                } else {
                    "runtime_reason"
                },
                Some(reference.id.clone()),
                Some(s),
                finished,
                false,
                now,
            )
        }),
        parent: rel(raw["started_by"].as_str())?,
        retry_of: rel(raw["retry_of"].as_str())?,
        resume_of: rel(raw["resume_of"].as_str())?,
        relaunch_of: rel(raw["relaunch_of"].as_str())?,
        resumed_as: rel(resumed.as_deref())?,
        reference,
    }))
}

pub(crate) fn inspect(
    store: &Store,
    ship: &str,
    wanted: &JobRef,
    c: &DelegatedContext,
    show: bool,
    now: i64,
) -> Result<Job> {
    let raw = checked_record(store, ship, wanted, c)?;
    job(
        store,
        ship,
        &wanted.origin,
        &raw,
        c,
        show && c.has("mail:read"),
        now,
    )?
    .ok_or_else(|| Denial::Ship.into())
}
fn checked_record(
    store: &Store,
    ship: &str,
    wanted: &JobRef,
    c: &DelegatedContext,
) -> Result<Value> {
    ensure!(
        c.has("fleet:read")
            && c.allows(ship)
            && owned(store, ship)?
            && origin(store)? == wanted.origin
            && wanted.ship == ship,
        Denial::Ship
    );
    let raw = record(store, ship, &wanted.id, c)?.ok_or(Denial::Ship)?;
    ensure!(
        reference(&raw, ship, &wanted.origin).as_ref() == Some(wanted),
        Denial::Ship
    );
    Ok(raw)
}
/// Release-time checks need custody/incarnation, not another authored-body scan.
pub(crate) fn check_reference(
    store: &Store,
    ship: &str,
    wanted: &JobRef,
    c: &DelegatedContext,
) -> Result<()> {
    checked_record(store, ship, wanted, c)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn work(
    store: &Store,
    ship: &str,
    source: &str,
    task: &Value,
    raw: Option<&str>,
    c: &DelegatedContext,
    show: bool,
    now: i64,
) -> Result<Option<Work>> {
    let id = task["id"].as_str().unwrap_or_default();
    let task_state = task["status"].as_str().unwrap_or_default();
    if !handle(id)
        || !["open", "running", "done", "cancelled", "blocked", "pending"].contains(&task_state)
    {
        return Ok(None);
    }
    let schedule: Value = raw
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or(Value::Null);
    let scheduled = !schedule.is_null();
    if scheduled && (schedule["owner"] != ship || schedule["task_id"] != id) {
        return Ok(None);
    }
    let state = if scheduled {
        schedule["state"].as_str().unwrap_or("unknown")
    } else {
        "unscheduled"
    };
    if ![
        "ready",
        "offered",
        "claimed",
        "executing",
        "waiting",
        "stopped",
        "fault",
        "unscheduled",
    ]
    .contains(&state)
    {
        return Ok(None);
    }
    let mut wait = None;
    if let Some(k) = schedule["wait"]["kind"]
        .as_str()
        .filter(|k| ["task", "request", "time", "remote", "attention", "manual"].contains(k))
    {
        let dep = schedule["wait"]["ref"].as_str();
        let visible = if k == "task" {
            match dep {
                Some(dep) => store.connection().query_row(
                    "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1 AND owner=?2)",
                    params![dep, ship],
                    |r| r.get::<_, bool>(0),
                )?,
                None => false,
            }
        } else {
            false
        };
        wait = Some(Wait {
            kind: k.into(),
            job: None,
            task_id: dep.filter(|d| visible && handle(d)).map(str::to_owned),
            reference_withheld: !visible,
            since: Age::new(None, "wait_transition_unknown", now),
            next: None,
        });
    }
    Ok(Some(Work {
        task_id: id.into(),
        owner: ship.into(),
        task_state: task_state.into(),
        state: state.into(),
        scheduled,
        backlog: !["done", "cancelled"].contains(&task_state),
        revision: schedule["revision"].as_u64(),
        wait,
        reason: text(
            if scheduled {
                "schedule_reason"
            } else {
                "legacy_or_draft"
            },
            Some(id.into()),
            schedule["reason"].as_str(),
            None,
            show,
            now,
        ),
        created: Age::new(schedule["created_ms"].as_i64(), "schedule_creation", now),
        last_checked: Age::new(schedule["last_checked_ms"].as_i64(), "schedule_check", now),
        next_check: Age::new(
            schedule["not_before_ms"].as_i64().filter(|v| *v > 0),
            "not_before",
            now,
        ),
        lease_until_ms: schedule["lease_until_ms"].as_i64().filter(|v| *v > 0),
        claimed: schedule["holder"].is_string(),
        offered: schedule["offer_id"].is_string(),
        job: related(store, ship, source, schedule["job_id"].as_str(), c)?,
        handoff_to: task["handoff_to"]
            .as_str()
            .filter(|s| handle(s) && c.allows(s))
            .map(str::to_owned),
    }))
}

pub(crate) struct Scan {
    pub drained: bool,
    pub jobs: Vec<(i64, Job)>,
    pub work: Vec<(i64, Work)>,
    pub counts: BTreeMap<String, u64>,
    pub complete: bool,
    pub last: i64,
}
#[allow(clippy::too_many_arguments)]
pub(crate) fn scan(
    store: &Store,
    ship: &str,
    c: &DelegatedContext,
    is_work: bool,
    after: i64,
    upper: i64,
    state: Option<&str>,
    filter_kind: Option<&str>,
    show: bool,
    now: i64,
) -> Result<Scan> {
    ensure!(
        c.has("fleet:read") && c.allows(ship) && owned(store, ship)?,
        Denial::Ship
    );
    let source = origin(store)?;
    let show = show && c.has("mail:read");
    let started = Instant::now();
    let budget = ObservationBudget::new(store.connection())?;
    let mut result = Scan {
        drained: true,
        jobs: vec![],
        work: vec![],
        counts: BTreeMap::new(),
        complete: after == 0,
        last: after,
    };
    let sql = if is_work {
        "SELECT t.rowid,json_object('id',t.id,'status',t.status,'handoff_to',t.handoff_to), CASE WHEN length(s.record)<=65536 THEN s.record WHEN s.record IS NOT NULL THEN '{}' END FROM tasks t LEFT JOIN task_schedule s ON s.task_id=t.id WHERE t.owner=?1 AND t.rowid>?2 AND t.rowid<=?3 ORDER BY t.rowid LIMIT 2001"
    } else {
        "SELECT j.rowid,CASE WHEN length(j.record)<=65536 THEN j.record ELSE '{}' END,NULL FROM jobs j JOIN tasks t ON t.id=json_extract(j.record,'$.task_id') WHERE t.owner=?1 AND j.rowid>?2 AND j.rowid<=?3 ORDER BY j.rowid LIMIT 2001"
    };
    let mut stmt = store.connection().prepare(sql)?;
    let rows = stmt.query_map(params![ship, after, upper], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    for (n, row) in rows.enumerate() {
        if n == SCAN || started.elapsed() >= LOCAL_BUDGET {
            result.drained = false;
            result.complete = false;
            break;
        }
        // Bound private in-memory DTO expansion independently of page size.
        if result.jobs.len() + result.work.len() >= 256 {
            result.drained = false;
            result.complete = false;
            break;
        }
        let previous = result.last;
        let processing = (|| -> Result<()> {
            let (key, raw, schedule) = row?;
            let raw: Value = serde_json::from_str(&raw)?;
            budget.check()?;
            result.last = key;
            if is_work {
                if let Some(row) = work(
                    store,
                    ship,
                    &source,
                    &raw,
                    schedule.as_deref(),
                    c,
                    show,
                    now,
                )? {
                    *result.counts.entry(row.state.clone()).or_default() += 1;
                    *result
                        .counts
                        .entry(
                            if row.scheduled {
                                "scheduled"
                            } else {
                                "unscheduled_total"
                            }
                            .into(),
                        )
                        .or_default() += 1;
                    if row.backlog {
                        *result.counts.entry("backlog".into()).or_default() += 1;
                    }
                    result.work.push((key, row));
                } else {
                    result.complete = false;
                }
            } else {
                if state.is_some_and(|s| {
                    if s == "completed" {
                        !["succeeded", "failed", "cancelled", "timed_out", "resumed"]
                            .iter()
                            .any(|s| raw["status"] == *s)
                    } else {
                        raw["status"] != s
                    }
                }) {
                    return Ok(());
                }
                if let Some(k) = filter_kind
                    && kind(store, &raw)? != k
                {
                    return Ok(());
                }
                if let Some(row) = job(store, ship, &source, &raw, c, show, now)? {
                    if state.is_some_and(|s| {
                        if s == "completed" {
                            !row.completed
                        } else {
                            row.state != s
                        }
                    }) || filter_kind.is_some_and(|k| row.kind != k)
                    {
                        return Ok(());
                    }
                    *result.counts.entry(row.state.clone()).or_default() += 1;
                    if row.completed {
                        *result.counts.entry("completed".into()).or_default() += 1;
                    }
                    result.jobs.push((key, row));
                } else {
                    result.complete = false;
                }
            }
            Ok(())
        })();
        if let Err(error) = processing {
            let interrupted=error.downcast_ref::<rusqlite::Error>().is_some_and(|e|matches!(e,rusqlite::Error::SqliteFailure(code,_) if code.code==rusqlite::ErrorCode::OperationInterrupted));
            if budget.check().is_err() || interrupted {
                // Do not skip the row interrupted midway through expansion.
                result.last = previous;
                result.drained = false;
                result.complete = false;
                break;
            }
            return Err(error);
        }
    }
    if budget.check().is_err() {
        result.complete = false;
    }
    Ok(result)
}

#[cfg(test)]
mod tests;
