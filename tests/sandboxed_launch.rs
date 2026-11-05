//! Qualification for #736 (docs/design/codex-sandboxed-launch.md, (f)): a
//! first mate whose CLI calls run inside a Codex-like sandbox (bwrap with its
//! own PID namespace on Linux, Seatbelt with Codex's base rules on macOS)
//! starts a real heavy job, crew and errand through the home's dispatcher;
//! each runs outside the sandbox, reports its outcome and is cancelled from
//! inside. The harness is a script: no login, provider or model.
mod support;

use serde_json::{Value, json};
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};
use support::{Fleet, Resident, now_settings};

/// The first mate's harness. Its own process runs outside the sandbox, as
/// Codex's app does; every Privateer call it makes runs inside `wrap.json`'s
/// sandbox, as the model's shell commands do.
const HARNESS: &str = r#"import json, os, pathlib, re, signal, subprocess, sys, time
cli, ship, prompt = json.loads(sys.argv[1]), sys.argv[2], sys.argv[3]
root = pathlib.Path(sys.argv[4]).parent
wrap = json.loads((root / "wrap.json").read_text())
def run(command, args, prefix=()):
    return subprocess.run(list(prefix) + wrap + cli + [command, "--json", json.dumps(args)],
                          capture_output=True, text=True)
def call(command, args):
    out = run(command, args)
    if out.returncode:
        (root / "harness-error").write_text("%s: %s" % (command, out.stderr))
        sys.exit(3)
    return json.loads(out.stdout)
def mode(name, default):
    path = root / name
    return path.read_text().strip() if path.exists() else default
def done():
    print(json.dumps({"type": "turn.completed", "usage": {"input_tokens": 10, "cached_input_tokens": 0, "output_tokens": 5}}), flush=True)
    sys.exit(0)
errand = re.search(r"^Errand (errand-\S+)", prompt, re.M)
if errand:
    if mode("errand-mode", "quick") == "sleep":
        time.sleep(120)
    print(json.dumps({"type": "item.completed", "item": {"type": "agent_message",
        "text": "Reviewed %s inside the sandbox." % errand.group(1)}}), flush=True)
    done()
through = re.search(r"through seq (\d+)", prompt)
if through:
    # A mail wake: acknowledge and finish.
    call("mail.ack", {"address": ship, "through_seq": int(through.group(1))})
    done()
crew = os.environ["PRIVATEER_JOB"]
task = call("job.status", {"id": crew})["task_id"]
kind = mode("crew-mode", "park")
if kind == "sleep":
    time.sleep(120)
    done()
if kind == "drop":
    # A crew that drops its role inside the sandbox.
    out = run("job.start", {"task_id": task, "command": ["true"]},
              prefix=["env", "-u", "PRIVATEER_ROLE", "-u", "PRIVATEER_ACTOR"])
    (root / "drop.txt").write_text("%d\n%s" % (out.returncode, out.stderr))
    done()
