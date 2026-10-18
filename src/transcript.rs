//! `wake.transcript {ship, wake_id}`: what a headless first-mate wake or an
//! errand did, read from its job log and normalized, so the captain's portal
//! never reads harness files or job directories.
//!
//! Only the operator reads it. Every text it returns is masked: values under
//! secret-looking keys, secret-shaped words, environment-looking assignments
//! and the word after a credential flag or label. The job's argv and
//! environment are never returned. A hosted first mate writes to its pane,
//! not a log, so it has no transcript here yet.
use crate::store::{Caller, Store, identifier, required};
use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

const MASK: &str = "•••";
/// Bounds on one event's text and previews, and on the events returned.
const TEXT_CHARS: usize = 4_000;
const PREVIEW_CHARS: usize = 500;
const MAX_EVENTS: usize = 500;
/// A job log is bounded near 1 MiB; never read much past that.
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

/// One wake or errand record, whichever the id names.
struct Run {
    ship: String,
    job_id: Option<String>,
    harness: Option<String>,
    status: String,
    started_ms: i64,
    finished_ms: Option<i64>,
    error: Option<String>,
    usage: Option<Value>,
}

pub fn transcript(args: &Value) -> Result<Value> {
    let ship = required(args, "ship")?;
    identifier(ship)?;
    let id = required(args, "wake_id")?;
    identifier(id)?;
    let Some((_, home)) = crate::captain::fleet()?
        .into_iter()
        .find(|(name, _)| name == ship)
    else {
        if let Some((_, machine, _)) = crate::machines::remote_ships()?
            .into_iter()
            .find(|(name, _, _)| name == ship)
        {
            bail!("wake {id} is on machine {machine}; ask there");
        }
        bail!("unknown ship {ship}");
    };
    let store = Store::open_readonly(&home, Caller::Operator)?;
    let run = find(&store, id)?.with_context(|| format!("unknown wake {id} for ship {ship}"))?;
    if run.ship != ship {
        bail!("wake {id} is not ship {ship}'s");
    }
    let job = match &run.job_id {
        Some(job) => store
            .connection()
            .query_row("SELECT record FROM jobs WHERE id=?1", [job], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok()),
        None => None,
    };
    let job = job.unwrap_or(Value::Null);
    let model_route = job["model_route"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| crate::routes::route_of_argv(&job["command"]));
    let harness = run.harness.clone().filter(|h| !h.is_empty()).or_else(|| {
        model_route
            .as_deref()
            .and_then(|r| r.split('/').next())
            .map(str::to_owned)
    });
    let hosted = job["hosted"] == true;
    let log = match (&run.job_id, job["incarnation"].as_str()) {
        (Some(job_id), Some(incarnation)) if !hosted => read_log(
            &store
                .home()
                .join("jobs")
                .join(job_id)
                .join(incarnation)
                .join("stdout.log"),
            job_id,
            incarnation,
        ),
        _ => String::new(),
    };
    let mut events = normalize(&log);
    if let Some(error) = &run.error {
        events.push(json!({"kind": "error", "text": bounded(&mask_text(error), TEXT_CHARS)}));
    }
    let more = events.len() > MAX_EVENTS;
    if more {
        // The end of a run says how it went; keep it and drop the middle.
        let head = MAX_EVENTS / 5;
        events.drain(head..events.len() - (MAX_EVENTS - head));
    }
    let truncated = crate::jobs::log_cut(&log) || job["stdout_dropped_bytes"].as_u64() > Some(0);
    let mut out = json!({
        "ship": ship, "wake_id": id, "job_id": run.job_id, "harness": harness,
        "model_route": model_route, "started_ms": run.started_ms,
        "finished_ms": run.finished_ms, "status": run.status,
        "usage": run.usage.or_else(|| job.get("usage").cloned().filter(|u| !u.is_null())),
        "truncated": truncated, "events": events, "more": more,
    });
    if hosted {
        out["hosted"] = json!(true);
        out["note"] = json!("this ran in a hosted pane, which keeps no transcript yet");
    }
    Ok(out)
}

