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
fn a_typed_wait_or_disabled_alarm_is_not_stranded() {
    let (_temp, store) = fixture::home();
    old_fault(&store);
    assert!(
        consider(&store, "infra", &json!({"backlog_alarm_secs":0}))
            .unwrap()
            .is_empty()
    );
    // Even an inconsistent fault row carrying a typed wait is not treated
    // as abandoned. Normal task.defer changes its state as well.
    store
        .connection()
        .execute(
            "UPDATE task_schedule SET record=json_set(record,'$.wait',json(?1))",
            [json!({"kind":"manual","reason":"Owner holds it"}).to_string()],
        )
        .unwrap();
    assert!(consider(&store, "infra", &json!({})).unwrap().is_empty());
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
    // An ordinary open task, a fresh fault, an exhausted fault, and a typed
    // wait must all be discarded before inspecting jobs. Invalid JSON is a
    // timing-independent scan sentinel: JSON extraction would fail if read.
    store
        .connection()
        .execute(
            "INSERT INTO jobs(id,record) VALUES('scan-sentinel','not json')",
            [],
        )
        .unwrap();
    let writes = || crate::store::WRITE_TRANSACTIONS.with(|n| n.get());
    for state in ["ready", "fresh", "exhausted", "waiting"] {
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
            _ => fixture::schedule(&store, "fault", Some(json!({"kind":"manual"}))),
        }
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
    // Prove the scan sentinel is effective once a fault really is due.
    old_fault(&store);
    assert!(due_ids(&store, "infra", "", Store::now_ms(), 3600).is_err());
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
