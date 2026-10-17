mod support;

use privateer::dispatch;
use serde_json::{Value, json};
use support::Fleet;

fn call(fleet: &mut Fleet, command: &str, args: Value) -> Value {
    dispatch(&mut fleet.store, command, args).unwrap()
}

fn attend(fleet: &mut Fleet) {
    call(
        fleet,
        "ship.attend",
        json!({"ship":"infra","holder":"host:scheduler-test","ttl_secs":90}),
    );
}

fn poll(fleet: &mut Fleet, idle: bool) -> Value {
    call(
        fleet,
        "work.poll",
        json!({"ship":"infra","holder":"host:scheduler-test","idle":idle,
        "attention":{"tokens":1000,"window":100000}}),
    )
}

#[test]
fn a_scheduled_wake_uses_the_same_first_mate_fallback_as_mail_wakes() {
    let mut fleet = Fleet::new(json!({}));
    std::fs::write(
        fleet.home.join("config.json"),
        json!({"resource_home":fleet.root.join("ledger"),"ship":"infra"}).to_string(),
    )
    .unwrap();
    let own_manifest = fleet.root.join("own/manifest.json");
    let fallback_manifest = fleet.root.join("fallback.json");
    std::fs::create_dir_all(own_manifest.parent().unwrap()).unwrap();
    std::fs::write(
        &own_manifest,
        json!({"schema_version":1,"id":"own","executable":[support::python(),fleet.root.join("adapter.py")],"timeout_ms":5000,"max_output_bytes":1_048_576}).to_string(),
    )
    .unwrap();
    std::fs::write(
        &fallback_manifest,
        json!({"schema_version":1,"id":"fallback","executable":[support::python(),fleet.root.join("adapter.py")],"timeout_ms":5000,"max_output_bytes":1_048_576}).to_string(),
    )
    .unwrap();
    let mut profile = call(&mut fleet, "ship.show", json!({"name":"infra"}))["first_mate"].clone();
    profile["manifest"] = json!(own_manifest);
    profile["options"]["model"] = json!("own-model");
    let fallback_options = json!({
        "mate":fleet.root.join("mate.py"),
        "mode":fleet.root.join("mode"),
        "log":fleet.root.join("prompts.log"),
        "model":"fallback-model"
    });
    call(
        &mut fleet,
        "ship.put",
        json!({"name":"infra","first_mate":profile}),
    );
    // Routes live beside the fleet registry, so write them through the
    // isolated CLI rather than in this process, which has no registry set.
    fleet.ok(
        "routes.set",
        json!({"routes":{"fallback":{"manifest":fallback_manifest,"options":fallback_options,"cost":"plan"}},"first_mate":["fallback"]}),
        None,
    );
    assert_eq!(
        privateer::presence::folder_ship(&fleet.store).as_deref(),
        Some("infra")
    );
    assert_eq!(
        fleet.ok("routes.show", json!({}), None)["first_mate"],
        json!(["fallback"])
    );
    assert!(fleet.store.authorize("infra-repair", "spawn").unwrap()["provider_plugin"].is_null());
    assert_eq!(
        privateer::routes::route_of(own_manifest.to_str().unwrap(), &profile["options"]).as_deref(),
        Some("own/-/own-model")
    );
    std::fs::create_dir_all(fleet.root.join("ledger")).unwrap();
    let ledger = rusqlite::Connection::open(fleet.root.join("ledger/resources.sqlite")).unwrap();
    ledger.execute_batch("CREATE TABLE model_routes(route TEXT PRIMARY KEY,failures INTEGER NOT NULL,until_ms INTEGER NOT NULL,last_error TEXT,updated_ms INTEGER NOT NULL)").unwrap();
    ledger
        .execute(
            "INSERT INTO model_routes VALUES('own/-/own-model',1,9223372036854770000,'429',0)",
            [],
        )
        .unwrap();
    assert!(
        privateer::routes::blocked(&ledger, "own/-/own-model")
            .unwrap()
            .is_some()
    );
    let fallback_route =
        privateer::routes::route_of(fallback_manifest.to_str().unwrap(), &fallback_options)
            .unwrap();
    assert!(
        privateer::routes::blocked(&ledger, &fallback_route)
            .unwrap()
            .is_none()
    );
    call(
        &mut fleet,
        "task.enqueue",
        json!({"id":"infra-repair","brief":"Run through the configured fallback route."}),
    );
    let report = fleet.tick();
    let scheduled = status(&mut fleet, "infra-repair");
    assert_eq!(scheduled["state"], "claimed", "{report} {scheduled}");
    let offer = report["work"][0]["offer_id"].as_str().unwrap();
    let job = call(&mut fleet, "job.status", json!({"id":offer}));
    assert!(
        job["command"]
            .as_array()
            .unwrap()
            .iter()
            .any(|arg| arg == "fallback-model"),
        "{job}"
    );
    assert!(
        !job["command"]
            .as_array()
            .unwrap()
            .iter()
            .any(|arg| arg == "own-model"),
        "{job}"
    );
    assert_ne!(own_manifest, fallback_manifest);
}

