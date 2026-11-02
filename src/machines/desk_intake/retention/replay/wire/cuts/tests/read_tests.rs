use super::*;

#[test]
fn prepared_local_read_validates_once_not_per_item_and_next_read_revalidates() {
    for copy in [false, true] {
        let mut f = Fixture::local();
        let b = binding_value(&f, 1000);
        import(&f, 1000);
        capture_at(&f.desk, &f.m, &b, false, &clock(1000)).unwrap();
        f.send("new");
        let wire = f.stage_latest(copy);
        f.adopt(&wire).unwrap();
        if copy {
            let w: Witness = serde_json::from_value(wire["proof"]["witnesses"][0].clone()).unwrap();
            classify::link(&f.desk, &f.m, &b, &w).unwrap();
        }
        assert!(
            ReadContext::prepare(&f.desk, &f.m, &b).is_err(),
            "requires a read transaction"
        );
        let tx =
            rusqlite::Transaction::new_unchecked(&f.desk, TransactionBehavior::Immediate).unwrap();
        let mut validations = 0;
        let before = tx.total_changes();
        let read = ReadContext::prepare_with(&tx, &f.m, &b, |c, m, s, batch| {
            validations += 1;
            current(c, m, s, batch)
        })
        .unwrap();
        for _ in 0..100 {
            assert_eq!(
                read.classify(&origin(&f, "new")).unwrap(),
                "since_switch_on"
            );
        }
        assert_eq!(validations, 1, "one live vector/home check for the read");
        assert_eq!(tx.total_changes(), before);
        drop(read);
        tx.commit().unwrap();
        // Real home mutation without a new source contact is detected by the
        // NEXT read's one live revalidation; a cached previous read is not reused.
        f.home
            .connection()
            .execute("DELETE FROM mail_source_snapshot", [])
            .unwrap();
        let tx =
            rusqlite::Transaction::new_unchecked(&f.desk, TransactionBehavior::Immediate).unwrap();
        let read = ReadContext::prepare(&tx, &f.m, &b).unwrap();
        assert_eq!(read.classify(&origin(&f, "new")).unwrap(), "unknown");
        drop(read);
        tx.commit().unwrap();
    }
}

#[test]
fn real_proof_changes_advance_revision_but_repeated_proof_and_classification_do_not() {
    let mut f = Fixture::remote();
    crate::door::subscription::init(&f.desk).unwrap();
    fn rev(c: &Connection) -> i64 {
        c.query_row("SELECT revision FROM desk_replay_revision", [], |r| {
            r.get(0)
        })
        .unwrap()
    }
    assert_eq!(rev(&f.desk), 0);
    let b = binding_value(&f, 1000);
    import(&f, 1000);
    let first = rev(&f.desk);
    assert!(first > 0, "v2/vector adoption affects eligibility");
    capture_at(&f.desk, &f.m, &b, false, &clock(1000)).unwrap();
    let cut = rev(&f.desk);
    assert!(cut > first);
    capture_at(&f.desk, &f.m, &b, false, &clock(1000)).unwrap();
    assert_eq!(rev(&f.desk), cut);
    f.send("new");
    let wire = f.import_latest(false);
    let adopted = rev(&f.desk);
    assert!(adopted > cut);
    f.adopt(&wire).unwrap();
    assert_eq!(rev(&f.desk), adopted, "same immutable batch is idempotent");
    assert_eq!(classify_id(&f, &b, "new"), "since_switch_on");
    assert_eq!(
        rev(&f.desk),
        adopted,
        "read cannot establish a proof revision"
    );
}
