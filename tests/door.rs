//! Front doors: a scripted adapter under the real `privateer door run`
//! supervisor, on a two-ship fleet with its own registry and HOME.
mod support;

include!("fixtures/door_clients.rs");
include!("fixtures/door_client_edit.rs");
include!("fixtures/door_oauth.rs");
include!("fixtures/door_delegated.rs");
include!("fixtures/door_reader.rs");
include!("fixtures/door_reader_copies.rs");
include!("fixtures/door_reader_regressions.rs");
include!("fixtures/door_reader_linked.rs");
include!("fixtures/door_desk_linked.rs");
include!("fixtures/door_desk_placement.rs");
include!("fixtures/door_desk_intake.rs");
include!("fixtures/door_desk_placement_delegated.rs");
include!("fixtures/door_delegated_linked.rs");
include!("fixtures/door_delegated_relay.rs");
include!("fixtures/door_delegated_admin.rs");
include!("fixtures/door_delegated_files.rs");
include!("fixtures/door_delegated_regressions.rs");
include!("fixtures/door_runtime.rs");
include!("fixtures/door_runtime_linked.rs");
include!("fixtures/link_incarnation.rs");
include!("fixtures/door_inspection.rs");
include!("fixtures/door_actions.rs");
include!("fixtures/door_resources.rs");
include!("fixtures/door_approvals.rs");
include!("fixtures/door_approvals_linked.rs");

