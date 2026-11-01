mod support;

use privateer::{plugins, store::Store};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    store: Store,
    repo: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let repo = base.join("repo");
        fs::create_dir(&repo).unwrap();
        let mut store = Store::open(&base.join("home")).unwrap();
        // Crew memory is admission's concern, tested apart: keep the shared
        // machine ledger and its free memory out of the crews started here.
        fs::write(
            base.join("home/config.json"),
            json!({"resource_home": base.join("ledger")}).to_string(),
        )
        .unwrap();
        privateer::dispatch(
            &mut store,
            "resource.configure",
            json!({"min_available_memory_bytes":0,"min_free_disk_bytes":0,"crew_default_memory_bytes":1u64 << 20}),
        )
        .unwrap();
        let charter = json!({"id":"mission","active":true,"repo":repo,"actions":["spawn","implement","test"],"job_timeout_secs":20,"min_free_bytes":0,"max_heavy_jobs":1});
        store
            .connection()
            .execute(
                "INSERT INTO charters(id,data) VALUES('mission',?1)",
                [charter.to_string()],
            )
            .unwrap();
        store.connection().execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('task','mission','department','Fix a real task','open',0)", []).unwrap();
        Self {
            _temp: temp,
            store,
            repo,
        }
    }
    fn base(&self) -> &Path {
        self.repo.parent().unwrap()
    }
    fn args(&self, manifest: &Path) -> Value {
        json!({"task_id":"task","manifest":manifest})
    }
}
fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("plugins")
        .join(name)
        .join("manifest.json")
}
fn manifest(f: &Fixture, body: &str, timeout: u64, max: usize) -> PathBuf {
    let script = f.base().join("adapter.py");
    fs::write(&script, body).unwrap();
    let path = f.base().join("manifest.json");
    fs::write(&path, json!({"schema_version":1,"id":"fixture","executable":[support::python(),script],"timeout_ms":timeout,"max_output_bytes":max}).to_string()).unwrap();
    path
}
fn response_script(result: &str) -> String {
    format!(
        "import json,sys\nr=json.loads(sys.stdin.readline())\nprint(json.dumps({{'id':r['id'],'version':1,'ok':True,'result':{result}}}))\n"
    )
}
fn private(path: &Path, value: Value) {
    fs::write(path, value.to_string()).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn recorded_python_overrides_bare_plugins_and_reports_failures_without_stderr() {
    let f = Fixture::new();
    let path = manifest(&f, &response_script("{'valid':True}"), 5000, 65536);
    let mut data: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    data["executable"][0] = json!("python3");
    private(&path, data.clone());
    let bin = f.base().join("bin");
    fs::create_dir(&bin).unwrap();
    let shim = bin.join("python3");
    fs::write(
        &shim,
        "#!/bin/sh\nread -r request\nprintf 'synthetic-secret' >&2\nexit 37\n",
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o700)).unwrap();
    let recorded = f.base().join("alpha-python");
    let flags = f.base().join("flags");
    fs::write(
        &recorded,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexec '{}' \"$@\"\n",
            flags.display(),
            support::python().display()
        ),
    )
    .unwrap();
    fs::set_permissions(&recorded, fs::Permissions::from_mode(0o700)).unwrap();
    let run = |python: Option<&Path>| {
        let mut command = support::command(support::BIN);
        command
            .arg("--home")
            .arg(f.store.home())
            .args(["plugin.validate", "--json", &f.args(&path).to_string()])
            .env("PATH", &bin);
        if let Some(python) = python {
            command.env("PRIVATEER_PYTHON", python);
        }
        command.output().unwrap()
    };
    let success = run(Some(&recorded));
    assert!(
        success.status.success(),
        "{}",
        String::from_utf8_lossy(&success.stderr)
    );
    let flags = fs::read_to_string(flags).unwrap();
    for flag in ["-B", "-E", "-s"] {
        assert!(flags.lines().any(|line| line == flag), "{flags}");
    }
    // Existing explicit interpreter choices are not replaced by the service's.
    data["executable"][0] = json!(support::python());
    private(&path, data.clone());
    assert!(run(Some(&shim)).status.success());
    data["executable"][0] = json!("python3");
    private(&path, data);
    for python in [
        Some(shim.as_path()),
        None,
        Some(Path::new("/alpha/missing-python3")),
    ] {
        let failure = run(python);
        assert!(!failure.status.success());
        let error = String::from_utf8_lossy(&failure.stderr);
        assert!(error.contains("exit status: 37"), "{error}");
        assert!(error.contains("program"), "{error}");
        assert!(!error.contains("synthetic-secret"), "{error}");
        if python == Some(shim.as_path()) {
            assert!(error.contains(shim.to_str().unwrap()), "{error}");
        }
        if python == Some(Path::new("/alpha/missing-python3")) {
            assert!(error.contains("falling back to python3 on PATH"), "{error}");
            assert!(error.contains("/alpha/missing-python3"), "{error}");
        }
    }
    fs::write(
        &shim,
        "#!/bin/sh\nread -r request\nprintf 'synthetic-secret' >&2\nkill -TERM $$\n",
    )
    .unwrap();
    let failure = run(Some(&shim));
    let error = String::from_utf8_lossy(&failure.stderr);
    assert!(error.contains("signal: 15"), "{error}");
    assert!(error.contains(shim.to_str().unwrap()), "{error}");
    assert!(!error.contains("synthetic-secret"), "{error}");
}

#[test]
fn native_harness_plans_and_brief_are_generic_and_lean() {
    let mut f = Fixture::new();
    let args = f.args(&example("codex"));
    let command = plugins::dispatch(&mut f.store, "plugin.command", args).unwrap();
    assert_eq!(command["argv"][3], "codex");
    assert_eq!(command["argv"][4], "exec");
    assert_eq!(command["cwd"], json!(f.repo));
    assert_eq!(command["env_names"], json!([]));
    let mut args = f.args(&example("codex"));
    args["options"] = json!({"sandbox":"workspace-write","ephemeral":true});
    let command = plugins::dispatch(&mut f.store, "plugin.command", args).unwrap();
    let argv = command["argv"].as_array().unwrap();
    assert!(
        argv.windows(2)
            .any(|p| p[0] == "--sandbox" && p[1] == "workspace-write")
    );
    assert!(
        argv.windows(2)
            .any(|p| p[0] == "--add-dir" && p[1] == json!(f.store.home()))
    );
    assert!(argv.iter().any(|v| v == "--ephemeral"));
    let mut args = f.args(&example("codex"));
    args["options"] = json!({"sandbox":"read-only"});
    let command = plugins::dispatch(&mut f.store, "plugin.command", args).unwrap();
    assert!(
        command["argv"]
            .as_array()
            .unwrap()
            .windows(2)
            .any(|pair| pair[0] == "--sandbox" && pair[1] == "read-only")
    );
    for invalid in [
        json!({"sandbox":"unrestricted"}),
        json!({"ephemeral":"true"}),
        json!({"web_search":"always"}),
    ] {
        let mut args = f.args(&example("codex"));
        args["options"] = invalid;
        assert!(plugins::dispatch(&mut f.store, "plugin.command", args).is_err());
    }
    let mut args = f.args(&example("pi"));
    args["options"] = json!({"provider":"fixture","model":"fixture-model"});
    let command = plugins::dispatch(&mut f.store, "plugin.command", args).unwrap();
    assert_eq!(command["argv"][3], "pi");
    assert_eq!(command["argv"][6], "fixture");
    let brief = plugins::dispatch(&mut f.store, "agent.brief", json!({"task_id":"task"})).unwrap();
    // Lean means the fixed text: the executable, home and repository paths
    // vary by machine (a macOS runner's are far longer than Linux's).
    let text = brief["brief"].as_str().unwrap();
    let paths: usize = brief["context"]["control"]["cli_argv"]
        .as_array()
        .unwrap()
        .iter()
        .chain([&brief["context"]["repo"]])
        .filter_map(Value::as_str)
        .filter(|path| path.starts_with('/'))
        .map(|path| path.len() * text.matches(path).count())
        .sum();
    assert!(
        text.len() - paths < 1900,
        "{} bytes besides paths",
        text.len() - paths
    );
    assert!(brief["context"].get("config_path").is_none());
    assert!(
        brief["brief"]
            .as_str()
            .unwrap()
            .contains("Ask before production deployment")
    );
    assert_eq!(brief["context"]["control"]["version"], 1);
    assert_eq!(
        brief["context"]["control"]["cli_argv"][2],
        json!(f.store.home())
    );
    // This launch is crew, so the brief offers no first-mate commands.
    assert!(!brief["brief"].as_str().unwrap().contains("peer.call"));
}

