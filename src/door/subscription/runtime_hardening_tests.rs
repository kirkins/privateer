//! Prepared-read fences and bounded/atomic maintenance, still not activation.
use super::*;
use selection::runtime::{Choice, Read};

#[test]
fn indeterminate_read_cannot_exclude_checkpoint_or_spend_a_revision() {
    let f = Fixture::new();
    let s = f.open();
    let sub = sub(&f, &s);
    let tx = begin(&f);
    selection::exclude(&tx, &sub, &span(1, 20)).unwrap();
    selection::advance_revision(&tx).unwrap();
    let mut read = Read::prepare(&tx, &sub, &f.m, &f.c).unwrap();
    read.defer_validation_for_test();
    let o = observation("deferred", 10, "closed");
    let spans = dump(&tx, "SELECT * FROM desk_subscription_exclusion");
    let state = dump(&tx, "SELECT * FROM desk_subscription_selection");
    let before = tx.total_changes();
    assert_eq!(read.choice(&o).unwrap(), Choice::Indeterminate);
    assert_eq!(read.exclude(&o).unwrap_err().to_string(), "indeterminate");
    assert!(
        !read.revisit(128, 50).unwrap(),
        "even an empty span cannot finish"
    );
    assert_eq!(tx.total_changes(), before);
    assert_eq!(
        dump(&tx, "SELECT * FROM desk_subscription_exclusion"),
        spans
    );
    assert_eq!(
        dump(&tx, "SELECT * FROM desk_subscription_selection"),
        state
    );
    drop(read);
    let read = Read::prepare(&tx, &sub, &f.m, &f.c).unwrap();
    assert!(
        read.revisit(128, 50).unwrap(),
        "a later determinate read can finish"
    );
    drop(read);
    tx.commit().unwrap();
}

