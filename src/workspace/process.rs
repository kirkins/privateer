//! Bounded argv-only helpers. The lock is inherited by the helper so a killed
//! caller cannot race an allocation that is still finishing in its child.
use anyhow::{Context, Result, ensure};
use std::{
    io::Read,
    os::{fd::RawFd, unix::process::CommandExt},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const LIMIT: usize = 1_048_576;
const FAILURE_DRAIN: Duration = Duration::from_millis(100);

/// Labels come from the runtime's operation, never argv, paths or remote URLs.
pub(super) enum Operation {
    Git,
    GitFetch,
    GitDefaultBranch,
    Treehouse,
    TreehouseStatus,
    TreehouseAllocate,
    TreehouseReturn,
}

impl Operation {
    pub(super) fn git(args: &[&str]) -> Self {
        match args.first().copied() {
            Some("fetch") => Self::GitFetch,
            Some("ls-remote") => Self::GitDefaultBranch,
            _ => Self::Git,
        }
    }

    pub(super) fn treehouse(args: &[&str]) -> Self {
        match args.first().copied() {
            Some("status") => Self::TreehouseStatus,
            Some("get") => Self::TreehouseAllocate,
            Some("return") => Self::TreehouseReturn,
            _ => Self::Treehouse,
        }
    }

    pub(super) fn run(self, command: Command, lock: RawFd) -> Result<Vec<u8>> {
        let label = match self {
            Self::Git => "Git workspace operation failed",
            Self::GitFetch => "Git fetch from origin failed",
            Self::GitDefaultBranch => "Git origin default-branch lookup failed",
            Self::Treehouse => "Treehouse workspace operation failed",
            Self::TreehouseStatus => "Treehouse lease status failed",
            Self::TreehouseAllocate => "Treehouse workspace allocation failed (may fetch origin)",
            Self::TreehouseReturn => "Treehouse workspace return failed",
        };
        // CLI/RPC surfaces use Display, not the anyhow chain. Flatten only
        // this runner's already-sanitized errors so the useful detail survives.
        run(command, lock).map_err(|error| anyhow::anyhow!("{label}: {error:#}"))
    }
}

/// Stderr is untrusted and can contain credentials in arbitrary forms. Only a
/// fixed advisory category leaves this module; no redaction claims or excerpts.
fn diagnostic(stderr: &[u8]) -> &'static str {
    let lower = stderr.to_ascii_lowercase();
    let contains = |signatures: &[&[u8]]| {
        signatures
            .iter()
            .any(|signature| lower.windows(signature.len()).any(|w| w == *signature))
    };
    if contains(&[
        b"authentication failed",
        b"permission denied (publickey",
        b"could not read username",
        b"could not read password",
        b"terminal prompts disabled",
        b"http basic: access denied",
    ]) {
        "authentication refused; check the configured Git credentials and repository access before retrying; diagnostics withheld"
    } else if contains(&[
        b"could not resolve host",
        b"could not resolve hostname",
        b"temporary failure in name resolution",
        b"network is unreachable",
        b"failed to connect",
        b"connection refused",
        b"connection timed out",
        b"connection reset by peer",
    ]) {
        "network unavailable; check connectivity to the configured origin before retrying; diagnostics withheld"
    } else {
        "failure category unknown; diagnostics withheld; inspect the configured helper locally before retrying"
    }
}

enum DrainEnd {
    Eof,
    OpenPipes,
    OutputLimit,
}

fn run(command: Command, lock: RawFd) -> Result<Vec<u8>> {
    run_with_failure_grace(command, lock, FAILURE_DRAIN)
}

