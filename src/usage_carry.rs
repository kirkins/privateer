//! Usage a reaching machine carries on its `fleet.collect`.
//!
//! Links are one way: the machine that is reached cannot ask the one that
//! reaches it. So each collect carries the reaching machine's settled harness
//! jobs after a cursor, read from its own homes only and bounded per contact.
//! The reached machine keeps them per machine in its registry, idempotent by
//! (machine, job id), and answers with the cursor it stored; the sender
//! advances only to that. A carry that cannot be kept never stops the mail.
use crate::store::{Caller, Store};
use crate::usage::JobUsage;
use anyhow::{Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// Rows carried on one contact, and roughly their size beside the snapshot
/// section in one gateway line; the rest go on the next contact.
pub(crate) const CARRY_ROWS: usize = 2_000;
const CARRY_BYTES: usize = 384 * 1024;
/// A job is carried once it settled this long ago, so a record committing
/// just after the read is not passed by the cursor.
const SETTLE_MS: i64 = 30_000;
/// Carried rows are kept this long, and at most this many per machine.
const KEEP_MS: i64 = 90 * 86_400_000;
const KEEP_ROWS: i64 = 200_000;
/// The whole read of this machine's homes for one contact.
const LOCAL_BUDGET: Duration = Duration::from_secs(3);

pub(crate) fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS fleet_usage(machine TEXT NOT NULL, job_id TEXT NOT NULL,
           finished_ms INTEGER NOT NULL, route TEXT, ship TEXT NOT NULL, usage TEXT,
           partial INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(machine, job_id));
         CREATE INDEX IF NOT EXISTS fleet_usage_finished ON fleet_usage(finished_ms);
         CREATE INDEX IF NOT EXISTS fleet_usage_machine ON fleet_usage(machine, finished_ms);
         CREATE TABLE IF NOT EXISTS fleet_usage_contacts(machine TEXT PRIMARY KEY,
           cursor_ms INTEGER, cursor_id TEXT, as_of_ms INTEGER, received_ms INTEGER NOT NULL);",
    )?;
    Ok(())
}

/// A position in a machine's settled jobs: (finished_ms, job id).
type Cursor = (i64, String);

fn cursor_json(cursor: Option<&Cursor>) -> Value {
    cursor.map_or(
        Value::Null,
        |(ms, id)| json!({"finished_ms": ms, "job_id": id}),
    )
}

fn parse_cursor(value: &Value) -> Result<Option<Cursor>> {
    match value {
        Value::Null => Ok(None),
        Value::Object(_) => match (value["finished_ms"].as_i64(), value["job_id"].as_str()) {
            (Some(ms), Some(id)) if id.len() <= 120 => Ok(Some((ms, id.to_owned()))),
            _ => bail!("a usage cursor needs finished_ms and job_id"),
        },
        other => bail!("a usage cursor must be an object or null, not {other}"),
    }
}

/// The cursor the machine this one reaches last stored for it.
pub(crate) fn sent_cursor(conn: &Connection, machine: &str) -> Result<Option<Cursor>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT usage_cursor FROM machines WHERE name=?1",
            [machine],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(raw
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| parse_cursor(&value).ok().flatten()))
}

/// Take the cursor a reached machine answered it stored. It may be behind
/// the one sent, when that machine lost what it had: the next contact
/// carries from there again.
pub(crate) fn advance(conn: &Connection, machine: &str, answered: &Value) -> Result<()> {
    let cursor = parse_cursor(&answered["cursor"])?;
    conn.execute(
        "UPDATE machines SET usage_cursor=?2 WHERE name=?1",
        params![
            machine,
            cursor.as_ref().map(|c| cursor_json(Some(c)).to_string())
        ],
    )?;
    Ok(())
}

