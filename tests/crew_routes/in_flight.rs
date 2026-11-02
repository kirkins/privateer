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
        Ok(())
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
        Ok(())
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
