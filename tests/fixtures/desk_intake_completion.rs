// Only source observation status is activated. All transports below invoke
// real gateways over alpha -> beta; no reverse reader or second link exists.
fn completion_source(desk: &Machine, core: &str) -> Value {
    let status = desk.status();
    assert_eq!(status["coverage_complete"], false);
    assert_eq!(status["subscriptions"], false);
    status["source_status"].as_array().unwrap().iter().find(|s| s["id"] == core).unwrap().clone()
}
fn completion_settle(a: &Machine, desk: &Machine, source: &Machine, core: &str) -> Value {
    let mut last = Value::Null;
    // Each pass imports one bounded work unit and exchanges real pages. This
    // is protocol progress, not a timing retry or a sleep around a race.
    for _ in 0..12 {
        intake_idle(source);
        a.collect();
        a.collect();
        last = completion_source(desk, core);
        if last["complete"] == true { return last; }
    }
    panic!("source did not complete: {last}; proof={:?}", intake_received_proof(desk, core));
}
fn completion_clock_pair() -> (tempfile::TempDir, Machine, Machine) {
    let (t, mut a, mut b) = pair();
    for m in [&mut a, &mut b] {
        m.intake_budget_ms = Some(10_000);
        // Fixture continuity is explicit, not boot uptime or wall time. A new
        // production gateway without this test-only feature is discontinuous.
        m.intake_clock = Some([1_000_000, 2000]);
    }
    (t, a, b)
}

