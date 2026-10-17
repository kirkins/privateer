mod support;

use privateer::store::Store;
use rusqlite::params;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_privateer");
struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    repo: PathBuf,
    ledger: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let repo = root.join("repo");
        fs::create_dir(&repo).unwrap();
        let ledger = root.join("ledger");
        Self {
            _temp: temp,
            root,
            repo,
            ledger,
        }
    }
    fn home(&self, name: &str, min_free: u64) -> PathBuf {
        let home = self.root.join(name);
        let mut store = Store::open(&home).unwrap();
        fs::write(
            home.join("config.json"),
            serde_json::to_vec(&json!({"resource_home":self.ledger})).unwrap(),
        )
        .unwrap();
        // These cases exercise supervision with one deterministic heavy slot;
        // live load, memory and disk admission is tested separately. These
        // tiny payloads must not inherit the production heavy-job estimates.
        privateer::dispatch(
            &mut store,
            "resource.configure",
            json!({"max_heavy_jobs":1,"max_load_per_cpu":1024,"min_available_memory_bytes":0,"ramp_secs":0,
                "heavy_estimate":{"memory_bytes":0,"disk_bytes":0}}),
        )
        .unwrap();
        let charter = json!({"id":"mission","objective":"isolated supervisor fixture","repo":self.repo,
            "active":true,"actions":["implement","test","recovery","cleanup"],"min_free_bytes":min_free,
            "job_timeout_secs":30,"max_heavy_jobs":1,"production_requires_approval":true});
        store
            .connection()
            .execute(
                "INSERT INTO charters(id,data) VALUES(?1,?2)",
                params!["mission", charter.to_string()],
            )
            .unwrap();
        store.connection().execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('task','mission','peer','fixture','open',0)", []).unwrap();
        home
    }
    /// One dispatcher pass, which alone settles a job whose supervisor is
    /// gone. It touches the fleet registry and Herdr, so user state is
    /// pointed into the fixture.
    fn serve(&self, home: &Path) {
        let output = support::isolated(&self.root)
            .arg("--home")
            .arg(home)
            .args(["serve", "--json", r#"{"once":true}"#])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "serve: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    /// Wait for a status only a dispatcher pass records.
    fn settle(&self, home: &Path, id: &str, accepted: &[&str]) -> Value {
        let until = Instant::now() + Duration::from_secs(12);
        loop {
            self.serve(home);
            let job = ok(home, "job.status", json!({"id":id}));
            if accepted.contains(&job["status"].as_str().unwrap()) {
                return job;
            }
            assert!(Instant::now() < until, "job did not settle: {job}");
            thread::sleep(Duration::from_millis(50));
        }
    }
}
fn run(home: &Path, command: &str, args: Value) -> std::process::Output {
    support::command(BIN)
        .arg("--home")
        .arg(home)
        .arg(command)
        .arg("--json")
        .arg(args.to_string())
        .output()
        .unwrap()
}
fn ok(home: &Path, command: &str, args: Value) -> Value {
    let output = run(home, command, args);
    assert!(
        output.status.success(),
        "{command}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
fn wait(home: &Path, id: &str, accepted: &[&str]) -> Value {
    let until = Instant::now() + Duration::from_secs(12);
    loop {
        let job = ok(home, "job.status", json!({"id":id}));
        if accepted.contains(&job["status"].as_str().unwrap()) {
            return job;
        }
        assert!(Instant::now() < until, "job did not settle: {job}");
        thread::sleep(Duration::from_millis(50));
    }
}
fn start(home: &Path, id: &str, command: &[&str]) -> Value {
    ok(
        home,
        "job.start",
        json!({"task_id":"task","id":id,"command":command,"heavy":true}),
    )
}
/// A heavy job that leaves its leftover descendants running.
fn start_keeping(home: &Path, id: &str, command: &[&str]) -> Value {
    ok(
        home,
        "job.start",
        json!({"task_id":"task","id":id,"command":command,"heavy":true,"cleanup_descendants":false}),
    )
}
fn active_pid(pid: i32) -> bool {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
        .unwrap();
    let stat = String::from_utf8(output.stdout).unwrap();
    !stat.trim().is_empty() && !stat.trim().starts_with('Z')
}

#[test]
fn detached_job_survives_cli_exit_and_idempotency_does_not_duplicate_work() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    let cmd = [
        "/bin/sh",
        "-c",
        "sleep 0.2; printf result >> result.txt; printf diagnostic >&2",
    ];
    let first = start(&home, "durable", &cmd);
    let repeated = start(&home, "durable", &cmd);
    assert_eq!(first["incarnation"], repeated["incarnation"]);
    let completed = wait(&home, "durable", &["succeeded"]);
    assert_eq!(completed["exit_code"], 0);
    assert_eq!(
        fs::read_to_string(fixture.repo.join("result.txt")).unwrap(),
        "result"
    );
    let logs = home
        .join("jobs/durable")
        .join(completed["incarnation"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(logs.join("stderr.log")).unwrap(),
        "diagnostic"
    );
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
    // The ended job is remembered, so the load it leaves in the average does
    // not hold back the next one.
    let deadline = Instant::now() + Duration::from_secs(5);
    while ok(&home, "resource.status", json!({}))["recently_ended"] != 1 {
        assert!(Instant::now() < deadline, "the ended job was not recorded");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !run(
            &home,
            "job.start",
            json!({"task_id":"task","id":"durable","command":["/bin/echo","different"]})
        )
        .status
        .success()
    );
}

#[test]
fn shared_admission_preserves_orphan_payload_until_explicit_cancellation() {
    let fixture = Fixture::new();
    let a = fixture.home("a", 0);
    let b = fixture.home("b", 0);
    start(&a, "hold", &["/bin/sleep", "30"]);
    let running = wait(&a, "hold", &["running"]);
    let denied = run(
        &b,
        "job.start",
        json!({"task_id":"task","id":"blocked","command":["/bin/echo","not-yet"]}),
    );
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("capacity exhausted"));
    let supervisor = running["supervisor"]["pid"].as_u64().unwrap() as i32;
    assert_eq!(unsafe { libc::kill(supervisor, libc::SIGKILL) }, 0);
    let until = Instant::now() + Duration::from_secs(3);
    while active_pid(supervisor) && Instant::now() < until {
        thread::sleep(Duration::from_millis(20));
    }
    fixture.settle(&a, "hold", &["orphaned"]);
    let resource = ok(&b, "resource.status", json!({}));
    assert_eq!(resource["heavy_reserved"], 1);
    assert_eq!(resource["reservations"][0]["payload_group_alive"], true);
    assert!(
        !run(
            &b,
            "job.start",
            json!({"task_id":"task","command":["/bin/echo","still-no"]})
        )
        .status
        .success()
    );
    // Revocation must stop fresh work without preventing safe operator
    // cleanup of already-owned orphan processes.
    let store = Store::open(&a).unwrap();
    let mut charter = store.charter("mission").unwrap();
    charter["active"] = json!(false);
    store
        .connection()
        .execute(
            "UPDATE charters SET data=?1 WHERE id='mission'",
            [charter.to_string()],
        )
        .unwrap();
    let stopped = ok(&a, "job.cancel", json!({"id":"hold"}));
    assert_eq!(stopped["status"], "cancelled");
    assert_eq!(ok(&b, "resource.status", json!({}))["heavy_reserved"], 0);
    start(&b, "now", &["/bin/echo", "capacity-recovered"]);
    wait(&b, "now", &["succeeded"]);
}

#[test]
fn cancellation_reaches_the_payload_process_group() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    start(
        &home,
        "tree",
        &["/bin/sh", "-c", "sleep 30 & echo $! > descendant.pid; wait"],
    );
    wait(&home, "tree", &["running"]);
    let until = Instant::now() + Duration::from_secs(3);
    while !fixture.repo.join("descendant.pid").exists() && Instant::now() < until {
        thread::sleep(Duration::from_millis(20));
    }
    let descendant: i32 = fs::read_to_string(fixture.repo.join("descendant.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    ok(&home, "job.cancel", json!({"id":"tree"}));
    wait(&home, "tree", &["cancelled"]);
    assert!(!active_pid(descendant));
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
}

#[test]
fn deadline_is_durable_and_retry_is_explicit_with_a_fresh_incarnation() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    let first = ok(
        &home,
        "job.start",
        json!({"task_id":"task","id":"deadline","timeout_secs":1,"cleanup_descendants":true,
        "command":["/bin/sh","-c","if test ! -f attempted; then touch attempted; exec sleep 20; fi; echo recovered"]}),
    );
    assert!(
        !run(
            &home,
            "job.retry",
            json!({"id":"deadline","new_id":"too-early"})
        )
        .status
        .success()
    );
    let expired = wait(&home, "deadline", &["timed_out"]);
    assert_eq!(expired["reason"], "job deadline reached");
    assert_eq!(
        ok(&home, "job.list", json!({}))["jobs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let retry = ok(
        &home,
        "job.retry",
        json!({"id":"deadline","new_id":"retry"}),
    );
    assert_ne!(first["incarnation"], retry["incarnation"]);
    assert_eq!(retry["retry_of"], "deadline");
    assert_eq!(retry["cleanup_descendants"], true);
    wait(&home, "retry", &["succeeded"]);
}

#[test]
fn disk_and_repository_limits_refuse_before_any_process_starts() {
    let fixture = Fixture::new();
    let low = fixture.home("low", u64::MAX);
    let normal = fixture.home("normal", 0);
    let disk = run(
        &low,
        "job.start",
        json!({"task_id":"task","command":["/bin/echo","no"]}),
    );
    assert!(!disk.status.success());
    assert!(String::from_utf8_lossy(&disk.stderr).contains("disk admission refused"));
    let outside = run(
        &normal,
        "job.start",
        json!({"task_id":"task","cwd":fixture.root,"command":["/bin/echo","no"]}),
    );
    assert!(!outside.status.success());
    assert!(String::from_utf8_lossy(&outside.stderr).contains("inside charter"));
    assert!(
        !run(
            &normal,
            "job.start",
            json!({"task_id":"task","production":true,"command":["/bin/echo","no"]})
        )
        .status
        .success()
    );
    assert_eq!(ok(&normal, "job.list", json!({}))["jobs"], json!([]));
    assert_eq!(
        ok(&normal, "resource.status", json!({}))["heavy_reserved"],
        0
    );
}

#[test]
fn output_is_drained_and_saved_logs_keep_their_head_and_tail() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    // A harness announces itself first and reports usage last; 3 MiB of
    // streaming in between must not cost either end.
    start(
        &home,
        "noisy",
        &[
            "/bin/sh",
            "-c",
            "echo '{\"type\":\"session\",\"id\":\"first\"}'; \
             head -c 3142656 /dev/zero | tr '\\0' x | fold -w 1023; echo; \
             echo '{\"type\":\"agent_end\",\"usage\":{\"input\":7}}'; \
             dd if=/dev/zero bs=1048576 count=3 >&2 2>/dev/null",
        ],
    );
    let done = wait(&home, "noisy", &["succeeded"]);
    let logs = home
        .join("jobs/noisy")
        .join(done["incarnation"].as_str().unwrap());
    let stdout = fs::read_to_string(logs.join("stdout.log")).unwrap();
    assert!(stdout.len() <= 1024 * 1024, "{}", stdout.len());
    assert!(stdout.starts_with("{\"type\":\"session\",\"id\":\"first\"}\n"));
    assert!(stdout.ends_with("{\"type\":\"agent_end\",\"usage\":{\"input\":7}}\n"));
    let marker: Value = stdout
        .lines()
        .find_map(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .filter(|v| v["type"] == "privateer.log.truncated")
        })
        .expect("truncation marker");
    let dropped = marker["dropped_bytes"].as_u64().unwrap();
    assert!(dropped > 1024 * 1024, "{dropped}");
    assert_eq!(done["stdout_dropped_bytes"], dropped);
    assert!(fs::metadata(logs.join("stderr.log")).unwrap().len() <= 1024 * 1024);
    assert!(done["stderr_dropped_bytes"].as_u64().unwrap() > 0);
    // Tail segments are folded into the log, not left beside it.
    let mut names: Vec<_> = fs::read_dir(&logs)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["stderr.log", "stdout.log"]);
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
}

#[test]
fn a_short_log_is_kept_whole_and_reports_nothing_dropped() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    start(&home, "quiet", &["/bin/sh", "-c", "echo hello"]);
    let done = wait(&home, "quiet", &["succeeded"]);
    let logs = home
        .join("jobs/quiet")
        .join(done["incarnation"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(logs.join("stdout.log")).unwrap(),
        "hello\n"
    );
    assert!(done.get("stdout_dropped_bytes").is_none());
}

#[test]
fn concurrent_same_id_has_one_incarnation_and_one_payload() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    let args = json!({"task_id":"task","id":"once","command":["/bin/sh","-c","sleep 0.3; printf x >> once.txt"]});
    let mut handles = vec![];
    for _ in 0..4 {
        let home = home.clone();
        let args = args.clone();
        handles.push(thread::spawn(move || ok(&home, "job.start", args)));
    }
    let results: Vec<Value> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert!(
        results
            .iter()
            .all(|r| r["incarnation"] == results[0]["incarnation"])
    );
    wait(&home, "once", &["succeeded"]);
    assert_eq!(
        fs::read_to_string(fixture.repo.join("once.txt")).unwrap(),
        "x"
    );
    assert_eq!(
        ok(&home, "job.list", json!({}))["jobs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
}

#[test]
fn timeout_escalates_for_a_recorded_descendant_that_ignores_term() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    ok(
        &home,
        "job.start",
        json!({"task_id":"task","id":"stubborn","timeout_secs":1,
        "command":["/bin/sh","-c","sh -c 'trap \"\" TERM; echo $$ > stubborn.pid; while :; do sleep 1; done' & wait"]}),
    );
    let completed = wait(&home, "stubborn", &["timed_out"]);
    assert_eq!(completed["reason"], "job deadline reached");
    let descendant: i32 = fs::read_to_string(fixture.repo.join("stubborn.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(!active_pid(descendant));
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
}

#[test]
fn provider_binding_survives_retry_and_changes_cancel_running_work() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    let store = Store::open(&home).unwrap();
    let mut charter = store.charter("mission").unwrap();
    charter["provider_plugin"] = json!({"manifest":"generic-old.json","config":"private.json"});
    store
        .connection()
        .execute(
            "UPDATE charters SET data=?1 WHERE id='mission'",
            [charter.to_string()],
        )
        .unwrap();
    let old_pin = charter["provider_plugin"].clone();
    ok(
        &home,
        "job.start",
        json!({"task_id":"task","id":"pinned-fail","command":["/bin/sh","-c","exit 7"],"plugin_binding":old_pin}),
    );
    wait(&home, "pinned-fail", &["failed"]);
    ok(
        &home,
        "job.start",
        json!({"task_id":"task","id":"pinned-running","command":["/bin/sleep","30"],"plugin_binding":old_pin}),
    );
    wait(&home, "pinned-running", &["running"]);
    charter["provider_plugin"] = json!({"manifest":"generic-new.json","config":"private.json"});
    store
        .connection()
        .execute(
            "UPDATE charters SET data=?1 WHERE id='mission'",
            [charter.to_string()],
        )
        .unwrap();
    let stopped = wait(&home, "pinned-running", &["cancelled"]);
    assert!(
        stopped["reason"]
            .as_str()
            .unwrap()
            .contains("binding changed")
    );
    let retry = run(
        &home,
        "job.retry",
        json!({"id":"pinned-fail","new_id":"refused"}),
    );
    assert!(!retry.status.success());
    assert!(String::from_utf8_lossy(&retry.stderr).contains("binding changed"));
    assert_eq!(
        ok(&home, "job.list", json!({}))["jobs"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
}

#[test]
fn ordinary_leader_exit_preserves_and_can_cancel_recorded_descendants() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    start_keeping(
        &home,
        "ordinary-orphan",
        &["/bin/sh", "-c", "sleep 30 & echo $! > orphan.pid; exit 0"],
    );
    let orphan = wait(&home, "ordinary-orphan", &["orphaned"]);
    assert_eq!(orphan["exit_code"], 0);
    assert!(!orphan["group_anchors"].as_array().unwrap().is_empty());
    let descendant: i32 = fs::read_to_string(fixture.repo.join("orphan.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(active_pid(descendant));
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 1);
    let stopped = ok(&home, "job.cancel", json!({"id":"ordinary-orphan"}));
    assert_eq!(stopped["status"], "cancelled");
    assert_eq!(stopped["reason"], "cancellation requested");
    assert!(!active_pid(descendant));
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
}

#[test]
fn recording_task_done_does_not_kill_its_already_running_result_producer() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    start(
        &home,
        "finish",
        &["/bin/sh", "-c", "sleep 0.4; printf finished > done.txt"],
    );
    wait(&home, "finish", &["running"]);
    let store = Store::open(&home).unwrap();
    store
        .connection()
        .execute("UPDATE tasks SET status='done' WHERE id='task'", [])
        .unwrap();
    wait(&home, "finish", &["succeeded"]);
    assert_eq!(
        fs::read_to_string(fixture.repo.join("done.txt")).unwrap(),
        "finished"
    );
    assert!(
        !run(
            &home,
            "job.start",
            json!({"task_id":"task","command":["/bin/echo","new-work"]})
        )
        .status
        .success()
    );
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
}

#[test]
fn unexpected_supervisor_error_is_private_durable_and_keeps_payload_capacity() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    let fakebin = fixture.root.join("fakebin");
    fs::create_dir(&fakebin).unwrap();
    let marker = fixture.repo.join("fail-ps");
    let script = format!(
        "#!/bin/sh\nif test -f '{}'; then exit 2; fi\nexec /bin/ps \"$@\"\n",
        marker.display()
    );
    fs::write(fakebin.join("ps"), script).unwrap();
    fs::set_permissions(fakebin.join("ps"), fs::Permissions::from_mode(0o700)).unwrap();
    let output = support::command(BIN).arg("--home").arg(&home).arg("job.start").arg("--json")
        .arg(json!({"task_id":"task","id":"diagnostic","command":["/bin/sh","-c","sleep 0.3; touch fail-ps; exec sleep 30"]}).to_string())
        .env("PATH",format!("{}:/usr/bin:/bin",fakebin.display())).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let orphan = wait(&home, "diagnostic", &["orphaned"]);
    let logfile = home
        .join("jobs/diagnostic")
        .join(orphan["incarnation"].as_str().unwrap())
        .join("supervisor.log");
    let diagnostic = fs::read_to_string(&logfile).unwrap();
    assert!(diagnostic.contains("inspection failed"), "{diagnostic}");
    assert!(fs::metadata(&logfile).unwrap().len() <= 1024 * 1024);
    assert_eq!(
        fs::metadata(&logfile).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 1);
    ok(&home, "job.cancel", json!({"id":"diagnostic"}));
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
}

#[test]
fn completion_cleanup_stops_leftover_helpers_by_default_and_preserves_success() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    ok(
        &home,
        "job.start",
        json!({"task_id":"task","id":"clean-completion",
        "command":["/bin/sh","-c","sh -c 'trap \"\" TERM; echo $$ > cleanup.pid; while :; do sleep 1; done' & while test ! -f cleanup.pid; do sleep 0.01; done; exit 0"]}),
    );
    let completed = wait(&home, "clean-completion", &["succeeded"]);
    assert_eq!(completed["exit_code"], 0);
    assert_eq!(completed["reason"], Value::Null);
    assert_eq!(completed["cleanup_descendants"], true);
    assert!(
        completed["finished_ms"].as_i64().unwrap() - completed["created_ms"].as_i64().unwrap()
            >= 1800
    );
    let descendant: i32 = fs::read_to_string(fixture.repo.join("cleanup.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(!active_pid(descendant));
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
}

fn ok_env(home: &Path, command: &str, args: Value, env: &[(&str, &str)]) -> Value {
    let output = support::command(BIN)
        .arg("--home")
        .arg(home)
        .arg(command)
        .arg("--json")
        .arg(args.to_string())
        .envs(env.iter().copied())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{command}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn process_identity_does_not_depend_on_caller_timezone_or_locale() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    ok_env(
        &home,
        "job.start",
        json!({"task_id":"task","id":"zoned","command":["sleep","30"],"heavy":true}),
        &[("TZ", "UTC0"), ("LC_ALL", "C")],
    );
    wait(&home, "zoned", &["running"]);
    // Status, listing and capacity inspection from another environment must
    // still recognise the supervisor and payload recorded under the first.
    let elsewhere = [("TZ", "JST-9"), ("LC_ALL", "C.UTF-8")];
    let job = ok_env(&home, "job.status", json!({"id":"zoned"}), &elsewhere);
    assert_eq!(job["status"], "running", "{job}");
    assert_eq!(job["reason"], Value::Null);
    let listed = ok_env(&home, "job.list", json!({}), &elsewhere)["jobs"].clone();
    assert_eq!(listed[0]["status"], "running");
    let resources = ok_env(&home, "resource.status", json!({}), &elsewhere);
    assert_eq!(resources["reservations"][0]["supervisor_alive"], true);
    assert_eq!(resources["reservations"][0]["payload_group_alive"], true);
    ok_env(&home, "job.cancel", json!({"id":"zoned"}), &elsewhere);
    let cancelled = wait(&home, "zoned", &["cancelled"]);
    assert!(!active_pid(
        cancelled["payload"]["pid"].as_i64().unwrap() as i32
    ));
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
}

#[cfg(target_os = "linux")]
#[test]
fn records_from_the_previous_identity_format_remain_recoverable() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    start(&home, "legacy", &["sleep", "30"]);
    let running = wait(&home, "legacy", &["running"]);
    let supervisor = running["supervisor"]["pid"].as_i64().unwrap() as i32;
    let payload = running["payload"]["pid"].as_i64().unwrap() as i32;
    // Rewrite every recorded identity as 0.1.0 stored it: local lstart + ticks.
    let store = Store::open(&home).unwrap();
    let raw: String = store
        .connection()
        .query_row("SELECT record FROM jobs WHERE id='legacy'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let mut record: Value = serde_json::from_str(&raw).unwrap();
    let legacy = |p: &mut Value| {
        let ticks = p["started"].as_str().unwrap().rsplit(':').next().unwrap();
        p["started"] = json!(format!("Thu Oct 15 05:21:59 2026|ticks:{ticks}"));
    };
    legacy(&mut record["supervisor"]);
    legacy(&mut record["payload"]);
    for anchor in record["group_anchors"].as_array_mut().unwrap() {
        legacy(anchor);
    }
    store
        .connection()
        .execute(
            "UPDATE jobs SET record=?1 WHERE id='legacy'",
            params![record.to_string()],
        )
        .unwrap();
    assert_eq!(
        ok(&home, "job.status", json!({"id":"legacy"}))["status"],
        "running"
    );
    // Without its supervisor, cancellation must still prove and stop the payload.
    assert_eq!(unsafe { libc::kill(supervisor, libc::SIGKILL) }, 0);
    let orphaned = fixture.settle(&home, "legacy", &["orphaned"]);
    assert_eq!(
        orphaned["reason"],
        "supervisor disappeared; payload capacity preserved"
    );
    ok(&home, "job.cancel", json!({"id":"legacy"}));
    assert_eq!(
        ok(&home, "job.status", json!({"id":"legacy"}))["status"],
        "cancelled"
    );
    assert!(!active_pid(payload));
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
}

fn leak_reservation(fixture: &Fixture, home: &Path, job: &str, age_ms: i64) -> String {
    // A reservation committed to the shared ledger whose home commit never
    // happened: no job record, supervisor or payload identity exists.
    ok(home, "resource.status", json!({})); // creates the shared ledger
    let key = format!("{}:{job}:lost-incarnation", home.display());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let record = json!({"key":key,"home":home,"job_id":job,"incarnation":"lost-incarnation",
        "heavy":true,"state":"starting","created_ms":now - age_ms,"supervisor":null,"payload":null});
    rusqlite::Connection::open(fixture.ledger.join("resources.sqlite"))
        .unwrap()
        .execute(
            "INSERT INTO reservations(key,record) VALUES(?1,?2)",
            params![key, record.to_string()],
        )
        .unwrap();
    key
}
fn fails(home: &Path, command: &str, args: Value) -> String {
    let output = run(home, command, args);
    assert!(!output.status.success(), "{command} unexpectedly succeeded");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn orphan_settles_to_its_leader_result_once_descendants_exit() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    start_keeping(&home, "lingering", &["/bin/sh", "-c", "sleep 1 & exit 0"]);
    let orphan = wait(&home, "lingering", &["orphaned", "succeeded"]);
    assert_eq!(
        orphan["status"], "orphaned",
        "descendant should outlive leader"
    );
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 1);
    let settled = fixture.settle(&home, "lingering", &["succeeded"]);
    assert_eq!(settled["exit_code"], 0);
    assert_eq!(settled["reason"], Value::Null);
    assert_eq!(settled["leader_result"], Value::Null);
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
    // A failing leader settles to failure with its exit code.
    start_keeping(
        &home,
        "lingering-failure",
        &["/bin/sh", "-c", "sleep 1 & exit 3"],
    );
    let failed = fixture.settle(&home, "lingering-failure", &["failed"]);
    assert_eq!(failed["exit_code"], 3);
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
    let audit = ok(&home, "audit.list", json!({"limit":1000}));
    let settled_events = audit["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "job.settled")
        .count();
    assert_eq!(settled_events, 2);
}

#[test]
fn recovery_releases_a_leaked_reservation_only_when_forced_after_grace() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    let other = fixture.home("other", 0);
    let leaked = leak_reservation(&fixture, &home, "lost", 60_000);
    let error = fails(
        &home,
        "job.start",
        json!({"task_id":"task","id":"blocked","command":["true"]}),
    );
    assert!(error.contains("capacity exhausted"), "{error}");
    let error = fails(&home, "resource.recover", json!({"key":leaked}));
    assert!(error.contains("force:true"), "{error}");
    // Any home sharing the ledger can perform the explicit recovery.
    let released = ok(
        &other,
        "resource.recover",
        json!({"key":leaked,"force":true}),
    );
    assert_eq!(released["forced"], true);
    assert_eq!(released["job_id"], "lost");
    let recovered = ok(&other, "audit.list", json!({"limit":1000}))["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "resource.recovered" && e["payload"]["key"] == leaked);
    assert!(recovered);
    start(&home, "admitted", &["true"]);
    wait(&home, "admitted", &["succeeded"]);
    // A reservation inside its start window may still be launching.
    let fresh = leak_reservation(&fixture, &home, "fresh-leak", 0);
    let error = fails(&home, "resource.recover", json!({"key":fresh,"force":true}));
    assert!(error.contains("start grace period"), "{error}");
}

#[test]
fn recovery_never_releases_live_supervisors_or_payload_groups() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    let running = start(&home, "live", &["sleep", "30"]);
    let running = wait(&home, running["id"].as_str().unwrap(), &["running"]);
    let key = ok(&home, "resource.status", json!({}))["reservations"][0]["key"]
        .as_str()
        .unwrap()
        .to_owned();
    let error = fails(&home, "resource.recover", json!({"key":key,"force":true}));
    assert!(error.contains("supervisor is still running"), "{error}");
    let supervisor = running["supervisor"]["pid"].as_i64().unwrap() as i32;
    assert_eq!(unsafe { libc::kill(supervisor, libc::SIGKILL) }, 0);
    fixture.settle(&home, "live", &["orphaned"]);
    let error = fails(&home, "resource.recover", json!({"key":key,"force":true}));
    assert!(error.contains("payload group is still running"), "{error}");
    ok(&home, "job.cancel", json!({"id":"live"}));
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
}

#[test]
fn recovery_settles_this_homes_job_when_its_processes_are_provably_gone() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    let running = start(&home, "vanished", &["sleep", "30"]);
    let running = wait(&home, running["id"].as_str().unwrap(), &["running"]);
    let key = ok(&home, "resource.status", json!({}))["reservations"][0]["key"]
        .as_str()
        .unwrap()
        .to_owned();
    // Supervisor and payload both die without recording an outcome.
    let supervisor = running["supervisor"]["pid"].as_i64().unwrap() as i32;
    let payload = running["payload"]["pid"].as_i64().unwrap() as i32;
    assert_eq!(unsafe { libc::kill(supervisor, libc::SIGKILL) }, 0);
    assert_eq!(unsafe { libc::kill(-payload, libc::SIGKILL) }, 0);
    let until = Instant::now() + Duration::from_secs(5);
    while active_pid(payload) && Instant::now() < until {
        thread::sleep(Duration::from_millis(20));
    }
    let released = ok(&home, "resource.recover", json!({"key":key}));
    assert_eq!(released["forced"], false);
    let job = ok(&home, "job.status", json!({"id":"vanished"}));
    assert_eq!(job["status"], "failed");
    assert_eq!(job["reason"], "reservation released by resource.recover");
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
}

#[test]
fn vanished_supervisor_with_ended_payload_settles_as_failed() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    let running = start(&home, "abandoned", &["sleep", "30"]);
    let running = wait(&home, running["id"].as_str().unwrap(), &["running"]);
    let supervisor = running["supervisor"]["pid"].as_i64().unwrap() as i32;
    let payload = running["payload"]["pid"].as_i64().unwrap() as i32;
    assert_eq!(unsafe { libc::kill(supervisor, libc::SIGKILL) }, 0);
    assert_eq!(unsafe { libc::kill(-payload, libc::SIGKILL) }, 0);
    let job = fixture.settle(&home, "abandoned", &["failed"]);
    assert_eq!(
        job["reason"],
        "supervisor disappeared; payload ended without a recorded result"
    );
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
}

