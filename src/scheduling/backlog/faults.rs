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
    Ok(())
}

/// One initial reminder per revision, then at most one repeat, no sooner
/// than eight hours later. Both the mail and the counter commit together.
pub(super) fn consider(store: &Store, ship: &str, profile: &Value) -> Result<Vec<String>> {
    let interval = interval_secs(profile);
    if interval <= 0 {
        return Ok(vec![]);
    }
    let now = Store::now_ms();
    let duty = profile["duty_task"].as_str().unwrap_or("");
    let tx = store.write_transaction()?;
    let mut statement = store.connection().prepare(&format!(
        "WITH RECURSIVE tree(root,id) AS (
           SELECT t.id,t.id FROM tasks t JOIN task_schedule s ON s.task_id=t.id
             WHERE t.owner=?1 AND t.status IN ('open','running')
               AND json_extract(s.record,'$.state')='fault'
           UNION SELECT tree.root,t.id FROM tasks t JOIN tree ON t.parent=tree.id)
         SELECT t.id FROM tasks t JOIN task_schedule s ON s.task_id=t.id
           LEFT JOIN schedule_fault_reminders r ON r.task_id=t.id AND r.owner=t.owner
             AND r.revision=json_extract(s.record,'$.revision')
         WHERE t.owner=?1 AND t.status IN ('open','running') AND t.handoff_to IS NULL
           AND t.id<>?1||'-duty' AND t.id<>?2
           AND json_extract(s.record,'$.state')='fault'
           AND json_extract(s.record,'$.wait.kind') IS NULL
           AND (r.task_id IS NULL OR (r.fires<?4 AND r.next_ms<=?3))
           AND NOT EXISTS(SELECT 1 FROM tree JOIN jobs j
             ON json_extract(j.record,'$.task_id')=tree.id
             WHERE tree.root=t.id AND json_extract(j.record,'$.status') IN ({LIVE}))
           AND NOT EXISTS(SELECT 1 FROM tree JOIN crew_parks p ON p.task_id=tree.id
             WHERE tree.root=t.id AND p.state IN ('parked','held','resuming'))
         ORDER BY t.rank IS NULL,t.rank,t.rowid LIMIT ?5"
    ))?;
    let ids = statement
        .query_map(params![ship, duty, now, MAX_FIRES, CANDIDATES], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    let mut sent = vec![];
    for task in ids {
        if held_above(store, &task)? {
            continue;
        }
        let work = load(store, &task)?.context("fault schedule vanished")?;
        // Old records have no fault time: start their grace when first seen.
        let since = if work.faulted_ms > 0 {
            work.faulted_ms
        } else {
            now
        };
        store.connection().execute(
            "INSERT INTO schedule_fault_reminders(task_id,revision,owner,fires,next_ms)
             VALUES(?1,?2,?3,0,?4) ON CONFLICT DO NOTHING",
            params![
                task,
                work.revision,
                ship,
                since.saturating_add(interval * 1000)
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
        if sent.len() == LISTED {
            break;
        }
        let id = format!(
            "schedule-fault-reminder-{task}-{}-{ship}-{}",
            work.revision,
            fires + 1
        );
        let facts = stale_facts(store, ship, &task)?;
        let body = format!(
            "Stranded schedule fault: {task} (revision {}): {}. No live supervised job or park in this task tree and no typed wait.\n{facts}\nReview the fault and these possibly stale facts. Explicitly task.enqueue an assignment to retry, task.defer with a real wait, hand off, or close with evidence. Nothing was replayed and no authority was granted. This reminder does not need the captain.",
            work.revision,
            clip(work.reason.as_deref().unwrap_or(""), REASON_CHARS),
        );
        // Use a thread, not task focus: the owner is being asked to decide,
        // not offered a new coordinator for the faulted task.
        crate::mail::record_crew(
            store.connection(),
            &json!({"id":id,"to":ship,"kind":"notice","urgency":"normal",
                "correlation":format!("schedule-fault-reminders-{ship}"),"body":body,
                "payload":{"schedule_fault_reminder":{"task_id":task,
                    "revision":work.revision,"fire":fires + 1}}}),
        )?;
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
        store.audit(
            "scheduling.fault_reminder",
            &json!({"ship":ship,"task_id":task,
            "revision":work.revision,"fire":fires + 1,"mail_id":id}),
        )?;
        sent.push(id);
    }
    tx.commit()?;
    Ok(sent)
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
