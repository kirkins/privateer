mod support;

#[cfg(target_os = "linux")]
use privateer::{dispatch, service};
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};
use support::{BIN, Fleet, now_settings};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Installation {
    fleet: Fleet,
    binary: PathBuf,
}
impl Installation {
    fn new() -> Self {
        let fleet = Fleet::new(now_settings());
        let install = fleet.root.join("installation with spaces");
        fs::create_dir(&install).unwrap();
        let binary = install.join("privateer");
        fs::copy(BIN, &binary).unwrap();
        Self { fleet, binary }
    }
    fn command(&self) -> Command {
        let user = self.fleet.root.join("user");
        fs::create_dir_all(&user).unwrap();
        let mut command = support::command(&self.binary);
        command
            .arg("--home")
            .arg(&self.fleet.home)
            .env("HOME", user)
            .env("PRIVATEER_RESOURCE_HOME", self.fleet.root.join("ledger"))
            .env("PRIVATEER_FLEET", self.fleet.root.join("fleet.sqlite"))
            .env("PRIVATEER_SERVICE", "none")
            .env_remove("XDG_STATE_HOME");
        command
    }
    fn replace(&self) {
        let next = self.binary.with_extension("next");
        fs::copy(BIN, &next).unwrap();
        fs::rename(next, &self.binary).unwrap();
    }
}

fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(Instant::now() < deadline, "subprocess did not become ready");
        std::thread::sleep(Duration::from_millis(50));
    }
}

