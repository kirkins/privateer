use super::*;
use std::cell::RefCell;
use std::collections::VecDeque;

struct Rpc {
    command: &'static str,
    cost: Duration,
    result: Result<Value>,
}
struct Fake {
    now: Instant,
    calls: Vec<(String, String, Duration)>,
    replies: VecDeque<Rpc>,
}
thread_local! {
    static FAKE: RefCell<Option<Fake>> = const { RefCell::new(None) };
}
struct Guard;
impl Guard {
    fn new(replies: Vec<Rpc>) -> Self {
        FAKE.with(|fake| {
            assert!(fake.borrow().is_none());
            *fake.borrow_mut() = Some(Fake {
                now: Instant::now(),
                calls: vec![],
                replies: replies.into(),
            });
        });
        Self
    }
    fn calls(&self) -> Vec<(String, String, u128)> {
        FAKE.with(|fake| {
            fake.borrow()
                .as_ref()
                .unwrap()
                .calls
                .iter()
                .map(|(machine, command, cap)| (machine.clone(), command.clone(), cap.as_millis()))
                .collect()
        })
    }
    fn remaining_replies(&self) -> usize {
        FAKE.with(|fake| fake.borrow().as_ref().unwrap().replies.len())
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        FAKE.with(|fake| *fake.borrow_mut() = None);
    }
}
pub(super) fn clock() -> Option<Instant> {
    FAKE.with(|fake| fake.borrow().as_ref().map(|fake| fake.now))
}
pub(super) fn intercept(
    machine: &Machine,
    command: &str,
    _: &Value,
    timeout: Option<Duration>,
) -> Option<Result<Value>> {
    FAKE.with(|fake| {
        let mut fake = fake.borrow_mut();
        let fake = fake.as_mut()?;
        let rpc = fake
            .replies
            .pop_front()
            .expect("unexpected RPC after budget exhaustion");
        assert_eq!(command, rpc.command);
        let cap = timeout.unwrap();
        assert!(rpc.cost <= cap, "fake RPC must obey its deadline");
        fake.calls.push((machine.name.clone(), command.into(), cap));
        fake.now += rpc.cost;
        Some(rpc.result)
    })
}
// Scenario durations use 10 ms ticks, keeping every useful slice above the
// production 250 ms floor. Recorded call caps below are actual milliseconds.
fn rpc(command: &'static str, ticks: u64, result: Value) -> Rpc {
    Rpc {
        command,
        cost: Duration::from_millis(ticks * 10),
        result: Ok(result),
    }
}
fn conn(path: &std::path::Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ships(name TEXT PRIMARY KEY, home TEXT NOT NULL,
        created_by TEXT NOT NULL, created_ms INTEGER NOT NULL, retired_ms INTEGER)",
    )
    .unwrap();
    init(&conn).unwrap();
    conn
}
fn add(conn: &Connection, name: &str) {
    conn.execute(
        "INSERT INTO machines(name,role,peer,known_as,added_ms,incarnation)
        VALUES(?1,'collects',?2,'alpha',0,'generation-1')",
        params![
            name,
            json!({"name":peer_name(name),"transport":"local",
            "executable":"/nonexistent/privateer","home":"/fictional/alpha",
            "timeout_ms":LINK_TIMEOUT_MS})
            .to_string()
        ],
    )
    .unwrap();
}
fn phase(conn: &Connection, name: &str) -> i64 {
    conn.query_row(
        "SELECT contact_phase FROM machines WHERE name=?1",
        [name],
        |r| r.get(0),
    )
    .unwrap()
}
fn run(conn: &Connection, name: &str, ticks: u64) -> Result<Value> {
    contact(
        conn,
        &super::super::machine(conn, name)?,
        now() + Duration::from_millis(ticks * 10),
        Duration::from_secs(10),
        &OnceCell::new(),
    )
}

