//! The backlog alarm: a quiet first mate whose open tasks nothing will start
//! is mailed one notice listing them, at most once an interval, backing off
//! while its wakes move nothing. The notice is ordinary mail, so the existing
//! paths wake the first mate; the alarm never starts a task itself. Design:
//! docs/design/backlog-alarm.md.
use super::*;
use std::collections::HashSet;

/// How often a quiet ship may be mailed about its backlog, in seconds.
pub(crate) const DEFAULT_SECS: u64 = 3_600;
/// Unproductive wakes back off to this spacing, or the interval if longer.
const MAX_BACKOFF_SECS: i64 = 8 * 3_600;
/// Candidates read per ship; the notice lists the first few and counts the rest.
const CANDIDATES: i64 = 100;
const LISTED: usize = 10;
const TITLE_CHARS: usize = 80;
const REASON_CHARS: usize = 120;
/// Live work under a ship, as the scheduler counts it, plus a crew parked on
/// its own heavy job.
const LIVE: &str = "'starting','queued','launching','running','orphaned','parked'";

pub(super) fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS backlog_alarms(ship TEXT PRIMARY KEY, fired_ms INTEGER NOT NULL,
           next_ms INTEGER NOT NULL, backoff_secs INTEGER NOT NULL, fires INTEGER NOT NULL,
           quiet INTEGER NOT NULL, evaluated INTEGER NOT NULL CHECK(evaluated IN (0,1)),
           considered TEXT NOT NULL, candidates TEXT NOT NULL, mail_id TEXT);",
    )?;
    Ok(())
}

/// A task nothing will start, with the schedule revision it was seen at.
struct Candidate {
    id: String,
    title: String,
    rank: Option<i64>,
    revision: u64,
    label: String,
}

/// A ship's alarm row: the last fire, the spacing before the next, and what
/// that fire listed, to judge whether its wake moved anything.
struct Alarm {
    fired_ms: i64,
    next_ms: i64,
    backoff_secs: i64,
    fires: i64,
    quiet: i64,
    evaluated: bool,
    considered: Vec<(String, u64)>,
    candidates: String,
}

fn load_alarm(store: &Store, ship: &str) -> Result<Option<Alarm>> {
    let row = store
        .connection()
        .query_row(
            "SELECT fired_ms, next_ms, backoff_secs, fires, quiet, evaluated, considered, candidates
             FROM backlog_alarms WHERE ship=?1",
            [ship],
            |r| {
                let alarm = Alarm {
                    fired_ms: r.get(0)?,
                    next_ms: r.get(1)?,
                    backoff_secs: r.get(2)?,
                    fires: r.get(3)?,
                    quiet: r.get(4)?,
                    evaluated: r.get(5)?,
                    considered: vec![],
                    candidates: r.get(7)?,
                };
                Ok((alarm, r.get::<_, String>(6)?))
            },
        )
        .optional()?;
    let Some((mut alarm, considered)) = row else {
        return Ok(None);
    };
    alarm.considered = serde_json::from_str(&considered).context("invalid backlog alarm")?;
    Ok(Some(alarm))
}

fn save_alarm(store: &Store, ship: &str, alarm: &Alarm, mail_id: Option<&str>) -> Result<()> {
    store.connection().execute(
        "INSERT INTO backlog_alarms(ship, fired_ms, next_ms, backoff_secs, fires, quiet, evaluated,
           considered, candidates, mail_id) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(ship) DO UPDATE SET fired_ms=excluded.fired_ms, next_ms=excluded.next_ms,
           backoff_secs=excluded.backoff_secs, fires=excluded.fires, quiet=excluded.quiet,
           evaluated=excluded.evaluated, considered=excluded.considered,
           candidates=excluded.candidates, mail_id=COALESCE(excluded.mail_id, backlog_alarms.mail_id)",
        params![
            ship,
            alarm.fired_ms,
            alarm.next_ms,
            alarm.backoff_secs,
            alarm.fires,
            alarm.quiet,
            alarm.evaluated,
            serde_json::to_string(&alarm.considered)?,
            alarm.candidates,
            mail_id
        ],
    )?;
    Ok(())
}

fn digest(set: &[Candidate]) -> String {
    let mut pairs: Vec<(&str, u64)> = set.iter().map(|c| (c.id.as_str(), c.revision)).collect();
    pairs.sort();
    crate::core::hash(&json!(pairs))
}