#[test]
fn job_list_puts_live_work_first_and_pages_every_job_once() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    // The live job is the oldest, so only its status can put it first.
    start(&home, "live", &["sleep", "30"]);
    wait(&home, "live", &["running"]);
    for n in 0..5 {
        start_light(&home, &format!("done-{n}"), &["true"]);
    }
    let ids = |page: &Value| -> Vec<String> {
        page["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|j| j["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let default = ok(&home, "job.list", json!({}));
    assert_eq!(ids(&default)[0], "live");
    assert_eq!(default["jobs"][0]["status"], "running");
    assert_eq!(default["more"], false);
    let small = ok(&home, "job.list", json!({"limit":2}));
    assert_eq!(ids(&small)[0], "live");
    assert_eq!(small["more"], true);
    assert!(small["next_cursor"].is_string());
    // Following the cursor reaches every job exactly once.
    let mut listed = Vec::new();
    let mut page = small;
    loop {
        listed.extend(ids(&page));
        if page["more"] == false {
            break;
        }
        let cursor = page["next_cursor"].clone();
        page = ok(&home, "job.list", json!({"limit":2,"cursor":cursor}));
    }
    let mut unique = listed.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(listed.len(), 6, "{listed:?}");
    assert_eq!(unique.len(), 6, "{listed:?}");
    assert_eq!(listed[0], "live");
    // Finished jobs follow, newest first.
    let finished = ok(&home, "job.list", json!({}));
    let created: Vec<i64> = finished["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .skip(1)
        .map(|j| j["created_ms"].as_i64().unwrap())
        .collect();
    assert!(created.windows(2).all(|w| w[0] >= w[1]), "{created:?}");
    ok(&home, "job.cancel", json!({"id":"live"}));
    wait(&home, "live", &["cancelled"]);
}

#[test]
fn job_reads_leave_a_lost_supervisor_to_the_dispatcher() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    start(&home, "lost", &["sleep", "30"]);
    let running = wait(&home, "lost", &["running"]);
    let supervisor = running["supervisor"]["pid"].as_i64().unwrap() as i32;
    assert_eq!(unsafe { libc::kill(supervisor, libc::SIGKILL) }, 0);
    let until = Instant::now() + Duration::from_secs(3);
    while active_pid(supervisor) && Instant::now() < until {
        thread::sleep(Duration::from_millis(20));
    }
    // A reader may be in a PID sandbox where the supervisor looks gone.
    assert_eq!(
        ok(&home, "job.status", json!({"id":"lost"}))["status"],
        "running"
    );
    assert_eq!(
        ok(&home, "job.list", json!({}))["jobs"][0]["status"],
        "running"
    );
    let orphan = fixture.settle(&home, "lost", &["orphaned"]);
    assert_eq!(
        orphan["reason"],
        "supervisor disappeared; payload capacity preserved"
    );
    ok(&home, "job.cancel", json!({"id":"lost"}));
}

#[cfg(target_os = "linux")]
#[test]
fn work_recorded_in_another_pid_namespace_is_never_settled_or_started() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    start(&home, "foreign", &["sleep", "30"]);
    let running = wait(&home, "foreign", &["running"]);
    let supervisor = running["supervisor"]["pid"].as_i64().unwrap() as i32;
    let store = Store::open(&home).unwrap();
    let edit = |namespace: &str| {
        store
            .connection()
            .execute(
                "UPDATE jobs SET record=json_set(record, '$.supervisor.pid_namespace', ?1)
                 WHERE id='foreign'",
                [namespace],
            )
            .unwrap();
    };
    // Its PID number means nothing here, so a gone supervisor proves nothing.
    edit("pid:[foreign]");
    assert_eq!(unsafe { libc::kill(supervisor, libc::SIGKILL) }, 0);
    fixture.serve(&home);
    assert_eq!(
        ok(&home, "job.status", json!({"id":"foreign"}))["status"],
        "running"
    );
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 1);
    // So new work is refused outside the dispatcher's namespace.
    store
        .connection()
        .execute(
            "UPDATE metadata SET value='pid:[foreign]' WHERE key='pid_namespace'",
            [],
        )
        .unwrap();
    let error = fails(
        &home,
        "job.start",
        json!({"task_id":"task","id":"sandboxed","command":["true"]}),
    );
    assert!(error.contains("outside the PID sandbox"), "{error}");
    edit(running["supervisor"]["pid_namespace"].as_str().unwrap());
    fixture.serve(&home);
    ok(&home, "job.cancel", json!({"id":"foreign"}));
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
}

