use super::*;
use crate::store::FrozenWallClock;

fn request(id: &str) -> Value {
    json!({"id":id,"expected_revision":0,"grantee":"alpha","environment":"beta",
        "production":false,"notify":false,
        "artifact_rule":{"kind":"green_branch_commit","repository_id":"gamma",
            "branch_ref":"refs/heads/main","required_checks":["test"],"app_id":"gamma",
            "workflow_path":".forgejo/workflows/test.yml","event":"push","provenance_mode":"ci_attestation"},
        "verifier":{"id":"gamma","sha256":"a".repeat(64)},
        "adapter":{"id":"beta","sha256":"b".repeat(64)}})
}

fn create(store: &Store, id: &str) -> Value {
    put(store, request(id), |_| Ok(())).unwrap()
}

fn call(store: &Store, verb: &str, rev: i64) -> Result<Value> {
    dispatch(store, verb, json!({"id":"alpha","expected_revision":rev}))
}

#[test]
fn lifecycle_revisions_and_irreversible_revocation_survive_reopen() {
    let _clock = FrozenWallClock::at(10_000);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let created = create(&store, "alpha");
    assert_eq!(created["revision"], 1);
    assert_eq!(created["created_by"], "operator");
    assert_eq!(created["consumption_enabled"], false);
    let mut replacement = request("alpha");
    replacement["expected_revision"] = json!(1);
    replacement["note"] = json!("gamma");
    assert_eq!(
        put(&store, replacement.clone(), |_| Ok(())).unwrap()["revision"],
        2
    );
    assert!(put(&store, replacement.clone(), |_| Ok(())).is_err());
    assert_eq!(
        call(&store, "deploy_grant.pause", 2).unwrap()["revision"],
        3
    );
    replacement["expected_revision"] = json!(3);
    assert!(put(&store, replacement.clone(), |_| Ok(())).is_err());
    assert!(call(&store, "deploy_grant.resume", 2).is_err());
    assert_eq!(
        call(&store, "deploy_grant.resume", 3).unwrap()["state"],
        "active"
    );
    assert_eq!(
        call(&store, "deploy_grant.revoke", 4).unwrap()["revision"],
        5
    );
    for command in [
        "deploy_grant.resume",
        "deploy_grant.pause",
        "deploy_grant.revoke",
    ] {
        assert!(call(&store, command, 5).is_err());
    }
    replacement["expected_revision"] = json!(5);
    assert!(put(&store, replacement, |_| Ok(())).is_err());
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    let grant = dispatch(&store, "deploy_grant.show", json!({"id":"alpha"})).unwrap();
    assert_eq!(grant["state"], "revoked");
    assert_eq!(grant["revision"], 5);
    let audits: Vec<String> = store
        .connection()
        .prepare("SELECT payload FROM audit WHERE kind='deploy_grant.revoke'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(
        audits
            .iter()
            .any(|a| serde_json::from_str::<Value>(a).unwrap()["grant"] == grant)
    );
}

#[test]
fn production_fields_are_required_but_never_enable_consumption() {
    let _clock = FrozenWallClock::at(10_000);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut args = request("alpha");
    args["production"] = json!(true);
    args["notify"] = json!(true);
    args["expires_ms"] = json!(100_000);
    args["max_uses"] = json!(3);
    args["wait_window_ms"] = json!(30_000);
    for field in ["notify", "expires_ms", "max_uses", "wait_window_ms"] {
        let mut missing = args.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(put(&store, missing, |_| Ok(())).is_err(), "{field}");
    }
    for field in ["max_uses", "wait_window_ms"] {
        let mut bad = args.clone();
        bad[field] = json!(0);
        assert!(put(&store, bad, |_| Ok(())).is_err());
    }
    assert_eq!(
        put(&store, args, |_| Ok(())).unwrap()["consumption_enabled"],
        false
    );
    let mut store = store;
    assert!(crate::dispatch(&mut store, "deployment.start", json!({"grant":"alpha"})).is_err());
    let approvals: bool = store
        .connection()
        .query_row("SELECT EXISTS(SELECT 1 FROM approvals)", [], |r| r.get(0))
        .unwrap();
    assert!(!approvals);
}

#[test]
fn malformed_rules_types_unknown_fields_and_bounds_refuse() {
    let _clock = FrozenWallClock::at(10_000);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    for (path, value) in [
        ("/artifact_rule/extra", json!("gamma")),
        ("/artifact_rule/event", json!("pull_request")),
        ("/artifact_rule/branch_ref", json!("main")),
        ("/artifact_rule/branch_ref", json!("refs/heads/../main")),
        ("/artifact_rule/required_checks", json!([])),
        ("/artifact_rule/required_checks", json!(["test", "test"])),
        ("/artifact_rule/workflow_path", json!("../gamma")),
        ("/artifact_rule/provenance_mode", json!("caller")),
        ("/verifier/sha256", json!("path-not-bytes")),
        ("/production", json!("false")),
        ("/expires_ms", json!(10_000)),
        ("/expires_ms", json!("later")),
        ("/note", json!("x".repeat(2049))),
        ("/expected_revision", json!(-1)),
    ] {
        let mut args = request("alpha");
        // JSON pointer_mut cannot add absent keys.
        let (parent, key) = path.rsplit_once('/').unwrap();
        args.pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(key.to_owned(), value);
        assert!(
            put(&store, args, |_| panic!("invalid input reached consent")).is_err(),
            "{path}"
        );
    }
    let mut args = request("alpha");
    args["relayed_by"] = json!("gamma");
    assert!(put(&store, args, |_| Ok(())).is_err());
    assert!(dispatch(&store, "deploy_grant.list", json!({"limit":101})).is_err());
    assert!(
        dispatch(
            &store,
            "deploy_grant.show",
            json!({"id":"alpha","extra":true})
        )
        .is_err()
    );
}

