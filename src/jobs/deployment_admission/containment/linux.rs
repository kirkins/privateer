//! No Rust allocation, locks, unwinding, or destructors on the clone child path.
//! clone3 is fork-like (never CLONE_VM). Only libc/syscalls run before _exit.
use super::Refusal;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    ffi::{CStr, CString},
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::fs::MetadataExt,
    },
    path::Path,
};

mod gate;
#[cfg(test)]
mod tests;

const LIMIT: u64 = 4096;
const PROBE_MS: u64 = 2000;
const CLEANUP_MS: u64 = 2000;
// Linux UAPI include/uapi/linux/sched.h. libc 0.2.190's deprecated constant
// overflows c_int; casting it would silently erase the containment flag.
const CLONE_INTO_CGROUP: u64 = 0x200000000;

// A held, empty, probed boundary. No public constructor or adapter spawn path.
// Dropping this token does NOT delete the cgroup or imply custody discharge.
#[derive(Debug)]
pub(crate) struct Qualified {
    directory: File,
    parent: File,
    name: CString,
    identity: Value,
    cleanup_unknown: bool,
}

impl Qualified {
    pub(crate) fn identity(&self) -> &Value {
        &self.identity
    }

    #[cfg(test)]
    pub(super) fn dispose_test_boundary(self) {
        assert_eq!(self.identity()["containment_scope"], "attempt_cgroup_only");
        assert_eq!(
            self.identity()["limitation"],
            "outside_ipc_delegation_not_excluded"
        );
        self.kill_empty(boottime_ms().unwrap() + CLEANUP_MS)
            .unwrap();
        self.remove().unwrap();
    }

    fn read(&self, name: &CStr) -> Result<String> {
        bounded_read(open_at(&self.directory, name, libc::O_RDONLY)?)
    }

    fn kill(&self) -> Result<()> {
        open_at(&self.directory, c"cgroup.kill", libc::O_WRONLY)?.write_all(b"1")?;
        Ok(())
    }

    fn kill_empty(&self, deadline: u64) -> Result<()> {
        self.kill()?;
        self.empty(deadline)
    }

    fn empty(&self, deadline: u64) -> Result<()> {
        loop {
            ensure!(
                boottime_ms()? < deadline,
                "cgroup cleanup deadline; emptiness unknown"
            );
            if populated(&self.read(c"cgroup.events")?)? == 0 {
                return Ok(());
            }
            // Bounded wait, rechecking CLOCK_BOOTTIME even across suspend/EINTR.
            unsafe {
                libc::poll(std::ptr::null_mut(), 0, 5);
            }
        }
    }

    fn remove(&self) -> Result<()> {
        ensure!(
            unsafe {
                libc::unlinkat(
                    self.parent.as_raw_fd(),
                    self.name.as_ptr(),
                    libc::AT_REMOVEDIR,
                )
            } == 0,
            "cannot remove empty probe cgroup"
        );
        Ok(())
    }
}

pub(super) fn qualify(path: &Path) -> Result<Qualified, Refusal> {
    let mut boundary = None;
    let result = (|| -> Result<()> {
        ensure!(
            kernel_floor(&bounded_read(File::open("/proc/sys/kernel/osrelease")?)?),
            "requires Linux >=5.16 open-time cgroup namespace/credential checks"
        );
        let parent = open_directory(path)?;
        let mount_id = verify_mount(&parent)?;
        // Only checks open permission, NEVER migrates the supervisor.
        let _ancestor_write = open_at(&parent, c"cgroup.procs", libc::O_WRONLY)?;
        let name = CString::new(format!("deployment-probe-{}", uuid::Uuid::new_v4()))?;
        ensure!(
            unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } == 0,
            "cannot create isolated cgroup: {}",
            std::io::Error::last_os_error()
        );
        let directory = match open_at(&parent, &name, libc::O_RDONLY | libc::O_DIRECTORY) {
            Ok(fd) => fd,
            Err(e) => {
                // No child has been created. Best effort empty-directory removal.
                unsafe {
                    libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR);
                }
                return Err(e);
            }
        };
        let metadata = directory.metadata()?;
        let label = name.to_string_lossy().into_owned();
        let parent_metadata = parent.metadata()?;
        boundary = Some(Qualified {
            directory,
            parent,
            name,
            identity: json!({"device":metadata.dev(), "inode":metadata.ino(),
                "mount_id":mount_id, "name":label,
                "delegation":path.to_string_lossy(),
                "parent_device":parent_metadata.dev(),
                "parent_inode":parent_metadata.ino(),
                "containment_scope":"attempt_cgroup_only",
                "limitation":"outside_ipc_delegation_not_excluded"}),
            cleanup_unknown: false,
        });
        let held = boundary.as_mut().unwrap();
        open_at(&held.directory, c"pids.max", libc::O_WRONLY)?.write_all(b"16")?;
        ensure!(
            held.read(c"pids.max")?.trim() == "16",
            "finite pids.max not enforced"
        );
        ensure!(
            populated(&held.read(c"cgroup.events")?)? == 0,
            "probe cgroup not empty"
        );
        probe(held)?;
        Ok(())
    })();
    match result {
        Ok(()) => Ok(boundary.unwrap()),
        Err(error) => {
            let mut reason = format!("containment refused: {error:#}");
            let retained = boundary.and_then(|held| {
                if held.cleanup_unknown {
                    return Some(held.identity);
                }
                match boottime_ms()
                    .and_then(|now| held.kill_empty(now + CLEANUP_MS))
                    .and_then(|()| held.remove())
                {
                    Ok(()) => None,
                    Err(e) => {
                        reason.push_str(&format!("; cleanup unresolved: {e:#}"));
                        Some(held.identity)
                    }
                }
            });
            let mut end = reason.len().min(1024);
            while !reason.is_char_boundary(end) {
                end -= 1;
            }
            reason.truncate(end);
            Err(Refusal { reason, retained })
        }
    }
}

