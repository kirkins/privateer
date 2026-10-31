//! Declared needs: the captain's capability vocabulary, what ships require
//! and machines offer, and the preflight that refuses a move before anything
//! stops. Two machines, each with its own registry and ship homes in separate
//! directories, linked one way through a fake `ssh` that runs the other
//! machine's real gateway subprocess, as the move tests in `machines.rs` do.
mod support;

use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

struct Machine {
    root: PathBuf,
}

impl Machine {
    fn registry(&self) -> PathBuf {
        self.root.join("fleet.sqlite")
    }

    fn folder(&self, ship: &str) -> PathBuf {
        self.root.join(ship)
    }

    fn command(&self, ship: &str, args: &[&str], ssh: Option<&Path>) -> std::process::Command {
        let mut command = support::command(support::BIN);
        command
            .args(args)
            .current_dir(self.folder(ship))
            .env("HOME", &self.root)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            )
            .env("PRIVATEER_FLEET", self.registry())
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env("PRIVATEER_SERVICE", "none")
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH");
        if let Some(ssh) = ssh {
            command.env("PRIVATEER_SSH_EXECUTABLE", ssh);
        }
        command
    }

    fn run(&self, ship: &str, args: &[&str], ssh: Option<&Path>) -> Output {
        self.command(ship, args, ssh).output().unwrap()
    }

    fn json(&self, ship: &str, command: &str, args: Value) -> Output {
        self.run(ship, &[command, "--json", &args.to_string()], None)
    }

    fn ok(&self, ship: &str, command: &str, args: Value) -> Value {
        let output = self.json(ship, command, args);
        assert!(output.status.success(), "{command}: {}", text(&output));
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// A command as the agent `actor`, in its own folder.
    fn as_agent(&self, actor: &str, command: &str, args: Value) -> Output {
        self.run(
            actor,
            &[
                command,
                "--json",
                &args.to_string(),
                "--role",
                "agent",
                "--actor",
                actor,
            ],
            None,
        )
    }

    /// One request to this machine's gateway, with extra environment.
    fn gateway(&self, command: &str, args: Value, env: &[(&str, &str)]) -> Value {
        use std::io::{BufRead, Write};
        let mut child = self
            .command("fitforge", &["gateway"], None)
            .envs(env.iter().copied())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let request = json!({"id": "test", "command": command, "args": args});
        writeln!(child.stdin.take().unwrap(), "{request}").unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(child.wait().unwrap().success());
        serde_json::from_str(&line).unwrap()
    }

    fn ship(&self, name: &str) {
        fs::create_dir_all(self.folder(name)).unwrap();
        let made = self.run(name, &["init", "--ship", name, "--harness", "codex"], None);
        assert!(made.status.success(), "{}", text(&made));
    }

    /// Every move row this machine's registry holds, as `role:state`.
    fn moves(&self) -> Vec<String> {
        let conn = rusqlite::Connection::open(self.registry()).unwrap();
        let mut statement = conn
            .prepare("SELECT role || ':' || state FROM moves ORDER BY role")
            .unwrap();
        statement
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// One dispatcher pass in a ship's home.
    fn pass(&self, ship: &str, ssh: Option<&Path>) -> Value {
        let output = self.run(ship, &["serve", "--json", r#"{"once":true}"#], ssh);
        assert!(output.status.success(), "{}", text(&output));
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .next_back()
            .unwrap_or(Value::Null)
    }

    /// One dispatcher pass in a ship's home, and the placement runner it
    /// started, if any, to its end: approved moves run detached from every
    /// dispatcher.
    fn run_moves(&self, ship: &str, ssh: Option<&Path>) -> Value {
        let ran = self.pass(ship, ssh);
        if let Some(pid) = ran["placement"]["runner"].as_u64() {
            gone(&pid.to_string());
        }
        ran
    }

    /// A command as the agent `actor`, which may reach the other machine.
    fn agent_via(&self, actor: &str, command: &str, args: Value, ssh: &Path) -> Output {
        self.run(
            actor,
            &[
                command,
                "--json",
                &args.to_string(),
                "--role",
                "agent",
                "--actor",
                actor,
            ],
            Some(ssh),
        )
    }

    /// A command as the agent `actor` that must succeed.
    fn agent_ok(&self, actor: &str, command: &str, args: Value) -> Value {
        let output = self.as_agent(actor, command, args);
        assert!(output.status.success(), "{command}: {}", text(&output));
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn conn(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.registry()).unwrap()
    }

    /// A ship's own database.
    fn home_conn(&self, ship: &str) -> rusqlite::Connection {
        rusqlite::Connection::open(self.folder(ship).join(".privateer/privateer.sqlite")).unwrap()
    }

    /// Messages in a ship's home addressed to `address`.
    fn inbox(&self, ship: &str, address: &str) -> Vec<Value> {
        self.ok(ship, "mail.inbox", json!({"address": address}))["messages"]
            .as_array()
            .unwrap()
            .clone()
    }

    /// A script under this machine's root, for a probe to run.
    fn script(&self, name: &str, body: &str) -> String {
        let path = self.root.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A laptop with the liaison and a ship `scout`, a Mac with `fitforge`, both
/// with the first mates' harness installed, and the fake ssh into the Mac.
fn fixture() -> (tempfile::TempDir, Machine, Machine, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let laptop = Machine {
        root: root.join("laptop"),
    };
    let mac = Machine {
        root: root.join("mac"),
    };
    for machine in [&laptop, &mac] {
        let bin = machine.root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("codex"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    laptop.ship("liaison");
    laptop.ship("scout");
    mac.ship("fitforge");
    // A fake ssh into the Mac whose gateway keeps the Mac's own home, ledger
    // and PATH, as a real remote host does.
    let ssh = mac.root.join("ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nfor argument do remote=$argument; done\nenv HOME={root} PATH={root}/bin:/usr/bin:/bin PRIVATEER_FLEET={root}/fleet.sqlite PRIVATEER_RESOURCE_HOME={root}/ledger /bin/sh -c \"$remote\"\n",
            root = mac.root.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    (temp, laptop, mac, ssh)
}

fn link(laptop: &Machine, mac: &Machine, ssh: &Path) {
    let registry = mac.registry();
    let linked = laptop.run(
        "liaison",
        &[
            "machine",
            "add",
            "mac",
            "test-host",
            "--executable",
            support::BIN,
            "--remote-fleet",
            registry.to_str().unwrap(),
            "--as",
            "laptop",
        ],
        Some(ssh),
    );
    assert!(linked.status.success(), "{}", text(&linked));
}

fn refresh(laptop: &Machine, ssh: &Path) {
    let refreshed = laptop.run("liaison", &["machine", "refresh"], Some(ssh));
    assert!(refreshed.status.success(), "{}", text(&refreshed));
}

fn capability(machine: &Machine, args: &[&str]) -> Output {
    let mut argv = vec!["capability"];
    argv.extend_from_slice(args);
    machine.run("liaison", &argv, None)
}

fn define(machine: &Machine, args: &[&str]) {
    let mut argv = vec!["put"];
    argv.extend_from_slice(args);
    let made = capability(machine, &argv);
    assert!(made.status.success(), "{}", text(&made));
}

fn move_ship(laptop: &Machine, args: &[&str], ssh: &Path) -> Output {
    let mut argv = vec!["ship", "move"];
    argv.extend_from_slice(args);
    laptop.run("liaison", &argv, Some(ssh))
}

/// Wait up to two minutes for a process to end.
fn gone(pid: &str) {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while alive(pid) {
        assert!(std::time::Instant::now() < until, "{pid} is still running");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Whether a process is still running (a zombie awaiting its reaper is not),
/// on Linux and macOS alike: macOS has no `/proc`.
fn alive(pid: &str) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", pid.trim()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&output.stdout);
    !stat.trim().is_empty() && !stat.trim().starts_with('Z')
}

#[test]
fn a_ship_needing_what_the_other_machine_lacks_is_refused_before_it_stops() {
    let (_temp, laptop, mac, ssh) = fixture();
    define(&laptop, &["xcode", "--probe-path", "xcodebuild-test"]);
    define(
        &laptop,
        &["phone", "--declared-only", "--note", "a phone on USB"],
    );
    laptop.ok(
        "liaison",
        "ship.requires",
        json!({"name": "scout", "requires": ["xcode", "phone"]}),
    );
    link(&laptop, &mac, &ssh);

    let moved = move_ship(&laptop, &["scout", "--to", "mac"], &ssh);
    assert!(!moved.status.success());
    assert!(
        text(&moved).contains(
            "scout stays on laptop: mac does not offer xcode (xcodebuild-test is not on PATH), phone (not declared here)"
        ),
        "{}",
        text(&moved)
    );
    // Nothing was exported, so its dispatcher was never stopped.
    assert!(laptop.moves().is_empty(), "{:?}", laptop.moves());
    assert!(mac.moves().is_empty(), "{:?}", mac.moves());
    assert!(laptop.run("scout", &["status"], None).status.success());

    // The same the other way: a Mac ship needing what the laptop lacks.
    mac.ship("rover");
    mac.ok(
        "rover",
        "ship.requires",
        json!({"name": "rover", "requires": ["xcode"]}),
    );
    refresh(&laptop, &ssh);
    let back = move_ship(&laptop, &["rover", "--here"], &ssh);
    assert!(!back.status.success());
    assert!(
        text(&back).contains(
            "rover stays on mac: laptop does not offer xcode (xcodebuild-test is not on PATH)"
        ),
        "{}",
        text(&back)
    );
    assert!(mac.moves().is_empty(), "{:?}", mac.moves());
    assert!(laptop.moves().is_empty(), "{:?}", laptop.moves());
    assert!(mac.run("rover", &["status"], None).status.success());

    // Once the Mac offers both, the ship moves, carrying what it requires.
    let declared = mac.ok("fitforge", "machine.offers", json!({"add": ["phone"]}));
    assert!(
        declared["offers"]
            .as_array()
            .unwrap()
            .contains(&json!({"name": "phone", "offered": true, "how": "declared"})),
        "{declared}"
    );
    fs::write(mac.root.join("bin/xcodebuild-test"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(
        mac.root.join("bin/xcodebuild-test"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let moved = move_ship(&laptop, &["scout", "--to", "mac"], &ssh);
    assert!(moved.status.success(), "{}", text(&moved));
    let scout = mac.ok("scout", "ship.show", json!({"name": "scout"}));
    assert_eq!(scout["requires"], json!(["xcode", "phone"]), "{scout}");
}

/// A create that names its needs, on this machine or a linked one, is
/// refused where they are not met. The linked machine is judged first from
/// what it last said it offers, as a placement target is, and live again
/// where the ship is created, so a stale report creates nothing.
#[test]
fn a_ship_is_created_only_where_its_declared_needs_are_met() {
    let (_temp, laptop, mac, ssh) = fixture();
    define(
        &laptop,
        &[
            "xcode",
            "--declared-only",
            "--note",
            "Xcode and its CLI tools",
        ],
    );
    define(
        &laptop,
        &["phone", "--declared-only", "--note", "a phone on USB"],
    );
    link(&laptop, &mac, &ssh);

    let create = |ship: &str, requires: Value| {
        laptop.run(
            "liaison",
            &[
                "ship.create",
                "--json",
                &json!({"name": ship, "machine": "mac", "requires": requires}).to_string(),
            ],
            Some(&ssh),
        )
    };

    // The Mac has offered neither, so the create is refused here, naming
    // every unmet need, without asking it: nothing exists on either side.
    let refused = create("bench", json!(["xcode", "phone"]));
    assert!(!refused.status.success(), "{}", text(&refused));
    assert!(
        text(&refused).contains("cannot create bench on mac: mac does not offer xcode, phone"),
        "{}",
        text(&refused)
    );
    assert!(!mac.folder("bench").exists());
    assert!(!laptop.folder("bench").exists());

    // The Mac declares both: until the laptop hears it again, its last
    // report still refuses, exactly as a placement target is judged.
    mac.ok(
        "fitforge",
        "machine.offers",
        json!({"add": ["xcode", "phone"]}),
    );
    let stale = create("bench", json!(["xcode", "phone"]));
    assert!(!stale.status.success(), "{}", text(&stale));

    // Once the report says so, the create succeeds and the ship's record
    // carries what it requires, here and there.
    refresh(&laptop, &ssh);
    let made = create("bench", json!(["xcode", "phone"]));
    assert!(made.status.success(), "{}", text(&made));
    let made: Value = serde_json::from_slice(&made.stdout).unwrap();
    assert_eq!(made["machine"], "mac", "{made}");
    // The reply echoes what the machine recorded, so the asking side can
    // tell a peer that checked the needs from one too old to.
    assert_eq!(made["requires"], json!(["xcode", "phone"]), "{made}");
    assert_eq!(
        mac.ok("fitforge", "ship.show", json!({"name": "bench"}))["requires"],
        json!(["xcode", "phone"])
    );
    assert_eq!(
        laptop.ok("liaison", "ship.show", json!({"name": "bench"}))["requires"],
        json!(["xcode", "phone"])
    );

    // A report that says the Mac offers what it withholds is caught where
    // the ship is created: the Mac checks the needs live, and nothing is
    // made there. The refusal is recorded here, as any refused create is.
    mac.ok("fitforge", "machine.offers", json!({"withhold": ["xcode"]}));
    let caught = create("gauge", json!(["xcode", "phone"]));
    assert!(!caught.status.success(), "{}", text(&caught));
    assert!(
        text(&caught).contains(
            "machine mac: cannot create gauge on mac: mac does not offer xcode (withheld here)"
        ),
        "{}",
        text(&caught)
    );
    assert!(!mac.folder("gauge").exists());
    let audit = laptop.ok("liaison", "audit.list", json!({"limit": 1000}));
    assert!(
        audit["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "ship.create.failed"
                && e["payload"]["ship"] == "gauge"
                && e["payload"]["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("withheld here"))),
        "{audit}"
    );

    // A machine that has not said what it offers is not one to create on.
    laptop
        .conn()
        .execute("UPDATE machines SET offers=NULL WHERE name='mac'", [])
        .unwrap();
    let unknown = create("mystery", json!(["xcode"]));
    assert!(!unknown.status.success(), "{}", text(&unknown));
    assert!(
        text(&unknown).contains("mac has not said what it offers, so xcode cannot be checked"),
        "{}",
        text(&unknown)
    );
    assert!(!mac.folder("mystery").exists());

    // A create that declares no needs is exactly as before, and a need the
    // vocabulary does not hold is refused before anything, as ship.requires
    // refuses it.
    let plain = laptop.run(
        "liaison",
        &[
            "ship.create",
            "--json",
            r#"{"name":"plain","machine":"mac"}"#,
        ],
        Some(&ssh),
    );
    assert!(plain.status.success(), "{}", text(&plain));
    assert!(mac.folder("plain").join(".privateer").is_dir());
    let typo = create("typo", json!(["chrmoe"]));
    assert!(!typo.status.success(), "{}", text(&typo));
    assert!(
        text(&typo).contains("the fleet's vocabulary has no capability chrmoe"),
        "{}",
        text(&typo)
    );
    assert!(!mac.folder("typo").exists());

    // No refusal names a host path.
    for output in [&refused, &caught, &unknown, &typo] {
        let said = text(output);
        assert!(!said.contains(&laptop.root.display().to_string()), "{said}");
        assert!(!said.contains(&mac.root.display().to_string()), "{said}");
    }
}

#[test]
fn a_ship_created_here_needs_what_this_machine_offers() {
    let (_temp, laptop, _mac, _ssh) = fixture();
    define(&laptop, &["chrome", "--probe-path", "chrome-test"]);
    let create = |ship: &str, requires: Value| {
        laptop.json(
            "liaison",
            "ship.create",
            json!({"name": ship, "requires": requires}),
        )
    };

    // The need is probed live here, as a move's check probes it, before any
    // state exists, and every unmet need is named.
    let refused = create("local", json!(["chrome"]));
    assert!(!refused.status.success(), "{}", text(&refused));
    assert!(
        text(&refused).contains(
            "cannot create local here: this machine does not offer chrome (chrome-test is not on PATH)"
        ),
        "{}",
        text(&refused)
    );
    assert!(!laptop.folder("local").exists());
    assert!(!text(&refused).contains(&laptop.root.display().to_string()));

    // With the need met, the ship is created here and carries it.
    fs::write(laptop.root.join("bin/chrome-test"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(
        laptop.root.join("bin/chrome-test"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let made = create("local", json!(["chrome"]));
    assert!(made.status.success(), "{}", text(&made));
    assert!(laptop.folder("local").join(".privateer").is_dir());
    let made: Value = serde_json::from_slice(&made.stdout).unwrap();
    assert_eq!(made["requires"], json!(["chrome"]), "{made}");
    assert_eq!(
        laptop.ok("liaison", "ship.show", json!({"name": "local"}))["requires"],
        json!(["chrome"])
    );

    // A ship that declares nothing is created exactly as before.
    let plain = create("plain", json!([]));
    assert!(plain.status.success(), "{}", text(&plain));
    assert_eq!(
        laptop.ok("liaison", "ship.show", json!({"name": "plain"}))["requires"],
        json!([])
    );
}

/// The charter check is cheap and comes first: a caller without spawn is
/// refused before any probe runs, and the probe runs only once the caller
/// may create at all.
#[test]
fn a_caller_without_spawn_is_refused_before_any_probe_runs() {
    let (_temp, laptop, _mac, _ssh) = fixture();
    // The probe leaves a trace: it ran exactly when the marker exists.
    let marker = laptop.root.join("probed");
    let probe = laptop.script("bin/probe", &format!("touch '{}'", marker.display()));
    define(&laptop, &["chrome", "--probe-argv", &probe]);
    laptop.ok(
        "scout",
        "charter.put",
        json!({"id": "scout", "actions": ["inspect", "implement"]}),
    );
    let refused = laptop.as_agent(
        "scout",
        "ship.create",
        json!({"name": "child", "requires": ["chrome"]}),
    );
    assert!(!refused.status.success(), "{}", text(&refused));
    assert!(
        text(&refused).contains("ship.create requires spawn"),
        "{}",
        text(&refused)
    );
    assert!(!marker.exists(), "a probe ran before the refusal");
    assert!(!laptop.folder("child").exists());

    // With spawn in hand the probe runs, the need is met, and the ship is
    // created with its needs recorded and echoed.
    laptop.ok(
        "scout",
        "charter.put",
        json!({"id": "scout", "actions": ["inspect", "spawn"]}),
    );
    let made = laptop.as_agent(
        "scout",
        "ship.create",
        json!({"name": "child", "requires": ["chrome"]}),
    );
    assert!(made.status.success(), "{}", text(&made));
    assert!(marker.exists(), "the probe never ran");
    let made: Value = serde_json::from_slice(&made.stdout).unwrap();
    assert_eq!(made["requires"], json!(["chrome"]), "{made}");
    assert_eq!(
        laptop.ok("liaison", "ship.show", json!({"name": "child"}))["requires"],
        json!(["chrome"])
    );
}

/// A linked machine running an older privateer ignores the requires key: it
/// creates the ship without recording or checking the needs, and its reply
/// echoes none of them. The asking side says what happened instead of
/// claiming a checked success, in the reply and in its audit trail.
#[test]
fn an_older_peer_that_drops_the_needs_is_called_out() {
    let (_temp, laptop, mac, ssh) = fixture();
    define(&laptop, &["xcode", "--probe-path", "xcodebuild-test"]);
    // A stand-in for the Mac's older release: its gateway answers
    // ship.create as that release did, with no echo of the needs, and
    // passes every other request through to the real gateway.
    let older = mac.script(
        "older",
        &[
            "line=$(cat)",
            "if printf '%s\\n' \"$line\" | grep -q '\"command\":\"ship.create\"'; then",
            "  id=$(printf '%s\\n' \"$line\" | sed -n 's/.*\"id\":\"\\([^\"]*\\)\".*/\\1/p')",
            "  printf '{\"id\":\"%s\",\"ok\":true,\"result\":{\"ship\":\"bench\",\"folder\":\"/ships/bench\",\"harness\":\"codex\",\"actions\":[\"inspect\"],\"created_by\":\"captain\",\"dispatcher\":\"none\"}}\\n' \"$id\"",
            "else",
            &format!("  printf '%s\\n' \"$line\" | '{}' gateway", support::BIN),
            "fi",
        ]
        .join("\n"),
    );
    let linked = laptop.run(
        "liaison",
        &[
            "machine",
            "add",
            "mac",
            "test-host",
            "--executable",
            &older,
            "--remote-fleet",
            mac.registry().to_str().unwrap(),
            "--as",
            "laptop",
        ],
        Some(&ssh),
    );
    assert!(linked.status.success(), "{}", text(&linked));
    mac.ok("fitforge", "machine.offers", json!({"add": ["xcode"]}));
    refresh(&laptop, &ssh);

    let made = laptop.run(
        "liaison",
        &[
            "ship.create",
            "--json",
            &json!({"name": "bench", "machine": "mac", "requires": ["xcode"]}).to_string(),
        ],
        Some(&ssh),
    );
    assert!(made.status.success(), "{}", text(&made));
    let made: Value = serde_json::from_slice(&made.stdout).unwrap();
    // The reply never claims the needs were recorded: it has no echo and
    // says what the older machine did instead.
    assert!(made.get("requires").is_none(), "{made}");
    let upgrade = made["upgrade"].as_str().unwrap_or_default();
    assert!(
        upgrade.contains("mac created bench without recording or checking its needs (xcode)")
            && upgrade.contains("older privateer")
            && upgrade.contains("upgrade privateer on mac"),
        "{made}"
    );
    // The caveat is a durable fact here, not only a note on the reply.
    let audit = laptop.ok("liaison", "audit.list", json!({"limit": 100}));
    assert!(
        audit["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "ship.created"
                && e["payload"]["ship"] == "bench"
                && e["payload"]["needs_unchecked"] == true),
        "{audit}"
    );
    assert!(
        !upgrade.contains(&laptop.root.display().to_string()),
        "{upgrade}"
    );
    assert!(
        !upgrade.contains(&mac.root.display().to_string()),
        "{upgrade}"
    );
}

#[test]
fn offers_are_declared_withheld_or_probed_and_probes_leave_nothing_behind() {
    let (_temp, laptop, mac, ssh) = fixture();
    let forker = mac.script(
        "forker",
        "sleep 300 &\necho $! > \"$HOME/forker.pid\"\nexit 0",
    );
    let hanger = mac.script(
        "hanger",
        "sleep 300 &\necho $! > \"$HOME/hanger.pid\"\nsleep 300",
    );
    define(&laptop, &["phone", "--declared-only"]);
    define(&laptop, &["shell", "--probe-path", "sh"]);
    define(&laptop, &["root", "--probe-file", "/"]);
    define(&laptop, &["fails", "--probe-argv", "false"]);
    define(&laptop, &["forks", "--probe-argv", &forker]);
    define(&laptop, &["hangs", "--probe-argv", &hanger]);
    link(&laptop, &mac, &ssh);

    // The Mac keeps the laptop's vocabulary, and may not define its own.
    let listed = mac.ok("fitforge", "capability.list", json!({}));
    assert_eq!(listed["defined_on"], "laptop", "{listed}");
    assert_eq!(listed["capabilities"].as_array().unwrap().len(), 6);
    let refused = mac.run(
        "fitforge",
        &["capability", "put", "local", "--declared-only"],
        None,
    );
    assert!(!refused.status.success());
    assert!(
        text(&refused).contains("define it on the linking machine"),
        "{}",
        text(&refused)
    );

    mac.ok(
        "fitforge",
        "machine.offers",
        json!({"add": ["phone"], "withhold": ["shell"]}),
    );
    let started = std::time::Instant::now();
    let checked = mac.gateway(
        "move.check",
        json!({"ship": "nobody",
            "requires": ["phone", "shell", "root", "fails", "forks", "hangs", "typo"]}),
        &[],
    );
    assert_eq!(checked["ok"], true, "{checked}");
    let result = &checked["result"];
    assert_eq!(result["met"], json!(["phone", "root", "forks"]), "{result}");
    assert_eq!(
        result["unmet"],
        json!([
            {"need": "shell", "why": "withheld here"},
            {"need": "fails", "why": "false exited with 1"},
            {"need": "hangs", "why": "probe timed out"},
            {"need": "typo", "why": "not in this machine's vocabulary"},
        ]),
        "{result}"
    );
    // The probes ran together, each within its five seconds.
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    assert!(result["free_disk_bytes"].as_u64().is_some(), "{result}");
    // What either probe forked was killed with its process group.
    std::thread::sleep(std::time::Duration::from_millis(200));
    for name in ["forker.pid", "hanger.pid"] {
        let pid = fs::read_to_string(mac.root.join(name)).unwrap();
        assert!(!alive(&pid), "{name} {pid} outlived its probe");
    }

    // The laptop learns what the Mac offers at the next exchange.
    refresh(&laptop, &ssh);
    let machines = laptop.ok("liaison", "machine.list", json!({}));
    let offers = &machines["machines"][0]["offers"]["offers"];
    assert!(
        offers.as_array().unwrap().contains(&json!("phone")),
        "{machines}"
    );
    assert!(
        !offers.as_array().unwrap().contains(&json!("shell")),
        "{machines}"
    );
}

#[test]
fn a_probe_sees_only_home_and_path_and_a_check_runs_only_stored_probes() {
    let (_temp, laptop, mac, ssh) = fixture();
    let environment = mac.script("environment", "/usr/bin/env > \"$HOME/probe-env\"");
    define(&laptop, &["env", "--probe-argv", &environment]);
    link(&laptop, &mac, &ssh);
    let _ = fs::remove_file(mac.root.join("probe-env"));

    let checked = mac.gateway(
        "move.check",
        json!({"ship": "nobody", "requires": ["env"]}),
        &[("PROBE_SECRET", "leak")],
    );
    assert_eq!(checked["result"]["met"], json!(["env"]), "{checked}");
    let seen = fs::read_to_string(mac.root.join("probe-env")).unwrap();
    let names: Vec<&str> = seen
        .lines()
        .filter_map(|line| line.split_once('=').map(|(name, _)| name))
        .collect();
    assert!(names.contains(&"HOME") && names.contains(&"PATH"), "{seen}");
    // Only what a shell itself adds besides.
    for name in &names {
        assert!(
            ["HOME", "PATH", "PWD", "OLDPWD", "SHLVL", "_"].contains(name),
            "the probe saw {name}: {seen}"
        );
    }
    assert!(
        seen.contains(&format!("HOME={}", mac.root.display())),
        "{seen}"
    );

    // A check names needs; a probe in its arguments is refused or ignored.
    let marker = mac.root.join("ran");
    let touch = json!({"argv": ["/usr/bin/touch", marker]});
    let refused = mac.gateway(
        "move.check",
        json!({"ship": "nobody", "requires": [{"name": "evil", "probe": touch}]}),
        &[],
    );
    assert_eq!(refused["ok"], false, "{refused}");
    let ignored = mac.gateway(
        "move.check",
        json!({"ship": "nobody", "requires": ["evil"], "probe": touch,
            "capabilities": [{"name": "evil", "note": "", "probe": touch}]}),
        &[],
    );
    assert_eq!(
        ignored["result"]["unmet"],
        json!([{"need": "evil", "why": "not in this machine's vocabulary"}]),
        "{ignored}"
    );
    assert!(!marker.exists());
}

#[test]
fn a_ships_requirements_are_checked_and_reach_every_home_and_the_other_machine() {
    let (_temp, laptop, mac, ssh) = fixture();
    define(&laptop, &["chrome", "--probe-path", "chrome"]);
    define(&laptop, &["phone", "--declared-only"]);
    link(&laptop, &mac, &ssh);

    let unknown = laptop.json(
        "liaison",
        "ship.requires",
        json!({"name": "scout", "requires": ["chrmoe"]}),
    );
    assert!(!unknown.status.success());
    assert!(
        text(&unknown).contains("no capability chrmoe"),
        "{}",
        text(&unknown)
    );
    let many: Vec<String> = (0..17).map(|i| format!("t{i}")).collect();
    let too_many = laptop.json(
        "liaison",
        "ship.requires",
        json!({"name": "scout", "requires": many}),
    );
    assert!(!too_many.status.success());
    assert!(
        text(&too_many).contains("at most 16"),
        "{}",
        text(&too_many)
    );
    // The liaison neither created scout nor holds a grant for it.
    let stranger = laptop.as_agent(
        "liaison",
        "ship.requires",
        json!({"name": "scout", "requires": ["chrome"]}),
    );
    assert!(!stranger.status.success());
    assert!(
        text(&stranger).contains("only ships it created or a grant covers"),
        "{}",
        text(&stranger)
    );

    // A ship sets its own.
    let own = laptop.as_agent(
        "scout",
        "ship.requires",
        json!({"name": "scout", "requires": ["chrome", "phone"]}),
    );
    assert!(own.status.success(), "{}", text(&own));
    let here = laptop.ok("scout", "ship.show", json!({"name": "scout"}));
    assert_eq!(here["requires"], json!(["chrome", "phone"]), "{here}");
    let listed = laptop.ok("liaison", "ship.show", json!({"name": "scout"}));
    assert_eq!(listed["requires"], json!(["chrome", "phone"]), "{listed}");
    // A directory refresh keeps it.
    refresh(&laptop, &ssh);
    let ours = laptop.ok("scout", "ship.show", json!({"name": "scout"}));
    assert_eq!(ours["requires"], json!(["chrome", "phone"]), "{ours}");
    let there = mac.ok("fitforge", "ship.show", json!({"name": "scout"}));
    assert_eq!(there["requires"], json!(["chrome", "phone"]), "{there}");
}

#[test]
fn only_the_captain_clears_a_captains_pin_and_a_pinned_ship_moves_only_when_unpinned() {
    let (_temp, laptop, mac, ssh) = fixture();
    link(&laptop, &mac, &ssh);
    laptop.ok(
        "liaison",
        "grant.put",
        json!({"grantee": "liaison", "actions": ["ship.requires"], "ships": ["scout"]}),
    );
    let pinned = laptop.ok(
        "liaison",
        "ship.requires",
        json!({"name": "scout", "pinned": "the signing keychain"}),
    );
    assert_eq!(pinned["pinned"]["by"], "captain", "{pinned}");
    for actor in ["scout", "liaison"] {
        for change in [json!(null), json!("somewhere else")] {
            let refused = laptop.as_agent(
                actor,
                "ship.requires",
                json!({"name": "scout", "pinned": change}),
            );
            assert!(!refused.status.success(), "{actor}");
            assert!(
                text(&refused).contains("the captain pinned this ship; only the captain unpins it"),
                "{}",
                text(&refused)
            );
        }
    }
    // Its requirements may still change around the pin.
    let kept = laptop.as_agent(
        "scout",
        "ship.requires",
        json!({"name": "scout", "requires": []}),
    );
    assert!(kept.status.success(), "{}", text(&kept));
    let scout = laptop.ok("scout", "ship.show", json!({"name": "scout"}));
    assert_eq!(scout["pinned"]["reason"], "the signing keychain", "{scout}");

    // The operator's move shows the pin and needs --unpin; refused, nothing stopped.
    let moved = move_ship(&laptop, &["scout", "--to", "mac"], &ssh);
    assert!(!moved.status.success());
    assert!(
        text(&moved).contains(
            "scout is pinned by captain: the signing keychain; pass --unpin to move it anyway"
        ),
        "{}",
        text(&moved)
    );
    assert!(laptop.moves().is_empty(), "{:?}", laptop.moves());

    // The operator clears it; a grantee's own pin is cleared by the grantee or the ship.
    laptop.ok(
        "liaison",
        "ship.requires",
        json!({"name": "scout", "pinned": null}),
    );
    let by_grantee = laptop.as_agent(
        "liaison",
        "ship.requires",
        json!({"name": "scout", "pinned": "simulator data"}),
    );
    assert!(by_grantee.status.success(), "{}", text(&by_grantee));
    let scout = laptop.ok("scout", "ship.show", json!({"name": "scout"}));
    assert_eq!(scout["pinned"]["by"], "liaison", "{scout}");
    let by_ship = laptop.as_agent(
        "scout",
        "ship.requires",
        json!({"name": "scout", "pinned": null}),
    );
    assert!(by_ship.status.success(), "{}", text(&by_ship));

    // Pinned again by the captain, the move with --unpin clears it on arrival.
    laptop.ok(
        "liaison",
        "ship.requires",
        json!({"name": "scout", "pinned": "the signing keychain"}),
    );
    let moved = move_ship(&laptop, &["scout", "--to", "mac", "--unpin"], &ssh);
    assert!(moved.status.success(), "{}", text(&moved));
    assert!(
        text(&moved).contains("its pin was cleared"),
        "{}",
        text(&moved)
    );
    let arrived = mac.ok("scout", "ship.show", json!({"name": "scout"}));
    assert!(arrived.get("pinned").is_none(), "{arrived}");
}

#[test]
fn move_begin_checks_again_and_undoes_a_move_whose_offer_went_away() {
    let (_temp, laptop, mac, ssh) = fixture();
    // Offered the first time it is asked, then never again.
    let flaky = mac.script(
        "flaky",
        "if [ -e \"$HOME/asked\" ]; then exit 1; fi\ntouch \"$HOME/asked\"",
    );
    define(&laptop, &["flaky", "--probe-argv", &flaky]);
    laptop.ok(
        "liaison",
        "ship.requires",
        json!({"name": "scout", "requires": ["flaky"]}),
    );
    link(&laptop, &mac, &ssh);
    let _ = fs::remove_file(mac.root.join("asked"));

    let moved = move_ship(&laptop, &["scout", "--to", "mac"], &ssh);
    assert!(!moved.status.success());
    assert!(
        text(&moved).contains("this machine does not offer flaky (flaky exited with 1)"),
        "{}",
        text(&moved)
    );
    // It got past the preflight and was exported, then undone: the laptop
    // restarted its dispatcher and the Mac kept nothing.
    let rows = laptop.moves();
    assert!(rows.contains(&"export:aborted".to_owned()), "{rows:?}");
    assert!(rows.contains(&"run:failed".to_owned()), "{rows:?}");
    assert!(!mac.folder("scout").exists());
    assert!(laptop.run("scout", &["status"], None).status.success());
}

#[test]
fn a_resumed_move_skips_the_preflight_but_begin_still_checks() {
    let (_temp, laptop, mac, ssh) = fixture();
    let counted = mac.script("counted", "echo probed >> \"$HOME/probes.log\"");
    define(&laptop, &["counted", "--probe-argv", &counted]);
    laptop.ok(
        "liaison",
        "ship.requires",
        json!({"name": "scout", "requires": ["counted"]}),
    );
    link(&laptop, &mac, &ssh);
    let _ = fs::remove_file(mac.root.join("probes.log"));
    // A move that was started and interrupted before it exported anything.
    let conn = rusqlite::Connection::open(laptop.registry()).unwrap();
    conn.execute(
        "INSERT INTO moves(id, role, ship, state, detail, updated_ms)
         VALUES('interrupted', 'run', 'scout', 'started', ?1, 1)",
        [json!({"to": "mac", "machine": "mac", "peer": "machine-mac"}).to_string()],
    )
    .unwrap();
    drop(conn);

    let moved = move_ship(&laptop, &["scout", "--to", "mac"], &ssh);
    assert!(moved.status.success(), "{}", text(&moved));
    // Probed once, by move.begin; no move.check ran.
    let log = fs::read_to_string(mac.root.join("probes.log")).unwrap();
    assert_eq!(log.lines().count(), 1, "{log}");
}

#[test]
fn a_check_reports_and_changes_nothing() {
    let (_temp, laptop, mac, ssh) = fixture();
    define(&laptop, &["xcode", "--probe-path", "xcodebuild-test"]);
    link(&laptop, &mac, &ssh);

    let checked = move_ship(&laptop, &["scout", "--to", "mac", "--check"], &ssh);
    assert!(checked.status.success(), "{}", text(&checked));
    assert!(
        text(&checked)
            .contains("scout can move from laptop to mac; nothing was stopped or changed"),
        "{}",
        text(&checked)
    );
    laptop.ok(
        "liaison",
        "ship.requires",
        json!({"name": "scout", "requires": ["xcode"]}),
    );
    let checked = move_ship(&laptop, &["scout", "--to", "mac", "--check"], &ssh);
    assert!(checked.status.success(), "{}", text(&checked));
    let said = text(&checked);
    assert!(
        said.contains("scout would not move from laptop to mac"),
        "{said}"
    );
    assert!(
        said.contains("unmet: xcode (xcodebuild-test is not on PATH)"),
        "{said}"
    );
    assert!(said.contains("0 projects"), "{said}");
    let asked = json!({"name": "scout", "to": "mac", "check": true}).to_string();
    let output = laptop.run("liaison", &["ship.move", "--json", &asked], Some(&ssh));
    assert!(output.status.success(), "{}", text(&output));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["movable"], false, "{report}");
    assert!(report["folder_bytes"].as_u64().unwrap() > 0, "{report}");

    assert!(laptop.moves().is_empty(), "{:?}", laptop.moves());
    assert!(mac.moves().is_empty(), "{:?}", mac.moves());
    assert!(!mac.folder("scout").exists());
    assert!(laptop.run("scout", &["status"], None).status.success());
}

// ---- Owner-proposed moves ------------------------------------------------

/// Name `owner` this machine's owner and give it a lasting disk alert: a
/// floor no disk meets, counted as persistent at once.
fn press(machine: &Machine, folder: &str, owner: &str, ssh: Option<&Path>) {
    machine.ship(owner);
    let named = machine.run(folder, &["machine", "owner", owner], None);
    assert!(named.status.success(), "{}", text(&named));
    machine.ok(
        folder,
        "resource.configure",
        json!({"min_free_disk_bytes": 1_u64 << 60, "pressure_after_secs": 0}),
    );
    machine.pass(owner, ssh);
}

/// One row of a registry's proposals: (state, record).
fn proposal(machine: &Machine, id: &str) -> (String, Value) {
    machine
        .conn()
        .query_row(
            "SELECT state, record FROM placement_proposals WHERE id=?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    serde_json::from_str(&r.get::<_, String>(1)?).unwrap(),
                ))
            },
        )
        .unwrap()
}

fn approvals(machine: &Machine) -> Vec<(String, Option<i64>, Option<String>)> {
    let conn = machine.conn();
    let mut statement = conn
        .prepare("SELECT proposal, used_ms, origin FROM placement_approvals ORDER BY proposal")
        .unwrap();
    statement
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// The laptop's owner proposes moving scout to the Mac, which the laptop
/// reaches, and the decision waits for the captain: (proposal, decision).
fn proposed_on_laptop(laptop: &Machine, mac: &Machine, ssh: &Path) -> (String, String) {
    link(laptop, mac, ssh);
    press(laptop, "liaison", "upkeep", Some(ssh));
    refresh(laptop, ssh);
    let proposed = propose_ok(
        laptop,
        json!({"name": "scout", "to": "mac", "why": "the build caches filled the disk"}),
        ssh,
    );
    assert_eq!(proposed["state"], "open", "{proposed}");
    (
        proposed["proposal"].as_str().unwrap().to_owned(),
        proposed["decision"].as_str().unwrap().to_owned(),
    )
}

/// The laptop owner's proposal, which must be accepted.
fn propose_ok(laptop: &Machine, args: Value, ssh: &Path) -> Value {
    let output = laptop.agent_via("upkeep", "ship.propose_move", args, ssh);
    assert!(output.status.success(), "{}", text(&output));
    serde_json::from_slice(&output.stdout).unwrap()
}

fn answer(machine: &Machine, decision: &str, option: &str, ssh: Option<&Path>) {
    let request = json!({"id": decision, "payload": {"answer": {"option_id": option}}}).to_string();
    let answered = machine.run("liaison", &["captain.reply", "--json", &request], ssh);
    assert!(answered.status.success(), "{}", text(&answered));
}

#[test]
fn a_lasting_disk_alert_lets_the_owner_propose_and_the_captains_yes_moves_the_ship_once() {
    let (_temp, laptop, mac, ssh) = fixture();
    link(&laptop, &mac, &ssh);
    press(&laptop, "liaison", "upkeep", Some(&ssh));
    refresh(&laptop, &ssh);

    // The owner sees the pressure, the eligible ships and where they could go.
    let placement = laptop.agent_ok("upkeep", "ship.placement", json!({}));
    assert_eq!(placement["persistent"], true, "{placement}");
    assert_eq!(placement["pressure"][0]["key"], "disk_floor", "{placement}");
    let ship = |name: &str| {
        placement["ships"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["ship"] == name)
            .unwrap()
            .clone()
    };
    assert_eq!(ship("scout")["eligible"], true, "{placement}");
    assert_eq!(ship("scout")["targets"][0]["machine"], "mac", "{placement}");
    assert_eq!(ship("scout")["targets"][0]["eligible"], true, "{placement}");
    assert!(
        ship("liaison")["reasons"]
            .to_string()
            .contains("the captain's home (the liaison)"),
        "{placement}"
    );
    assert!(
        ship("upkeep")["reasons"]
            .to_string()
            .contains("this machine's owner"),
        "{placement}"
    );
    // Only the owner proposes or reads it.
    let stranger = laptop.as_agent("scout", "ship.placement", json!({}));
    assert!(!stranger.status.success());

    let proposed = propose_ok(
        &laptop,
        json!({"name": "scout", "to": "mac", "why": "the build caches filled the disk"}),
        &ssh,
    );
    assert_eq!(proposed["state"], "open", "{proposed}");
    let id = proposed["proposal"].as_str().unwrap().to_owned();
    let decision = proposed["decision"].as_str().unwrap().to_owned();
    // One open proposal per machine.
    let second = laptop.agent_via(
        "upkeep",
        "ship.propose_move",
        json!({"name": "scout", "to": "mac", "why": "again"}),
        &ssh,
    );
    assert!(!second.status.success());
    assert!(
        text(&second).contains("one proposal is open at a time"),
        "{}",
        text(&second)
    );
    // The decision has the runtime's shape.
    let asked = laptop
        .inbox("upkeep", "captain")
        .into_iter()
        .find(|m| m["id"] == decision.as_str())
        .unwrap();
    assert_eq!(asked["kind"], "decision");
    assert!(
        asked["body"]
            .as_str()
            .unwrap()
            .starts_with("**Move scout from laptop to mac?** laptop has been below its disk floor"),
        "{asked}"
    );
    let options: Vec<&str> = asked["payload"]["decision"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap())
        .collect();
    assert_eq!(options, ["move_clean", "move_keep", "not_now", "never"]);
    assert_eq!(asked["payload"]["placement"]["id"], id.as_str());

    // Nothing moves before the yes.
    laptop.pass("liaison", Some(&ssh));
    assert!(laptop.folder("scout").exists());

    answer(&laptop, &decision, "move_clean", Some(&ssh));
    assert_eq!(approvals(&laptop).len(), 1);
    let ran = laptop.run_moves("liaison", Some(&ssh));
    assert!(ran["placement"]["runner"].is_u64(), "{ran}");
    assert_eq!(proposal(&laptop, &id).0, "moved");
    let arrived = mac.ok("scout", "ship.show", json!({"name": "scout"}));
    assert_eq!(arrived["location"], "here", "{arrived}");
    assert!(arrived["last_moved_ms"].as_i64().is_some(), "{arrived}");
    // The old folder was deleted, and the pressure clock starts again.
    assert!(!laptop.folder("scout").exists());
    let (state, record) = proposal(&laptop, &id);
    assert_eq!(state, "moved", "{record}");
    assert_eq!(record["outcome"]["cleaned"], true, "{record}");
    let restarted: Option<i64> = laptop
        .conn()
        .query_row("SELECT restarted_ms FROM machine_pressure", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(restarted.is_some());
    // The owner and the captain heard how it ended, on the decision's thread.
    let told = laptop.inbox("upkeep", "captain");
    let outcome = told
        .iter()
        .find(|m| m["correlation"] == decision.as_str() && m["kind"] == "notice")
        .unwrap();
    assert!(
        outcome["body"]
            .as_str()
            .unwrap()
            .contains("scout moved from laptop to mac"),
        "{outcome}"
    );
    assert!(
        laptop
            .inbox("upkeep", "upkeep")
            .iter()
            .any(|m| m["body"].as_str().unwrap_or("").contains("scout moved")),
    );

    // A replayed approval does not move it twice.
    let moves_before = laptop.moves().len();
    laptop
        .conn()
        .execute("UPDATE placement_approvals SET used_ms=NULL", [])
        .unwrap();
    let ran = laptop.run_moves("liaison", Some(&ssh));
    assert!(ran["placement"].is_null(), "{ran}");
    assert_eq!(laptop.moves().len(), moves_before);
    assert_eq!(proposal(&laptop, &id).0, "moved");
}

/// Wait up to two minutes for a proposal to reach `state`.
fn await_state(machine: &Machine, id: &str, state: &str) -> Value {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let (now, record) = proposal(machine, id);
        if now == state {
            return record;
        }
        assert!(
            std::time::Instant::now() < until,
            "proposal {id} is {now}, not {state}: {record}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[test]
fn the_moved_ships_own_dispatcher_starts_the_move_without_stopping_it() {
    let (_temp, laptop, mac, ssh) = fixture();
    let (id, decision) = proposed_on_laptop(&laptop, &mac, &ssh);
    answer(&laptop, &decision, "move_clean", Some(&ssh));
    // scout's own dispatcher, in the background as a service runs it, is the
    // only one ticking. The move stops it; the move must outlive that.
    let started = laptop
        .command("scout", &["service", "start"], Some(&ssh))
        .env("PRIVATEER_SERVICE", "detached")
        .output()
        .unwrap();
    assert!(started.status.success(), "{}", text(&started));
    let dispatcher =
        fs::read_to_string(laptop.folder("scout").join(".privateer/serve.pid")).unwrap();
    let record = await_state(&laptop, &id, "moved");
    assert_eq!(record["outcome"]["cleaned"], true, "{record}");
    assert!(!alive(&dispatcher), "the move stopped scout's dispatcher");
    let arrived = mac.ok("scout", "ship.show", json!({"name": "scout"}));
    assert_eq!(arrived["location"], "here", "{arrived}");
    assert!(!laptop.folder("scout").exists());
    assert!(approvals(&laptop)[0].1.is_some(), "the approval was used");
}

#[test]
fn a_move_a_crash_interrupted_is_closed_and_the_captain_told_how_to_finish_it() {
    let (_temp, laptop, mac, ssh) = fixture();
    let (id, decision) = proposed_on_laptop(&laptop, &mac, &ssh);
    answer(&laptop, &decision, "move_keep", Some(&ssh));
    // What a runner leaves when the machine goes down mid-move: the approval
    // taken, the proposal moving, and nothing holding the placement lock.
    let conn = laptop.conn();
    conn.execute("UPDATE placement_approvals SET used_ms=1", [])
        .unwrap();
    conn.execute(
        "UPDATE placement_proposals SET state='moving',
           record=json_set(record,'$.state','moving','$.moving_ms',1) WHERE id=?1",
        [&id],
    )
    .unwrap();
    let held = laptop.agent_ok("upkeep", "ship.placement", json!({}))["holds"].to_string();
    assert!(held.contains("is moving"), "{held}");

    let ran = laptop.pass("liaison", Some(&ssh));
    assert_eq!(
        ran["placement"]["interrupted"][0]["state"], "interrupted",
        "{ran}"
    );
    assert!(ran["placement"]["runner"].is_null(), "{ran}");
    let (state, record) = proposal(&laptop, &id);
    assert_eq!(state, "interrupted", "{record}");
    assert_eq!(
        record["outcome"]["finish"], "privateer ship move scout --to mac",
        "{record}"
    );
    // The captain hears it on the decision's thread, with the command.
    let told = laptop
        .inbox("upkeep", "captain")
        .into_iter()
        .find(|m| m["correlation"] == decision.as_str() && m["kind"] == "notice")
        .unwrap();
    let body = told["body"].as_str().unwrap();
    assert!(body.contains("was interrupted"), "{told}");
    assert!(
        body.contains("`privateer ship move scout --to mac` finishes the move"),
        "{told}"
    );
    assert_eq!(told["urgency"], "high", "{told}");
    // It no longer holds proposals as open; it cools down as a refusal does.
    let held = laptop.agent_ok("upkeep", "ship.placement", json!({}))["holds"].to_string();
    assert!(!held.contains("one proposal is open"), "{held}");
    assert!(held.contains("refused when it ran"), "{held}");
    // And it is closed once.
    let again = laptop.pass("liaison", Some(&ssh));
    assert!(again["placement"].is_null(), "{again}");
}

#[test]
fn a_linked_machines_proposal_waits_for_the_linking_machines_check_and_keeps_the_folder() {
    let (_temp, laptop, mac, ssh) = fixture();
    mac.ship("rover");
    link(&laptop, &mac, &ssh);
    press(&mac, "fitforge", "upkeep", None);
    // The laptop's exchange tells the Mac its room and what it calls the Mac.
    refresh(&laptop, &ssh);
    let placement = mac.agent_ok("upkeep", "ship.placement", json!({}));
    let rover = placement["ships"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["ship"] == "rover")
        .unwrap()
        .clone();
    assert_eq!(rover["targets"][0]["machine"], "laptop", "{placement}");
    assert_eq!(rover["targets"][0]["runs_move"], "laptop", "{placement}");
    assert_eq!(rover["targets"][0]["eligible"], true, "{placement}");

    let proposed = mac.agent_ok(
        "upkeep",
        "ship.propose_move",
        json!({"name": "rover", "to": "laptop", "why": "simulator images filled the disk"}),
    );
    assert_eq!(proposed["state"], "checking", "{proposed}");
    let id = proposed["proposal"].as_str().unwrap().to_owned();
    assert!(proposed["decision"].is_null());

    // The laptop learns of it and checks on its next pass; the answer goes
    // back on the one after, and only then is the decision written.
    let collect = || {
        let collected = laptop.run("liaison", &["machine", "collect"], Some(&ssh));
        assert!(collected.status.success(), "{}", text(&collected));
    };
    collect();
    assert_eq!(proposal(&mac, &id).0, "checking");
    collect();
    let (state, record) = proposal(&mac, &id);
    assert_eq!(state, "open", "{record}");
    assert_eq!(record["from"], "mac", "{record}");
    let decision = record["decision"].as_str().unwrap().to_owned();
    // It reached the captain's desk on the laptop.
    assert!(
        laptop
            .inbox("liaison", "captain")
            .iter()
            .any(|m| m["id"] == decision.as_str())
    );

    answer(&laptop, &decision, "move_keep", Some(&ssh));
    laptop.run_moves("liaison", Some(&ssh));
    assert_eq!(proposal(&laptop, &id).0, "moved");
    let arrived = laptop.ok("rover", "ship.show", json!({"name": "rover"}));
    assert_eq!(arrived["location"], "here", "{arrived}");
    // The Mac learns how it ended, keeps the folder and holds proposals.
    collect();
    let (state, record) = proposal(&mac, &id);
    assert_eq!(state, "moved", "{record}");
    assert!(mac.folder("rover").exists());
    let placement = mac.agent_ok("upkeep", "ship.placement", json!({}));
    assert!(
        placement["holds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h.as_str().unwrap().contains("clean-moved rover")),
        "{placement}"
    );
    let cleaned = mac.run("fitforge", &["ship", "clean-moved", "rover"], None);
    assert!(cleaned.status.success(), "{}", text(&cleaned));
    assert!(text(&cleaned).contains("is deleted"), "{}", text(&cleaned));
    assert!(!mac.folder("rover").exists());
    let placement = mac.agent_ok("upkeep", "ship.placement", json!({}));
    assert!(
        placement["holds"].as_array().unwrap().is_empty(),
        "{placement}"
    );
}

#[test]
fn clean_moved_finds_the_set_aside_folder_through_a_link_and_finishes_a_partial_delete() {
    let (_temp, laptop, mac, ssh) = fixture();
    mac.ship("rover");
    link(&laptop, &mac, &ssh);
    refresh(&laptop, &ssh);
    let moved = move_ship(&laptop, &["rover", "--here"], &ssh);
    assert!(moved.status.success(), "{}", text(&moved));
    let id: String = mac
        .conn()
        .query_row("SELECT id FROM moves WHERE role='export'", [], |r| r.get(0))
        .unwrap();
    let short = &id[..8];
    // Since then the ship moved back into its folder, which set the left
    // folder aside; here that copy is a link to where it really is.
    let real = mac.root.join("elsewhere");
    fs::rename(mac.folder("rover"), &real).unwrap();
    let aside = mac.root.join(format!("rover.left-{short}"));
    std::os::unix::fs::symlink(&real, &aside).unwrap();
    fs::create_dir_all(mac.folder("rover").join("projects")).unwrap();
    fs::create_dir_all(real.join("projects")).unwrap();
    fs::write(real.join("projects/cache.bin"), vec![0u8; 64 * 1024]).unwrap();
    // A directory the deletion cannot empty stops it partway.
    let stuck = real.join("projects/stuck");
    fs::create_dir_all(&stuck).unwrap();
    fs::write(stuck.join("file"), "x").unwrap();
    fs::set_permissions(&stuck, fs::Permissions::from_mode(0o500)).unwrap();
    let failed = mac.run("fitforge", &["ship", "clean-moved", "rover"], None);
    assert!(!failed.status.success(), "{}", text(&failed));
    let renamed = mac.root.join(format!("elsewhere.cleaning-{short}"));
    assert!(renamed.is_dir() && !real.exists() && !aside.exists());
    fs::set_permissions(
        renamed.join("projects/stuck"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    // The next clean finishes it, though its marker may be gone.
    let cleaned = mac.ok("fitforge", "ship.clean_moved", json!({"name": "rover"}));
    assert_eq!(cleaned["cleaned"], true, "{cleaned}");
    assert!(cleaned["freed_bytes"].as_u64().unwrap() > 0, "{cleaned}");
    assert!(!renamed.exists());
    // The ship's folder is kept.
    assert!(mac.folder("rover").join("projects").is_dir());
    let again = mac.json("fitforge", "ship.clean_moved", json!({"name": "rover"}));
    assert!(!again.status.success(), "{}", text(&again));
}

#[test]
fn pressure_lasts_only_while_raised_and_stays_with_the_machine() {
    let (_temp, laptop, mac, ssh) = fixture();
    link(&laptop, &mac, &ssh);
    laptop.ship("upkeep");
    laptop.run("liaison", &["machine", "owner", "upkeep"], None);
    laptop.ok(
        "liaison",
        "resource.configure",
        json!({"min_free_disk_bytes": 1_u64 << 60}),
    );
    laptop.pass("upkeep", Some(&ssh));
    let placement = laptop.agent_ok("upkeep", "ship.placement", json!({}));
    assert_eq!(placement["pressure"][0]["key"], "disk_floor", "{placement}");
    assert_eq!(placement["persistent"], false, "{placement}");
    let refused = laptop.agent_via(
        "upkeep",
        "ship.propose_move",
        json!({"name": "scout", "to": "mac", "why": "full"}),
        &ssh,
    );
    assert!(
        text(&refused).contains("not under persistent disk pressure"),
        "{}",
        text(&refused)
    );

    // Held four hours (backdated), it is persistent, and the owner is told once.
    let backdate = |conn: &rusqlite::Connection| {
        conn.execute(
            "UPDATE machine_pressure SET raised_ms=raised_ms-5*3600*1000",
            [],
        )
        .unwrap();
    };
    backdate(&laptop.conn());
    laptop.pass("upkeep", Some(&ssh));
    laptop.pass("upkeep", Some(&ssh));
    let lasting: Vec<Value> = laptop
        .inbox("upkeep", "upkeep")
        .into_iter()
        .filter(|m| m["payload"]["lasting"] == true)
        .collect();
    assert_eq!(lasting.len(), 1, "{lasting:?}");
    assert!(
        lasting[0]["body"]
            .as_str()
            .unwrap()
            .contains("This has lasted 5 hours; `ship.placement` lists ships"),
        "{lasting:?}"
    );
    assert_eq!(
        laptop.agent_ok("upkeep", "ship.placement", json!({}))["persistent"],
        true
    );

    // The owner going away and coming back leaves the clock as it was.
    let raised = |conn: &rusqlite::Connection| -> i64 {
        conn.query_row("SELECT raised_ms FROM machine_pressure", [], |r| r.get(0))
            .unwrap()
    };
    let before = raised(&laptop.conn());
    laptop.run("liaison", &["machine", "owner", "--clear"], None);
    laptop.pass("upkeep", Some(&ssh));
    laptop.run("liaison", &["machine", "owner", "upkeep"], None);
    laptop.pass("upkeep", Some(&ssh));
    assert_eq!(raised(&laptop.conn()), before);

    // Cleared and raised again, it starts over.
    laptop.ok(
        "liaison",
        "resource.configure",
        json!({"min_free_disk_bytes": 0}),
    );
    laptop.pass("upkeep", Some(&ssh));
    let gone: i64 = laptop
        .conn()
        .query_row("SELECT COUNT(*) FROM machine_pressure", [], |r| r.get(0))
        .unwrap();
    assert_eq!(gone, 0);
    laptop.ok(
        "liaison",
        "resource.configure",
        json!({"min_free_disk_bytes": 1_u64 << 60}),
    );
    laptop.pass("upkeep", Some(&ssh));
    assert!(raised(&laptop.conn()) > before + 4 * 3600 * 1000);
    assert_eq!(
        laptop.agent_ok("upkeep", "ship.placement", json!({}))["persistent"],
        false
    );
}

#[test]
fn each_eligibility_rule_keeps_a_ship_from_being_proposed() {
    let (_temp, laptop, mac, ssh) = fixture();
    link(&laptop, &mac, &ssh);
    press(&laptop, "liaison", "upkeep", Some(&ssh));
    let now = || -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    };
    let duty = |ship: &str| -> String {
        laptop.ok(ship, "ship.show", json!({"name": ship}))["first_mate"]["duty_task"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let rules: &[(&str, &str)] = &[
        ("woke", "its first mate woke within the last hour"),
        ("working", "of its tasks running"),
        ("queued", "scheduled work ready or running"),
        ("deferred", "deferred tasks due within a day"),
        ("pinned", "pinned by captain"),
        ("arrived", "moved here"),
        ("cooling", "refused when it ran"),
    ];
    for (ship, _) in rules {
        laptop.ship(ship);
    }
    laptop
        .home_conn("woke")
        .execute(
            "INSERT INTO wakes(id, ship, focus, through_seq, mode, status, created_ms)
             VALUES('w', 'woke', 'duty', 0, 'headless', 'finished', ?1)",
            [now()],
        )
        .unwrap();
    laptop
        .home_conn("working")
        .execute(
            "UPDATE tasks SET status='running' WHERE id=?1",
            [duty("working")],
        )
        .unwrap();
    for (ship, state, not_before) in [
        ("queued", "ready", 0),
        ("deferred", "waiting", now() + 3_600_000),
    ] {
        let conn = laptop.home_conn(ship);
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS task_schedule(task_id TEXT PRIMARY KEY REFERENCES tasks(id),record TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_schedule(task_id, record) VALUES(?1, ?2)",
            [
                duty(ship),
                json!({"owner": ship, "state": state, "not_before_ms": not_before}).to_string(),
            ],
        )
        .unwrap();
    }
    laptop.ok(
        "liaison",
        "ship.requires",
        json!({"name": "pinned", "pinned": "the signing keychain"}),
    );
    laptop
        .home_conn("arrived")
        .execute(
            "UPDATE ships SET record=json_set(record,'$.last_moved_ms',?1) WHERE name='arrived'",
            [now() - 3_600_000],
        )
        .unwrap();
    laptop
        .conn()
        .execute(
            "INSERT INTO placement_cooldowns(ship, until_ms) VALUES('cooling', ?1)",
            [now() + 3_600_000],
        )
        .unwrap();

    let placement = laptop.agent_ok("upkeep", "ship.placement", json!({}));
    for (ship, reason) in rules {
        let entry = placement["ships"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["ship"] == *ship)
            .unwrap();
        assert_eq!(entry["eligible"], false, "{ship}: {entry}");
        assert!(
            entry["reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r.as_str().unwrap().contains(reason)),
            "{ship}: {entry}"
        );
        let refused = laptop.agent_via(
            "upkeep",
            "ship.propose_move",
            json!({"name": ship, "to": "mac", "why": "full"}),
            &ssh,
        );
        assert!(!refused.status.success(), "{ship}");
        assert!(
            text(&refused).contains("may not be proposed"),
            "{}",
            text(&refused)
        );
    }
    // scout, idle and unpinned, still is.
    let scout = placement["ships"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["ship"] == "scout")
        .unwrap();
    assert_eq!(scout["eligible"], true, "{scout}");
}

#[test]
fn a_target_under_pressure_or_without_room_or_reached_through_a_third_is_never_listed() {
    let (_temp, laptop, mac, ssh) = fixture();
    link(&laptop, &mac, &ssh);
    press(&laptop, "liaison", "upkeep", Some(&ssh));
    // The Mac under pressure itself.
    mac.ship("macowner");
    mac.run("fitforge", &["machine", "owner", "macowner"], None);
    mac.ok(
        "fitforge",
        "resource.configure",
        json!({"min_free_disk_bytes": 1_u64 << 60}),
    );
    mac.pass("macowner", None);
    refresh(&laptop, &ssh);
    let targets = |laptop: &Machine| {
        laptop.agent_ok("upkeep", "ship.placement", json!({}))["ships"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["ship"] == "scout")
            .unwrap()["targets"]
            .clone()
    };
    let listed = targets(&laptop);
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(listed[0]["eligible"], false, "{listed}");
    let why = listed[0]["why"].to_string();
    assert!(
        why.contains("under disk pressure itself (disk_floor)"),
        "{why}"
    );
    assert!(why.contains("has no room"), "{why}");
    let refused = laptop.agent_via(
        "upkeep",
        "ship.propose_move",
        json!({"name": "scout", "to": "mac", "why": "full"}),
        &ssh,
    );
    assert!(
        text(&refused).contains("mac cannot host scout"),
        "{}",
        text(&refused)
    );
    // Only machines at the other end of a link are listed at all.
    let far = laptop.agent_via(
        "upkeep",
        "ship.propose_move",
        json!({"name": "scout", "to": "studio", "why": "full"}),
        &ssh,
    );
    assert!(
        text(&far).contains("studio is not linked with this machine"),
        "{}",
        text(&far)
    );
}

#[test]
fn not_now_backs_off_and_never_pins_the_ship_for_the_captain() {
    let (_temp, laptop, mac, ssh) = fixture();
    let (id, decision) = proposed_on_laptop(&laptop, &mac, &ssh);
    answer(&laptop, &decision, "not_now", Some(&ssh));
    // The owner's pass reads the answer.
    laptop.pass("upkeep", Some(&ssh));
    let (state, record) = proposal(&laptop, &id);
    assert_eq!(state, "not_now", "{record}");
    let placement = laptop.agent_ok("upkeep", "ship.placement", json!({}));
    assert!(
        placement["holds"][0]
            .as_str()
            .unwrap()
            .contains("not now (1 in a row); nothing is proposed for another"),
        "{placement}"
    );
    // A second "not now" doubles it: an expired proposal counts as one.
    laptop
        .conn()
        .execute("UPDATE placement_holds SET backoff_until_ms=0", [])
        .unwrap();
    let again = propose_ok(
        &laptop,
        json!({"name": "scout", "to": "mac", "why": "still full"}),
        &ssh,
    );
    let second = again["proposal"].as_str().unwrap().to_owned();
    laptop
        .conn()
        .execute(
            "UPDATE placement_proposals SET created_ms=created_ms-49*3600*1000 WHERE id=?1",
            [&second],
        )
        .unwrap();
    laptop.pass("upkeep", Some(&ssh));
    assert_eq!(proposal(&laptop, &second).0, "expired");
    let streak: (i64, i64) = laptop
        .conn()
        .query_row(
            "SELECT not_now, backoff_until_ms FROM placement_holds",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(streak.0, 2);
    // The decision itself was closed as expired.
    let asked = laptop
        .inbox("upkeep", "captain")
        .into_iter()
        .find(|m| m["id"] == again["decision"])
        .unwrap();
    assert_eq!(asked["state"], "expired", "{asked}");

    // "Never this ship" pins it for the captain.
    laptop
        .conn()
        .execute("UPDATE placement_holds SET backoff_until_ms=0", [])
        .unwrap();
    let third = propose_ok(
        &laptop,
        json!({"name": "scout", "to": "mac", "why": "full again"}),
        &ssh,
    );
    answer(
        &laptop,
        third["decision"].as_str().unwrap(),
        "never",
        Some(&ssh),
    );
    laptop.pass("upkeep", Some(&ssh));
    assert_eq!(
        proposal(&laptop, third["proposal"].as_str().unwrap()).0,
        "never"
    );
    let scout = laptop.ok("scout", "ship.show", json!({"name": "scout"}));
    assert_eq!(scout["pinned"]["by"], "captain", "{scout}");
    assert_eq!(scout["pinned"]["reason"], "the captain declined moving it");
    let unpin = laptop.as_agent(
        "scout",
        "ship.requires",
        json!({"name": "scout", "pinned": null}),
    );
    assert!(!unpin.status.success());
    assert!(approvals(&laptop).is_empty(), "no answer here was a yes");
}

#[test]
fn a_ships_reply_to_the_placement_decision_is_no_answer() {
    let (_temp, laptop, mac, ssh) = fixture();
    let (id, decision) = proposed_on_laptop(&laptop, &mac, &ssh);
    // A reply from a ship, however it reached the owner's home, marks the
    // decision answered; only the captain's reply is read as the answer.
    for (n, option) in ["never", "not_now"].into_iter().enumerate() {
        let forged = format!("forged-{n}");
        let home = laptop.home_conn("upkeep");
        home.execute(
            "INSERT INTO mail(id, sender, recipient, kind, correlation, body, payload, urgency,
               created_ms) VALUES(?1, 'scout', 'upkeep', 'reply', ?2, ?3, ?4, 'normal', 1)",
            rusqlite::params![
                forged,
                decision,
                option,
                json!({"answer": {"option_id": option}}).to_string()
            ],
        )
        .unwrap();
        home.execute(
            "UPDATE mail SET state='answered', answered_by=?2 WHERE id=?1",
            [&decision, &forged],
        )
        .unwrap();
        laptop.pass("upkeep", Some(&ssh));
        let (state, record) = proposal(&laptop, &id);
        assert_eq!(state, "open", "{option}: {record}");
    }
    let scout = laptop.ok("scout", "ship.show", json!({"name": "scout"}));
    assert!(scout["pinned"].is_null(), "{scout}");
    let streak: i64 = laptop
        .conn()
        .query_row(
            "SELECT COALESCE(MAX(not_now), 0) FROM placement_holds",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(streak, 0);
    // Unanswered by the captain, it still expires.
    laptop
        .conn()
        .execute(
            "UPDATE placement_proposals SET created_ms=created_ms-49*3600*1000 WHERE id=?1",
            [&id],
        )
        .unwrap();
    laptop.pass("upkeep", Some(&ssh));
    assert_eq!(proposal(&laptop, &id).0, "expired");
}

#[test]
fn an_approval_waits_unused_until_its_proposal_is_runnable() {
    let (_temp, laptop, mac, ssh) = fixture();
    let (id, decision) = proposed_on_laptop(&laptop, &mac, &ssh);
    answer(&laptop, &decision, "move_keep", Some(&ssh));
    // The yes is here before its proposal is runnable, as when a linked
    // machine's proposal has not yet arrived in an open state.
    let set = |state: &str| {
        laptop
            .conn()
            .execute(
                "UPDATE placement_proposals SET state=?2 WHERE id=?1",
                [&id, state],
            )
            .unwrap();
    };
    set("checking");
    let ran = laptop.run_moves("liaison", Some(&ssh));
    assert!(ran["placement"].is_null(), "{ran}");
    assert_eq!(approvals(&laptop)[0].1, None, "the approval is not spent");
    assert!(laptop.moves().is_empty());
    set("open");
    let ran = laptop.run_moves("liaison", Some(&ssh));
    assert!(ran["placement"]["runner"].is_u64(), "{ran}");
    assert_eq!(proposal(&laptop, &id).0, "moved");
    assert!(approvals(&laptop)[0].1.is_some());
}

#[test]
fn an_approved_move_never_resumes_a_move_it_did_not_start() {
    let (_temp, laptop, mac, ssh) = fixture();
    let (id, decision) = proposed_on_laptop(&laptop, &mac, &ssh);
    answer(&laptop, &decision, "move_keep", Some(&ssh));
    // The operator's own move of scout to the Mac, interrupted.
    laptop
        .conn()
        .execute(
            "INSERT INTO moves(id, role, ship, state, detail, updated_ms)
             VALUES('operator-move', 'run', 'scout', 'started', ?1, 1)",
            [json!({"to": "mac", "machine": "mac", "peer": "machine-mac"}).to_string()],
        )
        .unwrap();
    laptop.run_moves("liaison", Some(&ssh));
    let (state, record) = proposal(&laptop, &id);
    assert_eq!(state, "refused", "{record}");
    assert!(
        record["outcome"]["why"]
            .as_str()
            .unwrap()
            .contains("an earlier move of scout (operator-move) is started and unfinished"),
        "{record}"
    );
    assert!(laptop.folder("scout").exists());
    assert_eq!(laptop.moves(), ["run:started"]);
}

#[test]
fn a_linked_machines_proposal_moves_its_ship_only_from_that_machine() {
    let (_temp, laptop, mac, ssh) = fixture();
    mac.ship("rover");
    link(&laptop, &mac, &ssh);
    press(&mac, "fitforge", "upkeep", None);
    refresh(&laptop, &ssh);
    // The Mac's proposal says the ship comes from somewhere else.
    mac.conn()
        .execute(
            "UPDATE machines SET known_as=NULL, calls_here='studio' WHERE name='laptop'",
            [],
        )
        .unwrap();
    let proposed = mac.agent_ok(
        "upkeep",
        "ship.propose_move",
        json!({"name": "rover", "to": "laptop", "why": "full"}),
    );
    let id = proposed["proposal"].as_str().unwrap().to_owned();
    assert_eq!(proposal(&mac, &id).1["from"], "studio");
    for _ in 0..2 {
        let collected = laptop.run("liaison", &["machine", "collect"], Some(&ssh));
        assert!(collected.status.success(), "{}", text(&collected));
    }
    let (state, record) = proposal(&laptop, &id);
    assert_eq!(state, "open", "{record}");
    answer(
        &laptop,
        record["decision"].as_str().unwrap(),
        "move_keep",
        Some(&ssh),
    );
    laptop.run_moves("liaison", Some(&ssh));
    let (state, record) = proposal(&laptop, &id);
    assert_eq!(state, "refused", "{record}");
    assert!(
        record["outcome"]["why"]
            .as_str()
            .unwrap()
            .contains("only a move from mac to this machine (laptop) runs here"),
        "{record}"
    );
    assert!(laptop.moves().is_empty(), "{:?}", laptop.moves());
    assert!(mac.folder("rover").exists());
}

#[test]
fn a_long_move_is_not_reported_lapsed_while_the_mover_says_it_is_moving() {
    let (_temp, laptop, mac, ssh) = fixture();
    mac.ship("rover");
    link(&laptop, &mac, &ssh);
    press(&mac, "fitforge", "upkeep", None);
    refresh(&laptop, &ssh);
    let proposed = mac.agent_ok(
        "upkeep",
        "ship.propose_move",
        json!({"name": "rover", "to": "laptop", "why": "full"}),
    );
    let id = proposed["proposal"].as_str().unwrap().to_owned();
    let collect = || {
        let collected = laptop.run("liaison", &["machine", "collect"], Some(&ssh));
        assert!(collected.status.success(), "{}", text(&collected));
    };
    collect();
    collect();
    // The yes was seen eight hours ago on the Mac, and the laptop has been
    // running the move since.
    let eight_hours_ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        - 8 * 3600 * 1000;
    mac.conn()
        .execute(
            "UPDATE placement_proposals SET state='approved',
               record=json_set(record,'$.state','approved','$.approved_seen_ms',?2) WHERE id=?1",
            rusqlite::params![id, eight_hours_ago],
        )
        .unwrap();
    laptop
        .conn()
        .execute(
            "UPDATE placement_proposals SET state='moving',
               record=json_set(record,'$.state','moving') WHERE id=?1",
            [&id],
        )
        .unwrap();
    collect();
    mac.pass("upkeep", None);
    let (state, record) = proposal(&mac, &id);
    assert_eq!(state, "approved", "{record}");
    assert!(record["moving_seen_ms"].as_i64().is_some(), "{record}");
    // When the laptop stops saying so for seven hours, the move is
    // interrupted, not refused, and the captain is told how to finish it.
    mac.conn()
        .execute(
            "UPDATE placement_proposals SET record=json_set(record,'$.moving_seen_ms',?2) WHERE id=?1",
            rusqlite::params![id, eight_hours_ago],
        )
        .unwrap();
    mac.pass("upkeep", None);
    let (state, record) = proposal(&mac, &id);
    assert_eq!(state, "interrupted", "{record}");
    assert_eq!(
        record["outcome"]["finish"], "privateer ship move rover --here",
        "{record}"
    );
    assert_eq!(record["outcome"]["runs_on"], "laptop", "{record}");
}

#[test]
fn a_changed_lapsed_or_busy_approval_moves_nothing_and_cools_down() {
    let (_temp, laptop, mac, ssh) = fixture();
    let (id, decision) = proposed_on_laptop(&laptop, &mac, &ssh);
    answer(&laptop, &decision, "move_clean", Some(&ssh));
    // The proposal changed after the yes: a different check answer in its record.
    laptop
        .conn()
        .execute(
            "UPDATE placement_proposals SET record=json_set(record,'$.check.met',json('[\"forged\"]'))",
            [],
        )
        .unwrap();
    laptop.run_moves("liaison", Some(&ssh));
    assert!(laptop.folder("scout").exists());
    assert!(laptop.moves().is_empty(), "{:?}", laptop.moves());
    let (state, record) = proposal(&laptop, &id);
    assert_eq!(state, "refused");
    assert!(
        record["outcome"]["why"]
            .as_str()
            .unwrap()
            .contains("no longer matches what the captain approved"),
        "{record}"
    );
    // The ship cools down a day, the machine six hours, without backoff.
    let placement = laptop.agent_ok("upkeep", "ship.placement", json!({}));
    let holds = placement["holds"].to_string();
    assert!(holds.contains("refused when it ran"), "{holds}");
    assert!(!holds.contains("not now"), "{holds}");
    // The captain was told why, on the decision's thread.
    assert!(
        laptop
            .inbox("upkeep", "captain")
            .iter()
            .any(|m| m["correlation"] == decision.as_str()
                && m["body"].as_str().unwrap().contains("scout was not moved"))
    );
}

#[test]
fn a_lapsed_yes_or_a_ship_that_became_busy_is_not_moved() {
    let (_temp, laptop, mac, ssh) = fixture();
    let (id, decision) = proposed_on_laptop(&laptop, &mac, &ssh);
    answer(&laptop, &decision, "move_keep", Some(&ssh));
    laptop
        .conn()
        .execute("UPDATE placement_approvals SET expires_ms=1", [])
        .unwrap();
    laptop.run_moves("liaison", Some(&ssh));
    let (state, record) = proposal(&laptop, &id);
    assert_eq!(state, "refused");
    assert!(
        record["outcome"]["why"]
            .as_str()
            .unwrap()
            .contains("lapsed"),
        "{record}"
    );
    assert!(laptop.folder("scout").exists());

    // Another proposal, approved, then the ship's first mate wakes.
    let (_temp, laptop, mac, ssh) = fixture();
    let (id, decision) = proposed_on_laptop(&laptop, &mac, &ssh);
    answer(&laptop, &decision, "move_keep", Some(&ssh));
    laptop
        .home_conn("scout")
        .execute(
            "INSERT INTO wakes(id, ship, focus, through_seq, mode, status, created_ms)
             VALUES('w', 'scout', 'duty', 0, 'headless', 'running', 1)",
            [],
        )
        .unwrap();
    laptop.run_moves("liaison", Some(&ssh));
    let (state, record) = proposal(&laptop, &id);
    assert_eq!(state, "refused", "{record}");
    assert!(
        record["outcome"]["why"]
            .as_str()
            .unwrap()
            .contains("scout is no longer eligible: not idle"),
        "{record}"
    );
    assert!(laptop.moves().is_empty(), "{:?}", laptop.moves());
}

#[test]
fn only_the_operators_own_reply_or_the_desks_exchange_approves() {
    let (_temp, laptop, mac, ssh) = fixture();
    let (_, decision) = proposed_on_laptop(&laptop, &mac, &ssh);
    // A ship cannot write a decision that looks like a proposal, nor speak
    // as the captain.
    let forged = laptop.as_agent(
        "scout",
        "mail.send",
        json!({"from": "scout", "to": "captain", "kind": "decision", "body": "move me",
            "payload": {"placement": {"id": "x"}, "decision": {"question": "q", "options": [
                {"id": "move_clean", "label": "a", "what_happens": "b"},
                {"id": "not_now", "label": "c", "what_happens": "d"}]}}}),
    );
    assert!(
        text(&forged).contains("payload.placement is set by the runtime"),
        "{}",
        text(&forged)
    );
    let as_captain = laptop.as_agent(
        "scout",
        "mail.send",
        json!({"from": "captain", "to": "upkeep", "kind": "reply", "correlation": decision,
            "payload": {"answer": {"option_id": "move_clean"}}}),
    );
    assert!(!as_captain.status.success());
    assert!(approvals(&laptop).is_empty());

    // An approval the Mac holds is ignored by the laptop: the desk is here.
    let insert = |conn: &rusqlite::Connection, id: &str| {
        conn.execute(
            "INSERT INTO placement_approvals(proposal, ship, from_machine, to_machine, binding,
               cleanup, approved_ms, expires_ms) VALUES(?1, 'scout', 'laptop', 'mac', ?2,
               'move_clean', ?3, ?4)",
            rusqlite::params![id, "a".repeat(64), i64::MAX / 4, i64::MAX / 2],
        )
        .unwrap();
    };
    insert(&mac.conn(), "from-mac");
    refresh(&laptop, &ssh);
    assert!(
        approvals(&laptop).iter().all(|(id, _, _)| id != "from-mac"),
        "{:?}",
        approvals(&laptop)
    );
    // The Mac takes the laptop's, which holds its desk.
    insert(&laptop.conn(), "from-laptop");
    refresh(&laptop, &ssh);
    assert!(
        approvals(&mac)
            .iter()
            .any(|(id, _, origin)| id == "from-laptop" && origin.as_deref() == Some("laptop")),
        "{:?}",
        approvals(&mac)
    );
    // With the desk moved to the Mac, the laptop takes the Mac's.
    let moved = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved.status.success(), "{}", text(&moved));
    refresh(&laptop, &ssh);
    assert!(
        approvals(&laptop)
            .iter()
            .any(|(id, _, origin)| id == "from-mac" && origin.as_deref() == Some("mac")),
        "{:?}",
        approvals(&laptop)
    );
}

/// A fake ssh into the Mac whose gateway answers `refused` commands as an
/// older privateer would, and passes every other request on.
fn old_mac_ssh(mac: &Machine, refused: &[&str]) -> PathBuf {
    let proxy = mac.root.join("old-ssh");
    fs::write(
        &proxy,
        format!(
            r#"#!/usr/bin/env python3
import json, subprocess, sys
remote = sys.argv[-1]
env = {{"HOME": "{root}", "PATH": "{root}/bin:/usr/bin:/bin",
       "PRIVATEER_FLEET": "{root}/fleet.sqlite", "PRIVATEER_RESOURCE_HOME": "{root}/ledger"}}
refused = {refused:?}
child = subprocess.Popen(["/bin/sh", "-c", remote], stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, env=env, text=True)
for line in sys.stdin:
    request = json.loads(line)
    if request.get("command") in refused:
        print(json.dumps({{"id": request.get("id"), "ok": False,
                          "error": "unknown move command " + request["command"]}}), flush=True)
        continue
    child.stdin.write(line)
    child.stdin.flush()
    print(child.stdout.readline(), end="", flush=True)
child.stdin.close()
child.wait()
"#,
            root = mac.root.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&proxy, fs::Permissions::from_mode(0o755)).unwrap();
    proxy
}

#[test]
fn an_older_peer_is_named_rather_than_shown_as_an_unknown_command() {
    let (_temp, laptop, mac, ssh) = fixture();
    define(&laptop, &["xcode", "--declared-only"]);
    link(&laptop, &mac, &ssh);
    let old = old_mac_ssh(&mac, &["move.check", "move.inspect"]);

    // A target too old to check says so, and with no needs the move is only unchecked.
    let checked = move_ship(&laptop, &["scout", "--to", "mac", "--check"], &old);
    assert!(checked.status.success(), "{}", text(&checked));
    assert!(
        text(&checked).contains("unchecked: mac runs an older privateer"),
        "{}",
        text(&checked)
    );
    laptop.ok(
        "liaison",
        "ship.requires",
        json!({"name": "scout", "requires": ["xcode"]}),
    );
    let refused = move_ship(&laptop, &["scout", "--to", "mac"], &old);
    assert!(!refused.status.success());
    let said = text(&refused);
    assert!(
        said.contains("mac runs an older privateer that cannot check what scout requires (xcode); upgrade privateer on mac"),
        "{said}"
    );
    assert!(!said.contains("unknown move command"), "{said}");
    assert!(laptop.moves().is_empty());

    // A ship's machine too old to be asked is reported unchecked, not hidden.
    mac.ship("rover");
    refresh(&laptop, &ssh);
    let back = move_ship(&laptop, &["rover", "--here", "--check"], &old);
    assert!(back.status.success(), "{}", text(&back));
    let said = text(&back);
    assert!(
        said.contains("unchecked: mac cannot be asked before the move"),
        "{said}"
    );
    assert!(said.contains("folder: unknown"), "{said}");
}

#[test]
fn a_creator_cannot_clear_the_captains_pin() {
    let (_temp, laptop, _mac, _ssh) = fixture();
    // The liaison made scout, so it manages scout without a grant.
    laptop
        .conn()
        .execute(
            "UPDATE ships SET created_by='liaison' WHERE name='scout'",
            [],
        )
        .unwrap();
    laptop.ok(
        "liaison",
        "ship.requires",
        json!({"name": "scout", "pinned": "the signing keychain"}),
    );
    let refused = laptop.as_agent(
        "liaison",
        "ship.requires",
        json!({"name": "scout", "pinned": null}),
    );
    assert!(!refused.status.success());
    assert!(
        text(&refused).contains("the captain pinned this ship; only the captain unpins it"),
        "{}",
        text(&refused)
    );
    // Its requirements are still the creator's to change.
    let changed = laptop.as_agent(
        "liaison",
        "ship.requires",
        json!({"name": "scout", "requires": []}),
    );
    assert!(changed.status.success(), "{}", text(&changed));
}
