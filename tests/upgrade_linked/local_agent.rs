//! Agent-local requests use the real dispatcher/coordinator/apply subprocesses.
use super::*;

fn args(approval: &str) -> Value {
    json!({"commit":B,"scope":"local","task_id":"upgrade-task","approval_id":approval})
}
fn local_plan(machine: &Machine) -> Value {
    machine.json("upgrade.plan", json!({"commit":B,"scope":"local"}), true)
}
fn queued(machine: &Machine) -> i64 {
    machine
        .database()
        .query_row(
            "SELECT COUNT(*) FROM metadata WHERE key LIKE 'upgrade-request:%'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}
fn enqueue(machine: &Machine, approval: &str) -> Value {
    let plan = local_plan(machine);
    machine.approve(approval, B, &plan["environment"]);
    machine.json("upgrade.start", args(approval), true)
}

#[test]
fn local_plan_ignores_reverse_links_and_desk_placement_without_side_effects() {
    let dir = support::tempdir();
    let alpha = Machine::new(dir.path().join("alpha"), true);
    let beta = Machine::new(dir.path().join("beta"), true);
    alpha.link(&beta, "beta"); // beta has only the reverse-only link.
    let calls = fs::read_to_string(beta.root.join("calls")).unwrap_or_default();
    let plan = local_plan(&beta);
    assert_eq!(plan["machines"], json!([{"name":"local"}]));
    assert_ne!(plan["environment"], local_plan(&alpha)["environment"]);
    assert_eq!(plan["dry_run"], true);
    assert!(
        plan["environment"]
            .as_str()
            .unwrap()
            .starts_with("upgrade-local:")
    );
    let linked = beta.rpc("upgrade.plan", json!({"commit":B}), true);
    assert!(!linked.status.success(), "{}", text(&linked));
    assert!(text(&linked).contains("captain's desk"));
    assert_eq!(
        fs::read_to_string(beta.root.join("calls")).unwrap_or_default(),
        calls
    );
    assert!(!alpha.root.join("calls").exists());
    beta.assert_nothing_prepared();
    assert!(!beta.data().join("upgrade").exists());
    assert_eq!(queued(&beta), 0);
    for scope in [json!(null), json!(false), json!("all")] {
        assert!(
            !beta
                .rpc("upgrade.plan", json!({"commit":B,"scope":scope}), true)
                .status
                .success()
        );
    }
}

#[test]
fn local_approval_checks_are_exact_atomic_and_scope_separated() {
    let dir = support::tempdir();
    let alpha = Machine::new(dir.path().join("alpha"), true);
    alpha.request_setup();
    let local = local_plan(&alpha);
    let linked = alpha.json("upgrade.plan", json!({"commit":B}), true);
    assert_ne!(local["environment"], linked["environment"]);
    alpha.approve("linked", B, &linked["environment"]);
    assert!(
        !alpha
            .rpc("upgrade.start", args("linked"), true)
            .status
            .success()
    );
    alpha.approve("local", B, &local["environment"]);
    assert!(!alpha.request(Some("local")).status.success());
    assert!(
        !alpha
            .rpc("upgrade.start", args("missing"), true)
            .status
            .success()
    );
    let mut missing_id = args("missing");
    missing_id.as_object_mut().unwrap().remove("approval_id");
    assert!(
        !alpha
            .rpc("upgrade.start", missing_id, true)
            .status
            .success()
    );
    for (id, commit, environment) in [
        ("wrong-commit", A, local["environment"].clone()),
        (
            "wrong-environment",
            B,
            json!("upgrade-local:another-installation"),
        ),
        ("expired", B, local["environment"].clone()),
        ("legacy", B, local["environment"].clone()),
        ("consumed", B, local["environment"].clone()),
        ("charter", B, local["environment"].clone()),
        ("approver", B, local["environment"].clone()),
    ] {
        alpha.approve(id, commit, &environment);
        let db = alpha.database();
        match id {
            "expired" => {
                db.execute("UPDATE approvals SET expires_ms=0 WHERE id=?1", [id])
                    .unwrap();
            }
            "legacy" => {
                db.execute("UPDATE approvals SET expires_ms=NULL WHERE id=?1", [id])
                    .unwrap();
            }
            "consumed" => {
                db.execute("UPDATE approvals SET consumed=1 WHERE id=?1", [id])
                    .unwrap();
            }
            "charter" => {
                db.execute(
                    "UPDATE approvals SET charter_hash='stale' WHERE id=?1",
                    [id],
                )
                .unwrap();
            }
            "approver" => {
                db.execute("UPDATE approvals SET approver='other' WHERE id=?1", [id])
                    .unwrap();
            }
            _ => {}
        }
        let out = alpha.rpc("upgrade.start", args(id), true);
        assert!(!out.status.success(), "{id}: {}", text(&out));
        assert_eq!(queued(&alpha), 0, "{id}");
    }
    assert!(
        !alpha
            .command(&["upgrade", B, "--local"])
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", "requester")
            .output()
            .unwrap()
            .status
            .success()
    );
    let request = alpha.json("upgrade.start", args("local"), true);
    assert_eq!(request["dispatch"], "pending");
    assert!(request["approval_expires_ms"].as_i64().unwrap() > privateer::store::Store::now_ms());
    assert_eq!(queued(&alpha), 1);
    assert!(
        !alpha
            .rpc("upgrade.start", args("local"), true)
            .status
            .success()
    );
    let round = alpha.wait("linked", request);
    assert_eq!(round["outcome"], "switched", "{round}");
    assert!(!alpha.data().join("upgrade-prepare").exists());
}

#[test]
fn concurrent_local_starts_consume_one_approval_and_record_one_custody() {
    let dir = support::tempdir();
    let alpha = Machine::new(dir.path().join("alpha"), true);
    alpha.request_setup();
    alpha.approve("race", B, &local_plan(&alpha)["environment"]);
    let body = args("race").to_string();
    let spawn = || {
        alpha
            .command(&["upgrade.start", "--json", &body])
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", "requester")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let one = spawn();
    let two = spawn();
    let one = one.wait_with_output().unwrap();
    let two = two.wait_with_output().unwrap();
    assert_eq!(
        usize::from(one.status.success()) + usize::from(two.status.success()),
        1,
        "one: {} two: {}",
        text(&one),
        text(&two)
    );
    assert_eq!(queued(&alpha), 1);
    let out = if one.status.success() { one } else { two };
    let request: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(alpha.wait("linked", request)["outcome"], "switched");
}

#[test]
fn local_dispatch_uses_operator_environment_and_recovers_one_unsent_outcome() {
    let dir = support::tempdir();
    let alpha = Machine::new(dir.path().join("alpha"), true);
    let beta = Machine::new(dir.path().join("beta"), true);
    alpha.link(&beta, "beta");
    let calls = fs::read_to_string(beta.root.join("calls")).unwrap_or_default();
    beta.request_setup();
    beta.approve("operator-worker", B, &local_plan(&beta)["environment"]);
    let poison = beta.root.join("poison");
    let marker = beta.root.join("agent-environment-used");
    executable(
        &poison,
        &format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    );
    let out = beta
        .command(&[
            "upgrade.start",
            "--json",
            &args("operator-worker").to_string(),
        ])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "requester")
        .env("PRIVATEER_GH", &poison)
        .env("PRIVATEER_UPGRADE_SOURCE", &poison)
        .env("PRIVATEER_SYSTEMCTL", &poison)
        .env("PRIVATEER_LAUNCHCTL", &poison)
        .env("PRIVATEER_SERVICE", "systemd")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let request: Value = serde_json::from_slice(&out.stdout).unwrap();
    let mut corrupted = request.clone();
    corrupted["machines"] = json!([{"name":"beta","route_hash":"untrusted"}]);
    beta.database()
        .execute(
            "UPDATE metadata SET value=?2 WHERE key=?1",
            rusqlite::params![
                format!("upgrade-request:{}", request["id"].as_str().unwrap()),
                corrupted.to_string()
            ],
        )
        .unwrap();
    let round = beta.wait("linked", request.clone());
    assert_eq!(round["outcome"], "switched", "{round}");
    assert_eq!(round["dispatch"], "claimed");
    assert_eq!(round["machines"][0]["name"], "local");
    assert_eq!(round["machines"][0]["apply"]["kind"], "upgrade");
    assert_eq!(beta.selected(), B);
    assert_eq!(alpha.selected(), A);
    assert!(!marker.exists());
    assert!(!beta.data().join("upgrade-prepare").exists());
    assert_eq!(
        fs::read_to_string(beta.root.join("calls")).unwrap_or_default(),
        calls
    );
    // Reconstruct the precise crash boundary: journal saved, mail unsent.
    // Waiting above observes the state file before notify may commit; finish
    // that transaction via recovery first, then simulate its loss.
    beta.json("serve", json!({"once":true}), false);
    beta.outcome_reply(&request);
    let db = beta.database();
    db.execute(
        "DELETE FROM mail WHERE id=?1",
        [format!(
            "upgrade-outcome-{}",
            request["id"].as_str().unwrap()
        )],
    )
    .unwrap();
    db.execute(
        "UPDATE mail SET state='open',answered_by=NULL,answered_ms=NULL WHERE id=?1",
        [request["request_thread"].as_str().unwrap()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO metadata(key,value) VALUES(?1,?2)",
        rusqlite::params![
            format!("upgrade-request:{}", request["id"].as_str().unwrap()),
            round.to_string()
        ],
    )
    .unwrap();
    for _ in 0..2 {
        beta.json("serve", json!({"once":true}), false);
        assert_eq!(beta.outcome_reply(&request)["outcome"], "switched");
        assert_eq!(queued(&beta), 0);
    }
}

#[test]
fn local_dispatch_refuses_stale_selection_expiry_and_changed_installation() {
    for reason in ["selection", "expiry", "installation"] {
        let dir = support::tempdir();
        let alpha = Machine::new(dir.path().join("alpha"), true);
        alpha.request_setup();
        let mut request = enqueue(&alpha, "stale");
        match reason {
            "selection" => {
                let commit = "cccccccccccccccccccccccccccccccccccccccc";
                release(&alpha.data(), commit);
                symlink(
                    Path::new("releases").join(commit),
                    alpha.data().join("current.new"),
                )
                .unwrap();
                fs::rename(
                    alpha.data().join("current.new"),
                    alpha.data().join("current"),
                )
                .unwrap();
            }
            "expiry" => {
                request["approval_expires_ms"] = json!(0);
            }
            _ => {
                request["environment"] = json!("upgrade-local:changed");
            }
        }
        alpha
            .database()
            .execute(
                "UPDATE metadata SET value=?2 WHERE key=?1",
                rusqlite::params![
                    format!("upgrade-request:{}", request["id"].as_str().unwrap()),
                    request.to_string()
                ],
            )
            .unwrap();
        alpha.json("serve", json!({"once":true}), false);
        let reply = alpha.outcome_reply(&request);
        assert_eq!(reply["outcome"], "failed", "{reason}: {reply}");
        let expected = match reason {
            "selection" => "selected release changed",
            "expiry" => "expired before dispatch",
            _ => "changed",
        };
        assert!(
            reply["error"].as_str().unwrap().contains(expected),
            "{reply}"
        );
        assert!(!alpha.data().join("upgrade").exists());
        assert!(!alpha.data().join("upgrade-prepare").exists());
        assert_eq!(queued(&alpha), 0);
    }
}

#[test]
fn local_already_current_is_verified_without_an_apply_or_fetch() {
    let dir = support::tempdir();
    let alpha = Machine::new(dir.path().join("alpha"), true);
    alpha.request_setup();
    let request = enqueue(&alpha, "no-op");
    symlink(
        Path::new("releases").join(B),
        alpha.data().join("current.new"),
    )
    .unwrap();
    fs::rename(
        alpha.data().join("current.new"),
        alpha.data().join("current"),
    )
    .unwrap();
    let round = alpha.wait("linked", request);
    assert_eq!(round["outcome"], "already_current", "{round}");
    assert!(round["machines"][0]["apply"].is_null());
    assert!(!alpha.data().join("upgrade").exists());
    assert!(!alpha.data().join("upgrade-prepare").exists());
}

#[test]
fn local_requests_require_prior_operator_fetch_and_verified_releases() {
    for exists in [false, true] {
        let dir = support::tempdir();
        let alpha = Machine::new(dir.path().join("alpha"), exists);
        alpha.request_setup();
        if exists {
            fs::write(
                alpha
                    .data()
                    .join("releases")
                    .join(B)
                    .join("bin/privateer-dashboard"),
                "tampered",
            )
            .unwrap();
        }
        let request = enqueue(&alpha, "release-check");
        let round = alpha.wait("linked", request);
        assert_eq!(round["outcome"], "failed", "{round}");
        assert!(
            round["error"].as_str().unwrap().contains(if exists {
                "does not match SHA256SUMS"
            } else {
                "prior operator fetch"
            }),
            "{round}"
        );
        assert_eq!(alpha.selected(), A);
        assert!(!alpha.data().join("upgrade").exists());
        assert!(!alpha.data().join("upgrade-prepare").exists());
    }
}
