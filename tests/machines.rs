//! Two machines, each with its own registry and ship homes in separate
//! directories, linked one way through a real subprocess transport: a fake
//! `ssh` that runs the remote command, so the linking side reaches the other
//! machine's gateway exactly as it would over SSH, and the other side never
//! connects back.
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

    /// A privateer command on this machine, in a ship's folder.
    fn run(&self, ship: &str, args: &[&str], ssh: Option<&Path>) -> Output {
        self.command(ship, args, ssh).output().unwrap()
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

    /// One request to this machine's gateway, as a linking machine sends it.
    fn gateway(&self, command: &str, args: Value) -> Value {
        use std::io::{BufRead, Write};
        let mut child = self
            .command("fitforge", &["gateway"], None)
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

    fn ok(&self, ship: &str, command: &str, args: Value) -> Value {
        let output = self.run(ship, &[command, "--json", &args.to_string()], None);
        assert!(output.status.success(), "{command}: {}", text(&output));
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// The same, over a machine link: a command that asks another machine
    /// needs this machine's ssh to reach it.
    fn ok_over(&self, ship: &str, command: &str, args: Value, ssh: &Path) -> Value {
        let output = self.run(ship, &[command, "--json", &args.to_string()], Some(ssh));
        assert!(output.status.success(), "{command}: {}", text(&output));
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn ship(&self, name: &str) {
        fs::create_dir_all(self.folder(name)).unwrap();
        let made = self.run(name, &["init", "--ship", name, "--harness", "codex"], None);
        assert!(made.status.success(), "{}", text(&made));
    }

    /// One dispatcher pass in a ship's home.
    fn pass(&self, ship: &str, ssh: &Path) -> Value {
        let output = self.run(ship, &["serve", "--json", r#"{"once":true}"#], Some(ssh));
        assert!(output.status.success(), "{}", text(&output));
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .next_back()
            .unwrap_or(Value::Null)
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn fixture() -> (tempfile::TempDir, Machine, Machine, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let laptop = Machine {
        root: root.join("laptop"),
    };
    let mac = Machine {
        root: root.join("mac"),
    };
    laptop.ship("liaison");
    mac.ship("fitforge");
    // Like OpenSSH, run the last argument as a shell command line.
    let ssh = root.join("fake-ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\nfor argument do remote=$argument; done\nexec /bin/sh -c \"$remote\"\n",
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    (temp, laptop, mac, ssh)
}

fn link(laptop: &Machine, mac: &Machine, ssh: &Path) -> Output {
    link_as(laptop, "liaison", "laptop", mac, ssh)
}

fn link_as(laptop: &Machine, ship: &str, known_as: &str, mac: &Machine, ssh: &Path) -> Output {
    let registry = mac.registry();
    laptop.run(
        ship,
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
            known_as,
        ],
        Some(ssh),
    )
}

/// A fake ssh into `machine` whose gateway keeps that machine's own home,
/// ledger and PATH, as a real remote host does; the plain fake ssh would
/// pass the caller's environment through. Each connection is logged.
fn remote_ssh(machine: &Machine) -> PathBuf {
    let ssh = machine.root.join("ssh");
    fs::create_dir_all(machine.root.join("bin")).unwrap();
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nfor argument do remote=$argument; done\necho connected >> {root}/ssh.log\nexec env HOME={root} PATH={root}/bin:/usr/bin:/bin PRIVATEER_FLEET={root}/fleet.sqlite PRIVATEER_RESOURCE_HOME={root}/ledger /bin/sh -c \"$remote\"\n",
            root = machine.root.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    ssh
}

fn connections(machine: &Machine) -> usize {
    fs::read_to_string(machine.root.join("ssh.log"))
        .map(|log| log.lines().count())
        .unwrap_or(0)
}

fn messages(machine: &Machine, ship: &str, kind: &str) -> Vec<Value> {
    machine.ok(ship, "mail.inbox", json!({"address": ship}))["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["kind"] == kind)
        .cloned()
        .collect()
}

#[test]
fn a_one_way_link_delivers_requests_and_collects_the_replies() {
    let (_temp, laptop, mac, ssh) = fixture();
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    assert!(text(&linked).contains("fitforge"), "{}", text(&linked));

    // Each side knows the other's ships behind one machine peer.
    let fitforge = laptop.ok("liaison", "ship.show", json!({"name": "fitforge"}));
    assert_eq!(fitforge["location"], json!({"peer": "machine-mac"}));
    let liaison = mac.ok("fitforge", "ship.show", json!({"name": "liaison"}));
    assert_eq!(liaison["location"], json!({"peer": "machine-laptop"}));

    // Laptop to Mac: delivered at once through the Mac's gateway.
    let output = laptop.run(
        "liaison",
        &[
            "mail.send",
            "--json",
            &json!({"from": "liaison", "to": "fitforge", "kind": "request", "body": "Build 482 please"})
                .to_string(),
        ],
        Some(&ssh),
    );
    assert!(output.status.success(), "{}", text(&output));
    let request: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(request["delivery"], "delivered", "{request}");
    let arrived = messages(&mac, "fitforge", "request");
    assert_eq!(arrived.len(), 1);
    assert_eq!(arrived[0]["body"], "Build 482 please");

    // Mac to laptop: the Mac cannot connect, so the reply waits there...
    let reply = mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "liaison", "kind": "reply",
               "correlation": request["id"], "body": "Built and tested"}),
    );
    assert_eq!(reply["delivery"], "pending", "{reply}");
    let flushed = mac.ok("fitforge", "mail.flush", json!({}));
    assert_eq!(flushed["attempted"], 0, "{flushed}");

    // ...until a laptop dispatcher pass collects it.
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["delivered"], 1, "{pass}");
    let replies = messages(&laptop, "liaison", "reply");
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["body"], "Built and tested");
    let answered = laptop.ok("liaison", "mail.show", json!({"id": request["id"]}));
    assert_eq!(answered["state"], "answered", "{answered}");
    let status = mac.ok("fitforge", "status", json!({}));
    assert_eq!(status["undelivered_mail"], 0, "{status}");

    // The next pass leaves the Mac alone until it is due again, and
    // collecting now finds nothing and delivers nothing twice.
    let again = laptop.pass("liaison", &ssh);
    assert_eq!(again["machines"], json!([]), "{again}");
    let collected = laptop.run("liaison", &["machine", "collect"], Some(&ssh));
    assert!(collected.status.success(), "{}", text(&collected));
    assert!(
        text(&collected).contains("\"delivered\": 0"),
        "{}",
        text(&collected)
    );
    assert_eq!(messages(&laptop, "liaison", "reply").len(), 1);
}

#[test]
fn ship_names_stay_unique_across_linked_machines() {
    let (_temp, laptop, mac, ssh) = fixture();
    mac.ship("liaison-mac");
    // A name both machines hold refuses the link on both sides.
    laptop.ship("fitforge-copy");
    fs::create_dir_all(mac.folder("fitforge-copy")).unwrap();
    let clash = mac.run(
        "fitforge-copy",
        &["init", "--ship", "fitforge-copy", "--harness", "codex"],
        None,
    );
    assert!(clash.status.success(), "{}", text(&clash));
    let refused = link(&laptop, &mac, &ssh);
    assert!(!refused.status.success());
    assert!(
        text(&refused).contains("fitforge-copy"),
        "{}",
        text(&refused)
    );
    let unlinked = laptop.ok("liaison", "ship.list", json!({}));
    assert!(!unlinked.to_string().contains("machine-mac"), "{unlinked}");

    // Once linked, neither machine can take a name the other holds.
    let retired = laptop.ok("liaison", "ship.retire", json!({"name": "fitforge-copy"}));
    assert_eq!(retired["ship"], "fitforge-copy", "{retired}");
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    fs::create_dir_all(laptop.folder("fitforge")).unwrap();
    let taken = laptop.run(
        "fitforge",
        &["init", "--ship", "fitforge", "--harness", "codex"],
        None,
    );
    assert!(!taken.status.success());
    assert!(
        text(&taken).contains("linked machine mac"),
        "{}",
        text(&taken)
    );
    fs::create_dir_all(mac.folder("liaison")).unwrap();
    let taken = mac.run(
        "liaison",
        &["init", "--ship", "liaison", "--harness", "codex"],
        None,
    );
    assert!(!taken.status.success());
    assert!(
        text(&taken).contains("linked machine laptop"),
        "{}",
        text(&taken)
    );

    // A ship made later on the Mac reaches the laptop at the next refresh.
    mac.ship("server");
    let refreshed = laptop.run("liaison", &["machine", "refresh"], Some(&ssh));
    assert!(refreshed.status.success(), "{}", text(&refreshed));
    let server = laptop.ok("liaison", "ship.show", json!({"name": "server"}));
    assert_eq!(server["location"], json!({"peer": "machine-mac"}));
}

// A phone's front door: answers every request from a ship on the other
// machine by replying to its sender, as the Telegram door does, and logs
// everything it is sent.
const DOOR: &str = r#"import json, os, sys
log = open(os.environ["FAKE_DOOR_LOG"], "a")
def note(entry):
    log.write(json.dumps(entry) + "\n"); log.flush()
note(json.loads(sys.stdin.readline()))
print(json.dumps({"type": "request", "id": "ships", "command": "ships", "args": {}}), flush=True)
for line in sys.stdin:
    message = json.loads(line)
    note(message)
    if message["type"] == "mail":
        mail = message["mail"]
        print(json.dumps({"type": "delivered", "ref": message["ref"]}), flush=True)
        if mail["kind"] == "request":
            print(json.dumps({"type": "request", "id": "answer", "command": "send", "args": {
                "to": mail["from"], "kind": "reply", "correlation": mail["id"],
                "body": "Yes, ship it"}}), flush=True)
"#;

#[test]
fn the_captain_hears_from_ships_on_the_other_machine_and_answers_them() {
    let (_temp, laptop, mac, ssh) = fixture();
    let old = mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "notice", "body": "from before the link"}),
    );
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    let asked = mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "request", "body": "Upload build 12 to TestFlight?"}),
    );

    // The laptop collects it into the captain's home there; mail written
    // before the link stays on the Mac.
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["delivered"], 1, "{pass}");
    let inbox = laptop.ok("liaison", "mail.inbox", json!({"address": "captain"}));
    let bodies: Vec<&str> = inbox["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["body"].as_str())
        .collect();
    assert_eq!(bodies, ["Upload build 12 to TestFlight?"], "{inbox}");
    assert_ne!(old["id"], asked["id"]);

    // The captain answers from the phone; the answer crosses back and closes
    // the request on both machines.
    let door = laptop.root.join("door");
    fs::create_dir_all(&door).unwrap();
    fs::write(door.join("door.py"), DOOR).unwrap();
    fs::write(
        door.join("manifest.json"),
        json!({"schema_version": 1, "kind": "front-door", "id": "phone",
            "executable": ["python3", "door.py"]})
        .to_string(),
    )
    .unwrap();
    let manifest = door.join("manifest.json");
    let added = laptop.run(
        "liaison",
        &[
            "door",
            "add",
            "phone",
            "--manifest",
            manifest.to_str().unwrap(),
        ],
        None,
    );
    assert!(added.status.success(), "{}", text(&added));
    let log = laptop.root.join("door.jsonl");
    let mut supervisor = support::command(support::BIN)
        .args(["door", "run", "phone"])
        .current_dir(laptop.folder("liaison"))
        .env("HOME", &laptop.root)
        .env("PATH", "/usr/bin:/bin")
        .env("PRIVATEER_FLEET", laptop.registry())
        .env("PRIVATEER_RESOURCE_HOME", laptop.root.join("ledger"))
        .env("PRIVATEER_SERVICE", "none")
        .env("PRIVATEER_SSH_EXECUTABLE", &ssh)
        .env("FAKE_DOOR_LOG", &log)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let answered = loop {
        let request = mac.ok("fitforge", "mail.show", json!({"id": asked["id"]}));
        if request["state"] == "answered" || std::time::Instant::now() > deadline {
            break request;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    let _ = supervisor.kill();
    let _ = supervisor.wait();
    let entries = fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(answered["state"], "answered", "{answered}\n{entries}");
    let replies = messages(&mac, "fitforge", "reply");
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0]["from"], "captain");
    assert_eq!(replies[0]["body"], "Yes, ship it");
    let here = laptop.ok("liaison", "mail.show", json!({"id": asked["id"]}));
    assert_eq!(here["state"], "answered", "{here}");
    // The door lists the other machine's ships beside this one's.
    assert!(
        entries.contains(r#""machine": "mac""#) && entries.contains(r#""name": "fitforge""#),
        "{entries}"
    );

    // Only the operator carries the captain's mail across: an agent cannot.
    let forged = laptop.run(
        "liaison",
        &[
            "peer.call",
            "--json",
            &json!({"name": "machine-mac", "command": "mail.deliver", "args": {
                "id": "forged-captain-mail", "from": "captain", "to": "fitforge",
                "kind": "notice", "body": "forged"}})
            .to_string(),
            "--role",
            "agent",
            "--actor",
            "liaison",
        ],
        Some(&ssh),
    );
    let refused: Value = serde_json::from_slice(&forged.stdout).unwrap_or(Value::Null);
    assert_eq!(refused["ok"], false, "{}", text(&forged));
    assert!(messages(&mac, "fitforge", "notice").is_empty());
}

#[test]
fn a_machines_owner_hears_of_its_problems_and_may_stow_it() {
    let (_temp, laptop, _mac, ssh) = fixture();
    laptop.ship("upkeep");
    let named = laptop.run("liaison", &["machine", "owner", "upkeep"], None);
    assert!(named.status.success(), "{}", text(&named));
    let alerts = |laptop: &Machine| -> Vec<String> {
        messages(laptop, "upkeep", "notice")
            .iter()
            .filter(|m| m["from"] == "crew")
            .filter_map(|m| m["body"].as_str().map(str::to_owned))
            .collect()
    };

    // A disk floor the machine cannot meet reaches the owner once.
    laptop.ok(
        "liaison",
        "resource.configure",
        json!({"min_free_disk_bytes": 1_u64 << 60}),
    );
    laptop.pass("liaison", &ssh);
    assert!(
        alerts(&laptop).is_empty(),
        "only the owner's dispatcher alerts"
    );
    laptop.pass("upkeep", &ssh);
    laptop.pass("upkeep", &ssh);
    let raised = alerts(&laptop);
    assert_eq!(raised.len(), 1, "{raised:?}");
    assert!(
        raised[0].contains("below the machine's floor"),
        "{raised:?}"
    );
    // And again when it clears.
    laptop.ok(
        "liaison",
        "resource.configure",
        json!({"min_free_disk_bytes": 0}),
    );
    laptop.pass("upkeep", &ssh);
    let all = alerts(&laptop);
    assert_eq!(all.len(), 2, "{all:?}");
    assert!(all[1].contains("Cleared"), "{all:?}");

    // The owner may stow the machine, asking in its own name, and resume it.
    let agent = |ship: &str, command: &str| {
        laptop.run(
            ship,
            &[command, "--json", "{}", "--role", "agent", "--actor", ship],
            None,
        )
    };
    let refused = agent("liaison", "stow.start");
    assert!(!refused.status.success(), "{}", text(&refused));
    let stowed = agent("upkeep", "stow.start");
    assert!(stowed.status.success(), "{}", text(&stowed));
    let asked = messages(&laptop, "liaison", "request");
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(asked[0]["from"], "upkeep");
    assert!(
        asked[0]["body"]
            .as_str()
            .unwrap()
            .contains("upkeep, which looks after this machine"),
        "{asked:?}"
    );
    let resumed = agent("upkeep", "stow.resume");
    assert!(resumed.status.success(), "{}", text(&resumed));

    // It cannot end the captain's stow.
    laptop.ok("liaison", "stow.start", json!({}));
    let ended = agent("upkeep", "stow.resume");
    assert!(!ended.status.success());
    assert!(
        text(&ended).contains("only the captain"),
        "{}",
        text(&ended)
    );

    // Nor reach another machine's hold, which the captain's `--all` does.
    let all = laptop.run(
        "upkeep",
        &[
            "stow.resume",
            "--json",
            r#"{"all":true}"#,
            "--role",
            "agent",
            "--actor",
            "upkeep",
        ],
        None,
    );
    assert!(!all.status.success(), "{}", text(&all));
    assert!(text(&all).contains("operator command"), "{}", text(&all));
    laptop.ok("liaison", "stow.resume", json!({}));
}

#[test]
fn a_stow_of_a_linked_machine_reaches_only_that_machines_ships() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    let mac_ssh = remote_ssh(&mac);
    laptop.ship("upkeep");
    let named = laptop.run("liaison", &["machine", "owner", "upkeep"], None);
    assert!(named.status.success(), "{}", text(&named));

    // Only the captain stows a linked machine; the owner stows only its own.
    let owner = laptop.run(
        "upkeep",
        &[
            "stow.start",
            "--json",
            r#"{"machine":"mac"}"#,
            "--role",
            "agent",
            "--actor",
            "upkeep",
        ],
        Some(&mac_ssh),
    );
    assert!(!owner.status.success(), "{}", text(&owner));
    let unknown = laptop.run("liaison", &["stow", "--machine", "nowhere"], Some(&mac_ssh));
    assert!(!unknown.status.success());
    assert!(
        text(&unknown).contains("no linked machine named nowhere"),
        "{}",
        text(&unknown)
    );

    // The captain's stow asks the machine's own ships, in the machine's ledger.
    let stowed = laptop.run("liaison", &["stow", "--machine", "mac"], Some(&mac_ssh));
    assert!(stowed.status.success(), "{}", text(&stowed));
    assert!(
        text(&stowed).contains("Stowing the fleet on mac"),
        "{}",
        text(&stowed)
    );
    let asked = messages(&mac, "fitforge", "request");
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(asked[0]["from"], "captain");
    assert!(messages(&laptop, "liaison", "request").is_empty());
    let local = laptop.ok("liaison", "stow.status", json!({}));
    assert_eq!(local["stow"], Value::Null, "{local}");

    // Status names the machine it describes, and resume lifts the remote hold.
    let status = laptop.run(
        "liaison",
        &["stow", "status", "--machine", "mac"],
        Some(&mac_ssh),
    );
    assert!(
        text(&status).contains(" on mac (held)"),
        "{}",
        text(&status)
    );
    let resumed = laptop.run(
        "liaison",
        &["stow", "resume", "--machine", "mac"],
        Some(&mac_ssh),
    );
    assert!(resumed.status.success(), "{}", text(&resumed));
    assert!(
        text(&resumed).contains("Resumed the fleet on mac"),
        "{}",
        text(&resumed)
    );
    assert_eq!(messages(&mac, "fitforge", "notice").len(), 1);
}

