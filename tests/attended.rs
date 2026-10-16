mod support;

use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");

#[test]
fn hook_caller_flags_preserve_fail_open_and_own_ship_checks() {
    let ship = Ship::new();
    for (flags, role, actor) in [
        (vec!["--role", "agent", "--actor", "foreign"], None, None),
        (
            vec!["--role", "operator", "--actor", "foreign"],
            Some("agent"),
            Some("infra"),
        ),
        (vec!["--role", "invalid"], Some("agent"), Some("infra")),
    ] {
        let mut command = ship.command(&["hook", "claude", "SessionStart"]);
        command.args(flags);
        if let Some(role) = role {
            command.env("PRIVATEER_ROLE", role);
        }
        if let Some(actor) = actor {
            command.env("PRIVATEER_ACTOR", actor);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            ship.json("ship.presence", json!({"ship":"infra"}))["attended"],
            false
        );
    }
    let start = ship
        .command(&[
            "hook",
            "claude",
            "SessionStart",
            "--role",
            "agent",
            "--actor",
            "infra",
        ])
        .output()
        .unwrap();
    assert!(start.status.success());
    assert_eq!(
        ship.json("ship.presence", json!({"ship":"infra"}))["attended"],
        true
    );
    let end = ship
        .command(&[
            "hook",
            "claude",
            "SessionEnd",
            "--role",
            "agent",
            "--actor",
            "infra",
        ])
        .output()
        .unwrap();
    assert!(end.status.success());
    assert_eq!(
        ship.json("ship.presence", json!({"ship":"infra"}))["attended"],
        false
    );
    let cursor = ship
        .command(&["hook", "cursor", "sessionStart", "--role", "invalid"])
        .output()
        .unwrap();
    assert!(cursor.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&cursor.stdout).unwrap()["continue"],
        true
    );
}

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
        let mut command = support::command(BIN);
        command
            .args(args)
            .current_dir(&self.folder)
            .env("PATH", &self.path)
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("PRIVATEER_SERVICE", "none")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH")
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env("PRIVATEER_FLEET", self.root.join("fleet.sqlite"))
            .env_remove("PRIVATEER_HOME")
            .env_remove("PRIVATEER_ROLE")
            .env_remove("PRIVATEER_ACTOR");
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
        self.hook_with(event, session, &[])
    }
    fn hook_with(&self, event: &str, session: &str, envs: &[(&str, &str)]) -> Output {
        let mut child = self
            .command(&["hook", "claude", event])
            .envs(envs.iter().copied())
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
    let mut child = support::command(BIN)
        .args(["hook", "claude", "Stop"])
        .current_dir(&ship.root)
        .env_remove("PRIVATEER_HOME")
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
fn heartbeat_cannot_reclaim_a_replaced_or_released_holder() {
    let ship = Ship::new();
    let store = privateer::store::Store::open(&ship.folder.join(".privateer")).unwrap();
    privateer::presence::attend(&store, "infra", "old", 60, Some("pi")).unwrap();
    assert!(privateer::presence::refresh(&store, "infra", "old", 90).unwrap());
    privateer::presence::attend(&store, "infra", "new", 60, Some("omp")).unwrap();
    assert!(!privateer::presence::refresh(&store, "infra", "old", 90).unwrap());
    assert_eq!(ship.presence()["lease"]["holder"], "new");
    assert!(!privateer::presence::leave(&store, "infra", "old").unwrap());
    assert!(privateer::presence::leave(&store, "infra", "new").unwrap());
    assert!(!privateer::presence::refresh(&store, "infra", "new", 90).unwrap());
    assert_eq!(ship.presence()["attended"], false);
    privateer::presence::attend(&store, "infra", "expired", 60, Some("pi")).unwrap();
    store
        .connection()
        .execute("UPDATE presence SET expires_ms=0 WHERE ship='infra'", [])
        .unwrap();
    assert!(!privateer::presence::refresh(&store, "infra", "expired", 90).unwrap());
    assert_eq!(ship.presence()["attended"], false);
}

#[test]
fn wrapped_hooks_share_the_parent_lease_and_stale_hooks_do_not_deliver_mail() {
    let ship = Ship::new();
    ship.json(
        "ship.attend",
        json!({"ship":"infra","holder":"start:123","ttl_secs":90}),
    );
    let envs = [
        ("PRIVATEER_PRESENCE_HOLDER", "start:123"),
        ("PRIVATEER_ROLE", "agent"),
        ("PRIVATEER_ACTOR", "infra"),
    ];
    let start = ship.hook_with("SessionStart", "native-id", &envs);
    assert!(start.status.success());
    assert!(String::from_utf8_lossy(&start.stdout).contains("first mate"));
    assert_eq!(ship.presence()["lease"]["holder"], "start:123");
    ship.hook_with("SessionEnd", "native-id", &envs);
    assert_eq!(ship.presence()["attended"], true);
    ship.json(
        "ship.attend",
        json!({"ship":"infra","holder":"replacement","ttl_secs":90}),
    );
    ship.ok(&["ask", "infra", "private mail"]);
    let stale = ship.hook_with("UserPromptSubmit", "native-id", &envs);
    assert!(stale.stdout.is_empty());
    assert_eq!(ship.presence()["lease"]["holder"], "replacement");
    let wrong = ship.hook_with(
        "UserPromptSubmit",
        "wrong",
        &[("PRIVATEER_ROLE", "agent"), ("PRIVATEER_ACTOR", "apps")],
    );
    assert!(wrong.stdout.is_empty());
    assert_eq!(ship.presence()["lease"]["holder"], "replacement");
    assert!(
        String::from_utf8_lossy(&ship.hook("UserPromptSubmit", "current").stdout)
            .contains("private mail")
    );
}

#[test]
fn pi_family_starts_with_the_optional_extension_and_preserves_native_routes() {
    for harness in ["pi", "pi-signed", "omp"] {
        let ship = Ship::new();
        let probe = ship.root.join("native.json");
        let fake = ship.root.join("bin").join(harness);
        fs::write(&fake,format!(r#"#!/usr/bin/env python3
import json,os,subprocess,sys
env = {{k:os.environ.get(k) for k in ["PRIVATEER_ROLE","PRIVATEER_ACTOR","PRIVATEER_HOME","PRIVATEER_CONTROL_CLI","PRIVATEER_PRESENCE_HOLDER","PRIVATEER_ATTENDED_HARNESS","PI_CODING_AGENT_DIR","ZAI_BASE_URL","CLAUDECODE"]}}
p = subprocess.run([os.environ["PRIVATEER_CONTROL_CLI"],"ship.presence","--json",'{{"ship":"infra"}}'],capture_output=True,text=True,check=True)
with open({:?},"w") as out: json.dump({{"env":env,"argv":sys.argv[1:],"presence":json.loads(p.stdout)}},out)
sys.exit(7)
"#,probe.to_string_lossy())).unwrap();
        fs::set_permissions(fake, fs::Permissions::from_mode(0o755)).unwrap();
        let home = ship.folder.join(".privateer");
        let output = ship
            .command(&[
                "--home",
                home.to_str().unwrap(),
                "start",
                harness,
                "--",
                "literal; $value",
            ])
            .env(
                "PI_CODING_AGENT_DIR",
                ship.root.join("native-provider-config"),
            )
            .env("ZAI_BASE_URL", "https://gateway.example.invalid/v1")
            .env("CLAUDECODE", "foreign")
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(7),
            "{harness}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let seen: Value = serde_json::from_slice(&fs::read(&probe).unwrap()).unwrap();
        assert_eq!(seen["argv"][0], "-e");
        assert!(
            seen["argv"][1]
                .as_str()
                .unwrap()
                .ends_with("plugins/attended/mail.ts")
        );
        let prompt_at = if harness == "omp" {
            assert!(
                !seen["argv"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|a| { a.as_str().is_some_and(|a| a.ends_with("attended/calm.ts")) })
            );
            2
        } else {
            assert_eq!(seen["argv"][2], "-e");
            assert!(
                seen["argv"][3]
                    .as_str()
                    .unwrap()
                    .ends_with("attended/calm.ts")
            );
            4
        };
        assert_eq!(seen["argv"][prompt_at], "literal; $value");
        assert_eq!(seen["env"]["PRIVATEER_ROLE"], "agent");
        assert_eq!(seen["env"]["PRIVATEER_ACTOR"], "infra");
        assert_eq!(seen["env"]["PRIVATEER_ATTENDED_HARNESS"], harness);
        assert_eq!(
            seen["env"]["PRIVATEER_HOME"],
            ship.folder.join(".privateer").to_string_lossy().as_ref()
        );
        assert_eq!(
            seen["env"]["PI_CODING_AGENT_DIR"],
            ship.root
                .join("native-provider-config")
                .to_string_lossy()
                .as_ref()
        );
        assert_eq!(
            seen["env"]["ZAI_BASE_URL"],
            "https://gateway.example.invalid/v1"
        );
        assert!(seen["env"]["CLAUDECODE"].is_null());
        assert_eq!(seen["presence"]["attended"], true);
        assert_eq!(
            seen["presence"]["lease"]["holder"],
            seen["env"]["PRIVATEER_PRESENCE_HOLDER"]
        );
        assert_eq!(ship.presence()["attended"], false);
    }
}

#[test]
fn opencode_v2_start_uses_the_versioned_launcher_and_private_server() {
    let ship = Ship::new();
    let record = ship.root.join("opencode.json");
    let script = r#"#!/usr/bin/env python3
import json,os,sys
if sys.argv[1:]==['--version']:
    print('opencode v2.0.29');raise SystemExit(0)
with open(os.environ['OC_TEST_RECORD'],'w') as f:
    json.dump({'argv':sys.argv[1:],'role':os.getenv('PRIVATEER_ROLE'),'actor':os.getenv('PRIVATEER_ACTOR'),'route':os.getenv('OC_TEST_ROUTE'),'foreign':os.getenv('CLAUDECODE')},f)
"#;
    let native = ship.root.join("bin/opencode");
    let own = ship.folder.join(".opencode/plugins/privateer-attended-v2");
    let custom = ship.folder.join(".opencode/plugins/captain/tui.js");
    fs::create_dir_all(custom.parent().unwrap()).unwrap();
    fs::write(&custom, "export default {id:'captain'};\n").unwrap();
    fs::write(&native, script).unwrap();
    fs::set_permissions(&native, fs::Permissions::from_mode(0o755)).unwrap();
    let output = ship
        .command(&["start", "opencode-v2", "--", "--prompt", "literal; $value"])
        .env("OC_TEST_RECORD", &record)
        .env("OC_TEST_ROUTE", "https://gateway.example.invalid/v1")
        .env("CLAUDECODE", "foreign")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let seen: Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
    assert_eq!(
        seen["argv"],
        json!(["--standalone", "--prompt", "literal; $value"])
    );
    assert_eq!(seen["role"], "agent");
    assert_eq!(seen["actor"], "infra");
    assert_eq!(seen["route"], "https://gateway.example.invalid/v1");
    assert!(seen["foreign"].is_null());
    assert_eq!(ship.presence()["attended"], false);
    let entry = fs::read_to_string(own.join("tui.js")).unwrap();
    assert!(entry.starts_with("// Generated by Privateer"));
    assert!(entry.contains("attended-opencode-v2/tui.tsx"));
    assert_eq!(
        fs::read_to_string(&custom).unwrap(),
        "export default {id:'captain'};\n"
    );
    assert!(
        ship.command(&["start", "opencode-v2"])
            .env("OC_TEST_RECORD", &record)
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(fs::read_to_string(own.join("tui.js")).unwrap(), entry);
    fs::write(&native, "#!/bin/sh\nprintf '1.19.0\\n'\n").unwrap();
    assert!(
        !ship
            .command(&["start", "opencode-v2"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(ship.presence()["attended"], false);
}

#[test]
fn opencode_mail_installer_refuses_symlinks_and_custom_collisions() {
    use std::os::unix::fs::symlink;
    let plugins = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins");
    for suffix in [
        ".opencode",
        ".opencode/plugins",
        ".opencode/plugins/privateer-attended-v2",
        ".opencode/plugins/privateer-attended-v2/tui.js",
        ".opencode/plugins/privateer-attended-v2/package.json",
    ] {
        let ship = Ship::new();
        let link = ship.folder.join(suffix);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        let target = ship.root.join("outside");
        fs::create_dir(&target).unwrap();
        symlink(&target, &link).unwrap();
        assert!(privateer::opencode_mail::install(&ship.folder, &plugins).is_err());
        assert_eq!(fs::read_dir(target).unwrap().count(), 0);
    }
    let ship = Ship::new();
    let directory = ship.folder.join(".opencode/plugins/privateer-attended-v2");
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join("package.json"),
        "{\"name\":\"captains-plugin\"}",
    )
    .unwrap();
    assert!(privateer::opencode_mail::install(&ship.folder, &plugins).is_err());
    assert!(
        !directory.join("tui.js").exists(),
        "validate all owned files before writing"
    );
    let unusual = ship.root.join("plugin's # café");
    fs::create_dir_all(unusual.join("attended-opencode-v2")).unwrap();
    fs::write(
        unusual.join("attended-opencode-v2/tui.tsx"),
        "export default {};",
    )
    .unwrap();
    fs::remove_file(directory.join("package.json")).unwrap();
    privateer::opencode_mail::install(&ship.folder, &unusual).unwrap();
    let proxy = fs::read_to_string(directory.join("tui.js")).unwrap();
    assert!(proxy.contains("plugin%27s%20%23%20caf%C3%A9/attended-opencode-v2/tui.tsx"));
}

#[test]
fn opencode_tui_mail_uses_real_mail_and_native_lifecycle_contracts() {
    let output = support::command("node")
        .arg("tests/opencode-attended.mjs")
        .arg(BIN)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("Node is required to validate the optional OpenCode V2 TUI adapter");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn claude_start_loads_the_optional_mod_and_clears_foreign_mail_health() {
    let ship = Ship::new();
    let native = ship.root.join("bin/claude");
    let record = ship.root.join("claude.json");
    fs::write(&native, r#"#!/usr/bin/env python3
import json,os,sys
with open(os.environ['CLAUDE_TEST_RECORD'],'w') as f:
    json.dump({'argv':sys.argv[1:],'role':os.getenv('PRIVATEER_ROLE'),'actor':os.getenv('PRIVATEER_ACTOR'),'route':os.getenv('ANTHROPIC_BASE_URL'),'health':os.getenv('PRIVATEER_CLAUDE_MAIL_UNTIL'),'foreign':os.getenv('CLAUDECODE')},f)
"#).unwrap();
    fs::set_permissions(&native, fs::Permissions::from_mode(0o755)).unwrap();
    let output = ship
        .command(&["start", "claude", "--", "--model", "native-choice"])
        .env("CLAUDE_TEST_RECORD", &record)
        .env("ANTHROPIC_BASE_URL", "https://provider.example.invalid")
        .env("PRIVATEER_CLAUDE_MAIL_UNTIL", "9999999999999")
        .env("CLAUDECODE", "foreign")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let seen: Value = serde_json::from_slice(&fs::read(record).unwrap()).unwrap();
    let argv = seen["argv"].as_array().unwrap();
    assert!(
        argv.windows(2).any(|a| a[0] == "--plugin-dir"
            && a[1].as_str().unwrap().ends_with("plugins/attended-claude"))
    );
    assert!(
        argv.windows(2)
            .any(|a| a[0] == "--model" && a[1] == "native-choice")
    );
    assert!(
        argv.windows(2)
            .any(|a| a[0] == "--permission-mode" && a[1] == "acceptEdits")
    );
    assert!(argv.contains(&json!("Bash(git *)")) && argv.contains(&json!("Bash(gh *)")));
    assert_eq!(seen["role"], "agent");
    assert_eq!(seen["actor"], "infra");
    assert_eq!(seen["route"], "https://provider.example.invalid");
    assert!(seen["health"].is_null());
    assert!(seen["foreign"].is_null());
    assert_eq!(ship.presence()["attended"], false);
}

#[test]
fn crew_claude_hooks_never_take_the_captains_presence_or_mail() {
    let ship = Ship::new();
    ship.json(
        "ship.attend",
        json!({"ship":"infra","holder":"captain","ttl_secs":90}),
    );
    ship.json("mail.send", json!({"id":"pending-for-captain","from":"captain","to":"infra","kind":"request","body":"handle aboard"}));
    for event in ["SessionStart", "UserPromptSubmit", "Stop", "SessionEnd"] {
        let output = ship.hook_with(
            event,
            "crew-session",
            &[
                ("PRIVATEER_JOB", "crew-job"),
                ("PRIVATEER_PRESENCE_HOLDER", "captain"),
            ],
        );
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
    assert_eq!(ship.presence()["lease"]["holder"], "captain");
    assert_eq!(
        ship.json("mail.pending", json!({"address":"infra"}))["count"],
        1
    );
    let normal = ship.hook_with(
        "UserPromptSubmit",
        "captain-session",
        &[("PRIVATEER_PRESENCE_HOLDER", "captain")],
    );
    assert!(String::from_utf8_lossy(&normal.stdout).contains("pending-for-captain"));
}

#[test]
fn failed_start_releases_presence() {
    let ship = Ship::new();
    assert!(
        !ship
            .command(&["start", "missing-native-harness"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(ship.presence()["attended"], false);
}

#[test]
fn attended_mail_extension_uses_real_mail_and_native_lifecycle_contracts() {
    let output = support::command("node")
        .arg("tests/attended-mail.mjs")
        .arg(BIN)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("Node 24 is required to validate the optional Pi/OMP adapter");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn claude_mail_mod_uses_real_mail_and_native_lifecycle_contracts() {
    let output = Command::new("node")
        .arg("tests/claude-attended.mjs")
        .arg(BIN)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("Node is required to validate the optional Claude mail mod");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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

#[test]
fn starting_a_ship_uses_its_first_mates_model_unless_the_captain_picks_one() {
    let ship = Ship::new();
    ship.ok(&["init", "--harness", "pi"]);
    let home = ship.folder.join(".privateer");
    let mut profile: Value =
        serde_json::from_str(&ship.ok(&["ship.show", "--json", r#"{"name":"infra"}"#])).unwrap();
    profile["first_mate"]["options"] = json!({"provider":"openai","model":"chosen"});
    ship.ok(&[
        "ship.put",
        "--json",
        &json!({"name":"infra","first_mate":profile["first_mate"]}).to_string(),
    ]);
    let probe = ship.root.join("argv.json");
    let fake = ship.root.join("bin/pi");
    fs::write(
        &fake,
        format!(
            "#!/usr/bin/env python3\nimport json,sys\njson.dump(sys.argv[1:],open({:?},'w'))\n",
            probe.to_string_lossy()
        ),
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let argv = |extra: &[&str]| -> Vec<String> {
        let mut args = vec!["--home", home.to_str().unwrap(), "start"];
        args.extend(extra);
        let output = ship.command(&args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&fs::read(&probe).unwrap()).unwrap()
    };
    let routed = argv(&[]);
    let at = routed
        .iter()
        .position(|a| a == "--provider")
        .expect("provider");
    assert_eq!(routed[at + 1..at + 4], ["openai", "--model", "chosen"]);
    let own = argv(&["--", "--model", "mine"]);
    assert!(
        !own.iter().any(|a| a == "--provider" || a == "chosen"),
        "{own:?}"
    );
    assert!(own.ends_with(&["--model".to_owned(), "mine".to_owned()]));
}
