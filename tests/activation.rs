mod support;

use privateer::dispatch;
use serde_json::json;
use std::time::Duration;
use support::{Fleet, now_settings};

#[test]
fn mail_wakes_the_first_mate_which_answers_and_its_usage_is_recorded() {
    let mut fleet = Fleet::new(now_settings());
    let request = fleet.send(
        json!({"id":"staging","from":"apps","to":"infra","kind":"request","body":"Is staging healthy?"}),
    );
    let woke = Fleet::infra(&fleet.tick());
    assert_eq!(woke["status"], "running", "{woke}");
    assert_eq!(woke["through_seq"], request["seq"]);
    let wake = fleet.settle();
    assert_eq!(wake["status"], "succeeded", "{wake}");
    assert_eq!(wake["mode"], "fresh");
    assert_eq!(wake["usage"]["cached_input_tokens"], 1000);
    assert_eq!(wake["usage"]["output_tokens"], 80);
    // The first mate answered as its ship and acknowledged what it saw.
    let answered = dispatch(&mut fleet.store, "mail.show", json!({"id":"staging"})).unwrap();
    assert_eq!(answered["state"], "answered");
    let reply = dispatch(&mut fleet.store, "mail.inbox", json!({"address":"apps"})).unwrap();
    assert_eq!(reply["messages"][0]["body"], "handled by infra");
    let status = dispatch(&mut fleet.store, "status", json!({})).unwrap();
    assert_eq!(status["open_requests"], 0);
    // The wake text follows the stable brief, so the brief stays a cacheable prefix.
    let prompt = &fleet.prompts()[0];
    assert!(prompt.starts_with("Task infra-duty:"), "{prompt}");
    assert!(prompt.contains("New mail through seq"));
    assert!(prompt.contains("id staging from apps kind request"));
    // Crew guidance appears only when crew mail is in the wake.
    assert!(!prompt.contains("Mail from crew"));
    // Nothing new: no further wake.
    let idle = Fleet::infra(&fleet.tick());
    assert_eq!(idle["skipped"], "nothing wake-worthy");
}

#[test]
fn an_awake_first_mate_is_not_woken_twice_and_the_next_wake_covers_only_new_mail() {
    let mut fleet = Fleet::new(now_settings());
    fleet.mode("sleep 1.5");
    fleet.send(json!({"id":"first","from":"apps","to":"infra","kind":"request","body":"one"}));
    assert_eq!(Fleet::infra(&fleet.tick())["status"], "running");
    fleet.send(json!({"id":"second","from":"apps","to":"infra","kind":"request","body":"two"}));
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "first mate already awake"
    );
    // The tick that records the first wake as finished wakes the first mate
    // again for the mail that arrived meanwhile; settle waits for both.
    fleet.settle();
    let wakes = fleet.wakes();
    assert_eq!(wakes.len(), 2);
    assert!(wakes.iter().all(|w| w["status"] == "succeeded"));
    let prompts = fleet.prompts();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[1].contains("id second"));
    assert!(!prompts[1].contains("id first"));
}

#[test]
fn routine_notices_wait_for_the_next_real_wake() {
    let mut fleet = Fleet::new(now_settings());
    fleet.send(
        json!({"id":"fyi","from":"apps","to":"infra","kind":"notice","urgency":"low","body":"FYI: docs updated"}),
    );
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "nothing wake-worthy"
    );
    fleet.send(
        json!({"id":"real","from":"apps","to":"infra","kind":"notice","body":"Deploy window moved"}),
    );
    assert_eq!(Fleet::infra(&fleet.tick())["status"], "running");
    fleet.settle();
    let prompt = &fleet.prompts()[0];
    assert!(prompt.contains("id fyi") && prompt.contains("id real"));
}

#[test]
fn batching_and_the_hourly_budget_defer_wakes() {
    let mut fleet = Fleet::new(json!({"batch_secs":3600,"max_delay_secs":3600}));
    fleet.send(json!({"from":"apps","to":"infra","kind":"notice","body":"one"}));
    assert_eq!(Fleet::infra(&fleet.tick())["skipped"], "batching");
    fleet.configure(json!({"batch_secs":0,"max_delay_secs":0,"max_wakes_per_hour":1}));
    assert_eq!(Fleet::infra(&fleet.tick())["status"], "running");
    fleet.settle();
    fleet.send(json!({"from":"apps","to":"infra","kind":"notice","body":"two"}));
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "hourly wake budget spent"
    );
}

