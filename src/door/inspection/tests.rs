use super::*;

fn fixture() -> (tempfile::TempDir, Store, Connection, DelegatedContext) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.connection().execute_batch("INSERT INTO ships(name,record) VALUES('survey','{\"location\":\"here\"}'); INSERT INTO charters(id,data) VALUES('survey','{}'); INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('survey','survey','survey','survey','open',0);").unwrap();
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE ships(name TEXT,home TEXT,retired_ms INTEGER);")
        .unwrap();
    conn.execute(
        "INSERT INTO ships VALUES('survey',?1,NULL)",
        [dir.path().to_str().unwrap()],
    )
    .unwrap();
    let c=DelegatedContext::from_gateway(&json!({"version":1,"client":uuid::Uuid::new_v4().to_string(),"resource":"door:alpha","scopes":["fleet:read"],"ships":["survey"],"result_id":uuid::Uuid::new_v4().to_string()})).unwrap();
    for n in 0..55 {
        store.connection().execute("INSERT INTO jobs(id,record) VALUES(?1,?2)",rusqlite::params![format!("alpha-{n}"),json!({"id":format!("alpha-{n}"),"task_id":"survey","incarnation":"beta","status":"running","created_ms":0}).to_string()]).unwrap();
    }
    (dir, store, conn, c)
}
#[test]
fn fixed_schema_rejects_raw_rpc_paths_and_missing_or_forged_references() {
    for args in [
        json!({}),
        json!({"ship":"/alpha"}),
        json!({"ship":"survey","command":"job.list"}),
        json!({"ship":"survey","limit":51}),
        json!({"ship":"survey","limit":0}),
        json!({"ship":"survey","state":"blocked"}),
        json!({"ship":"survey","kind":"worker"}),
        json!({"ship":"survey","include_text":"yes"}),
        json!({"ship":"survey","continuation":"guessed"}),
    ] {
        assert!(validate_args(Command::ShipJobs, &args).is_err(), "{args}");
    }
    assert!(
        validate_args(
            Command::JobInspect,
            &json!({"ship":"survey","reference":"alpha"})
        )
        .is_err()
    );
    assert!(validate_args(Command::ShipWork, &json!({"ship":"survey","state":"open"})).is_err());
    for command in [Command::ShipJobs, Command::ShipWork] {
        assert!(validate_args(command, &json!({"ship":"survey","limit":50})).is_ok());
    }
}
#[test]
fn pages_recheck_scope_filters_revision_source_expiry_and_concurrent_finish() {
    let (_dir, store, conn, c) = fixture();
    let args = json!({"ship":"survey","limit":25});
    let first = local_read(&conn, Command::ShipJobs, &args, &c).unwrap();
    assert_eq!(first.jobs.len(), 25);
    assert_eq!(first.counts["running"], 55);
    assert!(first.coverage.counts_complete);
    assert!(first.more);
    assert_eq!(
        first.coverage.history,
        "retained_only_prior_history_unknown"
    );
    let mut next = args.clone();
    next["continuation"] = json!(first.continuation.as_ref().unwrap());
    store.connection().execute("UPDATE jobs SET record=json_set(record,'$.status','succeeded','$.finished_ms',100) WHERE id='alpha-25'",[]).unwrap();
    let second = local_read(&conn, Command::ShipJobs, &next, &c).unwrap();
    assert_eq!(second.jobs.len(), 25);
    assert_eq!(second.jobs[0].state, "succeeded");
    assert!(!second.coverage.counts_complete);
    assert_eq!(second.snapshot, first.snapshot);
    // Retention may remove the high-water tail between pages. A drained scan
    // must end, not fabricate another token from the former upper rowid.
    store
        .connection()
        .execute("DELETE FROM jobs WHERE rowid>50", [])
        .unwrap();
    let tail = local_read(
        &conn,
        Command::ShipJobs,
        &json!({"ship":"survey","limit":25,"continuation":second.continuation}),
        &c,
    )
    .unwrap();
    assert!(tail.jobs.is_empty());
    assert!(!tail.more);
    let mut changed = next.clone();
    changed["state"] = json!("running");
    assert!(local_read(&conn, Command::ShipJobs, &changed, &c).is_err());
    changed = next.clone();
    changed["limit"] = json!(50);
    assert!(local_read(&conn, Command::ShipJobs, &changed, &c).is_err());
    let mut wire = c.gateway();
    wire["visibility_revision"] = json!(2);
    let revised = DelegatedContext::from_gateway(&wire).unwrap();
    assert!(local_read(&conn, Command::ShipJobs, &next, &revised).is_err());
    wire = c.gateway();
    wire["ships"] = json!(["relay"]);
    let restricted = DelegatedContext::from_gateway(&wire).unwrap();
    assert!(local_read(&conn, Command::ShipJobs, &next, &restricted).is_err());
    wire = c.gateway();
    wire["scopes"] = json!(["mail:read"]);
    let nofleet = DelegatedContext::from_gateway(&wire).unwrap();
    assert!(local_read(&conn, Command::ShipJobs, &next, &nofleet).is_err());
    wire = c.gateway();
    wire["client"] = json!(uuid::Uuid::new_v4().to_string());
    let other = DelegatedContext::from_gateway(&wire).unwrap();
    assert!(local_read(&conn, Command::ShipJobs, &next, &other).is_err());
    let original: String = store
        .connection()
        .query_row(
            "SELECT value FROM metadata WHERE key='runtime_origin'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    store
        .connection()
        .execute(
            "UPDATE metadata SET value='gamma' WHERE key='runtime_origin'",
            [],
        )
        .unwrap();
    assert!(local_read(&conn, Command::ShipJobs, &next, &c).is_err());
    store
        .connection()
        .execute(
            "UPDATE metadata SET value=?1 WHERE key='runtime_origin'",
            [original],
        )
        .unwrap();
    let token = first.continuation.as_ref().unwrap();
    conn.execute(
        "UPDATE door_inspection_pages SET issued_ms=issued_ms-600001 WHERE token=?1",
        [token],
    )
    .unwrap();
    assert!(local_read(&conn, Command::ShipJobs, &next, &c).is_err());
}
#[test]
fn exact_details_refuse_changed_custody_and_hidden_and_guessed_identically() {
    let (_dir, store, conn, c) = fixture();
    let first = local_read(&conn, Command::ShipJobs, &json!({"ship":"survey"}), &c).unwrap();
    let mut args = json!({"ship":"survey","reference":first.jobs[0].reference});
    let detail = local_read(&conn, Command::JobInspect, &args, &c).unwrap();
    assert_eq!(detail.job.unwrap().state, "running");
    store
        .connection()
        .execute(
            "UPDATE jobs SET record=json_set(record,'$.incarnation','gamma') WHERE id='alpha-0'",
            [],
        )
        .unwrap();
    assert!(local_read(&conn, Command::JobInspect, &args, &c).is_err());
    args["reference"]["id"] = json!("guessed");
    let guessed = local_read(&conn, Command::JobInspect, &args, &c)
        .err()
        .unwrap()
        .to_string();
    args["reference"]["id"] = json!("alpha-0");
    store
        .connection()
        .execute("UPDATE tasks SET owner='relay'", [])
        .unwrap();
    let hidden = local_read(&conn, Command::JobInspect, &args, &c)
        .err()
        .unwrap()
        .to_string();
    assert_eq!(guessed, hidden);
}
#[test]
fn byte_bound_pages_do_not_skip_rows_and_tokens_are_bounded() {
    let (_dir, store, conn, c) = fixture();
    // Bounded progress text makes bytes, not the requested row limit, end
    // this page. Oversized authored prose is still bounded to 512 chars.
    for n in 0..55 {
        store.connection().execute("INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms) VALUES(?1,'survey','survey','notice',?2,?3,'normal',0)",rusqlite::params![format!("progress-{n}"),"x".repeat(600),json!({"job_id":format!("alpha-{n}"),"incarnation":"beta","report":"progress"}).to_string()]).unwrap();
    }
    let mut wire = c.gateway();
    wire["scopes"] = json!(["fleet:read", "mail:read"]);
    let c = DelegatedContext::from_gateway(&wire).unwrap();
    let mut args = json!({"ship":"survey","limit":50,"include_text":true});
    let mut seen = std::collections::BTreeSet::new();
    let mut byte_limited = false;
    loop {
        let page = local_read(&conn, Command::ShipJobs, &args, &c).unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() <= PAGE_BYTES);
        if page.more && page.jobs.len() < 50 {
            byte_limited = true;
        }
        for j in page.jobs {
            assert!(seen.insert(j.reference.id));
        }
        match page.continuation {
            Some(t) => args["continuation"] = json!(t),
            None => break,
        }
    }
    assert!(byte_limited);
    assert_eq!(seen.len(), 55);
    for _ in 0..40 {
        local_read(
            &conn,
            Command::ShipJobs,
            &json!({"ship":"survey","limit":1}),
            &c,
        )
        .unwrap();
    }
    assert!(
        conn.query_row(
            "SELECT COUNT(*) FROM door_inspection_pages WHERE grant_id=?1",
            [c.client()],
            |r| r.get::<_, u64>(0)
        )
        .unwrap()
            <= 32
    );
}
#[test]
fn large_authored_text_budget_resumes_without_skipping_interrupted_rows() {
    let (_dir, store, conn, c) = fixture();
    for n in 0..55 {
        store.connection().execute("INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms) VALUES(?1,'survey','survey','notice',?2,?3,'normal',0)",rusqlite::params![format!("progress-{n}"),"x".repeat(20_000),json!({"job_id":format!("alpha-{n}"),"incarnation":"beta","report":"progress"}).to_string()]).unwrap();
    }
    let mut wire = c.gateway();
    wire["scopes"] = json!(["fleet:read", "mail:read"]);
    let c = DelegatedContext::from_gateway(&wire).unwrap();
    let mut args = json!({"ship":"survey","limit":50,"include_text":true});
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let page = local_read(&conn, Command::ShipJobs, &args, &c).unwrap();
        for row in page.jobs {
            assert!(seen.insert(row.reference.id));
            assert_eq!(row.progress.unwrap().text.unwrap().len(), 512);
        }
        match page.continuation {
            Some(t) => args["continuation"] = json!(t),
            None => break,
        }
    }
    assert_eq!(seen.len(), 55);
}
#[test]
fn first_data_row_interrupt_does_not_issue_endless_empty_continuations() {
    let (_dir, store, conn, c) = fixture();
    // Force work-row decoding, not enrichment, over the deterministic SQLite
    // VM budget. A view avoids timing assumptions or test-only runtime hooks.
    store.connection().execute_batch(
        "INSERT INTO task_schedule(task_id,record) VALUES('survey','{}');
         ALTER TABLE task_schedule RENAME TO task_schedule_alpha;
         CREATE VIEW task_schedule AS SELECT task_id,
           (WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000)
            SELECT json_object('owner','survey','task_id','survey','state','ready','reason',sum(x)) FROM n) AS record
           FROM task_schedule_alpha;"
    ).unwrap();
    for _ in 0..2 {
        let error = local_read(&conn, Command::ShipWork, &json!({"ship":"survey"}), &c)
            .err()
            .unwrap();
        assert!(
            matches!(
                error.downcast_ref::<Denial>(),
                Some(Denial::MessageTooLarge)
            ),
            "{error:?}"
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM door_inspection_pages", [], |r| r
                .get::<_, u64>(0))
                .unwrap(),
            0
        );
    }
}