#[test]
fn a_read_only_codex_crew_that_reports_nothing_is_recorded_failed() {
    let fixture = Fixture::new();
    let home = fixture.home("home", 0);
    ok(
        &home,
        "job.start",
        json!({"task_id":"task","id":"readonly-noop","command":["sh","-c","exit 0"],
            "heavy":true,"codex_read_only_crew":true}),
    );
    let settled = wait(&home, "readonly-noop", &["failed", "succeeded"]);
    assert_eq!(settled["status"], "failed", "{settled}");
    assert!(
        settled["reason"]
            .as_str()
            .unwrap_or("")
            .contains("read-only"),
        "{settled}"
    );
}

#[test]
fn a_read_only_codex_crew_with_a_durable_report_succeeds() {
    let fixture = Fixture::new();
    let home = fixture.home("home", 0);
    let started = ok(
        &home,
        "job.start",
        json!({"task_id":"task","id":"readonly-reported","command":["sh","-c","sleep 1; exit 0"],
            "heavy":true,"codex_read_only_crew":true}),
    );
    let incarnation = started["incarnation"].as_str().unwrap().to_owned();
    {
        let store = Store::open(&home).unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO mail(id,sender,recipient,kind,task_id,body,payload,urgency,created_ms,state)
                 VALUES(?1,'crew','peer','notice','task','reported',?2,'normal',0,NULL)",
                params![
                    format!("crew-readonly-reported-{incarnation}"),
                    json!({"job_id":"readonly-reported","incarnation":incarnation.clone(),"report":"did work"})
                        .to_string()
                ],
            )
            .unwrap();
    }
    let settled = wait(&home, "readonly-reported", &["failed", "succeeded"]);
    assert_eq!(settled["status"], "succeeded", "{settled}");
}

