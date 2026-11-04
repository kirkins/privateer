use super::*;
use std::process::{Command, Stdio};

struct Fixture {
    root: tempfile::TempDir,
    binary: std::path::PathBuf,
}
impl Fixture {
    fn build() -> Self {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("alpha.c");
        let binary = root.path().join("alpha");
        std::fs::write(&source, include_str!("fixture.c")).unwrap();
        assert!(
            Command::new("cc")
                .args(["-static", "-O2", "-Wall", "-Wextra", "-Werror", "-o"])
                .arg(&binary)
                .arg(source)
                .status()
                .unwrap()
                .success()
        );
        Self { root, binary }
    }

    fn input(&self, bytes: &[u8], size: u64, digest: &str) -> Input {
        let path = self.root.path().join("artifact");
        std::fs::write(&path, bytes).unwrap();
        Input::new(
            File::open(path).unwrap(),
            &json!({"schema_version":1,
            "artifact":{"size_bytes":size,"sha256":digest}}),
        )
        .unwrap()
    }

    fn run(&self, mode: &str, input: Input, clock: Option<&str>) -> (ProtocolReport, i32) {
        let (streams, [child_input, child_output, child_error]) = streams().unwrap();
        let child = Command::new(&self.binary)
            .arg(mode)
            .env_clear()
            .current_dir(self.root.path())
            .stdin(Stdio::from(child_input))
            .stdout(Stdio::from(child_output))
            .stderr(Stdio::from(child_error))
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        assert!(fd >= 0);
        let guard = Child {
            pid,
            pidfd: unsafe { File::from_raw_fd(fd as i32) },
            reaped: false,
        };
        let report = match clock {
            // Deliberately unrelated to machine uptime. Each round must check
            // again even when a child continuously emits or never reads stdin.
            Some("deadline") => {
                let mut tick = 0;
                streams.run_with_clock(input, &guard.pidfd, 8, || {
                    tick += 1;
                    Ok(tick)
                })
            }
            Some("unreadable") => {
                streams.run_with_clock(input, &guard.pidfd, u64::MAX, || anyhow::bail!("fixture"))
            }
            _ => streams.run(input, &guard.pidfd, boottime_ms().unwrap() + 30_000),
        };
        // Kill only our own fixture. This direct-child reap is NOT a cgroup
        // cleanup proof and does not set cleanup_proven_empty in the report.
        if report.protocol_success || matches!(report.reason, "adapter_failure" | "invalid_result")
        {
            (report, guard.reap())
        } else {
            let status = guard.finish();
            (report, status)
        }
    }
}

