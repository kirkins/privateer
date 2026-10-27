//! Two machines, each with its own registry and ship homes in separate
//! directories, linked one way through a real subprocess transport: a fake
//! `ssh` that runs the remote command, so the linking side reaches the other
//! machine's gateway exactly as it would over SSH, and the other side never
//! connects back.
mod support;

use rusqlite::params;
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
        self.gateway_with(command, args, &[])
    }

    /// The same, with extra environment for the gateway process.
    fn gateway_with(&self, command: &str, args: Value, env: &[(&str, &str)]) -> Value {
        self.gateway_from("fitforge", command, args, env)
    }

    fn gateway_from(&self, ship: &str, command: &str, args: Value, env: &[(&str, &str)]) -> Value {
        use std::io::{BufRead, Write};
        let mut child = self
            .command(ship, &["gateway"], None)
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

    /// Put the harness its ships run on this machine's PATH, so machine
    /// readiness has nothing to tell the owner.
    fn ready(&self) {
        let bin = self.root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("codex"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o755)).unwrap();
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

#[test]
fn delegated_desk_receiver_refuses_when_another_machine_holds_the_desk() {
    for explicit in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let receiver = Machine {
            root: fs::canonicalize(temp.path()).unwrap(),
        };
        receiver.ship("fitforge");
        let conn = rusqlite::Connection::open(receiver.registry()).unwrap();
        // Explicit forwarding and the implicit collected-machine placement
        // must both refuse inbound desk calls, without attempting a next hop.
        conn.execute(
            "INSERT INTO machines(name,role,peer,added_ms) VALUES('elsewhere',?1,'{}',0)",
            [if explicit { "collects" } else { "collected" }],
        )
        .unwrap();
        if explicit {
            conn.execute(
                "INSERT INTO captain_desk(only,machine,set_ms) VALUES(1,'elsewhere',0)",
                [],
            )
            .unwrap();
        }
        let context = json!({"version":1,
            "client":"00000000-0000-4000-8000-000000000001",
            "resource":"door:fixture", "scopes":["captain:presence","mail:read","fleet:read"],
            "ships":["fitforge"], "result_id":"00000000-0000-4000-8000-000000000002"});
        for (operation, request) in [
            ("away", json!({"action":"start","words":"must not fork"})),
            ("away", json!({"action":"back"})),
            ("away", json!({"action":"status"})),
            ("held", json!({})),
            ("digest", json!({})),
            ("ack", json!({"refs":["not-served"]})),
        ] {
            let response = receiver.gateway(
                "fleet.delegated.v1",
                json!({
                "context":context, "command":"desk.v1", "args":{
                    "origin":"00000000-0000-4000-8000-000000000003",
                    "operation":operation, "request":request}}),
            );
            assert_eq!(response["ok"], false, "{response}");
            assert_eq!(
                response["denial_code"], "linked_provenance_unavailable",
                "{response}"
            );
            // No presence, held bookkeeping, digest or reader state is even
            // initialized on the non-desk receiver.
            for table in ["away", "away_digest", "captain_told", "door_reader_digests"] {
                let exists: bool = conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
                        [table],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert!(
                    !exists,
                    "{table} written by {operation}; explicit={explicit}"
                );
            }
        }
    }
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
    link_command(laptop, ship, known_as, mac, ssh)
        .output()
        .unwrap()
}

