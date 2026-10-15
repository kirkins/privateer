//! `privateer watch [SHIP]`: a live, plain-text view of one ship's first
//! mate for a terminal pane, built from records and logs with no model call.
//! Lines typed into it are mailed to the ship as the captain. Inside Herdr
//! the pane reports itself as that ship's agent (working, idle or blocked),
//! so first mates appear in Herdr's agent list.
use crate::store::Store;
use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How much of a log's end each poll reads to find what is new.
const WINDOW: u64 = 256 * 1024;
const POLL: Duration = Duration::from_millis(1000);
const REPORT_EVERY: Duration = Duration::from_secs(20);
/// Rendered lines shown when attaching to a wake already under way.
const ATTACH_LINES: usize = 15;

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

struct WakeRow {
    id: String,
    job: Option<String>,
    mode: String,
    focus: String,
    status: String,
    usage: Option<String>,
    error: Option<String>,
}

/// The wake being followed and the last log lines already read from it.
struct Followed {
    id: String,
    anchor: Option<(u64, u64)>,
    ended: bool,
}

pub fn run(store: &mut Store, ship: &str) -> Result<()> {
    let record = crate::dispatch(store, "ship.show", json!({"name": ship}))?;
    let harness = record["first_mate"]["manifest"]
        .as_str()
        .and_then(|m| Path::new(m).parent()?.file_name()?.to_str())
        .unwrap_or("agent")
        .to_owned();
    unsafe {
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::signal(
                signal,
                stop as extern "C" fn(libc::c_int) as libc::sighandler_t,
            );
        }
    }
    let mut herdr = Herdr::from_env(ship, &harness);
    let input = read_input();
    println!(
        "Watching {ship} ({harness}). Type a message and press Enter to mail it; Ctrl-C to stop."
    );
    let latest_seq: i64 = store.connection().query_row(
        "SELECT COALESCE(MAX(seq),0) FROM mail WHERE recipient=?1 OR sender=?1",
        [ship],
        |r| r.get(0),
    )?;
    let mut mail_after = (latest_seq - 3).max(0);
    let mut followed: Option<Followed> = None;
    let mut reported: Option<(String, Instant)> = None;
    while !STOP.load(Ordering::Relaxed) {
        while let Ok(line) = input.try_recv() {
            let body = line.trim();
            if body.is_empty() {
                continue;
            }
            match crate::dispatch(
                store,
                "mail.send",
                json!({"from": "captain", "to": ship, "kind": "notice", "body": body}),
            ) {
                Ok(_) => println!("→ you: {}", clip(body, 300)),
                Err(error) => println!("✗ not sent: {error:#}"),
            }
        }
        mail_after = print_mail(store, ship, mail_after)?;
        let latest = latest_wake(store, ship)?;
        if let Some(wake) = &latest {
            follow(store, &mut followed, wake)?;
        }
        let (state, message) = state(store, ship, latest.as_ref())?;
        let due = reported
            .as_ref()
            .is_none_or(|(last, at)| last.as_str() != state || at.elapsed() >= REPORT_EVERY);
        if due {
            if let Some(herdr) = herdr.as_mut() {
                herdr.report(state, &message);
            }
            reported = Some((state.to_owned(), Instant::now()));
        }
        let until = Instant::now() + POLL;
        while Instant::now() < until && !STOP.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    if let Some(herdr) = herdr.as_mut() {
        herdr.release();
    }
    Ok(())
}

/// Lines from stdin, read on their own thread so polling never blocks.
fn read_input() -> mpsc::Receiver<String> {
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if send.send(line).is_err() {
                break;
            }
        }
    });
    receive
}

