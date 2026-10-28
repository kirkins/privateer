use super::*;
#[path = "transport_regression_tests.rs"]
mod regression_tests;
#[path = "round4_tests.rs"]
mod round4_tests;
const SOURCE: &str = "00000000-0000-4000-8000-00000000000a";
fn contact_for_test(owner: bool) -> Contact {
    let manifest = serde_json::from_value(json!({"domain":"survey","manifest_revision":1,
        "max_source_age_ms":900000,"owner_core":"00000000-0000-4000-8000-00000000000b",
        "generation":1,"incarnation":uuid::Uuid::new_v4().to_string(),"coordinator":SOURCE,
        "sources":[{"core":SOURCE,"name":"alpha","ships":[],"intake_direction":"push"}]}))
    .unwrap();
    Contact {
        manifest,
        source: SOURCE.into(),
        owner,
        direction: "push".into(),
        _fence: tempfile::tempfile().unwrap(),
    }
}
fn count(db: &Connection, table: &str) -> i64 {
    db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}
fn db() -> Connection {
    let c = Connection::open_in_memory().unwrap();
    init(&c).unwrap();
    c.execute_batch("CREATE TABLE ships(home TEXT);").unwrap();
    c
}
#[test]
fn empty_pages_keep_custody_until_exact_receipt_and_prior_receipts_are_harmless() {
    let source = db();
    let desk = db();
    let c = contact_for_test(false);
    let owner = Contact {
        manifest: c.manifest.clone(),
        ..contact_for_test(true)
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let first = source_frame(&source, &c, true, deadline).unwrap();
    let first_id = first.page.as_ref().unwrap().checkpoint.id.clone();
    let retry = source_frame(&source, &c, true, deadline).unwrap();
    assert_eq!(retry.page.as_ref().unwrap().checkpoint.id, first_id);
    receive(&desk, &owner, first).unwrap();
    let first_ack = owner_frame(&desk, &owner).unwrap();
    let old_wire = serde_json::to_value(&first_ack).unwrap();
    receive(&source, &c, first_ack).unwrap();
    append_test_fact(&source, &c, 1);
    let second = source_frame(&source, &c, true, deadline).unwrap();
    let second_id = second.page.as_ref().unwrap().checkpoint.id.clone();
    assert_ne!(first_id, second_id);
    // The desk may repeat its previous receipt after our new page failed.
    receive(&source, &c, serde_json::from_value(old_wire).unwrap()).unwrap();
    let retried = source_frame(&source, &c, true, deadline).unwrap();
    assert_eq!(retried.page.as_ref().unwrap().checkpoint.id, second_id);
    let mut forged = owner_frame(&desk, &owner).unwrap();
    forged.ack.as_mut().unwrap().receipt = "forged".into();
    assert!(
        receive(&source, &c, forged)
            .unwrap_err()
            .to_string()
            .contains("invalid intake receipt")
    );
    assert!(
        !source
            .query_row("SELECT receipted FROM desk_intake_transport", [], |r| r
                .get::<_, bool>(0))
            .unwrap()
    );
    receive(&desk, &owner, second).unwrap();
    receive(&source, &c, owner_frame(&desk, &owner).unwrap()).unwrap();
    assert!(
        source
            .query_row("SELECT receipted FROM desk_intake_transport", [], |r| r
                .get::<_, bool>(0))
            .unwrap()
    );
}
#[test]
fn periodic_empty_push_barriers_use_domain_bound_without_resending_pages() {
    for max in [900_000, 2000] {
        let source = db();
        let desk = db();
        let mut c = contact_for_test(false);
        c.manifest.max_source_age_ms = max;
        let owner = Contact {
            manifest: c.manifest.clone(),
            ..contact_for_test(true)
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let f = source_frame_at(&source, &c, true, deadline, WIRE_BYTES, 10_000).unwrap();
        let first = f.page.clone().unwrap();
        let clock = Clock {
            session: "desk-clock",
            wall_ms: 20_000,
            tick_ms: 1000,
        };
        accept(&desk, &c.manifest, &first, &clock).unwrap();
        receive(&desk, &owner, f).unwrap();
        receive(&source, &c, owner_frame(&desk, &owner).unwrap()).unwrap();
        assert!(
            source_frame_at(
                &source,
                &c,
                true,
                deadline,
                WIRE_BYTES,
                10_000 + max / 2 - 1
            )
            .unwrap()
            .page
            .is_none()
        );
        let f = source_frame_at(&source, &c, true, deadline, WIRE_BYTES, 10_000 + max / 2).unwrap();
        let empty = f.page.clone().unwrap();
        assert!(empty.events.is_empty());
        assert_eq!(empty.after, empty.through);
        assert!(empty.checkpoint.issued > first.checkpoint.issued);
        let later = Clock {
            session: "desk-clock",
            wall_ms: clock.wall_ms + max / 2,
            tick_ms: clock.tick_ms + max / 2,
        };
        accept(&desk, &c.manifest, &empty, &later).unwrap();
        receive(&desk, &owner, f).unwrap();
        let evidence: (i64, String, i64) = desk
            .query_row(
                "SELECT received_at,session,tick_ms FROM desk_intake_checkpoint",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            evidence,
            (later.wall_ms, later.session.into(), later.tick_ms)
        );
        assert!(later.wall_ms > clock.wall_ms);
        let before: i64 = desk
            .query_row("SELECT received_at FROM desk_intake_checkpoint", [], |r| {
                r.get(0)
            })
            .unwrap();
        // A replay is not a new contact, even with a later desk clock.
        accept(
            &desk,
            &c.manifest,
            &empty,
            &Clock {
                session: "desk-clock",
                wall_ms: before + 500,
                tick_ms: later.tick_ms + 500,
            },
        )
        .unwrap();
        assert_eq!(
            desk.query_row("SELECT received_at FROM desk_intake_checkpoint", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap(),
            before
        );
        receive(&source, &c, owner_frame(&desk, &owner).unwrap()).unwrap();
        // A backwards source-clock discontinuity schedules another barrier;
        // it does not establish desk-clock freshness by itself.
        assert!(
            source_frame_at(&source, &c, true, deadline, WIRE_BYTES, 1)
                .unwrap()
                .page
                .is_some()
        );
    }
}

#[test]
fn negotiated_omissions_preserve_specific_gaps_and_use_partial_not_unsupported() {
    let db = db();
    let c = contact_for_test(false);
    state(&db, &c).unwrap();
    negotiation_gap(&db, &c, &Value::Null).unwrap();
    assert_eq!(
        db.query_row("SELECT state FROM desk_intake_gap", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "unsupported"
    );
    for state in [
        None,
        Some("unreadable"),
        Some("history_gap"),
        Some("integrity_gap"),
    ] {
        db.execute("DELETE FROM desk_intake_gap", []).unwrap();
        if let Some(state) = state {
            gap(&db, &c.manifest, SOURCE, state).unwrap();
        }
        db.execute("UPDATE desk_intake_transport SET supported=1", [])
            .unwrap();
        negotiation_gap(&db, &c, &Value::Null).unwrap();
        assert_eq!(
            db.query_row("SELECT state FROM desk_intake_gap", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            state.unwrap_or("backpressure")
        );
    }
}

#[test]
fn thousands_of_contacts_and_messages_reclaim_only_proved_safe_custody() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("home");
    let mut home = crate::store::Store::open(&path).unwrap();
    crate::mail::source::bind(home.connection(), SOURCE).unwrap();
    crate::mail::dispatch(
        &mut home,
        "ship.put",
        json!({"name":"survey","location":"here"}),
    )
    .unwrap();
    let held_path = dir.path().join("held");
    let mut held = crate::store::Store::open(&held_path).unwrap();
    crate::mail::source::bind(held.connection(), SOURCE).unwrap();
    crate::mail::dispatch(
        &mut held,
        "ship.put",
        json!({"name":"survey","location":"here"}),
    )
    .unwrap();
    let source = db();
    for path in [&path, &held_path] {
        source
            .execute(
                "INSERT INTO ships(home) VALUES(?1)",
                [path.to_str().unwrap()],
            )
            .unwrap();
    }
    let desk = db();
    let c = contact_for_test(false);
    let owner = Contact {
        manifest: c.manifest.clone(),
        ..contact_for_test(true)
    };
    // Keep both kinds of live custody across the entire run. A permanently
    // live early event must not pin disposable traffic behind it forever.
    for (id, from, to, kind) in [
        ("held-open", "captain", "survey", "request"),
        ("held-unread", "survey", "captain", "notice"),
    ] {
        crate::mail::dispatch(
            if id == "held-unread" {
                &mut held
            } else {
                &mut home
            },
            "mail.send",
            json!({"id":id,"from":from,"to":to,"kind":kind,"body":"held"}),
        )
        .unwrap();
    }
    for path in [&path, &held_path] {
        crate::machines::desk_intake::baseline::enroll_home(
            path,
            &source,
            &c.manifest,
            SOURCE,
            Instant::now() + Duration::from_secs(10),
            100,
            || Ok(()),
        )
        .unwrap();
    }
    let bounded_frame = || {
        // A production contact may legitimately return partial on a loaded
        // runner. Retry those bounded contacts, not arbitrary failures; each
        // committed import remains durable across the partial reply. Separate
        // injected-clock tests prove progress independent of machine speed.
        for _ in 0..100 {
            let deadline = Instant::now() + Duration::from_millis(WORK_MS);
            let _budget = Budget::new(&source, deadline).unwrap();
            match source_frame(&source, &c, true, deadline) {
                Ok(frame) => return frame,
                Err(e) => assert!(
                    retention::interrupted_or_busy(&e) || format!("{e:#}").contains("backpressure"),
                    "unexpected contact failure: {e:#}"
                ),
            }
        }
        panic!("intake did not resume after 100 bounded partial contacts");
    };
    let mut first = None;
    for n in 0..2000 {
        let id = format!("traffic-{n}");
        let incoming = n % 3 != 0;
        let request = n % 3 == 2;
        let sent = crate::mail::dispatch(
            &mut home,
            "mail.send",
            json!({"id":id,
            "from":if incoming {"survey"} else {"captain"},
            "to":if incoming {"captain"} else {"survey"},
            "kind":if request {"request"} else {"notice"},"body":"retained traffic"}),
        )
        .unwrap();
        // One home per contact: rotate across both real homes, without
        // using the traffic home's acknowledgement to read the held notice.
        for _ in 0..3 {
            let f = bounded_frame();
            if first.is_none() {
                first = f.page.clone();
            }
            receive(&desk, &owner, f).unwrap();
            receive(&source, &c, owner_frame(&desk, &owner).unwrap()).unwrap();
        }
        assert!(retention::head(&source, &c.manifest, SOURCE).unwrap() > n);
        if incoming {
            crate::mail::dismiss(&home, &id, "finished").unwrap();
            // Dismissal closes a request but does not implicitly read it.
            // Acknowledge that closed request in THIS home only.
            if request {
                crate::mail::dispatch(
                    &mut home,
                    "mail.ack",
                    json!({"address":"captain","through_seq":sent["seq"]}),
                )
                .unwrap();
            }
            for _ in 0..3 {
                let f = bounded_frame();
                receive(&desk, &owner, f).unwrap();
                receive(&source, &c, owner_frame(&desk, &owner).unwrap()).unwrap();
            }
        }
        for table in [
            "desk_source_event",
            "desk_home_receipt",
            "desk_source_barrier",
            "desk_source_issuance",
        ] {
            assert!(
                count(&source, table) <= 32,
                "{n}: {table}={}",
                count(&source, table)
            );
        }
        for table in ["mail_source_outbox", "mail_source_import"] {
            assert!(
                count(home.connection(), table) <= 32,
                "{n}: {table}={}",
                count(home.connection(), table)
            );
        }
        assert_eq!(count(&desk, "desk_intake_contact"), 1);
    }
    for _ in 0..8 {
        let f = bounded_frame();
        receive(&desk, &owner, f).unwrap();
        receive(&source, &c, owner_frame(&desk, &owner).unwrap()).unwrap();
    }
    // Dismissed requests create correlated notices too; the two held live
    // facts must survive along with every traffic and closure fact.
    let retained = 2000 + 2000 / 3 + 2;
    assert_eq!(
        count(&desk, "desk_intake_fact"),
        retained,
        "projection facts are not evicted"
    );
    assert_eq!(
        count(home.connection(), "mail") + count(held.connection(), "mail"),
        retained,
        "home facts are not evicted"
    );
    let health: (i64, i64) = home
        .connection()
        .query_row("SELECT entries,bytes FROM mail_source_health", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    let outbox: (i64, i64) = home
        .connection()
        .query_row(
            "SELECT COUNT(*),COALESCE(SUM(length(CAST(event AS BLOB))),0) FROM mail_source_outbox",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(health, outbox);
    let pending: String = source
        .query_row("SELECT pending FROM desk_intake_transport", [], |r| {
            r.get(0)
        })
        .unwrap();
    let current: Page = serde_json::from_str(&pending).unwrap();
    assert!(
        page(
            &source,
            &c.manifest,
            &current.checkpoint,
            0,
            ITEMS,
            WIRE_BYTES
        )
        .unwrap_err()
        .to_string()
        .starts_with("stale_or_unknown: reclaimed")
    );
    assert!(
        accept(&desk, &c.manifest, &first.unwrap(), &process_clock())
            .unwrap_err()
            .to_string()
            .contains("superseded")
    );
    assert!(
        desk.query_row(
            "SELECT unread FROM desk_intake_fact WHERE id='held-unread'",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap()
    );
    assert_eq!(
        desk.query_row(
            "SELECT state FROM desk_intake_fact WHERE id='held-open'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "open"
    );
    // Unread/open/pending custody survives even after exact receipts.
    append_test_fact(&source, &c, 9999);
    let f = source_frame(&source, &c, true, Instant::now() + Duration::from_secs(10)).unwrap();
    receive(&desk, &owner, f).unwrap();
    receive(&source, &c, owner_frame(&desk, &owner).unwrap()).unwrap();
    source_frame(&source, &c, true, Instant::now() + Duration::from_secs(10)).unwrap();
    assert_eq!(count(&source, "desk_source_event"), 3);
    assert_eq!(count(&desk, "desk_intake_fact"), retained + 1);
}

#[test]
fn optional_budget_interrupts_vm_and_restores_mail_busy_policy() {
    let c = db();
    c.busy_timeout(Duration::from_secs(10)).unwrap();
    {
        let _budget = Budget::new(&c, Instant::now()).unwrap();
        let result = c.query_row("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<100000) SELECT SUM(x) FROM n",[],|r|r.get::<_,i64>(0));
        assert!(result.is_err());
        assert!(check(Instant::now()).is_err());
    }
    assert_eq!(
        c.query_row("PRAGMA busy_timeout", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        10000
    );
    assert_eq!(
        c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0)).unwrap(),
        1
    );
}
#[test]
fn malformed_legacy_wrong_binding_role_direction_and_oversized_frames_fail_closed() {
    let c = contact_for_test(false);
    let owner = Contact {
        manifest: c.manifest.clone(),
        ..contact_for_test(true)
    };
    let good = serde_json::to_value(frame(&owner).unwrap()).unwrap();
    assert!(validate(&c, &good).is_ok());
    assert!(validate(&c, &Value::Null).is_err());
    for (key, value) in [
        ("protocol", json!("old")),
        ("binding", json!("wrong")),
        ("role", json!("source")),
        ("direction", json!("collect")),
        ("complete", json!(true)),
        ("extra", json!(true)),
    ] {
        let mut bad = good.clone();
        bad[key] = value;
        assert!(validate(&c, &bad).is_err(), "{key}");
    }
    let cdb = db();
    let mut mail = json!({"messages":[{"id":"ordinary","body":"x".repeat(1024*1024)}]});
    let unchanged = mail.clone();
    attach(&cdb, "beta", &mut mail, good, 1024 * 1024);
    assert_eq!(mail, unchanged);
}

fn append_test_fact(db: &Connection, c: &Contact, n: usize) {
    let fact = Fact {
        origin: MailIdentity::new(
            SOURCE,
            "00000000-0000-4000-8000-00000000000c",
            &format!("notice-{n}"),
        )
        .unwrap(),
        copy: None,
        content: json!({"body":format!("fact {n}")}),
        state: Some("open".into()),
        unread: true,
        pending: true,
    };
    append(
        db,
        &c.manifest,
        SOURCE,
        &uuid::Uuid::new_v4().to_string(),
        &fact,
    )
    .unwrap();
}

#[test]
fn two_hundred_quiet_and_advancing_contacts_reuse_and_reclaim_only_receipted_barriers() {
    for advancing in [false, true] {
        let source = db();
        let desk = db();
        let c = contact_for_test(false);
        let owner = Contact {
            manifest: c.manifest.clone(),
            ..contact_for_test(true)
        };
        let mut first = None;
        for n in 0..200 {
            if advancing {
                append_test_fact(&source, &c, n);
            }
            let f =
                source_frame(&source, &c, true, Instant::now() + Duration::from_secs(10)).unwrap();
            if n == 0 {
                first = f.page.clone();
            } else if !advancing {
                assert!(
                    f.page.is_none(),
                    "quiet contacts must not resend a receipted page"
                );
            }
            receive(&desk, &owner, f).unwrap();
            receive(&source, &c, owner_frame(&desk, &owner).unwrap()).unwrap();
            for (db, table) in [
                (&source, "desk_source_barrier"),
                (&desk, "desk_intake_contact"),
            ] {
                assert_eq!(
                    db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r
                        .get::<_, i64>(0))
                        .unwrap(),
                    1
                );
            }
        }
        assert_eq!(
            source
                .query_row("SELECT COUNT(*) FROM desk_source_issuance", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(desk.query_row("SELECT COUNT(*) FROM desk_intake_fact WHERE unread=1 AND pending=1 AND state='open'",[],|r|r.get::<_,i64>(0)).unwrap(), if advancing {200} else {0});
        assert_eq!(
            source
                .query_row("SELECT COUNT(*) FROM desk_source_event", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            if advancing { 200 } else { 0 }
        );
        let before: i64 = desk
            .query_row("SELECT received_at FROM desk_intake_checkpoint", [], |r| {
                r.get(0)
            })
            .unwrap();
        if advancing {
            // An expired receipt stays stale after reclamation. The ordinal
            // fence prevents resurrection and cannot refresh contact time.
            assert!(
                accept(&desk, &c.manifest, &first.unwrap(), &process_clock())
                    .unwrap_err()
                    .to_string()
                    .contains("superseded")
            );
        }
        assert_eq!(
            desk.query_row("SELECT received_at FROM desk_intake_checkpoint", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap(),
            before
        );
        assert_eq!(
            desk.query_row("SELECT COUNT(*) FROM desk_intake_audit", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}

#[test]
fn an_interrupted_barrier_pins_logs_but_not_other_final_issuances() {
    let source = db();
    let desk = db();
    let c = contact_for_test(false);
    let owner = Contact {
        manifest: c.manifest.clone(),
        ..contact_for_test(true)
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let first = source_frame(&source, &c, true, deadline).unwrap();
    receive(&desk, &owner, first).unwrap();
    receive(&source, &c, owner_frame(&desk, &owner).unwrap()).unwrap();
    let interrupted = checkpoint(
        &source,
        &c.manifest,
        SOURCE,
        &uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    for n in 0..100 {
        append_test_fact(&source, &c, n);
        let f = source_frame(&source, &c, true, deadline).unwrap();
        receive(&desk, &owner, f).unwrap();
        receive(&source, &c, owner_frame(&desk, &owner).unwrap()).unwrap();
        assert_eq!(count(&source, "desk_source_barrier"), 2);
        assert_eq!(
            count(&source, "desk_source_issuance"),
            2,
            "unrelated interrupted custody must not leak completed issuances"
        );
    }
    assert!(page(&source, &c.manifest, &interrupted, 0, ITEMS, WIRE_BYTES).is_ok());
    assert_eq!(count(&source, "desk_source_event"), 100);
}

#[test]
fn unreceipted_pages_are_never_reclaimed_when_log_advances() {
    let db = db();
    let c = contact_for_test(false);
    let deadline = Instant::now() + Duration::from_secs(10);
    let first = source_frame(&db, &c, true, deadline).unwrap().page.unwrap();
    for n in 0..40 {
        append_test_fact(&db, &c, n);
        assert_eq!(
            source_frame(&db, &c, true, deadline).unwrap().page.unwrap(),
            first
        );
    }
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM desk_source_barrier", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn binding_change_resets_transport_custody_but_not_old_journal_facts() {
    let db = db();
    let mut c = contact_for_test(false);
    state(&db, &c).unwrap();
    db.execute_batch("UPDATE desk_intake_transport SET after_seq=9,pending='old-page',home='old-home',supported=1,receipted=1,last_receipt='old-receipt'").unwrap();
    append_test_fact(&db, &c, 0);
    c.manifest.generation += 1;
    c.manifest.incarnation = uuid::Uuid::new_v4().to_string();
    assert_eq!(state(&db, &c).unwrap(), (0, None));
    let flags: (bool, bool, Option<String>, Option<String>) = db
        .query_row(
            "SELECT supported,receipted,home,last_receipt FROM desk_intake_transport",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(flags, (false, false, None, None));
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM desk_source_event", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    // Reset is transport only, not handoff authority or relabeling old facts.
    assert!(
        source_frame(&db, &c, true, Instant::now() + Duration::from_secs(10))
            .unwrap_err()
            .to_string()
            .contains("stale_generation")
    );
}

#[test]
fn capture_history_gap_does_not_hide_a_more_specific_gap() {
    let db = db();
    let c = contact_for_test(false);
    state(&db, &c).unwrap();
    capture_one(&db, &c, Instant::now() + Duration::from_secs(10)).unwrap();
    for specific in ["unsupported", "backpressure", "unreadable", "integrity_gap"] {
        gap(&db, &c.manifest, SOURCE, specific).unwrap();
        capture_one(&db, &c, Instant::now() + Duration::from_secs(10)).unwrap();
        assert_eq!(
            db.query_row("SELECT state FROM desk_intake_gap", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            specific
        );
    }
}

#[test]
fn intake_audit_is_independent_bounded_and_deduplicates_identical_peer_stage_errors() {
    let db = db();
    db.execute_batch("CREATE TABLE desk_placement_audit(seq INTEGER PRIMARY KEY,event TEXT,body TEXT,at_ms INTEGER);
        INSERT INTO desk_placement_audit VALUES(1,'placement diagnostic','preserved',0)").unwrap();
    for stage in ["prepare", "receive", "receipt"] {
        for error in ["unsupported", "backpressure", "intake exchange busy"] {
            for _ in 0..100 {
                audit(&db, "beta", stage, &anyhow::anyhow!(error));
            }
        }
    }
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM desk_intake_audit", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        9
    );
    for n in 0..300 {
        audit(&db, "beta", "prepare", &anyhow::anyhow!("error-{n}"));
    }
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM desk_intake_audit", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        256
    );
    assert_eq!(
        db.query_row("SELECT body FROM desk_placement_audit", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "preserved"
    );
}

#[test]
fn intake_frame_budget_accounts_for_mail_frame_and_wrapper() {
    let db = db();
    let wire = json!({"page":"x".repeat(4000)});
    let mail = json!({"messages":[{"id":"ordinary","body":"y".repeat(1000)}]});
    let mut body = mail.clone();
    attach(&db, "beta", &mut body, wire.clone(), 4096);
    assert_eq!(body["messages"], mail["messages"]);
    assert_eq!(body["desk_intake"], partial_frame());
    attach(&db, "beta", &mut body, wire, 8192);
    assert!(body["desk_intake"].is_object());
    assert!(serde_json::to_vec(&body).unwrap().len() + 1024 <= 8192);
}

#[test]
fn source_barrier_reclamation_and_successor_page_are_one_transaction() {
    let source = db();
    let desk = db();
    let c = contact_for_test(false);
    let owner = Contact {
        manifest: c.manifest.clone(),
        ..contact_for_test(true)
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let f = source_frame(&source, &c, true, deadline).unwrap();
    let first = f.page.as_ref().unwrap().checkpoint.clone();
    receive(&desk, &owner, f).unwrap();
    receive(&source, &c, owner_frame(&desk, &owner).unwrap()).unwrap();
    // Simulate a pre-fix journal already at the cap. Only our known final
    // receipt can free a slot; unrelated/interrupted barriers stay retained.
    for _ in 1..BARRIERS {
        checkpoint(
            &source,
            &c.manifest,
            SOURCE,
            &uuid::Uuid::new_v4().to_string(),
        )
        .unwrap();
    }
    append_test_fact(&source, &c, 0);
    source.execute_batch("CREATE TRIGGER fail_save BEFORE UPDATE OF pending ON desk_intake_transport BEGIN SELECT RAISE(ABORT,'save unavailable'); END").unwrap();
    assert!(
        source_frame(&source, &c, true, deadline)
            .unwrap_err()
            .to_string()
            .contains("save unavailable")
    );
    let exists = || {
        source
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM desk_source_barrier WHERE id=?1)",
                [&first.id],
                |r| r.get::<_, bool>(0),
            )
            .unwrap()
    };
    assert!(exists());
    assert_eq!(
        source
            .query_row("SELECT COUNT(*) FROM desk_source_issuance", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        BARRIERS
    );
    source.execute_batch("DROP TRIGGER fail_save").unwrap();
    let next = source_frame(&source, &c, true, deadline).unwrap();
    assert!(!exists());
    assert_eq!(next.page.as_ref().unwrap().through, 1);
    assert_eq!(
        source
            .query_row("SELECT COUNT(*) FROM desk_source_barrier", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        BARRIERS
    );
}

#[test]
fn interrupted_desk_contact_custody_is_not_reclaimed_to_bypass_backpressure() {
    let source = db();
    let desk = db();
    let c = contact_for_test(false);
    for n in 0..33 {
        append_test_fact(&source, &c, n);
    }
    let mut last = None;
    for n in 0..BARRIERS {
        let cp = checkpoint(
            &source,
            &c.manifest,
            SOURCE,
            &uuid::Uuid::new_v4().to_string(),
        )
        .unwrap();
        let p = page(&source, &c.manifest, &cp, n, 1, WIRE_BYTES).unwrap();
        accept(&desk, &c.manifest, &p, &process_clock()).unwrap();
        last = Some(cp);
    }
    let mut p = page(&source, &c.manifest, &last.unwrap(), 32, 1, WIRE_BYTES).unwrap();
    p.checkpoint.id = uuid::Uuid::new_v4().to_string();
    p.checkpoint.issued += 1;
    assert!(
        accept(&desk, &c.manifest, &p, &process_clock())
            .unwrap_err()
            .to_string()
            .contains("backpressure")
    );
    assert_eq!(
        desk.query_row(
            "SELECT COUNT(*) FROM desk_intake_contact WHERE through_seq<upper_seq",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        BARRIERS
    );
    assert_eq!(
        desk.query_row(
            "SELECT COUNT(*) FROM desk_intake_fact WHERE unread=1 AND pending=1",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        32
    );
}
