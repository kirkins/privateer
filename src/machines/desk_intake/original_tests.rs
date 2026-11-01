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
    let tuple: (String, i64) = desk
        .query_row(
            "SELECT source,first_order FROM desk_intake_original",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(tuple, (ALPHA.into(), 1));
    // Re-appending identical content at a later source order (as with a
    // pruned/re-created native ID and new M) cannot replace the direct-proof
    // tuple. Nor can changed content supply a matching original digest.
    capture(&source, &m, &request);
    let cp = checkpoint(&source, &m, ALPHA, &id()).unwrap();
    let recreated = page(&source, &m, &cp, 3, ITEMS, BYTES).unwrap();
    accept(&desk, &m, &recreated, &clock(400)).unwrap();
    let saved: (String, i64) = desk
        .query_row(
            "SELECT source,first_order FROM desk_intake_original",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(saved, (ALPHA.into(), 1));
    assert_ne!(saved.1, recreated.events[0].order);
    let mut conflict = recreated.clone();
    conflict.events[0].fact.content["body"] = json!("replacement");
    assert!(accept(&desk, &m, &conflict, &clock(500)).is_err());
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
fn populated_4a_registry_upgrade_preserves_unknown_sources_and_rollback_writes() {
    let source = db();
    let desk = db();
    let m = manifest();
    let request = fact(1);
    capture(&source, &m, &request);
    let first = one_page(&source, &m);
    accept(&desk, &m, &first, &clock(100)).unwrap();
    // Recreate the populated 4a registry shape. No door DDL is involved.
    desk.execute_batch(
        "ALTER TABLE desk_intake_original DROP COLUMN source;
        DROP TABLE desk_replay_import; DROP TABLE desk_replay_home;
        DROP TABLE desk_replay_witness; DROP TABLE desk_replay_omission;",
    )
    .unwrap();
    init(&desk).unwrap();
    let saved: (i64, Option<String>) = desk
        .query_row(
            "SELECT first_order,source FROM desk_intake_original",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(saved, (1, None));
    assert_eq!(original(&desk, &m, &request), Some(1));
    assert_eq!(count(&desk, "desk_intake_checkpoint"), 1);
    // Old binaries specify the old columns and can still write a new row.
    desk.execute("INSERT INTO desk_intake_original(domain,owner_core,generation,incarnation,core,mailbox,id,first_order)
        SELECT domain,owner_core,generation,incarnation,core,mailbox,'legacy-rollback',2 FROM desk_intake_original LIMIT 1",[]).unwrap();
    init(&desk).unwrap();
    let missing: i64 = desk
        .query_row(
            "SELECT COUNT(*) FROM desk_intake_original WHERE source IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(missing, 2);
    // Missing COLUMN alone must trigger migration even when every table exists.
    desk.execute_batch("ALTER TABLE desk_intake_original DROP COLUMN source")
        .unwrap();
    init(&desk).unwrap();
    desk.pragma_update(None, "query_only", true).unwrap();
    init(&desk).unwrap(); // Current initialization is read-only/idempotent.
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
