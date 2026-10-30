use super::*;
use serde_json::json;
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{fs::MetadataExt, net::UnixListener},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

struct HostFixture {
    pane: Arc<Mutex<Pane>>,
    spaces: Arc<Mutex<Value>>,
    calls: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    binding: Binding,
}
impl HostFixture {
    fn start(path: &std::path::Path) -> Self {
        let listener = UnixListener::bind(path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let pane = Pane {
            pane_id: "alpha".into(),
            terminal_id: "beta".into(),
            workspace_id: "gamma".into(),
            tab_id: "alpha".into(),
            agent: Some("pi".into()),
            agent_status: "idle".into(),
            title: Some("secret-title".into()),
            agent_session: Some(json!({"id":"session-canary"})),
        };
        let metadata = std::fs::metadata(path).unwrap();
        let binding = Binding {
            socket: path.into(),
            socket_identity: (metadata.dev(), metadata.ino()),
            pane: pane.clone(),
        };
        let pane = Arc::new(Mutex::new(pane));
        let spaces = Arc::new(Mutex::new(json!([])));
        let calls = Arc::new(Mutex::new(vec![]));
        let stop = Arc::new(AtomicBool::new(false));
        let (p, s, c, done) = (pane.clone(), spaces.clone(), calls.clone(), stop.clone());
        let thread = thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                let (stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let method = request["method"].as_str().unwrap();
                c.lock().unwrap().push(method.into());
                let result = match method {
                    "session.snapshot" => {
                        json!({"snapshot":{"protocol":20,"version":"alpha","panes":[*p.lock().unwrap()]}})
                    }
                    "workspace.list" => json!({"workspaces":*s.lock().unwrap()}),
                    other => panic!("census must never read screens or mutate host: {other}"),
                };
                writeln!(
                    reader.get_mut(),
                    "{}",
                    json!({"id":request["id"],"result":result})
                )
                .unwrap();
            }
        });
        Self {
            pane,
            spaces,
            calls,
            stop,
            thread: Some(thread),
            binding,
        }
    }
    fn raw(&self) -> String {
        serde_json::to_string(&self.binding).unwrap()
    }
}
impl Drop for HostFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }
}
fn empty() -> Input {
    Input {
        jobs: vec![],
        lease: None,
        panes: vec![],
        unbound: false,
        hosted_without_lease: false,
        owner: None,
        session_gap: false,
    }
}
fn observe_now(input: &Input) -> Census {
    let (c, _, _) = observe(input, "survey", Instant::now(), Some("alpha"));
    c.validate().unwrap();
    c
}
fn live_job(id: &str) -> Value {
    json!({"id":id,"incarnation":"alpha","task_id":"survey","status":"running","payload":serde_json::from_str::<Value>(&crate::jobs::own_identity().unwrap()).unwrap()})
}
#[test]
fn hosted_idle_busy_asleep_blocked_and_lapsed_are_distinct() {
    let dir = tempfile::tempdir().unwrap();
    let host = HostFixture::start(&dir.path().join("alpha.sock"));
    let mut input = empty();
    input.lease = Some(("host:alpha".into(), i64::MAX, Some(host.raw()), None));
    let idle = observe_now(&input);
    assert!(!idle.executing && idle.activity_complete);
    assert_eq!(
        (idle.active_workers, idle.sessions.live, idle.sessions.idle),
        (0, 1, 1)
    );
    host.pane.lock().unwrap().agent_status = "working".into();
    let busy = observe_now(&input);
    assert!(busy.executing && busy.activity_complete);
    assert_eq!((busy.active_workers, busy.first_mate), (1, 1));
    input.lease.as_mut().unwrap().3 = Some("host:alpha".into());
    assert!(
        !observe_now(&input).executing,
        "sleep/busy contradiction is unknown"
    );
    host.pane.lock().unwrap().agent_status = "idle".into();
    assert_eq!(observe_now(&input).sessions.asleep, 1);
    input.lease.as_mut().unwrap().3 = None;
    host.pane.lock().unwrap().agent_status = "blocked".into();
    assert_eq!(observe_now(&input).sessions.blocked, 1);
    input.lease.as_mut().unwrap().1 = 0;
    let lapsed = observe_now(&input);
    assert!(!lapsed.executing && !lapsed.activity_complete);
    assert_eq!(lapsed.sessions.unknown, 1);
    assert!(
        host.calls
            .lock()
            .unwrap()
            .iter()
            .all(|m| ["session.snapshot", "workspace.list"].contains(&m.as_str()))
    );
    let text = serde_json::to_string(&busy).unwrap();
    for canary in [
        "secret-title",
        "session-canary",
        "alpha.sock",
        "terminal_id",
        "pid",
    ] {
        assert!(!text.contains(canary), "{text}");
    }
}
#[test]
fn reused_pane_socket_session_and_missing_identity_never_prove_execution() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = HostFixture::start(&dir.path().join("alpha.sock"));
    host.pane.lock().unwrap().agent_status = "working".into();
    let mut input = empty();
    for field in ["terminal", "session", "socket", "missing_session", "agent"] {
        let mut binding = host.binding.clone();
        match field {
            "terminal" => binding.pane.terminal_id = "reused".into(),
            "session" => binding.pane.agent_session = Some(json!({"id":"reused"})),
            "socket" => binding.socket_identity.1 += 1,
            "missing_session" => binding.pane.agent_session = None,
            _ => binding.pane.agent = Some("claude".into()),
        }
        input.lease = Some((
            "host:alpha".into(),
            i64::MAX,
            Some(serde_json::to_string(&binding).unwrap()),
            None,
        ));
        let c = observe_now(&input);
        assert!(!c.executing && !c.sessions_complete, "{field}");
    }
    // Actual socket replacement at the same path, not just a changed label.
    let original = host.raw();
    host.stop.store(true, Ordering::SeqCst);
    host.thread.take().unwrap().join().unwrap();
    // Keep the original inode allocated, so replacement cannot reuse it.
    std::fs::rename(&host.binding.socket, dir.path().join("beta.sock")).unwrap();
    let replacement = HostFixture::start(&host.binding.socket);
    replacement.pane.lock().unwrap().agent_status = "working".into();
    input.lease = Some(("host:alpha".into(), i64::MAX, Some(original), None));
    assert!(!observe_now(&input).executing);
    host.thread = Some(thread::spawn(|| {}));
}
#[test]
fn duplicate_spaces_are_anomalies_not_additional_authorized_mates() {
    let dir = tempfile::tempdir().unwrap();
    let host = HostFixture::start(&dir.path().join("alpha.sock"));
    host.pane.lock().unwrap().agent_status = "working".into();
    *host.spaces.lock().unwrap() = json!([
        {"workspace_id":"alpha","tokens":{"privateer":"mate","ship":"survey"}},
        {"workspace_id":"beta","tokens":{"privateer":"mate","ship":"survey"}},
        {"workspace_id":"beta","tokens":{"privateer":"mate","ship":"survey"}}
    ]);
    let mut input = empty();
    input.lease = Some(("host:alpha".into(), i64::MAX, Some(host.raw()), None));
    let c = observe_now(&input);
    assert_eq!(c.active_workers, 1);
    assert_eq!(c.anomalies["duplicate_spaces"], 2);
    assert!(c.executing && !c.activity_complete);
}
#[test]
fn headless_payload_dedup_crew_viewer_command_and_parked_distinctions() {
    let dir = tempfile::tempdir().unwrap();
    let host = HostFixture::start(&dir.path().join("alpha.sock"));
    host.pane.lock().unwrap().agent_status = "working".into();
    let mut input = empty();
    input
        .jobs
        .push((live_job("alpha").to_string(), "first_mate".into()));
    input
        .jobs
        .push((live_job("beta").to_string(), "first_mate".into()));
    let c = observe_now(&input);
    assert_eq!(
        c.active_workers, 1,
        "same checked payload cannot count twice"
    );
    input.jobs.truncate(1);
    input.panes.push(("alpha".into(), host.raw(), true, true));
    let c = observe_now(&input);
    assert_eq!((c.active_workers, c.sessions.viewer), (1, 1));
    input.jobs[0].1 = "command".into();
    let c = observe_now(&input);
    assert_eq!((c.active_workers, c.commands, c.sessions.viewer), (0, 1, 1));
    input.jobs[0].1 = "crew".into();
    input.panes[0].2 = false;
    let c = observe_now(&input);
    assert_eq!((c.active_workers, c.crew, c.sessions.live), (1, 1, 1));
    input.panes[0].3 = false;
    assert!(
        !observe_now(&input).executing,
        "reused job incarnation cannot adopt old session"
    );
    input.panes.clear();
    for state in ["queued", "parked"] {
        let mut job = live_job("alpha");
        job["status"] = json!(state);
        input.jobs[0].0 = job.to_string();
        assert!(!observe_now(&input).executing);
    }
}
#[test]
fn supervisor_stored_running_foreign_namespace_and_host_failure_stay_unknown() {
    let mut input = empty();
    let mut job = live_job("alpha");
    job["supervisor"] = job["payload"].clone();
    job["payload"] = Value::Null;
    input.jobs.push((job.to_string(), "crew".into()));
    let c = observe_now(&input);
    assert!(!c.executing && !c.workers_complete);
    job = live_job("alpha");
    job["payload"]["pid_namespace"] = json!("unavailable");
    input.jobs[0].0 = job.to_string();
    assert!(!observe_now(&input).executing);
    job = live_job("alpha");
    job["payload"]["started"] = json!("reused");
    input.jobs[0].0 = job.to_string();
    assert!(!observe_now(&input).executing);
    job = live_job("alpha");
    job["hosted"] = json!(true);
    input.jobs[0].0 = job.to_string();
    assert!(
        !observe_now(&input).executing,
        "interactive payload is not a busy turn"
    );
    input.lease = Some(("host:alpha".into(), i64::MAX, Some("{}".into()), None));
    let c = observe_now(&input);
    assert!(!c.executing && !c.sessions_complete);
}
#[test]
fn bounded_census_and_untrusted_count_validation() {
    let mut input = empty();
    for n in 0..=LIMIT {
        let mut job = live_job(&format!("alpha-{n}"));
        job["status"] = json!("queued");
        input.jobs.push((job.to_string(), "crew".into()));
    }
    let c = observe_now(&input);
    assert!(!c.activity_complete && !c.workers_complete);
    assert_eq!(c.reason, "census_truncated");
    let mut bad = Census::empty();
    bad.first_mate = u64::MAX;
    bad.crew = u64::MAX;
    assert!(bad.validate().is_err());
    bad = Census::empty();
    bad.sessions.idle = u64::MAX;
    bad.sessions.viewer = u64::MAX;
    assert!(bad.validate().is_err());
}

