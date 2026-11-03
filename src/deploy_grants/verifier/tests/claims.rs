use super::*;
use crate::deploy_grants::{claims, ledger};
use rusqlite::params;

#[test]
fn replay_after_revocation_is_status_only_not_an_authorization() {
    let fixture = Fixture::new(json!({}));
    let proof = fixture.proof();
    let first = fixture.consume_proof(repeat(&proof), "alpha").unwrap();
    fixture
        .store
        .connection()
        .execute_batch("UPDATE standing_deploy_grants SET revoked_ms=1,revision=revision+1")
        .unwrap();
    let replay = ledger::consume_for_test(&fixture.store, proof, "alpha", |_| {
        panic!("a historical status lookup is not an authorization check")
    })
    .unwrap();
    assert_eq!(replay, first);
    assert!(replay.get("authorization").is_none());
    assert!(replay.get("deployment_authorized").is_none());
    let denied = fixture.claim(first["id"].as_str().unwrap()).unwrap();
    assert_eq!(denied.status["state"], "denied");
    assert!(denied.claim.is_none());
}

#[test]
fn adapter_preparation_uses_its_own_digest_and_verified_private_inputs() {
    let fixture = Fixture::new(json!({}));
    let original = fixture.root.join("adapter.py");
    private(
        &original,
        "import json,sys\nprint(json.dumps(json.load(sys.stdin)))\n",
    );
    let snapshot = fixture.snapshot();
    let work = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(&fixture.root)
        .unwrap();
    let tool = config::prepare(
        &snapshot.configuration,
        &snapshot.configuration.adapter,
        &snapshot.adapter_sha256,
        work.path(),
    )
    .unwrap();
    fs::rename(&original, fixture.root.join("gamma.py")).unwrap();
    private(
        &original,
        "raise AssertionError('unverified adapter replacement ran')\n",
    );
    let output = process::invoke(
        &tool,
        work.path(),
        &json!({"id":"alpha"}),
        Duration::from_secs(30),
    )
    .unwrap();
    assert_eq!(output, json!({"id":"alpha"}));
    config::check_prepared(
        &snapshot.configuration,
        &snapshot.configuration.adapter,
        &snapshot.adapter_sha256,
        &tool,
    )
    .unwrap();
    assert!(
        config::check_prepared(
            &snapshot.configuration,
            &snapshot.configuration.adapter,
            &snapshot.verifier_sha256,
            &tool
        )
        .is_err()
    );
    fs::set_permissions(&tool.files[0], fs::Permissions::from_mode(0o600)).unwrap();
    private(&tool.files[0], b"changed");
    assert!(
        config::check_prepared(
            &snapshot.configuration,
            &snapshot.configuration.adapter,
            &snapshot.adapter_sha256,
            &tool
        )
        .is_err()
    );
}

#[test]
fn invalid_source_is_rejected_before_creating_copy_directories() {
    let fixture = Fixture::new(json!({}));
    let snapshot = fixture.snapshot();
    let mut source = snapshot.configuration.adapter.clone();
    source.executable = fixture.root.join("absent/../adapter.py");
    let work = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(&fixture.root)
        .unwrap();
    assert!(
        config::prepare(
            &snapshot.configuration,
            &source,
            &snapshot.adapter_sha256,
            work.path()
        )
        .is_err()
    );
    assert!(!work.path().join("pinned").exists());
    assert!(!work.path().join("absent").exists());
}

