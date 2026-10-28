use super::*;

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
    let mut remaining = 7;
    let text = "\"\\\n".repeat(1024 * 1024);
    let size = string_size(&text, &mut remaining);
    assert_eq!(
        remaining, 0,
        "escape scanning must stop at its shared work bound"
    );
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
    retention::acknowledged(&source, &c.manifest, &p, deadline).unwrap();
    assert_eq!(count(&source, "desk_source_event"), 0);
    assert_eq!(count(&desk, "desk_intake_fact"), 1);
}
