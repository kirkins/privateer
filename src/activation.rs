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
use std::time::Duration;

/// Defaults for a first mate's activation settings, in seconds or counts.
const SETTINGS: &[(&str, u64, u64, u64)] = &[
    // name, default, minimum, maximum
    ("batch_secs", 5, 0, 3600),
    ("max_delay_secs", 60, 0, 86_400),
    ("max_wakes_per_hour", 12, 1, 3600),
    ("retry_backoff_secs", 60, 0, 86_400),
    // Resume a focus's session only while the provider's prompt cache is
    // likely warm (0 disables resuming), and rotate after this many wakes.
    ("cache_window_secs", 300, 0, 86_400),
    ("session_max_wakes", 6, 1, 1_000),
];
const MAX_BODY_CHARS: usize = 2_000;
const MAX_WAKE_BYTES: usize = 16_384;

pub fn init(store: &Store) -> Result<()> {
    let tx = store.write_transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS wakes(id TEXT PRIMARY KEY, ship TEXT NOT NULL, focus TEXT NOT NULL,
           through_seq INTEGER NOT NULL, job_id TEXT, mode TEXT NOT NULL, status TEXT NOT NULL,
           created_ms INTEGER NOT NULL, finished_ms INTEGER, error TEXT, usage TEXT);
         CREATE INDEX IF NOT EXISTS wake_ship ON wakes(ship, created_ms);",
    )?;
    for column in ["focuses", "harness", "session_id"] {
        let present: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('wakes') WHERE name=?1)",
            [column],
            |r| r.get(0),
        )?;
        if !present {
            tx.execute_batch(&format!("ALTER TABLE wakes ADD COLUMN {column} TEXT;"))?;
        }
    }
    // One native session per ship and focus, so topics never share a
    // conversation.
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions(ship TEXT NOT NULL, focus TEXT NOT NULL, harness TEXT NOT NULL,
           session_id TEXT NOT NULL, wakes INTEGER NOT NULL, started_ms INTEGER NOT NULL,
           last_turn_ms INTEGER NOT NULL, PRIMARY KEY(ship, focus));",
    )?;
    tx.commit()?;
    Ok(())
}

/// The native session a harness reported starting, from its JSON event
/// stream. Mirrors `session_started` in plugins/native_events.py; a harness
/// not listed there stays unresumable.
fn session_started(harness: &str, event: &Value) -> Option<String> {
    let kind = event[if harness == "agy" { "event" } else { "type" }].as_str()?;
    let value = match harness {
        "codex" if kind == "thread.started" => &event["thread_id"],
        "pi" | "pi-signed" | "gateway-pi" | "omp" if kind == "session" => &event["id"],
        "claude" | "cursor" if kind == "system" && event["subtype"] == "init" => {
            &event["session_id"]
        }
        "gemini" if kind == "init" => &event["session_id"],
        "agy" if kind == "init" => &event["conversation_id"],
        "opencode" | "opencode-v2" if kind == "step_start" => &event["sessionID"],
        "rovo" | "kimi" | "devin" | "grok" | "muse"
            if kind == "session.started" && event["harness"] == harness =>
        {
            &event["session_id"]
        }
        _ => return None,
    };
    let id = value.as_str()?;
    (!id.is_empty() && id.len() <= 512 && !id.chars().any(|c| c.is_whitespace() || c.is_control()))
        .then(|| id.to_owned())
}

fn read_session(harness: &str, log: &str) -> Option<String> {
    log.lines()
        .take(10_000)
        .filter(|line| line.len() <= 65_536)
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|event| session_started(harness, &event))
}

/// The manifest's declared harness id, which selects session decoding.
fn harness_id(profile: &Value) -> String {
    profile["manifest"]
        .as_str()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .and_then(|manifest| manifest["id"].as_str().map(str::to_owned))
        .unwrap_or_default()
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
    routine: bool,
    crew: bool,
    focus: String,
    summary: String,
}

