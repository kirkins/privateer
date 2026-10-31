use super::*;

fn thread(sub: &str, continuation: Option<String>, items: usize) -> Operation {
    Operation::Thread {
        subscription: sub.into(),
        origin: MailIdentity::new(CORE, BOX, "mail-1").unwrap(),
        continuation,
        items,
    }
}

fn seed(f: &Fixture, rows: usize, body_bytes: usize) {
    for i in 1..=rows {
        f.add(&format!("mail-{i}"), i as i64, "alpha", None);
    }
    f.conn
        .execute(
            "UPDATE desk_intake_fact SET content=json_set(content,'$.correlation','mail-1','$.body',?1)",
            ["x".repeat(body_bytes)],
        )
        .unwrap();
}

#[test]
fn maximum_thread_fits_receipts_with_exact_order_and_immutable_replay() {
    for body_bytes in [0, 100 * 1024] {
        let f = Fixture::new();
        let limit = snapshot::PAGE_ROWS;
        seed(&f, limit, body_bytes);
        let sub = f.open();
        let mut page = f.run(thread(&sub, None, 1));
        let first = page.clone();
        let resumed = f.run(Operation::Resume {
            subscription: sub.clone(),
        });
        let start = resumed["pending_snapshots"][0]["continuation"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(f.run(thread(&sub, Some(start), 1)), first);
        let mut seen = Vec::new();
        let mut first_outcome = None;
        loop {
            seen.extend(
                page["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| item["origin"]["id"].as_str().unwrap().to_owned()),
            );
            assert_eq!(references(&page).len(), 1);
            let outcome = f.run(f.ack(&sub, &page, references(&page)));
            first_outcome.get_or_insert(outcome);
            let Some(token) = next(&page) else { break };
            // Visit the same offset with different sizes, then follow the
            // one-row path. Every possible offset still fits the key budget.
            let wide = f.run(thread(&sub, Some(token.clone()), ITEMS));
            page = f.run(thread(
                &sub,
                Some(token),
                if body_bytes == 0 { 1 } else { ITEMS },
            ));
            assert_eq!(wide["ack_key"], page["ack_key"]);
        }
        assert_eq!(
            seen,
            (1..=limit).map(|i| format!("mail-{i}")).collect::<Vec<_>>()
        );
        assert_eq!(f.count("desk_subscription_receipt"), limit as i64 + 1);
        assert!(f.count("desk_subscription_receipt") < RECEIPTS);
        // No receipt eviction or clock advance is needed to finish or to
        // replay the original committed outcome.
        assert_eq!(
            f.run(f.ack(&sub, &first, references(&first))),
            first_outcome.unwrap()
        );
        assert_eq!(f.count("desk_subscription_ref"), 0);
        assert!(references(&f.pull(&sub, None, ITEMS)).is_empty());
    }
}

#[test]
fn oversized_threads_refuse_before_page_custody_and_pull_is_actionable() {
    // A boundary case, a small-row thread beyond the old ~127-page limit,
    // and 128 large rows forcing one-row pages with maximum-size requests.
    for (rows, body_bytes) in [(snapshot::PAGE_ROWS + 1, 0), (6400, 0), (128, 100 * 1024)] {
        let f = Fixture::new();
        seed(&f, rows, body_bytes);
        let sub = f.open();
        for now in [100, SNAPSHOT_TTL + 101] {
            assert_eq!(
                f.run_as(&f.c, thread(&sub, None, ITEMS), now)
                    .unwrap_err()
                    .to_string(),
                "thread_too_large"
            );
            assert_eq!(f.count("desk_subscription_snapshot"), 0);
            assert_eq!(f.count("desk_subscription_ref"), 0);
            assert_eq!(f.count("desk_subscription_progress"), 0);
            assert_eq!(f.count("desk_subscription_receipt"), 1);
        }
        let mut page = f.pull(&sub, None, ITEMS);
        assert_eq!(page["coverage"]["retry"]["action"], "pull_after_ack");
        let mut seen = Vec::new();
        loop {
            seen.extend(
                page["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| item["origin"]["id"].as_str().unwrap().to_owned()),
            );
            f.run(f.ack(&sub, &page, references(&page)));
            let Some(token) = next(&page) else { break };
            page = f.pull(&sub, Some(token), ITEMS);
        }
        assert_eq!(
            seen,
            (1..=snapshot::PAGE_ROWS)
                .map(|i| format!("mail-{i}"))
                .collect::<Vec<_>>()
        );
        assert_eq!(f.count("desk_intake_fact"), rows as i64);
        assert!(f.count("desk_subscription_receipt") < RECEIPTS);
    }
}
