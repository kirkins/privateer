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
                }
                "receipt" => intake_db(desk).execute_batch("CREATE TRIGGER intake_fail BEFORE INSERT ON desk_intake_contact BEGIN SELECT RAISE(ABORT,'receipt unavailable'); END;").unwrap(),
                _ => unreachable!(),
            }
            // The two ship directions use the existing ordinary mail path;
            // captain mail follows the agreed desk in both placement variants.
            // Suppress mail.send's eager flush without changing the route,
            // so both directions are pending when the failing contact runs.
            intake_home(&a).execute("INSERT OR REPLACE INTO mail_delivery_retry VALUES('beta',1,?1,0)",[i64::MAX]).unwrap();
            a.queued_mail("a-to-b",b.ship);
            intake_home(&a).execute("DELETE FROM mail_delivery_retry WHERE peer='beta'",[]).unwrap();
            b.queued_mail("b-to-a",a.ship);
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
            desk.assert_mail("captain-through-failure");
            let audits = intake_db(&a).query_row("SELECT COUNT(*) FROM desk_placement_audit WHERE event='desk.intake.exchange_failed'",[],|r| r.get::<_,i64>(0)).unwrap()
                + intake_db(&b).query_row("SELECT COUNT(*) FROM desk_placement_audit WHERE event='desk.intake.exchange_failed'",[],|r| r.get::<_,i64>(0)).unwrap();
            assert!(audits > 0,"{owner}/{stage} was not audited");
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
    for owner in ["alpha","beta"] {
        let (t,a,b) = pair();
        intake_enroll(&a,owner);
        let proxy = t.path().join("legacy-gateway");
        fs::write(&proxy,format!("#!{}\nimport json,subprocess,sys\nr=json.loads(sys.stdin.readline())\nr.get('args',{{}}).pop('desk_intake',None)\np=subprocess.run([{}]+sys.argv[1:],input=json.dumps(r)+'\\n',text=True,capture_output=True)\nfor line in p.stdout.splitlines():\n v=json.loads(line)\n if isinstance(v.get('result'),dict): v['result'].pop('desk_intake',None)\n print(json.dumps(v))\nsys.stderr.write(p.stderr)\nsys.exit(p.returncode)\n",support::python().display(),json!(support::BIN))).unwrap();
        fs::set_permissions(&proxy,fs::Permissions::from_mode(0o755)).unwrap();
        intake_db(&a).execute("UPDATE machines SET peer=json_set(peer,'$.executable',?1) WHERE name='beta'",[proxy.to_str().unwrap()]).unwrap();
        b.queued_mail("legacy-to-alpha",a.ship);
        a.collect();
        a.assert_mail("legacy-to-alpha");
        assert!(a.audit_count("desk.intake.exchange_failed") > 0);
        let gap: String = intake_db(&a).query_row("SELECT state FROM desk_intake_gap",[],|r|r.get(0)).unwrap();
        assert_eq!(gap,"unsupported");
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
    assert!(!intake_db(&b).query_row("SELECT receipted FROM desk_intake_transport",[],|r| r.get::<_,bool>(0)).unwrap()); // a newer empty page awaits its next receipt
}