#[test]
fn intake_source_visibility_completion_and_obstacles_cross_real_subprocesses() {
    for owner in ["alpha", "beta"] {
        let (t, a, b) = completion_clock_pair();
        let (desk, source) = if owner == "alpha" { (&a, &b) } else { (&b, &a) };
        let gamma = Machine::new(source.root.clone(), "gamma");
        intake_notice(&gamma, "retired-unread");
        for id in ["uncollected-notice", "pre-move-unread"] { intake_notice(source,id); }
        source.ok("mail.send", json!({"id":"old-open-request","from":source.ship,"to":"captain","kind":"request","body":"Retained before placement"}));
        source.ok("mail.send", json!({"id":"local-outgoing-481","from":"captain","to":source.ship,"kind":"notice","body":"Must remain local"}));
        intake_home(source).execute("UPDATE mail SET created_ms=1 WHERE id IN ('old-open-request','pre-move-unread')",[]).unwrap();
        let ids = ["uncollected-notice","pre-move-unread","old-open-request","local-outgoing-481"];
        for id in ids { assert!(!intake_fact_ids(desk).contains(&id.to_string()), "{id} must be absent before ingestion"); }
        intake_enroll(&a,owner);
        let core = intake_external_source(desk);
        let covered = completion_settle(&a,desk,source,&core);
        assert_eq!(covered["state"],"covered");
        assert_eq!(covered["barrier_mode"], if owner=="alpha" {"on_demand"} else {"last_received_push"});
        assert_eq!(covered["max_source_age_ms"],900000);
        for id in ids {
            assert_eq!(intake_fact_ids(desk).iter().filter(|got|got.as_str()==id).count(),1,"{owner}/{id}");
        }
        assert_eq!(intake_home(source).query_row("SELECT state FROM mail WHERE id='old-open-request'",[],|r|r.get::<_,String>(0)).unwrap(),"open");
        // The observational #481 gap closes without forwarding that local send.
        assert_eq!(intake_home(desk).query_row("SELECT COUNT(*) FROM mail WHERE id='local-outgoing-481'",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        source.ok("mail.send",json!({"id":"origin-closure","from":"captain","to":source.ship,"kind":"reply","correlation":"old-open-request","body":"Origin alone closes its ask"}));
        a.collect();
        assert_eq!(completion_source(desk,&core)["complete"],false,"post-census mail cannot reuse an old proof");
        completion_settle(&a,desk,source,&core);
        assert_eq!(intake_db(desk).query_row("SELECT state FROM desk_intake_fact WHERE id='old-open-request'",[],|r|r.get::<_,String>(0)).unwrap(),"closed");
        assert_eq!(intake_db(desk).query_row("SELECT COUNT(*) FROM desk_intake_fact WHERE id='origin-closure'",[],|r|r.get::<_,i64>(0)).unwrap(),1);
        // A missing retained home invalidates on contact before another idle
        // census. Facts and last-known unread/open counts remain available.
        let missing=t.path().join("gamma-missing");
        intake_db(source).execute("INSERT INTO desk_home_inventory(home) VALUES(?1)",[missing.to_string_lossy().as_ref()]).unwrap();
        a.collect();
        let partial=completion_source(desk,&core);
        assert_eq!(partial["complete"],false);
        assert!(partial["reason"].is_string());
        assert!(partial["homes"].as_array().unwrap().iter().any(|h|h["unread"].as_i64().is_some_and(|n|n>0)));
        intake_idle(source);
        for id in ids { assert!(intake_fact_ids(desk).contains(&id.to_string())); }
        // Removing only this fixture's bogus path heals a recoverable obstacle,
        // not a sticky inventory/history-loss marker.
        intake_db(source).execute("DELETE FROM desk_home_inventory WHERE home=?1",[missing.to_string_lossy().as_ref()]).unwrap();
        completion_settle(&a,desk,source,&core);
        // Unknown/transitive membership is not silently included or observed.
        intake_db(source).execute("INSERT INTO machine_ships(machine,name,scope) VALUES('gamma','gamma','fleet')",[]).unwrap();
        a.collect();
        assert_eq!(completion_source(desk,&core)["complete"],false);
        intake_db(source).execute("DELETE FROM machine_ships WHERE machine='gamma'",[]).unwrap();
        completion_settle(&a,desk,source,&core);
        // An unreadable enrolled home retains its projection and cached counts.
        let home=source.root.join("gamma").join(".privateer");
        let saved=home.with_extension("saved");
        fs::rename(&home,&saved).unwrap();
        fs::create_dir(&home).unwrap();
        fs::write(home.join("privateer.sqlite"),"unreadable gamma fixture").unwrap();
        // Ordinary remote mail still reaches the other, healthy home. Do not
        // try to open the intentionally broken source Store for a CLI command.
        b.queued_mail("ordinary-during-obstacle",a.ship);
        a.collect();
        assert_eq!(completion_source(desk,&core)["complete"],false);
        fs::remove_file(home.join("privateer.sqlite")).unwrap();
        fs::remove_dir(&home).unwrap();
        fs::rename(&saved,&home).unwrap();
        a.collect(); a.assert_mail("ordinary-during-obstacle");
        completion_settle(&a,desk,source,&core);
        // Retiring a manifest member changes membership, not custody. Its
        // unread facts and last-known counts cannot vanish from coverage.
        intake_db(source).execute("UPDATE ships SET retired_ms=1 WHERE name='gamma'",[]).unwrap();
        a.collect(); intake_idle(source); a.collect();
        assert_eq!(completion_source(desk,&core)["complete"],false);
        assert!(intake_fact_ids(desk).contains(&"retired-unread".to_string()));
        intake_db(source).execute("UPDATE ships SET retired_ms=NULL WHERE name='gamma'",[]).unwrap();
        completion_settle(&a,desk,source,&core);
        // A failed link is an availability obstacle independent of proof.
        intake_db(desk).execute("UPDATE machines SET failures=1",[]).unwrap();
        let failed=completion_source(desk,&core);
        assert_eq!(failed["complete"],false);
        assert_eq!(failed["reason"],"source_unreachable");
        intake_db(desk).execute("UPDATE machines SET failures=0",[]).unwrap();
        completion_settle(&a,desk,source,&core);
        // A removed required source remains in the manifest/status, with its
        // last-known facts. No timeout or directory deletion retires custody.
        intake_db(desk).execute("DELETE FROM machines WHERE name=?1",[if owner=="alpha" {"beta"} else {"alpha"}]).unwrap();
        let unavailable=completion_source(desk,&core);
        assert_eq!(unavailable["complete"],false);
        assert_eq!(unavailable["reason"],"source_contact_missing");
        for id in ids { assert!(intake_fact_ids(desk).contains(&id.to_string())); }
    }
}

#[test]
fn intake_push_freshness_empty_contacts_replay_restart_and_bounds_use_injected_clocks() {
    for bound in [900000,5000] {
    let (_t, mut a, mut b)=completion_clock_pair();
    intake_notice(&a,"push-retained");
    a.ok("machine.desk.propose",json!({"domain":"alpha-domain","owner":"beta","sources":["alpha","beta"],"max_source_age_ms":bound}));
    a.accept(); a.status(); intake_idle(&a); intake_idle(&b);
    let core=intake_external_source(&b);
    let covered=completion_settle(&a,&b,&a,&core);
    assert_eq!(covered["age_ms"],0);
    assert_eq!(covered["as_of"],"last_source_contact");
    // No new event, no new barrier: receipt/proof replay cannot refresh time.
    let old_proof=intake_received_proof(&b,&core).unwrap();
    let raw: String=intake_db(&a).query_row("SELECT pending FROM desk_intake_transport WHERE source=?1",[&core],|r|r.get(0)).unwrap();
    let old_page: Value=serde_json::from_str(&raw).unwrap();
    a.intake_clock=Some([1_000_000+bound+1,2000+bound+1]); b.intake_clock=a.intake_clock;
    let replay=b.gateway("fleet.collect",json!({"machine":"alpha","captain":false,"desk_intake":{"protocol":"fleet.door-mail.v1","binding":old_proof["checkpoint"]["binding"],"source":core,"direction":"push","role":"source","page":old_page,"ack":null,"complete":false},"desk_intake_proof":old_proof}));
    assert!(replay["error"].is_null(),"{replay}");
    let expired=completion_source(&b,&core);
    assert_eq!(expired["complete"],false);
    assert_eq!(expired["received_at"],covered["received_at"]);
    assert_eq!(expired["reason"],"contact_not_fresh");
    // Source wall time only schedules the existing periodic empty barrier.
    intake_db(&a).execute("UPDATE desk_source_head SET issued_at=0",[]).unwrap();
    a.collect(); a.collect();
    let empty=completion_source(&b,&core);
    assert_eq!(empty["complete"],true);
    assert_eq!(empty["received_at"],1_000_000+bound+1);
    assert_eq!(empty["max_source_age_ms"],bound);
    // A new CLI/gateway session without injected continuity is cached. The
    // read-only source proof and its original received time remain retained.
    b.intake_clock=None;
    let restart=completion_source(&b,&core);
    assert_eq!(restart["complete"],false);
    assert_eq!(restart["received_at"],empty["received_at"]);
    b.intake_clock=a.intake_clock;
    assert_eq!(completion_source(&b,&core)["complete"],false,"persistent restart invalidation is not undone by replay");
    intake_db(&a).execute("UPDATE desk_source_head SET issued_at=0",[]).unwrap();
    a.collect(); a.collect();
    assert_eq!(completion_source(&b,&core)["complete"],true);
    b.intake_clock=Some([999999,2000+bound+1]);
    assert_eq!(completion_source(&b,&core)["complete"],false,"backward desk clock cannot prove age");
    }
}

#[test]
fn intake_incomplete_pages_lost_receipts_and_newer_barriers_never_complete_early() {
    for owner in ["alpha","beta"] {
        let (_t,a,b)=completion_clock_pair();
        let (desk,source)=if owner=="alpha" {(&a,&b)} else {(&b,&a)};
        for index in 0..16 {
            source.ok("mail.send",json!({"id":format!("alpha-page-{index}"),"from":source.ship,"to":"captain","kind":"notice","body":"Bounded fixture content ".repeat(1024)}));
        }
        intake_enroll(&a,owner);
        let core=intake_external_source(desk);
        // Freeze the home baseline off-contact. Its individually viable
        // envelopes force a byte-bounded multi-page source barrier, without
        // fabricating journal rows or inline baseline enrollment.
        intake_idle(source);
        let mut partial=false;
        for _ in 0..3 {
            a.collect();
            partial=intake_db(desk).query_row("SELECT EXISTS(SELECT 1 FROM desk_intake_checkpoint WHERE source=?1 AND through_seq<upper_seq AND through_seq>0)",[&core],|r|r.get(0)).unwrap();
            if partial { break; }
        }
        assert!(partial,"real gateway must expose the unfinished page boundary");
        assert_eq!(completion_source(desk,&core)["complete"],false);
        let issued: i64=intake_db(desk).query_row("SELECT issued FROM desk_intake_order WHERE source=?1",[&core],|r|r.get(0)).unwrap();
        intake_notice(source,"alpha-after-interruption");
        intake_db(desk).execute_batch("CREATE TRIGGER completion_receipt_fail BEFORE UPDATE OF pending ON desk_intake_transport BEGIN SELECT RAISE(ABORT,'lost optional receipt'); END").unwrap();
        b.queued_mail("ordinary-through-partial",a.ship);
        a.collect(); a.assert_mail("ordinary-through-partial");
        assert_eq!(completion_source(desk,&core)["complete"],false);
        intake_db(desk).execute_batch("DROP TRIGGER completion_receipt_fail").unwrap();
        // Every command is a fresh host/gateway process; exact page custody
        // survives receipt loss and converges only under a newer proved vector.
        completion_settle(&a,desk,source,&core);
        let newer: i64=intake_db(desk).query_row("SELECT issued FROM desk_intake_order WHERE source=?1",[&core],|r|r.get(0)).unwrap();
        assert!(newer>issued);
        for index in 0..16 {
            let id=format!("alpha-page-{index}");
            assert_eq!(intake_fact_ids(desk).iter().filter(|got|got.as_str()==id).count(),1);
        }
        assert_eq!(intake_fact_ids(desk).iter().filter(|got|got.as_str()=="alpha-after-interruption").count(),1);
    }
}
