mod support;

use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");

struct Folder {
    _temp: tempfile::TempDir,
    root: PathBuf,
    ship: PathBuf,
    path: String,
}

impl Folder {
    /// A fresh folder, with a fake `codex` as the only harness on PATH.
    fn new(name: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let ship = root.join(name);
        fs::create_dir(&ship).unwrap();
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("codex"), "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:/usr/bin:/bin", bin.display());
        Self {
            _temp: temp,
            root,
            ship,
            path,
        }
    }
    fn run_in(&self, dir: &Path, args: &[&str]) -> Output {
        support::command(BIN)
            .args(args)
            .current_dir(dir)
            .env("PATH", &self.path)
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env("PRIVATEER_FLEET", self.root.join("fleet.sqlite"))
            .env_remove("PRIVATEER_HOME")
            .output()
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> String {
        let output = self.run_in(&self.ship, args);
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
}

#[test]
fn init_turns_a_folder_into_a_ship_any_harness_can_crew() {
    let folder = Folder::new("Infra Ship");
    let out = folder.ok(&["init", "--scope", "Deploys and build servers"]);
    assert!(out.contains("is now the ship infra-ship"), "{out}");
    assert!(out.contains("first mate harness: codex"));
    assert!(folder.ship.join(".privateer/privateer.sqlite").is_file());
    assert!(folder.ship.join("projects").is_dir());
    let agents = fs::read_to_string(folder.ship.join("AGENTS.md")).unwrap();
    assert!(agents.starts_with("<!-- privateer:generated -->"));
    assert!(agents.contains("first mate of the Privateer ship **infra-ship**"));
    assert!(agents.contains("Its scope: Deploys and build servers."));
    assert!(agents.contains(&format!("{BIN} --home ")));
    assert_eq!(
        fs::read_to_string(folder.ship.join("CLAUDE.md")).unwrap(),
        "@AGENTS.md\n"
    );
    let settings: Value =
        serde_json::from_slice(&fs::read(folder.ship.join(".claude/settings.json")).unwrap())
            .unwrap();
    assert_eq!(
        settings["permissions"]["allow"][0],
        format!("Bash({BIN}:*)")
    );
    let ignore = fs::read_to_string(folder.ship.join(".gitignore")).unwrap();
    assert!(ignore.contains(".privateer/") && ignore.contains("projects/"));

    let ship = folder.json("ship.show", json!({"name":"infra-ship"}));
    assert_eq!(ship["scope"], "Deploys and build servers");
    assert!(
        ship["first_mate"]["manifest"]
            .as_str()
            .unwrap()
            .ends_with("plugins/codex/manifest.json")
    );
    assert_eq!(ship["first_mate"]["options"]["sandbox"], "workspace-write");
    assert_eq!(ship["first_mate"]["duty_task"], "infra-ship-duty");
    let charter = folder.json("charter.show", json!({"id":"infra-ship"}));
    assert_eq!(charter["repo"], folder.ship.display().to_string());
    let task = folder.json("task.show", json!({"id":"infra-ship-duty"}));
    assert_eq!(task["owner"], "infra-ship");
}

#[test]
fn init_again_keeps_the_captains_edits() {
    let folder = Folder::new("infra");
    folder.ok(&["init", "--scope", "Deploys"]);
    fs::write(folder.ship.join("AGENTS.md"), "# My own instructions\n").unwrap();
    folder.json(
        "charter.put",
        json!({"id":"infra","objective":"Edited by the captain"}),
    );
    let out = folder.ok(&["init", "--harness", "claude"]);
    assert!(out.contains("was left alone"), "{out}");
    assert_eq!(
        fs::read_to_string(folder.ship.join("AGENTS.md")).unwrap(),
        "# My own instructions\n"
    );
    let ship = folder.json("ship.show", json!({"name":"infra"}));
    assert_eq!(ship["scope"], "Deploys");
    assert!(
        ship["first_mate"]["manifest"]
            .as_str()
            .unwrap()
            .ends_with("plugins/claude/manifest.json")
    );
    assert_eq!(
        ship["first_mate"]["options"]["permission_mode"],
        "acceptEdits"
    );
    let charter = folder.json("charter.show", json!({"id":"infra"}));
    assert_eq!(charter["objective"], "Edited by the captain");
    let settings: Value =
        serde_json::from_slice(&fs::read(folder.ship.join(".claude/settings.json")).unwrap())
            .unwrap();
    assert_eq!(
        settings["permissions"]["allow"].as_array().unwrap().len(),
        1
    );
}

#[test]
fn commands_find_the_ship_from_any_folder_inside_it() {
    let folder = Folder::new("infra");
    let outside = folder.run_in(&folder.root, &["status"]);
    assert!(!outside.status.success());
    assert!(String::from_utf8_lossy(&outside.stderr).contains("privateer init"));
    folder.ok(&["init"]);
    let nested = folder.ship.join("projects/app/src");
    fs::create_dir_all(&nested).unwrap();
    let status = folder.run_in(&nested, &["status"]);
    assert!(status.status.success());
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(text.contains("Bearings for infra"), "{text}");
    // JSON commands find the home the same way.
    let json = folder.run_in(&nested, &["status", "--json", "{}"]);
    let status: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(status["open_tasks"], 1);
    let ships = folder.ok(&["ships"]);
    assert!(ships.contains("infra (here, first mate: codex)"), "{ships}");
}

#[test]
fn the_captain_asks_reads_and_replies_in_plain_commands() {
    let folder = Folder::new("infra");
    folder.ok(&["init"]);
    let sent = folder.ok(&["ask", "infra", "Is", "staging", "healthy?"]);
    assert!(sent.contains("Sent request"), "{sent}");
    let mail = folder.json("mail.inbox", json!({"address":"infra"}));
    assert_eq!(mail["messages"][0]["body"], "Is staging healthy?");
    assert_eq!(mail["messages"][0]["from"], "captain");
    let request = mail["messages"][0]["id"].as_str().unwrap().to_owned();
    // The first mate answers, and asks something of its own.
    folder.json(
        "mail.send",
        json!({"from":"infra","to":"captain","kind":"reply","correlation":request,"body":"All green."}),
    );
    folder.json(
        "mail.send",
        json!({"id":"may-i","from":"infra","to":"captain","kind":"request","body":"May I restart the runner?"}),
    );
    let inbox = folder.ok(&["inbox"]);
    assert!(
        inbox.contains("All green.") && inbox.contains("May I restart the runner?"),
        "{inbox}"
    );
    assert!(folder.ok(&["inbox"]).contains("No new mail"));
    assert!(folder.ok(&["inbox", "--all"]).contains("All green."));
    folder.ok(&["reply", "may-i", "Yes,", "go", "ahead."]);
    let answered = folder.json("mail.show", json!({"id":"may-i"}));
    assert_eq!(answered["state"], "answered");
    folder.ok(&["tell", "infra", "Deploy window moved to Friday"]);
    let notices = folder.json("mail.inbox", json!({"address":"infra"}));
    let kinds: Vec<_> = notices["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["kind"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(kinds, ["request", "reply", "notice"]);
    assert!(folder.ok(&["wakes"]).contains("No wakes yet"));
}

#[test]
fn project_add_clones_a_repository_into_the_ship() {
    let folder = Folder::new("infra");
    folder.ok(&["init"]);
    let source = folder.root.join("source");
    fs::create_dir(&source).unwrap();
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&source)
                .env("PATH", &folder.path)
                .output()
                .unwrap()
                .status
                .success()
        )
    };
    git(&["init", "-q"]);
    fs::write(source.join("README.md"), "hello").unwrap();
    git(&["add", "."]);
    git(&[
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "commit",
        "-qm",
        "init",
    ]);
    let out = folder.ok(&["project", "add", source.to_str().unwrap(), "app"]);
    assert!(out.contains("projects/app"), "{out}");
    assert_eq!(
        fs::read_to_string(folder.ship.join("projects/app/README.md")).unwrap(),
        "hello"
    );
    let again = folder.run_in(
        &folder.ship,
        &["project", "add", source.to_str().unwrap(), "app"],
    );
    assert!(!again.status.success());
}

