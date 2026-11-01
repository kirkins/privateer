use super::*;

#[test]
fn baseline_high_water_prevents_more_than_witness_budget_old_ack_candidates() {
    let old = WITNESSES as usize + 51;
    let mut f = Fixture::with_old_rows(old);
    // Finish the frozen baseline in bounded import batches, then import every
    // old row's lifecycle ack in increasing M order. None was a delta before.
    for _ in 0..old.div_ceil(50) {
        f.capture();
    }
    let upper = snapshot::mail_high_water(f.h.connection())
        .unwrap()
        .unwrap();
    assert_eq!(upper, old as i64);
    let marked: i64 =
        f.r.query_row("SELECT mail_order FROM desk_replay_home", [], |r| r.get(0))
            .unwrap();
    assert_eq!(marked, upper);
    for through in (50..old).step_by(50).chain(std::iter::once(old)) {
        crate::mail::dispatch(
            &mut f.h,
            "mail.ack",
            json!({"address":"captain","through_seq":through}),
        )
        .unwrap();
        f.capture();
        assert_eq!(count(&f.r, "desk_replay_import").unwrap(), 0);
        assert_eq!(count(&f.r, "desk_replay_witness").unwrap(), 0);
    }
    assert_eq!(count(&f.r, "desk_replay_omission").unwrap(), 0);
    f.send("fresh-after-old-acks");
    f.capture();
    assert_eq!(f.witness().origin.id, "fresh-after-old-acks");
    assert_eq!(used(&f.r, "desk_replay_import").unwrap(), 1);
}

#[test]
fn every_observed_m_advances_home_mark_even_without_positive_lane() {
    let mut f = Fixture::new(false);
    f.send("template");
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
    let mut o = Observation {
        home: w.home,
        mailbox: w.mailbox,
        nonce: Some(w.nonce),
        lane: "baseline".into(),
        delta_order: Some(100),
        mail_order: Some(100),
        native: false,
    };
    let tx = f.r.unchecked_transaction().unwrap();
    for (n, lane) in ["baseline", "unknown", "delta"].into_iter().enumerate() {
        o.lane = lane.into();
        o.native = lane == "delta";
        o.mail_order = Some(100 + i64::from(lane == "unknown"));
        record(
            &tx,
            &f.m,
            ALPHA,
            &format!("observation-{n}"),
            &event,
            100 + n as i64,
            Some(&o),
        )
        .unwrap();
    }
    assert_eq!(used(&tx, "desk_replay_import").unwrap(), 1);
    assert_eq!(used(&tx, "desk_replay_witness").unwrap(), 1);
    assert_eq!(count(&tx, "desk_replay_omission").unwrap(), 0);
    let marked: i64 = tx
        .query_row("SELECT mail_order FROM desk_replay_home", [], |r| r.get(0))
        .unwrap();
    assert_eq!(marked, 101);
}

#[test]
fn stale_manifest_import_and_cleanup_cannot_release_current_custody() {
    let mut f = Fixture::new(false);
    f.send("current");
    f.capture();
    let w = f.witness();
    let mut stale = f.m.clone();
    stale.generation += 1;
    // Exact registry binding, not the caller's assertion, defines current.
    assert_eq!(cleanup(&f.r, &stale, ALPHA).unwrap(), 0);
    let event: String =
        f.h.connection()
            .query_row(
                "SELECT event FROM mail_source_import WHERE intent=?1",
                [&w.intent],
                |r| r.get(0),
            )
            .unwrap();
    let tx = f.r.unchecked_transaction().unwrap();
    // Exercise record's release path directly, inside the import transaction.
    // Normal append/head admission refuses the stale importer even earlier.
    record(&tx, &stale, ALPHA, "stale-import", &event, 100, None).unwrap();
    assert_eq!(used(&tx, "desk_replay_import").unwrap(), 1);
    assert_eq!(used(&tx, "desk_replay_witness").unwrap(), 1);
    assert_eq!(count(&tx, "desk_replay_omission").unwrap(), 1); // missing snapshot only
    let released: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM desk_replay_omission WHERE reason='superseded_binding'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(released, 0);
    tx.commit().unwrap();
    assert_eq!(f.witness(), w);
}

#[test]
fn upgrade_adds_baseline_identity_without_regressing_marks_or_witnesses() {
    let mut f = Fixture::new(false);
    f.send("retained-on-upgrade");
    f.capture();
    let w = f.witness();
    f.r.execute_batch(
        "ALTER TABLE desk_replay_home DROP COLUMN baseline;
        DELETE FROM desk_replay_budget WHERE name='replay_v5';
        DROP INDEX desk_replay_binding;",
    )
    .unwrap();
    assert!(!ready(&f.r).unwrap());
    super::super::super::super::init(&f.r).unwrap();
    assert!(ready(&f.r).unwrap());
    assert_eq!(f.witness(), w);
    f.capture(); // Seed conservatively once after upgrading the old registry.
    f.send("fresh-after-upgrade");
    f.capture();
    let fresh: i64 = f.r.query_row("SELECT COUNT(*) FROM desk_replay_witness WHERE json_extract(body,'$.origin.id')='fresh-after-upgrade'", [], |r| r.get(0)).unwrap();
    assert_eq!(fresh, 1);
}

#[test]
fn superseded_probe_uses_domain_binding_index() {
    let f = Fixture::new(false);
    let mut q = f.r.prepare("EXPLAIN QUERY PLAN SELECT EXISTS(SELECT 1 FROM desk_replay_import WHERE domain=?1 AND binding<?2) OR EXISTS(SELECT 1 FROM desk_replay_import WHERE domain=?1 AND binding>?2)").unwrap();
    let plan: Vec<String> = q
        .query_map(params![f.m.domain, binding(&f.m).unwrap()], |r| r.get(3))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let indexed = plan
        .iter()
        .filter(|line| line.contains("desk_replay_binding") && line.contains("SEARCH"))
        .count();
    assert_eq!(indexed, 2, "{plan:?}");
}
