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

/// The canonical path of a process's executable: /proc on Linux, ps on
/// macOS (whose comm column is the full path).
fn exe_of(pid: i32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    let exe = fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    #[cfg(not(target_os = "linux"))]
    let exe = {
        let out = Command::new("/bin/ps")
            .args(["-o", "comm=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
    };
    fs::canonicalize(exe).ok()
}

/// A broker the test started itself, stopped with SIGTERM when dropped.
struct Broker {
    child: Child,
    dir: PathBuf,
    record: PathBuf,
}

impl Broker {
    fn start(root: &Path, extra: &[&str], ready_wait: Option<u64>) -> Broker {
        Broker::start_with(root, extra, ready_wait, &[])
    }

    /// With broker options (workspace pools, window helpers) before `--`.
    fn start_with(
        root: &Path,
        extra: &[&str],
        ready_wait: Option<u64>,
        options: &[String],
    ) -> Broker {
        let dir = root.join("state");
        let record = root.join("record.jsonl");
        let mut command = support::command(BROKER);
        command.args(["serve", "--state-dir"]).arg(&dir);
        command.args(["--name", "xcode"]);
        if let Some(secs) = ready_wait {
            command.args(["--ready-wait-secs", &secs.to_string()]);
        }
        command.args(options);
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
        Client::spawn(socket, cwd, wait, &[])
    }

    /// A proxy with extra options (`--project REL`).
    fn connect_with(socket: &Path, cwd: &Path, options: &[&str]) -> Client {
        Client::spawn(socket, cwd, 10, options)
    }

    fn spawn(socket: &Path, cwd: &Path, wait: u64, options: &[&str]) -> Client {
        let mut child = support::command(BROKER)
            .args(["proxy", "--socket"])
            .arg(socket)
            .args(["--connect-wait-secs", &wait.to_string()])
            .args(options)
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
fn symlinked_files_in_the_state_dir_are_refused_and_their_targets_untouched() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("state");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let victim = |name: &str| {
        let path = temp.path().join(format!("victim-{name}"));
        fs::write(&path, "untouched").unwrap();
        path
    };
    let untouched = |name: &str| {
        assert_eq!(
            fs::read_to_string(temp.path().join(format!("victim-{name}"))).unwrap(),
            "untouched",
            "{name}"
        )
    };
    let record = temp.path().join("record.jsonl");
    let serve = |command: &mut Command| {
        command
            .args(["serve", "--state-dir"])
            .arg(&dir)
            .args(["--name", "xcode", "--"])
            .args(upstream(&record, &[]))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
    };
    // A symlinked lock: the broker refuses to start.
    symlink(victim("lock"), dir.join("xcode.lock")).unwrap();
    let mut refused = support::command(BROKER);
    serve(&mut refused);
    let refused = refused.output().unwrap();
    assert!(!refused.status.success());
    assert!(!dir.join("xcode.sock").exists());
    untouched("lock");
    fs::remove_file(dir.join("xcode.lock")).unwrap();
    // A symlinked log, state, pid, and state temp file (named for the
    // broker's own pid: the shell execs the broker in its place).
    symlink(victim("log"), dir.join("xcode.log")).unwrap();
    symlink(victim("state"), dir.join("xcode.state.json")).unwrap();
    symlink(victim("pid"), dir.join("xcode.pid")).unwrap();
    let mut command = support::command("/bin/sh");
    command
        .arg("-c")
        .arg(r#"ln -s "$1" "$2/.xcode.state.json.$$.tmp" && shift 2 && exec "$@""#)
        .arg("sh")
        .arg(victim("temp"))
        .arg(&dir)
        .arg(BROKER);
    serve(&mut command);
    let mut child = command.spawn().unwrap();
    eventually(Duration::from_secs(10), "the broker's socket", || {
        assert!(child.try_wait().unwrap().is_none(), "the broker exited");
        dir.join("xcode.sock").exists().then_some(())
    });
    let mut client = Client::connect(&dir.join("xcode.sock"));
    assert_eq!(
        text(&client.call(json!(1), "echo", json!({"text": "fine"}))),
        "fine"
    );
    drop(client);
    // The state and pid were replaced by rename, not written through.
    eventually(Duration::from_secs(5), "a regular state file", || {
        fs::symlink_metadata(dir.join("xcode.state.json"))
            .is_ok_and(|m| m.is_file())
            .then_some(())
    });
    assert!(
        fs::symlink_metadata(dir.join("xcode.pid"))
            .unwrap()
            .is_file()
    );
    // SAFETY: signalling the broker this test started.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let status = child.wait().unwrap();
    assert!(status.success());
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
    assert!(stderr.contains("not logging to"), "{stderr}");
    for name in ["log", "state", "pid", "temp"] {
        untouched(name);
    }
    assert!(
        fs::symlink_metadata(dir.join("xcode.log"))
            .unwrap()
            .is_symlink()
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
    // The socket's own report of the peer (SO_PEERCRED, LOCAL_PEERPID),
    // beside the hello's claim.
    assert_eq!(client["peer_pid"], pid);
    // stdin closed: the proxy exits.
    drop(a.stdin.take());
    let status = a
        .exits_within(Duration::from_secs(3))
        .expect("exit on stdin EOF");
    assert!(status.success());
    // The broker closes: the proxy exits.
    let mut b = Client::connect(&broker.socket());
    assert_eq!(text(&b.call(json!(1), "echo", json!({"text": "hi"}))), "hi");
    // A call in flight when the broker is stopped is answered before it exits.
    b.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "slow", "arguments": {"secs": 30}}}));
    broker.wait_record("the slow call", |r| r["params"]["name"] == "slow");
    broker.stop();
    assert_eq!(
        text(&b.answer(json!(2))),
        "upstream exited; the call may or may not have run"
    );
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
    assert!(output.status.success(), "{report}");
    // unittest -v reports a skip as: test_name (...) ... skipped 'REASON'
    let skip = report
        .lines()
        .find_map(|line| line.split_once(" ... skipped ").map(|(_, why)| why.trim()));
    match skip {
        None => {
            assert!(report.contains(" ... ok"), "{report}");
            println!("RAN a_pinned_proxy_runs_through_the_stdio_launcher");
        }
        Some(reason) => {
            // Shown with --nocapture and in any failure: never a silent pass.
            let line = format!("SKIPPED a_pinned_proxy_runs_through_the_stdio_launcher: {reason}");
            println!("{line}");
            eprintln!("{line}");
            // The one documented skip: a checkout below a path the
            // executable rule refuses. Under CI, anything else fails.
            let path_rule = reason.contains("rule refuses");
            assert!(
                path_rule || std::env::var_os("CI").is_none(),
                "{line}\n{report}"
            );
        }
    }
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

    /// The broker's run directory: under XDG data, never the ledger.
    fn dir(&self) -> PathBuf {
        self.root.join("data/privateer/mcp-broker/run")
    }

    fn ledger(&self) -> PathBuf {
        self.root.join("ledger")
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
        let ours =
            exe_of(pid).is_some_and(|exe| exe.to_string_lossy().contains("privateer-mcp-broker"));
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
    assert_eq!(config["upstream"], json!(["/usr/bin/xcrun", "mcpbridge"]));
    // Privateer's own workspace pool, <ledger>/worktrees, is always there.
    let own_pool = fs::canonicalize(machine.ledger().join("worktrees")).unwrap();
    assert_eq!(config["workspace_pools"], json!([own_pool]));
    // The Mac's open and osascript by default; no helper elsewhere.
    if cfg!(target_os = "macos") {
        assert_eq!(
            config["window_open"],
            json!(["/usr/bin/open", "-g", "-a", "Xcode"])
        );
        assert_eq!(config["window_close"][0], "/usr/bin/osascript");
    } else {
        assert!(config.get("window_open").is_none(), "{config}");
        assert!(config.get("window_close").is_none(), "{config}");
    }
    assert_eq!(fs::metadata(machine.dir()).unwrap().mode() & 0o777, 0o700);
    // Every broker file is in the run directory, outside the ledger that
    // sandboxed agents may write; nothing of the broker's is in the ledger.
    for key in ["config", "socket"] {
        let path = PathBuf::from(first[key].as_str().unwrap());
        assert_eq!(path.parent(), Some(machine.dir().as_path()), "{key}");
        assert!(!path.starts_with(machine.ledger()), "{key}");
    }
    assert!(!machine.ledger().join("mcp-broker").exists());
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
    assert_eq!(status["run_dir"], machine.dir().to_string_lossy().as_ref());
    // A run directory inside the ledger is refused.
    let inside = machine
        .command(here, &["mcp-broker", "install"])
        .env("XDG_DATA_HOME", machine.ledger().join("data"))
        .output()
        .unwrap();
    assert!(!inside.status.success());
    assert!(
        String::from_utf8_lossy(&inside.stderr).contains("lies inside"),
        "{}",
        String::from_utf8_lossy(&inside.stderr)
    );
}

#[test]
fn the_owners_dispatcher_starts_the_broker_detached_and_stop_ends_its_upstream() {
    let temp = tempfile::tempdir().unwrap();
    let machine = Machine {
        root: temp.path().to_path_buf(),
    };
    let upkeep = machine.ship("upkeep");
    let other = machine.ship("other");
    // The owner's home keeps its ledger elsewhere: the broker's files do not
    // follow the ledger, so an install from outside any ship folder (no
    // home) is the one its dispatcher finds.
    let home_config = upkeep.join(".privateer/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&home_config).unwrap()).unwrap();
    config["resource_home"] = json!(temp.path().join("owner-ledger"));
    fs::write(&home_config, config.to_string()).unwrap();
    let record = temp.path().join("record.jsonl");
    let mut install = vec!["mcp-broker", "install", "--upstream"];
    let argv = upstream(&record, &[]);
    install.extend(argv.iter().map(String::as_str));
    machine.run(temp.path(), &install);
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
    assert_eq!(exe_of(pid), fs::canonicalize(machine.stable()).ok());
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
    // The dispatcher passed the pinned config's pools to the broker.
    assert!(!status["workspace_pools"].as_array().unwrap().is_empty());
    assert_eq!(
        status["broker"]["workspace_pools"],
        status["workspace_pools"]
    );
    let upstream_pid = status["broker"]["upstream"]["pid"].as_i64().unwrap() as i32;
    assert!(alive(upstream_pid));
    // A state file naming another process group as the upstream: stop never
    // signals it, and the broker still ends its own upstream.
    let mut bystander = Command::new("sleep");
    std::os::unix::process::CommandExt::process_group(&mut bystander, 0);
    let mut bystander = bystander.arg("60").spawn().unwrap();
    let bystander_pid = bystander.id() as i32;
    let state_file = machine.dir().join("xcode.state.json");
    let mut forged: Value = serde_json::from_slice(&fs::read(&state_file).unwrap()).unwrap();
    forged["upstream"]["pid"] = json!(bystander_pid);
    let forged_temp = machine.dir().join("forged.tmp");
    fs::write(&forged_temp, forged.to_string()).unwrap();
    fs::rename(&forged_temp, &state_file).unwrap();
    // Stop: the broker and its upstream are gone, and it stays stopped.
    let stopped = machine.run(&upkeep, &["mcp-broker", "stop"]);
    assert_eq!(
        stopped["stopped"]["unverified_upstream_pid"], bystander_pid,
        "{stopped}"
    );
    assert_eq!(stopped["stopped"]["upstream_pid"], Value::Null, "{stopped}");
    assert_eq!(stopped["stopped"]["running"], false);
    assert!(
        bystander.try_wait().unwrap().is_none(),
        "the forged group was signalled"
    );
    let _ = bystander.kill();
    let _ = bystander.wait();
    eventually(
        Duration::from_secs(5),
        "the broker's own upstream to end",
        || (!alive(upstream_pid)).then_some(()),
    );
    eventually(Duration::from_secs(5), "the broker to exit", || {
        (!alive(pid)
            || fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |s| s.contains(") Z ")))
        .then_some(())
    });
    assert_eq!(machine.pass(&upkeep), Value::Null);
    // A config rewritten after install (defence in depth: agents cannot
    // write the run directory) is
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
    // A symlinked log in the run directory: the dispatcher refuses to open
    // it, starts nothing, and the link's target is untouched.
    machine.run(&upkeep, &install);
    let victim = temp.path().join("victim");
    fs::write(&victim, "untouched").unwrap();
    let _ = fs::remove_file(machine.dir().join("xcode.log"));
    std::os::unix::fs::symlink(&victim, machine.dir().join("xcode.log")).unwrap();
    let refused = machine.pass(&upkeep);
    assert!(
        refused["error"].as_str().unwrap().contains("xcode.log"),
        "{refused}"
    );
    assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");
    assert!(!pid_file.exists());
    fs::remove_file(machine.dir().join("xcode.log")).unwrap();
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

// ---- Step 3: windows and routing -------------------------------------------

fn window_helper() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp-broker/fake_xcode_window.py")
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Real git worktrees under a temporary workspace pool, laid out as
/// Treehouse leaves them, and the fake Xcode their windows live in.
struct Workspaces {
    root: PathBuf,
    pool: PathBuf,
    repo: PathBuf,
    /// Xcode's windows, as the fake mcpbridge lists them.
    windows: PathBuf,
    opened: PathBuf,
    closed: PathBuf,
}

impl Workspaces {
    fn new(temp: &Path) -> Workspaces {
        let root = fs::canonicalize(temp).unwrap();
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet", "--initial-branch=main"]);
        fs::write(repo.join("README"), "base\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "--quiet", "-m", "base"]);
        let pool = root.join("pool");
        fs::create_dir_all(&pool).unwrap();
        Workspaces {
            windows: root.join("windows.json"),
            opened: root.join("opened.jsonl"),
            closed: root.join("closed.jsonl"),
            root,
            pool,
            repo,
        }
    }

    /// A new worktree, with these directories in it.
    fn worktree(&self, slot: &str, dirs: &[&str]) -> PathBuf {
        let path = self.pool.join("home-hash").join(slot).join("repo");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        git(
            &self.repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                path.to_str().unwrap(),
            ],
        );
        for dir in dirs {
            fs::create_dir_all(path.join(dir)).unwrap();
        }
        path
    }

    /// Windows already open in Xcode, which the broker did not open.
    fn seed(&self, windows: Value) {
        fs::write(&self.windows, windows.to_string()).unwrap();
    }

    fn listed(&self) -> Vec<Value> {
        fs::read(&self.windows)
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<Value>>(&b).ok())
            .unwrap_or_default()
    }

    fn helper(&self, action: &str, extra: &[&str]) -> String {
        let log = if action == "open" {
            &self.opened
        } else {
            &self.closed
        };
        let mut argv = vec![
            fs::canonicalize(support::python())
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            "-I".into(),
            window_helper().to_string_lossy().into_owned(),
            action.into(),
            "--windows".into(),
            self.windows.to_string_lossy().into_owned(),
            "--log".into(),
            log.to_string_lossy().into_owned(),
        ];
        argv.extend(extra.iter().map(|s| s.to_string()));
        serde_json::to_string(&argv).unwrap()
    }

    /// The broker's options: this pool, and the fake helpers.
    fn options(&self, open: &[&str], close: &[&str]) -> Vec<String> {
        vec![
            "--workspace-pool".into(),
            self.pool.to_string_lossy().into_owned(),
            "--window-open".into(),
            self.helper("open", open),
            "--window-close".into(),
            self.helper("close", close),
        ]
    }

    /// A broker in `name` under the root, its upstream listing this Xcode.
    fn broker(&self, name: &str, open: &[&str], text: bool, ready_wait: Option<u64>) -> Broker {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let windows = self.windows.to_string_lossy().into_owned();
        let mut upstream = vec!["--windows", windows.as_str()];
        if text {
            upstream.push("--windows-text");
        }
        Broker::start_with(&dir, &upstream, ready_wait, &self.options(open, &[]))
    }

    fn opens(&self) -> Vec<Value> {
        records(&self.opened)
    }

    fn closes(&self) -> Vec<Value> {
        records(&self.closed)
    }
}

/// The upstream's tools/call arguments for `tool`, as it received them.
fn received(broker: &Broker, tool: &str) -> Vec<Value> {
    broker
        .records()
        .into_iter()
        .filter(|r| r["method"] == "tools/call" && r["params"]["name"] == tool)
        .map(|r| r["params"]["arguments"].clone())
        .collect()
}

fn is_error(answer: &Value) -> bool {
    answer["result"]["isError"] == true
}

/// One control line to the broker, and its one-line answer.
fn control(socket: &Path, line: Value) -> Value {
    let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(40)))
        .unwrap();
    writeln!(stream, "{line}").unwrap();
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer).unwrap();
    serde_json::from_str(&answer).unwrap_or_else(|_| panic!("{answer:?}"))
}

