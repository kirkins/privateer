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
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;

const HOUR_MS: i64 = 60 * 60 * 1000;
/// Emergencies that reach the phone per hour while away; more are held.
pub const BREAKTHROUGHS_PER_HOUR: usize = 3;
const HELD_LIMIT: usize = 200;
/// The summary request carries this much of each held message's body.
const SUMMARY_BODY_CHARS: usize = 400;
const SUMMARY_CHARS: usize = 24_000;

/// Every away default in one place. An optional `away.json` beside the
/// fleet registry overrides any of them; `/afk every 30m` overrides the
/// digest period for one away period.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub digest_every_mins: f64,
    /// A digest without urgent items is sent without a sound.
    pub digest_quiet: bool,
    pub digest_lines: usize,
    pub line_chars: usize,
    /// A digest with over-the-limit urgent items is due this soon after the first.
    pub urgent_due_mins: f64,
    pub breakthroughs_per_hour: usize,
    pub ship_breakthroughs_per_hour: usize,
    /// Requests from one ship that make a sound per hour; later ones go silently.
    pub loud_requests_per_hour: usize,
    pub repeat_hours: f64,
    /// Bodies at least this long count as repeats across ships (a verbatim relay).
    pub cross_ship_chars: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            digest_every_mins: 60.0,
            digest_quiet: true,
            digest_lines: 20,
            line_chars: 120,
            urgent_due_mins: 15.0,
            breakthroughs_per_hour: BREAKTHROUGHS_PER_HOUR,
            ship_breakthroughs_per_hour: 2,
            loud_requests_per_hour: 3,
            repeat_hours: 24.0,
            cross_ship_chars: 200,
        }
    }
}

/// The away settings in force: the defaults, overridden by `away.json`.
pub fn settings() -> Settings {
    let path = crate::fleet::registry_path()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("away.json")));
    path.and_then(|p| fs::read(p).ok())
        .filter(|raw| raw.len() <= 65_536)
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default()
}

fn registry() -> Result<rusqlite::Connection> {
    let conn = crate::fleet::registry()?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS away(started_ms INTEGER PRIMARY KEY, words TEXT NOT NULL,
           until_text TEXT, returned_ms INTEGER, breakthroughs TEXT NOT NULL DEFAULT '[]',
           source TEXT);",
    )?;
    let columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('away')")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for (column, definition) in [
        ("overflow", "TEXT NOT NULL DEFAULT '[]'"),
        ("requests", "TEXT NOT NULL DEFAULT '[]'"),
        ("digest_every_secs", "INTEGER"),
        ("entered_provenance", "TEXT"),
        ("returned_provenance", "TEXT"),
    ] {
        if !columns.iter().any(|c| c == column) {
            // Another process opening the registry at the same moment may add
            // it first; its duplicate is the same outcome.
            if let Err(error) = conn.execute_batch(&format!(
                "ALTER TABLE away ADD COLUMN {column} {definition}"
            )) && !error.to_string().contains("duplicate column name")
            {
                return Err(error.into());
            }
        }
    }
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
    period_in(&registry()?, filter)
}

fn period_in(conn: &rusqlite::Connection, filter: &str) -> Result<Option<Value>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT started_ms, words, until_text, returned_ms, breakthroughs, source, overflow,
                   requests, digest_every_secs FROM away
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
                    "source": r.get::<_, Option<String>>(5)?,
                    "overflow": serde_json::from_str::<Value>(&r.get::<_, String>(6)?)
                        .unwrap_or(json!([])),
                    "requests": serde_json::from_str::<Value>(&r.get::<_, String>(7)?)
                        .unwrap_or(json!([])),
                    "digest_every_secs": r.get::<_, Option<i64>>(8)?}),
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

pub(crate) fn liaison() -> Result<Option<(String, PathBuf)>> {
    Ok(members()?
        .into_iter()
        .find(|(_, home)| crate::door::is_liaison(home)))
}