fn find(store: &Store, id: &str) -> Result<Option<Run>> {
    let row = |r: &rusqlite::Row| -> rusqlite::Result<Run> {
        let usage: Option<String> = r.get(7)?;
        Ok(Run {
            ship: r.get(0)?,
            job_id: r.get(1)?,
            harness: r.get(2)?,
            status: r.get(3)?,
            started_ms: r.get(4)?,
            finished_ms: r.get(5)?,
            error: r.get(6)?,
            usage: usage.and_then(|u| serde_json::from_str(&u).ok()),
        })
    };
    let wake = store
        .connection()
        .query_row(
            "SELECT ship, job_id, harness, status, created_ms, finished_ms, error, usage
             FROM wakes WHERE id=?1",
            params![id],
            row,
        )
        .optional()?;
    if wake.is_some() {
        return Ok(wake);
    }
    // An errand's job has the errand's id.
    Ok(store
        .connection()
        .query_row(
            "SELECT ship, id, harness, status, created_ms, finished_ms, error, usage
             FROM errands WHERE id=?1",
            params![id],
            row,
        )
        .optional()?)
}

/// The job's log, bounded; empty when it is missing or not a plain file.
fn read_log(path: &Path, job_id: &str, incarnation: &str) -> String {
    let plain = |part: &str| !part.is_empty() && !part.contains(['/', '\0']) && part != "..";
    if !plain(job_id) || !plain(incarnation) {
        return String::new();
    }
    if !std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file()) {
        return String::new();
    }
    let mut bytes = vec![];
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file.take(MAX_LOG_BYTES).read_to_end(&mut bytes);
    }
    // A bounded log can be cut inside a UTF-8 character; keep the rest.
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The events of a Claude stream-json, Codex `exec --json` or Pi JSON log,
/// with the runtime's own turn lines. Streaming deltas, reasoning and
/// bookkeeping are left out; lines cut at the log's edges are skipped.
pub(crate) fn normalize(log: &str) -> Vec<Value> {
    let mut events = vec![];
    // Tool names by call id, for results that name only the call.
    let mut tools: HashMap<String, String> = HashMap::new();
    for line in log.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let at = event["timestamp"]
            .as_i64()
            .or_else(|| event["message"]["timestamp"].as_i64());
        let before = events.len();
        read(&event, &mut tools, &mut events);
        if let Some(at) = at {
            for added in &mut events[before..] {
                added["at_ms"] = json!(at);
            }
        }
    }
    events
}

