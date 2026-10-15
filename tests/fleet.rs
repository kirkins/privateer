mod support;

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
        let mut command = support::command(BIN);
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
fn an_agent_needs_an_active_spawn_charter_before_creating_a_ship() {
    let machine = Machine::new();
    let liaison = machine.ship("liaison");
    for (child, change, reason) in [
        (
            "no-spawn",
            json!({"id":"liaison","actions":["inspect","implement","test","message"]}),
            "spawn",
        ),
        (
            "revoked",
            json!({"id":"liaison","actions":["inspect","spawn","implement"],"active":false}),
            "revoked",
        ),
    ] {
        machine.json(&liaison, "charter.put", change);
        let output = machine.as_agent(&liaison, "liaison", "ship.create", json!({"name":child}));
        assert!(
            !output.status.success(),
            "created {child} without authority"
        );
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(reason), "{error}");
        assert!(!machine.ships.join(child).exists());
        assert_eq!(machine.fleet(&liaison), ["liaison"]);
    }
}

#[test]
fn ship_creation_waits_for_committed_revocation_before_any_child_side_effect() {
    use privateer::{
        dispatch,
        store::{Caller, Store},
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    static WAITING: AtomicBool = AtomicBool::new(false);
    let machine = Machine::new();
    let liaison = machine.ship("liaison");
    let store = Store::open(&liaison.join(".privateer")).unwrap();
    // Open both connections before taking the lock, so store initialization
    // cannot accidentally serialize an otherwise stale authority read.
    let mut creating = Store::open(store.home()).unwrap();
    creating.set_caller(Caller::Agent {
        actor: "liaison".into(),
    });
    creating
        .connection()
        .busy_handler(Some(|attempt| {
            WAITING.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(1));
            attempt < 5_000
        }))
        .unwrap();
    let tx = store.write_transaction().unwrap();
    let mut charter = store.charter("liaison").unwrap();
    charter["active"] = json!(false);
    tx.execute(
        "UPDATE charters SET data=?1 WHERE id='liaison'",
        [charter.to_string()],
    )
    .unwrap();
    let folder = machine.ships.join("too-late");
    let target = folder.clone();
    let create = std::thread::spawn(move || {
        // The unavailable adapter also keeps a regressed implementation from
        // publishing anything outside this isolated folder before it fails.
        dispatch(
            &mut creating,
            "ship.create",
            json!({
                "name":"too-late", "folder":target, "harness":"privateer-test-unavailable"
            }),
        )
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !WAITING.load(Ordering::SeqCst) && !create.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    tx.commit().unwrap();
    let error = create.join().unwrap().unwrap_err();
    assert!(
        WAITING.load(Ordering::SeqCst),
        "creation never waited for the authority lock: {error}"
    );
    assert!(error.to_string().contains("revoked"), "{error}");
    assert!(!folder.exists());
    assert_eq!(machine.fleet(&liaison), ["liaison"]);
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

const DEFAULT_ACTIONS: [&str; 9] = [
    "inspect",
    "spawn",
    "implement",
    "test",
    "publish_pr",
    "merge_green",
    "message",
    "recovery",
    "cleanup",
];

fn liaison(machine: &Machine) -> PathBuf {
    let folder = machine.ships.join("liaison");
    fs::create_dir_all(&folder).unwrap();
    machine.ok(&folder, &["init", "--liaison"]);
    folder
}

/// A one-commit repository to clone.
fn repository(machine: &Machine) -> PathBuf {
    let repo = machine.root.join("source.git");
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .current_dir(machine.root.as_path())
            .env("HOME", &machine.root)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "--quiet", "source.git"]);
    fs::write(repo.join("README"), "hello\n").unwrap();
    git(&["-C", "source.git", "add", "README"]);
    git(&["-C", "source.git", "commit", "--quiet", "-m", "first"]);
    repo
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_liaison_staffs_and_provisions_ships_for_the_captain() {
    let machine = Machine::new();
    let liaison = liaison(&machine);
    let apps = machine.ship("apps");
    let repo = repository(&machine);
    let made = machine.as_agent(
        &liaison,
        "liaison",
        "ship.create",
        json!({"name":"tooling"}),
    );
    assert!(made.status.success(), "{}", stderr(&made));
    let tooling = machine.ships.join("tooling");
    // The captain's grant to the liaison passes to the ship it creates:
    // working authority, without production deployment.
    let charter = machine.json(&tooling, "charter.show", json!({"id":"tooling"}));
    assert_eq!(charter["actions"], json!(DEFAULT_ACTIONS));

    let url = repo.to_str().unwrap();
    let added = machine.as_agent(
        &liaison,
        "liaison",
        "project.add",
        json!({"url":url,"target":"tooling"}),
    );
    assert!(added.status.success(), "{}", stderr(&added));
    let added: Value = serde_json::from_slice(&added.stdout).unwrap();
    assert_eq!(added["ship"], "tooling");
    assert_eq!(added["project"], "source");
    assert_eq!(added["by"], "liaison");
    let projects = tooling.join("projects");
    assert_eq!(
        fs::read_to_string(projects.join("source/README")).unwrap(),
        "hello\n"
    );
    let entries: Vec<_> = fs::read_dir(&projects)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(entries, ["source"], "no staging clone is left behind");

    // Its own folder, too, under another name.
    let own = machine.as_agent(
        &liaison,
        "liaison",
        "project.add",
        json!({"url":url,"name":"notes"}),
    );
    assert!(own.status.success(), "{}", stderr(&own));
    assert!(liaison.join("projects/notes/README").is_file());

    for (args, reason) in [
        // A ship the liaison did not create is not its to provision.
        (json!({"url":url,"target":"apps"}), "only ships it created"),
        (json!({"url":url,"target":"tooling"}), "already exists"),
        (
            json!({"url":"ext::sh -c true","target":"tooling","name":"x"}),
            "run commands",
        ),
        (
            json!({"url":url,"target":"tooling","name":".."}),
            "cannot start with .",
        ),
    ] {
        let refused = machine.as_agent(&liaison, "liaison", "project.add", args);
        assert!(!refused.status.success());
        assert!(stderr(&refused).contains(reason), "{}", stderr(&refused));
    }
    assert!(!apps.join("projects/source").exists());

    // The captain's CLI adds to any ship in the fleet.
    let out = machine.ok(&liaison, &["project", "add", url, "--ship", "apps"]);
    assert!(out.contains("into apps's projects/source"), "{out}");
    assert!(apps.join("projects/source/README").is_file());
}

#[test]
fn ship_actions_change_only_created_ships_within_the_granters_actions() {
    let machine = Machine::new();
    let liaison = liaison(&machine);
    machine.ship("apps");
    machine.json(
        &liaison,
        "charter.put",
        json!({"id":"liaison","actions":["inspect","spawn","implement","message"]}),
    );
    let made = machine.as_agent(
        &liaison,
        "liaison",
        "ship.create",
        json!({"name":"tooling","actions":["inspect"]}),
    );
    assert!(made.status.success(), "{}", stderr(&made));
    let tooling = machine.ships.join("tooling");

    let granted = machine.as_agent(
        &liaison,
        "liaison",
        "ship.actions",
        json!({"name":"tooling","actions":["inspect","implement"]}),
    );
    assert!(granted.status.success(), "{}", stderr(&granted));
    let charter = machine.json(&tooling, "charter.show", json!({"id":"tooling"}));
    assert_eq!(charter["actions"], json!(["inspect", "implement"]));
    assert_eq!(charter["active"], true);

    for (name, actions, reason) in [
        ("tooling", json!(["inspect", "merge_green"]), "merge_green"),
        ("tooling", json!(["deploy_production"]), "deploy_production"),
        ("apps", json!(["inspect"]), "only ships it created"),
    ] {
        let refused = machine.as_agent(
            &liaison,
            "liaison",
            "ship.actions",
            json!({"name":name,"actions":actions}),
        );
        assert!(!refused.status.success());
        assert!(stderr(&refused).contains(reason), "{}", stderr(&refused));
    }
    // The ship cannot widen itself through its creator's command.
    let itself = machine.as_agent(
        &tooling,
        "tooling",
        "ship.actions",
        json!({"name":"tooling","actions":["inspect","implement","test"]}),
    );
    assert!(!itself.status.success());

    machine.json(
        &liaison,
        "charter.put",
        json!({"id":"liaison","active":false}),
    );
    let revoked = machine.as_agent(
        &liaison,
        "liaison",
        "ship.actions",
        json!({"name":"tooling","actions":["inspect"]}),
    );
    assert!(!revoked.status.success());
    assert!(stderr(&revoked).contains("revoked"), "{}", stderr(&revoked));
    let charter = machine.json(&tooling, "charter.show", json!({"id":"tooling"}));
    assert_eq!(charter["actions"], json!(["inspect", "implement"]));
}

#[test]
fn reinit_upgrades_only_an_unedited_contact_only_liaison() {
    use privateer::store::Store;
    let machine = Machine::new();
    let liaison = liaison(&machine);
    // A liaison made before the preset acted for the captain.
    let store = Store::open(&liaison.join(".privateer")).unwrap();
    let mut charter = store.charter("liaison").unwrap();
    assert_eq!(charter["revision"], 1);
    charter["actions"] = json!(["inspect", "spawn", "message"]);
    store
        .connection()
        .execute(
            "UPDATE charters SET data=?1 WHERE id='liaison'",
            [charter.to_string()],
        )
        .unwrap();
    drop(store);
    machine.ok(&liaison, &["init"]);
    let upgraded = machine.json(&liaison, "charter.show", json!({"id":"liaison"}));
    assert_eq!(upgraded["actions"], json!(DEFAULT_ACTIONS));

    // The captain's own choice of the same actions is kept.
    machine.json(
        &liaison,
        "charter.put",
        json!({"id":"liaison","actions":["inspect","spawn","message"]}),
    );
    machine.ok(&liaison, &["init"]);
    let kept = machine.json(&liaison, "charter.show", json!({"id":"liaison"}));
    assert_eq!(kept["actions"], json!(["inspect", "spawn", "message"]));
}

#[test]
fn a_created_ship_keeps_its_creators_model_route_on_the_same_harness() {
    let machine = Machine::new();
    let liaison = liaison(&machine);
    let mut profile =
        machine.json(&liaison, "ship.show", json!({"name":"liaison"}))["first_mate"].take();
    let options = json!({"sandbox":"workspace-write","provider":"openai","model":"gpt-test"});
    profile["options"] = options.clone();
    machine.json(
        &liaison,
        "ship.put",
        json!({"name":"liaison","first_mate":profile}),
    );
    let first_mate = |name: &str| {
        machine.json(&machine.ships.join(name), "ship.show", json!({"name":name}))["first_mate"]
            .take()
    };

    let same = machine.as_agent(
        &liaison,
        "liaison",
        "ship.create",
        json!({"name":"tooling"}),
    );
    assert!(same.status.success(), "{}", stderr(&same));
    assert_eq!(first_mate("tooling")["options"], options);

    let named = machine.as_agent(
        &liaison,
        "liaison",
        "ship.create",
        json!({"name":"named","harness":"codex"}),
    );
    assert!(named.status.success(), "{}", stderr(&named));
    assert_eq!(first_mate("named")["options"], options);

    // Another harness cannot use the creator's options; it gets its defaults.
    let other = machine.as_agent(
        &liaison,
        "liaison",
        "ship.create",
        json!({"name":"other","harness":"pi"}),
    );
    assert!(other.status.success(), "{}", stderr(&other));
    let other = first_mate("other");
    assert!(
        other["manifest"]
            .as_str()
            .unwrap()
            .ends_with("pi/manifest.json")
    );
    assert_eq!(other["options"], json!({}));
}

#[test]
fn running_init_again_keeps_the_first_mates_model_for_the_same_harness() {
    let machine = Machine::new();
    let apps = machine.ship("apps");
    let mut profile = machine.json(&apps, "ship.show", json!({"name":"apps"}))["first_mate"].take();
    let chosen = json!({"sandbox":"workspace-write","provider":"openai","model":"chosen"});
    profile["options"] = chosen.clone();
    machine.json(
        &apps,
        "ship.put",
        json!({"name":"apps","first_mate":profile}),
    );
    let options = || {
        machine.json(&apps, "ship.show", json!({"name":"apps"}))["first_mate"]["options"].clone()
    };

    machine.ok(&apps, &["init"]);
    assert_eq!(options(), chosen);
    machine.ok(&apps, &["init", "--harness", "pi"]);
    assert_eq!(options(), json!({}));
}
