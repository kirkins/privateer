use super::*;

pub(super) fn setup() -> (tempfile::TempDir, Store, DelegatedContext) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.connection().execute_batch("INSERT INTO ships(name,record) VALUES('survey','{\"location\":\"here\"}'); INSERT INTO charters(id,data) VALUES('survey','{}'); INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('survey','survey','survey','survey','open',0);").unwrap();
    let c=DelegatedContext::from_gateway(&json!({"version":1,"client":uuid::Uuid::new_v4().to_string(),"resource":"door:alpha","scopes":["fleet:read"],"ships":["survey"],"result_id":uuid::Uuid::new_v4().to_string()})).unwrap();
    (dir, store, c)
}
pub(super) fn add(store: &Store, id: &str, state: &str) {
    store.connection().execute("INSERT INTO jobs(id,record) VALUES(?1,?2)",params![id,json!({"id":id,"incarnation":"alpha","task_id":"survey","status":state,"created_ms":100,"spawned_ms":200,"finished_ms":if ["succeeded","failed","cancelled","timed_out","resumed"].contains(&state) {Some(300)} else {None},"command":["argv-canary"],"env":{"TOKEN":"env-canary"},"cwd":"/absolute/cwd-canary","ledger":"/absolute/ledger-canary","plugin_binding":{"secret":"binding-canary"},"supervisor":{"pid":999,"started":"process-canary"},"action":"command","heavy":false,"timeout_secs":60,"cancel_requested":false}).to_string()]).unwrap();
}
#[test]
fn states_ages_lineage_native_parity_and_no_home_writes() {
    let (dir, mut store, c) = setup();
    for (n, s) in STATES.iter().enumerate() {
        add(&store, &format!("alpha-{n}"), s);
    }
    store.connection().execute("INSERT INTO crew_parks(id,root,crew_job_id,child_job_id,task_id,ship,next,state,created_ms,updated_ms) VALUES('alpha','alpha-4','alpha-4','alpha-0','survey','survey','password=park-secret','parked',250,260)",[]).unwrap();
    store.connection().execute("UPDATE jobs SET record=json_set(record,'$.resume_of','alpha-10','$.retry_of','alpha-7','$.started_by','alpha-0') WHERE id='alpha-3'",[]).unwrap();
    let db = dir.path().join("privateer.sqlite");
    let before = std::fs::read(&db).unwrap();
    let wal = std::fs::read(db.with_file_name("privateer.sqlite-wal")).ok();
    let scan = scan(
        &store,
        "survey",
        &c,
        false,
        0,
        i64::MAX,
        None,
        None,
        false,
        1000,
    )
    .unwrap();
    assert!(scan.complete);
    assert_eq!(scan.jobs.len(), 11);
    assert_eq!(scan.counts["completed"], 5);
    for (_, j) in &scan.jobs {
        let native =
            crate::jobs::dispatch(&mut store, "job.status", json!({"id":j.reference.id})).unwrap();
        assert_eq!(native["status"], j.state);
        assert_eq!(native["task_id"], j.reference.task_id);
        assert_eq!(j.created.age_ms, Some(900));
        if !j.completed {
            assert_eq!(j.state_since.at_ms, None);
        }
        if j.state == "queued" {
            assert_eq!(j.wait.as_ref().unwrap().since.age_ms, Some(900));
        }
        if j.state == "parked" {
            let w = j.wait.as_ref().unwrap();
            assert_eq!(w.since.age_ms, Some(740));
            assert!(w.next.as_ref().unwrap().withheld);
        }
        if j.state == "resumed" {
            assert!(j.resumed_as.is_some());
            assert_eq!(j.exit, "resumed");
        }
    }
    let wire = serde_json::to_string(&scan.jobs).unwrap();
    for secret in [
        "argv-canary",
        "env-canary",
        "cwd-canary",
        "ledger-canary",
        "binding-canary",
        "park-secret",
        "pid",
        "command\":",
    ] {
        assert!(!wire.contains(secret), "{secret}: {wire}");
    }
    assert_eq!(std::fs::read(&db).unwrap(), before);
    assert_eq!(
        std::fs::read(db.with_file_name("privateer.sqlite-wal")).ok(),
        wal
    );
}
#[test]
fn exact_custody_worker_and_incarnation_refusals() {
    let (_dir, store, c) = setup();
    add(&store, "alpha", "running");
    let wanted = reference(
        &record(&store, "survey", "alpha", &c).unwrap().unwrap(),
        "survey",
        &origin(&store).unwrap(),
    )
    .unwrap();
    assert!(inspect(&store, "survey", &wanted, &c, false, 1000).is_ok());
    for field in ["origin", "incarnation", "task_id", "ship", "id"] {
        let mut wrong = serde_json::to_value(&wanted).unwrap();
        wrong[field] = json!("gamma");
        assert!(
            inspect(
                &store,
                "survey",
                &serde_json::from_value(wrong).unwrap(),
                &c,
                false,
                1000
            )
            .is_err(),
            "{field}"
        );
    }
    for sql in [
        "UPDATE jobs SET record=json_set(record,'$.incarnation','beta')",
        "UPDATE tasks SET owner='relay'",
        "UPDATE jobs SET record=json_set(record,'$.agent_actor','relay')",
    ] {
        store.connection().execute(sql, []).unwrap();
        assert!(inspect(&store, "survey", &wanted, &c, false, 1000).is_err());
        store
            .connection()
            .execute("UPDATE tasks SET owner='survey'", [])
            .unwrap();
        store.connection().execute("UPDATE jobs SET record=json_remove(json_set(record,'$.incarnation','alpha'),'$.agent_actor')",[]).unwrap();
    }
    store.connection().execute("INSERT INTO wakes(id,ship,focus,through_seq,mode,status,created_ms,job_id) VALUES('beta','relay','task:survey',0,'headless','running',0,'alpha')",[]).unwrap();
    assert!(
        scan(
            &store,
            "survey",
            &c,
            false,
            0,
            i64::MAX,
            None,
            None,
            false,
            1000
        )
        .unwrap()
        .jobs
        .is_empty()
    );
}
#[test]
fn scheduled_unscheduled_backlog_is_not_workers_and_wait_time_is_unknown() {
    let (_dir, store, c) = setup();
    store
        .connection()
        .execute("UPDATE tasks SET status='done'", [])
        .unwrap();
    for n in 0..40 {
        store.connection().execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES(?1,'survey','survey','title-secret','open',0)",[format!("alpha-{n}")]).unwrap();
    }
    let idle = super::super::census::read(&store, "survey").unwrap();
    assert_eq!(idle.active_workers, 0);
    assert!(idle.workers_complete);
    store.connection().execute("INSERT INTO task_schedule(task_id,record) VALUES('alpha-0',?1)",[json!({"task_id":"alpha-0","owner":"survey","state":"waiting","revision":3,"reason":"password=reason-secret","wait":{"kind":"remote","ref":"hidden-ref","ship":"relay"},"created_ms":100,"last_checked_ms":200,"not_before_ms":2000,"lease_until_ms":0,"offer_id":null,"holder":null}).to_string()]).unwrap();
    let rows = scan(
        &store,
        "survey",
        &c,
        true,
        0,
        i64::MAX,
        None,
        None,
        false,
        1000,
    )
    .unwrap();
    assert!(rows.complete);
    assert_eq!(rows.counts["backlog"], 40);
    assert_eq!(rows.counts["scheduled"], 1);
    assert_eq!(rows.counts["unscheduled_total"], 40);
    let scheduled = &rows.work.iter().find(|(_, w)| w.scheduled).unwrap().1;
    assert_eq!(scheduled.wait.as_ref().unwrap().since.at_ms, None);
    assert!(scheduled.reason.withheld);
    assert_eq!(scheduled.last_checked.age_ms, Some(800));
    assert_eq!(scheduled.next_check.age_ms, None);
    let wire = serde_json::to_string(&rows.work).unwrap();
    for secret in ["reason-secret", "hidden-ref", "relay", "title-secret"] {
        assert!(!wire.contains(secret), "{wire}");
    }
}
#[test]
fn text_requires_opt_in_and_mail_scope_and_masks_controls_secrets_paths() {
    let (_dir, store, c) = setup();
    add(&store, "alpha", "running");
    let raw = "\u{1b}[31mpassword=secret-canary\u{1b}[0m\nTOKEN=token-canary /absolute/path https://private.invalid user@host\u{1b}]0;terminal-secret\u{7}\u{0}";
    let shown = safe_text(raw);
    for canary in [
        "secret-canary",
        "token-canary",
        "absolute",
        "private.invalid",
        "user@host",
        "terminal-secret",
    ] {
        assert!(!shown.contains(canary), "{shown}");
    }
    assert!(!shown.chars().any(char::is_control));
    assert!(safe_text(&"x".repeat(20_000)).chars().count() <= 512);
    store.connection().execute("INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms) VALUES('alpha-progress','survey','survey','notice',?1,?2,'normal',500)",params![raw,json!({"job_id":"alpha","incarnation":"alpha","report":"progress"}).to_string()]).unwrap();
    let wanted = reference(
        &record(&store, "survey", "alpha", &c).unwrap().unwrap(),
        "survey",
        &origin(&store).unwrap(),
    )
    .unwrap();
    let metadata = inspect(&store, "survey", &wanted, &c, false, 1000).unwrap();
    assert!(metadata.progress.unwrap().withheld);
    assert!(
        inspect(&store, "survey", &wanted, &c, true, 1000)
            .unwrap()
            .progress
            .unwrap()
            .withheld
    );
    let mut wire = c.gateway();
    wire["scopes"] = json!(["fleet:read", "mail:read"]);
    let mail = DelegatedContext::from_gateway(&wire).unwrap();
    let body = inspect(&store, "survey", &wanted, &mail, true, 1000)
        .unwrap()
        .progress
        .unwrap();
    assert_eq!(body.at.age_ms, Some(500));
    assert_eq!(body.text.unwrap(), shown);
}
#[test]
fn bounded_scan_filters_before_counting_and_reports_partial_windows() {
    let (_dir, store, c) = setup();
    let tx = store.connection().unchecked_transaction().unwrap();
    for n in 0..SCAN + 5 {
        add(&store, &format!("alpha-{n}"), "running");
    }
    tx.commit().unwrap();
    let none = scan(
        &store,
        "survey",
        &c,
        false,
        0,
        i64::MAX,
        Some("failed"),
        None,
        false,
        1000,
    )
    .unwrap();
    assert!(none.jobs.is_empty());
    assert!(none.counts.is_empty());
    assert!(!none.complete);
    assert_eq!(none.last, SCAN as i64);
    let tail = scan(
        &store,
        "survey",
        &c,
        false,
        none.last,
        i64::MAX,
        None,
        None,
        false,
        1000,
    )
    .unwrap();
    assert_eq!(tail.jobs.len(), 5);
    assert!(!tail.complete);
}

