//! Dormant, bounded home capture. This is a projection, never mail custody.
//!
//! Mail writers call only the best-effort observation hook, under their own
//! transaction. Capture/import is deliberately not called by any exchange.
#![cfg_attr(not(test), allow(dead_code))]
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

pub(crate) const LIMIT: i64 = 10_000;
const BYTES: i64 = 64 * 1024 * 1024;
const ITEM_BYTES: usize = 128 * 1024;
const TABLES: &[&str] = &[
    "mail_source_health",
    "mail_source_origin",
    "mail_source_outbox",
    "mail_source_snapshot",
    "mail_source_import",
];

pub(crate) fn missing(conn: &Connection, tables: &[&str]) -> Result<bool> {
    for table in tables {
        if !conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |r| r.get::<_, bool>(0),
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Called only by ordinary writable startup. A current schema needs no lock.
pub(crate) fn init(conn: &Connection) -> Result<()> {
    if !missing(conn, TABLES)? {
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    if missing(&tx, TABLES)? {
        tx.execute_batch("CREATE TABLE IF NOT EXISTS mail_source_health(
            only INTEGER PRIMARY KEY CHECK(only=1), mailbox TEXT NOT NULL,
            entries INTEGER NOT NULL DEFAULT 0, bytes INTEGER NOT NULL DEFAULT 0,
            lost INTEGER NOT NULL DEFAULT 0);
          CREATE TABLE IF NOT EXISTS mail_source_origin(
            id TEXT PRIMARY KEY, provenance TEXT NOT NULL, content TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS mail_source_outbox(
            seq INTEGER PRIMARY KEY AUTOINCREMENT, intent TEXT NOT NULL UNIQUE, event TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS mail_source_snapshot(
            only INTEGER PRIMARY KEY CHECK(only=1), binding TEXT NOT NULL,
            upper_seq INTEGER NOT NULL, copied_through INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS mail_source_import(
            seq INTEGER PRIMARY KEY AUTOINCREMENT, intent TEXT NOT NULL UNIQUE, event TEXT NOT NULL, receipt TEXT);")?;
        tx.execute(
            "INSERT OR IGNORE INTO mail_source_health(only,mailbox) VALUES(1,?1)",
            [uuid::Uuid::new_v4().to_string()],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// No inferred cross-home identities. Inbound old-protocol mail is Unresolved.
/// LocalCopy is used only by a same-machine runtime copy with a known mailbox.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum Provenance {
    Local,
    LocalCopy { mailbox: String, id: String },
    Copy(crate::door::coverage::MailIdentity),
    Unresolved,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HomeFact {
    pub mailbox: String,
    pub id: String,
    pub provenance: Provenance,
    pub content: Value,
    pub state: Option<String>,
    pub unread: bool,
    pub pending: bool,
}

fn fact(conn: &Connection, id: &str, new: Option<&Provenance>) -> Result<Option<HomeFact>> {
    let row = conn
        .query_row(
            &format!(
                "SELECT {} FROM mail WHERE id=?1 AND (sender='captain' OR recipient='captain')",
                super::COLUMNS
            ),
            [id],
            super::message,
        )
        .optional()?;
    let Some(row) = row else {
        return Ok(None);
    };
    let mailbox = conn.query_row(
        "SELECT mailbox FROM mail_source_health WHERE only=1",
        [],
        |r| r.get(0),
    )?;
    // Do not replicate mutable attachment/delivery bookkeeping as immutable
    // content. Preserve the original payload on first observation, including
    // correlation and both conversation participants.
    let mut content = row.clone();
    for key in [
        "seq",
        "state",
        "answered_by",
        "answered_ms",
        "ended_by",
        "ended_ms",
        "end_reason",
        "delivery",
        "delivery_attempts",
        "delivery_error",
        "delivered_ms",
        "remote_end_state",
    ] {
        content.as_object_mut().unwrap().remove(key);
    }
    let old: Option<(String, String)> = conn
        .query_row(
            "SELECT provenance,content FROM mail_source_origin WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (provenance, content) = if let Some((p, c)) = old {
        let provenance: Provenance = serde_json::from_str(&p)?;
        if let Some(new) = new {
            ensure!(*new == provenance, "conflicting provenance");
        }
        (provenance, serde_json::from_str(&c)?)
    } else {
        (new.cloned().unwrap_or(Provenance::Unresolved), content)
    };
    let cursor: i64 = conn.query_row(
        "SELECT COALESCE((SELECT through_seq FROM mail_cursor WHERE address='captain'),0)",
        [],
        |r| r.get(0),
    )?;
    Ok(Some(HomeFact {
        mailbox,
        id: id.into(),
        provenance,
        content,
        state: row["state"]
            .as_str()
            .map(|s| if s == "open" { "open" } else { "closed" }.into()),
        unread: row["to"] == "captain"
            && row["seq"].as_i64().unwrap_or(0) > cursor
            && row["state"] != "acknowledged",
        pending: row["state"] == "open",
    }))
}

fn remember(conn: &Connection, f: &HomeFact) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO mail_source_origin(id,provenance,content) VALUES(?1,?2,?3)",
        params![
            f.id,
            serde_json::to_string(&f.provenance)?,
            serde_json::to_string(&f.content)?
        ],
    )?;
    Ok(())
}

/// Savepoint errors never escape into mail. No trigger couples mail success to
/// outbox success. Zero busy timeout means an optional outbox table lock cannot
/// consume the ordinary writer's ten-second wait. The separate health row is
/// sticky on overflow/insert failure; capture also refuses missing schema.
/// Database-wide write locks still govern *mail itself*, not an outbox wait.
fn best_effort(conn: &Connection, f: impl FnOnce() -> Result<()>) {
    let timeout: rusqlite::Result<u64> = conn.query_row("PRAGMA busy_timeout", [], |r| r.get(0));
    let Ok(timeout) = timeout else {
        return;
    };
    if conn.busy_timeout(Duration::ZERO).is_err() {
        return;
    }
    if conn.execute_batch("SAVEPOINT mail_source_observe").is_ok() {
        if f().is_err() {
            let _ = conn.execute_batch("ROLLBACK TO mail_source_observe");
            let _ = conn.execute("UPDATE mail_source_health SET lost=1 WHERE only=1", []);
        }
        let _ = conn.execute_batch("RELEASE mail_source_observe");
    }
    let _ = conn.busy_timeout(Duration::from_millis(timeout));
}

/// `new` is supplied only on actual insertion; closures retain provenance.
pub(crate) fn observe(conn: &Connection, id: &str, new: Option<&Provenance>) {
    best_effort(conn, || {
        let Some(f) = fact(conn, id, new)? else {
            return Ok(());
        };
        let text = serde_json::to_string(&f)?;
        let (entries, bytes): (i64, i64) = conn.query_row(
            "SELECT entries,bytes FROM mail_source_health WHERE only=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        ensure!(
            entries < LIMIT && bytes + text.len() as i64 <= BYTES && text.len() <= ITEM_BYTES,
            "outbox full"
        );
        remember(conn, &f)?;
        conn.execute(
            "INSERT INTO mail_source_outbox(intent,event) VALUES(?1,?2)",
            params![uuid::Uuid::new_v4().to_string(), text],
        )?;
        conn.execute(
            "UPDATE mail_source_health SET entries=entries+1,bytes=bytes+?1 WHERE only=1",
            [text.len() as i64],
        )?;
        Ok(())
    });
}

/// Ack is a bounded set of new observation events, not a lifecycle closure.
/// Large legacy acknowledgments explicitly lose coverage, never block mail.
pub(crate) fn acknowledged(conn: &Connection, after: i64, through: i64) {
    best_effort(conn, || {
        let mut stmt = conn.prepare("SELECT id FROM mail WHERE recipient='captain' AND seq>?1 AND seq<=?2 ORDER BY seq LIMIT 51")?;
        let ids = stmt
            .query_map(params![after, through], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(ids.len() <= 50, "ack coverage overflow");
        for id in ids {
            observe(conn, &id, None);
        }
        Ok(())
    });
}

/// One bounded home transaction freezes retained history AND its delta upper.
/// All retained captain conversations are included (no timestamp cutoff). If
/// the bounded history cannot fit, the transaction rolls back, never omitting
/// old unread/open facts to make a successful-looking baseline.
pub(crate) fn snapshot(conn: &Connection, binding: &str) -> Result<i64> {
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    if let Some((old, upper)) = tx
        .query_row(
            "SELECT binding,upper_seq FROM mail_source_snapshot WHERE only=1",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?
    {
        ensure!(old == binding, "stale_generation");
        return Ok(upper);
    }
    let upper: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq),0) FROM mail_source_outbox",
        [],
        |r| r.get(0),
    )?;
    let mut stmt = tx.prepare("SELECT id FROM mail WHERE sender='captain' OR recipient='captain' ORDER BY seq LIMIT 10001")?;
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        ids.len() <= LIMIT as usize,
        "backpressure: retained history"
    );
    let mut bytes = 0;
    for id in ids {
        let f = fact(&tx, &id, None)?.context("snapshot mail vanished")?;
        let event = serde_json::to_string(&f)?;
        bytes += event.len() as i64;
        ensure!(
            bytes <= BYTES && event.len() <= ITEM_BYTES,
            "backpressure: retained history bytes"
        );
        remember(&tx, &f)?;
        tx.execute(
            "INSERT INTO mail_source_import(intent,event) VALUES(?1,?2)",
            params![uuid::Uuid::new_v4().to_string(), event],
        )?;
    }
    drop(stmt);
    tx.execute("INSERT INTO mail_source_snapshot(only,binding,upper_seq,copied_through) VALUES(1,?1,?2,?2)", params![binding,upper])?;
    tx.commit()?;
    Ok(upper)
}

/// After the immutable baseline, copy ordered deltas into durable import
/// intents. Advancing this local pointer is atomic with the copied intents;
/// it is NOT the registry receipt. Neither log nor pending intent is pruned.
pub(crate) fn stage_deltas(conn: &Connection) -> Result<()> {
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let through: i64 = tx.query_row(
        "SELECT copied_through FROM mail_source_snapshot WHERE only=1",
        [],
        |r| r.get(0),
    )?;
    let mut stmt = tx.prepare(
        "SELECT seq,intent,event FROM mail_source_outbox WHERE seq>?1 ORDER BY seq LIMIT 50",
    )?;
    let rows = stmt
        .query_map([through], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut next = through;
    for (seq, intent, event) in rows {
        ensure!(seq == next + 1, "history_gap: outbox sequence");
        tx.execute(
            "INSERT OR IGNORE INTO mail_source_import(intent,event) VALUES(?1,?2)",
            params![intent, event],
        )?;
        next = seq;
    }
    drop(stmt);
    tx.execute(
        "UPDATE mail_source_snapshot SET copied_through=?1 WHERE only=1",
        [next],
    )?;
    tx.commit()?;
    Ok(())
}

/// Explicit same-machine copy evidence; never reconstruct from equal IDs.
pub(crate) fn copy_provenance(conn: &Connection, id: &str) -> Provenance {
    let result = (|| -> Result<Provenance> {
        let raw: String = conn.query_row(
            "SELECT provenance FROM mail_source_origin WHERE id=?1",
            [id],
            |r| r.get(0),
        )?;
        Ok(match serde_json::from_str(&raw)? {
            Provenance::Local => Provenance::LocalCopy {
                mailbox: conn.query_row(
                    "SELECT mailbox FROM mail_source_health WHERE only=1",
                    [],
                    |r| r.get(0),
                )?,
                id: id.into(),
            },
            other => other,
        })
    })();
    result.unwrap_or(Provenance::Unresolved)
}

#[cfg(test)]
mod tests;
