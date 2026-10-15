//! Crew outcomes: how a first mate hears what its crew did.
//!
//! Crew are supervised jobs working on a ship's tasks. When one reaches a
//! reported status, the runtime writes a notice from the reserved sender
//! `crew` to the ship that owns the job's task, in the same transaction as
//! the status, from the job's record and logs, never from model output. The
//! id `crew-<job>-<incarnation>` makes it exactly one per incarnation. From
//! there activation, focus, hooks, acknowledgement and retention treat it
//! like any mail.
use crate::mail::record_crew;
use crate::store::Store;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Outcome bodies stay within what a wake or a hook shows of one message.
const BODY_CHARS: usize = 2_000;
const TAIL_BYTES: u64 = 1_400;
const COMMAND_CHARS: usize = 160;
const REASON_CHARS: usize = 300;

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

/// A first mate's own wake job. Serve records the wake before starting it.
fn wake_job(store: &Store, job_id: &str) -> Result<bool> {
    Ok(store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM wakes WHERE job_id=?1)",
        [job_id],
        |r| r.get(0),
    )?)
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
    let urgency = match status {
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
