mod support;

use privateer::dispatch;
use serde_json::{Value, json};
use std::{
    fs,
    process::Command,
    thread,
    time::{Duration, Instant},
};
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
fn actionable_mail_after_a_full_routine_page_keeps_contiguous_wake_coverage() {
    let mut fleet = Fleet::new(json!({"batch_secs":0,"max_delay_secs":0,"max_wakes_per_hour":1}));
    for n in 0..205 {
        fleet.send(
            json!({"id":format!("routine-{n}"),"from":"apps","to":"infra",
            "kind":"notice","urgency":"low","body":"Routine progress"}),
        );
    }
    let request = fleet.send(json!({"id":"after-the-page","from":"apps","to":"infra",
        "kind":"request","urgency":"high","body":"Please decide"}));
    let first = Fleet::infra(&fleet.tick());
    assert_eq!(first["status"], "running", "{first}");
    assert_eq!(first["through_seq"], 200);
    let settled = fleet.settle();
    assert_eq!(settled["status"], "succeeded");
    // A successful first page cannot acknowledge or cover the later request.
    let inbox = fleet.ok("mail.inbox", json!({"address":"infra"}), None);
    assert_eq!(inbox["acknowledged_through"], 200);
    assert!(
        inbox["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == request["id"])
    );
    assert_eq!(
        fleet.ok("mail.show", json!({"id":request["id"]}), None)["state"],
        "open"
    );
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "hourly wake budget spent"
    );
    // When the budget permits another wake, it starts exactly after page one.
    fleet.configure(now_settings());
    assert_eq!(Fleet::infra(&fleet.tick())["through_seq"], request["seq"]);
    fleet.settle();
    assert_eq!(
        fleet.ok("mail.show", json!({"id":request["id"]}), None)["state"],
        "answered"
    );
    let history = fleet.ok(
        "mail.inbox",
        json!({"address":"infra","all":true,"limit":1000}),
        None,
    );
    assert_eq!(history["messages"].as_array().unwrap().len(), 206);
    let prompts = fleet.prompts();
    assert_eq!(prompts.len(), 2);
    assert!(prompts.iter().all(|prompt| prompt.len() < 32_768));
    assert!(!prompts[0].contains("id after-the-page"));
    assert!(prompts[1].contains("id after-the-page"));
}

#[test]
fn concurrent_legacy_activation_upgrades_preserve_history() {
    use privateer::store::Store;
    use std::sync::{Arc, Barrier};
    let fleet = Fleet::new(now_settings());
    fleet
        .store
        .connection()
        .execute(
            "INSERT INTO wakes(id, ship, focus, through_seq, mode, status, created_ms)
         VALUES('legacy', 'old-ship', 'ship', 17, 'fresh', 'succeeded', 1)",
            [],
        )
        .unwrap();
    for _ in 0..3 {
        fleet
            .store
            .connection()
            .execute_batch(
                "ALTER TABLE wakes DROP COLUMN focuses;
             ALTER TABLE wakes DROP COLUMN harness;
             ALTER TABLE wakes DROP COLUMN session_id;
             ALTER TABLE wakes DROP COLUMN dispatcher_generation;",
            )
            .unwrap();
        let barrier = Arc::new(Barrier::new(16));
        let openers: Vec<_> = (0..16)
            .map(|_| {
                let barrier = barrier.clone();
                let home = fleet.home.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    Store::open(&home)
                        .map(|store| {
                            store
                                .connection()
                                .query_row(
                                    "SELECT through_seq FROM wakes WHERE id='legacy'",
                                    [],
                                    |r| r.get::<_, i64>(0),
                                )
                                .unwrap()
                        })
                        .map_err(|e| e.to_string())
                })
            })
            .collect();
        for opener in openers {
            assert_eq!(opener.join().unwrap().unwrap(), 17);
        }
        let columns: i64 = fleet.store.connection().query_row(
            "SELECT COUNT(*) FROM pragma_table_info('wakes') WHERE name IN ('focuses','harness','session_id','dispatcher_generation')",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(columns, 4);
    }
}

