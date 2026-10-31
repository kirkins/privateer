use super::*;

fn original(conn: &Connection, m: &Manifest, f: &Fact) -> Option<i64> {
    subscription::original_order(conn, m, &f.origin).unwrap()
}

#[test]
fn original_content_order_survives_lifecycle_ack_and_real_restart() {
    let source = db();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("beta.sqlite");
    let m = manifest();
    let mut request = fact(1);
    request.state = Some("open".into());
    capture(&source, &m, &request);
    let first = one_page(&source, &m);
    {
        let desk = Connection::open(&path).unwrap();
        init(&desk).unwrap();
        accept(&desk, &m, &first, &clock(100)).unwrap();
        assert_eq!(original(&desk, &m, &request), Some(1));
    }
    let desk = Connection::open(&path).unwrap();
    init(&desk).unwrap();
    request.state = Some("closed".into());
    request.pending = false;
    capture(&source, &m, &request);
    request.unread = false;
    capture(&source, &m, &request);
    let cp = checkpoint(&source, &m, ALPHA, &id()).unwrap();
    let delta = page(&source, &m, &cp, 1, ITEMS, BYTES).unwrap();
    accept(&desk, &m, &delta, &clock(200)).unwrap();
    assert_eq!(original(&desk, &m, &request), Some(1));
    let observation = subscription::observation(&desk, &m, &request.origin)
        .unwrap()
        .unwrap();
    assert_eq!(observation.order, 3);
    assert_eq!(observation.state.as_deref(), Some("closed"));
    assert_eq!(
        accept(&desk, &m, &delta, &clock(300)).unwrap()["replayed"],
        true
    );
    assert_eq!(original(&desk, &m, &request), Some(1));
    assert_eq!(count(&desk, "desk_intake_original"), 1);
    let mut stale = m.clone();
    stale.incarnation = id();
    assert_eq!(original(&desk, &stale, &request), None);
}

#[test]
fn legacy_original_is_unknown_even_after_update_and_upgrade_is_noop_after_first_init() {
    let source = db();
    let desk = db();
    let m = manifest();
    let mut request = fact(1);
    capture(&source, &m, &request);
    let first = one_page(&source, &m);
    accept(&desk, &m, &first, &clock(100)).unwrap();
    desk.execute_batch("DROP TABLE desk_intake_original")
        .unwrap();
    init(&desk).unwrap();
    assert_eq!(original(&desk, &m, &request), None);
    let schema: i64 = desk
        .pragma_query_value(None, "schema_version", |r| r.get(0))
        .unwrap();
    let changes = desk.total_changes();
    init(&desk).unwrap();
    assert_eq!(changes, desk.total_changes());
    assert_eq!(
        schema,
        desk.pragma_query_value::<i64, _>(None, "schema_version", |r| r.get(0))
            .unwrap()
    );
    request.unread = false;
    capture(&source, &m, &request);
    let newer = fact(2);
    capture(&source, &m, &newer);
    let cp = checkpoint(&source, &m, ALPHA, &id()).unwrap();
    let delta = page(&source, &m, &cp, 1, ITEMS, BYTES).unwrap();
    accept(&desk, &m, &delta, &clock(200)).unwrap();
    assert_eq!(original(&desk, &m, &request), None);
    assert_eq!(original(&desk, &m, &newer), Some(3));
    desk.pragma_update(None, "query_only", true).unwrap();
    assert_eq!(original(&desk, &m, &newer), Some(3));
}

#[test]
fn original_evidence_rolls_back_with_an_invalid_page() {
    let source = db();
    let desk = db();
    let m = manifest();
    let request = fact(1);
    capture(&source, &m, &request);
    capture(&source, &m, &fact(2));
    let mut p = one_page(&source, &m);
    p.events[1].fact.origin.core = BETA.into();
    assert!(accept(&desk, &m, &p, &clock(100)).is_err());
    assert_eq!(count(&desk, "desk_intake_fact"), 0);
    assert_eq!(count(&desk, "desk_intake_original"), 0);
    assert_eq!(count(&desk, "desk_intake_checkpoint"), 0);
    p.events[1].fact.origin.core = ALPHA.into();
    accept(&desk, &m, &p, &clock(100)).unwrap();
    assert_eq!(original(&desk, &m, &request), Some(1));
}