#[test]
fn an_idle_hosted_session_claims_prepared_work_without_new_mail() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    call(
        &mut fleet,
        "task.enqueue",
        json!({"id":"infra-repair","brief":"Repair the isolated fixture and verify the result."}),
    );
    let first = poll(&mut fleet, true);
    assert_eq!(first["offer"]["task_id"], "infra-repair", "{first}");
    assert!(first["offer"]["id"].is_string());
    assert!(
        first["offer"]["prompt"]
            .as_str()
            .unwrap()
            .contains("Attention: normal")
    );
    assert!(
        poll(&mut fleet, true)["offer"].is_null(),
        "another poll must not duplicate an outstanding offer"
    );
    assert_eq!(
        call(&mut fleet, "mail.pending", json!({"address":"infra"}))["count"],
        0
    );
}

#[test]
fn legacy_open_tasks_and_busy_sessions_do_not_start_work() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    assert!(poll(&mut fleet, true)["offer"].is_null());
    call(
        &mut fleet,
        "task.enqueue",
        json!({"id":"infra-repair","brief":"A deliberately prepared assignment."}),
    );
    assert!(poll(&mut fleet, false)["offer"].is_null());
    assert_eq!(poll(&mut fleet, true)["offer"]["task_id"], "infra-repair");
}

#[test]
fn a_person_started_session_parks_work_until_it_leaves() {
    let mut fleet = Fleet::new(json!({}));
    worker(
        &fleet,
        "call('task.update', {'id':'infra-repair','actor':ship,'status':'done'})",
    );
    call(
        &mut fleet,
        "ship.attend",
        json!({"ship":"infra","holder":"person-fixture","ttl_secs":90}),
    );
    enqueue(&mut fleet);
    fleet
        .store
        .connection()
        .execute(
            "UPDATE task_schedule SET record=json_set(record,'$.created_ms',0)",
            [],
        )
        .unwrap();
    fleet.tick();
    let work = status(&mut fleet, "infra-repair");
    assert_eq!(work["state"], "ready", "{work}");
    assert!(work["reason"].as_str().unwrap().contains("person-started"));
    assert!(fleet.prompts().is_empty());
    call(
        &mut fleet,
        "ship.leave",
        json!({"ship":"infra","holder":"person-fixture"}),
    );
    fleet.tick();
    fleet.settle();
    assert_eq!(fleet.prompts().len(), 1);
    assert_eq!(fleet.store.task("infra-repair").unwrap()["status"], "done");
}

