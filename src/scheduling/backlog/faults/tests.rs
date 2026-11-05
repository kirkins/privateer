use super::*;
use crate::scheduling::fixture;

fn old_fault(store: &Store) {
    fixture::schedule(store, "fault", None);
    store
        .connection()
        .execute(
            "UPDATE task_schedule SET record=json_set(record,'$.faulted_ms',?1)",
            [Store::now_ms() - 3_600_001],
        )
        .unwrap();
}

fn child(store: &mut Store) {
    crate::dispatch(
        store,
        "task.create",
        json!({"id":"child","parent":"work-1",
        "charter_id":"project","owner":"infra","title":"Child"}),
    )
    .unwrap();
    crate::dispatch(
        store,
        "task.create",
        json!({"id":"unrelated","charter_id":"project","owner":"infra","title":"Other tree"}),
    )
    .unwrap();
}

// A fresh read-only connection loads sqlite_schema on its first read. The
// reminder table adds bounded startup work, not extra mail-query work. Dropping
// it reconstructs the pre-#750 schema (the branch's only schema addition).
#[test]
fn reminder_schema_adds_only_startup_work_and_does_not_change_mail_read_plan() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let (_temp, store) = fixture::home();
    let path = store.home().join("privateer.sqlite");
    let measure = || {
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let steps = Arc::new(AtomicUsize::new(0));
        let counter = steps.clone();
        conn.progress_handler(
            1,
            Some(move || {
                counter.fetch_add(1, Ordering::Relaxed);
                false
            }),
        );
        conn.query_row(
            "SELECT mailbox FROM mail_source_health WHERE only=1",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap();
        let cold = steps.swap(0, Ordering::Relaxed);
        conn.query_row(
            "SELECT mailbox FROM mail_source_health WHERE only=1",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap();
        let warm = steps.load(Ordering::Relaxed);
        conn.progress_handler(0, None::<fn() -> bool>);
        let plan = conn
            .prepare("EXPLAIN QUERY PLAN SELECT mailbox FROM mail_source_health WHERE only=1")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        (cold, warm, plan)
    };
    let with = measure();
    store
        .connection()
        .execute_batch("DROP TABLE schedule_fault_reminders")
        .unwrap();
    let without = measure();
    eprintln!("mail read with reminder schema: {with:?}; base schema: {without:?}");
    assert_eq!(with.1, without.1);
    assert_eq!(with.2, without.2);
    assert!(
        with.0 > without.0,
        "fresh connection includes schema loading"
    );
    assert!(
        with.0 <= without.0 + 20,
        "extra schema-load VM work: {}",
        with.0 - without.0
    );
}

#[test]
fn supervised_work_in_the_tree_suppresses_reminders_but_unrelated_work_does_not() {
    for status in [
        "starting",
        "queued",
        "launching",
        "running",
        "orphaned",
        "parked",
    ] {
        let (_temp, mut store) = fixture::home();
        old_fault(&store);
        child(&mut store);
        store
            .connection()
            .execute(
                "INSERT INTO jobs(id,record) VALUES('live',?1)",
                [json!({"task_id":"child","status":status}).to_string()],
            )
            .unwrap();
        assert!(
            consider(&store, "infra", &json!({})).unwrap().is_empty(),
            "{status}"
        );
        store
            .connection()
            .execute(
                "UPDATE jobs SET record=json_set(record,'$.task_id','work-1')",
                [],
            )
            .unwrap();
        assert!(
            consider(&store, "infra", &json!({})).unwrap().is_empty(),
            "root {status}"
        );
        // Same ship, different task tree: not a reason to strand this fault.
        store
            .connection()
            .execute(
                "UPDATE jobs SET record=json_set(record,'$.task_id','unrelated')",
                [],
            )
            .unwrap();
        assert_eq!(
            consider(&store, "infra", &json!({})).unwrap().len(),
            1,
            "{status}"
        );
    }
}

#[test]
fn descendant_parks_are_live_even_without_a_live_job_record() {
    for state in ["parked", "held", "resuming"] {
        let (_temp, mut store) = fixture::home();
        old_fault(&store);
        child(&mut store);
        store.connection().execute(
            "INSERT INTO crew_parks(id,root,crew_job_id,child_job_id,task_id,ship,state,next,created_ms,updated_ms)
             VALUES('park','crew','crew','heavy','child','infra',?1,'Review outcome',0,0)", [state],
        ).unwrap();
        assert!(
            consider(&store, "infra", &json!({})).unwrap().is_empty(),
            "{state}"
        );
        store
            .connection()
            .execute("UPDATE crew_parks SET task_id='unrelated'", [])
            .unwrap();
        assert_eq!(
            consider(&store, "infra", &json!({})).unwrap().len(),
            1,
            "{state}"
        );
    }
}

#[test]
fn a_live_typed_wait_or_disabled_alarm_is_not_stranded() {
    let (_temp, store) = fixture::home();
    old_fault(&store);
    assert!(
        consider(&store, "infra", &json!({"backlog_alarm_secs":0}))
            .unwrap()
            .is_empty()
    );
    fixture::schedule(
        &store,
        "waiting",
        Some(json!({"kind":"time","at_ms":i64::MAX})),
    );
    assert!(consider(&store, "infra", &json!({})).unwrap().is_empty());
}

#[test]
fn cancelled_task_and_closed_remote_waits_are_evidence_not_live_waits_after_faulting() {
    for wait in [
        json!({"kind":"task","ref":"child","reason":"Needs child"}),
        json!({"kind":"remote","ref":{"ship":"apps","task":"far-work"},
            "reason":"Needs far work","end":{"notice":"wait_closed","reason":"retired"}}),
    ] {
        let (_temp, mut store) = fixture::home();
        child(&mut store);
        store
            .connection()
            .execute("UPDATE tasks SET status='cancelled' WHERE id='child'", [])
            .unwrap();
        store
            .connection()
            .execute("UPDATE tasks SET status='blocked' WHERE id='work-1'", [])
            .unwrap();
        fixture::schedule(&store, "waiting", Some(wait.clone()));
        assert!(consider(&store, "infra", &json!({})).unwrap().is_empty());
        let mut work = load(&store, "work-1").unwrap().unwrap();
        crate::scheduling::advance(&store, &mut work, Store::now_ms() - 3_600_001).unwrap();
        assert_eq!(work.state, "fault", "reason: {:?}", work.reason);
        assert_eq!(work.wait, Some(wait), "fault preserves the old wait");
        assert!(
            work.reason.as_deref().unwrap().contains("cancelled")
                || work.reason.as_deref().unwrap().contains("was closed"),
            "reason: {:?}",
            work.reason
        );
        save(&store, &work).unwrap();
        assert_eq!(consider(&store, "infra", &json!({})).unwrap().len(), 1);
        assert!(consider(&store, "infra", &json!({})).unwrap().is_empty());
    }
}

#[test]
fn a_failed_counter_update_rolls_back_its_mail_and_retry_is_deduplicated() {
    let (_temp, store) = fixture::home();
    old_fault(&store);
    store
        .connection()
        .execute_batch(
            "CREATE TRIGGER fail_reminder BEFORE UPDATE ON schedule_fault_reminders
         BEGIN SELECT RAISE(ABORT,'simulated crash'); END;",
        )
        .unwrap();
    assert!(consider(&store, "infra", &json!({})).is_err());
    let count = || -> i64 {
        store.connection().query_row(
        "SELECT count(*) FROM mail WHERE json_extract(payload,'$.schedule_fault_reminder') IS NOT NULL",
        [], |r| r.get(0)).unwrap()
    };
    assert_eq!(count(), 0);
    store
        .connection()
        .execute_batch("DROP TRIGGER fail_reminder;")
        .unwrap();
    assert_eq!(consider(&store, "infra", &json!({})).unwrap().len(), 1);
    assert!(consider(&store, "infra", &json!({})).unwrap().is_empty());
    assert_eq!(count(), 1);
}

#[test]
fn fault_batches_are_bounded_without_starving_later_tasks() {
    let (_temp, mut store) = fixture::home();
    old_fault(&store);
    let raw: String = store
        .connection()
        .query_row(
            "SELECT record FROM task_schedule WHERE task_id='work-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    for n in 0..14 {
        let id = format!("fault-{n}");
        crate::dispatch(
            &mut store,
            "task.create",
            json!({"id":id,
            "charter_id":"project","owner":"infra","title":"Another fault"}),
        )
        .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO task_schedule(task_id,record) VALUES(?1,json_set(?2,'$.task_id',?1))",
                params![id, raw],
            )
            .unwrap();
    }
    assert_eq!(consider(&store, "infra", &json!({})).unwrap().len(), 1);
    let payload: String = store
        .connection()
        .query_row(
            "SELECT payload FROM mail ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let payload: Value = serde_json::from_str(&payload).unwrap();
    let tasks = payload["schedule_fault_reminder"]["tasks"]
        .as_array()
        .unwrap();
    assert_eq!(tasks.len(), LISTED, "one digest for ten due faults");
    let body: String = store
        .connection()
        .query_row("SELECT body FROM mail ORDER BY seq DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    for task in tasks {
        assert!(body.contains(task["task_id"].as_str().unwrap()), "{body}");
    }
    assert_eq!(consider(&store, "infra", &json!({})).unwrap().len(), 1);
    let count: i64 = store
        .connection()
        .query_row(
            "SELECT count(*) FROM schedule_fault_reminders WHERE fires=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 15, "second digest handles the remaining five");
    assert!(consider(&store, "infra", &json!({})).unwrap().is_empty());
}

#[test]
fn nothing_due_never_scans_jobs_or_takes_the_write_lock() {
    let (_temp, store) = fixture::home();
    // Count SQLite VM steps, not wall time: adding jobs must add no query
    // work when every root was discarded by the materialized due filters.
    fn steps(store: &Store) -> (Vec<String>, usize) {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let count = Arc::new(AtomicUsize::new(0));
        let observed = count.clone();
        store.connection().progress_handler(
            1,
            Some(move || {
                observed.fetch_add(1, Ordering::Relaxed);
                false
            }),
        );
        let ids = due_ids(store, "infra", "", Store::now_ms(), 3600).unwrap();
        store.connection().progress_handler(0, None::<fn() -> bool>);
        (ids, count.load(Ordering::Relaxed))
    }
    let writes = || crate::store::WRITE_TRANSACTIONS.with(|n| n.get());
    for state in ["ready", "fresh", "exhausted", "waiting"] {
        store.connection().execute("DELETE FROM jobs", []).unwrap();
        fixture::schedule(&store, "fault", None);
        store
            .connection()
            .execute("DELETE FROM schedule_fault_reminders", [])
            .unwrap();
        match state {
            "ready" => fixture::schedule(&store, "ready", None),
            "fresh" => {
                store
                    .connection()
                    .execute(
                        "UPDATE task_schedule SET record=json_set(record,'$.faulted_ms',?1)",
                        [Store::now_ms()],
                    )
                    .unwrap();
            }
            "exhausted" => {
                store
                    .connection()
                    .execute(
                        "INSERT INTO schedule_fault_reminders(task_id,revision,owner,fires,next_ms)
                     VALUES('work-1',1,'infra',2,0)",
                        [],
                    )
                    .unwrap();
            }
            _ => fixture::schedule(
                &store,
                "waiting",
                Some(json!({"kind":"task","ref":"dependency"})),
            ),
        }
        let (ids, empty_steps) = steps(&store);
        assert!(ids.is_empty());
        for n in 0..128 {
            store
                .connection()
                .execute(
                    "INSERT INTO jobs(id,record) VALUES(?1,?2)",
                    params![
                        format!("scan-sentinel-{n}"),
                        json!({"task_id":"work-1","status":"succeeded"}).to_string()
                    ],
                )
                .unwrap();
        }
        let (ids, populated_steps) = steps(&store);
        assert!(ids.is_empty());
        assert_eq!(populated_steps, empty_steps, "no jobs scan for {state}");
        let before = writes();
        assert!(
            consider(&store, "infra", &json!({})).unwrap().is_empty(),
            "{state}"
        );
        assert_eq!(writes(), before, "no writer reservation for {state}");
    }
    let plan = store
        .connection()
        .prepare(&format!("EXPLAIN QUERY PLAN {}", due_sql()))
        .unwrap()
        .query_map(
            params![
                "infra",
                "",
                Store::now_ms(),
                MAX_FIRES,
                CANDIDATES,
                3_600_000
            ],
            |r| r.get::<_, String>(3),
        )
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(plan.iter().any(|s| s == "MATERIALIZE due"), "{plan:?}");
    assert!(plan.iter().any(|s| s.contains("jobs_live")), "{plan:?}");
    // Even a due root ignores terminal history through the partial index.
    old_fault(&store);
    let (ids, populated_steps) = steps(&store);
    assert_eq!(ids, vec!["work-1"]);
    store.connection().execute("DELETE FROM jobs", []).unwrap();
    let (_, empty_steps) = steps(&store);
    assert_eq!(
        populated_steps, empty_steps,
        "due roots do not scan terminal jobs"
    );
    store
        .connection()
        .execute(
            "INSERT INTO jobs(id,record) VALUES('live',?1)",
            [json!({"task_id":"work-1","status":"running"}).to_string()],
        )
        .unwrap();
    let (ids, live_steps) = steps(&store);
    assert!(ids.is_empty(), "live work still suppresses a due root");
    for n in 0..128 {
        store
            .connection()
            .execute(
                "INSERT INTO jobs(id,record) VALUES(?1,?2)",
                params![
                    format!("retained-{n}"),
                    json!({"task_id":"work-1","status":"failed"}).to_string()
                ],
            )
            .unwrap();
    }
    let (ids, with_history_steps) = steps(&store);
    assert!(ids.is_empty());
    assert_eq!(
        live_steps, with_history_steps,
        "suppressed roots do not scan retained jobs each tick"
    );
}

#[test]
fn held_ancestors_cannot_starve_later_faults() {
    for state in ["stopped", "offered", "claimed", "executing"] {
        let (_temp, mut store) = fixture::home();
        old_fault(&store);
        let raw: String = store
            .connection()
            .query_row(
                "SELECT record FROM task_schedule WHERE task_id='work-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        store
            .connection()
            .execute(
                "UPDATE task_schedule SET record=json_set(record,'$.state',?1)",
                [state],
            )
            .unwrap();
        for n in 0..CANDIDATES + 1 {
            let id = format!("held-{n}");
            crate::dispatch(
                &mut store,
                "task.create",
                json!({"id":id,"parent":"work-1",
                "charter_id":"project","owner":"infra","title":"Held fault"}),
            )
            .unwrap();
            store.connection().execute(
                "INSERT INTO task_schedule(task_id,record) VALUES(?1,json_set(?2,'$.task_id',?1))",
                params![id, raw],
            ).unwrap();
        }
        child(&mut store);
        store.connection().execute(
            "INSERT INTO task_schedule(task_id,record) VALUES('unrelated',json_set(?1,'$.task_id','unrelated'))",
            [&raw],
        ).unwrap();
        let ids = due_ids(&store, "infra", "", Store::now_ms(), 3600).unwrap();
        assert_eq!(ids, vec!["unrelated"], "{state}");
        assert_eq!(consider(&store, "infra", &json!({})).unwrap().len(), 1);
    }
}

#[test]
fn live_descendant_schedule_claims_suppress_the_root_without_a_job() {
    for state in ["offered", "claimed", "executing"] {
        let (_temp, mut store) = fixture::home();
        old_fault(&store);
        child(&mut store);
        store
            .connection()
            .execute(
                "INSERT INTO task_schedule(task_id,record)
             SELECT 'child',json_set(record,'$.task_id','child','$.state',?1)
             FROM task_schedule WHERE task_id='work-1'",
                [state],
            )
            .unwrap();
        assert!(
            consider(&store, "infra", &json!({})).unwrap().is_empty(),
            "{state}"
        );
        store
            .connection()
            .execute("DELETE FROM task_schedule WHERE task_id='child'", [])
            .unwrap();
        assert_eq!(consider(&store, "infra", &json!({})).unwrap().len(), 1);
    }
}

#[test]
fn a_denied_or_withdrawn_park_on_ask_leaves_a_remindable_blocked_fault() {
    for ended in ["denied", "withdrawn"] {
        let (_temp, store) = fixture::home();
        fixture::schedule(&store, "ready", None);
        store.connection().execute(
            "INSERT INTO mail(id,sender,recipient,kind,task_id,body,payload,urgency,created_ms,state)
             VALUES('ask','infra','captain','request','work-1','Review blocker','{}','normal',0,'open')", [],
        ).unwrap();
        assert!(crate::scheduling::park_on_ask(&store, "work-1", "ask", "Tool approval").unwrap());
        store
            .connection()
            .execute("UPDATE mail SET state=?1 WHERE id='ask'", [ended])
            .unwrap();
        let mut work = load(&store, "work-1").unwrap().unwrap();
        crate::scheduling::advance(&store, &mut work, Store::now_ms() - 3_600_001).unwrap();
        save(&store, &work).unwrap();
        assert_eq!(work.state, "fault");
        assert!(
            work.wait.is_some(),
            "fault retains the ended request as evidence"
        );
        assert_eq!(store.task("work-1").unwrap()["status"], "blocked");
        assert_eq!(
            consider(&store, "infra", &json!({})).unwrap().len(),
            1,
            "{ended}"
        );
    }
}

#[test]
fn migrating_the_fault_counter_preserves_an_existing_repeat_budget() {
    let (_temp, store) = fixture::home();
    old_fault(&store);
    store
        .connection()
        .execute(
            "UPDATE task_schedule SET record=json_set(record,'$.faults',1)",
            [],
        )
        .unwrap();
    assert_eq!(consider(&store, "infra", &json!({})).unwrap().len(), 1);
    store
        .connection()
        .execute("UPDATE schedule_fault_reminders SET next_ms=0", [])
        .unwrap();
    assert_eq!(consider(&store, "infra", &json!({})).unwrap().len(), 1);
    store
        .connection()
        .execute_batch("ALTER TABLE schedule_fault_reminders DROP COLUMN faults;")
        .unwrap();
    init(&store).unwrap();
    assert!(consider(&store, "infra", &json!({})).unwrap().is_empty());
    let counters: (i64, i64) = store
        .connection()
        .query_row(
            "SELECT faults,fires FROM schedule_fault_reminders",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(counters, (1, 2));
}

#[test]
fn blocked_faults_wait_only_for_open_requests_or_decisions_in_their_tree() {
    for kind in ["request", "decision"] {
        for ended in ["denied", "withdrawn"] {
            let (_temp, mut store) = fixture::home();
            old_fault(&store);
            child(&mut store);
            store
                .connection()
                .execute("UPDATE tasks SET status='blocked' WHERE id='work-1'", [])
                .unwrap();
            store.connection().execute(
                "INSERT INTO mail(id,sender,recipient,kind,task_id,body,payload,urgency,created_ms,state)
                 VALUES('ask','infra','captain',?1,'child','Review this blocker','{}','normal',0,'open')",
                [kind],
            ).unwrap();
            assert!(
                consider(&store, "infra", &json!({})).unwrap().is_empty(),
                "{kind}"
            );
            store
                .connection()
                .execute("UPDATE mail SET state=?1 WHERE id='ask'", [ended])
                .unwrap();
            assert_eq!(
                consider(&store, "infra", &json!({})).unwrap().len(),
                1,
                "{kind} {ended}"
            );
            assert_eq!(
                store.task("work-1").unwrap()["status"],
                "blocked",
                "reminders never resume work"
            );
        }
    }
}
