//! What a first mate needs to keep working when it starts fresh.
//!
//! Context follows the focus: a task the ship owns (`task:<id>`), a request
//! thread (`thread:<root message id>`), the captain's conversation
//! (`captain`), or the ship's long-lived knowledge (`ship`). The runtime
//! records facts; a first mate keeps one short handoff note per focus holding
//! its judgement, which replaces the previous note (a few versions are kept).
//! Bearings summarize a ship from records; digests return a focus's history
//! on demand, curated and bounded, rather than preloading everything.
use crate::store::{Caller, Store, identifier, required};
use anyhow::{Result, bail, ensure};
use rusqlite::params;
use serde_json::{Value, json};

const NOTE_BYTES: usize = 2_048;
const SHIP_NOTE_BYTES: usize = 4_096;
const NOTE_VERSIONS: i64 = 5;
const LINE_CHARS: usize = 100;

pub fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS notes(ship TEXT NOT NULL, focus TEXT NOT NULL, version INTEGER NOT NULL,
           body TEXT NOT NULL, author TEXT NOT NULL, created_ms INTEGER NOT NULL,
           PRIMARY KEY(ship, focus, version));",
    )?;
    Ok(())
}

fn clip(text: &str, chars: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= chars {
        flat
    } else {
        format!("{}…", flat.chars().take(chars).collect::<String>())
    }
}

/// A ship hosted in this home.
fn local_ship(store: &Store, ship: &str) -> Result<()> {
    identifier(ship)?;
    let location: Option<String> = store
        .connection()
        .query_row(
            "SELECT json_extract(record, '$.location') FROM ships WHERE name=?1",
            [ship],
            |r| r.get(0),
        )
        .ok()
        .flatten();
    ensure!(
        location.as_deref() == Some("here"),
        "{ship} is not a ship hosted by this home"
    );
    Ok(())
}

/// Validate a focus name for a ship.
pub fn check_focus(store: &Store, ship: &str, focus: &str) -> Result<()> {
    match focus.split_once(':') {
        None if matches!(focus, "captain" | "ship") => Ok(()),
        Some(("task", id)) => {
            let task = store.task(id)?;
            ensure!(task["owner"] == ship, "task {id} is not owned by {ship}");
            Ok(())
        }
        Some(("thread", id)) => identifier(id),
        _ => bail!("focus must be task:<id>, thread:<id>, captain or ship"),
    }
}

/// The focus a message belongs to, for the ship that received it.
pub fn focus_of(store: &Store, ship: &str, message: &Value) -> Result<String> {
    if message["from"] == "captain" {
        return Ok("captain".into());
    }
    let correlated = match message["correlation"].as_str() {
        Some(id) => store
            .connection()
            .query_row("SELECT task_id FROM mail WHERE id=?1", [id], |r| {
                r.get::<_, Option<String>>(0)
            })
            .ok()
            .flatten(),
        None => None,
    };
    if let Some(task) = message["task_id"]
        .as_str()
        .map(str::to_owned)
        .or(correlated)
    {
        if store.task(&task).is_ok_and(|t| t["owner"] == ship) {
            return Ok(format!("task:{task}"));
        }
    }
    let root = message["correlation"]
        .as_str()
        .or(message["id"].as_str())
        .unwrap_or("");
    Ok(format!("thread:{root}"))
}

/// The current note for a focus, if any.
pub fn current_note(store: &Store, ship: &str, focus: &str) -> Result<Option<Value>> {
    Ok(store
        .connection()
        .query_row(
            "SELECT version, body, author, created_ms FROM notes WHERE ship=?1 AND focus=?2
             ORDER BY version DESC LIMIT 1",
            params![ship, focus],
            |r| {
                Ok(
                    json!({"version": r.get::<_, i64>(0)?, "body": r.get::<_, String>(1)?,
                    "author": r.get::<_, String>(2)?, "created_ms": r.get::<_, i64>(3)?}),
                )
            },
        )
        .ok())
}

fn note_put(store: &Store, args: &Value) -> Result<Value> {
    let ship = required(args, "ship")?;
    let focus = required(args, "focus")?;
    local_ship(store, ship)?;
    check_focus(store, ship, focus)?;
    let body = required(args, "body")?;
    let cap = if focus == "ship" {
        SHIP_NOTE_BYTES
    } else {
        NOTE_BYTES
    };
    ensure!(
        body.len() <= cap,
        "a {focus} note is limited to {cap} bytes; keep it to judgement: doing, why, waiting on, next"
    );
    let author = match store.caller() {
        Caller::Agent { actor } => actor.clone(),
        Caller::Operator => "operator".into(),
    };
    let tx = store.write_transaction()?;
    let version: i64 = tx.query_row(
        "SELECT COALESCE(MAX(version), 0) + 1 FROM notes WHERE ship=?1 AND focus=?2",
        params![ship, focus],
        |r| r.get(0),
    )?;
    tx.execute(
        "INSERT INTO notes(ship, focus, version, body, author, created_ms) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![ship, focus, version, body, author, Store::now_ms()],
    )?;
    // Notes replace each other; only the last few versions are kept.
    tx.execute(
        "DELETE FROM notes WHERE ship=?1 AND focus=?2 AND version <= ?3",
        params![ship, focus, version - NOTE_VERSIONS],
    )?;
    tx.commit()?;
    Ok(json!({"ship": ship, "focus": focus, "version": version}))
}