#[test]
fn init_needs_a_harness_or_says_how_to_choose_one() {
    let folder = Folder::new("infra");
    fs::remove_file(folder.root.join("bin/codex")).unwrap();
    let output = folder.run_in(&folder.ship, &["init"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--harness"));
    let output = folder.run_in(&folder.ship, &["init", "--harness", "no-such-harness"]);
    assert!(!output.status.success());
}

#[test]
fn liaison_preset_is_read_message_delegate_and_keeps_captain_edits() {
    let folder = Folder::new("contact");
    folder.ok(&["init", "--liaison", "--harness", "codex"]);
    let charter = folder.json("charter.show", json!({"id":"contact"}));
    assert_eq!(charter["actions"], json!(["inspect", "spawn", "message"]));
    let emitted = fs::read_to_string(folder.ship.join("AGENTS.md")).unwrap();
    // AGENTS.md is the generated public first-mate interface, not a proxy
    // for successful model onboarding; the PTY separately proves creation.
    assert!(emitted.contains("Onboarding the captain"));
    assert!(emitted.contains("never widen your own charter"));
    folder.ok(&["init", "--harness", "codex"]);
    assert!(
        fs::read_to_string(folder.ship.join("AGENTS.md"))
            .unwrap()
            .contains("Onboarding the captain")
    );
    folder.json(
        "charter.put",
        json!({"id":"contact","actions":["inspect","message"]}),
    );
    folder.ok(&["init", "--liaison"]);
    assert_eq!(
        folder.json("charter.show", json!({"id":"contact"}))["actions"],
        json!(["inspect", "message"])
    );
    let config: Value =
        serde_json::from_slice(&fs::read(folder.ship.join(".privateer/config.json")).unwrap())
            .unwrap();
    assert_eq!(config["preset"], "liaison");
}

#[test]
fn generated_first_mate_commands_work_from_a_folder_with_shell_metacharacters() {
    let folder = Folder::new("contact's $(touch escaped)");
    folder.ok(&[
        "init",
        "--ship",
        "contact",
        "--liaison",
        "--harness",
        "codex",
    ]);
    let text = fs::read_to_string(folder.ship.join("AGENTS.md")).unwrap();
    let prefix = text
        .split_once("Run Privateer as `")
        .unwrap()
        .1
        .split_once(" COMMAND")
        .unwrap()
        .0;
    // Execute the generated public shell interface, rather than merely asserting
    // that its source contains quoting tokens.
    let output = support::command("sh")
        .arg("-c")
        .arg(format!(
            "{prefix} ship.bearings --json '{{\"ship\":\"contact\"}}'"
        ))
        .current_dir(&folder.ship)
        .env("HOME", &folder.root)
        .env("XDG_CONFIG_HOME", folder.root.join("config"))
        .env("PRIVATEER_FLEET", folder.root.join("fleet.sqlite"))
        .env("PRIVATEER_RESOURCE_HOME", folder.root.join("ledger"))
        .env("PRIVATEER_SERVICE", "none")
        .env_remove("PRIVATEER_ROLE")
        .env_remove("PRIVATEER_ACTOR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["ship"],
        "contact"
    );
    assert!(!folder.ship.join("escaped").exists());
}