/// Whether a scheduled ancestor holds this task: explicitly stopped, or under
/// a coordinator's live claim, which owns its children.
fn held_above(store: &Store, task: &str) -> Result<bool> {
    let mut next = store.task(task)?["parent"].as_str().map(str::to_owned);
    for _ in 0..128 {
        let Some(id) = next else {
            return Ok(false);
        };
        if load(store, &id)?.is_some_and(|w| {
            matches!(
                w.state.as_str(),
                "stopped" | "offered" | "claimed" | "executing"
            )
        }) {
            return Ok(true);
        }
        next = store.task(&id)?["parent"].as_str().map(str::to_owned);
    }
    // An ancestry this deep is not one to wake anybody about.
    Ok(true)
}

/// The ship's open tasks that nothing will start, next first: unscheduled
/// open ones, running ones nothing is moving (`context::idle_tasks`), and
/// faulted schedules. A task waiting on an open request the ship sent, under
/// a held ancestor, blocked, handed off or the duty task is never one.
fn candidates(store: &Store, ship: &str, duty: &str) -> Result<Vec<Candidate>> {
    let idle: HashSet<String> = crate::context::idle_tasks(store, ship)?
        .iter()
        .filter_map(|t| t["id"].as_str().map(str::to_owned))
        .collect();
    let mut statement = store.connection().prepare(
        "WITH RECURSIVE tree(root, id) AS (
           SELECT id, id FROM tasks WHERE owner=?1 AND status IN ('open','running')
           UNION SELECT tree.root, t.id FROM tasks t JOIN tree ON t.parent=tree.id)
         SELECT t.id, t.title, t.rank, t.status, json_extract(s.record,'$.revision'),
           json_extract(s.record,'$.reason')
         FROM tasks t LEFT JOIN task_schedule s ON s.task_id=t.id
         WHERE t.owner=?1 AND t.status IN ('open','running') AND t.handoff_to IS NULL
           AND t.id<>?1||'-duty' AND t.id<>?2
           AND (s.task_id IS NULL OR json_extract(s.record,'$.state')='fault')
           AND NOT EXISTS(SELECT 1 FROM tree JOIN mail m ON m.task_id=tree.id
             WHERE tree.root=t.id AND m.sender=?1 AND m.state='open')
         ORDER BY t.rank IS NULL, t.rank, t.rowid LIMIT ?3",
    )?;
    let rows = statement
        .query_map(params![ship, duty, CANDIDATES], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    let mut set = vec![];
    for (id, title, rank, status, revision, reason) in rows {
        if status == "running" && !idle.contains(&id) {
            continue;
        }
        if held_above(store, &id)? {
            continue;
        }
        let label = match (revision, status.as_str()) {
            (Some(_), _) => format!(
                "fault: {}",
                clip(reason.as_deref().unwrap_or(""), REASON_CHARS)
            ),
            (None, "running") => "running, nothing moving it".into(),
            (None, _) => "unscheduled".into(),
        };
        set.push(Candidate {
            id,
            title,
            rank,
            revision: revision.map_or(0, |r| r.max(0) as u64),
            label,
        });
    }
    Ok(set)
}

fn clip(text: &str, chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= chars {
        flat
    } else {
        format!("{}…", flat.chars().take(chars).collect::<String>())
    }
}

fn notice(set: &[Candidate], high: bool) -> String {
    let mut text =
        String::from("Backlog check: you are idle and nothing will start these tasks.\n");
    for c in set.iter().take(LISTED) {
        let rank = c.rank.map(|n| format!(" (rank {n})")).unwrap_or_default();
        text.push_str(&format!(
            "- {}{rank}: {} [{}]\n",
            c.id,
            clip(&c.title, TITLE_CHARS),
            c.label
        ));
    }
    if set.len() > LISTED {
        text.push_str(&format!(
            "... and {} more (work.status lists them).\n",
            set.len() - LISTED
        ));
    }
    let first = if high {
        "Attention is high: enqueue, defer, hand off or close only; do not start work in this conversation. Enqueued work goes to a fresh coordinator. For each"
    } else {
        "For each: write assignment:<id> and task.enqueue it; or"
    };
    text.push_str(&format!(
        "{first}{} enqueue and task.defer with a real wait; or task.update status blocked with the reason in its task note; or task.handoff; or close it with evidence. This check does not need the captain; do not mail them about it.",
        if high { ":" } else { "" }
    ));
    text
}

