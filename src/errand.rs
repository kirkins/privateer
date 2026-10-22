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
           harness TEXT, session_id TEXT, verdict TEXT, error TEXT, usage TEXT,
           route TEXT, route_why TEXT);
         CREATE INDEX IF NOT EXISTS errand_ship ON errands(ship, created_ms);",
    )?;
    // Checked and added under the write lock so concurrent opens cannot both add one.
    let tx = store.write_transaction()?;
    let has_digest: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('errands') WHERE name='plan_digest')",
        [],
        |r| r.get(0),
    )?;
    if !has_digest {
        tx.execute_batch("ALTER TABLE errands ADD COLUMN plan_digest TEXT;")?;
    }
    let has_route: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('errands') WHERE name='route')",
        [],
        |r| r.get(0),
    )?;
    if !has_route {
        tx.execute_batch("ALTER TABLE errands ADD COLUMN route TEXT; ALTER TABLE errands ADD COLUMN route_why TEXT;")?;
    }
    tx.commit()?;
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

/// The live scheduled offer for a task that the calling job holds, when the
/// caller is a woken first mate (the job this process runs in, as the agent
/// that job runs as). A stale
/// offer, such as one a `task.defer` has released, is not carried forward.
fn woken_offer(store: &Store, task_id: &str) -> Result<Option<String>> {
    let (Caller::Agent { actor }, Some(job)) = (store.caller(), crate::crew_scope::job_id()) else {
        return Ok(None);
    };
    if !crate::scheduling::available(store)? {
        return Ok(None);
    }
    let offer: Option<Option<String>> = store
        .connection()
        .query_row(
            "SELECT json_extract(j.record,'$.work_offer') FROM jobs j
               JOIN task_schedule s ON s.task_id=?2
             WHERE j.id=?1 AND json_extract(j.record,'$.agent_actor')=?3
               AND json_extract(s.record,'$.offer_id')=json_extract(j.record,'$.work_offer')
               AND json_extract(s.record,'$.state') IN ('offered','claimed','executing')",
            params![job, task_id, actor],
            |r| r.get(0),
        )
        .optional()?;
    Ok(offer.flatten())
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
    // Under a scheduled claim the errand carries its offer: named by the
    // caller, or the live offer a woken first mate holds for this task.
    let offer = match args.get("work_offer") {
        Some(offer) => Some(
            offer
                .as_str()
                .context("work_offer must be text")?
                .to_owned(),
        ),
        None => woken_offer(store, &task_id)?,
    };
    // The captain's errand routes when listed, otherwise the ship's own.
    let require = crate::crew_routes::parse_require(args.get("require"))?;
    let requested = args
        .get("route")
        .map(|r| r.as_str().context("route must be a route name"))
        .transpose()?;
    let choice = crate::crew_routes::choose(store, &ship, "errand", requested, require.as_ref())?;
    let (manifest, options, route_name, route_why, route_info) = match choice {
        Some(choice) => {
            let info = choice.to_route_json();
            (
                choice.manifest,
                choice.options,
                Some(choice.name),
                Some(choice.why),
                info,
            )
        }
        None => {
            let manifest = profile["manifest"].clone();
            let options = profile["options"].clone();
            let identity = manifest
                .as_str()
                .and_then(|m| crate::routes::route_of(m, &options));
            let info = json!({
                "name": Value::Null,
                "identity": identity,
                "tier": Value::Null,
                "uses": [],
                "why": Value::Null,
            });
            (manifest, options, None, None, info)
        }
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
        "INSERT INTO errands(id, ship, focus, task_id, brief, mode, status, created_ms, harness, session_id, route, route_why)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, 'running', ?7, ?8, ?9, ?10, ?11)",
        params![id, ship, focus, task_id, brief, mode, Store::now_ms(), harness, resume, route_name, route_why],
    )?;
    let mut launch = json!({"id": id, "task_id": task_id, "manifest": manifest,
        "options": options, "wake": text});
    if let Some(name) = &route_name {
        launch["route_name"] = json!(name);
    }
    if let Some(why) = &route_why {
        launch["route_why"] = json!(why);
    }
    if let Some(selected) = profile.get("tool_bundles").filter(|_| own) {
        launch["tool_bundles"] = selected.clone();
    }
    if let Some(config) = profile.get("config").filter(|_| own) {
        launch["config"] = config.clone();
    }
    if let Some(offer) = &offer {
        launch["work_offer"] = json!(offer);
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
    Ok(json!({
        "errand": id,
        "focus": focus,
        "mode": mode,
        "status": "running",
        "route": route_info,
    }))
}

/// A review's errand, as `review.start` launches it: its own id, the route
/// the reviewer was chosen on, and the prompt the runtime built whole.
pub(crate) struct ReviewLaunch<'a> {
    pub id: &'a str,
    pub ship: &'a str,
    pub task_id: &'a str,
    pub brief: &'a str,
    pub choice: &'a crate::crew_routes::Choice,
    pub prompt: &'a str,
}

