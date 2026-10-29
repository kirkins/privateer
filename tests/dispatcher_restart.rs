mod support;

use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::Output,
    thread,
    time::{Duration, Instant},
};
use support::{Fleet, now_settings};

// No inference or mailbox acknowledgement: successful wake coverage itself
// must suppress replay. Each real Python payload waits on its own gate.
const GATED_MATE: &str = r#"import json, os, pathlib, sys, time
prompt, log, resume = sys.argv[3], pathlib.Path(sys.argv[5]), sys.argv[6]
launches = log.with_name('launches.jsonl')
n = len(launches.read_text().splitlines()) + 1 if launches.exists() else 1
with log.open('a') as f:
    f.write(json.dumps(prompt) + '\n')
with launches.open('a') as f:
    f.write(json.dumps({'pid': os.getpid(), 'resume': resume, 'prompt': prompt}) + '\n')
print(json.dumps({'type': 'thread.started', 'thread_id': resume or 'session-' + str(os.getpid())}), flush=True)
while not log.with_name('gate-' + str(n)).exists():
    time.sleep(0.05)
print(json.dumps({'type': 'turn.completed', 'usage': {'output_tokens': 1}}), flush=True)
"#;

fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(Instant::now() < deadline, "restart fixture timed out");
        thread::sleep(Duration::from_millis(50));
    }
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