fn status(fleet: &mut Fleet, id: &str) -> Value {
    call(fleet, "work.status", json!({"ship":"infra"}))["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["task_id"] == id)
        .unwrap()
        .clone()
}

fn enqueue(fleet: &mut Fleet) {
    call(
        fleet,
        "task.enqueue",
        json!({"id":"infra-repair","brief":"Verify and complete the prepared fixture."}),
    );
}

fn worker(fleet: &Fleet, body: &str) {
    std::fs::write(fleet.root.join("mate.py"),format!(r#"import json, subprocess, sys, re
cli, ship, prompt = json.loads(sys.argv[1]), sys.argv[2], sys.argv[3]
print(json.dumps({{"type":"thread.started","thread_id":"work-fixture"}}),flush=True)
with open(sys.argv[5],"a") as log: log.write(json.dumps(prompt)+"\n")
def call(command,args):
    p=subprocess.run(cli+[command,"--json",json.dumps(args)],capture_output=True,text=True,check=True)
    return json.loads(p.stdout)
{body}
print(json.dumps({{"type":"turn.completed","usage":{{"input_tokens":100,"output_tokens":20}}}}),flush=True)
"#)).unwrap();
}

#[test]
fn a_headless_ready_task_runs_and_completes_without_mail() {
    let mut fleet = Fleet::new(json!({}));
    worker(
        &fleet,
        "call('task.update', {'id':'infra-repair','actor':ship,'status':'done'})",
    );
    enqueue(&mut fleet);
    let first = fleet.tick();
    assert_eq!(first["work"][0]["state"], "claimed", "{first}");
    fleet.settle();
    assert_eq!(
        call(&mut fleet, "task.show", json!({"id":"infra-repair"}))["status"],
        "done"
    );
    assert_eq!(fleet.prompts().len(), 1);
    fleet.tick();
    assert_eq!(fleet.prompts().len(), 1);
}

#[test]
fn a_successful_process_without_task_progress_faults_instead_of_looping() {
    let mut fleet = Fleet::new(json!({}));
    worker(&fleet, "pass");
    enqueue(&mut fleet);
    fleet.tick();
    fleet.settle();
    fleet.tick();
    assert_eq!(status(&mut fleet, "infra-repair")["state"], "fault");
    for _ in 0..3 {
        fleet.tick();
    }
    assert_eq!(
        fleet.prompts().len(),
        1,
        "no-progress success must not start more model calls"
    );
    let needs = call(&mut fleet, "runtime.needs", json!({}));
    assert_eq!(needs["available"], true, "{needs}");
    assert!(
        needs["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["evidence"] == "scheduling_fault")
    );
}

#[test]
fn typed_time_and_manual_waits_are_distinct_and_survive_reopening() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    enqueue(&mut fleet);
    let until = privateer::store::Store::now_ms() + 100000;
    call(
        &mut fleet,
        "task.defer",
        json!({"id":"infra-repair","wait":{"kind":"time","reason":"An explicit cooldown","until_ms":until}}),
    );
    assert!(poll(&mut fleet, true)["offer"].is_null());
    // Advance only the fixture's persisted deadline; no wall-clock sleep.
    fleet.store.connection().execute("UPDATE task_schedule SET record=json_set(record,'$.not_before_ms',0) WHERE task_id='infra-repair'",[]).unwrap();
    let offer = poll(&mut fleet, true)["offer"].clone();
    assert_eq!(offer["task_id"], "infra-repair");
    call(
        &mut fleet,
        "task.defer",
        json!({"id":"infra-repair","offer_id":offer["id"],"wait":{"kind":"manual","reason":"Explicitly stopped; new direction required"}}),
    );
    for _ in 0..3 {
        assert!(poll(&mut fleet, true)["offer"].is_null());
    }
    assert_eq!(status(&mut fleet, "infra-repair")["state"], "stopped");
    let reopened = privateer::store::Store::open(fleet.store.home()).unwrap();
    assert_eq!(
        dispatch(&mut { reopened }, "work.status", json!({"ship":"infra"})).unwrap()["tasks"][0]["state"],
        "stopped"
    );
}

#[test]
fn a_corrective_child_runs_under_a_parent_waiting_on_the_correction() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    call(
        &mut fleet,
        "task.update",
        json!({"id":"infra-repair","actor":"infra","status":"blocked"}),
    );
    call(
        &mut fleet,
        "task.create",
        json!({"id":"correction","parent":"infra-repair","owner":"infra","title":"Resolve the parent's blocker"}),
    );
    call(
        &mut fleet,
        "task.enqueue",
        json!({"id":"correction","brief":"Apply the bounded correction."}),
    );
    assert_eq!(poll(&mut fleet, true)["offer"]["task_id"], "correction");
}

#[test]
fn stale_or_replaced_attention_never_silently_becomes_healthy() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    call(
        &mut fleet,
        "attention.report",
        json!({"ship":"infra","holder":"host:scheduler-test","kind":"sample","tokens":90000,"window":100000}),
    );
    fleet
        .store
        .connection()
        .execute("UPDATE attention SET at_ms=at_ms-7200000", [])
        .unwrap();
    // The same session's full context stays full however long it sits idle.
    let attention = call(&mut fleet, "attention.show", json!({"ship":"infra"}));
    assert_eq!(attention["pressure"], "high", "{attention}");
    call(
        &mut fleet,
        "ship.attend",
        json!({"ship":"infra","holder":"host:new-session","ttl_secs":90}),
    );
    assert_eq!(
        call(&mut fleet, "attention.show", json!({"ship":"infra"}))["pressure"],
        "unknown"
    );
}

#[test]
fn attention_wait_resumes_on_a_fresh_normal_reading_before_its_deadline() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    enqueue(&mut fleet);
    call(
        &mut fleet,
        "attention.report",
        json!({"ship":"infra","holder":"host:scheduler-test","kind":"sample","tokens":90000,"window":100000}),
    );
    let deferred = call(
        &mut fleet,
        "task.defer",
        json!({"id":"infra-repair","wait":{"kind":"attention","reason":"Wait for context to ease","until_ms":privateer::store::Store::now_ms()+100000}}),
    );
    assert_eq!(deferred["state"], "waiting");
    // A high reading alone does not resume it.
    fleet.tick();
    assert_eq!(
        call(&mut fleet, "work.status", json!({"ship":"infra"}))["tasks"][0]["state"],
        "waiting"
    );
    // A fresh normal sample resumes it before the deadline.
    call(
        &mut fleet,
        "attention.report",
        json!({"ship":"infra","holder":"host:scheduler-test","kind":"sample","tokens":1000,"window":100000}),
    );
    fleet.tick();
    assert_eq!(
        call(&mut fleet, "work.status", json!({"ship":"infra"}))["tasks"][0]["state"],
        "ready"
    );
}

