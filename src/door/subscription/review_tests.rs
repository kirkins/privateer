use super::*;

pub(super) fn seed_legacy(conn: &Connection) {
    conn.execute_batch("CREATE TABLE door_cursor(door TEXT PRIMARY KEY, through_seq INTEGER NOT NULL);
      INSERT INTO door_cursor VALUES('alpha',17),('beta',9);
      CREATE TABLE door_reader_stream(client TEXT NOT NULL,resource TEXT NOT NULL,mailbox TEXT NOT NULL,generation TEXT NOT NULL,through_seq INTEGER NOT NULL,PRIMARY KEY(client,resource,mailbox));
      INSERT INTO door_reader_stream VALUES('other-grant','door:beta','alpha-box','old-generation',4);
      CREATE TABLE door_reader_served(client TEXT NOT NULL,resource TEXT NOT NULL,mailbox TEXT NOT NULL,id TEXT NOT NULL,seq INTEGER NOT NULL,ref TEXT NOT NULL UNIQUE,acked INTEGER NOT NULL,generation TEXT NOT NULL,PRIMARY KEY(client,resource,mailbox,id));
      INSERT INTO door_reader_served VALUES('other-grant','door:beta','alpha-box','legacy-pending',5,'legacy-ref',0,'old-generation');
      CREATE TABLE mail(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,sender TEXT NOT NULL,recipient TEXT NOT NULL,kind TEXT NOT NULL,correlation TEXT,task_id TEXT,request_kind TEXT,body TEXT NOT NULL,payload TEXT NOT NULL,urgency TEXT NOT NULL,deadline_ms INTEGER,created_ms INTEGER NOT NULL,state TEXT,answered_by TEXT,answered_ms INTEGER);
      INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,state) VALUES('legacy-open','alpha','captain','request','notice','{}','normal',0,'open');
      INSERT INTO mail(id,sender,recipient,kind,correlation,body,payload,urgency,created_ms,state,answered_by,answered_ms) VALUES('legacy-closed','alpha','captain','request','legacy-open','notice','{}','normal',0,'closed','captain',1);
      CREATE TABLE mail_cursor(address TEXT PRIMARY KEY,through_seq INTEGER NOT NULL);
      INSERT INTO mail_cursor VALUES('captain',1);").unwrap();
}

pub(super) fn legacy_state(conn: &Connection) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
    [
        "door_cursor",
        "door_reader_stream",
        "door_reader_served",
        "mail",
        "mail_cursor",
    ]
    .iter()
    .map(|table| {
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .unwrap();
        let columns = stmt.column_count();
        stmt.query_map([], |r| (0..columns).map(|i| r.get(i)).collect())
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    })
    .collect()
}