/// A resume is one machine's. A ship on it must never read it as the whole
/// fleet's, so the notice names the machine and lists the linked machines that
/// are still holding their own ships.
#[test]
fn a_resume_notice_lists_a_linked_machine_still_stowed() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    let mac_ssh = remote_ssh(&mac);
    laptop.ship("upkeep");
    let notice = |machine: &Machine, ship: &str| -> String {
        messages(machine, ship, "notice")
            .into_iter()
            .last()
            .map(|m| m["body"].as_str().unwrap_or("").to_owned())
            .unwrap_or_default()
    };

    // The Mac is stowed over the link, then this machine, as on the day.
    assert!(
        laptop
            .run("liaison", &["stow", "--machine", "mac"], Some(&mac_ssh))
            .status
            .success()
    );
    laptop.ok("liaison", "stow.start", json!({}));

    // Resuming this machine alone tells its ships about the Mac.
    let resumed = laptop.run("liaison", &["stow", "resume"], Some(&mac_ssh));
    assert!(resumed.status.success(), "{}", text(&resumed));
    assert!(
        text(&resumed).contains("mac: still stowed"),
        "{}",
        text(&resumed)
    );
    let body = notice(&laptop, "upkeep");
    assert!(body.contains("resumed this machine (laptop)"), "{body}");
    assert!(
        body.contains("Linked machine mac is still stowed"),
        "{body}"
    );
    assert!(!body.contains("the fleet"), "{body}");
    // The Mac has heard nothing: only its own resume may say it runs again.
    assert!(messages(&mac, "fitforge", "notice").is_empty());

    // The claim is checkable in records before anyone repeats it.
    let status = laptop.ok_over("liaison", "stow.status", json!({}), &mac_ssh);
    assert_eq!(status["this_machine"], json!("laptop"), "{status}");
    assert_eq!(
        status["machines"],
        json!([{"machine": "mac", "asked": true, "held": true}]),
        "{status}"
    );
}