#[test]
fn revisit_counts_empty_spans_toward_the_fifty_span_limit() {
    let f = Fixture::new();
    let s = f.open();
    let sub = sub(&f, &s);
    let tx = begin(&f);
    for n in 0..51 {
        selection::exclude(&tx, &sub, &span(1 + n * 2, 1 + n * 2)).unwrap();
    }
    selection::advance_revision(&tx).unwrap();
    let read = Read::prepare(&tx, &sub, &f.m, &f.c).unwrap();
    assert!(read.revisit(128, 51).is_err());
    assert!(!read.revisit(128, 50).unwrap());
    assert_eq!(
        tx.query_row(
            "SELECT COUNT(*) FROM desk_subscription_exclusion WHERE checked_revision=1",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        50
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
    assert!(read.revisit(128, 50).unwrap());
    drop(read);
    tx.commit().unwrap();
}

#[test]
fn refresh_closures_rolls_back_partial_carries_and_preserves_original_pins_on_pressure() {
    let f = Fixture::new();
    let s = f.open();
    let sub = sub(&f, &s);
    let tx = begin(&f);
    // Two pinned closures, but only one spare exception slot: even if the
    // first closure reserves successfully, the whole refresh must roll back.
    for n in 0..255 {
        selection::carry(
            &tx,
            &sub,
            &observation(&format!("reserved-{n}"), 100 + n, "open"),
            DeliveryClass::Unknown,
            "proof",
        )
        .unwrap();
    }
    for id in ["first", "second"] {
        selection::offer_pin(
            &tx,
            &sub,
            &observation(id, 1, "open"),
            DeliveryClass::Unknown,
        )
        .unwrap();
        f.add(id, 2, "alpha", Some("closed"));
    }
    let pins = dump(&tx, "SELECT * FROM desk_subscription_pin ORDER BY mail");
    let exceptions = dump(
        &tx,
        "SELECT * FROM desk_subscription_exception ORDER BY mail",
    );
    let read = Read::prepare(&tx, &sub, &f.m, &f.c).unwrap();
    assert_eq!(
        read.refresh_closures().unwrap_err().to_string(),
        "backpressure"
    );
    assert_eq!(
        dump(&tx, "SELECT * FROM desk_subscription_pin ORDER BY mail"),
        pins
    );
    assert_eq!(
        dump(
            &tx,
            "SELECT * FROM desk_subscription_exception ORDER BY mail"
        ),
        exceptions
    );
    drop(read);
    // The caller may keep unrelated outer work after the atomic call fails.
    tx.commit().unwrap();
    assert_eq!(f.count("desk_subscription_exception"), 255);
}

#[test]
fn hidden_origins_never_gain_revisit_custody_or_refreshed_closures() {
    let f = Fixture::new();
    let s = f.open();
    let sub = sub(&f, &s);
    f.add("hidden-new", 10, "beta", Some("open"));
    f.add("hidden-closure", 11, "beta", Some("closed"));
    f.conn
        .execute(
            "UPDATE desk_intake_fact SET content=json_set(content,'$.kind','request')",
            [],
        )
        .unwrap();
    let restricted = DelegatedContext::resolved(
        f.c.client().into(),
        "door:alpha".into(),
        vec!["mail:read".into()],
        Some(vec!["alpha".into()]),
    );
    let tx = begin(&f);
    selection::offer_pin(
        &tx,
        &sub,
        &observation("hidden-closure", 1, "open"),
        DeliveryClass::Unknown,
    )
    .unwrap();
    selection::exclude(&tx, &sub, &span(1, 20)).unwrap();
    selection::advance_revision(&tx).unwrap();
    let pins = dump(&tx, "SELECT * FROM desk_subscription_pin");
    let read = Read::prepare(&tx, &sub, &f.m, &restricted).unwrap();
    assert!(read.revisit(128, 50).unwrap());
    read.refresh_closures().unwrap();
    for id in ["hidden-new", "hidden-closure"] {
        let o = intake::observation(&tx, &f.m, &MailIdentity::new(CORE, BOX, id).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(read.choice(&o).unwrap(), Choice::Hidden);
    }
    assert_eq!(dump(&tx, "SELECT * FROM desk_subscription_pin"), pins);
    assert_eq!(
        tx.query_row(
            "SELECT COUNT(*) FROM desk_subscription_exception",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    drop(read);
    tx.commit().unwrap();
}

#[test]
fn visibility_reset_invalidates_a_runtime_read_and_all_its_maintenance_methods() {
    let f = Fixture::new();
    let s = f.open();
    let sub = sub(&f, &s);
    let o = observation("stale", 1, "closed");
    let tx = begin(&f);
    let read = Read::prepare(&tx, &sub, &f.m, &f.c).unwrap();
    let restricted = DelegatedContext::resolved(
        f.c.client().into(),
        "door:alpha".into(),
        vec!["mail:read".into()],
        Some(vec!["alpha".into()]),
    );
    run_admitted(
        &tx,
        &restricted,
        &f.m,
        Operation::Resume { subscription: s },
        100,
    )
    .unwrap();
    for result in [
        read.choice(&o).map(|_| ()),
        read.offer(&o).map(|_| ()),
        read.exclude(&o),
        read.refresh_closures(),
        read.revisit(128, 50).map(|_| ()),
    ] {
        assert_eq!(result.unwrap_err().to_string(), "stale_or_unknown");
    }
    drop(read);
    tx.commit().unwrap();
}

#[test]
fn proof_revision_change_invalidates_runtime_classification_and_maintenance() {
    let f = Fixture::new();
    enable(&f, "established", &format!("{}:10", uuid()));
    let s = f.open();
    let sub = sub(&f, &s);
    let tx = begin(&f);
    let read = Read::prepare(&tx, &sub, &f.m, &f.c).unwrap();
    selection::advance_revision(&tx).unwrap();
    let o = observation("stale", 1, "closed");
    for result in [
        read.choice(&o).map(|_| ()),
        read.exclude(&o),
        read.refresh_closures(),
        read.revisit(128, 50).map(|_| ()),
    ] {
        assert_eq!(result.unwrap_err().to_string(), "stale_or_unknown");
    }
    drop(read);
    tx.commit().unwrap();
}
