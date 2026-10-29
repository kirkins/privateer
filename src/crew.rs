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

/// Only cap exhaustion turns an otherwise normal session exit into a timeout.
#[derive(Debug)]
pub(crate) struct ParkCapRefusal(String);

impl std::fmt::Display for ParkCapRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParkCapRefusal {}

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
    if let Some(mismatch) = job.get("route_mismatch").filter(|m| m.is_object()) {
        body.push_str(&format!(
            "Route mismatch: the launch named model {}, the harness reported using {}.\n",
            clip(mismatch["expected"].as_str().unwrap_or("?"), REASON_CHARS),
            clip(&mismatch["used"].to_string(), REASON_CHARS)
        ));
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
        "retry_of": job["retry_of"], "route_mismatch": job["route_mismatch"],
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
/// A late-resume cancel that keeps failing is retried on an exponential
/// schedule, so a stuck payload cannot make every dispatcher pass re-run its
/// kill waits and write another failure row. The cap stays in minutes: the
/// intent is never dropped while its incarnation exists, so custody is
/// re-checked for as long as the job is stuck.
const RESUME_CANCEL_BACKOFF_BASE_MS: i64 = 30_000;
const RESUME_CANCEL_BACKOFF_MAX_MS: i64 = 10 * 60_000;

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
/// its ship, not a first mate's wake.
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
    park_automatically(store, crew, true)
}

/// A hosted harness may finish its turn just after starting a job, without
/// explicitly parking. Keep custody of that outcome instead of reporting the
/// crew finished. Unlike a deadline, a normal exit with no work is not held.
/// Called only after the payload and all its descendants have stopped.
pub(crate) fn park_on_exit(store: &Store, crew: &str) -> Result<Option<String>> {
    park_automatically(store, crew, false)
}

fn park_automatically(store: &Store, crew: &str, deadline: bool) -> Result<Option<String>> {
    // Wait behind any admission that checked this crew just before its
    // deadline but has not committed its child yet. Later admissions are
    // refused by check_starting_crew, so the scan stays complete after commit.
    let tx = store.write_transaction()?;
    let job = crate::jobs::record(store, crew)?;
    // Only a crew (a spawned agent job) parks; an agent's own build does not.
    if job["action"] != "spawn"
        || job["agent_actor"].is_null()
        || (!deadline && job["hosted"] != json!(true))
        || job["cancel_requested"] == json!(true)
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
    if !deadline && waiting.is_none() {
        return Ok(None);
    }
    // A spent cap ends the crew at its deadline or exit: the job it would wait on is
    // cancelled, and the job times out with the reason.
    let task = job["task_id"].as_str().context("job task")?.to_owned();
    let root = chain.last().context("crew lineage")?.clone();
    let spent = spent_cap(store, &root, &task, true)?;
    // Cancellation and opening a park each take their own write transaction.
    tx.commit()?;
    if let Some(reason) = spent {
        if let Some(child) = &waiting {
            crate::jobs::cancel_crew(store, child)?;
        }
        let when = if deadline { "deadline" } else { "session exit" };
        return Err(ParkCapRefusal(format!("{reason}; the crew ends at its {when}")).into());
    }
    let parked = match &waiting {
        Some(child) => (
            child.as_str(),
            "parked",
            if deadline {
                format!("its time budget ran out while job {child} runs")
            } else {
                format!(
                    "its hosted session ended while job {child} runs; read the outcome and finish the task"
                )
            },
        ),
        None => (
            "",
            "held",
            "its time budget ran out with no heavy job outstanding".to_owned(),
        ),
    };
    // On exit, the selected child may settle between the scan and the park.
    // Still park on it: settle_own_park will deliver that outcome immediately.
    Ok(Some(open_park_inner(
        store, &job, parked.0, &parked.2, parked.1, !deadline,
    )?))
}

/// Open a park on `child` (empty for a held crew), in one transaction with
/// the crew's own record: the crew is marked parked on it, and its ship is
/// told: quietly for a park, and as a normal notice for a held crew.
fn open_park(store: &Store, job: &Value, child: &str, next: &str, state: &str) -> Result<String> {
    open_park_inner(store, job, child, next, state, false)
}

