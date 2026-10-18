mod support;

use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::Duration;

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
fn a_request_bound_to_a_stow_ends_when_the_stow_resumes() {
    let machine = Machine::new();
    let liaison = liaison(&machine);
    let apps = machine.ship("apps");
    machine.ok(&liaison, &["stow"]);
    let stow = machine.json(&liaison, "stow.status", json!({}))["stow"]
        .as_str()
        .unwrap()
        .to_owned();
    let send = |folder: &Path, id: &str, from: &str, binding: Option<Value>| {
        let mut payload = json!({});
        if let Some(depends_on) = binding {
            payload["depends_on"] = depends_on;
        }
        machine.json(
            folder,
            "mail.send",
            json!({"id": id, "from": from, "to": "captain", "kind": "request",
                "body": "Resume the stow?", "payload": payload}),
        )
    };
    let show = |folder: &Path, id: &str| machine.json(folder, "mail.show", json!({"id": id}));
    // The captain's mailbox is this home's, so the closing notice is where the
    // request was written.
    let notices = |folder: &Path| -> Vec<Value> {
        machine.json(folder, "mail.inbox", json!({"address": "captain"}))["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["kind"] == "notice")
            .cloned()
            .collect()
    };

    // One request waits for the stow, one is answered, one waits for nothing.
    let bound = send(&apps, "bound", "apps", Some(json!({"stow": stow})));
    assert_eq!(bound["state"], "open", "{bound}");
    assert_eq!(bound["payload"]["depends_on"]["stow"], json!(stow));
    let answered = send(&apps, "answered", "apps", Some(json!({"stow": stow})));
    assert_eq!(answered["state"], "open", "{answered}");
    machine.json(
        &apps,
        "mail.send",
        json!({"id": "answered-by", "from": "captain", "to": "apps", "kind": "reply",
            "correlation": "answered", "body": "Stow stands."}),
    );
    let unbound = send(&apps, "unbound", "apps", None);
    assert_eq!(unbound["payload"], json!({}), "{unbound}");

    // A binding is explicit: an unknown local stow, or one that has resumed,
    // is refused rather than guessed at.
    let unknown = machine.run(
        &apps,
        &[
            "mail.send",
            "--json",
            &json!({"id": "unknown-stow", "from": "apps", "to": "captain", "kind": "request",
            "body": "Which stow?", "payload": {"depends_on": {"stow": "stow-0000000"}}})
            .to_string(),
        ],
    );
    assert!(!unknown.status.success());
    assert!(
        stderr(&unknown).contains("no stow stow-0000000 on this machine"),
        "{}",
        stderr(&unknown)
    );
    let on_a_notice = machine.run(
        &apps,
        &[
            "mail.send",
            "--json",
            &json!({"id": "notice-binding", "from": "apps", "to": "captain", "kind": "notice",
            "body": "Nothing waits.", "payload": {"depends_on": {"stow": stow}}})
            .to_string(),
        ],
    );
    assert!(!on_a_notice.status.success());
    assert!(
        stderr(&on_a_notice).contains("depends_on applies to requests"),
        "{}",
        stderr(&on_a_notice)
    );

    let out = machine.ok(&liaison, &["stow", "resume"]);
    assert!(out.contains("ended 1 request(s)"), "{out}");

    // The bound request is withdrawn on the sender's behalf, with its reason.
    let ended = show(&apps, "bound");
    assert_eq!(ended["state"], "withdrawn", "{ended}");
    let reason = ended["end_reason"].as_str().unwrap();
    assert!(reason.contains(&format!("stow {stow} on ")), "{ended}");
    assert!(reason.contains(" resumed at "), "{ended}");
    let answered = show(&apps, "answered");
    assert_eq!(answered["state"], "answered", "{answered}");
    assert_eq!(show(&apps, "unbound")["state"], "open");
    let closing = notices(&apps);
    assert_eq!(closing.len(), 1, "{closing:?}");
    assert_eq!(closing[0]["correlation"], "bound");
    assert_eq!(closing[0]["body"], reason);
    assert_eq!(closing[0]["urgency"], "low");

    // A later stow changes nothing about the request that already ended.
    let resumed = show(&apps, "bound");
    machine.ok(&liaison, &["stow"]);
    machine.ok(&liaison, &["stow", "resume"]);
    assert_eq!(show(&apps, "bound")["ended_ms"], resumed["ended_ms"]);
    assert_eq!(
        notices(&apps).len(),
        1,
        "a second closing notice was written"
    );
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
    // The operator made it from the liaison's folder: the liaison lent its
    // settings, but the captain is its creator, so the liaison cannot retire it.
    assert_eq!(entry["created_by"], "captain");
    let retire = machine.as_agent(&liaison, "liaison", "ship.retire", json!({"name": "infra"}));
    assert!(!retire.status.success(), "the liaison did not create infra");
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
    assert_eq!(other["manifest"], "bundled:pi");
    assert_eq!(other["options"], json!({}));
}

#[test]
fn running_init_again_keeps_the_first_mates_harness_and_model() {
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

    // Without --harness, init keeps the ship's harness even when another one
    // would be detected first on PATH.
    let mut profile = machine.json(&apps, "ship.show", json!({"name":"apps"}))["first_mate"].take();
    let pi = json!({"provider":"openai","model":"chosen"});
    profile["options"] = pi.clone();
    machine.json(
        &apps,
        "ship.put",
        json!({"name":"apps","first_mate":profile}),
    );
    machine.ok(&apps, &["init"]);
    let kept = machine.json(&apps, "ship.show", json!({"name":"apps"}))["first_mate"].take();
    assert_eq!(kept["manifest"], "bundled:pi");
    assert_eq!(kept["options"], pi);
}

#[test]
fn a_scope_change_reaches_every_ship_and_only_its_ship_or_creator_makes_it() {
    let machine = Machine::new();
    let liaison = liaison(&machine);
    let apps = machine.ship("apps");
    let made = machine.as_agent(
        &liaison,
        "liaison",
        "ship.create",
        json!({"name":"tooling","scope":"Desktop tools and machine upkeep"}),
    );
    assert!(made.status.success(), "{}", stderr(&made));
    let tooling = machine.ships.join("tooling");

    let scope = "Desktop tools; machine upkeep belongs to apps";
    let changed = machine.as_agent(
        &liaison,
        "liaison",
        "ship.scope",
        json!({"name":"tooling","scope":scope}),
    );
    assert!(changed.status.success(), "{}", stderr(&changed));
    for home in [&tooling, &liaison, &apps] {
        let seen = machine.json(home, "ship.show", json!({"name":"tooling"}));
        assert_eq!(seen["scope"], scope, "as seen from {}", home.display());
    }
    // The ship's own record keeps everything else, and it learns the news.
    let own = machine.json(&tooling, "ship.show", json!({"name":"tooling"}));
    assert_eq!(own["location"], "here");
    assert!(own["first_mate"]["manifest"].is_string());
    let agents = fs::read_to_string(tooling.join("AGENTS.md")).unwrap();
    assert!(agents.contains(scope), "{agents}");
    let inbox = machine.json(&tooling, "mail.inbox", json!({"address":"tooling"}));
    assert!(
        inbox["messages"][0]["body"]
            .as_str()
            .unwrap()
            .contains(scope)
    );

    // The liaison did not create apps, so apps restates its own scope.
    let refused = machine.as_agent(
        &liaison,
        "liaison",
        "ship.scope",
        json!({"name":"apps","scope":"Anything"}),
    );
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("only ships it created"),
        "{}",
        stderr(&refused)
    );
    let restated = machine.as_agent(
        &apps,
        "apps",
        "ship.scope",
        json!({"name":"apps","scope":"Apps and machine upkeep"}),
    );
    assert!(restated.status.success(), "{}", stderr(&restated));
    let seen = machine.json(&liaison, "ship.show", json!({"name":"apps"}));
    assert_eq!(seen["scope"], "Apps and machine upkeep");
}