pub(super) fn kernel_floor(release: &str) -> bool {
    let mut parts = release.trim().split('.');
    let major = parts.next().and_then(|s| s.parse::<u32>().ok());
    let minor = parts.next().and_then(|s| s.parse::<u32>().ok());
    matches!((major, minor), (Some(5), Some(16..)) | (Some(6..), Some(_)))
}

fn open_directory(path: &Path) -> Result<File> {
    use std::os::unix::ffi::OsStrExt;
    ensure!(path.is_absolute(), "delegation must be absolute");
    let mut dir = File::open("/")?;
    for part in path.components() {
        match part {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => {
                dir = open_at(
                    &dir,
                    &CString::new(name.as_bytes())?,
                    libc::O_PATH | libc::O_DIRECTORY,
                )?;
            }
            _ => anyhow::bail!("noncanonical delegation path"),
        }
    }
    Ok(dir)
}

fn open_at(directory: &File, name: &CStr, flags: i32) -> Result<File> {
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    ensure!(
        fd >= 0,
        "cgroup open failed: {}",
        std::io::Error::last_os_error()
    );
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn bounded_read(file: File) -> Result<String> {
    let mut bytes = String::new();
    file.take(LIMIT + 1).read_to_string(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= LIMIT,
        "cgroup/proc observation exceeds bound"
    );
    Ok(bytes)
}

fn verify_mount(directory: &File) -> Result<String> {
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::fstatfs(directory.as_raw_fd(), &mut stat) } == 0
            && stat.f_type == libc::CGROUP2_SUPER_MAGIC,
        "delegation is not cgroup2"
    );
    let info = bounded_read(File::open(format!(
        "/proc/self/fdinfo/{}",
        directory.as_raw_fd()
    ))?)?;
    let mount_id = info
        .lines()
        .find_map(|l| l.strip_prefix("mnt_id:\t"))
        .context("mount id missing")?;
    // mountinfo can be large; bound it, fail closed instead of partial matching.
    let mut mounts = String::new();
    File::open("/proc/self/mountinfo")?
        .take(1_048_577)
        .read_to_string(&mut mounts)?;
    ensure!(mounts.len() <= 1_048_576, "mountinfo exceeds bound");
    let line = mounts
        .lines()
        .find(|l| l.split_whitespace().next() == Some(mount_id))
        .context("held mount missing")?;
    let (_, fs) = line.split_once(" - ").context("malformed mountinfo")?;
    let parts: Vec<_> = fs.split_whitespace().collect();
    ensure!(
        parts.len() == 3 && parts[0] == "cgroup2" && parts[2].split(',').any(|o| o == "nsdelegate"),
        "held mount lacks nsdelegate"
    );
    Ok(mount_id.to_owned())
}

pub(super) fn populated(events: &str) -> Result<u8> {
    let values: Vec<_> = events
        .lines()
        .filter_map(|l| l.strip_prefix("populated "))
        .collect();
    ensure!(values.len() == 1, "missing/duplicate populated observation");
    match values[0] {
        "0" => Ok(0),
        "1" => Ok(1),
        _ => anyhow::bail!("invalid populated observation"),
    }
}

fn boottime_ms() -> Result<u64> {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) } == 0,
        "CLOCK_BOOTTIME unavailable"
    );
    Ok(ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000)
}

// Kernel clone_args ABI, including fields introduced by version 2 (5.7).
#[repr(C)]
#[derive(Default)]
struct CloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}

