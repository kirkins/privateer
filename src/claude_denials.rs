//! The denials a hosted Claude first mate's own transcript records.
//!
//! A hosted pane's screen shows that something was denied, not what. Claude
//! writes each tool use and its result to a JSONL transcript under its
//! projects folder, one folder per working folder. A refused tool use's result
//! is an error whose record carries `permissionDecision.decision = "reject"`;
//! the auto-mode classifier's refusal also names its reason, such as
//! `Reason: [Credential Materialization]`. The result names its tool use by
//! `tool_use_id`, and the assistant record holding that `tool_use` gives the
//! tool and its input, however far back it is.
//!
//! Claude 2.1.300 also writes every API request it sends into the transcript
//! (`api-request`, `api-request-blob`): nine tenths of its bytes, many MiB an
//! hour in a busy session. So the end of a transcript is read first, then
//! more of it, fourfold each time up to `MAX_BYTES`, until the read reaches
//! back past the start of the incident and every refusal's tool use is found.
//!
//! A refusal the same session got past, by a later successful call of the
//! same tool with an equivalent input (the same file for an edit, the same
//! command for Bash), is marked `passed_ms`: the session is no longer blocked
//! on it. A symlink is never followed, and a transcript the pane did not write
//! (a headless run's) is skipped. Claude names a folder's transcripts by its
//! path with every other character a dash, so `web-app` and `web_app` share
//! one: only records whose `cwd` is the pane's folder, or inside it, are read
//! as the pane's. The raw input is handed to `block_ask`, which masks it.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// How much of the end of one transcript is read first.
const TAIL_BYTES: u64 = 512 * 1024;
/// The most of one transcript read back from its end for one incident.
const MAX_BYTES: u64 = 64 * 1024 * 1024;
/// The most of a transcript one pass reads forward for later calls.
const STEP_BYTES: u64 = 8 * 1024 * 1024;
/// The newest transcripts tried, newest first.
const CANDIDATES: usize = 4;
/// How the classifier's refusal starts its result text.
const CLASSIFIER_REFUSAL: &str =
    "Permission for this action was denied by the Claude Code auto mode classifier";
/// Clock slack between Claude's record timestamps and the screen read.
const SLACK_MS: i64 = 2_000;

/// The refusals one transcript of the pane's records since an incident's
/// start, and where its last whole line ended when it was read.
pub(crate) struct Scan {
    /// Newest first, as `{tool_name, tool_input, reason, classifier,
    /// tool_use_id, session, denied_ms}`, with `passed_ms` when the session
    /// got past it, or `{unidentified: true, tool_use_id, ...}` when its tool
    /// use cannot be found.
    pub(crate) denials: Vec<Value>,
    /// The pane's calls with no result written yet, as `(tool_use_id, retry
    /// key, tool, session)`: a retry, or another call, may still be running.
    pub(crate) running: Vec<(String, String, String, Option<String>)>,
    /// The session of the pane's last record read: the one writing now.
    pub(crate) session: Option<String>,
    pub(crate) transcript: PathBuf,
    pub(crate) end: u64,
}

/// The refusals in the pane's own transcript for `folder` at or after
/// `since_ms`. None when no transcript of the pane's can be found or read.
pub(crate) fn since(projects: &Path, folder: &Path, since_ms: i64) -> Option<Scan> {
    let dir = projects.join(crate::host::claude_project_name(folder));
    // The folder itself must be a folder, not a link to somewhere else.
    if !std::fs::symlink_metadata(&dir).ok()?.is_dir() {
        return None;
    }
    // The newest few regular transcripts of the whole folder, kept as it is
    // read, so however many entries it holds, none newer is left unread.
    let mut files: Vec<(SystemTime, PathBuf)> = vec![];
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        // The entry's own type: a symlink is not a file here.
        let path = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_file())
            || path.extension().is_none_or(|x| x != "jsonl")
        {
            continue;
        }
        let Some(modified) = entry.metadata().ok().and_then(|m| m.modified().ok()) else {
            continue;
        };
        files.push((modified, path));
        files.sort_by_key(|file| std::cmp::Reverse(file.0));
        files.truncate(CANDIDATES);
    }
    files
        .into_iter()
        .find_map(|(_, path)| scan(&path, folder, since_ms))
}

/// Whether a record was written in `folder` or a folder inside it.
fn ours(record: &Value, folder: &Path) -> bool {
    record["cwd"]
        .as_str()
        .is_some_and(|cwd| Path::new(cwd).starts_with(folder))
}

/// One transcript's refusals since `since_ms`, read back from its end as far
/// as the incident needs. None when the transcript is not the pane's.
pub(crate) fn scan(path: &Path, folder: &Path, since_ms: i64) -> Option<Scan> {
    let mut bytes = TAIL_BYTES;
    loop {
        let (records, start, end) = tail(path, bytes)?;
        let whole = start == 0;
        let done = whole || bytes >= MAX_BYTES;
        let pane: Vec<&Value> = records.iter().filter(|r| ours(r, folder)).collect();
        if pane.is_empty() {
            // Another folder's, or only API request records so far.
            if done {
                return None;
            }
            bytes = bytes.saturating_mul(4).min(MAX_BYTES);
            continue;
        }
        // A headless run (`claude -p`) in the same folder is not the pane.
        let marked = pane.iter().any(|r| r["entrypoint"].is_string());
        if marked && !pane.iter().any(|r| r["entrypoint"] == "cli") {
            return None;
        }
        let found = denials(&records, folder, since_ms);
        // The read reaches back past the incident's start.
        let reached = pane
            .iter()
            .filter_map(|r| r["timestamp"].as_str())
            .find_map(crate::routes::iso_timestamp_ms)
            .is_some_and(|first| first + SLACK_MS < since_ms);
        let named = found.iter().all(|d| d["unidentified"] != true);
        if done || (reached && named) {
            return Some(Scan {
                denials: found,
                running: running(&records, folder),
                session: pane
                    .iter()
                    .rev()
                    .find_map(|r| r["sessionId"].as_str())
                    .map(str::to_owned),
                transcript: path.to_path_buf(),
                end,
            });
        }
        bytes = bytes.saturating_mul(4).min(MAX_BYTES);
    }
}

