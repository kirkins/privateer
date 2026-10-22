use privateer::{dispatch, rpc, store::Store};
use serde_json::{Value, json};
use std::fs;

fn fixture() -> (tempfile::TempDir, Store) {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    let mut store = Store::open(&temp.path().join("home")).unwrap();
    dispatch(
        &mut store,
        "init",
        json!({"resource_home":temp.path().join("ledger")}),
    )
    .unwrap();
    dispatch(
        &mut store,
        "charter.put",
        json!({"id":"project","objective":"Deliver the project","repo":repo,"min_free_bytes":0}),
    )
    .unwrap();
    dispatch(
        &mut store,
        "task.create",
        json!({"id":"main","charter_id":"project","owner":"navigator","title":"Deliver"}),
    )
    .unwrap();
    (temp, store)
}

#[test]
fn standing_assignment_survives_restart_corrections_and_peer_delegation() {
    let (_temp, mut s) = fixture();
    dispatch(
        &mut s,
        "task.create",
        json!({"id":"ios","parent":"main","owner":"ios","title":"Repair UI"}),
    )
    .unwrap();
    dispatch(
        &mut s,
        "task.update",
        json!({"id":"ios","actor":"ios","status":"blocked"}),
    )
    .unwrap();
    let home = s.home().to_owned();
    drop(s);
    let mut s = Store::open(&home).unwrap();
    dispatch(
        &mut s,
        "task.update",
        json!({"id":"ios","actor":"ios","status":"running"}),
    )
    .unwrap();
    dispatch(
        &mut s,
        "task.create",
        json!({"id":"infra","parent":"ios","owner":"infra","title":"Check gateway"}),
    )
    .unwrap();
    assert!(s.authorize("infra", "implement").is_ok());
    assert!(s.authorize("infra", "merge_green").is_ok());
    assert!(s.authorize("infra", "deploy_production").is_err());
}

#[test]
fn children_cannot_switch_charters_and_revocation_reaches_existing_descendants() {
    let (_temp, mut s) = fixture();
    let other = s.charter("project").unwrap();
    dispatch(
        &mut s,
        "charter.put",
        json!({"id":"other","objective":"Other","repo":other["repo"]}),
    )
    .unwrap();
    assert!(
        dispatch(
            &mut s,
            "task.create",
            json!({"id":"child","parent":"main","charter_id":"other","owner":"infra","title":"Bad"})
        )
        .is_err()
    );
    dispatch(
        &mut s,
        "task.create",
        json!({"id":"child","parent":"main","owner":"infra","title":"Good"}),
    )
    .unwrap();
    dispatch(&mut s, "charter.revoke", json!({"id":"project"})).unwrap();
    // An ordinary correction does not reactivate a revoked assignment.
    dispatch(
        &mut s,
        "charter.put",
        json!({"id":"project","objective":"Corrected wording"}),
    )
    .unwrap();
    assert!(s.authorize("main", "implement").is_err());
    assert!(s.authorize("child", "implement").is_err());
}

#[test]
fn delegation_depth_and_cancelled_parent_prevent_unbounded_fanout() {
    let (_temp, mut s) = fixture();
    dispatch(
        &mut s,
        "charter.put",
        json!({"id":"project","max_delegation_depth":1}),
    )
    .unwrap();
    dispatch(
        &mut s,
        "task.create",
        json!({"id":"child","parent":"main","owner":"ios","title":"Repair"}),
    )
    .unwrap();
    assert!(
        dispatch(
            &mut s,
            "task.create",
            json!({"id":"grandchild","parent":"child","owner":"infra","title":"Investigate"})
        )
        .is_err()
    );
    dispatch(
        &mut s,
        "task.update",
        json!({"id":"main","actor":"navigator","status":"cancelled"}),
    )
    .unwrap();
    assert!(s.authorize("child", "test").is_err());
}

