//! Away mode: the captain says they are away, and the fleet holds its mail.
//!
//! The captain talks to the fleet mostly from a phone. While away, the front
//! door stops pushing their mail: it stays in each ship's home, unacknowledged,
//! and the door simply does not carry it. Only emergencies, high-urgency mail
//! from a ship, still reach the phone, at most three an hour. The captain's
//! first ordinary message ends the away period: they get a digest built from
//! records at once, and the liaison is asked for a plain-words summary of what
//! was held. Away changes delivery, never authority; the captain's away words
//! go to the liaison as an ordinary captain message.
use crate::store::Store;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::path::PathBuf;

const HOUR_MS: i64 = 60 * 60 * 1000;
/// Emergencies that reach the phone per hour while away; more are held.
pub const BREAKTHROUGHS_PER_HOUR: usize = 3;
const HELD_LIMIT: usize = 200;
/// The summary request carries this much of each held message's body.
const SUMMARY_BODY_CHARS: usize = 400;
const SUMMARY_CHARS: usize = 24_000;

fn registry() -> Result<rusqlite::Connection> {
    let conn = crate::fleet::registry()?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS away(started_ms INTEGER PRIMARY KEY, words TEXT NOT NULL,
           until_text TEXT, returned_ms INTEGER, breakthroughs TEXT NOT NULL DEFAULT '[]',
           source TEXT);",
    )?;
    Ok(conn)
}

/// The away period in force, if any.
pub fn active() -> Result<Option<Value>> {
    period("WHERE returned_ms IS NULL")
}

/// The latest away period, in force or ended.
fn latest() -> Result<Option<Value>> {
    period("")
}

fn period(filter: &str) -> Result<Option<Value>> {
    let conn = registry()?;
    Ok(conn
        .query_row(
            &format!(
                "SELECT started_ms, words, until_text, returned_ms, breakthroughs, source FROM away
                 {filter} ORDER BY started_ms DESC LIMIT 1"
            ),
            [],
            |r| {
                Ok(
                    json!({"since_ms": r.get::<_, i64>(0)?, "words": r.get::<_, String>(1)?,
                    "until": r.get::<_, Option<String>>(2)?,
                    "returned_ms": r.get::<_, Option<i64>>(3)?,
                    "breakthroughs": serde_json::from_str::<Value>(&r.get::<_, String>(4)?)
                        .unwrap_or(json!([])),
                    "source": r.get::<_, Option<String>>(5)?}),
                )
            },
        )
        .optional()?)
}

fn members() -> Result<Vec<(String, PathBuf)>> {
    Ok(crate::fleet::members(&crate::fleet::registry()?)?
        .into_iter()
        .filter(|m| m.home.is_dir())
        .map(|m| (m.name, m.home))
        .collect())
}

fn liaison() -> Result<Option<(String, PathBuf)>> {
    Ok(members()?
        .into_iter()
        .find(|(_, home)| crate::door::is_liaison(home)))
}

/// Mail the liaison as the captain, in the liaison's home.
fn tell_liaison(id: &str, kind: &str, body: &str) -> Result<Option<String>> {
    let Some((ship, home)) = liaison()? else {
        return Ok(None);
    };
    let mut store = Store::open(&home)?;
    let sent = crate::mail::dispatch(
        &mut store,
        "mail.send",
        json!({"id": id, "from": crate::mail::CAPTAIN, "to": ship, "kind": kind, "body": body}),
    )?;
    Ok(sent["id"].as_str().map(str::to_owned))
}

