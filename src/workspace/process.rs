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

pub(super) fn run(mut command: Command, lock: RawFd) -> Result<Vec<u8>> {
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
    let mut child = command
        .spawn()
        .context("cannot start workspace helper; install Treehouse with durable JSON leases")?;
    let pid = child.id();
    let result = (|| {
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
        let mut ended = [false; 2];
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
            if let Some(success) = crate::jobs::exited_unreaped(pid)? {
                ensure!(
                    success,
                    "workspace helper refused the operation (diagnostics withheld)"
                );
                if ended.iter().all(|v| *v) {
                    return Ok(output);
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
    let output = result?;
    waited?;
    Ok(output)
}