#[test]
fn older_home_missing_pane_incarnation_is_read_without_migration() {
    let (_dir, store) = store();
    store
        .connection()
        .execute_batch("ALTER TABLE crew_panes DROP COLUMN incarnation;")
        .unwrap();
    let c = read(&store, "survey").unwrap();
    assert_eq!(c.active_workers, 0);
    assert!(!super::super::column(store.connection(), "crew_panes", "incarnation").unwrap());
}

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.connection().execute_batch("INSERT INTO ships(name,record) VALUES('survey','{\"location\":\"here\"}'); INSERT INTO charters(id,data) VALUES('survey','{}'); INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('survey','survey','survey','survey','open',0);").unwrap();
    (dir, store)
}
#[test]
fn read_rechecks_finish_custody_and_incarnation_in_a_new_transaction() {
    let (_dir, store) = store();
    let job = live_job("alpha");
    store
        .connection()
        .execute(
            "INSERT INTO jobs(id,record) VALUES('alpha',?1)",
            [job.to_string()],
        )
        .unwrap();
    store.connection().execute("INSERT INTO wakes(id,ship,focus,through_seq,mode,status,created_ms,job_id) VALUES('alpha','survey','task:survey',0,'headless','running',0,'alpha')", []).unwrap();
    assert_eq!(
        read(&store, "survey").unwrap().first_mate,
        1,
        "wake plus job counted once"
    );
    for sql in [
        "UPDATE jobs SET record=json_set(record,'$.status','succeeded')",
        "UPDATE jobs SET record=json_set(record,'$.incarnation','beta')",
        "UPDATE tasks SET owner='relay'",
    ] {
        store
            .connection()
            .execute("UPDATE jobs SET record=?1", [job.to_string()])
            .unwrap();
        store
            .connection()
            .execute("UPDATE tasks SET owner='survey'", [])
            .unwrap();
        let result = read_with(&store, "survey", || {
            store.connection().execute(sql, []).unwrap();
        });
        assert!(result.is_err(), "{sql}");
    }
}
#[test]
fn heartbeat_expiry_change_does_not_invalidate_checked_census() {
    let (dir, store) = store();
    let host = HostFixture::start(&dir.path().join("alpha.sock"));
    host.pane.lock().unwrap().agent_status = "working".into();
    store.connection().execute("INSERT INTO presence(ship,holder,started_ms,expires_ms,binding) VALUES('survey','host:alpha',0,?1,?2)", rusqlite::params![Store::now_ms()+60_000,host.raw()]).unwrap();
    let result = read_with(&store, "survey", || {
        store
            .connection()
            .execute("UPDATE presence SET expires_ms=expires_ms+1000", [])
            .unwrap();
    })
    .unwrap();
    assert_eq!(result.first_mate, 1);
    assert!(result.executing);
}
#[test]
fn lapsed_lease_drops_only_its_turn_not_unrelated_job_evidence() {
    let (dir, store) = store();
    let host = HostFixture::start(&dir.path().join("alpha.sock"));
    host.pane.lock().unwrap().agent_status = "working".into();
    store.connection().execute("INSERT INTO presence(ship,holder,started_ms,expires_ms,binding) VALUES('survey','host:alpha',0,?1,?2)", rusqlite::params![Store::now_ms()+60_000,host.raw()]).unwrap();
    store
        .connection()
        .execute(
            "INSERT INTO jobs(id,record) VALUES('alpha',?1)",
            [live_job("alpha").to_string()],
        )
        .unwrap();
    let result = read_with(&store, "survey", || {
        store
            .connection()
            .execute("UPDATE presence SET expires_ms=0", [])
            .unwrap();
    })
    .unwrap();
    assert_eq!(result.first_mate, 0);
    assert_eq!(result.commands, 1);
    assert!(result.session_details.as_ref().unwrap().is_empty());
    assert!(result.executing);
    assert!(!result.sessions_complete);
    assert_eq!(
        host.calls.lock().unwrap().as_slice(),
        ["session.snapshot", "workspace.list"],
        "lease expiry must not trigger a second, unchecked host observation"
    );
}
#[test]
fn lapsed_already_unknown_session_is_not_counted_twice() {
    for (status, asleep) in [("unrecognised", false), ("working", true)] {
        let (dir, store) = store();
        let host = HostFixture::start(&dir.path().join("alpha.sock"));
        host.pane.lock().unwrap().agent_status = status.into();
        store.connection().execute("INSERT INTO presence(ship,holder,started_ms,expires_ms,binding,asleep_holder) VALUES('survey','host:alpha',0,?1,?2,?3)",rusqlite::params![Store::now_ms()+60_000,host.raw(),asleep.then_some("host:alpha")]).unwrap();
        let result = read_with(&store, "survey", || {
            store
                .connection()
                .execute("UPDATE presence SET expires_ms=0", [])
                .unwrap();
        })
        .unwrap();
        assert_eq!(result.sessions.unknown, 1, "{status}/{asleep}");
        assert!(result.session_details.as_ref().unwrap().is_empty());
        assert!(!result.sessions_complete);
    }
}
#[test]
fn anonymous_session_rows_follow_checked_identity_not_state_or_private_labels() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = HostFixture::start(&dir.path().join("alpha.sock"));
    let mut input = empty();
    input.lease = Some(("host:alpha".into(), i64::MAX, Some(host.raw()), None));
    let first = observe_now(&input);
    let row = &first.session_details.as_ref().unwrap()[0];
    assert_eq!(
        (
            &*row.role,
            &*row.state,
            row.harness.as_deref(),
            &*row.host_mode
        ),
        ("first_mate", "idle", Some("pi"), "hosted")
    );
    let reference = row.reference.clone();
    host.pane.lock().unwrap().agent_status = "working".into();
    let busy = observe_now(&input);
    assert_eq!(
        busy.session_details.as_ref().unwrap()[0].reference,
        reference
    );
    assert_eq!(busy.session_details.as_ref().unwrap()[0].state, "working");
    let other_home = observe(&input, "survey", Instant::now(), Some("beta")).0;
    assert_ne!(
        other_home.session_details.as_ref().unwrap()[0].reference,
        reference
    );
    // A checked replacement adapter session at the same endpoint is a new ref.
    host.binding.pane.agent_session = Some(json!({"id":"replacement-canary"}));
    host.pane.lock().unwrap().agent_session = host.binding.pane.agent_session.clone();
    input.lease.as_mut().unwrap().2 = Some(host.raw());
    let replaced = observe_now(&input);
    assert_ne!(
        replaced.session_details.as_ref().unwrap()[0].reference,
        reference
    );
    // Never echo arbitrary adapter identifiers as a harness label.
    host.binding.pane.agent = Some("/private/harness-canary".into());
    host.pane.lock().unwrap().agent = host.binding.pane.agent.clone();
    input.lease.as_mut().unwrap().2 = Some(host.raw());
    let custom = observe_now(&input);
    assert!(
        custom.session_details.as_ref().unwrap()[0]
            .harness
            .is_none()
    );
    for c in [first, busy, other_home, replaced, custom] {
        c.validate().unwrap();
        let text = serde_json::to_string(&c).unwrap();
        for secret in [
            "alpha.sock",
            "session-canary",
            "replacement-canary",
            "harness-canary",
            "secret-title",
            "workspace_id",
            "terminal_id",
            "host:alpha",
            "socket_identity",
        ] {
            assert!(!text.contains(secret), "{secret}: {text}");
        }
    }
}

