//! Exercise the native CLI boundary with a real controlling terminal. The
//! authority fixture is fictional and never contacts a fleet or a provider.
#![cfg(target_os = "linux")]
mod support;
use privateer::store::Store;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{fs, os::unix::process::CommandExt, path::PathBuf, process::Command};

struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    ledger: PathBuf,
    request: Value,
}
fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_privateer")
}
fn operator(command: &str, f: &Fixture, args: Value) -> Command {
    let mut cmd = support::isolated(f._temp.path());
    cmd.args([
        "--home",
        f.home.to_str().unwrap(),
        command,
        "--json",
        &args.to_string(),
    ]);
    cmd
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("alpha");
        let mut store = Store::open(&home).unwrap();
        let dir = temp.path().join("ledger");
        fs::create_dir(&dir).unwrap();
        fs::write(
            home.join("config.json"),
            json!({"resource_home":dir}).to_string(),
        )
        .unwrap();
        privateer::dispatch(&mut store, "resource.status", json!({})).unwrap();
        let ledger = dir.join("resources.sqlite");
        store
            .connection()
            .execute(
                "INSERT INTO ships(name,record) VALUES('alpha',?1)",
                [json!({"location":"here"}).to_string()],
            )
            .unwrap();
        let mut child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let ticks = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap();
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
        let namespace = fs::read_link("/proc/self/ns/pid").unwrap();
        let process = json!({"pid":pid,"started":format!("linux:{}:{ticks}",boot.trim()),"pid_namespace":namespace,"zombie":false});
        child.kill().unwrap();
        child.wait().unwrap();
        // Epoch zero is an explicit old fixture, not a claim about uptime.
        let job = json!({"id":"job-alpha","task_id":"task-alpha","incarnation":"incarnation-alpha",
            "command":["true"],"cwd":home,"action":"test","heavy":true,"timeout_secs":60,
            "ledger":ledger,"status":"failed","created_ms":0,"finished_ms":10,
            "supervisor":process,"payload":process,"cancel_requested":false,"exit_code":1,
            "reason":null,"retry_of":null,"launch_ms":1,"spawned_ms":2});
        store
            .connection()
            .execute(
                "INSERT INTO jobs(id,record) VALUES('job-alpha',?1)",
                [job.to_string()],
            )
            .unwrap();
        let key = format!("{}:job-alpha:incarnation-alpha", home.display());
        let hold = json!({"key":key,"home":home,"job_id":"job-alpha","incarnation":"incarnation-alpha",
            "heavy":true,"state":"failed","created_ms":0,"supervisor":process,"payload":process,"ship":"alpha","task":"task-alpha"});
        Connection::open(&ledger)
            .unwrap()
            .execute(
                "INSERT INTO reservations VALUES(?1,?2)",
                params![key, hold.to_string()],
            )
            .unwrap();
        let mut f = Self {
            _temp: temp,
            home,
            ledger,
            request: Value::Null,
        };
        let output = operator(
            "resource.recovery.reference",
            &f,
            json!({"job_id":"job-alpha"}),
        )
        .output()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let reference: Value = serde_json::from_slice(&output.stdout).unwrap();
        f.request = json!({"reference":reference["reference"],"authority":{"grant":"grant-alpha","resource":"resource-alpha",
            "grant_revision":"one","resource_revision":"one","visibility_revision":"one","link_incarnation":null},"idempotency_key":"key-alpha"});
        f
    }
    fn held(&self) -> bool {
        Connection::open(&self.ledger)
            .unwrap()
            .query_row("SELECT EXISTS(SELECT 1 FROM reservations)", [], |r| {
                r.get(0)
            })
            .unwrap()
    }
}