#[test]
fn a_failed_wake_backs_off_then_retries_the_same_mail() {
    let mut fleet =
        Fleet::new(json!({"batch_secs":0,"max_delay_secs":0,"retry_backoff_secs":3600}));
    fleet.mode("fail");
    fleet
        .send(json!({"id":"retry-me","from":"apps","to":"infra","kind":"request","body":"please"}));
    fleet.tick();
    let failed = fleet.settle();
    assert_eq!(failed["status"], "failed");
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "backing off after a failed wake"
    );
    fleet.configure(now_settings());
    fleet.mode("normal");
    assert_eq!(Fleet::infra(&fleet.tick())["status"], "running");
    assert_eq!(fleet.settle()["status"], "succeeded");
    assert_eq!(
        dispatch(&mut fleet.store, "mail.show", json!({"id":"retry-me"})).unwrap()["state"],
        "answered"
    );
}

#[test]
fn only_one_operator_dispatcher_runs_per_home() {
    let fleet = Fleet::new(now_settings());
    let agent = fleet
        .command()
        .args(["serve", "--json", "{\"once\":true}"])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "infra")
        .output()
        .unwrap();
    assert!(!agent.status.success());
    let mut background = fleet
        .command()
        .args(["serve", "--json", "{\"interval_secs\":1}"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let second = fleet
        .command()
        .args(["serve", "--json", "{\"once\":true}"])
        .output()
        .unwrap();
    background.kill().unwrap();
    background.wait().unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already running"));
}

#[test]
fn first_mate_profiles_are_validated() {
    let mut fleet = Fleet::new(now_settings());
    let manifest = fleet.root.join("manifest.json");
    for bad in [
        json!({"manifest": manifest, "duty_task": "apps-duty"}),
        json!({"manifest": "relative/manifest.json", "duty_task": "infra-duty"}),
        json!({"duty_task": "infra-duty"}),
        json!({"manifest": manifest, "duty_task": "infra-duty", "batch_secs": -1}),
        json!({"manifest": manifest, "duty_task": "infra-duty", "surprise": true}),
    ] {
        assert!(
            dispatch(
                &mut fleet.store,
                "ship.put",
                json!({"name":"infra","first_mate":bad})
            )
            .is_err(),
            "{bad}"
        );
    }
    let ship = dispatch(&mut fleet.store, "ship.show", json!({"name":"infra"})).unwrap();
    assert_eq!(ship["first_mate"]["max_wakes_per_hour"], 12);
}

#[test]
fn wakes_group_mail_by_focus_carry_handoff_notes_and_run_under_the_focus_task() {
    let mut fleet = Fleet::new(now_settings());
    let first = fleet.send(
        json!({"id":"repair-status","from":"apps","to":"infra","kind":"request","task_id":"infra-repair","body":"How is the repair going?"}),
    );
    let woke = Fleet::infra(&fleet.tick());
    assert_eq!(woke["focus"], "task:infra-repair", "{woke}");
    assert_eq!(woke["task_id"], "infra-repair");
    fleet.settle();
    // The first mate left a note for the focus it worked on.
    let note = dispatch(
        &mut fleet.store,
        "note.get",
        json!({"ship":"infra","focus":"task:infra-repair"}),
    )
    .unwrap();
    assert_eq!(note["notes"][0]["author"], "infra");
    assert_eq!(
        note["notes"][0]["body"],
        format!("Seen through seq {}", first["seq"])
    );
    // Later mail on the same task brings that note back; an unrelated
    // question is its own thread, run under the duty task.
    fleet.send(
        json!({"id":"repair-more","from":"apps","to":"infra","kind":"notice","task_id":"infra-repair","body":"Also check the cache"}),
    );
    fleet.send(
        json!({"id":"unrelated","from":"apps","to":"infra","kind":"request","body":"Any disk alerts?"}),
    );
    let woke = Fleet::infra(&fleet.tick());
    assert_eq!(woke["task_id"], "infra-repair");
    assert_eq!(
        woke["focuses"],
        json!(["task:infra-repair", "thread:unrelated"])
    );
    fleet.settle();
    let prompt = fleet.prompts().pop().unwrap();
    assert!(prompt.contains("## task:infra-repair"), "{prompt}");
    assert!(prompt.contains("Your handoff note (v1)"));
    assert!(prompt.contains(&format!("Seen through seq {}", first["seq"])));
    assert!(prompt.contains("## thread:unrelated"));
    assert!(prompt.contains("Bearings for infra"));
    // Mail with no owned task runs under the duty task.
    fleet.send(
        json!({"id":"just-a-question","from":"apps","to":"infra","kind":"request","body":"Which region?"}),
    );
    let woke = Fleet::infra(&fleet.tick());
    assert_eq!(woke["focus"], "thread:just-a-question");
    assert_eq!(woke["task_id"], "infra-duty");
    fleet.settle();
}

#[test]
fn a_warm_single_focus_session_is_resumed_with_only_the_new_mail() {
    let mut fleet = Fleet::new(now_settings());
    fleet.send(
        json!({"id":"q1","from":"apps","to":"infra","kind":"request","task_id":"infra-repair","body":"Start the repair?"}),
    );
    fleet.tick();
    let first = fleet.settle();
    assert_eq!(first["mode"], "fresh");
    let session = first["session_id"].as_str().unwrap().to_owned();
    fleet.send(
        json!({"id":"q2","from":"apps","to":"infra","kind":"request","task_id":"infra-repair","body":"And the cache?"}),
    );
    let woke = Fleet::infra(&fleet.tick());
    assert_eq!(woke["mode"], "resume", "{woke}");
    let second = fleet.settle();
    assert_eq!(second["session_id"], session.as_str());
    // A resumed wake sends only the wake text: no brief, bearings or notes.
    let prompt = fleet.prompts().pop().unwrap();
    assert!(prompt.starts_with("Wake wake-"), "{prompt}");
    assert!(!prompt.contains("Bearings for"));
    assert!(!prompt.contains("Your handoff note"));
    assert!(prompt.contains("id q2"));
}

#[test]
fn cold_mixed_rotated_or_failed_sessions_start_fresh() {
    // Outside the cache window.
    let mut fleet = Fleet::new(
        json!({"batch_secs":0,"max_delay_secs":0,"retry_backoff_secs":0,"cache_window_secs":0}),
    );
    for id in ["a1", "a2"] {
        fleet.send(json!({"id":id,"from":"apps","to":"infra","kind":"notice","task_id":"infra-repair","body":id}));
        fleet.tick();
        assert_eq!(fleet.settle()["mode"], "fresh");
    }
    // Past the rotation limit.
    let mut fleet = Fleet::new(
        json!({"batch_secs":0,"max_delay_secs":0,"retry_backoff_secs":0,"session_max_wakes":1}),
    );
    for id in ["b1", "b2"] {
        fleet.send(json!({"id":id,"from":"apps","to":"infra","kind":"notice","task_id":"infra-repair","body":id}));
        fleet.tick();
        assert_eq!(fleet.settle()["mode"], "fresh");
    }
    // Mail spanning two foci never resumes one focus's conversation.
    let mut fleet = Fleet::new(now_settings());
    fleet.send(
        json!({"from":"apps","to":"infra","kind":"notice","task_id":"infra-repair","body":"c1"}),
    );
    fleet.tick();
    fleet.settle();
    fleet.send(
        json!({"from":"apps","to":"infra","kind":"notice","task_id":"infra-repair","body":"c2"}),
    );
    fleet.send(json!({"id":"other","from":"apps","to":"infra","kind":"notice","body":"elsewhere"}));
    fleet.tick();
    assert_eq!(fleet.settle()["mode"], "fresh");
    // A failed resumed wake drops the session; the retry starts fresh.
    let mut fleet =
        Fleet::new(json!({"batch_secs":0,"max_delay_secs":0,"retry_backoff_secs":3600}));
    fleet.send(
        json!({"from":"apps","to":"infra","kind":"notice","task_id":"infra-repair","body":"d1"}),
    );
    fleet.tick();
    fleet.settle();
    fleet.mode("fail");
    fleet.send(
        json!({"from":"apps","to":"infra","kind":"notice","task_id":"infra-repair","body":"d2"}),
    );
    assert_eq!(Fleet::infra(&fleet.tick())["mode"], "resume");
    assert_eq!(fleet.settle()["status"], "failed");
    fleet.mode("normal");
    fleet.configure(now_settings());
    assert_eq!(Fleet::infra(&fleet.tick())["mode"], "fresh");
    fleet.settle();
}
