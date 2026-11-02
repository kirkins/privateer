use super::*;

#[test]
fn scoped_reads_refuse_null_binding_in_place_and_keep_two_doors_independent() {
    let f = Fixture::new();
    f.add("alpha", 1, "alpha", Some("open"));
    let legacy = f.open();
    let binding = json!({"domain":f.m.domain,"owner_core":f.m.owner_core,"generation":1,
        "incarnation":f.m.incarnation,"manifest_revision":1,
        "manifest_hash":crate::core::hash(&serde_json::to_value(&f.m).unwrap()),
        "mode":"fresh_replay","replay_policy":"open_and_new","switch_on_ms":1,"switch_on":[]});
    f.conn
        .execute_batch(
            "CREATE TABLE door_desk_exposure(door TEXT PRIMARY KEY,enabled INTEGER,binding TEXT);",
        )
        .unwrap();
    for door in ["alpha", "beta", "gamma"] {
        f.conn
            .execute(
                "INSERT INTO door_desk_exposure VALUES(?1,1,?2)",
                params![door, binding.to_string()],
            )
            .unwrap();
    }
    let tx = rusqlite::Transaction::new_unchecked(&f.conn, TransactionBehavior::Immediate).unwrap();
    assert_eq!(
        read_admitted(&tx, &f.c, &f.m, &binding, Operation::Open, BYTES),
        Err(ReadRefusal::StaleOrUnknown)
    );
    tx.rollback().unwrap();
    assert_eq!(f.count("desk_subscription"), 1);
    let kept: Option<String> = f
        .conn
        .query_row(
            "SELECT binding FROM desk_subscription_replay WHERE subscription=?1",
            [&legacy],
            |r| r.get(0),
        )
        .unwrap();
    assert!(kept.is_none(), "never recapture a legacy boundary");
    let mut readers = Vec::new();
    for door in ["beta", "gamma"] {
        let c = DelegatedContext::resolved(
            uuid(),
            format!("door:{door}"),
            vec!["mail:read".into()],
            None,
        );
        f.grant(&c);
        let tx =
            rusqlite::Transaction::new_unchecked(&f.conn, TransactionBehavior::Immediate).unwrap();
        let opened = read_admitted(&tx, &c, &f.m, &binding, Operation::Open, BYTES).unwrap();
        let id = opened["subscription"].as_str().unwrap().to_owned();
        let page = read_admitted(
            &tx,
            &c,
            &f.m,
            &binding,
            Operation::Pull {
                subscription: id.clone(),
                continuation: None,
                items: 50,
            },
            BYTES,
        )
        .unwrap();
        assert_eq!(ids(&page), BTreeSet::from(["alpha".into()]));
        tx.commit().unwrap();
        readers.push((id, references(&page)));
    }
    assert_ne!(readers[0], readers[1]);
    assert_eq!(f.count("desk_subscription_progress"), 0);
}