fn close_workspace(socket: &Path, root: &Path) -> Value {
    control(
        socket,
        json!({"privateer_control": 1, "close_workspace": root}),
    )
}

#[test]
fn each_workspace_builds_in_its_own_window_and_a_foreign_tab_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspaces::new(temp.path());
    let a = ws.worktree("1", &["A.xcodeproj"]);
    let b = ws.worktree("2", &["B.xcodeproj", "src"]);
    ws.seed(json!([
        {"tabIdentifier": "tab-a", "workspacePath": a.join("A.xcodeproj")},
        {"tabIdentifier": "tab-b", "workspacePath": b.join("B.xcodeproj")},
    ]));
    let broker = ws.broker("one", &[], false, None);
    let mut first = Client::connect_from(&broker.socket(), &a, 10);
    // Below the checkout: the nearest .git ancestor inside the pool.
    let mut second = Client::connect_from(&broker.socket(), &b.join("src"), 10);
    first.send(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "BuildProject", "arguments": {}}}));
    second.send(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "BuildProject"}}));
    assert_eq!(text(&first.answer(json!(1))), "built in tab tab-a");
    assert_eq!(text(&second.answer(json!(1))), "built in tab tab-b");
    let mut tabs: Vec<Value> = received(&broker, "BuildProject")
        .into_iter()
        .map(|a| a["tabIdentifier"].clone())
        .collect();
    tabs.sort_by_key(|t| t.to_string());
    assert_eq!(tabs, [json!("tab-a"), json!("tab-b")]);
    // Another workspace's tab is refused and never forwarded; its own passes.
    let refused = first.call(json!(2), "BuildProject", json!({"tabIdentifier": "tab-b"}));
    assert!(is_error(&refused), "{refused}");
    assert_eq!(
        text(&refused),
        "tabIdentifier tab-b is not this workspace's Xcode window; call XcodeListWindows"
    );
    let own = first.call(json!(3), "BuildProject", json!({"tabIdentifier": "tab-a"}));
    assert_eq!(text(&own), "built in tab tab-a");
    let to_b = received(&broker, "BuildProject")
        .into_iter()
        .filter(|a| a["tabIdentifier"] == "tab-b")
        .count();
    assert_eq!(to_b, 1, "only the second client's own call reached tab-b");
    // Windows that were already open: nothing was opened, nothing recorded.
    assert!(ws.opens().is_empty());
    let state = broker.state();
    let roots: Vec<&Value> = state["clients"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| &c["root"])
        .collect();
    assert!(
        roots.contains(&&json!(a)) && roots.contains(&&json!(b)),
        "{state}"
    );
    assert_eq!(state["windows"], json!([]));
}

