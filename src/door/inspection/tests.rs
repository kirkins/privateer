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
        "UPDATE door_inspection_pages SET issued_ms=-600000 WHERE token=?1",
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
    // Maximal safe related references make bytes, not the requested row limit,
    // end this page. Long arbitrary prose must still be bounded to 512 chars.
    for n in 0..55 {
        store.connection().execute("INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms) VALUES(?1,'survey','survey','notice',?2,?3,'normal',0)",rusqlite::params![format!("progress-{n}"),"x".repeat(20_000),json!({"job_id":format!("alpha-{n}"),"incarnation":"beta","report":"progress"}).to_string()]).unwrap();
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
