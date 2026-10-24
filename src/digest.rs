//! Away digests: the captain's held notices, built from records into one
//! message per period, and the catch-up on return.
//!
//! Each digest is a row in the fleet registry, built once and re-sent
//! verbatim until its door acknowledges it. A door's digest cursor in each
//! home is derived from those rows only: the largest `through` of its
//! `baseline` and `delivered` rows for the away period. Nothing is written to
//! a home when a digest is acknowledged, so one unreadable home cannot hold
//! every later digest back.
//!
//! Repeats are recognised here too: a notice whose words equal the last thing
//! told to the captain on its key (sender plus task or correlation).
use crate::away::Settings;
use crate::store::Store;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

const HOUR_MS: i64 = 60 * 60 * 1000;
/// Captain mail read from one home for one digest.
const READ_LIMIT: i64 = 2000;

fn registry() -> Result<rusqlite::Connection> {
    let conn = crate::fleet::registry()?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS away_digest(id TEXT PRIMARY KEY, door TEXT NOT NULL,
           since_ms INTEGER NOT NULL, kind TEXT NOT NULL, built_ms INTEGER NOT NULL,
           through TEXT NOT NULL DEFAULT '{}', lines TEXT NOT NULL DEFAULT '[]',
           loud INTEGER NOT NULL DEFAULT 0, state TEXT NOT NULL, delivered_ms INTEGER);
         CREATE INDEX IF NOT EXISTS away_digest_door ON away_digest(door, since_ms);
         CREATE TABLE IF NOT EXISTS captain_told(key TEXT PRIMARY KEY, text TEXT NOT NULL,
           id TEXT NOT NULL, ms INTEGER NOT NULL, repeat INTEGER NOT NULL, held INTEGER NOT NULL);",
    )?;
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('away_digest') WHERE name='provenance')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        if let Err(error) = conn.execute_batch("ALTER TABLE away_digest ADD COLUMN provenance TEXT")
        {
            if !error.to_string().contains("duplicate column name") {
                return Err(error.into());
            }
        }
    }
    Ok(conn)
}

fn members() -> Result<Vec<(String, PathBuf)>> {
    Ok(crate::fleet::members(&crate::fleet::registry()?)?
        .into_iter()
        .filter(|m| m.home.is_dir())
        .map(|m| (m.name, m.home))
        .collect())
}

/// Record, once per door and away period, where the door's delivery stood
/// in each home when the period began: the digests start after it.
pub fn baseline(door: &str, since: i64, through: &BTreeMap<String, i64>) -> Result<()> {
    registry()?.execute(
        "INSERT OR IGNORE INTO away_digest(id, door, since_ms, kind, built_ms, through, state)
         VALUES(?1, ?2, ?3, 'baseline', ?4, ?5, 'baseline')",
        params![
            format!("baseline-{door}-{since}"),
            door,
            since,
            Store::now_ms(),
            json!(through).to_string()
        ],
    )?;
    Ok(())
}

