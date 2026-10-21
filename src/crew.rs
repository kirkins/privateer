//! Crew outcomes and reports: how a first mate hears from its crew.
//!
//! Crew are supervised jobs working on a ship's tasks. When one reaches a
//! reported status, the runtime writes a notice from the reserved sender
//! `crew` to the ship that owns the job's task, in the same transaction as
//! the status, from the job's record and logs, never from model output. The
//! id `crew-<job>-<incarnation>` makes it exactly one per incarnation. From
//! there activation, focus, hooks, acknowledgement and retention treat it
//! like any mail. Crew run as their ship, so they cannot mail their own
//! first mate; `crew.report` lets a running crew job send progress, or ask
//! for a decision that the first mate answers with a reply and the crew
//! reads with `crew.answer`.
use crate::mail::{CREW, find, record_crew};
use crate::store::{Caller, Store, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Outcome bodies stay within what a wake or a hook shows of one message.
const BODY_CHARS: usize = 2_000;
const TAIL_BYTES: u64 = 1_400;
const COMMAND_CHARS: usize = 160;
const REASON_CHARS: usize = 300;
const REPORT_BYTES: usize = 4_096;
const MAX_WAIT_SECS: u64 = 600;

fn clip(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        text.to_owned()
    } else {
        format!("{}…", text.chars().take(chars).collect::<String>())
    }
}

/// The ship hosted here that owns a task, if any.
pub(crate) fn owning_ship(store: &Store, task_id: &str) -> Result<Option<String>> {
    let Ok(task) = store.task(task_id) else {
        return Ok(None);
    };
    let owner = task["owner"].as_str().unwrap_or("").to_owned();
    let here: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM ships WHERE name=?1 AND json_extract(record,'$.location')='here')",
        [&owner],
        |r| r.get(0),
    )?;
    Ok(here.then_some(owner))
}

/// A first mate's own wake job or errand, recorded before it starts. An
/// errand's verdict is its report, so its outcome is not mailed again.
pub(crate) fn wake_job(store: &Store, job_id: &str) -> Result<bool> {
    let wake: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM wakes WHERE job_id=?1)",
        [job_id],
        |r| r.get(0),
    )?;
    Ok(wake || crate::errand::is_errand(store, job_id)?)
}

/// The last bytes of a log, as readable text. Missing logs read as empty.
fn tail(path: &Path, limit: u64) -> String {
    let read = || -> std::io::Result<(Vec<u8>, bool)> {
        let mut file = std::fs::File::open(path)?;
        let length = file.metadata()?.len();
        let start = length.saturating_sub(limit);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = vec![];
        file.take(limit).read_to_end(&mut bytes)?;
        Ok((bytes, start > 0))
    };
    let Ok((bytes, cut)) = read() else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    // Start at a line boundary when the cut left a partial first line.
    let text = match text.find('\n') {
        Some(at) if cut && at < text.len() - 1 => &text[at + 1..],
        _ => &text,
    };
    text.chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                '?'
            } else {
                c
            }
        })
        .collect::<String>()
        .trim_end()
        .to_owned()
}

