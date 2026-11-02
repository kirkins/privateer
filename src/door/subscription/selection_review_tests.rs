//! Review regressions for inactive selection storage, not selector activation.
use super::*;

const CUSTODY: [&str; 6] = [
    "desk_subscription_selection",
    "desk_subscription_exclusion",
    "desk_subscription_exception",
    "desk_subscription_pin",
    "desk_subscription_history_ref",
    "desk_subscription_history_receipt",
];

fn seed(f: &Fixture, s: &str) {
    let sub = sub(f, s);
    let tx = begin(f);
    selection::exclude(&tx, &sub, &span(1, 10)).unwrap();
    selection::advance_revision(&tx).unwrap();
    selection::start_revisit(&tx, &sub).unwrap();
    let o = observation("request", 5, "open");
    selection::carry(&tx, &sub, &o, DeliveryClass::Unknown, "offer").unwrap();
    selection::offer_pin(&tx, &sub, &o, DeliveryClass::Unknown).unwrap();
    tx.execute(
        "INSERT INTO desk_subscription_history_ref VALUES(?1,?2,?3,?4,?5,'request',5,?6,0)",
        params![
            uuid(),
            s,
            sub.incarnation,
            CORE,
            BOX,
            serde_json::to_string(&o).unwrap()
        ],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO desk_subscription_history_receipt VALUES(?1,?2,?3,1000,NULL,NULL)",
        params![uuid(), s, sub.incarnation],
    )
    .unwrap();
    tx.commit().unwrap();
}

#[test]
fn reset_and_retirement_clear_custody_even_from_fence_unaware_writers() {
    for mode in ["reset", "retire", "v4-reset", "v4-retire"] {
        let f = Fixture::new();
        let s = f.open();
        let old = sub(&f, &s);
        seed(&f, &s);
        match mode {
            "reset" => {
                let c = DelegatedContext::resolved(
                    f.c.client().into(),
                    "door:alpha".into(),
                    vec!["mail:read".into()],
                    Some(vec!["alpha".into()]),
                );
                f.grant(&c);
                f.run_as(
                    &c,
                    Operation::Resume {
                        subscription: s.clone(),
                    },
                    100,
                )
                .unwrap();
                assert_ne!(sub(&f, &s).incarnation, old.incarnation);
            }
            "retire" => {
                f.conn
                    .execute(
                        "UPDATE door_clients SET revoked_ms=1 WHERE id=?1",
                        [f.c.client()],
                    )
                    .unwrap();
                let c = DelegatedContext::resolved(
                    uuid(),
                    "door:alpha".into(),
                    vec!["mail:read".into()],
                    None,
                );
                f.grant(&c);
                f.run_as(&c, Operation::Open, 100).unwrap();
            }
            "v4-reset" => {
                f.conn
                    .execute(
                        "UPDATE desk_subscription SET incarnation=?2,visibility='v4' WHERE id=?1",
                        params![s, uuid()],
                    )
                    .unwrap();
            }
            "v4-retire" => {
                f.conn
                    .execute("DELETE FROM desk_subscription WHERE id=?1", [&s])
                    .unwrap();
            }
            _ => unreachable!(),
        }
        for table in CUSTODY {
            assert_eq!(f.count(table), 0, "{mode}: {table}");
        }
        let tx = begin(&f);
        assert!(
            selection::exclude(&tx, &old, &span(1, 1)).is_err(),
            "{mode}"
        );
        tx.rollback().unwrap();
    }
}