if kind == "forge":
    # A crew that claims the operator: no role, no actor, no PRIVATEER_JOB.
    env = {k: v for k, v in os.environ.items()
           if k not in ("PRIVATEER_ROLE", "PRIVATEER_ACTOR", "PRIVATEER_JOB")}
    def forge(job, err=None):
        args = {"task_id": task, "id": job, "command": ["touch", str(root / ("ran-" + job))]}
        argv = wrap + cli + ["job.start", "--json", json.dumps(args)]
        if err is None:
            return subprocess.Popen(argv, env=env, stdout=subprocess.PIPE,
                                    stderr=subprocess.PIPE, text=True)
        # Detached: its parent exits at once, so launchd adopts it and its
        # ancestry no longer reaches this crew's supervisor. Without that, a
        # Seatbelt sandbox, which hides no ancestry, lets the CLI's own walk
        # refuse it before it records any request.
        pid = os.fork()
        if pid == 0:
            if os.fork() == 0:
                try:
                    fd = os.open(str(err), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
                    null = os.open(os.devnull, os.O_RDWR)
                    os.dup2(null, 0)
                    os.dup2(fd, 1)
                    os.dup2(fd, 2)
                    os.execvpe(argv[0], argv, env)
                finally:
                    os._exit(127)
            os._exit(0)
        os.waitpid(pid, 0)
        return None
    live = forge("forged-live")
    _, err = live.communicate(timeout=60)
    (root / "forge-live.txt").write_text("%d\n%s" % (live.returncode, err))
    gaps = root / "gaps"
    if sys.platform == "darwin":
        # Detached and left alone: launchd adopts the sandboxed requester,
        # whose chain then never leaves its sandbox. The dispatcher refuses
        # it before resolving its claim.
        orphan = gaps / "forged-orphan.stderr"
        forge("forged-orphan", orphan)
        deadline = time.time() + 60
        while not (orphan.exists() and orphan.read_text()) and time.time() < deadline:
            time.sleep(0.05)
        (root / "forge-orphan.txt").write_text(orphan.read_text() if orphan.exists() else "")
    # The dispatcher holds this one's launcher once it has identified the
    # requester; the requester then exits, as a crew that kills and reaps
    # its own waiter would, and only then is its caller resolved.
    stderr = gaps / "forged-gap.stderr"
    gapped = forge("forged-gap", stderr if sys.platform == "darwin" else None)
    identified = gaps / "forged-gap.identified"
    deadline = time.time() + 60
    while not (identified.exists() and identified.read_text()) and time.time() < deadline:
        time.sleep(0.05)
    waiter = int(identified.read_text()) if identified.exists() and identified.read_text() else None
    if gapped is not None:
        gapped.kill()
        try:
            stderr.write_text(gapped.communicate(timeout=10)[1])
        except subprocess.TimeoutExpired:
            gapped.wait()
    elif waiter is not None:
        try:
            os.kill(waiter, signal.SIGKILL)
        except ProcessLookupError:
            pass  # already gone: the note below still records it
    note = "never identified"
    if waiter is not None:
        while time.time() < deadline:
            try:
                os.kill(waiter, 0)
            except ProcessLookupError:
                note = "requester %d gone" % waiter
                break
            time.sleep(0.05)
    if waiter is None:
        note += "; requester stderr: " + (stderr.read_text() if stderr.exists() else "(none)")
    (gaps / "forged-gap.hold").unlink()
    (root / "forge-gap.txt").write_text(note)
    done()
marker = root / ("parked-on-" + task)
if not marker.exists():
    # "park": the build waits for the gate this harness opens once its park
    # is recorded, so the crew always parks and is resumed. "settled": the
    # crew waits for the build to succeed and parks only then, so the park
    # always finds it settled and the crew reads its outcome at once.
    settled = kind == "settled"
    gate = root / ("park-gate-" + task)
    build = "echo crew-build-ok > %s" % (root / ("crew-build-" + task))
    if not settled:
        build = "i=0; while [ ! -e %s ] && [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done; %s" % (gate, build)
    started = call("job.start", {"task_id": task, "heavy": True, "wait_for_capacity": True,
        "command": ["sh", "-c", build]})
    marker.write_text(started["id"])
    if settled:
        deadline = time.time() + 60
        while call("job.status", {"id": started["id"]})["status"] != "succeeded":
            if time.time() > deadline:
                (root / "harness-error").write_text("the settled build never succeeded")
                sys.exit(3)
            time.sleep(0.1)
    parked = run("crew.park", {"job_id": started["id"], "next": "read the build"})
    if parked.returncode == 0 and not settled:
        gate.write_text("parked")
        done()
    # A build that settled before the park: a real crew reads its outcome now.
    if parked.returncode == 0 or "read its outcome instead of parking" not in parked.stderr:
        (root / "harness-error").write_text("crew.park (%s): %d %s" % (kind, parked.returncode, parked.stderr))
        sys.exit(3)
    how = "read at its park"
else:
    how = "resumed"
job = call("job.status", {"id": marker.read_text().strip()})
call("crew.report", {"kind": "progress", "body": "%s on %s; build %s" % (how, task, job["status"])})
done()
"#;

struct Ship {
    fleet: Fleet,
    wrap: Vec<OsString>,
    dispatcher: Option<Resident>,
}

impl Ship {
    /// A fleet whose first mate is `infra`, with this harness, or `None`
    /// where no sandbox can be made here.
    fn new() -> Option<Self> {
        let fleet = Fleet::new(now_settings());
        let wrap = support::sandbox_prefix(&fleet.root)?;
        Some(Self::fixture(fleet, wrap))
    }

    /// Exercise the real waiter/dispatcher/supervisor boundary even on hosts
    /// where a PID or Seatbelt sandbox cannot be created.
    fn forced() -> Self {
        Self::fixture(
            Fleet::new(now_settings()),
            vec!["env".into(), "PRIVATEER_LAUNCH_VIA=dispatcher".into()],
        )
    }

    fn fixture(fleet: Fleet, wrap: Vec<OsString>) -> Self {
        fs::write(fleet.root.join("mate.py"), HARNESS).unwrap();
        let wrap_json: Vec<String> = wrap
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        fs::write(fleet.root.join("wrap.json"), json!(wrap_json).to_string()).unwrap();
        // One deterministic heavy slot, as in tests/jobs.rs: admission under
        // live load is tested elsewhere.
        fleet.ok(
            "resource.configure",
            json!({"max_heavy_jobs":2,"max_load_per_cpu":1024,"ramp_secs":0,
                "heavy_estimate":{"memory_bytes":0,"disk_bytes":0}}),
            None,
        );
        Self {
            fleet,
            wrap,
            dispatcher: None,
        }
    }

    /// Start the home's resident dispatcher and wait for its first pass,
    /// which records the PID namespace work is recorded in.
    fn serve(&mut self) {
        self.serve_with(&[]);
    }

    fn serve_with(&mut self, env: &[(&str, &Path)]) {
        let mut command = self.fleet.command();
        remove_mise_and_codex_environment(&mut command);
        command.args(["serve", "--json", r#"{"interval_secs":1}"#]);
        for (name, value) in env {
            command.env(name, value);
        }
        self.dispatcher = Some(Resident::spawn(command, &self.fleet.root));
        self.until("the dispatcher's first pass", || {
            rusqlite::Connection::open(self.fleet.home.join("privateer.sqlite"))
                .ok()
                .and_then(|db| {
                    db.query_row(
                        "SELECT value FROM metadata WHERE key='dispatcher_release'",
                        [],
                        |r| r.get::<_, String>(0),
                    )
                    .ok()
                })
                .is_some_and(|raw| raw.contains("passed_ms"))
        });
    }

    fn stop(&mut self) {
        if let Some(mut dispatcher) = self.dispatcher.take() {
            dispatcher.stop();
        }
    }

    /// `command` as the first mate, inside the sandbox.
    fn inside(&self, command: &str, args: &Value) -> Output {
        self.inside_with(command, args, &[])
    }

    fn inside_with(&self, command: &str, args: &Value, env: &[(&str, &str)]) -> Output {
        let mut cli = self.fleet.command();
        cli.args([command, "--json", &args.to_string()])
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", "infra");
        for (name, value) in env {
            cli.env(name, value);
        }
        support::inside(&self.wrap, &cli).output().unwrap()
    }

    fn ok_inside(&self, command: &str, args: Value) -> Value {
        let output = self.inside(command, &args);
        assert!(
            output.status.success(),
            "{command} inside the sandbox: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn refused_inside(&self, command: &str, args: Value, env: &[(&str, &str)]) -> String {
        let output = self.inside_with(command, &args, env);
        assert!(
            !output.status.success(),
            "{command} inside the sandbox was not refused: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    fn until(&self, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(90);
        while !done() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; harness: {}",
                fs::read_to_string(self.fleet.root.join("harness-error")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// A job's status, read from inside the sandbox.
    fn status_inside(&self, id: &str) -> Value {
        self.ok_inside("job.status", json!({"id": id}))
    }

    fn wait_status(&self, id: &str, accepted: &[&str]) -> Value {
        let mut last = Value::Null;
        self.until(&format!("job {id} to reach {accepted:?}"), || {
            last = self.status_inside(id);
            accepted.contains(&last["status"].as_str().unwrap_or(""))
        });
        last
    }

    fn requests(&self) -> Vec<Value> {
        let db = rusqlite::Connection::open(self.fleet.home.join("privateer.sqlite")).unwrap();
        let exists: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='launch_requests')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if !exists {
            return vec![];
        }
        let mut statement = db
            .prepare("SELECT id, command, state, error FROM launch_requests ORDER BY created_ms")
            .unwrap();
        statement
            .query_map([], |r| {
                Ok(
                    json!({"id": r.get::<_, String>(0)?, "command": r.get::<_, String>(1)?,
                    "state": r.get::<_, String>(2)?, "error": r.get::<_, Option<String>>(3)?}),
                )
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// The state and error of the request whose `id` argument is `job`.
    fn request_for(&self, job: &str) -> Option<(String, Option<String>)> {
        let db = rusqlite::Connection::open(self.fleet.home.join("privateer.sqlite")).unwrap();
        db.query_row(
            "SELECT state, error FROM launch_requests WHERE json_extract(args, '$.id')=?1",
            [job],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()
    }

    fn jobs(&self) -> Vec<Value> {
        self.fleet.ok("job.list", json!({}), None)["jobs"]
            .as_array()
            .unwrap()
            .clone()
    }
}

impl Drop for Ship {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Whether a recorded process identity still runs: the same pid and start.
fn running(process: &Value) -> bool {
    let Some(pid) = process["pid"].as_u64() else {
        return false;
    };
    let started = process["started"].as_str().unwrap_or("");
    #[cfg(target_os = "linux")]
    {
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let fields: Vec<&str> = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        !matches!(fields[0], "Z" | "X") && started.ends_with(&format!(":{}", fields[19]))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let out = Command::new("/bin/ps")
            .args(["-p", &pid.to_string(), "-o", "lstart=", "-o", "stat="])
            .env("TZ", "UTC0")
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let fields: Vec<&str> = text.split_whitespace().collect();
        fields.len() > 1
            && !fields.last().unwrap().starts_with('Z')
            && fields[..fields.len() - 1].join(" ") == started
    }
}

/// Where the payload ran: outside any sandbox, in the dispatcher's world.
#[cfg(target_os = "linux")]
fn placement_probe(out: &Path) -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        format!(
            "readlink /proc/self/ns/pid > {0}; env | sort > {0}.env",
            out.display()
        ),
    ]
}
#[cfg(target_os = "linux")]
fn outside_marker() -> String {
    fs::read_link("/proc/self/ns/pid")
        .unwrap()
        .to_string_lossy()
        .into_owned()
}
#[cfg(not(target_os = "linux"))]
fn placement_probe(out: &Path) -> Vec<String> {
    // `ps -ax` is denied under Codex's Seatbelt profile (#732).
    vec![
        "sh".into(),
        "-c".into(),
        format!(
            "/bin/ps -ax >/dev/null && echo unsandboxed > {0}; env | sort > {0}.env",
            out.display()
        ),
    ]
}
#[cfg(not(target_os = "linux"))]
fn outside_marker() -> String {
    "unsandboxed".into()
}

/// The refusal names a sandbox limit and says nothing was started.
fn assert_sandbox_refusal(text: &str) {
    assert!(
        text.contains("not a missing Privateer permission"),
        "{text}"
    );
    assert!(text.contains("No work was started"), "{text}");
    assert!(text.contains("retrying the same command is safe"), "{text}");
    assert!(!text.contains("permissions.blocked"), "{text}");
    assert!(!text.to_lowercase().contains("permission denied"), "{text}");
}

/// The fields a forwarded record must share with a direct one: the work, its
/// placement, its admission and its route, all but its identity and timing.
fn comparable(job: &Value) -> Value {
    let mut job = job.clone();
    for key in [
        "id",
        "incarnation",
        "created_ms",
        "finished_ms",
        "launch_ms",
        "spawned_ms",
        "supervisor",
        "payload",
        "status",
        "reason",
        "exit_code",
        "leader_result",
        "group_anchors",
        "escaped",
        "usage",
        "usage_partial",
        "run_ms",
        "agent_request_hash",
        "cancel_requested",
        "cancelled_by",
        "cancel_reason",
        "stdout_dropped_bytes",
        "stderr_dropped_bytes",
    ] {
        job.as_object_mut().unwrap().remove(key);
    }
    job
}

/// #747: requester session and executable lookup state must not enter the
/// unsandboxed launcher's world. This runs real subprocesses without needing
/// user namespaces; the sandbox qualification below repeats the assertion.
#[test]
fn a_forwarded_launch_inherits_the_dispatcher_not_the_requester_session() {
    let mut ship = Ship::forced();
    ship.serve_with(&[
        ("PRIVATEER_TEST_LAUNCH_SOURCE", Path::new("dispatcher")),
        (
            "CODEX_CA_CERTIFICATE",
            Path::new("/fixture/dispatcher-ca.pem"),
        ),
    ]);
    assert_clean_forwarded_environment(
        &ship,
        &[
            ("PRIVATEER_TEST_LAUNCH_SOURCE", "dispatcher"),
            ("CODEX_CA_CERTIFICATE", "/fixture/dispatcher-ca.pem"),
        ],
    );
}

fn assert_clean_forwarded_environment(ship: &Ship, dispatcher_env: &[(&str, &str)]) {
    let root = &ship.fleet.root;
    let probe = root.join("session-placement");
    let direct_probe = root.join("session-direct-placement");
    let arg0 = root.join("user/.codex/tmp/arg0-linux/codex-arg0fixture");
    fs::create_dir_all(&arg0).unwrap();
    let mut baseline = ship.fleet.command();
    remove_mise_and_codex_environment(&mut baseline);
    baseline.envs(dispatcher_env.iter().copied());
    let baseline_path = baseline
        .get_envs()
        .find(|(name, _)| *name == "PATH")
        .and_then(|(_, value)| value)
        .unwrap();
    let path = format!("{}:{}", arg0.display(), baseline_path.to_string_lossy());
    let output = ship.inside_with(
        "job.start",
        &json!({"task_id": "infra-repair", "command": placement_probe(&probe)}),
        &[
            ("PATH", &path),
            ("__MISE_DIFF", "fixture-diff"),
            ("__MISE_TASK_TOOL_ARGS", "fixture-tool-args"),
            ("CODEX_THREAD_ID", "fixture-thread"),
            ("CODEX_SESSION_ID", "fixture-session"),
            ("CODEX_TOOL_CALL_ID", "fixture-tool-call"),
            ("CODEX_CI", "1"),
            ("CODEX_VERSION", "fixture-version"),
            ("CODEX_CA_CERTIFICATE", "/untrusted/requester-ca.pem"),
            ("PRIVATEER_TEST_REQUESTER_ONLY", "not-launcher-config"),
            ("PRIVATEER_TEST_LAUNCH_SOURCE", "requester"),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let started: Value = serde_json::from_slice(&output.stdout).unwrap();
    let direct_output = baseline
        .args([
            "job.start",
            "--json",
            &json!({"task_id": "infra-repair", "command": placement_probe(&direct_probe)})
                .to_string(),
        ])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "infra")
        .output()
        .unwrap();
    assert!(
        direct_output.status.success(),
        "{}",
        String::from_utf8_lossy(&direct_output.stderr)
    );
    let direct: Value = serde_json::from_slice(&direct_output.stdout).unwrap();
    for job in [&started, &direct] {
        ship.wait_status(job["id"].as_str().unwrap(), &["succeeded"]);
    }
    let environment = |file: &Path| -> Vec<String> {
        fs::read_to_string(file.with_extension("env"))
            .unwrap()
            .lines()
            .filter(|line| !line.starts_with("PRIVATEER_JOB="))
            .map(str::to_owned)
            .collect()
    };
    let forwarded = environment(&probe);
    for (name, value) in dispatcher_env {
        assert!(
            forwarded.contains(&format!("{name}={value}")),
            "the dispatcher's marker must reach the payload"
        );
    }
    assert!(!forwarded.contains(&"PRIVATEER_TEST_LAUNCH_SOURCE=requester".to_owned()));
    assert!(
        forwarded.iter().all(|line| {
            !line.starts_with("__MISE_")
                && ![
                    "CODEX_THREAD_ID=",
                    "CODEX_SESSION_ID=",
                    "CODEX_TOOL_CALL_ID=",
                    "CODEX_CI=",
                    "CODEX_VERSION=",
                ]
                .iter()
                .any(|name| line.starts_with(name))
                && !line.starts_with("PRIVATEER_TEST_REQUESTER_ONLY=")
                && !line.contains("codex-arg0fixture")
        }),
        "requester session state reached the payload"
    );
    assert_eq!(
        forwarded,
        environment(&direct_probe),
        "a forwarded payload has a direct launch's environment"
    );
}

fn remove_mise_and_codex_environment(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy();
        if name.starts_with("__MISE_") || name.starts_with("CODEX_") {
            command.env_remove(name.as_ref());
        }
    }
    if let Some((_, Some(path))) = command.get_envs().find(|(name, _)| *name == "PATH") {
        let entries: Vec<_> = std::env::split_paths(path)
            .filter(|entry| !entry.to_string_lossy().contains("/.codex/tmp/arg0-"))
            .collect();
        command.env("PATH", std::env::join_paths(entries).unwrap());
    }
}

#[test]
fn a_sandboxed_first_mate_starts_runs_and_cancels_a_heavy_job_a_crew_and_an_errand() {
    let Some(mut ship) = Ship::new() else {
        return;
    };
    ship.serve();
    let root = ship.fleet.root.clone();
    assert_clean_forwarded_environment(&ship, &[]);

    // A heavy job: the same answer as a direct start, run outside the
    // sandbox, its outcome readable from inside.
    let probe = root.join("heavy-placement");
    let started = ship.ok_inside(
        "job.start",
        json!({"task_id": "infra-repair", "heavy": true, "wait_for_capacity": true,
            "command": placement_probe(&probe)}),
    );
    let id = started["id"].as_str().unwrap().to_owned();
    assert!(
        matches!(
            started["status"].as_str(),
            Some("starting" | "queued" | "launching" | "running" | "succeeded")
        ),
        "{started}"
    );
    let direct = ship.fleet.ok(
        "job.start",
        json!({"task_id": "infra-repair", "heavy": true, "wait_for_capacity": true,
            "command": placement_probe(&root.join("direct-placement"))}),
        Some("infra"),
    );
    let mut started_keys: Vec<&String> = started.as_object().unwrap().keys().collect();
    let mut direct_keys: Vec<&String> = direct.as_object().unwrap().keys().collect();
    started_keys.sort();
    direct_keys.sort();
    assert_eq!(
        started_keys, direct_keys,
        "the answer has a direct start's shape"
    );
    let finished = ship.wait_status(&id, &["succeeded", "failed"]);
    assert_eq!(finished["status"], "succeeded", "{finished}");
    assert_eq!(finished["exit_code"], 0, "{finished}");
    assert_eq!(
        fs::read_to_string(&probe).unwrap().trim(),
        outside_marker(),
        "the payload ran outside the sandbox"
    );
    let direct_finished = ship.wait_status(direct["id"].as_str().unwrap(), &["succeeded"]);
    // Condition B: the same work, workspace mapping and admission.
    let mut forwarded = comparable(&finished);
    let mut reference = comparable(&direct_finished);
    for record in [&mut forwarded, &mut reference] {
        record["command"] = json!("probe");
    }
    assert_eq!(
        forwarded, reference,
        "a forwarded job is recorded as a direct one"
    );
    // ... and its payload gets the environment a direct start gives, less
    // what names the job itself.
    let environment = |path: &Path, job: &str| -> Vec<String> {
        fs::read_to_string(path.with_extension("env"))
            .unwrap()
            .lines()
            .filter(|line| !line.starts_with("PRIVATEER_JOB") && !line.contains(job))
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(
        environment(&probe, &id),
        environment(
            &root.join("direct-placement"),
            direct["id"].as_str().unwrap()
        ),
        "a forwarded payload's environment is a direct one's"
    );

    // A heavy job cancelled from inside: its payload is gone, not just its wrapper.
    let sleeper = ship.ok_inside(
        "job.start",
        json!({"task_id": "infra-repair", "heavy": true, "wait_for_capacity": true,
            "command": ["sleep", "120"]}),
    );
    let sleeper_id = sleeper["id"].as_str().unwrap().to_owned();
    let mut payload = Value::Null;
    ship.until("the sleeping job's payload", || {
        payload = ship.status_inside(&sleeper_id)["payload"].clone();
        payload.is_object()
    });
    assert!(running(&payload));
    let cancelled = ship.ok_inside("job.cancel", json!({"id": sleeper_id}));
    assert!(
        matches!(cancelled["status"].as_str(), Some("cancelled" | "running")),
        "{cancelled}"
    );
    ship.wait_status(&sleeper_id, &["cancelled"]);
    ship.until("the cancelled payload to stop", || !running(&payload));

    // A crew: launched from inside; its harness's own calls run inside the
    // sandbox too. It starts a heavy job, parks on it, is resumed by the
    // outcome and reports; a build that settled first it reads at once.
    let crew = ship.ok_inside(
        "agent.start",
        json!({"id": "sandboxed-crew", "task_id": "infra-repair"}),
    );
    assert_eq!(crew["id"], "sandboxed-crew", "{crew}");
    let direct_crew = ship.fleet.ok(
        "agent.start",
        json!({"id": "direct-crew", "task_id": "infra-duty"}),
        Some("infra"),
    );
    let reported = |body: &str| {
        let db = rusqlite::Connection::open(ship.fleet.home.join("privateer.sqlite")).unwrap();
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM mail WHERE body LIKE '%' || ?1 || '%')",
            [body],
            |r| r.get::<_, bool>(0),
        )
        .unwrap()
    };
    // Its build waits until the park is recorded, so it always parks and
    // is resumed.
    ship.until("the crew's report on its build after its resume", || {
        reported("resumed on infra-repair; build succeeded")
    });
    let crew_record = ship.status_inside("sandboxed-crew");
    let direct_crew_record = ship.wait_status(
        direct_crew["id"].as_str().unwrap(),
        &["succeeded", "failed", "parked", "resumed"],
    );
    // Condition B for a crew: the same harness command (all but its brief,
    // which names the task), working directory and route resolution as a
    // direct launch.
    let harness = |record: &Value| {
        let mut argv = record["command"].as_array().unwrap().clone();
        argv.remove(4);
        argv
    };
    assert_eq!(
        harness(&crew_record),
        harness(&direct_crew_record),
        "the forwarded crew runs the same harness command"
    );
    assert_eq!(crew_record["cwd"], direct_crew_record["cwd"]);
    for key in [
        "action",
        "agent_actor",
        "heavy",
        "model_route",
        "route_name",
        "route_auto",
        "timeout_secs",
        "launch",
        "hosted",
    ] {
        assert_eq!(
            crew_record[key], direct_crew_record[key],
            "crew field {key}: {crew_record} vs {direct_crew_record}"
        );
    }
    let built = fs::read_to_string(root.join("crew-build-infra-repair")).unwrap_or_default();
    assert_eq!(built.trim(), "crew-build-ok");
    assert!(!reported("read at its park on infra-repair"));

    // A crew whose build has already succeeded when it parks: the park is
    // refused with its outcome, which the crew reads and reports at once.
    ship.fleet.ok(
        "task.create",
        json!({"id": "infra-audit", "charter_id": "fleet", "owner": "infra",
            "title": "infra audit"}),
        None,
    );
    fs::write(root.join("crew-mode"), "settled").unwrap();
    ship.ok_inside(
        "agent.start",
        json!({"id": "settled-crew", "task_id": "infra-audit"}),
    );
    ship.until("the settled crew's report on its build", || {
        reported("read at its park on infra-audit; build succeeded")
    });
    assert!(!reported("resumed on infra-audit"));

    // A crew cancelled from inside.
    fs::write(root.join("crew-mode"), "sleep").unwrap();
    ship.ok_inside(
        "agent.start",
        json!({"id": "sleeping-crew", "task_id": "infra-repair", "replace": true}),
    );
    let mut crew_payload = Value::Null;
    ship.until("the sleeping crew's payload", || {
        crew_payload = ship.status_inside("sleeping-crew")["payload"].clone();
        crew_payload.is_object()
    });
    ship.ok_inside("job.cancel", json!({"id": "sleeping-crew"}));
    ship.wait_status("sleeping-crew", &["cancelled"]);
    ship.until("the cancelled crew's harness to stop", || {
        !running(&crew_payload)
    });

    // An errand: started from inside, settled with its reply.
    let errand = ship.ok_inside(
        "errand.start",
        json!({"focus": "thread:sandboxed", "brief": "Review the cache."}),
    );
    let errand_id = errand["errand"].as_str().unwrap().to_owned();
    ship.until("the errand to settle", || {
        ship.ok_inside("errand.list", json!({}))["errands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["id"] == errand_id.as_str() && e["status"] != "running")
    });
    let listed = ship.ok_inside("errand.list", json!({}));
    let settled = listed["errands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == errand_id.as_str())
        .unwrap()
        .clone();
    assert!(
        settled["verdict"]
            .as_str()
            .is_some_and(|v| v.contains("inside the sandbox")),
        "{settled}"
    );

    // An errand cancelled from inside.
    fs::write(root.join("errand-mode"), "sleep").unwrap();
    let long = ship.ok_inside(
        "errand.start",
        json!({"focus": "thread:sandboxed-long", "brief": "Take your time."}),
    );
    let long_id = long["errand"].as_str().unwrap().to_owned();
    let mut errand_payload = Value::Null;
    ship.until("the long errand's payload", || {
        errand_payload = ship.status_inside(&long_id)["payload"].clone();
        errand_payload.is_object()
    });
    ship.ok_inside("job.cancel", json!({"id": long_id}));
    ship.wait_status(&long_id, &["cancelled"]);
    ship.until("the cancelled errand's harness to stop", || {
        !running(&errand_payload)
    });

    // Every launch above went through the dispatcher, and none was refused
    // or lost (the direct crew's resume may still be running).
    let requests = ship.requests();
    assert!(requests.len() >= 8, "{requests:?}");
    assert!(
        requests
            .iter()
            .all(|r| matches!(r["state"].as_str(), Some("done" | "pending" | "claimed"))),
        "{requests:?}"
    );
}

#[test]
fn with_no_dispatcher_a_sandboxed_launch_is_refused_at_once_and_never_runs_later() {
    let Some(mut ship) = Ship::new() else {
        return;
    };
    // A dispatcher has run (it recorded its PID namespace) but is stopped.
    ship.fleet.tick();
    let began = Instant::now();
    let refused = ship.refused_inside(
        "job.start",
        json!({"task_id": "infra-repair", "command": ["true"]}),
        &[],
    );
    assert!(began.elapsed() < Duration::from_secs(10), "refused at once");
    assert_sandbox_refusal(&refused);
    assert!(refused.contains("no dispatcher is running"), "{refused}");
    // Started afterwards, the dispatcher launches nothing for it.
    ship.serve();
    std::thread::sleep(Duration::from_secs(2));
    assert!(ship.jobs().is_empty(), "{:?}", ship.jobs());
    assert!(
        ship.requests().iter().all(|r| r["state"] == "expired"),
        "{:?}",
        ship.requests()
    );
}

#[test]
fn an_unclaimed_request_is_withdrawn_after_its_window_and_never_runs_late() {
    let Some(mut ship) = Ship::new() else {
        return;
    };
    ship.fleet.tick();
    // Something holds the serve lock but never claims, as a wedged
    // dispatcher would.
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(ship.fleet.home.join("serve.lock"))
        .unwrap();
    use std::os::fd::AsRawFd;
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let refused = ship.refused_inside(
        "job.start",
        json!({"task_id": "infra-repair", "command": ["true"]}),
        &[("PRIVATEER_LAUNCH_CLAIM_MS", "1500")],
    );
    assert_sandbox_refusal(&refused);
    assert!(
        refused.contains("no dispatcher took the request within 2 s"),
        "{refused}"
    );
    drop(lock);
    ship.serve();
    std::thread::sleep(Duration::from_secs(2));
    assert!(ship.jobs().is_empty(), "{:?}", ship.jobs());
}

#[test]
fn a_request_naming_another_process_is_refused_and_starts_nothing() {
    let Some(mut ship) = Ship::new() else {
        return;
    };
    ship.serve();
    // A live process that is not waiting on the request: a forger's row
    // naming someone else's process.
    let mut bystander = Command::new("sleep").arg("60").spawn().unwrap();
    let pid = bystander.id();
    let identity = {
        let mut cli = ship.fleet.command();
        cli.args(["job.list", "--json", "{}"]);
        // The bystander's identity in the form jobs record it.
        #[cfg(target_os = "linux")]
        {
            let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            let ticks = stat
                .rsplit_once(')')
                .unwrap()
                .1
                .split_whitespace()
                .nth(19)
                .unwrap()
                .to_owned();
            let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
            json!({"pid": pid, "started": format!("linux:{}:{ticks}", boot.trim()),
                "pid_namespace": outside_marker(), "zombie": false})
        }
        #[cfg(not(target_os = "linux"))]
        {
            let out = Command::new("/bin/ps")
                .args(["-p", &pid.to_string(), "-o", "lstart="])
                .env("TZ", "UTC0")
                .env("LC_ALL", "C")
                .output()
                .unwrap();
            let started = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            json!({"pid": pid, "started": started, "pid_namespace": null, "zombie": false})
        }
    };
    // Make sure the table exists, then forge a pending row.
    let _ = ship.refused_inside("job.cancel", json!({"id": "no-such-job"}), &[]);
    let db = rusqlite::Connection::open(ship.fleet.home.join("privateer.sqlite")).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    db.execute(
        "INSERT INTO launch_requests(id, command, args, requester, claimed, state, created_ms, deadline_ms)
         VALUES('forged', 'job.start', ?1, ?2, ?3, 'pending', ?4, ?5)",
        rusqlite::params![
            json!({"task_id": "infra-repair", "id": "forged-job", "command": ["true"]}).to_string(),
            identity.to_string(),
            json!({"role": "operator", "sandbox": "forced"}).to_string(),
            now,
            now + 30_000
        ],
    )
    .unwrap();
    ship.until("the forged row to be judged", || {
        ship.requests()
            .iter()
            .any(|r| r["id"] == "forged" && r["state"] != "pending" && r["state"] != "claimed")
    });
    let forged = ship
        .requests()
        .into_iter()
        .find(|r| r["id"] == "forged")
        .unwrap();
    assert_eq!(forged["state"], "refused", "{forged}");
    assert!(
        forged["error"]
            .as_str()
            .unwrap()
            .contains("is not waiting on this request"),
        "{forged}"
    );
    assert!(
        !ship.jobs().iter().any(|j| j["id"] == "forged-job"),
        "nothing was started"
    );
    let _ = bystander.kill();
    let _ = bystander.wait();
}

#[test]
fn a_sandboxed_crew_that_drops_its_role_is_refused_by_the_dispatcher() {
    let Some(mut ship) = Ship::new() else {
        return;
    };
    ship.serve();
    fs::write(ship.fleet.root.join("crew-mode"), "drop").unwrap();
    ship.ok_inside(
        "agent.start",
        json!({"id": "dropping-crew", "task_id": "infra-repair"}),
    );
    let drop_file = ship.fleet.root.join("drop.txt");
    ship.until("the crew's attempt", || drop_file.exists());
    let text = fs::read_to_string(&drop_file).unwrap();
    assert!(!text.starts_with("0\n"), "{text}");
    assert!(text.contains("cannot drop its role"), "{text}");
    assert!(
        ship.jobs()
            .iter()
            .all(|j| j["started_by"] != "dropping-crew"),
        "the crew started nothing"
    );
}

#[test]
fn a_requester_gone_before_the_claim_is_refused() {
    let Some(mut ship) = Ship::new() else {
        return;
    };
    ship.fleet.tick();
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(ship.fleet.home.join("serve.lock"))
        .unwrap();
    use std::os::fd::AsRawFd;
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let mut cli = ship.fleet.command();
    cli.args([
        "job.start",
        "--json",
        &json!({"task_id": "infra-repair", "id": "orphan-job", "command": ["true"]}).to_string(),
    ])
    .env("PRIVATEER_ROLE", "agent")
    .env("PRIVATEER_ACTOR", "infra");
    let mut waiter = support::inside(&ship.wrap, &cli).spawn().unwrap();
    ship.until("the request to be pending", || {
        ship.requests().iter().any(|r| r["state"] == "pending")
    });
    // The whole sandbox goes, with the waiter in it.
    let _ = waiter.kill();
    let _ = waiter.wait();
    std::thread::sleep(Duration::from_millis(300));
    drop(lock);
    ship.serve();
    ship.until("the request to be judged", || {
        ship.requests()
            .iter()
            .all(|r| r["state"] != "pending" && r["state"] != "claimed")
    });
    let request = ship.requests().pop().unwrap();
    assert_eq!(request["state"], "refused", "{request}");
    assert_sandbox_refusal(request["error"].as_str().unwrap());
    assert!(!ship.jobs().iter().any(|j| j["id"] == "orphan-job"));
}

#[test]
fn a_request_rewritten_after_its_waiter_started_is_refused_and_starts_nothing() {
    let Some(mut ship) = Ship::new() else {
        return;
    };
    ship.fleet.tick();
    // Hold the serve lock so the request waits, pending, for a dispatcher.
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(ship.fleet.home.join("serve.lock"))
        .unwrap();
    use std::os::fd::AsRawFd;
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let mut cli = ship.fleet.command();
    cli.args([
        "job.start",
        "--json",
        &json!({"task_id": "infra-repair", "id": "honest-job", "command": ["true"]}).to_string(),
    ])
    .env("PRIVATEER_ROLE", "agent")
    .env("PRIVATEER_ACTOR", "infra");
    let waiter = support::inside(&ship.wrap, &cli)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    ship.until("the request to be pending", || {
        ship.request_for("honest-job")
            .is_some_and(|(state, _)| state == "pending")
    });
    // Someone else with write access to the home rewrites the waiting row:
    // another command, and a claim to be the operator.
    let marker = ship.fleet.root.join("rewritten-ran");
    let db = rusqlite::Connection::open(ship.fleet.home.join("privateer.sqlite")).unwrap();
    assert_eq!(
        db.execute(
            "UPDATE launch_requests SET args=?1, claimed=?2
             WHERE json_extract(args, '$.id')='honest-job' AND state='pending'",
            rusqlite::params![
                json!({"task_id": "infra-repair", "id": "honest-job",
                    "command": ["touch", marker.to_str().unwrap()]})
                .to_string(),
                json!({"role": "operator", "actor": null, "env_job": null, "fleet": null,
                    "sandbox": "pid_namespace"})
                .to_string()
            ],
        )
        .unwrap(),
        1
    );
    drop(lock);
    ship.serve();
    let output = waiter.wait_with_output().unwrap();
    assert!(!output.status.success(), "the waiter reports a refusal");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("is not waiting on this request"),
        "{stderr}"
    );
    assert_sandbox_refusal(&stderr);
    let (state, error) = ship.request_for("honest-job").unwrap();
    assert_eq!(state, "refused", "{error:?}");
    assert!(!marker.exists(), "the rewritten command never ran");
    assert!(!ship.jobs().iter().any(|j| j["id"] == "honest-job"));
}

/// The race review f9369bed found: a sandboxed crew records a request that
/// claims the operator, with no `PRIVATEER_JOB`, and kills and reaps its own
/// waiter after the dispatcher identified it but before it resolved its
/// caller. The walk then finds nothing; the claim must never stand in.
#[cfg(feature = "test-launch-gap")]
#[test]
fn a_crew_claiming_the_operator_is_refused_even_if_its_requester_exits_mid_launch() {
    let Some(mut ship) = Ship::new() else {
        return;
    };
    let gaps = ship.fleet.root.join("gaps");
    fs::create_dir_all(&gaps).unwrap();
    fs::write(gaps.join("forged-gap.hold"), "").unwrap();
    ship.serve_with(&[("PRIVATEER_TEST_LAUNCH_GAP", &gaps)]);
    fs::write(ship.fleet.root.join("crew-mode"), "forge").unwrap();
    ship.ok_inside(
        "agent.start",
        json!({"id": "forging-crew", "task_id": "infra-repair"}),
    );
    let gap = ship.fleet.root.join("forge-gap.txt");
    ship.until("the crew's attempts", || gap.exists());
    // With its requester alive, the walk finds the crew's supervisor: the
    // launcher's on Linux, where the PID namespace hides the ancestry from
    // the CLI, and the CLI's own under Seatbelt, which hides none.
    let live = fs::read_to_string(ship.fleet.root.join("forge-live.txt")).unwrap();
    assert!(!live.starts_with("0\n"), "{live}");
    assert!(live.contains("cannot drop its role"), "{live}");
    let note = fs::read_to_string(&gap).unwrap();
    assert!(
        note.contains("gone"),
        "{note}; forged-gap request: {:?}; all requests: {:?}",
        ship.request_for("forged-gap"),
        ship.requests()
    );
    ship.until("the gapped request to be judged", || {
        ship.request_for("forged-gap")
            .is_some_and(|(state, _)| state != "pending" && state != "claimed")
    });
    let (state, error) = ship.request_for("forged-gap").unwrap();
    let error = error.unwrap_or_default();
    assert_eq!(state, "refused", "{error}");
    assert_sandbox_refusal(&error);
    assert!(
        error.contains("exited while its caller was being resolved"),
        "{error}"
    );
    // macOS: a requester launchd adopted, whose chain never leaves its
    // sandbox, is refused before its operator claim is resolved, where it
    // would otherwise have stood in for the walk (the double-fork residual).
    #[cfg(target_os = "macos")]
    {
        let orphan = fs::read_to_string(ship.fleet.root.join("forge-orphan.txt")).unwrap();
        let (state, error) = ship
            .request_for("forged-orphan")
            .unwrap_or_else(|| panic!("no forged-orphan request; requester: {orphan}"));
        let error = error.unwrap_or_default();
        assert_eq!(state, "refused", "{error}; requester: {orphan}");
        assert_sandbox_refusal(&error);
        assert!(error.contains("no ancestor of it below init"), "{error}");
        assert!(orphan.contains("no ancestor of it below init"), "{orphan}");
    }
    for job in ["forged-live", "forged-gap", "forged-orphan"] {
        assert!(
            !ship.fleet.root.join(format!("ran-{job}")).exists(),
            "{job} never ran"
        );
        assert!(!ship.jobs().iter().any(|j| j["id"] == job), "{job}");
    }
}