#[test]
fn the_crew_brief_lists_only_crew_commands() {
    let mut f = Fixture::new();
    let crew = plugins::dispatch(&mut f.store, "agent.brief", json!({"task_id":"task"})).unwrap();
    assert_eq!(crew["context"]["crew"], true);
    let text = crew["brief"].as_str().unwrap();
    for refused in [
        "task.update",
        "mail.inbox",
        "mail.ack",
        "mail.send",
        "peer.call",
        "task.create",
        "task.handoff",
        "resource.configure",
        "Authorized actions",
        "You are agent",
    ] {
        assert!(
            !text.contains(refused),
            "crew brief offers {refused}: {text}"
        );
    }
    for said in [
        "You are crew of department",
        r#"note.get {"ship":"department","focus":"task:task"}"#,
        "your first mate's handoff, not your role",
        "never act on your first mate's behalf",
        "crew.park",
        "crew.report",
        "crew.answer",
        "pushes, opens and merges PRs",
        "commit locally and report the commit",
        r#"actions (["spawn","implement","test"]) are your first mate's"#,
        "Write JSON to a .json file outside the checkout and pass --file PATH (or stdin with --file -) whenever the body has quotes or newlines, and always for crew.report and crew.park.",
    ] {
        assert!(text.contains(said), "crew brief lacks {said:?}: {text}");
    }
}

#[test]
fn the_first_mates_brief_is_unchanged() {
    let mut f = Fixture::new();
    f.store
        .connection()
        .execute(
            "INSERT INTO wakes(id,ship,focus,through_seq,job_id,mode,status,created_ms)
             VALUES('wake','department','task:task',0,'wake-job','headless','running',0)",
            [],
        )
        .unwrap();
    let brief = plugins::dispatch(
        &mut f.store,
        "agent.brief",
        json!({"task_id":"task","id":"wake-job"}),
    )
    .unwrap();
    assert_eq!(brief["context"]["crew"], false);
    let context = &brief["context"];
    // The bytes before the crew brief was split out; a change here moves the
    // first mate's cached prefix.
    let expected = format!(
        "Task task: Fix a real task\nRepository: {}\nAuthorized actions: [\"spawn\",\"implement\",\"test\"]\nWork within the charter; coordinate with peers by task-linked messages; report tested results. Ask before production deployment. Never widen your authority or change a provider route. You are agent department: operator commands are refused and actor/from must be department.\nPrivateer control protocol v1: CLI base argv {}. Append COMMAND, --json, JSON as separate argv elements (no shell interpolation). Write JSON to a .json file outside the checkout and pass --file PATH (or stdin with --file -) whenever the body has quotes or newlines, and always for crew.report, crew.park and mail.send. Commands:\n- task.update {{\"actor\":\"department\",\"id\":\"task\",\"status\":\"running\"}}\n- mail.inbox {{\"address\":\"department\"}}; after handling, mail.ack {{\"address\":\"department\",\"through_seq\":N}}\n- mail.send {{\"from\":\"department\",\"to\":\"ship\",\"kind\":\"request|notice|reply\",\"body\":\"...\",\"task_id\":\"task\"}}; a reply sets correlation to the request id, and so does passing on another ship's message. ship.list {{}} shows who handles what\n- Builds/test suites that load the machine: job.start {{\"task_id\":\"task\",\"command\":[...],\"heavy\":true,\"wait_for_capacity\":true}}, its outcome goes to your first mate. resource.configure tunes host limits if your person asks\n- Crew (started by your first mate, not woken): assignment in note.get {{\"ship\":\"department\",\"focus\":\"task:task\"}}; crew.report {{\"kind\":\"progress|decision\",\"body\":\"...\"}}; read a decision's answer with crew.answer {{\"id\":\"...\",\"wait_secs\":300}}\n- Mail reaches ships in other homes too; peer.call {{\"name\":\"peer\",\"command\":...,\"args\":{{...}}}} runs other commands there\n- task.create {{\"id\":\"unique-child\",\"parent\":\"task\",\"owner\":\"department\",\"title\":\"bounded outcome\"}} (as your own, never for a peer)\n- task.handoff {{\"id\":\"task\",\"actor\":\"department\",\"target\":\"peer-owner\"}}; custody changes only when target calls task.accept.\nRead structured results; do not claim a message, handoff, test, merge or deployment succeeded without evidence.",
        context["repo"].as_str().unwrap(),
        context["control"]["cli_argv"],
    );
    assert_eq!(brief["brief"].as_str().unwrap(), expected);
}