#[test]
fn hidden_worker_coverage_does_not_depend_on_state_or_kind_filter() {
    let (_dir, store, c) = setup();
    add(&store, "alpha", "running");
    store
        .connection()
        .execute(
            "UPDATE jobs SET record=json_set(record,'$.agent_actor','relay','$.action','spawn')",
            [],
        )
        .unwrap();
    for state in [None, Some("running"), Some("failed"), Some("completed")] {
        for kind in [None, Some("crew"), Some("command")] {
            let rows = scan(
                &store,
                "survey",
                &c,
                false,
                0,
                i64::MAX,
                state,
                kind,
                false,
                1000,
            )
            .unwrap();
            assert!(!rows.complete, "{state:?} {kind:?}");
            assert!(rows.counts.is_empty());
            assert!(rows.jobs.is_empty());
        }
    }
}

#[test]
fn parks_require_matching_task_and_current_parked_state() {
    let (_dir, store, c) = setup();
    add(&store, "alpha", "parked");
    add(&store, "beta", "running");
    store.connection().execute_batch("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('relay','survey','survey','relay','open',0);
        INSERT INTO crew_parks(id,root,crew_job_id,child_job_id,task_id,ship,next,state,created_ms,updated_ms) VALUES
        ('alpha','alpha','alpha','beta','survey','survey','old-park-canary','resumed',0,900),
        ('beta','alpha','alpha','beta','relay','survey','wrong-task-canary','parked',0,800);").unwrap();
    let row = scan(
        &store,
        "survey",
        &c,
        false,
        0,
        i64::MAX,
        Some("parked"),
        None,
        true,
        1000,
    )
    .unwrap()
    .jobs
    .remove(0)
    .1;
    let wait = row.wait.unwrap();
    assert!(wait.job.is_none());
    assert!(wait.next.is_none());
    assert!(wait.since.at_ms.is_none());
    store
        .connection()
        .execute("UPDATE crew_parks SET state='parked' WHERE id='alpha'", [])
        .unwrap();
    let row = scan(
        &store,
        "survey",
        &c,
        false,
        0,
        i64::MAX,
        Some("parked"),
        None,
        false,
        1000,
    )
    .unwrap()
    .jobs
    .remove(0)
    .1;
    assert_eq!(row.wait.unwrap().job.unwrap().id, "beta");
}

#[test]
fn realistic_home_batches_progress_and_resumes_within_observation_budget() {
    let (_dir, store, c) = setup();
    let tx = store.connection().unchecked_transaction().unwrap();
    for n in 0..400 {
        add(&store, &format!("alpha-{n}"), "running");
    }
    store.connection().execute("UPDATE jobs SET record=json_set(record,'$.resume_of','alpha-0') WHERE id IN ('alpha-398','alpha-399')", []).unwrap();
    for n in 0..8000 {
        store.connection().execute("INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms) VALUES(?1,'survey','survey','notice','progress',?2,'normal',?3)", params![format!("beta-{n}"), json!({"job_id":format!("alpha-{}", n % 400),"incarnation":"alpha","report":"progress"}).to_string(), n]).unwrap();
    }
    // A newer report for a different incarnation must not win the batch join.
    store.connection().execute("INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms) VALUES('gamma','survey','survey','notice','wrong-incarnation',?1,'normal',9000)", [json!({"job_id":"alpha-0","incarnation":"beta","report":"progress"}).to_string()]).unwrap();
    tx.commit().unwrap();
    let mut wire = c.gateway();
    wire["scopes"] = json!(["fleet:read", "mail:read"]);
    let c = DelegatedContext::from_gateway(&wire).unwrap();
    let started = Instant::now();
    let rows = scan(
        &store,
        "survey",
        &c,
        false,
        0,
        i64::MAX,
        None,
        None,
        true,
        10000,
    )
    .unwrap();
    let elapsed = started.elapsed();
    assert!(
        rows.jobs.len() >= 25,
        "page exhausted its budget at {} rows",
        rows.jobs.len()
    );
    assert!(
        elapsed < Duration::from_millis(250),
        "batched scan took {elapsed:?}"
    );
    let first = &rows.jobs[0].1;
    assert_eq!(
        first.progress.as_ref().unwrap().id.as_deref(),
        Some("beta-7600")
    );
    assert_eq!(
        first.progress.as_ref().unwrap().text.as_deref(),
        Some("progress")
    );
    assert_eq!(first.resumed_as.as_ref().unwrap().id, "alpha-399");
}
