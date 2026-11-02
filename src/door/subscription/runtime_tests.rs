//! Focused unhooked-runtime evidence, not 2B2 serving activation.
use super::*;
use selection::runtime::{self, Choice, Read};

#[test]
fn unacked_normal_ref_cannot_be_excluded_and_its_ack_succeeds_at_full_span_capacity() {
    let f = Fixture::new();
    f.add("normal", 5, "alpha", Some("closed"));
    let s = f.open();
    let sub = sub(&f, &s);
    let page = f.pull(&s, None, 1);
    let o = intake::observation(
        &f.conn,
        &f.m,
        &MailIdentity::new(CORE, BOX, "normal").unwrap(),
    )
    .unwrap()
    .unwrap();
    let tx = begin(&f);
    assert!(selection::exclude(&tx, &sub, &span(1, 10)).is_err());
    for n in 0..512 {
        selection::exclude(&tx, &sub, &span(100 + n * 2, 100 + n * 2)).unwrap();
    }
    runtime::acknowledge(&tx, &sub, &o).unwrap();
    ack::apply(
        &tx,
        &sub,
        &f.m,
        &f.c,
        page["ack_key"].as_str().unwrap(),
        references(&page),
        100,
    )
    .unwrap();
    assert_eq!(through(&tx, &sub, &o).unwrap(), 5);
    assert_eq!(
        tx.query_row(
            "SELECT COUNT(*) FROM desk_subscription_exclusion",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        512
    );
    tx.commit().unwrap();
}

#[test]
fn bounded_revisit_backpressures_without_consuming_target_and_recovers_after_restart() {
    let f = Fixture::new();
    let s = f.open();
    for n in 1..=257 {
        f.add(&format!("request-{n}"), n, "alpha", Some("open"));
    }
    f.conn
        .execute(
            "UPDATE desk_intake_fact SET content=json_set(content,'$.kind','request')",
            [],
        )
        .unwrap();
    let sub = sub(&f, &s);
    let tx = begin(&f);
    selection::exclude(&tx, &sub, &span(1, 300)).unwrap();
    tx.execute(
        "INSERT INTO desk_subscription_progress VALUES(?1,?2,?3,300)",
        params![s, CORE, BOX],
    )
    .unwrap();
    selection::advance_revision(&tx).unwrap();
    let read = Read::prepare(&tx, &sub, &f.m, &f.c).unwrap();
    assert!(!read.revisit(128, 50).unwrap());
    assert!(!read.revisit(128, 50).unwrap());
    assert!(
        !read.revisit(128, 50).unwrap(),
        "257th exception has no capacity"
    );
    assert_eq!(
        tx.query_row(
            "SELECT consumed_revision,target_revision FROM desk_subscription_selection",
            [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        )
        .unwrap(),
        (0, 1)
    );
    drop(read);
    tx.commit().unwrap();
    assert_eq!(f.count("desk_subscription_exception"), 256);
    assert_eq!(
        f.count("desk_subscription_pin"),
        0,
        "proof reservation alone never offers an origin"
    );
    let dir = tempfile::tempdir().unwrap();
    let reopened = reopen(&f, &dir.path().join("alpha.sqlite"));
    let tx = begin(&reopened);
    let first = intake::observation(
        &tx,
        &reopened.m,
        &MailIdentity::new(CORE, BOX, "request-1").unwrap(),
    )
    .unwrap()
    .unwrap();
    runtime::acknowledge(&tx, &sub, &first).unwrap();
    let read = Read::prepare(&tx, &sub, &reopened.m, &reopened.c).unwrap();
    assert!(read.revisit(128, 50).unwrap());
    assert_eq!(
        read.choice(&first).unwrap(),
        Choice::Acknowledged,
        "the punched prefix hole is permanent"
    );
    let before = tx.total_changes();
    assert!(read.revisit(128, 50).unwrap());
    // The same proof revision neither allocates another exception nor even
    // rewrites the completed watermarks; it cannot resurrect the actual ack.
    assert_eq!(
        tx.query_row(
            "SELECT COUNT(*) FROM desk_subscription_exception",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        256
    );
    assert_eq!(tx.total_changes(), before);
    drop(read);
    tx.commit().unwrap();
}

#[test]
fn unknown_incoming_offers_freeze_labels_and_pin_closure_without_history_or_outgoing_pins() {
    let f = Fixture::new();
    for id in ["incoming", "decision", "outgoing", "notice"] {
        f.add(id, 1, "alpha", Some("open"));
    }
    f.conn.execute("UPDATE desk_intake_fact SET content=json_set(content,'$.kind',CASE id WHEN 'decision' THEN 'decision' WHEN 'notice' THEN 'notice' ELSE 'request' END)",[]).unwrap();
    f.conn.execute("UPDATE desk_intake_fact SET content=json_set(content,'$.from','captain','$.to','alpha') WHERE id='outgoing'",[]).unwrap();
    let s = f.open();
    let sub = sub(&f, &s);
    let get = |id: &str| {
        intake::observation(&f.conn, &f.m, &MailIdentity::new(CORE, BOX, id).unwrap())
            .unwrap()
            .unwrap()
    };
    let open = get("incoming");
    let tx = begin(&f);
    let read = Read::prepare(&tx, &sub, &f.m, &f.c).unwrap();
    assert_eq!(
        read.choice(&get("outgoing")).unwrap(),
        Choice::Excluded(selection::UNKNOWN)
    );
    assert_eq!(
        read.choice(&get("notice")).unwrap(),
        Choice::Excluded(selection::UNKNOWN)
    );
    assert_eq!(
        read.choice(&get("decision")).unwrap(),
        Choice::Eligible(DeliveryClass::Unknown)
    );
    assert_eq!(read.offer(&open).unwrap(), DeliveryClass::Unknown);
    drop(read);
    tx.commit().unwrap();
    assert_eq!(f.count("desk_subscription_pin"), 1);
    assert_eq!(
        f.count("desk_subscription_ref"),
        0,
        "helpers do not expose refs"
    );
    f.add("incoming", 2, "alpha", Some("closed"));
    let closed = get("incoming");
    let tx = begin(&f);
    let read = Read::prepare(&tx, &sub, &f.m, &f.c).unwrap();
    read.refresh_closures().unwrap();
    assert_eq!(
        read.choice(&closed).unwrap(),
        Choice::Eligible(DeliveryClass::Unknown)
    );
    assert_eq!(read.offer(&closed).unwrap(), DeliveryClass::Unknown);
    drop(read);
    assert_eq!(
        tx.query_row(
            "SELECT COUNT(*) FROM desk_subscription_exception",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    runtime::acknowledge(&tx, &sub, &closed).unwrap();
    assert_eq!(
        tx.query_row("SELECT COUNT(*) FROM desk_subscription_pin", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1,
        "displaced open still holds custody"
    );
    runtime::acknowledge(&tx, &sub, &open).unwrap();
    assert_eq!(
        tx.query_row("SELECT COUNT(*) FROM desk_subscription_pin", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        intake::observation(&tx, &f.m, &open.origin)
            .unwrap()
            .unwrap()
            .state
            .as_deref(),
        Some("closed"),
        "raw custody ack never changes lifecycle"
    );
    tx.commit().unwrap();
}

#[test]
fn native_and_copy_first_proof_upgrade_skipped_origins_below_compaction_without_serving_hooks() {
    for copy in [false, true] {
        let mut f = intake::ReplayFixture::remote();
        super::super::super::super::init(&f.desk).unwrap();
        // The public crate initializer is used explicitly, not an engine
        // operation. This fixture's grant does not gain an exposed command.
        let b = json!({"domain":f.m.domain,"owner_core":f.m.owner_core,"generation":f.m.generation,"incarnation":f.m.incarnation,"manifest_revision":f.m.manifest_revision,"manifest_hash":crate::core::hash(&json!(f.m)),"mode":"fresh_replay","replay_policy":"open_and_new","switch_on_ms":1000,"switch_on":[]});
        f.desk.execute_batch("CREATE TABLE door_desk_exposure(door TEXT PRIMARY KEY,enabled INTEGER,binding TEXT)").unwrap();
        f.desk
            .execute(
                "INSERT INTO door_desk_exposure VALUES('alpha',1,?1)",
                [b.to_string()],
            )
            .unwrap();
        f.import_latest(false);
        f.capture_boundary(&b);
        let sub = Subscription {
            id: uuid(),
            incarnation: uuid(),
        };
        f.desk
            .execute(
                "INSERT INTO desk_subscription VALUES(?1,?2,'reader','owner',?3,'visibility')",
                params![sub.id, f.m.domain, sub.incarnation],
            )
            .unwrap();
        f.desk
            .execute(
                "INSERT INTO desk_subscription_replay VALUES(?1,?2)",
                params![sub.id, b.to_string()],
            )
            .unwrap();
        let c =
            DelegatedContext::resolved(uuid(), "door:alpha".into(), vec!["mail:read".into()], None);
        f.send("late");
        let wire = f.stage_latest(copy);
        let (core, mailbox, id): (String, String, String) = f
            .desk
            .query_row(
                "SELECT core,mailbox,id FROM desk_intake_fact WHERE id='late'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        let o = intake::observation(
            &f.desk,
            &f.m,
            &MailIdentity::new(&core, &mailbox, &id).unwrap(),
        )
        .unwrap()
        .unwrap();
        let tx =
            rusqlite::Transaction::new_unchecked(&f.desk, TransactionBehavior::Immediate).unwrap();
        let read = Read::prepare(&tx, &sub, &f.m, &c).unwrap();
        assert_eq!(
            read.choice(&o).unwrap(),
            Choice::Excluded(selection::UNKNOWN)
        );
        read.exclude(&o).unwrap();
        drop(read);
        tx.execute(
            "INSERT INTO desk_subscription_progress VALUES(?1,?2,?3,?4)",
            params![sub.id, core, mailbox, o.order],
        )
        .unwrap();
        tx.commit().unwrap();
        f.adopt(&wire).unwrap();
        let tx =
            rusqlite::Transaction::new_unchecked(&f.desk, TransactionBehavior::Immediate).unwrap();
        let read = Read::prepare(&tx, &sub, &f.m, &c).unwrap();
        assert!(read.revisit(128, 50).unwrap());
        assert_eq!(
            read.choice(&o).unwrap(),
            Choice::Eligible(DeliveryClass::SinceSwitchOn)
        );
        assert_eq!(read.offer(&o).unwrap(), DeliveryClass::SinceSwitchOn);
        assert_eq!(
            tx.query_row("SELECT COUNT(*) FROM desk_subscription_ref", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0,
            "no serving hook"
        );
        drop(read);
        runtime::acknowledge(&tx, &sub, &o).unwrap();
        tx.commit().unwrap();
        f.adopt(&wire).unwrap();
        let tx =
            rusqlite::Transaction::new_unchecked(&f.desk, TransactionBehavior::Immediate).unwrap();
        let read = Read::prepare(&tx, &sub, &f.m, &c).unwrap();
        assert!(read.revisit(128, 50).unwrap());
        assert_eq!(read.choice(&o).unwrap(), Choice::Acknowledged);
        drop(read);
        tx.commit().unwrap();
    }
}
