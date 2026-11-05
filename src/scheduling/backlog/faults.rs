//! Task-scoped fault reminders, independent of the ship's idle alarm. Mail is
//! advice, never a replay: ordinary activation still applies its own gates.
use super::*;

const MAX_FIRES: i64 = 2;

#[cfg(test)]
mod tests;

pub(super) fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS schedule_fault_reminders(
           task_id TEXT NOT NULL REFERENCES tasks(id), revision INTEGER NOT NULL,
           owner TEXT NOT NULL, fires INTEGER NOT NULL, next_ms INTEGER NOT NULL,
           PRIMARY KEY(task_id, revision, owner));",
    )?;
    let tx = store.write_transaction()?;
    let has_faults: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('schedule_fault_reminders') WHERE name='faults')",
        [],
        |r| r.get(0),
    )?;
    if !has_faults {
        // An old spent budget is attributed to the current fault count. A
        // re-fault before this upgrade may therefore miss one generation's
        // reminders; later fault counts and revisions get a fresh budget.
        tx.execute_batch(
            "ALTER TABLE schedule_fault_reminders ADD COLUMN faults INTEGER NOT NULL DEFAULT 0;
             UPDATE schedule_fault_reminders SET faults=COALESCE((
               SELECT json_extract(s.record,'$.faults') FROM task_schedule s
               WHERE s.task_id=schedule_fault_reminders.task_id
                 AND json_extract(s.record,'$.revision')=schedule_fault_reminders.revision),0);",
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// One initial reminder per fault generation, then at most one repeat, no sooner
/// than eight hours later. Both the mail and the counter commit together.
pub(super) fn consider(store: &Store, ship: &str, profile: &Value) -> Result<Vec<String>> {
    let interval = interval_secs(profile);
    if interval <= 0 {
        return Ok(vec![]);
    }
    let now = Store::now_ms();
    let duty = profile["duty_task"].as_str().unwrap_or("");
    // Most ticks have no due faults. Do not reserve SQLite's writer for this
    // read, and never walk jobs for ordinary open tasks. Revalidate after
    // acquiring the writer so concurrent dispositions/reminders cannot race.
    if due_ids(store, ship, duty, now, interval)?.is_empty() {
        return Ok(vec![]);
    }
    let tx = store.write_transaction()?;
    let ids = due_ids(store, ship, duty, now, interval)?;
    let mut listed = vec![];
    let mut lines = vec![];
    for task in ids {
        let work = load(store, &task)?.context("fault schedule vanished")?;
        // Old records have no fault time: start their grace when first seen.
        let since = if work.faulted_ms > 0 {
            work.faulted_ms
        } else {
            now
        };
        store.connection().execute(
            "INSERT INTO schedule_fault_reminders(task_id,revision,owner,fires,next_ms,faults)
             VALUES(?1,?2,?3,0,?4,?5) ON CONFLICT(task_id,revision,owner) DO UPDATE
             SET fires=0,next_ms=excluded.next_ms,faults=excluded.faults
             WHERE schedule_fault_reminders.faults<>excluded.faults",
            params![
                task,
                work.revision,
                ship,
                since.saturating_add(interval * 1000),
                work.faults
            ],
        )?;
        let (fires, next): (i64, i64) = store.connection().query_row(
            "SELECT fires,next_ms FROM schedule_fault_reminders
             WHERE task_id=?1 AND revision=?2 AND owner=?3",
            params![task, work.revision, ship],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if now < next || fires >= MAX_FIRES {
            continue;
        }
        let facts = stale_facts(store, ship, &task)?;
        lines.push(format!(
            "- {task} (revision {}, fault {}): {}\n{facts}",
            work.revision,
            work.faults,
            clip(work.reason.as_deref().unwrap_or(""), REASON_CHARS),
        ));
        listed.push(json!({"task_id":task,"revision":work.revision,
            "faults":work.faults,"fire":fires + 1}));
        store.connection().execute(
            "UPDATE schedule_fault_reminders SET fires=fires+1,next_ms=?4
             WHERE task_id=?1 AND revision=?2 AND owner=?3",
            params![
                task,
                work.revision,
                ship,
                now.saturating_add(MAX_BACKOFF_SECS.max(interval) * 1000)
            ],
        )?;
        if listed.len() == LISTED {
            break;
        }
    }
    let mut sent = vec![];
    if !listed.is_empty() {
        // One owner digest, including for hosted ships (which have no wake
        // row). Ten faults must not become ten independent wake notices.
        let id = format!(
            "schedule-fault-reminder-{ship}-{}",
            crate::core::hash(&json!(listed))
        );
        let body = format!(
            "Stranded schedule faults: no live supervised job, claim or park in these task trees. Retained waits on faulted schedules are evidence, not live resume conditions.\n{}\nReview the faults and these possibly stale facts. Explicitly task.enqueue an assignment to retry, task.defer with a real wait, hand off, or close with evidence. Nothing was replayed and no authority was granted. This reminder does not need the captain.",
            lines.join("\n"),
        );
        // Keep the single-task fields for existing readers; all digest items
        // are explicit in tasks. Never use task focus: this is owner advice.
        let mut payload = listed[0].clone();
        payload["tasks"] = json!(listed);
        crate::mail::record_crew(
            store.connection(),
            &json!({"id":id,"to":ship,"kind":"notice","urgency":"normal",
                "correlation":format!("schedule-fault-reminders-{ship}"),"body":body,
                "payload":{"schedule_fault_reminder":payload}}),
        )?;
        for item in &listed {
            store.audit(
                "scheduling.fault_reminder",
                &json!({"ship":ship,
                "task_id":item["task_id"],"revision":item["revision"],
                "faults":item["faults"],"fire":item["fire"],"mail_id":id}),
            )?;
        }
        sent.push(id);
    }
    tx.commit()?;
    Ok(sent)
}

/// Materialization is intentional: SQLite must apply the cheap fault/due
/// filters before evaluating correlated live-work checks. A fault's retained
/// wait is evidence only: the scheduler honours waits only in state waiting.
/// Holds are excluded
/// before LIMIT, so a large stopped subtree cannot monopolize every batch.
fn due_sql() -> String {
    format!(
        "WITH RECURSIVE due AS MATERIALIZED (
           SELECT t.id,t.parent,t.status,t.rank,t.rowid AS ord
           FROM tasks t JOIN task_schedule s ON s.task_id=t.id
           LEFT JOIN schedule_fault_reminders r ON r.task_id=t.id AND r.owner=t.owner
             AND r.revision=json_extract(s.record,'$.revision')
             AND r.faults=COALESCE(json_extract(s.record,'$.faults'),0)
           WHERE t.owner=?1 AND t.status IN ('open','running','blocked')
             AND t.handoff_to IS NULL AND t.id<>?1||'-duty' AND t.id<>?2
             AND json_extract(s.record,'$.state')='fault'
             AND ((r.task_id IS NULL AND COALESCE(json_extract(s.record,'$.faulted_ms'),0)<=?3-?6)
               OR (r.task_id IS NOT NULL AND r.fires<?4 AND r.next_ms<=?3))),
         ancestors(root,id,depth) AS (
           SELECT id,parent,1 FROM due WHERE parent IS NOT NULL
           UNION ALL SELECT a.root,t.parent,a.depth+1 FROM ancestors a
             JOIN tasks t ON t.id=a.id WHERE t.parent IS NOT NULL AND a.depth<128),
         tree(root,id) AS (SELECT id,id FROM due
           UNION SELECT tree.root,t.id FROM tasks t JOIN tree ON t.parent=tree.id)
         SELECT d.id FROM due d
         WHERE NOT EXISTS(SELECT 1 FROM ancestors a LEFT JOIN task_schedule s ON s.task_id=a.id
           WHERE a.root=d.id AND (a.depth=128 OR json_extract(s.record,'$.state')
             IN ('stopped','offered','claimed','executing')))
           AND (d.status<>'blocked' OR NOT EXISTS(SELECT 1 FROM tree JOIN mail m ON m.task_id=tree.id
             WHERE tree.root=d.id AND m.sender=?1 AND m.state='open' AND m.kind IN ('request','decision')))
           AND NOT EXISTS(SELECT 1 FROM tree JOIN task_schedule s ON s.task_id=tree.id
             WHERE tree.root=d.id AND json_extract(s.record,'$.state') IN ('offered','claimed','executing'))
           AND NOT EXISTS(SELECT 1 FROM tree JOIN jobs j ON json_extract(j.record,'$.task_id')=tree.id
             WHERE tree.root=d.id AND json_extract(j.record,'$.status') IN ({LIVE})
               AND json_extract(j.record,'$.status') NOT IN {terminal})
           AND NOT EXISTS(SELECT 1 FROM tree JOIN crew_parks p ON p.task_id=tree.id
             WHERE tree.root=d.id AND p.state IN ('parked','held','resuming'))
         ORDER BY d.rank IS NULL,d.rank,d.ord LIMIT ?5",
        terminal = crate::jobs::TERMINAL_SQL,
    )
}

fn due_ids(store: &Store, ship: &str, duty: &str, now: i64, interval: i64) -> Result<Vec<String>> {
    Ok(store
        .connection()
        .prepare(&due_sql())?
        .query_map(
            params![ship, duty, now, MAX_FIRES, CANDIDATES, interval * 1000],
            |r| r.get(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Name a bounded sample of the untyped facts that previously hid faults.
/// They are evidence for the owner to review, not waits the runtime trusts.
fn stale_facts(store: &Store, ship: &str, task: &str) -> Result<String> {
    let mut text = String::new();
    // Sample each kind separately so a pile of requests cannot hide pins.
    for (kind, select) in [
        (
            "requests",
            "SELECT 'request '||m.id||' on task:'||m.task_id FROM tree JOIN mail m ON m.task_id=tree.id WHERE m.sender=?1 AND m.state='open' ORDER BY m.seq",
        ),
        (
            "waiting pins",
            "SELECT 'pin '||p.focus||'/'||p.key||' = '||p.value FROM tree JOIN pins p ON p.focus='task:'||tree.id WHERE p.ship=?1 AND p.key='waiting' ORDER BY p.focus",
        ),
    ] {
        let mut statement = store.connection().prepare(&format!(
            "WITH RECURSIVE tree(id) AS (SELECT ?2 UNION SELECT t.id FROM tasks t JOIN tree ON t.parent=tree.id) {select} LIMIT 3"
        ))?;
        let facts = statement
            .query_map(params![ship, task], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for fact in facts.iter().take(2) {
            text.push_str(&format!("- {}\n", clip(fact, 160)));
        }
        if facts.len() > 2 {
            text.push_str(&format!("- ... more {kind}; inspect this task tree.\n"));
        }
    }
    if text.is_empty() {
        Ok("No open outgoing requests or waiting pins in this tree.".into())
    } else {
        Ok(format!(
            "Possibly stale requests/pins (not typed waits):\n{text}"
        ))
    }
}
