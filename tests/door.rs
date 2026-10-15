//! Front doors: a scripted adapter under the real `privateer door run`
//! supervisor, on a two-ship fleet with its own registry and HOME.
mod support;

use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");

// Logs every line it is sent, acknowledges mail unless its body says "hold",
// and asks the runtime what a phone would. Exits when its input closes.
const ADAPTER: &str = r#"import json, os, sys
log = open(os.environ["FAKE_DOOR_LOG"], "a")
def note(entry):
    log.write(json.dumps(entry) + "\n"); log.flush()
hello = json.loads(sys.stdin.readline())
note({"hello": hello, "pid": os.getpid(), "env": sorted(k for k in os.environ if k.startswith("PRIVATEER_"))})
def ask(ident, command, args):
    print(json.dumps({"type": "request", "id": ident, "command": command, "args": args}), flush=True)
ask("s1", "send", {"body": "from the phone"})
ask("s2", "ships", {})
ask("s3", "status", {})
ask("s4", "waiting", {})
ask("s5", "send", {"to": "nobody", "body": "lost"})
ask("s6", "send", {"to": "ops", "kind": "decision", "body": "not mine"})
for line in sys.stdin:
    message = json.loads(line)
    note(message)
    if message["type"] == "mail" and "hold" not in message["mail"]["body"]:
        print(json.dumps({"type": "delivered", "ref": message["ref"]}), flush=True)
"#;

struct Fleet {
    _temp: tempfile::TempDir,
    root: PathBuf,
    log: PathBuf,
    path: String,
}

