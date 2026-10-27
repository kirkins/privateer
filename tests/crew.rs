//! Crew outcomes wake the first mate: real supervised jobs, a real dispatcher
//! and the scripted first mate from the activation tests.
mod support;

use privateer::dispatch;
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};
use support::{Fleet, now_settings};

/// Start a light crew job for infra, as infra.
fn crew(fleet: &Fleet, id: &str, task: &str, script: &str) -> Value {
    fleet.ok(
        "job.start",
        json!({"id": id, "task_id": task, "command": ["sh", "-c", script], "heavy": false}),
        Some("infra"),
    )
}

fn wait(fleet: &Fleet, id: &str, accepted: &[&str]) -> Value {
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let job = fleet.ok("job.status", json!({"id": id}), None);
        if accepted.contains(&job["status"].as_str().unwrap_or("")) {
            return job;
        }
        assert!(
            Instant::now() < until,
            "job {id} never reached {accepted:?}: {job}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// All mail from crew a ship ever received, acknowledged or not.
fn from_crew(fleet: &mut Fleet, ship: &str) -> Vec<Value> {
    let inbox = dispatch(
        &mut fleet.store,
        "mail.inbox",
        json!({"address": ship, "all": true, "limit": 1000}),
    )
    .unwrap();
    inbox["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["from"] == "crew")
        .cloned()
        .collect()
}

fn outcomes_of(fleet: &mut Fleet, job: &str) -> Vec<Value> {
    from_crew(fleet, "infra")
        .into_iter()
        .filter(|m| m["payload"]["job_id"] == job && m["kind"] == "notice")
        .filter(|m| m["payload"]["status"].is_string())
        .collect()
}

fn kill(pid: &Value, group: bool) {
    let pid = pid.as_i64().unwrap() as i32;
    unsafe { libc::kill(if group { -pid } else { pid }, libc::SIGKILL) };
}

/// Wait until a recorded process is gone (or only a zombie), on Linux and
/// macOS alike.
fn gone(pid: &Value) {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&output.stdout);
        if stat.trim().is_empty() || stat.trim().starts_with('Z') {
            return;
        }
        assert!(Instant::now() < until, "process {pid} survived");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_finished_crew_job_wakes_its_first_mate_under_the_task_focus() {
    let mut fleet = Fleet::new(now_settings());
    crew(&fleet, "build-1", "infra-repair", "echo built the thing");
    let job = wait(&fleet, "build-1", &["succeeded"]);
    let outcomes = outcomes_of(&mut fleet, "build-1");
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    let outcome = &outcomes[0];
    assert_eq!(
        outcome["id"],
        format!("crew-build-1-{}", job["incarnation"].as_str().unwrap())
    );
    assert_eq!(outcome["to"], "infra");
    assert_eq!(outcome["kind"], "notice");
    assert_eq!(outcome["urgency"], "normal");
    assert_eq!(outcome["task_id"], "infra-repair");
    assert_eq!(outcome["payload"]["status"], "succeeded");
    assert_eq!(outcome["payload"]["exit_code"], 0);
    let body = outcome["body"].as_str().unwrap();
    assert!(
        body.starts_with("Crew job build-1 succeeded with exit code 0"),
        "{body}"
    );
    assert!(body.contains("built the thing"), "{body}");

    let woke = Fleet::infra(&fleet.tick());
    assert_eq!(woke["status"], "running", "{woke}");
    assert_eq!(woke["focus"], "task:infra-repair");
    assert_eq!(woke["task_id"], "infra-repair");
    assert_eq!(fleet.settle()["status"], "succeeded");
    let prompt = fleet.prompts().pop().unwrap();
    assert!(prompt.contains("## task:infra-repair"), "{prompt}");
    assert!(prompt.contains("from crew kind notice urgency normal"));
    assert!(prompt.contains("Crew job build-1 succeeded"));
    assert!(prompt.contains("An exit status is not proof"));
    // The body states the payload; the wake does not repeat it.
    assert!(!prompt.contains("\"incarnation\""), "{prompt}");
    // The scripted first mate saw and acknowledged it.
    let unread = dispatch(&mut fleet.store, "mail.inbox", json!({"address": "infra"})).unwrap();
    assert_eq!(unread["messages"], json!([]));
    // The wake's own job is not crew: no outcome for it, and no further wake.
    assert_eq!(from_crew(&mut fleet, "infra").len(), 1);
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "nothing wake-worthy"
    );
}

#[test]
fn a_failing_crew_job_reports_high_urgency_with_its_exit_code_and_log_tail() {
    let mut fleet = Fleet::new(now_settings());
    crew(
        &fleet,
        "fail-1",
        "infra-repair",
        "echo some output; yes 'noise line' | head -c 200000 >&2; echo 'final error: disk full' >&2; exit 3",
    );
    wait(&fleet, "fail-1", &["failed"]);
    let outcomes = outcomes_of(&mut fleet, "fail-1");
    assert_eq!(outcomes.len(), 1);
    let outcome = &outcomes[0];
    assert_eq!(outcome["urgency"], "high");
    assert_eq!(outcome["payload"]["status"], "failed");
    assert_eq!(outcome["payload"]["exit_code"], 3);
    let body = outcome["body"].as_str().unwrap();
    assert!(body.contains("failed with exit code 3"), "{body}");
    assert!(body.contains("final error: disk full"), "{body}");
    assert!(body.chars().count() <= 2_000, "{}", body.len());
    assert!(body.contains("/jobs/fail-1/"), "{body}");
    // A retry is a new job and a new outcome.
    fleet.ok(
        "job.retry",
        json!({"id": "fail-1", "new_id": "fail-2"}),
        Some("infra"),
    );
    wait(&fleet, "fail-2", &["failed"]);
    let retried = outcomes_of(&mut fleet, "fail-2");
    assert_eq!(retried.len(), 1);
    assert_eq!(retried[0]["payload"]["retry_of"], "fail-1");
    assert_ne!(retried[0]["id"], outcome["id"]);
    assert_eq!(outcomes_of(&mut fleet, "fail-1").len(), 1);
}

#[test]
fn a_killed_supervisor_still_yields_exactly_one_outcome() {
    let mut fleet = Fleet::new(now_settings());
    // The payload outlives its supervisor: the job is orphaned, reported
    // once however many callers reconcile it at the same time.
    crew(&fleet, "long", "infra-repair", "sleep 60");
    let running = wait(&fleet, "long", &["running"]);
    kill(&running["supervisor"]["pid"], false);
    gone(&running["supervisor"]["pid"]);
    let racers: Vec<_> = (0..4)
        .map(|_| {
            fleet
                .command()
                .args(["job.status", "--json", "{\"id\":\"long\"}"])
                .stdout(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    fleet.tick();
    for mut racer in racers {
        assert!(racer.wait().unwrap().success());
    }
    let outcomes = outcomes_of(&mut fleet, "long");
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert_eq!(outcomes[0]["payload"]["status"], "orphaned");
    assert_eq!(outcomes[0]["urgency"], "high");
    assert!(
        outcomes[0]["body"]
            .as_str()
            .unwrap()
            .contains("supervisor disappeared; payload capacity preserved")
    );
    // The outcome names the ship's own way to free it.
    assert!(
        outcomes[0]["body"]
            .as_str()
            .unwrap()
            .contains(r#"job.cancel {"id":"long"}"#)
    );
    // Stopping the orphan settles it without a second outcome.
    let cancelled = fleet.ok("job.cancel", json!({"id": "long"}), Some("infra"));
    assert_eq!(cancelled["status"], "cancelled");
    fleet.tick();
    assert_eq!(outcomes_of(&mut fleet, "long").len(), 1);

    // Supervisor and payload both gone: the dispatcher alone settles it.
    crew(&fleet, "lost", "infra-repair", "sleep 60");
    let running = wait(&fleet, "lost", &["running"]);
    kill(&running["supervisor"]["pid"], false);
    gone(&running["supervisor"]["pid"]);
    kill(&running["payload"]["pid"], true);
    gone(&running["payload"]["pid"]);
    let report = fleet.tick();
    // An unrelated first-mate wake may also finish in this tick; any other
    // reconciled job would still be a failure.
    let reconciled: Vec<_> = report["reconciled"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| {
            !entry["job_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("wake-"))
        })
        .cloned()
        .collect();
    assert_eq!(
        reconciled,
        vec![json!({"job_id": "lost", "status": "failed"})],
        "{report}"
    );
    for _ in 0..3 {
        fleet.tick();
        fleet.ok("job.status", json!({"id": "lost"}), None);
    }
    let outcomes = outcomes_of(&mut fleet, "lost");
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert_eq!(outcomes[0]["payload"]["status"], "failed");
    assert_eq!(outcomes[0]["urgency"], "high");
    assert!(
        outcomes[0]["body"]
            .as_str()
            .unwrap()
            .contains("payload ended without a recorded result")
    );
    fleet.settle();
}

#[test]
fn urgency_follows_the_outcome_and_who_stopped_the_job() {
    let mut fleet = Fleet::new(now_settings());
    // Cancelled by the ship itself: low, and it waits for a real wake.
    crew(&fleet, "own-cancel", "infra-repair", "sleep 60");
    wait(&fleet, "own-cancel", &["running"]);
    fleet.ok("job.cancel", json!({"id": "own-cancel"}), Some("infra"));
    wait(&fleet, "own-cancel", &["cancelled"]);
    let own = outcomes_of(&mut fleet, "own-cancel");
    assert_eq!(own.len(), 1);
    assert_eq!(own[0]["urgency"], "low");
    assert_eq!(own[0]["payload"]["cancelled_by"], "infra");
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "nothing wake-worthy"
    );
    // Cancelled by the operator: the first mate should hear of it.
    crew(&fleet, "their-cancel", "infra-repair", "sleep 60");
    wait(&fleet, "their-cancel", &["running"]);
    fleet.ok("job.cancel", json!({"id": "their-cancel"}), None);
    wait(&fleet, "their-cancel", &["cancelled"]);
    let theirs = outcomes_of(&mut fleet, "their-cancel");
    assert_eq!(theirs[0]["urgency"], "normal");
    assert_eq!(theirs[0]["payload"]["cancelled_by"], "operator");
    // Timed out: high.
    fleet.ok(
        "job.start",
        json!({"id": "slow", "task_id": "infra-repair", "command": ["sleep", "60"],
            "heavy": false, "timeout_secs": 1}),
        Some("infra"),
    );
    wait(&fleet, "slow", &["timed_out"]);
    let slow = outcomes_of(&mut fleet, "slow");
    assert_eq!(slow.len(), 1);
    assert_eq!(slow[0]["urgency"], "high");
    assert_eq!(slow[0]["payload"]["reason"], "job deadline reached");
    let woke = Fleet::infra(&fleet.tick());
    assert_eq!(woke["status"], "running", "{woke}");
    fleet.settle();
    let prompt = fleet.prompts().pop().unwrap();
    for job in ["own-cancel", "their-cancel", "slow"] {
        assert!(prompt.contains(&format!("Crew job {job}")), "{prompt}");
    }
}

#[test]
fn jobs_that_end_in_the_capacity_queue_report_too() {
    let mut fleet = Fleet::new(now_settings());
    fleet.ok(
        "resource.configure",
        json!({"max_heavy_jobs": 1, "max_load_per_cpu": 1024,
            "min_available_memory_bytes": 0, "min_free_disk_bytes": 0, "heavy_estimate":{"memory_bytes":0,"disk_bytes":0}, "ramp_secs": 0}),
        None,
    );
    let heavy = |id: &str, extra: Value| {
        let mut args = json!({"id": id, "task_id": "infra-repair",
            "command": ["sleep", "60"], "heavy": true,
            "estimate": {"cpus": 1, "memory_bytes": 67108864}});
        for (key, value) in extra.as_object().unwrap() {
            args[key] = value.clone();
        }
        fleet.ok("job.start", args, Some("infra"))
    };
    heavy("holder", json!({}));
    wait(&fleet, "holder", &["running"]);
    assert_eq!(
        heavy("waiter", json!({"wait_for_capacity": true}))["status"],
        "queued"
    );
    assert_eq!(
        heavy(
            "impatient",
            json!({"wait_for_capacity": true, "max_wait_secs": 1})
        )["status"],
        "queued"
    );
    wait(&fleet, "impatient", &["failed"]);
    fleet.ok("job.cancel", json!({"id": "waiter"}), None);
    wait(&fleet, "waiter", &["cancelled"]);
    fleet.ok("job.cancel", json!({"id": "holder"}), Some("infra"));
    wait(&fleet, "holder", &["cancelled"]);
    let impatient = outcomes_of(&mut fleet, "impatient");
    assert_eq!(impatient.len(), 1);
    assert_eq!(impatient[0]["urgency"], "high");
    assert!(
        impatient[0]["payload"]["reason"]
            .as_str()
            .unwrap()
            .starts_with("no capacity within 1 s")
    );
    let waiter = outcomes_of(&mut fleet, "waiter");
    assert_eq!(waiter.len(), 1);
    assert_eq!(
        waiter[0]["payload"]["reason"],
        "cancelled while waiting for capacity"
    );
    assert_eq!(outcomes_of(&mut fleet, "holder").len(), 1);
}

const CREW_SCRIPT: &str = r#"import json, subprocess, sys
binary, home = sys.argv[1], sys.argv[2]
def call(command, args):
    out = subprocess.run([binary, "--home", home, command, "--json", json.dumps(args)], capture_output=True, text=True)
    if out.returncode:
        sys.exit("%s failed: %s" % (command, out.stderr))
    return json.loads(out.stdout)
call("crew.report", {"kind": "progress", "body": "halfway there"})
asked = call("crew.report", {"kind": "decision", "id": "which-plan", "body": "Plan A or plan B?"})
again = call("crew.report", {"kind": "decision", "id": "which-plan", "body": "Plan A or plan B?"})
assert again["id"] == asked["id"] and again["duplicate"], again
answer = call("crew.answer", {"id": asked["id"], "wait_secs": 60})
print("answer:", answer["reply"]["body"])
"#;

#[test]
fn a_crew_decision_reaches_the_first_mate_and_the_crew_reads_its_reply() {
    let mut fleet = Fleet::new(now_settings());
    let script = fleet.root.join("crew.py");
    fs::write(&script, CREW_SCRIPT).unwrap();
    let job = fleet.ok(
        "job.start",
        json!({"id": "asker", "task_id": "infra-repair", "heavy": false,
            "command": [support::python(), script, support::BIN, fleet.home]}),
        Some("infra"),
    );
    // The dispatcher wakes the first mate for the decision; the scripted
    // first mate answers open requests, so the crew gets its reply and ends.
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        fleet.tick();
        let status = fleet.ok("job.status", json!({"id": "asker"}), None);
        // A job passes through queued and starting before it runs.
        if !["running", "launching", "starting", "queued"]
            .contains(&status["status"].as_str().unwrap())
        {
            assert_eq!(status["status"], "succeeded", "{status}");
            break;
        }
        assert!(
            Instant::now() < until,
            "crew never got its answer: {status}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let log = fleet
        .home
        .join("jobs/asker")
        .join(job["incarnation"].as_str().unwrap())
        .join("stdout.log");
    assert_eq!(
        fs::read_to_string(log).unwrap().trim(),
        "answer: handled by infra"
    );
    let reports = from_crew(&mut fleet, "infra");
    let progress = reports
        .iter()
        .find(|m| m["payload"]["report"] == "progress")
        .unwrap();
    assert_eq!(progress["kind"], "notice");
    assert_eq!(progress["urgency"], "low");
    assert_eq!(progress["task_id"], "infra-repair");
    let decisions: Vec<_> = reports
        .iter()
        .filter(|m| m["payload"]["report"] == "decision")
        .collect();
    assert_eq!(decisions.len(), 1, "an id makes a report idempotent");
    let decision = decisions[0];
    assert_eq!(decision["kind"], "request");
    assert_eq!(decision["urgency"], "high");
    assert_eq!(decision["state"], "answered");
    assert!(
        decision["body"]
            .as_str()
            .unwrap()
            .ends_with("Plan A or plan B?")
    );
    // The answer is a recorded reply, readable later by the crew's ship.
    let answer = fleet.ok("crew.answer", json!({"id": decision["id"]}), Some("infra"));
    assert_eq!(answer["state"], "answered");
    assert_eq!(answer["reply"]["from"], "infra");
    assert_eq!(answer["reply"]["to"], "crew");
    // Replies to crew are not anyone's unread mail.
    fleet.settle();
    let status = dispatch(&mut fleet.store, "status", json!({})).unwrap();
    assert_eq!(status["unacknowledged_mail"], 0, "{status}");
    assert_eq!(status["open_requests"], 0);
    assert_eq!(outcomes_of(&mut fleet, "asker").len(), 1);
}

/// A crew that starts its own failing test job, follows it to the end, then
/// keeps working until `release` exists.
const WATCHING_CREW: &str = r#"import json, os, subprocess, sys, time
binary, home, release = sys.argv[1], sys.argv[2], sys.argv[3]
def call(command, args):
    out = subprocess.run([binary, "--home", home, command, "--json", json.dumps(args)], capture_output=True, text=True)
    if out.returncode:
        sys.exit("%s failed: %s" % (command, out.stderr))
    return json.loads(out.stdout)
call("job.start", {"id": "crew-test", "task_id": "infra-repair", "heavy": False, "command": ["false"]})
while call("job.status", {"id": "crew-test"})["status"] != "failed":
    time.sleep(0.1)
while not os.path.exists(release):
    time.sleep(0.1)
"#;

#[test]
fn a_job_its_crew_follows_waits_for_the_crew_instead_of_waking_the_first_mate() {
    let mut fleet = Fleet::new(now_settings());
    let script = fleet.root.join("crew.py");
    fs::write(&script, WATCHING_CREW).unwrap();
    let release = fleet.root.join("release");
    fleet.ok(
        "job.start",
        json!({"id": "worker", "task_id": "infra-repair", "heavy": false,
            "command": [support::python(), script, support::BIN, fleet.home, release]}),
        Some("infra"),
    );
    let until = Instant::now() + Duration::from_secs(30);
    while outcomes_of(&mut fleet, "crew-test").is_empty() {
        assert!(Instant::now() < until, "the crew's test job never ended");
        std::thread::sleep(Duration::from_millis(100));
    }
    // The failure is the crew's to handle while it runs.
    assert_eq!(outcomes_of(&mut fleet, "crew-test")[0]["urgency"], "low");
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "nothing wake-worthy"
    );
    // When the crew ends, the first mate wakes and sees both outcomes.
    fs::write(&release, "").unwrap();
    wait(&fleet, "worker", &["succeeded"]);
    let woke = Fleet::infra(&fleet.tick());
    assert_eq!(woke["status"], "running", "{woke}");
    fleet.settle();
    let prompt = fleet.prompts().pop().unwrap();
    assert!(prompt.contains("Crew job crew-test failed"), "{prompt}");
    assert!(prompt.contains("Crew job worker succeeded"), "{prompt}");
}

#[test]
fn only_the_runtime_speaks_as_crew() {
    let mut fleet = Fleet::new(now_settings());
    for command in ["ship.put", "ship.create"] {
        let error = dispatch(&mut fleet.store, command, json!({"name": "crew"}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("crew is reserved"), "{command}: {error}");
    }
    let forged = json!({"from": "crew", "to": "infra", "kind": "notice", "body": "all good"});
    assert!(
        fleet
            .refused("mail.send", forged.clone(), None)
            .contains("only the runtime speaks as crew")
    );
    assert!(
        fleet
            .refused("mail.send", forged.clone(), Some("infra"))
            .contains("cannot act as")
    );
    assert!(
        fleet
            .refused(
                "mail.deliver",
                json!({"id": "forged", "from": "crew", "to": "infra", "kind": "notice", "body": "x"}),
                None
            )
            .contains("crew reports stay in their own home")
    );
    assert!(
        fleet
            .refused(
                "mail.send",
                json!({"from": "infra", "to": "crew", "kind": "notice", "body": "hello"}),
                Some("infra")
            )
            .contains("answered only by replying")
    );
    assert!(
        fleet
            .refused("mail.inbox", json!({"address": "crew"}), None)
            .contains("crew has no mailbox")
    );
    // A reply to crew must answer a crew request.
    let request = fleet.send(
        json!({"id": "ask-infra", "from": "apps", "to": "infra", "kind": "request", "body": "?"}),
    );
    assert!(
        fleet
            .refused(
                "mail.send",
                json!({"from": "infra", "to": "crew", "kind": "reply", "correlation": request["id"], "body": "x"}),
                Some("infra")
            )
            .contains("only the recipient")
    );
    // crew.report needs an agent inside its own running job.
    let report = json!({"kind": "progress", "body": "forged"});
    assert!(
        fleet
            .refused("crew.report", report.clone(), None)
            .contains("is for crew")
    );
    assert!(
        fleet
            .refused("crew.report", report.clone(), Some("infra"))
            .contains("PRIVATEER_JOB is not set")
    );
    crew(&fleet, "busy", "infra-repair", "sleep 60");
    wait(&fleet, "busy", &["running"]);
    let as_job = |job: &str, agent: &str| {
        let output = fleet
            .command()
            .args(["crew.report", "--json", &report.to_string()])
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", agent)
            .env("PRIVATEER_JOB", job)
            .output()
            .unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let (ok, error) = as_job("busy", "apps");
    assert!(!ok && error.contains("is not crew of apps"), "{error}");
    crew(&fleet, "done", "infra-repair", "true");
    wait(&fleet, "done", &["succeeded"]);
    let (ok, error) = as_job("done", "infra");
    assert!(!ok && error.contains("is not running"), "{error}");
    let (ok, error) = as_job("busy", "infra");
    assert!(ok, "{error}");
    fleet.ok("job.cancel", json!({"id": "busy"}), Some("infra"));
    // Every message from crew was written by the runtime.
    assert!(
        from_crew(&mut fleet, "infra")
            .iter()
            .all(|m| m["body"] != "all good")
    );
}

#[test]
fn outcomes_go_only_to_ships_hosted_here() {
    let mut fleet = Fleet::new(now_settings());
    dispatch(
        &mut fleet.store,
        "task.create",
        json!({"id": "ghost-task", "charter_id": "fleet", "owner": "ghost", "title": "no such ship"}),
    )
    .unwrap();
    fleet.ok(
        "job.start",
        json!({"id": "ghostly", "task_id": "ghost-task", "command": ["true"], "heavy": false}),
        None,
    );
    // apps has a mailbox but no first mate: it still hears from its crew.
    fleet.ok(
        "job.start",
        json!({"id": "apps-build", "task_id": "apps-duty", "command": ["true"], "heavy": false}),
        Some("apps"),
    );
    wait(&fleet, "ghostly", &["succeeded"]);
    wait(&fleet, "apps-build", &["succeeded"]);
    let crew_mail: i64 = fleet
        .store
        .connection()
        .query_row("SELECT COUNT(*) FROM mail WHERE sender='crew'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(crew_mail, 1);
    let apps = from_crew(&mut fleet, "apps");
    assert_eq!(apps[0]["payload"]["job_id"], "apps-build");
}

#[test]
fn an_attended_ship_gets_crew_outcomes_through_its_hooks_without_a_headless_wake() {
    let fleet = Fleet::new(now_settings());
    fs::write(
        fleet.home.join("config.json"),
        json!({"resource_home": fleet.root.join("ledger"), "ship": "infra"}).to_string(),
    )
    .unwrap();
    let hook = |event: &str| {
        let mut child = fleet
            .command()
            .args(["hook", "claude", event])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(json!({"session_id": "captain-1"}).to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    assert!(hook("SessionStart").status.success());
    crew(
        &fleet,
        "aboard",
        "infra-repair",
        "echo attended result >&2; exit 1",
    );
    wait(&fleet, "aboard", &["failed"]);
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "captain's session is aboard"
    );
    // At turn end the session is kept working with the outcome as the reason.
    let stop = hook("Stop");
    assert_eq!(stop.status.code(), Some(2));
    let shown = String::from_utf8_lossy(&stop.stderr);
    assert!(shown.contains("from crew kind notice"), "{shown}");
    assert!(
        shown.contains("Crew job aboard failed with exit code 1"),
        "{shown}"
    );
    assert!(shown.contains("attended result"), "{shown}");
    // Shown once.
    assert_eq!(hook("Stop").status.code(), Some(0));
    assert_eq!(
        fleet.ok("wake.list", json!({"ship": "infra"}), None)["wakes"],
        json!([])
    );
    assert!(hook("SessionEnd").status.success());
}

#[test]
fn outcome_and_verdict_notices_name_the_route_and_why() {
    let mut fleet = Fleet::new(now_settings());
    fs::write(
        fleet.home.join("config.json"),
        json!({"resource_home": fleet.root.join("ledger"), "ship": "infra"}).to_string(),
    )
    .unwrap();
    let routes_file = fleet.root.join("routes.json");
    let routes = json!({
        "routes": {
            "work": {
                "manifest": fleet.root.join("manifest.json"),
                "options": {
                    "mate": fleet.root.join("mate.py"),
                    "mode": fleet.root.join("mode"),
                    "log": fleet.root.join("prompts.log"),
                    "model": "model-work",
                },
                "cost": "free",
                "tier": 1,
                "uses": ["review", "implement"],
            }
        },
        "crew": ["work"],
        "errand": ["work"],
    });
    fs::write(&routes_file, routes.to_string()).unwrap();
    let set_routes = fleet
        .command()
        .args(["routes", "set", routes_file.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        set_routes.status.success(),
        "routes set failed: {}",
        String::from_utf8_lossy(&set_routes.stderr)
    );

    // 1. Crew job via agent.start (no require)
    fleet.mode("crew");
    fleet.ok(
        "agent.start",
        json!({"id": "routed-crew", "task_id": "infra-repair"}),
        Some("infra"),
    );
    let job = wait(&fleet, "routed-crew", &["succeeded"]);
    assert_eq!(job["status"], "succeeded");
    let outcomes = outcomes_of(&mut fleet, "routed-crew");
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    let outcome_body = outcomes[0]["body"].as_str().unwrap();
    assert!(
        outcome_body.contains(
            "Route: first in allowance order on infra's crew list: work (no requirement)"
        ),
        "{outcome_body}"
    );

    // 2. Errand via errand.start with require
    fleet.mode("normal");
    let started = fleet.ok(
        "errand.start",
        json!({
            "focus": "task:infra-repair",
            "brief": "Review the cache rebuild.",
            "require": {"use": "review", "tier": 1}
        }),
        Some("infra"),
    );
    assert_eq!(
        started["route"]["why"],
        "cheapest on infra's errand list meeting review at tier <= 1: work"
    );
    let errand_id = started["errand"].as_str().unwrap();
    wait(&fleet, errand_id, &["succeeded"]);
    fleet.tick();

    let mail = from_crew(&mut fleet, "infra");
    let verdict_mail = mail
        .iter()
        .find(|m| {
            m["body"]
                .as_str()
                .unwrap_or("")
                .contains("Errand on task:infra-repair, verdict:")
        })
        .expect("verdict mail not found");
    let verdict_body = verdict_mail["body"].as_str().unwrap();
    assert!(
        verdict_body
            .contains("Route: cheapest on infra's errand list meeting review at tier <= 1: work"),
        "{verdict_body}"
    );
}

#[test]
fn a_first_mate_may_shorten_a_crews_deadline_within_the_charter_cap() {
    let fleet = Fleet::new(now_settings());
    fleet.mode("crew");

    let not_whole = fleet.refused(
        "agent.start",
        json!({"id": "fractional-crew", "task_id": "infra-repair", "timeout_secs": 1.5}),
        Some("infra"),
    );
    assert!(not_whole.contains("positive whole number"), "{not_whole}");

    let over = fleet.refused(
        "agent.start",
        json!({"id": "overlong-crew", "task_id": "infra-repair", "timeout_secs": 61}),
        Some("infra"),
    );
    assert!(over.contains("job_timeout_secs"), "{over}");
    assert!(over.contains("60"), "{over}");
    assert!(
        !fleet.ok("job.list", json!({}), None)["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|job| job["id"] == "overlong-crew")
    );

    let bounded = fleet.ok(
        "agent.start",
        json!({"id": "bounded-crew", "task_id": "infra-repair", "timeout_secs": 45}),
        Some("infra"),
    );
    assert_eq!(bounded["timeout_secs"], 45);
    assert_eq!(
        wait(&fleet, "bounded-crew", &["succeeded"])["timeout_secs"],
        45
    );

    let defaulted = fleet.ok(
        "agent.start",
        json!({"id": "default-crew", "task_id": "infra-duty"}),
        Some("infra"),
    );
    assert_eq!(defaulted["timeout_secs"], 60);
    assert_eq!(
        wait(&fleet, "default-crew", &["succeeded"])["timeout_secs"],
        60
    );
}

#[test]
fn a_claude_crew_command_with_multiline_quoted_body_uses_file_without_subshells_or_nested_quotes() {
    let mut fleet = Fleet::new(now_settings());
    crew(&fleet, "busy", "infra-repair", "sleep 60");
    wait(&fleet, "busy", &["running"]);

    let multiline_body = "Line 1: \"double quotes\" and 'single quotes'\nLine 2: `backticks` and $PATH\nLine 3: $(rm -rf /) dangerous subshell text\n";
    let report_data = json!({
        "kind": "progress",
        "body": multiline_body,
    });
    let report_path = fleet.root.join("report.json");
    fs::write(&report_path, serde_json::to_vec(&report_data).unwrap()).unwrap();

    // A Claude-harness crew runs commands via its Bash tool.
    // With --file PATH, the command needs no $(...) subshells and no nested quoting tricks.
    let shell_command = format!(
        "{} --home {} crew.report --file {}",
        support::BIN,
        fleet.home.display(),
        report_path.display(),
    );
    assert!(!shell_command.contains("$("));
    assert!(!shell_command.contains("`"));
    assert!(!shell_command.contains("'\""));

    let output = support::command("sh")
        .args(["-c", &shell_command])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "infra")
        .env("PRIVATEER_JOB", "busy")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let body = result["body"].as_str().unwrap();
    let prefix = "Crew job busy progress:\n";
    assert!(body.starts_with(prefix));
    assert_eq!(&body[prefix.len()..], multiline_body);

    // Also verify that stdin (--file -) round-trips such a body exactly without $(...) or nested quotes.
    let stdin_command = format!(
        "{} --home {} crew.report --file -",
        support::BIN,
        fleet.home.display(),
    );
    assert!(!stdin_command.contains("$("));
    assert!(!stdin_command.contains("`"));
    let mut child = support::command("sh")
        .args(["-c", &stdin_command])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "infra")
        .env("PRIVATEER_JOB", "busy")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&report_data).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let body = result["body"].as_str().unwrap();
    assert_eq!(&body[prefix.len()..], multiline_body);

    fleet.ok("job.cancel", json!({"id": "busy"}), Some("infra"));
}
