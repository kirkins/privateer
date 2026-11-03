use super::*;
use crate::store::{Caller, FrozenWallClock};
use std::os::unix::fs::symlink;

const FAKE: &str = r#"import json, os, sys
options = json.load(open(os.environ['PRIVATEER_VERIFIER_CONFIG']))
assert os.umask(0o077) == 0o077
if 'inherited_fd' in options:
    import errno
    try: os.fstat(options['inherited_fd'])
    except OSError as error: assert error.errno == errno.EBADF
    else: raise AssertionError('inherited descriptor leaked')
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
    ready, notify = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(ready)
        import signal
        boot = open('/proc/sys/kernel/random/boot_id').read().strip()
        start = open('/proc/self/stat').read().rsplit(')', 1)[1].split()[19]
        with open(options['identity'], 'w') as out:
            json.dump(dict(pid=os.getpid(), started='linux:' + boot + ':' + start,
                           pid_namespace=os.readlink('/proc/self/ns/pid')), out)
        os.write(notify, b'1')
        os.close(notify)
        while True: signal.pause()
    os.close(notify)
    assert os.read(ready, 1) == b'1'
    os.close(ready)
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
        let temp = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let home = root.join("home");
        directory(&home).unwrap();
        let mut store = Store::open(&home).unwrap();
        crate::dispatch(&mut store, "ship.put", json!({"name":"alpha"})).unwrap();
        crate::dispatch(
            &mut store,
            "charter.put",
            json!({"id":"alpha","objective":"beta","repo":root,
            "actions":["deploy_nonproduction"]}),
        )
        .unwrap();
        crate::dispatch(
            &mut store,
            "task.create",
            json!({"id":"alpha","owner":"alpha","charter_id":"alpha","title":"beta"}),
        )
        .unwrap();
        directory(&store.home().join("deployment")).unwrap();
        let installation = root.join("installation.json");
        private(&installation, json!({"id":"alpha"}).to_string());
        private(
            &root.join("pause.json"),
            json!({"installation_id":"alpha","paused":false}).to_string(),
        );
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
        // CI's Python can be owned by another account or group-writable.
        // A copied venv gives this fixture its own executable without chmod'ing
        // the host installation. Let venv handle platform-specific launchers.
        let host_python = crate::python::resolve(&std::env::var_os("PATH").unwrap()).unwrap();
        let venv = root.join("python");
        directory(&venv).unwrap();
        let output = std::process::Command::new(host_python)
            .args(["-I", "-B", "-m", "venv", "--without-pip", "--copies"])
            .arg(&venv)
            .output()
            .unwrap();
        assert!(output.status.success(), "venv setup failed: {output:?}");
        let bin = venv.join("bin");
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
        let python = bin.join("python3");
        assert!(fs::symlink_metadata(&python).unwrap().is_file());
        fs::set_permissions(&python, fs::Permissions::from_mode(0o700)).unwrap();
        let python_config = venv.join("pyvenv.cfg");
        fs::set_permissions(&python_config, fs::Permissions::from_mode(0o600)).unwrap();
        let configuration = json!({"schema_version":1,"installation_id":"alpha","home":store.home().canonicalize().unwrap(),
            "environment":"beta","production":false,"trusted_build_policy":"gamma",
            "verifier":{"id":"gamma","executable":python,"args":["-I","-B",script],"files":[script,python_config],"config":settings},
            "adapter":{"id":"beta","executable":python,"args":["-I","-B",adapter],"files":[adapter,python_config]}});
        private(
            &store.home().join("deployment/beta.json"),
            configuration.to_string(),
        );
        let request = json!({"task_id":"alpha","commit":"a".repeat(40),"artifact_path":artifact,
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
        .map(|proof| proof.value)
    }
    fn proof(&self) -> Verified {
        verify_with(
            &self.store,
            "alpha",
            self.request.clone(),
            |e| config::load_at(&self.store, e, &self.installation),
            Duration::from_secs(30),
        )
        .unwrap()
    }
    fn recheck(&self, store: &Store, proof: &Verified) -> Result<()> {
        recheck_with(
            store,
            proof,
            |e| config::load_at(store, e, &self.installation),
            |snapshot| config::unpaused_at(snapshot, &self.root.join("pause.json")),
        )
    }
    fn consume(&self, key: &str) -> Result<Value> {
        self.consume_proof(self.proof(), key)
    }
    fn consume_proof(&self, proof: Verified, key: &str) -> Result<Value> {
        super::super::ledger::consume_for_test(&self.store, proof, key, |p| {
            self.recheck(&self.store, p)
        })
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
fn evidence_timestamp_is_not_refreshed_by_later_custody_or_authority_checks() {
    let clock = std::cell::RefCell::new(Some(FrozenWallClock::at(10_000)));
    let calls = std::cell::Cell::new(0);
    let fixture = Fixture::new(json!({}));
    let verified = verify_with(
        &fixture.store,
        "alpha",
        fixture.request.clone(),
        |environment| {
            if calls.replace(calls.get() + 1) == 1 {
                drop(clock.borrow_mut().take());
                *clock.borrow_mut() = Some(FrozenWallClock::at(20_000));
            }
            config::load_at(&fixture.store, environment, &fixture.installation)
        },
        Duration::from_secs(30),
    )
    .unwrap();
    assert_eq!(Store::now_ms(), 20_000);
    assert_eq!(verified.value["verified_ms"], 10_000);
}

#[test]
fn fixture_owns_its_interpreter_and_unsafe_executable_permissions_still_refuse() {
    let fixture = Fixture::new(json!({}));
    let executable = fixture.snapshot().configuration.verifier.executable;
    assert_eq!(fs::metadata(&executable).unwrap().uid(), unsafe {
        libc::geteuid()
    });
    for path in [
        fixture.root.clone(),
        fixture.store.home().to_path_buf(),
        fixture.store.home().join("deployment"),
        fixture.root.join("python"),
        fixture.root.join("python/bin"),
        executable.clone(),
    ] {
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700,
            "{}",
            path.display()
        );
    }
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o770)).unwrap();
    assert!(config::load_at(&fixture.store, "beta", &fixture.installation).is_err());
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    fixture.verify().unwrap();
}