pub fn has_baseline(door: &str, since: i64) -> Result<bool> {
    Ok(registry()?
        .query_row(
            "SELECT 1 FROM away_digest WHERE id=?1",
            [format!("baseline-{door}-{since}")],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// The door's digest cursor per home for an away period.
fn cursors(conn: &rusqlite::Connection, door: &str, since: i64) -> Result<HashMap<String, i64>> {
    let mut statement = conn.prepare(
        "SELECT through FROM away_digest WHERE door=?1 AND since_ms=?2
           AND state IN ('baseline', 'delivered')",
    )?;
    let mut cursors: HashMap<String, i64> = HashMap::new();
    for through in statement.query_map(params![door, since], |r| r.get::<_, String>(0))? {
        let through: HashMap<String, i64> = serde_json::from_str(&through?).unwrap_or_default();
        for (ship, seq) in through {
            let cursor = cursors.entry(ship).or_insert(seq);
            *cursor = (*cursor).max(seq);
        }
    }
    Ok(cursors)
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    Ok(
        json!({"id": r.get::<_, String>(0)?, "door": r.get::<_, String>(1)?,
        "since_ms": r.get::<_, i64>(2)?, "kind": r.get::<_, String>(3)?,
        "built_ms": r.get::<_, i64>(4)?,
        "lines": serde_json::from_str::<Value>(&r.get::<_, String>(5)?).unwrap_or(json!([])),
        "loud": r.get::<_, bool>(6)?, "state": r.get::<_, String>(7)?,
        "provenance": r.get::<_, Option<String>>(8)?.and_then(|s| serde_json::from_str::<Value>(&s).ok())}),
    )
}

/// Digests and catch-ups this door has not yet acknowledged, oldest first.
pub fn pending(door: &str) -> Result<Vec<Value>> {
    let conn = registry()?;
    let mut statement = conn.prepare(
        "SELECT id, door, since_ms, kind, built_ms, lines, loud, state, provenance FROM away_digest
         WHERE door=?1 AND state='pending' ORDER BY built_ms, id",
    )?;
    Ok(statement
        .query_map([door], row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// The door has shown a digest to the captain.
pub fn delivered(door: &str, id: &str) -> Result<()> {
    registry()?.execute(
        "UPDATE away_digest SET state='delivered', delivered_ms=?3
         WHERE id=?1 AND door=?2 AND state='pending'",
        params![id, door, Store::now_ms()],
    )?;
    Ok(())
}

/// When the door's next digest of this away period is due.
pub fn due_ms(door: &str, period: &Value, settings: &Settings) -> Result<i64> {
    let since = period["since_ms"].as_i64().unwrap_or(0);
    let last: i64 = registry()?.query_row(
        "SELECT COALESCE(MAX(built_ms), ?3) FROM away_digest
         WHERE door=?1 AND since_ms=?2 AND kind='digest'",
        params![door, since, since],
        |r| r.get(0),
    )?;
    let every = period["digest_every_secs"]
        .as_i64()
        .map(|s| s * 1000)
        .unwrap_or((settings.digest_every_mins * 60_000.0) as i64);
    let urgent = period["overflow"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| o["ms"].as_i64())
        .filter(|ms| *ms >= last)
        .min()
        .map(|first| first + (settings.urgent_due_mins * 60_000.0) as i64);
    Ok(urgent.map_or(last + every, |u| u.min(last + every)))
}

/// Normalized words for repeat matching: lowercase, whitespace collapsed,
/// trailing punctuation dropped; digits kept.
pub fn normalize(body: &str) -> String {
    body.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .trim_end_matches(|c: char| c.is_ascii_punctuation())
        .to_owned()
}

fn key(mail: &Value) -> String {
    let topic = mail["task_id"]
        .as_str()
        .or(mail["correlation"].as_str())
        .unwrap_or("");
    format!("{}|{topic}", mail["from"].as_str().unwrap_or(""))
}

/// Whether a notice repeats the last thing told to the captain on its key,
/// remembering it as told either way. Requests, decisions and high urgency
/// are never repeats. The answer for one message id does not change, so
/// every door agrees and a restart repeats it.
pub fn told(mail: &Value, held: bool, settings: &Settings) -> Result<bool> {
    if !matches!(mail["kind"].as_str(), Some("notice" | "reply")) || mail["urgency"] == "high" {
        return Ok(false);
    }
    let id = mail["id"].as_str().unwrap_or_default();
    let text = normalize(mail["body"].as_str().unwrap_or(""));
    let now = Store::now_ms();
    let window = (settings.repeat_hours * HOUR_MS as f64) as i64;
    let mut conn = registry()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute("DELETE FROM captain_told WHERE ms < ?1", [now - window])?;
    let mut keys = vec![key(mail)];
    if text.chars().count() >= settings.cross_ship_chars {
        keys.push(format!("*|{text}"));
    }
    let mut repeat = false;
    for key in &keys {
        let last: Option<(String, String, bool)> = tx
            .query_row(
                "SELECT text, id, repeat FROM captain_told WHERE key=?1",
                [key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        match last {
            Some((_, last_id, was)) if last_id == id => {
                tx.commit()?;
                return Ok(was);
            }
            Some((last_text, _, _)) if last_text == text => repeat = true,
            _ => {}
        }
    }
    for key in &keys {
        tx.execute(
            "INSERT INTO captain_told(key, text, id, ms, repeat, held) VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(key) DO UPDATE SET text=excluded.text, id=excluded.id, ms=excluded.ms,
               repeat=excluded.repeat, held=excluded.held",
            params![key, text, id, now, repeat, held],
        )?;
    }
    tx.commit()?;
    Ok(repeat)
}

/// What the captain was last told on this message's key, if the words are
/// the same: (that message's id, when, whether it went to a digest).
pub fn last_told(mail: &Value, settings: &Settings) -> Result<Option<(String, i64, bool)>> {
    let text = normalize(mail["body"].as_str().unwrap_or(""));
    let since = Store::now_ms() - (settings.repeat_hours * HOUR_MS as f64) as i64;
    let conn = registry()?;
    let mut keys = vec![key(mail)];
    if text.chars().count() >= settings.cross_ship_chars {
        keys.push(format!("*|{text}"));
    }
    for key in keys {
        let found = conn
            .query_row(
                "SELECT id, ms, held FROM captain_told WHERE key=?1 AND text=?2 AND ms >= ?3",
                params![key, text, since],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if found.is_some() {
            return Ok(found);
        }
    }
    Ok(None)
}

struct Held {
    ship: String,
    mail: Value,
}

/// Held captain mail for the door's next digest: everything after its digest
/// cursor in each readable home that did not already go to the captain at
/// once. Returns the items and the new cursor per home.
fn collect(door: &str, period: &Value) -> Result<(Vec<Held>, BTreeMap<String, i64>)> {
    collect_visible(door, period, None)
}

fn collect_visible(
    door: &str,
    period: &Value,
    context: Option<&crate::door::scoped::DelegatedContext>,
) -> Result<(Vec<Held>, BTreeMap<String, i64>)> {
    let since = period["since_ms"].as_i64().unwrap_or(0);
    let cursors = cursors(&registry()?, door, since)?;
    let sent: HashSet<&str> = period["breakthroughs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| b["id"].as_str())
        .collect();
    let mut items = vec![];
    let mut through = BTreeMap::new();
    let mut seen = HashSet::new();
    for (ship, home) in members()? {
        if context.is_some_and(|c| !c.allows(&ship)) {
            continue;
        }
        // One unreadable home must not stop the digest: its cursor stays put.
        let Ok(store) = Store::open(&home) else {
            continue;
        };
        let (after, floor) = match cursors.get(&ship) {
            Some(cursor) => (*cursor, 0),
            None => (0, since),
        };
        let read = (|| -> Result<Vec<Value>> {
            let mut statement = store.connection().prepare(
                "SELECT seq, id, sender, kind, urgency, body, created_ms, task_id, correlation,
                   payload FROM mail WHERE recipient=?1 AND seq > ?2 AND created_ms >= ?3
                 ORDER BY seq LIMIT ?4",
            )?;
            let rows = statement
                .query_map(
                    params![crate::mail::CAPTAIN, after, floor, READ_LIMIT],
                    |r| {
                        Ok(
                            json!({"seq": r.get::<_, i64>(0)?, "id": r.get::<_, String>(1)?,
                        "from": r.get::<_, String>(2)?, "kind": r.get::<_, String>(3)?,
                        "urgency": r.get::<_, String>(4)?, "body": r.get::<_, String>(5)?,
                        "created_ms": r.get::<_, i64>(6)?,
                        "task_id": r.get::<_, Option<String>>(7)?,
                        "correlation": r.get::<_, Option<String>>(8)?,
                        "payload": serde_json::from_str::<Value>(&r.get::<_, String>(9)?)
                            .unwrap_or(json!({}))}),
                        )
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })();
        let Ok(rows) = read else { continue };
        for mail in rows {
            if context.is_some_and(|c| !c.allows(mail["from"].as_str().unwrap_or_default())) {
                continue;
            }
            through.insert(ship.clone(), mail["seq"].as_i64().unwrap_or(0));
            let id = mail["id"].as_str().unwrap_or_default().to_owned();
            // A desk copy of one message in two homes is one item.
            if !seen.insert(id.clone()) {
                continue;
            }
            let immediate = matches!(mail["kind"].as_str(), Some("request" | "decision"))
                || crate::away::card_edit(&mail)
                || sent.contains(id.as_str());
            if !immediate {
                items.push(Held {
                    ship: ship.clone(),
                    mail,
                });
            }
        }
    }
    items.sort_by_key(|h| h.mail["created_ms"].as_i64().unwrap_or(0));
    Ok((items, through))
}

fn line(item: &Held, text: &str, settings: &Settings) -> String {
    let mail = &item.mail;
    let words: String = text.chars().take(settings.line_chars).collect();
    let cut = if text.chars().count() > settings.line_chars {
        "…"
    } else {
        ""
    };
    format!(
        "• {} {} {}: {words}{cut}",
        mail["from"].as_str().unwrap_or(&item.ship),
        mail["id"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(8)
            .collect::<String>(),
        crate::away::clock(mail["created_ms"].as_i64().unwrap_or(0)),
    )
}

/// Lines for held items: the urgent ones first, then answers, then updates
/// collapsed per sender and topic, then a count of routine mail. Returns
/// the lines, whether there was anything urgent, and whether lines were cut.
fn render(items: &[Held], period: &Value, settings: &Settings) -> (Vec<String>, bool, bool) {
    let overflow: HashSet<&str> = period["overflow"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| o["id"].as_str())
        .collect();
    let mut urgent = vec![];
    let mut answers: Vec<Vec<&Held>> = vec![];
    let mut updates: Vec<Vec<&Held>> = vec![];
    let mut routine = 0;
    let mut routine_ships = BTreeSet::new();
    for item in items {
        let mail = &item.mail;
        if mail["urgency"] == "high" && mail["from"] != crate::mail::CREW
            || overflow.contains(mail["id"].as_str().unwrap_or(""))
        {
            urgent.push(item);
        } else if mail["urgency"] == "low" || mail["from"] == crate::mail::CREW {
            routine += 1;
            routine_ships.insert(item.ship.as_str());
        } else {
            let groups = if mail["kind"] == "reply" {
                &mut answers
            } else {
                &mut updates
            };
            // Mail without a task or correlation collapses only with the
            // same words from the same sender.
            let topic = |m: &Value| {
                m["task_id"]
                    .as_str()
                    .or(m["correlation"].as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("\0{}", normalize(m["body"].as_str().unwrap_or(""))))
            };
            let found = groups.iter().position(|g| {
                g[0].mail["from"] == mail["from"] && topic(&g[0].mail) == topic(mail)
            });
            match found {
                Some(index) => groups[index].push(item),
                None => groups.push(vec![item]),
            }
        }
    }
    let mut lines = vec![];
    let mut budget = settings.digest_lines;
    let mut more = 0;
    let mut push = |lines: &mut Vec<String>, text: String| {
        if budget == 0 {
            more += 1;
        } else {
            budget -= 1;
            lines.push(text);
        }
    };
    if !urgent.is_empty() {
        lines.push("🚨 Urgent, over the hourly limit".to_owned());
        for item in &urgent {
            let text = item.mail["body"].as_str().unwrap_or("");
            push(
                &mut lines,
                line(
                    item,
                    &text.split_whitespace().collect::<Vec<_>>().join(" "),
                    settings,
                ),
            );
        }
    }
    for (title, groups) in [("Answers to you", &answers), ("Updates", &updates)] {
        if groups.is_empty() {
            continue;
        }
        lines.push(title.to_owned());
        for group in groups {
            let latest = group[group.len() - 1];
            let text = latest.mail["body"]
                .as_str()
                .unwrap_or("")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let same = group
                .iter()
                .all(|h| normalize(h.mail["body"].as_str().unwrap_or("")) == normalize(&text));
            let shown = match group.len() {
                1 => text,
                n if same => format!("{text} ×{n}"),
                n => format!("{n} updates, latest: {text}"),
            };
            push(&mut lines, line(latest, &shown, settings));
        }
    }
    let cut = more > 0;
    if cut {
        lines.push(format!("…and {more} more: /held"));
    }
    if routine > 0 {
        lines.push(format!(
            "Routine: {routine} low-urgency update{} from {} ship{}.",
            if routine == 1 { "" } else { "s" },
            routine_ships.len(),
            if routine_ships.len() == 1 { "" } else { "s" },
        ));
    }
    (lines, !urgent.is_empty(), cut)
}

fn insert(row: &Value, through: &BTreeMap<String, i64>) -> Result<()> {
    registry()?.execute(
        "INSERT OR IGNORE INTO away_digest(id, door, since_ms, kind, built_ms, through, lines, loud, state, provenance)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9)",
        params![
            row["id"].as_str(),
            row["door"].as_str(),
            row["since_ms"].as_i64(),
            row["kind"].as_str(),
            row["built_ms"].as_i64(),
            json!(through).to_string(),
            row["lines"].to_string(),
            row["loud"].as_bool().unwrap_or(false),
            row.get("provenance").map(Value::to_string)
        ],
    )?;
    Ok(())
}

/// Build the door's next digest for the away period now, unless one is
/// still pending or nothing is held. The row is stored pending and returned.
pub fn build(door: &str, period: &Value, settings: &Settings) -> Result<Option<Value>> {
    build_visible(door, period, settings, None)
}

fn build_visible(
    door: &str,
    period: &Value,
    settings: &Settings,
    context: Option<&crate::door::scoped::DelegatedContext>,
) -> Result<Option<Value>> {
    let since = period["since_ms"].as_i64().unwrap_or(0);
    let conn = registry()?;
    let (pending, built): (i64, i64) = conn.query_row(
        "SELECT COALESCE(SUM(state='pending'), 0), COALESCE(MAX(built_ms), ?3) FROM away_digest
         WHERE door=?1 AND since_ms=?2 AND kind='digest'",
        params![door, since, since],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if pending > 0 {
        return Ok(None);
    }
    let (items, through) = collect_visible(door, period, context)?;
    if items.is_empty() {
        return Ok(None);
    }
    let (body, urgent, _) = render(&items, period, settings);
    let now = Store::now_ms();
    let ships: BTreeSet<&str> = items.iter().map(|h| h.ship.as_str()).collect();
    let mut lines = vec![format!(
        "📋 Away digest {}–{} · {} message{} from {} ship{}",
        crate::away::clock(built),
        crate::away::clock(now),
        items.len(),
        if items.len() == 1 { "" } else { "s" },
        ships.len(),
        if ships.len() == 1 { "" } else { "s" },
    )];
    lines.extend(body);
    lines.push("/held for all of it · /dismiss ID closes one".to_owned());
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM away_digest WHERE door=?1 AND since_ms=?2 AND kind='digest'",
        params![door, since],
        |r| r.get(0),
    )?;
    let mut row = json!({"id": format!("digest-{door}-{since}-{}", n + 1), "door": door,
        "since_ms": since, "kind": "digest", "built_ms": now, "lines": lines,
        "loud": urgent || !settings.digest_quiet, "state": "pending"});
    if let Some(context) = context {
        row["provenance"] = context.provenance();
    }
    insert(&row, &through)?;
    Ok(Some(row))
}

/// The catch-up for one door on the captain's return: the welcome line,
/// everything held and not yet in a delivered digest, and every request
/// still waiting, by line. Pending digests of the period are superseded by
/// it. Returns the row and whether its lines overflowed.
pub fn catch_up(
    door: &str,
    period: &Value,
    welcome: &str,
    waiting: &[Value],
    settings: &Settings,
) -> Result<(Value, bool)> {
    catch_up_with(door, period, welcome, waiting, settings, None)
}

pub(crate) fn catch_up_with(
    door: &str,
    period: &Value,
    welcome: &str,
    waiting: &[Value],
    settings: &Settings,
    context: Option<&crate::door::scoped::DelegatedContext>,
) -> Result<(Value, bool)> {
    let since = period["since_ms"].as_i64().unwrap_or(0);
    registry()?.execute(
        "UPDATE away_digest SET state='superseded' WHERE door=?1 AND since_ms=?2 AND state='pending'",
        params![door, since],
    )?;
    let (items, through) = collect(door, period)?;
    let (body, _, cut) = render(&items, period, settings);
    let mut lines = vec![format!("🌅 {welcome}")];
    lines.extend(body);
    if !waiting.is_empty() {
        lines.push(format!("Waiting on you ({}):", waiting.len()));
        for (index, item) in waiting.iter().enumerate() {
            if index == settings.digest_lines {
                lines.push(format!(
                    "…and {} more: /waiting",
                    waiting.len() - settings.digest_lines
                ));
                break;
            }
            let mail = &item["mail"];
            let held = Held {
                ship: item["ship"].as_str().unwrap_or("").to_owned(),
                mail: mail.clone(),
            };
            let text = mail["body"]
                .as_str()
                .unwrap_or("")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            lines.push(line(&held, &text, settings));
        }
    }
    let mut row = json!({"id": format!("catchup-{door}-{since}"), "door": door, "since_ms": since,
        "kind": "catch_up", "built_ms": Store::now_ms(), "lines": lines,
        "loud": true, "state": "pending"});
    if let Some(context) = context {
        row["provenance"] = context.provenance();
    }
    insert(&row, &through)?;
    Ok((row, cut || waiting.len() > settings.digest_lines))
}

/// Explicit, grant-owned digest generation. No v1 delivery cursor or legacy
/// acknowledgment is used. Pending output is retried unchanged; reader/ack
/// lifecycle belongs to phase 2.4.
pub(crate) fn delegated(context: &crate::door::scoped::DelegatedContext) -> Result<Value> {
    let Some(period) = crate::away::active()? else {
        return Ok(json!({"away":false,"digest":null}));
    };
    // '/' is rejected by identifier(), unlike ':'. Ignore old client: rows:
    // their ownership cannot be distinguished from a real v1 door's stream.
    let reader = format!("client/{}", context.client());
    let since = period["since_ms"].as_i64().unwrap_or(0);
    if let Some(row) = pending(&reader)?
        .into_iter()
        .find(|r| r["since_ms"] == since)
    {
        return Ok(json!({"away":true,"digest":row}));
    }
    let digest = build_visible(&reader, &period, &crate::away::settings(), Some(context))?;
    // Bound delegated digest history without evicting any v1 stream.
    registry()?.execute("DELETE FROM away_digest WHERE door=?1 AND id NOT IN (SELECT id FROM away_digest WHERE door=?1 ORDER BY built_ms DESC,id DESC LIMIT 128)", [&reader])?;
    Ok(json!({"away":true,"digest":digest}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizing_keeps_digits_and_drops_trailing_punctuation() {
        assert_eq!(normalize("  CI   green. "), "ci green");
        assert_ne!(normalize("3 tests failing"), normalize("4 tests failing"));
    }
}
