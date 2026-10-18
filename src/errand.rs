//! Errands: a first mate's thinking done in a fresh context.
//!
//! A live first mate continues one thread, so whatever it reads stays in its
//! context: reviewing a crew's results by pulling their logs and sources into
//! that thread filled a 272k window in under an hour, and each compaction's
//! summary grew on the last. An errand is a short headless session for one
//! focus that reviews, investigates or catches up and ends with a verdict. It
//! starts from the focus's records, not from the first mate's thread, and the
//! runtime mails the first mate only its final reply. A follow-up errand on
//! the same focus resumes the errand's session while the prompt cache is warm,
//! and rotates after a few, as headless wakes do.
//!
//! An errand is not crew: it gets no workspace and no live pane, and runs as
//! its ship in the ship's folder. Like a wake, its own outcome is not reported
//! as a crew notice; its verdict is the report. Unlike a wake, a stow holds it.
use crate::store::{Caller, Store, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

const VERDICT_CHARS: usize = 4_000;
const BRIEF_CHARS: usize = 8_000;

pub fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS errands(id TEXT PRIMARY KEY, ship TEXT NOT NULL,
           focus TEXT NOT NULL, task_id TEXT NOT NULL, brief TEXT NOT NULL, mode TEXT NOT NULL,
           status TEXT NOT NULL, created_ms INTEGER NOT NULL, finished_ms INTEGER,
           harness TEXT, session_id TEXT, verdict TEXT, error TEXT, usage TEXT);
         CREATE INDEX IF NOT EXISTS errand_ship ON errands(ship, created_ms);",
    )?;
    let has_digest: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('errands') WHERE name='plan_digest')",
        [],
        |r| r.get(0),
    )?;
    if !has_digest {
        store
            .connection()
            .execute_batch("ALTER TABLE errands ADD COLUMN plan_digest TEXT;")?;
    }
    crate::herdr::errands::init(store)?;
    Ok(())
}

/// Whether a job is an errand still being launched or run.
pub(crate) fn running(store: &Store, job_id: &str) -> Result<bool> {
    Ok(store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM errands WHERE id=?1 AND status='running')",
        [job_id],
        |r| r.get(0),
    )?)
}

/// Whether a job is or was an errand.
pub(crate) fn is_errand(store: &Store, job_id: &str) -> Result<bool> {
    Ok(store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM errands WHERE id=?1)",
        [job_id],
        |r| r.get(0),
    )?)
}

/// The session key an errand's focus resumes under, apart from wakes'.
fn session_key(focus: &str) -> String {
    format!("errand:{focus}")
}

/// The focus's records an errand starts from: its note, pins and, for a
/// task, the crew assignment. Built from records, with no model call.
fn records(store: &Store, ship: &str, focus: &str) -> Result<String> {
    let mut text = String::new();
    if let Some(note) = crate::context::current_note(store, ship, focus)? {
        text.push_str(&format!(
            "The first mate's handoff note for {focus}:\n{}\n",
            note["body"].as_str().unwrap_or("")
        ));
    }
    let pins = crate::context::pins(store, ship, focus)?;
    if let Some(pins) = pins.as_object().filter(|p| !p.is_empty()) {
        text.push_str("Pinned facts:\n");
        for (key, value) in pins {
            text.push_str(&format!("- {key}: {}\n", value.as_str().unwrap_or("")));
        }
    }
    if let Some(("task", id)) = focus.split_once(':') {
        let task = store.task(id)?;
        text.push_str(&format!(
            "Task {id} [{}]: {}\n",
            task["status"].as_str().unwrap_or(""),
            task["title"].as_str().unwrap_or("")
        ));
        if let Some(assignment) =
            crate::context::current_note(store, ship, &format!("assignment:{id}"))?
        {
            text.push_str(&format!(
                "The crew assignment for {id}:\n{}\n",
                assignment["body"].as_str().unwrap_or("")
            ));
        }
    }
    Ok(text)
}