#[test]
fn agent_jobs_carry_the_role_into_their_payload() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    let agent = [("PRIVATEER_ROLE", "agent"), ("PRIVATEER_ACTOR", "peer")];
    // The payload is an agent: an operator command fails inside it.
    let script = format!(
        "'{BIN}' --home '{}' charter.put --json '{{\"id\":\"mission\",\"active\":true}}' 2>/dev/null; echo \"$? $PRIVATEER_ROLE $PRIVATEER_ACTOR\" > role.txt",
        home.display()
    );
    let started = ok_env(
        &home,
        "job.start",
        json!({"task_id":"task","id":"agent-work","command":["/bin/sh","-c",script],"heavy":false}),
        &agent,
    );
    assert_eq!(started["agent_actor"], "peer");
    wait(&home, "agent-work", &["succeeded"]);
    assert_eq!(
        fs::read_to_string(fixture.repo.join("role.txt")).unwrap(),
        "1 agent peer\n"
    );
    // Operator jobs never inherit a stray role marker.
    let started = ok(
        &home,
        "job.start",
        json!({"task_id":"task","id":"operator-work","heavy":false,
            "command":["/bin/sh","-c","echo \"${PRIVATEER_ROLE:-operator}\" > role.txt"]}),
    );
    assert_eq!(started["agent_actor"], Value::Null);
    wait(&home, "operator-work", &["succeeded"]);
    assert_eq!(
        fs::read_to_string(fixture.repo.join("role.txt")).unwrap(),
        "operator\n"
    );
    // Agents start work only on tasks they own, and only as themselves.
    let output = support::command(BIN)
        .args(["--home", home.to_str().unwrap(), "job.start", "--json"])
        .arg(json!({"task_id":"task","id":"foreign","command":["true"],"heavy":false}).to_string())
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "someone-else")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("tasks it owns"));
    let output = support::command(BIN)
        .args(["--home", home.to_str().unwrap(), "job.start", "--json"])
        .arg(json!({"task_id":"task","id":"disguised","command":["true"],"heavy":false,"agent_actor":"other"}).to_string())
        .envs(agent)
        .output()
        .unwrap();
    assert!(!output.status.success());
}