fn run_with_failure_grace(
    mut command: Command,
    lock: RawFd,
    failure_grace: Duration,
) -> Result<Vec<u8>> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.process_group(0);
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(lock, libc::F_GETFD);
            if flags < 0 || libc::fcntl(lock, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().context("cannot start workspace helper")?;
    let pid = child.id();
    let result = (|| -> Result<_> {
        use std::os::fd::AsRawFd;
        let mut stdout = child.stdout.take().context("helper stdout missing")?;
        let mut stderr = child.stderr.take().context("helper stderr missing")?;
        for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            ensure!(
                flags >= 0
                    && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
                "cannot bound workspace helper output"
            );
        }
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut bytes = 0;
        let mut output = vec![];
        let mut diagnostics = vec![];
        let mut ended = [false; 2];
        let mut success = None;
        let mut failure_drain_until = None;
        loop {
            let now = Instant::now();
            if success == Some(false) && now >= deadline {
                // An escaped pipe holder need not exit with the helper's
                // process group. Preserve the observed failure after a short
                // drain, without claiming that every descendant stopped.
                return Ok((false, output, diagnostics, DrainEnd::OpenPipes));
            }
            ensure!(
                now < deadline,
                "workspace helper timed out; the recorded lease is retained for recovery"
            );
            let mut progress = false;
            for (index, pipe) in [&mut stdout as &mut dyn Read, &mut stderr]
                .into_iter()
                .enumerate()
            {
                if ended[index] {
                    continue;
                }
                let mut buf = [0; 8192];
                match pipe.read(&mut buf) {
                    Ok(0) => ended[index] = true,
                    Ok(n) => {
                        progress = true;
                        bytes += n;
                        if bytes > LIMIT && success == Some(false) {
                            return Ok((false, output, diagnostics, DrainEnd::OutputLimit));
                        }
                        ensure!(bytes <= LIMIT, "workspace helper output exceeded 1 MiB");
                        if index == 0 {
                            output.extend_from_slice(&buf[..n]);
                        } else {
                            diagnostics.extend_from_slice(&buf[..n]);
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            if success.is_none() {
                success = crate::jobs::exited_unreaped(pid)?;
                if success == Some(false) {
                    // Stop pipe-holding descendants promptly, while our
                    // unreaped child still reserves this group identity.
                    unsafe {
                        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                    }
                    failure_drain_until = Some(Instant::now() + failure_grace);
                }
            }
            // A failed leader can exit before all its stderr was read. Drain
            // both streams fairly under the cumulative bound and failure
            // budget before classifying. Successful stderr never becomes output.
            if ended.iter().all(|v| *v)
                && let Some(success) = success
            {
                return Ok((success, output, diagnostics, DrainEnd::Eof));
            }
            if success == Some(false) {
                // A scheduler pause must not discard ready diagnostic bytes.
                // Drain without sleeping while reads progress; even a writer
                // that escaped our group cannot exceed the combined byte cap.
                if progress {
                    continue;
                }
                if failure_drain_until.is_some_and(|until| Instant::now() >= until) {
                    return Ok((false, output, diagnostics, DrainEnd::OpenPipes));
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
    })();
    // Signal before reaping: our child still reserves its PID/group identity.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
    let waited = child.wait();
    let (success, output, diagnostics, drain_end) = result?;
    let status = waited?;
    let incomplete = match drain_end {
        DrainEnd::Eof => "",
        DrainEnd::OpenPipes => {
            "; diagnostics incomplete after bounded failure drain; descendants may still be running"
        }
        DrainEnd::OutputLimit => {
            "; workspace helper output exceeded 1 MiB; diagnostics incomplete; descendants may still be running"
        }
    };
    ensure!(
        success,
        "workspace helper refused the operation ({status}): {}{incomplete}",
        diagnostic(&diagnostics)
    );
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::fd::AsRawFd, path::Path};

    fn shell(script: &str, argument: &Path) -> Result<Vec<u8>> {
        let lock = tempfile::tempfile().unwrap();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script, "workspace-diagnostic-fixture"]);
        command.arg(argument);
        run(command, lock.as_raw_fd())
    }

    fn assert_withheld(error: &str) {
        for canary in [
            "diagnostic-host-canary",
            "stdout-token-canary",
            "unknown-secret-canary",
            "workspace-password-canary",
            "git@example.invalid",
            "private-path-canary",
            "private-argv-canary",
            "drain-no-echo-canary",
            "Authorization:",
            "\u{1b}",
            "\u{fffd}",
        ] {
            assert!(
                !error.contains(canary),
                "diagnostic leaked {canary:?}: {error}"
            );
        }
    }

    fn assert_stopped(pid: &str) {
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            let output = Command::new("ps")
                .args(["-p", pid.trim(), "-o", "stat="])
                .output()
                .unwrap();
            let state = String::from_utf8(output.stdout).unwrap();
            if state.trim().is_empty() || state.trim().starts_with('Z') {
                return;
            }
            assert!(Instant::now() < until, "helper descendant survived: {pid}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// The escaped fixture responds to a private stop file and self-expires.
    /// Cleanup never signals a PID, and verifies its recorded start identity
    /// disappeared before assertions can leave this test.
    struct EscapedPipeFixture(std::path::PathBuf);

    impl EscapedPipeFixture {
        fn stop(&self) -> bool {
            if fs::write(self.0.join("stop"), "").is_err() {
                return false;
            }
            let pid = fs::read_to_string(self.0.join("ready"))
                .ok()
                .and_then(|text| text.split_whitespace().next()?.parse::<u32>().ok());
            let Some(pid) = pid else {
                return true;
            };
            let Ok(identity) = crate::jobs::process_identity(pid) else {
                return false;
            };
            let until = Instant::now() + Duration::from_secs(30);
            loop {
                let Ok(current) = crate::jobs::process_identity(pid) else {
                    return false;
                };
                if self.0.join("exited").exists()
                    && identity
                        .as_ref()
                        .is_none_or(|identity| current.as_ref() != Some(identity))
                {
                    return true;
                }
                if Instant::now() >= until {
                    return false;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Drop for EscapedPipeFixture {
        fn drop(&mut self) {
            let _ = self.stop();
        }
    }

    fn escaped_pipe_failure(flood: bool) {
        let temp = tempfile::tempdir().unwrap();
        let escaped = EscapedPipeFixture(temp.path().to_owned());
        let lock = tempfile::tempfile().unwrap();
        let mut command = Command::new("python3");
        command.args([
            "-c",
            r#"
import os
from pathlib import Path
import sys
import time

root = Path(sys.argv[1])
control = os.pipe() if sys.argv[2] == "flood" else None
if control is not None:
    sentinel = os.fork()
    if sentinel == 0:
        os.close(control[0])
        os.close(0)
        os.close(1)
        os.close(2)
        time.sleep(20)
        os._exit(0)
child = os.fork()
if child == 0:
    if control is not None:
        os.close(control[1])
    os.setsid()
    os.close(0)
    os.close(1)
    ready = root / "ready"
    temporary = root / "ready.tmp"
    temporary.write_text(f"{os.getpid()} {os.getpgrp()} {os.getsid(0)}")
    temporary.replace(ready)
    until = time.monotonic() + 20
    if control is not None:
        # Only the in-group sentinel holds the write end. EOF proves the
        # runtime observed failure and killed that reserved group first.
        os.read(control[0], 1)
        os.close(control[0])
        while root.exists() and not (root / "stop").exists() and time.monotonic() < until:
            try:
                os.write(2, b"drain-no-echo-canary " * 512)
            except BrokenPipeError:
                break
    while root.exists() and not (root / "stop").exists() and time.monotonic() < until:
        time.sleep(.01)
    if root.exists():
        (root / "exited").write_text("escaped fixture stopped")
    os._exit(0)
if control is not None:
    os.close(control[0])
    os.close(control[1])
until = time.monotonic() + 60
while not (root / "ready").exists():
    if time.monotonic() >= until:
        os._exit(91)
    time.sleep(.005)
sys.stderr.write("Could not resolve host: diagnostic-host-canary\n")
sys.stderr.flush()
os._exit(17)
"#,
        ]);
        command
            .arg(temp.path())
            .arg(if flood { "flood" } else { "hold" });
        let started = Instant::now();
        let result = if flood {
            // The control-pipe EOF proves failure observation. Give the
            // independently scheduled writer ample time to reach the cap.
            run_with_failure_grace(command, lock.as_raw_fd(), Duration::from_secs(60))
        } else {
            run(command, lock.as_raw_fd())
        };
        let elapsed = started.elapsed();
        let ready = fs::read_to_string(temp.path().join("ready")).unwrap();
        let ids: Vec<u32> = ready
            .split_whitespace()
            .map(|value| value.parse().unwrap())
            .collect();
        let still_live = !temp.path().join("exited").exists()
            && crate::jobs::process_identity(ids[0]).unwrap().is_some();
        let cleaned = escaped.stop();
        assert!(cleaned, "escaped fixture cleanup was not verified");
        assert_eq!(
            ids,
            vec![ids[0]; 3],
            "fixture did not escape into its own session"
        );
        assert!(
            still_live,
            "the runner waited for the escaped pipe holder to exit ({elapsed:?})"
        );
        let error = result.unwrap_err().to_string();
        assert!(error.contains("exit status: 17"), "{error}");
        assert!(error.contains("network unavailable"), "{error}");
        assert!(!error.contains("timed out"), "{error}");
        assert!(error.contains("diagnostics incomplete"), "{error}");
        assert!(
            error.contains("descendants may still be running"),
            "{error}"
        );
        if flood {
            assert!(error.contains("output exceeded 1 MiB"), "{error}");
        }
        assert_withheld(&error);
    }

    #[test]
    fn workspace_helper_diagnostics_bound_failed_leader_drain_with_an_escaped_pipe_holder() {
        escaped_pipe_failure(false);
    }

    #[test]
    fn workspace_helper_diagnostics_retain_failure_when_an_escaped_writer_fills_the_drain_cap() {
        escaped_pipe_failure(true);
    }

    #[test]
    fn workspace_helper_diagnostics_survive_the_operation_display_boundary() {
        let lock = tempfile::tempfile().unwrap();
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "printf 'Could not resolve host: diagnostic-host-canary\\n' >&2; exit 17",
        ]);
        let error = Operation::TreehouseAllocate
            .run(command, lock.as_raw_fd())
            .unwrap_err()
            .to_string();
        assert!(error.contains("Treehouse workspace allocation"), "{error}");
        assert!(error.contains("may fetch origin"), "{error}");
        assert!(error.contains("network unavailable"), "{error}");
        assert!(error.contains("exit status: 17"), "{error}");
        assert_withheld(&error);
    }

    #[test]
    fn workspace_helper_diagnostics_drain_late_stderr_and_stop_descendants() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("child");
        let started = Instant::now();
        let error = shell(
            "sleep 20 & echo $! > \"$1\"; \
             head -c 24576 /dev/zero | tr '\\000' x >&2; \
             printf '\\377\\033[31m\\nCould not resolve ' >&2; \
             printf 'host: diagnostic-host-canary\\n' >&2; \
             printf stdout-token-canary; exit 17",
            &pid_file,
        )
        .unwrap_err()
        .to_string();
        assert!(started.elapsed() < Duration::from_secs(8));
        assert_stopped(&fs::read_to_string(pid_file).unwrap());
        assert!(error.contains("network unavailable"), "{error}");
        assert!(error.contains("exit status: 17"), "{error}");
        assert_withheld(&error);
    }

    #[test]
    fn workspace_helper_diagnostics_unknown_and_signal_failures_withhold_output() {
        let temp = tempfile::tempdir().unwrap();
        for (script, summary) in [
            (
                "printf 'unknown-secret-canary private-path-canary private-argv-canary\\377\\033[31m' >&2; \
                 printf stdout-token-canary; exit 23",
                "exit status: 23",
            ),
            (
                "printf unknown-secret-canary >&2; kill -TERM $$",
                "signal: 15",
            ),
        ] {
            let error = shell(script, temp.path()).unwrap_err().to_string();
            assert!(error.contains(summary), "{error}");
            assert!(error.contains("diagnostics withheld"), "{error}");
            assert_withheld(&error);
        }
    }

    #[test]
    fn workspace_helper_diagnostics_authentication_precedes_permission_and_ignores_success_stderr()
    {
        let temp = tempfile::tempdir().unwrap();
        let error = shell(
            "printf 'git@example.invalid: Permission denied (publickey).\\n' >&2; \
             printf 'Authorization: workspace-password-canary\\n' >&2; exit 19",
            temp.path(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("authentication refused"), "{error}");
        assert!(error.contains("exit status: 19"), "{error}");
        assert_withheld(&error);
        assert_eq!(
            shell(
                "printf success; printf 'Authentication failed: unknown-secret-canary' >&2",
                temp.path(),
            )
            .unwrap(),
            b"success"
        );
    }
}
