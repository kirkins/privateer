use super::*;

fn home() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("home")).unwrap();
    ensure_table(store.connection()).unwrap();
    (dir, store)
}

fn insert(store: &Store, id: &str, state: &str, deadline_ms: i64, launcher: Option<&str>) {
    store
        .connection()
        .execute(
            "INSERT INTO launch_requests(id, command, args, requester, claimed, state, created_ms,
               deadline_ms, launcher, claimed_ms, finished_ms)
             VALUES(?1, 'job.start', ?2, '{}', ?3, ?4, ?5, ?6, ?7, ?5,
               CASE WHEN ?4 IN ('done','failed','refused','expired') THEN ?5 END)",
            params![
                id,
                json!({"task_id": "t", "id": format!("{id}-job")}).to_string(),
                json!({"role": "agent", "actor": "harbor", "sandbox": "pid_namespace"}).to_string(),
                state,
                Store::now_ms(),
                deadline_ms,
                launcher
            ],
        )
        .unwrap();
}

fn state(store: &Store, id: &str) -> (String, Option<String>) {
    let row = load(store.connection(), id).unwrap().unwrap();
    (row.state, row.error)
}

#[test]
fn proc_status_lines_give_the_innermost_pid_and_the_real_uid() {
    let status = "Name:\tsleep\nUid:\t1000\t1000\t1000\t1000\nNSpid:\t744280\t2\n";
    assert_eq!(identify::nspid_innermost(status), Some(2));
    assert_eq!(identify::real_uid(status), Some(1000));
    assert_eq!(
        identify::nspid_innermost("NSpid:\t17\n"),
        Some(17),
        "a process in the root namespace has one pid"
    );
    assert_eq!(identify::nspid_innermost("Name:\tx\n"), None);
}

#[test]
fn an_environment_block_splits_at_the_first_equals_sign() {
    let env = identify::environ(b"A=1\0PATH=/usr/bin:/bin\0B=x=y\0=skipped\0junk\0\0");
    let pairs: Vec<(String, String)> = env
        .into_iter()
        .map(|(k, v)| (k.into_string().unwrap(), v.into_string().unwrap()))
        .collect();
    assert_eq!(
        pairs,
        [("A", "1"), ("PATH", "/usr/bin:/bin"), ("B", "x=y")]
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
    );
}