fn link_command(
    laptop: &Machine,
    ship: &str,
    known_as: &str,
    mac: &Machine,
    ssh: &Path,
) -> std::process::Command {
    let registry = mac.registry();
    laptop.command(
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

/// A login shell on `machine` that adds nothing to the PATH, so a readiness
/// check there sees only that machine's own tools.
fn bare_login_shell(machine: &Machine) -> PathBuf {
    let shell = machine.root.join("bare-login-shell");
    // It runs the command it is given, as `$SHELL -lc` does, on a bare PATH.
    fs::write(
        &shell,
        "#!/bin/sh\nPATH=/usr/bin:/bin exec /bin/sh -c \"$2\"\n",
    )
    .unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
    shell
}

/// A fake ssh into `machine` whose gateway keeps that machine's own home,
/// ledger and PATH, as a real remote host does; the plain fake ssh would
/// pass the caller's environment through. Each connection is logged.
fn remote_ssh(machine: &Machine) -> PathBuf {
    remote_ssh_with(machine, "ssh", "")
}

/// A fake ssh like `remote_ssh`, with `answer` (a shell pipeline) applied to
/// what the remote gateway writes back.
fn remote_ssh_with(machine: &Machine, name: &str, answer: &str) -> PathBuf {
    let ssh = machine.root.join(name);
    fs::create_dir_all(machine.root.join("bin")).unwrap();
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nfor argument do remote=$argument; done\necho connected >> {root}/ssh.log\nenv HOME={root} PATH={root}/bin:/usr/bin:/bin PRIVATEER_FLEET={root}/fleet.sqlite PRIVATEER_RESOURCE_HOME={root}/ledger /bin/sh -c \"$remote\" {answer}\n",
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

/// One exchange with a linked machine now, outside the dispatcher's schedule,
/// and what it ended of the requests bound to that machine's stows.
fn collect_as(machine: &Machine, ship: &str, ssh: &Path) -> Value {
    let output = machine.run(ship, &["machine.collect", "--json", "{}"], Some(ssh));
    assert!(output.status.success(), "{}", text(&output));
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The captain's closing notices in this home, where the requests were written.
fn notices(machine: &Machine, ship: &str) -> Vec<Value> {
    machine.ok(ship, "mail.inbox", json!({"address": "captain"}))["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["kind"] == "notice")
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
fn a_machines_owner_hears_before_free_disk_reaches_the_floor() {
    let (_temp, laptop, _mac, ssh) = fixture();
    laptop.ship("upkeep");
    laptop.ready();
    assert!(
        laptop
            .run("liaison", &["machine", "owner", "upkeep"], None)
            .status
            .success()
    );
    let free = laptop.ok("liaison", "resource.status", json!({}))["host"]["disk_available_bytes"]
        .as_u64()
        .unwrap();
    // A floor an eighth under what is free now, so the machine is above it:
    // only a falling rate can raise anything. Margins are fractions of the
    // live reading, so writes by other work on the machine move them little.
    laptop.ok(
        "liaison",
        "resource.configure",
        json!({"min_free_disk_bytes":free - free / 8}),
    );
    // A quarter of what is free was written in the last six hours, as an
    // earlier reading says: the floor is about three hours of that away.
    let home = laptop.folder("upkeep").join(".privateer");
    let key = privateer::admission::disk_key(&home).unwrap();
    let ledger = rusqlite::Connection::open(laptop.root.join("ledger/resources.sqlite")).unwrap();
    ledger
        .execute(
            "INSERT INTO disk_samples(path,sampled_ms,free_bytes) VALUES(?1,?2,?3)",
            params![
                key,
                privateer::store::Store::now_ms() - 6 * 3_600_000,
                (free + free / 4) as i64
            ],
        )
        .unwrap();
    let alerts = |ship: &str| -> Vec<String> {
        messages(&laptop, ship, "notice")
            .iter()
            .filter(|m| m["from"] == "crew")
            .filter_map(|m| m["body"].as_str().map(str::to_owned))
            .collect()
    };
    laptop.pass("upkeep", &ssh);
    let raised = alerts("upkeep");
    assert_eq!(raised.len(), 1, "{raised:?}");
    assert!(raised[0].contains("Free disk is falling"), "{raised:?}");
    assert!(raised[0].contains("per hour"), "{raised:?}");
    // It is not the floor's alarm: the machine is still above its floor.
    assert!(
        !raised[0].contains("below the machine's floor"),
        "{raised:?}"
    );
    // And again once the space stops falling.
    ledger
        .execute("DELETE FROM disk_samples WHERE path=?1", [&key])
        .unwrap();
    laptop.pass("upkeep", &ssh);
    let all = alerts("upkeep");
    assert_eq!(all.len(), 2, "{all:?}");
    assert!(all[1].contains("Cleared"), "{all:?}");
}

#[test]
fn a_falling_warning_is_not_cleared_while_the_floor_alarm_is_up() {
    let (_temp, laptop, _mac, ssh) = fixture();
    laptop.ship("upkeep");
    laptop.ready();
    assert!(
        laptop
            .run("liaison", &["machine", "owner", "upkeep"], None)
            .status
            .success()
    );
    let free = laptop.ok("liaison", "resource.status", json!({}))["host"]["disk_available_bytes"]
        .as_u64()
        .unwrap();
    laptop.ok(
        "liaison",
        "resource.configure",
        json!({"min_free_disk_bytes":free - free / 8}),
    );
    let home = laptop.folder("upkeep").join(".privateer");
    let key = privateer::admission::disk_key(&home).unwrap();
    let ledger = rusqlite::Connection::open(laptop.root.join("ledger/resources.sqlite")).unwrap();
    ledger
        .execute(
            "INSERT INTO disk_samples(path,sampled_ms,free_bytes) VALUES(?1,?2,?3)",
            params![
                key,
                privateer::store::Store::now_ms() - 6 * 3_600_000,
                (free + free / 4) as i64
            ],
        )
        .unwrap();
    let notices = || -> Vec<String> {
        messages(&laptop, "upkeep", "notice")
            .iter()
            .filter(|m| m["from"] == "crew")
            .filter_map(|m| m["body"].as_str().map(str::to_owned))
            .collect()
    };
    laptop.pass("upkeep", &ssh);
    assert_eq!(notices().len(), 1, "{:?}", notices());
    assert!(
        notices()[0].contains("Free disk is falling"),
        "{:?}",
        notices()
    );
    // The floor is crossed while the space is still falling: the floor alarm
    // arrives, and the warning is not cleared in the same pass.
    // Well above live free space, so other writers on a shared runner cannot
    // free enough in between to keep this machine above it.
    laptop.ok(
        "liaison",
        "resource.configure",
        json!({"min_free_disk_bytes":free + free / 8}),
    );
    laptop.pass("upkeep", &ssh);
    let all = notices();
    assert_eq!(all.len(), 2, "{all:?}");
    assert!(all[1].contains("below the machine's floor"), "{all:?}");
    assert!(all.iter().all(|body| !body.contains("Cleared")), "{all:?}");
    // The floor recovers, and the space is still falling: the floor clears
    // alone, and the warning stays raised.
    laptop.ok(
        "liaison",
        "resource.configure",
        json!({"min_free_disk_bytes":free - free / 8}),
    );
    laptop.pass("upkeep", &ssh);
    let recovered = notices();
    assert_eq!(recovered.len(), 3, "{recovered:?}");
    assert!(recovered[2].contains("Cleared"), "{recovered:?}");
    assert!(recovered[2].contains("disk_floor"), "{recovered:?}");
    // Only once the space stops falling does the warning clear.
    ledger
        .execute("DELETE FROM disk_samples WHERE path=?1", [&key])
        .unwrap();
    laptop.pass("upkeep", &ssh);
    let cleared = notices();
    assert_eq!(cleared.len(), 4, "{cleared:?}");
    assert!(cleared[3].contains("disk_trend"), "{cleared:?}");
}

/// One load reading a minute in the shared ledger, from `from` to `to`
/// minutes ago, each at `load` on one CPU. The newest lands a second ago, so
/// the dispatcher's own reading, at most one a minute, does not join them and
/// the test decides what the machine's load has been.
fn loads(ledger: &rusqlite::Connection, from: i64, to: i64, load: f64) {
    let now = privateer::store::Store::now_ms() - 1_000;
    for minutes in (to..=from).rev() {
        ledger
            .execute(
                "INSERT OR REPLACE INTO load_samples(sampled_ms,load,cpus) VALUES(?1,?2,1)",
                params![now - minutes * 60_000, load],
            )
            .unwrap();
    }
}

#[test]
fn sustained_load_reaches_the_owner_once_and_clears_when_it_settles() {
    let (_temp, laptop, _mac, ssh) = fixture();
    laptop.ship("upkeep");
    laptop.ready();
    assert!(
        laptop
            .run("liaison", &["machine", "owner", "upkeep"], None)
            .status
            .success()
    );
    // The gate is as loose as it goes, so the real machine is under it and
    // the jobs below start however busy the runner is.
    laptop.ok(
        "liaison",
        "resource.configure",
        json!({"max_load_per_cpu":1024,"min_available_memory_bytes":0,"ramp_secs":0,
            "heavy_estimate":{"memory_bytes":0,"disk_bytes":0}}),
    );
    let home = laptop.folder("upkeep").join(".privateer");
    let repo = laptop.root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    {
        let store = privateer::store::Store::open(&home).unwrap();
        let charter = json!({"id":"mission","objective":"load alert fixture","repo":repo,
            "active":true,"actions":["implement","test","recovery","cleanup"],"min_free_bytes":0,
            "job_timeout_secs":120});
        store
            .connection()
            .execute(
                "INSERT INTO charters(id,data) VALUES(?1,?2)",
                params!["mission", charter.to_string()],
            )
            .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('task','mission','upkeep','fixture','open',0)",
                [],
            )
            .unwrap();
    }
    // A heavy build, and a simulator run started as light work.
    for (id, heavy) in [("build", true), ("sim-ui", false)] {
        laptop.ok(
            "upkeep",
            "job.start",
            json!({"task_id":"task","id":id,"command":["sleep","120"],"heavy":heavy}),
        );
    }
    let until = std::time::Instant::now() + std::time::Duration::from_secs(12);
    for id in ["build", "sim-ui"] {
        while laptop.ok("upkeep", "job.status", json!({"id":id}))["status"] != "running" {
            assert!(std::time::Instant::now() < until, "{id} never ran");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    let notices = || -> Vec<String> {
        messages(&laptop, "upkeep", "notice")
            .iter()
            .filter(|m| m["from"] == "crew")
            .filter_map(|m| m["body"].as_str().map(str::to_owned))
            .collect()
    };
    let ledger = rusqlite::Connection::open(laptop.root.join("ledger/resources.sqlite")).unwrap();

    // A burst of two minutes over a quiet machine is not an alarm.
    loads(&ledger, 11, 3, 0.0);
    loads(&ledger, 2, 0, 1e9);
    laptop.pass("upkeep", &ssh);
    assert!(notices().is_empty(), "{:?}", notices());

    // Eleven minutes above the gate reach the owner, once.
    ledger.execute("DELETE FROM load_samples", []).unwrap();
    loads(&ledger, 11, 0, 1e9);
    laptop.pass("upkeep", &ssh);
    laptop.pass("upkeep", &ssh);
    let raised = notices();
    assert_eq!(raised.len(), 1, "{raised:?}");
    let body = &raised[0];
    assert!(
        body.contains("CPU load has stayed at or above the admission gate for 11 minutes"),
        "{body}"
    );
    assert!(body.contains("on 1 CPUs"), "{body}");
    assert!(body.contains("the gate is 1024 per CPU"), "{body}");
    assert!(
        body.contains("Heavy jobs running, by ship: upkeep 1."),
        "{body}"
    );
    // The light run is named, so the owner can have it marked heavy; the
    // heavy build is not among them.
    assert!(body.contains("heavy:false"), "{body}");
    assert!(body.contains("upkeep sim-ui"), "{body}");
    assert!(!body.contains("upkeep build"), "{body}");
    assert!(body.contains("max_heavy_jobs is unset"), "{body}");

    // Two quiet minutes between jobs do not clear it.
    ledger.execute("DELETE FROM load_samples", []).unwrap();
    loads(&ledger, 11, 2, 1e9);
    loads(&ledger, 1, 0, 0.0);
    laptop.pass("upkeep", &ssh);
    assert_eq!(notices().len(), 1, "{:?}", notices());

    // Six quiet minutes do, once.
    ledger.execute("DELETE FROM load_samples", []).unwrap();
    loads(&ledger, 11, 7, 1e9);
    loads(&ledger, 6, 0, 0.0);
    laptop.pass("upkeep", &ssh);
    laptop.pass("upkeep", &ssh);
    let all = notices();
    assert_eq!(all.len(), 2, "{all:?}");
    assert!(all[1].contains("Cleared"), "{all:?}");
    assert!(all[1].contains("load"), "{all:?}");

    for id in ["build", "sim-ui"] {
        laptop.ok("upkeep", "job.cancel", json!({"id":id}));
    }
}

#[test]
fn a_machines_owner_hears_of_its_problems_and_may_stow_it() {
    let (_temp, laptop, _mac, ssh) = fixture();
    laptop.ship("upkeep");
    laptop.ready();
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
fn a_resume_of_all_ends_requests_bound_to_each_machines_stow() {
    // `--all` reaches each machine's own resume, so a request bound to either
    // machine's stow ends when that stow lifts.
    let (_temp, laptop, mac, _ssh) = fixture();
    let mac_ssh = remote_ssh(&mac);
    assert!(link(&laptop, &mac, &mac_ssh).status.success());
    let stow_of = |machine: &Machine, ship: &str| -> String {
        machine.ok(ship, "stow.start", json!({}));
        machine.ok(ship, "stow.status", json!({}))["stow"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let ours = stow_of(&laptop, "liaison");
    let theirs = stow_of(&mac, "fitforge");
    let bind = |machine: &Machine, ship: &str, id: &str, stow: &str| {
        let sent = machine.ok(
            ship,
            "mail.send",
            json!({"id": id, "from": ship, "to": "captain", "kind": "request",
                "body": "Resume the stow?", "payload": {"depends_on": {"stow": stow}}}),
        );
        assert_eq!(sent["state"], "open", "{sent}");
    };
    bind(&laptop, "liaison", "laptop-bound", &ours);
    bind(&mac, "fitforge", "mac-bound", &theirs);

    let all = laptop.run("liaison", &["stow", "resume", "--all"], Some(&mac_ssh));
    assert!(all.status.success(), "{}", text(&all));
    let state = |machine: &Machine, ship: &str, id: &str| {
        machine.ok(ship, "mail.show", json!({"id": id}))["state"].clone()
    };
    assert_eq!(state(&laptop, "liaison", "laptop-bound"), "withdrawn");
    assert_eq!(state(&mac, "fitforge", "mac-bound"), "withdrawn");
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
fn a_request_bound_to_a_linked_machines_stow_ends_when_that_machine_resumes() {
    let (_temp, laptop, mac, _ssh) = fixture();
    // The remote ssh carries mac's own home and ledger, as a real remote host
    // does, so the gateway answers for mac's stows and not the laptop's.
    let mac_ssh = remote_ssh(&mac);
    assert!(link(&laptop, &mac, &mac_ssh).status.success());
    mac.ok("fitforge", "stow.start", json!({}));
    let stow = mac.ok("fitforge", "stow.status", json!({}))["stow"]
        .as_str()
        .unwrap()
        .to_owned();

    // A stow this home has not heard of yet is a binding it may record.
    let bound = laptop.ok(
        "liaison",
        "mail.send",
        json!({"id": "peer-bound", "from": "liaison", "to": "captain", "kind": "request",
            "body": "Resume the stow on mac?", "payload": {"depends_on": {"stow": stow,
            "machine": "mac"}}}),
    );
    assert_eq!(bound["state"], "open", "{bound}");

    // An exchange records the peer's stows; a stow still in force holds the
    // request open.
    let exchanged = collect_as(&laptop, "liaison", &mac_ssh);
    assert_eq!(
        exchanged["collected"][0]["requests_ended"], 0,
        "{exchanged}"
    );
    let listed = laptop.ok("liaison", "machine.list", json!({}));
    let macs = listed["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "mac")
        .unwrap()
        .clone();
    assert_eq!(macs["stows"]["active"], json!([stow]), "{macs}");
    assert_eq!(
        laptop.ok("liaison", "mail.show", json!({"id": "peer-bound"}))["state"],
        "open"
    );

    // The peer's resume arrives with the next exchange and ends it.
    mac.ok("fitforge", "stow.resume", json!({}));
    let exchanged = collect_as(&laptop, "liaison", &mac_ssh);
    assert_eq!(
        exchanged["collected"][0]["requests_ended"], 1,
        "{exchanged}"
    );
    let ended = laptop.ok("liaison", "mail.show", json!({"id": "peer-bound"}));
    assert_eq!(ended["state"], "withdrawn", "{ended}");
    let reason = ended["end_reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!("stow {stow} on mac resumed at ")),
        "{ended}"
    );
    let closing = notices(&laptop, "liaison");
    assert_eq!(closing.len(), 1, "{closing:?}");
    assert_eq!(closing[0]["correlation"], "peer-bound");
    assert_eq!(closing[0]["body"], reason);

    // A repeat exchange finds nothing open: no second closing notice.
    let exchanged = collect_as(&laptop, "liaison", &mac_ssh);
    assert_eq!(
        exchanged["collected"][0]["requests_ended"], 0,
        "{exchanged}"
    );
    assert_eq!(
        notices(&laptop, "liaison").len(),
        1,
        "a second notice was written"
    );
}

#[test]
fn a_request_bound_to_a_stow_the_linked_machine_never_had_is_ended_as_unknown() {
    let (_temp, laptop, mac, _ssh) = fixture();
    let mac_ssh = remote_ssh(&mac);
    assert!(link(&laptop, &mac, &mac_ssh).status.success());

    // No stow in force on mac: the binding was a guess and cannot sit forever.
    let bound = laptop.ok(
        "liaison",
        "mail.send",
        json!({"id": "unknown-bound", "from": "liaison", "to": "captain", "kind": "request",
            "body": "Resume the stow on mac?", "payload": {"depends_on": {"stow": "stow-0000000",
            "machine": "mac"}}}),
    );
    assert_eq!(bound["state"], "open", "{bound}");
    let exchanged = collect_as(&laptop, "liaison", &mac_ssh);
    assert_eq!(
        exchanged["collected"][0]["requests_ended"], 1,
        "{exchanged}"
    );
    let ended = laptop.ok("liaison", "mail.show", json!({"id": "unknown-bound"}));
    assert_eq!(ended["state"], "withdrawn", "{ended}");
    assert_eq!(
        ended["end_reason"], "stow stow-0000000 not known on mac",
        "{ended}"
    );
    assert_eq!(notices(&laptop, "liaison").len(), 1);

    // A binding to a machine this fleet is not linked with is refused outright.
    let unlinked = laptop.run(
        "liaison",
        &[
            "mail.send",
            "--json",
            &json!({"id": "elsewhere", "from": "liaison", "to": "captain", "kind": "request",
            "body": "Which machine?", "payload": {"depends_on": {"stow": "stow-0000000",
            "machine": "nowhere"}}})
            .to_string(),
        ],
        None,
    );
    assert!(!unlinked.status.success());
    assert!(
        text(&unlinked).contains("no linked machine named nowhere"),
        "{}",
        text(&unlinked)
    );
}

#[test]
fn a_request_on_a_linked_machine_bound_to_this_machines_stow_ends_when_it_resumes() {
    let (_temp, laptop, mac, _ssh) = fixture();
    let mac_ssh = remote_ssh(&mac);
    assert!(link(&laptop, &mac, &mac_ssh).status.success());
    // This machine stows; the link is one-way, so the other machine never
    // connects back and learns of it in the exchange the laptop makes.
    laptop.ok("liaison", "stow.start", json!({}));
    let stow = laptop.ok("liaison", "stow.status", json!({}))["stow"]
        .as_str()
        .unwrap()
        .to_owned();
    let bound = mac.ok(
        "fitforge",
        "mail.send",
        json!({"id": "peer-bound", "from": "fitforge", "to": "captain", "kind": "request",
            "body": "Resume the stow on the laptop?", "payload": {"depends_on": {"stow": stow,
            "machine": "laptop"}}}),
    );
    assert_eq!(bound["state"], "open", "{bound}");

    collect_as(&laptop, "liaison", &mac_ssh);
    assert_eq!(
        mac.ok("fitforge", "mail.show", json!({"id": "peer-bound"}))["state"],
        "open",
        "a stow the peer reports as in force holds its request"
    );

    laptop.ok("liaison", "stow.resume", json!({}));
    collect_as(&laptop, "liaison", &mac_ssh);
    let ended = mac.ok("fitforge", "mail.show", json!({"id": "peer-bound"}));
    assert_eq!(ended["state"], "withdrawn", "{ended}");
    assert!(
        ended["end_reason"]
            .as_str()
            .unwrap()
            .contains(&format!("stow {stow} on laptop resumed at ")),
        "{ended}"
    );
    assert_eq!(notices(&mac, "fitforge").len(), 1);
}

/// Drops the stows field from a gateway answer, as a peer that predates it
/// sends it: the stows object is the only one with an `active` list.
const WITHOUT_STOWS: &str = r#"| sed -E 's/,"stows":\{"active":\[[^]]*\],"resumed":\[[^]]*\]\}//'"#;

#[test]
fn a_peer_too_old_to_report_its_stows_leaves_a_bound_request_open() {
    let (_temp, laptop, mac, _ssh) = fixture();
    let old_ssh = remote_ssh_with(&mac, "old-ssh", WITHOUT_STOWS);
    assert!(link(&laptop, &mac, &old_ssh).status.success());
    mac.ok("fitforge", "stow.start", json!({}));
    let stow = mac.ok("fitforge", "stow.status", json!({}))["stow"]
        .as_str()
        .unwrap()
        .to_owned();
    let bound = laptop.ok(
        "liaison",
        "mail.send",
        json!({"id": "old-bound", "from": "liaison", "to": "captain", "kind": "request",
            "body": "Resume the stow on mac?", "payload": {"depends_on": {"stow": stow,
            "machine": "mac"}}}),
    );
    assert_eq!(bound["state"], "open", "{bound}");

    // With no stows field there is nothing to record and nothing to end, even
    // for a stow the peer has since resumed: the request waits for a peer that
    // reports, rather than being ended as unknown.
    let exchanged = collect_as(&laptop, "liaison", &old_ssh);
    assert_eq!(
        exchanged["collected"][0]["requests_ended"], 0,
        "{exchanged}"
    );
    mac.ok("fitforge", "stow.resume", json!({}));
    let exchanged = collect_as(&laptop, "liaison", &old_ssh);
    assert_eq!(
        exchanged["collected"][0]["requests_ended"], 0,
        "{exchanged}"
    );
    assert_eq!(
        laptop.ok("liaison", "mail.show", json!({"id": "old-bound"}))["state"],
        "open"
    );
    assert!(notices(&laptop, "liaison").is_empty());
}

#[test]
fn a_request_between_two_homes_bound_to_a_local_stow_ends_with_one_closing_notice() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    laptop.ok("liaison", "stow.start", json!({}));
    let stow = laptop.ok("liaison", "stow.status", json!({}))["stow"]
        .as_str()
        .unwrap()
        .to_owned();
    let sent = laptop.run(
        "liaison",
        &[
            "mail.send",
            "--json",
            &json!({"id": "cross-bound", "from": "liaison", "to": "fitforge", "kind": "request",
            "body": "Build once the stow lifts?", "payload": {"depends_on": {"stow": stow}}})
            .to_string(),
        ],
        Some(&ssh),
    );
    assert!(sent.status.success(), "{}", text(&sent));
    let sent: Value = serde_json::from_slice(&sent.stdout).unwrap();
    assert_eq!(sent["delivery"], "delivered", "{sent}");

    // The sender's home closes its row; the closing notice travels to the
    // other home, which then holds the request as withdrawn.
    laptop.ok("liaison", "stow.resume", json!({}));
    assert_eq!(
        laptop.ok("liaison", "mail.show", json!({"id": "cross-bound"}))["state"],
        "withdrawn"
    );
    laptop.pass("liaison", &ssh);
    assert_eq!(
        mac.ok("fitforge", "mail.show", json!({"id": "cross-bound"}))["state"],
        "withdrawn"
    );
    let closing = |mac: &Machine| {
        messages(mac, "fitforge", "notice")
            .into_iter()
            .filter(|m| m["correlation"] == "cross-bound")
            .count()
    };
    assert_eq!(closing(&mac), 1);

    // Another exchange writes no second notice, and a resume on the receiving
    // machine does not end its own copy: it is closed by the sender's home.
    laptop.pass("liaison", &ssh);
    collect_as(&laptop, "liaison", &ssh);
    mac.ok("fitforge", "stow.start", json!({}));
    mac.ok("fitforge", "stow.resume", json!({}));
    assert_eq!(closing(&mac), 1);
    assert_eq!(
        mac.ok("fitforge", "mail.show", json!({"id": "cross-bound"}))["state"],
        "withdrawn"
    );
}

#[test]
fn a_request_collected_from_another_machine_is_not_closed_by_this_machines_resume() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    // The binding is mac's own stow, so the copy that reaches the laptop
    // is a received copy, and this machine's resume must leave it open.
    mac.ok("fitforge", "stow.start", json!({}));
    let mac_stow = mac.ok("fitforge", "stow.status", json!({}))["stow"]
        .as_str()
        .unwrap()
        .to_owned();
    let sent = mac.ok(
        "fitforge",
        "mail.send",
        json!({"id": "collected-bound", "from": "fitforge", "to": "liaison", "kind": "request",
            "body": "Build once mac resumes?", "payload": {"depends_on": {"stow": mac_stow}}}),
    );
    assert_eq!(sent["state"], "open", "{sent}");
    collect_as(&laptop, "liaison", &ssh);
    assert_eq!(
        laptop.ok("liaison", "mail.show", json!({"id": "collected-bound"}))["state"],
        "open"
    );

    laptop.ok("liaison", "stow.start", json!({}));
    laptop.ok("liaison", "stow.resume", json!({}));
    let copy = laptop.ok("liaison", "mail.show", json!({"id": "collected-bound"}));
    assert_eq!(copy["state"], "open", "{copy}");
    assert_eq!(copy["delivery"], "received", "{copy}");
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

/// The unit a ship's dispatcher was installed with under `machine`'s home.
fn unit_text(machine: &Machine, ship: &str) -> String {
    let dir = machine.root.join(".config/systemd/user");
    let mut units = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&format!("privateer-{ship}-")))
        });
    let unit = units.next().unwrap_or_else(|| panic!("no unit for {ship}"));
    fs::read_to_string(unit).unwrap()
}

#[test]
fn a_ship_created_for_another_machine_records_the_login_shell_path() {
    use std::os::unix::fs::PermissionsExt;
    let (_temp, laptop, mac, _ssh) = fixture();
    let mac_ssh = remote_ssh(&mac);
    assert!(link(&laptop, &mac, &mac_ssh).status.success());
    // The gateway's own PATH is bare; its login shell knows where harnesses live.
    // The fake shell runs the command on its own PATH and prints nothing
    // else, so nothing else from it can be recorded.
    let login_bin = mac.root.join("login-bin");
    let shell = mac.root.join("login-shell");
    fs::write(
        &shell,
        format!(
            "#!/bin/sh\nPATH='{}:/usr/bin:/bin' exec /bin/sh -c \"$2\"\n",
            login_bin.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
    let systemctl = mac.root.join("systemctl");
    fs::write(&systemctl, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();
    let service = [
        ("PRIVATEER_SERVICE", "systemd"),
        ("PRIVATEER_SYSTEMCTL", systemctl.to_str().unwrap()),
        ("SHELL", shell.to_str().unwrap()),
    ];

    let response = mac.gateway_with(
        "ship.create",
        json!({"name": "bench", "from_machine": "laptop", "created_by": "captain", "harness": "codex"}),
        &service,
    );
    assert_eq!(response["ok"], true, "{response}");
    let unit = unit_text(&mac, "bench");
    assert!(
        unit.contains(&login_bin.display().to_string()),
        "a gateway-made unit records the login shell's PATH:\n{unit}"
    );
    // The gateway's own PATH is still first.
    assert!(
        unit.contains(&format!(
            "PATH={}:/usr/bin:/bin",
            mac.root.join("bin").display()
        )),
        "{unit}"
    );

    // A ship made here, with no machine, records only this process's PATH.
    let local = laptop
        .command(
            "liaison",
            &[
                "ship.create",
                "--json",
                r#"{"name":"nearby","harness":"codex"}"#,
            ],
            Some(&mac_ssh),
        )
        .envs(service.iter().copied())
        .output()
        .unwrap();
    assert!(local.status.success(), "{}", text(&local));
    let unit = unit_text(&laptop, "nearby");
    assert!(!unit.contains("login-bin"), "{unit}");
    assert!(
        unit.contains(&format!(
            "PATH={}:/usr/bin:/bin",
            laptop.root.join("bin").display()
        )),
        "{unit}"
    );
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
    // Its Claude settings name the laptop's binary, as they did before the move.
    let stale = "/home/laptop/.cargo/bin/privateer";
    fs::create_dir_all(laptop.folder("scout").join(".claude")).unwrap();
    fs::write(
        laptop.folder("scout").join(".claude/settings.json"),
        json!({"permissions": {"allow": [format!("Bash({stale}:*)")]}}).to_string(),
    )
    .unwrap();

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
    // The Mac's copy of its settings points at the binary running here.
    let settings: Value = serde_json::from_slice(
        &fs::read(mac.folder("scout").join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_privateer");
    assert_eq!(
        settings["permissions"]["allow"],
        json!([format!("Bash({bin}:*)")]),
        "{settings}"
    );
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

/// An SSH-shaped transport with real gateways and deterministic response loss.
fn move_transport(machine: &Machine, faults: Value) -> PathBuf {
    let path = machine.root.join("move-ssh");
    fs::write(&path, include_str!("fixtures/move_transport.py")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(machine.root.join("move-faults.json"), faults.to_string()).unwrap();
    path
}

fn move_calls(machine: &Machine, command: &str) -> usize {
    fs::read_to_string(machine.root.join("move-calls.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|call| call["command"] == command)
        .count()
}

fn file_sha(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
}

#[test]
fn move_batches_small_files_and_retries_a_timeout_and_failed_call() {
    let (_temp, laptop, mac, _ssh) = fixture();
    laptop.ready();
    mac.ready();
    laptop.ship("scout");
    let ssh = move_transport(
        &mac,
        json!({"move.write_batch": ["timeout", "fail", "pass"]}),
    );
    assert!(link(&laptop, &mac, &ssh).status.success());
    // A real peer deadline, shortened only in this fixture. The delayed call
    // applies its write before outliving this deadline and losing its response.
    let conn = rusqlite::Connection::open(laptop.registry()).unwrap();
    conn.execute(
        "UPDATE machines SET peer=json_set(peer,'$.timeout_ms',1500) WHERE name='mac'",
        [],
    )
    .unwrap();
    let notes = laptop.folder("scout").join("notes");
    fs::create_dir_all(&notes).unwrap();
    for i in 0..1120 {
        fs::write(notes.join(format!("{i:04}")), vec![b'x'; 100]).unwrap();
    }
    fs::write(notes.join("empty"), []).unwrap();
    fs::write(notes.join("large"), vec![b'z'; 768 * 1024 + 37]).unwrap();
    let moved = laptop.run(
        "liaison",
        &["ship", "move", "scout", "--to", "mac"],
        Some(&ssh),
    );
    assert!(moved.status.success(), "{}", text(&moved));
    for name in (0..1120)
        .map(|i| format!("{i:04}"))
        .chain(["empty".into(), "large".into()])
    {
        assert_eq!(
            file_sha(&notes.join(&name)),
            file_sha(&mac.folder("scout").join("notes").join(&name)),
            "{name}"
        );
    }
    let reads = move_calls(&mac, "move.read_batch");
    let writes = move_calls(&mac, "move.write_batch");
    assert_eq!(reads, 0); // export is local in this direction
    assert!(
        (6..=10).contains(&writes),
        "1123 fixture files plus ship state used {writes} calls (including two retries)"
    );
    assert_eq!(move_calls(&mac, "move.write"), 0);
    let conn = rusqlite::Connection::open(mac.registry()).unwrap();
    let state: String = conn
        .query_row("SELECT state FROM moves WHERE role='import'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(state, "finished");
    assert!(
        !mac.root
            .join("moves")
            .read_dir()
            .map(|mut e| e.next().is_some())
            .unwrap_or(false)
    );
    eprintln!(
        "1123 payload files: {writes} remote batch writes including timeout + failure retries; legacy minimum 1122 remote writes plus large-file chunks"
    );
}

#[test]
fn move_retries_remote_batch_reads_when_bringing_a_ship_here() {
    let (_temp, laptop, mac, _ssh) = fixture();
    laptop.ready();
    mac.ready();
    mac.ship("scout");
    let payload = mac.folder("scout").join("payload");
    fs::write(&payload, vec![b'r'; 400 * 1024]).unwrap();
    let ssh = move_transport(
        &mac,
        json!({"move.read_batch": ["timeout", "fail", "pass"]}),
    );
    assert!(link(&laptop, &mac, &ssh).status.success());
    let conn = rusqlite::Connection::open(laptop.registry()).unwrap();
    conn.execute(
        "UPDATE machines SET peer=json_set(peer,'$.timeout_ms',1500) WHERE name='mac'",
        [],
    )
    .unwrap();
    let moved = laptop.run("liaison", &["ship", "move", "scout", "--here"], Some(&ssh));
    assert!(moved.status.success(), "{}", text(&moved));
    assert_eq!(
        file_sha(&payload),
        file_sha(&laptop.folder("scout").join("payload"))
    );
    let reads = move_calls(&mac, "move.read_batch");
    assert!((4..=10).contains(&reads), "{reads}");
    // The first three requests replay the exact same ranges after timeout and
    // transport failure, not a new cursor that might skip source bytes.
    let calls: Vec<Value> = fs::read_to_string(mac.root.join("move-calls.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|call| call["command"] == "move.read_batch")
        .collect();
    assert_eq!(calls[0]["args"], calls[1]["args"]);
    assert_eq!(calls[1]["args"], calls[2]["args"]);
    assert_eq!(move_calls(&mac, "move.read"), 0);
}

#[test]
fn move_batches_still_check_file_digests_before_activation() {
    let (_temp, laptop, mac, _ssh) = fixture();
    laptop.ready();
    mac.ready();
    laptop.ship("scout");
    let ssh = move_transport(&mac, json!({"move.write_batch": ["corrupt", "pass"]}));
    assert!(link(&laptop, &mac, &ssh).status.success());
    let source = laptop.folder("scout").join("payload");
    fs::write(&source, vec![b'a'; 400 * 1024]).unwrap();
    let before = file_sha(&source);
    let moved = laptop.run(
        "liaison",
        &["ship", "move", "scout", "--to", "mac"],
        Some(&ssh),
    );
    assert!(!moved.status.success());
    assert!(
        text(&moved).contains("incomplete or damaged"),
        "{}",
        text(&moved)
    );
    assert_eq!(file_sha(&source), before);
    assert!(!mac.folder("scout").exists());
    let conn = rusqlite::Connection::open(mac.registry()).unwrap();
    let (id, state): (String, String) = conn
        .query_row("SELECT id,state FROM moves WHERE role='import'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(state, "discarded");
    assert!(!mac.root.join("moves").join(&id).join("import").exists());
}

#[test]
fn a_partial_move_checks_discard_then_retries_with_a_new_snapshot() {
    let (_temp, laptop, mac, _ssh) = fixture();
    laptop.ready();
    mac.ready();
    laptop.ship("scout");
    let source = laptop.folder("scout").join("payload");
    fs::write(&source, vec![b'a'; 700 * 1024]).unwrap();
    let before = file_sha(&source);
    let ssh = move_transport(
        &mac,
        json!({"move.write_batch": ["pass", "fail"], "move.discard": ["fail", "pass"]}),
    );
    assert!(link(&laptop, &mac, &ssh).status.success());
    let moved = laptop.run(
        "liaison",
        &["ship", "move", "scout", "--to", "mac"],
        Some(&ssh),
    );
    assert!(!moved.status.success(), "{}", text(&moved));
    assert_eq!(move_calls(&mac, "move.discard"), 2);
    assert_eq!(file_sha(&source), before);
    let conn = rusqlite::Connection::open(mac.registry()).unwrap();
    let (id, state): (String, String) = conn
        .query_row("SELECT id,state FROM moves WHERE role='import'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(state, "discarded");
    assert!(!mac.root.join("moves").join(&id).join("import").exists());
    assert!(laptop.run("scout", &["status"], None).status.success());
    // A delayed writer must not recreate the removed stage.
    let stale = mac.gateway(
        "move.write_batch",
        json!({"id":id,"chunks":[{"path":"payload","offset":0,"hex":"61"}]}),
    );
    assert_eq!(stale["ok"], false, "{stale}");
    assert!(!mac.root.join("moves").join(&id).join("import").exists());
    fs::write(&source, vec![b'b'; 700 * 1024]).unwrap();
    fs::write(mac.root.join("move-faults.json"), "{}").unwrap();
    let moved = laptop.run(
        "liaison",
        &["ship", "move", "scout", "--to", "mac"],
        Some(&ssh),
    );
    assert!(moved.status.success(), "{}", text(&moved));
    assert_eq!(
        file_sha(&source),
        file_sha(&mac.folder("scout").join("payload"))
    );
}

#[test]
fn unreachable_discard_retains_cleanup_custody_and_stale_receiving_stages_expire() {
    let (_temp, laptop, mac, _ssh) = fixture();
    laptop.ready();
    mac.ready();
    laptop.ship("scout");
    fs::write(
        laptop.folder("scout").join("payload"),
        vec![b'a'; 700 * 1024],
    )
    .unwrap();
    let ssh = move_transport(
        &mac,
        json!({"move.write_batch":["pass","fail"],"move.discard":["fail"]}),
    );
    assert!(link(&laptop, &mac, &ssh).status.success());
    let moved = laptop.run(
        "liaison",
        &["ship", "move", "scout", "--to", "mac"],
        Some(&ssh),
    );
    assert!(!moved.status.success());
    assert!(
        text(&moved).contains("rollback incomplete"),
        "{}",
        text(&moved)
    );
    let local = rusqlite::Connection::open(laptop.registry()).unwrap();
    let state: String = local
        .query_row("SELECT state FROM moves WHERE role='run'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(state, "cleanup");
    let remote = rusqlite::Connection::open(mac.registry()).unwrap();
    let id: String = remote
        .query_row("SELECT id FROM moves WHERE role='import'", [], |r| r.get(0))
        .unwrap();
    let stage = mac.root.join("moves").join(&id).join("import");
    assert!(stage.is_dir());
    remote
        .execute("UPDATE moves SET updated_ms=0 WHERE role='import'", [])
        .unwrap();
    let status = mac.gateway("move.status", json!({"id":id}));
    assert_eq!(status["result"]["state"], "discarded", "{status}");
    assert!(!stage.exists());
    fs::write(mac.root.join("move-faults.json"), "{}").unwrap();
    let cleanup = laptop.run(
        "liaison",
        &["ship", "move", "scout", "--to", "mac"],
        Some(&ssh),
    );
    assert!(!cleanup.status.success());
    assert!(
        text(&cleanup).contains("cleanup completed"),
        "{}",
        text(&cleanup)
    );
    let moved = laptop.run(
        "liaison",
        &["ship", "move", "scout", "--to", "mac"],
        Some(&ssh),
    );
    assert!(moved.status.success(), "{}", text(&moved));
}

#[test]
fn discarded_moves_fence_delayed_begin_and_only_receiving_stages_expire() {
    let (_temp, laptop, mac, _ssh) = fixture();
    laptop.ready();
    mac.ready();
    laptop.ship("scout");
    let exported = laptop.gateway_from("liaison", "move.export", json!({"id":"delayed-begin","ship":"scout"}), &[]);
    assert_eq!(exported["ok"], true, "{exported}");
    let discarded = mac.gateway("move.discard", json!({"id":"delayed-begin"}));
    assert_eq!(discarded["result"]["state"], "discarded", "{discarded}");
    let late = mac.gateway(
        "move.begin",
        json!({"id":"delayed-begin","manifest":exported["result"]}),
    );
    assert_eq!(late["ok"], false, "{late}");
    assert!(!mac.root.join("moves/delayed-begin/import").exists());
    let aborted = laptop.gateway_from("liaison", "move.abort", json!({"id":"delayed-begin"}), &[]);
    assert_eq!(aborted["result"]["state"], "aborted", "{aborted}");
    // A stale placed row is not a receiving-stage orphan. Expiry must not
    // touch either its staging debris or its already laid-out folder.
    let stage = mac.root.join("moves/placed-old/import");
    fs::create_dir_all(&stage).unwrap();
    fs::write(stage.join("keep"), "placed data").unwrap();
    let conn = rusqlite::Connection::open(mac.registry()).unwrap();
    conn.execute("INSERT INTO moves(id,role,ship,state,detail,updated_ms) VALUES('placed-old','import','scout','placed','{}',0)", []).unwrap();
    let status = mac.gateway("move.status", json!({"id":"placed-old"}));
    assert_eq!(status["result"]["state"], "placed", "{status}");
    assert_eq!(
        fs::read_to_string(stage.join("keep")).unwrap(),
        "placed data"
    );
}

#[test]
fn mixed_build_moves_refuse_before_exporting() {
    let (_temp, laptop, mac, _ssh) = fixture();
    laptop.ready();
    mac.ready();
    laptop.ship("scout");
    let ssh = move_transport(&mac, json!({"move.protocol":["old"]}));
    assert!(link(&laptop, &mac, &ssh).status.success());
    let moved = laptop.run(
        "liaison",
        &["ship", "move", "scout", "--to", "mac"],
        Some(&ssh),
    );
    assert!(!moved.status.success());
    assert!(
        text(&moved).contains("incompatible move transfer"),
        "{}",
        text(&moved)
    );
    let conn = rusqlite::Connection::open(laptop.registry()).unwrap();
    let exports: i64 = conn
        .query_row("SELECT count(*) FROM moves WHERE role='export'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(exports, 0);
    assert!(
        !laptop
            .folder("scout")
            .join(".privateer/moved.json")
            .exists()
    );
}

#[test]
fn a_move_whose_login_shell_fails_says_so_in_its_reply() {
    let (_temp, laptop, mac, _ssh) = fixture();
    let mac_ssh = remote_ssh(&mac);
    ship_with_project(&laptop, &mac, "scout");
    assert!(link(&laptop, &mac, &mac_ssh).status.success());
    // The fake ssh passes SHELL through: both machines' login shells fail,
    // so each arriving dispatcher keeps only its own process's PATH.
    let shell = laptop.root.join("failing-login-shell");
    fs::write(&shell, "#!/bin/sh\nexit 3\n").unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
    let said = "the login shell exited with 3, so the dispatcher records only \
                this process's PATH";

    let asked = json!({"name": "scout", "to": "mac"}).to_string();
    let moved = laptop
        .command("liaison", &["ship.move", "--json", &asked], Some(&mac_ssh))
        .env("SHELL", &shell)
        .output()
        .unwrap();
    assert!(moved.status.success(), "{}", text(&moved));
    let reply: Value = serde_json::from_slice(&moved.stdout).unwrap();
    let why = reply["login_shell"]
        .as_str()
        .unwrap_or_else(|| panic!("{reply}"));
    assert!(why.starts_with(said), "{why}");

    // The captain's text reply says it too.
    let back = laptop
        .command(
            "liaison",
            &["ship", "move", "scout", "--here"],
            Some(&mac_ssh),
        )
        .env("SHELL", &shell)
        .output()
        .unwrap();
    assert!(back.status.success(), "{}", text(&back));
    assert!(
        text(&back).contains(&format!("  note: {said}")),
        "{}",
        text(&back)
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
/// Opens a ship's database on one machine, as the move leaves it.
fn ship_database(machine: &Machine, ship: &str) -> rusqlite::Connection {
    rusqlite::Connection::open(machine.folder(ship).join(".privateer/privateer.sqlite")).unwrap()
}

#[test]
fn a_ship_move_settles_its_unfinished_records_on_the_new_machine() {
    let (_temp, laptop, mac, _ssh) = fixture();
    let mac_ssh = remote_ssh(&mac);
    ship_with_project(&laptop, &mac, "scout");
    assert!(link(&laptop, &mac, &mac_ssh).status.success());
    // A crew parked on a job whose ledger is the laptop's. A park is not busy,
    // so the ship moves with it, and the record must not look live on the Mac.
    let laptop_ledger = laptop.root.join("laptop-state/resources.sqlite");
    let parked = json!({"id": "parked", "task_id": "task", "incarnation": "inc-1",
        "command": ["sleep", "1"], "cwd": laptop.folder("scout"), "action": "spawn",
        "heavy": false, "heavy_declared": false, "timeout_secs": 30,
        "ledger": laptop_ledger, "status": "parked", "created_ms": 1,
        "cancel_requested": false, "group_anchors": [], "escaped": []});
    ship_database(&laptop, "scout")
        .execute(
            "INSERT INTO jobs(id, record) VALUES('parked', ?1)",
            [parked.to_string()],
        )
        .unwrap();

    let moved = laptop.run(
        "liaison",
        &["ship", "move", "scout", "--to", "mac"],
        Some(&mac_ssh),
    );
    assert!(moved.status.success(), "{}", text(&moved));

    let record = mac.ok("scout", "job.status", json!({"id": "parked"}));
    assert_eq!(record["status"], "failed", "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap()
            .starts_with("left behind when the ship moved from"),
        "{record}"
    );
    let live: i64 = ship_database(&mac, "scout")
        .query_row(
            "SELECT COUNT(*) FROM jobs
             WHERE json_extract(record,'$.status') NOT IN ('succeeded','failed','cancelled','timed_out','resumed')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(live, 0);
    let audited: i64 = ship_database(&mac, "scout")
        .query_row(
            "SELECT COUNT(*) FROM audit WHERE kind='job.foreign'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(audited, 1);
}

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
fn the_captain_annotates_a_report_notice_from_the_other_machine() {
    let (_temp, laptop, mac, ssh) = fixture();
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    let reported = mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "notice", "body": "Audit done",
            "payload": {"report": {"title": "Audit", "format": "markdown",
                "content": "Ship it on Friday."}}}),
    );
    let id = reported["id"].as_str().unwrap();
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["delivered"], 1, "{pass}");

    // The captain annotates the collected copy by prefix; the notice crosses
    // back to the ship's own home, in the report's thread.
    let annotations = json!([{"anchor": {"quote": "Friday"}, "comment": "Make it Monday."}]);
    let sent = laptop.ok_over(
        "liaison",
        "captain.reply",
        json!({"id": &id[..8], "payload": {"annotations": annotations}}),
        &ssh,
    );
    assert_eq!(sent["id"], id, "{sent}");
    assert_eq!(sent["to"], "fitforge");
    assert_eq!(sent["reply"]["kind"], "notice");
    let received: Vec<Value> = messages(&mac, "fitforge", "notice")
        .into_iter()
        .filter(|m| m["from"] == "captain")
        .collect();
    assert_eq!(received.len(), 1, "{received:?}");
    assert_eq!(received[0]["correlation"], id);
    assert_eq!(received[0]["payload"]["annotations"], annotations);
    // Neither copy of the report notice is changed by it.
    for (machine, ship) in [(&laptop, "liaison"), (&mac, "fitforge")] {
        let report = machine.ok(ship, "mail.show", json!({"id": id}));
        assert!(report["state"].is_null(), "{report}");
    }
}

#[test]
fn a_ship_reply_to_a_request_this_machine_never_held_still_reaches_the_captain() {
    let (_temp, laptop, mac, ssh) = fixture();
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    // The captain asks fitforge from the Mac itself, so the request is
    // written only there; fitforge's answer closes it there.
    let asked = mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "captain", "to": "fitforge", "kind": "request", "body": "Upload build 12?"}),
    );
    let answered = mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "reply",
            "correlation": asked["id"], "body": "Yes, uploaded"}),
    );
    let request = mac.ok("fitforge", "mail.show", json!({"id": asked["id"]}));
    assert_eq!(request["state"], "answered", "{request}");

    // The captain's home here reads the answer, not a refusal of it.
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["delivered"], 1, "{pass}");
    assert_eq!(pass["machines"][0]["refused"], 0, "{pass}");
    let inbox = captain_mail(&laptop, "liaison");
    assert_eq!(inbox.len(), 1, "{inbox:?}");
    assert_eq!(inbox[0]["id"], answered["id"]);
    assert_eq!(inbox[0]["kind"], "reply");
    assert_eq!(inbox[0]["body"], "Yes, uploaded");
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

fn captain_bodies(machine: &Machine, ship: &str) -> Vec<String> {
    machine.ok(ship, "mail.inbox", json!({"address": "captain"}))["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["body"].as_str().map(str::to_owned))
        .collect()
}

fn collect(laptop: &Machine, ssh: &Path) -> String {
    let collected = laptop.run("liaison", &["machine", "collect"], Some(ssh));
    assert!(collected.status.success(), "{}", text(&collected));
    text(&collected)
}

#[test]
fn the_captains_desk_can_sit_on_the_machine_this_one_reaches() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("scout");
    assert!(link(&laptop, &mac, &ssh).status.success());
    let moved = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved.status.success(), "{}", text(&moved));

    // Every laptop home's mail to the captain goes to the Mac's captain home.
    let asked = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "scout", "to": "captain", "kind": "request", "body": "Rotate the deploy key?"}),
    );
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["captain_pushed"], 1, "{pass}");
    assert_eq!(captain_bodies(&mac, "fitforge"), ["Rotate the deploy key?"]);

    // The Mac's own captain mail stays there.
    mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "notice", "body": "Build 12 is up"}),
    );
    assert!(collect(&laptop, &ssh).contains("\"delivered\": 0"));
    assert!(captain_bodies(&laptop, "liaison").is_empty());

    // The captain answers at the Mac; the answer comes back with the next
    // collection and closes the request where it was asked.
    mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "captain", "to": "scout", "kind": "reply",
               "correlation": asked["id"], "body": "Yes, rotate it"}),
    );
    assert!(collect(&laptop, &ssh).contains("\"delivered\": 1"));
    let request = laptop.ok("scout", "mail.show", json!({"id": asked["id"]}));
    assert_eq!(request["state"], "answered", "{request}");
}

#[test]
fn the_captain_reads_and_answers_at_a_desk_on_another_machine() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("scout");
    assert!(link(&laptop, &mac, &ssh).status.success());
    let ship = mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "request", "body": "Ship build 14?"}),
    );
    let rename = mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "request", "body": "Rename the scheme?"}),
    );
    let bodies = |listed: &Value| -> Vec<String> {
        listed["waiting"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["mail"]["body"].as_str().unwrap().to_owned())
            .collect()
    };
    // Asked on the laptop before the desk moves: the desk never holds it.
    let rotate = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "scout", "to": "captain", "kind": "request", "body": "Rotate the key?"}),
    );
    // The laptop alone has not collected the Mac's asks.
    assert_eq!(
        bodies(&laptop.ok("liaison", "captain.waiting", json!({}))),
        ["Rotate the key?"]
    );

    assert!(
        laptop
            .run("liaison", &["machine", "captain", "mac"], None)
            .status
            .success()
    );

    // With the desk on the Mac, the captain's commands on the laptop run
    // there, and the ask only the laptop holds is still listed.
    let waiting = laptop.ok_over("liaison", "captain.waiting", json!({}), &ssh);
    assert_eq!(
        bodies(&waiting),
        ["Ship build 14?", "Rename the scheme?", "Rotate the key?"]
    );
    // It is answered where it is held.
    laptop.ok_over(
        "liaison",
        "captain.reply",
        json!({"id": rotate["id"], "body": "Yes"}),
        &ssh,
    );
    let rotated = laptop.ok("scout", "mail.show", json!({"id": rotate["id"]}));
    assert_eq!(rotated["state"], "answered", "{rotated}");
    laptop.ok_over(
        "liaison",
        "captain.reply",
        json!({"id": ship["id"], "body": "Yes, ship it"}),
        &ssh,
    );
    laptop.ok_over(
        "liaison",
        "captain.dismiss",
        json!({"id": rename["id"]}),
        &ssh,
    );

    let state = |id: &Value| mac.ok("fitforge", "mail.show", json!({"id": id}))["state"].clone();
    assert_eq!(state(&ship["id"]), "answered");
    assert_eq!(state(&rename["id"]), "dismissed");
    assert!(bodies(&laptop.ok_over("liaison", "captain.waiting", json!({}), &ssh)).is_empty());
    let feed = laptop.ok_over("liaison", "captain.feed", json!({}), &ssh);
    assert!(feed.to_string().contains("Yes, ship it"), "{feed}");
}

