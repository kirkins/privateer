//! Long-lived homes and selected-ship approval ancestry regressions.
use super::tests::{fixture, hold, queue, restricted};
use super::*;
use rusqlite::params;

#[test]
fn finished_job_history_cannot_exhaust_current_capacity_or_needs_custody() {
    let (_dir, store, ledger, registry, c) = fixture();
    let c = restricted(&c, json!(["survey"]));
    let tx = store.connection().unchecked_transaction().unwrap();
    for n in 0..4000 {
        let id = format!("alpha-{n}");
        let status = ["succeeded", "failed", "cancelled", "timed_out", "resumed"][n % 5];
        tx.execute("INSERT INTO jobs(id,record) VALUES(?1,?2)", params![id, json!({"id":id,"task_id":"survey","incarnation":"alpha","status":status,"created_ms":0}).to_string()]).unwrap();
    }
    tx.commit().unwrap();
    // The current rows come after the entire history; a pre-filter cap would
    // lose them (or refuse the whole observation).
    queue(&ledger, &store, "queued", "survey", None);
    hold(&ledger, &store, "running", "survey", true);
    store.connection().execute("INSERT INTO jobs(id,record) VALUES('orphaned',?1)", [json!({"id":"orphaned","task_id":"survey","incarnation":"alpha","status":"orphaned","created_ms":0}).to_string()]).unwrap();
    let selected = |states: &[&str]| -> Vec<String> {
        serde_json::from_str(&view::visible_job_ids(&store, "survey", states, &c).unwrap()).unwrap()
    };
    assert_eq!(selected(&["queued"]), ["queued"]);
    let mut jobs = selected(&["queued", "orphaned"]);
    jobs.sort();
    assert_eq!(jobs, ["orphaned", "queued"]);
    let before = std::fs::read(store.home().join("privateer.sqlite")).unwrap();
    let cap = local_read(
        &registry,
        Command::MachineCapacity,
        &json!({"machine":"local","ship":"survey"}),
        &c,
    )
    .unwrap();
    assert_eq!(cap.coverage.state, "covered");
    let cap = cap.capacity.unwrap();
    assert_eq!(cap.queue_depth, 1);
    assert_eq!(cap.queue[0].job_id.as_deref(), Some("queued"));
    // Running holds are rechecked separately, not lost by the queued-ID filter.
    assert_eq!(cap.reservation_count, 1);
    assert_eq!(cap.holds[0].job_id.as_deref(), Some("running"));
    let needs = local_read(
        &registry,
        Command::RuntimeNeeds,
        &json!({"ship":"survey"}),
        &c,
    )
    .unwrap();
    assert_eq!(needs.coverage.state, "covered");
    let mut ids: Vec<_> = needs.needs.iter().map(|n| n.source_id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["orphaned", "queued"]);
    assert!(!needs.more);
    assert_eq!(
        std::fs::read(store.home().join("privateer.sqlite")).unwrap(),
        before
    );
}

#[test]
fn finished_tasks_and_mail_history_do_not_exhaust_workspace_or_needs_readers() {
    let (_dir, store, ledger, registry, c) = fixture();
    let c = restricted(&c, json!(["survey", "relay"]));
    let tx = store.connection().unchecked_transaction().unwrap();
    for n in 0..4000 {
        tx.execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES(?1,'survey','survey','history',?2,0)", params![format!("alpha-{n}"), if n % 2 == 0 { "done" } else { "cancelled" }]).unwrap();
        let id = format!("gamma-{n}");
        tx.execute("INSERT INTO jobs(id,record) VALUES(?1,?2)", params![id, json!({"id":id,"task_id":format!("alpha-{n}"),"incarnation":"alpha","status":"succeeded","created_ms":0}).to_string()]).unwrap();
    }
    for n in 0..8000 {
        tx.execute("INSERT INTO mail(id,sender,recipient,kind,task_id,body,payload,urgency,created_ms,state) VALUES(?1,'survey','captain','request',?2,'Past question','{}','normal',0,'answered')", params![format!("beta-{n}"), format!("alpha-{}", n % 4000)]).unwrap();
    }
    tx.execute("INSERT INTO mail(id,sender,recipient,kind,task_id,body,payload,urgency,created_ms,state) VALUES('ask','survey','captain','request','survey','Current question','{}','normal',0,'open')", []).unwrap();
    tx.commit().unwrap();
    // Retained workspace queue entries for closed tasks are not current work.
    // Include them to verify both readers filter ownership *and* task lifecycle.
    for id in ["survey", "alpha-0", "alpha-1"] {
        ledger.execute("INSERT INTO queue VALUES(?1,?2)", params![id,json!({"key":id,"kind":"workspace","home":store.home(),"job_id":id,"incarnation":"alpha","created_ms":0,"supervisor":null}).to_string()]).unwrap();
    }
    let before = std::fs::read(store.home().join("privateer.sqlite")).unwrap();
    let ledger_before: String = ledger
        .query_row("SELECT group_concat(record) FROM queue", [], |r| r.get(0))
        .unwrap();
    let cap = local_read(
        &registry,
        Command::MachineCapacity,
        &json!({"machine":"local","ship":"survey"}),
        &c,
    )
    .unwrap();
    assert_eq!(cap.coverage.state, "covered");
    let cap = cap.capacity.unwrap();
    assert_eq!(cap.queue_depth, 1);
    assert_eq!(cap.queue[0].job_id.as_deref(), Some("survey"));
    let needs = local_read(
        &registry,
        Command::RuntimeNeeds,
        &json!({"ship":"survey","limit":1}),
        &c,
    )
    .unwrap();
    assert_eq!(needs.coverage.state, "covered");
    assert_eq!(needs.needs[0].source_id, "ask");
    assert!(needs.more);
    let next = local_read(
        &registry,
        Command::RuntimeNeeds,
        &json!({"ship":"survey","limit":1,"continuation":needs.continuation}),
        &c,
    )
    .unwrap();
    assert_eq!(next.coverage.state, "covered");
    assert_eq!(next.needs.len(), 1);
    assert_eq!(next.needs[0].source, "workspace");
    assert_eq!(next.needs[0].source_id, "survey");
    assert!(!next.more);
    assert_eq!(
        std::fs::read(store.home().join("privateer.sqlite")).unwrap(),
        before
    );
    assert_eq!(
        ledger
            .query_row::<String, _, _>("SELECT group_concat(record) FROM queue", [], |r| r.get(0))
            .unwrap(),
        ledger_before
    );
}