/// This machine's own jobs after `after` for one contact, bounded, oldest
/// first. A first contact starts as far back as carried rows are kept.
/// `through` is the last row carried, and `as_of_ms` the time through which
/// the jobs are complete on this clock. A home that cannot be read is named
/// in `unknown`; the other homes' jobs are carried all the same, but the
/// cursor stays where it was and no time is given, since it must not pass
/// the unread home's jobs. The reached machine keeps them by job id, so
/// carrying them again once that home reads counts each once.
pub(crate) fn outgoing(conn: &Connection, after: Option<&Cursor>) -> Result<Value> {
    let now = Store::now_ms();
    let before_ms = now - SETTLE_MS;
    let start = after
        .cloned()
        .unwrap_or_else(|| (now - KEEP_MS, String::new()));
    let started = Instant::now();
    let same = |home: &std::path::Path| home.canonicalize().unwrap_or_else(|_| home.to_owned());
    let mut seen = BTreeSet::new();
    let mut jobs = vec![];
    let mut more = false;
    let mut unknown = vec![];
    for member in crate::fleet::members(conn)? {
        // Two ships sharing a home share its jobs: read them once.
        if !member.home.is_dir() || !seen.insert(same(&member.home)) {
            continue;
        }
        if started.elapsed() >= LOCAL_BUDGET {
            unknown.push(format!("home:{}", member.name));
            continue;
        }
        let read = Store::open_readonly(&member.home, Caller::Operator).and_then(|store| {
            crate::usage::home_jobs_after(
                &store,
                &member.name,
                (start.0, &start.1),
                before_ms,
                CARRY_ROWS,
            )
        });
        match read {
            Ok(read) => {
                more |= read.len() >= CARRY_ROWS;
                jobs.extend(read);
            }
            Err(_) => unknown.push(format!("home:{}", member.name)),
        }
    }
    // Each home gave its oldest; the oldest of them all are carried, so
    // nothing a home did not give lies before the last one carried.
    jobs.sort_by(|a, b| (a.finished_ms, &a.job_id).cmp(&(b.finished_ms, &b.job_id)));
    more |= jobs.len() > CARRY_ROWS;
    let mut rows = vec![];
    let mut bytes = 0;
    let mut through = after.cloned();
    for job in jobs.iter().take(CARRY_ROWS) {
        let row = job.to_carry();
        bytes += row.to_string().len();
        if bytes > CARRY_BYTES {
            more = true;
            break;
        }
        through = Some((job.finished_ms, job.job_id.clone()));
        rows.push(row);
    }
    if !unknown.is_empty() {
        return Ok(
            json!({"after": cursor_json(after), "through": cursor_json(after),
            "as_of_ms": null, "rows": rows, "unknown": unknown}),
        );
    }
    let as_of_ms = if more {
        through.as_ref().map(|(ms, _)| *ms)
    } else {
        Some(before_ms)
    };
    Ok(
        json!({"after": cursor_json(after), "through": cursor_json(through.as_ref()),
        "as_of_ms": as_of_ms, "rows": rows}),
    )
}

