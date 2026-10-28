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
        // Leased workspaces are admission-gated on the volume the fixture's
        // ledger and pool sit on, so it must not land on a small tmpfs below
        // the default free-disk floor.
        let tmp = support::tempdir();
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
        // The free-disk floor is admission's concern, tested apart: this
        // fixture starts real jobs on whatever volume and space the shared
        // test machine has, so its own private ledger must not refuse them.
        f.json("resource.configure", json!({"min_free_disk_bytes":0}));
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
    fn lease(&self, folder: &Path, task: &str) -> Value {
        use std::os::unix::fs::PermissionsExt;
        let script = self.root.join("treehouse");
        fs::write(&script, include_str!("fixtures/treehouse.py")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let config_path = self.ship.join(".privateer/config.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        config["treehouse_binary"] = json!(script);
        fs::write(&config_path, config.to_string()).unwrap();
        self.json("charter.put", json!({"id":"workshop","min_free_bytes":0}));
        self.json(
            "task.create",
            json!({"id":task,"charter_id":"workshop","owner":"workshop","title":task}),
        );
        self.json("workspace.acquire", json!({"task_id":task,"cwd":folder}))
    }
    fn lease_plan(&self, lease: &Value, harness: &str) -> Value {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("plugins")
            .join(harness)
            .join("manifest.json");
        self.json("plugin.command", json!({"task_id":lease["task_id"],"manifest":manifest,"_workspace_lease_id":lease["lease_id"]}))
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
        // support::command clears inherited agent identity; only this isolated
        // operator process simulates a captain answering the phone decision.
        let out = self
            .command(BIN)
            .env("PRIVATEER_ROLE", "operator")
            .args([
                "mail.send",
                "--json",
                &json!({"from":"captain","to":"workshop","kind":"reply",
                    "correlation":ask["id"],"body":option,
                    "payload":{"answer":{"option_id":option}}})
                .to_string(),
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
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
    assert!(ask["body"].as_str().unwrap().contains("arbitrary commands"));
    assert!(
        ask["payload"]["decision"]["why"]
            .as_str()
            .unwrap()
            .contains("code execution")
    );
    assert!(!has(&f.plan(folder, "claude", None), BUILD));
    let forged = f.command(BIN).env("PRIVATEER_ROLE","agent").env("PRIVATEER_ACTOR","workshop")
        .args(["mail.send","--json",&json!({"from":"captain","to":"workshop","kind":"reply",
            "correlation":ask["id"],"body":"accept","payload":{"operator":true,"answer":{"option_id":"accept"}}}).to_string()])
        .output().unwrap();
    assert!(!forged.status.success());
    let refusal: Value = serde_json::from_slice(&forged.stderr)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&forged.stderr)));
    assert_eq!(
        refusal["error"], "agent workshop cannot act as \"captain\"",
        "{refusal}"
    );
    assert!(!has(&f.plan(folder, "claude", None), BUILD));
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
fn first_mate_wakes_errands_and_reviewers_never_receive_project_grants() {
    let f = Fleet::new();
    let added = f.add("engine", false);
    let folder = Path::new(added["folder"].as_str().unwrap());
    f.permission("engine", "accept", &[]);
    let conn = f.local_db();
    // Insert runtime identity fixtures, not caller-supplied role flags.
    conn.execute("INSERT INTO wakes(id,ship,focus,through_seq,job_id,mode,status,created_ms)
        VALUES('wake-fixture','workshop','task:workshop-duty',0,'wake-fixture','headless','running',0)",[]).unwrap();
    assert!(!has(&f.plan(folder, "claude", Some("wake-fixture")), BUILD));
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
fn real_leased_worktrees_receive_only_their_source_projects_grant() {
    let f = Fleet::new();
    let added = f.add("engine", false);
    let folder = Path::new(added["folder"].as_str().unwrap());
    let lease = f.lease(folder, "engine-crew");
    assert!(lease["lease_id"].is_string(), "{lease}");
    let worktree = Path::new(lease["path"].as_str().unwrap());
    assert!(!worktree.starts_with(folder));
    assert!(worktree.join(".git").is_file());
    assert!(!has(&f.lease_plan(&lease, "claude"), TEST));
    let codex_before = f.lease_plan(&lease, "codex");
    f.permission("engine", "accept", &[TEST]);
    assert!(has(&f.lease_plan(&lease, "claude"), TEST));
    assert!(!has(&f.lease_plan(&lease, "claude"), BUILD));
    assert_eq!(codex_before, f.lease_plan(&lease, "codex"));
    let other = f.add("other", false);
    let other_lease = f.lease(Path::new(other["folder"].as_str().unwrap()), "other-crew");
    assert!(!has(&f.lease_plan(&other_lease, "claude"), TEST));
    f.permission("engine", "clear", &[]);
    assert!(!has(&f.lease_plan(&lease, "claude"), TEST));
}

#[test]
fn deletion_recreation_and_inode_reuse_do_not_restore_consent() {
    use std::os::unix::fs::MetadataExt;
    let f = Fleet::new();
    let added = f.add("engine", false);
    let folder = Path::new(added["folder"].as_str().unwrap());
    // Use an empty root so deletion is real, not merely a rename.
    fs::rename(folder, folder.with_extension("old")).unwrap();
    fs::create_dir(folder).unwrap();
    f.permission("engine", "accept", &[TEST]);
    assert!(has(&f.plan(folder, "claude", None), TEST));
    std::thread::sleep(std::time::Duration::from_millis(10));
    fs::remove_dir(folder).unwrap();
    fs::create_dir(folder).unwrap();
    // Simulate inode reuse deterministically even on filesystems that assign
    // a fresh inode here: only the old recorded birth/ctime remains different.
    let meta = folder.metadata().unwrap();
    let conn = rusqlite::Connection::open(f.root.join("fleet.sqlite")).unwrap();
    conn.execute(
        "UPDATE project_permissions SET device=?1,inode=?2 WHERE project='workshop/engine'",
        [meta.dev().to_string(), meta.ino().to_string()],
    )
    .unwrap();
    assert!(!has(&f.plan(folder, "claude", None), TEST));
    assert_eq!(f.permission("engine", "show", &[])["accepted"], json!([]));
    f.permission("engine", "accept", &[TEST]);
    fs::remove_dir(folder).unwrap();
    assert!(!has(&f.plan(&f.ship, "claude", None), TEST));
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM project_permissions WHERE project='workshop/engine'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "observed removal must retire the row");
    fs::create_dir(folder).unwrap();
    assert!(!has(&f.plan(folder, "claude", None), TEST));
}

#[test]
fn old_registry_grants_need_explicit_renewal_after_identity_migration() {
    let f = Fleet::new();
    let added = f.add("engine", false);
    let folder = Path::new(added["folder"].as_str().unwrap());
    f.permission("engine", "accept", &[TEST]);
    let conn = rusqlite::Connection::open(f.root.join("fleet.sqlite")).unwrap();
    conn.execute(
        "ALTER TABLE project_permissions DROP COLUMN identity_time",
        [],
    )
    .unwrap();
    assert!(!has(&f.plan(folder, "claude", None), TEST));
    f.permission("engine", "accept", &[TEST]);
    assert!(has(&f.plan(folder, "claude", None), TEST));
}

#[test]
fn project_show_prints_the_recorded_change_time_fallback_warning() {
    let f = Fleet::new();
    f.add("engine", false);
    let conn = rusqlite::Connection::open(f.root.join("fleet.sqlite")).unwrap();
    // Simulate a registry row accepted on a filesystem without birth time.
    conn.execute(
        "UPDATE project_permissions SET identity_time='change:1:2' WHERE project='workshop/engine'",
        [],
    )
    .unwrap();
    let shown = f.permission("engine", "show", &[]);
    let note = shown["identity_note"].as_str().unwrap();
    assert!(note.contains("Birth time is unavailable"));
    assert!(note.contains("Creating, removing or renaming"));
    assert!(note.contains("clears consent"));
    assert!(note.contains("fails closed"));
    conn.execute(
        "UPDATE project_permissions SET identity_time='birth:1:2' WHERE project='workshop/engine'",
        [],
    )
    .unwrap();
    assert!(f.permission("engine", "show", &[])["identity_note"].is_null());
}

#[test]
fn project_registry_errors_fail_closed_on_grants_not_on_launches() {
    let f = Fleet::new();
    let added = f.add("engine", false);
    let folder = Path::new(added["folder"].as_str().unwrap());
    f.permission("engine", "accept", &[TEST]);
    let conn = rusqlite::Connection::open(f.root.join("fleet.sqlite")).unwrap();
    conn.execute("UPDATE project_permissions SET accepted='broken JSON'", [])
        .unwrap();
    assert!(!has(&f.plan(folder, "claude", None), TEST));
    conn.execute_batch(
        "ALTER TABLE project_permissions RENAME TO saved_project_permissions;
        CREATE TABLE project_permissions(project TEXT, identity_time TEXT);",
    )
    .unwrap();
    assert!(!has(&f.plan(folder, "claude", None), TEST));
}

#[test]
fn project_add_cli_prints_ungranted_proposal_and_phone_decline_preserves_defaults() {
    let f = Fleet::new();
    let output = f.ok(&["project", "add", f.source.to_str().unwrap(), "engine"]);
    assert!(output.contains(BUILD));
    assert!(output.contains("not yet allowed"));
    assert!(output.contains("Claude crews only"));
    assert!(output.contains("arbitrary commands"));
    assert!(output.contains("privateer permissions project workshop/engine accept"));
    assert_eq!(f.permission("engine", "show", &[])["accepted"], json!([]));
    f.add("second", true);
    let ask = f.ask();
    f.reply(&ask, "decline");
    assert_eq!(f.permission("second", "show", &[])["state"], "decline");
    assert_eq!(f.permission("second", "show", &[])["accepted"], json!([]));
}
