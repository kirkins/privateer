mod support;

use privateer::{
    dispatch,
    releases::{BINARIES, Release, ShipOutcome, UpgradeRecord},
    store::Store,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use support::{BIN, Fleet, now_settings};

const OLD: &str = "1111111111111111111111111111111111111111";
const NEW: &str = "2222222222222222222222222222222222222222";

fn release(data: &Path, commit: &str) -> Release {
    let release = Release::new(data, commit).unwrap();
    fs::create_dir_all(release.root.join("bin")).unwrap();
    fs::copy(BIN, release.binary("privateer").unwrap()).unwrap();
    let plugins = release.plugins();
    fs::create_dir_all(plugins.join("pi")).unwrap();
    fs::write(plugins.join("native.py"), "# bundle marker\n").unwrap();
    fs::write(plugins.join("pi/adapter.py"), format!(r#"import json,sys
r = json.loads(sys.stdin.readline())
print(json.dumps({{"version": 1, "id": r["id"], "ok": True, "result": {{"argv": ["echo", "{commit}"], "cwd": r["context"]["repo"], "env_names": []}}}}))
"#)).unwrap();
    fs::write(
        plugins.join("pi/manifest.json"),
        json!({"schema_version":1,"id":"pi","executable":["python3","adapter.py"]}).to_string(),
    )
    .unwrap();
    release
}

fn select(data: &Path, release: &Release) {
    let next = data.join("next");
    symlink(&release.root, &next).unwrap();
    fs::rename(next, data.join("current")).unwrap();
}

struct Rpc {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Rpc {
    fn new(mut command: Command) -> Self {
        let mut child = command
            .arg("rpc")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
        }
    }
    fn call(&mut self, command: &str, args: Value) -> Value {
        writeln!(
            self.input,
            "{}",
            json!({"id":"test","command":command,"args":args})
        )
        .unwrap();
        self.input.flush().unwrap();
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["ok"], true, "{response}");
        response["result"].clone()
    }
}
impl Drop for Rpc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn command(fleet: &Fleet, binary: &Path) -> Command {
    let mut command = fleet.command();
    // Keep all of Fleet's isolation settings, replacing only the executable.
    let mut installed = support::command(binary);
    installed
        .args(command.get_args())
        .envs(command.get_envs().filter_map(|(k, v)| v.map(|v| (k, v))));
    for (k, v) in command.get_envs() {
        if v.is_none() {
            installed.env_remove(k);
        }
    }
    installed.env("XDG_DATA_HOME", fleet.root.join("data"));
    // A hostile/stale development override must not redirect a release image.
    installed.env("PRIVATEER_PLUGINS", fleet.root.join("wrong-plugins"));
    command = installed;
    command
}

#[test]
fn running_release_keeps_its_plugins_and_persisted_commands_follow_current() {
    let fleet = Fleet::new(now_settings());
    let data = fleet.root.join("data/privateer");
    let old = release(&data, OLD);
    let new = release(&data, NEW);
    select(&data, &old);
    let stable = old.entrypoint("privateer").unwrap();
    let mut running = Rpc::new(command(&fleet, &stable));
    let args = json!({"task_id":"infra-duty","manifest":"bundled:pi"});
    let before = running.call("plugin.command", args.clone());
    assert_eq!(before["argv"][1], OLD);
    select(&data, &new);
    let after = running.call("plugin.command", args.clone());
    assert_eq!(after["argv"][1], OLD);
    let brief = running.call("agent.brief", json!({"task_id":"infra-duty"}));
    assert_eq!(brief["context"]["control"]["cli_argv"][0], json!(stable));
    let status = running.call("upgrade.status", json!({}));
    assert_eq!(status["running_commit"], OLD);
    assert_eq!(status["current_commit"], NEW);
    let mut restarted = Rpc::new(command(&fleet, &stable));
    assert_eq!(restarted.call("plugin.command", args)["argv"][1], NEW);
    // A damaged immutable bundle fails closed instead of finding the checkout
    // (or the deliberately wrong PRIVATEER_PLUGINS override).
    fs::remove_file(old.plugins().join("native.py")).unwrap();
    writeln!(running.input, "{}", json!({"id":"broken","command":"plugin.command","args":{"task_id":"infra-duty","manifest":"bundled:pi"}})).unwrap();
    running.input.flush().unwrap();
    let mut line = String::new();
    running.output.read_line(&mut line).unwrap();
    let failed: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(failed["ok"], false);
    assert!(
        failed["error"]
            .as_str()
            .unwrap()
            .contains("missing its bundled plugins")
    );
}

#[test]
fn old_dispatcher_launches_its_own_adapter_after_current_moves() {
    use std::time::{Duration, Instant};
    struct Server(Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut fleet = Fleet::new(now_settings());
    dispatch(
        &mut fleet.store,
        "ship.put",
        json!({"name":"infra", "first_mate":{
            "manifest":"bundled:pi", "duty_task":"infra-duty", "batch_secs":0, "max_delay_secs":0
        }}),
    )
    .unwrap();
    let data = fleet.root.join("data/privateer");
    let old = release(&data, OLD);
    let new = release(&data, NEW);
    select(&data, &old);
    let mut server = Server(
        command(&fleet, &old.entrypoint("privateer").unwrap())
            .args(["serve", "--json", "{\"interval_secs\":1}"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while !privateer::service::running(&fleet.home).unwrap() {
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "dispatcher exited before readiness"
        );
        assert!(Instant::now() < deadline, "dispatcher not ready");
        std::thread::sleep(Duration::from_millis(50));
    }
    select(&data, &new);
    fleet.send(json!({"from":"apps","to":"infra","kind":"notice","body":"test old release"}));
    loop {
        assert!(
            Instant::now() < deadline,
            "dispatcher did not complete a wake"
        );
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "old dispatcher exited"
        );
        if let Some(wake) = fleet.wakes().iter().find(|w| w["status"] == "succeeded") {
            let job =
                dispatch(&mut fleet.store, "job.status", json!({"id":wake["job_id"]})).unwrap();
            let log = fleet
                .home
                .join("jobs")
                .join(job["id"].as_str().unwrap())
                .join(job["incarnation"].as_str().unwrap())
                .join("stdout.log");
            assert_eq!(fs::read_to_string(log).unwrap().trim(), OLD);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn release_layout_rejects_refs_and_non_release_executables() {
    let root = Path::new("/test/privateer");
    for commit in [
        "main",
        "../escape",
        "abc",
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    ] {
        assert!(Release::new(root, commit).is_err());
    }
    assert!(Release::new(Path::new("relative"), OLD).is_err());
    let release = Release::new(root, OLD).unwrap();
    assert_eq!(
        Release::from_executable(&release.binary("privateer").unwrap()),
        Some(release.clone())
    );
    assert_eq!(
        Release::from_executable(&PathBuf::from("/tmp/target/debug/privateer")),
        None
    );
    assert!(release.binary("../escape").is_err());
    assert_eq!(
        release.binary("privateer-mcp-broker").unwrap(),
        root.join("releases")
            .join(OLD)
            .join("bin/privateer-mcp-broker")
    );
    assert_eq!(
        release.plugins(),
        root.join("releases")
            .join(OLD)
            .join("share/privateer/plugins")
    );
    assert_eq!(
        release.entrypoint("privateer-dashboard").unwrap(),
        root.join("current/bin/privateer-dashboard")
    );
}

#[test]
fn legacy_ship_records_migrate_transactionally_and_preserve_custom_paths_and_options() {
    let mut fleet = Fleet::new(now_settings());
    let checkout = fleet.root.join("old-checkout");
    let old = checkout.join("plugins/pi/manifest.json");
    fs::create_dir_all(old.parent().unwrap()).unwrap();
    fs::create_dir(checkout.join(".git")).unwrap();
    fs::write(
        checkout.join("Cargo.toml"),
        "[package]\nname = \"privateer\"\n",
    )
    .unwrap();
    fs::write(checkout.join("plugins/native.py"), "# marker").unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/pi/manifest.json"),
        &old,
    )
    .unwrap();
    let mut record = dispatch(&mut fleet.store, "ship.show", json!({"name":"infra"})).unwrap();
    record["first_mate"]["manifest"] = json!(old);
    let options = record["first_mate"]["options"].clone();
    fleet
        .store
        .connection()
        .execute(
            "UPDATE ships SET record=?1 WHERE name='infra'",
            [record.to_string()],
        )
        .unwrap();
    let mut reopened = Store::open(&fleet.home).unwrap();
    let migrated = dispatch(&mut reopened, "ship.show", json!({"name":"infra"})).unwrap();
    assert_eq!(migrated["first_mate"]["manifest"], "bundled:pi");
    assert_eq!(migrated["first_mate"]["options"], options);
    drop(reopened);
    let mut reopened = Store::open(&fleet.home).unwrap();
    assert_eq!(
        dispatch(&mut reopened, "ship.show", json!({"name":"infra"})).unwrap(),
        migrated
    );

    // Identical manifest content at a custom path is not evidence of a bundle.
    let custom = fleet.root.join("custom/plugins/pi/manifest.json");
    fs::create_dir_all(custom.parent().unwrap()).unwrap();
    fs::copy(&old, &custom).unwrap();
    for path in [
        custom,
        fleet.root.join("gone/plugins/pi/manifest.json"),
        fleet.root.join("manifest.json"),
    ] {
        record["first_mate"]["manifest"] = json!(path);
        reopened
            .connection()
            .execute(
                "UPDATE ships SET record=?1 WHERE name='infra'",
                [record.to_string()],
            )
            .unwrap();
        let mut again = Store::open(&fleet.home).unwrap();
        assert_eq!(
            dispatch(&mut again, "ship.show", json!({"name":"infra"})).unwrap()["first_mate"]["manifest"],
            json!(path)
        );
    }
    // Deleted old releases can be identified without consulting their files.
    let deleted = Release::new(&fleet.root.join("installation"), OLD)
        .unwrap()
        .plugins()
        .join("pi/manifest.json");
    record["first_mate"]["manifest"] = json!(deleted);
    reopened
        .connection()
        .execute(
            "UPDATE ships SET record=?1 WHERE name='infra'",
            [record.to_string()],
        )
        .unwrap();
    let mut again = Store::open(&fleet.home).unwrap();
    assert_eq!(
        dispatch(&mut again, "ship.show", json!({"name":"infra"})).unwrap()["first_mate"]["manifest"],
        "bundled:pi"
    );
}

#[test]
fn upgrade_status_is_read_only_home_independent_and_reports_history() {
    let fleet = Fleet::new(now_settings());
    let registry = fleet.root.join("fleet.sqlite");
    let mut command = fleet.command();
    command
        .args(["upgrade", "status"])
        .env_remove("PRIVATEER_HOME")
        .env("XDG_DATA_HOME", fleet.root.join("data"));
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let empty: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(empty["current_commit"].is_null());
    assert!(empty["running_commit"].is_null());
    assert_eq!(empty["history"], json!([]));
    assert!(
        !registry.exists(),
        "read-only status must not create a registry"
    );
    let conn = Connection::open(&registry).unwrap();
    let record = UpgradeRecord {
        id: "upgrade-1".into(),
        from_commit: Some(OLD.into()),
        to_commit: NEW.into(),
        actor: "captain".into(),
        started_ms: 100,
        completed_ms: 200,
        ships: vec![
            ShipOutcome {
                ship: "infra".into(),
                outcome: "upgraded".into(),
            },
            ShipOutcome {
                ship: "apps".into(),
                outcome: "skipped: drain-timeout".into(),
            },
        ],
    };
    privateer::releases::record_upgrade(&conn, &record).unwrap();
    privateer::releases::record_upgrade(&conn, &record).unwrap();
    let mut conflict = record.clone();
    conflict.to_commit = OLD.into();
    assert!(privateer::releases::record_upgrade(&conn, &conflict).is_err());
    let mut invalid = record.clone();
    invalid.to_commit = "main".into();
    assert!(privateer::releases::record_upgrade(&conn, &invalid).is_err());
    let output = command.output().unwrap();
    assert!(output.status.success());
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["history"], json!([record]));
    assert_eq!(status["keep_releases"], 3);

    // No home argument or ship discovery is necessary, even for an agent.
    let output = support::command(BIN)
        .current_dir(&fleet.root)
        .args(["upgrade.status", "--json", "{}"])
        .env("HOME", fleet.root.join("user"))
        .env("XDG_DATA_HOME", fleet.root.join("data"))
        .env("PRIVATEER_FLEET", &registry)
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "infra")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["history"],
        status["history"]
    );
}

#[test]
fn status_reports_broken_current_without_repairing_it() {
    let fleet = Fleet::new(now_settings());
    let data = fleet.root.join("data/privateer");
    fs::create_dir_all(&data).unwrap();
    symlink(data.join("releases").join(OLD), data.join("current")).unwrap();
    let output = fleet
        .command()
        .args(["upgrade", "status"])
        .env("XDG_DATA_HOME", fleet.root.join("data"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot resolve the current release"));
    assert_eq!(
        fs::read_link(data.join("current")).unwrap(),
        data.join("releases").join(OLD)
    );
}

/// The release layout (CI's and a source build's) copies every binary a
/// release ships, the MCP broker included: `mcp-broker install` takes it
/// from releases/<commit>/bin.
#[test]
fn the_release_layout_ships_every_binary_and_install_takes_the_broker_from_it() {
    let script =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/release-layout.sh"))
            .unwrap();
    let copy = script
        .lines()
        .find(|line| line.starts_with("cp \"$build/privateer\" "))
        .expect("the release layout's binary copy");
    for name in BINARIES {
        assert!(
            copy.split_whitespace()
                .any(|word| word == format!("\"$build/{name}\"")),
            "the release layout does not copy {name}: {copy}"
        );
    }
    // A release laid out that way: its privateer installs its own broker.
    let temp = tempfile::tempdir().unwrap();
    // Short, so the broker's socket path fits macOS's 104 bytes.
    let short = tempfile::Builder::new()
        .prefix("pd")
        .tempdir_in("/tmp")
        .unwrap();
    let data = short.path().canonicalize().unwrap();
    let release = Release::new(&data.join("privateer"), NEW).unwrap();
    fs::create_dir_all(release.root.join("bin")).unwrap();
    let broker = env!("CARGO_BIN_EXE_privateer-mcp-broker");
    fs::copy(BIN, release.binary("privateer").unwrap()).unwrap();
    fs::copy(broker, release.binary("privateer-mcp-broker").unwrap()).unwrap();
    fs::create_dir_all(temp.path().join("user")).unwrap();
    let output = support::command(release.binary("privateer").unwrap())
        .args([
            "mcp-broker",
            "install",
            "--name",
            "plain",
            "--upstream",
            "/usr/bin/true",
        ])
        .current_dir(temp.path())
        .env("HOME", temp.path().join("user"))
        .env("XDG_DATA_HOME", &data)
        .env("PRIVATEER_RESOURCE_HOME", temp.path().join("ledger"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let installed: Value = serde_json::from_slice(&output.stdout).unwrap();
    let stable = data.join("privateer/mcp-broker/bin/privateer-mcp-broker");
    assert_eq!(installed["binary"], stable.to_string_lossy().as_ref());
    assert_eq!(fs::read(&stable).unwrap(), fs::read(broker).unwrap());
}
