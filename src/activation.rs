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
    // A live first mate compacts its session once it has been quiet this long
    // with its context at least this full (0 seconds turns it off).
    ("quiet_compact_secs", 600, 0, 86_400),
    ("quiet_compact_percent", 40, 1, 100),
    // A hosted Claude first mate is warned in its bearings at this fullness.
    // A settled turn end that crosses the compaction fullness, busy or idle,
    // makes a resumable one /compact its conversation, and a bounded one (tool
    // bundles, which cannot resume) relaunch into a fresh session from its notes.
    ("context_warn_percent", 40, 1, 100),
    ("context_compact_percent", 50, 1, 100),
    // A hosted ship whose Herdr is unreachable waits this long, from when it
    // was first found unreachable, before mail wakes a headless first mate,
    // so a dispatcher that starts before Herdr at boot does not (0 waits not).
    ("herdr_grace_secs", crate::host::HERDR_GRACE_SECS, 0, 3600),
    // A hosted first mate's harness stops after this long idle and starts
    // again on the same conversation when needed (0 keeps it running).
    (
        "sleep_after_secs",
        crate::host::sleep::DEFAULT_AFTER_SECS,
        0,
        86_400,
    ),
    // A quiet first mate with open tasks nothing will start is mailed a
    // backlog notice at most this often, backing off when it moves nothing
    // (0 turns it off).
    (
        "backlog_alarm_secs",
        crate::scheduling::BACKLOG_ALARM_SECS,
        0,
        86_400,
    ),
    // A remote wait with no report from the far owner for this long is shown
    // as stale in the captain's needs and the backlog alarm; it never
    // resumes or faults by itself (0 shows none).
    (
        "remote_wait_stale_secs",
        crate::scheduling::REMOTE_WAIT_STALE_SECS,
        0,
        604_800,
    ),
];
const MAX_BODY_CHARS: usize = 2_000;
const MAX_WAKE_BYTES: usize = 16_384;

pub fn init(store: &Store) -> Result<()> {
    let tx = store.write_transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS wakes(id TEXT PRIMARY KEY, ship TEXT NOT NULL, focus TEXT NOT NULL,
           through_seq INTEGER NOT NULL, job_id TEXT, mode TEXT NOT NULL, status TEXT NOT NULL,
           created_ms INTEGER NOT NULL, finished_ms INTEGER, error TEXT, usage TEXT);
         CREATE INDEX IF NOT EXISTS wake_ship ON wakes(ship, created_ms);
         CREATE INDEX IF NOT EXISTS wake_job ON wakes(job_id);
         CREATE TABLE IF NOT EXISTS host_launches(ship TEXT PRIMARY KEY, holder TEXT NOT NULL,
           failures INTEGER NOT NULL, retry_ms INTEGER NOT NULL, error TEXT);
         CREATE TABLE IF NOT EXISTS hosted_block_alerts(
           ship TEXT PRIMARY KEY, blocked_since_ms INTEGER NOT NULL,
           alerted INTEGER NOT NULL DEFAULT 0 CHECK(alerted IN (0,1)));
         CREATE TABLE IF NOT EXISTS duplicate_mate_alerts(ship TEXT PRIMARY KEY,
           first_seen_ms INTEGER NOT NULL, workspaces TEXT NOT NULL, mail_id TEXT);
         CREATE TABLE IF NOT EXISTS hosted_panes(ship TEXT PRIMARY KEY, pid INTEGER NOT NULL,
           started TEXT NOT NULL, executable TEXT NOT NULL, harness_pid INTEGER,
           harness_started TEXT, recorded_ms INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS hosted_automode(ship TEXT PRIMARY KEY, incident TEXT NOT NULL,
           ms INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS hosted_untrusted(ship TEXT PRIMARY KEY, folder TEXT NOT NULL,
           ms INTEGER NOT NULL, mail_id TEXT);
         CREATE TABLE IF NOT EXISTS hosted_relaunches(ship TEXT PRIMARY KEY, holder TEXT NOT NULL,
           force INTEGER NOT NULL CHECK(force IN (0,1)), requested_ms INTEGER NOT NULL,
           expires_ms INTEGER NOT NULL);",
    )?;
    // The Claude settings digest a hosted pane launched with, for settings drift.
    let digest_column: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('hosted_panes') WHERE name='settings_digest')",
        [],
        |r| r.get(0),
    )?;
    if !digest_column {
        tx.execute_batch("ALTER TABLE hosted_panes ADD COLUMN settings_digest TEXT;")?;
    }
    // The captain approvals a hosted pane launched with, for approval drift.
    let approvals_column: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('hosted_panes') WHERE name='approvals_digest')",
        [],
        |r| r.get(0),
    )?;
    if !approvals_column {
        tx.execute_batch("ALTER TABLE hosted_panes ADD COLUMN approvals_digest TEXT;")?;
    }
    crate::host::restored::init(&tx)?;
    // A pane relaunched by an upgrade, or from sleep, starts asleep.
    let asleep_column: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('host_launches') WHERE name='start_asleep')",
        [],
        |r| r.get(0),
    )?;
    if !asleep_column {
        tx.execute_batch(
            "ALTER TABLE host_launches ADD COLUMN start_asleep INTEGER NOT NULL DEFAULT 0 CHECK(start_asleep IN (0,1));",
        )?;
    }
    // The mail the retired pane had seen: mail after it wakes the pane that starts asleep.
    let through_column: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('host_launches') WHERE name='start_through')",
        [],
        |r| r.get(0),
    )?;
    if !through_column {
        tx.execute_batch("ALTER TABLE host_launches ADD COLUMN start_through INTEGER;")?;
    }
    // When a launch first found Herdr unreachable; cleared once Herdr answers.
    let unreachable_column: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('host_launches') WHERE name='unreachable_ms')",
        [],
        |r| r.get(0),
    )?;
    if !unreachable_column {
        tx.execute_batch("ALTER TABLE host_launches ADD COLUMN unreachable_ms INTEGER;")?;
    }
    for column in [
        "focuses",
        "harness",
        "session_id",
        "dispatcher_generation",
        "plan_digest",
        // The wake this one retries, so a failed wake is retried once and
        // only once.
        "retry_of",
    ] {
        let present: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('wakes') WHERE name=?1)",
            [column],
            |r| r.get(0),
        )?;
        if !present {
            tx.execute_batch(&format!("ALTER TABLE wakes ADD COLUMN {column} TEXT;"))?;
        }
    }
    let has_reason: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('wakes') WHERE name='reason')",
        [],
        |r| r.get(0),
    )?;
    if !has_reason {
        tx.execute_batch("ALTER TABLE wakes ADD COLUMN reason TEXT NOT NULL DEFAULT 'mail';")?;
    }
    // One native session per ship and focus, so topics never share a
    // conversation.
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions(ship TEXT NOT NULL, focus TEXT NOT NULL, harness TEXT NOT NULL,
           session_id TEXT NOT NULL, wakes INTEGER NOT NULL, started_ms INTEGER NOT NULL,
           last_turn_ms INTEGER NOT NULL, PRIMARY KEY(ship, focus));",
    )?;
    // A bounded session's launch plan digest; NULL for an unbounded one.
    let has_digest: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('sessions') WHERE name='plan_digest')",
        [],
        |r| r.get(0),
    )?;
    if !has_digest {
        tx.execute_batch("ALTER TABLE sessions ADD COLUMN plan_digest TEXT;")?;
    }
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