#[test]
fn native_profiles_plan_explicit_resume_and_interactive_commands_without_core_changes() {
    let mut f = Fixture::new();
    let id = "46a81234-1234-4567-8123-123456789abc";
    for name in [
        "codex",
        "pi",
        "pi-signed",
        "omp",
        "claude",
        "opencode",
        "opencode-v2",
        "gemini",
        "cursor",
        "grok",
        "agy",
        "muse",
        "devin",
        "kimi",
        "rovo",
    ] {
        let mut args = f.args(&example(name));
        let planned = plugins::dispatch(&mut f.store, "plugin.command", args.clone()).unwrap();
        assert_eq!(planned["cwd"], json!(f.repo));
        if matches!(name, "rovo" | "grok" | "omp") {
            assert!(
                planned["argv"][1]
                    .as_str()
                    .unwrap()
                    .ends_with(if name == "omp" {
                        "omp_rpc.py"
                    } else {
                        "acp.py"
                    })
            );
        } else if name == "claude" {
            assert_eq!(planned["argv"][2], "--control");
            let control: Value =
                serde_json::from_str(planned["argv"][3].as_str().unwrap()).unwrap();
            assert_eq!(control["cli_argv"][2], json!(f.store.home()));
            assert_eq!(planned["argv"][4], "claude");
        } else {
            assert_eq!(planned["argv"][2], name);
        }
        assert_eq!(planned["env_names"], json!([]));
        let resume = if matches!(name, "opencode" | "opencode-v2") {
            "ses_0123456789abc"
        } else {
            id
        };
        args["options"] = json!({"resume":resume});
        let resumed = plugins::dispatch(&mut f.store, "plugin.command", args.clone());
        if name == "muse" {
            assert!(
                resumed.is_err(),
                "unqualified headless resume must not become a fresh run"
            );
        } else {
            let resumed = resumed.unwrap();
            if matches!(name, "rovo" | "grok" | "omp") {
                let payload: Value =
                    serde_json::from_str(resumed["argv"][3].as_str().unwrap()).unwrap();
                assert_eq!(payload["resume"], resume);
            } else {
                assert!(
                    resumed["argv"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|s| s == resume)
                );
            }
            assert!(
                !resumed["argv"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|s| s == "--last" || s == "--continue")
            );
        }
        args["options"] = json!({"mode":"interactive"});
        let plan = plugins::dispatch(&mut f.store, "plugin.command", args);
        if matches!(name, "kimi" | "rovo") {
            assert!(
                plan.is_err(),
                "Kimi requires readiness-gated interactive delivery; a bare launch must not lose the brief"
            );
            continue;
        }
        let plan = plan.unwrap();
        assert!(
            !plan["argv"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s == "--print" || s == "--single" || s == "exec")
        );
    }
    for invalid in [
        json!({"resume":"latest"}),
        json!({"resume":"46a81234"}),
        json!({"resume":id,"ephemeral":true}),
        json!({"mode":"injet"}),
        json!({"model":"--steal-config"}),
        json!({"session":id}),
        json!({"endpoint":"https://unexpected.example.invalid"}),
    ] {
        let mut args = f.args(&example("codex"));
        args["options"] = invalid;
        assert!(plugins::dispatch(&mut f.store, "plugin.command", args).is_err());
    }
    let mut args = f.args(&example("gemini"));
    args["options"] = json!({"effort":"high"});
    assert!(
        plugins::dispatch(&mut f.store, "plugin.command", args).is_err(),
        "unsupported options are not silently omitted"
    );
    for name in ["rovo", "kimi", "devin"] {
        let mut args = f.args(&example(name));
        args["options"] =
            json!({"transport":"acp","resume":"saved-session","acp_permissions":"allow_once"});
        let plan = plugins::dispatch(&mut f.store, "plugin.command", args.clone()).unwrap();
        assert!(plan["argv"][1].as_str().unwrap().ends_with("acp.py"));
        let payload: Value = serde_json::from_str(plan["argv"][3].as_str().unwrap()).unwrap();
        assert_eq!(payload["binary"], name);
        assert_eq!(payload["resume"], "saved-session");
        assert_eq!(payload["permissions"], "auto");
        for invalid in [
            json!({"transport":"acp","mode":"interactive"}),
            json!({"transport":"acp","permission_mode":"autonomous"}),
            json!({"transport":"native","acp_permissions":"allow_once"}),
        ] {
            args["options"] = invalid;
            assert!(plugins::dispatch(&mut f.store, "plugin.command", args.clone()).is_err());
        }
    }
}