#[test]
fn the_captain_writes_to_any_ship_from_any_folder_and_machine() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("scout");
    assert!(link(&laptop, &mac, &ssh).status.success());
    let bodies = |machine: &Machine, ship: &str, kind: &str| -> Vec<String> {
        messages(machine, ship, kind)
            .iter()
            .filter(|m| m["from"] == "captain")
            .map(|m| m["body"].as_str().unwrap().to_owned())
            .collect()
    };

    // A ship on this machine, from another ship's folder: written in its home.
    let sent = laptop.ok(
        "liaison",
        "captain.send",
        json!({"to": "scout", "body": "Rotate the key."}),
    );
    assert_eq!(sent["home"], "scout", "{sent}");
    assert_eq!(bodies(&laptop, "scout", "notice"), ["Rotate the key."]);

    // A ship on the linked machine: from the captain's home, over the link.
    let sent = laptop.ok_over(
        "scout",
        "captain.send",
        json!({"to": "fitforge", "kind": "request", "body": "Ship build 14?"}),
        &ssh,
    );
    assert_eq!(sent["mail"]["delivery"], "delivered", "{sent}");
    assert_eq!(bodies(&mac, "fitforge", "request"), ["Ship build 14?"]);

    // With the desk on the Mac, the captain writes from there.
    assert!(
        laptop
            .run("liaison", &["machine", "captain", "mac"], None)
            .status
            .success()
    );
    laptop.ok_over(
        "scout",
        "captain.send",
        json!({"to": "fitforge", "body": "Build 14 is out."}),
        &ssh,
    );
    assert_eq!(bodies(&mac, "fitforge", "notice"), ["Build 14 is out."]);

    // Only notices and requests, and only to a ship that exists.
    for args in [
        json!({"to": "fitforge", "kind": "reply", "body": "x"}),
        json!({"to": "nobody", "body": "x"}),
        json!({"to": "scout", "body": "  "}),
    ] {
        let output = laptop.run(
            "scout",
            &["captain.send", "--json", &args.to_string()],
            Some(&ssh),
        );
        assert!(!output.status.success(), "{args}: {}", text(&output));
    }
}

#[test]
fn captain_send_keeps_local_ships_local_with_a_desk_over_a_one_way_link() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("scout");
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    let moved = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved.status.success(), "{}", text(&moved));
    let remote = remote_ssh(&mac);

    // The Mac only knows the reverse link: it cannot call the laptop.
    let machines = mac.ok("fitforge", "machine.list", json!({}));
    assert_eq!(
        machines["machines"][0]["direction"], "reaches here",
        "{machines}"
    );

    // Both kinds of captain mail go straight into the local ship's own home,
    // even when sent from another folder. No desk connection is needed.
    for kind in ["notice", "request"] {
        let sent = laptop.ok_over(
            "liaison",
            "captain.send",
            json!({"to": "scout", "kind": kind, "body": "Rotate the key."}),
            &remote,
        );
        assert_eq!(sent["home"], "scout", "{sent}");
        let inbox = messages(&laptop, "scout", kind);
        assert_eq!(inbox.len(), 1, "{inbox:?}");
        assert_eq!(inbox[0]["id"], sent["mail"]["id"]);
        assert_eq!(inbox[0]["from"], "captain");
        assert_eq!(inbox[0]["body"], "Rotate the key.");
    }
    assert_eq!(connections(&mac), 0);

    // file.put already chooses local delivery. An attachment must stay local
    // with its message rather than trying to upload back from the desk.
    let source = laptop.root.join("plan.txt");
    fs::write(&source, b"Rotate the key before release.\n").unwrap();
    let stored = laptop.ok_over(
        "liaison",
        "file.put",
        json!({"to": "scout", "name": "plan.txt", "content_file": source}),
        &remote,
    );
    let path = Path::new(stored["path"].as_str().unwrap());
    assert!(path.starts_with(laptop.folder("scout")));
    assert_eq!(fs::read(path).unwrap(), fs::read(source).unwrap());
    let sent = laptop.ok_over(
        "liaison",
        "captain.send",
        json!({"to": "scout", "body": "The plan is attached.",
            "payload": {"files": [{"file_id": stored["file_id"]}]}}),
        &remote,
    );
    let delivered = laptop.ok("scout", "mail.show", json!({"id": sent["mail"]["id"]}));
    assert_eq!(
        delivered["payload"]["files"][0]["file_id"],
        stored["file_id"]
    );
    assert_eq!(connections(&mac), 0);

    // A remote target still goes through the desk, which chooses its own
    // target home. A captain already at that desk also sends locally.
    let sent = laptop.ok_over(
        "scout",
        "captain.send",
        json!({"to": "fitforge", "body": "Build 14 is out."}),
        &remote,
    );
    assert_eq!(sent["home"], "fitforge", "{sent}");
    assert_eq!(connections(&mac), 1);
    let at_desk = mac.ok(
        "fitforge",
        "captain.send",
        json!({"to": "fitforge", "body": "Check the release."}),
    );
    assert_eq!(at_desk["home"], "fitforge", "{at_desk}");
    let inbox = messages(&mac, "fitforge", "notice");
    for sent in [sent, at_desk] {
        assert!(
            inbox
                .iter()
                .any(|m| m["id"] == sent["mail"]["id"] && m["from"] == "captain"),
            "{inbox:?}"
        );
    }
}

#[test]
fn captain_send_uses_the_desk_for_a_ship_on_a_third_machine() {
    let (_temp, laptop, mac, ssh) = fixture();
    let server = Machine {
        root: laptop.root.parent().unwrap().join("server"),
    };
    server.ship("harbor");
    let registry = server.registry();
    let linked = mac.run(
        "fitforge",
        &[
            "machine",
            "add",
            "server",
            "test-host",
            "--executable",
            support::BIN,
            "--remote-fleet",
            registry.to_str().unwrap(),
            "--as",
            "mac",
        ],
        Some(&ssh),
    );
    assert!(linked.status.success(), "{}", text(&linked));
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    let moved = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved.status.success(), "{}", text(&moved));

    // The laptop has no link to the server; the desk on the Mac does.
    let sent = laptop.ok_over(
        "liaison",
        "captain.send",
        json!({"to": "harbor", "body": "Check the harbor."}),
        &ssh,
    );
    assert_eq!(sent["home"], "fitforge", "{sent}");
    let inbox = messages(&server, "harbor", "notice");
    assert_eq!(inbox.len(), 1, "{inbox:?}");
    assert_eq!(inbox[0]["id"], sent["mail"]["id"]);
    assert_eq!(inbox[0]["from"], "captain");
    assert_eq!(inbox[0]["body"], "Check the harbor.");
}

#[test]
fn the_captain_reads_a_report_stored_on_the_linked_machine() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    let stored = mac.ok(
        "fitforge",
        "report.put",
        json!({"from": "fitforge", "title": "Backup audit", "format": "markdown",
               "content": "# Backup audit\n\nExports omit photos."}),
    );
    let id = stored["report_id"].as_str().unwrap();

    let report = laptop.ok_over("liaison", "report.get", json!({"id": id}), &ssh);
    assert_eq!(report["ship"], "fitforge", "{report}");
    assert_eq!(report["title"], "Backup audit");
    assert!(report["content"].as_str().unwrap().contains("omit photos"));

    let missing = laptop.run(
        "liaison",
        &["report.get", "--json", r#"{"id":"report-nowhere"}"#],
        Some(&ssh),
    );
    assert!(!missing.status.success());
    assert!(
        text(&missing).contains("no report report-nowhere in the fleet"),
        "{}",
        text(&missing)
    );
}

#[test]
fn a_machine_whose_desk_is_elsewhere_hands_its_captain_mail_only_there() {
    let (_temp, laptop, mac, ssh) = fixture();
    let server = Machine {
        root: laptop.root.parent().unwrap().join("server"),
    };
    server.ship("harbor");
    assert!(link(&laptop, &mac, &ssh).status.success());
    // The Mac also reaches the server, and keeps the captain's desk there.
    let registry = server.registry();
    let linked = mac.run(
        "fitforge",
        &[
            "machine",
            "add",
            "server",
            "test-host",
            "--executable",
            support::BIN,
            "--remote-fleet",
            registry.to_str().unwrap(),
            "--as",
            "mac",
        ],
        Some(&ssh),
    );
    assert!(linked.status.success(), "{}", text(&linked));
    assert!(
        mac.run("fitforge", &["machine", "captain", "server"], None)
            .status
            .success()
    );
    mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "notice", "body": "Build 13 is up"}),
    );

    // The laptop, which collects from the Mac, gets none of it...
    assert!(collect(&laptop, &ssh).contains("\"delivered\": 0"));
    assert!(captain_bodies(&laptop, "liaison").is_empty());
    // ...and the Mac hands it to the server.
    let pass = mac.pass("fitforge", &ssh);
    assert_eq!(pass["machines"][0]["captain_pushed"], 1, "{pass}");
    assert_eq!(captain_bodies(&server, "harbor"), ["Build 13 is up"]);
}

/// The captain on the laptop, whose desk is on the Mac, asks a Mac ship.
fn ask_from_the_laptop(laptop: &Machine, ssh: &Path, body: &str) -> Value {
    let output = laptop.run(
        "liaison",
        &[
            "mail.send",
            "--json",
            &json!({"from": "captain", "to": "fitforge", "kind": "request", "body": body})
                .to_string(),
        ],
        Some(ssh),
    );
    assert!(output.status.success(), "{}", text(&output));
    let request: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(request["delivery"], "delivered", "{request}");
    request
}

#[test]
fn a_reply_to_the_captain_goes_back_to_the_machine_that_asked() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    let moved = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved.status.success(), "{}", text(&moved));

    // A front door on the laptop asks a Mac ship from the captain's home here.
    let asked = ask_from_the_laptop(&laptop, &ssh, "Is the backup green?");
    mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "reply",
               "correlation": asked["id"], "body": "Green since noon"}),
    );
    // The desk's own captain mail stays on the Mac: a notice that answers
    // nothing, and the answer to a request the captain asked at the Mac.
    mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "notice", "body": "Build 13 is up"}),
    );
    let local = mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "captain", "to": "fitforge", "kind": "request", "body": "Ship it?"}),
    );
    mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "reply",
               "correlation": local["id"], "body": "Shipped"}),
    );

    // The reply reaches the home that asked, and closes the request there.
    assert!(collect(&laptop, &ssh).contains("\"delivered\": 1"));
    assert_eq!(captain_bodies(&laptop, "liaison"), ["Green since noon"]);
    let request = laptop.ok("liaison", "mail.show", json!({"id": asked["id"]}));
    assert_eq!(request["state"], "answered", "{request}");
    // The desk keeps its copy, beside its own mail.
    assert_eq!(
        captain_bodies(&mac, "fitforge"),
        ["Green since noon", "Build 13 is up", "Shipped"]
    );
    let copy = mac.ok("fitforge", "mail.show", json!({"id": asked["id"]}));
    assert_eq!(copy["state"], "answered", "{copy}");

    // Each reply goes once.
    assert!(collect(&laptop, &ssh).contains("\"delivered\": 0"));
    assert_eq!(captain_bodies(&laptop, "liaison"), ["Green since noon"]);

    // A later note in the same thread follows the reply.
    mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "notice",
               "correlation": asked["id"], "body": "Still green"}),
    );
    assert!(collect(&laptop, &ssh).contains("\"delivered\": 1"));
    assert_eq!(
        captain_bodies(&laptop, "liaison"),
        ["Green since noon", "Still green"]
    );
}

