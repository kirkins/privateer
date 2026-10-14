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
        let store = Store::open(&home).unwrap();
        fs::write(
            home.join("config.json"),
            serde_json::to_vec(&json!({"resource_home":self.ledger,"max_heavy_jobs":1})).unwrap(),
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
}
fn run(home: &Path, command: &str, args: Value) -> std::process::Output {
    Command::new(BIN)
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
    let orphan = ok(&a, "job.status", json!({"id":"hold"}));
    assert_eq!(orphan["status"], "orphaned");
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
        ok(&home, "job.list", json!({})).as_array().unwrap().len(),
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
    assert_eq!(ok(&normal, "job.list", json!({})), json!([]));
    assert_eq!(
        ok(&normal, "resource.status", json!({}))["heavy_reserved"],
        0
    );
}

#[test]
fn output_is_drained_but_saved_logs_are_bounded() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    start(
        &home,
        "noisy",
        &[
            "/bin/sh",
            "-c",
            "dd if=/dev/zero bs=1048576 count=3 2>/dev/null; dd if=/dev/zero bs=1048576 count=3 >&2 2>/dev/null",
        ],
    );
    let done = wait(&home, "noisy", &["succeeded"]);
    let logs = home
        .join("jobs/noisy")
        .join(done["incarnation"].as_str().unwrap());
    assert_eq!(
        fs::metadata(logs.join("stdout.log")).unwrap().len(),
        1024 * 1024
    );
    assert_eq!(
        fs::metadata(logs.join("stderr.log")).unwrap().len(),
        1024 * 1024
    );
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
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
        ok(&home, "job.list", json!({})).as_array().unwrap().len(),
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
        ok(&home, "job.list", json!({})).as_array().unwrap().len(),
        2
    );
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
}

#[test]
fn ordinary_leader_exit_preserves_and_can_cancel_recorded_descendants() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    start(
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
    let output = Command::new(BIN).arg("--home").arg(&home).arg("job.start").arg("--json")
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
    assert!(diagnostic.contains("process inspection failed"));
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
fn opted_in_completion_cleanup_stops_owned_helpers_and_preserves_success() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    ok(
        &home,
        "job.start",
        json!({"task_id":"task","id":"clean-completion","cleanup_descendants":true,
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
