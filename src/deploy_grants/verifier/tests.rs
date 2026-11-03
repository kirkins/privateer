use super::*;
use crate::store::{Caller, FrozenWallClock};
use std::os::unix::fs::symlink;

const FAKE: &str = r#"import json, os, sys
options = json.load(open(os.environ['PRIVATEER_VERIFIER_CONFIG']))
assert not any(k in os.environ for k in ['PRIVATEER_FLEET', 'PRIVATEER_RESOURCE_HOME', 'PYTHONPATH', 'LD_PRELOAD'])
r = json.loads(sys.stdin.read())
assert r['schema_version'] == 1
rule = r['artifact_rule']
checks = [dict(id='alpha', run_id='beta', name=name, commit=r['commit'], app_id=rule['app_id'],
    workflow_path=rule['workflow_path'], event=rule['event'], kind='check_run', status='completed', conclusion='success')
    for name in rule['required_checks']]
e = dict(schema_version=1, repository_id=rule['repository_id'], branch_ref=rule['branch_ref'],
    commit=r['commit'], branch_tip=r['commit'], artifact_sha256=r['artifact_sha256'], checks=checks,
    attestation=dict(id='gamma', mode='ci_attestation', repository_id=rule['repository_id'], commit=r['commit'],
        artifact_sha256=r['artifact_sha256'], app_id=rule['app_id'], workflow_path=rule['workflow_path'],
        trusted_build_policy=r['trusted_build_policy']))
for path, value in options.get('patches', []):
    target = e
    parts = path.split('/')
    for key in parts[:-1]: target = target[int(key)] if isinstance(target, list) else target[key]
    target[int(parts[-1]) if isinstance(target, list) else parts[-1]] = value
mode = options.get('mode', 'valid')
if mode == 'bad-json': print('not-json'); sys.exit(0)
if mode == 'extra-line': print('{}')
if mode == 'output-limit': print('x' * 65537); sys.exit(0)
if mode == 'stderr-limit': sys.stderr.write('x' * 65537); sys.exit(0)
if mode == 'failure': sys.stderr.write('fixture-private'); sys.exit(7)
if mode == 'change-config': open(os.environ['PRIVATEER_VERIFIER_CONFIG'], 'a').write(' ')
if mode == 'change-code': open(__file__, 'a').write('\n')
if mode == 'change-grant':
    import sqlite3
    db = sqlite3.connect(options['database'])
    db.execute("UPDATE standing_deploy_grants SET paused_ms=1,revision=revision+1 WHERE id='alpha'")
    db.commit()
if mode == 'change-host':
    import sqlite3
    db = sqlite3.connect(options['database'])
    db.execute("UPDATE ships SET record=json_set(record,'$.location',json('{\"peer\":\"gamma\"}')) WHERE name='alpha'")
    db.commit()
if mode == 'change-artifact': open(options['artifact'], 'wb').write(b'beta artifact')
if mode == 'pipe-holder':
    pid = os.fork()
    if pid == 0:
        import signal
        boot = open('/proc/sys/kernel/random/boot_id').read().strip()
        start = open('/proc/self/stat').read().rsplit(')', 1)[1].split()[19]
        with open(options['identity'], 'w') as out: out.write('linux:' + boot + ':' + start)
        while True: signal.pause()
print(json.dumps(e), flush=True)
"#;

