//! Relaunching a crew whose route throttled before it did any work.
//!
//! A crew that meets a rate limit at its first model stream has done nothing,
//! and its route is held until the allowance returns (see `routes`). Leaving
//! the replacement to the first mate costs the task the time until its next
//! wake, and the next hour's probe was often the crew that hit the limit
//! again. So once the dispatcher's route scan has held the throttled route,
//! the same crew is started again on the next available route in the
//! captain's list, once: the relaunch carries the same task (so the same
//! assignment, which a crew brief reads from the task) and the same launch
//! settings and work offer, and its route is chosen fresh, skipping the held
//! one. A relaunch is never relaunched, so a route list throttled across the
//! board still ends failed, for the first mate to place by hand.
//!
//! Only a crew that provably did no work is relaunched: the run must have
//! lasted less than a short window, and its log must show no model output at
//! all — no assistant message, tool call or finished step, in any harness's
//! event shape. A run that cannot be shown empty is left to the first mate,
//! who has the failure outcome; the relaunch itself is reported in a crew
//! notice and on the failed job's record, and audited.
use crate::mail::record_crew;
use crate::store::{Caller, Store};
use anyhow::Result;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};

/// How long a run may have lasted and still count as having done no work:
/// generous for a harness to start and fail its first stream, well short of
/// any real work. Anything longer is the first mate's to place again.
const NO_WORK_WINDOW_MS: u64 = 180_000;
/// The metadata cursor: failed spawn jobs that settled before the last pass.
const CURSOR: &str = "crew_relaunch_scanned_ms";

/// Whether an event is the bookkeeping every harness writes around a turn
/// — session and step starts, results, plan windows, the final error a
/// throttle is read from, the runtime's own turn lines — rather than model
/// output. An assistant reply, a tool call or result, an output part or a
/// finished step is output, and so is any shape this reader cannot place:
/// a run is only ever shown empty by the shapes it knows, never assumed
/// empty from ones it does not.
fn bookkeeping(event: &Value) -> bool {
    if event["event"].is_string() {
        // AGY keeps its kinds under `event`; only its session start is
        // bookkeeping.
        return event["event"] == "init";
    }
    match event["type"].as_str() {
        Some(
            "session"
            | "system"
            | "init"
            | "step_start"
            | "thread.started"
            | "session.started"
            | "turn.completed"
            | "turn.failed"
            | "native.turn.failed"
            | "native.permission_blocked"
            | "result"
            | "error"
            | "rate_limit_event"
            | "message_start",
        ) => true,
        // Pi's turn envelope: an assistant reply, or a tool's result, is work.
        Some("message_end") => !matches!(
            event["message"]["role"].as_str(),
            Some("assistant" | "toolResult")
        ),
        // Codex reports each item it completed; only an error item is not work.
        Some("item.completed") => event["item"]["type"].as_str() == Some("error"),
        _ => false,
    }
}

/// Whether a run's log shows its model produced anything: not only the
/// assistant messages, tool calls and finished steps this reader can name,
/// but every event it cannot place, so an unparsed log never looks like a
/// run that did nothing.
pub(crate) fn worked(log: &str) -> bool {
    crate::reply::events(log).any(|event| !bookkeeping(&event))
}

/// Relaunch crews whose route throttled before they did any work. Runs after
/// the route scan in the same dispatcher pass, so the throttled route is
/// already held and the launch below takes the next one in the list. Called
/// by each dispatcher pass; no model call.
pub fn throttled(store: &mut Store) -> Result<Vec<Value>> {
    let since: i64 = store
        .connection()
        .query_row("SELECT value FROM metadata WHERE key=?1", [CURSOR], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(Store::now_ms);
    let jobs: Vec<Value> = {
        let mut statement = store.connection().prepare(
            "SELECT record FROM jobs WHERE json_extract(record,'$.action')='spawn'
               AND json_extract(record,'$.status')='failed'
               AND json_extract(record,'$.finished_ms') > ?1
             ORDER BY json_extract(record,'$.finished_ms')",
        )?;
        statement
            .query_map([since], |r| r.get::<_, String>(0))?
            .filter_map(|raw| raw.ok().and_then(|raw| serde_json::from_str(&raw).ok()))
            .collect()
    };
    let mut through = since;
    let mut relaunched = vec![];
    for job in jobs {
        match consider(store, &job)? {
            // A job that is only missing the route hold yet (it settled
            // between this pass's scan and its own read) is tried again next
            // pass, so the cursor stops before it; later jobs are re-read
            // too, which their own idempotency keys make harmless.
            Decision::Deferred => break,
            Decision::Done(entry) => {
                through = through.max(job["finished_ms"].as_i64().unwrap_or(through));
                if let Some(entry) = entry {
                    relaunched.push(entry);
                }
            }
        }
    }
    store.connection().execute(
        "INSERT INTO metadata(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        rusqlite::params![CURSOR, through.to_string()],
    )?;
    Ok(relaunched)
}

fn known(store: &Store, table: &str, id: &str) -> Result<bool> {
    Ok(store.connection().query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE id=?1)"),
        [id],
        |r| r.get(0),
    )?)
}