#[test]
fn a_slow_peer_has_a_bounded_pass_and_local_work_and_other_peers_progress() {
    use privateer::store::Store;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;
    let mut fleet = Fleet::new(now_settings());
    let remote_home = fleet.root.join("remote");
    let mut remote = Store::open(&remote_home).unwrap();
    dispatch(
        &mut remote,
        "peer.put",
        json!({"name":"origin","transport":"local",
        "executable":support::BIN,"home":fleet.home}),
    )
    .unwrap();
    dispatch(
        &mut remote,
        "ship.put",
        json!({"name":"apps","location":{"peer":"origin"}}),
    )
    .unwrap();
    dispatch(&mut remote, "ship.put", json!({"name":"healthy"})).unwrap();
    for (peer, ship) in [("slow", "slow-ship"), ("healthy", "healthy")] {
        dispatch(
            &mut fleet.store,
            "peer.put",
            json!({"name":peer,"transport":"local",
            "executable":"/nonexistent/privateer","home":remote_home,"timeout_ms":5000}),
        )
        .unwrap();
        dispatch(
            &mut fleet.store,
            "ship.put",
            json!({"name":ship,"location":{"peer":peer}}),
        )
        .unwrap();
        fleet.send(json!({"id":format!("out-{peer}"),"from":"apps","to":ship,
            "kind":"notice","body":"queued while offline"}));
    }
    let marker = fleet.root.join("slow-started.json");
    let script = fleet.root.join("slow-peer.py");
    fs::write(&script, format!(
        "#!{}\nimport json, os, pathlib, sqlite3, time\ntime.sleep(0.25)  # exercise cold interpreter startup\nwith sqlite3.connect({:?}) as db:\n    wakes = db.execute(\"SELECT COUNT(*) FROM wakes\").fetchone()[0]\npathlib.Path({:?}).write_text(json.dumps({{\"pid\":os.getpid(),\"wakes\":wakes}}))\ntime.sleep(5)\n",
        support::python().display(), fleet.home.join("privateer.sqlite").to_string_lossy(), marker.to_string_lossy(),
    )).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    for (peer, executable) in [
        ("slow", script.as_path()),
        ("healthy", std::path::Path::new(support::BIN)),
    ] {
        dispatch(
            &mut fleet.store,
            "peer.put",
            json!({"name":peer,"transport":"local",
            "executable":executable,"home":remote_home,"timeout_ms":5000}),
        )
        .unwrap();
    }
    fleet
        .store
        .connection()
        .execute_batch(
            "UPDATE mail_delivery_retry SET next_attempt_ms=0,
           last_attempt_ms=CASE peer WHEN 'slow' THEN 1 ELSE 2 END;",
        )
        .unwrap();
    fleet.send(json!({"id":"local-request","from":"apps","to":"infra",
        "kind":"request","body":"local work must start before remote delivery"}));
    let tick = |budget| {
        let output = fleet
            .command()
            .args(["serve", "--json"])
            .arg(json!({"once":true,"delivery_budget_ms":budget}).to_string())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let started = Instant::now();
    // Budget for cold Python startup (including the injected delay), while
    // remaining well below the fixture's five-second blocked call and the
    // configured peer timeout. Production timeouts are unchanged.
    let first = tick(1000);
    assert!(started.elapsed() < Duration::from_secs(3), "{first}");
    assert_eq!(Fleet::infra(&first)["status"], "running", "{first}");
    assert_eq!(first["delivered"]["attempted"], 1);
    let observed: serde_json::Value = serde_json::from_slice(&fs::read(marker).unwrap()).unwrap();
    assert_eq!(
        observed["wakes"], 1,
        "local wake must precede the slow call"
    );
    // The capped peer call reaped its transport; no timed-out payload remains.
    assert_eq!(
        unsafe { libc::kill(observed["pid"].as_i64().unwrap() as i32, 0) },
        -1
    );
    let second = tick(1000);
    assert_eq!(second["delivered"]["delivered"], 1, "{second}");
    assert_eq!(second["delivered"]["pending"], 1);
    let inbox = dispatch(&mut remote, "mail.inbox", json!({"address":"healthy"})).unwrap();
    assert_eq!(inbox["messages"][0]["id"], "out-healthy");
    fleet.settle();
    assert_eq!(
        fleet.ok("mail.show", json!({"id":"local-request"}), None)["state"],
        "answered"
    );
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
    // Pins ride along with the note; a crew assignment stays out of wakes.
    dispatch(
        &mut fleet.store,
        "note.pin",
        json!({"ship":"infra","focus":"task:infra-repair","key":"head","value":"ad536c8"}),
    )
    .unwrap();
    dispatch(
        &mut fleet.store,
        "note.put",
        json!({"ship":"infra","focus":"assignment:infra-repair","body":"Rebuild the cache index and report the timings."}),
    )
    .unwrap();
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
    assert!(prompt.contains("Pinned: head=ad536c8"), "{prompt}");
    assert!(!prompt.contains("Rebuild the cache index"));
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
    fleet.ok(
        "charter.put",
        json!({"id":"fleet","actions":["inspect","spawn","message"]}),
        None,
    );
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
    assert_eq!(woke["task_id"], "infra-repair");
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

#[test]
fn a_read_message_delegate_first_mate_wakes_without_implementation_authority() {
    let mut fleet = Fleet::new(now_settings());
    dispatch(
        &mut fleet.store,
        "charter.put",
        json!({"id":"fleet","actions":["inspect","spawn","message"]}),
    )
    .unwrap();
    fleet.send(json!({"id":"liaison-question","from":"apps","to":"infra","kind":"request","task_id":"infra-repair","body":"Read and relay"}));
    let woke = Fleet::infra(&fleet.tick());
    assert_eq!(woke["status"], "running");
    assert_eq!(woke["task_id"], "infra-repair");
    let wake = fleet.settle();
    assert_eq!(wake["status"], "succeeded", "{wake}");
    assert_eq!(
        dispatch(
            &mut fleet.store,
            "mail.show",
            json!({"id":"liaison-question"})
        )
        .unwrap()["state"],
        "answered"
    );
    let job = dispatch(&mut fleet.store, "job.status", json!({"id":wake["job_id"]})).unwrap();
    assert_eq!(job["action"], "spawn");
    assert_eq!(job["task_id"], "infra-repair");
    for action in ["implement", "spawn"] {
        let denied = dispatch(
            &mut fleet.store,
            "job.start",
            json!({"task_id":"infra-duty","command":["sh","-c","exit 0"],"action":action}),
        );
        assert!(
            denied.is_err(),
            "ordinary jobs may not claim {action} under a liaison charter"
        );
    }
}

#[test]
fn task_linked_liaison_wakes_stop_the_harness_and_child_when_authority_ends() {
    fn active(pid: i64) -> bool {
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "stat="])
            .output()
            .unwrap();
        let stat = String::from_utf8(output.stdout).unwrap();
        !stat.trim().is_empty() && !stat.trim().starts_with('Z')
    }

    for revoke in [false, true] {
        let mut fleet = Fleet::new(now_settings());
        fleet.ok(
            "charter.put",
            json!({"id":"fleet","actions":["inspect","spawn","message"]}),
            None,
        );
        fleet.ok(
            "charter.put",
            json!({"id":"liaison-focus","objective":"Coordinate a peer request",
            "repo":fleet.root.join("repo"),"min_free_bytes":0,"job_timeout_secs":60,
            "actions":["inspect","spawn","message"]}),
            None,
        );
        fleet.ok(
            "task.create",
            json!({"id":"liaison-request","charter_id":"liaison-focus",
            "owner":"infra","title":"Relay peer request"}),
            None,
        );
        fs::write(fleet.root.join("mate.py"), r#"import json, os, pathlib, subprocess, sys
child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
pathlib.Path(sys.argv[5]).write_text(json.dumps({'pid': os.getpid(), 'child': child.pid, 'owner': sys.argv[2]}))
child.wait()
"#).unwrap();
        if revoke {
            fleet.send(
                json!({"id":"linked-request","from":"infra","to":"apps","kind":"request",
                "task_id":"liaison-request","body":"Which peer can help?"}),
            );
            fleet.send(
                json!({"id":"linked-reply","from":"apps","to":"infra","kind":"reply",
                "correlation":"linked-request","body":"Please coordinate with us"}),
            );
        } else {
            fleet.send(
                json!({"id":"linked-request","from":"apps","to":"infra","kind":"request",
                "task_id":"liaison-request","body":"Please coordinate with us"}),
            );
        }
        let woke = Fleet::infra(&fleet.tick());
        assert_eq!(woke["status"], "running", "{woke}");
        let job_id = &woke["wake"];
        let until = Instant::now() + Duration::from_secs(8);
        let (running, processes) = loop {
            let job = fleet.ok("job.status", json!({"id":job_id}), None);
            let processes = fs::read(fleet.root.join("prompts.log"))
                .ok()
                .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok());
            if let Some(processes) = processes.filter(|_| job["status"] == "running") {
                break (job, processes);
            }
            if Instant::now() >= until {
                fleet.ok("job.cancel", json!({"id":job_id}), None);
                panic!("harness did not start: {job}");
            }
            thread::sleep(Duration::from_millis(20));
        };
        let pid = processes["pid"].as_i64().unwrap();
        let child = processes["child"].as_i64().unwrap();
        let was_alive = active(pid) && active(child);
        if revoke {
            fleet.ok("charter.revoke", json!({"id":"liaison-focus"}), None);
        } else {
            fleet.ok(
                "task.update",
                json!({"id":"liaison-request","actor":"infra","status":"cancelled"}),
                None,
            );
        }
        let until = Instant::now() + Duration::from_secs(8);
        let stopped = loop {
            let job = fleet.ok("job.status", json!({"id":job_id}), None);
            if job["status"] == "cancelled" || Instant::now() >= until {
                break job;
            }
            thread::sleep(Duration::from_millis(20));
        };
        let payload_alive = active(pid);
        let child_alive = active(child);
        fleet.ok("job.cancel", json!({"id":job_id}), None);
        assert_eq!(woke["focus"], "task:liaison-request");
        assert_eq!(woke["task_id"], "liaison-request");
        assert_eq!(running["task_id"], "liaison-request");
        assert_eq!(
            fleet.ok("task.show", json!({"id":"liaison-request"}), None)["charter_id"],
            "liaison-focus"
        );
        assert_eq!(running["action"], "spawn");
        assert_eq!(running["agent_actor"], "infra");
        assert_eq!(running["payload"]["pid"], processes["pid"]);
        assert_eq!(processes["owner"], "infra");
        assert!(
            was_alive,
            "harness and child must be running before authority ends"
        );
        assert_eq!(stopped["status"], "cancelled", "{stopped}");
        let reason = stopped["reason"].as_str().unwrap();
        assert!(
            reason.contains(if revoke {
                "charter was revoked"
            } else {
                "task is terminal"
            }),
            "{reason}"
        );
        assert!(
            !payload_alive,
            "harness survived the end of its task authority"
        );
        assert!(
            !child_alive,
            "harness child survived the end of its task authority"
        );
        assert!(fleet.store.authorize("infra-duty", "spawn").is_ok());
    }
}