fn private(path: &Path, bytes: impl AsRef<[u8]>) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    store: Store,
    installation: PathBuf,
    request: Value,
}
impl Fixture {
    fn new(mut options: Value) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut store = Store::open(&root.join("home")).unwrap();
        crate::dispatch(&mut store, "ship.put", json!({"name":"alpha"})).unwrap();
        directory(&store.home().join("deployment")).unwrap();
        let installation = root.join("installation.json");
        private(&installation, json!({"id":"alpha"}).to_string());
        let script = root.join("verifier.py");
        private(&script, FAKE);
        let adapter = root.join("adapter.py");
        private(&adapter, "raise AssertionError('adapter must never run')\n");
        let settings = root.join("settings.json");
        let artifact = root.join("artifact");
        private(&artifact, b"alpha artifact");
        options["database"] = json!(store.home().join("privateer.sqlite"));
        options["artifact"] = json!(artifact);
        options["identity"] = json!(root.join("identity"));
        private(&settings, options.to_string());
        let python = crate::python::resolve(&std::env::var_os("PATH").unwrap())
            .unwrap()
            .canonicalize()
            .unwrap();
        let configuration = json!({"schema_version":1,"installation_id":"alpha","home":store.home().canonicalize().unwrap(),
            "environment":"beta","production":false,"trusted_build_policy":"gamma",
            "verifier":{"id":"gamma","executable":python,"args":["-I","-B",script],"files":[script],"config":settings},
            "adapter":{"id":"beta","executable":python,"args":["-I","-B",adapter],"files":[adapter]}});
        private(
            &store.home().join("deployment/beta.json"),
            configuration.to_string(),
        );
        let request = json!({"commit":"a".repeat(40),"artifact_path":artifact,
            "artifact_sha256":format!("{:x}", Sha256::digest(b"alpha artifact"))});
        let fixture = Self {
            _temp: temp,
            root,
            store,
            installation,
            request,
        };
        fixture.bind();
        fixture
    }
    fn snapshot(&self) -> config::Snapshot {
        config::load_at(&self.store, "beta", &self.installation).unwrap()
    }
    fn bind(&self) {
        let bindings = self.snapshot().bindings();
        let grant = json!({"id":"alpha","expected_revision":0,"grantee":"alpha","environment":"beta",
            "production":false,"notify":false,"artifact_rule":{"kind":"green_branch_commit","repository_id":"gamma",
                "branch_ref":"refs/heads/main","required_checks":["test"],"app_id":"gamma",
                "workflow_path":".forgejo/workflows/test.yml","event":"push","provenance_mode":"ci_attestation"},
            "verifier":bindings["verifier"],"adapter":bindings["adapter"]});
        crate::deploy_grants::put(&self.store, grant, |_| Ok(())).unwrap();
    }
    fn verify(&self) -> Result<Value> {
        verify_with(
            &self.store,
            "alpha",
            self.request.clone(),
            |environment| config::load_at(&self.store, environment, &self.installation),
            Duration::from_secs(30),
        )
    }
    fn change_config(&self, change: impl FnOnce(&mut Value)) {
        let path = self.store.home().join("deployment/beta.json");
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        change(&mut value);
        private(&path, value.to_string());
    }
}

#[test]
fn real_subprocess_retains_verified_copy_without_authorizing_any_deployment() {
    let _clock = FrozenWallClock::at(10_000);
    let fixture = Fixture::new(json!({}));
    let verified = fixture.verify().unwrap();
    assert_eq!(verified["verified_ms"], 10_000);
    assert_eq!(verified["deployment_authorized"], false);
    assert_eq!(verified["grant_revision"], 1);
    assert_eq!(verified["bindings"], fixture.snapshot().bindings());
    let copy = Path::new(verified["artifact"]["path"].as_str().unwrap());
    assert_ne!(
        copy,
        Path::new(fixture.request["artifact_path"].as_str().unwrap())
    );
    assert_eq!(fs::read(copy).unwrap(), b"alpha artifact");
    assert_eq!(
        fs::metadata(copy).unwrap().permissions().mode() & 0o777,
        0o400
    );
    fs::write(
        fixture.request["artifact_path"].as_str().unwrap(),
        b"beta artifact",
    )
    .unwrap();
    assert_eq!(fs::read(copy).unwrap(), b"alpha artifact");
    let approvals: bool = fixture
        .store
        .connection()
        .query_row("SELECT EXISTS(SELECT 1 FROM approvals)", [], |r| r.get(0))
        .unwrap();
    assert!(!approvals);
    assert!(crate::accepted_args("deployment.start").is_none());
}

