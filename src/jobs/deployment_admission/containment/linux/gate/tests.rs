use super::*;

fn isolated_boundary() -> Option<Qualified> {
    if let Some(path) = std::env::var_os("PRIVATEER_TEST_CGROUP_DELEGATION") {
        Some(
            super::super::qualify(Path::new(&path))
                .expect("explicit isolated delegation must qualify; no fake fallback"),
        )
    } else {
        let temp = tempfile::tempdir().unwrap();
        let refused = super::super::qualify(temp.path()).unwrap_err();
        eprintln!(
            "unqualified exec-gate test environment; activation refused: {}",
            refused.reason
        );
        assert!(refused.retained.is_none());
        None
    }
}

fn fixture_args(name: &str) -> Vec<CString> {
    vec![
        CString::new("alpha").unwrap(),
        CString::new("--exact").unwrap(),
        CString::new(format!(
            "jobs::deployment_admission::containment::linux::gate::tests::{name}"
        ))
        .unwrap(),
        CString::new("--nocapture").unwrap(),
    ]
}

#[test]
fn native_fixture() {
    if std::env::var("PRIVATEER_ADAPTER_PROTOCOL").as_deref() != Ok("1") {
        return;
    }
    assert_eq!(std::env::vars_os().count(), 1);
    assert_eq!(unsafe { libc::umask(0o077) }, 0o077);
    // Inspect before any filesystem IO opens temporary descriptors. Rust's test
    // harness owns no inherited files; only null standard streams may remain.
    for fd in 3..1024 {
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) },
            -1,
            "inherited fd {fd}"
        );
    }
    for fd in 0..3 {
        assert_eq!(
            std::fs::read_link(format!("/proc/self/fd/{fd}")).unwrap(),
            Path::new("/dev/null")
        );
    }
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(std::fs::metadata(&cwd).unwrap().mode() & 0o777, 0o700);
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    for line in [
        "CapEff:\t0000000000000000",
        "CapPrm:\t0000000000000000",
        "NoNewPrivs:\t1",
    ] {
        assert!(status.lines().any(|s| s == line), "{line}");
    }
    std::fs::write(cwd.join("executed"), "native fixture only").unwrap();
}

#[test]
fn real_gate_blocks_until_release_and_execs_with_fixed_policy() {
    let Some(mut held) = isolated_boundary() else {
        return;
    };
    let executable = File::open("/proc/self/exe").unwrap();
    let pending = held
        .gate(&executable, &fixture_args("native_fixture"), 10_000)
        .unwrap();
    assert!(!pending._cwd.path().join("executed").exists());
    assert!(pending.held.cleanup_unknown);
    assert!(pending.held.identity.get("gate_process").is_some());
    let status = pending.execute().unwrap();
    assert!(libc::WIFEXITED(status));
    assert_eq!(libc::WEXITSTATUS(status), 0, "native policy fixture failed");
    assert!(!held.cleanup_unknown);
    assert_eq!(populated(&held.read(c"cgroup.events").unwrap()).unwrap(), 0);
    held.dispose_test_boundary();
}

#[test]
fn real_gate_drop_and_deadline_kill_and_empty_without_replay() {
    let Some(mut held) = isolated_boundary() else {
        return;
    };
    let executable = File::open("/proc/self/exe").unwrap();
    let pending = held
        .gate(&executable, &fixture_args("native_fixture"), 10_000)
        .unwrap();
    let cwd = pending._cwd.path().to_owned();
    drop(pending);
    assert!(!held.cleanup_unknown);
    assert!(!cwd.join("executed").exists());
    assert_eq!(populated(&held.read(c"cgroup.events").unwrap()).unwrap(), 0);
    let mut pending = held
        .gate(&executable, &fixture_args("native_fixture"), 10_000)
        .unwrap();
    // Expire the existing bound explicitly: no assumption about scheduler speed,
    // boot uptime, or how much progress a loaded host makes within a sleep.
    pending.deadline = boottime_ms().unwrap();
    assert!(
        pending
            .execute()
            .unwrap_err()
            .to_string()
            .contains("deadline")
    );
    assert!(!held.cleanup_unknown);
    assert_eq!(populated(&held.read(c"cgroup.events").unwrap()).unwrap(), 0);
    held.dispose_test_boundary();
}

#[test]
fn real_gate_rechecks_boundary_and_rejects_scripts_before_clone() {
    let Some(mut held) = isolated_boundary() else {
        return;
    };
    let temp = tempfile::tempfile().unwrap();
    (&temp).write_all(b"#!/bin/false\n").unwrap();
    assert!(
        held.gate(&temp, &fixture_args("native_fixture"), 10_000)
            .is_err()
    );
    assert!(!held.cleanup_unknown);
    open_at(&held.directory, c"pids.max", libc::O_WRONLY)
        .unwrap()
        .write_all(b"15")
        .unwrap();
    let executable = File::open("/proc/self/exe").unwrap();
    assert!(
        held.gate(&executable, &fixture_args("native_fixture"), 10_000)
            .is_err()
    );
    assert!(!held.cleanup_unknown);
    held.dispose_test_boundary();
}
