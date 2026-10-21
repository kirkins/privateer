//! The MCP broker against a fake mcpbridge: real broker, proxy and dispatcher
//! processes, an isolated HOME, XDG data, ledger and fleet registry.
mod support;

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const BROKER: &str = env!("CARGO_BIN_EXE_privateer-mcp-broker");

fn fake() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp-broker/fake_mcpbridge.py")
}

/// The fake's argv, recording into `record`.
fn upstream(record: &Path, extra: &[&str]) -> Vec<String> {
    let mut argv = vec![
        fs::canonicalize(support::python())
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        "-I".into(),
        fake().to_string_lossy().into_owned(),
        "--record".into(),
        record.to_string_lossy().into_owned(),
    ];
    argv.extend(extra.iter().map(|s| s.to_string()));
    argv
}

fn eventually<T>(within: Duration, what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + within;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn records(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn alive(pid: i32) -> bool {
    // SAFETY: kill with signal 0 only checks.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// A broker the test started itself, stopped with SIGTERM when dropped.
struct Broker {
    child: Child,
    dir: PathBuf,
    record: PathBuf,
}

impl Broker {
    fn start(root: &Path, extra: &[&str], ready_wait: Option<u64>) -> Broker {
        let dir = root.join("state");
        let record = root.join("record.jsonl");
        let mut command = support::command(BROKER);
        command.args(["serve", "--state-dir"]).arg(&dir);
        command.args(["--name", "xcode"]);
        if let Some(secs) = ready_wait {
            command.args(["--ready-wait-secs", &secs.to_string()]);
        }
        command
            .arg("--")
            .args(upstream(&record, extra))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn().unwrap();
        let socket = dir.join("xcode.sock");
        eventually(Duration::from_secs(10), "the broker's socket", || {
            assert!(child.try_wait().unwrap().is_none(), "the broker exited");
            socket.exists().then_some(())
        });
        Broker { child, dir, record }
    }

    fn socket(&self) -> PathBuf {
        self.dir.join("xcode.sock")
    }

    fn records(&self) -> Vec<Value> {
        records(&self.record)
    }

    fn wait_record(&self, what: &str, check: impl Fn(&Value) -> bool) -> Value {
        eventually(Duration::from_secs(10), what, || {
            self.records().into_iter().find(|r| check(r))
        })
    }

    fn state(&self) -> Value {
        serde_json::from_slice(&fs::read(self.dir.join("xcode.state.json")).unwrap_or_default())
            .unwrap_or(Value::Null)
    }

    fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    fn stop(&mut self) {
        // SAFETY: signalling the child this test started.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let _ = self.child.wait();
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One crew client: a proxy process speaking newline-delimited JSON-RPC.
struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Value>,
    held: VecDeque<Value>,
}

impl Client {
    fn connect(socket: &Path) -> Client {
        Client::connect_from(socket, Path::new(env!("CARGO_MANIFEST_DIR")), 10)
    }

    fn connect_from(socket: &Path, cwd: &Path, wait: u64) -> Client {
        let mut child = support::command(BROKER)
            .args(["proxy", "--socket"])
            .arg(socket)
            .args(["--connect-wait-secs", &wait.to_string()])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(value) = serde_json::from_str(&line) {
                    if tx.send(value).is_err() {
                        break;
                    }
                }
            }
        });
        Client {
            stdin: child.stdin.take(),
            child,
            lines,
            held: VecDeque::new(),
        }
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(bytes).unwrap();
        stdin.flush().unwrap();
    }

    fn send(&mut self, message: Value) {
        self.send_raw(format!("{message}\n").as_bytes());
    }

    /// The first message, held or new, that `check` accepts; others are held.
    fn until(&mut self, what: &str, check: impl Fn(&Value) -> bool) -> Value {
        if let Some(at) = self.held.iter().position(&check) {
            return self.held.remove(at).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let message = self
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no {what}; held {:?}", self.held));
            if check(&message) {
                return message;
            }
            self.held.push_back(message);
        }
    }

    fn answer(&mut self, id: Value) -> Value {
        self.until(&format!("answer to {id}"), |m| {
            m.get("method").is_none() && m.get("id") == Some(&id)
        })
    }

    fn call(&mut self, id: Value, tool: &str, arguments: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments}}));
        self.answer(id)
    }

    fn initialize(&mut self, id: Value) -> Value {
        self.send(
            json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "test", "version": "1"}}}),
        );
        let answer = self.answer(id);
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        answer
    }

    fn exits_within(&mut self, within: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn text(result: &Value) -> &str {
    result["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("")
}

#[test]
fn two_clients_with_the_same_id_share_one_initialize_and_get_their_own_answers() {
    let temp = tempfile::tempdir().unwrap();
    let broker = Broker::start(temp.path(), &[], None);
    let mut a = Client::connect(&broker.socket());
    let mut b = Client::connect(&broker.socket());
    let first = a.initialize(json!(1));
    let second = b.initialize(json!(1));
    assert_eq!(first["result"]["serverInfo"]["name"], "fake-mcpbridge");
    assert_eq!(first["result"], second["result"]);
    a.send(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "echo", "arguments": {"text": "from a"}}}));
    b.send(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "echo", "arguments": {"text": "from b"}}}));
    assert_eq!(text(&a.answer(json!(1))), "from a");
    assert_eq!(text(&b.answer(json!(1))), "from b");
    let seen = broker.records();
    let initializes: Vec<_> = seen
        .iter()
        .filter(|r| r["method"] == "initialize")
        .collect();
    let initialized: Vec<_> = seen
        .iter()
        .filter(|r| r["method"] == "notifications/initialized")
        .collect();
    assert_eq!(initializes.len(), 1, "{seen:?}");
    assert_eq!(initialized.len(), 1, "{seen:?}");
    assert_eq!(initialized[0]["params"], json!({}));
    // Upstream ids are the broker's own, never the clients' clashing 1s.
    let ids: Vec<_> = seen
        .iter()
        .filter(|r| r["method"] == "tools/call")
        .map(|r| r["id"].clone())
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn a_client_that_disconnects_mid_call_cancels_it_and_no_one_else_notices() {
    let temp = tempfile::tempdir().unwrap();
    let mut broker = Broker::start(temp.path(), &[], None);
    let mut a = Client::connect(&broker.socket());
    let mut b = Client::connect(&broker.socket());
    a.send(json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
        "params": {"name": "slow", "arguments": {"secs": 5}}}));
    let call = broker.wait_record("the slow call upstream", |r| {
        r["method"] == "tools/call" && r["params"]["name"] == "slow"
    });
    drop(a);
    let cancelled = broker.wait_record("its cancellation", |r| {
        r["method"] == "notifications/cancelled"
    });
    assert_eq!(cancelled["params"]["requestId"], call["id"]);
    assert_eq!(
        text(&b.call(json!(1), "echo", json!({"text": "fine"}))),
        "fine"
    );
    assert!(broker.running());
    assert_eq!(broker.state()["client_count"], 1);
}

