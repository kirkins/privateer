use super::*;

/// Handshakes, not run-duration assumptions: no payload finishes until its
/// release file is written. Even a failed assertion releases every payload.
struct Gate(PathBuf);
impl Drop for Gate {
    fn drop(&mut self) {
        let _ = fs::write(self.0.join("release-all"), "");
    }
}
fn gated(f: &Fleet) -> Gate {
    let script = f.root.join("bin/codex");
    fs::write(
        &script,
        r#"#!/usr/bin/env python3
import os, pathlib, time
root = pathlib.Path(os.environ['HOME'])
id = os.environ['PRIVATEER_JOB']
(root / ('started-' + id)).touch()
while not (root / ('release-' + id)).exists() and not (root / 'release-all').exists():
    time.sleep(0.01)
raise SystemExit(1 if (root / ('fail-' + id)).exists() else 0)
"#,
    )
    .unwrap();
    fs::set_permissions(script, fs::Permissions::from_mode(0o700)).unwrap();
    Gate(f.root.clone())
}
fn started(f: &Fleet, id: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !f.root.join(format!("started-{id}")).exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "payload {id} never started"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
fn release(f: &Fleet, id: &str) {
    fs::write(f.root.join(format!("release-{id}")), "").unwrap();
}
fn ledger(f: &Fleet) -> rusqlite::Connection {
    rusqlite::Connection::open(f.root.join("ledger/resources.sqlite")).unwrap()
}
fn scalar(f: &Fleet, sql: &str) -> i64 {
    ledger(f).query_row(sql, [], |r| r.get(0)).unwrap()
}

#[test]
fn concurrent_real_launches_claim_the_budget_before_finishing_and_scan_once() {
    let f = Fleet::new();
    let _gate = gated(&f);
    let mut budget = codex("model-budget", "plan");
    budget["allowance"] = json!({"window_hours":5,"runs":3});
    budget["tier"] = json!(1);
    budget["crew_headroom_pct"] = json!(100);
    f.routes(
        json!({"routes":{"budget":budget,"other":codex("model-other","credits")},
        "crew":["budget","other"]}),
    );
    let mut children = vec![];
    for n in 0..6 {
        let args = json!({"task_id":"fix","id":format!("burst-{n}")}).to_string();
        children.push(
            f.command(Some("engineering"), &["agent.start", "--json", &args])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    let jobs: Vec<Value> = children
        .into_iter()
        .map(|c| {
            let output = c.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice(&output.stdout).unwrap()
        })
        .collect();
    let budget_jobs: Vec<_> = jobs
        .iter()
        .filter(|j| f.model(j) == "model-budget")
        .collect();
    assert_eq!(budget_jobs.len(), 3, "{jobs:?}");
    assert_eq!(
        scalar(
            &f,
            "SELECT COUNT(*) FROM route_runs WHERE route='codex/-/model-budget' AND accounted=0"
        ),
        3
    );
    let bearings = f.json(None, "ship.bearings", json!({"ship":"engineering"}));
    assert!(
        bearings["text"].as_str().unwrap().contains("3 of 3 runs"),
        "{bearings}"
    );
    for job in &jobs {
        started(&f, job["id"].as_str().unwrap());
    }
    let success = budget_jobs[0]["id"].as_str().unwrap();
    let failed = budget_jobs[1]["id"].as_str().unwrap();
    let cancelled = budget_jobs[2]["id"].as_str().unwrap();
    fs::write(f.root.join(format!("fail-{failed}")), "").unwrap();
    f.json(Some("engineering"), "job.cancel", json!({"id":cancelled}));
    // Cancellation is asynchronous. Keep this payload gated until its
    // supervisor has actually cancelled it: releasing it after only requesting
    // cancellation can let a normal successful exit win, legitimately spending
    // a second run and no longer exercising cancelled-claim settlement.
    f.settled(cancelled);
    assert_eq!(
        f.json(None, "job.status", json!({"id":cancelled}))["status"],
        "cancelled"
    );
    for job in &jobs {
        release(&f, job["id"].as_str().unwrap());
    }
    for job in &jobs {
        f.settled(job["id"].as_str().unwrap());
    }
    assert_eq!(
        f.json(None, "job.status", json!({"id":success}))["status"],
        "succeeded"
    );
    assert_eq!(
        f.json(None, "job.status", json!({"id":failed}))["status"],
        "failed"
    );
    // No dispatcher ran before these launches: its first pass must recover
    // their pending claims rather than begin its cursor after their finishes.
    f.tick();
    assert_eq!(
        scalar(
            &f,
            "SELECT runs FROM route_usage WHERE route='codex/-/model-budget'"
        ),
        1
    );
    assert_eq!(
        scalar(&f, "SELECT COUNT(*) FROM route_runs WHERE accounted=0"),
        0
    );
    // Model a crash after ledger commit but before the home scan cursor.
    f.in_process(|store| {
        store.connection().execute(
            "UPDATE metadata SET value='0' WHERE key='routes_scanned_ms'",
            [],
        )?;
        Ok::<(), anyhow::Error>(())
    })
    .unwrap();
    f.tick();
    assert_eq!(
        scalar(
            &f,
            "SELECT runs FROM route_usage WHERE route='codex/-/model-budget'"
        ),
        1
    );
}

#[test]
fn max_running_falls_through_refuses_named_and_queues_a_named_wait() {
    let f = Fleet::new();
    let _gate = gated(&f);
    let mut limited = codex("model-limited", "credits");
    limited["max_running"] = json!(1);
    f.routes(
        json!({"routes":{"limited":limited,"other":codex("model-other","credits")},
        "crew":["limited","other"],"errand":["limited","other"]}),
    );
    let first = f.json(
        Some("engineering"),
        "agent.start",
        json!({"id":"holder","task_id":"fix"}),
    );
    assert_eq!(f.model(&first), "model-limited");
    started(&f, "holder");
    let next = f.json(
        Some("engineering"),
        "agent.start",
        json!({"id":"overflow","task_id":"fix"}),
    );
    assert_eq!(f.model(&next), "model-other");
    let refusal = f.refused(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","route":"limited"}),
    );
    assert!(refusal.contains("1 running, max_running 1"), "{refusal}");
    let queued = f.json(
        Some("engineering"),
        "agent.start",
        json!({"id":"waiting","task_id":"fix", "route":"limited","wait_for_capacity":true}),
    );
    assert_eq!(queued["status"], "queued", "{queued}");
    assert!(
        queued["reason"].as_str().unwrap().contains("max_running 1"),
        "{queued}"
    );
    assert!(!f.root.join("started-waiting").exists());
    assert_eq!(
        scalar(
            &f,
            "SELECT COUNT(*) FROM reservations WHERE json_extract(record,'$.model_route')='codex/-/model-limited'"
        ),
        1
    );
    let bearings = f.json(None, "ship.bearings", json!({"ship":"engineering"}));
    let text = bearings["text"].as_str().unwrap();
    assert!(
        text.contains("1 running, max_running 1") && text.contains(" full"),
        "{text}"
    );
    // A route-only waiter must not become a host-capacity head-of-line
    // blocker. Give this job real CPU/memory/disk demand, but no host-load
    // assumptions: the fixture configures permissive host thresholds.
    let heavy = f.json(
        Some("engineering"),
        "job.start",
        json!({"id":"independent-build","task_id":"fix","command":["sh","-c","exit 0"],
            "heavy":true,"estimate":{"cpus":1,"memory_bytes":1048576,"disk_bytes":1048576}}),
    );
    assert_ne!(heavy["status"], "queued", "{heavy}");
    f.settled("independent-build");
    assert!(!f.root.join("started-waiting").exists());
    let queue = f.json(None, "resource.status", json!({}));
    assert!(
        queue["queue"]
            .as_array()
            .unwrap()
            .iter()
            .any(|q| q["job_id"] == "waiting"),
        "{queue}"
    );
    let snapshot = f.json(None, "fleet.snapshot", json!({}));
    assert!(
        snapshot.to_string().contains("\"max_running\":1"),
        "{snapshot}"
    );
    release(&f, "holder");
    f.settled("holder");
    started(&f, "waiting");
    assert_eq!(
        scalar(
            &f,
            "SELECT COUNT(*) FROM reservations WHERE json_extract(record,'$.model_route')='codex/-/model-limited'"
        ),
        1
    );
    release(&f, "waiting");
    release(&f, "overflow");
    f.settled("waiting");
    f.settled("overflow");
    f.tick();
    assert_eq!(
        scalar(&f, "SELECT COUNT(*) FROM route_runs WHERE accounted=0"),
        0
    );
}

#[test]
fn dispatcher_restart_settles_a_claim_from_an_unclaimed_supervisor() {
    let f = Fleet::new();
    let _gate = gated(&f);
    let mut budget = codex("model-budget", "plan");
    budget["allowance"] = json!({"window_hours":5,"runs":1});
    budget["tier"] = json!(1);
    budget["max_running"] = json!(1);
    budget["crew_headroom_pct"] = json!(100);
    f.routes(json!({"routes":{"budget":budget},"crew":["budget"]}));
    f.json(
        Some("engineering"),
        "agent.start",
        json!({"id":"template","task_id":"fix"}),
    );
    started(&f, "template");
    let mut job = f.json(None, "job.status", json!({"id":"template"}));
    let raw: String = ledger(&f)
        .query_row("SELECT record FROM reservations", [], |r| r.get(0))
        .unwrap();
    let mut reservation: Value = serde_json::from_str(&raw).unwrap();
    f.json(Some("engineering"), "job.cancel", json!({"id":"template"}));
    f.settled("template");
    f.tick();
    // Model the durable state at the ledger/home commit boundary, with no
    // supervisor ever claimed. Backdate the stored start grace, not the host
    // clock or a sleep, so the next dispatcher can deterministically settle it.
    let now = privateer::store::Store::now_ms();
    let key = format!(
        "{}:unclaimed:{}",
        f.root.join("engineering/.privateer").display(),
        job["incarnation"].as_str().unwrap()
    );
    job["id"] = json!("unclaimed");
    job["status"] = json!("starting");
    job["created_ms"] = json!(now - 31_000);
    for field in [
        "supervisor",
        "payload",
        "finished_ms",
        "spawned_ms",
        "launch_ms",
        "reason",
    ] {
        job[field] = Value::Null;
    }
    reservation["key"] = json!(key);
    reservation["job_id"] = json!("unclaimed");
    reservation["state"] = json!("starting");
    reservation["created_ms"] = json!(now - 31_000);
    reservation["supervisor"] = Value::Null;
    reservation["payload"] = Value::Null;
    f.in_process(|store| {
        store.connection().execute(
            "INSERT INTO jobs(id,record) VALUES('unclaimed',?1)",
            [job.to_string()],
        )?;
        Ok::<(), anyhow::Error>(())
    })
    .unwrap();
    ledger(&f)
        .execute(
            "INSERT INTO reservations(key,record) VALUES(?1,?2)",
            rusqlite::params![key, reservation.to_string()],
        )
        .unwrap();
    ledger(&f).execute("INSERT INTO route_runs(key,route,created_ms,window_ends_ms) VALUES(?1,'codex/-/model-budget',?2,?3)",rusqlite::params![key,now-31_000,now+3_600_000]).unwrap();
    f.tick();
    assert_eq!(
        f.json(None, "job.status", json!({"id":"unclaimed"}))["status"],
        "failed"
    );
    assert_eq!(scalar(&f, "SELECT COUNT(*) FROM reservations"), 0);
    assert_eq!(
        scalar(&f, "SELECT COUNT(*) FROM route_runs WHERE accounted=0"),
        0
    );
    let next = f.json(
        Some("engineering"),
        "agent.start",
        json!({"id":"after-restart","task_id":"fix"}),
    );
    assert_eq!(f.model(&next), "model-budget");
    release(&f, "after-restart");
    f.settled("after-restart");
}

#[test]
fn max_running_is_opt_in_positive_and_aliases_agree() {
    let f = Fleet::new();
    let file = f.root.join("bad-routes.json");
    for value in [
        json!(0),
        json!(-1),
        json!(1.5),
        json!("2"),
        Value::Null,
        json!(true),
    ] {
        let mut route = codex("model-limited", "credits");
        route["max_running"] = value;
        fs::write(&file, json!({"routes":{"limited":route}}).to_string()).unwrap();
        let output = f.run(None, &["routes", "set", file.to_str().unwrap()]);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("max_running must be a positive whole number")
        );
    }
    let mut limited = codex("model-limited", "credits");
    limited["max_running"] = json!(2);
    fs::write(
        &file,
        json!({"routes":{"limited":limited,"alias":codex("model-limited","credits")}}).to_string(),
    )
    .unwrap();
    let output = f.run(None, &["routes", "set", file.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("different max_running"));
}

#[test]
fn first_mate_wakes_bypass_a_full_route_but_count_against_later_crew_and_errands() {
    let f = Fleet::new();
    let _gate = gated(&f);
    let mut limited = codex("model-limited", "credits");
    limited["max_running"] = json!(1);
    f.routes(
        json!({"routes":{"limited":limited,"other":codex("model-other","credits")},
        "crew":["limited","other"],"errand":["limited","other"],"first_mate":["limited","other"]}),
    );
    f.json(
        Some("engineering"),
        "agent.start",
        json!({"id":"crew-holder","task_id":"fix","route":"limited"}),
    );
    started(&f, "crew-holder");
    let refusal = f.refused(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","route":"limited"}),
    );
    assert!(refusal.contains("1 running, max_running 1"), "{refusal}");
    // Model the activation boundary: a wake row is durable before agent.start.
    // Launch through the real agent path; a caller's wake string alone grants
    // no exemption (the ordinary crew launches above have none).
    f.in_process(|store| {
        store.connection().execute(
            "INSERT INTO wakes(id,ship,focus,through_seq,job_id,mode,status,created_ms)
         VALUES('wake-test','engineering','task:fix',0,'mate-wake','fresh','running',?1)",
            [privateer::store::Store::now_ms()],
        )
    })
    .unwrap();
    f.json(Some("engineering"), "agent.start", json!({"id":"mate-wake","task_id":"fix",
        "manifest":"bundled:codex","options":{"model":"model-limited","sandbox":"danger-full-access"},"wake":"Inspect the ship"}));
    started(&f, "mate-wake");
    assert_eq!(
        scalar(
            &f,
            "SELECT COUNT(*) FROM reservations WHERE json_extract(record,'$.model_route')='codex/-/model-limited'"
        ),
        2
    );
    release(&f, "crew-holder");
    f.settled("crew-holder");
    let refusal = f.refused(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","route":"limited"}),
    );
    assert!(refusal.contains("1 running, max_running 1"), "{refusal}");
    let errand = f.json(
        Some("engineering"),
        "errand.start",
        json!({"focus":"task:fix","brief":"Inspect the change"}),
    );
    assert_eq!(errand["route"]["name"], "other", "{errand}");
    release(&f, "mate-wake");
    f.settled("mate-wake");
    // The errand uses the same gate script, so release its own job explicitly.
    let id = errand["errand"].as_str().unwrap();
    started(&f, id);
    release(&f, id);
    f.settled(id);
}

#[test]
fn aliases_use_the_same_headroom_at_selection_and_admission() {
    let f = Fleet::new();
    let _gate = gated(&f);
    let mut strict = codex("model-shared", "plan");
    strict["allowance"] = json!({"window_hours":5,"runs":2});
    strict["tier"] = json!(1);
    strict["crew_headroom_pct"] = json!(50);
    let mut alias = strict.clone();
    alias["crew_headroom_pct"] = json!(100);
    f.routes(
        json!({"routes":{"a-strict":strict,"z-alias":alias,"other":codex("model-other","credits")},
        "crew":["z-alias","other"],"errand":["z-alias","other"]}),
    );
    let first = f.json(
        Some("engineering"),
        "agent.start",
        json!({"id":"alias-first","task_id":"fix"}),
    );
    assert_eq!(f.model(&first), "model-shared");
    let second = f.json(
        Some("engineering"),
        "agent.start",
        json!({"id":"alias-second","task_id":"fix"}),
    );
    assert_eq!(f.model(&second), "model-other");
    let errand = f.json(
        Some("engineering"),
        "errand.start",
        json!({"focus":"task:fix","brief":"Inspect"}),
    );
    assert_eq!(errand["route"]["name"], "other", "{errand}");
    f.in_process(|store| {
        let rows: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM errands", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            rows, 1,
            "route planning must not create failed errand attempts"
        );
        let reviews: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM reviews", [], |r| r.get(0))
            .unwrap();
        assert_eq!(reviews, 0);
    });
    for id in [
        "alias-first",
        "alias-second",
        errand["errand"].as_str().unwrap(),
    ] {
        started(&f, id);
        release(&f, id);
        f.settled(id);
    }
}

#[test]
fn a_classic_home_does_not_load_or_apply_fleet_route_limits_at_admission() {
    let f = Fleet::new();
    let _gate = gated(&f);
    let mut route = codex("model-limited", "credits");
    route["max_running"] = json!(1);
    f.routes(json!({"routes":{"limited":route},"crew":["limited"]}));
    f.json(
        Some("engineering"),
        "agent.start",
        json!({"id":"fleet-holder","task_id":"fix"}),
    );
    started(&f, "fleet-holder");
    let config = f.root.join("engineering/.privateer/config.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    value.as_object_mut().unwrap().remove("ship");
    fs::write(config, value.to_string()).unwrap();
    for id in ["classic-valid", "classic-invalid"] {
        if id == "classic-invalid" {
            fs::write(f.root.join("fleet.routes.json"), "not valid JSON").unwrap();
        }
        f.json(
            None,
            "agent.start",
            json!({"id":id,"task_id":"fix","manifest":"bundled:codex",
            "options":{"model":"model-limited","sandbox":"danger-full-access"}}),
        );
        started(&f, id);
        release(&f, id);
        f.settled(id);
    }
    release(&f, "fleet-holder");
    f.settled("fleet-holder");
}

#[test]
fn a_resume_replaces_its_parked_claim_at_the_run_budget_edge() {
    let f = Fleet::new();
    let _gate = gated(&f);
    let mut budget = codex("model-budget", "plan");
    budget["allowance"] = json!({"window_hours":5,"runs":1});
    budget["tier"] = json!(1);
    budget["crew_headroom_pct"] = json!(100);
    f.routes(json!({"routes":{"budget":budget},"crew":["budget"]}));
    f.json(
        Some("engineering"),
        "agent.start",
        json!({"id":"predecessor","task_id":"fix"}),
    );
    started(&f, "predecessor");
    f.json(
        Some("engineering"),
        "job.cancel",
        json!({"id":"predecessor"}),
    );
    f.settled("predecessor");
    // Model the durable park/continuation boundary with a real, stopped
    // incarnation and its original open claim. No process survives the park.
    // Only the exact continuation allowed by launch_blockers can admit it.
    f.in_process(|store| {
        store.connection().execute(
            "UPDATE jobs SET record=json_set(record,'$.status','parked','$.finished_ms',NULL,
             '$.cancel_requested',json('false'),'$.park','park-edge') WHERE id='predecessor'", [],)?;
        store.connection().execute(
            "INSERT INTO crew_parks(id,root,crew_job_id,child_job_id,task_id,ship,state,next,attempt,created_ms,updated_ms)
             VALUES('park-edge','predecessor','predecessor','settled-child','fix','engineering','resuming','continue',1,?1,?1)",
            [privateer::store::Store::now_ms()],)?;
        Ok::<(), anyhow::Error>(())
    }).unwrap();
    let resumed = f.json(None, "agent.start", json!({"id":"crew-resume-park-edge-1","task_id":"fix",
        "manifest":"bundled:codex","options":{"model":"model-budget","sandbox":"danger-full-access"},
        "_resume_of":"predecessor"}));
    assert_eq!(f.model(&resumed), "model-budget");
    started(&f, "crew-resume-park-edge-1");
    assert_eq!(
        scalar(&f, "SELECT COUNT(*) FROM route_runs WHERE accounted=0"),
        1
    );
    assert_eq!(
        scalar(&f, "SELECT COUNT(*) FROM route_runs WHERE accounted=1"),
        1
    );
    release(&f, "crew-resume-park-edge-1");
    f.settled("crew-resume-park-edge-1");
    f.tick();
    assert_eq!(
        scalar(
            &f,
            "SELECT runs FROM route_usage WHERE route='codex/-/model-budget'"
        ),
        1
    );
}