fn read(event: &Value, tools: &mut HashMap<String, String>, out: &mut Vec<Value>) {
    let Some(kind) = event["type"].as_str() else {
        return;
    };
    match kind {
        // Claude
        "assistant" => {
            for item in event["message"]["content"].as_array().into_iter().flatten() {
                match item["type"].as_str() {
                    Some("text") => assistant(out, item["text"].as_str()),
                    Some("tool_use") => {
                        let name = item["name"].as_str().unwrap_or("tool");
                        if let Some(id) = item["id"].as_str() {
                            tools.insert(id.to_owned(), name.to_owned());
                        }
                        out.push(tool_call(name, &item["input"]));
                    }
                    _ => {}
                }
            }
        }
        "user" => {
            for item in event["message"]["content"].as_array().into_iter().flatten() {
                if item["type"] == "tool_result" {
                    let name = item["tool_use_id"].as_str().and_then(|id| tools.get(id));
                    out.push(tool_result(
                        name.map(String::as_str),
                        &content_text(&item["content"]),
                        item["is_error"] == true,
                    ));
                }
            }
        }
        "result" => {
            let mut result = json!({"kind": "result"});
            if let Some(text) = event["result"].as_str().filter(|t| !t.trim().is_empty()) {
                result["text"] = json!(bounded(&mask_text(text), TEXT_CHARS));
            }
            if event["is_error"] == true {
                result["is_error"] = json!(true);
            }
            out.push(result);
        }
        // Pi
        "message_end" => {
            let message = &event["message"];
            match message["role"].as_str() {
                Some("assistant") => {
                    for item in message["content"].as_array().into_iter().flatten() {
                        match item["type"].as_str() {
                            Some("text") => assistant(out, item["text"].as_str()),
                            Some("toolCall") => {
                                let name = item["name"].as_str().unwrap_or("tool");
                                if let Some(id) = item["id"].as_str() {
                                    tools.insert(id.to_owned(), name.to_owned());
                                }
                                out.push(tool_call(name, &item["arguments"]));
                            }
                            _ => {}
                        }
                    }
                    if message["stopReason"] == "error" {
                        error(
                            out,
                            message["errorMessage"]
                                .as_str()
                                .unwrap_or("the model call failed"),
                        );
                    }
                }
                Some("toolResult") => {
                    let name = message["toolName"].as_str().or_else(|| {
                        message["toolCallId"]
                            .as_str()
                            .and_then(|id| tools.get(id))
                            .map(String::as_str)
                    });
                    out.push(tool_result(
                        name,
                        &content_text(&message["content"]),
                        message["isError"] == true,
                    ));
                }
                _ => {}
            }
        }
        // Codex
        "item.completed" => {
            let item = &event["item"];
            match item["type"].as_str() {
                Some("agent_message") => assistant(out, item["text"].as_str()),
                Some("command_execution") => {
                    let command = item["command"].as_str().unwrap_or("");
                    out.push(tool_call("command", &json!(command)));
                    let failed = item["exit_code"].as_i64().is_some_and(|code| code != 0)
                        || item["status"] == "failed";
                    out.push(tool_result(
                        Some("command"),
                        item["aggregated_output"].as_str().unwrap_or(""),
                        failed,
                    ));
                }
                Some("mcp_tool_call") => {
                    let name = format!(
                        "{}/{}",
                        item["server"].as_str().unwrap_or("mcp"),
                        item["tool"].as_str().unwrap_or("tool")
                    );
                    out.push(tool_call(&name, &item["arguments"]));
                    let failed = item["status"] == "failed" || !item["error"].is_null();
                    let output = if failed {
                        item["error"]["message"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| item["error"].to_string())
                    } else {
                        content_text(&item["result"]["content"])
                    };
                    out.push(tool_result(Some(&name), &output, failed));
                }
                Some("file_change") => {
                    let paths: Vec<Value> = item["changes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|c| json!({"path": c["path"], "kind": c["kind"]}))
                        .collect();
                    out.push(tool_call("file_change", &json!({"changes": paths})));
                }
                Some("web_search") => {
                    out.push(tool_call("web_search", &json!({"query": item["query"]})));
                }
                Some("error") => error(out, item["message"].as_str().unwrap_or("error")),
                _ => {}
            }
        }
        "turn.failed" => error(
            out,
            event["error"]["message"]
                .as_str()
                .unwrap_or("the turn failed"),
        ),
        "error" => error(out, event["message"].as_str().unwrap_or("error")),
        // The runtime's own lines, after every harness's turn.
        "turn.completed" => {
            if out.last().is_none_or(|last| last["kind"] != "result") {
                out.push(json!({"kind": "result"}));
            }
        }
        "native.turn.failed" => error(
            out,
            &format!(
                "the turn failed{}",
                event["reason"]
                    .as_str()
                    .map(|r| format!(": {r}"))
                    .unwrap_or_default()
            ),
        ),
        "native.permission_blocked" => {
            let denied: Vec<&str> = event["denials"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|d| d["tool_name"].as_str())
                .collect();
            error(out, &format!("tools were denied: {}", denied.join(", ")));
        }
        _ => {}
    }
}

fn assistant(out: &mut Vec<Value>, text: Option<&str>) {
    if let Some(text) = text.filter(|t| !t.trim().is_empty()) {
        out.push(json!({"kind": "assistant", "text": bounded(&mask_text(text), TEXT_CHARS)}));
    }
}

fn error(out: &mut Vec<Value>, text: &str) {
    out.push(json!({"kind": "error", "text": bounded(&mask_text(text), TEXT_CHARS)}));
}

fn tool_call(name: &str, input: &Value) -> Value {
    let mut call = json!({"kind": "tool_call", "tool": bounded(name, 200)});
    if !input.is_null() {
        call["input_preview"] = json!(preview(input));
    }
    call
}

fn tool_result(name: Option<&str>, output: &str, failed: bool) -> Value {
    let mut result = json!({"kind": "tool_result",
        "output_preview": bounded(&mask_output(output), PREVIEW_CHARS)});
    if let Some(name) = name {
        result["tool"] = json!(bounded(name, 200));
    }
    if failed {
        result["is_error"] = json!(true);
    }
    result
}

/// A tool input, masked, then bounded: a string as itself, anything else as
/// compact JSON.
fn preview(input: &Value) -> String {
    let shown = match mask_value(input) {
        Value::String(text) => text,
        other => other.to_string(),
    };
    bounded(&shown, PREVIEW_CHARS)
}

/// A tool's output masked: as JSON when it is a JSON document, else as text.
fn mask_output(text: &str) -> String {
    match serde_json::from_str::<Value>(text.trim()) {
        Ok(value @ (Value::Object(_) | Value::Array(_))) => mask_value(&value).to_string(),
        _ => mask_text(text),
    }
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Whether a key's value is hidden whole: a secret-looking name, or an
/// environment.
fn hidden_key(key: &str) -> bool {
    crate::block_ask::sensitive_name(key)
        || matches!(key.to_ascii_lowercase().as_str(), "env" | "environment")
}

/// Every value under a hidden key replaced, maps included; other strings
/// masked as text.
pub(crate) fn mask_value(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| {
                    let shown = if hidden_key(key) {
                        json!(MASK)
                    } else {
                        mask_value(value)
                    };
                    (key.clone(), shown)
                })
                .collect::<Map<_, _>>(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(mask_value).collect()),
        Value::String(text) => json!(mask_text(text)),
        other => other.clone(),
    }
}