/// `--all` lifts the linked machines this one reaches before its own, so the
/// notice this machine's ships read names no machine that is still stowed.
#[test]
fn a_resume_of_all_from_the_reached_side_succeeds() {
    // The Mac lists the laptop as a machine that reaches it and that it
    // cannot ask; `--all` never tries the laptop, so it must not fail on it.
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    mac.ok("fitforge", "stow.start", json!({}));
    let all = mac.run("fitforge", &["stow", "resume", "--all"], None);
    assert!(all.status.success(), "{}", text(&all));
    assert_eq!(
        mac.ok("fitforge", "stow.status", json!({}))["held"],
        false,
        "the Mac runs again"
    );
}

#[test]
fn a_resume_of_all_lifts_the_linked_machines_first() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    let mac_ssh = remote_ssh(&mac);
    laptop.ship("upkeep");
    let notice = |machine: &Machine, ship: &str| -> String {
        messages(machine, ship, "notice")
            .into_iter()
            .last()
            .map(|m| m["body"].as_str().unwrap_or("").to_owned())
            .unwrap_or_default()
    };

    assert!(
        laptop
            .run("liaison", &["stow", "--machine", "mac"], Some(&mac_ssh))
            .status
            .success()
    );
    laptop.ok("liaison", "stow.start", json!({}));
    let all = laptop.run("liaison", &["stow", "resume", "--all"], Some(&mac_ssh));
    assert!(all.status.success(), "{}", text(&all));
    assert!(text(&all).contains("mac: running again"), "{}", text(&all));

    // The Mac's own hold ended before this machine's: its ledger says so.
    let theirs = mac.ok("fitforge", "stow.status", json!({}));
    let ours = laptop.ok_over("liaison", "stow.status", json!({}), &mac_ssh);
    assert_eq!(ours["held"], false, "{ours}");
    let theirs_resumed = theirs["resumed_ms"].as_i64().expect("the Mac resumed");
    let ours_resumed = ours["resumed_ms"].as_i64().expect("this machine resumed");
    assert!(
        theirs_resumed <= ours_resumed,
        "the Mac resumed at {theirs_resumed}, this machine at {ours_resumed}"
    );

    // Neither notice leaves a machine still stowed, and each names its own.
    let body = notice(&laptop, "upkeep");
    assert!(body.contains("resumed this machine (laptop)"), "{body}");
    assert!(!body.contains("is still stowed, so new jobs"), "{body}");
    let theirs_body = notice(&mac, "fitforge");
    assert!(
        theirs_body.contains("resumed this machine ("),
        "{theirs_body}"
    );
    // The Mac cannot ask the laptop, so it says so instead of guessing.
    assert!(
        theirs_body.contains("Linked machine laptop reaches this machine"),
        "{theirs_body}"
    );

    // And it works when only the linked machine is stowed, which is the
    // case the plain `stow resume` refuses here.
    assert!(
        laptop
            .run("liaison", &["stow", "--machine", "mac"], Some(&mac_ssh))
            .status
            .success()
    );
    let refused = laptop.run("liaison", &["stow", "resume"], Some(&mac_ssh));
    assert!(!refused.status.success(), "{}", text(&refused));
    assert!(text(&refused).contains("not stowed"), "{}", text(&refused));
    let all = laptop.run("liaison", &["stow", "resume", "--all"], Some(&mac_ssh));
    assert!(all.status.success(), "{}", text(&all));
    assert_eq!(
        mac.ok("fitforge", "stow.status", json!({}))["held"],
        false,
        "the Mac runs again"
    );

    // `--all` covers every machine this one reaches, so it is refused beside
    // one name, and beside stowing: a machine is stowed one at a time.
    let both = laptop.run(
        "liaison",
        &["stow", "resume", "--all", "--machine", "mac"],
        Some(&mac_ssh),
    );
    assert!(!both.status.success(), "{}", text(&both));
    assert!(text(&both).contains("drop --machine"), "{}", text(&both));
    let stow_all = laptop.run("liaison", &["stow", "--all"], Some(&mac_ssh));
    assert!(!stow_all.status.success(), "{}", text(&stow_all));
    assert!(
        text(&stow_all).contains("for resuming"),
        "{}",
        text(&stow_all)
    );
}

