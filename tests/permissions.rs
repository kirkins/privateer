mod support;

use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");
const FAKE: &str = r#"#!/usr/bin/env python3
import json,os,pathlib,sys
if os.getenv('CAPTURE_ARGV'):
 pathlib.Path(os.environ['CAPTURE_ARGV']).write_text(json.dumps(sys.argv[1:]))
if '--print' in sys.argv:
 print(json.dumps({'type':'system','subtype':'init','session_id':'12345678-1234-4234-8234-123456789abc'}))
 denied=[] if 'bypassPermissions' in sys.argv else [{'tool_name':'Bash','tool_input':{'command':'PRIVATE_DO_NOT_MAIL'}}]
 print(json.dumps({'type':'result','subtype':'success','is_error':False,'session_id':'12345678-1234-4234-8234-123456789abc','permission_denials':denied}))
"#;

struct Fleet {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}
impl Fleet {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        for name in ["claude", "codex"] {
            let p = root.join("bin").join(name);
            fs::write(&p, FAKE).unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let f = Self { _tmp: tmp, root };
        f.init("liaison", "claude");
        f
    }
    fn command(&self, ship: Option<&str>, args: &[&str]) -> Command {
        let mut c = support::command(BIN);
        c.current_dir(ship.map(|s| self.root.join(s)).unwrap_or(self.root.clone()))
            .args(args)
            .env("HOME", &self.root)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.root.join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("PRIVATEER_FLEET", self.root.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env("PRIVATEER_SERVICE", "none")
            .env("CAPTURE_ARGV", self.root.join("argv.json"))
            .env(
                "PRIVATEER_PLUGINS",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins"),
            )
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH");
        c
    }
    fn ok(&self, ship: Option<&str>, args: &[&str]) -> String {
        let o = self.command(ship, args).output().unwrap();
        assert!(
            o.status.success(),
            "{:?}: {}",
            args,
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8(o.stdout).unwrap()
    }
    fn json(&self, ship: &str, verb: &str, args: Value) -> Value {
        serde_json::from_str(&self.ok(Some(ship), &[verb, "--json", &args.to_string()])).unwrap()
    }
    fn init(&self, name: &str, harness: &str) {
        let folder = self.root.join(name);
        fs::create_dir_all(&folder).unwrap();
        self.ok(Some(name), &["init", "--ship", name, "--harness", harness]);
        let mut profile = self.json(name, "ship.show", json!({"name":name}));
        profile["first_mate"]["batch_secs"] = json!(0);
        profile["first_mate"]["retry_backoff_secs"] = json!(0);
        self.json(name, "ship.put", profile);
    }
    fn plan(&self, name: &str) -> Value {
        let s = self.json(name, "ship.show", json!({"name":name}));
        self.json(name,"plugin.command",json!({"task_id":format!("{name}-duty"),"manifest":s["first_mate"]["manifest"],"options":s["first_mate"]["options"]}))
    }
    fn wait(&self, name: &str, id: &str) -> Value {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let j = self.json(name, "job.status", json!({"id":id}));
            if !matches!(
                j["status"].as_str(),
                Some("running" | "starting" | "launching" | "queued")
            ) {
                return j;
            }
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}

#[test]
fn captain_policy_applies_to_old_and_future_ships_without_editing_harness_settings() {
    let f = Fleet::new();
    let settings = f.root.join("liaison/.claude/settings.json");
    let before = fs::read(&settings).unwrap();
    let standard = f.plan("liaison").to_string();
    assert!(standard.contains("Bash(git *)") && standard.contains("Bash(gh *)"));
    f.ok(None, &["permissions", "allow", "Bash(forge-helper *)"]);
    assert!(
        f.plan("liaison")
            .to_string()
            .contains("Bash(forge-helper *)")
    );
    f.ok(None, &["permissions", "yolo"]);
    f.init("engineering", "codex");
    assert!(f.plan("liaison").to_string().contains("bypassPermissions"));
    let plan = f.plan("engineering");
    let argv = plan["argv"].as_array().unwrap();
    assert!(
        argv.iter()
            .any(|a| a == "--dangerously-bypass-approvals-and-sandbox")
    );
    assert!(!argv.iter().any(|a| a == "--sandbox"));
    assert_eq!(
        f.json("engineering", "ship.show", json!({"name":"engineering"}))["first_mate"]["options"]
            ["sandbox"],
        "workspace-write"
    );
    f.ok(None, &["permissions", "standard"]);
    assert!(!f.plan("liaison").to_string().contains("bypassPermissions"));
    assert!(
        f.plan("engineering")
            .to_string()
            .contains("workspace-write")
    );
    assert_eq!(fs::read(&settings).unwrap(), before);
    for name in ["liaison", "engineering"] {
        assert!(
            !f.json(name, "charter.show", json!({"id":name}))["actions"]
                .as_array()
                .unwrap()
                .contains(&json!("deploy_production"))
        );
    }
    let mode = fs::metadata(f.root.join("fleet.permissions.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0);
}

#[test]
fn agents_cannot_switch_fleet_mode_or_add_rules() {
    let f = Fleet::new();
    for args in [
        vec!["permissions", "yolo"],
        vec!["permissions", "allow", "Bash(*)"],
        vec!["permissions.set", "--json", "{\"mode\":\"yolo\"}"],
    ] {
        let o = f
            .command(Some("liaison"), &args)
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", "liaison")
            .output()
            .unwrap();
        assert!(!o.status.success());
    }
    assert!(!f.root.join("fleet.permissions.json").exists());
}

#[test]
fn attended_launches_receive_the_same_fleet_policy() {
    let f = Fleet::new();
    f.ok(None, &["permissions", "yolo"]);
    f.ok(
        Some("liaison"),
        &["start", "claude", "--", "--permission-mode", "plan"],
    );
    let args: Vec<String> =
        serde_json::from_slice(&fs::read(f.root.join("argv.json")).unwrap()).unwrap();
    assert!(
        args.windows(2)
            .any(|v| v == ["--permission-mode", "bypassPermissions"])
    );
    assert!(!args.contains(&"plan".into()));
    assert!(args.contains(&"Bash(git *)".into()));
    assert!(args.contains(&"Bash(gh *)".into()));
}

#[test]
fn native_permission_denial_becomes_one_question_and_pauses_automatic_retries() {
    let f = Fleet::new();
    f.json("liaison","mail.send",json!({"id":"request","from":"captain","to":"liaison","kind":"request","body":"do the task"}));
    let profile = f.json("liaison", "ship.show", json!({"name":"liaison"}));
    for id in ["blocked-one", "blocked-two"] {
        f.json("liaison","agent.start",json!({"id":id,"task_id":"liaison-duty","manifest":profile["first_mate"]["manifest"],"options":profile["first_mate"]["options"]}));
        assert_eq!(f.wait("liaison", id)["status"], "failed");
    }
    let mail = f.json("liaison", "mail.inbox", json!({"address":"captain"}));
    let messages = mail["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["kind"], "request");
    assert!(!mail.to_string().contains("PRIVATE_DO_NOT_MAIL"));
    let before = f
        .json("liaison", "job.list", json!({}))
        .as_array()
        .unwrap()
        .len();
    f.ok(Some("liaison"), &["serve", "--json", "{\"once\":true}"]);
    assert_eq!(
        f.json("liaison", "job.list", json!({}))
            .as_array()
            .unwrap()
            .len(),
        before
    );
    // A blocked outcome must not freeze fresh captain input. It earns one
    // new attempt, then pauses again rather than repeatedly spending inference.
    f.json("liaison","mail.send",json!({"id":"new-input","from":"captain","to":"liaison","kind":"request","body":"fresh guidance"}));
    f.ok(Some("liaison"), &["serve", "--json", "{\"once\":true}"]);
    let fresh = f.json("liaison", "job.list", json!({}));
    assert_eq!(fresh.as_array().unwrap().len(), before + 1);
    for j in fresh.as_array().unwrap() {
        f.wait("liaison", j["id"].as_str().unwrap());
    }
    f.ok(Some("liaison"), &["serve", "--json", "{\"once\":true}"]);
    assert_eq!(
        f.json("liaison", "job.list", json!({}))
            .as_array()
            .unwrap()
            .len(),
        before + 1
    );
    assert_eq!(
        f.json("liaison", "mail.inbox", json!({"address":"captain"}))["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    f.ok(None, &["permissions", "yolo"]);
    f.ok(Some("liaison"), &["serve", "--json", "{\"once\":true}"]);
    let jobs = f.json("liaison", "job.list", json!({}));
    assert_eq!(jobs.as_array().unwrap().len(), before + 2);
    for j in jobs.as_array().unwrap() {
        f.wait("liaison", j["id"].as_str().unwrap());
    }
}
