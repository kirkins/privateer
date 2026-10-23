//! Verdict custody across real supervised subprocesses, independent of the
//! native harness's result-line convention. No model or provider is involved.
mod support;

use privateer::errand;
use serde_json::{Value, json};
use std::{
    fs, thread,
    time::{Duration, Instant},
};
use support::{Fleet, now_settings};

const HARNESS: &str = r#"import json, pathlib, re, sys, time
cli, prompt = json.loads(sys.argv[1]), sys.argv[3]
match = re.search(r'^Errand (errand-\S+)', prompt, re.M)
if not match:
    sys.exit(0)
root = pathlib.Path(sys.argv[4]).parent
mode = pathlib.Path(sys.argv[4]).read_text().strip()
home = pathlib.Path(cli[cli.index('--home') + 1])
# The supervisor owns one incarnation directory per launch.
job = home / 'jobs' / match.group(1)
directory, = [p for p in job.iterdir() if p.is_dir()]
verdict = 'APPROVE\n' + 'complete verdict ' * 140
reply = directory / 'reply'
event = json.dumps({'type':'result', 'is_error':False, 'result':verdict})
if mode in ('sidecar-only', 'unmarked-sidecar', 'review-sidecar'):
    if mode == 'review-sidecar': verdict += '\nVERDICT: APPROVE'
    reply.write_text(verdict)
    if mode != 'unmarked-sidecar': (directory / 'reply.ended').touch()
elif mode == 'partial-sidecar':
    with reply.open('w') as stream:
        stream.write(verdict[:40]); stream.flush()
        (root / 'ready').touch()
        while not (root / 'release').exists(): time.sleep(.01)
        stream.write(verdict[40:])
    (directory / 'reply.ended').touch()
elif mode == 'partial-frame':
    sys.stdout.write(event[:40]); sys.stdout.flush()
    (root / 'ready').touch()
    while not (root / 'release').exists(): time.sleep(.01)
    print(event[40:], flush=True)
elif mode == 'result-only':
    print(event, flush=True)
elif mode == 'unconfirmed':
    print(json.dumps({'type':'tool_execution_start','toolName':'bash', 'args':{'command':'not for the notice'}}), flush=True)
reason = {'unconfirmed':'event-size', 'spaced-reason':'failed with private details', 'path-reason':'/private/config/secret'}.get(mode, 'Native completion was not confirmed.')
print(json.dumps({'type':'native.turn.failed', 'reason':reason}), flush=True)
sys.exit(2)
"#;