/// `--machine NAME` speaks for that machine and no other: its answer names it
/// and carries none of that machine's own view of the fleet, which here would
/// be read as this machine's.
#[test]
fn a_named_machine_carries_only_that_machines_answer() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    let mac_ssh = remote_ssh(&mac);
    laptop.ship("upkeep");

    let named = |command: &str| -> Value {
        let args = json!({"machine": "mac"}).to_string();
        let out = laptop.run("liaison", &[command, "--json", &args], Some(&mac_ssh));
        assert!(out.status.success(), "{}", text(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let status = named("stow.status");
    assert_eq!(status["machine"], "mac", "{status}");
    assert!(status.get("machines").is_none(), "{status}");
    assert!(status.get("this_machine").is_none(), "{status}");

    // The captain's terminal names the machine it is about, and lists no other
    // machine's hold as this one's.
    let shown = text(&laptop.run(
        "liaison",
        &["stow", "status", "--machine", "mac"],
        Some(&mac_ssh),
    ));
    assert!(shown.contains("fleet on mac"), "{shown}");
    for other in [
        "still stowed",
        "running again",
        "unknown here",
        "reaches this machine",
    ] {
        assert!(!shown.contains(other), "{shown} says {other}");
    }

    // Asked about itself, this machine still names both.
    let ours = laptop.ok_over("liaison", "stow.status", json!({}), &mac_ssh);
    assert_eq!(ours["this_machine"], json!("laptop"), "{ours}");
    assert_eq!(
        ours["machines"],
        json!([{"machine": "mac", "asked": true, "held": false}]),
        "{ours}"
    );
}

/// A resume of every machine lifts what it can and says what it could not: a
/// linked machine whose resume never came back is unknown here, never still
/// stowed and never resumed, and the run does not report itself finished.
#[test]
fn a_resume_of_all_that_cannot_lift_a_linked_machine_does_not_claim_it_did() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    laptop.ship("upkeep");

    // The Mac answers about its own hold, then drops the resume itself: a link
    // that fails halfway is the case a notice must not turn into a claim.
    let flaky = mac.root.join("flaky-ssh");
    fs::write(
        &flaky,
        format!(
            "#!/bin/sh\nfor argument do remote=$argument; done\ninput=$(cat)\n\
             case \"$input\" in *stow.resume*) exit 255 ;; esac\n\
             printf '%s\\n' \"$input\" | env HOME={root} PATH={root}/bin:/usr/bin:/bin \
             PRIVATEER_FLEET={root}/fleet.sqlite PRIVATEER_RESOURCE_HOME={root}/ledger \
             /bin/sh -c \"$remote\"\n",
            root = mac.root.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&flaky, fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        laptop
            .run("liaison", &["stow", "--machine", "mac"], Some(&flaky))
            .status
            .success()
    );
    laptop.ok("liaison", "stow.start", json!({}));

    let all = laptop.run("liaison", &["stow", "resume", "--all"], Some(&flaky));
    assert!(
        !all.status.success(),
        "the run lifted no machine: {}",
        text(&all)
    );
    let shown = text(&all);
    assert!(shown.contains("mac: could not be reached"), "{shown}");
    assert!(shown.contains("hold unknown here: mac"), "{shown}");
    assert!(!shown.contains("mac: still stowed"), "{shown}");

    // This machine's own hold did end; its ships hear that, and the Mac is
    // named unknown rather than resumed or left holding.
    let body = messages(&laptop, "upkeep", "notice")
        .into_iter()
        .last()
        .map(|m| m["body"].as_str().unwrap_or("").to_owned())
        .unwrap_or_default();
    assert!(body.contains("resumed this machine (laptop)"), "{body}");
    assert!(
        body.contains("Linked machine mac could not be reached from here"),
        "{body}"
    );
    assert!(!body.contains("is still stowed, so new jobs"), "{body}");

    // A resume it never heard cannot have lifted the Mac's hold.
    assert_eq!(
        mac.ok("fitforge", "stow.status", json!({}))["held"],
        true,
        "the Mac is still stowed"
    );
    assert!(messages(&mac, "fitforge", "notice").is_empty());
    // And the claim is checkable in the records before anyone repeats it.
    let ours = laptop.ok_over("liaison", "stow.status", json!({}), &flaky);
    let theirs = &ours["machines"][0];
    assert_eq!(theirs["machine"], "mac", "{ours}");
    assert_eq!(theirs["held"], true, "{ours}");
    assert!(theirs.get("error").is_none(), "{ours}");
}

/// A machine that cannot be reached is named unknown, never resumed, and
/// `--all` leaves its hold alone rather than claiming to have lifted it.
#[test]
fn a_resume_that_cannot_reach_a_linked_machine_says_so() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    let mac_ssh = remote_ssh(&mac);
    laptop.ship("upkeep");
    assert!(
        laptop
            .run("liaison", &["stow", "--machine", "mac"], Some(&mac_ssh))
            .status
            .success()
    );
    laptop.ok("liaison", "stow.start", json!({}));

    // The Mac is asleep: its ssh never answers.
    let dead = laptop.root.join("dead-ssh");
    fs::write(&dead, "#!/bin/sh\nexit 255\n").unwrap();
    fs::set_permissions(&dead, fs::Permissions::from_mode(0o755)).unwrap();

    let resumed = laptop.run("liaison", &["stow", "resume"], Some(&dead));
    assert!(resumed.status.success(), "{}", text(&resumed));
    assert!(
        text(&resumed).contains("mac: could not be reached"),
        "{}",
        text(&resumed)
    );
    let body = messages(&laptop, "upkeep", "notice")
        .into_iter()
        .last()
        .map(|m| m["body"].as_str().unwrap_or("").to_owned())
        .unwrap_or_default();
    assert!(
        body.contains("Linked machine mac could not be reached from here"),
        "{body}"
    );
    assert!(!body.contains("is still stowed, so new jobs"), "{body}");

    // `--all` cannot lift what it cannot ask, and says it did not finish.
    laptop.ok("liaison", "stow.start", json!({}));
    let all = laptop.run("liaison", &["stow", "resume", "--all"], Some(&dead));
    assert!(!all.status.success(), "{}", text(&all));
    assert!(
        text(&all).contains("hold unknown here: mac"),
        "{}",
        text(&all)
    );
    assert_eq!(
        mac.ok("fitforge", "stow.status", json!({}))["held"],
        true,
        "the Mac is still stowed"
    );
    assert!(messages(&mac, "fitforge", "notice").is_empty());
}

#[test]
fn a_server_ship_finds_its_consumers_on_both_machines() {
    let (_temp, laptop, mac, ssh) = fixture();
    mac.ship("runtime");
    mac.ship("cosmic");
    let as_agent = |machine: &Machine, ship: &str, args: Value| {
        machine.run(
            ship,
            &[
                "ship.consumes",
                "--json",
                &args.to_string(),
                "--role",
                "agent",
                "--actor",
                ship,
            ],
            None,
        )
    };
    // A ship records what it consumes; the operator may record it for any.
    let own = as_agent(
        &mac,
        "fitforge",
        json!({"name": "fitforge", "consumes": ["runtime"]}),
    );
    assert!(own.status.success(), "{}", text(&own));
    mac.ok(
        "cosmic",
        "ship.consumes",
        json!({"name": "cosmic", "consumes": ["runtime"]}),
    );
    // Not for a ship it did not create, and only ships it knows.
    let other = as_agent(&mac, "fitforge", json!({"name": "cosmic", "consumes": []}));
    assert!(!other.status.success(), "{}", text(&other));
    let unknown = as_agent(
        &mac,
        "fitforge",
        json!({"name": "fitforge", "consumes": ["nobody"]}),
    );
    assert!(!unknown.status.success(), "{}", text(&unknown));

    let consumers = |machine: &Machine| -> Vec<String> {
        machine.ok("runtime", "ship.consumers", json!({"name": "runtime"}))["consumers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(consumers(&mac), ["cosmic", "fitforge"]);

    // A ship on the linked laptop that uses the server is found too, and a
    // directory refresh keeps what every ship consumes.
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    laptop.ok(
        "liaison",
        "ship.consumes",
        json!({"name": "liaison", "consumes": ["runtime"]}),
    );
    let refreshed = laptop.run("liaison", &["machine", "refresh"], Some(&ssh));
    assert!(refreshed.status.success(), "{}", text(&refreshed));
    assert_eq!(consumers(&mac), ["cosmic", "fitforge", "liaison"]);
    let seen = laptop.ok("liaison", "ship.show", json!({"name": "fitforge"}));
    assert_eq!(seen["consumes"], json!(["runtime"]), "{seen}");
}

#[test]
fn a_ship_created_on_a_linked_machine_lives_only_there() {
    let (_temp, laptop, mac, _ssh) = fixture();
    let mac_ssh = remote_ssh(&mac);
    assert!(link(&laptop, &mac, &mac_ssh).status.success());
    // The Mac's own default harness is the one on its PATH, not the laptop's.
    fs::write(mac.root.join("bin/pi"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(mac.root.join("bin/pi"), fs::Permissions::from_mode(0o755)).unwrap();

    let made = laptop.run(
        "liaison",
        &[
            "ship.create",
            "--json",
            r#"{"name":"bench","machine":"mac","scope":"benchmarks"}"#,
        ],
        Some(&mac_ssh),
    );
    assert!(made.status.success(), "{}", text(&made));
    let made: Value = serde_json::from_slice(&made.stdout).unwrap();
    assert_eq!(made["machine"], "mac", "{made}");
    assert_eq!(made["created_by"], "captain", "{made}");
    assert_eq!(made["harness"], "pi", "{made}");
    // Beside the Mac's own ships, in the Mac's registry only.
    assert_eq!(made["folder"], json!(mac.folder("bench")), "{made}");
    assert!(mac.folder("bench").join(".privateer").is_dir());
    assert!(!laptop.folder("bench").exists());
    let ships = |machine: &Machine, ship: &str| machine.ok(ship, "fleet.list", json!({}));
    let on_mac = ships(&mac, "fitforge");
    let bench = on_mac["ships"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "bench")
        .unwrap_or_else(|| panic!("{on_mac}"));
    assert_eq!(bench["created_by"], "captain");
    assert!(!ships(&laptop, "liaison").to_string().contains("bench"));
    // Reachable from the laptop at once, behind the Mac's peer.
    let shown = laptop.ok("liaison", "ship.show", json!({"name": "bench"}));
    assert_eq!(shown["location"], json!({"peer": "machine-mac"}), "{shown}");

    // A name the other machine holds, an unknown machine and a link made from
    // the other side are refused, naming the machine.
    let refusal = |machine: &Machine, ship: &str, args: Value| {
        let output = machine.run(
            ship,
            &["ship.create", "--json", &args.to_string()],
            Some(&mac_ssh),
        );
        assert!(!output.status.success());
        text(&output)
    };
    let taken = refusal(
        &laptop,
        "liaison",
        json!({"name": "fitforge", "machine": "mac"}),
    );
    assert!(taken.contains("linked machine mac"), "{taken}");
    let unknown = refusal(
        &laptop,
        "liaison",
        json!({"name": "x", "machine": "nowhere"}),
    );
    assert!(
        unknown.contains("no linked machine named nowhere"),
        "{unknown}"
    );
    let backwards = refusal(&mac, "fitforge", json!({"name": "x", "machine": "laptop"}));
    assert!(
        backwards.contains("laptop reaches this machine"),
        "{backwards}"
    );
    let remote = refusal(
        &laptop,
        "liaison",
        json!({"name": "y", "machine": "mac", "harness": "no-such-harness"}),
    );
    assert!(remote.contains("machine mac:"), "{remote}");
    // The machine's refusal is recorded here, after the call.
    let audit = laptop.ok("liaison", "audit.list", json!({"limit": 1000}));
    assert!(
        audit["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "ship.create.failed"
                && e["payload"]["ship"] == "y"
                && e["payload"]["error"]
                    .as_str()
                    .unwrap()
                    .contains("machine mac:")),
        "{audit}"
    );

    // The gateway itself refuses a creator that is not a ship on the asking
    // machine, and a machine not linked to it, creating nothing.
    let desk = Machine {
        root: laptop.root.with_file_name("desk"),
    };
    desk.ship("drafts");
    assert!(
        link_as(&desk, "drafts", "desk", &mac, &mac_ssh)
            .status
            .success()
    );
    let gateway_refusal = |args: Value| {
        let response = mac.gateway("ship.create", args);
        assert_eq!(response["ok"], false, "{response}");
        response["error"].as_str().unwrap().to_owned()
    };
    let own =
        gateway_refusal(json!({"name": "x", "from_machine": "laptop", "created_by": "fitforge"}));
    assert!(own.contains("fitforge is a ship on this machine"), "{own}");
    let third =
        gateway_refusal(json!({"name": "x", "from_machine": "laptop", "created_by": "drafts"}));
    assert!(
        third.contains("drafts is a ship on the linked machine desk"),
        "{third}"
    );
    let stranger =
        gateway_refusal(json!({"name": "x", "from_machine": "nowhere", "created_by": "captain"}));
    assert!(stranger.contains("nowhere"), "{stranger}");
    assert!(!mac.folder("x").exists());

    // Without a machine, ship.create still makes the ship here.
    let local = laptop.ok(
        "liaison",
        "ship.create",
        json!({"name": "nearby", "harness": "codex"}),
    );
    assert_eq!(local["folder"], json!(laptop.folder("nearby")), "{local}");
    assert_eq!(local.get("machine"), None, "{local}");
    assert!(!ships(&mac, "fitforge").to_string().contains("nearby"));
}

#[test]
fn only_an_agent_that_may_spawn_creates_a_ship_on_a_linked_machine() {
    let (_temp, laptop, mac, _ssh) = fixture();
    let mac_ssh = remote_ssh(&mac);
    assert!(link(&laptop, &mac, &mac_ssh).status.success());
    let linked = connections(&mac);
    let agent = |args: Value| {
        laptop.run(
            "liaison",
            &[
                "ship.create",
                "--json",
                &args.to_string(),
                "--role",
                "agent",
                "--actor",
                "liaison",
            ],
            Some(&mac_ssh),
        )
    };
    let request = json!({"name": "bench", "machine": "mac", "harness": "codex"});

    // Without spawn it is refused here, before the Mac is contacted.
    laptop.ok(
        "liaison",
        "charter.put",
        json!({"id": "liaison", "actions": ["inspect", "message"]}),
    );
    let refused = agent(request.clone());
    assert!(!refused.status.success());
    assert!(
        text(&refused).contains("requires spawn"),
        "{}",
        text(&refused)
    );
    // Nor may it give the new ship an action it lacks.
    laptop.ok(
        "liaison",
        "charter.put",
        json!({"id": "liaison", "actions": ["inspect", "spawn", "message"]}),
    );
    let mut wider = request.clone();
    wider["actions"] = json!(["inspect", "implement"]);
    let refused = agent(wider);
    assert!(!refused.status.success());
    assert!(text(&refused).contains("implement"), "{}", text(&refused));
    assert_eq!(connections(&mac), linked, "the Mac was contacted");
    assert!(!mac.folder("bench").exists());

    // With spawn, the new ship holds the creator's actions and names it.
    // While the Mac is creating it, the liaison's home still takes writes:
    // the first connection writes there before reaching the Mac.
    let probe = laptop.root.join("probe-ssh");
    fs::write(
        &probe,
        format!(
            "#!/bin/sh
[ -e {root}/probe.log ] || (cd {root}/liaison && {bin} note.put --json '{{\"ship\":\"liaison\",\"focus\":\"ship\",\"body\":\"during the call\"}}' > {root}/probe.out 2>&1; echo $? > {root}/probe.log)
exec {mac_ssh} \"$@\"
",
            root = laptop.root.display(),
            bin = support::BIN,
            mac_ssh = mac_ssh.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).unwrap();
    let made = laptop.run(
        "liaison",
        &[
            "ship.create",
            "--json",
            &request.to_string(),
            "--role",
            "agent",
            "--actor",
            "liaison",
        ],
        Some(&probe),
    );
    assert!(made.status.success(), "{}", text(&made));
    assert_eq!(
        fs::read_to_string(laptop.root.join("probe.log"))
            .unwrap()
            .trim(),
        "0",
        "a write in the creator's home failed during the call: {}",
        fs::read_to_string(laptop.root.join("probe.out")).unwrap_or_default()
    );
    let audit = laptop.ok("liaison", "audit.list", json!({"limit": 1000}));
    let kinds: Vec<&str> = audit["events"]
        .as_array()
        .unwrap_or_else(|| panic!("{audit}"))
        .iter()
        .filter_map(|e| e["kind"].as_str())
        .filter(|k| k.starts_with("ship.create"))
        .collect();
    assert_eq!(kinds, ["ship.create.requested", "ship.created"], "{audit}");
    let made: Value = serde_json::from_slice(&made.stdout).unwrap();
    assert_eq!(made["created_by"], "liaison", "{made}");
    assert_eq!(made["actions"], json!(["inspect", "spawn", "message"]));
    let on_mac = mac.ok("fitforge", "fleet.list", json!({}));
    assert!(
        on_mac["ships"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "bench" && s["created_by"] == "liaison"),
        "{on_mac}"
    );
}

fn git(dir: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=Captain",
            "-c",
            "user.email=captain@example.com",
        ])
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", text(&output));
}

/// A ship with one project cloned from a shared remote, and a harness each
/// machine has installed.
fn ship_with_project(laptop: &Machine, mac: &Machine, ship: &str) -> PathBuf {
    for machine in [laptop, mac] {
        let bin = machine.root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("codex"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let origin = laptop.root.join("charts.git");
    git(
        &laptop.root,
        &[
            "init",
            "--quiet",
            "--bare",
            "--initial-branch=main",
            "charts.git",
        ],
    );
    let seed = laptop.root.join("seed");
    git(
        &laptop.root,
        &["clone", "--quiet", origin.to_str().unwrap(), "seed"],
    );
    fs::write(seed.join("README"), "charts\n").unwrap();
    git(&seed, &["add", "README"]);
    git(&seed, &["commit", "--quiet", "-m", "First chart"]);
    git(&seed, &["push", "--quiet", "origin", "HEAD:main"]);
    laptop.ship(ship);
    let added = laptop.run(ship, &["project", "add", origin.to_str().unwrap()], None);
    assert!(added.status.success(), "{}", text(&added));
    laptop.folder(ship).join("projects/charts")
}

#[test]
fn an_idle_ship_moves_to_the_other_machine_and_back() {
    let (_temp, laptop, mac, _ssh) = fixture();
    let mac_ssh = remote_ssh(&mac);
    ship_with_project(&laptop, &mac, "scout");
    assert!(link(&laptop, &mac, &mac_ssh).status.success());
    laptop.ok(
        "scout",
        "note.put",
        json!({"ship": "scout", "focus": "ship", "body": "Chart the north passage first."}),
    );

    let moved = laptop.run(
        "liaison",
        &["ship", "move", "scout", "--to", "mac"],
        Some(&mac_ssh),
    );
    assert!(moved.status.success(), "{}", text(&moved));

    // It lives on the Mac now, beside the Mac's ships, with what it knew, its
    // charter pointing at its new folder and its project cloned again.
    let note = mac.ok(
        "scout",
        "note.get",
        json!({"ship": "scout", "focus": "ship"}),
    );
    assert!(note.to_string().contains("north passage"), "{note}");
    let charter = mac.ok("scout", "charter.show", json!({"id": "scout"}));
    assert_eq!(charter["repo"], json!(mac.folder("scout")), "{charter}");
    assert_eq!(charter["active"], true, "{charter}");
    assert!(mac.folder("scout").join("projects/charts/README").is_file());
    // The laptop's folder is kept but runs nothing, and laptop ships reach
    // the ship on the Mac.
    let stale = laptop.run("scout", &["status"], None);
    assert!(!stale.status.success());
    assert!(
        text(&stale).contains("moved to the machine mac"),
        "{}",
        text(&stale)
    );
    let scout = laptop.ok("liaison", "ship.show", json!({"name": "scout"}));
    assert_eq!(scout["location"], json!({"peer": "machine-mac"}), "{scout}");
    let sent = laptop.run(
        "liaison",
        &[
            "mail.send",
            "--json",
            &json!({"from": "liaison", "to": "scout", "kind": "notice", "body": "Storm ahead"})
                .to_string(),
        ],
        Some(&mac_ssh),
    );
    assert!(sent.status.success(), "{}", text(&sent));
    assert_eq!(messages(&mac, "scout", "notice").len(), 1);

    // Brought back, it carries the mail it got on the Mac; the laptop's old
    // folder is set aside, never deleted, and the Mac's copy runs nothing.
    let back = laptop.run(
        "liaison",
        &["ship", "move", "scout", "--here"],
        Some(&mac_ssh),
    );
    assert!(back.status.success(), "{}", text(&back));
    let notices = messages(&laptop, "scout", "notice");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["body"], "Storm ahead");
    let note = laptop.ok(
        "scout",
        "note.get",
        json!({"ship": "scout", "focus": "ship"}),
    );
    assert!(note.to_string().contains("north passage"), "{note}");
    let aside: Vec<_> = fs::read_dir(&laptop.root)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("scout.left-"))
        .collect();
    assert_eq!(aside.len(), 1);
    assert!(!mac.run("scout", &["status"], None).status.success());
    let scout = mac.ok("fitforge", "ship.show", json!({"name": "scout"}));
    assert_eq!(
        scout["location"],
        json!({"peer": "machine-laptop"}),
        "{scout}"
    );
}

#[test]
fn a_ship_with_unpushed_work_stays_where_it_is() {
    let (_temp, laptop, mac, _ssh) = fixture();
    let mac_ssh = remote_ssh(&mac);
    let project = ship_with_project(&laptop, &mac, "scout");
    assert!(link(&laptop, &mac, &mac_ssh).status.success());
    fs::write(project.join("route.txt"), "north\n").unwrap();
    git(&project, &["add", "route.txt"]);
    git(&project, &["commit", "--quiet", "-m", "Plot the route"]);

    let moved = laptop.run(
        "liaison",
        &["ship", "move", "scout", "--to", "mac"],
        Some(&mac_ssh),
    );
    assert!(!moved.status.success());
    assert!(
        text(&moved).contains("commits not pushed"),
        "{}",
        text(&moved)
    );
    // Nothing changed: the ship still runs on the laptop and the Mac never
    // heard of a new ship.
    assert!(laptop.run("scout", &["status"], None).status.success());
    assert!(!mac.folder("scout").exists());
    let scout = laptop.ok("liaison", "ship.show", json!({"name": "scout"}));
    assert_eq!(scout["location"], json!({"peer": "fleet-scout"}), "{scout}");
}

/// The captain's own mail written in a home on this machine, sent over the link.
fn captain_sends(machine: &Machine, ship: &str, ssh: &Path, mail: Value) -> Output {
    let mut mail = mail;
    mail["from"] = json!("captain");
    machine.run(ship, &["mail.send", "--json", &mail.to_string()], Some(ssh))
}

/// What the captain has been sent in a home here, notices included.
fn captain_mail(machine: &Machine, ship: &str) -> Vec<Value> {
    machine.ok(ship, "mail.inbox", json!({"address": "captain"}))["messages"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn a_captain_reply_reaches_the_home_that_asked_across_the_link() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("rig");
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    // The captain asks from a ship's home that is not the captain's.
    let asked = captain_sends(
        &laptop,
        "rig",
        &ssh,
        json!({"to": "fitforge", "kind": "request", "body": "Upload build 12?"}),
    );
    assert!(asked.status.success(), "{}", text(&asked));
    let asked: Value = serde_json::from_slice(&asked.stdout).unwrap();

    let answered = mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "reply",
            "correlation": asked["id"], "body": "Yes, ship it"}),
    );
    assert!(answered["id"].is_string(), "{answered}");
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["delivered"], 1, "{pass}");
    assert_eq!(pass["machines"][0]["refused"], 0, "{pass}");

    // The reply closes the request where it was written and waits in that
    // home's captain mailbox, which the captain's door reads.
    let here = laptop.ok("rig", "mail.show", json!({"id": asked["id"]}));
    assert_eq!(here["state"], "answered", "{here}");
    let inbox = captain_mail(&laptop, "rig");
    let bodies: Vec<&str> = inbox.iter().filter_map(|m| m["body"].as_str()).collect();
    assert_eq!(bodies, ["Yes, ship it"], "{bodies:?}");
    assert!(captain_mail(&laptop, "liaison").is_empty());
}

#[test]
fn a_captain_reply_refused_across_the_link_is_put_before_the_captain() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("rig");
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    let asked = captain_sends(
        &laptop,
        "rig",
        &ssh,
        json!({"to": "fitforge", "kind": "request", "body": "Upload build 12?"}),
    );
    assert!(asked.status.success(), "{}", text(&asked));
    let asked: Value = serde_json::from_slice(&asked.stdout).unwrap();
    let answered = mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "reply",
            "correlation": asked["id"], "body": "Yes, ship it"}),
    );

    // Nothing here holds the request any more, so the reply cannot close it.
    let home: String = rusqlite::Connection::open(laptop.registry())
        .unwrap()
        .query_row("SELECT home FROM ships WHERE name='rig'", [], |r| r.get(0))
        .unwrap();
    let database = PathBuf::from(home).join("privateer.sqlite");
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute(
            "DELETE FROM mail WHERE id=?1",
            [asked["id"].as_str().unwrap()],
        )
        .unwrap();
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["refused"], 1, "{pass}");

    let notices: Vec<Value> = captain_mail(&laptop, "liaison")
        .into_iter()
        .filter(|m| m["kind"] == "notice")
        .collect();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["payload"]["refused"]["id"], answered["id"]);
    assert_eq!(notices[0]["payload"]["refused"]["from"], "fitforge");
    assert!(
        notices[0]["payload"]["refused"]["reason"]
            .as_str()
            .unwrap()
            .contains("unknown request"),
        "{notices:?}"
    );
}