#[test]
fn window_lists_are_filtered_to_the_clients_workspace_in_both_forms() {
    for text_form in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let ws = Workspaces::new(temp.path());
        let a = ws.worktree("1", &["A.xcodeproj"]);
        let b = ws.worktree("2", &["B.xcodeproj"]);
        ws.seed(json!([
            {"tabIdentifier": "tab-a", "workspacePath": a.join("A.xcodeproj")},
            {"tabIdentifier": "tab-b", "workspacePath": b.join("B.xcodeproj")},
            {"tabIdentifier": "tab-x", "workspacePath": ws.root.join("elsewhere/X.xcodeproj")},
        ]));
        let broker = ws.broker("one", &[], text_form, None);
        let outside = ws.root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let mut mine = Client::connect_from(&broker.socket(), &a, 10);
        let mut stranger = Client::connect_from(&broker.socket(), &outside, 10);
        let listed = mine.call(json!(1), "XcodeListWindows", json!({}));
        let shown = listed.to_string();
        assert!(shown.contains("tab-a"), "{shown}");
        assert!(
            !shown.contains("tab-b") && !shown.contains("tab-x"),
            "{shown}"
        );
        if !text_form {
            assert_eq!(
                listed["result"]["structuredContent"]["windows"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
        }
        let none = stranger.call(json!(1), "XcodeListWindows", json!({}));
        let shown = none.to_string();
        assert!(!is_error(&none), "{shown}");
        assert!(!shown.contains("tab-"), "{shown}");
    }
}

#[test]
fn the_socket_peer_decides_the_workspace_never_the_hello() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspaces::new(temp.path());
    let a = ws.worktree("1", &["A.xcodeproj"]);
    let b = ws.worktree("2", &["B.xcodeproj"]);
    ws.seed(json!([
        {"tabIdentifier": "tab-a", "workspacePath": a.join("A.xcodeproj")},
        {"tabIdentifier": "tab-b", "workspacePath": b.join("B.xcodeproj")},
    ]));
    let broker = ws.broker("one", &[], false, None);
    // A raw client in workspace a whose hello claims workspace b.
    let script = r#"import json, socket, sys
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
f = s.makefile('rw')
f.write(json.dumps({"privateer_proxy": 1, "cwd": sys.argv[2], "pid": 1}) + "\n")
f.write(json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": "BuildProject", "arguments": {}}}) + "\n")
f.flush()
print(f.readline().strip())
"#;
    let out = support::command(support::python())
        .args(["-I", "-c", script])
        .arg(broker.socket())
        .arg(&b)
        .current_dir(&a)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let answer: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(text(&answer), "built in tab tab-a");
    let log = fs::read_to_string(broker.dir.join("xcode.log")).unwrap();
    assert!(
        log.lines()
            .any(|l| l.contains("claims cwd") && l.contains("but its working directory is")),
        "{log}"
    );
    // Outside every pool: window tools refused, window-free tools pass.
    let outside = ws.root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let mut stranger = Client::connect_from(&broker.socket(), &outside, 10);
    let refused = stranger.call(json!(1), "BuildProject", json!({}));
    assert!(is_error(&refused));
    assert_eq!(
        text(&refused),
        "Xcode window tools work only from a Privateer task workspace"
    );
    let named = stranger.call(json!(2), "BuildProject", json!({"tabIdentifier": "tab-a"}));
    assert!(is_error(&named), "{named}");
    let docs = stranger.call(json!(3), "DocumentationSearch", json!({"query": "SwiftUI"}));
    assert_eq!(text(&docs), "found SwiftUI");
    assert_eq!(received(&broker, "BuildProject").len(), 1);
    // The state file: the proxy's own pid, as the socket reports it.
    let mut mine = Client::connect_from(&broker.socket(), &a, 10);
    let pid = mine.child.id();
    mine.initialize(json!(0));
    let client = eventually(Duration::from_secs(10), "the client", || {
        broker.state()["clients"]
            .as_array()
            .and_then(|c| c.iter().find(|c| c["pid"] == pid).cloned())
    });
    assert_eq!(client["peer_pid"], pid);
    assert_eq!(client["root"], json!(a));
    assert_eq!(client["peer_cwd"], json!(a));
}

#[test]
fn path_arguments_stay_inside_the_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspaces::new(temp.path());
    let a = ws.worktree("1", &["A.xcodeproj", "src"]);
    let b = ws.worktree("2", &[]);
    ws.seed(json!([{"tabIdentifier": "tab-a", "workspacePath": a.join("A.xcodeproj")}]));
    let broker = ws.broker("one", &[], false, None);
    let mut client = Client::connect_from(&broker.socket(), &a, 10);
    let outside = client.call(json!(1), "XcodeRead", json!({"filePath": b.join("README")}));
    assert!(is_error(&outside), "{outside}");
    assert!(
        text(&outside).contains("lies outside this workspace"),
        "{outside}"
    );
    let climbing = client.call(json!(2), "XcodeRead", json!({"filePath": "src/../../x"}));
    assert!(is_error(&climbing), "{climbing}");
    let inside = a.join("src/main.swift");
    let ok = client.call(json!(3), "XcodeRead", json!({"filePath": inside}));
    assert_eq!(text(&ok), format!("read {} in tab tab-a", inside.display()));
    // Plural and URL names, and arrays of paths, are checked too.
    for (n, arguments) in [
        json!({"filePaths": [inside, b.join("README")]}),
        json!({"uri": format!("file://{}", b.join("README").display())}),
        json!({"file": "src/../../2/repo/README"}),
    ]
    .into_iter()
    .enumerate()
    {
        let refused = client.call(json!(10 + n), "XcodeRead", arguments);
        assert!(is_error(&refused), "{refused}");
    }
    let forwarded = received(&broker, "XcodeRead");
    assert_eq!(forwarded.len(), 1, "{forwarded:?}");
    assert_eq!(forwarded[0]["filePath"], json!(inside));
}

