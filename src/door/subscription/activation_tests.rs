//! Serving-level activation matrix. Helpers below never expose a command.
use super::*;

fn request(f: &Fixture, id: &str, order: i64, state: &str) {
    f.add(id, order, "alpha", Some(state));
    f.conn
        .execute(
            "UPDATE desk_intake_fact SET content=json_set(content,'$.kind','request') WHERE id=?1",
            [id],
        )
        .unwrap();
}
fn dump(conn: &Connection, sql: &str) -> Value {
    let mut stmt = conn.prepare(sql).unwrap();
    let columns = stmt.column_count();
    json!(
        stmt.query_map([], |r| Ok((0..columns)
            .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
            .collect::<Vec<_>>()))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    )
}
fn automatic(f: &Fixture) -> Vec<Value> {
    ["desk_subscription_ref", "desk_subscription_receipt", "desk_subscription_progress",
     "desk_subscription_selection", "desk_subscription_exclusion", "desk_subscription_exception", "desk_subscription_pin"]
        .into_iter().map(|table| dump(&f.conn, &format!("SELECT * FROM {table} ORDER BY rowid")))
        .chain([dump(&f.conn, "SELECT * FROM desk_subscription_snapshot WHERE COALESCE(json_extract(body,'$.history'),0)=0 ORDER BY rowid")]).collect()
}
fn show(s: &str, id: &str) -> Operation {
    Operation::Show {
        subscription: s.into(),
        origin: MailIdentity::new(CORE, BOX, id).unwrap(),
    }
}
fn thread(s: &str, id: &str, token: Option<String>, items: usize) -> Operation {
    Operation::Thread {
        subscription: s.into(),
        origin: MailIdentity::new(CORE, BOX, id).unwrap(),
        continuation: token,
        items,
    }
}

