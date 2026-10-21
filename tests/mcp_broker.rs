//! The MCP broker against a fake upstream: real broker, proxy and dispatcher
//! processes, an isolated HOME, XDG data, ledger and fleet registry. The
//! routing tests use a generic profile (tests/fixtures/mcp-broker/
//! fake_profile.json); the Xcode example profile is installed as one more.
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

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/mcp-broker")
        .join(name)
}

fn fake() -> PathBuf {
    fixture("fake_mcpbridge.py")
}

/// The generic routing profile every routing test uses.
fn fake_profile() -> PathBuf {
    fixture("fake_profile.json")
}

/// The Xcode example, as the docs ship it.
fn xcode_profile() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/examples/mcp-broker/xcode.json")
}

/// A short temporary directory: macOS caps a socket path at 104 bytes, and
/// its per-user temp dir alone takes about 60.
fn short_temp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("pd")
        .tempdir_in("/tmp")
        .unwrap()
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
        command.args(["--name", "fake"]);
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
        let socket = dir.join("fake.sock");
        eventually(Duration::from_secs(10), "the broker's socket", || {
            assert!(child.try_wait().unwrap().is_none(), "the broker exited");
            socket.exists().then_some(())
        });
        Broker { child, dir, record }
    }

    fn socket(&self) -> PathBuf {
        self.dir.join("fake.sock")
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
        serde_json::from_slice(&fs::read(self.dir.join("fake.state.json")).unwrap_or_default())
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
    let temp = short_temp();
    let broker = Broker::start(temp.path(), &[], None);
    let mut a = Client::connect(&broker.socket());
    let mut b = Client::connect(&broker.socket());
    let first = a.initialize(json!(1));
    let second = b.initialize(json!(1));
    assert_eq!(first["result"]["serverInfo"]["name"], "fake-upstream");
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
    let temp = short_temp();
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
    let temp = short_temp();
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
    let temp = short_temp();
    let broker = Broker::start(temp.path(), &["--init-delay", "30"], Some(2));
    let mut a = Client::connect(&broker.socket());
    let started = Instant::now();
    let busy = a.call(json!(1), "echo", json!({"text": "x"}));
    let took = started.elapsed();
    assert!(took < Duration::from_secs(3), "{took:?}");
    assert!(took >= Duration::from_millis(1_900), "{took:?}");
    assert_eq!(busy["result"]["isError"], true);
    // With no --label, the instance's name names it.
    assert!(text(&busy).starts_with("fake tools busy: "), "{busy}");
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
            .starts_with("fake tools busy: ")
    );
}

#[test]
fn symlinked_files_in_the_state_dir_are_refused_and_their_targets_untouched() {
    use std::os::unix::fs::symlink;
    let temp = short_temp();
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
            .args(["--name", "fake", "--"])
            .args(upstream(&record, &[]))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
    };
    // A symlinked lock: the broker refuses to start.
    symlink(victim("lock"), dir.join("fake.lock")).unwrap();
    let mut refused = support::command(BROKER);
    serve(&mut refused);
    let refused = refused.output().unwrap();
    assert!(!refused.status.success());
    assert!(!dir.join("fake.sock").exists());
    untouched("lock");
    fs::remove_file(dir.join("fake.lock")).unwrap();
    // A symlinked log, state, pid, and state temp file (named for the
    // broker's own pid: the shell execs the broker in its place).
    symlink(victim("log"), dir.join("fake.log")).unwrap();
    symlink(victim("state"), dir.join("fake.state.json")).unwrap();
    symlink(victim("pid"), dir.join("fake.pid")).unwrap();
    let mut command = support::command("/bin/sh");
    command
        .arg("-c")
        .arg(r#"ln -s "$1" "$2/.fake.state.json.$$.tmp" && shift 2 && exec "$@""#)
        .arg("sh")
        .arg(victim("temp"))
        .arg(&dir)
        .arg(BROKER);
    serve(&mut command);
    let mut child = command.spawn().unwrap();
    eventually(Duration::from_secs(10), "the broker's socket", || {
        assert!(child.try_wait().unwrap().is_none(), "the broker exited");
        dir.join("fake.sock").exists().then_some(())
    });
    let mut client = Client::connect(&dir.join("fake.sock"));
    assert_eq!(
        text(&client.call(json!(1), "echo", json!({"text": "fine"}))),
        "fine"
    );
    drop(client);
    // The state and pid were replaced by rename, not written through.
    eventually(Duration::from_secs(5), "a regular state file", || {
        fs::symlink_metadata(dir.join("fake.state.json"))
            .is_ok_and(|m| m.is_file())
            .then_some(())
    });
    assert!(
        fs::symlink_metadata(dir.join("fake.pid"))
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
        fs::symlink_metadata(dir.join("fake.log"))
            .unwrap()
            .is_symlink()
    );
}