#[test]
fn writable_ancestors_refuse_even_when_the_final_tool_is_safe() {
    let fixture = Fixture::new(json!({}));
    let parent = fixture.root.join("python/bin");
    for mode in [0o777, 0o1777, 0o770] {
        fs::set_permissions(&parent, fs::Permissions::from_mode(mode)).unwrap();
        assert!(fixture.verify().is_err(), "{mode:o}");
    }
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    fixture.verify().unwrap();
}

#[test]
fn prepared_tool_uses_pinned_private_copies_after_original_path_replacement() {
    let fixture = Fixture::new(json!({}));
    let snapshot = fixture.snapshot();
    let work = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(&fixture.root)
        .unwrap();
    let tool = config::prepare(&snapshot, work.path()).unwrap();
    let original = fixture.root.join("verifier.py");
    fs::rename(&original, fixture.root.join("gamma.py")).unwrap();
    private(
        &original,
        "raise AssertionError('unverified replacement ran')",
    );
    let evidence = process::invoke(&tool, work.path(), &json!({"schema_version":1,
        "artifact_rule":crate::deploy_grants::known(&fixture.store,"alpha").unwrap()["artifact_rule"],
        "commit":fixture.request["commit"],"artifact_sha256":fixture.request["artifact_sha256"],
        "trusted_build_policy":"gamma"}), Duration::from_secs(30)).unwrap();
    assert_eq!(evidence["commit"], fixture.request["commit"]);
    config::check_prepared(&snapshot, &tool).unwrap();
    assert_ne!(fixture.snapshot().bindings(), snapshot.bindings());
}

#[test]
fn verifier_closes_inherited_descriptors_and_uses_a_fixed_umask() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let file = tempfile::tempfile().unwrap();
    let raw = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 128) };
    assert!(raw >= 128);
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let fixture = Fixture::new(json!({"inherited_fd":fd.as_raw_fd()}));
    fixture.verify().unwrap();
    assert!(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } >= 0);
}

