//! `captain.feed`: the captain's mail as one time-ordered history across every
//! fleet home on this machine, for the captain's own portal.
//!
//! Each home keeps its own mailbox, so the feed is a bounded merge: every home
//! is asked only for the page after (or before) one cursor, and the merged page
//! is cut where the slowest home stopped, so no entry is skipped or repeated
//! across pages. A cursor is the composite (created_ms, home, seq): the same
//! order in every home, and it stays valid as homes are added or retired.
//!
//! A message held in two homes (a delivered copy, a desk copy) is one entry.
//! The copy shown is an open one if any copy is open; among those, the
//! original (in the home of the ship that sent it, or that the captain wrote
//! to), else the first by home name. It is decided from every copy, never from
//! the page at hand, so a message cannot appear once per page.
use crate::store::{Caller, Store, identifier};
use anyhow::{Context, Result, ensure};
use rusqlite::params;
use serde_json::{Value, json};

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;
/// How many rows one home may hand the merge per call, as a multiple of the
/// page: copies held elsewhere are skipped, and the skipping must stay bounded.
const SCAN_PAGES: usize = 4;
/// States an entry can be filtered by: the stored request and notice states.
const STATES: &[&str] = &[
    "open",
    "answered",
    "dismissed",
    "withdrawn",
    "acknowledged",
    "approved",
    "denied",
    "task_done",
    "task_cancelled",
];
const CURSOR_PREFIX: &str = "f1.";

/// A position in the feed. Home names are identifiers, so `""` sorts before
/// every home and `"~"` after every one; a time bound uses them.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    ms: i64,
    home: String,
    seq: i64,
}

impl Key {
    fn encode(&self) -> String {
        format!("{CURSOR_PREFIX}{}.{}.{}", self.ms, self.seq, self.home)
    }

    /// A cursor this command returned, or a plain millisecond time bound.
    fn parse(value: &Value, forward: bool) -> Result<Key> {
        if let Some(ms) = value.as_i64() {
            let home = if forward { "~" } else { "" };
            return Ok(Key {
                ms,
                home: home.into(),
                seq: 0,
            });
        }
        let text = value
            .as_str()
            .context("a cursor is a string this command returned, or a time in ms")?;
        let parsed = text.strip_prefix(CURSOR_PREFIX).and_then(|rest| {
            let mut parts = rest.splitn(3, '.');
            let ms = parts.next()?.parse().ok()?;
            let seq = parts.next()?.parse().ok()?;
            let home = parts.next()?;
            identifier(home).ok()?;
            Some(Key {
                ms,
                home: home.into(),
                seq,
            })
        });
        parsed.with_context(|| format!("{text} is not a feed cursor"))
    }

    /// The seq to compare `(created_ms, seq)` against in `home`, so that one
    /// row-value comparison per home orders rows exactly as `Key` does.
    fn seq_bound(&self, home: &str) -> i64 {
        match home.cmp(self.home.as_str()) {
            std::cmp::Ordering::Equal => self.seq,
            // Rows at the key's ms lie before it in an earlier home, after it in a later one.
            std::cmp::Ordering::Less => i64::MAX,
            std::cmp::Ordering::Greater => i64::MIN,
        }
    }
}

struct Home {
    ship: String,
    store: Store,
}

struct Query {
    after: Option<Key>,
    before: Option<Key>,
    forward: bool,
    want: usize,
    scope: String,
    ship: Option<String>,
}

/// Messages in feed order, each with its position.
type Rows = Vec<(Key, Value)>;