#[test]
fn large_old_and_unknown_baseline_is_excluded_before_snapshot_refs_and_receipts() {
    let f = Fixture::new();
    enable(&f, "established", &format!("{}:1000", uuid()));
    for n in 1..=1000 {
        let id = format!("old-{n}");
        f.add(&id, n, "alpha", Some("closed"));
        original(&f, &id, n);
    }
    request(&f, "outgoing", 1001, "open");
    f.conn.execute("UPDATE desk_intake_fact SET content=json_set(content,'$.from','captain','$.to','alpha') WHERE id='outgoing'", []).unwrap();
    f.fact("unknown-notice", 1002, "alpha", None);
    request(&f, "incoming", 1003, "open");
    request(&f, "decision", 1004, "open");
    f.conn.execute("UPDATE desk_intake_fact SET content=json_set(content,'$.kind','decision') WHERE id='decision'", []).unwrap();
    let s = f.open();
    let p = f.pull(&s, None, ITEMS);
    assert_eq!(
        ids(&p),
        BTreeSet::from(["incoming".into(), "decision".into()])
    );
    assert_eq!(f.count("desk_subscription_ref"), 2);
    assert_eq!(f.count("desk_subscription_exception"), 2);
    assert_eq!(f.count("desk_subscription_pin"), 2);
    assert_eq!(f.count("desk_subscription_receipt"), 2);
    assert_eq!(p["replay_selection"]["history_excluded"], 1000);
    assert_eq!(p["replay_selection"]["unknown_proof_withheld"], 2);
    for item in p["items"].as_array().unwrap() {
        assert_eq!(item["state"], "open");
        assert_eq!(item["delivery_class"], "unknown");
    }
    f.run(f.ack(&s, &p, references(&p)));
    assert_eq!(
        f.conn
            .query_row(
                "SELECT through_order FROM desk_subscription_progress",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1004
    );
    assert!(ids(&f.pull(&s, None, ITEMS)).is_empty());
}

#[test]
fn snapshot_offer_pins_unserved_open_rows_and_carries_closure_across_expiry_and_lost_ack() {
    let f = Fixture::new();
    enable(&f, "switch_on_pending", "");
    request(&f, "incoming", 1, "open");
    request(&f, "decision", 2, "open");
    let s = f.open();
    let first = f.pull(&s, None, 1);
    assert_eq!(
        f.count("desk_subscription_pin"),
        2,
        "the unserved second row is offered too"
    );
    let resumed = f.run(Operation::Resume {
        subscription: s.clone(),
    });
    let token = resumed["pending_snapshots"][0]["continuation"]
        .as_str()
        .unwrap()
        .into();
    assert_eq!(f.pull(&s, Some(token), 1), first);
    request(&f, "incoming", 3, "closed");
    request(&f, "decision", 4, "closed");
    let now = 100 + SNAPSHOT_TTL;
    f.run_as(
        &f.c,
        Operation::Resume {
            subscription: s.clone(),
        },
        now,
    )
    .unwrap();
    let p = f
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
    assert_eq!(
        p["items"].as_array().unwrap().len(),
        4,
        "displaced opens and both closures survive"
    );
    assert!(
        p["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["delivery_class"] == "unknown")
    );
    let closed: Vec<String> = p["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["state"] == "closed")
        .map(|i| i["ref"].as_str().unwrap().into())
        .collect();
    let ack = f.run_as(&f.c, f.ack(&s, &p, closed.clone()), now).unwrap();
    assert_eq!(f.run_as(&f.c, f.ack(&s, &p, closed), now).unwrap(), ack);
    assert_eq!(
        f.count("desk_subscription_pin"),
        2,
        "unacked displaced opens keep pins"
    );
    let resume = f
        .run_as(
            &f.c,
            Operation::Resume {
                subscription: s.clone(),
            },
            now,
        )
        .unwrap();
    let open: Vec<String> = p["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["state"] == "open")
        .map(|i| i["ref"].as_str().unwrap().into())
        .collect();
    let key = resume["ack_key"].as_str().unwrap().into();
    f.run_as(
        &f.c,
        Operation::Ack {
            subscription: s.clone(),
            key,
            refs: open,
        },
        now,
    )
    .unwrap();
    assert_eq!(f.count("desk_subscription_pin"), 0);
    assert_eq!(
        f.conn
            .query_row(
                "SELECT COUNT(*) FROM desk_intake_fact WHERE state='closed'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        2,
        "raw ack never changes origin lifecycle"
    );
    assert!(
        ids(&f
            .run_as(
                &f.c,
                Operation::Pull {
                    subscription: s,
                    continuation: None,
                    items: ITEMS
                },
                now
            )
            .unwrap())
        .is_empty()
    );
}

#[test]
fn show_refuses_guessed_history_but_allows_carried_origin_and_frozen_unknown_label() {
    let f = Fixture::new();
    enable(&f, "switch_on_pending", "");
    f.add("old", 1, "alpha", Some("closed"));
    request(&f, "open", 2, "open");
    let s = f.open();
    assert_eq!(
        f.run_as(&f.c, show(&s, "old"), 100)
            .unwrap_err()
            .to_string(),
        "stale_or_unknown"
    );
    let p = f.pull(&s, None, ITEMS);
    request(&f, "open", 3, "closed");
    let shown = f.run(show(&s, "open"));
    assert_eq!(shown["items"][0]["delivery_class"], "unknown");
    assert_eq!(shown["items"][0]["state"], "closed");
    assert_eq!(p["items"][0]["state"], "open");
    assert_eq!(f.count("desk_subscription_pin"), 1);
}

#[test]
fn bounded_history_windows_and_history_ack_leave_automatic_custody_byte_identical() {
    let f = Fixture::new();
    enable(&f, "switch_on_pending", "");
    for n in 1..=snapshot::PAGE_ROWS * 3 + 1 {
        f.add(&format!("mail-{n}"), n as i64, "alpha", Some("closed"));
    }
    f.conn
        .execute(
            "UPDATE desk_intake_fact SET content=json_set(content,'$.correlation','mail-1')",
            [],
        )
        .unwrap();
    request(&f, "incoming", 1000, "open");
    let s = f.open();
    let auto = f.pull(&s, None, ITEMS);
    let before = automatic(&f);
    let mut page = f.run(thread(&s, "mail-1", None, ITEMS));
    let mut seen = BTreeSet::new();
    loop {
        seen.extend(ids(&page));
        assert!(
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|i| i["delivery_class"] == "history")
        );
        assert_eq!(automatic(&f), before);
        assert!(
            f.run_as(&f.c, f.ack(&s, &auto, references(&page)), 100)
                .is_err(),
            "automatic key cannot acknowledge history handles"
        );
        f.run(f.ack(&s, &page, references(&page)));
        assert_eq!(automatic(&f), before);
        let Some(token) = next(&page) else { break };
        page = f.run(thread(&s, "mail-1", Some(token), ITEMS));
        assert!(f.count("desk_subscription_history_ref") <= (snapshot::PAGE_ROWS * 2) as i64);
        assert!(f.count("desk_subscription_history_receipt") <= RECEIPTS);
    }
    assert_eq!(seen.len(), snapshot::PAGE_ROWS * 3 + 1);
    assert_eq!(automatic(&f), before);
    f.run(f.ack(&s, &auto, references(&auto)));
}

#[test]
fn delayed_cut_withholding_and_diagnostics_are_frozen_and_sanitized_on_visibility_reset() {
    let f = Fixture::new();
    enable(&f, "switch_on_pending", "");
    f.fact("interval", 1, "alpha", None);
    original(&f, "interval", 1);
    let s = f.open();
    let b = replay::binding(&f.conn, &sub(&f, &s)).unwrap().unwrap();
    saved_cut(&f, &b, true);
    let p = f.pull(&s, None, ITEMS);
    assert!(ids(&p).is_empty());
    assert_eq!(p["replay_selection"]["delayed_cut_excluded"], 1);
    assert_eq!(p["replay_selection"]["history_excluded"], 0);
    let raw = p["replay_selection"].to_string();
    for forbidden in [
        "journal_upper",
        "vector_digest",
        "mail_upper",
        "baseline_upper",
        "nonce",
        "home",
        "checkpoint",
    ] {
        assert!(!raw.contains(forbidden), "{forbidden}");
    }
    let resumed = f.run(Operation::Resume {
        subscription: s.clone(),
    });
    let token = resumed["pending_snapshots"][0]["continuation"]
        .as_str()
        .unwrap()
        .to_owned();
    f.conn
        .execute(
            "UPDATE desk_exposure_cut SET body=json_set(body,'$.delayed',json('false'))",
            [],
        )
        .unwrap();
    assert_eq!(
        f.pull(&s, Some(token.clone()), ITEMS),
        p,
        "new evidence cannot change a frozen diagnostic"
    );
    let c = DelegatedContext::resolved(
        f.c.client().into(),
        "door:alpha".into(),
        vec!["mail:read".into()],
        Some(vec!["beta".into()]),
    );
    f.grant(&c);
    let restricted = f
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
    assert!(ids(&restricted).is_empty());
    assert_eq!(
        restricted["replay_selection"]["history_excluded"],
        Value::Null
    );
    assert!(restricted["replay_selection"]["sources"][0]["cuts"].is_null());
    assert!(
        f.run_as(
            &c,
            Operation::Pull {
                subscription: s,
                continuation: Some(token),
                items: ITEMS
            },
            100
        )
        .is_err()
    );
}

#[test]
fn serving_checkpoints_bounded_revisit_and_recovers_after_ack_and_restart() {
    let f = Fixture::new();
    enable(&f, "switch_on_pending", "");
    for n in 1..=257 {
        request(&f, &format!("request-{n}"), n, "open");
    }
    let s = f.open();
    let subscription = sub(&f, &s);
    let tx = rusqlite::Transaction::new_unchecked(&f.conn, TransactionBehavior::Immediate).unwrap();
    selection::exclude(&tx, &subscription, &selection_tests::span(1, 300)).unwrap();
    tx.execute(
        "INSERT INTO desk_subscription_progress VALUES(?1,?2,?3,300)",
        params![s, CORE, BOX],
    )
    .unwrap();
    selection::advance_revision(&tx).unwrap();
    tx.commit().unwrap();
    let first = f.pull(&s, None, ITEMS);
    assert_eq!(first["replay_selection"]["bounded_work_pending"], true);
    assert_eq!(f.count("desk_subscription_exception"), 128);
    let second = f.pull(&s, None, ITEMS);
    assert_eq!(second["replay_selection"]["bounded_work_pending"], true);
    assert_eq!(f.count("desk_subscription_exception"), 256);
    assert!(
        f.run_as(
            &f.c,
            Operation::Pull {
                subscription: s.clone(),
                continuation: None,
                items: ITEMS
            },
            100
        )
        .is_err()
    );
    assert_eq!(
        f.conn
            .query_row(
                "SELECT consumed_revision,target_revision FROM desk_subscription_selection",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            )
            .unwrap(),
        (0, 1)
    );
    let mut page = first;
    loop {
        f.run(f.ack(&s, &page, references(&page)));
        let Some(token) = next(&page) else { break };
        page = f.pull(&s, Some(token), ITEMS);
    }
    let dir = tempfile::tempdir().unwrap();
    let restarted = reopen(&f, &dir.path().join("alpha.sqlite"));
    let p = restarted.pull(&s, None, ITEMS);
    assert!(!ids(&p).is_empty());
    assert_eq!(
        restarted
            .conn
            .query_row(
                "SELECT consumed_revision,target_revision IS NULL FROM desk_subscription_selection",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, bool>(1)?))
            )
            .unwrap(),
        (1, true)
    );
    assert!(
        restarted
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM desk_subscription_exception WHERE mail='request-257')",
                [],
                |r| r.get::<_, bool>(0)
            )
            .unwrap(),
        "revision consumption follows reserving the final exception"
    );
}