/// Local wall-clock time as HH:MM, without a date library.
pub(crate) fn clock(ms: i64) -> String {
    let secs = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

/// `/afk [words]`: the captain is away from now. Running it again while away
/// replaces the words; with none, it leaves the period as it is.
pub fn enter(words: &str, until: Option<&str>) -> Result<Value> {
    let words = words.trim();
    let conn = registry()?;
    let now = Store::now_ms();
    let current = active()?;
    let since = match &current {
        Some(period) => {
            let since = period["since_ms"].as_i64().unwrap_or(now);
            if words.is_empty() && until.is_none() {
                return Ok(json!({"away": period, "refreshed": false}));
            }
            conn.execute(
                "UPDATE away SET words=?2, until_text=COALESCE(?3, until_text) WHERE started_ms=?1",
                params![since, words, until],
            )?;
            since
        }
        None => {
            conn.execute(
                "INSERT INTO away(started_ms, words, until_text) VALUES(?1, ?2, ?3)",
                params![now, words, until],
            )?;
            now
        }
    };
    // The liaison acts for the captain: it learns of the away period, and the
    // words are its standing instruction until the return.
    let mut note = format!(
        "The captain is away from {}{}. Their mail is held until they return; only high-urgency messages reach them (at most {BREAKTHROUGHS_PER_HOUR} an hour).",
        clock(since),
        until
            .map(|u| format!(" (back about {u})"))
            .unwrap_or_default()
    );
    if words.is_empty() {
        note.push_str(" They left no instructions; hold what needs them for their return.");
    } else {
        note.push_str(&format!(
            "\nTheir instructions while away, in their words: \"{words}\"\nAct on them only within your charter. A decision they answer can be recorded on the asking ship's request with captain.relay, citing this message as the source. Hold everything else that needs the captain for their return; production deployment approvals are never relayed."
        ));
    }
    let source = tell_liaison(&format!("away-{since}-{now}"), "notice", &note)?;
    conn.execute(
        "UPDATE away SET source=?2 WHERE started_ms=?1",
        params![since, source],
    )?;
    Ok(json!({"away": active()?, "refreshed": current.is_some()}))
}

/// Whether a message to the captain may reach the phone while away: a ship's
/// high-urgency mail, within the hourly budget. A message that already broke
/// through stays allowed, so redelivery after a restart costs nothing.
pub fn breakthrough(mail: &Value) -> Result<bool> {
    if mail["urgency"] != "high" || mail["from"] == crate::mail::CREW {
        return Ok(false);
    }
    let Some(period) = active()? else {
        return Ok(false);
    };
    let id = mail["id"].as_str().unwrap_or_default();
    let mut through: Vec<Value> = period["breakthroughs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if through.iter().any(|b| b["id"] == id) {
        return Ok(true);
    }
    let now = Store::now_ms();
    let recent = through
        .iter()
        .filter(|b| b["ms"].as_i64().unwrap_or(0) > now - HOUR_MS)
        .count();
    if recent >= BREAKTHROUGHS_PER_HOUR {
        return Ok(false);
    }
    through.push(json!({"ms": now, "id": id}));
    registry()?.execute(
        "UPDATE away SET breakthroughs=?2 WHERE started_ms=?1",
        params![period["since_ms"].as_i64(), json!(through).to_string()],
    )?;
    Ok(true)
}

/// Captain mail held during the latest away period, oldest first: every
/// message to the captain in that window that did not break through.
pub fn held() -> Result<Value> {
    let Some(period) = latest()? else {
        return Ok(json!({"away": null, "held": []}));
    };
    let since = period["since_ms"].as_i64().unwrap_or(0);
    let until = period["returned_ms"].as_i64().unwrap_or(i64::MAX);
    let through: Vec<&str> = period["breakthroughs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| b["id"].as_str())
        .collect();
    let mut held = vec![];
    for (ship, home) in members()? {
        let store = Store::open(&home)?;
        let mut statement = store.connection().prepare(
            "SELECT id, sender, kind, urgency, body, created_ms, state FROM mail
             WHERE recipient=?1 AND created_ms >= ?2 AND created_ms <= ?3 ORDER BY seq",
        )?;
        let rows = statement.query_map(params![crate::mail::CAPTAIN, since, until], |r| {
            Ok(
                json!({"ship": ship, "id": r.get::<_, String>(0)?, "from": r.get::<_, String>(1)?,
                "kind": r.get::<_, String>(2)?, "urgency": r.get::<_, String>(3)?,
                "body": r.get::<_, String>(4)?, "created_ms": r.get::<_, i64>(5)?,
                "state": r.get::<_, Option<String>>(6)?}),
            )
        })?;
        for row in rows {
            let row = row?;
            if !through.contains(&row["id"].as_str().unwrap_or_default()) {
                held.push(row);
            }
        }
    }
    held.sort_by_key(|m| m["created_ms"].as_i64().unwrap_or(0));
    held.truncate(HELD_LIMIT);
    Ok(json!({"away": period, "held": held}))
}

fn duration(ms: i64) -> String {
    let minutes = (ms / 60_000).max(0);
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m} m"),
        (h, m) => format!("{h} h {m} m"),
    }
}

