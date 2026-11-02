//! Door v6 adds only inactive contact-observation storage, preserving v5 facts.
use super::*;

fn dump(conn: &Connection, sql: &str) -> Value {
    let mut stmt = conn.prepare(sql).unwrap();
    let columns = stmt.column_count();
    json!(
        stmt.query_map([], |r| Ok((0..columns)
            .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
            .collect::<Vec<_>>()))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    )
}
fn current(conn: &Connection, marker: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
        [marker],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn populated_v5_to_v6_preserves_all_prior_schema_facts_custody_and_output() {
    let f = Fixture::new();
    for n in 1..=3 {
        f.add(&format!("mail-{n}"), n, "alpha", Some("open"));
    }
    let s = f.open();
    let first = f.pull(&s, None, 1);
    f.run(f.ack(&s, &first, references(&first)));
    let token = next(&first).unwrap();
    let pending = f.pull(&s, Some(token.clone()), 1);
    // Reconstruct exactly the v5 schema, including its retained marker. No
    // history/proof/selection/automatic table is rebuilt or reinterpreted.
    f.conn
        .execute_batch(
            "DROP TABLE door_client_schema_v6; DROP TABLE desk_replay_observation;
        CREATE TABLE door_client_schema_v5(singleton INTEGER PRIMARY KEY CHECK(singleton=1));",
        )
        .unwrap();
    let names: Vec<String> = f.conn.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").unwrap()
        .query_map([],|r|r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    let facts = |conn: &Connection| {
        names
            .iter()
            .map(|name| dump(conn, &format!("SELECT * FROM {name} ORDER BY rowid")))
            .collect::<Vec<_>>()
    };
    let schema = |conn: &Connection| {
        dump(
            conn,
            "SELECT type,name,sql FROM sqlite_master WHERE name NOT IN ('door_client_schema_v6','desk_replay_observation','sqlite_autoindex_desk_replay_observation_1') ORDER BY type,name",
        )
    };
    let before = facts(&f.conn);
    let before_schema = schema(&f.conn);
    super::super::super::clients::init(&f.conn).unwrap();
    assert!(current(&f.conn, "door_client_schema_v6"));
    assert!(
        current(&f.conn, "door_client_schema_v5"),
        "rollback marker retained"
    );
    assert_eq!(facts(&f.conn), before);
    assert_eq!(schema(&f.conn), before_schema);
    assert_eq!(
        f.count("desk_replay_observation"),
        0,
        "no evidence invented at upgrade"
    );
    assert_eq!(
        f.pull(&s, Some(token.clone()), 1),
        pending,
        "serving stays byte-identical"
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alpha.sqlite");
    f.conn
        .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
        .unwrap();
    let reopened = Fixture {
        conn: Connection::open(path).unwrap(),
        m: f.m.clone(),
        c: DelegatedContext::from_gateway(&f.c.gateway()).unwrap(),
    };
    let version: i64 = reopened
        .conn
        .pragma_query_value(None, "schema_version", |r| r.get(0))
        .unwrap();
    let changes = reopened.conn.total_changes();
    super::super::super::clients::init(&reopened.conn).unwrap();
    assert_eq!(reopened.conn.total_changes(), changes);
    assert_eq!(
        reopened
            .conn
            .pragma_query_value::<i64, _>(None, "schema_version", |r| r.get(0))
            .unwrap(),
        version
    );
    assert_eq!(reopened.pull(&s, Some(token), 1), pending);
    assert_eq!(reopened.count("desk_replay_observation"), 0);
}

#[test]
fn v6_marker_is_not_installed_after_failed_additive_migration() {
    let f = Fixture::new();
    f.conn
        .execute_batch(
            "DROP TABLE door_client_schema_v6; DROP TABLE desk_replay_observation;
        CREATE TABLE door_client_schema_v5(singleton INTEGER PRIMARY KEY CHECK(singleton=1));
        CREATE VIEW desk_replay_observation AS SELECT 1;",
        )
        .unwrap();
    let before = dump(&f.conn, "SELECT * FROM door_clients ORDER BY id");
    let tx = rusqlite::Transaction::new_unchecked(&f.conn, TransactionBehavior::Immediate).unwrap();
    assert!(super::super::super::clients::init(&tx).is_err());
    assert!(!current(&tx, "door_client_schema_v6"));
    assert!(current(&tx, "door_client_schema_v5"));
    assert_eq!(dump(&tx, "SELECT * FROM door_clients ORDER BY id"), before);
    tx.execute("UPDATE door_clients SET name='outer'", [])
        .unwrap();
    tx.rollback().unwrap();
    assert_eq!(
        dump(&f.conn, "SELECT * FROM door_clients ORDER BY id"),
        before
    );
}