#[test]
fn history_prune_keeps_pending_facts_and_reserved_capacity() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    let emit = |id: &str, issue: &str| {
        ok(
            &home,
            "event.emit",
            json!({"id":id,"task_id":"task","issue_key":issue,"kind":"alert"}),
        )["seq"]
            .as_i64()
            .unwrap()
    };
    emit("acked-1", "build");
    let last = emit("acked-2", "build");
    ok(
        &home,
        "event.ack",
        json!({"task_id":"task","issue_key":"build","through_seq":last}),
    );
    emit("pending-1", "deploy");
    // The fact message.send records (this fixture's charter has no "message").
    ok(
        &home,
        "event.emit",
        json!({"id":"unread","task_id":"task","issue_key":"message:infra:unread","kind":"message",
            "payload":{"from":"peer","to":"infra","body":"look"}}),
    );
    start(&home, "finished", &["true"]);
    wait(&home, "finished", &["succeeded"]);
    // A finished job whose reservation survived the completion/release crash
    // window still holds capacity and must not be forgotten.
    start(&home, "held", &["true"]);
    let held = wait(&home, "held", &["succeeded"]);
    let key = format!(
        "{}:held:{}",
        home.display(),
        held["incarnation"].as_str().unwrap()
    );
    rusqlite::Connection::open(fixture.ledger.join("resources.sqlite"))
        .unwrap()
        .execute(
            "INSERT INTO reservations(key,record) VALUES(?1,?2)",
            params![
                key,
                json!({"key":key,"home":home,"job_id":"held","incarnation":held["incarnation"],
                "heavy":false,"state":"succeeded","created_ms":0,"supervisor":null,"payload":null})
                .to_string()
            ],
        )
        .unwrap();
    assert!(home.join("jobs/finished").exists());
    let now = ok(&home, "status", json!({}));
    let cutoff = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 1000;

    let preview = ok(
        &home,
        "history.prune",
        json!({"before_ms":cutoff,"dry_run":true}),
    );
    assert_eq!(preview["events"], 2, "{preview}");
    assert_eq!(preview["jobs"], 1);
    assert_eq!(preview["jobs_still_reserved"], 1);
    assert!(preview["audit"].as_i64().unwrap() > 0);
    assert_eq!(ok(&home, "status", json!({})), now);
    assert!(home.join("jobs/finished").exists());

    let pruned = ok(&home, "history.prune", json!({"before_ms":cutoff}));
    assert_eq!(pruned["events"], 2);
    assert_eq!(pruned["jobs"], 1);
    assert_eq!(pruned["log_directories_failed"], 0);
    let history = ok(&home, "event.history", json!({"limit":100}));
    let ids: Vec<_> = history["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, ["pending-1", "unread"]);
    let status = ok(&home, "status", json!({}));
    assert_eq!(status["pending_notifications"], 2);
    let inbox = ok(&home, "message.inbox", json!({"actor":"infra"}));
    assert_eq!(inbox["messages"][0]["id"], "unread");
    assert!(
        !run(&home, "job.status", json!({"id":"finished"}))
            .status
            .success()
    );
    assert!(!home.join("jobs/finished").exists());
    assert_eq!(
        ok(&home, "job.status", json!({"id":"held"}))["status"],
        "succeeded"
    );
    let audit = ok(&home, "audit.list", json!({"limit":1000}))["events"].clone();
    assert_eq!(audit.as_array().unwrap().len(), 1);
    assert_eq!(audit[0]["kind"], "history.prune");

    // Facts written before events recorded a time count as older than any cutoff.
    let seq = emit("legacy", "legacy");
    ok(
        &home,
        "event.ack",
        json!({"task_id":"task","issue_key":"legacy","through_seq":seq}),
    );
    Store::open(&home)
        .unwrap()
        .connection()
        .execute("UPDATE events SET time_ms=NULL WHERE id='legacy'", [])
        .unwrap();
    assert_eq!(
        ok(&home, "history.prune", json!({"before_ms":1}))["events"],
        1
    );
    assert_eq!(
        ok(&home, "history.prune", json!({"older_than_days":30}))["events"],
        0
    );
    for bad in [
        json!({}),
        json!({"before_ms":1,"older_than_days":1}),
        json!({"older_than_days":0}),
        json!({"before_ms":1,"dry_run":"yes"}),
    ] {
        assert!(
            !run(&home, "history.prune", bad.clone()).status.success(),
            "{bad}"
        );
    }
}

#[test]
fn job_list_is_read_only_and_dispatcher_repairs_a_leftover_reservation() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    for id in ["a", "b", "c"] {
        start(&home, id, &["true"]);
        wait(&home, id, &["succeeded"]);
    }
    let ids = |page: &Value| -> Vec<String> {
        page["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|j| j["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let first = ok(&home, "job.list", json!({"limit":2}));
    assert_eq!(ids(&first), ["c", "b"]);
    assert_eq!(first["more"], true);
    let rest = ok(
        &home,
        "job.list",
        json!({"limit":2,"cursor":first["next_cursor"]}),
    );
    assert_eq!(ids(&rest), ["a"]);
    assert_eq!(rest["more"], false);
    let after = ok(&home, "job.list", json!({"limit":2,"after_id":"b"}));
    assert_eq!(ids(&after), ["a"]);
    assert!(
        !run(&home, "job.list", json!({"limit":"all"}))
            .status
            .success()
    );
    // A finished job whose release was interrupted still holds capacity;
    // listing is read-only and the owning dispatcher repairs it.
    let job = ok(&home, "job.status", json!({"id":"b"}));
    let key = format!(
        "{}:b:{}",
        home.display(),
        job["incarnation"].as_str().unwrap()
    );
    rusqlite::Connection::open(fixture.ledger.join("resources.sqlite"))
        .unwrap()
        .execute(
            "INSERT INTO reservations(key,record) VALUES(?1,?2)",
            params![key, json!({"key":key,"home":home,"job_id":"b","incarnation":job["incarnation"],
                "heavy":true,"state":"succeeded","created_ms":0,"supervisor":job["supervisor"],"payload":job["payload"]}).to_string()],
        )
        .unwrap();
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 1);
    ok(&home, "job.list", json!({}));
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 1);
    fixture.serve(&home);
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
}

fn heavy_job(home: &Path, id: &str, extra: Value) -> std::process::Output {
    let mut args = json!({"task_id":"task","id":id,"command":["sleep","30"],"heavy":true});
    for (key, value) in extra.as_object().unwrap() {
        args[key] = value.clone();
    }
    run(home, "job.start", args)
}

