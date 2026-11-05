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
                "UPDATE jobs SET record=json_set(record,'$.task_id','infra-duty')",
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
            .execute("UPDATE crew_parks SET task_id='infra-duty'", [])
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
    assert_eq!(consider(&store, "infra", &json!({})).unwrap().len(), LISTED);
    assert_eq!(consider(&store, "infra", &json!({})).unwrap().len(), 5);
    assert!(consider(&store, "infra", &json!({})).unwrap().is_empty());
}