fn open_park_inner(
    store: &Store,
    job: &Value,
    child: &str,
    next: &str,
    state: &str,
    allow_settled: bool,
) -> Result<String> {
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
    if elapsed_ms > (max_elapsed as i64).saturating_mul(1000) {
        return Err(ParkCapRefusal(format!(
            "crew {root} has run for more than {max_elapsed} seconds in total; it cannot park again"
        ))
        .into());
    }
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
            allow_settled
                || matches!(
                    child_status.as_str(),
                    "starting" | "queued" | "launching" | "running"
                ),
            "job {child} already {child_status}; read its outcome instead of parking"
        );
    }
    // Still the same running crew, checked again inside the transaction.
    let (crew_status, cancel_requested): (String, bool) = tx.query_row(
        "SELECT json_extract(record,'$.status'), COALESCE(json_extract(record,'$.cancel_requested'),0) FROM jobs WHERE id=?1",
        [&crew],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(
        matches!(crew_status.as_str(), "launching" | "running"),
        "job {crew} is {crew_status}, not running"
    );
    // Serialize with cancellation's graph check and durable intent. A crew
    // cannot add a park/child edge after that check has admitted cancellation.
    ensure!(
        !cancel_requested,
        "job {crew} has cancellation requested; it cannot park"
    );
    let parks: i64 = tx.query_row(
        "SELECT COUNT(*) FROM crew_parks WHERE root=?1",
        [&root],
        |r| r.get(0),
    )?;
    if parks as u64 >= max_parks {
        return Err(ParkCapRefusal(format!(
            "crew {root} has parked {parks} times; the cap is {max_parks}"
        ))
        .into());
    }
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
        // A parked record alone is not proof the old session stopped: the
        // supervisor may have left a live descendant and kept its reservation.
        // Never resume into the same session/worktree beside that payload.
        if owns && status == "parked" && crate::jobs::crew_payload_alive(store, &crew)? {
            continue;
        }
        if owns
            && status == "parked"
            && let Some(reason) = spent_cap_for_park(store, &park)?
        {
            crate::jobs::end_spent_park(store, &crew, &format!("{reason}; the park ends"))?;
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
    // Cancellation never starts work: retry even while the fleet is stowed.
    retry_resume_cancels(store)?;
    // A spent cap ends a parked or held crew, even while the fleet is stowed.
    let parked: Vec<(String, String)> = {
        let mut stmt = store.connection().prepare(
            "SELECT id, crew_job_id FROM crew_parks WHERE state IN ('parked','held','resuming')",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (park, crew) in parked {
        if let Some(reason) = spent_cap_for_park(store, &park)? {
            store.audit(
                "crew.park_cap_spent",
                &json!({"job_id": crew, "park": park, "reason": reason}),
            )?;
            crate::jobs::end_spent_park(store, &crew, &format!("{reason}; the park ends"))?;
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
                // Its claimer may have died before linking the park to it.
                settle_resume_link(store, &park, &crew, "resumed", &id)?;
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
                let failed = store.connection().execute(
                    "UPDATE crew_parks SET state='failed', updated_ms=?2
                       WHERE id=?1 AND state='resuming' AND attempt=?3 AND retries=?4",
                    params![park, Store::now_ms(), attempt, retries],
                )?;
                if failed != 0 {
                    crate::jobs::fail_parked(
                        store,
                        &crew,
                        &format!("resume {id} did not start, even after one retry"),
                    )?;
                }
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

/// Record the job a park's resume launched, so `lineage` follows it back to
/// the crew it continues, and the park's state with it. Only an unresolved
/// resume can be linked: recovery may already have seen the job running while
/// its launcher still holds a starting/queued response. That late response
/// must not move a resumed park back to resuming (or reopen a closed park).
fn link_resume(store: &Store, park: &str, state: &str, resumed: &str) -> Result<usize> {
    Ok(store.connection().execute(
        "UPDATE crew_parks SET state=?2, resumed_job_id=?3, updated_ms=?4
           WHERE id=?1 AND state='resuming'",
        params![park, state, resumed, Store::now_ms()],
    )?)
}

/// Recovery may have linked this job first, or the park may have closed while
/// agent.start waited. Only the former owns a continuation; cancel any other
/// late launch through the native path, with a durable explanation of custody.
fn settle_resume_link(
    store: &Store,
    park: &str,
    crew: &str,
    state: &str,
    resumed: &str,
) -> Result<()> {
    if link_resume(store, park, state, resumed)? == 0 {
        let (current, linked): (String, Option<String>) = store.connection().query_row(
            "SELECT state, resumed_job_id FROM crew_parks WHERE id=?1",
            [park],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if current != "resumed" || linked.as_deref() != Some(resumed) {
            // A closed park is no longer scanned by resume recovery. Commit
            // retry custody with the audit before native cancellation can fail
            // (or this process can die), without overwriting the park's link.
            let tx = store.write_transaction()?;
            let job = crate::jobs::record(store, resumed)?;
            let incarnation = required(&job, "incarnation")?;
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO crew_resume_cancels(job_id, incarnation) VALUES(?1, ?2)",
                params![resumed, incarnation],
            )?;
            if inserted != 0 {
                store.audit(
                    "crew.resume_cancel_requested",
                    &json!({"park": park, "crew_job_id": crew, "resumed_job_id": resumed,
                        "incarnation": incarnation, "park_state": current, "linked_job_id": linked}),
                )?;
            }
            tx.commit()?;
            return cancel_late_resume(store, resumed, incarnation);
        }
    }
    // The crew's record now says where its work went; it reports nothing.
    crate::jobs::finish_resumed(store, crew, resumed)
}

/// An intent is retired only after native cancellation accepts it, or the
/// recorded incarnation no longer exists. A retry must not cancel a newer job
/// that happens to reuse the same id. This queue does not depend on audit
/// retention, and survives a failed cancel or a dispatcher restart.
pub(crate) fn cancel_late_resume(store: &Store, job: &str, incarnation: &str) -> Result<()> {
    let current: Option<String> = store
        .connection()
        .query_row(
            "SELECT json_extract(record,'$.incarnation') FROM jobs WHERE id=?1",
            [job],
            |r| r.get(0),
        )
        .optional()?;
    if current.as_deref() == Some(incarnation) {
        crate::jobs::cancel_crew_exact(store, job, incarnation)?;
    }
    store.connection().execute(
        "DELETE FROM crew_resume_cancels WHERE job_id=?1 AND incarnation=?2",
        params![job, incarnation],
    )?;
    Ok(())
}

/// Retry the cancels whose backoff has elapsed. An intent that keeps failing
/// re-runs its kill waits at exponentially widening boundaries only, so a
/// stuck payload can neither slow every dispatcher pass nor grow the audit
/// log without bound; the intent itself is kept until it retires.
fn retry_resume_cancels(store: &Store) -> Result<()> {
    let pending: Vec<(String, String)> = {
        let mut stmt = store.connection().prepare(
            "SELECT job_id, incarnation FROM crew_resume_cancels WHERE next_attempt_ms<=?1",
        )?;
        stmt.query_map([Store::now_ms()], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    for (job, incarnation) in pending {
        if let Err(error) = cancel_late_resume(store, &job, &incarnation) {
            defer_resume_cancel(store, &job, &incarnation, &error)?;
        }
    }
    Ok(())
}

/// The wait after a cancel's `attempts`th failed dispatcher retry: 30 s,
/// doubling, capped at ten minutes. A corrupted attempts count saturates at
/// the cap instead of overflowing.
fn resume_cancel_backoff_ms(attempts: i64) -> i64 {
    (RESUME_CANCEL_BACKOFF_BASE_MS << (attempts.clamp(1, 6) - 1)).min(RESUME_CANCEL_BACKOFF_MAX_MS)
}

/// Keep a failed cancel's intent and schedule its next attempt. The one
/// failure audit row lands with the schedule, so at most one is written per
/// backoff step, and only while this pass still owns the row: an intent a
/// concurrent pass retired or advanced is not deferred or audited again.
fn defer_resume_cancel(
    store: &Store,
    job: &str,
    incarnation: &str,
    error: &anyhow::Error,
) -> Result<()> {
    let attempts: Option<i64> = store
        .connection()
        .query_row(
            "SELECT attempts FROM crew_resume_cancels WHERE job_id=?1 AND incarnation=?2",
            params![job, incarnation],
            |r| r.get(0),
        )
        .optional()?;
    let Some(previous) = attempts else {
        // The intent retired while this attempt ran; nothing to defer.
        return Ok(());
    };
    let attempts = previous.saturating_add(1);
    let next_attempt_ms = Store::now_ms().saturating_add(resume_cancel_backoff_ms(attempts));
    let deferred = store.connection().execute(
        "UPDATE crew_resume_cancels SET attempts=?3, next_attempt_ms=?4
           WHERE job_id=?1 AND incarnation=?2 AND attempts=?5",
        params![job, incarnation, attempts, next_attempt_ms, previous],
    )?;
    if deferred != 0 {
        store.audit(
            "crew.resume_cancel_failed",
            &json!({"job_id": job, "incarnation": incarnation, "attempts": attempts,
                "next_attempt_ms": next_attempt_ms, "error": format!("{error:#}")}),
        )?;
    }
    Ok(())
}

fn set_state(store: &Store, park: &str, state: &str) -> Result<()> {
    store.connection().execute(
        "UPDATE crew_parks SET state=?2, updated_ms=?3 WHERE id=?1",
        params![park, state, Store::now_ms()],
    )?;
    Ok(())
}

/// Close custody before stopping its work. A continuation may already exist
/// even if its launcher has not linked it yet. Record incarnation-bound cancel
/// intents in the same transaction so a failed cancel or restart cannot lose it.
pub(crate) fn cancel_park(
    store: &Store,
    park: &str,
    quiet: bool,
    cap_only: bool,
) -> Result<Option<Vec<(String, String)>>> {
    if park.is_empty() {
        return Ok((!cap_only).then(Vec::new));
    }
    let tx = store.write_transaction()?;
    let row: Option<(String, String, i64, String)> = tx
        .query_row(
            "SELECT child_job_id, crew_job_id, attempt, state FROM crew_parks
               WHERE id=?1 AND state IN ('parked','held','resuming','resumed')",
            [park],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((child, crew, attempt, state)) = row else {
        return Ok((!cap_only).then(Vec::new));
    };
    if cap_only {
        // Fence a new run, preserving a budget already granted. The write
        // transaction serializes this decision with admission and linking.
        if spent_cap_for_park(store, park)?.is_none() {
            return Ok(None);
        }
        if matches!(state.as_str(), "resuming" | "resumed") {
            let id = format!("crew-resume-{park}-{attempt}");
            if let Ok(job) = crate::jobs::record(store, &id) {
                let before_spawn = job["status"] == "launching"
                    && job["payload"].is_null()
                    && crate::jobs::is_recorded_supervisor(&job["supervisor"], std::process::id())?;
                if !matches!(job["status"].as_str(), Some("starting" | "queued")) && !before_spawn {
                    return Ok(None);
                }
            }
        }
    }
    let mut targets = vec![];
    let own = crate::jobs::record(store, &crew)?;
    if !matches!(
        own["status"].as_str(),
        Some("succeeded" | "failed" | "cancelled" | "timed_out" | "resumed")
    ) {
        targets.push((crew.clone(), required(&own, "incarnation")?.to_owned()));
    }
    if !child.is_empty()
        && matches!(state.as_str(), "parked" | "resuming")
        && let Ok(job) = crate::jobs::record(store, &child)
    {
        targets.push((child, required(&job, "incarnation")?.to_owned()));
    }
    if matches!(state.as_str(), "resuming" | "resumed") {
        let id = format!("crew-resume-{park}-{attempt}");
        if let Ok(job) = crate::jobs::record(store, &id)
            && job["resume_of"] == crew
        {
            targets.push((id, required(&job, "incarnation")?.to_owned()));
        }
    }
    tx.execute(
        "UPDATE crew_parks SET state='cancelled', updated_ms=?2 WHERE id=?1",
        params![park, Store::now_ms()],
    )?;
    if quiet {
        for id in std::iter::once(&crew).chain(targets.iter().map(|(id, _)| id)) {
            tx.execute(
                "UPDATE jobs SET record=json_set(record,'$.quiet',json('true')) WHERE id=?1",
                [id],
            )?;
        }
    }
    for (id, incarnation) in &targets {
        let by = match store.caller() {
            Caller::Agent { actor } => actor.as_str(),
            Caller::Operator => "operator",
        };
        crate::jobs::request_cancel_in(store, id, incarnation, by)?;
        tx.execute(
            "INSERT OR IGNORE INTO crew_resume_cancels(job_id, incarnation) VALUES(?1, ?2)",
            params![id, incarnation],
        )?;
    }
    tx.commit()?;
    Ok(Some(targets))
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
    launch_resume_with(store, park, crew, lead, settlement, retry, |store, args| {
        crate::dispatch(store, "agent.start", args)
    })
}

/// Keep the launch boundary injectable so tests can order recovery or closure
/// during agent.start, then exercise the real reply handling below.
fn launch_resume_with(
    store: &mut Store,
    park: &str,
    crew: &str,
    lead: &str,
    settlement: &str,
    retry: bool,
    start: impl FnOnce(&mut Store, Value) -> Result<Value>,
) -> Result<()> {
    if let Some(reason) = spent_cap_for_park(store, park)? {
        crate::jobs::end_spent_park(store, crew, &format!("{reason}; the park ends"))?;
        return Ok(());
    }
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
    let previous_attempt = attempt;
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
    // The native session the crew last reported starting, from its complete
    // log.
    let logs = store
        .home()
        .join("jobs")
        .join(crew)
        .join(crew_record["incarnation"].as_str().unwrap_or_default());
    let session = match &launch_harness {
        Some(launch) => {
            let log = fs::read_to_string(logs.join("stdout.log")).unwrap_or_default();
            crate::activation::latest_session(launch, &log)
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
    if let Some(timeout) = args["timeout_secs"].as_u64() {
        let charter = store.authorize(&task, "spawn")?;
        let cap = charter["job_timeout_secs"].as_u64().unwrap_or(900);
        args["timeout_secs"] = json!(timeout.min(cap));
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
    let claimer = crate::jobs::own_identity()?;
    let updated = store.connection().execute(
        "UPDATE crew_parks SET attempt=?2, updated_ms=?3, retries=retries+?4, claimer=?5,
           session=?6
           WHERE id=?1 AND state='resuming' AND attempt=?7",
        params![
            park,
            attempt,
            Store::now_ms(),
            i64::from(retry),
            claimer,
            session,
            previous_attempt
        ],
    )?;
    if updated == 0 {
        return Ok(());
    }
    // A resume is the runtime's own launch: a crew's agent caller would not
    // be allowed to set the budget it carries, nor would the first mate.
    let caller = store.caller().clone();
    store.set_caller(Caller::Operator);
    let started = start(store, args);
    store.set_caller(caller);
    match started {
        Ok(started) => {
            let resumed = started["id"].as_str().unwrap_or(&id).to_owned();
            let admitted = !matches!(started["status"].as_str(), Some("queued" | "starting"));
            let state = if admitted { "resumed" } else { "resuming" };
            settle_resume_link(store, park, crew, state, &resumed)?;
        }
        Err(error) if error.is::<ParkCapRefusal>() => {
            crate::jobs::end_spent_park(store, crew, &format!("{error}; the park ends"))?;
        }
        Err(_) if stowed(store)? => {
            // A stow holds the resume like any launch: it is not a failure. The
            // park waits, and the next tick resumes it once the stow is lifted.
            store.connection().execute(
                "UPDATE crew_parks SET state='parked', updated_ms=?2
                   WHERE id=?1 AND state='resuming' AND attempt=?3 AND claimer=?4",
                params![park, Store::now_ms(), attempt, claimer],
            )?;
        }
        Err(error) => {
            let failed = store.connection().execute(
                "UPDATE crew_parks SET state='failed', updated_ms=?2
                   WHERE id=?1 AND state='resuming' AND attempt=?3 AND claimer=?4",
                params![park, Store::now_ms(), attempt, claimer],
            )?;
            if failed != 0 {
                crate::jobs::fail_parked(store, crew, &format!("resume failed: {error:#}"))?;
            }
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
    let mut skipped = vec![];
    for (park, crew) in held {
        if crate::jobs::crew_payload_alive(store, &crew)? {
            skipped.push(json!({"park": park, "crew_job_id": crew,
                "reason": "old crew payload or descendants are still alive; retry after they stop"}));
            continue;
        }
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
    Ok(json!({"task_id": task, "resumed": resumed, "skipped": skipped}))
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
           WHERE p.task_id=?1 AND (p.state='resuming' OR
             (p.state IN ('parked','held') AND json_extract(j.record,'$.status') IN ('parked','launching','running')))",
    )?;
    let rows = stmt.query_map([task], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Only the runtime's exact continuation may pass its own park's custody.
/// Recheck at native admission and after waits: a launcher can outlive a close.
pub(crate) fn launch_blockers(
    store: &Store,
    task: &str,
    id: &str,
    resume_of: Option<&str>,
) -> Result<Vec<String>> {
    if let Some(crew) = resume_of {
        let park: Option<String> = store
            .connection()
            .query_row(
                "SELECT id FROM crew_parks WHERE task_id=?1 AND crew_job_id=?2
               AND ?3='crew-resume-' || id || '-' || attempt
               AND ((state='resuming' AND (resumed_job_id IS NULL OR resumed_job_id=?3))
                 OR (state='resumed' AND resumed_job_id=?3))",
                params![task, crew, id],
                |r| r.get(0),
            )
            .optional()?;
        let park = park.with_context(|| format!("crew resume {id} no longer owns its park"))?;
        if let Some(reason) = spent_cap_for_park(store, &park)? {
            return Err(ParkCapRefusal(reason).into());
        }
    }
    Ok(parked_crews(store, task)?
        .into_iter()
        .filter(|crew| Some(crew.as_str()) != resume_of)
        .collect())
}

/// The argument keys of each command here; see `crate::Accepted`.
pub(crate) const ACCEPTED: crate::Accepted = &[
    ("crew.park", &["job_id", "next"]),
    ("crew.report", &["kind", "body", "payload", "id"]),
    ("crew.answer", &["id", "wait_secs"]),
    ("crew.resume", &["task_id"]),
];

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "crew.park" => park(store, &args),
        "crew.resume" => resume_held(store, &args),
        "crew.report" => report(store, &args),
        "crew.answer" => answer(store, &args),
        _ => bail!("unknown crew command {command}"),
    }
}

#[cfg(test)]
mod recover_tests {
    use super::*;

    fn insert_job(store: &Store, id: &str, status: &str) {
        let record = json!({
            "id": id, "task_id": "fix", "incarnation": "one",
            "command": ["claude"], "cwd": store.home(), "action": "spawn", "heavy": false,
            "timeout_secs": 60, "ledger": store.home().join("ledger.sqlite"),
            "status": status, "created_ms": Store::now_ms(), "finished_ms": null,
            "supervisor": null, "payload": null, "cancel_requested": false,
            "exit_code": null, "reason": null, "retry_of": null,
            "agent_actor": "engineering",
        });
        store
            .connection()
            .execute(
                "INSERT INTO jobs(id, record) VALUES(?1, ?2)",
                params![id, record.to_string()],
            )
            .unwrap();
    }

    fn insert_park(store: &Store, id: &str, crew: &str, state: &str, resumed: Option<&str>) {
        // A real park always retains its root job for elapsed-cap checks.
        if crate::jobs::record(store, "crew-1").is_err() {
            insert_job(store, "crew-1", "resumed");
        }
        store
            .connection()
            .execute(
                "INSERT INTO crew_parks(id, root, crew_job_id, child_job_id, task_id, ship,
                   state, next, resumed_job_id, attempt, created_ms, updated_ms)
                 VALUES(?1, 'crew-1', ?2, '', 'fix', 'engineering', ?3, 'go on', ?4, 1, ?5, ?5)",
                params![id, crew, state, resumed, Store::now_ms()],
            )
            .unwrap();
    }

    #[test]
    fn only_the_exact_runtime_continuation_passes_its_park() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        insert_job(&store, "crew-alpha", "parked");
        insert_park(&store, "park-alpha", "crew-alpha", "resuming", None);
        let id = "crew-resume-park-alpha-1";
        assert_eq!(
            launch_blockers(&store, "fix", "other", None).unwrap(),
            ["crew-alpha"]
        );
        assert!(
            launch_blockers(&store, "fix", id, Some("crew-alpha"))
                .unwrap()
                .is_empty()
        );
        assert!(launch_blockers(&store, "fix", "other", Some("crew-alpha")).is_err());
        assert!(launch_blockers(&store, "fix", id, Some("other")).is_err());
        store
            .connection()
            .execute(
                "UPDATE crew_parks SET state='resumed', resumed_job_id=?1 WHERE id='park-alpha'",
                [id],
            )
            .unwrap();
        assert!(
            launch_blockers(&store, "fix", id, Some("crew-alpha"))
                .unwrap()
                .is_empty()
        );
        set_state(&store, "park-alpha", "cancelled").unwrap();
        assert!(launch_blockers(&store, "fix", id, Some("crew-alpha")).is_err());
    }

    #[test]
    fn launch_errors_cannot_reopen_a_closed_park_even_while_stowed() {
        for stow in [false, true] {
            for closed in ["cancelled", "failed", "resumed"] {
                let temp = tempfile::tempdir().unwrap();
                let mut store = Store::open(temp.path()).unwrap();
                fs::write(
                    store.home().join("config.json"),
                    json!({
                        "resource_home": store.home().join("host-ledger")
                    })
                    .to_string(),
                )
                .unwrap();
                if stow {
                    crate::jobs::host_ledger(&store)
                        .unwrap()
                        .execute(
                            "INSERT INTO stows(id,started_ms,ships) VALUES('test',0,'[]')",
                            [],
                        )
                        .unwrap();
                }
                insert_job(&store, "crew-alpha", "parked");
                insert_park(&store, "park-alpha", "crew-alpha", "resuming", None);
                launch_resume_with(
                    &mut store,
                    "park-alpha",
                    "crew-alpha",
                    "settled",
                    "continue",
                    false,
                    |store, _| {
                        set_state(store, "park-alpha", closed)?;
                        bail!("injected launch error")
                    },
                )
                .unwrap();
                let state: String = store
                    .connection()
                    .query_row(
                        "SELECT state FROM crew_parks WHERE id='park-alpha'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(state, closed, "stow={stow}");
                assert_eq!(
                    crate::jobs::record(&store, "crew-alpha").unwrap()["status"],
                    "parked"
                );
            }
        }
    }

    #[test]
    fn cancellation_follows_a_selected_park_that_just_finished_resuming() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        insert_job(&store, "crew-alpha", "resumed");
        let id = "crew-resume-park-alpha-1";
        insert_job(&store, id, "starting");
        insert_park(&store, "park-alpha", "crew-alpha", "resuming", None);
        store.connection().execute_batch(
            "UPDATE jobs SET record=json_set(record,'$.park','park-alpha') WHERE id='crew-alpha';
             UPDATE jobs SET record=json_set(record,'$.resume_of','crew-alpha') WHERE id='crew-resume-park-alpha-1';",
        ).unwrap();
        let selected = parked_crews(&store, "fix").unwrap();
        assert_eq!(selected, ["crew-alpha"]);
        link_resume(&store, "park-alpha", "resumed", id).unwrap();
        crate::jobs::cancel_crew(&store, &selected[0]).unwrap();
        assert_eq!(
            crate::jobs::record(&store, id).unwrap()["status"],
            "cancelled"
        );
        let state: String = store
            .connection()
            .query_row(
                "SELECT state FROM crew_parks WHERE id='park-alpha'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "cancelled");
    }

    #[test]
    fn cancellation_committed_after_the_initial_park_read_refuses_without_writes() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        store.connection().execute_batch("INSERT INTO charters(id,data) VALUES('alpha','{\"id\":\"alpha\",\"active\":true,\"actions\":[\"spawn\"]}');
            INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('fix','alpha','alpha','fix','open',0);").unwrap();
        insert_job(&store, "alpha", "running");
        insert_job(&store, "beta", "running");
        store
            .connection()
            .execute_batch("UPDATE jobs SET record=json_set(record,'$.agent_actor','alpha');")
            .unwrap();
        let stale = crate::jobs::record(&store, "alpha").unwrap();
        let tx = store.write_transaction().unwrap();
        crate::jobs::request_cancel_in(&store, "alpha", "one", "door:alpha").unwrap();
        tx.commit().unwrap();
        let before = crate::jobs::record(&store, "alpha").unwrap();
        let audit = || {
            store
                .connection()
                .prepare("SELECT kind, payload FROM audit ORDER BY rowid")
                .unwrap()
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        let before_audit = audit();
        for (child, state) in [("beta", "parked"), ("", "held")] {
            let error =
                open_park_inner(&store, &stale, child, "check output", state, true).unwrap_err();
            assert!(
                error.to_string().contains("cancellation requested"),
                "{error:#}"
            );
            assert_eq!(crate::jobs::record(&store, "alpha").unwrap(), before);
            let parks: i64 = store
                .connection()
                .query_row("SELECT count(*) FROM crew_parks", [], |r| r.get(0))
                .unwrap();
            assert_eq!(parks, 0);
            assert_eq!(audit(), before_audit);
        }
    }

    #[test]
    fn a_budget_lowered_between_resume_planning_and_dispatch_is_clamped() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp.path()).unwrap();
        fs::write(
            store.home().join("config.json"),
            json!({
                "ship":"engineering", "resource_home":store.home().join("host-ledger")
            })
            .to_string(),
        )
        .unwrap();
        let charter = json!({"id":"alpha", "active":true, "actions":["spawn"],
            "repo":store.home(), "job_timeout_secs":120});
        store
            .connection()
            .execute(
                "INSERT INTO charters(id,data) VALUES('alpha',?1)",
                [charter.to_string()],
            )
            .unwrap();
        store.connection().execute_batch(
            "INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('fix','alpha','engineering','fix','open',0);",
        ).unwrap();
        crate::jobs::host_ledger(&store)
            .unwrap()
            .execute(
                "INSERT INTO stows(id,started_ms,ships) VALUES('test',0,'[]')",
                [],
            )
            .unwrap();
        insert_job(&store, "crew-alpha", "parked");
        insert_park(&store, "park-alpha", "crew-alpha", "resuming", None);
        store.connection().execute_batch(
            "UPDATE jobs SET record=json_set(record,'$.park','park-alpha','$.launch',json('{\"manifest\":\"bundled:codex\",\"timeout_secs\":90}')) WHERE id='crew-alpha';",
        ).unwrap();
        launch_resume_with(&mut store, "park-alpha", "crew-alpha", "settled", "continue", false,
            |store, args| {
                assert_eq!(args["timeout_secs"], 90);
                store.connection().execute_batch(
                    "UPDATE charters SET data=json_set(data,'$.job_timeout_secs',60) WHERE id='alpha';",
                )?;
                crate::dispatch(store, "agent.start", args)
            },
        ).unwrap();
        let id = "crew-resume-park-alpha-2";
        let job = crate::jobs::record(&store, id).unwrap();
        assert_eq!(job["timeout_secs"], 60, "{job}");
        assert_eq!(job["status"], "queued", "{job}");
        crate::jobs::cancel_crew(&store, "crew-alpha").unwrap();
    }

    #[test]
    fn a_spent_cap_does_not_end_a_continuation_that_already_started() {
        for state in ["resuming", "resumed"] {
            let temp = tempfile::tempdir().unwrap();
            let mut store = Store::open(temp.path()).unwrap();
            fs::write(
                store.home().join("config.json"),
                json!({
                    "resource_home": store.home().join("host-ledger")
                })
                .to_string(),
            )
            .unwrap();
            insert_job(&store, "crew-alpha", "resumed");
            let id = "crew-resume-park-alpha-1";
            insert_job(&store, id, "running");
            insert_park(&store, "park-alpha", "crew-alpha", state, Some(id));
            store.connection().execute_batch(
                "UPDATE jobs SET record=json_set(record,'$.park','park-alpha') WHERE id='crew-alpha';
                 UPDATE jobs SET record=json_set(record,'$.created_ms',0) WHERE id='crew-1';",
            ).unwrap();
            crate::jobs::end_spent_park(&store, "crew-alpha", "elapsed cap spent").unwrap();
            resume_pending(&mut store).unwrap();
            let job = crate::jobs::record(&store, id).unwrap();
            assert_eq!(job["status"], "running", "{state}: {job}");
            assert_eq!(job["cancel_requested"], false);
            assert_eq!(job["timeout_secs"], 60);
        }
    }

    #[test]
    fn a_failed_target_cancel_still_fences_the_running_crew_from_reparking() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        store.connection().execute_batch(
            "INSERT INTO charters(id,data) VALUES('alpha','{\"id\":\"alpha\",\"active\":true,\"actions\":[\"spawn\"]}');
             INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('fix','alpha','engineering','fix','open',0);",
        ).unwrap();
        insert_job(&store, "crew-alpha", "running");
        insert_job(&store, "child", "running");
        insert_park(&store, "park-alpha", "crew-alpha", "parked", None);
        store.connection().execute(
            "UPDATE jobs SET record=json_set(record,'$.park','park-alpha','$.supervisor',json(?1)) WHERE id='crew-alpha'",
            [crate::jobs::own_identity().unwrap()],
        ).unwrap();
        store
            .connection()
            .execute_batch("UPDATE crew_parks SET child_job_id='child' WHERE id='park-alpha';")
            .unwrap();
        let stale = crate::jobs::record(&store, "crew-alpha").unwrap();
        let error =
            crate::jobs::end_parked(&store, "crew-alpha", "replacement", false).unwrap_err();
        assert!(
            error.to_string().contains("identity unavailable"),
            "{error:#}"
        );
        assert_eq!(
            crate::jobs::record(&store, "crew-alpha").unwrap()["cancel_requested"],
            true
        );
        let error =
            open_park_inner(&store, &stale, "child", "try again", "parked", true).unwrap_err();
        assert!(
            error.to_string().contains("cancellation requested"),
            "{error:#}"
        );
    }

    #[test]
    fn a_parked_hosted_crew_with_a_live_payload_cannot_claim_its_outcome() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp.path()).unwrap();
        insert_job(&store, "crew-1", "parked");
        insert_job(&store, "child-1", "succeeded");
        insert_park(&store, "park-1", "crew-1", "parked", None);
        // Even if a stale supervisor wrote parked while a descendant still
        // runs outside its group, settlement must not start another session.
        let live: Value = serde_json::from_str(&crate::jobs::own_identity().unwrap()).unwrap();
        store
            .connection()
            .execute(
                "UPDATE jobs SET record=json_set(record,'$.park','park-1','$.hosted',json('true'),
               '$.payload',json(?1),'$.escaped',json(?2)) WHERE id='crew-1'",
                params![live.to_string(), json!([live]).to_string()],
            )
            .unwrap();
        store
            .connection()
            .execute(
                "UPDATE crew_parks SET child_job_id='child-1' WHERE id='park-1'",
                [],
            )
            .unwrap();
        for _ in 0..3 {
            settled(&mut store, "child-1").unwrap();
        }
        let state: String = store
            .connection()
            .query_row("SELECT state FROM crew_parks WHERE id='park-1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(state, "parked");
        assert_eq!(
            crate::jobs::record(&store, "crew-1").unwrap()["status"],
            "parked"
        );
    }

    /// agent.start can return a starting/queued snapshot after recovery has
    /// already seen the resume running. Replaying that late launch response
    /// must not move the park backwards, even if the resume has parked again.
    #[test]
    fn a_late_launch_response_does_not_regress_a_recovered_resume() {
        for status in ["starting", "queued", "running"] {
            let temp = tempfile::tempdir().unwrap();
            let mut store = Store::open(temp.path()).unwrap();
            insert_job(&store, "crew-alpha", "parked");
            insert_park(&store, "park-alpha", "crew-alpha", "parked", None);
            assert!(claim(&store, "park-alpha", "parked").unwrap());
            let resume = "crew-resume-park-alpha-2";
            launch_resume_with(
                &mut store,
                "park-alpha",
                "crew-alpha",
                "settled",
                "continue",
                false,
                |store, args| {
                    assert_eq!(args["id"], resume);
                    insert_job(store, resume, "running");
                    recover_resuming(store)?;
                    // The new crew parks again before agent.start returns its
                    // older snapshot. No later tick should be needed to repair
                    // the first park, and this continuation must not be cancelled.
                    insert_park(store, "park-beta", resume, "parked", None);
                    store.connection().execute(
                        "UPDATE jobs SET record=json_set(record,'$.status','parked') WHERE id=?1",
                        [resume],
                    )?;
                    Ok(json!({"id": resume, "status": status}))
                },
            )
            .unwrap();
            let linked: (String, String) = store
                .connection()
                .query_row(
                    "SELECT state, resumed_job_id FROM crew_parks WHERE id='park-alpha'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(linked, ("resumed".into(), resume.into()));
            assert_eq!(lineage(&store, resume).unwrap(), [resume, "crew-alpha"]);
            assert_eq!(
                crate::jobs::record(&store, "crew-alpha").unwrap()["status"],
                "resumed"
            );
            let job = crate::jobs::record(&store, resume).unwrap();
            assert_eq!(job["status"], "parked");
            assert_eq!(job["cancel_requested"], false);
        }
    }

    /// A close during agent.start must retain custody of its late job without
    /// reopening the park or marking the old crew resumed.
    #[test]
    fn a_launch_that_returns_after_its_park_closed_is_cancelled() {
        for closed in ["cancelled", "failed", "resumed"] {
            for status in ["starting", "queued"] {
                let temp = tempfile::tempdir().unwrap();
                let mut store = Store::open(temp.path()).unwrap();
                insert_job(&store, "crew-alpha", "parked");
                insert_park(&store, "park-alpha", "crew-alpha", "parked", None);
                assert!(claim(&store, "park-alpha", "parked").unwrap());
                let resume = "crew-resume-park-alpha-2";
                launch_resume_with(
                    &mut store,
                    "park-alpha",
                    "crew-alpha",
                    "settled",
                    "continue",
                    false,
                    |store, args| {
                        assert_eq!(args["id"], resume);
                        set_state(store, "park-alpha", closed)?;
                        if closed == "resumed" {
                            store.connection().execute(
                                "UPDATE crew_parks SET resumed_job_id='crew-beta' WHERE id='park-alpha'",
                                [],
                            )?;
                        }
                        insert_job(store, resume, status);
                        Ok(json!({"id": resume, "status": status}))
                    },
                )
                .unwrap();
                let park: (String, Option<String>) = store
                    .connection()
                    .query_row(
                        "SELECT state, resumed_job_id FROM crew_parks WHERE id='park-alpha'",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .unwrap();
                let prior_link = (closed == "resumed").then(|| "crew-beta".to_owned());
                assert_eq!(park, (closed.into(), prior_link));
                let job = crate::jobs::record(&store, resume).unwrap();
                assert_eq!(job["status"], "cancelled");
                assert_eq!(job["cancel_requested"], true);
                assert_eq!(
                    crate::jobs::record(&store, "crew-alpha").unwrap()["status"],
                    "parked"
                );
                let audit: String = store
                    .connection()
                    .query_row(
                        "SELECT payload FROM audit WHERE kind='crew.resume_cancel_requested'
                           AND json_extract(payload,'$.resumed_job_id')=?1",
                        [resume],
                        |r| r.get(0),
                    )
                    .unwrap();
                let audit: Value = serde_json::from_str(&audit).unwrap();
                assert_eq!(audit["park"], "park-alpha");
                assert_eq!(audit["crew_job_id"], "crew-alpha");
                assert_eq!(audit["park_state"], closed);
            }
        }
    }

    #[test]
    fn a_failed_late_resume_cancel_keeps_custody_across_restart() {
        for closed in ["cancelled", "failed", "resumed"] {
            let temp = tempfile::tempdir().unwrap();
            let store = Store::open(temp.path()).unwrap();
            insert_job(&store, "crew-alpha", "parked");
            insert_job(&store, "crew-beta", "starting");
            insert_park(
                &store,
                "park-alpha",
                "crew-alpha",
                closed,
                Some("crew-gamma"),
            );
            store
                .connection()
                .execute_batch(
                    "CREATE TRIGGER fail_cancel BEFORE UPDATE ON jobs
                 WHEN OLD.id='crew-beta' BEGIN SELECT RAISE(FAIL, 'cancel unavailable'); END;",
                )
                .unwrap();
            let error =
                settle_resume_link(&store, "park-alpha", "crew-alpha", "resumed", "crew-beta")
                    .unwrap_err();
            assert!(
                error.to_string().contains("cancel unavailable"),
                "{error:#}"
            );
            let audited: bool = store
                .connection()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM audit WHERE kind='crew.resume_cancel_requested'
                 AND json_extract(payload,'$.resumed_job_id')='crew-beta')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(audited);
            // Intent is independent of audit retention and outlives the
            // launcher. A failed retry must preserve it too.
            store.connection().execute("DELETE FROM audit", []).unwrap();
            drop(store);
            let store = Store::open(temp.path()).unwrap();
            retry_resume_cancels(&store).unwrap();
            let pending = || -> bool {
                store
                    .connection()
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM crew_resume_cancels
                     WHERE job_id='crew-beta' AND incarnation='one')",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap()
            };
            assert!(pending());
            assert_eq!(
                crate::jobs::record(&store, "crew-beta").unwrap()["cancel_requested"],
                false
            );
            // The failed retry scheduled the next attempt instead of running
            // it on every pass, and wrote its one failure row for that step.
            let (attempts, next_attempt_ms): (i64, i64) = store
                .connection()
                .query_row(
                    "SELECT attempts, next_attempt_ms FROM crew_resume_cancels
                 WHERE job_id='crew-beta' AND incarnation='one'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(attempts, 1);
            let failed: i64 = store
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM audit WHERE kind='crew.resume_cancel_failed'
                 AND json_extract(payload,'$.job_id')='crew-beta'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(failed, 1);
            store
                .connection()
                .execute_batch("DROP TRIGGER fail_cancel;")
                .unwrap();
            // The cancel is retried when its backoff elapses, not before.
            let _clock = crate::store::FrozenWallClock::at(next_attempt_ms - 1);
            retry_resume_cancels(&store).unwrap();
            assert!(pending());
            drop(_clock);
            let _clock = crate::store::FrozenWallClock::at(next_attempt_ms);
            retry_resume_cancels(&store).unwrap();
            assert!(!pending());
            let job = crate::jobs::record(&store, "crew-beta").unwrap();
            assert_eq!(job["status"], "cancelled");
            assert_eq!(job["cancel_requested"], true);
            retry_resume_cancels(&store).unwrap();
            let park: (String, String) = store
                .connection()
                .query_row(
                    "SELECT state, resumed_job_id FROM crew_parks WHERE id='park-alpha'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(park, (closed.into(), "crew-gamma".into()));
            assert_eq!(
                crate::jobs::record(&store, "crew-alpha").unwrap()["status"],
                "parked"
            );
        }
    }

    #[test]
    fn a_late_resume_cancel_does_not_follow_a_reused_job_id() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        insert_job(&store, "crew-alpha", "starting");
        store
            .connection()
            .execute_batch(
                "INSERT INTO crew_resume_cancels(job_id, incarnation) VALUES('crew-alpha','previous');
             INSERT INTO crew_resume_cancels(job_id, incarnation) VALUES('crew-beta','missing');",
            )
            .unwrap();
        let before = crate::jobs::record(&store, "crew-alpha").unwrap();
        retry_resume_cancels(&store).unwrap();
        assert_eq!(crate::jobs::record(&store, "crew-alpha").unwrap(), before);
        let pending: bool = store
            .connection()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM crew_resume_cancels)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!pending);
    }

    /// A cancel that keeps failing runs its kill waits only at backoff
    /// boundaries, writes one failure row per step, doubles to its cap, and
    /// retires the intent when it finally succeeds. The clock is injected, so
    /// no pass depends on wall time.
    #[test]
    fn a_failing_late_resume_cancel_retries_only_at_backoff_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        insert_job(&store, "crew-beta", "starting");
        store
            .connection()
            .execute_batch(
                "CREATE TRIGGER fail_cancel BEFORE UPDATE ON jobs
             WHEN OLD.id='crew-beta' BEGIN SELECT RAISE(FAIL, 'cancel unavailable'); END;
             INSERT INTO crew_resume_cancels(job_id, incarnation) VALUES('crew-beta','one');",
            )
            .unwrap();
        let intent = || -> Option<(i64, i64)> {
            store
                .connection()
                .query_row(
                    "SELECT attempts, next_attempt_ms FROM crew_resume_cancels
                 WHERE job_id='crew-beta' AND incarnation='one'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .unwrap()
        };
        let failures = |after: i64| -> i64 {
            store
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM audit WHERE kind='crew.resume_cancel_failed'
                 AND json_extract(payload,'$.job_id')='crew-beta'
                 AND json_extract(payload,'$.attempts')>?1",
                    [after],
                    |r| r.get(0),
                )
                .unwrap()
        };
        let base = 1_700_000_000_000;
        {
            let _clock = crate::store::FrozenWallClock::at(base);
            retry_resume_cancels(&store).unwrap();
            // A pass before the boundary does not re-run the kill waits.
            retry_resume_cancels(&store).unwrap();
        }
        assert_eq!(intent(), Some((1, base + RESUME_CANCEL_BACKOFF_BASE_MS)));
        assert_eq!(failures(0), 1);
        {
            // Just before the boundary the intent is still held.
            let _clock =
                crate::store::FrozenWallClock::at(base + RESUME_CANCEL_BACKOFF_BASE_MS - 1);
            retry_resume_cancels(&store).unwrap();
        }
        assert_eq!(intent(), Some((1, base + RESUME_CANCEL_BACKOFF_BASE_MS)));
        assert_eq!(failures(0), 1);
        {
            // At the boundary the second attempt fails and doubles the wait.
            let _clock = crate::store::FrozenWallClock::at(base + RESUME_CANCEL_BACKOFF_BASE_MS);
            retry_resume_cancels(&store).unwrap();
        }
        assert_eq!(
            intent(),
            Some((2, base + 3 * RESUME_CANCEL_BACKOFF_BASE_MS))
        );
        assert_eq!(failures(0), 2);
        assert_eq!(failures(2), 0);
        // A long-stuck intent (many attempts) waits the cap, never longer.
        store
            .connection()
            .execute_batch("UPDATE crew_resume_cancels SET attempts=6, next_attempt_ms=0;")
            .unwrap();
        let capped = base + 20 * RESUME_CANCEL_BACKOFF_BASE_MS;
        {
            let _clock = crate::store::FrozenWallClock::at(capped);
            retry_resume_cancels(&store).unwrap();
        }
        assert_eq!(intent(), Some((7, capped + RESUME_CANCEL_BACKOFF_MAX_MS)));
        assert_eq!(failures(2), 1);
        store
            .connection()
            .execute_batch("DROP TRIGGER fail_cancel;")
            .unwrap();
        {
            // Success lands exactly at the boundary and retires the intent.
            let _clock =
                crate::store::FrozenWallClock::at(capped + RESUME_CANCEL_BACKOFF_MAX_MS - 1);
            retry_resume_cancels(&store).unwrap();
        }
        assert!(intent().is_some());
        {
            let _clock = crate::store::FrozenWallClock::at(capped + RESUME_CANCEL_BACKOFF_MAX_MS);
            retry_resume_cancels(&store).unwrap();
        }
        assert_eq!(intent(), None);
        assert_eq!(failures(2), 1);
        let job = crate::jobs::record(&store, "crew-beta").unwrap();
        assert_eq!(job["status"], "cancelled");
        assert_eq!(job["cancel_requested"], true);
    }

    /// A queued launch is linked while unresolved, then advances once it is
    /// admitted. A delayed launch/recovery write cannot reopen a closed park.
    #[test]
    fn resume_linking_advances_only_unresolved_parks() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        insert_park(&store, "park-alpha", "crew-alpha", "resuming", None);
        let resume = "crew-resume-park-alpha-1";
        for state in ["resuming", "resumed"] {
            assert_eq!(link_resume(&store, "park-alpha", state, resume).unwrap(), 1);
            let linked: (String, String) = store
                .connection()
                .query_row(
                    "SELECT state, resumed_job_id FROM crew_parks WHERE id='park-alpha'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(linked, (state.into(), resume.into()));
        }
        for closed in ["resumed", "cancelled", "failed"] {
            set_state(&store, "park-alpha", closed).unwrap();
            for stale in ["resuming", "resumed"] {
                assert_eq!(
                    link_resume(&store, "park-alpha", stale, "other-resume").unwrap(),
                    0
                );
                let linked: (String, String) = store
                    .connection()
                    .query_row(
                        "SELECT state, resumed_job_id FROM crew_parks WHERE id='park-alpha'",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .unwrap();
                assert_eq!(linked, (closed.into(), resume.into()));
            }
        }
    }

    /// The claimer died after agent.start recorded the resume job but before
    /// it linked the park to it. Recovery settles the park as resumed, and
    /// must link it too, or the resumed crew starts a lineage of its own and
    /// its parks no longer count against the caps.
    #[test]
    fn a_recovered_resume_keeps_the_crews_lineage_and_caps() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp.path()).unwrap();
        // The crew has already parked to its cap: five resumed parks, then
        // the one whose resume was in flight when its claimer died.
        insert_job(&store, "crew-1", "resumed");
        let mut crew = "crew-1".to_owned();
        for n in 1..MAX_PARKS {
            let resumed = format!("crew-resume-park-crew-1-{n}-1");
            insert_park(
                &store,
                &format!("park-crew-1-{n}"),
                &crew,
                "resumed",
                Some(&resumed),
            );
            insert_job(&store, &resumed, "resumed");
            crew = resumed;
        }
        let last = format!("park-crew-1-{MAX_PARKS}");
        insert_park(&store, &last, &crew, "resuming", None);
        store
            .connection()
            .execute(
                "UPDATE jobs SET record=json_set(record,'$.status','parked') WHERE id=?1",
                [&crew],
            )
            .unwrap();
        let resume = format!("crew-resume-{last}-1");
        insert_job(&store, &resume, "running");

        recover_resuming(&mut store).unwrap();

        let (state, linked): (String, Option<String>) = store
            .connection()
            .query_row(
                "SELECT state, resumed_job_id FROM crew_parks WHERE id=?1",
                [&last],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "resumed");
        assert_eq!(linked.as_deref(), Some(resume.as_str()));
        assert_eq!(
            crate::jobs::record(&store, &crew).unwrap()["status"],
            "resumed"
        );
        let chain = lineage(&store, &resume).unwrap();
        assert_eq!(chain.len() as u64, MAX_PARKS + 1, "{chain:?}");
        let root = chain.last().unwrap();
        assert_eq!(root, "crew-1");
        // The resumed crew is at its park cap: it may not park again.
        let spent = spent_cap(&store, root, "fix", true).unwrap();
        assert!(
            spent
                .as_deref()
                .is_some_and(|why| why.contains("the cap is 6")),
            "{spent:?}"
        );
    }
}