struct Dispatcher<'a>(&'a Fleet);
impl<'a> Dispatcher<'a> {
    fn new(fleet: &'a Fleet) -> Self {
        fs::write(
            fleet.home.join("config.json"),
            json!({
                "ship":"infra", "resource_home":fleet.root.join("ledger")
            })
            .to_string(),
        )
        .unwrap();
        Self(fleet)
    }
    fn start(&self) {
        success(
            self.0
                .command()
                .args(["service", "start"])
                .env("PRIVATEER_SERVICE", "detached")
                .output()
                .unwrap(),
        );
        until(|| privateer::service::running(&self.0.home).unwrap());
    }
    fn stop(&self) {
        let stopped = success(self.0.command().args(["service", "stop"]).output().unwrap());
        assert!(stopped.contains("Stopped the dispatcher"), "{stopped}");
        assert!(!privateer::service::running(&self.0.home).unwrap());
    }
}
impl Drop for Dispatcher<'_> {
    fn drop(&mut self) {
        let _ = self.0.command().args(["service", "stop"]).output();
        // Also clean up real supervised payloads if an assertion failed while
        // a gate was shut. Dispatcher death alone is never cleanup evidence.
        let output = self.0.run("job.list", json!({}), None);
        if let Ok(jobs) = serde_json::from_slice::<Value>(&output.stdout) {
            for job in jobs["jobs"].as_array().into_iter().flatten() {
                if matches!(
                    job["status"].as_str(),
                    Some("running" | "starting" | "launching" | "queued" | "orphaned")
                ) {
                    let _ = self.0.run("job.cancel", json!({"id":job["id"]}), None);
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let output = self.0.run("resource.status", json!({}), None);
            if serde_json::from_slice::<Value>(&output.stdout)
                .ok()
                .is_some_and(|v| v["reservations"].as_array().is_some_and(Vec::is_empty))
            {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        eprintln!("restart fixture cleanup could not prove all payload reservations released");
    }
}

fn launches(fleet: &Fleet) -> Vec<Value> {
    whole_lines(&fs::read_to_string(fleet.root.join("launches.jsonl")).unwrap_or_default())
}

/// Every record the log has finished writing: up to and including its last
/// newline. The fake launcher can be caught mid-line, so a torn tail without
/// its newline is left for the wait loop to read again; every whole line is
/// kept and still parsed strictly.
fn whole_lines(log: &str) -> Vec<Value> {
    log[..log.rfind('\n').map_or(0, |i| i + 1)]
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn a_losing_detached_child_cannot_replace_the_running_dispatchers_identity() {
    let fleet = gated_fleet();
    let dispatcher = Dispatcher::new(&fleet);
    dispatcher.start();
    let path = fleet.home.join("serve.pid");
    let identity = fleet.home.join("serve.pid.identity");
    let before = fs::read(&path).unwrap();
    let identity_before = fs::read(&identity).unwrap();
    let record: Value = serde_json::from_slice(&identity_before).unwrap();
    assert!(record["started"].is_string(), "{record}");
    let loser = fleet
        .command()
        .arg("serve")
        .env("PRIVATEER_DETACHED_PID", &path)
        .output()
        .unwrap();
    assert!(!loser.status.success(), "{loser:?}");
    assert!(
        String::from_utf8_lossy(&loser.stderr).contains("another serve is already running"),
        "{loser:?}"
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::read(&identity).unwrap(), identity_before);
    dispatcher.stop();
    // A foreground maintenance pass never acquires detached custody.
    success(
        fleet
            .command()
            .args(["serve", "--json", "{\"once\":true}"])
            .output()
            .unwrap(),
    );
    assert!(!path.exists());
    assert!(!identity.exists());
}

#[test]
fn a_torn_tail_keeps_the_whole_lines_before_it() {
    let a = json!({"id": "a"}).to_string();
    let b = json!({"id": "b"}).to_string();
    let log = a + "\n" + &b + "\n{\"id\": \"c\"";
    let entries = whole_lines(&log);
    assert_eq!(entries, [json!({"id": "a"}), json!({"id": "b"})]);
}

#[test]
#[should_panic(expected = "called `Result::unwrap()` on an `Err` value")]
fn a_malformed_whole_line_still_fails_the_read() {
    whole_lines("{\"id\": \"a\"}\nnot a record\n");
}
fn wakes(fleet: &Fleet) -> Vec<Value> {
    fleet.ok("wake.list", json!({"ship":"infra"}), None)["wakes"]
        .as_array()
        .unwrap()
        .clone()
}
fn sessions(fleet: &Fleet) -> i64 {
    fleet
        .store
        .connection()
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap()
}
fn send(fleet: &Fleet, id: &str) -> Value {
    fleet.ok(
        "mail.send",
        json!({"id":id,"from":"apps","to":"infra","kind":"notice",
        "task_id":"infra-repair","body":id}),
        None,
    )
}
fn job(fleet: &Fleet, wake: &Value) -> Value {
    fleet.ok("job.status", json!({"id":wake["job_id"]}), None)
}
fn gated_fleet() -> Fleet {
    let fleet = Fleet::new(now_settings());
    fs::write(fleet.root.join("mate.py"), GATED_MATE).unwrap();
    fleet
}

#[test]
fn running_wake_survives_detached_stop_restart_and_outage_mail_is_covered_once() {
    let fleet = gated_fleet();
    let dispatcher = Dispatcher::new(&fleet);
    fs::write(fleet.root.join("gate-1"), "").unwrap();
    send(&fleet, "before");
    dispatcher.start();
    until(|| {
        wakes(&fleet)
            .first()
            .is_some_and(|w| w["status"] == "succeeded")
    });
    let seed = wakes(&fleet)[0].clone();
    let session = seed["session_id"].as_str().unwrap().to_owned();
    assert_eq!(sessions(&fleet), 1);
    // Failed contenders must not rotate the active dispatcher's generation.
    for args in [json!({"once":true}), json!({})] {
        let contender = fleet
            .command()
            .args(["serve", "--json", &args.to_string()])
            .output()
            .unwrap();
        assert!(!contender.status.success());
        assert!(String::from_utf8_lossy(&contender.stderr).contains("already running"));
        assert_eq!(sessions(&fleet), 1);
    }
    let first_mail = send(&fleet, "held");
    until(|| launches(&fleet).len() == 2);
    let held = wakes(&fleet)[0].clone();
    assert_eq!(held["mode"], "resume");
    until(|| job(&fleet, &held)["status"] == "running");
    let original = job(&fleet, &held);
    assert_eq!(original["status"], "running");
    let payload = original["payload"].clone();
    let supervisor = original["supervisor"].clone();
    assert_eq!(payload["pid"], launches(&fleet)[1]["pid"]);
    assert!(!payload["pid"].is_null());
    assert!(!supervisor["pid"].is_null());
    assert_eq!(launches(&fleet)[1]["resume"], session);
    assert_eq!(held["through_seq"], first_mail["seq"]);

    dispatcher.stop(); // Real service stop: SIGTERM only the recorded dispatcher.
    let outage_mail = send(&fleet, "during-outage");
    let absent = job(&fleet, &held);
    assert_eq!(absent["status"], "running");
    assert_eq!(absent["payload"], payload);
    assert_eq!(absent["supervisor"], supervisor);
    dispatcher.start();
    until(|| sessions(&fleet) == 0);
    thread::sleep(Duration::from_millis(1200)); // At least one new dispatcher pass.
    assert_eq!(launches(&fleet).len(), 2);
    assert_eq!(wakes(&fleet)[0]["id"], held["id"]);
    let adopted = job(&fleet, &held);
    assert_eq!(adopted["incarnation"], original["incarnation"]);
    assert_eq!(adopted["payload"], payload);
    assert_eq!(adopted["supervisor"], supervisor);
    assert_eq!(adopted["status"], "running");

    fs::write(fleet.root.join("gate-2"), "").unwrap();
    until(|| launches(&fleet).len() == 3);
    let after = wakes(&fleet);
    assert_eq!(after.len(), 3);
    assert_eq!(after[1]["id"], held["id"]);
    assert_eq!(after[1]["status"], "succeeded");
    assert!(after[1]["error"].is_null());
    assert_eq!(after[0]["through_seq"], outage_mail["seq"]);
    assert_eq!(after[0]["mode"], "fresh");
    assert_eq!(
        sessions(&fleet),
        0,
        "adopted completion restored old warmth"
    );
    let seen = launches(&fleet);
    assert_eq!(seen[2]["resume"], "");
    let prompts: Vec<_> = seen.iter().map(|l| l["prompt"].as_str().unwrap()).collect();
    assert_eq!(
        prompts
            .iter()
            .filter(|p| p.contains("id during-outage "))
            .count(),
        1
    );
    assert!(!prompts[2].contains("id held "));
    assert!(!prompts[2].contains("id before "));

    fs::write(fleet.root.join("gate-3"), "").unwrap();
    until(|| wakes(&fleet)[0]["status"] == "succeeded");
    assert_eq!(sessions(&fleet), 1);
    dispatcher.stop();
    // Store opens and one-shot maintenance passes must preserve current warmth.
    let _reopened = privateer::store::Store::open(&fleet.home).unwrap();
    for _ in 0..3 {
        assert_eq!(
            Fleet::infra(&fleet.tick())["skipped"],
            "nothing wake-worthy"
        );
    }
    assert_eq!(launches(&fleet).len(), 3);
    assert_eq!(sessions(&fleet), 1);
    assert_eq!(
        fleet.ok("resource.status", json!({}), None)["reservations"],
        json!([])
    );
}

#[test]
fn a_lost_supervisor_with_a_live_payload_never_allows_a_duplicate_wake() {
    let fleet = gated_fleet();
    let dispatcher = Dispatcher::new(&fleet);
    send(&fleet, "held");
    dispatcher.start();
    until(|| launches(&fleet).len() == 1);
    let wake = wakes(&fleet)[0].clone();
    until(|| job(&fleet, &wake)["status"] == "running");
    let before = job(&fleet, &wake);
    dispatcher.stop();
    let pid = before["supervisor"]["pid"].as_i64().unwrap() as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    // Job reads are read-only now: only the owning home's dispatcher settles a
    // lost supervisor, so a one-shot maintenance pass observes the orphan.
    until(|| {
        fleet.tick();
        job(&fleet, &wake)["status"] == "orphaned"
    });
    send(&fleet, "during-outage");
    dispatcher.start();
    thread::sleep(Duration::from_millis(1200));
    let orphan = job(&fleet, &wake);
    assert_eq!(orphan["payload"], before["payload"]);
    assert_eq!(orphan["status"], "orphaned");
    assert_eq!(
        unsafe { libc::kill(launches(&fleet)[0]["pid"].as_i64().unwrap() as i32, 0) },
        0
    );
    assert_eq!(wakes(&fleet).len(), 1);
    assert_eq!(wakes(&fleet)[0]["status"], "running");
    assert_eq!(launches(&fleet).len(), 1);
    dispatcher.stop();
    // Explicit cancellation, not restarting, owns termination/retry policy.
    fleet.ok("job.cancel", json!({"id":wake["job_id"]}), None);
    assert_eq!(
        fleet.ok("resource.status", json!({}), None)["reservations"],
        json!([])
    );
}

#[test]
fn systemd_dispatcher_preserves_wakes_and_repairs_old_policy_before_stop() {
    let fleet = Fleet::new(now_settings());
    fs::write(
        fleet.home.join("config.json"),
        json!({"ship":"infra", "resource_home":fleet.root.join("ledger")}).to_string(),
    )
    .unwrap();
    let systemctl = fleet.root.join("systemctl");
    let log = fleet.root.join("systemctl.jsonl");
    fs::write(&systemctl, format!(r#"#!{python}
import json, pathlib, sys
units = pathlib.Path({config:?}).joinpath('systemd/user')
with pathlib.Path({log:?}).open('a') as f:
    f.write(json.dumps({{'args':sys.argv[1:], 'units':[p.read_text() for p in units.glob('*.service')]}}) + '\n')
if units.joinpath('fail-reload').exists() and 'daemon-reload' in sys.argv:
    sys.exit(1)
"#, python=support::python().display(), config=fleet.root.join("config").to_str().unwrap(), log=log.to_str().unwrap())).unwrap();
    fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();
    let run = |action: &str| {
        fleet
            .command()
            .args(["service", action])
            .env("PRIVATEER_SERVICE", "systemd")
            .env("PRIVATEER_SYSTEMCTL", &systemctl)
            .output()
            .unwrap()
    };
    success(run("start"));
    let unit_path = fs::read_dir(fleet.root.join("config/systemd/user"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let generated = fs::read_to_string(&unit_path).unwrap();
    assert!(generated.contains("\nKillMode=process\n"), "{generated}");
    // Simulate a pre-fix unit. Its pinned executable/env must not be replaced.
    let legacy = generated
        .replace("KillMode=process\n", "KillMode=control-group\n")
        .replace(
            "Restart=on-failure",
            "Environment=KEEP_THIS=yes\nRestart=on-failure",
        );
    fs::write(&unit_path, legacy).unwrap();
    success(run("stop"));
    let calls: Vec<Value> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let stop = calls
        .iter()
        .position(|c| c["args"][1] == "disable")
        .unwrap();
    assert_eq!(calls[stop - 1]["args"], json!(["--user", "daemon-reload"]));
    let loaded = calls[stop - 1]["units"][0].as_str().unwrap();
    assert!(loaded.contains("\nKillMode=process\n"));
    assert!(!loaded.contains("KillMode=control-group"));
    assert!(loaded.contains("Environment=KEEP_THIS=yes"));
    assert!(!unit_path.exists());

    // If reloading the policy fails, never issue the destructive stop.
    fs::write(&unit_path, generated.replace("KillMode=process\n", "")).unwrap();
    fs::write(unit_path.parent().unwrap().join("fail-reload"), "").unwrap();
    let refused = run("stop");
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("not stopping dispatcher"));
    let calls: Vec<Value> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        calls.last().unwrap()["args"],
        json!(["--user", "daemon-reload"])
    );
    assert_eq!(
        calls.iter().filter(|c| c["args"][1] == "disable").count(),
        1
    );
    assert!(unit_path.exists());
}

#[test]
fn legacy_wake_finishing_after_service_start_cannot_rebind_a_session() {
    let fleet = gated_fleet();
    let dispatcher = Dispatcher::new(&fleet);
    send(&fleet, "legacy");
    fleet.tick(); // No resident dispatcher generation yet, like a legacy row.
    until(|| launches(&fleet).len() == 1);
    fleet
        .store
        .connection()
        .execute("UPDATE wakes SET dispatcher_generation=NULL", [])
        .unwrap();
    dispatcher.start();
    fs::write(fleet.root.join("gate-1"), "").unwrap();
    until(|| wakes(&fleet)[0]["status"] == "succeeded");
    assert_eq!(sessions(&fleet), 0);
    dispatcher.stop();
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "nothing wake-worthy"
    );
}