#[test]
fn scoped_approval_ancestry_stops_before_hidden_cancelled_missing_or_looping_parent() {
    let (_dir, store, _ledger, registry, c) = fixture();
    let payload = json!({"artifact":"alpha","environment":"beta","charter_hash":crate::core::hash(&store.charter("survey").unwrap())});
    store.connection().execute("INSERT INTO mail(id,sender,recipient,kind,request_kind,task_id,body,payload,urgency,created_ms,state) VALUES('approval','survey','captain','request','production_approval','survey','Approve?',?1,'normal',0,'open')", [payload.to_string()]).unwrap();
    store
        .connection()
        .execute("UPDATE tasks SET parent='relay' WHERE id='survey'", [])
        .unwrap();
    // Even a named grant listing both ships still selects survey's needs, not
    // the hidden parent's lifecycle. The operator reader retains full checks.
    for ships in [json!(["survey"]), json!(["survey", "relay"]), Value::Null] {
        let c = restricted(&c, ships);
        for status in ["open", "cancelled", "done"] {
            store
                .connection()
                .execute(
                    "UPDATE tasks SET status=?1,parent=NULL WHERE id='relay'",
                    [status],
                )
                .unwrap();
            let reply = local_read(
                &registry,
                Command::RuntimeNeeds,
                &json!({"ship":"survey"}),
                &c,
            )
            .unwrap();
            assert_eq!(reply.coverage.state, "covered");
            assert_eq!(reply.needs.len(), 1);
            assert_eq!(reply.needs[0].source_id, "approval");
            assert!(
                reply
                    .unknown
                    .iter()
                    .any(|s| s == "task_captain_actionability")
            );
            assert_eq!(reply.action_authority, "none");
            let native = crate::needs::query(&store, &json!({})).unwrap();
            assert_eq!(native["available"], true);
            assert_eq!(
                native["items"].as_array().unwrap().len(),
                usize::from(status != "cancelled")
            );
        }
        // A hidden self-cycle is never walked (including its cancelled status).
        store
            .connection()
            .execute(
                "UPDATE tasks SET status='open',parent='relay' WHERE id='relay'",
                [],
            )
            .unwrap();
        let looped = local_read(
            &registry,
            Command::RuntimeNeeds,
            &json!({"ship":"survey"}),
            &c,
        )
        .unwrap();
        assert_eq!(looped.coverage.state, "covered");
        assert_eq!(looped.needs.len(), 1);
        assert_eq!(
            crate::needs::query(&store, &json!({})).unwrap()["available"],
            false
        );
        // Broken hidden ancestry has the same selected result as any foreign
        // parent. Disable only the fixture writer's FK enforcement to plant it.
        store
            .connection()
            .pragma_update(None, "foreign_keys", false)
            .unwrap();
        store
            .connection()
            .execute("UPDATE tasks SET parent='gamma' WHERE id='survey'", [])
            .unwrap();
        let missing = local_read(
            &registry,
            Command::RuntimeNeeds,
            &json!({"ship":"survey"}),
            &c,
        )
        .unwrap();
        assert_eq!(missing.coverage.state, "covered");
        assert_eq!(missing.needs.len(), 1);
        assert_eq!(missing.needs[0].id, looped.needs[0].id);
        assert_eq!(
            crate::needs::query(&store, &json!({})).unwrap()["available"],
            false
        );
        store
            .connection()
            .execute("UPDATE tasks SET parent='relay' WHERE id='survey'", [])
            .unwrap();
        store
            .connection()
            .pragma_update(None, "foreign_keys", true)
            .unwrap();
    }
    // Visible ancestry is still checked: selected-parent cancellation hides
    // the stale request, and a selected cycle remains unavailable.
    store
        .connection()
        .execute(
            "UPDATE tasks SET owner='survey',status='cancelled',parent=NULL WHERE id='relay'",
            [],
        )
        .unwrap();
    let c = restricted(&c, json!(["survey"]));
    let stopped = local_read(
        &registry,
        Command::RuntimeNeeds,
        &json!({"ship":"survey"}),
        &c,
    )
    .unwrap();
    assert_eq!(stopped.coverage.state, "covered");
    assert!(stopped.needs.is_empty());
    store
        .connection()
        .execute(
            "UPDATE tasks SET status='open',parent='relay' WHERE id='relay'",
            [],
        )
        .unwrap();
    let cycle = local_read(
        &registry,
        Command::RuntimeNeeds,
        &json!({"ship":"survey"}),
        &c,
    )
    .unwrap();
    assert_eq!(cycle.coverage.state, "unavailable");
}
