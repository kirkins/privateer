//! Multi-round R3 regressions from review-a98b3762 at the serving boundary.
use super::*;

fn notice_in(f: &Fixture, mailbox: &str, id: &str, order: i64) {
    f.fact(id, order, "alpha", None);
    f.conn
        .execute(
            "UPDATE desk_intake_fact SET mailbox=?1 WHERE id=?2",
            params![mailbox, id],
        )
        .unwrap();
}

#[test]
fn ack_progress_survives_520_mailboxes_sorted_before_the_reader_for_300_rounds() {
    for (group, rounds) in [("3000", 300), ("5000", 30)] {
        let f = Fixture::new();
        enable(&f, "switch_on_pending", "");
        let s = f.open();
        for n in 0..520 {
            let mailbox = format!("00000000-0000-{group}-8000-{n:012x}");
            notice_in(&f, &mailbox, &format!("other-{n}"), 2 * n + 2);
        }
        for round in 0..rounds {
            let order = 2003 + 4 * round;
            let request = format!("request-{round}");
            f.fact(&format!("notice-{round}"), order - 2, "alpha", None);
            f.add(&request, order, "alpha", Some("open"));
            // Exercise receipt expiry deterministically, not by sleeping or
            // relying on wall/boot uptime. Open-pin pressure also crosses 256.
            let now = 100 + round * (RECEIPT_TTL + 1);
            let mut delivered = false;
            for _ in 0..6 {
                let mut continuation = None;
                loop {
                    let page = f
                        .run_as(
                            &f.c,
                            Operation::Pull {
                                subscription: s.clone(),
                                continuation,
                                items: ITEMS,
                            },
                            now,
                        )
                        .unwrap();
                    delivered |= ids(&page).contains(&request);
                    let refs = references(&page);
                    if !refs.is_empty() {
                        f.run_as(&f.c, f.ack(&s, &page, refs), now).unwrap();
                    }
                    continuation = next(&page);
                    if continuation.is_none() {
                        break;
                    }
                }
                if delivered {
                    break;
                }
            }
            assert!(delivered, "group={group}, round={round}");
            let prefix: i64 = f.conn.query_row(
                "SELECT through_order FROM desk_subscription_progress WHERE subscription=?1 AND core=?2 AND mailbox=?3",
                params![s, CORE, BOX], |r| r.get(0),
            ).unwrap();
            assert!(
                prefix >= order,
                "group={group}, round={round}, prefix={prefix}"
            );
            assert_eq!(f.count("desk_subscription_ref"), 0, "round={round}");
            assert!(f.count("desk_subscription_exclusion") <= 512);
        }
    }
}

#[test]
fn checked_excluded_traffic_cannot_rewind_revisit_and_starve_a_later_span() {
    for traffic in [false, true] {
        let f = Fixture::new();
        enable(&f, "switch_on_pending", "");
        f.fact("left", 2, "alpha", None);
        f.fact("right", 4, "alpha", None);
        f.add("boundary", 5, "alpha", Some("open"));
        let s = f.open();
        let page = f.pull(&s, None, ITEMS);
        f.run(f.ack(&s, &page, references(&page)));
        let earlier = "00000000-0000-3000-8000-000000000001";
        for n in 0..200 {
            notice_in(&f, earlier, &format!("earlier-{n}"), 1000 + 2 * n);
        }
        assert!(ids(&f.pull(&s, None, ITEMS)).is_empty());
        // Model a newly eligible observation inside retained skipped evidence
        // below the prefix, notified through the real revision primitive.
        f.add("late", 3, "alpha", Some("open"));
        let tx = f.conn.unchecked_transaction().unwrap();
        selection::advance_revision(&tx).unwrap();
        tx.commit().unwrap();
        let mut delivered = false;
        for round in 0..12 {
            if traffic {
                notice_in(&f, earlier, &format!("traffic-{round}"), 5000 + 2 * round);
            }
            let page = f.pull(&s, None, ITEMS);
            delivered |= ids(&page).contains("late");
            let refs = references(&page);
            if !refs.is_empty() {
                f.run(f.ack(&s, &page, refs));
            }
            if delivered {
                break;
            }
        }
        assert!(
            delivered,
            "traffic={traffic}: late row starved behind revisit"
        );
        assert!(
            ids(&f.pull(&s, None, ITEMS)).is_empty(),
            "ack stays permanent"
        );
    }
}
