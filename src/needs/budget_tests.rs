use super::*;
use crate::{needs::query, store::Store};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use serde_json::json;

#[test]
fn scheduler_pause_does_not_discard_a_captain_request() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .connection()
        .execute(
            "INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,state)
         VALUES('alpha','beta','captain','request','Review alpha?','{}','normal',1,'open')",
            [],
        )
        .unwrap();
    // Park inside the actual query, after its budget is installed. This models
    // a descheduled reader without competing for the host's CPUs or assuming
    // anything about wall/boot clock uptime. No elapsed-time assertion: an
    // arbitrarily late resume must still retain this tiny, valid observation.
    let (sender, receiver) = std::sync::mpsc::channel::<()>();
    let mut paused = false;
    store
        .connection()
        .authorizer(Some(move |context: AuthContext<'_>| {
            if !paused
                && matches!(
                    context.action,
                    AuthAction::Read {
                        table_name: "metadata",
                        ..
                    }
                )
            {
                paused = true;
                assert!(matches!(
                    receiver.recv_timeout(Duration::from_millis(300)),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ));
            }
            Authorization::Allow
        }));
    // Call directly, just as fleet.snapshot does (not through dispatch).
    let needs = query(&store, &json!({})).unwrap();
    drop(sender);
    assert_eq!(needs["available"], true, "{needs}");
    assert!(
        needs["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["source_id"] == "alpha"),
        "request missing: {needs}"
    );
}

const SMALL_SCAN: &str = "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000) SELECT sum(x) FROM n";
const LARGE_SCAN: &str = "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000) SELECT sum(x) FROM n";

#[test]
fn scheduler_pause_does_not_interrupt_sqlite_progress() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let budget = ObservationBudget::new(&conn).unwrap();
    let (_sender, receiver) = std::sync::mpsc::channel::<()>();
    assert!(matches!(
        receiver.recv_timeout(Duration::from_millis(300)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    let sum: i64 = conn.query_row(SMALL_SCAN, [], |row| row.get(0)).unwrap();
    assert_eq!(sum, 500500);
    budget.check().unwrap();
}

#[test]
fn cpu_exhaustion_stops_both_checkpoints_and_sqlite_progress() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    // Exhausted from the start, without spinning or making speed assumptions.
    let budget = ObservationBudget::with_cpu_limit(&conn, Duration::ZERO).unwrap();
    assert!(budget.check().is_err());
    let error = conn
        .query_row::<i64, _, _>(SMALL_SCAN, [], |row| row.get(0))
        .unwrap_err();
    assert_eq!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::OperationInterrupted)
    );
}

#[test]
fn deterministic_work_bounds_remain_and_drop_restores_the_connection() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.busy_timeout(Duration::from_millis(731)).unwrap();
    {
        // Keep the CPU limit out of the way to exercise the independent bounds.
        let budget = ObservationBudget::with_cpu_limit(&conn, Duration::from_secs(3600)).unwrap();
        for _ in 0..4096 {
            budget.check().unwrap();
        }
        assert!(budget.check().is_err());
        let error = conn
            .query_row::<i64, _, _>(LARGE_SCAN, [], |row| row.get(0))
            .unwrap_err();
        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::OperationInterrupted)
        );
    }
    let timeout: u64 = conn
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap();
    assert_eq!(timeout, 731);
    let sum: i64 = conn.query_row(LARGE_SCAN, [], |row| row.get(0)).unwrap();
    assert_eq!(sum, 500000500000);
}
