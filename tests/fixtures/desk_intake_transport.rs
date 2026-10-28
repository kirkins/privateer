// Real one-way gateway subprocesses; no reverse link is installed.
fn intake_enroll(a: &Machine, owner: &str) {
    proposal(a, owner, &["alpha", "beta"]);
    a.accept();
    a.status();
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
    intake_db(m).prepare("SELECT id FROM desk_intake_fact ORDER BY id").unwrap()
        .query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
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
                    // Fail optional negotiation state independently of mail.
                    intake_db(source).execute_batch("CREATE TRIGGER intake_fail BEFORE INSERT ON desk_intake_transport BEGIN SELECT RAISE(ABORT,'negotiation unavailable'); END;").unwrap();
                }
                "snapshot" => intake_home(source).execute_batch("CREATE TRIGGER intake_fail BEFORE INSERT ON mail_source_snapshot BEGIN SELECT RAISE(ABORT,'snapshot unavailable'); END;").unwrap(),
                "checkpoint" => intake_db(source).execute_batch("CREATE TRIGGER intake_fail BEFORE INSERT ON desk_source_barrier BEGIN SELECT RAISE(ABORT,'checkpoint unavailable'); END;").unwrap(),
                "page" => {
                    // Capture a source event, then corrupt its optional page
                    // content. Ordinary mail rows and routing are untouched.
                    intake_notice(source,"page-fact");
                    intake_until(&a,desk,"page-fact");
                    intake_db(source).execute_batch("UPDATE desk_source_event SET event='invalid-json'; UPDATE desk_intake_transport SET after_seq=0,pending=NULL,receipted=0;").unwrap();
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
                "page" => "expected value",
                "receipt" => "receipt unavailable",
                _ => unreachable!(),
            };
            let matching = |m: &Machine| intake_db(m).query_row("SELECT COUNT(*) FROM desk_intake_audit WHERE instr(body,?1)>0",[needle],|r|r.get::<_,i64>(0)).unwrap();
            let audit_bodies = |m: &Machine| intake_db(m).prepare("SELECT body FROM desk_intake_audit").unwrap().query_map([],|r|r.get::<_,String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
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
        fs::write(&proxy,format!("#!{}\nimport json,subprocess,sys\nr=json.loads(sys.stdin.readline())\nr.get('args',{{}}).pop('desk_intake',None)\np=subprocess.run([{}]+sys.argv[1:],input=json.dumps(r)+'\\n',text=True,capture_output=True)\nfor line in p.stdout.splitlines():\n v=json.loads(line)\n if isinstance(v.get('result'),dict): v['result'].pop('desk_intake',None)\n print(json.dumps(v))\nsys.stderr.write(p.stderr)\nsys.exit(p.returncode)\n",support::python().display(),json!(support::BIN))).unwrap();
        fs::set_permissions(&proxy,fs::Permissions::from_mode(0o755)).unwrap();
        intake_db(&a).execute("UPDATE machines SET peer=json_set(peer,'$.executable',?1) WHERE name='beta'",[proxy.to_str().unwrap()]).unwrap();
        b.queued_mail("legacy-to-alpha",a.ship);
        a.collect();
        a.assert_mail("legacy-to-alpha");
        assert!(intake_audits(&a) > 0);
        let gap: String = intake_db(&a).query_row("SELECT state FROM desk_intake_gap",[],|r|r.get(0)).unwrap();
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
    a.collect();
    assert_eq!(intake_fact_ids(&a),["replay-after-restart"]);
    let before: i64 = intake_db(&a).query_row("SELECT received_at FROM desk_intake_checkpoint",[],|r| r.get(0)).unwrap();
    intake_db(&a).execute_batch("DROP TRIGGER receipt_fail;").unwrap();
    a.collect();
    assert_eq!(intake_fact_ids(&a),["replay-after-restart"]);
    let after: i64 = intake_db(&a).query_row("SELECT received_at FROM desk_intake_checkpoint",[],|r| r.get(0)).unwrap();
    assert_eq!(before,after,"a replay cannot refresh contact time");
    a.collect();
    assert!(intake_db(&b).query_row("SELECT receipted FROM desk_intake_transport",[],|r| r.get::<_,bool>(0)).unwrap()); // quiet contact carries no already-receipted page
}

#[test]
fn intake_steady_state_is_quiet_for_two_hundred_real_contacts_and_advances_past_barrier_cap() {
    for owner in ["alpha","beta"] {
        let (_t,a,b) = pair();
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
        for n in 0..45 {
            let id = format!("advancing-{n}");
            intake_notice(source,&id);
            intake_until(&a,desk,&id);
        }
        assert!(barriers()>32);
        assert!(intake_db(source).query_row("SELECT COUNT(*) FROM desk_source_barrier",[],|r|r.get::<_,i64>(0)).unwrap()<=2);
        assert_eq!(intake_db(desk).query_row("SELECT COUNT(*) FROM desk_intake_contact",[],|r|r.get::<_,i64>(0)).unwrap(),1);
        assert_eq!(intake_audits(&a)+intake_audits(&b),audits);
    }
}

#[test]
fn intake_quiet_push_uses_periodic_empty_barriers_across_gateway_restarts() {
    let (_t,a,b) = pair();
    intake_enroll(&a,"beta");
    intake_notice(&a,"initial-push");
    intake_until(&a,&b,"initial-push");
    for _ in 0..3 { a.collect(); }
    let first: String = intake_db(&b).query_row("SELECT barrier FROM desk_intake_checkpoint",[],|r|r.get(0)).unwrap();
    a.collect(); // restart alone must not issue an empty barrier
    assert_eq!(intake_db(&b).query_row("SELECT barrier FROM desk_intake_checkpoint",[],|r|r.get::<_,String>(0)).unwrap(),first);
    intake_db(&a).execute("UPDATE desk_source_head SET issued_at=0",[]).unwrap();
    let out = a.run(&["machine.collect","--json","{}"]);
    assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    assert!(out.stderr.is_empty(),"{}",String::from_utf8_lossy(&out.stderr));
    let (second,upper):(String,i64) = intake_db(&b).query_row("SELECT barrier,upper_seq FROM desk_intake_checkpoint",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_ne!(first,second);
    let raw: String = intake_db(&a).query_row("SELECT pending FROM desk_intake_transport",[],|r|r.get(0)).unwrap();
    let p: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(p["events"],json!([]));
    assert_eq!(p["through"],upper);
    assert_eq!(p["after"],upper);
    a.collect();
    assert_eq!(intake_db(&b).query_row("SELECT barrier FROM desk_intake_checkpoint",[],|r|r.get::<_,String>(0)).unwrap(),second);
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