#[test]
fn a_captain_reply_to_a_request_asked_before_the_link_reaches_the_other_machine() {
    let (_temp, laptop, mac, ssh) = fixture();
    let asked = mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "request", "body": "Before the link?"}),
    );
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));

    // The request was never collected here, so the captain's home does not
    // hold it; the reply goes across the link and the other machine checks it.
    let answer = captain_sends(
        &laptop,
        "liaison",
        &ssh,
        json!({"to": "fitforge", "kind": "reply", "correlation": asked["id"], "body": "Yes"}),
    );
    assert!(answer.status.success(), "{}", text(&answer));
    let answered = mac.ok("fitforge", "mail.show", json!({"id": asked["id"]}));
    assert_eq!(answered["state"], "answered", "{answered}");
    let replies = messages(&mac, "fitforge", "reply");
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0]["from"], "captain");
    assert!(captain_mail(&laptop, "liaison").is_empty());
}

#[test]
fn a_captain_message_the_other_machine_refuses_is_put_before_the_captain() {
    let (_temp, laptop, mac, ssh) = fixture();
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));

    let answer = captain_sends(
        &laptop,
        "liaison",
        &ssh,
        json!({"to": "fitforge", "kind": "reply", "correlation": "no-such-request", "body": "Yes"}),
    );
    assert!(answer.status.success(), "{}", text(&answer));
    let answer: Value = serde_json::from_slice(&answer.stdout).unwrap();
    let notices: Vec<Value> = captain_mail(&laptop, "liaison")
        .into_iter()
        .filter(|m| m["kind"] == "notice")
        .collect();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["payload"]["refused"]["id"], answer["id"]);
    assert_eq!(notices[0]["payload"]["refused"]["to"], "fitforge");
    assert!(
        notices[0]["body"].as_str().unwrap().contains("was refused"),
        "{notices:?}"
    );

    // The refused reply does not hold back the captain's later mail to that machine.
    let later = captain_sends(
        &laptop,
        "liaison",
        &ssh,
        json!({"to": "fitforge", "kind": "notice", "body": "Still reaching you"}),
    );
    assert!(later.status.success(), "{}", text(&later));
    let inbox = messages(&mac, "fitforge", "notice");
    assert!(
        inbox.iter().any(|m| m["body"] == "Still reaching you"),
        "{inbox:?}"
    );
}

