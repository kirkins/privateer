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

/// A fake `claude` in `bin` for one scripted turn. The loader refuses a #!
/// wrapper, so it is a compiled exec shim into a Python script. It answers
/// the adapter's version and help probes, logs each launch's argv to `log`,
/// and reports the session it was given, or a new one, as Claude's
/// stream-json does, around `turn`.
fn fake_claude(root: &Path, bin: &Path, log: &Path, turn: &str) {
    fake_claude_with(root, bin, log, "", turn);
}

/// `fake_claude` running `prelude` before its init event, which `prelude`
/// may extend through the `init` dict, as Claude reports its MCP servers there.
fn fake_claude_with(root: &Path, bin: &Path, log: &Path, prelude: &str, turn: &str) {
    use std::os::unix::fs::PermissionsExt;
    let python = support::python();
    let script = root.join("claude.py");
    fs::write(&script, format!(r#"import json, os, re, subprocess, sys, uuid
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
init = {{"type": "system", "subtype": "init", "session_id": session}}
{prelude}
print(json.dumps(init), flush=True)
{turn}
print(json.dumps({{"type": "result", "subtype": "success", "is_error": False, "session_id": session}}), flush=True)
"#, json!(log))).unwrap();
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
fn selected_mcp_gate_helpers_refuse_unsafe_records() {
    let output = support::command("node")
        .arg("tests/pi-mcp-selected.mjs")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("Node is required to check the selected-MCP gate");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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

/// A hosted first mate's pane is not a job, so its stop could leave a stdio
/// server running: the runtime refuses one in a first mate's selection, with
/// the adapter's reason, while HTTP servers and a crew's stdio server stay.
#[test]
fn a_first_mate_selection_refuses_stdio_servers_and_keeps_http() {
    let (_temp, mut store, mut config) = fixture();
    let python = fs::canonicalize("/usr/bin/python3").unwrap();
    let digest = format!("{:x}", Sha256::digest(fs::read(&python).unwrap()));
    let bundles = config["bundles"].as_array_mut().unwrap();
    bundles.push(json!({"ref":"stdio@1","roles":["crew","first_mate"],
        "mcp_servers":[{"name":"xcode","timeout":10,"sha256":digest,"command":[python,"-I","/srv/mcp/server.py"]}]}));
    bundles.push(json!({"ref":"docs@1","roles":["crew","first_mate"],
        "credentials":[{"name":"DOCS_TOKEN","ref":"docs-search"}],
        "mcp_servers":[{"name":"docs","url":"https://mcp.example.test/mcp","credential":"DOCS_TOKEN","timeout":5}]}));
    dispatch(&mut store, "bundle.configure", json!({"registry":config})).unwrap();
    let profile = |selected: Value| {
        json!({"manifest":"bundled:pi","duty_task":"task","options":{"provider":"chosen","model":"exact"},
            "tool_bundles":selected})
    };
    for selected in [json!(["stdio@1"]), json!(["docs@1", "stdio@1"])] {
        let error = privateer::activation::profile(&store, "ship", &profile(selected)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "stdio MCP servers run only in crew jobs, whose teardown ends them; \
             first_mate launches are not jobs"
        );
    }
    let validated =
        privateer::activation::profile(&store, "ship", &profile(json!(["docs@1"]))).unwrap();
    assert_eq!(validated["tool_bundles"], json!(["docs@1"]));
    let crew = selected_plan(&plan(&mut store, Some(json!(["stdio@1"]))).unwrap());
    assert_eq!(crew["mcp_servers"][0]["command"][0], json!(python));
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
    // The fake claude acknowledges its wake's mail.
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    let python = support::python();
    let log = root.join("claude-argv.jsonl");
    let cli = json!([support::BIN, "--home", home]);
    let turn = format!(
        r#"through = int(re.search(r"through seq (\d+)", args[-1]).group(1))
subprocess.run({cli} + ["mail.ack", "--json", json.dumps({{"address": "ship", "through_seq": through}})],
               check=True, capture_output=True)"#
    );
    fake_claude(&root, &bin, &log, &turn);
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

/// A crew resumes only its own session: the one its park is resuming, for
/// its task, under the plan the crew launched with. It never resumes a first
/// mate's or an errand's session, and neither of those resumes a crew's.
#[test]
fn a_bounded_crew_resumes_only_its_own_parked_session_under_its_plan() {
    let (temp, mut store, mut config) = fixture();
    config["claude_version"] = json!("2.1.300");
    dispatch(&mut store, "bundle.configure", json!({"registry":config})).unwrap();
    store.connection().execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('other','charter','ship','other task','open',0)", []).unwrap();
    let root = temp.path().canonicalize().unwrap();
    let claude = root.join("claude-config");
    fs::create_dir(&claude).unwrap();
    let settings = |model: &str| {
        fs::write(
            claude.join("settings.json"),
            json!({"model":model}).to_string(),
        )
        .unwrap();
    };
    settings("exact");
    let home = store.home().to_owned();
    let path = format!(
        "{}:/usr/bin:/bin",
        support::python().parent().unwrap().display()
    );
    let launch = |id: &str, task: &str, resume: Option<&str>| -> Result<Value, String> {
        let mut args = json!({"id":id,"task_id":task,"manifest":"bundled:claude",
            "tool_bundles":["fixture@1"],"options":{"model":"exact"}});
        if let Some(session) = resume {
            args["options"]["resume"] = json!(session);
        }
        let output = support::command(support::BIN)
            .args(["--home", home.to_str().unwrap(), "plugin.command", "--json"])
            .arg(args.to_string())
            .env("HOME", &root)
            .env("PATH", &path)
            .env("CLAUDE_CONFIG_DIR", &claude)
            .env("XDG_CONFIG_HOME", root.join("xdg"))
            .env("XDG_STATE_HOME", root.join("xdg-state"))
            .env("PRIVATEER_RESOURCE_HOME", root.join("ledger"))
            .output()
            .unwrap();
        if output.status.success() {
            Ok(serde_json::from_slice(&output.stdout).unwrap())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).into_owned())
        }
    };
    let refused = |result: Result<Value, String>| {
        let error = result.expect_err("resumed");
        assert!(
            error.contains("a bounded session resumes only under the plan it started with"),
            "{error}"
        );
        error
    };
    // A crew's park, resuming its crew's session, as the runtime records it
    // before the resume launches.
    let park =
        |crew: &str, task: &str, state: &str, session: &str, digest: &str| {
            store
                .connection()
                .execute(
                    "INSERT INTO jobs(id,record) VALUES(?1,?2)",
                    rusqlite::params![
                        crew,
                        json!({"id":crew,"launch":{"plan_digest":digest}}).to_string()
                    ],
                )
                .unwrap();
            store.connection().execute(
            "INSERT INTO crew_parks(id,root,crew_job_id,child_job_id,task_id,ship,state,next,
               harness,session,attempt,created_ms,updated_ms)
             VALUES('park-'||?1,?1,?1,'render',?2,'ship',?3,'check the render','claude',?4,1,0,0)",
            rusqlite::params![crew, task, state, session],
        )
        .unwrap();
        };

    // A crew's launch carries its plan's digest, for its job to record.
    let digest = launch("crew-1", "task", None).unwrap()["plan_digest"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(digest.len(), 64);
    let session = "00000000-0000-4000-8000-00000000c1e1";
    // No park resumes it yet.
    refused(launch("crew-2", "task", Some(session)));
    park("crew-1", "task", "resuming", session, &digest);
    let resumed = launch("crew-resume-park-crew-1-1", "task", Some(session)).unwrap();
    assert!(
        resumed["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == session),
        "{resumed}"
    );
    assert_eq!(resumed["plan_digest"], digest.as_str());
    // A crew of another task cannot take it.
    refused(launch("crew-3", "other", Some(session)));
    // Neither a first mate's wake nor an errand resumes a crew's session.
    store
        .connection()
        .execute(
            "INSERT INTO wakes(id,ship,focus,through_seq,job_id,mode,status,created_ms,session_id)
             VALUES('wake-1','ship','duty',0,'wake-1','resume','running',0,?1)",
            [session],
        )
        .unwrap();
    refused(launch("wake-1", "task", Some(session)));
    store
        .connection()
        .execute(
            "INSERT INTO errands(id,ship,focus,task_id,brief,mode,status,created_ms)
             VALUES('errand-1','ship','errand','task','look','resume','running',0)",
            [],
        )
        .unwrap();
    refused(launch("errand-1", "task", Some(session)));
    // A crew never resumes a first mate's or an errand's session, even one
    // started under its own digest that a park claims.
    for (n, focus) in ["duty", "errand:errand-0"].iter().enumerate() {
        let other = format!("00000000-0000-4000-8000-00000000f00{n}");
        store.connection().execute(
            "INSERT INTO sessions(ship,focus,harness,session_id,wakes,started_ms,last_turn_ms,plan_digest)
             VALUES('ship',?1,'claude',?2,1,0,0,?3)",
            rusqlite::params![focus, other, digest],
        )
        .unwrap();
        let crew = format!("crew-forged-{n}");
        park(&crew, "task", "resuming", &other, &digest);
        refused(launch(&format!("{crew}-resume"), "task", Some(&other)));
    }
    // A park that is not resuming resumes nothing.
    let waiting = "00000000-0000-4000-8000-00000000c1e2";
    park("crew-4", "task", "parked", waiting, &digest);
    refused(launch("crew-4-resume", "task", Some(waiting)));
    // The plan changed while the crew was parked: refused, saying so.
    settings("other");
    let error = refused(launch("crew-resume-park-crew-1-2", "task", Some(session)));
    assert!(
        error.contains("task task: crew session not resumed because its bundle plan changed"),
        "{error}"
    );
}

/// A bounded Claude crew that parks on a gated heavy job, with what it left.
struct ParkedCrew {
    _temp: TempDir,
    store: Store,
    /// The crew's job once its park settled: resumed or failed.
    crew: Value,
    /// Each claude launch's argv.
    launches: Vec<Vec<String>>,
    /// The mail its first mate was sent.
    mail: Vec<String>,
}

/// Launch a bounded crew that parks on a heavy job it starts, and release
/// the job once it is parked, after changing the crew's carried Claude
/// settings when `change_plan`. The job's supervisor resumes the crew.
fn park_a_bounded_crew(change_plan: bool) -> ParkedCrew {
    use std::time::{Duration, Instant};
    let (temp, mut store, mut config) = fixture();
    // The crew's budget covers its launch on a loaded runner.
    store
        .connection()
        .execute(
            "UPDATE charters SET data=json_set(data,'$.job_timeout_secs',300) WHERE id='charter'",
            [],
        )
        .unwrap();
    config["claude_version"] = json!("2.1.300");
    dispatch(&mut store, "bundle.configure", json!({"registry":config})).unwrap();
    let root = temp.path().canonicalize().unwrap();
    let home = store.home().to_owned();
    let claude = root.join("claude-config");
    fs::create_dir(&claude).unwrap();
    let settings = |model: &str| {
        fs::write(
            claude.join("settings.json"),
            json!({"model":model}).to_string(),
        )
        .unwrap();
    };
    settings("exact");
    // The crew starts a heavy job that runs until released, and parks on it.
    // Its resume only finishes its turn.
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    let log = root.join("claude-argv.jsonl");
    let release = root.join("release");
    let gate = format!(
        "while [ ! -e '{}' ]; do [ -d '{}' ] || exit 1; sleep 0.1; done; echo rendered",
        release.display(),
        root.display()
    );
    let cli = json!([support::BIN, "--home", home]);
    let render = json!({"id":"render","task_id":"task","command":["sh","-c",gate],"heavy":true});
    let park = json!({"job_id":"render","next":"check the render"});
    let turn = format!(
        r#"if not os.environ["PRIVATEER_JOB"].startswith("crew-resume-"):
    for command, value in (("job.start", {render}), ("crew.park", {park})):
        subprocess.run({cli} + [command, "--json", value], check=True, capture_output=True)"#,
        // Each request as JSON text, quoted as a Python string.
        render = json!(render.to_string()),
        park = json!(park.to_string()),
    );
    fake_claude(&root, &bin, &log, &turn);
    let path = format!(
        "{}:{}:/usr/bin:/bin",
        bin.display(),
        support::python().parent().unwrap().display()
    );
    let run = |agent: bool, command: &str, args: Value| -> Value {
        let mut cli = support::command(support::BIN);
        cli.args(["--home", home.to_str().unwrap(), command, "--json"])
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
            .env_remove("HERDR_SOCKET_PATH");
        if agent {
            cli.env("PRIVATEER_ROLE", "agent")
                .env("PRIVATEER_ACTOR", "ship");
        }
        let output = cli.output().unwrap();
        assert!(
            output.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    run(false, "ship.put", json!({"name":"ship","scope":"ship"}));
    run(
        false,
        "resource.configure",
        json!({"min_available_memory_bytes":0,"min_free_disk_bytes":0,
            "max_load_per_cpu":1024,"ramp_secs":0,
            "crew_memory_bytes":{"claude":1u64 << 20},
            "heavy_estimate":{"disk_bytes":1u64 << 20}}),
    );
    run(
        true,
        "agent.start",
        json!({"task_id":"task","id":"crew-1","manifest":"bundled:claude",
            "tool_bundles":["fixture@1"],"options":{"model":"exact"}}),
    );
    let wait = |id: &str, accepted: &[&str]| -> Value {
        let until = Instant::now() + Duration::from_secs(90);
        loop {
            let job = run(false, "job.status", json!({"id":id}));
            if accepted.contains(&job["status"].as_str().unwrap_or_default()) {
                return job;
            }
            assert!(
                Instant::now() < until,
                "{id} never reached {accepted:?}: {job}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    let parked = wait("crew-1", &["parked"]);
    assert_eq!(parked["park"], "park-crew-1-1", "{parked}");
    if change_plan {
        settings("other");
    }
    fs::write(&release, "").unwrap();
    let crew = wait("crew-1", &["resumed", "failed"]);
    if crew["status"] == "resumed" {
        wait("crew-resume-park-crew-1-1-1", &["succeeded"]);
    }
    let launches = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let mail = run(
        false,
        "mail.inbox",
        json!({"address":"ship","all":true,"limit":1000}),
    )["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["body"].as_str().map(str::to_owned))
        .collect();
    ParkedCrew {
        _temp: temp,
        store,
        crew,
        launches,
        mail,
    }
}

/// A bounded crew parks, its job ends, and it resumes its own session under
/// the plan it launched with.
#[test]
fn a_parked_bounded_crew_resumes_its_session_under_the_same_plan() {
    let parked = park_a_bounded_crew(false);
    assert_eq!(parked.crew["status"], "resumed", "{}", parked.crew);
    assert_eq!(parked.launches.len(), 2, "{:?}", parked.launches);
    let (session, started, resumed): (String, String, String) = parked
        .store
        .connection()
        .query_row(
            "SELECT p.session, json_extract(c.record,'$.launch.plan_digest'),
                    json_extract(r.record,'$.launch.plan_digest')
               FROM crew_parks p JOIN jobs c ON c.id=p.crew_job_id
               JOIN jobs r ON r.id=p.resumed_job_id WHERE p.id='park-crew-1-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(started.len(), 64);
    assert_eq!(resumed, started, "the resume runs under the crew's plan");
    let argv = &parked.launches[1];
    let at = argv.iter().position(|a| a == "--resume").unwrap();
    assert_eq!(argv[at + 1], session);
    assert!(!parked.launches[0].contains(&"--resume".to_owned()));
}

/// The crew's plan changes while it is parked: its session is not resumed,
/// and its first mate is told why, naming the task.
#[test]
fn a_parked_bounded_crew_whose_plan_changed_is_not_resumed() {
    let parked = park_a_bounded_crew(true);
    assert_eq!(parked.crew["status"], "failed", "{}", parked.crew);
    let why = "task task: crew session not resumed because its bundle plan changed";
    let reason = parked.crew["reason"].as_str().unwrap_or_default();
    assert!(reason.contains(why), "{reason}");
    assert_eq!(parked.launches.len(), 1, "{:?}", parked.launches);
    assert!(
        parked
            .mail
            .iter()
            .any(|body| body.starts_with("Crew job crew-1 failed") && body.contains(why)),
        "{:?}",
        parked.mail
    );
}

/// A bounded Claude crew's stdio MCP server, with a grandchild that ignores
/// SIGTERM, ends with the crew's job: inside the server's process group
/// (`stubborn`) or in a session of its own (`escaped`). The fake claude spawns
/// each server from its --mcp-config as Claude does, then exits without
/// stopping it, as Claude 2.1.300 and Pi 1.3.0 leave such a grandchild
/// (tests/claude_bundles_native.py, tests/pi_bundles_native.py).
#[test]
fn a_bounded_crews_stdio_mcp_server_and_its_stubborn_grandchild_end_with_its_job() {
    use std::time::{Duration, Instant};
    let (temp, mut store, mut config) = fixture();
    store
        .connection()
        .execute(
            "UPDATE charters SET data=json_set(data,'$.job_timeout_secs',300) WHERE id='charter'",
            [],
        )
        .unwrap();
    let root = temp.path().canonicalize().unwrap();
    // A root-owned interpreter: nothing in its path is writable or a link.
    let python = fs::canonicalize("/usr/bin/python3").unwrap();
    let digest = format!("{:x}", Sha256::digest(fs::read(&python).unwrap()));
    let server = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi-mcp/server.py");
    let marker = |mode: &str| root.join(format!("{mode}.jsonl"));
    config["claude_version"] = json!("2.1.300");
    for mode in ["stubborn", "escaped"] {
        config["bundles"].as_array_mut().unwrap().push(json!({
            "ref": format!("{mode}@1"), "roles": ["crew"],
            "mcp_servers": [{"name": mode, "timeout": 10, "sha256": digest,
                "command": [python, "-I", server, marker(mode), "selected", mode]}]}));
    }
    dispatch(&mut store, "bundle.configure", json!({"registry":config})).unwrap();
    let home = store.home().to_owned();
    let claude = root.join("claude-config");
    fs::create_dir(&claude).unwrap();
    fs::write(
        claude.join("settings.json"),
        json!({"model":"exact"}).to_string(),
    )
    .unwrap();
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    let log = root.join("claude-argv.jsonl");
    // Claude starts each configured server with its own environment plus the
    // entry's, and reports it connected at init once its tools list.
    let prelude = r#"import pathlib, time
config = next(a.split("=", 1)[1] for a in flags if a.startswith("--mcp-config="))
servers = json.load(open(config))["mcpServers"]
for name, entry in servers.items():
    # Privateer's launcher, by absolute interpreter path, with the pinned command in its spec.
    assert entry["type"] == "stdio" and entry["env"] == {} and entry["command"].startswith("/"), entry
    assert entry["args"][0] == "-I" and entry["args"][1].endswith("/plugins/mcp_stdio.py"), entry
    subprocess.Popen([entry["command"], *entry["args"]], env={**os.environ, **entry["env"]},
                     stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    marker = pathlib.Path(json.loads(entry["args"][2])["command"][3])
    deadline = time.monotonic() + 30
    while not (marker.exists() and '"child"' in marker.read_text()) and time.monotonic() < deadline:
        time.sleep(0.05)
# Long enough for the job to record what left its process group.
time.sleep(1.5)
init["mcp_servers"] = [{"name": name, "status": "connected"} for name in servers]
init["tools"] = ["mcp__" + name + "__echo" for name in servers]"#;
    fake_claude_with(&root, &bin, &log, prelude, "");
    let path = format!(
        "{}:{}:/usr/bin:/bin",
        bin.display(),
        support::python().parent().unwrap().display()
    );
    let run = |agent: bool, command: &str, args: Value| -> Value {
        let mut cli = support::command(support::BIN);
        cli.args(["--home", home.to_str().unwrap(), command, "--json"])
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
            .env_remove("HERDR_SOCKET_PATH");
        if agent {
            cli.env("PRIVATEER_ROLE", "agent")
                .env("PRIVATEER_ACTOR", "ship");
        }
        let output = cli.output().unwrap();
        assert!(
            output.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    run(false, "ship.put", json!({"name":"ship","scope":"ship"}));
    run(
        false,
        "resource.configure",
        json!({"min_available_memory_bytes":0,"min_free_disk_bytes":0,
            "max_load_per_cpu":1024,"ramp_secs":0,
            "crew_memory_bytes":{"claude":1u64 << 20}}),
    );
    let active = |pid: i64| {
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "stat="])
            .output()
            .unwrap();
        let stat = String::from_utf8(output.stdout).unwrap();
        !stat.trim().is_empty() && !stat.trim().starts_with('Z')
    };
    for mode in ["stubborn", "escaped"] {
        let id = format!("crew-{mode}");
        run(
            true,
            "agent.start",
            json!({"task_id":"task","id":id,"manifest":"bundled:claude",
                "tool_bundles":[format!("{mode}@1")],"options":{"model":"exact"}}),
        );
        let until = Instant::now() + Duration::from_secs(90);
        let job = loop {
            let job = run(false, "job.status", json!({"id":id}));
            if matches!(
                job["status"].as_str(),
                Some("succeeded" | "failed" | "cancelled" | "timed_out" | "orphaned")
            ) {
                break job;
            }
            assert!(Instant::now() < until, "{id} never settled: {job}");
            std::thread::sleep(Duration::from_millis(100));
        };
        assert_eq!(job["status"], "succeeded", "{job}");
        let events: Vec<Value> = fs::read_to_string(marker(mode))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let pids: Vec<i64> = events
            .iter()
            .filter(|e| e["event"] == "start" || e["event"] == "child")
            .map(|e| e["pid"].as_i64().unwrap())
            .collect();
        assert_eq!(pids.len(), 2, "{events:?}");
        let child = events.iter().find(|e| e["event"] == "child").unwrap();
        assert_eq!(
            child["pgid"] == child["pid"],
            mode == "escaped",
            "{mode}: {child}"
        );
        // The job settled only after its descendants were gone; a short grace
        // for the kernel to reap what it killed.
        let until = Instant::now() + Duration::from_secs(5);
        while pids.iter().any(|pid| active(*pid)) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        for pid in &pids {
            assert!(!active(*pid), "{mode}: process {pid} outlived its job");
        }
    }
}
