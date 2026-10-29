// Real one-way gateway subprocesses; no reverse link is installed.
fn intake_enroll(a: &Machine, owner: &str) {
    proposal(a, owner, &["alpha", "beta"]);
    a.accept();
    a.status();
    // These fixtures have no resident dispatcher. Run its real bounded idle
    // unit explicitly, with contacts disabled only for this maintenance pass.
    // machine.collect itself must never create a baseline. One bounded unit
    // can legitimately stop before publishing (reclaim passes, contention or
    // its own retry backoff), so verify each home's published readiness —
    // exactly what a real contact requires — and drive further units rather
    // than assuming the first one finished.
    let b = Machine { root: a.root.with_file_name("beta"), ship: "relay", intake_budget_ms: None };
    intake_until_published(&b);
    intake_until_published(a);
}
// Published readiness is the same marker a contact reads: a snapshot row with
// a non-negative upper. Nothing short of it counts as an enrolled home.
fn intake_published(m: &Machine) -> bool {
    intake_home(m).query_row("SELECT EXISTS(SELECT 1 FROM mail_source_snapshot WHERE only=1 AND upper_seq>=0)",[],|r| r.get(0)).unwrap()
}
fn intake_until_published(m: &Machine) {
    for _ in 0..10 {
        if intake_published(m) { return; }
        // An unpublished attempt schedules the production retry backoff.
        // Inject eligibility for the next bounded unit instead of sleeping
        // for wall time; deterministic lib tests cover that backoff itself.
        intake_db(m).execute("UPDATE desk_baseline_home SET attempted_ms=0", []).unwrap();
        intake_idle(m);
    }
    assert!(intake_published(m),"{}: enrollment never published its baseline",m.ship);
}
fn intake_idle(m: &Machine) {
    intake_db(m).execute("UPDATE machines SET next_contact_ms=?1", [i64::MAX]).unwrap();
    let out = m.run(&["serve","--json","{\"once\":true}"]);
    intake_db(m).execute("UPDATE machines SET next_contact_ms=0", []).unwrap();
    assert!(out.status.success(),"{} {}",String::from_utf8_lossy(&out.stdout),String::from_utf8_lossy(&out.stderr));
}
fn intake_db(m: &Machine) -> Connection {
    Connection::open(m.registry()).unwrap()
}
fn intake_home(m: &Machine) -> Connection {
    Connection::open(m.root.join(m.ship).join(".privateer/privateer.sqlite")).unwrap()
}
fn intake_audits(m: &Machine) -> i64 {
    intake_db(m).query_row("SELECT COUNT(*) FROM desk_intake_audit",[],|r|r.get(0)).unwrap()
}
fn intake_fact_ids(m: &Machine) -> Vec<String> {
    intake_db(m).prepare("SELECT id FROM desk_intake_fact WHERE json_extract(content,'$.payload.desk_placement') IS NULL ORDER BY id").unwrap()
        .query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
}
fn intake_external_source(m: &Machine) -> String {
    let row = m.row();
    let manifest = &row["proposal"]["manifest"];
    manifest["sources"].as_array().unwrap().iter().find(|s| s["core"] != manifest["owner_core"]).unwrap()["core"].as_str().unwrap().to_owned()
}
fn intake_notice(m: &Machine, id: &str) {
    m.ok("mail.send",json!({"id":id,"from":m.ship,"to":"captain","kind":"notice","body":"Snapshot and delta fact"}));
}
fn intake_until(a: &Machine, desk: &Machine, id: &str) {
    for _ in 0..5 {
        a.collect();
        if intake_fact_ids(desk).contains(&id.to_string()) { return; }
    }
    panic!("{id} absent from intake: {:?}",intake_fact_ids(desk));
}

