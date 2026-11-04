//! Private authority-free exec machinery. There is deliberately no exported
//! constructor/release method. The later consuming Handoff spawn must supply
//! sealed executable verification and durable custody BEFORE using this gate.
use super::*;

pub(super) struct ChildExec {
    gate: RawFd,
    executable: RawFd,
    null: RawFd,
    argv: *const *const libc::c_char,
    env: *const *const libc::c_char,
}

struct ExecGate<'a> {
    held: &'a mut Qualified,
    pid: i32,
    pidfd: File,
    release: File,
    report: File,
    _cwd: tempfile::TempDir,
    deadline: u64,
    cleaned: bool,
}

fn pipe() -> Result<(File, File)> {
    let mut fds = [-1; 2];
    ensure!(
        unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } == 0,
        "gate pipe unavailable"
    );
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}

impl Qualified {
    // Intentionally private even to the rest of the crate. Only authority-free
    // fixtures call this in iii-a. A held File is not a digest/claim proof.
    fn gate(
        &mut self,
        executable: &File,
        args: &[CString],
        timeout_ms: u64,
    ) -> Result<ExecGate<'_>> {
        use std::os::unix::fs::FileExt;
        ensure!(
            (1..=3_600_000).contains(&timeout_ms),
            "invalid gate timeout"
        );
        ensure!(
            !args.is_empty() && args.len() <= 64,
            "invalid gate argv count"
        );
        ensure!(
            args.iter()
                .map(|a| a.as_bytes_with_nul().len())
                .sum::<usize>()
                <= 65536,
            "gate argv exceeds bound"
        );
        let mut magic = [0; 4];
        ensure!(
            executable.metadata()?.is_file()
                && executable.read_at(&mut magic, 0)? == 4
                && magic == *b"\x7fELF",
            "gate requires a held native executable, not a shebang"
        );
        ensure!(!self.cleanup_unknown, "unresolved earlier custody");
        verify_mount(&self.directory)?;
        ensure!(self.read(c"pids.max")?.trim() == "16", "pids.max changed");
        ensure!(
            populated(&self.read(c"cgroup.events")?)? == 0,
            "boundary not empty"
        );
        // Repeat the full disposable-child proof on this exact held boundary,
        // then repeat child policy/proof once more at the actual exec gate.
        probe(self)?;
        let deadline = boottime_ms()? + timeout_ms;
        let parent_namespace = File::open("/proc/self/ns/cgroup")?.metadata()?.ino();
        let cwd = tempfile::tempdir()?;
        let work = duplicate(File::open(cwd.path())?.as_raw_fd())?;
        let ancestor = duplicate(self.parent.as_raw_fd())?;
        let native = duplicate(executable.as_raw_fd())?;
        let null = duplicate(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/null")?
                .as_raw_fd(),
        )?;
        let (input, output) = pipe()?;
        let output = duplicate(output.as_raw_fd())?;
        let (gate, release) = pipe()?;
        let gate = duplicate(gate.as_raw_fd())?;
        let argv: Vec<_> = args
            .iter()
            .map(|arg| arg.as_ptr())
            .chain([std::ptr::null()])
            .collect();
        let env = [c"PRIVATEER_ADAPTER_PROTOCOL=1".as_ptr(), std::ptr::null()];
        let child = ChildExec {
            gate: gate.as_raw_fd(),
            executable: native.as_raw_fd(),
            null: null.as_raw_fd(),
            argv: argv.as_ptr(),
            env: env.as_ptr(),
        };
        let mut pidfd = -1i32;
        let args = CloneArgs {
            flags: CLONE_INTO_CGROUP | libc::CLONE_NEWCGROUP as u64 | libc::CLONE_PIDFD as u64,
            pidfd: (&mut pidfd as *mut i32) as u64,
            exit_signal: libc::SIGCHLD as u64,
            cgroup: self.directory.as_raw_fd() as u64,
            ..Default::default()
        };
        let pid =
            unsafe { libc::syscall(libc::SYS_clone3, &args, std::mem::size_of::<CloneArgs>()) };
        if pid == 0 {
            unsafe {
                probe_child(
                    ancestor.as_raw_fd(),
                    output.as_raw_fd(),
                    work.as_raw_fd(),
                    parent_namespace,
                    &child,
                )
            }
        }
        ensure!(
            pid > 0,
            "exec gate clone3 refused: {}",
            std::io::Error::last_os_error()
        );
        let pidfd = unsafe { File::from_raw_fd(pidfd) };
        // Establish RAII cleanup immediately after clone, before fallible reads.
        self.cleanup_unknown = true;
        let mut pending = ExecGate {
            held: self,
            pid: pid as i32,
            pidfd,
            release,
            report: input,
            _cwd: cwd,
            deadline,
            cleaned: false,
        };
        drop(output);
        pending.held.identity["gate_pid"] = json!(pid);
        pending.held.identity["gate_process"] = serde_json::to_value(
            crate::jobs::identity(pid as u32)?.context("gate process identity unavailable")?,
        )?;
        let mut report = [0; 12];
        pending.read_report(&mut report)?;
        validate_report(&report)?;
        ensure!(
            pending.held.read(c"cgroup.procs")?.trim() == pid.to_string(),
            "exec gate membership mismatch"
        );
        Ok(pending)
    }
}