fn issue_at_terminal(f: &Fixture, background: bool) -> Value {
    let file = f._temp.path().join("request.json");
    fs::write(&file, json!({"request":f.request}).to_string()).unwrap();
    let script = r#"
import errno, json, os, pty, re, select, signal, sys, time
pid, fd = pty.fork()
if pid == 0:
    if sys.argv[4] == 'background':
        child = os.fork()
        if child:
            _, status = os.waitpid(child, 0)
            os._exit(os.waitstatus_to_exitcode(status))
        os.setpgid(0, 0)
        signal.signal(signal.SIGTTIN, signal.SIG_IGN)
        signal.signal(signal.SIGTTOU, signal.SIG_IGN)
    env = dict(os.environ)
    for name in ('PRIVATEER_ROLE', 'PRIVATEER_ACTOR', 'PRIVATEER_JOB', 'PRIVATEER_CREW'):
        env.pop(name, None)
    os.execve(sys.argv[1], [sys.argv[1], '--home', sys.argv[2], 'resource.recovery_permit.issue', '--file', sys.argv[3]], env)
data = b''
sent = False
deadline = time.monotonic() + 30
try:
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError('terminal fixture deadline exceeded')
        ready, _, _ = select.select([fd], [], [], remaining)
        if not ready:
            continue
        try:
            chunk = os.read(fd, 8192)
        except OSError as error:
            if error.errno != errno.EIO:
                raise
            break
        if not chunk:
            break
        data += chunk
        match = re.search(rb'Type ([0-9a-f]{8}) to confirm:', data)
        if match and not sent:
            os.write(fd, match[1] + b'\n')
            sent = True
    _, status = os.waitpid(pid, 0)
    pid = None
    print(json.dumps({'status':os.waitstatus_to_exitcode(status), 'output':data.decode()}))
finally:
    os.close(fd)
    if pid is not None:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
"#;
    let output = support::command(support::python())
        .env("PRIVATEER_FLEET", f._temp.path().join("fleet.sqlite"))
        .args([
            "-c",
            script,
            binary(),
            f.home.to_str().unwrap(),
            file.to_str().unwrap(),
            if background {
                "background"
            } else {
                "foreground"
            },
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn background_terminal_process_cannot_request_a_confirmation() {
    let f = Fixture::new();
    let terminal = issue_at_terminal(&f, true);
    assert_ne!(terminal["status"], 0, "{terminal}");
    let output = terminal["output"].as_str().unwrap();
    assert!(output.contains("terminal foreground"), "{terminal}");
    assert!(!output.contains("Type "), "{terminal}");
    assert!(f.held());
    assert!(
        !Connection::open(&f.ledger)
            .unwrap()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM resource_recovery_permits)",
                [],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
    );
}

#[test]
fn terminal_confirmation_issues_once_and_cli_replays_the_committed_result() {
    let f = Fixture::new();
    let terminal = issue_at_terminal(&f, false);
    assert_eq!(terminal["status"], 0, "{terminal}");
    let issued: Value = terminal["output"]
        .as_str()
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v.get("approval_id").is_some())
        .unwrap();
    let args = json!({"request":f.request,"approval_id":issued["approval_id"]});
    let broken = f._temp.path().join("broken-probe");
    fs::create_dir(&broken).unwrap();
    let ps = broken.join("ps");
    fs::write(&ps, "#!/bin/sh\nexit 1\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&ps, fs::Permissions::from_mode(0o700)).unwrap();
    let refused = operator("resource.recovery.apply", &f, args.clone())
        .env("PATH", &broken)
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("recovery_identity_unknown"));
    assert!(f.held());
    let applied = operator("resource.recovery.apply", &f, args.clone())
        .output()
        .unwrap();
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert!(!f.held());
    let replay = operator("resource.recovery.apply", &f, args)
        .output()
        .unwrap();
    assert!(
        replay.status.success(),
        "{}",
        String::from_utf8_lossy(&replay.stderr)
    );
    assert_eq!(applied.stdout, replay.stdout);
}

#[test]
fn peer_rpc_and_delivered_mail_cannot_execute_native_recovery() {
    let f = Fixture::new();
    let output = operator(
        "peer.put",
        &f,
        json!({"name":"beta","transport":"local","executable":binary(),"home":f.home}),
    )
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let conn = Connection::open(&f.ledger).unwrap();
    for (command, args) in [
        (
            "resource.recovery_permit.issue",
            json!({"request":f.request}),
        ),
        (
            "resource.recovery.apply",
            json!({"request":f.request,"approval_id":"permit-alpha"}),
        ),
        ("resource.recovery.reference", json!({"job_id":"job-alpha"})),
        (
            "resource.recovery_permit.revoke",
            json!({"approval_id":"permit-alpha"}),
        ),
    ] {
        let output = operator(
            "peer.call",
            &f,
            json!({"name":"beta","command":command,"args":args}),
        )
        .output()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["ok"], false, "{response}");
        assert_eq!(
            response["error"],
            "native recovery commands require the existing-store operator CLI"
        );
        // Even operator-carried mail with a command-shaped payload is only
        // mail; it cannot dispatch an action or echo terminal confirmation.
        let output = operator("mail.deliver", &f, json!({
            "id":format!("mail-{command}"),"from":"captain","to":"alpha","kind":"notice",
            "body":"operator confirmation","payload":{"command":command,"args":args,"confirmed":true}
        }))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(f.held());
        for table in [
            "resource_recovery_permits",
            "resource_recovery_results",
            "resource_recovery_audit",
        ] {
            assert!(
                !conn
                    .query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table})"), [], |r| {
                        r.get::<_, bool>(0)
                    })
                    .unwrap(),
                "{command} changed {table}"
            );
        }
    }
}

#[test]
fn stdin_approval_and_agent_issuance_are_not_terminal_authority() {
    let f = Fixture::new();
    let args = json!({"request":f.request});
    let mut cmd = operator("resource.recovery_permit.issue", &f, args.clone());
    // A new session has no controlling terminal, even when the harness does.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = cmd.output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("controlling terminal"));
    let out = operator("resource.recovery_permit.issue", &f, args)
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "alpha")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(f.held());
    let conn = Connection::open(&f.ledger).unwrap();
    assert!(
        !conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM resource_recovery_permits)",
                [],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
    );
}

#[test]
fn cli_refusals_preserve_legacy_schema_without_initializing_missing_homes() {
    let f = Fixture::new();
    let conn = Connection::open(&f.ledger).unwrap();
    conn.execute_batch("DROP TABLE resource_recovery_permits")
        .unwrap();
    let before: String = conn
        .query_row("SELECT group_concat(sql) FROM sqlite_master", [], |r| {
            r.get(0)
        })
        .unwrap();
    let out = operator(
        "resource.recovery.reference",
        &f,
        json!({"job_id":"job-alpha"}),
    )
    .output()
    .unwrap();
    assert!(!out.status.success());
    assert_eq!(
        before,
        conn.query_row::<String, _, _>("SELECT group_concat(sql) FROM sqlite_master", [], |r| r
            .get(0))
            .unwrap()
    );
    let missing = f._temp.path().join("missing");
    let out = support::command(binary())
        .env("PRIVATEER_FLEET", f._temp.path().join("fleet.sqlite"))
        .args([
            "--home",
            missing.to_str().unwrap(),
            "resource.recovery.reference",
            "--json",
            "{\"job_id\":\"job-alpha\"}",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!missing.exists());
    assert!(f.held());
}