#[test]
fn resource_settings_are_host_wide_open_to_agents_and_validated() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    let other = fixture.home("other", 0);
    let agent = [("PRIVATEER_ROLE", "agent"), ("PRIVATEER_ACTOR", "peer")];
    // An unconfigured host applies the documented 2 GiB free-disk floor.
    let fresh = ok(&other, "resource.status", json!({}));
    assert_eq!(fresh["settings"]["min_free_disk_bytes"], 2_147_483_648_u64);
    let configured = ok_env(
        &home,
        "resource.configure",
        json!({"max_load_per_cpu":0.7,"min_available_memory_bytes":4_294_967_296_u64,"min_free_disk_bytes":123}),
        &agent,
    );
    assert_eq!(configured["settings"]["max_load_per_cpu"], 0.7);
    // Another home on the host sees the same policy, with live readings.
    let status = ok(&other, "resource.status", json!({}));
    assert_eq!(status["settings"]["max_load_per_cpu"], 0.7);
    assert_eq!(
        status["settings"]["min_available_memory_bytes"],
        4_294_967_296_u64
    );
    assert_eq!(status["settings"]["min_free_disk_bytes"], 123);
    let host = &status["host"];
    assert!(host["cpus"].as_f64().unwrap() >= 1.0);
    assert!(host["load"].as_f64().unwrap() >= 0.0);
    let available = host["memory_available_bytes"].as_u64().unwrap();
    assert!(available > 0 && available <= host["memory_total_bytes"].as_u64().unwrap());
    assert!(host["disk_available_bytes"].as_u64().unwrap() > 0);
    assert!(
        host["disk_total_bytes"].as_u64().unwrap()
            >= host["disk_available_bytes"].as_u64().unwrap()
    );
    // null restores a default.
    let reset = ok(
        &other,
        "resource.configure",
        json!({"max_load_per_cpu":null}),
    );
    assert_eq!(reset["settings"]["max_load_per_cpu"], 1.0);
    for bad in [
        json!({"max_load_per_cpu":0}),
        json!({"max_heavy_jobs":0}),
        json!({"heavy_estimate":{"gpus":1}}),
        json!({"unknown":1}),
    ] {
        assert!(
            !run(&home, "resource.configure", bad.clone())
                .status
                .success(),
            "{bad}"
        );
    }
    let audit = ok(&home, "audit.list", json!({"limit":100}));
    // The fixture configured the host as operator first; the agent's change
    // is recorded with its identity.
    let by_agent = audit["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "resource.configured" && e["payload"]["by"]["agent"] == "peer");
    assert!(by_agent, "{audit}");
}

#[test]
fn disk_floor_refusal_queues_jobs_and_resource_status_reports_it() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    let total = ok(&home, "resource.status", json!({}))["host"]["disk_total_bytes"]
        .as_u64()
        .unwrap();
    ok(
        &home,
        "resource.configure",
        json!({"min_free_disk_bytes":total.saturating_mul(2)}),
    );
    let refused = run(
        &home,
        "job.start",
        json!({"task_id":"task","id":"disk-refused","command":["true"],"heavy":false}),
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("disk available"));

    let queued = ok(
        &home,
        "job.start",
        json!({"task_id":"task","id":"disk-queued","command":["true"],"heavy":false,"wait_for_capacity":true}),
    );
    assert_eq!(queued["status"], "queued");
    assert!(
        queued["reason"]
            .as_str()
            .unwrap()
            .contains("disk available")
    );
    let status = ok(&home, "resource.status", json!({}));
    assert_eq!(status["disk_floor_met"], false);
    assert_eq!(status["queue"][0]["job_id"], "disk-queued");

    let store = Store::open(&home).unwrap();
    store
        .connection()
        .execute(
            "UPDATE jobs SET record=json_set(record,'$.created_ms',?2) WHERE id=?1",
            params!["disk-queued", privateer::store::Store::now_ms() - 61_000],
        )
        .unwrap();
    let needs =
        privateer::dispatch(&mut Store::open(&home).unwrap(), "runtime.needs", json!({})).unwrap();
    let blocker = needs["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["source_id"] == "disk-queued")
        .unwrap();
    assert_eq!(blocker["evidence"], "disk_floor_wait");

    ok(
        &home,
        "resource.configure",
        json!({"min_free_disk_bytes":0}),
    );
    assert_eq!(
        wait(&home, "disk-queued", &["succeeded"])["status"],
        "succeeded"
    );
    assert_eq!(ok(&home, "resource.status", json!({}))["queue"], json!([]));
}

