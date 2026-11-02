//! Review regressions at the serving boundary, not hand-written progress.
use super::*;

#[test]
fn interleaved_policy_exclusions_keep_serving_beyond_the_span_cap() {
    for bound in [false, true] {
        let f = Fixture::new();
        if bound {
            enable(&f, "switch_on_pending", "");
        }
        let s = f.open();
        let mut retired = 0;
        for n in 0..300 {
            let base = n * 4;
            f.add(&format!("request-{n}"), base + 1, "alpha", Some("open"));
            f.fact(&format!("notice-{n}"), base + 2, "alpha", None);
            // Advance the injected engine clock between rounds: this test
            // targets span retention, not the independent receipt TTL bound.
            let now = 100 + base * (RECEIPT_TTL + 1);
            let page = f
                .run_as(
                    &f.c,
                    Operation::Pull {
                        subscription: s.clone(),
                        continuation: None,
                        items: ITEMS,
                    },
                    now,
                )
                .unwrap();
            assert_eq!(ids(&page), BTreeSet::from([format!("request-{n}")]));
            retired += page["replay_selection"]["exclusion_spans_retired"]
                .as_u64()
                .unwrap_or(0);
            f.run_as(&f.c, f.ack(&s, &page, references(&page)), now)
                .unwrap();
            f.add(&format!("request-{n}"), base + 3, "alpha", Some("closed"));
            f.fact(&format!("closure-notice-{n}"), base + 4, "alpha", None);
            let now = now + RECEIPT_TTL + 1;
            let page = f
                .run_as(
                    &f.c,
                    Operation::Pull {
                        subscription: s.clone(),
                        continuation: None,
                        items: ITEMS,
                    },
                    now,
                )
                .unwrap();
            retired += page["replay_selection"]["exclusion_spans_retired"]
                .as_u64()
                .unwrap_or(0);
            assert_eq!(ids(&page), BTreeSet::from([format!("request-{n}")]));
            f.run_as(&f.c, f.ack(&s, &page, references(&page)), now)
                .unwrap();
        }
        assert!(f.count("desk_subscription_exclusion") <= 512);
        if bound {
            assert!(retired > 0);
        } else {
            assert_eq!(f.count("desk_subscription_exclusion"), 0);
        }
    }
}

#[test]
fn fresh_scan_charges_exclusions_before_offering_the_retained_prefix() {
    let f = Fixture::new();
    enable(&f, "switch_on_pending", "");
    let s = f.open();
    let sub = sub(&f, &s);
    let tx = f.conn.unchecked_transaction().unwrap();
    for n in 0..510 {
        selection::exclude(&tx, &sub, &selection_tests::span(1 + 2 * n, 1 + 2 * n)).unwrap();
    }
    tx.commit().unwrap();
    f.add("first", 1021, "alpha", Some("open"));
    f.fact("notice", 1022, "alpha", None);
    f.add("second", 1023, "alpha", Some("open"));
    let page = f.pull(&s, None, ITEMS);
    assert!(ids(&page).contains("first"));
    assert_eq!(page["replay_selection"]["bounded_work_pending"], true);
    f.run(f.ack(&s, &page, references(&page)));
    let page = f.pull(&s, None, ITEMS);
    assert!(
        ids(&page).contains("second"),
        "ack of the partial prefix frees admission"
    );
    f.run(f.ack(&s, &page, references(&page)));
}

#[test]
fn activated_null_thread_reserves_split_and_does_not_fabricate_an_ack() {
    let f = Fixture::new();
    for n in 1..=3 {
        f.fact(&format!("notice-{n}"), n, "alpha", None);
    }
    f.add("incoming", 4, "alpha", Some("open"));
    let s = f.open();
    // Retained exclusions from the earlier activated engine remain valid on
    // upgrade even though new NULL readers no longer create skipped evidence.
    let sub = sub(&f, &s);
    let tx = f.conn.unchecked_transaction().unwrap();
    selection::exclude(&tx, &sub, &selection_tests::span(1, 3)).unwrap();
    tx.commit().unwrap();
    let page = f.pull(&s, None, ITEMS);
    f.run(f.ack(&s, &page, references(&page)));
    let lookup = f.run(Operation::Thread {
        subscription: s.clone(),
        origin: MailIdentity::new(CORE, BOX, "notice-2").unwrap(),
        continuation: None,
        items: ITEMS,
    });
    let reference = &references(&lookup)[0];
    let acked: bool = f
        .conn
        .query_row(
            "SELECT acked FROM desk_subscription_ref WHERE ref=?1",
            [reference],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!acked, "a policy skip is not an automatic acknowledgment");
    assert_eq!(f.count("desk_subscription_exception"), 1);
    let tx = f.conn.unchecked_transaction().unwrap();
    // One span and one exception reservation; fill every remaining slot.
    for n in 0..510 {
        selection::exclude(&tx, &sub, &selection_tests::span(10 + 2 * n, 10 + 2 * n)).unwrap();
    }
    tx.commit().unwrap();
    f.run(f.ack(&s, &lookup, references(&lookup)));
    assert_eq!(f.count("desk_subscription_exception"), 0);
    assert_eq!(f.count("desk_subscription_exclusion"), 512);
}

#[test]
fn exclusion_pressure_cannot_roll_back_an_explicit_ack() {
    let f = Fixture::new();
    enable(&f, "switch_on_pending", "");
    f.add("incoming", 1025, "alpha", Some("open"));
    let s = f.open();
    let page = f.pull(&s, None, ITEMS);
    let subscription = sub(&f, &s);
    let tx = f.conn.unchecked_transaction().unwrap();
    for n in 0..511 {
        selection::exclude(
            &tx,
            &subscription,
            &selection_tests::span(1 + 2 * n, 1 + 2 * n),
        )
        .unwrap();
    }
    tx.commit().unwrap();
    f.fact("new-exclusion", 1026, "alpha", None);
    f.add("next-request", 1027, "alpha", Some("open"));
    f.fact("another-exclusion", 1028, "alpha", None);
    // The exception's split reservation fills slot 512 before the ack. Its
    // release only makes room for one of the two fresh exclusion runs.
    f.run(f.ack(&s, &page, references(&page)));
    assert_eq!(f.count("desk_subscription_exception"), 0);
    let next = f.pull(&s, None, ITEMS);
    assert_eq!(ids(&next), BTreeSet::from(["next-request".into()]));
}

#[test]
fn bound_thread_can_reclaim_drained_automatic_snapshot_without_changing_custody() {
    let f = Fixture::new();
    enable(&f, "switch_on_pending", "");
    f.add("incoming", 1, "alpha", Some("open"));
    f.fact("history", 2, "alpha", Some("closed"));
    let s = f.open();
    let page = f.pull(&s, None, ITEMS);
    f.run(f.ack(&s, &page, references(&page)));
    f.pull(&s, None, ITEMS); // Two drained automatic snapshots.
    assert_eq!(f.count("desk_subscription_snapshot"), 2);
    let before = f.count("desk_subscription_pin");
    let lookup = f.run(Operation::Thread {
        subscription: s,
        origin: MailIdentity::new(CORE, BOX, "history").unwrap(),
        continuation: None,
        items: ITEMS,
    });
    assert_eq!(ids(&lookup), BTreeSet::from(["history".into()]));
    assert_eq!(f.count("desk_subscription_pin"), before);
}
