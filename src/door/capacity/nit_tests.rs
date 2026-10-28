//! V5 review nits carried by V6a.
use super::tests::{fixture, restricted};
use super::*;
use rusqlite::params;

#[test]
fn hidden_parent_does_not_skip_selected_tasks_own_charter_liveness_or_hash() {
    let (_dir, store, _ledger, registry, c) = fixture();
    let c = restricted(&c, json!(["survey"]));
    let original = store.charter("survey").unwrap();
    let payload = json!({"artifact":"alpha","environment":"beta","charter_hash":crate::core::hash(&original)});
    store.connection().execute("INSERT INTO mail(id,sender,recipient,kind,request_kind,task_id,body,payload,urgency,created_ms,state) VALUES('approval','survey','captain','request','production_approval','survey','Approve?',?1,'normal',0,'open')",[payload.to_string()]).unwrap();
    store
        .connection()
        .execute("UPDATE tasks SET parent='relay' WHERE id='survey'", [])
        .unwrap();
    let read = || {
        local_read(
            &registry,
            Command::RuntimeNeeds,
            &json!({"ship":"survey"}),
            &c,
        )
        .unwrap()
    };
    assert_eq!(read().needs.len(), 1);
    for edit in [
        json!({"active":false}),
        json!({"active":true,"max_heavy_jobs":3}),
    ] {
        let mut charter = original.clone();
        for (k, v) in edit.as_object().unwrap() {
            charter[k] = v.clone();
        }
        store
            .connection()
            .execute(
                "UPDATE charters SET data=?1 WHERE id='survey'",
                [charter.to_string()],
            )
            .unwrap();
        let reply = read();
        assert_eq!(reply.coverage.state, "covered");
        assert!(reply.needs.is_empty());
    }
    store
        .connection()
        .execute(
            "UPDATE charters SET data=?1 WHERE id='survey'",
            [original.to_string()],
        )
        .unwrap();
    assert_eq!(read().needs.len(), 1);
}

#[test]
fn selected_capacity_keeps_closed_task_workspace_waiters_with_supervisors() {
    let (_dir, store, ledger, registry, c) = fixture();
    let c = restricted(&c, json!(["survey"]));
    store
        .connection()
        .execute("UPDATE tasks SET status='done' WHERE id='survey'", [])
        .unwrap();
    let record = json!({"key":"alpha","kind":"workspace","home":store.home(),"job_id":"survey","incarnation":"alpha","created_ms":0,"supervisor":{"pid":1,"started":"alpha"}});
    ledger
        .execute("INSERT INTO queue VALUES('alpha',?1)", [record.to_string()])
        .unwrap();
    let selected = local_read(
        &registry,
        Command::MachineCapacity,
        &json!({"machine":"local","ship":"survey"}),
        &c,
    )
    .unwrap();
    assert_eq!(selected.capacity.unwrap().queue_depth, 1);
    // The conservative shared reader counts the same recorded waiter without
    // process probing, pruning it or inferring that closed means payload gone.
    let mut wire = c.gateway();
    wire["ships"] = Value::Null;
    let all = DelegatedContext::from_gateway(&wire).unwrap();
    let shared = local_read(
        &registry,
        Command::MachineCapacity,
        &json!({"machine":"local","ship":"survey"}),
        &all,
    )
    .unwrap();
    assert_eq!(shared.capacity.unwrap().queue_depth, 1);
    ledger
        .execute(
            "UPDATE queue SET record=json_set(record,'$.supervisor',NULL) WHERE key=?1",
            params!["alpha"],
        )
        .unwrap();
    let stale = local_read(
        &registry,
        Command::MachineCapacity,
        &json!({"machine":"local","ship":"survey"}),
        &c,
    )
    .unwrap();
    assert_eq!(stale.capacity.unwrap().queue_depth, 0);
}

#[test]
fn hidden_workspace_waiters_cannot_exhaust_the_selected_queue_id_budget() {
    let (_dir, store, ledger, registry, c) = fixture();
    let c = restricted(&c, json!(["survey"]));
    let tx = ledger.unchecked_transaction().unwrap();
    for n in 0..2050 {
        let key = format!("alpha-{n}");
        tx.execute("INSERT INTO queue VALUES(?1,?2)",params![key,json!({"key":key,"kind":"workspace","home":store.home(),"job_id":"relay","incarnation":"alpha","created_ms":0,"supervisor":null}).to_string()]).unwrap();
    }
    tx.execute("INSERT INTO queue VALUES('survey',?1)",[json!({"key":"survey","kind":"workspace","home":store.home(),"job_id":"survey","incarnation":"alpha","created_ms":0,"supervisor":null}).to_string()]).unwrap();
    tx.commit().unwrap();
    let before = std::fs::read(store.home().join("privateer.sqlite")).unwrap();
    let selected = local_read(
        &registry,
        Command::MachineCapacity,
        &json!({"machine":"local","ship":"survey"}),
        &c,
    )
    .unwrap();
    assert_eq!(selected.coverage.state, "covered");
    let cap = selected.capacity.unwrap();
    assert_eq!(cap.queue_depth, 1);
    assert_eq!(cap.queue[0].job_id.as_deref(), Some("survey"));
    assert_eq!(
        std::fs::read(store.home().join("privateer.sqlite")).unwrap(),
        before
    );
}