/// Why this ship is not quiet enough for a backlog notice, if it is not. A
/// hosted pane is quiet asleep, or awake and idle past the sleep threshold; a
/// headless ship is quiet unattended. A hosted ship without its pane waits for
/// the pane: a pane that starts asleep marks the mail already there as seen.
fn not_quiet(store: &Store, ship: &str, profile: &Value) -> Result<Option<&'static str>> {
    let presence = crate::presence::presence(store, ship)?;
    if presence["attended"] != true {
        return Ok(crate::host::hosted(profile).then_some("hosted pane not running"));
    }
    let Some(holder) = presence["lease"]["holder"]
        .as_str()
        .filter(|h| h.starts_with(crate::presence::HOSTED))
    else {
        return Ok(Some("captain's session is aboard"));
    };
    let reporting: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM work_sessions WHERE ship=?1 AND holder=?2 AND checked_ms>?3)",
        params![ship, holder, Store::now_ms() - crate::host::sleep::FRESH_MS],
        |r| r.get(0),
    )?;
    if !reporting
        || crate::host::sleep::idle_for(
            store,
            ship,
            holder,
            crate::host::sleep::DEFAULT_AFTER_SECS,
        )?
    {
        return Ok(None);
    }
    Ok(Some("first mate is working"))
}

/// Mail a headless wake would take: newer than both the acknowledgement and
/// the last wake's coverage, as `activation::consider` reads it.
fn mail_pending(store: &Store, ship: &str) -> Result<bool> {
    Ok(store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM mail WHERE recipient=?1 AND COALESCE(delivery,'received')='received'
           AND (kind<>'notice' OR urgency<>'low')
           AND seq > MAX(
             COALESCE((SELECT MAX(through_seq) FROM wakes WHERE ship=?1 AND status='succeeded'), 0),
             COALESCE((SELECT through_seq FROM mail_cursor WHERE address=?1), 0)))",
        [ship],
        |r| r.get(0),
    )?)
}

fn live(store: &Store, ship: &str) -> Result<bool> {
    Ok(store.connection().query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM jobs j JOIN tasks t ON t.id=json_extract(j.record,'$.task_id')
               WHERE t.owner=?1 AND json_extract(j.record,'$.status') IN ({LIVE}))
             OR EXISTS(SELECT 1 FROM crew_parks WHERE ship=?1 AND state IN ('parked','held','resuming'))"
        ),
        [ship],
        |r| r.get(0),
    )?)
}

fn interval_secs(profile: &Value) -> i64 {
    // Ships recorded before the setting existed have the alarm on too.
    profile
        .get("backlog_alarm_secs")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_SECS as i64)
}

fn skipped(ship: &str, reason: &str) -> Value {
    json!({"ship": ship, "skipped": reason})
}