#[test]
fn an_upstream_crash_mid_call_is_reported_never_retried_and_the_next_call_works() {
    let temp = tempfile::tempdir().unwrap();
    let broker = Broker::start(temp.path(), &[], None);
    let mut a = Client::connect(&broker.socket());
    a.initialize(json!(1));
    let crashed = a.call(json!(2), "crash", json!({}));
    assert_eq!(crashed["result"]["isError"], true);
    assert_eq!(
        text(&crashed),
        "upstream exited; the call may or may not have run"
    );
    // Within the default ready wait, the restarted upstream takes it.
    let next = a.call(json!(3), "echo", json!({"text": "again"}));
    assert_eq!(text(&next), "again");
    let seen = broker.records();
    assert_eq!(
        seen.iter().filter(|r| r.get("started").is_some()).count(),
        2
    );
    assert_eq!(
        seen.iter().filter(|r| r["method"] == "initialize").count(),
        2
    );
    assert_eq!(
        seen.iter()
            .filter(|r| r["method"] == "notifications/initialized")
            .count(),
        2
    );
    assert_eq!(
        seen.iter()
            .filter(|r| r["params"]["name"] == "crash")
            .count(),
        1,
        "a call the upstream died on is never retried"
    );
    assert_eq!(broker.state()["upstream"]["restarts"], 1);
}

