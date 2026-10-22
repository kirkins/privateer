//! Open tasks nothing has touched for the ship's `first_mate.stale_task_days`
//! (default 3, 0 shows none): one warning line in its bearings and a count on
//! the bridge deck. Read-only: it never changes a task, blocks or mails.
//!
//! A task's last update is the latest of what the store already records
//! about it: its `task.*` audit rows (create, update, rank, defer, enqueue,
//! handoff, accept, share), the starts of its jobs (`job.started`), its notes
//! (`task:<id>`, `assignment:<id>`), its pins and its mail, crew reports included. A task waiting
//! on a typed wait (`task.defer`) is left out: the wait is its update, and
//! the remote-wait staleness check already watches the long ones. The duty
//! task is ongoing by design.
use anyhow::Result;
use rusqlite::{Connection, params};

pub(crate) const DEFAULT_DAYS: u64 = 3;
const DAY_MS: i64 = 86_400_000;
/// Tasks the bearings line names; the rest are counted.
const NAMED: usize = 5;

pub(crate) struct Stale {
    pub days: i64,
    pub total: usize,
    /// The oldest first, at most `NAMED`.
    pub oldest: Vec<Task>,
}

pub(crate) struct Task {
    pub id: String,
    pub status: String,
    /// The task has a pinned `waiting` fact (no typed wait).
    pub waiting: bool,
    /// None when nothing about it is recorded any more.
    pub last_ms: Option<i64>,
}

/// One query: the ship's open tasks with no update since `now` less its
/// threshold, oldest first, and how many there are. None for a home from
/// before the tables it reads.
pub(crate) fn read(conn: &Connection, ship: &str, now: i64) -> Result<Option<Stale>> {
    let present: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table'
           AND name IN ('ships','mail','notes','pins','task_schedule')",
        [],
        |r| r.get(0),
    )?;
    if present < 5 {
        return Ok(None);
    }
    let mut statement = conn.prepare_cached(
        "WITH days(n) AS (SELECT COALESCE((SELECT json_extract(record,'$.first_mate.stale_task_days')
             FROM ships WHERE name=?1), ?3)),
         open(id, status) AS MATERIALIZED (SELECT t.id, t.status FROM tasks t
           WHERE t.owner=?1 AND t.status NOT IN ('done','cancelled') AND t.id <> ?1 || '-duty'
             AND NOT EXISTS (SELECT 1 FROM task_schedule s WHERE s.task_id=t.id
               AND json_extract(s.record,'$.state')='waiting')),
         touched(id, ms) AS (
           SELECT substr(focus, instr(focus, ':') + 1), created_ms FROM notes
             WHERE ship=?1 AND (focus IN (SELECT 'task:' || id FROM open)
               OR focus IN (SELECT 'assignment:' || id FROM open))
           UNION ALL SELECT substr(focus, 6), set_ms FROM pins
             WHERE ship=?1 AND focus IN (SELECT 'task:' || id FROM open)
           UNION ALL SELECT task_id, MAX(created_ms, COALESCE(answered_ms, 0)) FROM mail
             WHERE task_id IN (SELECT id FROM open)
           UNION ALL SELECT json_extract(payload,
               CASE kind WHEN 'job.started' THEN '$.task_id' ELSE '$.id' END), time_ms FROM audit
             WHERE (kind='job.started' OR (kind >= 'task.' AND kind < 'task/'))
               AND json_extract(payload, CASE kind WHEN 'job.started' THEN '$.task_id' ELSE '$.id' END)
                 IN (SELECT id FROM open)),
         stale(id, status, last) AS (SELECT o.id, o.status, MAX(t.ms) FROM open o
           LEFT JOIN touched t ON t.id=o.id GROUP BY o.id, o.status)
         SELECT n, s.id, s.status, s.last, COUNT(*) OVER (),
           EXISTS(SELECT 1 FROM pins p WHERE p.ship=?1 AND p.focus='task:' || s.id AND p.key='waiting')
         FROM days LEFT JOIN stale s ON n > 0 AND (s.last IS NULL OR s.last <= ?2 - n * 86400000)
         ORDER BY s.last IS NOT NULL, s.last, s.id LIMIT ?4",
    )?;
    let mut days = DEFAULT_DAYS as i64;
    let mut total = 0;
    let mut oldest = Vec::new();
    let mut rows = statement.query(params![ship, now, DEFAULT_DAYS, NAMED])?;
    while let Some(row) = rows.next()? {
        days = row.get(0)?;
        let Some(id) = row.get::<_, Option<String>>(1)? else {
            continue;
        };
        total = row.get::<_, i64>(4)? as usize;
        oldest.push(Task {
            id,
            status: row.get(2)?,
            last_ms: row.get(3)?,
            waiting: row.get(5)?,
        });
    }
    Ok(Some(Stale {
        days,
        total,
        oldest,
    }))
}

/// The bearings warning, or None when no open task is stale.
pub(crate) fn line(stale: &Stale, now: i64) -> Option<String> {
    if stale.total == 0 {
        return None;
    }
    let named: Vec<String> = stale
        .oldest
        .iter()
        .map(|task| {
            let mut notes = vec![match task.last_ms {
                Some(ms) => format!("{} d", (now - ms).max(0) / DAY_MS),
                None => "no recorded update".into(),
            }];
            if task.status != "open" {
                notes.push(task.status.clone());
            }
            if task.waiting {
                notes.push("waiting".into());
            }
            format!("{} ({})", task.id, notes.join(", "))
        })
        .collect();
    let more = stale.total.saturating_sub(named.len());
    let (count, verb) = match stale.total {
        1 => ("1 open task".to_owned(), "has"),
        n => (format!("{n} open tasks"), "have"),
    };
    Some(format!(
        "{count} {verb} had no update in {} days or more: {}{}. Update, defer, hand off or close {}; this is a warning only.",
        stale.days,
        named.join(", "),
        if more > 0 {
            format!(", and {more} more")
        } else {
            String::new()
        },
        if stale.total == 1 { "it" } else { "them" },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_names_the_oldest_five_with_their_state_and_counts_the_rest() {
        let now = 10 * DAY_MS;
        let task = |id: &str, status: &str, waiting, days: Option<i64>| Task {
            id: id.into(),
            status: status.into(),
            waiting,
            last_ms: days.map(|d| now - d * DAY_MS - 1),
        };
        let stale = Stale {
            days: 3,
            total: 7,
            oldest: vec![
                task("a", "open", false, None),
                task("b", "blocked", false, Some(6)),
                task("c", "open", true, Some(5)),
                task("d", "running", false, Some(4)),
                task("e", "open", false, Some(3)),
            ],
        };
        assert_eq!(
            line(&stale, now).unwrap(),
            "7 open tasks have had no update in 3 days or more: a (no recorded update), \
             b (6 d, blocked), c (5 d, waiting), d (4 d, running), e (3 d), and 2 more. \
             Update, defer, hand off or close them; this is a warning only."
        );
        let one = Stale {
            days: 3,
            total: 1,
            oldest: vec![task("x", "open", false, Some(4))],
        };
        assert!(
            line(&one, now)
                .unwrap()
                .starts_with("1 open task has had no update in 3 days or more: x (4 d).")
        );
        let none = Stale {
            days: 3,
            total: 0,
            oldest: vec![],
        };
        assert_eq!(line(&none, now), None);
    }
}