#[test]
fn charter_task_and_cross_revision_budget_refuse_before_verifier_launch() {
    for sql in [
        "UPDATE charters SET data=json_set(data,'$.actions',json('[\"publish_pr\"]'))",
        "UPDATE charters SET data=json_set(data,'$.active',json('false'))",
        "UPDATE tasks SET owner='beta'",
        "UPDATE tasks SET status='done'",
        "UPDATE standing_deploy_grants SET spec=json_set(spec,'$.max_uses',1),revision=2;\
         INSERT INTO deployment_uses VALUES('alpha','alpha',1,'alpha','alpha','alpha','alpha','{}','{}',1)",
    ] {
        let fixture = Fixture::new(json!({}));
        fixture.store.connection().execute_batch(sql).unwrap();
        assert!(
            verify_with(
                &fixture.store,
                "alpha",
                fixture.request.clone(),
                |_| panic!("invalid authority reached verifier configuration"),
                Duration::ZERO
            )
            .is_err(),
            "{sql}"
        );
    }
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

fn repeat(proof: &Verified) -> Verified {
    Verified {
        value: proof.value.clone(),
    }
}

#[test]
fn dormant_consumption_atomically_records_exact_use_intent_and_audit_without_approvals() {
    let _clock = FrozenWallClock::at(10_000);
    let fixture = Fixture::new(json!({}));
    let result = fixture.consume("alpha").unwrap();
    assert_eq!(result["state"], "pending");
    assert_eq!(result["dispatch_enabled"], false);
    assert_eq!(result["authorization"]["deployment_authorized"], true);
    assert_eq!(result["authorization"]["verified_ms"], 10_000);
    let id = result["id"].as_str().unwrap();
    let audit: String = fixture.store.connection().query_row(
        "SELECT payload FROM audit WHERE kind='deploy_grant.used' AND json_extract(payload,'$.deployment_id')=?1", [id], |r| r.get(0)).unwrap();
    let audit: Value = serde_json::from_str(&audit).unwrap();
    assert_eq!(audit["artifact"], result["authorization"]["artifact"]);
    assert_eq!(audit["environment"], "beta");
    assert_eq!(audit["grant_revision"], 1);
    let reopened = Store::open(fixture.store.home()).unwrap();
    let stored: String = reopened
        .connection()
        .query_row(
            "SELECT evidence FROM deployment_uses WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&stored).unwrap(),
        result["authorization"]
    );
    assert!(
        reopened
            .connection()
            .execute("UPDATE deployment_uses SET created_ms=1 WHERE id=?1", [id])
            .is_err()
    );
    assert!(
        reopened
            .connection()
            .execute("DELETE FROM deployment_uses WHERE id=?1", [id])
            .is_err()
    );
    assert!(
        !reopened
            .connection()
            .query_row("SELECT EXISTS(SELECT 1 FROM approvals)", [], |r| r
                .get::<_, bool>(0))
            .unwrap()
    );
    assert!(crate::accepted_args("deployment.start").is_none());
}

#[test]
fn idempotency_returns_the_original_attempt_and_rejects_changed_artifact_request() {
    let mut fixture = Fixture::new(json!({}));
    let proof = fixture.proof();
    let first = fixture.consume_proof(repeat(&proof), "alpha").unwrap();
    assert_eq!(first, fixture.consume_proof(proof, "alpha").unwrap());
    fixture.request["commit"] = json!("b".repeat(40));
    assert!(
        fixture
            .consume("alpha")
            .unwrap_err()
            .to_string()
            .contains("changed inputs")
    );
    assert_ne!(fixture.consume("beta").unwrap()["id"], first["id"]);
}

#[test]
fn cross_revision_budget_cannot_be_refreshed_by_replacement_or_failed_attempts() {
    let fixture = Fixture::new(json!({}));
    let mut spec = crate::deploy_grants::known(&fixture.store, "alpha").unwrap();
    // Construct a typed replacement from the stored policy, not computed fields.
    for key in [
        "revision",
        "created_by",
        "created_ms",
        "updated_ms",
        "paused_ms",
        "revoked_ms",
        "state",
        "consumption_enabled",
    ] {
        spec.as_object_mut().unwrap().remove(key);
    }
    spec["expected_revision"] = json!(1);
    spec["max_uses"] = json!(1);
    crate::deploy_grants::put(&fixture.store, spec.clone(), |_| Ok(())).unwrap();
    let first = fixture.consume("alpha").unwrap();
    fixture
        .store
        .connection()
        .execute(
            "UPDATE deployment_intents SET state='unknown' WHERE use_id=?1",
            [first["id"].as_str().unwrap()],
        )
        .unwrap();
    spec["expected_revision"] = json!(2);
    spec["note"] = json!("beta");
    crate::deploy_grants::put(&fixture.store, spec.clone(), |_| Ok(())).unwrap();
    assert!(
        fixture
            .verify()
            .unwrap_err()
            .to_string()
            .contains("budget exhausted")
    );
    spec["expected_revision"] = json!(3);
    spec["max_uses"] = json!(2);
    crate::deploy_grants::put(&fixture.store, spec, |_| Ok(())).unwrap();
    let second = fixture.consume("beta").unwrap();
    assert_eq!(second["authorization"]["grant_revision"], 4);
    assert!(fixture.verify().is_err());
}

#[test]
fn authority_and_configuration_changes_between_verification_and_consumption_refuse() {
    for sql in [
        "UPDATE standing_deploy_grants SET paused_ms=1,revision=revision+1",
        "UPDATE standing_deploy_grants SET revoked_ms=1,revision=revision+1",
        "UPDATE standing_deploy_grants SET spec=json_set(spec,'$.expires_ms',1)",
        "UPDATE standing_deploy_grants SET revision=revision+1",
        "UPDATE standing_deploy_grants SET spec=json_set(spec,'$.production',json('true'))",
        "UPDATE tasks SET owner='beta'",
        "UPDATE tasks SET status='done'",
        "UPDATE charters SET data=json_set(data,'$.active',json('false'))",
        "UPDATE charters SET data=json_set(data,'$.actions',json('[\"publish_pr\"]'))",
        "UPDATE charters SET data=json_set(data,'$.revision',999)",
        "UPDATE ships SET record=json_set(record,'$.location',json('{\"peer\":\"gamma\"}'))",
    ] {
        let fixture = Fixture::new(json!({}));
        let proof = fixture.proof();
        fixture.store.connection().execute_batch(sql).unwrap();
        assert!(fixture.consume_proof(proof, "alpha").is_err(), "{sql}");
        assert!(
            !fixture
                .store
                .connection()
                .query_row("SELECT EXISTS(SELECT 1 FROM deployment_uses)", [], |r| r
                    .get::<_, bool>(
                    0
                ))
                .unwrap()
        );
    }
    let fixture = Fixture::new(json!({}));
    let proof = fixture.proof();
    fixture.change_config(|c| c["trusted_build_policy"] = json!("beta"));
    assert!(fixture.consume_proof(proof, "alpha").is_err());
    let fixture = Fixture::new(json!({}));
    let proof = fixture.proof();
    private(&fixture.installation, json!({"id":"gamma"}).to_string());
    assert!(fixture.consume_proof(proof, "alpha").is_err());
}

#[test]
fn sealed_proof_does_not_transfer_authority_to_another_ship_or_bypass_replay_visibility() {
    let mut fixture = Fixture::new(json!({}));
    let proof = fixture.proof();
    fixture.consume_proof(repeat(&proof), "alpha").unwrap();
    fixture.store.set_caller(Caller::Agent {
        actor: "beta".into(),
    });
    assert!(fixture.consume_proof(repeat(&proof), "alpha").is_err());
    assert!(fixture.consume_proof(proof, "beta").is_err());
}

#[test]
fn emergency_pause_is_authoritative_private_installation_bound_and_fail_closed() {
    for bytes in [
        "missing",
        "not-json",
        "{}",
        "{\"installation_id\":\"alpha\",\"paused\":true}",
        "{\"installation_id\":\"beta\",\"paused\":false}",
    ] {
        let fixture = Fixture::new(json!({}));
        let proof = fixture.proof();
        let path = fixture.root.join("pause.json");
        if bytes == "missing" {
            fs::remove_file(path).unwrap();
        } else {
            private(&path, bytes);
        }
        assert!(fixture.consume_proof(proof, "alpha").is_err(), "{bytes}");
    }
    let fixture = Fixture::new(json!({}));
    let proof = fixture.proof();
    fs::set_permissions(
        fixture.root.join("pause.json"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(fixture.consume_proof(proof, "alpha").is_err());
}

#[test]
fn evidence_age_is_bounded_with_an_injected_clock_not_elapsed_test_time() {
    for now in [9_999, 310_000, 310_001] {
        let fixture = Fixture::new(json!({}));
        let proof = {
            let _clock = FrozenWallClock::at(10_000);
            fixture.proof()
        };
        let _clock = FrozenWallClock::at(now);
        assert_eq!(
            fixture.consume_proof(proof, "alpha").is_ok(),
            now == 310_000
        );
    }
}

#[test]
fn consumption_rolls_back_use_intent_and_audit_when_any_durable_write_fails() {
    for trigger in [
        "CREATE TRIGGER fail_use BEFORE INSERT ON deployment_uses BEGIN SELECT RAISE(ABORT,'fixture'); END",
        "CREATE TRIGGER fail_intent BEFORE INSERT ON deployment_intents BEGIN SELECT RAISE(ABORT,'fixture'); END",
        "CREATE TRIGGER fail_audit BEFORE INSERT ON audit WHEN NEW.kind='deploy_grant.used' BEGIN SELECT RAISE(ABORT,'fixture'); END",
    ] {
        let fixture = Fixture::new(json!({}));
        let proof = fixture.proof();
        fixture.store.connection().execute_batch(trigger).unwrap();
        assert!(fixture.consume_proof(proof, "alpha").is_err());
        for table in ["deployment_uses", "deployment_intents"] {
            assert!(
                !fixture
                    .store
                    .connection()
                    .query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table})"), [], |r| {
                        r.get::<_, bool>(0)
                    })
                    .unwrap()
            );
        }
        assert!(
            !fixture
                .store
                .connection()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM audit WHERE kind='deploy_grant.used')",
                    [],
                    |r| r.get::<_, bool>(0)
                )
                .unwrap()
        );
    }
}

