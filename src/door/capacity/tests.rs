use super::*;
use rusqlite::params;

pub(super) fn fixture() -> (
    tempfile::TempDir,
    Store,
    Connection,
    Connection,
    DelegatedContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("survey")).unwrap();
    let ledger_dir = dir.path().join("ledger");
    std::fs::write(
        store.home().join("config.json"),
        json!({"resource_home":ledger_dir}).to_string(),
    )
    .unwrap();
    store.connection().execute_batch("INSERT INTO ships(name,record) VALUES('survey','{\"location\":\"here\"}'); INSERT INTO charters(id,data) VALUES('survey','{\"id\":\"survey\",\"active\":true,\"max_heavy_jobs\":2}'); INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('survey','survey','survey','survey','open',0); INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('relay','survey','relay','hidden-title','open',0);").unwrap();
    let ledger = crate::jobs::shared_ledger(&store).unwrap();
    let registry = Connection::open_in_memory().unwrap();
    registry
        .execute_batch("CREATE TABLE ships(name TEXT,home TEXT,retired_ms INTEGER);")
        .unwrap();
    registry
        .execute(
            "INSERT INTO ships VALUES('survey',?1,NULL)",
            [store.home().to_str().unwrap()],
        )
        .unwrap();
    let c=DelegatedContext::from_gateway(&json!({"version":1,"client":uuid::Uuid::new_v4().to_string(),"resource":"door:alpha","scopes":["fleet:read"],"ships":null,"result_id":uuid::Uuid::new_v4().to_string()})).unwrap();
    (dir, store, ledger, registry, c)
}
pub(super) fn hold(ledger: &Connection, store: &Store, id: &str, ship: &str, heavy: bool) {
    store
        .connection()
        .execute(
            "INSERT OR REPLACE INTO jobs(id,record) VALUES(?1,?2)",
            params![
                id,
                json!({"id":id,"task_id":ship,"incarnation":"alpha","status":"running"})
                    .to_string()
            ],
        )
        .unwrap();
    let value = json!({"key":id,"home":store.home(),"job_id":id,"incarnation":"alpha","heavy":heavy,"state":"running","created_ms":Store::now_ms()-1000,"supervisor":null,"payload":null,"charter":"survey","ship":ship,"task":ship,"demand":{"cpus":if heavy{1}else{0},"memory_bytes":100,"disk_bytes":200},"escaped":[{"pid":999999,"started":"process-canary"}]});
    ledger
        .execute(
            "INSERT INTO reservations VALUES(?1,?2)",
            params![id, value.to_string()],
        )
        .unwrap();
}
fn queue(ledger: &Connection, store: &Store, id: &str, task: &str, blocked_ms: Option<i64>) {
    store.connection().execute("INSERT INTO jobs(id,record) VALUES(?1,?2)",params![id,json!({"id":id,"task_id":task,"incarnation":"alpha","status":"queued","created_ms":0}).to_string()]).unwrap();
    ledger.execute("INSERT INTO queue VALUES(?1,?2)",params![id,json!({"key":id,"home":store.home(),"job_id":id,"incarnation":"alpha","created_ms":0,"supervisor":null,"blocked":"cpu","blocked_ms":blocked_ms}).to_string()]).unwrap();
}
fn restricted(c: &DelegatedContext, ships: Value) -> DelegatedContext {
    let mut wire = c.gateway();
    wire["ships"] = ships;
    DelegatedContext::from_gateway(&wire).unwrap()
}
#[test]
fn finite_unbounded_unknown_policy_and_native_reader_parity_are_nonmutating() {
    let (_dir, store, ledger, registry, c) = fixture();
    hold(&ledger, &store, "alpha", "survey", true);
    hold(&ledger, &store, "beta", "relay", true);
    hold(&ledger, &store, "gamma", "survey", false);
    crate::admission::configure(&ledger, &json!({"max_heavy_jobs":5,"ramp_secs":60})).unwrap();
    queue(&ledger, &store, "queued", "survey", Some(123));
    let args = json!({"machine":"local","ship":"survey","task_id":"survey"});
    let before = std::fs::read(store.home().join("privateer.sqlite")).unwrap();
    let ledger_before: Vec<String> = ledger
        .prepare("SELECT record FROM reservations ORDER BY key")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let reply = local_read(&registry, Command::MachineCapacity, &args, &c).unwrap();
    let cap = reply.capacity.unwrap();
    assert_eq!(cap.occupied_heavy_slots, 2);
    assert_eq!(cap.charter_occupied_heavy_slots, Some(2));
    assert_eq!(cap.free_heavy_slots, Some(0)); // min(host remaining 3, charter remaining 0)
    assert_eq!(cap.reservation_count, 3);
    assert_eq!(cap.memory_reservation_count, 3);
    assert_eq!(cap.queue_depth, 1);
    assert_eq!(cap.queue[0].blocked.as_deref(), Some("cpu"));
    assert_eq!(cap.queue[0].blocked_at_ms, Some(123));
    assert!(cap.queue[0].blocker_basis.contains("last_head"));
    assert!(
        cap.holds
            .iter()
            .all(|h| h.liveness == "unknown_held_conservatively")
    );
    let policy = cap.policy.unwrap();
    let native = crate::admission::settings(&ledger).unwrap();
    assert_eq!(policy.configured_host_heavy_cap, native.max_heavy_jobs);
    assert_eq!(policy.sources["max_heavy_jobs"], "host_configuration");
    assert_eq!(policy.sources["min_free_disk_bytes"], "runtime_default");
    let readings = cap.readings.unwrap();
    assert_eq!(readings.recent_cpus, 2.0);
    assert_eq!(readings.recent_memory_bytes, 300);
    assert!(!readings.disks.is_empty());
    assert_eq!(
        std::fs::read(store.home().join("privateer.sqlite")).unwrap(),
        before
    );
    let ledger_after: Vec<String> = ledger
        .prepare("SELECT record FROM reservations ORDER BY key")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(ledger_before, ledger_after);
    assert_eq!(
        ledger
            .query_row("SELECT COUNT(*) FROM queue", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1,
        "stale queue was not pruned"
    );
    crate::admission::configure(&ledger, &json!({"max_heavy_jobs":null})).unwrap();
    let unbounded = local_read(
        &registry,
        Command::MachineCapacity,
        &json!({"machine":"local","ship":"survey"}),
        &c,
    )
    .unwrap()
    .capacity
    .unwrap();
    assert!(unbounded.policy.unwrap().resolved_host_heavy_cap.is_none());
    assert!(unbounded.free_heavy_slots.is_none());
    ledger
        .execute(
            "UPDATE settings SET value='invalid' WHERE key='admission'",
            [],
        )
        .unwrap();
    let unknown = local_read(&registry, Command::MachineCapacity, &args, &c).unwrap();
    assert!(unknown.capacity.is_none());
    assert_eq!(unknown.coverage.state, "unavailable");
}
#[test]
fn named_ship_lists_even_all_current_ships_cannot_infer_shared_host_by_difference() {
    let (_dir, store, ledger, registry, c) = fixture();
    hold(&ledger, &store, "alpha", "survey", true);
    queue(&ledger, &store, "queued", "survey", Some(123));
    let args = json!({"machine":"local","ship":"survey"});
    for ships in [json!(["survey"]), json!(["survey", "relay"])] {
        let c = restricted(&c, ships);
        let before = local_read(&registry, Command::MachineCapacity, &args, &c)
            .unwrap()
            .capacity
            .unwrap();
        hold(&ledger, &store, "beta", "relay", true);
        queue(&ledger, &store, "hidden-queue", "relay", Some(456));
        crate::admission::configure(
            &ledger,
            &json!({"max_heavy_jobs":1,"min_free_disk_bytes":42}),
        )
        .unwrap();
        let after = local_read(&registry, Command::MachineCapacity, &args, &c)
            .unwrap()
            .capacity
            .unwrap();
        assert_eq!(
            serde_json::to_value(&before).unwrap(),
            serde_json::to_value(&after).unwrap()
        );
        assert_eq!(after.occupied_heavy_slots, 1);
        assert_eq!(after.queue_depth, 1);
        assert!(
            after.policy.is_none() && after.free_heavy_slots.is_none() && after.readings.is_none()
        );
        let text = serde_json::to_string(&after).unwrap();
        for canary in [
            "relay",
            "beta",
            "hidden-queue",
            "process-canary",
            "supervisor",
            "payload",
            "resource_home",
        ] {
            assert!(!text.contains(canary), "{canary}: {text}");
        }
        ledger
            .execute("DELETE FROM reservations WHERE key='beta'", [])
            .unwrap();
        ledger
            .execute("DELETE FROM queue WHERE key='hidden-queue'", [])
            .unwrap();
        store
            .connection()
            .execute("DELETE FROM jobs WHERE id='hidden-queue'", [])
            .unwrap();
    }
}
#[test]
fn typed_needs_filter_before_paging_gate_text_and_bind_continuations() {
    let (_dir, store, _ledger, registry, c) = fixture();
    for (id, sender, task) in [
        ("alpha", "survey", "survey"),
        ("beta", "survey", "survey"),
        ("gamma", "relay", "relay"),
    ] {
        store.connection().execute("INSERT INTO mail(id,sender,recipient,kind,task_id,body,payload,urgency,created_ms,state) VALUES(?1,?2,'captain','request',?3,'Question token=canary-secret /private/path',?4,'high',100,'open')",params![id,sender,task,json!({"choices":["yes","no"],"argv":["approval-program-canary"],"cwd":"/approval-cwd-canary"}).to_string()]).unwrap();
    }
    let args = json!({"ship":"survey","limit":1});
    let first = local_read(&registry, Command::RuntimeNeeds, &args, &c).unwrap();
    assert_eq!(first.needs.len(), 1);
    assert!(first.more);
    assert!(!first.coverage.complete && !first.coverage.mail_complete);
    assert!(first.needs[0].question.is_none() && first.needs[0].text_withheld);
    let native = crate::needs::query(&store, &json!({})).unwrap();
    assert_eq!(first.needs[0].source_id, native["items"][0]["source_id"]);
    let next = json!({"ship":"survey","limit":1,"continuation":first.continuation});
    let second = local_read(&registry, Command::RuntimeNeeds, &next, &c).unwrap();
    assert_eq!(second.needs[0].source_id, "beta");
    assert!(!second.more, "hidden ask cannot create another page");
    let mut wire = c.gateway();
    wire["scopes"] = json!(["fleet:read", "mail:read"]);
    let textc = DelegatedContext::from_gateway(&wire).unwrap();
    let text = local_read(
        &registry,
        Command::RuntimeNeeds,
        &json!({"ship":"survey","include_text":true}),
        &textc,
    )
    .unwrap();
    assert!(text.needs[0].question.is_some());
    assert_eq!(text.needs[0].choices.as_ref().unwrap().len(), 2);
    let serialized = serde_json::to_string(&text).unwrap();
    for canary in [
        "canary-secret",
        "/private/path",
        "approval-program-canary",
        "approval-cwd-canary",
        "gamma",
    ] {
        assert!(!serialized.contains(canary), "{serialized}");
    }
    assert!(local_read(&registry, Command::RuntimeNeeds, &next, &textc).is_err());
    let mut changed = next.clone();
    changed["limit"] = json!(2);
    assert!(local_read(&registry, Command::RuntimeNeeds, &changed, &c).is_err());
    registry
        .execute("UPDATE door_inspection_pages SET issued_ms=-1", [])
        .unwrap();
    assert!(local_read(&registry, Command::RuntimeNeeds, &next, &c).is_err());
    assert!(
        local_read(
            &registry,
            Command::RuntimeNeeds,
            &json!({"ship":"relay"}),
            &restricted(&c, json!(["survey"]))
        )
        .is_err()
    );
}
#[test]
fn fixed_schema_and_receiving_allowlist_refuse_paths_actions_and_forged_scope() {
    for args in [
        json!({}),
        json!({"machine":"/alpha"}),
        json!({"machine":"local","task_id":"survey"}),
        json!({"machine":"local","configure":true}),
    ] {
        assert!(validate_args(Command::MachineCapacity, &args).is_err());
    }
    for args in [
        json!({}),
        json!({"ship":"survey","limit":51}),
        json!({"ship":"survey","include_text":"yes"}),
        json!({"ship":"survey","continuation":"guess"}),
        json!({"ship":"survey","command":"runtime.needs"}),
    ] {
        assert!(validate_args(Command::RuntimeNeeds, &args).is_err());
    }
    let (_dir, _store, _ledger, registry, c) = fixture();
    let restricted = restricted(&c, json!(["survey"]));
    assert!(
        local_read(
            &registry,
            Command::MachineCapacity,
            &json!({"machine":"local"}),
            &restricted
        )
        .is_err()
    );
    let mut wire = c.gateway();
    wire["scopes"] = json!([]);
    let denied = DelegatedContext::from_gateway(&wire).unwrap();
    assert!(
        local_read(
            &registry,
            Command::MachineCapacity,
            &json!({"machine":"local","ship":"survey"}),
            &denied
        )
        .is_err()
    );
}