impl ExecGate<'_> {
    fn check_deadline(&self) -> Result<()> {
        ensure!(boottime_ms()? < self.deadline, "exec gate hard deadline");
        Ok(())
    }

    fn read_report(&mut self, bytes: &mut [u8]) -> Result<()> {
        let mut got = 0;
        while got < bytes.len() {
            self.check_deadline()?;
            match self.report.read(&mut bytes[got..]) {
                Ok(0) => anyhow::bail!("gate exited before acknowledgement"),
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => unsafe {
                    libc::poll(std::ptr::null_mut(), 0, 5);
                },
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    // Consuming and private. No deployment protocol or metadata is sent. iii-b
    // must replace this fixture-only wait with the bounded IO supervisor.
    fn execute(mut self) -> Result<i32> {
        self.check_deadline()?;
        self.release.write_all(b"1")?;
        // CLOEXEC EOF acknowledges exec; an explicit errno acknowledges failure.
        let mut error = [0; 4];
        let mut got = 0;
        loop {
            self.check_deadline()?;
            match self.report.read(&mut error[got..]) {
                Ok(0) if got == 0 => break,
                Ok(0) => anyhow::bail!("partial exec acknowledgement"),
                Ok(n) => {
                    got += n;
                    if got == 4 {
                        anyhow::bail!("held-inode exec failed: {}", i32::from_ne_bytes(error));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => unsafe {
                    libc::poll(std::ptr::null_mut(), 0, 5);
                },
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        loop {
            self.check_deadline()?;
            let mut fd = libc::pollfd {
                fd: self.pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut fd, 1, 5) };
            ensure!(
                result >= 0
                    || std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted,
                "gate exit observation failed"
            );
            if result > 0 {
                ensure!(
                    fd.revents & libc::POLLIN != 0,
                    "gate pidfd observation invalid"
                );
                break;
            }
        }
        // Never repeat an ambiguous cleanup with a fresh deadline on Drop.
        self.cleaned = true;
        let status = cleanup_child(self.held, self.pid, &self.pidfd, boottime_ms)?;
        self.held.cleanup_unknown = false;
        Ok(status)
    }
}

impl Drop for ExecGate<'_> {
    fn drop(&mut self) {
        if !self.cleaned {
            // Drop cannot report success or discard ambiguous custody. The held
            // boundary and recorded identity outlive this borrowed gate token.
            if cleanup_child(self.held, self.pid, &self.pidfd, boottime_ms).is_ok() {
                self.held.cleanup_unknown = false;
            }
        }
    }
}

// Child-only: all fd sources were duplicated above 9 before clone. Never add
// allocation, formatting, locks, destructors or unwinding to these functions.
pub(super) unsafe fn prepare_child(exec: &ChildExec) {
    unsafe {
        if libc::dup3(exec.gate, 5, libc::O_CLOEXEC) < 0
            || libc::dup3(exec.executable, 6, libc::O_CLOEXEC) < 0
        {
            libc::_exit(123);
        }
        for fd in 0..3 {
            if libc::dup2(exec.null, fd) < 0 {
                libc::_exit(123);
            }
        }
        if libc::syscall(libc::SYS_close_range, 7u32, u32::MAX, 0) < 0 {
            libc::_exit(123);
        }
    }
}

pub(super) unsafe fn exec_child(exec: &ChildExec) -> ! {
    unsafe {
        let mut release = 0u8;
        loop {
            let n = libc::read(5, (&mut release as *mut u8).cast(), 1);
            if n == 1 {
                break;
            }
            if n == 0 {
                libc::_exit(124);
            }
            let errno = *libc::__errno_location();
            if errno != libc::EAGAIN && errno != libc::EINTR {
                libc::_exit(124);
            }
            let mut fd = libc::pollfd {
                fd: 5,
                events: libc::POLLIN,
                revents: 0,
            };
            libc::poll(&mut fd, 1, 5);
        }
        if release != b'1' {
            libc::_exit(124);
        }
        libc::close(5);
        libc::syscall(
            libc::SYS_execveat,
            6,
            c"".as_ptr(),
            exec.argv,
            exec.env,
            libc::AT_EMPTY_PATH,
        );
        let errno = *libc::__errno_location();
        libc::write(4, errno.to_ne_bytes().as_ptr().cast(), 4);
        libc::_exit(125);
    }
}

#[cfg(test)]
mod tests;