#[test]
fn a_captain_request_from_an_older_machine_keeps_its_replies_where_they_are() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    // An older machine delivers without naming itself: the Mac cannot tell
    // where the request came from, and answers it as it always has.
    let delivered = mac.gateway(
        "mail.deliver",
        json!({"id": "captain-older", "from": "captain", "to": "fitforge",
               "kind": "request", "body": "Rotate the key?"}),
    );
    assert_eq!(delivered["ok"], true, "{delivered}");
    // A newer one names itself.
    let delivered = mac.gateway(
        "mail.deliver",
        json!({"id": "captain-newer", "from": "captain", "to": "fitforge",
               "kind": "request", "body": "Rotate the cert?", "via": "laptop"}),
    );
    assert_eq!(delivered["ok"], true, "{delivered}");
    for request in ["captain-older", "captain-newer"] {
        mac.ok(
            "fitforge",
            "mail.send",
            json!({"from": "fitforge", "to": "captain", "kind": "reply",
                   "correlation": request, "body": format!("Done: {request}")}),
        );
    }
    // A machine whose desk is elsewhere gets only the answer it can place.
    let collected = mac.gateway(
        "fleet.collect",
        json!({"machine": "laptop", "captain": false}),
    );
    assert_eq!(collected_bodies(&collected), ["Done: captain-newer"]);
    // An older collector names no desk, so it is offered all the captain's
    // mail as before, each message once.
    let collected = mac.gateway("fleet.collect", json!({"machine": "laptop"}));
    assert_eq!(
        collected_bodies(&collected),
        ["Done: captain-older", "Done: captain-newer"]
    );
    // What the delivering machine named stays on the Mac, off the wire.
    assert!(
        collected["result"]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m.get("origin_machine").is_none() && m.get("via").is_none()),
        "{collected}"
    );
}

#[test]
fn only_the_answers_to_a_machine_s_own_requests_go_back_to_it() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    let desk = Machine {
        root: laptop.root.with_file_name("desk"),
    };
    desk.ship("drafts");
    assert!(
        link_as(&desk, "drafts", "desk", &mac, &ssh)
            .status
            .success()
    );
    // Each linked machine asks once, and a machine the Mac does not know
    // claims a third.
    for (id, via) in [
        ("captain-laptop", "laptop"),
        ("captain-desk", "desk"),
        ("captain-stranger", "nowhere"),
    ] {
        let delivered = mac.gateway(
            "mail.deliver",
            json!({"id": id, "from": "captain", "to": "fitforge",
                   "kind": "request", "body": format!("Asked from {via}"), "via": via}),
        );
        assert_eq!(delivered["ok"], true, "{delivered}");
        mac.ok(
            "fitforge",
            "mail.send",
            json!({"from": "fitforge", "to": "captain", "kind": "reply",
                   "correlation": id, "body": format!("Done: {id}")}),
        );
    }
    // The ship also asks the captain in the laptop's thread. Those asks are
    // the desk's to answer: a second open copy on the laptop would never close.
    mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "request",
               "correlation": "captain-laptop", "body": "Which backup?"}),
    );
    mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "decision",
        "correlation": "captain-laptop", "body": "Which path?",
        "payload": {"decision": {"question": "Which path?", "options": [
            {"id": "ship", "label": "Ship it", "what_happens": "The release goes out today."},
            {"id": "wait", "label": "Wait", "what_happens": "Nothing ships until review."}
        ]}}}),
    );

    // The unlinked name is not kept, so that request has no origin.
    let origin: Option<String> = ship_database(&mac, "fitforge")
        .query_row(
            "SELECT origin_machine FROM mail WHERE id='captain-stranger'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(origin, None);

    // Each machine is offered the reply to its own request, and nothing else.
    for (machine, reply) in [
        ("laptop", "Done: captain-laptop"),
        ("desk", "Done: captain-desk"),
    ] {
        let collected = mac.gateway(
            "fleet.collect",
            json!({"machine": machine, "captain": false}),
        );
        assert_eq!(collected_bodies(&collected), [reply], "{machine}");
    }
    // A collect under the unlinked name is refused outright.
    let stranger = mac.gateway(
        "fleet.collect",
        json!({"machine": "nowhere", "captain": false}),
    );
    assert_eq!(stranger["ok"], false, "{stranger}");
}

/// The bodies of the mail one `fleet.collect` offered.
fn collected_bodies(collected: &Value) -> Vec<String> {
    assert_eq!(collected["ok"], true, "{collected}");
    collected["result"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["body"].as_str().unwrap().to_owned())
        .collect()
}

fn listed_ships(machine: &Machine, ship: &str) -> Vec<String> {
    machine.ok(ship, "ship.list", json!({}))["ships"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn a_ship_retired_on_the_linked_machine_leaves_the_list_at_the_next_refresh() {
    let (_temp, laptop, mac, _ssh) = fixture();
    let mac_ssh = remote_ssh(&mac);
    mac.ship("server");
    assert!(link(&laptop, &mac, &mac_ssh).status.success());
    assert!(listed_ships(&laptop, "liaison").contains(&"server".to_owned()));

    mac.ok("fitforge", "ship.retire", json!({"name": "server"}));
    let refreshed = laptop.run("liaison", &["machine", "refresh"], Some(&mac_ssh));
    assert!(refreshed.status.success(), "{}", text(&refreshed));

    assert!(!listed_ships(&laptop, "liaison").contains(&"server".to_owned()));
    let gone = laptop.run(
        "liaison",
        &["ship.show", "--json", r#"{"name":"server"}"#],
        None,
    );
    assert!(!gone.status.success(), "{}", text(&gone));
    // The ship that is still there keeps its entry.
    let fitforge = laptop.ok("liaison", "ship.show", json!({"name": "fitforge"}));
    assert_eq!(
        fitforge["location"],
        json!({"peer": "machine-mac"}),
        "{fitforge}"
    );
}

#[test]
fn a_ship_moved_from_the_linked_machine_here_stays_here() {
    let (_temp, laptop, mac, _ssh) = fixture();
    let mac_ssh = remote_ssh(&mac);
    ship_with_project(&mac, &laptop, "scout");
    assert!(link(&laptop, &mac, &mac_ssh).status.success());

    let to_here = laptop.run(
        "liaison",
        &["ship", "move", "scout", "--here"],
        Some(&mac_ssh),
    );
    assert!(to_here.status.success(), "{}", text(&to_here));

    let refreshed = laptop.run("liaison", &["machine", "refresh"], Some(&mac_ssh));
    assert!(refreshed.status.success(), "{}", text(&refreshed));
    // The ship is hosted here: its own home says so, and the list still has it.
    let scout = laptop.ok("scout", "ship.show", json!({"name": "scout"}));
    assert_eq!(scout["location"], "here", "{scout}");
    assert!(listed_ships(&laptop, "liaison").contains(&"scout".to_owned()));
}

#[test]
fn a_ship_behind_another_peer_is_untouched_by_a_refresh() {
    let (_temp, laptop, mac, _ssh) = fixture();
    let mac_ssh = remote_ssh(&mac);
    laptop.ok(
        "liaison",
        "peer.put",
        json!({"name": "far-away", "transport": "ssh", "host": "far-host", "executable": "/usr/bin/privateer", "home": "/srv/far"}),
    );
    laptop.ok(
        "liaison",
        "ship.put",
        json!({"name": "cloudy", "location": {"peer": "far-away"}}),
    );

    mac.ship("server");
    assert!(link(&laptop, &mac, &mac_ssh).status.success());
    mac.ok("fitforge", "ship.retire", json!({"name": "server"}));
    let refreshed = laptop.run("liaison", &["machine", "refresh"], Some(&mac_ssh));
    assert!(refreshed.status.success(), "{}", text(&refreshed));

    let cloudy = laptop.ok("liaison", "ship.show", json!({"name": "cloudy"}));
    assert_eq!(cloudy["location"], json!({"peer": "far-away"}), "{cloudy}");
    assert!(!listed_ships(&laptop, "liaison").contains(&"server".to_owned()));
}

#[test]
fn a_refresh_whose_directory_cannot_be_read_drops_nothing() {
    let (_temp, laptop, mac, _ssh) = fixture();
    let mac_ssh = remote_ssh(&mac);
    mac.ship("server");
    assert!(link(&laptop, &mac, &mac_ssh).status.success());

    // The peer answers with a truncated reply, so its directory is unreadable.
    let truncated = remote_ssh_with(&mac, "truncated-ssh", "| head -c 12");
    let failed = laptop.run("liaison", &["machine", "refresh"], Some(&truncated));
    assert!(!failed.status.success(), "{}", text(&failed));

    let server = laptop.ok("liaison", "ship.show", json!({"name": "server"}));
    assert_eq!(
        server["location"],
        json!({"peer": "machine-mac"}),
        "{server}"
    );
    assert!(listed_ships(&laptop, "liaison").contains(&"server".to_owned()));
}

/// Make a ship's home unreadable by putting a folder where its database is.
fn make_unreadable(machine: &Machine, ship: &str) {
    let database = machine.folder(ship).join(".privateer/privateer.sqlite");
    let _ = fs::remove_file(&database);
    fs::create_dir_all(&database).unwrap();
}

#[test]
fn an_unreadable_home_on_the_linked_machine_drops_no_ship_here() {
    let (_temp, laptop, mac, _ssh) = fixture();
    let mac_ssh = remote_ssh(&mac);
    mac.ship("server");
    assert!(link(&laptop, &mac, &mac_ssh).status.success());
    assert!(listed_ships(&laptop, "liaison").contains(&"server".to_owned()));

    make_unreadable(&mac, "server");
    let refreshed = laptop.run("liaison", &["machine", "refresh"], Some(&mac_ssh));
    assert!(refreshed.status.success(), "{}", text(&refreshed));

    let server = laptop.ok("liaison", "ship.show", json!({"name": "server"}));
    assert_eq!(
        server["location"],
        json!({"peer": "machine-mac"}),
        "{server}"
    );
}

#[test]
fn an_unreadable_home_here_drops_no_ship_on_the_linked_machine() {
    let (_temp, laptop, mac, _ssh) = fixture();
    let mac_ssh = remote_ssh(&mac);
    laptop.ship("scout");
    assert!(link(&laptop, &mac, &mac_ssh).status.success());
    let before = mac.ok("fitforge", "ship.show", json!({"name": "scout"}));
    assert_eq!(before["location"]["peer"], "machine-laptop", "{before}");

    make_unreadable(&laptop, "scout");
    let refreshed = laptop.run("liaison", &["machine", "refresh"], Some(&mac_ssh));
    assert!(refreshed.status.success(), "{}", text(&refreshed));

    let after = mac.ok("fitforge", "ship.show", json!({"name": "scout"}));
    assert_eq!(after["location"], before["location"], "{after}");
}

/// With the desk on the Mac, one laptop home that cannot be opened does not
/// stop the others' captain mail or closures from reaching it; the pass names
/// the home it skipped.
#[test]
fn an_unreadable_home_here_stops_no_other_homes_captain_mail_to_the_desk() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("scout");
    laptop.ship("rig");
    assert!(link(&laptop, &mac, &ssh).status.success());
    let moved = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved.status.success(), "{}", text(&moved));
    make_unreadable(&laptop, "rig");

    let asked = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "scout", "to": "captain", "kind": "request", "body": "Rotate the deploy key?"}),
    );
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["captain_pushed"], 1, "{pass}");
    let skipped = pass["machines"][0]["skipped_homes"].to_string();
    assert!(skipped.contains("/rig/"), "{pass}");
    assert_eq!(captain_requests(&mac, "fitforge", "open").len(), 1);

    laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "captain", "to": "scout", "kind": "reply",
               "correlation": asked["id"], "body": "Yes, rotate it"}),
    );
    collect_now(&laptop);
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["captain_closed"], 1, "{pass}");
    assert!(captain_requests(&mac, "fitforge", "open").is_empty());
}

/// One home on the Mac that cannot be opened does not stop the laptop
/// collecting the other homes' mail, nor the Mac recording it collected.
#[test]
fn an_unreadable_home_on_the_linked_machine_stops_no_other_homes_collection() {
    let (_temp, laptop, mac, ssh) = fixture();
    mac.ship("server");
    assert!(link(&laptop, &mac, &ssh).status.success());
    make_unreadable(&mac, "server");
    mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "notice", "body": "Build 12 is up"}),
    );

    let collected = collect(&laptop, &ssh);
    assert!(collected.contains("\"delivered\": 1"), "{collected}");
    assert_eq!(captain_bodies(&laptop, "liaison"), ["Build 12 is up"]);
    // The Mac recorded it collected: the next collection does not bring it again.
    let again = collect(&laptop, &ssh);
    assert!(again.contains("\"delivered\": 0"), "{again}");
    assert_eq!(captain_bodies(&laptop, "liaison"), ["Build 12 is up"]);
}

#[test]
fn a_directory_without_the_complete_flag_drops_nothing() {
    let (_temp, laptop, mac, _ssh) = fixture();
    let mac_ssh = remote_ssh(&mac);
    mac.ship("server");
    assert!(link(&laptop, &mac, &mac_ssh).status.success());

    // An older peer's reply: the same directory, with no complete flag.
    let older = remote_ssh_with(&mac, "older-ssh", r#"| sed 's/"complete":[a-z]*,//'"#);
    mac.ok("fitforge", "ship.retire", json!({"name": "server"}));
    let refreshed = laptop.run("liaison", &["machine", "refresh"], Some(&older));
    assert!(refreshed.status.success(), "{}", text(&refreshed));

    let server = laptop.ok("liaison", "ship.show", json!({"name": "server"}));
    assert_eq!(
        server["location"],
        json!({"peer": "machine-mac"}),
        "{server}"
    );
}

#[test]
fn mail_closing_requests_the_desk_never_held_reaches_the_captain() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("scout");
    assert!(link(&laptop, &mac, &ssh).status.success());
    // scout asks the captain while the desk is still here, then the desk moves.
    let early = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "scout", "to": "captain", "kind": "request", "body": "Merge PR 7?"}),
    );
    let moved = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved.status.success(), "{}", text(&moved));

    // The captain asks scout from the laptop, not at the desk; scout answers,
    // and withdraws its earlier request. Both requests live only here.
    let asked = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "captain", "to": "scout", "kind": "request", "body": "Rotate the deploy key?"}),
    );
    laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "scout", "to": "captain", "kind": "reply",
               "correlation": asked["id"], "body": "Rotated"}),
    );
    laptop.ok(
        "scout",
        "mail.withdraw",
        json!({"id": early["id"], "reason": "PR 7 merged"}),
    );

    // The desk takes both and puts neither before the captain as a refusal.
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["captain_pushed"], 2, "{pass}");
    let inbox = captain_mail(&mac, "fitforge");
    let kinds: Vec<(&str, &str)> = inbox
        .iter()
        .map(|m| (m["kind"].as_str().unwrap(), m["body"].as_str().unwrap()))
        .collect();
    assert_eq!(kinds, [("reply", "Rotated"), ("notice", "PR 7 merged")]);
    for (id, state) in [(&asked["id"], "answered"), (&early["id"], "withdrawn")] {
        let request = laptop.ok("scout", "mail.show", json!({"id": id}));
        assert_eq!(request["state"], state, "{request}");
    }
}

/// A heavy job holding this machine's one heavy slot, and a second one queued
/// behind it that has waited past the owner's threshold. The holder runs for
/// real under its own supervisor; the queue's age is set in the shared ledger
/// so the test does not wait ten minutes.
fn queued_behind_a_holder(laptop: &Machine) -> (PathBuf, i32) {
    laptop.ok(
        "liaison",
        "resource.configure",
        json!({"max_heavy_jobs":1,"max_load_per_cpu":1024,"min_available_memory_bytes":0,"ramp_secs":0,
            "heavy_estimate":{"memory_bytes":0,"disk_bytes":0}}),
    );
    let home = laptop.folder("upkeep").join(".privateer");
    let repo = laptop.root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    {
        let store = privateer::store::Store::open(&home).unwrap();
        let charter = json!({"id":"mission","objective":"owner alert fixture","repo":repo,
            "active":true,"actions":["implement","test","recovery","cleanup"],"min_free_bytes":0,
            "job_timeout_secs":30,"max_heavy_jobs":1});
        store
            .connection()
            .execute(
                "INSERT INTO charters(id,data) VALUES(?1,?2)",
                params!["mission", charter.to_string()],
            )
            .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('task','mission','upkeep','fixture','open',0)",
                [],
            )
            .unwrap();
    }
    let holder = laptop.ok(
        "upkeep",
        "job.start",
        json!({"task_id":"task","id":"holder","command":["sleep","60"],"heavy":true,"wait_for_capacity":true}),
    );
    // A fast machine may already have it launching by the time start returns.
    assert!(
        matches!(holder["status"].as_str(), Some("starting" | "launching")),
        "{holder}"
    );
    let until = std::time::Instant::now() + std::time::Duration::from_secs(12);
    let supervisor = loop {
        let job = laptop.ok("upkeep", "job.status", json!({"id":"holder"}));
        if let (Some(pid), "running") = (
            job["supervisor"]["pid"].as_i64(),
            job["status"].as_str().unwrap_or_default(),
        ) {
            break pid as i32;
        }
        assert!(
            std::time::Instant::now() < until,
            "supervisor never recorded"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let queued = laptop.ok(
        "upkeep",
        "job.start",
        json!({"task_id":"task","id":"queued","command":["true"],"heavy":true,"wait_for_capacity":true}),
    );
    assert_eq!(queued["status"], "queued", "{queued}");
    // Age the queue entry, once its own supervisor has recorded itself in it.
    let ledger = rusqlite::Connection::open(laptop.root.join("ledger/resources.sqlite")).unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(12);
    while ledger
        .query_row(
            "SELECT COUNT(*) FROM queue WHERE json_extract(record,'$.supervisor.pid') IS NOT NULL",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        == 0
    {
        assert!(
            std::time::Instant::now() < until,
            "queue never recorded its supervisor"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let eleven_minutes_ago = privateer::store::Store::now_ms() - 11 * 60_000;
    ledger
        .execute(
            "UPDATE queue SET record=json_set(record,'$.created_ms',?1)",
            [eleven_minutes_ago],
        )
        .unwrap();
    (home, supervisor)
}

/// Capacity-queue alerts the machine's owner has been sent so far.
fn capacity_alerts(laptop: &Machine, ship: &str) -> Vec<String> {
    messages(laptop, ship, "notice")
        .iter()
        .filter(|m| m["from"] == "crew")
        .filter_map(|m| m["body"].as_str().map(str::to_owned))
        .filter(|body| body.contains("capacity queue"))
        .collect()
}

#[test]
fn a_long_wait_behind_a_healthy_heavy_job_does_not_alert_the_owner() {
    let (_temp, laptop, _mac, ssh) = fixture();
    laptop.ship("upkeep");
    assert!(
        laptop
            .run("liaison", &["machine", "owner", "upkeep"], None)
            .status
            .success()
    );
    let (_home, _supervisor) = queued_behind_a_holder(&laptop);
    laptop.pass("upkeep", &ssh);
    assert!(capacity_alerts(&laptop, "upkeep").is_empty());
    // The waiting ship still sees what it waits behind.
    let bearings = laptop.ok("upkeep", "ship.bearings", json!({"ship":"upkeep"}));
    let text = bearings["text"].as_str().unwrap();
    assert!(
        text.contains("queued is waiting behind upkeep's heavy job holder"),
        "{text}"
    );
    assert!(bearings["capacity_waits"][0]["behind"]["unhealthy"].is_null());
    laptop.ok("upkeep", "job.cancel", json!({"id":"queued"}));
    laptop.ok("upkeep", "job.cancel", json!({"id":"holder"}));
}

#[test]
fn a_long_wait_behind_an_orphaned_heavy_job_alerts_the_owner() {
    let (_temp, laptop, _mac, ssh) = fixture();
    laptop.ship("upkeep");
    assert!(
        laptop
            .run("liaison", &["machine", "owner", "upkeep"], None)
            .status
            .success()
    );
    let (_home, supervisor) = queued_behind_a_holder(&laptop);
    // The supervisor dies under the payload it was watching.
    assert_eq!(unsafe { libc::kill(supervisor, libc::SIGKILL) }, 0);
    laptop.pass("upkeep", &ssh);
    let raised = capacity_alerts(&laptop, "upkeep");
    assert_eq!(raised.len(), 1, "{raised:?}");
    assert!(raised[0].contains("looks unhealthy"), "{raised:?}");
    assert!(raised[0].contains("orphaned"), "{raised:?}");
    let holder = laptop.ok("upkeep", "job.status", json!({"id":"holder"}));
    if let Some(pid) = holder["payload"]["pid"].as_i64() {
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
}

#[test]
fn a_long_wait_with_no_live_holder_alerts_the_owner() {
    let (_temp, laptop, _mac, ssh) = fixture();
    laptop.ship("upkeep");
    assert!(
        laptop
            .run("liaison", &["machine", "owner", "upkeep"], None)
            .status
            .success()
    );
    let (_home, supervisor) = queued_behind_a_holder(&laptop);
    let holder = laptop.ok("upkeep", "job.status", json!({"id":"holder"}));
    let payload = holder["payload"]["pid"].as_i64();
    // Nothing runs for the holder any more, so nothing holds capacity.
    assert_eq!(unsafe { libc::kill(supervisor, libc::SIGKILL) }, 0);
    if let Some(pid) = payload {
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
    laptop.pass("upkeep", &ssh);
    let raised = capacity_alerts(&laptop, "upkeep");
    assert_eq!(raised.len(), 1, "{raised:?}");
    assert!(
        raised[0].contains("nothing live holds capacity"),
        "{raised:?}"
    );
}

#[test]
fn a_typed_decision_answered_from_the_other_machine_is_checked_where_it_lives() {
    let (_temp, laptop, mac, ssh) = fixture();
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    let asked = mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "decision", "body": "Which path?",
        "payload": {"decision": {"question": "Which path?", "options": [
            {"id": "ship", "label": "Ship it", "what_happens": "The release goes out today."},
            {"id": "wait", "label": "Wait", "what_happens": "Nothing ships until review."}
        ]}}}),
    );
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["delivered"], 1, "{pass}");

    // The captain answers from the laptop with an option the decision does not
    // offer. The laptop holds the captain's copy of the decision, so it refuses
    // the answer at once, naming the field; nothing reaches the Mac and the
    // decision stays open there.
    let bad = laptop.run(
        "liaison",
        &[
            "mail.send",
            "--json",
            &json!({"from": "captain", "to": "fitforge", "kind": "reply",
            "correlation": asked["id"], "body": "Neither.",
            "payload": {"answer": {"option_id": "nope"}}})
            .to_string(),
        ],
        Some(&ssh),
    );
    assert!(!bad.status.success(), "{}", text(&bad));
    assert!(
        text(&bad).contains("answer.option_id must be one of the decision's option ids"),
        "{}",
        text(&bad)
    );
    laptop.pass("liaison", &ssh);
    let still_open = mac.ok("fitforge", "mail.show", json!({"id": asked["id"]}));
    assert_eq!(still_open["state"], "open", "{still_open}");
    assert!(
        messages(&mac, "fitforge", "reply").is_empty(),
        "a refused answer is not delivered"
    );

    // A good option id closes the decision on the Mac.
    let good = laptop.run(
        "liaison",
        &[
            "mail.send",
            "--json",
            &json!({"from": "captain", "to": "fitforge", "kind": "reply",
            "correlation": asked["id"], "body": "Wait for review.",
            "payload": {"answer": {"option_id": "wait"}}})
            .to_string(),
        ],
        Some(&ssh),
    );
    assert!(good.status.success(), "{}", text(&good));
    laptop.pass("liaison", &ssh);
    let closed = mac.ok("fitforge", "mail.show", json!({"id": asked["id"]}));
    assert_eq!(closed["state"], "answered", "{closed}");
    let replies = messages(&mac, "fitforge", "reply");
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(
        replies[0]["payload"]["answer"]["option_id"], "wait",
        "{replies:?}"
    );
}

/// A ship made in its own folder with `ssh` as this machine's way to the
/// linked machine, as `privateer init` does on a fleet's machine.
fn init_over(machine: &Machine, ship: &str, ssh: &Path) -> Output {
    fs::create_dir_all(machine.folder(ship)).unwrap();
    machine.run(
        ship,
        &["init", "--ship", ship, "--harness", "codex"],
        Some(ssh),
    )
}

#[test]
fn a_ship_made_here_is_known_on_the_linked_machine_at_once() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());

    // Made after the link, with no refresh: the Mac knows it at once.
    let made = init_over(&laptop, "rig", &ssh);
    assert!(made.status.success(), "{}", text(&made));
    let shown = mac.ok("fitforge", "ship.show", json!({"name": "rig"}));
    assert_eq!(
        shown["location"],
        json!({"peer": "machine-laptop"}),
        "{shown}"
    );
}