fn prompt(store: &Store, id: &str, ship: &str, focus: &str, brief: &str) -> Result<String> {
    Ok(format!(
        "Errand {id} for the ship {ship}, on {focus}.\n\
         You are running an errand for {ship}'s first mate: a short session, separate from its \
         own conversation, for one piece of thinking. Do this errand:\n{brief}\n\n\
         {records}\n\
         Read what you need: job status and logs, workspaces, files, focus.digest. Run builds or \
         test suites only with job.start, heavy and wait_for_capacity. Do not start crew or other \
         errands, change task status or write to the captain; the first mate decides.\n\
         Pin exact facts the first mate will need with note.pin on {focus}. End with your verdict \
         as your final reply, in a few short paragraphs: what you found, what you recommend and \
         anything uncertain. That reply is all the first mate receives.",
        records = records(store, ship, focus)?
    ))
}

/// `errand.start {focus, brief}`: run one errand for the caller's ship.
fn start(store: &mut Store, args: &Value) -> Result<Value> {
    let ship = match store.caller() {
        Caller::Agent { actor } => actor.clone(),
        Caller::Operator => {
            crate::presence::folder_ship(store).context("run errand.start from a ship's folder")?
        }
    };
    let focus = required(args, "focus")?;
    ensure!(
        matches!(focus.split_once(':'), Some(("task" | "thread", _))),
        "an errand works on a task:<id> or thread:<id> focus"
    );
    crate::context::check_focus(store, &ship, focus)?;
    let brief = required(args, "brief")?;
    ensure!(
        !brief.trim().is_empty() && brief.chars().count() <= BRIEF_CHARS,
        "an errand needs a brief of at most {BRIEF_CHARS} characters"
    );
    let profile =
        crate::mail::dispatch(store, "ship.show", json!({"name": ship}))?["first_mate"].clone();
    ensure!(
        profile["manifest"].is_string(),
        "{ship} has no first mate profile to run errands with"
    );
    let task_id = match focus.split_once(':') {
        Some(("task", id)) => id.to_owned(),
        _ => profile["duty_task"]
            .as_str()
            .context("the ship has no duty task")?
            .to_owned(),
    };
    // The captain's errand routes when listed, otherwise the ship's own.
    let requested = args
        .get("route")
        .map(|r| r.as_str().context("route must be a route name"))
        .transpose()?;
    let (manifest, options) = match crate::crew_routes::choose(store, &ship, "errand", requested)? {
        Some(route) => (route.manifest, route.options),
        None => (profile["manifest"].clone(), profile["options"].clone()),
    };
    // Bundles and plugin config belong to the ship's own route.
    let own = manifest == profile["manifest"];
    let harness = crate::activation::harness_id(&json!({"manifest": manifest}));
    let key = session_key(focus);
    let digest = own
        .then(|| crate::tool_bundles::session_digest(store, &profile))
        .flatten();
    let resume = if !own || profile.get("tool_bundles").is_none() || digest.is_some() {
        crate::activation::warm_session(store, &ship, &key, &harness, &profile, digest.as_deref())?
    } else {
        None
    };
    let id = format!("errand-{}", uuid::Uuid::new_v4());
    let text = if resume.is_some() {
        format!(
            "Errand {id}, continuing on {focus}. Do this next:\n{brief}\n\nEnd with your verdict as your final reply."
        )
    } else {
        prompt(store, &id, &ship, focus, brief)?
    };
    let mode = if resume.is_some() { "resume" } else { "fresh" };
    // Recorded before its job exists, so launch planning treats it as the
    // first mate's own headless session rather than crew.
    store.connection().execute(
        "INSERT INTO errands(id, ship, focus, task_id, brief, mode, status, created_ms, harness, session_id)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, 'running', ?7, ?8, ?9)",
        params![id, ship, focus, task_id, brief, mode, Store::now_ms(), harness, resume],
    )?;
    let mut launch = json!({"id": id, "task_id": task_id, "manifest": manifest,
        "options": options, "wake": text});
    if let Some(selected) = profile.get("tool_bundles").filter(|_| own) {
        launch["tool_bundles"] = selected.clone();
    }
    if let Some(config) = profile.get("config").filter(|_| own) {
        launch["config"] = config.clone();
    }
    if let Some(session) = &resume {
        launch["options"]["resume"] = json!(session);
        launch["wake_only"] = json!(true);
    }
    if let Err(error) = crate::dispatch(store, "agent.start", launch) {
        store.connection().execute(
            "UPDATE errands SET status='failed', finished_ms=?2, error=?3 WHERE id=?1",
            params![id, Store::now_ms(), format!("{error:#}")],
        )?;
        return Err(error);
    }
    crate::herdr::errands::reconcile(store);
    store.audit(
        "errand.start",
        &json!({"errand": id, "ship": ship, "focus": focus}),
    )?;
    Ok(json!({"errand": id, "focus": focus, "mode": mode, "status": "running"}))
}

