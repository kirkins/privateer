//! A verifier is a bounded read-only helper, not an uploader. Keep its leader
//! unreaped until group cleanup, including success; no detached helpers allowed.
use super::config::Tool;
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    io::{Read, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const LIMIT: usize = 65_536;
fn nonblocking(pipe: &impl AsRawFd) -> Result<()> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(
        flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
        "cannot make verifier pipe nonblocking"
    );
    Ok(())
}

fn drain(pipe: &mut impl Read, bytes: &mut usize, output: Option<&mut Vec<u8>>) -> Result<bool> {
    let mut output = output;
    let mut buffer = [0; 4096];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                *bytes += n;
                ensure!(
                    *bytes <= LIMIT,
                    "verifier output exceeds limit; diagnostics withheld"
                );
                if let Some(out) = output.as_mut() {
                    out.extend_from_slice(&buffer[..n]);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => bail!("verifier output failed; diagnostics withheld"),
        }
    }
}

pub(super) fn invoke(tool: &Tool, cwd: &Path, request: &Value, timeout: Duration) -> Result<Value> {
    let mut request = serde_json::to_vec(request)?;
    request.push(b'\n');
    ensure!(request.len() <= 16_384, "verifier request exceeds limit");
    let mut command = Command::new(&tool.executable);
    command
        .args(&tool.args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", cwd)
        .env("TMPDIR", cwd)
        .env("XDG_CONFIG_HOME", cwd)
        .env("XDG_DATA_HOME", cwd)
        .env("XDG_STATE_HOME", cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(config) = &tool.config {
        command.env("PRIVATEER_VERIFIER_CONFIG", config);
    }
    // CLOEXEC closes inherited descriptors at exec without closing Rust's
    // private exec-error pipe before it can report a failed launch.
    let max_fd = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    ensure!(max_fd > 0, "cannot determine descriptor limit");
    unsafe {
        command.pre_exec(move || {
            libc::umask(0o077);
            #[cfg(target_os = "linux")]
            if libc::syscall(
                libc::SYS_close_range,
                3u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            ) == 0
            {
                return Ok(());
            }
            // Bound the portable fallback's work and conversion. If close_range
            // is unavailable and the limit is larger, refuse rather than leak
            // descriptors or loop over an unbounded/overflowing range.
            if !(1..=65_536).contains(&max_fd) {
                return Err(std::io::Error::from_raw_os_error(libc::E2BIG));
            }
            for fd in 3..(max_fd as i32) {
                if libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::EBADF) {
                        return Err(error);
                    }
                }
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .context("cannot start captain-configured verifier")?;
    let pid = child.id();
    let result = (|| -> Result<Value> {
        let input = child.stdin.take().context("verifier stdin missing")?;
        let mut stdout = child.stdout.take().context("verifier stdout missing")?;
        let mut stderr = child.stderr.take().context("verifier stderr missing")?;
        nonblocking(&input)?;
        nonblocking(&stdout)?;
        nonblocking(&stderr)?;
        let deadline = Instant::now() + timeout;
        let mut written = 0;
        let mut input = Some(input);
        let (mut out_done, mut err_done, mut bytes) = (false, false, 0);
        let mut output = Vec::new();
        loop {
            ensure!(
                Instant::now() < deadline,
                "verifier timed out; diagnostics withheld"
            );
            if let Some(pipe) = input.as_mut() {
                match pipe.write(&request[written..]) {
                    Ok(n) => written += n,
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => bail!("verifier input failed; diagnostics withheld"),
                }
                if written == request.len() {
                    input = None;
                }
            }
            if !out_done {
                out_done = drain(&mut stdout, &mut bytes, Some(&mut output))?;
            }
            if !err_done {
                err_done = drain(&mut stderr, &mut bytes, None)?;
            }
            if let Some(success) = crate::jobs::exited_unreaped(pid)? {
                ensure!(success, "verifier failed; diagnostics withheld");
                if out_done && err_done {
                    ensure!(
                        written == request.len(),
                        "verifier exited before accepting request"
                    );
                    let text = std::str::from_utf8(&output).context("invalid verifier UTF-8")?;
                    let lines: Vec<_> = text
                        .lines()
                        .filter(|line| !line.trim().is_empty())
                        .collect();
                    ensure!(lines.len() == 1, "verifier must emit exactly one JSON line");
                    return serde_json::from_str(lines[0]).map_err(|_| {
                        anyhow::anyhow!("invalid verifier JSON; diagnostics withheld")
                    });
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    // The child has not been reaped: its PID still reserves its process group.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
    let waited = child.wait();
    if result.is_ok() {
        ensure!(
            waited.context("cannot reap verifier")?.success(),
            "verifier failed"
        );
    }
    result
}