#[test]
fn a_ship_retired_here_leaves_the_linked_machine_at_once() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    assert!(init_over(&laptop, "rig", &ssh).status.success());
    assert!(mac.ok("fitforge", "ship.show", json!({"name": "rig"}))["location"].is_object());

    laptop.ok_over("liaison", "ship.retire", json!({"name": "rig"}), &ssh);

    let gone = mac.run(
        "fitforge",
        &["ship.show", "--json", r#"{"name":"rig"}"#],
        None,
    );
    assert!(!gone.status.success(), "{}", text(&gone));
}

#[test]
fn a_ship_made_while_the_linked_machine_is_unreachable_is_still_made() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    let asleep = mac.root.join("asleep-ssh");
    fs::write(&asleep, "#!/bin/sh\nexit 255\n").unwrap();
    fs::set_permissions(&asleep, fs::Permissions::from_mode(0o755)).unwrap();

    let made = init_over(&laptop, "rig", &asleep);
    assert!(made.status.success(), "{}", text(&made));
    // Read from the ship's own home: liaison's home sees rig as a peer.
    let here = laptop.ok("rig", "ship.show", json!({"name": "rig"}));
    assert_eq!(here["location"], json!("here"), "{here}");
    // The skip is audited, and the Mac learns the ship at its next refresh.
    let audit = laptop.ok("rig", "audit.list", json!({"limit": 1000}))["events"].to_string();
    assert!(audit.contains("machine.skipped"), "{audit}");
    let refreshed = laptop.run("liaison", &["machine", "refresh"], Some(&ssh));
    assert!(refreshed.status.success(), "{}", text(&refreshed));
    let on_mac = mac.ok("fitforge", "ship.show", json!({"name": "rig"}));
    assert_eq!(
        on_mac["location"],
        json!({"peer": "machine-laptop"}),
        "{on_mac}"
    );
}

#[test]
fn mail_from_a_ship_the_collecting_machine_has_not_heard_of_is_delivered_at_once() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    // Made on the Mac after the link: the laptop is never told of it there.
    mac.ship("bench");
    mac.ok(
        "bench",
        "mail.send",
        json!({"from": "bench", "to": "liaison", "kind": "request", "body": "Benchmarks ready"}),
    );

    collect_now(&laptop);
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["delivered"], 1, "{pass}");
    assert_eq!(pass["machines"][0]["refused"], 0, "{pass}");
    let got = messages(&laptop, "liaison", "request");
    assert!(
        got.iter()
            .any(|m| m["from"] == "bench" && m["body"] == "Benchmarks ready"),
        "{got:?}"
    );
}

#[test]
fn mail_for_a_ship_no_longer_known_here_is_refused_only_after_it_waits() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    assert!(init_over(&laptop, "rig", &ssh).status.success());
    // Sent while the link is down, so it waits on the Mac for collection.
    let asleep = mac.root.join("asleep-ssh");
    fs::write(&asleep, "#!/bin/sh\nexit 255\n").unwrap();
    fs::set_permissions(&asleep, fs::Permissions::from_mode(0o755)).unwrap();
    let sent = mac.run(
        "fitforge",
        &[
            "mail.send",
            "--json",
            r#"{"from":"fitforge","to":"rig","kind":"request","body":"Ping"}"#,
        ],
        Some(&asleep),
    );
    assert!(sent.status.success(), "{}", text(&sent));
    // Retired here while the directory exchange fails: the Mac keeps its route
    // to rig, and the message names a ship this machine does not know yet.
    // The request arrives on stdin; a directory exchange (fleet.link) fails.
    let link_down = laptop.root.join("link-down-ssh");
    fs::write(
        &link_down,
        "#!/bin/sh\nfor argument do remote=$argument; done\nrequest=\"$(dirname \"$0\")/link-down-request\"\ncat > \"$request\"\ncase \"$(cat \"$request\")\" in *'\"command\":\"fleet.link\"'*) exit 255;; esac\nexec /bin/sh -c \"$remote\" < \"$request\"\n",
    )
    .unwrap();
    fs::set_permissions(&link_down, fs::Permissions::from_mode(0o755)).unwrap();
    laptop.ok_over("liaison", "ship.retire", json!({"name": "rig"}), &link_down);

    for pass in 1..=2 {
        collect_now(&laptop);
        let collected = laptop.pass("liaison", &link_down);
        assert_eq!(
            collected["machines"][0]["refused"], 0,
            "pass {pass}: {collected}"
        );
    }
    collect_now(&laptop);
    let last = laptop.pass("liaison", &link_down);
    assert_eq!(last["machines"][0]["refused"], 1, "{last}");
}

/// An ssh that cannot reach the linked machine, so its calls fail at once.
fn asleep_ssh(machine: &Machine) -> PathBuf {
    let asleep = machine.root.join("asleep-ssh");
    fs::write(&asleep, "#!/bin/sh\nexit 255\n").unwrap();
    fs::set_permissions(&asleep, fs::Permissions::from_mode(0o755)).unwrap();
    asleep
}

/// Marks this machine's directory as refreshed just now, and its contact due,
/// so the next pass collects and pushes without exchanging directories first.
fn refreshed_now(machine: &Machine) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    rusqlite::Connection::open(machine.registry())
        .unwrap()
        .execute(
            "UPDATE machines SET refreshed_ms=?1, next_contact_ms=0",
            params![i64::try_from(now.as_millis()).unwrap()],
        )
        .unwrap();
}

#[test]
fn a_captain_reply_from_a_ship_made_while_the_desk_was_unreachable_is_delivered_after_one_pass() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    let moved = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved.status.success(), "{}", text(&moved));
    // Made while the Mac is asleep: its directory exchange is skipped.
    let asleep = asleep_ssh(&laptop);
    assert!(init_over(&laptop, "rig", &asleep).status.success());
    laptop.ok(
        "rig",
        "mail.send",
        json!({"from": "rig", "to": "captain", "kind": "request", "body": "Ship build 12?"}),
    );

    // The Mac is reachable again. The next pass exchanges directories before
    // it pushes, so the Mac knows rig and takes the reply.
    collect_now(&laptop);
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["captain_pushed"], 1, "{pass}");
    assert_eq!(captain_bodies(&mac, "fitforge"), ["Ship build 12?"]);
}

#[test]
fn a_captain_reply_from_a_ship_the_desk_does_not_know_is_held_then_refused() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    let moved = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved.status.success(), "{}", text(&moved));
    let asleep = asleep_ssh(&laptop);
    assert!(init_over(&laptop, "rig", &asleep).status.success());
    let asked = laptop.ok(
        "rig",
        "mail.send",
        json!({"from": "rig", "to": "captain", "kind": "request", "body": "Ship build 12?"}),
    );

    // The directory here is fresh, so no pass exchanges it: the Mac keeps not
    // knowing rig. The reply is held for the passes it may still be exchanged.
    for pass in 1..=2 {
        refreshed_now(&laptop);
        let held = laptop.pass("liaison", &ssh);
        assert_eq!(
            held["machines"][0]["captain_pushed"], 0,
            "pass {pass}: {held}"
        );
        assert!(captain_bodies(&mac, "fitforge").is_empty());
    }
    refreshed_now(&laptop);
    let last = laptop.pass("liaison", &ssh);
    assert_eq!(last["machines"][0]["captain_pushed"], 1, "{last}");
    let notices: Vec<Value> = captain_mail(&laptop, "liaison")
        .into_iter()
        .filter(|m| m["kind"] == "notice")
        .collect();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["payload"]["refused"]["id"], asked["id"]);
}

/// The captain's requests in a state, as the desk's home lists them.
fn captain_requests(machine: &Machine, ship: &str, state: &str) -> Vec<Value> {
    machine.ok(
        ship,
        "mail.requests",
        json!({"address": "captain", "state": state, "limit": 100}),
    )["requests"]
        .as_array()
        .unwrap()
        .clone()
}

/// A request asked on the laptop with the desk on the Mac, answered on the
/// laptop: the desk's copy closes in one pass, and a repeat pass sends nothing.
#[test]
fn a_captain_request_answered_on_the_linked_machine_closes_the_desk_copy() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("scout");
    assert!(link(&laptop, &mac, &ssh).status.success());
    let moved = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved.status.success(), "{}", text(&moved));
    let asked = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "scout", "to": "captain", "kind": "request", "body": "Rotate the deploy key?"}),
    );
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["captain_pushed"], 1, "{pass}");
    assert_eq!(captain_requests(&mac, "fitforge", "open").len(), 1);

    laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "captain", "to": "scout", "kind": "reply",
               "correlation": asked["id"], "body": "Yes, rotate it"}),
    );
    collect_now(&laptop);
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["captain_closed"], 1, "{pass}");
    assert!(captain_requests(&mac, "fitforge", "open").is_empty());
    let closed = captain_requests(&mac, "fitforge", "answered");
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(closed[0]["id"], asked["id"]);

    // Offered once: a repeated pass sends no closure again.
    collect_now(&laptop);
    let again = laptop.pass("liaison", &ssh);
    assert_eq!(again["machines"][0]["captain_closed"], 0, "{again}");
}

/// A closure's answer is recorded only in a home holding the ask, and only
/// for the ship that ask was addressed from. A linked machine cannot land a
/// captain reply in another ship's desk-local inbox.
#[test]
fn a_closure_reply_addressed_to_another_ship_does_not_reach_its_inbox() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("scout");
    mac.ship("other");
    assert!(link(&laptop, &mac, &ssh).status.success());
    let moved = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved.status.success(), "{}", text(&moved));
    let asked = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "scout", "to": "captain", "kind": "request", "body": "Rotate the deploy key?"}),
    );
    assert_eq!(
        laptop.pass("liaison", &ssh)["machines"][0]["captain_pushed"],
        1
    );
    let replied = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "captain", "to": "scout", "kind": "reply",
               "correlation": asked["id"], "body": "Yes, rotate it"}),
    );
    // Readdress the stored answer to a different desk-local ship, as a forged
    // closure from the linked machine would.
    ship_database(&laptop, "scout")
        .execute(
            "UPDATE mail SET recipient='other' WHERE id=?1",
            params![replied["id"].as_str().unwrap()],
        )
        .unwrap();
    // The desk-local home for that other ship holds a copy of the same ask.
    ship_database(&mac, "other")
        .execute(
            "INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,state,delivery)
             VALUES(?1,'scout','captain','request','Rotate the deploy key?','{}','normal',0,'open','received')",
            params![asked["id"].as_str().unwrap()],
        )
        .unwrap();

    collect_now(&laptop);
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["captain_closed"], 1, "{pass}");
    assert!(captain_requests(&mac, "fitforge", "open").is_empty());

    // The closure closes the copy it names, but its answer was addressed to
    // another ship and is not recorded in that ship's inbox.
    let inbox = mac.ok("other", "mail.inbox", json!({"address": "other"}))["messages"]
        .as_array()
        .unwrap()
        .clone();
    assert!(inbox.iter().all(|m| m["id"] != replied["id"]), "{inbox:?}");
}

/// A captain's answer whose stored envelope is just over the wire limit is
/// left out of the closure rather than refusing it: the desk's copy still
/// closes, instead of waiting forever for a reply that can never be carried.
#[test]
fn a_reply_too_large_for_the_wire_still_closes_the_desk_copy() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("scout");
    assert!(link(&laptop, &mac, &ssh).status.success());
    let moved = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved.status.success(), "{}", text(&moved));
    let asked = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "scout", "to": "captain", "kind": "request", "body": "Rotate the deploy key?"}),
    );
    assert_eq!(
        laptop.pass("liaison", &ssh)["machines"][0]["captain_pushed"],
        1
    );
    // The send arguments fit, but the stored envelope (with its timestamps and
    // defaults) is over the limit and cannot travel in a closure.
    let replied = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "captain", "to": "scout", "kind": "reply",
               "correlation": asked["id"], "body": "x".repeat(65_300)}),
    );

    collect_now(&laptop);
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["captain_closed"], 1, "{pass}");
    assert_eq!(audited(&laptop, "liaison", "machine.closure_refused"), 0);
    assert!(captain_requests(&mac, "fitforge", "open").is_empty());
    let closed = captain_requests(&mac, "fitforge", "answered");
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(closed[0]["id"], asked["id"]);
    // The oversized answer itself was not copied; the copy still closed.
    let copied: i64 = ship_database(&mac, "fitforge")
        .query_row(
            "SELECT COUNT(*) FROM mail WHERE id=?1",
            [replied["id"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(copied, 0);
}

/// A request withdrawn on the laptop leaves the desk's copy with the same
/// end reason, so the desk's waiting drops it.
#[test]
fn a_captain_request_withdrawn_on_the_linked_machine_closes_the_desk_copy() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("scout");
    assert!(link(&laptop, &mac, &ssh).status.success());
    let moved = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved.status.success(), "{}", text(&moved));
    let asked = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "scout", "to": "captain", "kind": "request", "body": "Merge PR 9?"}),
    );
    assert_eq!(
        laptop.pass("liaison", &ssh)["machines"][0]["captain_pushed"],
        1
    );

    laptop.ok(
        "scout",
        "mail.withdraw",
        json!({"id": asked["id"], "reason": "PR 9 closed"}),
    );
    collect_now(&laptop);
    laptop.pass("liaison", &ssh);
    assert!(captain_requests(&mac, "fitforge", "open").is_empty());
    let closed = captain_requests(&mac, "fitforge", "withdrawn");
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(closed[0]["end_reason"], "PR 9 closed");
}

/// An open captain request written before the desk moved to another machine is
/// pushed to the desk on the next pass; an answered request and a notice from
/// before the move stay behind. Running `set_desk` again does not strand an open
/// request, and a second pass sends no duplicates.
#[test]
fn open_captain_requests_written_before_the_desk_moved_reach_the_desk() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("scout");
    assert!(link(&laptop, &mac, &ssh).status.success());

    // 1. An open request written before the desk moves
    let open_req = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "scout", "to": "captain", "kind": "request", "body": "Merge PR 1?"}),
    );
    // 2. An answered request from before the move
    let answered_req = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "scout", "to": "captain", "kind": "request", "body": "Merge PR 2?"}),
    );
    laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "captain", "to": "scout", "kind": "reply",
               "correlation": answered_req["id"], "body": "Already merged"}),
    );
    // 3. A notice from before the move
    let notice = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "scout", "to": "captain", "kind": "notice", "body": "CI passed"}),
    );

    // Move the desk to mac
    let moved = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved.status.success(), "{}", text(&moved));

    // Running set_desk again does not strand the open request
    let moved_again = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved_again.status.success(), "{}", text(&moved_again));

    // Next pass pushes the open request to the desk; answered request and notice are not pushed
    collect_now(&laptop);
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["captain_pushed"], 1, "{pass}");

    // The open request is on the desk
    let desk_open = captain_requests(&mac, "fitforge", "open");
    assert_eq!(desk_open.len(), 1, "{desk_open:?}");
    assert_eq!(desk_open[0]["id"], open_req["id"]);

    // The answered request and notice from before the move are not on the desk
    let desk_all = captain_mail(&mac, "fitforge");
    assert!(
        desk_all
            .iter()
            .all(|m| m["id"] != answered_req["id"] && m["id"] != notice["id"]),
        "{desk_all:?}"
    );

    // No duplicate on a second pass
    collect_now(&laptop);
    let second_pass = laptop.pass("liaison", &ssh);
    assert_eq!(
        second_pass["machines"][0]["captain_pushed"], 0,
        "{second_pass}"
    );
    let desk_open_again = captain_requests(&mac, "fitforge", "open");
    assert_eq!(desk_open_again.len(), 1, "{desk_open_again:?}");
}

/// An answer given at the desk closes the laptop's original, and that closure
/// goes back to the desk once: the desk's copy stays as it was answered there.
#[test]
fn a_captain_answer_given_at_the_desk_closes_the_original_without_looping() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("scout");
    assert!(link(&laptop, &mac, &ssh).status.success());
    let moved = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved.status.success(), "{}", text(&moved));
    let asked = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "scout", "to": "captain", "kind": "request", "body": "Ship build 12?"}),
    );
    assert_eq!(
        laptop.pass("liaison", &ssh)["machines"][0]["captain_pushed"],
        1
    );

    let answered = mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "captain", "to": "scout", "kind": "reply",
               "correlation": asked["id"], "body": "Ship it"}),
    );
    assert!(collect(&laptop, &ssh).contains("\"delivered\": 1"));
    let request = laptop.ok("scout", "mail.show", json!({"id": asked["id"]}));
    assert_eq!(request["state"], "answered", "{request}");

    collect_now(&laptop);
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["captain_closed"], 1, "{pass}");
    let closed = captain_requests(&mac, "fitforge", "answered");
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(closed[0]["answered_by"], answered["id"]);
    assert!(captain_requests(&mac, "fitforge", "open").is_empty());
    collect_now(&laptop);
    let again = laptop.pass("liaison", &ssh);
    assert_eq!(again["machines"][0]["captain_closed"], 0, "{again}");
}

/// A typed approval granted or denied on the linked machine closes the desk's
/// copy of its request, as the answer it is: the desk's waiting drops both.
#[test]
fn an_approval_outcome_on_the_linked_machine_closes_the_desk_copy() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("scout");
    assert!(link(&laptop, &mac, &ssh).status.success());
    let moved = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved.status.success(), "{}", text(&moved));
    laptop.ok(
        "scout",
        "task.create",
        json!({"id": "release", "charter_id": "scout", "owner": "scout", "title": "Release"}),
    );
    let granted = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "scout", "to": "captain", "kind": "request", "request_kind": "production_approval",
               "task_id": "release", "body": "Release build A?",
               "payload": {"artifact": "sha256:granted", "environment": "production"}}),
    );
    let denied = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "scout", "to": "captain", "kind": "request", "request_kind": "production_approval",
               "task_id": "release", "body": "Release build B?",
               "payload": {"artifact": "sha256:denied", "environment": "production"}}),
    );
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["captain_pushed"], 2, "{pass}");
    assert_eq!(captain_requests(&mac, "fitforge", "open").len(), 2);

    for (request, artifact, outcome) in [
        (&granted, "sha256:granted", "approval.grant"),
        (&denied, "sha256:denied", "approval.deny"),
    ] {
        let version = laptop.ok("scout", "mail.show", json!({"id": request["id"]}))["payload"]
            ["charter_hash"]
            .clone();
        laptop.ok(
            "scout",
            outcome,
            json!({"id": format!("grant-{artifact}"), "task_id": "release", "artifact": artifact,
                   "environment": "production", "approver": "captain",
                   "request_id": request["id"], "expected_version": version}),
        );
    }
    assert_eq!(captain_requests(&mac, "fitforge", "open").len(), 2);

    collect_now(&laptop);
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["captain_closed"], 2, "{pass}");
    assert!(captain_requests(&mac, "fitforge", "open").is_empty());
    let approved = captain_requests(&mac, "fitforge", "approved");
    assert_eq!(approved.len(), 1, "{approved:?}");
    assert_eq!(approved[0]["id"], granted["id"]);
    assert_eq!(approved[0]["end_reason"], "approved");
    let refused = captain_requests(&mac, "fitforge", "denied");
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0]["id"], denied["id"]);
    assert_eq!(refused[0]["end_reason"], "denied");
}

/// Make a ship's home refuse mail updates while still opening and reading, as
/// a home whose database is busy does. `accept_mail_updates` lifts it. (A home
/// that cannot be opened stops the desk's collection before closures, so it
/// never reaches the closure path.)
fn refuse_mail_updates(machine: &Machine, ship: &str) {
    rusqlite::Connection::open(machine.folder(ship).join(".privateer/privateer.sqlite"))
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER refuse_mail_updates BEFORE UPDATE ON mail
             BEGIN SELECT RAISE(ABORT, 'database is locked'); END;",
        )
        .unwrap();
}

fn accept_mail_updates(machine: &Machine, ship: &str) {
    rusqlite::Connection::open(machine.folder(ship).join(".privateer/privateer.sqlite"))
        .unwrap()
        .execute_batch("DROP TRIGGER refuse_mail_updates;")
        .unwrap();
}

