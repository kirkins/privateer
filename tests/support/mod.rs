//! The scripted first mate and its fleet, shared by the activation and crew
//! tests. Every subprocess gets this fleet's own state: its home, resource
//! ledger, fleet registry and HOME, and no service manager.
#![allow(dead_code)]
use privateer::{dispatch, store::Store};
use serde_json::{Value, json};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_privateer");

/// A command that inherits no Privateer settings from whoever runs the
/// tests. A crew job running `cargo test` has PRIVATEER_ROLE, _ACTOR and
/// _JOB set, which would otherwise make every test's subprocess its agent.
/// Its fleet registry is this process's own temporary one, not the machine's:
/// under a crew's gate job, a call that reaches the real registry is held to
/// the crew's role. Tests that need a setting set it explicitly afterwards.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("PRIVATEER_") {
            command.env_remove(name);
        }
    }
    let fleet = std::env::temp_dir().join(format!("privateer-test-fleet-{}", std::process::id()));
    command
        .env("PRIVATEER_FLEET", fleet.join("fleet.sqlite"))
        .env("PATH", fixture_path())
        .env_remove("XDG_STATE_HOME");
    command
}

/// A privateer command whose user state (HOME, configuration, resource
/// ledger, fleet registry, Herdr socket, service manager) all lies under
/// `root`, as a dispatcher pass needs.
pub fn isolated(root: &Path) -> Command {
    let user = root.join("user");
    fs::create_dir_all(&user).unwrap();
    let mut command = command(BIN);
    command
        .env("HOME", user)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env_remove("HERDR_ENV")
        .env_remove("HERDR_SOCKET_PATH")
        .env("PRIVATEER_RESOURCE_HOME", root.join("ledger"))
        .env("PRIVATEER_FLEET", root.join("fleet.sqlite"))
        .env("PRIVATEER_SERVICE", "none")
        .env("PATH", fixture_path())
        .env_remove("XDG_STATE_HOME");
    command
}

/// A temporary directory on a volume with room for the fixture: fixture homes
/// hold resource ledgers, worktree pools and job working directories, and
/// admission gates every job and workspace on the filesystem that work uses
/// (2048 MiB must stay free by default), while /tmp is a small tmpfs on many
/// dev machines, below that floor. An explicit TMPDIR is honoured exactly as
/// tempfile does. Without one, /var/tmp is preferred: disk-backed on Linux and
/// macOS, and outside any git checkout (a fixture home inside the repository,
/// such as Cargo's target tmpdir, is refused as a charter workspace).
pub fn tempdir() -> tempfile::TempDir {
    if std::env::var_os("TMPDIR").is_none() {
        let base = Path::new("/var/tmp");
        if base.is_dir() {
            if let Ok(temp) = tempfile::Builder::new()
                .prefix("privateer-fixture-")
                .tempdir_in(base)
            {
                return temp;
            }
        }
    }
    tempfile::tempdir().unwrap()
}

/// Whether `path` lies inside a directory `tempdir` may create fixtures in:
/// the system temporary directory or the /var/tmp it prefers. Fixtures that
/// run real upgrade or adopt code check this before touching anything.
pub fn is_temporary(path: &Path) -> bool {
    [std::env::temp_dir(), PathBuf::from("/var/tmp")]
        .iter()
        .filter_map(|base| base.canonicalize().ok())
        .any(|base| path.starts_with(base))
}

/// The interpreter every fixture runs, resolved once. A version-manager shim
/// on PATH (mise) refuses the isolated HOME's untrusted configuration rather
/// than run the script, so the first `python3` on PATH that is not a shim is
/// used, falling back to the system interpreter.
pub fn python() -> PathBuf {
    static PYTHON: OnceLock<PathBuf> = OnceLock::new();
    PYTHON
        .get_or_init(|| {
            real_interpreter("python3").unwrap_or_else(|| PathBuf::from("/usr/bin/python3"))
        })
        .clone()
}