/// Makes this machine's next collection due now, rather than after its contact interval.
fn collect_now(machine: &Machine) {
    rusqlite::Connection::open(machine.registry())
        .unwrap()
        .execute("UPDATE machines SET next_contact_ms=0", [])
        .unwrap();
}

/// The captain's notices in the captain's mailbox here.
fn captain_notices(machine: &Machine, ship: &str) -> Vec<Value> {
    captain_mail(machine, ship)
        .into_iter()
        .filter(|m| m["kind"] == "notice")
        .collect()
}

#[test]
fn a_captain_reply_the_collected_side_cannot_judge_is_refused_at_once() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("rig");
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));

    // The captain answers from the machine that is collected from, a request
    // this machine does not hold. The collecting machine would refuse it
    // later, so it is refused here, where nothing waits to be collected.
    let answer = captain_sends(
        &mac,
        "fitforge",
        &ssh,
        json!({"to": "rig", "kind": "reply", "correlation": "no-such-request", "body": "Yes"}),
    );
    assert!(!answer.status.success(), "{}", text(&answer));
    assert!(
        text(&answer).contains("unknown correlated message"),
        "{}",
        text(&answer)
    );

    collect_now(&laptop);
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["delivered"], 0, "{pass}");
    assert_eq!(pass["machines"][0]["refused"], 0, "{pass}");
    assert!(captain_notices(&laptop, "liaison").is_empty());
}

