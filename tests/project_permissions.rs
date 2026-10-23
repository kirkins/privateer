mod support;

use privateer::project_permissions::detect;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");
const BUILD: &str = "Bash(cargo build *)";
const TEST: &str = "Bash(cargo test *)";

#[test]
fn manifest_detection_is_literal_bounded_and_never_executes_project_code() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let marker = root.join("EXECUTED");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='sample'\nversion='0.1.0'\nbuild='build.rs'\n",
    )
    .unwrap();
    fs::write(
        root.join("build.rs"),
        format!(
            "fn main() {{ std::fs::write({:?}, \"ran\").unwrap(); }}",
            marker
        ),
    )
    .unwrap();
    fs::write(
        root.join("package.json"),
        json!({"scripts":{
            "build":format!("touch {}", marker.display()),"test":"exit 1","install":"sudo forbidden"
        }})
        .to_string(),
    )
    .unwrap();
    fs::write(
        root.join("Makefile"),
        format!(
            "$(shell touch {})\ninclude untrusted.mk\nbuild test: ; touch {}\n\tdeploy: ignored\n",
            marker.display(),
            marker.display()
        ),
    )
    .unwrap();
    let found = detect(root);
    assert_eq!(
        found,
        vec![
            BUILD,
            "Bash(cargo check *)",
            TEST,
            "Bash(cargo fmt *)",
            "Bash(cargo clippy *)",
            "Bash(npm run build *)",
            "Bash(npm test *)",
            "Bash(make build *)",
            "Bash(make test *)"
        ]
    );
    assert!(!marker.exists());
    fs::remove_file(root.join("Cargo.toml")).unwrap();
    fs::write(
        root.join("package.json"),
        r#"{"scripts":{"build":"","check":"npm install"}}"#,
    )
    .unwrap();
    fs::write(
        root.join("Makefile"),
        "# build: comment\n\ttest: recipe\nbuild := variable\n%: pattern\n$(eval test:):\n",
    )
    .unwrap();
    assert!(detect(root).is_empty());
    fs::write(root.join("package.json"), "malformed").unwrap();
    assert!(detect(root).is_empty());
    fs::write(root.join("Cargo.toml"), vec![b'x'; 1_048_577]).unwrap();
    assert!(detect(root).is_empty());
    fs::remove_file(root.join("Cargo.toml")).unwrap();
    std::os::unix::fs::symlink(root.join("build.rs"), root.join("Cargo.toml")).unwrap();
    assert!(detect(root).is_empty());
    assert!(!marker.exists());
}