#[test]
fn the_first_window_call_opens_the_workspace_window_once() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspaces::new(temp.path());
    let a = ws.worktree(
        "1",
        &[
            "App.xcodeproj/project.xcworkspace",
            "App.xcworkspace",
            "Pods/Pods.xcworkspace",
            "DerivedData/Build.xcworkspace",
            ".hidden/Hidden.xcworkspace",
        ],
    );
    // The window shows a second after the open helper exits.
    let broker = ws.broker("one", &["--delay", "1"], false, None);
    let mut first = Client::connect_from(&broker.socket(), &a, 10);
    let mut second = Client::connect_from(&broker.socket(), &a, 10);
    for client in [&mut first, &mut second] {
        client.send(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "BuildProject", "arguments": {}}}));
    }
    assert_eq!(text(&first.answer(json!(1))), "built in tab tab-1");
    assert_eq!(text(&second.answer(json!(1))), "built in tab tab-1");
    let opens = ws.opens();
    assert_eq!(opens.len(), 1, "{opens:?}");
    let project = a.join("App.xcworkspace");
    assert_eq!(
        opens[0]["argv"].as_array().unwrap().last().unwrap(),
        &json!(project)
    );
    assert_eq!(opens[0]["cwd"], "/");
    let record: Value =
        serde_json::from_slice(&fs::read(broker.dir.join("xcode.windows.json")).unwrap()).unwrap();
    let windows = record["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 1, "{record}");
    assert_eq!(windows[0]["root"], json!(a));
    assert_eq!(windows[0]["project"], json!(project));
    assert_eq!(windows[0]["tab"], "tab-1");
    assert_eq!(broker.state()["windows"][0]["tab"], "tab-1");

    // Ambiguous: refused with the candidates; --project picks one, and a
    // --project outside the workspace is refused.
    let c = ws.worktree("3", &["X.xcodeproj", "Y.xcodeproj"]);
    let mut unsure = Client::connect_from(&broker.socket(), &c, 10);
    let refused = unsure.call(json!(1), "BuildProject", json!({}));
    assert!(is_error(&refused), "{refused}");
    assert!(
        text(&refused).contains("X.xcodeproj, Y.xcodeproj"),
        "{refused}"
    );
    let mut sure = Client::connect_with(&broker.socket(), &c, &["--project", "Y.xcodeproj"]);
    assert_eq!(
        text(&sure.call(json!(1), "BuildProject", json!({}))),
        "built in tab tab-2"
    );
    assert_eq!(
        ws.opens().last().unwrap()["argv"]
            .as_array()
            .unwrap()
            .last()
            .unwrap(),
        &json!(c.join("Y.xcodeproj"))
    );
    let d = ws.worktree("4", &["Z.xcodeproj"]);
    let mut escaping = Client::connect_with(
        &broker.socket(),
        &d,
        &["--project", "../../1/repo/App.xcworkspace"],
    );
    let refused = escaping.call(json!(1), "BuildProject", json!({}));
    assert!(is_error(&refused), "{refused}");
    assert!(text(&refused).contains("--project"), "{refused}");
    assert_eq!(ws.opens().len(), 2);
}

