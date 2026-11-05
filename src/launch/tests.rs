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
fn only_the_bound_waiter_of_this_home_request_and_digest_is_the_requester() {
    let home = Path::new("/ships/harbor/.privateer");
    let argv = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let waiter = |id: &str, digest: &str| {
        argv(&[
            "/bin/privateer",
            "--home",
            "/ships/harbor/.privateer",
            "_launch-wait",
            id,
            digest,
        ])
    };
    assert!(identify::bound(&waiter("r1", "d1"), home, "r1", "d1"));
    for wrong in [
        waiter("r2", "d1"),
        waiter("r1", "d2"),
        argv(&[
            "/bin/privateer",
            "--home",
            "/ships/other/.privateer",
            "_launch-wait",
            "r1",
            "d1",
        ]),
        argv(&[
            "/bin/privateer",
            "--home",
            "/ships/harbor/.privateer",
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
            "d1",
            "x",
        ]),
        argv(&["sleep", "60"]),
    ] {
        assert!(!identify::bound(&wrong, home, "r1", "d1"), "{wrong:?}");
    }
}

#[test]
fn the_digest_a_waiter_binds_covers_the_command_its_arguments_claim_and_directory() {
    let args = json!({"task_id": "t", "id": "j1", "command": ["true"]});
    let claimed = json!({"role": "agent", "actor": "harbor", "env_job": null,
        "fleet": null, "sandbox": "pid_namespace"});
    let bound = digest("job.start", &args, &claimed, Some("/ships/harbor"));
    assert_eq!(bound.len(), 64);
    assert_eq!(
        bound,
        digest("job.start", &args, &claimed, Some("/ships/harbor")),
        "the same content gives the same digest"
    );
    let operator = json!({"role": "operator", "actor": null, "env_job": null,
        "fleet": null, "sandbox": "pid_namespace"});
    for (what, other) in [
        (
            "command",
            digest("job.retry", &args, &claimed, Some("/ships/harbor")),
        ),
        (
            "arguments",
            digest(
                "job.start",
                &json!({"task_id": "t", "id": "j1", "command": ["sh", "-c", "touch x"]}),
                &claimed,
                Some("/ships/harbor"),
            ),
        ),
        (
            "claim",
            digest("job.start", &args, &operator, Some("/ships/harbor")),
        ),
        (
            "directory",
            digest("job.start", &args, &claimed, Some("/tmp")),
        ),
        ("no directory", digest("job.start", &args, &claimed, None)),
    ] {
        assert_ne!(bound, other, "a rewritten {what} changes the digest");
    }
    // A row read back gives the digest its waiter was bound to.
    let (_dir, store) = home();
    insert(&store, "r1", "pending", Store::now_ms() + 60_000, None);
    let row = load(store.connection(), "r1").unwrap().unwrap();
    assert_eq!(
        row_digest(&row),
        digest(&row.command, &row.args, &row.claimed, None)
    );
}

#[test]
fn an_environment_larger_than_a_launcher_adopts_is_refused() {
    let pair = |n: usize, len: usize| {
        (
            OsString::from(format!("V{n}")),
            OsString::from("x".repeat(len)),
        )
    };
    let fits: Vec<_> = (0..identify::ENV_ENTRIES).map(|n| pair(n, 8)).collect();
    identify::ensure_bounded(&fits).unwrap();
    let many: Vec<_> = (0..=identify::ENV_ENTRIES).map(|n| pair(n, 8)).collect();
    let refused = identify::ensure_bounded(&many).unwrap_err().to_string();
    assert!(
        refused.contains("larger than a launcher adopts"),
        "{refused}"
    );
    let large = vec![pair(0, identify::ENV_BYTES)];
    assert!(identify::ensure_bounded(&large).is_err());
}

#[test]
fn an_adopted_environment_keeps_the_first_of_a_name_and_drops_the_sandbox_markers() {
    let env: Vec<(OsString, OsString)> = [
        ("PATH", "/usr/bin"),
        ("CODEX_SANDBOX", "seatbelt"),
        ("PRIVATEER_LAUNCH_VIA", "dispatcher"),
        ("PRIVATEER_JOB", "crew-1"),
        ("PATH", "/tmp/evil"),
        ("CODEX_SANDBOX_NETWORK_DISABLED", "1"),
    ]
    .iter()
    .map(|(k, v)| (OsString::from(k), OsString::from(v)))
    .collect();
    let block: Vec<String> = environment_block(&env)
        .into_iter()
        .map(|entry| entry.into_string().unwrap())
        .collect();
    assert_eq!(block, ["PATH=/usr/bin", "PRIVATEER_JOB=crew-1"]);
}