struct Child {
    pid: i32,
    pidfd: File,
    reaped: bool,
}
impl Child {
    fn kill(&self) {
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
        }
    }
    fn reap(mut self) -> i32 {
        let status = reap(self.pid, &self.pidfd, boottime_ms().unwrap() + CLEANUP_MS).unwrap();
        self.reaped = true;
        status
    }
    fn finish(self) -> i32 {
        self.kill();
        self.reap()
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        if !self.reaped {
            self.kill();
            let _ = reap(
                self.pid,
                &self.pidfd,
                boottime_ms().unwrap_or(0).saturating_add(CLEANUP_MS),
            );
        }
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[test]
fn real_subprocess_protocol_frames_and_hashes_under_backpressure() {
    let fixture = Fixture::build();
    // Larger than the socket buffer: exercises partial nonblocking delivery.
    let bytes = vec![b'a'; 2 * 1024 * 1024];
    for mode in [
        "success",
        "fragmented",
        "failure",
        "malformed",
        "extra",
        "truncated",
        "empty",
        "oversize-result",
        "nonzero",
    ] {
        let input = fixture.input(&bytes, bytes.len() as u64, &digest(&bytes));
        let (report, status) = fixture.run(mode, input, None);
        assert!(libc::WIFEXITED(status), "{mode}: {report:?}");
        assert_eq!(
            libc::WEXITSTATUS(status),
            if mode == "nonzero" { 42 } else { 0 }
        );
        assert_eq!(
            report.protocol_success,
            matches!(mode, "success" | "fragmented" | "nonzero"),
            "{mode}: {report:?}"
        );
        assert_eq!(report.artifact_bytes, bytes.len() as u64);
        assert_eq!(report.artifact_sha256, digest(&bytes));
        assert!(
            !report.cleanup_proven_empty,
            "ordinary fixtures do not prove containment"
        );
        if matches!(mode, "success" | "fragmented") {
            assert_eq!(report.stderr_sha256, digest(b"beta"));
            assert_eq!(report.stderr_bytes, 4);
            let body = br#"{"schema_version":1,"status":"success"}"#;
            let mut framed = (body.len() as u32).to_be_bytes().to_vec();
            framed.extend_from_slice(body);
            assert_eq!(report.stdout_sha256, digest(&framed));
            assert_eq!(report.stdout_bytes, framed.len() as u64);
        }
        let serialized = serde_json::to_string(&report).unwrap();
        assert!(!serialized.contains("beta"));
        assert!(!serialized.contains("secret"));
        assert!(serialized.contains("outside_ipc_delegation_not_excluded"));
        assert!(report.capacity_retained);
    }
}

#[test]
fn short_extra_changed_and_replaced_sources_never_substitute_a_path() {
    let fixture = Fixture::build();
    let expected = b"aaaaaaaa";
    for (bytes, reason) in [
        (b"aaaaa".as_slice(), "short_artifact"),
        (b"aaaaaaaaa".as_slice(), "extra_artifact"),
        (b"aaaaaaab".as_slice(), "artifact_mismatch"),
    ] {
        let input = fixture.input(bytes, expected.len() as u64, &digest(expected));
        let (report, _) = fixture.run("success", input, None);
        assert_eq!(report.reason, reason, "{report:?}");
        assert!(!report.protocol_success);
        assert_eq!(
            report.artifact_sha256,
            digest(&bytes[..report.artifact_bytes as usize])
        );
    }
    let input = fixture.input(expected, expected.len() as u64, &digest(expected));
    let path = fixture.root.path().join("artifact");
    std::fs::rename(&path, fixture.root.path().join("held")).unwrap();
    std::fs::write(path, b"bbbbbbbb").unwrap();
    let (report, _) = fixture.run("success", input, None);
    assert!(report.protocol_success, "{report:?}");
    assert_eq!(report.artifact_sha256, digest(expected));

    let input = fixture.input(expected, expected.len() as u64, &digest(expected));
    std::fs::write(fixture.root.path().join("artifact"), b"aaaaaaab").unwrap();
    let (report, _) = fixture.run("success", input, None);
    assert_eq!(report.reason, "artifact_mismatch");
}

#[test]
fn closed_input_output_flood_and_deadlines_are_bounded_and_never_success() {
    let fixture = Fixture::build();
    let bytes = vec![b'a'; 2 * 1024 * 1024];
    for (mode, clock, reason) in [
        ("closed-input", None, "input_delivery_failed"),
        ("early-success", None, "input_delivery_failed"),
        ("output-limit", None, "output_limit"),
        ("continuous", Some("deadline"), "hard_deadline"),
        ("hang", Some("deadline"), "hard_deadline"),
        ("hang", Some("unreadable"), "clock_unreadable"),
    ] {
        let input = fixture.input(&bytes, bytes.len() as u64, &digest(&bytes));
        let (report, _) = fixture.run(mode, input, clock);
        assert!(!report.protocol_success, "{mode}: {report:?}");
        if matches!(mode, "closed-input" | "early-success") {
            assert!(
                matches!(report.reason, "input_delivery_failed" | "partial_delivery"),
                "{mode}: {report:?}"
            );
        } else {
            assert_eq!(report.reason, reason, "{mode}: {report:?}");
        }
        assert!(report.artifact_bytes < bytes.len() as u64);
        assert_eq!(
            report.artifact_sha256,
            digest(&bytes[..report.artifact_bytes as usize])
        );
        assert!(report.stdout_bytes + report.stderr_bytes <= OUTPUT_MAX + 1);
        assert!(report.capacity_retained);
    }
}

#[test]
fn report_success_requires_zero_exit_and_cleanup_proof_but_never_releases_capacity() {
    let fixture = Fixture::build();
    for mode in ["success", "nonzero", "failure"] {
        let input = fixture.input(b"aaaa", 4, &digest(b"aaaa"));
        let (mut report, status) = fixture.run(mode, input, None);
        // Inject the cleanup result for report-policy testing only. The
        // ordinary child reap above is NOT positive cgroup qualification.
        report.finish_cleanup(Ok(status));
        assert_eq!(report.protocol_success, mode == "success");
        assert!(report.cleanup_proven_empty);
        assert!(report.capacity_retained);
        report.finish_cleanup(Err(anyhow::anyhow!("unproven fixture cleanup")));
        assert!(!report.protocol_success);
        assert_eq!(report.reason, "cleanup_unproven");
        assert!(report.capacity_retained);
    }
}

#[test]
fn input_and_result_validation_are_strict_and_bounded() {
    let file = tempfile::tempfile().unwrap();
    for metadata in [
        json!({}),
        json!({"artifact":{"size_bytes":ARTIFACT_MAX+1,"sha256":"a".repeat(64)}}),
        json!({"artifact":{"size_bytes":0,"sha256":"B".repeat(64)}}),
        json!({"artifact":{"size_bytes":0,"sha256":"a".repeat(64)},"extra":"x".repeat(METADATA_MAX)}),
    ] {
        assert!(Input::new(file.try_clone().unwrap(), &metadata).is_err());
    }
    for body in [
        r#"{"schema_version":2,"status":"success"}"#,
        r#"{"schema_version":1,"status":"unknown"}"#,
        r#"{"schema_version":1,"status":"success","status":"failure"}"#,
        r#"{"schema_version":1,"status":"success","config":"beta"}"#,
        "not json",
    ] {
        let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(body.as_bytes());
        assert_eq!(result(&bytes), Err("invalid_result"));
    }
    assert_eq!(result(&[]), Err("invalid_result"));
    assert_eq!(result(&[0, 0, 0, 0]), Err("invalid_result"));
}