fn fixture(mode: &str) -> (Fleet, String) {
    let fleet = Fleet::new(now_settings());
    fleet.ok(
        "charter.put",
        json!({"id":"fleet","actions":["inspect","spawn","message"]}),
        None,
    );
    fs::write(fleet.root.join("mate.py"), HARNESS).unwrap();
    fleet.mode(mode);
    let started = fleet.ok(
        "errand.start",
        json!({"focus":"thread:verdict","brief":"Review the cache."}),
        Some("infra"),
    );
    let id = started["errand"].as_str().unwrap().to_owned();
    (fleet, id)
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "fake subprocess did not reach its boundary"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn terminal(fleet: &Fleet, id: &str) -> Value {
    let mut job = Value::Null;
    wait_until(|| {
        job = fleet.ok("job.status", json!({"id":id}), None);
        matches!(job["status"].as_str(), Some("failed" | "succeeded"))
    });
    job
}

fn notice(fleet: &mut Fleet, id: &str) -> Value {
    errand::settle(&mut fleet.store).unwrap();
    // A second settlement (including after reopen) cannot mail twice.
    let mut reopened = privateer::store::Store::open(&fleet.home).unwrap();
    assert!(errand::settle(&mut reopened).unwrap().is_empty());
    let history = fleet.ok(
        "mail.inbox",
        json!({"address":"infra","all":true}),
        Some("infra"),
    );
    let messages: Vec<_> = history["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["id"] == format!("{id}-verdict"))
        .cloned()
        .collect();
    assert_eq!(messages.len(), 1, "{history}");
    messages[0].clone()
}

#[test]
fn a_marked_complete_sidecar_without_a_result_line_is_mailed() {
    let (mut fleet, id) = fixture("sidecar-only");
    let job = terminal(&fleet, &id);
    let logs = fleet
        .home
        .join("jobs")
        .join(&id)
        .join(job["incarnation"].as_str().unwrap());
    assert!(logs.join("reply.ended").exists());
    assert!(
        !fs::read_to_string(logs.join("stdout.log"))
            .unwrap()
            .contains("APPROVE")
    );
    let mail = notice(&mut fleet, &id);
    assert!(
        mail["body"]
            .as_str()
            .unwrap()
            .contains("APPROVE\ncomplete verdict"),
        "{mail}"
    );
    assert_eq!(mail["urgency"], "normal");
    // Deliver the verdict without claiming the failing harness succeeded.
    assert_eq!(mail["payload"]["verdict"], "unconfirmed");
    assert_eq!(mail["payload"]["status"], "failed");
}

#[test]
fn unmarked_legacy_sidecars_without_matching_ended_log_replies_are_not_verdicts() {
    for log in [
        "{\"type\":\"native.turn.failed\"}\n",
        concat!(
            "{\"type\":\"result\",\"result\":\"Earlier progress\"}\n",
            "{\"type\":\"privateer.log.truncated\",\"dropped_bytes\":900000}\n",
            "{\"type\":\"native.turn.failed\"}\n"
        ),
    ] {
        let (mut fleet, id) = fixture("unmarked-sidecar");
        let job = terminal(&fleet, &id);
        let logs = fleet
            .home
            .join("jobs")
            .join(&id)
            .join(job["incarnation"].as_str().unwrap());
        assert!(!logs.join("reply.ended").exists());
        assert!(!logs.join("reply.incomplete").exists());
        assert!(
            fs::read_to_string(logs.join("reply"))
                .unwrap()
                .contains("APPROVE")
        );
        fs::write(logs.join("stdout.log"), log).unwrap();
        let mail = notice(&mut fleet, &id);
        let body = mail["body"].as_str().unwrap();
        assert!(body.contains("without a verdict"), "{mail}");
        assert!(!body.contains("APPROVE"), "{mail}");
        assert!(!body.contains("Earlier progress"), "{mail}");
        assert!(mail["payload"].get("verdict").is_none(), "{mail}");
        assert_eq!(mail["urgency"], "high");
    }
}

#[test]
fn a_result_line_is_a_verdict_even_without_a_sidecar() {
    let (mut fleet, id) = fixture("result-only");
    let job = terminal(&fleet, &id);
    let logs = fleet
        .home
        .join("jobs")
        .join(&id)
        .join(job["incarnation"].as_str().unwrap());
    fs::remove_file(logs.join("reply")).unwrap();
    fs::remove_file(logs.join("reply.ended")).unwrap();
    let mail = notice(&mut fleet, &id);
    assert!(
        mail["body"]
            .as_str()
            .unwrap()
            .contains("APPROVE\ncomplete verdict"),
        "{mail}"
    );
}

#[test]
fn an_unconfirmed_run_without_a_reply_names_its_cause_and_last_tool() {
    let (mut fleet, id) = fixture("unconfirmed");
    terminal(&fleet, &id);
    let mail = notice(&mut fleet, &id);
    let body = mail["body"].as_str().unwrap();
    assert!(body.contains("failed without a verdict"), "{mail}");
    assert!(
        body.contains("event size; last tool: bash (started)"),
        "{mail}"
    );
    assert!(!body.contains("not for the notice"), "{mail}");
    assert_eq!(mail["urgency"], "high");
    assert_eq!(
        mail["payload"]["ended"],
        "event size; last tool: bash (started)"
    );
}

#[test]
fn untrusted_native_failure_reasons_do_not_enter_the_notice_or_error() {
    for mode in ["spaced-reason", "path-reason"] {
        let (mut fleet, id) = fixture(mode);
        terminal(&fleet, &id);
        let mail = notice(&mut fleet, &id);
        assert_eq!(mail["payload"]["ended"], "native completion not confirmed");
        let body = mail["body"].as_str().unwrap();
        assert!(body.contains("native completion not confirmed"), "{mail}");
        assert!(!body.contains("private details"), "{mail}");
        assert!(!body.contains("/private/config/secret"), "{mail}");
        let error: String = fleet
            .store
            .connection()
            .query_row("SELECT error FROM errands WHERE id=?1", [&id], |r| r.get(0))
            .unwrap();
        assert_eq!(error, "errand job failed (native completion not confirmed)");
    }
}

#[test]
fn a_failed_review_purpose_errand_cannot_approve_from_its_saved_reply() {
    let (mut fleet, id) = fixture("review-sidecar");
    let job = terminal(&fleet, &id);
    assert_eq!(job["status"], "failed");
    // Seed the review-purpose record normally created before launch. This
    // isolates settlement from forge access and does not grant posting rights.
    fleet
        .store
        .connection()
        .execute(
            "INSERT INTO reviews(id, ship, task_id, forge, repo, pr, base_ref, base_sha,
            head_ref, head_sha, head_tree, errand_id, route_name, route_identity,
            route_model, route_tier, route_uses, route_why, fresh, authors, declared,
            folder, clone, verdict, post_state, created_ms, kind)
         VALUES('review-failed', 'infra', 'infra-duty', 'forgejo', 'example/project', 1,
            'main', 'base', 'feature', 'head', 'tree', ?1, 'reviewer', 'test/-/model',
            'model', 1, '[\"review\"]', 'test fixture', 1, '{}', '[]', ?2, ?2,
            'running', 'pending', 0, 'errand')",
            rusqlite::params![id, fleet.root.join("repo").to_str().unwrap()],
        )
        .unwrap();
    let mail = notice(&mut fleet, &id);
    assert!(
        mail["body"].as_str().unwrap().contains("VERDICT: APPROVE"),
        "{mail}"
    );
    assert_eq!(mail["payload"]["status"], "failed");
    assert_eq!(mail["payload"]["verdict"], "unconfirmed");
    assert_eq!(mail["payload"]["review"]["verdict"], "failed");
    let verdict: String = fleet
        .store
        .connection()
        .query_row(
            "SELECT verdict FROM reviews WHERE errand_id=?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(verdict, "failed");
}

#[test]
fn a_failed_notice_write_rolls_back_settlement_for_restart_recovery() {
    let (mut fleet, id) = fixture("sidecar-only");
    terminal(&fleet, &id);
    fleet
        .store
        .connection()
        .execute_batch(
            "CREATE TRIGGER reject_verdict BEFORE INSERT ON mail WHEN NEW.id LIKE '%-verdict'
         BEGIN SELECT RAISE(ABORT, 'fixture mail failure'); END;",
        )
        .unwrap();
    assert!(errand::settle(&mut fleet.store).is_err());
    let status: String = fleet
        .store
        .connection()
        .query_row("SELECT status FROM errands WHERE id=?1", [&id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(status, "running");
    fleet
        .store
        .connection()
        .execute_batch("DROP TRIGGER reject_verdict")
        .unwrap();
    fleet.store = privateer::store::Store::open(&fleet.home).unwrap();
    let mail = notice(&mut fleet, &id);
    assert!(mail["body"].as_str().unwrap().contains("APPROVE"), "{mail}");
}

#[test]
fn explicit_negative_provenance_never_resurrects_a_saved_reply() {
    let (mut fleet, id) = fixture("sidecar-only");
    let job = terminal(&fleet, &id);
    let logs = fleet
        .home
        .join("jobs")
        .join(&id)
        .join(job["incarnation"].as_str().unwrap());
    // Model the conservative state after a crash between publishing negative
    // provenance and clearing the old positive marker.
    fs::write(logs.join("reply.ended"), "").unwrap();
    fs::write(logs.join("reply.incomplete"), "").unwrap();
    let mail = notice(&mut fleet, &id);
    assert!(
        mail["body"].as_str().unwrap().contains("without a verdict"),
        "{mail}"
    );
    assert!(
        !mail["body"].as_str().unwrap().contains("APPROVE"),
        "{mail}"
    );
}

#[test]
fn settlement_waits_for_the_writer_and_capture_before_reading_a_verdict() {
    for mode in ["partial-sidecar", "partial-frame"] {
        let (mut fleet, id) = fixture(mode);
        wait_until(|| fleet.root.join("ready").exists());
        // The first look is while a file/frame is only partly written. A
        // terminal job is published only after capture has drained and joined.
        assert!(errand::settle(&mut fleet.store).unwrap().is_empty());
        let history = fleet.ok(
            "mail.inbox",
            json!({"address":"infra","all":true}),
            Some("infra"),
        );
        assert!(
            !history["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["id"] == format!("{id}-verdict"))
        );
        fs::write(fleet.root.join("release"), "go").unwrap();
        let job = terminal(&fleet, &id);
        let logs = fleet
            .home
            .join("jobs")
            .join(&id)
            .join(job["incarnation"].as_str().unwrap());
        let saved = fs::read_to_string(logs.join("reply")).unwrap();
        assert_eq!(
            saved,
            format!("APPROVE\n{}", "complete verdict ".repeat(140))
        );
        let mail = notice(&mut fleet, &id);
        assert!(
            mail["body"].as_str().unwrap().contains(&saved),
            "{mode}: {mail}"
        );
    }
}
