use super::*;

#[test]
fn baseline_larger_than_details_and_non_native_deltas_leave_witness_capacity() {
    let mut f = Fixture::new(true);
    let baseline = source::enrollment::open(&f.dir.path().join("alpha"), false).unwrap();
    let event: String = baseline
        .query_row("SELECT event FROM mail_source_import LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let original: HomeFact = serde_json::from_str(&event).unwrap();
    let o = Observation {
        home: crate::core::hash(&json!(f.dir.path().join("alpha"))),
        mailbox: original.mailbox.clone(),
        nonce: None,
        lane: "baseline".into(),
        delta_order: None,
        mail_order: None,
        native: false,
    };
    // Exercise the ledger across the equivalent of many reclaimed source
    // pages, without retaining a history-sized journal/receipt fixture.
    let tx = f.r.unchecked_transaction().unwrap();
    for n in 2..=DETAILS + 2 {
        let mut fact = original.clone();
        fact.id = format!("baseline-{n}");
        fact.provenance =
            Provenance::Origin(MailIdentity::new(ALPHA, &fact.mailbox, &fact.id).unwrap());
        fact.content["id"] = json!(fact.id);
        record(
            &tx,
            &f.m,
            ALPHA,
            &format!("baseline-intent-{n}"),
            &serde_json::to_string(&fact).unwrap(),
            n,
            Some(&o),
        )
        .unwrap();
        fact.provenance =
            Provenance::Copy(MailIdentity::new(ALPHA, &fact.mailbox, &fact.id).unwrap());
        let delta = Observation {
            lane: "delta".into(),
            ..o.clone()
        };
        record(
            &tx,
            &f.m,
            ALPHA,
            &format!("copy-intent-{n}"),
            &serde_json::to_string(&fact).unwrap(),
            n + DETAILS + 1,
            Some(&delta),
        )
        .unwrap();
    }
    tx.execute(
        "UPDATE desk_source_head SET upper_seq=?1",
        [2 * DETAILS + 3],
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(count(&f.r, "desk_replay_import").unwrap(), 0);
    assert_eq!(count(&f.r, "desk_replay_home").unwrap(), 0);
    assert_eq!(count(&f.r, "desk_replay_omission").unwrap(), 0);
    f.send("new-after-large-baseline");
    f.capture();
    assert_eq!(f.witness().origin.id, "new-after-large-baseline");
    assert_eq!(count(&f.r, "desk_replay_import").unwrap(), 1);
}

#[test]
fn native_lifecycle_update_is_not_another_first_content_witness() {
    let mut f = Fixture::new(false);
    f.send("new");
    f.capture();
    let w = f.witness();
    crate::mail::dispatch(
        &mut f.h,
        "mail.withdraw",
        json!({"id":"new","actor":"survey","reason":"done"}),
    )
    .unwrap();
    f.capture();
    let n: i64 =
        f.r.query_row(
            "SELECT COUNT(*) FROM desk_replay_witness WHERE json_extract(body,'$.origin.id')='new'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(candidate(&f.r, &f.m, ALPHA, &w.intent).unwrap(), Some(w));
    let details: i64 =
        f.r.query_row(
            "SELECT COUNT(*) FROM desk_replay_import WHERE json_extract(origin,'$.id')='new'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(details, 1);
    assert_eq!(count(&f.r, "desk_replay_omission").unwrap(), 0);
}

#[test]
fn pressure_ranges_are_exact_reason_specific_then_explicitly_widen() {
    let mut f = Fixture::new(false);
    f.send("witnessed-gap");
    f.capture();
    let witnessed = f.witness().journal_order;
    for why in ["witness_budget", "detail_budget", "rebuild_unavailable"] {
        omitted(&f.r, &f.m, ALPHA, witnessed - 1, why).unwrap();
        omitted(&f.r, &f.m, ALPHA, witnessed + 1, why).unwrap();
    }
    let rows: Vec<(i64, i64, bool)> =
        f.r.prepare("SELECT first_order,last_order,enclosing FROM desk_replay_omission")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
    assert_eq!(rows.len(), 6);
    assert!(
        rows.iter()
            .all(|(a, b, enclosing)| a == b && *a != witnessed && !enclosing)
    );
    // Adjacency coalesces exactly, not across the witnessed gap.
    omitted(&f.r, &f.m, ALPHA, witnessed + 2, "detail_budget").unwrap();
    assert_eq!(count(&f.r, "desk_replay_omission").unwrap(), 6);
    for n in 2..=storage::range_budget() {
        // A retained candidate splits the omission run. Empty journal orders
        // no longer spend range slots merely because they are non-adjacent.
        let barrier = witnessed + n * 10 - 1;
        f.r.execute("INSERT INTO desk_replay_import(domain,source,binding,intent,home,mailbox,nonce,lane,journal_order,origin,digest,event_digest,delta_order,mail_order,native)
            SELECT domain,source,binding,?1,home,mailbox,nonce,lane,?2,origin,digest,event_digest,delta_order,mail_order,native FROM desk_replay_import LIMIT 1",
            params![format!("barrier-{n}"),barrier]).unwrap();
        omitted(&f.r, &f.m, ALPHA, witnessed + n * 10, "detail_budget").unwrap();
    }
    let rows: Vec<(i64,i64,bool)> = f.r.prepare("SELECT first_order,last_order,enclosing FROM desk_replay_omission WHERE reason='detail_budget'").unwrap()
        .query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    assert_eq!(
        rows,
        vec![(
            witnessed - 1,
            witnessed + storage::range_budget() * 10,
            true
        )]
    );
    assert_eq!(count(&f.r, "desk_replay_omission").unwrap(), 5);
}

#[test]
fn global_range_pressure_keeps_separate_reserved_reason_fallbacks() {
    let f = Fixture::new(false);
    f.r.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<?1)
        INSERT INTO desk_replay_omission SELECT 'filler',CAST(x AS TEXT),'binding',1,1,'detail_budget',0 FROM n", [DETAILS]).unwrap();
    for why in ["witness_budget", "detail_budget", "rebuild_unavailable"] {
        omitted(&f.r, &f.m, ALPHA, 4, why).unwrap();
        omitted(&f.r, &f.m, ALPHA, 8, why).unwrap();
    }
    assert_eq!(count(&f.r, "desk_replay_omission").unwrap(), DETAILS + 3);
    let n: i64 = f.r.query_row("SELECT COUNT(*) FROM desk_replay_omission WHERE domain='*' AND first_order=4 AND last_order=8 AND enclosing=1", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 3);
}

#[test]
fn symlinked_inventory_home_matches_reconcile_and_rebuild() {
    let mut f = Fixture::new(false);
    let link = f.dir.path().join("beta");
    std::os::unix::fs::symlink(f.dir.path().join("alpha"), &link).unwrap();
    f.send("new");
    source::stage_deltas_until(f.h.connection(), None).unwrap();
    // The home connection is already authorized/open. Test the inventory
    // spelling at the import boundary, not a forbidden symlink filesystem
    // open: capture_ready_until and census must retain their symlink fence.
    super::super::super::super::home::import_pending(
        &link,
        f.h.connection(),
        &f.r,
        &f.m,
        ALPHA,
        None,
    )
    .unwrap();
    let w = f.witness();
    assert_eq!(w.home, crate::core::hash(&json!(link)));
    // Reconciliation also needs the ordinary machine-registry schema, as in
    // reconcile_tests::registry; the ledger-only Fixture omits those tables.
    crate::machines::init(&f.r).unwrap();
    f.r.execute_batch("CREATE TABLE ships(name TEXT,home TEXT,retired_ms INTEGER);")
        .unwrap();
    super::super::super::super::home::retain_home(&f.r, &link).unwrap();
    let proof =
        super::super::super::super::reconcile::inspect(&f.r, &f.m, ALPHA, &[], deadline()).unwrap();
    let proof = serde_json::to_value(proof).unwrap();
    assert!(
        proof["homes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["home"] == w.home)
    );
    assert!(
        rebuild(
            &link,
            &f.r,
            f.h.connection(),
            &f.m,
            ALPHA,
            &w.intent,
            &f.cut(&w)
        )
        .unwrap()
    );
    assert!(
        !rebuild(
            &f.dir.path().join("alpha"),
            &f.r,
            f.h.connection(),
            &f.m,
            ALPHA,
            &w.intent,
            &f.cut(&w)
        )
        .unwrap()
    );
}

#[test]
fn busy_observation_stops_before_registry_receipt_and_retries_with_witness() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
    use std::sync::{Arc, Mutex};
    let mut f = Fixture::new(false);
    f.send("retry-busy");
    source::stage_deltas_until(f.h.connection(), None).unwrap();
    // WAL writer locks do not block readers. Use SQLite's rollback journal
    // here to create a real, deterministic busy observation (not a receipt
    // write failure after successful observation).
    f.h.connection()
        .execute_batch("PRAGMA journal_mode=DELETE")
        .unwrap();
    f.h.connection().busy_timeout(Duration::ZERO).unwrap();
    let file: String =
        f.h.connection()
            .query_row(
                "SELECT file FROM pragma_database_list WHERE name='main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
    let blocker = Arc::new(Mutex::new(Connection::open(file).unwrap()));
    let lock = blocker.clone();
    // Take a real SQLite exclusive lock precisely between the pending scan
    // and observation's first read, rather than relying on a timing race.
    f.h.connection()
        .authorizer(Some(move |ctx: AuthContext<'_>| {
            if matches!(
                ctx.action,
                AuthAction::Transaction {
                    operation: TransactionOperation::Begin
                }
            ) {
                lock.lock()
                    .unwrap()
                    .execute_batch("BEGIN EXCLUSIVE")
                    .unwrap();
            }
            Authorization::Allow
        }));
    let path = f.dir.path().join("alpha");
    let outcome = super::super::super::super::home::import_pending(
        &path,
        f.h.connection(),
        &f.r,
        &f.m,
        ALPHA,
        None,
    )
    .unwrap();
    f.h.connection()
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    blocker.lock().unwrap().execute_batch("ROLLBACK").unwrap();
    assert_eq!(outcome.0, 0);
    assert_eq!(count(&f.r, "desk_home_receipt").unwrap(), 0);
    assert_eq!(count(&f.r, "desk_replay_omission").unwrap(), 0);
    f.capture();
    assert_eq!(f.witness().origin.id, "retry-busy");
}

#[test]
fn initial_1a_schema_upgrade_releases_only_unowned_non_candidates() {
    let mut f = Fixture::new(false);
    f.send("retained");
    f.capture();
    let w = f.witness();
    f.r.execute_batch("INSERT INTO desk_replay_import(domain,source,binding,intent,home,mailbox,lane,journal_order,origin,digest,event_digest,native)
        VALUES('legacy','legacy','binding','baseline','home','mailbox','baseline',1,'{}','digest','digest',0);").unwrap();
    for table in [
        "desk_replay_import",
        "desk_replay_witness",
        "desk_replay_omission",
    ] {
        f.r.execute_batch(&format!(
            "DROP TRIGGER {table}_insert; DROP TRIGGER {table}_delete;"
        ))
        .unwrap();
    }
    f.r.execute_batch("DROP TABLE desk_replay_budget; DROP TABLE desk_replay_omission;
        CREATE TABLE desk_replay_omission(domain TEXT,source TEXT,binding TEXT,first_order INTEGER,last_order INTEGER,reason TEXT,enclosing INTEGER CHECK(enclosing=1),PRIMARY KEY(domain,source,binding));
        INSERT INTO desk_replay_omission VALUES('legacy','legacy','binding',2,8,'multiple_omissions',1);").unwrap();
    assert!(!ready(&f.r).unwrap());
    super::super::super::super::init(&f.r).unwrap();
    assert!(ready(&f.r).unwrap());
    assert_eq!(f.witness(), w);
    assert_eq!(count(&f.r, "desk_replay_import").unwrap(), 1);
    let legacy: (i64, i64, String, bool) =
        f.r.query_row(
            "SELECT first_order,last_order,reason,enclosing FROM desk_replay_omission",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(legacy, (2, 8, "multiple_omissions".into(), true));
    for table in [
        "desk_replay_import",
        "desk_replay_witness",
        "desk_replay_omission",
    ] {
        assert_eq!(used(&f.r, table).unwrap(), count(&f.r, table).unwrap());
    }
}

#[test]
fn budget_counters_follow_rollback_adoption_and_restart() {
    let mut f = Fixture::new(false);
    f.send("new");
    f.capture();
    let tables = [
        "desk_replay_import",
        "desk_replay_witness",
        "desk_replay_omission",
    ];
    let check = |r: &Connection| {
        for table in tables {
            assert_eq!(used(r, table).unwrap(), count(r, table).unwrap(), "{table}");
        }
    };
    check(&f.r);
    let tx = f.r.unchecked_transaction().unwrap();
    omitted(&tx, &f.m, ALPHA, 7, "detail_budget").unwrap();
    check(&tx);
    drop(tx);
    check(&f.r);
    let w = f.witness();
    adopt(&f.r, &f.m, &w).unwrap();
    cleanup(&f.r, &f.m, ALPHA).unwrap();
    check(&f.r);
    drop(f.r);
    f.r = Connection::open(f.dir.path().join("registry.sqlite")).unwrap();
    super::super::super::super::init(&f.r).unwrap();
    check(&f.r);
}
