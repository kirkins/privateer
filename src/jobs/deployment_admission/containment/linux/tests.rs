use super::*;

#[test]
fn only_a_successful_open_and_write_enoent_qualifies() {
    let report = |root: i32, opened: i32, wrote: i32| {
        let mut report = [0; 12];
        report[..4].copy_from_slice(&root.to_ne_bytes());
        report[4..8].copy_from_slice(&opened.to_ne_bytes());
        report[8..].copy_from_slice(&wrote.to_ne_bytes());
        report
    };
    validate_report(&report(1, 0, libc::ENOENT)).unwrap();
    for (root, opened, wrote) in [
        (1, libc::ENOENT, libc::ENOENT),
        (1, libc::ENOENT, 0),
        (1, libc::EACCES, libc::ENOENT),
        (1, 0, libc::EACCES),
        (1, 0, 0),
        (0, 0, libc::ENOENT),
    ] {
        assert!(validate_report(&report(root, opened, wrote)).is_err());
    }
}

// Real subprocess test of rescue/reaping, NOT a containment proof. Fake cgroup
// control files are used only to observe an unconditional kill attempt.
#[test]
fn clock_read_failure_still_attempts_cgroup_kill_and_pidfd_kill_and_reaps() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("cgroup.kill"), "").unwrap();
    std::fs::write(temp.path().join("cgroup.events"), "populated 0\n").unwrap();
    let held = Qualified {
        directory: File::open(temp.path()).unwrap(),
        parent: File::open(temp.path()).unwrap(),
        name: CString::new("alpha").unwrap(),
        identity: json!({}),
        cleanup_unknown: true,
    };
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        unsafe {
            loop {
                libc::pause();
            }
        }
    }
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(fd >= 0, "test requires pidfd support");
    let pidfd = unsafe { File::from_raw_fd(fd as i32) };
    let error = cleanup_child(&held, pid, &pidfd, || {
        anyhow::bail!("injected clock read failure")
    })
    .unwrap_err();
    assert!(error.to_string().contains("injected clock"));
    assert_eq!(
        std::fs::read(temp.path().join("cgroup.kill")).unwrap(),
        b"1"
    );
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    assert!(
        held.cleanup_unknown,
        "clock failure cannot discharge custody"
    );
}

#[test]
fn actual_child_reports_missing_ancestor_file_as_open_error() {
    let temp = tempfile::tempdir().unwrap();
    let ancestor = duplicate(File::open(temp.path()).unwrap().as_raw_fd()).unwrap();
    let work = duplicate(File::open(temp.path()).unwrap().as_raw_fd()).unwrap();
    let mut pipes = [-1; 2];
    assert_eq!(
        unsafe { libc::pipe2(pipes.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) },
        0
    );
    let mut input = unsafe { File::from_raw_fd(pipes[0]) };
    let original = unsafe { File::from_raw_fd(pipes[1]) };
    let output = duplicate(original.as_raw_fd()).unwrap();
    drop(original);
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        unsafe {
            probe_child(
                ancestor.as_raw_fd(),
                output.as_raw_fd(),
                work.as_raw_fd(),
                0,
                std::ptr::null(),
            )
        }
    }
    drop(output);
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(fd >= 0);
    let pidfd = unsafe { File::from_raw_fd(fd as i32) };
    let deadline = boottime_ms().unwrap() + PROBE_MS;
    // Collect the report without any assertion that could skip child cleanup.
    let report = (|| -> Result<[u8; 12]> {
        let mut report = [0; 12];
        let mut got = 0;
        while got < report.len() {
            ensure!(boottime_ms()? < deadline, "test child report deadline");
            match input.read(&mut report[got..]) {
                Ok(0) => anyhow::bail!("test child exited before report"),
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => unsafe {
                    libc::poll(std::ptr::null_mut(), 0, 5);
                },
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(report)
    })();
    assert_eq!(
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                pidfd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        },
        0
    );
    reap(pid, &pidfd, boottime_ms().unwrap() + CLEANUP_MS).unwrap();
    let report = report.unwrap();
    assert_eq!(
        i32::from_ne_bytes(report[4..8].try_into().unwrap()),
        libc::ENOENT
    );
    assert_eq!(i32::from_ne_bytes(report[8..].try_into().unwrap()), 0);
    assert!(validate_report(&report).is_err());
}