/// The audit events of a kind in a ship's home here.
fn audited(laptop: &Machine, ship: &str, kind: &str) -> usize {
    laptop.ok(ship, "audit.list", json!({"limit": 1000}))["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == kind)
        .count()
}

/// A desk home that cannot take a closure right now is a transient failure,
/// not a refusal: the closure is offered again, and the copy closes once the
/// home can take it.
#[test]
fn a_closure_the_desk_cannot_write_yet_is_offered_again() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("scout");
    assert!(link(&laptop, &mac, &ssh).status.success());
    let moved = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved.status.success(), "{}", text(&moved));
    let asked = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "scout", "to": "captain", "kind": "request", "body": "Rotate the deploy key?"}),
    );
    assert_eq!(
        laptop.pass("liaison", &ssh)["machines"][0]["captain_pushed"],
        1
    );

    refuse_mail_updates(&mac, "fitforge");
    laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "captain", "to": "scout", "kind": "reply",
               "correlation": asked["id"], "body": "Yes, rotate it"}),
    );
    collect_now(&laptop);
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["captain_closed"], 0, "{pass}");
    assert_eq!(audited(&laptop, "liaison", "machine.closure_refused"), 0);

    accept_mail_updates(&mac, "fitforge");
    collect_now(&laptop);
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["captain_closed"], 1, "{pass}");
    assert!(captain_requests(&mac, "fitforge", "open").is_empty());
    let closed = captain_requests(&mac, "fitforge", "answered");
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(closed[0]["id"], asked["id"]);
}

/// A desk that predates closures answers without a closed list. Its closures
/// go once, then not again until the link refreshes, with one audit of it.
#[test]
fn a_desk_predating_closures_is_sent_none_until_its_next_refresh() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("scout");
    assert!(link(&laptop, &mac, &ssh).status.success());
    let moved = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved.status.success(), "{}", text(&moved));
    let asked = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "scout", "to": "captain", "kind": "request", "body": "Rotate the deploy key?"}),
    );
    assert_eq!(
        laptop.pass("liaison", &ssh)["machines"][0]["captain_pushed"],
        1
    );

    // The older desk's reply, with its closed list removed.
    let older = remote_ssh_with(&mac, "older-ssh", r#"| sed 's/"closed":\[[^]]*\],//'"#);
    laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "captain", "to": "scout", "kind": "reply",
               "correlation": asked["id"], "body": "Yes, rotate it"}),
    );
    collect_now(&laptop);
    let pass = laptop.pass("liaison", &older);
    assert_eq!(pass["machines"][0]["captain_closed"], 0, "{pass}");
    assert_eq!(captain_requests(&mac, "fitforge", "answered").len(), 1);
    assert_eq!(
        audited(&laptop, "liaison", "machine.closures_unsupported"),
        1
    );

    collect_now(&laptop);
    let again = laptop.pass("liaison", &older);
    assert_eq!(again["machines"][0]["captain_closed"], 0, "{again}");
    assert_eq!(
        audited(&laptop, "liaison", "machine.closures_unsupported"),
        1
    );
}

/// A closure the desk can never apply is refused once, reported, and not sent
/// again. The stored reason is set past the limit directly, as no path writes one.
#[test]
fn a_malformed_closure_is_refused_once_and_not_sent_again() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("scout");
    assert!(link(&laptop, &mac, &ssh).status.success());
    let moved = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved.status.success(), "{}", text(&moved));
    let asked = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "scout", "to": "captain", "kind": "request", "body": "Merge PR 9?"}),
    );
    assert_eq!(
        laptop.pass("liaison", &ssh)["machines"][0]["captain_pushed"],
        1
    );
    laptop.ok(
        "scout",
        "mail.withdraw",
        json!({"id": asked["id"], "reason": "PR 9 closed"}),
    );
    rusqlite::Connection::open(laptop.folder("scout").join(".privateer/privateer.sqlite"))
        .unwrap()
        .execute(
            "UPDATE mail SET end_reason=?2 WHERE id=?1",
            params![asked["id"].as_str().unwrap(), "x".repeat(2001)],
        )
        .unwrap();

    collect_now(&laptop);
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["captain_closed"], 0, "{pass}");
    assert_eq!(audited(&laptop, "liaison", "machine.closure_refused"), 1);

    collect_now(&laptop);
    let again = laptop.pass("liaison", &ssh);
    assert_eq!(again["machines"][0]["captain_closed"], 0, "{again}");
    assert_eq!(audited(&laptop, "liaison", "machine.closure_refused"), 1);
}

#[test]
fn a_relay_on_the_other_machine_joins_the_ask_at_the_desk_and_only_the_desks_allow_counts() {
    let (_temp, laptop, mac, ssh) = fixture();
    mac.ship("scout");
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    let rule = "Bash(apify run *)";
    let policy = |machine: &Machine| {
        fs::read_to_string(machine.root.join("fleet.permissions.json")).unwrap_or_default()
    };
    // A hosted first mate on the mac reports its own denial.
    let blocked = || {
        mac.ok(
            "fitforge",
            "permissions.blocked",
            json!({"task_id": "fitforge-duty", "tool": "Bash",
                "input": {"command": "apify run actor/x"}}),
        )["request_id"]
            .clone()
    };
    let second = blocked();
    // A captain reply that no desk delivered loses its operator marker, so
    // its Allow is refused for the deliverer to put before the captain: it
    // neither adds the rule nor closes the ask.
    let forged = mac.gateway(
        "mail.deliver",
        json!({"id": "forged", "from": "captain", "to": "fitforge", "kind": "reply",
            "correlation": second, "body": "Allow", "via": "elsewhere",
            "payload": {"answer": {"option_id": "allow"}, "operator": true}}),
    );
    assert_eq!(forged["ok"], false, "{forged}");
    let refusal = forged["error"].as_str().unwrap();
    assert!(refusal.contains("stays open"), "{refusal}");
    assert!(
        refusal.contains("Allow must come from the captain"),
        "{refusal}"
    );
    assert!(messages(&mac, "fitforge", "reply").is_empty());
    assert_eq!(
        mac.ok("fitforge", "mail.show", json!({"id": second}))["state"],
        "open"
    );
    assert!(!policy(&mac).contains(rule));
    // Another ship on the mac relays it, citing the ask.
    let relay = mac.ok(
        "scout",
        "mail.send",
        json!({"from": "scout", "to": "captain", "kind": "request", "about": second,
            "body": "fitforge is blocked on apify; allow it?"}),
    );
    assert_eq!(relay["attached_to"], second, "{relay}");
    let pass = laptop.pass("liaison", &ssh);
    assert!(
        pass["machines"][0]["delivered"].as_u64() >= Some(2),
        "{pass}"
    );
    // At the desk the relay is attached to the ask's copy: one item waits.
    let copy = laptop.ok("liaison", "mail.show", json!({"id": relay["id"]}));
    assert_eq!(copy["payload"]["attached_to"], second, "{copy}");
    let waiting = laptop.ok("liaison", "captain.waiting", json!({}));
    let items = waiting["waiting"].as_array().unwrap();
    assert!(
        !items.iter().any(|w| w["mail"]["id"] == relay["id"]),
        "{waiting}"
    );
    let ask = items.iter().find(|w| w["mail"]["id"] == second).unwrap();
    assert_eq!(ask["attached"], 1, "{waiting}");

    // The captain's Allow at the desk reaches the mac, which adds the rule
    // where the ask lives; the relay gets the same answer.
    let answered = captain_sends(
        &laptop,
        "liaison",
        &ssh,
        json!({"to": "fitforge", "kind": "reply", "correlation": second, "body": "Allow",
            "payload": {"answer": {"option_id": "allow"}}}),
    );
    assert!(answered.status.success(), "{}", text(&answered));
    assert_eq!(
        mac.ok("fitforge", "mail.show", json!({"id": second}))["state"],
        "answered"
    );
    assert!(policy(&mac).contains(rule), "{}", policy(&mac));
    assert!(!policy(&laptop).contains(rule));
    let relayed = mac.ok("scout", "mail.show", json!({"id": relay["id"]}));
    assert_eq!(relayed["state"], "answered", "{relayed}");
}

#[test]
fn the_captains_allow_to_an_ask_the_desk_never_held_is_applied_where_it_lives() {
    let (_temp, laptop, mac, ssh) = fixture();
    let rule = "Bash(apify run *)";
    // Asked before the link, so the desk holds no copy of it.
    let asked = mac.ok(
        "fitforge",
        "permissions.blocked",
        json!({"task_id": "fitforge-duty", "tool": "Bash",
            "input": {"command": "apify run actor/x"}}),
    )["request_id"]
        .clone();
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));

    // The captain's own Allow over the gateway link carries the marker, which
    // the mac keeps because the laptop holds its desk.
    let answered = captain_sends(
        &laptop,
        "liaison",
        &ssh,
        json!({"to": "fitforge", "kind": "reply", "correlation": asked, "body": "Allow",
            "payload": {"answer": {"option_id": "allow"}}}),
    );
    assert!(answered.status.success(), "{}", text(&answered));
    assert_eq!(
        mac.ok("fitforge", "mail.show", json!({"id": asked}))["state"],
        "answered"
    );
    let policy = fs::read_to_string(mac.root.join("fleet.permissions.json")).unwrap_or_default();
    assert!(policy.contains(rule), "{policy}");
    assert!(captain_notices(&laptop, "liaison").is_empty());
}

#[test]
fn a_desk_copy_of_an_ask_no_rule_narrows_stays_its_own_card() {
    let (_temp, laptop, mac, ssh) = fixture();
    mac.ship("scout");
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    let blocked = |ship: &str, task: &str, tool: &str, input: Value| {
        if !task.ends_with("-duty") {
            mac.ok(
                ship,
                "task.create",
                json!({"id": task, "charter_id": ship, "owner": ship, "title": "Other work"}),
            );
        }
        mac.ok(
            ship,
            "permissions.blocked",
            json!({"task_id": task, "tool": tool, "input": input}),
        )["request_id"]
            .clone()
    };
    // Two ships report a tool only as unknown: the same fleet key, no rule.
    let first = blocked("fitforge", "fitforge-duty", "unknown", json!({}));
    let second = blocked("scout", "scout-duty", "unknown", json!({}));
    // Attached at the origin anyway, as an older runtime attached it.
    rusqlite::Connection::open(mac.folder("scout").join(".privateer/privateer.sqlite"))
        .unwrap()
        .execute(
            "UPDATE mail SET payload=json_set(payload,'$.attached_to',?2,'$.attached_ship','fitforge','$.about',?2)
             WHERE id=?1",
            params![second.as_str().unwrap(), first.as_str().unwrap()],
        )
        .unwrap();
    // The same derived rule on both ships is one ask, at the origin and at the desk.
    let rule = json!({"command": "kubectl apply -f web.yaml"});
    let shared = blocked("fitforge", "fitforge-k8s", "Bash", rule.clone());
    let joined = blocked("scout", "scout-k8s", "Bash", rule);
    assert_eq!(
        mac.ok("scout", "mail.show", json!({"id": joined}))["payload"]["attached_to"],
        shared
    );
    let pass = laptop.pass("liaison", &ssh);
    assert!(
        pass["machines"][0]["delivered"].as_u64() >= Some(4),
        "{pass}"
    );
    let copy = laptop.ok("liaison", "mail.show", json!({"id": second}));
    assert_eq!(copy["delivery"], "received", "{copy}");
    assert!(copy["payload"]["attached_to"].is_null(), "{copy}");
    assert!(copy["payload"]["about"].is_null(), "{copy}");
    let joined_copy = laptop.ok("liaison", "mail.show", json!({"id": joined}));
    assert_eq!(
        joined_copy["payload"]["attached_to"], shared,
        "{joined_copy}"
    );
    let waiting = laptop.ok("liaison", "captain.waiting", json!({}));
    let items = waiting["waiting"].as_array().unwrap();
    for id in [&first, &second, &shared] {
        assert!(items.iter().any(|w| w["mail"]["id"] == *id), "{waiting}");
    }
    assert!(
        !items.iter().any(|w| w["mail"]["id"] == joined),
        "{waiting}"
    );
}

#[test]
fn the_captain_replies_to_and_dismisses_an_ask_from_the_other_machine_by_prefix() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    let ask = |body: &str| {
        mac.ok(
            "fitforge",
            "mail.send",
            json!({"from": "fitforge", "to": "captain", "kind": "request", "body": body}),
        )["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let answered = ask("Upload build 12?");
    let dismissed = ask("Upload build 11?");
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["delivered"], 2, "{pass}");

    // The captain on the laptop answers from any folder, by prefix; the
    // answer crosses the link and closes the request where it was asked.
    let replied = laptop.ok_over(
        "liaison",
        "captain.reply",
        json!({"id": &answered[..8], "body": "Yes, ship it"}),
        &ssh,
    );
    assert_eq!(replied["id"], answered, "{replied}");
    let there = mac.ok("fitforge", "mail.show", json!({"id": answered}));
    assert_eq!(there["state"], "answered", "{there}");

    // A dismissal crosses the same way: dismissed there, with one notice.
    let closed = laptop.ok_over(
        "liaison",
        "captain.dismiss",
        json!({"id": &dismissed[..8], "note": "build 11 is superseded"}),
        &ssh,
    );
    assert_eq!(closed["state"], "dismissed", "{closed}");
    let there = mac.ok("fitforge", "mail.show", json!({"id": dismissed}));
    assert_eq!(there["state"], "dismissed", "{there}");
    assert_eq!(there["end_reason"], "build 11 is superseded");
    let told: Vec<Value> = messages(&mac, "fitforge", "notice")
        .into_iter()
        .filter(|m| m["correlation"] == dismissed.as_str())
        .collect();
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0]["from"], "captain");
    let waiting = laptop.ok("liaison", "captain.waiting", json!({}));
    assert_eq!(waiting["waiting"], json!([]), "{waiting}");
}

#[test]
fn a_dismissal_closes_the_ask_on_the_desk_and_where_it_was_asked() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("scout");
    assert!(link(&laptop, &mac, &ssh).status.success());
    let moved = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved.status.success(), "{}", text(&moved));
    let ask = |body: &str| {
        laptop.ok(
            "scout",
            "mail.send",
            json!({"from": "scout", "to": "captain", "kind": "request", "body": body}),
        )["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let at_desk = ask("Rotate the deploy key?");
    let at_home = ask("Rotate the signing key?");
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["captain_pushed"], 2, "{pass}");

    // Dismissed at the desk: the next collection closes it where it was asked.
    let closed = mac.ok(
        "fitforge",
        "captain.dismiss",
        json!({"id": &at_desk[..8], "note": "rotated already"}),
    );
    assert_eq!(closed["state"], "dismissed", "{closed}");
    collect(&laptop, &ssh);
    let request = laptop.ok("scout", "mail.show", json!({"id": at_desk}));
    assert_eq!(request["state"], "dismissed", "{request}");
    assert_eq!(request["end_reason"], "rotated already");

    // Dismissed where it was asked: the desk's copy closes on the next pass.
    laptop.ok(
        "scout",
        "captain.dismiss",
        json!({"id": at_home, "note": "not needed"}),
    );
    collect_now(&laptop);
    let pass = laptop.pass("liaison", &ssh);
    // The ask dismissed at the desk is offered back too; its copy is already closed.
    assert_eq!(pass["machines"][0]["captain_closed"], 2, "{pass}");
    let copy = mac.ok("fitforge", "mail.show", json!({"id": at_home}));
    assert_eq!(copy["state"], "dismissed", "{copy}");
    assert_eq!(copy["end_reason"], "not needed");
    let waiting = mac.ok("fitforge", "captain.waiting", json!({}));
    assert_eq!(waiting["waiting"], json!([]), "{waiting}");
}

#[test]
fn the_other_machines_captain_mail_reaches_the_feed_through_the_desk() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    let asked = mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "request", "body": "Upload build 12?"}),
    );
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["delivered"], 1, "{pass}");

    // The desk copy is one entry in the desk home, named by its asking ship.
    let feed = laptop.ok("liaison", "captain.feed", json!({}));
    let entries: Vec<&Value> = feed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["id"] == asked["id"])
        .collect();
    assert_eq!(entries.len(), 1, "{feed}");
    assert_eq!(entries[0]["ship"], "fitforge");
    assert_eq!(entries[0]["home"], "liaison");
    assert_eq!(entries[0]["body"], "Upload build 12?");
    assert_eq!(entries[0]["state"], "open");

    // Answered from the desk: the feed shows the reply and the closed ask.
    laptop.ok_over(
        "liaison",
        "captain.reply",
        json!({"id": asked["id"], "body": "Yes"}),
        &ssh,
    );
    let feed = laptop.ok("liaison", "captain.feed", json!({"ship": "fitforge"}));
    let entries = feed["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "{feed}");
    assert_eq!(entries[0]["state"], "answered");
    assert_eq!(entries[1]["from"], "captain");
    assert_eq!(entries[1]["correlation"], asked["id"]);
}

#[test]
fn the_ship_profile_of_a_ship_on_another_machine_is_asked_there() {
    let (_temp, laptop, mac, ssh) = fixture();
    assert!(link(&laptop, &mac, &ssh).status.success());
    let own = laptop.ok("liaison", "ship.profile", json!({"ship": "liaison"}));
    assert_eq!(own["machine"], "laptop", "{own}");
    // No gateway call: the profile is read on the machine holding the ship.
    let elsewhere = laptop.run(
        "liaison",
        &["ship.profile", "--json", r#"{"ship":"fitforge"}"#],
        Some(&ssh),
    );
    assert!(!elsewhere.status.success());
    assert!(
        text(&elsewhere).contains("ship fitforge is on machine mac; ask there"),
        "{}",
        text(&elsewhere)
    );
    let there = mac.ok("fitforge", "ship.profile", json!({"ship": "fitforge"}));
    assert_eq!(there["first_mate"]["harness"], "codex", "{there}");
}

/// A reply given where the ask was asked stays in the ship's own home; the
/// closure that answers the desk's copy carries the reply too, so the
/// captain's feed at the desk lists what the captain said, once, beside the
/// ask it answers.
#[test]
fn a_reply_given_on_the_linked_machine_reaches_the_desks_feed() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("scout");
    assert!(link(&laptop, &mac, &ssh).status.success());
    let moved = laptop.run("liaison", &["machine", "captain", "mac"], None);
    assert!(moved.status.success(), "{}", text(&moved));
    let asked = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "scout", "to": "captain", "kind": "request", "body": "Rotate the key?"}),
    );
    assert_eq!(
        laptop.pass("liaison", &ssh)["machines"][0]["captain_pushed"],
        1
    );

    // Answered where it was asked: the reply is recorded in scout's home only.
    let replied = laptop.ok(
        "scout",
        "mail.send",
        json!({"from": "captain", "to": "scout", "kind": "reply",
               "correlation": asked["id"], "body": "Yes, rotate it"}),
    );
    collect_now(&laptop);
    let pass = laptop.pass("liaison", &ssh);
    assert_eq!(pass["machines"][0]["captain_closed"], 1, "{pass}");

    // The desk's feed shows the reply once, correlated to the ask.
    let feed = laptop.ok_over("liaison", "captain.feed", json!({"ship": "scout"}), &ssh);
    let entries = feed["entries"].as_array().unwrap();
    let replies: Vec<&Value> = entries
        .iter()
        .filter(|e| e["id"] == replied["id"])
        .collect();
    assert_eq!(replies.len(), 1, "{feed}");
    assert_eq!(replies[0]["kind"], "reply", "{feed}");
    assert_eq!(replies[0]["correlation"], asked["id"], "{feed}");
    assert_eq!(replies[0]["body"], "Yes, rotate it", "{feed}");
    assert_eq!(replies[0]["home"], "fitforge", "{feed}");
    let asks: Vec<&Value> = entries.iter().filter(|e| e["id"] == asked["id"]).collect();
    assert_eq!(asks.len(), 1, "{feed}");
    assert_eq!(asks[0]["state"], "answered", "{feed}");
    assert_eq!(asks[0]["answered_by"], replied["id"], "{feed}");

    // Offered once: a repeated pass sends no closure or reply again.
    collect_now(&laptop);
    let again = laptop.pass("liaison", &ssh);
    assert_eq!(again["machines"][0]["captain_closed"], 0, "{again}");
    let feed = laptop.ok_over("liaison", "captain.feed", json!({"ship": "scout"}), &ssh);
    assert_eq!(
        feed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["id"] == replied["id"])
            .count(),
        1,
        "{feed}"
    );
}

fn machine_row<'a>(snapshot: &'a Value, name: &str) -> &'a Value {
    snapshot["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == name)
        .unwrap_or_else(|| panic!("{name} missing: {snapshot}"))
}

fn ships_on<'a>(snapshot: &'a Value, machine: &str) -> Vec<&'a Value> {
    snapshot["ships"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["machine"] == machine)
        .collect()
}

