use super::*;

#[test]
fn interrupted_contact_resumes_with_newer_barrier_without_poisoning_coverage() {
    let source = db();
    let desk = db();
    let m = manifest();
    for n in 0..3 {
        capture(&source, &m, &fact(n));
    }
    let old = checkpoint(&source, &m, ALPHA, &id()).unwrap();
    accept(
        &desk,
        &m,
        &page(&source, &m, &old, 0, 1, BYTES).unwrap(),
        &clock(10),
    )
    .unwrap();
    capture(&source, &m, &fact(3));
    let newer = checkpoint(&source, &m, ALPHA, &id()).unwrap();
    let error = accept(
        &desk,
        &m,
        &page(&source, &m, &newer, 0, 1, BYTES).unwrap(),
        &clock(20),
    )
    .unwrap_err();
    assert!(error.to_string().starts_with("stale_or_unknown:"));
    assert!(error.to_string().contains("resume after 1"));
    assert_eq!(count(&desk, "desk_intake_gap"), 0);
    let resumed = page(&source, &m, &newer, 1, ITEMS, BYTES).unwrap();
    accept(&desk, &m, &resumed, &clock(30)).unwrap();
    assert_eq!(count(&desk, "desk_intake_fact"), 4);
    assert_eq!(received(&desk), Some(30));
    assert!(source_evidence(&desk, &m, ALPHA, &clock(31)).unwrap().1);
    // A late page from the interrupted, smaller barrier is resumably stale.
    let error = accept(
        &desk,
        &m,
        &page(&source, &m, &old, 1, ITEMS, BYTES).unwrap(),
        &clock(40),
    )
    .unwrap_err();
    assert!(error.to_string().starts_with("stale_or_unknown:"));
    assert_eq!(count(&desk, "desk_intake_gap"), 0);
}

#[test]
fn append_reserves_whole_single_event_page_at_the_byte_boundary() {
    let source = db();
    let m = manifest();
    let mut f = fact(0);
    f.content["body"] = json!("");
    let mut single = Page {
        checkpoint: Checkpoint {
            binding: binding(&m).unwrap(),
            source: ALPHA.into(),
            id: "urn:uuid:ffffffff-ffff-ffff-ffff-ffffffffffff".into(),
            upper: i64::MAX,
        },
        after: i64::MAX - 1,
        through: i64::MAX,
        events: vec![Event {
            order: i64::MAX,
            fact: f.clone(),
        }],
    };
    let headroom = BYTES - serde_json::to_vec(&single).unwrap().len();
    f.content["body"] = json!("x".repeat(headroom + 1));
    assert!(serde_json::to_vec(&f).unwrap().len() < BYTES);
    assert!(
        append(&source, &m, ALPHA, &id(), &f)
            .unwrap_err()
            .to_string()
            .starts_with("backpressure")
    );
    assert_eq!(count(&source, "desk_source_event"), 0);
    f.content["body"] = json!("x".repeat(headroom));
    append(&source, &m, ALPHA, &id(), &f).unwrap();
    source
        .execute("UPDATE desk_source_event SET seq=?1", [i64::MAX])
        .unwrap();
    let cp = checkpoint(&source, &m, ALPHA, &single.checkpoint.id).unwrap();
    single.events[0].fact = f;
    assert_eq!(serde_json::to_vec(&single).unwrap().len(), BYTES);
    assert_eq!(
        page(&source, &m, &cp, i64::MAX - 1, 1, BYTES).unwrap(),
        single
    );
}

#[test]
fn read_only_existing_evidence_is_cached_after_restart_without_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("desk.sqlite3");
    let m = manifest();
    let source = db();
    {
        let desk = Connection::open(&path).unwrap();
        init(&desk).unwrap();
        accept(&desk, &m, &one_page(&source, &m), &clock(10)).unwrap();
    }
    let desk =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    for c in [
        Clock {
            session: "process-2",
            wall_ms: 20,
            tick_ms: 0,
        },
        Clock {
            session: "process-1",
            wall_ms: 9,
            tick_ms: 20,
        },
        Clock {
            session: "process-1",
            wall_ms: 100_000,
            tick_ms: 20,
        },
    ] {
        let (evidence, fresh) = source_evidence(&desk, &m, ALPHA, &c).unwrap();
        assert_eq!(evidence.state, "cached");
        assert!(!fresh);
        assert_eq!(evidence.barrier.unwrap().received_at, 10);
    }
    assert!(valid(&desk)); // Read-only inspection did not mutate persisted evidence.
}