/// The failed job's log, as the route scan reads it.
fn log_of(store: &Store, job: &Value) -> String {
    let incarnation = job["incarnation"].as_str().unwrap_or_default();
    std::fs::read_to_string(
        store
            .home()
            .join("jobs")
            .join(job["id"].as_str().unwrap_or_default())
            .join(incarnation)
            .join("stdout.log"),
    )
    .unwrap_or_default()
}

/// The route a failed job ran on, from its record or its argv.
fn route_of(job: &Value) -> Option<String> {
    job["model_route"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| crate::routes::route_of_argv(&job["command"]))
}

/// What the pass did with one failed job: settled it, relaunched it (carrying
/// the entry it reported), or deferred it to the next pass because its route
/// hold had not landed yet.
enum Decision {
    Done(Option<Value>),
    Deferred,
}

/// Whether this failed job is a crew whose route throttled before it did any
/// work, and the relaunch of it: the same crew on the next available route.
fn consider(store: &mut Store, job: &Value) -> Result<Decision> {
    let id = job["id"].as_str().unwrap_or_default().to_owned();
    let relaunched_as = format!("crew-relaunch-{id}");
    // A relaunch is never relaunched, and one relaunch per crew: the id of
    // the job a relaunch would start is also its idempotency key.
    if job["relaunch_of"].is_string() {
        return Ok(Decision::Done(None));
    }
    if known(store, "jobs", &relaunched_as)? {
        // The launch already landed, so only the report may still be owed.
        if known(store, "mail", &relaunched_as)? {
            crate::jobs::note_relaunched(store, &id, &relaunched_as)?;
            return Ok(Decision::Done(None));
        }
        return Ok(recover(store, job, &relaunched_as));
    }
    // Live hosted sessions retry inside their own harness, and a long run is
    // never "before any work" however empty its log looks.
    if job["hosted"] == json!(true)
        || job["run_ms"]
            .as_u64()
            .is_some_and(|ms| ms > NO_WORK_WINDOW_MS)
    {
        return Ok(Decision::Done(None));
    }
    // Only a crew its first mate or the operator started: not a wake or an
    // errand, and not a nested agent job, which belongs to the crew that
    // started it and is that crew's to replace.
    if crate::crew::wake_job(store, &id)? {
        return Ok(Decision::Done(None));
    }
    if let Some(parent) = job["started_by"].as_str()
        && !crate::crew::wake_job(store, parent)?
    {
        return Ok(Decision::Done(None));
    }
    let task = job["task_id"].as_str().unwrap_or_default().to_owned();
    let Some(ship) = crate::crew::owning_ship(store, &task)? else {
        return Ok(Decision::Done(None));
    };
    let log = log_of(store, job);
    // The throttle the route scan read, on a run that shows no work at all.
    let Some(error) = crate::routes::throttle(&log) else {
        return Ok(Decision::Done(None));
    };
    if worked(&log) {
        return Ok(Decision::Done(None));
    }
    let Some(route) = route_of(job) else {
        return Ok(Decision::Done(None));
    };
    // The hold the route scan just recorded is the warrant: the relaunch
    // moves the crew because its route is held, and the choice below skips
    // it. A throttle with no hold yet only settled between the scan and this
    // read, so it waits for the next pass rather than being dropped.
    if crate::routes::blocked(&crate::jobs::host_ledger(store)?, &route)?.is_none() {
        return Ok(Decision::Deferred);
    }
    // The first mate may already have placed the work again: a live crew on
    // the task is not run beside.
    let placed: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM jobs WHERE json_extract(record,'$.task_id')=?1
           AND json_extract(record,'$.action')='spawn'
           AND json_extract(record,'$.status') IN
             ('starting','queued','launching','running','parked'))",
        [&task],
        |r| r.get(0),
    )?;
    if placed {
        return Ok(Decision::Done(None));
    }
    // The same crew again: the task carries the assignment, and the settings
    // come back as they were given, minus the route that throttled.
    let launch = job["launch"].clone();
    let mut args = json!({"id": relaunched_as, "task_id": task, "_relaunch_of": id});
    for key in [
        "heavy",
        "config",
        "tool_bundles",
        "cwd",
        "require",
        "timeout_secs",
    ] {
        if !launch[key].is_null() {
            args[key] = launch[key].clone();
        }
    }
    for key in ["wait_for_capacity", "max_wait_secs", "work_offer"] {
        if !job[key].is_null() {
            args[key] = job[key].clone();
        }
    }
    // A relaunch is the runtime's own launch, as a resume is.
    let caller = store.caller().clone();
    store.set_caller(Caller::Operator);
    let started = crate::dispatch(store, "agent.start", args);
    store.set_caller(caller);
    match started {
        Ok(started) => {
            let resumed = started["id"].as_str().unwrap_or(&relaunched_as).to_owned();
            let entry = notice(store, &ship, &task, &id, &route, &error, &resumed)?;
            crate::jobs::note_relaunched(store, &id, &resumed)?;
            store.audit(
                "crew.relaunched",
                &json!({"job_id": id, "relaunched_as": resumed, "route": route}),
            )?;
            Ok(Decision::Done(Some(entry)))
        }
        Err(error) => {
            // No route could take the crew, or the launch was refused: the
            // failure outcome already reached the first mate.
            store.audit(
                "crew.relaunch_failed",
                &json!({"job_id": id, "route": route, "error": format!("{error:#}")}),
            )?;
            Ok(Decision::Done(None))
        }
    }
}

