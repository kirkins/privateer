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
fn real_agent_start_crews_do_not_inherit_bundles_and_cannot_nest() {
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
        json!({"id":"empty","parent":"task","owner":"ship","title":"explicit empty bundles"}),
    )
    .unwrap();
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    let fake = bin.join("pi");
    let cli = json!([env!("CARGO_BIN_EXE_privateer"), "--home", store.home()]);
    let log = root.join("native-argv.jsonl");
    let refusal = root.join("nested-start.json");
    fs::write(&fake, format!(r#"#!{}
import json,subprocess,sys,time,uuid
with open({},'a') as f: f.write(json.dumps(sys.argv[1:])+'\n')
if any('Task task:' in a for a in sys.argv):
    out=subprocess.run({} + ['agent.start','--json',json.dumps({{'id':'nested-job','task_id':'empty'}})],capture_output=True,text=True)
    with open({},'w') as f: f.write(json.dumps({{'returncode':out.returncode,'stderr':out.stderr}}))
print(json.dumps({{'type':'session','id':str(uuid.uuid4())}}),flush=True)
print(json.dumps({{'type':'message_end','message':{{'role':'assistant','stopReason':'stop'}}}}),flush=True)
print(json.dumps({{'type':'agent_settled','aborted':False}}),flush=True)
"#, support::python().display(), json!(log), cli, json!(refusal))).unwrap();
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
    // The crew's memory is admission's concern, tested apart, so keep it tiny
    // here.
    run(
        "resource.configure",
        json!({"min_available_memory_bytes":0,"crew_memory_bytes":{"pi":1u64 << 20}}),
    );
    let args = json!({"id":"legacy-job","task_id":"task"});
    run("agent.start", args.clone());
    let legacy = wait("legacy-job");
    assert_eq!(legacy["status"], "succeeded");
    // A crew cannot start a crew: the nested start is refused as the first
    // mate's command, and the crew succeeds without it.
    let refused: Value = serde_json::from_str(&fs::read_to_string(refusal).unwrap()).unwrap();
    assert_ne!(refused["returncode"], 0);
    assert!(
        refused["stderr"]
            .as_str()
            .unwrap()
            .contains("agent.start is the first mate's, not crew's")
    );
    assert!(
        !run("job.list", json!({}))["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|j| j["id"] == "nested-job")
    );
    let command = legacy["command"].as_array().unwrap();
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
    let replay = run("agent.start", args);
    assert_eq!(replay["incarnation"], legacy["incarnation"]);
    assert!(
        run(
            "agent.start",
            json!({"id":"empty-job","task_id":"empty","tool_bundles":[]})
        )["command"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "--tool-bundles")
    );
    // The activation wrapper refuses this deliberately unsupported native
    // binary before it runs: explicit [] is not silently legacy.
    assert_eq!(wait("empty-job")["exit_code"], 2);
    assert_eq!(fs::read_to_string(log).unwrap().lines().count(), 1);
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

#[test]
fn a_bounded_claude_first_mate_resumes_its_session_until_the_plan_changes() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};
    let (temp, mut store, mut config) = fixture();
    config["claude_version"] = json!("2.1.300");
    dispatch(&mut store, "bundle.configure", json!({"registry":config})).unwrap();
    let root = temp.path().canonicalize().unwrap();
    let home = store.home().to_owned();
    let claude = root.join("claude-config");
    fs::create_dir(&claude).unwrap();
    let settings = |model: &str| {
        fs::write(
            claude.join("settings.json"),
            json!({"model":model,"theme":"dark"}).to_string(),
        )
        .unwrap();
    };
    settings("exact");
    // The loader refuses a #! wrapper, so the fake claude is a compiled exec
    // shim into a scripted turn: it acknowledges its wake's mail and reports
    // the session it was given, or a new one, as Claude's stream-json does.
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    let python = support::python();
    let log = root.join("claude-argv.jsonl");
    let cli = json!([support::BIN, "--home", home]);
    let script = root.join("claude.py");
    fs::write(&script, format!(r#"import json, re, subprocess, sys, uuid
args = sys.argv[1:]
if args == ["--version"]:
    print("2.1.300 (Claude Code)")
    sys.exit(0)
if args == ["--help"]:
    print("Options:\n  --setting-sources <sources>  Comma-separated list of setting sources\n  --strict-mcp-config  Only use MCP servers from --mcp-config\n  --settings <file-or-json>  Path to a settings JSON file\n  --plugin-dir <path>  Load a plugin from a directory\n  -p, --print  Print response and exit")
    sys.exit(0)
with open({}, "a") as f:
    f.write(json.dumps(args) + "\n")
flags = args[:args.index("--")]
session = flags[flags.index("--resume") + 1] if "--resume" in flags else str(uuid.uuid4())
print(json.dumps({{"type": "system", "subtype": "init", "session_id": session}}), flush=True)
through = int(re.search(r"through seq (\d+)", args[-1]).group(1))
subprocess.run({} + ["mail.ack", "--json", json.dumps({{"address": "ship", "through_seq": through}})],
               check=True, capture_output=True)
print(json.dumps({{"type": "result", "subtype": "success", "is_error": False, "session_id": session}}), flush=True)
"#, json!(log), cli)).unwrap();
    let shim = root.join("claude.c");
    fs::write(
        &shim,
        format!(
            r#"#include <unistd.h>
int main(int argc, char **argv) {{
    char *args[argc + 2];
    args[0] = {python:?};
    args[1] = {script:?};
    for (int i = 1; i <= argc; i++) args[i + 1] = argv[i];
    execv(args[0], args);
    return 127;
}}
"#,
            python = python.display().to_string(),
            script = script.display().to_string()
        ),
    )
    .unwrap();
    let built = Command::new("cc")
        .arg("-o")
        .arg(bin.join("claude"))
        .arg(&shim)
        .status()
        .unwrap();
    assert!(built.success());
    fs::set_permissions(bin.join("claude"), fs::Permissions::from_mode(0o700)).unwrap();
    let path = format!(
        "{}:{}:/usr/bin:/bin",
        bin.display(),
        python.parent().unwrap().display()
    );
    let run = |command: &str, args: Value| -> Value {
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
            .env("PRIVATEER_SERVICE", "none")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    run(
        "ship.put",
        json!({"name":"ship","first_mate":{"manifest":"bundled:claude","duty_task":"task",
            "tool_bundles":["fixture@1"],"options":{},
            "batch_secs":0,"max_delay_secs":0,"retry_backoff_secs":0}}),
    );
    run("ship.put", json!({"name":"apps","scope":"apps"}));
    let tick = || -> Value {
        let report = run("serve", json!({"once":true}));
        report["ships"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["ship"] == "ship")
            .cloned()
            .unwrap()
    };
    let wake = |body: &str| -> Value {
        let before = run("wake.list", json!({"ship":"ship"}))["wakes"]
            .as_array()
            .unwrap()
            .len();
        run(
            "mail.send",
            json!({"from":"apps","to":"ship","kind":"notice","task_id":"task","body":body}),
        );
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            tick();
            let wakes = run("wake.list", json!({"ship":"ship"}))["wakes"].clone();
            let latest = wakes[0].clone();
            if wakes.as_array().unwrap().len() > before && latest["status"] != "running" {
                // A failed wake shows its job's output, not just "failed".
                let job = home
                    .join("jobs")
                    .join(latest["job_id"].as_str().unwrap_or(""));
                let output: String = fs::read_dir(&job)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .flat_map(|run| fs::read_dir(run.path()).into_iter().flatten().flatten())
                    .map(|log| fs::read_to_string(log.path()).unwrap_or_default())
                    .collect();
                assert_eq!(latest["status"], "succeeded", "{latest}\n{output}");
                return latest;
            }
            assert!(Instant::now() < until, "wake never finished: {latest}");
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    let digests = |id: &Value| -> (String, String) {
        store
            .connection()
            .query_row(
                "SELECT w.plan_digest, s.plan_digest FROM wakes w, sessions s
                 WHERE w.id=?1 AND s.ship='ship' AND s.focus=w.focus",
                [id.as_str().unwrap()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    };
    let resumed = |line: usize| -> Option<String> {
        let argv: Vec<String> =
            serde_json::from_str(fs::read_to_string(&log).unwrap().lines().nth(line).unwrap())
                .unwrap();
        let index = argv.iter().position(|a| a == "--resume")?;
        Some(argv[index + 1].clone())
    };

    // The first bounded wake starts fresh, and its session takes the digest
    // the wake recorded at launch.
    let first = wake("first");
    assert_eq!(first["mode"], "fresh", "{first}");
    let session = first["session_id"].as_str().unwrap().to_owned();
    let (wake_digest, session_digest) = digests(&first["id"]);
    assert_eq!(wake_digest.len(), 64);
    assert_eq!(session_digest, wake_digest);
    assert_eq!(resumed(0), None);

    // Same plan: the next wake resumes that exact session.
    let second = wake("second");
    assert_eq!(second["mode"], "resume", "{second}");
    assert_eq!(second["session_id"], session.as_str());
    assert_eq!(resumed(1).as_deref(), Some(session.as_str()));
    assert_eq!(digests(&second["id"]).0, wake_digest);

    // A changed carried setting is another plan: a fresh session under it.
    settings("other");
    let third = wake("third");
    assert_eq!(third["mode"], "fresh", "{third}");
    assert_ne!(third["session_id"], session.as_str());
    assert_eq!(resumed(2), None);
    let (changed, recorded) = digests(&third["id"]);
    assert_ne!(changed, wake_digest);
    assert_eq!(recorded, changed);
}

#[test]
fn credentials_are_the_captains_from_stdin_and_plans_carry_only_refs() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    let (temp, mut store, mut config) = fixture();
    let root = temp.path().canonicalize().unwrap();
    let cli = |command: &str, args: Value, input: &[u8], agent: bool| -> (bool, String) {
        let mut cli = support::command(env!("CARGO_BIN_EXE_privateer"));
        cli.args([command, "--json", &args.to_string()])
            .env("HOME", root.join("user"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("PRIVATEER_FLEET", root.join("fleet.sqlite"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // A restrictive umask must not change the 0700 directory or 0600 file.
        unsafe {
            cli.pre_exec(|| {
                libc::umask(0o277);
                Ok(())
            })
        };
        if agent {
            cli.env("PRIVATEER_ROLE", "agent")
                .env("PRIVATEER_ACTOR", "ship");
        }
        let mut child = cli.spawn().unwrap();
        // A refused command may exit before reading its stdin.
        let _ = child.stdin.take().unwrap().write_all(input);
        let output = child.wait_with_output().unwrap();
        let text = [output.stdout, output.stderr].concat();
        (output.status.success(), String::from_utf8(text).unwrap())
    };
    let brave = json!({"ref":"brave-search"});
    let directory = root.join("fleet.credentials");
    for (command, args) in [
        ("credential.set", brave.clone()),
        ("credential.list", json!({})),
        ("credential.remove", brave.clone()),
    ] {
        let (ok, text) = cli(command, args, b"SENTINEL-SECRET\n", true);
        assert!(!ok && text.contains("operator command"), "{text}");
    }
    assert!(!directory.exists());
    // The secret comes from stdin alone: never JSON arguments, never empty.
    let (ok, text) = cli(
        "credential.set",
        json!({"ref":"brave-search","value":"SENTINEL-SECRET"}),
        b"",
        false,
    );
    assert!(!ok && text.contains("stdin"), "{text}");
    assert!(!cli("credential.set", brave.clone(), b"\n", false).0);
    assert!(!cli("credential.set", json!({"ref":"Brave"}), b"x", false).0);
    let (ok, text) = cli("credential.set", brave.clone(), b"SENTINEL-SECRET\n", false);
    assert!(ok && !text.contains("SENTINEL"), "{text}");
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&directory), 0o700);
    assert_eq!(mode(&directory.join("brave-search")), 0o600);
    assert_eq!(
        fs::read(directory.join("brave-search")).unwrap(),
        b"SENTINEL-SECRET"
    );
    let (ok, text) = cli("credential.list", json!({}), b"", false);
    assert!(ok, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap(),
        json!({"refs":["brave-search"]})
    );
    // Never over RPC or a peer: there is no stdin to read the secret from.
    let error = dispatch(&mut store, "credential.list", json!({})).unwrap_err();
    assert!(error.to_string().contains("captain's CLI"));
    // A bundle names the ref; its selected plan carries the name and ref only.
    config["bundles"][0]["credentials"] = json!([{"name":"BRAVE_API_KEY","ref":"brave-search"}]);
    dispatch(&mut store, "bundle.configure", json!({"registry":config})).unwrap();
    let selected = plan(&mut store, Some(json!(["fixture@1"]))).unwrap();
    assert_eq!(
        selected_plan(&selected)["credentials"],
        json!([{"name":"BRAVE_API_KEY","ref":"brave-search"}])
    );
    assert!(!selected.to_string().contains("SENTINEL"));
    assert!(
        selected_plan(&plan(&mut store, Some(json!([]))).unwrap())
            .get("credentials")
            .is_none()
    );
    // An exposed directory refuses rather than listing or storing.
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o750)).unwrap();
    assert!(!cli("credential.list", json!({}), b"", false).0);
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let (ok, text) = cli("credential.remove", brave.clone(), b"", false);
    assert!(ok && text.contains("\"removed\":true"), "{text}");
    assert!(!directory.join("brave-search").exists());
    let (_, text) = cli("credential.list", json!({}), b"", false);
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap(),
        json!({"refs":[]})
    );
}

#[test]
fn configure_accepts_mcp_only_bundle_and_refusals_name_field_not_value() {
    let (_temp, mut store, config) = fixture();
    let mut mcp_only = config.clone();
    mcp_only["bundles"] = json!([{"ref":"docs@1","roles":["crew"],
        "credentials":[{"name":"DOCS_TOKEN","ref":"docs-search"}],
        "mcp_servers":[{"name":"docs","url":"https://mcp.example.test/mcp","credential":"DOCS_TOKEN","timeout":5}]}]);
    let configured =
        dispatch(&mut store, "bundle.configure", json!({"registry":mcp_only})).unwrap();
    assert_eq!(configured["configured"], true);

    let mut bad_pin = config.clone();
    bad_pin["bundles"][0]["skills"][0]["sha256"] = json!("e".repeat(64));
    let error = dispatch(&mut store, "bundle.configure", json!({"registry":bad_pin})).unwrap_err();
    assert_eq!(error.to_string(), "bundle fixture@1 refused: skills.sha256");

    let sentinel = "TOOL_BUNDLE_SENTINEL_4f1a";
    let mut bad_url = mcp_only.clone();
    bad_url["bundles"][0]["mcp_servers"][0]["url"] = json!(format!(
        "http://{sentinel}.example.test/mcp?token={sentinel}"
    ));
    let error = dispatch(&mut store, "bundle.configure", json!({"registry":bad_url})).unwrap_err();
    assert_eq!(error.to_string(), "bundle docs@1 refused: mcp_servers.url");
    assert!(!format!("{error:#}").contains(sentinel));

    let mut undeclared = mcp_only.clone();
    undeclared["bundles"][0]["mcp_servers"][0]["credential"] = json!(sentinel);
    let error = dispatch(
        &mut store,
        "bundle.configure",
        json!({"registry":undeclared}),
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "bundle docs@1 refused: mcp_servers.credential"
    );
    assert!(!format!("{error:#}").contains(sentinel));
}

#[test]
fn a_forge_credential_is_never_given_to_a_launch() {
    use std::io::Write;
    use std::process::Stdio;
    let (temp, _store, config) = fixture();
    let root = temp.path().canonicalize().unwrap();
    let cli = |command: &str, args: &Value, input: &[u8]| -> (bool, String) {
        let mut cli = support::command(env!("CARGO_BIN_EXE_privateer"));
        cli.args(["--home", root.join("home").to_str().unwrap()])
            .args([command, "--json", &args.to_string()])
            .env("HOME", root.join("user"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("PRIVATEER_FLEET", root.join("fleet.sqlite"))
            .env("PRIVATEER_FORGE_ADAPTER", root.join("no-adapter"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cli.spawn().unwrap();
        let _ = child.stdin.take().unwrap().write_all(input);
        let output = child.wait_with_output().unwrap();
        let text = [output.stdout, output.stderr].concat();
        (output.status.success(), String::from_utf8(text).unwrap())
    };
    let (ok, text) = cli("credential.set", &json!({"ref":"brave-search"}), b"brave-value\n");
    assert!(ok, "{text}");
    let forges = json!({"repos": {"github.com/acme/widget": {
        "forge": "github", "api": "https://api.github.com", "branch": "main",
        "required_checks": ["ci"],
        "reviewer": {"account": "reviewer-bot", "credential": "forge-reviewer-github"},
        "merger": {"account": "captain", "credential": "forge-merger-github"}}}});
    let (ok, text) = cli("forges.set", &forges, b"");
    assert!(ok, "{text}");
    assert!(text.contains("forge-reviewer-github, forge-merger-github is not stored yet"), "{text}");
    // A bundle registry that names a forges.json ref is refused.
    for forge_ref in ["forge-reviewer-github", "forge-merger-github"] {
        let mut named = config.clone();
        named["bundles"][0]["credentials"] = json!([{"name":"FORGE_TOKEN","ref":forge_ref}]);
        let (ok, text) = cli("bundle.configure", &json!({"registry": named}), b"");
        assert!(!ok, "{forge_ref}: {text}");
        assert!(text.contains("forge credential named in forges.json"), "{text}");
    }
    let mut brave = config.clone();
    brave["bundles"][0]["credentials"] = json!([{"name":"BRAVE_API_KEY","ref":"brave-search"}]);
    let (ok, text) = cli("bundle.configure", &json!({"registry": brave}), b"");
    assert!(ok, "{text}");
    // forges.set cannot later name a ref this home's bundles give to launches.
    let mut taken = forges.clone();
    taken["repos"]["github.com/acme/widget"]["merger"]["credential"] = json!("brave-search");
    let (ok, text) = cli("forges.set", &taken, b"");
    assert!(!ok && text.contains("given to launches"), "{text}");
    // A launch plan that names one (as another home's registry could) is
    // refused where its credential files are given out, in bundles.py; a
    // forges.json that cannot be read refuses every credential.
    let launch = |reference: &str| -> String {
        let script = format!(
            "import json, sys\nsys.path.insert(0, {plugins:?})\nfrom bundles import credential_environment\n\
             try:\n    found = credential_environment({{'credentials': [{{'name': 'TOKEN', 'ref': {reference:?}}}]}}, \
             {{'PRIVATEER_FLEET': {fleet:?}, 'HOME': {home:?}}})\n    print('given ' + ','.join(sorted(found)))\n\
             except Exception as error:\n    print('refused ' + type(error).__name__)\n",
            plugins = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins").to_str().unwrap(),
            fleet = root.join("fleet.sqlite").to_str().unwrap(),
            home = root.join("user").to_str().unwrap(),
        );
        let output = Command::new(support::python())
            .args(["-I", "-c", &script])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    assert_eq!(launch("brave-search"), "given PRIVATEER_CREDENTIAL_TOKEN_FILE");
    assert_eq!(launch("forge-reviewer-github"), "refused Refused");
    assert_eq!(launch("forge-merger-github"), "refused Refused");
    fs::write(root.join("fleet.forges.json"), "{not json").unwrap();
    assert_eq!(launch("brave-search"), "refused Refused");
    let (ok, text) = cli("bundle.configure", &json!({"registry": brave}), b"");
    assert!(!ok && text.contains("forges"), "{text}");
}