#[test]
fn new_partial_barrier_preserves_last_completed_contact_evidence() {
    let source = db();
    let desk = db();
    let m = manifest();
    accept(&desk, &m, &one_page(&source, &m), &clock(10)).unwrap();
    for n in 0..2 {
        capture(&source, &m, &fact(n));
    }
    let cp = checkpoint(&source, &m, ALPHA, &id()).unwrap();
    accept(
        &desk,
        &m,
        &page(&source, &m, &cp, 0, 1, BYTES).unwrap(),
        &clock(20),
    )
    .unwrap();
    assert_eq!(received(&desk), Some(10));
    let evidence: (String, i64) = desk
        .query_row(
            "SELECT session,tick_ms FROM desk_intake_checkpoint",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(evidence, ("process-1".into(), 10));
    let partial = source_coverage(&desk, &m, ALPHA, &clock(21)).unwrap();
    assert!(
        partial.barrier.is_none(),
        "new incomplete barrier must not carry old contact time"
    );
    assert!(!source_evidence(&desk, &m, ALPHA, &clock(21)).unwrap().1);
    accept(
        &desk,
        &m,
        &page(&source, &m, &cp, 1, 1, BYTES).unwrap(),
        &clock(30),
    )
    .unwrap();
    assert_eq!(received(&desk), Some(30));
}

#[test]
fn caller_metadata_budget_error_is_not_a_durable_source_gap() {
    let source = db();
    let m = manifest();
    let cp = checkpoint(&source, &m, ALPHA, &id()).unwrap();
    let error = page(&source, &m, &cp, 0, ITEMS, 1).unwrap_err();
    assert!(error.to_string().starts_with("invalid page bounds:"));
    assert_eq!(count(&source, "desk_intake_gap"), 0);
    assert!(page(&source, &m, &cp, 0, ITEMS, BYTES).is_ok());
}

#[test]
fn intent_retry_checks_generation_before_content_and_records_no_integrity_gap() {
    let source = db();
    let mut m = manifest();
    let intent = id();
    let mut f = fact(0);
    append(&source, &m, ALPHA, &intent, &f).unwrap();
    m.generation += 1;
    m.incarnation = id();
    for changed in [false, true] {
        if changed {
            f.content["body"] = json!("changed");
        }
        assert_eq!(
            append(&source, &m, ALPHA, &intent, &f)
                .unwrap_err()
                .to_string(),
            "stale_generation"
        );
    }
    assert_eq!(count(&source, "desk_intake_gap"), 0);
    assert_eq!(count(&source, "desk_source_event"), 1);
}

#[test]
fn failure_to_record_gap_preserves_original_intake_error() {
    let source = db();
    let m = manifest();
    source.execute_batch("DROP TABLE desk_intake_gap").unwrap();
    let mut f = fact(0);
    f.content["body"] = json!("x".repeat(BYTES));
    let error = append(&source, &m, ALPHA, &id(), &f).unwrap_err();
    let detail = format!("{error:#}");
    assert!(detail.contains("intake failed (backpressure)"), "{detail}");
    assert!(
        detail.contains("no such table: desk_intake_gap"),
        "{detail}"
    );
    assert_eq!(count(&source, "desk_source_event"), 0);
}

#[test]
fn origin_and_copy_wire_identities_reject_unknown_fields() {
    let mut f = fact(0);
    f.copy = Some(MailIdentity::new(BETA, COPY_BOX, "desk-copy").unwrap());
    let value = serde_json::to_value(f).unwrap();
    for field in ["origin", "copy"] {
        let mut unknown = value.clone();
        unknown[field]["extra"] = json!("not admitted");
        assert!(serde_json::from_value::<Fact>(unknown).is_err());
    }
    assert!(serde_json::from_value::<Fact>(value).is_ok());
}

#[test]
fn query_only_writable_connection_reports_discontinuity_without_update() {
    let source = db();
    let desk = db();
    let m = manifest();
    capture(&source, &m, &fact(0));
    accept(&desk, &m, &one_page(&source, &m), &clock(10)).unwrap();
    assert!(valid(&desk));
    desk.execute_batch("PRAGMA query_only=ON").unwrap();
    assert!(!desk.is_readonly(rusqlite::DatabaseName::Main).unwrap());
    let discontinuous = Clock {
        session: "process-2",
        wall_ms: 11,
        tick_ms: 11,
    };
    let (coverage, fresh) = source_evidence(&desk, &m, ALPHA, &discontinuous).unwrap();
    assert_eq!(coverage.state, "cached");
    assert!(!fresh);
    assert!(valid(&desk), "query_only evidence must remain untouched");
}