#[test]
fn a_captain_reply_the_collecting_machine_refuses_is_put_before_the_captain() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("rig");
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));

    // This machine holds a request from rig to the captain that rig's home
    // does not, so the reply is accepted here and refused where it is collected.
    let home: String = rusqlite::Connection::open(mac.registry())
        .unwrap()
        .query_row("SELECT home FROM ships WHERE name='fitforge'", [], |r| {
            r.get(0)
        })
        .unwrap();
    rusqlite::Connection::open(PathBuf::from(home).join("privateer.sqlite"))
        .unwrap()
        .execute(
            "INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,state)
             VALUES('rig-request','rig','captain','request','Upload build 12?','{}','normal',0,'open')",
            [],
        )
        .unwrap();

    let answer = captain_sends(
        &mac,
        "fitforge",
        &ssh,
        json!({"to": "rig", "kind": "reply", "correlation": "rig-request", "body": "Yes"}),
    );
    assert!(answer.status.success(), "{}", text(&answer));
    let answer: Value = serde_json::from_slice(&answer.stdout).unwrap();

    collect_now(&laptop);
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["refused"], 1, "{pass}");
    let notices = captain_notices(&laptop, "liaison");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["from"], "runtime", "{notices:?}");
    assert_eq!(notices[0]["payload"]["refused"]["id"], answer["id"]);
    assert_eq!(notices[0]["payload"]["refused"]["from"], "captain");
    assert_eq!(notices[0]["payload"]["refused"]["to"], "rig");
    assert!(
        notices[0]["payload"]["refused"]["reason"]
            .as_str()
            .unwrap()
            .contains("unknown request"),
        "{notices:?}"
    );
}