fn consider(store: &Store, ship: &str, profile: &Value) -> Result<Value> {
    let interval = interval_secs(profile);
    if interval <= 0 {
        return Ok(skipped(ship, "backlog alarm off"));
    }
    let now = Store::now_ms();
    // A ship first seen (an upgrade, or a new ship) counts as just fired, so
    // its first notice comes one interval later, never on this tick.
    let known: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM backlog_alarms WHERE ship=?1)",
        [ship],
        |r| r.get(0),
    )?;
    if !known
        && store.connection().execute(
            "INSERT INTO backlog_alarms(ship, fired_ms, next_ms, backoff_secs, fires, quiet,
               evaluated, considered, candidates) VALUES(?1, ?2, ?3, ?4, 0, 0, 1, '[]', ?5)
             ON CONFLICT(ship) DO NOTHING",
            params![ship, now, now + interval * 1000, interval, digest(&[])],
        )? == 1
    {
        return Ok(
            json!({"ship": ship, "skipped": "first backlog check", "next_ms": now + interval * 1000}),
        );
    }
    let ledger = crate::jobs::host_ledger(store)?;
    if crate::jobs::stowed(&ledger)?.is_some() {
        return Ok(skipped(ship, "stowed"));
    }
    if crate::permissions::paused(store, ship)? {
        return Ok(skipped(
            ship,
            "tool permission question awaits captain or a policy change",
        ));
    }
    if live(store, ship)? {
        return Ok(skipped(ship, "work is live"));
    }
    let awake: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM wakes WHERE ship=?1 AND status IN ('offered','running'))",
        [ship],
        |r| r.get(0),
    )?;
    if awake {
        return Ok(skipped(ship, "first mate already awake"));
    }
    if let Some(reason) = not_quiet(store, ship, profile)? {
        return Ok(skipped(ship, reason));
    }
    if crate::activation::own_route_blocked(store, profile)? {
        return Ok(skipped(ship, "model route backing off"));
    }
    if has_ready(store, ship)? {
        return Ok(skipped(ship, "scheduled work is ready"));
    }
    if !crate::host::hosted(profile) && mail_pending(store, ship)? {
        return Ok(skipped(ship, "mail will wake it"));
    }
    let duty = profile["duty_task"].as_str().unwrap_or("");
    let high = crate::attention::summary_for(store, ship)?["pressure"] == "high";

    let tx = store.write_transaction()?;
    let mut alarm = load_alarm(store, ship)?.context("backlog alarm row vanished")?;
    let set = candidates(store, ship, duty)?;
    let hash = digest(&set);
    let reset = |alarm: &mut Alarm| {
        alarm.backoff_secs = interval;
        alarm.next_ms = alarm.fired_ms + interval * 1000;
    };
    if set.is_empty() {
        // Everything listed has moved: the next candidate gets the spacing.
        reset(&mut alarm);
        alarm.quiet = 0;
        alarm.evaluated = true;
        alarm.considered.clear();
        alarm.candidates = hash;
        save_alarm(store, ship, &alarm, None)?;
        tx.commit()?;
        return Ok(skipped(ship, "no candidates"));
    }
    let changed = hash != alarm.candidates;
    if !alarm.evaluated && (now >= alarm.next_ms || changed) {
        // Judge the last fire once: did its wake move any task it listed?
        let productive = alarm.considered.iter().any(|(id, revision)| {
            set.iter()
                .find(|c| c.id == *id)
                .is_none_or(|c| c.revision != *revision)
        });
        let kind = if productive {
            reset(&mut alarm);
            alarm.quiet = 0;
            "backlog.productive"
        } else if changed {
            reset(&mut alarm);
            "backlog.changed"
        } else {
            alarm.backoff_secs = (alarm.backoff_secs * 2).min(MAX_BACKOFF_SECS.max(interval));
            alarm.next_ms = alarm.fired_ms + alarm.backoff_secs * 1000;
            alarm.quiet += 1;
            "backlog.unproductive"
        };
        alarm.evaluated = true;
        alarm.candidates = hash.clone();
        store.audit(
            kind,
            &json!({"ship": ship, "fires": alarm.fires, "backoff_secs": alarm.backoff_secs,
                "quiet": alarm.quiet, "next_ms": alarm.next_ms}),
        )?;
    } else if alarm.evaluated && changed {
        // A new task, a reopened one or a new fault: list it after the spacing.
        reset(&mut alarm);
        alarm.candidates = hash.clone();
    }
    if now < alarm.next_ms {
        save_alarm(store, ship, &alarm, None)?;
        tx.commit()?;
        return Ok(
            json!({"ship": ship, "skipped": "spaced", "next_ms": alarm.next_ms,
            "backoff_secs": alarm.backoff_secs}),
        );
    }
    let recent: i64 = store.connection().query_row(
        "SELECT COUNT(*) FROM wakes WHERE ship=?1 AND created_ms > ?2",
        params![ship, now - 3_600_000],
        |r| r.get(0),
    )?;
    if recent >= profile["max_wakes_per_hour"].as_i64().unwrap_or(12) {
        save_alarm(store, ship, &alarm, None)?;
        tx.commit()?;
        return Ok(skipped(ship, "hourly wake budget spent"));
    }

    alarm.fires += 1;
    alarm.fired_ms = now;
    alarm.next_ms = now + alarm.backoff_secs * 1000;
    alarm.evaluated = false;
    alarm.considered = set
        .iter()
        .take(LISTED)
        .map(|c| (c.id.clone(), c.revision))
        .collect();
    alarm.candidates = hash;
    let id = format!("backlog-{ship}-{}", alarm.fires);
    let thread = format!("backlog-{ship}");
    let listed: Vec<&str> = alarm.considered.iter().map(|(id, _)| id.as_str()).collect();
    crate::mail::record_crew(
        store.connection(),
        &json!({"id": id, "to": ship, "kind": "notice", "urgency": "normal",
            "correlation": thread, "body": notice(&set, high),
            "payload": {"backlog_alarm": {"fires": alarm.fires, "considered": listed,
                "total": set.len()}}}),
    )?;
    // A hosted wake has no wake row of its own; this record counts it
    // against the hourly budget. It is never offered or running.
    if crate::host::hosted(profile) {
        let focus = format!("thread:{thread}");
        store.connection().execute(
            "INSERT INTO wakes(id, ship, focus, through_seq, mode, status, created_ms, finished_ms,
               focuses, reason, dispatcher_generation)
             VALUES(?1, ?2, ?3, 0, 'hosted', 'succeeded', ?4, ?4, ?5, 'backlog',
               (SELECT value FROM metadata WHERE key='dispatcher_generation'))
             ON CONFLICT(id) DO NOTHING",
            params![id, ship, focus, now, json!([focus]).to_string()],
        )?;
    }
    store.audit(
        "backlog.alarm",
        &json!({"ship": ship, "fires": alarm.fires, "considered": listed, "total": set.len(),
            "backoff_secs": alarm.backoff_secs, "attention": if high { "high" } else { "not high" }}),
    )?;
    save_alarm(store, ship, &alarm, Some(&id))?;
    tx.commit()?;
    Ok(
        json!({"ship": ship, "fired": id, "considered": listed, "total": set.len(),
        "next_ms": alarm.next_ms, "backoff_secs": alarm.backoff_secs}),
    )
}