/// Node every fixture runs, resolved once by the same rule as `python`:
/// the first `node` on PATH that is not a version-manager shim, falling back
/// to the system Node. None on a machine with no Node at all; the suites
/// that need one skip.
pub fn node() -> Option<PathBuf> {
    static NODE: OnceLock<Option<PathBuf>> = OnceLock::new();
    NODE.get_or_init(|| real_interpreter("node").or_else(|| real_file("/usr/bin/node")))
        .clone()
}

fn real_interpreter(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file() && !candidate.to_string_lossy().contains("/shims/"))
}

fn real_file(path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(path);
    path.is_file().then_some(path)
}

/// A private bin whose `node` runs the resolved real Node, ahead of the
/// inherited PATH (shim detection is by a `/shims/` path segment, as mise
/// and asdf use; other launchers such as Volta's are not recognised), on
/// every fixture command. A version-manager shim first on
/// PATH is a launcher that reads its user's configuration, which a fixture's
/// isolated HOME and cleared environment do not have, so fixtures must not
/// run it however they invoke Node, including `#!/usr/bin/env node` fakes.
/// None (and unused) without Node.
fn node_ahead_of_shims() -> Option<PathBuf> {
    static AHEAD: OnceLock<Option<PathBuf>> = OnceLock::new();
    AHEAD
        .get_or_init(|| {
            let node = node()?;
            let bin =
                std::env::temp_dir().join(format!("privateer-test-node-{}", std::process::id()));
            fs::create_dir_all(&bin).ok()?;
            let quoted = node.to_string_lossy().replace('\'', "'\\''");
            fs::write(
                bin.join("node"),
                format!("#!/bin/sh\nexec '{quoted}' \"$@\"\n"),
            )
            .ok()?;
            fs::set_permissions(bin.join("node"), fs::Permissions::from_mode(0o755)).ok()?;
            Some(bin)
        })
        .clone()
}

/// The fixture PATH with a fleet's own `bin` of fakes in front of it. The
/// interpreter's directory still comes before the inherited PATH, so a shim
/// there is not reached under the fleet's redirected XDG_STATE_HOME, where
/// mise finds no record of the user's trusted configuration.
pub fn path_with(bin: &Path) -> std::ffi::OsString {
    let path = fixture_path();
    let dirs = std::iter::once(bin.to_path_buf()).chain(std::env::split_paths(&path));
    std::env::join_paths(dirs).unwrap_or(path)
}

/// PATH with the interpreter's directory first, so `#!/usr/bin/env python3`
/// fakes and bare `python3` manifests in a fixture reach the same interpreter.
/// The private `node` wrapper sits behind it, before the inherited PATH, so a
/// shim first there never stands in for Node either.
fn fixture_path() -> std::ffi::OsString {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let interpreter = python();
    let dirs = interpreter
        .parent()
        .into_iter()
        .map(Path::to_path_buf)
        .chain(node_ahead_of_shims())
        .chain(std::env::split_paths(&path));
    std::env::join_paths(dirs).unwrap_or(path)
}

const ADAPTER: &str = r#"import json, sys
r = json.loads(sys.stdin.readline())
c, o = r["context"], r["options"]
argv = [sys.executable, o["mate"], json.dumps(c["control"]["cli_argv"]), c["owner"], c["brief"], o["mode"], o["log"], o.get("resume", ""), o.get("model", "")]
print(json.dumps({"id": r["id"], "version": 1, "ok": True, "result": {"argv": argv, "cwd": c["repo"], "env_names": []}}))
"#;

// A scripted first mate: reads its mailbox through the real CLI, answers open
// requests, acknowledges through the sequence its wake names, and reports its
// session and usage the way Codex does. No model is involved.
const MATE: &str = r#"import json, pathlib, re, subprocess, sys, time, uuid
cli, ship, prompt = json.loads(sys.argv[1]), sys.argv[2], sys.argv[3]
mode_file, log, resume = pathlib.Path(sys.argv[4]), pathlib.Path(sys.argv[5]), sys.argv[6]
print(json.dumps({"type": "thread.started", "thread_id": resume or str(uuid.uuid4())}), flush=True)
with log.open("a") as f:
    f.write(json.dumps(prompt) + "\n")
