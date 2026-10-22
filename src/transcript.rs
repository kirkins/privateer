//! `wake.transcript {ship, wake_id}`: what a headless first-mate wake or an
//! errand did, read from its job log and normalized, so the captain's portal
//! never reads harness files or job directories.
//!
//! Only the operator reads it. Every text it returns is masked: values under
//! secret-looking keys, secret-shaped words, environment-looking assignments
//! and the word after a credential flag or label, `"name": "value"` pairs
//! under a secret-looking name anywhere in text, and each line that is JSON
//! on its own. The job's argv and environment are never returned. Masking
//! is best effort. Still shown: a secret with no label or secret-looking name
//! beside it (a bare password on its own line; prose such as "the password is
//! X"; a .netrc `password X`; curl `-u user:pw` and mysql `-pX`); an array
//! under a sensitive key on a pretty-printed line; Python `b'...'` values;
//! JSON escaped twice inside text. A hosted first mate writes to its pane,
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
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut bytes = vec![];
    // Opened without following a link swapped in after the check above,
    // and read only when what was opened is a plain file.
    if let Ok(file) = options.open(path)
        && file.metadata().is_ok_and(|m| m.file_type().is_file())
    {
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
/// masked as text; keys that are themselves secret-shaped hidden too.
pub(crate) fn mask_value(value: &Value) -> Value {
    match value {
        Value::Object(fields) => {
            let mut shown = Map::new();
            for (key, value) in fields {
                let value = if hidden_key(key) {
                    json!(MASK)
                } else {
                    mask_value(value)
                };
                let mut key = if crate::block_ask::secret_shaped(key) {
                    MASK.to_owned()
                } else {
                    key.clone()
                };
                // Two hidden keys stay two entries.
                let mut n = 1;
                while shown.contains_key(&key) {
                    n += 1;
                    key = format!("{MASK}{n}");
                }
                shown.insert(key, value);
            }
            Value::Object(shown)
        }
        Value::Array(items) => {
            // An argv: the element after a credential flag is its value.
            let mut hide_next = false;
            let mut shown = Vec::with_capacity(items.len());
            for item in items {
                let hidden = std::mem::replace(&mut hide_next, false);
                match item {
                    _ if hidden => shown.push(json!(MASK)),
                    Value::String(flag) if credential_flag(flag) => {
                        hide_next = true;
                        shown.push(json!(mask_text(flag)));
                    }
                    _ => shown.push(mask_value(item)),
                }
            }
            Value::Array(shown)
        }
        Value::String(text) => json!(mask_text(text)),
        other => other.clone(),
    }
}

/// Whether a word is a credential flag, `--password` or `--api-key`, so the
/// word after it is its value.
fn credential_flag(word: &str) -> bool {
    word.starts_with('-') && crate::block_ask::sensitive_name(word.trim_start_matches('-'))
}

/// In text, a quoted credential flag followed by `,` and a quoted value, as
/// in `"--password","x"`: where the value's quote opens, the quote, and where
/// it closes.
fn flag_value(text: &str) -> Option<(usize, &'static str, usize)> {
    let quote = QUOTES.iter().find(|q| text.starts_with(**q))?;
    let end = quoted_end(text, quote, 128)?;
    if !credential_flag(&text[quote.len()..end - quote.len()]) {
        return None;
    }
    let after = text[end..]
        .strip_prefix(',')?
        .trim_start_matches([' ', '\t']);
    let open = text.len() - after.len();
    let value_quote = QUOTES.iter().find(|q| after.starts_with(**q))?;
    let close = quoted_end(after, value_quote, usize::MAX)?;
    Some((open, value_quote, open + close))
}

/// Whether a name reads as an environment variable: `API_KEY`, `HOME`.
fn env_name(name: &str) -> bool {
    name.len() >= 2
        && name.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Text with what may be secret hidden: each line that is a JSON object or
/// array on its own (after a line number, as a file read shows it) masked as
/// JSON; then every quoted `"name": "value"` pair under a secret-looking
/// name, every `NAME=value` whose name reads as an environment variable,
/// the word after a credential flag or label (`--password x`, `token: x`),
/// the value assigned to a secret-looking name (`password = x`,
/// `secret := x`, `token: str = x`), every secret-shaped piece between
/// quotes, brackets and separators, the values in a YAML block under a
/// secret-looking key, and what `block_ask::mask` hides. A value that opens
/// a quote or bracket is hidden through its close, within its line.
pub(crate) fn mask_text(text: &str) -> String {
    use crate::block_ask::{
        Labelled, ends_line, hide_assigned, hide_value, labelled, numbered_line,
    };
    let text =
        crate::block_ask::mask_yaml_blocks(&mask_pieces(&mask_pairs(&mask_json_lines(text))));
    let words: Vec<&str> = text.split_inclusive(char::is_whitespace).collect();
    let mut out = String::with_capacity(text.len());
    let mut hide_next = false;
    // A line ended between the label and the word it hides.
    let mut after_line = false;
    // The label's line, and this line, begin with a file read's line number.
    let mut numbered = false;
    let mut line_numbered = false;
    let mut at = 0;
    while at < words.len() {
        if at == 0 || ends_line(words[at - 1]) {
            line_numbered = numbered_line(&words[at..]);
        }
        let word = words[at];
        let trimmed = word.trim_end();
        if trimmed.is_empty() {
            after_line |= ends_line(word);
            out.push_str(word);
            at += 1;
            continue;
        }
        if std::mem::take(&mut hide_next) {
            match labelled(word, after_line, after_line && numbered) {
                Labelled::Skip => {
                    hide_next = true;
                    out.push_str(word);
                    at += 1;
                    continue;
                }
                Labelled::Value(from) => {
                    at = hide_value(&words, at, from, &mut out);
                    continue;
                }
                Labelled::Kept => {}
            }
        }
        if let Some(next) = hide_assigned(&words, at, &mut out) {
            at = next;
            continue;
        }
        let bare = trimmed.trim_start_matches(['"', '\'', '`', '(', '{', '[', ',', ';']);
        if let Some((name, value)) = bare.split_once('=')
            && (env_name(name) && !value.is_empty() || crate::block_ask::sensitive_name(name))
        {
            let cut = trimmed.find('=').map_or(trimmed.len(), |at| at + 1);
            at = hide_value(&words, at, cut, &mut out);
            continue;
        }
        let label = bare.trim_end_matches(['"', '\'', '`']);
        let name = label.trim_matches(|c: char| !c.is_alphanumeric());
        if !label.contains('=')
            && (label.ends_with(':') || label.starts_with('-'))
            && crate::block_ask::sensitive_name(name)
        {
            hide_next = true;
            after_line = ends_line(word);
            numbered = line_numbered;
        }
        out.push_str(word);
        at += 1;
    }
    crate::block_ask::mask(&out)
}

/// Each line that is a JSON object or array once a leading line number
/// (`   12\t`, `12→`) is set aside, masked by `mask_value`.
fn mask_json_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let end = &line[body.len()..];
        let start = crate::block_ask::line_number_len(body);
        match serde_json::from_str::<Value>(&body[start..]) {
            Ok(value @ (Value::Object(_) | Value::Array(_))) => {
                out.push_str(&body[..start]);
                out.push_str(&mask_value(&value).to_string());
                out.push_str(end);
            }
            _ => out.push_str(line),
        }
    }
    out
}

