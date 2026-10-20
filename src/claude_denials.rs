//! The denials a hosted Claude first mate's own transcript records.
//!
//! A hosted pane's screen shows that something was denied, not what. Claude
//! writes each tool use and its result to a JSONL transcript under its
//! projects folder, one folder per working folder. A refused tool use's result
//! is an error whose record carries `permissionDecision.decision = "reject"`;
//! the auto-mode classifier's refusal also names its reason, such as
//! `Reason: [Credential Materialization]`.
//!
//! Only the end of the newest few transcripts is read, a symlink is never
//! followed, and a transcript the pane did not write (a headless run's) is
//! skipped. The raw input is handed to `block_ask`, which masks it.
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::SystemTime;

/// How much of the end of one transcript is read. A denial is short, but a
/// tool result before it can be long.
const TAIL_BYTES: u64 = 512 * 1024;
/// The newest transcripts tried, newest first.
const CANDIDATES: usize = 4;
/// Folder entries looked at for the newest transcripts.
const MAX_ENTRIES: usize = 4096;
/// How the classifier's refusal starts its result text.
const CLASSIFIER_REFUSAL: &str = "denied by the Claude Code auto mode classifier";
/// Clock slack between Claude's record timestamps and the screen read.
const SLACK_MS: i64 = 2_000;

/// The denials in the pane's own transcript for `folder`, newest first, at
/// or after `since_ms`, as `{tool_name, tool_input, reason, classifier}`.
/// None when no transcript of the pane's can be found or read.
pub(crate) fn since(projects: &Path, folder: &Path, since_ms: i64) -> Option<Vec<Value>> {
    let dir = projects.join(crate::host::claude_project_name(folder));
    // The folder itself must be a folder, not a link to somewhere else.
    if !std::fs::symlink_metadata(&dir).ok()?.is_dir() {
        return None;
    }
    let mut files: Vec<(SystemTime, std::path::PathBuf)> = std::fs::read_dir(&dir)
        .ok()?
        .take(MAX_ENTRIES)
        .flatten()
        .filter_map(|entry| {
            // The entry's own type: a symlink is not a file here.
            let regular = entry.file_type().ok()?.is_file();
            let path = entry.path();
            (regular && path.extension().is_some_and(|x| x == "jsonl")).then_some(())?;
            Some((entry.metadata().ok()?.modified().ok()?, path))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in files.into_iter().take(CANDIDATES) {
        let Some(records) = tail(&path) else {
            continue;
        };
        // A headless run (`claude -p`) in the same folder is not the pane.
        let marked = records.iter().any(|r| r["entrypoint"].is_string());
        if marked && !records.iter().any(|r| r["entrypoint"] == "cli") {
            continue;
        }
        return Some(denials(&records, since_ms));
    }
    None
}

/// The records in the last `TAIL_BYTES` of a transcript. A line cut at the
/// bound does not parse and is skipped.
fn tail(path: &Path) -> Option<Vec<Value>> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let length = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut bytes = vec![];
    file.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    Some(
        bytes
            .split(|b| *b == b'\n')
            .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
            .collect(),
    )
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
/// error or the person at the pane saying no is not a permission policy.
fn refusal(record: &Value, block: &Value, text: &str) -> Option<(Option<String>, bool)> {
    if block["is_error"] != true {
        return None;
    }
    let decision = &record["permissionDecision"];
    let source = decision["source"].as_str().unwrap_or_default();
    let classifier = decision["decision"] != "accept" && text.contains(CLASSIFIER_REFUSAL);
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

/// The refused tool uses among `records`, newest first. A result whose tool
/// use is before the tail cannot be named, so it is left out.
fn denials(records: &[Value], since_ms: i64) -> Vec<Value> {
    let mut uses: HashMap<&str, (&str, &Value)> = HashMap::new();
    let mut found = vec![];
    for record in records {
        let Some(blocks) = record["message"]["content"].as_array() else {
            continue;
        };
        for block in blocks {
            match block["type"].as_str() {
                Some("tool_use") => {
                    if let (Some(id), Some(name)) = (block["id"].as_str(), block["name"].as_str())
                    {
                        uses.insert(id, (name, &block["input"]));
                    }
                }
                Some("tool_result") => {
                    let text = result_text(&block["content"]);
                    let Some((reason, classifier)) = refusal(record, block, &text) else {
                        continue;
                    };
                    let at = record["timestamp"]
                        .as_str()
                        .and_then(crate::routes::iso_timestamp_ms);
                    if at.is_none_or(|at| at + SLACK_MS < since_ms) {
                        continue;
                    }
                    let Some((name, input)) =
                        block["tool_use_id"].as_str().and_then(|id| uses.get(id))
                    else {
                        continue;
                    };
                    found.push(json!({"tool_name": name, "tool_input": input,
                        "reason": reason, "classifier": classifier}));
                }
                _ => {}
            }
        }
    }
    found.reverse();
    found
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A pane transcript: a classifier-refused compound Bash command, a
    /// simple command a rule refused, a refusal the person at the pane made,
    /// and an ordinary result that quotes a refusal.
    pub(crate) const FIXTURE: &str = include_str!("../tests/fixtures/claude/pane-denials.jsonl");

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
        let found = since(temp.path(), folder, 0).unwrap();
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
        let found = since(temp.path(), folder, after_first).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["tool_input"]["command"], "cargo publish --dry-run");
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
            outside.path().join(crate::host::claude_project_name(folder)),
            linked.path().join(crate::host::claude_project_name(folder)),
        )
        .unwrap();
        assert!(since(linked.path(), folder, 0).is_none());
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
