mod support;

use privateer::{
    dispatch,
    store::{Caller, Store},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

fn registry(root: &Path) -> Value {
    let skill = root.join("fixture");
    fs::create_dir_all(&skill).unwrap();
    let text = "---\nname: fixture\ndescription: Fixture instructions, loaded only when needed.\n---\nUse the fixture client.\n";
    fs::write(skill.join("SKILL.md"), text).unwrap();
    let hash = format!("{:x}", Sha256::digest(text));
    let tree = format!("{:x}", Sha256::digest(format!("SKILL.md\0{hash}\n")));
    json!({"schema_version":1,"pi_version":"1.2.0",
        "integrations":{"reviewed":true,"extensions":[]},
        "bundles":[{"ref":"fixture@1","roles":["crew","first_mate"],
            "skills":[{"name":"fixture","root":skill,"sha256":tree}]}]})
}
fn fixture() -> (TempDir, Store, Value) {
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let repo = root.join("repo");
    fs::create_dir(&repo).unwrap();
    let store = Store::open(&root.join("home")).unwrap();
    store.connection().execute("INSERT INTO charters(id,data) VALUES('charter',?1)",
        [json!({"id":"charter","active":true,"repo":repo,"actions":["spawn","implement","message"],"job_timeout_secs":10,"min_free_bytes":0,"max_heavy_jobs":1}).to_string()]).unwrap();
    store.connection().execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('task','charter','ship','bounded task','open',0)", []).unwrap();
    let config = registry(&root);
    (temp, store, config)
}
fn plan(store: &mut Store, selected: Option<Value>) -> anyhow::Result<Value> {
    let mut args = json!({"task_id":"task","manifest":"bundled:pi","options":{"provider":"chosen","model":"exact"}});
    if let Some(selected) = selected {
        args["tool_bundles"] = selected;
    }
    dispatch(store, "plugin.command", args)
}
fn selected_plan(value: &Value) -> Value {
    let argv = value["argv"].as_array().unwrap();
    let index = argv.iter().position(|a| a == "--tool-bundles").unwrap();
    serde_json::from_str(argv[index + 1].as_str().unwrap()).unwrap()
}

#[test]
fn deterministic_python_bundle_validation() {
    for suite in ["tests/pi_bundles.py", "tests/claude_bundles.py"] {
        let status = Command::new("python3")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join(suite))
            .status()
            .unwrap();
        assert!(status.success(), "{suite}");
    }
}

#[test]
fn operator_registry_explicit_empty_legacy_and_plan_boundaries() {
    let (_temp, mut store, config) = fixture();
    assert!(
        plan(&mut store, None).is_ok(),
        "omitted selection preserves legacy without a registry"
    );
    assert!(
        plan(&mut store, Some(json!([]))).is_err(),
        "[] is bounded and requires approval"
    );
    let configured = dispatch(&mut store, "bundle.configure", json!({"registry":config})).unwrap();
    assert_eq!(configured["refs"], json!(["fixture@1"]));
    let empty = selected_plan(&plan(&mut store, Some(json!([]))).unwrap());
    assert_eq!(empty["skills"], json!([]));
    let selected = plan(&mut store, Some(json!(["fixture@1"]))).unwrap();
    assert_eq!(selected_plan(&selected)["skills"][0]["name"], "fixture");
    let argv = selected["argv"].as_array().unwrap();
    assert!(
        argv.windows(2)
            .any(|p| p[0] == "--provider" && p[1] == "chosen")
    );
    assert!(
        argv.windows(2)
            .any(|p| p[0] == "--model" && p[1] == "exact")
    );
    assert!(
        argv.last()
            .unwrap()
            .as_str()
            .unwrap()
            .contains("Privateer control protocol v1")
    );
    assert_eq!(selected["env_names"], json!([]));
    assert!(plan(&mut store, Some(json!(["unknown@1"]))).is_err());
    assert!(plan(&mut store, Some(json!(["fixture@1", "fixture@1"]))).is_err());
    assert!(
        dispatch(
            &mut store,
            "plugin.command",
            json!({"task_id":"task","manifest":"bundled:codex","tool_bundles":[]})
        )
        .is_err()
    );
    assert!(
        dispatch(
            &mut store,
            "plugin.command",
            json!({"task_id":"task","manifest":"bundled:pi","options":{"tool_bundles":[]}})
        )
        .is_err()
    );
}