/// The quotes a key or value may be written in: escaped inside a JSON
/// string, double or single.
const QUOTES: [&str; 3] = ["\\\"", "\"", "'"];

/// Where the string opened by `quote` at the start of `text` ends: the byte
/// after its closing quote, within one line and `limit` bytes.
fn quoted_end(text: &str, quote: &str, limit: usize) -> Option<usize> {
    let mut at = quote.len();
    while at < text.len() && at <= limit {
        let rest = &text[at..];
        if rest.starts_with(quote) {
            return Some(at + quote.len());
        }
        let c = rest.chars().next()?;
        if c == '\n' {
            return None;
        }
        // A backslash escapes the next character, unless it begins the
        // escaped quote that closes this string.
        at += if c == '\\' && quote == "\"" {
            1 + rest[1..].chars().next().map_or(0, char::len_utf8)
        } else {
            c.len_utf8()
        };
    }
    None
}

/// The value of every quoted `"name": "value"`, `'name': 'value'` or
/// `"name"=value` pair whose name sounds secret, anywhere in text: JSON
/// fragments inside log lines, tool output and prose.
fn mask_pairs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        let Some(quote) = QUOTES.iter().find(|q| rest.starts_with(**q)) else {
            let c = rest.chars().next().map_or(1, char::len_utf8);
            out.push_str(&rest[..c]);
            at += c;
            continue;
        };
        if let Some((open, value_quote, end)) = flag_value(rest) {
            out.push_str(&rest[..open]);
            out.push_str(value_quote);
            out.push_str(MASK);
            out.push_str(value_quote);
            at += end;
            continue;
        }
        let pair = quoted_end(rest, quote, 128).and_then(|key_end| {
            let key = &rest[quote.len()..key_end - quote.len()];
            let after = rest[key_end..].trim_start_matches([' ', '\t']);
            let after = after.strip_prefix([':', '='])?;
            let value = after.trim_start_matches([' ', '\t']);
            let value_at = rest.len() - value.len();
            Some((key, value_at))
        });
        let Some((key, value_at)) = pair else {
            out.push_str(quote);
            at += quote.len();
            continue;
        };
        out.push_str(&rest[..value_at]);
        at += value_at;
        let value = &text[at..];
        if !crate::block_ask::sensitive_name(key) {
            continue;
        }
        if let Some(open) = QUOTES.iter().find(|q| value.starts_with(**q)) {
            if let Some(end) = quoted_end(value, open, usize::MAX) {
                out.push_str(open);
                out.push_str(MASK);
                out.push_str(open);
                at += end;
            }
        } else if !value.starts_with(['{', '[']) {
            let end = value
                .find(|c: char| c.is_whitespace() || ",;}])\"'\\".contains(c))
                .unwrap_or(value.len());
            if end > 0 {
                out.push_str(MASK);
                at += end;
            }
        }
    }
    out
}