/// Keep the jobs a reaching machine carried, and answer the cursor stored
/// for it. Rows replace any kept under the same job id. The cursor moves to
/// what was carried only when the carry begins at or before the stored one;
/// otherwise this registry lost rows the sender passed, and the stored one
/// is answered so it carries them again. A row that cannot be read is
/// skipped and counted; the others are kept. A carry naming homes it could
/// not read is kept too, but leaves the contact as it was: the sender is not
/// dated as if all of it had been carried.
pub(crate) fn absorb(conn: &Connection, machine: &str, carried: &Value) -> Result<Value> {
    let after = parse_cursor(&carried["after"])?;
    let through = parse_cursor(&carried["through"])?;
    let rows = carried["rows"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("carried usage needs rows"))?;
    ensure!(rows.len() <= CARRY_ROWS, "carried usage exceeds its bound");
    let jobs: Vec<JobUsage> = rows
        .iter()
        .filter_map(|row| JobUsage::from_carry(row).ok())
        .collect();
    let skipped = rows.len() - jobs.len();
    let partial = carried["unknown"].as_array().is_some_and(|u| !u.is_empty());
    let now = Store::now_ms();
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    for job in &jobs {
        tx.execute(
            "INSERT INTO fleet_usage(machine, job_id, finished_ms, route, ship, usage, partial)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(machine, job_id) DO UPDATE SET finished_ms=excluded.finished_ms,
               route=excluded.route, ship=excluded.ship, usage=excluded.usage,
               partial=excluded.partial",
            params![
                machine,
                job.job_id,
                job.finished_ms,
                job.route,
                job.ship,
                job.usage.map(|_| job.usage_json().to_string()),
                job.partial
            ],
        )?;
    }
    let stored: Option<(Option<i64>, Option<String>, Option<i64>)> = tx
        .query_row(
            "SELECT cursor_ms, cursor_id, as_of_ms FROM fleet_usage_contacts WHERE machine=?1",
            [machine],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let (stored_cursor, stored_as_of) = match stored {
        Some((Some(ms), Some(id), as_of)) => (Some((ms, id)), as_of),
        Some((_, _, as_of)) => (None, as_of),
        None => (None, None),
    };
    let contiguous = match (&after, &stored_cursor) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(after), Some(stored)) => after <= stored,
    };
    let cursor = if partial {
        stored_cursor
    } else {
        let (cursor, as_of_ms) = if contiguous {
            (
                through.max(stored_cursor),
                carried["as_of_ms"].as_i64().or(stored_as_of),
            )
        } else {
            (stored_cursor, stored_as_of)
        };
        tx.execute(
            "INSERT INTO fleet_usage_contacts(machine, cursor_ms, cursor_id, as_of_ms, received_ms)
             VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(machine) DO UPDATE SET cursor_ms=excluded.cursor_ms,
               cursor_id=excluded.cursor_id, as_of_ms=excluded.as_of_ms,
               received_ms=excluded.received_ms",
            params![
                machine,
                cursor.as_ref().map(|c| c.0),
                cursor.as_ref().map(|c| c.1.clone()),
                as_of_ms,
                now
            ],
        )?;
        cursor
    };
    tx.execute(
        "DELETE FROM fleet_usage WHERE finished_ms < ?1",
        [now - KEEP_MS],
    )?;
    tx.execute(
        "DELETE FROM fleet_usage WHERE machine=?1 AND rowid IN (SELECT rowid FROM fleet_usage
           WHERE machine=?1 ORDER BY finished_ms DESC LIMIT -1 OFFSET ?2)",
        params![machine, KEEP_ROWS],
    )?;
    tx.commit()?;
    Ok(json!({"cursor": cursor_json(cursor.as_ref()), "skipped": skipped}))
}

/// When a reaching machine last carried its usage here.
pub(crate) struct Contact {
    /// Through when its jobs are complete, on its own clock.
    pub as_of_ms: Option<i64>,
    /// When this registry received it, on this clock.
    received_ms: i64,
}

impl Contact {
    /// Received here within two contacts. A receive time ahead of `now`
    /// (this clock stepped back) is not fresh: the next contact dates it.
    pub(crate) fn fresh(&self, now: i64) -> bool {
        self.received_ms <= now && now - self.received_ms < 2 * crate::machines::CONTACT_MS
    }
}

/// The jobs a reaching machine carried, settled in the period on its clock,
/// and its last contact.
pub(crate) fn carried(
    conn: &Connection,
    machine: &str,
    since_ms: i64,
    until_ms: Option<i64>,
) -> Result<(Vec<JobUsage>, Option<Contact>)> {
    let contact = conn
        .query_row(
            "SELECT as_of_ms, received_ms FROM fleet_usage_contacts WHERE machine=?1",
            [machine],
            |r| {
                Ok(Contact {
                    as_of_ms: r.get(0)?,
                    received_ms: r.get(1)?,
                })
            },
        )
        .optional()?;
    let mut statement = conn.prepare(
        "SELECT job_id, finished_ms, route, ship, usage, partial FROM fleet_usage
         WHERE machine=?1 AND finished_ms >= ?2 AND finished_ms < ?3",
    )?;
    let rows = statement.query_map(
        params![machine, since_ms, until_ms.unwrap_or(i64::MAX)],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, bool>(5)?,
            ))
        },
    )?;
    let mut jobs = vec![];
    for row in rows {
        let (job_id, finished_ms, route, ship, usage, partial) = row?;
        let usage = usage
            .and_then(|u| serde_json::from_str::<Value>(&u).ok())
            .unwrap_or(Value::Null);
        jobs.push(JobUsage::from_carry(&json!({"job_id": job_id,
            "finished_ms": finished_ms, "route": route, "ship": ship, "usage": usage,
            "partial": partial}))?);
    }
    Ok((jobs, contact))
}