/// Mail for a ship newer than both its acknowledgement and its last wake,
/// each labelled with the focus it belongs to.
fn new_mail(store: &Store, ship: &str, after: i64) -> Result<Vec<Mail>> {
    let mut statement = store.connection().prepare(
        "SELECT seq, id, sender, kind, correlation, request_kind, urgency, body, payload, task_id
         FROM mail WHERE recipient=?1 AND COALESCE(delivery, 'received')='received' AND seq > ?2
         ORDER BY seq LIMIT 200",
    )?;
    let rows = statement.query_map(params![ship, after], |r| {
        let kind: String = r.get(3)?;
        let urgency: String = r.get(6)?;
        let body: String = r.get(7)?;
        let body: String = body.chars().take(MAX_BODY_CHARS).collect();
        let payload: String = r.get(8)?;
        let id: String = r.get(1)?;
        let from: String = r.get(2)?;
        let correlation: Option<String> = r.get(4)?;
        let mut summary = format!(
            "- seq {} id {id} from {from} kind {kind}",
            r.get::<_, i64>(0)?
        );
        if let Some(request_kind) = r.get::<_, Option<String>>(5)? {
            summary.push_str(&format!(" request_kind {request_kind}"));
        }
        if let Some(correlation) = &correlation {
            summary.push_str(&format!(" correlation {correlation}"));
        }
        summary.push_str(&format!(" urgency {urgency}\n  {body}"));
        // A crew report's body already states its payload.
        if from != crate::mail::CREW && payload != "{}" && payload.len() <= 1024 {
            summary.push_str(&format!("\n  payload {payload}"));
        }
        let message = json!({"id": id, "from": from, "correlation": correlation,
            "task_id": r.get::<_, Option<String>>(9)?});
        Ok((
            Mail {
                seq: r.get(0)?,
                // Information-only notices wait for the next real wake.
                routine: kind == "notice" && urgency == "low",
                crew: from == crate::mail::CREW,
                focus: String::new(),
                summary,
            },
            message,
        ))
    })?;
    let mut mail = vec![];
    for row in rows {
        let (mut item, message) = row?;
        item.focus = crate::context::focus_of(store, ship, &message)?;
        mail.push(item);
    }
    Ok(mail)
}

/// The changing part of a wake prompt, appended after the stable brief:
/// bearings from records, then the new mail grouped by focus, each focus
/// with its handoff note, so a fresh first mate sees the judgement it left
/// for exactly the topics in play.
fn wake_text(
    store: &Store,
    wake: &str,
    ship: &str,
    mail: &[Mail],
    through: i64,
    resumed: bool,
) -> Result<String> {
    let mut text = format!("Wake {wake} for ship {ship}.\n");
    // A resumed session already has its bearings and notes in context.
    if !resumed {
        let bearings = crate::context::bearings(store, ship)?;
        text.push_str(bearings["text"].as_str().unwrap_or(""));
    }
    text.push_str(&format!("New mail through seq {through}, by focus:\n"));
    let mut foci: Vec<&str> = vec![];
    for item in mail {
        if !foci.contains(&item.focus.as_str()) {
            foci.push(&item.focus);
        }
    }
    let mut shown = 0;
    for focus in &foci {
        let mut section = format!("## {focus}\n");
        let note = if resumed {
            None
        } else {
            crate::context::current_note(store, ship, focus)?
        };
        if let Some(note) = note {
            section.push_str(&format!(
                "Your handoff note (v{}):\n{}\n",
                note["version"],
                note["body"].as_str().unwrap_or("")
            ));
        }
        let mut items = 0;
        for item in mail.iter().filter(|m| m.focus == *focus) {
            if text.len() + section.len() + item.summary.len() > MAX_WAKE_BYTES {
                break;
            }
            section.push_str(&item.summary);
            section.push('\n');
            items += 1;
        }
        if items == 0 {
            break;
        }
        shown += items;
        text.push_str(&section);
    }
    if shown < mail.len() {
        text.push_str(&format!(
            "...and {} more; read them with mail.inbox.\n",
            mail.len() - shown
        ));
    }
    if mail.iter().any(|m| m.crew) {
        text.push_str(&format!(
        "Mail from crew is written by the runtime: crew job outcomes from the job's record and logs, and reports from your running crew. An exit status is not proof: check results yourself (job.status, the logs, the work) before relying on them or reporting them, and record your judgement in the task's note. Answer a crew request with mail.send {{\"from\":\"{ship}\",\"to\":\"crew\",\"kind\":\"reply\",\"correlation\":...}}.\n"
        ));
    }
    text.push_str(&format!(
        "Handle this mail within your charter: answer requests with mail.send kind reply and correlation set to the request id, delegate or act as needed. For more history on a focus use focus.digest {{\"ship\":\"{ship}\",\"focus\":...}}. Before you finish, for each focus you worked on, replace its handoff note with note.put {{\"ship\":\"{ship}\",\"focus\":...,\"body\":...}}: what you are doing and why, what you are waiting on, and what comes next, in under 2 KB; facts are already recorded. Then acknowledge with mail.ack {{\"address\":\"{ship}\",\"through_seq\":{through}}}."
    ));
    Ok(text)
}