// A concurrent fork can briefly retain a writable descriptor for a fixture
// copy until its exec closes CLOEXEC descriptors. Retry only that transient
// ETXTBSY; permission and missing-installation failures remain observable.
fn retry_fixture_exec<T>(mut exec: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match exec() {
            Err(error)
                if error.raw_os_error() == Some(libc::ETXTBSY) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => return result,
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
fn replacing_the_installation_does_not_break_a_dispatcher_mail_round_trip() {
    let mut installation = Installation::new();
    // A resident dispatcher of this fixture: stopped with its group and any
    // detached payload when dropped, however the test ends.
    let mut dispatcher = retry_fixture_exec(|| {
        let mut command = installation.command();
        command
            .args(["serve", "--json", "{\"interval_secs\":1}"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        support::Resident::try_spawn(command, &installation.fleet.root)
    })
    .unwrap();
    until(|| {
        assert!(dispatcher.try_wait().unwrap().is_none());
        service::running(&installation.fleet.home).unwrap()
    });
    installation.replace();
    // This must be an already-running, unlinked image, not a fresh invocation.
    assert!(
        fs::read_link(format!("/proc/{}/exe", dispatcher.id()))
            .unwrap()
            .as_os_str()
            .as_encoded_bytes()
            .ends_with(b" (deleted)")
    );
    installation.fleet.send(json!({
        "id":"after-replacement", "from":"apps", "to":"infra",
        "kind":"request", "body":"Answer using the replacement installation"
    }));
    until(|| {
        let wakes = installation.fleet.wakes();
        let Some(wake) = wakes.first() else {
            return false;
        };
        assert_ne!(wake["status"], "failed", "{wake}");
        wake["status"] == "succeeded"
    });
    let answered = dispatch(
        &mut installation.fleet.store,
        "mail.show",
        json!({"id":"after-replacement"}),
    )
    .unwrap();
    assert_eq!(answered["state"], "answered");
    let inbox = dispatch(
        &mut installation.fleet.store,
        "mail.inbox",
        json!({"address":"apps"}),
    )
    .unwrap();
    let reply = &inbox["messages"][0];
    assert_eq!(reply["correlation"], "after-replacement");
    assert_eq!(reply["body"], "handled by infra");
}

struct Rpc {
    _process: Process,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Rpc {
    fn new(installation: &Installation) -> Self {
        let mut process = Process(
            retry_fixture_exec(|| {
                installation
                    .command()
                    .arg("rpc")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
            })
            .unwrap(),
        );
        let input = process.0.stdin.take().unwrap();
        let output = BufReader::new(process.0.stdout.take().unwrap());
        let mut rpc = Self {
            _process: process,
            input,
            output,
        };
        assert_eq!(rpc.call("status", json!({}))["ok"], true);
        rpc
    }
    fn call(&mut self, command: &str, args: Value) -> Value {
        writeln!(
            self.input,
            "{}",
            json!({"id":1,"command":command,"args":args})
        )
        .unwrap();
        self.input.flush().unwrap();
        let mut line = String::new();
        assert_ne!(self.output.read_line(&mut line).unwrap(), 0);
        serde_json::from_str(&line).unwrap()
    }
}

#[test]
#[cfg(target_os = "linux")]
fn removing_the_installation_reports_an_actionable_control_cli_error_but_can_supervise_jobs() {
    let installation = Installation::new();
    let mut rpc = Rpc::new(&installation);
    fs::remove_file(&installation.binary).unwrap();
    let brief = rpc.call("agent.brief", json!({"task_id":"infra-duty"}));
    assert_eq!(brief["ok"], false, "{brief}");
    let error = brief["error"].as_str().unwrap();
    assert!(error.contains("restart"), "{error}");
    assert!(
        error.contains(installation.binary.to_str().unwrap()),
        "{error}"
    );
    let started = rpc.call(
        "job.start",
        json!({"id":"retained-image", "task_id":"infra-duty", "heavy":false,
               "command":["/bin/sh","-c","printf retained-image > retained.txt"]}),
    );
    assert_eq!(started["ok"], true, "{started}");
    until(|| {
        let status = rpc.call("job.status", json!({"id":"retained-image"}));
        assert_eq!(status["ok"], true, "{status}");
        assert_ne!(status["result"]["status"], "failed", "{status}");
        status["result"]["status"] == "succeeded"
    });
    assert_eq!(
        fs::read_to_string(installation.fleet.root.join("repo/retained.txt")).unwrap(),
        "retained-image"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn a_replaced_rpc_process_emits_an_executable_control_cli_not_a_proc_self_alias() {
    let installation = Installation::new();
    // The old image can still have links elsewhere: link count alone cannot
    // distinguish a deleted executable path from a literal filename suffix.
    fs::hard_link(
        &installation.binary,
        installation.fleet.root.join("old-image"),
    )
    .unwrap();
    let mut rpc = Rpc::new(&installation);
    installation.replace();
    let brief = rpc.call("agent.brief", json!({"task_id":"infra-duty"}));
    assert_eq!(brief["ok"], true, "{brief}");
    let argv = brief["result"]["context"]["control"]["cli_argv"]
        .as_array()
        .unwrap();
    assert_eq!(argv[0], installation.binary.to_str().unwrap());
    let output = retry_fixture_exec(|| {
        installation
            .command()
            .arg("status")
            .arg("--json")
            .arg("{}")
            .output()
    })
    .unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[test]
#[cfg(target_os = "linux")]
fn a_literal_deleted_suffix_is_preserved_before_and_after_replacement() {
    let mut installation = Installation::new();
    let literal = installation.binary.with_file_name("privateer (deleted)");
    fs::rename(&installation.binary, &literal).unwrap();
    installation.binary = literal;
    let mut rpc = Rpc::new(&installation);
    for replaced in [false, true] {
        if replaced {
            installation.replace();
        }
        let brief = rpc.call("agent.brief", json!({"task_id":"infra-duty"}));
        assert_eq!(brief["ok"], true, "{brief}");
        assert_eq!(
            brief["result"]["context"]["control"]["cli_argv"][0],
            installation.binary.to_str().unwrap()
        );
    }
}

#[test]
fn a_nonexecutable_replacement_is_rejected_instead_of_advertised_to_agents() {
    let installation = Installation::new();
    let mut rpc = Rpc::new(&installation);
    let uid = unsafe { libc::geteuid() };
    for (mode, executable) in [
        (0o644, false),
        (0o641, uid == 0),
        (0o654, uid == 0),
        (0o744, true),
    ] {
        let next = installation.binary.with_extension("next");
        fs::copy(BIN, &next).unwrap();
        fs::set_permissions(&next, fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(fs::metadata(&next).unwrap().uid(), uid);
        fs::rename(next, &installation.binary).unwrap();
        let execution = retry_fixture_exec(|| {
            installation
                .command()
                .args(["status", "--json", "{}"])
                .output()
        });
        assert_eq!(
            execution.as_ref().is_ok_and(|out| out.status.success()),
            executable,
            "mode {mode:o}: {execution:?}"
        );
        let brief = rpc.call("agent.brief", json!({"task_id":"infra-duty"}));
        assert_eq!(brief["ok"], executable, "mode {mode:o}: {brief}");
        if executable {
            assert_eq!(
                brief["result"]["context"]["control"]["cli_argv"][0],
                installation.binary.to_str().unwrap()
            );
        } else {
            let error = brief["error"].as_str().unwrap();
            assert!(error.contains("restart"), "{error}");
            assert!(
                error.contains(installation.binary.to_str().unwrap()),
                "{error}"
            );
        }
    }
}

#[test]
fn an_unavailable_supervisor_does_not_strand_jobs_or_capacity() {
    let installation = Installation::new();
    let mut rpc = Rpc::new(&installation);
    assert_eq!(
        rpc.call(
            "resource.configure",
            // These payloads are an echo that must never run: they are not
            // heavy work, so they claim no memory or disk appetite.
            json!({"max_heavy_jobs":1,"max_load_per_cpu":1024,
                   "min_available_memory_bytes":0,"ramp_secs":0,
                   "heavy_estimate":{"memory_bytes":0,"disk_bytes":0}})
        )["ok"],
        true
    );
    #[cfg(not(target_os = "linux"))]
    installation.replace();
    fs::set_permissions(&installation.binary, fs::Permissions::from_mode(0o644)).unwrap();
    for heavy in [false, true] {
        for queued in [false, true] {
            let id = format!("unspawned-{heavy}-{queued}");
            let response = rpc.call(
                "job.start",
                json!({"id":id,"task_id":"infra-duty","heavy":heavy,
                       "command":["/bin/echo","should-not-run"],
                       "wait_for_capacity":queued,
                       "estimate":{"cpus":1,"memory_bytes":if queued {u64::MAX} else {0}}}),
            );
            assert_eq!(response["ok"], false, "{response}");
            let error = response["error"].as_str().unwrap();
            #[cfg(not(target_os = "linux"))]
            assert!(error.contains("restart"), "{error}");
            assert!(
                error.contains("supervisor") || error.contains("not executable"),
                "{error}"
            );
            #[cfg(target_os = "linux")]
            {
                let retry = rpc.call("job.retry", json!({"id":id,"new_id":format!("retry-{id}")}));
                assert_eq!(retry["ok"], false, "{retry}");
            }
        }
    }
    let conn = installation.fleet.store.connection();
    let jobs: Vec<String> = conn
        .prepare("SELECT record FROM jobs")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    #[cfg(target_os = "linux")]
    {
        assert_eq!(jobs.len(), 8);
        for raw in jobs {
            let job: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(job["status"], "failed", "{job}");
            assert!(job["finished_ms"].is_number(), "{job}");
        }
    }
    #[cfg(not(target_os = "linux"))]
    assert!(jobs.is_empty(), "{jobs:?}");
    let ledger =
        rusqlite::Connection::open(installation.fleet.root.join("ledger/resources.sqlite"))
            .unwrap();
    let held: u64 = ledger
        .query_row(
            "SELECT (SELECT COUNT(*) FROM reservations) + (SELECT COUNT(*) FROM queue)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(held, 0);
    fs::set_permissions(&installation.binary, fs::Permissions::from_mode(0o755)).unwrap();
    let args = json!({"id":"restored-supervisor","task_id":"infra-duty","heavy":true,
                     "command":["/bin/echo","restored"],"estimate":{"cpus":1,"memory_bytes":0}});
    let started = rpc.call("job.start", args.clone());
    assert_eq!(started["ok"], true, "{started}");
    let repeated = rpc.call("job.start", args);
    assert_eq!(repeated["ok"], true, "{repeated}");
    assert_eq!(
        repeated["result"]["incarnation"],
        started["result"]["incarnation"]
    );
    until(|| {
        let status = rpc.call("job.status", json!({"id":"restored-supervisor"}));
        assert_eq!(status["ok"], true, "{status}");
        assert_ne!(status["result"]["status"], "failed", "{status}");
        status["result"]["status"] == "succeeded"
    });
    assert_eq!(
        rpc.call("resource.status", json!({}))["result"]["heavy_reserved"],
        0
    );
}