#[test]
fn enrichment_interrupt_still_issues_a_retry_at_the_same_cursor() {
    let (_dir, store, conn, c) = fixture();
    store.connection().execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000) INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms) SELECT 'beta-'||x,'survey','survey','notice','','{}','normal',x FROM n;").unwrap();
    let page = local_read(&conn, Command::ShipJobs, &json!({"ship":"survey"}), &c).unwrap();
    assert!(page.jobs.is_empty() && page.more && !page.coverage.complete);
    let token = page.continuation.unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT after_key FROM door_inspection_pages WHERE token=?1",
            [&token],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    store.connection().execute("DELETE FROM mail", []).unwrap();
    let retry = local_read(
        &conn,
        Command::ShipJobs,
        &json!({"ship":"survey","continuation":token}),
        &c,
    )
    .unwrap();
    assert_eq!(retry.jobs.len(), 25);
    assert_eq!(retry.jobs[0].reference.id, "alpha-0");
}

#[test]
fn restrict_restore_has_a_monotonic_visibility_generation() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE door_clients(id TEXT PRIMARY KEY,scopes TEXT,ships TEXT,paused INTEGER,revoked_ms INTEGER);INSERT INTO door_clients VALUES('alpha','[]',NULL,0,NULL);").unwrap();
    visibility_schema(&conn).unwrap();
    conn.execute_batch(
        "UPDATE door_clients SET ships='[\"survey\"]';UPDATE door_clients SET ships=NULL;",
    )
    .unwrap();
    assert_eq!(
        conn.query_row("SELECT revision FROM door_runtime_visibility", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        2
    );
    conn.execute("DELETE FROM door_clients", []).unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM door_runtime_visibility", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn text_cut_inside_a_mask_is_valid_and_pending_task_round_trips() {
    let (_dir, store, conn, c) = fixture();
    let mut wire = c.gateway();
    wire["scopes"] = json!(["fleet:read", "mail:read"]);
    let c = DelegatedContext::from_gateway(&wire).unwrap();
    for prefix in [498, 500] {
        let raw = format!("{} password=secret-canary tail", "x".repeat(prefix));
        let shown = view::safe_text(&raw);
        assert_eq!(shown.chars().count(), 512);
        assert!(!shown.contains("secret-canary"));
        if prefix == 498 {
            assert!(shown.ends_with("••• "));
            assert_ne!(
                shown,
                view::safe_text(&shown),
                "a cut after a mask leaves trailing whitespace that a second pass trims"
            );
        } else {
            assert!(
                shown.ends_with("password=••"),
                "exercise a cut inside masked output"
            );
        }
        store.connection().execute("INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms) VALUES(?1,'survey','survey','notice',?2,?3,'normal',500)", params![format!("gamma-{prefix}"), raw, json!({"job_id":"alpha-0","incarnation":"beta","report":"progress"}).to_string()]).unwrap();
        let page = local_read(
            &conn,
            Command::ShipJobs,
            &json!({"ship":"survey","include_text":true}),
            &c,
        )
        .unwrap();
        assert_eq!(
            page.jobs[0].progress.as_ref().unwrap().text.as_deref(),
            Some(shown.as_str())
        );
    }
    store
        .connection()
        .execute("UPDATE tasks SET status='pending'", [])
        .unwrap();
    let mut work = local_read(&conn, Command::ShipWork, &json!({"ship":"survey"}), &c).unwrap();
    work.work[0].reason.text = Some("token=receiver-canary".into());
    work.remask();
    assert!(
        !work.work[0]
            .reason
            .text
            .as_ref()
            .unwrap()
            .contains("receiver-canary")
    );
    work.validate("survey", &c, true).unwrap();
    assert_eq!(work.work[0].task_state, "pending");
    assert_eq!(work.counts["backlog"], 1);
    for bad in ["/absolute/path", "user@host", "escape\u{1b}[31m"] {
        let t = view::Text {
            category: "progress".into(),
            id: None,
            text: Some(bad.into()),
            withheld: false,
            at: view::Age {
                at_ms: None,
                age_ms: None,
                basis: "recorded_text".into(),
            },
        };
        assert!(validate_text(&t, true).is_err());
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn token_clock_is_boot_bound_across_gateway_processes() {
    const CHILD: &str = "PRIVATEER_INSPECTION_CLOCK_CHILD";
    if std::env::var_os(CHILD).is_some() {
        println!(
            "CLOCK:{}",
            serde_json::to_string(&token_clock().unwrap()).unwrap()
        );
        return;
    }
    let (boot, before) = token_clock().unwrap();
    #[cfg(target_os = "macos")]
    assert!(
        uuid::Uuid::parse_str(boot.strip_prefix("boot:").unwrap()).is_ok(),
        "boot identity must be kern.bootsessionuuid, not a wall-clock-derived stamp"
    );
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "door::inspection::tests::token_clock_is_boot_bound_across_gateway_processes",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    let stdout = String::from_utf8(child.stdout).unwrap();
    let line = stdout
        .lines()
        .find_map(|s| s.split_once("CLOCK:").map(|(_, value)| value))
        .expect(&stdout);
    let (child_boot, child_time): (String, i64) = serde_json::from_str(line).unwrap();
    let (after_boot, after) = token_clock().unwrap();
    assert_eq!(boot, child_boot);
    assert_eq!(boot, after_boot);
    assert!(before <= child_time && child_time <= after);
}