/// The records in the last `bytes` of a transcript, where that read started
/// and where its last whole line ended. A line cut at the start of the read
/// does not parse and is skipped; a line still being written at its end is
/// left for a watch to read from its start once it is whole.
fn tail(path: &Path, bytes: u64) -> Option<(Vec<Value>, u64, u64)> {
    let mut file = open(path)?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(bytes);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut read = vec![];
    file.take(length - start).read_to_end(&mut read).ok()?;
    let whole = read.iter().rposition(|b| *b == b'\n').map_or(0, |n| n + 1);
    let records = read[..whole]
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .collect();
    Some((records, start, start + whole as u64))
}

/// A transcript, never through a symlink.
fn open(path: &Path) -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()
}

/// A tool result's text, whether one string or a list of text blocks.
fn result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// Whether one tool result is a permission refusal, and its reason: the
/// classifier's label, or the first line of a rule's refusal. A hook's
/// error or the person at the pane saying no is not a permission policy,
/// and a result that only quotes the classifier's sentence (a log, a test's
/// output) is no refusal: the sentence must start the result, or Claude must
/// have recorded the refusal itself.
fn refusal(record: &Value, block: &Value, text: &str) -> Option<(Option<String>, bool)> {
    if block["is_error"] != true {
        return None;
    }
    let decision = &record["permissionDecision"];
    let source = decision["source"].as_str().unwrap_or_default();
    let blocked = [record, block]
        .iter()
        .any(|v| v["toolDenialKind"] == "automode-blocked");
    let classifier = decision["decision"] != "accept"
        && (blocked
            || text.trim_start().starts_with(CLASSIFIER_REFUSAL)
            || (decision["decision"] == "reject" && text.contains(CLASSIFIER_REFUSAL)));
    let rejected =
        decision["decision"] == "reject" && source != "hook" && !source.starts_with("user");
    if !classifier && !rejected {
        return None;
    }
    let reason = if classifier {
        text.split_once("Reason: ").map(|(_, rest)| {
            rest.split(". If you have")
                .next()
                .unwrap_or(rest)
                .lines()
                .next()
                .unwrap_or_default()
        })
    } else {
        text.lines().next()
    };
    let reason = reason
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_owned);
    Some((reason, classifier))
}

/// What makes two calls the same step: the same file for any edit of it,
/// the same command for Bash, the same input for any other tool. A call of
/// the same tool with the same key after a refusal is its retry.
pub(crate) fn retry_key(tool: &str, input: &Value) -> Option<String> {
    let key = match tool {
        "Edit" | "MultiEdit" | "Write" => format!("file\0{}", input["file_path"].as_str()?),
        "NotebookEdit" => format!("notebook\0{}", input["notebook_path"].as_str()?),
        "Bash" => format!("Bash\0{}", input["command"].as_str()?.trim()),
        "" => return None,
        _ => format!("{tool}\0{input}"),
    };
    Some(format!("{:x}", Sha256::digest(key)))
}

/// The tool uses a set of records holds, by id: its tool, its input, its
/// record's position and its session. An API request record repeats a tool
/// use without a folder, so any record may name one.
fn uses(records: &[Value]) -> HashMap<&str, (&str, &Value, usize)> {
    let mut uses = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        for block in record["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if block["type"] == "tool_use"
                && let (Some(id), Some(name)) = (block["id"].as_str(), block["name"].as_str())
            {
                uses.entry(id).or_insert((name, &block["input"], index));
            }
        }
    }
    uses
}

/// The pane's own calls among `records` that no result among them answers,
/// in order, as `(tool_use_id, retry key, tool, session)`. An API request
/// record repeats calls long answered, so only the pane's records are taken.
fn running(records: &[Value], folder: &Path) -> Vec<(String, String, String, Option<String>)> {
    let answered: HashSet<&str> = results(records, folder)
        .filter_map(|(_, _, block)| block["tool_use_id"].as_str())
        .collect();
    let mut running = vec![];
    for record in records.iter().filter(|r| ours(r, folder)) {
        for block in record["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let (Some(id), Some(name)) = (block["id"].as_str(), block["name"].as_str()) else {
                continue;
            };
            if block["type"] != "tool_use" || answered.contains(id) {
                continue;
            }
            if let Some(key) = retry_key(name, &block["input"]) {
                let session = record["sessionId"].as_str().map(str::to_owned);
                running.push((id.to_owned(), key, name.to_owned(), session));
            }
        }
    }
    running
}