/// A live `sleep`, its identity as a requester records it, and the
/// launcher's view of it.
#[cfg(target_os = "linux")]
fn sleeper() -> (std::process::Child, Value, identify::Found) {
    let child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id();
    let started = crate::jobs::process_identity(pid).unwrap().unwrap();
    let recorded = json!({"pid": pid, "started": started,
        "pid_namespace": crate::jobs::pid_namespace()});
    let found = identify::Found {
        pid,
        env: vec![],
        cwd: None,
    };
    (child, recorded, found)
}

/// The gap between identifying a requester and resolving its caller: a
/// requester that exits there leaves no ancestry to walk. Its claim, here
/// the operator with no job, must never stand in for the walk.
#[cfg(target_os = "linux")]
#[test]
fn a_requester_that_exits_between_identification_and_resolution_is_refused() {
    let (mut child, recorded, found) = sleeper();
    identify::ensure_still(&recorded, &found).unwrap();
    // Identified; now it exits and is reaped before its caller is resolved.
    child.kill().unwrap();
    child.wait().unwrap();
    let walked = crate::crew_scope::resolve_from(found.pid, None, Caller::Operator, None);
    let refused = walked.expect_err("a gone requester resolves as nobody");
    assert!(
        refused
            .to_string()
            .contains("has exited, so its ancestry cannot be read"),
        "{refused:#}"
    );
    let refused = identify::ensure_still(&recorded, &found)
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("exited while its caller was being resolved"),
        "{refused}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_requester_whose_pid_was_reused_during_resolution_is_refused() {
    let (mut child, mut recorded, found) = sleeper();
    // Another process now holds the pid the request recorded.
    recorded["started"] = json!("linux:0:0");
    let refused = identify::ensure_still(&recorded, &found)
        .unwrap_err()
        .to_string();
    assert!(refused.contains("exited while its caller"), "{refused}");
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(target_os = "linux")]
#[test]
fn the_kernel_gives_the_requester_s_working_directory() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .current_dir(dir.path())
        .spawn()
        .unwrap();
    let pid = child.id();
    let started = crate::jobs::process_identity(pid).unwrap().unwrap();
    let recorded = json!({"pid": pid, "started": started,
        "pid_namespace": crate::jobs::pid_namespace()});
    // Not its waiter, so the identification itself refuses; the directory
    // read is the one it would make.
    assert!(identify::requester(&recorded, Path::new("/h"), "r1", "d1").is_err());
    assert_eq!(
        identify::kernel_cwd(pid).map(|d| std::fs::canonicalize(d).unwrap()),
        Some(std::fs::canonicalize(dir.path()).unwrap())
    );
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(identify::kernel_cwd(pid), None, "a gone process has none");
}

/// Launchers for requests whose `id` is each of `ids`, standing in for
/// `_launch-run`s that are still running.
#[cfg(target_os = "linux")]
fn running_launchers(launchers: &mut Launchers, ids: &[&str]) {
    for id in ids {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        launchers.running.insert((*id).to_owned(), child);
    }
}

