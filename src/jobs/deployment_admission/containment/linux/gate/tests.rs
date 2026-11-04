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

struct Fixture {
    root: tempfile::TempDir,
    executable: File,
}

impl Fixture {
    fn build() -> Self {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("alpha.c");
        let binary = root.path().join("alpha");
        std::fs::write(&source, include_str!("fixture.c")).unwrap();
        let status = std::process::Command::new("cc")
            .args(["-static", "-O2", "-Wall", "-Wextra", "-Werror", "-o"])
            .arg(&binary)
            .arg(&source)
            .status()
            .expect("static fixture requires a C compiler; no dynamic fallback");
        assert!(status.success(), "static fixture compilation failed");
        let executable = File::open(binary).unwrap();
        elf::validate(&executable).unwrap();
        Self { root, executable }
    }

    fn marker(&self) -> std::path::PathBuf {
        self.root.path().join("executed")
    }

    fn args(&self) -> Vec<CString> {
        vec![
            CString::new("alpha").unwrap(),
            CString::new(self.marker().to_str().unwrap()).unwrap(),
        ]
    }
}

fn child_pidfd(pid: i32) -> File {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(fd >= 0, "test requires pidfd support");
    unsafe { File::from_raw_fd(fd as i32) }
}

// Always rescue/reap before propagating a failed observation/asserting. This is
// real subprocess policy testing, NOT a substitute for cgroup qualification.
fn finish_child(pid: i32, pidfd: &File) -> i32 {
    let result = reap(pid, pidfd, boottime_ms().unwrap() + CLEANUP_MS);
    if result.is_err() {
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                pidfd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
        }
        reap(pid, pidfd, boottime_ms().unwrap() + CLEANUP_MS).unwrap();
    }
    result.unwrap()
}

#[test]
fn native_static_fixture_is_accepted_without_a_loader_exception() {
    let fixture = Fixture::build();
    elf::validate(&fixture.executable).unwrap();
    assert!(!fixture.marker().exists());
}

#[test]
fn setup_child_exit_is_eof_not_the_runtime_deadline() {
    let (mut input, output) = report_pipe().unwrap();
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        // Fail at the first dup3: actual setup exit 120, before acknowledgement.
        unsafe { probe_child(-1, output.as_raw_fd(), -1, 0, std::ptr::null()) }
    }
    drop(output);
    let pidfd = child_pidfd(pid);
    let observed = read_ack(&mut input, &mut [0; 12], boottime_ms().unwrap() + PROBE_MS);
    let status = finish_child(pid, &pidfd);
    assert!(libc::WIFEXITED(status));
    assert_eq!(libc::WEXITSTATUS(status), 120);
    assert!(
        observed
            .unwrap_err()
            .to_string()
            .contains("exited before acknowledgement")
    );
}

#[test]
fn child_resets_all_signals_mask_and_session_before_exec() {
    let (mut input, output) = report_pipe().unwrap();
    let parent_session = unsafe { libc::getsid(0) };
    let parent_group = unsafe { libc::getpgrp() };
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        unsafe {
            // Deliberately poison policy in the disposable child, never in the
            // multithreaded test runner. Include glibc's reserved signals 32/33.
            let ignored = [libc::SIG_IGN as u64, 0, 0, 0];
            for signal in 1..=64 {
                if signal != libc::SIGKILL
                    && signal != libc::SIGSTOP
                    && libc::syscall(
                        libc::SYS_rt_sigaction,
                        signal,
                        ignored.as_ptr(),
                        std::ptr::null_mut::<u64>(),
                        8,
                    ) != 0
                {
                    libc::_exit(90);
                }
            }
            let blocked = u64::MAX;
            if libc::syscall(
                libc::SYS_rt_sigprocmask,
                libc::SIG_SETMASK,
                &blocked,
                std::ptr::null_mut::<u64>(),
                8,
            ) != 0
            {
                libc::_exit(91);
            }
            if !reset_signals_and_session() {
                libc::_exit(92);
            }
            let mut mask = u64::MAX;
            if libc::syscall(
                libc::SYS_rt_sigprocmask,
                libc::SIG_SETMASK,
                std::ptr::null::<u64>(),
                &mut mask,
                8,
            ) != 0
                || mask != 0
            {
                libc::_exit(93);
            }
            for signal in 1..=64 {
                if signal == libc::SIGKILL || signal == libc::SIGSTOP {
                    continue;
                }
                let mut action = [0u64; 4];
                if libc::syscall(
                    libc::SYS_rt_sigaction,
                    signal,
                    std::ptr::null::<u64>(),
                    action.as_mut_ptr(),
                    8,
                ) != 0
                    || action[0] != 0
                {
                    libc::_exit(94);
                }
            }
            if libc::getsid(0) != libc::getpid()
                || libc::getpgrp() != libc::getpid()
                || libc::getsid(0) == parent_session
                || libc::getpgrp() == parent_group
            {
                libc::_exit(95);
            }
            libc::write(output.as_raw_fd(), b"1".as_ptr().cast(), 1);
            libc::_exit(0);
        }
    }
    drop(output);
    let pidfd = child_pidfd(pid);
    let report = read_ack(&mut input, &mut [0], boottime_ms().unwrap() + PROBE_MS);
    let status = finish_child(pid, &pidfd);
    assert!(libc::WIFEXITED(status));
    assert_eq!(libc::WEXITSTATUS(status), 0);
    report.unwrap();
}