/// The captain is back: end the away period, mark the held mail carried, ask
/// the liaison to summarize it, and return a digest built from records.
/// Nothing happens when the captain is not away.
pub fn back() -> Result<Option<Value>> {
    let Some(period) = active()? else {
        return Ok(None);
    };
    let since = period["since_ms"].as_i64().unwrap_or(0);
    let now = Store::now_ms();
    registry()?.execute(
        "UPDATE away SET returned_ms=?2 WHERE started_ms=?1",
        params![since, now],
    )?;
    let held = held()?["held"].as_array().cloned().unwrap_or_default();
    // Held mail is summarized, not replayed: every door's cursor moves past
    // what reached the captain's mailbox while they were away.
    let doors = crate::door::names()?;
    for (_, home) in members()? {
        let store = Store::open(&home)?;
        let latest: i64 = store.connection().query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM mail WHERE recipient=?1",
            [crate::mail::CAPTAIN],
            |r| r.get(0),
        )?;
        for door in &doors {
            crate::door::advance(&store, door, latest)?;
        }
    }
    let waiting = crate::captain::waiting()?["waiting"]
        .as_array()
        .map_or(0, Vec::len);
    let ships: std::collections::BTreeSet<&str> =
        held.iter().filter_map(|m| m["ship"].as_str()).collect();
    let sent = period["breakthroughs"].as_array().map_or(0, Vec::len);
    let urgent_held = held.iter().filter(|m| m["urgency"] == "high").count();
    let mut digest = format!(
        "Welcome back. Away {}: {} message{} from {} ship{}, {} waiting on you, {} emergenc{} sent.",
        duration(now - since),
        held.len(),
        if held.len() == 1 { "" } else { "s" },
        ships.len(),
        if ships.len() == 1 { "" } else { "s" },
        waiting,
        sent,
        if sent == 1 { "y" } else { "ies" },
    );
    if urgent_held > 0 {
        digest.push_str(&format!(
            " {urgent_held} urgent message{} held over the hourly limit.",
            if urgent_held == 1 { " was" } else { "s were" }
        ));
    }
    let mut summary = None;
    if held.is_empty() && waiting > 0 {
        let body = format!(
            "I'm back after {} away. Nothing new reached my mailbox, but {waiting} request{} still wait{} on me: remind me of each in plain words, with its ship, then answer anything I write next.",
            duration(now - since),
            if waiting == 1 { "" } else { "s" },
            if waiting == 1 { "s" } else { "" },
        );
        summary = tell_liaison(&format!("away-{since}-summary"), "request", &body)?;
    } else if !held.is_empty() {
        let mut body = format!(
            "I'm back after {} away. Please summarize for me, in plain words, what reached my mailbox while I was away: first what needs me (decisions and requests, each with its ship), then what finished, then what is still in progress. Collapse updates that a later message superseded. Then answer anything I write next.\nHeld messages, oldest first:\n",
            duration(now - since)
        );
        for m in &held {
            let text: String = m["body"]
                .as_str()
                .unwrap_or("")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(SUMMARY_BODY_CHARS)
                .collect();
            let line = format!(
                "- {} {} ({}{}{}): {text}\n",
                m["ship"].as_str().unwrap_or(""),
                m["kind"].as_str().unwrap_or(""),
                clock(m["created_ms"].as_i64().unwrap_or(0)),
                if m["urgency"] == "high" {
                    ", urgent"
                } else {
                    ""
                },
                if m["state"] == "open" {
                    ", still open"
                } else {
                    ""
                },
            );
            if body.len() + line.len() > SUMMARY_CHARS {
                body.push_str("- ...more; /held lists them all.\n");
                break;
            }
            body.push_str(&line);
        }
        summary = tell_liaison(&format!("away-{since}-summary"), "request", &body)?;
    }
    Ok(Some(
        json!({"digest": digest, "held": held.len(), "waiting": waiting,
        "breakthroughs": sent, "summary_requested": summary, "since_ms": since, "returned_ms": now}),
    ))
}

/// One line for every ship's bearings while the captain is away.
pub fn bearings_line() -> Option<String> {
    let period = active().ok().flatten()?;
    Some(format!(
        "Captain away since {}{}: only high-urgency messages reach them. Record waits and keep working rather than stopping on a question.",
        clock(period["since_ms"].as_i64().unwrap_or(0)),
        period["until"]
            .as_str()
            .map(|u| format!(" (back about {u})"))
            .unwrap_or_default()
    ))
}

/// The away period's state for status views.
pub fn status() -> Result<Value> {
    Ok(json!({"away": active()?}))
}

/// Split `until 8am rest of words` into the expected return and the words.
pub fn parse(text: &str) -> (Option<String>, String) {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix("until ") {
        let mut parts = rest.trim_start().splitn(2, char::is_whitespace);
        let until = parts.next().map(str::to_owned).filter(|u| !u.is_empty());
        return (until, parts.next().unwrap_or("").trim().to_owned());
    }
    (None, text.to_owned())
}

/// `privateer afk [until TIME] [WORDS]`, `privateer back`, `privateer away`.
pub fn cli(command: &str, args: &[String]) -> Result<()> {
    match command {
        "afk" => {
            let (until, words) = parse(&args.join(" "));
            let entered = enter(&words, until.as_deref())?;
            println!(
                "Away{}: the fleet holds your mail; only high-urgency messages reach you, at most {BREAKTHROUGHS_PER_HOUR} an hour.",
                if entered["refreshed"] == true {
                    " (updated)"
                } else {
                    ""
                },
            );
            if let Some(words) = entered["away"]["words"].as_str().filter(|w| !w.is_empty()) {
                println!("Your instructions for the liaison: \"{words}\"");
            }
        }
        "back" => match back()? {
            Some(returned) => println!("{}", returned["digest"].as_str().unwrap_or("")),
            None => println!("You were not away."),
        },
        _ => match active()? {
            Some(away) => println!(
                "Away since {}; {} emergencies sent.",
                clock(away["since_ms"].as_i64().unwrap_or(0)),
                away["breakthroughs"].as_array().map_or(0, Vec::len)
            ),
            None => println!("Not away."),
        },
    }
    Ok(())
}
