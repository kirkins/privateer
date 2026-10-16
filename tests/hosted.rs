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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::BIN;

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
        fs::write(
            bin.join("pi"),
            format!(
                "#!/bin/sh\nif [ \"$PRIVATEER_HOSTED_CREW\" = 1 ]; then printf '%s\\n' \"$@\" > '{root}/crew-argv'; printf '%s' \"$PRIVATEER_HOME\" > '{root}/crew-home'; exit 0; fi\n\
                 printf '%s\\n' \"$@\" > '{root}/pi-argv'\n\
                 printf '%s %s\\n' \"$PRIVATEER_HOSTED\" \"$PRIVATEER_PRESENCE_HOLDER\" > '{root}/pi-env'\n\
                 while [ ! -e '{root}/release' ]; do sleep 0.05; done\n",
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
        };
        place.ok(&["init", "--harness", "pi"]);
        let mut first_mate = place.json("ship.show", json!({"name":"infra"}))["first_mate"].take();
        first_mate["host"] = json!("herdr");
        first_mate["batch_secs"] = json!(0);
        place.json("ship.put", json!({"name":"infra","first_mate":first_mate}));
        place
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = support::command(BIN);
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
        let output = self.command(args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
    /// A command as the ship's agent, as its first mate runs them.
    fn agent(&self, command: &str, args: Value) -> Value {
        let output = self
            .command(&[command, "--json", &args.to_string()])
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", "infra")
            .output()
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
        let (seen, order, live) = (requests.clone(), spaces.clone(), panes.clone());
        let socket = socket.to_owned();
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
                let result = match request["method"].as_str().unwrap_or("") {
                    "session.snapshot" => {
                        json!({"snapshot":{"version":"test","protocol":20,"panes":*live.lock().unwrap()}})
                    }
                    "workspace.create" => {
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
                        let mut child = shell.spawn().unwrap();
                        std::thread::spawn(move || child.wait());
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

    // When the session ends its lease goes; without Herdr, mail still gets a
    // headless wake.
    fs::write(place.root.join("release"), "").unwrap();
    until("the lease to be released", || {
        place.json("ship.presence", json!({"ship":"infra"}))["attended"] == false
    });
    fs::remove_file(&socket).unwrap();
    place.json(
        "mail.send",
        json!({"from":"captain","to":"infra","kind":"request","body":"still there?"}),
    );
    let report = place.tick();
    assert!(report["wake"].is_string(), "{report}");
    assert!(report["host_error"].is_string(), "{report}");
    assert_eq!(place.wakes(), 1);
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
