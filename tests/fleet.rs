use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");

struct Machine {
    _temp: tempfile::TempDir,
    root: PathBuf,
    ships: PathBuf,
    path: String,
}

impl Machine {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let ships = root.join("ships");
        fs::create_dir(&ships).unwrap();
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("codex"), "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            path: format!("{}:/usr/bin:/bin", bin.display()),
            _temp: temp,
            root,
            ships,
        }
    }
    fn command(&self, dir: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        command
            .args(args)
            .current_dir(dir)
            .env("PATH", &self.path)
            .env("HOME", &self.root)
            .env("PRIVATEER_FLEET", self.root.join("fleet.sqlite"))
            .env("PRIVATEER_SERVICE", "none")
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env_remove("PRIVATEER_HOME")
            .env_remove("PRIVATEER_ROLE")
            .env_remove("PRIVATEER_ACTOR");
        command
    }
    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        self.command(dir, args).output().unwrap()
    }
    fn ok(&self, dir: &Path, args: &[&str]) -> String {
        let output = self.run(dir, args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
    fn json(&self, dir: &Path, command: &str, args: Value) -> Value {
        serde_json::from_str(&self.ok(dir, &[command, "--json", &args.to_string()])).unwrap()
    }
    fn as_agent(&self, dir: &Path, actor: &str, command: &str, args: Value) -> Output {
        self.command(dir, &[command, "--json", &args.to_string()])
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", actor)
            .output()
            .unwrap()
    }
    fn ship(&self, name: &str) -> PathBuf {
        let folder = self.ships.join(name);
        fs::create_dir_all(&folder).unwrap();
        self.ok(&folder, &["init"]);
        folder
    }
    fn fleet(&self, dir: &Path) -> Vec<String> {
        self.json(dir, "fleet.list", json!({}))["ships"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["retired_ms"].is_null())
            .map(|s| s["name"].as_str().unwrap().to_owned())
            .collect()
    }
}

#[test]
fn ships_on_one_machine_find_and_mail_each_other() {
    let machine = Machine::new();
    let liaison = machine.ship("liaison");
    let apps = machine.ship("apps");
    assert_eq!(machine.fleet(&liaison), ["apps", "liaison"]);
    // Each knows the other behind a local peer, both ways.
    let seen = machine.json(&liaison, "ship.show", json!({"name":"apps"}));
    assert_eq!(seen["location"]["peer"], "fleet-apps");
    let seen = machine.json(&apps, "ship.show", json!({"name":"liaison"}));
    assert_eq!(seen["location"]["peer"], "fleet-liaison");
    machine.json(
        &liaison,
        "mail.send",
        json!({"id":"hello","from":"liaison","to":"apps","kind":"request","body":"Ready to start?"}),
    );
    let inbox = machine.json(&apps, "mail.inbox", json!({"address":"apps"}));
    assert_eq!(inbox["messages"][0]["body"], "Ready to start?");
    let listed = machine.ok(&apps, &["ships"]);
    assert!(
        listed.contains("Fleet on this machine (2 ships)"),
        "{listed}"
    );
}

#[test]
fn a_ship_creates_a_linked_sibling_ship() {
    let machine = Machine::new();
    let liaison = machine.ship("liaison");
    let out = machine.ok(
        &liaison,
        &[
            "ship",
            "new",
            "infra",
            "--scope",
            "Deploys and build servers",
        ],
    );
    assert!(out.contains("The ship infra is ready"), "{out}");
    let infra = machine.ships.join("infra");
    assert!(infra.join(".privateer/privateer.sqlite").is_file());
    assert!(infra.join("AGENTS.md").is_file());
    let ship = machine.json(&infra, "ship.show", json!({"name":"infra"}));
    assert_eq!(ship["scope"], "Deploys and build servers");
    assert_eq!(
        machine.json(&liaison, "ship.show", json!({"name":"infra"}))["location"]["peer"],
        "fleet-infra"
    );
    let entry = machine.json(&liaison, "fleet.list", json!({}))["ships"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "infra")
        .cloned()
        .unwrap();
    assert_eq!(entry["created_by"], "liaison");
    // A folder that is not empty is never taken over.
    fs::create_dir_all(machine.ships.join("busy")).unwrap();
    fs::write(machine.ships.join("busy/notes.txt"), "mine").unwrap();
    assert!(
        !machine
            .run(&liaison, &["ship", "new", "busy"])
            .status
            .success()
    );
}

#[test]
fn an_agent_creates_ships_only_within_its_own_charter() {
    let machine = Machine::new();
    let liaison = machine.ship("liaison");
    machine.json(
        &liaison,
        "charter.put",
        json!({"id":"liaison","actions":["inspect","spawn","implement","test","message"]}),
    );
    let made = machine.as_agent(
        &liaison,
        "liaison",
        "ship.create",
        json!({"name":"research"}),
    );
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    let research = machine.ships.join("research");
    let charter = machine.json(&research, "charter.show", json!({"id":"research"}));
    assert_eq!(
        charter["actions"],
        json!(["inspect", "spawn", "implement", "test", "message"])
    );
    let wider = machine.as_agent(
        &liaison,
        "liaison",
        "ship.create",
        json!({"name":"release","actions":["implement","merge_green"]}),
    );
    assert!(!wider.status.success());
    assert!(String::from_utf8_lossy(&wider.stderr).contains("merge_green"));
}

#[test]
fn retiring_revokes_unlinks_and_keeps_the_folder() {
    let machine = Machine::new();
    let liaison = machine.ship("liaison");
    let made = machine.as_agent(
        &liaison,
        "liaison",
        "ship.create",
        json!({"name":"scratch"}),
    );
    assert!(made.status.success());
    let apps = machine.ship("apps");
    // An agent retires only ships it created.
    let refused = machine.as_agent(&liaison, "liaison", "ship.retire", json!({"name":"apps"}));
    assert!(!refused.status.success());
    let retired = machine.as_agent(
        &liaison,
        "liaison",
        "ship.retire",
        json!({"name":"scratch"}),
    );
    assert!(
        retired.status.success(),
        "{}",
        String::from_utf8_lossy(&retired.stderr)
    );
    let scratch = machine.ships.join("scratch");
    assert!(scratch.join("AGENTS.md").is_file());
    let charter = machine.json(&scratch, "charter.show", json!({"id":"scratch"}));
    assert_eq!(charter["active"], false);
    assert_eq!(machine.fleet(&liaison), ["apps", "liaison"]);
    assert!(
        machine
            .run(&apps, &["ship.show", "--json", r#"{"name":"scratch"}"#])
            .status
            .code()
            != Some(0)
    );
    // The name is free again.
    let again = machine.ships.join("scratch-two");
    fs::create_dir(&again).unwrap();
    machine.ok(&again, &["init", "--ship", "scratch"]);
    assert!(machine.fleet(&liaison).contains(&"scratch".to_owned()));
}

#[test]
fn a_deleted_ship_folder_frees_its_name_and_is_not_resurrected() {
    let machine = Machine::new();
    let old = machine.ship("infra");
    fs::remove_dir_all(&old).unwrap();
    let replacement = machine.ships.join("infra-new");
    fs::create_dir(&replacement).unwrap();
    machine.ok(&replacement, &["init", "--ship", "infra"]);
    machine.ship("apps");
    assert!(!old.exists(), "linking recreated a deleted ship folder");
    // A live ship's name stays taken.
    let clash = machine.ships.join("other");
    fs::create_dir(&clash).unwrap();
    let output = machine.run(&clash, &["init", "--ship", "apps"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already has a ship named apps"));
}