fn note_get(store: &Store, args: &Value) -> Result<Value> {
    let ship = required(args, "ship")?;
    let focus = required(args, "focus")?;
    local_ship(store, ship)?;
    let versions = args
        .get("versions")
        .and_then(Value::as_i64)
        .unwrap_or(1)
        .clamp(1, NOTE_VERSIONS);
    let mut statement = store.connection().prepare(
        "SELECT version, body, author, created_ms FROM notes WHERE ship=?1 AND focus=?2
         ORDER BY version DESC LIMIT ?3",
    )?;
    let notes = statement
        .query_map(params![ship, focus, versions], |r| {
            Ok(
                json!({"version": r.get::<_, i64>(0)?, "body": r.get::<_, String>(1)?,
                "author": r.get::<_, String>(2)?, "created_ms": r.get::<_, i64>(3)?}),
            )
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"ship": ship, "focus": focus, "notes": notes}))
}

fn note_list(store: &Store, args: &Value) -> Result<Value> {
    let ship = required(args, "ship")?;
    local_ship(store, ship)?;
    let mut statement = store.connection().prepare(
        "SELECT focus, MAX(version), MAX(created_ms) FROM notes WHERE ship=?1
         GROUP BY focus ORDER BY MAX(created_ms) DESC LIMIT 200",
    )?;
    let foci = statement
        .query_map([ship], |r| {
            Ok(
                json!({"focus": r.get::<_, String>(0)?, "version": r.get::<_, i64>(1)?,
                "updated_ms": r.get::<_, i64>(2)?}),
            )
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"ship": ship, "notes": foci}))
}

/// A bounded summary of a ship built from records, with no model call:
/// open tasks, requests owed and owing, unacknowledged mail and the
/// long-lived ship note.
pub fn bearings(store: &Store, ship: &str) -> Result<Value> {
    local_ship(store, ship)?;
    let lines = |sql: &str,
                 render: &dyn Fn(&rusqlite::Row) -> rusqlite::Result<String>|
     -> Result<Vec<String>> {
        let mut statement = store.connection().prepare(sql)?;
        let rows = statement.query_map([ship], |r| render(r))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    };
    let tasks = lines(
        "SELECT id, status, title FROM tasks WHERE owner=?1 AND status NOT IN ('done','cancelled')
         ORDER BY id LIMIT 20",
        &|r| {
            Ok(format!(
                "task {} [{}]: {}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                clip(&r.get::<_, String>(2)?, LINE_CHARS)
            ))
        },
    )?;
    let owed = lines(
        "SELECT id, sender, body FROM mail WHERE recipient=?1 AND state='open' ORDER BY seq LIMIT 10",
        &|r| {
            Ok(format!(
                "{} from {}: {}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                clip(&r.get::<_, String>(2)?, LINE_CHARS)
            ))
        },
    )?;
    let owing = lines(
        "SELECT id, recipient, body FROM mail WHERE sender=?1 AND state='open' ORDER BY seq LIMIT 10",
        &|r| {
            Ok(format!(
                "{} to {}: {}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                clip(&r.get::<_, String>(2)?, LINE_CHARS)
            ))
        },
    )?;
    let unacknowledged: i64 = store.connection().query_row(
        "SELECT COUNT(*) FROM mail WHERE recipient=?1 AND COALESCE(delivery, 'received')='received'
         AND seq > COALESCE((SELECT through_seq FROM mail_cursor WHERE address=?1), 0)",
        [ship],
        |r| r.get(0),
    )?;
    let knowledge = current_note(store, ship, "ship")?;
    let mut text = format!("Bearings for {ship}: {unacknowledged} unacknowledged messages.\n");
    for (title, items) in [
        ("Open tasks", &tasks),
        ("Requests you owe a reply", &owed),
        ("Requests awaiting replies to you", &owing),
    ] {
        if items.is_empty() {
            text.push_str(&format!("{title}: none.\n"));
        } else {
            text.push_str(&format!("{title}:\n"));
            for item in items {
                text.push_str(&format!("- {item}\n"));
            }
        }
    }
    if let Some(note) = &knowledge {
        text.push_str(&format!(
            "Ship knowledge (note v{}):\n{}\n",
            note["version"],
            note["body"].as_str().unwrap_or("")
        ));
    }
    Ok(json!({
        "ship": ship, "unacknowledged_mail": unacknowledged, "open_tasks": tasks,
        "requests_owed": owed, "requests_owing": owing, "knowledge": knowledge, "text": text,
    }))
}