mode = mode_file.read_text().strip() if mode_file.exists() else "normal"
if mode == "fail":
    sys.exit(1)
if mode == "throttle":
    # How Pi ends a run the provider refused, as seen in this fleet's logs.
    print(json.dumps({"type": "message_end", "message": {"role": "assistant", "stopReason": "error",
        "errorMessage": "429: {\"type\":\"upstream_unavailable\"}", "content": []}}), flush=True)
    sys.exit(1)
if mode == "window":
    # How Gemini ends a run a plan window refused: its own error event, naming
    # the window's reset rather than a short retry.
    print(json.dumps({"type": "error", "timestamp": 1, "severity": "error",
        "message": "429 Too Many Requests. RESOURCE_EXHAUSTED: rate limit exceeded; resets in 5 hours"}), flush=True)
    sys.exit(1)
if mode in ("crew", "no-action"):
    # A crew's run or a no-action wake: Codex usage, no mailbox changes.
    print(json.dumps({"type": "turn.completed", "usage": {"input_tokens": 300, "cached_input_tokens": 100, "output_tokens": 30}}))
    sys.exit(0)
if mode.startswith("sleep"):
    time.sleep(float(mode.split()[1]))
def call(command, args):
    out = subprocess.run(cli + [command, "--json", json.dumps(args)], capture_output=True, text=True, check=True)
    return json.loads(out.stdout)
if mode == "half-throttle":
    # Answers the first open request, then the provider ends the run with a
    # limit: the mail shows a reply that must never be answered twice.
    first = next((m for m in call("mail.inbox", {"address": ship})["messages"]
                  if m["kind"] == "request" and m["state"] == "open"), None)
    if first:
        call("mail.send", {"from": ship, "to": first["from"], "kind": "reply",
                           "correlation": first["id"], "body": "answered before the limit"})
    print(json.dumps({"type": "message_end", "message": {"role": "assistant", "stopReason": "error",
        "errorMessage": "429: {\"type\":\"upstream_unavailable\"}", "content": []}}), flush=True)
    sys.exit(1)
