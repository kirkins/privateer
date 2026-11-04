//! Bounded protocol machinery, not terminal ledger reconciliation or release.
use super::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::os::unix::fs::FileExt;

const CHUNK: usize = 65_536;
const METADATA_MAX: usize = 16_384;
const RESULT_MAX: usize = 4096;
const OUTPUT_MAX: u64 = 1024 * 1024;
const ARTIFACT_MAX: u64 = 512 * 1024 * 1024;

/// Only fixed reasons/counts/digests leave this module; never raw adapter text.
/// Even success retains capacity and describes no remote deployment outcome.
#[derive(Debug, serde::Serialize)]
pub(crate) struct ProtocolReport {
    pub(crate) protocol_success: bool,
    pub(crate) reason: &'static str,
    pub(crate) artifact_bytes: u64,
    pub(crate) artifact_sha256: String,
    pub(crate) stdout_bytes: u64,
    pub(crate) stdout_sha256: String,
    pub(crate) stderr_bytes: u64,
    pub(crate) stderr_sha256: String,
    pub(crate) wait_status: Option<i32>,
    pub(crate) cleanup_proven_empty: bool,
    capacity_retained: bool,
    containment_scope: &'static str,
    limitation: &'static str,
}

pub(super) struct Input {
    file: File,
    frame: Vec<u8>,
    size: u64,
    digest: String,
}

impl Input {
    pub(super) fn new(file: File, metadata: &Value) -> Result<Self> {
        let body = serde_json::to_vec(metadata)?;
        ensure!(body.len() <= METADATA_MAX, "adapter metadata exceeds bound");
        let artifact = &metadata["artifact"];
        let size = artifact["size_bytes"]
            .as_u64()
            .context("artifact size missing")?;
        let digest = artifact["sha256"]
            .as_str()
            .context("artifact digest missing")?;
        ensure!(size <= ARTIFACT_MAX, "artifact exceeds stream bound");
        ensure!(
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid artifact digest"
        );
        let mut frame = (body.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(&body);
        Ok(Self {
            file,
            frame,
            size,
            digest: digest.to_owned(),
        })
    }
}

pub(super) struct Streams {
    input: File,
    output: File,
    error: File,
}

pub(super) fn streams() -> Result<(Streams, [File; 3])> {
    let mut fds = [-1; 2];
    ensure!(
        unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        } == 0,
        "adapter input stream unavailable"
    );
    let (parent, child) = unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) };
    ensure!(
        unsafe { libc::shutdown(parent.as_raw_fd(), libc::SHUT_RD) } == 0
            && unsafe { libc::shutdown(child.as_raw_fd(), libc::SHUT_WR) } == 0,
        "adapter input stream setup failed"
    );
    let (output, child_output) = pipe()?;
    let (error, child_error) = pipe()?;
    // Parent IO is nonblocking. Child stdio must have ordinary blocking
    // semantics; no adapter-specific EAGAIN handling is required by protocol.
    for file in [&child, &child_output, &child_error] {
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        ensure!(
            flags >= 0
                && unsafe {
                    libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK)
                } == 0,
            "adapter child stream setup failed"
        );
    }
    Ok((
        Streams {
            input: parent,
            output,
            error,
        },
        [child, child_output, child_error],
    ))
}

#[derive(Default)]
struct Observed {
    artifact: Sha256,
    artifact_bytes: u64,
    stdout: Sha256,
    stdout_bytes: u64,
    stderr: Sha256,
    stderr_bytes: u64,
    result: Vec<u8>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterResult {
    schema_version: u32,
    status: String,
}

fn result(bytes: &[u8]) -> std::result::Result<(), &'static str> {
    if bytes.len() < 4 {
        return Err("invalid_result");
    }
    let size = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    if size == 0 || size > RESULT_MAX || bytes.len() != 4 + size {
        return Err("invalid_result");
    }
    let reply: AdapterResult = serde_json::from_slice(&bytes[4..]).map_err(|_| "invalid_result")?;
    match (reply.schema_version, reply.status.as_str()) {
        (1, "success") => Ok(()),
        (1, "failure") => Err("adapter_failure"),
        _ => Err("invalid_result"),
    }
}

