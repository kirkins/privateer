//! Waking first mates when their ship has mail, at no model cost while idle.
//!
//! Mail is the durable record of a pending wake: nothing separate needs
//! queueing, so a crash or restart loses nothing. Each wake records the
//! mail it covered (through a sequence number), its job, its outcome and the
//! token usage the harness reported. `privateer serve` runs `tick`
//! repeatedly; one tick flushes pending deliveries, settles finished wakes
//! and starts the wakes that are due.
use crate::store::{Caller, Store, identifier};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value, json};
use std::path::Path;

/// Defaults for a first mate's activation settings, in seconds or counts.
const SETTINGS: &[(&str, u64, u64, u64)] = &[
    // name, default, minimum, maximum
    ("batch_secs", 5, 0, 3600),
    ("max_delay_secs", 60, 0, 86_400),
    ("max_wakes_per_hour", 12, 1, 3600),
    ("retry_backoff_secs", 60, 0, 86_400),
];
const MAX_BODY_CHARS: usize = 2_000;
const MAX_WAKE_BYTES: usize = 16_384;

pub fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS wakes(id TEXT PRIMARY KEY, ship TEXT NOT NULL, focus TEXT NOT NULL,
           through_seq INTEGER NOT NULL, job_id TEXT, mode TEXT NOT NULL, status TEXT NOT NULL,
           created_ms INTEGER NOT NULL, finished_ms INTEGER, error TEXT, usage TEXT);
         CREATE INDEX IF NOT EXISTS wake_ship ON wakes(ship, created_ms);",
    )?;
    Ok(())
}

/// Validate a ship's first-mate profile: the harness plugin to launch, the
/// duty task it works under (owned by the ship, so its charter applies), and
/// activation settings.
pub fn profile(store: &Store, ship: &str, profile: &Value) -> Result<Value> {
    let object = profile
        .as_object()
        .context("first_mate must be an object")?;
    let mut out = Map::new();
    for key in ["manifest", "config"] {
        if let Some(path) = object.get(key).filter(|v| !v.is_null()) {
            let path = path
                .as_str()
                .with_context(|| format!("first_mate.{key} must be a path"))?;
            ensure!(
                Path::new(path).is_absolute() && Path::new(path).is_file(),
                "first_mate.{key} must be an existing absolute file"
            );
            out.insert(key.into(), json!(path));
        }
    }
    ensure!(
        out.contains_key("manifest"),
        "first_mate.manifest is required"
    );
    let duty = object
        .get("duty_task")
        .and_then(Value::as_str)
        .context("first_mate.duty_task is required")?;
    let task = store.task(duty)?;
    ensure!(
        task["owner"] == ship,
        "duty task {duty} must be owned by {ship}"
    );
    out.insert("duty_task".into(), json!(duty));
    let options = object.get("options").cloned().unwrap_or(json!({}));
    ensure!(options.is_object(), "first_mate.options must be an object");
    out.insert("options".into(), options);
    for (name, default, min, max) in SETTINGS {
        let value = match object.get(*name).filter(|v| !v.is_null()) {
            Some(v) => v
                .as_u64()
                .filter(|v| (*min..=*max).contains(v))
                .with_context(|| format!("first_mate.{name} must be {min}..{max}"))?,
            None => *default,
        };
        out.insert((*name).into(), json!(value));
    }
    for key in object.keys() {
        ensure!(out.contains_key(key), "unknown first_mate setting {key}");
    }
    Ok(Value::Object(out))
}

fn setting(profile: &Value, name: &str) -> i64 {
    profile[name].as_i64().unwrap_or(0)
}

/// Local ships with a first mate to wake.
fn ships(store: &Store) -> Result<Vec<Value>> {
    let mut statement = store
        .connection()
        .prepare("SELECT record FROM ships ORDER BY name")?;
    let records = statement
        .query_map([], |r| r.get::<_, String>(0))?
        .map(|raw| Ok(serde_json::from_str::<Value>(&raw?)?))
        .collect::<Result<Vec<_>>>()?;
    Ok(records
        .into_iter()
        .filter(|s| s["location"] == "here" && s["first_mate"].is_object())
        .collect())
}

struct Mail {
    seq: i64,
    created_ms: i64,
    routine: bool,
    summary: String,
}

