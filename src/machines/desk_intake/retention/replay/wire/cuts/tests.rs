use super::super::tests::Fixture;
use super::*;
const ALPHA: &str = "00000000-0000-4000-8000-00000000000a";
const BETA: &str = "00000000-0000-4000-8000-00000000000b";
fn clock(at: i64) -> Clock<'static> {
    Clock {
        session: "alpha",
        wall_ms: at,
        tick_ms: at,
    }
}
fn binding_value(f: &Fixture, at: i64) -> Value {
    json!({"domain":f.m.domain,"owner_core":f.m.owner_core,"generation":f.m.generation,
        "incarnation":f.m.incarnation,"manifest_revision":f.m.manifest_revision,
        "manifest_hash":crate::core::hash(&json!(f.m)),"mode":"fresh_replay","replay_policy":"open_and_new",
        "switch_on_ms":at,"switch_on":[]})
}
fn origin(f: &Fixture, id: &str) -> MailIdentity {
    let mailbox: String = f
        .home
        .connection()
        .query_row("SELECT mailbox FROM mail_source_health", [], |r| r.get(0))
        .unwrap();
    MailIdentity::new(ALPHA, &mailbox, id).unwrap()
}
fn import(f: &Fixture, at: i64) -> Batch {
    let e = f.evidence();
    let wire = f.wire(&e);
    f.adopt(&wire).unwrap();
    f.desk.execute("UPDATE desk_intake_checkpoint SET received_at=?1,tick_ms=?1,session='alpha',clock_valid=1",[at]).unwrap();
    serde_json::from_value(wire["proof"].clone()).unwrap()
}
fn count(c: &Connection, t: &str) -> i64 {
    c.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
        .unwrap()
}
fn classify_id(f: &Fixture, b: &Value, id: &str) -> &'static str {
    classify(&f.desk, &f.m, b, &origin(f, id)).unwrap()
}
fn saved(f: &Fixture, b: &Value) -> String {
    f.desk
        .query_row(
            "SELECT body FROM desk_exposure_cut WHERE binding=?1 AND source=?2",
            params![crate::core::hash(b), ALPHA],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn local_owner_uses_the_same_cut_native_and_copy_first_predicate() {
    for copy_first in [false, true] {
        let mut f = Fixture::local();
        let b = binding_value(&f, 1000);
        import(&f, 1000);
        capture_at(&f.desk, &f.m, &b, false, &clock(1000)).unwrap();
        assert_eq!(count(&f.desk, "desk_exposure_cut"), 1);
        f.send("new");
        let e = f.evidence();
        let wire = f.wire(&e);
        if copy_first {
            f.desk
                .execute("UPDATE desk_intake_original SET first_kind='copy'", [])
                .unwrap();
        }
        f.adopt(&wire).unwrap();
        if copy_first {
            let w: Witness = serde_json::from_value(wire["proof"]["witnesses"][0].clone()).unwrap();
            assert_eq!(classify_id(&f, &b, "new"), "unknown");
            classify::link(&f.desk, &f.m, &b, &w).unwrap();
        }
        assert_eq!(classify_id(&f, &b, "new"), "since_switch_on");
        // Actual home identity changes, with no new contact, must invalidate
        // local proof before a reader could use the old snapshot.
        f.home
            .connection()
            .execute("DELETE FROM mail_source_snapshot", [])
            .unwrap();
        assert_eq!(classify_id(&f, &b, "new"), "unknown");
        assert_eq!(count(&f.desk, "desk_exposure_cut"), 1);
    }
}

#[test]
fn quiet_source_gets_enable_cut_then_first_native_mail_is_new_without_progress_changes() {
    let mut f = Fixture::remote();
    let b = binding_value(&f, 1000);
    let empty = import(&f, 1000);
    assert!(empty.witnesses.is_empty());
    assert_eq!(
        count(&f.desk, "desk_replay_receipt"),
        0,
        "A-compatible empty batch has no receipt churn"
    );
    capture_at(&f.desk, &f.m, &b, false, &clock(1000)).unwrap();
    let boundary = saved(&f, &b);
    f.send("new");
    import(&f, 2000);
    assert_eq!(classify_id(&f, &b, "new"), "since_switch_on");
    let before = f.desk.total_changes();
    for _ in 0..3 {
        assert_eq!(classify_id(&f, &b, "new"), "since_switch_on");
    }
    assert_eq!(
        f.desk.total_changes(),
        before,
        "classification is read-only"
    );
    assert_eq!(count(&f.desk, "desk_subscription_ref"), 0);
    assert_eq!(count(&f.desk, "desk_subscription_receipt"), 0);
    assert_eq!(count(&f.desk, "desk_subscription_progress"), 0);
    capture_at(&f.desk, &f.m, &b, false, &clock(2_000_000)).unwrap();
    assert_eq!(saved(&f, &b), boundary, "aged cut is never refreshed");
}

#[test]
fn stale_enable_rejects_cached_cut_then_reconnect_adopts_once_with_delayed_reason() {
    let mut f = Fixture::remote();
    f.send("before");
    let stale = import(&f, 1000);
    let mut b = binding_value(&f, 1_000_000);
    b["switch_on"] = json!([{"source":ALPHA,"state":"switch_on_pending","through":format!("{}:{}",stale.evidence.checkpoint.id,stale.evidence.checkpoint.upper)}]);
    capture_at(&f.desk, &f.m, &b, false, &clock(1_000_000)).unwrap();
    assert_eq!(count(&f.desk, "desk_exposure_cut"), 0);
    // Replaying the old completed checkpoint cannot refresh its original age.
    adopt(&f.desk, &f.m, &b, ALPHA, &stale, true, &clock(1_000_001)).unwrap();
    assert_eq!(count(&f.desk, "desk_exposure_cut"), 0);
    f.send("interval");
    let fresh = import(&f, 1_000_100);
    adopt(&f.desk, &f.m, &b, ALPHA, &fresh, true, &clock(1_000_150)).unwrap();
    let cut: SavedCut = serde_json::from_str(&saved(&f, &b)).unwrap();
    assert!(cut.delayed);
    assert_eq!(cut.age_at_adoption_ms, 50);
    assert_eq!(cut.received_at, 1_000_100);
    assert_eq!(cut.switch_on_ms, 1_000_000);
    assert_eq!(cut.age(&clock(1_000_200)), Some(100));
    assert_eq!(
        cut.age(&Clock {
            session: "restored",
            wall_ms: 1_000_200,
            tick_ms: 1_000_200
        }),
        None
    );
    for id in ["before", "interval"] {
        assert_eq!(classify_id(&f, &b, id), "delayed_cut");
    }
    f.send("after");
    import(&f, 1_000_300);
    assert_eq!(classify_id(&f, &b, "after"), "since_switch_on");
    let old = saved(&f, &b);
    capture_at(&f.desk, &f.m, &b, true, &clock(1_000_300)).unwrap();
    assert_eq!(saved(&f, &b), old);
}

#[test]
fn copy_first_same_and_remote_source_need_additive_exact_link_and_no_history_veto() {
    for source in [ALPHA, BETA] {
        let mut f = Fixture::remote();
        let b = binding_value(&f, 1000);
        import(&f, 1000);
        capture_at(&f.desk, &f.m, &b, false, &clock(1000)).unwrap();
        f.send("copy-first");
        let e = f.evidence();
        let wire = f.wire(&e);
        let w: Witness = serde_json::from_value(wire["proof"]["witnesses"][0].clone()).unwrap();
        f.desk
            .execute(
                "UPDATE desk_intake_original SET first_kind='copy',source=?1,first_order=77",
                [source],
            )
            .unwrap();
        assert_eq!(
            classify_id(&f, &b, "copy-first"),
            "unknown",
            "body/order alone is not birth proof"
        );
        f.adopt(&wire).unwrap();
        assert_eq!(
            classify_id(&f, &b, "copy-first"),
            "unknown",
            "provisional custody alone is not a link"
        );
        classify::link(&f.desk, &f.m, &b, &w).unwrap();
        assert_eq!(count(&f.desk, "desk_exposure_upgrade"), 1);
        assert_eq!(classify_id(&f, &b, "copy-first"), "since_switch_on");
        let first: (String, i64, String) = f
            .desk
            .query_row(
                "SELECT source,first_order,first_kind FROM desk_intake_original",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(first, (source.into(), 77, "copy".into()));
        // Any supplying-source history beats an otherwise valid native link.
        let mut veto = b.clone();
        veto["switch_on"] = json!([{"source":source,"state":"established","through":format!("{}:77",uuid::Uuid::new_v4())}]);
        assert_eq!(classify_id(&f, &veto, "copy-first"), "history");
        for sql in [
            "UPDATE desk_intake_original SET first_kind=NULL",
            "UPDATE desk_intake_original SET first_kind='native'",
            "UPDATE desk_intake_original SET first_kind='copy',first_order=78",
        ] {
            f.desk.execute_batch(sql).unwrap();
            assert_eq!(classify_id(&f, &b, "copy-first"), "unknown");
        }
    }
}

#[test]
fn positive_requires_current_vector_enrollment_lineage_and_exact_native_tuple() {
    let mut f = Fixture::remote();
    let b = binding_value(&f, 1000);
    import(&f, 1000);
    capture_at(&f.desk, &f.m, &b, false, &clock(1000)).unwrap();
    f.send("new");
    import(&f, 2000);
    let cut = saved(&f, &b);
    assert_eq!(classify_id(&f, &b, "new"), "since_switch_on");
    // Snapshot/proof changes race deterministically inside rolled-back
    // transactions, not sleeps or assumptions about the machine's uptime.
    for sql in [
        "UPDATE desk_intake_proof SET valid=0",
        "UPDATE desk_intake_proof SET body=json_set(body,'$.vector.registry_stamp','changed')",
        "UPDATE desk_replay_vector SET body=json_set(body,'$.homes[0].nonce','changed')",
        "UPDATE desk_replay_vector SET body=json_set(body,'$.homes[0].delta_upper',0)",
        "UPDATE desk_replay_vector SET body=json_set(body,'$.homes[0].mail_upper',0)",
        "UPDATE desk_replay_adopted SET body=json_set(body,'$.nonce','changed')",
        "UPDATE desk_replay_adopted SET body=json_set(body,'$.source','wrong')",
        "UPDATE desk_replay_adopted SET body=json_set(body,'$.digest','wrong')",
        "UPDATE desk_replay_adopted SET body=json_set(body,'$.journal_order',999999)",
        "UPDATE desk_intake_original SET first_order=first_order+1",
        "UPDATE desk_intake_original SET source=NULL",
        "UPDATE desk_intake_original SET first_kind=NULL",
    ] {
        f.desk.execute_batch("SAVEPOINT race").unwrap();
        f.desk.execute_batch(sql).unwrap();
        assert_eq!(classify_id(&f, &b, "new"), "unknown", "{sql}");
        f.desk
            .execute_batch("ROLLBACK TO race; RELEASE race")
            .unwrap();
        assert_eq!(saved(&f, &b), cut);
    }
    // Detectable restore/loss cannot mint new proof or recapture the boundary.
    f.desk
        .execute(
            "INSERT INTO desk_intake_loss VALUES(?1,?2,'history_gap')",
            params![f.m.domain, ALPHA],
        )
        .unwrap();
    assert_eq!(classify_id(&f, &b, "new"), "unknown");
}

#[test]
fn late_home_baseline_and_pre_cut_deltas_are_delayed_not_new() {
    use crate::mail::source;
    let f = Fixture::remote();
    let b = binding_value(&f, 1000);
    import(&f, 1000);
    capture_at(&f.desk, &f.m, &b, false, &clock(1000)).unwrap();
    let old = saved(&f, &b);
    let path = f.dir.path().join("gamma");
    let mut home = crate::store::Store::open(&path).unwrap();
    crate::mail::dispatch(
        &mut home,
        "ship.put",
        json!({"name":"survey","location":"here"}),
    )
    .unwrap();
    source::bind(home.connection(), ALPHA).unwrap();
    source::init_intake(home.connection()).unwrap();
    let send = |h: &mut crate::store::Store, id: &str| {
        crate::mail::dispatch(
            h,
            "mail.send",
            json!({"id":id,"from":"survey","to":"captain","kind":"notice","body":"gamma"}),
        )
        .unwrap();
    };
    send(&mut home, "late-baseline");
    let bound = format!("{}:{ALPHA}", binding(&f.m).unwrap());
    source::enrollment::begin(home.connection(), &bound).unwrap();
    let baseline = source::enrollment::open(&path, true).unwrap();
    source::init(&baseline).unwrap();
    source::init_intake(&baseline).unwrap();
    let deadline = || Instant::now() + std::time::Duration::from_secs(86_400);
    source::enrollment::freeze(home.connection(), &baseline, &bound, deadline()).unwrap();
    source::enrollment::publish(home.connection(), &baseline, &bound).unwrap();
    f.source
        .execute(
            "INSERT INTO ships VALUES('survey',?1,1)",
            [path.to_str().unwrap()],
        )
        .unwrap();
    home::capture_ready_until(&path, home.connection(), &f.source, &f.m, ALPHA, deadline())
        .unwrap();
    send(&mut home, "before-effective-cut");
    home::capture_ready_until(&path, home.connection(), &f.source, &f.m, ALPHA, deadline())
        .unwrap();
    let latest = import(&f, 2000);
    let mailbox: String = home
        .connection()
        .query_row("SELECT mailbox FROM mail_source_health", [], |r| r.get(0))
        .unwrap();
    let o = |id| MailIdentity::new(ALPHA, &mailbox, id).unwrap();
    assert_eq!(
        classify(&f.desk, &f.m, &b, &o("late-baseline")).unwrap(),
        "unknown"
    );
    adopt(&f.desk, &f.m, &b, ALPHA, &latest, true, &clock(2000)).unwrap();
    for id in ["late-baseline", "before-effective-cut"] {
        assert_eq!(classify(&f.desk, &f.m, &b, &o(id)).unwrap(), "delayed_cut");
    }
    let rows = load(&f.desk, &crate::core::hash(&b), ALPHA).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .any(|s| serde_json::to_string(s).unwrap() == old)
    );
    send(&mut home, "after-effective-cut");
    home::capture_ready_until(&path, home.connection(), &f.source, &f.m, ALPHA, deadline())
        .unwrap();
    import(&f, 3000);
    assert_eq!(
        classify(&f.desk, &f.m, &b, &o("after-effective-cut")).unwrap(),
        "since_switch_on"
    );
}

#[test]
fn reenrollment_needs_new_cut_and_keeps_the_previous_boundary() {
    use crate::mail::source;
    let mut f = Fixture::remote();
    let b = binding_value(&f, 1000);
    import(&f, 1000);
    capture_at(&f.desk, &f.m, &b, false, &clock(1000)).unwrap();
    let old = saved(&f, &b);
    f.send("before-reenrollment");
    import(&f, 1500);
    assert_eq!(
        classify_id(&f, &b, "before-reenrollment"),
        "since_switch_on"
    );
    let bound = format!("{}:{ALPHA}", binding(&f.m).unwrap());
    f.home
        .connection()
        .execute("DELETE FROM mail_source_snapshot", [])
        .unwrap();
    source::enrollment::begin(f.home.connection(), &bound).unwrap();
    let path = f.dir.path().join("alpha");
    let baseline = source::enrollment::open(&path, false).unwrap();
    let deadline = || Instant::now() + std::time::Duration::from_secs(86_400);
    source::enrollment::freeze(f.home.connection(), &baseline, &bound, deadline()).unwrap();
    source::enrollment::publish(f.home.connection(), &baseline, &bound).unwrap();
    f.capture();
    f.send("interval");
    let batch = import(&f, 2000);
    assert_eq!(classify_id(&f, &b, "interval"), "unknown");
    adopt(&f.desk, &f.m, &b, ALPHA, &batch, true, &clock(2000)).unwrap();
    assert_eq!(classify_id(&f, &b, "interval"), "delayed_cut");
    assert_eq!(
        classify_id(&f, &b, "before-reenrollment"),
        "unknown",
        "a later nonce's cut cannot reclassify the original enrollment's native tuple as history"
    );
    assert_eq!(
        load(&f.desk, &crate::core::hash(&b), ALPHA).unwrap().len(),
        2
    );
    assert!(
        load(&f.desk, &crate::core::hash(&b), ALPHA)
            .unwrap()
            .iter()
            .any(|c| serde_json::to_string(c).unwrap() == old)
    );
    f.send("after");
    import(&f, 3000);
    assert_eq!(classify_id(&f, &b, "after"), "since_switch_on");
}

#[test]
fn legal_contact_adopts_pending_disabled_binding_but_never_recaptures_it() {
    let mut f = Fixture::remote();
    let e = f.evidence();
    let wire = f.wire(&e);
    let now = transport::process_clock();
    let b = binding_value(&f, now.wall_ms);
    f.desk
        .execute_batch(
            "CREATE TABLE door_desk_exposure(door TEXT PRIMARY KEY,enabled INTEGER,binding TEXT)",
        )
        .unwrap();
    f.desk
        .execute(
            "INSERT INTO door_desk_exposure VALUES('alpha',0,?1)",
            [b.to_string()],
        )
        .unwrap();
    f.desk
        .execute(
            "UPDATE desk_intake_checkpoint SET received_at=?1,tick_ms=?2,session=?3,clock_valid=1",
            params![now.wall_ms, now.tick_ms, now.session],
        )
        .unwrap();
    f.adopt(&wire).unwrap();
    let old = saved(&f, &b);
    assert!(serde_json::from_str::<SavedCut>(&old).unwrap().delayed);
    assert_eq!(count(&f.desk, "desk_replay_receipt"), 0);
    f.send("copy-first");
    let e = f.evidence();
    f.desk
        .execute("UPDATE desk_intake_original SET first_kind='copy'", [])
        .unwrap();
    f.desk
        .execute("UPDATE door_desk_exposure SET enabled=1", [])
        .unwrap();
    f.adopt(&f.wire(&e)).unwrap();
    assert_eq!(saved(&f, &b), old);
    assert_eq!(
        count(&f.desk, "desk_exposure_upgrade"),
        1,
        "legal contact atomically records the copy-first link"
    );
    assert_eq!(classify_id(&f, &b, "copy-first"), "since_switch_on");
}

#[test]
fn cut_failure_rolls_back_only_optional_cut_work_not_adoption_or_receipt() {
    let mut f = Fixture::remote();
    f.send("new");
    let e = f.evidence();
    let wire = f.wire(&e);
    let now = transport::process_clock();
    let b = binding_value(&f, now.wall_ms);
    f.desk.execute_batch("CREATE TABLE door_desk_exposure(door TEXT PRIMARY KEY,binding TEXT);
        CREATE TRIGGER fail_cut AFTER INSERT ON desk_exposure_cut BEGIN SELECT RAISE(ABORT,'cut unavailable'); END;").unwrap();
    f.desk
        .execute(
            "INSERT INTO door_desk_exposure VALUES('alpha',?1)",
            [b.to_string()],
        )
        .unwrap();
    f.desk
        .execute(
            "UPDATE desk_intake_checkpoint SET received_at=?1,tick_ms=?2,session=?3,clock_valid=1",
            params![now.wall_ms, now.tick_ms, now.session],
        )
        .unwrap();
    f.adopt(&wire).unwrap();
    assert_eq!(count(&f.desk, "desk_exposure_cut"), 0);
    assert_eq!(count(&f.desk, "desk_replay_adopted"), 1);
    assert_eq!(count(&f.desk, "desk_replay_vector"), 1);
    let receipt: String = f
        .desk
        .query_row("SELECT body FROM desk_replay_receipt", [], |r| r.get(0))
        .unwrap();
    let receipt: Receipt = serde_json::from_str(&receipt).unwrap();
    assert_eq!(
        receipt.adopted,
        vec![wire["proof"]["witnesses"][0]["intent"].as_str().unwrap()]
    );
    assert!(f.desk.query_row("SELECT EXISTS(SELECT 1 FROM desk_intake_audit WHERE stage='replay.cuts' AND body LIKE '%cut unavailable%')",
        [], |r| r.get::<_,bool>(0)).unwrap());
}

#[test]
fn later_cut_only_contact_retries_link_after_transient_current_failure() {
    let mut f = Fixture::remote();
    let b = binding_value(&f, 1000);
    import(&f, 1000);
    capture_at(&f.desk, &f.m, &b, false, &clock(1000)).unwrap();
    f.desk
        .execute_batch("CREATE TABLE door_desk_exposure(door TEXT PRIMARY KEY,binding TEXT)")
        .unwrap();
    f.desk
        .execute(
            "INSERT INTO door_desk_exposure VALUES('alpha',?1)",
            [b.to_string()],
        )
        .unwrap();
    f.send("copy-first");
    let e = f.evidence();
    let wire = f.wire(&e);
    f.desk
        .execute("UPDATE desk_intake_original SET first_kind='copy'", [])
        .unwrap();
    f.desk
        .execute("UPDATE desk_intake_proof SET valid=0", [])
        .unwrap();
    // Fifty permanently unresolvable older candidates must neither make the
    // first retry unbounded nor starve the real candidate on later contacts.
    for n in 0..50 {
        let mut blocked = wire["proof"]["witnesses"][0].clone();
        let id = format!("unresolved-{n}");
        blocked["origin"]["id"] = json!(id);
        f.desk
            .execute(
                "INSERT INTO desk_replay_adopted VALUES(?1,?2,?3,?4,?5,?2,1,'copy',?6)",
                params![
                    f.m.domain,
                    ALPHA,
                    blocked["mailbox"].as_str().unwrap(),
                    id,
                    binding(&f.m).unwrap(),
                    blocked.to_string()
                ],
            )
            .unwrap();
    }
    f.adopt(&wire).unwrap();
    assert_eq!(count(&f.desk, "desk_replay_adopted"), 51);
    assert_eq!(
        f.desk
            .query_row("SELECT cursor FROM desk_replay_link_retry", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        50
    );
    let plan = f.desk.prepare("EXPLAIN QUERY PLAN SELECT rowid,body FROM desk_replay_adopted INDEXED BY desk_replay_adopted_retry
        WHERE domain=?1 AND binding=?2 AND core=?3 AND first_kind='copy' AND rowid>?4 ORDER BY rowid LIMIT 50").unwrap()
        .query_map(params![f.m.domain,binding(&f.m).unwrap(),ALPHA,50], |r|r.get::<_,String>(3)).unwrap()
        .collect::<rusqlite::Result<Vec<_>>>().unwrap().join(" ");
    assert!(
        plan.contains("desk_replay_adopted_retry")
            && plan.contains("rowid>?")
            && !plan.contains("TEMP"),
        "{plan}"
    );
    assert_eq!(count(&f.desk, "desk_exposure_upgrade"), 0);
    let receipts = outgoing(&f.desk, &f.m, ALPHA, true, None, Instant::now()).unwrap();
    receive(&f.source, &f.m, ALPHA, false, &receipts).unwrap();
    let e = f.evidence();
    let quiet = f.wire(&e);
    assert_eq!(quiet["proof"]["witnesses"], json!([]));
    f.adopt(&quiet).unwrap();
    assert_eq!(count(&f.desk, "desk_exposure_upgrade"), 1);
    assert_eq!(classify_id(&f, &b, "copy-first"), "since_switch_on");
    assert_eq!(count(&f.desk, "desk_subscription_ref"), 0);
    assert_eq!(count(&f.desk, "desk_subscription_progress"), 0);
}

#[test]
fn retry_budget_is_fifty_total_and_fair_across_bindings_and_restarts() {
    let mut f = Fixture::remote();
    f.send("template");
    let mut latest = import(&f, 1000);
    let w = latest.witnesses[0].clone();
    latest.witnesses.clear();
    f.desk
        .execute_batch("CREATE TABLE door_desk_exposure(door TEXT PRIMARY KEY,binding TEXT)")
        .unwrap();
    for (door, at) in [("alpha", 1000), ("beta", 1001)] {
        let b = binding_value(&f, at);
        f.desk
            .execute(
                "INSERT INTO door_desk_exposure VALUES(?1,?2)",
                params![door, b.to_string()],
            )
            .unwrap();
    }
    for n in 0..100 {
        let mut blocked = w.clone();
        blocked.origin.id = format!("unresolved-{n}");
        f.desk
            .execute(
                "INSERT INTO desk_replay_adopted VALUES(?1,?2,?3,?4,?5,?2,1,'copy',?6)",
                params![
                    f.m.domain,
                    ALPHA,
                    blocked.mailbox,
                    blocked.origin.id,
                    binding(&f.m).unwrap(),
                    serde_json::to_string(&blocked).unwrap()
                ],
            )
            .unwrap();
    }
    for expected in [25, 50] {
        let retry = Connection::open(f.dir.path().join("desk.sqlite")).unwrap();
        contact(&retry, &f.m, ALPHA, &latest).unwrap();
        let cursors = retry
            .prepare("SELECT cursor FROM desk_replay_link_retry ORDER BY exposure")
            .unwrap()
            .query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        // The native template is rowid 1, so the copy-first keyset starts at 2.
        assert_eq!(cursors, vec![expected + 1, expected + 1]);
    }
    assert_eq!(count(&f.desk, "desk_exposure_upgrade"), 0);
}

#[test]
fn reads_keep_cut_and_links_across_restart_without_creating_contacts() {
    let mut f = Fixture::remote();
    let b = binding_value(&f, 1000);
    import(&f, 1000);
    capture_at(&f.desk, &f.m, &b, false, &clock(1000)).unwrap();
    f.send("new");
    import(&f, 2000);
    let kept = saved(&f, &b);
    let reopened = Connection::open(f.dir.path().join("desk.sqlite")).unwrap();
    crate::machines::init(&reopened).unwrap();
    let before = reopened.total_changes();
    assert_eq!(
        classify(&reopened, &f.m, &b, &origin(&f, "new")).unwrap(),
        "since_switch_on"
    );
    assert_eq!(reopened.total_changes(), before);
    assert_eq!(saved(&f, &b), kept);
    let diag = diagnostics(&reopened, &f.m, &b).unwrap();
    assert!(
        diag["sources"][0]["cuts"][0]["age_ms"].is_null(),
        "unprovable age must be null, never wall-only"
    );
    assert_eq!(diag["sources"][0]["cuts"][0]["age_at_adoption_ms"], 0);
}
