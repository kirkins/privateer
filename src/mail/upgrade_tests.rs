use super::*;

#[test]
fn legacy_ask_survives_premise_migration() {
    let h = rusqlite::Connection::open_in_memory().unwrap();
    h.execute_batch("CREATE TABLE mail(seq INTEGER PRIMARY KEY,id TEXT,body TEXT,state TEXT); INSERT INTO mail VALUES(1,'old-ask','Keep this fact','open');").unwrap();
    upgrade_columns(&h).unwrap();
    let row: (String, String, Option<String>, Option<String>) = h
        .query_row(
            "SELECT body,state,premise,premise_outcome FROM mail WHERE id='old-ask'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(row, ("Keep this fact".into(), "open".into(), None, None));
    h.execute(
        "UPDATE mail SET premise=?1,premise_outcome='done'",
        [r#"{"kind":"task","ref":"work"}"#],
    )
    .unwrap();
    upgrade_columns(&h).unwrap();
    assert_eq!(
        h.query_row("SELECT premise_outcome FROM mail", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "done"
    );
}

#[test]
fn additive_upgrade_fast_path_checks_every_column_index_and_table() {
    let h = rusqlite::Connection::open_in_memory().unwrap();
    h.execute_batch("CREATE TABLE mail(seq INTEGER PRIMARY KEY)")
        .unwrap();
    upgrade_columns(&h).unwrap();
    assert!(upgrade_ready(&h).unwrap());
    let changes = h.total_changes();
    upgrade_columns(&h).unwrap();
    assert_eq!(h.total_changes(), changes);

    for (column, _) in UPGRADE_COLUMNS {
        // Remove a dependent index only when SQLite requires it. All other
        // columns must defeat the fast path even with every object present.
        match *column {
            "delivery" => h.execute_batch("DROP INDEX mail_pending").unwrap(),
            "because_of" => h.execute_batch("DROP INDEX mail_because_of").unwrap(),
            _ => (),
        }
        h.execute_batch(&format!("ALTER TABLE mail DROP COLUMN {column}"))
            .unwrap();
        assert!(!upgrade_ready(&h).unwrap());
        upgrade_columns(&h).unwrap();
        assert!(upgrade_ready(&h).unwrap());
    }
    for (kind, name) in [
        ("INDEX", "mail_pending"),
        ("INDEX", "mail_because_of"),
        ("TABLE", "mail_delivery_retry"),
    ] {
        h.execute_batch(&format!("DROP {kind} {name}")).unwrap();
        assert!(!upgrade_ready(&h).unwrap());
        upgrade_columns(&h).unwrap();
        assert!(upgrade_ready(&h).unwrap());
    }
}