#[test]
fn liaison_wakes_keep_duty_fallback_for_ineligible_or_foreign_tasks() {
    for ineligible in ["foreign", "no-spawn", "cancelled", "done", "revoked"] {
        let mut fleet = Fleet::new(now_settings());
        fleet.ok(
            "charter.put",
            json!({"id":"fleet","actions":["inspect","spawn","message"]}),
            None,
        );
        fleet.ok("charter.put", json!({"id":"focus","objective":"Coordinate request",
            "repo":fleet.root.join("repo"),"min_free_bytes":0,"actions":["inspect","spawn","message"]}), None);
        let owner = if ineligible == "foreign" {
            "apps"
        } else {
            "infra"
        };
        fleet.ok(
            "task.create",
            json!({"id":"focus","charter_id":"focus","owner":owner,"title":"Peer request"}),
            None,
        );
        match ineligible {
            "no-spawn" => {
                fleet.ok(
                    "charter.put",
                    json!({"id":"focus","actions":["inspect","message"]}),
                    None,
                );
            }
            "cancelled" | "done" => {
                fleet.ok(
                    "task.update",
                    json!({"id":"focus","actor":"infra","status":ineligible}),
                    None,
                );
            }
            "revoked" => {
                fleet.ok("charter.revoke", json!({"id":"focus"}), None);
            }
            _ => {}
        }
        // Terminal tasks cannot acquire new open questions. A notice still
        // carries historical task context and must take the same duty fallback.
        let kind = if matches!(ineligible, "cancelled" | "done") {
            "notice"
        } else {
            "request"
        };
        fleet.send(json!({"id":"question","from":"apps","to":"infra","kind":kind,"task_id":"focus","body":"Can you relay this?"}));
        let woke = Fleet::infra(&fleet.tick());
        let wake = fleet.settle();
        assert_eq!(woke["task_id"], "infra-duty", "{ineligible}: {woke}");
        assert_eq!(wake["status"], "succeeded", "{ineligible}: {wake}");
        let job = fleet.ok("job.status", json!({"id":wake["job_id"]}), None);
        assert_eq!(job["task_id"], "infra-duty");
        assert_eq!(job["action"], "spawn");
        assert_eq!(job["agent_actor"], "infra");
    }
}

