use super::*;

#[test]
fn generation_change_keeps_lifecycle_and_rebaseline_intake_moving_without_proof() {
    let mut f = Fixture::new(true);
    // The old baseline journal prefix has already been received/reclaimed.
    // Generation changes still reject any unretired old-generation events.
    f.r.execute("DELETE FROM desk_source_event", []).unwrap();
    f.m.generation += 1;
    f.place();
    let bound = binding(&f.m).unwrap();
    // Model the generation transition's already-authorized registry/home
    // rebinding. There are no per-origin source anchors.
    f.r.execute("UPDATE desk_source_head SET binding=?1", [&bound])
        .unwrap();
    let home_bound = format!("{bound}:{ALPHA}");
    for table in ["mail_source_snapshot", "mail_source_enrollment"] {
        f.h.connection()
            .execute(&format!("UPDATE {table} SET binding=?1"), [&home_bound])
            .unwrap();
    }
    crate::mail::dispatch(
        &mut f.h,
        "mail.ack",
        json!({"address":"captain","through_seq":1}),
    )
    .unwrap();
    source::stage_deltas_until(f.h.connection(), None).unwrap();
    let path = f.dir.path().join("alpha");
    let import = |home: &Connection| {
        super::super::super::super::home::import_pending(&path, home, &f.r, &f.m, ALPHA, None)
            .unwrap()
            .0
    };
    assert_eq!(import(f.h.connection()), 1, "lifecycle intake continues");
    let baseline = source::enrollment::open(&path, false).unwrap();
    baseline
        .execute("UPDATE mail_source_snapshot SET binding=?1", [&home_bound])
        .unwrap();
    baseline
        .execute(
            "UPDATE mail_source_import SET intent=?1,receipt=NULL",
            [uuid::Uuid::new_v4().to_string()],
        )
        .unwrap();
    assert_eq!(import(&baseline), 1, "native re-baseline intake continues");
    assert_eq!(import(&baseline), 0, "the retry receipt committed");
    assert_eq!(count(&f.r, "desk_home_receipt").unwrap(), 3);
    assert_eq!(count(&f.r, "desk_source_event").unwrap(), 2);
    // The original baseline seeded M, and the mark survives the binding
    // transition: even the FIRST old lifecycle observation mints nothing.
    assert_eq!(count(&f.r, "desk_replay_witness").unwrap(), 0);
    assert_eq!(count(&f.r, "desk_replay_import").unwrap(), 0);
    assert_eq!(count(&f.r, "desk_replay_omission").unwrap(), 0);
}

#[test]
fn later_same_m_content_cannot_replace_outstanding_candidate() {
    let mut f = Fixture::new(false);
    f.send("native");
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
    let mut fact: HomeFact = serde_json::from_str(&event).unwrap();
    fact.content["body"] = json!("conflicting immutable content");
    let o = Observation {
        home: w.home.clone(),
        mailbox: w.mailbox.clone(),
        nonce: Some(w.nonce.clone()),
        lane: "delta".into(),
        delta_order: Some(w.delta_order + 1),
        mail_order: Some(w.mail_order),
        native: true,
    };
    let tx = f.r.unchecked_transaction().unwrap();
    record(
        &tx,
        &f.m,
        ALPHA,
        "conflicting-anchor",
        &serde_json::to_string(&fact).unwrap(),
        w.journal_order + 1,
        Some(&o),
    )
    .unwrap();
    drop(tx);
    assert_eq!(f.witness(), w);
    assert_eq!(count(&f.r, "desk_replay_import").unwrap(), 1);
    assert_eq!(count(&f.r, "desk_replay_omission").unwrap(), 0);
}

#[test]
fn adopted_delta_churn_retains_one_home_mark_not_per_origin_history() {
    let mut f = Fixture::new(true);
    f.send("seed");
    f.capture();
    let seed = f.witness();
    let event: String =
        f.h.connection()
            .query_row(
                "SELECT event FROM mail_source_import WHERE intent=?1",
                [&seed.intent],
                |r| r.get(0),
            )
            .unwrap();
    let template: HomeFact = serde_json::from_str(&event).unwrap();
    let mut o = Observation {
        home: seed.home.clone(),
        mailbox: seed.mailbox.clone(),
        nonce: Some(seed.nonce.clone()),
        lane: "delta".into(),
        delta_order: Some(seed.delta_order),
        mail_order: Some(seed.mail_order),
        native: true,
    };
    let tx = f.r.unchecked_transaction().unwrap();
    adopt(&tx, &f.m, &seed).unwrap();
    assert_eq!(cleanup(&tx, &f.m, ALPHA).unwrap(), 1);
    // Equivalent to many reclaimed source pages. Actual record/adopt/cleanup
    // run for over forty thousand origins, with only 50 in flight.
    let total = DETAILS * 4 + 50;
    for n in 1..=total {
        let mut fact = template.clone();
        fact.id = format!("delta-{n}");
        fact.provenance =
            Provenance::Origin(MailIdentity::new(ALPHA, &fact.mailbox, &fact.id).unwrap());
        fact.content["id"] = json!(fact.id);
        o.delta_order = Some(seed.delta_order + n);
        o.mail_order = Some(seed.mail_order + n);
        let intent = format!("delta-intent-{n}");
        record(
            &tx,
            &f.m,
            ALPHA,
            &intent,
            &serde_json::to_string(&fact).unwrap(),
            seed.journal_order + n,
            Some(&o),
        )
        .unwrap();
        let w = candidate(&tx, &f.m, ALPHA, &intent)
            .unwrap()
            .expect("new delta remains provable");
        adopt(&tx, &f.m, &w).unwrap();
        if n % 50 == 0 {
            assert_eq!(cleanup(&tx, &f.m, ALPHA).unwrap(), 50);
            assert_eq!(
                count(&tx, "desk_replay_home").unwrap(),
                1,
                "one mark, independent of adopted traffic"
            );
            assert_eq!(used(&tx, "desk_replay_import").unwrap(), 0);
            assert_eq!(used(&tx, "desk_replay_witness").unwrap(), 0);
        }
    }
    tx.execute(
        "UPDATE desk_source_head SET upper_seq=?1",
        [seed.journal_order + total],
    )
    .unwrap();
    tx.commit().unwrap();
    f.send("after-churn");
    f.capture();
    // These synthetic imports have advanced M beyond the real home. Lower-M
    // first content is withheld rather than guessed to be new.
    assert_eq!(count(&f.r, "desk_replay_witness").unwrap(), 0);
    assert_eq!(count(&f.r, "desk_replay_omission").unwrap(), 0);
}