#[test]
fn session_rows_deduplicate_roles_and_keep_unverified_or_lapsed_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let host = HostFixture::start(&dir.path().join("alpha.sock"));
    host.pane.lock().unwrap().agent_status = "working".into();
    let mut input = empty();
    input.lease = Some(("host:alpha".into(), i64::MAX, Some(host.raw()), None));
    input
        .jobs
        .push((live_job("alpha").to_string(), "crew".into()));
    input.panes.push(("alpha".into(), host.raw(), false, true));
    let c = observe_now(&input);
    assert_eq!(c.session_details.as_ref().unwrap().len(), 1);
    assert_eq!(c.first_mate, 1);
    assert_eq!(c.crew, 0);
    input.lease = None;
    let crew = observe_now(&input);
    assert_eq!(crew.session_details.as_ref().unwrap()[0].role, "crew");
    input.panes[0].2 = true;
    let viewer = observe_now(&input);
    assert_eq!(viewer.session_details.as_ref().unwrap()[0].state, "viewer");
    assert_eq!(
        viewer.active_workers, 1,
        "headless payload still executes independently"
    );
    input.jobs.clear();
    input.panes.clear();
    input.lease = Some(("host:alpha".into(), 0, Some(host.raw()), None));
    let lapsed = observe_now(&input);
    assert!(lapsed.session_details.as_ref().unwrap().is_empty());
    assert_eq!(lapsed.sessions.unknown, 1);
    input.lease.as_mut().unwrap().1 = i64::MAX;
    host.pane.lock().unwrap().agent_session = None;
    let unverified = observe_now(&input);
    assert!(unverified.session_details.as_ref().unwrap().is_empty());
    assert!(!unverified.sessions_complete);
}

