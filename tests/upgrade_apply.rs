//! `privateer upgrade COMMIT --local`, `upgrade rollback` and `upgrade
//! prune` on a fake installation: published releases holding copies of the
//! test binary, `current` selecting one, a ship home registered in a fleet
//! registry, a real dispatcher started from `current`, and fake `systemctl`
//! that starts what a unit names. Every command runs from a copy inside the
//! fixture with HOME, XDG directories, the fleet registry and the ledger all
//! inside it; the real installation, units and service managers are never
//! touched.
mod support;

use privateer::releases::fetch::SUMS;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use support::{BIN, Fleet, now_settings};

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const C: &str = "cccccccccccccccccccccccccccccccccccccccc";
const D: &str = "dddddddddddddddddddddddddddddddddddddddd";
const BROKER: &str = env!("CARGO_BIN_EXE_privateer-mcp-broker");

// A first mate whose turn waits on its own gate file, as in the
// dispatcher_restart tests.
const GATED_MATE: &str = r#"import json, os, pathlib, sys, time
prompt, log, resume = sys.argv[3], pathlib.Path(sys.argv[5]), sys.argv[6]
launches = log.with_name('launches.jsonl')
n = len(launches.read_text().splitlines()) + 1 if launches.exists() else 1
with launches.open('a') as f:
    f.write(json.dumps({'pid': os.getpid(), 'prompt': prompt}) + '\n')
print(json.dumps({'type': 'thread.started', 'thread_id': resume or 'session-' + str(os.getpid())}), flush=True)
while not log.with_name('gate-' + str(n)).exists():
    time.sleep(0.05)
print(json.dumps({'type': 'turn.completed', 'usage': {'output_tokens': 1}}), flush=True)
"#;

fn until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// Whether some process holds an exclusive lock on `path`.
fn held(path: &Path) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(file) = fs::OpenOptions::new().write(true).open(path) else {
        return false;
    };
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) != 0 }
}

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(root, &path, out);
        } else {
            out.push(path.strip_prefix(root).unwrap().to_string_lossy().into());
        }
    }
}

/// A complete, checked release, as fetch publishes it; `privateer` is the
/// test binary unless a script stands in for a release that cannot start.
fn release(data: &Path, commit: &str, privateer: Option<&str>) {
    let root = data.join("releases").join(commit);
    fs::create_dir_all(root.join("bin")).unwrap();
    match privateer {
        Some(script) => executable(&root.join("bin/privateer"), script),
        None => {
            fs::copy(BIN, root.join("bin/privateer")).unwrap();
        }
    }
    fs::copy(BROKER, root.join("bin/privateer-mcp-broker")).unwrap();
    executable(&root.join("bin/privateer-dashboard"), "#!/bin/sh\nexit 0\n");
    let plugins = root.join("share/privateer/plugins/pi");
    fs::create_dir_all(&plugins).unwrap();
    fs::write(
        plugins.join("manifest.json"),
        json!({"schema_version":1,"id":"pi","executable":["python3","adapter.py"]}).to_string(),
    )
    .unwrap();
    let mut names = Vec::new();
    walk(&root, &root, &mut names);
    names.sort();
    let sums: String = names
        .iter()
        .map(|n| {
            format!(
                "{:x}  {n}\n",
                Sha256::digest(fs::read(root.join(n)).unwrap())
            )
        })
        .collect();
    fs::write(root.join(SUMS), sums).unwrap();
}

/// A fleet installed on releases: A and B published, `current` on A, and
/// the ship registered in the fleet registry so an upgrade finds its
/// dispatcher.
struct World {
    fleet: Fleet,
}

impl World {
    fn new(fleet: Fleet) -> Self {
        let world = Self { fleet };
        fs::write(
            world.fleet.home.join("config.json"),
            json!({"ship":"infra", "resource_home": world.root().join("ledger")}).to_string(),
        )
        .unwrap();
        release(&world.data(), A, None);
        release(&world.data(), B, None);
        symlink(Path::new("releases").join(A), world.data().join("current")).unwrap();
        let conn = rusqlite::Connection::open(world.root().join("fleet.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS ships(name TEXT PRIMARY KEY, home TEXT NOT NULL,
               created_by TEXT NOT NULL, created_ms INTEGER NOT NULL, retired_ms INTEGER);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ships(name, home, created_by, created_ms) VALUES('infra', ?1, 'captain', 0)",
            [world.fleet.home.to_string_lossy()],
        )
        .unwrap();
        world
    }
    fn root(&self) -> &Path {
        &self.fleet.root
    }
    fn data(&self) -> PathBuf {
        self.root().join("data/privateer")
    }
    fn entry(&self) -> PathBuf {
        self.data().join("current/bin/privateer")
    }
    fn current(&self) -> PathBuf {
        fs::read_link(self.data().join("current")).unwrap()
    }

    /// `current/bin/privateer` with every user directory inside the fixture.
    fn command(&self, args: &[&str]) -> Command {
        self.command_at(&self.fleet.home, args)
    }

