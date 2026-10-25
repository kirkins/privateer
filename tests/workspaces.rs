mod support;

use privateer::{dispatch, store::Store};
use rusqlite::params;
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    thread,
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");
struct Fixture {
    _temp: tempfile::TempDir,
    _data: tempfile::TempDir,
    root: PathBuf,
    /// XDG_DATA_HOME, kept short: macOS caps a socket path at 104 bytes,
    /// and its per-user temp dir alone takes about 60.
    data: PathBuf,
    repo: PathBuf,
    home: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let repo = root.join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "--quiet", "--initial-branch=main"]);
        fs::write(repo.join("file.txt"), "base\n").unwrap();
        fs::write(repo.join(".gitignore"), "/cache/\n/target/\n").unwrap();
        git(&repo, &["add", "."]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "base",
            ],
        );
        let script = root.join("treehouse");
        fs::write(&script, include_str!("fixtures/treehouse.py")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let home = root.join("home");
        let mut store = Store::open(&home).unwrap();
        fs::write(
            home.join("config.json"),
            json!({"resource_home":root.join("ledger"),"treehouse_binary":script}).to_string(),
        )
        .unwrap();
        dispatch(&mut store, "charter.put", json!({"id":"mission","objective":"isolated workspace fixture","repo":repo,"min_free_bytes":0,"job_timeout_secs":20})).unwrap();
        // Crew memory is admission's concern, tested apart: these crews run
        // side by side on whatever memory the test machine has.
        dispatch(
            &mut store,
            "resource.configure",
            json!({"min_available_memory_bytes":0,"crew_default_memory_bytes":1u64 << 20}),
        )
        .unwrap();
        dispatch(&mut store, "ship.put", json!({"name":"ship"})).unwrap();
        for task in ["one", "two"] {
            dispatch(
                &mut store,
                "task.create",
                json!({"id":task,"charter_id":"mission","owner":"ship","title":task}),
            )
            .unwrap();
        }
        let data_dir = tempfile::Builder::new()
            .prefix("pd")
            .tempdir_in("/tmp")
            .unwrap();
        let data = data_dir.path().canonicalize().unwrap();
        Self {
            _temp: temp,
            _data: data_dir,
            root,
            data,
            repo,
            home,
        }
    }
    fn command(&self, verb: &str, args: Value) -> Command {
        let mut c = support::command(BIN);
        c.args([
            "--home",
            self.home.to_str().unwrap(),
            verb,
            "--json",
            &args.to_string(),
        ])
        .env("HOME", &self.root)
        .env(
            "PATH",
            format!(
                "/usr/bin:/bin:{}",
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("PRIVATEER_SERVICE", "none")
        .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
        // Short, so the MCP broker's socket path fits macOS's limit.
        .env("XDG_DATA_HOME", &self.data);
        c
    }
    fn run(&self, verb: &str, args: Value) -> Output {
        self.command(verb, args).output().unwrap()
    }
    fn ok(&self, verb: &str, args: Value) -> Value {
        parse(self.run(verb, args))
    }
    fn acquire(&self, task: &str) -> Value {
        self.ok("workspace.acquire", json!({"task_id":task}))
    }
    fn release(&self, w: &Value, archive: bool) -> Value {
        self.ok(
            "workspace.release",
            json!({"task_id":w["task_id"],"lease_id":w["lease_id"],"archive":archive}),
        )
    }
    fn wait(&self, id: &str) -> Value {
        let until = Instant::now() + Duration::from_secs(12);
        loop {
            let j = self.ok("job.status", json!({"id":id}));
            if ["succeeded", "failed", "cancelled", "timed_out"]
                .contains(&j["status"].as_str().unwrap())
            {
                return j;
            }
            assert!(Instant::now() < until, "job did not settle: {j}");
            thread::sleep(Duration::from_millis(30));
        }
    }
    fn start(&self, task: &str, id: &str, body: &str) -> Value {
        self.ok(
            "job.start",
            json!({"task_id":task,"id":id,"command":["/bin/sh","-c",body],"heavy":false}),
        )
    }
}
fn parse(o: Output) -> Value {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    serde_json::from_slice(&o.stdout).unwrap()
}
fn git(repo: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8(o.stdout).unwrap().trim().into()
}
fn path(w: &Value) -> PathBuf {
    PathBuf::from(w["path"].as_str().unwrap())
}

#[test]
fn git_jobs_are_isolated_per_task_and_reuse_a_durable_checkout() {
    let f = Fixture::new();
    let a = f.start("one", "a", "printf one > file.txt");
    let b = f.start("two", "b", "printf two > file.txt");
    assert_eq!(f.wait("a")["status"], "succeeded");
    assert_eq!(f.wait("b")["status"], "succeeded");
    assert_ne!(a["cwd"], b["cwd"]);
    assert_eq!(
        fs::read_to_string(f.repo.join("file.txt")).unwrap(),
        "base\n"
    );
    assert_eq!(
        fs::read_to_string(Path::new(a["cwd"].as_str().unwrap()).join("file.txt")).unwrap(),
        "one"
    );
    let again = f.start("one", "again", "test \"$(cat file.txt)\" = one");
    assert_eq!(a["workspace_lease_id"], again["workspace_lease_id"]);
    assert_eq!(f.wait("again")["status"], "succeeded");
    assert_eq!(f.acquire("one")["state"], "ready");
}

#[test]
fn concurrent_acquisition_and_separate_homes_do_not_double_book() {
    let f = Fixture::new();
    let a = f
        .command("workspace.acquire", json!({"task_id":"one"}))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let b = f.acquire("one");
    let a = parse(a.wait_with_output().unwrap());
    assert_eq!(a["lease_id"], b["lease_id"]);
    let home = f.root.join("another-home");
    let mut other = Store::open(&home).unwrap();
    fs::copy(f.home.join("config.json"), home.join("config.json")).unwrap();
    dispatch(
        &mut other,
        "charter.put",
        json!({"id":"mission","objective":"other","repo":f.repo,"min_free_bytes":0}),
    )
    .unwrap();
    dispatch(
        &mut other,
        "task.create",
        json!({"id":"one","charter_id":"mission","owner":"ship","title":"one"}),
    )
    .unwrap();
    let o = dispatch(&mut other, "workspace.acquire", json!({"task_id":"one"})).unwrap();
    assert_ne!(o["path"], a["path"]);
    assert_ne!(o["pool"], a["pool"]);
}

#[test]
fn interrupted_allocation_recovers_the_existing_lease() {
    let f = Fixture::new();
    fs::write(f.root.join("fail-acquire-response"), "").unwrap();
    assert!(
        !f.run("workspace.acquire", json!({"task_id":"one"}))
            .status
            .success()
    );
    assert_eq!(
        f.ok("workspace.status", json!({"task_id":"one"}))["state"],
        "acquiring"
    );
    let w = f.acquire("one");
    let slots: Value = serde_json::from_slice(
        &fs::read(Path::new(w["pool"].as_str().unwrap()).join("fixture-state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(slots.as_array().unwrap().len(), 1);
    assert_eq!(w["lease_id"], slots[0]["lease_id"]);
}

#[test]
fn killing_the_caller_does_not_race_its_still_allocating_helper() {
    let f = Fixture::new();
    fs::write(f.root.join("pause-acquire-response"), "").unwrap();
    let mut child = f
        .command("workspace.acquire", json!({"task_id":"one"}))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(8);
    while !f.root.join("helper-acquired").exists() {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let w = f.acquire("one");
    let slots: Value = serde_json::from_slice(
        &fs::read(Path::new(w["pool"].as_str().unwrap()).join("fixture-state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(slots.as_array().unwrap().len(), 1);
}

#[test]
fn release_preserves_dirty_unlanded_and_archived_work() {
    let f = Fixture::new();
    let w = f.acquire("one");
    let p = path(&w);
    fs::write(p.join("file.txt"), "changed\n").unwrap();
    let release = |archive| json!({"task_id":"one","lease_id":w["lease_id"],"archive":archive});
    assert!(!f.run("workspace.release", release(true)).status.success());
    git(&p, &["add", "file.txt"]);
    git(
        &p,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "work",
        ],
    );
    assert!(!f.run("workspace.release", release(false)).status.success());
    let head = git(&p, &["rev-parse", "HEAD"]);
    let released = f.release(&w, true);
    assert_eq!(released["state"], "released");
    assert_eq!(
        git(
            &f.repo,
            &["rev-parse", released["archive_ref"].as_str().unwrap()]
        ),
        head
    );
    assert_eq!(f.release(&w, true)["state"], "released");
    let new = f.acquire("one");
    assert_ne!(w["lease_id"], new["lease_id"]);
    assert!(!f.run("workspace.release", release(true)).status.success());
    assert_eq!(f.acquire("one")["lease_id"], new["lease_id"]);
}

#[test]
fn clean_release_reuses_ignored_caches_and_recovers_an_interrupted_return() {
    let f = Fixture::new();
    let w = f.acquire("one");
    let p = path(&w);
    fs::create_dir(p.join("cache")).unwrap();
    fs::write(p.join("cache/build"), "warm").unwrap();
    fs::write(f.root.join("fail-return-response"), "").unwrap();
    assert!(
        !f.run(
            "workspace.release",
            json!({"task_id":"one","lease_id":w["lease_id"]})
        )
        .status
        .success()
    );
    assert_eq!(
        f.ok("workspace.status", json!({"task_id":"one"}))["state"],
        "releasing"
    );
    assert_eq!(f.release(&w, false)["state"], "released");
    let new = f.acquire("two");
    assert_eq!(new["path"], w["path"]);
    assert_eq!(fs::read_to_string(p.join("cache/build")).unwrap(), "warm");
}

#[test]
fn live_payloads_keep_the_workspace_after_the_supervisor_dies() {
    let f = Fixture::new();
    let j = f.start("one", "live", "sleep 20");
    let until = Instant::now() + Duration::from_secs(8);
    let running = loop {
        let j = f.ok("job.status", json!({"id":"live"}));
        if j["status"] == "running" {
            break j;
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(20));
    };
    unsafe {
        libc::kill(
            running["supervisor"]["pid"].as_i64().unwrap() as i32,
            libc::SIGKILL,
        );
    }
    let w = f.acquire("one");
    assert_eq!(w["lease_id"], j["workspace_lease_id"]);
    assert!(
        !f.run(
            "workspace.release",
            json!({"task_id":"one","lease_id":w["lease_id"]})
        )
        .status
        .success()
    );
    f.ok("job.cancel", json!({"id":"live"}));
    f.wait("live");
    assert_eq!(f.release(&w, false)["state"], "released");
}

#[test]
fn job_supervisor_does_not_inherit_another_workspaces_cwd() {
    let f = Fixture::new();
    let w1 = f.acquire("one");
    let p1 = path(&w1);
    let mut cmd = f.command(
        "job.start",
        json!({
            "task_id": "two",
            "id": "job-other",
            "command": ["/bin/sh", "-c", "sleep 10"],
            "heavy": false,
        }),
    );
    cmd.current_dir(&p1);
    let out = cmd.output().unwrap();
    let _j2 = parse(out);
    let until = Instant::now() + Duration::from_secs(8);
    let running = loop {
        let j = f.ok("job.status", json!({"id": "job-other"}));
        if j["status"] == "running" {
            break j;
        }
        assert!(Instant::now() < until, "job did not start: {j}");
        thread::sleep(Duration::from_millis(20));
    };
    let _ = &running;
    #[cfg(target_os = "linux")]
    {
        let supervisor_pid = running["supervisor"]["pid"].as_i64().unwrap();
        let supervisor_cwd = fs::read_link(format!("/proc/{supervisor_pid}/cwd")).unwrap();
        let p1_canonical = fs::canonicalize(&p1).unwrap_or_else(|_| p1.clone());
        assert!(
            !supervisor_cwd.starts_with(&p1) && !supervisor_cwd.starts_with(&p1_canonical),
            "supervisor cwd {:?} must not sit inside another workspace {:?}",
            supervisor_cwd,
            p1
        );
    }
    assert_eq!(f.release(&w1, false)["state"], "released");
    f.ok("job.cancel", json!({"id": "job-other"}));
    f.wait("job-other");
}

#[test]
fn task_custody_controls_workspace_access_and_list_visibility() {
    let f = Fixture::new();
    let w = f.acquire("one");
    let mut store = Store::open(&f.home).unwrap();
    dispatch(
        &mut store,
        "task.handoff",
        json!({"id":"one","actor":"ship","target":"other"}),
    )
    .unwrap();
    dispatch(
        &mut store,
        "task.accept",
        json!({"id":"one","actor":"other"}),
    )
    .unwrap();
    store.set_caller(privateer::store::Caller::Agent {
        actor: "ship".into(),
    });
    assert!(dispatch(&mut store, "workspace.acquire", json!({"task_id":"one"})).is_err());
    assert!(
        dispatch(
            &mut store,
            "workspace.release",
            json!({"task_id":"one","lease_id":w["lease_id"]})
        )
        .is_err()
    );
    assert_eq!(
        dispatch(&mut store, "workspace.list", json!({})).unwrap(),
        json!([])
    );
    store.set_caller(privateer::store::Caller::Agent {
        actor: "other".into(),
    });
    let same = dispatch(&mut store, "workspace.acquire", json!({"task_id":"one"})).unwrap();
    assert_eq!(same["lease_id"], w["lease_id"]);
    assert_eq!(same["owner"], "other");
}

#[test]
fn provider_plans_run_inside_the_workspace_and_cannot_escape_it() {
    let f = Fixture::new();
    let adapter = f.root.join("adapter.py");
    fs::write(&adapter,"import json,sys\nr=json.loads(sys.stdin.readline()); c=r['context']\nprint(json.dumps({'id':r['id'],'version':1,'ok':True,'result':{'argv':['/bin/sh','-c','pwd > agent-cwd'], 'cwd':c['repo'],'env_names':[]}}))\n").unwrap();
    let manifest = f.root.join("manifest.json");
    fs::write(
        &manifest,
        json!({"schema_version":1,"id":"fixture","executable":[support::python(),adapter]})
            .to_string(),
    )
    .unwrap();
    let a = f.ok(
        "agent.start",
        json!({"task_id":"one","id":"agent","manifest":manifest}),
    );
    assert_eq!(f.wait("agent")["status"], "succeeded");
    let cwd = Path::new(a["cwd"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(cwd.join("agent-cwd")).unwrap().trim(),
        cwd.to_str().unwrap()
    );
    assert!(!f.repo.join("agent-cwd").exists());
    let script = fs::read_to_string(&adapter)
        .unwrap()
        .replace("'cwd':c['repo']", "'cwd':c['source_repo']");
    fs::write(&adapter, script).unwrap();
    assert!(
        !f.run("agent.start", json!({"task_id":"two","manifest":manifest}))
            .status
            .success()
    );
}

#[test]
#[ignore = "requires an installed Treehouse with durable JSON leases"]
fn installed_treehouse_contract() {
    let f = Fixture::new();
    let binary = std::env::var("PRIVATEER_TEST_TREEHOUSE")
        .expect("set PRIVATEER_TEST_TREEHOUSE to an absolute Treehouse executable");
    fs::write(
        f.home.join("config.json"),
        json!({"resource_home":f.root.join("ledger"),"treehouse_binary":binary}).to_string(),
    )
    .unwrap();
    f.ok("charter.put", json!({"id":"mission","repo":f.root}));
    let w = f.ok("workspace.acquire", json!({"task_id":"one","cwd":f.repo}));
    assert_eq!(f.acquire("one")["lease_id"], w["lease_id"]);
    let j = f.start("one", "real", "test -f file.txt");
    assert_eq!(j["workspace_lease_id"], w["lease_id"]);
    assert_eq!(f.wait("real")["status"], "succeeded");
    assert_eq!(f.release(&w, false)["state"], "released");
    assert_ne!(
        f.ok("workspace.acquire", json!({"task_id":"two","cwd":f.repo}))["lease_id"],
        w["lease_id"]
    );
}

#[test]
fn terminal_tasks_can_recover_an_allocation_without_launching_new_work() {
    let f = Fixture::new();
    fs::write(f.root.join("fail-acquire-response"), "").unwrap();
    assert!(
        !f.run("workspace.acquire", json!({"task_id":"one"}))
            .status
            .success()
    );
    let intent = f.ok("workspace.status", json!({"task_id":"one"}));
    f.ok(
        "task.update",
        json!({"id":"one","actor":"ship","status":"done"}),
    );
    assert!(
        !f.run("workspace.acquire", json!({"task_id":"one"}))
            .status
            .success()
    );
    assert!(
        !f.run(
            "workspace.recover",
            json!({"task_id":"one","generation":"stale"})
        )
        .status
        .success()
    );
    let recovered = f.ok(
        "workspace.recover",
        json!({"task_id":"one","generation":intent["generation"]}),
    );
    assert_eq!(recovered["state"], "ready");
    assert_eq!(f.release(&recovered, false)["state"], "released");
}

#[test]
fn output_limits_stop_the_helper_group_and_keep_a_recoverable_intent() {
    let f = Fixture::new();
    fs::write(f.root.join("flood-helper"), "").unwrap();
    let started = Instant::now();
    assert!(
        !f.run("workspace.acquire", json!({"task_id":"one"}))
            .status
            .success()
    );
    assert!(started.elapsed() < Duration::from_secs(8));
    let pid = fs::read_to_string(f.root.join("flood-child")).unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        let out = Command::new("ps")
            .args(["-p", pid.trim(), "-o", "stat="])
            .output()
            .unwrap();
        let state = String::from_utf8(out.stdout).unwrap();
        if state.trim().is_empty() || state.trim().starts_with('Z') {
            break;
        }
        assert!(Instant::now() < until, "helper descendant survived");
        thread::sleep(Duration::from_millis(20));
    }
    fs::remove_file(f.root.join("flood-helper")).unwrap();
    let intent = f.ok("workspace.status", json!({"task_id":"one"}));
    assert_eq!(
        f.ok(
            "workspace.recover",
            json!({"task_id":"one","generation":intent["generation"]})
        )["state"],
        "released"
    );
}

#[test]
fn custody_changes_stop_old_workers_before_the_new_owner_reuses_the_checkout() {
    let f = Fixture::new();
    f.ok("job.start", json!({"task_id":"one","id":"owner","command":["/bin/sleep","20"],"heavy":false,"agent_actor":"ship"}));
    let until = Instant::now() + Duration::from_secs(8);
    while f.ok("job.status", json!({"id":"owner"}))["status"] != "running" {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(10));
    }
    let w = f.acquire("one");
    f.ok(
        "task.handoff",
        json!({"id":"one","actor":"ship","target":"other"}),
    );
    f.ok("task.accept", json!({"id":"one","actor":"other"}));
    let acquired = f.run("workspace.acquire", json!({"task_id":"one"}));
    if acquired.status.success() {
        assert_eq!(
            f.ok("job.status", json!({"id":"owner"}))["status"],
            "cancelled"
        );
    }
    assert_eq!(f.wait("owner")["status"], "cancelled");
    assert_eq!(f.acquire("one")["lease_id"], w["lease_id"]);
}

#[test]
fn queued_jobs_hold_their_own_workspace_until_cancelled() {
    let f = Fixture::new();
    f.ok("resource.configure", json!({"max_heavy_jobs":1,"max_load_per_cpu":1024,"min_available_memory_bytes":0,"ramp_secs":0,"heavy_estimate":{"memory_bytes":0,"disk_bytes":0}}));
    let first = f.ok(
        "job.start",
        json!({"task_id":"one","id":"heavy","command":["/bin/sleep","3"],"heavy":true}),
    );
    let queued = f.ok("job.start", json!({"task_id":"two","id":"queued","command":["/bin/sh","-c","true"],"heavy":true,"wait_for_capacity":true}));
    assert_eq!(queued["status"], "queued");
    assert_ne!(queued["cwd"], first["cwd"]);
    let w = f.acquire("two");
    assert!(
        !f.run(
            "workspace.release",
            json!({"task_id":"two","lease_id":w["lease_id"]})
        )
        .status
        .success()
    );
    f.ok("job.cancel", json!({"id":"queued"}));
    assert_eq!(f.wait("queued")["status"], "cancelled");
    // The queued supervisor records the cancellation and then exits; until it
    // has, release refuses for that reason alone.
    let until = Instant::now() + Duration::from_secs(12);
    let released = loop {
        let out = f.run(
            "workspace.release",
            json!({"task_id":"two","lease_id":w["lease_id"],"archive":false}),
        );
        if out.status.success() {
            break parse(out);
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("workspace supervisor is still running"),
            "{stderr}"
        );
        assert!(Instant::now() < until, "the queued supervisor never exited");
        thread::sleep(Duration::from_millis(30));
    };
    assert_eq!(released["state"], "released");
    f.wait("heavy");
}

#[test]
fn only_a_recorded_agent_wake_keeps_the_ship_repository() {
    let f = Fixture::new();
    let store = Store::open(&f.home).unwrap();
    store.connection().execute("INSERT INTO wakes(id,ship,focus,through_seq,job_id,mode,status,created_ms) VALUES('wake','ship','ship',0,'wake','fresh','running',0),('ordinary','ship','ship',0,'ordinary','fresh','running',0)", []).unwrap();
    let adapter = f.root.join("wake-adapter.py");
    fs::write(&adapter, "import json,sys\nr=json.loads(sys.stdin.readline())\nprint(json.dumps({'id':r['id'],'version':1,'ok':True,'result':{'argv':['/bin/sh','-c','true'],'cwd':r['context']['repo'],'env_names':[]}}))\n").unwrap();
    let manifest = f.root.join("wake-manifest.json");
    fs::write(
        &manifest,
        json!({"schema_version":1,"id":"wake-fixture","executable":[support::python(),adapter]})
            .to_string(),
    )
    .unwrap();
    let wake = f.ok(
        "agent.start",
        json!({"task_id":"one","id":"wake","manifest":manifest}),
    );
    assert_eq!(wake["cwd"], json!(f.repo));
    assert!(wake["workspace_lease_id"].is_null());
    let completed = f.wait("wake");
    assert_eq!(completed["status"], "succeeded", "{completed}");
    let ordinary = f.start("two", "ordinary", "true");
    assert!(ordinary["workspace_lease_id"].is_string());
    f.wait("ordinary");
}

#[test]
fn agent_started_builds_share_the_task_checkout_and_correct_pwd() {
    let f = Fixture::new();
    let worker = f.root.join("nested-worker.py");
    fs::write(&worker, r#"import json, os, pathlib, subprocess, sys, time
c = json.loads(sys.argv[1])
assert os.environ['PWD'] == os.getcwd()
def call(verb, args):
    return json.loads(subprocess.check_output(c['control']['cli_argv'] + [verb, '--json', json.dumps(args)]))
job = call('job.start', dict(id='nested', task_id=c['task_id'], heavy=False,
    command=['/bin/sh', '-c', 'test "$PWD" = "$(pwd)"; pwd > nested-cwd']))
for _ in range(100):
    job = call('job.status', dict(id='nested'))
    if job['status'] == 'succeeded': break
    time.sleep(.02)
assert job['status'] == 'succeeded', job
pathlib.Path('nested-record.json').write_text(json.dumps(job))
"#).unwrap();
    let adapter = f.root.join("nested-adapter.py");
    fs::write(&adapter, format!("import json,sys\nr=json.loads(sys.stdin.readline());c=r['context']\nprint(json.dumps(dict(id=r['id'],version=1,ok=True,result=dict(argv=[{},{},json.dumps(c)],cwd=c['repo'],env_names=[]))))\n", json!(support::python()), json!(worker))).unwrap();
    let manifest = f.root.join("nested-manifest.json");
    fs::write(
        &manifest,
        json!({"schema_version":1,"id":"nested","executable":[support::python(),adapter]})
            .to_string(),
    )
    .unwrap();
    let parent = f.ok(
        "agent.start",
        json!({"task_id":"one","id":"parent","manifest":manifest}),
    );
    assert_eq!(f.wait("parent")["status"], "succeeded");
    let cwd = Path::new(parent["cwd"].as_str().unwrap());
    let nested: Value =
        serde_json::from_slice(&fs::read(cwd.join("nested-record.json")).unwrap()).unwrap();
    assert_eq!(nested["cwd"], parent["cwd"]);
    assert_eq!(nested["workspace_lease_id"], parent["workspace_lease_id"]);
    assert_eq!(nested["agent_actor"], "ship");
    assert_eq!(
        fs::read_to_string(cwd.join("nested-cwd")).unwrap().trim(),
        cwd.to_str().unwrap()
    );
}

#[test]
fn an_external_lease_replacement_cannot_be_returned_by_a_stale_record() {
    let f = Fixture::new();
    let w = f.acquire("one");
    let state = Path::new(w["pool"].as_str().unwrap()).join("fixture-state.json");
    let mut slots: Value = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
    slots[0]["lease_id"] = json!("replacement");
    slots[0]["lease_holder"] = json!("other-holder");
    fs::write(&state, slots.to_string()).unwrap();
    assert!(
        !f.run(
            "workspace.release",
            json!({"task_id":"one","lease_id":w["lease_id"],"archive":true})
        )
        .status
        .success()
    );
    let preserved: Value = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
    assert_eq!(preserved[0]["lease_id"], "replacement");
    assert_eq!(
        fs::read_to_string(path(&w).join("file.txt")).unwrap(),
        "base\n"
    );
    assert!(
        !f.run("workspace.acquire", json!({"task_id":"one"}))
            .status
            .success()
    );
}

#[test]
fn a_charter_subdirectory_stays_scoped_inside_its_task_worktree() {
    let f = Fixture::new();
    fs::create_dir(f.repo.join("component")).unwrap();
    fs::write(f.repo.join("component/input"), "input").unwrap();
    git(&f.repo, &["add", "component"]);
    git(
        &f.repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "component",
        ],
    );
    f.ok(
        "charter.put",
        json!({"id":"mission","repo":f.repo.join("component")}),
    );
    let job = f.start("one", "subdir", "test -f input; pwd > output");
    assert_eq!(f.wait("subdir")["status"], "succeeded");
    let w = f.acquire("one");
    assert_eq!(job["cwd"], json!(path(&w).join("component")));
    assert!(!f.repo.join("component/output").exists());
    assert!(!f.run("job.start",json!({"task_id":"one","id":"escape","cwd":path(&w),"command":["/bin/sh","-c","true"],"heavy":false})).status.success());
}

#[test]
fn acquisition_uses_the_fresh_remote_default_even_from_a_feature_checkout() {
    let f = Fixture::new();
    let remote = f.root.join("remote.git");
    git(
        &f.root,
        &[
            "clone",
            "--quiet",
            "--bare",
            f.repo.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    git(
        &f.repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    let updater = f.root.join("updater");
    git(
        &f.root,
        &[
            "clone",
            "--quiet",
            remote.to_str().unwrap(),
            updater.to_str().unwrap(),
        ],
    );
    fs::write(updater.join("file.txt"), "fresh\n").unwrap();
    git(&updater, &["add", "file.txt"]);
    git(
        &updater,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fresh",
        ],
    );
    git(&updater, &["push", "--quiet", "origin", "main"]);
    git(&f.repo, &["checkout", "--quiet", "-b", "unrelated"]);
    let w = f.acquire("one");
    assert_eq!(w["base_branch"], "main");
    assert_eq!(
        fs::read_to_string(path(&w).join("file.txt")).unwrap(),
        "fresh\n"
    );
    assert_eq!(
        fs::read_to_string(f.repo.join("file.txt")).unwrap(),
        "base\n"
    );
    assert_eq!(git(&f.repo, &["branch", "--show-current"]), "unrelated");
}

#[test]
fn replay_after_release_does_not_acquire_a_new_workspace_and_retry_stays_pinned() {
    let f = Fixture::new();
    let first = f.start("one", "failed", "exit 1");
    assert_eq!(f.wait("failed")["status"], "failed");
    let old = f.acquire("one");
    f.release(&old, false);
    let replay = f.start("one", "failed", "exit 1");
    assert_eq!(replay["incarnation"], first["incarnation"]);
    assert_eq!(
        f.ok("workspace.status", json!({"task_id":"one"}))["state"],
        "released"
    );
    let new = f.acquire("one");
    assert_ne!(old["lease_id"], new["lease_id"]);
    assert!(
        !f.run("job.retry", json!({"id":"failed","new_id":"must-not-run"}))
            .status
            .success()
    );
    assert_eq!(f.acquire("one")["lease_id"], new["lease_id"]);
    assert!(
        f.ok("job.list", json!({}))["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|j| j["id"] != "must-not-run")
    );
}

#[test]
fn independent_harnesses_need_separate_tasks_but_can_run_in_parallel() {
    let f = Fixture::new();
    let adapter = f.root.join("parallel-adapter.py");
    fs::write(&adapter,"import json,sys\nr=json.loads(sys.stdin.readline())\nprint(json.dumps(dict(id=r['id'],version=1,ok=True,result=dict(argv=['/bin/sleep','2'],cwd=r['context']['repo'],env_names=[]))))\n").unwrap();
    let manifest = f.root.join("parallel-manifest.json");
    fs::write(
        &manifest,
        json!({"schema_version":1,"id":"parallel","executable":[support::python(),adapter]})
            .to_string(),
    )
    .unwrap();
    let one = f.ok(
        "agent.start",
        json!({"task_id":"one","id":"first","manifest":manifest}),
    );
    assert!(
        !f.run(
            "agent.start",
            json!({"task_id":"one","id":"duplicate","manifest":manifest})
        )
        .status
        .success()
    );
    let two = f.ok(
        "agent.start",
        json!({"task_id":"two","id":"second","manifest":manifest}),
    );
    assert_ne!(one["cwd"], two["cwd"]);
    assert_eq!(f.wait("first")["status"], "succeeded");
    assert_eq!(f.wait("second")["status"], "succeeded");
    let w = f.acquire("one");
    f.release(&w, false);
    let replay = f.ok(
        "agent.start",
        json!({"task_id":"one","id":"first","manifest":manifest}),
    );
    assert_eq!(replay["incarnation"], one["incarnation"]);
    assert_eq!(
        f.ok("workspace.status", json!({"task_id":"one"}))["state"],
        "released"
    );
    assert!(
        !f.run(
            "agent.start",
            json!({"task_id":"one","id":"first","manifest":manifest,"heavy":true})
        )
        .status
        .success()
    );
}

#[test]
fn disk_and_cwd_refusals_do_not_allocate_or_start_work() {
    let f = Fixture::new();
    f.ok(
        "charter.put",
        json!({"id":"mission","min_free_bytes":u64::MAX}),
    );
    assert!(
        !f.run(
            "job.start",
            json!({"task_id":"one","command":["/bin/sh","-c","true"],"heavy":false})
        )
        .status
        .success()
    );
    assert!(f.ok("workspace.status", json!({"task_id":"one"})).is_null());
    f.ok("charter.put", json!({"id":"mission","min_free_bytes":0}));
    assert!(
        !f.run(
            "workspace.acquire",
            json!({"task_id":"one","cwd":f.repo.join("file.txt")})
        )
        .status
        .success()
    );
    assert!(f.ok("workspace.status", json!({"task_id":"one"})).is_null());
    assert!(
        !f.run(
            "job.start",
            json!({"task_id":"one","cwd":f.root,"command":["/bin/sh","-c","true"],"heavy":false})
        )
        .status
        .success()
    );
    assert!(f.ok("workspace.status", json!({"task_id":"one"})).is_null());
    assert_eq!(f.ok("job.list", json!({}))["jobs"], json!([]));
}

#[test]
fn workspace_disk_floor_wait_uses_host_fifo_and_cleans_up_on_timeout() {
    let f = Fixture::new();
    let total = f.ok("resource.status", json!({}))["host"]["disk_total_bytes"]
        .as_u64()
        .unwrap();
    f.ok(
        "resource.configure",
        json!({"min_free_disk_bytes":total.saturating_mul(2)}),
    );
    let result = f.run(
        "workspace.acquire",
        json!({
            "task_id":"one", "wait_for_capacity":true, "max_wait_secs":1
        }),
    );
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("no workspace disk capacity"));
    assert_eq!(f.ok("resource.status", json!({}))["queue"], json!([]));
    assert!(f.ok("workspace.status", json!({"task_id":"one"})).is_null());
}

#[test]
fn runtime_needs_surfaces_long_workspace_disk_queue_entries() {
    let f = Fixture::new();
    f.ok("resource.status", json!({}));
    let ledger = rusqlite::Connection::open(f.root.join("ledger/resources.sqlite")).unwrap();
    let record = json!({
        "key":"workspace:fixture:one",
        "home":f.home,
        "job_id":"one",
        "incarnation":"fixture-incarnation",
        "created_ms":Store::now_ms() - 61_000,
        "supervisor":{"pid":123,"started":"fixture-start"},
        "kind":"workspace"
    });
    ledger
        .execute(
            "INSERT INTO queue(key,record) VALUES(?1,?2)",
            params![record["key"].as_str().unwrap(), record.to_string()],
        )
        .unwrap();
    let needs = dispatch(
        &mut Store::open(&f.home).unwrap(),
        "runtime.needs",
        json!({}),
    )
    .unwrap();
    let blocker = needs["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["source"] == "workspace")
        .unwrap();
    assert_eq!(blocker["evidence"], "workspace_disk_wait");
    assert_eq!(blocker["task_id"], "one");
    ledger
        .execute(
            "DELETE FROM queue WHERE key=?1",
            [record["key"].as_str().unwrap()],
        )
        .unwrap();
}

#[test]
fn repeated_release_attempts_never_overwrite_an_archived_snapshot() {
    let f = Fixture::new();
    let w = f.acquire("one");
    let p = path(&w);
    let commit = |text: &str| {
        fs::write(p.join("file.txt"), text).unwrap();
        git(&p, &["add", "file.txt"]);
        git(
            &p,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "snapshot",
            ],
        );
        git(&p, &["rev-parse", "HEAD"])
    };
    let first_head = commit("first");
    fs::write(f.root.join("fail-before-return"), "").unwrap();
    assert!(
        !f.run(
            "workspace.release",
            json!({"task_id":"one","lease_id":w["lease_id"],"archive":true})
        )
        .status
        .success()
    );
    let first_ref = f.ok("workspace.status", json!({"task_id":"one"}))["archive_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    git(&p, &["reset", "--hard", "main"]);
    let second_head = commit("second");
    let released = f.release(&w, true);
    assert_ne!(released["archive_ref"], json!(first_ref));
    assert_eq!(git(&f.repo, &["rev-parse", &first_ref]), first_head);
    assert_eq!(
        git(
            &f.repo,
            &["rev-parse", released["archive_ref"].as_str().unwrap()]
        ),
        second_head
    );
}

#[test]
fn projects_inside_a_non_git_ship_get_workspaces_and_nested_jobs_reuse_them() {
    let f = Fixture::new();
    f.ok("charter.put", json!({"id":"mission","repo":f.root}));
    let first = f.ok("job.start",json!({"task_id":"one","id":"project","cwd":f.repo,"command":["/bin/sh","-c","printf project > file.txt"],"heavy":false}));
    assert_eq!(f.wait("project")["status"], "succeeded");
    assert_ne!(first["cwd"], json!(f.repo));
    assert!(first["workspace_lease_id"].is_string());
    let nested = f.start(
        "one",
        "nested-project",
        "test \"$(cat file.txt)\" = project",
    );
    assert_eq!(f.wait("nested-project")["status"], "succeeded");
    assert_eq!(nested["cwd"], first["cwd"]);
    assert_eq!(nested["workspace_lease_id"], first["workspace_lease_id"]);
    assert_eq!(
        fs::read_to_string(f.repo.join("file.txt")).unwrap(),
        "base\n"
    );
    let w = f.acquire("one");
    assert_eq!(w["charter_repo"], json!(f.root));
    assert_eq!(w["repo"], json!(f.repo));
}

#[test]
fn a_project_selector_routes_agent_planning_but_cannot_escape_or_retarget() {
    let f = Fixture::new();
    f.ok("charter.put", json!({"id":"mission","repo":f.root}));
    let adapter = f.root.join("project-adapter.py");
    fs::write(&adapter,"import json,sys\nr=json.loads(sys.stdin.readline())\nprint(json.dumps(dict(id=r['id'],version=1,ok=True,result=dict(argv=['/bin/sh','-c','pwd > project-cwd'],cwd=r['context']['repo'],env_names=[]))))\n").unwrap();
    let manifest = f.root.join("project-manifest.json");
    fs::write(
        &manifest,
        json!({"schema_version":1,"id":"project","executable":[support::python(),adapter]})
            .to_string(),
    )
    .unwrap();
    let agent = f.ok(
        "agent.start",
        json!({"task_id":"one","id":"project-agent","cwd":f.repo,"manifest":manifest}),
    );
    assert_eq!(f.wait("project-agent")["status"], "succeeded");
    assert_ne!(agent["cwd"], json!(f.repo));
    let cwd = Path::new(agent["cwd"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(cwd.join("project-cwd")).unwrap().trim(),
        cwd.to_str().unwrap()
    );
    assert!(!f.repo.join("project-cwd").exists());
    let other = f.root.join("other-project");
    fs::create_dir(&other).unwrap();
    assert!(
        !f.run(
            "job.start",
            json!({"task_id":"one","cwd":other,"command":["/bin/sh","-c","true"],"heavy":false})
        )
        .status
        .success()
    );
    assert!(
        !f.run(
            "agent.start",
            json!({"task_id":"two","cwd":f.root.parent().unwrap(),"manifest":manifest})
        )
        .status
        .success()
    );
    assert!(f.ok("workspace.status", json!({"task_id":"two"})).is_null());
}

#[test]
fn crew_default_to_their_first_mates_harness_and_model() {
    let f = Fixture::new();
    let adapter = f.root.join("adapter.py");
    // The plan records the options it was given, inside the task checkout.
    fs::write(&adapter,"import json,sys\nr=json.loads(sys.stdin.readline()); c=r['context']\nprint(json.dumps({'id':r['id'],'version':1,'ok':True,'result':{'argv':['/bin/sh','-c','printf %s \"$0\" > crew-options',json.dumps(r['options'],sort_keys=True)], 'cwd':c['repo'],'env_names':[]}}))\n").unwrap();
    let manifest = f.root.join("manifest.json");
    fs::write(
        &manifest,
        json!({"schema_version":1,"id":"fixture","executable":[support::python(),adapter]})
            .to_string(),
    )
    .unwrap();
    f.ok(
        "task.create",
        json!({"id":"duty","charter_id":"mission","owner":"ship","title":"duty"}),
    );
    f.ok(
        "ship.put",
        json!({"name":"ship","first_mate":{"manifest":manifest,"duty_task":"duty","options":{"model":"chosen","provider":"route"}}}),
    );
    let crew = f.ok("agent.start", json!({"task_id":"one","id":"crew"}));
    assert_eq!(f.wait("crew")["status"], "succeeded");
    let cwd = Path::new(crew["cwd"].as_str().unwrap());
    assert_ne!(cwd, f.repo.as_path());
    assert_eq!(
        fs::read_to_string(cwd.join("crew-options")).unwrap(),
        r#"{"model": "chosen", "provider": "route"}"#
    );
    // Explicit options still win.
    let other = f.ok(
        "agent.start",
        json!({"task_id":"two","id":"other","options":{"model":"other"}}),
    );
    assert_eq!(f.wait("other")["status"], "succeeded");
    let cwd = Path::new(other["cwd"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(cwd.join("crew-options")).unwrap(),
        r#"{"model": "other"}"#
    );
}

#[test]
fn the_dispatcher_releases_only_workspaces_whose_work_has_landed() {
    let f = Fixture::new();
    f.ok(
        "task.create",
        json!({"id":"fresh","charter_id":"mission","owner":"ship","title":"fresh"}),
    );
    let commit = |dir: &Path, file: &str, message: &str| {
        fs::write(dir.join(file), "content\n").unwrap();
        git(dir, &["add", file]);
        git(
            dir,
            &[
                "-c",
                "user.name=F",
                "-c",
                "user.email=f@example.invalid",
                "commit",
                "--quiet",
                "-m",
                message,
            ],
        );
    };
    let landed = f.acquire("one");
    commit(&path(&landed), "landed.txt", "work");
    // Squash-merged: the same content lands on main as a different commit.
    commit(&f.repo, "landed.txt", "squash of work");
    let pending = f.acquire("two");
    commit(&path(&pending), "pending.txt", "wip");
    let fresh = f.acquire("fresh");

    let report = f.ok("serve", json!({"once": true}));
    let released: Vec<&str> = report["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["task_id"].as_str().unwrap())
        .collect();
    assert_eq!(released, ["one"], "{report}");
    let state = |task: &str| f.ok("workspace.status", json!({"task_id":task}))["state"].clone();
    assert_eq!(state("one"), "released");
    assert_eq!(state("two"), "ready");
    assert_eq!(state("fresh"), "ready");
    assert!(path(&pending).join("pending.txt").exists());
    assert!(path(&fresh).is_dir());
}

#[test]
fn a_workspace_needs_only_disk_and_does_not_wait_behind_heavy_jobs() {
    let f = Fixture::new();
    f.ok("resource.configure", json!({"max_heavy_jobs":1,"max_load_per_cpu":1024,"min_available_memory_bytes":0,"ramp_secs":0,"heavy_estimate":{"memory_bytes":0,"disk_bytes":0}}));
    f.ok(
        "job.start",
        json!({"task_id":"one","id":"building","command":["/bin/sleep","3"],"heavy":true}),
    );
    let waiting = f.ok("job.start", json!({"task_id":"one","id":"waiting","command":["/bin/sh","-c","true"],"heavy":true,"wait_for_capacity":true}));
    assert_eq!(waiting["status"], "queued");
    // Disk is fine, so another task's workspace comes at once, without
    // joining the queue the second build waits in.
    let w = f.acquire("two");
    assert!(w["lease_id"].is_string(), "{w}");
    // Below the floor it is refused unless it chooses to wait for disk.
    let total = f.ok("resource.status", json!({}))["host"]["disk_total_bytes"]
        .as_u64()
        .unwrap();
    f.release(&w, false);
    f.ok(
        "resource.configure",
        json!({"min_free_disk_bytes":total.saturating_mul(2)}),
    );
    let refused = f.run("workspace.acquire", json!({"task_id":"two"}));
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("workspace disk admission refused"));
    f.ok("resource.configure", json!({"min_free_disk_bytes":0}));
    f.wait("building");
    f.wait("waiting");
}

const MIB: u64 = 1024 * 1024;

fn add_task(f: &Fixture, id: &str) {
    f.ok(
        "task.create",
        json!({"id":id,"charter_id":"mission","owner":"ship","title":id}),
    );
}

/// A build's output in a slot, sized so the budget arithmetic is exact.
fn build_output(slot: &Path, bytes: usize) {
    fs::create_dir_all(slot.join("target")).unwrap();
    fs::write(slot.join("target/blob"), vec![0u8; bytes]).unwrap();
}

#[test]
fn release_past_budget_trims_the_least_recently_released_outputs() {
    let f = Fixture::new();
    add_task(&f, "three");
    add_task(&f, "four");
    let slots: Vec<Value> = ["one", "two", "three", "four"]
        .into_iter()
        .map(|task| f.acquire(task))
        .collect();
    for w in &slots {
        build_output(&path(w), MIB as usize);
    }
    // 1.5 MiB keeps one 1 MiB output and no more.
    f.ok(
        "resource.configure",
        json!({"workspace_cache_budget_bytes": MIB * 3 / 2}),
    );
    for w in &slots[..3] {
        f.release(w, false);
        // Release times order the trims; keep them distinct.
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!path(&slots[0]).join("target").exists());
    assert!(!path(&slots[1]).join("target").exists());
    assert!(path(&slots[2]).join("target/blob").exists());
    // A leased slot is never trimmed, however far the pool is over budget.
    assert!(path(&slots[3]).join("target/blob").exists());
    // Only build output goes: the source checkout stays whole.
    assert!(path(&slots[0]).join("file.txt").exists());
}

#[test]
fn a_short_disk_trims_released_output_before_it_waits() {
    let f = Fixture::new();
    let w = f.acquire("one");
    build_output(&path(&w), 64 * MIB as usize);
    f.release(&w, false);
    let total = f.ok("resource.status", json!({}))["host"]["disk_total_bytes"]
        .as_u64()
        .unwrap();
    // A floor beyond the whole filesystem, so the disk is short whatever
    // other writers do to free space: a floor near live free space lets
    // another test's freed tempdir lift the disk over it, and then nothing
    // is trimmed. The trim comes first, then the refusal for disk.
    f.ok(
        "resource.configure",
        json!({"min_free_disk_bytes": total.saturating_mul(2)}),
    );
    let refused = f.run("workspace.acquire", json!({"task_id":"two"}));
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("workspace disk admission refused"));
    assert!(!path(&w).join("target").exists());
    // Only build output goes: the source checkout stays whole.
    assert!(path(&w).join("file.txt").exists());
    // The trimmed slot is the one handed out once the disk allows it.
    f.ok("resource.configure", json!({"min_free_disk_bytes":0}));
    let again = f.ok("workspace.acquire", json!({"task_id":"two"}));
    assert_eq!(again["path"], w["path"]);
    assert!(!path(&again).join("target").exists());
}

#[test]
fn a_trim_error_does_not_fail_the_acquire() {
    let f = Fixture::new();
    add_task(&f, "three");
    let stuck = f.acquire("one");
    let spare = f.acquire("two");
    build_output(&path(&stuck), MIB as usize);
    build_output(&path(&spare), 16 * MIB as usize);
    // A directory in the released output that cannot be read: sizing it fails.
    let locked = path(&stuck).join("target/locked");
    fs::create_dir_all(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    f.release(&stuck, false);
    f.release(&spare, false);
    let total = f.ok("resource.status", json!({}))["host"]["disk_total_bytes"]
        .as_u64()
        .unwrap();
    // The floor asks for room the trim looks for. The trim skips the
    // unreadable slot and trims the spare; the call is then refused for disk.
    f.ok(
        "resource.configure",
        // Beyond the whole filesystem, so the disk is short however other
        // writers move free space.
        json!({"min_free_disk_bytes": total.saturating_mul(2)}),
    );
    let again = f.run("workspace.acquire", json!({"task_id":"three"}));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    // The trim went past the unreadable slot and removed the spare's output.
    assert!(!path(&spare).join("target").exists());
    assert_trim_did_not_fail(&again);
}

/// A trim error never fails the call. The call succeeds, or, where freed space
/// shows late (APFS frees lazily, and a shared runner writes meanwhile), it is
/// refused for disk like any short disk, never with the trim's own error.
fn assert_trim_did_not_fail(output: &Output) {
    if output.status.success() {
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_ne!(value["status"], "refused", "{value}");
        return;
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("disk admission refused"), "{text}");
    assert!(!text.contains("Permission denied"), "{text}");
}

#[test]
fn a_trim_error_does_not_fail_the_job_start() {
    let f = Fixture::new();
    add_task(&f, "three");
    let stuck = f.acquire("one");
    let spare = f.acquire("two");
    build_output(&path(&stuck), MIB as usize);
    build_output(&path(&spare), 16 * MIB as usize);
    let locked = path(&stuck).join("target/locked");
    fs::create_dir_all(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    f.release(&stuck, false);
    f.release(&spare, false);
    let total = f.ok("resource.status", json!({}))["host"]["disk_total_bytes"]
        .as_u64()
        .unwrap();
    // The job's declared space does not fit in what is free. The trim skips
    // the unreadable slot and trims the spare; the call is then refused for
    // disk.
    f.ok(
        "resource.configure",
        // With the job's 1 MiB this fills the filesystem exactly, so the job
        // could fit an empty disk, but free space never reaches it however
        // other writers move it: this fixture's own files hold more than 1 MiB.
        json!({"min_free_disk_bytes": total - MIB}),
    );
    let started = f.run(
        "job.start",
        json!({"task_id":"three","id":"builds","command":["/bin/true"],"heavy":true,"estimate":{"disk_bytes":MIB}}),
    );
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!path(&spare).join("target").exists());
    assert_trim_did_not_fail(&started);
    if started.status.success() {
        f.wait("builds");
    }
}

#[test]
fn trimming_never_touches_a_slot_with_untracked_work() {
    let f = Fixture::new();
    let kept = f.acquire("one");
    let trimmed = f.acquire("two");
    build_output(&path(&kept), MIB as usize);
    build_output(&path(&trimmed), MIB as usize);
    f.release(&kept, false);
    // Scratch work appears in the released slot; a trim must not touch it.
    fs::write(path(&kept).join("notes.txt"), "keep me").unwrap();
    f.ok(
        "resource.configure",
        json!({"workspace_cache_budget_bytes": 0}),
    );
    f.release(&trimmed, false);
    assert!(path(&kept).join("target/blob").exists());
    assert_eq!(
        fs::read_to_string(path(&kept).join("notes.txt")).unwrap(),
        "keep me"
    );
    assert!(!path(&trimmed).join("target").exists());
}

fn commit_work(dir: &Path, file: &str, message: &str) {
    fs::write(dir.join(file), "content\n").unwrap();
    git(dir, &["add", file]);
    git(
        dir,
        &[
            "-c",
            "user.name=F",
            "-c",
            "user.email=f@example.invalid",
            "commit",
            "--quiet",
            "-m",
            message,
        ],
    );
}

#[test]
fn a_crew_switches_branches_lands_work_and_slot_comes_back_within_one_sweep() {
    let f = Fixture::new();
    let w = f.acquire("one");
    let p = path(&w);

    git(&p, &["checkout", "-b", "fix/crew-feature"]);
    commit_work(&p, "feature.txt", "crew feature commit");
    commit_work(&f.repo, "feature.txt", "squash merge crew feature");

    let status = f.ok("workspace.status", json!({"task_id":"one"}));
    assert_eq!(status["switched_branch"], "fix/crew-feature");
    assert_eq!(status["current_branch"], "fix/crew-feature");

    f.ok(
        "task.update",
        json!({"id":"one","actor":"ship","status":"done"}),
    );

    let report = f.ok("serve", json!({"once": true}));
    let released: Vec<&str> = report["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["task_id"].as_str())
        .collect();
    assert!(released.contains(&"one"), "{report}");
    let final_status = f.ok("workspace.status", json!({"task_id":"one"}));
    assert_eq!(final_status["state"], "released");
    assert_eq!(final_status["switched_branch"], "fix/crew-feature");
}

#[test]
fn a_switched_branch_with_unlanded_commits_is_kept_and_alerts() {
    let f = Fixture::new();
    let w = f.acquire("one");
    let p = path(&w);

    git(&p, &["checkout", "-b", "fix/unlanded-feature"]);
    commit_work(&p, "unlanded.txt", "unlanded work");

    f.ok(
        "task.update",
        json!({"id":"one","actor":"ship","status":"done"}),
    );

    let report = f.ok("serve", json!({"once": true}));
    let released: Vec<&str> = report["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["task_id"].as_str())
        .collect();
    assert!(!released.contains(&"one"), "{report}");

    let status = f.ok("workspace.status", json!({"task_id":"one"}));
    assert_eq!(status["state"], "ready");
    assert_eq!(status["switched_branch"], "fix/unlanded-feature");
    assert!(
        status["retained_reason"]
            .as_str()
            .unwrap()
            .contains("not landed")
    );

    let inbox = f.ok("mail.inbox", json!({"address":"ship"}));
    let alerts: Vec<&Value> = inbox["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["payload"]["workspace_alert"] == "one")
        .collect();
    assert_eq!(alerts.len(), 1, "alert raised once: {inbox}");

    // Clear sweep metadata to simulate next tick's sweep
    let store = Store::open(&f.home).unwrap();
    store
        .connection()
        .execute("DELETE FROM metadata WHERE key='workspace_sweep_ms'", [])
        .unwrap();
    drop(store);

    f.ok("serve", json!({"once": true}));
    let inbox2 = f.ok("mail.inbox", json!({"address":"ship"}));
    let alerts2: Vec<&Value> = inbox2["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["payload"]["workspace_alert"] == "one")
        .collect();
    assert_eq!(alerts2.len(), 1, "alert not repeated every tick: {inbox2}");
}

#[test]
fn a_cancelled_tasks_owner_archives_and_releases_its_unlanded_workspace() {
    let f = Fixture::new();
    let w = f.acquire("one");
    let p = path(&w);
    commit_work(&p, "unlanded.txt", "unlanded work");
    let head = git(&p, &["rev-parse", "HEAD"]);
    let landed = f.acquire("two");
    let cancel = |task: &str| {
        f.ok(
            "task.update",
            json!({"id":task,"actor":"ship","status":"cancelled"}),
        )
    };
    cancel("one");
    // The dispatcher keeps the unlanded checkout and alerts its owner.
    f.ok("serve", json!({"once": true}));
    cancel("two");
    let alerts = |store: &Store| -> i64 {
        store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM workspace_alerts WHERE task_id='one'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    let mut store = Store::open(&f.home).unwrap();
    assert_eq!(alerts(&store), 1);

    let as_agent = |store: &mut Store, actor: &str| {
        store.set_caller(privateer::store::Caller::Agent {
            actor: actor.into(),
        })
    };
    as_agent(&mut store, "ship");
    let status = dispatch(&mut store, "workspace.status", json!({"task_id":"one"})).unwrap();
    assert_eq!(status["state"], "ready");
    assert_eq!(status["lease_id"], w["lease_id"]);

    let refused = dispatch(
        &mut store,
        "workspace.release",
        json!({"task_id":"one","lease_id":w["lease_id"]}),
    )
    .unwrap_err()
    .to_string();
    assert!(refused.contains("archive:true"), "{refused}");
    assert!(
        dispatch(
            &mut store,
            "workspace.release",
            json!({"task_id":"one","lease_id":"stale","archive":true}),
        )
        .is_err()
    );
    as_agent(&mut store, "other");
    assert!(
        dispatch(
            &mut store,
            "workspace.release",
            json!({"task_id":"one","lease_id":w["lease_id"],"archive":true}),
        )
        .is_err()
    );
    assert_eq!(alerts(&store), 1);

    as_agent(&mut store, "ship");
    let released = dispatch(
        &mut store,
        "workspace.release",
        json!({"task_id":"one","lease_id":w["lease_id"],"archive":true}),
    )
    .unwrap();
    assert_eq!(released["state"], "released");
    assert_eq!(
        git(
            &f.repo,
            &["rev-parse", released["archive_ref"].as_str().unwrap()]
        ),
        head
    );
    assert_eq!(alerts(&store), 0);

    // Nothing unlanded: a plain release of a cancelled task's checkout.
    let plain = dispatch(
        &mut store,
        "workspace.release",
        json!({"task_id":"two","lease_id":landed["lease_id"]}),
    )
    .unwrap();
    assert_eq!(plain["state"], "released");
}

#[test]
fn a_revoked_charter_keeps_a_terminal_tasks_agent_from_releasing() {
    let f = Fixture::new();
    let done = f.acquire("one");
    commit_work(&path(&done), "done.txt", "done work");
    let cancelled = f.acquire("two");
    commit_work(&path(&cancelled), "cancelled.txt", "cancelled work");
    f.ok(
        "task.update",
        json!({"id":"one","actor":"ship","status":"done"}),
    );
    f.ok(
        "task.update",
        json!({"id":"two","actor":"ship","status":"cancelled"}),
    );
    f.ok("serve", json!({"once": true}));
    let alerts = |store: &Store| -> i64 {
        store
            .connection()
            .query_row("SELECT COUNT(*) FROM workspace_alerts", [], |r| r.get(0))
            .unwrap()
    };
    let mut store = Store::open(&f.home).unwrap();
    assert_eq!(alerts(&store), 2);
    // The ship retired or moved: its old agent keeps no authority here.
    dispatch(&mut store, "charter.revoke", json!({"id":"mission"})).unwrap();

    store.set_caller(privateer::store::Caller::Agent {
        actor: "ship".into(),
    });
    for w in [&done, &cancelled] {
        let refused = dispatch(
            &mut store,
            "workspace.release",
            json!({"task_id":w["task_id"],"lease_id":w["lease_id"],"archive":true}),
        )
        .unwrap_err()
        .to_string();
        assert!(refused.contains("charter was revoked"), "{refused}");
    }
    assert_eq!(alerts(&store), 2);
    for w in [&done, &cancelled] {
        let held = dispatch(
            &mut store,
            "workspace.status",
            json!({"task_id":w["task_id"]}),
        )
        .unwrap();
        assert_eq!(held["state"], "ready", "{held}");
    }

    // The operator still returns what the revoked ship left behind.
    store.set_caller(privateer::store::Caller::Operator);
    for w in [&done, &cancelled] {
        let released = dispatch(
            &mut store,
            "workspace.release",
            json!({"task_id":w["task_id"],"lease_id":w["lease_id"],"archive":true}),
        )
        .unwrap();
        assert_eq!(released["state"], "released");
    }
    assert_eq!(alerts(&store), 0);
}

#[test]
fn a_cancelled_child_of_a_cancelled_parent_still_archives_its_workspace() {
    let f = Fixture::new();
    f.ok(
        "task.create",
        json!({"id":"child","parent":"one","charter_id":"mission","owner":"ship","title":"child"}),
    );
    let w = f.acquire("child");
    commit_work(&path(&w), "child.txt", "child work");
    for task in ["one", "child"] {
        f.ok(
            "task.update",
            json!({"id":task,"actor":"ship","status":"cancelled"}),
        );
    }
    let mut store = Store::open(&f.home).unwrap();
    store.set_caller(privateer::store::Caller::Agent {
        actor: "ship".into(),
    });
    let released = dispatch(
        &mut store,
        "workspace.release",
        json!({"task_id":"child","lease_id":w["lease_id"],"archive":true}),
    )
    .unwrap();
    assert_eq!(released["state"], "released");
    assert!(released["archive_ref"].is_string(), "{released}");
}

#[test]
fn a_stale_lease_moved_record_is_cleared() {
    let f = Fixture::new();
    let w = f.acquire("one");
    assert_eq!(w["state"], "ready");

    let w_path = path(&w);
    let pool = w_path.parent().unwrap().parent().unwrap();
    let state_file = pool.join("fixture-state.json");
    let mut slots: Vec<Value> = serde_json::from_slice(&fs::read(&state_file).unwrap()).unwrap();
    slots[0]["lease_holder"] = json!("privateer:other:gen999");
    slots[0]["lease_id"] = json!("moved-lease-id");
    fs::write(&state_file, serde_json::to_string(&slots).unwrap()).unwrap();

    f.ok("serve", json!({"once": true}));

    let status = f.ok("workspace.status", json!({"task_id":"one"}));
    assert_eq!(status["state"], "released");

    let list = f.ok("workspace.list", json!({}));
    let listed = list
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["task_id"] == "one")
        .expect("released records stay in the default listing");
    assert_eq!(listed["state"], "released");

    let active = f.ok("workspace.list", json!({"active":true}));
    assert!(
        !active
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["task_id"] == "one")
    );
}

#[test]
fn an_unlanded_task_branch_is_kept_after_the_crew_switches_and_lands_elsewhere() {
    let f = Fixture::new();
    let w = f.acquire("one");
    let p = path(&w);
    let task_branch = w["branch"].as_str().unwrap().to_owned();

    commit_work(&p, "task.txt", "task branch work");
    let task_tip = git(&p, &["rev-parse", &format!("refs/heads/{task_branch}")]);
    // The crew switches to a branch from base and lands it; the task branch
    // keeps its own unlanded commit.
    git(&p, &["checkout", "-b", "fix/other", "HEAD~1"]);
    commit_work(&p, "other.txt", "other work");
    commit_work(&f.repo, "other.txt", "squash merge other work");

    f.ok(
        "task.update",
        json!({"id":"one","actor":"ship","status":"done"}),
    );

    let report = f.ok("serve", json!({"once": true}));
    let released: Vec<&str> = report["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["task_id"].as_str())
        .collect();
    assert!(!released.contains(&"one"), "{report}");

    let status = f.ok("workspace.status", json!({"task_id":"one"}));
    assert_eq!(status["state"], "ready");
    assert_eq!(status["switched_branch"], "fix/other");
    let reason = status["retained_reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!("task branch {task_branch} changes are not landed")),
        "{reason}"
    );

    let inbox = f.ok("mail.inbox", json!({"address":"ship"}));
    let alerts = inbox["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["payload"]["workspace_alert"] == "one")
        .count();
    assert_eq!(alerts, 1, "{inbox}");

    let released = f.release(&w, true);
    assert_eq!(released["state"], "released");
    let archived = git(
        &f.repo,
        &[
            "for-each-ref",
            "refs/privateer/workspaces/",
            "--format=%(objectname)",
        ],
    );
    assert!(archived.lines().any(|sha| sha == task_tip), "{archived}");
}

#[test]
fn release_with_archive_true_archives_switched_branch_and_releases() {
    let f = Fixture::new();
    let w = f.acquire("one");
    let p = path(&w);

    git(&p, &["checkout", "-b", "fix/archive-branch"]);
    commit_work(&p, "archive.txt", "archived work");
    let head = git(&p, &["rev-parse", "HEAD"]);

    assert!(
        !f.run(
            "workspace.release",
            json!({"task_id":"one","lease_id":w["lease_id"],"archive":false})
        )
        .status
        .success()
    );

    let released = f.release(&w, true);
    assert_eq!(released["state"], "released");
    assert_eq!(released["switched_branch"], "fix/archive-branch");
    assert_eq!(
        git(
            &f.repo,
            &["rev-parse", released["archive_ref"].as_str().unwrap()]
        ),
        head
    );
    let task_tip = git(
        &f.repo,
        &[
            "rev-parse",
            &format!("refs/heads/{}", w["branch"].as_str().unwrap()),
        ],
    );
    assert_eq!(
        git(
            &f.repo,
            &["rev-parse", released["task_archive_ref"].as_str().unwrap()]
        ),
        task_tip
    );
}

#[test]
fn a_detached_crew_keeps_the_unlanded_task_branch_and_alerts() {
    let f = Fixture::new();
    let w = f.acquire("one");
    let p = path(&w);
    let task_branch = w["branch"].as_str().unwrap().to_owned();

    commit_work(&p, "task.txt", "task branch work");
    git(&p, &["checkout", "--quiet", "--detach", "main"]);

    f.ok(
        "task.update",
        json!({"id":"one","actor":"ship","status":"done"}),
    );

    let report = f.ok("serve", json!({"once": true}));
    let released: Vec<&str> = report["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["task_id"].as_str())
        .collect();
    assert!(!released.contains(&"one"), "{report}");

    let status = f.ok("workspace.status", json!({"task_id":"one"}));
    assert_eq!(status["state"], "ready");
    let reason = status["retained_reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!("task branch {task_branch} changes are not landed")),
        "{reason}"
    );

    let inbox = f.ok("mail.inbox", json!({"address":"ship"}));
    let alerts = inbox["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["payload"]["workspace_alert"] == "one")
        .count();
    assert_eq!(alerts, 1, "{inbox}");
}

// ---- MCP brokers close a workspace's windows on release --------------------

const BROKER: &str = env!("CARGO_BIN_EXE_privateer-mcp-broker");

fn broker_run_dir(f: &Fixture) -> PathBuf {
    f.data.join("privateer/mcp-broker/run")
}

/// Record windows instance `name` opened, as a broker that has since stopped
/// would have left them: (root, project) each.
fn record_windows(f: &Fixture, name: &str, windows: &[(&Path, &Path)]) {
    let run = broker_run_dir(f);
    fs::create_dir_all(&run).unwrap();
    fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).unwrap();
    let windows: Vec<Value> = windows
        .iter()
        .map(|(root, project)| json!({"root": root, "project": project, "target": null, "opened_ms": 1}))
        .collect();
    fs::write(
        run.join(format!("{name}.windows.json")),
        json!({"windows": windows}).to_string(),
    )
    .unwrap();
}

fn recorded(f: &Fixture, name: &str) -> Vec<Value> {
    fs::read(broker_run_dir(f).join(format!("{name}.windows.json")))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["windows"].as_array().cloned())
        .unwrap_or_default()
}

/// A real MCP broker instance in the fixture's run directory, routed by
/// the generic fake profile, its upstream the fake one and its window
/// helpers the fake ones. Stopped when dropped.
struct McpBroker {
    child: std::process::Child,
    socket: PathBuf,
    closed: PathBuf,
}

impl McpBroker {
    fn start(f: &Fixture, name: &str, close: &[&str]) -> McpBroker {
        McpBroker::serve(f, name, Some(close))
    }

    /// A plain instance: no routing, so no helper to close anything with.
    fn plain(f: &Fixture, name: &str) -> McpBroker {
        McpBroker::serve(f, name, None)
    }

    fn serve(f: &Fixture, name: &str, close: Option<&[&str]>) -> McpBroker {
        let run = broker_run_dir(f);
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp-broker");
        let python = fs::canonicalize(support::python())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let windows = f.root.join(format!("{name}-windows.json"));
        let closed = f.root.join(format!("{name}-closed.jsonl"));
        let helper = |action: &str, log: &Path, extra: &[&str]| {
            let mut argv = vec![
                python.clone(),
                "-I".into(),
                fixtures
                    .join("fake_window.py")
                    .to_string_lossy()
                    .into_owned(),
                action.into(),
                "--windows".into(),
                windows.to_string_lossy().into_owned(),
                "--log".into(),
                log.to_string_lossy().into_owned(),
            ];
            argv.extend(extra.iter().map(|s| s.to_string()));
            argv
        };
        let profile = fixtures.join("fake_profile.json");
        let mut routing: Value = serde_json::from_slice(&fs::read(&profile).unwrap()).unwrap();
        let mut routing = routing["routing"].take();
        routing["open"] = json!(helper(
            "open",
            &f.root.join(format!("{name}-opened.jsonl")),
            &[]
        ));
        let pool = fs::canonicalize(f.root.join("ledger/worktrees")).unwrap();
        let mut command = support::command(BROKER);
        command
            .args(["serve", "--state-dir"])
            .arg(&run)
            .args(["--name", name, "--label", "Fake", "--workspace-pool"])
            .arg(&pool);
        if let Some(close) = close {
            routing["close"] = json!(helper("close", &closed, close));
            command.arg("--routing").arg(routing.to_string());
        }
        let child = command
            .arg("--")
            .arg(&python)
            .arg("-I")
            .arg(fixtures.join("fake_mcpbridge.py"))
            .arg("--profile")
            .arg(&profile)
            .arg("--windows")
            .arg(&windows)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let socket = run.join(format!("{name}.sock"));
        let until = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            assert!(Instant::now() < until, "the broker did not start");
            thread::sleep(Duration::from_millis(30));
        }
        McpBroker {
            child,
            socket,
            closed,
        }
    }

    /// The projects the close helper was run for.
    fn closes(&self) -> Vec<String> {
        fs::read_to_string(&self.closed)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|r| r["argv"].as_array()?.last()?.as_str().map(str::to_owned))
            .collect()
    }
}

impl Drop for McpBroker {
    fn drop(&mut self) {
        // SAFETY: signalling the broker this test started.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.socket);
    }
}

fn audits(f: &Fixture, kind: &str) -> Vec<Value> {
    let store = Store::open(&f.home).unwrap();
    let mut statement = store
        .connection()
        .prepare("SELECT payload FROM audit WHERE kind=?1")
        .unwrap();
    statement
        .query_map([kind], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|p| serde_json::from_str(&p.unwrap()).unwrap())
        .collect()
}

#[test]
fn release_closes_the_window_before_its_checks() {
    let f = Fixture::new();
    let w = f.acquire("one");
    let root = fs::canonicalize(path(&w)).unwrap();
    let project = root.join("App.proj");
    // What a window leaves in the worktree while it is open: release's clean
    // check would refuse it, so the close must come first.
    let left = root.join("window.lock");
    fs::write(&left, "held by the window").unwrap();
    record_windows(&f, "fake", &[(&root, &project)]);
    let broker = McpBroker::start(&f, "fake", &["--remove", left.to_str().unwrap()]);
    let released = f.release(&w, false);
    assert_eq!(released["state"], "released");
    assert_eq!(broker.closes(), [project.to_string_lossy()]);
    assert!(!left.exists());
    assert!(recorded(&f, "fake").is_empty());
}

#[test]
fn a_failed_close_refuses_the_release_and_keeps_the_lease() {
    let f = Fixture::new();
    let w = f.acquire("one");
    let root = fs::canonicalize(path(&w)).unwrap();
    let project = root.join("App.proj");
    record_windows(&f, "fake", &[(&root, &project)]);
    let broker = McpBroker::start(&f, "fake", &["--fail"]);
    let refused = f.run(
        "workspace.release",
        json!({"task_id": "one", "lease_id": w["lease_id"]}),
    );
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("could not close the fake window for this workspace")
            && stderr.contains("could not close the Fake window of")
            && stderr.contains("-1708"),
        "{stderr}"
    );
    assert_eq!(broker.closes().len(), 1);
    let status = f.ok("workspace.status", json!({"task_id": "one"}));
    assert_eq!(status["state"], "ready");
    assert_eq!(status["lease_id"], w["lease_id"]);
    assert_eq!(recorded(&f, "fake").len(), 1);
}

#[test]
fn with_no_broker_release_proceeds_and_audits_the_unclosed_window() {
    let f = Fixture::new();
    let w = f.acquire("one");
    let root = fs::canonicalize(path(&w)).unwrap();
    let project = root.join("App.proj");
    record_windows(&f, "fake", &[(&root, &project)]);
    assert_eq!(f.release(&w, false)["state"], "released");
    let audited = audits(&f, "workspace.mcp_window_unclosed");
    assert_eq!(audited.len(), 1, "{audited:?}");
    assert_eq!(audited[0]["instance"], "fake");
    assert_eq!(audited[0]["root"], json!(root));
    assert_eq!(audited[0]["windows"][0]["project"], json!(project));
}

#[test]
fn a_running_instance_with_no_close_helper_never_holds_a_release() {
    // Reinstalled without the profile that recorded the window: the plain
    // instance answers with the window as unclosable, which is audited.
    let f = Fixture::new();
    let w = f.acquire("one");
    let root = fs::canonicalize(path(&w)).unwrap();
    let project = root.join("App.proj");
    record_windows(&f, "fake", &[(&root, &project)]);
    let broker = McpBroker::plain(&f, "fake");
    assert_eq!(f.release(&w, false)["state"], "released");
    assert!(broker.closes().is_empty());
    let audited = audits(&f, "workspace.mcp_window_unclosed");
    assert_eq!(audited.len(), 1, "{audited:?}");
    assert_eq!(audited[0]["instance"], "fake");
    assert_eq!(audited[0]["root"], json!(root));
    // The same entry shape a stopped instance's audit lists.
    assert_eq!(audited[0]["windows"][0]["project"], json!(project));
    assert_eq!(audited[0]["windows"][0]["root"], json!(root));
    assert_eq!(audited[0]["reason"], "the MCP broker has no close helper");
    // Dropped from the record: the slot's next release asks nobody.
    assert!(recorded(&f, "fake").is_empty());
}

#[test]
fn release_asks_every_instance_that_records_a_window_and_names_the_one_that_fails() {
    let f = Fixture::new();
    let w = f.acquire("one");
    let root = fs::canonicalize(path(&w)).unwrap();
    let (first, second) = (root.join("A.proj"), root.join("B.proj"));
    record_windows(&f, "fake", &[(&root, &first)]);
    record_windows(&f, "two", &[(&root, &second)]);
    // An instance that records nothing under the root is never asked.
    let other = f.acquire("two");
    let elsewhere = fs::canonicalize(path(&other)).unwrap();
    record_windows(&f, "spare", &[(&elsewhere, &elsewhere.join("C.proj"))]);
    {
        let fake = McpBroker::start(&f, "fake", &[]);
        let two = McpBroker::start(&f, "two", &["--fail"]);
        let spare = McpBroker::start(&f, "spare", &[]);
        let refused = f.run(
            "workspace.release",
            json!({"task_id": "one", "lease_id": w["lease_id"]}),
        );
        assert!(!refused.status.success());
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(
            stderr.contains("could not close the two window for this workspace")
                && !stderr.contains("the fake window"),
            "{stderr}"
        );
        // Both were asked; the one that worked closed its window.
        assert_eq!(fake.closes(), [first.to_string_lossy()]);
        assert_eq!(two.closes(), [second.to_string_lossy()]);
        assert!(spare.closes().is_empty());
        assert!(recorded(&f, "fake").is_empty());
        assert_eq!(recorded(&f, "two").len(), 1);
        assert_eq!(recorded(&f, "spare").len(), 1);
        let status = f.ok("workspace.status", json!({"task_id": "one"}));
        assert_eq!(status["state"], "ready");
    }
    // Both working: the release asks both, and goes ahead.
    record_windows(&f, "fake", &[(&root, &first)]);
    let fake = McpBroker::start(&f, "fake", &[]);
    let two = McpBroker::start(&f, "two", &[]);
    assert_eq!(f.release(&w, false)["state"], "released");
    assert_eq!(fake.closes().len(), 2);
    assert_eq!(two.closes().len(), 2);
    assert!(recorded(&f, "fake").is_empty() && recorded(&f, "two").is_empty());
}

#[test]
fn sweep_closes_the_window_before_it_releases() {
    let f = Fixture::new();
    let w = f.acquire("one");
    let p = path(&w);
    commit_work(&p, "feature.txt", "crew feature commit");
    commit_work(&f.repo, "feature.txt", "squash merge crew feature");
    f.ok(
        "task.update",
        json!({"id":"one","actor":"ship","status":"done"}),
    );
    let root = fs::canonicalize(&p).unwrap();
    let project = root.join("App.proj");
    record_windows(&f, "fake", &[(&root, &project)]);
    let broker = McpBroker::start(&f, "fake", &[]);
    let report = f.ok("serve", json!({"once": true}));
    let released: Vec<&str> = report["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["task_id"].as_str())
        .collect();
    assert!(released.contains(&"one"), "{report}");
    assert_eq!(broker.closes(), [project.to_string_lossy()]);
    assert!(recorded(&f, "fake").is_empty());
}

#[test]
fn sweep_leaves_the_window_of_a_checkout_another_task_now_holds() {
    let f = Fixture::new();
    let moved = f.acquire("one");
    let freed = f.acquire("two");
    let moved_root = fs::canonicalize(path(&moved)).unwrap();
    let freed_root = fs::canonicalize(path(&freed)).unwrap();
    let pool = path(&moved)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let state_file = pool.join("fixture-state.json");
    let mut slots: Vec<Value> = serde_json::from_slice(&fs::read(&state_file).unwrap()).unwrap();
    for slot in slots.iter_mut() {
        if slot["lease_id"] == moved["lease_id"] {
            // Another task's live lease on the same checkout.
            slot["lease_holder"] = json!("privateer:other:gen999");
            slot["lease_id"] = json!("moved-lease-id");
        } else if slot["lease_id"] == freed["lease_id"] {
            // Returned to the pool behind Privateer's back: nobody holds it.
            slot["status"] = json!("available");
            slot["lease_holder"] = json!("");
            slot["lease_id"] = json!("");
        }
    }
    fs::write(&state_file, serde_json::to_string(&slots).unwrap()).unwrap();
    let moved_project = moved_root.join("A.proj");
    let freed_project = freed_root.join("B.proj");
    record_windows(
        &f,
        "fake",
        &[(&moved_root, &moved_project), (&freed_root, &freed_project)],
    );
    let broker = McpBroker::start(&f, "fake", &[]);
    f.ok("serve", json!({"once": true}));
    for task in ["one", "two"] {
        let status = f.ok("workspace.status", json!({"task_id": task}));
        assert_eq!(status["state"], "released", "{task}: {status}");
    }
    // Only the free checkout's window closed; the new holder keeps its own.
    assert_eq!(broker.closes(), [freed_project.to_string_lossy()]);
    let left = recorded(&f, "fake");
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0]["root"], json!(moved_root));
}

/// A broker instance that holds its lock and socket, accepts every
/// connection and never answers. Stopped when dropped.
struct HungBroker(std::process::Child);

impl HungBroker {
    fn start(f: &Fixture, name: &str) -> HungBroker {
        let run = broker_run_dir(f);
        fs::create_dir_all(&run).unwrap();
        fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).unwrap();
        let script = r#"import fcntl, os, socket, sys
run, name = sys.argv[1], sys.argv[2]
lock = os.open(os.path.join(run, name + '.lock'), os.O_RDWR | os.O_CREAT, 0o600)
fcntl.flock(lock, fcntl.LOCK_EX)
listener = socket.socket(socket.AF_UNIX)
os.umask(0o177)
listener.bind(os.path.join(run, name + '.sock'))
listener.listen(16)
held = []
while True:
    held.append(listener.accept()[0])
"#;
        let child = support::command(support::python())
            .args(["-I", "-c", script])
            .arg(&run)
            .arg(name)
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let socket = run.join(format!("{name}.sock"));
        let until = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            assert!(Instant::now() < until, "the hung broker did not start");
            thread::sleep(Duration::from_millis(30));
        }
        HungBroker(child)
    }
}

impl Drop for HungBroker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_hung_broker_holds_up_only_the_release_of_a_workspace_with_a_window() {
    let f = Fixture::new();
    let quiet = f.acquire("one");
    let windowed = f.acquire("two");
    let root = fs::canonicalize(path(&windowed)).unwrap();
    let project = root.join("App.proj");
    record_windows(&f, "fake", &[(&root, &project)]);
    let _hung = HungBroker::start(&f, "fake");
    // No window recorded under it: the broker is never asked.
    let started = Instant::now();
    assert_eq!(f.release(&quiet, false)["state"], "released");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    // A recorded window: refused once the 35 s run out, and the lease kept.
    let started = Instant::now();
    let refused = f.run(
        "workspace.release",
        json!({"task_id": "two", "lease_id": windowed["lease_id"]}),
    );
    let took = started.elapsed();
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("could not close the fake window for this workspace")
            && stderr.contains("the MCP broker did not answer within 35 s"),
        "{stderr}"
    );
    assert!(
        took >= Duration::from_secs(34) && took < Duration::from_secs(50),
        "{took:?}"
    );
    let status = f.ok("workspace.status", json!({"task_id": "two"}));
    assert_eq!(status["state"], "ready");
    assert_eq!(status["lease_id"], windowed["lease_id"]);
    assert_eq!(recorded(&f, "fake").len(), 1);
}

#[test]
fn two_hung_instances_share_one_deadline() {
    let f = Fixture::new();
    let w = f.acquire("one");
    let root = fs::canonicalize(path(&w)).unwrap();
    record_windows(&f, "fake", &[(&root, &root.join("A.proj"))]);
    record_windows(&f, "two", &[(&root, &root.join("B.proj"))]);
    let _fake = HungBroker::start(&f, "fake");
    let _two = HungBroker::start(&f, "two");
    // What a release takes with nothing to ask, shown beside the one below.
    let quiet = f.acquire("two");
    let started = Instant::now();
    assert_eq!(f.release(&quiet, false)["state"], "released");
    let overhead = started.elapsed();
    let started = Instant::now();
    let refused = f.run(
        "workspace.release",
        json!({"task_id": "one", "lease_id": w["lease_id"]}),
    );
    let took = started.elapsed();
    eprintln!("two hung instances: the release took {took:?}; a quiet one {overhead:?}");
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("could not close the fake window for this workspace")
            && stderr.contains("could not close the two window for this workspace"),
        "{stderr}"
    );
    // Asked in parallel: 35 s plus 3, never 70.
    assert!(
        took >= Duration::from_secs(34) && took < Duration::from_secs(38),
        "{took:?}"
    );
    let status = f.ok("workspace.status", json!({"task_id": "one"}));
    assert_eq!(status["state"], "ready");
}