#[test]
fn session_detail_wire_validation_and_legacy_missing_origin_fail_closed() {
    let (dir, store) = store();
    let host = HostFixture::start(&dir.path().join("alpha.sock"));
    store.connection().execute("INSERT INTO presence(ship,holder,started_ms,expires_ms,binding) VALUES('survey','host:alpha',0,?1,?2)", rusqlite::params![i64::MAX,host.raw()]).unwrap();
    let first = read(&store, "survey").unwrap();
    let reference = first.session_details.as_ref().unwrap()[0].reference.clone();
    let wire = serde_json::to_value(&first).unwrap();
    for (key, value) in [
        ("reference", json!("raw-session-canary")),
        ("role", json!("secret")),
        ("state", json!("executing")),
        ("host_mode", json!("/private/socket")),
        ("harness", json!("/private/harness")),
        ("argv", json!([])),
    ] {
        let mut bad = wire.clone();
        bad["session_details"][0][key] = value;
        assert!(!serde_json::from_value::<Census>(bad).is_ok_and(|c| c.validate().is_ok()));
    }
    let mut bad = first.clone();
    let duplicate = bad.session_details.as_ref().unwrap()[0].clone();
    bad.session_details.as_mut().unwrap().push(duplicate);
    assert!(bad.validate().is_err());
    let mut bad = first.clone();
    bad.session_details.as_mut().unwrap()[0].state = "blocked".into();
    assert!(bad.validate().is_err(), "row categories must match summary");
    let mut old = wire;
    old.as_object_mut().unwrap().remove("session_details");
    assert!(
        serde_json::from_value::<Census>(old)
            .unwrap()
            .session_details
            .is_none()
    );
    assert!(
        read_with(&store, "survey", || {
            store
                .connection()
                .execute(
                    "UPDATE metadata SET value='beta' WHERE key='runtime_origin'",
                    [],
                )
                .unwrap();
        })
        .is_err(),
        "home-origin race discards all rows/positives"
    );
    assert_ne!(
        read(&store, "survey").unwrap().session_details.unwrap()[0].reference,
        reference
    );
    store
        .connection()
        .execute("DELETE FROM metadata WHERE key='runtime_origin'", [])
        .unwrap();
    let legacy = read(&store, "survey").unwrap();
    assert_eq!(legacy.sessions.idle, 1);
    assert!(
        legacy.session_details.is_none(),
        "no fabricated identity on unmigrated home"
    );
    assert!(
        !store
            .connection()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='runtime_origin')",
                [],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn real_payload_subprocess_exit_never_turns_a_supervisor_into_a_worker() {
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let mut job = live_job("alpha");
    job["payload"] = json!({"pid":child.id(),"started":crate::jobs::process_identity(child.id()).unwrap().unwrap(),"pid_namespace":std::fs::read_link("/proc/self/ns/pid").unwrap().to_string_lossy()});
    let mut input = empty();
    input.jobs.push((job.to_string(), "errand".into()));
    let live = observe_now(&input);
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(live.errand, 1);
    let ended = observe_now(&input);
    assert!(!ended.executing && !ended.activity_complete);
    assert_eq!(ended.uncertain_jobs, 1);
}
