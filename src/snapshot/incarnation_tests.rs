use super::*;

#[test]
fn replacement_rejects_old_rows_conditional_receipts_and_late_writes() {
    let conn = Connection::open_in_memory().unwrap();
    crate::machines::init(&conn).unwrap();
    conn.execute("INSERT INTO machines(name,role,peer,added_ms,incarnation) VALUES('beta','collects','{}',0,'alpha')", []).unwrap();
    let alpha = crate::machines::link_incarnation(&conn, "beta").unwrap();
    let old = json!({"machine":{"name":"beta"},"ships":[{"name":"survey","presence":"asleep"}],"needs":[]});
    record_remote(&conn, ("beta", &alpha), "direct", 10, 7, Some(&old), 100).unwrap();
    let reused = reached(&conn, "beta", &[], 101).unwrap();
    assert_eq!(reused.row["via"], "reused");
    assert_eq!(reused.section.unwrap()["ships"][0]["presence"], "asleep");

    conn.execute(
        "UPDATE machines SET incarnation='gamma' WHERE name='beta'",
        [],
    )
    .unwrap();
    let gamma = crate::machines::link_incarnation(&conn, "beta").unwrap();
    assert!(stored_for(&conn, "beta", &gamma).unwrap().is_none());
    assert!(stored_for(&conn, "beta", &alpha).unwrap().is_none());
    // The replacement happens to have the same changed_ms: it cannot confirm
    // the previous peer's row, even if that response omits its section.
    assert!(
        record_remote(&conn, ("beta", &gamma), "direct", 11, 7, None, 101)
            .unwrap()
            .is_none()
    );
    let new = json!({"machine":{"name":"beta"},"ships":[{"name":"survey","presence":"absent"}],"needs":[]});
    record_remote(&conn, ("beta", &gamma), "direct", 12, 7, Some(&new), 102).unwrap();
    assert!(record_remote(&conn, ("beta", &alpha), "direct", 13, 8, Some(&old), 103).is_err());
    assert!(record_remote(&conn, ("beta", &alpha), "direct", 13, 7, None, 103).is_err());
    assert_eq!(
        stored_for(&conn, "beta", &gamma).unwrap().unwrap().section,
        new
    );
    forget(&conn, "beta").unwrap();
    assert!(record_remote(&conn, ("beta", &alpha), "direct", 13, 8, Some(&old), 103).is_err());
    assert!(
        stored(&conn, "beta").unwrap().is_none(),
        "a late response cannot recreate a row"
    );
}

#[test]
fn legacy_row_with_recent_receipt_is_not_a_linked_fallback() {
    let conn = Connection::open_in_memory().unwrap();
    crate::machines::init(&conn).unwrap();
    conn.execute("INSERT INTO machines(name,role,peer,added_ms,incarnation) VALUES('beta','collected','{}',0,'alpha')", []).unwrap();
    conn.execute("INSERT INTO fleet_snapshots(machine,digest,changed_ms,as_of_ms,via,body,received_ms) VALUES('beta','alpha',1,1,'collect','{}',100)", []).unwrap();
    let incarnation = crate::machines::link_incarnation(&conn, "beta").unwrap();
    assert!(stored_for(&conn, "beta", &incarnation).unwrap().is_none());
    assert_eq!(
        reaching(&conn, "beta", &[], 101).unwrap().row["via"],
        "directory"
    );
}