#[test]
fn handoff_requires_acceptance_and_preserves_exactly_one_owner() {
    let (_temp, mut s) = fixture();
    dispatch(
        &mut s,
        "task.handoff",
        json!({"id":"main","actor":"navigator","target":"runtime"}),
    )
    .unwrap();
    assert_eq!(s.task("main").unwrap()["owner"], "navigator");
    assert!(dispatch(&mut s, "task.accept", json!({"id":"main","actor":"ios"})).is_err());
    dispatch(
        &mut s,
        "task.accept",
        json!({"id":"main","actor":"runtime"}),
    )
    .unwrap();
    assert!(
        dispatch(
            &mut s,
            "task.update",
            json!({"id":"main","actor":"navigator","status":"done"})
        )
        .is_err()
    );
    dispatch(
        &mut s,
        "task.update",
        json!({"id":"main","actor":"runtime","status":"done"}),
    )
    .unwrap();
}

#[test]
fn thousand_alerts_keep_one_pending_notification_and_all_facts() {
    let (_temp, mut s) = fixture();
    for n in 0..1000 {
        dispatch(&mut s,"event.emit",json!({"id":format!("e{n}"),"task_id":"main","issue_key":"gateway","kind":"wake","payload":{"attempt":n}})).unwrap();
    }
    let status = dispatch(&mut s, "status", json!({})).unwrap();
    assert_eq!(status["event_facts"], 1000);
    assert_eq!(status["pending_notifications"], 1);
    let events = dispatch(&mut s, "event.pending", json!({})).unwrap();
    assert_eq!(events["events"][0]["payload"]["attempt"], 999);
    let all = dispatch(&mut s, "event.history", json!({"limit":1000})).unwrap();
    assert_eq!(all["events"].as_array().unwrap().len(), 1000);
}

#[test]
fn ack_of_observed_alert_cannot_erase_a_newer_arrival_or_replay_old_fact() {
    let (_temp, mut s) = fixture();
    let e = json!({"id":"old","task_id":"main","issue_key":"build","kind":"wake"});
    let old = dispatch(&mut s, "event.emit", e.clone()).unwrap();
    dispatch(
        &mut s,
        "event.emit",
        json!({"id":"new","task_id":"main","issue_key":"build","kind":"wake"}),
    )
    .unwrap();
    let ack = dispatch(
        &mut s,
        "event.ack",
        json!({"task_id":"main","issue_key":"build","through_seq":old["seq"]}),
    )
    .unwrap();
    assert_eq!(ack["acknowledged"], false);
    let pending = dispatch(&mut s, "event.pending", json!({})).unwrap()["events"][0].clone();
    dispatch(
        &mut s,
        "event.ack",
        json!({"task_id":"main","issue_key":"build","through_seq":pending["seq"]}),
    )
    .unwrap();
    dispatch(&mut s, "event.emit", e).unwrap();
    assert_eq!(
        dispatch(&mut s, "status", json!({})).unwrap()["pending_notifications"],
        0
    );
    assert!(
        dispatch(
            &mut s,
            "event.ack",
            json!({"task_id":"main","issue_key":"build","through_seq":9000})
        )
        .is_err()
    );
}

#[test]
fn direct_peer_messages_do_not_depend_on_navigator_claims_or_readiness() {
    let (_temp, mut s) = fixture();
    dispatch(
        &mut s,
        "task.update",
        json!({"id":"main","actor":"navigator","status":"blocked"}),
    )
    .unwrap();
    dispatch(
        &mut s,
        "task.create",
        json!({"id":"ios","parent":"main","owner":"ios","title":"App"}),
    )
    .unwrap();
    dispatch(&mut s,"message.send",json!({"id":"request","task_id":"ios","from":"ios","to":"infra","body":"Investigate the gateway"})).unwrap();
    let inbox = dispatch(&mut s, "message.inbox", json!({"actor":"infra"})).unwrap();
    assert_eq!(inbox["messages"].as_array().unwrap().len(), 1);
    assert_eq!(
        dispatch(&mut s, "message.inbox", json!({"actor":"navigator"})).unwrap()["messages"],
        json!([])
    );
}

