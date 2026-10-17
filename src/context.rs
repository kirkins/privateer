//! What a first mate needs to keep working when it starts fresh.
//!
//! Context follows the focus: a task the ship owns (`task:<id>`), a request
//! thread (`thread:<root message id>`), the captain's conversation
//! (`captain`), or the ship's long-lived knowledge (`ship`). The runtime
//! records facts; a first mate keeps one short handoff note per focus holding
//! its judgement, which replaces the previous note (a few versions are kept).
//! Exact identifiers it must carry go in a focus's few pins instead, and a
//! crew task's assignment is a longer document (`assignment:<task>`) that the
//! crew read and the first mate's wakes never load.
//! Bearings summarize a ship from records; digests return a focus's history
//! on demand, curated and bounded, rather than preloading everything.
use crate::store::{Caller, Store, identifier, required};
use anyhow::{Result, bail, ensure};
use rusqlite::params;
use serde_json::{Value, json};

const NOTE_BYTES: usize = 2_048;
const SHIP_NOTE_BYTES: usize = 4_096;
const ASSIGNMENT_BYTES: usize = 16_384;
const PINS_PER_FOCUS: i64 = 8;
const PIN_VALUE_CHARS: usize = 200;
const NOTE_VERSIONS: i64 = 5;
/// The longest average word a readable note has.
const READABLE_WORD_BYTES: usize = 12;
const LINE_CHARS: usize = 100;

pub fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS notes(ship TEXT NOT NULL, focus TEXT NOT NULL, version INTEGER NOT NULL,
           body TEXT NOT NULL, author TEXT NOT NULL, created_ms INTEGER NOT NULL,
           PRIMARY KEY(ship, focus, version));
         CREATE TABLE IF NOT EXISTS pins(ship TEXT NOT NULL, focus TEXT NOT NULL, key TEXT NOT NULL,
           value TEXT NOT NULL, set_ms INTEGER NOT NULL, PRIMARY KEY(ship, focus, key));",
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
        Some(("task" | "assignment", id)) => {
            let task = store.task(id)?;
            ensure!(task["owner"] == ship, "task {id} is not owned by {ship}");
            Ok(())
        }
        Some(("thread", id)) => identifier(id),
        _ => bail!("focus must be task:<id>, assignment:<id>, thread:<id>, captain or ship"),
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
    } else if focus.starts_with("assignment:") {
        ASSIGNMENT_BYTES
    } else {
        NOTE_BYTES
    };
    ensure!(
        body.len() <= cap,
        "a {focus} note is limited to {cap} bytes; keep it to judgement: doing, why, waiting on, next. Pin exact ids with note.pin and leave logs to the records instead of dropping spaces to fit"
    );
    // Squeezing a note under its cap by deleting spaces leaves text the next
    // wake must decode. Ordinary prose, ids and paths included, averages well
    // under twelve characters a word; squeezed notes run from fifteen to forty.
    let words = body.split_whitespace().count().max(1);
    let letters = body.split_whitespace().map(str::len).sum::<usize>();
    ensure!(
        body.len() < 200 || letters <= READABLE_WORD_BYTES * words,
        "this note averages {} characters a word, which reads as text with its spaces removed; write it in sentences, and leave ids, hashes and logs to the records (job.status, mail.show, focus.digest)",
        letters / words
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
    let mut out =
        json!({"ship": ship, "focus": focus, "notes": notes, "pins": pins(store, ship, focus)?});
    // Crew read their task's note; its assignment comes with it.
    if let Some(("task", id)) = focus.split_once(':') {
        out["assignment"] = current_note(store, ship, &format!("assignment:{id}"))?
            .map_or(Value::Null, |note| note["body"].clone());
    }
    Ok(out)
}

/// A focus's pinned facts, by key.
pub fn pins(store: &Store, ship: &str, focus: &str) -> Result<Value> {
    let mut statement = store
        .connection()
        .prepare("SELECT key, value FROM pins WHERE ship=?1 AND focus=?2 ORDER BY key")?;
    let pins = statement
        .query_map(params![ship, focus], |r| {
            Ok((r.get::<_, String>(0)?, Value::String(r.get(1)?)))
        })?
        .collect::<rusqlite::Result<serde_json::Map<_, _>>>()?;
    Ok(Value::Object(pins))
}

