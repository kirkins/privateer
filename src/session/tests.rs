use super::*;

#[test]
fn initialized_pane_schema_does_not_acquire_the_writer_lock() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let writer = rusqlite::Connection::open(dir.path().join("privateer.sqlite")).unwrap();
    writer
        .execute_batch("BEGIN IMMEDIATE; UPDATE metadata SET value=value;")
        .unwrap();
    // A second BEGIN IMMEDIATE would fail while writer holds the lock.
    store
        .connection()
        .busy_timeout(std::time::Duration::from_millis(1))
        .unwrap();
    init(&store).unwrap();
    writer.execute_batch("ROLLBACK").unwrap();
}