#[test]
fn one_broker_per_directory_private_modes_and_a_stale_socket_replaced() {
    let temp = short_temp();
    let dir = temp.path().join("state");
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    // A socket file left by a broker that died.
    drop(UnixListener::bind(dir.join("fake.sock")).unwrap());
    assert!(dir.join("fake.sock").exists());
    let broker = Broker::start(temp.path(), &[], None);
    let mut a = Client::connect(&broker.socket());
    assert_eq!(text(&a.call(json!(1), "echo", json!({"text": "up"}))), "up");
    assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
    assert_eq!(fs::metadata(broker.socket()).unwrap().mode() & 0o777, 0o600);
    let pid: u32 = fs::read_to_string(dir.join("fake.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(pid, broker.child.id());
    let started = Instant::now();
    let second = support::command(BROKER)
        .args(["serve", "--state-dir"])
        .arg(&dir)
        .args(["--name", "fake", "--"])
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
    let temp = short_temp();
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
    let temp = short_temp();
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
        .arg(temp.path().join("nobody/fake.sock"))
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
    fn new(temp: &Path) -> Machine {
        Machine {
            root: fs::canonicalize(temp).unwrap(),
        }
    }

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

    /// A command that must fail: its stderr.
    fn refused(&self, folder: &Path, args: &[&str]) -> String {
        let output = self.command(folder, args).output().unwrap();
        assert!(!output.status.success(), "{args:?} was accepted");
        String::from_utf8_lossy(&output.stderr).into_owned()
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

    fn pin(&self, name: &str) -> PathBuf {
        self.root
            .join(format!("data/privateer/mcp-broker/{name}.json.sha256"))
    }

    fn ledger(&self) -> PathBuf {
        self.root.join("ledger")
    }

    fn pass(&self, folder: &Path) -> Value {
        self.run(folder, &["serve", "--json", r#"{"once":true}"#])["mcp_broker"].clone()
    }
}

/// `install --name NAME [EXTRA...] --upstream ARGV...`, as argv.
fn install_args<'a>(name: &'a str, extra: &[&'a str], upstream: &'a [String]) -> Vec<&'a str> {
    let mut args = vec!["mcp-broker", "install", "--name", name];
    args.extend(extra);
    args.push("--upstream");
    args.extend(upstream.iter().map(String::as_str));
    args
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
    let temp = short_temp();
    let machine = Machine::new(temp.path());
    let here = machine.root.clone();
    let here = here.as_path();
    let first = machine.run(
        here,
        &[
            "mcp-broker",
            "install",
            "--name",
            "fake",
            "--upstream",
            "/opt/x/mcp",
            "serve",
        ],
    );
    assert_eq!(first["action"], "installed");
    assert_eq!(first["name"], "fake");
    let stable = machine.stable();
    assert_eq!(first["binary"], stable.to_string_lossy().as_ref());
    assert_eq!(first["sha256"], sha256(&stable));
    assert_eq!(fs::read(&stable).unwrap(), fs::read(BROKER).unwrap());
    assert_eq!(fs::metadata(&stable).unwrap().mode() & 0o777, 0o755);
    let config: Value =
        serde_json::from_slice(&fs::read(machine.dir().join("fake.json")).unwrap()).unwrap();
    assert_eq!(config["upstream"], json!(["/opt/x/mcp", "serve"]));
    // Privateer's own workspace pool, <ledger>/worktrees, is always there.
    let own_pool = fs::canonicalize(machine.ledger().join("worktrees")).unwrap();
    assert_eq!(config["workspace_pools"], json!([own_pool]));
    // No profile: a plain shared broker, labelled by its name, with no
    // window helper anywhere.
    assert_eq!(config["label"], "fake");
    assert!(config.get("routing").is_none(), "{config}");
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
        json!([stable, "proxy", "--socket", machine.dir().join("fake.sock")])
    );
    let inode = fs::metadata(&stable).unwrap().ino();
    let again = machine.run(
        here,
        &[
            "mcp-broker",
            "install",
            "--name",
            "fake",
            "--upstream",
            "/opt/x/mcp",
            "a",
        ],
    );
    assert_eq!(again["action"], "unchanged");
    assert_eq!(fs::metadata(&stable).unwrap().ino(), inode);
    assert_eq!(again["sha256"], first["sha256"]);
    assert_eq!(again["upstream"], json!(["/opt/x/mcp", "a"]));
    // Another version at the stable path is replaced, by rename.
    fs::write(&stable, "#!/bin/sh\necho privateer-mcp-broker 0.0.0-old\n").unwrap();
    let old_inode = fs::metadata(&stable).unwrap().ino();
    let replaced = machine.run(
        here,
        &[
            "mcp-broker",
            "install",
            "--name",
            "fake",
            "--upstream",
            "/opt/x/mcp",
        ],
    );
    assert_eq!(replaced["action"], "replaced");
    assert_ne!(fs::metadata(&stable).unwrap().ino(), old_inode);
    assert_eq!(replaced["sha256"], sha256(&stable));
    assert_eq!(fs::read(&stable).unwrap(), fs::read(BROKER).unwrap());
    // Relative upstreams refuse; agents may read status but not install.
    let relative = machine.refused(
        here,
        &[
            "mcp-broker",
            "install",
            "--name",
            "fake",
            "--upstream",
            "mcp",
            "serve",
        ],
    );
    assert!(relative.contains("absolute, normalized path"), "{relative}");
    let agent = machine
        .command(
            here,
            &[
                "mcp-broker",
                "install",
                "--name",
                "fake",
                "--upstream",
                "/x",
            ],
        )
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
    assert_eq!(status["run_dir"], machine.dir().to_string_lossy().as_ref());
    assert_eq!(status["instances"]["fake"]["running"], false);
    assert_eq!(status["instances"]["fake"]["routing"], false);
    // A run directory inside the ledger is refused.
    let inside = machine
        .command(
            here,
            &[
                "mcp-broker",
                "install",
                "--name",
                "fake",
                "--upstream",
                "/x",
            ],
        )
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
    let temp = short_temp();
    let machine = Machine::new(temp.path());
    let upkeep = machine.ship("upkeep");
    let other = machine.ship("other");
    // The owner's home keeps its ledger elsewhere: the broker's files do not
    // follow the ledger, so an install from outside any ship folder (no
    // home) is the one its dispatcher finds.
    let home_config = upkeep.join(".privateer/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&home_config).unwrap()).unwrap();
    config["resource_home"] = json!(machine.root.join("owner-ledger"));
    fs::write(&home_config, config.to_string()).unwrap();
    let record = machine.root.join("record.jsonl");
    let argv = upstream(&record, &[]);
    let install = install_args("fake", &[], &argv);
    machine.run(&machine.root, &install);
    let pid_file = machine.dir().join("fake.pid");
    let _detached = Detached(pid_file.clone());
    // No machine owner: no dispatcher starts it.
    assert_eq!(machine.pass(&upkeep), Value::Null);
    machine.run(&upkeep, &["machine", "owner", "upkeep"]);
    // Another ship's dispatcher never does.
    assert_eq!(machine.pass(&other), Value::Null);
    // Nor the owner's while the lock is held.
    let held = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(machine.dir().join("fake.lock"))
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
    let pid = started["fake"]["started"]
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
    let mut client = Client::connect(&machine.dir().join("fake.sock"));
    assert_eq!(
        text(&client.call(json!(1), "echo", json!({"text": "kept"}))),
        "kept"
    );
    drop(client);
    // Running: the next pass leaves it alone.
    assert_eq!(machine.pass(&upkeep), Value::Null);
    let status = machine.run(&upkeep, &["mcp-broker", "status", "--name", "fake"]);
    let fake = &status["instances"]["fake"];
    assert_eq!(fake["running"], true);
    assert_eq!(fake["pid"], pid);
    assert_eq!(fake["broker"]["upstream"]["state"], "ready");
    // The dispatcher passed the pinned config's pools and label to the broker.
    assert!(!fake["workspace_pools"].as_array().unwrap().is_empty());
    assert_eq!(fake["broker"]["workspace_pools"], fake["workspace_pools"]);
    assert_eq!(fake["broker"]["label"], "fake");
    let upstream_pid = fake["broker"]["upstream"]["pid"].as_i64().unwrap() as i32;
    assert!(alive(upstream_pid));
    // A state file naming another process group as the upstream: stop never
    // signals it, and the broker still ends its own upstream.
    let mut bystander = Command::new("sleep");
    std::os::unix::process::CommandExt::process_group(&mut bystander, 0);
    let mut bystander = bystander.arg("60").spawn().unwrap();
    let bystander_pid = bystander.id() as i32;
    let state_file = machine.dir().join("fake.state.json");
    let mut forged: Value = serde_json::from_slice(&fs::read(&state_file).unwrap()).unwrap();
    forged["upstream"]["pid"] = json!(bystander_pid);
    let forged_temp = machine.dir().join("forged.tmp");
    fs::write(&forged_temp, forged.to_string()).unwrap();
    fs::rename(&forged_temp, &state_file).unwrap();
    // Stop needs a name.
    let unnamed = machine.refused(&upkeep, &["mcp-broker", "stop"]);
    assert!(unnamed.contains("--name"), "{unnamed}");
    // Stop: the broker and its upstream are gone, and it stays stopped.
    let stopped = machine.run(&upkeep, &["mcp-broker", "stop", "--name", "fake"]);
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
    // write the run directory) is never started.
    machine.run(&upkeep, &install);
    let config = machine.dir().join("fake.json");
    let rewritten = json!({"upstream": ["/usr/bin/touch", machine.root.join("pwned")]});
    fs::write(&config, rewritten.to_string()).unwrap();
    let refused = machine.pass(&upkeep);
    assert!(
        refused["fake"]["error"]
            .as_str()
            .unwrap()
            .contains("differs"),
        "{refused}"
    );
    assert!(!machine.root.join("pwned").exists());
    // A symlinked log in the run directory: the dispatcher refuses to open
    // it, starts nothing, and the link's target is untouched.
    machine.run(&upkeep, &install);
    let victim = machine.root.join("victim");
    fs::write(&victim, "untouched").unwrap();
    let _ = fs::remove_file(machine.dir().join("fake.log"));
    std::os::unix::fs::symlink(&victim, machine.dir().join("fake.log")).unwrap();
    let refused = machine.pass(&upkeep);
    assert!(
        refused["fake"]["error"]
            .as_str()
            .unwrap()
            .contains("fake.log"),
        "{refused}"
    );
    assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");
    assert!(!pid_file.exists());
    fs::remove_file(machine.dir().join("fake.log")).unwrap();
    // Install lifts the stop; removing the config stops it again.
    machine.run(&upkeep, &install);
    let restarted = machine.pass(&upkeep)["fake"]["started"].as_i64().unwrap() as i32;
    eventually(Duration::from_secs(10), "the restarted broker", || {
        (fs::read_to_string(&pid_file).ok()?.trim() == restarted.to_string()).then_some(())
    });
    let upstream_pid = eventually(Duration::from_secs(10), "its upstream", || {
        let state: Value =
            serde_json::from_slice(&fs::read(machine.dir().join("fake.state.json")).ok()?).ok()?;
        state["upstream"]["pid"].as_i64()
    }) as i32;
    fs::remove_file(machine.dir().join("fake.json")).unwrap();
    let gone = machine.pass(&upkeep);
    assert_eq!(gone["fake"]["stopped"]["upstream_gone"], true, "{gone}");
    assert!(!alive(upstream_pid));
}

#[test]
fn two_instances_run_side_by_side_with_files_and_errors_of_their_own() {
    let temp = short_temp();
    let machine = Machine::new(temp.path());
    let upkeep = machine.ship("upkeep");
    machine.run(&upkeep, &["machine", "owner", "upkeep"]);
    let profile = fake_profile();
    let profile = profile.to_str().unwrap();
    let fake_argv = upstream(&machine.root.join("fake.jsonl"), &[]);
    let two_argv = upstream(&machine.root.join("two.jsonl"), &[]);
    let fake_install = install_args("fake", &["--profile", profile], &fake_argv);
    machine.run(&upkeep, &fake_install);
    machine.run(&upkeep, &install_args("two", &[], &two_argv));
    let _fake = Detached(machine.dir().join("fake.pid"));
    let _two = Detached(machine.dir().join("two.pid"));
    // One instance's error never skips the other.
    fs::write(machine.dir().join("fake.json"), r#"{"upstream":["/x"]}"#).unwrap();
    let first = machine.pass(&upkeep);
    assert!(
        first["fake"]["error"].as_str().unwrap().contains("differs"),
        "{first}"
    );
    assert!(first["two"]["started"].is_i64(), "{first}");
    machine.run(&upkeep, &fake_install);
    let second = machine.pass(&upkeep);
    assert!(second["fake"]["started"].is_i64(), "{second}");
    assert!(
        second.get("two").is_none(),
        "two runs: left alone: {second}"
    );
    for name in ["fake", "two"] {
        let socket = machine.dir().join(format!("{name}.sock"));
        eventually(Duration::from_secs(10), "both sockets", || {
            socket.exists().then_some(())
        });
    }
    let status = machine.run(&upkeep, &["mcp-broker", "status"]);
    let instances = status["instances"].as_object().unwrap();
    assert_eq!(
        instances.keys().collect::<Vec<_>>(),
        ["fake", "two"],
        "{status}"
    );
    assert_eq!(instances["fake"]["routing"], true);
    assert_eq!(instances["fake"]["label"], "Fake");
    assert_eq!(instances["two"]["routing"], false);
    assert_eq!(instances["two"]["label"], "two");
    // Each has its own files and upstream.
    let mut fake_client = Client::connect(&machine.dir().join("fake.sock"));
    let mut two_client = Client::connect(&machine.dir().join("two.sock"));
    assert_eq!(
        text(&fake_client.call(json!(1), "echo", json!({"text": "f"}))),
        "f"
    );
    assert_eq!(
        text(&two_client.call(json!(1), "echo", json!({"text": "t"}))),
        "t"
    );
    for (name, other) in [("fake", "two"), ("two", "fake")] {
        let seen = records(&machine.root.join(format!("{name}.jsonl")));
        let echoes: Vec<&Value> = seen
            .iter()
            .filter(|r| r["params"]["name"] == "echo")
            .collect();
        assert_eq!(echoes.len(), 1, "{name}");
        assert_ne!(echoes[0]["params"]["arguments"]["text"], other[..1]);
        let state: Value = serde_json::from_slice(
            &fs::read(machine.dir().join(format!("{name}.state.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(state["name"], name);
        assert!(machine.pin(name).exists());
    }
    // A plain instance binds nothing: its window-shaped calls pass as they are.
    assert_eq!(
        text(&two_client.call(json!(2), "Build", json!({}))),
        "built in window None"
    );
    drop(fake_client);
    // Stopping one leaves the other running.
    let stopped = machine.run(&upkeep, &["mcp-broker", "stop", "--name", "fake"]);
    assert_eq!(stopped["stopped"]["running"], false, "{stopped}");
    let status = machine.run(&upkeep, &["mcp-broker", "status"]);
    assert_eq!(status["instances"]["fake"]["running"], false);
    assert_eq!(status["instances"]["fake"]["stopped"], true);
    assert_eq!(status["instances"]["two"]["running"], true);
    assert_eq!(
        text(&two_client.call(json!(3), "echo", json!({"text": "still"}))),
        "still"
    );
    assert_eq!(machine.pass(&upkeep), Value::Null, "fake stays stopped");
    let unknown = machine.refused(&upkeep, &["mcp-broker", "status", "--name", "three"]);
    assert!(
        unknown.contains("no MCP broker instance three"),
        "{unknown}"
    );
    machine.run(&upkeep, &["mcp-broker", "stop", "--name", "two"]);
}

#[test]
fn a_pinned_config_of_the_first_shape_still_starts_as_a_plain_instance() {
    let temp = short_temp();
    let machine = Machine::new(temp.path());
    let upkeep = machine.ship("upkeep");
    machine.run(&upkeep, &["machine", "owner", "upkeep"]);
    // Install once so the stable binary is there, then leave only what #346
    // wrote: `xcode.json` with the upstream alone, and its pin.
    machine.run(
        &upkeep,
        &[
            "mcp-broker",
            "install",
            "--name",
            "spare",
            "--upstream",
            "/x",
        ],
    );
    fs::remove_file(machine.dir().join("spare.json")).unwrap();
    fs::remove_file(machine.pin("spare")).unwrap();
    let argv = upstream(&machine.root.join("record.jsonl"), &[]);
    let config = serde_json::to_vec(&json!({"upstream": argv})).unwrap();
    fs::write(machine.dir().join("xcode.json"), &config).unwrap();
    fs::set_permissions(
        machine.dir().join("xcode.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    {
        use sha2::{Digest, Sha256};
        fs::write(
            machine.pin("xcode"),
            format!("{:x}\n", Sha256::digest(&config)),
        )
        .unwrap();
    }
    let _detached = Detached(machine.dir().join("xcode.pid"));
    let started = machine.pass(&upkeep);
    assert!(started["xcode"]["started"].is_i64(), "{started}");
    let socket = machine.dir().join("xcode.sock");
    eventually(Duration::from_secs(10), "the socket", || {
        socket.exists().then_some(())
    });
    let mut client = Client::connect(&socket);
    assert_eq!(
        text(&client.call(json!(1), "echo", json!({"text": "old"}))),
        "old"
    );
    let status = machine.run(&upkeep, &["mcp-broker", "status", "--name", "xcode"]);
    assert_eq!(status["instances"]["xcode"]["routing"], false);
    assert_eq!(status["instances"]["xcode"]["broker"]["routing"], false);
    drop(client);
    machine.run(&upkeep, &["mcp-broker", "stop", "--name", "xcode"]);
}

#[test]
fn install_refuses_each_bad_input_and_names_its_cause() {
    let temp = short_temp();
    let machine = Machine::new(temp.path());
    let here = machine.root.clone();
    let profile = fake_profile();
    let profile = profile.to_str().unwrap();
    let profile_json: Value = serde_json::from_slice(&fs::read(&profile).unwrap()).unwrap();
    // A copy of the fake profile with one change.
    let changed = |name: &str, change: &dyn Fn(&mut Value)| -> String {
        let mut copy = profile_json.clone();
        change(&mut copy);
        let path = machine.root.join(format!("{name}.json"));
        fs::write(&path, copy.to_string()).unwrap();
        path.to_string_lossy().into_owned()
    };
    let typo_top = changed("typo-top", &|p| p["labl"] = json!("x"));
    let typo_routing = changed("typo-routing", &|p| p["routing"]["targt"] = json!("x"));
    let typo_list = changed("typo-list", &|p| p["routing"]["list"]["tol"] = json!("x"));
    let typo_project = changed("typo-project", &|p| {
        p["routing"]["project"]["skip_dir"] = json!([])
    });
    let typo_kind = changed("typo-kind", &|p| {
        p["routing"]["project"]["kinds"][0] = json!({"dir_sufix": ".proj"})
    });
    let bad_kind = changed("bad-kind", &|p| {
        p["routing"]["project"]["kinds"][0] = json!({"dir_suffix": "proj"})
    });
    let shell = r#"["/bin/sh","-c","true"]"#;
    let true_close = r#"["/usr/bin/true","close"]"#;
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec!["--name", "fake", "--profile", profile], "no upstream"),
        (vec!["--name", "Xcode", "--upstream", "/x"], "--name"),
        (
            vec!["--name", "abcdefghijklmnopq", "--upstream", "/x"],
            "--name",
        ),
        (vec!["--name", "../x", "--upstream", "/x"], "--name"),
        (vec!["--upstream", "/x"], "--name NAME"),
        (
            vec!["--name", "fake", "--profile", &typo_top, "--upstream", "/x"],
            "unknown profile key labl",
        ),
        (
            vec![
                "--name",
                "fake",
                "--profile",
                &typo_routing,
                "--upstream",
                "/x",
            ],
            "routing.targt",
        ),
        (
            vec![
                "--name",
                "fake",
                "--profile",
                &typo_list,
                "--upstream",
                "/x",
            ],
            "routing.list.tol",
        ),
        (
            vec![
                "--name",
                "fake",
                "--profile",
                &typo_project,
                "--upstream",
                "/x",
            ],
            "routing.project.skip_dir",
        ),
        (
            vec![
                "--name",
                "fake",
                "--profile",
                &typo_kind,
                "--upstream",
                "/x",
            ],
            "dir_sufix",
        ),
        (
            vec!["--name", "fake", "--profile", &bad_kind, "--upstream", "/x"],
            "must start with",
        ),
        (
            vec![
                "--name",
                "fake",
                "--profile",
                profile,
                "--window-open",
                r#"["/usr/bin/true"]"#,
                "--window-close",
                shell,
                "--upstream",
                "/x",
            ],
            "may not be sh",
        ),
        (
            vec![
                "--name",
                "fake",
                "--profile",
                profile,
                "--window-open",
                r#"["/usr/bin/env","open"]"#,
                "--window-close",
                true_close,
                "--upstream",
                "/x",
            ],
            "may not be env",
        ),
        (
            vec![
                "--name",
                "fake",
                "--profile",
                profile,
                "--window-open",
                r#"["/usr/bin/true"]"#,
                "--upstream",
                "/x",
            ],
            "requires routing.close",
        ),
        (
            vec![
                "--name",
                "fake",
                "--profile",
                profile,
                "--window-open",
                r#"["bin/open"]"#,
                "--window-close",
                true_close,
                "--upstream",
                "/x",
            ],
            "absolute, normalized",
        ),
        (
            vec![
                "--name",
                "fake",
                "--profile",
                profile,
                "--window-open",
                r#"["/usr/bin/../bin/open"]"#,
                "--window-close",
                true_close,
                "--upstream",
                "/x",
            ],
            "absolute, normalized",
        ),
        (
            vec![
                "--name",
                "fake",
                "--profile",
                profile,
                "--window-open",
                "/usr/bin/open",
                "--upstream",
                "/x",
            ],
            "JSON array",
        ),
        (
            vec![
                "--name",
                "fake",
                "--window-open",
                r#"["/usr/bin/true"]"#,
                "--upstream",
                "/x",
            ],
            "needs a profile with routing",
        ),
        (
            vec![
                "--name",
                "fake",
                "--workspace-pool",
                "workspaces",
                "--upstream",
                "/x",
            ],
            "absolute, normalized",
        ),
    ];
    for (args, cause) in &cases {
        let mut all = vec!["mcp-broker", "install"];
        all.extend(args);
        let stderr = machine.refused(&here, &all);
        assert!(stderr.contains(cause), "{args:?}: {stderr}");
    }
    assert!(
        !machine.dir().join("fake.json").exists(),
        "nothing was written"
    );
    assert!(!machine.pin("fake").exists());
    // A socket path of 104 bytes or more, from a long XDG_DATA_HOME.
    let long = machine.root.join("d".repeat(80));
    fs::create_dir_all(&long).unwrap();
    let output = machine
        .command(
            &here,
            &[
                "mcp-broker",
                "install",
                "--name",
                "fake",
                "--upstream",
                "/x",
            ],
        )
        .env("XDG_DATA_HOME", &long)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("XDG_DATA_HOME") && stderr.contains("at most 103"),
        "{stderr}"
    );
    assert!(!long.join("privateer/mcp-broker/run/fake.json").exists());
}

// ---- Windows and routing, by the generic fake profile ----------------------

fn window_helper() -> PathBuf {
    fixture("fake_window.py")
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
/// Treehouse leaves them, and the fake windows the upstream lists.
struct Workspaces {
    root: PathBuf,
    pool: PathBuf,
    repo: PathBuf,
    /// The open windows, as the fake upstream lists them.
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

    /// Windows already open, which the broker did not open: (target, path).
    fn seed(&self, windows: &[(&str, &Path)]) {
        let list: Vec<Value> = windows
            .iter()
            .map(|(target, path)| json!({"target": target, "path": path}))
            .collect();
        fs::write(&self.windows, json!(list).to_string()).unwrap();
    }

    fn listed(&self) -> Vec<Value> {
        fs::read(&self.windows)
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<Value>>(&b).ok())
            .unwrap_or_default()
    }

    fn helper_argv(&self, action: &str, extra: &[&str]) -> Vec<String> {
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
        argv
    }

    fn helper(&self, action: &str, extra: &[&str]) -> String {
        serde_json::to_string(&self.helper_argv(action, extra)).unwrap()
    }

    /// The broker's options: this pool, the fake profile's label and
    /// routing, and the fake helpers as its open and close.
    fn options(&self, open: &[&str], close: &[&str]) -> Vec<String> {
        let profile: Value = serde_json::from_slice(&fs::read(fake_profile()).unwrap()).unwrap();
        let mut routing = profile["routing"].clone();
        routing["open"] = json!(self.helper_argv("open", open));
        routing["close"] = json!(self.helper_argv("close", close));
        vec![
            "--label".into(),
            profile["label"].as_str().unwrap().into(),
            "--workspace-pool".into(),
            self.pool.to_string_lossy().into_owned(),
            "--routing".into(),
            routing.to_string(),
        ]
    }

    /// The fake upstream's argv tail: this profile and these windows.
    fn upstream_args(&self, profile: &Path, text: bool) -> Vec<String> {
        let mut args = vec![
            "--profile".to_owned(),
            profile.to_string_lossy().into_owned(),
            "--windows".into(),
            self.windows.to_string_lossy().into_owned(),
        ];
        if text {
            args.push("--windows-text".into());
        }
        args
    }

    /// A broker in `name` under the root, its upstream listing these windows.
    fn broker(&self, name: &str, open: &[&str], text: bool, ready_wait: Option<u64>) -> Broker {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let upstream = self.upstream_args(&fake_profile(), text);
        let upstream: Vec<&str> = upstream.iter().map(String::as_str).collect();
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
    received_in(&broker.records(), tool)
}

fn received_in(records: &[Value], tool: &str) -> Vec<Value> {
    records
        .iter()
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
fn each_workspace_works_in_its_own_window_and_a_foreign_target_is_refused() {
    let temp = short_temp();
    let ws = Workspaces::new(temp.path());
    let a = ws.worktree("1", &["A.proj"]);
    let b = ws.worktree("2", &["B.proj", "src"]);
    ws.seed(&[("win-a", &a.join("A.proj")), ("win-b", &b.join("B.proj"))]);
    let broker = ws.broker("one", &[], false, None);
    let mut first = Client::connect_from(&broker.socket(), &a, 10);
    // Below the checkout: the nearest .git ancestor inside the pool.
    let mut second = Client::connect_from(&broker.socket(), &b.join("src"), 10);
    first.send(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "Build", "arguments": {}}}));
    second.send(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "Build"}}));
    assert_eq!(text(&first.answer(json!(1))), "built in window win-a");
    assert_eq!(text(&second.answer(json!(1))), "built in window win-b");
    let mut targets: Vec<Value> = received(&broker, "Build")
        .into_iter()
        .map(|a| a["windowId"].clone())
        .collect();
    targets.sort_by_key(|t| t.to_string());
    assert_eq!(targets, [json!("win-a"), json!("win-b")]);
    // Another workspace's window is refused and never forwarded; its own passes.
    let refused = first.call(json!(2), "Build", json!({"windowId": "win-b"}));
    assert!(is_error(&refused), "{refused}");
    assert_eq!(
        text(&refused),
        "windowId win-b is not this workspace's Fake window; call ListWindows"
    );
    let own = first.call(json!(3), "Build", json!({"windowId": "win-a"}));
    assert_eq!(text(&own), "built in window win-a");
    let to_b = received(&broker, "Build")
        .into_iter()
        .filter(|a| a["windowId"] == "win-b")
        .count();
    assert_eq!(to_b, 1, "only the second client's own call reached win-b");
    // Arguments that are not an object are replaced, never forwarded as given.
    first.send(json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {"name": "Build", "arguments": "junk"}}));
    assert_eq!(text(&first.answer(json!(4))), "built in window win-a");
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
    assert_eq!(state["label"], "Fake");
}

#[test]
fn window_lists_are_filtered_to_the_clients_workspace_in_both_forms() {
    for text_form in [false, true] {
        let temp = short_temp();
        let ws = Workspaces::new(temp.path());
        let a = ws.worktree("1", &["A.proj"]);
        let b = ws.worktree("2", &["B.proj"]);
        ws.seed(&[
            ("win-a", &a.join("A.proj")),
            ("win-b", &b.join("B.proj")),
            ("win-x", &ws.root.join("elsewhere/X.proj")),
        ]);
        let broker = ws.broker("one", &[], text_form, None);
        let outside = ws.root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let mut mine = Client::connect_from(&broker.socket(), &a, 10);
        let mut stranger = Client::connect_from(&broker.socket(), &outside, 10);
        let listed = mine.call(json!(1), "ListWindows", json!({}));
        let shown = listed.to_string();
        assert!(shown.contains("win-a"), "{shown}");
        assert!(
            !shown.contains("win-b") && !shown.contains("win-x"),
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
        let none = stranger.call(json!(1), "ListWindows", json!({}));
        let shown = none.to_string();
        assert!(!is_error(&none), "{shown}");
        assert!(!shown.contains("win-"), "{shown}");
    }
}

#[test]
fn the_socket_peer_decides_the_workspace_never_the_hello() {
    let temp = short_temp();
    let ws = Workspaces::new(temp.path());
    let a = ws.worktree("1", &["A.proj"]);
    let b = ws.worktree("2", &["B.proj"]);
    ws.seed(&[("win-a", &a.join("A.proj")), ("win-b", &b.join("B.proj"))]);
    let broker = ws.broker("one", &[], false, None);
    // A raw client in workspace a whose hello claims workspace b.
    let script = r#"import json, socket, sys
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
f = s.makefile('rw')
f.write(json.dumps({"privateer_proxy": 1, "cwd": sys.argv[2], "pid": 1}) + "\n")
f.write(json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": "Build", "arguments": {}}}) + "\n")
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
    assert_eq!(text(&answer), "built in window win-a");
    let log = fs::read_to_string(broker.dir.join("fake.log")).unwrap();
    assert!(
        log.lines()
            .any(|l| l.contains("claims cwd") && l.contains("but its working directory is")),
        "{log}"
    );
    // Outside every pool: window tools refused, window-free tools pass.
    let outside = ws.root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let mut stranger = Client::connect_from(&broker.socket(), &outside, 10);
    let refused = stranger.call(json!(1), "Build", json!({}));
    assert!(is_error(&refused));
    assert_eq!(
        text(&refused),
        "Fake window tools work only from a Privateer task workspace"
    );
    let named = stranger.call(json!(2), "Build", json!({"windowId": "win-a"}));
    assert!(is_error(&named), "{named}");
    let docs = stranger.call(json!(3), "Search", json!({"query": "q"}));
    assert_eq!(text(&docs), "found q");
    assert_eq!(received(&broker, "Build").len(), 1);
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
    let temp = short_temp();
    let ws = Workspaces::new(temp.path());
    let a = ws.worktree("1", &["A.proj", "src"]);
    let b = ws.worktree("2", &[]);
    ws.seed(&[("win-a", &a.join("A.proj"))]);
    let broker = ws.broker("one", &[], false, None);
    let mut client = Client::connect_from(&broker.socket(), &a, 10);
    let outside = client.call(json!(1), "Read", json!({"filePath": b.join("README")}));
    assert!(is_error(&outside), "{outside}");
    assert!(
        text(&outside).contains("lies outside this workspace"),
        "{outside}"
    );
    let climbing = client.call(json!(2), "Read", json!({"filePath": "src/../../x"}));
    assert!(is_error(&climbing), "{climbing}");
    let inside = a.join("src/main.txt");
    let ok = client.call(json!(3), "Read", json!({"filePath": inside}));
    assert_eq!(
        text(&ok),
        format!("read {} in window win-a", inside.display())
    );
    // Plural and URL names, and arrays of paths, are checked too.
    for (n, arguments) in [
        json!({"filePaths": [inside, b.join("README")]}),
        json!({"uri": format!("file://{}", b.join("README").display())}),
        json!({"file": "src/../../2/repo/README"}),
    ]
    .into_iter()
    .enumerate()
    {
        let refused = client.call(json!(10 + n), "Read", arguments);
        assert!(is_error(&refused), "{refused}");
    }
    let forwarded = received(&broker, "Read");
    assert_eq!(forwarded.len(), 1, "{forwarded:?}");
    assert_eq!(forwarded[0]["filePath"], json!(inside));
}

#[test]
fn the_first_window_call_opens_the_workspace_window_once() {
    let temp = short_temp();
    let ws = Workspaces::new(temp.path());
    let a = ws.worktree(
        "1",
        &[
            "App.proj/Inner.proj",
            "lib",
            "vendor/Vendored.proj",
            ".hidden/Hidden.proj",
        ],
    );
    // A Fake.toml directory too: the .proj kind comes first.
    fs::write(a.join("lib/Fake.toml"), "").unwrap();
    // The window shows a second after the open helper exits.
    let broker = ws.broker("one", &["--delay", "1"], false, None);
    let mut first = Client::connect_from(&broker.socket(), &a, 10);
    let mut second = Client::connect_from(&broker.socket(), &a, 10);
    for client in [&mut first, &mut second] {
        client.send(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "Build", "arguments": {}}}));
    }
    assert_eq!(text(&first.answer(json!(1))), "built in window win-1");
    assert_eq!(text(&second.answer(json!(1))), "built in window win-1");
    let opens = ws.opens();
    assert_eq!(opens.len(), 1, "{opens:?}");
    let project = a.join("App.proj");
    assert_eq!(
        opens[0]["argv"].as_array().unwrap().last().unwrap(),
        &json!(project)
    );
    assert_eq!(opens[0]["cwd"], "/");
    let record: Value =
        serde_json::from_slice(&fs::read(broker.dir.join("fake.windows.json")).unwrap()).unwrap();
    let windows = record["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 1, "{record}");
    assert_eq!(windows[0]["root"], json!(a));
    assert_eq!(windows[0]["project"], json!(project));
    assert_eq!(windows[0]["target"], "win-1");
    assert_eq!(broker.state()["windows"][0]["target"], "win-1");

    // Ambiguous: refused with the candidates; --project picks one, and a
    // --project outside the workspace is refused.
    let c = ws.worktree("3", &["X.proj", "Y.proj"]);
    let mut unsure = Client::connect_from(&broker.socket(), &c, 10);
    let refused = unsure.call(json!(1), "Build", json!({}));
    assert!(is_error(&refused), "{refused}");
    assert_eq!(
        text(&refused),
        "more than one project in this workspace: X.proj, Y.proj; start the proxy with --project REL"
    );
    let mut sure = Client::connect_with(&broker.socket(), &c, &["--project", "Y.proj"]);
    assert_eq!(
        text(&sure.call(json!(1), "Build", json!({}))),
        "built in window win-2"
    );
    assert_eq!(
        ws.opens().last().unwrap()["argv"]
            .as_array()
            .unwrap()
            .last()
            .unwrap(),
        &json!(c.join("Y.proj"))
    );
    let d = ws.worktree("4", &["Z.proj"]);
    let mut escaping = Client::connect_with(
        &broker.socket(),
        &d,
        &["--project", "../../1/repo/App.proj"],
    );
    let refused = escaping.call(json!(1), "Build", json!({}));
    assert!(is_error(&refused), "{refused}");
    assert!(text(&refused).contains("--project"), "{refused}");
    // Nothing of any kind: the refusal lists the kinds.
    let e = ws.worktree("5", &["vendor/V.proj"]);
    let mut empty = Client::connect_from(&broker.socket(), &e, 10);
    let refused = empty.call(json!(1), "Build", json!({}));
    assert_eq!(
        text(&refused),
        "no project in this workspace (looked for .proj, Fake.toml)"
    );
    assert_eq!(ws.opens().len(), 2);
}

/// The window lists the upstream has received.
fn window_lists(broker: &Broker) -> usize {
    received(broker, "ListWindows").len()
}

#[test]
fn a_slow_open_runs_once_and_every_caller_is_busy_by_its_deadline() {
    let temp = short_temp();
    let ws = Workspaces::new(temp.path());
    let a = ws.worktree("1", &["A.proj"]);
    // The open helper runs for 4 s, past the 2 s each call may wait.
    let broker = ws.broker("one", &["--hold", "4"], false, Some(2));
    let mut first = Client::connect_from(&broker.socket(), &a, 10);
    let mut second = Client::connect_from(&broker.socket(), &a, 10);
    // Ready, with the tool list cached, so the waits below are the window's.
    for client in [&mut first, &mut second] {
        assert_eq!(
            text(&client.call(json!(1), "Search", json!({"query": "q"}))),
            "found q"
        );
    }
    let busy = "Fake tools busy: Fake is opening this workspace's window; retry after 15 s";
    let first_sent = Instant::now();
    first.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "Build", "arguments": {}}}));
    eventually(Duration::from_secs(5), "the open helper", || {
        (!ws.opens().is_empty()).then_some(())
    });
    let second_sent = Instant::now();
    second.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "Build", "arguments": {}}}));
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
        text(&first.call(json!(3), "Build", json!({}))),
        "built in window win-1"
    );
    assert_eq!(ws.opens().len(), 1, "{:?}", ws.opens());
    assert!(received(&broker, "Build").len() == 1);
}