/// Tick until the errand has settled, and return its record.
fn errand_settled(fleet: &mut Fleet, id: &str) -> Value {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        fleet.tick();
        let list = fleet.ok("errand.list", json!({}), Some("infra"));
        let errand = list["errands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == id)
            .cloned()
            .unwrap();
        if errand["status"] != "running" {
            return errand;
        }
        assert!(std::time::Instant::now() < until, "errand never settled");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[test]
fn an_errand_thinks_in_its_own_session_and_mails_only_its_verdict() {
    let mut fleet = Fleet::new(now_settings());
    fleet.ok(
        "charter.put",
        json!({"id":"fleet","actions":["inspect","spawn","message"]}),
        None,
    );
    fleet.ok(
        "note.put",
        json!({"ship":"infra","focus":"task:infra-repair","body":"Crew rebuilt the cache index; review it before merging."}),
        Some("infra"),
    );
    // Only the ship that owns the focus sends errands on it.
    let refused = fleet.refused(
        "errand.start",
        json!({"focus":"task:infra-repair","brief":"Review it"}),
        Some("apps"),
    );
    assert!(refused.contains("not owned by apps"), "{refused}");

    let started = fleet.ok(
        "errand.start",
        json!({"focus":"task:infra-repair","brief":"Review the cache rebuild and say whether it can merge."}),
        Some("infra"),
    );
    assert_eq!(started["mode"], "fresh");
    let id = started["errand"].as_str().unwrap().to_owned();
    let bearings = fleet.ok("ship.bearings", json!({"ship":"infra"}), Some("infra"));
    assert!(
        bearings["text"]
            .as_str()
            .unwrap()
            .contains(&format!("- {id} on task:infra-repair"))
    );
    let errand = errand_settled(&mut fleet, &id);
    assert_eq!(errand["status"], "succeeded", "{errand}");
    // It started from the focus's records, not the first mate's thread.
    let prompt = fleet.prompts().pop().unwrap();
    assert!(
        prompt.contains(&format!("Errand {id} for the ship infra")),
        "{prompt}"
    );
    assert!(prompt.contains("Review the cache rebuild"));
    assert!(prompt.contains("review it before merging"));
    // The first mate receives the verdict and no crew outcome for the errand.
    let inbox = fleet.ok("mail.inbox", json!({"address":"infra"}), Some("infra"));
    let mail: Vec<&Value> = inbox["messages"].as_array().unwrap().iter().collect();
    assert_eq!(mail.len(), 1, "{inbox}");
    assert_eq!(mail[0]["from"], "crew");
    assert_eq!(
        mail[0]["body"],
        format!("Errand on task:infra-repair, verdict:\nReviewed: {id}. Brief seen: True")
    );

    // A follow-up on the same focus continues the errand's session.
    let again = fleet.ok(
        "errand.start",
        json!({"focus":"task:infra-repair","brief":"Check the fix to the cache test."}),
        Some("infra"),
    );
    assert_eq!(again["mode"], "resume");
    let next = again["errand"].as_str().unwrap().to_owned();
    assert_eq!(errand_settled(&mut fleet, &next)["status"], "succeeded");
    let prompt = fleet.prompts().pop().unwrap();
    assert!(
        prompt.starts_with(&format!("Errand {next}, continuing on task:infra-repair")),
        "{prompt}"
    );

    // An errand that dies without a reply says so.
    fleet.mode("fail");
    let failing = fleet.ok(
        "errand.start",
        json!({"focus":"thread:outage","brief":"Catch up on the outage thread."}),
        Some("infra"),
    );
    let failed = failing["errand"].as_str().unwrap().to_owned();
    assert_eq!(errand_settled(&mut fleet, &failed)["status"], "failed");
    let inbox = fleet.ok("mail.inbox", json!({"address":"infra"}), Some("infra"));
    let last = inbox["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert!(
        last["body"].as_str().unwrap().contains("without a verdict"),
        "{last}"
    );
    assert_eq!(last["urgency"], "high");
}

#[test]
fn a_throttled_model_route_backs_off_and_a_success_clears_it() {
    let mut fleet = Fleet::new(now_settings());
    fleet.ok(
        "charter.put",
        json!({"id":"fleet","actions":["inspect","spawn","message"]}),
        None,
    );
    let ledger_path = fleet.root.join("ledger/resources.sqlite");
    let ledger = || rusqlite::Connection::open(&ledger_path).unwrap();
    let routes =
        |fleet: &Fleet| fleet.ok("resource.status", json!({}), None)["model_routes"].clone();
    let crew = |fleet: &Fleet, id: &str| {
        fleet.run(
            "agent.start",
            json!({"id":id,"task_id":"infra-repair"}),
            Some("infra"),
        )
    };
    let wait_job = |fleet: &Fleet, id: &str| {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !matches!(
            fleet.ok("job.status", json!({"id":id}), None)["status"].as_str(),
            Some("succeeded" | "failed" | "cancelled" | "timed_out")
        ) {
            assert!(std::time::Instant::now() < until, "job {id} never ended");
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    };

    // A run that fails for another reason says nothing about the provider.
    fleet.tick();
    fleet.mode("fail");
    assert!(crew(&fleet, "broken").status.success());
    wait_job(&fleet, "broken");
    fleet.tick();
    assert_eq!(routes(&fleet), json!([]));

    // A run the provider throttled backs its route off.
    fleet.mode("throttle");
    assert!(crew(&fleet, "first").status.success());
    wait_job(&fleet, "first");
    fleet.tick();
    let backed = routes(&fleet);
    // A wake on the same route may also have been throttled meanwhile.
    let first_failures = backed[0]["failures"].as_i64().unwrap();
    assert!(first_failures >= 1, "{backed}");
    assert!(backed[0]["last_error"].as_str().unwrap().starts_with("429"));
    let until = backed[0]["until_ms"].as_i64().unwrap();

    // New crew on that route are refused, and headless wakes wait.
    let refused = crew(&fleet, "second");
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("backing off"));
    fleet.send(json!({"id":"q","from":"apps","to":"infra","kind":"request","body":"Ready?"}));
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "model route backing off"
    );
    let bearings = fleet.ok("ship.bearings", json!({"ship":"infra"}), Some("infra"));
    assert!(
        bearings["text"]
            .as_str()
            .unwrap()
            .contains("is backing off")
    );

    // Throttled again once it is allowed back: the backoff doubles.
    ledger()
        .execute("UPDATE model_routes SET until_ms=0", [])
        .unwrap();
    assert!(crew(&fleet, "third").status.success());
    wait_job(&fleet, "third");
    fleet.tick();
    let backed = routes(&fleet);
    assert!(
        backed[0]["failures"].as_i64().unwrap() > first_failures,
        "{backed}"
    );
    let doubled = backed[0]["until_ms"].as_i64().unwrap() - until;
    assert!(
        doubled >= 25_000,
        "the second backoff was not longer: {doubled} ms"
    );

    // A run that succeeds on the route clears it.
    ledger()
        .execute("UPDATE model_routes SET until_ms=0", [])
        .unwrap();
    fleet.mode("normal");
    fleet.tick();
    fleet.settle();
    fleet.tick();
    assert_eq!(routes(&fleet), json!([]));
    let left: i64 = ledger()
        .query_row("SELECT COUNT(*) FROM model_routes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0);
}

#[test]
fn codex_crew_sandbox_defaults_and_flags_read_only() {
    let fleet = Fleet::new(now_settings());
    // An isolated bundled:codex manifest so the test never runs real Codex.
    let plugins = fleet.root.join("plugins");
    fs::create_dir_all(plugins.join("codex")).unwrap();
    fs::write(
        plugins.join("codex/manifest.json"),
        json!({
            "schema_version":1,"id":"codex",
            "executable":["python3", fleet.root.join("codex_adapter.py")],
            "timeout_ms":2000,"max_output_bytes":65536
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        fleet.root.join("codex_adapter.py"),
        r#"import json,sys
r=json.loads(sys.stdin.readline())
sandbox=r.get("options",{}).get("sandbox","-")
print(json.dumps({"id":r["id"],"version":1,"ok":True,"result":{"argv":[sys.executable,"-c","import sys;sys.exit(0)","--sandbox",sandbox],"cwd":r["context"]["repo"],"env_names":[]}}))
"#,
    )
    .unwrap();
    let start = |args: Value| {
        let mut cmd = fleet.command();
        cmd.env("PRIVATEER_PLUGINS", &plugins)
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", "infra")
            .args(["agent.start", "--json", &args.to_string()]);
        cmd.output().unwrap()
    };
    let repo = fleet.root.join("repo");

    let default = start(json!({"id":"codex-default","task_id":"infra-repair",
        "manifest":"bundled:codex","options":{},"cwd":repo}));
    assert!(
        default.status.success(),
        "{}",
        String::from_utf8_lossy(&default.stderr)
    );
    let job: Value = serde_json::from_slice(&default.stdout).unwrap();
    assert!(
        job["command"]
            .as_array()
            .unwrap()
            .windows(2)
            .any(|p| p == ["--sandbox", "danger-full-access"]),
        "{job}"
    );
    assert_ne!(job["codex_read_only_crew"], json!(true), "{job}");

    let read_only = start(json!({"id":"codex-read-only","task_id":"infra-repair",
        "manifest":"bundled:codex","options":{"sandbox":"read-only"},"cwd":repo}));
    assert!(
        read_only.status.success(),
        "{}",
        String::from_utf8_lossy(&read_only.stderr)
    );
    let job: Value = serde_json::from_slice(&read_only.stdout).unwrap();
    assert!(
        job["command"]
            .as_array()
            .unwrap()
            .windows(2)
            .any(|p| p == ["--sandbox", "read-only"]),
        "{job}"
    );
    assert_eq!(job["codex_read_only_crew"], json!(true), "{job}");

    // Malformed options must reach the clean typed error, never a panic.
    let malformed = start(json!({"id":"codex-malformed","task_id":"infra-repair",
        "manifest":"bundled:codex","options":"nope","cwd":repo}));
    assert!(!malformed.status.success());
    assert!(
        String::from_utf8_lossy(&malformed.stderr).contains("plugin options must be an object"),
        "{}",
        String::from_utf8_lossy(&malformed.stderr)
    );
}
