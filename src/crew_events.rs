//! A hosted crew's job log. Its harness keeps the pane's terminal, so the
//! supervisor has no output pipe to read. The crew extension instead writes
//! the harness's own events, the JSON lines a headless Pi prints, into a FIFO
//! in the job's directory. The supervisor strips terminal escapes from each
//! line, masks it as transcripts are masked, and passes it to the bounded log
//! capture. Outcome mail, `job.status`, verdicts and a park's last actions
//! then read a hosted crew's log as they read a headless one's, while it runs
//! and after a timeout or kill. What the captain sees in the pane is
//! unchanged.
use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

/// Names the FIFO for the crew extension.
pub(crate) const ENV: &str = "PRIVATEER_CREW_EVENTS";
/// A longer event is dropped, and a line in the log says how long it was.
const MAX_LINE: usize = 1 << 20;
const DROPPED: &str = "privateer.event.dropped";

/// The FIFO, open at both ends: the read end is the supervisor's, and its own
/// write end keeps a harness's close from reading as the end while it may
/// still reopen. The stream ends when the payload has stopped.
pub(crate) struct Events {
    path: PathBuf,
    read: File,
    _write: File,
}

impl Events {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Make the job's events FIFO, private to its owner, and hold its read end
/// before the harness starts, so the extension's non-blocking open finds a
/// reader.
pub(crate) fn open(dir: &Path) -> Result<Events> {
    let path = dir.join("events");
    match fs::remove_file(&path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    let name = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot make the crew events FIFO");
    }
    let read = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)?;
    let write = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)?;
    Ok(Events {
        path,
        read,
        _write: write,
    })
}

/// Relay masked event lines to the returned pipe until `stop` is set and
/// what was written before it is read, as `jobs::capture` drains a payload's
/// pipe. The pipe ends with the relay, so its reader sees everything.
pub(crate) fn relay(
    events: Events,
    stop: Arc<AtomicBool>,
) -> Result<(UnixStream, thread::JoinHandle<Result<()>>)> {
    let (reader, mut log) = UnixStream::pair()?;
    Ok((
        reader,
        thread::spawn(move || {
            let mut events = events;
            let mut buf = [0u8; 8192];
            let mut line = Vec::new();
            // Bytes of an over-long line being skipped, once one is.
            let mut skipped: Option<usize> = None;
            let mut stopping_bytes = 0usize;
            loop {
                if stop.load(Ordering::Relaxed) && stopping_bytes >= 65536 {
                    break;
                }
                match events.read.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        for piece in buf[..n].split_inclusive(|b| *b == b'\n') {
                            let ended = piece.last() == Some(&b'\n');
                            let body = &piece[..piece.len() - usize::from(ended)];
                            match &mut skipped {
                                Some(bytes) => *bytes += body.len(),
                                None if line.len() + body.len() > MAX_LINE => {
                                    skipped = Some(line.len() + body.len());
                                    line.clear();
                                }
                                None => line.extend_from_slice(body),
                            }
                            if ended {
                                let shown = match skipped.take() {
                                    Some(bytes) => {
                                        format!("{}\n", json!({"type": DROPPED, "bytes": bytes}))
                                    }
                                    None => shown_line(&line),
                                };
                                line.clear();
                                // The log's reader only stops on its own error.
                                if log.write_all(shown.as_bytes()).is_err() {
                                    return Ok(());
                                }
                            }
                        }
                        if stop.load(Ordering::Relaxed) {
                            stopping_bytes += n;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            // A harness stopped mid-line leaves what it wrote of it.
            if skipped.is_none() && !line.is_empty() {
                let _ = log.write_all(shown_line(&line).as_bytes());
            }
            Ok(())
        }),
    ))
}

/// One line as the log keeps it: an event with escapes stripped and its text
/// masked, or other text masked as text. Blank lines are dropped.
fn shown_line(line: &[u8]) -> String {
    let text = String::from_utf8_lossy(line);
    let text = text.strip_suffix('\r').unwrap_or(&text);
    if text.trim().is_empty() {
        return String::new();
    }
    let shown = match serde_json::from_str::<Value>(text) {
        Ok(event @ Value::Object(_)) => shown_event(&event).to_string(),
        _ => crate::transcript::mask_text(&plain(text)),
    };
    format!("{shown}\n")
}

/// An event masked as a transcript masks it. What the log's readers key on
/// stays as the harness gave it: the session a park resumes, and token
/// counts, some of which sit under names masking hides.
fn shown_event(event: &Value) -> Value {
    let mut shown = crate::transcript::mask_value(&plain_value(event));
    if event["type"] == "session"
        && let Some(id) = event["id"].as_str().filter(|id| {
            !id.is_empty()
                && id.len() <= 512
                && !id.chars().any(|c| c.is_whitespace() || c.is_control())
        })
    {
        shown["id"] = json!(id);
    }
    for at in ["/usage", "/message/usage", "/result/usage"] {
        if let (Some(usage), Some(slot)) = (event.pointer(at), shown.pointer_mut(at)) {
            *slot = counts(usage);
        }
    }
    shown
}

/// Only the numbers of a usage record, nested costs included.
fn counts(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .filter(|(_, v)| v.is_number() || v.is_object())
                .map(|(k, v)| (k.clone(), counts(v)))
                .collect::<Map<_, _>>(),
        ),
        Value::Number(_) => value.clone(),
        _ => Value::Null,
    }
}