use rusqlite::OptionalExtension;
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
        Self::with_ships(&[("liaison", true), ("ops", false)])
    }

    fn with_ships(ships: &[(&str, bool)]) -> Self {
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
            // env-python fakes must use the suite's resolved interpreter, not
            // a version-manager shim under this fleet's isolated HOME.
            path: support::path_with(&bin).into_string().unwrap(),
            _temp: temp,
            root,
        };
        for &(ship, liaison) in ships {
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
        use std::os::unix::process::CommandExt;
        self.command(&self.root)
            .args(["door", "run", "fake"])
            // Its own group: a group signal can only reach this supervisor
            // and its own descendants, never another test's processes.
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn entries(&self) -> Vec<Value> {
        whole_lines(&fs::read_to_string(&self.log).unwrap_or_default())
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

impl Drop for Fleet {
    /// Every `door run` supervisor this fleet started, and the adapter each
    /// one runs, stops with the fleet: a process still referring to this
    /// fleet's unique temporary root in its command line or working directory
    /// is ended by pid before the root is deleted, so a supervisor a test
    /// forgot to stop can no longer outlive the test run orphaned. Only this
    /// fleet's own processes match that path; nothing is signalled by name.
    fn drop(&mut self) {
        support::end_processes_referring_to(&self.root);
    }
}

/// Every record the log has finished writing: up to and including its last
/// newline. The adapter can be caught mid-line, so a torn tail without its
/// newline is left for the wait loop to read again; every whole line is kept
/// and still parsed strictly.
fn whole_lines(log: &str) -> Vec<Value> {
    log[..log.rfind('\n').map_or(0, |i| i + 1)]
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn a_torn_tail_keeps_the_whole_lines_before_it() {
    let a = json!({"id": "a"}).to_string();
    let b = json!({"id": "b"}).to_string();
    let log = a + "\n" + &b + "\n{\"id\": \"c\"";
    let entries = whole_lines(&log);
    assert_eq!(entries, [json!({"id": "a"}), json!({"id": "b"})]);
}

#[test]
#[should_panic(expected = "called `Result::unwrap()` on an `Err` value")]
fn a_malformed_whole_line_still_fails_the_read() {
    whole_lines("{\"id\": \"a\"}\nnot a record\n");
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

/// Register a protocol choice without changing any live fleet or bundled plugin.
fn manifest_protocol(fleet: &Fleet, protocol: u32) {
    let path = fleet.root.join("adapter/manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    manifest["door_protocol"] = json!(protocol);
    fs::write(path, manifest.to_string()).unwrap();
}

#[test]
fn scoped_v2_refuses_credentials_spoofing_and_unknown_shapes_without_fleet_effects() {
    let fleet = Fleet::new();
    manifest_protocol(&fleet, 2);
    fs::write(
        fleet.root.join("adapter/door.py"),
        r#"import json, os, sys
log = open(os.environ['FAKE_DOOR_LOG'], 'a')
def note(m):
    log.write(json.dumps(m)+'\n'); log.flush()
note({'hello':json.loads(sys.stdin.readline())})
def ask(i, command, args, **extra):
    print(json.dumps(dict(type='request', id=i, command=command, args=args, **extra)), flush=True)
ask('public', 'protocol', {})
ask('missing', 'ships', {})
ask('bad', 'status', {}, auth={'token':'fixture-secret-never-log'})
ask('empty', 'status', {}, auth={'token':''})
ask('malformed-auth', 'status', {}, auth={'token':7})
ask('unknown', 'mail.inbox', {}, auth={'token':'fixture-secret-never-log'})
ask('unknown-args', 'ships', {'all':True})
ask('wrong-shape', 'ships', [])
ask('unknown-action', 'away', {'action':'teleport'})
ask('from', 'send', {'body':'spoof', 'from':'captain'})
ask('actor', 'ships', {}, actor='captain')
ask('operator', 'send', {'body':'spoof', 'payload':{'nested':[{'operator':True}]}})
ask('grant', 'status', {}, auth={'token':'x', 'grant':'owner'})
ask('payload', 'send', {'body':'spoof', 'payload':{'unknown':True}})
ask('downgrade', 'send', {'body':'spoof'}, version=1)
ask('send', 'send', {'to':'ops', 'body':'blocked'})
ask('digest', 'digest', {})
print(json.dumps(dict(type='hello', id='hello-claim', version=1)), flush=True)
print(json.dumps(dict(type='delivered', id='ack', ref='ops:999999')), flush=True)
ask('public-extra', 'protocol', {'fleet':True})
ask('meta-wildcard', 'meta.ships', {})
ask('oauth-wildcard', 'oauth.grant', {})
ask('done', 'protocol', {})
for line in sys.stdin:
    note(json.loads(line))
"#,
    )
    .unwrap();
    fleet.add();
    fleet.call(
        "ops",
        "mail.send",
        json!({"from":"ops", "to":"captain", "kind":"notice", "body":"private unread mail"}),
    );
    fleet.ok(&["afk", "still away"]);
    let supervisor = fleet.supervise();
    let entries = fleet.wait("scoped denials", |entries| {
        response(entries, "done").map(|_| entries.to_vec())
    });
    let hello = &entries[0]["hello"];
    assert_eq!(hello["version"], 2);
    assert!(hello.get("recipient").is_none());
    assert!(hello.get("mail_delivery").is_none());
    for id in ["public", "done"] {
        let result = response(&entries, id).unwrap();
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["result"], json!({"version":2}));
    }
    for id in [
        "missing",
        "bad",
        "empty",
        "malformed-auth",
        "unknown",
        "unknown-args",
        "wrong-shape",
        "unknown-action",
        "from",
        "actor",
        "operator",
        "grant",
        "payload",
        "downgrade",
        "send",
        "digest",
        "hello-claim",
        "ack",
        "public-extra",
        "meta-wildcard",
        "oauth-wildcard",
    ] {
        let result = response(&entries, id).unwrap();
        assert_eq!(result["ok"], false, "{result}");
        assert_eq!(
            result["error"],
            json!({"code":"not_found", "message":"Not found"}),
            "{result}"
        );
    }
    // Give the normal one-second polling boundary time to run as well.
    std::thread::sleep(Duration::from_millis(1200));
    stop(supervisor);
    assert!(
        fleet
            .entries()
            .iter()
            .all(|e| e["type"] != "mail" && e["type"] != "digest")
    );
    assert!(fleet.ok(&["away"]).contains("Away since"));
    let registry = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    let words: String = registry
        .query_row(
            "SELECT words FROM away WHERE returned_ms IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(words, "still away");
    let log = fs::read_to_string(fleet.root.join("state/doors/fake/door.log")).unwrap();
    for reason in [
        "missing_credential",
        "invalid_credential",
        "unknown_command",
        "unsupported_arguments",
        "reserved_identity",
        "protocol_mismatch",
        "invalid_envelope",
    ] {
        assert!(log.contains(&format!("reason={reason}")), "{log}");
    }
    assert!(!log.contains("fixture-secret-never-log"), "{log}");
    for ship in ["liaison", "ops"] {
        let conn = rusqlite::Connection::open(fleet.home(ship).join("privateer.sqlite")).unwrap();
        let sent: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mail WHERE body IN ('blocked','spoof')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sent, 0);
        let cursors: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='door_cursor'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            cursors, 0,
            "scoped polling must not initialize captain cursors"
        );
    }
}

#[test]
fn registered_protocol_survives_manifest_edits_restart_and_runtime_claims() {
    for protocol in [1, 2] {
        let fleet = Fleet::new();
        manifest_protocol(&fleet, protocol);
        fs::write(fleet.root.join("adapter/door.py"), r#"import json, os, sys
log = open(os.environ['FAKE_DOOR_LOG'], 'a')
def note(m):
    log.write(json.dumps(m)+'\n'); log.flush()
hello=json.loads(sys.stdin.readline())
note({'hello':hello})
print(json.dumps(dict(type='request',id='claim',command='ships',args={},version=3-hello['version'])),flush=True)
for line in sys.stdin:
    note(json.loads(line))
"#).unwrap();
        fleet.add();
        // A scoped adapter's runtime manifest edit cannot downgrade trust.
        // A v1 row changing to a scoped manifest is refused before launch
        // instead (tested separately below).
        if protocol == 2 {
            manifest_protocol(&fleet, 1);
        }
        for session in 0..2 {
            let supervisor = fleet.supervise();
            let entries = fleet.wait("pinned protocol and rejected runtime claim", |entries| {
                let hellos: Vec<_> = entries
                    .iter()
                    .filter(|e| e.get("hello").is_some())
                    .collect();
                (hellos.len() == session + 1
                    && entries.iter().filter(|e| e["id"] == "claim").count() == session + 1)
                    .then(|| entries.to_vec())
            });
            let hello = entries
                .iter()
                .rev()
                .find(|e| e.get("hello").is_some())
                .unwrap();
            assert_eq!(hello["hello"]["version"], protocol);
            assert_eq!(response(&entries, "claim").unwrap()["ok"], false);
            stop(supervisor);
        }
        let conn = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
        let recorded: u32 = conn
            .query_row("SELECT protocol FROM doors WHERE name='fake'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(recorded, protocol);
    }
}

#[test]
fn trusted_registration_refuses_runtime_scoped_manifests_before_spawning() {
    for kind in ["front-door", "front-door-scoped", "guard"] {
        let fleet = Fleet::new();
        if kind == "guard" {
            manifest_protocol(&fleet, 2);
        }
        fleet.add();
        if kind == "guard" {
            // Even a corrupt/legacy row pointing to a scoped guard must not
            // start the adapter with trusted captain authority.
            let conn = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
            conn.execute("UPDATE doors SET protocol=1 WHERE name='fake'", [])
                .unwrap();
        } else {
            let source = fleet.root.join("adapter/manifest.json");
            let mut adapter: Value = serde_json::from_slice(&fs::read(&source).unwrap()).unwrap();
            adapter["kind"] = json!(kind);
            adapter["door_protocol"] = json!(2);
            fs::write(source, adapter.to_string()).unwrap();
        }
        let supervisor = fleet.supervise();
        let log = fleet.root.join("state/doors/fake/door.log");
        fleet.wait("runtime scoped manifest refusal", |_| {
            fs::read_to_string(&log)
                .unwrap_or_default()
                .contains("a scoped adapter requires scoped registration; pass --protocol 2")
                .then_some(())
        });
        stop(supervisor);
        assert!(fleet.entries().is_empty(), "scoped adapter must not spawn");
        assert!(!fleet.log.exists(), "adapter must not even open its log");
    }
}

#[test]
fn protocol_registration_is_explicit_and_scoped_manifests_cannot_be_trusted() {
    let fleet = Fleet::new();
    let path = fleet.root.join("adapter/manifest.json");
    let manifest = path.to_str().unwrap();
    fleet.ok(&[
        "door",
        "add",
        "fake",
        "--manifest",
        manifest,
        "--protocol",
        "2",
    ]);
    assert!(fleet.ok(&["door", "status", "fake"]).contains("protocol 2"));
    // Omitting --protocol on re-add must retain the stored restriction, even
    // though this adapter's manifest still defaults to trusted v1.
    fleet.add();
    assert!(fleet.ok(&["door", "status", "fake"]).contains("protocol 2"));
    let downgrade = fleet.ok(&[
        "door",
        "add",
        "fake",
        "--manifest",
        manifest,
        "--protocol",
        "1",
    ]);
    assert!(downgrade.contains("Explicit --protocol 1"), "{downgrade}");
    assert!(
        downgrade.contains("trusted v1 captain authority"),
        "{downgrade}"
    );
    assert!(fleet.ok(&["door", "status", "fake"]).contains("protocol 1"));
    manifest_protocol(&fleet, 2);
    for args in [
        vec!["door", "add", "fake", "--manifest", manifest],
        vec![
            "door",
            "add",
            "fake",
            "--manifest",
            manifest,
            "--protocol",
            "1",
        ],
    ] {
        let error = fleet.refused(&args);
        assert!(error.contains("cannot be registered"), "{error}");
        assert!(error.contains("pass --protocol 2"), "{error}");
        assert!(fleet.ok(&["door", "status", "fake"]).contains("protocol 1"));
    }
    manifest_protocol(&fleet, 7);
    assert!(
        fleet
            .refused(&["door", "add", "fake", "--manifest", manifest])
            .contains("unsupported door protocol")
    );
    manifest_protocol(&fleet, 1);
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["id"] = json!("mcp");
    fs::write(&path, value.to_string()).unwrap();
    assert!(
        fleet
            .refused(&["door", "add", "fake", "--manifest", manifest])
            .contains("MCP front door requires scoped")
    );
}

#[test]
fn scoped_registrations_store_rollback_safe_manifests_and_load_the_original_adapter() {
    for kind in ["front-door", "front-door-scoped"] {
        let fleet = Fleet::new();
        let source = fleet.root.join("adapter/manifest.json");
        let mut adapter: Value = serde_json::from_slice(&fs::read(&source).unwrap()).unwrap();
        adapter["kind"] = json!(kind);
        adapter["door_protocol"] = json!(2);
        fs::write(&source, adapter.to_string()).unwrap();
        fleet.add();
        let conn = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
        let stored: String = conn
            .query_row(
                "SELECT manifest FROM doors WHERE name='fake' AND protocol=2",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let guard: Value = serde_json::from_slice(&fs::read(&stored).unwrap()).unwrap();
        assert_ne!(Path::new(&stored), source);
        assert_eq!(guard["schema_version"], 1);
        assert_eq!(guard["kind"], "front-door-scoped");
        assert_eq!(guard["door_protocol"], 2);
        assert_eq!(guard["adapter_manifest"], source.to_str().unwrap());
        // Pre-v2 load_manifest accepts only kind == "front-door" and also
        // requires executable/id. This form cannot load as a trusted adapter.
        assert_ne!(guard["kind"], "front-door");
        assert!(guard.get("executable").is_none());
        // Re-adding via the stored form must resolve to the original source,
        // not make a recursive guard chain that cannot restart.
        fleet.ok(&["door", "add", "fake", "--manifest", &stored]);
        let replacement: String = conn
            .query_row("SELECT manifest FROM doors WHERE name='fake'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(replacement, stored, "re-add reuses the stable guard path");
        let replacement: Value = serde_json::from_slice(&fs::read(&replacement).unwrap()).unwrap();
        assert_eq!(replacement["adapter_manifest"], source.to_str().unwrap());
        for _ in 0..3 {
            fleet.add();
        }
        let guards: Vec<_> = fs::read_dir(fleet.root.join("state/doors/fake"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_file())
            .collect();
        assert_eq!(guards, vec![PathBuf::from(&stored)], "no stale guard files");
        assert_eq!(
            fs::metadata(&stored).unwrap().permissions().mode() & 0o777,
            0o600,
            "guard is private"
        );
        let supervisor = fleet.supervise();
        let entries = fleet.wait(
            "guard loads adapter with original working directory",
            |entries| response(entries, "s2").is_some().then(|| entries.to_vec()),
        );
        assert_eq!(
            entries.iter().find(|e| e.get("hello").is_some()).unwrap()["hello"]["version"],
            2
        );
        assert_eq!(response(&entries, "s2").unwrap()["ok"], false);
        stop(supervisor);
    }
}

#[test]
fn registered_door_reads_do_not_take_the_registry_write_lock() {
    let fleet = Fleet::new();
    fleet.add();
    let conn = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    // Keep the writer lock held throughout both real CLI reads. A write-lock
    // attempt in table() times out and fails instead of returning a door.
    assert!(fleet.ok(&["door", "list"]).contains("protocol 1"));
    assert!(fleet.ok(&["door", "status", "fake"]).contains("protocol 1"));
    conn.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn returning_from_away_advances_only_trusted_door_cursors() {
    let fleet = Fleet::new();
    fleet.add();
    let source = fleet.root.join("adapter/manifest.json");
    fleet.ok(&[
        "door",
        "add",
        "scoped",
        "--manifest",
        source.to_str().unwrap(),
        "--protocol",
        "2",
    ]);
    let conn = rusqlite::Connection::open(fleet.home("ops").join("privateer.sqlite")).unwrap();
    conn.execute_batch(
        "CREATE TABLE door_cursor(door TEXT PRIMARY KEY, through_seq INTEGER NOT NULL);
        INSERT INTO door_cursor VALUES('fake',0),('scoped',0);",
    )
    .unwrap();
    fleet.ok(&["afk", "cursor isolation"]);
    fleet.tell("ops", "held for the captain", "notice", "normal");
    fleet.ok(&["back"]);
    let trusted: i64 = conn
        .query_row(
            "SELECT through_seq FROM door_cursor WHERE door='fake'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let scoped: i64 = conn
        .query_row(
            "SELECT through_seq FROM door_cursor WHERE door='scoped'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(trusted > 0);
    assert_eq!(scoped, 0);
    let liaison =
        rusqlite::Connection::open(fleet.home("liaison").join("privateer.sqlite")).unwrap();
    let initialized: i64 = liaison
        .query_row(
            "SELECT COUNT(*) FROM door_cursor WHERE door='scoped'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        initialized, 0,
        "return must not initialize a scoped reader cursor"
    );
}

#[test]
fn legacy_registry_rows_migrate_to_trusted_v1() {
    let fleet = Fleet::new();
    let conn = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    conn.execute_batch("DROP TABLE IF EXISTS doors; CREATE TABLE doors(name TEXT PRIMARY KEY, manifest TEXT NOT NULL, config TEXT, recipient TEXT, created_ms INTEGER NOT NULL);").unwrap();
    conn.execute(
        "INSERT INTO doors(name,manifest,created_ms) VALUES('fake',?1,0)",
        [fleet.root.join("adapter/manifest.json").to_str().unwrap()],
    )
    .unwrap();
    assert!(fleet.ok(&["door", "list"]).contains("protocol 1"));
    let recorded: u32 = conn
        .query_row("SELECT protocol FROM doors WHERE name='fake'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(recorded, 1);
    let supervisor = fleet.supervise();
    let response = fleet.wait("legacy trusted send", |entries| {
        response(entries, "s1").cloned()
    });
    assert_eq!(response["ok"], true, "{response}");
    stop(supervisor);
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

/// A dropped fixture must not leave a `door run` supervisor or its adapter
/// behind: the orphaned door processes observed under the user's service
/// manager outlived their test runs and the deletion of the test home. The
/// check is `/proc`, scanned for this fixture's own temporary home path, on
/// Linux only: a process's command line or working directory referring to
/// that unique path can only be one this fixture started.
#[test]
#[cfg(target_os = "linux")]
fn a_dropped_fixture_leaves_no_door_process_referring_to_its_home() {
    let fleet = Fleet::new();
    fleet.add();
    let mut supervisor = fleet.supervise();
    let hello = fleet.wait("the adapter's hello", |entries| {
        entries
            .first()
            .filter(|e| e["hello"]["door"] == "fake")
            .cloned()
    });
    let adapter = hello["pid"].as_i64().unwrap();
    assert!(alive(adapter), "the adapter should be running");
    assert!(
        alive(supervisor.id() as i64),
        "the supervisor should be running"
    );
    let root = fleet.root.clone();
    let running = support::processes_referring_to(&root);
    assert!(
        running.contains(&(adapter as u32)),
        "the running adapter {adapter} should be visible in /proc: {running:?}"
    );
    drop(fleet);
    let leftover = support::processes_referring_to(&root);
    assert!(
        leftover.is_empty(),
        "processes outlived the dropped fixture: {leftover:?}"
    );
    // Reap what the sweep ended: a fixture also leaves no zombie of its own.
    let _ = supervisor.wait();
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
fn a_door_send_reply_cannot_close_a_production_approval() {
    let fleet = Fleet::new();
    fs::write(
        fleet.root.join("adapter/door.py"),
        r#"import json, os, sys
log = open(os.environ['FAKE_DOOR_LOG'], 'a')
json.loads(sys.stdin.readline())
for line in sys.stdin:
    message = json.loads(line)
    log.write(json.dumps(message) + '\n'); log.flush()
    if message['type'] == 'mail' and message['mail']['kind'] == 'request':
        mail = message['mail']
        for variant in ['body', 'answer']:
            args = dict(id=mail['id'] + '-' + variant, to='ops', kind='reply',
                        correlation=mail['id'], body='yes')
            if variant == 'answer':
                args['payload'] = dict(answer='yes')
            print(json.dumps(dict(type='request', id=args['id'], command='send', args=args)), flush=True)
"#,
    )
    .unwrap();
    fleet.add();
    let request = fleet.call(
        "ops",
        "mail.send",
        json!({"id":"production-ask","from":"ops","to":"captain","kind":"request",
            "request_kind":"production_approval","task_id":"ops-duty","body":"Ship it?",
            "payload":{"artifact":"sha256:exact","environment":"production"}}),
    );
    let version = fleet.call("ops", "runtime.needs", json!({}))["items"][0]["version"].clone();
    let supervisor = fleet.supervise();
    let entries = fleet.wait("both send reply refusals", |e| {
        (response(e, "production-ask-body").is_some()
            && response(e, "production-ask-answer").is_some())
        .then(|| e.to_vec())
    });
    stop(supervisor);
    for id in ["production-ask-body", "production-ask-answer"] {
        let refused = response(&entries, id).unwrap();
        assert_eq!(refused["ok"], false, "{refused}");
        let error = refused["error"].as_str().unwrap();
        assert!(error.contains("production approval"), "{error}");
        assert!(error.contains("approval.grant"), "{error}");
        assert!(error.contains("approval.deny"), "{error}");
    }
    assert_eq!(
        fleet.call("ops", "mail.show", json!({"id":"production-ask"})),
        request
    );
    let conn = rusqlite::Connection::open(fleet.home("ops").join("privateer.sqlite")).unwrap();
    for table in ["approvals", "mail WHERE kind='reply'"] {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "{table}");
    }
    drop(conn);
    let gate = json!({"task_id":"ops-duty","current_head":"sha256:exact",
        "tested_head":"sha256:exact","checks_passed":true,
        "deploys_production":true,"environment":"production"});
    let error = fleet.refused(&[
        "--home",
        fleet.home("ops").to_str().unwrap(),
        "delivery.check",
        "--json",
        &gate.to_string(),
    ]);
    assert!(error.contains("approval"), "{error}");
    fleet.call(
        "ops",
        "approval.grant",
        json!({"id":"real-grant","request_id":"production-ask","expected_version":version,
            "task_id":"ops-duty","artifact":"sha256:exact","environment":"production",
            "approver":"captain"}),
    );
    assert_eq!(
        fleet.call("ops", "mail.show", json!({"id":"production-ask"}))["state"],
        "approved"
    );
    assert_eq!(
        fleet.call("ops", "delivery.check", gate)["approval_id"],
        "real-grant"
    );
}

// Hands the runtime files from everywhere: its inbox, deep in its state
// directory, and from outside it by every route a door could try.
const FILE_PUT_ADAPTER: &str = r#"import json, os, sys
log = open(os.environ['FAKE_DOOR_LOG'], 'a')
hello = json.loads(sys.stdin.readline())
state = hello['state_dir']
root = os.path.dirname(os.environ['FAKE_DOOR_LOG'])
outside = os.path.join(root, 'outside.txt')
inbox = os.path.join(state, 'inbox')
os.makedirs(inbox)
os.makedirs(os.path.join(state, 'downloads'))
open(os.path.join(inbox, 'photo.bin'), 'w').write('inbox bytes')
open(os.path.join(state, 'downloads', 'note.txt'), 'w').write('deep bytes')
os.symlink(outside, os.path.join(inbox, 'link.txt'))
os.link(outside, os.path.join(inbox, 'hard.txt'))
def ask(ident, args):
    print(json.dumps(dict(type='request', id=ident, command='file.put', args=args)), flush=True)
def put(ident, content_file):
    ask(ident, dict(to='ops', name='x.bin', content_file=content_file))
ask('inbox', dict(to='ops', name='photo.bin', content_file=os.path.join(inbox, 'photo.bin')))
ask('deep', dict(to='ops', name='note.txt', content_file=os.path.join(state, 'downloads', 'note.txt')))
put('outside', outside)
put('escape', os.path.join(state, os.path.relpath(outside, state)))
put('symlink', os.path.join(inbox, 'link.txt'))
put('hardlink', os.path.join(inbox, 'hard.txt'))
put('directory', inbox)
put('missing', os.path.join(inbox, 'gone.txt'))
for line in sys.stdin:
    log.write(json.dumps(json.loads(line))+'\n'); log.flush()
"#;

#[test]
fn a_door_puts_only_files_from_its_own_state_directory() {
    let fleet = Fleet::new();
    fs::write(fleet.root.join("outside.txt"), "the door never wrote this").unwrap();
    fs::write(fleet.root.join("adapter/door.py"), FILE_PUT_ADAPTER).unwrap();
    fleet.add();
    let supervisor = fleet.supervise();
    let entries = fleet.wait("every file.put answer", |entries| {
        response(entries, "missing").map(|_| entries.to_vec())
    });
    // The Telegram path — state/inbox — and a deeper directory are accepted,
    // and the stored files carry their own bytes.
    let inbox = response(&entries, "inbox").unwrap();
    assert_eq!(inbox["ok"], true, "{inbox}");
    assert_eq!(
        fs::read(inbox["result"]["path"].as_str().unwrap()).unwrap(),
        b"inbox bytes"
    );
    let deep = response(&entries, "deep").unwrap();
    assert_eq!(deep["ok"], true, "{deep}");
    assert_eq!(
        fs::read(deep["result"]["path"].as_str().unwrap()).unwrap(),
        b"deep bytes"
    );
    // An absolute path outside, a .. escape, a symlink inside pointing out
    // and a hard link to an outside file are all refused by name of the
    // rule, never of the file's content.
    for id in ["outside", "escape", "symlink", "hardlink"] {
        let refused = response(&entries, id).unwrap();
        assert_eq!(refused["ok"], false, "{refused}");
        assert!(
            refused["error"]
                .as_str()
                .unwrap()
                .contains("inside the door's state directory"),
            "{refused}"
        );
    }
    // A directory and a missing file are refused too.
    let directory = response(&entries, "directory").unwrap();
    assert_eq!(directory["ok"], false, "{directory}");
    assert!(
        directory["error"]
            .as_str()
            .unwrap()
            .contains("inside the door's state directory"),
        "{directory}"
    );
    let missing = response(&entries, "missing").unwrap();
    assert_eq!(missing["ok"], false, "{missing}");
    assert!(
        missing["error"]
            .as_str()
            .unwrap()
            .contains("cannot open content_file"),
        "{missing}"
    );
    // Only the door's own two files were ever stored for the ship.
    let stored: Vec<String> = fs::read_dir(fleet.home("ops").join("files"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "uploads")
        .collect();
    assert_eq!(stored.len(), 2, "{stored:?}");
    stop(supervisor);
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
    // Abandon the starter at the same spawn/publication boundary as an
    // interrupted dispatcher upgrade. The door must publish its own custody.
    let marker = fleet.root.join("after-spawn");
    let mut starter = fleet
        .command(&fleet.root)
        .args(["door", "start", "fake"])
        .env("PRIVATEER_SERVICE", "detached")
        .env("PRIVATEER_TEST_DETACHED_AFTER_SPAWN", &marker)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker.is_file() {
        assert!(
            Instant::now() < deadline,
            "starter did not reach spawn boundary"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    starter.kill().unwrap();
    assert!(!starter.wait().unwrap().success());
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