#[test]
fn snapshot_and_ack_ignore_large_acknowledged_and_hidden_history() {
    let f = Fixture::new();
    let restricted = DelegatedContext::resolved(
        f.c.client().into(),
        f.c.resource().into(),
        vec!["mail:read".into()],
        Some(vec!["alpha".into()]),
    );
    f.grant(&restricted);
    let sub = f.run_as(&restricted, Operation::Open, 100).unwrap()["subscription"]
        .as_str()
        .unwrap()
        .to_owned();
    f.conn.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<400)
      INSERT INTO desk_intake_fact(domain,owner_core,generation,incarnation,source,core,mailbox,id,source_order,content,unread,pending)
      SELECT ?1,?2,1,?3,?2,?2,?4,'history-'||x,x,json_object('from',CASE WHEN x<=200 THEN 'alpha' ELSE 'beta' END,'to','captain','body',?5),1,1 FROM n", params![f.m.domain,CORE,f.m.incarnation,BOX,"x".repeat(90*1024)]).unwrap();
    let bytes: i64 = f
        .conn
        .query_row(
            "SELECT SUM(length(content)) FROM desk_intake_fact",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(bytes > 16 * 1024 * 1024);
    f.conn
        .execute(
            "INSERT INTO desk_subscription_progress VALUES(?1,?2,?3,200)",
            params![sub, CORE, BOX],
        )
        .unwrap();
    for i in 1..=3 {
        f.add(&format!("new-{i}"), 400 + i, "alpha", None);
    }
    f.conn.execute("UPDATE desk_intake_fact SET content=json_set(content,'$.correlation','new-1') WHERE id='new-3'", []).unwrap();
    let show = f
        .run_as(
            &restricted,
            Operation::Show {
                subscription: sub.clone(),
                origin: MailIdentity::new(CORE, BOX, "new-3").unwrap(),
            },
            100,
        )
        .unwrap();
    assert_eq!(ids(&show), BTreeSet::from(["new-3".into()]));
    f.run_as(&restricted, f.ack(&sub, &show, references(&show)), 100)
        .unwrap();
    // Sparse show cannot skip new-1 or new-2 even with large history.
    let through: i64 = f
        .conn
        .query_row(
            "SELECT through_order FROM desk_subscription_progress",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(through, 200);
    let thread = f
        .run_as(
            &restricted,
            Operation::Thread {
                subscription: sub.clone(),
                origin: MailIdentity::new(CORE, BOX, "new-1").unwrap(),
                continuation: None,
                items: 50,
            },
            100,
        )
        .unwrap();
    assert_eq!(
        ids(&thread),
        BTreeSet::from(["new-1".into(), "new-3".into()])
    );
    let pull = f
        .run_as(
            &restricted,
            Operation::Pull {
                subscription: sub.clone(),
                continuation: None,
                items: 50,
            },
            SNAPSHOT_TTL + 101,
        )
        .unwrap();
    assert_eq!(ids(&pull), BTreeSet::from(["new-1".into(), "new-2".into()]));
    f.run_as(
        &restricted,
        f.ack(&sub, &pull, references(&pull)),
        SNAPSHOT_TTL + 101,
    )
    .unwrap();
    let through: i64 = f
        .conn
        .query_row(
            "SELECT through_order FROM desk_subscription_progress",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(through, 403);
}

#[test]
fn unused_keys_do_not_exhaust_long_lived_subscription_and_used_receipts_survive() {
    let f = Fixture::new();
    f.add("mail-1", 1, "alpha", None);
    let sub = f.open();
    let page = f.pull(&sub, None, 1);
    let refs = references(&page);
    let committed = f.run(f.ack(&sub, &page, refs.clone()));
    let mut resume_key = None;
    for _ in 0..200 {
        let resumed = f.run(Operation::Resume {
            subscription: sub.clone(),
        });
        if let Some(key) = &resume_key {
            assert_eq!(&resumed["ack_key"], key);
        }
        resume_key = Some(resumed["ack_key"].clone());
        assert!(ids(&f.pull(&sub, None, 50)).is_empty());
        // Drained snapshots now remain replayable until admission needs a
        // slot: two snapshot keys, one standalone key and the committed
        // outcome remain bounded even after hundreds of fresh pulls.
        assert!(f.count("desk_subscription_snapshot") <= 2);
        assert!(f.count("desk_subscription_receipt") <= 4);
    }
    assert_eq!(committed, f.run(f.ack(&sub, &page, refs)));
    // Expiring a snapshot frees its unused page keys, not pending refs.
    f.add("mail-2", 2, "alpha", None);
    f.add("mail-3", 3, "alpha", None);
    let first = f.pull(&sub, None, 1);
    let second = f.pull(&sub, next(&first), 1);
    let resumed = f
        .run_as(
            &f.c,
            Operation::Resume {
                subscription: sub.clone(),
            },
            SNAPSHOT_TTL + 101,
        )
        .unwrap();
    f.run_as(
        &f.c,
        Operation::Pull {
            subscription: sub.clone(),
            continuation: None,
            items: 50,
        },
        SNAPSHOT_TTL + 101,
    )
    .unwrap();
    assert!(
        f.run_as(
            &f.c,
            f.ack(&sub, &second, references(&second)),
            SNAPSHOT_TTL + 101
        )
        .is_err()
    );
    f.run_as(
        &f.c,
        Operation::Ack {
            subscription: sub,
            key: resumed["ack_key"].as_str().unwrap().into(),
            refs: references(&second),
        },
        SNAPSHOT_TTL + 101,
    )
    .unwrap();
}

#[test]
fn terminal_grants_are_retired_with_all_state_but_paused_grants_still_count() {
    let f = Fixture::new();
    f.add("mail-1", 1, "alpha", None);
    f.add("mail-2", 2, "alpha", None);
    for i in 0..16 {
        let c =
            DelegatedContext::resolved(uuid(), "door:alpha".into(), vec!["mail:read".into()], None);
        f.grant(&c);
        let sub = f.run_as(&c, Operation::Open, 100).unwrap()["subscription"]
            .as_str()
            .unwrap()
            .to_owned();
        let page = f
            .run_as(
                &c,
                Operation::Pull {
                    subscription: sub.clone(),
                    continuation: None,
                    items: 50,
                },
                100,
            )
            .unwrap();
        f.run_as(
            &c,
            f.ack(&sub, &page, vec![references(&page)[0].clone()]),
            100,
        )
        .unwrap();
        // Defer terminal changes until all sixteen slots have filled.
        f.conn
            .execute(
                "UPDATE door_clients SET registration=?2 WHERE id=?1",
                params![c.client(), if i % 2 == 0 { "revoke" } else { "expire" }],
            )
            .unwrap();
    }
    assert_eq!(f.count("desk_subscription"), 16);
    f.conn
        .execute("UPDATE door_clients SET paused=1", [])
        .unwrap();
    let extra =
        DelegatedContext::resolved(uuid(), "door:alpha".into(), vec!["mail:read".into()], None);
    f.grant(&extra);
    assert_eq!(
        f.run_as(&extra, Operation::Open, 100)
            .unwrap_err()
            .to_string(),
        "backpressure"
    );
    f.conn
        .execute(
            "UPDATE door_clients SET revoked_ms=1 WHERE registration='revoke'",
            [],
        )
        .unwrap();
    f.conn
        .execute(
            "UPDATE door_clients SET expires_ms=1 WHERE registration='expire'",
            [],
        )
        .unwrap();
    let opened = f.run_as(&extra, Operation::Open, 100).unwrap();
    assert_eq!(f.count("desk_subscription"), 1);
    assert_eq!(f.count("desk_subscription_snapshot"), 0);
    assert_eq!(f.count("desk_subscription_ref"), 0);
    assert_eq!(f.count("desk_subscription_progress"), 0);
    assert_eq!(f.count("desk_subscription_receipt"), 1);
    assert_eq!(opened["ok"], true);
}

#[test]
fn fenced_reopen_starts_new_identity_without_translating_old_state() {
    let mut f = Fixture::new();
    f.add("mail-1", 1, "alpha", None);
    f.add("mail-2", 2, "alpha", None);
    let old = f.open();
    let page = f.pull(&old, None, 1);
    let acked = f.run(f.ack(&old, &page, references(&page)));
    let pending = f.pull(&old, next(&page), 1);
    f.m.incarnation = uuid();
    assert!(
        f.run_as(
            &f.c,
            Operation::Resume {
                subscription: old.clone()
            },
            100
        )
        .is_err()
    );
    let opened = f.run(Operation::Open);
    let sub = opened["subscription"].as_str().unwrap();
    assert_ne!(sub, old);
    assert_ne!(opened["incarnation"], acked["incarnation"]);
    assert_eq!(f.count("desk_subscription_progress"), 0);
    assert_eq!(f.count("desk_subscription_ref"), 0);
    assert_eq!(f.count("desk_subscription_snapshot"), 0);
    assert_eq!(f.count("desk_subscription_receipt"), 1);
    assert!(
        f.run_as(&f.c, f.ack(sub, &pending, references(&pending)), 100)
            .is_err()
    );
    assert!(
        f.run_as(
            &f.c,
            Operation::Pull {
                subscription: old,
                continuation: next(&page),
                items: 1
            },
            100
        )
        .is_err()
    );
    // Only facts reconciled to the new binding are eligible. The engine does
    // not relocate the desk or import old-generation intake by reopening.
    assert!(ids(&f.pull(sub, None, 50)).is_empty());
    f.conn
        .execute(
            "UPDATE desk_intake_fact SET incarnation=?1",
            [&f.m.incarnation],
        )
        .unwrap();
    let fresh = f.pull(sub, None, 50);
    assert_eq!(
        ids(&fresh),
        BTreeSet::from(["mail-1".into(), "mail-2".into()])
    );
    assert_ne!(references(&fresh)[1], references(&pending)[0]);
}

#[test]
fn malformed_endpoints_and_mismatched_registered_door_are_denied() {
    let f = Fixture::new();
    let sub = f.open();
    for (i, (from, to)) in [
        (json!("alpha"), json!(null)),
        (json!(7), json!("captain")),
        (json!("captain"), json!("captain")),
        (json!("alpha"), json!("beta")),
    ]
    .iter()
    .enumerate()
    {
        let id = format!("bad-{i}");
        f.add(&id, i as i64 + 1, "alpha", None);
        f.conn
            .execute(
                "UPDATE desk_intake_fact SET content=?2 WHERE id=?1",
                params![id, json!({"from":from,"to":to}).to_string()],
            )
            .unwrap();
    }
    assert!(ids(&f.pull(&sub, None, 50)).is_empty());
    f.conn
        .execute(
            "UPDATE door_clients SET door='beta' WHERE id=?1",
            [f.c.client()],
        )
        .unwrap();
    assert_eq!(
        f.run_as(&f.c, Operation::Resume { subscription: sub }, 100)
            .unwrap_err()
            .to_string(),
        "denied"
    );
}