/// Mail for a ship newer than both its acknowledgement and its last wake.
fn new_mail(store: &Store, ship: &str, after: i64) -> Result<Vec<Mail>> {
    let mut statement = store.connection().prepare(
        "SELECT seq, id, sender, kind, correlation, request_kind, urgency, body, payload, created_ms
         FROM mail WHERE recipient=?1 AND COALESCE(delivery, 'received')='received' AND seq > ?2
         ORDER BY seq LIMIT 200",
    )?;
    let rows = statement.query_map(params![ship, after], |r| {
        let kind: String = r.get(3)?;
        let urgency: String = r.get(6)?;
        let body: String = r.get(7)?;
        let body: String = body.chars().take(MAX_BODY_CHARS).collect();
        let payload: String = r.get(8)?;
        let mut summary = format!(
            "- seq {} id {} from {} kind {}",
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            kind
        );
        if let Some(request_kind) = r.get::<_, Option<String>>(5)? {
            summary.push_str(&format!(" request_kind {request_kind}"));
        }
        if let Some(correlation) = r.get::<_, Option<String>>(4)? {
            summary.push_str(&format!(" correlation {correlation}"));
        }
        summary.push_str(&format!(" urgency {urgency}\n  {body}"));
        if payload != "{}" && payload.len() <= 1024 {
            summary.push_str(&format!("\n  payload {payload}"));
        }
        Ok(Mail {
            seq: r.get(0)?,
            created_ms: r.get(9)?,
            // Information-only notices wait for the next real wake.
            routine: kind == "notice" && urgency == "low",
            summary,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The changing part of a wake prompt, appended after the stable brief.
fn wake_text(store: &Store, wake: &str, ship: &str, mail: &[Mail], through: i64) -> Result<String> {
    let owed: i64 = store.connection().query_row(
        "SELECT COUNT(*) FROM mail WHERE recipient=?1 AND state='open'",
        [ship],
        |r| r.get(0),
    )?;
    let owing: i64 = store.connection().query_row(
        "SELECT COUNT(*) FROM mail WHERE sender=?1 AND state='open'",
        [ship],
        |r| r.get(0),
    )?;
    let mut text = format!(
        "Wake {wake} for ship {ship}. Requests you owe a reply: {owed}; requests awaiting replies to you: {owing}.\nNew mail through seq {through}:\n"
    );
    let mut shown = 0;
    for item in mail {
        if text.len() + item.summary.len() > MAX_WAKE_BYTES {
            break;
        }
        text.push_str(&item.summary);
        text.push('\n');
        shown += 1;
    }
    if shown < mail.len() {
        text.push_str(&format!(
            "...and {} more; read them with mail.inbox.\n",
            mail.len() - shown
        ));
    }
    text.push_str(&format!(
        "Handle this mail within your charter: answer requests with mail.send kind reply and correlation set to the request id, delegate or act as needed. When done, acknowledge with mail.ack {{\"address\":\"{ship}\",\"through_seq\":{through}}}."
    ));
    Ok(text)
}

/// Normalize the token usage a harness printed as JSON lines on stdout.
/// Codex reports per turn (`turn.completed`), Claude a cumulative `result`;
/// anything else falls back to the last usage object seen.
fn usage(log: &str) -> Option<Value> {
    let mut turns: Option<Map<String, Value>> = None;
    let mut result = None;
    let mut last = None;
    for line in log.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(usage) = value.get("usage").and_then(Value::as_object) else {
            continue;
        };
        match value["type"].as_str() {
            Some("turn.completed") => {
                let sum = turns.get_or_insert_with(Map::new);
                for (key, n) in usage {
                    if let Some(n) = n.as_u64() {
                        let total = sum.get(key).and_then(Value::as_u64).unwrap_or(0) + n;
                        sum.insert(key.clone(), json!(total));
                    }
                }
            }
            Some("result") => result = Some(usage.clone()),
            _ => last = Some(usage.clone()),
        }
    }
    turns
        .or(result)
        .or(last)
        .map(|fields| Value::Object(fields.into_iter().filter(|(_, v)| v.is_number()).collect()))
}

fn job_status(store: &mut Store, job_id: &str) -> Result<Value> {
    crate::jobs::dispatch(store, "job.status", json!({"id": job_id}))
}

/// Record the outcome of wakes whose first mate has finished.
fn settle(store: &mut Store) -> Result<Vec<Value>> {
    let running: Vec<(String, String)> = {
        let mut statement = store
            .connection()
            .prepare("SELECT id, job_id FROM wakes WHERE status='running'")?;
        statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut settled = vec![];
    for (wake, job_id) in running {
        let job = job_status(store, &job_id)?;
        let status = job["status"].as_str().unwrap_or("");
        if !matches!(status, "succeeded" | "failed" | "cancelled" | "timed_out") {
            continue;
        }
        let log = store
            .home()
            .join("jobs")
            .join(&job_id)
            .join(job["incarnation"].as_str().unwrap_or(""))
            .join("stdout.log");
        let reported = std::fs::read_to_string(log).ok().and_then(|l| usage(&l));
        let outcome = if status == "succeeded" {
            "succeeded"
        } else {
            "failed"
        };
        store.connection().execute(
            "UPDATE wakes SET status=?2, finished_ms=?3, error=?4, usage=?5 WHERE id=?1",
            params![
                wake,
                outcome,
                Store::now_ms(),
                (outcome == "failed").then(|| format!("first mate job {status}")),
                reported.as_ref().map(Value::to_string)
            ],
        )?;
        settled.push(json!({"wake": wake, "status": outcome, "usage": reported}));
    }
    Ok(settled)
}

/// Decide whether a ship's first mate is due, and wake it if so.
fn consider(store: &mut Store, ship: &Value) -> Result<Value> {
    let name = ship["name"].as_str().context("ship name")?.to_owned();
    let profile = ship["first_mate"].clone();
    let now = Store::now_ms();
    let running: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM wakes WHERE ship=?1 AND status='running')",
        [&name],
        |r| r.get(0),
    )?;
    if running {
        return Ok(json!({"ship": name, "skipped": "first mate already awake"}));
    }
    // A wake covers mail through its sequence whether or not the first mate
    // acknowledged it, so an agent that forgets to ack is not woken forever.
    let woken_through: i64 = store.connection().query_row(
        "SELECT COALESCE(MAX(through_seq), 0) FROM wakes WHERE ship=?1 AND status='succeeded'",
        [&name],
        |r| r.get(0),
    )?;
    let acknowledged: i64 = store
        .connection()
        .query_row(
            "SELECT through_seq FROM mail_cursor WHERE address=?1",
            [&name],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    let mail = new_mail(store, &name, woken_through.max(acknowledged))?;
    let triggers: Vec<&Mail> = mail.iter().filter(|m| !m.routine).collect();
    if triggers.is_empty() {
        return Ok(json!({"ship": name, "skipped": "nothing wake-worthy"}));
    }
    let newest = triggers.iter().map(|m| m.created_ms).max().unwrap_or(now);
    let oldest = triggers.iter().map(|m| m.created_ms).min().unwrap_or(now);
    let quiet = now - newest >= setting(&profile, "batch_secs") * 1000;
    let overdue = now - oldest >= setting(&profile, "max_delay_secs") * 1000;
    if !quiet && !overdue {
        return Ok(json!({"ship": name, "skipped": "batching"}));
    }
    let last: Option<(String, Option<i64>)> = store
        .connection()
        .query_row(
            "SELECT status, finished_ms FROM wakes WHERE ship=?1 ORDER BY created_ms DESC LIMIT 1",
            [&name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let last_failure = last
        .filter(|(status, _)| status == "failed")
        .and_then(|(_, at)| at);
    if last_failure.is_some_and(|at| now - at < setting(&profile, "retry_backoff_secs") * 1000) {
        return Ok(json!({"ship": name, "skipped": "backing off after a failed wake"}));
    }
    let recent: i64 = store.connection().query_row(
        "SELECT COUNT(*) FROM wakes WHERE ship=?1 AND created_ms > ?2",
        params![name, now - 3_600_000],
        |r| r.get(0),
    )?;
    if recent >= setting(&profile, "max_wakes_per_hour") {
        return Ok(json!({"ship": name, "skipped": "hourly wake budget spent"}));
    }

    let through = mail.iter().map(|m| m.seq).max().unwrap_or(0);
    let wake = format!("wake-{}", uuid::Uuid::new_v4());
    let focus = profile["duty_task"].as_str().unwrap_or("").to_owned();
    let text = wake_text(store, &wake, &name, &mail, through)?;
    let mut start = json!({
        "id": wake, "task_id": focus, "manifest": profile["manifest"],
        "options": profile["options"], "wake": text,
    });
    if let Some(config) = profile.get("config") {
        start["config"] = config.clone();
    }
    let started = crate::dispatch(store, "agent.start", start);
    let (status, error) = match &started {
        Ok(_) => ("running", None),
        Err(error) => ("failed", Some(format!("{error:#}"))),
    };
    store.connection().execute(
        "INSERT INTO wakes(id, ship, focus, through_seq, job_id, mode, status, created_ms, finished_ms, error)
         VALUES(?1, ?2, ?3, ?4, ?5, 'fresh', ?6, ?7, ?8, ?9)",
        params![
            wake,
            name,
            focus,
            through,
            started.is_ok().then_some(&wake),
            status,
            now,
            error.is_some().then_some(now),
            error
        ],
    )?;
    store.audit(
        "wake.started",
        &json!({"wake": wake, "ship": name, "through_seq": through, "status": status}),
    )?;
    Ok(
        json!({"ship": name, "wake": wake, "status": status, "through_seq": through, "error": error}),
    )
}

/// One dispatcher pass: deliver pending mail, settle finished wakes, wake
/// the first mates that are due.
pub fn tick(store: &mut Store) -> Result<Value> {
    let delivered = crate::dispatch(store, "mail.flush", json!({}))?;
    let settled = settle(store)?;
    let mut considered = vec![];
    for ship in ships(store)? {
        considered.push(consider(store, &ship)?);
    }
    Ok(json!({"delivered": delivered, "settled": settled, "ships": considered}))
}

/// Run the dispatcher. Only one runs per home, guarded by an exclusive lock
/// on `<home>/serve.lock` held for the life of the process.
pub fn serve(store: &mut Store, args: &Value) -> Result<()> {
    ensure!(
        *store.caller() == Caller::Operator,
        "serve runs as the operator's background service, not as an agent"
    );
    use std::os::fd::AsRawFd;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(store.home().join("serve.lock"))?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "another serve is already running for this home"
    );
    let once = args.get("once") == Some(&json!(true));
    let interval = args
        .get("interval_secs")
        .and_then(Value::as_u64)
        .unwrap_or(2)
        .clamp(1, 3600);
    loop {
        let report = tick(store)?;
        let acted = report["settled"].as_array().is_some_and(|s| !s.is_empty())
            || report["delivered"]["delivered"].as_u64().unwrap_or(0) > 0
            || report["ships"]
                .as_array()
                .is_some_and(|s| s.iter().any(|r| r.get("wake").is_some()));
        if once || acted {
            println!("{}", serde_json::to_string(&report)?);
        }
        if once {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "wake.list" => {
            let ship = args.get("ship").and_then(Value::as_str);
            if let Some(ship) = ship {
                identifier(ship)?;
            }
            let limit = args
                .get("limit")
                .and_then(Value::as_i64)
                .unwrap_or(50)
                .clamp(1, 1000);
            let mut statement = store.connection().prepare(
                "SELECT id, ship, focus, through_seq, job_id, mode, status, created_ms, finished_ms, error, usage
                 FROM wakes WHERE (?1 IS NULL OR ship=?1) ORDER BY created_ms DESC LIMIT ?2",
            )?;
            let wakes = statement
                .query_map(params![ship, limit], |r| {
                    let usage: Option<String> = r.get(10)?;
                    Ok(json!({
                        "id": r.get::<_, String>(0)?, "ship": r.get::<_, String>(1)?,
                        "focus": r.get::<_, String>(2)?, "through_seq": r.get::<_, i64>(3)?,
                        "job_id": r.get::<_, Option<String>>(4)?, "mode": r.get::<_, String>(5)?,
                        "status": r.get::<_, String>(6)?, "created_ms": r.get::<_, i64>(7)?,
                        "finished_ms": r.get::<_, Option<i64>>(8)?, "error": r.get::<_, Option<String>>(9)?,
                        "usage": usage.and_then(|u| serde_json::from_str::<Value>(&u).ok()),
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(json!({"wakes": wakes}))
        }
        _ => bail!("unknown wake command {command}"),
    }
}