/// The XcodeListWindows calls the upstream has received.
fn window_lists(broker: &Broker) -> usize {
    received(broker, "XcodeListWindows").len()
}

#[test]
fn a_slow_open_runs_once_and_every_caller_is_busy_by_its_deadline() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspaces::new(temp.path());
    let a = ws.worktree("1", &["A.xcodeproj"]);
    // The open helper runs for 4 s, past the 2 s each call may wait.
    let broker = ws.broker("one", &["--hold", "4"], false, Some(2));
    let mut first = Client::connect_from(&broker.socket(), &a, 10);
    let mut second = Client::connect_from(&broker.socket(), &a, 10);
    // Ready, with the tool list cached, so the waits below are the window's.
    for client in [&mut first, &mut second] {
        assert_eq!(
            text(&client.call(json!(1), "DocumentationSearch", json!({"query": "q"}))),
            "found q"
        );
    }
    let busy = "Xcode tools busy: Xcode is opening this workspace's window; retry after 15 s";
    let first_sent = Instant::now();
    first.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "BuildProject", "arguments": {}}}));
    eventually(Duration::from_secs(5), "the open helper", || {
        (!ws.opens().is_empty()).then_some(())
    });
    let second_sent = Instant::now();
    second.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "BuildProject", "arguments": {}}}));
    let answer = first.answer(json!(2));
    let first_took = first_sent.elapsed();
    assert_eq!(text(&answer), busy, "{answer}");
    let answer = second.answer(json!(2));
    let second_took = second_sent.elapsed();
    assert_eq!(text(&answer), busy, "{answer}");
    // The opener is not held for the helper, and the waiter waited on the
    // running open (the grace path would have polled the window list).
    assert!(first_took < Duration::from_secs(3), "{first_took:?}");
    assert!(second_took < Duration::from_secs(3), "{second_took:?}");
    assert_eq!(window_lists(&broker), 2, "one look each before the open");
    assert_eq!(ws.opens().len(), 1);
    // The helper ends and its window is recorded; the next call uses it.
    eventually(Duration::from_secs(10), "the window", || {
        (!ws.listed().is_empty()).then_some(())
    });
    eventually(Duration::from_secs(10), "the record", || {
        (broker.state()["windows"].as_array()?.len() == 1).then_some(())
    });
    assert_eq!(
        text(&first.call(json!(3), "BuildProject", json!({}))),
        "built in tab tab-1"
    );
    assert_eq!(ws.opens().len(), 1, "{:?}", ws.opens());
    assert!(received(&broker, "BuildProject").len() == 1);
}

