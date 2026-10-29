use super::*;

#[test]
fn quiet_collect_with_a_retained_baseline_writes_no_intake_rows() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("survey");
    let mut home = crate::store::Store::open(&path).unwrap();
    crate::mail::source::bind(home.connection(), SOURCE).unwrap();
    crate::mail::dispatch(
        &mut home,
        "ship.put",
        json!({"name":"survey","location":"here"}),
    )
    .unwrap();
    crate::mail::dispatch(&mut home,"mail.send",json!({"id":"retained","from":"survey","to":"captain","kind":"notice","body":"Retain unread evidence"})).unwrap();
    let source = db();
    let desk = db();
    source
        .execute(
            "INSERT INTO ships(home) VALUES(?1)",
            [path.to_string_lossy().as_ref()],
        )
        .unwrap();
    let mut c = contact_for_test(false);
    c.direction = "collect".into();
    c.manifest.sources[0].intake_direction = "collect".into();
    let owner = Contact {
        manifest: c.manifest.clone(),
        direction: "collect".into(),
        ..contact_for_test(true)
    };
    super::super::super::baseline::enroll_home(
        &path,
        &source,
        &c.manifest,
        SOURCE,
        Instant::now() + Duration::from_secs(10),
        100,
        || Ok(()),
    )
    .unwrap();
    // Settle receipts/cleanup with a fixed injected scheduling clock, not the
    // runner's uptime. Source/desk facts and retry custody remain retained.
    for _ in 0..4 {
        let frame = source_frame_at(
            &source,
            &c,
            true,
            Instant::now() + Duration::from_secs(10),
            WIRE_BYTES,
            1000,
        )
        .unwrap();
        receive(&desk, &owner, frame).unwrap();
        receive(&source, &c, owner_frame(&desk, &owner).unwrap()).unwrap();
    }
    let baseline = crate::mail::source::enrollment::open(&path, false).unwrap();
    let version = |conn: &Connection| {
        conn.query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0))
            .unwrap()
    };
    let before = (
        source.total_changes(),
        desk.total_changes(),
        version(home.connection()),
        version(&baseline),
    );
    for _ in 0..32 {
        let frame = source_frame_at(
            &source,
            &c,
            true,
            Instant::now() + Duration::from_secs(10),
            WIRE_BYTES,
            1000,
        )
        .unwrap();
        assert!(frame.page.is_none());
        receive(&desk, &owner, frame).unwrap();
        receive(&source, &c, owner_frame(&desk, &owner).unwrap()).unwrap();
    }
    assert_eq!(
        (
            source.total_changes(),
            desk.total_changes(),
            version(home.connection()),
            version(&baseline)
        ),
        before
    );
    assert_eq!(count(&desk, "desk_intake_fact"), 1);
    assert_eq!(count(home.connection(), "mail"), 1);
}