#[test]
fn a_stow_asks_every_ship_holds_new_jobs_but_not_wakes_and_resumes() {
    let machine = Machine::new();
    let liaison = liaison(&machine);
    let apps = machine.ship("apps");
    let job = |id: &str, wait: bool| {
        machine.run(
            &apps,
            &[
                "job.start",
                "--json",
                &json!({"task_id":"apps-duty","id":id,"command":["sleep","30"],"heavy":false,"wait_for_capacity":wait})
                    .to_string(),
            ],
        )
    };

    let out = machine.ok(&liaison, &["stow"]);
    assert!(out.starts_with("Stowing this machine ("), "{out}");
    assert!(out.contains("asked apps to stow"), "{out}");
    assert!(out.contains("asked liaison to stow"), "{out}");
    let inbox = machine.json(&apps, "mail.inbox", json!({"address":"apps"}));
    let request = &inbox["messages"][0];
    assert_eq!(request["from"], "captain");
    assert_eq!(request["kind"], "request");
    assert!(!machine.run(&liaison, &["stow"]).status.success());

    // New work waits; a job that may wait keeps its place for after.
    let held = job("held", false);
    assert!(!held.status.success());
    assert!(stderr(&held).contains("stowed"), "{}", stderr(&held));
    let queued = job("queued", true);
    assert!(queued.status.success(), "{}", stderr(&queued));
    let status = machine.json(&apps, "job.status", json!({"id":"queued"}));
    assert_eq!(status["status"], "queued", "{status}");

    // The stow request wakes the first mate: wakes are not held.
    thread::sleep(Duration::from_millis(5_500));
    machine.ok(&apps, &["serve", "--json", r#"{"once":true}"#]);
    let wakes = machine.json(&apps, "wake.list", json!({}));
    let wake = &wakes["wakes"][0];
    assert!(
        !wake["error"].as_str().unwrap_or("").contains("stowed"),
        "{wake}"
    );
    assert!(wake["job_id"].is_string(), "{wake}");

    // Not ready until each ship has replied.
    let status = machine.json(&liaison, "stow.status", json!({}));
    assert_eq!(status["held"], true);
    assert_eq!(status["ready"], false);
    for (folder, ship) in [(&apps, "apps"), (&liaison, "liaison")] {
        let id = format!("{}-{ship}", status["stow"].as_str().unwrap());
        machine.json(
            folder,
            "mail.send",
            json!({"from":ship,"to":"captain","kind":"reply","correlation":id,"body":"Stowed; nothing in flight."}),
        );
    }
    let status = machine.json(&liaison, "stow.status", json!({}));
    let apps_status = status["ships"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["ship"] == "apps")
        .unwrap()
        .clone();
    assert_eq!(apps_status["replied"], true);
    assert_eq!(apps_status["reply"], "Stowed; nothing in flight.");

    let out = machine.ok(&liaison, &["stow", "resume"]);
    assert!(out.contains("Resumed this machine ("), "{out}");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while machine.json(&apps, "job.status", json!({"id":"queued"}))["status"] == "queued" {
        assert!(
            std::time::Instant::now() < deadline,
            "the queued job never started"
        );
        thread::sleep(Duration::from_millis(100));
    }
    let started = job("after", false);
    assert!(started.status.success(), "{}", stderr(&started));
    for id in ["queued", "after"] {
        machine.json(&apps, "job.cancel", json!({"id":id}));
    }
}

/// A stow is one machine's hold, so every word about it names that machine. A
/// machine with no links says what it knows and nothing more: the notice must
/// not read as a claim about machines it cannot see.
#[test]
fn a_resume_notice_names_this_machine_and_claims_nothing_about_others() {
    let machine = Machine::new();
    let liaison = liaison(&machine);
    let apps = machine.ship("apps");

    let asked = machine.json(&liaison, "stow.status", json!({}));
    let this = asked["this_machine"].as_str().unwrap().to_owned();
    assert!(!this.is_empty(), "{asked}");

    machine.ok(&liaison, &["stow"]);
    let request = &machine.json(&apps, "mail.inbox", json!({"address":"apps"}))["messages"][0];
    let body = request["body"].as_str().unwrap();
    assert!(
        body.contains(&format!("stowing this machine ({this})")),
        "{body}"
    );
    assert!(!body.contains("the fleet"), "{body}");

    machine.ok(&liaison, &["stow", "resume"]);
    let inbox = machine.json(&apps, "mail.inbox", json!({"address":"apps"}));
    let notice = inbox["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["kind"] == "notice")
        .expect("a resume notice");
    let body = notice["body"].as_str().unwrap();
    assert!(
        body.contains(&format!("resumed this machine ({this})")),
        "{body}"
    );
    assert!(body.contains("new jobs start again here"), "{body}");
    // No machine is linked here, so no machine is named but this one.
    assert!(!body.contains("Linked machine"), "{body}");
    assert!(!body.contains("the fleet"), "{body}");
    // And the same claim in records, from this machine alone.
    let status = machine.json(&liaison, "stow.status", json!({}));
    assert_eq!(status["this_machine"], json!(this), "{status}");
    assert_eq!(status["machines"], json!([]), "{status}");
}

#[test]
fn waiting_requests_say_when_each_was_asked_and_how_old_it_is_now() {
    let machine = Machine::new();
    let liaison = liaison(&machine);
    let apps = machine.ship("apps");
    let before = now_ms();
    let asked = machine.json(
        &apps,
        "mail.send",
        json!({"from":"apps","to":"captain","kind":"request","body":"Is the Mac still stowed?"}),
    );
    // Another ship asks a moment later; the list stays oldest first.
    std::thread::sleep(Duration::from_millis(20));
    let later = machine.json(
        &liaison,
        "mail.send",
        json!({"from":"liaison","to":"captain","kind":"request","body":"Which project first?"}),
    );

    let out = machine
        .command(&liaison, &["captain.waiting", "--json", "{}"])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "liaison")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    // The list's own clock must be this test's wall clock: it was asked while
    // these requests were being written and read.
    let after = now_ms();
    let read: Value = serde_json::from_slice(&out.stdout).unwrap();
    let listed: Vec<&Value> = read["waiting"].as_array().unwrap().iter().collect();
    let ids: Vec<&str> = listed
        .iter()
        .map(|w| w["mail"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [asked["id"].as_str().unwrap(), later["id"].as_str().unwrap()]
    );

    let now = read["now_ms"]
        .as_i64()
        .expect("the list says which clock it used");
    assert!(
        (before..=after).contains(&now),
        "now_ms {now} is not this test's own wall clock {before}..{after}"
    );
    for item in &listed {
        // The age is derived from when the request was asked, not stored.
        assert_eq!(item["asked_ms"], item["mail"]["created_ms"], "{item}");
        let age = item["age_ms"]
            .as_i64()
            .expect("every waiting request has an age");
        assert!(age >= 0, "{item}");
        assert!(
            (now - item["asked_ms"].as_i64().unwrap() - age).abs() < 1_000,
            "{item} claims to be {age} ms old against now_ms {now}"
        );
        assert!(
            age < 3_600_000,
            "{item} is an hour old after being asked now"
        );
    }
    // Both requests were asked within this test, so the older one is not younger.
    assert!(
        listed[0]["age_ms"].as_i64().unwrap() >= listed[1]["age_ms"].as_i64().unwrap(),
        "{listed:?}"
    );

    // Answering one closes it, and what is left carries the remaining age.
    machine.json(
        &apps,
        "mail.send",
        json!({"from":"captain","to":"apps","kind":"reply","correlation":asked["id"],"body":"No, it resumed."}),
    );
    let out = machine
        .command(&liaison, &["captain.waiting", "--json", "{}"])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "liaison")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let read: Value = serde_json::from_slice(&out.stdout).unwrap();
    let listed: Vec<&Value> = read["waiting"].as_array().unwrap().iter().collect();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["mail"]["id"], later["id"]);
    assert!(listed[0]["age_ms"].as_i64().unwrap() > 0, "{listed:?}");
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[test]
fn hands_on_asks_are_listed_apart_with_their_step_and_malformed_marks_are_refused() {
    let machine = Machine::new();
    let liaison = liaison(&machine);
    let apps = machine.ship("apps");
    let ordinary = machine.json(
        &apps,
        "mail.send",
        json!({"from":"apps","to":"captain","kind":"request","body":"Which project first?"}),
    );
    std::thread::sleep(Duration::from_millis(20));
    // The report reads first, and the ask the captain must act on comes last.
    let step = "Run `sudo privateer-unlock apps` on the Mac";
    let hands = machine.json(
        &apps,
        "mail.send",
        json!({"from":"apps","to":"captain","kind":"request",
            "body":"The build finished. One thing needs you.",
            "payload":{"needs_hands":{"step":step}}}),
    );

    let read = |machine: &Machine| -> Value {
        let out = machine.as_agent(&liaison, "liaison", "captain.waiting", json!({}));
        assert!(out.status.success(), "{}", stderr(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let listed = read(&machine);
    let hands_on = listed["hands_on"].as_array().unwrap();
    assert_eq!(hands_on.len(), 1, "{listed}");
    assert_eq!(hands_on[0]["mail"]["id"], hands["id"], "{listed}");
    assert_eq!(hands_on[0]["step"], step, "{listed}");
    assert_eq!(hands_on[0]["ship"], "apps", "{listed}");
    // Both stay in the full list, and only the hands-on ask carries a step.
    let waiting = listed["waiting"].as_array().unwrap();
    assert_eq!(waiting.len(), 2, "{listed}");
    let plain = waiting
        .iter()
        .find(|w| w["mail"]["id"] == ordinary["id"])
        .expect("the ordinary request is still waiting");
    assert!(plain["step"].is_null(), "{plain}");
    assert!(plain["age_ms"].as_i64().is_some(), "{plain}");

    // A marker that is not a bounded, non-empty step is refused, and nothing is sent.
    let refused = [
        json!({"step":"   "}),
        json!({"step":"x".repeat(1_025)}),
        json!({}),
        json!("sudo privateer-unlock"),
    ];
    for marks in refused {
        let out = machine.run(
            &apps,
            &[
                "mail.send",
                "--json",
                &json!({"from":"apps","to":"captain","kind":"request",
                "body":"Unlock please.","payload":{"needs_hands":marks}})
                .to_string(),
            ],
        );
        assert!(!out.status.success(), "accepted {marks}");
    }
    // A hands-on mark belongs on a request to the captain, not on a ship's mail.
    let out = machine.run(
        &apps,
        &[
            "mail.send",
            "--json",
            &json!({"from":"apps","to":"liaison","kind":"request",
            "body":"Unlock please.","payload":{"needs_hands":{"step":step}}})
            .to_string(),
        ],
    );
    assert!(!out.status.success(), "a hands-on mark reached a ship");
    assert_eq!(read(&machine)["waiting"].as_array().unwrap().len(), 2);

    // Answering the hands-on ask clears it from the hands-on list.
    machine.json(
        &apps,
        "mail.send",
        json!({"from":"captain","to":"apps","kind":"reply","correlation":hands["id"],"body":"Done."}),
    );
    let listed = read(&machine);
    assert!(
        listed["hands_on"].as_array().unwrap().is_empty(),
        "{listed}"
    );
    assert_eq!(listed["waiting"].as_array().unwrap().len(), 1, "{listed}");
}

#[test]
fn a_full_waiting_list_still_lists_every_hands_on_ask_in_both_lists() {
    let machine = Machine::new();
    let liaison = liaison(&machine);
    let apps = machine.ship("apps");
    // One more open ask than the list shows, and the newest one needs hands.
    for n in 0..100 {
        machine.json(
            &apps,
            "mail.send",
            json!({"from":"apps","to":"captain","kind":"request","body":format!("Ordinary {n}")}),
        );
    }
    let hands = machine.json(
        &apps,
        "mail.send",
        json!({"from":"apps","to":"captain","kind":"request","body":"Unlock the Mac",
            "payload":{"needs_hands":{"step":"Run `sudo privateer-unlock apps`"}}}),
    );
    let out = machine.as_agent(&liaison, "liaison", "captain.waiting", json!({}));
    assert!(out.status.success(), "{}", stderr(&out));
    let read: Value = serde_json::from_slice(&out.stdout).unwrap();
    let waiting = read["waiting"].as_array().unwrap();
    let hands_on = read["hands_on"].as_array().unwrap();
    assert_eq!(waiting.len(), 100, "the list stays bounded");
    assert_eq!(hands_on.len(), 1, "{read}");
    assert_eq!(hands_on[0]["mail"]["id"], hands["id"], "{read}");
    assert!(
        waiting.iter().any(|w| w["mail"]["id"] == hands["id"]),
        "a hands-on ask is missing from the full list"
    );
}

#[test]
fn the_liaison_sees_what_waits_on_the_captain_and_relays_the_captains_answers() {
    let machine = Machine::new();
    let liaison = liaison(&machine);
    let apps = machine.ship("apps");
    let send = |folder: &Path, actor: &str, args: Value| -> Value {
        let out = machine.as_agent(folder, actor, "mail.send", args);
        assert!(out.status.success(), "{}", stderr(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let asked = send(
        &apps,
        "apps",
        json!({"from":"apps","to":"captain","kind":"request","body":"May I cancel the 258 obsolete checks?"}),
    );
    let stale = send(
        &liaison,
        "liaison",
        json!({"from":"liaison","to":"captain","kind":"request","body":"Testing is blocked on memory."}),
    );

    // The liaison sees the whole fleet's list; another ship does not.
    let out = machine.as_agent(&liaison, "liaison", "captain.waiting", json!({}));
    assert!(out.status.success(), "{}", stderr(&out));
    let waiting: Value = serde_json::from_slice(&out.stdout).unwrap();
    let ids: Vec<(&str, &str)> = waiting["waiting"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| {
            (
                w["ship"].as_str().unwrap(),
                w["mail"]["id"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        ids,
        [
            ("apps", asked["id"].as_str().unwrap()),
            ("liaison", stale["id"].as_str().unwrap())
        ]
    );
    let refused = machine.as_agent(&apps, "apps", "captain.waiting", json!({}));
    assert!(!refused.status.success());

    // The captain answers in conversation with the liaison.
    let said = machine.json(
        &liaison,
        "mail.send",
        json!({"from":"captain","to":"liaison","kind":"notice","body":"Yes, cancel the obsolete checks."}),
    );
    let relay = |source: &str| {
        machine.as_agent(
            &liaison,
            "liaison",
            "captain.relay",
            json!({"request":asked["id"],"source":source,"body":"Approved: cancel them."}),
        )
    };
    // Only the captain's own message can stand behind a relayed answer.
    let invented = relay(stale["id"].as_str().unwrap());
    assert!(!invented.status.success());
    assert!(
        stderr(&invented).contains("not the captain's message"),
        "{}",
        stderr(&invented)
    );
    let relayed = relay(said["id"].as_str().unwrap());
    assert!(relayed.status.success(), "{}", stderr(&relayed));
    let request = machine.json(&apps, "mail.show", json!({"id":asked["id"]}));
    assert_eq!(request["state"], "answered");
    let reply = machine.json(&apps, "mail.show", json!({"id":request["answered_by"]}));
    assert_eq!(reply["from"], "captain");
    assert_eq!(reply["body"], "Approved: cancel them.");
    assert_eq!(reply["payload"]["relayed_by"], "liaison");
    assert_eq!(
        reply["payload"]["captain_said"],
        "Yes, cancel the obsolete checks."
    );
    assert!(!relay(said["id"].as_str().unwrap()).status.success());

    // Only its sender withdraws a stale request, and it stops waiting.
    let withdraw = |folder: &Path, actor: &str, id: &Value| {
        machine.as_agent(
            folder,
            actor,
            "mail.withdraw",
            json!({"id":id,"reason":"memory is fixed"}),
        )
    };
    assert!(!withdraw(&apps, "apps", &stale["id"]).status.success());
    let out = withdraw(&liaison, "liaison", &stale["id"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let waiting = machine.json(&liaison, "captain.waiting", json!({}));
    assert_eq!(waiting["waiting"], json!([]));

    // A request to another ship is withdrawn in both homes.
    let question = send(
        &liaison,
        "liaison",
        json!({"from":"liaison","to":"apps","kind":"request","body":"Still need the region?"}),
    );
    let out = withdraw(&liaison, "liaison", &question["id"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["remote"], "withdrawn", "{result}");
    let theirs = machine.json(&apps, "mail.show", json!({"id":question["id"]}));
    assert_eq!(theirs["state"], "withdrawn");
    assert_eq!(theirs["ended_by"], result["ended_by"]);
    assert!(theirs["answered_by"].is_null());
    let again = withdraw(&liaison, "liaison", &question["id"]);
    assert!(again.status.success(), "{}", stderr(&again));
    let retried: Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(retried["remote"], "withdrawn");
    assert_eq!(retried["ended_by"], result["ended_by"]);

    // A cited captain message permits normal answers, not production grants
    // or consumption of the typed approval request. No actor/role clearing.
    machine.json(
        &apps,
        "task.create",
        json!({"id":"release","charter_id":"apps","owner":"apps","title":"Release"}),
    );
    let production = send(
        &apps,
        "apps",
        json!({"from":"apps","to":"captain","kind":"request","request_kind":"production_approval","task_id":"release","body":"Release exact build?","payload":{"artifact":"sha256:exact","environment":"production"}}),
    );
    let approval_source = machine.json(
        &liaison,
        "mail.send",
        json!({"from":"captain","to":"liaison","kind":"notice","body":"Yes, release that build."}),
    );
    let refused = machine.as_agent(&liaison,"liaison","captain.relay",json!({"request":production["id"],"source":approval_source["id"],"body":"Approved for production."}));
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("production approvals"),
        "{}",
        stderr(&refused)
    );
    let still_open = machine.json(&apps, "mail.show", json!({"id":production["id"]}));
    assert_eq!(still_open["state"], "open");
    assert!(still_open["answered_by"].is_null());
    let needs = machine.json(&apps, "runtime.needs", json!({}));
    assert_eq!(needs["items"][0]["kind"], "approval");
    assert_eq!(needs["items"][0]["source_id"], production["id"]);
    // Specific liaison desk permission does not grant readonly bridge access.
    assert!(
        !machine
            .as_agent(&liaison, "liaison", "runtime.needs", json!({}))
            .status
            .success()
    );
    let consume = machine.run(&apps,&["approval.consume","--json","{\"id\":\"no-relayed-grant\",\"task_id\":\"release\",\"artifact\":\"sha256:exact\",\"environment\":\"production\"}"]);
    assert!(!consume.status.success());
    let waiting = machine.json(&liaison, "captain.waiting", json!({}));
    assert_eq!(waiting["waiting"].as_array().unwrap().len(), 1);
    assert_eq!(waiting["waiting"][0]["mail"]["id"], production["id"]);
}

#[test]
fn a_standing_grant_lets_a_ship_act_on_another_until_paused_or_revoked() {
    let machine = Machine::new();
    let liaison = machine.ship("liaison");
    let apps = machine.ship("apps");
    let rescope = |scope: &str| {
        machine
            .as_agent(
                &liaison,
                "liaison",
                "ship.scope",
                json!({"name": "apps", "scope": scope}),
            )
            .status
            .success()
    };
    // The captain made apps, so the liaison cannot touch it, nor grant
    // itself the right.
    assert!(!rescope("Apps"));
    let own = machine.as_agent(
        &liaison,
        "liaison",
        "grant.put",
        json!({"grantee": "liaison", "actions": ["ship.scope"], "ships": ["apps"]}),
    );
    assert!(!own.status.success());

    let made = machine.ok(
        &liaison,
        &[
            "grant",
            "add",
            "liaison",
            "--actions",
            "ship.scope",
            "--ships",
            "apps",
            "--notify",
        ],
    );
    let id = serde_json::from_str::<Value>(&made).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(rescope("Apps and their releases"));
    assert_eq!(
        machine.json(&apps, "ship.show", json!({"name": "apps"}))["scope"],
        "Apps and their releases"
    );
    // Only the actions it names, and the captain hears of each use.
    let retire = machine.as_agent(&liaison, "liaison", "ship.retire", json!({"name": "apps"}));
    assert!(!retire.status.success());
    let told = machine.json(&liaison, "mail.inbox", json!({"address": "captain"}));
    assert!(
        told.to_string().contains(&format!("used your grant {id}")),
        "{told}"
    );
    // A ship sees the grants it holds.
    let held: Value = serde_json::from_slice(
        &machine
            .as_agent(&liaison, "liaison", "grant.list", json!({}))
            .stdout,
    )
    .unwrap();
    assert_eq!(held["grants"][0]["id"], id.as_str());

    machine.ok(&liaison, &["grant", "pause", &id]);
    assert!(!rescope("Paused"));
    machine.ok(&liaison, &["grant", "resume", &id]);
    assert!(rescope("Resumed"));
    machine.ok(&liaison, &["grant", "revoke", &id]);
    assert!(!rescope("Revoked"));
}

#[test]
fn a_typed_decision_reaches_the_liaison_with_its_options_whole() {
    let machine = Machine::new();
    let liaison = liaison(&machine);
    let apps = machine.ship("apps");
    let decision = machine.json(
        &apps,
        "mail.send",
        json!({"from":"apps","to":"captain","kind":"decision","body":"Which path?",
        "payload":{"decision":{"question":"Which path?","options":[
            {"id":"a","label":"Path A","what_happens":"Work starts now."},
            {"id":"b","label":"Path B","what_happens":"Nothing starts.","recommended":true}
        ]}}}),
    );
    assert_eq!(decision["payload"]["choices"], json!(["Path A", "Path B"]));
    let out = machine.as_agent(&liaison, "liaison", "captain.waiting", json!({}));
    assert!(out.status.success(), "{}", stderr(&out));
    let read: Value = serde_json::from_slice(&out.stdout).unwrap();
    let item = read["waiting"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["mail"]["id"] == decision["id"])
        .expect("the decision is waiting");
    assert_eq!(
        item["mail"]["payload"]["decision"]["question"],
        "Which path?"
    );
    assert_eq!(
        item["mail"]["payload"]["decision"]["options"][1]["recommended"],
        true
    );
    assert_eq!(
        item["mail"]["payload"]["choices"],
        json!(["Path A", "Path B"])
    );
}