/// Run a review as an errand: always a fresh session, never a warm one, with
/// no tool bundles or plugin config, and no records of the focus. Its job
/// runs under the reviewer role in the review's worktree.
pub(crate) fn start_review(store: &mut Store, launch: &ReviewLaunch) -> Result<()> {
    let choice = launch.choice;
    let harness = crate::activation::harness_id(&json!({"manifest": choice.manifest}));
    let focus = format!("task:{}", launch.task_id);
    store.connection().execute(
        "INSERT INTO errands(id, ship, focus, task_id, brief, mode, status, created_ms, harness, route, route_why)
         VALUES(?1, ?2, ?3, ?4, ?5, 'fresh', 'running', ?6, ?7, ?8, ?9)",
        params![launch.id, launch.ship, focus, launch.task_id, launch.brief, Store::now_ms(),
            harness, choice.name, choice.why],
    )?;
    let start = json!({"id": launch.id, "task_id": launch.task_id, "manifest": choice.manifest,
        "options": choice.options, "wake": launch.prompt, "route_name": choice.name,
        "route_why": choice.why});
    if let Err(error) = crate::dispatch(store, "agent.start", start) {
        store.connection().execute(
            "UPDATE errands SET status='failed', finished_ms=?2, error=?3 WHERE id=?1",
            params![launch.id, Store::now_ms(), format!("{error:#}")],
        )?;
        return Err(error);
    }
    crate::herdr::errands::reconcile(store);
    store.audit(
        "errand.start",
        &json!({"errand": launch.id, "ship": launch.ship, "focus": focus, "review": true}),
    )?;
    Ok(())
}

/// The last assistant reply in a harness's JSON event log: Pi-family
/// `message_end`, Codex `agent_message` items and Claude's `result`.
/// Returned with whether it also ended its turn.
pub(crate) fn last_reply(log: &str) -> Option<(String, bool)> {
    let mut reply = None;
    for event in crate::reply::events(log) {
        if let Some(text) = crate::reply::text(&event) {
            reply = Some((text, crate::reply::ends_turn(&event)));
        }
    }
    reply
}

/// How a run that did not succeed ended: its job status when the runtime
/// stopped it, else the turn wrapper's own lines in its log, else its exit.
fn run_ending(status: &str, job: &Value, log: &str) -> String {
    match status {
        "timed_out" => return "timed out".to_owned(),
        "cancelled" | "orphaned" => return status.to_owned(),
        _ => {}
    }
    let mut blocked = false;
    let mut failed = None;
    for event in crate::reply::events(log) {
        match event["type"].as_str() {
            Some("native.permission_blocked") => blocked = true,
            Some("native.turn.failed") => {
                failed = Some(match event["reason"].as_str() {
                    None | Some("Native completion was not confirmed.") => {
                        "native completion not confirmed".to_owned()
                    }
                    // Wrapper reasons are slugs such as `repeated-result`.
                    Some(reason) if !reason.contains(' ') => reason.replace('-', " "),
                    Some(reason) => reason.trim_end_matches('.').to_owned(),
                });
            }
            Some("turn.failed") => {
                failed = Some(
                    event["error"]["message"]
                        .as_str()
                        .map(|m| m.chars().take(200).collect())
                        .unwrap_or_else(|| "the turn failed".to_owned()),
                );
            }
            _ => {}
        }
    }
    if blocked {
        return "permission blocked".to_owned();
    }
    failed.unwrap_or_else(|| match job["exit_code"].as_i64() {
        Some(code) => format!("exit {code}"),
        None => status.to_owned(),
    })
}

struct RunningErrand {
    id: String,
    ship: String,
    focus: String,
    task_id: String,
    route_why: Option<String>,
}

