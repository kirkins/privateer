//! Inactive v5 storage tests, not evidence of activated selection semantics.
use super::*;
use crate::door::subscription::selection::{self, Span};
#[path = "selection_review_tests.rs"]
mod review_tests;
#[path = "runtime_tests.rs"]
mod runtime_tests;

fn downgrade_v4(f: &Fixture) {
    f.conn.execute_batch("DROP TABLE door_client_schema_v5;
        CREATE TABLE door_client_schema_v4(singleton INTEGER PRIMARY KEY CHECK(singleton=1));
        DROP TRIGGER desk_subscription_selection_reset; DROP TRIGGER desk_subscription_selection_retire;
        DROP TABLE desk_subscription_history_ref; DROP TABLE desk_subscription_history_receipt;
        DROP TABLE desk_replay_revision; DROP TABLE desk_subscription_selection;
        DROP TABLE desk_subscription_exclusion; DROP TABLE desk_subscription_exception; DROP TABLE desk_subscription_pin;").unwrap();
}
fn state(f: &Fixture) -> Vec<Value> {
    ["door_clients", "desk_subscription", "desk_subscription_replay", "desk_subscription_snapshot",
        "desk_subscription_progress", "desk_intake_original", "door_desk_exposure", "desk_exposure_cut", "desk_exposure_upgrade"]
        .into_iter().map(|table| dump(&f.conn, &format!("SELECT * FROM {table} ORDER BY rowid")))
        .chain([dump(&f.conn,"SELECT ref,subscription,incarnation,core,mailbox,mail,source_order,observation,acked FROM desk_subscription_ref ORDER BY ref"),
            dump(&f.conn,"SELECT key,subscription,expires,batch,outcome FROM desk_subscription_receipt ORDER BY key"),
            dump(&f.conn,"SELECT type,name,sql FROM sqlite_master WHERE tbl_name IN ('desk_subscription_ref','desk_subscription_receipt') ORDER BY type,name")]).collect()
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
fn span(first: i64, last: i64) -> Span {
    Span {
        core: CORE.into(),
        mailbox: BOX.into(),
        first,
        last,
        reasons: selection::UNKNOWN,
        checked: 0,
        through: first - 1,
        scan: 0,
    }
}
fn observation(id: &str, order: i64, state: &str) -> Observation {
    Observation {
        origin: MailIdentity::new(CORE, BOX, id).unwrap(),
        order,
        content: json!({"id":id,"from":"alpha","to":"captain","body":"content"}),
        state: Some(state.into()),
    }
}
fn begin(f: &Fixture) -> rusqlite::Transaction<'_> {
    rusqlite::Transaction::new_unchecked(&f.conn, TransactionBehavior::Immediate).unwrap()
}

#[test]
fn populated_v4_upgrade_and_old_writer_reupgrade_preserve_all_custody_and_output() {
    let f = Fixture::new();
    enable(&f, "established", &format!("{}:10", uuid()));
    for (id, order) in [("acked", 1), ("old-open", 2), ("pending", 3)] {
        f.add(id, order, "alpha", Some("open"));
        original(&f, id, order);
    }
    let s = f.open();
    let page = f.pull(&s, None, 1);
    f.run(f.ack(&s, &page, references(&page)));
    let token = next(&page).unwrap();
    let pending = f.pull(&s, Some(token.clone()), 1);
    let b = replay::binding(&f.conn, &sub(&f, &s)).unwrap().unwrap();
    let cut = saved_cut(&f, &b, false);
    downgrade_v4(&f);
    let before = state(&f);
    super::super::super::super::clients::init(&f.conn).unwrap();
    assert_eq!(state(&f), before);
    assert_eq!(f.pull(&s, Some(token.clone()), 1), pending);
    assert_eq!(selection::revision(&f.conn).unwrap(), 0);
    for table in [
        "desk_subscription_selection",
        "desk_subscription_pin",
        "desk_subscription_exception",
        "desk_subscription_exclusion",
    ] {
        assert_eq!(f.count(table), 0, "inactive: {table}");
    }
    let schema: i64 = f
        .conn
        .pragma_query_value(None, "schema_version", |r| r.get(0))
        .unwrap();
    let changes = f.conn.total_changes();
    super::super::super::super::clients::init(&f.conn).unwrap();
    assert_eq!(changes, f.conn.total_changes());
    assert_eq!(
        schema,
        f.conn
            .pragma_query_value::<i64, _>(None, "schema_version", |r| r.get(0))
            .unwrap()
    );
    // A real v4 writer uses named legacy columns and leaves all new evidence
    // absent. Its retained marker must not cause v5 to skip re-upgrade.
    f.conn
        .execute_batch("DROP TABLE door_client_schema_v5")
        .unwrap();
    f.add("old-writer", 11, "alpha", None);
    f.conn.execute("INSERT INTO desk_intake_original(domain,owner_core,generation,incarnation,core,mailbox,id,first_order) VALUES(?1,?2,1,?3,?2,?4,'old-writer',11)",params![f.m.domain,CORE,f.m.incarnation,BOX]).unwrap();
    super::super::super::super::clients::init(&f.conn).unwrap();
    assert_eq!(class(&f, &s, "old-writer"), DeliveryClass::Unknown);
    assert_eq!(
        f.conn
            .query_row("SELECT body FROM desk_exposure_cut", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        cut
    );
    assert_eq!(f.pull(&s, Some(token), 1), pending);
    // Existing thread/show/ack still use their old automatic namespace; the
    // separate activation PR must switch them, not this schema migration.
    let shown = f.run(Operation::Show {
        subscription: s.clone(),
        origin: MailIdentity::new(CORE, BOX, "old-writer").unwrap(),
    });
    assert!(shown["items"][0].get("delivery_class").is_none());
    assert_eq!(
        f.conn
            .query_row(
                "SELECT COUNT(*) FROM desk_subscription_history_ref",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    f.run(f.ack(&s, &shown, references(&shown)));
    assert_eq!(selection::revision(&f.conn).unwrap(), 0);
}

#[test]
fn v5_migration_failure_rolls_back_ddl_and_preserves_the_outer_writer() {
    let f = Fixture::new();
    let s = f.open();
    f.add("pending", 1, "alpha", None);
    f.pull(&s, None, 1);
    downgrade_v4(&f);
    // An incompatible existing table fails after new history-table creation.
    f.conn
        .execute_batch("CREATE TABLE desk_subscription_exclusion(incompatible TEXT)")
        .unwrap();
    let before = dump(&f.conn, "SELECT * FROM desk_subscription_ref");
    let tx = begin(&f);
    tx.execute(
        "UPDATE desk_subscription SET visibility='outer' WHERE id=?1",
        [&s],
    )
    .unwrap();
    assert!(super::super::super::super::clients::init(&tx).is_err());
    assert_eq!(dump(&tx, "SELECT * FROM desk_subscription_ref"), before);
    assert!(!selection::ready(&tx).unwrap());
    assert_eq!(
        tx.query_row(
            "SELECT visibility FROM desk_subscription WHERE id=?1",
            [&s],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "outer"
    );
    tx.rollback().unwrap();
    assert!(
        !f.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='door_client_schema_v5')",
                [],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
    );
}

#[test]
fn revisit_checkpoints_reserve_exceptions_before_consuming_a_revision_and_restart() {
    let f = Fixture::new();
    let s = f.open();
    let sub = sub(&f, &s);
    let tx = begin(&f);
    selection::exclude(&tx, &sub, &span(1, 100)).unwrap();
    assert_eq!(selection::advance_revision(&tx).unwrap(), 1);
    assert_eq!(selection::start_revisit(&tx, &sub).unwrap(), 1);
    let work = selection::revisit(&tx, &sub).unwrap().unwrap();
    assert!(selection::finish_revisit(&tx, &sub).is_err());
    let o = observation("late-proof", 50, "closed");
    selection::carry(&tx, &sub, &o, DeliveryClass::SinceSwitchOn, "proof").unwrap();
    selection::checkpoint(&tx, &sub, &work, 50).unwrap();
    assert!(
        selection::checkpoint(&tx, &sub, &work, 60).is_err(),
        "stale checkpoint"
    );
    tx.commit().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let reopened = reopen(&f, &dir.path().join("alpha.sqlite"));
    let tx = begin(&reopened);
    let work = selection::revisit(&tx, &sub).unwrap().unwrap();
    assert_eq!(work.through, 50);
    assert_eq!(selection::advance_revision(&tx).unwrap(), 2);
    assert_eq!(
        selection::start_revisit(&tx, &sub).unwrap(),
        1,
        "do not replace pending target"
    );
    selection::checkpoint(&tx, &sub, &work, 100).unwrap();
    selection::finish_revisit(&tx, &sub).unwrap();
    assert_eq!(selection::start_revisit(&tx, &sub).unwrap(), 2);
    let work = selection::revisit(&tx, &sub).unwrap().unwrap();
    assert_eq!(
        work.through, 0,
        "new proof must revisit the entire skipped span"
    );
    // Repeated positive proof changes neither custody nor frozen label.
    let changes = tx.total_changes();
    selection::carry(&tx, &sub, &o, DeliveryClass::Unknown, "proof").unwrap();
    assert_eq!(tx.total_changes(), changes);
    selection::checkpoint(&tx, &sub, &work, 100).unwrap();
    selection::finish_revisit(&tx, &sub).unwrap();
    selection::acknowledge_carried(&tx, &sub, &o).unwrap();
    tx.execute(
        "INSERT INTO desk_subscription_progress VALUES(?1,?2,?3,100)",
        params![s, CORE, BOX],
    )
    .unwrap();
    assert!(
        selection::exclude(&tx, &sub, &span(1, 100)).is_err(),
        "acknowledged hole cannot be re-enclosed"
    );
    assert!(selection::exclude(&tx, &sub, &span(50, 50)).is_err());
    assert!(
        selection::carry(&tx, &sub, &o, DeliveryClass::SinceSwitchOn, "proof").is_err(),
        "actual ack must never be resurrected by repeated proof"
    );
    selection::advance_revision(&tx).unwrap();
    selection::start_revisit(&tx, &sub).unwrap();
    while let Some(work) = selection::revisit(&tx, &sub).unwrap() {
        assert!(
            work.last < 50 || work.first > 50,
            "actual ack is absent from future revisit work"
        );
        selection::checkpoint(&tx, &sub, &work, work.last).unwrap();
    }
    selection::finish_revisit(&tx, &sub).unwrap();
    assert_eq!(
        tx.query_row(
            "SELECT COUNT(*) FROM desk_subscription_exception",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    tx.commit().unwrap();
}

#[test]
fn exception_capacity_keeps_revisit_pending_and_reserves_ack_split_room() {
    let f = Fixture::new();
    let s = f.open();
    let sub = sub(&f, &s);
    let tx = begin(&f);
    selection::exclude(&tx, &sub, &span(1, 100)).unwrap();
    let o = observation("middle", 50, "closed");
    selection::carry(&tx, &sub, &o, DeliveryClass::SinceSwitchOn, "proof").unwrap();
    // 511 spans + 1 carried split reservation fill the fixed budget.
    for n in 0..510 {
        selection::exclude(&tx, &sub, &span(200 + n * 2, 200 + n * 2)).unwrap();
    }
    assert!(
        selection::carry(
            &tx,
            &sub,
            &observation("next", 51, "closed"),
            DeliveryClass::SinceSwitchOn,
            "proof"
        )
        .is_err()
    );
    selection::advance_revision(&tx).unwrap();
    selection::start_revisit(&tx, &sub).unwrap();
    assert!(selection::finish_revisit(&tx, &sub).is_err());
    selection::acknowledge_carried(&tx, &sub, &o).unwrap();
    assert_eq!(
        tx.query_row(
            "SELECT COUNT(*) FROM desk_subscription_exclusion",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        512
    );
    tx.rollback().unwrap();
    assert_eq!(f.count("desk_subscription_exclusion"), 0);
    assert_eq!(f.count("desk_subscription_exception"), 0);
    assert_eq!(
        selection::revision(&f.conn).unwrap(),
        0,
        "proof/work transaction rolls back together"
    );
}

#[test]
fn pins_freeze_class_and_hold_closure_until_all_automatic_custody_is_acked() {
    let f = Fixture::new();
    let s = f.open();
    let sub = sub(&f, &s);
    let tx = begin(&f);
    let open = observation("request", 1, "open");
    let closed = observation("request", 2, "closed");
    assert!(selection::offer_pin(&tx, &sub, &closed, DeliveryClass::Unknown).is_err());
    selection::offer_pin(&tx, &sub, &open, DeliveryClass::Unknown).unwrap();
    selection::carry(&tx, &sub, &open, DeliveryClass::Unknown, "offer").unwrap();
    selection::offer_pin(&tx, &sub, &closed, DeliveryClass::SinceSwitchOn).unwrap();
    selection::carry(&tx, &sub, &closed, DeliveryClass::Unknown, "closure").unwrap();
    assert_eq!(
        tx.query_row(
            "SELECT delivery_class FROM desk_subscription_pin",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "unknown"
    );
    selection::acknowledge_carried(&tx, &sub, &closed).unwrap();
    selection::offer_pin(&tx, &sub, &closed, DeliveryClass::SinceSwitchOn).unwrap();
    selection::retire_closed_pins(&tx, &sub).unwrap();
    assert_eq!(
        tx.query_row("SELECT COUNT(*) FROM desk_subscription_pin", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    selection::acknowledge_carried(&tx, &sub, &open).unwrap();
    selection::retire_closed_pins(&tx, &sub).unwrap();
    assert_eq!(
        tx.query_row("SELECT COUNT(*) FROM desk_subscription_pin", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    tx.commit().unwrap();
}

#[test]
fn v4_readers_ignore_history_custody_even_beyond_automatic_capacity() {
    let f = Fixture::new();
    enable(&f, "established", &format!("{}:10", uuid()));
    f.add("same", 1, "alpha", None);
    f.add("unseen", 2, "alpha", None);
    let s = f.open();
    let sub = sub(&f, &s);
    let page = f.pull(&s, None, 1);
    let automatic = references(&page)[0].clone();
    let history = uuid();
    f.conn.execute("INSERT INTO desk_subscription_history_ref SELECT ?2,subscription,incarnation,core,mailbox,mail,source_order,observation,0 FROM desk_subscription_ref WHERE ref=?1",params![automatic,history]).unwrap();
    let before = state(&f);
    f.conn
        .execute(
            "UPDATE desk_subscription_history_ref SET acked=1 WHERE ref=?1",
            [&history],
        )
        .unwrap();
    let unseen = observation("unseen", 2, "closed");
    for n in 0..300 {
        // One history-acked projected observation must not be a false auto ack.
        // Unacked displaced history bodies must not enter automatic pending.
        let o = if n == 0 {
            unseen.clone()
        } else {
            observation(&format!("history-{n}"), n + 2, "closed")
        };
        f.conn
            .execute(
                "INSERT INTO desk_subscription_history_ref VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    uuid(),
                    s,
                    sub.incarnation,
                    CORE,
                    BOX,
                    o.origin.id,
                    o.order,
                    serde_json::to_string(&o).unwrap(),
                    i64::from(n == 0)
                ],
            )
            .unwrap();
    }
    let history_key = uuid();
    for n in 0..150 {
        f.conn.execute("INSERT INTO desk_subscription_history_receipt(key,subscription,incarnation,expires) VALUES(?1,?2,?3,1000)",params![if n==0 {history_key.clone()} else {uuid()},s,sub.incarnation]).unwrap();
    }
    assert_eq!(state(&f), before, "history ack changes no automatic bytes");
    // Drop only the v5 marker to model a rollback. All operations below use
    // unchanged v4 code paths, which have no knowledge of the history tables.
    f.conn
        .execute_batch("DROP TABLE door_client_schema_v5")
        .unwrap();
    assert!(!acknowledged(&f.conn, &sub, &unseen).unwrap());
    let all = f.pull(&s, None, 50);
    assert_eq!(ids(&all), BTreeSet::from(["same".into(), "unseen".into()]));
    assert!(f.run_as(&f.c, f.ack(&s, &all, vec![history]), 100).is_err());
    assert!(
        f.run_as(
            &f.c,
            Operation::Ack {
                subscription: s.clone(),
                key: history_key,
                refs: vec![automatic]
            },
            100
        )
        .is_err()
    );
    f.run(f.ack(&s, &page, references(&page)));
    assert_eq!(through(&f.conn, &sub, &unseen).unwrap(), 1);
    assert!(!acknowledged(&f.conn, &sub, &unseen).unwrap());
    assert_eq!(
        // Both frozen snapshots still retain unseen; continue the first rather
        // than exceeding the unchanged two-pending-snapshot admission limit.
        ids(&f.pull(&s, next(&page), 50)),
        BTreeSet::from(["unseen".into()])
    );
    assert_eq!(f.count("desk_subscription_history_ref"), 301);
    assert_eq!(f.count("desk_subscription_history_receipt"), 150);
}

#[test]
fn exclusion_coalescing_preserves_reason_flags_and_does_not_rewind_live_work() {
    let f = Fixture::new();
    let s = f.open();
    let sub = sub(&f, &s);
    let tx = begin(&f);
    let mut history = span(1, 2);
    history.reasons = selection::HISTORY;
    let mut delayed = span(3, 4);
    delayed.reasons = selection::DELAYED;
    selection::exclude(&tx, &sub, &history).unwrap();
    selection::exclude(&tx, &sub, &delayed).unwrap();
    selection::advance_revision(&tx).unwrap();
    selection::start_revisit(&tx, &sub).unwrap();
    let work = selection::revisit(&tx, &sub).unwrap().unwrap();
    assert_eq!(
        (work.first, work.last, work.reasons),
        (1, 4, selection::HISTORY | selection::DELAYED)
    );
    selection::checkpoint(&tx, &sub, &work, 2).unwrap();
    let before = tx.total_changes();
    selection::exclude(&tx, &sub, &history).unwrap();
    assert_eq!(tx.total_changes(), before);
    assert_eq!(selection::revisit(&tx, &sub).unwrap().unwrap().through, 2);
    tx.commit().unwrap();
}

#[test]
fn ledger_rejects_oversized_custody_and_untransactional_or_stale_writes() {
    let f = Fixture::new();
    let s = f.open();
    let mut sub = sub(&f, &s);
    assert!(selection::exclude(&f.conn, &sub, &span(1, 1)).is_err());
    let tx = begin(&f);
    let mut o = observation("large", 1, "closed");
    o.content["body"] = json!("x".repeat(16 * 1024 * 1024));
    assert!(selection::carry(&tx, &sub, &o, DeliveryClass::Unknown, "proof").is_err());
    assert_eq!(
        tx.query_row(
            "SELECT COUNT(*) FROM desk_subscription_exception",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    sub.incarnation = uuid();
    assert!(selection::exclude(&tx, &sub, &span(1, 1)).is_err());
    tx.rollback().unwrap();
}
