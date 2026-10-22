//! `activity.list`: merged, time-ordered audit across fleet homes and linked machines.
//!
//! Operator-only. Each home records audit events in its SQLite `audit` table.
//! `activity.list` merges rows across local homes and reached machines in
//! `time_ms ASC, seq ASC` order, tagged with machine and ship, cursor-paged with an
//! opaque cursor.
//!
//! Reaching machines connect one-way inbound to this machine, so they are listed in
//! `machines:[{name, reached: false, error}]` rather than carrying audit entries on collect;
//! this keeps audit bounded and avoids expanding the carry payload.
use crate::store::{Caller, Store, identifier};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;
const SCAN_PAGES: usize = 4;
const CURSOR_PREFIX: &str = "a1.";
const SET: &str = "<set>";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Key {
    pub(crate) ms: i64,
    pub(crate) home: String,
    pub(crate) seq: i64,
}

impl Key {
    pub(crate) fn encode(&self) -> String {
        format!("{CURSOR_PREFIX}{}.{}.{}", self.ms, self.seq, self.home)
    }

    pub(crate) fn parse(value: &Value, forward: bool) -> Result<Key> {
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
        parsed.with_context(|| format!("{text} is not an activity cursor"))
    }

    pub(crate) fn seq_bound(&self, home: &str) -> i64 {
        match home.cmp(self.home.as_str()) {
            std::cmp::Ordering::Equal => self.seq,
            std::cmp::Ordering::Less => i64::MAX,
            std::cmp::Ordering::Greater => i64::MIN,
        }
    }
}

pub(crate) fn masked(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .filter(|(key, val)| {
                    if key.as_str() == "argv" || key.as_str() == "env" {
                        return false;
                    }
                    if key.as_str() == "environment" && !val.is_string() {
                        return false;
                    }
                    true
                })
                .map(|(key, value)| {
                    let shown = if crate::block_ask::sensitive_name(key) {
                        json!(SET)
                    } else {
                        masked(value)
                    };
                    (key.clone(), shown)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(masked).collect()),
        Value::String(text) => json!(crate::block_ask::mask(text)),
        other => other.clone(),
    }
}

pub(crate) fn matches_kind(kind: &str, filters: &[String]) -> bool {
    filters.iter().any(|pattern| {
        if pattern.ends_with('.') {
            kind.starts_with(pattern)
        } else {
            kind == pattern || kind.starts_with(&format!("{pattern}."))
        }
    })
}

struct Home {
    ship: String,
    store: Store,
}

struct Query {
    after: Option<Key>,
    want: usize,
    ship: Option<String>,
    kinds: Option<Vec<String>>,
}

type Rows = Vec<(Key, Value)>;

fn local_homes(asking: Option<&Store>) -> Result<Vec<Home>> {
    let conn = crate::fleet::registry()?;
    let members = crate::fleet::members(&conn)?;
    let same = |home: &std::path::Path| home.canonicalize().unwrap_or_else(|_| home.to_owned());
    let mut seen = BTreeSet::new();
    let mut homes = vec![];
    if let Some(store) = asking {
        if seen.insert(same(store.home())) {
            let ship = crate::usage::home_ship(store, &members);
            homes.push(Home {
                ship,
                store: Store::open_readonly(store.home(), Caller::Operator)?,
            });
        }
    }
    for member in members {
        if !member.home.is_dir() || !seen.insert(same(&member.home)) {
            continue;
        }
        if let Ok(store) = Store::open_readonly(&member.home, Caller::Operator) {
            homes.push(Home {
                ship: member.name,
                store,
            });
        }
    }
    homes.sort_by(|a, b| a.ship.cmp(&b.ship));
    Ok(homes)
}

