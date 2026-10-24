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
    # The captain taps Approve on anything that asks for an approval.
    # A tap carries the hash the phone showed; a stale one answers nothing.
    if message["type"] == "mail" and message["mail"].get("request_kind"):
        mail = message["mail"]
        shown = ((mail.get("payload") or {}).get("privileged") or {}).get("hash", "")[:8]
        if shown:
            ask("stale-" + mail["id"], "approval",
                {"id": mail["id"], "action": "grant", "hash": "00000000"})
            ask("bare-" + mail["id"], "approval", {"id": mail["id"], "action": "grant"})
        ask("approve-" + mail["id"], "approval",
            {"id": mail["id"], "action": "grant", "hash": shown or None})
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
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH")
            .env("PRIVATEER_GH", self.root.join("no-gh"))
            .env("PRIVATEER_FLEET", self.root.join("state/fleet.sqlite"))
            .env("PRIVATEER_SERVICE", "none")
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env(
                "PRIVATEER_PLUGINS",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins"),
            )
            .env("FAKE_DOOR_LOG", &self.log)
            // Only a fleet linked to another machine reaches for it.
            .env("PRIVATEER_SSH_EXECUTABLE", self.root.join("fake-ssh"))
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

    fn wait<T>(&self, what: &str, found: impl FnMut(&[Value]) -> Option<T>) -> T {
        self.wait_within(30, what, found)
    }

    fn wait_within<T>(
        &self,
        secs: u64,
        what: &str,
        mut found: impl FnMut(&[Value]) -> Option<T>,
    ) -> T {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(value) = found(&self.entries()) {
                return value;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {:#?}\ndoor log:\n{}",
                self.entries(),
                fs::read_to_string(self.root.join("state/doors/fake/door.log")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Wait until the door's delivery cursor in each ship's home covers the
    /// mail events given. The adapter's `delivered` lines are read by the
    /// supervisor after the mail events it answers, so a supervisor stopped
    /// as soon as the events appear can exit with the cursor still behind.
    fn wait_delivered(&self, mail: &[Value]) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let covered = mail.iter().all(|e| {
                let seq = e["mail"]["seq"].as_i64().unwrap();
                let path = self.home(e["ship"].as_str().unwrap()).join("privateer.sqlite");
                let conn = rusqlite::Connection::open(path).unwrap();
                conn.busy_timeout(Duration::from_secs(5)).unwrap();
                let through: i64 = conn
                    .query_row(
                        "SELECT COALESCE((SELECT through_seq FROM door_cursor WHERE door='fake'), 0)",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                through >= seq
            });
            if covered {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "door cursor never covered {mail:#?}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Every session, regardless of its termination path, records one reaped
/// exit status. Check the PID so an earlier graceful stop cannot mask a gap.
fn adapter_exit(log: &str, pid: i64) -> &str {
    let prefix = format!("supervisor: adapter exited: pid={pid}, status=");
    let lines: Vec<_> = log.lines().filter(|line| line.contains(&prefix)).collect();
    assert_eq!(lines.len(), 1, "expected one exit record for {pid}: {log}");
    lines[0]
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

#[test]
fn upgrade_door_previews_requires_confirmation_and_consumes_the_confirmation_once() {
    let fleet = Fleet::new();
    fs::write(
        fleet.root.join("adapter/door.py"),
        r#"import json, os, sys
log = open(os.environ['FAKE_DOOR_LOG'], 'a')
json.loads(sys.stdin.readline())
def ask(i, a):
    print(json.dumps(dict(type='request', id=i, command='upgrade', args=a)), flush=True)
ask('preview', dict(action='plan', commit='b'*40))
for line in sys.stdin:
    message=json.loads(line)
    log.write(json.dumps(message)+'\n'); log.flush()
    if message.get('id') == 'preview':
        assert message['ok'], message
        token=message['result']['confirmation']
        ask('confirmed', dict(action='confirm', confirmation=token))
        ask('replayed', dict(action='confirm', confirmation=token))
"#,
    )
    .unwrap();
    fleet.add();
    let supervisor = fleet.supervise();
    let entries = fleet.wait("preview, confirmation and replay refusal", |entries| {
        (response(entries, "replayed").is_some()).then(|| entries.to_vec())
    });
    let preview = response(&entries, "preview").unwrap();
    assert_eq!(preview["result"]["dry_run"], true);
    assert_eq!(preview["result"]["machines"][0]["name"], "local");
    let confirmed = response(&entries, "confirmed").unwrap();
    assert_eq!(confirmed["ok"], true, "{confirmed}");
    assert!(confirmed["result"]["id"].is_string());
    assert_eq!(response(&entries, "replayed").unwrap()["ok"], false);
    let state = fleet
        .root
        .join("data/privateer/upgrade-linked")
        .join(format!(
            "{}.json",
            confirmed["result"]["id"].as_str().unwrap()
        ));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if fs::read(&state)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|s| !s["outcome"].is_null())
        {
            break;
        }
        assert!(Instant::now() < deadline, "fixture round did not finish");
        std::thread::sleep(Duration::from_millis(30));
    }
    stop(supervisor);
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
    assert!(adapter_exit(&log, adapter).contains("signal: 9"), "{log}");
    // The restarted adapter exits on its own once stdin closes, or is
    // SIGTERMed if a slow runner keeps it past the stop grace; either way
    // its exit must be logged.
    let restarted_exit = adapter_exit(&log, restarted);
    assert!(
        restarted_exit.contains("exit status: 0") || restarted_exit.contains("signal: 15"),
        "{log}"
    );
}

#[test]
fn an_adapter_with_closed_input_still_has_its_exit_logged() {
    let fleet = Fleet::new();
    fs::write(
        fleet.root.join("adapter/door.py"),
        r#"import json, os, sys
json.loads(sys.stdin.readline())
log = open(os.environ['FAKE_DOOR_LOG'], 'a')
log.write(json.dumps({'hello': True, 'pid': os.getpid()}) + '\n'); log.flush()
os.close(0)
# Keep stdout open and the child alive. Responses eventually discover the
# disconnected input writer, ending the session through emit(), not waitid().
while True:
    print(json.dumps({'type': 'request', 'id': 'closed', 'command': 'ships', 'args': {}}), flush=True)
"#,
    )
    .unwrap();
    fleet.add();
    let supervisor = fleet.supervise();
    let first = fleet.wait("restart after closed input", |entries| {
        (entries.len() >= 2).then(|| entries[0]["pid"].as_i64().unwrap())
    });
    stop(supervisor);
    let log = fs::read_to_string(fleet.root.join("state/doors/fake/door.log")).unwrap();
    assert!(log.contains("supervisor: adapter input closed"), "{log}");
    assert!(adapter_exit(&log, first).contains("signal: 15"), "{log}");
    assert!(!alive(first));
}

#[test]
fn an_adapter_with_closed_output_still_has_its_exit_logged() {
    let fleet = Fleet::new();
    fs::write(
        fleet.root.join("adapter/door.py"),
        r#"import json, os, sys
json.loads(sys.stdin.readline())
log = open(os.environ['FAKE_DOOR_LOG'], 'a')
log.write(json.dumps({'hello': True, 'pid': os.getpid()}) + '\n'); log.flush()
os.close(1)
# Stay alive until common cleanup closes stdin, then return a known status.
for line in sys.stdin:
    pass
sys.exit(23)
"#,
    )
    .unwrap();
    fleet.add();
    let supervisor = fleet.supervise();
    let first = fleet.wait("restart after closed output", |entries| {
        (entries.len() >= 2).then(|| entries[0]["pid"].as_i64().unwrap())
    });
    stop(supervisor);
    let log = fs::read_to_string(fleet.root.join("state/doors/fake/door.log")).unwrap();
    assert!(
        log.contains("supervisor: adapter closed its output"),
        "{log}"
    );
    assert!(
        adapter_exit(&log, first).contains("exit status: 23"),
        "{log}"
    );
    assert!(!alive(first));
}

#[test]
fn the_door_approves_a_privileged_command_but_never_a_production_approval() {
    let fleet = Fleet::new();
    fleet.add();
    let staged = fleet.call(
        "ops",
        "privileged.stage",
        json!({"from": "ops", "task_id": "ops-duty", "program": "/usr/bin/id", "args": ["-u"],
            "cwd": "/", "reason": "Check the effective user"}),
    );
    let production = fleet.call(
        "ops",
        "mail.send",
        json!({"from": "ops", "to": "captain", "kind": "request",
            "request_kind": "production_approval", "task_id": "ops-duty", "body": "Ship it?",
            "payload": {"artifact": "sha256:exact", "environment": "production"}}),
    );
    let privileged_id = staged["id"].as_str().unwrap();
    let production_id = production["id"].as_str().unwrap();
    let supervisor = fleet.supervise();
    let entries = fleet.wait("both approval responses", |e| {
        (response(e, &format!("approve-{privileged_id}")).is_some()
            && response(e, &format!("approve-{production_id}")).is_some())
        .then(|| e.to_vec())
    });
    stop(supervisor);
    let stale = response(&entries, &format!("stale-{privileged_id}")).unwrap();
    assert_eq!(stale["ok"], false, "{stale}");
    assert!(
        stale["error"]
            .as_str()
            .unwrap()
            .contains("changed since your phone showed it"),
        "{stale}"
    );
    // An answer without the hash the phone showed answers nothing.
    let bare = response(&entries, &format!("bare-{privileged_id}")).unwrap();
    assert_eq!(bare["ok"], false, "{bare}");
    assert!(
        bare["error"]
            .as_str()
            .unwrap()
            .contains("must carry the hash the phone showed"),
        "{bare}"
    );
    let granted = response(&entries, &format!("approve-{privileged_id}")).unwrap();
    assert_eq!(granted["ok"], true, "{granted}");
    assert_eq!(granted["result"]["action"], "approval.grant");
    assert!(
        granted["result"]["run"]
            .as_str()
            .unwrap()
            .ends_with(&format!("privileged run {privileged_id}")),
        "{granted}"
    );
    let request = fleet.call("ops", "mail.show", json!({"id": privileged_id}));
    assert_eq!(request["state"], "approved");
    let reply = fleet.call("ops", "mail.show", json!({"id": request["answered_by"]}));
    assert_eq!(
        reply["body"],
        "approved, waiting for the captain at the machine"
    );
    // The phone never approves a production deployment.
    let refused = response(&entries, &format!("approve-{production_id}")).unwrap();
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("only privileged commands"),
        "{refused}"
    );
    let open = fleet.call("ops", "mail.show", json!({"id": production_id}));
    assert_eq!(open["state"], "open");
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
    let unlinked = fleet.run_in(&fleet.root, &["door", "start", "fake"]);
    assert!(unlinked.status.success());
    assert!(unlinked.stderr.is_empty(), "unexpected coverage warning");
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
    assert!(unit.contains("\nKillMode=control-group\n"), "{unit}");
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

// Plays the captain's phone through an away period: acknowledges mail,
// peeks while away, then comes back with an ordinary message.
const AWAY_ADAPTER: &str = r#"import json, os, sys
log = open(os.environ["FAKE_DOOR_LOG"], "a")
def note(entry):
    log.write(json.dumps(entry) + "\n"); log.flush()
note({"hello": json.loads(sys.stdin.readline())})
def ask(ident, command, args):
    print(json.dumps({"type": "request", "id": ident, "command": command, "args": args}), flush=True)
mails = 0
for line in sys.stdin:
    message = json.loads(line)
    note(message)
    if message["type"] == "mail":
        print(json.dumps({"type": "delivered", "ref": message["ref"]}), flush=True)
        mails += 1
        if mails == 3:
            ask("held", "held", {}); ask("peek", "status", {}); ask("still", "away", {"action": "status"})
            ask("morning", "send", {"body": "good morning"})
    elif message["type"] == "response" and message["id"] == "morning":
        ask("again", "away", {"action": "start", "words": ""})
        ask("back", "away", {"action": "back"})
"#;

#[test]
fn away_holds_mail_lets_three_emergencies_through_and_summarizes_on_return() {
    let fleet = Fleet::new();
    fs::write(fleet.root.join("adapter/door.py"), AWAY_ADAPTER).unwrap();
    // One place overrides the away defaults: here, one ship may spend the
    // whole fleet's urgent budget.
    fs::write(
        fleet.root.join("state/away.json"),
        json!({"ship_breakthroughs_per_hour": 3}).to_string(),
    )
    .unwrap();
    fleet.add();
    let out = fleet.ok(&["afk", "until", "8am", "merge #9 if CI is green"]);
    assert!(
        out.contains("requests and high-urgency messages reach you at once"),
        "{out}"
    );
    let bearings = fleet.call("ops", "ship.bearings", json!({"ship":"ops"}));
    assert!(
        bearings["text"]
            .as_str()
            .unwrap()
            .contains("notices go in a digest")
    );
    let mail = |body: &str, kind: &str, urgency: &str| {
        fleet.call(
            "ops",
            "mail.send",
            json!({"from":"ops","to":"captain","kind":kind,"urgency":urgency,"body":body}),
        )
    };
    mail("routine progress", "notice", "normal");
    for n in 1..=4 {
        mail(&format!("fire {n}"), "notice", "high");
    }
    let decide = mail("decide the region", "request", "normal");

    let supervisor = fleet.supervise();
    let entries = fleet.wait("the return and the second away period", |e| {
        response(e, "back").is_some().then(|| e.to_vec())
    });
    // Three emergencies reached the phone, marked, and the request went at
    // once on a door without digests; the fourth emergency and notices waited.
    let pushed: Vec<(&str, bool, bool)> = entries
        .iter()
        .filter(|e| e["type"] == "mail")
        .map(|e| {
            (
                e["mail"]["body"].as_str().unwrap(),
                e["breakthrough"] == true,
                e["away"] == true,
            )
        })
        .collect();
    assert_eq!(
        pushed[..4],
        [
            ("fire 1", true, false),
            ("fire 2", true, false),
            ("fire 3", true, false),
            ("decide the region", false, true)
        ]
    );
    // Peeking left the captain away.
    let held: Vec<&str> = response(&entries, "held").unwrap()["result"]["held"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["body"].as_str().unwrap())
        .collect();
    assert_eq!(held, ["routine progress", "fire 4", "decide the region"]);
    assert_eq!(response(&entries, "peek").unwrap()["ok"], true);
    assert!(response(&entries, "still").unwrap()["result"]["away"].is_object());

    // The first ordinary message is the return: a digest from records at once.
    let morning = &response(&entries, "morning").unwrap()["result"];
    assert_eq!(morning["ship"], "liaison");
    let digest = morning["returned"]["digest"].as_str().unwrap();
    assert!(
        digest.contains(": 3 messages from 1 ship, 1 waiting on you, 3 emergencies sent. 1 urgent message was held over the hourly limit."),
        "{digest}"
    );
    // The liaison got the away words, then the summary request, then the message.
    let inbox = fleet.call("liaison", "mail.inbox", json!({"address":"liaison"}));
    let bodies: Vec<&str> = inbox["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["body"].as_str().unwrap())
        .collect();
    // Then the second, empty away period: its notice and a reminder of what waits.
    assert_eq!(bodies.len(), 5, "{bodies:#?}");
    assert!(bodies[3].contains("They left no instructions"));
    assert!(
        bodies[4].contains("Nothing new reached my mailbox, but 1 request still waits on me"),
        "{}",
        bodies[4]
    );
    assert!(
        bodies[0].contains("(back about 8am)") && bodies[0].contains("\"merge #9 if CI is green\"")
    );
    assert!(bodies[1].starts_with("I'm back after"));
    for line in [
        "ops notice",
        "routine progress",
        "fire 4",
        "decide the region",
        "still open",
    ] {
        assert!(
            bodies[1].contains(line),
            "{line} missing from {}",
            bodies[1]
        );
    }
    assert!(!bodies[1].contains("fire 1"));
    assert_eq!(bodies[2], "good morning");
    let request = fleet.call(
        "liaison",
        "mail.show",
        json!({"id": inbox["messages"][1]["id"]}),
    );
    assert_eq!(request["kind"], "request");
    assert_eq!(request["from"], "captain");
    let _ = decide;

    // A second, empty away period ends with an empty digest; held mail from
    // the first is never replayed, and new mail flows normally again.
    let again = &response(&entries, "back").unwrap()["result"]["returned"]["digest"];
    assert!(
        again
            .as_str()
            .unwrap()
            .contains(": 0 messages from 0 ships"),
        "{again}"
    );
    mail("after you returned", "notice", "normal");
    let later = fleet.wait("mail after the return", |e| {
        let bodies: Vec<String> = e
            .iter()
            .filter(|e| e["type"] == "mail")
            .map(|e| e["mail"]["body"].as_str().unwrap().to_owned())
            .collect();
        (bodies.len() > 4).then_some(bodies)
    });
    assert_eq!(later[4..], ["after you returned"]);
    stop(supervisor);
}

// Logs every line, acknowledges mail, and acknowledges digests unless
// `<log>.noack` exists, as a phone that cannot reach Telegram would not.
const DIGEST_ADAPTER: &str = r#"import json, os, sys
path = os.environ["FAKE_DOOR_LOG"]
log = open(path, "a")
def note(entry):
    log.write(json.dumps(entry) + "\n"); log.flush()
note({"hello": json.loads(sys.stdin.readline())})
for line in sys.stdin:
    message = json.loads(line)
    note(message)
    if message["type"] == "mail" or (message["type"] == "digest" and not os.path.exists(path + ".noack")):
        print(json.dumps({"type": "delivered", "ref": message["ref"]}), flush=True)
"#;

impl Fleet {
    fn digest_door(&self, settings: Value) {
        fs::write(self.root.join("adapter/door.py"), DIGEST_ADAPTER).unwrap();
        fs::write(
            self.root.join("adapter/manifest.json"),
            json!({"schema_version": 1, "kind": "front-door", "id": "fake",
                "executable": ["python3", "door.py"], "digests": true})
            .to_string(),
        )
        .unwrap();
        self.settings(settings);
        self.add();
    }

    fn settings(&self, settings: Value) {
        fs::write(self.root.join("state/away.json"), settings.to_string()).unwrap();
    }

    fn tell(&self, ship: &str, body: &str, kind: &str, urgency: &str) -> Value {
        self.call(
            ship,
            "mail.send",
            json!({"from": ship, "to": "captain", "kind": kind, "urgency": urgency, "body": body}),
        )
    }

    fn noack(&self, on: bool) {
        let path = self.log.with_extension("jsonl.noack");
        if on {
            fs::write(path, "").unwrap();
        } else {
            let _ = fs::remove_file(path);
        }
    }
}

/// Events of one type the adapter was sent in its `session`th run (from 0).
fn events(entries: &[Value], session: usize, kind: &str) -> Vec<Value> {
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
        .filter(|e| e["type"] == kind)
        .cloned()
        .collect()
}

fn text(digest: &Value) -> String {
    digest["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn away_sends_requests_at_once_and_digests_the_rest_with_urgent_items_loud_and_early() {
    let fleet = Fleet::new();
    fleet.digest_door(
        json!({"digest_every_mins": 600, "urgent_due_mins": 0, "ship_breakthroughs_per_hour": 1}),
    );
    // Present: a state that flips back, and generic words from two ships, are
    // news; only the same words again on one key are marked a repeat.
    let supervisor = fleet.supervise();
    for (ship, body) in [
        ("ops", "prod up"),
        ("ops", "prod down"),
        ("ops", "prod up"),
        ("liaison", "Done."),
        ("ops", "Done."),
        ("ops", "before away"),
        ("ops", "before away"),
    ] {
        fleet.tell(ship, body, "notice", "normal");
    }
    // Bounded but generous: seven mails through a supervisor under CI load.
    let present = fleet.wait_within(60, "mail while present", |e| {
        let mail = events(e, 0, "mail");
        (mail.len() == 7).then_some(mail)
    });
    // The away baseline is the door's cursor, so let the adapter's delivery
    // acks land before the supervisor stops.
    fleet.wait_delivered(&present);
    // Homes are read one after another, so compare each ship's own order.
    let again = |ship: &str| -> Vec<(String, bool)> {
        present
            .iter()
            .filter(|e| e["ship"] == ship)
            .map(|e| {
                (
                    e["mail"]["body"].as_str().unwrap().to_owned(),
                    e["again"] == true,
                )
            })
            .collect()
    };
    let told = |pairs: &[(&str, bool)]| -> Vec<(String, bool)> {
        pairs.iter().map(|(b, a)| (b.to_string(), *a)).collect()
    };
    assert_eq!(
        again("ops"),
        told(&[
            ("prod up", false),
            ("prod down", false),
            ("prod up", false),
            ("Done.", false),
            ("before away", false),
            ("before away", true)
        ])
    );
    assert_eq!(again("liaison"), told(&[("Done.", false)]));
    let repeat = present.iter().find(|e| e["again"] == true).unwrap();
    assert_eq!(repeat["quiet"], true);
    stop(supervisor);

    fleet.ok(&["afk"]);
    let first = fleet.tell("ops", "progress A", "notice", "normal");
    assert!(
        first["note"]
            .as_str()
            .unwrap()
            .contains("goes in the next digest"),
        "{first}"
    );
    fleet.tell("ops", "progress A", "notice", "normal");
    let told = fleet.tell("ops", "before away", "notice", "normal");
    assert!(
        told["note"]
            .as_str()
            .unwrap()
            .contains("The captain already has this"),
        "{told}"
    );
    fleet.tell("ops", "routine bit", "notice", "low");
    fleet.tell("ops", "decide the region", "request", "normal");
    fleet.tell("ops", "fire 1", "notice", "high");
    let over = fleet.tell("ops", "fire 2", "notice", "high");
    assert!(over.get("note").is_none(), "{over}");

    let supervisor = fleet.supervise();
    let digest = fleet.wait("the urgent digest", |e| {
        events(e, 1, "digest").first().cloned()
    });
    let lines = text(&digest);
    assert_eq!(digest["loud"], true, "{lines}");
    for wanted in [
        "Urgent, over the hourly limit",
        "fire 2",
        "progress A ×2",
        "before away",
        "Routine: 1 low-urgency update from 1 ship",
    ] {
        assert!(lines.contains(wanted), "{wanted} missing from {lines}");
    }
    for unwanted in ["prod", "decide the region", "fire 1"] {
        assert!(!lines.contains(unwanted), "{unwanted} in {lines}");
    }
    let mail = fleet.wait("mail sent at once", |e| {
        let mail = events(e, 1, "mail");
        (mail.len() >= 2).then_some(mail)
    });
    let at_once: Vec<(&str, bool, bool)> = mail
        .iter()
        .map(|e| {
            (
                e["mail"]["body"].as_str().unwrap(),
                e["away"] == true,
                e["breakthrough"] == true,
            )
        })
        .collect();
    assert_eq!(
        at_once,
        [("decide the region", true, false), ("fire 1", false, true)]
    );
    stop(supervisor);

    // After a restart with the budget free again, the over-limit item stays
    // in the digest: it is not also broken through, and the delivered digest
    // is not sent again.
    fleet.settings(json!({"digest_every_mins": 600, "ship_breakthroughs_per_hour": 5}));
    let supervisor = fleet.supervise();
    fleet.wait("the third session", |e| {
        (e.iter().filter(|e| e.get("hello").is_some()).count() == 3).then_some(())
    });
    std::thread::sleep(Duration::from_secs(3));
    let entries = fleet.entries();
    assert!(events(&entries, 2, "digest").is_empty());
    assert!(
        events(&entries, 2, "mail")
            .iter()
            .all(|e| e["mail"]["body"] != "fire 2")
    );
    stop(supervisor);
}

#[test]
fn a_digest_and_the_catch_up_are_resent_until_delivered() {
    let fleet = Fleet::new();
    fleet.digest_door(json!({"digest_every_mins": 0}));
    fleet.ok(&["afk"]);
    fleet.tell("ops", "n1", "notice", "normal");
    fleet.tell("ops", "decide the region", "request", "normal");
    fleet.noack(true);
    let supervisor = fleet.supervise();
    let first = fleet.wait("the digest", |e| events(e, 0, "digest").first().cloned());
    assert!(text(&first).contains("n1"));
    assert_eq!(first["loud"], false);
    stop(supervisor);

    fleet.noack(false);
    let supervisor = fleet.supervise();
    let again = fleet.wait("the digest again", |e| {
        events(e, 1, "digest").first().cloned()
    });
    assert_eq!(again["id"], first["id"]);
    std::thread::sleep(Duration::from_secs(2));

    // The return: one catch-up listing the open request by line, printed by
    // `privateer back` and retried by the door until acknowledged.
    fleet.noack(true);
    let back = fleet.ok(&["back"]);
    assert!(
        back.contains("Waiting on you (1):") && back.contains("decide the region"),
        "{back}"
    );
    let catch_up = fleet.wait("the catch-up", |e| {
        events(e, 1, "digest")
            .into_iter()
            .find(|d| d["kind"] == "catch_up")
    });
    assert!(text(&catch_up).contains("decide the region"));
    stop(supervisor);
    fleet.noack(false);
    let supervisor = fleet.supervise();
    let resent = fleet.wait("the catch-up again", |e| {
        events(e, 2, "digest").first().cloned()
    });
    assert_eq!(resent["id"], catch_up["id"]);
    std::thread::sleep(Duration::from_secs(2));
    stop(supervisor);

    // Delivered: nothing is sent again. Mail delivered while present is
    // not in the next away period's digest.
    let supervisor = fleet.supervise();
    fleet.tell("ops", "while present", "notice", "normal");
    fleet.wait("mail while present", |e| {
        events(e, 3, "mail")
            .iter()
            .any(|m| m["mail"]["body"] == "while present")
            .then_some(())
    });
    std::thread::sleep(Duration::from_secs(2));
    fleet.ok(&["afk"]);
    fleet.tell("ops", "second away", "notice", "normal");
    let second = fleet.wait("the second period's digest", |e| {
        events(e, 3, "digest").first().cloned()
    });
    let lines = text(&second);
    assert!(lines.contains("second away"), "{lines}");
    assert!(
        !lines.contains("while present") && !lines.contains("n1"),
        "{lines}"
    );
    stop(supervisor);
}

#[test]
fn desk_copies_and_an_unreadable_home_do_not_spoil_the_digest() {
    let fleet = Fleet::new();
    fleet.digest_door(json!({"digest_every_mins": 0}));
    fleet.ok(&["afk"]);
    for ship in ["ops", "liaison"] {
        fleet.call(
            ship,
            "mail.send",
            json!({"id": "desk-copy-1", "from": ship, "to": "captain", "kind": "notice",
                "body": "one shared fact"}),
        );
    }
    let supervisor = fleet.supervise();
    let first = fleet.wait("the digest", |e| events(e, 0, "digest").first().cloned());
    assert_eq!(
        text(&first).matches("one shared fact").count(),
        1,
        "{}",
        text(&first)
    );
    // No digest is due for hours, so ops's news waits; an over-the-limit
    // urgent item from the liaison makes one due while ops cannot be read.
    fleet.settings(
        json!({"digest_every_mins": 600, "urgent_due_mins": 0, "ship_breakthroughs_per_hour": 0}),
    );
    std::thread::sleep(Duration::from_secs(2));
    fleet.tell("ops", "ops news", "notice", "normal");
    std::thread::sleep(Duration::from_secs(2));
    // A store that refuses to open (its database became a symlink) is an
    // unreadable home; `Store::open` repairs plain permission changes.
    let database = fleet.home("ops").join("privateer.sqlite");
    let aside = database.with_extension("sqlite.aside");
    fs::rename(&database, &aside).unwrap();
    std::os::unix::fs::symlink(&aside, &database).unwrap();
    fleet.tell("liaison", "liaison news", "notice", "high");
    let second = fleet.wait("a digest without ops", |e| {
        events(e, 0, "digest").get(1).cloned()
    });
    fs::remove_file(&database).unwrap();
    fs::rename(&aside, &database).unwrap();
    assert!(text(&second).contains("liaison news"));
    assert!(!text(&second).contains("ops news"), "{}", text(&second));
    fleet.settings(json!({"digest_every_mins": 0}));
    let third = fleet.wait("ops once readable", |e| {
        events(e, 0, "digest").get(2).cloned()
    });
    assert!(text(&third).contains("ops news"), "{}", text(&third));
    stop(supervisor);
}

#[test]
fn past_the_hourly_limit_requests_go_quietly_but_urgent_ones_still_sound() {
    let fleet = Fleet::new();
    fleet.digest_door(json!({"digest_every_mins": 600}));
    fleet.ok(&["afk"]);
    for n in 1..=4 {
        fleet.tell("ops", &format!("decide {n}"), "request", "normal");
    }
    fleet.tell("ops", "the database is down: restore?", "request", "high");
    let supervisor = fleet.supervise();
    let mail = fleet.wait("five requests", |e| {
        let mail = events(e, 0, "mail");
        (mail.len() == 5).then_some(mail)
    });
    stop(supervisor);
    let quiet: Vec<(String, bool)> = mail
        .iter()
        .map(|e| {
            (
                e["mail"]["body"].as_str().unwrap().to_owned(),
                e["quiet"] == true,
            )
        })
        .collect();
    assert_eq!(
        quiet,
        [
            ("decide 1".to_owned(), false),
            ("decide 2".to_owned(), false),
            ("decide 3".to_owned(), false),
            ("decide 4".to_owned(), true),
            ("the database is down: restore?".to_owned(), false),
        ]
    );
}

// Asks a ship on the linked machine, then logs and acknowledges what comes.
const REMOTE_ADAPTER: &str = r#"import json, os, sys
log = open(os.environ["FAKE_DOOR_LOG"], "a")
def note(entry):
    log.write(json.dumps(entry) + "\n"); log.flush()
note({"hello": json.loads(sys.stdin.readline())})
print(json.dumps({"type": "request", "id": "ask", "command": "send",
    "args": {"to": "harbor", "body": "Is the backup green?"}}), flush=True)
print(json.dumps({"type": "request", "id": "coverage", "command": "status", "args": {}}), flush=True)
for line in sys.stdin:
    message = json.loads(line)
    note(message)
    if message["type"] == "mail":
        print(json.dumps({"type": "delivered", "ref": message["ref"]}), flush=True)
"#;

#[test]
fn a_reply_from_a_ship_on_another_machine_reaches_the_door_that_asked() {
    linked_reply(false);
}

#[test]
fn a_reverse_only_door_warns_and_does_not_receive_the_remote_desks_reply() {
    linked_reply(true);
}

fn linked_reply(reverse: bool) {
    let fleet = Fleet::new();
    // A server whose ship the door asks, and which holds the captain's desk.
    let server = fleet.root.join("server");
    let on_server = |dir: &Path| {
        let mut command = fleet.command(dir);
        command
            .env("HOME", &server)
            .env("PRIVATEER_FLEET", server.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", server.join("ledger"));
        command
    };
    let ok = |mut command: Command, args: &[&str]| -> String {
        let output = command.args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let harbor = server.join("harbor");
    fs::create_dir_all(&harbor).unwrap();
    ok(
        on_server(&harbor),
        &["init", "--ship", "harbor", "--harness", "codex"],
    );
    // Like OpenSSH, run the last argument as a shell command line.
    let ssh = fleet.root.join("fake-ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\nfor argument do remote=$argument; done\nexec /bin/sh -c \"$remote\"\n",
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    let liaison = fleet.root.join("ships/liaison");
    let registry = if reverse {
        fleet.root.join("state/fleet.sqlite")
    } else {
        server.join("fleet.sqlite")
    };
    ok(
        if reverse {
            on_server(&harbor)
        } else {
            fleet.command(&liaison)
        },
        &[
            "machine",
            "add",
            if reverse { "laptop" } else { "server" },
            "test-host",
            "--executable",
            BIN,
            "--remote-fleet",
            registry.to_str().unwrap(),
            "--as",
            if reverse { "server" } else { "laptop" },
        ],
    );
    if !reverse {
        ok(fleet.command(&liaison), &["machine", "captain", "server"]);
    }

    fs::write(fleet.root.join("adapter/door.py"), REMOTE_ADAPTER).unwrap();
    fleet.add();
    // Explicit desk placement and implicit collected-link desks both warn.
    // SERVICE=none keeps this a diagnostic check, not a background service.
    let start = fleet.run_in(&fleet.root, &["door", "start", "fake"]);
    assert!(start.status.success());
    let warning = String::from_utf8(start.stderr).unwrap();
    assert!(
        warning.contains("captain desk elsewhere: server"),
        "{warning}"
    );
    assert!(
        warning.contains("cannot collect from reverse-only links"),
        "{warning}"
    );
    assert!(
        warning.contains("uncorrelated notices and ship requests"),
        "{warning}"
    );
    let supervisor = fleet.supervise();
    let asked = fleet.wait("the request to the server", |e| response(e, "ask").cloned());
    assert_eq!(asked["ok"], true, "{asked}");
    let request = asked["result"]["mail"]["id"].as_str().unwrap().to_owned();
    let hello = fleet
        .entries()
        .into_iter()
        .find_map(|e| e.get("hello").cloned())
        .unwrap();
    let scope = &hello["mail_delivery"];
    assert_eq!(scope["desk_machines"], json!(["server"]));
    // The request status diagnostic is live, and agrees with hello at start.
    let status = fleet.wait("coverage status", |e| response(e, "coverage").cloned());
    assert_eq!(status["result"]["mail_delivery"], *scope, "{status}");
    assert!(scope["warning"].as_str().unwrap().contains("server"));
    assert_eq!(
        scope["reverse_only"],
        if reverse {
            json!(["server"])
        } else {
            json!([])
        }
    );
    assert_eq!(
        scope["reaches"],
        if reverse {
            json!([])
        } else {
            json!(["server"])
        }
    );
    if reverse {
        assert_eq!(asked["result"]["mail"]["delivery"], "pending", "{asked}");
        let collected = ok(on_server(&harbor), &["machine", "collect"]);
        assert!(collected.contains("\"delivered\": 1"), "{collected}");
    } else {
        assert_eq!(asked["result"]["mail"]["delivery"], "delivered", "{asked}");
    }

    // The server ship answers there, and the laptop collects the answer only
    // when it reaches the responder. The reverse-only side cannot pull it.
    let reply: Value = serde_json::from_str(&ok(
        on_server(&harbor),
        &[
            "mail.send",
            "--json",
            &json!({"from": "harbor", "to": "captain", "kind": "reply",
                    "correlation": request, "body": "Green since noon"})
            .to_string(),
        ],
    ))
    .unwrap();
    let collected = ok(fleet.command(&liaison), &["machine", "collect"]);
    if reverse {
        assert_eq!(
            serde_json::from_str::<Value>(&collected).unwrap()["collected"],
            json!([])
        );
        // A collect from the linking side does not push its captain mail back.
        ok(on_server(&harbor), &["machine", "collect"]);
        let inbox = fleet.call("liaison", "mail.inbox", json!({"address": "captain"}));
        assert!(inbox["messages"].as_array().unwrap().is_empty(), "{inbox}");
        let shown = fleet.call("liaison", "mail.show", json!({"id": request}));
        assert_eq!(shown["state"], "open", "{shown}");
        assert!(
            !fleet
                .entries()
                .iter()
                .any(|e| e["type"] == "mail" && e["mail"]["id"] == reply["id"])
        );
        stop(supervisor);
        return;
    }
    assert!(collected.contains("\"delivered\": 1"), "{collected}");

    // The door hears it from the home holding the request, which it closed.
    let heard = fleet.wait("the reply", |e| {
        e.iter()
            .find(|e| e["type"] == "mail" && e["mail"]["id"] == reply["id"])
            .cloned()
    });
    fleet.wait_delivered(std::slice::from_ref(&heard));
    stop(supervisor);
    assert_eq!(heard["ship"], "liaison", "{heard}");
    assert_eq!(heard["mail"]["body"], "Green since noon");
    let shown = fleet.call("liaison", "mail.show", json!({"id": request}));
    assert_eq!(shown["state"], "answered", "{shown}");
}
