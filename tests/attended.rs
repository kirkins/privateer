use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");

struct Ship {
    _temp: tempfile::TempDir,
    root: PathBuf,
    folder: PathBuf,
    path: String,
}

impl Ship {
    /// A folder made into the ship "infra", with a fake codex harness that
    /// exits at once.
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let folder = root.join("infra");
        fs::create_dir(&folder).unwrap();
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("codex"), "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o755)).unwrap();
        let ship = Self {
            _temp: temp,
            path: format!("{}:/usr/bin:/bin", bin.display()),
            root,
            folder,
        };
        ship.ok(&["init"]);
        ship
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        command
            .args(args)
            .current_dir(&self.folder)
            .env("PATH", &self.path)
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("PRIVATEER_SERVICE", "none")
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env("PRIVATEER_FLEET", self.root.join("fleet.sqlite"))
            .env_remove("PRIVATEER_HOME")
            .env_remove("PRIVATEER_ROLE")
            .env_remove("PRIVATEER_ACTOR")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH");
        command
    }
    fn ok(&self, args: &[&str]) -> String {
        let output = self.command(args).output().unwrap();
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
    fn hook(&self, event: &str, session: &str) -> Output {
        let mut child = self
            .command(&["hook", "claude", event])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                json!({"session_id": session, "hook_event_name": event})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        child.wait_with_output().unwrap()
    }
    fn presence(&self) -> Value {
        self.json("ship.presence", json!({"ship":"infra"}))
    }
}