#[test]
fn reservation_and_early_delivery_share_the_original_end() {
    let start = Instant::now();
    let budget = Duration::from_millis(100);
    assert_eq!(delivery_end(start, budget, false), start + budget);
    assert_eq!(delivery_end(start, budget, true), start + budget / 2);
    // An early delivery leaves its unused reservation to machines, never
    // starts a second full budget. These are clock values, not elapsed time.
    let end = start + budget;
    assert_eq!(
        end.duration_since(start + Duration::from_millis(10))
            .as_millis(),
        90
    );
    assert_eq!(
        end.duration_since(delivery_end(start, budget, true))
            .as_millis(),
        50
    );
}

#[test]
fn exhausted_exchange_rotates_machines_and_collection_across_restarts() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fleet.sqlite");
    let registry = conn(&path);
    for name in ["beta", "gamma"] {
        add(&registry, name);
    }
    let fake = Guard::new(vec![
        rpc("fleet.link", 100, json!({"ships":[]})),
        rpc("fleet.link", 100, json!({"ships":[]})),
        rpc("fleet.collect", 100, json!({"messages":[]})),
        rpc("fleet.collect", 100, json!({"messages":[]})),
    ]);
    for expected in ["beta", "gamma", "beta", "gamma"] {
        let registry = conn(&path);
        let next = ordered(&registry).unwrap().remove(0);
        assert_eq!(next.name, expected);
        let report = run(&registry, expected, 100).unwrap();
        assert!(report["deferred"].is_string(), "{report}");
        let status: (i64, Option<i64>) = registry
            .query_row(
                "SELECT failures,next_contact_ms FROM machines WHERE name=?1",
                [expected],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, (0, None), "budget-skipped work is not unreachable");
    }
    assert_eq!(fake.remaining_replies(), 0);
    assert_eq!(
        fake.calls()
            .iter()
            .map(|(_, c, _)| c.as_str())
            .collect::<Vec<_>>(),
        ["fleet.link", "fleet.link", "fleet.collect", "fleet.collect"]
    );
}

#[test]
fn refused_directory_cannot_starve_collection() {
    let registry = conn(std::path::Path::new(":memory:"));
    add(&registry, "beta");
    let fake = Guard::new(vec![
        Rpc {
            command: "fleet.link",
            cost: Duration::from_millis(1000),
            result: Err(anyhow::anyhow!("directory refused")),
        },
        rpc("fleet.collect", 100, json!({"messages":[]})),
    ]);
    assert!(run(&registry, "beta", 100).unwrap()["deferred"].is_string());
    assert_eq!(phase(&registry, "beta"), 1);
    assert!(run(&registry, "beta", 100).unwrap()["deferred"].is_string());
    assert_eq!(
        fake.calls()
            .iter()
            .map(|(_, c, _)| c.as_str())
            .collect::<Vec<_>>(),
        ["fleet.link", "fleet.collect"]
    );
}

