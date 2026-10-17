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
        let mut command = support::command(support::BIN);
        command
            .args(args)
            .current_dir(self.folder(ship))
            .env("HOME", &self.root)
            .env("PATH", "/usr/bin:/bin")
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
        command.output().unwrap()
    }

    fn ok(&self, ship: &str, command: &str, args: Value) -> Value {
        let output = self.run(ship, &[command, "--json", &args.to_string()], None);
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
    let registry = mac.registry();
    laptop.run(
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
    )
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