#[test]
fn only_the_bound_waiter_of_this_home_and_request_is_the_requester() {
    let home = Path::new("/ships/harbor/.privateer");
    let argv = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    assert!(identify::bound(
        &argv(&[
            "/bin/privateer",
            "--home",
            "/ships/harbor/.privateer",
            "_launch-wait",
            "r1"
        ]),
        home,
        "r1"
    ));
    for wrong in [
        argv(&[
            "/bin/privateer",
            "--home",
            "/ships/harbor/.privateer",
            "_launch-wait",
            "r2",
        ]),
        argv(&[
            "/bin/privateer",
            "--home",
            "/ships/other/.privateer",
            "_launch-wait",
            "r1",
        ]),
        argv(&[
            "/bin/privateer",
            "--home",
            "/ships/harbor/.privateer",
            "job.start",
            "--json",
            "{}",
        ]),
        argv(&[
            "/bin/privateer",
            "--home",
            "/ships/harbor/.privateer",
            "_launch-wait",
            "r1",
            "x",
        ]),
        argv(&["sleep", "60"]),
    ] {
        assert!(!identify::bound(&wrong, home, "r1"), "{wrong:?}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_live_process_not_waiting_on_the_request_is_not_its_requester() {
    let mut bystander = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = bystander.id();
    let started = crate::jobs::process_identity(pid).unwrap().unwrap();
    let home = Path::new("/ships/harbor/.privateer");
    let recorded = json!({"pid": pid, "started": started,
        "pid_namespace": crate::jobs::pid_namespace()});
    let refused = identify::requester(&recorded, home, "r1")
        .err()
        .unwrap()
        .to_string();
    assert!(
        refused.contains("is not waiting on this request"),
        "{refused}"
    );
    let reused = json!({"pid": pid, "started": "linux:0:0",
        "pid_namespace": crate::jobs::pid_namespace()});
    let refused = identify::requester(&reused, home, "r1")
        .err()
        .unwrap()
        .to_string();
    assert!(
        refused.contains("has exited or its pid was reused"),
        "{refused}"
    );
    let _ = bystander.kill();
    let _ = bystander.wait();
    let gone = identify::requester(&recorded, home, "r1")
        .err()
        .unwrap()
        .to_string();
    assert!(gone.contains("has exited"), "{gone}");
}

#[cfg(target_os = "linux")]
#[test]
fn a_namespace_nothing_visible_lives_in_finds_no_requester() {
    let recorded = json!({"pid": 2, "started": "linux:0:0", "pid_namespace": "pid:[1]"});
    let refused = identify::requester(&recorded, Path::new("/h"), "r1")
        .err()
        .unwrap()
        .to_string();
    assert!(
        refused.contains("is visible from the dispatcher"),
        "{refused}"
    );
}

#[test]
fn ids_are_drawn_before_the_request_and_a_given_id_is_kept() {
    let mut start = json!({"task_id": "t"});
    fill_ids("job.start", &mut start);
    assert!(start["id"].as_str().is_some_and(|id| id.len() == 36));
    let mut named = json!({"task_id": "t", "id": "mine"});
    fill_ids("agent.start", &mut named);
    assert_eq!(named["id"], "mine");
    let mut retry = json!({"id": "old"});
    fill_ids("job.retry", &mut retry);
    assert_eq!(retry["id"], "old");
    assert!(retry["new_id"].is_string());
    let mut cancel = json!({"id": "j"});
    fill_ids("job.cancel", &mut cancel);
    assert_eq!(cancel, json!({"id": "j"}));
}

#[test]
fn a_claim_survives_its_round_trip_through_the_row() {
    for caller in [
        Caller::Operator,
        Caller::Agent {
            actor: "harbor".into(),
        },
    ] {
        let claimed = Claimed {
            caller,
            env_job: Some("job-1".into()),
            fleet: Some(PathBuf::from("/fleet.sqlite")),
            sandbox: "seatbelt".into(),
        };
        assert_eq!(Claimed::from_json(&claimed.to_json()).unwrap(), claimed);
    }
    assert!(Claimed::from_json(&json!({"role": "captain"})).is_err());
}

#[test]
fn every_refusal_names_a_sandbox_limit_and_never_a_grant() {
    let home = Path::new("/ships/harbor/.privateer");
    let row = Row {
        id: "r1".into(),
        command: "job.start".into(),
        args: json!({"id": "j1"}),
        requester: json!({}),
        claimed: json!({}),
        cwd: None,
        state: "claimed".into(),
        created_ms: 0,
        launcher: None,
        resolved: None,
        outcome: None,
        error: None,
        claimed_ms: None,
        heartbeat_ms: None,
        finished_ms: None,
    };
    let unclaimed_texts = [
        no_dispatcher("job.start", "pid_namespace", home),
        not_claimed("job.start", "seatbelt", 30_000),
        unconfirmed("errand.start", "process 2 has exited or its pid was reused"),
    ];
    for text in &unclaimed_texts {
        assert!(
            text.contains("not a missing Privateer permission"),
            "{text}"
        );
        assert!(text.contains("No work was started"), "{text}");
        assert!(text.contains("retrying the same command is safe"), "{text}");
    }
    assert!(unclaimed_texts[0].contains("no dispatcher is running for /ships/harbor/.privateer"));
    assert!(unclaimed_texts[0].contains("another PID namespace than the home's dispatcher"));
    assert!(unclaimed_texts[1].contains("within 30 s"));
    assert!(unclaimed_texts[1].contains("a macOS sandbox"));
    let contained = contained_refusal("job.start", "review-1", "as a read-only review errand");
    assert!(contained.contains("not a missing Privateer permission"));
    let lost = stopped(&row);
    assert!(lost.contains("job j1 may have started"), "{lost}");
    for text in unclaimed_texts.iter().chain([&contained, &lost]) {
        assert!(!text.contains("permissions.blocked"), "{text}");
        assert!(!text.to_lowercase().contains("permission denied"), "{text}");
        assert!(!text.contains("grant"), "{text}");
    }
}

#[test]
fn a_withdrawn_request_cannot_be_claimed_and_a_claimed_one_cannot_be_withdrawn() {
    let (_dir, store) = home();
    let conn = store.connection();
    let far = Store::now_ms() + 60_000;
    insert(&store, "a", "pending", far, None);
    assert!(withdraw(conn, "a", "gone").unwrap());
    assert!(!withdraw(conn, "a", "again").unwrap());
    assert_eq!(state(&store, "a"), ("expired".into(), Some("gone".into())));
    insert(&store, "b", "claimed", far, Some("{}"));
    assert!(!withdraw(conn, "b", "late").unwrap());
    assert_eq!(state(&store, "b").0, "claimed");
}

#[test]
fn the_launcher_pass_expires_late_requests_fails_lost_ones_and_prunes() {
    let (_dir, store) = home();
    let conn = store.connection();
    let now = Store::now_ms();
    insert(&store, "late", "pending", now - 1, None);
    insert(&store, "unbound", "recorded", now - 1, None);
    let dead = json!({"pid": std::process::id(), "started": "never", "zombie": false,
        "pid_namespace": crate::jobs::pid_namespace()})
    .to_string();
    insert(&store, "lost", "claimed", now + 60_000, Some(&dead));
    let alive = crate::jobs::own_identity().unwrap();
    insert(&store, "working", "claimed", now + 60_000, Some(&alive));
    for n in 0..(KEEP_ROWS + 5) {
        insert(&store, &format!("old-{n}"), "done", now + 60_000, None);
    }
    conn.execute(
        "UPDATE launch_requests SET finished_ms=?1 WHERE id='old-0'",
        [now - KEEP_MS - 1],
    )
    .unwrap();
    let mut launchers = Launchers::default();
    launcher_pass(store.home(), conn, &mut launchers).unwrap();
    for id in ["late", "unbound"] {
        let (state, error) = state(&store, id);
        assert_eq!(state, "expired", "{id}");
        assert!(
            error
                .unwrap()
                .contains("no dispatcher took the request within 30 s")
        );
    }
    let (lost, error) = state(&store, "lost");
    assert_eq!(lost, "failed");
    assert!(error.unwrap().contains("stopped before recording"));
    assert_eq!(
        state(&store, "working").0,
        "claimed",
        "a live launcher keeps its row"
    );
    assert!(load(conn, "old-0").unwrap().is_none(), "past its keep");
    let kept: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM launch_requests WHERE state IN ('done','failed','refused','expired')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, KEEP_ROWS);
    assert!(launchers.running.is_empty(), "nothing was pending");
}

#[test]
fn a_request_whose_launchers_keep_failing_is_failed_not_retried_forever() {
    let (_dir, store) = home();
    let conn = store.connection();
    insert(&store, "stuck", "pending", Store::now_ms() + 60_000, None);
    let mut launchers = Launchers::default();
    launchers.attempts.insert("stuck".into(), ATTEMPTS);
    launcher_pass(store.home(), conn, &mut launchers).unwrap();
    let (state, error) = state(&store, "stuck");
    assert_eq!(state, "failed");
    let error = error.unwrap();
    assert!(error.contains("No work was started"), "{error}");
    assert!(launchers.running.is_empty());
}

#[test]
fn a_crew_reads_only_its_own_requests() {
    let (_dir, mut store) = home();
    insert(&store, "mine", "done", Store::now_ms(), None);
    store
        .connection()
        .execute(
            "UPDATE launch_requests SET resolved=?1 WHERE id='mine'",
            [json!({"actor": "harbor", "job": "crew-1", "crew": "crew-1"}).to_string()],
        )
        .unwrap();
    store.set_caller(Caller::Agent {
        actor: "harbor".into(),
    });
    assert_eq!(
        dispatch(&mut store, "launch.status", json!({"id": "mine"})).unwrap()["state"],
        "done"
    );
    store.set_caller(Caller::Agent {
        actor: "other".into(),
    });
    assert!(dispatch(&mut store, "launch.status", json!({"id": "mine"})).is_err());
}
