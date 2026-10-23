use super::*;

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn fixture(data: &Path) -> Round {
    for commit in [A, B] {
        fs::create_dir_all(data.join("releases").join(commit).join("bin")).unwrap();
    }
    fs::create_dir_all(rounds(data)).unwrap();
    symlink(Path::new("releases").join(A), data.join("current")).unwrap();
    Round {
        path: state_path(data, "abcd"),
        state: json!({"id": "abcd", "from": A, "commit": B, "requested_ms": 0,
            "outcome": null, "steps": [{"step": "plan", "state": "done"}]}),
    }
}

#[test]
fn killed_between_rename_and_done_save() {
    // Use a fresh test subprocess rather than forking a multithreaded Rust
    // test runner. Kill the actual process after the real rename, before
    // the caller can save its done step.
    if let Some(data) = std::env::var_os("PRIVATEER_TEST_SWITCH_DATA") {
        let data = PathBuf::from(data);
        let path = state_path(&data, "abcd");
        let state = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mut round = Round { path, state };
        let step = std::env::var("PRIVATEER_TEST_SWITCH_STEP").unwrap();
        begin_switch(&mut round, &data, A, B, &step).unwrap();
        unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
        panic!("SIGKILL returned");
    }
    for step in ["switch", "rollback: switch"] {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path();
        let round = fixture(data);
        save(&round.path, &round.state).unwrap();
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "releases::apply::tests::killed_between_rename_and_done_save",
            ])
            .env("PRIVATEER_TEST_SWITCH_DATA", data)
            .env("PRIVATEER_TEST_SWITCH_STEP", step)
            .output()
            .unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(result.status.signal(), Some(libc::SIGKILL), "{result:?}");
        let state: Value = serde_json::from_slice(&fs::read(&round.path).unwrap()).unwrap();
        assert_eq!(state["steps"][1]["state"], "switching");
        assert_eq!(selected(data).unwrap().unwrap().commit, B);
        assert_eq!(last_switch(data, &state), Some((step, B)));
        assert_eq!(interrupted(data, None).unwrap()["id"], "abcd");
        let status = last_round(data);
        assert_eq!(status["interrupted"], true, "{status}");
        let note = status["note"].as_str().unwrap();
        assert!(!note.contains("before it switched anything"), "{status}");
        assert!(note.contains(B), "{status}");
        assert!(
            note.contains(if step == "switch" {
                "resumes its restarts"
            } else {
                "rolling back"
            }),
            "{status}"
        );
        // Intent alone does not prove a switch: a kill before the rename
        // must not claim that the destination was selected.
        switch(data, A).unwrap();
        assert_eq!(last_switch(data, &state), None);
        assert!(
            last_round(data)["note"]
                .as_str()
                .unwrap()
                .contains("before it switched anything")
        );
    }
}

#[test]
fn intent_must_be_saved_before_current_moves() {
    let temp = tempfile::tempdir().unwrap();
    let mut round = fixture(temp.path());
    round.path = temp.path().join("missing/round.json");
    assert!(begin_switch(&mut round, temp.path(), A, B, "switch").is_err());
    assert_eq!(selected(temp.path()).unwrap().unwrap().commit, A);
}

#[test]
fn a_completed_switch_supersedes_an_older_interruption() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path();
    let mut round = fixture(data);
    begin_switch(&mut round, data, A, B, "switch").unwrap();
    let mut newer = round.state.clone();
    newer["id"] = json!("dcba");
    newer["requested_ms"] = json!(1);
    newer["outcome"] = json!("failed");
    newer["steps"][1]["state"] = json!("done");
    save(&state_path(data, "dcba"), &newer).unwrap();
    assert!(interrupted(data, None).is_none());
    assert_eq!(last_round(data)["id"], "dcba");
}

#[test]
fn final_health_recheck_requires_the_same_generation_and_a_held_lock() {
    use std::os::fd::AsRawFd;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let store = Store::open(home).unwrap();
    let set_generation = |generation| {
        store
            .connection()
            .execute(
                "INSERT INTO metadata(key,value) VALUES('dispatcher_generation',?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [generation],
            )
            .unwrap();
    };
    set_generation("first");
    let lock = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join("serve.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let target = Target {
        name: "fixture".into(),
        home: Some(home.into()),
        service: crate::service::restartable_dispatcher(home, "fixture"),
        how: "detached",
    };
    still_up(&target, "first").unwrap();
    set_generation("replacement");
    assert!(
        still_up(&target, "first")
            .unwrap_err()
            .to_string()
            .contains("started again")
    );
    drop(lock);
    assert!(
        still_up(&target, "replacement")
            .unwrap_err()
            .to_string()
            .contains("stopped")
    );
}
