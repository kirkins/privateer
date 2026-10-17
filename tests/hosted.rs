//! Hosted first mates: the dispatcher launches a ship's first mate as a live
//! session in its own Herdr space. A fake Herdr runs what the dispatcher types
//! into the new pane, so the real `privateer start --hosted` and its harness
//! (a fake `pi` on PATH) run as subprocesses.
mod support;

use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use support::BIN;

// Concurrent forks can briefly retain a writable descriptor for a copied
// fixture executable. Retry only Linux ETXTBSY before the child starts, with
// the same bounded window used by the other copied-binary fixtures.
fn retry_fixture_exec<T>(mut exec: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match exec() {
            Err(error)
                if error.raw_os_error() == Some(libc::ETXTBSY) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => return result,
        }
    }
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct Place {
    _temp: tempfile::TempDir,
    root: PathBuf,
    folder: PathBuf,
    binary: PathBuf,
    path: String,
}

impl Place {
    /// The Pi ship "infra" with `first_mate.host = "herdr"`. Its `pi` records
    /// how it was started; a first mate stays up until `release` exists, and
    /// a hosted crew member ends at once.
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let folder = root.join("infra");
        fs::create_dir(&folder).unwrap();
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        // Existing hosted-mate cases deliberately exercise an installation
        // WITHOUT a dashboard, independent of Cargo's sibling build artifacts.
        let binary = bin.join("privateer");
        fs::copy(BIN, &binary).unwrap();
        fs::write(
            bin.join("pi"),
            format!(
                "#!/bin/sh\nif [ \"$PRIVATEER_HOSTED_CREW\" = 1 ]; then printf '%s\\n' \"$@\" > '{root}/crew-argv'; printf '%s' \"$PRIVATEER_HOME\" > '{root}/crew-home'; exit 0; fi\n\
                 printf '%s\\n' \"$@\" > '{root}/pi-argv'\n\
                 printf '%s\\n' \"$$\" > '{root}/pi-pid'\n\
                 printf '%s\\n' \"$PRIVATEER_HOME\" \"$PRIVATEER_CONTROL_CLI\" \"$HOME\" \"$FIXTURE_AUTH\" > '{root}/pi-context'\n\
                 printf '%s %s\\n' \"$PRIVATEER_HOSTED\" \"$PRIVATEER_PRESENCE_HOLDER\" > '{root}/pi-env'\n\
                 if [ -e '{root}/native-fail' ]; then exit 17; fi\n\
                 while [ ! -e '{root}/release' ]; do sleep 0.05; done\n\
                 printf '%s\\n' '{{\"type\":\"session\",\"id\":\"11111111-1111-4111-8111-111111111111\"}}' '{{\"type\":\"agent_start\"}}' '{{\"type\":\"message_end\",\"message\":{{\"role\":\"assistant\",\"stopReason\":\"stop\",\"usage\":{{\"input\":1,\"output\":1}}}}}}' '{{\"type\":\"agent_settled\",\"aborted\":false}}'\n",
                root = root.display()
            ),
        )
        .unwrap();
        fs::set_permissions(bin.join("pi"), fs::Permissions::from_mode(0o755)).unwrap();
        let place = Self {
            _temp: temp,
            path: format!("{}:/usr/bin:/bin", bin.display()),
            root,
            folder,
            binary,
        };
        place.ok(&["init", "--harness", "pi"]);
        let mut first_mate = place.json("ship.show", json!({"name":"infra"}))["first_mate"].take();
        first_mate["host"] = json!("herdr");
        first_mate["batch_secs"] = json!(0);
        place.json("ship.put", json!({"name":"infra","first_mate":first_mate}));
        place
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = support::command(&self.binary);
        command
            .args(args)
            .current_dir(&self.folder)
            .env("PATH", &self.path)
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("PRIVATEER_SERVICE", "none")
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env("PRIVATEER_FLEET", self.root.join("fleet.sqlite"))
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_PANE_ID")
            .env_remove("PRIVATEER_HOME")
            .env_remove("PRIVATEER_ROLE")
            .env_remove("PRIVATEER_ACTOR");
        command
    }
    fn ok(&self, args: &[&str]) -> String {
        let output = retry_fixture_exec(|| self.command(args).output()).unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
    /// A command as the ship's agent, as its first mate runs them.
    fn agent(&self, command: &str, args: Value) -> Value {
        let output = retry_fixture_exec(|| {
            self.command(&[command, "--json", &args.to_string()])
                .env("PRIVATEER_ROLE", "agent")
                .env("PRIVATEER_ACTOR", "infra")
                .output()
        })
        .unwrap();
        assert!(
            output.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn json(&self, command: &str, args: Value) -> Value {
        serde_json::from_str(&self.ok(&[command, "--json", &args.to_string()])).unwrap()
    }
    /// One dispatcher pass, with Herdr's socket where its default would be.
    fn tick(&self) -> Value {
        let line = self.ok(&["serve", "--json", r#"{"once":true}"#]);
        serde_json::from_str::<Value>(&line).unwrap()["ships"][0].take()
    }
    fn at(&self, name: &str, command: &str, args: Value) -> Value {
        let home = self.folder.parent().unwrap().join(name).join(".privateer");
        let output = retry_fixture_exec(|| {
            self.command(&[
                "--home",
                home.to_str().unwrap(),
                command,
                "--json",
                &args.to_string(),
            ])
            .output()
        })
        .unwrap();
        assert!(
            output.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn database(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.folder.join(".privateer/privateer.sqlite")).unwrap()
    }
    fn retry_now(&self) {
        self.database()
            .execute("UPDATE host_launches SET retry_ms=0", [])
            .unwrap();
    }
    fn launch_state(&self) -> (i64, i64, Option<String>) {
        self.database()
            .query_row(
                "SELECT failures,retry_ms,error FROM host_launches WHERE ship='infra'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    }
    fn native_pid(&self) -> i32 {
        fs::read_to_string(self.root.join("pi-pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }
    fn stop_native(&self, ship: &str) {
        let pid = self.native_pid();
        fs::write(self.root.join("release"), "").unwrap();
        until("hosted lease release", || {
            self.at(ship, "ship.presence", json!({"ship":ship}))["attended"] == false
        });
        until(
            "actual native payload exit, before fixture cleanup",
            || unsafe { libc::kill(pid, 0) } == -1,
        );
        for file in ["release", "pi-pid", "pi-env", "pi-context", "pi-argv"] {
            fs::remove_file(self.root.join(file)).unwrap();
        }
    }
    fn wake_diagnostics(&self, phase: &str, wake: &str, job: &Value) {
        println!("{phase}: job.status {job}");
        let db = self.database();
        for query in [
            "SELECT json_object('id',id,'focus',focus,'focuses',focuses,'mode',mode,'status',status,'error',error,'harness',harness,'session_id',session_id) FROM wakes ORDER BY created_ms,id",
            "SELECT json_object('ship',ship,'focus',focus,'harness',harness,'session_id',session_id,'wakes',wakes,'last_turn_ms',last_turn_ms) FROM sessions ORDER BY ship,focus",
        ] {
            let mut stmt = db.prepare(query).unwrap();
            for row in stmt.query_map([], |r| r.get::<_, String>(0)).unwrap() {
                println!("{phase}: {}", row.unwrap());
            }
        }
        let dir = self
            .folder
            .join(".privateer/jobs")
            .join(wake)
            .join(job["incarnation"].as_str().unwrap());
        let mut logs: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.file_name().unwrap().to_string_lossy().contains(".log"))
            .collect();
        logs.sort();
        for log in logs {
            let bytes = fs::read(&log).unwrap();
            println!(
                "{phase}: COMPLETE {} ({} bytes)\n{}\nEND {}",
                log.display(),
                bytes.len(),
                String::from_utf8_lossy(&bytes),
                log.display()
            );
        }
    }
    fn wait_wake(&self, phase: &str, wake: &str, payload: bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let job = self.json("job.status", json!({"id":wake}));
            let terminal = matches!(
                job["status"].as_str().unwrap(),
                "succeeded" | "failed" | "cancelled" | "timed_out" | "orphaned"
            );
            if (payload && !terminal && self.root.join("pi-env").exists())
                || (!payload && job["status"] == "succeeded")
            {
                return job;
            }
            if terminal || Instant::now() >= deadline {
                self.wake_diagnostics(phase, wake, &job);
                panic!(
                    "{phase}: expected successful headless fallback (waiting for payload={payload}), got {job}"
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    fn stop_wake(&self, phase: &str, report: &Value) {
        println!("{phase}: wake report {report}");
        assert_eq!(
            report["status"], "running",
            "{phase}: planning failed: {report}"
        );
        let wake = report["wake"].as_str().unwrap();
        self.wait_wake(phase, wake, true);
        let pid = self.native_pid();
        fs::write(self.root.join("release"), "").unwrap();
        let job = self.wait_wake(phase, wake, false);
        self.wake_diagnostics(phase, wake, &job);
        until(
            "actual headless payload exit, before fixture cleanup",
            || unsafe { libc::kill(pid, 0) } == -1,
        );
        for file in ["release", "pi-pid", "pi-env", "pi-context", "pi-argv"] {
            fs::remove_file(self.root.join(file)).unwrap();
        }
    }
    fn wakes(&self) -> i64 {
        rusqlite::Connection::open(self.folder.join(".privateer/privateer.sqlite"))
            .unwrap()
            .query_row("SELECT COUNT(*) FROM wakes", [], |r| r.get(0))
            .unwrap()
    }
}

/// A Herdr that answers one request per connection, keeps its spaces in
/// sidebar order (starting with one of the captain's own), and runs the text
/// sent to a space it created with that pane's Herdr environment.
struct Herdr {
    requests: Arc<Mutex<Vec<Value>>>,
    spaces: Arc<Mutex<Vec<(String, Value)>>>,
    panes: Arc<Mutex<Vec<Value>>>,
    reject_create: Arc<AtomicBool>,
    suppress_input: Arc<AtomicBool>,
    #[cfg(feature = "dashboard")]
    dashboards: Arc<Mutex<Vec<DashboardChild>>>,
    #[cfg(feature = "dashboard")]
    slow_create: Arc<AtomicBool>,
    #[cfg(feature = "dashboard")]
    creating: Arc<AtomicBool>,
}

impl Herdr {
    /// `base` is the environment a pane starts with, as Herdr's own would be.
    fn start(socket: &Path, base: Vec<(String, String)>) -> Self {
        let listener = UnixListener::bind(socket).unwrap();
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let spaces = Arc::new(Mutex::new(vec![("w1".to_owned(), json!({}))]));
        let pane = |n: usize| {
            json!({"pane_id":format!("w{n}:p1"),"terminal_id":format!("t{n}"),
                "workspace_id":format!("w{n}"),"tab_id":format!("w{n}:t1"),
                "agent":null,"agent_status":"unknown"})
        };
        let panes = Arc::new(Mutex::new(vec![pane(1)]));
        let reject_create = Arc::new(AtomicBool::new(false));
        let suppress_input = Arc::new(AtomicBool::new(false));
        let (reject, suppress) = (reject_create.clone(), suppress_input.clone());
        let (seen, order, live) = (requests.clone(), spaces.clone(), panes.clone());
        let socket = socket.to_owned();
        #[cfg(feature = "dashboard")]
        let dashboards = Arc::new(Mutex::new(Vec::new()));
        #[cfg(feature = "dashboard")]
        let running = Arc::downgrade(&dashboards);
        #[cfg(feature = "dashboard")]
        let slow_create = Arc::new(AtomicBool::new(false));
        #[cfg(feature = "dashboard")]
        let creating = Arc::new(AtomicBool::new(false));
        #[cfg(feature = "dashboard")]
        let (slow, entered) = (slow_create.clone(), creating.clone());
        std::thread::spawn(move || {
            let mut created: Vec<Value> = vec![];
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut line = String::new();
                if BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .is_err()
                {
                    continue;
                }
                let Ok(request) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let params = &request["params"];
                if request["method"] == "workspace.create" && reject.load(Ordering::SeqCst) {
                    let _ = writeln!(
                        stream,
                        "{}",
                        json!({"id":request["id"],"error":"fixture host unavailable"})
                    );
                    seen.lock().unwrap().push(request);
                    continue;
                }
                if request["method"] == "pane.send_input" && suppress.load(Ordering::SeqCst) {
                    let _ = writeln!(
                        stream,
                        "{}",
                        json!({"id":request["id"],"result":{"type":"ok"}})
                    );
                    seen.lock().unwrap().push(request);
                    continue;
                }
                let result = match request["method"].as_str().unwrap_or("") {
                    "session.snapshot" => {
                        json!({"snapshot":{"version":"test","protocol":20,"panes":*live.lock().unwrap()}})
                    }
                    "workspace.create" => {
                        #[cfg(feature = "dashboard")]
                        if slow.load(Ordering::SeqCst) {
                            entered.store(true, Ordering::SeqCst);
                            // Remain inside the normal 800ms protocol deadline,
                            // so another HOME really contends with this launch.
                            std::thread::sleep(Duration::from_millis(250));
                        }
                        created.push(params["env"].clone());
                        let n = created.len() + 1;
                        order.lock().unwrap().push((format!("w{n}"), json!({})));
                        live.lock().unwrap().push(pane(n));
                        json!({"type":"workspace_created","root_pane":pane(n)})
                    }
                    "workspace.report_metadata" => {
                        let mut order = order.lock().unwrap();
                        if let Some(space) = order
                            .iter_mut()
                            .find(|(id, _)| *id == params["workspace_id"])
                        {
                            space.1 = params["tokens"].clone();
                        }
                        json!({"type":"ok"})
                    }
                    "workspace.list" => {
                        let order = order.lock().unwrap();
                        let spaces: Vec<Value> = order
                            .iter()
                            .map(|(id, tokens)| json!({"workspace_id":id,"tokens":tokens}))
                            .collect();
                        json!({"type":"workspace_list","workspaces":spaces})
                    }
                    "workspace.move_block" => {
                        let mut order = order.lock().unwrap();
                        let moved: Vec<String> =
                            serde_json::from_value(params["workspace_ids"].clone()).unwrap();
                        let block: Vec<(String, Value)> = moved
                            .iter()
                            .map(|id| order.iter().find(|(w, _)| w == id).unwrap().clone())
                            .collect();
                        order.retain(|(id, _)| !moved.contains(id));
                        let at = params["before_workspace_id"]
                            .as_str()
                            .and_then(|before| order.iter().position(|(id, _)| id == before))
                            .unwrap_or(order.len());
                        order.splice(at..at, block);
                        json!({"type":"ok"})
                    }
                    "pane.send_input" => {
                        let id = params["pane_id"].as_str().unwrap();
                        let n: usize = id[1..id.find(':').unwrap()].parse().unwrap();
                        let mut shell = Command::new("sh");
                        shell
                            .args(["-c", params["text"].as_str().unwrap()])
                            .env_clear()
                            .envs(base.iter().cloned())
                            .env("HERDR_ENV", "1")
                            .env("HERDR_SOCKET_PATH", &socket)
                            .env("HERDR_PANE_ID", id)
                            .env("HERDR_WORKSPACE_ID", format!("w{n}"))
                            .env("HERDR_TAB_ID", format!("w{n}:t1"))
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null());
                        for (key, value) in created[n - 2].as_object().unwrap() {
                            shell.env(key, value.as_str().unwrap());
                        }
                        #[cfg(feature = "dashboard")]
                        if created[n - 2].get("PRIVATEER_BRIDGE_TOKEN").is_some() {
                            let child = DashboardChild::spawn(shell);
                            if let Some(running) = running.upgrade() {
                                running.lock().unwrap().push(child);
                            }
                        } else {
                            let mut child = shell.spawn().unwrap();
                            std::thread::spawn(move || child.wait());
                        }
                        #[cfg(not(feature = "dashboard"))]
                        {
                            let mut child = shell.spawn().unwrap();
                            std::thread::spawn(move || child.wait());
                        }
                        json!({"type":"ok"})
                    }
                    _ => json!({"type":"ok"}),
                };
                let _ = writeln!(stream, "{}", json!({"id":request["id"],"result":result}));
                seen.lock().unwrap().push(request);
            }
        });
        Self {
            requests,
            spaces,
            panes,
            reject_create,
            suppress_input,
            #[cfg(feature = "dashboard")]
            dashboards,
            #[cfg(feature = "dashboard")]
            slow_create,
            #[cfg(feature = "dashboard")]
            creating,
        }
    }
    fn calls(&self, method: &str) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == method)
            .cloned()
            .collect()
    }
    /// The sidebar, top to bottom.
    fn order(&self) -> Vec<String> {
        self.spaces
            .lock()
            .unwrap()
            .iter()
            .map(|(id, _)| id.clone())
            .collect()
    }
}

/// The real dashboard runs directly in a disposable PTY. No ANSI transcript
/// matching: only kernel raw-mode readiness/restoration and normal exit/reap are
/// asserted. The behavior under test is Herdr launch/custody/focus, not rendering.
#[cfg(feature = "dashboard")]
struct DashboardChild {
    child: std::process::Child,
    master: fs::File,
    initial: libc::termios,
    drain: Option<std::thread::JoinHandle<()>>,
    reaped: bool,
}
#[cfg(feature = "dashboard")]
impl DashboardChild {
    fn attributes(file: &fs::File) -> libc::termios {
        use std::os::fd::AsRawFd;
        let mut attrs = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(file.as_raw_fd(), &mut attrs) }, 0);
        attrs
    }
    fn spawn(mut command: Command) -> Self {
        use std::os::{
            fd::{AsRawFd, FromRawFd},
            unix::process::CommandExt,
        };
        let (mut master, mut slave) = (-1, -1);
        let mut size = libc::winsize {
            ws_row: 32,
            ws_col: 110,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::addr_of_mut!(size),
                )
            },
            0
        );
        let master = unsafe { fs::File::from_raw_fd(master) };
        let slave = unsafe { fs::File::from_raw_fd(slave) };
        // openpty fds must not leak into any unrelated child or dashboard.
        for f in [&master, &slave] {
            assert_eq!(
                unsafe { libc::fcntl(f.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
        let initial = Self::attributes(&master);
        command
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave)
            .env("TERM", "xterm-256color");
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        // Drop Command's parent-side copies of the slave before waiting/reaping.
        drop(command);
        let mut output = master.try_clone().unwrap();
        let drain = Some(std::thread::spawn(move || {
            use std::io::Read;
            let mut block = [0; 8192];
            while matches!(output.read(&mut block), Ok(n) if n > 0) {}
        }));
        Self {
            child,
            master,
            initial,
            drain,
            reaped: false,
        }
    }
    fn stop(&mut self) {
        until("real dashboard raw-mode readiness", || {
            Self::attributes(&self.master).c_lflag & libc::ICANON == 0
        });
        self.master.write_all(b"q").unwrap();
        let mut exit = None;
        until(
            "dashboard normal exit and reap BEFORE fixture cleanup",
            || {
                exit = self.child.try_wait().unwrap();
                exit.is_some()
            },
        );
        self.reaped = true;
        assert!(exit.unwrap().success(), "dashboard did not exit normally");
        let restored = Self::attributes(&self.master);
        assert_eq!(
            (
                restored.c_iflag,
                restored.c_oflag,
                restored.c_cflag,
                restored.c_lflag,
                restored.c_cc
            ),
            (
                self.initial.c_iflag,
                self.initial.c_oflag,
                self.initial.c_cflag,
                self.initial.c_lflag,
                self.initial.c_cc
            )
        );
        self.drain.take().unwrap().join().unwrap();
    }
}
#[cfg(feature = "dashboard")]
impl Drop for DashboardChild {
    fn drop(&mut self) {
        if !self.reaped {
            // Failure-only cleanup of the DIRECT child; never evidence of pass
            // or payload death inferred from stopping a supervisor wrapper.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(feature = "dashboard")]
#[test]
fn machine_bridge_contends_reuses_and_recreates_without_automatic_focus() {
    let place = Place::new();
    let apps = place.root.join("apps/.privateer");
    fs::create_dir_all(apps.parent().unwrap()).unwrap();
    let infra = place.folder.join(".privateer");
    let fleet = place.root.join("state/privateer/fleet.sqlite");
    fs::create_dir_all(fleet.parent().unwrap()).unwrap();
    fs::rename(place.root.join("fleet.sqlite"), &fleet).unwrap();
    let coordination = fleet.with_file_name("fleet.sqlite.bridge");
    let runtime = place.root.join("runtime ' bridge");
    fs::create_dir(&runtime).unwrap();
    let binary = runtime.join("privateer");
    fs::copy(BIN, &binary).unwrap();
    // Authless disposable CHILD allowlist. Do not touch the test supervisor's
    // role/job/actor/control/mail/provider environment or product caller checks.
    let command = |home: &Path, args: &[&str]| {
        let mut c = Command::new(&binary);
        c.env_clear()
            .args(["--home", home.to_str().unwrap()])
            .args(args)
            .current_dir(home.parent().unwrap())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &place.root)
            .env("XDG_CONFIG_HOME", place.root.join("config"))
            .env("XDG_STATE_HOME", place.root.join("state"))
            // Deliberately no PRIVATEER_FLEET override: host must forward the
            // resolved default despite Herdr's different XDG environment.
            .env("PRIVATEER_RESOURCE_HOME", place.root.join("ledger"))
            .env("PRIVATEER_SERVICE", "none");
        c
    };
    let ok = |home: &Path, args: &[&str]| {
        let out = retry_fixture_exec(|| command(home, args).output()).unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let json = |home: &Path, verb: &str, value: Value| -> Value {
        serde_json::from_str(&ok(home, &[verb, "--json", &value.to_string()])).unwrap()
    };
    let tick = |home: &Path| json(home, "serve", json!({"once":true}));
    ok(&apps, &["init", "--harness", "pi"]);
    for (home, ship) in [(&infra, "infra"), (&apps, "apps")] {
        json(
            home,
            "ship.attend",
            json!({"ship":ship,"holder":"captain-fixture","ttl_secs":90}),
        );
    }
    let socket = place.root.join("config/herdr/herdr.sock");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let herdr = Herdr::start(
        &socket,
        vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HOME".into(), place.root.display().to_string()),
            (
                "XDG_CONFIG_HOME".into(),
                place.root.join("config").display().to_string(),
            ),
            (
                "XDG_STATE_HOME".into(),
                place.root.join("herdr-state").display().to_string(),
            ),
        ],
    );
    // Missing installed dashboard does not use a PATH harness/wrapper.
    assert!(tick(&infra)["bridge"].is_null());
    assert!(herdr.calls("workspace.create").is_empty());
    fs::copy(
        env!("CARGO_BIN_EXE_privateer-dashboard"),
        runtime.join("privateer-dashboard"),
    )
    .unwrap();
    let configure = |home: &Path, ship: &str, enabled: bool| {
        let mut profile = json(home, "ship.show", json!({"name":ship}))["first_mate"].take();
        if enabled {
            profile["host"] = json!("herdr");
        } else {
            profile.as_object_mut().unwrap().remove("host");
        }
        json(home, "ship.put", json!({"name":ship,"first_mate":profile}));
    };
    configure(&infra, "infra", false);
    assert!(tick(&infra)["bridge"].is_null());
    assert!(tick(&apps)["bridge"].is_null());
    assert!(!coordination.exists());
    configure(&infra, "infra", true);
    configure(&apps, "apps", true);
    let claim_path = coordination.join("claim.json");
    let claim = || -> Value { serde_json::from_slice(&fs::read(&claim_path).unwrap()).unwrap() };
    let retry = || {
        let mut c = claim();
        c["retry_ms"] = json!(0); // deterministic clock jump, not a blind sleep
        fs::write(&claim_path, c.to_string()).unwrap();
    };
    // No socket: bounded durable retry survives fresh dispatcher processes.
    fs::rename(&socket, socket.with_extension("saved")).unwrap();
    assert!(tick(&infra)["bridge"]["error"].is_string());
    let deadline = claim()["retry_ms"].clone();
    assert_eq!(tick(&apps)["bridge"]["retry_ms"], deadline);
    assert!(herdr.calls("workspace.create").is_empty());
    fs::rename(socket.with_extension("saved"), &socket).unwrap();
    retry();

    // Two genuinely independent homes/processes overlap. The first is inside
    // fake Herdr create (normal protocol deadline); the second must lose the
    // SAME machine claim rather than taking a per-home lock and launching too.
    herdr.slow_create.store(true, Ordering::SeqCst);
    let mut first = command(&infra, &["serve", "--json", r#"{"once":true}"#]);
    let first =
        retry_fixture_exec(|| first.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()).unwrap();
    until("first dispatcher inside fake Herdr create", || {
        herdr.creating.load(Ordering::SeqCst)
    });
    let second =
        retry_fixture_exec(|| command(&apps, &["serve", "--json", r#"{"once":true}"#]).output())
            .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(
        second["bridge"]["error"], "bridge reconciliation busy",
        "{second}"
    );
    let first = first.wait_with_output().unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first["bridge"]["status"], "launched", "{first}");
    herdr.slow_create.store(false, Ordering::SeqCst);
    until("direct dashboard claim adoption", || {
        claim()["adopted"] == true
    });
    assert_eq!(herdr.calls("workspace.create").len(), 1);
    assert_eq!(herdr.calls("pane.send_input").len(), 1);
    assert_eq!(herdr.panes.lock().unwrap().len(), 2);
    let create = &herdr.calls("workspace.create")[0]["params"];
    assert_eq!(create["focus"], false);
    assert_eq!(create["label"], "bridge");
    assert_eq!(create["env"]["PRIVATEER_FLEET"], fleet.to_str().unwrap());
    let input = herdr.calls("pane.send_input")[0]["params"]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        input.contains("'\"'\"'"),
        "launch paths must be shell quoted: {input}"
    );
    assert!(
        !input.contains("--home"),
        "bridge must observe fleet, not winning home"
    );
    let saved = claim();
    // A different home/restarted dispatcher repairs presentation, not custody.
    herdr
        .spaces
        .lock()
        .unwrap()
        .iter_mut()
        .find(|(id, _)| id == "w2")
        .unwrap()
        .1 = json!({});
    assert_eq!(tick(&apps)["bridge"]["status"], "live");
    assert_eq!(claim()["owner"], saved["owner"]);
    assert_eq!(claim()["token"], saved["token"]);
    assert_eq!(herdr.order(), ["w2", "w1"]); // first under Local, no global pin
    assert_eq!(herdr.spaces.lock().unwrap()[0].1["privateer"], "bridge");
    assert!(herdr.calls("pane.focus").is_empty());
    assert!(herdr.calls("agent.focus").is_empty());
    // Bare entry reuses exactly once, without execing a dashboard/harness.
    ok(&apps, &[]);
    assert_eq!(herdr.calls("pane.focus").len(), 1);
    assert_eq!(herdr.calls("workspace.create").len(), 1);
    assert_eq!(herdr.calls("pane.send_input").len(), 1);
    let original = herdr.panes.lock().unwrap().clone();
    let no_effects = || {
        assert_eq!(herdr.calls("workspace.create").len(), 1);
        assert_eq!(herdr.calls("pane.send_input").len(), 1);
        assert_eq!(herdr.calls("pane.focus").len(), 1);
        assert!(herdr.calls("agent.focus").is_empty());
    };
    // A removed/replaced lock PATH does not prove its original open inode (or
    // its actual dashboard) died. Pin both coordination incarnations too.
    for name in ["live", "guard"] {
        use std::os::unix::fs::OpenOptionsExt;
        let path = coordination.join(name);
        let saved_path = coordination.join(format!("{name}.saved"));
        fs::rename(&path, &saved_path).unwrap();
        if name == "live" {
            assert!(tick(&apps)["bridge"]["error"].is_string());
            assert!(
                !retry_fixture_exec(|| command(&apps, &[]).output())
                    .unwrap()
                    .status
                    .success()
            );
            no_effects();
        }
        fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        assert!(tick(&apps)["bridge"]["error"].is_string());
        assert!(
            !retry_fixture_exec(|| command(&apps, &[]).output())
                .unwrap()
                .status
                .success()
        );
        no_effects();
        fs::remove_file(&path).unwrap();
        fs::rename(saved_path, path).unwrap();
    }
    // Same plausible label is never endpoint ownership. Every authoritative
    // identity change, duplicate/malformed snapshot and changed socket refuses
    // BOTH automatic adoption/recreation and manual focus.
    for field in [
        "terminal_id",
        "workspace_id",
        "tab_id",
        "agent_session",
        "agent",
    ] {
        herdr.panes.lock().unwrap()[1][field] = json!("unexpected-occupant");
        assert!(tick(&apps)["bridge"]["error"].is_string());
        assert!(
            !retry_fixture_exec(|| command(&apps, &[]).output())
                .unwrap()
                .status
                .success()
        );
        no_effects();
        *herdr.panes.lock().unwrap() = original.clone();
    }
    for extra in [original[1].clone(), Value::Null] {
        herdr.panes.lock().unwrap().push(extra);
        assert!(tick(&apps)["bridge"]["error"].is_string());
        assert!(
            !retry_fixture_exec(|| command(&apps, &[]).output())
                .unwrap()
                .status
                .success()
        );
        no_effects();
        *herdr.panes.lock().unwrap() = original.clone();
    }
    fs::rename(&socket, socket.with_extension("old")).unwrap();
    let replacement = Herdr::start(&socket, vec![]);
    *replacement.panes.lock().unwrap() = original.clone();
    assert!(
        tick(&apps)["bridge"]["error"]
            .as_str()
            .unwrap()
            .contains("incarnation")
    );
    assert!(
        !retry_fixture_exec(|| command(&apps, &[]).output())
            .unwrap()
            .status
            .success()
    );
    assert!(replacement.calls("workspace.create").is_empty());
    assert!(replacement.calls("pane.send_input").is_empty());
    assert!(replacement.calls("pane.focus").is_empty());
    fs::remove_file(&socket).unwrap();
    fs::rename(socket.with_extension("old"), &socket).unwrap();
    configure(&apps, "apps", false);
    assert!(tick(&apps)["bridge"].is_null());
    assert!(
        !retry_fixture_exec(|| command(&apps, &[]).output())
            .unwrap()
            .status
            .success()
    );
    no_effects();
    configure(&apps, "apps", true);
    // Missing pane is NOT proof the live dashboard exited; no timer guesses.
    herdr.panes.lock().unwrap().pop();
    retry();
    assert_eq!(
        tick(&apps)["bridge"]["status"],
        "live dashboard; pane missing"
    );
    no_effects();
    *herdr.panes.lock().unwrap() = original.clone();
    herdr.dashboards.lock().unwrap()[0].stop(); // exit 0/reap/restoration first
    herdr.panes.lock().unwrap().pop();
    herdr.spaces.lock().unwrap().retain(|(id, _)| id != "w2");
    // Closure recreates in background. An accepted-but-delayed launch survives
    // dispatcher restart without duplicate creation or replaying launch input.
    herdr.suppress_input.store(true, Ordering::SeqCst);
    assert_eq!(tick(&apps)["bridge"]["status"], "launched");
    let pending = claim();
    assert_eq!(pending["adopted"], false);
    assert_eq!(
        tick(&infra)["bridge"]["status"],
        "awaiting dashboard or pane exit"
    );
    assert_eq!(herdr.calls("workspace.create").len(), 2);
    assert_eq!(herdr.calls("pane.send_input").len(), 2);
    herdr.panes.lock().unwrap().pop(); // explicit closure, not elapsed time
    herdr.spaces.lock().unwrap().retain(|(id, _)| id != "w3");
    herdr.suppress_input.store(false, Ordering::SeqCst);
    retry();
    assert_eq!(tick(&infra)["bridge"]["status"], "launched");
    until("recreated direct dashboard adoption", || {
        claim()["adopted"] == true
    });
    assert_ne!(claim()["token"], pending["token"]);
    // A delayed old executable cannot take custody after its token is fenced.
    let stale = command(&apps, &["--help"])
        .get_envs()
        .filter_map(|(k, v)| v.map(|v| (k.to_owned(), v.to_owned())))
        .collect::<Vec<_>>();
    let refused = retry_fixture_exec(|| {
        Command::new(runtime.join("privateer-dashboard"))
            .env_clear()
            .envs(stale.clone())
            .env("PRIVATEER_BRIDGE_TOKEN", pending["token"].as_str().unwrap())
            .output()
    })
    .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("stale bridge launch token"));
    assert_eq!(herdr.calls("workspace.create").len(), 3);
    assert_eq!(herdr.calls("pane.send_input").len(), 3);
    assert_eq!(herdr.panes.lock().unwrap().len(), 2);
    assert_eq!(herdr.calls("pane.focus").len(), 1);
    assert!(herdr.calls("agent.focus").is_empty());
    herdr.dashboards.lock().unwrap()[1].stop();
    use std::os::fd::AsRawFd;
    let lease = fs::File::open(coordination.join("live")).unwrap();
    assert_eq!(
        unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "actual dashboard lease must disappear BEFORE fixture cleanup"
    );
    assert!(!place.root.join("pi-pid").exists());
    println!(
        "bridge behavior: independent HOME contention, one initial pane/input, one manual focus, endpoint/socket ambiguity refusal, pending-token fencing, exit0/reap/terminal restoration and closure recreation; no automatic focus"
    );
}

#[test]
fn hosting_inherits_only_with_the_native_route_and_explicit_overrides_win() {
    let place = Place::new();
    let socket = place.root.join("config/herdr/herdr.sock");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let herdr = Herdr::start(
        &socket,
        vec![
            ("PATH".into(), place.path.clone()),
            ("HOME".into(), place.root.display().to_string()),
            (
                "XDG_CONFIG_HOME".into(),
                place.root.join("config").display().to_string(),
            ),
            (
                "FIXTURE_AUTH".into(),
                "native-environment-sentinel-not-a-credential".into(),
            ),
        ],
    );
    place.json(
        "bundle.configure",
        json!({"registry":{
        "schema_version":1,"pi_version":"1.2.0",
        "integrations":{"reviewed":true,"extensions":[]},"bundles":[]}}),
    );
    let mut parent = place.json("ship.show", json!({"name":"infra"}))["first_mate"].take();
    let route = json!({"provider":"fixture-provider", "model":"fixture-model"});
    parent["options"] = route.clone();
    parent["tool_bundles"] = json!([]);
    parent["batch_secs"] = json!(123);
    parent["retry_backoff_secs"] = json!(0);
    place.json("ship.put", json!({"name":"infra","first_mate":parent}));
    let original = place.json("ship.show", json!({"name":"infra"}));
    for (name, extra, host, options) in [
        ("implicit", json!({}), json!("herdr"), route.clone()),
        (
            "same",
            json!({"harness":"pi"}),
            json!("herdr"),
            route.clone(),
        ),
        ("headless", json!({"host":null}), Value::Null, route.clone()),
        (
            "codex",
            json!({"harness":"codex"}),
            Value::Null,
            json!({"sandbox":"workspace-write"}),
        ),
        (
            "signed",
            json!({"harness":"pi-signed"}),
            Value::Null,
            json!({}),
        ),
        (
            "explicit",
            json!({"harness":"pi-signed","host":"herdr"}),
            json!("herdr"),
            json!({}),
        ),
    ] {
        let mut args = extra;
        args["name"] = json!(name);
        args["actions"] = json!(["inspect", "spawn", "message"]);
        place.agent("ship.create", args);
        let profile = place.at(name, "ship.show", json!({"name":name}))["first_mate"].take();
        assert_eq!(profile["host"], host, "{name}: {profile}");
        assert_eq!(profile["options"], options, "{name}: {profile}");
        assert!(profile.get("tool_bundles").is_none(), "{profile}");
        assert!(profile.get("config").is_none(), "{profile}");
        assert_eq!(profile["batch_secs"], 5);
        assert_eq!(profile["retry_backoff_secs"], 60);
        assert_eq!(profile["duty_task"], format!("{name}-duty"));
        let charter = place.at(name, "charter.show", json!({"id":name}));
        assert_eq!(charter["actions"], json!(["inspect", "spawn", "message"]));
        assert_eq!(charter["production_requires_approval"], true);
    }
    assert_eq!(place.json("ship.show", json!({"name":"infra"})), original);
    assert_eq!(herdr.calls("workspace.create").len(), 0);
    let refused = retry_fixture_exec(|| {
        place
            .command(&[
                "ship.create",
                "--json",
                r#"{"name":"invalid","host":"other-host"}"#,
            ])
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", "infra")
            .output()
    })
    .unwrap();
    assert!(!refused.status.success());
    assert!(!place.root.join("invalid").exists());
    // A custom manifest that merely declares Pi's id is not a native-route
    // inheritance credential. Keep the old eligibility rule, not an id match.
    let custom = place.root.join("custom-pi.json");
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/pi/manifest.json"),
        &custom,
    )
    .unwrap();
    let mut custom_parent = original["first_mate"].clone();
    custom_parent["manifest"] = json!(custom);
    custom_parent
        .as_object_mut()
        .unwrap()
        .remove("tool_bundles");
    place.json(
        "ship.put",
        json!({"name":"infra","first_mate":custom_parent}),
    );
    place.agent(
        "ship.create",
        json!({"name":"unrelated-native","harness":"pi"}),
    );
    let unrelated = place.at(
        "unrelated-native",
        "ship.show",
        json!({"name":"unrelated-native"}),
    );
    assert!(unrelated["first_mate"].get("host").is_none());
    assert_eq!(unrelated["first_mate"]["options"], json!({}));
    place.json(
        "ship.put",
        json!({"name":"infra","first_mate":original["first_mate"]}),
    );
    // No mail or child bundle configuration is needed. The real native boundary
    // gets the inherited route, original native HOME/auth environment and the
    // child's own control/home identity, never its creator's bundle selection.
    let tick = place.at("implicit", "serve", json!({"once":true}));
    assert!(tick["ships"][0]["hosted"].is_object(), "{tick}");
    until("inherited child's actual native process", || {
        place.root.join("pi-env").exists()
    });
    let argv = fs::read_to_string(place.root.join("pi-argv")).unwrap();
    let argv: Vec<_> = argv.lines().collect();
    assert!(
        argv.windows(2)
            .any(|p| p == ["--provider", "fixture-provider"])
    );
    assert!(argv.windows(2).any(|p| p == ["--model", "fixture-model"]));
    assert!(argv.contains(&"--continue"));
    assert!(!argv.contains(&"--print"));
    let context = fs::read_to_string(place.root.join("pi-context")).unwrap();
    let context: Vec<_> = context.lines().collect();
    assert_eq!(
        context[0],
        place.root.join("implicit/.privateer").to_str().unwrap()
    );
    assert_eq!(
        fs::canonicalize(context[1]).unwrap(),
        fs::canonicalize(&place.binary).unwrap()
    );
    assert_eq!(context[2], place.root.to_str().unwrap());
    assert_eq!(context[3], "native-environment-sentinel-not-a-credential");
    assert_eq!(herdr.order(), ["w2", "w1"]);
    assert_eq!(
        herdr.spaces.lock().unwrap()[0].1,
        json!({"privateer":"mate","ship":"implicit"})
    );
    assert_eq!(
        place.at("implicit", "serve", json!({"once":true}))["ships"][0]["skipped"],
        "its live first mate has the mail"
    );
    assert_eq!(herdr.calls("workspace.create").len(), 1);
    place.stop_native("implicit");
}

#[test]
fn ordinary_idle_ticks_launch_once_and_back_off_durably_before_recovery() {
    let place = Place::new();
    let socket = place.root.join("config/herdr/herdr.sock");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let herdr = Herdr::start(
        &socket,
        vec![
            ("PATH".into(), place.path.clone()),
            ("HOME".into(), place.root.display().to_string()),
            (
                "XDG_CONFIG_HOME".into(),
                place.root.join("config").display().to_string(),
            ),
        ],
    );
    assert!(
        place.json("mail.inbox", json!({"address":"infra"}))["messages"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // Attendance, native compatibility, effective authority and cwd all apply
    // before an idle launch just as they do for mail-driven work.
    place.json(
        "ship.attend",
        json!({"ship":"infra","holder":"captain-fixture","ttl_secs":90}),
    );
    assert_eq!(place.tick()["skipped"], "captain's session is aboard");
    place.json(
        "ship.leave",
        json!({"ship":"infra","holder":"captain-fixture"}),
    );
    let original = place.json("ship.show", json!({"name":"infra"}))["first_mate"].take();
    let mut unsupported = original.clone();
    unsupported["manifest"] = json!("bundled:codex");
    place.json("ship.put", json!({"name":"infra","first_mate":unsupported}));
    assert!(
        place.tick()["host_error"]
            .as_str()
            .unwrap()
            .contains("cannot be hosted")
    );
    place.json("ship.put", json!({"name":"infra","first_mate":original}));
    let wrong_repo = place.root.join("other-repo");
    fs::create_dir(&wrong_repo).unwrap();
    place.json("charter.put", json!({"id":"infra","repo":wrong_repo}));
    place.retry_now();
    assert!(
        place.tick()["host_error"]
            .as_str()
            .unwrap()
            .contains("cwd escapes")
    );
    place.json("charter.put", json!({"id":"infra","repo":place.folder}));
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/codex/manifest.json");
    place.json(
        "charter.put",
        json!({"id":"infra","provider_plugin":{"manifest":manifest}}),
    );
    place.retry_now();
    let pinned = place.tick();
    println!("idle native provider-pin refusal: {pinned}");
    // Native hosting supplies bundled:pi. The existing pinned-provider path
    // requires an absolute supplied manifest, so it refuses that reference
    // before comparing adapter identities. Do not change either route to make
    // this fixture reach the later 'selected provider adapter' diagnostic.
    assert_eq!(
        pinned["host_error"], "plugin paths must be absolute",
        "{pinned}"
    );
    assert!(pinned["hosted"].is_null(), "{pinned}");
    assert!(pinned["wake"].is_null(), "{pinned}");
    assert_eq!(place.wakes(), 0);
    assert_eq!(herdr.calls("workspace.create").len(), 0);
    assert_eq!(herdr.calls("pane.send_input").len(), 0);
    assert!(!place.root.join("pi-pid").exists());
    let presence = place.json("ship.presence", json!({"ship":"infra"}));
    assert_eq!(presence["attended"], false, "{presence}");
    assert!(presence["lease"].is_null(), "{presence}");
    assert_eq!(
        place.json("charter.show", json!({"id":"infra"}))["provider_plugin"],
        json!({"manifest":manifest})
    );
    assert_eq!(
        place.json("ship.show", json!({"name":"infra"}))["first_mate"],
        original
    );
    place.json("charter.put", json!({"id":"infra","provider_plugin":null}));
    place.json("charter.revoke", json!({"id":"infra"}));
    place.retry_now();
    assert!(
        place.tick()["host_error"]
            .as_str()
            .unwrap()
            .contains("revoked")
    );
    place.json("charter.put", json!({"id":"infra","active":true}));
    place
        .database()
        .execute("UPDATE tasks SET status='done' WHERE id='infra-duty'", [])
        .unwrap();
    place.retry_now();
    assert!(
        place.tick()["host_error"]
            .as_str()
            .unwrap()
            .contains("terminal")
    );
    place
        .database()
        .execute("UPDATE tasks SET status='open' WHERE id='infra-duty'", [])
        .unwrap();
    assert_eq!(herdr.calls("workspace.create").len(), 0);
    assert_eq!(
        place.json("ship.presence", json!({"ship":"infra"}))["attended"],
        false
    );
    place.retry_now();

    // Racing ordinary maintenance passes cannot launch twice. Each starts a
    // fresh Store/process; there is no resident-service startup special case.
    let mut passes = vec![];
    for _ in 0..3 {
        let mut pass = place.command(&["serve", "--json", r#"{"once":true}"#]);
        pass.stdout(Stdio::piped()).stderr(Stdio::piped());
        passes.push(retry_fixture_exec(|| pass.spawn()).unwrap());
    }
    let mut succeeded = 0;
    for pass in passes {
        let output = pass.wait_with_output().unwrap();
        if output.status.success() {
            succeeded += 1;
        } else {
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("another serve is already running")
            );
        }
    }
    assert!(succeeded > 0);
    until("idle native process without any mail", || {
        place.root.join("pi-env").exists()
    });
    assert_eq!(herdr.calls("workspace.create").len(), 1);
    assert_eq!(place.wakes(), 0);
    let lease = place.json("ship.presence", json!({"ship":"infra"}))["lease"].clone();
    let holder = lease["holder"].clone();
    assert!(holder.as_str().unwrap().starts_with("host:"));
    assert_eq!(lease["harness"], "pi");
    assert_eq!(place.tick()["skipped"], "its live first mate has the mail");
    assert_eq!(herdr.order(), ["w2", "w1"]);
    place.stop_native("infra");

    // A host refusal releases only its launch lease and stores a bounded retry.
    // Repeated fresh dispatcher processes neither call the host again nor append
    // an error/audit every tick. Setting retry_ms is a deterministic clock jump.
    herdr.reject_create.store(true, Ordering::SeqCst);
    place.retry_now();
    assert!(place.tick()["host_error"].is_string());
    assert_eq!(
        place.json("ship.presence", json!({"ship":"infra"}))["attended"],
        false
    );
    let state = place.launch_state();
    assert_eq!(state.0, 1);
    let remaining = state.1 - privateer::store::Store::now_ms();
    assert!((50_000..=60_000).contains(&remaining), "{state:?}");
    let calls = herdr.calls("workspace.create").len();
    let audit: i64 = place
        .database()
        .query_row("SELECT COUNT(*) FROM audit", [], |r| r.get(0))
        .unwrap();
    for _ in 0..3 {
        let report = place.tick();
        assert_eq!(report["host_backoff_until_ms"], state.1);
        assert!(report["host_error"].is_null(), "{report}");
    }
    assert_eq!(herdr.calls("workspace.create").len(), calls);
    assert_eq!(place.launch_state(), state);
    assert_eq!(
        place
            .database()
            .query_row("SELECT COUNT(*) FROM audit", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        audit
    );
    // Host backoff does not consume mail, block the headless fallback, or compete
    // with a running wake. Its real native payload must exit before cleanup.
    place.json(
        "mail.send",
        json!({"from":"captain","to":"infra","kind":"request","body":"fallback"}),
    );
    let report = place.tick();
    assert!(report["wake"].is_string(), "{report}");
    assert_eq!(report["host_backoff_until_ms"], state.1);
    until("headless native payload", || {
        place.root.join("pi-env").exists()
    });
    place.retry_now();
    assert_eq!(place.tick()["skipped"], "first mate already awake");
    assert_eq!(herdr.calls("workspace.create").len(), calls);
    place.stop_wake("idle backoff fallback", &report);
    herdr.reject_create.store(false, Ordering::SeqCst);
    // A host that accepts input but never runs it leaves an unadopted lease.
    // Even its expiry and dispatcher restart cannot lose the reserved backoff.
    herdr.suppress_input.store(true, Ordering::SeqCst);
    let pending = place.tick();
    assert!(pending["hosted"].is_object(), "{pending}");
    let state = place.launch_state();
    assert_eq!(state.0, 2);
    assert!((100_000..=120_000).contains(&(state.1 - privateer::store::Store::now_ms())));
    assert!(!place.root.join("pi-env").exists());
    place
        .database()
        .execute("UPDATE presence SET expires_ms=0 WHERE ship='infra'", [])
        .unwrap();
    assert_eq!(place.tick()["host_backoff_until_ms"], state.1);
    assert!(!place.root.join("pi-env").exists());
    assert_eq!(place.launch_state(), state);
    // Saturate repeated failures without wall-clock sleeps; the deadline and
    // row count remain bounded, and later configuration/host recovery works.
    herdr.reject_create.store(true, Ordering::SeqCst);
    for _ in 0..9 {
        place.retry_now();
        assert!(place.tick()["host_error"].is_string());
    }
    let state = place.launch_state();
    assert_eq!(state.0, 7);
    assert!((3_590_000..=3_600_000).contains(&(state.1 - privateer::store::Store::now_ms())));
    assert_eq!(
        place
            .database()
            .query_row("SELECT COUNT(*) FROM host_launches", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    herdr.reject_create.store(false, Ordering::SeqCst);
    herdr.suppress_input.store(false, Ordering::SeqCst);
    // A real native startup failure after adoption is also rate limited.
    fs::write(place.root.join("native-fail"), "").unwrap();
    place.retry_now();
    assert!(place.tick()["hosted"].is_object());
    until("native refusal lease release", || {
        place.root.join("pi-env").exists()
            && place.json("ship.presence", json!({"ship":"infra"}))["attended"] == false
    });
    let pid = place.native_pid();
    until(
        "failed native payload gone",
        || unsafe { libc::kill(pid, 0) } == -1,
    );
    assert!(place.launch_state().2.unwrap().contains("unsuccessfully"));
    let calls = herdr.calls("workspace.create").len();
    assert!(place.tick()["host_backoff_until_ms"].is_number());
    assert_eq!(herdr.calls("workspace.create").len(), calls);
    for file in ["native-fail", "pi-pid", "pi-env", "pi-context", "pi-argv"] {
        fs::remove_file(place.root.join(file)).unwrap();
    }
    place.retry_now();
    let recovered = place.tick();
    assert!(recovered["hosted"].is_object(), "{recovered}");
    until("idle recovery native process", || {
        place.root.join("pi-env").exists()
    });
    assert_eq!(place.launch_state().0, 0);
    assert_eq!(place.wakes(), 1); // only the real mail fallback; idle uses no prompt
    assert_eq!(place.tick()["skipped"], "its live first mate has the mail");
    place.stop_native("infra");
}

#[test]
fn a_hosted_first_mate_runs_live_in_its_own_herdr_space_and_falls_back_without_it() {
    let place = Place::new();
    let socket = place.root.join("config/herdr/herdr.sock");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let herdr = Herdr::start(
        &socket,
        vec![
            ("PATH".into(), place.path.clone()),
            ("HOME".into(), place.root.display().to_string()),
            (
                "XDG_CONFIG_HOME".into(),
                place.root.join("config").display().to_string(),
            ),
        ],
    );
    place.json(
        "mail.send",
        json!({"from":"captain","to":"infra","kind":"request","body":"check CI"}),
    );

    // Mail launches the first mate in a background space, not a headless wake.
    let report = place.tick();
    assert_eq!(report["hosted"]["pane"], "w2:p1", "{report}");
    assert_eq!(place.wakes(), 0);
    let create = &herdr.calls("workspace.create")[0]["params"];
    assert_eq!(create["label"], "infra");
    assert_eq!(create["focus"], false);
    assert!(
        herdr.calls("pane.send_input")[0]["params"]["text"]
            .as_str()
            .unwrap()
            .ends_with(" start --hosted")
    );

    // The live session adopts the dispatcher's lease, bound to its pane, and
    // carries on the ship's latest conversation with the mail adapter.
    until("the hosted harness", || place.root.join("pi-env").exists());
    let argv = fs::read_to_string(place.root.join("pi-argv")).unwrap();
    assert!(argv.lines().any(|a| a == "--continue"), "{argv}");
    assert!(
        argv.lines().any(|a| a.ends_with("attended/mail.ts")),
        "{argv}"
    );
    assert!(
        argv.lines().any(|a| a.ends_with("attended/calm.ts")),
        "{argv}"
    );
    let env = fs::read_to_string(place.root.join("pi-env")).unwrap();
    let (flag, holder) = env.trim().split_once(' ').unwrap();
    assert_eq!(flag, "1");
    assert!(holder.starts_with("host:"), "{env}");
    let lease = place.json("ship.presence", json!({"ship":"infra"}));
    assert_eq!(lease["lease"]["holder"], holder);
    assert_eq!(lease["lease"]["binding"]["pane"]["pane_id"], "w2:p1");

    // While it lives, more mail goes to it: no second space, no wake.
    place.json(
        "mail.send",
        json!({"from":"captain","to":"infra","kind":"request","body":"and the docs"}),
    );
    assert_eq!(place.tick()["skipped"], "its live first mate has the mail");
    assert_eq!(herdr.calls("workspace.create").len(), 1);
    assert_eq!(place.wakes(), 0);

    // Adoption resets failures, not the reserved retry deadline. Prove the
    // native payload and lease are gone before exercising headless fallback.
    place.stop_native("infra");
    let state = place.launch_state();
    assert_eq!(state.0, 0);
    assert!(state.1 > privateer::store::Store::now_ms(), "{state:?}");
    assert!(state.2.is_none(), "{state:?}");
    fs::remove_file(&socket).unwrap();
    let calls = herdr.requests.lock().unwrap().len();
    place.json(
        "mail.send",
        json!({"from":"captain","to":"infra","kind":"request","body":"still there?"}),
    );
    let report = place.tick();
    assert!(report["wake"].is_string(), "{report}");
    assert!(report["host_error"].is_null(), "{report}");
    assert_eq!(report["host_backoff_until_ms"], state.1, "{report}");
    assert_eq!(place.launch_state(), state);
    assert_eq!(herdr.requests.lock().unwrap().len(), calls);
    assert_eq!(place.wakes(), 1);
    place.stop_wake("adopted host backoff fallback", &report);

    // Only a due attempt reports a new host error. Jump the durable deadline
    // rather than deleting backoff, then prove missing Herdr still falls back
    // to a real headless subprocess without leaking the launch lease.
    place.retry_now();
    place.json(
        "mail.send",
        json!({"from":"captain","to":"infra","kind":"request","body":"retry after deadline"}),
    );
    let report = place.tick();
    assert!(report["wake"].is_string(), "{report}");
    assert!(
        report["host_error"]
            .as_str()
            .unwrap()
            .starts_with("Herdr socket unavailable:"),
        "{report}"
    );
    assert!(report["host_backoff_until_ms"].is_null(), "{report}");
    let state = place.launch_state();
    assert_eq!(state.0, 1);
    assert!(state.1 > privateer::store::Store::now_ms(), "{state:?}");
    assert_eq!(state.2.as_deref(), report["host_error"].as_str());
    let presence = place.json("ship.presence", json!({"ship":"infra"}));
    assert_eq!(presence["attended"], false, "{presence}");
    assert!(presence["lease"].is_null(), "{presence}");
    assert_eq!(herdr.requests.lock().unwrap().len(), calls);
    assert_eq!(place.wakes(), 2);
    place.stop_wake("due missing-socket fallback", &report);
}

#[test]
fn a_hosted_claude_first_mate_needs_folder_trust_and_continues_only_a_real_conversation() {
    let place = Place::new();
    // The fake records its argv like the Pi fake, so stop_native applies.
    fs::copy(place.root.join("bin/pi"), place.root.join("bin/claude")).unwrap();
    let socket = place.root.join("config/herdr/herdr.sock");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let _herdr = Herdr::start(
        &socket,
        vec![
            ("PATH".into(), place.path.clone()),
            ("HOME".into(), place.root.display().to_string()),
            (
                "XDG_CONFIG_HOME".into(),
                place.root.join("config").display().to_string(),
            ),
        ],
    );
    let mut first_mate = place.json("ship.show", json!({"name":"infra"}))["first_mate"].take();
    first_mate["manifest"] = json!("bundled:claude");
    first_mate["options"] = json!({"model":"claude-opus-5-5","permission_mode":"auto"});
    place.json("ship.put", json!({"name":"infra","first_mate":first_mate}));

    // An untrusted folder would stop at Claude's trust dialog: refuse instead.
    let error = place.tick()["host_error"].as_str().unwrap().to_owned();
    assert!(error.contains("Yes, I trust this folder"), "{error}");
    assert!(!place.root.join("pi-argv").exists());

    // Trust on a folder above is not enough: Claude asks again for a folder
    // with its own permission rules, which every ship folder has.
    let trusted = |dir: &Path| {
        let trust = json!({"projects": {dir.to_str().unwrap(): {"hasTrustDialogAccepted": true}}});
        fs::write(place.root.join(".claude.json"), trust.to_string()).unwrap();
        place.retry_now();
    };
    trusted(&place.root);
    assert!(
        place.tick()["host_error"]
            .as_str()
            .unwrap()
            .contains("Yes, I trust this folder")
    );
    trusted(&place.folder);
    place.tick();
    until("the hosted Claude", || place.root.join("pi-env").exists());
    let argv = fs::read_to_string(place.root.join("pi-argv")).unwrap();
    let args: Vec<&str> = argv.lines().collect();
    assert!(
        args.windows(2).any(|w| w == ["--permission-mode", "auto"]),
        "{argv}"
    );
    assert!(
        args.windows(2).any(|w| w == ["--model", "claude-opus-5-5"]),
        "{argv}"
    );
    assert!(
        args.iter().any(|a| a.ends_with("attended-claude")),
        "{argv}"
    );
    // With no conversation in the folder, --continue would exit at once.
    assert!(!args.contains(&"--continue"), "{argv}");
    place.stop_native("infra");

    // Claude's own naming: every character but a letter or digit is a dash.
    let folder: String = place
        .folder
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let transcripts = place.root.join(".claude/projects").join(folder);
    fs::create_dir_all(&transcripts).unwrap();
    // Headless wakes leave transcripts there too; --continue passes over them.
    fs::write(
        transcripts.join("wake.jsonl"),
        "{\"entrypoint\":\"sdk-cli\"}\n",
    )
    .unwrap();
    place.retry_now();
    place.tick();
    until("the fresh hosted Claude", || {
        place.root.join("pi-env").exists()
    });
    let argv = fs::read_to_string(place.root.join("pi-argv")).unwrap();
    assert!(!argv.lines().any(|a| a == "--continue"), "{argv}");
    place.stop_native("infra");
    fs::write(
        transcripts.join("session.jsonl"),
        "{\"entrypoint\":\"cli\"}\n",
    )
    .unwrap();
    place.retry_now();
    place.tick();
    until("the resumed hosted Claude", || {
        place.root.join("pi-env").exists()
    });
    let argv = fs::read_to_string(place.root.join("pi-argv")).unwrap();
    assert!(argv.lines().any(|a| a == "--continue"), "{argv}");
    place.stop_native("infra");
}

#[test]
fn a_hosted_ships_crew_runs_live_in_its_own_space_under_the_same_custody() {
    let place = Place::new();
    let socket = place.root.join("config/herdr/herdr.sock");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let herdr = Herdr::start(
        &socket,
        vec![
            ("PATH".into(), place.path.clone()),
            ("HOME".into(), place.root.display().to_string()),
            (
                "XDG_CONFIG_HOME".into(),
                place.root.join("config").display().to_string(),
            ),
        ],
    );
    place.json(
        "task.create",
        json!({"id":"infra-fix","parent":"infra-duty","owner":"infra","title":"fix it"}),
    );
    let started = place.agent("agent.start", json!({"id":"crew-1","task_id":"infra-fix"}));
    assert_eq!(started["hosted"], true, "{started}");

    // The supervisor runs in a background space under the task, and gives an
    // interactive harness, with the crew extension, the pane's terminal.
    let create = &herdr.calls("workspace.create")[0]["params"];
    assert_eq!(create["label"], "└ infra-fix");
    assert_eq!(create["focus"], false);
    let mut job = Value::Null;
    until("the crew to finish", || {
        job = place.json("job.status", json!({"id":"crew-1"}));
        job["status"] == "succeeded"
    });
    let argv = fs::read_to_string(place.root.join("crew-argv")).unwrap();
    let argv: Vec<&str> = argv.lines().collect();
    assert!(!argv.contains(&"--print"), "{argv:?}");
    let extension = argv.iter().position(|a| *a == "-e").unwrap();
    assert!(argv[extension + 1].ends_with("attended/crew.ts"));
    assert_eq!(argv[extension + 2], "-e");
    assert!(argv[extension + 3].ends_with("attended/calm.ts"));
    assert!(extension + 3 < argv.iter().rposition(|a| *a == "--").unwrap());

    assert_eq!(
        fs::read_to_string(place.root.join("crew-home")).unwrap(),
        place.folder.join(".privateer").to_string_lossy()
    );

    // Custody is unchanged: the outcome reaches the first mate as crew mail.
    let inbox = place.json("mail.inbox", json!({"address":"infra","all":true}));
    let outcome = inbox["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["from"] == "crew" && m["payload"]["job_id"] == "crew-1")
        .cloned()
        .unwrap();
    assert_eq!(outcome["payload"]["status"], "succeeded");
}

#[test]
fn bounded_hosted_crew_plan_retains_its_lifecycle_adapter_without_inheriting_selection() {
    let place = Place::new();
    place.json(
        "bundle.configure",
        json!({"registry":{
            "schema_version":1,"pi_version":"1.2.0",
            "integrations":{"reviewed":true,"extensions":[]},"bundles":[]
        }}),
    );
    let mut profile = place.json("ship.show", json!({"name":"infra"}))["first_mate"].take();
    profile["tool_bundles"] = json!([]);
    place.json("ship.put", json!({"name":"infra","first_mate":profile}));
    let socket = place.root.join("config/herdr/herdr.sock");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let _herdr = Herdr::start(
        &socket,
        vec![
            ("PATH".into(), place.path.clone()),
            ("HOME".into(), place.root.display().to_string()),
            (
                "XDG_CONFIG_HOME".into(),
                place.root.join("config").display().to_string(),
            ),
        ],
    );
    for task in ["legacy-crew", "bounded-crew"] {
        place.json(
            "task.create",
            json!({"id":task,"parent":"infra-duty","owner":"infra","title":task}),
        );
    }
    let legacy = place.agent(
        "agent.start",
        json!({"id":"legacy","task_id":"legacy-crew"}),
    );
    assert_eq!(legacy["hosted"], true);
    assert!(
        !legacy["command"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "--tool-bundles")
    );
    until("legacy hosted crew", || {
        place.json("job.status", json!({"id":"legacy"}))["status"] == "succeeded"
    });
    fs::remove_file(place.root.join("crew-argv")).unwrap();

    let bounded = place.agent(
        "agent.start",
        json!({"id":"bounded","task_id":"bounded-crew","tool_bundles":[]}),
    );
    assert_eq!(bounded["hosted"], true);
    let command = bounded["command"].as_array().unwrap();
    assert!(command.iter().any(|v| v == "--tool-bundles"));
    assert!(!command.iter().any(|v| v == "--print"));
    let at = command.iter().rposition(|v| v == "--").unwrap();
    assert!(
        command[..at]
            .windows(2)
            .any(|p| p[0] == "-e" && p[1].as_str().unwrap().ends_with("attended/crew.ts"))
    );
    assert!(
        command[..at]
            .windows(2)
            .any(|p| p[0] == "-e" && p[1].as_str().unwrap().ends_with("attended/calm.ts"))
    );
    // A deliberately unsupported activation wrapper cannot run a bounded
    // session, even in a real hosted supervisor. The separate native fixture
    // qualifies this same injected argv with the installed Pi 1.2 binary.
    until("bounded refusal", || {
        place.json("job.status", json!({"id":"bounded"}))["status"] == "failed"
    });
    assert_eq!(
        place.json("job.status", json!({"id":"bounded"}))["exit_code"],
        2
    );
    assert!(!place.root.join("crew-argv").exists());
}

#[test]
fn a_hosted_crew_session_ends_when_its_work_settles() {
    let output = support::command("node")
        .arg("tests/hosted-crew.mjs")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("Node is required to check the hosted crew extension");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn an_untagged_live_first_mate_is_repaired_before_its_crew_in_the_sidebar() {
    let place = Place::new();
    let socket = place.root.join("config/herdr/herdr.sock");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let herdr = Herdr::start(
        &socket,
        vec![
            ("PATH".into(), place.path.clone()),
            ("HOME".into(), place.root.display().to_string()),
            (
                "XDG_CONFIG_HOME".into(),
                place.root.join("config").display().to_string(),
            ),
        ],
    );
    // The captain's own space (w1) is first until Privateer opens its own.
    place.json(
        "mail.send",
        json!({"from":"captain","to":"infra","kind":"request","body":"check CI"}),
    );
    assert_eq!(place.tick()["hosted"]["workspace"], "w2");
    assert_eq!(herdr.order(), ["w2", "w1"]);
    until("the first mate's live workspace binding", || {
        place.json("ship.presence", json!({"ship":"infra"}))["lease"]["binding"]["pane"]["workspace_id"]
            == "w2"
    });
    let forget_mate = || {
        let mut spaces = herdr.spaces.lock().unwrap();
        spaces.iter_mut().find(|(id, _)| id == "w2").unwrap().1 = json!({});
        // Reproduce a space predating metadata, below the captain's space.
        spaces.sort_by_key(|(id, _)| id.clone());
    };
    // Unhealthy fleet homes sort before our own. Locks must not each cost a
    // busy timeout; CPU-heavy old/untrusted schemas share ONE query budget.
    // None may be opened through Store (which would create/migrate them).
    let fleet = rusqlite::Connection::open(place.root.join("fleet.sqlite")).unwrap();
    let fixtures = place.root.join("recovery-fixtures");
    fs::create_dir(&fixtures).unwrap();
    let mut locked = vec![];
    let mut originals = vec![];
    let register = |name: &str, home: &Path| {
        fleet
            .execute(
                "INSERT INTO ships(name,home,created_by,created_ms) VALUES(?1,?2,'captain',0)",
                rusqlite::params![name, home.to_str().unwrap()],
            )
            .unwrap();
    };
    for n in 0..64 {
        let name = format!("a-locked-{n:02}");
        let home = fixtures.join(&name);
        fs::create_dir(&home).unwrap();
        let path = home.join("privateer.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE old(value); BEGIN EXCLUSIVE;")
            .unwrap();
        originals.push((path.clone(), fs::read(&path).unwrap()));
        locked.push(conn);
        register(&name, &home);
    }
    let missing = fixtures.join("missing");
    register("a-missing", &missing);
    for (name, bytes) in [
        ("a-corrupt", b"not a database".as_slice()),
        ("a-unreadable", b"unreadable"),
    ] {
        let home = fixtures.join(name);
        fs::create_dir(&home).unwrap();
        let path = home.join("privateer.sqlite");
        fs::write(&path, bytes).unwrap();
        originals.push((path, bytes.to_vec()));
        register(name, &home);
    }
    let unreadable = fixtures.join("a-unreadable/privateer.sqlite");
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o0)).unwrap();
    let old = fixtures.join("a-old");
    fs::create_dir(&old).unwrap();
    let path = old.join("privateer.sqlite");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE old(value);")
        .unwrap();
    originals.push((path.clone(), fs::read(&path).unwrap()));
    register("a-old", &old);
    let alias = fixtures.join("alias");
    std::os::unix::fs::symlink(&old, &alias).unwrap();
    register("a-symlink", &alias);
    // Each view would run beyond the old per-home 500ms deadline. Even if
    // our own home is never reached, its local binding must still repair it.
    for n in 0..12 {
        let name = format!("b-slow-{n:02}");
        let home = fixtures.join(&name);
        fs::create_dir(&home).unwrap();
        let path = home.join("privateer.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE ships(name TEXT, record TEXT);
             INSERT INTO ships VALUES('{name}', '{{\"location\":\"here\",\"first_mate\":{{\"host\":\"herdr\"}}}}');
             CREATE VIEW presence AS WITH RECURSIVE spin(x) AS
               (VALUES(0) UNION ALL SELECT x+1 FROM spin WHERE x<1000000000)
             SELECT '{name}' AS ship, '{{}}' AS binding, 9223372036854775807 AS expires_ms
             FROM spin WHERE x=1000000000;"
        )).unwrap();
        drop(conn);
        originals.push((path.clone(), fs::read(&path).unwrap()));
        register(&name, &home);
    }
    forget_mate();
    let started = Instant::now();
    assert_eq!(place.tick()["skipped"], "its live first mate has the mail");
    // A generous watchdog, not a millisecond performance assertion: the old
    // cumulative lock/query waits take >10s on these same fixtures.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "fleet held up local mail/repair"
    );
    assert_eq!(herdr.order(), ["w2", "w1"]);
    assert_eq!(herdr.calls("workspace.create").len(), 1);
    assert_eq!(
        herdr.spaces.lock().unwrap()[0].1,
        json!({"privateer":"mate","ship":"infra"})
    );
    // Losing metadata again must also be repaired by a crew launch, without
    // waiting for the next dispatcher pass.
    forget_mate();
    place.json(
        "task.create",
        json!({"id":"infra-fix","parent":"infra-duty","owner":"infra","title":"fix it"}),
    );
    place.agent("agent.start", json!({"id":"crew-1","task_id":"infra-fix"}));
    assert_eq!(herdr.order(), ["w2", "w3", "w1"]);
    assert_eq!(
        herdr.spaces.lock().unwrap()[0].1,
        json!({"privateer":"mate","ship":"infra"})
    );
    // A misplaced crew must not decide ship order. Mate position wins, and
    // ships with no mate follow all mate/crew groups in ship-name order.
    {
        let mut spaces = herdr.spaces.lock().unwrap();
        spaces.insert(
            0,
            ("orphan-z".into(), json!({"privateer":"crew","ship":"z"})),
        );
        spaces.insert(
            1,
            (
                "other-crew".into(),
                json!({"privateer":"crew","ship":"other"}),
            ),
        );
        spaces.insert(
            2,
            ("orphan-a".into(), json!({"privateer":"crew","ship":"a"})),
        );
        spaces.insert(
            4,
            (
                "other-mate".into(),
                json!({"privateer":"mate","ship":"other"}),
            ),
        );
        spaces.push(("captain-2".into(), json!({})));
    }
    place.tick();
    assert_eq!(
        herdr.order(),
        [
            "w2",
            "w3",
            "other-mate",
            "other-crew",
            "orphan-a",
            "orphan-z",
            "w1",
            "captain-2"
        ]
    );
    until("the sidebar scenario's crew to finish", || {
        place.json("job.status", json!({"id":"crew-1"}))["status"] == "succeeded"
    });
    assert_eq!(herdr.calls("workspace.create").len(), 2);

    // Remove broken entries before the identity phases, checking that recovery
    // neither created missing homes nor wrote/migrated any existing database.
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(!missing.exists());
    for (path, bytes) in &originals {
        assert_eq!(
            &fs::read(path).unwrap(),
            bytes,
            "changed {}",
            path.display()
        );
        assert_eq!(
            fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1,
            "created sidecars in {}",
            path.display()
        );
    }
    fleet
        .execute("DELETE FROM ships WHERE name != 'infra'", [])
        .unwrap();
    drop(locked);

    // A plausible binding targeting the captain's w1 is not ownership when
    // its lease expired, or when ANY authoritative pane identity was reused.
    let conn =
        rusqlite::Connection::open(place.folder.join(".privateer/privateer.sqlite")).unwrap();
    let mut record = place.json("ship.show", json!({"name":"infra"}));
    record["name"] = json!("stale");
    conn.execute(
        "INSERT INTO ships(name,record) VALUES('stale',?1)",
        [record.to_string()],
    )
    .unwrap();
    let captain = herdr.panes.lock().unwrap()[0].clone();
    let mut binding =
        place.json("ship.presence", json!({"ship":"infra"}))["lease"]["binding"].clone();
    binding["pane"] = captain.clone();
    binding["pane"]["agent_session"] = json!("captain-session");
    herdr.panes.lock().unwrap()[0] = binding["pane"].clone();
    conn.execute(
        "INSERT INTO presence(ship,holder,started_ms,expires_ms,binding)
         VALUES('stale','host:stale',0,0,?1)",
        [binding.to_string()],
    )
    .unwrap();
    let assert_no_retag = |host: &Herdr| {
        let spaces = host.spaces.lock().unwrap().clone();
        let tags = host.calls("workspace.report_metadata").len();
        let moves = host.calls("workspace.move_block").len();
        place.tick();
        assert_eq!(
            *host.spaces.lock().unwrap(),
            spaces,
            "stale binding changed captain spaces"
        );
        assert_eq!(
            host.calls("workspace.report_metadata").len(),
            tags,
            "stale binding retagged a workspace"
        );
        assert_eq!(
            host.calls("workspace.move_block").len(),
            moves,
            "stale binding reordered workspaces"
        );
    };
    assert_no_retag(&herdr); // expired, otherwise exact binding
    conn.execute(
        "UPDATE presence SET expires_ms=9223372036854775807 WHERE ship='stale'",
        [],
    )
    .unwrap();
    for field in [
        "terminal_id",
        "pane_id",
        "workspace_id",
        "tab_id",
        "agent_session",
    ] {
        let mut reused = binding["pane"].clone();
        reused[field] = json!(format!("replacement-{field}"));
        herdr.panes.lock().unwrap()[0] = reused;
        assert_no_retag(&herdr);
    }
    herdr.panes.lock().unwrap()[0] = binding["pane"].clone();

    // Same path and exact pane/terminal/session, different actual socket inode.
    // Retain the old socket so inode reuse cannot make this a false positive.
    fs::rename(&socket, socket.with_extension("old")).unwrap();
    let replacement = Herdr::start(&socket, vec![]);
    *replacement.spaces.lock().unwrap() = herdr.spaces.lock().unwrap().clone();
    *replacement.panes.lock().unwrap() = herdr.panes.lock().unwrap().clone();
    assert_no_retag(&replacement);
    assert!(replacement.calls("workspace.create").is_empty());
    assert_eq!(herdr.calls("workspace.create").len(), 2);
    conn.execute("DELETE FROM presence WHERE ship='stale'", [])
        .unwrap();

    fs::write(place.root.join("release"), "").unwrap();
    // Do not delete the temporary release signal before the fake first mate
    // observes it: that leaves its polling shell alive after a green test.
    until("the hosted first mate to release its lease", || {
        place.json("ship.presence", json!({"ship":"infra"}))["attended"] == false
    });
}