#[test]
fn agents_and_relay_shaped_calls_cannot_write_and_reads_are_owner_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    create(&store, "alpha");
    let mut other = request("beta");
    other["grantee"] = json!("beta");
    put(&store, other, |_| Ok(())).unwrap();
    // RPC/door/relay dispatch cannot create even with the operator marker.
    assert!(
        dispatch(&store, "deploy_grant.put", request("gamma"))
            .unwrap_err()
            .to_string()
            .contains("terminal")
    );
    store.set_caller(Caller::Agent {
        actor: "alpha".into(),
    });
    assert!(put(&store, request("gamma"), |_| panic!("agent consent")).is_err());
    for command in [
        "deploy_grant.put",
        "deploy_grant.pause",
        "deploy_grant.resume",
        "deploy_grant.revoke",
    ] {
        assert!(
            crate::dispatch(
                &mut store,
                command,
                json!({"id":"alpha","expected_revision":1})
            )
            .is_err()
        );
    }
    let listed = dispatch(&store, "deploy_grant.list", json!({})).unwrap();
    assert_eq!(listed["grants"].as_array().unwrap().len(), 1);
    assert_eq!(listed["grants"][0]["id"], "alpha");
    assert!(dispatch(&store, "deploy_grant.list", json!({"grantee":"beta"})).is_err());
    assert!(dispatch(&store, "deploy_grant.show", json!({"id":"beta"})).is_err());
    assert!(
        dispatch(
            &store,
            "deploy_grant.pause",
            json!({"id":"alpha","expected_revision":1,"relayed_by":"gamma"})
        )
        .is_err()
    );
}

#[test]
fn bounded_keyset_pagination_and_expired_effective_state() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    {
        let _clock = FrozenWallClock::at(10_000);
        let mut args = request("alpha");
        args["expires_ms"] = json!(20_000);
        put(&store, args, |_| Ok(())).unwrap();
        create(&store, "beta");
    }
    let _clock = FrozenWallClock::at(20_000);
    let first = dispatch(&store, "deploy_grant.list", json!({"limit":1})).unwrap();
    assert_eq!(first["grants"][0]["state"], "expired");
    assert_eq!(first["next_after_id"], "alpha");
    let next = dispatch(
        &store,
        "deploy_grant.list",
        json!({"limit":1,"after_id":"alpha"}),
    )
    .unwrap();
    assert_eq!(next["grants"][0]["id"], "beta");
    assert!(next["next_after_id"].is_null());
}

#[test]
fn consent_failure_or_concurrent_revision_change_cannot_write() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    assert!(put(&store, request("alpha"), |_| bail!("not confirmed")).is_err());
    assert!(find(&store, "alpha").unwrap().is_none());
    create(&store, "alpha");
    let other = Store::open(dir.path()).unwrap();
    let mut args = request("alpha");
    args["expected_revision"] = json!(1);
    assert!(
        put(&store, args, |_| {
            call(&other, "deploy_grant.pause", 1)?;
            Ok(())
        })
        .is_err()
    );
    assert_eq!(known(&store, "alpha").unwrap()["state"], "paused");
    assert_eq!(known(&store, "alpha").unwrap()["revision"], 2);
}

#[test]
fn audit_failure_rolls_back_creation_and_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    create(&store, "alpha");
    store
        .connection()
        .execute_batch(
            "CREATE TRIGGER refuse_deploy_audit BEFORE INSERT ON audit
        WHEN NEW.kind LIKE 'deploy_grant.%' BEGIN SELECT RAISE(ABORT,'audit unavailable'); END;",
        )
        .unwrap();
    assert!(put(&store, request("beta"), |_| Ok(())).is_err());
    assert!(find(&store, "beta").unwrap().is_none());
    assert!(call(&store, "deploy_grant.pause", 1).is_err());
    let grant = known(&store, "alpha").unwrap();
    assert_eq!(grant["revision"], 1);
    assert_eq!(grant["state"], "active");
}

#[test]
fn accepted_arguments_cover_exact_lifecycle_protocol() {
    for (command, keys) in ACCEPTED {
        assert_eq!(crate::accepted_args(command), Some(*keys));
    }
    assert!(crate::accepted_args("deployment.start").is_none());
}