#[test]
fn attention_wait_deadline_hands_off_when_the_reading_never_clears() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    enqueue(&mut fleet);
    call(
        &mut fleet,
        "attention.report",
        json!({"ship":"infra","holder":"host:scheduler-test","kind":"sample","tokens":90000,"window":100000}),
    );
    call(
        &mut fleet,
        "task.defer",
        json!({"id":"infra-repair","wait":{"kind":"attention","reason":"Wait for context to ease","until_ms":privateer::store::Store::now_ms()+100000}}),
    );
    // Reaching the deadline still hands off to a fresh context while high.
    fleet.store.connection().execute("UPDATE task_schedule SET record=json_set(record,'$.not_before_ms',0) WHERE task_id='infra-repair'", []).unwrap();
    fleet.tick();
    assert_eq!(
        call(&mut fleet, "work.status", json!({"ship":"infra"}))["tasks"][0]["state"],
        "ready"
    );
}

#[test]
fn attention_wait_is_refused_when_the_fresh_reading_is_normal() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    enqueue(&mut fleet);
    call(
        &mut fleet,
        "attention.report",
        json!({"ship":"infra","holder":"host:scheduler-test","kind":"sample","tokens":1000,"window":100000}),
    );
    let result = dispatch(
        &mut fleet.store,
        "task.defer",
        json!({"id":"infra-repair","wait":{"kind":"attention","reason":"Wait for context to ease","until_ms":privateer::store::Store::now_ms()+100000}}),
    );
    let error = result.unwrap_err().to_string();
    assert!(error.contains("attention wait refused"), "{error}");
    assert!(error.contains("Attention: normal"), "{error}");
}