#[test]
fn registry_cannot_be_retargeted_even_after_removal_and_mutation_refuses_launch() {
    let (_temp, mut store, config) = fixture();
    dispatch(&mut store, "bundle.configure", json!({"registry":config})).unwrap();
    let mut changed = config.clone();
    changed["bundles"][0]["roles"] = json!(["crew"]);
    assert!(dispatch(&mut store, "bundle.configure", json!({"registry":changed})).is_err());
    let mut removed = config.clone();
    removed["bundles"] = json!([]);
    dispatch(&mut store, "bundle.configure", json!({"registry":removed})).unwrap();
    assert!(dispatch(&mut store, "bundle.configure", json!({"registry":changed})).is_err());
    dispatch(&mut store, "bundle.configure", json!({"registry":config})).unwrap();
    let path = config["bundles"][0]["skills"][0]["root"].as_str().unwrap();
    fs::write(Path::new(path).join("SKILL.md"), "changed").unwrap();
    assert!(plan(&mut store, Some(json!(["fixture@1"]))).is_err());
    assert!(
        plan(&mut store, Some(json!([]))).is_err(),
        "registry drift is not silently ignored"
    );
}

#[test]
fn registry_is_per_home_and_an_agent_cannot_approve_it() {
    let (temp, mut store, config) = fixture();
    dispatch(&mut store, "bundle.configure", json!({"registry":config})).unwrap();
    let mut other = Store::open(&temp.path().join("other")).unwrap();
    assert!(
        dispatch(
            &mut other,
            "bundle.configure",
            json!({"registry":{"secret":"sentinel"}})
        )
        .is_err()
    );
    let role = Caller::Agent {
        actor: "ship".into(),
    };
    let mut agent = Store::open_as(store.home(), role).unwrap();
    let error = dispatch(&mut agent, "bundle.configure", json!({"registry":config})).unwrap_err();
    assert!(error.to_string().contains("operator command"));
    assert!(plan(&mut agent, Some(json!(["fixture@1"]))).is_ok());
}