struct Fleet {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    ship: PathBuf,
    source: PathBuf,
}
impl Fleet {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let ship = root.join("workshop");
        fs::create_dir(&ship).unwrap();
        let source = root.join("source");
        fs::create_dir(&source).unwrap();
        let f = Self {
            _tmp: tmp,
            root,
            ship,
            source,
        };
        f.ok(&["init", "--ship", "workshop", "--harness", "claude"]);
        for args in [
            vec!["init", "--quiet"],
            vec!["add", "."],
            vec!["commit", "--quiet", "-m", "fixture"],
        ] {
            if args[0] == "add" {
                fs::write(f.source.join("Cargo.toml"), "[workspace]\n").unwrap();
            }
            let out = f
                .command("git")
                .current_dir(&f.source)
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        f
    }
    fn command(&self, bin: &str) -> Command {
        let mut c = support::command(bin);
        c.current_dir(&self.ship)
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("PRIVATEER_FLEET", self.root.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env("PRIVATEER_SERVICE", "none")
            .env(
                "PRIVATEER_PLUGINS",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins"),
            )
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        c
    }
    fn ok(&self, args: &[&str]) -> String {
        let out = self.command(BIN).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
    fn json(&self, command: &str, args: Value) -> Value {
        serde_json::from_str(&self.ok(&[command, "--json", &args.to_string()])).unwrap()
    }
    fn add(&self, name: &str, agent: bool) -> Value {
        let mut c = self.command(BIN);
        if agent {
            c.env("PRIVATEER_ROLE", "agent")
                .env("PRIVATEER_ACTOR", "workshop");
        }
        let out = c
            .args([
                "project.add",
                "--json",
                &json!({"url":self.source,"name":name}).to_string(),
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(value.get("tool_consent_error").is_none(), "{value}");
        value
    }
    fn permission(&self, name: &str, action: &str, rules: &[&str]) -> Value {
        let mut args = vec!["permissions", "project", name, action];
        args.extend_from_slice(rules);
        serde_json::from_str(&self.ok(&args)).unwrap()
    }
    fn plan(&self, folder: &Path, harness: &str, id: Option<&str>) -> Value {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("plugins")
            .join(harness)
            .join("manifest.json");
        let mut args =
            json!({"task_id":"workshop-duty","manifest":manifest,"_workspace_cwd":folder});
        if let Some(id) = id {
            args["id"] = json!(id);
        }
        self.json("plugin.command", args)
    }
    fn ask(&self) -> Value {
        self.json("mail.inbox", json!({"address":"captain"}))["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["kind"] == "decision" && m["state"] == "open")
            .unwrap()
            .clone()
    }
    fn reply(&self, ask: &Value, option: &str) -> Value {
        self.json(
            "mail.send",
            json!({"from":"captain","to":"workshop","kind":"reply",
            "correlation":ask["id"],"body":option,"payload":{"answer":{"option_id":option}}}),
        )
    }
    fn local_db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.ship.join(".privateer/privateer.sqlite")).unwrap()
    }
}
fn has(plan: &Value, rule: &str) -> bool {
    plan["argv"]
        .as_array()
        .unwrap()
        .iter()
        .any(|arg| arg == rule)
}
fn assert_refused(out: Output) {
    assert!(
        !out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("operator"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn consent_is_opt_in_editable_captain_only_and_scoped_to_claude_launch_cwd() {
    let f = Fleet::new();
    let added = f.add("engine", false);
    let folder = Path::new(added["folder"].as_str().unwrap());
    assert_eq!(added["tool_consent"]["accepted"], json!([]));
    assert!(!has(&f.plan(folder, "claude", None), BUILD));
    let plain = f.plan(folder, "codex", None);
    let accepted = f.permission("engine", "accept", &[TEST]);
    assert_eq!(accepted["accepted"], json!([TEST]));
    let plan = f.plan(folder, "claude", None);
    assert!(has(&plan, TEST));
    assert!(!has(&plan, BUILD));
    fs::create_dir(folder.join("nested")).unwrap();
    assert!(has(&f.plan(&folder.join("nested"), "claude", None), TEST));
    assert!(!has(&f.plan(&f.ship, "claude", None), TEST));
    let other = f.add("other", false);
    assert!(!has(
        &f.plan(Path::new(other["folder"].as_str().unwrap()), "claude", None),
        TEST
    ));
    assert_eq!(plain, f.plan(folder, "codex", None));
    let invalid = f
        .command(BIN)
        .args(["permissions", "project", "engine", "accept", "Bash(*)"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    for action in ["accept", "clear", "decline"] {
        assert_refused(
            f.command(BIN)
                .env("PRIVATEER_ROLE", "agent")
                .env("PRIVATEER_ACTOR", "workshop")
                .args(["permissions", "project", "engine", action])
                .output()
                .unwrap(),
        );
    }
    assert_eq!(
        f.permission("engine", "show", &[])["accepted"],
        json!([TEST])
    );
    f.permission("engine", "clear", &[]);
    assert!(!has(&f.plan(folder, "claude", None), TEST));
    f.permission("engine", "accept", &[]);
    assert!(has(&f.plan(folder, "claude", None), BUILD));
    f.permission("engine", "decline", &[]);
    assert!(!has(&f.plan(folder, "claude", None), BUILD));
    // A directory replaced at the same path must not inherit its predecessor's grant.
    f.permission("engine", "accept", &[]);
    fs::rename(folder, folder.with_extension("old")).unwrap();
    fs::create_dir(folder).unwrap();
    assert!(!has(&f.plan(folder, "claude", None), TEST));
}

#[test]
fn agent_add_sends_a_phone_decision_and_durable_reply_recovery_respects_clear() {
    let f = Fleet::new();
    let added = f.add("engine", true);
    let folder = Path::new(added["folder"].as_str().unwrap());
    let ask = f.ask();
    assert!(ask["body"].as_str().unwrap().contains(BUILD));
    assert!(!has(&f.plan(folder, "claude", None), BUILD));
    assert_refused(f.command(BIN).env("PRIVATEER_ROLE","agent").env("PRIVATEER_ACTOR","workshop")
        .args(["mail.send","--json",&json!({"from":"captain","to":"workshop","kind":"reply",
            "correlation":ask["id"],"body":"accept","payload":{"operator":true,"answer":{"option_id":"accept"}}}).to_string()])
        .output().unwrap());
    let reply = f.reply(&ask, "accept");
    assert_eq!(reply["payload"]["operator"], true);
    assert!(has(&f.plan(folder, "claude", None), BUILD));
    // Simulate a crash after the durable mail commit and before policy application.
    let conn = rusqlite::Connection::open(f.root.join("fleet.sqlite")).unwrap();
    conn.execute("UPDATE project_permissions SET accepted='[]',state='pending',ask=?1 WHERE project='workshop/engine'",
        [ask["id"].as_str().unwrap()]).unwrap();
    assert!(has(&f.plan(folder, "claude", None), BUILD));
    f.permission("engine", "clear", &[]);
    assert!(!has(&f.plan(folder, "claude", None), BUILD));
    let second = f.add("second", true);
    let ask = f.ask();
    f.permission("second", "clear", &[]);
    f.reply(&ask, "accept");
    assert!(!has(
        &f.plan(
            Path::new(second["folder"].as_str().unwrap()),
            "claude",
            None
        ),
        BUILD
    ));
}

#[test]
fn first_mate_wakes_receive_grants_but_errands_and_reviewers_never_do() {
    let f = Fleet::new();
    let added = f.add("engine", false);
    let folder = Path::new(added["folder"].as_str().unwrap());
    f.permission("engine", "accept", &[]);
    let conn = f.local_db();
    // Insert runtime identity fixtures, not caller-supplied role flags.
    conn.execute("INSERT INTO wakes(id,ship,focus,through_seq,job_id,mode,status,created_ms)
        VALUES('wake-fixture','workshop','task:workshop-duty',0,'wake-fixture','headless','running',0)",[]).unwrap();
    assert!(has(&f.plan(folder, "claude", Some("wake-fixture")), BUILD));
    conn.execute("INSERT INTO errands(id,ship,focus,task_id,brief,mode,status,created_ms) VALUES('errand-fixture','workshop','task:workshop-duty','workshop-duty','review fixture','headless','running',0)",[]).unwrap();
    assert!(!has(
        &f.plan(folder, "claude", Some("errand-fixture")),
        BUILD
    ));
    conn.execute(
        "INSERT INTO reviews(id,ship,task_id,forge,repo,pr,base_ref,base_sha,
        head_ref,head_sha,head_tree,errand_id,route_name,route_identity,route_model,route_tier,
        route_uses,route_why,fresh,authors,declared,folder,clone,verdict,post_state,created_ms,kind)
        VALUES('review-fixture','workshop','workshop-duty','github','example/project',1,
        'main','base','feature','head','tree','errand-fixture','reviewer','fixture','model',1,
        '[\"review\"]','fixture',1,'{}','[]',?1,?1,'running','pending',0,'errand')",
        [folder.to_str().unwrap()],
    )
    .unwrap();
    assert!(!has(
        &f.plan(folder, "claude", Some("errand-fixture")),
        BUILD
    ));
    conn.execute(
        "UPDATE reviews SET kind='gate' WHERE id='review-fixture'",
        [],
    )
    .unwrap();
    let reviewer = f.plan(folder, "claude", Some("errand-fixture"));
    assert!(!has(&reviewer, BUILD));
    assert!(!has(&reviewer, "Bash(git *)"));
}

#[test]
fn project_add_cli_prints_ungranted_proposal_and_phone_decline_preserves_defaults() {
    let f = Fleet::new();
    let output = f.ok(&["project", "add", f.source.to_str().unwrap(), "engine"]);
    assert!(output.contains(BUILD));
    assert!(output.contains("not yet allowed"));
    assert!(output.contains("privateer permissions project workshop/engine accept"));
    assert_eq!(f.permission("engine", "show", &[])["accepted"], json!([]));
    f.add("second", true);
    let ask = f.ask();
    f.reply(&ask, "decline");
    assert_eq!(f.permission("second", "show", &[])["state"], "decline");
    assert_eq!(f.permission("second", "show", &[])["accepted"], json!([]));
}
