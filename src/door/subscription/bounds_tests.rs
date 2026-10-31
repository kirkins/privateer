use super::*;

#[test]
fn serialized_pages_split_without_skipping_and_tokens_bind_operation_and_anchor() {
    let f = Fixture::new();
    for i in 1..=4 {
        f.add(&format!("mail-{i}"), i, "alpha", None);
    }
    f.conn
        .execute(
            "UPDATE desk_intake_fact SET content=json_set(content,'$.body',?1)",
            ["x".repeat(90 * 1024)],
        )
        .unwrap();
    let sub = f.open();
    let first = f.pull(&sub, None, 50);
    assert_eq!(ids(&first).len(), 2);
    assert!(serde_json::to_vec(&first).unwrap().len() <= BYTES);
    let token = next(&first).unwrap();
    let second = f.pull(&sub, Some(token.clone()), 50);
    assert_eq!(
        ids(&second),
        BTreeSet::from_iter(["mail-3", "mail-4"].map(str::to_owned))
    );
    assert!(next(&second).is_none());
    assert!(
        f.run_as(
            &f.c,
            Operation::Thread {
                subscription: sub.clone(),
                origin: MailIdentity::new(CORE, BOX, "mail-1").unwrap(),
                continuation: Some(token.clone()),
                items: 50
            },
            100
        )
        .is_err()
    );
    let forged = format!("{token}x");
    assert!(
        f.run_as(
            &f.c,
            Operation::Pull {
                subscription: sub.clone(),
                continuation: Some(forged),
                items: 50
            },
            100
        )
        .is_err()
    );
    assert!(
        f.run_as(
            &f.c,
            Operation::Pull {
                subscription: sub,
                continuation: Some(token),
                items: 50
            },
            SNAPSHOT_TTL + 100
        )
        .is_err()
    );
    assert_eq!(f.count("desk_subscription_progress"), 0);
}

#[test]
fn snapshot_row_and_byte_bounds_keep_prefix_and_subscription_limit_refuses() {
    let f = Fixture::new();
    // Snapshot-eligible rows have an explicit ceiling independent of page
    // size. A bounded prefix may not claim the rest drained.
    f.conn.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10001)
      INSERT INTO desk_intake_fact(domain,owner_core,generation,incarnation,source,core,mailbox,id,source_order,content,state,unread,pending)
      SELECT ?1,?2,1,?3,?2,?2,?4,'mail-'||x,x,'{\"from\":\"alpha\",\"to\":\"captain\",\"body\":\"notice\"}',NULL,1,1 FROM n",params![f.m.domain,CORE,f.m.incarnation,BOX]).unwrap();
    let sub = f.open();
    let first = f.pull(&sub, None, 1);
    assert_eq!(ids(&first), BTreeSet::from(["mail-1".into()]));
    assert_eq!(first["coverage"]["complete"], false);
    let saved: String = f
        .conn
        .query_row("SELECT body FROM desk_subscription_snapshot", [], |r| {
            r.get(0)
        })
        .unwrap();
    let saved: Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(saved["rows"].as_array().unwrap().len(), snapshot::PULL_ROWS);
    assert_eq!(
        saved["rows"][snapshot::PULL_ROWS - 1]["order"],
        snapshot::PULL_ROWS
    );
    assert_eq!(f.count("desk_intake_fact"), 10001);
    // Stored body byte admission is separate from serialized page admission.
    f.conn
        .execute("DELETE FROM desk_intake_fact WHERE source_order>200", [])
        .unwrap();
    f.conn
        .execute(
            "UPDATE desk_intake_fact SET content=json_set(content,'$.body',?1)",
            ["x".repeat(90 * 1024)],
        )
        .unwrap();
    let page = f.pull(&sub, None, 1);
    assert_eq!(ids(&page), BTreeSet::from(["mail-1".into()]));
    assert_eq!(page["coverage"]["complete"], false);
    let sizes: Vec<i64> = f
        .conn
        .prepare("SELECT length(body) FROM desk_subscription_snapshot")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(sizes.iter().all(|n| *n <= 16 * 1024 * 1024));
    for _ in 1..16 {
        let c =
            DelegatedContext::resolved(uuid(), "door:alpha".into(), vec!["mail:read".into()], None);
        f.grant(&c);
        f.run_as(&c, Operation::Open, 100).unwrap();
    }
    let extra =
        DelegatedContext::resolved(uuid(), "door:alpha".into(), vec!["mail:read".into()], None);
    f.grant(&extra);
    assert_eq!(
        f.run_as(&extra, Operation::Open, 100)
            .unwrap_err()
            .to_string(),
        "backpressure"
    );
    assert_eq!(f.count("desk_subscription"), 16);
}