#[test]
fn a_workspace_released_while_its_window_opens_closes_it_when_the_open_ends() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspaces::new(temp.path());
    let a = ws.worktree("1", &["A.xcodeproj"]);
    let broker = ws.broker("one", &["--hold", "3"], false, Some(1));
    let mut client = Client::connect_from(&broker.socket(), &a, 10);
    assert_eq!(
        text(&client.call(json!(1), "DocumentationSearch", json!({"query": "q"}))),
        "found q"
    );
    let busy = client.call(json!(2), "BuildProject", json!({}));
    assert!(text(&busy).contains("is opening"), "{busy}");
    // Recorded as it opens, so a release asks; it closes when the open ends.
    let project = a.join("A.xcodeproj");
    let recorded: Value =
        serde_json::from_slice(&fs::read(broker.dir.join("xcode.windows.json")).unwrap()).unwrap();
    assert_eq!(recorded["windows"][0]["project"], json!(project));
    let started = Instant::now();
    assert_eq!(
        close_workspace(&broker.socket(), &a),
        json!({"closed": [], "opening": [project]})
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    let closes = eventually(Duration::from_secs(10), "the close", || {
        let closes = ws.closes();
        (!closes.is_empty()).then_some(closes)
    });
    assert_eq!(closes.len(), 1);
    assert_eq!(
        closes[0]["argv"].as_array().unwrap().last().unwrap(),
        &json!(project)
    );
    eventually(Duration::from_secs(10), "an empty record", || {
        (broker.state()["windows"] == json!([])).then_some(())
    });
    assert!(ws.listed().is_empty(), "{:?}", ws.listed());
}