#[test]
fn the_snapshot_reads_a_reached_machine_and_keeps_its_last_section_when_it_is_down() {
    let (_temp, laptop, mac, ssh) = fixture();
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    let asked = mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "captain", "kind": "request", "body": "Ship build 9?"}),
    );
    let remote = remote_ssh(&mac);

    let first = laptop.ok_over("liaison", "fleet.snapshot", json!({}), &remote);
    let row = machine_row(&first, "mac");
    assert_eq!(row["link"], "reaches", "{first}");
    assert_eq!(row["reachable"], true, "{first}");
    assert_eq!(row["via"], "direct");
    assert!(
        row["routes"].is_array(),
        "the reached machine's routes: {first}"
    );
    let fitforge = ships_on(&first, "mac");
    assert_eq!(fitforge.len(), 1, "{first}");
    assert_eq!(fitforge[0]["name"], "fitforge");
    assert_eq!(fitforge[0]["presence"], "absent");
    let need = first["needs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["mail_id"] == asked["id"])
        .unwrap_or_else(|| panic!("request missing: {first}"));
    assert_eq!(need["machine"], "mac");
    assert_eq!(need["actions"], json!(["mail.send"]));
    assert_eq!(connections(&mac), 1);

    // Asked again at once, the read is reused: no new connection.
    let again = laptop.ok_over("liaison", "fleet.snapshot", json!({}), &remote);
    assert_eq!(machine_row(&again, "mac")["via"], "reused", "{again}");
    assert_eq!(
        machine_row(&again, "mac")["as_of_ms"],
        row["as_of_ms"],
        "{again}"
    );
    assert_eq!(connections(&mac), 1);

    // The mac goes down: its last section stays, dated, and cannot be acted on.
    std::thread::sleep(std::time::Duration::from_millis(5_100));
    let down = remote_ssh_with(&mac, "down-ssh", "| (cat >/dev/null; echo garbage)");
    let cached = laptop.ok_over("liaison", "fleet.snapshot", json!({}), &down);
    let row_down = machine_row(&cached, "mac");
    assert_eq!(row_down["reachable"], false, "{cached}");
    assert_eq!(row_down["via"], "cache");
    assert!(row_down["error"].as_str().is_some(), "{cached}");
    assert_eq!(row_down["as_of_ms"], row["as_of_ms"]);
    assert_eq!(row_down["changed_ms"], row["changed_ms"]);
    assert_eq!(ships_on(&cached, "mac").len(), 1, "{cached}");
    let stale = cached["needs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["mail_id"] == asked["id"])
        .unwrap_or_else(|| panic!("cached request missing: {cached}"));
    assert_eq!(stale["as_of_ms"], row["as_of_ms"]);
    assert_eq!(stale["actions"], json!([]));

    // A since naming the mac's changed_ms skips its ships and routes, never
    // its needs: what waits on the captain is always returned in full.
    let since = json!({"since": {"mac": row["changed_ms"]}});
    let quiet = laptop.ok_over("liaison", "fleet.snapshot", since.clone(), &down);
    let row_quiet = machine_row(&quiet, "mac");
    assert_eq!(row_quiet["unchanged"], true, "{quiet}");
    assert!(row_quiet.get("routes").is_none(), "{quiet}");
    assert!(ships_on(&quiet, "mac").is_empty(), "{quiet}");
    assert!(
        quiet["needs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["mail_id"] == asked["id"] && n["actions"] == json!([])),
        "{quiet}"
    );

    // Once the captain answers the desk copy here, the cached need is gone.
    laptop.pass("liaison", &remote);
    laptop.ok(
        "liaison",
        "mail.send",
        json!({"from": "captain", "to": "fitforge", "kind": "reply",
            "correlation": asked["id"], "body": "Yes."}),
    );
    let answered = laptop.ok_over("liaison", "fleet.snapshot", json!({}), &down);
    assert!(
        !answered["needs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["mail_id"] == asked["id"]),
        "{answered}"
    );
    // The mac's changed_ms has not moved, yet with since the need is gone too.
    let answered_since = laptop.ok_over("liaison", "fleet.snapshot", since, &down);
    assert_eq!(
        machine_row(&answered_since, "mac")["unchanged"],
        true,
        "{answered_since}"
    );
    assert!(
        !answered_since["needs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["mail_id"] == asked["id"]),
        "{answered_since}"
    );

    // A home here that cannot be read for answers is named, not taken as
    // an all-clear; the home that holds the answer still hides the need.
    laptop.ship("spare");
    let locker =
        rusqlite::Connection::open(laptop.folder("spare").join(".privateer/privateer.sqlite"))
            .unwrap();
    locker
        .execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
        .unwrap();
    let locked = laptop.ok_over("liaison", "fleet.snapshot", json!({}), &down);
    locker.execute_batch("ROLLBACK;").unwrap();
    drop(locker);
    let unknown = machine_row(&locked, "laptop")["unknown"]
        .as_array()
        .unwrap();
    assert!(unknown.contains(&json!("answered:spare")), "{locked}");
    assert!(!unknown.contains(&json!("answered:liaison")), "{locked}");
    assert!(
        !locked["needs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["mail_id"] == asked["id"]),
        "{locked}"
    );

    // Removing the link forgets the cached section.
    laptop.ok("liaison", "machine.remove", json!({"name": "mac"}));
    let removed = laptop.ok("liaison", "fleet.snapshot", json!({}));
    assert_eq!(
        removed["machines"].as_array().unwrap().len(),
        1,
        "{removed}"
    );
}

#[test]
fn a_reaching_machine_carries_its_section_on_collect_to_the_machine_it_reaches() {
    let (_temp, laptop, mac, ssh) = fixture();
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));

    // Before any collect the mac knows the laptop only from the directory,
    // and never connects to it.
    let before = mac.ok("fitforge", "fleet.snapshot", json!({}));
    let row = machine_row(&before, "laptop");
    assert_eq!(row["link"], "reached_by", "{before}");
    assert_eq!(row["reachable"], false);
    assert_eq!(row["via"], "directory");
    let listed = ships_on(&before, "laptop");
    assert_eq!(listed.len(), 1, "{before}");
    assert_eq!(listed[0]["presence"], "unknown");

    // The laptop's collect carries its own section.
    collect_as(&laptop, "liaison", &remote_ssh(&mac));
    let after = mac.ok("fitforge", "fleet.snapshot", json!({}));
    let row = machine_row(&after, "laptop");
    assert_eq!(row["via"], "collect", "{after}");
    assert_eq!(row["reachable"], true, "{after}");
    let liaison = ships_on(&after, "laptop");
    assert_eq!(liaison.len(), 1, "{after}");
    assert_eq!(liaison[0]["name"], "liaison");
    assert_eq!(liaison[0]["presence"], "absent", "{after}");
}

#[test]
fn a_carried_section_the_reached_machine_cannot_keep_never_stops_the_mail() {
    let (_temp, laptop, mac, ssh) = fixture();
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    // The mac's registry refuses to keep any section about the laptop.
    rusqlite::Connection::open(mac.registry())
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER refuse_laptop BEFORE INSERT ON fleet_snapshots
             WHEN NEW.machine = 'laptop' BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .unwrap();
    let waiting = mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "liaison", "kind": "notice", "body": "Built 9"}),
    );
    assert_eq!(waiting["delivery"], "pending", "{waiting}");

    let collected = collect_as(&laptop, "liaison", &remote_ssh(&mac));
    assert_eq!(collected["collected"][0]["delivered"], 1, "{collected}");
    let notices = messages(&laptop, "liaison", "notice");
    assert!(
        notices.iter().any(|m| m["body"] == "Built 9"),
        "{notices:?}"
    );
    // Nothing was kept: the mac still knows the laptop from its directory.
    let snapshot = mac.ok("fitforge", "fleet.snapshot", json!({}));
    assert_eq!(
        machine_row(&snapshot, "laptop")["via"],
        "directory",
        "{snapshot}"
    );
}

#[test]
fn a_section_carried_under_a_name_that_does_not_reach_this_machine_is_refused() {
    let (_temp, laptop, mac, ssh) = fixture();
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    let section = json!({"as_of_ms": 1, "changed_ms": 1,
        "section": {"machine": {"name": "ghost"}, "ships": [{"name": "fake"}], "needs": []}});
    let refused = mac.gateway(
        "fleet.collect",
        json!({"machine": "ghost", "captain": true, "snapshot": section}),
    );
    assert_eq!(refused["ok"], false, "{refused}");
    let kept: i64 = rusqlite::Connection::open(mac.registry())
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM fleet_snapshots WHERE machine='ghost'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, 0, "nothing carried was kept");
    let snapshot = mac.ok("fitforge", "fleet.snapshot", json!({}));
    assert!(
        !snapshot["ships"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "fake"),
        "{snapshot}"
    );
}

