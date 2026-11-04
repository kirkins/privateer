//! Incremental, bounded evidence for one-time approval receipts. Keep only
//! compact Bash calls/results, never API request blobs or command output.
use super::*;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::MetadataExt;

#[cfg(test)]
mod tests;

const MAX_RECORDS: usize = 256;
const MAX_COMMAND: usize = 8192;

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Cursor {
    identity: Option<(u64, u64)>,
    pub(crate) end: u64,
    interactive: bool,
    records: Vec<Value>,
    #[cfg(test)]
    #[serde(skip)]
    pub(crate) read_bytes: usize,
}

/// Read at most STEP_BYTES, only from the last whole-line offset after the
/// first read. Replacement or truncation discards all carried evidence.
/// The first read starts at most MAX_BYTES back, so old transcripts cannot
/// make a new watcher walk their entire history. Missing/evicted evidence
/// never retires an approval: the captain can still revoke it.
pub(crate) fn read(path: &Path, folder: &Path, session: &str, cursor: &mut Cursor) -> Option<Scan> {
    let mut file = open(path)?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    let identity = (metadata.dev(), metadata.ino());
    let length = metadata.len();
    if cursor.identity != Some(identity) || length < cursor.end {
        *cursor = Cursor {
            identity: Some(identity),
            end: length.saturating_sub(MAX_BYTES),
            ..Cursor::default()
        };
    }
    file.seek(SeekFrom::Start(cursor.end)).ok()?;
    let mut bytes = vec![];
    file.take(STEP_BYTES.min(length - cursor.end))
        .read_to_end(&mut bytes)
        .ok()?;
    #[cfg(test)]
    {
        cursor.read_bytes = bytes.len();
    }
    if let Some(last) = bytes.iter().rposition(|b| *b == b'\n') {
        for line in bytes[..=last].split(|b| *b == b'\n') {
            let Ok(record) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            if !ours(&record, folder)
                || record["sessionId"].as_str() != Some(session)
                || record["timestamp"].as_str().is_none_or(|s| s.len() > 40)
            {
                continue;
            }
            if let Some(entrypoint) = record["entrypoint"].as_str() {
                if entrypoint != "cli" {
                    continue;
                }
                cursor.interactive = true;
            }
            let content: Vec<Value> = record["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|block| {
                    let id = if block["type"] == "tool_use" {
                        &block["id"]
                    } else {
                        &block["tool_use_id"]
                    };
                    id.as_str().is_some_and(|s| s.len() <= 200)
                })
                .take(MAX_RECORDS)
                .filter_map(|block| match block["type"].as_str()? {
                    "tool_use" if block["name"] == "Bash" => {
                        let command = block["input"]["command"].as_str()?;
                        (command.len() <= MAX_COMMAND).then(|| {
                            json!({"type":"tool_use","id":block["id"],"name":"Bash",
                                "input":{"command":command}})
                        })
                    }
                    "tool_result" => {
                        let classified = refusal(&record, block, &result_text(&block["content"]))
                            .is_some_and(|(_, classifier)| classifier);
                        Some(
                            json!({"type":"tool_result","tool_use_id":block["tool_use_id"],
                            "is_error":block["is_error"],
                            "content":if classified {CLASSIFIER_REFUSAL} else {""}}),
                        )
                    }
                    _ => None,
                })
                .collect();
            if content.is_empty() {
                continue;
            }
            // Drop all unrelated fields, especially request blobs and output.
            // One block per retained record keeps the persisted size bounded.
            for block in content {
                cursor.records.push(json!({"cwd":folder,"sessionId":session,
                    "timestamp":record["timestamp"],"message":{"content":[block]}}));
            }
            let over = cursor.records.len().saturating_sub(MAX_RECORDS);
            cursor.records.drain(..over);
        }
        cursor.end += last as u64 + 1;
    } else if bytes.len() as u64 == STEP_BYTES {
        // A huge API record is stepped over, as in the refusal watcher.
        cursor.end += STEP_BYTES;
    }
    let over = cursor.records.len().saturating_sub(MAX_RECORDS);
    cursor.records.drain(..over);
    Some(Scan {
        denials: if cursor.interactive {
            denials(&cursor.records, folder, 0)
        } else {
            vec![]
        },
        running: vec![],
        session: Some(session.to_owned()),
        transcript: path.to_path_buf(),
        end: cursor.end,
    })
}