pub(crate) fn read_session(harness: &str, log: &str) -> Option<String> {
    log.lines()
        .take(10_000)
        .filter(|line| line.len() <= 65_536)
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|event| session_started(harness, &event))
}

/// The manifest's declared harness id, which selects session decoding.
pub(crate) fn harness_id(profile: &Value) -> String {
    profile["manifest"]
        .as_str()
        .and_then(|reference| crate::bundled::resolve(reference).ok())
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
            let reference = if key == "manifest" {
                crate::bundled::normalize(path)?
            } else {
                path.to_owned()
            };
            let resolved = if key == "manifest" {
                crate::bundled::resolve(&reference)?
            } else {
                reference.clone().into()
            };
            ensure!(
                resolved.is_absolute() && resolved.is_file(),
                "first_mate.{key} must be an existing absolute file or bundled manifest"
            );
            out.insert(key.into(), json!(reference));
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
    if let Some(selected) = object.get("tool_bundles") {
        crate::tool_bundles::selection(selected)?;
        let harness = match out["manifest"].as_str() {
            Some("bundled:pi") => "pi",
            Some("bundled:claude") => "claude",
            _ => bail!("bounded tool bundles require bundled:pi or bundled:claude"),
        };
        crate::tool_bundles::resolve(
            store,
            selected,
            "first_mate",
            harness,
            crate::plugins::Wait::Declared,
        )?;
        out.insert("tool_bundles".into(), selected.clone());
    }
    if let Some(host) = object.get("host").filter(|v| !v.is_null()) {
        out.insert("host".into(), crate::host::validate(host)?);
    }
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
pub(crate) fn ships(store: &Store) -> Result<Vec<Value>> {
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

#[derive(Clone)]
struct Mail {
    seq: i64,
    routine: bool,
    crew: bool,
    id: String,
    focus: String,
    summary: String,
}

/// A wake attempt: the last one for a ship, as the retry rules read it.
struct Last {
    id: String,
    status: String,
    finished_ms: Option<i64>,
    job_id: Option<String>,
    /// The wake this one is itself a retry of.
    retry_of: Option<String>,
    created_ms: i64,
}

/// A failed wake to retry at once on the fallback, with the requests it had
/// already answered.
struct Retry {
    /// The wake being retried.
    wake: String,
    /// Its classified failure, for the retry to read.
    error: String,
    /// The route that failed, when known.
    failed_route: Option<String>,
    /// The requests that attempt answered, by id.
    answered: Vec<String>,
}

/// A finished job's stdout, read through its job record for the incarnation
/// that ran. A log that cannot be read is empty, which says nothing about the
/// route and never makes a wake look retried.
fn job_log(store: &mut Store, job_id: &str) -> Result<String> {
    // A pruned job record has no log to read, so it reads as an empty one.
    let Ok(job) = job_status(store, job_id) else {
        return Ok(String::new());
    };
    let path = store
        .home()
        .join("jobs")
        .join(job_id)
        .join(job["incarnation"].as_str().unwrap_or(""))
        .join("stdout.log");
    // A bounded log can be cut inside a UTF-8 character; keep the rest.
    Ok(std::fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default())
}

/// Whether the last wake failed on something another route may do better: a
/// provider limit or an outage, read out of its log, and not a wake that has
/// already been retried or is itself a retry. A task error leaves no wake
/// retried: the same first mate with the same brief would fail the same way,
/// so it waits out the ordinary backoff instead.
fn retryable(store: &mut Store, ship: &str, last: &Last) -> Result<Option<Retry>> {
    if last.status != "failed" || last.retry_of.is_some() {
        return Ok(None);
    }
    let already: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM wakes WHERE retry_of=?1)",
        [&last.id],
        |r| r.get(0),
    )?;
    if already {
        return Ok(None);
    }
    let Some(job_id) = &last.job_id else {
        return Ok(None);
    };
    let Ok(log) = job_log(store, job_id) else {
        return Ok(None);
    };
    let Some(error) = crate::routes::throttle(&log) else {
        return Ok(None);
    };
    let failed_route = job_status(store, job_id).ok().and_then(|job| {
        job["model_route"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| crate::routes::route_of_argv(&job["command"]))
    });
    // What that attempt already did to this mailbox is a durable fact in it,
    // not a reading of a transcript: a reply marks the request it answers
    // answered, so both come from one place.
    let mut statement = store.connection().prepare(
        "SELECT correlation FROM mail WHERE sender=?1 AND kind='reply'
           AND correlation IS NOT NULL AND created_ms>=?2 ORDER BY seq",
    )?;
    let answered = statement
        .query_map(params![ship, last.created_ms], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(Some(Retry {
        wake: last.id.clone(),
        error,
        failed_route,
        answered,
    }))
}

/// Mail for a ship newer than both its acknowledgement and its last wake,
/// each labelled with the focus it belongs to. A notice in the thread of a
/// request this ship still has open is not routine: it corrects the
/// follow-up, and the wake names that request.
fn new_mail(store: &Store, ship: &str, after: i64) -> Result<Vec<Mail>> {
    let mut statement = store.connection().prepare(&format!(
        "SELECT seq, id, sender, kind, correlation, request_kind, urgency, body, payload, task_id,
            NOT ({}) AS actionable, {}
         FROM mail WHERE recipient=?1 AND COALESCE(delivery, 'received')='received' AND seq > ?2
         ORDER BY seq LIMIT 200",
        crate::mail::ROUTINE_MAIL,
        crate::mail::OPEN_FOLLOW_UP
    ))?;
    let rows = statement.query_map(params![ship, after], |r| {
        let kind: String = r.get(3)?;
        let urgency: String = r.get(6)?;
        let body: String = r.get(7)?;
        let body: String = body.chars().take(MAX_BODY_CHARS).collect();
        let payload: String = r.get(8)?;
        let id: String = r.get(1)?;
        let from: String = r.get(2)?;
        let correlation: Option<String> = r.get(4)?;
        let follow_up: Option<String> = r.get(11)?;
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
        if let Some(follow_up) = &follow_up {
            summary.push_str(&format!(
                "\n  the mail is in the thread of a request that your open request {follow_up} grows from; \
                 answer it in that thread, or withdraw or update it with mail.withdraw or mail.send"
            ));
        }
        // The captain's files are named, sized and placed where the ship
        // reads them; no other sender's mail carries them.
        summary.push_str(&payload_lines(&from, &payload));
        let message = json!({"id": id, "from": from, "correlation": correlation,
            "task_id": r.get::<_, Option<String>>(9)?});
        Ok((
            Mail {
                seq: r.get(0)?,
                // Information-only notices wait for the next real wake.
                routine: !r.get::<_, bool>(10)?,
                crew: from == crate::mail::CREW,
                id,
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

/// The payload part of one mail item's summary: the files a captain's mail
/// carries, one line each, and the payload line for what is left. Files are
/// the captain's alone, and they never ride the generic payload line, or a
/// small mail would show them twice.
fn payload_lines(from: &str, payload: &str) -> String {
    let mut lines = String::new();
    let mut rest = payload.to_string();
    if payload.contains("\"files\"")
        && let Ok(parsed) = serde_json::from_str::<Value>(payload)
    {
        if from == crate::mail::CAPTAIN {
            lines.push_str(&crate::files::summary(&parsed));
        }
        let mut without = parsed;
        if let Some(fields) = without.as_object_mut() {
            fields.remove("files");
        }
        rest = without.to_string();
    }
    // A crew report's body already states its payload.
    if from != crate::mail::CREW && rest != "{}" && rest.len() <= 1024 {
        lines.push_str(&format!("\n  payload {rest}"));
    }
    lines
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
    retry: Option<&Retry>,
) -> Result<String> {
    let mut text = format!("Wake {wake} for ship {ship}.\n");
    // A resumed session already has its bearings and notes in context.
    if !resumed {
        let bearings = crate::context::bearings(store, ship)?;
        text.push_str(bearings["text"].as_str().unwrap_or(""));
    }
    // A retry says what it is retrying and what is already done, so the first
    // mate does not answer a request twice or read its own past reply as new.
    if let Some(retry) = retry {
        text.push_str(&format!(
            "\nThis is one retry of wake {}, which the provider ended with: {}. \
             It has already answered: {}. Those requests are left out below and \
             need no answer; check bearings for crew or notices the failed attempt \
             already started. This attempt skips the one-minute wait and the \
             hourly wake budget, and the next one, if any, waits as usual.\n",
            retry.wake,
            retry.error,
            if retry.answered.is_empty() {
                "nothing".to_owned()
            } else {
                retry.answered.join(", ")
            }
        ));
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
        // Pins change between wakes and are short, so even a resumed
        // session sees the current ones.
        let pins = crate::context::pins(store, ship, focus)?;
        if let Some(pins) = pins.as_object().filter(|p| !p.is_empty()) {
            let line: Vec<String> = pins
                .iter()
                .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
                .collect();
            section.push_str(&format!("Pinned: {}\n", line.join(", ")));
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
        "Handle this mail within your charter: answer requests with mail.send kind reply and correlation set to the request id, delegate or act as needed. Anything about a request goes in that request's thread: its answer, a correction, or \"no longer needed\" all carry correlation set to the request id, so a low notice in the thread is not lost; when you raise a request because of another one, set because_of to the id of the request it grows from. For more history on a focus use focus.digest {{\"ship\":\"{ship}\",\"focus\":...}}. Before you finish, for each focus you worked on, replace its handoff note with note.put {{\"ship\":\"{ship}\",\"focus\":...,\"body\":...}}: what you are doing and why, what you are waiting on, and what comes next, in under 2 KB; facts are already recorded. Then acknowledge with mail.ack {{\"address\":\"{ship}\",\"through_seq\":{through}}}."
    ));
    Ok(text)
}

/// Normalize the token usage a harness printed as JSON lines on stdout.
/// Codex and native guards (Pi, Claude, Gemini, Cursor, AGY) report per turn
/// (`turn.completed` or a failed native turn); an unguarded stream's final
/// `result` is taken as reported; otherwise use the last object.
pub(crate) fn usage(log: &str) -> Option<Value> {
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

/// Whether a first mate's own route cannot take a wake now: it is held off, or
/// its plan is past the share this route keeps for first mates. A first mate
/// moves to its fallback on either, in a fresh session.
pub(crate) fn own_route_blocked(store: &Store, profile: &Value) -> Result<bool> {
    let Some(route) = profile["manifest"]
        .as_str()
        .and_then(|manifest| crate::routes::route_of(manifest, &profile["options"]))
    else {
        return Ok(false);
    };
    let limits = crate::crew_routes::limits_of(store, &route)?;
    Ok(crate::routes::refusal_for(store, &route, crate::routes::Who::FirstMate, limits)?.is_some())
}

/// Record the outcome of wakes whose first mate has finished.
fn settle(store: &mut Store) -> Result<Vec<Value>> {
    let running: Vec<(String, String)> = {
        let mut statement = store.connection().prepare(
            "SELECT id, job_id FROM wakes WHERE status='running' AND job_id IS NOT NULL
                AND (reason<>'work' OR EXISTS(SELECT 1 FROM jobs WHERE jobs.id=wakes.job_id))",
        )?;
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
        // A bounded log can be cut inside a UTF-8 character; keep the rest.
        let text = std::fs::read(log)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
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
    // An adopted wake may finish after the new dispatcher starts. It still
    // covers its mail, but must not restore (or delete) the new generation's
    // native warmth. NULL generations are the legacy/pre-service epoch.
    let current: bool = store.connection().query_row(
        "SELECT COALESCE(dispatcher_generation, '') = COALESCE(
           (SELECT value FROM metadata WHERE key='dispatcher_generation'), '')
         FROM wakes WHERE id=?1",
        [wake],
        |r| r.get(0),
    )?;
    if !current {
        return Ok(());
    }
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
            "INSERT INTO sessions(ship, focus, harness, session_id, wakes, started_ms, last_turn_ms, plan_digest)
             VALUES(?1, ?2, ?3, ?4, 1, ?5, ?5, (SELECT plan_digest FROM wakes WHERE id=?6))
             ON CONFLICT(ship, focus) DO UPDATE SET harness=excluded.harness,
               session_id=excluded.session_id, wakes=1, started_ms=excluded.started_ms,
               last_turn_ms=excluded.last_turn_ms, plan_digest=excluded.plan_digest",
            params![ship, focus, harness, session, now, wake],
        )?;
    }
    store.connection().execute(
        "UPDATE wakes SET session_id=?2 WHERE id=?1",
        params![wake, session],
    )?;
    Ok(())
}

/// A focus session worth resuming: same harness and launch plan digest (none
/// when unbounded), last turn inside the cache window, and below the rotation
/// limit.
pub(crate) fn warm_session(
    store: &Store,
    ship: &str,
    focus: &str,
    harness: &str,
    profile: &Value,
    digest: Option<&str>,
) -> Result<Option<String>> {
    let window = setting(profile, "cache_window_secs") * 1000;
    if window == 0 {
        return Ok(None);
    }
    Ok(store
        .connection()
        .query_row(
            "SELECT session_id FROM sessions WHERE ship=?1 AND focus=?2 AND harness=?3
             AND last_turn_ms >= ?4 AND wakes < ?5 AND plan_digest IS ?6",
            params![
                ship,
                focus,
                harness,
                Store::now_ms() - window,
                setting(profile, "session_max_wakes"),
                digest
            ],
            |r| r.get(0),
        )
        .optional()?)
}

/// Decide whether a ship's first mate is due, and wake it if so.
fn consider(store: &mut Store, ship: &Value) -> Result<Value> {
    let name = ship["name"].as_str().context("ship name")?.to_owned();
    let profile = ship["first_mate"].clone();
    if crate::permissions::paused(store, &name)? {
        return Ok(
            json!({"ship":name,"skipped":"tool permission question awaits captain or a policy change"}),
        );
    }
    let running: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM wakes WHERE ship=?1 AND status IN ('offered','running'))",
        [&name],
        |r| r.get(0),
    )?;
    if running {
        return Ok(json!({"ship": name, "skipped": "first mate already awake"}));
    }
    // The captain's interactive session is the first mate while it is
    // aboard; its hooks or inbox reads deliver the mail instead.
    if crate::presence::attended(store, &name)? {
        let skipped = if crate::presence::hosted(store, &name)? {
            "its live first mate has the mail"
        } else {
            "captain's session is aboard"
        };
        return Ok(json!({"ship": name, "skipped": skipped}));
    }
    // Maintaining an interactive first mate is independent of mail batching,
    // wake budgets and prompt construction. Idle and mail-driven launches use
    // one durable admission path, before the headless fallback below.
    let mut host_error = None;
    let mut host_backoff_until_ms = None;
    let mut own_blocked = None;
    // While the configured route is backing off, use headless fallback wakes.
    // Keeping the hosted session on its own native route avoids changing host adapter
    // identity or inheriting the ship's route-specific config/tool bundles.
    if crate::host::hosted(&profile) {
        match own_route_blocked(store, &profile) {
            Ok(blocked) => own_blocked = Some(blocked),
            Err(error) => host_error = Some(format!("checking first-mate route: {error:#}")),
        }
    }
    if crate::host::hosted(&profile) && own_blocked == Some(false) {
        match crate::host::launch(store, &name, &profile) {
            Ok(crate::host::Attempt::Launched(hosted)) => {
                return Ok(json!({"ship": name, "hosted": hosted}));
            }
            Ok(crate::host::Attempt::Backoff(until)) => host_backoff_until_ms = Some(until),
            Ok(crate::host::Attempt::Busy) => {
                return Ok(json!({"ship": name, "skipped": "first mate already aboard or awake"}));
            }
            Err(error) => host_error = Some(format!("{error:#}")),
        }
    }
    headless_wake(
        store,
        name,
        profile,
        host_error,
        host_backoff_until_ms,
        own_blocked,
        true,
    )
}

/// The headless wake of a ship that its hosted first mate did not take.
/// `retry_allowed` is false once a failed wake's retry has found no route to
/// take: that wake then runs as any other, with its waits and its whole mail.
fn headless_wake(
    store: &mut Store,
    name: String,
    mut profile: Value,
    host_error: Option<String>,
    host_backoff_until_ms: Option<i64>,
    mut own_blocked: Option<bool>,
    retry_allowed: bool,
) -> Result<Value> {
    let now = Store::now_ms();
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
        &format!(
            "SELECT MIN(created_ms), MAX(created_ms) FROM mail
         WHERE recipient=?1 AND COALESCE(delivery, 'received')='received' AND seq>?2
           AND NOT ({})",
            crate::mail::ROUTINE_MAIL
        ),
        params![name, after],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let (Some(oldest), Some(newest)) = (oldest, newest) else {
        return Ok(json!({"ship": name, "skipped": "nothing wake-worthy",
            "host_error": host_error, "host_backoff_until_ms": host_backoff_until_ms}));
    };
    let mail = new_mail(store, &name, after)?;
    if mail.is_empty() {
        return Ok(json!({"ship": name, "skipped": "nothing wake-worthy"}));
    }
    // Herdr may be starting after this dispatcher, as at boot: give it a
    // grace before a headless first mate takes mail its live session would.
    if own_blocked == Some(false) {
        // Ships recorded before the setting existed wait too.
        let grace = profile["herdr_grace_secs"]
            .as_i64()
            .unwrap_or(crate::host::HERDR_GRACE_SECS as i64)
            * 1000;
        if let Some(until) = crate::host::awaiting_herdr(store, &name, grace)? {
            return Ok(
                json!({"ship": name, "skipped": "waiting for Herdr before a headless wake",
                "herdr_grace_until_ms": until, "host_error": host_error,
                "host_backoff_until_ms": host_backoff_until_ms}),
            );
        }
    }
    let quiet = now - newest >= setting(&profile, "batch_secs") * 1000;
    let overdue = now - oldest >= setting(&profile, "max_delay_secs") * 1000;
    if !quiet && !overdue {
        return Ok(json!({"ship": name, "skipped": "batching"}));
    }
    let last: Option<Last> = store
        .connection()
        .query_row(
            // A backlog alarm's record row is not a wake attempt; it must not
            // hide a recent failure from the retry backoff.
            "SELECT id, status, finished_ms, job_id, retry_of, created_ms FROM wakes
             WHERE ship=?1 AND reason<>'backlog' ORDER BY created_ms DESC LIMIT 1",
            [&name],
            |r| {
                Ok(Last {
                    id: r.get(0)?,
                    status: r.get(1)?,
                    finished_ms: r.get(2)?,
                    job_id: r.get(3)?,
                    retry_of: r.get(4)?,
                    created_ms: r.get(5)?,
                })
            },
        )
        .optional()?;
    // A wake the provider refused is retried once, at once, on the fallback,
    // rather than waiting out the backoff on a route that just failed. A wake
    // that failed for its own reasons keeps its mail and waits as it always
    // did: only a limit or an outage says another route may do better.
    let retry = match &last {
        Some(last) if retry_allowed => retryable(store, &name, last)?,
        _ => None,
    };
    let last_failure = last
        .as_ref()
        .filter(|last| last.status == "failed")
        .and_then(|last| last.finished_ms);
    if retry.is_none()
        && last_failure.is_some_and(|at| now - at < setting(&profile, "retry_backoff_secs") * 1000)
    {
        return Ok(json!({"ship": name, "skipped": "backing off after a failed wake"}));
    }
    let recent: i64 = store.connection().query_row(
        "SELECT COUNT(*) FROM wakes WHERE ship=?1 AND created_ms > ?2",
        params![name, now - 3_600_000],
        |r| r.get(0),
    )?;
    if retry.is_none() && recent >= setting(&profile, "max_wakes_per_hour") {
        return Ok(json!({"ship": name, "skipped": "hourly wake budget spent"}));
    }
    // The retry covers the same mail as the attempt it replaces, including the
    // requests that attempt answered: a wake covers mail through its sequence
    // whether or not this first mate read it, so nothing is asked twice.
    let through = mail.iter().map(|m| m.seq).max().unwrap_or(0);
    let mail: Vec<Mail> = match &retry {
        Some(retry) => mail
            .iter()
            .filter(|item| !retry.answered.contains(&item.id))
            .cloned()
            .collect::<Vec<Mail>>(),
        None => mail,
    };
    if mail.is_empty() {
        return Ok(json!({"ship": name, "skipped": "nothing left to retry"}));
    }
    let triggers: Vec<&Mail> = mail.iter().filter(|m| !m.routine).collect();
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
        .filter(|t| store.authorize(t, "spawn").is_ok())
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
    let mut harness = harness_id(&profile);
    let digest = crate::tool_bundles::session_digest(store, &profile);
    let mut resume = match foci.as_slice() {
        [focus] if profile.get("tool_bundles").is_none() || digest.is_some() => {
            warm_session(store, &name, focus, &harness, &profile, digest.as_deref())?
        }
        _ => None,
    };
    // A headless wake on a route that cannot take it would only fail. Try the
    // captain's first_mate order; if it has no clear route, preserve the
    // existing wait. A retry goes there whether or not its own route is clear,
    // because the route that just failed is the one it is moving off.
    if own_blocked.is_none() {
        own_blocked = Some(own_route_blocked(store, &profile)?);
    }
    if retry.is_some() || own_blocked == Some(true) {
        let fallback = match &retry {
            Some(retry) => crate::crew_routes::retry_fallback(
                store,
                &name,
                &task,
                &profile,
                retry.failed_route.as_deref(),
            )?,
            None => crate::crew_routes::first_mate_fallback(store, &name, &task, &profile)?,
        };
        let Some(fallback) = fallback else {
            // A retry with no route to move to is no retry: the wake waits as
            // it does without a failure, through the backoff, the hourly budget
            // and the whole mail. Nothing was recorded, so this runs once.
            if retry.is_some() {
                return headless_wake(
                    store,
                    name,
                    profile,
                    host_error,
                    host_backoff_until_ms,
                    own_blocked,
                    false,
                );
            }
            return Ok(json!({
                "ship": name,
                "skipped": "model route backing off or past its headroom"
            }));
        };
        profile["manifest"] = fallback.manifest;
        profile["options"] = fallback.options;
        profile.as_object_mut().unwrap().remove("config");
        profile.as_object_mut().unwrap().remove("tool_bundles");
        // A resumed session belongs to the configured route's harness. The
        // fallback must start fresh under its own harness and route options.
        resume = None;
        harness = harness_id(&profile);
    }
    let text = wake_text(
        store,
        &wake,
        &name,
        &mail,
        through,
        resume.is_some(),
        retry.as_ref(),
    )?;
    let mut start = json!({
        "id": wake, "task_id": task, "manifest": profile["manifest"],
        "options": profile["options"], "wake": text,
    });
    if let Some(selected) = profile.get("tool_bundles") {
        start["tool_bundles"] = selected.clone();
    }
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
        "INSERT INTO wakes(id, ship, focus, through_seq, job_id, mode, status, created_ms, focuses, harness, session_id, retry_of, dispatcher_generation)
         VALUES(?1, ?2, ?3, ?4, ?1, ?5, 'running', ?6, ?7, ?8, ?9, ?10,
           (SELECT value FROM metadata WHERE key='dispatcher_generation'))",
        params![
            wake,
            name,
            primary,
            through,
            mode,
            now,
            json!(foci).to_string(),
            harness,
            resume,
            retry.as_ref().map(|retry| retry.wake.clone())
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
        &json!({"wake": wake, "ship": name, "through_seq": through, "status": status,
            "retries": retry.as_ref().map(|retry| retry.wake.clone())}),
    )?;
    Ok(
        json!({"ship": name, "wake": wake, "status": status, "through_seq": through,
        "focus": primary, "task_id": task, "focuses": foci, "mode": mode, "error": error,
        "retry_of": retry.as_ref().map(|retry| retry.wake.clone()),
        "host_error": host_error, "host_backoff_until_ms": host_backoff_until_ms}),
    )
}

/// Local reconciliation and wakes run before bounded remote delivery, so a
/// failed or slow peer cannot hold local work behind its whole outbox.
pub fn tick(store: &mut Store) -> Result<Value> {
    tick_with_delivery_budget(store, Duration::from_millis(10_000))
}

fn tick_with_delivery_budget(store: &mut Store, budget: Duration) -> Result<Value> {
    // Before any space opens, so a restored copy is not counted as one.
    let restored = crate::host::restored::sweep(store);
    let bridge = crate::host::bridge::maintain(store).unwrap_or_else(
        |error| json!({"error": format!("{error:#}").chars().take(2_000).collect::<String>()}),
    );
    let hosted_observations = crate::host::repair(store);
    let hosted_automode = crate::automode::alert(store, &hosted_observations).map_or_else(
        |error| json!({"error": format!("{error:#}").chars().take(2_000).collect::<String>()}),
        |raised| json!(raised),
    );
    let hosted_blocked = crate::host::alert_blocked(store, &hosted_observations).unwrap_or_else(
        |error| json!({"error": format!("{error:#}").chars().take(2_000).collect::<String>()}),
    );
    let duplicate_mates = crate::host::check_duplicate_mates(store).unwrap_or_else(
        |error| json!({"error": format!("{error:#}").chars().take(2_000).collect::<String>()}),
    );
    let reconciled = crate::jobs::reconcile_unsupervised(store)?;
    // Asks a policy change covers end, even when it was made in another home.
    let asks_ended = crate::block_ask::sweep(store).unwrap_or_else(|error| {
        eprintln!("privateer: could not sweep tool-permission asks: {error:#}");
        vec![]
    });
    crate::crew::resume_pending(store)?;
    let settled = settle(store)?;
    let errands = crate::errand::settle(store)?;
    let routes = crate::routes::scan(store)?;
    // After the scan, so the throttled route is already held and the relaunch
    // takes the next one in its list.
    let relaunched = crate::crew_relaunch::throttled(store)?;
    let work = crate::scheduling::tick(store)?;
    // Before the wakes, so a headless ship's wake takes the notice this tick.
    let backlog = crate::scheduling::backlog_tick(store)?;
    let mut considered = vec![];
    for ship in ships(store)? {
        considered.push(match consider(store, &ship) {
            Ok(report) => report,
            Err(error) => json!({
                "ship": ship["name"],
                "error": format!("{error:#}").chars().take(2_000).collect::<String>()
            }),
        });
    }
    // Untrusted folders raised by this pass's launches are reported here too.
    let hosted_blocked = crate::host::with_untrusted(store, hosted_blocked);
    let delivered = crate::mail::flush_dispatcher(store, budget)?;
    // Mail other machines hold for ships here, which they cannot send.
    let machines = crate::machines::tick(store.home(), budget);
    // This machine's own problems, for the ship that looks after it.
    let owner_alerts = crate::machines::alert_owner(store);
    // Approved placement moves this machine is the one to run.
    let placement = crate::placement::tick(store);
    // Landed work frees its checkout without waking anyone.
    let workspaces = crate::workspace::sweep(store)
        .unwrap_or_else(|error| vec![json!({"error": format!("{error:#}")})]);
    Ok(
        json!({"delivered": delivered, "machines": machines, "owner_alerts": owner_alerts, "placement": placement, "hosted_blocked": hosted_blocked, "hosted_automode": hosted_automode, "duplicate_mates": duplicate_mates, "reconciled": reconciled, "asks_ended": asks_ended, "settled": settled, "errands": errands, "routes": routes, "relaunched": relaunched, "work": work, "backlog": backlog, "ships": considered, "workspaces": workspaces, "bridge": bridge, "restored": restored}),
    )
}

/// Run the dispatcher. Only one runs per home, guarded by an exclusive lock
/// on `<home>/serve.lock` held for the life of the process.
pub fn serve(store: &mut Store, args: &Value) -> Result<()> {
    let _ = std::env::set_current_dir(store.home());
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
    // Only a resident service start is a dispatcher generation boundary.
    // `serve --once` is a maintenance pass, not a restart: it must neither
    // destroy warmth on every tick nor change the epoch of adopted wakes.
    // Do this after taking serve.lock and validating arguments, before settle.
    // A new build's instructions reach the first mate's next session. Only a
    // changed file is written, so a maintenance pass costs one read.
    match crate::cli::refresh_instructions(store) {
        Ok(true) => eprintln!("updated AGENTS.md to this build's instructions"),
        Ok(false) => {}
        Err(error) => eprintln!("could not refresh AGENTS.md: {error:#}"),
    }
    if !once {
        let tx = store.write_transaction()?;
        tx.execute("DELETE FROM sessions", [])?;
        tx.execute(
            "INSERT INTO metadata(key, value) VALUES('dispatcher_generation', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [uuid::Uuid::new_v4().to_string()],
        )?;
        tx.commit()?;
    }
    loop {
        let report = tick_with_delivery_budget(store, Duration::from_millis(delivery_budget))?;
        let acted = report["work"].as_array().is_some_and(|w| !w.is_empty())
            || report["settled"].as_array().is_some_and(|s| !s.is_empty())
            || report["reconciled"]
                .as_array()
                .is_some_and(|s| !s.is_empty())
            || report["delivered"]["delivered"].as_u64().unwrap_or(0) > 0
            || report["backlog"]
                .as_array()
                .is_some_and(|b| b.iter().any(|r| r.get("fired").is_some()))
            || report["workspaces"]
                .as_array()
                .is_some_and(|w| !w.is_empty())
            || report["ships"].as_array().is_some_and(|s| {
                s.iter()
                    .any(|r| r.get("wake").is_some() || r.get("hosted").is_some())
            });
        if once || acted {
            println!("{}", serde_json::to_string(&report)?);
        }
        if once {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}

pub(crate) fn idle_duration(ms: i64) -> String {
    let minutes = (ms / 60_000).max(0);
    if minutes < 60 {
        format!("{minutes}m")
    } else {
        format!("{}h {}m", minutes / 60, minutes % 60)
    }
}

fn wake_output_idle(store: &Store, job_id: &str) -> Option<i64> {
    let (incarnation, spawned_ms): (Option<String>, Option<i64>) = store
        .connection()
        .query_row(
            "SELECT json_extract(record, '$.incarnation'), json_extract(record, '$.spawned_ms') FROM jobs WHERE id=?1",
            [job_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .ok()
        .flatten()?;
    let spawned_ms = spawned_ms?;
    let log_path = match incarnation {
        Some(inc) => store
            .home()
            .join("jobs")
            .join(job_id)
            .join(inc)
            .join("stdout.log"),
        None => store.home().join("jobs").join(job_id).join("stdout.log"),
    };
    let now = Store::now_ms();
    match std::fs::metadata(&log_path) {
        Ok(meta) => {
            if meta.len() == 0 {
                Some(now.saturating_sub(spawned_ms))
            } else {
                let modified_ms = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(spawned_ms);
                Some(now.saturating_sub(modified_ms))
            }
        }
        Err(_) => Some(now.saturating_sub(spawned_ms)),
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
                    let id = r.get::<_, String>(0)?;
                    let ship = r.get::<_, String>(1)?;
                    let focus = r.get::<_, String>(2)?;
                    let through_seq = r.get::<_, i64>(3)?;
                    let job_id = r.get::<_, Option<String>>(4)?;
                    let raw_mode = r.get::<_, String>(5)?;
                    let status = r.get::<_, String>(6)?;
                    let created_ms = r.get::<_, i64>(7)?;
                    let finished_ms = r.get::<_, Option<i64>>(8)?;
                    let error = r.get::<_, Option<String>>(9)?;
                    let usage: Option<String> = r.get(10)?;
                    let focuses: Option<String> = r.get(11)?;
                    let session_id = r.get::<_, Option<String>>(12)?;
                    let idle_ms = if status == "running" {
                        job_id
                            .as_deref()
                            .and_then(|jid| wake_output_idle(store, jid))
                    } else {
                        None
                    };
                    let mut wake = json!({
                        "id": id, "ship": ship,
                        "focus": focus, "through_seq": through_seq,
                        "job_id": job_id, "mode": raw_mode,
                        "status": status, "created_ms": created_ms,
                        "finished_ms": finished_ms, "error": error,
                        "usage": usage.and_then(|u| serde_json::from_str::<Value>(&u).ok()),
                        "focuses": focuses.and_then(|f| serde_json::from_str::<Value>(&f).ok()),
                        "session_id": session_id,
                    });
                    if let Some(idle) = idle_ms {
                        wake["output_idle_ms"] = json!(idle);
                    }
                    Ok(wake)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(json!({"wakes": wakes}))
        }
        _ => bail!("unknown wake command {command}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn stalled_running_wake_is_not_called_fresh() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let mut store = Store::open(&home).unwrap();
        let now = Store::now_ms();
        let thirty_six_mins_ago = now - 36 * 60 * 1000;

        crate::jobs::init(&store).unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO jobs(id, record) VALUES(?1, ?2)",
                params![
                    "job-stalled",
                    json!({"id": "job-stalled", "incarnation": "inc-1", "spawned_ms": thirty_six_mins_ago}).to_string()
                ],
            )
            .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO wakes(id, ship, focus, through_seq, job_id, mode, status, created_ms)
                 VALUES('wake-stalled', 'art', 'task:art-1', 1, 'job-stalled', 'fresh', 'running', ?1)",
                [thirty_six_mins_ago],
            )
            .unwrap();

        // Wake whose job is waiting for capacity (spawned_ms is None)
        let ten_mins_ago = now - 10 * 60 * 1000;
        store
            .connection()
            .execute(
                "INSERT INTO jobs(id, record) VALUES(?1, ?2)",
                params![
                    "job-queued",
                    json!({"id": "job-queued", "spawned_ms": Value::Null}).to_string()
                ],
            )
            .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO wakes(id, ship, focus, through_seq, job_id, mode, status, created_ms)
                 VALUES('wake-waiting', 'art', 'task:art-3', 3, 'job-queued', 'fresh', 'running', ?1)",
                [ten_mins_ago],
            )
            .unwrap();

        // Fresh wake started 5 seconds ago with no job yet
        store
            .connection()
            .execute(
                "INSERT INTO wakes(id, ship, focus, through_seq, job_id, mode, status, created_ms)
                 VALUES('wake-fresh', 'art', 'task:art-2', 2, NULL, 'fresh', 'running', ?1)",
                [now - 5_000],
            )
            .unwrap();

        let res = dispatch(&mut store, "wake.list", json!({"ship": "art"})).unwrap();
        let wakes = res["wakes"].as_array().unwrap();
        assert_eq!(wakes.len(), 3);

        // wake-fresh was created 5s ago: mode stays "fresh", no output_idle_ms
        assert_eq!(wakes[0]["id"], "wake-fresh");
        assert_eq!(wakes[0]["mode"], "fresh");
        assert!(wakes[0].get("output_idle_ms").is_none());

        // wake-waiting is waiting for capacity: mode stays "fresh", shows no idle text
        assert_eq!(wakes[1]["id"], "wake-waiting");
        assert_eq!(wakes[1]["mode"], "fresh");
        assert!(wakes[1].get("output_idle_ms").is_none());

        // wake-stalled spawned 36m ago: mode stays "fresh", output_idle_ms records idle duration
        assert_eq!(wakes[2]["id"], "wake-stalled");
        assert_eq!(wakes[2]["mode"], "fresh");
        let idle = wakes[2]["output_idle_ms"].as_i64().unwrap();
        assert!(idle >= 36 * 60 * 1000);
        assert_eq!(idle_duration(idle), "36m");
    }

    #[test]
    fn the_wake_names_files_only_on_the_captains_mail_and_once() {
        let files = r#"{"about":"charts","files":[{"file_id":"file-1","name":"plan.md",
            "bytes":4,"type":"text/markdown","path":"/ships/apps/files/file-1/plan.md"}]}"#;
        // The captain's mail: one file line, and a payload line without them.
        let captain = payload_lines("captain", files);
        assert!(
            captain.contains("file plan.md (4 bytes, text/markdown)"),
            "{captain}"
        );
        assert!(
            captain.contains("/ships/apps/files/file-1/plan.md"),
            "{captain}"
        );
        assert!(captain.contains("payload"), "{captain}");
        assert!(!captain.contains("\"files\""), "{captain}");

        // Another sender's mail shows no file line at all, whatever its
        // payload claims, and its payload line carries only the rest.
        let ship = payload_lines("apps", files);
        assert!(!ship.contains("file plan.md"), "{ship}");
        assert!(ship.contains("charts"), "{ship}");
        assert!(!ship.contains("\"files\""), "{ship}");

        // Files alone leave no payload line behind.
        let only = r#"{"files":[{"file_id":"file-1","name":"plan.md","bytes":4,
            "type":"text/markdown","path":"/p"}]}"#;
        assert_eq!(payload_lines("captain", only).matches('\n').count(), 1);
        // A crew report's body already states its payload.
        assert_eq!(payload_lines("crew", files), "");
    }
}