#[test]
fn full_carry_capacity_returns_existing_custody_before_reserving_early_fresh_offers() {
    let f = Fixture::new();
    enable(&f, "switch_on_pending", "");
    request(&f, "early", 1, "open");
    for n in 0..256 {
        f.fact(&format!("carried-{n}"), 1000 + n, "alpha", Some("closed"));
    }
    let s = f.open();
    let subscription = sub(&f, &s);
    let tx = rusqlite::Transaction::new_unchecked(&f.conn, TransactionBehavior::Immediate).unwrap();
    for n in 0..256 {
        let o = intake::observation(
            &tx,
            &f.m,
            &MailIdentity::new(CORE, BOX, &format!("carried-{n}")).unwrap(),
        )
        .unwrap()
        .unwrap();
        selection::carry(&tx, &subscription, &o, DeliveryClass::Unknown, "offer").unwrap();
    }
    tx.commit().unwrap();
    let mut page = f.pull(&s, None, ITEMS);
    assert!(
        !ids(&page).is_empty(),
        "full carry must not strand custody behind a fresh early offer"
    );
    assert!(!ids(&page).contains("early"));
    assert_eq!(page["replay_selection"]["bounded_work_pending"], true);
    assert_eq!(f.count("desk_subscription_exception"), 256);
    loop {
        f.run(f.ack(&s, &page, references(&page)));
        let Some(token) = next(&page) else { break };
        page = f.pull(&s, Some(token), ITEMS);
    }
    assert!(
        ids(&f.pull(&s, None, ITEMS)).contains("early"),
        "ack freed real capacity for the withheld fresh request"
    );
}