/// Mail the liaison as the captain, in the liaison's home.
fn tell_liaison(
    id: &str,
    kind: &str,
    body: &str,
    context: Option<&crate::door::scoped::DelegatedContext>,
) -> Result<Option<String>> {
    let Some((ship, home)) = liaison()? else {
        return Ok(None);
    };
    let mut store = Store::open(&home)?;
    let message =
        json!({"id": id, "from": crate::mail::CAPTAIN, "to": ship, "kind": kind, "body": body});
    let sent = match context {
        Some(context) => crate::mail::send_delegated(&store, &message, context)?,
        None => crate::mail::dispatch(&mut store, "mail.send", message)?,
    };
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
pub fn enter(words: &str, until: Option<&str>, every_secs: Option<i64>) -> Result<Value> {
    enter_with(words, until, every_secs, None)
}

pub(crate) fn enter_with(
    words: &str,
    until: Option<&str>,
    every_secs: Option<i64>,
    context: Option<&crate::door::scoped::DelegatedContext>,
) -> Result<Value> {
    let _fence = crate::machines::desk_placement::legacy_writer()?;
    let words = words.trim();
    let conn = registry()?;
    let now = Store::now_ms();
    let current = active()?;
    let provenance = context.map(|c| c.provenance().to_string());
    let since = match &current {
        Some(period) => {
            let since = period["since_ms"].as_i64().unwrap_or(now);
            if words.is_empty() && until.is_none() && every_secs.is_none() {
                return Ok(json!({"away": period, "refreshed": false}));
            }
            conn.execute(
                "UPDATE away SET words=?2, until_text=COALESCE(?3, until_text),
                   digest_every_secs=COALESCE(?4, digest_every_secs), entered_provenance=?5 WHERE started_ms=?1",
                params![since, words, until, every_secs, provenance],
            )?;
            since
        }
        None => {
            conn.execute(
                "INSERT INTO away(started_ms, words, until_text, digest_every_secs, entered_provenance)
                 VALUES(?1, ?2, ?3, ?4, ?5)",
                params![now, words, until, every_secs, provenance],
            )?;
            now
        }
    };
    // The liaison acts for the captain: it learns of the away period, and the
    // words are its standing instruction until the return.
    let mut note = format!(
        "The captain is away from {}{}. Requests and high-urgency messages reach them at once (high urgency at most {BREAKTHROUGHS_PER_HOUR} an hour); other notices go in a periodic digest.",
        clock(since),
        until
            .map(|u| format!(" (back about {u})"))
            .unwrap_or_default()
    );
    if words.is_empty() {
        note.push_str(" They left no instructions; hold what needs them for their return.");
    } else if context.is_some() {
        note.push_str(&format!(
            "\nA delegated client supplied this away note: \"{words}\"\ncaptain.relay may carry this message only under its original grant: ship:message and the target ships must be allowed. Relays never grant approvals or tool permissions. Act only within your charter; hold decisions outside that grant for the captain's return. Production deployment approvals are never relayed."
        ));
    } else {
        note.push_str(&format!(
            "\nTheir instructions while away, in their words: \"{words}\"\nAct on them only within your charter. A decision they answer can be recorded on the asking ship's request with captain.relay, citing this message as the source. Hold everything else that needs the captain for their return; production deployment approvals are never relayed."
        ));
    }
    let source = tell_liaison(&format!("away-{since}-{now}"), "notice", &note, context)?;
    conn.execute(
        "UPDATE away SET source=?2 WHERE started_ms=?1",
        params![since, source],
    )?;
    Ok(json!({"away": active()?, "refreshed": current.is_some()}))
}

/// How a message to the captain travels while the captain is away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// An addition to, or the closure of, a card the captain already has.
    Card,
    /// A request or decision: at once, silently past the ship's hourly share.
    Immediate {
        quiet: bool,
    },
    /// High urgency within the hourly budgets: at once, with a sound.
    Breakthrough,
    /// High urgency over a budget: it leads the next digest, which comes early.
    Overflow,
    Digest,
}

