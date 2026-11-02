use super::*;

#[test]
fn range_revisit_uses_mailbox_order_index_after_populated_upgrade_and_restart() {
    let conn = db();
    let m = manifest();
    for n in 1..=4 {
        capture(&conn, &m, &fact(n));
    }
    accept(&conn, &m, &one_page(&conn, &m), &clock(100)).unwrap();
    let before: Vec<(String, i64, String)> = conn
        .prepare("SELECT id,source_order,content FROM desk_intake_fact ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    // Model a populated v5 intake schema: migrate the index on normal writable
    // initialization, without rewriting any fact, evidence or custody row.
    conn.execute_batch("DROP INDEX desk_intake_fact_order")
        .unwrap();
    init(&conn).unwrap();
    let after: Vec<(String, i64, String)> = conn
        .prepare("SELECT id,source_order,content FROM desk_intake_fact ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(after, before);
    let sql = format!("EXPLAIN QUERY PLAN {}", subscription::RANGE_QUERY);
    let plan: Vec<String> = conn
        .prepare(&sql)
        .unwrap()
        .query_map(
            params![
                m.domain,
                m.owner_core,
                m.generation,
                m.incarnation,
                ALPHA,
                BOX,
                2,
                4,
                2
            ],
            |r| r.get(3),
        )
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let plan = plan.join("\n");
    assert!(
        plan.contains("SEARCH desk_intake_fact USING INDEX desk_intake_fact_order"),
        "{plan}"
    );
    assert!(!plan.contains("SCAN desk_intake_fact"), "{plan}");
    assert!(
        !plan.contains("TEMP B-TREE"),
        "the entire keyset order is indexed: {plan}"
    );
    let mut visited = Vec::new();
    let (count, last) = subscription::visit_range(&conn, &m, ALPHA, BOX, 2..=4, 2, |o| {
        visited.push(o.order);
        Ok(true)
    })
    .unwrap();
    assert_eq!((count, last, visited), (2, Some(3), vec![2, 3]));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alpha.sqlite");
    conn.execute("VACUUM INTO ?1", [path.to_str().unwrap()])
        .unwrap();
    let reopened = Connection::open(path).unwrap();
    let schema: i64 = reopened
        .pragma_query_value(None, "schema_version", |r| r.get(0))
        .unwrap();
    let changes = reopened.total_changes();
    init(&reopened).unwrap();
    assert_eq!(
        reopened
            .pragma_query_value::<i64, _>(None, "schema_version", |r| r.get(0))
            .unwrap(),
        schema
    );
    assert_eq!(reopened.total_changes(), changes);
    let mut visited = Vec::new();
    let (count, last) = subscription::visit_range(&reopened, &m, ALPHA, BOX, 4..=4, 2, |o| {
        visited.push(o.order);
        Ok(false)
    })
    .unwrap();
    assert_eq!(
        (count, last, visited),
        (1, None, vec![4]),
        "unreserved work cannot advance its keyset"
    );
}