fn print_mail(store: &Store, ship: &str, after: i64) -> Result<i64> {
    let mut statement = store.connection().prepare(
        "SELECT seq, sender, recipient, kind, body FROM mail
         WHERE (recipient=?1 OR sender=?1) AND seq>?2 ORDER BY seq LIMIT 50",
    )?;
    let rows = statement.query_map(params![ship, after], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut last = after;
    for row in rows {
        let (seq, sender, recipient, kind, body) = row?;
        let kind = if kind == "notice" {
            String::new()
        } else {
            format!(" [{kind}]")
        };
        println!("✉ {sender} → {recipient}{kind}: {}", clip(&body, 300));
        last = seq;
    }
    Ok(last)
}

fn latest_wake(store: &Store, ship: &str) -> Result<Option<WakeRow>> {
    Ok(store
        .connection()
        .query_row(
            "SELECT id, job_id, mode, focus, status, usage, error FROM wakes
             WHERE ship=?1 ORDER BY created_ms DESC LIMIT 1",
            [ship],
            |r| {
                Ok(WakeRow {
                    id: r.get(0)?,
                    job: r.get(1)?,
                    mode: r.get(2)?,
                    focus: r.get(3)?,
                    status: r.get(4)?,
                    usage: r.get(5)?,
                    error: r.get(6)?,
                })
            },
        )
        .optional()?)
}

fn follow(store: &Store, followed: &mut Option<Followed>, wake: &WakeRow) -> Result<()> {
    let attaching = followed.is_none();
    if followed.as_ref().is_none_or(|f| f.id != wake.id) {
        if attaching && wake.status != "running" {
            // Nothing live: say how the last wake went and wait for the next.
            println!("⚓ last wake {}", ending(wake));
            *followed = Some(Followed {
                id: wake.id.clone(),
                anchor: None,
                ended: true,
            });
            return Ok(());
        }
        println!(
            "⚓ wake {} {} ({}) for {}",
            short(&wake.id),
            if attaching { "under way" } else { "started" },
            wake.mode,
            wake.focus
        );
        *followed = Some(Followed {
            id: wake.id.clone(),
            anchor: None,
            ended: false,
        });
    }
    let current = followed.as_mut().expect("set above");
    if current.ended {
        return Ok(());
    }
    if let Some(dir) = log_dir(store, wake)? {
        let lines = new_lines(&dir, &mut current.anchor);
        let mut rendered: Vec<String> = lines.iter().filter_map(|l| render(l)).collect();
        if attaching && rendered.len() > ATTACH_LINES {
            rendered.drain(..rendered.len() - ATTACH_LINES);
        }
        for line in rendered {
            println!("{line}");
        }
    }
    if wake.status != "running" && wake.status != "starting" {
        println!("⚓ wake {}", ending(wake));
        current.ended = true;
    }
    Ok(())
}

fn ending(wake: &WakeRow) -> String {
    let usage = wake
        .usage
        .as_deref()
        .and_then(|u| serde_json::from_str::<Value>(u).ok())
        .and_then(|u| {
            let parts: Vec<String> = u
                .as_object()?
                .iter()
                .filter(|(_, v)| v.as_u64().is_some_and(|n| n > 0))
                .map(|(k, v)| format!("{k} {v}"))
                .collect();
            (!parts.is_empty()).then(|| format!(" [{}]", parts.join(", ")))
        })
        .unwrap_or_default();
    let error = wake
        .error
        .as_deref()
        .map(|e| format!(": {}", clip(e, 200)))
        .unwrap_or_default();
    format!("{} {}{usage}{error}", short(&wake.id), wake.status)
}

fn log_dir(store: &Store, wake: &WakeRow) -> Result<Option<PathBuf>> {
    let Some(job) = &wake.job else {
        return Ok(None);
    };
    let incarnation: Option<String> = store
        .connection()
        .query_row(
            "SELECT json_extract(record,'$.incarnation') FROM jobs WHERE id=?1",
            [job],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(incarnation.map(|i| store.home().join("jobs").join(job).join(i)))
}

/// Complete log lines written since `anchor`, the hashes of the last two
/// lines read. A job's log is a head, then rotating tail segments that are
/// folded back in when it ends; reading the end of whichever is current and
/// resuming after the anchor follows all three without tracking offsets.
fn new_lines(dir: &Path, anchor: &mut Option<(u64, u64)>) -> Vec<String> {
    let bytes = window(dir);
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.split_inclusive('\n').collect();
    if lines.last().is_some_and(|l| !l.ends_with('\n')) {
        lines.pop(); // still being written
    }
    let hashes: Vec<u64> = lines.iter().map(|l| hash(l)).collect();
    let start = anchor
        .and_then(|(before, last)| {
            (1..hashes.len())
                .rev()
                .find(|&i| hashes[i] == last && hashes[i - 1] == before)
                .map(|i| i + 1)
        })
        .unwrap_or(0);
    if hashes.len() >= 2 {
        *anchor = Some((hashes[hashes.len() - 2], hashes[hashes.len() - 1]));
    }
    lines[start.min(lines.len())..]
        .iter()
        .map(|l| l.trim_end().to_owned())
        .collect()
}

fn window(dir: &Path) -> Vec<u8> {
    let mut segments: Vec<(std::time::SystemTime, PathBuf)> =
        ["stdout.log.tail0", "stdout.log.tail1"]
            .iter()
            .map(|name| dir.join(name))
            .filter_map(|path| Some((fs::metadata(&path).ok()?.modified().ok()?, path)))
            .collect();
    segments.sort();
    let mut bytes = Vec::new();
    if segments.is_empty() {
        tail(&dir.join("stdout.log"), WINDOW, &mut bytes);
    } else {
        for (_, path) in segments {
            tail(&path, WINDOW, &mut bytes);
        }
    }
    let excess = bytes.len().saturating_sub(WINDOW as usize);
    bytes.drain(..excess);
    bytes
}

fn tail(path: &Path, limit: u64, out: &mut Vec<u8>) {
    let Ok(mut file) = fs::File::open(path) else {
        return;
    };
    let length = file.metadata().map_or(0, |m| m.len());
    if file
        .seek(SeekFrom::Start(length.saturating_sub(limit)))
        .is_ok()
    {
        let _ = file.take(limit).read_to_end(out);
    }
}

fn hash(line: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    line.hash(&mut hasher);
    hasher.finish()
}

/// One readable line for a harness event worth showing, from Pi, Claude or
/// Codex JSON; streaming deltas and bookkeeping are skipped.
pub(crate) fn render(line: &str) -> Option<String> {
    let event: Value = serde_json::from_str(line).ok()?;
    match event["type"].as_str()? {
        // Pi
        "message_end" if event["message"]["role"] == "assistant" => {
            said(&event["message"]["content"])
        }
        "tool_execution_start" => Some(format!(
            "  ▶ {}",
            tool(event["toolName"].as_str().unwrap_or("tool"), &event["args"])
        )),
        "tool_execution_end" if event["isError"] == true => Some(format!(
            "  ✗ {} failed: {}",
            event["toolName"].as_str().unwrap_or("tool"),
            clip(&content_text(&event["result"]["content"]), 200)
        )),
        // Claude
        "assistant" => {
            let content = &event["message"]["content"];
            let mut out = vec![];
            if let Some(text) = said(content) {
                out.push(text);
            }
            for item in content.as_array().into_iter().flatten() {
                if item["type"] == "tool_use" {
                    out.push(format!(
                        "  ▶ {}",
                        tool(item["name"].as_str().unwrap_or("tool"), &item["input"])
                    ));
                }
            }
            (!out.is_empty()).then(|| out.join("\n"))
        }
        "user" => {
            let failed: Vec<String> = event["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["type"] == "tool_result" && item["is_error"] == true)
                .map(|item| format!("  ✗ failed: {}", clip(&content_text(&item["content"]), 200)))
                .collect();
            (!failed.is_empty()).then(|| failed.join("\n"))
        }
        // Codex
        "item.completed" => {
            let item = &event["item"];
            match item["type"].as_str()? {
                "agent_message" => item["text"]
                    .as_str()
                    .map(|t| format!("  💬 {}", indent(&clip(t, 600)))),
                "command_execution" => {
                    let command = clip(first_line(item["command"].as_str().unwrap_or("")), 160);
                    Some(match item["exit_code"].as_i64() {
                        Some(0) | None => format!("  ▶ {command}"),
                        Some(code) => format!("  ▶ {command}\n  ✗ exit {code}"),
                    })
                }
                _ => None,
            }
        }
        "privateer.log.truncated" => Some(format!(
            "  … {} bytes of output not kept",
            event["dropped_bytes"].as_u64().unwrap_or(0)
        )),
        _ => None,
    }
}

fn said(content: &Value) -> Option<String> {
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter(|i| i["type"] == "text")
            .filter_map(|i| i["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    let text = text.trim();
    (!text.is_empty()).then(|| format!("  💬 {}", indent(&clip(text, 600))))
}

fn tool(name: &str, args: &Value) -> String {
    let detail = [
        "command",
        "cmd",
        "path",
        "file_path",
        "pattern",
        "url",
        "query",
    ]
    .iter()
    .find_map(|key| args[*key].as_str())
    .map(|s| clip(first_line(s), 160))
    .unwrap_or_else(|| {
        let compact = args.to_string();
        if compact == "null" || compact == "{}" {
            String::new()
        } else {
            clip(&compact, 120)
        }
    });
    if detail.is_empty() {
        name.to_owned()
    } else {
        format!("{name}: {detail}")
    }
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => first_line(s).to_owned(),
        Value::Array(items) => items
            .iter()
            .find_map(|i| i["text"].as_str())
            .map(|s| first_line(s).to_owned())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn first_line(s: &str) -> &str {
    s.trim().lines().next().unwrap_or("")
}

fn indent(s: &str) -> String {
    s.replace('\n', "\n     ")
}

fn clip(s: &str, max: usize) -> String {
    let s: String = s
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect();
    if s.chars().count() <= max {
        s
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

fn short(id: &str) -> &str {
    id.strip_prefix("wake-")
        .unwrap_or(id)
        .get(..8)
        .unwrap_or(id)
}

/// What Herdr shows for this ship: working while a wake runs, blocked when
/// the captain is needed or the last wake failed, otherwise idle.
fn state(store: &Store, ship: &str, latest: Option<&WakeRow>) -> Result<(&'static str, String)> {
    if latest.is_some_and(|w| w.status == "running" || w.status == "starting") {
        return Ok(("working", format!("{ship}: wake running")));
    }
    if crate::permissions::paused(store, ship)? {
        return Ok((
            "blocked",
            format!("{ship}: tool permission question for the captain"),
        ));
    }
    if let Some(wake) = latest.filter(|w| matches!(w.status.as_str(), "failed" | "timed_out")) {
        return Ok(("blocked", format!("{ship}: last wake {}", wake.status)));
    }
    Ok(("idle", format!("{ship}: idle")))
}

/// This pane's Herdr identity, when running inside Herdr.
struct Herdr {
    socket: PathBuf,
    client: Option<crate::herdr::Client>,
    pane: String,
    agent: String,
    ship: String,
}

impl Herdr {
    fn from_env(ship: &str, harness: &str) -> Option<Self> {
        if std::env::var("HERDR_ENV").ok().as_deref() != Some("1") {
            return None;
        }
        Some(Self {
            socket: std::env::var_os("HERDR_SOCKET_PATH").map(PathBuf::from)?,
            client: None,
            pane: std::env::var("HERDR_PANE_ID")
                .ok()
                .filter(|p| !p.is_empty())?,
            agent: harness.to_owned(),
            ship: ship.to_owned(),
        })
    }

    /// Best effort: a Herdr restart or refusal never stops the watch.
    fn report(&mut self, state: &str, message: &str) {
        let result = (|| -> Result<()> {
            if self.client.is_none() {
                self.client = Some(crate::herdr::Client::connect(&self.socket)?);
            }
            let client = self.client.as_mut().context("connected above")?;
            client.report_agent(&self.pane, &self.agent, &self.ship, state, message)
        })();
        if result.is_err() {
            self.client = None;
        }
    }

    fn release(&mut self) {
        if let Some(client) = self.client.as_mut() {
            let _ = client.release_agent(&self.pane, &self.agent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(range: std::ops::Range<usize>) -> String {
        range.map(|i| format!("{{\"n\":{i}}}\n")).collect()
    }

    #[test]
    fn following_a_log_through_its_tail_segments_and_fold_skips_and_repeats_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("stdout.log");
        let mut anchor = None;
        let mut seen = vec![];
        let mut read = |anchor: &mut Option<(u64, u64)>| {
            seen.extend(new_lines(dir.path(), anchor));
        };
        // The head, written in two steps with a line still being written.
        fs::write(&log, lines(0..10) + "{\"n\":10").unwrap();
        read(&mut anchor);
        fs::write(&log, lines(0..20)).unwrap();
        read(&mut anchor);
        // The head is full; newer output goes to a rotating tail segment.
        fs::write(dir.path().join("stdout.log.tail0"), lines(20..30)).unwrap();
        read(&mut anchor);
        std::thread::sleep(Duration::from_millis(20));
        fs::write(dir.path().join("stdout.log.tail1"), lines(30..40)).unwrap();
        read(&mut anchor);
        // The job ends: segments are folded into the log after a marker.
        let folded = lines(0..20)
            + "{\"type\":\"privateer.log.truncated\",\"dropped_bytes\":0}\n"
            + &lines(20..45);
        fs::write(&log, folded).unwrap();
        fs::remove_file(dir.path().join("stdout.log.tail0")).unwrap();
        fs::remove_file(dir.path().join("stdout.log.tail1")).unwrap();
        read(&mut anchor);
        let expected: Vec<String> = (0..45).map(|i| format!("{{\"n\":{i}}}")).collect();
        assert_eq!(seen, expected);
    }
}
