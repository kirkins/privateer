use super::*;
use crate::{needs::query, store::Store};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use serde_json::json;

#[derive(Clone, Copy)]
struct TestClock {
    reading: ClockReading,
    failed: bool,
}

thread_local! {
    static CLOCK: Cell<Option<TestClock>> = const { Cell::new(None) };
}

pub(super) fn clock_override() -> Option<Result<ClockReading>> {
    CLOCK.get().map(|clock| {
        ensure!(!clock.failed, "injected observation clock failure");
        Ok(clock.reading)
    })
}

struct FakeClock;

impl FakeClock {
    fn install() -> Self {
        assert!(CLOCK.get().is_none());
        CLOCK.set(Some(TestClock {
            reading: ClockReading {
                cpu: Duration::ZERO,
                wall: Instant::now(),
            },
            failed: false,
        }));
        Self
    }

    fn advance(cpu: Duration, wall: Duration) {
        let mut clock = CLOCK.get().unwrap();
        clock.reading.cpu += cpu;
        clock.reading.wall += wall;
        CLOCK.set(Some(clock));
    }

    fn fail() {
        let mut clock = CLOCK.get().unwrap();
        clock.failed = true;
        CLOCK.set(Some(clock));
    }
}

impl Drop for FakeClock {
    fn drop(&mut self) {
        CLOCK.set(None);
    }
}

fn query_after_pause(pause: Duration, deadline: Option<Instant>) -> serde_json::Value {
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
    // Pause inside the actual query, after its budget is installed. Advance
    // only elapsed time, modeling a descheduled/I/O-stalled reader without
    // sleeps, host CPU competition, speed assertions, or uptime assumptions.
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
                FakeClock::advance(Duration::ZERO, pause);
            }
            Authorization::Allow
        }));
    // Call directly, just as fleet.snapshot does (not through dispatch).
    match deadline {
        None => query(&store, &json!({})),
        Some(deadline) => crate::needs::query_with_deadline(&store, &json!({}), Some(deadline)),
    }
    .unwrap()
}

#[test]
fn scheduler_pause_does_not_discard_a_captain_request() {
    let _clock = FakeClock::install();
    // This exceeded the former 250 ms elapsed limit, but not the 2 s backstop.
    let needs = query_after_pause(Duration::from_millis(300), None);
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

#[test]
fn stalled_query_is_unavailable_without_partial_items() {
    let _clock = FakeClock::install();
    let needs = query_after_pause(WALL_LIMIT, None);
    assert_eq!(needs["available"], false, "{needs}");
    assert_eq!(needs["items"], json!([]), "{needs}");
    assert_eq!(needs["next_after"], serde_json::Value::Null, "{needs}");
    assert!(!needs["unknown"].as_array().unwrap().is_empty(), "{needs}");
}

#[test]
fn needs_query_honors_the_callers_remaining_deadline() {
    let _clock = FakeClock::install();
    let deadline = observation_clock().unwrap().wall + Duration::from_millis(300);
    let needs = query_after_pause(Duration::from_millis(300), Some(deadline));
    assert_eq!(needs["available"], false, "{needs}");
    assert_eq!(needs["items"], json!([]), "{needs}");
}

const SMALL_SCAN: &str = "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000) SELECT sum(x) FROM n";
const LARGE_SCAN: &str = "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000) SELECT sum(x) FROM n";

fn assert_interrupted(conn: &rusqlite::Connection) {
    let error = conn
        .query_row::<i64, _, _>(SMALL_SCAN, [], |row| row.get(0))
        .unwrap_err();
    assert_eq!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::OperationInterrupted)
    );
}

#[test]
fn scheduler_pause_does_not_interrupt_sqlite_progress() {
    let _clock = FakeClock::install();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let budget = ObservationBudget::new(&conn).unwrap();
    FakeClock::advance(Duration::ZERO, Duration::from_millis(300));
    let sum: i64 = conn.query_row(SMALL_SCAN, [], |row| row.get(0)).unwrap();
    assert_eq!(sum, 500500);
    budget.check().unwrap();
}

#[test]
fn wall_exhaustion_stops_both_checkpoints_and_sqlite_progress_without_cpu_work() {
    let _clock = FakeClock::install();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let budget = ObservationBudget::new(&conn).unwrap();
    FakeClock::advance(Duration::ZERO, WALL_LIMIT);
    assert_eq!(observation_clock().unwrap().cpu, Duration::ZERO);
    assert!(budget.check().is_err());
    assert_interrupted(&conn);
}

#[test]
fn caller_deadline_shortens_but_never_extends_the_wall_backstop() {
    let _clock = FakeClock::install();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for remaining in [Duration::from_secs(1), Duration::from_secs(3)] {
        let now = observation_clock().unwrap().wall;
        let budget = ObservationBudget::with_deadline(&conn, Some(now + remaining)).unwrap();
        let allowed = remaining.min(WALL_LIMIT);
        assert_eq!(budget.wall_deadline(), now + allowed);
        FakeClock::advance(Duration::ZERO, allowed - Duration::from_millis(1));
        budget.check().unwrap();
        let sum: i64 = conn.query_row(SMALL_SCAN, [], |row| row.get(0)).unwrap();
        assert_eq!(sum, 500500);
        FakeClock::advance(Duration::ZERO, Duration::from_millis(1));
        assert!(budget.check().is_err());
        assert_interrupted(&conn);
    }
}

#[test]
fn clock_errors_fail_closed_in_checkpoints_progress_and_installation() {
    let _clock = FakeClock::install();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    {
        let budget = ObservationBudget::new(&conn).unwrap();
        FakeClock::fail();
        assert!(budget.check().is_err());
        assert_interrupted(&conn);
    }
    assert!(ObservationBudget::new(&conn).is_err());
}

#[test]
fn cpu_exhaustion_stops_both_checkpoints_and_sqlite_progress() {
    let _clock = FakeClock::install();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let budget = ObservationBudget::new(&conn).unwrap();
    FakeClock::advance(CPU_LIMIT, Duration::ZERO);
    assert!(budget.check().is_err());
    assert_interrupted(&conn);
}

#[test]
fn deterministic_work_bounds_remain_and_drop_restores_the_connection() {
    let _clock = FakeClock::install();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.busy_timeout(Duration::from_millis(731)).unwrap();
    {
        // The fake clock stays still to exercise the independent work bounds.
        let budget = ObservationBudget::new(&conn).unwrap();
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