/// The notice that says where a relaunched crew's work went, from the
/// relaunched job's own record. Its id is the relaunch job's id, so it is
/// written once however often the pass sees the failed job.
fn notice(
    store: &Store,
    ship: &str,
    task: &str,
    failed: &str,
    route: &str,
    error: &str,
    resumed: &str,
) -> Result<Value> {
    let record = crate::jobs::record(store, resumed)?;
    let on = record["model_route"].as_str().unwrap_or("another route");
    let why = record["route_why"].as_str().unwrap_or_default();
    let body = format!(
        "Crew job {failed} failed on route {route} before doing any work: the provider \
         throttled ({error}). The route is held until it clears; the same crew was \
         relaunched as job {resumed} on {on}{}.",
        if why.is_empty() {
            String::new()
        } else {
            format!(" ({why})")
        }
    );
    let entry = json!({"job_id": failed, "relaunched_as": resumed, "route": route, "on": on});
    record_crew(
        store.connection(),
        &json!({
            "id": format!("crew-relaunch-{failed}"), "to": ship, "kind": "notice",
            "task_id": task, "urgency": "normal", "body": body,
            "payload": entry,
        }),
    )?;
    Ok(entry)
}

/// A relaunch whose launch landed but whose notice did not (the pass died in
/// between): say it now, and mark the failed job's record.
fn recover(store: &mut Store, job: &Value, relaunched_as: &str) -> Decision {
    let say = |store: &mut Store| -> Result<Decision> {
        let id = job["id"].as_str().unwrap_or_default();
        let task = job["task_id"].as_str().unwrap_or_default().to_owned();
        let Some(ship) = crate::crew::owning_ship(store, &task)? else {
            return Ok(Decision::Done(None));
        };
        let Some(error) = crate::routes::throttle(&log_of(store, job)) else {
            return Ok(Decision::Done(None));
        };
        let Some(route) = route_of(job) else {
            return Ok(Decision::Done(None));
        };
        let entry = notice(store, &ship, &task, id, &route, &error, relaunched_as)?;
        crate::jobs::note_relaunched(store, id, relaunched_as)?;
        Ok(Decision::Done(Some(entry)))
    };
    say(store).unwrap_or_else(|error| {
        let _ = store.audit(
            "crew.relaunch_failed",
            &json!({"job_id": job["id"], "error": format!("{error:#}")}),
        );
        Decision::Done(None)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run's log, one event per line, as a harness writes it.
    fn log(events: &[Value]) -> String {
        events
            .iter()
            .map(|event| event.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn startup_and_throttle_events_are_not_work_but_model_output_is() {
        let startup = log(&[
            json!({"type":"session","id":"ses_1"}),
            json!({"type":"step_start","sessionID":"ses_1"}),
            json!({"type":"error","error":{"data":{"message":"Rate limit exceeded"}}}),
        ]);
        assert!(!worked(&startup), "a throttled first stream did no work");
        // Each harness's own shape of an assistant reply is work, as is a
        // tool call or a finished step; a tool result is too, since a model
        // called the tool.
        for event in [
            json!({"type":"message_end","message":{"role":"assistant","content":[]}}),
            json!({"type":"message_end","message":{"role":"toolResult"}}),
            json!({"type":"assistant","message":{"content":[]}}),
            json!({"type":"message","text":"done"}),
            json!({"type":"tool_call","call_id":"c"}),
            json!({"type":"tool_use","tool_id":"t"}),
            json!({"type":"text","part":{"type":"text","text":"done"}}),
            json!({"type":"step_finish","sessionID":"ses_1"}),
            json!({"type":"item.completed","item":{"type":"command_execution"}}),
            // A shape this reader cannot place is work too: a run is never
            // assumed empty from events it does not know.
            json!({"type":"mystery","anything":true}),
        ] {
            assert!(
                worked(&log(std::slice::from_ref(&event))),
                "{event} is work"
            );
        }
        // A user echo and bookkeeping are not model output: AGY's session
        // start, Pi's user envelope, Codex's error item, an empty log.
        for event in [
            json!({"event":"init","conversation_id":"c"}),
            json!({"type":"message_end","message":{"role":"user"}}),
            json!({"type":"item.completed","item":{"type":"error"}}),
        ] {
            assert!(
                !worked(&log(std::slice::from_ref(&event))),
                "{event} is not work"
            );
        }
        assert!(!worked(""));
    }
}