#[test]
fn history_and_automatic_receipt_capacity_are_independent() {
    for history_full in [false, true] {
        let f = Fixture::new();
        enable(&f, "switch_on_pending", "");
        request(&f, "incoming", 1, "open");
        f.fact("history", 2, "alpha", Some("closed"));
        let s = f.open();
        let table = if history_full {
            "desk_subscription_history_receipt"
        } else {
            "desk_subscription_receipt"
        };
        let incarnation = sub(&f, &s).incarnation;
        f.conn.execute(&format!("DELETE FROM {table}"), []).unwrap();
        for _ in 0..RECEIPTS {
            if history_full {
                f.conn.execute("INSERT INTO desk_subscription_history_receipt VALUES(?1,?2,?3,?4,'[]','{}')",params![uuid(),s,incarnation,RECEIPT_TTL]).unwrap();
            } else {
                f.conn
                    .execute(
                        "INSERT INTO desk_subscription_receipt VALUES(?1,?2,?3,'[]','{}')",
                        params![uuid(), s, RECEIPT_TTL],
                    )
                    .unwrap();
            }
        }
        let op = if history_full {
            Operation::Pull {
                subscription: s.clone(),
                continuation: None,
                items: ITEMS,
            }
        } else {
            thread(&s, "history", None, ITEMS)
        };
        let page = f.run(op);
        assert_eq!(references(&page).len(), 1);
        f.run(f.ack(&s, &page, references(&page)));
        assert_eq!(
            f.count(table),
            RECEIPTS,
            "the opposite namespace cannot evict full receipts"
        );
    }
}