/// Record errands whose job has ended and mail their verdicts.
pub fn settle(store: &mut Store) -> Result<Vec<Value>> {
    let running: Vec<RunningErrand> = {
        let mut statement = store.connection().prepare(
            "SELECT id, ship, focus, task_id, route_why FROM errands WHERE status='running' ORDER BY created_ms",
        )?;
        statement
            .query_map([], |r| {
                Ok(RunningErrand {
                    id: r.get(0)?,
                    ship: r.get(1)?,
                    focus: r.get(2)?,
                    task_id: r.get(3)?,
                    route_why: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut settled = vec![];
    for errand in running {
        let id = errand.id;
        let ship = errand.ship;
        let focus = errand.focus;
        let task_id = errand.task_id;
        let route_why = errand.route_why;
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
        let saved = std::fs::read_to_string(logs.join("reply"))
            .ok()
            .filter(|reply| !reply.is_empty());
        let reply = match (status, last_reply(&log)) {
            ("succeeded", last) => saved.or(last.map(|(text, _)| text)),
            // A run that did not succeed may have stopped mid-turn: its reply
            // is a verdict only when the last one its log shows ended a turn,
            // or the log kept none and the reply file holds one.
            (_, Some((text, true))) => saved.or(Some(text)),
            (_, Some((_, false))) => None,
            (_, None) => saved,
        };
        let outcome = if status == "succeeded" && reply.is_some() {
            "succeeded"
        } else {
            "failed"
        };
        let ending = (status != "succeeded").then(|| run_ending(status, &job, &log));
        let harness: String = store.connection().query_row(
            "SELECT COALESCE(harness,'') FROM errands WHERE id=?1",
            [&id],
            |r| r.get(0),
        )?;
        // A review records its filtered verdict before its notice is mailed,
        // and its session is never kept: every review starts fresh.
        let review = crate::review::settle(store, &id, status, outcome, reply.as_deref())?;
        if review.is_none() {
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
        }
        let usage = crate::activation::usage(&log);
        let mut body = match (&review, &reply) {
            (Some(review), _) => review.body.clone(),
            (None, Some(reply)) => match &ending {
                None => format!("Errand on {focus}, verdict:\n{reply}"),
                Some(ending) => format!(
                    "Errand on {focus}, verdict (unconfirmed; the run ended: {ending}):\n{reply}"
                ),
            },
            _ => format!(
                "Errand on {focus} {status} without a verdict. Its log is at {}/stdout.log.",
                logs.display()
            ),
        };
        if review.is_none() && body.chars().count() > VERDICT_CHARS {
            body = format!(
                "{}…\n(The full reply is in {}/stdout.log.)",
                body.chars().take(VERDICT_CHARS).collect::<String>(),
                logs.display()
            );
        }
        if let Some(why) = route_why.as_ref().filter(|_| review.is_none()) {
            if !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str(&format!("Route: {why}\n"));
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
                match &ending {
                    Some(ending) if ending != status => format!("errand job {status} ({ending})"),
                    _ => format!("errand job {status}"),
                }
            ],
        )?;
        let mut payload = json!({"errand": id, "focus": focus, "status": outcome});
        // A delivered verdict keeps the status its job had, marked unconfirmed.
        let unconfirmed = outcome == "failed" && reply.is_some() && review.is_none();
        if unconfirmed {
            payload["verdict"] = json!("unconfirmed");
            payload["ended"] = json!(ending);
        }
        let mut urgency = if outcome == "succeeded" || unconfirmed {
            "normal"
        } else {
            "high"
        };
        if let Some(review) = &review {
            payload["review"] = review.payload.clone();
            urgency = review.urgency;
        }
        // An errand stopped by a tool denial: the captain has the ask, and its
        // answer wakes the ship, so a missing verdict waits for that. A
        // delivered one keeps its urgency: the ship can act on it now.
        if outcome == "failed"
            && let Some(ask) = crate::block_ask::ask_for_job(store, &id, &logs.join("stdout.log"))?
        {
            let ask_id = ask["id"].as_str().unwrap_or_default();
            body.push_str(&format!(
                "\nClaude was denied {}. The captain has decision {ask_id}; the task is not held meanwhile. Do not ask the captain again.",
                crate::block_ask::rules_text(&ask["payload"]["block"])
            ));
            payload["about"] = json!(ask_id);
            if !unconfirmed {
                urgency = "low";
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_ending_names_how_its_turn_ended() {
        let job = json!({"exit_code": 2});
        let ended = |status: &str, log: &str| run_ending(status, &job, log);
        assert_eq!(
            ended(
                "failed",
                r#"{"type":"native.turn.failed","reason":"Native completion was not confirmed."}"#
            ),
            "native completion not confirmed"
        );
        assert_eq!(
            ended(
                "failed",
                r#"{"type":"native.turn.failed","reason":"repeated-result"}"#
            ),
            "repeated result"
        );
        // Claude prints its failed turn, then the denial that caused it.
        assert_eq!(
            ended(
                "failed",
                "{\"type\":\"native.turn.failed\"}\n{\"type\":\"native.permission_blocked\",\"denials\":[]}"
            ),
            "permission blocked"
        );
        assert_eq!(
            ended(
                "failed",
                r#"{"type":"turn.failed","error":{"message":"stream disconnected"}}"#
            ),
            "stream disconnected"
        );
        assert_eq!(ended("failed", ""), "exit 2");
        assert_eq!(
            ended(
                "timed_out",
                r#"{"type":"native.turn.failed","reason":"stalled"}"#
            ),
            "timed out"
        );
    }
}
