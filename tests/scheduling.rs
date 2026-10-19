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
    ledger.execute_batch("CREATE TABLE IF NOT EXISTS model_routes(route TEXT PRIMARY KEY,failures INTEGER NOT NULL,until_ms INTEGER NOT NULL,last_error TEXT,updated_ms INTEGER NOT NULL)").unwrap();
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

#[test]
fn a_taken_up_offer_that_defers_on_a_child_is_never_counted_as_an_attempt() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    enqueue(&mut fleet);
    for round in 0..4 {
        let child = format!("infra-child-{round}");
        call(
            &mut fleet,
            "task.create",
            json!({"id":child,"parent":"infra-repair","owner":"infra","title":"child","actor":"infra"}),
        );
        let offer = poll(&mut fleet, true)["offer"].clone();
        assert!(offer.is_object(), "round {round}: {offer}");
        call(
            &mut fleet,
            "work.started",
            json!({"ship":"infra","holder":"host:scheduler-test","offer_id":offer["id"]}),
        );
        call(
            &mut fleet,
            "task.defer",
            json!({"id":"infra-repair","offer_id":offer["id"],
            "wait":{"kind":"task","reason":"child correction","ref":child}}),
        );
        assert_eq!(status(&mut fleet, "infra-repair")["attempts"], 0);
        call(
            &mut fleet,
            "task.update",
            json!({"id":child,"actor":"infra","status":"done"}),
        );
    }
    let state = status(&mut fleet, "infra-repair");
    assert_ne!(state["state"], "fault", "{state}");
}

#[test]
fn failed_deliveries_fault_after_three_attempts_and_notify_the_owner_once() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    enqueue(&mut fleet);
    for round in 0..3 {
        let offer = poll(&mut fleet, true)["offer"].clone();
        assert!(offer.is_object(), "round {round}: {offer}");
        call(
            &mut fleet,
            "work.release",
            json!({"ship":"infra","holder":"host:scheduler-test","offer_id":offer["id"]}),
        );
        fleet
            .store
            .connection()
            .execute(
                "UPDATE task_schedule SET record=json_set(record,'$.not_before_ms',0)",
                [],
            )
            .unwrap();
    }
    assert!(poll(&mut fleet, true)["offer"].is_null());
    let state = status(&mut fleet, "infra-repair");
    assert_eq!(state["state"], "fault", "{state}");
    assert!(
        state["reason"]
            .as_str()
            .unwrap()
            .contains("retry budget exhausted")
    );
    for _ in 0..2 {
        poll(&mut fleet, true);
    }
    let notices: i64 = fleet
        .store
        .connection()
        .query_row(
            "SELECT count(*) FROM mail WHERE recipient='infra' AND kind='notice' AND task_id='infra-repair'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(notices, 1);
}

/// The backlog alarm's report for infra in one dispatcher pass.
fn backlog(report: &Value) -> Value {
    report["backlog"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["ship"] == "infra")
        .cloned()
        .unwrap_or_else(|| panic!("no backlog report: {report}"))
}

/// As if `secs` passed: the alarm's last fire and the wake records move back.
fn pass(fleet: &Fleet, secs: i64) {
    let ms = secs * 1000;
    let db = fleet.store.connection();
    db.execute(
        "UPDATE backlog_alarms SET fired_ms=fired_ms-?1, next_ms=next_ms-?1",
        [ms],
    )
    .unwrap();
    db.execute(
        "UPDATE wakes SET created_ms=created_ms-?1, finished_ms=finished_ms-?1",
        [ms],
    )
    .unwrap();
}

fn alarm(fleet: &mut Fleet) -> Value {
    call(fleet, "work.status", json!({"ship":"infra"}))["backlog_alarm"].clone()
}

