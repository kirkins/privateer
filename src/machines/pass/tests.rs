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
fn rpc(command: &'static str, millis: u64, result: Value) -> Rpc {
    Rpc {
        command,
        cost: Duration::from_millis(millis),
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
            "executable":"/nonexistent/privateer","home":"/fictional/alpha"})
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
fn run(conn: &Connection, name: &str, millis: u64) -> Result<Value> {
    contact(
        conn,
        &super::super::machine(conn, name)?,
        now() + Duration::from_millis(millis),
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
fn refused_directory_cannot_starve_collection_and_real_timeouts_still_fail() {
    let registry = conn(std::path::Path::new(":memory:"));
    add(&registry, "beta");
    let fake = Guard::new(vec![
        Rpc {
            command: "fleet.link",
            cost: Duration::from_millis(100),
            result: Err(anyhow::anyhow!("directory refused")),
        },
        rpc("fleet.collect", 50, json!({"messages":[]})),
        Rpc {
            command: "fleet.link",
            cost: Duration::from_millis(50),
            result: Err(anyhow::anyhow!("timed out after 50 ms")),
        },
    ]);
    assert!(run(&registry, "beta", 100).unwrap()["deferred"].is_string());
    assert_eq!(phase(&registry, "beta"), 1);
    let error = run(&registry, "beta", 100).unwrap_err();
    assert!(error.to_string().contains("timed out"));
    assert_eq!(
        fake.calls()
            .iter()
            .map(|(_, c, _)| c.as_str())
            .collect::<Vec<_>>(),
        ["fleet.link", "fleet.collect", "fleet.link"]
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
            [path.to_str().unwrap()],
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
        [100, 90, 70, 60, 50]
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
            ("fleet.collect", 100),
            ("fleet.collected", 100),
            ("fleet.collect", 90)
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
            ("fleet.collect", 100),
            ("fleet.link", 100),
            ("fleet.collect", 100),
            ("fleet.collected", 50)
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
        [100, 80, 50]
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
        Budget::Pass(now() + Duration::from_secs(1)),
    )
    .unwrap();
    assert!(fake.calls().is_empty());
}