pub(crate) fn forget(conn: &Connection, machine: &str) -> Result<()> {
    conn.execute("DELETE FROM fleet_usage WHERE machine=?1", [machine])?;
    conn.execute(
        "DELETE FROM fleet_usage_contacts WHERE machine=?1",
        [machine],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("fleet.sqlite")).unwrap();
        init(&conn).unwrap();
        (dir, conn)
    }

    fn row(id: &str, at: i64, tokens: u64) -> Value {
        json!({"job_id": id, "finished_ms": at, "route": "opencode/zai/glm", "ship": "liaison",
            "harness": "opencode", "partial": false, "usage": {"input": tokens, "output": 0,
            "cacheRead": 0, "cacheWrite": 0, "totalTokens": tokens}})
    }

    fn carry(after: Value, rows: Vec<Value>, as_of_ms: i64) -> Value {
        let through = rows.last().map_or(
            after.clone(),
            |last| json!({"finished_ms": last["finished_ms"], "job_id": last["job_id"]}),
        );
        json!({"after": after, "through": through, "as_of_ms": as_of_ms, "rows": rows})
    }

    #[test]
    fn carrying_the_same_rows_again_keeps_one_of_each_and_the_latest_cursor() {
        let (_dir, conn) = registry();
        let now = Store::now_ms();
        let first = carry(
            Value::Null,
            vec![row("a", now, 1), row("b", now + 1, 2)],
            now + 5,
        );
        let answered = absorb(&conn, "laptop", &first).unwrap();
        assert_eq!(
            answered["cursor"],
            json!({"finished_ms": now + 1, "job_id": "b"})
        );
        // The answer was lost: the sender carries the same rows again.
        assert_eq!(absorb(&conn, "laptop", &first).unwrap(), answered);
        let (jobs, contact) = carried(&conn, "laptop", 0, None).unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(contact.unwrap().as_of_ms, Some(now + 5));
        // The next carry starts at the stored cursor and moves it on.
        let next = carry(
            answered["cursor"].clone(),
            vec![row("c", now + 2, 3)],
            now + 9,
        );
        let moved = absorb(&conn, "laptop", &next).unwrap();
        assert_eq!(moved["cursor"]["job_id"], "c");
        // A late, older carry never moves the cursor back.
        assert_eq!(absorb(&conn, "laptop", &first).unwrap(), moved);
        assert_eq!(carried(&conn, "laptop", 0, None).unwrap().0.len(), 3);
    }

    #[test]
    fn a_carry_past_what_this_registry_kept_answers_the_cursor_it_has() {
        let (_dir, conn) = registry();
        let now = Store::now_ms();
        let after = json!({"finished_ms": now, "job_id": "z"});
        let answered = absorb(
            &conn,
            "laptop",
            &carry(after, vec![row("y", now + 1, 1)], now + 2),
        )
        .unwrap();
        assert_eq!(
            answered["cursor"],
            Value::Null,
            "carry again from the start"
        );
        // What came is kept all the same, and counts once when carried again.
        assert_eq!(carried(&conn, "laptop", 0, None).unwrap().0.len(), 1);
        absorb(
            &conn,
            "laptop",
            &carry(Value::Null, vec![row("y", now + 1, 1)], now + 3),
        )
        .unwrap();
        assert_eq!(carried(&conn, "laptop", 0, None).unwrap().0.len(), 1);
    }

    #[test]
    fn old_rows_and_rows_beyond_the_cap_are_pruned() {
        let (_dir, conn) = registry();
        let now = Store::now_ms();
        let old = carry(Value::Null, vec![row("old", now - KEEP_MS - 1, 1)], now);
        absorb(&conn, "laptop", &old).unwrap();
        let fresh = carry(Value::Null, vec![row("new", now, 1)], now);
        absorb(&conn, "laptop", &fresh).unwrap();
        let (jobs, _) = carried(&conn, "laptop", 0, None).unwrap();
        assert_eq!(
            jobs.iter().map(|j| j.job_id.as_str()).collect::<Vec<_>>(),
            ["new"]
        );
    }

    #[test]
    fn a_carry_with_too_many_rows_keeps_nothing() {
        let (_dir, conn) = registry();
        let now = Store::now_ms();
        let many = (0..=CARRY_ROWS)
            .map(|n| row(&format!("j{n}"), now, 1))
            .collect();
        assert!(absorb(&conn, "laptop", &carry(Value::Null, many, now)).is_err());
        let (jobs, contact) = carried(&conn, "laptop", 0, None).unwrap();
        assert!(jobs.is_empty() && contact.is_none());
    }

    #[test]
    fn a_bad_row_is_skipped_and_counted_and_the_rest_are_kept() {
        let (_dir, conn) = registry();
        let now = Store::now_ms();
        let bad = carry(
            Value::Null,
            vec![
                row("a", now, 1),
                json!({"job_id": "b"}),
                row("c", now + 1, 2),
            ],
            now + 5,
        );
        let answered = absorb(&conn, "laptop", &bad).unwrap();
        assert_eq!(answered["skipped"], 1, "{answered}");
        assert_eq!(answered["cursor"]["job_id"], "c", "{answered}");
        let (jobs, contact) = carried(&conn, "laptop", 0, None).unwrap();
        assert_eq!(
            jobs.iter().map(|j| j.job_id.as_str()).collect::<Vec<_>>(),
            ["a", "c"]
        );
        assert_eq!(contact.unwrap().as_of_ms, Some(now + 5));
    }

    #[test]
    fn a_carry_missing_a_home_keeps_its_rows_but_not_the_contact() {
        let (_dir, conn) = registry();
        let now = Store::now_ms();
        let first = carry(Value::Null, vec![row("a", now, 1)], now + 5);
        let stored = absorb(&conn, "laptop", &first).unwrap();
        conn.execute(
            "UPDATE fleet_usage_contacts SET received_ms=received_ms-600000",
            [],
        )
        .unwrap();
        // A home there could not be read: the rest of its homes' rows come,
        // with the cursor held where it was and no time they are complete to.
        let partial = json!({"after": stored["cursor"], "through": stored["cursor"],
            "as_of_ms": null, "rows": [row("b", now + 1, 2)], "unknown": ["home:rig"]});
        let answered = absorb(&conn, "laptop", &partial).unwrap();
        assert_eq!(answered["cursor"], stored["cursor"]);
        let (jobs, contact) = carried(&conn, "laptop", 0, None).unwrap();
        assert_eq!(jobs.len(), 2);
        let contact = contact.unwrap();
        assert_eq!(contact.as_of_ms, Some(now + 5));
        assert!(
            !contact.fresh(Store::now_ms()),
            "dated as if all was carried"
        );
    }

    #[test]
    fn a_contact_is_fresh_within_two_contacts_on_this_clock() {
        let contact = Contact {
            as_of_ms: Some(1),
            received_ms: 1_000_000,
        };
        assert!(contact.fresh(1_000_000 + 2 * crate::machines::CONTACT_MS - 1));
        assert!(!contact.fresh(1_000_000 + 2 * crate::machines::CONTACT_MS));
        assert!(!contact.fresh(999_999), "received ahead of this clock");
    }
}