#[test]
fn genuinely_frozen_legacy_snapshot_and_output_are_byte_identical_after_restart() {
    let f = Fixture::new();
    enable(&f, "switch_on_pending", "");
    f.fact("legacy-notice", 1, "alpha", None);
    f.fact("legacy-outgoing", 2, "alpha", Some("closed"));
    let s = f.open();
    let subscription = sub(&f, &s);
    let rows: Vec<Observation> = ["legacy-notice", "legacy-outgoing"]
        .into_iter()
        .map(|id| {
            intake::observation(&f.conn, &f.m, &MailIdentity::new(CORE, BOX, id).unwrap())
                .unwrap()
                .unwrap()
        })
        .collect();
    let secret = uuid();
    let id = uuid();
    let key = uuid();
    let body = json!({"secret":secret,"rows":rows,"older":false,"predecessor":null}).to_string();
    let operation = json!([
        "pull",
        null,
        f.m.manifest_revision,
        subscription.incarnation,
        Owner::of(&f.m),
        visibility(&f.c)
    ])
    .to_string();
    let mut coverage = f.run(Operation::Resume {
        subscription: s.clone(),
    })["coverage"]
        .clone();
    coverage["snapshot"] = json!(id);
    coverage["observed_at"] = json!(100);
    ack::reserve_named(&f.conn, &subscription, &key, Some(&id), 100).unwrap();
    f.conn
        .execute(
            "INSERT INTO desk_subscription_snapshot VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                id,
                s,
                operation,
                100 + SNAPSHOT_TTL,
                body,
                coverage.to_string(),
                key
            ],
        )
        .unwrap();
    let token = format!("{id}:0:{}", crate::core::hash(&json!([secret, 0])));
    let first = f.pull(&s, Some(token.clone()), 1);
    assert_eq!(ids(&first).len(), 1);
    assert!(first.get("replay_selection").is_none());
    assert!(first["items"][0].get("delivery_class").is_none());
    assert_eq!(first["items"][0]["state"], Value::Null);
    let dir = tempfile::tempdir().unwrap();
    let restarted = reopen(&f, &dir.path().join("alpha.sqlite"));
    assert_eq!(
        restarted.pull(&s, Some(token), 1).to_string(),
        first.to_string()
    );
    let saved: String = restarted
        .conn
        .query_row(
            "SELECT body FROM desk_subscription_snapshot WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(saved, body);
    assert_eq!(restarted.count("desk_subscription_selection"), 0);
    // The legacy snapshot also offered its unserved second row. New policy
    // cannot exclude it or compact over it merely because no raw ref exists.
    request(&restarted, "later", 3, "open");
    let fresh = restarted.pull(&s, None, ITEMS);
    assert_eq!(
        ids(&fresh),
        BTreeSet::from([
            "legacy-notice".into(),
            "legacy-outgoing".into(),
            "later".into()
        ])
    );
    restarted.run(restarted.ack(&s, &first, references(&first)));
    let later: Vec<String> = fresh["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["origin"]["id"] == "later")
        .map(|i| i["ref"].as_str().unwrap().into())
        .collect();
    restarted.run(restarted.ack(&s, &fresh, later));
    assert_eq!(
        restarted
            .conn
            .query_row(
                "SELECT through_order FROM desk_subscription_progress",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1,
        "unserved legacy offer still blocks the prefix"
    );
}

#[test]
fn null_binding_guard_admits_only_open_incoming_and_retains_legacy_and_closure_custody() {
    let f = Fixture::new();
    f.fact("notice", 1, "alpha", None);
    f.fact("closed", 2, "alpha", Some("closed"));
    request(&f, "incoming", 3, "open");
    request(&f, "outgoing", 4, "open");
    f.conn.execute("UPDATE desk_intake_fact SET content=json_set(content,'$.from','captain','$.to','alpha') WHERE id='outgoing'",[]).unwrap();
    let s = f.open();
    f.retain_legacy(&s, "closed");
    let p = f.pull(&s, None, ITEMS);
    assert_eq!(
        ids(&p),
        BTreeSet::from(["closed".into(), "incoming".into()])
    );
    assert_eq!(f.count("desk_subscription_pin"), 1);
    f.run(f.ack(&s, &p, references(&p)));
    request(&f, "incoming", 5, "closed");
    let closure = f.pull(&s, None, ITEMS);
    assert_eq!(ids(&closure), BTreeSet::from(["incoming".into()]));
    assert_eq!(closure["items"][0]["delivery_class"], "unknown");
    assert_eq!(closure["items"][0]["state"], "closed");
    f.run(f.ack(&s, &closure, references(&closure)));
    assert_eq!(f.count("desk_subscription_pin"), 0);
    // NULL-binding explicit lookup retains its previous behavior.
    assert_eq!(
        ids(&f.run(show(&s, "notice"))),
        BTreeSet::from(["notice".into()])
    );
}

#[test]
fn serving_revisits_native_and_copy_first_proof_after_prefix_compaction_and_restart() {
    for copy in [false, true] {
        let mut remote = intake::ReplayFixture::remote();
        super::super::super::super::clients::init(&remote.desk).unwrap();
        let m = remote.m.clone();
        coverage::install_placement_manifest(
            &remote.desk,
            &m.domain,
            m.max_source_age_ms,
            &m.sources
                .iter()
                .map(|s| (s.core.clone(), s.ships.clone()))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let c =
            DelegatedContext::resolved(uuid(), "door:alpha".into(), vec!["mail:read".into()], None);
        let f = Fixture {
            conn: Connection::open_in_memory().unwrap(),
            m: m.clone(),
            c: DelegatedContext::from_gateway(&c.gateway()).unwrap(),
        };
        // Use the real source/desk databases for proof and the regular engine's
        // grant/admission helpers after moving the desk connection into Fixture.
        let binding = json!({"domain":m.domain,"owner_core":m.owner_core,"generation":m.generation,"incarnation":m.incarnation,"manifest_revision":m.manifest_revision,"manifest_hash":crate::core::hash(&json!(m)),"mode":"fresh_replay","replay_policy":"open_and_new","switch_on_ms":1000,"switch_on":[]});
        remote.desk.execute_batch("CREATE TABLE door_desk_exposure(door TEXT PRIMARY KEY,enabled INTEGER,binding TEXT)").unwrap();
        remote
            .desk
            .execute(
                "INSERT INTO door_desk_exposure VALUES('alpha',1,?1)",
                [binding.to_string()],
            )
            .unwrap();
        remote.import_latest(false);
        remote.capture_boundary(&binding);
        remote.send("late");
        let wire = remote.stage_latest(copy);
        let desk = std::mem::replace(&mut remote.desk, Connection::open_in_memory().unwrap());
        let f = Fixture { conn: desk, ..f };
        f.grant(&f.c);
        let s = f.open();
        // A later incoming request advances the real automatic prefix over the
        // withheld notice, with durable exclusion evidence, not a fabricated ack.
        let (mailbox, order): (String, i64) = f
            .conn
            .query_row(
                "SELECT mailbox,source_order FROM desk_intake_fact WHERE id='late'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        f.conn.execute("INSERT INTO desk_intake_fact(domain,owner_core,generation,incarnation,source,core,mailbox,id,source_order,content,state,unread,pending) VALUES(?1,?2,1,?3,?4,?4,?5,'incoming',?6,?7,'open',1,1)",params![m.domain,m.owner_core,m.incarnation,CORE,mailbox,order+10,json!({"id":"incoming","from":m.sources[0].ships[0],"to":"captain","kind":"request"}).to_string()]).unwrap();
        let p = f.pull(&s, None, ITEMS);
        assert_eq!(ids(&p), BTreeSet::from(["incoming".into()]));
        f.run(f.ack(&s, &p, references(&p)));
        assert_eq!(
            f.conn
                .query_row(
                    "SELECT through_order FROM desk_subscription_progress",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            order + 10
        );
        remote.desk = f.conn;
        remote.adopt(&wire).unwrap();
        let desk = std::mem::replace(&mut remote.desk, Connection::open_in_memory().unwrap());
        let f = Fixture { conn: desk, m, c };
        let dir = tempfile::tempdir().unwrap();
        let restarted = reopen(&f, &dir.path().join("alpha.sqlite"));
        let new = restarted.pull(&s, None, ITEMS);
        assert_eq!(ids(&new), BTreeSet::from(["late".into()]));
        assert_eq!(new["items"][0]["delivery_class"], "since_switch_on");
        restarted.run(restarted.ack(&s, &new, references(&new)));
        remote.desk = restarted.conn;
        remote.adopt(&wire).unwrap();
        let desk = std::mem::replace(&mut remote.desk, Connection::open_in_memory().unwrap());
        let final_f = Fixture {
            conn: desk,
            m: remote.m.clone(),
            c: DelegatedContext::from_gateway(&f.c.gateway()).unwrap(),
        };
        assert!(
            ids(&final_f.pull(&s, None, ITEMS)).is_empty(),
            "identical proof cannot resurrect the punched ack"
        );
    }
}