/// The backlog notices infra has been sent, oldest first.
fn notices(fleet: &Fleet) -> Vec<Value> {
    let db = fleet.store.connection();
    let mut statement = db
        .prepare(
            "SELECT id, sender, kind, body, payload, task_id FROM mail
             WHERE recipient='infra' AND correlation='backlog-infra' ORDER BY seq",
        )
        .unwrap();
    statement
        .query_map([], |r| {
            Ok(
                json!({"id": r.get::<_, String>(0)?, "from": r.get::<_, String>(1)?,
                "kind": r.get::<_, String>(2)?, "body": r.get::<_, String>(3)?,
                "payload": serde_json::from_str::<Value>(&r.get::<_, String>(4)?).unwrap(),
                "task_id": r.get::<_, Option<String>>(5)?}),
            )
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn audits(fleet: &mut Fleet, kind: &str) -> usize {
    call(fleet, "audit.list", json!({"limit":1000}))["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == kind)
        .count()
}

fn task(fleet: &mut Fleet, id: &str) {
    call(
        fleet,
        "task.create",
        json!({"id":id,"charter_id":"fleet","owner":"infra","title":format!("Draft {id}")}),
    );
}

/// An upgrade never fires on its first pass: a ship with no alarm row counts
/// as just fired, and its first notice comes one interval later.
#[test]
fn the_first_backlog_check_after_an_upgrade_fires_nothing() {
    let mut fleet = Fleet::new(support::now_settings());
    let first = fleet.tick();
    assert_eq!(backlog(&first)["skipped"], "first backlog check", "{first}");
    let again = fleet.tick();
    assert_eq!(backlog(&again)["skipped"], "spaced", "{again}");
    assert!(notices(&fleet).is_empty());
    assert!(fleet.wakes().is_empty());
    pass(&fleet, 1800);
    assert_eq!(backlog(&fleet.tick())["skipped"], "spaced");
    pass(&fleet, 1800);
    assert_eq!(backlog(&fleet.tick())["fired"], "backlog-infra-1");
}

/// A quiet headless ship with an open, unscheduled task gets one backlog
/// notice, which wakes its first mate on a stable backlog thread, and no
/// second one within the hour.
#[test]
fn a_quiet_ship_is_woken_once_for_its_backlog_and_not_again_within_the_hour() {
    let mut fleet = Fleet::new(support::now_settings());
    fleet.tick();
    pass(&fleet, 3600);
    let report = fleet.tick();
    let fired = backlog(&report);
    assert_eq!(fired["fired"], "backlog-infra-1", "{report}");
    assert_eq!(fired["considered"], json!(["infra-repair"]));
    assert_eq!(fired["total"], 1);
    // The notice is a crew notice on the ship's backlog thread, not a reply
    // and not about one task, and the headless wake takes it this pass.
    let sent = notices(&fleet);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["from"], "crew");
    assert_eq!(sent[0]["kind"], "notice");
    assert!(sent[0]["task_id"].is_null());
    let body = sent[0]["body"].as_str().unwrap();
    assert!(
        body.contains("- infra-repair: infra duty [unscheduled]"),
        "{body}"
    );
    assert!(body.contains("For each: write assignment:<id>"), "{body}");
    assert!(!body.contains("Attention is high"), "{body}");
    let wake = Fleet::infra(&report);
    assert_eq!(wake["focus"], "thread:backlog-infra", "{wake}");
    assert_eq!(wake["task_id"], "infra-duty");
    assert_eq!(fleet.settle()["status"], "succeeded");
    assert!(fleet.prompts()[0].contains("Backlog check"));
    // The thread is a focus: its note carries to the next backlog wake.
    let note = call(
        &mut fleet,
        "note.get",
        json!({"ship":"infra","focus":"thread:backlog-infra"}),
    );
    assert_eq!(note["notes"].as_array().unwrap().len(), 1, "{note}");
    // Within the hour, nothing more.
    pass(&fleet, 1800);
    let later = fleet.tick();
    assert_eq!(backlog(&later)["skipped"], "spaced", "{later}");
    assert_eq!(notices(&fleet).len(), 1);
    assert_eq!(fleet.wakes().len(), 1);
    assert_eq!(audits(&mut fleet, "backlog.alarm"), 1);
    let state = alarm(&mut fleet);
    assert_eq!(state["fires"], 1, "{state}");
    assert_eq!(state["considered"], json!(["infra-repair"]));
}

/// A wake that changes nothing is evaluated once per fire and the next
/// arrival fires: 0, 2, 6 and 14 hours, then every 8. Each pass is a fresh
/// dispatcher process, so the backoff survives restarts.
#[test]
fn an_unproductive_backlog_wake_backs_off_to_2_6_and_14_hours() {
    let mut fleet = Fleet::new(support::now_settings());
    fleet.tick();
    pass(&fleet, 3600);
    let mut fired = vec![];
    let mut backoffs = vec![];
    for hour in 0..=15 {
        if hour > 0 {
            pass(&fleet, 3600);
        }
        let report = fleet.tick();
        let check = backlog(&report);
        if check["fired"].is_string() {
            fired.push(hour);
            backoffs.push(check["backoff_secs"].as_i64().unwrap());
            assert_eq!(fleet.settle()["status"], "succeeded");
        } else {
            assert_eq!(check["skipped"], "spaced", "hour {hour}: {report}");
        }
    }
    assert_eq!(fired, [0, 2, 6, 14]);
    assert_eq!(backoffs, [3600, 7200, 14400, 28800]);
    let state = alarm(&mut fleet);
    assert_eq!(state["fires"], 4, "{state}");
    assert_eq!(state["quiet"], 3, "{state}");
    assert_eq!(state["evaluated"], false, "{state}");
    assert_eq!(audits(&mut fleet, "backlog.unproductive"), 3);
    assert_eq!(audits(&mut fleet, "backlog.productive"), 0);
    assert_eq!(notices(&fleet).len(), 4);
    assert_eq!(fleet.wakes().len(), 4);
}

/// Tasks something else will move are not a backlog: a deferred one with a
/// wait, a blocked one, one handed off, the duty task and a running one
/// waiting on a request it sent. A new open task is then listed alone.
#[test]
fn deferred_blocked_handed_off_and_asking_tasks_raise_no_backlog_alarm() {
    let mut fleet = Fleet::new(support::now_settings());
    enqueue(&mut fleet);
    call(
        &mut fleet,
        "task.defer",
        json!({"id":"infra-repair","wait":{"kind":"time","reason":"After the window",
            "until_ms":privateer::store::Store::now_ms() + 86_400_000}}),
    );
    task(&mut fleet, "infra-blocked");
    call(
        &mut fleet,
        "task.update",
        json!({"id":"infra-blocked","actor":"infra","status":"blocked"}),
    );
    task(&mut fleet, "infra-offered");
    call(
        &mut fleet,
        "task.handoff",
        json!({"id":"infra-offered","actor":"infra","target":"apps"}),
    );
    task(&mut fleet, "infra-asking");
    call(
        &mut fleet,
        "task.update",
        json!({"id":"infra-asking","actor":"infra","status":"running"}),
    );
    fleet.send(json!({"from":"infra","to":"apps","kind":"request",
        "body":"Which window is free?","task_id":"infra-asking"}));
    // Open and unscheduled, but waiting on an answer the ship asked for: only
    // the open-request clause keeps it off the list, since it is not running.
    task(&mut fleet, "infra-open-asking");
    fleet.send(json!({"from":"infra","to":"apps","kind":"request",
        "body":"Which deploy window?","task_id":"infra-open-asking"}));
    fleet.tick();
    pass(&fleet, 3600);
    let report = fleet.tick();
    assert_eq!(backlog(&report)["skipped"], "no candidates", "{report}");
    assert!(notices(&fleet).is_empty());
    // A new open task is the only candidate.
    task(&mut fleet, "infra-new");
    let report = fleet.tick();
    let fired = backlog(&report);
    assert_eq!(fired["fired"], "backlog-infra-1", "{report}");
    assert_eq!(fired["considered"], json!(["infra-new"]));
    assert_eq!(fired["total"], 1);
}

/// A stowed fleet wakes nobody and leaves the alarm's spacing as it was.
#[test]
fn a_stowed_ship_is_not_woken_for_its_backlog() {
    let mut fleet = Fleet::new(support::now_settings());
    fleet.tick();
    pass(&fleet, 3600);
    let ledger = rusqlite::Connection::open(fleet.root.join("ledger/resources.sqlite")).unwrap();
    ledger
        .execute(
            "INSERT INTO stows(id, started_ms, ships) VALUES('stow-test', 1, '[]')",
            [],
        )
        .unwrap();
    let before = alarm(&mut fleet);
    let report = fleet.tick();
    assert_eq!(backlog(&report)["skipped"], "stowed", "{report}");
    assert!(notices(&fleet).is_empty());
    assert_eq!(alarm(&mut fleet), before);
    ledger
        .execute("UPDATE stows SET resumed_ms=2 WHERE id='stow-test'", [])
        .unwrap();
    assert_eq!(backlog(&fleet.tick())["fired"], "backlog-infra-1");
    assert_eq!(notices(&fleet).len(), 1);
}

/// A quiet pass with nothing new writes no alarm row: the row is read on every
/// tick, and only a change to it may write (the trigger counts UPDATEs).
#[test]
fn a_quiet_tick_with_nothing_changed_leaves_the_alarm_row_untouched() {
    let fleet = Fleet::new(support::now_settings());
    fleet
        .store
        .connection()
        .execute_batch(
            "CREATE TABLE alarm_writes(n INTEGER NOT NULL);
             INSERT INTO alarm_writes VALUES(0);
             CREATE TRIGGER alarm_writes_count AFTER UPDATE ON backlog_alarms
             BEGIN UPDATE alarm_writes SET n=n+1; END;",
        )
        .unwrap();
    let writes = |fleet: &Fleet| -> i64 {
        fleet
            .store
            .connection()
            .query_row("SELECT n FROM alarm_writes", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(backlog(&fleet.tick())["skipped"], "first backlog check");
    // The first pass after the row exists lists the candidate and may write
    // its new candidate set once; after that nothing changes.
    assert_eq!(backlog(&fleet.tick())["skipped"], "spaced");
    let settled = writes(&fleet);
    for _ in 0..3 {
        assert_eq!(backlog(&fleet.tick())["skipped"], "spaced");
    }
    assert_eq!(writes(&fleet), settled);
    assert!(notices(&fleet).is_empty());
}

/// A task that faults, is re-enqueued and faults again is a new revision, so
/// the wake that listed its first fault counts as productive.
#[test]
fn a_fault_re_enqueued_and_faulted_again_counts_as_productive() {
    let mut fleet = Fleet::new(support::now_settings());
    enqueue(&mut fleet);
    fleet
        .store
        .connection()
        .execute(
            "UPDATE task_schedule SET record=json_set(record,'$.state','fault','$.reason','first')
             WHERE task_id='infra-repair'",
            [],
        )
        .unwrap();
    fleet.tick();
    pass(&fleet, 3600);
    let first = backlog(&fleet.tick());
    assert_eq!(first["fired"], "backlog-infra-1", "{first}");
    fleet.settle();
    // Re-enqueued (a new revision) and faulted again before the next check.
    call(
        &mut fleet,
        "task.enqueue",
        json!({"id":"infra-repair","brief":"Try again."}),
    );
    fleet
        .store
        .connection()
        .execute(
            "UPDATE task_schedule SET record=json_set(record,'$.state','fault','$.reason','again')
             WHERE task_id='infra-repair'",
            [],
        )
        .unwrap();
    pass(&fleet, 3600);
    fleet.tick();
    assert_eq!(audits(&mut fleet, "backlog.productive"), 1);
    assert_eq!(audits(&mut fleet, "backlog.unproductive"), 0);
}

/// With more candidates than a notice lists, only a handled task resets the
/// backoff: an untouched backlog keeps backing off, and the notice lists ten
/// and counts the rest.
#[test]
fn a_large_backlog_backs_off_until_one_listed_task_is_handled() {
    let mut fleet = Fleet::new(support::now_settings());
    call(
        &mut fleet,
        "task.update",
        json!({"id":"infra-repair","actor":"infra","status":"done"}),
    );
    for n in 1..=15 {
        task(&mut fleet, &format!("infra-d{n:02}"));
    }
    fleet.tick();
    pass(&fleet, 3600);
    let first = backlog(&fleet.tick());
    assert_eq!(first["fired"], "backlog-infra-1", "{first}");
    assert_eq!(first["considered"].as_array().unwrap().len(), 10);
    assert_eq!(first["total"], 15);
    let body = notices(&fleet)[0]["body"].as_str().unwrap().to_owned();
    assert!(body.contains("... and 5 more"), "{body}");
    assert!(body.len() < 2048, "{}", body.len());
    fleet.settle();
    // Untouched: the evaluation backs off rather than seeing five "new" tasks.
    pass(&fleet, 3600);
    let check = backlog(&fleet.tick());
    assert_eq!(check["skipped"], "spaced", "{check}");
    assert_eq!(check["backoff_secs"], 7200);
    pass(&fleet, 3600);
    assert_eq!(backlog(&fleet.tick())["fired"], "backlog-infra-2");
    fleet.settle();
    // One listed task handled: productive, back to the hour, and it fires now.
    call(
        &mut fleet,
        "task.update",
        json!({"id":"infra-d01","actor":"infra","status":"blocked"}),
    );
    pass(&fleet, 7200);
    let check = backlog(&fleet.tick());
    assert_eq!(check["fired"], "backlog-infra-3", "{check}");
    assert_eq!(check["backoff_secs"], 3600);
    assert_eq!(check["total"], 14);
    assert!(
        !check["considered"]
            .as_array()
            .unwrap()
            .contains(&json!("infra-d01"))
    );
    let state = alarm(&mut fleet);
    assert_eq!(state["quiet"], 0, "{state}");
    assert_eq!(audits(&mut fleet, "backlog.productive"), 1);
    fleet.settle();
    // Untouched again: backing off again.
    pass(&fleet, 3600);
    let check = backlog(&fleet.tick());
    assert_eq!(check["backoff_secs"], 7200, "{check}");
    assert_eq!(alarm(&mut fleet)["quiet"], 1);
}

fn settle_job(fleet: &Fleet, id: &Value) -> Value {
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
}

#[test]
fn errand_start_under_a_claim_takes_only_its_live_offer() {
    let mut fleet = Fleet::new(support::now_settings());
    attend(&mut fleet);
    enqueue(&mut fleet);
    let offer = poll(&mut fleet, true)["offer"]["id"].clone();
    let brief = json!({"focus":"task:infra-repair","brief":"Review the claimed work."});
    let refused = fleet.refused("errand.start", brief.clone(), Some("infra"));
    assert!(refused.contains("execution claim"), "{refused}");
    let mut wrong = brief.clone();
    wrong["work_offer"] = json!("work-not-the-claim");
    let refused = fleet.refused("errand.start", wrong, Some("infra"));
    assert!(refused.contains("execution claim"), "{refused}");
    let mut current = brief;
    current["work_offer"] = offer;
    let started = fleet.ok("errand.start", current, Some("infra"));
    assert_eq!(started["status"], "running", "{started}");
}

#[test]
fn a_resume_under_a_claim_continues_work_the_claim_admitted() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    enqueue(&mut fleet);
    let offer = poll(&mut fleet, true)["offer"]["id"].clone();
    let crew = fleet.ok(
        "job.start",
        json!({"task_id":"infra-repair","command":["true"],"heavy":false,"work_offer":offer}),
        None,
    );
    assert_eq!(settle_job(&fleet, &crew["id"])["status"], "succeeded");
    // A resume names the parked crew it continues, and the claim admits it
    // through that crew's lineage.
    let resume = fleet.ok(
        "job.start",
        json!({"id":"crew-resume-test-1","task_id":"infra-repair","command":["true"],"heavy":false,
        "_resume_of":crew["id"]}),
        None,
    );
    assert_eq!(resume["id"], "crew-resume-test-1", "{resume}");
    assert_eq!(settle_job(&fleet, &resume["id"])["status"], "succeeded");
    // A launch that is no resume of anything stays refused.
    let unrelated = fleet.refused(
        "job.start",
        json!({"task_id":"infra-repair","command":["true"],"heavy":false}),
        None,
    );
    assert!(unrelated.contains("execution claim"), "{unrelated}");
}

#[test]
fn a_crew_started_before_a_claim_resumes_under_it() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    let crew = fleet.ok(
        "job.start",
        json!({"task_id":"infra-repair","command":["true"],"heavy":false}),
        None,
    );
    assert_eq!(settle_job(&fleet, &crew["id"])["status"], "succeeded");
    enqueue(&mut fleet);
    poll(&mut fleet, true);
    let resume = fleet.ok(
        "job.start",
        json!({"id":"crew-resume-test-2","task_id":"infra-repair","command":["true"],"heavy":false,
        "_resume_of":crew["id"]}),
        None,
    );
    assert_eq!(settle_job(&fleet, &resume["id"])["status"], "succeeded");
    let unrelated = fleet.refused(
        "job.start",
        json!({"task_id":"infra-repair","command":["true"],"heavy":false}),
        None,
    );
    assert!(unrelated.contains("execution claim"), "{unrelated}");
}

/// A crew started under the live claim, its job settled, for resume tests.
fn claimed_crew(fleet: &mut Fleet, id: &str) -> Value {
    attend(fleet);
    enqueue(fleet);
    let offer = poll(fleet, true)["offer"]["id"].clone();
    let crew = fleet.ok(
        "job.start",
        json!({"id":id,"task_id":"infra-repair","command":["true"],"heavy":false,"work_offer":offer}),
        None,
    );
    assert_eq!(settle_job(fleet, &crew["id"])["status"], "succeeded");
    crew
}

#[test]
fn a_crew_resumed_twice_under_a_claim_resumes_both_times() {
    let mut fleet = Fleet::new(json!({}));
    let crew = claimed_crew(&mut fleet, "crew-a0");
    // Each resume names the one before it, as a second park does through the
    // runtime's own launch. The second resume reaches the claim through the
    // first resume's record, not through the original crew alone.
    let first = fleet.ok(
        "job.start",
        json!({"id":"crew-a1","task_id":"infra-repair","command":["true"],"heavy":false,
        "_resume_of":crew["id"]}),
        None,
    );
    assert_eq!(settle_job(&fleet, &first["id"])["status"], "succeeded");
    let second = fleet.ok(
        "job.start",
        json!({"id":"crew-a2","task_id":"infra-repair","command":["true"],"heavy":false,
        "_resume_of":first["id"]}),
        None,
    );
    assert_eq!(settle_job(&fleet, &second["id"])["status"], "succeeded");
}

#[test]
fn a_resumed_crew_starts_its_own_job_under_the_claim() {
    let mut fleet = Fleet::new(json!({}));
    let crew = claimed_crew(&mut fleet, "crew-b0");
    let gate = json!({"task_id":"infra-repair","command":["true"],"heavy":false});
    let out = fleet.root.join("resumed-gate.out");
    // The resumed crew runs as the first mate's agent actor and starts its own
    // gate job, which must inherit the claim through the resume it continues.
    let resume = fleet.ok(
        "job.start",
        json!({"id":"crew-b1","task_id":"infra-repair","heavy":false,"agent_actor":"infra",
        "_resume_of":crew["id"],"command":[
            "env","PRIVATEER_ROLE=agent","PRIVATEER_ACTOR=infra",
            "sh","-c","\"$0\" --home \"$1\" job.start --json \"$2\" >\"$3\" 2>&1",
            support::BIN, fleet.home, gate.to_string(), out]}),
        None,
    );
    assert_eq!(settle_job(&fleet, &resume["id"])["status"], "succeeded");
    let output = std::fs::read_to_string(&out).unwrap();
    let gate: Value = serde_json::from_str(&output).unwrap_or_else(|_| panic!("{output}"));
    assert_eq!(settle_job(&fleet, &gate["id"])["status"], "succeeded");
}

#[test]
fn an_agent_cannot_pass_a_resume_to_get_past_the_claim() {
    let mut fleet = Fleet::new(json!({}));
    let crew = claimed_crew(&mut fleet, "crew-c0");
    // Only the runtime's own launch may name a resume; an agent's naming of
    // the crew is ignored, so the launch is an unrelated one and is refused.
    let refused = fleet.refused(
        "job.start",
        json!({"id":"crew-c1","task_id":"infra-repair","command":["true"],"heavy":false,
        "_resume_of":crew["id"]}),
        Some("infra"),
    );
    assert!(refused.contains("execution claim"), "{refused}");
}

#[test]
fn a_woken_first_mate_errand_takes_its_live_offer_without_naming_it() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    enqueue(&mut fleet);
    let offer = poll(&mut fleet, true)["offer"]["id"].clone();
    // The woken first mate's job carries the live offer; its errand inherits
    // that offer without the caller naming it. The job runs the errand CLI as
    // the agent, so the errand sees its supervising job through PRIVATEER_JOB.
    let brief = json!({"focus":"task:infra-repair","brief":"Review the claimed work."});
    let out = fleet.root.join("woken-errand.out");
    let mate = fleet.ok(
        "job.start",
        json!({"id":"mate-d1","task_id":"infra-repair","heavy":false,"work_offer":offer,"command":[
            "env","PRIVATEER_ROLE=agent","PRIVATEER_ACTOR=infra",
            "sh","-c","\"$0\" --home \"$1\" errand.start --json \"$2\" >\"$3\" 2>&1",
            support::BIN, fleet.home, brief.to_string(), out]}),
        None,
    );
    assert_eq!(settle_job(&fleet, &mate["id"])["status"], "succeeded");
    let output = std::fs::read_to_string(&out).unwrap();
    let started: Value = serde_json::from_str(&output).unwrap_or_else(|_| panic!("{output}"));
    assert_eq!(started["status"], "running", "{started}");
}

#[test]
fn a_released_offer_cannot_start_an_errand() {
    let mut fleet = Fleet::new(json!({}));
    attend(&mut fleet);
    enqueue(&mut fleet);
    let offer = poll(&mut fleet, true)["offer"]["id"].clone();
    let until = privateer::store::Store::now_ms() + 100000;
    call(
        &mut fleet,
        "task.defer",
        json!({"id":"infra-repair","offer_id":offer,"wait":{"kind":"time","reason":"Release the claim","until_ms":until}}),
    );
    // The released offer is no longer the claim, so an errand naming it is
    // refused rather than admitted by the stale record.
    let refused = fleet.refused(
        "errand.start",
        json!({"focus":"task:infra-repair","brief":"Review the claimed work.","work_offer":offer}),
        Some("infra"),
    );
    assert!(refused.contains("no longer an active claim"), "{refused}");
}

#[test]
fn a_resume_of_an_unrelated_job_cannot_pass_the_claim() {
    let mut fleet = Fleet::new(json!({}));
    call(
        &mut fleet,
        "task.create",
        json!({"id":"infra-other","charter_id":"fleet","owner":"infra","title":"infra other"}),
    );
    // Admitted before the claim, but on another task: it is neither under the
    // offer nor a crew of this task, so its named resume is refused.
    let other = fleet.ok(
        "job.start",
        json!({"id":"other-1","task_id":"infra-other","command":["true"],"heavy":false}),
        None,
    );
    assert_eq!(settle_job(&fleet, &other["id"])["status"], "succeeded");
    attend(&mut fleet);
    enqueue(&mut fleet);
    poll(&mut fleet, true);
    let refused = fleet.refused(
        "job.start",
        json!({"id":"crew-f1","task_id":"infra-repair","command":["true"],"heavy":false,
        "_resume_of":other["id"]}),
        None,
    );
    assert!(refused.contains("execution claim"), "{refused}");
}