#[test]
fn every_ledger_read_ignores_rows_from_a_different_incarnation() {
    let f = Fixture::new();
    let s = f.open();
    let current = sub(&f, &s);
    seed(&f, &s);
    // Deliberately retain stale rows independently of reset triggers, to prove
    // the SQL fences themselves, including nested selects and capacity reads.
    for table in CUSTODY {
        f.conn
            .execute(&format!("UPDATE {table} SET incarnation='old'"), [])
            .unwrap();
    }
    let stale = CUSTODY.map(|table| {
        dump(
            &f.conn,
            &format!("SELECT * FROM {table} WHERE incarnation='old'"),
        )
    });
    let tx = begin(&f);
    assert_eq!(selection::start_revisit(&tx, &current).unwrap(), 1);
    assert!(selection::revisit(&tx, &current).unwrap().is_none());
    selection::finish_revisit(&tx, &current).unwrap();
    // A current exclusion must neither merge with nor delete the old span.
    selection::exclude(&tx, &current, &span(4, 6)).unwrap();
    let o = observation("request", 5, "open");
    selection::carry(&tx, &current, &o, DeliveryClass::SinceSwitchOn, "proof").unwrap();
    selection::offer_pin(&tx, &current, &o, DeliveryClass::SinceSwitchOn).unwrap();
    let closed = observation("request", 6, "closed");
    selection::offer_pin(&tx, &current, &closed, DeliveryClass::Unknown).unwrap();
    selection::acknowledge_carried(&tx, &current, &o).unwrap();
    selection::acknowledge_closure(&tx, &current, &closed).unwrap();
    selection::retire_closed_pins(&tx, &current).unwrap();
    assert_eq!(
        tx.query_row(
            "SELECT COUNT(*) FROM desk_subscription_pin WHERE incarnation=?1",
            [&current.incarnation],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0,
        "old exceptions cannot hold a current pin"
    );
    for (table, before) in CUSTODY.into_iter().zip(stale) {
        assert_eq!(
            dump(
                &tx,
                &format!("SELECT * FROM {table} WHERE incarnation='old'")
            ),
            before,
            "{table}"
        );
    }
    tx.commit().unwrap();
}

#[test]
fn normal_ref_ack_punches_a_permanent_hole_and_uncarried_closure_releases_pin() {
    let f = Fixture::new();
    f.add("normal", 5, "alpha", Some("closed"));
    let s = f.open();
    let sub = sub(&f, &s);
    let tx = begin(&f);
    selection::exclude(&tx, &sub, &span(1, 10)).unwrap();
    tx.commit().unwrap();
    // Model a normal ref admitted after the span, without an exception. New
    // runtime admissions reserve carry; this exercises the lower primitive.
    let page = f.pull(&s, None, 1);
    let o = observation("normal", 5, "closed");
    let tx = begin(&f);
    selection::acknowledge_automatic(&tx, &sub, &o).unwrap();
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
    assert!(selection::exclude(&tx, &sub, &span(1, 10)).is_err());
    assert!(selection::carry(&tx, &sub, &o, DeliveryClass::SinceSwitchOn, "proof").is_err());
    selection::advance_revision(&tx).unwrap();
    selection::start_revisit(&tx, &sub).unwrap();
    while let Some(work) = selection::revisit(&tx, &sub).unwrap() {
        assert!(work.last < 5 || work.first > 5);
        selection::checkpoint(&tx, &sub, &work, work.last).unwrap();
    }
    selection::finish_revisit(&tx, &sub).unwrap();
    let open = observation("request", 11, "open");
    let closed = observation("request", 12, "closed");
    selection::offer_pin(&tx, &sub, &open, DeliveryClass::Unknown).unwrap();
    selection::offer_pin(&tx, &sub, &closed, DeliveryClass::Unknown).unwrap();
    selection::retire_closed_pins(&tx, &sub).unwrap();
    assert_eq!(
        tx.query_row("SELECT COUNT(*) FROM desk_subscription_pin", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    selection::acknowledge_closure(&tx, &sub, &closed).unwrap();
    selection::retire_closed_pins(&tx, &sub).unwrap();
    assert_eq!(
        tx.query_row("SELECT COUNT(*) FROM desk_subscription_pin", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        tx.query_row(
            "SELECT COUNT(*) FROM desk_subscription_exception",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0,
        "neither ack requires an exception"
    );
    tx.commit().unwrap();
}

#[test]
fn exceptions_and_pins_each_enforce_256_and_replays_cost_no_slots() {
    let f = Fixture::new();
    let s = f.open();
    let sub = sub(&f, &s);
    let tx = begin(&f);
    for n in 1..=256 {
        let o = observation(&format!("request-{n}"), n, "open");
        selection::carry(&tx, &sub, &o, DeliveryClass::Unknown, "offer").unwrap();
        selection::offer_pin(&tx, &sub, &o, DeliveryClass::Unknown).unwrap();
    }
    let changes = tx.total_changes();
    let repeat = observation("request-1", 1, "open");
    selection::carry(&tx, &sub, &repeat, DeliveryClass::Unknown, "offer").unwrap();
    selection::offer_pin(&tx, &sub, &repeat, DeliveryClass::Unknown).unwrap();
    assert_eq!(tx.total_changes(), changes);
    let extra = observation("extra", 257, "open");
    assert_eq!(
        selection::carry(&tx, &sub, &extra, DeliveryClass::Unknown, "offer")
            .unwrap_err()
            .to_string(),
        "backpressure"
    );
    assert_eq!(
        selection::offer_pin(&tx, &sub, &extra, DeliveryClass::Unknown)
            .unwrap_err()
            .to_string(),
        "backpressure"
    );
    assert_eq!(tx.total_changes(), changes, "capacity refusal is pre-write");
    selection::acknowledge_carried(&tx, &sub, &repeat).unwrap();
    selection::carry(&tx, &sub, &extra, DeliveryClass::Unknown, "offer").unwrap();
    let closed = observation("request-1", 258, "closed");
    selection::offer_pin(&tx, &sub, &closed, DeliveryClass::Unknown).unwrap();
    selection::acknowledge_closure(&tx, &sub, &closed).unwrap();
    selection::retire_closed_pins(&tx, &sub).unwrap();
    selection::offer_pin(&tx, &sub, &extra, DeliveryClass::Unknown).unwrap();
    tx.commit().unwrap();
    assert_eq!(f.count("desk_subscription_exception"), 256);
    assert_eq!(f.count("desk_subscription_pin"), 256);
}

#[test]
fn many_exceptions_enforce_cumulative_serialized_16_mib_envelope() {
    let f = Fixture::new();
    let s = f.open();
    let sub = sub(&f, &s);
    let tx = begin(&f);
    let mut admitted = 0;
    let mut expected_bytes = 0;
    for n in 1..=256 {
        let mut o = observation(&format!("large-{n}"), n, "closed");
        o.content["body"] = json!("x".repeat(70 * 1024));
        let size = serde_json::to_vec(&json!({"subscription":sub.id,"incarnation":sub.incarnation,"origin":o.origin,"observation":o,"delivery_class":"unknown","cause":"proof"})).unwrap().len() as i64;
        let changes = tx.total_changes();
        match selection::carry(&tx, &sub, &o, DeliveryClass::Unknown, "proof") {
            Ok(()) => {
                admitted += 1;
                expected_bytes += size;
            }
            Err(e) => {
                assert_eq!(e.to_string(), "backpressure");
                assert!(admitted > 1 && admitted < 256, "byte bound, not row bound");
                assert!(expected_bytes <= 16 * 1024 * 1024);
                assert!(expected_bytes + size > 16 * 1024 * 1024);
                assert_eq!(tx.total_changes(), changes);
                break;
            }
        }
    }
    assert_eq!(
        tx.query_row(
            "SELECT SUM(bytes) FROM desk_subscription_exception",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        expected_bytes
    );
    assert!(admitted < 256);
    tx.rollback().unwrap();
    assert_eq!(f.count("desk_subscription_exception"), 0);
}