impl Fleet {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("codex"), "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o755)).unwrap();
        let adapter = root.join("adapter");
        fs::create_dir(&adapter).unwrap();
        fs::write(adapter.join("door.py"), ADAPTER).unwrap();
        fs::write(
            adapter.join("manifest.json"),
            json!({"schema_version": 1, "kind": "front-door", "id": "fake",
                "executable": ["python3", "door.py"]})
            .to_string(),
        )
        .unwrap();
        let fleet = Self {
            log: root.join("door.jsonl"),
            path: format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
            ),
            _temp: temp,
            root,
        };
        for (ship, liaison) in [("liaison", true), ("ops", false)] {
            let folder = fleet.root.join("ships").join(ship);
            fs::create_dir_all(&folder).unwrap();
            let mut args = vec!["init", "--ship", ship, "--harness", "codex"];
            if liaison {
                args.push("--liaison");
            }
            fleet.ok_in(&folder, &args);
        }
        fleet
    }

    fn command(&self, dir: &Path) -> Command {
        let mut command = support::command(BIN);
        command
            .current_dir(dir)
            .env("PATH", &self.path)
            .env("HOME", &self.root)
            .env("PRIVATEER_FLEET", self.root.join("state/fleet.sqlite"))
            .env("PRIVATEER_SERVICE", "none")
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env(
                "PRIVATEER_PLUGINS",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins"),
            )
            .env("FAKE_DOOR_LOG", &self.log)
            .stdin(Stdio::null());
        command
    }

    fn run_in(&self, dir: &Path, args: &[&str]) -> Output {
        self.command(dir).args(args).output().unwrap()
    }

    fn ok_in(&self, dir: &Path, args: &[&str]) -> String {
        let output = self.run_in(dir, args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        self.ok_in(&self.root, args)
    }

    fn refused(&self, args: &[&str]) -> String {
        let output = self.run_in(&self.root, args);
        assert!(!output.status.success(), "{args:?} should be refused");
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    fn home(&self, ship: &str) -> PathBuf {
        self.root.join("ships").join(ship).join(".privateer")
    }

    fn call(&self, ship: &str, command: &str, args: Value) -> Value {
        let home = self.home(ship);
        let out = self.ok(&[
            "--home",
            home.to_str().unwrap(),
            command,
            "--json",
            &args.to_string(),
        ]);
        serde_json::from_str(&out).unwrap()
    }

    fn config(&self, mode: u32) -> PathBuf {
        let path = self.root.join("door.toml");
        fs::write(&path, "token = \"x\"\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    fn add(&self) {
        let manifest = self.root.join("adapter/manifest.json");
        let config = self.config(0o600);
        self.ok(&[
            "door",
            "add",
            "fake",
            "--manifest",
            manifest.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
        ]);
    }

    fn supervise(&self) -> Child {
        self.command(&self.root)
            .args(["door", "run", "fake"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn entries(&self) -> Vec<Value> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn wait<T>(&self, what: &str, mut found: impl FnMut(&[Value]) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(value) = found(&self.entries()) {
                return value;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {:#?}",
                self.entries()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn response<'a>(entries: &'a [Value], id: &str) -> Option<&'a Value> {
    entries
        .iter()
        .rev()
        .find(|e| e["type"] == "response" && e["id"] == id)
}

/// Mail bodies the adapter was sent in its `session`th run (from 0).
fn mail_bodies(entries: &[Value], session: usize) -> Vec<String> {
    let Some((start, _)) = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.get("hello").is_some())
        .nth(session)
    else {
        return vec![];
    };
    entries[start + 1..]
        .iter()
        .take_while(|e| e.get("hello").is_none())
        .filter(|e| e["type"] == "mail")
        .map(|e| e["mail"]["body"].as_str().unwrap().to_owned())
        .collect()
}

fn alive(pid: i64) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn stop(mut supervisor: Child) {
    unsafe { libc::kill(supervisor.id() as libc::pid_t, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(20);
    while supervisor.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "supervisor did not stop");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn registering_a_front_door_is_the_captains_act() {
    let fleet = Fleet::new();
    let manifest = fleet.root.join("adapter/manifest.json");
    let manifest = manifest.to_str().unwrap();
    // A door speaks as the captain: agents cannot register one.
    let output = fleet
        .command(&fleet.root)
        .args(["door", "add", "fake", "--manifest", manifest])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "ops")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("operator command"));

    let harness = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/codex/manifest.json");
    let error = fleet.refused(&[
        "door",
        "add",
        "fake",
        "--manifest",
        harness.to_str().unwrap(),
    ]);
    assert!(error.contains("not a front-door plugin"), "{error}");

    let public = fleet.config(0o644);
    let error = fleet.refused(&[
        "door",
        "add",
        "fake",
        "--manifest",
        manifest,
        "--config",
        public.to_str().unwrap(),
    ]);
    assert!(error.contains("private"), "{error}");

    let error = fleet.refused(&[
        "door",
        "add",
        "fake",
        "--manifest",
        manifest,
        "--to",
        "nobody",
    ]);
    assert!(error.contains("no active ship named nobody"), "{error}");

    let error = fleet.refused(&["door", "add", "telegram"]);
    assert!(error.contains("--config"), "{error}");

    let private = fleet.config(0o600);
    let private = private.to_str().unwrap();
    fleet.ok(&[
        "door",
        "add",
        "fake",
        "--manifest",
        manifest,
        "--config",
        private,
        "--to",
        "ops",
    ]);
    let listed = fleet.ok(&["door", "list"]);
    assert!(listed.contains("fake: not running"), "{listed}");
    assert!(listed.contains("messages go to ops"), "{listed}");
    let removed = fleet.ok(&["door", "remove", "fake"]);
    assert!(removed.contains("removed"), "{removed}");
    assert!(fleet.ok(&["door", "list"]).contains("No front doors"));
}

#[test]
fn harness_planning_refuses_a_front_door_manifest() {
    let fleet = Fleet::new();
    let telegram = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/telegram/manifest.json");
    let home = fleet.home("ops");
    let output = fleet.run_in(
        &fleet.root,
        &[
            "--home",
            home.to_str().unwrap(),
            "plugin.validate",
            "--json",
            &json!({"task_id": "ops-duty", "manifest": telegram}).to_string(),
        ],
    );
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("not a harness adapter"), "{error}");
}

#[test]
fn a_front_door_carries_captain_mail_both_ways_at_least_once() {
    let fleet = Fleet::new();
    fleet.add();
    // Unread captain mail from before the door existed still reaches it.
    let first = fleet.call(
        "ops",
        "mail.send",
        json!({"from": "ops", "to": "captain", "kind": "notice", "body": "first"}),
    );
    let held = fleet.call(
        "ops",
        "mail.send",
        json!({"from": "ops", "to": "captain", "kind": "request", "body": "hold this"}),
    );
    let supervisor = fleet.supervise();
    let entries = fleet.wait("both messages and every response", |e| {
        (mail_bodies(e, 0).len() >= 2 && response(e, "s6").is_some()).then(|| e.to_vec())
    });
    let hello = &entries[0]["hello"];
    assert_eq!(hello["version"], 1);
    assert_eq!(hello["recipient"], "liaison");
    assert!(
        hello["config_path"]
            .as_str()
            .unwrap()
            .ends_with("door.toml")
    );
    // The adapter is no ship and no agent: it inherits no Privateer role.
    assert_eq!(entries[0]["env"], json!([]));
    assert_eq!(mail_bodies(&entries, 0), ["first", "hold this"]);
    let mail = entries.iter().find(|e| e["type"] == "mail").unwrap();
    assert_eq!(mail["ship"], "ops");
    assert_eq!(mail["mail"]["id"], first["id"]);

    // Sent without naming a ship, the captain's message goes to the liaison,
    // in the liaison's home, as a request from the captain.
    let sent = response(&entries, "s1").unwrap();
    assert_eq!(sent["ok"], true, "{sent}");
    assert_eq!(sent["result"]["ship"], "liaison");
    let inbox = fleet.call("liaison", "mail.inbox", json!({"address": "liaison"}));
    let message = &inbox["messages"][0];
    assert_eq!(message["from"], "captain");
    assert_eq!(message["kind"], "request");
    assert_eq!(message["body"], "from the phone");

    let ships = &response(&entries, "s2").unwrap()["result"];
    let names: Vec<&str> = ships["ships"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["liaison", "ops"]);
    assert_eq!(ships["recipient"], "liaison");
    let status = &response(&entries, "s3").unwrap()["result"]["ships"];
    let ops = status
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "ops")
        .unwrap();
    assert_eq!(ops["waiting_on_captain"], 1, "{status}");
    let waiting = &response(&entries, "s4").unwrap()["result"]["waiting"];
    assert_eq!(waiting[0]["mail"]["id"], held["id"]);
    let lost = response(&entries, "s5").unwrap();
    assert_eq!(lost["ok"], false);
    assert!(
        lost["error"]
            .as_str()
            .unwrap()
            .contains("no active ship named nobody")
    );
    let decision = response(&entries, "s6").unwrap();
    assert!(
        decision["error"]
            .as_str()
            .unwrap()
            .contains("requests, notices and replies")
    );

    // One supervisor per door.
    let second = fleet.run_in(&fleet.root, &["door", "run", "fake"]);
    assert!(String::from_utf8_lossy(&second.stderr).contains("already running"));

    // Stopping the supervisor ends the adapter.
    let adapter = entries[0]["pid"].as_i64().unwrap();
    stop(supervisor);
    assert!(!alive(adapter), "adapter outlived its supervisor");

    // Restarted, the door gets what it never acknowledged, plus new mail,
    // and not what it did acknowledge.
    fleet.call(
        "ops",
        "mail.send",
        json!({"from": "ops", "to": "captain", "kind": "notice", "body": "third"}),
    );
    let supervisor = fleet.supervise();
    let resumed = fleet.wait("mail after the restart", |e| {
        let bodies = mail_bodies(e, 1);
        (bodies.len() >= 2).then_some(bodies)
    });
    assert_eq!(resumed, ["hold this", "third"]);

    // An adapter that dies is started again.
    let adapter = fleet.wait("the second adapter", |e| {
        e.iter()
            .filter(|e| e.get("hello").is_some())
            .nth(1)
            .map(|e| e["pid"].as_i64().unwrap())
    });
    unsafe { libc::kill(adapter as libc::pid_t, libc::SIGKILL) };
    let restarted = fleet.wait("a restarted adapter", |e| {
        e.iter()
            .filter(|e| e.get("hello").is_some())
            .nth(2)
            .map(|e| e["pid"].as_i64().unwrap())
    });
    assert_ne!(restarted, adapter);
    stop(supervisor);
    assert!(!alive(restarted));
    let log = fs::read_to_string(fleet.root.join("state/doors/fake/door.log")).unwrap();
    assert!(log.contains("adapter exited"), "{log}");
}

#[test]
fn door_start_and_stop_run_the_supervisor_in_the_background() {
    let fleet = Fleet::new();
    fleet.add();
    let detached = |args: &[&str]| {
        let output = fleet
            .command(&fleet.root)
            .args(args)
            .env("PRIVATEER_SERVICE", "detached")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    assert!(detached(&["door", "start", "fake"]).contains("started in the background"));
    let adapter = fleet.wait("the adapter", |e| {
        e.iter()
            .find(|e| e.get("hello").is_some())
            .map(|e| e["pid"].as_i64().unwrap())
    });
    assert!(detached(&["door", "start", "fake"]).contains("already running"));
    assert!(detached(&["door", "status", "fake"]).contains("fake: running"));
    let stopped = detached(&["door", "stop", "fake"]);
    assert!(stopped.contains("Stopped the front door fake"), "{stopped}");
    assert!(!alive(adapter), "adapter outlived its supervisor");
    assert!(detached(&["door", "status", "fake"]).contains("fake: not running"));
    assert!(detached(&["door", "stop", "fake"]).contains("was not running"));
}

#[test]
fn door_start_installs_a_user_service_where_systemd_runs() {
    let fleet = Fleet::new();
    fleet.add();
    let calls = fleet.root.join("systemctl.log");
    let systemctl = fleet.root.join("bin/systemctl");
    fs::write(
        &systemctl,
        format!("#!/bin/sh\necho \"$@\" >> {}\n", calls.display()),
    )
    .unwrap();
    fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();
    let config = fleet.root.join("config");
    let run = |verb: &str| {
        let output = fleet
            .command(&fleet.root)
            .args(["door", verb, "fake"])
            .env("PRIVATEER_SERVICE", "systemd")
            .env("PRIVATEER_SYSTEMCTL", &systemctl)
            .env("XDG_CONFIG_HOME", &config)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{verb}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let started = run("start");
    assert!(
        started.contains("installed as user service privateer-door-fake-"),
        "{started}"
    );
    let units: Vec<_> = fs::read_dir(config.join("systemd/user"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(units.len(), 1);
    let unit = fs::read_to_string(&units[0]).unwrap();
    assert!(
        unit.contains(&format!("ExecStart=\"{BIN}\" door run fake\n")),
        "{unit}"
    );
    let registry = fleet.root.join("state/fleet.sqlite");
    assert!(
        unit.contains(&format!(
            "Environment=\"PRIVATEER_FLEET={}\"",
            registry.display()
        )),
        "{unit}"
    );
    let stopped = run("stop");
    assert!(
        stopped.contains("Stopped and removed user service"),
        "{stopped}"
    );
    assert!(
        fs::read_dir(config.join("systemd/user"))
            .unwrap()
            .next()
            .is_none()
    );
    let log = fs::read_to_string(&calls).unwrap();
    assert!(
        log.contains("--user enable --now privateer-door-fake-"),
        "{log}"
    );
    assert!(
        log.contains("--user disable --now privateer-door-fake-"),
        "{log}"
    );
}

#[test]
fn the_telegram_door_works_end_to_end_against_a_fake_bot_api() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/telegram_door.py");
    let output = support::command("python3")
        .arg(script)
        .arg(BIN)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
