//! Review regressions at the serving boundary, not hand-written progress.
use super::*;

#[test]
fn activated_null_thread_reserves_split_and_does_not_fabricate_an_ack() {
    let f = Fixture::new();
    for n in 1..=3 {
        f.fact(&format!("notice-{n}"), n, "alpha", None);
    }
    f.add("incoming", 4, "alpha", Some("open"));
    let s = f.open();
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
    let sub = sub(&f, &s);
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
