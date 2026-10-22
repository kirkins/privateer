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
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use support::{BIN, Fleet, now_settings};

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const C: &str = "cccccccccccccccccccccccccccccccccccccccc";
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
        let entry = self.entry();
        assert!(
            entry.starts_with(self.root())
                && self
                    .root()
                    .starts_with(std::env::temp_dir().canonicalize().unwrap()),
            "upgrade runs only from a copy inside a temporary fixture"
        );
        let mut command = support::command(&entry);
        command
            .env("HOME", self.root().join("user"))
            .env("XDG_DATA_HOME", self.root().join("data"))
            .env("XDG_CONFIG_HOME", self.root().join("config"))
            .env("PRIVATEER_FLEET", self.root().join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", self.root().join("ledger"))
            .env("PRIVATEER_SERVICE", "detached")
            .env("PRIVATEER_SYSTEMCTL", self.root().join("fake/systemctl"))
            .env("PRIVATEER_LAUNCHCTL", self.root().join("fake/launchctl"))
            .env_remove("CARGO_HOME")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH")
            .args(["--home"])
            .arg(&self.fleet.home)
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
    fs::read_to_string(fleet.root.join("launches.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
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
            "switch: restart dispatcher",
            "switch: health",
            "rollback: switch",
            "rollback: restart dispatcher",
            "rollback: health"
        ]
    );
    assert_eq!(round["steps"][3]["state"], "failed", "{round}");
    assert_eq!(round["steps"][6]["state"], "ok", "{round}");
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
    let mut refused = world.command(&["upgrade", A, "--local"]);
    systemd(&mut refused);
    let output = refused.output().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("run `privateer upgrade adopt` first"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(world.current(), Path::new("releases").join(B));
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
    let local = world.run(&["upgrade", B]);
    assert!(String::from_utf8_lossy(&local.stderr).contains("pass --local"));
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
    let gone = json!([commits[4], commits[0], B, A]);
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
    assert_eq!(kept, [&commits[5], &commits[3], &commits[2], &commits[1]]);
    assert!(
        report["kept"][3]["why"][0]
            .as_str()
            .unwrap()
            .starts_with("in use by pid"),
        "{report}"
    );
    for commit in [&commits[0], &commits[4], &A.to_owned(), &B.to_owned()] {
        assert!(
            fs::symlink_metadata(releases.join(commit)).is_err(),
            "{commit}"
        );
    }
    for commit in [&commits[1], &commits[2], &commits[3], &commits[5]] {
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