#[test]
fn missing_private_config_installation_and_move_fail_closed() {
    let fixture = Fixture::new(json!({}));
    private(&fixture.installation, json!({"id":"gamma"}).to_string());
    assert!(fixture.verify().is_err());
    private(&fixture.installation, json!({"id":"alpha"}).to_string());
    fixture.change_config(|c| c["home"] = json!(fixture.root));
    assert!(fixture.verify().is_err());
    fixture.change_config(|c| c["home"] = json!(fixture.store.home()));
    fs::set_permissions(&fixture.installation, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(fixture.verify().is_err());
    fs::remove_file(&fixture.installation).unwrap();
    assert!(fixture.verify().is_err());
}

#[test]
fn tool_bytes_supporting_inputs_and_credentials_config_are_in_the_binding() {
    for target in ["verifier.py", "adapter.py", "settings.json"] {
        let fixture = Fixture::new(json!({}));
        fs::OpenOptions::new()
            .append(true)
            .open(fixture.root.join(target))
            .unwrap()
            .write_all(b"\n")
            .unwrap();
        assert!(fixture.verify().is_err(), "{target}");
    }
    let fixture = Fixture::new(json!({}));
    let original = fixture.snapshot().bindings();
    fixture.change_config(|c| c["trusted_build_policy"] = json!("beta"));
    assert_ne!(original, fixture.snapshot().bindings());
    assert!(fixture.verify().is_err());
    fixture.change_config(|c| c["verifier"]["files"] = json!([]));
    assert!(fixture.verify().is_err());
}

#[test]
fn malformed_configuration_unknown_fields_and_unsafe_paths_refuse() {
    for change in [
        json!({"schema_version":2}),
        json!({"unexpected":true}),
        json!({"environment":"gamma"}),
        json!({"verifier":{"id":"gamma","executable":"relative","args":[],"files":[]}}),
    ] {
        let fixture = Fixture::new(json!({}));
        fixture.change_config(|c| {
            for (key, value) in change.as_object().unwrap() {
                c[key] = value.clone();
            }
        });
        assert!(fixture.verify().is_err());
    }
    let fixture = Fixture::new(json!({}));
    let script = fixture.root.join("verifier.py");
    fs::rename(&script, fixture.root.join("gamma.py")).unwrap();
    symlink(fixture.root.join("gamma.py"), &script).unwrap();
    assert!(fixture.verify().is_err());
}

#[test]
fn verifier_schema_exact_checks_and_attestation_bindings_refuse_substitution() {
    for (path, value) in [
        ("schema_version", json!(2)),
        ("repository_id", json!("beta")),
        ("branch_ref", json!("refs/heads/gamma")),
        ("commit", json!("b".repeat(40))),
        ("branch_tip", json!("b".repeat(40))),
        ("artifact_sha256", json!("b".repeat(64))),
        ("checks", json!([])),
        ("checks/0/kind", json!("commit_status")),
        ("checks/0/commit", json!("b".repeat(40))),
        ("checks/0/app_id", json!("alpha")),
        ("checks/0/workflow_path", json!("gamma.yml")),
        ("checks/0/event", json!("pull_request")),
        ("checks/0/status", json!("pending")),
        ("checks/0/conclusion", json!("skipped")),
        ("checks/0/name", json!("gamma")),
        ("checks/0/id", json!("")),
        ("attestation/id", json!("")),
        ("attestation/mode", json!("runtime_build")),
        ("attestation/repository_id", json!("alpha")),
        ("attestation/commit", json!("b".repeat(40))),
        ("attestation/artifact_sha256", json!("b".repeat(64))),
        ("attestation/app_id", json!("beta")),
        ("attestation/workflow_path", json!("gamma.yml")),
        ("attestation/trusted_build_policy", json!("alpha")),
        ("unexpected", json!(true)),
    ] {
        let fixture = Fixture::new(json!({"patches":[[path,value]]}));
        assert!(fixture.verify().is_err(), "{path}");
    }
    let fixture = Fixture::new(json!({}));
    let valid = fixture.verify().unwrap();
    let mut duplicate = valid["evidence"].clone();
    let check = duplicate["checks"][0].clone();
    duplicate["checks"].as_array_mut().unwrap().push(check);
    let mut rule: super::super::ArtifactRule = serde_json::from_value(
        crate::deploy_grants::known(&fixture.store, "alpha").unwrap()["artifact_rule"].clone(),
    )
    .unwrap();
    rule.required_checks.push("gamma".into());
    let evidence: Evidence = serde_json::from_value(duplicate).unwrap();
    let request: Request = serde_json::from_value(fixture.request.clone()).unwrap();
    assert!(matched(&evidence, &rule, &request, "gamma").is_err());
}

#[test]
fn invalid_subprocess_output_failure_and_deadline_are_bounded_and_sanitized() {
    for mode in [
        "bad-json",
        "extra-line",
        "output-limit",
        "stderr-limit",
        "failure",
    ] {
        let fixture = Fixture::new(json!({"mode":mode}));
        let error = fixture.verify().unwrap_err();
        assert!(!format!("{error:#}").contains("fixture-private"));
    }
    let fixture = Fixture::new(json!({}));
    let result = process::invoke(
        &fixture.snapshot().configuration.verifier,
        &fixture.root,
        &json!({}),
        Duration::ZERO,
    );
    assert!(result.unwrap_err().to_string().contains("timed out"));
}

#[test]
fn post_verification_configuration_lifecycle_host_and_artifact_races_refuse() {
    for mode in [
        "change-code",
        "change-config",
        "change-grant",
        "change-host",
        "change-artifact",
    ] {
        let fixture = Fixture::new(json!({"mode":mode}));
        assert!(fixture.verify().is_err(), "{mode}");
    }
}

#[test]
fn content_addressed_storage_is_not_trusted_by_filename_and_sources_cannot_be_links() {
    let fixture = Fixture::new(json!({}));
    let first = fixture.verify().unwrap();
    assert_eq!(fixture.verify().unwrap()["artifact"], first["artifact"]);
    let path = Path::new(first["artifact"]["path"].as_str().unwrap());
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(path, b"beta artifact").unwrap();
    assert!(fixture.verify().is_err());
    let source = fixture.root.join("artifact");
    fs::rename(&source, fixture.root.join("gamma")).unwrap();
    symlink(fixture.root.join("gamma"), &source).unwrap();
    assert!(fixture.verify().is_err());
}

#[test]
fn production_runtime_build_wrong_owner_and_invalid_requests_are_not_supported() {
    let mut fixture = Fixture::new(json!({}));
    fixture.store.set_caller(Caller::Agent {
        actor: "beta".into(),
    });
    assert!(fixture.verify().is_err());
    assert!(inspect(&fixture.store, "beta").is_err());
    fixture.store.set_caller(Caller::Operator);
    fixture
        .store
        .connection()
        .execute(
            "UPDATE standing_deploy_grants SET spec=json_set(spec,'$.production',json('true'))",
            [],
        )
        .unwrap();
    assert!(fixture.verify().is_err());
    fixture.store.connection().execute("UPDATE standing_deploy_grants SET spec=json_set(spec,'$.production',json('false'),'$.artifact_rule.provenance_mode','runtime_build')", []).unwrap();
    assert!(fixture.verify().is_err());
    for change in [
        json!({"commit":"abc"}),
        json!({"artifact_sha256":"B".repeat(64)}),
        json!({"checks_passed":true}),
        json!({"executable":"gamma"}),
    ] {
        let mut request = fixture.request.clone();
        for (key, value) in change.as_object().unwrap() {
            request[key] = value.clone();
        }
        assert!(
            verify_with(
                &fixture.store,
                "alpha",
                request,
                |_| panic!("invalid request reached configuration"),
                Duration::ZERO
            )
            .is_err()
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn wrapper_exit_with_pipe_holding_child_is_not_success_and_group_is_cleaned() {
    let fixture = Fixture::new(json!({"mode":"pipe-holder"}));
    let result = verify_with(
        &fixture.store,
        "alpha",
        fixture.request.clone(),
        |environment| config::load_at(&fixture.store, environment, &fixture.installation),
        Duration::from_secs(5),
    );
    assert!(result.unwrap_err().to_string().contains("timed out"));
    let identity = fs::read_to_string(fixture.root.join("identity")).unwrap();
    assert!(!crate::jobs::identity_alive(&identity).unwrap());
}