#[test]
fn grok_and_agy_use_explicit_owned_native_controls_without_global_configuration() {
    let mut f = Fixture::new();
    let mut args = f.args(&example("grok"));
    args["options"] =
        json!({"model":"fixture-model","effort":"low","acp_permissions":"allow_once"});
    let plan = plugins::dispatch(&mut f.store, "plugin.command", args.clone()).unwrap();
    assert!(plan["argv"][1].as_str().unwrap().ends_with("acp.py"));
    let payload: Value = serde_json::from_str(plan["argv"][3].as_str().unwrap()).unwrap();
    assert_eq!(payload["binary"], "grok");
    assert_eq!(payload["permissions"], "auto");
    assert_eq!(payload["effort"], "low");
    for invalid in [
        json!({"transport":"native"}),
        json!({"permission_mode":"bypassPermissions"}),
        json!({"transport":"acp","mode":"interactive"}),
    ] {
        args["options"] = invalid;
        assert!(plugins::dispatch(&mut f.store, "plugin.command", args.clone()).is_err());
    }
    let mut args = f.args(&example("agy"));
    args["options"] = json!({"skip_permissions":true,"permission_mode":"accept-edits","sandbox":true,"disable_slash_commands":true});
    let plan = plugins::dispatch(&mut f.store, "plugin.command", args.clone()).unwrap();
    let argv = plan["argv"].as_array().unwrap();
    assert!(argv.iter().any(|v| v == "--dangerously-skip-permissions"));
    assert!(argv.iter().any(|v| v == "--sandbox"));
    assert!(argv.iter().any(|v| v == "--disable-slash-commands"));
    assert_eq!(
        argv.windows(2)
            .filter(|v| v[0] == "--add-dir" && v[1] == json!(f.store.home()))
            .count(),
        1
    );
    args["options"] = json!({});
    let plan = plugins::dispatch(&mut f.store, "plugin.command", args.clone()).unwrap();
    assert!(
        !plan["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "--dangerously-skip-permissions" || v == "--sandbox")
    );
    for invalid in [
        json!({"skip_permissions":"true"}),
        json!({"permission_mode":"bypassPermissions"}),
        json!({"sandbox":"true"}),
        json!({"disable_slash_commands":true,"mode":"interactive"}),
    ] {
        args["options"] = invalid;
        assert!(plugins::dispatch(&mut f.store, "plugin.command", args.clone()).is_err());
    }
}

#[test]
fn omp_workers_plan_native_permissions_and_verified_rpc_without_provider_configuration() {
    let mut f = Fixture::new();
    let mut args = f.args(&example("omp"));
    args["options"] = json!({"model":"fixture/fixture-model","effort":"low","bare":true,"permission_mode":"write"});
    let plan = plugins::dispatch(&mut f.store, "plugin.command", args.clone()).unwrap();
    assert!(plan["argv"][1].as_str().unwrap().ends_with("omp_rpc.py"));
    let request: Value = serde_json::from_str(plan["argv"][3].as_str().unwrap()).unwrap();
    let argv = request["argv"].as_array().unwrap();
    assert!(
        argv.windows(2)
            .any(|v| v[0] == "--approval-mode" && v[1] == "write")
    );
    assert!(
        argv.windows(2)
            .any(|v| v[0] == "--cwd" && v[1] == json!(f.repo))
    );
    assert_eq!(
        argv.windows(2)
            .filter(|v| v[0] == "--add-dir" && v[1] == json!(f.store.home()))
            .count(),
        1
    );
    assert!(argv.iter().any(|v| v == "--no-extensions"));
    assert!(argv.iter().any(|v| v == "--no-prewalk"));
    assert!(
        !argv
            .iter()
            .any(|v| v == "--print" || v == "--continue" || v == "--auto-approve")
    );
    args["options"] = json!({"transport":"native"});
    let plan = plugins::dispatch(&mut f.store, "plugin.command", args.clone()).unwrap();
    assert!(
        plan["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "--print")
    );
    assert!(
        !plan["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "--approval-mode")
    );
    for invalid in [
        json!({"transport":"rpc","mode":"interactive"}),
        json!({"transport":"native","resume":"46a81234-1234-4567-8123-123456789abc"}),
        json!({"mode":"interactive","resume":"46a81234-1234-4567-8123-123456789abc"}),
        json!({"permission_mode":"bypassPermissions"}),
        json!({"bare":"true"}),
        json!({"provider":"fixture"}),
    ] {
        args["options"] = invalid;
        assert!(plugins::dispatch(&mut f.store, "plugin.command", args.clone()).is_err());
    }
}

#[test]
fn muse_msp_plans_exact_resume_and_runtime_roots_without_changing_native_defaults() {
    let mut f = Fixture::new();
    let mut args = f.args(&example("muse"));
    args["options"] = json!({"transport":"msp","provider":"meta","model":"fixture-model","effort":"low","resume":"46a81234-1234-4567-8123-123456789abc","msp_permissions":"allow_once","msp_approval_mode":"onRequest","trust":true,"sandbox_network":"restricted"});
    let plan = plugins::dispatch(&mut f.store, "plugin.command", args.clone()).unwrap();
    assert!(plan["argv"][1].as_str().unwrap().ends_with("msp.py"));
    let request: Value = serde_json::from_str(plan["argv"][3].as_str().unwrap()).unwrap();
    assert_eq!(request["resume"], args["options"]["resume"]);
    assert_eq!(request["roots"][0], json!(f.repo));
    assert!(
        request["roots"]
            .as_array()
            .unwrap()
            .contains(&json!(f.store.home()))
    );
    assert_eq!(request["permissions"], "allow_once");
    assert_eq!(request["approval_mode"], "onRequest");
    let argv = request["argv"].as_array().unwrap();
    assert!(argv.iter().any(|v| v == "--trust-workspace"));
    assert!(!argv.iter().any(|v| v == "--disable-sandbox"));
    args["options"] = json!({"provider":"meta","permission_mode":"never","bare":true});
    let plan = plugins::dispatch(&mut f.store, "plugin.command", args.clone()).unwrap();
    let argv = plan["argv"].as_array().unwrap();
    assert!(argv.iter().any(|v| v == "exec"));
    assert!(argv.iter().any(|v| v == "--no-foreign-personal-context"));
    assert!(!argv.iter().any(|v| v == "--disable-sandbox"));
    for invalid in [
        json!({"transport":"msp","mode":"interactive"}),
        json!({"transport":"msp","bare":true}),
        json!({"transport":"msp","permission_mode":"never"}),
        json!({"msp_permissions":"allow_once"}),
        json!({"transport":"msp","msp_permissions":"allow_always"}),
        json!({"transport":"msp","trust":"true"}),
        json!({"transport":"msp","sandbox_network":"unrestricted"}),
        json!({"bare":true,"mode":"interactive"}),
        json!({"permission_mode":"allowAll"}),
    ] {
        args["options"] = invalid;
        assert!(plugins::dispatch(&mut f.store, "plugin.command", args.clone()).is_err());
    }
}

#[test]
fn claude_grants_the_privateer_cli_without_relying_on_workspace_trust() {
    // A ship made by ship.create is never opened interactively, so Claude
    // ignores its settings.json allow list. The grant must be on argv.
    let mut f = Fixture::new();
    let mut args = f.args(&example("claude"));
    args["options"] = json!({"permission_mode":"acceptEdits"});
    let plan = plugins::dispatch(&mut f.store, "plugin.command", args).unwrap();
    let argv: Vec<&str> = plan["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let exe = std::env::current_exe().unwrap();
    let grant = format!("Bash({}:*)", exe.display());
    let flag = argv.iter().position(|a| *a == "--allowedTools").unwrap();
    assert_eq!(argv[flag + 1], grant);
    // The grant must not be read as part of the prompt.
    assert!(flag < argv.iter().position(|a| *a == "--").unwrap());
}

#[test]
fn claude_first_mates_delegate_through_the_fleet_and_no_run_schedules_detached_work() {
    let mut f = Fixture::new();
    let plan = |f: &mut Fixture, id: &str| {
        let mut args = f.args(&example("claude"));
        args["id"] = json!(id);
        let plan = plugins::dispatch(&mut f.store, "plugin.command", args).unwrap();
        let argv: Vec<String> = plan["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        let end = argv.iter().position(|a| a == "--").unwrap();
        let flag = argv.iter().position(|a| a == "--disallowedTools").unwrap();
        let next = argv[flag + 1..end]
            .iter()
            .position(|a| a.starts_with("--"))
            .map_or(end, |at| flag + 1 + at);
        (argv[flag + 1..next].to_vec(), argv[..end].to_vec())
    };
    let (crew, argv) = plan(&mut f, "crew-job");
    assert!(crew.contains(&"CronCreate".into()) && crew.contains(&"RemoteTrigger".into()));
    assert!(
        !crew.contains(&"Task".into()),
        "crew may use Claude's own agents"
    );
    // The home holds the ship's records; Claude's file tools may read it.
    let home = f.store.home().canonicalize().unwrap();
    assert!(
        argv.windows(2)
            .any(|w| w[0] == "--add-dir" && w[1] == home.to_str().unwrap())
    );
    f.store
        .connection()
        .execute(
            "INSERT INTO wakes(id,ship,focus,through_seq,job_id,mode,status,created_ms)
             VALUES('wake','department','ship',1,'wake-job','fresh','running',0)",
            [],
        )
        .unwrap();
    let (first_mate, _) = plan(&mut f, "wake-job");
    for tool in ["Task", "Agent", "Workflow", "EnterWorktree", "CronCreate"] {
        assert!(first_mate.contains(&tool.into()), "{tool}: {first_mate:?}");
    }
}

#[test]
fn native_exec_preserves_role_and_routes_but_does_not_inherit_foreign_harness_identity() {
    let mut f = Fixture::new();
    let fakebin = f.base().join("bin");
    fs::create_dir(&fakebin).unwrap();
    let fake = fakebin.join("claude");
    fs::write(&fake, "#!/usr/bin/env python3\nimport json,os,sys\nsid='4fa4f4bc-5b2d-4553-8ae1-eb09e501ea63'\nprint(json.dumps({'type':'system','subtype':'init','session_id':sid,'argv':sys.argv[1:],'role':os.getenv('PRIVATEER_ROLE'),'actor':os.getenv('PRIVATEER_ACTOR'),'route':os.getenv('ANTHROPIC_BASE_URL'),'pi_config':os.getenv('PI_CODING_AGENT_DIR'),'token':os.getenv('SYNTHETIC_TEST_TOKEN'),'foreign':[k for k in ['CLAUDECODE','GEMINI_CLI','PI_CODING_AGENT','CURSOR_AGENT'] if k in os.environ]}))\nprint(json.dumps({'type':'result','subtype':'success','is_error':False,'session_id':sid}))\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let mut args = f.args(&example("claude"));
    args["options"] = json!({"model":"fixture-model","permission_mode":"plan"});
    let plan = plugins::dispatch(&mut f.store, "plugin.command", args).unwrap();
    let argv = plan["argv"].as_array().unwrap();
    let output = support::command(argv[0].as_str().unwrap())
        .args(argv[1..].iter().map(|v| v.as_str().unwrap()))
        .env_clear()
        .env(
            "PATH",
            format!("{}:{}", fakebin.display(), std::env::var("PATH").unwrap()),
        )
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "department")
        .env("ANTHROPIC_BASE_URL", "https://fixture.example.invalid")
        .env("PI_CODING_AGENT_DIR", f.base().join("native-private"))
        .env("SYNTHETIC_TEST_TOKEN", "not-a-real-token")
        .env("CLAUDECODE", "1")
        .env("GEMINI_CLI", "1")
        .env("PI_CODING_AGENT", "true")
        .env("CURSOR_AGENT", "1")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let actual: Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    assert_eq!(actual["foreign"], json!([]));
    assert_eq!(actual["role"], "agent");
    assert_eq!(actual["actor"], "department");
    assert_eq!(actual["route"], "https://fixture.example.invalid");
    assert_eq!(actual["pi_config"], json!(f.base().join("native-private")));
    assert_eq!(actual["token"], "not-a-real-token");
    assert!(
        actual["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "fixture-model")
    );
    // Empty PATH cannot fall back to another installed harness or provider.
    // The resolver also looks in the mise data directory, so point it at an
    // empty one: a machine with its own mise-installed claude must not leak in.
    let failed = support::command(argv[0].as_str().unwrap())
        .args(argv[1..].iter().map(|v| v.as_str().unwrap()))
        .env_clear()
        .env("PATH", f.base().join("missing-bin"))
        .env("MISE_DATA_DIR", f.base().join("no-mise"))
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(!String::from_utf8_lossy(&failed.stderr).contains("fixture-model"));
}

#[test]
fn native_session_evidence_ignores_tool_text_and_bounds_malformed_logs() {
    let f = Fixture::new();
    let script = r#"
import json,sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from native_events import read_session, session_started
root=Path(sys.argv[2])
log=root/'native.log'
identity='46a81234-1234-4567-8123-123456789abc'
for event in [
    {'type':'item.completed','item':{'thread_id':identity}},
    {'type':'assistant','session_id':identity},
    {'type':'error','thread_id':identity},
    {'type':'system','subtype':'tool','session_id':identity},
    [], None,
]:
    assert session_started('codex',event) is None
    assert session_started('claude',event) is None
log.write_text('not json\n'+json.dumps({'type':'thread.started','thread_id':identity})+'\n')
assert read_session('codex',log)==identity
assert read_session('claude',log) is None
log.write_text('x'*65537+'\n'+json.dumps({'type':'thread.started','thread_id':identity})+'\n')
assert read_session('codex',log) is None
assert session_started('codex',{'type':'thread.started','thread_id':'bad\nidentity'}) is None
for harness in ('rovo','kimi','devin','grok','muse'):
    assert session_started(harness,{'type':'session.started','harness':harness,'session_id':identity})==identity
    assert session_started(harness,{'type':'session.started','harness':'unrelated','session_id':identity}) is None
assert session_started('agy',{'event':'init','conversation_id':identity,'init':{'cwd':str(root)}})==identity
assert session_started('agy',{'event':'step_update','step_update':{'event':'init','conversation_id':identity}}) is None
assert session_started('agy',{'type':'init','session_id':identity}) is None
"#;
    let output = support::command(support::python())
        .args(["-c", script])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins"))
        .arg(f.base())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Runs one of the Python suites that qualify a native bridge across real
/// pipes and processes. Each is its own test, so the suites run in parallel.
fn native_stream_bridge_qualifies(script: &str) {
    let output = support::command(support::python())
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join(script))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

macro_rules! native_stream_bridges {
    ($($test:ident: $script:literal,)*) => {$(
        #[test]
        fn $test() {
            native_stream_bridge_qualifies($script);
        }
    )*};
}

native_stream_bridges! {
    native_stream_bridge_acp_qualifies: "tests/acp_bridge.py",
    native_stream_bridge_pi_turn_qualifies: "tests/pi_turn.py",
    native_stream_bridge_msp_qualifies: "tests/msp_bridge.py",
    native_stream_bridge_omp_rpc_qualifies: "tests/omp_rpc.py",
    native_stream_bridge_agy_turn_qualifies: "tests/agy_turn.py",
    native_stream_bridge_result_turn_qualifies: "tests/result_turn.py",
    native_stream_bridge_claude_turn_qualifies: "tests/claude_turn.py",
    native_stream_bridge_tool_permissions_qualifies: "tests/tool_permissions.py",
    native_stream_bridge_opencode_v2_qualifies: "tests/opencode_v2.py",
    native_stream_bridge_opencode_watch_qualifies: "tests/opencode_watch.py",
}

#[test]
fn native_zero_exit_after_a_provider_error_records_a_failed_wake_not_a_warm_session() {
    for harness in ["pi", "cursor", "gemini"] {
        let mut f = Fixture::new();
        fs::write(
            f.store.home().join("config.json"),
            json!({"resource_home":f.base().join("ledger")}).to_string(),
        )
        .unwrap();
        let fakebin = f.base().join("bin");
        fs::create_dir(&fakebin).unwrap();
        let fake = fakebin.join(if harness == "cursor" {
            "cursor-agent"
        } else {
            harness
        });
        fs::write(
        &fake,
        r#"#!/usr/bin/env python3
import json,os
harness=os.path.basename(__file__)
identity='46a81234-1234-4567-8123-123456789abc'
events = [
    {'type':'session','id':'46a81234-1234-4567-8123-123456789abc'},
    {'type':'agent_start'},
    {'type':'message_end','message':{'role':'assistant','stopReason':'error','usage':{'input':2}}},
    {'type':'agent_end','willRetry':False},
    {'type':'agent_settled','aborted':False},
]
if harness=='cursor-agent':
    events=[{'type':'system','subtype':'init','session_id':identity,'cwd':os.getcwd()},
            {'type':'result','subtype':'error','is_error':True,'session_id':identity,'usage':{'inputTokens':2}}]
elif harness=='gemini':
    events=[{'type':'init','session_id':identity},
            {'type':'result','status':'error','stats':{'input_tokens':2}}]
for event in events:print(json.dumps(event),flush=True)
"#,
    )
    .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        privateer::dispatch(&mut f.store,"ship.put",json!({"name":"department","scope":"Native failure fixture","first_mate":{"manifest":example(harness),"duty_task":"task","batch_secs":0,"max_delay_secs":0}})).unwrap();
        privateer::dispatch(&mut f.store,"mail.send",json!({"id":"pi-work","from":"captain","to":"department","kind":"request","body":"Handle this fixture"})).unwrap();
        let start = support::command(env!("CARGO_BIN_EXE_privateer"))
            .args([
                "--home",
                f.store.home().to_str().unwrap(),
                "serve",
                "--json",
                "{\"once\":true}",
            ])
            .env(
                "PATH",
                format!("{}:{}", fakebin.display(), std::env::var("PATH").unwrap()),
            )
            .output()
            .unwrap();
        assert!(
            start.status.success(),
            "{}",
            String::from_utf8_lossy(&start.stderr)
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let wake = loop {
            privateer::activation::tick(&mut f.store).unwrap();
            let wakes =
                privateer::dispatch(&mut f.store, "wake.list", json!({"ship":"department"}))
                    .unwrap();
            let wake = &wakes["wakes"][0];
            if wake["status"] != "running" {
                break wake.clone();
            }
            assert!(
                Instant::now() < deadline,
                "{harness} fixture wake did not finish"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(wake["status"], "failed", "{wake}");
        assert_eq!(wake["usage"]["input"], 2);
        let sessions: i64 = f
            .store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE ship='department'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            sessions, 0,
            "a provider failure became a warm resumable session"
        );
        let mail = privateer::dispatch(&mut f.store, "mail.show", json!({"id":"pi-work"})).unwrap();
        assert_eq!(mail["state"], "open");
    }
}

#[test]
fn stdio_first_mates_resume_focused_wakes_under_the_existing_mail_and_role_contract() {
    for harness in ["rovo", "grok", "muse", "omp", "agy", "cursor", "gemini"] {
        let mut f = Fixture::new();
        fs::write(
            f.store.home().join("config.json"),
            json!({"resource_home":f.base().join("ledger")}).to_string(),
        )
        .unwrap();
        let fakebin = f.base().join("bin");
        fs::create_dir(&fakebin).unwrap();
        let server = fakebin.join(if harness == "cursor" {
            "cursor-agent"
        } else {
            harness
        });
        let script = if matches!(harness, "omp" | "agy" | "cursor" | "gemini") {
            format!(
                "#!/usr/bin/env python3\nimport sys\nsys.path.insert(0,{:?})\nfrom {} import SERVER\nexec(SERVER)\n",
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests")
                    .display()
                    .to_string(),
                match harness {
                    "omp" => "omp_rpc",
                    "agy" => "agy_turn",
                    _ => "result_turn",
                }
            )
        } else {
            r#"#!/usr/bin/env python3
import json,os,subprocess,sys
harness=os.path.basename(sys.argv[0])
sid='46a81234-1234-4567-8123-123456789abc'
turn='46a81234-1234-4567-8123-123456789abd'
def call(command,args):
    output=subprocess.run([os.environ['STDIO_TEST_CLI'],'--home',os.environ['STDIO_TEST_HOME'],command,'--json',json.dumps(args)],capture_output=True,text=True)
    assert output.returncode==0,command
    return json.loads(output.stdout)
for line in sys.stdin:
    request=json.loads(line)
    method=request['method']
    if method=='initialize':
        result={'schema':{'version':1},'sessionDurability':'durable'} if harness=='muse' else {'protocolVersion':1,'agentCapabilities':{'loadSession':True}}
    elif method=='initialized':continue
    elif method in ('session/new','session/load'):result={'sessionId':sid,'_meta':{'currentWorkingDirectory':os.getcwd()}}
    elif method in ('session/start','session/resume'):
        if method=='session/resume':assert request['params']['sessionId']==sid
        result={'session':{'sessionId':sid,'workspaceRoot':os.getcwd(),'status':'idle','activeTurnId':None},'pendingRequests':[]}
    elif method in ('session/prompt','turn/start'):
        assert os.environ['PRIVATEER_ROLE']=='agent' and os.environ['PRIVATEER_ACTOR']=='department'
        prompt=request['params']['input' if harness=='muse' else 'prompt'][0]['text']
        assert 'New mail through seq' in prompt
        inbox=call('mail.inbox',{'address':'department'})
        for mail in inbox['messages']:
            if mail['kind']=='request' and mail['state']=='open':
                call('mail.send',{'id':'reply-'+mail['id'],'from':'department','to':mail['from'],'kind':'reply','correlation':mail['id'],'body':'handled through stdio protocol fixture'})
        call('mail.ack',{'address':'department','through_seq':max(m['seq'] for m in inbox['messages'])})
        result={'commandId':request['params']['commandId'],'status':'accepted','turnId':turn,'disposition':'started','startedNewTurn':True} if harness=='muse' else {'stopReason':'end_turn'}
    else:raise RuntimeError('unexpected method')
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}),flush=True)
    if method=='turn/start':print(json.dumps({'jsonrpc':'2.0','method':'turn/completed','params':{'sessionId':sid,'turnId':turn,'terminal':'completed'}}),flush=True)
"#.to_owned()
        };
        fs::write(&server, script).unwrap();
        fs::set_permissions(&server, fs::Permissions::from_mode(0o700)).unwrap();
        let options = if harness == "muse" {
            json!({"transport":"msp"})
        } else {
            json!({})
        };
        privateer::dispatch(&mut f.store,"ship.put",json!({"name":"department","scope":"stdio protocol fixture","first_mate":{"manifest":example(harness),"options":options,"duty_task":"task","batch_secs":0,"max_delay_secs":0}})).unwrap();
        for (index, id) in ["stdio-work", "stdio-more"].into_iter().enumerate() {
            privateer::dispatch(&mut f.store,"mail.send",json!({"id":id,"from":"captain","to":"department","kind":"request","task_id":"task","body":"Handle the isolated protocol test"})).unwrap();
            let tick = support::command(env!("CARGO_BIN_EXE_privateer"))
                .args([
                    "--home",
                    f.store.home().to_str().unwrap(),
                    "serve",
                    "--json",
                    "{\"once\":true}",
                ])
                .env(
                    "PATH",
                    format!("{}:{}", fakebin.display(), std::env::var("PATH").unwrap()),
                )
                .env("STDIO_TEST_CLI", env!("CARGO_BIN_EXE_privateer"))
                .env("STDIO_TEST_HOME", f.store.home())
                .env("OMP_TEST_RUNTIME_CLI", env!("CARGO_BIN_EXE_privateer"))
                .env("OMP_TEST_RUNTIME_HOME", f.store.home())
                .output()
                .unwrap();
            assert!(
                tick.status.success(),
                "{}",
                String::from_utf8_lossy(&tick.stderr)
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            let wake = loop {
                privateer::activation::tick(&mut f.store).unwrap();
                let wakes =
                    privateer::dispatch(&mut f.store, "wake.list", json!({"ship":"department"}))
                        .unwrap();
                assert_eq!(wakes["wakes"].as_array().unwrap().len(), index + 1);
                let wake = &wakes["wakes"][0];
                if wake["status"] != "running" {
                    break wake.clone();
                }
                assert!(
                    Instant::now() < deadline,
                    "{harness} fixture wake did not finish"
                );
                std::thread::sleep(Duration::from_millis(20));
            };
            assert_eq!(wake["status"], "succeeded", "{harness}: {wake}");
            assert_eq!(
                wake["mode"],
                if index == 0 { "fresh" } else { "resume" },
                "{wake}"
            );
            assert_eq!(wake["session_id"], "46a81234-1234-4567-8123-123456789abc");
            if matches!(harness, "omp" | "agy") {
                assert_eq!(wake["usage"], json!({"input":4,"output":2}));
            } else {
                assert!(
                    wake["usage"].is_null(),
                    "unreported native usage must remain absent"
                );
            }
            let request = privateer::dispatch(&mut f.store, "mail.show", json!({"id":id})).unwrap();
            assert_eq!(request["state"], "answered");
        }
    }
}

#[test]
fn adapters_cannot_change_charter_or_return_environment_values() {
    let mut f = Fixture::new();
    for result in [
        "{'argv':['fake'],'cwd':'/','env_names':[]}".to_owned(),
        "{'argv':['fake'],'cwd':r['context']['repo'],'env_names':['INVALID-NAME']}".to_owned(),
        "{'argv':['fake'],'cwd':r['context']['repo'],'env_names':[],'env':{'TOKEN':'not-a-real-token'}}".to_owned(),
        "{'argv':['fake'],'cwd':r['context']['repo'],'env_names':[],'actions':['deploy']}".to_owned(),
    ] {
        let path = manifest(&f,&response_script(&result),2000,65536);
        let args = f.args(&path);
        assert!(plugins::dispatch(&mut f.store,"plugin.command",args).is_err());
    }
    let path = manifest(
        &f,
        &response_script("{'valid':True,'secret':'not-a-real-token'}"),
        2000,
        65536,
    );
    let args = f.args(&path);
    let result = plugins::dispatch(&mut f.store, "plugin.validate", args).unwrap();
    assert!(result.get("secret").is_none());
}

#[test]
fn timeout_bounds_blocked_stdin_and_inherited_output() {
    let mut f = Fixture::new();
    for body in [
        "import time; time.sleep(5)",
        "import os,time\nif os.fork()==0: time.sleep(5)\nelse: os._exit(0)",
    ] {
        let path = manifest(&f, body, 100, 65536);
        let mut args = f.args(&path);
        args["options"] = json!({"padding":"x".repeat(40000)});
        let started = Instant::now();
        let error = plugins::dispatch(&mut f.store, "plugin.validate", args).unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}

/// An adapter runs with the first mate's environment, so its files must be
/// this user's alone; it still imports its siblings, but not from the
/// environment's `PYTHONPATH` or user site-packages.
#[test]
fn adapters_import_siblings_without_the_environment_and_refuse_writable_files() {
    let mut f = Fixture::new();
    fs::write(f.base().join("sibling.py"), "VALUE = 'sibling'\n").unwrap();
    let path = manifest(
        &f,
        &response_script(
            "{'valid': bool(sys.flags.ignore_environment and sys.flags.no_user_site and __import__('sibling').VALUE == 'sibling')}",
        ),
        1000,
        65536,
    );
    let args = f.args(&path);
    let result = plugins::dispatch(&mut f.store, "plugin.validate", args).unwrap();
    assert_eq!(result["valid"], true);
    let script = f.base().join("adapter.py");
    let directory = f.base().to_path_buf();
    for (file, loose, what) in [
        (script.as_path(), 0o666, "plugin script"),
        (path.as_path(), 0o666, "plugin manifest"),
        (directory.as_path(), 0o777, "plugin directory"),
    ] {
        let kept = fs::metadata(file).unwrap().permissions();
        fs::set_permissions(file, fs::Permissions::from_mode(loose)).unwrap();
        let args = f.args(&path);
        let error = plugins::dispatch(&mut f.store, "plugin.validate", args).unwrap_err();
        fs::set_permissions(file, kept).unwrap();
        let message = error.to_string();
        assert!(
            message.starts_with(what) && message.contains("world-writable"),
            "{message}"
        );
    }
    let args = f.args(&path);
    assert!(plugins::dispatch(&mut f.store, "plugin.validate", args).is_ok());
}

#[test]
fn launch_planning_outwaits_a_slow_plugin_that_an_ordinary_request_does_not() {
    let mut f = Fixture::new();
    let repo = f.repo.display().to_string();
    // Answers well after its declared timeout, well inside the launch floor.
    let body = response_script(&format!(
        "{{'argv':['true'],'cwd':{repo:?},'env_names':[]}}"
    ))
    .replacen("\nprint(", "\nimport time; time.sleep(1.5)\nprint(", 1);
    let path = manifest(&f, &body, 300, 65536);
    let args = f.args(&path);
    let error = plugins::dispatch(&mut f.store, "plugin.command", args).unwrap_err();
    assert!(
        error.to_string().contains("timed out after 300 ms"),
        "{error}"
    );
    let mut args = f.args(&path);
    args["id"] = json!("slow-plan");
    let job = privateer::dispatch(&mut f.store, "agent.start", args).unwrap();
    assert_eq!(job["command"], json!(["true"]));
}

#[test]
fn stdout_stderr_and_response_identity_are_bounded() {
    let mut f = Fixture::new();
    for body in [
        "import sys;sys.stdout.write('x'*10000)",
        "import sys;sys.stderr.write('x'*10000)",
        "import json;print(json.dumps({'id':'wrong','version':1,'ok':True,'result':{'valid':True}}))",
        "print('{}');print('{}')",
    ] {
        let path = manifest(&f, body, 1000, 128);
        let args = f.args(&path);
        assert!(plugins::dispatch(&mut f.store, "plugin.validate", args).is_err());
    }
    let path = manifest(
        &f,
        "import sys;sys.stderr.write('not-a-real-token');sys.exit(1)",
        1000,
        65536,
    );
    let args = f.args(&path);
    let error = plugins::dispatch(&mut f.store, "plugin.validate", args).unwrap_err();
    assert!(!error.to_string().contains("not-a-real-token"));
}

#[test]
fn revocation_prevents_plugin_execution_and_public_config_is_refused() {
    let mut f = Fixture::new();
    let config = f.base().join("private.json");
    fs::write(&config, "{}").unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();
    let mut args = f.args(&example("pi"));
    args["config"] = json!(config);
    assert!(plugins::dispatch(&mut f.store, "plugin.validate", args).is_err());
    f.store
        .connection()
        .execute(
            "UPDATE charters SET data=json_set(data,'$.active',json('false')) WHERE id='mission'",
            [],
        )
        .unwrap();
    let args = f.args(&example("pi"));
    assert!(plugins::dispatch(&mut f.store, "plugin.command", args).is_err());
}

#[test]
fn charter_selected_provider_adapter_cannot_be_silently_switched() {
    let mut f = Fixture::new();
    let pin = json!({"manifest":example("pi")});
    f.store.connection().execute("UPDATE charters SET data=json_set(data,'$.provider_plugin',json(?1)) WHERE id='mission'",[pin.to_string()]).unwrap();
    let args = f.args(&example("codex"));
    let error = plugins::dispatch(&mut f.store, "plugin.command", args).unwrap_err();
    assert!(error.to_string().contains("selected provider"));
    assert!(plugins::dispatch(&mut f.store, "plugin.command", json!({"task_id":"task"})).is_ok());
    let config = f.base().join("additional.json");
    private(&config, json!({}));
    assert!(
        plugins::dispatch(
            &mut f.store,
            "plugin.command",
            json!({"task_id":"task","config":config})
        )
        .is_err()
    );
    let (agent, gateway_config) = gateway_fixture(&f);
    let pin = json!({"manifest":example("gateway-pi"),"config":gateway_config});
    f.store.connection().execute("UPDATE charters SET data=json_set(data,'$.provider_plugin',json(?1)) WHERE id='mission'",[pin.to_string()]).unwrap();
    let args = f.args(&example("pi"));
    assert!(plugins::dispatch(&mut f.store, "plugin.command", args).is_err());
    assert!(
        plugins::dispatch(
            &mut f.store,
            "plugin.command",
            json!({"task_id":"task","config":config})
        )
        .is_err()
    );
    let output = support::command(env!("CARGO_BIN_EXE_privateer"))
        .args([
            "--home",
            f.store.home().to_str().unwrap(),
            "plugin.command",
            "--json",
            "{\"task_id\":\"task\"}",
        ])
        .env("PI_CODING_AGENT_DIR", agent)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["argv"][3], json!(gateway_config));
}

fn gateway_fixture(f: &Fixture) -> (PathBuf, PathBuf) {
    let agent = f.base().join("pi-agent");
    fs::create_dir(&agent).unwrap();
    private(
        &agent.join("models.json"),
        json!({"providers":{"fixture-route":{"baseUrl":"http://127.0.0.1:12345/v1","apiKey":"not-a-real-token","models":[{"id":"fixture-model"}]}}}),
    );
    let config = f.base().join("gateway.json");
    private(
        &config,
        json!({"schema_version":1,"pi_config":agent.join("models.json"),"endpoint":"http://127.0.0.1:12345/v1","provider":"fixture-route","model":"fixture-model"}),
    );
    (agent, config)
}
fn gateway_call(f: &Fixture, agent: &Path, config: &Path, method: &str) -> std::process::Output {
    support::command(env!("CARGO_BIN_EXE_privateer"))
        .args([
            "--home",
            f.store.home().to_str().unwrap(),
            method,
            "--json",
            &json!({"task_id":"task","manifest":example("gateway-pi"),"config":config}).to_string(),
        ])
        .env("PI_CODING_AGENT_DIR", agent)
        .output()
        .unwrap()
}

#[test]
fn gateway_plans_and_launches_only_exact_native_route_without_exposing_credentials() {
    let f = Fixture::new();
    let (agent, config) = gateway_fixture(&f);
    let output = gateway_call(&f, &agent, &config, "plugin.command");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!String::from_utf8_lossy(&output.stdout).contains("not-a-real-token"));
    let resume = "46a81234-1234-4567-8123-123456789abc";
    let gateway_options = |options: Value| {
        support::command(env!("CARGO_BIN_EXE_privateer"))
        .args(["--home", f.store.home().to_str().unwrap(), "plugin.command", "--json",
            &json!({"task_id":"task","manifest":example("gateway-pi"),"config":config,"options":options}).to_string()])
        .env("PI_CODING_AGENT_DIR", &agent).output().unwrap()
    };
    let resumed = gateway_options(json!({"resume":resume,"effort":"high"}));
    assert!(resumed.status.success());
    let resumed_plan: Value = serde_json::from_slice(&resumed.stdout).unwrap();
    let resumed_options: Value =
        serde_json::from_str(resumed_plan["argv"][5].as_str().unwrap()).unwrap();
    assert_eq!(resumed_options["resume"], resume);
    assert_eq!(resumed_options["provider"], "fixture-route");
    for options in [
        json!({"provider":"different"}),
        json!({"model":"different"}),
        json!({"endpoint":"https://direct.example.invalid"}),
    ] {
        assert!(!gateway_options(options).status.success());
    }
    let fakebin = f.base().join("bin");
    fs::create_dir(&fakebin).unwrap();
    let fake = fakebin.join("pi");
    fs::write(
        &fake,
        "#!/usr/bin/env python3\nimport json,sys\nprint(json.dumps({'type':'session','id':'fixture-session','argv':sys.argv[1:]}))\nprint(json.dumps({'type':'message_end','message':{'role':'assistant','stopReason':'stop'}}))\nprint(json.dumps({'type':'agent_settled','aborted':False}))\n",
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let argv = result["argv"].as_array().unwrap();
    let mut process = support::command(argv[0].as_str().unwrap());
    for arg in &argv[1..] {
        process.arg(arg.as_str().unwrap());
    }
    let launch = process
        .env("PI_CODING_AGENT_DIR", &agent)
        .env(
            "PATH",
            format!("{}:{}", fakebin.display(), std::env::var("PATH").unwrap()),
        )
        .output()
        .unwrap();
    assert!(
        launch.status.success(),
        "{}",
        String::from_utf8_lossy(&launch.stderr)
    );
    let header: Value = serde_json::from_str(
        String::from_utf8_lossy(&launch.stdout)
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    let actual = &header["argv"];
    assert_eq!(actual[0], "--print");
    assert_eq!(actual[2], "fixture-route");
    assert_eq!(actual[4], "fixture-model");
    private(
        &agent.join("models.json"),
        json!({"providers":{"fixture-route":{"baseUrl":"http://direct.example.invalid/v1","models":[{"id":"fixture-model"}]}}}),
    );
    let mut process = support::command(argv[0].as_str().unwrap());
    for arg in &argv[1..] {
        process.arg(arg.as_str().unwrap());
    }
    let refused = process.env("PI_CODING_AGENT_DIR", &agent).output().unwrap();
    assert!(!refused.status.success());
    assert!(!String::from_utf8_lossy(&refused.stderr).contains("direct.example"));
}

#[test]
fn cancellation_reaches_the_execed_harness_and_its_descendants() {
    for harness in ["codex", "rovo", "grok", "pi", "muse", "omp", "agy"] {
        let mut f = Fixture::new();
        fs::write(
            f.store.home().join("config.json"),
            json!({"resource_home": f.base().join("ledger")}).to_string(),
        )
        .unwrap();
        let fakebin = f.base().join("bin");
        fs::create_dir(&fakebin).unwrap();
        let ready = f.repo.join("ready.json");
        let fake = fakebin.join(harness);
        // Expose the file-creation/write window deterministically. Readiness
        // means a complete PID record, not merely an existing file.
        fs::write(
            &fake,
            format!(
                r#"#!/usr/bin/env python3
import json,os,time
child=os.fork()
if child==0:
    time.sleep(30)
else:
    with open({:?},'w') as stream:
        time.sleep(0.1)
        stream.write(json.dumps([os.getpid(),child]))
    time.sleep(30)
"#,
                ready.display().to_string()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        let start = support::command(env!("CARGO_BIN_EXE_privateer"))
            .args([
                "--home",
                f.store.home().to_str().unwrap(),
                "agent.start",
                "--json",
                &json!({"id":"native-cancel","task_id":"task","manifest":example(harness),"options":if harness == "muse" {json!({"transport":"msp"})} else {json!({})}})
                    .to_string(),
            ])
            .env(
                "PATH",
                format!("{}:{}", fakebin.display(), std::env::var("PATH").unwrap()),
            )
            .output()
            .unwrap();
        assert!(
            start.status.success(),
            "{}",
            String::from_utf8_lossy(&start.stderr)
        );
        let until = Instant::now() + Duration::from_secs(3);
        let pids = loop {
            let pids = fs::read_to_string(&ready)
                .ok()
                .and_then(|s| serde_json::from_str::<Vec<i32>>(&s).ok())
                .filter(|pids| pids.len() == 2 && pids.iter().all(|pid| *pid > 1));
            if pids.is_some() || Instant::now() >= until {
                break pids;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        privateer::dispatch(&mut f.store, "job.cancel", json!({"id":"native-cancel"})).unwrap();
        let pids =
            pids.unwrap_or_else(|| panic!("fake {harness} did not publish a complete PID record"));
        let until = Instant::now() + Duration::from_secs(3);
        for pid in pids {
            loop {
                let out = Command::new("ps")
                    .args(["-p", &pid.to_string(), "-o", "stat="])
                    .output()
                    .unwrap();
                let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if state.is_empty() || state.starts_with('Z') {
                    break;
                }
                assert!(
                    Instant::now() < until,
                    "native payload {pid} survived cancellation"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[test]
fn gateway_refuses_wrong_model_endpoint_and_inactive_configuration() {
    let f = Fixture::new();
    let (agent, config) = gateway_fixture(&f);
    for (key, value) in [
        ("model", json!("missing-model")),
        ("endpoint", json!("https://other.example.invalid/v1")),
        (
            "endpoint",
            json!("https://user:not-a-real-token@example.invalid/v1"),
        ),
    ] {
        let mut data: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
        let old = data[key].clone();
        data[key] = value;
        private(&config, data.clone());
        let refused = gateway_call(&f, &agent, &config, "plugin.validate");
        assert!(!refused.status.success());
        assert!(!String::from_utf8_lossy(&refused.stderr).contains("not-a-real-token"));
        data[key] = old;
        private(&config, data);
    }
    let other = f.base().join("other-agent");
    fs::create_dir(&other).unwrap();
    private(&other.join("models.json"), json!({}));
    assert!(
        !gateway_call(&f, &other, &config, "plugin.validate")
            .status
            .success()
    );
    let database = fs::read(f.store.home().join("privateer.sqlite")).unwrap();
    assert!(!String::from_utf8_lossy(&database).contains("not-a-real-token"));
}

#[test]
fn failed_plugin_descendants_are_stopped() {
    let mut f = Fixture::new();
    let pidfile = f.base().join("descendant.pid");
    // The descendant releases the response pipes, so only the group signal
    // issued for the failed plugin can stop it.
    let body = format!(
        "import os,sys,time\nif os.fork()==0:\n    os.close(1); os.close(2)\n    open({pidfile:?},'w').write(str(os.getpid()))\n    time.sleep(30)\nelse:\n    while not os.path.exists({pidfile:?}): time.sleep(0.01)\n    time.sleep(0.05)\n    sys.exit(1)\n",
        pidfile = pidfile.display().to_string()
    );
    let path = manifest(&f, &body, 5000, 65536);
    let args = f.args(&path);
    let error = plugins::dispatch(&mut f.store, "plugin.validate", args).unwrap_err();
    assert!(
        error.to_string().contains("plugin executable failed"),
        "{error}"
    );
    let pid: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    let alive = |pid: i32| {
        let out = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "stat="])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        !stat.is_empty() && !stat.starts_with('Z')
    };
    while alive(pid) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(pid), "descendant {pid} survived its failed plugin");
}

#[test]
fn agent_harness_jobs_are_light_and_run_as_the_task_owner() {
    let mut f = Fixture::new();
    // Keep this job's reservation in a private ledger, not the host's.
    fs::write(
        f.store.home().join("config.json"),
        json!({"resource_home": f.base().join("ledger")}).to_string(),
    )
    .unwrap();
    let repo = f.repo.display().to_string();
    let path = manifest(
        &f,
        &response_script(&format!(
            "{{'argv':['true'],'cwd':{repo:?},'env_names':[]}}"
        )),
        2000,
        65536,
    );
    let mut args = f.args(&path);
    args["id"] = json!("harness");
    let job = privateer::dispatch(&mut f.store, "agent.start", args).unwrap();
    assert_eq!(job["heavy"], false);
    assert_eq!(job["agent_actor"], "department");
    assert_eq!(job["cleanup_descendants"], true);
}
