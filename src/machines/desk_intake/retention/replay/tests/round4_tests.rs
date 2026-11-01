use super::*;

#[test]
fn superseded_full_custody_releases_in_bounded_batches_and_new_delta_is_admitted() {
    let mut f = Fixture::new(false);
    f.send("native-a");
    f.capture();
    let old = f.witness();
    // Fill authentic-shaped custody under A, including witnessless rebuild
    // details. No fake global counter: actual rows consume the budgets.
    f.r.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<?1)
        INSERT INTO desk_replay_import(domain,source,binding,intent,home,mailbox,nonce,lane,journal_order,origin,digest,event_digest,delta_order,mail_order,native)
        SELECT domain,source,binding,'waiting-'||x,home,mailbox,nonce,lane,journal_order+x,origin,digest,event_digest,delta_order,mail_order,native
        FROM desk_replay_import CROSS JOIN n WHERE intent=?2", params![DETAILS-1,old.intent]).unwrap();
    f.r.execute("INSERT INTO desk_replay_witness(domain,source,intent,body)
        SELECT i.domain,i.source,i.intent,json_set(w.body,'$.intent',i.intent,'$.journal_order',i.journal_order)
        FROM desk_replay_import i JOIN desk_replay_witness w USING(domain,source)
        WHERE i.intent!=w.intent ORDER BY i.journal_order LIMIT ?1", [WITNESSES-1]).unwrap();
    // A removed source must not pin shared budgets either: it may never
    // contact again. Include one such candidate in the superseded binding.
    f.r.execute(
        "UPDATE desk_replay_import SET source=?1 WHERE intent='waiting-1'",
        [BETA],
    )
    .unwrap();
    f.r.execute(
        "UPDATE desk_replay_witness SET source=?1 WHERE intent='waiting-1'",
        [BETA],
    )
    .unwrap();
    f.m.sources.retain(|s| s.core != BETA);
    f.m.owner_core = ALPHA.into();
    f.m.sources[0].intake_direction = "local".into();
    assert_eq!(used(&f.r, "desk_replay_import").unwrap(), DETAILS);
    assert_eq!(used(&f.r, "desk_replay_witness").unwrap(), WITNESSES);
    f.r.execute("DELETE FROM desk_source_event", []).unwrap();
    f.m.generation += 1;
    f.place();
    let bound = binding(&f.m).unwrap();
    f.r.execute("UPDATE desk_source_head SET binding=?1", [&bound])
        .unwrap();
    let home_bound = format!("{bound}:{ALPHA}");
    for table in ["mail_source_snapshot", "mail_source_enrollment"] {
        f.h.connection()
            .execute(&format!("UPDATE {table} SET binding=?1"), [&home_bound])
            .unwrap();
    }
    assert!(
        adopt(&f.r, &f.m, &old)
            .unwrap_err()
            .to_string()
            .contains("stale_generation")
    );
    assert!(candidate(&f.r, &f.m, ALPHA, &old.intent).unwrap().is_none());
    let path = f.dir.path().join("alpha");
    // Direct imports bypass capture's seed step. Seed the authorized binding
    // before the fresh delta, not after its insertion M is in the high-water.
    seed_baseline(&path, f.h.connection(), &f.r, &f.m, ALPHA).unwrap();
    f.send("native-b");
    source::stage_deltas_until(f.h.connection(), None).unwrap();
    // Test the atomic import boundary after authorized rebinding, rather than
    // capture_ready's unrelated baseline scheduler (still on the old cut).
    super::super::super::super::home::import_pending(
        &path,
        f.h.connection(),
        &f.r,
        &f.m,
        ALPHA,
        None,
    )
    .unwrap();
    let current: String = f.r.query_row(
        "SELECT body FROM desk_replay_witness WHERE json_extract(body,'$.origin.id')='native-b'", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(
        serde_json::from_str::<Witness>(&current).unwrap().binding,
        bound
    );
    assert_eq!(used(&f.r, "desk_replay_import").unwrap(), DETAILS - 50 + 1);
    let mut released = 50;
    loop {
        let n = cleanup(&f.r, &f.m, ALPHA).unwrap();
        assert!(n <= 50);
        if n == 0 {
            break;
        }
        released += n as i64;
    }
    assert_eq!(released, DETAILS);
    assert_eq!(used(&f.r, "desk_replay_import").unwrap(), 1);
    assert_eq!(used(&f.r, "desk_replay_witness").unwrap(), 1);
    let range: (String, i64, i64) = f.r.query_row(
        "SELECT binding,first_order,last_order FROM desk_replay_omission WHERE reason='superseded_binding' AND source=?1", [ALPHA],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    ).unwrap();
    assert_eq!(
        range,
        (
            old.binding,
            old.journal_order,
            old.journal_order + DETAILS - 1
        )
    );
    let removed: (i64, i64) = f.r.query_row(
        "SELECT first_order,last_order FROM desk_replay_omission WHERE reason='superseded_binding' AND source=?1", [BETA],
        |r| Ok((r.get(0)?,r.get(1)?)),
    ).unwrap();
    assert_eq!(removed, (old.journal_order + 1, old.journal_order + 1));
    // Old mail cannot re-mint under B after custody release.
    crate::mail::dispatch(
        &mut f.h,
        "mail.ack",
        json!({"address":"captain","through_seq":old.mail_order}),
    )
    .unwrap();
    source::stage_deltas_until(f.h.connection(), None).unwrap();
    super::super::super::super::home::import_pending(
        &path,
        f.h.connection(),
        &f.r,
        &f.m,
        ALPHA,
        None,
    )
    .unwrap();
    assert_eq!(used(&f.r, "desk_replay_witness").unwrap(), 1);
    assert_eq!(count(&f.r, "desk_replay_home").unwrap(), 1);
}

#[test]
fn repeated_rebaselines_after_cleanup_do_not_retain_origin_history() {
    let mut f = Fixture::new(false);
    f.send("seed");
    f.capture();
    let w = f.witness();
    let event: String =
        f.h.connection()
            .query_row(
                "SELECT event FROM mail_source_import WHERE intent=?1",
                [&w.intent],
                |r| r.get(0),
            )
            .unwrap();
    adopt(&f.r, &f.m, &w).unwrap();
    cleanup(&f.r, &f.m, ALPHA).unwrap();
    let o = Observation {
        home: w.home,
        mailbox: w.mailbox,
        nonce: None,
        lane: "baseline".into(),
        delta_order: None,
        mail_order: None,
        native: false,
    };
    let mut fact: HomeFact = serde_json::from_str(&event).unwrap();
    let tx = f.r.unchecked_transaction().unwrap();
    for n in 1..=DETAILS * 3 {
        fact.id = format!("released-{n}");
        fact.provenance =
            Provenance::Origin(MailIdentity::new(ALPHA, &fact.mailbox, &fact.id).unwrap());
        fact.content["id"] = json!(fact.id);
        // Repeated baselines and unknown-lane imports of arbitrarily many
        // released origins must allocate no per-origin proof storage.
        for lane in ["baseline", "unknown"] {
            let o = Observation {
                lane: lane.into(),
                ..o.clone()
            };
            record(
                &tx,
                &f.m,
                ALPHA,
                &format!("{lane}-{n}"),
                &serde_json::to_string(&fact).unwrap(),
                n,
                Some(&o),
            )
            .unwrap();
        }
    }
    assert_eq!(count(&tx, "desk_replay_home").unwrap(), 1);
    for table in [
        "desk_replay_import",
        "desk_replay_witness",
        "desk_replay_omission",
    ] {
        assert_eq!(count(&tx, table).unwrap(), 0);
    }
    assert!(crate::mail::source::missing(&tx, &["desk_replay_anchor"]).unwrap());
}

#[test]
fn upgrade_folds_legacy_anchors_into_one_home_mark_and_preserves_custody() {
    let mut f = Fixture::new(false);
    f.send("retained");
    f.capture();
    let w = f.witness();
    f.r.execute_batch("DELETE FROM desk_replay_home;
        DELETE FROM desk_replay_budget WHERE name='replay_v5';
        CREATE TABLE desk_replay_anchor(domain TEXT,source TEXT,binding TEXT,origin TEXT,digest TEXT,first_order INTEGER,home TEXT,nonce TEXT,lane TEXT,mail_order INTEGER);").unwrap();
    for n in 1..=100 {
        f.r.execute(
            "INSERT INTO desk_replay_anchor VALUES(?1,?2,?3,?4,'digest',?5,?6,?7,'delta',?5)",
            params![
                f.m.domain,
                ALPHA,
                w.binding,
                format!("origin-{n}"),
                n,
                w.home,
                w.nonce
            ],
        )
        .unwrap();
    }
    assert!(!ready(&f.r).unwrap());
    super::super::super::super::init(&f.r).unwrap();
    assert!(ready(&f.r).unwrap());
    assert_eq!(f.witness(), w);
    assert_eq!(count(&f.r, "desk_replay_home").unwrap(), 1);
    let hw: i64 =
        f.r.query_row("SELECT mail_order FROM desk_replay_home", [], |r| r.get(0))
            .unwrap();
    assert_eq!(hw, 100);
    assert!(crate::mail::source::missing(&f.r, &["desk_replay_anchor"]).unwrap());
    assert_eq!(used(&f.r, "desk_replay_import").unwrap(), 1);
    // Restart/reinitialization cannot regress the migrated high-water.
    super::super::super::super::init(&f.r).unwrap();
    let hw: i64 =
        f.r.query_row("SELECT mail_order FROM desk_replay_home", [], |r| r.get(0))
            .unwrap();
    assert_eq!(hw, 100);
}

#[test]
fn concurrent_upgrade_rechecks_readiness_after_acquiring_writer_lock() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
    use std::sync::{Arc, Barrier};

    let dir = tempfile::tempdir().unwrap();
    let version = |c: &Connection| {
        c.pragma_query_value::<i64, _>(None, "schema_version", |r| r.get(0))
            .unwrap()
    };
    let prepare = |name: &str| {
        let path = dir.path().join(name);
        let c = Connection::open(&path).unwrap();
        super::super::super::super::init(&c).unwrap();
        c.execute("DELETE FROM desk_replay_budget WHERE name='replay_v5'", [])
            .unwrap();
        (path, c)
    };
    let (_, serial) = prepare("serial.sqlite");
    let before = version(&serial);
    super::super::super::super::init(&serial).unwrap();
    let changes = version(&serial) - before;
    assert!(changes > 0);

    let (path, concurrent) = prepare("concurrent.sqlite");
    let before = version(&concurrent);
    let barrier = Arc::new(Barrier::new(2));
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let barrier = barrier.clone();
                let path = &path;
                scope.spawn(move || {
                    let c = Connection::open(path).unwrap();
                    c.busy_timeout(Duration::from_secs(10)).unwrap();
                    // Both outer readiness checks see the old marker before
                    // either BEGIN executes. No sleep or scheduler race required.
                    c.authorizer(Some(move |ctx: AuthContext<'_>| {
                        if matches!(
                            ctx.action,
                            AuthAction::Transaction {
                                operation: TransactionOperation::Begin
                            }
                        ) {
                            barrier.wait();
                        }
                        Authorization::Allow
                    }));
                    super::super::super::super::init(&c).unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
    });
    assert_eq!(version(&concurrent), before + changes);
    assert!(ready(&concurrent).unwrap());
    super::super::super::super::init(&concurrent).unwrap();
    assert_eq!(version(&concurrent), before + changes);
}