/// Record a crew job's outcome for the ship that owns its task. Called by
/// the jobs module inside the transaction that sets the reported status.
/// `block` is the captain ask its denial raised or re-hit, if any: the
/// captain has it, so the outcome waits for the next real wake.
pub(crate) fn outcome(store: &Store, job: &Value, block: Option<&Value>) -> Result<()> {
    let job_id = job["id"].as_str().context("job id")?;
    let incarnation = job["incarnation"].as_str().context("job incarnation")?;
    let task_id = job["task_id"].as_str().context("job task")?;
    let status = job["status"].as_str().context("job status")?;
    let Some(ship) = owning_ship(store, task_id)? else {
        return Ok(());
    };
    // A task closed by its owner cancels its crew quietly; any other ending
    // of a quiet job, such as an orphan still holding capacity, is reported.
    if parked_on(store, job_id)? || (job["quiet"] == true && status == "cancelled") {
        return Ok(());
    }
    // A first mate is not told about its own wake: that would wake it again.
    if wake_job(store, job_id)? {
        return Ok(());
    }
    // Crew follow the jobs they start. While that crew still runs, the outcome
    // waits for the first mate's next real wake instead of causing one.
    let crew_watching = match job["started_by"].as_str() {
        Some(parent) => {
            !wake_job(store, parent)?
                && crate::jobs::record(store, parent)
                    .is_ok_and(|p| matches!(p["status"].as_str(), Some("launching" | "running")))
        }
        None => false,
    };
    let asked = block.and_then(|b| b["ask"].as_str());
    let declined = block.is_some_and(|b| b["declined"] == true);
    let urgency = match status {
        _ if crew_watching => "low",
        _ if asked.is_some() => "low",
        _ if declined => "normal",
        "succeeded" => "normal",
        // The ship stopped it itself; it can wait for the next real wake.
        "cancelled" if job["cancelled_by"] == ship.as_str() => "low",
        "cancelled" => "normal",
        _ => "high",
    };
    let duration_ms = job["finished_ms"]
        .as_i64()
        .unwrap_or_else(Store::now_ms)
        .saturating_sub(job["created_ms"].as_i64().unwrap_or(0))
        .max(0);
    let command = clip(
        &job["command"]
            .as_array()
            .map(|args| {
                args.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default(),
        COMMAND_CHARS,
    );
    let reason = job["reason"].as_str().map(|r| clip(r, REASON_CHARS));
    let logs = store.home().join("jobs").join(job_id).join(incarnation);
    let mut body = format!("Crew job {job_id} {status}");
    if let Some(code) = job["exit_code"].as_i64() {
        body.push_str(&format!(" with exit code {code}"));
    }
    body.push_str(&format!(
        " after {:.1} s, for task {task_id}.\n",
        duration_ms as f64 / 1000.0
    ));
    if let Some(route_why) = job["route_why"].as_str() {
        body.push_str(&format!("Route: {route_why}\n"));
    }
    if let Some(reason) = &reason {
        body.push_str(&format!("Reason: {reason}\n"));
    }
    if let Some(block) = block {
        let rules = block["rules"].as_str().unwrap_or("a tool");
        match asked {
            Some(ask) => body.push_str(&format!(
                "Claude was denied {rules}. The captain has decision {ask}{}; the task waits for the answer. Do not ask the captain again.\n",
                if block["duplicate"] == true { " already" } else { "" }
            )),
            None => body.push_str(&format!(
                "Claude was denied {rules}, which the captain declined for this task. Do it without them.\n"
            )),
        }
    }
    if let Some(leader) = job["leader_result"]["status"].as_str() {
        body.push_str(&format!(
            "Its leader process {leader}; descendants were still running.\n"
        ));
    }
    if status == "orphaned" {
        // Freeing it is the ship's own step; resource.recover is the captain's,
        // and refuses while any recorded process remains.
        body.push_str(&format!(
            "It still holds its capacity. Once you have what you need from it, \
             job.cancel {{\"id\":\"{job_id}\"}} stops its recorded process group and \
             frees that capacity; killing processes by hand can miss some.\n"
        ));
    }
    body.push_str(&format!(
        "Command: {command}\nLogs: {}/{{stdout,stderr}}.log\n",
        logs.display()
    ));
    // Whatever room is left goes to the end of stderr, then stdout.
    let room = BODY_CHARS.saturating_sub(body.chars().count() + 80) as u64;
    let budget = room.min(TAIL_BYTES);
    let stderr = tail(&logs.join("stderr.log"), budget);
    let stdout = tail(
        &logs.join("stdout.log"),
        budget.saturating_sub(stderr.len() as u64),
    );
    for (name, text) in [("stderr", stderr), ("stdout", stdout)] {
        if !text.is_empty() {
            body.push_str(&format!("--- end of {name} ---\n{text}\n"));
        }
    }
    // The ellipsis counts too.
    let body = clip(&body, BODY_CHARS - 1);
    let payload = json!({
        "job_id": job_id, "incarnation": incarnation, "task_id": task_id,
        "status": status, "exit_code": job["exit_code"], "reason": reason,
        "duration_ms": duration_ms, "command": command, "logs": logs,
        "leader_result": job["leader_result"], "cancelled_by": job["cancelled_by"],
        "retry_of": job["retry_of"],
    });
    let mut payload = payload;
    if let Some(block) = block {
        payload["block"] = block.clone();
        if let Some(ask) = asked {
            payload["about"] = json!(ask);
        }
    }
    record_crew(
        store.connection(),
        &json!({"id": format!("crew-{job_id}-{incarnation}"), "to": ship, "kind": "notice",
            "task_id": task_id, "body": body, "payload": payload, "urgency": urgency}),
    )?;
    Ok(())
}

/// `crew.report`: a running crew job tells its first mate something. The
/// caller must be the agent a running job (named by `PRIVATEER_JOB`, which
/// the supervisor sets) works for, on a task its ship still owns.
fn report(store: &Store, args: &Value) -> Result<Value> {
    let Caller::Agent { actor } = store.caller() else {
        bail!("crew.report is for crew, which run as their ship's agent");
    };
    let job_id = crate::crew_scope::job_id()
        .context("crew.report runs inside a crew job; PRIVATEER_JOB is not set")?;
    let job = crate::jobs::record(store, &job_id)?;
    ensure!(
        job["agent_actor"] == actor.as_str(),
        "job {job_id} is not crew of {actor}"
    );
    ensure!(
        matches!(job["status"].as_str(), Some("launching" | "running")),
        "job {job_id} is not running"
    );
    ensure!(
        !wake_job(store, &job_id)?,
        "a first mate's own session mails as its ship, not as crew"
    );
    let task_id = job["task_id"].as_str().context("job task")?;
    ensure!(
        owning_ship(store, task_id)?.as_deref() == Some(actor.as_str()),
        "task {task_id} is no longer {actor}'s; mail its owner instead"
    );
    let kind = required(args, "kind")?;
    let (mail_kind, urgency, lead) = match kind {
        "decision" => ("request", "high", "asks for a decision"),
        "progress" => ("notice", "low", "reports progress"),
        _ => bail!("kind must be decision or progress"),
    };
    let body = required(args, "body")?;
    ensure!(
        body.len() <= REPORT_BYTES,
        "a crew report is limited to {REPORT_BYTES} bytes"
    );
    let data = args.get("payload").cloned().unwrap_or(json!({}));
    ensure!(
        data.is_object() && data.to_string().len() <= REPORT_BYTES,
        "payload must be an object of at most {REPORT_BYTES} bytes"
    );
    let suffix = match args.get("id").filter(|v| !v.is_null()) {
        Some(id) => {
            let id = id.as_str().context("id must be a string")?;
            crate::store::identifier(id)?;
            ensure!(id.len() <= 64, "id is limited to 64 characters");
            id.to_owned()
        }
        None => uuid::Uuid::new_v4().to_string(),
    };
    let incarnation = job["incarnation"].as_str().unwrap_or("");
    let id = format!("crew-{job_id}-{incarnation}-{suffix}");
    let message = json!({
        "id": id, "to": actor, "kind": mail_kind, "task_id": task_id, "urgency": urgency,
        "body": format!("Crew job {job_id} {lead}:\n{body}"),
        "payload": {"job_id": job_id, "incarnation": incarnation, "report": kind, "data": data},
    });
    let tx = store.write_transaction()?;
    let written = record_crew(&tx, &message)?;
    tx.commit()?;
    let mut stored = find(store, &id)?.context("crew report vanished")?;
    if !written {
        ensure!(
            stored["body"] == message["body"] && stored["payload"] == message["payload"],
            "report id {suffix} already holds a different report"
        );
        stored["duplicate"] = json!(true);
    }
    Ok(stored)
}

/// `crew.answer`: the state of a crew decision and its reply, optionally
/// waiting up to `wait_secs` for the first mate to answer.
fn answer(store: &Store, args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    let wait = args
        .get("wait_secs")
        .filter(|v| !v.is_null())
        .map(|v| {
            v.as_u64()
                .filter(|s| *s <= MAX_WAIT_SECS)
                .with_context(|| format!("wait_secs must be 0..{MAX_WAIT_SECS}"))
        })
        .transpose()?
        .unwrap_or(0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait);
    loop {
        let request = find(store, id)?
            .filter(|m| m["from"] == CREW && m["kind"] == "request")
            .with_context(|| format!("no crew decision {id}"))?;
        if let Caller::Agent { actor } = store.caller() {
            ensure!(
                request["to"] == actor.as_str(),
                "crew decision {id} belongs to another ship"
            );
        }
        let reply = match request["answered_by"].as_str() {
            Some(reply) => find(store, reply)?,
            None => None,
        };
        if reply.is_some() || std::time::Instant::now() >= deadline {
            return Ok(json!({"id": id, "state": request["state"],
                "overdue": request["overdue"], "reply": reply}));
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// Parks allowed per crew lineage, and its elapsed cap, unless the charter says
/// otherwise. A park resumes the crew; see docs/design/crew-park-resume.md.
const MAX_PARKS: u64 = 6;
const MAX_ELAPSED_SECS: u64 = 86_400;
const NEXT_CHARS: usize = 300;
/// How long a resume's launch may take before recovery stops waiting for its
/// claimer: the longest workspace wait, and a generous margin.
const LAUNCH_BOUND_MS: i64 = (crate::jobs::DEFAULT_MAX_WAIT_SECS as i64 + 900) * 1000;

/// The crew job itself, then the crew jobs its resumes replaced, oldest last.
/// A child started by any of them belongs to this crew.
pub(crate) fn lineage(store: &Store, crew: &str) -> Result<Vec<String>> {
    let mut chain = vec![crew.to_owned()];
    loop {
        let previous: Option<String> = store
            .connection()
            .query_row(
                "SELECT crew_job_id FROM crew_parks WHERE resumed_job_id=?1",
                [chain.last()],
                |r| r.get(0),
            )
            .optional()?;
        match previous {
            Some(previous) => chain.push(previous),
            None => return Ok(chain),
        }
    }
}

/// Whether the running job is a crew that may park: a supervised crew job of
/// its ship, not a first mate's wake and not hosted.
pub(crate) fn may_park(store: &Store) -> bool {
    let Some(crew) = crate::crew_scope::job_id() else {
        return false;
    };
    let Ok(job) = crate::jobs::record(store, &crew) else {
        return false;
    };
    let actor = match store.caller() {
        Caller::Agent { actor } => actor.as_str(),
        Caller::Operator => return false,
    };
    job["action"] == "spawn"
        && job["agent_actor"] == actor
        && job["hosted"] != json!(true)
        && matches!(job["status"].as_str(), Some("launching" | "running"))
        && !wake_job(store, &crew).unwrap_or(true)
}

/// Whether a child job's outcome belongs to a crew parked on it, or about to
/// park on it (the child can settle before the crew's payload has ended). Its
/// resume reads the outcome, so the first mate is not told about it again.
pub(crate) fn parked_on(store: &Store, child: &str) -> Result<bool> {
    Ok(store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM crew_parks p JOIN jobs j ON j.id=p.crew_job_id
           WHERE p.child_job_id=?1 AND p.state='parked' AND p.crew_job_id=json_extract(j.record,'$.id')
             AND json_extract(j.record,'$.park')=p.id
             AND json_extract(j.record,'$.status') IN ('parked','launching','running'))",
        [child],
        |r| r.get(0),
    )?)
}

/// What a crew reads of a job that settled: its status, exit, reason and the
/// tail of its logs, bounded like an outcome notice.
fn settlement_text(store: &Store, job: &Value) -> String {
    let job_id = job["id"].as_str().unwrap_or_default();
    let incarnation = job["incarnation"].as_str().unwrap_or_default();
    let mut text = format!(
        "Job {job_id} {}",
        job["status"].as_str().unwrap_or_default()
    );
    if let Some(code) = job["exit_code"].as_i64() {
        text.push_str(&format!(" with exit code {code}"));
    }
    text.push_str(".\n");
    if let Some(reason) = job["reason"].as_str() {
        text.push_str(&format!("Reason: {}\n", clip(reason, REASON_CHARS)));
    }
    let command = job["command"]
        .as_array()
        .map(|args| {
            args.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let logs = store.home().join("jobs").join(job_id).join(incarnation);
    text.push_str(&format!(
        "Command: {}\nLogs: {}/{{stdout,stderr}}.log\n",
        clip(&command, COMMAND_CHARS),
        logs.display()
    ));
    let stderr = tail(&logs.join("stderr.log"), TAIL_BYTES);
    let stdout = tail(
        &logs.join("stdout.log"),
        TAIL_BYTES.saturating_sub(stderr.len() as u64),
    );
    for (name, lines) in [("stderr", stderr), ("stdout", stdout)] {
        if !lines.is_empty() {
            text.push_str(&format!("--- end of {name} ---\n{lines}\n"));
        }
    }
    clip(&text, BODY_CHARS)
}

/// `crew.park`: a running crew ends its turn to wait on a heavy job it
/// started, instead of polling. The job's outcome resumes the crew.
fn park(store: &Store, args: &Value) -> Result<Value> {
    let Caller::Agent { actor } = store.caller() else {
        bail!("crew.park is for crew, which run as their ship's agent");
    };
    let crew = crate::crew_scope::job_id()
        .context("crew.park runs inside a crew job; PRIVATEER_JOB is not set")?;
    let child = required(args, "job_id")?;
    let next = required(args, "next")?;
    ensure!(
        !next.trim().is_empty() && next.chars().count() <= NEXT_CHARS && !next.contains('\n'),
        "next must be one line of at most {NEXT_CHARS} characters"
    );
    let job = crate::jobs::record(store, &crew)?;
    ensure!(
        job["action"] == "spawn" && job["agent_actor"] == actor.as_str(),
        "job {crew} is not crew of {actor}"
    );
    ensure!(
        job["hosted"] != json!(true),
        "a hosted crew cannot park yet: its pane keeps the session, so wait in the job instead"
    );
    ensure!(
        matches!(job["status"].as_str(), Some("launching" | "running")),
        "job {crew} is not running"
    );
    ensure!(
        !wake_job(store, &crew)?,
        "a first mate's own session cannot park"
    );
    let chain = lineage(store, &crew)?;
    let child_record = crate::jobs::record(store, child)?;
    ensure!(
        child_record["started_by"]
            .as_str()
            .is_some_and(|parent| chain.iter().any(|c| c == parent)),
        "job {child} is not work that crew {crew} started"
    );
    let park = open_park(store, &job, child, next, "parked")?;
    Ok(json!({"id": park, "state": "parked", "job_id": crew, "child_job_id": child}))
}

/// The crew's budget ran out (see `supervise_inner`). A crew with an
/// unfinished heavy job parks on it; a crew with none is held for its first
/// mate to resume with `crew.resume`. `None` when it cannot park, in which
/// case the job times out as before.
pub(crate) fn park_at_deadline(store: &Store, crew: &str) -> Result<Option<String>> {
    let job = crate::jobs::record(store, crew)?;
    // Only a crew (a spawned agent job) parks; an agent's own build does not.
    if job["action"] != "spawn"
        || job["agent_actor"].is_null()
        || job["hosted"] == json!(true)
        || !matches!(job["status"].as_str(), Some("launching" | "running"))
        || wake_job(store, crew)?
    {
        return Ok(None);
    }
    let chain = lineage(store, crew)?;
    let mut waiting = None;
    for member in &chain {
        let child: Option<String> = store
            .connection()
            .query_row(
                "SELECT id FROM jobs WHERE json_extract(record,'$.started_by')=?1
                   AND json_extract(record,'$.status') IN ('starting','queued','launching','running')
                 ORDER BY json_extract(record,'$.created_ms') LIMIT 1",
                [member],
                |r| r.get(0),
            )
            .optional()?;
        if child.is_some() {
            waiting = child;
            break;
        }
    }
    // A spent cap ends the crew at its deadline: the job it would wait on is
    // cancelled, and the job times out with the reason.
    let task = job["task_id"].as_str().context("job task")?.to_owned();
    let root = chain.last().context("crew lineage")?.clone();
    if let Some(reason) = spent_cap(store, &root, &task, true)? {
        if let Some(child) = &waiting {
            crate::jobs::cancel_crew(store, child)?;
        }
        bail!("{reason}; the crew ends at its deadline");
    }
    let parked = match &waiting {
        Some(child) => (
            child.as_str(),
            "parked",
            format!("its time budget ran out while job {child} runs"),
        ),
        None => (
            "",
            "held",
            "its time budget ran out with no heavy job outstanding".to_owned(),
        ),
    };
    Ok(Some(open_park(store, &job, parked.0, &parked.2, parked.1)?))
}

/// Open a park on `child` (empty for a held crew), in one transaction with
/// the crew's own record: the crew is marked parked on it, and its ship is
/// told: quietly for a park, and as a normal notice for a held crew.
fn open_park(store: &Store, job: &Value, child: &str, next: &str, state: &str) -> Result<String> {
    let crew = job["id"].as_str().context("crew job id")?.to_owned();
    let actor = job["agent_actor"]
        .as_str()
        .context("crew agent")?
        .to_owned();
    let task = job["task_id"].as_str().context("job task")?.to_owned();
    let charter = store.authorize(&task, "spawn")?;
    let chain = lineage(store, &crew)?;
    let root = chain.last().context("crew lineage")?.clone();
    let root_record = crate::jobs::record(store, &root)?;
    let max_parks = charter
        .get("crew_max_parks")
        .and_then(Value::as_u64)
        .unwrap_or(MAX_PARKS);
    let max_elapsed = charter
        .get("crew_max_elapsed_secs")
        .and_then(Value::as_u64)
        .unwrap_or(MAX_ELAPSED_SECS);
    let now = Store::now_ms();
    let elapsed_ms = now - root_record["created_ms"].as_i64().unwrap_or(now);
    ensure!(
        elapsed_ms <= (max_elapsed as i64).saturating_mul(1000),
        "crew {root} has run for more than {max_elapsed} seconds in total; it cannot park again"
    );
    let harness = job["launch"]["manifest"]
        .as_str()
        .and_then(crate::bundled::harness);
    let tx = store.write_transaction()?;
    if !child.is_empty() {
        let child_status: String = tx.query_row(
            "SELECT json_extract(record,'$.status') FROM jobs WHERE id=?1",
            [child],
            |r| r.get(0),
        )?;
        ensure!(
            matches!(
                child_status.as_str(),
                "starting" | "queued" | "launching" | "running"
            ),
            "job {child} already {child_status}; read its outcome instead of parking"
        );
    }
    // Still the same running crew, checked again inside the transaction.
    let crew_status: String = tx.query_row(
        "SELECT json_extract(record,'$.status') FROM jobs WHERE id=?1",
        [&crew],
        |r| r.get(0),
    )?;
    ensure!(
        matches!(crew_status.as_str(), "launching" | "running"),
        "job {crew} is {crew_status}, not running"
    );
    let parks: i64 = tx.query_row(
        "SELECT COUNT(*) FROM crew_parks WHERE root=?1",
        [&root],
        |r| r.get(0),
    )?;
    ensure!(
        (parks as u64) < max_parks,
        "crew {root} has parked {parks} times; the cap is {max_parks}"
    );
    // Named for the lineage root, not the crew job, which after a resume is
    // itself a resume id: naming it by `crew` would nest one level per park.
    let park_id = format!("park-{root}-{}", parks + 1);
    tx.execute(
        "INSERT INTO crew_parks(id, root, crew_job_id, child_job_id, task_id, ship, state,
           next, harness, session, attempt, created_ms, updated_ms)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, 0, ?10, ?10)",
        params![
            park_id, root, crew, child, task, actor, state, next, harness, now
        ],
    )?;
    // Mark the crew parked in its own record, in this transaction.
    let raw: String = tx.query_row("SELECT record FROM jobs WHERE id=?1", [&crew], |r| r.get(0))?;
    let mut record: Value = serde_json::from_str(&raw)?;
    record["park"] = json!(park_id);
    tx.execute(
        "UPDATE jobs SET record=?2 WHERE id=?1",
        params![crew, record.to_string()],
    )?;
    if state == "held" {
        record_crew(
            &tx,
            &json!({"id": format!("crew-held-{park_id}"), "to": actor, "kind": "notice",
                "task_id": task, "urgency": "normal",
                "body": format!("Crew job {crew} is held: {next}. Resume it with crew.resume {{\"task_id\":\"{task}\"}}.")}),
        )?;
    } else {
        record_crew(
            &tx,
            &json!({"id": format!("crew-park-{park_id}"), "to": actor, "kind": "notice",
                "task_id": task, "urgency": "low",
                "body": format!("Crew job {crew} parked on job {child} and will resume when it settles. Next: {next}")}),
        )?;
    }
    tx.commit()?;
    Ok(park_id)
}

/// Called when a job reaches its final state: if a crew is parked on it, the
/// crew resumes. Its own park closes first if the crew is no longer parked.
pub(crate) fn settled(store: &mut Store, child: &str) -> Result<()> {
    let Ok(job) = crate::jobs::record(store, child) else {
        return Ok(());
    };
    if !matches!(
        job["status"].as_str(),
        Some("succeeded" | "failed" | "cancelled" | "timed_out")
    ) {
        return Ok(());
    }
    let parks: Vec<(String, String)> = {
        let mut stmt = store.connection().prepare(
            "SELECT id, crew_job_id FROM crew_parks WHERE child_job_id=?1 AND state='parked'",
        )?;
        let rows = stmt.query_map([child], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (park, crew) in parks {
        let record = crate::jobs::record(store, &crew);
        let owns = record.as_ref().is_ok_and(|c| c["park"] == park.as_str());
        let status = record
            .as_ref()
            .map(|c| c["status"].as_str().unwrap_or_default().to_owned())
            .unwrap_or_default();
        // Still parking: the crew's own settlement finds this child settled
        // (see `settle_own_park`), so nothing is claimed here.
        if owns && matches!(status.as_str(), "launching" | "running") {
            continue;
        }
        if !claim(store, &park, "parked")? {
            continue;
        }
        if !(owns && status == "parked") {
            set_state(store, &park, "cancelled")?;
            continue;
        }
        resume(
            store,
            &park,
            &crew,
            "Your heavy job has settled; its outcome:",
            &settlement_text(store, &job),
        )?;
    }
    Ok(())
}

/// A crew whose own payload has ended in its park: if its child already
/// settled while it was still parking, that settlement resumes it now.
pub(crate) fn settle_own_park(store: &mut Store, crew: &str) -> Result<()> {
    let Ok(job) = crate::jobs::record(store, crew) else {
        return Ok(());
    };
    if job["status"] != "parked" {
        return Ok(());
    }
    let child: Option<String> = store
        .connection()
        .query_row(
            "SELECT child_job_id FROM crew_parks WHERE id=?1 AND state='parked'",
            [job["park"].as_str().unwrap_or_default()],
            |r| r.get(0),
        )
        .optional()?;
    match child {
        Some(child) => settled(store, &child),
        None => Ok(()),
    }
}

/// Whether a fleet stow holds new launches on this host.
fn stowed(store: &Store) -> Result<bool> {
    Ok(crate::jobs::stowed(&crate::jobs::host_ledger(store)?)?.is_some())
}

/// Parks whose child settled while nothing was watching (the dispatcher was
/// down, or a settlement was missed) resume now. Each is claimed once.
pub(crate) fn resume_pending(store: &mut Store) -> Result<()> {
    // A spent cap ends a parked or held crew, even while the fleet is stowed.
    let parked: Vec<(String, String)> = {
        let mut stmt = store
            .connection()
            .prepare("SELECT id, crew_job_id FROM crew_parks WHERE state IN ('parked','held')")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (park, crew) in parked {
        if let Some(reason) = spent_cap_for_park(store, &park)? {
            store.audit(
                "crew.park_cap_spent",
                &json!({"job_id": crew, "park": park, "reason": reason}),
            )?;
            crate::jobs::end_parked(store, &crew, &format!("{reason}; the park ends"), false)?;
        }
    }
    // A stowed fleet starts nothing, so parks wait for `stow resume`.
    if stowed(store)? {
        return Ok(());
    }
    let children: Vec<String> = {
        let mut stmt = store
            .connection()
            .prepare("SELECT DISTINCT child_job_id FROM crew_parks WHERE state='parked'")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for child in children {
        if let Err(error) = settled(store, &child) {
            store.audit(
                "crew.resume_failed",
                &json!({"job_id": child, "error": format!("{error:#}")}),
            )?;
        }
    }
    recover_resuming(store)
}

/// `park N of M`: the parks a crew lineage has made against its charter's cap.
pub(crate) fn park_count(store: &Store, root: &str, task: &str) -> Result<String> {
    let parks: i64 = store.connection().query_row(
        "SELECT COUNT(*) FROM crew_parks WHERE root=?1",
        [root],
        |r| r.get(0),
    )?;
    let max = store
        .authorize(task, "spawn")
        .ok()
        .and_then(|charter| charter.get("crew_max_parks").and_then(Value::as_u64))
        .unwrap_or(MAX_PARKS);
    Ok(format!("park {parks} of {max}"))
}

/// Why a crew may no longer park: the elapsed cap, or the park count. A new
/// park needs a count under the cap; a parked crew only ends when the charter
/// has since lowered the cap below the parks it already has. `None` while it
/// may.
fn spent_cap(store: &Store, root: &str, task: &str, new_park: bool) -> Result<Option<String>> {
    let charter = store.authorize(task, "spawn").unwrap_or(Value::Null);
    let max_parks = charter
        .get("crew_max_parks")
        .and_then(Value::as_u64)
        .unwrap_or(MAX_PARKS);
    let max_elapsed = charter
        .get("crew_max_elapsed_secs")
        .and_then(Value::as_u64)
        .unwrap_or(MAX_ELAPSED_SECS);
    let created = crate::jobs::record(store, root)?["created_ms"]
        .as_i64()
        .unwrap_or_else(Store::now_ms);
    let elapsed_ms = Store::now_ms() - created;
    if elapsed_ms > (max_elapsed as i64).saturating_mul(1000) {
        return Ok(Some(format!(
            "crew {root} has run for more than {max_elapsed} seconds in total"
        )));
    }
    let parks: i64 = store.connection().query_row(
        "SELECT COUNT(*) FROM crew_parks WHERE root=?1",
        [root],
        |r| r.get(0),
    )?;
    let over = if new_park {
        parks as u64 >= max_parks
    } else {
        parks as u64 > max_parks
    };
    if over {
        return Ok(Some(format!(
            "crew {root} has parked {parks} times; the cap is {max_parks}"
        )));
    }
    Ok(None)
}

/// `spent_cap` for a park that is already open.
fn spent_cap_for_park(store: &Store, park: &str) -> Result<Option<String>> {
    let (root, task): (String, String) = store.connection().query_row(
        "SELECT root, task_id FROM crew_parks WHERE id=?1",
        [park],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    spent_cap(store, &root, &task, false)
}

/// A park whose resume was recorded `resuming`. The resumed job settles the
/// park as resumed once it is no longer queued. A resume whose job never
/// landed is left alone while its claimer still runs and is within a launch's
/// time; once the claimer is gone (it died mid-launch) or overdue, the resume
/// is retried once under its own id, and fails the crew with a notice if that
/// does not land either.
fn recover_resuming(store: &mut Store) -> Result<()> {
    let rows: Vec<(String, String, String, i64)> = {
        let mut stmt = store.connection().prepare(
            "SELECT id, crew_job_id, child_job_id, retries FROM crew_parks WHERE state='resuming'",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (park, crew, child, retries) in rows {
        // A park waits on its child: its outcome is what the resume reads.
        if !child.is_empty() {
            let settled = crate::jobs::record(store, &child).is_ok_and(|job| {
                matches!(
                    job["status"].as_str(),
                    Some("succeeded" | "failed" | "cancelled" | "timed_out")
                )
            });
            if !settled {
                continue;
            }
        }
        let attempt: i64 = store.connection().query_row(
            "SELECT attempt FROM crew_parks WHERE id=?1",
            [&park],
            |r| r.get(0),
        )?;
        let id = format!("crew-resume-{park}-{attempt}");
        let status: Option<String> = store
            .connection()
            .query_row(
                "SELECT json_extract(record,'$.status') FROM jobs WHERE id=?1",
                [&id],
                |r| r.get(0),
            )
            .optional()?;
        match status.as_deref() {
            Some("queued" | "starting") => {}
            // Its claimer has not written the job yet: it may be waiting for
            // a workspace inside agent.start.
            None if launch_in_flight(store, &park)? => {}
            Some(_) => {
                set_state(store, &park, "resumed")?;
                crate::jobs::finish_resumed(store, &crew, &id)?;
            }
            None if retries == 0 => {
                let (lead, settlement) = if child.is_empty() {
                    (
                        "Your crew was held at its time budget with no heavy job outstanding, and its first mate resumed it.".to_owned(),
                        "Continue your task from your last actions below.".to_owned(),
                    )
                } else {
                    let job = crate::jobs::record(store, &child)?;
                    (
                        "Your heavy job has settled; its outcome:".to_owned(),
                        settlement_text(store, &job),
                    )
                };
                launch_resume(store, &park, &crew, &lead, &settlement, true)?;
            }
            None => {
                set_state(store, &park, "failed")?;
                crate::jobs::fail_parked(
                    store,
                    &crew,
                    &format!("resume {id} did not start, even after one retry"),
                )?;
            }
        }
    }
    Ok(())
}

/// Claim a park's resume for this process: `resuming`, with this process
/// recorded as its claimer, only if the park is still in `from`. False when
/// another claimed it first.
fn claim(store: &Store, park: &str, from: &str) -> Result<bool> {
    let claimed = store.connection().execute(
        "UPDATE crew_parks SET state='resuming', claimer=?3, updated_ms=?4
           WHERE id=?1 AND state=?2",
        params![park, from, crate::jobs::own_identity()?, Store::now_ms()],
    )?;
    Ok(claimed == 1)
}

/// Whether a `resuming` park's launch may still be in flight: the process
/// that claimed it is still running, and the claim is younger than a launch
/// can take (it may wait for a workspace, up to its queue wait).
fn launch_in_flight(store: &Store, park: &str) -> Result<bool> {
    let (claimer, updated_ms): (Option<String>, i64) = store.connection().query_row(
        "SELECT claimer, updated_ms FROM crew_parks WHERE id=?1",
        [park],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let Some(claimer) = claimer else {
        return Ok(false);
    };
    let young = Store::now_ms() - updated_ms < LAUNCH_BOUND_MS;
    Ok(young && crate::jobs::identity_alive(&claimer)?)
}

fn set_state(store: &Store, park: &str, state: &str) -> Result<()> {
    store.connection().execute(
        "UPDATE crew_parks SET state=?2, updated_ms=?3 WHERE id=?1",
        params![park, state, Store::now_ms()],
    )?;
    Ok(())
}

/// Close a park for cancellation. Returns the child job it waited on, if the
/// park was still waiting, so the caller can cancel that job too.
pub(crate) fn cancel_park(store: &Store, park: &str) -> Result<Option<String>> {
    if park.is_empty() {
        return Ok(None);
    }
    let child: Option<String> = store
        .connection()
        .query_row(
            "SELECT child_job_id FROM crew_parks WHERE id=?1 AND state='parked'",
            [park],
            |r| r.get(0),
        )
        .optional()?;
    set_state(store, park, "cancelled")?;
    Ok(child)
}

/// Resume a parked or held crew: a new launch through agent.start, so
/// admission, the stow hold and the current routes all apply. The park's
/// route resumes only while the captain still lists it for the ship and it is
/// not backing off; otherwise the crew starts fresh on the first available
/// route. The native session resumes only when the launch that would run
/// names the harness that wrote it. The launch runs as the runtime, so it
/// carries the crew's recorded estimate and the budget it has spent.
fn resume(store: &mut Store, park: &str, crew: &str, lead: &str, settlement: &str) -> Result<()> {
    launch_resume(store, park, crew, lead, settlement, false)
}

/// A resume launch. A retry (`retry`) keeps the attempt that did not land, so
/// it runs under the same resume id.
fn launch_resume(
    store: &mut Store,
    park: &str,
    crew: &str,
    lead: &str,
    settlement: &str,
    retry: bool,
) -> Result<()> {
    let (task, ship, next, harness, attempt) = store.connection().query_row(
        "SELECT task_id, ship, next, harness, attempt FROM crew_parks WHERE id=?1",
        [park],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, i64>(4)?,
            ))
        },
    )?;
    let attempt = if retry { attempt } else { attempt + 1 };
    let crew_record = crate::jobs::record(store, crew)?;
    let launch = crew_record["launch"].clone();
    let launch_harness = launch["manifest"]
        .as_str()
        .and_then(crate::bundled::harness);
    let id = format!("crew-resume-{park}-{attempt}");
    let route_ok = crate::crew_routes::resume_route(
        store,
        &ship,
        launch["manifest"].as_str().unwrap_or_default(),
        &launch["options"],
    )?;
    // The harness the launch would run. A route the captain no longer offers
    // runs nothing of the old session: that resume starts fresh.
    let current = if route_ok {
        launch_harness.clone()
    } else {
        None
    };
    // The native session the crew reported starting, from its complete log.
    let logs = store
        .home()
        .join("jobs")
        .join(crew)
        .join(crew_record["incarnation"].as_str().unwrap_or_default());
    let session = match &launch_harness {
        Some(launch) => {
            let log = fs::read_to_string(logs.join("stdout.log")).unwrap_or_default();
            crate::activation::read_session(launch, &log)
        }
        None => None,
    };
    let same_session = session.is_some() && current.is_some() && current == harness;
    // The launch's own settings come back as they were given, on the parked
    // route or, when that route is gone, on the first one still offered.
    let mut args = json!({"id": id, "task_id": task, "wait_for_capacity": true});
    let keys: &[&str] = if route_ok {
        &[
            "heavy",
            "manifest",
            "options",
            "config",
            "tool_bundles",
            "cwd",
            "require",
            "timeout_secs",
        ]
    } else {
        &[
            "heavy",
            "config",
            "tool_bundles",
            "cwd",
            "require",
            "timeout_secs",
        ]
    };
    for key in keys {
        if !launch[key].is_null() {
            args[key] = launch[key].clone();
        }
    }
    if route_ok {
        if !crew_record["estimate"].is_null() {
            // The estimate the crew was admitted under, memory included: the
            // launch record has no route figure to size it again from.
            args["_resume_estimate"] = crew_record["estimate"].clone();
        }
        if let Some(why) = crew_record.get("route_why").and_then(Value::as_str) {
            args["_resume_why"] = json!(why);
        } else {
            args["_resume_why"] = json!("");
        }
    }
    // Each resume gets a fresh budget (see `supervise_inner`); the run time
    // carried here is recorded on the job for reporting and bearings only.
    args["_run_ms"] = json!(crew_record["run_ms"].as_u64().unwrap_or(0));
    // The parked crew this continues: its lineage admits the resume under a
    // scheduled claim, as the work it was started under.
    args["_resume_of"] = json!(crew);
    if same_session {
        // The resumed session already holds the brief and its earlier turns.
        args["options"]["resume"] = json!(session);
        args["wake_only"] = json!(true);
        args["wake"] = json!(format!("{lead}\n{settlement}\nContinue your task."));
    } else {
        let last = tail(&logs.join("stdout.log"), 2 * TAIL_BYTES);
        args["wake"] = json!(format!(
            "You are resuming crew work after parking. Your next step was: {next}\n\
             {lead}\n{settlement}\n\
             Your last actions before the park, from your log:\n{last}"
        ));
    }
    // This process launches it, so it is the claimer recovery checks. The
    // session it resumes is recorded first: a bounded launch resumes only the
    // session of a crew of its task whose park is resuming it, under the plan
    // that crew launched with (see `tool_bundles::record_plan`).
    let session = session.filter(|_| same_session);
    store.connection().execute(
        "UPDATE crew_parks SET attempt=?2, updated_ms=?3, retries=retries+?4, claimer=?5,
           session=?6
           WHERE id=?1",
        params![
            park,
            attempt,
            Store::now_ms(),
            i64::from(retry),
            crate::jobs::own_identity()?,
            session
        ],
    )?;
    // A resume is the runtime's own launch: a crew's agent caller would not
    // be allowed to set the budget it carries, nor would the first mate.
    let caller = store.caller().clone();
    store.set_caller(Caller::Operator);
    let started = crate::dispatch(store, "agent.start", args);
    store.set_caller(caller);
    match started {
        Ok(started) => {
            let resumed = started["id"].as_str().unwrap_or(&id).to_owned();
            let admitted = !matches!(started["status"].as_str(), Some("queued" | "starting"));
            store.connection().execute(
                "UPDATE crew_parks SET state=?2, resumed_job_id=?3, session=?4, updated_ms=?5
                   WHERE id=?1",
                params![
                    park,
                    if admitted { "resumed" } else { "resuming" },
                    resumed,
                    session,
                    Store::now_ms()
                ],
            )?;
            // The crew's record now says where its work went; it reports nothing.
            crate::jobs::finish_resumed(store, crew, &resumed)?;
        }
        Err(_) if stowed(store)? => {
            // A stow holds the resume like any launch: it is not a failure. The
            // park waits, and the next tick resumes it once the stow is lifted.
            set_state(store, park, "parked")?;
        }
        Err(error) => {
            store.connection().execute(
                "UPDATE crew_parks SET state='failed', updated_ms=?2 WHERE id=?1",
                params![park, Store::now_ms()],
            )?;
            crate::jobs::fail_parked(store, crew, &format!("resume failed: {error:#}"))?;
        }
    }
    Ok(())
}

/// `crew.resume {"task_id":...}`: the task's first mate (or the operator)
/// resumes its held crews, those whose budget ran out with no heavy job
/// outstanding.
fn resume_held(store: &mut Store, args: &Value) -> Result<Value> {
    let task = required(args, "task_id")?.to_owned();
    let owner = store.task(&task)?["owner"]
        .as_str()
        .context("task owner missing")?
        .to_owned();
    if let Caller::Agent { actor } = store.caller() {
        ensure!(
            actor == &owner,
            "{actor} can resume only the crews of its own tasks"
        );
    }
    let held: Vec<(String, String)> = {
        let mut stmt = store.connection().prepare(
            "SELECT p.id, p.crew_job_id FROM crew_parks p JOIN jobs j ON j.id=p.crew_job_id
               WHERE p.task_id=?1 AND p.state='held' AND json_extract(j.record,'$.status')='parked'",
        )?;
        let rows = stmt.query_map([&task], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    ensure!(!held.is_empty(), "task {task} has no held crew to resume");
    // A resume is one more run: refused once the crew's caps are spent, as at
    // a deadline. The dispatcher's next pass ends such a crew.
    for (park, _) in &held {
        if let Some(reason) = spent_cap_for_park(store, park)? {
            bail!("crew.resume refused: {reason}");
        }
    }
    let mut resumed = vec![];
    for (park, crew) in held {
        if !claim(store, &park, "held")? {
            continue;
        }
        resume(
            store,
            &park,
            &crew,
            "Your crew was held at its time budget with no heavy job outstanding, and its first mate resumed it.",
            "Continue your task from your last actions below.",
        )?;
        resumed.push(json!({"park": park, "crew_job_id": crew}));
    }
    Ok(json!({"task_id": task, "resumed": resumed}))
}

/// Crews of a ship that are parked on a heavy job or held, for bearings.
pub(crate) fn parked_lines(store: &Store, ship: &str) -> Result<Vec<String>> {
    let mut stmt = store.connection().prepare(
        "SELECT p.crew_job_id, p.child_job_id, p.next, p.state, p.task_id, p.root FROM crew_parks p
           JOIN jobs j ON j.id=p.crew_job_id
           WHERE p.ship=?1 AND p.state IN ('parked','held')
             AND json_extract(j.record,'$.status') IN ('parked','launching','running')
           ORDER BY p.created_ms LIMIT 20",
    )?;
    let rows = stmt.query_map([ship], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
        ))
    })?;
    let mut lines = vec![];
    for row in rows {
        let (crew, child, next, state, task, root) = row?;
        let count = park_count(store, &root, &task)?;
        lines.push(if state == "held" {
            format!(
                "Crew {crew} is held at its time budget with no heavy job outstanding ({count}); crew.resume {{\"task_id\":\"{task}\"}} continues it."
            )
        } else {
            format!(
                "Crew {crew} is parked on job {child} ({count}) and resumes when it settles. Next: {}",
                clip(&next, NEXT_CHARS)
            )
        });
    }
    Ok(lines)
}

/// The crews of a task that are parked or held: a launch on the task waits
/// for them, unless it replaces them.
pub(crate) fn parked_crews(store: &Store, task: &str) -> Result<Vec<String>> {
    let mut stmt = store.connection().prepare(
        "SELECT p.crew_job_id FROM crew_parks p JOIN jobs j ON j.id=p.crew_job_id
           WHERE p.task_id=?1 AND p.state IN ('parked','held')
             AND json_extract(j.record,'$.status') IN ('parked','launching','running')",
    )?;
    let rows = stmt.query_map([task], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "crew.park" => park(store, &args),
        "crew.resume" => resume_held(store, &args),
        "crew.report" => report(store, &args),
        "crew.answer" => answer(store, &args),
        _ => bail!("unknown crew command {command}"),
    }
}
