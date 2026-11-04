use super::*;

#[test]
fn only_the_current_hosted_claude_lease_records_the_pane_session() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = Store::open(temp.path()).unwrap();
    crate::dispatch(
        &mut store,
        "ship.put",
        json!({"name":"alpha","location":"here","scope":"Publish beta"}),
    )
    .unwrap();
    store
        .connection()
        .execute(
            "INSERT INTO hosted_panes(ship,pid,started,executable,recorded_ms)
         VALUES('alpha',123,'alpha','alpha',?1)",
            [Store::now_ms()],
        )
        .unwrap();
    attend(&store, "alpha", "host:alpha", 90, Some("claude")).unwrap();
    let session = || -> Option<String> {
        store
            .connection()
            .query_row(
                "SELECT session_id FROM hosted_panes WHERE ship='alpha'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    record_hosted_claude_session(&store, "alpha", "claude:beta", "beta").unwrap();
    record_hosted_claude_session(&store, "alpha", "host:beta", "beta").unwrap();
    assert_eq!(session(), None);
    record_hosted_claude_session(&store, "alpha", "host:alpha", "alpha").unwrap();
    assert_eq!(session().as_deref(), Some("alpha"));
    record_hosted_claude_session(&store, "alpha", "host:alpha", "../beta").unwrap();
    assert_eq!(session().as_deref(), Some("alpha"));
    store
        .connection()
        .execute("UPDATE presence SET expires_ms=0", [])
        .unwrap();
    record_hosted_claude_session(&store, "alpha", "host:alpha", "gamma").unwrap();
    assert_eq!(session().as_deref(), Some("alpha"));
}