#[test]
fn lifecycle_after_cleanup_cannot_remint_against_a_real_pre_content_cut() {
    let mut f = Fixture::new(false);
    // Take the actual pre-content high water: this cut is genuinely before
    // this row, not an artificial M upper that would hide a re-mint bug.
    let mail_upper = snapshot::mail_high_water(f.h.connection())
        .unwrap()
        .unwrap();
    let delta_upper: i64 =
        f.h.connection()
            .query_row("SELECT upper_seq FROM mail_source_snapshot", [], |r| {
                r.get(0)
            })
            .unwrap();
    f.send("recreated");
    f.capture();
    let old = f.witness();
    let mut cut = f.cut(&old);
    cut.mail_upper = mail_upper;
    cut.delta_upper = delta_upper;
    assert!(old.beyond(&cut));
    adopt(&f.r, &f.m, &old).unwrap();
    cleanup(&f.r, &f.m, ALPHA).unwrap();
    assert_eq!(count(&f.r, "desk_replay_home").unwrap(), 1);
    // M remains beyond that cut, so the persistent home mark must veto the
    // lifecycle observation even though the detail and witness were released.
    crate::mail::dispatch(
        &mut f.h,
        "mail.ack",
        json!({"address":"captain","through_seq":old.mail_order}),
    )
    .unwrap();
    f.capture();
    assert_eq!(count(&f.r, "desk_replay_witness").unwrap(), 0);
    // A subsequent import of the same lifecycle row still cannot re-mint.
    crate::mail::dispatch(
        &mut f.h,
        "mail.ack",
        json!({"address":"captain","through_seq":old.mail_order}),
    )
    .unwrap();
    f.capture();
    assert_eq!(count(&f.r, "desk_replay_witness").unwrap(), 0);
}

#[test]
fn omission_runs_bridge_non_native_and_lifecycle_orders_but_not_details() {
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
    let fact: HomeFact = serde_json::from_str(&event).unwrap();
    let o = Observation {
        home: w.home,
        mailbox: w.mailbox,
        nonce: Some(w.nonce),
        lane: "delta".into(),
        delta_order: Some(w.delta_order),
        mail_order: Some(w.mail_order),
        native: true,
    };
    let mut copy = fact.clone();
    copy.provenance = Provenance::Copy(w.origin);
    let copy_event = serde_json::to_string(&copy).unwrap();
    for n in 1..=storage::range_budget() * 3 {
        let start = n * 3;
        record(
            &f.r,
            &f.m,
            ALPHA,
            &format!("copy-{n}"),
            &copy_event,
            start,
            None,
        )
        .unwrap();
        record(
            &f.r,
            &f.m,
            ALPHA,
            &format!("lifecycle-{n}"),
            &event,
            start + 1,
            Some(&o),
        )
        .unwrap();
        omitted(&f.r, &f.m, ALPHA, start + 2, "detail_budget").unwrap();
    }
    let rows: Vec<(i64,i64,bool)> = f.r.prepare(
        "SELECT first_order,last_order,enclosing FROM desk_replay_omission WHERE reason='detail_budget'",
    ).unwrap().query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    assert_eq!(rows, vec![(5, storage::range_budget() * 9 + 2, false)]);
    assert_eq!(
        count(&f.r, "desk_replay_omission").unwrap(),
        1,
        "non-native with no snapshot is not an omission"
    );
}

#[test]
fn upgrade_recomputes_stale_counters_before_releasing_non_candidates() {
    let mut f = Fixture::new(false);
    f.send("retained");
    f.capture();
    let w = f.witness();
    // Existing v2 counters can be stale, including a zero whose old delete
    // trigger would underflow during migration's non-candidate cleanup.
    f.r.execute_batch("INSERT INTO desk_replay_import(domain,source,binding,intent,home,mailbox,lane,journal_order,origin,digest,event_digest,native)
        VALUES('legacy','legacy','binding','baseline','home','mailbox','baseline',1,'{}','digest','digest',0);
        UPDATE desk_replay_budget SET used=0;
        DELETE FROM desk_replay_budget WHERE name='replay_v5';").unwrap();
    assert!(!ready(&f.r).unwrap());
    super::super::super::super::init(&f.r).unwrap();
    assert!(ready(&f.r).unwrap());
    assert_eq!(f.witness(), w);
    for table in [
        "desk_replay_import",
        "desk_replay_witness",
        "desk_replay_omission",
    ] {
        assert_eq!(used(&f.r, table).unwrap(), count(&f.r, table).unwrap());
    }
    assert_eq!(used(&f.r, "desk_replay_import").unwrap(), 1);
}