#[test]
fn a_tool_missing_from_the_list_is_listed_again_and_never_given_a_window() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspaces::new(temp.path());
    let a = ws.worktree("1", &["A.xcodeproj"]);
    ws.seed(json!([{"tabIdentifier": "tab-a", "workspacePath": a.join("A.xcodeproj")}]));
    let broker = ws.broker("one", &[], false, None);
    let mut client = Client::connect_from(&broker.socket(), &a, 10);
    assert_eq!(
        text(&client.call(json!(1), "DocumentationSearch", json!({"query": "q"}))),
        "found q"
    );
    let lists = || {
        broker
            .records()
            .into_iter()
            .filter(|r| r["method"] == "tools/list")
            .count()
    };
    let before = lists();
    let refused = client.call(json!(2), "Mystery", json!({"tabIdentifier": "tab-a"}));
    assert!(is_error(&refused), "{refused}");
    assert!(text(&refused).contains("does not list"), "{refused}");
    assert_eq!(lists(), before + 1, "listed again, once");
    // Without a tab it goes up as a window-free call.
    let forwarded = client.call(json!(3), "Mystery", json!({}));
    assert_eq!(forwarded["error"]["message"], "unknown tool", "{forwarded}");
    assert_eq!(lists(), before + 2);
    let sent = received(&broker, "Mystery");
    assert_eq!(sent, [json!({})]);
}

#[test]
fn a_window_that_never_shows_is_busy_and_a_failed_open_is_an_error() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspaces::new(temp.path());
    let a = ws.worktree("1", &["A.xcodeproj"]);
    let never = ws.broker("never", &["--never"], false, Some(2));
    let mut client = Client::connect_from(&never.socket(), &a, 10);
    // Ready first, so the wait below is the window's alone.
    assert_eq!(
        text(&client.call(json!(1), "DocumentationSearch", json!({"query": "q"}))),
        "found q"
    );
    let started = Instant::now();
    let busy = client.call(json!(2), "BuildProject", json!({}));
    let took = started.elapsed();
    assert!(took < Duration::from_secs(3), "{took:?}");
    assert_eq!(
        text(&busy),
        "Xcode tools busy: Xcode is opening this workspace's window; retry after 15 s"
    );
    assert_eq!(busy["result"]["_meta"]["privateer/retry_after_secs"], 15);
    assert!(received(&never, "BuildProject").is_empty());
    drop(never);

    let failing = ws.broker("failing", &["--fail"], false, None);
    let mut client = Client::connect_from(&failing.socket(), &a, 10);
    let failed = client.call(json!(1), "BuildProject", json!({}));
    assert!(is_error(&failed), "{failed}");
    let shown = text(&failed);
    assert!(
        shown.starts_with("could not open this workspace's Xcode window:")
            && shown.contains("LSOpenURLsWithRole")
            && !shown.contains("second line"),
        "{shown}"
    );
    assert!(received(&failing, "BuildProject").is_empty());
    // Recorded while it opened, dropped once the open failed.
    eventually(Duration::from_secs(10), "an empty record", || {
        let record: Value =
            serde_json::from_slice(&fs::read(failing.dir.join("xcode.windows.json")).ok()?).ok()?;
        (record["windows"] == json!([])).then_some(())
    });
}