#[test]
fn presence_leases_are_refreshed_released_and_expire() {
    let ship = Ship::new();
    assert_eq!(ship.presence()["attended"], false);
    ship.json(
        "ship.attend",
        json!({"ship":"infra","holder":"pane-1","ttl_secs":60}),
    );
    assert_eq!(ship.presence()["attended"], true);
    assert!(
        ship.ok(&["status"])
            .contains("The captain's session is aboard infra.")
    );
    // Only the holder releases it.
    assert_eq!(
        ship.json(
            "ship.leave",
            json!({"ship":"infra","holder":"someone-else"})
        )["left"],
        false
    );
    assert_eq!(
        ship.json("ship.leave", json!({"ship":"infra","holder":"pane-1"}))["left"],
        true
    );
    ship.json(
        "ship.attend",
        json!({"ship":"infra","holder":"pane-2","ttl_secs":1}),
    );
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert_eq!(ship.presence()["attended"], false);
    // An agent can be aboard only its own ship.
    ship.json("ship.put", json!({"name":"apps","scope":"apps"}));
    let refused = ship
        .command(&["ship.attend", "--json", r#"{"ship":"apps","holder":"x"}"#])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "infra")
        .output()
        .unwrap();
    assert!(!refused.status.success());
}

#[test]
fn expired_reused_holder_rebinds_while_live_refresh_preserves_identity() {
    use std::{
        io::{BufRead, BufReader},
        os::unix::net::UnixListener,
    };
    let ship = Ship::new();
    let socket = ship.root.join("herdr.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        for pane in ["old", "new", "new", "claim"] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            let request: Value = serde_json::from_str(&request).unwrap();
            assert_eq!(request["method"], "session.snapshot");
            writeln!(stream, "{}", json!({"id":request["id"],"result":{"snapshot":{"version":"test","protocol":20,"panes":[{"pane_id":pane,"terminal_id":format!("terminal-{pane}"),"workspace_id":"workspace","tab_id":"tab","agent_status":"idle"}]}}})).unwrap();
        }
    });
    let holder = "start:reused-pid";
    let attend = |pane: &str| {
        let output = ship
            .command(&[
                "ship.attend",
                "--json",
                &json!({"ship":"infra","holder":holder,"ttl_secs":60}).to_string(),
            ])
            .env("HERDR_ENV", "1")
            .env("HERDR_SOCKET_PATH", &socket)
            .env("HERDR_PANE_ID", pane)
            .env("HERDR_WORKSPACE_ID", "workspace")
            .env("HERDR_TAB_ID", "tab")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let first = attend("old");
    assert_eq!(first["lease"]["binding"]["pane"]["pane_id"], "old");
    let store = privateer::store::Store::open(&ship.folder.join(".privateer")).unwrap();
    store
        .connection()
        .execute(
            "UPDATE presence SET started_ms=1, presented_through=7 WHERE ship='infra'",
            [],
        )
        .unwrap();
    let refresh = attend("new");
    assert_eq!(refresh["lease"]["started_ms"], 1);
    assert_eq!(refresh["lease"]["binding"], first["lease"]["binding"]);
    assert!(!ship.command(&["start"]).output().unwrap().status.success());
    assert_eq!(ship.presence(), refresh);
    store
        .connection()
        .execute("UPDATE presence SET expires_ms=0 WHERE ship='infra'", [])
        .unwrap();
    let reclaimed = attend("new");
    assert!(reclaimed["lease"]["started_ms"].as_i64().unwrap() > 1);
    assert_eq!(reclaimed["lease"]["binding"]["pane"]["pane_id"], "new");
    let presented: i64 = store
        .connection()
        .query_row(
            "SELECT presented_through FROM presence WHERE ship='infra'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(presented, 7);
    store
        .connection()
        .execute(
            "UPDATE presence SET started_ms=1, expires_ms=0 WHERE ship='infra'",
            [],
        )
        .unwrap();
    let probe = ship.root.join("claimed.json");
    let harness = ship.root.join("bin/claim-probe");
    fs::write(
        &harness,
        format!(
            "#!/bin/sh\n'{BIN}' ship.presence --json '{{\"ship\":\"infra\"}}' > '{}'\n",
            probe.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&harness, fs::Permissions::from_mode(0o755)).unwrap();
    let output = ship
        .command(&["start", "claim-probe"])
        .env("HERDR_ENV", "1")
        .env("HERDR_SOCKET_PATH", &socket)
        .env("HERDR_PANE_ID", "claim")
        .env("HERDR_WORKSPACE_ID", "workspace")
        .env("HERDR_TAB_ID", "tab")
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let claimed: Value = serde_json::from_slice(&fs::read(probe).unwrap()).unwrap();
    assert!(
        claimed["lease"]["holder"]
            .as_str()
            .unwrap()
            .starts_with("start:")
    );
    assert!(claimed["lease"]["started_ms"].as_i64().unwrap() > 1);
    assert_eq!(claimed["lease"]["binding"]["pane"]["pane_id"], "claim");
    assert_eq!(ship.presence()["attended"], false);
    ship.json("ship.attend", json!({"ship":"infra","holder":holder}));
    store
        .connection()
        .execute(
            "UPDATE presence SET started_ms=1, expires_ms=0, binding=?1 WHERE ship='infra'",
            [first["lease"]["binding"].to_string()],
        )
        .unwrap();
    let unbound = ship.json("ship.attend", json!({"ship":"infra","holder":holder}));
    assert!(unbound["lease"]["started_ms"].as_i64().unwrap() > 1);
    assert!(unbound["lease"]["binding"].is_null());
}

#[test]
fn init_installs_claude_hooks_once() {
    let ship = Ship::new();
    ship.ok(&["init"]);
    let settings: Value =
        serde_json::from_slice(&fs::read(ship.folder.join(".claude/settings.json")).unwrap())
            .unwrap();
    for event in ["SessionStart", "UserPromptSubmit", "Stop", "SessionEnd"] {
        let entries = settings["hooks"][event].as_array().unwrap();
        assert_eq!(entries.len(), 1, "{event}");
        assert_eq!(
            entries[0]["hooks"][0]["command"],
            format!("{BIN} hook claude {event}")
        );
    }
}

#[test]
fn claude_hooks_deliver_mail_to_the_session_once_and_track_presence() {
    let ship = Ship::new();
    let start = ship.hook("SessionStart", "s1");
    assert!(start.status.success());
    let context = String::from_utf8_lossy(&start.stdout);
    assert!(
        context.contains("first mate of the ship infra"),
        "{context}"
    );
    assert!(context.contains("Bearings for infra"));
    assert_eq!(ship.presence()["lease"]["holder"], "claude:s1");

    ship.ok(&["ask", "infra", "Is", "staging", "healthy?"]);
    let prompt = ship.hook("UserPromptSubmit", "s1");
    assert!(String::from_utf8_lossy(&prompt.stdout).contains("Is staging healthy?"));
    // Presented once: the next prompt does not repeat it.
    let again = ship.hook("UserPromptSubmit", "s1");
    assert!(!String::from_utf8_lossy(&again.stdout).contains("Is staging healthy?"));

    // Mail arriving mid-turn keeps the first mate working at turn end, once.
    ship.ok(&["tell", "infra", "Deploy", "window", "moved"]);
    let stop = ship.hook("Stop", "s1");
    assert_eq!(stop.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&stop.stderr).contains("Deploy window moved"));
    let stop = ship.hook("Stop", "s1");
    assert_eq!(stop.status.code(), Some(0));

    let end = ship.hook("SessionEnd", "s1");
    assert!(end.status.success());
    assert_eq!(ship.presence()["attended"], false);
}

#[test]
fn hooks_never_break_the_harness_outside_a_ship() {
    let ship = Ship::new();
    let mut child = ship
        .command(&["hook", "claude", "Stop"])
        .current_dir(&ship.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"not json").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
}

#[test]
fn serve_leaves_an_attended_ship_to_its_session() {
    let ship = Ship::new();
    ship.json(
        "ship.put",
        json!({"name":"infra","first_mate":{"manifest": ship.json("ship.show", json!({"name":"infra"}))["first_mate"]["manifest"],
            "duty_task":"infra-duty","options":{"sandbox":"workspace-write"},"batch_secs":0,"max_delay_secs":0}}),
    );
    ship.json(
        "ship.attend",
        json!({"ship":"infra","holder":"pane","ttl_secs":60}),
    );
    ship.ok(&["ask", "infra", "anything?"]);
    let report: Value =
        serde_json::from_str(&ship.ok(&["serve", "--json", r#"{"once":true}"#])).unwrap();
    assert_eq!(report["ships"][0]["skipped"], "captain's session is aboard");
    ship.json("ship.leave", json!({"ship":"infra","holder":"pane"}));
    let report: Value =
        serde_json::from_str(&ship.ok(&["serve", "--json", r#"{"once":true}"#])).unwrap();
    assert_eq!(report["ships"][0]["status"], "running", "{report}");
}

#[test]
fn start_runs_the_harness_as_the_ship_while_holding_presence() {
    let ship = Ship::new();
    let probe = ship.root.join("probe.txt");
    let harness = ship.root.join("bin/fakeharness");
    fs::write(
        &harness,
        format!(
            "#!/bin/sh\n{{ echo \"$PRIVATEER_ROLE $PRIVATEER_ACTOR $1\"; '{BIN}' ship.presence --json '{{\"ship\":\"infra\"}}'; }} > '{}'\nexit 7\n",
            probe.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&harness, fs::Permissions::from_mode(0o755)).unwrap();
    let output = ship
        .command(&["start", "fakeharness", "--", "hello"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let seen = fs::read_to_string(&probe).unwrap();
    let (env, presence) = seen.split_once('\n').unwrap();
    assert_eq!(env, "agent infra hello");
    let presence: Value = serde_json::from_str(presence).unwrap();
    assert_eq!(presence["attended"], true);
    assert!(
        presence["lease"]["holder"]
            .as_str()
            .unwrap()
            .starts_with("start:")
    );
    assert_eq!(ship.presence()["attended"], false);
}

#[test]
fn a_failed_attended_launch_releases_its_presence() {
    let ship = Ship::new();
    let output = ship
        .command(&["start", "privateer-nonexistent-test-harness"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(ship.presence()["attended"], false);
}

#[test]
fn simultaneous_attended_starts_have_one_live_harness_and_lease_owner() {
    use std::time::{Duration, Instant};
    let ship = Ship::new();
    let harness = ship.root.join("bin/controlled-mate");
    let began = ship.root.join("began");
    let release = ship.root.join("release");
    fs::write(
        &harness,
        format!(
            "#!/bin/sh\ntouch '{}'\nwhile [ ! -e '{}' ]; do sleep 0.02; done\n",
            began.display(),
            release.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&harness, fs::Permissions::from_mode(0o755)).unwrap();
    let mut first = ship
        .command(&["start", "controlled-mate"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !began.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if !began.exists() {
        fs::write(&release, "").unwrap();
        let _ = first.wait();
        panic!("first attended harness did not start");
    }
    let owner = ship.presence()["lease"]["holder"].clone();
    let second = ship
        .command(&["start", "controlled-mate"])
        .output()
        .unwrap();
    let still = ship.presence();
    fs::write(release, "").unwrap();
    let ended = first.wait().unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already aboard"));
    assert_eq!(still["lease"]["holder"], owner);
    assert!(ended.success());
    assert_eq!(ship.presence()["attended"], false);
}