    fn command_at(&self, home: &Path, args: &[&str]) -> Command {
        let entry = self.entry();
        assert!(
            entry.starts_with(self.root()) && support::is_temporary(self.root()),
            "upgrade runs only from a copy inside a temporary fixture"
        );
        let mut command = support::command(&entry);
        command
            .env("HOME", self.root().join("user"))
            .env("XDG_DATA_HOME", self.root().join("data"))
            .env("XDG_CONFIG_HOME", self.root().join("config"))
            .env("XDG_STATE_HOME", self.root().join("state"))
            .env("XDG_CACHE_HOME", self.root().join("cache"))
            .env("PRIVATEER_FLEET", self.root().join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", self.root().join("ledger"))
            .env("PRIVATEER_SERVICE", "detached")
            .env("PRIVATEER_SYSTEMCTL", self.root().join("fake/systemctl"))
            .env("PRIVATEER_LAUNCHCTL", self.root().join("fake/launchctl"))
            .env_remove("CARGO_HOME")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH")
            .args(["--home"])
            .arg(home)
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// A round's final state, printed by the command.
    fn round(&self, output: &Output) -> Value {
        serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!(
                "no round state: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }

    fn running(&self) -> bool {
        privateer::service::running(&self.fleet.home).unwrap()
    }

    /// The release the home's dispatcher generation says it runs.
    fn dispatcher_commit(&self) -> Option<String> {
        let raw: String = self
            .fleet
            .store
            .connection()
            .query_row(
                "SELECT value FROM metadata WHERE key='dispatcher_release'",
                [],
                |r| r.get(0),
            )
            .ok()?;
        let release: Value = serde_json::from_str(&raw).unwrap();
        let image = PathBuf::from(release["image"].as_str().unwrap());
        Some(
            image
                .parent()?
                .parent()?
                .file_name()?
                .to_string_lossy()
                .into_owned(),
        )
    }

    fn history(&self) -> Vec<Value> {
        let status: Value = serde_json::from_slice(&self.run(&["upgrade", "status"]).stdout)
            .expect("upgrade status");
        status["history"].as_array().unwrap().clone()
    }

    fn start_detached(&self) {
        let output = self.run(&["service", "start"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        until("the dispatcher", || self.running());
        // serve.lock is held before activation records dispatcher_release.
        until("the dispatcher release on A", || {
            self.dispatcher_commit().as_deref() == Some(A)
        });
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = self.command(&["service", "stop"]).output();
        // Processes the fake systemctl started.
        if let Ok(entries) = fs::read_dir(self.root().join("fake")) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|x| x == "pid") {
                    if let Some(pid) = fs::read_to_string(&path)
                        .ok()
                        .and_then(|p| p.trim().parse::<i32>().ok())
                    {
                        unsafe { libc::kill(pid, libc::SIGTERM) };
                    }
                }
            }
        }
        let output = self.fleet.run("job.list", json!({}), None);
        if let Ok(jobs) = serde_json::from_slice::<Value>(&output.stdout) {
            for job in jobs["jobs"].as_array().into_iter().flatten() {
                if matches!(
                    job["status"].as_str(),
                    Some("running" | "starting" | "launching" | "queued" | "orphaned")
                ) {
                    let _ = self.fleet.run("job.cancel", json!({"id":job["id"]}), None);
                }
            }
        }
    }
}

fn launches(fleet: &Fleet) -> Vec<Value> {
    whole_lines(&fs::read_to_string(fleet.root.join("launches.jsonl")).unwrap_or_default())
}

/// Every record the log has finished writing: up to and including its last
/// newline. The fake launcher can be caught mid-line, so a torn tail without
/// its newline is left for the wait loop to read again; every whole line is
/// kept and still parsed strictly.
fn whole_lines(log: &str) -> Vec<Value> {
    log[..log.rfind('\n').map_or(0, |i| i + 1)]
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
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

fn wakes(fleet: &Fleet) -> Vec<Value> {
    fleet.ok("wake.list", json!({"ship":"infra"}), None)["wakes"]
        .as_array()
        .unwrap()
        .clone()
}

fn send(fleet: &Fleet, id: &str) -> Value {
    fleet.ok(
        "mail.send",
        json!({"id":id,"from":"apps","to":"infra","kind":"notice",
        "task_id":"infra-repair","body":id}),
        None,
    )
}

fn steps(round: &Value) -> Vec<String> {
    round["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["step"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn an_approved_upgrade_requested_inside_a_wake_is_detached_and_replies_in_its_thread() {
    let fleet = Fleet::new(now_settings());
    let mate = GATED_MATE.replace("'prompt': prompt", "'prompt': prompt, 'sid': os.getsid(0)")
        .replace("while not log.with_name('gate-' + str(n)).exists():", r#"
if n == 1:
    import subprocess
    root = log.parent
    env = dict(os.environ, PRIVATEER_SERVICE='none')
    # These are copies in an isolated installation, never a live service.
    args = json.loads((root / 'upgrade-args.json').read_text())
    result = subprocess.run([str(root / 'data/privateer/current/bin/privateer'),
        '--home', str(root / 'home'), 'upgrade.start', '--json', json.dumps(args)],
        env=env, text=True, capture_output=True, timeout=15)
    (root / 'upgrade-start.json').write_text(json.dumps(dict(code=result.returncode, out=result.stdout, err=result.stderr)))
while not log.with_name('gate-' + str(n)).exists():"#);
    fs::write(fleet.root.join("mate.py"), mate).unwrap();
    let world = World::new(fleet);
    let output = world.run(&["upgrade.plan", "--json", &json!({"commit":B}).to_string()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    let output = world.run(&[
        "approval.grant",
        "--json",
        &json!({"id":"wake-upgrade","task_id":"infra-repair",
        "artifact":B,"environment":plan["environment"],"approver":"captain"})
        .to_string(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::write(
        world.root().join("upgrade-args.json"),
        json!({"commit":B,"task_id":"infra-repair","approval_id":"wake-upgrade"}).to_string(),
    )
    .unwrap();
    // The operator dispatcher owns the upgrade environment. Start a real
    // disposable dispatcher with manager access disabled, recording custody
    // exactly as detached service start does. Restart still uses its pid/lock.
    // A resident of this fixture: stopped with its group and any detached
    // payload when dropped, however the test ends.
    let mut dispatcher = support::Resident::spawn(
        {
            let mut command = world.command(&["serve"]);
            command
                .env("PRIVATEER_SERVICE", "none")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            command
        },
        world.root(),
    );
    fs::write(
        world.fleet.home.join("serve.pid"),
        dispatcher.id().to_string(),
    )
    .unwrap();
    until("the isolated operator dispatcher", || {
        world.running() && world.dispatcher_commit().as_deref() == Some(A)
    });
    send(&world.fleet, "start-upgrade");
    until("the wake's nonblocking upgrade request", || {
        world.root().join("upgrade-start.json").is_file()
    });
    let result: Value =
        serde_json::from_slice(&fs::read(world.root().join("upgrade-start.json")).unwrap())
            .unwrap();
    assert_eq!(result["code"], 0, "{result}");
    let request: Value = serde_json::from_str(result["out"].as_str().unwrap()).unwrap();
    let held = wakes(&world.fleet)[0].clone();
    until("the linked round's outcome", || {
        fs::read(
            world
                .data()
                .join("upgrade-linked")
                .join(format!("{}.json", request["id"].as_str().unwrap())),
        )
        .ok()
        .and_then(|s| serde_json::from_slice::<Value>(&s).ok())
        .is_some_and(|s| !s["outcome"].is_null())
    });
    let round: Value = serde_json::from_slice(
        &fs::read(
            world
                .data()
                .join("upgrade-linked")
                .join(format!("{}.json", request["id"].as_str().unwrap())),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(round["outcome"], "switched", "{round}");
    assert_eq!(world.dispatcher_commit().as_deref(), Some(B));
    let _ = dispatcher.wait();
    assert_eq!(round["dispatch"], "claimed");
    let job = world
        .fleet
        .ok("job.status", json!({"id":held["job_id"]}), None);
    assert_eq!(job["status"], "running", "{job}");
    assert_eq!(launches(&world.fleet).len(), 1);
    assert_ne!(round["session"], launches(&world.fleet)[0]["sid"]);
    let db = world.fleet.store.connection();
    until("the request thread's reply", || {
        db.query_row(
            "SELECT COUNT(*) FROM mail WHERE kind='reply' AND recipient='infra' AND correlation=?1",
            [request["request_thread"].as_str().unwrap()],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 1
    });
    fs::write(world.root().join("gate-1"), "").unwrap();
    until("the outcome wake", || launches(&world.fleet).len() == 2);
    assert!(
        launches(&world.fleet)[1]["prompt"]
            .as_str()
            .unwrap()
            .contains("Upgrade")
    );
    fs::write(world.root().join("gate-2"), "").unwrap();
    until("both wakes completing once", || {
        wakes(&world.fleet)
            .iter()
            .all(|w| w["status"] == "succeeded")
    });
    let count: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM mail WHERE kind='reply' AND correlation=?1",
            [request["request_thread"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn a_detached_round_keeps_a_running_wake_delivers_restart_mail_once_and_rolls_back() {
    let fleet = Fleet::new(now_settings());
    fs::write(fleet.root.join("mate.py"), GATED_MATE).unwrap();
    let world = World::new(fleet);
    let fleet = &world.fleet;
    world.start_detached();
    assert_eq!(world.dispatcher_commit().as_deref(), Some(A));

    send(fleet, "held");
    until("the held wake", || launches(fleet).len() == 1);
    let held = wakes(fleet)[0].clone();
    let job = |wake: &Value| fleet.ok("job.status", json!({"id":wake["job_id"]}), None);
    until("the held wake's job", || job(&held)["status"] == "running");
    let before = job(&held);

    // The round runs detached; mail sent while it restarts the dispatcher.
    let mut upgrade = world.command(&["upgrade", B, "--local"]);
    let child = upgrade
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let rounds = world.data().join("upgrade");
    until("the switch", || {
        fs::read_dir(&rounds).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|e| fs::read_to_string(e.path()).is_ok_and(|text| text.contains("\"switch\"")))
        })
    });
    let restart_mail = send(fleet, "during-restart");
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let round = world.round(&output);
    assert_eq!(round["outcome"], "switched", "{round}");
    assert_eq!(round["from"], A);
    assert!(
        round["steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|step| step["step"] == "switch: health recheck"
                && step["state"] == "ok"
                && step["detail"] == "lock held; recorded generation unchanged (separate checks)"),
        "{round}"
    );
    assert_eq!(world.current(), Path::new("releases").join(B));
    assert_eq!(world.dispatcher_commit().as_deref(), Some(B));
    assert!(
        round["summary"][0]
            .as_str()
            .unwrap()
            .starts_with("infra: dispatcher restarted (detached) on bbbbbbbbbbbb"),
        "{round}"
    );

    // The wake ran through the round untouched, and completes once.
    let during = job(&held);
    assert_eq!(during["status"], "running");
    assert_eq!(during["incarnation"], before["incarnation"]);
    assert_eq!(during["payload"], before["payload"]);
    assert_eq!(during["supervisor"], before["supervisor"]);
    assert_eq!(launches(fleet).len(), 1);
    fs::write(fleet.root.join("gate-1"), "").unwrap();
    until("the restart mail's wake", || launches(fleet).len() == 2);
    fs::write(fleet.root.join("gate-2"), "").unwrap();
    until("both wakes to finish", || {
        wakes(fleet).iter().all(|w| w["status"] == "succeeded")
    });
    thread::sleep(Duration::from_millis(1500)); // Another dispatcher pass.
    let seen = launches(fleet);
    assert_eq!(seen.len(), 2, "{seen:?}");
    let delivered = seen
        .iter()
        .filter(|l| l["prompt"].as_str().unwrap().contains("id during-restart "))
        .count();
    assert_eq!(delivered, 1, "{seen:?}");
    let all = wakes(fleet);
    assert_eq!(all.len(), 2);
    assert_eq!(all[1]["id"], held["id"]);
    assert_eq!(all[0]["through_seq"], restart_mail["seq"]);

    // Rollback returns to the release the upgrade came from.
    let output = world.run(&["upgrade", "rollback", "--local"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rollback = world.round(&output);
    assert_eq!(rollback["kind"], "rollback", "{rollback}");
    assert_eq!(rollback["outcome"], "switched", "{rollback}");
    assert_eq!(rollback["commit"], A);
    assert_eq!(rollback["rollback_of"], round["id"]);
    assert_eq!(world.current(), Path::new("releases").join(A));
    assert_eq!(world.dispatcher_commit().as_deref(), Some(A));
    let history = world.history();
    assert_eq!(history[0]["kind"], "rollback");
    assert_eq!(history[0]["to_commit"], A);
    assert_eq!(history[0]["rollback_of"], round["id"]);
    assert_eq!(history[1]["kind"], "upgrade");
    assert_eq!(history[1]["outcome"], "switched");
    assert_eq!(history[1]["from_commit"], A);
    assert_eq!(history[1]["to_commit"], B);
    // A second rollback steps further back, and there is nothing there.
    let again = world.run(&["upgrade", "rollback", "--local"]);
    assert!(!again.status.success());
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("no recorded upgrade onto"),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    assert_eq!(
        fleet.ok("resource.status", json!({}), None)["reservations"],
        json!([])
    );
}

#[test]
fn a_dispatcher_that_cannot_start_on_the_new_release_is_rolled_back() {
    let world = World::new(Fleet::new(now_settings()));
    release(&world.data(), C, Some("#!/bin/sh\nexit 3\n"));
    world.start_detached();
    let output = world
        .command(&["upgrade", C, "--local"])
        .env("PRIVATEER_UPGRADE_HEALTH_SECS", "3")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let round = world.round(&output);
    assert_eq!(round["outcome"], "rolled_back", "{round}");
    assert!(
        round["error"]
            .as_str()
            .unwrap()
            .contains("infra did not come back on"),
        "{round}"
    );
    assert_eq!(
        steps(&round),
        [
            "plan",
            "switch",
            "switch",
            "switch: restart dispatcher",
            "switch: health",
            "rollback: switch",
            "rollback: switch",
            "rollback: restart dispatcher",
            "rollback: health",
            "rollback: health recheck"
        ]
    );
    assert_eq!(round["steps"][1]["state"], "switching", "{round}");
    assert_eq!(round["steps"][4]["state"], "failed", "{round}");
    assert_eq!(round["steps"][8]["state"], "ok", "{round}");
    assert_eq!(world.current(), Path::new("releases").join(A));
    until("the dispatcher on A", || world.running());
    assert_eq!(world.dispatcher_commit().as_deref(), Some(A));
    let history = world.history();
    assert_eq!(history[0]["kind"], "rollback");
    assert_eq!(history[0]["outcome"], "switched");
    assert_eq!(history[0]["from_commit"], C);
    assert_eq!(history[0]["to_commit"], A);
    assert_eq!(history[0]["rollback_of"], round["id"]);
    assert_eq!(history[1]["kind"], "upgrade");
    assert_eq!(history[1]["outcome"], "rolled_back");
    assert_eq!(history[1]["id"], round["id"]);
    // `upgrade status` shows the round's last result.
    let status: Value = serde_json::from_slice(&world.run(&["upgrade", "status"]).stdout).unwrap();
    assert_eq!(status["round"]["outcome"], "rolled_back", "{status}");
    assert_eq!(status["round"]["in_progress"], false, "{status}");

    // A release whose dispatcher records its generation on the new release
    // and then exits, before any pass, is not healthy.
    release(
        &world.data(),
        D,
        Some(&format!("#!{}\n{CRASHING}", support::python().display())),
    );
    let output = world
        .command(&["upgrade", D, "--local"])
        .env("PRIVATEER_UPGRADE_HEALTH_SECS", "10")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let round = world.round(&output);
    assert_eq!(round["outcome"], "rolled_back", "{round}");
    assert!(
        round["steps"][4]["detail"]
            .as_str()
            .unwrap()
            .contains("stopped before finishing a pass"),
        "{round}"
    );
    assert_eq!(world.current(), Path::new("releases").join(A));
    until("the dispatcher on A", || world.running());
    assert_eq!(world.dispatcher_commit().as_deref(), Some(A));

    // A round that cannot record its outcome once `current` moved (here its
    // history write fails) does not stay switched without a record: it
    // goes back, and the history says so.
    let registry = rusqlite::Connection::open(world.root().join("fleet.sqlite")).unwrap();
    registry
        .execute_batch(
            r#"CREATE TRIGGER no_switch BEFORE INSERT ON upgrades
               WHEN NEW.record LIKE '%"kind":"upgrade"%' AND NEW.record LIKE '%"outcome":"switched"%'
               BEGIN SELECT RAISE(ABORT, 'injected history failure'); END;"#,
        )
        .unwrap();
    let output = world.run(&["upgrade", B, "--local"]);
    assert!(!output.status.success());
    let round = world.round(&output);
    assert_eq!(round["outcome"], "rolled_back", "{round}");
    let error = round["error"].as_str().unwrap();
    assert!(
        error.contains(&format!("could not finish on {B}"))
            && error.contains("injected history failure")
            && error.contains(&format!("current is back on {A}")),
        "{round}"
    );
    assert_eq!(world.current(), Path::new("releases").join(A));
    until("the dispatcher on A", || world.running());
    assert_eq!(world.dispatcher_commit().as_deref(), Some(A));
    let history = world.history();
    assert_eq!(history[0]["kind"], "rollback");
    assert_eq!(history[0]["outcome"], "switched");
    assert_eq!(history[0]["rollback_of"], round["id"]);
    assert_eq!(history[1]["id"], round["id"]);
    assert_eq!(history[1]["outcome"], "rolled_back");
}

#[test]
fn a_dispatcher_that_exits_while_another_finishes_its_pass_is_rolled_back() {
    let world = World::new(Fleet::new(now_settings()));
    let other = Fleet::new(now_settings());
    fs::write(
        other.home.join("config.json"),
        json!({
            "ship": "infra", "resource_home": world.root().join("ledger")
        })
        .to_string(),
    )
    .unwrap();
    // Stop the second real dispatcher even if an assertion fails.
    struct Stop<'a>(&'a World, &'a Path);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            let _ = self.0.command_at(self.1, &["service", "stop"]).output();
        }
    }
    let _stop = Stop(&world, &other.home);
    let registry = rusqlite::Connection::open(world.root().join("fleet.sqlite")).unwrap();
    registry.execute(
        "INSERT INTO ships(name, home, created_by, created_ms) VALUES('later', ?1, 'captain', 0)",
        [other.home.to_string_lossy()],
    ).unwrap();
    world.start_detached();
    let start = world
        .command_at(&other.home, &["service", "start"])
        .output()
        .unwrap();
    assert!(start.status.success(), "{start:?}");
    until("the second dispatcher", || {
        held(&other.home.join("serve.lock"))
    });
    // The first dispatcher completes a pass, then exits only after its
    // first health result was recorded. The second completes its pass only
    // after that exit, forcing the first check to become stale.
    let script = format!(
        "#!{}\nFIRST_HOME = {}\n{EXIT_AFTER_PASS}",
        support::python().display(),
        serde_json::to_string(&world.fleet.home).unwrap()
    );
    release(&world.data(), C, Some(&script));
    let output = world
        .command(&["upgrade", C, "--local"])
        .env("PRIVATEER_UPGRADE_HEALTH_SECS", "15")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let round = world.round(&output);
    assert_eq!(round["outcome"], "rolled_back", "{round}");
    let steps = round["steps"].as_array().unwrap();
    for name in ["infra", "later"] {
        assert!(
            steps
                .iter()
                .any(|s| s["step"] == "switch: health" && s["name"] == name && s["state"] == "ok"),
            "{round}"
        );
    }
    assert!(
        steps.iter().any(|s| s["step"] == "switch: health recheck"
            && s["name"] == "infra"
            && s["state"] == "failed"
            && s["detail"]
                .as_str()
                .unwrap()
                .contains("stopped after its first pass")),
        "{round}"
    );
    assert_eq!(world.current(), Path::new("releases").join(A));
    assert_eq!(world.dispatcher_commit().as_deref(), Some(A));
}

const EXIT_AFTER_PASS: &str = r#"
import fcntl, json, os, pathlib, sqlite3, sys, time, uuid
home = pathlib.Path(sys.argv[sys.argv.index('--home') + 1])
lock = open(home / 'serve.lock', 'a')
fcntl.flock(lock, fcntl.LOCK_EX)
generation = str(uuid.uuid4())
release = {'generation': generation, 'image': os.path.realpath(sys.argv[0]), 'pid': os.getpid()}
first = str(home) == FIRST_HOME
if first:
    release['passed_ms'] = 1
db = sqlite3.connect(home / 'privateer.sqlite', timeout=10)
def put(key, value):
    db.execute('INSERT INTO metadata(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value', (key,value))
put('dispatcher_generation', generation)
put('dispatcher_release', json.dumps(release))
db.commit()
if first:
    rounds = pathlib.Path(sys.argv[0]).resolve().parents[3] / 'upgrade'
    while True:
        for path in rounds.glob('*.json'):
            state = json.loads(path.read_text())
            if any(s.get('step') == 'switch: health' and s.get('name') == 'infra' and s.get('state') == 'ok' for s in state['steps']):
                sys.exit(1)
        time.sleep(0.02)
else:
    first_lock = open(pathlib.Path(FIRST_HOME) / 'serve.lock', 'a')
    fcntl.flock(first_lock, fcntl.LOCK_EX)
    fcntl.flock(first_lock, fcntl.LOCK_UN)
    release['passed_ms'] = 1
    put('dispatcher_release', json.dumps(release))
    db.commit()
    time.sleep(600)
"#;

/// A dispatcher that takes its lock and records a generation on its own
/// release, then exits before a pass: a release that crashes on start.
const CRASHING: &str = r#"
import fcntl, json, os, sqlite3, sys, time, uuid
home = sys.argv[sys.argv.index('--home') + 1]
lock = open(os.path.join(home, 'serve.lock'), 'a')
fcntl.flock(lock, fcntl.LOCK_EX)
generation = str(uuid.uuid4())
release = {'generation': generation, 'image': os.path.realpath(sys.argv[0]), 'pid': os.getpid()}
db = sqlite3.connect(os.path.join(home, 'privateer.sqlite'), timeout=10)
for key, value in [('dispatcher_generation', generation), ('dispatcher_release', json.dumps(release))]:
    db.execute("INSERT INTO metadata(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", (key, value))
db.commit()
time.sleep(1)
sys.exit(1)
"#;

#[test]
fn an_interrupted_round_says_so_and_the_same_commit_resumes_its_restarts() {
    let world = World::new(Fleet::new(now_settings()));
    world.start_detached();
    let upgrade = world
        .command(&["upgrade", B, "--local"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // The round is killed as soon as it has switched.
    let rounds = world.data().join("upgrade");
    let deadline = Instant::now() + Duration::from_secs(30);
    let state = loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the switch"
        );
        let switched = fs::read_dir(&rounds).ok().and_then(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                .filter_map(|e| serde_json::from_slice::<Value>(&fs::read(e.path()).ok()?).ok())
                .find(|state| {
                    state["steps"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|s| s["step"] == "switch" && s["state"] == "done")
                })
        });
        if let Some(state) = switched {
            break state;
        }
        thread::sleep(Duration::from_millis(2));
    };
    let pid = state["pid"].as_i64().unwrap() as i32;
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let output = upgrade.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert_eq!(world.current(), Path::new("releases").join(B));

    let status: Value = serde_json::from_slice(&world.run(&["upgrade", "status"]).stdout).unwrap();
    let interrupted = &status["round"];
    assert_eq!(interrupted["id"], state["id"], "{status}");
    assert!(interrupted["outcome"].is_null(), "{status}");
    assert_eq!(interrupted["in_progress"], false, "{status}");
    assert_eq!(interrupted["interrupted"], true, "{status}");
    assert!(
        interrupted["note"].as_str().unwrap().contains(&format!(
            "`privateer upgrade {B} --local` resumes its restarts"
        )),
        "{status}"
    );

    // A real resume attempt can fail before its switch (here daemon-reload).
    // Its terminal state must not hide the older interrupted round.
    let units = world.root().join("config/systemd/user");
    fs::create_dir_all(&units).unwrap();
    let unit = units.join("privateer-reload-fixture.service");
    let unsafe_unit = format!(
        "[Unit]\nDescription=Privateer dispatcher for ship fixture\n[Service]\nExecStart=\"{}\" --home \"{}\" serve\nKillMode=control-group\n",
        world.entry().display(),
        world.fleet.home.display()
    );
    fs::write(&unit, &unsafe_unit).unwrap();
    let fake = world.root().join("fake");
    fs::create_dir_all(&fake).unwrap();
    executable(
        &fake.join("systemctl"),
        "#!/bin/sh\necho injected-reload-failure >&2\nexit 1\n",
    );
    let failed = world.run(&["upgrade", B, "--local"]);
    assert!(!failed.status.success());
    let failed = world.round(&failed);
    assert_eq!(failed["outcome"], "failed", "{failed}");
    assert!(
        failed["error"]
            .as_str()
            .unwrap()
            .contains("daemon-reload failed"),
        "{failed}"
    );
    assert!(!steps(&failed).contains(&"switch".into()), "{failed}");
    let status: Value = serde_json::from_slice(&world.run(&["upgrade", "status"]).stdout).unwrap();
    assert_eq!(status["round"]["id"], state["id"], "{status}");
    assert_eq!(status["round"]["interrupted"], true, "{status}");
    assert!(
        status["round"]["note"]
            .as_str()
            .unwrap()
            .contains("resumes its restarts"),
        "{status}"
    );
    // An attempt can also die before switching and never save an outcome.
    // Exercise both a resume and an upgrade to a different release; neither
    // may hide the original switch or prevent a subsequent resume of B.
    release(&world.data(), C, None);
    executable(
        &fake.join("systemctl"),
        &format!(
            "#!{}\nimport os, signal\nos.kill(os.getppid(), signal.SIGKILL)\n",
            support::python().display()
        ),
    );
    for commit in [B, C] {
        // The previous attempt already repaired this unit before its reload
        // failed. Reset it so each attempt needs daemon-reload: detached
        // dispatcher restarts themselves never call the fake systemctl.
        fs::write(&unit, &unsafe_unit).unwrap();
        let killed = world.run(&["upgrade", commit, "--local"]);
        assert!(!killed.status.success(), "{killed:?}");
        let newest = fs::read_dir(&rounds)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .map(|e| serde_json::from_slice::<Value>(&fs::read(e.path()).unwrap()).unwrap())
            .max_by_key(|s| s["requested_ms"].as_i64().unwrap())
            .unwrap();
        assert_ne!(newest["id"], state["id"], "{newest}");
        assert_ne!(newest["id"], failed["id"], "{newest}");
        assert_eq!(newest["commit"], commit, "{newest}");
        assert!(newest["outcome"].is_null(), "{newest}");
        assert!(steps(&newest).contains(&"stop policy".into()), "{newest}");
        assert!(
            !steps(&newest).contains(&"daemon-reload".into()),
            "{newest}"
        );
        assert!(!steps(&newest).contains(&"switch".into()), "{newest}");
        assert!(!held(&world.data().join("upgrade.lock")));
        assert_eq!(world.current(), Path::new("releases").join(B));
        let status: Value =
            serde_json::from_slice(&world.run(&["upgrade", "status"]).stdout).unwrap();
        assert_eq!(status["round"]["id"], state["id"], "{status}");
        assert_eq!(status["round"]["interrupted"], true, "{status}");
        assert!(
            status["round"]["note"]
                .as_str()
                .unwrap()
                .contains("resumes its restarts"),
            "{status}"
        );
        let dry = world.run(&["upgrade", B, "--local", "--dry-run"]);
        assert!(dry.status.success(), "{dry:?}");
        let plan: Value = serde_json::from_slice(&dry.stdout).unwrap();
        assert_eq!(plan["resumes"], state["id"], "{plan}");
    }
    fs::remove_file(unit).unwrap();

    // The same commit resumes: the dispatcher, whether the killed round had
    // stopped it, restarted it or not reached it, comes back on B.
    let output = world.run(&["upgrade", B, "--local"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let round = world.round(&output);
    assert_eq!(round["outcome"], "switched", "{round}");
    assert_eq!(round["resumes"], state["id"], "{round}");
    assert_eq!(round["from"], A, "{round}");
    assert!(
        steps(&round).contains(&"switch: health".to_owned()),
        "{round}"
    );
    assert_eq!(world.current(), Path::new("releases").join(B));
    until("the dispatcher on B", || world.running());
    assert_eq!(world.dispatcher_commit().as_deref(), Some(B));
    let closed: Value = serde_json::from_slice(
        &fs::read(rounds.join(format!("{}.json", state["id"].as_str().unwrap()))).unwrap(),
    )
    .unwrap();
    assert_eq!(closed["outcome"], "interrupted");
    assert_eq!(closed["resumed_by"], round["id"]);
    let history = world.history();
    assert_eq!(history[0]["kind"], "upgrade");
    assert_eq!(history[0]["outcome"], "switched");
    assert_eq!(history[0]["from_commit"], A);
    assert_eq!(history[0]["to_commit"], B);
    // Nothing is left to resume.
    let again = world.run(&["upgrade", B, "--local"]);
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("current already selects"),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
}

#[test]
fn the_same_commit_resumes_from_switch_intent_without_a_done_step() {
    let world = World::new(Fleet::new(now_settings()));
    world.start_detached();
    let dry = world.run(&["upgrade", B, "--local", "--dry-run"]);
    assert!(dry.status.success(), "{dry:?}");
    let plan: Value = serde_json::from_slice(&dry.stdout).unwrap();
    // Seed exactly the durable state produced by the SIGKILL boundary in
    // the releases unit test, then exercise the real CLI resume path.
    let rounds = world.data().join("upgrade");
    fs::create_dir_all(&rounds).unwrap();
    fs::write(
        rounds.join("abcd.json"),
        json!({
            "id": "abcd", "kind": "upgrade", "commit": B, "from": A,
            "requested_ms": 0, "outcome": null,
            "steps": [{"step": "plan", "state": "done", "plan": plan},
                {"step": "switch", "state": "switching", "from": A, "to": B}]
        })
        .to_string(),
    )
    .unwrap();
    symlink(
        Path::new("releases").join(B),
        world.data().join("current.new"),
    )
    .unwrap();
    fs::rename(
        world.data().join("current.new"),
        world.data().join("current"),
    )
    .unwrap();
    let status: Value = serde_json::from_slice(&world.run(&["upgrade", "status"]).stdout).unwrap();
    assert_eq!(status["round"]["interrupted"], true, "{status}");
    assert!(
        status["round"]["note"]
            .as_str()
            .unwrap()
            .contains("resumes its restarts"),
        "{status}"
    );
    let output = world.run(&["upgrade", B, "--local"]);
    assert!(output.status.success(), "{output:?}");
    let round = world.round(&output);
    assert_eq!(round["outcome"], "switched", "{round}");
    assert_eq!(round["resumes"], "abcd", "{round}");
    assert_eq!(round["from"], A, "{round}");
    assert_eq!(world.dispatcher_commit().as_deref(), Some(B));
}

/// The plan leaves out a ship whose pid file names no live process while
/// something else holds its lock (restarting it would fail after the
/// switch), and refuses an agent that would end its wakes when it stops.
#[test]
fn the_plan_leaves_out_a_dispatcher_it_did_not_start_and_refuses_an_agent_that_kills_wakes() {
    let world = World::new(Fleet::new(now_settings()));
    let home = world.fleet.home.clone();
    let mut gone = Command::new("true").spawn().unwrap();
    gone.wait().unwrap();
    fs::write(home.join("serve.pid"), gone.id().to_string()).unwrap();
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _holder = Kill(
        Command::new(support::python())
            .args([
                "-c",
                "import fcntl,sys,time\nf=open(sys.argv[1],'a')\nfcntl.flock(f,fcntl.LOCK_EX)\ntime.sleep(60)",
            ])
            .arg(home.join("serve.lock"))
            .spawn()
            .unwrap(),
    );
    until("the hand-started holder", || world.running());
    let output = world.run(&["upgrade", B, "--local", "--dry-run"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["dispatchers"], json!([]), "{plan}");
    let left = plan["left"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["name"] == "infra")
        .unwrap_or_else(|| panic!("{plan}"));
    assert!(
        left["reason"]
            .as_str()
            .unwrap()
            .contains("is not the process Privateer started"),
        "{plan}"
    );

    // An agent for the dispatcher that keeps its process group.
    let short: String = Sha256::digest(home.to_string_lossy().as_bytes())
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect();
    let agents = world.root().join("user/Library/LaunchAgents");
    fs::create_dir_all(&agents).unwrap();
    let label = format!("privateer-infra-{short}");
    fs::write(
        agents.join(format!("{label}.plist")),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{label}</string>\n<key>ProgramArguments</key><array><string>{}</string><string>--home</string><string>{}</string><string>serve</string></array>\n</dict></plist>\n",
            world.entry().display(),
            home.display()
        ),
    )
    .unwrap();
    let output = world.run(&["upgrade", B, "--local", "--dry-run"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("does not abandon its process group"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(world.current(), Path::new("releases").join(A));
}

/// A fake systemctl that does what a user manager would with the units
/// `service start` writes: `enable --now` and `restart` start the unit's
/// ExecStart in its own session (a door's stands in as a process holding
/// the door's lock), stopping the one before and waiting for its lock.
const SYSTEMCTL: &str = r#"
import fcntl, json, os, pathlib, shlex, signal, subprocess, sys, time
fake = pathlib.Path(__file__).parent
units = pathlib.Path(os.environ['XDG_CONFIG_HOME']) / 'systemd/user'
with (fake / 'calls.log').open('a') as f:
    f.write(' '.join(sys.argv[1:]) + '\n')
args = sys.argv[2:] if sys.argv[1:2] == ['--user'] else sys.argv[1:]
def held(path):
    try:
        fd = os.open(path, os.O_WRONLY)
    except OSError:
        return False
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return False
    except OSError:
        return True
    finally:
        os.close(fd)
def unit_of(name):
    text = (units / name).read_text()
    lines = text.splitlines()
    argv = shlex.split(next(l for l in lines if l.startswith('ExecStart='))[len('ExecStart='):])
    env = dict(os.environ)
    for l in lines:
        if l.startswith('Environment='):
            k, v = shlex.split(l[len('Environment='):])[0].split('=', 1)
            env[k] = v
    return text, argv, env
def lock_of(text, argv):
    if 'Description=Privateer front door' in text:
        return pathlib.Path(os.environ['PRIVATEER_FLEET']).parent / 'doors' / argv[-1] / 'lock'
    return pathlib.Path(argv[argv.index('--home') + 1]) / 'serve.lock'
def stop(name):
    text, argv, _ = unit_of(name)
    pid = fake / (name + '.pid')
    if pid.exists():
        try:
            os.kill(int(pid.read_text()), signal.SIGTERM)
        except ProcessLookupError:
            pass
        pid.unlink()
    lock = lock_of(text, argv)
    for _ in range(200):
        if not held(lock):
            return
        time.sleep(0.05)
    sys.exit('the unit did not stop')
def start(name):
    text, argv, env = unit_of(name)
    if 'Description=Privateer front door' in text:
        lock = lock_of(text, argv)
        lock.parent.mkdir(parents=True, exist_ok=True)
        argv = [sys.executable, '-c', 'import fcntl,sys,time\nf=open(sys.argv[1],"a")\nfcntl.flock(f,fcntl.LOCK_EX)\ntime.sleep(600)', str(lock)]
    log = open(fake / (name + '.log'), 'a')
    child = subprocess.Popen(argv, env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
    (fake / (name + '.pid')).write_text(str(child.pid))
if args[:1] == ['restart']:
    if (fake / ('fail-' + args[1])).exists():
        sys.exit('injected restart failure')
    stop(args[1]); start(args[1])
elif args[:2] == ['enable', '--now']:
    start(args[2])
elif args[:2] == ['disable', '--now']:
    stop(args[2])
"#;

#[test]
fn units_get_the_safe_stop_policy_then_dispatchers_restart_before_doors() {
    let world = World::new(Fleet::new(now_settings()));
    let fake = world.root().join("fake");
    fs::create_dir_all(&fake).unwrap();
    executable(
        &fake.join("systemctl"),
        &format!("#!{}\n{SYSTEMCTL}", support::python().display()),
    );
    let systemd = |command: &mut Command| {
        command.env("PRIVATEER_SERVICE", "systemd");
    };
    let mut start = world.command(&["service", "start"]);
    systemd(&mut start);
    let output = start.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    until("the dispatcher unit", || world.running());
    let units = world.root().join("config/systemd/user");
    let dispatcher_unit = fs::read_dir(&units)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "service"))
        .unwrap();
    // A unit from before the safe stop policy, still through current.
    let generated = fs::read_to_string(&dispatcher_unit).unwrap();
    assert!(generated.contains(&format!("ExecStart=\"{}\"", world.entry().display())));
    fs::write(
        &dispatcher_unit,
        generated.replace("KillMode=process\n", "KillMode=control-group\n"),
    )
    .unwrap();

    // A front door, registered and running from its unit.
    let registry = world.root().join("fleet.sqlite");
    let conn = rusqlite::Connection::open(&registry).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS doors(name TEXT PRIMARY KEY, manifest TEXT NOT NULL,
           config TEXT, recipient TEXT, created_ms INTEGER NOT NULL);
         INSERT INTO doors(name, manifest, created_ms) VALUES('tg', '/nonexistent/manifest.json', 0);",
    )
    .unwrap();
    let short: String = Sha256::digest(registry.to_string_lossy().as_bytes())
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect();
    let door_unit = format!("privateer-door-tg-{short}.service");
    fs::write(
        units.join(&door_unit),
        format!(
            "[Unit]\nDescription=Privateer front door tg\n\n[Service]\nExecStart=\"{}\" door run tg\nKillMode=control-group\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n",
            world.entry().display()
        ),
    )
    .unwrap();
    let door_lock = world.root().join("doors/tg/lock");
    let started = Command::new(fake.join("systemctl"))
        .args(["--user", "enable", "--now", &door_unit])
        .env("XDG_CONFIG_HOME", world.root().join("config"))
        .env("PRIVATEER_FLEET", &registry)
        .status()
        .unwrap();
    assert!(started.success());
    until("the door", || held(&door_lock));

    // An MCP broker instance whose installed copy is another build.
    let broker = world.data().join("mcp-broker");
    fs::create_dir_all(broker.join("bin")).unwrap();
    executable(&broker.join("bin/privateer-mcp-broker"), "#!/bin/sh\n");
    fs::write(broker.join("web.json.sha256"), "0\n").unwrap();

    // The plan names both, changing nothing.
    let mut dry = world.command(&["upgrade", B, "--local", "--dry-run"]);
    systemd(&mut dry);
    let plan: Value = serde_json::from_slice(&dry.output().unwrap().stdout).unwrap();
    assert_eq!(plan["dispatchers"][0]["name"], "infra", "{plan}");
    assert_eq!(plan["dispatchers"][0]["managed"], "systemd", "{plan}");
    assert_eq!(plan["doors"][0]["name"], "door:tg", "{plan}");
    assert_eq!(plan["stop_policy"][0], json!(dispatcher_unit), "{plan}");
    assert_eq!(world.current(), Path::new("releases").join(A));

    fs::write(fake.join("calls.log"), "").unwrap();
    let mut upgrade = world.command(&["upgrade", B, "--local"]);
    systemd(&mut upgrade);
    let output = upgrade.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let round = world.round(&output);
    assert_eq!(round["outcome"], "switched", "{round}");
    let calls: Vec<String> = fs::read_to_string(fake.join("calls.log"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let dispatcher_name = dispatcher_unit.file_name().unwrap().to_string_lossy();
    assert_eq!(
        calls,
        [
            "--user daemon-reload".to_owned(),
            format!("--user restart {dispatcher_name}"),
            format!("--user restart {door_unit}"),
        ]
    );
    let fixed = fs::read_to_string(&dispatcher_unit).unwrap();
    assert!(fixed.contains("\nKillMode=process\n") && !fixed.contains("control-group"));
    assert_eq!(world.dispatcher_commit().as_deref(), Some(B));
    assert!(held(&door_lock));
    let summary: Vec<&str> = round["summary"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert_eq!(
        summary[0],
        "infra: dispatcher restarted (systemd) on bbbbbbbbbbbb"
    );
    assert_eq!(summary[1], "door:tg: door restarted on bbbbbbbbbbbb");
    assert!(summary[2].starts_with("MCP broker web: "), "{summary:?}");
    assert_eq!(round["brokers"], json!(["web"]));
    // The broker's installed copy is never touched.
    assert_eq!(
        fs::read_to_string(broker.join("bin/privateer-mcp-broker")).unwrap(),
        "#!/bin/sh\n"
    );
    let history = world.history();
    assert_eq!(history[0]["kind"], "upgrade");
    assert_eq!(history[0]["ships"][0]["ship"], "infra");
    assert_eq!(history[0]["ships"][1]["ship"], "door:tg");

    // Units already safe are still read again before any restart, and a
    // door that does not restart ends the round non-zero.
    fs::write(fake.join(format!("fail-{door_unit}")), "").unwrap();
    fs::write(fake.join("calls.log"), "").unwrap();
    let mut back = world.command(&["upgrade", A, "--local"]);
    systemd(&mut back);
    let output = back.output().unwrap();
    assert!(!output.status.success());
    let round = world.round(&output);
    assert_eq!(round["outcome"], "switched_with_failures", "{round}");
    assert!(
        round["error"]
            .as_str()
            .unwrap()
            .contains("door:tg did not restart"),
        "{round}"
    );
    let calls: Vec<String> = fs::read_to_string(fake.join("calls.log"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        calls,
        [
            "--user daemon-reload".to_owned(),
            format!("--user restart {dispatcher_name}"),
            format!("--user restart {door_unit}"),
        ]
    );
    assert_eq!(world.current(), Path::new("releases").join(A));
    assert_eq!(world.dispatcher_commit().as_deref(), Some(A));
    assert_eq!(world.history()[0]["outcome"], "switched_with_failures");
    fs::remove_file(fake.join(format!("fail-{door_unit}"))).unwrap();

    // An unadopted unit, one still naming a release directly, is refused
    // before anything moves.
    let pinned = fixed.replace(
        &world.entry().display().to_string(),
        &world
            .data()
            .join("releases")
            .join(B)
            .join("bin/privateer")
            .display()
            .to_string(),
    );
    fs::write(&dispatcher_unit, pinned).unwrap();
    let mut refused = world.command(&["upgrade", B, "--local"]);
    systemd(&mut refused);
    let output = refused.output().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("run `privateer upgrade adopt` first"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(world.current(), Path::new("releases").join(A));
    fs::write(&dispatcher_unit, &fixed).unwrap();
    let mut stop = world.command(&["service", "stop"]);
    systemd(&mut stop);
    stop.output().unwrap();
}

#[test]
fn upgrade_refuses_an_unfetched_release_and_an_unadopted_machine() {
    let world = World::new(Fleet::new(now_settings()));
    let missing = world.run(&["upgrade", C, "--local"]);
    assert!(!missing.status.success());
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(
        stderr.contains(&format!("run `privateer upgrade fetch {C}` first")),
        "{stderr}"
    );
    let linked = world.run(&["upgrade", B, "--dry-run"]);
    assert!(
        linked.status.success(),
        "{}",
        String::from_utf8_lossy(&linked.stderr)
    );
    let linked: Value = serde_json::from_slice(&linked.stdout).unwrap();
    assert_eq!(linked["machines"], json!(["local"]));
    assert_eq!(world.current(), Path::new("releases").join(A));
    let agent = world
        .command(&["upgrade", B, "--local"])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "infra")
        .output()
        .unwrap();
    assert!(!agent.status.success());
    assert!(String::from_utf8_lossy(&agent.stderr).contains("agents cannot run it"));
    // No current: not adopted.
    fs::copy(
        world.data().join("releases").join(A).join("bin/privateer"),
        world.root().join("privateer"),
    )
    .unwrap();
    fs::remove_file(world.data().join("current")).unwrap();
    let mut unadopted = support::command(world.root().join("privateer"));
    let output = unadopted
        .env("HOME", world.root().join("user"))
        .env("XDG_DATA_HOME", world.root().join("data"))
        .env("XDG_CONFIG_HOME", world.root().join("config"))
        .env("XDG_STATE_HOME", world.root().join("state"))
        .env("XDG_CACHE_HOME", world.root().join("cache"))
        .env("PRIVATEER_RESOURCE_HOME", world.root().join("ledger"))
        .env("PRIVATEER_SERVICE", "none")
        .env_remove("HERDR_ENV")
        .env_remove("HERDR_SOCKET_PATH")
        .env("PRIVATEER_FLEET", world.root().join("fleet.sqlite"))
        .args(["upgrade", B, "--local"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("run `privateer upgrade adopt` first"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fs::symlink_metadata(world.data().join("current")).is_err());
}

#[test]
fn prune_keeps_the_newest_current_previous_and_a_release_a_process_runs() {
    let world = World::new(Fleet::new(now_settings()));
    let releases = world.data().join("releases");
    let commits: Vec<String> = ('0'..='5').map(|d| d.to_string().repeat(40)).collect();
    for commit in &commits {
        fs::create_dir_all(releases.join(commit).join("bin")).unwrap();
        fs::write(releases.join(commit).join("bin/privateer"), "x").unwrap();
    }
    // Published oldest to newest: 0, 1, 2, 3, 4, 5, and A and B before them.
    let base = SystemTime::now() - Duration::from_secs(3600);
    for (i, commit) in [A, B]
        .iter()
        .map(|c| c.to_string())
        .chain(commits.iter().cloned())
        .enumerate()
    {
        fs::File::open(releases.join(commit))
            .unwrap()
            .set_modified(base + Duration::from_secs(60 * i as u64))
            .unwrap();
    }
    // current is 3, reached by an upgrade from 2.
    let current = world.data().join("current");
    fs::remove_file(&current).unwrap();
    symlink(Path::new("releases").join(&commits[3]), &current).unwrap();
    let conn = rusqlite::Connection::open(world.root().join("fleet.sqlite")).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS upgrades(
           seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL, record TEXT NOT NULL);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO upgrades(id, record) VALUES('u1', ?1)",
        [
            json!({"id":"u1","kind":"upgrade","outcome":"switched","from_commit":commits[2],
            "to_commit":commits[3],"actor":"operator","started_ms":1,"completed_ms":2,"ships":[]})
            .to_string(),
        ],
    )
    .unwrap();
    // A process runs release 1.
    let sleeper = releases.join(&commits[1]).join("bin/sleep");
    fs::copy("/bin/sleep", &sleeper).unwrap();
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _sleeping = Kill(Command::new(&sleeper).arg("60").spawn().unwrap());
    // Another works in release 4, as a plugin adapter runs from its
    // directory with relative arguments.
    let _working = Kill(
        Command::new("sleep")
            .arg("60")
            .current_dir(releases.join(&commits[4]).join("bin"))
            .spawn()
            .unwrap(),
    );
    // Not releases: left alone.
    fs::write(releases.join("notes.txt"), "keep").unwrap();
    let elsewhere = world.root().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    symlink(&elsewhere, releases.join("9".repeat(40))).unwrap();

    let prune = |args: &[&str]| {
        let mut command = support::command(BIN);
        command
            .env("HOME", world.root().join("user"))
            .env("XDG_DATA_HOME", world.root().join("data"))
            .env("XDG_CONFIG_HOME", world.root().join("config"))
            .env("XDG_STATE_HOME", world.root().join("state"))
            .env("XDG_CACHE_HOME", world.root().join("cache"))
            .env("PRIVATEER_RESOURCE_HOME", world.root().join("ledger"))
            .env("PRIVATEER_SERVICE", "none")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH")
            .env("PRIVATEER_FLEET", world.root().join("fleet.sqlite"))
            .args(["upgrade", "prune"])
            .args(args);
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let dry = prune(&["--keep", "1", "--dry-run"]);
    let gone = json!([commits[0], B, A]);
    assert_eq!(dry["would_remove"], gone, "{dry}");
    for commit in [A, B] {
        assert!(releases.join(commit).is_dir());
    }
    let report = prune(&["--keep", "1"]);
    assert_eq!(report["removed"], gone, "{report}");
    let kept: Vec<&str> = report["kept"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["commit"].as_str().unwrap())
        .collect();
    assert_eq!(
        kept,
        [
            &commits[5],
            &commits[4],
            &commits[3],
            &commits[2],
            &commits[1]
        ]
    );
    // Kept as in use, newest first: release 4 (its working directory) and
    // release 1 (its executable).
    for index in [1, 4] {
        assert!(
            report["kept"][index]["why"][0]
                .as_str()
                .unwrap()
                .starts_with("in use by pid"),
            "{report}"
        );
    }
    for commit in [&commits[0], &A.to_owned(), &B.to_owned()] {
        assert!(
            fs::symlink_metadata(releases.join(commit)).is_err(),
            "{commit}"
        );
    }
    for commit in [
        &commits[1],
        &commits[2],
        &commits[3],
        &commits[4],
        &commits[5],
    ] {
        assert!(releases.join(commit).is_dir(), "{commit}");
    }
    assert!(releases.join("notes.txt").is_file());
    assert!(elsewhere.is_dir());
    let names: Vec<String> = fs::read_dir(&releases)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(!names.iter().any(|n| n.starts_with(".prune-")), "{names:?}");
}