/// Every secret-shaped piece between whitespace, quotes, brackets and
/// separators hidden, so a token inside `{"t":"ghp_…"}` is judged alone.
fn mask_pieces(text: &str) -> String {
    let split = |c: char| c.is_whitespace() || "\"'`:,;{}[]()=\\".contains(c);
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let cut = rest.find(split).unwrap_or(rest.len());
        let piece = &rest[..cut];
        if crate::block_ask::secret_shaped(piece) {
            out.push_str(MASK);
        } else {
            out.push_str(piece);
        }
        let c = rest[cut..].chars().next().map_or(0, char::len_utf8);
        out.push_str(&rest[cut..cut + c]);
        rest = &rest[cut + c..];
    }
    out
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
    fn compact_json_inside_text_hides_its_secrets() {
        let access = "sk-ant-oat01-PLANTEDaccess0123456789abcdefghijklmnop";
        let refresh = "sk-ant-ort01-PLANTEDrefresh0123456789abcdefghijklmno";
        let openai = "sk-proj-PLANTEDopenai0123456789abcdefghij";
        let password = "Tr0ub4dor&3";
        let credentials = format!(
            r#"{{"claudeAiOauth":{{"accessToken":"{access}","refreshToken":"{refresh}","expiresAt":1792339332990,"scopes":["user:inference"]}}}}"#
        );
        let planted = [
            access, refresh, openai, password, TOKEN, "hunter22", "hunter2", "s3cr3t", "short",
            "abc",
        ];
        let texts = [
            // A Read result with Claude's and cat -n's line numbers.
            format!("     1→{credentials}\n     2→"),
            format!("     1\t{credentials}\n"),
            // JSONL and a log tail.
            format!("{{\"event\":\"login\",\"token\":\"{TOKEN}\"}}\n{{\"event\":\"ok\"}}\n"),
            format!("12:00:01 loaded {credentials} from disk"),
            // Fragments in prose, and an assistant quoting them.
            format!(
                r#"saw {{"token":"{TOKEN}"}}, then {{"OPENAI_API_KEY":"{openai}"}} and {{"password":"{password}"}}."#
            ),
            format!("It reads 'password': '{password}' and \"secret\"=hunter22 here."),
            // JSON escaped inside a JSON string, and a secret used as a key.
            r#"payload "{\"api_key\":\"s3cr3t\",\"n\":1}" sent"#.to_owned(),
            format!(r#"keys {{"{TOKEN}":true}} end"#),
            // A secret-looking name, then `=` or `:` as a separate word.
            "password = \"Tr0ub4dor&3\"\nAPI_KEY = \"short\"\ntoken = \"abc\"".to_owned(),
            // A credential flag and its value as separate argv elements.
            r#"["mysql","--password","hunter2"]"#.to_owned(),
            r#"ran {"command":["mysql","--password","hunter2"]} ok"#.to_owned(),
            "  9\t[\"mysql\", \"--password\", \"hunter2\"]".to_owned(),
        ];
        for text in &texts {
            let shown = mask_text(text);
            for secret in planted {
                assert!(!shown.contains(secret), "{secret} in {shown}");
            }
        }
        assert_eq!(
            mask_text(&texts[0]),
            "     1→{\"claudeAiOauth\":\"•••\"}\n     2→"
        );
        assert_eq!(
            mask_text(&texts[4]),
            // `block_ask::mask` reads `{"token":"•••"},` as the word token
            // and hides the word after it too.
            r#"saw {"token":"•••"}, ••• {"OPENAI_API_KEY":"•••"} and {"password":"•••"}."#
        );
        assert_eq!(
            mask_text(&texts[5]),
            r#"It reads 'password': ••• and "secret"=••• here."#
        );
        assert_eq!(mask_text(&texts[7]), r#"keys {"•••":true} end"#);
        assert_eq!(
            mask_text(&format!(
                "  7\t{{\"oauth\":{{\"accessToken\":\"{access}\",\"scopes\":[\"x\"]}}}}"
            )),
            "  7\t{\"oauth\":\"•••\"}"
        );
        assert_eq!(
            mask_text(&format!(
                "  7\t{{\"login\":{{\"accessToken\":\"{access}\",\"scopes\":[\"x\"]}}}}"
            )),
            "  7\t{\"login\":{\"accessToken\":\"•••\",\"scopes\":[\"x\"]}}"
        );
        assert_eq!(
            mask_value(&json!({TOKEN: 1, "ghp_0aZ3kQ7rT1mN5vB8xC2dF6hJ0lP4sW": 2})),
            json!({"•••": 2, "•••2": 1})
        );
        // Ordinary JSON and prose stay readable.
        assert_eq!(
            mask_text(r#"{"path":"/w/a.rs","ok":true} said "name": "ada""#),
            r#"{"path":"/w/a.rs","ok":true} said "name": "ada""#
        );

        // The same through each harness's log: a Write of the file, its Read,
        // a Codex command around it and the assistant quoting it.
        let line = |event: Value| event.to_string();
        let log = [
            line(json!({"type":"assistant","message":{"content":[
                {"type":"tool_use","id":"w","name":"Write","input":
                    {"file_path":"/h/.claude/.credentials.json","content":credentials}},
                {"type":"tool_use","id":"r","name":"Read","input":{"file_path":"/h/c.json"}}]}})),
            line(json!({"type":"user","message":{"content":[{"type":"tool_result",
                "tool_use_id":"r","content":format!("     1→{credentials}\n")}]}})),
            line(json!({"type":"item.completed","item":{"type":"command_execution",
                "command":"cat ~/.config/tokens.jsonl","exit_code":0,"aggregated_output":
                format!("reading\n{{\"token\":\"{TOKEN}\"}}\n{{\"OPENAI_API_KEY\":\"{openai}\"}}\nwarning: {{\"password\":\"{password}\"}} kept\n")}})),
            line(json!({"type":"item.completed","item":{"type":"agent_message",
                "text":format!("The file holds {credentials} and {{\"password\":\"{password}\"}}.")}})),
        ]
        .join("\n");
        let events = normalize(&log);
        let text = serde_json::to_string(&events).unwrap();
        for secret in planted {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        assert!(text.contains("cat ~/.config/tokens.jsonl"), "{text}");
        assert!(text.contains("warning:"), "{text}");
        assert_eq!(events.len(), 6, "{text}");
    }

    #[test]
    fn a_typed_assignment_hides_its_value_and_the_word_after_its_label() {
        // What reads as a type may be the value, so it is hidden as before.
        assert_eq!(
            mask_text("    password: str = hunter2\n    user: str = ada\n"),
            "    password: ••• = •••\n    user: str = ada\n"
        );
        assert_eq!(
            mask_text("const token: string = \"tok.planted\";\nconst port: number = 80;"),
            "const token: ••• = •••\nconst port: number = 80;"
        );
        assert_eq!(
            mask_text("let api_key: &str = \"planted-key\";"),
            "let api_key: ••• = •••"
        );
        assert_eq!(
            mask_text("    secret: Option<HashMap<String, String>> = Some(m),"),
            "    secret: ••• String>> = •••"
        );
        // A label without `=` after it still hides the word after it.
        assert_eq!(mask_text("password: swordfish now"), "password: ••• now");
    }

    #[test]
    fn what_main_hid_after_a_label_or_name_stays_hidden() {
        let cases = [
            ("token: abc123 ttl = 3600", "abc123"),
            ("password: hunter2, retries = 3", "hunter2"),
            ("secret: xyz # default = none", "xyz"),
            ("auth_token: 9f8e7d, ttl = 300", "9f8e7d"),
            ("Set-Cookie: sid123; Path = /", "sid123"),
            ("password\n= hunter2", "hunter2"),
        ];
        let mut shown: Vec<String> = cases
            .iter()
            .flat_map(|(text, secret)| {
                [mask_text(text), crate::block_ask::mask(text)]
                    .into_iter()
                    .filter(move |shown| shown.contains(secret))
            })
            .collect();
        // A value glued to a block marker is no block scalar, and a line
        // number is read only in numbered output.
        for (text, secret) in [
            ("password: >1234", "1234"),
            ("password: |1234", "1234"),
            ("password: |1234\n  below", "1234"),
            ("password:\n1234\tnext", "1234"),
            ("password:\n  1234\tnext", "1234"),
        ] {
            shown.extend(Some(mask_text(text)).filter(|shown| shown.contains(secret)));
        }
        assert!(shown.is_empty(), "{shown:#?}");
        assert_eq!(
            mask_text("    12\tpassword:\n    13\thunter2 shown"),
            "    12\tpassword:\n    13\t••• shown"
        );
    }

    #[test]
    fn closers_after_a_value_mask_in_linear_time() {
        let text = format!("password={}", ")".repeat(100_000));
        let started = std::time::Instant::now();
        let shown = mask_text(&text);
        let masked = crate::block_ask::mask(&text);
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_secs(2), "{took:?}");
        assert!(shown.starts_with("password=•••)") && shown.ends_with("))"));
        assert_eq!(shown, masked);
    }

    #[test]
    fn a_go_short_declaration_hides_its_value() {
        let shown = mask_text(
            "\tsecret := \"planted-go\"\n\tapiKey := os.Getenv(\"X\")\n\tname := \"ada\"\n",
        );
        assert_eq!(
            shown,
            "\tsecret := •••\n\tapiKey := •••\n\tname := \"ada\"\n"
        );
    }

    #[test]
    fn a_quoted_value_with_spaces_is_hidden_whole() {
        let phrase = "correct horse battery staple";
        let texts = [
            format!("password = \"{phrase}\"\nnext"),
            format!("password=\"{phrase}\" ./run"),
            format!("export API_KEY='{phrase}' && go"),
            format!("mysql --password \"{phrase}\" -h db"),
            format!("login({{ password: \"{phrase}\" }})"),
            format!("let token: String = \"{phrase}\".into();"),
            format!("tokens: [\"{phrase}\", \"b c\"] # list"),
        ];
        for text in &texts {
            let shown = mask_text(text);
            for word in phrase.split(' ') {
                assert!(!shown.contains(word), "{word} in {shown}");
            }
        }
        assert_eq!(mask_text(&texts[0]), "password = •••\nnext");
        assert_eq!(mask_text(&texts[1]), "password=••• ./run");
        assert_eq!(mask_text(&texts[2]), "export API_KEY=••• && go");
        assert_eq!(mask_text(&texts[3]), "mysql --password ••• -h db");
        assert_eq!(mask_text(&texts[4]), "login({ password: ••• })");
        assert_eq!(mask_text(&texts[6]), "tokens: ••• # list");
        // A quote never closed hides to the end of its line only.
        assert_eq!(
            mask_text("password = \"open quote\nnext line"),
            "password = •••\nnext line"
        );
        // Quoted text under an ordinary name stays.
        assert_eq!(
            mask_text("title = \"correct horse\""),
            "title = \"correct horse\""
        );
    }

    #[test]
    fn a_yaml_block_under_a_secret_key_hides_its_values() {
        let file = "\
service: api
tokens:
  - tok-alpha-one
  - tok-beta-two
api_keys:
- name: prod
  value: prodvalue
password: |
  line one
  line two
credentials:
  user: alice
  scopes:
    - read
replicas: 3
labels:
  - web
";
        let masked = "\
service: api
tokens:
  - •••
  - •••
api_keys:
- name: •••
  value: •••
password: |
  •••
  •••
credentials:
  user: •••
  scopes:
    - •••
replicas: 3
labels:
  - web
";
        assert_eq!(mask_text(file), masked);
        // The same file as a Read shows it, numbered.
        let number = |text: &str, sep: &str| {
            text.lines()
                .enumerate()
                .map(|(n, line)| format!("{:>6}{sep}{line}\n", n + 1))
                .collect::<String>()
        };
        for sep in ["→", "\t"] {
            assert_eq!(mask_text(&number(file, sep)), number(masked, sep));
        }
        // A key whose value is on its line, and a key with nothing under it,
        // hide nothing below.
        assert_eq!(
            mask_text("token: abc\n  - shown\nsecret:\nnext: 1\n"),
            "token: •••\n  - shown\nsecret:\nnext: 1\n"
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
                    "input_preview":"bash -lc 'cargo test --token=•••'"}),
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