fn scan(machine: &str, home: &Home, query: &Query) -> Result<(Rows, Option<Key>)> {
    let mut sql = "SELECT seq, time_ms, kind, payload FROM audit
               WHERE (time_ms, seq) > (?1, ?2)"
        .to_string();
    let mut pattern_params: Vec<String> = vec![];
    if let Some(kinds) = &query.kinds {
        if kinds.is_empty() {
            sql.push_str(" AND (0)");
        } else {
            let mut clauses = vec![];
            let mut param_idx = 4;
            for pattern in kinds {
                if pattern.ends_with('.') {
                    clauses.push(format!("kind LIKE ?{param_idx}"));
                    pattern_params.push(format!("{pattern}%"));
                    param_idx += 1;
                } else {
                    clauses.push(format!(
                        "(kind = ?{param_idx} OR kind LIKE ?{})",
                        param_idx + 1
                    ));
                    pattern_params.push(pattern.clone());
                    pattern_params.push(format!("{pattern}.%"));
                    param_idx += 2;
                }
            }
            sql.push_str(&format!(" AND ({})", clauses.join(" OR ")));
        }
    }
    sql.push_str(" ORDER BY time_ms ASC, seq ASC LIMIT ?3");
    let mut after = query
        .after
        .as_ref()
        .map_or((i64::MIN, i64::MIN), |k| (k.ms, k.seq_bound(&home.ship)));
    let mut kept = vec![];
    let mut scanned = 0;
    let budget = query.want * SCAN_PAGES;
    let mut statement = home.store.connection().prepare(&sql)?;
    loop {
        let batch = (query.want - kept.len()).min(budget - scanned).max(1);
        let mut params: Vec<rusqlite::types::Value> = vec![
            rusqlite::types::Value::Integer(after.0),
            rusqlite::types::Value::Integer(after.1),
            rusqlite::types::Value::Integer(batch as i64),
        ];
        for p in &pattern_params {
            params.push(rusqlite::types::Value::Text(p.clone()));
        }
        let rows = statement
            .query_map(rusqlite::params_from_iter(params), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let exhausted = rows.len() < batch;
        let mut last = None;
        for (seq, time_ms, kind, payload_str) in rows {
            scanned += 1;
            let key = Key {
                ms: time_ms,
                home: home.ship.clone(),
                seq,
            };
            after = (key.ms, key.seq);
            let raw_payload: Value =
                serde_json::from_str(&payload_str).unwrap_or_else(|_| json!({}));
            let entry_ship = raw_payload
                .get("ship")
                .and_then(Value::as_str)
                .unwrap_or(&home.ship)
                .to_owned();
            if let Some(target_ship) = &query.ship {
                if entry_ship != *target_ship {
                    last = Some(key);
                    continue;
                }
            }
            if let Some(kinds) = &query.kinds {
                if !matches_kind(&kind, kinds) {
                    last = Some(key);
                    continue;
                }
            }
            let detail = masked(&raw_payload);
            let entry = json!({
                "id": format!("{}:{seq}", home.ship),
                "cursor": key.encode(),
                "machine": machine,
                "ship": entry_ship,
                "home": home.ship,
                "at_ms": time_ms,
                "kind": kind,
                "detail": detail,
            });
            kept.push((key.clone(), entry));
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

fn parse_query(args: &Value) -> Result<Query> {
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
    let kinds = match args.get("kinds").filter(|v| !v.is_null()) {
        Some(value) => {
            let list = value
                .as_array()
                .context("kinds must be a list of strings")?;
            let parsed = list
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .context("each kind must be a string")
                })
                .collect::<Result<Vec<_>>>()?;
            Some(parsed)
        }
        None => None,
    };
    let after = match args.get("after").filter(|v| !v.is_null()) {
        Some(value) => Some(Key::parse(value, true)?),
        None => None,
    };
    Ok(Query {
        after,
        want: limit + 1,
        ship,
        kinds,
    })
}

/// This machine's own homes only: what a gateway answers.
pub(crate) fn local(args: &Value) -> Result<Value> {
    let query = parse_query(args)?;
    let homes = local_homes(None)?;
    let machine = crate::machines::this_machine();
    let limit = match args.get("limit").filter(|v| !v.is_null()) {
        Some(value) => value.as_i64().context("limit must be a whole number")?,
        None => DEFAULT_LIMIT,
    }
    .clamp(1, MAX_LIMIT) as usize;

    let mut found: Vec<(Key, Value)> = vec![];
    let mut stop: Option<Key> = None;
    for home in &homes {
        let (rows, last) = scan(&machine, home, &query)?;
        found.extend(rows);
        if let Some(last) = last {
            stop = Some(match stop {
                Some(stop) if stop < last => stop,
                _ => last,
            });
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some(stop) = &stop {
        found.retain(|(key, _)| key <= stop);
    }
    let (more, next) = if found.len() > limit {
        found.truncate(limit);
        (true, found.last().map(|(key, _)| key.clone()))
    } else {
        (stop.is_some(), stop)
    };
    let entries: Vec<Value> = found.into_iter().map(|(_, e)| e).collect();
    Ok(json!({
        "entries": entries,
        "more": more,
        "next": next.map(|k| k.encode()),
    }))
}

/// The argument keys of each command here; see `crate::Accepted`.
pub(crate) const ACCEPTED: crate::Accepted =
    &[("activity.list", &["limit", "ship", "kinds", "after"])];

/// The fleet's activity across local homes and linked machines.
pub fn list(store: &Store, args: &Value) -> Result<Value> {
    let query = parse_query(args)?;
    let limit = match args.get("limit").filter(|v| !v.is_null()) {
        Some(value) => value.as_i64().context("limit must be a whole number")?,
        None => DEFAULT_LIMIT,
    }
    .clamp(1, MAX_LIMIT) as usize;
    let machine = crate::machines::this_machine();
    let homes = local_homes(Some(store))?;
    let mut machines = vec![json!({"name": machine, "reached": true})];

    let mut found: Vec<(Key, Value)> = vec![];
    let mut stop: Option<Key> = None;

    // Local homes on this machine
    for home in &homes {
        let (rows, last) = scan(&machine, home, &query)?;
        found.extend(rows);
        if let Some(last) = last {
            stop = Some(match stop {
                Some(stop) if stop < last => stop,
                _ => last,
            });
        }
    }

    // Linked machines if available
    if let Ok(conn) = crate::fleet::registry() {
        if let Ok(linked) = crate::machines::linked_for_snapshot(&conn) {
            let forwarded_after = match args.get("after").filter(|v| !v.is_null()) {
                Some(value) if value.is_i64() || value.is_u64() => Some(value.clone()),
                _ => query.after.as_ref().map(|k| json!(k.encode())),
            };
            let asked = json!({
                "after": forwarded_after,
                "limit": query.want,
                "ship": query.ship,
                "kinds": query.kinds,
            });

            // Reached machines over gateway
            let reached: Vec<(String, Result<Value>)> = std::thread::scope(|scope| {
                let handles: Vec<_> = linked
                    .iter()
                    .filter(|(_, role, _)| role == "collects")
                    .map(|(name, _, _)| {
                        let asked = asked.clone();
                        (
                            name.clone(),
                            scope.spawn(move || crate::machines::activity_of(name, asked)),
                        )
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|(name, h)| {
                        let result = h
                            .join()
                            .unwrap_or_else(|_| Err(anyhow::anyhow!("panicked")));
                        (name, result)
                    })
                    .collect()
            });

            for (name, result) in reached {
                match result {
                    Ok(mut answered) => {
                        machines.push(json!({"name": name, "reached": true}));
                        if let Some(entries) = answered["entries"].as_array_mut() {
                            for entry in entries {
                                entry["machine"] = json!(name);
                                let Ok(key) = Key::parse(&entry["cursor"], true) else {
                                    continue;
                                };
                                // Mask only what the other machine recorded: a cursor or
                                // id can look secret-shaped and must stay usable.
                                entry["detail"] = masked(&entry["detail"]);
                                found.push((key, entry.clone()));
                            }
                        }
                        if answered["more"].as_bool().unwrap_or(false) {
                            if let Ok(last) = Key::parse(&answered["next"], true) {
                                stop = Some(match stop {
                                    Some(stop) if stop < last => stop,
                                    _ => last,
                                });
                            }
                        }
                    }
                    Err(error) => {
                        machines.push(json!({
                            "name": name,
                            "reached": false,
                            "error": format!("{error:#}").chars().take(500).collect::<String>(),
                        }));
                    }
                }
            }

            // Reaching machines
            for (name, role, _) in &linked {
                if role != "collected" {
                    continue;
                }
                machines.push(json!({
                    "name": name,
                    "reached": false,
                    "error": format!("links one way; {name} reaches this machine, not reached from here"),
                }));
            }
        }
    }

    machines.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

    found.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some(stop) = &stop {
        found.retain(|(key, _)| key <= stop);
    }
    let (more, next) = if found.len() > limit {
        found.truncate(limit);
        (true, found.last().map(|(key, _)| key.clone()))
    } else {
        (stop.is_some(), stop)
    };
    let entries: Vec<Value> = found.into_iter().map(|(_, e)| e).collect();
    Ok(json!({
        "entries": entries,
        "more": more,
        "next": next.map(|k| k.encode()),
        "machines": machines,
    }))
}