#[test]
fn strict_partial_envelope_is_backpressure_not_unsupported_and_never_loses_negotiation() {
    let db = db();
    let c = contact_for_test(false);
    state(&db, &c).unwrap();
    db.execute("UPDATE desk_intake_transport SET supported=1", [])
        .unwrap();
    db.execute("DELETE FROM desk_intake_gap", []).unwrap();
    for _ in 0..10 {
        let partial = partial_frame();
        assert_eq!(
            validate(&c, &partial).unwrap_err().to_string(),
            "backpressure: peer intake partial"
        );
        negotiation_gap(&db, &c, &partial).unwrap();
    }
    assert_eq!(
        db.query_row("SELECT supported FROM desk_intake_transport", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row("SELECT state FROM desk_intake_gap", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "backpressure"
    );
    let mut invalid = partial_frame();
    invalid["extra"] = json!(true);
    assert!(
        validate(&c, &invalid)
            .unwrap_err()
            .to_string()
            .starts_with("unsupported:")
    );
}

#[test]
fn repeated_nulls_forget_capability_but_a_valid_contact_resets_the_run() {
    for specific in [None, Some("unreadable"), Some("history_gap")] {
        let db = db();
        let c = contact_for_test(false);
        state(&db, &c).unwrap();
        db.execute("UPDATE desk_intake_transport SET supported=1", [])
            .unwrap();
        if let Some(s) = specific {
            gap(&db, &c.manifest, SOURCE, s).unwrap();
        }
        for _ in 0..2 {
            negotiation_gap(&db, &c, &Value::Null).unwrap();
        }
        let owner = Contact {
            manifest: c.manifest.clone(),
            ..contact_for_test(true)
        };
        receive(&db, &c, frame(&owner).unwrap()).unwrap();
        for expected in [2, 3, 0] {
            negotiation_gap(&db, &c, &Value::Null).unwrap();
            assert_eq!(
                db.query_row("SELECT supported FROM desk_intake_transport", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                expected
            );
        }
        assert_eq!(
            db.query_row("SELECT state FROM desk_intake_gap", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            specific.unwrap_or("unsupported")
        );
    }
}

#[test]
fn json_size_bound_covers_escaping_and_sizing_never_mutates_large_mail() {
    for v in [
        json!(null),
        json!(false),
        json!(-1.7976931348623157e308),
        json!(u64::MAX),
        json!({"\u{0}\"\\":"\u{1}\n\r\t\"\\é😀","array":[true,false,null,0,1e-100]}),
    ] {
        assert!(
            json_size_bound(&v) >= serde_json::to_vec(&v).unwrap().len(),
            "{v}"
        );
    }
    let db = db();
    let mut mail = json!({"messages":[{"id":"ordinary","body":"x".repeat(1024*1024)}]});
    let ptr = mail["messages"][0]["body"].as_str().unwrap().as_ptr();
    let bound = json_size_bound(&mail);
    assert!(bound > 1024 * 1024);
    let text = "\"\\\n".repeat(1024 * 1024);
    let size = string_size(&text);
    assert!(size >= serde_json::to_vec(&text).unwrap().len());
    attach(&db, "beta", &mut mail, partial_frame(), 1024 * 1024);
    assert_eq!(
        mail["messages"][0]["body"].as_str().unwrap().as_ptr(),
        ptr,
        "sizing must neither clone nor serialize/reallocate the mail payload"
    );
    assert!(mail.get("desk_intake").is_none());
}

#[test]
fn a_superseding_event_must_be_desk_receipted_before_old_live_custody_can_be_reclaimed() {
    let source = db();
    let c = contact_for_test(false);
    append_test_fact(&source, &c, 1);
    let cp = checkpoint(
        &source,
        &c.manifest,
        SOURCE,
        &uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    let p = page(&source, &c.manifest, &cp, 0, ITEMS, BYTES).unwrap();
    let mut fact = p.events[0].fact.clone();
    fact.unread = false;
    fact.pending = false;
    fact.state = Some("closed".into());
    append(
        &source,
        &c.manifest,
        SOURCE,
        &uuid::Uuid::new_v4().to_string(),
        &fact,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    retention::acknowledged(&source, &c.manifest, &p, deadline).unwrap();
    assert_eq!(
        count(&source, "desk_source_event"),
        2,
        "unreceived closure is not a receipt proof"
    );
    source
        .execute("DELETE FROM desk_source_barrier", [])
        .unwrap();
    let cp = checkpoint(
        &source,
        &c.manifest,
        SOURCE,
        &uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    let p = page(&source, &c.manifest, &cp, 0, ITEMS, BYTES).unwrap();
    let desk = db();
    accept(
        &desk,
        &c.manifest,
        &p,
        &Clock {
            session: "desk",
            wall_ms: 100,
            tick_ms: 1,
        },
    )
    .unwrap();
    // Cursor cleanup can first reach the newer row, then wrap to the older
    // newly-superseded live row. Progress is bounded, not prefix-blocked.
    for _ in 0..3 {
        retention::acknowledged(&source, &c.manifest, &p, deadline).unwrap();
    }
    assert_eq!(count(&source, "desk_source_event"), 0);
    assert_eq!(count(&desk, "desk_intake_fact"), 1);
}

#[test]
fn large_plain_mail_keeps_intake_and_oversize_pages_fall_back_to_partial() {
    let db = db();
    let mut body = json!({"snapshot":"x".repeat(256 * 1024)});
    let ordinary = serde_json::to_vec(&body).unwrap().len();
    assert!(json_size_bound(&body) <= ordinary + 8);
    attach(&db, "beta", &mut body, partial_frame(), 300 * 1024);
    assert_eq!(body["desk_intake"], partial_frame());
    let mut body = json!({"messages":[]});
    attach(
        &db,
        "beta",
        &mut body,
        json!({"page":"x".repeat(WIRE_BYTES)}),
        16 * 1024,
    );
    assert_eq!(body["desk_intake"], partial_frame());
    let c = contact_for_test(false);
    state(&db, &c).unwrap();
    db.execute("UPDATE desk_intake_transport SET supported=1", [])
        .unwrap();
    for _ in 0..5 {
        negotiation_gap(&db, &c, &body["desk_intake"]).unwrap();
    }
    assert_eq!(
        db.query_row("SELECT supported FROM desk_intake_transport", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn registry_cleanup_work_is_bounded_past_thousands_of_live_facts() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    for size in [600, 3000] {
        let source = db();
        let c = contact_for_test(false);
        // Seed in one transaction; avoid measuring append admission here.
        let tx = source.unchecked_transaction().unwrap();
        for n in 0..size {
            let mut f = Fact {
                origin: MailIdentity::new(
                    SOURCE,
                    "00000000-0000-4000-8000-00000000000c",
                    &format!("fact-{n}"),
                )
                .unwrap(),
                copy: None,
                content: json!({}),
                state: None,
                unread: false,
                pending: false,
            };
            f.unread = n < 600;
            f.pending = n < 600;
            f.state = Some(if n < 600 { "open" } else { "closed" }.into());
            tx.execute("INSERT INTO desk_source_event(domain,source,generation,incarnation,seq,intent,event) VALUES(?1,?2,1,?3,?4,?5,?6)",
                params![c.manifest.domain,SOURCE,c.manifest.incarnation,n+1,uuid::Uuid::new_v4().to_string(),serde_json::to_string(&f).unwrap()]).unwrap();
        }
        tx.commit().unwrap();
        let cp = checkpoint(
            &source,
            &c.manifest,
            SOURCE,
            &uuid::Uuid::new_v4().to_string(),
        )
        .unwrap();
        // The final page's custody authorizes cleanup of the whole range.
        let p = Page {
            checkpoint: cp,
            after: size,
            through: size,
            events: vec![],
        };
        for _ in 0..(size / 50 + 2) {
            let steps = Arc::new(AtomicUsize::new(0));
            let counter = steps.clone();
            source.progress_handler(
                1,
                Some(move || {
                    counter.fetch_add(1, Ordering::Relaxed);
                    false
                }),
            );
            retention::acknowledged(
                &source,
                &c.manifest,
                &p,
                Instant::now() + Duration::from_secs(10),
            )
            .unwrap();
            source.progress_handler(0, None::<fn() -> bool>);
            assert!(
                steps.load(Ordering::Relaxed) < 20_000,
                "size={size}, VM steps={}",
                steps.load(Ordering::Relaxed)
            );
        }
        assert_eq!(count(&source, "desk_source_event"), 600);
        assert_eq!(
            source
                .query_row("SELECT reclaimed_through FROM desk_source_head", [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
        if size > 600 {
            assert!(
                page(&source, &c.manifest, &p.checkpoint, 0, ITEMS, BYTES)
                    .unwrap_err()
                    .to_string()
                    .starts_with("stale_or_unknown")
            );
        }
    }
}