#[test]
fn a_workspace_released_while_its_window_opens_closes_it_when_the_open_ends() {
    let temp = short_temp();
    let ws = Workspaces::new(temp.path());
    let a = ws.worktree("1", &["A.proj"]);
    let broker = ws.broker("one", &["--hold", "3"], false, Some(1));
    let mut client = Client::connect_from(&broker.socket(), &a, 10);
    assert_eq!(
        text(&client.call(json!(1), "Search", json!({"query": "q"}))),
        "found q"
    );
    let busy = client.call(json!(2), "Build", json!({}));
    assert!(text(&busy).contains("is opening"), "{busy}");
    // Recorded as it opens, so a release asks; it closes when the open ends.
    let project = a.join("A.proj");
    let recorded: Value =
        serde_json::from_slice(&fs::read(broker.dir.join("fake.windows.json")).unwrap()).unwrap();
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
    let temp = short_temp();
    let ws = Workspaces::new(temp.path());
    let a = ws.worktree("1", &["A.proj"]);
    ws.seed(&[("win-a", &a.join("A.proj"))]);
    let broker = ws.broker("one", &[], false, None);
    let mut client = Client::connect_from(&broker.socket(), &a, 10);
    assert_eq!(
        text(&client.call(json!(1), "Search", json!({"query": "q"}))),
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
    let refused = client.call(json!(2), "Mystery", json!({"windowId": "win-a"}));
    assert!(is_error(&refused), "{refused}");
    assert_eq!(
        text(&refused),
        "the upstream does not list the tool \"Mystery\"; a call naming a windowId is not forwarded"
    );
    assert_eq!(lists(), before + 1, "listed again, once");
    // Without a window it goes up as a window-free call.
    let forwarded = client.call(json!(3), "Mystery", json!({}));
    assert_eq!(forwarded["error"]["message"], "unknown tool", "{forwarded}");
    assert_eq!(lists(), before + 2);
    let sent = received(&broker, "Mystery");
    assert_eq!(sent, [json!({})]);
}

#[test]
fn a_window_that_never_shows_is_busy_and_a_failed_open_is_an_error() {
    let temp = short_temp();
    let ws = Workspaces::new(temp.path());
    let a = ws.worktree("1", &["A.proj"]);
    let never = ws.broker("never", &["--never"], false, Some(2));
    let mut client = Client::connect_from(&never.socket(), &a, 10);
    // Ready first, so the wait below is the window's alone.
    assert_eq!(
        text(&client.call(json!(1), "Search", json!({"query": "q"}))),
        "found q"
    );
    let started = Instant::now();
    let busy = client.call(json!(2), "Build", json!({}));
    let took = started.elapsed();
    assert!(took < Duration::from_secs(3), "{took:?}");
    assert_eq!(
        text(&busy),
        "Fake tools busy: Fake is opening this workspace's window; retry after 15 s"
    );
    assert_eq!(busy["result"]["_meta"]["privateer/retry_after_secs"], 15);
    assert!(received(&never, "Build").is_empty());
    drop(never);

    let failing = ws.broker("failing", &["--fail"], false, None);
    let mut client = Client::connect_from(&failing.socket(), &a, 10);
    let failed = client.call(json!(1), "Build", json!({}));
    assert!(is_error(&failed), "{failed}");
    let shown = text(&failed);
    assert!(
        shown.starts_with("could not open this workspace's Fake window:")
            && shown.contains("open failed")
            && !shown.contains("second line"),
        "{shown}"
    );
    assert!(received(&failing, "Build").is_empty());
    // Recorded while it opened, dropped once the open failed.
    eventually(Duration::from_secs(10), "an empty record", || {
        let record: Value =
            serde_json::from_slice(&fs::read(failing.dir.join("fake.windows.json")).ok()?).ok()?;
        (record["windows"] == json!([])).then_some(())
    });
}

#[test]
fn without_an_open_helper_a_window_call_with_no_window_is_refused() {
    let temp = short_temp();
    let ws = Workspaces::new(temp.path());
    let a = ws.worktree("1", &["A.proj"]);
    let dir = ws.root.join("one");
    fs::create_dir_all(&dir).unwrap();
    let profile: Value = serde_json::from_slice(&fs::read(fake_profile()).unwrap()).unwrap();
    let options = vec![
        "--label".to_owned(),
        "Fake".into(),
        "--workspace-pool".into(),
        ws.pool.to_string_lossy().into_owned(),
        "--routing".into(),
        profile["routing"].to_string(),
    ];
    let upstream = ws.upstream_args(&fake_profile(), false);
    let upstream: Vec<&str> = upstream.iter().map(String::as_str).collect();
    let broker = Broker::start_with(&dir, &upstream, None, &options);
    let mut client = Client::connect_from(&broker.socket(), &a, 10);
    let refused = client.call(json!(1), "Build", json!({}));
    assert_eq!(
        text(&refused),
        "no window helper configured: cannot open this workspace's Fake window"
    );
    assert!(received(&broker, "Build").is_empty());
}

#[test]
fn a_released_workspace_closes_only_the_windows_the_broker_opened() {
    let temp = short_temp();
    let ws = Workspaces::new(temp.path());
    let a = ws.worktree("1", &["A.proj"]);
    let b = ws.worktree("2", &["B.proj"]);
    // b's window was open already: the broker never closes it.
    ws.seed(&[("win-b", &b.join("B.proj"))]);
    let mut broker = ws.broker("one", &[], false, None);
    let mut client = Client::connect_from(&broker.socket(), &a, 10);
    assert_eq!(
        text(&client.call(json!(1), "Build", json!({}))),
        "built in window win-1"
    );
    let project = a.join("A.proj");
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
        .map(|w| w["target"].clone())
        .collect();
    assert_eq!(open, [json!("win-b")]);
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
        let error = bad["error"]
            .as_str()
            .unwrap_or_else(|| panic!("{root}: {bad}"));
        assert!(
            error.contains("strictly inside a workspace pool"),
            "{error}"
        );
    }
    assert_eq!(ws.closes().len(), 1);
    // Opened again, then the broker restarts: the new one still closes it.
    assert_eq!(
        text(&client.call(json!(2), "Build", json!({}))),
        "built in window win-1"
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
    let temp = short_temp();
    let ws = Workspaces::new(temp.path());
    let a = ws.worktree("1", &["A.proj"]);
    ws.seed(&[("win-a", &a.join("A.proj"))]);
    let broker = ws.broker("one", &[], false, None);
    let mut client = Client::connect_from(&broker.socket(), &a, 10);
    assert_eq!(
        text(&client.call(json!(1), "Grep", json!({"pattern": "x"}))),
        "grep in window None"
    );
    assert_eq!(
        text(&client.call(json!(2), "retool", json!({}))),
        "retooled"
    );
    assert_eq!(
        text(&client.call(json!(3), "Grep", json!({"pattern": "x"}))),
        "grep in window win-a"
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
fn install_pins_the_profile_pools_and_window_helpers() {
    let temp = short_temp();
    let machine = Machine::new(temp.path());
    let here = machine.root.clone();
    let extra = machine.root.join("more-workspaces");
    fs::create_dir_all(&extra).unwrap();
    let profile = fake_profile();
    let open = r#"["/usr/bin/true","open"]"#;
    let close = r#"["/usr/bin/true","close"]"#;
    let installed = machine.run(
        &here,
        &[
            "mcp-broker",
            "install",
            "--name",
            "fake",
            "--profile",
            profile.to_str().unwrap(),
            "--workspace-pool",
            extra.to_str().unwrap(),
            "--window-open",
            open,
            "--window-close",
            close,
            "--upstream",
            "/opt/x/mcp",
        ],
    );
    let bytes = fs::read(machine.dir().join("fake.json")).unwrap();
    let config: Value = serde_json::from_slice(&bytes).unwrap();
    let own_pool = fs::canonicalize(machine.ledger().join("worktrees")).unwrap();
    assert_eq!(config["workspace_pools"], json!([own_pool, extra]));
    assert_eq!(config["upstream"], json!(["/opt/x/mcp"]));
    assert_eq!(config["label"], "Fake");
    // The flags override the profile's helpers; the rest is the profile's,
    // in canonical form.
    assert_eq!(config["routing"]["open"], json!(["/usr/bin/true", "open"]));
    assert_eq!(
        config["routing"]["close"],
        json!(["/usr/bin/true", "close"])
    );
    assert_eq!(config["routing"]["list"]["tool"], "ListWindows");
    assert_eq!(config["routing"]["project"]["skip_dirs"], json!(["vendor"]));
    assert_eq!(installed["routing"], config["routing"]);
    assert_eq!(installed["workspace_pools"], config["workspace_pools"]);
    // The pin covers it all: it is the hash of exactly these bytes.
    let pin = fs::read_to_string(machine.pin("fake")).unwrap();
    assert_eq!(pin.trim(), {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(&bytes))
    });
    // Refused inputs rewrite nothing.
    let not_dir = extra.join("missing");
    let dotted = format!("{}/./x", extra.display());
    for args in [
        vec!["--window-close", r#"["/bin/sh","-c","true"]"#],
        vec!["--workspace-pool", dotted.as_str()],
        vec!["--workspace-pool", not_dir.to_str().unwrap()],
    ] {
        let mut all = vec![
            "mcp-broker",
            "install",
            "--name",
            "fake",
            "--profile",
            profile.to_str().unwrap(),
        ];
        all.extend(&args);
        all.extend(["--upstream", "/opt/x/mcp"]);
        machine.refused(&here, &all);
    }
    assert_eq!(fs::read(machine.dir().join("fake.json")).unwrap(), bytes);
}

// ---- The Xcode example profile, with fakes ---------------------------------

#[test]
fn the_xcode_example_profile_routes_builds_and_discovers_its_projects() {
    let temp = short_temp();
    let ws = Workspaces::new(temp.path());
    fs::create_dir_all(ws.root.join("m")).unwrap();
    let machine = Machine::new(&ws.root.join("m"));
    let upkeep = machine.ship("upkeep");
    machine.run(&upkeep, &["machine", "owner", "upkeep"]);
    let example = xcode_profile();
    // The fake upstream reads the same file the broker pins.
    let mut fake = upstream(&ws.root.join("record.jsonl"), &[]);
    fake.extend(ws.upstream_args(&example, false));
    let open = ws.helper("open", &[]);
    let close = ws.helper("close", &[]);
    let install = install_args(
        "xcode",
        &[
            "--profile",
            example.to_str().unwrap(),
            "--workspace-pool",
            ws.pool.to_str().unwrap(),
            "--window-open",
            &open,
            "--window-close",
            &close,
        ],
        &fake,
    );
    let installed = machine.run(&upkeep, &install);
    assert_eq!(installed["label"], "Xcode");
    assert_eq!(installed["routing"]["target_argument"], "tabIdentifier");
    let _detached = Detached(machine.dir().join("xcode.pid"));
    let started = machine.pass(&upkeep);
    assert!(started["xcode"]["started"].is_i64(), "{started}");
    let socket = machine.dir().join("xcode.sock");
    eventually(Duration::from_secs(10), "the socket", || {
        socket.exists().then_some(())
    });
    // .xcworkspace is preferred, and the workspace inside an .xcodeproj and
    // anything under Pods are never candidates.
    let a = ws.worktree(
        "1",
        &[
            "App.xcodeproj/project.xcworkspace",
            "App.xcworkspace",
            "Pods/Pods.xcworkspace",
        ],
    );
    let b = ws.worktree(
        "2",
        &[
            "App.xcodeproj/project.xcworkspace",
            "Pods/Pods.xcworkspace",
            "DerivedData/Build.xcworkspace",
            ".build/Hidden.xcworkspace",
        ],
    );
    let mut first = Client::connect_from(&socket, &a, 10);
    let mut second = Client::connect_from(&socket, &b, 10);
    // Build with no tabIdentifier reaches the upstream with its own tab.
    assert_eq!(
        text(&first.call(json!(1), "Build", json!({}))),
        "built in window win-1"
    );
    assert_eq!(
        text(&second.call(json!(1), "Build", json!({}))),
        "built in window win-2"
    );
    let opened: Vec<Value> = ws
        .opens()
        .iter()
        .map(|o| o["argv"].as_array().unwrap().last().unwrap().clone())
        .collect();
    assert_eq!(
        opened,
        [
            json!(a.join("App.xcworkspace")),
            json!(b.join("App.xcodeproj"))
        ]
    );
    let builds = received_in(&records(&ws.root.join("record.jsonl")), "Build");
    assert_eq!(
        builds,
        [
            json!({"tabIdentifier": "win-1"}),
            json!({"tabIdentifier": "win-2"})
        ]
    );
    let refused = first.call(json!(2), "Build", json!({"tabIdentifier": "win-2"}));
    assert_eq!(
        text(&refused),
        "tabIdentifier win-2 is not this workspace's Xcode window; call XcodeListWindows"
    );
    drop(first);
    drop(second);
    machine.run(&upkeep, &["mcp-broker", "stop", "--name", "xcode"]);
}

#[test]
fn the_xcode_examples_close_script_is_a_valid_helper_that_holds_no_path() {
    let output = Command::new(BROKER)
        .arg("check-profile")
        .stdin(fs::File::open(xcode_profile()).unwrap())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let canonical: Value = serde_json::from_slice(&output.stdout).unwrap();
    let close: Vec<String> = serde_json::from_value(canonical["routing"]["close"].clone()).unwrap();
    assert_eq!(close[0], "/usr/bin/osascript");
    assert!(close.len() <= 32, "{}", close.len());
    // The project path is the script's argument, never part of it.
    assert!(
        close
            .iter()
            .any(|line| line == "set wantedPath to item 1 of argv")
    );
    for line in &close[1..] {
        assert!(
            !line.replace("\"/\"", "").contains('/') && !line.contains("do shell"),
            "{line}"
        );
    }
    // An Xcode that is not running is never launched just to close.
    assert!(close.iter().any(|line| line.contains("is running then")));
    assert_eq!(
        canonical["routing"]["open"],
        json!(["/usr/bin/open", "-g", "-a", "Xcode"])
    );
}
