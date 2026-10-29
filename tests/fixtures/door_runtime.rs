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
    // Initialization creates a root task; exclude it from the exact backlog fixture.
    conn.execute("UPDATE tasks SET status='done'", []).unwrap();
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
    assert_eq!(legacy.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>(),
        ["mail_delivery", "provenance", "ships"]);
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
    assert_eq!(idle["observed_totals"]["active_workers"]["value"], 0);
    assert_eq!(idle["observed_totals"]["active_workers"]["complete"], true);
    assert_eq!(idle["coverage"]["scope"], "runtime-domain");
    assert_eq!(idle["coverage"]["complete"], false);
    let workers = idle["coverage"]["sources"].as_array().unwrap().iter()
        .find(|s| s["facet"] == "workers").unwrap();
    assert_eq!(workers["state"], "covered", "V3 supplies a bounded worker census");
    assert_eq!(workers["reason"], "identity_checked_census");
    let selected = delegated_ok(&fleet, &token, "ship_runtime", json!({"ship":"survey","machine":"local"}));
    assert_eq!(selected["schema"], "runtime.v1");
    assert_eq!(selected["selection"], json!({"ship":"survey","machine":"local"}));
    assert_eq!(selected["ships"].as_array().unwrap().len(), 1);
    assert_eq!(selected["ships"][0]["working"], false);
    assert_eq!(selected["ships"][0]["open_tasks"], 40);
    assert_eq!(selected["observed_totals"]["active_workers"]["value"], 0);
    assert_eq!(selected["observed_totals"]["active_workers"]["complete"], true);
    assert_eq!(selected["observed_totals"]["scope"], "selected");
    assert_eq!(selected["coverage"]["complete"], false);
    assert_eq!(selected["writes"], "none");
    assert_eq!(selected["cache_writes"], "bounded_process_memory_only");
    for canary in ["relay", "lookout", "beta", "argv-canary", "cwd-canary"] {
        assert!(!selected.to_string().contains(canary), "{canary}: {selected}");
    }
    for args in [json!({}), json!({"machine":"local"}), json!({"ship":null}),
        json!({"ship":"survey","schema":"runtime.v1"}), json!({"ship":"survey","include_text":true}),
        json!({"ship":"survey","machine":"/tmp/alpha"}), json!({"ship":"survey","command":"status"})] {
        credential_request(&fleet, &token, "ship_runtime", args, "unsupported_arguments");
    }
    let (_, mail_only) = delegated_issue(&fleet, "alpha", "mail:read", Some("survey"));
    credential_request(&fleet, &mail_only, "ship_runtime", json!({"ship":"survey"}), "insufficient_scope");
    let denied = delegated_exchange(&fleet, &token, "ship_runtime", json!({"ship":"relay"}));
    let absent = delegated_exchange(&fleet, &token, "ship_runtime", json!({"ship":"gamma"}));
    assert_eq!(denied["error"], absent["error"]);
    credential_request(&fleet, &all, "ship_runtime", json!({"ship":"survey","machine":"gamma"}), "ship_not_allowed");
    let visible = delegated_ok(&fleet, &token, "ships", json!({"schema":"runtime.v1"}));
    assert_eq!(visible["ships"].as_array().unwrap().len(), 1);
    for canary in ["relay", "lookout", "beta", "argv-canary", "cwd-canary"] {
        assert!(!visible.to_string().contains(canary), "{canary}: {visible}");
    }
    assert_eq!(fs::read(home.join("privateer.sqlite")).unwrap(), before_home);
    assert_eq!(fs::read(home.join("privateer.sqlite-wal")).ok(), before_wal, "no home, work, presence or mail writes");
    assert!(!registry.query_row("SELECT EXISTS(SELECT 1 FROM fleet_snapshots)", [], |r| r.get::<_, bool>(0)).unwrap(), "no runtime cache bookkeeping in V1");
    for args in [json!({"schema":"runtime.v2"}), json!({"ship":"survey"}), json!({"schema":"runtime.v1","cwd":"cwd-canary"}), json!({"schema":"runtime.v1","machine":"/tmp/alpha"})] {
        credential_request(&fleet, &token, "status", args, "unsupported_arguments");
    }
    let denied = delegated_exchange(&fleet, &token, "status", json!({"schema":"runtime.v1","ship":"relay"}));
    let absent = delegated_exchange(&fleet, &token, "status", json!({"schema":"runtime.v1","ship":"gamma"}));
    assert_eq!(denied["error"], absent["error"]);
    let remote = delegated_ok(&fleet, &all, "status", json!({"schema":"runtime.v1","machine":"beta"}));
    assert_eq!(remote["ships"][0]["name"], "lookout");
    assert_eq!(remote["ships"][0]["working"], "unknown");
    assert_eq!(remote["ships"][0]["dispatcher"], Value::Null);
    assert_eq!(remote["observed_totals"]["recorded_running_jobs"]["complete"], false);
    // Turn custody precedes job creation: none of these records may look idle.
    for status in ["offered", "running"] {
        conn.execute("INSERT INTO wakes(id,ship,focus,through_seq,mode,status,created_ms) VALUES('survey','survey','task:survey',0,'headless',?1,0)", [status]).unwrap();
        let launching = delegated_ok(&fleet, &token, "status", args.clone());
        assert_eq!(launching["ships"][0]["recorded_running_jobs"], 0);
        assert_eq!(launching["ships"][0]["activity_evidence"]["no_nonterminal_jobs"], true);
        assert_eq!(launching["ships"][0]["activity_evidence"]["no_launching_turns"], false);
        assert_eq!(launching["ships"][0]["working"], "unknown", "{status} wake without a job");
        conn.execute("DELETE FROM wakes", []).unwrap();
    }
    conn.execute("INSERT INTO errands(id,ship,focus,task_id,brief,mode,status,created_ms) VALUES('survey','survey','task:survey','survey-0','fixture','headless','running',0)", []).unwrap();
    let errand = delegated_ok(&fleet, &token, "status", args.clone());
    assert_eq!(errand["ships"][0]["recorded_running_jobs"], 0);
    assert_eq!(errand["ships"][0]["activity_evidence"]["no_launching_turns"], false);
    assert_eq!(errand["ships"][0]["working"], "unknown", "running errand without a job");
    conn.execute("DELETE FROM errands", []).unwrap();
    conn.execute("INSERT INTO hosted_panes(ship,pid,started,executable,recorded_ms) VALUES('survey',1,'fixture','fixture',0)", []).unwrap();
    let pane = delegated_ok(&fleet, &token, "status", args.clone());
    assert_eq!(pane["ships"][0]["attended"], false, "no presence lease");
    assert_eq!(pane["ships"][0]["activity_evidence"]["no_sessions_or_leases"], false);
    assert_eq!(pane["ships"][0]["working"], "unknown", "recorded pane without census");
    conn.execute("DELETE FROM hosted_panes", []).unwrap();
    let cleared = delegated_ok(&fleet, &token, "status", args.clone());
    assert_eq!(cleared["ships"][0]["working"], false, "negative evidence restored");
    // A hosted lease is not a working-turn observation, even if idle or asleep.
    conn.execute("INSERT INTO presence(ship,holder,started_ms,expires_ms) VALUES('survey','host:lookout',0,9223372036854775807)", []).unwrap();
    let hosted = delegated_ok(&fleet, &token, "status", args.clone());
    assert_eq!(hosted["ships"][0]["working"], "unknown");
    assert_eq!(hosted["ships"][0]["working_reason"], "session_unverified");
    conn.execute("DELETE FROM presence", []).unwrap();
    conn.execute("INSERT INTO jobs(id,record) VALUES('survey',?1)", [json!({"id":"survey","task_id":"survey-0","status":"running","heavy":false,"command":["argv-canary"],"cwd":"cwd-canary","plugin_binding":{"secret":"binding-canary"}}).to_string()]).unwrap();
    let active = delegated_ok(&fleet, &token, "ship_runtime", json!({"ship":"survey"}));
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
    let stopped = delegated_ok(&fleet, &token, "status", args.clone());
    assert_eq!(stopped["ships"][0]["dispatcher"], false);
    assert_eq!(stopped["ships"][0]["working"], "unknown", "stopped dispatcher is not complete idle evidence");
    fs::create_dir(home.join("serve.lock")).unwrap();
    let service = delegated_ok(&fleet, &token, "status", args.clone());
    assert_eq!(service["ships"][0]["dispatcher"], Value::Null);
    assert_eq!(service["ships"][0]["working"], "unknown");
    assert_eq!(service["ships"][0]["working_reason"], "service_unreadable");
    let legacy_ships = delegated_ok(&fleet, &token, "ships", json!({}));
    assert_eq!(legacy_ships["ships"][0]["dispatcher"], false, "legacy service errors retain false fallback");
    // Only the opt-in keeps a missing registered home as an unknown row.
    fs::rename(&home, home.with_extension("gamma")).unwrap();
    let missing = delegated_ok(&fleet, &token, "ship_runtime", json!({"ship":"survey"}));
    assert_eq!(missing["ships"][0]["working"], "unknown");
    assert_eq!(missing["ships"][0]["recorded_running_jobs"], Value::Null);
    assert_eq!(missing["ships"][0]["working_reason"], "home_unreadable");
    let missing_inventory = delegated_ok(&fleet, &token, "ships", json!({}));
    assert_eq!(missing_inventory["ships"], json!([]), "legacy excludes missing homes");
    assert!(!home.exists());
    stop(supervisor);
}