#[test]
fn a_gateway_too_old_to_serve_the_snapshot_reads_unreachable_with_its_refusal() {
    let (_temp, laptop, mac, ssh) = fixture();
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    // An older gateway knows no fleet.snapshot: rename the command on its way
    // in, so the real gateway refuses it as one that predates it would.
    let old = mac.root.join("old-gateway-ssh");
    fs::write(
        &old,
        format!(
            "#!/bin/sh\nfor argument do remote=$argument; done\necho connected >> {root}/ssh.log\nsed -u 's/\"fleet.snapshot\"/\"fleet.snapshot.unknown\"/' | env HOME={root} PATH={root}/bin:/usr/bin:/bin PRIVATEER_FLEET={root}/fleet.sqlite PRIVATEER_RESOURCE_HOME={root}/ledger /bin/sh -c \"$remote\"\n",
            root = mac.root.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&old, fs::Permissions::from_mode(0o755)).unwrap();

    let snapshot = laptop.ok_over("liaison", "fleet.snapshot", json!({}), &old);
    assert_eq!(connections(&mac), 1, "the old gateway was asked");
    let row = machine_row(&snapshot, "mac");
    assert_eq!(row["reachable"], false, "{snapshot}");
    assert_eq!(row["via"], "directory", "{snapshot}");
    assert!(
        row["error"]
            .as_str()
            .unwrap()
            .contains("unknown gateway command fleet.snapshot"),
        "{snapshot}"
    );
    assert_eq!(row["routes"], json!([]));
    // Not an empty machine: its ships are listed from the directory, unknown.
    let listed = ships_on(&snapshot, "mac");
    assert_eq!(listed.len(), 1, "{snapshot}");
    assert_eq!(listed[0]["name"], "fitforge");
    assert_eq!(listed[0]["presence"], "unknown");
    // This machine's own section is whole.
    assert_eq!(machine_row(&snapshot, "laptop")["reachable"], true);
    assert_eq!(ships_on(&snapshot, "laptop").len(), 1, "{snapshot}");
}

/// Record a settled harness job in a ship's home, as its supervisor would.
fn settled_job(machine: &Machine, ship: &str, id: &str, route: &str, usage: Value, at: i64) {
    settled_job_with(
        machine,
        ship,
        id,
        route,
        usage,
        at,
        json!({"agent_actor": ship}),
    );
}

/// The same, with fields of the record replaced by `extra`.
fn settled_job_with(
    machine: &Machine,
    ship: &str,
    id: &str,
    route: &str,
    usage: Value,
    at: i64,
    extra: Value,
) {
    let home: String = rusqlite::Connection::open(machine.registry())
        .unwrap()
        .query_row("SELECT home FROM ships WHERE name=?1", [ship], |r| r.get(0))
        .unwrap();
    let mut record = json!({"id": id, "action": "spawn", "status": "succeeded",
        "model_route": route, "usage": usage, "created_ms": at - 1_000, "finished_ms": at});
    for (key, value) in extra.as_object().unwrap() {
        record[key] = value.clone();
    }
    rusqlite::Connection::open(PathBuf::from(home).join("privateer.sqlite"))
        .unwrap()
        .execute(
            "INSERT INTO jobs(id, record) VALUES(?1, ?2)",
            params![id, record.to_string()],
        )
        .unwrap();
}

#[test]
fn the_usage_summary_sums_a_reached_machine_and_lists_it_when_it_is_down() {
    let (_temp, laptop, mac, ssh) = fixture();
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    let used = |n: u64| json!({"input": n, "output": n, "cacheRead": n, "cacheWrite": n, "totalTokens": 4 * n});
    let day = 1_790_000_000_000_i64;
    settled_job(&laptop, "liaison", "a", "claude/-/opus", used(1), day);
    settled_job(&mac, "fitforge", "b", "claude/-/opus", used(10), day + 1);
    settled_job(&mac, "fitforge", "c", "pi/zai/glm", Value::Null, day + 2);
    // Outside the period.
    settled_job(&mac, "fitforge", "d", "pi/zai/glm", used(100), day + 10_000);
    let remote = remote_ssh(&mac);
    let ask = |group_by: &str, ssh: &Path| {
        laptop.ok_over(
            "liaison",
            "usage.summary",
            json!({"since_ms": day, "until_ms": day + 10_000, "group_by": group_by}),
            ssh,
        )
    };

    let by_machine = ask("machine", &remote);
    assert_eq!(
        by_machine["groups"],
        json!([
            {"key": "laptop", "jobs": 1, "usage": used(1)},
            {"key": "mac", "jobs": 2, "usage": used(10), "gaps": ["pi"]},
        ]),
        "{by_machine}"
    );
    let reached: Vec<_> = by_machine["machines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["name"].clone(), m["reached"].clone()))
        .collect();
    assert_eq!(
        reached,
        vec![(json!("laptop"), json!(true)), (json!("mac"), json!(true))]
    );
    let by_route = ask("route", &remote);
    assert_eq!(
        by_route["groups"],
        json!([
            {"key": "claude/-/opus", "jobs": 2, "usage": used(11)},
            {"key": "pi/zai/glm", "jobs": 1, "usage": null, "gaps": ["pi"]},
        ]),
        "{by_route}"
    );
    let by_ship = ask("ship", &remote);
    assert_eq!(by_ship["groups"][0]["key"], "fitforge", "{by_ship}");
    assert_eq!(by_ship["groups"][1]["key"], "liaison", "{by_ship}");
    assert_eq!(
        ask("day", &remote)["groups"],
        json!([{"key": "2026-09-21", "jobs": 3, "usage": used(11), "gaps": ["pi"]}])
    );

    // The gateway answers for its own homes only, and to the operator.
    let own = mac.gateway(
        "usage.summary",
        json!({"since_ms": day, "until_ms": day + 10_000, "group_by": "machine"}),
    );
    assert_eq!(own["result"]["groups"][0]["jobs"], 2, "{own}");

    // Down, the mac is listed as not reached; the laptop's own still counts.
    let down = remote_ssh_with(&mac, "down-ssh", "| (cat >/dev/null; echo garbage)");
    let partial = ask("machine", &down);
    assert_eq!(
        partial["groups"],
        json!([{"key": "laptop", "jobs": 1, "usage": used(1)}]),
        "{partial}"
    );
    let mac_row = partial["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "mac")
        .cloned()
        .unwrap_or_else(|| panic!("mac missing: {partial}"));
    assert_eq!(mac_row["reached"], false, "{partial}");
    assert!(mac_row["error"].as_str().is_some(), "{partial}");

    // The reached side cannot ask back over a one-way link, and says so.
    let other_side = mac.ok(
        "fitforge",
        "usage.summary",
        json!({"since_ms": day, "until_ms": day + 10_000, "group_by": "machine"}),
    );
    let laptop_row = other_side["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "laptop")
        .cloned()
        .unwrap_or_else(|| panic!("laptop missing: {other_side}"));
    assert_eq!(laptop_row["reached"], false, "{other_side}");
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn machine_entry(summary: &Value, name: &str) -> Value {
    summary["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("{name} missing: {summary}"))
}

#[test]
fn the_reached_machine_sums_the_usage_a_reaching_machine_carries_on_collect() {
    let (_temp, laptop, mac, ssh) = fixture();
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    let used = |n: u64| json!({"input": n, "output": n, "cacheRead": n, "cacheWrite": n, "totalTokens": 4 * n});
    // Settled a minute ago: past the margin a carry leaves for records still
    // committing.
    let at = now_ms() - 60_000;
    settled_job(&laptop, "liaison", "a", "claude/-/opus", used(1), at);
    settled_job_with(
        &laptop,
        "liaison",
        "b",
        "opencode/zai/glm",
        used(2),
        at + 1,
        json!({"agent_actor": "crew-1", "usage_partial": true}),
    );
    settled_job(&laptop, "liaison", "c", "pi/zai/glm", Value::Null, at + 2);
    // Too recent to carry yet.
    settled_job(
        &laptop,
        "liaison",
        "late",
        "claude/-/opus",
        used(50),
        now_ms(),
    );
    // The mac's own job, with no actor: keyed by the asking home's ship.
    settled_job_with(
        &mac,
        "fitforge",
        "m",
        "claude/-/opus",
        used(10),
        at + 3,
        json!({"agent_actor": null}),
    );
    let ask = |group_by: &str| {
        mac.ok(
            "fitforge",
            "usage.summary",
            json!({"since_ms": at - 1_000, "group_by": group_by}),
        )
    };
    // Before any collect the laptop is listed, not reached, with nothing carried.
    let before = ask("machine");
    let row = machine_entry(&before, "laptop");
    assert_eq!(row["reached"], false, "{before}");
    assert_eq!(row["as_of_ms"], Value::Null, "{before}");

    collect_as(&laptop, "liaison", &remote_ssh(&mac));
    let by_machine = ask("machine");
    // The reached side gave itself no name: it goes by its host's.
    let here = by_machine["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] != "laptop")
        .map(|m| m["name"].clone())
        .unwrap();
    // Groups come sorted by key, and the host's name may sort either side
    // of "laptop", so compare them in that same order.
    let mut expected = vec![
        json!({"key": "laptop", "jobs": 3, "usage": used(3), "gaps": ["pi"], "partial_jobs": 1}),
        json!({"key": here, "jobs": 1, "usage": used(10)}),
    ];
    expected.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
    assert_eq!(by_machine["groups"], json!(expected), "{by_machine}");
    let row = machine_entry(&by_machine, "laptop");
    assert_eq!(row["reached"], true, "{by_machine}");
    assert_eq!(row["via"], "collect", "{by_machine}");
    let as_of = row["as_of_ms"].as_i64().unwrap();
    assert!(as_of >= at + 2 && as_of <= now_ms(), "{by_machine}");
    assert_eq!(
        ask("route")["groups"],
        json!([
            {"key": "claude/-/opus", "jobs": 2, "usage": used(11)},
            {"key": "opencode/zai/glm", "jobs": 1, "usage": used(2), "partial_jobs": 1},
            {"key": "pi/zai/glm", "jobs": 1, "usage": null, "gaps": ["pi"]},
        ])
    );
    let ships: Vec<Value> = ask("ship")["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| json!([g["key"], g["jobs"]]))
        .collect();
    assert_eq!(
        ships,
        vec![
            json!(["crew-1", 1]),
            json!(["fitforge", 1]),
            json!(["liaison", 2])
        ]
    );
    let days = ask("day");
    assert_eq!(
        days["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["jobs"].as_u64().unwrap())
            .sum::<u64>(),
        4,
        "{days}"
    );

    // Carried again from the start, the same jobs count once.
    let registry = rusqlite::Connection::open(laptop.registry()).unwrap();
    let cursor: Option<String> = registry
        .query_row(
            "SELECT usage_cursor FROM machines WHERE name='mac'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let cursor: Value = serde_json::from_str(&cursor.expect("the cursor the mac stored")).unwrap();
    assert_eq!(cursor["job_id"], "c", "{cursor}");
    registry
        .execute("UPDATE machines SET usage_cursor=NULL", [])
        .unwrap();
    collect_as(&laptop, "liaison", &remote_ssh(&mac));
    let again = ask("machine");
    assert_eq!(again["groups"], by_machine["groups"]);
    let as_of = machine_entry(&again, "laptop")["as_of_ms"]
        .as_i64()
        .unwrap();

    // Silent for long, the laptop is not reached; what it carried still counts.
    rusqlite::Connection::open(mac.registry())
        .unwrap()
        .execute(
            "UPDATE fleet_usage_contacts SET received_ms=received_ms-600000",
            [],
        )
        .unwrap();
    let quiet = ask("machine");
    let row = machine_entry(&quiet, "laptop");
    assert_eq!(row["reached"], false, "{quiet}");
    assert_eq!(row["as_of_ms"].as_i64(), Some(as_of), "{quiet}");
    assert!(row["error"].as_str().unwrap().contains("lately"), "{quiet}");
    assert_eq!(quiet["groups"], by_machine["groups"]);

    // Its link removed here, nothing it carried is kept.
    let removed = mac.run("fitforge", &["machine", "remove", "laptop"], None);
    assert!(removed.status.success(), "{}", text(&removed));
    let kept: i64 = rusqlite::Connection::open(mac.registry())
        .unwrap()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM fleet_usage) + (SELECT COUNT(*) FROM fleet_usage_contacts)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, 0);
}

/// One laptop home that cannot be read does not empty the whole carry: the
/// readable homes' jobs still count. The cursor does not pass the unread
/// home's jobs, and the laptop is not dated as if all of it was carried.
#[test]
fn an_unreadable_home_carries_the_other_homes_usage_but_holds_the_cursor() {
    let (_temp, laptop, mac, ssh) = fixture();
    laptop.ship("rig");
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    let at = now_ms() - 60_000;
    settled_job(
        &laptop,
        "liaison",
        "a",
        "claude/-/opus",
        json!({"input": 1}),
        at,
    );
    make_unreadable(&laptop, "rig");

    collect_as(&laptop, "liaison", &remote_ssh(&mac));
    let summary = mac.ok(
        "fitforge",
        "usage.summary",
        json!({"since_ms": at - 1_000, "group_by": "machine"}),
    );
    let groups = summary["groups"].as_array().unwrap();
    assert!(
        groups
            .iter()
            .any(|g| g["key"] == "laptop" && g["jobs"] == 1),
        "{summary}"
    );
    assert_eq!(
        machine_entry(&summary, "laptop")["reached"],
        false,
        "{summary}"
    );
    let cursor: Option<String> = rusqlite::Connection::open(laptop.registry())
        .unwrap()
        .query_row(
            "SELECT usage_cursor FROM machines WHERE name='mac'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cursor, None, "the cursor passed the unread home");
}

#[test]
fn usage_the_reached_machine_cannot_keep_never_stops_the_mail_or_moves_the_cursor() {
    let (_temp, laptop, mac, ssh) = fixture();
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    settled_job(
        &laptop,
        "liaison",
        "a",
        "claude/-/opus",
        json!({"input": 1}),
        now_ms() - 60_000,
    );
    rusqlite::Connection::open(mac.registry())
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER refuse_usage BEFORE INSERT ON fleet_usage
             BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .unwrap();
    let waiting = mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "liaison", "kind": "notice", "body": "Built 9"}),
    );
    assert_eq!(waiting["delivery"], "pending", "{waiting}");

    let collected = collect_as(&laptop, "liaison", &remote_ssh(&mac));
    assert_eq!(collected["collected"][0]["delivered"], 1, "{collected}");
    let notices = messages(&laptop, "liaison", "notice");
    assert!(
        notices.iter().any(|m| m["body"] == "Built 9"),
        "{notices:?}"
    );
    let cursor: Option<String> = rusqlite::Connection::open(laptop.registry())
        .unwrap()
        .query_row(
            "SELECT usage_cursor FROM machines WHERE name='mac'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cursor, None, "nothing kept, so nothing passed");
    let summary = mac.ok(
        "fitforge",
        "usage.summary",
        json!({"since_ms": 0, "group_by": "machine"}),
    );
    assert_eq!(
        machine_entry(&summary, "laptop")["reached"],
        false,
        "{summary}"
    );

    // Once the mac can keep it, the next collect carries it.
    rusqlite::Connection::open(mac.registry())
        .unwrap()
        .execute_batch("DROP TRIGGER refuse_usage")
        .unwrap();
    collect_as(&laptop, "liaison", &remote_ssh(&mac));
    let summary = mac.ok(
        "fitforge",
        "usage.summary",
        json!({"since_ms": 0, "group_by": "machine"}),
    );
    assert_eq!(summary["groups"][0]["key"], "laptop", "{summary}");
    assert_eq!(summary["groups"][0]["jobs"], 1, "{summary}");
}

#[test]
fn the_activity_list_merges_reached_machine_and_lists_reaching_machines() {
    let (_temp, laptop, mac, ssh) = fixture();
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    let remote = remote_ssh(&mac);

    let store_laptop =
        privateer::store::Store::open(&laptop.folder("liaison").join(".privateer")).unwrap();
    let store_mac =
        privateer::store::Store::open(&mac.folder("fitforge").join(".privateer")).unwrap();

    let conn_l = store_laptop.connection();
    let conn_m = store_mac.connection();

    conn_l.execute("DELETE FROM audit;", []).unwrap();
    conn_m.execute("DELETE FROM audit;", []).unwrap();
    let _ = conn_l.execute("DELETE FROM sqlite_sequence WHERE name='audit';", []);
    let _ = conn_m.execute("DELETE FROM sqlite_sequence WHERE name='audit';", []);

    conn_l
        .execute(
            "INSERT INTO audit(time_ms, kind, payload) VALUES (?1, ?2, ?3)",
            rusqlite::params![100, "laptop.event", r#"{"ship":"liaison"}"#],
        )
        .unwrap();
    conn_m
        .execute(
            "INSERT INTO audit(time_ms, kind, payload) VALUES (?1, ?2, ?3)",
            rusqlite::params![200, "mac.event", r#"{"ship":"fitforge"}"#],
        )
        .unwrap();

    // The gateway answers for its own homes only:
    let own = mac.gateway("activity.list", json!({}));
    assert_eq!(
        own["result"]["entries"].as_array().unwrap().len(),
        1,
        "{own}"
    );
    assert_eq!(own["result"]["entries"][0]["kind"], "mac.event");

    // Laptop asks over gateway to mac:
    let merged = laptop.ok_over("liaison", "activity.list", json!({}), &remote);
    let entries = merged["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "{merged}");
    assert_eq!(entries[0]["kind"], "laptop.event");
    assert_eq!(entries[0]["machine"], "laptop");
    assert_eq!(entries[0]["ship"], "liaison");
    assert_eq!(entries[0]["at_ms"], 100);

    assert_eq!(entries[1]["kind"], "mac.event");
    assert_eq!(entries[1]["machine"], "mac");
    assert_eq!(entries[1]["ship"], "fitforge");
    assert_eq!(entries[1]["at_ms"], 200);

    let machines = merged["machines"].as_array().unwrap();
    let mac_entry = machines.iter().find(|m| m["name"] == "mac").unwrap();
    assert_eq!(mac_entry["reached"], true);

    let laptop_entry = machines.iter().find(|m| m["name"] == "laptop").unwrap();
    assert_eq!(laptop_entry["reached"], true);

    // When mac is down, laptop still gets its own entries and mac is reached: false with error:
    let down = remote_ssh_with(&mac, "down-ssh", "| (cat >/dev/null; echo garbage)");
    let partial = laptop.ok_over("liaison", "activity.list", json!({}), &down);
    assert_eq!(partial["entries"].as_array().unwrap().len(), 1, "{partial}");
    assert_eq!(partial["entries"][0]["kind"], "laptop.event");
    let mac_row = partial["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "mac")
        .cloned()
        .unwrap();
    assert_eq!(mac_row["reached"], false, "{partial}");
    assert!(mac_row["error"].as_str().is_some(), "{partial}");

    // The reaching side (mac) cannot ask back over a one-way link:
    let mac_side = mac.ok("fitforge", "activity.list", json!({}));
    let laptop_row = mac_side["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "laptop")
        .cloned()
        .unwrap();
    assert_eq!(laptop_row["reached"], false, "{mac_side}");
    assert!(
        laptop_row["error"]
            .as_str()
            .unwrap()
            .contains("links one way")
    );
}

#[test]
fn the_activity_list_pages_across_reached_machine_and_after_time_keeps_reached() {
    let (_temp, laptop, mac, ssh) = fixture();
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    let remote = remote_ssh(&mac);

    let store_laptop =
        privateer::store::Store::open(&laptop.folder("liaison").join(".privateer")).unwrap();
    let store_mac =
        privateer::store::Store::open(&mac.folder("fitforge").join(".privateer")).unwrap();

    let conn_l = store_laptop.connection();
    let conn_m = store_mac.connection();

    conn_l.execute("DELETE FROM audit;", []).unwrap();
    conn_m.execute("DELETE FROM audit;", []).unwrap();
    let _ = conn_l.execute("DELETE FROM sqlite_sequence WHERE name='audit';", []);
    let _ = conn_m.execute("DELETE FROM sqlite_sequence WHERE name='audit';", []);

    // Real millisecond times, so cursors are as long as they are in use.
    const T0: i64 = 1_760_802_000_000;
    conn_l
        .execute(
            "INSERT INTO audit(time_ms, kind, payload) VALUES (?1, ?2, ?3)",
            rusqlite::params![T0 + 100, "laptop.event1", r#"{"ship":"liaison"}"#],
        )
        .unwrap();
    conn_m
        .execute(
            "INSERT INTO audit(time_ms, kind, payload) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                T0 + 200,
                "mac.event1",
                r#"{"ship":"fitforge","secret_token":"leak123","error":"Bearer leaktoken456"}"#
            ],
        )
        .unwrap();
    conn_l
        .execute(
            "INSERT INTO audit(time_ms, kind, payload) VALUES (?1, ?2, ?3)",
            rusqlite::params![T0 + 300, "laptop.event2", r#"{"ship":"liaison"}"#],
        )
        .unwrap();
    conn_m
        .execute(
            "INSERT INTO audit(time_ms, kind, payload) VALUES (?1, ?2, ?3)",
            rusqlite::params![T0 + 400, "mac.event2", r#"{"ship":"fitforge"}"#],
        )
        .unwrap();

    // 1. Paging that crosses a reached machine:
    let mut pages = vec![];
    let mut args = json!({"limit": 1});
    loop {
        let page = laptop.ok_over("liaison", "activity.list", args.clone(), &remote);
        let more = page["more"].as_bool().unwrap();
        let next = page["next"].clone();
        pages.push(page);
        if !more {
            break;
        }
        args["after"] = next;
        assert!(pages.len() < 10, "paging loop did not terminate");
    }

    assert_eq!(pages.len(), 4, "{pages:?}");
    assert_eq!(pages[0]["entries"][0]["kind"], "laptop.event1");
    assert_eq!(pages[0]["entries"][0]["machine"], "laptop");
    assert_eq!(pages[1]["entries"][0]["kind"], "mac.event1");
    assert_eq!(pages[1]["entries"][0]["machine"], "mac");
    assert_eq!(pages[2]["entries"][0]["kind"], "laptop.event2");
    assert_eq!(pages[2]["entries"][0]["machine"], "laptop");
    assert_eq!(pages[3]["entries"][0]["kind"], "mac.event2");
    assert_eq!(pages[3]["entries"][0]["machine"], "mac");

    // Every entry has a valid cursor
    for page in &pages {
        let e = &page["entries"][0];
        let cursor = e["cursor"].as_str().unwrap();
        assert!(
            cursor.starts_with("a1.") && !cursor.contains('•'),
            "{cursor}"
        );
    }
    // Resuming from the last entry, a remote one, returns nothing newer.
    let tail = laptop.ok_over(
        "liaison",
        "activity.list",
        json!({"after": pages[3]["entries"][0]["cursor"]}),
        &remote,
    );
    assert_eq!(tail["entries"], json!([]), "{tail}");

    // Remote entry was masked on this side:
    let mac_detail = &pages[1]["entries"][0]["detail"];
    assert_eq!(mac_detail["secret_token"], "<set>");
    assert!(mac_detail["error"].as_str().unwrap().contains("•••"));
    assert!(
        !mac_detail["error"]
            .as_str()
            .unwrap()
            .contains("leaktoken456")
    );

    // 2. after as a time with a reached machine keeps it reached:
    let after_time = laptop.ok_over(
        "liaison",
        "activity.list",
        json!({"after": T0 + 150}),
        &remote,
    );
    let machines = after_time["machines"].as_array().unwrap();
    let mac_entry = machines.iter().find(|m| m["name"] == "mac").unwrap();
    assert_eq!(mac_entry["reached"], true, "{after_time}");
    assert!(mac_entry.get("error").is_none(), "{after_time}");

    let after_entries = after_time["entries"].as_array().unwrap();
    assert_eq!(after_entries.len(), 3, "{after_time}");
    assert_eq!(after_entries[0]["kind"], "mac.event1");
    assert_eq!(after_entries[0]["at_ms"], T0 + 200);
    assert_eq!(after_entries[1]["kind"], "laptop.event2");
    assert_eq!(after_entries[1]["at_ms"], T0 + 300);
    assert_eq!(after_entries[2]["kind"], "mac.event2");
    assert_eq!(after_entries[2]["at_ms"], T0 + 400);
}

#[test]
fn a_file_the_captain_sends_to_a_ship_on_the_other_machine_crosses_in_chunks() {
    let (_temp, laptop, mac, ssh) = fixture();
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));

    // More than two chunks of 512 KiB, so the file crosses the link in parts.
    let data: Vec<u8> = (0..1_258_291).map(|i| (i * 31 % 251) as u8).collect();
    let source = laptop.root.join("plan.bin");
    fs::write(&source, &data).unwrap();
    let put = laptop.ok_over(
        "liaison",
        "file.put",
        json!({"to": "fitforge", "name": "plan.bin", "content_file": source}),
        &ssh,
    );
    assert_eq!(put["ship"], "fitforge", "{put}");
    assert_eq!(put["type"], "application/octet-stream");
    let stored = PathBuf::from(put["path"].as_str().unwrap());
    assert!(stored.starts_with(mac.folder("fitforge")), "{stored:?}");
    assert_eq!(fs::read(&stored).unwrap(), data);

    // The captain's mail names the file; it reaches the Mac with its path there.
    let sent = laptop.ok_over(
        "liaison",
        "captain.send",
        json!({"to": "fitforge", "body": "Plan attached.",
            "payload": {"files": [{"file_id": put["file_id"]}]}}),
        &ssh,
    );
    assert_eq!(sent["mail"]["delivery"], "delivered", "{sent}");
    let mail = messages(&mac, "fitforge", "notice");
    let message = mail
        .iter()
        .find(|m| m["body"] == "Plan attached.")
        .unwrap_or_else(|| panic!("no captain mail in {mail:?}"));
    let file = &message["payload"]["files"][0];
    assert_eq!(file["file_id"], put["file_id"], "{message}");
    assert_eq!(file["path"], put["path"], "{message}");
    assert_eq!(fs::read(file["path"].as_str().unwrap()).unwrap(), data);
}

#[test]
fn a_chunked_upload_whose_bytes_do_not_match_its_sha256_is_refused_on_the_other_machine() {
    let (_temp, _laptop, mac, _ssh) = fixture();
    let begun = mac.gateway(
        "file.begin",
        json!({"ship": "fitforge", "name": "bad.txt", "bytes": 3, "sha256": "0".repeat(64)}),
    );
    assert_eq!(begun["ok"], true, "{begun}");
    let upload = begun["result"]["upload_id"].as_str().unwrap().to_string();
    // "abc" in base64.
    let appended = mac.gateway(
        "file.append",
        json!({"ship": "fitforge", "upload_id": upload, "offset": 0, "data": "YWJj"}),
    );
    assert_eq!(appended["ok"], true, "{appended}");
    let committed = mac.gateway(
        "file.commit",
        json!({"ship": "fitforge", "upload_id": upload}),
    );
    assert_eq!(committed["ok"], false, "{committed}");
    assert!(
        committed["error"]
            .as_str()
            .unwrap()
            .contains("does not match"),
        "{committed}"
    );
}

#[test]
fn an_agent_is_refused_at_every_gateway_file_command() {
    let (_temp, _laptop, mac, _ssh) = fixture();
    let agent = &[("PRIVATEER_ROLE", "agent"), ("PRIVATEER_ACTOR", "fitforge")];
    for (command, args) in [
        (
            "file.begin",
            json!({"ship": "fitforge", "name": "x", "bytes": 1, "sha256": "0".repeat(64)}),
        ),
        (
            "file.append",
            json!({"ship": "fitforge", "upload_id": "upload-x", "offset": 0, "data": "YQ=="}),
        ),
        (
            "file.commit",
            json!({"ship": "fitforge", "upload_id": "upload-x"}),
        ),
        (
            "file.describe",
            json!({"ship": "fitforge", "file_id": "file-x"}),
        ),
    ] {
        let refused = mac.gateway_with(command, args, agent);
        assert_eq!(refused["ok"], false, "{command}: {refused}");
        assert!(
            refused["error"]
                .as_str()
                .unwrap()
                .contains("operator command"),
            "{command}: {refused}"
        );
    }
}

#[test]
fn another_ships_file_id_is_refused_across_machines_too() {
    let (_temp, laptop, mac, ssh) = fixture();
    mac.ship("scout");
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    let source = laptop.root.join("plan.md");
    fs::write(&source, b"plan").unwrap();
    let put = laptop.ok_over(
        "liaison",
        "file.put",
        json!({"to": "fitforge", "name": "plan.md", "content_file": source}),
        &ssh,
    );
    // The captain cannot pass fitforge's file to scout, who lives on the same
    // machine as fitforge: its machine says whose the file is.
    let refused = laptop.run(
        "liaison",
        &[
            "captain.send",
            "--json",
            &json!({"to": "scout", "body": "Plan.",
                "payload": {"files": [{"file_id": put["file_id"]}]}})
            .to_string(),
        ],
        Some(&ssh),
    );
    let said = text(&refused);
    assert!(!refused.status.success(), "{said}");
    assert!(said.contains("fitforge's, not scout's"), "{said}");
    assert!(messages(&mac, "scout", "notice").is_empty());
}

#[test]
fn files_cross_a_link_only_on_the_captains_mail_and_by_its_own_rows() {
    let (_temp, laptop, mac, ssh) = fixture();
    let linked = link(&laptop, &mac, &ssh);
    assert!(linked.status.success(), "{}", text(&linked));
    let source = laptop.root.join("plan.md");
    fs::write(&source, b"plan").unwrap();
    let put = laptop.ok_over(
        "liaison",
        "file.put",
        json!({"to": "fitforge", "name": "plan.md", "content_file": source}),
        &ssh,
    );
    let file_id = put["file_id"].clone();

    // A ship's delivered mail cannot carry files: the wake would show them.
    // Such mail can only predate that rule or come from an older machine, so
    // it delivers without the files rather than retrying forever.
    // A delivery that strips files and then fails (here, a reply naming an
    // unknown request) leaves no audit row; a retry that succeeds leaves exactly one.
    let failed = mac.gateway(
        "mail.deliver",
        json!({"id": "liaison-files", "from": "liaison", "to": "fitforge",
            "kind": "reply", "correlation": "unknown-request", "body": "Look.",
            "payload": {"files": [{"file_id": file_id}]}}),
    );
    assert_eq!(failed["ok"], false, "{failed}");
    let audit_before = mac.ok("fitforge", "audit.list", json!({"limit": 1000}));
    assert!(
        !audit_before["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "mail.files_dropped"),
        "{audit_before}"
    );

    let delivered = mac.gateway(
        "mail.deliver",
        json!({"id": "liaison-files", "from": "liaison", "to": "fitforge",
            "kind": "notice", "body": "Look.",
            "payload": {"files": [{"file_id": file_id}]}}),
    );
    assert_eq!(delivered["ok"], true, "{delivered}");
    let dropped = messages(&mac, "fitforge", "notice");
    assert_eq!(dropped.len(), 1, "{dropped:?}");
    assert_eq!(dropped[0]["body"], "Look.", "{dropped:?}");
    assert!(dropped[0]["payload"].get("files").is_none(), "{dropped:?}");
    assert_eq!(dropped[0]["payload"]["files_dropped"], 1, "{dropped:?}");
    let audit = mac.ok("fitforge", "audit.list", json!({"limit": 1000}));
    let dropped_events: Vec<_> = audit["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "mail.files_dropped" && e["payload"]["id"] == "liaison-files")
        .collect();
    assert_eq!(dropped_events.len(), 1, "{audit}");

    // The captain's mail may, and the home that reads it keeps only what its
    // own rows say: a forged path does not survive the crossing.
    let delivered = mac.gateway(
        "mail.deliver",
        json!({"id": "captain-files", "from": "captain", "to": "fitforge",
            "kind": "notice", "body": "Plan attached.",
            "payload": {"files": [{"file_id": file_id, "name": "plan.md", "bytes": 4,
                "type": "text/markdown", "path": "/home/someone/.ssh/id_ed25519"}]}}),
    );
    assert_eq!(delivered["ok"], true, "{delivered}");
    let mail = messages(&mac, "fitforge", "notice");
    let plan = mail
        .iter()
        .find(|m| m["body"] == "Plan attached.")
        .unwrap_or_else(|| panic!("{mail:?}"));
    let file = &plan["payload"]["files"][0];
    assert_eq!(file["file_id"], file_id, "{mail:?}");
    assert_eq!(file["path"], put["path"], "{mail:?}");
    assert!(
        file["path"].as_str().unwrap().contains("fitforge"),
        "{mail:?}"
    );
    assert_eq!(fs::read(file["path"].as_str().unwrap()).unwrap(), b"plan");
}

#[test]
fn machine_add_reports_what_the_linked_machine_lacks_and_links_all_the_same() {
    let (_temp, laptop, mac, _ssh) = fixture();
    // The Mac's fitforge runs codex, which is not on the Mac's PATH.
    let mac_ssh = remote_ssh(&mac);
    let shell = bare_login_shell(&mac);
    let linked = link_command(&laptop, "liaison", "laptop", &mac, &mac_ssh)
        .env("SHELL", &shell)
        .output()
        .unwrap();
    assert!(linked.status.success(), "{}", text(&linked));
    let result: Value = serde_json::from_slice(&linked.stdout).unwrap();
    let readiness = &result["readiness"];
    assert_eq!(readiness["status"], "failing", "{result}");
    let codex = readiness["failing"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["tool"] == "codex")
        .unwrap_or_else(|| panic!("codex missing: {result}"));
    assert_eq!(codex["level"], "block");
    assert_eq!(codex["needed_by"], json!(["fitforge"]));
    assert_eq!(
        codex["install"],
        "install codex so it is on the dispatcher's PATH"
    );
    let summary = readiness["summary"].as_str().unwrap();
    assert!(
        summary.starts_with("readiness: ")
            && summary.contains("codex (block): ")
            && summary.contains("the first mate of fitforge cannot wake")
            && summary.contains("Install: install codex"),
        "{summary}"
    );
    println!("machine add readiness: {summary}");
    // The link is made regardless.
    let fitforge = laptop.ok("liaison", "ship.show", json!({"name": "fitforge"}));
    assert_eq!(fitforge["location"], json!({"peer": "machine-mac"}));
    assert_eq!(result["ships"], json!(["fitforge"]), "{result}");
}

#[test]
fn the_gateway_reports_its_machines_readiness_to_the_operator_only() {
    let (_temp, _laptop, mac, _ssh) = fixture();
    let shell = bare_login_shell(&mac);
    let env = [("SHELL", shell.to_str().unwrap())];
    let home = mac.folder("fitforge").join(".privateer");
    let database = home.join("privateer.sqlite");
    // Rows its dispatcher stored a moment ago: codex missing, the rest found.
    {
        let conn = rusqlite::Connection::open(&database).unwrap();
        for (tool, ok, why) in [
            ("git", true, None),
            ("python3", true, None),
            (
                "codex",
                false,
                Some("codex is not on the dispatcher's PATH"),
            ),
        ] {
            conn.execute(
                "INSERT OR REPLACE INTO machine_readiness(tool, level, ok, why, install,
                   needed_by, fingerprint, checked_ms)
                 VALUES(?1, 'block', ?2, ?3, 'install it', '[\"fitforge\"]', 'test',
                   CAST(strftime('%s','now') AS INTEGER) * 1000)",
                params![tool, ok, why],
            )
            .unwrap();
        }
    }
    let answer = mac.gateway_with("machine.readiness", json!({}), &env);
    assert_eq!(answer["ok"], true, "{answer}");
    let report = &answer["result"];
    assert_eq!(report["homes"].as_array().unwrap().len(), 1, "{report}");
    assert_eq!(
        fs::canonicalize(report["homes"][0]["home"].as_str().unwrap()).unwrap(),
        fs::canonicalize(&home).unwrap(),
        "{report}"
    );
    // The stored rows are fresh, so they answer.
    assert_eq!(report["homes"][0]["source"], "stored", "{report}");
    assert!(
        report["failing"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["tool"] == "codex" && row["level"] == "block"),
        "{report}"
    );

    // Installed since, and the rows grown old: checked live, stored nowhere.
    mac.ready();
    let aged = |conn: &rusqlite::Connection| {
        conn.execute(
            "UPDATE machine_readiness SET checked_ms=checked_ms-86400000",
            [],
        )
        .unwrap();
    };
    aged(&rusqlite::Connection::open(&database).unwrap());
    let stored = |conn: &rusqlite::Connection| -> Vec<(String, bool, i64)> {
        conn.prepare("SELECT tool, ok, checked_ms FROM machine_readiness ORDER BY tool")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    let before = stored(&rusqlite::Connection::open(&database).unwrap());
    let answer = mac.gateway_with("machine.readiness", json!({}), &env);
    assert_eq!(answer["ok"], true, "{answer}");
    assert_eq!(answer["result"]["homes"][0]["source"], "live", "{answer}");
    assert_eq!(answer["result"]["failing"], json!([]), "{answer}");
    assert_eq!(
        stored(&rusqlite::Connection::open(&database).unwrap()),
        before,
        "the report is read-only"
    );

    // An agent may not read it, through the gateway or here.
    let agent = &[("PRIVATEER_ROLE", "agent"), ("PRIVATEER_ACTOR", "fitforge")];
    let refused = mac.gateway_with("machine.readiness", json!({}), agent);
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("machine.readiness is an operator command"),
        "{refused}"
    );
    let local = mac.run(
        "fitforge",
        &[
            "machine.readiness",
            "--json",
            "{}",
            "--role",
            "agent",
            "--actor",
            "fitforge",
        ],
        None,
    );
    assert!(!local.status.success(), "{}", text(&local));
    assert!(
        text(&local).contains("operator command"),
        "{}",
        text(&local)
    );
}

#[test]
fn machine_add_to_a_machine_too_old_for_readiness_reads_unknown() {
    let (_temp, laptop, mac, _ssh) = fixture();
    // An older gateway knows no machine.readiness: rename the command on its
    // way in, so the real gateway refuses it as one that predates it would.
    let old = mac.root.join("old-gateway-ssh");
    fs::write(
        &old,
        format!(
            "#!/bin/sh\nfor argument do remote=$argument; done\nsed -u 's/\"machine.readiness\"/\"machine.readiness.unknown\"/' | env HOME={root} PATH={root}/bin:/usr/bin:/bin PRIVATEER_FLEET={root}/fleet.sqlite PRIVATEER_RESOURCE_HOME={root}/ledger /bin/sh -c \"$remote\"\n",
            root = mac.root.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&old, fs::Permissions::from_mode(0o755)).unwrap();
    let linked = link(&laptop, &mac, &old);
    assert!(linked.status.success(), "{}", text(&linked));
    let result: Value = serde_json::from_slice(&linked.stdout).unwrap();
    assert_eq!(
        result["readiness"],
        json!({"status": "unknown", "failing": [],
            "summary": "readiness: unknown (remote is older)"}),
        "{result}"
    );
    assert_eq!(result["ships"], json!(["fitforge"]), "{result}");
}