errand = re.search(r"^Errand (errand-\S+)", prompt, re.M)
if errand:
    # An errand ends with its verdict as the final reply, as a model would.
    verdict = "Reviewed: %s. Brief seen: %s" % (errand.group(1), "the cache" in prompt)
    if mode == "errand-unconfirmed":
        # Pi wrote its whole stopped reply, but the turn wrapper could not
        # confirm the completion. Pi closes its turn and run after the reply.
        reply = {"role": "assistant", "stopReason": "stop", "content": [{"type": "text", "text": verdict}]}
        print(json.dumps({"type": "message_end", "message": reply}), flush=True)
        print(json.dumps({"type": "turn_end", "message": reply, "toolResults": []}), flush=True)
        print(json.dumps({"type": "agent_end", "messages": [reply], "willRetry": False}), flush=True)
        print(json.dumps({"type": "agent_settled", "aborted": False}), flush=True)
        print(json.dumps({"type": "native.turn.failed", "harness": "pi", "native_exit_code": 1,
            "reason": "Native completion was not confirmed."}), flush=True)
        sys.exit(2)
    if mode == "errand-blocked":
        # Claude finished its result, but a tool was denied along the way.
        print(json.dumps({"type": "result", "subtype": "success", "is_error": False, "result": verdict}), flush=True)
        print(json.dumps({"type": "native.turn.failed", "harness": "claude", "native_exit_code": 0}), flush=True)
        print(json.dumps({"type": "native.permission_blocked", "denials": [{"tool_name": "Bash",
            "tool_input": {"command": "cargo test"}}], "denied": 1}), flush=True)
        sys.exit(2)
    if mode == "errand-error-result":
        # Claude's result is an API error, not a verdict.
        print(json.dumps({"type": "result", "subtype": "success", "is_error": True,
            "result": "API Error: 400 invalid_request_error"}), flush=True)
        print(json.dumps({"type": "native.turn.failed", "harness": "claude", "native_exit_code": 1}), flush=True)
        sys.exit(2)
    if mode == "errand-multiturn":
        # Claude's first turn ends with a progress result, its second works and
        # then stalls: the first result is not the verdict.
        print(json.dumps({"type": "result", "subtype": "success", "is_error": False, "result_index": 1,
            "result": "Started the check in the background, I'll report when it finishes."}), flush=True)
        print(json.dumps({"type": "assistant", "message": {"role": "assistant",
            "content": [{"type": "tool_use", "name": "Bash", "input": {"command": "cargo test"}}]}}), flush=True)
        print(json.dumps({"type": "native.turn.failed", "harness": "claude", "reason": "stalled"}), flush=True)
        sys.exit(2)
    if mode == "errand-midturn-cut":
        # A Codex message said mid-turn, past the log's kept head, then more
        # output than its tail keeps: only the reply file still has it.
        work = json.dumps({"type": "item.completed", "item": {"type": "command_execution",
            "aggregated_output": "x" * 4000}})
        for _ in range(80):
            print(work)
        print(json.dumps({"type": "item.completed", "item": {"type": "agent_message",
            "text": "Let me run the tests first."}}), flush=True)
        for _ in range(320):
            print(work)
        print(json.dumps({"type": "turn.failed", "error": {"message": "stream disconnected"}}), flush=True)
        sys.exit(1)
    if mode == "errand-blocked-cut":
        # Claude's whole result is longer than the log's tail keeps, then a
        # tool denial: only the reply file holds the verdict.
        work = json.dumps({"type": "assistant", "message": {"role": "assistant",
            "content": [{"type": "text", "text": "x" * 4000}]}})
        for _ in range(80):
            print(work)
        print(json.dumps({"type": "result", "subtype": "success", "is_error": False,
            "result": verdict + "\n" + "y" * 900000}), flush=True)
        print(json.dumps({"type": "native.turn.failed", "harness": "claude", "native_exit_code": 0}), flush=True)
        print(json.dumps({"type": "native.permission_blocked", "denials": [{"tool_name": "Bash",
            "tool_input": {"command": "cargo test"}}], "denied": 1}), flush=True)
        sys.exit(2)
    if mode == "errand-midturn":
        # A Codex message said before a tool call, then the turn failed.
        print(json.dumps({"type": "item.completed", "item": {"type": "agent_message",
            "text": "Let me run the tests first."}}), flush=True)
        print(json.dumps({"type": "turn.failed", "error": {"message": "stream disconnected"}}), flush=True)
        sys.exit(1)
    print(json.dumps({"type": "item.completed", "item": {"type": "agent_message", "text": verdict}}), flush=True)
    print(json.dumps({"type": "turn.completed", "usage": {"input_tokens": 900, "cached_input_tokens": 0, "output_tokens": 40}}))
    sys.exit(0)
# The whole unacknowledged inbox: a wake may cover routine notices it only counts.
for m in call("mail.inbox", {"address": ship, "limit": 1000})["messages"]:
    if m["kind"] == "request" and m["state"] == "open":
        call("mail.send", {"from": ship, "to": m["from"], "kind": "reply", "correlation": m["id"], "body": "handled by " + ship})
through = int(re.search(r"through seq (\d+)", prompt).group(1))
# One CLI call per focus: a wake over hundreds of routine threads would spend
# a minute or more in them on a loaded runner; a test of coverage turns them off.
if mode != "no-notes":
    for focus in re.findall(r"^## (\S+)$", prompt, re.M):
        call("note.put", {"ship": ship, "focus": focus, "body": "Seen through seq %d" % through})
call("mail.ack", {"address": ship, "through_seq": through})
print(json.dumps({"type": "turn.completed", "usage": {"input_tokens": 1200, "cached_input_tokens": 1000, "output_tokens": 80}}))
"#;