#[test]
fn a_slow_initialize_gives_a_busy_answer_within_the_ready_wait() {
    let temp = tempfile::tempdir().unwrap();
    let broker = Broker::start(temp.path(), &["--init-delay", "30"], Some(2));
    let mut a = Client::connect(&broker.socket());
    let started = Instant::now();
    let busy = a.call(json!(1), "echo", json!({"text": "x"}));
    let took = started.elapsed();
    assert!(took < Duration::from_secs(3), "{took:?}");
    assert!(took >= Duration::from_millis(1_900), "{took:?}");
    assert_eq!(busy["result"]["isError"], true);
    assert!(text(&busy).starts_with("Xcode tools busy: "), "{busy}");
    let after = busy["result"]["_meta"]["privateer/retry_after_secs"]
        .as_u64()
        .unwrap();
    assert!(text(&busy).ends_with(&format!("retry after {after} s")));
    let started = Instant::now();
    a.send(json!({"jsonrpc": "2.0", "id": "list", "method": "tools/list"}));
    let listed = a.answer(json!("list"));
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(
        listed["error"]["data"]["retry_after_secs"]
            .as_u64()
            .is_some(),
        "{listed}"
    );
    assert!(
        listed["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("Xcode tools busy: ")
    );
}

#[test]
fn one_broker_per_directory_private_modes_and_a_stale_socket_replaced() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("state");
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    // A socket file left by a broker that died.
    drop(UnixListener::bind(dir.join("xcode.sock")).unwrap());
    assert!(dir.join("xcode.sock").exists());
    let broker = Broker::start(temp.path(), &[], None);
    let mut a = Client::connect(&broker.socket());
    assert_eq!(text(&a.call(json!(1), "echo", json!({"text": "up"}))), "up");
    assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
    assert_eq!(fs::metadata(broker.socket()).unwrap().mode() & 0o777, 0o600);
    let pid: u32 = fs::read_to_string(dir.join("xcode.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(pid, broker.child.id());
    let started = Instant::now();
    let second = support::command(BROKER)
        .args(["serve", "--state-dir"])
        .arg(&dir)
        .args(["--name", "xcode", "--"])
        .args(upstream(&temp.path().join("second.jsonl"), &[]))
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("another broker holds"),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        !temp.path().join("second.jsonl").exists(),
        "the second started no upstream"
    );
    // The first is untouched.
    assert_eq!(
        text(&a.call(json!(2), "echo", json!({"text": "still"}))),
        "still"
    );
}

#[test]
fn server_requests_broadcasts_progress_batches_and_oversize_lines() {
    let temp = tempfile::tempdir().unwrap();
    let mut broker = Broker::start(temp.path(), &[], None);
    let mut a = Client::connect(&broker.socket());
    let mut b = Client::connect(&broker.socket());
    a.initialize(json!(1));
    b.initialize(json!(1));
    // The fake reports the error code its server request got back.
    assert_eq!(text(&a.call(json!(2), "ask", json!({}))), "-32601");
    // A notification reaches every client.
    assert_eq!(text(&a.call(json!(3), "notify", json!({}))), "notified");
    for client in [&mut a, &mut b] {
        let note = client.until("the broadcast", |m| m["method"] == "notifications/message");
        assert_eq!(note["params"]["data"], "hello everyone");
    }
    // Progress travels under the broker's token and comes back under ours,
    // to us alone.
    a.send(
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {
        "name": "slow", "arguments": {"secs": 0.4}, "_meta": {"progressToken": "tok-a"}}}),
    );
    let progress = a.until("progress", |m| m["method"] == "notifications/progress");
    assert_eq!(progress["params"]["progressToken"], "tok-a");
    assert_eq!(text(&a.answer(json!(4))), "slept");
    let upstream_token = broker
        .wait_record("the slow call", |r| r["params"]["name"] == "slow")["params"]["_meta"]
        ["progressToken"]
        .clone();
    assert_ne!(upstream_token, "tok-a");
    assert_eq!(text(&b.call(json!(9), "echo", json!({"text": "b"}))), "b");
    assert!(
        b.held
            .iter()
            .all(|m| m["method"] != "notifications/progress"),
        "{:?}",
        b.held
    );
    // A batch is refused.
    a.send(json!([{"jsonrpc": "2.0", "id": 5, "method": "ping"}]));
    let refused = a.until("the batch refusal", |m| m["error"]["code"] == -32600);
    assert_eq!(refused["error"]["message"], "batches are not supported");
    // An oversize line is refused; the connection and broker carry on.
    let mut huge = b"{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"ping\",\"pad\":\"".to_vec();
    huge.resize(huge.len() + 17 * 1024 * 1024, b'a');
    huge.extend_from_slice(b"\"}\n");
    a.send_raw(&huge);
    let oversize = a.until("the oversize refusal", |m| {
        m["error"]["message"] == "message exceeds 16 MiB"
    });
    assert_eq!(oversize["id"], Value::Null);
    assert_eq!(
        text(&a.call(json!(7), "echo", json!({"text": "after"}))),
        "after"
    );
    a.send(json!({"jsonrpc": "2.0", "id": 8, "method": "ping"}));
    assert_eq!(a.answer(json!(8))["result"], json!({}));
    assert!(broker.running());
}