#[test]
fn high_attention_explicitly_hands_ready_work_to_a_fresh_task_context() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    worker(
        &fleet,
        "call('task.update', {'id':'infra-repair','actor':ship,'status':'done'})",
    );
    enqueue(&mut fleet);
    let response = call(
        &mut fleet,
        "work.poll",
        json!({"ship":"infra","holder":"host:scheduler-test","idle":true,
        "attention":{"tokens":90000,"window":100000}}),
    );
    assert_eq!(response["state"], "focused_handoff");
    assert!(response["offer"].is_null());
    let report = fleet.tick();
    assert_eq!(report["work"][0]["state"], "claimed", "{report}");
    fleet.settle();
    assert_eq!(fleet.prompts().len(), 1);
    assert_eq!(
        call(&mut fleet, "task.show", json!({"id":"infra-repair"}))["status"],
        "done"
    );
}

#[test]
fn duplicate_claims_and_stale_holder_commands_are_refused() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    enqueue(&mut fleet);
    let offer = poll(&mut fleet, true)["offer"].clone();
    assert!(
        dispatch(
            &mut fleet.store,
            "task.enqueue",
            json!({"id":"infra-repair","brief":"Race the current claim"})
        )
        .is_err()
    );
    call(
        &mut fleet,
        "ship.attend",
        json!({"ship":"infra","holder":"host:replacement","ttl_secs":90}),
    );
    assert!(
        dispatch(
            &mut fleet.store,
            "work.started",
            json!({"ship":"infra","holder":"host:scheduler-test","offer_id":offer["id"]})
        )
        .is_err()
    );
    assert_eq!(status(&mut fleet, "infra-repair")["state"], "offered");
}

#[test]
fn cancelled_or_revoked_tasks_never_receive_an_offer() {
    for revoke in [false, true] {
        let mut fleet = Fleet::new(json!({}));
        attend(&mut fleet);
        enqueue(&mut fleet);
        if revoke {
            call(&mut fleet, "charter.revoke", json!({"id":"fleet"}));
        } else {
            call(
                &mut fleet,
                "task.update",
                json!({"id":"infra-repair","actor":"infra","status":"cancelled"}),
            );
        }
        assert!(poll(&mut fleet, true)["offer"].is_null());
        assert_eq!(fleet.wakes().len(), 0);
    }
}

#[test]
fn a_recorded_launch_intent_recovers_after_a_dispatcher_outage() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    worker(
        &fleet,
        "call('task.update', {'id':'infra-repair','actor':ship,'status':'done'})",
    );
    enqueue(&mut fleet);
    let handoff = call(
        &mut fleet,
        "work.poll",
        json!({"ship":"infra","holder":"host:scheduler-test","idle":true,"attention":null}),
    );
    assert_eq!(handoff["state"], "focused_handoff");
    fleet
        .store
        .connection()
        .execute(
            "UPDATE task_schedule SET record=json_set(record,'$.lease_until_ms',0)",
            [],
        )
        .unwrap();
    // No job was admitted before the outage; replay the SAME launch id.
    let report = fleet.tick();
    assert_eq!(
        report["work"][0]["offer_id"], handoff["offer_id"],
        "{report}"
    );
    fleet.settle();
    fleet.tick();
    assert_eq!(fleet.prompts().len(), 1);
}