/// Pin an exact fact to a focus, such as a frozen head, a gate job or a PR,
/// so it need not be carried in the note's prose. An empty or null value
/// unpins it.
fn note_pin(store: &Store, args: &Value) -> Result<Value> {
    let ship = required(args, "ship")?;
    let focus = required(args, "focus")?;
    local_ship(store, ship)?;
    check_focus(store, ship, focus)?;
    let key = required(args, "key")?;
    identifier(key)?;
    ensure!(key.len() <= 32, "a pin key is at most 32 characters");
    let value = args.get("value").and_then(Value::as_str).unwrap_or("");
    let tx = store.write_transaction()?;
    if value.is_empty() {
        tx.execute(
            "DELETE FROM pins WHERE ship=?1 AND focus=?2 AND key=?3",
            params![ship, focus, key],
        )?;
    } else {
        ensure!(
            value.chars().count() <= PIN_VALUE_CHARS && !value.contains('\n'),
            "a pin is one line of at most {PIN_VALUE_CHARS} characters; judgement belongs in the note"
        );
        let others: i64 = tx.query_row(
            "SELECT COUNT(*) FROM pins WHERE ship=?1 AND focus=?2 AND key<>?3",
            params![ship, focus, key],
            |r| r.get(0),
        )?;
        ensure!(
            others < PINS_PER_FOCUS,
            "a focus holds at most {PINS_PER_FOCUS} pins; unpin one that no longer matters"
        );
        tx.execute(
            "INSERT INTO pins(ship, focus, key, value, set_ms) VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(ship, focus, key) DO UPDATE SET value=excluded.value, set_ms=excluded.set_ms",
            params![ship, focus, key, value, Store::now_ms()],
        )?;
    }
    tx.commit()?;
    Ok(json!({"ship": ship, "focus": focus, "pins": pins(store, ship, focus)?}))
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
        // Work in hand first, then the backlog in the order it starts.
        &format!(
            "SELECT id, status, title, rank FROM tasks WHERE owner=?1 AND status NOT IN ('done','cancelled')
             ORDER BY CASE status WHEN 'running' THEN 0 WHEN 'blocked' THEN 1 ELSE 2 END, {BACKLOG_ORDER} LIMIT 20"
        ),
        &|r| {
            let rank = r
                .get::<_, Option<i64>>(3)?
                .map(|n| format!(" #{n}"))
                .unwrap_or_default();
            Ok(format!(
                "task {} [{}{rank}]: {}",
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
    let idle = idle_tasks(store, ship)?;
    let attention = crate::attention::summary_for(store, ship)?;
    let scheduling = crate::scheduling::snapshot(store, ship)?;
    let knowledge = current_note(store, ship, "ship")?;
    let mut text = format!("Bearings for {ship}: {unacknowledged} unacknowledged messages.\n");
    if let Some(line) = crate::away::bearings_line() {
        text.push_str(&line);
        text.push('\n');
    }
    // A hand-edited routes file must not stop wakes; launches fail closed.
    match crate::crew_routes::summary(store, ship) {
        Ok(Some(line)) => text.push_str(&format!("{line}\n")),
        Ok(None) => {}
        Err(error) => text.push_str(&format!("Routes file unreadable: {error:#}\n")),
    }
    if let Some(line) = crate::attention::line(&attention) {
        text.push_str(&line);
        text.push('\n');
    }
    let throttled = crate::jobs::host_ledger(store)
        .and_then(|conn| crate::routes::list(&conn))
        .unwrap_or_default();
    for route in &throttled {
        text.push_str(&format!(
            "Model route {} is backing off for {}s ({}); new crew and errands on it wait.\n",
            route["route"].as_str().unwrap_or(""),
            ((route["until_ms"].as_i64().unwrap_or(0) - Store::now_ms()) / 1000).max(1),
            route["last_error"].as_str().unwrap_or("")
        ));
    }
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
    let errands = crate::errand::active(store, ship)?;
    if !errands.is_empty() {
        text.push_str("Errands running (their verdicts arrive as mail):\n");
        for (id, focus, brief) in &errands {
            text.push_str(&format!("- {id} on {focus}: {}\n", clip(brief, LINE_CHARS)));
        }
    }
    if !idle.is_empty() {
        text.push_str(
            "Running tasks with nothing moving them (no job, open request or pinned wait):\n",
        );
        for task in &idle {
            text.push_str(&format!(
                "- task {}: {}\n",
                task["id"].as_str().unwrap_or(""),
                task["title"].as_str().unwrap_or("")
            ));
        }
    }
    if !scheduling.is_empty() {
        text.push_str("Task scheduling (unscheduled tasks require explicit task.enqueue):\n");
        for work in scheduling.iter().take(20) {
            text.push_str(&format!(
                "- {}: {} · {} · next check {}\n",
                work["task_id"].as_str().unwrap_or("?"),
                work["state"].as_str().unwrap_or("unknown"),
                clip(work["reason"].as_str().unwrap_or(""), 120),
                work["next_check_ms"]
            ));
        }
    }
    if let Some(note) = &knowledge {
        text.push_str(&format!(
            "Ship knowledge (note v{}):\n{}\n",
            note["version"],
            note["body"].as_str().unwrap_or("")
        ));
    }
    // Only a hosted first mate has a pane; its release is reported, not judged.
    let hosted_pane = match crate::host::upgrade::view(store, ship) {
        Ok(pane) => pane,
        Err(error) => Some(json!({"error": format!("{error:#}")})),
    };
    if let Some(pane) = hosted_pane.as_ref().filter(|pane| pane["drift"] == true) {
        text.push_str(&format!(
            "Hosted first mate runs release {} while {} is current; it relaunches at its next idle point, or `privateer host relaunch {ship}` moves it now.\n",
            pane["commit"].as_str().unwrap_or("an unversioned build"),
            pane["current_commit"].as_str().unwrap_or("")
        ));
    }
    Ok(json!({
        "ship": ship, "unacknowledged_mail": unacknowledged, "open_tasks": tasks,
        "backlog": backlog(store, ship)?,
        "requests_owed": owed, "requests_owing": owing, "knowledge": knowledge,
        "idle_tasks": idle, "attention": attention, "scheduling": scheduling, "text": text,
        "hosted_pane": hosted_pane,
    }))
}

/// Tasks the ship marks running that nothing is moving: no live job on the
/// task or any task beneath it, no open request the ship sent about them, and
/// no pinned `waiting` fact. A first mate that says it will carry on and then
/// stops leaves exactly these behind. The duty task is ongoing by design.
pub fn idle_tasks(store: &Store, ship: &str) -> Result<Vec<Value>> {
    let mut statement = store.connection().prepare(
        "WITH RECURSIVE tree(root, id) AS (
           SELECT id, id FROM tasks WHERE owner=?1 AND status='running'
           UNION SELECT tree.root, t.id FROM tasks t JOIN tree ON t.parent = tree.id)
         SELECT t.id, t.title FROM tasks t
         WHERE t.owner=?1 AND t.status='running' AND t.id <> ?1 || '-duty'
           AND NOT EXISTS (SELECT 1 FROM tree JOIN jobs j
             ON json_extract(j.record,'$.task_id') = tree.id
             WHERE tree.root = t.id AND json_extract(j.record,'$.status')
               IN ('starting','queued','launching','running','orphaned'))
           AND NOT EXISTS (SELECT 1 FROM tree JOIN mail m ON m.task_id = tree.id
             WHERE tree.root = t.id AND m.sender=?1 AND m.state='open')
           AND NOT EXISTS (SELECT 1 FROM pins p
             WHERE p.ship=?1 AND p.focus='task:' || t.id AND p.key='waiting')
         ORDER BY t.id LIMIT 20",
    )?;
    let rows = statement.query_map([ship], |r| {
        Ok(json!({"id": r.get::<_, String>(0)?, "title": r.get::<_, String>(1)?}))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Open tasks start in this order: ranked ones by rank, then the rest oldest
/// first.
pub(crate) const BACKLOG_ORDER: &str = "rank IS NULL, rank, rowid";

/// A ship's backlog: open tasks it has not started, next first. The duty
/// task is ongoing by design, not waiting work.
pub fn backlog(store: &Store, ship: &str) -> Result<Vec<Value>> {
    let mut statement = store.connection().prepare(&format!(
        "SELECT id, title, rank FROM tasks WHERE owner=?1 AND status='open'
           AND id <> ?1 || '-duty'
         ORDER BY {BACKLOG_ORDER} LIMIT 100"
    ))?;
    let rows = statement.query_map([ship], |r| {
        Ok(
            json!({"id": r.get::<_, String>(0)?, "title": r.get::<_, String>(1)?,
            "rank": r.get::<_, Option<i64>>(2)?}),
        )
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
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
           SELECT 1 FROM tasks t WHERE t.id = substr(notes.focus, 6) AND t.status IN ('done','cancelled')))
         OR (focus LIKE 'assignment:%' AND EXISTS(
           SELECT 1 FROM tasks t WHERE t.id = substr(notes.focus, 12) AND t.status IN ('done','cancelled'))))
         AND (SELECT MAX(created_ms) FROM notes n WHERE n.ship=notes.ship AND n.focus=notes.focus) < ?1";
    let stale_pins = "(focus LIKE 'thread:%' OR (focus LIKE 'task:%' AND EXISTS(
           SELECT 1 FROM tasks t WHERE t.id = substr(pins.focus, 6) AND t.status IN ('done','cancelled'))))
         AND (SELECT MAX(set_ms) FROM pins p WHERE p.ship=pins.ship AND p.focus=pins.focus) < ?1";
    let count: i64 = store.connection().query_row(
        &format!("SELECT COUNT(*) FROM notes WHERE {stale}"),
        [cutoff_ms],
        |r| r.get(0),
    )?;
    if !dry_run {
        store
            .connection()
            .execute(&format!("DELETE FROM notes WHERE {stale}"), [cutoff_ms])?;
        store
            .connection()
            .execute(&format!("DELETE FROM pins WHERE {stale_pins}"), [cutoff_ms])?;
    }
    Ok(count)
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "note.put" => note_put(store, &args),
        "note.get" => note_get(store, &args),
        "note.list" => note_list(store, &args),
        "note.pin" => note_pin(store, &args),
        "focus.digest" => digest(store, &args),
        "ship.bearings" => bearings(store, required(&args, "ship")?),
        _ => bail!("unknown context command {command}"),
    }
}
