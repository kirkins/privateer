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
use serde_json::{Value, json};
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
fn owning_ship(store: &Store, task_id: &str) -> Result<Option<String>> {
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
fn wake_job(store: &Store, job_id: &str) -> Result<bool> {
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
pub(crate) fn outcome(store: &Store, job: &Value) -> Result<()> {
    let job_id = job["id"].as_str().context("job id")?;
    let incarnation = job["incarnation"].as_str().context("job incarnation")?;
    let task_id = job["task_id"].as_str().context("job task")?;
    let status = job["status"].as_str().context("job status")?;
    let Some(ship) = owning_ship(store, task_id)? else {
        return Ok(());
    };
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
    let urgency = match status {
        _ if crew_watching => "low",
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
    if let Some(reason) = &reason {
        body.push_str(&format!("Reason: {reason}\n"));
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
    let job_id = std::env::var("PRIVATEER_JOB")
        .ok()
        .filter(|j| !j.is_empty())
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

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "crew.report" => report(store, &args),
        "crew.answer" => answer(store, &args),
        _ => bail!("unknown crew command {command}"),
    }
}