#[test]
fn an_unacknowledged_native_offer_expires_to_a_visible_fault_without_retries() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    enqueue(&mut fleet);
    let offer = poll(&mut fleet, true)["offer"].clone();
    fleet
        .store
        .connection()
        .execute(
            "UPDATE task_schedule SET record=json_set(record,'$.lease_until_ms',0)",
            [],
        )
        .unwrap();
    for _ in 0..3 {
        assert!(poll(&mut fleet, true)["offer"].is_null());
    }
    let state = status(&mut fleet, "infra-repair");
    assert_eq!(state["state"], "fault");
    assert_eq!(state["offer_id"], offer["id"]);
    assert_eq!(fleet.wakes().len(), 1);
    let before: i64 = fleet
        .store
        .connection()
        .query_row("SELECT count(*) FROM audit", [], |r| r.get(0))
        .unwrap();
    let observed = call(&mut fleet, "runtime.needs", json!({}));
    assert!(
        observed["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["evidence"] == "scheduling_fault")
    );
    assert_eq!(
        before,
        fleet
            .store
            .connection()
            .query_row("SELECT count(*) FROM audit", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        "observation must not repair state"
    );
}

#[test]
fn rank_and_shared_wake_budget_apply_to_automatic_work() {
    let mut fleet = Fleet::new(json!({"max_wakes_per_hour":1}));
    attend(&mut fleet);
    enqueue(&mut fleet);
    call(
        &mut fleet,
        "task.create",
        json!({"id":"urgent","charter_id":"fleet","owner":"infra","title":"Higher priority"}),
    );
    call(
        &mut fleet,
        "task.enqueue",
        json!({"id":"urgent","brief":"The explicit higher priority task."}),
    );
    call(&mut fleet, "task.rank", json!({"id":"urgent","rank":1}));
    let offer = poll(&mut fleet, true)["offer"].clone();
    assert_eq!(offer["task_id"], "urgent");
    call(
        &mut fleet,
        "work.started",
        json!({"ship":"infra","holder":"host:scheduler-test","offer_id":offer["id"]}),
    );
    call(
        &mut fleet,
        "task.update",
        json!({"id":"urgent","actor":"infra","status":"done"}),
    );
    call(
        &mut fleet,
        "work.settled",
        json!({"ship":"infra","holder":"host:scheduler-test","offer_id":offer["id"]}),
    );
    assert!(
        poll(&mut fleet, true)["offer"].is_null(),
        "hosted work must consume the same wake budget"
    );
    fleet
        .store
        .connection()
        .execute("UPDATE wakes SET created_ms=created_ms-3600001", [])
        .unwrap();
    assert_eq!(poll(&mut fleet, true)["offer"]["task_id"], "infra-repair");
}

#[test]
fn request_waits_resume_only_after_the_correlated_answer() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    enqueue(&mut fleet);
    let request = call(
        &mut fleet,
        "mail.send",
        json!({"id":"acceptance","from":"infra","to":"captain","kind":"request","task_id":"infra-repair","body":"Please confirm the fixture."}),
    );
    call(
        &mut fleet,
        "task.defer",
        json!({"id":"infra-repair","wait":{"kind":"request","ref":request["id"],"reason":"Waiting for owner acceptance"}}),
    );
    assert!(poll(&mut fleet, true)["offer"].is_null());
    call(
        &mut fleet,
        "mail.send",
        json!({"from":"captain","to":"infra","kind":"reply","correlation":request["id"],"body":"Accepted; continue."}),
    );
    assert_eq!(poll(&mut fleet, true)["offer"]["task_id"], "infra-repair");
}

#[test]
fn an_unreadable_wait_faults_its_task_without_stopping_the_dispatcher() {
    let mut fleet = Fleet::new(json!({}));
    enqueue(&mut fleet);
    let request = call(
        &mut fleet,
        "mail.send",
        json!({"from":"infra","to":"captain","kind":"request","task_id":"infra-repair","body":"Please confirm the fixture."}),
    );
    call(
        &mut fleet,
        "task.defer",
        json!({"id":"infra-repair","wait":{"kind":"request","ref":request["id"],"reason":"Waiting for owner acceptance"}}),
    );
    // As if history.prune removed the request while the dispatcher was down.
    fleet
        .store
        .connection()
        .execute("DELETE FROM mail WHERE id=?1", [request["id"].as_str()])
        .unwrap();
    fleet.tick();
    assert_eq!(status(&mut fleet, "infra-repair")["state"], "fault");
}

#[test]
fn concurrent_hosted_polls_create_one_claim() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    enqueue(&mut fleet);
    let home = fleet.store.home().to_path_buf();
    let handles:Vec<_>=(0..4).map(|_|{let home=home.clone();std::thread::spawn(move||{
        let mut store=privateer::store::Store::open(&home).unwrap();
        dispatch(&mut store,"work.poll",json!({"ship":"infra","holder":"host:scheduler-test","idle":true,"attention":{"tokens":1000,"window":100000}})).unwrap()
    })}).collect();
    let responses: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(
        responses.iter().filter(|r| r["offer"].is_object()).count(),
        1,
        "{responses:?}"
    );
    assert_eq!(fleet.wakes().len(), 1);
}