#[test]
fn ship_runtime_is_explicitly_unsupported_on_trusted_v1() {
    let fleet = Fleet::with_ships(&[("alpha", false)]);
    fleet.add();
    fs::write(fleet.root.join("adapter/door.py"), r#"import json, os, sys
log = open(os.environ['FAKE_DOOR_LOG'], 'a')
log.write(json.dumps({'hello':json.loads(sys.stdin.readline())})+'\n'); log.flush()
print(json.dumps({'type':'request','id':'alpha','command':'ship_runtime','args':{'ship':'alpha'}}),flush=True)
for line in sys.stdin:
 log.write(line); log.flush()
"#).unwrap();
    let supervisor = fleet.supervise();
    let reply = fleet.wait("trusted runtime refusal", |e| response(e, "alpha").cloned());
    assert_eq!(reply["ok"], false, "{reply}");
    assert_eq!(reply["error"], "ship_runtime requires scoped protocol v2");
    assert!(reply.get("result").is_none());
    stop(supervisor);
}

#[test]
fn runtime_legacy_ships_preserves_recipient_and_defaults() {
    use std::os::fd::AsRawFd;
    for scoped in [false, true] {
        // No liaison: the sole existing home is both the routing and inventory default.
        let fleet = Fleet::with_ships(&[("survey", false), ("relay", false)]);
        let token = if scoped {
            delegated_adapter(&fleet);
            Some(delegated_issue(&fleet, "lookout", "fleet:read", None).1)
        } else {
            fleet.add();
            None
        };
        let home = fleet.home("survey");
        fs::rename(fleet.home("relay"), fleet.home("relay").with_extension("missing")).unwrap();
        let conn = rusqlite::Connection::open(home.join("privateer.sqlite")).unwrap();
        conn.execute("UPDATE ships SET record=json_remove(record,'$.scope') WHERE name='survey'", []).unwrap();
        conn.execute("INSERT INTO presence(ship,holder,started_ms,expires_ms) VALUES('survey','fixture',0,9223372036854775807)", []).unwrap();
        let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(home.join("serve.lock")).unwrap();
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        let supervisor = fleet.supervise();
        fleet.wait("legacy compatibility hello", |e| e.first().cloned());
        let result = if let Some(token) = &token {
            delegated_ok(&fleet, token, "ships", json!({}))
        } else {
            let reply = fleet.wait("trusted ships", |e| response(e, "s2").cloned());
            assert_eq!(reply["ok"], true, "{reply}");
            reply["result"].clone()
        };
        assert_eq!(result["recipient"], "survey", "scoped={scoped}: {result}");
        assert_eq!(result["ships"], json!([{"name":"survey", "scope":"", "liaison":false,
            "attended":true, "dispatcher":true}]), "legacy defaults missing scope but keeps known attendance/service values");
        if let Some(token) = &token {
            // The cached legacy store still falls back on individual read failures.
            conn.execute_batch("DROP TABLE presence; UPDATE ships SET record='{' WHERE name='survey';").unwrap();
            drop(lock);
            fs::remove_file(home.join("serve.lock")).unwrap();
            fs::create_dir(home.join("serve.lock")).unwrap();
            let failed = delegated_ok(&fleet, token, "ships", json!({}));
            assert_eq!(failed["ships"], json!([{"name":"survey", "scope":"", "liaison":false,
                "attended":false, "dispatcher":false}]), "legacy read failures keep their old values");
        }
        stop(supervisor);
    }
}

#[test]
fn runtime_legacy_ships_preserves_symlink_store_open_errors() {
    for scoped in [false, true] {
        let fleet = Fleet::with_ships(&[("survey", false)]);
        let token = if scoped {
            delegated_adapter(&fleet);
            Some(delegated_issue(&fleet, "lookout", "fleet:read", None).1)
        } else {
            fleet.add();
            None
        };
        let home = fleet.home("survey");
        let alias = fleet.root.join("survey-link");
        std::os::unix::fs::symlink(home.parent().unwrap(), &alias).unwrap();
        let registry = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
        registry.execute("UPDATE ships SET home=?1 WHERE name='survey'", [alias.join(".privateer").to_str().unwrap()]).unwrap();
        let supervisor = fleet.supervise();
        fleet.wait("symlink legacy hello", |e| e.first().cloned());
        let reply = if let Some(token) = &token {
            delegated_exchange(&fleet, token, "ships", json!({}))
        } else {
            fleet.wait("trusted symlink error", |e| response(e, "s2").cloned())
        };
        assert_eq!(reply["ok"], false, "legacy Store::open rejected symlink ancestors before V1: {reply}");
        assert!(reply.get("result").is_none(), "no fabricated null-valued row");
        if !scoped {
            assert!(reply["error"].as_str().unwrap().contains("symlink path is not allowed"));
        }
        stop(supervisor);
    }
}