#[test]
fn delivery_checks_head_and_production_approval_is_exact_and_once_only() {
    let (_temp, mut s) = fixture();
    let actions = vec![
        "inspect",
        "spawn",
        "implement",
        "test",
        "merge_green",
        "deploy_production",
    ];
    dispatch(
        &mut s,
        "charter.put",
        json!({"id":"project","actions":actions}),
    )
    .unwrap();
    let check = json!({"task_id":"main","current_head":"abc123","tested_head":"abc123","checks_passed":true});
    dispatch(&mut s, "delivery.check", check.clone()).unwrap();
    let mut bad = check.clone();
    bad["tested_head"] = json!("old");
    assert!(dispatch(&mut s, "delivery.check", bad).is_err());
    let mut prod = check.clone();
    prod["deploys_production"] = json!(true);
    prod["environment"] = json!("production");
    assert!(dispatch(&mut s, "delivery.check", prod.clone()).is_err());
    let grant = json!({"id":"permission","task_id":"main","artifact":"abc123","environment":"production","approver":"release-manager"});
    dispatch(&mut s, "approval.grant", grant.clone()).unwrap();
    let allowed = dispatch(&mut s, "delivery.check", prod.clone()).unwrap();
    assert_eq!(allowed["approval_id"], "permission");
    // The allowed delivery used the approval; it cannot be consumed again.
    assert!(dispatch(&mut s, "approval.consume", grant.clone()).is_err());
    assert!(dispatch(&mut s, "approval.grant", grant).is_err());
    assert!(dispatch(&mut s, "delivery.check", prod).is_err());
}

#[test]
fn changed_charter_invalidates_old_approval_without_resetting_omitted_limits() {
    let (_temp, mut s) = fixture();
    dispatch(&mut s,"charter.put",json!({"id":"project","actions":["inspect","merge_green","deploy_production"],"max_heavy_jobs":2})).unwrap();
    dispatch(
        &mut s,
        "approval.grant",
        json!({"id":"permission","task_id":"main","artifact":"abc123","environment":"production","approver":"release-manager"}),
    )
    .unwrap();
    let changed = dispatch(
        &mut s,
        "charter.put",
        json!({"id":"project","objective":"Changed outcome"}),
    )
    .unwrap();
    assert_eq!(changed["max_heavy_jobs"], 2);
    assert!(
        !changed["actions"]
            .as_array()
            .unwrap()
            .contains(&json!("implement"))
    );
    assert!(dispatch(&mut s,"approval.consume",json!({"id":"permission","task_id":"main","artifact":"abc123","environment":"production"})).is_err());
}

#[test]
fn production_checks_and_approvals_recheck_authority_after_waiting_for_a_writer() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    static WAITING: AtomicBool = AtomicBool::new(false);
    for change in ["revocation", "cancellation"] {
        for command in ["delivery.check", "approval.consume", "approval.grant"] {
            let (_temp, mut store) = fixture();
            let grant = json!({"id":"permission","task_id":"main","artifact":"abc123",
                "environment":"production","approver":"release-manager"});
            dispatch(&mut store, "approval.grant", grant.clone()).unwrap();
            // Open the command's connection before the changing transaction.
            let mut other = Store::open(store.home()).unwrap();
            WAITING.store(false, Ordering::SeqCst);
            other
                .connection()
                .busy_handler(Some(|attempt| {
                    WAITING.store(true, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(1));
                    attempt < 5_000
                }))
                .unwrap();
            let tx = store.write_transaction().unwrap();
            if change == "revocation" {
                let mut charter = store.charter("project").unwrap();
                charter["active"] = json!(false);
                tx.execute(
                    "UPDATE charters SET data=?1 WHERE id='project'",
                    [charter.to_string()],
                )
                .unwrap();
            } else {
                tx.execute("UPDATE tasks SET status='cancelled' WHERE id='main'", [])
                    .unwrap();
            }
            let args = match command {
                "delivery.check" => json!({"task_id":"main","current_head":"abc123",
                    "tested_head":"abc123","checks_passed":true,
                    "deploys_production":true,"environment":"production"}),
                "approval.consume" => json!({"id":"permission","task_id":"main",
                    "artifact":"abc123","environment":"production"}),
                _ => json!({"id":"late-approval","task_id":"main","artifact":"abc123",
                    "environment":"production","approver":"release-manager"}),
            };
            let pending = std::thread::spawn(move || dispatch(&mut other, command, args));
            let deadline = Instant::now() + Duration::from_secs(5);
            while !WAITING.load(Ordering::SeqCst)
                && !pending.is_finished()
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            tx.commit().unwrap();
            let error = pending.join().unwrap().unwrap_err();
            let reason = if change == "revocation" {
                "revoked"
            } else {
                "terminal"
            };
            assert!(
                WAITING.load(Ordering::SeqCst),
                "{command} did not serialize with {change}"
            );
            assert!(
                error.to_string().contains(reason),
                "{command} after {change}: {error}"
            );
            let (approvals, consumed): (i64, i64) = store
                .connection()
                .query_row("SELECT COUNT(*), SUM(consumed) FROM approvals", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .unwrap();
            assert_eq!((approvals, consumed), (1, 0), "{command} after {change}");
        }
    }
}

