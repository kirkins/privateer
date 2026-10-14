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
        // live load and memory admission is tested separately.
        privateer::dispatch(
            &mut store,
            "resource.configure",
            json!({"max_heavy_jobs":1,"max_load_per_cpu":1024,"min_available_memory_bytes":0,"ramp_secs":0}),
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

fn ok_env(home: &Path, command: &str, args: Value, env: &[(&str, &str)]) -> Value {
    let output = Command::new(BIN)
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
    let listed = ok_env(&home, "job.list", json!({}), &elsewhere);
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
    let orphaned = wait(&home, "legacy", &["orphaned"]);
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
    start(&home, "lingering", &["/bin/sh", "-c", "sleep 1 & exit 0"]);
    let orphan = wait(&home, "lingering", &["orphaned", "succeeded"]);
    assert_eq!(
        orphan["status"], "orphaned",
        "descendant should outlive leader"
    );
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 1);
    let settled = wait(&home, "lingering", &["succeeded"]);
    assert_eq!(settled["exit_code"], 0);
    assert_eq!(settled["reason"], Value::Null);
    assert_eq!(settled["leader_result"], Value::Null);
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
    // A failing leader settles to failure with its exit code.
    start(
        &home,
        "lingering-failure",
        &["/bin/sh", "-c", "sleep 1 & exit 3"],
    );
    let failed = wait(&home, "lingering-failure", &["failed"]);
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
    wait(&home, "live", &["orphaned"]);
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
    let job = wait(&home, "abandoned", &["failed"]);
    assert_eq!(
        job["reason"],
        "supervisor disappeared; payload ended without a recorded result"
    );
    assert_eq!(ok(&home, "resource.status", json!({}))["heavy_reserved"], 0);
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
    let output = Command::new(BIN)
        .args(["--home", home.to_str().unwrap(), "job.start", "--json"])
        .arg(json!({"task_id":"task","id":"foreign","command":["true"],"heavy":false}).to_string())
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "someone-else")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("tasks it owns"));
    let output = Command::new(BIN)
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
fn job_list_pages_and_still_repairs_a_leftover_reservation() {
    let fixture = Fixture::new();
    let home = fixture.home("mate", 0);
    for id in ["a", "b", "c"] {
        start(&home, id, &["true"]);
        wait(&home, id, &["succeeded"]);
    }
    let ids = |page: &Value| -> Vec<String> {
        page.as_array()
            .unwrap()
            .iter()
            .map(|j| j["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let first = ok(&home, "job.list", json!({"limit":2}));
    assert_eq!(ids(&first), ["a", "b"]);
    let rest = ok(&home, "job.list", json!({"limit":2,"after_id":"b"}));
    assert_eq!(ids(&rest), ["c"]);
    assert!(
        !run(&home, "job.list", json!({"limit":"all"}))
            .status
            .success()
    );
    // A finished job whose release was interrupted still holds capacity;
    // listing repairs it once its process group is provably gone.
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
    let configured = ok_env(
        &home,
        "resource.configure",
        json!({"max_load_per_cpu":0.7,"min_available_memory_bytes":4_294_967_296_u64}),
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
    let host = &status["host"];
    assert!(host["cpus"].as_f64().unwrap() >= 1.0);
    assert!(host["load"].as_f64().unwrap() >= 0.0);
    let available = host["memory_available_bytes"].as_u64().unwrap();
    assert!(available > 0 && available <= host["memory_total_bytes"].as_u64().unwrap());
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
            "heavy_estimate":{"memory_bytes":0},"ramp_secs":2}),
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