#[test]
fn real_gate_blocks_until_release_and_execs_with_fixed_policy() {
    let Some(mut held) = isolated_boundary() else {
        return;
    };
    let fixture = Fixture::build();
    let pending = held
        .gate(&fixture.executable, &fixture.args(), 10_000)
        .unwrap();
    assert!(!fixture.marker().exists());
    assert!(pending.held.cleanup_unknown);
    assert!(pending.held.identity.get("gate_process").is_some());
    let status = pending.execute().unwrap();
    assert!(libc::WIFEXITED(status));
    assert_eq!(libc::WEXITSTATUS(status), 0, "static policy fixture failed");
    assert_eq!(std::fs::read(fixture.marker()).unwrap(), b"executed");
    assert!(!held.cleanup_unknown);
    assert_eq!(populated(&held.read(c"cgroup.events").unwrap()).unwrap(), 0);
    held.dispose_test_boundary();
}

#[test]
fn real_gate_drop_and_deadline_kill_and_empty_without_replay() {
    let Some(mut held) = isolated_boundary() else {
        return;
    };
    let fixture = Fixture::build();
    let pending = held
        .gate(&fixture.executable, &fixture.args(), 10_000)
        .unwrap();
    drop(pending);
    assert!(!held.cleanup_unknown);
    // The marker's lifetime is independent of the gate's deleted private cwd.
    assert!(!fixture.marker().exists());
    let status = held.identity["last_wait_status"].as_i64().unwrap() as i32;
    assert!(libc::WIFSIGNALED(status));
    assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
    assert_eq!(populated(&held.read(c"cgroup.events").unwrap()).unwrap(), 0);
    let mut pending = held
        .gate(&fixture.executable, &fixture.args(), 10_000)
        .unwrap();
    // Expire the existing bound explicitly: no uptime/scheduling assumptions.
    pending.deadline = boottime_ms().unwrap();
    assert!(
        pending
            .execute()
            .unwrap_err()
            .to_string()
            .contains("deadline")
    );
    assert!(!fixture.marker().exists());
    assert!(!held.cleanup_unknown);
    assert_eq!(populated(&held.read(c"cgroup.events").unwrap()).unwrap(), 0);
    held.dispose_test_boundary();
}

#[test]
fn real_gate_rechecks_membership_at_release_after_recording_gap() {
    let Some(mut held) = isolated_boundary() else {
        return;
    };
    let fixture = Fixture::build();
    let pending = held
        .gate(&fixture.executable, &fixture.args(), 10_000)
        .unwrap();
    // Model loss of the gated process during future durable ledger recording.
    pending
        .held
        .kill_empty(boottime_ms().unwrap() + CLEANUP_MS)
        .unwrap();
    assert!(
        pending
            .execute()
            .unwrap_err()
            .to_string()
            .contains("membership changed")
    );
    assert!(!fixture.marker().exists());
    assert!(!held.cleanup_unknown);
    held.dispose_test_boundary();
}

#[test]
fn acknowledgement_deadline_is_not_extended_by_available_data() {
    let (mut input, mut output) = report_pipe().unwrap();
    output.write_all(b"1").unwrap();
    let error = read_ack(&mut input, &mut [0], boottime_ms().unwrap()).unwrap_err();
    assert!(error.to_string().contains("deadline"));
}

#[test]
fn real_gate_rechecks_boundary_and_rejects_scripts_before_clone() {
    let Some(mut held) = isolated_boundary() else {
        return;
    };
    let temp = tempfile::tempfile().unwrap();
    (&temp).write_all(b"#!/bin/false\n").unwrap();
    assert!(
        held.gate(&temp, &[CString::new("alpha").unwrap()], 10_000)
            .is_err()
    );
    assert!(!held.cleanup_unknown);
    open_at(&held.directory, c"pids.max", libc::O_WRONLY)
        .unwrap()
        .write_all(b"15")
        .unwrap();
    let fixture = Fixture::build();
    assert!(
        held.gate(&fixture.executable, &fixture.args(), 10_000)
            .is_err()
    );
    assert!(!held.cleanup_unknown);
    held.dispose_test_boundary();
}