#[test]
fn the_proxy_says_hello_from_its_real_cwd_and_exits_with_either_side() {
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real-workspace");
    fs::create_dir_all(&real).unwrap();
    let link = temp.path().join("linked");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let mut broker = Broker::start(temp.path(), &[], None);
    let mut a = Client::connect_from(&broker.socket(), &link, 10);
    let pid = a.child.id();
    let real = fs::canonicalize(&real).unwrap();
    let client = eventually(Duration::from_secs(10), "the hello", || {
        broker.state()["clients"]
            .as_array()
            .and_then(|c| c.iter().find(|c| c["pid"] == pid).cloned())
    });
    assert_eq!(client["cwd"], real.to_string_lossy().as_ref());
    // stdin closed: the proxy exits.
    drop(a.stdin.take());
    let status = a
        .exits_within(Duration::from_secs(3))
        .expect("exit on stdin EOF");
    assert!(status.success());
    // The broker closes: the proxy exits.
    let mut b = Client::connect(&broker.socket());
    assert_eq!(text(&b.call(json!(1), "echo", json!({"text": "hi"}))), "hi");
    broker.stop();
    assert!(
        b.exits_within(Duration::from_secs(5)).is_some(),
        "exit on broker close"
    );
    // No broker at all: one line on stderr, non-zero, after the wait.
    let started = Instant::now();
    let missing: Output = support::command(BROKER)
        .args(["proxy", "--socket"])
        .arg(temp.path().join("nobody/xcode.sock"))
        .args(["--connect-wait-secs", "1"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let took = started.elapsed();
    assert!(!missing.status.success());
    assert!(
        took >= Duration::from_secs(1) && took < Duration::from_secs(3),
        "{took:?}"
    );
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
}

#[test]
fn a_pinned_proxy_runs_through_the_stdio_launcher() {
    let output = support::command(support::python())
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mcp_broker_launcher.py"))
        .env("MCP_BROKER_BIN", BROKER)
        .output()
        .unwrap();
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // A skip (a checkout the executable rule refuses) is reported, not hidden.
    eprintln!("{report}");
    assert!(output.status.success(), "{report}");
}

/// A machine under `root`: HOME, XDG data, ledger and registry.
struct Machine {
    root: PathBuf,
}

impl Machine {
    fn command(&self, folder: &Path, args: &[&str]) -> Command {
        let mut command = support::isolated(&self.root);
        command
            .args(args)
            .current_dir(folder)
            .env("XDG_DATA_HOME", self.root.join("data"));
        command
    }

    fn run(&self, folder: &Path, args: &[&str]) -> Value {
        let output = self.command(folder, args).output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{args:?}: {stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
        // Pretty JSON whole, or a dispatcher's last JSON line.
        serde_json::from_str(&stdout)
            .ok()
            .or_else(|| {
                stdout.lines().rev().find_map(|line| {
                    serde_json::from_str::<Value>(line)
                        .ok()
                        .filter(Value::is_object)
                })
            })
            .unwrap_or(Value::Null)
    }

    fn ship(&self, name: &str) -> PathBuf {
        let folder = self.root.join(name);
        fs::create_dir_all(&folder).unwrap();
        self.run(&folder, &["init", "--ship", name, "--harness", "codex"]);
        folder
    }

    fn stable(&self) -> PathBuf {
        self.root
            .join("data/privateer/mcp-broker/bin/privateer-mcp-broker")
    }

    fn dir(&self) -> PathBuf {
        self.root.join("ledger/mcp-broker")
    }

    fn pass(&self, folder: &Path) -> Value {
        self.run(folder, &["serve", "--json", r#"{"once":true}"#])["mcp_broker"].clone()
    }
}

/// Stops a broker a dispatcher started, if the test ends early: it is
/// detached, so nothing else would.
struct Detached(PathBuf);

impl Drop for Detached {
    fn drop(&mut self) {
        let Some(pid) = fs::read_to_string(&self.0)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
        else {
            return;
        };
        let ours = fs::read_link(format!("/proc/{pid}/exe"))
            .is_ok_and(|exe| exe.to_string_lossy().contains("privateer-mcp-broker"));
        if ours {
            // SAFETY: signalling this test's own broker.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
}

fn sha256(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
}

#[test]
fn install_copies_only_a_missing_or_different_version() {
    let temp = tempfile::tempdir().unwrap();
    let machine = Machine {
        root: temp.path().to_path_buf(),
    };
    let here = temp.path();
    let first = machine.run(here, &["mcp-broker", "install"]);
    assert_eq!(first["action"], "installed");
    let stable = machine.stable();
    assert_eq!(first["binary"], stable.to_string_lossy().as_ref());
    assert_eq!(first["sha256"], sha256(&stable));
    assert_eq!(fs::read(&stable).unwrap(), fs::read(BROKER).unwrap());
    assert_eq!(fs::metadata(&stable).unwrap().mode() & 0o777, 0o755);
    let config: Value =
        serde_json::from_slice(&fs::read(machine.dir().join("xcode.json")).unwrap()).unwrap();
    assert_eq!(config, json!({"upstream": ["/usr/bin/xcrun", "mcpbridge"]}));
    assert_eq!(fs::metadata(machine.dir()).unwrap().mode() & 0o777, 0o700);
    assert_eq!(
        first["bundle_command"],
        json!([
            stable,
            "proxy",
            "--socket",
            machine.dir().join("xcode.sock")
        ])
    );
    let inode = fs::metadata(&stable).unwrap().ino();
    let again = machine.run(
        here,
        &["mcp-broker", "install", "--upstream", "/opt/x/mcp", "a"],
    );
    assert_eq!(again["action"], "unchanged");
    assert_eq!(fs::metadata(&stable).unwrap().ino(), inode);
    assert_eq!(again["sha256"], first["sha256"]);
    assert_eq!(again["upstream"], json!(["/opt/x/mcp", "a"]));
    // Another version at the stable path is replaced, by rename.
    fs::write(&stable, "#!/bin/sh\necho privateer-mcp-broker 0.0.0-old\n").unwrap();
    let old_inode = fs::metadata(&stable).unwrap().ino();
    let replaced = machine.run(here, &["mcp-broker", "install"]);
    assert_eq!(replaced["action"], "replaced");
    assert_ne!(fs::metadata(&stable).unwrap().ino(), old_inode);
    assert_eq!(replaced["sha256"], sha256(&stable));
    assert_eq!(fs::read(&stable).unwrap(), fs::read(BROKER).unwrap());
    // Relative upstreams refuse; agents may read status but not install.
    let relative = machine
        .command(
            here,
            &["mcp-broker", "install", "--upstream", "xcrun", "mcpbridge"],
        )
        .output()
        .unwrap();
    assert!(!relative.status.success());
    let agent = machine
        .command(here, &["mcp-broker", "install"])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "someone")
        .output()
        .unwrap();
    assert!(!agent.status.success());
    let status = machine
        .command(here, &["mcp-broker", "status"])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "someone")
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["installed"], true);
    assert_eq!(status["sha256"], sha256(&stable));
    assert_eq!(status["running"], false);
}

#[test]
fn the_owners_dispatcher_starts_the_broker_detached_and_stop_ends_its_upstream() {
    let temp = tempfile::tempdir().unwrap();
    let machine = Machine {
        root: temp.path().to_path_buf(),
    };
    let upkeep = machine.ship("upkeep");
    let other = machine.ship("other");
    let record = temp.path().join("record.jsonl");
    let mut install = vec!["mcp-broker", "install", "--upstream"];
    let argv = upstream(&record, &[]);
    install.extend(argv.iter().map(String::as_str));
    machine.run(&upkeep, &install);
    let pid_file = machine.dir().join("xcode.pid");
    let _detached = Detached(pid_file.clone());
    // No machine owner: no dispatcher starts it.
    assert_eq!(machine.pass(&upkeep), Value::Null);
    machine.run(&upkeep, &["machine", "owner", "upkeep"]);
    // Another ship's dispatcher never does.
    assert_eq!(machine.pass(&other), Value::Null);
    // Nor the owner's while the lock is held.
    let held = fs::File::open(machine.dir().join("xcode.lock"))
        .or_else(|_| {
            fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(machine.dir().join("xcode.lock"))
        })
        .unwrap();
    use std::os::fd::AsRawFd;
    // SAFETY: locking a file this test opened.
    assert_eq!(
        unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert_eq!(machine.pass(&upkeep), Value::Null);
    assert!(!pid_file.exists());
    assert!(!record.exists());
    drop(held);
    // Free: the owner's dispatcher starts the installed binary, detached.
    let started = machine.pass(&upkeep);
    let pid = started["started"]
        .as_i64()
        .unwrap_or_else(|| panic!("{started}")) as i32;
    eventually(Duration::from_secs(10), "the broker's pid file", || {
        (fs::read_to_string(&pid_file).ok()?.trim() == pid.to_string()).then_some(())
    });
    // SAFETY: getsid only reads.
    let (theirs, ours) = unsafe { (libc::getsid(pid), libc::getsid(0)) };
    assert_eq!(theirs, pid, "the broker leads its own session");
    assert_ne!(theirs, ours);
    assert_eq!(
        fs::read_link(format!("/proc/{pid}/exe")).ok(),
        Some(machine.stable())
    );
    // The dispatcher that started it is gone; the broker still serves.
    let mut client = Client::connect(&machine.dir().join("xcode.sock"));
    assert_eq!(
        text(&client.call(json!(1), "echo", json!({"text": "kept"}))),
        "kept"
    );
    drop(client);
    // Running: the next pass leaves it alone.
    assert_eq!(machine.pass(&upkeep), Value::Null);
    let status = machine.run(&upkeep, &["mcp-broker", "status"]);
    assert_eq!(status["running"], true);
    assert_eq!(status["pid"], pid);
    assert_eq!(status["broker"]["upstream"]["state"], "ready");
    let upstream_pid = status["broker"]["upstream"]["pid"].as_i64().unwrap() as i32;
    assert!(alive(upstream_pid));
    // Stop: the broker and its upstream are gone, and it stays stopped.
    let stopped = machine.run(&upkeep, &["mcp-broker", "stop"]);
    assert_eq!(stopped["stopped"]["upstream_gone"], true, "{stopped}");
    assert_eq!(stopped["stopped"]["running"], false);
    assert!(!alive(upstream_pid));
    eventually(Duration::from_secs(5), "the broker to exit", || {
        (!alive(pid)
            || fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |s| s.contains(") Z ")))
        .then_some(())
    });
    assert_eq!(machine.pass(&upkeep), Value::Null);
    // A config rewritten after install (an agent may write the ledger) is
    // never started.
    machine.run(&upkeep, &install);
    let config = machine.dir().join("xcode.json");
    let rewritten = json!({"upstream": ["/usr/bin/touch", temp.path().join("pwned")]});
    fs::write(&config, rewritten.to_string()).unwrap();
    let refused = machine.pass(&upkeep);
    assert!(
        refused["error"].as_str().unwrap().contains("differs"),
        "{refused}"
    );
    assert!(!temp.path().join("pwned").exists());
    // Install lifts the stop; removing the config stops it again.
    machine.run(&upkeep, &install);
    let restarted = machine.pass(&upkeep)["started"].as_i64().unwrap() as i32;
    eventually(Duration::from_secs(10), "the restarted broker", || {
        (fs::read_to_string(&pid_file).ok()?.trim() == restarted.to_string()).then_some(())
    });
    let upstream_pid = eventually(Duration::from_secs(10), "its upstream", || {
        let state: Value =
            serde_json::from_slice(&fs::read(machine.dir().join("xcode.state.json")).ok()?).ok()?;
        state["upstream"]["pid"].as_i64()
    }) as i32;
    fs::remove_file(machine.dir().join("xcode.json")).unwrap();
    let gone = machine.pass(&upkeep);
    assert_eq!(gone["stopped"]["upstream_gone"], true, "{gone}");
    assert!(!alive(upstream_pid));
}
