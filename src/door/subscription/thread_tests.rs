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

// Drain pull first to prove that thread windows do not depend on unread state.
fn drain(f: &Fixture, sub: &str) {
    loop {
        let mut page = f.pull(sub, None, ITEMS);
        if references(&page).is_empty() {
            break;
        }
        loop {
            f.run(f.ack(sub, &page, references(&page)));
            let Some(token) = next(&page) else { break };
            page = f.pull(sub, Some(token), ITEMS);
        }
    }
}

#[test]
fn tail_windows_read_all_history_after_pull_ack_with_bounded_receipts() {
    // Sixteen windows plus the original pull stay below the receipt budget.
    // Larger live receipt sets still exert honest, hour-bounded backpressure.
    let f = Fixture::new();
    let rows = snapshot::PAGE_ROWS * 16 + 1;
    seed(&f, rows, 0);
    let sub = f.open();
    drain(&f, &sub);
    let progress = f
        .conn
        .query_row(
            "SELECT through_order FROM desk_subscription_progress",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(progress, rows as i64);
    let mut page = f.run(thread(&sub, None, ITEMS));
    assert_eq!(
        page["items"][0]["origin"]["id"],
        format!("mail-{}", rows - snapshot::PAGE_ROWS + 1)
    );
    let mut seen = BTreeSet::new();
    loop {
        for id in ids(&page) {
            assert!(seen.insert(id));
        }
        let token = next(&page);
        f.run(f.ack(&sub, &page, references(&page)));
        assert_eq!(f.count("desk_subscription_ref"), 0);
        assert!(f.count("desk_subscription_receipt") <= RECEIPTS);
        assert!(f.count("desk_subscription_snapshot") <= 2);
        let Some(token) = token else { break };
        let replay = f.run(thread(&sub, Some(token.clone()), ITEMS));
        page = f.run(thread(&sub, Some(token), ITEMS));
        assert_eq!(page, replay);
    }
    assert_eq!(seen, (1..=rows).map(|i| format!("mail-{i}")).collect());
    assert_eq!(
        f.conn
            .query_row(
                "SELECT through_order FROM desk_subscription_progress",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        progress
    );
    assert!(references(&f.pull(&sub, None, ITEMS)).is_empty());
}

#[test]
fn byte_limited_thread_windows_and_read_only_sparse_state() {
    let f = Fixture::new();
    // Each item fits a result, but 96 such rows exceed 16 MiB.
    seed(&f, 96, 180 * 1024);
    let sub = f.open();
    let sparse = f.run(Operation::Show {
        subscription: sub.clone(),
        origin: MailIdentity::new(CORE, BOX, "mail-96").unwrap(),
    });
    f.run(f.ack(&sub, &sparse, references(&sparse)));
    let first = f.run(thread(&sub, None, ITEMS));
    assert_eq!(first["coverage"]["retry"]["action"], "thread_continue");
    let body: String = f
        .conn
        .query_row("SELECT body FROM desk_subscription_snapshot", [], |r| {
            r.get(0)
        })
        .unwrap();
    let saved: Value = serde_json::from_str(&body).unwrap();
    assert!(saved["rows"].as_array().unwrap().len() < 96);
    assert!(body.len() <= 16 * 1024 * 1024);
    let mut seen = BTreeSet::new();
    let mut page = first;
    loop {
        seen.extend(ids(&page));
        assert_eq!(f.count("desk_subscription_progress"), 0);
        let Some(token) = next(&page) else { break };
        page = f.run(thread(&sub, Some(token), ITEMS));
    }
    assert_eq!(seen.len(), 96);
    // Reads issue refs but neither acknowledge them nor alter the existing
    // sparse acknowledgment or compact a prefix.
    assert_eq!(f.count("desk_subscription_ref"), 96);
    assert_eq!(
        f.conn
            .query_row("SELECT SUM(acked) FROM desk_subscription_ref", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        f.conn
            .query_row(
                "SELECT ref FROM desk_subscription_ref WHERE acked=1",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        references(&sparse)[0]
    );
}

#[test]
fn oversized_item_precedes_window_bounds_without_issuing_custody() {
    let f = Fixture::new();
    seed(&f, snapshot::PAGE_ROWS + 1, 0);
    f.conn
        .execute(
            "UPDATE desk_intake_fact SET content=json_set(content,'$.body',?1) WHERE id='mail-1'",
            ["x".repeat(16 * 1024 * 1024)],
        )
        .unwrap();
    let sub = f.open();
    assert_eq!(
        f.run_as(&f.c, thread(&sub, None, ITEMS), 100)
            .unwrap_err()
            .to_string(),
        "oversized_item"
    );
    assert_eq!(f.count("desk_subscription_snapshot"), 0);
    assert_eq!(f.count("desk_subscription_ref"), 0);
    assert_eq!(f.count("desk_subscription_receipt"), 1);
}

fn older_token(f: &Fixture, sub: &str) -> String {
    let first = f.run(thread(sub, None, ITEMS));
    let last = f.run(thread(sub, next(&first), ITEMS));
    next(&last).unwrap()
}

#[test]
fn thread_continuations_fence_visibility_incarnation_owner_anchor_and_forgery() {
    for change in [
        "visibility",
        "incarnation",
        "generation",
        "owner_incarnation",
        "anchor",
        "forged",
        "tampered",
        "expired",
    ] {
        let mut f = Fixture::new();
        seed(&f, snapshot::PAGE_ROWS + 1, 0);
        let sub = f.open();
        let mut token = older_token(&f, &sub);
        let mut now = 100;
        let restricted = DelegatedContext::resolved(
            f.c.client().into(),
            f.c.resource().into(),
            vec!["mail:read".into()],
            Some(vec!["alpha".into()]),
        );
        let c = if change == "visibility" {
            f.grant(&restricted);
            &restricted
        } else {
            &f.c
        };
        match change {
            "incarnation" => {
                f.conn
                    .execute("UPDATE desk_subscription SET incarnation=?1", [uuid()])
                    .unwrap();
            }
            "generation" => {
                f.m.generation += 1;
            }
            "owner_incarnation" => {
                f.m.incarnation = uuid();
            }
            "forged" => {
                token = format!("{}:96:{}", uuid(), "0".repeat(64));
            }
            "tampered" => {
                token.push('0');
            }
            "expired" => {
                now += SNAPSHOT_TTL;
            }
            _ => {}
        }
        let mut op = thread(&sub, Some(token), ITEMS);
        if change == "anchor" {
            op = Operation::Thread {
                subscription: sub,
                origin: MailIdentity::new(CORE, BOX, "mail-2").unwrap(),
                continuation: match op {
                    Operation::Thread { continuation, .. } => continuation,
                    _ => unreachable!(),
                },
                items: ITEMS,
            };
        }
        assert_eq!(
            f.run_as(c, op, now).unwrap_err().to_string(),
            "stale_or_unknown",
            "{change}"
        );
    }
}

#[test]
fn older_window_and_lost_transition_recover_after_file_backed_restart() {
    let f = Fixture::new();
    seed(&f, snapshot::PAGE_ROWS + 1, 0);
    let sub = f.open();
    drain(&f, &sub);
    let older = older_token(&f, &sub);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alpha.sqlite");
    f.conn
        .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
        .unwrap();
    let reopened = Fixture {
        conn: Connection::open(&path).unwrap(),
        m: f.m.clone(),
        c: DelegatedContext::from_gateway(&f.c.gateway()).unwrap(),
    };
    let child = reopened.run(thread(&sub, Some(older.clone()), ITEMS));
    assert_eq!(ids(&child), BTreeSet::from(["mail-1".into()]));
    assert_eq!(reopened.count("desk_subscription_snapshot"), 1);
    drop(reopened.conn);
    let restarted = Fixture {
        conn: Connection::open(path).unwrap(),
        m: reopened.m,
        c: reopened.c,
    };
    assert_eq!(restarted.run(thread(&sub, Some(older), ITEMS)), child);
}

#[test]
fn restricted_thread_windows_hide_owner_revision_and_hidden_endpoints() {
    let f = Fixture::new();
    seed(&f, 150, 0);
    f.add("hidden", 151, "beta", None);
    f.add("outside", 152, "gamma", None);
    f.conn
        .execute(
            "UPDATE desk_intake_fact SET content=json_set(content,'$.correlation','mail-1')",
            [],
        )
        .unwrap();
    let c = DelegatedContext::resolved(
        f.c.client().into(),
        f.c.resource().into(),
        vec!["mail:read".into()],
        Some(vec!["alpha".into()]),
    );
    f.grant(&c);
    let opened = f.run_as(&c, Operation::Open, 100).unwrap();
    let sub = opened["subscription"].as_str().unwrap();
    let mut token = None;
    let mut seen = BTreeSet::new();
    loop {
        let page = f.run_as(&c, thread(sub, token, ITEMS), 100).unwrap();
        assert_eq!(page["coverage"]["complete"], false);
        assert_eq!(page["coverage"]["owner"], Value::Null);
        assert_eq!(page["coverage"]["manifest_revision"], Value::Null);
        let encoded = page["coverage"].to_string();
        assert!(!encoded.contains(CORE) && !encoded.contains("beta") && !encoded.contains("gamma"));
        seen.extend(ids(&page));
        token = next(&page);
        if token.is_none() {
            break;
        }
    }
    assert_eq!(seen, (1..=150).map(|i| format!("mail-{i}")).collect());
    assert_eq!(f.count("desk_subscription_progress"), 0);
}

#[test]
fn unread_windows_backpressure_without_eviction_and_transition_replays_after_reclaim() {
    let f = Fixture::new();
    seed(&f, snapshot::PAGE_ROWS * 3, 0);
    let sub = f.open();
    let mut page = f.run(thread(&sub, None, ITEMS));
    page = f.run(thread(&sub, next(&page), ITEMS));
    let older = next(&page).unwrap();
    let first_child = f.run(thread(&sub, Some(older.clone()), ITEMS));
    let last_child = f.run(thread(&sub, next(&first_child), ITEMS));
    let final_token = next(&last_child).unwrap();
    assert_eq!(f.count("desk_subscription_snapshot"), 2);
    assert_eq!(
        f.run_as(&f.c, thread(&sub, Some(final_token), ITEMS), 100)
            .unwrap_err()
            .to_string(),
        "backpressure"
    );
    assert_eq!(f.count("desk_subscription_snapshot"), 2);
    assert_eq!(
        f.count("desk_subscription_ref"),
        (snapshot::PAGE_ROWS * 2) as i64
    );
    assert_eq!(f.count("desk_subscription_progress"), 0);

    // A pull-acked reader can reclaim the parent during the transition;
    // repeating that exact request still returns the retained child.
    let f = Fixture::new();
    seed(&f, snapshot::PAGE_ROWS + 1, 0);
    let sub = f.open();
    drain(&f, &sub);
    let older = older_token(&f, &sub);
    let child = f.run(thread(&sub, Some(older.clone()), ITEMS));
    assert_eq!(f.count("desk_subscription_snapshot"), 1);
    assert_eq!(f.run(thread(&sub, Some(older), ITEMS)), child);
}
