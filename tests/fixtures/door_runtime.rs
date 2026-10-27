// Local-only V1 through the real v2 supervisor, with exact negative evidence.
#[test]
fn runtime_v1_projection_selectors_errors_and_legacy_compatibility() {
    use std::os::fd::AsRawFd;
    let fleet = Fleet::with_ships(&[("survey", true), ("relay", false)]);
    delegated_adapter(&fleet);
    let (_, token) = delegated_issue(&fleet, "lookout", "fleet:read", Some("survey"));
    let (_, all) = delegated_issue(&fleet, "relay", "fleet:read", None);
    let home = fleet.home("survey");
    let conn = rusqlite::Connection::open(home.join("privateer.sqlite")).unwrap();
    for n in 0..40 {
        conn.execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES(?1,'survey','survey','backlog','open',0)", [format!("survey-{n}")]).unwrap();
    }
    fs::create_dir_all(fleet.root.join("ledger")).unwrap();
    let ledger = rusqlite::Connection::open(fleet.root.join("ledger/resources.sqlite")).unwrap();
    ledger.execute_batch("CREATE TABLE IF NOT EXISTS reservations(key TEXT PRIMARY KEY,record TEXT NOT NULL); CREATE TABLE IF NOT EXISTS queue(key TEXT PRIMARY KEY,record TEXT NOT NULL);").unwrap();
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(home.join("serve.lock")).unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    let registry = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    registry.execute("INSERT INTO machine_ships(machine,name,scope) VALUES('beta','lookout','argv-canary cwd-canary')", []).unwrap();
    let supervisor = fleet.supervise();
    fleet.wait("scoped hello", |e| e.first().cloned());
    let legacy = delegated_ok(&fleet, &token, "status", json!({}));
    assert_eq!(legacy.as_object().unwrap().len(), 2);
    assert!(legacy.get("schema").is_none());
    assert!(legacy.get("coverage").is_none());
    assert_eq!(legacy["ships"][0]["working"], false);
    let before_home = fs::read(home.join("privateer.sqlite")).unwrap();
    let before_wal = fs::read(home.join("privateer.sqlite-wal")).ok();
    let args = json!({"schema":"runtime.v1","ship":"survey","machine":"local"});
    let idle = delegated_ok(&fleet, &token, "status", args.clone());
    assert_eq!(idle["ships"][0]["working"], false, "{idle}");
    assert_eq!(idle["ships"][0]["open_tasks"], 40);
    assert_eq!(idle["ships"][0]["activity_evidence"]["ledger_clear"], true);
    assert!(idle["ships"][0]["activity_evidence"]["observed_ms"].is_i64());
    assert_eq!(idle["observed_totals"]["active_workers"]["value"], Value::Null);
    assert_eq!(idle["coverage"]["scope"], "runtime-domain");
    assert_eq!(idle["coverage"]["complete"], false);
    let visible = delegated_ok(&fleet, &token, "ships", json!({"schema":"runtime.v1"}));
    assert_eq!(visible["ships"].as_array().unwrap().len(), 1);
    for canary in ["relay", "lookout", "beta", "argv-canary", "cwd-canary"] {
        assert!(!visible.to_string().contains(canary), "{canary}: {visible}");
    }
    assert_eq!(fs::read(home.join("privateer.sqlite")).unwrap(), before_home);
    assert_eq!(fs::read(home.join("privateer.sqlite-wal")).ok(), before_wal, "no home, work, presence or mail writes");
    assert!(!registry.query_row("SELECT EXISTS(SELECT 1 FROM fleet_snapshots)", [], |r| r.get::<_, bool>(0)).unwrap(), "no runtime cache bookkeeping in V1");
    for args in [json!({"schema":"runtime.v2"}), json!({"ship":"survey"}), json!({"schema":"runtime.v1","cwd":"cwd-canary"}), json!({"schema":"runtime.v1","machine":"/tmp/alpha"})] {
        assert_eq!(delegated_exchange(&fleet, &token, "status", args)["error"]["code"], "unsupported_arguments");
    }
    let denied = delegated_exchange(&fleet, &token, "status", json!({"schema":"runtime.v1","ship":"relay"}));
    let absent = delegated_exchange(&fleet, &token, "status", json!({"schema":"runtime.v1","ship":"gamma"}));
    assert_eq!(denied["error"], absent["error"]);
    let remote = delegated_ok(&fleet, &all, "status", json!({"schema":"runtime.v1","machine":"beta"}));
    assert_eq!(remote["ships"][0]["name"], "lookout");
    assert_eq!(remote["ships"][0]["working"], "unknown");
    assert_eq!(remote["ships"][0]["dispatcher"], Value::Null);
    assert_eq!(remote["observed_totals"]["recorded_running_jobs"]["complete"], false);
    // A hosted lease is not a working-turn observation, even if idle or asleep.
    conn.execute("INSERT INTO presence(ship,holder,started_ms,expires_ms) VALUES('survey','host:lookout',0,9223372036854775807)", []).unwrap();
    let hosted = delegated_ok(&fleet, &token, "status", args.clone());
    assert_eq!(hosted["ships"][0]["working"], "unknown");
    assert_eq!(hosted["ships"][0]["working_reason"], "census_deferred");
    conn.execute("DELETE FROM presence", []).unwrap();
    conn.execute("INSERT INTO jobs(id,record) VALUES('survey',?1)", [json!({"id":"survey","task_id":"survey-0","status":"running","heavy":false,"command":["argv-canary"],"cwd":"cwd-canary","plugin_binding":{"secret":"binding-canary"}}).to_string()]).unwrap();
    let active = delegated_ok(&fleet, &token, "status", args.clone());
    assert_eq!(active["ships"][0]["recorded_running_jobs"], 1);
    assert_eq!(active["ships"][0]["working"], "unknown", "stored running light job is not liveness");
    for canary in ["argv-canary", "cwd-canary", "binding-canary"] { assert!(!active.to_string().contains(canary)); }
    conn.execute("DELETE FROM jobs", []).unwrap();
    ledger.execute_batch("DROP TABLE reservations").unwrap();
    let unreadable = delegated_ok(&fleet, &token, "status", args.clone());
    assert_eq!(unreadable["ships"][0]["working"], "unknown");
    assert_eq!(unreadable["ships"][0]["working_reason"], "ledger_unreadable");
    ledger.execute_batch("CREATE TABLE reservations(key TEXT PRIMARY KEY,record TEXT NOT NULL)").unwrap();
    ledger.execute("INSERT INTO reservations(key,record) VALUES('survey',?1)", [json!({"ship":"survey","command":["argv-canary"]}).to_string()]).unwrap();
    let held = delegated_ok(&fleet, &token, "status", args.clone());
    assert_eq!(held["ships"][0]["working"], "unknown");
    assert_eq!(held["ships"][0]["activity_evidence"]["ledger_clear"], false);
    ledger.execute("DELETE FROM reservations", []).unwrap();
    drop(ledger);
    fs::rename(fleet.root.join("ledger/resources.sqlite"), fleet.root.join("ledger/gamma.sqlite")).unwrap();
    let missing_ledger = delegated_ok(&fleet, &token, "status", args.clone());
    assert_eq!(missing_ledger["ships"][0]["working"], "unknown");
    assert_eq!(missing_ledger["ships"][0]["activity_evidence"]["ledger_clear"], Value::Null);
    assert!(!fleet.root.join("ledger/resources.sqlite").exists());
    fs::rename(fleet.root.join("ledger/gamma.sqlite"), fleet.root.join("ledger/resources.sqlite")).unwrap();
    drop(lock);
    fs::remove_file(home.join("serve.lock")).unwrap();
    fs::create_dir(home.join("serve.lock")).unwrap();
    let service = delegated_ok(&fleet, &token, "status", args.clone());
    assert_eq!(service["ships"][0]["dispatcher"], Value::Null);
    assert_eq!(service["ships"][0]["working"], "unknown");
    assert_eq!(service["ships"][0]["working_reason"], "service_unreadable");
    let legacy_ships = delegated_ok(&fleet, &token, "ships", json!({}));
    assert_eq!(legacy_ships["ships"][0]["dispatcher"], Value::Null);
    // Missing registered home must remain an unknown row and must not be recreated.
    fs::rename(&home, home.with_extension("gamma")).unwrap();
    let missing = delegated_ok(&fleet, &token, "status", args);
    assert_eq!(missing["ships"][0]["working"], "unknown");
    assert_eq!(missing["ships"][0]["recorded_running_jobs"], Value::Null);
    assert_eq!(missing["ships"][0]["working_reason"], "home_unreadable");
    assert!(!home.exists());
    stop(supervisor);
}