#[cfg(target_os = "linux")]
fn end_launchers(launchers: &mut Launchers) {
    for child in launchers.running.values_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn launchers_carrying_out_blocked_commands_never_starve_a_later_cancel() {
    let (_dir, store) = home();
    let conn = store.connection();
    let far = Store::now_ms() + 60_000;
    let alive = crate::jobs::own_identity().unwrap();
    // Four forwarded commands that block (wait_for_capacity), each claimed
    // by a launcher that is still running.
    let blocked = ["wait-1", "wait-2", "wait-3", "wait-4"];
    for id in blocked {
        insert(&store, id, "claimed", far, Some(&alive));
    }
    insert(&store, "cancel", "pending", far, None);
    let mut launchers = Launchers::default();
    running_launchers(&mut launchers, &blocked);
    launcher_pass(store.home(), conn, &mut launchers).unwrap();
    let started = launchers.running.contains_key("cancel");
    end_launchers(&mut launchers);
    assert!(started, "the cancel got a launcher");
    for id in blocked {
        assert_eq!(state(&store, id).0, "claimed", "{id}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn launchers_that_have_not_claimed_yet_are_bounded() {
    let (_dir, store) = home();
    let conn = store.connection();
    let far = Store::now_ms() + 60_000;
    let unclaimed = ["p-1", "p-2", "p-3", "p-4"];
    for id in unclaimed {
        insert(&store, id, "pending", far, None);
    }
    insert(&store, "p-5", "pending", far + 1, None);
    let mut launchers = Launchers::default();
    running_launchers(&mut launchers, &unclaimed);
    launcher_pass(store.home(), conn, &mut launchers).unwrap();
    let started = launchers.running.contains_key("p-5");
    end_launchers(&mut launchers);
    assert!(!started, "four unclaimed launchers fill every slot");
    assert_eq!(state(&store, "p-5").0, "pending");
}

#[test]
fn a_caller_that_is_a_review_errand_or_a_read_only_launch_stays_contained() {
    let (_dir, store) = home();
    let conn = store.connection();
    conn.execute(
        "INSERT INTO reviews(id,ship,task_id,forge,repo,pr,base_ref,base_sha,
        head_ref,head_sha,head_tree,errand_id,route_name,route_identity,route_model,route_tier,
        route_uses,route_why,fresh,authors,declared,folder,clone,verdict,post_state,created_ms,kind)
        VALUES('review-1','harbor','harbor-duty','github','example/project',1,
        'main','base','feature','head','tree','review-errand','reviewer','fixture','model',1,
        '[\"review\"]','fixture',1,'{}','[]','/f','/c','running','pending',0,'gate')",
        [],
    )
    .unwrap();
    let job = |id: &str, extra: Value| {
        let mut record = json!({"id": id, "task_id": "harbor-duty", "incarnation": "alpha",
            "command": ["true"], "cwd": "/ships/harbor", "action": "spawn", "heavy": false,
            "timeout_secs": 60, "ledger": "/ships/ledger", "status": "running",
            "created_ms": 0, "cancel_requested": false});
        for (key, value) in extra.as_object().unwrap() {
            record[key] = value.clone();
        }
        conn.execute(
            "INSERT INTO jobs(id, record) VALUES(?1, ?2)",
            params![id, record.to_string()],
        )
        .unwrap();
        // Each fixture loads: `contained` refuses a proven job that does not.
        crate::jobs::record(&store, id).unwrap();
    };
    job("review-errand", json!({}));
    job("read-only-crew", json!({"codex_read_only_crew": true}));
    job(
        "read-only-launch",
        json!({"launch": {"options": {"sandbox": "read-only"}}}),
    );
    job(
        "writing-crew",
        json!({"launch": {"options": {"sandbox": "workspace-write"}}}),
    );
    assert_eq!(
        contained(&store, "review-errand", true).unwrap(),
        Some("as a read-only review errand")
    );
    assert_eq!(
        contained(&store, "read-only-crew", true).unwrap(),
        Some("read-only")
    );
    assert_eq!(
        contained(&store, "read-only-launch", true).unwrap(),
        Some("read-only")
    );
    assert_eq!(contained(&store, "writing-crew", true).unwrap(), None);
    // Only named by PRIVATEER_JOB: a direct call tolerates a job that does
    // not exist, and so does a forwarded one.
    assert_eq!(contained(&store, "no-such-job", false).unwrap(), None);
}

/// Review 11b5a79e: a job the walk proved whose record this home cannot
/// load, because it is missing here or is not a job record, is refused,
/// never read as no containment, even when the raw row says read-only.
#[test]
fn a_proven_job_whose_record_cannot_be_loaded_stays_contained() {
    let (_dir, store) = home();
    store
        .connection()
        .execute(
            "INSERT INTO jobs(id, record) VALUES('garbled-crew', '{\"id\": \"garbled-crew\", \"codex_read_only_crew\": true}')",
            [],
        )
        .unwrap();
    assert!(crate::jobs::record(&store, "garbled-crew").is_err());
    for job in ["garbled-crew", "elsewhere-crew"] {
        assert_eq!(
            contained(&store, job, true).unwrap(),
            Some(UNREADABLE),
            "{job}"
        );
    }
    let text = contained_refusal("job.start", "elsewhere-crew", UNREADABLE);
    assert!(text.contains("was not started"), "{text}");
    assert!(text.contains("may be read-only"), "{text}");
}

/// Review ff34f5bf: a proven job whose record lives in another home (the
/// walk proved it there, and the command targets this one) is judged from
/// that home's record, not refused for being absent here.
#[test]
fn a_proven_job_in_another_home_is_judged_from_that_home() {
    let (_dir, store) = home();
    let other_dir = tempfile::tempdir().unwrap();
    let other = Store::open(&other_dir.path().join("cove")).unwrap();
    let job = |store: &Store, id: &str, extra: Value| {
        let mut record = json!({"id": id, "task_id": "cove-duty", "incarnation": "alpha",
            "command": ["true"], "cwd": "/ships/cove", "action": "spawn", "heavy": false,
            "timeout_secs": 60, "ledger": "/ships/ledger", "status": "running",
            "created_ms": 0, "cancel_requested": false});
        for (key, value) in extra.as_object().unwrap() {
            record[key] = value.clone();
        }
        store
            .connection()
            .execute(
                "INSERT INTO jobs(id, record) VALUES(?1, ?2)",
                params![id, record.to_string()],
            )
            .unwrap();
        crate::jobs::record(store, id).unwrap();
    };
    job(&other, "cove-writer", json!({}));
    job(&other, "cove-reader", json!({"codex_read_only_crew": true}));
    let elsewhere = Some(other.home());
    let listed = [std::fs::canonicalize(other.home()).unwrap()];
    let proven = |id: &str| vec![(id.to_owned(), true)];
    // Absent here, loadable where it was proven: judged there.
    assert_eq!(
        containment(&store, elsewhere, &listed, &proven("cove-writer")).unwrap(),
        None
    );
    assert_eq!(
        containment(&store, elsewhere, &listed, &proven("cove-reader")).unwrap(),
        Some(("cove-reader".to_owned(), "read-only"))
    );
    // A crew classified with the proven job is judged there too.
    assert_eq!(
        containment(
            &store,
            elsewhere,
            &listed,
            &[("cove-writer".into(), true), ("cove-reader".into(), true)]
        )
        .unwrap(),
        Some(("cove-reader".to_owned(), "read-only"))
    );
    // Without a proven home the record is looked for here, and missing.
    assert_eq!(
        containment(&store, None, &listed, &proven("cove-writer")).unwrap(),
        Some(("cove-writer".to_owned(), UNREADABLE))
    );
    // A proven home the registry lists that is gone cannot be opened, and
    // is unknown; so is one that holds no such job.
    let gone = other_dir.path().join("no-such-home");
    assert!(Store::open_readonly(&gone, Caller::Operator).is_err());
    let listed_gone = [listed[0].clone(), gone.clone()];
    assert_eq!(
        containment(&store, Some(&gone), &listed_gone, &proven("cove-writer")).unwrap(),
        Some(("cove-writer".to_owned(), UNREADABLE))
    );
    // One the registry does not list is refused before any open.
    assert_eq!(
        containment(&store, Some(&gone), &listed, &proven("cove-writer")).unwrap(),
        Some(("cove-writer".to_owned(), UNREADABLE))
    );
    assert_eq!(
        containment(&store, elsewhere, &listed, &proven("cove-missing")).unwrap(),
        Some(("cove-missing".to_owned(), UNREADABLE))
    );
    // A job only PRIVATEER_JOB names is looked for in this home, and a
    // missing one is tolerated, as before.
    assert_eq!(
        containment(&store, elsewhere, &listed, &[("cove-reader".into(), false)]).unwrap(),
        None
    );
}

/// Review 76c970dc: a requester whose PID namespace link cannot be read is
/// refused either way, and the text says whether it exited or the read was
/// denied while it lives.
#[cfg(target_os = "linux")]
#[test]
fn an_unreadable_namespace_says_whether_the_requester_exited_or_was_denied() {
    let text = |errno| {
        identify::namespace_unread(42, &std::io::Error::from_raw_os_error(errno)).to_string()
    };
    for errno in [libc::ENOENT, libc::ESRCH] {
        assert_eq!(text(errno), "process 42 has exited");
    }
    for errno in [libc::EACCES, libc::EPERM] {
        let text = text(errno);
        assert!(
            text.contains("denied") && !text.contains("exited"),
            "{text}"
        );
        assert!(text.contains("/proc/42/ns/pid"), "{text}");
    }
    let other = text(libc::EIO);
    assert!(
        other.contains("could not be read") && !other.contains("exited"),
        "{other}"
    );
    // A real read of a process that is gone gives ENOENT.
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    let error = std::fs::read_link(format!("/proc/{pid}/ns/pid")).unwrap_err();
    assert_eq!(
        identify::namespace_unread(pid, &error).to_string(),
        format!("process {pid} has exited")
    );
}

/// Review 76c970dc: from outside a sandbox every argv is visible, so a
/// requester that can write files can start its own process with a
/// `_job-supervise` argv, nearer than its real supervisor, pointed at a
/// database it wrote whose record names that process as the supervisor of a
/// job that writes. The walk believes it (that weakness predates the
/// launcher); containment does not, because the home is not one the caller
/// could name: it is refused as unknown, never read as no containment.
#[test]
fn a_fake_nearer_supervisor_in_an_unlisted_home_stays_contained() {
    use crate::crew_scope::ancestry::Ancestor;
    let (_dir, store) = home();
    let forged_dir = tempfile::tempdir().unwrap();
    let forged = Store::open(&forged_dir.path().join("decoy")).unwrap();
    const REAL_START: &str = "Mon Nov  2 09:00:00 2026";
    const FAKE_START: &str = "Mon Nov  2 09:30:00 2026";
    let job = |store: &Store, id: &str, pid: u32, started: &str, extra: Value| {
        let mut record = json!({"id": id, "task_id": "harbor-duty", "incarnation": "alpha",
            "command": ["true"], "cwd": "/ships/harbor", "action": "spawn", "heavy": false,
            "timeout_secs": 60, "ledger": "/ships/ledger", "status": "running",
            "created_ms": 0, "cancel_requested": false,
            "supervisor": {"pid": pid, "started": started, "pid_namespace": null}});
        for (key, value) in extra.as_object().unwrap() {
            record[key] = value.clone();
        }
        store
            .connection()
            .execute(
                "INSERT INTO jobs(id, record) VALUES(?1, ?2)",
                params![id, record.to_string()],
            )
            .unwrap();
        crate::jobs::record(store, id).unwrap();
    };
    // The requester's real job, in this home: a read-only crew.
    job(
        &store,
        "harbor-reader",
        200,
        REAL_START,
        json!({"codex_read_only_crew": true}),
    );
    // The forgery: the same job id, recorded as a writer, supervised by the
    // requester's own process 250.
    job(&forged, "harbor-reader", 250, FAKE_START, json!({}));
    let supervise = |home: &Path| {
        Some(
            [
                "privateer",
                "--home",
                &home.display().to_string(),
                "_job-supervise",
                "harbor-reader",
            ]
            .map(str::to_owned)
            .to_vec(),
        )
    };
    let ancestor = |pid: u32, started: &str, argv: Option<Vec<String>>| Ancestor {
        pid,
        started: Some(started.into()),
        comm: Some("privateer".into()),
        argv,
    };
    let chain = [
        ancestor(300, FAKE_START, Some(vec!["codex".into(), "exec".into()])),
        ancestor(250, FAKE_START, supervise(forged.home())),
        ancestor(200, REAL_START, supervise(store.home())),
    ];
    let comms = ["privateer".to_owned()];
    let (proven_home, proven_job, _) = crate::crew_scope::find_supervisor(&chain, &[], &comms, 0)
        .unwrap()
        .unwrap();
    let forged_home = std::fs::canonicalize(forged.home()).unwrap();
    assert_eq!(
        (&proven_home, proven_job.as_str()),
        (&forged_home, "harbor-reader")
    );
    let jobs = [("harbor-reader".to_owned(), true)];
    // The homes this caller can name: this one only. The forged home is not
    // believed, so the job is refused as unknown.
    let listed = [std::fs::canonicalize(store.home()).unwrap()];
    assert_eq!(
        containment(&store, Some(&proven_home), &listed, &jobs).unwrap(),
        Some(("harbor-reader".to_owned(), UNREADABLE))
    );
    // Had the forged home been believed, the writer record would have let
    // the launch out of the sandbox: what the listing check prevents.
    assert_eq!(
        containment(&store, Some(&proven_home), &[forged_home], &jobs).unwrap(),
        None
    );
    // The real supervisor's home is this one, judged here: read-only.
    assert_eq!(
        containment(&store, Some(store.home()), &listed, &jobs).unwrap(),
        Some(("harbor-reader".to_owned(), "read-only"))
    );
}

/// The double-fork residual for forwarded launches: a sandboxed requester
/// must have an ancestor below init outside its sandbox (macOS: one
/// `sandbox_check` reports unsandboxed; Linux: one in another PID
/// namespace). Injected chains, as either OS reads them from outside.
#[test]
fn a_sandboxed_requester_whose_chain_never_leaves_its_sandbox_is_refused() {
    use std::collections::HashMap;
    // macOS: `true` is a sandboxed ancestor, `false` an unsandboxed one.
    let seatbelt = HashMap::from([(30, true), (20, false), (25, true)]);
    let outside = |pid: u32| seatbelt.get(&pid).map(|sandboxed| !sandboxed);
    // Codex's shell (sandboxed) under the app-server (not): it leaves.
    assert!(identify::leaves(&[30, 20], outside));
    // Adopted by launchd after a double fork: no ancestor below it.
    assert!(!identify::leaves(&[], outside));
    // Only launchd, or only sandboxed ancestors, or ones that are gone.
    assert!(!identify::leaves(&[1], |_| Some(true)));
    assert!(!identify::leaves(&[30, 25], outside));
    assert!(!identify::leaves(&[30, 99], outside));
    // Linux: the requester's namespace is "inner"; its init's parent, seen
    // from the dispatcher, is bwrap outside it.
    let namespaces = HashMap::from([(41, "inner"), (40, "outer"), (39, "outer")]);
    let outside = |pid: u32| namespaces.get(&pid).map(|ns| *ns != "inner");
    assert!(identify::leaves(&[41, 40, 39], outside));
    // Its init reparented to the dispatcher namespace's init: it never
    // leaves below pid 1.
    assert!(!identify::leaves(&[41, 1], outside));
    assert!(!identify::leaves(&[41], outside));
    // The refusal names a sandbox limit and that nothing started.
    let text = unconfirmed(
        "job.start",
        &identify::never_leaves(4242, "a macOS sandbox"),
    );
    assert!(text.contains("job.start was not started"), "{text}");
    assert!(
        text.contains("process 4242 runs in a macOS sandbox"),
        "{text}"
    );
    assert!(text.contains("no ancestor of it below init"), "{text}");
    assert!(
        text.contains("not a missing Privateer permission"),
        "{text}"
    );
    assert!(!text.contains("permissions.blocked"), "{text}");
}

/// A requester in this launcher's own PID namespace is not judged by the
/// chain rule (a forced route, or no sandbox): this process passes.
#[cfg(target_os = "linux")]
#[test]
fn a_requester_in_the_dispatchers_namespace_is_not_judged_by_its_chain() {
    let found = identify::Found {
        pid: std::process::id(),
        env: vec![],
        cwd: None,
    };
    identify::ensure_leaves_sandbox(&found, "forced").unwrap();
    // One that has gone cannot be judged at all.
    let gone = identify::Found {
        pid: 99_999_999,
        env: vec![],
        cwd: None,
    };
    let error = identify::ensure_leaves_sandbox(&gone, "pid_namespace")
        .unwrap_err()
        .to_string();
    assert!(error.contains("has exited"), "{error}");
}

#[test]
fn a_contained_refusal_names_the_job_and_a_sandbox_limit() {
    // The refusal names the caller's own job and a sandbox limit.
    let text = contained_refusal("job.start", "review-errand", "as a read-only review errand");
    assert!(text.contains("job review-errand runs as a read-only review errand"));
    assert!(text.contains("not a missing Privateer permission"));
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
    let refused = identify::requester(&recorded, home, "r1", "d1")
        .err()
        .unwrap()
        .to_string();
    assert!(
        refused.contains("is not waiting on this request"),
        "{refused}"
    );
    let reused = json!({"pid": pid, "started": "linux:0:0",
        "pid_namespace": crate::jobs::pid_namespace()});
    let refused = identify::requester(&reused, home, "r1", "d1")
        .err()
        .unwrap()
        .to_string();
    assert!(
        refused.contains("has exited or its pid was reused"),
        "{refused}"
    );
    let _ = bystander.kill();
    let _ = bystander.wait();
    let gone = identify::requester(&recorded, home, "r1", "d1")
        .err()
        .unwrap()
        .to_string();
    assert!(gone.contains("has exited"), "{gone}");
}

#[cfg(target_os = "linux")]
#[test]
fn a_namespace_nothing_visible_lives_in_finds_no_requester() {
    let recorded = json!({"pid": 2, "started": "linux:0:0", "pid_namespace": "pid:[1]"});
    let refused = identify::requester(&recorded, Path::new("/h"), "r1", "d1")
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