/// Normalize the token usage a harness printed as JSON lines on stdout.
/// Codex and native guards report per turn (`turn.completed` or a failed
/// native turn), Claude a cumulative `result`; otherwise use the last object.
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
            Some("turn.completed" | "native.turn.failed") => {
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
        // The wake is recorded before its job starts; a crash in between
        // leaves no job to wait for.
        let exists: bool = store.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM jobs WHERE id=?1)",
            [&job_id],
            |r| r.get(0),
        )?;
        if !exists {
            store.connection().execute(
                "UPDATE wakes SET status='failed', finished_ms=?2, error=?3 WHERE id=?1",
                params![wake, Store::now_ms(), "first mate job was never started"],
            )?;
            settled.push(json!({"wake": wake, "status": "failed", "usage": null}));
            continue;
        }
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
        let text = std::fs::read_to_string(log).unwrap_or_default();
        let reported = usage(&text);
        let outcome = if status == "succeeded" {
            "succeeded"
        } else {
            "failed"
        };
        bind_session(store, &wake, outcome, &text)?;
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

/// After a wake, keep the focus's native session for warm resumes: a
/// single-focus wake binds the session its harness reported (a fresh one
/// replaces the old, a resumed one counts another wake); a failed wake drops
/// the focus's session so the next wake starts fresh.
fn bind_session(store: &Store, wake: &str, outcome: &str, log: &str) -> Result<()> {
    let (ship, focuses, harness, requested): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = store.connection().query_row(
        "SELECT ship, focuses, harness, session_id FROM wakes WHERE id=?1",
        [wake],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let focuses: Vec<String> = focuses
        .and_then(|f| serde_json::from_str(&f).ok())
        .unwrap_or_default();
    let [focus] = focuses.as_slice() else {
        return Ok(());
    };
    if outcome != "succeeded" {
        store.connection().execute(
            "DELETE FROM sessions WHERE ship=?1 AND focus=?2",
            params![ship, focus],
        )?;
        return Ok(());
    }
    let harness = harness.unwrap_or_default();
    let Some(session) = read_session(&harness, log) else {
        return Ok(());
    };
    let now = Store::now_ms();
    if requested.as_deref() == Some(session.as_str()) {
        store.connection().execute(
            "UPDATE sessions SET wakes=wakes+1, last_turn_ms=?3 WHERE ship=?1 AND focus=?2",
            params![ship, focus, now],
        )?;
    } else {
        store.connection().execute(
            "INSERT INTO sessions(ship, focus, harness, session_id, wakes, started_ms, last_turn_ms)
             VALUES(?1, ?2, ?3, ?4, 1, ?5, ?5)
             ON CONFLICT(ship, focus) DO UPDATE SET harness=excluded.harness,
               session_id=excluded.session_id, wakes=1, started_ms=excluded.started_ms,
               last_turn_ms=excluded.last_turn_ms",
            params![ship, focus, harness, session, now],
        )?;
    }
    store.connection().execute(
        "UPDATE wakes SET session_id=?2 WHERE id=?1",
        params![wake, session],
    )?;
    Ok(())
}

/// A focus session worth resuming: same harness, last turn inside the cache
/// window, and below the rotation limit.
fn warm_session(
    store: &Store,
    ship: &str,
    focus: &str,
    harness: &str,
    profile: &Value,
) -> Result<Option<String>> {
    let window = setting(profile, "cache_window_secs") * 1000;
    if window == 0 {
        return Ok(None);
    }
    Ok(store
        .connection()
        .query_row(
            "SELECT session_id FROM sessions WHERE ship=?1 AND focus=?2 AND harness=?3
             AND last_turn_ms >= ?4 AND wakes < ?5",
            params![
                ship,
                focus,
                harness,
                Store::now_ms() - window,
                setting(profile, "session_max_wakes")
            ],
            |r| r.get(0),
        )
        .optional()?)
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
    // The captain's interactive session is the first mate while it is
    // aboard; its hooks or inbox reads deliver the mail instead.
    if crate::presence::attended(store, &name)? {
        return Ok(json!({"ship": name, "skipped": "captain's session is aboard"}));
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
    let after = woken_through.max(acknowledged);
    // Eligibility covers the whole backlog, not just the bounded prompt
    // page. A routine-only page must advance when actionable mail follows
    // it, without claiming coverage of messages beyond that page.
    let (oldest, newest): (Option<i64>, Option<i64>) = store.connection().query_row(
        "SELECT MIN(created_ms), MAX(created_ms) FROM mail
         WHERE recipient=?1 AND COALESCE(delivery, 'received')='received' AND seq>?2
           AND (kind!='notice' OR urgency!='low')",
        params![name, after],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let (Some(oldest), Some(newest)) = (oldest, newest) else {
        return Ok(json!({"ship": name, "skipped": "nothing wake-worthy"}));
    };
    let mail = new_mail(store, &name, after)?;
    if mail.is_empty() {
        return Ok(json!({"ship": name, "skipped": "nothing wake-worthy"}));
    }
    let triggers: Vec<&Mail> = mail.iter().filter(|m| !m.routine).collect();
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
    // The primary focus is the oldest wake-worthy mail's. A wake about a task
    // the ship owns runs under that task when its charter allows a first
    // mate; anything else runs under the duty task.
    let primary = triggers
        .iter()
        .min_by_key(|m| m.seq)
        .map(|m| m.focus.clone())
        .or_else(|| mail.first().map(|m| m.focus.clone()))
        .unwrap_or_default();
    let duty = profile["duty_task"].as_str().unwrap_or("").to_owned();
    let task = primary
        .strip_prefix("task:")
        .filter(|t| store.authorize(t, "spawn").is_ok() && store.authorize(t, "implement").is_ok())
        .map(str::to_owned)
        .unwrap_or(duty);
    let mut foci: Vec<String> = vec![];
    for item in &mail {
        if !foci.contains(&item.focus) {
            foci.push(item.focus.clone());
        }
    }
    // Resume only a single focus's warm session: mixing topics in one
    // conversation is what makes long sessions costly and confused.
    let harness = harness_id(&profile);
    let resume = match foci.as_slice() {
        [focus] => warm_session(store, &name, focus, &harness, &profile)?,
        _ => None,
    };
    let text = wake_text(store, &wake, &name, &mail, through, resume.is_some())?;
    let mut start = json!({
        "id": wake, "task_id": task, "manifest": profile["manifest"],
        "options": profile["options"], "wake": text,
    });
    if let Some(session) = &resume {
        // The resumed session already holds the brief and earlier turns:
        // send only the new wake text.
        start["options"]["resume"] = json!(session);
        start["wake_only"] = json!(true);
    }
    let mode = if resume.is_some() { "resume" } else { "fresh" };
    if let Some(config) = profile.get("config") {
        start["config"] = config.clone();
    }
    // Record the wake, with its job id, before the job exists: its outcome
    // must never be mistaken for crew and wake this first mate again.
    store.connection().execute(
        "INSERT INTO wakes(id, ship, focus, through_seq, job_id, mode, status, created_ms, focuses, harness, session_id)
         VALUES(?1, ?2, ?3, ?4, ?1, ?5, 'running', ?6, ?7, ?8, ?9)",
        params![
            wake,
            name,
            primary,
            through,
            mode,
            now,
            json!(foci).to_string(),
            harness,
            resume
        ],
    )?;
    let started = crate::dispatch(store, "agent.start", start);
    let (status, error) = match &started {
        Ok(_) => ("running", None),
        Err(error) => ("failed", Some(format!("{error:#}"))),
    };
    if let Some(error) = &error {
        store.connection().execute(
            "UPDATE wakes SET status='failed', finished_ms=?2, error=?3,
               job_id=CASE WHEN EXISTS(SELECT 1 FROM jobs WHERE id=?1) THEN job_id END
             WHERE id=?1",
            params![wake, Store::now_ms(), error],
        )?;
    }
    store.audit(
        "wake.started",
        &json!({"wake": wake, "ship": name, "through_seq": through, "status": status}),
    )?;
    Ok(
        json!({"ship": name, "wake": wake, "status": status, "through_seq": through,
        "focus": primary, "task_id": task, "focuses": foci, "mode": mode, "error": error}),
    )
}

/// Local reconciliation and wakes run before bounded remote delivery, so a
/// failed or slow peer cannot hold local work behind its whole outbox.
pub fn tick(store: &mut Store) -> Result<Value> {
    tick_with_delivery_budget(store, Duration::from_millis(10_000))
}

fn tick_with_delivery_budget(store: &mut Store, budget: Duration) -> Result<Value> {
    let reconciled = crate::jobs::reconcile_unsupervised(store)?;
    let settled = settle(store)?;
    let mut considered = vec![];
    for ship in ships(store)? {
        considered.push(consider(store, &ship)?);
    }
    let delivered = crate::mail::flush_dispatcher(store, budget)?;
    Ok(
        json!({"delivered": delivered, "reconciled": reconciled, "settled": settled, "ships": considered}),
    )
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
    let delivery_budget = args
        .get("delivery_budget_ms")
        .map(|v| {
            v.as_u64()
                .filter(|ms| (1..=120_000).contains(ms))
                .context("delivery_budget_ms must be 1..120000")
        })
        .transpose()?
        .unwrap_or(10_000);
    let interval = args
        .get("interval_secs")
        .and_then(Value::as_u64)
        .unwrap_or(2)
        .clamp(1, 3600);
    loop {
        let report = tick_with_delivery_budget(store, Duration::from_millis(delivery_budget))?;
        let acted = report["settled"].as_array().is_some_and(|s| !s.is_empty())
            || report["reconciled"]
                .as_array()
                .is_some_and(|s| !s.is_empty())
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
                "SELECT id, ship, focus, through_seq, job_id, mode, status, created_ms, finished_ms, error, usage, focuses, session_id
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
                        "focuses": r.get::<_, Option<String>>(11)?
                            .and_then(|f| serde_json::from_str::<Value>(&f).ok()),
                        "session_id": r.get::<_, Option<String>>(12)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(json!({"wakes": wakes}))
        }
        _ => bail!("unknown wake command {command}"),
    }
}