#[test]
fn claim_reverifies_retained_bytes_at_the_last_consumed_use_without_a_second_debit() {
    let fixture = Fixture::new(json!({}));
    fixture
        .store
        .connection()
        .execute_batch("UPDATE standing_deploy_grants SET spec=json_set(spec,'$.max_uses',1)")
        .unwrap();
    let first = {
        let _clock = FrozenWallClock::at(10_000);
        fixture.consume("alpha").unwrap()
    };
    assert!(
        fixture.verify().is_err(),
        "no additional unused use remains"
    );
    fs::remove_file(fixture.request["artifact_path"].as_str().unwrap()).unwrap();
    let _clock = FrozenWallClock::at(400_000);
    let id = first["id"].as_str().unwrap();
    let claimed = fixture.claim(id).unwrap();
    assert_eq!(claimed.status["state"], "claimed");
    assert!(claimed.status.get("authorization").is_none());
    let token = claimed.claim.unwrap();
    assert_eq!(token.id(), id);
    let identity: Value = serde_json::from_str(token.identity()).unwrap();
    assert!(crate::jobs::identity_alive(&identity["dispatcher"].to_string()).unwrap());
    let payload: String = fixture.store.connection().query_row(
        "SELECT payload FROM audit WHERE kind='deployment.claimed' AND json_extract(payload,'$.deployment_id')=?1",
        [id], |r| r.get(0)).unwrap();
    let payload: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(payload["detail"]["verified_ms"], 400_000);
    assert_eq!(payload["detail"]["evidence"]["checks"][0]["id"], "alpha");
    assert_eq!(fixture.authorization(&first)["verified_ms"], 10_000);
    let replay = claims::claim_with(
        &fixture.store,
        id,
        |_| panic!("must not reverify a claim replay"),
        |_, _| panic!("must not reauthorize a claim replay"),
    )
    .unwrap();
    assert!(replay.claim.is_none());
    assert_eq!(replay.status["state"], "claimed");
    let used: i64 = fixture
        .store
        .connection()
        .query_row(
            "SELECT count(*) FROM deployment_uses WHERE grant_id='alpha'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(used, 1);
    assert!(crate::accepted_args("deployment.start").is_none());
    assert!(crate::accepted_args("deployment.show").is_none());
    assert!(crate::accepted_args("deployment.list").is_none());
}

#[test]
fn claim_checks_current_authority_both_before_verification_and_under_writer_lock() {
    for after_verification in [false, true] {
        for sql in [
            "UPDATE standing_deploy_grants SET paused_ms=1,revision=revision+1",
            "UPDATE standing_deploy_grants SET revoked_ms=1,revision=revision+1",
            "UPDATE standing_deploy_grants SET spec=json_set(spec,'$.expires_ms',1)",
            "UPDATE standing_deploy_grants SET revision=revision+1",
            "UPDATE standing_deploy_grants SET spec=json_set(spec,'$.production',json('true'))",
            "UPDATE standing_deploy_grants SET spec=json_set(spec,'$.max_uses',0)",
            "UPDATE tasks SET owner='beta'",
            "UPDATE tasks SET status='done'",
            "UPDATE charters SET data=json_set(data,'$.active',json('false'))",
            "UPDATE charters SET data=json_set(data,'$.actions',json('[\"publish_pr\"]'))",
            "UPDATE charters SET data=json_set(data,'$.revision',999)",
            "UPDATE ships SET record=json_set(record,'$.location',json('{\"peer\":\"gamma\"}'))",
        ] {
            let fixture = Fixture::new(json!({}));
            let first = fixture.consume("alpha").unwrap();
            if !after_verification {
                fixture.store.connection().execute_batch(sql).unwrap();
            }
            let outcome = fixture
                .claim_after(first["id"].as_str().unwrap(), || {
                    if after_verification {
                        fixture.store.connection().execute_batch(sql).unwrap();
                    }
                })
                .unwrap();
            assert_eq!(
                outcome.status["state"], "denied",
                "{after_verification}: {sql}"
            );
            assert!(outcome.claim.is_none());
        }
    }
}

#[test]
fn changed_remote_evidence_artifact_and_config_prevent_claim() {
    for patches in [
        json!([["branch_tip", "b".repeat(40)]]),
        json!([["checks/0/conclusion", "failure"]]),
        json!([["attestation/artifact_sha256", "b".repeat(64)]]),
    ] {
        let fixture = Fixture::new(json!({}));
        let first = fixture.consume("alpha").unwrap();
        private(&fixture.root.join("remote.json"), patches.to_string());
        let outcome = fixture.claim(first["id"].as_str().unwrap()).unwrap();
        assert_eq!(outcome.status["state"], "denied");
        assert!(outcome.claim.is_none());
    }
    for change in ["config", "installation", "artifact", "adapter"] {
        let fixture = Fixture::new(json!({}));
        let first = fixture.consume("alpha").unwrap();
        let outcome = fixture
            .claim_after(first["id"].as_str().unwrap(), || match change {
                "config" => fixture.change_config(|c| c["trusted_build_policy"] = json!("beta")),
                "installation" => private(&fixture.installation, json!({"id":"beta"}).to_string()),
                "artifact" => {
                    let authorization = fixture.authorization(&first);
                    let path = Path::new(authorization["artifact"]["path"].as_str().unwrap());
                    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
                    private(path, b"changed");
                }
                "adapter" => private(&fixture.root.join("adapter.py"), b"changed adapter"),
                _ => unreachable!(),
            })
            .unwrap();
        // The artifact is checked again at claim, not only during initial
        // custody. A same-account rewrite between verification and lock refuses.
        assert_eq!(outcome.status["state"], "denied", "{change}");
    }
}

#[test]
fn claim_rereads_emergency_pause_fail_closed_and_rejects_old_or_future_evidence() {
    for bytes in [
        "missing",
        "not-json",
        "{}",
        "{\"installation_id\":\"alpha\",\"paused\":true}",
        "{\"installation_id\":\"beta\",\"paused\":false}",
        "unsafe",
    ] {
        let fixture = Fixture::new(json!({}));
        let first = fixture.consume("alpha").unwrap();
        let outcome = fixture
            .claim_after(first["id"].as_str().unwrap(), || {
                let path = fixture.root.join("pause.json");
                match bytes {
                    "missing" => fs::remove_file(path).unwrap(),
                    "unsafe" => {
                        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap()
                    }
                    _ => private(&path, bytes),
                }
            })
            .unwrap();
        assert_eq!(outcome.status["state"], "denied", "{bytes}");
    }
    for now in [9_999, 310_000, 310_001] {
        let fixture = Fixture::new(json!({}));
        let (first, proof) = {
            let _clock = FrozenWallClock::at(10_000);
            let first = fixture.consume("alpha").unwrap();
            let used = ledger::recorded(&fixture.store, first["id"].as_str().unwrap()).unwrap();
            let proof = verify_claim_with(
                &fixture.store,
                &used,
                |e| config::load_at(&fixture.store, e, &fixture.installation),
                Duration::from_secs(30),
            )
            .unwrap();
            (first, proof)
        };
        let _clock = FrozenWallClock::at(now);
        let outcome = claims::claim_with(
            &fixture.store,
            first["id"].as_str().unwrap(),
            |_| Ok(proof),
            |proof, used| {
                recheck_attempt(
                    &fixture.store,
                    proof,
                    Some(used),
                    |e| config::load_at(&fixture.store, e, &fixture.installation),
                    |s| config::unpaused_at(s, &fixture.root.join("pause.json")),
                )
            },
        )
        .unwrap();
        assert_eq!(outcome.claim.is_some(), now == 310_000);
    }
}

#[test]
fn claim_denial_and_recovery_audit_failures_roll_back_state() {
    for kind in [
        "deployment.claimed",
        "deployment.denied",
        "deployment.unknown",
    ] {
        let fixture = Fixture::new(json!({}));
        let first = fixture.consume("alpha").unwrap();
        let id = first["id"].as_str().unwrap();
        if kind == "deployment.unknown" {
            fixture.claim(id).unwrap();
        }
        fixture.store.connection().execute_batch(&format!(
            "CREATE TRIGGER fail_claim_audit BEFORE INSERT ON audit WHEN NEW.kind='{kind}' BEGIN SELECT RAISE(ABORT,'fixture'); END")).unwrap();
        if kind == "deployment.denied" {
            fs::remove_file(fixture.root.join("pause.json")).unwrap();
        }
        if kind == "deployment.unknown" {
            assert!(claims::recover_with(&fixture.store, |_| Ok(false)).is_err());
        } else {
            assert!(fixture.claim(id).is_err());
        }
        let state: String = fixture
            .store
            .connection()
            .query_row(
                "SELECT state FROM deployment_intents WHERE use_id=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            state,
            if kind == "deployment.unknown" {
                "claimed"
            } else {
                "pending"
            }
        );
        assert!(
            fixture.authorization(&first)["deployment_authorized"]
                .as_bool()
                .unwrap()
        );
    }
}

#[test]
fn recovery_preserves_ambiguous_custody_and_never_replays_claimed_or_started_work() {
    for state in ["claimed", "started"] {
        let fixture = Fixture::new(json!({}));
        let first = fixture.consume("alpha").unwrap();
        let id = first["id"].as_str().unwrap();
        let token = fixture.claim(id).unwrap().claim.unwrap();
        fixture
            .store
            .connection()
            .execute(
                "UPDATE deployment_intents SET state=?2 WHERE use_id=?1",
                params![id, state],
            )
            .unwrap();
        let reopened = Store::open(fixture.store.home()).unwrap();
        assert!(
            claims::recover(&reopened).unwrap().is_empty(),
            "current claim owner is live"
        );
        let unknown = claims::recover_with(&reopened, |_| Ok(false)).unwrap();
        assert_eq!(unknown[0]["state"], "unknown");
        let identity: String = reopened
            .connection()
            .query_row(
                "SELECT claim_identity FROM deployment_intents WHERE use_id=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            identity,
            token.identity(),
            "never erase payload-custody evidence"
        );
        let replay = claims::claim_with(
            &reopened,
            id,
            |_| panic!("unknown attempts cannot replay"),
            |_, _| unreachable!(),
        )
        .unwrap();
        assert!(replay.claim.is_none());
        assert_eq!(replay.status["state"], "unknown");
        assert!(
            claims::recover_with(&reopened, |_| Ok(false))
                .unwrap()
                .is_empty()
        );
    }
    let fixture = Fixture::new(json!({}));
    let pending = fixture.consume("alpha").unwrap();
    assert!(claims::recover(&fixture.store).unwrap().is_empty());
    let id = pending["id"].as_str().unwrap();
    fixture.claim(id).unwrap();
    let unknown =
        claims::recover_with(&fixture.store, |_| anyhow::bail!("unreadable identity")).unwrap();
    assert_eq!(unknown[0]["state"], "unknown");
}

#[test]
fn agent_cannot_claim_or_reconcile() {
    let mut fixture = Fixture::new(json!({}));
    let first = fixture.consume("alpha").unwrap();
    fixture.store.set_caller(Caller::Agent {
        actor: "alpha".into(),
    });
    assert!(fixture.claim(first["id"].as_str().unwrap()).is_err());
    assert!(claims::recover(&fixture.store).is_err());
}

#[test]
fn concurrent_claims_commit_only_one_fresh_token_and_no_second_use() {
    use std::sync::{Arc, Barrier};
    let fixture = Fixture::new(json!({}));
    let first = fixture.consume("alpha").unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let home = fixture.store.home().to_path_buf();
            let installation = fixture.installation.clone();
            let pause = fixture.root.join("pause.json");
            let id = first["id"].as_str().unwrap().to_owned();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = Store::open(&home).unwrap();
                claims::claim_with(
                    &store,
                    &id,
                    |used| {
                        let proof = verify_claim_with(
                            &store,
                            used,
                            |e| config::load_at(&store, e, &installation),
                            Duration::from_secs(30),
                        );
                        barrier.wait();
                        proof
                    },
                    |proof, used| {
                        recheck_attempt(
                            &store,
                            proof,
                            Some(used),
                            |e| config::load_at(&store, e, &installation),
                            |s| config::unpaused_at(s, &pause),
                        )
                    },
                )
                .unwrap()
                .claim
                .is_some()
            })
        })
        .collect();
    let tokens = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|fresh| *fresh)
        .count();
    assert_eq!(tokens, 1);
}