#[test]
fn a_job_declares_disk_demand_and_admission_counts_it() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    let other = fixture.home("other", 0);
    let status = ok(&home, "resource.status", json!({}));
    let free = status["host"]["disk_available_bytes"].as_u64().unwrap();
    let total = status["host"]["disk_total_bytes"].as_u64().unwrap();
    // Margins are fractions of what is free now, so the ordinary writes of
    // other work on the machine move them little: a holding job takes half,
    // and a job that wants three quarters more than what is left is refused.
    let default_disk = free / 2;
    ok(
        &home,
        "resource.configure",
        json!({"min_free_disk_bytes":0,"heavy_estimate":{"disk_bytes":default_disk,"memory_bytes":0},
            "max_heavy_jobs":null,"max_load_per_cpu":1024.0}),
    );
    // A light job without an estimate needs no disk, and runs.
    start_light(&home, "light", &["true"]);
    // A job that never says whether it is heavy is heavy for CPU and memory
    // but declares no disk, so it reserves none.
    let plain = run(
        &home,
        "job.start",
        json!({"task_id":"task","id":"plain","command":["sleep","30"]}),
    );
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    wait(&home, "plain", &["running"]);
    let plain_held = ok(&home, "resource.status", json!({}))["reservations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["job_id"] == "plain")
        .cloned()
        .unwrap();
    assert_eq!(plain_held["demand"]["disk_bytes"], 0);
    ok(&home, "job.cancel", json!({"id":"plain"}));
    let freed = Instant::now() + Duration::from_secs(10);
    while ok(&home, "resource.status", json!({}))["reservations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["job_id"] == "plain")
    {
        assert!(
            Instant::now() < freed,
            "the cancelled job kept its reservation"
        );
        thread::sleep(Duration::from_millis(100));
    }
    // A job may declare what it will really write, in either direction.
    let modest = heavy_job(&other, "modest", json!({"estimate":{"disk_bytes":free/4}}));
    assert!(
        modest.status.success(),
        "{}",
        String::from_utf8_lossy(&modest.stderr)
    );
    ok(&other, "job.cancel", json!({"id":"modest"}));
    // More than the whole disk can never fit, so it is refused at once with
    // a reason that says so, not queued behind the others.
    let never = heavy_job(&other, "never", json!({"estimate":{"disk_bytes":total+1}}));
    assert!(!never.status.success());
    assert!(String::from_utf8_lossy(&never.stderr).contains("never fits"));
    // Disk a running job holds counts against the next one, in another home,
    // for as long as that job runs.
    let holding = run(
        &home,
        "job.start",
        json!({"task_id":"task","id":"holding","command":["sleep","30"],"heavy":true,
            "estimate":{"disk_bytes":free/2}}),
    );
    assert!(
        holding.status.success(),
        "{}",
        String::from_utf8_lossy(&holding.stderr)
    );
    wait(&home, "holding", &["running"]);
    // The resolved demand is recorded with the reservation, so another home's
    // admission and resource.status both see it.
    let reservations = ok(&home, "resource.status", json!({}))["reservations"].clone();
    let held = reservations
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["job_id"] == "holding")
        .unwrap();
    assert_eq!(held["demand"]["disk_bytes"], free / 2);
    let behind = heavy_job(
        &other,
        "behind",
        json!({"estimate":{"disk_bytes":free*3/4}}),
    );
    assert!(!behind.status.success());
    assert!(String::from_utf8_lossy(&behind.stderr).contains("declared by running jobs"));
    ok(&home, "job.cancel", json!({"id":"holding"}));
    // Once the first job is over and its reservation is released, its disk is
    // not held back any more.
    let released = Instant::now() + Duration::from_secs(10);
    while ok(&home, "resource.status", json!({}))["reservations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["job_id"] == "holding")
    {
        assert!(
            Instant::now() < released,
            "the cancelled job kept its reservation"
        );
        thread::sleep(Duration::from_millis(100));
    }
    let after = heavy_job(&other, "after", json!({"estimate":{"disk_bytes":free/2}}));
    assert!(
        after.status.success(),
        "{}",
        String::from_utf8_lossy(&after.stderr)
    );
    ok(&other, "job.cancel", json!({"id":"after"}));
    // The heavy default and the warning horizon are host-wide settings.
    ok(
        &home,
        "resource.configure",
        json!({"heavy_estimate":{"disk_bytes":default_disk,"memory_bytes":0},"disk_warn_horizon_secs":0}),
    );
    assert_eq!(
        ok(&home, "resource.status", json!({}))["settings"]["heavy_estimate"]["disk_bytes"],
        default_disk
    );
    assert_eq!(
        ok(&home, "resource.status", json!({}))["settings"]["disk_warn_horizon_secs"],
        0
    );
    for bad in [
        json!({"heavy_estimate":{"disk_bytes":-1}}),
        json!({"disk_warn_horizon_secs":-1}),
        json!({"disk_warn_horizon_secs":30*86_400+1}),
    ] {
        assert!(
            !run(&home, "resource.configure", bad.clone())
                .status
                .success(),
            "{bad}"
        );
    }
    let refused = run(
        &home,
        "job.start",
        json!({"task_id":"task","id":"bad-declared","command":["true"],"estimate":{"disk_bytes":-1}}),
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("whole number of bytes"));
}

#[test]
fn a_reservation_written_before_jobs_declared_disk_stays_readable() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    ok(
        &home,
        "job.start",
        json!({"task_id":"task","id":"legacy-demand","command":["sleep","30"],"heavy":false}),
    );
    wait(&home, "legacy-demand", &["running"]);
    let conn = rusqlite::Connection::open(fixture.ledger.join("resources.sqlite")).unwrap();
    let (key, record): (String, String) = conn
        .query_row(
            "SELECT key,record FROM reservations WHERE json_extract(record,'$.job_id')='legacy-demand'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    // The shape an older build wrote: no disk demand in the record.
    let mut stored: Value = serde_json::from_str(&record).unwrap();
    stored["demand"] = json!({"cpus":1.0,"memory_bytes":0});
    conn.execute(
        "UPDATE reservations SET record=?2 WHERE key=?1",
        params![key, stored.to_string()],
    )
    .unwrap();
    // Reading it counts no disk, rather than refusing the whole host status.
    let status = ok(&home, "resource.status", json!({}));
    let held = status["reservations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["job_id"] == "legacy-demand")
        .unwrap();
    assert_eq!(held["demand"]["disk_bytes"], 0);
    ok(&home, "job.cancel", json!({"id":"legacy-demand"}));
}

#[test]
fn admission_keeps_a_free_space_sample_and_resource_status_reports_the_trend() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    // One reading is not yet a rate.
    assert_eq!(
        ok(&home, "resource.status", json!({}))["disk_trend"],
        json!(null)
    );
    assert_eq!(
        ok(&home, "resource.status", json!({}))["disk_forecast"],
        json!(null)
    );
    // A job records the reading it admitted against, under the filesystem it
    // runs on, whichever home that work belongs to.
    let key = privateer::admission::disk_key(&home).unwrap();
    start_light(&home, "sampler", &["true"]);
    let status = ok(&home, "resource.status", json!({}));
    let conn = rusqlite::Connection::open(fixture.ledger.join("resources.sqlite")).unwrap();
    let samples: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM disk_samples WHERE path=?1",
            [&key],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(samples, 1, "{status}");
    // Readings hours apart describe a rate, which resource.status reports and
    // the owner's warning uses.
    conn.execute(
        "INSERT INTO disk_samples(path,sampled_ms,free_bytes) VALUES(?1,?2,?3)",
        params![
            key,
            Store::now_ms() - 6 * 3_600_000,
            status["host"]["disk_available_bytes"].as_u64().unwrap() + 6 * 1024 * 1024 * 1024
        ],
    )
    .unwrap();
    let status = ok(&home, "resource.status", json!({}));
    assert_eq!(status["disk_trend"]["path"], key);
    // The newest reading is the one admission took, moments ago: close to the
    // live figure, since the machine keeps writing.
    let newest = status["disk_trend"]["to_free_bytes"].as_u64().unwrap();
    let live = status["host"]["disk_available_bytes"].as_u64().unwrap();
    assert!(newest.abs_diff(live) < 64 * 1024 * 1024, "{status}");
    assert!(
        Store::now_ms() - status["disk_trend"]["to_ms"].as_i64().unwrap() < 60_000,
        "{status}"
    );
    let forecast = &status["disk_forecast"];
    assert!(
        forecast["gib_per_hour"].as_f64().unwrap() > 0.9,
        "{forecast}"
    );
    assert!(
        forecast["hours_to_floor"].as_f64().unwrap() > 0.0,
        "{forecast}"
    );
}

#[test]
fn recent_estimates_space_out_heavy_jobs_until_their_ramp_passes() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    let other = fixture.home("other", 0);
    let host = ok(&home, "resource.status", json!({}))["host"].clone();
    let cpus = host["cpus"].as_f64().unwrap();
    // Size the limit and the estimate from this machine's own readings: CI
    // hosts can run far above one runnable task per CPU. The big estimate is
    // twice the margin, so it refuses a follow-up whatever the load does
    // within that margin.
    let big = (cpus * 50.0).min(1024.0);
    let margin = big / cpus / 2.0;
    let limit = host["load"].as_f64().unwrap() / cpus + margin;
    // No slot cap: only live load plus recent estimates decide.
    ok(
        &home,
        "resource.configure",
        json!({"max_heavy_jobs":null,"max_load_per_cpu":limit,"min_available_memory_bytes":0,
            "heavy_estimate":{"memory_bytes":0,"disk_bytes":0},"ramp_secs":2}),
    );
    let big = json!({"estimate":{"cpus":big}});
    let first = heavy_job(&home, "big-build", big);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    // Its load is not visible yet, but its estimate is counted, across homes.
    let refused = heavy_job(&other, "second-build", json!({}));
    assert!(!refused.status.success());
    let error = String::from_utf8_lossy(&refused.stderr);
    assert!(error.contains("CPU demand"), "{error}");
    // After the ramp the live reading is trusted; a sleeping job adds no load.
    thread::sleep(Duration::from_millis(2500));
    let admitted = heavy_job(&other, "second-build", json!({}));
    assert!(
        admitted.status.success(),
        "{}",
        String::from_utf8_lossy(&admitted.stderr)
    );
    ok(&home, "job.cancel", json!({"id":"big-build"}));
    ok(&other, "job.cancel", json!({"id":"second-build"}));
}

#[test]
fn a_lone_heavy_job_needs_only_the_floor_while_others_count_its_estimate() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    let other = fixture.home("other", 0);
    let available = ok(&home, "resource.status", json!({}))["host"]["memory_available_bytes"]
        .as_u64()
        .unwrap();
    // An estimate as large as everything available never fits beside the
    // floor, so only running alone can admit it.
    ok(
        &home,
        "resource.configure",
        json!({"max_heavy_jobs":null,"max_load_per_cpu":1000.0,
            "min_available_memory_bytes":available / 4,
            "heavy_estimate":{"memory_bytes":available,"disk_bytes":0},"ramp_secs":1}),
    );
    let first = heavy_job(&home, "first-build", json!({}));
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    // Cancel only a job whose payload is recorded: one caught while still
    // starting keeps its reservation, rightly, until it is recovered.
    wait(&home, "first-build", &["running"]);
    thread::sleep(Duration::from_millis(1500));
    let refused = heavy_job(&other, "second-build", json!({}));
    let error = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success());
    assert!(error.contains("must stay free"), "{error}");
    // Once the first job has stopped and released its reservation, the
    // second runs alone.
    ok(&home, "job.cancel", json!({"id":"first-build"}));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ok(&home, "resource.status", json!({}))["reservations"]
        .as_array()
        .unwrap()
        .is_empty()
    {
        assert!(
            Instant::now() < deadline,
            "the cancelled job kept its reservation"
        );
        thread::sleep(Duration::from_millis(100));
    }
    let alone = heavy_job(&other, "second-build", json!({}));
    assert!(
        alone.status.success(),
        "{}",
        String::from_utf8_lossy(&alone.stderr)
    );
    wait(&other, "second-build", &["running"]);
    ok(&other, "job.cancel", json!({"id":"second-build"}));
}

#[test]
fn memory_floor_and_charter_share_refuse_heavy_work_only() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    let other = fixture.home("other", 0);
    let total = ok(&home, "resource.status", json!({}))["host"]["memory_total_bytes"]
        .as_u64()
        .unwrap();
    ok(
        &home,
        "resource.configure",
        json!({"max_heavy_jobs":null,"min_available_memory_bytes":total * 2}),
    );
    let refused = heavy_job(&home, "needs-memory", json!({}));
    let error = String::from_utf8_lossy(&refused.stderr);
    assert!(error.contains("must stay free"), "{error}");
    // Light jobs are not subject to admission unless they opt in.
    start_light(&home, "light", &["true"]);
    let estimated = run(
        &home,
        "job.start",
        json!({"task_id":"task","id":"estimated","command":["true"],"heavy":false,"estimate":{"memory_bytes":1}}),
    );
    assert!(!estimated.status.success());
    // Each fixture charter allows itself one heavy job at a time.
    ok(
        &home,
        "resource.configure",
        json!({"min_available_memory_bytes":0,"max_load_per_cpu":1024,"ramp_secs":0}),
    );
    assert!(heavy_job(&home, "share-1", json!({})).status.success());
    let second = heavy_job(&home, "share-2", json!({}));
    let error = String::from_utf8_lossy(&second.stderr);
    assert!(error.contains("charter heavy-job share"), "{error}");
    assert!(heavy_job(&other, "elsewhere", json!({})).status.success());
    // The estimate is part of a job's identity.
    let changed = heavy_job(&home, "share-1", json!({"estimate":{"cpus":1}}));
    assert!(!changed.status.success());
    ok(&home, "job.cancel", json!({"id":"share-1"}));
    ok(&other, "job.cancel", json!({"id":"elsewhere"}));
}

fn start_light(home: &Path, id: &str, command: &[&str]) {
    ok(
        home,
        "job.start",
        json!({"task_id":"task","id":id,"command":command,"heavy":false}),
    );
    wait(home, id, &["succeeded"]);
}