/// Whether captain mail edits a card rather than posting: an attachment to an
/// open ask (never high urgency, which must make a sound) or an ask's closure.
pub fn card_edit(mail: &Value) -> bool {
    let payload = &mail["payload"];
    (payload["attached_to"].is_string() && mail["kind"] != "reply" && mail["urgency"] != "high")
        || (mail["kind"] == "notice" && payload["request_end"].is_object())
}

fn decided<'a>(list: &'a [Value], id: &str) -> Option<&'a Value> {
    list.iter().find(|b| b["id"] == id)
}

/// Route one message held in `ship`'s home while away, recording each budget
/// decision on the away row first, so a restart repeats the same decision.
/// Outside an away period everything is delivered as usual (`None`).
pub fn route(mail: &Value, ship: &str) -> Result<Option<Route>> {
    let _fence = crate::machines::desk_placement::legacy_writer()?;
    let mut conn = registry()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let Some(period) = period_in(&tx, "WHERE returned_ms IS NULL")? else {
        return Ok(None);
    };
    let settings = settings();
    let since = period["since_ms"].as_i64().unwrap_or(0);
    let id = mail["id"].as_str().unwrap_or_default();
    let now = Store::now_ms();
    let list = |key: &str| period[key].as_array().cloned().unwrap_or_default();
    let recent = |list: &[Value], ship: Option<&str>| {
        list.iter()
            .filter(|b| b["ms"].as_i64().unwrap_or(0) > now - HOUR_MS)
            .filter(|b| ship.is_none_or(|s| b["ship"] == s))
            .count()
    };
    let kind = mail["kind"].as_str().unwrap_or_default();
    let route = if card_edit(mail) {
        Route::Card
    } else if matches!(kind, "request" | "decision") {
        let mut requests = list("requests");
        if let Some(seen) = decided(&requests, id) {
            Route::Immediate {
                quiet: seen["quiet"] == true,
            }
        } else {
            // High urgency and production approvals always make a sound.
            let urgent = mail["urgency"] == "high" || mail["request_kind"] == "production_approval";
            let quiet = !urgent && recent(&requests, Some(ship)) >= settings.loud_requests_per_hour;
            requests.push(json!({"ms": now, "id": id, "ship": ship, "quiet": quiet}));
            tx.execute(
                "UPDATE away SET requests=?2 WHERE started_ms=?1",
                params![since, json!(requests).to_string()],
            )?;
            Route::Immediate { quiet }
        }
    } else if mail["urgency"] == "high" && mail["from"] != crate::mail::CREW {
        let mut through = list("breakthroughs");
        let mut overflow = list("overflow");
        if decided(&through, id).is_some() {
            Route::Breakthrough
        } else if decided(&overflow, id).is_some() {
            Route::Overflow
        } else if recent(&through, None) < settings.breakthroughs_per_hour
            && recent(&through, Some(ship)) < settings.ship_breakthroughs_per_hour
        {
            through.push(json!({"ms": now, "id": id, "ship": ship}));
            tx.execute(
                "UPDATE away SET breakthroughs=?2 WHERE started_ms=?1",
                params![since, json!(through).to_string()],
            )?;
            Route::Breakthrough
        } else {
            overflow.push(json!({"ms": now, "id": id, "ship": ship}));
            tx.execute(
                "UPDATE away SET overflow=?2 WHERE started_ms=?1",
                params![since, json!(overflow).to_string()],
            )?;
            Route::Overflow
        }
    } else {
        Route::Digest
    };
    tx.commit()?;
    Ok(Some(route))
}

