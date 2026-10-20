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

/// Whether a process is still running (a zombie awaiting its reaper is not).
fn alive(pid: &str) -> bool {
    fs::read_to_string(format!("/proc/{}/stat", pid.trim()))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .map(|(_, rest)| rest.trim_start().chars().next())
        })
        .flatten()
        .is_some_and(|state| state != 'Z')
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
    let report = laptop.ok(
        "liaison",
        "ship.move",
        json!({"name": "scout", "to": "mac", "check": true}),
    );
    assert_eq!(report["movable"], false, "{report}");
    assert!(report["folder_bytes"].as_u64().unwrap() > 0, "{report}");

    assert!(laptop.moves().is_empty(), "{:?}", laptop.moves());
    assert!(mac.moves().is_empty(), "{:?}", mac.moves());
    assert!(!mac.folder("scout").exists());
    assert!(laptop.run("scout", &["status"], None).status.success());
}
