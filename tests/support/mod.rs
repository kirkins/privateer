//! The scripted first mate and its fleet, shared by the activation and crew
//! tests. Every subprocess gets this fleet's own state: its home, resource
//! ledger, fleet registry and HOME, and no service manager.
#![allow(dead_code)]
use privateer::{dispatch, store::Store};
use serde_json::{Value, json};
use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_privateer");

/// A command that inherits no Privateer settings from whoever runs the
/// tests. A crew job running `cargo test` has PRIVATEER_ROLE, _ACTOR and
/// _JOB set, which would otherwise make every test's subprocess its agent.
/// Tests that need a setting set it explicitly afterwards.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("PRIVATEER_") {
            command.env_remove(name);
        }
    }
    command
}

/// Resolve the interpreter before changing HOME for a fixture. A shebang
/// through `env python3` can otherwise enter a version-manager shim which
/// refuses the isolated HOME's untrusted configuration instead of the script.
pub fn python() -> PathBuf {
    let output = command("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
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
if mode.startswith("sleep"):
    time.sleep(float(mode.split()[1]))
def call(command, args):
    out = subprocess.run(cli + [command, "--json", json.dumps(args)], capture_output=True, text=True, check=True)
    return json.loads(out.stdout)
errand = re.search(r"^Errand (errand-\S+)", prompt, re.M)
if errand:
    # An errand ends with its verdict as the final reply, as a model would.
    verdict = "Reviewed: %s. Brief seen: %s" % (errand.group(1), "the cache" in prompt)
    print(json.dumps({"type": "item.completed", "item": {"type": "agent_message", "text": verdict}}), flush=True)
    print(json.dumps({"type": "turn.completed", "usage": {"input_tokens": 900, "cached_input_tokens": 0, "output_tokens": 40}}))
    sys.exit(0)
for m in call("mail.inbox", {"address": ship})["messages"]:
    if m["kind"] == "request" and m["state"] == "open":
        call("mail.send", {"from": ship, "to": m["from"], "kind": "reply", "correlation": m["id"], "body": "handled by " + ship})
through = int(re.search(r"through seq (\d+)", prompt).group(1))
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
        let temp = tempfile::tempdir().unwrap();
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
        let user = self.root.join("user");
        fs::create_dir_all(&user).unwrap();
        let mut command = command(BIN);
        command
            .arg("--home")
            .arg(&self.home)
            .env("HOME", user)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH")
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env("PRIVATEER_FLEET", self.root.join("fleet.sqlite"))
            .env("PRIVATEER_SERVICE", "none")
            .env_remove("XDG_STATE_HOME");
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
        let until = Instant::now() + Duration::from_secs(20);
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

pub fn now_settings() -> Value {
    json!({"batch_secs":0,"max_delay_secs":0,"retry_backoff_secs":0})
}