/// Whether a name reads as an environment variable: `API_KEY`, `HOME`.
fn env_name(name: &str) -> bool {
    name.len() >= 2
        && name.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Text with what may be secret hidden: every `NAME=value` whose name reads
/// as an environment variable, the word after a credential flag or label
/// (`--password x`, `token: x`), then what `block_ask::mask` hides.
pub(crate) fn mask_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut hide_next = false;
    for word in text.split_inclusive(char::is_whitespace) {
        let trimmed = word.trim_end();
        let tail = &word[trimmed.len()..];
        if trimmed.is_empty() {
            out.push_str(word);
            continue;
        }
        if hide_next {
            hide_next = false;
            out.push_str(MASK);
            out.push_str(tail);
            continue;
        }
        let bare = trimmed.trim_start_matches(['"', '\'', '`', '(', '{', '[', ',', ';']);
        if let Some((name, value)) = bare.split_once('=')
            && env_name(name)
            && !value.is_empty()
        {
            let cut = trimmed.find('=').map_or(trimmed.len(), |at| at + 1);
            out.push_str(&trimmed[..cut]);
            out.push_str(MASK);
            out.push_str(tail);
            continue;
        }
        let label = bare.trim_end_matches(['"', '\'', '`']);
        if !label.contains('=')
            && (label.ends_with(':') || label.starts_with('-'))
            && crate::block_ask::sensitive_name(label.trim_matches(|c: char| !c.is_alphanumeric()))
        {
            hide_next = true;
        }
        out.push_str(word);
    }
    crate::block_ask::mask(&out)
}