/// The pane's tool results among `records`, in order: the record's position,
/// the record and the result block.
fn results<'a>(
    records: &'a [Value],
    folder: &'a Path,
) -> impl Iterator<Item = (usize, &'a Value, &'a Value)> {
    records
        .iter()
        .enumerate()
        .filter(move |(_, record)| ours(record, folder))
        .flat_map(|(index, record)| {
            record["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|block| block["type"] == "tool_result")
                .map(move |block| (index, record, block))
        })
}

/// The refused tool uses among `records` at or after `since_ms`, newest
/// first, each marked `passed_ms` when a later successful call in the same
/// session was its retry.
fn denials(records: &[Value], folder: &Path, since_ms: i64) -> Vec<Value> {
    let uses = uses(records);
    // Each successful call: its position, retry key, session and time.
    let mut succeeded: Vec<(usize, String, Option<&str>, Option<i64>)> = vec![];
    let mut found = vec![];
    for (index, record, block) in results(records, folder) {
        let at = record["timestamp"]
            .as_str()
            .and_then(crate::routes::iso_timestamp_ms);
        let session = record["sessionId"].as_str();
        let id = block["tool_use_id"].as_str().unwrap_or_default();
        let named = uses.get(id);
        if block["is_error"] != true {
            if let Some(key) = named.and_then(|(name, input, _)| retry_key(name, input)) {
                succeeded.push((index, key, session, at));
            }
            continue;
        }
        let text = result_text(&block["content"]);
        let Some((reason, classifier)) = refusal(record, block, &text) else {
            continue;
        };
        if at.is_none_or(|at| at + SLACK_MS < since_ms) {
            continue;
        }
        let mut denial = match named {
            Some((name, input, _)) => json!({"tool_name": name, "tool_input": input}),
            None => {
                let mut unread = crate::block_ask::unidentified();
                unread["tool_use_id"] = json!(id);
                unread
            }
        };
        denial["reason"] = json!(reason);
        denial["classifier"] = json!(classifier);
        denial["tool_use_id"] = json!(id);
        denial["session"] = json!(session);
        denial["denied_ms"] = json!(at);
        found.push((index, denial));
    }
    let mut found: Vec<Value> = found
        .into_iter()
        .map(|(index, mut denial)| {
            let key = retry_key(
                denial["tool_name"].as_str().unwrap_or_default(),
                &denial["tool_input"],
            );
            let session = denial["session"].as_str();
            let retry = succeeded.iter().find(|(at, retried, in_session, _)| {
                *at > index
                    && key.as_ref() == Some(retried)
                    && (in_session.is_none() || session.is_none() || *in_session == session)
            });
            if let Some((_, _, _, ms)) = retry {
                denial["passed_ms"] = json!(ms.unwrap_or_default());
            }
            denial
        })
        .collect();
    found.reverse();
    found
}

/// The most calls a watch keeps waiting for their results.
pub(crate) const MAX_PENDING: usize = 64;

/// What a later result of a watched call says, and when it was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Retry {
    /// The call succeeded: the session got past the refusal.
    Passed(i64),
    /// The call was refused again: it blocks again.
    Refused(i64),
}

/// One read of a watched session's later results.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Retried {
    /// Each result that counts, in the order written, as `(retry key, tool,
    /// result)`.
    pub(crate) found: Vec<(String, String, Retry)>,
    /// Where the next read starts.
    pub(crate) next: u64,
    /// Whether the read ended at the end of what was written, at the end of
    /// a whole record: a record past it, or one still being written, may
    /// refuse a call again.
    pub(crate) whole: bool,
}

/// The key a refusal is kept under when the watch cannot see it got past:
/// its call was never read, it is another session's, or it has no time. No
/// success's retry key is ever this.
pub(crate) fn unpassable(tool_use_id: &str) -> String {
    format!("refused\0{tool_use_id}")
}

/// How long a transcript is now, never through a symlink.
pub(crate) fn written(path: &Path) -> Option<u64> {
    Some(open(path)?.metadata().ok()?.len())
}