fn retry(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
    )
}

impl Streams {
    pub(super) fn run(self, input: Input, pidfd: &File, deadline: u64) -> ProtocolReport {
        self.run_with_clock(input, pidfd, deadline, boottime_ms)
    }

    fn run_with_clock(
        mut self,
        input: Input,
        pidfd: &File,
        deadline: u64,
        clock: impl FnMut() -> Result<u64>,
    ) -> ProtocolReport {
        let mut seen = Observed::default();
        let outcome = self.drive(input, pidfd, deadline, &mut seen, clock);
        ProtocolReport {
            protocol_success: outcome.is_ok(),
            reason: outcome.err().unwrap_or("protocol_complete"),
            artifact_bytes: seen.artifact_bytes,
            artifact_sha256: format!("{:x}", seen.artifact.finalize()),
            stdout_bytes: seen.stdout_bytes,
            stdout_sha256: format!("{:x}", seen.stdout.finalize()),
            stderr_bytes: seen.stderr_bytes,
            stderr_sha256: format!("{:x}", seen.stderr.finalize()),
            wait_status: None,
            cleanup_proven_empty: false,
            capacity_retained: true,
            containment_scope: "attempt_cgroup_only",
            limitation: "outside_ipc_delegation_not_excluded",
        }
    }

    fn drive(
        &mut self,
        input: Input,
        pidfd: &File,
        deadline: u64,
        seen: &mut Observed,
        mut clock: impl FnMut() -> Result<u64>,
    ) -> std::result::Result<(), &'static str> {
        let (mut metadata_sent, mut start, mut end) = (0, 0, 0);
        let (mut input_done, mut stdout_eof, mut stderr_eof) = (false, false, false);
        let mut artifact = [0; CHUNK];
        let mut output = [0; CHUNK];
        loop {
            if clock().map_err(|_| "clock_unreadable")? >= deadline {
                return Err("hard_deadline");
            }
            // At most one nonblocking send and one read per output per round.
            // Partial sends hash only the artifact bytes accepted by the fd.
            if !input_done {
                let metadata = metadata_sent < input.frame.len();
                if !metadata && start == end && seen.artifact_bytes < input.size {
                    let count = (input.size - seen.artifact_bytes).min(CHUNK as u64) as usize;
                    end = match input
                        .file
                        .read_at(&mut artifact[..count], seen.artifact_bytes)
                    {
                        Ok(0) => return Err("short_artifact"),
                        Ok(n) => n,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => return Err("artifact_read_failed"),
                    };
                    start = 0;
                }
                let bytes = if metadata {
                    &input.frame[metadata_sent..]
                } else {
                    &artifact[start..end]
                };
                if !bytes.is_empty() {
                    let n = unsafe {
                        libc::send(
                            self.input.as_raw_fd(),
                            bytes.as_ptr().cast(),
                            bytes.len(),
                            libc::MSG_NOSIGNAL,
                        )
                    };
                    if n > 0 {
                        let n = n as usize;
                        if metadata {
                            metadata_sent += n;
                        } else {
                            seen.artifact.update(&bytes[..n]);
                            seen.artifact_bytes += n as u64;
                            start += n;
                        }
                    } else if n == 0 || !retry(&std::io::Error::last_os_error()) {
                        return Err("input_delivery_failed");
                    }
                } else {
                    let mut extra = [0];
                    match input.file.read_at(&mut extra, input.size) {
                        Ok(0) => {}
                        Ok(_) => return Err("extra_artifact"),
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => return Err("artifact_read_failed"),
                    }
                    if seen.artifact_bytes != input.size
                        || format!("{:x}", seen.artifact.clone().finalize()) != input.digest
                    {
                        return Err("artifact_mismatch");
                    }
                    if unsafe { libc::shutdown(self.input.as_raw_fd(), libc::SHUT_WR) } != 0 {
                        return Err("input_delivery_failed");
                    }
                    input_done = true;
                }
            }
            for (file, eof, stdout) in [
                (&mut self.output, &mut stdout_eof, true),
                (&mut self.error, &mut stderr_eof, false),
            ] {
                if *eof {
                    continue;
                }
                // Observe at most one byte beyond the combined bound.
                let count = (OUTPUT_MAX + 1 - seen.stdout_bytes - seen.stderr_bytes)
                    .min(CHUNK as u64) as usize;
                match file.read(&mut output[..count]) {
                    Ok(0) => *eof = true,
                    Ok(n) => {
                        if stdout {
                            seen.stdout.update(&output[..n]);
                            seen.stdout_bytes += n as u64;
                        } else {
                            seen.stderr.update(&output[..n]);
                            seen.stderr_bytes += n as u64;
                        }
                        if seen.stdout_bytes + seen.stderr_bytes > OUTPUT_MAX {
                            return Err("output_limit");
                        }
                        if stdout {
                            if seen.result.len() + n > RESULT_MAX + 4 {
                                return Err("invalid_result");
                            }
                            seen.result.extend_from_slice(&output[..n]);
                            if seen.result.len() >= 4 {
                                let size = u32::from_be_bytes(seen.result[..4].try_into().unwrap())
                                    as usize;
                                if size == 0 || size > RESULT_MAX || seen.result.len() > size + 4 {
                                    return Err("invalid_result");
                                }
                            }
                        }
                    }
                    Err(e) if retry(&e) => {}
                    Err(_) => return Err("output_read_failed"),
                }
            }
            let mut fds = [
                libc::pollfd {
                    fd: pidfd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if input_done {
                        -1
                    } else {
                        self.input.as_raw_fd()
                    },
                    events: libc::POLLOUT,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if stdout_eof {
                        -1
                    } else {
                        self.output.as_raw_fd()
                    },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if stderr_eof {
                        -1
                    } else {
                        self.error.as_raw_fd()
                    },
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let polled = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 5) };
            if polled < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err("poll_failed");
            }
            if fds.iter().any(|fd| fd.revents & libc::POLLNVAL != 0) {
                return Err("poll_failed");
            }
            if fds[0].revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                return Err("identity_observation_failed");
            }
            if fds[0].revents & libc::POLLIN != 0 && stdout_eof && stderr_eof {
                if clock().map_err(|_| "clock_unreadable")? >= deadline {
                    return Err("hard_deadline");
                }
                if !input_done {
                    return Err("partial_delivery");
                }
                return result(&seen.result);
            }
        }
    }
}

impl ProtocolReport {
    // Only the held boundary's cleanup result reaches this in production.
    fn finish_cleanup(&mut self, cleanup: Result<i32>) {
        match cleanup {
            Ok(status) => {
                self.wait_status = Some(status);
                self.cleanup_proven_empty = true;
                if self.protocol_success
                    && (!libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0)
                {
                    self.protocol_success = false;
                    self.reason = "adapter_exit";
                }
            }
            Err(_) => {
                self.protocol_success = false;
                self.reason = "cleanup_unproven";
                self.cleanup_proven_empty = false;
                self.wait_status = None;
            }
        }
    }
}

impl ExecGate<'_> {
    pub(super) fn supervise(mut self, streams: Streams, input: Input) -> Result<ProtocolReport> {
        self.acknowledge_exec()?;
        let mut report = streams.run(input, &self.pidfd, self.deadline);
        // Exactly one bounded cleanup; never retry ambiguity on Drop. Even
        // zero exit + proven empty leaves both ledgers/capacity untouched.
        self.cleaned = true;
        report.finish_cleanup(cleanup_child(self.held, self.pid, &self.pidfd, boottime_ms));
        Ok(report)
    }
}

#[cfg(test)]
mod tests;