/// At most `max` characters, with control characters other than newlines
/// and tabs removed, and `…` where it was cut.
fn bounded(text: &str, max: usize) -> String {
    let clean: String = text
        .chars()
        .filter(|c| matches!(c, '\n' | '\t') || !c.is_control())
        .collect();
    if clean.chars().count() <= max {
        clean
    } else {
        format!("{}…", clean.chars().take(max - 1).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "ghp_9aZ3kQ7rT1mN5vB8xC2dF6hJ0lP4sW";

    #[test]
    fn masking_hides_assignments_flags_labels_and_secret_keys() {
        let shown = mask_text(&format!(
            "export GITHUB_TOKEN={TOKEN} && HOME=/root tool --password hunter2 -v\npassword: swordfish\nok"
        ));
        assert!(!shown.contains(TOKEN), "{shown}");
        assert!(!shown.contains("/root"), "{shown}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(!shown.contains("swordfish"), "{shown}");
        assert!(shown.contains("GITHUB_TOKEN=•••"), "{shown}");
        assert!(
            shown.contains(" -v\n") && shown.ends_with("\nok"),
            "{shown}"
        );
        let value = mask_value(
            &json!({"command": format!("curl -H 'Authorization: Bearer {TOKEN}'"),
            "env": {"PLAIN": "x"}, "headers": {"cookie": "c=1"}, "path": "/w/a.rs"}),
        );
        let text = value.to_string();
        assert!(!text.contains(TOKEN) && !text.contains("c=1") && !text.contains("PLAIN"));
        assert_eq!(value["path"], "/w/a.rs");
        assert_eq!(value["env"], MASK);
        let output = mask_output(r#"{"access_token":"short","ok":true}"#);
        assert!(
            !output.contains("short") && output.contains("\"ok\":true"),
            "{output}"
        );
    }

    #[test]
    fn bounds_count_characters_and_mark_the_cut() {
        assert_eq!(bounded("abc", 3), "abc");
        let long = bounded(&"é".repeat(10), 5);
        assert_eq!(long.chars().count(), 5);
        assert!(long.ends_with('…'));
        assert_eq!(bounded("a\u{1b}[31mb", 10), "a[31mb");
    }

    #[test]
    fn runtime_lines_end_a_turn_once_and_name_denials() {
        let log = [
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#,
            r#"{"type":"turn.completed","harness":"claude","usage":{"input":1}}"#,
            r#"{"type":"native.permission_blocked","denials":[{"tool_name":"Bash","tool_input":{"command":"x"}}]}"#,
            r#"{"type":"native.turn.failed","reason":"truncated-stream"}"#,
        ]
        .join("\n");
        let events = normalize(&log);
        assert_eq!(
            events,
            vec![
                json!({"kind":"result","text":"done"}),
                json!({"kind":"error","text":"tools were denied: Bash"}),
                json!({"kind":"error","text":"the turn failed: truncated-stream"}),
            ]
        );
    }

    #[test]
    fn a_codex_log_reads_as_commands_their_output_and_the_reply() {
        let events = normalize(include_str!(
            "../tests/fixtures/transcript/codex-wake.jsonl"
        ));
        let text = serde_json::to_string(&events).unwrap();
        assert!(!text.contains("PLANTED"), "{text}");
        assert_eq!(
            events,
            vec![
                json!({"kind":"tool_call","tool":"command","input_preview":"bash -lc ls"}),
                json!({"kind":"tool_result","tool":"command","output_preview":"Cargo.toml\nsrc\n"}),
                json!({"kind":"tool_call","tool":"command",
                    "input_preview":"bash -lc 'cargo test --token=•••"}),
                json!({"kind":"tool_result","tool":"command","is_error":true,
                    "output_preview":"error: unexpected argument\n"}),
                json!({"kind":"assistant","text":"The repository has a Cargo.toml and src."}),
                json!({"kind":"result"}),
            ]
        );
    }

    #[test]
    fn a_pi_log_reads_as_messages_with_their_tool_calls_and_times() {
        let events = normalize(include_str!("../tests/fixtures/transcript/pi-wake.jsonl"));
        let text = serde_json::to_string(&events).unwrap();
        assert!(!text.contains("PLANTED"), "{text}");
        assert_eq!(
            events,
            vec![
                json!({"kind":"assistant","text":"Reading the config.","at_ms":1792188600000_i64}),
                json!({"kind":"tool_call","tool":"read","at_ms":1792188600000_i64,
                    "input_preview":r#"{"api_key":"•••","path":"config.json"}"#}),
                json!({"kind":"tool_result","tool":"read","at_ms":1792188601000_i64,
                    "output_preview":r#"{"password":"•••","port":8080}"#}),
                json!({"kind":"assistant","text":"The config listens on 8080.",
                    "at_ms":1792188602000_i64}),
                json!({"kind":"result"}),
            ]
        );
    }
}