/// The later results of the calls in one session of the pane's transcript,
/// from `offset`, in the order written, and where the next read starts.
/// Every permission refusal counts, of any call: from then on its key is
/// wanted, so a success after it passes it. `wanted` maps each key already
/// refused to when it was last refused, or got past: only a success after
/// that passes it, and a refusal from then blocks it again. A refusal the
/// watch cannot see got past (see `unpassable`) is found under a key that
/// nothing passes. A call of the session whose result is not written yet is
/// kept in `pending`, as `(tool_use_id, retry key, tool)`, so its result is
/// matched however many reads later it is written; at most `MAX_PENDING`
/// are kept, the oldest dropped first, and a dropped call's refusal is one
/// the watch cannot see got past. A line longer than one read is stepped
/// over, not read. A transcript now shorter than `offset` was cut or
/// replaced: nothing is read, and the next read starts at 0. None when the
/// transcript cannot be read.
pub(crate) fn retried(
    path: &Path,
    folder: &Path,
    session: &str,
    offset: u64,
    wanted: &HashMap<String, i64>,
    pending: &mut Vec<(String, String, String)>,
) -> Option<Retried> {
    let mut file = open(path)?;
    let length = file.metadata().ok()?.len();
    if length <= offset {
        // Shorter than the place it was read to, it was cut or replaced:
        // what it holds now was never read, so the next read starts over.
        return Some(Retried {
            found: vec![],
            next: if length == offset { offset } else { 0 },
            whole: length == offset,
        });
    }
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut read = vec![];
    file.take(STEP_BYTES.min(length - offset))
        .read_to_end(&mut read)
        .ok()?;
    // Only whole lines: a line still being written is read next time. One
    // that fills a whole read is no record this watch reads (an API
    // request's), so its head is stepped over; the rest of it, read next
    // time from inside it, does not parse.
    let Some(last) = read.iter().rposition(|b| *b == b'\n') else {
        let step = if read.len() as u64 >= STEP_BYTES {
            read.len() as u64
        } else {
            0
        };
        let next = offset + step;
        return Some(Retried {
            found: vec![],
            next,
            whole: next == length,
        });
    };
    let records: Vec<Value> = read[..=last]
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .collect();
    let next = offset + last as u64 + 1;
    let uses = uses(&records);
    let mut wanted = wanted.clone();
    let mut found = vec![];
    for (_, record, block) in results(&records, folder) {
        let id = block["tool_use_id"].as_str().unwrap_or_default();
        // A call read earlier is matched by its id; it is answered now.
        let earlier = pending
            .iter()
            .position(|(waiting, _, _)| waiting == id)
            .map(|at| pending.remove(at));
        // A failure that is no permission refusal neither passes nor blocks.
        let refused = block["is_error"] == true;
        if refused && refusal(record, block, &result_text(&block["content"])).is_none() {
            continue;
        }
        let named = earlier.map(|(_, key, tool)| (key, tool)).or_else(|| {
            let (name, input, _) = uses.get(id)?;
            Some((retry_key(name, input)?, (*name).to_owned()))
        });
        let ms = record["timestamp"]
            .as_str()
            .and_then(crate::routes::iso_timestamp_ms);
        let ours = record["sessionId"].as_str().is_none_or(|s| s == session);
        match (named, ms) {
            (Some((key, tool)), Some(ms)) if ours => {
                let since = wanted.get(&key).copied();
                let result = match since {
                    None if !refused => continue,
                    Some(since) if !refused && ms <= since => continue,
                    Some(since) if refused && ms < since => continue,
                    _ if refused => Retry::Refused(ms),
                    _ => Retry::Passed(ms),
                };
                wanted.insert(key.clone(), ms);
                found.push((key, tool, result));
            }
            // Another session's success is not this one's.
            _ if !refused => {}
            (named, ms) => {
                let tool = named.map(|(_, tool)| tool).unwrap_or_default();
                found.push((unpassable(id), tool, Retry::Refused(ms.unwrap_or_default())));
            }
        }
    }
    // A call of the session's with no result yet waits for it, in the
    // order written.
    for (id, key, tool, of) in running(&records, folder) {
        if of.as_deref().is_none_or(|s| s == session)
            && !pending.iter().any(|(known, _, _)| *known == id)
        {
            pending.push((id, key, tool));
        }
    }
    let over = pending.len().saturating_sub(MAX_PENDING);
    pending.drain(..over);
    Some(Retried {
        found,
        next,
        whole: next == length,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A pane transcript: a classifier-refused compound Bash command, a
    /// simple command a rule refused, a refusal the person at the pane made,
    /// and an ordinary result that quotes a refusal.
    pub(crate) const FIXTURE: &str = include_str!("../tests/fixtures/claude/pane-denials.jsonl");

    /// A hosted pane's 2.1.300 transcript in fitforge's shape: a Bash edit of
    /// `nativest.json` refused as `[Auto-Mode Bypass]` (line 3), an `Edit`
    /// refused by rule (6), a `git push` refused (9), then the same Bash
    /// command (12) and the same file's `Edit` (15) succeeding. Each tool use
    /// is written twice: an API request record, then the assistant's.
    pub(crate) const RETRIES: &str = include_str!("../tests/fixtures/claude/pane-retries.jsonl");

    /// The fixture's lines that `keep` keeps, for the ship at `folder`.
    pub(crate) fn retries(folder: &Path, keep: impl Fn(usize) -> bool) -> String {
        RETRIES
            .replace("/ships/fitforge", folder.to_str().unwrap())
            .lines()
            .enumerate()
            .filter(|(n, _)| keep(*n))
            .map(|(_, line)| format!("{line}\n"))
            .collect()
    }

    /// Records a busy session writes between two calls: API request
    /// records (no folder) and ordinary pane calls, about `bytes` of them.
    pub(crate) fn filler(folder: &Path, at: &str, bytes: usize) -> String {
        let mut out = String::new();
        let mut n = 0;
        while out.len() < bytes {
            n += 1;
            let blob = json!({"type": "api-request-blob", "sessionId": "00000000-0000-4000-8000-0000000000f1",
                "hash": format!("filler-{n}"), "message": {"role": "user",
                "content": [{"type": "text", "text": "x".repeat(16 * 1024)}]}});
            let call = json!({"cwd": folder, "entrypoint": "cli", "type": "assistant",
                "sessionId": "00000000-0000-4000-8000-0000000000f1", "timestamp": at,
                "message": {"role": "assistant", "content": [{"type": "tool_use",
                "id": format!("toolu_filler_{n}"), "name": "Read", "input": {"file_path": "notes.md"}}]}});
            let result = json!({"cwd": folder, "entrypoint": "cli", "type": "user",
                "sessionId": "00000000-0000-4000-8000-0000000000f1", "timestamp": at,
                "message": {"role": "user", "content": [{"type": "tool_result",
                "tool_use_id": format!("toolu_filler_{n}"), "content": "notes", "is_error": false}]}});
            for record in [blob, call, result] {
                out.push_str(&record.to_string());
                out.push('\n');
            }
        }
        out
    }

    fn pane(projects: &Path, folder: &Path, text: &str) -> PathBuf {
        let dir = projects.join(crate::host::claude_project_name(folder));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("00000000-0000-4000-8000-0000000000f1.jsonl");
        std::fs::write(&path, text).unwrap();
        path
    }

    const FITFORGE: &str = "/ships/fitforge";

    #[test]
    fn a_bash_denial_is_named_by_its_tool_use_id() {
        let temp = tempfile::tempdir().unwrap();
        let folder = Path::new(FITFORGE);
        pane(temp.path(), folder, &retries(folder, |n| n <= 3));
        let found = since(temp.path(), folder, 0).unwrap().denials;
        assert_eq!(found.len(), 1, "{found:?}");
        let denial = &found[0];
        assert_eq!(denial["tool_name"], "Bash");
        assert!(denial["unidentified"].is_null(), "{denial}");
        assert_eq!(denial["tool_use_id"], "toolu_nativest_bash");
        assert_eq!(denial["reason"], "[Auto-Mode Bypass]");
        assert!(
            denial["tool_input"]["command"]
                .as_str()
                .unwrap()
                .contains("scratch/nativest.json")
        );
        assert!(denial["passed_ms"].is_null());
        // Its tool use found only in the API request record is named too.
        pane(temp.path(), folder, &retries(folder, |n| n <= 3 && n != 2));
        let found = since(temp.path(), folder, 0).unwrap().denials;
        assert_eq!(found[0]["tool_name"], "Bash");
        // With its tool use nowhere, it is kept as unidentified, not dropped.
        pane(temp.path(), folder, &retries(folder, |n| n == 0 || n == 3));
        let found = since(temp.path(), folder, 0).unwrap().denials;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["unidentified"], true);
        assert_eq!(found[0]["tool_use_id"], "toolu_nativest_bash");
    }

    #[test]
    fn an_edit_denial_is_named_by_its_file() {
        let temp = tempfile::tempdir().unwrap();
        let folder = Path::new(FITFORGE);
        pane(temp.path(), folder, &retries(folder, |n| n <= 6));
        let found = since(temp.path(), folder, 0).unwrap().denials;
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0]["tool_name"], "Edit");
        assert_eq!(
            found[0]["tool_input"]["file_path"],
            "/ships/fitforge/scratch/plan.json"
        );
        assert_eq!(
            found[0]["reason"],
            "Permission to use Edit has been denied."
        );
        assert_eq!(found[0]["classifier"], false);
        assert_eq!(found[1]["tool_name"], "Bash");
    }

    #[test]
    fn a_refusal_the_session_retried_successfully_is_passed() {
        let temp = tempfile::tempdir().unwrap();
        let folder = Path::new(FITFORGE);
        pane(temp.path(), folder, &retries(folder, |_| true));
        let found = since(temp.path(), folder, 0).unwrap().denials;
        assert_eq!(found.len(), 3, "{found:?}");
        let by = |id: &str| found.iter().find(|d| d["tool_use_id"] == id).unwrap();
        let ms = |at: &str| json!(crate::routes::iso_timestamp_ms(at).unwrap());
        assert_eq!(
            by("toolu_nativest_bash")["passed_ms"],
            ms("2026-10-20T00:45:50Z")
        );
        assert_eq!(
            by("toolu_nativest_edit")["passed_ms"],
            ms("2026-10-20T00:45:56Z")
        );
        assert!(by("toolu_push")["passed_ms"].is_null());
    }

    #[test]
    fn a_failed_retry_or_another_sessions_success_is_not_passed() {
        let temp = tempfile::tempdir().unwrap();
        let folder = Path::new(FITFORGE);
        // The retry refused again.
        let refused = retries(folder, |n| n <= 3 || (10..=12).contains(&n)).replace(
            r#""content":"","is_error":false"#,
            r#""content":"denied again","is_error":true"#,
        );
        pane(temp.path(), folder, &refused);
        let found = since(temp.path(), folder, 0).unwrap().denials;
        assert!(found.iter().all(|d| d["passed_ms"].is_null()), "{found:?}");
        // The same command succeeding in another session is not this one's.
        let text = retries(folder, |n| n <= 3);
        let other =
            retries(folder, |n| (10..=12).contains(&n)).replace("0000000000f1", "0000000000f2");
        pane(temp.path(), folder, &(text + &other));
        let found = since(temp.path(), folder, 0).unwrap().denials;
        assert!(found[0]["passed_ms"].is_null(), "{found:?}");
        // A different command is not a retry either.
        let text = retries(folder, |n| n <= 3)
            + &retries(folder, |n| (10..=12).contains(&n))
                .replace("calories']=2400", "calories']=2500");
        pane(temp.path(), folder, &text);
        let found = since(temp.path(), folder, 0).unwrap().denials;
        assert!(found[0]["passed_ms"].is_null(), "{found:?}");
    }

    #[test]
    fn a_denial_far_behind_the_tail_is_still_named() {
        let temp = tempfile::tempdir().unwrap();
        let folder = Path::new(FITFORGE);
        // Forty minutes of a busy session after the refusal: several MiB.
        let text = retries(folder, |n| n <= 3)
            + &filler(folder, "2026-10-20T00:30:00.000Z", 3 * 1024 * 1024);
        let path = pane(temp.path(), folder, &text);
        assert!(std::fs::metadata(&path).unwrap().len() > 4 * TAIL_BYTES);
        let start = crate::routes::iso_timestamp_ms("2026-10-20T00:00:00Z").unwrap();
        let scan = since(temp.path(), folder, start).unwrap();
        assert_eq!(scan.denials.len(), 1, "{:?}", scan.denials);
        assert_eq!(scan.denials[0]["tool_name"], "Bash");
        assert_eq!(scan.end, std::fs::metadata(&path).unwrap().len());
        // A window that starts after it does not reach back for it.
        let later = crate::routes::iso_timestamp_ms("2026-10-20T00:20:00Z").unwrap();
        assert!(
            since(temp.path(), folder, later)
                .unwrap()
                .denials
                .is_empty()
        );
    }

    /// Append `text` to the transcript at `path`.
    fn append(path: &Path, text: &str) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    /// The fixture's Bash refusal as a watch wants it: its key, and when it
    /// was refused.
    fn wanted_bash(projects: &Path, folder: &Path) -> (String, HashMap<String, i64>) {
        let found = since(projects, folder, 0).unwrap().denials;
        let key = retry_key("Bash", &found[0]["tool_input"]).unwrap();
        let denied = found[0]["denied_ms"].as_i64().unwrap();
        (key.clone(), HashMap::from([(key, denied)]))
    }

    #[test]
    fn a_slow_retry_is_matched_however_many_reads_later_its_result_comes() {
        let temp = tempfile::tempdir().unwrap();
        let folder = Path::new(FITFORGE);
        let session = "00000000-0000-4000-8000-0000000000f1";
        let path = pane(temp.path(), folder, &retries(folder, |n| n <= 3));
        let (key, wanted) = wanted_bash(temp.path(), folder);
        let mut pending = vec![];
        let end = std::fs::metadata(&path).unwrap().len();
        assert_eq!(
            retried(&path, folder, session, end, &wanted, &mut pending).unwrap(),
            Retried {
                found: vec![],
                next: end,
                whole: true
            }
        );
        // The retry is written, its result is not yet: the read moves on,
        // and the call waits by its id.
        append(&path, &filler(folder, "2026-10-20T00:40:00.000Z", 1024));
        append(&path, &retries(folder, |n| n == 10 || n == 11));
        let Retried {
            found, mut next, ..
        } = retried(&path, folder, session, end, &wanted, &mut pending).unwrap();
        assert!(found.is_empty());
        assert_eq!(next, std::fs::metadata(&path).unwrap().len());
        assert_eq!(
            pending,
            vec![(
                "toolu_nativest_retry".to_owned(),
                key.clone(),
                "Bash".to_owned()
            )]
        );
        // Many reads go by while it runs, with other work and nothing new.
        for _ in 0..5 {
            append(&path, &filler(folder, "2026-10-20T00:44:00.000Z", 256));
            next = retried(&path, folder, session, next, &wanted, &mut pending)
                .unwrap()
                .next;
            assert_eq!(pending.len(), 1);
        }
        append(&path, &retries(folder, |n| n == 12));
        let Retried { found, next, .. } =
            retried(&path, folder, session, next, &wanted, &mut pending).unwrap();
        let at = crate::routes::iso_timestamp_ms("2026-10-20T00:45:50Z").unwrap();
        assert_eq!(
            found,
            vec![(key.clone(), "Bash".to_owned(), Retry::Passed(at))]
        );
        assert!(pending.is_empty(), "{pending:?}");
        assert_eq!(next, std::fs::metadata(&path).unwrap().len());
        // Its result as a refusal answers it, and passes nothing.
        let mut pending = vec![("toolu_nativest_retry".to_owned(), key, "Bash".to_owned())];
        append(
            &path,
            &retries(folder, |n| n == 12).replace(
                r#""content":"","is_error":false"#,
                r#""content":"denied again","is_error":true"#,
            ),
        );
        let found = retried(&path, folder, session, next, &wanted, &mut pending)
            .unwrap()
            .found;
        assert!(found.is_empty());
        assert!(pending.is_empty());
    }

    #[test]
    fn only_a_success_after_the_refusal_is_its_retry() {
        let temp = tempfile::tempdir().unwrap();
        let folder = Path::new(FITFORGE);
        let session = "00000000-0000-4000-8000-0000000000f1";
        let path = pane(temp.path(), folder, &retries(folder, |n| n <= 3));
        let (key, _) = wanted_bash(temp.path(), folder);
        let end = std::fs::metadata(&path).unwrap().len();
        append(&path, &retries(folder, |n| (10..=12).contains(&n)));
        // Refused again at 00:50, after this success at 00:45:50.
        let later = crate::routes::iso_timestamp_ms("2026-10-20T00:50:00Z").unwrap();
        let wanted = HashMap::from([(key, later)]);
        let found = retried(&path, folder, session, end, &wanted, &mut vec![])
            .unwrap()
            .found;
        assert!(found.is_empty(), "{found:?}");
    }

    /// The fixture's push refused at `at` with tool use `id`: its call and
    /// result, as the classifier refuses it.
    fn push_refused(folder: &Path, id: &str, at: &str) -> String {
        retries(folder, |n| n == 8 || n == 9)
            .replace("toolu_push", id)
            .replace("2026-10-20T00:08:00.000Z", at)
            .replace("2026-10-20T00:08:01.000Z", at)
    }

    #[test]
    fn a_refusal_after_a_success_is_read_with_it_in_order() {
        let temp = tempfile::tempdir().unwrap();
        let folder = Path::new(FITFORGE);
        let session = "00000000-0000-4000-8000-0000000000f1";
        let path = pane(temp.path(), folder, &retries(folder, |n| n <= 3));
        let (key, wanted) = wanted_bash(temp.path(), folder);
        let end = std::fs::metadata(&path).unwrap().len();
        // The retry succeeds at 00:45:50, then the same command is refused
        // again, all before the next read.
        append(&path, &retries(folder, |n| (10..=12).contains(&n)));
        let again = retries(folder, |n| n == 2 || n == 3)
            .replace("toolu_nativest_bash", "toolu_nativest_again")
            .replace("2026-10-20T00:06:10.000Z", "2026-10-20T00:47:00.000Z")
            .replace("2026-10-20T00:06:16.000Z", "2026-10-20T00:47:01.000Z");
        append(&path, &again);
        let found = retried(&path, folder, session, end, &wanted, &mut vec![])
            .unwrap()
            .found;
        let ms = |at: &str| crate::routes::iso_timestamp_ms(at).unwrap();
        let bash = || "Bash".to_owned();
        assert_eq!(
            found,
            vec![
                (
                    key.clone(),
                    bash(),
                    Retry::Passed(ms("2026-10-20T00:45:50Z"))
                ),
                (
                    key.clone(),
                    bash(),
                    Retry::Refused(ms("2026-10-20T00:47:01Z"))
                ),
            ]
        );
        // A key already got past is wanted from then: only its refusal
        // after that counts. A refusal of another call counts as its own.
        let wanted = HashMap::from([(key.clone(), ms("2026-10-20T00:45:50Z"))]);
        append(
            &path,
            &push_refused(folder, "toolu_push_late", "2026-10-20T00:48:00.000Z"),
        );
        let found = retried(&path, folder, session, end, &wanted, &mut vec![])
            .unwrap()
            .found;
        let push = retry_key("Bash", &json!({"command": "git push origin main"})).unwrap();
        assert_eq!(
            found,
            vec![
                (key, bash(), Retry::Refused(ms("2026-10-20T00:47:01Z"))),
                (push, bash(), Retry::Refused(ms("2026-10-20T00:48:00Z"))),
            ]
        );
    }

    #[test]
    fn the_first_read_ends_at_the_last_whole_line() {
        let temp = tempfile::tempdir().unwrap();
        let folder = Path::new(FITFORGE);
        let whole = retries(folder, |n| n <= 3);
        let cut = retries(folder, |n| n == 12);
        let path = pane(
            temp.path(),
            folder,
            &(whole.clone() + &cut[..cut.len() / 2]),
        );
        let scan = since(temp.path(), folder, 0).unwrap();
        assert_eq!(scan.denials.len(), 1);
        assert_eq!(scan.end, whole.len() as u64);
        assert!(scan.end < std::fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn a_read_ending_inside_a_record_is_not_whole() {
        let temp = tempfile::tempdir().unwrap();
        let folder = Path::new(FITFORGE);
        let session = "00000000-0000-4000-8000-0000000000f1";
        let path = pane(temp.path(), folder, &retries(folder, |n| n <= 3));
        let (_, wanted) = wanted_bash(temp.path(), folder);
        let end = std::fs::metadata(&path).unwrap().len();
        // The retry succeeds, and the same command's refusal is half
        // written when the watch reads.
        append(&path, &retries(folder, |n| (10..=12).contains(&n)));
        let again = retries(folder, |n| n == 2 || n == 3)
            .replace("toolu_nativest_bash", "toolu_nativest_again")
            .replace("2026-10-20T00:06:10.000Z", "2026-10-20T00:47:00.000Z")
            .replace("2026-10-20T00:06:16.000Z", "2026-10-20T00:47:01.000Z");
        let (head, rest) = again.split_at(again.len() - 10);
        append(&path, head);
        let read = retried(&path, folder, session, end, &wanted, &mut vec![]).unwrap();
        assert!(!read.whole, "{read:?}");
        assert!(read.next < std::fs::metadata(&path).unwrap().len());
        // Once it is whole, the read reaches the end, and the refusal.
        append(&path, rest);
        let mut pending = vec![];
        let first = retried(&path, folder, session, end, &wanted, &mut pending).unwrap();
        assert!(first.whole, "{first:?}");
        assert_eq!(first.found.len(), 2, "{first:?}");
    }

    #[test]
    fn a_transcript_shorter_than_the_place_read_to_is_read_again_from_its_start() {
        let temp = tempfile::tempdir().unwrap();
        let folder = Path::new(FITFORGE);
        let session = "00000000-0000-4000-8000-0000000000f1";
        let path = pane(temp.path(), folder, &retries(folder, |n| n <= 3));
        let (_, wanted) = wanted_bash(temp.path(), folder);
        let end = std::fs::metadata(&path).unwrap().len();
        let read = retried(&path, folder, session, end + 100, &wanted, &mut vec![]).unwrap();
        assert!(!read.whole, "{read:?}");
        assert!(read.found.is_empty(), "{read:?}");
        assert_eq!(read.next, 0);
        // Read again from there, what it holds now is read, whole.
        let read = retried(&path, folder, session, read.next, &wanted, &mut vec![]).unwrap();
        assert!(read.whole, "{read:?}");
        assert_eq!(read.next, end);
        assert_eq!(read.found.len(), 1, "{read:?}");
    }

    #[test]
    fn a_line_longer_than_one_read_is_stepped_over() {
        let temp = tempfile::tempdir().unwrap();
        let folder = Path::new(FITFORGE);
        let session = "00000000-0000-4000-8000-0000000000f1";
        let path = pane(temp.path(), folder, &retries(folder, |n| n <= 3));
        let (key, wanted) = wanted_bash(temp.path(), folder);
        let end = std::fs::metadata(&path).unwrap().len();
        // One API request record bigger than a whole read, then the retry.
        let huge = json!({"type": "api-request-blob", "sessionId": session, "message": {"role": "user",
            "content": [{"type": "text", "text": "x".repeat(STEP_BYTES as usize + 1024)}]}});
        append(&path, &format!("{huge}\n"));
        append(&path, &retries(folder, |n| (10..=12).contains(&n)));
        let mut pending = vec![];
        let mut offset = end;
        let mut found = vec![];
        let mut reads = vec![];
        for _ in 0..4 {
            let Retried {
                found: more,
                next,
                whole,
            } = retried(&path, folder, session, offset, &wanted, &mut pending).unwrap();
            found.extend(more);
            reads.push(whole);
            assert!(next > offset || next == std::fs::metadata(&path).unwrap().len());
            offset = next;
        }
        let at = crate::routes::iso_timestamp_ms("2026-10-20T00:45:50Z").unwrap();
        assert_eq!(found, vec![(key, "Bash".to_owned(), Retry::Passed(at))]);
        // Only a read that reaches the end of what was written is whole.
        assert_eq!(reads, vec![false, true, true, true]);
        // A line still being written, shorter than a read, is waited for:
        // the read does not reach the end of what was written.
        let length = std::fs::metadata(&path).unwrap().len();
        append(&path, r#"{"type":"api-request","#);
        let Retried { next, whole, .. } =
            retried(&path, folder, session, length, &wanted, &mut pending).unwrap();
        assert_eq!(next, length);
        assert!(!whole);
    }

    /// Write the fixture as the pane's transcript for `folder` under `projects`.
    pub(crate) fn plant(projects: &Path, folder: &Path) {
        let dir = projects.join(crate::host::claude_project_name(folder));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session.jsonl"), FIXTURE).unwrap();
    }

    #[test]
    fn the_refused_tool_uses_are_named_with_their_reasons_newest_first() {
        let temp = tempfile::tempdir().unwrap();
        let folder = Path::new("/ships/sysadmin");
        plant(temp.path(), folder);
        let found = since(temp.path(), folder, 0).unwrap().denials;
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0]["tool_name"], "Bash");
        assert_eq!(found[0]["tool_input"]["command"], "cargo publish --dry-run");
        assert_eq!(found[0]["classifier"], false);
        assert_eq!(found[0]["reason"], "This command requires approval");
        assert_eq!(found[1]["classifier"], true);
        assert_eq!(found[1]["reason"], "[Credential Materialization]");
        assert!(
            found[1]["tool_input"]["command"]
                .as_str()
                .unwrap()
                .contains("&&")
        );
    }

    #[test]
    fn only_denials_since_the_last_incident_are_taken() {
        let temp = tempfile::tempdir().unwrap();
        let folder = Path::new("/ships/sysadmin");
        plant(temp.path(), folder);
        let after_first = crate::routes::iso_timestamp_ms("2026-10-20T19:03:30Z").unwrap();
        let found = since(temp.path(), folder, after_first).unwrap().denials;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["tool_input"]["command"], "cargo publish --dry-run");
    }

    #[test]
    fn a_result_that_only_quotes_the_classifier_is_no_refusal() {
        let refusal = format!("{CLASSIFIER_REFUSAL}. Reason: [Credential Leakage]. If you have");
        let record = |text: &str, extra: Value| {
            let mut record = json!({"timestamp": "2026-10-20T19:03:12.000Z", "cwd": "/ships/sysadmin",
                "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_1",
                    "content": text, "is_error": true}]}});
            for (key, value) in extra.as_object().unwrap() {
                record[key] = value.clone();
            }
            vec![
                json!({"message": {"content": [{"type": "tool_use", "id": "toolu_1",
                    "name": "Bash", "input": {"command": "make test"}}]}}),
                record,
            ]
        };
        // A failed command whose output quotes the sentence.
        let quoted = format!("make: *** test failed\n{refusal}");
        assert!(denials(&record(&quoted, json!({})), Path::new("/ships/sysadmin"), 0).is_empty());
        // The sentence starting the result, or Claude's own record of it.
        let found = denials(
            &record(&refusal, json!({})),
            Path::new("/ships/sysadmin"),
            0,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["reason"], "[Credential Leakage]");
        assert_eq!(found[0]["classifier"], true);
        let marked = denials(
            &record(&quoted, json!({"toolDenialKind": "automode-blocked"})),
            Path::new("/ships/sysadmin"),
            0,
        );
        assert_eq!(marked[0]["classifier"], true);
        let rejected = denials(
            &record(
                &quoted,
                json!({"permissionDecision": {"decision": "reject", "source": "config"}}),
            ),
            Path::new("/ships/sysadmin"),
            0,
        );
        assert_eq!(rejected[0]["classifier"], true);
    }

    #[test]
    fn a_missing_or_linked_transcript_is_not_read() {
        let temp = tempfile::tempdir().unwrap();
        let folder = Path::new("/ships/sysadmin");
        assert!(since(temp.path(), folder, 0).is_none());
        // A transcript that links out of the folder is not followed.
        let outside = tempfile::tempdir().unwrap();
        plant(outside.path(), folder);
        let dir = temp.path().join(crate::host::claude_project_name(folder));
        std::fs::create_dir_all(&dir).unwrap();
        let real = outside
            .path()
            .join(crate::host::claude_project_name(folder))
            .join("session.jsonl");
        std::os::unix::fs::symlink(&real, dir.join("session.jsonl")).unwrap();
        assert!(since(temp.path(), folder, 0).is_none());
        // Nor is a projects folder that is itself a link.
        let linked = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            outside
                .path()
                .join(crate::host::claude_project_name(folder)),
            linked.path().join(crate::host::claude_project_name(folder)),
        )
        .unwrap();
        assert!(since(linked.path(), folder, 0).is_none());
    }

    #[test]
    fn a_folder_sharing_the_panes_transcript_name_is_not_the_pane() {
        let temp = tempfile::tempdir().unwrap();
        let ours = Path::new("/ships/web-app");
        let theirs = Path::new("/ships/web_app");
        assert_eq!(
            crate::host::claude_project_name(ours),
            crate::host::claude_project_name(theirs)
        );
        let dir = temp.path().join(crate::host::claude_project_name(ours));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("theirs.jsonl"),
            FIXTURE.replace("/ships/sysadmin", "/ships/web_app"),
        )
        .unwrap();
        assert!(since(temp.path(), ours, 0).is_none());
        // Our own pane, working in a folder inside the ship's, is read.
        std::fs::write(
            dir.join("ours.jsonl"),
            FIXTURE.replace("/ships/sysadmin", "/ships/web-app/src"),
        )
        .unwrap();
        assert_eq!(since(temp.path(), ours, 0).unwrap().denials.len(), 2);
    }

    #[test]
    fn the_newest_transcripts_are_read_however_many_the_folder_holds() {
        let temp = tempfile::tempdir().unwrap();
        let folder = Path::new("/ships/sysadmin");
        let dir = temp.path().join(crate::host::claude_project_name(folder));
        std::fs::create_dir_all(&dir).unwrap();
        let old = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        for n in 0..(CANDIDATES * 3) {
            let path = dir.join(format!("old-{n}.jsonl"));
            // An older pane session that refused nothing.
            std::fs::write(&path, FIXTURE.lines().next().unwrap()).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(old)
                .unwrap();
            // Neither a folder nor a file of another kind is a transcript.
            std::fs::create_dir(dir.join(format!("session-{n}"))).unwrap();
            std::fs::write(dir.join(format!("notes-{n}.txt")), "").unwrap();
        }
        std::fs::write(dir.join("pane.jsonl"), FIXTURE).unwrap();
        assert_eq!(since(temp.path(), folder, 0).unwrap().denials.len(), 2);
    }

    #[test]
    fn a_headless_transcript_is_not_the_panes() {
        let temp = tempfile::tempdir().unwrap();
        let folder = Path::new("/ships/sysadmin");
        let dir = temp.path().join(crate::host::claude_project_name(folder));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("headless.jsonl"),
            FIXTURE.replace(r#""entrypoint":"cli""#, r#""entrypoint":"sdk-cli""#),
        )
        .unwrap();
        assert!(since(temp.path(), folder, 0).is_none());
    }
}