#[test]
fn pending_intent_backpressure_refuses_without_losing_a_use() {
    let fixture = Fixture::new(json!({}));
    let proof = fixture.proof();
    for index in 0..20 {
        fixture
            .consume_proof(repeat(&proof), &format!("alpha-{index}"))
            .unwrap();
    }
    assert!(
        fixture
            .consume_proof(proof, "beta")
            .unwrap_err()
            .to_string()
            .contains("too many pending")
    );
    assert_eq!(
        fixture
            .store
            .connection()
            .query_row("SELECT count(*) FROM deployment_uses", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        20
    );
}

#[test]
fn concurrent_consumers_serialize_idempotency_and_the_cross_revision_budget() {
    use std::sync::{Arc, Barrier};
    for (same_key, max) in [(true, 1), (false, 1), (false, 2)] {
        let _clock = FrozenWallClock::at(10_000);
        let fixture = Fixture::new(json!({}));
        fixture
            .store
            .connection()
            .execute(
                "UPDATE standing_deploy_grants SET spec=json_set(spec,'$.max_uses',?1)",
                [max],
            )
            .unwrap();
        let proof = fixture.proof();
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|index| {
                let proof = repeat(&proof);
                let home = fixture.store.home().to_path_buf();
                let installation = fixture.installation.clone();
                let pause = fixture.root.join("pause.json");
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let _clock = FrozenWallClock::at(10_000);
                    let store = Store::open(&home).unwrap();
                    barrier.wait();
                    super::super::ledger::consume_for_test(
                        &store,
                        proof,
                        if same_key || index == 0 {
                            "alpha"
                        } else {
                            "beta"
                        },
                        |p| {
                            recheck_with(
                                &store,
                                p,
                                |e| config::load_at(&store, e, &installation),
                                |s| config::unpaused_at(s, &pause),
                            )
                        },
                    )
                })
            })
            .collect();
        let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        if same_key {
            assert_eq!(
                outcomes[0].as_ref().unwrap()["id"],
                outcomes[1].as_ref().unwrap()["id"]
            );
        } else {
            assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), max as usize);
        }
        assert_eq!(
            fixture
                .store
                .connection()
                .query_row("SELECT count(*) FROM deployment_uses", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            if same_key { 1 } else { max }
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
    // SIGKILL delivery to the descendant need not precede reaping its already
    // exited wrapper. Wait on the kernel's exit event, not an arbitrary sleep.
    if crate::jobs::identity_alive(&identity).unwrap() {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let process: Value = serde_json::from_str(&identity).unwrap();
        let fd =
            unsafe { libc::syscall(libc::SYS_pidfd_open, process["pid"].as_u64().unwrap(), 0) };
        if fd >= 0 {
            let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
            if crate::jobs::identity_alive(&identity).unwrap() {
                let mut event = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                assert_eq!(unsafe { libc::poll(&mut event, 1, 5000) }, 1);
            }
        }
    }
    assert!(!crate::jobs::identity_alive(&identity).unwrap());
}