/// One home's next rows in feed order, keeping only the copies this feed
/// shows. Returns them and, unless the home ran out, the last key it read.
fn scan(homes: &[Home], index: usize, query: &Query) -> Result<(Rows, Option<Key>)> {
    let home = &homes[index];
    let order = if query.forward { "ASC" } else { "DESC" };
    let sql = format!(
        "SELECT {} FROM mail WHERE ({})
           AND (created_ms, seq) > (?1, ?2) AND (created_ms, seq) < (?3, ?4)
         ORDER BY created_ms {order}, seq {order} LIMIT ?5",
        crate::mail::COLUMNS,
        query.scope
    );
    // Open ends are the extremes, never NULL, so SQLite seeks the time index.
    let mut after = query
        .after
        .as_ref()
        .map_or((i64::MIN, i64::MIN), |k| (k.ms, k.seq_bound(&home.ship)));
    let mut before = query
        .before
        .as_ref()
        .map_or((i64::MAX, i64::MAX), |k| (k.ms, k.seq_bound(&home.ship)));
    let mut kept = vec![];
    let mut scanned = 0;
    let budget = query.want * SCAN_PAGES;
    let mut statement = home.store.connection().prepare(&sql)?;
    loop {
        let batch = (query.want - kept.len()).min(budget - scanned).max(1);
        let rows = statement
            .query_map(
                params![
                    after.0,
                    after.1,
                    before.0,
                    before.1,
                    batch as i64,
                    query.ship,
                ],
                crate::mail::message,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let exhausted = rows.len() < batch;
        let mut last = None;
        for row in rows {
            scanned += 1;
            let key = Key {
                ms: row["created_ms"].as_i64().unwrap_or(0),
                home: home.ship.clone(),
                seq: row["seq"].as_i64().unwrap_or(0),
            };
            if query.forward {
                after = (key.ms, key.seq);
            } else {
                before = (key.ms, key.seq);
            }
            if shown_here(homes, index, &row)? {
                kept.push((key.clone(), row));
            }
            last = Some(key);
        }
        if exhausted {
            return Ok((kept, None));
        }
        if kept.len() >= query.want || scanned >= budget {
            return Ok((kept, last));
        }
    }
}

/// Whether `homes[index]` holds the copy of this message the feed shows.
fn shown_here(homes: &[Home], index: usize, row: &Value) -> Result<bool> {
    let id = row["id"].as_str().context("message id")?;
    // Senders choose their own ids, so a copy is the same id between the
    // same two mailboxes; an unrelated message that shares an id is not one.
    let from = row["from"].as_str().context("message sender")?;
    let to = row["to"].as_str().context("message recipient")?;
    let mut open = vec![];
    let mut holders = vec![];
    for (i, home) in homes.iter().enumerate() {
        let state: Option<Option<String>> = if i == index {
            Some(row["state"].as_str().map(str::to_owned))
        } else {
            rusqlite::OptionalExtension::optional(home.store.connection().query_row(
                "SELECT state FROM mail WHERE id=?1 AND sender=?2 AND recipient=?3",
                [id, from, to],
                |r| r.get(0),
            ))?
        };
        if let Some(state) = state {
            holders.push(i);
            if state.as_deref() == Some("open") {
                open.push(i);
            }
        }
    }
    if holders.len() == 1 {
        return Ok(true);
    }
    // The original is in the ship's own home: the sender's, or for the
    // captain's own mail, the recipient's.
    let origin = if row["from"] == crate::mail::CAPTAIN {
        &row["to"]
    } else {
        &row["from"]
    };
    // Open copies first; among equals, the original, then the first home.
    let candidates = if open.is_empty() { &holders } else { &open };
    let chosen = candidates
        .iter()
        .find(|&&i| homes[i].ship == *origin)
        .or(candidates.first())
        .copied();
    Ok(chosen == Some(index))
}

/// The entry the portal reads for one stored message.
fn entry(home: &str, key: &Key, mut mail: Value) -> Value {
    let ship = if mail["from"] == crate::mail::CAPTAIN {
        mail["to"].clone()
    } else {
        mail["from"].clone()
    };
    let mut out = json!({"ship": ship, "home": home, "cursor": key.encode()});
    for field in [
        "id",
        "from",
        "to",
        "kind",
        "request_kind",
        "urgency",
        "created_ms",
        "body",
        "payload",
        "correlation",
        "task_id",
        "state",
        "end_reason",
        "answered_by",
        "answered_ms",
        "ended_by",
        "ended_ms",
        "deadline_ms",
    ] {
        out[field] = mail[field].take();
    }
    out
}

fn states(args: &Value) -> Result<Option<Vec<&'static str>>> {
    let open_only = match args.get("open_only").filter(|v| !v.is_null()) {
        Some(value) => value.as_bool().context("open_only must be true or false")?,
        None => false,
    };
    let Some(given) = args.get("states").filter(|v| !v.is_null()) else {
        return Ok(open_only.then(|| vec!["open"]));
    };
    ensure!(!open_only, "give states or open_only, not both");
    let given = given.as_array().context("states must be a list")?;
    ensure!(!given.is_empty(), "states must name at least one state");
    given
        .iter()
        .map(|state| {
            let state = state.as_str().context("each state is a string")?;
            STATES
                .iter()
                .find(|s| **s == state)
                .copied()
                .with_context(|| format!("unknown state {state}; one of {}", STATES.join(", ")))
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

/// The captain's mail across the fleet's homes, a page at a time.
pub fn feed(args: &Value) -> Result<Value> {
    let limit = match args.get("limit").filter(|v| !v.is_null()) {
        Some(value) => value.as_i64().context("limit must be a whole number")?,
        None => DEFAULT_LIMIT,
    }
    .clamp(1, MAX_LIMIT) as usize;
    let ship = match args.get("ship").filter(|v| !v.is_null()) {
        Some(value) => {
            let ship = value.as_str().context("ship must be a name")?;
            identifier(ship)?;
            Some(ship.to_owned())
        }
        None => None,
    };
    let ship_mail = match args.get("include_ship_mail").filter(|v| !v.is_null()) {
        Some(value) => value
            .as_bool()
            .context("include_ship_mail must be true or false")?,
        None => false,
    };
    let after = match args.get("after").filter(|v| !v.is_null()) {
        Some(value) => Some(Key::parse(value, true)?),
        None => None,
    };
    let before = match args.get("before").filter(|v| !v.is_null()) {
        Some(value) => Some(Key::parse(value, false)?),
        None => None,
    };
    if let (Some(after), Some(before)) = (&after, &before) {
        ensure!(after < before, "after must come before before");
    }
    // Pages run forward from `after` (or the start); `before` alone pages back.
    let forward = before.is_none() || after.is_some();

    // ?6 is the ship, or NULL for every ship.
    let captain = format!("'{}'", crate::mail::CAPTAIN);
    let mut scope = if ship_mail {
        "?6 IS NULL OR sender=?6 OR recipient=?6".to_owned()
    } else {
        format!(
            "(sender={captain} AND (?6 IS NULL OR recipient=?6))
             OR (recipient={captain} AND (?6 IS NULL OR sender=?6))"
        )
    };
    if let Some(states) = states(args)? {
        // Each state is one of STATES, never the caller's text.
        let list: Vec<String> = states.iter().map(|s| format!("'{s}'")).collect();
        scope = format!("({scope}) AND state IN ({})", list.join(","));
    }
    let query = Query {
        after,
        before,
        forward,
        want: limit + 1,
        scope,
        ship,
    };

    let mut homes = crate::captain::fleet()?
        .into_iter()
        .map(|(ship, home)| {
            Ok(Home {
                ship,
                store: Store::open_readonly(&home, Caller::Operator)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    homes.sort_by(|a, b| a.ship.cmp(&b.ship));

    let mut found: Vec<(Key, Value)> = vec![];
    // Where the merge must stop: the nearest point a home stopped reading.
    let mut stop: Option<Key> = None;
    for index in 0..homes.len() {
        let (rows, last) = scan(&homes, index, &query)?;
        found.extend(rows);
        if let Some(last) = last {
            stop = Some(match stop {
                Some(stop) if (stop < last) == query.forward => stop,
                _ => last,
            });
        }
    }
    if query.forward {
        found.sort_by(|a, b| a.0.cmp(&b.0));
    } else {
        found.sort_by(|a, b| b.0.cmp(&a.0));
    }
    if let Some(stop) = &stop {
        found.retain(|(key, _)| {
            if query.forward {
                key <= stop
            } else {
                key >= stop
            }
        });
    }
    let (more, next) = if found.len() > limit {
        found.truncate(limit);
        (true, found.last().map(|(key, _)| key.clone()))
    } else {
        (stop.is_some(), stop)
    };
    if !query.forward {
        found.reverse();
    }
    let entries: Vec<Value> = found
        .into_iter()
        .map(|(key, mail)| entry(&key.home, &key, mail))
        .collect();
    Ok(json!({"entries": entries, "more": more, "next": next.map(|k| k.encode())}))
}