fn duplicate(fd: RawFd) -> Result<File> {
    let new = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
    ensure!(new >= 0, "cannot prepare probe descriptors");
    Ok(unsafe { File::from_raw_fd(new) })
}

fn probe(held: &mut Qualified) -> Result<()> {
    let parent_namespace = File::open("/proc/self/ns/cgroup")?.metadata()?.ino();
    let deadline = boottime_ms()? + PROBE_MS;
    let cwd = tempfile::tempdir()?;
    let work = duplicate(File::open(cwd.path())?.as_raw_fd())?;
    let ancestor = duplicate(held.parent.as_raw_fd())?;
    let mut pipes = [-1; 2];
    ensure!(
        unsafe { libc::pipe2(pipes.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } == 0,
        "probe pipe unavailable"
    );
    let mut input = unsafe { File::from_raw_fd(pipes[0]) };
    let original_output = unsafe { File::from_raw_fd(pipes[1]) };
    let output = duplicate(original_output.as_raw_fd())?;
    drop(original_output);
    let mut pidfd = -1i32;
    let args = CloneArgs {
        flags: CLONE_INTO_CGROUP | libc::CLONE_NEWCGROUP as u64 | libc::CLONE_PIDFD as u64,
        pidfd: (&mut pidfd as *mut i32) as u64,
        exit_signal: libc::SIGCHLD as u64,
        cgroup: held.directory.as_raw_fd() as u64,
        ..Default::default()
    };
    let pid = unsafe { libc::syscall(libc::SYS_clone3, &args, std::mem::size_of::<CloneArgs>()) };
    if pid == 0 {
        // SAFETY: no allocation, Rust cleanup or shared-VM clone on this path.
        unsafe {
            probe_child(
                ancestor.as_raw_fd(),
                output.as_raw_fd(),
                work.as_raw_fd(),
                parent_namespace,
                std::ptr::null(),
            )
        }
    }
    ensure!(
        pid > 0,
        "clone3 INTO_CGROUP/NEWCGROUP/PIDFD refused: {}",
        std::io::Error::last_os_error()
    );
    // CLONE_PIDFD is mandatory: never signal a possibly reused numeric pid.
    let pidfd = unsafe { File::from_raw_fd(pidfd) };
    // Retain custody conservatively on every subsequent early error. Only a
    // completed pidfd reap AND kill-and-empty proof can clear this flag.
    held.cleanup_unknown = true;
    held.identity["probe_pid"] = json!(pid);
    drop(output);
    let result = (|| -> Result<()> {
        held.identity["probe_process"] = serde_json::to_value(
            crate::jobs::identity(pid as u32)?.context("probe process identity unavailable")?,
        )?;
        let mut report = [0u8; 12];
        let mut got = 0;
        while got < report.len() {
            ensure!(boottime_ms()? < deadline, "probe child deadline");
            match input.read(&mut report[got..]) {
                Ok(0) => anyhow::bail!("probe exited before acknowledgement"),
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => unsafe {
                    libc::poll(std::ptr::null_mut(), 0, 5);
                },
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        validate_report(&report)?;
        let members = held.read(c"cgroup.procs")?;
        ensure!(
            members.trim() == pid.to_string(),
            "probe membership mismatch"
        );
        Ok(())
    })();
    // The pidfd also reaches a probe that unexpectedly migrated outside the
    // attempt. Such migration always refuses qualification, even after cleanup.
    let cleanup = cleanup_child(held, pid as i32, &pidfd, boottime_ms);
    cleanup?;
    result
}

fn validate_report(report: &[u8; 12]) -> Result<()> {
    let root = i32::from_ne_bytes(report[..4].try_into()?);
    let opened = i32::from_ne_bytes(report[4..8].try_into()?);
    let migration = i32::from_ne_bytes(report[8..].try_into()?);
    ensure!(
        root == 1,
        "child namespace root/fd/credential policy not proven"
    );
    ensure!(opened == 0, "child ancestor open failed: {opened}");
    ensure!(
        migration == libc::ENOENT,
        "child ancestor write not ENOENT: {migration}"
    );
    Ok(())
}

// No fallible operation may short-circuit either kill. A second BOOTTIME read
// may bound rescue if the first read fails, but can never qualify the attempt.
// Persistent clock failure permits only nonblocking reap/observation. Never
// reuse the runtime deadline: an early drop could otherwise wait up to an hour.
fn cleanup_child(
    held: &mut Qualified,
    pid: i32,
    pidfd: &File,
    mut clock: impl FnMut() -> Result<u64>,
) -> Result<i32> {
    held.cleanup_unknown = true;
    let first_clock = clock();
    let deadline = first_clock
        .as_ref()
        .copied()
        .or_else(|_| clock())
        .map_or(0, |now| now + CLEANUP_MS);
    let killed_cgroup = held.kill();
    let killed = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    let kill_error = std::io::Error::last_os_error().raw_os_error();
    let reaped = reap(pid, pidfd, deadline);
    if let Ok(status) = &reaped {
        held.identity["last_wait_status"] = json!(status);
    }
    let empty = held.empty(deadline);
    ensure!(
        killed == 0 || kill_error == Some(libc::ESRCH),
        "probe pidfd kill failed"
    );
    killed_cgroup?;
    let status = reaped?;
    empty?;
    first_clock?;
    held.cleanup_unknown = false;
    Ok(status)
}

fn reap(pid: i32, pidfd: &File, deadline: u64) -> Result<i32> {
    loop {
        let mut status = 0;
        let done = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if done == pid {
            return Ok(status);
        }
        ensure!(
            done == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR),
            "probe reap failed"
        );
        ensure!(
            boottime_ms()? < deadline,
            "probe reap deadline; inspect custody"
        );
        let mut fd = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        unsafe {
            libc::poll(&mut fd, 1, 5);
        }
    }
}

// Probe child policy is the future adapter's mandatory pre-exec policy. It
// opens migration itself AFTER clone/credential drop, never receives authority,
// and stays gated until the parent exercises kill-and-empty. No exec in ii.
unsafe fn probe_child(
    ancestor: RawFd,
    output: RawFd,
    cwd: RawFd,
    parent_namespace: u64,
    exec: *const gate::ChildExec,
) -> ! {
    unsafe {
        if libc::dup3(ancestor, 3, libc::O_CLOEXEC) < 0
            || libc::dup3(output, 4, libc::O_CLOEXEC) < 0
            || libc::fchdir(cwd) < 0
        {
            libc::_exit(120);
        }
        if exec.is_null() {
            if libc::syscall(libc::SYS_close_range, 5u32, u32::MAX, 0) < 0 {
                libc::_exit(120);
            }
            for fd in 0..3 {
                libc::close(fd);
            }
        } else {
            gate::prepare_child(&*exec);
        }
        libc::umask(0o077);
        #[repr(C)]
        struct Header {
            version: u32,
            pid: i32,
        }
        #[repr(C)]
        struct Caps {
            effective: u32,
            permitted: u32,
            inheritable: u32,
        }
        let header = Header {
            version: 0x20080522,
            pid: 0,
        };
        let caps = [
            Caps {
                effective: 0,
                permitted: 0,
                inheritable: 0,
            },
            Caps {
                effective: 0,
                permitted: 0,
                inheritable: 0,
            },
        ];
        if libc::syscall(libc::SYS_capset, &header, caps.as_ptr()) < 0
            || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0
        {
            libc::_exit(121);
        }
        let fd = libc::open(
            c"/proc/self/cgroup".as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        );
        let mut root = [0u8; 64];
        let n = if fd >= 0 {
            libc::read(fd, root.as_mut_ptr().cast(), root.len())
        } else {
            -1
        };
        libc::close(fd);
        let ns = libc::open(
            c"/proc/self/ns/cgroup".as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        );
        let mut stat: libc::stat = std::mem::zeroed();
        let distinct =
            ns >= 0 && libc::fstat(ns, &mut stat) == 0 && stat.st_ino != parent_namespace;
        libc::close(ns);
        let rooted: i32 = i32::from(n == 5 && &root[..5] == b"0::/\n" && distinct);
        // The opener must be this child, so ctx->ns and f_cred are this policy's.
        let migration = libc::openat(
            3,
            c"cgroup.procs".as_ptr(),
            libc::O_WRONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        );
        let opened = if migration < 0 {
            *libc::__errno_location()
        } else {
            0
        };
        let errno = if migration < 0 {
            0 // Never treat an open error as the write's ENOENT proof.
        } else {
            let wrote = libc::write(migration, b"0".as_ptr().cast(), 1);
            if wrote == 1 {
                0
            } else {
                *libc::__errno_location()
            }
        };
        libc::close(migration);
        libc::close(3);
        let mut report = [0u8; 12];
        report[..4].copy_from_slice(&rooted.to_ne_bytes());
        report[4..8].copy_from_slice(&opened.to_ne_bytes());
        report[8..].copy_from_slice(&errno.to_ne_bytes());
        if libc::write(4, report.as_ptr().cast(), 12) != 12 {
            libc::_exit(122);
        }
        if !exec.is_null() {
            gate::exec_child(&*exec);
        }
        libc::close(4);
        loop {
            libc::pause();
        }
    }
}