#[test]
fn a_foreign_job_cannot_race_a_scheduled_claim() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    enqueue(&mut fleet);
    let offer = poll(&mut fleet, true)["offer"].clone();
    let foreign = dispatch(
        &mut fleet.store,
        "job.start",
        json!({"task_id":"infra-repair","command":["true"],"heavy":false}),
    );
    assert!(foreign.unwrap_err().to_string().contains("execution claim"));
    let job = fleet.ok(
        "job.start",
        json!({"task_id":"infra-repair","command":["true"],"heavy":false,"work_offer":offer["id"]}),
        None,
    );
    assert!(job["id"].is_string());
    // Let the fixture's supervised payload settle before its home is removed.
    for _ in 0..100 {
        let job = fleet.ok("job.status", json!({"id":job["id"]}), None);
        if job["status"] == "succeeded" {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("fixture job did not settle");
}

#[test]
fn crew_started_under_a_hosted_claim_can_start_their_own_jobs() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    enqueue(&mut fleet);
    let offer = poll(&mut fleet, true)["offer"]["id"].clone();
    // Like a test gate a crew starts: it knows nothing of the offer and
    // inherits the claim from the supervised job that carries it.
    let gate = json!({"task_id":"infra-repair","command":["true"],"heavy":false});
    let out = fleet.root.join("gate.out");
    let crew = fleet.ok(
        "job.start",
        json!({"task_id":"infra-repair","heavy":false,"work_offer":offer,"command":[
            "sh","-c","\"$0\" --home \"$1\" job.start --json \"$2\" >\"$3\" 2>&1",
            support::BIN, fleet.home, gate.to_string(), out]}),
        Some("infra"),
    );
    let settled = |fleet: &Fleet, id: &Value| {
        for _ in 0..250 {
            let job = fleet.ok("job.status", json!({"id":id}), None);
            if !matches!(
                job["status"].as_str(),
                Some("starting" | "queued" | "launching" | "running")
            ) {
                return job;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("fixture job did not settle");
    };
    let crew = settled(&fleet, &crew["id"]);
    let output = std::fs::read_to_string(&out).unwrap();
    assert_eq!(crew["status"], "succeeded", "{output}");
    let gate: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(settled(&fleet, &gate["id"])["status"], "succeeded");
}

#[test]
fn an_old_offer_cannot_launch_after_a_new_assignment_revision() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    enqueue(&mut fleet);
    let offer = poll(&mut fleet, true)["offer"].clone();
    call(
        &mut fleet,
        "task.defer",
        json!({"id":"infra-repair","offer_id":offer["id"],
        "wait":{"kind":"manual","reason":"Assignment withdrawn"}}),
    );
    enqueue(&mut fleet);
    assert_eq!(status(&mut fleet, "infra-repair")["revision"], 2);
    let error = dispatch(
        &mut fleet.store,
        "job.start",
        json!({"id":offer["id"],
        "task_id":"infra-repair","command":["true"],"heavy":false,"work_offer":offer["id"]}),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("no longer an active claim"),
        "{error}"
    );
    assert_eq!(status(&mut fleet, "infra-repair")["state"], "ready");
}

#[test]
fn a_stale_native_release_cannot_reopen_a_handed_off_task() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    enqueue(&mut fleet);
    let offer = poll(&mut fleet, true)["offer"].clone();
    call(
        &mut fleet,
        "task.handoff",
        json!({"id":"infra-repair","actor":"infra","target":"apps"}),
    );
    call(
        &mut fleet,
        "task.accept",
        json!({"id":"infra-repair","actor":"apps"}),
    );
    let before = fleet.store.task("infra-repair").unwrap();
    assert!(
        dispatch(
            &mut fleet.store,
            "work.release",
            json!({"ship":"infra",
        "holder":"host:scheduler-test","offer_id":offer["id"]})
        )
        .is_err()
    );
    assert_eq!(fleet.store.task("infra-repair").unwrap(), before);
    fleet.tick();
    let record: String = fleet
        .store
        .connection()
        .query_row(
            "SELECT record FROM task_schedule WHERE task_id='infra-repair'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&record).unwrap()["state"],
        "fault"
    );
}