#[test]
fn a_light_job_starts_while_a_heavy_job_waits_in_the_queue() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    // The fixture allows one heavy job; fill it and queue a second.
    start(&home, "running", &["sleep", "1"]);
    let queued = ok(
        &home,
        "job.start",
        json!({"task_id":"task","id":"heavy-wait","command":["true"],"wait_for_capacity":true}),
    );
    assert_eq!(queued["status"], "queued", "{queued}");
    // Light work needs no capacity, so it starts now instead of queueing.
    let light = ok(
        &home,
        "job.start",
        json!({"task_id":"task","id":"light-now","command":["true"],"heavy":false}),
    );
    assert_ne!(light["status"], "queued", "{light}");
    wait(&home, "light-now", &["succeeded"]);
    wait(&home, "heavy-wait", &["succeeded"]);
}

#[test]
fn waiting_jobs_start_in_arrival_order_and_others_cannot_jump_the_queue() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    let other = fixture.home("other", 0);
    assert_eq!(
        ok(&home, "resource.status", json!({}))["settings"]["heavy_estimate"]["memory_bytes"],
        0
    );
    // The fixture allows one heavy job on the host.
    start(&home, "running", &["sleep", "1"]);
    let marker = fixture.repo.join("order.txt");
    let append = |name: &str| {
        vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!("printf {name} >> '{}'", marker.display()),
        ]
    };
    let first = ok(
        &home,
        "job.start",
        json!({"task_id":"task","id":"first","command":append("A"),"wait_for_capacity":true}),
    );
    assert_eq!(first["status"], "queued", "{first}");
    assert!(
        first["reason"]
            .as_str()
            .unwrap()
            .contains("waiting for capacity")
    );
    let second = ok(
        &other,
        "job.start",
        json!({"task_id":"task","id":"second","command":append("B"),"wait_for_capacity":true}),
    );
    assert_eq!(second["status"], "queued");
    assert_eq!(
        ok(&home, "resource.status", json!({}))["queue"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    // A direct request that admission applies to may not jump the line. This
    // light job's estimate fits the load limits and no slot cap applies to it,
    // so only the waiting line refuses it.
    let jumped = run(
        &other,
        "job.start",
        json!({"task_id":"task","id":"jumper","command":["true"],"heavy":false,"estimate":{"cpus":0.1}}),
    );
    assert!(!jumped.status.success());
    let error = String::from_utf8_lossy(&jumped.stderr);
    assert!(error.contains("2 jobs are waiting for capacity"), "{error}");
    wait(&home, "running", &["succeeded"]);
    wait(&home, "first", &["succeeded"]);
    wait(&other, "second", &["succeeded"]);
    assert_eq!(fs::read_to_string(&marker).unwrap(), "AB");
    let status = ok(&home, "resource.status", json!({}));
    assert_eq!(status["queue"], json!([]));
    assert_eq!(status["heavy_reserved"], 0);
}

#[test]
fn waiting_jobs_end_on_cancel_timeout_or_a_vanished_supervisor() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    start(&home, "holder", &["sleep", "30"]);
    let queue = |id: &str, extra: Value| {
        let mut args =
            json!({"task_id":"task","id":id,"command":["true"],"wait_for_capacity":true});
        for (key, value) in extra.as_object().unwrap() {
            args[key] = value.clone();
        }
        ok(&home, "job.start", args)
    };
    queue("impatient", json!({"max_wait_secs":1}));
    let timed_out = wait(&home, "impatient", &["failed"]);
    assert!(
        timed_out["reason"]
            .as_str()
            .unwrap()
            .contains("no capacity within 1 s"),
        "{timed_out}"
    );
    queue("withdrawn", json!({}));
    ok(&home, "job.cancel", json!({"id":"withdrawn"}));
    let cancelled = wait(&home, "withdrawn", &["cancelled"]);
    assert_eq!(cancelled["reason"], "cancelled while waiting for capacity");
    queue("abandoned", json!({}));
    let until = Instant::now() + Duration::from_secs(5);
    let supervisor = loop {
        let job = ok(&home, "job.status", json!({"id":"abandoned"}));
        if let Some(pid) = job["supervisor"]["pid"].as_i64() {
            break pid as i32;
        }
        assert!(Instant::now() < until, "supervisor never recorded");
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(unsafe { libc::kill(supervisor, libc::SIGKILL) }, 0);
    let failed = fixture.settle(&home, "abandoned", &["failed"]);
    assert_eq!(
        failed["reason"],
        "supervisor disappeared while waiting for capacity"
    );
    assert_eq!(ok(&home, "resource.status", json!({}))["queue"], json!([]));
    assert!(
        !run(
            &home,
            "job.start",
            json!({"task_id":"task","id":"bad","command":["true"],"max_wait_secs":5})
        )
        .status
        .success()
    );
    ok(&home, "job.cancel", json!({"id":"holder"}));
}

/// A payload that starts `sleep` in a session of its own, as `xcodebuild`
/// does, records its pid, then waits `linger` seconds before exiting.
fn escaping_payload(linger: &str) -> Vec<String> {
    vec![
        support::python().display().to_string(),
        "-c".into(),
        format!(
            "import subprocess, sys, time\n\
             child = subprocess.Popen(['sleep', '60'], start_new_session=True)\n\
             open('escaped.pid', 'w').write(str(child.pid))\n\
             time.sleep({linger})"
        ),
    ]
}

fn escaped_pid(fixture: &Fixture) -> i32 {
    let path = fixture.repo.join("escaped.pid");
    let deadline = Instant::now() + Duration::from_secs(5);
    // The file can exist before the payload has written the pid into it.
    loop {
        if let Some(pid) = fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "the payload never started its child"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn gone(pid: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(8);
    while active_pid(pid) {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

#[test]
fn a_timed_out_job_also_ends_what_left_its_process_group() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    ok(
        &home,
        "job.start",
        json!({"task_id":"task","id":"build","timeout_secs":2,"heavy":true,
            "command": escaping_payload("60")}),
    );
    let child = escaped_pid(&fixture);
    let ended = wait(&home, "build", &["timed_out", "orphaned"]);
    assert_eq!(ended["status"], "timed_out", "{ended}");
    assert!(gone(child), "the escaped child outlived its timed-out job");
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
}

#[test]
fn a_finished_job_keeps_its_capacity_while_what_it_left_still_runs() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    // Asked to clean up after itself, it ends what it left and succeeds.
    ok(
        &home,
        "job.start",
        json!({"task_id":"task","id":"tidy","heavy":true,"cleanup_descendants":true,
            "command": escaping_payload("1.5")}),
    );
    let child = escaped_pid(&fixture);
    let tidy = wait(&home, "tidy", &["succeeded", "orphaned"]);
    assert_eq!(tidy["status"], "succeeded", "{tidy}");
    assert!(gone(child));
    fs::remove_file(fixture.repo.join("escaped.pid")).unwrap();

    // Otherwise the wrapper exits cleanly, but the build it started still
    // runs, so the job is orphaned and keeps its capacity.
    ok(
        &home,
        "job.start",
        json!({"task_id":"task","id":"build","heavy":true,"cleanup_descendants":false,
            "command": escaping_payload("1.5")}),
    );
    let child = escaped_pid(&fixture);
    let left = wait(&home, "build", &["orphaned", "succeeded"]);
    assert_eq!(left["status"], "orphaned", "{left}");
    assert!(active_pid(child));
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 1);
    // Cancelling it stops what it left and frees the capacity.
    ok(&home, "job.cancel", json!({"id":"build"}));
    wait(&home, "build", &["cancelled", "succeeded"]);
    assert!(gone(child));
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
}

/// Where a job's stdout log is, for the one incarnation this fixture ran.
fn stdout_log(home: &Path, id: &str) -> PathBuf {
    let job = ok(home, "job.status", json!({"id":id}));
    home.join("jobs")
        .join(id)
        .join(job["incarnation"].as_str().unwrap())
        .join("stdout.log")
}

#[test]
fn a_killed_job_keeps_the_lines_it_printed_before_the_kill() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    // A slow harness prints a line every 100 ms and is killed at 2 s, part way.
    let command = json!({"task_id":"task","id":"slow","heavy":true,"timeout_secs":2,
        "command":["/bin/sh","-c","head -c 400000 /dev/zero | tr '\\0' x | fold -w 1023; echo; i=0; while :; do i=$((i+1)); echo line-$i; sleep 0.1; done"]});
    ok(&home, "job.start", command);
    // Streamed lines reach the log while the harness is still running.
    let until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < until {
        thread::sleep(Duration::from_millis(50));
    }
    // The first lines come after 400 KB of filler, so they sit in the tail.
    let live = fs::read_to_string(stdout_log(&home, "slow")).unwrap();
    assert!(
        live.contains("line-1\n") && live.contains("line-3\n"),
        "{} bytes",
        live.len()
    );
    let done = wait(&home, "slow", &["timed_out"]);
    let text = fs::read_to_string(stdout_log(&home, "slow")).unwrap();
    assert!(text.len() <= 1024 * 1024, "{}", text.len());
    assert!(
        text.contains("line-1\n") && text.contains("line-3\n"),
        "{done}"
    );
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
}
