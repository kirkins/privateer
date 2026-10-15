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
    /// how it was started and stays up until `release` exists.
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
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{root}/pi-argv'\n\
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

/// A Herdr that answers one request per connection and runs the text sent to
/// the space it creates, with that pane's Herdr environment.
struct Herdr {
    requests: Arc<Mutex<Vec<Value>>>,
}

impl Herdr {
    fn start(socket: &Path, path: &str) -> Self {
        let listener = UnixListener::bind(socket).unwrap();
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let seen = requests.clone();
        let (socket, path) = (socket.to_owned(), path.to_owned());
        std::thread::spawn(move || {
            let pane = json!({"pane_id":"w9:p1","terminal_id":"t9","workspace_id":"w9",
                "tab_id":"w9:t1","agent":null,"agent_status":"unknown"});
            let mut created: Option<Value> = None;
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
                let result = match request["method"].as_str().unwrap_or("") {
                    "session.snapshot" => {
                        let panes = if created.is_some() {
                            vec![pane.clone()]
                        } else {
                            vec![]
                        };
                        json!({"snapshot":{"version":"test","protocol":20,"panes":panes}})
                    }
                    "workspace.create" => {
                        created = Some(request["params"]["env"].clone());
                        json!({"type":"workspace_created","root_pane":pane})
                    }
                    "pane.send_input" => {
                        let mut shell = Command::new("sh");
                        shell
                            .args(["-c", request["params"]["text"].as_str().unwrap()])
                            .env("PATH", &path)
                            .env("HERDR_ENV", "1")
                            .env("HERDR_SOCKET_PATH", &socket)
                            .env("HERDR_PANE_ID", "w9:p1")
                            .env("HERDR_WORKSPACE_ID", "w9")
                            .env("HERDR_TAB_ID", "w9:t1")
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null());
                        for (key, value) in created.as_ref().unwrap().as_object().unwrap() {
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
        Self { requests }
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
}

#[test]
fn a_hosted_first_mate_runs_live_in_its_own_herdr_space_and_falls_back_without_it() {
    let place = Place::new();
    let socket = place.root.join("config/herdr/herdr.sock");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let herdr = Herdr::start(&socket, &place.path);
    place.json(
        "mail.send",
        json!({"from":"captain","to":"infra","kind":"request","body":"check CI"}),
    );

    // Mail launches the first mate in a background space, not a headless wake.
    let report = place.tick();
    assert_eq!(report["hosted"]["pane"], "w9:p1", "{report}");
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
    let env = fs::read_to_string(place.root.join("pi-env")).unwrap();
    let (flag, holder) = env.trim().split_once(' ').unwrap();
    assert_eq!(flag, "1");
    assert!(holder.starts_with("host:"), "{env}");
    let lease = place.json("ship.presence", json!({"ship":"infra"}));
    assert_eq!(lease["lease"]["holder"], holder);
    assert_eq!(lease["lease"]["binding"]["pane"]["pane_id"], "w9:p1");

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
