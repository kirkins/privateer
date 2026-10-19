//! Hosted first mates: the dispatcher launches a ship's first mate as a live
//! session in its own Herdr space. A fake Herdr runs what the dispatcher types
//! into the new pane, so the real `privateer start --hosted` and its harness
//! (a fake `pi` on PATH) run as subprocesses.
#[path = "hosted/errand_labels.rs"]
mod errand_labels;
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

fn until(what: &str, done: impl FnMut() -> bool) {
    until_secs(what, 20, done);
}

fn until_secs(what: &str, secs: u64, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
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
                "#!/bin/sh\ncase \"$PRIVATEER_JOB\" in errand-*) printf '%s' \"$$\" > '{root}/'$PRIVATEER_JOB'-pid'; while [ ! -e '{root}/'$PRIVATEER_JOB'-release' ] && [ -d '{root}' ]; do sleep 0.05; done; printf '%s\\n' '{{\"type\":\"session\",\"id\":\"22222222-2222-4222-8222-222222222222\"}}' '{{\"type\":\"message_end\",\"message\":{{\"role\":\"assistant\",\"stopReason\":\"stop\",\"content\":[{{\"type\":\"text\",\"text\":\"Reviewed fixture\"}}]}}}}' '{{\"type\":\"agent_settled\",\"aborted\":false}}'; exit 0;; esac\nif [ \"$PRIVATEER_HOSTED_CREW\" = 1 ]; then printf '%s\\n' \"$@\" > '{root}/crew-argv'; printf '%s' \"$PRIVATEER_HOME\" > '{root}/crew-home'; exit 0; fi\n\
                 printf '%s\\n' \"$@\" > '{root}/pi-argv'\n\
                 printf '%s\\n' \"$$\" > '{root}/pi-pid'\n\
                 printf '%s\\n' \"$PRIVATEER_HOME\" \"$PRIVATEER_CONTROL_CLI\" \"$HOME\" \"$FIXTURE_AUTH\" > '{root}/pi-context'\n\
                 case \" $* \" in *\" --continue \"*) sid=$(cat '{root}/pi-session' 2>/dev/null) ;; *) sid= ;; esac\n\
                 prev=; for arg in \"$@\"; do [ \"$prev\" = --session ] && sid=$arg; prev=$arg; done\n\
                 [ -n \"$sid\" ] || sid=$(printf '%08x-0000-4000-8000-%012x' $$ $$)\n\
                 printf '%s' \"$sid\" > '{root}/pi-session'\n\
                 printf '%s\\n' \"$sid\" >> '{root}/pi-sessions'\n\
                 printf '%s %s\\n' \"$PRIVATEER_HOSTED\" \"$PRIVATEER_PRESENCE_HOLDER\" > '{root}/pi-env'\n\
                 if [ -e '{root}/native-fail' ]; then exit 17; fi\n\
                 while [ ! -e '{root}/release' ]; do [ -d '{root}' ] || exit 1; sleep 0.05; done\n\
                 printf '%s\\n' '{{\"type\":\"session\",\"id\":\"'\"$sid\"'\"}}' '{{\"type\":\"agent_start\"}}' '{{\"type\":\"message_end\",\"message\":{{\"role\":\"assistant\",\"stopReason\":\"stop\",\"usage\":{{\"input\":1,\"output\":1}}}}}}' '{{\"type\":\"agent_settled\",\"aborted\":false}}'\n",
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
        // Crew memory is admission's concern, tested apart: these crews run on
        // whatever memory the test machine has.
        place.json(
            "resource.configure",
            json!({"min_available_memory_bytes":0,"crew_default_memory_bytes":1u64 << 20}),
        );
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
            .env_remove("PRIVATEER_ACTOR")
            .env_remove("CLAUDE_CONFIG_DIR");
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

/// Each pane's process terminal master, and whether that process still runs.
type PaneTerminals = Arc<Mutex<std::collections::HashMap<String, (fs::File, Arc<AtomicBool>)>>>;

/// A pane's process terminal: its stdin is the slave, as a Herdr pane's is,
/// and the master is what Herdr types into.
fn pane_terminal() -> (fs::File, fs::File) {
    use std::os::fd::{AsRawFd, FromRawFd};
    let (mut master, mut slave) = (-1, -1);
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    let master = unsafe { fs::File::from_raw_fd(master) };
    let slave = unsafe { fs::File::from_raw_fd(slave) };
    for f in [&master, &slave] {
        assert_eq!(
            unsafe { libc::fcntl(f.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
    }
    (master, slave)
}

/// Run a pane's command with the pane's terminal as its stdin, and record the
/// terminal so text sent to the pane while the command runs is typed into it.
fn spawn_in_pane(terminals: &PaneTerminals, pane: &str, mut shell: Command) {
    let (master, slave) = pane_terminal();
    let alive = Arc::new(AtomicBool::new(true));
    let mut child = shell.stdin(slave).spawn().unwrap();
    let mut output = master.try_clone().unwrap();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut block = [0; 1024];
        while matches!(output.read(&mut block), Ok(n) if n > 0) {}
    });
    terminals
        .lock()
        .unwrap()
        .insert(pane.to_owned(), (master, alive.clone()));
    std::thread::spawn(move || {
        let _ = child.wait();
        alive.store(false, Ordering::SeqCst);
    });
}

/// A Herdr that answers one request per connection, keeps its spaces in
/// sidebar order (starting with one of the captain's own), and runs the text
/// sent to a space it created with that pane's Herdr environment. Text sent
/// to a pane whose process is still running is typed into that process's
/// terminal instead, as Herdr types into a pane.
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
    reject_caption: Arc<AtomicBool>,
    /// The pane the captain's client has focused.
    focus: Arc<Mutex<Option<String>>>,
    /// Herdr answers ok to reports and drops them.
    drop_reports: Arc<AtomicBool>,
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
        let reject_caption = Arc::new(AtomicBool::new(false));
        let focus = Arc::new(Mutex::new(None::<String>));
        let focused = focus.clone();
        let drop_reports = Arc::new(AtomicBool::new(false));
        let dropping = drop_reports.clone();
        // Each pane's process terminal master, and whether that process still runs.
        let terminals: PaneTerminals = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let reject_label = reject_caption.clone();
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
                if request["method"] == "pane.report_metadata"
                    && params["source"] == "privateer-errands"
                    && reject_label.load(Ordering::SeqCst)
                {
                    let _ = writeln!(
                        stream,
                        "{}",
                        json!({"id":request["id"],"error":"fixture metadata refusal"})
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
                        json!({"snapshot":{"version":"test","protocol":20,"panes":*live.lock().unwrap(),
                            "focused_pane_id":*focused.lock().unwrap()}})
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
                        let text = params["text"].as_str().unwrap();
                        if let Some((master, alive)) = terminals.lock().unwrap().get_mut(id) {
                            if alive.load(Ordering::SeqCst) {
                                writeln!(master, "{text}").unwrap();
                                let _ = writeln!(
                                    stream,
                                    "{}",
                                    json!({"id":request["id"],"result":{"type":"ok"}})
                                );
                                seen.lock().unwrap().push(request);
                                continue;
                            }
                        }
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
                            spawn_in_pane(&terminals, id, shell);
                        }
                        #[cfg(not(feature = "dashboard"))]
                        spawn_in_pane(&terminals, id, shell);
                        json!({"type":"ok"})
                    }
                    "pane.report_agent" => {
                        // Herdr drops a report naming the agent it recorded as exited in this
                        // pane, and answers ok; any other report takes the pane.
                        let mut live = live.lock().unwrap();
                        if let Some(pane) =
                            live.iter_mut().find(|p| p["pane_id"] == params["pane_id"])
                        {
                            if !dropping.load(Ordering::SeqCst)
                                && pane["exited_agent"] != params["agent"]
                            {
                                pane["agent"] = params["agent"].clone();
                                pane["agent_source"] = params["source"].clone();
                                pane["agent_status"] = params["state"].clone();
                                pane["exited_agent"] = Value::Null;
                            }
                        }
                        json!({"type":"ok"})
                    }
                    "pane.report_metadata" => {
                        let mut live = live.lock().unwrap();
                        if let Some(pane) =
                            live.iter_mut().find(|p| p["pane_id"] == params["pane_id"])
                        {
                            // A guard names the agent the label must still be.
                            // Errand captions keep their own source and are not
                            // modelled here.
                            let guarded = params.get("agent").is_some_and(|a| *a != pane["agent"]);
                            if params["source"] == "privateer"
                                && !dropping.load(Ordering::SeqCst)
                                && !guarded
                            {
                                if let Some(display) = params.get("display_agent") {
                                    pane["display_agent"] = display.clone();
                                }
                                if params.get("clear_display_agent") == Some(&json!(true)) {
                                    pane["display_agent"] = Value::Null;
                                }
                            }
                        }
                        json!({"type":"ok"})
                    }
                    "pane.release_agent" => {
                        let mut live = live.lock().unwrap();
                        if let Some(pane) =
                            live.iter_mut().find(|p| p["pane_id"] == params["pane_id"])
                        {
                            if pane["agent"] == params["agent"]
                                && pane["agent_source"] == params["source"]
                            {
                                pane["agent"] = Value::Null;
                                pane["agent_source"] = Value::Null;
                                pane["agent_status"] = json!("unknown");
                                pane["display_agent"] = Value::Null;
                            }
                        }
                        json!({"type":"ok"})
                    }
                    "pane.read" => {
                        json!({"type":"pane_read","read":{"pane_id":params["pane_id"],
                            "workspace_id":"w2","tab_id":"w2:t1","source":params["source"],
                            "format":params["format"],"revision":7,"truncated":false,
                            "text":"\u{1b}[1mpi\u{1b}[0m ready"}})
                    }
                    "pane.process_info" => {
                        let live = live.lock().unwrap();
                        let pane = live.iter().find(|p| p["pane_id"] == params["pane_id"]);
                        json!({"type":"pane_process_info",
                            "process_info": pane.map(|p| p["process_info"].clone())})
                    }
                    "workspace.close" => {
                        live.lock()
                            .unwrap()
                            .retain(|p| p["workspace_id"] != params["workspace_id"]);
                        order
                            .lock()
                            .unwrap()
                            .retain(|(id, _)| *id != params["workspace_id"]);
                        json!({"type":"ok"})
                    }
                    "pane.get" => {
                        let live = live.lock().unwrap();
                        let pane = live.iter().find(|p| p["pane_id"] == params["pane_id"]);
                        json!({"pane": pane.cloned().unwrap_or(Value::Null)})
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
            reject_caption,
            focus,
            drop_reports,
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
    fn status(&self, workspace_id: &str, status: &str) {
        let mut panes = self.panes.lock().unwrap();
        let pane = panes
            .iter_mut()
            .find(|pane| pane["workspace_id"] == workspace_id)
            .expect("hosted pane exists");
        pane["agent_status"] = json!(status);
    }
    /// Herdr has recorded the harness in this pane as exited, so it drops a
    /// report naming that agent until a new agent process is detected.
    fn exit_agent(&self, pane_id: &str, agent: &str) {
        let mut panes = self.panes.lock().unwrap();
        let pane = panes
            .iter_mut()
            .find(|p| p["pane_id"] == pane_id)
            .expect("hosted pane exists");
        pane["agent"] = Value::Null;
        pane["agent_status"] = json!("unknown");
        pane["display_agent"] = Value::Null;
        pane["exited_agent"] = json!(agent);
    }
    fn pane_state(&self, pane_id: &str) -> Value {
        let panes = self.panes.lock().unwrap();
        panes
            .iter()
            .find(|p| p["pane_id"] == pane_id)
            .cloned()
            .unwrap()
    }
    /// Herdr restarted and restored this space: the same pane under a new
    /// terminal, running a shell in `cwd`, with `command` in front if given.
    fn restore(&self, workspace_id: &str, cwd: &Path, command: Option<&str>) {
        let mut panes = self.panes.lock().unwrap();
        let pane = panes
            .iter_mut()
            .find(|p| p["workspace_id"] == workspace_id)
            .expect("restored pane exists");
        pane["terminal_id"] = json!(format!("{workspace_id}-restored"));
        pane["agent"] = Value::Null;
        let mut processes = vec![json!({"pid":100,"name":"bash","cwd":cwd})];
        if let Some(command) = command {
            processes = vec![json!({"pid":101,"name":command,"cwd":cwd})];
        }
        pane["process_info"] = json!({"pane_id":pane["pane_id"],"shell_pid":100,
            "foreground_process_group_id":processes[0]["pid"],"foreground_processes":processes});
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

fn hosted_fixture() -> (Place, Herdr) {
    hosted(Place::new())
}

fn hosted(place: Place) -> (Place, Herdr) {
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
        json!({"from":"captain","to":"infra","kind":"request","body":"start hosted mate"}),
    );
    assert_eq!(place.tick()["hosted"]["pane"], "w2:p1");
    until("the hosted harness", || place.root.join("pi-env").exists());
    (place, herdr)
}

fn captain_blocked_alerts(place: &Place) -> Vec<Value> {
    place.json(
        "mail.inbox",
        json!({"address":"captain","all":true,"limit":100}),
    )["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["payload"]["hosted_blocked"] == "infra")
        .cloned()
        .collect()
}

fn age_hosted_block(place: &Place) {
    place
        .database()
        .execute(
            "UPDATE hosted_block_alerts SET blocked_since_ms=?1 WHERE ship='infra'",
            [privateer::store::Store::now_ms() - 5 * 60 * 1000 - 1],
        )
        .unwrap();
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
    // A reboot ends every lease, and btrfs/APFS may renumber the device under
    // unchanged inodes. Such a claim is stale, but only a free lease retires
    // it: the dashboard holding it on this boot keeps custody.
    let held = claim();
    let write = |c: &Value| fs::write(&claim_path, c.to_string()).unwrap();
    let mut earlier = held.clone();
    earlier["owner"]["boot"] = json!("00000000-0000-0000-0000-000000000000");
    let mut moved = held.clone();
    for name in ["registry", "guard", "live"] {
        moved[name][0] = json!(moved[name][0].as_u64().unwrap().wrapping_add(1));
    }
    let mut legacy = moved.clone();
    legacy["owner"].as_object_mut().unwrap().remove("boot");
    for stale in [&earlier, &legacy] {
        write(stale);
        let error = tick(&apps)["bridge"]["error"].clone();
        assert!(error.as_str().unwrap().contains("retained"), "{error}");
        assert_eq!(herdr.calls("workspace.create").len(), 3);
    }
    // A live claim from before boots were recorded learns this boot.
    let mut unstamped = held.clone();
    unstamped["owner"].as_object_mut().unwrap().remove("boot");
    write(&unstamped);
    assert_eq!(tick(&apps)["bridge"]["status"], "live");
    assert_eq!(claim()["owner"]["boot"], held["owner"]["boot"]);
    assert!(held["owner"]["boot"].is_string());
    herdr.dashboards.lock().unwrap()[1].stop();
    // Within one boot a renumbered device is still a replaced scope.
    write(&moved);
    let error = tick(&apps)["bridge"]["error"].clone();
    assert_eq!(error, "bridge claim scope changed");
    assert_eq!(herdr.calls("workspace.create").len(), 3);
    // Once the lease is free, each stale claim is taken over, not wedged.
    herdr.suppress_input.store(true, Ordering::SeqCst);
    for (stale, creates) in [(&earlier, 4), (&legacy, 5)] {
        write(stale);
        assert_eq!(tick(&apps)["bridge"]["status"], "launched");
        assert_eq!(herdr.calls("workspace.create").len(), creates);
        let now = claim();
        assert_ne!(now["token"], held["token"]);
        assert_eq!(now["owner"]["boot"], held["owner"]["boot"]);
        assert_eq!(now["live"], held["live"]);
        assert_eq!(now["adopted"], false);
    }
    herdr.suppress_input.store(false, Ordering::SeqCst);
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
    // The wrapper records the start once its spawn returns, which can be just
    // after the native process has written pi-env; until then the launch's own
    // pending count (the native failure's 1, plus this attempt) stands.
    until("idle recovery recorded as started", || {
        place.launch_state().0 == 0
    });
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
fn a_space_herdr_restores_after_a_restart_is_closed_unless_the_captain_uses_it() {
    let (place, herdr) = hosted_fixture();
    // Herdr restarts: the session it ran is gone, and it restores the space
    // with a shell the captain has started something in. That one is theirs.
    place.stop_native("infra");
    herdr.restore("w2", &place.folder, Some("vim"));
    place.retry_now();
    let report = place.tick();
    assert_eq!(report["hosted"]["pane"], "w3:p1", "{report}");
    assert!(herdr.calls("workspace.close").is_empty());
    assert!(herdr.order().contains(&"w2".to_owned()));

    // A restored copy at its bare shell is closed, and a fresh space opened.
    until("the second hosted harness", || {
        place.root.join("pi-env").exists()
    });
    place.stop_native("infra");
    herdr.restore("w3", &place.folder, None);
    place.retry_now();
    let report = place.tick();
    assert_eq!(report["hosted"]["pane"], "w4:p1", "{report}");
    let closed = herdr.calls("workspace.close");
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(closed[0]["params"]["workspace_id"], "w3");
    let mut left = herdr.order();
    left.sort();
    assert_eq!(left, ["w1", "w2", "w4"]);
}

/// Idle the hosted first mate past its sleep threshold until it sleeps: its
/// adapter reports it idle, and its native payload exits. Herdr has recorded
/// the harness as exited by then, which is when it drops a report naming it.
fn sleep_hosted(place: &Place, herdr: &Herdr) -> String {
    let pid = place.native_pid();
    let env = fs::read_to_string(place.root.join("pi-env")).unwrap();
    let holder = env.trim().split_once(' ').unwrap().1.to_owned();
    let idle = json!({"ship":"infra","holder":holder,"idle":true});
    place.agent("work.poll", idle.clone());
    std::thread::sleep(Duration::from_millis(1100));
    herdr.exit_agent("w2:p1", "pi");
    place.agent("work.poll", idle);
    until(
        "the idle native payload to exit",
        || unsafe { libc::kill(pid, 0) } == -1,
    );
    for file in ["pi-pid", "pi-env", "pi-argv"] {
        fs::remove_file(place.root.join(file)).unwrap();
    }
    holder
}

/// The hosted first mate "infra", set to sleep after one second, in its pane.
fn hosted_sleeper() -> (Place, Herdr) {
    let place = Place::new();
    let mut first_mate = place.json("ship.show", json!({"name":"infra"}))["first_mate"].take();
    first_mate["sleep_after_secs"] = json!(1);
    place.json("ship.put", json!({"name":"infra","first_mate":first_mate}));
    hosted(place)
}

/// Audit rows of reports Herdr answered ok but did not take.
fn ignored_reports(place: &Place) -> Vec<Value> {
    place.json("audit.list", json!({"limit":1000}))["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "host.report_ignored")
        .map(|event| event["payload"].clone())
        .collect()
}

#[test]
fn the_captain_reads_and_types_into_a_hosted_first_mates_session() {
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
    assert_eq!(place.tick()["hosted"]["pane"], "w2:p1");
    until("the hosted session's binding", || {
        place.json("ship.presence", json!({"ship":"infra"}))["lease"]["binding"]["pane"]["pane_id"]
            == "w2:p1"
    });

    // The screen, with its colours and revision, from the ship's own pane.
    let read = place.json("session.read", json!({"ship":"infra","lines":50}));
    assert_eq!(read["pane_id"], "w2:p1", "{read}");
    assert_eq!(read["revision"], 7);
    assert_eq!(read["text"], "\u{1b}[1mpi\u{1b}[0m ready");
    let asked = &herdr.calls("pane.read")[0]["params"];
    assert_eq!(
        (asked["format"].as_str(), asked["lines"].as_u64()),
        (Some("ansi"), Some(50))
    );

    // Typing goes into that pane, and is audited without the text.
    place.json(
        "session.send",
        json!({"ship":"infra","text":"how is CI?","keys":["Enter"]}),
    );
    let typed = herdr.calls("pane.send_input").last().unwrap()["params"].clone();
    assert_eq!(typed["pane_id"], "w2:p1");
    assert_eq!(typed["text"], "how is CI?");
    assert_eq!(typed["keys"], json!(["Enter"]));

    // Nothing to send, or a ship with no live session, is refused.
    for args in [json!({"ship":"infra"}), json!({"ship":"nobody","text":"x"})] {
        let refused = place
            .command(&["session.send", "--json", &args.to_string()])
            .output()
            .unwrap();
        assert!(!refused.status.success(), "{args}");
    }
}

#[test]
fn an_idle_hosted_first_mate_sleeps_in_its_pane_and_mail_or_focus_wakes_it() {
    let (place, herdr) = hosted_sleeper();
    let holder = sleep_hosted(&place, &herdr);

    // Asleep, the pane keeps the ship's hosted lease under a new holder, so
    // the dispatcher neither opens another space nor wakes it headless, and
    // it keeps the ship in Herdr's agent list, asleep.
    let lease = place.json("ship.presence", json!({"ship":"infra"}));
    let asleep = lease["lease"]["holder"].as_str().unwrap().to_owned();
    assert!(asleep.starts_with("host:") && asleep != holder, "{lease}");
    assert_eq!(place.tick()["skipped"], "its live first mate has the mail");
    assert_eq!(herdr.calls("workspace.create").len(), 1);
    assert_eq!(place.wakes(), 0);
    until("the asleep report", || {
        herdr.calls("pane.report_agent").iter().any(|call| {
            call["params"]["pane_id"] == "w2:p1"
                && call["params"]["state"] == "idle"
                && call["params"]["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("asleep"))
        })
    });
    // Herdr lists the pane under the mate's own label, not the harness's
    // (which it dropped), with the ship as its name.
    until("the pane listed as asleep", || {
        let pane = herdr.pane_state("w2:p1");
        pane["agent"] == "privateer"
            && pane["display_agent"] == "infra"
            && pane["agent_status"] == "idle"
    });
    assert!(ignored_reports(&place).is_empty());

    // Mail wakes it in the same pane, on the same conversation, as the
    // session the lease now names, and hands the pane back to detection.
    place.json(
        "mail.send",
        json!({"from":"captain","to":"infra","kind":"request","body":"wake up"}),
    );
    until("the woken harness", || place.root.join("pi-env").exists());
    let env = fs::read_to_string(place.root.join("pi-env")).unwrap();
    assert_eq!(env.trim(), format!("1 {asleep}"));
    let argv = fs::read_to_string(place.root.join("pi-argv")).unwrap();
    assert!(argv.lines().any(|a| a == "--continue"), "{argv}");
    assert!(!herdr.calls("pane.release_agent").is_empty());
    // Released before the harness started: the pane no longer carries the label.
    let pane = herdr.pane_state("w2:p1");
    assert!(
        pane["agent"].is_null() && pane["display_agent"].is_null(),
        "{pane}"
    );
    assert_eq!(herdr.calls("workspace.create").len(), 1);
    assert_eq!(place.wakes(), 0);

    // Asleep again, the captain moving to its pane wakes it.
    *herdr.focus.lock().unwrap() = Some("w1:p1".into());
    sleep_hosted(&place, &herdr);
    until("the asleep report", || {
        herdr.calls("pane.report_agent").len() >= 2
    });
    *herdr.focus.lock().unwrap() = Some("w2:p1".into());
    until("the harness woken by focus", || {
        place.root.join("pi-env").exists()
    });
    assert_eq!(herdr.calls("workspace.create").len(), 1);
    place.stop_native("infra");
}

/// A sleeping hosted first mate with an open, unscheduled task is woken by
/// one backlog notice once its hour has passed, on the same conversation, and
/// not again within the hour. The wake is recorded against the hourly budget.
#[test]
fn a_sleeping_hosted_first_mate_with_an_unscheduled_task_is_woken_once_for_its_backlog() {
    let (place, herdr) = hosted_sleeper();
    place.json(
        "task.create",
        json!({"id":"infra-draft","parent":"infra-duty","owner":"infra","title":"Draft the plan"}),
    );
    sleep_hosted(&place, &herdr);
    let serve = || -> Value {
        serde_json::from_str(&place.ok(&["serve", "--json", r#"{"once":true}"#])).unwrap()
    };
    let backlog_notices = || -> i64 {
        place
            .database()
            .query_row(
                "SELECT COUNT(*) FROM mail WHERE recipient='infra' AND correlation='backlog-infra'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    // The alarm's first check was at the launch; its hour has not passed.
    let early = serve();
    assert_eq!(early["backlog"][0]["skipped"], "spaced", "{early}");
    assert!(!place.root.join("pi-env").exists());
    place
        .database()
        .execute(
            "UPDATE backlog_alarms SET fired_ms=fired_ms-3600001, next_ms=next_ms-3600001",
            [],
        )
        .unwrap();
    let report = serve();
    assert_eq!(report["backlog"][0]["fired"], "backlog-infra-1", "{report}");
    assert_eq!(report["backlog"][0]["considered"], json!(["infra-draft"]));
    until("the harness woken by the backlog notice", || {
        place.root.join("pi-env").exists()
    });
    let argv = fs::read_to_string(place.root.join("pi-argv")).unwrap();
    assert!(argv.lines().any(|a| a == "--continue"), "{argv}");
    until("the backlog wake audited", || {
        place.json("audit.list", json!({"limit":1000}))["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "host.woke" && e["payload"]["reason"] == "backlog")
    });
    let wakes: Vec<(String, String, String)> = {
        let db = place.database();
        let mut statement = db
            .prepare("SELECT reason, status, focus FROM wakes WHERE ship='infra'")
            .unwrap();
        statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(
        wakes,
        [(
            "backlog".to_owned(),
            "succeeded".to_owned(),
            "thread:backlog-infra".to_owned()
        )]
    );
    // Asleep again with the task untouched: no second notice this hour.
    sleep_hosted(&place, &herdr);
    let again = serve();
    assert_eq!(again["backlog"][0]["skipped"], "spaced", "{again}");
    assert_eq!(backlog_notices(), 1);
    assert_eq!(herdr.calls("workspace.create").len(), 1);
    place.json(
        "mail.send",
        json!({"from":"captain","to":"infra","kind":"request","body":"wake up"}),
    );
    until("the woken harness", || place.root.join("pi-env").exists());
    place.stop_native("infra");
}

/// Type `text` into a pane, as Herdr does for a captain typing in it.
fn type_into_pane(place: &Place, pane: &str, text: &str) {
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(place.root.join("config/herdr/herdr.sock")).unwrap();
    let request = json!({"id":1,"method":"pane.send_input","params":{"pane_id":pane,"text":text}});
    writeln!(stream, "{request}").unwrap();
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&reply).unwrap()["result"]["type"],
        "ok"
    );
}

#[test]
fn text_typed_into_a_sleeping_hosted_first_mate_wakes_it_as_mail_from_the_captain() {
    let (place, herdr) = hosted_sleeper();
    sleep_hosted(&place, &herdr);
    until("the pane listed as asleep", || {
        herdr.pane_state("w2:p1")["display_agent"] == "infra"
    });

    type_into_pane(&place, "w2:p1", "typed wake");
    until("the harness woken by typing", || {
        place.root.join("pi-env").exists()
    });
    let argv = fs::read_to_string(place.root.join("pi-argv")).unwrap();
    assert!(argv.lines().any(|a| a == "--continue"), "{argv}");
    assert_eq!(herdr.calls("workspace.create").len(), 1);
    assert_eq!(place.wakes(), 0);
    // The text reached the mailbox as the captain's own mail, not a shell command.
    let mail = place.json(
        "mail.inbox",
        json!({"address":"infra","all":true,"limit":100}),
    )["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["from"] == "captain" && m["body"] == "typed wake")
        .count();
    assert_eq!(mail, 1);
    place.stop_native("infra");
}

#[test]
fn a_sleep_report_herdr_drops_is_warned_about_and_audited_once() {
    let (place, herdr) = hosted_sleeper();
    herdr.drop_reports.store(true, Ordering::SeqCst);
    sleep_hosted(&place, &herdr);
    until("the audit of the dropped report", || {
        !ignored_reports(&place).is_empty()
    });
    // Herdr keeps answering ok: a second sleep report cycle, and its read-back,
    // come and go without another row.
    let reports = |herdr: &Herdr| {
        herdr
            .calls("pane.report_agent")
            .iter()
            .filter(|call| call["params"]["state"] == "idle")
            .count()
    };
    let before = reports(&herdr);
    until_secs("a second sleep report", 40, || reports(&herdr) > before);
    std::thread::sleep(Duration::from_secs(6));
    let rows = ignored_reports(&place);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["ship"], "infra");
    assert_eq!(rows[0]["step"], "report");
    assert_eq!(rows[0]["sent"]["agent"], "privateer");
    assert_eq!(rows[0]["sent"]["display_agent"], "infra");
    assert!(rows[0]["read"]["agent"].is_null(), "{rows:?}");
    assert!(herdr.pane_state("w2:p1")["display_agent"].is_null());

    // Herdr accepts reports again; mail still wakes the mate.
    herdr.drop_reports.store(false, Ordering::SeqCst);
    place.json(
        "mail.send",
        json!({"from":"captain","to":"infra","kind":"request","body":"wake up"}),
    );
    until("the woken harness", || place.root.join("pi-env").exists());
    assert_eq!(ignored_reports(&place).len(), 1);
    place.stop_native("infra");
}

#[test]
fn hosted_blocked_alerts_once_after_the_threshold() {
    let (place, herdr) = hosted_fixture();
    herdr.status("w2", "blocked");
    place.tick();
    assert!(captain_blocked_alerts(&place).is_empty());
    age_hosted_block(&place);
    place.tick();
    let alerts = captain_blocked_alerts(&place);
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0]["to"], "captain");
    assert_eq!(alerts[0]["from"], "crew");
    assert_eq!(alerts[0]["urgency"], "normal");
    assert_eq!(alerts[0]["task_id"], "infra-duty");
    assert!(
        alerts[0]["body"]
            .as_str()
            .unwrap()
            .contains("waiting for input in Herdr")
    );
    place.tick();
    assert_eq!(captain_blocked_alerts(&place).len(), 1);
    place.stop_native("infra");
}

#[test]
fn hosted_blocked_alerts_again_after_clear_and_reblock() {
    let (place, herdr) = hosted_fixture();
    herdr.status("w2", "blocked");
    place.tick();
    age_hosted_block(&place);
    place.tick();
    assert_eq!(captain_blocked_alerts(&place).len(), 1);
    herdr.status("w2", "idle");
    place.tick();
    herdr.status("w2", "blocked");
    place.tick();
    assert_eq!(captain_blocked_alerts(&place).len(), 1);
    age_hosted_block(&place);
    place.tick();
    assert_eq!(captain_blocked_alerts(&place).len(), 2);
    place.stop_native("infra");
}

#[test]
fn hosted_blocked_alerts_never_for_an_idle_pane() {
    let (place, herdr) = hosted_fixture();
    herdr.status("w2", "idle");
    place.tick();
    assert!(captain_blocked_alerts(&place).is_empty());
    assert_eq!(
        place
            .database()
            .query_row(
                "SELECT COUNT(*) FROM hosted_block_alerts WHERE ship='infra'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0,
    );
    place.stop_native("infra");
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
    // A hosted first mate stays out of the captain's browser unless its
    // options ask for it.
    assert!(args.contains(&"--no-chrome"), "{argv}");
    assert!(!args.contains(&"--chrome"), "{argv}");
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
fn a_hosted_claude_first_mate_resumes_past_a_long_preamble_and_records_why() {
    let place = Place::new();
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
    let trust =
        json!({"projects": {place.folder.to_str().unwrap(): {"hasTrustDialogAccepted": true}}});
    fs::write(place.root.join(".claude.json"), trust.to_string()).unwrap();

    // Starts `claude` afresh and returns the argv it was given.
    let launch = |place: &Place| -> Vec<String> {
        let _ = fs::remove_file(place.root.join("pi-argv"));
        let _ = fs::remove_file(place.root.join("pi-env"));
        place.retry_now();
        place.tick();
        until("the hosted Claude", || place.root.join("pi-env").exists());
        let argv = fs::read_to_string(place.root.join("pi-argv")).unwrap();
        place.stop_native("infra");
        argv.lines().map(str::to_owned).collect()
    };
    // The last host.launched audit: whether its launch resumed a conversation.
    let launched = |place: &Place| -> Value {
        let payload: String = place
            .database()
            .query_row(
                "SELECT payload FROM audit WHERE kind='host.launched' ORDER BY rowid DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        serde_json::from_str(&payload).unwrap()
    };

    // No transcript at all: fresh, and the audit says so.
    let args = launch(&place);
    assert!(!args.iter().any(|a| a == "--continue"), "{args:?}");
    let audit = launched(&place);
    assert_eq!(audit["continued"], json!(false), "{audit}");
    assert_eq!(audit["continuation"], json!("no transcript"), "{audit}");

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
    fs::write(
        transcripts.join("wake.jsonl"),
        "{\"entrypoint\":\"sdk-cli\"}\n",
    )
    .unwrap();
    let args = launch(&place);
    assert!(!args.iter().any(|a| a == "--continue"), "{args:?}");
    let audit = launched(&place);
    assert_eq!(audit["continued"], json!(false), "{audit}");
    assert_eq!(audit["continuation"], json!("headless only"), "{audit}");

    // Claude's record of the session opens with 110 KB of records that carry
    // no entrypoint before the interactive one; the decision must still see it.
    let preamble = format!(
        "{{\"type\":\"api-request-blob\",\"blob\":\"{}\"}}\n",
        "x".repeat(110 * 1024)
    );
    fs::write(
        transcripts.join("session.jsonl"),
        format!("{preamble}{{\"entrypoint\":\"cli\"}}\n"),
    )
    .unwrap();
    let args = launch(&place);
    assert!(args.iter().any(|a| a == "--continue"), "{args:?}");
    let audit = launched(&place);
    assert_eq!(audit["continued"], json!(true), "{audit}");
    assert_eq!(audit["continuation"], json!("interactive"), "{audit}");
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
fn a_hosted_ships_crew_on_another_harness_runs_headless_with_a_viewer_space() {
    let place = Place::new();
    // A headless Codex crew: one reply, then a completed turn.
    fs::write(
        place.root.join("bin/codex"),
        "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"thread.started\",\"thread_id\":\"t1\"}' \
         '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"cache fixed\"}}'\n\
         sleep 1\nprintf '%s\\n' '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'\n",
    )
    .unwrap();
    fs::set_permissions(
        place.root.join("bin/codex"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
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
    let started = place.agent(
        "agent.start",
        json!({"id":"crew-2","task_id":"infra-fix","manifest":"bundled:codex","options":{"model":"m"}}),
    );
    // Codex cannot be hosted, so the crew runs headless and a space follows it.
    assert!(started["hosted"].is_null(), "{started}");
    assert_eq!(started["viewer"], true, "{started}");
    let create = &herdr.calls("workspace.create")[0]["params"];
    assert_eq!(create["label"], "└ infra-fix");
    assert_eq!(create["focus"], false);
    assert!(
        herdr.calls("pane.send_input")[0]["params"]["text"]
            .as_str()
            .unwrap()
            .ends_with(" watch --job crew-2")
    );
    until("the crew to finish", || {
        place.json("job.status", json!({"id":"crew-2"}))["status"] == "succeeded"
    });
    // The view reports the crew to Herdr as working, then done.
    until("the viewer to report the crew done", || {
        herdr
            .calls("pane.report_agent")
            .iter()
            .any(|r| r["params"]["state"] == "done" && r["params"]["agent"] == "codex")
    });
    let titles: Vec<Value> = herdr
        .calls("pane.report_metadata")
        .iter()
        .map(|r| r["params"]["title"].clone())
        .collect();
    assert!(
        titles.contains(&json!("infra crew on infra-fix")),
        "{titles:?}"
    );
}

#[test]
fn the_captain_reads_and_types_into_a_live_crews_session_until_it_ends() {
    let place = Place::new();
    // A headless Codex crew that keeps working until released.
    fs::write(
        place.root.join("bin/codex"),
        format!(
            "#!/bin/sh\nprintf '%s\\n' '{{\"type\":\"thread.started\",\"thread_id\":\"t1\"}}'\n\
             while [ ! -e '{root}/release' ]; do sleep 0.05; done\n\
             printf '%s\\n' '{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}'\n",
            root = place.root.display()
        ),
    )
    .unwrap();
    fs::set_permissions(
        place.root.join("bin/codex"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
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
    place.agent(
        "agent.start",
        json!({"id":"crew-3","task_id":"infra-fix","manifest":"bundled:codex","options":{"model":"m"}}),
    );
    let pane = herdr.calls("pane.send_input")[0]["params"]["pane_id"].clone();

    // The fleet shows which crew have a session, and of which kind.
    let snapshot = place.json("fleet.snapshot", json!({}));
    let job = snapshot["ships"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|s| s["jobs"].as_array().unwrap())
        .find(|j| j["id"] == "crew-3")
        .unwrap();
    assert_eq!(job["session"], "viewer", "{job}");

    let read = place.json("session.read", json!({"ship":"infra","job":"crew-3"}));
    assert_eq!(
        (&read["pane_id"], &read["viewer"]),
        (&pane, &json!(true)),
        "{read}"
    );
    place.json(
        "session.send",
        json!({"ship":"infra","job":"crew-3","text":"use the cache","keys":["Enter"]}),
    );
    let typed = herdr.calls("pane.send_input").last().unwrap()["params"].clone();
    assert_eq!(
        (&typed["pane_id"], &typed["text"]),
        (&pane, &json!("use the cache"))
    );

    // Another ship's name, or a crew that has ended, reaches nothing.
    let refused = |args: Value| {
        !place
            .command(&["session.read", "--json", &args.to_string()])
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(refused(json!({"ship":"nobody","job":"crew-3"})));
    fs::write(place.root.join("release"), "").unwrap();
    until("the crew to finish", || {
        place.json("job.status", json!({"id":"crew-3"}))["status"] == "succeeded"
    });
    assert!(refused(json!({"ship":"infra","job":"crew-3"})));
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

/// A 40-digit commit, the shape a release directory is named by.
fn commit(digit: char) -> String {
    std::iter::repeat_n(digit, 40).collect()
}

fn holder_of(place: &Place) -> String {
    let env = fs::read_to_string(place.root.join("pi-env")).unwrap();
    env.trim().split_once(' ').unwrap().1.to_owned()
}

impl Place {
    fn release_dir(&self, commit: &str) -> PathBuf {
        self.root.join("data/releases").join(commit)
    }

    /// Install a release holding this build, as an upgrade leaves it on disk.
    fn install_release(&self, commit: &str) {
        let release = self.release_dir(commit);
        let bin = release.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::copy(BIN, bin.join("privateer")).unwrap();
        fs::set_permissions(bin.join("privateer"), fs::Permissions::from_mode(0o755)).unwrap();
        // A release carries its bundled plugins; these are the repository's.
        let share = release.join("share/privateer");
        fs::create_dir_all(&share).unwrap();
        std::os::unix::fs::symlink(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins"),
            share.join("plugins"),
        )
        .unwrap();
    }

    /// Switch `current`, as an upgrade does.
    fn select(&self, commit: &str) {
        let current = self.root.join("data/current");
        let _ = fs::remove_file(&current);
        std::os::unix::fs::symlink(self.release_dir(commit), &current).unwrap();
    }

    /// Run this installation from `current`, so each pane records its release.
    fn on_release(&mut self, commit: &str) {
        self.install_release(commit);
        self.select(commit);
        self.binary = self.root.join("data/current/bin/privateer");
    }

    /// `privateer host relaunch infra` with extra flags, as the operator.
    fn relaunch_cmd(&self, extra: &[&str]) -> std::process::Output {
        let mut args = vec!["host", "relaunch", "infra"];
        args.extend_from_slice(extra);
        retry_fixture_exec(|| self.command(&args).output()).unwrap()
    }
}

#[test]
fn a_hosted_pane_reports_drift_and_host_relaunch_moves_it_onto_the_current_release() {
    let (a, b) = (commit('a'), commit('b'));
    let mut place = Place::new();
    place.on_release(&a);
    let (mut place, herdr) = hosted(place);

    let bearings = place.json("ship.bearings", json!({"ship":"infra"}));
    assert_eq!(bearings["hosted_pane"]["commit"], a.as_str(), "{bearings}");
    assert_eq!(bearings["hosted_pane"]["drift"], false, "{bearings}");

    // An upgrade moves `current`: the running pane is now behind it, and
    // upgrade status reports the same pane.
    place.install_release(&b);
    place.select(&b);
    let bearings = place.json("ship.bearings", json!({"ship":"infra"}));
    assert_eq!(bearings["hosted_pane"]["drift"], true, "{bearings}");
    assert_eq!(bearings["hosted_pane"]["current_commit"], b.as_str());
    assert!(
        bearings["text"]
            .as_str()
            .unwrap()
            .contains("host relaunch infra"),
        "{bearings}"
    );
    let status = place.json("upgrade.status", json!({}));
    assert_eq!(status["hosted_panes"][0]["ship"], "infra", "{status}");
    assert_eq!(status["hosted_panes"][0]["drift"], true, "{status}");
    let human: Value = serde_json::from_str(&place.ok(&["upgrade", "status"])).unwrap();
    assert_eq!(human["hosted_panes"][0]["drift"], true, "{human}");
    // Asked from a binary outside any installation, the pane still reports
    // drift against the release it runs from, not against the caller's.
    let installed = std::mem::replace(&mut place.binary, PathBuf::from(BIN));
    let bearings = place.json("ship.bearings", json!({"ship":"infra"}));
    place.binary = installed;
    assert_eq!(bearings["hosted_pane"]["drift"], true, "{bearings}");
    assert_eq!(
        bearings["hosted_pane"]["current_commit"],
        b.as_str(),
        "{bearings}"
    );

    // A running turn is refused without --force, and an agent cannot relaunch
    // even its own ship.
    let holder = holder_of(&place);
    place.agent(
        "work.poll",
        json!({"ship":"infra","holder":holder,"idle":false}),
    );
    let refused = place.relaunch_cmd(&[]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("in a turn"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let agent = place
        .command(&["host", "relaunch", "infra", "--force"])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "infra")
        .output()
        .unwrap();
    assert!(!agent.status.success());
    assert!(
        String::from_utf8_lossy(&agent.stderr).contains("agents cannot run it"),
        "{}",
        String::from_utf8_lossy(&agent.stderr)
    );
    assert_eq!(
        place.json("ship.presence", json!({"ship":"infra"}))["attended"],
        true
    );

    // Forced, the pane stops its harness, verifies it exited and releases its
    // lease; a fresh pane on the current release continues the conversation.
    let native = place.native_pid();
    for file in ["pi-env", "pi-argv"] {
        fs::remove_file(place.root.join(file)).unwrap();
    }
    let relaunch = place.relaunch_cmd(&["--force"]);
    assert!(
        relaunch.status.success(),
        "{}",
        String::from_utf8_lossy(&relaunch.stderr)
    );
    let report: Value = serde_json::from_slice(&relaunch.stdout).unwrap();
    assert_eq!(report["from_commit"], a.as_str(), "{report}");
    assert_eq!(report["to_commit"], b.as_str(), "{report}");
    assert_eq!(report["harness_exited"], true, "{report}");
    assert!(report["launch"]["launched"]["pane"].is_string(), "{report}");
    assert_eq!(
        unsafe { libc::kill(native, 0) },
        -1,
        "the old harness outlived its pane"
    );
    until("the relaunched harness", || {
        place.root.join("pi-env").exists()
    });
    let argv = fs::read_to_string(place.root.join("pi-argv")).unwrap();
    assert!(argv.lines().any(|a| a == "--continue"), "{argv}");
    // The first harness and its replacement are one conversation.
    let sessions = fs::read_to_string(place.root.join("pi-sessions")).unwrap();
    let ids: Vec<&str> = sessions.lines().collect();
    assert_eq!(ids.len(), 2, "{sessions}");
    assert_eq!(ids[0], ids[1], "{sessions}");
    assert_eq!(herdr.calls("workspace.create").len(), 2);
    let bearings = place.json("ship.bearings", json!({"ship":"infra"}));
    assert_eq!(bearings["hosted_pane"]["commit"], b.as_str(), "{bearings}");
    assert_eq!(bearings["hosted_pane"]["drift"], false, "{bearings}");

    // A binary replaced in place (renamed over the old file, as an atomic
    // install does) is drift even while its release is still current. Only
    // Linux reports the running image, so only there can the replacement show.
    #[cfg(target_os = "linux")]
    {
        let installed = place.release_dir(&b).join("bin/privateer");
        let staged = installed.with_extension("next");
        fs::copy(BIN, &staged).unwrap();
        fs::rename(&staged, &installed).unwrap();
        let bearings = place.json("ship.bearings", json!({"ship":"infra"}));
        assert_eq!(bearings["hosted_pane"]["drift"], true, "{bearings}");
        assert_eq!(bearings["hosted_pane"]["commit"], b.as_str(), "{bearings}");
    }
    place.stop_native("infra");
}

#[test]
fn an_idle_pane_behind_the_current_release_catches_up_at_its_sleep_point_and_a_busy_one_waits() {
    let (a, b) = (commit('a'), commit('b'));
    let mut place = Place::new();
    place.on_release(&a);
    let mut first_mate = place.json("ship.show", json!({"name":"infra"}))["first_mate"].take();
    first_mate["sleep_after_secs"] = json!(1);
    place.json("ship.put", json!({"name":"infra","first_mate":first_mate}));
    let (place, _herdr) = hosted(place);
    place.install_release(&b);
    place.select(&b);
    let native = place.native_pid();
    let holder = holder_of(&place);

    // A turn in progress is never interrupted, however long it runs.
    place.agent(
        "work.poll",
        json!({"ship":"infra","holder":holder,"idle":false}),
    );
    std::thread::sleep(Duration::from_secs(6));
    assert_eq!(
        unsafe { libc::kill(native, 0) },
        0,
        "a busy first mate was interrupted"
    );

    // Once idle past its sleep point, the behind pane ends at a safe point,
    // having stopped its harness, and the next dispatcher pass starts it again
    // on the current release.
    let idle = json!({"ship":"infra","holder":holder,"idle":true});
    place.agent("work.poll", idle.clone());
    std::thread::sleep(Duration::from_millis(1100));
    place.agent("work.poll", idle);
    until("the behind pane to catch up and leave", || {
        (unsafe { libc::kill(native, 0) }) == -1
            && place.json("ship.presence", json!({"ship":"infra"}))["attended"] == false
    });
    for file in ["pi-env", "pi-argv"] {
        fs::remove_file(place.root.join(file)).unwrap();
    }
    let report = place.tick();
    assert!(report["hosted"]["pane"].is_string(), "{report}");
    // A catch-up does not wake the ship: the new pane starts asleep, current,
    // and runs no harness until mail wakes it.
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        !place.root.join("pi-env").exists(),
        "the catch-up woke the ship"
    );
    let bearings = place.json("ship.bearings", json!({"ship":"infra"}));
    assert_eq!(bearings["hosted_pane"]["commit"], b.as_str(), "{bearings}");
    assert_eq!(bearings["hosted_pane"]["drift"], false, "{bearings}");
    place.json(
        "mail.send",
        json!({"from":"captain","to":"infra","kind":"request","body":"wake up"}),
    );
    until("the woken harness", || place.root.join("pi-env").exists());
    let argv = fs::read_to_string(place.root.join("pi-argv")).unwrap();
    assert!(argv.lines().any(|a| a == "--continue"), "{argv}");
    let sessions = fs::read_to_string(place.root.join("pi-sessions")).unwrap();
    let ids: Vec<&str> = sessions.lines().collect();
    assert_eq!(ids.len(), 2, "{sessions}");
    assert_eq!(ids[0], ids[1], "{sessions}");
    place.stop_native("infra");
}

#[test]
fn mail_that_lands_between_a_catch_up_and_its_replacement_wakes_the_asleep_pane() {
    let (a, b) = (commit('a'), commit('b'));
    let mut place = Place::new();
    place.on_release(&a);
    let mut first_mate = place.json("ship.show", json!({"name":"infra"}))["first_mate"].take();
    first_mate["sleep_after_secs"] = json!(1);
    place.json("ship.put", json!({"name":"infra","first_mate":first_mate}));
    let (place, _herdr) = hosted(place);
    place.install_release(&b);
    place.select(&b);
    let native = place.native_pid();
    let holder = holder_of(&place);

    // The catch-up fences the old pane at its sleep point and it leaves.
    let idle = json!({"ship":"infra","holder":holder,"idle":true});
    place.agent("work.poll", idle.clone());
    std::thread::sleep(Duration::from_millis(1100));
    place.agent("work.poll", idle);
    until("the behind pane to catch up and leave", || {
        (unsafe { libc::kill(native, 0) }) == -1
            && place.json("ship.presence", json!({"ship":"infra"}))["attended"] == false
    });
    for file in ["pi-env", "pi-argv"] {
        fs::remove_file(place.root.join(file)).unwrap();
    }
    // Mail after the fence, before the replacement pane is up: it must wake
    // that pane, not be taken as mail it had already seen.
    place.json(
        "mail.send",
        json!({"from":"captain","to":"infra","kind":"request","body":"wake up"}),
    );
    let report = place.tick();
    assert!(report["hosted"]["pane"].is_string(), "{report}");
    until("the replacement pane woken by the mail", || {
        place.root.join("pi-env").exists()
    });
    let bearings = place.json("ship.bearings", json!({"ship":"infra"}));
    assert_eq!(bearings["hosted_pane"]["commit"], b.as_str(), "{bearings}");
    place.stop_native("infra");
}

#[test]
fn a_pane_predating_relaunch_support_fails_at_once_and_says_how_to_restart() {
    let a = commit('a');
    let mut place = Place::new();
    place.on_release(&a);
    let (place, _herdr) = hosted(place);
    // Its lease is held, but it never recorded a release, as before relaunch existed.
    place
        .database()
        .execute("DELETE FROM hosted_panes WHERE ship='infra'", [])
        .unwrap();
    let started = Instant::now();
    let relaunch = place.relaunch_cmd(&["--force"]);
    assert!(!relaunch.status.success());
    let stderr = String::from_utf8_lossy(&relaunch.stderr);
    assert!(stderr.contains("predates relaunch support"), "{stderr}");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
    let requests: i64 = place
        .database()
        .query_row("SELECT COUNT(*) FROM hosted_relaunches", [], |r| r.get(0))
        .unwrap();
    assert_eq!(requests, 0);
    place.stop_native("infra");
}

#[test]
fn a_relaunch_refuses_a_second_harness_while_the_first_still_runs() {
    let a = commit('a');
    let mut place = Place::new();
    place.on_release(&a);
    let (place, herdr) = hosted(place);
    let native = place.native_pid();
    let relaunch = place
        .command(&["host", "relaunch", "infra", "--force"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Once the request is in, the pane's lease passes to another holder while
    // its harness still runs, so the request's pane has left without exiting.
    until("the relaunch request", || {
        place
            .database()
            .query_row("SELECT COUNT(*) FROM hosted_relaunches", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
            == 1
    });
    place
        .database()
        .execute(
            "UPDATE presence SET holder='host:someone-else' WHERE ship='infra'",
            [],
        )
        .unwrap();
    let output = relaunch.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("still running"), "{stderr}");
    assert_eq!(
        unsafe { libc::kill(native, 0) },
        0,
        "the harness was stopped"
    );
    assert_eq!(herdr.calls("workspace.create").len(), 1);
    // The lease this test handed to someone else never heartbeats, so drop it
    // before stopping the harness; otherwise teardown waits on it forever.
    place
        .database()
        .execute("DELETE FROM presence WHERE ship='infra'", [])
        .unwrap();
    place.stop_native("infra");
}

/// A hosted Claude first mate "infra", set to sleep after one second, launched
/// with the given model and permission mode in its pane. Its folder and the
/// ship folder are trusted, as Claude asks for each folder with its own rules.
fn hosted_claude_sleeper(model: &str, permission_mode: &str) -> (Place, Herdr) {
    let place = Place::new();
    fs::copy(place.root.join("bin/pi"), place.root.join("bin/claude")).unwrap();
    let mut first_mate = place.json("ship.show", json!({"name":"infra"}))["first_mate"].take();
    first_mate["manifest"] = json!("bundled:claude");
    first_mate["options"] = json!({"model": model, "permission_mode": permission_mode});
    first_mate["sleep_after_secs"] = json!(1);
    place.json("ship.put", json!({"name":"infra","first_mate":first_mate}));
    let trust = json!({"projects": {
        place.root.to_str().unwrap(): {"hasTrustDialogAccepted": true},
        place.folder.to_str().unwrap(): {"hasTrustDialogAccepted": true},
    }});
    fs::write(place.root.join(".claude.json"), trust.to_string()).unwrap();
    hosted(place)
}

/// Mail the sleeping hosted first mate awake and wait for its relaunched argv.
fn wake_hosted(place: &Place) -> String {
    place.json(
        "mail.send",
        json!({"from":"captain","to":"infra","kind":"request","body":"wake up"}),
    );
    until("the woken harness", || place.root.join("pi-env").exists());
    fs::read_to_string(place.root.join("pi-argv")).unwrap()
}

#[test]
fn a_woken_hosted_claude_first_mate_relaunches_with_the_profile_read_at_the_wake() {
    let (place, herdr) = hosted_claude_sleeper("claude-opus-5-5", "auto");
    let launched = fs::read_to_string(place.root.join("pi-argv")).unwrap();
    assert!(
        launched
            .lines()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|w| w == ["--permission-mode", "auto"]),
        "{launched}"
    );
    sleep_hosted(&place, &herdr);

    // The captain changes the profile while the mate sleeps: the wake applies
    // it, and the result says so.
    let mut first_mate = place.json("ship.show", json!({"name":"infra"}))["first_mate"].take();
    first_mate["options"] =
        json!({"model":"claude-sonnet-5-5","permission_mode":"bypassPermissions"});
    let put = place.json("ship.put", json!({"name":"infra","first_mate":first_mate}));
    assert!(
        put["first_mate_applies"]
            .as_str()
            .is_some_and(|m| m.contains("next wake")),
        "{put}"
    );
    let argv = wake_hosted(&place);
    let args: Vec<&str> = argv.lines().collect();
    assert!(
        args.windows(2)
            .any(|w| w == ["--permission-mode", "bypassPermissions"]),
        "{argv}"
    );
    assert!(
        args.windows(2)
            .any(|w| w == ["--model", "claude-sonnet-5-5"]),
        "{argv}"
    );
    assert!(!args.contains(&"auto"), "{argv}");
    assert!(!args.contains(&"claude-opus-5-5"), "{argv}");
    place.stop_native("infra");
}

#[test]
fn an_unchanged_hosted_claude_first_mate_relaunches_with_the_same_argv() {
    let (place, herdr) = hosted_claude_sleeper("claude-opus-5-5", "auto");
    let launched = fs::read_to_string(place.root.join("pi-argv")).unwrap();
    sleep_hosted(&place, &herdr);
    let argv = wake_hosted(&place);
    assert_eq!(argv, launched);
    place.stop_native("infra");
}

#[test]
fn fleet_yolo_set_while_a_hosted_first_mate_sleeps_applies_at_its_wake() {
    let (place, herdr) = hosted_claude_sleeper("claude-opus-5-5", "auto");
    sleep_hosted(&place, &herdr);
    // The captain turns on fleet YOLO while the pane sleeps; the wake reads
    // the fleet policy again rather than the one from the pane's launch.
    place.ok(&["permissions", "yolo"]);
    let argv = wake_hosted(&place);
    let args: Vec<&str> = argv.lines().collect();
    assert!(
        args.windows(2)
            .any(|w| w == ["--permission-mode", "bypassPermissions"]),
        "{argv}"
    );
    place.stop_native("infra");
}