/// One pass over the local ships with a first mate. A failing ship reports
/// its error and never stops the dispatcher.
pub(crate) fn tick(store: &Store) -> Result<Vec<Value>> {
    if !available(store)? {
        return Ok(vec![]);
    }
    let mut reports = vec![];
    for ship in crate::activation::ships(store)? {
        let name = ship["name"].as_str().unwrap_or("").to_owned();
        reports.push(
            consider(store, &name, &ship["first_mate"]).unwrap_or_else(|error| {
                json!({"ship": name,
                    "error": format!("{error:#}").chars().take(2_000).collect::<String>()})
            }),
        );
    }
    Ok(reports)
}

/// The ship's alarm state for `work.status`, or null before its first check.
pub(crate) fn view(store: &Store, ship: &str) -> Result<Value> {
    if !available(store)? {
        return Ok(Value::Null);
    }
    Ok(match load_alarm(store, ship)? {
        Some(alarm) => json!({"fired_ms": alarm.fired_ms, "next_ms": alarm.next_ms,
            "backoff_secs": alarm.backoff_secs, "fires": alarm.fires, "quiet": alarm.quiet,
            "evaluated": alarm.evaluated,
            "considered": alarm.considered.iter().map(|(id, _)| id).collect::<Vec<_>>()}),
        None => Value::Null,
    })
}

/// One bearings line once the alarm has fired for this ship.
pub(crate) fn bearings_line(store: &Store, ship: &str) -> Result<Option<String>> {
    if !available(store)? {
        return Ok(None);
    }
    Ok(load_alarm(store, ship)?
        .filter(|alarm| alarm.fires > 0)
        .map(|alarm| {
            let state = if alarm.quiet > 0 {
                format!(" (backing off, {} unproductive)", alarm.quiet)
            } else {
                String::new()
            };
            format!(
                "Backlog alarm: {} fired, last listed {}; next not before {}{state}.",
                alarm.fires,
                alarm.considered.len(),
                alarm.next_ms
            )
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(n: usize) -> Candidate {
        Candidate {
            id: format!("draft-{n}"),
            title: "x".repeat(200),
            rank: (n == 0).then_some(2),
            revision: 0,
            label: "unscheduled".into(),
        }
    }

    #[test]
    fn only_high_attention_restricts_the_brief_and_the_notice_stays_bounded() {
        let set: Vec<Candidate> = (0..40).map(draft).collect();
        let normal = notice(&set, false);
        assert!(normal.starts_with("Backlog check: you are idle"));
        assert!(normal.contains("- draft-0 (rank 2): "));
        assert!(normal.contains("For each: write assignment:<id> and task.enqueue it; or enqueue"));
        assert!(normal.contains("... and 30 more (work.status lists them)."));
        assert!(!normal.contains("draft-10:"));
        assert!(normal.len() < 2_048, "{}", normal.len());
        let high = notice(&set, true);
        assert!(high.contains(
            "Attention is high: enqueue, defer, hand off or close only; do not start work in this conversation. Enqueued work goes to a fresh coordinator. For each: enqueue and task.defer"
        ));
        assert!(!high.contains("write assignment"));
        assert!(high.len() < 2_048, "{}", high.len());
    }

    #[test]
    fn the_candidate_hash_ignores_order_but_not_revisions() {
        let mut a = vec![draft(1), draft(2)];
        let b = vec![draft(2), draft(1)];
        assert_eq!(digest(&a), digest(&b));
        a[0].revision = 3;
        assert_ne!(digest(&a), digest(&b));
        assert_ne!(digest(&[]), digest(&b));
    }
}