pub struct Fleet {
    _temp: tempfile::TempDir,
    pub root: PathBuf,
    pub home: PathBuf,
    pub store: Store,
}

impl Fleet {
    pub fn new(settings: Value) -> Self {
        let temp = tempdir();
        let root = fs::canonicalize(temp.path()).unwrap();
        let repo = root.join("repo");
        fs::create_dir(&repo).unwrap();
        let home = root.join("home");
        let mut store = Store::open(&home).unwrap();
        fs::write(
            home.join("config.json"),
            json!({"resource_home": root.join("ledger")}).to_string(),
        )
        .unwrap();
        fs::write(root.join("adapter.py"), ADAPTER).unwrap();
        fs::write(root.join("mate.py"), MATE).unwrap();
        fs::write(
            root.join("manifest.json"),
            json!({"schema_version":1,"id":"codex","executable":["python3", root.join("adapter.py")],"timeout_ms":5000,"max_output_bytes":1_048_576}).to_string(),
        )
        .unwrap();
        dispatch(
            &mut store,
            "charter.put",
            json!({"id":"fleet","objective":"Answer mail","repo":repo,"min_free_bytes":0,"job_timeout_secs":60}),
        )
        .unwrap();
        // Crew memory and the free-memory and free-disk floors are admission's
        // concern, tested apart. These fleets run several crews at once on
        // whatever memory and temporary-directory space the test machine has,
        // so neither floor nor a crew's estimate may refuse them.
        dispatch(
            &mut store,
            "resource.configure",
            json!({"min_available_memory_bytes":0,"min_free_disk_bytes":0,"crew_default_memory_bytes":1u64 << 20,"crew_memory_bytes":{"codex":1u64 << 20}}),
        )
        .unwrap();
        for (task, owner) in [
            ("apps-duty", "apps"),
            ("infra-duty", "infra"),
            ("infra-repair", "infra"),
        ] {
            dispatch(
                &mut store,
                "task.create",
                json!({"id":task,"charter_id":"fleet","owner":owner,"title":format!("{owner} duty")}),
            )
            .unwrap();
        }
        dispatch(
            &mut store,
            "ship.put",
            json!({"name":"apps","scope":"apps"}),
        )
        .unwrap();
        let mut fleet = Self {
            _temp: temp,
            root,
            home,
            store,
        };
        fleet.configure(settings);
        fleet
    }
    pub fn configure(&mut self, settings: Value) {
        let mut profile = json!({
            "manifest": self.root.join("manifest.json"), "duty_task": "infra-duty",
            "options": {"mate": self.root.join("mate.py"), "mode": self.root.join("mode"), "log": self.root.join("prompts.log")},
        });
        for (key, value) in settings.as_object().unwrap() {
            profile[key] = value.clone();
        }
        dispatch(
            &mut self.store,
            "ship.put",
            json!({"name":"infra","scope":"deploys","first_mate":profile}),
        )
        .unwrap();
    }
    pub fn mode(&self, mode: &str) {
        fs::write(self.root.join("mode"), mode).unwrap();
    }
    pub fn send(&mut self, args: Value) -> Value {
        dispatch(&mut self.store, "mail.send", args).unwrap()
    }
    /// A privateer command on this fleet's home, isolated from the user's
    /// state, as the operator.
    pub fn command(&self) -> Command {
        let mut command = isolated(&self.root);
        command.arg("--home").arg(&self.home);
        command
    }
    /// Run a JSON command, as the agent `as_agent` when given.
    pub fn run(&self, command: &str, args: Value, as_agent: Option<&str>) -> Output {
        let mut cli = self.command();
        cli.args([command, "--json", &args.to_string()]);
        if let Some(agent) = as_agent {
            cli.env("PRIVATEER_ROLE", "agent")
                .env("PRIVATEER_ACTOR", agent);
        }
        cli.output().unwrap()
    }
    pub fn ok(&self, command: &str, args: Value, as_agent: Option<&str>) -> Value {
        let output = self.run(command, args, as_agent);
        assert!(
            output.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    /// The error a refused command reports.
    pub fn refused(&self, command: &str, args: Value, as_agent: Option<&str>) -> String {
        let output = self.run(command, args, as_agent);
        assert!(!output.status.success(), "{command} was not refused");
        String::from_utf8_lossy(&output.stderr).into_owned()
    }
    pub fn tick(&self) -> Value {
        let output = self
            .command()
            .args(["serve", "--json"])
            .arg(json!({"once": true}).to_string())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "serve: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    pub fn infra(report: &Value) -> Value {
        report["ships"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["ship"] == "infra")
            .cloned()
            .unwrap()
    }
    pub fn wakes(&mut self) -> Vec<Value> {
        dispatch(&mut self.store, "wake.list", json!({"ship":"infra"})).unwrap()["wakes"]
            .as_array()
            .unwrap()
            .clone()
    }
    /// Tick until the latest wake is no longer running.
    pub fn settle(&mut self) -> Value {
        // Condition-based: a passing wake returns at once. The bound only
        // matters on a loaded runner, where a fake harness can run for ~20 s.
        let until = Instant::now() + Duration::from_secs(60);
        loop {
            self.tick();
            let latest = self.wakes()[0].clone();
            if latest["status"] != "running" {
                return latest;
            }
            assert!(Instant::now() < until, "wake never finished: {latest}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    pub fn prompts(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("prompts.log"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str::<String>(l).unwrap())
            .collect()
    }
}

impl Drop for Fleet {
    /// Nothing a fixture starts outlives it. A resident dispatcher, a door
    /// supervisor or a payload that detached into its own session still
    /// refers to this fleet's unique temporary root in its command line or
    /// working directory; each such process is ended here by pid, before the
    /// root itself is deleted. Only this fleet's own processes can match that
    /// path, and nothing is ever signalled by name.
    fn drop(&mut self) {
        end_processes_referring_to(&self.root);
    }
}

pub fn now_settings() -> Value {
    json!({"batch_secs":0,"max_delay_secs":0,"retry_backoff_secs":0})
}

/// A resident process a fixture started: a dispatcher, a door supervisor or
/// another long-running child. Dropping it stops the process with the group
/// it leads and waits for it, then ends anything that detached from it into
/// its own session, so nothing the fixture started outlives the test. A
/// process is only ever signalled by a pid this fixture holds or after
/// `/proc` shows the process refers to this fixture's own temporary root:
/// never by name.
pub struct Resident {
    child: Child,
    root: PathBuf,
}

impl Resident {
    /// Start `command` at the head of its own process group, so a group
    /// signal can only reach this child and its own descendants.
    pub fn spawn(command: Command, root: &Path) -> Resident {
        Self::try_spawn(command, root).expect("cannot start the resident process")
    }

    /// The same, for a fixture whose executable may briefly refuse exec
    /// (a copy another fork still holds writable): the caller retries the
    /// transient error itself.
    pub fn try_spawn(command: Command, root: &Path) -> io::Result<Resident> {
        use std::os::unix::process::CommandExt;
        let mut command = command;
        command.process_group(0);
        Ok(Resident {
            child: command.spawn()?,
            root: root.to_path_buf(),
        })
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// The child's own exit status, if it has already ended.
    pub fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    /// Wait for the process to end by itself: no signal, no sweep. For a
    /// process something else stops and restarts while the test keeps
    /// watching what it left running; the full stop still happens on drop.
    pub fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        self.child.wait()
    }

    /// Stop this process and anything it left behind, and wait for both.
    pub fn stop(&mut self) {
        stop_resident(&mut self.child, &self.root);
    }
}

impl Drop for Resident {
    fn drop(&mut self) {
        self.stop();
    }
}

/// End a process this fixture started and wait for it: a graceful SIGTERM
/// to the process and the group it leads, then SIGKILL. Afterwards every
/// process that detached into its own session and still refers to `root` is
/// ended the same way, so a fixture's teardown cannot leave an orphan behind
/// however the child was stopped.
pub fn stop_resident(child: &mut Child, root: &Path) {
    if child
        .try_wait()
        .map(|status| status.is_none())
        .unwrap_or(false)
    {
        let pid = child.id() as libc::pid_t;
        for number in [libc::SIGTERM, libc::SIGKILL] {
            // SAFETY: signalling a pid this fixture itself spawned, and the
            // group that pid leads; an error means the process is already
            // gone, which is the outcome being waited for. Never asserts: a
            // drop path must not panic while another failure unwinds.
            unsafe {
                libc::kill(-pid, number);
                libc::kill(pid, number);
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            while child
                .try_wait()
                .map(|status| status.is_none())
                .unwrap_or(false)
            {
                if Instant::now() >= deadline {
                    eprintln!(
                        "test fixture: resident process {pid} did not stop after signal {number}"
                    );
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if child
                .try_wait()
                .map(|status| status.is_some())
                .unwrap_or(false)
            {
                break;
            }
        }
    }
    let _ = child.wait();
    end_processes_referring_to(root);
}

/// Running processes whose command line or working directory refers to
/// `root`, by pid. A fixture's temporary root is unique, so only processes
/// that fixture itself started can be listed; nothing is matched by name.
/// A zombie has no command line and no working directory, so a process the
/// fixture already ended and whose parent has not reaped it is not listed.
/// Linux only: there is no `/proc` to scan elsewhere, so nothing is found.
pub fn processes_referring_to(root: &Path) -> Vec<u32> {
    if cfg!(not(target_os = "linux")) {
        return Vec::new();
    }
    let mine = std::process::id();
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == mine || pid < 2 {
            continue;
        }
        if process_refers_to(pid, root) {
            found.push(pid);
        }
    }
    found
}

/// Whether one running process's command line or working directory refers to
/// `root`. A zombie reports neither, so a process whose parent has not
/// reaped it yet counts as gone. Checked again immediately before a signal,
/// so a pid that exited and was reused in between can never be signalled.
fn process_refers_to(pid: u32, root: &Path) -> bool {
    if cfg!(not(target_os = "linux")) {
        return false;
    }
    let refers = |path: &Path| path == root || path.starts_with(root);
    let dir = Path::new("/proc").join(pid.to_string());
    let cmdline = fs::read(dir.join("cmdline")).unwrap_or_default();
    let named = cmdline
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .any(|argument| refers(Path::new(OsStr::from_bytes(argument))));
    let working = fs::read_link(dir.join("cwd"))
        .map(|cwd| refers(&cwd))
        .unwrap_or(false);
    named || working
}

/// End every running process that refers to `root`: a fixture's own leftover
/// that detached into its own session, matched by the unique path of the
/// fixture's temporary home and signalled by pid. SIGTERM first, SIGKILL
/// after a grace period; a process that outlives both is reported, not
/// awaited forever.
pub fn end_processes_referring_to(root: &Path) {
    for number in [libc::SIGTERM, libc::SIGKILL] {
        // Identity re-checked per pid just before each signal, so an exited
        // pid the kernel reused for an unrelated process is never signalled.
        let found: Vec<u32> = processes_referring_to(root)
            .into_iter()
            .filter(|pid| process_refers_to(*pid, root))
            .collect();
        if found.is_empty() {
            return;
        }
        for pid in &found {
            // SAFETY: `/proc` just showed this pid's command line or working
            // directory lies under this fixture's unique temporary root, so
            // the process is this fixture's own; an error means it is already
            // gone, which is the outcome being waited for.
            unsafe { libc::kill(*pid as libc::pid_t, number) };
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !processes_referring_to(root).is_empty() {
            if Instant::now() >= deadline {
                eprintln!(
                    "test fixture: processes {found:?} referring to {} survived signal {number}",
                    root.display()
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
