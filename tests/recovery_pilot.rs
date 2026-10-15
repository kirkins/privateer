//! One retained, isolated recovery run through the real CLI and supervisor.
mod support;

use serde_json::Value;
use std::fs;
use std::path::Path;

#[test]
fn two_ship_fixture_recovers_without_duplicate_work_or_surviving_payloads() {
    let output = support::command("python3")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/recovery-pilot.py"))
        .args([
            "--binary",
            env!("CARGO_BIN_EXE_privateer"),
            "--timeout",
            "90",
        ])
        .output()
        .expect("Python 3 is required for the isolated recovery pilot");
    assert!(
        output.status.success(),
        "recovery pilot failed; retained evidence location follows:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(summary["passed"], true);
    assert_eq!(summary["native_inference"], false);
    let evidence: Value =
        serde_json::from_slice(&fs::read(summary["evidence"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(evidence["schema_version"], 1);
    assert_eq!(evidence["cleanup"]["passed"], true);
    assert!(evidence["cleanup"]["errors"].as_array().unwrap().is_empty());
    for assertion in [
        "baseline_regression_fails",
        "accepted_task_has_one_owner",
        "crew_decision_answered",
        "payload_survives_dispatcher_interruption",
        "repair_finishes_while_dispatcher_is_down",
        "independent_regression_passes_at_recorded_head",
        "job_replay_preserves_incarnation",
        "one_durable_crew_outcome_before_restart",
        "payload_and_owned_helper_exit_confirmed",
        "restart_delivers_durable_result",
        "custody_survives_recovery",
        "cross_home_request_and_reply_are_durable",
        "one_decision_and_one_outcome_after_replays",
        "one_worker_invocation",
    ] {
        assert_eq!(
            evidence["assertions"][assertion]["passed"], true,
            "{assertion}"
        );
    }
    let root = Path::new(summary["directory"].as_str().unwrap());
    for log in evidence["command_logs"].as_array().unwrap() {
        let bytes = fs::read(root.join(log.as_str().unwrap())).unwrap();
        assert!(bytes.len() <= 1024 * 1024);
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let command: Value = serde_json::from_slice(line).unwrap();
            assert!(command["argv"].is_array() || command["dispatcher"].is_string());
        }
    }
    assert_eq!(evidence["binary_sha256"].as_str().unwrap().len(), 64);
    assert!(!evidence["tested_head"].as_str().unwrap().is_empty());
}