/// The last assistant reply in a harness's JSON event log: Pi-family
/// `message_end`, Codex `agent_message` items and Claude's `result`.
pub(crate) fn last_reply(log: &str) -> Option<String> {
    let mut reply = None;
    for event in crate::reply::events(log) {
        if let Some(text) = crate::reply::text(&event) {
            reply = Some(text);
        }
    }
    reply
}

/// Record errands whose job has ended and mail their verdicts.
pub fn settle(store: &mut Store) -> Result<Vec<Value>> {
    let running: Vec<(String, String, String, String)> = {
        let mut statement = store.connection().prepare(
            "SELECT id, ship, focus, task_id FROM errands WHERE status='running' ORDER BY created_ms",
        )?;
        statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut settled = vec![];
    for (id, ship, focus, task_id) in running {
        let job = store
            .connection()
            .query_row("SELECT record FROM jobs WHERE id=?1", [&id], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;
        let Some(job) = job else {
            continue;
        };
        let job: Value = serde_json::from_str(&job)?;
        let status = job["status"].as_str().unwrap_or("");
        if !matches!(
            status,
            "succeeded" | "failed" | "cancelled" | "timed_out" | "orphaned"
        ) {
            continue;
        }
        let logs = store
            .home()
            .join("jobs")
            .join(&id)
            .join(job["incarnation"].as_str().unwrap_or(""));
        let log = std::fs::read(logs.join("stdout.log"))
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        let reply = std::fs::read_to_string(logs.join("reply"))
            .ok()
            .filter(|reply| !reply.is_empty())
            .or_else(|| last_reply(&log));
        let outcome = if status == "succeeded" && reply.is_some() {
            "succeeded"
        } else {
            "failed"
        };
        let harness: String = store.connection().query_row(
            "SELECT COALESCE(harness,'') FROM errands WHERE id=?1",
            [&id],
            |r| r.get(0),
        )?;
        let session = crate::activation::read_session(&harness, &log);
        bind(
            store,
            &id,
            &ship,
            &focus,
            &harness,
            outcome,
            session.as_deref(),
        )?;
        let usage = crate::activation::usage(&log);
        let mut body = match &reply {
            Some(reply) if outcome == "succeeded" => {
                format!("Errand on {focus}, verdict:\n{reply}")
            }
            _ => format!(
                "Errand on {focus} {status} without a verdict. Its log is at {}/stdout.log.",
                logs.display()
            ),
        };
        if body.chars().count() > VERDICT_CHARS {
            body = format!(
                "{}…\n(The full reply is in {}/stdout.log.)",
                body.chars().take(VERDICT_CHARS).collect::<String>(),
                logs.display()
            );
        }
        store.connection().execute(
            "UPDATE errands SET status=?2, finished_ms=?3, verdict=?4, usage=?5,
               error=CASE WHEN ?2='failed' THEN ?6 ELSE NULL END WHERE id=?1",
            params![
                id,
                outcome,
                Store::now_ms(),
                reply,
                usage.as_ref().map(Value::to_string),
                format!("errand job {status}")
            ],
        )?;
        let mut payload = json!({"errand": id, "focus": focus, "status": outcome});
        let mut urgency = if outcome == "succeeded" {
            "normal"
        } else {
            "high"
        };
        // An errand stopped by a tool denial: the captain has the ask, and its
        // answer wakes the ship, so this verdict waits for that.
        if outcome == "failed"
            && let Some(ask) = crate::block_ask::ask_for_job(store, &id, &logs.join("stdout.log"))?
        {
            let ask_id = ask["id"].as_str().unwrap_or_default();
            body.push_str(&format!(
                "\nClaude was denied {}. The captain has decision {ask_id}; the task is not held meanwhile. Do not ask the captain again.",
                crate::block_ask::rules_text(&ask["payload"]["block"])
            ));
            payload["about"] = json!(ask_id);
            urgency = "low";
        }
        crate::mail::record_crew(
            store.connection(),
            &json!({"id": format!("{id}-verdict"), "to": ship, "kind": "notice",
                "task_id": task_id, "body": body, "payload": payload, "urgency": urgency}),
        )?;
        settled.push(json!({"errand": id, "status": outcome}));
    }
    crate::herdr::errands::reconcile(store);
    Ok(settled)
}

/// Keep a successful errand's session for its focus's next errand; a failed
/// errand drops it so the next starts fresh.
fn bind(
    store: &Store,
    id: &str,
    ship: &str,
    focus: &str,
    harness: &str,
    outcome: &str,
    session: Option<&str>,
) -> Result<()> {
    let key = session_key(focus);
    let (Some(session), "succeeded") = (session, outcome) else {
        store.connection().execute(
            "DELETE FROM sessions WHERE ship=?1 AND focus=?2",
            params![ship, key],
        )?;
        return Ok(());
    };
    let requested: Option<String> =
        store
            .connection()
            .query_row("SELECT session_id FROM errands WHERE id=?1", [id], |r| {
                r.get(0)
            })?;
    let now = Store::now_ms();
    if requested.as_deref() == Some(session) {
        store.connection().execute(
            "UPDATE sessions SET wakes=wakes+1, last_turn_ms=?3 WHERE ship=?1 AND focus=?2",
            params![ship, key, now],
        )?;
    } else {
        store.connection().execute(
            "INSERT INTO sessions(ship, focus, harness, session_id, wakes, started_ms, last_turn_ms, plan_digest)
             VALUES(?1, ?2, ?3, ?4, 1, ?5, ?5, (SELECT plan_digest FROM errands WHERE id=?6))
             ON CONFLICT(ship, focus) DO UPDATE SET harness=excluded.harness,
               session_id=excluded.session_id, wakes=1, started_ms=excluded.started_ms,
               last_turn_ms=excluded.last_turn_ms, plan_digest=excluded.plan_digest",
            params![ship, key, harness, session, now, id],
        )?;
    }
    store.connection().execute(
        "UPDATE errands SET session_id=?2 WHERE id=?1",
        params![id, session],
    )?;
    Ok(())
}

fn list(store: &Store, args: &Value) -> Result<Value> {
    let ship = match store.caller() {
        Caller::Agent { actor } => Some(actor.clone()),
        Caller::Operator => args.get("ship").and_then(Value::as_str).map(str::to_owned),
    };
    let mut statement = store.connection().prepare(
        "SELECT id, ship, focus, mode, status, created_ms, finished_ms, substr(brief,1,200),
           substr(COALESCE(verdict,''),1,400), error
         FROM errands WHERE (?1 IS NULL OR ship=?1) ORDER BY created_ms DESC LIMIT 50",
    )?;
    let errands = statement
        .query_map([ship], |r| {
            Ok(
                json!({"id": r.get::<_, String>(0)?, "ship": r.get::<_, String>(1)?,
                "focus": r.get::<_, String>(2)?, "mode": r.get::<_, String>(3)?,
                "status": r.get::<_, String>(4)?, "created_ms": r.get::<_, i64>(5)?,
                "finished_ms": r.get::<_, Option<i64>>(6)?, "brief": r.get::<_, String>(7)?,
                "verdict": r.get::<_, String>(8)?, "error": r.get::<_, Option<String>>(9)?}),
            )
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"errands": errands}))
}

/// Errands still running for a ship, for its bearings: `(id, focus, brief)`.
pub(crate) fn active(store: &Store, ship: &str) -> Result<Vec<(String, String, String)>> {
    let mut statement = store.connection().prepare(
        "SELECT id, focus, substr(brief,1,80) FROM errands WHERE ship=?1 AND status='running'
         ORDER BY created_ms",
    )?;
    let rows = statement.query_map([ship], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "errand.start" => start(store, &args),
        "errand.list" => list(store, &args),
        _ => bail!("unknown errand command {command}"),
    }
}