fn home(registry: &Connection, path: &std::path::Path) -> Store {
    let mut store = Store::open(path).unwrap();
    crate::mail::dispatch(
        &mut store,
        "ship.put",
        json!({"name":"alpha","location":"here"}),
    )
    .unwrap();
    registry
        .execute(
            "INSERT INTO ships(name,home,created_by,created_ms) VALUES('alpha',?1,'alpha',0)",
            [store.home().to_str().unwrap()],
        )
        .unwrap();
    store
}
fn captain_message(store: &Store, id: &str, kind: &str, state: &str) {
    store.connection().execute("INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,state,delivery)
        VALUES(?1,'alpha','captain',?2,'Report','{}','normal',0,?3,'received')", params![id,kind,state]).unwrap();
}

#[test]
fn exchange_collect_receipt_and_captain_calls_recompute_the_remainder() {
    let temp = tempfile::tempdir().unwrap();
    let registry = conn(&temp.path().join("fleet.sqlite"));
    add(&registry, "beta");
    let store = home(&registry, &temp.path().join("alpha"));
    captain_message(&store, "report", "notice", "open");
    captain_message(&store, "closed", "request", "withdrawn");
    crate::mail::collected_captain(&store, "machine-beta", &[("closed".into(), None)]).unwrap();
    registry
        .execute(
            "INSERT INTO captain_desk(only,machine,set_ms) VALUES(1,'beta',0)",
            [],
        )
        .unwrap();
    let fake = Guard::new(vec![
        rpc("fleet.link", 10, json!({"ships":[]})),
        rpc(
            "fleet.collect",
            20,
            json!({"messages":[{"id":"incoming","from":"captain","to":"alpha","kind":"notice","body":"Hello","payload":{}}]}),
        ),
        rpc("fleet.collected", 10, json!({})),
        rpc(
            "fleet.captain",
            10,
            json!({"delivered":["report"],"refused":[]}),
        ),
        rpc("fleet.captain", 50, json!({"closed":["closed"]})),
    ]);
    let report = run(&registry, "beta", 100).unwrap();
    assert_eq!(report["delivered"], 1, "{report}");
    assert_eq!(report["captain_pushed"], 1, "{report}");
    assert_eq!(report["captain_closed"], 1, "{report}");
    assert_eq!(
        fake.calls()
            .iter()
            .map(|(_, _, cap)| *cap)
            .collect::<Vec<_>>(),
        [1000, 900, 700, 600, 500]
    );
    assert_eq!(fake.remaining_replies(), 0);
}

#[test]
fn exhausted_receipt_is_durable_and_replayed_before_another_collection() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fleet.sqlite");
    let registry = conn(&path);
    add(&registry, "beta");
    let _store = home(&registry, &temp.path().join("alpha"));
    registry
        .execute(
            "UPDATE machines SET contact_phase=1,refreshed_ms=?1",
            [Store::now_ms()],
        )
        .unwrap();
    let fake = Guard::new(vec![
        rpc(
            "fleet.collect",
            100,
            json!({"messages":[{"id":"incoming","from":"captain","to":"alpha","kind":"notice","body":"Hello","payload":{}}]}),
        ),
        rpc("fleet.collected", 10, json!({})),
        rpc("fleet.collect", 90, json!({"messages":[]})),
    ]);
    let report = run(&registry, "beta", 100).unwrap();
    assert!(report["deferred"].is_string(), "{report}");
    assert_eq!(
        report["delivered"], 1,
        "partial-pass facts must be reported"
    );
    assert_eq!(phase(&registry, "beta"), 2);
    drop(registry);
    let registry = conn(&path);
    let report = run(&registry, "beta", 100).unwrap();
    assert_eq!(report["delivered"], 0, "{report}");
    assert_eq!(
        fake.calls()
            .iter()
            .map(|(_, c, cap)| (c.as_str(), *cap))
            .collect::<Vec<_>>(),
        [
            ("fleet.collect", 1000),
            ("fleet.collected", 1000),
            ("fleet.collect", 900)
        ]
    );
    let receipt: Option<String> = registry
        .query_row(
            "SELECT collect_receipt FROM machines WHERE name='beta'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(receipt.is_none());
}

#[test]
fn a_slow_unknown_batch_yields_to_its_required_directory_exchange() {
    let temp = tempfile::tempdir().unwrap();
    let registry = conn(&temp.path().join("fleet.sqlite"));
    add(&registry, "beta");
    let _store = home(&registry, &temp.path().join("alpha"));
    registry
        .execute(
            "UPDATE machines SET contact_phase=1,refreshed_ms=?1",
            [Store::now_ms()],
        )
        .unwrap();
    let message = json!({"messages":[{"id":"incoming","from":"beta","to":"alpha",
        "kind":"notice","body":"Hello","payload":{}}]});
    let fake = Guard::new(vec![
        rpc("fleet.collect", 100, message.clone()),
        rpc(
            "fleet.link",
            100,
            json!({"ships":[{"name":"beta","scope":"survey"}]}),
        ),
        rpc("fleet.collect", 50, message),
        rpc("fleet.collected", 50, json!({})),
    ]);
    assert!(run(&registry, "beta", 100).unwrap()["deferred"].is_string());
    assert_eq!(
        phase(&registry, "beta"),
        0,
        "refresh, not the slow batch, must go next"
    );
    assert!(run(&registry, "beta", 100).unwrap()["deferred"].is_string());
    assert_eq!(phase(&registry, "beta"), 1);
    let report = run(&registry, "beta", 100).unwrap();
    assert_eq!(report["delivered"], 1, "{report}");
    assert_eq!(
        fake.calls()
            .iter()
            .map(|(_, c, cap)| (c.as_str(), *cap))
            .collect::<Vec<_>>(),
        [
            ("fleet.collect", 1000),
            ("fleet.link", 1000),
            ("fleet.collect", 1000),
            ("fleet.collected", 500)
        ]
    );
}

#[test]
fn inline_refresh_uses_only_the_collect_remainder() {
    let temp = tempfile::tempdir().unwrap();
    let registry = conn(&temp.path().join("fleet.sqlite"));
    add(&registry, "beta");
    let _store = home(&registry, &temp.path().join("alpha"));
    registry
        .execute(
            "UPDATE machines SET contact_phase=1,refreshed_ms=?1",
            [Store::now_ms()],
        )
        .unwrap();
    let fake = Guard::new(vec![
        rpc(
            "fleet.collect",
            20,
            json!({"messages":[{"id":"incoming","from":"beta","to":"alpha",
            "kind":"notice","body":"Hello","payload":{}}]}),
        ),
        rpc(
            "fleet.link",
            30,
            json!({"ships":[{"name":"beta","scope":"survey"}]}),
        ),
        rpc("fleet.collected", 50, json!({})),
    ]);
    let report = run(&registry, "beta", 100).unwrap();
    assert_eq!(report["delivered"], 1, "{report}");
    assert_eq!(
        fake.calls()
            .iter()
            .map(|(_, _, cap)| *cap)
            .collect::<Vec<_>>(),
        [1000, 800, 500]
    );
}

#[test]
fn busy_collection_and_receipt_replay_yield_to_captain_phases() {
    let temp = tempfile::tempdir().unwrap();
    let registry = conn(&temp.path().join("fleet.sqlite"));
    add(&registry, "beta");
    let store = home(&registry, &temp.path().join("alpha"));
    captain_message(&store, "report", "notice", "open");
    captain_message(&store, "closed", "request", "withdrawn");
    crate::mail::collected_captain(&store, "machine-beta", &[("closed".into(), None)]).unwrap();
    registry
        .execute(
            "INSERT INTO captain_desk(only,machine,set_ms) VALUES(1,'beta',0)",
            [],
        )
        .unwrap();
    registry
        .execute(
            "UPDATE machines SET contact_phase=1,refreshed_ms=?1",
            [Store::now_ms()],
        )
        .unwrap();
    let fake = Guard::new(vec![
        rpc(
            "fleet.collect",
            100,
            json!({"messages":[{"id":"incoming","from":"captain","to":"alpha",
            "kind":"notice","body":"Hello","payload":{}}]}),
        ),
        rpc(
            "fleet.captain",
            100,
            json!({"delivered":["report"],"refused":[]}),
        ),
        rpc("fleet.captain", 100, json!({"closed":["closed"]})),
        rpc("fleet.collected", 100, json!({})),
        rpc("fleet.collect", 100, json!({"messages":[]})),
    ]);
    for expected_phase in [2, 3, 0, 2] {
        let report = run(&registry, "beta", 100).unwrap();
        assert!(report["deferred"].is_string(), "{report}");
        assert_eq!(phase(&registry, "beta"), expected_phase);
    }
    let report = run(&registry, "beta", 100).unwrap();
    assert_eq!(report["delivered"], 0, "{report}");
    assert_eq!(phase(&registry, "beta"), 2);
    assert_eq!(fake.remaining_replies(), 0);
    assert_eq!(
        fake.calls()
            .iter()
            .map(|(_, c, _)| c.as_str())
            .collect::<Vec<_>>(),
        [
            "fleet.collect",
            "fleet.captain",
            "fleet.captain",
            "fleet.collected",
            "fleet.collect"
        ]
    );
}

fn status(conn: &Connection, name: &str) -> (i64, Option<i64>) {
    conn.query_row(
        "SELECT failures,next_contact_ms FROM machines WHERE name=?1",
        [name],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

fn recorded(conn: &Connection, ticks: u64) -> Value {
    let machine = super::super::machine(conn, "beta").unwrap();
    record(conn, &machine, run(conn, "beta", ticks)).unwrap()
}

fn timeout(command: &'static str, cap_ms: u64) -> Rpc {
    Rpc {
        command,
        cost: Duration::from_millis(cap_ms),
        // Context must preserve the typed transport cause, not just its text.
        result: Err(anyhow::Error::new(crate::peers::CallTimeout(cap_ms))
            .context("automatic machine RPC")
            .context("contact")),
    }
}

#[test]
fn timeouts_back_off_at_the_budget_aware_threshold_with_spawn_slack() {
    for first_phase in [0, 1] {
        // Real links use 30 seconds, but the pass has only 10 seconds.
        // Half the smaller allowance minus 250 ms is the fair threshold.
        for cap_ms in [4740, 4750, 4760, 4990, 5000, 9990, 10_000] {
            let registry = conn(std::path::Path::new(":memory:"));
            add(&registry, "beta");
            registry
                .execute(
                    "UPDATE machines SET contact_phase=?1,next_contact_ms=0",
                    [first_phase],
                )
                .unwrap();
            let command = if first_phase == 0 {
                "fleet.link"
            } else {
                "fleet.collect"
            };
            let fake = Guard::new(vec![timeout(command, cap_ms)]);
            let report = recorded(&registry, cap_ms / 10);
            if cap_ms < 4750 {
                assert!(report["deferred"].is_string(), "{report}");
                assert_eq!(status(&registry, "beta"), (0, Some(0)));
            } else {
                assert!(
                    report["error"].as_str().unwrap().contains("timed out"),
                    "{report}"
                );
                let (failures, next) = status(&registry, "beta");
                assert_eq!(failures, 1);
                assert!(next.is_some_and(|at| at > 0));
                assert!(!report["deferred"].is_string(), "{report}");
            }
            assert_eq!(
                fake.calls(),
                [("beta".into(), command.into(), u128::from(cap_ms))]
            );
            // Below the useful-slice floor, no RPC or scheduling takes place.
            let before = status(&registry, "beta");
            let next_phase = phase(&registry, "beta");
            let report = recorded(&registry, 24);
            assert!(report["deferred"].is_string(), "{report}");
            assert_eq!(phase(&registry, "beta"), next_phase);
            assert_eq!(status(&registry, "beta"), before);
            assert_eq!(fake.calls().len(), 1);
        }
    }
}

#[test]
fn blackholed_default_peer_backs_off_and_releases_the_delivery_reservation() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fleet.sqlite");
    let registry = conn(&path);
    add(&registry, "beta");
    let store = home(&registry, &temp.path().join("alpha"));
    let budget = Duration::from_millis(10_000);
    let fake = Guard::new(vec![
        timeout("fleet.link", 4950),
        timeout("fleet.collect", 4950),
    ]);
    for expected_failures in [1, 2] {
        // Explicitly make the next contact due instead of waiting for backoff
        // or assuming any particular wall/boot-clock uptime.
        registry
            .execute("UPDATE machines SET next_contact_ms=0", [])
            .unwrap();
        assert!(due_contact(&registry, &path, store.home()).unwrap());
        let start = now();
        let delivery = delivery_end(start, budget, true);
        // Simulate some spawn/registry overhead beyond delivery's half.
        FAKE.with(|fake| {
            fake.borrow_mut().as_mut().unwrap().now = delivery + Duration::from_millis(50);
        });
        let machine = super::super::machine(&registry, "beta").unwrap();
        let outcome = contact(
            &registry,
            &machine,
            start + budget,
            budget,
            &OnceCell::new(),
        );
        let report = record(&registry, &machine, outcome).unwrap();
        assert!(report["error"].is_string(), "{report}");
        let (failures, next) = status(&registry, "beta");
        assert_eq!(failures, expected_failures);
        assert!(next.is_some_and(|at| at > 0));
        // The following passes neither reserve machine time nor initiate a
        // contact while the persisted backoff is in effect.
        for _ in 0..3 {
            let due = due_contact(&registry, &path, store.home()).unwrap();
            assert!(!due);
            assert_eq!(delivery_end(now(), budget, due), now() + budget);
        }
        assert_eq!(fake.calls().len(), expected_failures as usize);
    }
    assert_eq!(fake.remaining_replies(), 0);
}

fn streak(conn: &Connection) -> Option<i64> {
    conn.query_row(
        "SELECT exhausted_timeouts FROM machines WHERE name='beta'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn timeout_threshold_keeps_the_peer_fallback_and_shorter_call_limit() {
    for (configured, cap_ms, fails) in [
        (None, 4740, false),
        (None, 4750, true),
        (Some(2000), 740, false),
        (Some(2000), 750, true),
    ] {
        let registry = conn(std::path::Path::new(":memory:"));
        add(&registry, "beta");
        let mut peer = super::super::machine(&registry, "beta").unwrap().peer;
        peer.as_object_mut().unwrap().remove("timeout_ms");
        if let Some(ms) = configured {
            peer["timeout_ms"] = json!(ms);
        }
        registry
            .execute("UPDATE machines SET peer=?1", [peer.to_string()])
            .unwrap();
        let fake = Guard::new(vec![timeout("fleet.link", cap_ms)]);
        let report = recorded(&registry, cap_ms / 10);
        assert_eq!(report["error"].is_string(), fails, "{report}");
        assert_eq!(report["deferred"].is_string(), !fails, "{report}");
        assert_eq!(status(&registry, "beta").0, i64::from(fails));
        assert_eq!(streak(&registry), if fails { None } else { Some(1) });
        assert_eq!(fake.remaining_replies(), 0);
    }
}

#[test]
fn third_truncated_timeout_backs_off_across_passes_and_restarts() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fleet.sqlite");
    {
        let registry = conn(&path);
        add(&registry, "beta");
        registry
            .execute("UPDATE machines SET next_contact_ms=0", [])
            .unwrap();
    }
    let fake = Guard::new(vec![
        timeout("fleet.link", 4000),
        timeout("fleet.collect", 4000),
        timeout("fleet.link", 4000),
    ]);
    for attempt in 1..=3 {
        let registry = conn(&path);
        assert_eq!(streak(&registry), (attempt > 1).then_some(attempt - 1));
        // Merely running out of time before any RPC never adds a timeout.
        let no_call = recorded(&registry, 24);
        assert!(no_call["deferred"].is_string());
        assert_eq!(streak(&registry), (attempt > 1).then_some(attempt - 1));
        let report = recorded(&registry, 400);
        if attempt < 3 {
            assert!(report["deferred"].is_string(), "{report}");
            assert_eq!(streak(&registry), Some(attempt));
            assert_eq!(status(&registry, "beta"), (0, Some(0)));
        } else {
            assert!(report["error"].is_string(), "{report}");
            assert!(report["error"].as_str().unwrap().contains("timed out"));
            assert_eq!(streak(&registry), None);
            let (failures, next) = status(&registry, "beta");
            assert_eq!(failures, 1);
            assert!(next.is_some_and(|at| at > 0));
        }
    }
    let registry = conn(&path);
    assert_eq!(streak(&registry), None);
    assert_eq!(status(&registry, "beta").0, 1);
    assert_eq!(fake.remaining_replies(), 0);
}

#[test]
fn healthy_late_pass_timeouts_never_escalate_with_successes_in_between() {
    let registry = conn(std::path::Path::new(":memory:"));
    add(&registry, "beta");
    let fake = Guard::new(vec![
        timeout("fleet.collect", 4000),
        rpc("fleet.collect", 400, json!({"messages":[]})),
        timeout("fleet.collect", 4000),
        rpc("fleet.collect", 400, json!({"messages":[]})),
        timeout("fleet.collect", 4000),
    ]);
    for expected in [Some(1), None, Some(1), None, Some(1)] {
        registry
            .execute("UPDATE machines SET contact_phase=1,next_contact_ms=0", [])
            .unwrap();
        let report = recorded(&registry, 400);
        assert!(report["deferred"].is_string(), "{report}");
        assert_eq!(status(&registry, "beta"), (0, Some(0)));
        assert_eq!(streak(&registry), expected);
    }
    assert_eq!(fake.remaining_replies(), 0);
}

#[test]
fn real_failure_and_explicit_success_reset_the_timeout_streak() {
    let registry = conn(std::path::Path::new(":memory:"));
    add(&registry, "beta");
    let fake = Guard::new(vec![
        timeout("fleet.collect", 4000),
        timeout("fleet.collect", 5000),
        timeout("fleet.collect", 4000),
        rpc("fleet.link", 10, json!({"ships":[]})),
    ]);
    for (ticks, expected) in [(400, Some(1)), (500, None), (400, Some(1))] {
        registry
            .execute("UPDATE machines SET contact_phase=1,next_contact_ms=0", [])
            .unwrap();
        let report = recorded(&registry, ticks);
        assert_eq!(report["error"].is_string(), expected.is_none(), "{report}");
        assert_eq!(streak(&registry), expected);
    }
    Budget::Call(Some(Duration::from_secs(30)))
        .call(
            &registry,
            &super::super::machine(&registry, "beta").unwrap(),
            "fleet.link",
            json!({}),
        )
        .unwrap();
    assert_eq!(streak(&registry), None);
    assert_eq!(fake.remaining_replies(), 0);
}

#[test]
fn timeout_streak_migration_is_nullable_and_preserves_existing_link_state() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fleet.sqlite");
    let registry = conn(&path);
    add(&registry, "beta");
    registry
        .execute_batch(
            "ALTER TABLE machines DROP COLUMN exhausted_timeouts;
         UPDATE machines SET failures=2,next_contact_ms=123,contact_phase=1;",
        )
        .unwrap();
    assert!(needs_migration(&registry).unwrap());
    init(&registry).unwrap();
    assert_eq!(streak(&registry), None);
    assert_eq!(status(&registry, "beta"), (2, Some(123)));
    assert_eq!(phase(&registry, "beta"), 1);
    let not_null: bool = registry
        .query_row(
            "SELECT \"notnull\" FROM pragma_table_info('machines') WHERE name='exhausted_timeouts'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!not_null);
    registry
        .execute("UPDATE machines SET exhausted_timeouts=2", [])
        .unwrap();
    drop(registry);
    let registry = conn(&path);
    assert_eq!(streak(&registry), Some(2));
    assert_eq!(status(&registry, "beta"), (2, Some(123)));
    assert_eq!(
        super::super::machine(&registry, "beta").unwrap().peer["timeout_ms"],
        LINK_TIMEOUT_MS
    );
}

#[test]
fn slow_healthy_collection_cut_short_late_in_a_pass_stays_due() {
    let registry = conn(std::path::Path::new(":memory:"));
    add(&registry, "beta");
    registry
        .execute("UPDATE machines SET next_contact_ms=0", [])
        .unwrap();
    let fake = Guard::new(vec![
        rpc("fleet.link", 600, json!({"ships":[]})),
        timeout("fleet.collect", 4000),
        rpc("fleet.collect", 10, json!({"messages":[]})),
    ]);
    let report = recorded(&registry, 1000);
    assert!(report["deferred"].is_string(), "{report}");
    assert_eq!(status(&registry, "beta"), (0, Some(0)));
    let report = recorded(&registry, 1000);
    assert_eq!(report["delivered"], 0, "{report}");
    assert!(!report["deferred"].is_string(), "{report}");
    assert_eq!(status(&registry, "beta").0, 0);
    assert_eq!(
        fake.calls(),
        [
            ("beta".into(), "fleet.link".into(), 10_000),
            ("beta".into(), "fleet.collect".into(), 4000),
            ("beta".into(), "fleet.collect".into(), 10_000),
        ]
    );
}

#[test]
fn remote_refusal_mentioning_transport_timeout_is_not_a_local_timeout() {
    for first_phase in [0, 1] {
        let registry = conn(std::path::Path::new(":memory:"));
        add(&registry, "beta");
        registry
            .execute(
                "UPDATE machines SET contact_phase=?1,next_contact_ms=0",
                [first_phase],
            )
            .unwrap();
        let mut replies = vec![Rpc {
            command: if first_phase == 0 {
                "fleet.link"
            } else {
                "fleet.collect"
            },
            cost: Duration::from_millis(100),
            result: Err(anyhow::anyhow!(
                "machine beta: peer call timed out after 4000 milliseconds"
            )),
        }];
        if first_phase == 0 {
            replies.push(rpc("fleet.collect", 390, json!({"messages":[]})));
        }
        let fake = Guard::new(replies);
        let report = recorded(&registry, 400);
        if first_phase == 0 {
            // Refresh refusal still allows collection, despite its text.
            assert_eq!(report["delivered"], 0, "{report}");
            assert_eq!(fake.calls().len(), 2);
            assert_eq!(status(&registry, "beta"), (0, Some(0)));
        } else {
            assert!(report["error"].is_string(), "{report}");
            assert!(!report["deferred"].is_string(), "{report}");
            assert_eq!(status(&registry, "beta").0, 1);
        }
        assert_eq!(fake.remaining_replies(), 0);
    }
}

#[test]
fn successful_partial_contact_recovers_without_scheduling_unfinished_work() {
    let registry = conn(std::path::Path::new(":memory:"));
    add(&registry, "beta");
    registry
        .execute(
            "UPDATE machines SET contact_phase=1,failures=2,next_contact_ms=0",
            [],
        )
        .unwrap();
    let fake = Guard::new(vec![rpc("fleet.collect", 100, json!({"messages":[]}))]);
    let report = recorded(&registry, 100);
    assert_eq!(report["delivered"], 0, "{report}");
    assert!(report["deferred"].is_string(), "{report}");
    assert_eq!(status(&registry, "beta"), (0, Some(0)));
    assert_eq!(fake.remaining_replies(), 0);
}

#[test]
fn reservation_matches_canonical_homes_and_ignores_a_siblings_contact_lock() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fleet.sqlite");
    let registry = conn(&path);
    add(&registry, "beta");
    let store = home(&registry, &temp.path().join("alpha"));
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(store.home(), &alias).unwrap();
    assert!(registered_home(&registry, &alias).unwrap());
    registry
        .execute("UPDATE ships SET home=?1", [alias.to_str().unwrap()])
        .unwrap();
    assert!(registered_home(&registry, store.home()).unwrap());
    assert!(due_contact(&registry, &path, store.home()).unwrap());
    let lock = contact_lock(&path, "beta").unwrap().unwrap();
    assert!(!due_contact(&registry, &path, &alias).unwrap());
    drop(lock);
    assert!(due_contact(&registry, &path, &alias).unwrap());
}

#[test]
fn a_receipt_from_a_replaced_link_is_never_sent() {
    let registry = conn(std::path::Path::new(":memory:"));
    add(&registry, "beta");
    let receipt = json!({"incarnation":link_incarnation(&registry,"beta").unwrap(),"body":{}});
    registry
        .execute(
            "UPDATE machines SET collect_receipt=?1,incarnation='new'",
            [receipt.to_string()],
        )
        .unwrap();
    let fake = Guard::new(vec![]);
    acknowledge(
        &registry,
        &super::super::machine(&registry, "beta").unwrap(),
        Budget::Pass {
            deadline: now() + Duration::from_secs(1),
            allowance: Duration::from_secs(10),
            reached: &Cell::new(false),
        },
    )
    .unwrap();
    assert!(fake.calls().is_empty());
}
