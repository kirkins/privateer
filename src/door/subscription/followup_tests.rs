use super::*;

#[test]
fn first_snapshot_over_16_mib_pages_and_acks_every_version_in_order() {
    let f = Fixture::new();
    f.add("mail-1", 1, "alpha", Some("open"));
    let sub = f.open();
    let pending = f.pull(&sub, None, 1);
    // Displace a served version, then exceed the snapshot byte budget with
    // visible, unacknowledged history (still below intake's 64 MiB bound).
    f.add("mail-1", 201, "alpha", Some("closed"));
    for i in 2..=200 {
        f.add(&format!("mail-{i}"), i, "alpha", None);
    }
    f.conn
        .execute(
            "UPDATE desk_intake_fact SET content=json_set(content,'$.body',?1)",
            ["x".repeat(90 * 1024)],
        )
        .unwrap();
    let bytes: i64 = f
        .conn
        .query_row(
            "SELECT SUM(length(content)) FROM desk_intake_fact",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(bytes > 16 * 1024 * 1024 && bytes < 64 * 1024 * 1024);
    let now = SNAPSHOT_TTL + 101;
    let mut seen = Vec::new();
    let mut snapshots = 0;
    loop {
        let mut page = f
            .run_as(
                &f.c,
                Operation::Pull {
                    subscription: sub.clone(),
                    continuation: None,
                    items: 50,
                },
                now,
            )
            .unwrap();
        if references(&page).is_empty() {
            break;
        }
        snapshots += 1;
        if snapshots == 1 {
            assert_eq!(page["coverage"]["complete"], false);
            assert_eq!(page["coverage"]["retry"]["action"], "pull_after_ack");
            assert_eq!(references(&page)[0], references(&pending)[0]);
        }
        loop {
            for item in page["items"].as_array().unwrap() {
                seen.push((
                    item["origin"]["id"].as_str().unwrap().to_owned(),
                    item["state"].clone(),
                ));
            }
            let committed = f
                .run_as(&f.c, f.ack(&sub, &page, references(&page)), now)
                .unwrap();
            assert_eq!(
                committed,
                f.run_as(&f.c, f.ack(&sub, &page, references(&page)), now)
                    .unwrap()
            );
            let Some(token) = next(&page) else { break };
            page = f
                .run_as(
                    &f.c,
                    Operation::Pull {
                        subscription: sub.clone(),
                        continuation: Some(token),
                        items: 50,
                    },
                    now,
                )
                .unwrap();
        }
        assert!(
            snapshots <= 3,
            "acknowledgments must advance the next snapshot"
        );
    }
    let mut expected = vec![("mail-1".into(), json!("open"))];
    expected.extend((2..=200).map(|i| (format!("mail-{i}"), Value::Null)));
    expected.push(("mail-1".into(), json!("closed")));
    assert_eq!(seen, expected);
    assert_eq!(snapshots, 3);
    assert_eq!(f.count("desk_subscription_ref"), 0);
    let through: i64 = f
        .conn
        .query_row(
            "SELECT through_order FROM desk_subscription_progress",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(through, 201);
    assert_eq!(f.count("desk_intake_fact"), 200);
}

#[test]
fn maximum_pull_snapshot_fits_page_receipts_and_fresh_pull_continues() {
    // Exercise both row-limited small mail and mail large enough to force
    // one-row byte-limited pages even when requesting the maximum page size.
    for body_bytes in [0, 100 * 1024] {
        let f = Fixture::new();
        let limit = snapshot::PAGE_ROWS;
        for i in 1..=limit + 2 {
            f.add(&format!("mail-{i}"), i as i64, "alpha", None);
        }
        if body_bytes > 0 {
            f.conn
                .execute(
                    "UPDATE desk_intake_fact SET content=json_set(content,'$.body',?1)",
                    ["x".repeat(body_bytes)],
                )
                .unwrap();
        }
        let sub = f.open();
        let mut page = f.pull(&sub, None, 1);
        assert_eq!(page["coverage"]["complete"], false);
        assert_eq!(page["coverage"]["retry"]["action"], "pull_after_ack");
        let saved: String = f
            .conn
            .query_row("SELECT body FROM desk_subscription_snapshot", [], |r| {
                r.get(0)
            })
            .unwrap();
        let saved: Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(saved["rows"].as_array().unwrap().len(), limit);
        let first = page.clone();
        let mut seen = Vec::new();
        let mut pages = 0;
        loop {
            seen.extend(
                page["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| item["origin"]["id"].as_str().unwrap().to_owned()),
            );
            assert_eq!(references(&page).len(), 1);
            f.run(f.ack(&sub, &page, references(&page)));
            pages += 1;
            let Some(token) = next(&page) else { break };
            // Changing the requested size must not invalidate the receipt
            // budget; small rows use one-row requests, large rows hit bytes.
            page = f.pull(&sub, Some(token), if body_bytes == 0 { 1 } else { ITEMS });
        }
        assert_eq!(pages, limit);
        assert_eq!(f.count("desk_subscription_receipt"), limit as i64 + 1);
        // A whole maximum snapshot leaves room for another fresh pull; no
        // clock advance, receipt eviction or weakening of replay is needed.
        page = f.pull(&sub, None, ITEMS);
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
            (1..=limit + 2)
                .map(|i| format!("mail-{i}"))
                .collect::<Vec<_>>()
        );
        assert!(references(&f.pull(&sub, None, ITEMS)).is_empty());
        assert_eq!(
            f.run(f.ack(&sub, &first, references(&first)))["acknowledged"],
            1
        );
        assert_eq!(f.count("desk_subscription_ref"), 0);
    }
}

#[test]
fn receipt_cap_recovers_after_an_hour_without_key_reuse_or_shortened_replay() {
    let f = Fixture::new();
    for i in 1..=130 {
        f.add(&format!("mail-{i}"), i, "alpha", None);
    }
    let sub = f.open();
    let mut page = f.pull(&sub, None, 1);
    let first = page.clone();
    let first_outcome = f.run(f.ack(&sub, &page, references(&page)));
    for _ in 1..RECEIPTS - 1 {
        page = f.pull(&sub, next(&page), 1);
        f.run(f.ack(&sub, &page, references(&page)));
    }
    assert_eq!(f.count("desk_subscription_receipt"), RECEIPTS);
    assert_eq!(
        f.run_as(
            &f.c,
            Operation::Pull {
                subscription: sub.clone(),
                continuation: next(&page),
                items: 1
            },
            100
        )
        .unwrap_err()
        .to_string(),
        "backpressure"
    );
    assert_eq!(
        first_outcome,
        f.run_as(
            &f.c,
            f.ack(&sub, &first, references(&first)),
            SNAPSHOT_TTL + 99
        )
        .unwrap()
    );
    let now = SNAPSHOT_TTL + 100;
    assert_eq!(
        f.run_as(&f.c, f.ack(&sub, &first, references(&first)), now)
            .unwrap_err()
            .to_string(),
        "stale_or_unknown"
    );
    assert!(
        f.run_as(
            &f.c,
            Operation::Pull {
                subscription: sub.clone(),
                continuation: next(&page),
                items: 1
            },
            now
        )
        .is_err()
    );
    let fresh = f
        .run_as(
            &f.c,
            Operation::Pull {
                subscription: sub.clone(),
                continuation: None,
                items: 50,
            },
            now,
        )
        .unwrap();
    assert_ne!(fresh["ack_key"], first["ack_key"]);
    assert_eq!(
        ids(&fresh),
        BTreeSet::from_iter((128..=130).map(|i| format!("mail-{i}")))
    );
    f.run_as(&f.c, f.ack(&sub, &fresh, references(&fresh)), now)
        .unwrap();
    assert_eq!(
        f.run_as(&f.c, f.ack(&sub, &first, references(&fresh)), now)
            .unwrap_err()
            .to_string(),
        "stale_or_unknown"
    );
}

#[test]
fn reused_standalone_key_renews_expiry_but_committed_retry_does_not() {
    let f = Fixture::new();
    f.add("mail-1", 1, "alpha", None);
    let opened = f.run(Operation::Open);
    let sub = opened["subscription"].as_str().unwrap();
    let near_expiry = RECEIPT_TTL + 99;
    let resumed = f
        .run_as(
            &f.c,
            Operation::Resume {
                subscription: sub.into(),
            },
            near_expiry,
        )
        .unwrap();
    assert_eq!(opened["ack_key"], resumed["ack_key"]);
    let page = f
        .run_as(
            &f.c,
            Operation::Pull {
                subscription: sub.into(),
                continuation: None,
                items: 1,
            },
            RECEIPT_TTL + 101,
        )
        .unwrap();
    let ack = || Operation::Ack {
        subscription: sub.into(),
        key: resumed["ack_key"].as_str().unwrap().into(),
        refs: references(&page),
    };
    let outcome = f.run_as(&f.c, ack(), RECEIPT_TTL + 101).unwrap();
    let expiry = || {
        f.conn
            .query_row(
                "SELECT expires FROM desk_subscription_receipt WHERE key=?1",
                [resumed["ack_key"].as_str().unwrap()],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
    };
    let before = expiry();
    assert_eq!(outcome, f.run_as(&f.c, ack(), RECEIPT_TTL + 102).unwrap());
    assert_eq!(before, expiry());
}

#[test]
fn backwards_commit_clock_cannot_expire_a_key_while_its_snapshot_is_live() {
    let f = Fixture::new();
    f.add("mail-1", 1, "alpha", None);
    f.add("mail-2", 2, "alpha", None);
    let sub = f.open();
    let first = f.pull(&sub, None, 1);
    let second = f.pull(&sub, next(&first), 1);
    f.run_as(&f.c, f.ack(&sub, &second, references(&second)), 50)
        .unwrap();
    let replay = f
        .run_as(
            &f.c,
            Operation::Pull {
                subscription: sub.clone(),
                continuation: next(&first),
                items: 1,
            },
            SNAPSHOT_TTL + 75,
        )
        .unwrap();
    assert_eq!(replay["ack_key"], second["ack_key"]);
    // If the receipt had expired at commit-time + TTL, this page replay could
    // have re-reserved its key and accepted the still-unacked first ref.
    assert_eq!(
        f.run_as(
            &f.c,
            f.ack(&sub, &replay, references(&first)),
            SNAPSHOT_TTL + 75
        )
        .unwrap_err()
        .to_string(),
        "invalid_ack"
    );
}

#[test]
fn retirement_uses_injected_time_and_reextended_grant_restarts_from_zero() {
    let f = Fixture::new();
    f.add("mail-1", 1, "alpha", None);
    let old = f.open();
    let page = f.pull(&old, None, 1);
    f.run(f.ack(&old, &page, references(&page)));
    f.conn
        .execute(
            "UPDATE door_clients SET expires_ms=500 WHERE id=?1",
            [f.c.client()],
        )
        .unwrap();
    let other =
        DelegatedContext::resolved(uuid(), "door:beta".into(), vec!["mail:read".into()], None);
    f.grant(&other);
    // Admission of another grant sweeps terminal readers; neither call relies
    // on the real wall clock matching this deliberately small injected epoch.
    f.run_as(&other, Operation::Open, 499).unwrap();
    assert_eq!(f.count("desk_subscription"), 2);
    f.run_as(&other, Operation::Open, 500).unwrap();
    assert_eq!(f.count("desk_subscription"), 1);
    f.conn
        .execute(
            "UPDATE door_clients SET expires_ms=NULL WHERE id=?1",
            [f.c.client()],
        )
        .unwrap();
    let new = f.run_as(&f.c, Operation::Open, 501).unwrap();
    let sub = new["subscription"].as_str().unwrap();
    assert_ne!(sub, old);
    assert_eq!(f.count("desk_subscription_progress"), 0);
    assert_eq!(
        f.run_as(&f.c, f.ack(sub, &page, references(&page)), 501)
            .unwrap_err()
            .to_string(),
        "stale_or_unknown"
    );
    let replay = f
        .run_as(
            &f.c,
            Operation::Pull {
                subscription: sub.into(),
                continuation: None,
                items: 50,
            },
            501,
        )
        .unwrap();
    assert_eq!(ids(&replay), BTreeSet::from(["mail-1".into()]));
}
