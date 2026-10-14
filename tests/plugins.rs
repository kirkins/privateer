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
        let store = Store::open(&base.join("home")).unwrap();
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
    fs::write(&path, json!({"schema_version":1,"id":"fixture","executable":["python3",script],"timeout_ms":timeout,"max_output_bytes":max}).to_string()).unwrap();
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
fn native_harness_plans_and_brief_are_generic_and_lean() {
    let mut f = Fixture::new();
    let args = f.args(&example("codex"));
    let command = plugins::dispatch(&mut f.store, "plugin.command", args).unwrap();
    assert_eq!(command["argv"][0], "codex");
    assert_eq!(command["argv"][1], "exec");
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
    for invalid in [
        json!({"sandbox":"unrestricted"}),
        json!({"ephemeral":"true"}),
    ] {
        let mut args = f.args(&example("codex"));
        args["options"] = invalid;
        assert!(plugins::dispatch(&mut f.store, "plugin.command", args).is_err());
    }
    let mut args = f.args(&example("pi"));
    args["options"] = json!({"provider":"fixture","model":"fixture-model"});
    let command = plugins::dispatch(&mut f.store, "plugin.command", args).unwrap();
    assert_eq!(command["argv"][0], "pi");
    assert_eq!(command["argv"][3], "fixture");
    let brief = plugins::dispatch(&mut f.store, "agent.brief", json!({"task_id":"task"})).unwrap();
    assert!(brief["brief"].as_str().unwrap().len() < 2048);
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
    assert!(brief["brief"].as_str().unwrap().contains("peer.call"));
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
    let output = Command::new(env!("CARGO_BIN_EXE_privateer"))
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
    Command::new(env!("CARGO_BIN_EXE_privateer"))
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
    let fakebin = f.base().join("bin");
    fs::create_dir(&fakebin).unwrap();
    let fake = fakebin.join("pi");
    fs::write(
        &fake,
        "#!/usr/bin/env python3\nimport json,sys\nprint(json.dumps(sys.argv[1:]))\n",
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let argv = result["argv"].as_array().unwrap();
    let mut process = Command::new(argv[0].as_str().unwrap());
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
    let actual: Value = serde_json::from_slice(&launch.stdout).unwrap();
    assert_eq!(actual[0], "--print");
    assert_eq!(actual[2], "fixture-route");
    assert_eq!(actual[4], "fixture-model");
    private(
        &agent.join("models.json"),
        json!({"providers":{"fixture-route":{"baseUrl":"http://direct.example.invalid/v1","models":[{"id":"fixture-model"}]}}}),
    );
    let mut process = Command::new(argv[0].as_str().unwrap());
    for arg in &argv[1..] {
        process.arg(arg.as_str().unwrap());
    }
    let refused = process.env("PI_CODING_AGENT_DIR", &agent).output().unwrap();
    assert!(!refused.status.success());
    assert!(!String::from_utf8_lossy(&refused.stderr).contains("direct.example"));
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