/// How captain mail is expected to travel while the captain is away, as a
/// note for its sender: `None` when the captain is not away or the mail
/// goes at once. A preview only; the door decides when it carries it.
pub fn preview(mail: &Value) -> Result<Option<(&'static str, String)>> {
    let Some(period) = active()? else {
        return Ok(None);
    };
    let settings = settings();
    let kind = mail["kind"].as_str().unwrap_or_default();
    if card_edit(mail)
        || matches!(kind, "request" | "decision")
        || mail["from"] == crate::mail::CAPTAIN
    {
        return Ok(None);
    }
    let now = Store::now_ms();
    if mail["urgency"] == "high" && mail["from"] != crate::mail::CREW {
        let recent = period["breakthroughs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["ms"].as_i64().unwrap_or(0) > now - HOUR_MS)
            .count();
        if recent < settings.breakthroughs_per_hour {
            return Ok(None);
        }
        return Ok(Some((
            "urgent_digest",
            format!(
                "Urgent messages to the captain are over the hourly limit; this leads the next digest, due within {} minutes. Do not resend.",
                settings.urgent_due_mins
            ),
        )));
    }
    if let Some((id, ms, held)) = crate::digest::last_told(mail, &settings)?
        && mail["id"] != id.as_str()
    {
        let short: String = id.chars().take(8).collect();
        return Ok(Some(if held {
            (
                "repeat",
                format!(
                    "The captain will get this in the digest already (as {short}); it is counted, not shown again. Do not resend."
                ),
            )
        } else {
            (
                "repeat",
                format!(
                    "The captain already has this ({short} at {}); it is counted, not shown again. Do not resend.",
                    clock(ms)
                ),
            )
        }));
    }
    Ok(Some((
        "digest",
        "The captain is away; this goes in the next digest. Do not resend.".to_owned(),
    )))
}

/// Captain mail held during the latest away period, oldest first: every
/// message to the captain in that window that did not break through.
pub fn held() -> Result<Value> {
    held_visible(None)
}

pub(crate) fn held_visible(
    context: Option<&crate::door::scoped::DelegatedContext>,
) -> Result<Value> {
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
        if context.is_some_and(|c| !c.allows(&ship)) {
            continue;
        }
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
            if context.is_some_and(|c| !c.allows(row["from"].as_str().unwrap_or_default())) {
                continue;
            }
            if !through.contains(&row["id"].as_str().unwrap_or_default()) {
                held.push(row);
            }
        }
    }
    held.sort_by_key(|m| m["created_ms"].as_i64().unwrap_or(0));
    held.truncate(HELD_LIMIT);
    let mut period = period;
    if context.is_some() {
        for key in ["breakthroughs", "overflow", "requests", "source"] {
            period.as_object_mut().expect("stored period").remove(key);
        }
    }
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
    back_with(None)
}