#[test]
fn a_captain_reply_whose_request_home_stays_unreadable_is_refused_after_waiting() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("rig");
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    let asked = captain_sends(
        &laptop,
        "rig",
        &ssh,
        json!({"to": "fitforge", "kind": "request", "body": "Upload build 12?"}),
    );
    assert!(asked.status.success(), "{}", text(&asked));
    let asked: Value = serde_json::from_slice(&asked.stdout).unwrap();
    let answered = mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "reply",
            "correlation": asked["id"], "body": "Yes, ship it"}),
    );

    // The home that holds the request cannot be read for now.
    let home: String = rusqlite::Connection::open(laptop.registry())
        .unwrap()
        .query_row("SELECT home FROM ships WHERE name='rig'", [], |r| r.get(0))
        .unwrap();
    let database = PathBuf::from(home).join("privateer.sqlite");
    fs::write(&database, b"not a database").unwrap();

    // It waits for the next passes, and is refused on the last of them.
    for pass in 1..10 {
        collect_now(&laptop);
        let result = laptop.pass("liaison", &ssh);
        assert_eq!(result["machines"][0]["refused"], 0, "pass {pass}: {result}");
        assert!(
            captain_notices(&laptop, "liaison").is_empty(),
            "pass {pass}"
        );
    }
    collect_now(&laptop);
    let result = laptop.pass("liaison", &ssh);
    assert_eq!(result["machines"][0]["refused"], 1, "{result}");
    let notices = captain_notices(&laptop, "liaison");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["payload"]["refused"]["id"], answered["id"]);
    assert!(
        notices[0]["payload"]["refused"]["reason"]
            .as_str()
            .unwrap()
            .contains("not yet located"),
        "{notices:?}"
    );
}