fn plain_value(value: &Value) -> Value {
    match value {
        Value::String(text) => json!(plain(text)),
        Value::Array(items) => Value::Array(items.iter().map(plain_value).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(k, v)| (plain(k), plain_value(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Text without terminal escape sequences (CSI, OSC and other string
/// controls, two- and three-character escapes) or other control characters
/// but newline and tab.
pub(crate) fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let introducer = match c {
            '\u{1b}' => chars.next(),
            '\u{9b}' => Some('['),
            '\n' | '\t' => {
                out.push(c);
                continue;
            }
            c if c.is_control() => continue,
            c => {
                out.push(c);
                continue;
            }
        };
        match introducer {
            // Parameters and intermediates up to a final byte.
            Some('[') => {
                for n in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&n) {
                        break;
                    }
                }
            }
            // A string up to BEL or the string terminator.
            Some(']' | 'P' | 'X' | '^' | '_') => {
                while let Some(n) = chars.next() {
                    if n == '\u{7}' || n == '\u{9c}' {
                        break;
                    }
                    if n == '\u{1b}' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            // A character set or similar: one more character.
            Some(' '..='/') => {
                chars.next();
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_strips_escapes_and_controls() {
        assert_eq!(plain("a\u{1b}[1;31mred\u{1b}[0m b"), "ared b");
        assert_eq!(plain("\u{1b}]0;title\u{7}x\u{1b}]8;;u\u{1b}\\y"), "xy");
        assert_eq!(plain("\u{1b}(Bz\u{1b}=w\u{9b}2Kv"), "zwv");
        assert_eq!(plain("one\r\ntwo\tthree\u{8}"), "one\ntwo\tthree");
        assert_eq!(plain("cut\u{1b}["), "cut");
    }

    #[test]
    fn events_keep_their_shape_without_escapes_or_secrets() {
        let token = "ghp_0123456789abcdefABCDEF0123456789abcd";
        let line = json!({"type":"message_end","message":{"role":"assistant","stopReason":"stop",
            "content":[{"type":"text","text":format!("\u{1b}[32mDone\u{1b}[0m with {token}")}],
            "usage":{"input":3,"output":4,"totalTokens":7,"cost":{"total":0.5}}}})
        .to_string();
        let shown = shown_line(line.as_bytes());
        assert!(
            shown.ends_with('\n') && !shown.contains('\u{1b}'),
            "{shown}"
        );
        assert!(
            !shown.contains(token) && !shown.contains("u001b"),
            "{shown}"
        );
        let event: Value = serde_json::from_str(&shown).unwrap();
        assert_eq!(event["message"]["stopReason"], "stop");
        assert!(
            crate::reply::text(&event)
                .unwrap()
                .starts_with("Done with ")
        );
        assert_eq!(
            event["message"]["usage"],
            json!({"input":3,"output":4,"totalTokens":7,"cost":{"total":0.5}})
        );

        let id = "0d5a1f0e-7b1c-4b8e-9c55-1f2e3d4c5b6a";
        let header = json!({"type":"session","id":id,"cwd":"/w"}).to_string();
        let shown = shown_line(header.as_bytes());
        assert_eq!(
            crate::activation::read_session("pi", &shown).as_deref(),
            Some(id)
        );

        let args = json!({"type":"tool_execution_start","toolName":"bash",
            "args":{"command":"deploy","api_key":"hunter2-plain"}})
        .to_string();
        let shown = shown_line(args.as_bytes());
        assert!(!shown.contains("hunter2"), "{shown}");
        // Text that is not an event is masked as text.
        let shown = shown_line(format!("\u{1b}[2Kexport TOKEN={token}\r").as_bytes());
        assert!(
            !shown.contains(token) && !shown.contains('\u{1b}'),
            "{shown}"
        );
        assert_eq!(shown_line(b"  \r"), "");
    }

    #[test]
    fn the_relay_masks_lines_and_ends_after_stop() {
        let dir = tempfile::tempdir().unwrap();
        let events = open(dir.path()).unwrap();
        let path = events.path().to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let (mut log, relay) = relay(events, stop.clone()).unwrap();
        // The relay holds the read end, so this open does not wait.
        let mut writer = OpenOptions::new().write(true).open(&path).unwrap();
        writer
            .write_all(b"{\"type\":\"agent_start\"}\n\x1b[1mloose\n")
            .unwrap();
        // Too long for one event: noted, not kept.
        writer.write_all(&vec![b'x'; MAX_LINE + 1]).unwrap();
        writer.write_all(b"xxxx\n").unwrap();
        writer.write_all(b"{\"type\":\"agent_settled\"}").unwrap();
        drop(writer);
        thread::sleep(Duration::from_millis(100));
        stop.store(true, Ordering::Relaxed);
        relay.join().unwrap().unwrap();
        let mut text = String::new();
        log.read_to_string(&mut text).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], r#"{"type":"agent_start"}"#);
        assert_eq!(lines[1], "loose");
        let dropped: Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(dropped["type"], DROPPED);
        assert_eq!(lines[3], r#"{"type":"agent_settled"}"#);
        assert_eq!(lines.len(), 4, "{text}");
        assert!(!path.exists(), "the FIFO is removed with the relay");
    }
}