pub(crate) fn back_with(
    context: Option<&crate::door::scoped::DelegatedContext>,
) -> Result<Option<Value>> {
    let _fence = crate::machines::desk_placement::legacy_writer()?;
    let tell_liaison = |id: &str, kind: &str, body: &str| tell_liaison(id, kind, body, context);
    let Some(period) = active()? else {
        return Ok(None);
    };
    let since = period["since_ms"].as_i64().unwrap_or(0);
    let now = Store::now_ms();
    registry()?.execute(
        "UPDATE away SET returned_ms=?2, returned_provenance=?3 WHERE started_ms=?1",
        params![since, now, context.map(|c| c.provenance().to_string())],
    )?;
    let held = held()?["held"].as_array().cloned().unwrap_or_default();
    // Held mail is summarized, not replayed: trusted v1 door cursors move
    // past it. Scoped readers must not inherit captain presence side effects.
    let doors = crate::door::trusted_names()?;
    for (_, home) in members()? {
        // A home that cannot be opened keeps its cursor; at worst its held
        // mail is shown again later. The catch-up must still be built.
        let Ok(store) = Store::open(&home) else {
            continue;
        };
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
    // Each door that takes digests gets one catch-up, retried until delivered:
    // what no delivered digest carried, and every request still waiting.
    let settings = settings();
    let waiting_items = crate::captain::waiting()?["waiting"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let digest_doors = crate::door::digest_doors()?;
    let mut catch_up = vec![];
    let mut catch_up_text = None;
    let mut overflowed = false;
    for door in &digest_doors {
        let (row, cut) = crate::digest::catch_up_with(
            door,
            &period,
            &digest,
            &waiting_items,
            &settings,
            context,
        )?;
        overflowed |= cut;
        catch_up_text.get_or_insert_with(|| {
            row["lines"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        });
        catch_up.push(row["id"].clone());
    }
    if catch_up_text.is_none() && !digest_doors.is_empty() {
        catch_up_text = Some(digest.clone());
    }
    let mut summary = None;
    if !digest_doors.is_empty() {
        // The catch-up already lists what was held and what waits; the
        // liaison's summary is worth its turn only when the catch-up was cut.
        if overflowed {
            let body = format!(
                "I'm back after {} away and my catch-up was too long to read. Please summarize for me, in plain words, what reached my mailbox while I was away: first what needs me, then what finished, then what is still in progress (/held lists it). Then answer anything I write next.",
                duration(now - since)
            );
            summary = tell_liaison(&format!("away-{since}-summary"), "request", &body)?;
        }
    } else if held.is_empty() && waiting > 0 {
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
        "breakthroughs": sent, "summary_requested": summary, "since_ms": since, "returned_ms": now,
        "catch_up": catch_up, "catch_up_text": catch_up_text}),
    ))
}

/// One line for every ship's bearings while the captain is away.
pub fn bearings_line() -> Option<String> {
    let period = active().ok().flatten()?;
    Some(format!(
        "Captain away since {}{}: requests and high urgency reach them at once; notices go in a digest. Tell each outcome once. Record waits and keep working rather than stopping on a question.",
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

/// What `/afk` was given: when the captain expects to be back, how often
/// they want a digest, and their words for the liaison.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Afk {
    pub until: Option<String>,
    pub every_secs: Option<i64>,
    pub words: String,
}

/// A digest period such as `30m`, `2h` or `45`, kept between 10 minutes and 6 hours.
fn every(text: &str) -> Option<i64> {
    let (number, unit) = match text.strip_suffix('h') {
        Some(n) => (n, 3600),
        None => (text.strip_suffix('m').unwrap_or(text), 60),
    };
    let secs = number.parse::<i64>().ok()?.checked_mul(unit)?;
    Some(secs.clamp(600, 6 * 3600))
}

/// Split `[until 8am] [every 30m] rest of words`, in either order.
pub fn parse(text: &str) -> Afk {
    let mut afk = Afk::default();
    let mut rest = text.trim();
    loop {
        let mut parts = rest.splitn(3, char::is_whitespace);
        let (head, value) = (parts.next().unwrap_or(""), parts.next());
        let tail = parts.next().unwrap_or("").trim_start();
        match (head, value) {
            ("until", Some(value)) if afk.until.is_none() && !value.is_empty() => {
                afk.until = Some(value.to_owned())
            }
            ("every", Some(value)) if afk.every_secs.is_none() && every(value).is_some() => {
                afk.every_secs = every(value)
            }
            _ => break,
        }
        rest = tail;
    }
    afk.words = rest.trim().to_owned();
    afk
}

/// `privateer afk [until TIME] [WORDS]`, `privateer back`, `privateer away`.
pub fn cli(command: &str, args: &[String]) -> Result<()> {
    match command {
        "afk" => {
            let afk = parse(&args.join(" "));
            let entered = enter(&afk.words, afk.until.as_deref(), afk.every_secs)?;
            println!(
                "Away{}: requests and high-urgency messages reach you at once (high urgency at most {BREAKTHROUGHS_PER_HOUR} an hour); other notices come in a digest.",
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
            Some(returned) => println!(
                "{}",
                returned["catch_up_text"]
                    .as_str()
                    .or(returned["digest"].as_str())
                    .unwrap_or("")
            ),
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
