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

fn run(mut command: Command, lock: RawFd) -> Result<Vec<u8>> {
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
        loop {
            ensure!(
                Instant::now() < deadline,
                "workspace helper timed out; the recorded lease is retained for recovery"
            );
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
                        bytes += n;
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
                }
            }
            // A failed leader can exit before all its stderr was read. Drain
            // both streams fairly, under the same cumulative bound/deadline,
            // before classifying. Successful stderr never becomes output.
            if ended.iter().all(|v| *v)
                && let Some(success) = success
            {
                return Ok((success, output, diagnostics));
            }
            thread::sleep(Duration::from_millis(5));
        }
    })();
    // Signal before reaping: our child still reserves its PID/group identity.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
    let waited = child.wait();
    let (success, output, diagnostics) = result?;
    let status = waited?;
    ensure!(
        success,
        "workspace helper refused the operation ({status}): {}",
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
