//! Captain-approved privileged actions, end to end through the real binary,
//! with a fake sudo (`PRIVATEER_SUDO`, honoured only in a `test-sudo` build)
//! and the captain's typed confirmation read from a file (`PRIVATEER_TEST_TTY`).
//! No test runs the real sudo.
mod support;

use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");
const HOUR_MS: i64 = 3_600_000;

struct Machine {
    _temp: tempfile::TempDir,
    root: PathBuf,
    folder: PathBuf,
    path: String,
}

impl Machine {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("codex"), "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o755)).unwrap();
        let machine = Self {
            path: format!("{}:/usr/bin:/bin", bin.display()),
            folder: root.join("ships/apps"),
            _temp: temp,
            root,
        };
        fs::create_dir_all(&machine.folder).unwrap();
        machine.ok(&["init", "--ship", "apps", "--harness", "codex"]);
        machine
    }
    fn home(&self) -> PathBuf {
        self.folder.join(".privateer")
    }
    fn command(&self) -> Command {
        let mut command = support::command(BIN);
        command
            .current_dir(&self.folder)
            .env("PATH", &self.path)
            .env("HOME", &self.root)
            .env("PRIVATEER_FLEET", self.root.join("fleet.sqlite"))
            .env("PRIVATEER_SERVICE", "none")
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .arg("--home")
            .arg(self.home());
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
        String::from_utf8(out.stdout).unwrap()
    }
    fn json(&self, command: &str, args: Value) -> Value {
        serde_json::from_str(&self.ok(&[command, "--json", &args.to_string()])).unwrap()
    }
    fn refused(&self, command: &str, args: Value) -> String {
        let out = self.run(&[command, "--json", &args.to_string()]);
        assert!(!out.status.success(), "{command} {args} should be refused");
        stderr(&out)
    }
    fn agent(&self, command: &str, args: Value) -> Output {
        self.command()
            .args([command, "--json", &args.to_string()])
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", "apps")
            .output()
            .unwrap()
    }
    /// Stage as the ship, trusting this test's own files as root's.
    fn stage(&self, edit: impl FnOnce(&mut Value)) -> Output {
        let mut args = json!({"from": "apps", "task_id": "apps-duty", "program": "/usr/bin/id",
            "args": ["-u"], "cwd": self.root, "reason": "Check the effective user"});
        edit(&mut args);
        self.command()
            .args(["privileged.stage", "--json", &args.to_string()])
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", "apps")
            .env("PRIVATEER_TEST_TRUST_UID", uid().to_string())
            .output()
            .unwrap()
    }
    fn staged(&self, edit: impl FnOnce(&mut Value)) -> Value {
        let out = self.stage(edit);
        assert!(out.status.success(), "{}", stderr(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn need(&self, id: &str) -> Option<Value> {
        self.json("runtime.needs", json!({}))["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["source_id"] == id)
            .cloned()
    }
    fn answer(&self, id: &str, verb: &str) -> Output {
        let item = self.need(id).expect("the request is listed");
        let mut args = json!({"request_id": id, "expected_version": item["version"],
            "task_id": item["task_id"], "artifact": item["artifact"],
            "environment": item["environment"], "approver": "captain"});
        if verb == "approval.grant" {
            args["id"] = json!(format!("grant-{id}"));
        }
        self.run(&[verb, "--json", &args.to_string()])
    }
    fn grant(&self, id: &str) {
        let out = self.answer(id, "approval.grant");
        assert!(out.status.success(), "{}", stderr(&out));
    }
    /// A fake sudo that records what it was given, prints canned output and
    /// exits 3, or sleeps when `sleep`.
    fn fake_sudo(&self, sleep: bool) -> (PathBuf, PathBuf) {
        let log = self.root.join("sudo.json");
        let script = self.root.join(if sleep { "sudo-sleep" } else { "sudo" });
        fs::write(
            &script,
            format!(
                "#!{}\nimport json, os, sys, time\n\
                 open({:?}, 'w').write(json.dumps({{'argv': sys.argv[1:], 'env': dict(os.environ), \
                 'cwd': os.getcwd(), 'pid': os.getpid()}}))\n\
                 print('fake output'); print('fake error', file=sys.stderr); sys.stdout.flush()\n\
                 {}\nsys.exit(3)\n",
                support::python().display(),
                log.to_str().unwrap(),
                if sleep { "time.sleep(60)" } else { "" }
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        (script, log)
    }
    /// `privateer privileged run ID`, the captain typing `typed`.
    fn privileged_run(&self, id: &str, typed: &str, extra: &[&str]) -> Command {
        let (sudo, _) = self.fake_sudo(false);
        let tty = self.root.join("tty");
        fs::write(&tty, format!("{typed}\n")).unwrap();
        let mut command = self.command();
        command
            .args(["privileged", "run", id])
            .args(extra)
            .env("PRIVATEER_SUDO", sudo)
            .env("PRIVATEER_TEST_TTY", tty)
            .env("PRIVATEER_TEST_TRUST_UID", uid().to_string())
            .env("TERM", "xterm-test")
            .env("LANG", "C.UTF-8")
            .env("SECRET_TOKEN", "must-not-reach-sudo");
        command
    }
    fn sql(&self, statement: &str, values: &[&dyn rusqlite::ToSql]) {
        let conn = rusqlite::Connection::open(self.home().join("privateer.sqlite")).unwrap();
        conn.busy_timeout(Duration::from_secs(5)).unwrap();
        conn.execute(statement, values).unwrap();
    }
    fn mail(&self, id: &str) -> Value {
        self.json("mail.show", json!({"id": id}))
    }
    fn thread(&self, id: &str) -> Vec<Value> {
        self.json("mail.inbox", json!({"address": "apps", "limit": 200}))["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["correlation"] == id)
            .cloned()
            .collect()
    }
}

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn prefix(staged: &Value) -> String {
    staged["hash"].as_str().unwrap()[..8].to_owned()
}

/// A program this test owns, in a folder only it can write, trusted as
/// root's through PRIVATEER_TEST_TRUST_UID. Kept under the build's own
/// scratch folder: a temporary folder is writable by everyone.
fn own_program(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("privileged-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let program = dir.join("tool");
    fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    program
}

#[test]
fn a_granted_command_runs_once_as_sudo_k_with_a_cleared_environment() {
    let machine = Machine::new();
    let staged = machine.staged(|_| {});
    let id = staged["id"].as_str().unwrap();
    assert_eq!(staged["argv"], json!(["/usr/bin/id", "-u"]));
    // The captain's needs list it as an approval, with the exact argv.
    let item = machine.need(id).unwrap();
    assert_eq!(item["kind"], "approval");
    assert_eq!(item["privileged"]["argv"], json!(["/usr/bin/id", "-u"]));
    assert_eq!(item["privileged"]["hash"], staged["hash"]);
    assert_eq!(
        item["artifact"],
        format!("privileged:sha256:{}", staged["hash"].as_str().unwrap())
    );
    assert!(item["environment"].as_str().unwrap().starts_with("host:"));
    assert_eq!(item["actions"], json!(["approval.grant", "approval.deny"]));
    // Not approved yet: nothing runs.
    let early = machine
        .privileged_run(id, &prefix(&staged), &[])
        .output()
        .unwrap();
    assert!(!early.status.success());
    assert!(
        stderr(&early).contains("not approved"),
        "{}",
        stderr(&early)
    );

    machine.grant(id);
    assert_eq!(machine.mail(id)["state"], "approved");
    let told = machine.thread(id);
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(
        told[0]["body"],
        "approved, waiting for the captain at the machine"
    );

    // A wrong prefix runs nothing and leaves the approval standing.
    let wrong = machine
        .privileged_run(id, "00000000", &[])
        .output()
        .unwrap();
    assert!(!wrong.status.success());
    assert!(
        stderr(&wrong).contains("do not match"),
        "{}",
        stderr(&wrong)
    );
    assert!(!machine.root.join("sudo.json").exists());

    let ran = machine
        .privileged_run(id, &prefix(&staged), &[])
        .output()
        .unwrap();
    assert!(ran.status.success(), "{}", stderr(&ran));
    // The prompt showed the full hash and the command again.
    assert!(stderr(&ran).contains(staged["hash"].as_str().unwrap()));
    assert!(String::from_utf8_lossy(&ran.stdout).contains("fake output"));
    let result: Value =
        serde_json::from_str(String::from_utf8_lossy(&ran.stdout).lines().last().unwrap()).unwrap();
    assert_eq!(result["exit_code"], 3, "{result}");
    assert_eq!(result["timed_out"], false);

    // Exactly `sudo -k -- PROGRAM ARGS`, no shell, in the staged folder, with
    // only PATH, TERM and LANG.
    let seen: Value =
        serde_json::from_str(&fs::read_to_string(machine.root.join("sudo.json")).unwrap()).unwrap();
    assert_eq!(seen["argv"], json!(["-k", "--", "/usr/bin/id", "-u"]));
    assert_eq!(seen["cwd"], json!(machine.root));
    let mut env: Vec<String> = seen["env"].as_object().unwrap().keys().cloned().collect();
    env.sort();
    assert_eq!(env, ["LANG", "PATH", "TERM"], "{seen}");
    assert_eq!(seen["env"]["PATH"], "/usr/sbin:/usr/bin:/sbin:/bin");
    assert_eq!(seen["env"]["TERM"], "xterm-test");

    // Saved, and mailed to the ship in the request's thread.
    let listed = machine.json("privileged.list", json!({}));
    let run = &listed["requests"][0]["run"];
    assert_eq!(run["exit_code"], 3, "{listed}");
    assert!(run["finished_ms"].is_i64());
    let told = machine.thread(id);
    let result = told
        .iter()
        .find(|m| m["payload"]["privileged_result"].is_object())
        .expect("the result notice");
    assert_eq!(result["from"], "captain");
    assert_eq!(result["kind"], "notice");
    let carried = &result["payload"]["privileged_result"];
    assert_eq!(carried["exit_code"], 3);
    assert_eq!(carried["state"], "ran");
    let tail = carried["output_tail"].as_str().unwrap();
    assert!(
        tail.contains("fake output") && tail.contains("fake error"),
        "{tail}"
    );

    // Once only.
    let again = machine
        .privileged_run(id, &prefix(&staged), &[])
        .output()
        .unwrap();
    assert!(!again.status.success());
    assert!(stderr(&again).contains("already ran"), "{}", stderr(&again));
}

#[test]
fn a_tampered_request_a_changed_program_or_a_changed_charter_is_refused() {
    let machine = Machine::new();
    // The program file changes after the captain approved it.
    let program = own_program("changed");
    let staged = machine.staged(|a| a["program"] = json!(program));
    let id = staged["id"].as_str().unwrap();
    machine.grant(id);
    fs::write(&program, "#!/bin/sh\necho swapped\n").unwrap();
    let out = machine
        .privileged_run(id, &prefix(&staged), &[])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr(&out).contains("changed"), "{}", stderr(&out));
    assert!(!machine.root.join("sudo.json").exists());

    // The stored payload is edited after the grant.
    let staged = machine.staged(|_| {});
    let id = staged["id"].as_str().unwrap();
    machine.grant(id);
    machine.sql(
        "UPDATE mail SET payload=json_set(payload,'$.privileged.args',json('[\"-g\"]')) WHERE id=?1",
        &[&id],
    );
    let out = machine
        .privileged_run(id, &prefix(&staged), &[])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr(&out).contains("binding changed"), "{}", stderr(&out));
    // Nor is the edited request listed as approvable.
    let tampered = machine.staged(|_| {});
    let tampered_id = tampered["id"].as_str().unwrap();
    machine.sql(
        "UPDATE mail SET payload=json_set(payload,'$.privileged.cwd','/') WHERE id=?1",
        &[&tampered_id],
    );
    assert!(machine.need(tampered_id).is_none());

    // The charter changes after the grant.
    let staged = machine.staged(|_| {});
    let id = staged["id"].as_str().unwrap();
    machine.grant(id);
    machine.json(
        "charter.put",
        json!({"id": "apps", "objective": "A different objective"}),
    );
    let out = machine
        .privileged_run(id, &prefix(&staged), &[])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr(&out).contains("charter changed"), "{}", stderr(&out));
    assert!(!machine.root.join("sudo.json").exists());
}

#[test]
fn a_request_is_good_for_24_hours_from_staging_whether_approved_or_not() {
    let machine = Machine::new();
    let now = privateer::store::Store::now_ms();
    // Approved at hour 23: still runs before hour 24.
    let staged = machine.staged(|_| {});
    let id = staged["id"].as_str().unwrap();
    machine.sql(
        "UPDATE mail SET created_ms=?2 WHERE id=?1",
        &[&id, &(now - 23 * HOUR_MS)],
    );
    machine.grant(id);
    let ran = machine
        .privileged_run(id, &prefix(&staged), &[])
        .output()
        .unwrap();
    assert!(ran.status.success(), "{}", stderr(&ran));

    // Approved at hour 23, not run by hour 24: refused.
    let late = machine.staged(|_| {});
    let late_id = late["id"].as_str().unwrap();
    machine.sql(
        "UPDATE mail SET created_ms=?2 WHERE id=?1",
        &[&late_id, &(now - 23 * HOUR_MS)],
    );
    machine.grant(late_id);
    machine.sql(
        "UPDATE mail SET created_ms=?2 WHERE id=?1",
        &[&late_id, &(now - 24 * HOUR_MS - 1000)],
    );
    let out = machine
        .privileged_run(late_id, &prefix(&late), &[])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr(&out).contains("expired"), "{}", stderr(&out));

    // Unapproved past 24 hours: not listed, not grantable.
    let stale = machine.staged(|_| {});
    let stale_id = stale["id"].as_str().unwrap();
    machine.sql(
        "UPDATE mail SET created_ms=?2 WHERE id=?1",
        &[&stale_id, &(now - 25 * HOUR_MS)],
    );
    assert!(machine.need(stale_id).is_none());

    // The dispatcher's sweep ends both and tells the ship once.
    let store = privateer::store::Store::open(&machine.home()).unwrap();
    let mut ended = privateer::privileged::sweep(&store).unwrap();
    ended.sort();
    let mut expected = vec![late_id.to_owned(), stale_id.to_owned()];
    expected.sort();
    assert_eq!(ended, expected);
    assert!(privateer::privileged::sweep(&store).unwrap().is_empty());
    let closed = machine.mail(stale_id);
    assert_eq!(closed["state"], "withdrawn");
    assert_eq!(closed["end_reason"], "expired");
    for request in [late_id, stale_id] {
        let told: Vec<Value> = machine
            .thread(request)
            .into_iter()
            .filter(|m| m["payload"]["privileged_result"]["state"] == "expired")
            .collect();
        assert_eq!(told.len(), 1, "{request}: {told:?}");
    }
    // The lapsed approval is used up: it can never run.
    let out = machine
        .privileged_run(late_id, &prefix(&late), &[])
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn staging_refuses_each_class_of_command() {
    let machine = Machine::new();
    let refused = |edit: &dyn Fn(&mut Value), says: &str| {
        let out = machine.stage(|a| edit(a));
        assert!(!out.status.success(), "staged despite {says}");
        assert!(
            stderr(&out).contains(says),
            "{} (wanted {says})",
            stderr(&out)
        );
    };
    // Shape.
    refused(&|a| a["program"] = json!("id"), "absolute");
    refused(&|a| a["args"] = json!(vec!["x"; 65]), "at most 64");
    refused(&|a| a["args"] = json!(["x".repeat(4097)]), "4096 bytes");
    refused(
        &|a| a["args"] = json!(vec!["x".repeat(4000); 5]),
        "in total",
    );
    refused(&|a| a["args"] = json!(["a\nb"]), "control character");
    refused(&|a| a["args"] = json!(["a\u{0}b"]), "control character");
    refused(&|a| a["args"] = json!(["a\u{1b}[2J"]), "control character");
    refused(&|a| a["cwd"] = json!("relative"), "absolute");
    refused(&|a| a["cwd"] = json!("/no/such/folder"), "existing folder");
    refused(&|a| a["reason"] = json!(""), "reason");
    refused(&|a| a["reason"] = json!("x".repeat(1025)), "reason");
    refused(&|a| a["env"] = json!({"A": "1"}), "environment");
    refused(&|a| a["environment"] = json!({"A": "1"}), "environment");
    refused(&|a| a["shell"] = json!(true), "not shell");
    // Arguments that name sudoers.
    refused(
        &|a| a["args"] = json!(["--file", "/etc/sudoers.d/x"]),
        "sudoers",
    );
    // Programs, by class, where this machine has them.
    for (program, class) in [
        ("/bin/sh", "a shell"),
        ("/usr/bin/bash", "a shell"),
        ("/usr/bin/env", "a launcher"),
        ("/usr/bin/nice", "a launcher"),
        ("/usr/bin/timeout", "a launcher"),
        ("/usr/bin/xargs", "a launcher"),
        ("/usr/bin/find", "a launcher"),
        ("/usr/bin/ssh", "a launcher"),
        ("/usr/bin/sudo", "a privilege tool"),
        ("/usr/bin/su", "a privilege tool"),
        ("/usr/bin/python3", "an interpreter"),
        ("/usr/bin/perl", "an interpreter"),
        ("/usr/bin/awk", "an interpreter"),
        ("/usr/bin/sed", "an interpreter"),
        ("/usr/bin/vi", "an editor or pager"),
        ("/usr/bin/less", "an editor or pager"),
        ("/usr/bin/man", "an editor or pager"),
    ] {
        if Path::new(program).exists() {
            refused(&|a| a["program"] = json!(program), class);
        }
    }
    // A program file someone other than root owns, or a folder anyone writes.
    let program = own_program("unowned");
    let out = machine
        .command()
        .args([
            "privileged.stage",
            "--json",
            &json!({"from": "apps", "task_id": "apps-duty", "program": program, "args": [],
                "cwd": machine.root, "reason": "Run my own tool"})
            .to_string(),
        ])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "apps")
        .output()
        .unwrap();
    if fs::metadata(&program).unwrap().uid() != 0 {
        assert!(!out.status.success());
        assert!(
            stderr(&out).contains("not owned by root"),
            "{}",
            stderr(&out)
        );
    }
    let open = own_program("writable");
    fs::set_permissions(open.parent().unwrap(), fs::Permissions::from_mode(0o777)).unwrap();
    refused(
        &|a| a["program"] = json!(open),
        "writable by group or others",
    );
    // Another ship's task.
    refused(&|a| a["task_id"] = json!("nope"), "nope");
    // Nothing was staged.
    assert_eq!(
        machine.json("privileged.list", json!({}))["requests"],
        json!([])
    );
}

#[test]
fn only_the_runtime_writes_the_kind_and_only_privileged_run_uses_its_approval() {
    let machine = Machine::new();
    let staged = machine.staged(|_| {});
    let id = staged["id"].as_str().unwrap();
    let payload = machine.mail(id)["payload"].clone();
    let refused_send = |out: Output| {
        assert!(!out.status.success());
        let error = stderr(&out);
        assert!(
            error.contains("privileged.stage") || error.contains("set by the runtime"),
            "{error}"
        );
    };
    // A ship, or the operator, writing the kind or its payloads by hand.
    let forged = json!({"from": "apps", "to": "captain", "kind": "request",
        "request_kind": "privileged_action", "task_id": "apps-duty", "body": "Run it",
        "payload": payload});
    refused_send(machine.agent("mail.send", forged.clone()));
    refused_send(machine.run(&["mail.send", "--json", &forged.to_string()]));
    let mut kind_only = forged.clone();
    kind_only["payload"] = json!({"artifact": "x", "environment": "y"});
    refused_send(machine.agent("mail.send", kind_only));
    let mut approves = forged.clone();
    approves["payload"] = json!({"approves": {"action": "Run it as root"}});
    refused_send(machine.agent("mail.send", approves));
    refused_send(machine.agent(
        "mail.send",
        json!({"from": "apps", "to": "captain", "kind": "request", "body": "Hi",
            "payload": {"privileged": payload["privileged"]}}),
    ));
    refused_send(machine.agent(
        "mail.send",
        json!({"from": "apps", "to": "captain", "kind": "notice", "body": "Done",
            "payload": {"privileged_result": {"exit_code": 0}}}),
    ));
    // A production approval cannot name a privileged artifact or host.
    for (artifact, environment) in [
        ("privileged:sha256:abc", "production"),
        ("sha256:abc", "host:laptop"),
    ] {
        let out = machine.agent(
            "mail.send",
            json!({"from": "apps", "to": "captain", "kind": "request",
                "request_kind": "production_approval", "task_id": "apps-duty", "body": "Ship?",
                "payload": {"artifact": artifact, "environment": environment}}),
        );
        assert!(!out.status.success());
        assert!(
            stderr(&out).contains("privileged approvals"),
            "{}",
            stderr(&out)
        );
    }
    // An uncorrelated grant of a privileged artifact or host is refused.
    let error = machine.refused(
        "approval.grant",
        json!({"id": "direct", "task_id": "apps-duty", "artifact": payload["artifact"],
            "environment": payload["environment"], "approver": "captain"}),
    );
    assert!(error.contains("privileged approvals"), "{error}");
    // A captain reply grants nothing: it is refused, and the request stays open.
    let error = machine.refused(
        "mail.send",
        json!({"from": "captain", "to": "apps", "kind": "reply", "correlation": id,
            "body": "yes"}),
    );
    assert!(error.contains("approval.grant"), "{error}");
    let error = machine.refused("captain.reply", json!({"id": id, "body": "yes"}));
    assert!(error.contains("privileged command"), "{error}");
    let error = machine.refused("captain.dismiss", json!({"id": id}));
    assert!(error.contains("privileged command"), "{error}");
    assert_eq!(machine.mail(id)["state"], "open");

    // Granted: neither approval.consume nor a production delivery uses it up.
    machine.grant(id);
    let error = machine.refused(
        "approval.consume",
        json!({"id": format!("grant-{id}"), "task_id": "apps-duty",
            "artifact": payload["artifact"], "environment": payload["environment"]}),
    );
    assert!(error.contains("privileged"), "{error}");
    let out = machine.agent(
        "delivery.check",
        json!({"task_id": "apps-duty", "current_head": payload["artifact"],
            "tested_head": payload["artifact"], "checks_passed": true,
            "deploys_production": true, "environment": payload["environment"]}),
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("privileged"), "{}", stderr(&out));
    // The approval is still there for the captain's run.
    let ran = machine
        .privileged_run(id, &prefix(&staged), &[])
        .output()
        .unwrap();
    assert!(ran.status.success(), "{}", stderr(&ran));

    // Agents cannot run it, nor reach it through dispatch.
    let error = stderr(&machine.agent("privileged.run", json!({"id": id})));
    assert!(error.contains("PRIVATEER_ROLE"), "{error}");
}

#[test]
fn the_run_is_the_captains_at_a_terminal() {
    let machine = Machine::new();
    let staged = machine.staged(|_| {});
    let id = staged["id"].as_str().unwrap();
    machine.grant(id);
    for (key, value) in [("PRIVATEER_ROLE", "agent"), ("PRIVATEER_JOB", "job-1")] {
        let out = machine
            .privileged_run(id, &prefix(&staged), &[])
            .env(key, value)
            .env("PRIVATEER_ACTOR", "apps")
            .output()
            .unwrap();
        assert!(!out.status.success(), "ran with {key}");
        assert!(stderr(&out).contains(key), "{}", stderr(&out));
    }
    let out = machine
        .privileged_run(id, &prefix(&staged), &[])
        .env("PRIVATEER_TEST_TTY", machine.root.join("no-such-tty"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr(&out).contains("/dev/tty"), "{}", stderr(&out));
    // RPC is no terminal either.
    let mut rpc = machine
        .command()
        .arg("rpc")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    writeln!(
        rpc.stdin.take().unwrap(),
        "{}",
        json!({"id": "1", "command": "privileged.run", "args": {"id": id}})
    )
    .unwrap();
    let answered = rpc.wait_with_output().unwrap();
    let response: Value = serde_json::from_slice(&answered.stdout).unwrap();
    assert_eq!(response["ok"], false, "{response}");
    assert!(
        response["error"]
            .as_str()
            .unwrap()
            .contains("captain's terminal"),
        "{response}"
    );
    // None of those used the approval up.
    assert!(!machine.root.join("sudo.json").exists());
    let ran = machine
        .privileged_run(id, &prefix(&staged), &[])
        .output()
        .unwrap();
    assert!(ran.status.success(), "{}", stderr(&ran));
}

#[test]
fn a_timeout_ends_the_fake_sudo() {
    let machine = Machine::new();
    let staged = machine.staged(|_| {});
    let id = staged["id"].as_str().unwrap();
    machine.grant(id);
    let (sleeper, log) = machine.fake_sudo(true);
    let started = Instant::now();
    let out = machine
        .privileged_run(id, &prefix(&staged), &["--timeout-secs", "1"])
        .env("PRIVATEER_SUDO", sleeper)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(started.elapsed() < Duration::from_secs(20));
    let result: Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).lines().last().unwrap()).unwrap();
    assert_eq!(result["timed_out"], true, "{result}");
    let seen: Value = serde_json::from_str(&fs::read_to_string(log).unwrap()).unwrap();
    let pid = seen["pid"].as_i64().unwrap() as libc::pid_t;
    assert!(
        unsafe { libc::kill(pid, 0) } != 0,
        "the fake sudo survived the timeout"
    );
    let listed = machine.json("privileged.list", json!({}));
    assert_eq!(listed["requests"][0]["run"]["timed_out"], true);
    let error = machine.refused_run(id, &staged);
    assert!(error.contains("already ran"), "{error}");
}

#[test]
fn a_copy_from_another_machine_is_shown_without_actions() {
    let machine = Machine::new();
    let staged = machine.staged(|_| {});
    let id = staged["id"].as_str().unwrap();
    // As a desk copy collected from the machine that staged it is stored.
    machine.sql("UPDATE mail SET delivery='received' WHERE id=?1", &[&id]);
    let item = machine.need(id).unwrap();
    assert_eq!(item["actions"], json!([]));
    assert_eq!(item["privileged"]["held_elsewhere"], true);
    let mut args = json!({"id": "copy-grant", "request_id": id,
        "expected_version": staged["mail"]["payload"]["charter_hash"], "task_id": "apps-duty",
        "artifact": item["artifact"], "environment": item["environment"], "approver": "captain"});
    let error = machine.refused("approval.grant", args.clone());
    assert!(error.contains("another machine"), "{error}");
    args.as_object_mut().unwrap().remove("id");
    let error = machine.refused("approval.deny", args);
    assert!(error.contains("another machine"), "{error}");
}

impl Machine {
    fn refused_run(&self, id: &str, staged: &Value) -> String {
        let out = self
            .privileged_run(id, &prefix(staged), &[])
            .output()
            .unwrap();
        assert!(!out.status.success());
        stderr(&out)
    }
}
