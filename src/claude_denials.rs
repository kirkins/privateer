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
/// start, and where that transcript ended when it was read.
pub(crate) struct Scan {
    /// Newest first, as `{tool_name, tool_input, reason, classifier,
    /// tool_use_id, session, denied_ms}`, with `passed_ms` when the session
    /// got past it, or `{unidentified: true, tool_use_id, ...}` when its tool
    /// use cannot be found.
    pub(crate) denials: Vec<Value>,
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
fn scan(path: &Path, folder: &Path, since_ms: i64) -> Option<Scan> {
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
                transcript: path.to_path_buf(),
                end,
            });
        }
        bytes = bytes.saturating_mul(4).min(MAX_BYTES);
    }
}

/// The records in the last `bytes` of a transcript, where that read started
/// and where the transcript ended. A line cut at the start of the read does
/// not parse and is skipped.
fn tail(path: &Path, bytes: u64) -> Option<(Vec<Value>, u64, u64)> {
    let mut file = open(path)?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(bytes);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut read = vec![];
    file.take(length - start).read_to_end(&mut read).ok()?;
    let records = read
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .collect();
    Some((records, start, length))
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

/// The later successful calls in one session of the pane's transcript, from
/// `offset`: the retry keys among `wanted` they match, with their times, and
/// where the next read starts. A wanted call whose result is not written yet
/// is read again next time. None when the transcript cannot be read.
pub(crate) fn retried(
    path: &Path,
    folder: &Path,
    session: &str,
    offset: u64,
    wanted: &HashSet<String>,
) -> Option<(Vec<(String, i64)>, u64)> {
    let mut file = open(path)?;
    let length = file.metadata().ok()?.len();
    if length <= offset {
        return Some((vec![], length.min(offset)));
    }
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut read = vec![];
    file.take(STEP_BYTES.min(length - offset))
        .read_to_end(&mut read)
        .ok()?;
    // Only whole lines: a line still being written is read next time.
    let Some(last) = read.iter().rposition(|b| *b == b'\n') else {
        return Some((vec![], offset));
    };
    let mut starts = vec![];
    let mut records = vec![];
    let mut at = 0;
    for line in read[..=last].split_inclusive(|b| *b == b'\n') {
        if let Ok(record) = serde_json::from_slice::<Value>(line) {
            starts.push(offset + at as u64);
            records.push(record);
        }
        at += line.len();
    }
    let mut next = offset + last as u64 + 1;
    let uses = uses(&records);
    let mut answered = HashSet::new();
    let mut found = vec![];
    for (_, record, block) in results(&records, folder) {
        let id = block["tool_use_id"].as_str().unwrap_or_default();
        answered.insert(id);
        if block["is_error"] == true || record["sessionId"].as_str().is_some_and(|s| s != session) {
            continue;
        }
        let Some(key) = uses
            .get(id)
            .and_then(|(name, input, _)| retry_key(name, input))
        else {
            continue;
        };
        if wanted.contains(&key) {
            let ms = record["timestamp"]
                .as_str()
                .and_then(crate::routes::iso_timestamp_ms)
                .unwrap_or_default();
            found.push((key, ms));
        }
    }
    // A wanted call with no result yet holds the next read back to it,
    // unless it starts this read: then the read moves on.
    for (id, (name, input, index)) in &uses {
        if !answered.contains(id)
            && records[*index]["sessionId"]
                .as_str()
                .is_none_or(|s| s == session)
            && retry_key(name, input).is_some_and(|key| wanted.contains(&key))
            && starts[*index] > offset
        {
            next = next.min(starts[*index]);
        }
    }
    Some((found, next))
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

    #[test]
    fn a_later_retry_is_read_on_from_where_the_read_stopped() {
        let temp = tempfile::tempdir().unwrap();
        let folder = Path::new(FITFORGE);
        let session = "00000000-0000-4000-8000-0000000000f1";
        let path = pane(temp.path(), folder, &retries(folder, |n| n <= 3));
        let key = {
            let found = since(temp.path(), folder, 0).unwrap().denials;
            retry_key("Bash", &found[0]["tool_input"]).unwrap()
        };
        let wanted = HashSet::from([key.clone()]);
        let end = std::fs::metadata(&path).unwrap().len();
        assert_eq!(
            retried(&path, folder, session, end, &wanted).unwrap(),
            (vec![], end)
        );
        // The retry is written, its result is not yet: it is read again.
        let append = |text: String| {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(text.as_bytes()).unwrap();
        };
        append(filler(folder, "2026-10-20T00:40:00.000Z", 1024));
        let before_retry = std::fs::metadata(&path).unwrap().len();
        append(retries(folder, |n| n == 10 || n == 11));
        let (found, next) = retried(&path, folder, session, end, &wanted).unwrap();
        assert!(found.is_empty());
        assert_eq!(next, before_retry);
        append(retries(folder, |n| n == 12));
        let (found, next) = retried(&path, folder, session, next, &wanted).unwrap();
        let at = crate::routes::iso_timestamp_ms("2026-10-20T00:45:50Z").unwrap();
        assert_eq!(found, vec![(key, at)]);
        assert_eq!(next, std::fs::metadata(&path).unwrap().len());
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