#[test]
fn an_unavailable_profile_faults_its_schedule_without_stopping_the_dispatcher() {
    let mut fleet = Fleet::new(json!({}));
    enqueue(&mut fleet);
    call(
        &mut fleet,
        "ship.put",
        json!({"name":"infra","scope":"deploys"}),
    );
    let report = fleet.tick();
    assert!(report["work"].as_array().unwrap().is_empty());
    let work = status(&mut fleet, "infra-repair");
    assert_eq!(work["state"], "fault", "{work}");
    assert!(
        work["reason"]
            .as_str()
            .unwrap()
            .contains("profile unavailable")
    );
}

#[test]
fn scheduled_payload_survives_dispatcher_death_and_restart_without_duplication() {
    use std::{
        fs,
        process::{Child, Stdio},
        time::{Duration, Instant},
    };
    struct Runtime {
        children: Vec<Child>,
        release: std::path::PathBuf,
    }
    impl Drop for Runtime {
        fn drop(&mut self) {
            let _ = fs::write(&self.release, "release");
            for child in &mut self.children {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    let mut fleet = Fleet::new(json!({}));
    worker(
        &fleet,
        r#"import pathlib,time,os
root=pathlib.Path(sys.argv[4]).parent
(root/'worker-ready').write_text(str(os.getpid()))
while not (root/'release').exists(): time.sleep(0.02)
call('task.update', {'id':'infra-repair','actor':ship,'status':'done'})"#,
    );
    enqueue(&mut fleet);
    let mut runtime = Runtime {
        children: vec![],
        release: fleet.root.join("release"),
    };
    fn launch(fleet: &Fleet) -> Child {
        fleet
            .command()
            .args(["serve", "--json", r#"{"interval_secs":1}"#])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }
    runtime.children.push(launch(&fleet));
    let until = Instant::now() + Duration::from_secs(15);
    while !fleet.root.join("worker-ready").exists() {
        assert!(
            Instant::now() < until,
            "scheduled worker never reached the gate"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    let offer = status(&mut fleet, "infra-repair")["offer_id"].clone();
    let before = fleet.ok("job.status", json!({"id":offer}), None);
    assert_eq!(before["status"], "running");
    let payload = before["payload"].clone();
    assert!(payload["pid"].is_number());
    runtime.children[0].kill().unwrap();
    runtime.children[0].wait().unwrap();
    assert_eq!(
        fleet.ok("job.status", json!({"id":offer}), None)["status"],
        "running",
        "the actual payload must outlive its dispatcher"
    );
    let generation_before: Option<String> = fleet
        .store
        .connection()
        .query_row(
            "SELECT value FROM metadata WHERE key='dispatcher_generation'",
            [],
            |r| r.get(0),
        )
        .ok();
    runtime.children.push(launch(&fleet));
    // Observe the new dispatcher generation, rather than sleeping and assuming
    // restart/reconciliation occurred.
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        let generation: Option<String> = fleet
            .store
            .connection()
            .query_row(
                "SELECT value FROM metadata WHERE key='dispatcher_generation'",
                [],
                |r| r.get(0),
            )
            .ok();
        if generation != generation_before {
            break;
        }
        assert!(
            Instant::now() < until,
            "dispatcher generation never changed"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    let resumed = fleet.ok("job.status", json!({"id":offer}), None);
    assert_eq!(resumed["payload"], payload);
    assert_eq!(resumed["status"], "running");
    fs::write(&runtime.release, "release").unwrap();
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        let job = fleet.ok("job.status", json!({"id":offer}), None);
        if job["status"] == "succeeded" {
            break;
        }
        assert!(Instant::now() < until, "payload failed to finish: {job}");
        std::thread::sleep(Duration::from_millis(30));
    }
    for child in &mut runtime.children {
        let _ = child.kill();
        let _ = child.wait();
    }
    runtime.children.clear();
    fleet.tick();
    assert_eq!(
        fleet.prompts().len(),
        1,
        "restart must not invoke a second coordinator"
    );
    assert_eq!(fleet.wakes().len(), 1);
    assert_eq!(
        call(&mut fleet, "task.show", json!({"id":"infra-repair"}))["status"],
        "done"
    );
}