#[test]
fn intake_large_pending_baseline_is_never_captured_by_a_real_mail_contact() {
    for owner in ["alpha","beta"] {
        let (_t,a,b)=pair();
        let (desk,source)=if owner=="alpha" {(&a,&b)} else {(&b,&a)};
        proposal(&a,owner,&["alpha","beta"]); a.accept(); a.status();
        intake_home(source).execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<5000)
            INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,state)
            SELECT 'retained-'||x,CASE WHEN x>4700 THEN 'survey' ELSE 'captain' END,
            CASE WHEN x>4700 THEN 'captain' ELSE 'survey' END,CASE WHEN x>4850 THEN 'request' ELSE 'notice' END,
            'Retained survey report','{}','normal',1,CASE WHEN x>4850 THEN 'open' ELSE NULL END FROM n;").unwrap();
        // Model the worker's held serialization lock and live WAL snapshot
        // with real process boundaries, not a sleep/timing assumption. Neither
        // can be a lock that contacts or ordinary writers wait for.
        use std::os::fd::AsRawFd;
        let baseline_lock=fs::OpenOptions::new().create(true).truncate(false).write(true)
            .open(source.registry().with_extension("desk-baseline.lock")).unwrap();
        assert_eq!(unsafe {libc::flock(baseline_lock.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},0);
        let running_snapshot=intake_home(source);
        running_snapshot.execute_batch("BEGIN; SELECT COUNT(*) FROM mail;").unwrap();
        // Missing/unsupported provenance remains conservative here; the unit
        // fixture separately proves all 5000 explicitly pinned facts import.
        for n in 0..3 {
            let id=format!("mail-while-pending-{n}"); b.queued_mail(&id,a.ship);
            let started=std::time::Instant::now();
            a.collect(); a.assert_mail(&id);
            eprintln!("PENDING_BASELINE_CONTACT owner={owner} rows=5000 elapsed_ms={:.3}",started.elapsed().as_secs_f64()*1000.0);
            assert_eq!(intake_home(source).query_row("SELECT COUNT(*) FROM mail_source_snapshot",[],|r|r.get::<_,i64>(0)).unwrap(),0);
            assert_eq!(intake_home(source).query_row("SELECT COUNT(*) FROM mail_source_import",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        }
        assert!(!source.root.join(source.ship).join(".privateer/mail-baseline.sqlite").exists());
        running_snapshot.execute_batch("ROLLBACK").unwrap();
        drop(baseline_lock);
        // An idle unit can stop on zero-wait contention or its independent
        // setup/work budget. Retry bounded units, not a wall-clock deadline or
        // an assumption that a single pass must finish on a loaded runner.
        for _ in 0..10 {
            intake_idle(source);
            if intake_home(source).query_row("SELECT EXISTS(SELECT 1 FROM mail_source_snapshot)",[],|r|r.get::<_,bool>(0)).unwrap() { break; }
            // Inject eligibility for the next attempt rather than sleep for
            // wall time. Deterministic lib tests cover the production backoff.
            intake_db(source).execute("UPDATE desk_baseline_home SET attempted_ms=0",[]).unwrap();
        }
        let audits: Vec<String> = intake_db(source).prepare("SELECT body FROM desk_intake_audit ORDER BY seq").unwrap().query_map([],|r|r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        let inventory: Vec<String> = intake_db(source).prepare("SELECT home FROM ships UNION SELECT home FROM desk_home_inventory ORDER BY home").unwrap().query_map([],|r|r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        for body in &audits {
            let body: Value=serde_json::from_str(body).unwrap();
            if body["stage"]=="baseline" {
                let error=body["error"].as_str().unwrap();
                assert!(["backpressure","baseline_too_large","busy","locked","interrupted"].iter().any(|s|error.contains(s)),
                    "unexpected idle failure: {body}");
            }
        }
        assert_eq!(intake_home(source).query_row("SELECT COUNT(*) FROM mail_source_snapshot",[],|r|r.get::<_,i64>(0)).unwrap(),1,
            "{owner}: idle enrollment failed: audits={audits:?} inventory={inventory:?}");
        let baseline=Connection::open(source.root.join(source.ship).join(".privateer/mail-baseline.sqlite")).unwrap();
        assert_eq!(baseline.query_row("SELECT COUNT(*) FROM mail_source_import WHERE json_extract(event,'$.id') LIKE 'retained-%'",[],|r|r.get::<_,i64>(0)).unwrap(),5000);
        assert_eq!(desk.status()["coverage_complete"],false);
    }
}

#[test]
fn intake_real_collect_and_push_snapshot_delta_and_restart_custody() {
    for owner in ["alpha","beta"] {
        let (_t,a,b) = pair();
        let (desk,source) = if owner == "alpha" {(&a,&b)} else {(&b,&a)};
        intake_notice(source,"snapshot-notice");
        intake_enroll(&a,owner);
        assert!(intake_fact_ids(desk).is_empty());
        intake_until(&a,desk,"snapshot-notice");
        // Every CLI/gateway contact is a real process restart. A source page
        // remains durable until a matching desk receipt reaches its source.
        let pending: String = intake_db(source).query_row("SELECT pending FROM desk_intake_transport",[],|r| r.get(0)).unwrap();
        assert!(pending.contains("snapshot-notice"));
        intake_notice(source,"delta-notice");
        intake_until(&a,desk,"delta-notice");
        for id in ["snapshot-notice","delta-notice"] {
            assert_eq!(intake_fact_ids(desk).iter().filter(|s| *s == id).count(),1);
        }
        let row = desk.row();
        assert_eq!(row["state"],"active");
        assert_eq!(desk.status()["coverage_complete"],false);
        assert_eq!(intake_db(&b).query_row("SELECT role FROM machines WHERE name='alpha'",[],|r| r.get::<_,String>(0)).unwrap(),"collected");
        assert_eq!(intake_db(source).query_row("SELECT COUNT(*) FROM desk_intake_gap WHERE state='history_gap'",[],|r| r.get::<_,i64>(0)).unwrap(),1);
    }
}

#[test]
fn intake_failures_never_fail_or_reorder_bidirectional_ship_and_captain_mail() {
    for owner in ["alpha","beta"] {
        for stage in ["negotiation","snapshot","checkpoint","page","receipt"] {
            let (_t,a,b) = pair();
            intake_enroll(&a,owner);
            let (desk,source) = if owner == "alpha" {(&a,&b)} else {(&b,&a)};
            // Negotiate push before enrolling its home, but retain no source
            // facts yet. Collect sources are negotiated on the owner's request.
            if owner == "beta" { a.collect(); }
            match stage {
                "negotiation" => {
                    // Fail actual first-state admission, not a no-op upsert
                    // which quiet contacts now correctly avoid altogether.
                    intake_db(source).execute("DELETE FROM desk_intake_transport WHERE source=?1",[intake_external_source(source)]).unwrap();
                    intake_db(source).execute_batch("CREATE TRIGGER intake_fail BEFORE INSERT ON desk_intake_transport BEGIN SELECT RAISE(ABORT,'negotiation unavailable'); END;").unwrap();
                }
                "snapshot" => {
                    intake_home(source).execute_batch("DELETE FROM mail_source_snapshot; CREATE TRIGGER intake_fail BEFORE INSERT ON mail_source_snapshot BEGIN SELECT RAISE(ABORT,'snapshot unavailable'); END;").unwrap();
                    intake_idle(source); // failure is now off the mail contact
                },
                "checkpoint" => intake_db(source).execute_batch("CREATE TRIGGER intake_fail BEFORE INSERT ON desk_source_barrier BEGIN SELECT RAISE(ABORT,'checkpoint unavailable'); END;").unwrap(),
                "page" => {
                    // Capture a source event, then corrupt its optional page
                    // schema (valid JSON for the expression index). Ordinary
                    // mail rows and routing are untouched.
                    intake_notice(source,"page-fact");
                    intake_until(&a,desk,"page-fact");
                    intake_db(source).execute_batch("UPDATE desk_source_event SET event='{}'; UPDATE desk_intake_transport SET after_seq=0,pending=NULL,receipted=0;").unwrap();
                    // Do not let the old transport ack fail first on the
                    // collect direction: this case must reach page decoding.
                    intake_db(desk).execute("UPDATE desk_intake_transport SET pending=NULL",[]).unwrap();
                }
                "receipt" => intake_db(desk).execute_batch("CREATE TRIGGER intake_fail BEFORE INSERT ON desk_intake_contact BEGIN SELECT RAISE(ABORT,'receipt unavailable'); END;").unwrap(),
                _ => unreachable!(),
            }
            // The two ship directions use the existing ordinary mail path;
            // captain mail follows the agreed desk in both placement variants.
            // Suppress mail.send's eager flush without changing the route,
            // so both directions are pending when the failing contact runs.
            intake_home(&a).execute("INSERT OR REPLACE INTO mail_delivery_retry SELECT json_extract(record,'$.location.peer'),1,?1,0 FROM ships WHERE name=?2",rusqlite::params![i64::MAX,b.ship]).unwrap();
            a.queued_mail("a-to-b",b.ship);
            a.queued_mail("a-to-b-2",b.ship);
            intake_home(&a).execute("DELETE FROM mail_delivery_retry",[]).unwrap();
            b.queued_mail("b-to-a",a.ship);
            b.queued_mail("b-to-a-2",a.ship);
            intake_notice(source,"captain-through-failure");
            // The scheduled contact also exercises the real captain push;
            // explicit machine.collect itself only collects legacy mail.
            intake_db(&a).execute("UPDATE machines SET next_contact_ms=0",[]).unwrap();
            let pass = a.run(&["serve","--json","{\"once\":true}"]);
            assert!(pass.status.success(),"{}",String::from_utf8_lossy(&pass.stderr));
            a.assert_mail("b-to-a");
            // alpha -> beta ship mail is flushed by the existing mail path.
            a.ok("mail.flush",json!({}));
            b.assert_mail("a-to-b");
            for (receiver,first,second) in [(&a,"b-to-a","b-to-a-2"),(&b,"a-to-b","a-to-b-2")] {
                let first = receiver.ok("mail.show",json!({"id":first}));
                let second = receiver.ok("mail.show",json!({"id":second}));
                assert!(first["seq"].as_i64().unwrap() < second["seq"].as_i64().unwrap(),"{owner}/{stage}: reordered mail");
            }
            desk.assert_mail("captain-through-failure");
            let audits = intake_audits(&a) + intake_audits(&b);
            assert!(audits > 0,"{owner}/{stage} was not audited");
            let needle = match stage {
                "negotiation" => "negotiation unavailable",
                "snapshot" => "snapshot unavailable",
                "checkpoint" => "checkpoint unavailable",
                "page" => "missing field",
                "receipt" => "receipt unavailable",
                _ => unreachable!(),
            };
            let matching = |m: &Machine| intake_db(m).query_row("SELECT COUNT(*) FROM desk_intake_audit WHERE instr(body,?1)>0",[needle],|r|r.get::<_,i64>(0)).unwrap();
            let audit_bodies = |m: &Machine| intake_db(m).prepare("SELECT body FROM desk_intake_audit").unwrap().query_map([],|r|r.get::<_,String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
            // Under CI load the per-contact intake time budget can stop a
            // contact at an earlier stage with backpressure. Each scheduled
            // contact gets a fresh budget, so keep contacting until the
            // injected stage is reached.
            for _ in 0..20 {
                if matching(&a)+matching(&b) > 0 { break; }
                intake_db(&a).execute("UPDATE machines SET next_contact_ms=0",[]).unwrap();
                let pass = a.run(&["serve","--json","{\"once\":true}"]);
                assert!(pass.status.success(),"{}",String::from_utf8_lossy(&pass.stderr));
            }
            assert!(matching(&a)+matching(&b)>0,"{owner}/{stage}: injected failure not reached: {:?} {:?}",audit_bodies(&a),audit_bodies(&b));
            assert_eq!(desk.status()["coverage_complete"],false);
        }
    }
}

#[test]
fn intake_malformed_and_missing_negotiation_does_not_stop_gateway_mail() {
    for owner in ["alpha","beta"] {
        let (_t,a,b) = pair();
        intake_enroll(&a,owner);
        b.queued_mail("ordinary",a.ship);
        for wire in [Value::Null,json!({"protocol":"legacy","complete":true})] {
            let answer = b.gateway("fleet.collect",json!({"machine":"alpha","desk_intake":wire}));
            assert_eq!(answer["ok"],true,"{answer}");
            assert!(answer["result"]["messages"].as_array().unwrap().iter().any(|m| m["id"]=="ordinary"));
            assert_ne!(answer["result"]["desk_intake"]["complete"],true);
        }
        a.collect();
        a.assert_mail("ordinary");
    }
}

#[test]
fn intake_legacy_peer_omits_frames_through_real_exchange_without_stopping_mail() {
    use std::os::unix::fs::PermissionsExt;
    for (owner,negotiated) in [("alpha",false),("beta",false),("alpha",true),("beta",true)] {
        let (t,a,b) = pair();
        intake_enroll(&a,owner);
        if negotiated {
            a.collect();
            intake_db(&a).execute("INSERT OR REPLACE INTO desk_intake_gap(domain,source,state) SELECT domain,source,'unreadable' FROM desk_intake_transport",[]).unwrap();
        }
        let proxy = t.path().join("legacy-gateway");
        fs::write(&proxy,format!("#!{}\nimport json,subprocess,sys\nr=json.loads(sys.stdin.readline())\nfor k in ['desk_intake','mail_copy_v1','mail_copies']: r.get('args',{{}}).pop(k,None)\np=subprocess.run([{}]+sys.argv[1:],input=json.dumps(r)+'\\n',text=True,capture_output=True)\nfor line in p.stdout.splitlines():\n v=json.loads(line)\n if isinstance(v.get('result'),dict):\n  for k in ['desk_intake','mail_copy_v1','mail_copies']: v['result'].pop(k,None)\n print(json.dumps(v))\nsys.stderr.write(p.stderr)\nsys.exit(p.returncode)\n",support::python().display(),json!(support::BIN))).unwrap();
        fs::set_permissions(&proxy,fs::Permissions::from_mode(0o755)).unwrap();
        intake_db(&a).execute("UPDATE machines SET peer=json_set(peer,'$.executable',?1) WHERE name='beta'",[proxy.to_str().unwrap()]).unwrap();
        b.queued_mail("legacy-to-alpha",a.ship);
        b.queued_mail("legacy-to-alpha-2",a.ship);
        intake_home(&a).execute("INSERT OR REPLACE INTO mail_delivery_retry SELECT json_extract(record,'$.location.peer'),1,?1,0 FROM ships WHERE name=?2",rusqlite::params![i64::MAX,b.ship]).unwrap();
        a.queued_mail("alpha-to-legacy",b.ship);
        a.queued_mail("alpha-to-legacy-2",b.ship);
        intake_home(&a).execute("DELETE FROM mail_delivery_retry",[]).unwrap();
        a.ok("mail.flush",json!({}));
        let (desk,source)=if owner=="alpha" {(&a,&b)} else {(&b,&a)};
        intake_notice(source,"legacy-captain-copy");
        a.collect();
        if owner=="beta" {
            intake_db(&a).execute("UPDATE machines SET next_contact_ms=0",[]).unwrap();
            let pass=a.run(&["serve","--json","{\"once\":true}"]);
            assert!(pass.status.success(),"{}",String::from_utf8_lossy(&pass.stderr));
        }
        desk.assert_mail("legacy-captain-copy");
        a.assert_mail("legacy-to-alpha");
        b.assert_mail("alpha-to-legacy");
        for (receiver,first,second) in [(&a,"legacy-to-alpha","legacy-to-alpha-2"),(&b,"alpha-to-legacy","alpha-to-legacy-2")] {
            let first=receiver.ok("mail.show",json!({"id":first}));
            let second=receiver.ok("mail.show",json!({"id":second}));
            assert!(first["seq"].as_i64().unwrap()<second["seq"].as_i64().unwrap());
        }
        assert_eq!(intake_home(desk).query_row("SELECT provenance FROM mail_source_origin WHERE id='legacy-captain-copy'",[],|r|r.get::<_,String>(0)).unwrap(),"\"Unresolved\"");
        assert!(intake_audits(&a) > 0);
        let gap: String = intake_db(&a).query_row("SELECT state FROM desk_intake_gap WHERE source=?1",[intake_external_source(&a)],|r|r.get(0)).unwrap();
        assert_eq!(gap,if negotiated {"unreadable"} else {"unsupported"});
    }
}

#[test]
fn intake_receipt_failure_replays_after_restart_without_duplicate_facts() {
    let (_t,a,b) = pair();
    intake_enroll(&a,"alpha");
    intake_notice(&b,"replay-after-restart");
    // Fail the separate transport receipt commit AFTER accept committed the
    // fact/checkpoint/contact transaction. The next gateway process retries.
    intake_db(&a).execute_batch("CREATE TRIGGER receipt_fail BEFORE UPDATE OF pending ON desk_intake_transport BEGIN SELECT RAISE(ABORT,'lost transport receipt'); END;").unwrap();
    // A successful collect only means the mail exchange finished: the 75 ms
    // optional-intake budget can legitimately stop that same contact before
    // intake acceptance. Drive bounded contacts until BOTH the accepted fact
    // and the injected lost-transport-receipt failure are durable, so the
    // replay below always starts from a genuinely accepted page.
    let audited = || intake_db(&a).query_row("SELECT COUNT(*) FROM desk_intake_audit WHERE instr(body,'lost transport receipt')>0",[],|r| r.get::<_,i64>(0)).unwrap() > 0;
    let mut fact_seen = false;
    for _ in 0..20 {
        a.collect();
        fact_seen = intake_fact_ids(&a).contains(&"replay-after-restart".to_string());
        if fact_seen && audited() { break; }
    }
    assert!(fact_seen,"accepted fact replay-after-restart never reached the desk: {:?}",intake_fact_ids(&a));
    assert!(audited(),"injected lost transport receipt failure was never observed");
    assert_eq!(intake_fact_ids(&a),["replay-after-restart"]);
    let before: i64 = intake_db(&a).query_row("SELECT received_at FROM desk_intake_checkpoint WHERE source=?1",[intake_external_source(&a)],|r| r.get(0)).unwrap();
    intake_db(&a).execute_batch("DROP TRIGGER receipt_fail;").unwrap();
    // The replaying contact saves the desk's transport acknowledgement, and
    // only a later contact carries it back so the source marks the page
    // receipted. Drive bounded contacts until both durable outcomes exist;
    // without both, a passing contact proves nothing about replay.
    let ack_saved = || intake_db(&a).query_row("SELECT json_extract(pending,'$.receipt') IS NOT NULL FROM desk_intake_transport WHERE source=?1",[intake_external_source(&a)],|r| r.get::<_,bool>(0)).unwrap();
    let source_receipted = || intake_db(&b).query_row("SELECT receipted FROM desk_intake_transport",[],|r| r.get::<_,bool>(0)).unwrap();
    for _ in 0..20 {
        a.collect();
        if ack_saved() && source_receipted() { break; }
    }
    assert!(ack_saved(),"the desk never saved the replayed page's acknowledgement");
    assert!(source_receipted(),"the source never receipted the replayed page");
    assert_eq!(intake_fact_ids(&a),["replay-after-restart"]);
    let after: i64 = intake_db(&a).query_row("SELECT received_at FROM desk_intake_checkpoint WHERE source=?1",[intake_external_source(&a)],|r| r.get(0)).unwrap();
    assert_eq!(before,after,"a replay cannot refresh contact time");
    a.collect();
    assert!(source_receipted()); // quiet contact carries no already-receipted page
}

#[test]
#[cfg_attr(
    not(feature = "test-intake-budget"),
    ignore = "needs --features test-intake-budget; CI runs --all-features"
)]
fn intake_steady_state_is_quiet_for_two_hundred_real_contacts_and_advances_past_barrier_cap() {
    for owner in ["alpha","beta"] {
        let (_t,a,b) = pair();
        // The property under test is what a quiet, in-sync contact does — no
        // audits, no barrier issuance — not how fast the runner is. The
        // wall-clock per-contact budget is production-fixed at 75 ms, which a
        // loaded CI machine can legitimately spend on a quiet contact's
        // subprocess work; widening it here (a test-feature-build override)
        // keeps the strict assertions meaningful without loosening the
        // product budget or accepting backpressure audits. a initiates every
        // contact, and the gateway subprocesses it spawns inherit the setting.
        let a = a.with_intake_budget(10_000);
        let (desk,source) = if owner == "alpha" {(&a,&b)} else {(&b,&a)};
        intake_enroll(&a,owner);
        intake_notice(source,"initial-fact");
        intake_until(&a,desk,"initial-fact");
        for _ in 0..3 { a.collect(); }
        let barriers = || intake_db(source).query_row("SELECT seq FROM sqlite_sequence WHERE name='desk_source_issuance'",[],|r|r.get::<_,i64>(0)).unwrap();
        let before = barriers();
        let audits = intake_audits(&a)+intake_audits(&b);
        for _ in 0..200 {
            let out = a.run(&["machine.collect","--json","{}"]);
            assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
            assert!(out.stderr.is_empty(),"{}",String::from_utf8_lossy(&out.stderr));
            assert_eq!(intake_audits(&a)+intake_audits(&b),audits);
            assert_eq!(barriers(),before);
        }
        let marks: Vec<i64> = [&a,&b].iter().map(|m| intake_db(m).query_row("SELECT COALESCE(MAX(rowid),0) FROM desk_intake_audit",[],|r|r.get(0)).unwrap()).collect();
        for n in 0..45 {
            let id = format!("advancing-{n}");
            intake_notice(source,&id);
            intake_until(&a,desk,&id);
        }
        assert!(barriers()>32);
        assert!(intake_db(source).query_row("SELECT COUNT(*) FROM desk_source_barrier",[],|r|r.get::<_,i64>(0)).unwrap()<=2);
        assert_eq!(intake_db(desk).query_row("SELECT COUNT(*) FROM desk_intake_contact WHERE source=?1",[intake_external_source(desk)],|r|r.get::<_,i64>(0)).unwrap(),1);
        // Even with widened headroom an advancing contact may only ever audit
        // backpressure-class failures; any other audit is a bug.
        for (m,mark) in [&a,&b].into_iter().zip(marks) {
            let bodies: Vec<String> = intake_db(m).prepare("SELECT body FROM desk_intake_audit WHERE rowid>?1").unwrap()
                .query_map([mark],|r|r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
            assert!(bodies.iter().all(|body| body.contains("backpressure")),"{bodies:?}");
        }
    }
}

#[test]
fn intake_busy_gateway_emits_a_recognized_partial_without_downgrading_or_stopping_mail() {
    use std::os::fd::AsRawFd;
    for (owner, busy_initiator) in [("alpha",false),("beta",false),("alpha",true),("beta",true)] {
        let (_t,a,b) = pair();
        intake_enroll(&a,owner);
        for _ in 0..3 { a.collect(); }
        intake_db(&a).execute("INSERT OR REPLACE INTO desk_intake_gap(domain,source,state) SELECT domain,source,'unreadable' FROM desk_intake_transport",[]).unwrap();
        let lock = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true)
            .open(if busy_initiator {a.registry()} else {b.registry()}.with_extension("desk-intake.lock")).unwrap();
        assert_eq!(unsafe {libc::flock(lock.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},0);
        b.queued_mail("through-busy-gateway",a.ship);
        let out = a.run(&["machine.collect","--json","{}"]);
        assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
        a.assert_mail("through-busy-gateway");
        let audit = |m:&Machine,needle:&str| intake_db(m).query_row("SELECT COUNT(*) FROM desk_intake_audit WHERE instr(body,?1)>0",[needle],|r|r.get::<_,i64>(0)).unwrap();
        for _ in 0..4 { a.collect(); }
        let (busy,other) = if busy_initiator {(&a,&b)} else {(&b,&a)};
        assert!(audit(busy,"intake exchange busy")>0,"must exercise the real busy emitter");
        assert!(audit(other,"backpressure: peer intake partial")>0,"partial must parse as partial");
        assert_eq!(intake_db(other).query_row("SELECT supported FROM desk_intake_transport",[],|r|r.get::<_,i64>(0)).unwrap(),1);
        assert_eq!(audit(&a,"unknown field"),0);
        assert_eq!(audit(&a,"unsupported: intake negotiation"),0);
        assert_eq!(intake_db(&a).query_row("SELECT supported FROM desk_intake_transport",[],|r|r.get::<_,i64>(0)).unwrap(),1);
        assert_eq!(intake_db(&a).query_row("SELECT state FROM desk_intake_gap",[],|r|r.get::<_,String>(0)).unwrap(),"unreadable");
        drop(lock);
        let (desk,source) = if owner=="alpha" {(&a,&b)} else {(&b,&a)};
        intake_notice(source,"after-busy");
        intake_until(&a,desk,"after-busy");
    }
}

#[test]
fn intake_quiet_push_uses_periodic_empty_barriers_across_gateway_restarts() {
    let (_t,a,b) = pair();
    intake_enroll(&a,"beta");
    intake_notice(&a,"initial-push");
    intake_until(&a,&b,"initial-push");
    for _ in 0..3 { a.collect(); }
    let first: String = intake_db(&b).query_row("SELECT barrier FROM desk_intake_checkpoint WHERE source=?1",[intake_external_source(&b)],|r|r.get(0)).unwrap();
    a.collect(); // restart alone must not issue an empty barrier
    assert_eq!(intake_db(&b).query_row("SELECT barrier FROM desk_intake_checkpoint WHERE source=?1",[intake_external_source(&b)],|r|r.get::<_,String>(0)).unwrap(),first);
    intake_db(&a).execute("UPDATE desk_source_head SET issued_at=0",[]).unwrap();
    // The reset barrier schedule is due on every following contact, but any
    // one contact can still stop inside its 75 ms optional-intake budget
    // before issuing. Drive bounded contacts until the empty barrier is
    // actually accepted; the issuing contact itself must stay quiet.
    let checkpoint_barrier = || intake_db(&b).query_row("SELECT barrier FROM desk_intake_checkpoint WHERE source=?1",[intake_external_source(&b)],|r| r.get::<_,String>(0)).unwrap();
    let mut issued = None;
    for _ in 0..20 {
        let out = a.run(&["machine.collect","--json","{}"]);
        assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
        if checkpoint_barrier() != first { issued = Some(out); break; }
    }
    let out = issued.unwrap_or_else(|| panic!("the periodic empty barrier was never issued: {first:?}"));
    assert!(out.stderr.is_empty(),"{}",String::from_utf8_lossy(&out.stderr));
    let (second,upper):(String,i64) = intake_db(&b).query_row("SELECT barrier,upper_seq FROM desk_intake_checkpoint WHERE source=?1",[intake_external_source(&b)],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_ne!(first,second);
    let raw: String = intake_db(&a).query_row("SELECT pending FROM desk_intake_transport",[],|r|r.get(0)).unwrap();
    let p: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(p["events"],json!([]));
    assert_eq!(p["through"],upper);
    assert_eq!(p["after"],upper);
    a.collect();
    assert_eq!(intake_db(&b).query_row("SELECT barrier FROM desk_intake_checkpoint WHERE source=?1",[intake_external_source(&b)],|r|r.get::<_,String>(0)).unwrap(),second);
    assert_eq!(b.status()["coverage_complete"],false);
}

#[test]
fn intake_small_link_pages_make_progress_with_mail_in_the_same_response() {
    let (_t,a,b) = pair();
    intake_enroll(&a,"alpha");
    intake_db(&a).execute("UPDATE machines SET peer=json_set(peer,'$.max_response_bytes',16384) WHERE name='beta'",[]).unwrap();
    for n in 0..80 {
        b.ok("mail.send",json!({"id":format!("bounded-{n}"),"from":"captain","to":b.ship,"kind":"notice","body":"x".repeat(300)}));
    }
    for n in 0..30 {
        let id = format!("ordinary-with-page-{n}");
        b.queued_mail(&id,a.ship);
        let out = a.run(&["machine.collect","--json","{}"]);
        assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
        a.assert_mail(&id);
        if intake_fact_ids(&a).iter().filter(|id| id.starts_with("bounded-")).count()==80 { break; }
    }
    assert_eq!(intake_fact_ids(&a).iter().filter(|id| id.starts_with("bounded-")).count(),80);
    assert_eq!(intake_db(&b).query_row("SELECT COUNT(*) FROM desk_intake_audit WHERE stage='envelope'",[],|r|r.get::<_,i64>(0)).unwrap(),0);
}

#[test]
fn intake_respects_small_advertised_response_limit_and_ordinary_mail_keeps_flowing() {
    let (_t,a,b) = pair();
    intake_enroll(&a,"alpha");
    // A #481 source-local captain send belongs in intake, not in the
    // ordinary remote mailbox. Keep it below mail's 64-KiB envelope limit.
    b.ok("mail.send",json!({"id":"large-intake","from":"captain","to":b.ship,"kind":"notice","body":"x".repeat(48*1024)}));
    intake_db(&a).execute("UPDATE machines SET peer=json_set(peer,'$.max_response_bytes',16384) WHERE name='beta'",[]).unwrap();
    for n in 0..4 {
        let id = format!("small-mail-{n}");
        b.queued_mail(&id,a.ship);
        let out = a.run(&["machine.collect","--json","{}"]);
        assert!(out.status.success(),"{} {}",String::from_utf8_lossy(&out.stdout),String::from_utf8_lossy(&out.stderr));
        let result: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(!result.to_string().contains("configured byte limit"),"{result}");
        a.assert_mail(&id);
    }
    assert!(!intake_fact_ids(&a).contains(&"large-intake".to_string()));
    assert_eq!(intake_db(&b).query_row("SELECT COUNT(*) FROM desk_intake_audit WHERE instr(body,'oversized fact')>0",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    // Raising only the existing link's byte limit lets the same durable page
    // converge. No page receipt was forged to make room for ordinary mail.
    intake_db(&a).execute("UPDATE machines SET peer=json_set(peer,'$.max_response_bytes',1048576) WHERE name='beta'",[]).unwrap();
    intake_until(&a,&a,"large-intake");
}

#[test]
fn intake_nonparticipant_contacts_are_silent_with_or_without_a_peer_identity_pin() {
    let (t,a,b) = pair();
    intake_enroll(&a,"alpha");
    let g = Machine::new(t.path().join("gamma"),"survey-gamma");
    g.link("gamma",&a,"alpha");
    for pin in [false,true] {
        if pin {
            intake_db(&a).execute("INSERT OR REPLACE INTO desk_placement_peer(machine,core,description) VALUES('gamma',?1,'{}')",["00000000-0000-4000-8000-00000000000c"]).unwrap();
        } else {
            intake_db(&a).execute("DELETE FROM desk_placement_peer WHERE machine='gamma'",[]).unwrap();
        }
        let before = intake_audits(&a);
        let out = g.run(&["machine.collect","--json","{}"]);
        assert!(out.status.success());
        assert!(out.stderr.is_empty(),"{}",String::from_utf8_lossy(&out.stderr));
        assert_eq!(intake_audits(&a),before);
    }
    assert_eq!(intake_audits(&b),0);
}