#[test]
fn real_agent_start_nested_crew_replay_and_explicit_empty_do_not_inherit() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};
    let (temp, mut store, config) = fixture();
    let root = temp.path();
    fs::write(
        store.home().join("config.json"),
        json!({"resource_home":root.join("ledger")}).to_string(),
    )
    .unwrap();
    dispatch(&mut store, "bundle.configure", json!({"registry":config})).unwrap();
    dispatch(
        &mut store,
        "ship.put",
        json!({"name":"ship","first_mate":{
        "manifest":"bundled:pi","duty_task":"task","tool_bundles":["fixture@1"],
        "options":{"provider":"chosen","model":"exact"}}}),
    )
    .unwrap();
    dispatch(
        &mut store,
        "task.create",
        json!({"id":"nested","parent":"task","owner":"ship","title":"nested crew"}),
    )
    .unwrap();
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    let fake = bin.join("pi");
    let cli = json!([env!("CARGO_BIN_EXE_privateer"), "--home", store.home()]);
    let log = root.join("native-argv.jsonl");
    fs::write(&fake, format!(r#"#!{}
import json,subprocess,sys,time,uuid
with open({},'a') as f: f.write(json.dumps(sys.argv[1:])+'\n')
if any('Task task:' in a for a in sys.argv):
    out=subprocess.run({} + ['agent.start','--json',json.dumps({{'id':'nested-job','task_id':'nested'}})],capture_output=True,text=True)
    if out.returncode: sys.exit(3)
print(json.dumps({{'type':'session','id':str(uuid.uuid4())}}),flush=True)
print(json.dumps({{'type':'message_end','message':{{'role':'assistant','stopReason':'stop'}}}}),flush=True)
print(json.dumps({{'type':'agent_settled','aborted':False}}),flush=True)
"#, support::python().display(), json!(log), cli)).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let run = |command: &str, args: Value| -> Value {
        let output = support::command(env!("CARGO_BIN_EXE_privateer"))
            .args([
                "--home",
                store.home().to_str().unwrap(),
                command,
                "--json",
                &args.to_string(),
            ])
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("PRIVATEER_RESOURCE_HOME", root.join("ledger"))
            .env("PRIVATEER_FLEET", root.join("fleet.sqlite"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let wait = |id: &str| -> Value {
        let start = Instant::now();
        loop {
            let job = run("job.status", json!({"id":id}));
            if matches!(
                job["status"].as_str(),
                Some("succeeded" | "failed" | "cancelled")
            ) {
                break job;
            }
            assert!(start.elapsed() < Duration::from_secs(9));
            std::thread::sleep(Duration::from_millis(25));
        }
    };
    let args = json!({"id":"legacy-job","task_id":"task"});
    run("agent.start", args.clone());
    let legacy = wait("legacy-job");
    assert_eq!(legacy["status"], "succeeded");
    let nested = wait("nested-job");
    assert_eq!(nested["status"], "succeeded");
    assert_eq!(nested["started_by"], "legacy-job");
    for job in [&legacy, &nested] {
        let command = job["command"].as_array().unwrap();
        assert!(!command.iter().any(|v| v == "--tool-bundles"));
        assert!(
            command
                .windows(2)
                .any(|p| p[0] == "--provider" && p[1] == "chosen")
        );
        assert!(
            command
                .windows(2)
                .any(|p| p[0] == "--model" && p[1] == "exact")
        );
    }
    let replay = run("agent.start", args);
    assert_eq!(replay["incarnation"], legacy["incarnation"]);
    assert!(
        run(
            "agent.start",
            json!({"id":"empty-job","task_id":"nested","tool_bundles":[]})
        )["command"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "--tool-bundles")
    );
    // The activation wrapper refuses this deliberately unsupported native
    // binary before it runs: explicit [] is not silently legacy.
    assert_eq!(wait("empty-job")["exit_code"], 2);
    assert_eq!(fs::read_to_string(log).unwrap().lines().count(), 2);
}

#[test]
fn first_mate_selection_is_not_an_inherited_harness_option() {
    let (_temp, mut store, config) = fixture();
    dispatch(&mut store, "bundle.configure", json!({"registry":config})).unwrap();
    let profile = json!({"manifest":"bundled:pi","duty_task":"task","options":{"provider":"chosen","model":"exact"},"tool_bundles":["fixture@1"]});
    let validated = privateer::activation::profile(&store, "ship", &profile).unwrap();
    assert_eq!(validated["tool_bundles"], json!(["fixture@1"]));
    assert!(validated["options"].get("tool_bundles").is_none());
    // Every ordinary/nested/sibling plan is explicit: profile approval does
    // not mutate registry defaults or inject resources into later plans.
    let legacy = plan(&mut store, None).unwrap();
    assert!(
        !legacy["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "--tool-bundles")
    );
    let explicit = selected_plan(&plan(&mut store, Some(json!([]))).unwrap());
    assert_eq!(explicit["skills"], json!([]));
}

#[test]
fn claude_plan_carries_allowlist_and_resumes_only_under_its_digest() {
    let (temp, mut store, mut config) = fixture();
    config["claude_version"] = json!("2.1.300");
    dispatch(&mut store, "bundle.configure", json!({"registry":config})).unwrap();
    // Planning reads the user Claude settings: never the real ones.
    let root = temp.path().canonicalize().unwrap();
    let claude = root.join("claude-config");
    fs::create_dir(&claude).unwrap();
    let settings = |model: &str| {
        let value =
            json!({"model":model,"hooks":{"SessionStart":[]},"enabledPlugins":{"p@m":true}});
        fs::write(claude.join("settings.json"), value.to_string()).unwrap();
    };
    settings("exact");
    let home = store.home().to_owned();
    // The bundle adapter runs python3 from PATH: resolve it before HOME moves.
    let path = format!(
        "{}:/usr/bin:/bin",
        support::python().parent().unwrap().display()
    );
    let run = |command: &str, args: Value| -> Result<Value, String> {
        let output = support::command(support::BIN)
            .args(["--home", home.to_str().unwrap(), command, "--json"])
            .arg(args.to_string())
            .env("HOME", &root)
            .env("PATH", &path)
            .env("CLAUDE_CONFIG_DIR", &claude)
            .env("XDG_CONFIG_HOME", root.join("xdg"))
            .env("XDG_STATE_HOME", root.join("xdg-state"))
            .env("PRIVATEER_FLEET", root.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", root.join("ledger"))
            .output()
            .unwrap();
        if output.status.success() {
            Ok(serde_json::from_slice(&output.stdout).unwrap())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).into_owned())
        }
    };
    run(
        "ship.put",
        json!({"name":"ship","first_mate":{"manifest":"bundled:claude","duty_task":"task",
            "tool_bundles":["fixture@1"],"options":{}}}),
    )
    .unwrap();
    let wake = |id: &str, session: Option<&str>| {
        store.connection().execute(
            "INSERT INTO wakes(id,ship,focus,through_seq,job_id,mode,status,created_ms,session_id)
             VALUES(?1,'ship','duty',0,?1,'fresh','running',0,?2)",
            rusqlite::params![id, session],
        )
        .unwrap();
    };
    let launch = |id: &str, resume: Option<&str>| {
        let mut args = json!({"id":id,"task_id":"task","manifest":"bundled:claude",
            "tool_bundles":["fixture@1"],"options":{"model":"exact"}});
        if let Some(session) = resume {
            args["options"]["resume"] = json!(session);
        }
        run("plugin.command", args)
    };
    let legacy = run(
        "plugin.command",
        json!({"task_id":"task","manifest":"bundled:claude","options":{"model":"exact"}}),
    )
    .unwrap();
    assert!(
        !legacy["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "--tool-bundles")
    );
    wake("wake-1", None);
    let plan = selected_plan(&launch("wake-1", None).unwrap());
    assert_eq!(plan["claude_version"], "2.1.300");
    assert_eq!(plan["skills"][0]["name"], "fixture");
    assert_eq!(plan["settings"], json!({"model":"exact"}));
    let digest: String = store
        .connection()
        .query_row("SELECT plan_digest FROM wakes WHERE id='wake-1'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(digest.len(), 64);
    // The session wake-1 started, as recorded when it succeeded.
    let session = "00000000-0000-4000-8000-000000000001";
    store.connection().execute(
        "INSERT INTO sessions(ship,focus,harness,session_id,wakes,started_ms,last_turn_ms,plan_digest)
         VALUES('ship','duty','claude',?1,1,0,0,?2)",
        rusqlite::params![session, digest],
    )
    .unwrap();
    wake("wake-2", Some(session));
    let resumed = launch("wake-2", Some(session)).unwrap();
    assert!(
        resumed["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == session)
    );
    assert!(
        launch("crew-job", Some(session)).is_err(),
        "crew never resume a bounded session"
    );
    // A changed carried setting is a different plan: start fresh instead.
    settings("other");
    wake("wake-3", Some(session));
    assert!(launch("wake-3", Some(session)).is_err());
    assert!(launch("wake-3", None).is_ok());
}
