//! A first mate held off every route on its first_mate list while wake-worthy
//! mail waits. The dispatcher pass records the fact once that mail has waited
//! past `max_delay_secs`, and removes it at the next pass that finds a route
//! clear, the mail gone or the first mate otherwise attended to; runtime.needs
//! shows it to the captain. Nothing is mailed.
use crate::store::Store;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

pub(crate) fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS first_mate_waits(ship TEXT PRIMARY KEY, why TEXT NOT NULL,
           mail_since_ms INTEGER NOT NULL, checked_ms INTEGER NOT NULL);",
    )?;
    Ok(())
}

/// Whether this home has the table, as a read-only observation finds it.
pub(crate) fn available(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='first_mate_waits')",
        [],
        |r| r.get(0),
    )?)
}

/// `ship`'s first mate has no route to take (`why` gives each listed route's
/// reason) and wake-worthy mail waiting since `mail_since_ms`.
pub(crate) fn record(store: &Store, ship: &str, why: &str, mail_since_ms: i64) -> Result<()> {
    let why: String = why.chars().take(4000).collect();
    store.connection().execute(
        "INSERT INTO first_mate_waits(ship, why, mail_since_ms, checked_ms) VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(ship) DO UPDATE SET why=excluded.why, mail_since_ms=excluded.mail_since_ms,
           checked_ms=excluded.checked_ms",
        params![ship, why, mail_since_ms, Store::now_ms()],
    )?;
    Ok(())
}

/// The first mate is not waiting on a route, or has nothing overdue to wake for.
pub(crate) fn clear(store: &Store, ship: &str) -> Result<()> {
    store
        .connection()
        .execute("DELETE FROM first_mate_waits WHERE ship=?1", [ship])?;
    Ok(())
}

/// The runtime.needs item for a recorded wait, read inside the observation's
/// transaction.
pub(crate) fn need_item(conn: &Connection, ship: &str, now: i64) -> Result<Value> {
    let (why, mail_since_ms, checked_ms): (String, i64, i64) = conn
        .query_row(
            "SELECT why, mail_since_ms, checked_ms FROM first_mate_waits WHERE ship=?1",
            [ship],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .context("first mate wait vanished")?;
    Ok(json!({
        "kind": "blocker",
        "evidence": "first_mate_no_route",
        "ship": ship,
        "status": "waiting",
        "summary": format!(
            "{ship}'s first mate has wake-worthy mail waiting and no route to take: {why}. It wakes when one clears; a model of its own (ship.put) or a route added to the first_mate list frees it now."
        ),
        "created_ms": mail_since_ms,
        "waited_ms": now.saturating_sub(mail_since_ms),
        "checked_ms": checked_ms,
        // Not the reasons, which count down a backoff's seconds: the same
        // wait is the same need from one pass to the next.
        "version": crate::core::hash(&json!([ship, mail_since_ms])),
        "actions": [],
    }))
}
