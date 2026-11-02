//! Review regressions at the serving boundary, not hand-written progress.
use super::*;

#[test]
fn sparse_excluded_reader_rows_do_not_bury_a_fresh_request() {
    for earlier_ack in [false, true] {
        let f = Fixture::new();
        enable(&f, "switch_on_pending", "");
        let s = f.open();
        if earlier_ack {
            f.add("earlier", 1, "alpha", Some("open"));
            let page = f.pull(&s, None, ITEMS);
            f.run(f.ack(&s, &page, references(&page)));
        }
        for n in 1..=600 {
            f.fact(&format!("notice-{n}"), n * 2, "alpha", None);
        }
        f.add("fresh", 1201, "alpha", Some("open"));
        let page = f.pull(&s, None, ITEMS);
        assert_eq!(ids(&page), BTreeSet::from(["fresh".into()]));
        assert_eq!(f.count("desk_subscription_exclusion"), 1);
        f.run(f.ack(&s, &page, references(&page)));
        assert!(ids(&f.pull(&s, None, ITEMS)).is_empty());
    }
}

#[test]
fn merged_sparse_span_revisits_late_proof_and_reserves_its_ack_split() {
    let f = Fixture::new();
    enable(&f, "switch_on_pending", "");
    f.fact("left", 2, "alpha", None);
    f.fact("right", 4, "alpha", None);
    f.add("boundary", 5, "alpha", Some("open"));
    let s = f.open();
    let page = f.pull(&s, None, ITEMS);
    assert_eq!(f.count("desk_subscription_exclusion"), 1);
    f.run(f.ack(&s, &page, references(&page)));
    // Even a late copied observation within an enclosed gap is not an ack.
    // A real new proof advances this same revision before bounded revisit.
    f.add("late", 3, "alpha", Some("open"));
    let tx = f.conn.unchecked_transaction().unwrap();
    selection::advance_revision(&tx).unwrap();
    tx.commit().unwrap();
    let page = f.pull(&s, None, ITEMS);
    assert_eq!(ids(&page), BTreeSet::from(["late".into()]));
    assert_eq!(f.count("desk_subscription_exception"), 1);
    f.run(f.ack(&s, &page, references(&page)));
    assert_eq!(f.count("desk_subscription_exclusion"), 2);
    assert!(ids(&f.pull(&s, None, ITEMS)).is_empty());
}

#[test]
fn visibility_expansion_invalidates_merged_sparse_evidence() {
    let f = Fixture::new();
    enable(&f, "switch_on_pending", "");
    f.fact("left", 2, "alpha", None);
    f.add("hidden", 3, "beta", Some("open"));
    f.fact("right", 4, "alpha", None);
    f.add("boundary", 5, "alpha", Some("open"));
    let c = DelegatedContext::resolved(
        f.c.client().into(),
        "door:alpha".into(),
        vec!["mail:read".into()],
        Some(vec!["alpha".into()]),
    );
    f.grant(&c);
    let s = f.run_as(&c, Operation::Open, 100).unwrap()["subscription"]
        .as_str()
        .unwrap()
        .to_owned();
    let page = f
        .run_as(
            &c,
            Operation::Pull {
                subscription: s.clone(),
                continuation: None,
                items: ITEMS,
            },
            100,
        )
        .unwrap();
    assert_eq!(ids(&page), BTreeSet::from(["boundary".into()]));
    assert_eq!(f.count("desk_subscription_exclusion"), 1);
    f.run_as(&c, f.ack(&s, &page, references(&page)), 100)
        .unwrap();
    let old = sub(&f, &s).incarnation;
    f.grant(&f.c);
    let expanded = f.pull(&s, None, ITEMS);
    assert!(ids(&expanded).contains("hidden"));
    assert_ne!(sub(&f, &s).incarnation, old);
}