#[test]
fn a_released_workspace_closes_only_the_windows_the_broker_opened() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspaces::new(temp.path());
    let a = ws.worktree("1", &["A.xcodeproj"]);
    let b = ws.worktree("2", &["B.xcodeproj"]);
    // b's window was open already: the broker never closes it.
    ws.seed(json!([{"tabIdentifier": "tab-b", "workspacePath": b.join("B.xcodeproj")}]));
    let mut broker = ws.broker("one", &[], false, None);
    let mut client = Client::connect_from(&broker.socket(), &a, 10);
    assert_eq!(
        text(&client.call(json!(1), "BuildProject", json!({}))),
        "built in tab tab-1"
    );
    let project = a.join("A.xcodeproj");
    assert_eq!(
        close_workspace(&broker.socket(), &a),
        json!({"closed": [project]})
    );
    let closes = ws.closes();
    assert_eq!(closes.len(), 1);
    assert_eq!(
        closes[0]["argv"].as_array().unwrap().last().unwrap(),
        &json!(project)
    );
    let open: Vec<Value> = ws
        .listed()
        .into_iter()
        .map(|w| w["tabIdentifier"].clone())
        .collect();
    assert_eq!(open, [json!("tab-b")]);
    assert_eq!(broker.state()["windows"], json!([]));
    assert_eq!(close_workspace(&broker.socket(), &b), json!({"closed": []}));
    assert_eq!(ws.listed().len(), 1, "b's own window is untouched");
    assert_eq!(ws.closes().len(), 1);
    // A malformed control line, or a root not strictly inside a pool, gets
    // an error, not a close.
    let outside = ws.root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    for root in [
        json!("relative"),
        json!("/"),
        json!(ws.pool),
        json!(outside),
        json!(format!("{}/../2/repo", a.display())),
        json!(null),
    ] {
        let bad = control(
            &broker.socket(),
            json!({"privateer_control": 1, "close_workspace": root}),
        );
        assert!(bad["error"].is_string(), "{root}: {bad}");
    }
    assert_eq!(ws.closes().len(), 1);
    // Opened again, then the broker restarts: the new one still closes it.
    assert_eq!(
        text(&client.call(json!(2), "BuildProject", json!({}))),
        "built in tab tab-1"
    );
    drop(client);
    broker.stop();
    let restarted = ws.broker("one", &[], false, None);
    assert_eq!(restarted.state()["windows"][0]["project"], json!(project));
    assert_eq!(
        close_workspace(&restarted.socket(), &a),
        json!({"closed": [project]})
    );
    assert_eq!(ws.closes().len(), 2);
    assert_eq!(ws.listed().len(), 1);
}

#[test]
fn a_changed_tool_list_refreshes_which_tools_are_window_bound() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspaces::new(temp.path());
    let a = ws.worktree("1", &["A.xcodeproj"]);
    ws.seed(json!([{"tabIdentifier": "tab-a", "workspacePath": a.join("A.xcodeproj")}]));
    let broker = ws.broker("one", &[], false, None);
    let mut client = Client::connect_from(&broker.socket(), &a, 10);
    assert_eq!(
        text(&client.call(json!(1), "XcodeGrep", json!({"pattern": "x"}))),
        "grep in tab None"
    );
    assert_eq!(
        text(&client.call(json!(2), "retool", json!({}))),
        "retooled"
    );
    assert_eq!(
        text(&client.call(json!(3), "XcodeGrep", json!({"pattern": "x"}))),
        "grep in tab tab-a"
    );
    // The broker's own tools/list went upstream under its own ids, at least
    // once before and once after; the client was never answered for them.
    let lists = broker
        .records()
        .into_iter()
        .filter(|r| r["method"] == "tools/list")
        .count();
    assert!(lists >= 2, "{lists}");
    let stray = client.lines.try_iter().collect::<Vec<_>>();
    assert!(
        stray
            .iter()
            .all(|m| m["method"] == "notifications/tools/list_changed"),
        "{stray:?}"
    );
}

#[test]
fn install_pins_the_workspace_pools_and_window_helpers() {
    let temp = tempfile::tempdir().unwrap();
    let machine = Machine {
        root: temp.path().to_path_buf(),
    };
    let here = temp.path();
    let extra = fs::canonicalize(temp.path())
        .unwrap()
        .join("more-workspaces");
    fs::create_dir_all(&extra).unwrap();
    let open = r#"["/usr/bin/true","open"]"#;
    let close = r#"["/usr/bin/true","close"]"#;
    let installed = machine.run(
        here,
        &[
            "mcp-broker",
            "install",
            "--workspace-pool",
            extra.to_str().unwrap(),
            "--window-open",
            open,
            "--window-close",
            close,
        ],
    );
    let bytes = fs::read(machine.dir().join("xcode.json")).unwrap();
    let config: Value = serde_json::from_slice(&bytes).unwrap();
    let own_pool = fs::canonicalize(machine.ledger().join("worktrees")).unwrap();
    assert_eq!(config["workspace_pools"], json!([own_pool, extra]));
    assert_eq!(config["window_open"], json!(["/usr/bin/true", "open"]));
    assert_eq!(config["window_close"], json!(["/usr/bin/true", "close"]));
    assert_eq!(installed["workspace_pools"], config["workspace_pools"]);
    // The pin covers them: it is the hash of exactly these bytes.
    let pin = fs::read_to_string(
        machine
            .root
            .join("data/privateer/mcp-broker/xcode.json.sha256"),
    )
    .unwrap();
    assert_eq!(pin.trim(), {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(&bytes))
    });
    // Refused: relative, non-normalized or shell helpers, and relative,
    // non-normalized or missing pools. Nothing is rewritten.
    let not_dir = extra.join("missing");
    let dotted = format!("{}/./x", extra.display());
    for args in [
        vec!["--window-open", r#"["bin/open"]"#],
        vec!["--window-open", r#"["/usr/bin/../bin/open"]"#],
        vec!["--window-close", r#"["/bin/sh","-c","true"]"#],
        vec!["--window-close", r#"["/usr/bin/env","osascript"]"#],
        vec!["--window-open", "/usr/bin/open"],
        vec!["--workspace-pool", "workspaces"],
        vec!["--workspace-pool", dotted.as_str()],
        vec!["--workspace-pool", not_dir.to_str().unwrap()],
    ] {
        let mut all = vec!["mcp-broker", "install"];
        all.extend(&args);
        let out = machine.command(here, &all).output().unwrap();
        assert!(!out.status.success(), "{args:?} was accepted");
    }
    assert_eq!(fs::read(machine.dir().join("xcode.json")).unwrap(), bytes);
}