/// A focus's history, curated and bounded, for a first mate that starts
/// fresh and needs more than its note.
fn digest(store: &Store, args: &Value) -> Result<Value> {
    let ship = required(args, "ship")?;
    let focus = required(args, "focus")?;
    local_ship(store, ship)?;
    check_focus(store, ship, focus)?;
    let limit = args
        .get("limit")
        .and_then(Value::as_i64)
        .unwrap_or(20)
        .clamp(1, 100);
    let mail = |sql: &str, key: &str| -> Result<Vec<Value>> {
        let mut statement = store.connection().prepare(sql)?;
        let rows = statement.query_map(params![key, limit], |r| {
            Ok(
                json!({"seq": r.get::<_, i64>(0)?, "id": r.get::<_, String>(1)?,
                "from": r.get::<_, String>(2)?, "to": r.get::<_, String>(3)?,
                "kind": r.get::<_, String>(4)?, "state": r.get::<_, Option<String>>(5)?,
                "body": clip(&r.get::<_, String>(6)?, 500)}),
            )
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    };
    const COLUMNS: &str = "seq, id, sender, recipient, kind, state, body";
    let mut out = json!({"ship": ship, "focus": focus, "note": current_note(store, ship, focus)?});
    match focus.split_once(':') {
        Some(("task", id)) => {
            out["task"] = store.task(id)?;
            let mut statement = store.connection().prepare(
                "SELECT seq, kind, issue_key, payload FROM events WHERE task_id=?1 ORDER BY seq DESC LIMIT ?2",
            )?;
            out["events"] = json!(
                statement
                    .query_map(params![id, limit], |r| {
                        Ok(json!({"seq": r.get::<_, i64>(0)?, "kind": r.get::<_, String>(1)?,
                            "issue": r.get::<_, String>(2)?, "payload": clip(&r.get::<_, String>(3)?, 200)}))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            );
            let mut statement = store.connection().prepare(
                "SELECT json_extract(record,'$.id'), json_extract(record,'$.status'),
                   json_extract(record,'$.exit_code'), json_extract(record,'$.reason'),
                   json_extract(record,'$.created_ms')
                 FROM jobs WHERE json_extract(record,'$.task_id')=?1
                 ORDER BY json_extract(record,'$.created_ms') DESC LIMIT ?2",
            )?;
            out["jobs"] =
                json!(
                statement
                    .query_map(params![id, limit], |r| {
                        Ok(json!({"id": r.get::<_, String>(0)?, "status": r.get::<_, String>(1)?,
                            "exit_code": r.get::<_, Option<i64>>(2)?,
                            "reason": r.get::<_, Option<String>>(3)?.map(|s| clip(&s, 200)),
                            "created_ms": r.get::<_, i64>(4)?}))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            );
            out["mail"] = json!(mail(
                &format!("SELECT {COLUMNS} FROM mail WHERE task_id=?1 ORDER BY seq DESC LIMIT ?2"),
                id
            )?);
        }
        Some(("thread", root)) => {
            out["mail"] = json!(mail(
                &format!(
                    "SELECT {COLUMNS} FROM mail WHERE id=?1 OR correlation=?1 ORDER BY seq LIMIT ?2"
                ),
                root
            )?);
        }
        _ if focus == "captain" => {
            let mut statement = store.connection().prepare(&format!(
                "SELECT {COLUMNS} FROM mail WHERE (sender='captain' AND recipient=?1)
                 OR (sender=?1 AND recipient='captain') ORDER BY seq DESC LIMIT ?2"
            ))?;
            out["mail"] = json!(
                statement
                    .query_map(params![ship, limit], |r| {
                        Ok(
                            json!({"seq": r.get::<_, i64>(0)?, "id": r.get::<_, String>(1)?,
                            "from": r.get::<_, String>(2)?, "to": r.get::<_, String>(3)?,
                            "kind": r.get::<_, String>(4)?, "state": r.get::<_, Option<String>>(5)?,
                            "body": clip(&r.get::<_, String>(6)?, 500)}),
                        )
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            );
        }
        _ => {}
    }
    Ok(out)
}

/// Retention for `history.prune`: notes for finished tasks and for threads
/// whose latest note predates the cutoff. Captain and ship notes are kept.
pub fn prune(store: &Store, cutoff_ms: i64, dry_run: bool) -> Result<i64> {
    let stale = "(focus LIKE 'thread:%' OR (focus LIKE 'task:%' AND EXISTS(
           SELECT 1 FROM tasks t WHERE t.id = substr(notes.focus, 6) AND t.status IN ('done','cancelled'))))
         AND (SELECT MAX(created_ms) FROM notes n WHERE n.ship=notes.ship AND n.focus=notes.focus) < ?1";
    let count: i64 = store.connection().query_row(
        &format!("SELECT COUNT(*) FROM notes WHERE {stale}"),
        [cutoff_ms],
        |r| r.get(0),
    )?;
    if !dry_run {
        store
            .connection()
            .execute(&format!("DELETE FROM notes WHERE {stale}"), [cutoff_ms])?;
    }
    Ok(count)
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "note.put" => note_put(store, &args),
        "note.get" => note_get(store, &args),
        "note.list" => note_list(store, &args),
        "focus.digest" => digest(store, &args),
        "ship.bearings" => bearings(store, required(&args, "ship")?),
        _ => bail!("unknown context command {command}"),
    }
}