#[test]
fn rpc_correlates_success_and_failure_without_panicking_on_bad_requests() {
    let (_temp, mut s) = fixture();
    assert_eq!(
        rpc(&mut s, json!({"id":"one","command":"status"}))["id"],
        "one"
    );
    let bad = rpc(&mut s, json!({"id":"two","command":"not.real"}));
    assert_eq!(bad["id"], "two");
    assert_eq!(bad["ok"], false);
    assert_eq!(rpc(&mut s, Value::Null)["ok"], false);
}

#[test]
fn state_symlinks_are_refused_and_private_files_have_restricted_permissions() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (temp, s) = fixture();
    assert_eq!(
        fs::metadata(s.home()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(s.home().join("privateer.sqlite"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let alias = temp.path().join("alias");
    symlink(s.home(), &alias).unwrap();
    assert!(Store::open(&alias).is_err());
}

#[test]
fn partial_update_waits_for_revocation_instead_of_overwriting_stale_authority() {
    let (_temp, s) = fixture();
    let other = Store::open(s.home()).unwrap();
    let tx = rusqlite::Transaction::new_unchecked(
        s.connection(),
        rusqlite::TransactionBehavior::Immediate,
    )
    .unwrap();
    let mut charter = s.charter("project").unwrap();
    charter["active"] = json!(false);
    charter["revision"] = json!(2);
    tx.execute(
        "UPDATE charters SET data=?1 WHERE id='project'",
        [charter.to_string()],
    )
    .unwrap();
    let update = std::thread::spawn(move || {
        let mut other = other;
        dispatch(
            &mut other,
            "charter.put",
            json!({"id":"project","objective":"Corrected wording"}),
        )
        .unwrap()
    });
    std::thread::sleep(std::time::Duration::from_millis(100));
    tx.commit().unwrap();
    assert_eq!(update.join().unwrap()["active"], false);
    assert!(s.authorize("main", "implement").is_err());
}

#[test]
fn corrupt_charter_is_not_silently_replaced_with_default_authority() {
    let (_temp, mut s) = fixture();
    s.connection()
        .execute("UPDATE charters SET data='broken' WHERE id='project'", [])
        .unwrap();
    let repo = s.home().parent().unwrap().join("repo");
    assert!(
        dispatch(
            &mut s,
            "charter.put",
            json!({"id":"project","objective":"Correction","repo":repo})
        )
        .is_err()
    );
}

#[test]
fn explicit_production_approval_does_not_require_widening_the_standing_charter() {
    let (_temp, mut s) = fixture();
    assert!(s.authorize("main", "deploy_production").is_err());
    let before = s.charter("project").unwrap();
    let check = json!({"task_id":"main","current_head":"release-a","tested_head":"release-a","checks_passed":true,"deploys_production":true,"environment":"production"});
    assert!(dispatch(&mut s, "delivery.check", check.clone()).is_err());
    dispatch(&mut s,"approval.grant",json!({"id":"one-deployment","task_id":"main","artifact":"release-a","environment":"production","approver":"release-manager"})).unwrap();
    dispatch(&mut s, "delivery.check", check.clone()).unwrap();
    assert_eq!(s.charter("project").unwrap(), before);
    assert!(s.authorize("main", "deploy_production").is_err());
    assert!(dispatch(&mut s,"approval.consume",json!({"id":"one-deployment","task_id":"main","artifact":"release-b","environment":"production"})).is_err());
    // The single deployment it authorised has used it.
    assert!(dispatch(&mut s, "delivery.check", check).is_err());
    assert!(dispatch(&mut s,"approval.consume",json!({"id":"one-deployment","task_id":"main","artifact":"release-a","environment":"production"})).is_err());
}

#[test]
fn concurrent_writers_wait_instead_of_failing() {
    let (_temp, s) = fixture();
    let home = s.home().to_path_buf();
    drop(s);
    let threads = 16;
    let per_thread = 5;
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let home = home.clone();
            std::thread::spawn(move || {
                // Each writer uses its own connection, as separate processes would.
                let mut store = Store::open(&home).unwrap();
                (0..per_thread)
                    .map(|n| {
                        dispatch(
                            &mut store,
                            "event.emit",
                            json!({"id":format!("t{t}-e{n}"),"task_id":"main","issue_key":"gateway","kind":"wake","payload":{"thread":t,"n":n}}),
                        )
                        .map_err(|e| e.to_string())
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let results: Vec<_> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    assert_eq!(results.len(), threads * per_thread);
    let failures: Vec<_> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    assert!(failures.is_empty(), "concurrent emits failed: {failures:?}");

    let mut s = Store::open(&home).unwrap();
    let status = dispatch(&mut s, "status", json!({})).unwrap();
    assert_eq!(status["event_facts"], (threads * per_thread) as u64);
    assert_eq!(status["pending_notifications"], 1);
    let all = dispatch(&mut s, "event.history", json!({"limit":1000})).unwrap();
    assert_eq!(
        all["events"].as_array().unwrap().len(),
        threads * per_thread
    );
}

#[test]
fn custody_and_approval_do_not_depend_on_an_inspect_action() {
    let (_temp, mut s) = fixture();
    // A narrow charter without "inspect".
    dispatch(
        &mut s,
        "charter.put",
        json!({"id":"project","actions":["implement","test","message"]}),
    )
    .unwrap();
    dispatch(
        &mut s,
        "task.handoff",
        json!({"id":"main","actor":"navigator","target":"apps"}),
    )
    .unwrap();
    let accepted = dispatch(&mut s, "task.accept", json!({"id":"main","actor":"apps"})).unwrap();
    assert_eq!(accepted["owner"], "apps");
    let approval = json!({"task_id":"main","artifact":"abc123","environment":"production","approver":"release-manager"});
    let granted = dispatch(&mut s, "approval.grant", approval.clone()).unwrap();
    let mut consume = approval;
    consume["id"] = granted["id"].clone();
    // A consumer names the approval, not who approved it.
    consume.as_object_mut().unwrap().remove("approver");
    assert_eq!(
        dispatch(&mut s, "approval.consume", consume).unwrap()["consumed"],
        true
    );
    // Accepting still needs an open task under an active charter.
    dispatch(
        &mut s,
        "task.handoff",
        json!({"id":"main","actor":"apps","target":"infra"}),
    )
    .unwrap();
    dispatch(&mut s, "charter.revoke", json!({"id":"project"})).unwrap();
    let error = dispatch(&mut s, "task.accept", json!({"id":"main","actor":"infra"})).unwrap_err();
    assert!(error.to_string().contains("revoked"), "{error}");
    dispatch(&mut s, "charter.put", json!({"id":"project","active":true})).unwrap();
    dispatch(
        &mut s,
        "task.update",
        json!({"id":"main","actor":"apps","status":"done"}),
    )
    .unwrap();
    let error = dispatch(&mut s, "task.accept", json!({"id":"main","actor":"infra"})).unwrap_err();
    assert!(error.to_string().contains("terminal"), "{error}");
    assert_eq!(s.task("main").unwrap()["owner"], "apps");
}

#[test]
fn agents_cannot_run_operator_commands_or_act_as_someone_else() {
    use privateer::store::Caller;
    let (_temp, mut s) = fixture();
    s.set_caller(Caller::Agent {
        actor: "navigator".into(),
    });
    for command in privateer::OPERATOR_COMMANDS {
        let error = dispatch(&mut s, command, json!({"id":"project"})).unwrap_err();
        assert!(
            error.to_string().contains("operator command"),
            "{command}: {error}"
        );
    }
    let error = dispatch(
        &mut s,
        "task.update",
        json!({"id":"main","actor":"apps","status":"running"}),
    )
    .unwrap_err();
    assert!(error.to_string().contains("cannot act as"), "{error}");
    let error = dispatch(
        &mut s,
        "message.send",
        json!({"id":"m1","task_id":"main","from":"apps","to":"infra","body":"hi"}),
    )
    .unwrap_err();
    assert!(error.to_string().contains("cannot act as"), "{error}");
    // Ordinary department work under the charter still works.
    dispatch(
        &mut s,
        "task.update",
        json!({"id":"main","actor":"navigator","status":"running"}),
    )
    .unwrap();
    dispatch(
        &mut s,
        "message.send",
        json!({"id":"m2","task_id":"main","from":"navigator","to":"infra","body":"hi"}),
    )
    .unwrap();
    dispatch(
        &mut s,
        "task.create",
        json!({"id":"child","parent":"main","owner":"infra","title":"Delegated"}),
    )
    .unwrap();
    // Nothing the agent tried changed the charter.
    assert_eq!(s.charter("project").unwrap()["revision"], 1);
}

#[test]
fn caller_role_can_be_narrowed_but_never_widened() {
    use privateer::store::Caller;
    let agent = Caller::Agent {
        actor: "apps".into(),
    };
    assert_eq!(
        Caller::resolve(None, None, None, None).unwrap(),
        Caller::Operator
    );
    assert_eq!(
        Caller::resolve(Some("agent"), Some("apps"), Some("operator"), None).unwrap(),
        agent
    );
    assert_eq!(
        Caller::resolve(None, None, Some("agent"), Some("apps")).unwrap(),
        agent
    );
    assert!(Caller::resolve(Some("agent"), None, None, None).is_err());
    assert!(Caller::resolve(Some("agent"), Some("apps"), None, Some("infra")).is_err());
    assert!(Caller::resolve(Some("root"), None, None, None).is_err());
    assert!(Caller::resolve(Some("agent"), Some("bad name"), None, None).is_err());
}

#[test]
fn approvals_name_an_approver_other_than_the_task_owner() {
    let (temp, mut s) = fixture();
    let grant = |approver: Option<&str>| {
        let mut args = json!({"task_id":"main","artifact":"abc123","environment":"production"});
        if let Some(approver) = approver {
            args["approver"] = json!(approver);
        }
        args
    };
    assert!(dispatch(&mut s, "approval.grant", grant(None)).is_err());
    let error = dispatch(&mut s, "approval.grant", grant(Some("navigator"))).unwrap_err();
    assert!(error.to_string().contains("task owner"), "{error}");
    let granted = dispatch(&mut s, "approval.grant", grant(Some("release-manager"))).unwrap();
    assert_eq!(granted["approver"], "release-manager");
    // A home created before approvers were recorded gains the column on open.
    s.connection()
        .execute_batch("ALTER TABLE approvals DROP COLUMN approver;")
        .unwrap();
    drop(s);
    let mut s = Store::open(&temp.path().join("home")).unwrap();
    dispatch(&mut s, "approval.grant", grant(Some("release-manager"))).unwrap();
}