#[test]
fn full_unmergeable_above_prefix_evidence_cannot_starve_offers() {
    let f = Fixture::new();
    enable(&f, "switch_on_pending", "");
    let s = f.open();
    let subscription = sub(&f, &s);
    let tx = f.conn.unchecked_transaction().unwrap();
    for n in 0..512 {
        let mut span = selection_tests::span(2, 2);
        span.mailbox = format!("other-{n}");
        selection::exclude(&tx, &subscription, &span).unwrap();
    }
    tx.commit().unwrap();
    f.add("fresh", 1201, "alpha", Some("open"));
    let page = f.pull(&s, None, ITEMS);
    assert_eq!(ids(&page), BTreeSet::from(["fresh".into()]));
    f.run(f.ack(&s, &page, references(&page)));
}

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
            // Unrelated journal events consume the even orders. Alternate
            // sparse exclusions with actual offer/closure/ack holes.
            let base = n * 8;
            f.add(&format!("request-{n}"), base + 1, "alpha", Some("open"));
            f.fact(&format!("notice-{n}"), base + 3, "alpha", None);
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
            f.add(&format!("request-{n}"), base + 5, "alpha", Some("closed"));
            f.fact(&format!("closure-notice-{n}"), base + 7, "alpha", None);
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
    assert_eq!(
        ids(&page),
        BTreeSet::from(["first".into(), "second".into()])
    );
    assert!(f.count("desk_subscription_exclusion") + f.count("desk_subscription_exception") <= 512);
    // Reconstructible above-prefix evidence can be recycled to fit both offers;
    // neither may overcommit the exclusions made during the same scan.
    f.run(f.ack(&s, &page, references(&page)));
    assert!(ids(&f.pull(&s, None, ITEMS)).is_empty());
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
fn acknowledged_open_pin_pressure_admits_new_requests_without_dropping_unseen_custody() {
    // 0: all acknowledged; 1: oldest offer unacked; 2: observed pending closure.
    for (bound, protected) in [(true, 0), (true, 1), (true, 2), (false, 0)] {
        let f = Fixture::new();
        if bound {
            enable(&f, "switch_on_pending", "");
        }
        for n in 0..256 {
            f.add(&format!("request-{n}"), n + 1, "alpha", Some("open"));
        }
        let s = f.open();
        for _ in 0..4 {
            let page = f.pull(&s, None, ITEMS);
            // Drain the whole frozen snapshot, not only its first page.
            let mut page = page;
            loop {
                f.run(f.ack(&s, &page, references(&page)));
                let Some(token) = next(&page) else { break };
                page = f.pull(&s, Some(token), ITEMS);
            }
            if f.count("desk_subscription_pin") == 256 {
                break;
            }
        }
        assert_eq!(f.count("desk_subscription_pin"), 256);
        if protected == 1 {
            f.add("request-0", 257, "alpha", Some("open"));
            f.pull(&s, None, ITEMS); // Deliberately leave this newer offer unacked.
        }
        if protected == 2 {
            f.add("request-0", 257, "alpha", Some("closed"));
        }
        f.add("fresh", 258, "alpha", Some("open"));
        let page = f.pull(&s, None, ITEMS);
        assert!(ids(&page).contains("fresh"));
        assert_eq!(page["replay_selection"]["closure_pins_demoted"], 1);
        assert_eq!(f.count("desk_subscription_pin"), 256);
        let demoted = if protected == 0 {
            "request-0"
        } else {
            "request-1"
        };
        let pinned: bool = f
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM desk_subscription_pin WHERE mail=?1)",
                [demoted],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!pinned, "the oldest safe acknowledged pin is demoted");
        if protected != 0 {
            assert!(ids(&page).contains("request-0"));
            assert!(
                f.conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM desk_subscription_pin WHERE mail='request-0')",
                        [],
                        |r| r.get::<_, bool>(0)
                    )
                    .unwrap()
            );
        } else {
            f.run(f.ack(&s, &page, references(&page)));
            let lookup = f.run(Operation::Thread {
                subscription: s.clone(),
                origin: MailIdentity::new(CORE, BOX, demoted).unwrap(),
                continuation: None,
                items: ITEMS,
            });
            assert!(ids(&lookup).contains(demoted));
            f.run(f.ack(&s, &lookup, references(&lookup)));
            f.add(demoted, 259, "alpha", Some("closed"));
            assert!(
                !ids(&f.pull(&s, None, ITEMS)).contains(demoted),
                "demotion visibly relinquished this later automatic closure"
            );
        }
    }
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
