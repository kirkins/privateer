use super::*;

#[test]
fn slow_proof_sweeps_resume_without_rolling_back_pages() {
    let dir = tempfile::tempdir().unwrap();
    let source = Connection::open(dir.path().join("registry.sqlite")).unwrap();
    crate::machines::init(&source).unwrap();
    source
        .execute_batch("CREATE TABLE ships(name TEXT,home TEXT,retired_ms INTEGER)")
        .unwrap();
    let mut c = contact_for_test(false);
    c.direction = "collect".into();
    c.manifest.sources[0].ships = vec!["alpha".into()];
    let deadline = Instant::now() + Duration::from_secs(3600);
    let mut homes = Vec::new();
    for (index, name) in ["alpha", "beta", "gamma"].into_iter().enumerate() {
        let path = dir.path().join(name);
        let mut h = crate::store::Store::open(&path).unwrap();
        crate::mail::source::bind(h.connection(), SOURCE).unwrap();
        crate::mail::dispatch(&mut h, "ship.put", json!({"name":name,"location":"here"})).unwrap();
        source
            .execute(
                "INSERT INTO ships VALUES(?1,?2,?3)",
                params![name, path.to_str().unwrap(), (index > 0).then_some(1)],
            )
            .unwrap();
        for _ in 0..4 {
            home::capture(h.connection(), &source, &c.manifest, SOURCE).unwrap();
        }
        homes.push(h);
    }
    obstacle(&source, &c.manifest, SOURCE, "history_gap").unwrap();
    reconcile::save(&source, &c.manifest, SOURCE, deadline).unwrap();
    assert!(
        reconcile::load(&source, &c.manifest, SOURCE)
            .unwrap()
            .unwrap()
            .candidate
    );
    // Inject a one-home optional proof budget, not a sleep or runner-speed
    // assumption. Drop resets it even if the assertion unwinds.
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            reconcile::HOME_BUDGET.with(|n| n.set(None));
        }
    }
    let _reset = Reset;
    reconcile::HOME_BUDGET.with(|n| n.set(Some(1)));
    let f = source_frame_at(&source, &c, true, deadline, WIRE_BYTES, 1000).unwrap();
    let p = f.page.clone().expect("slow proof must not prevent a page");
    assert!(f.proof.is_none());
    assert_eq!(count(&source, "desk_source_issuance"), 1);
    assert_eq!(
        source
            .query_row("SELECT next_home FROM desk_source_proof_work", [], |r| {
                r.get::<_, usize>(0)
            })
            .unwrap(),
        1
    );
    let desk = db();
    let owner = Contact {
        manifest: c.manifest.clone(),
        ..contact_for_test(true)
    };
    receive(&desk, &owner, f).unwrap();
    receive(&source, &c, owner_frame(&desk, &owner).unwrap()).unwrap();
    let old_id = p.checkpoint.id.clone();
    // Each completed contact resumes at the next home, even across a registry
    // reopen (as in the real gateway process boundary).
    for next in [2, 0] {
        let reopened = Connection::open(dir.path().join("registry.sqlite")).unwrap();
        let frame = source_frame_at(&reopened, &c, true, deadline, WIRE_BYTES, 1000).unwrap();
        assert!(
            frame.page.is_none(),
            "proof work must not manufacture new pages"
        );
        if next > 0 {
            assert_eq!(
                reopened
                    .query_row("SELECT next_home FROM desk_source_proof_work", [], |r| {
                        r.get::<_, usize>(0)
                    })
                    .unwrap(),
                next
            );
        }
    }
    assert_eq!(
        count(&source, "desk_source_proof"),
        1,
        "all homes eventually checked"
    );
    assert_eq!(count(&source, "desk_source_proof_work"), 0);
    assert_eq!(
        source
            .query_row("SELECT barrier FROM desk_source_proof", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        old_id
    );
    // Accumulating preparation never bypasses the full per-advertisement
    // freshness check: the one-home limit still withholds valid evidence.
    assert!(
        !proof::outgoing(&source, &c.manifest, &p.checkpoint, deadline)
            .unwrap()
            .unwrap()
            .is_valid()
    );
    reconcile::HOME_BUDGET.with(|n| n.set(None));
    assert!(
        proof::outgoing(&source, &c.manifest, &p.checkpoint, deadline)
            .unwrap()
            .unwrap()
            .is_valid()
    );
    let before = source.total_changes();
    source_frame_at(&source, &c, true, deadline, WIRE_BYTES, 1000).unwrap();
    assert_eq!(source.total_changes(), before, "settled proof is quiet");
}

#[test]
fn optional_proof_errors_preserve_page_receipts_and_capability_bookkeeping() {
    let source = db();
    let desk = db();
    let c = contact_for_test(false);
    let owner = Contact {
        manifest: c.manifest.clone(),
        ..contact_for_test(true)
    };
    let deadline = Instant::now() + Duration::from_secs(3600);
    let f = source_frame(&source, &c, true, deadline).unwrap();
    receive(&desk, &owner, f).unwrap();
    desk.execute(
        "INSERT OR REPLACE INTO desk_intake_proof VALUES(?1,?2,'not a supported stored body',1)",
        params![c.manifest.domain, SOURCE],
    )
    .unwrap();
    let work = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let charged = work.clone();
    desk.progress_handler(
        1,
        Some(move || {
            charged.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            false
        }),
    );
    let mut f = source_frame(&source, &c, true, deadline).unwrap();
    f.proof=Some(serde_json::from_value(json!({"version":1,"checkpoint":f.page.as_ref().unwrap().checkpoint,
        "vector":{"candidate":false,"registry_stamp":null,"copies":[],"binding":"alpha","source":SOURCE,"source_upper":0,"inventory":"alpha","membership_history_gap":true,"membership_proved":false,"history_proved":false,"homes":[]},"valid":true})).unwrap());
    receive(&desk, &owner, f).unwrap();
    assert!(
        !desk
            .query_row("SELECT valid FROM desk_intake_proof", [], |r| r
                .get::<_, bool>(0))
            .unwrap(),
        "unparseable stored sidecars are invalidated without losing the page"
    );
    // Any optional write can fail independently of ordinary accepted pages.
    desk.execute_batch("UPDATE desk_intake_proof SET valid=1; CREATE TRIGGER refuse_proof BEFORE UPDATE ON desk_intake_proof BEGIN SELECT RAISE(FAIL,'optional proof busy'); END").unwrap();
    receive(
        &desk,
        &owner,
        source_frame(&source, &c, true, deadline).unwrap(),
    )
    .unwrap();
    work.store(0, std::sync::atomic::Ordering::Relaxed);
    desk.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))
        .unwrap();
    assert!(
        work.load(std::sync::atomic::Ordering::Relaxed) > 0,
        "optional diagnostics must not clear the enclosing contact VM budget"
    );
    assert!(owner_frame(&desk, &owner).unwrap().ack.is_some());
    assert_eq!(
        desk.query_row("SELECT supported FROM desk_intake_transport", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    negotiation_gap(&desk, &owner, &Value::Null).unwrap();
    assert_eq!(
        desk.query_row("SELECT supported FROM desk_intake_transport", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert!(
        desk.query_row(
            "SELECT EXISTS(SELECT 1 FROM desk_intake_audit WHERE stage='proof.receive')",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap()
    );
    receive(&source, &c, owner_frame(&desk, &owner).unwrap()).unwrap();
    assert!(
        source
            .query_row("SELECT receipted FROM desk_intake_transport", [], |r| r
                .get::<_, bool>(0))
            .unwrap()
    );
}
