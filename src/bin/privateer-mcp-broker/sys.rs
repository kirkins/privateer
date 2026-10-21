//! The few operating-system calls std does not cover: flock, the socket
//! peer's uid, a signal self-pipe, and a bounded log.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The log is cut over to `NAME.log.1` past this size.
const LOG_LIMIT: u64 = 1024 * 1024;

pub fn euid() -> u32 {
    // SAFETY: geteuid cannot fail.
    unsafe { libc::geteuid() }
}

/// Take `path`'s exclusive flock without waiting; `None` if another process
/// holds it. The returned file keeps the lock until it is dropped.
pub fn try_lock(path: &Path) -> io::Result<Option<File>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    // SAFETY: a valid descriptor this function owns.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(None)
    } else {
        Err(error)
    }
}

/// The uid of the process at the other end of a connected Unix socket.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn peer_uid(fd: RawFd) -> io::Result<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` describe a writable ucred.
    let done = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if done != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn peer_uid(fd: RawFd) -> io::Result<u32> {
    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: both out-pointers are valid.
    if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(_: libc::c_int) {
    let fd = SIGNAL_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: write is async-signal-safe; a full pipe already says enough.
        unsafe { libc::write(fd, [1u8].as_ptr().cast(), 1) };
    }
}

/// A descriptor that becomes readable on SIGTERM or SIGINT. SIGHUP is
/// ignored: the broker runs detached.
pub fn stop_signals() -> io::Result<RawFd> {
    let mut fds = [0; 2];
    // SAFETY: `fds` holds two descriptors.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for fd in fds {
        // SAFETY: descriptors this function just made.
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
        }
    }
    SIGNAL_PIPE.store(fds[1], Ordering::Relaxed);
    // SAFETY: installing a handler that only writes one byte.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_signal as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = libc::SA_RESTART;
        for signal in [libc::SIGTERM, libc::SIGINT] {
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    Ok(fds[0])
}

/// Wait until one of `fds` is readable; the readable ones.
pub fn wait_readable(fds: &[RawFd]) -> io::Result<Vec<RawFd>> {
    let mut polls: Vec<libc::pollfd> = fds
        .iter()
        .map(|fd| libc::pollfd {
            fd: *fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    loop {
        // SAFETY: `polls` is a valid array of its length.
        let ready = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as _, -1) };
        if ready >= 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok(polls
        .iter()
        .filter(|p| p.revents != 0)
        .map(|p| p.fd)
        .collect())
}

/// Signal a whole process group; false once it no longer exists.
pub fn signal_group(pgid: u32, signal: libc::c_int) -> bool {
    // SAFETY: kill has no memory effects.
    unsafe { libc::kill(-(pgid as libc::pid_t), signal) == 0 }
}

/// Write `bytes` to `path` through a temporary file beside it, mode 0600.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(bytes)?;
    fs::rename(&temp, path)
}

/// Make `dir` (0700, ours, not a symlink), or refuse.
pub fn private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let info = fs::symlink_metadata(dir)?;
    if !info.is_dir() || info.uid() != euid() {
        return Err(io::Error::other(format!(
            "{} is not a directory this user owns",
            dir.display()
        )));
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// A log of one-line events, never message bodies, cut over to `.1` past
/// LOG_LIMIT. When the broker's own stdout and stderr were opened on this log
/// (the dispatcher's start), they follow it to the new file.
pub struct Log {
    path: PathBuf,
    file: Mutex<Option<File>>,
    owns_stdio: bool,
}

impl Log {
    pub fn open(path: PathBuf) -> Log {
        let file = append(&path).ok();
        let owns_stdio = file.as_ref().is_some_and(|file| same_file(file, 2));
        Log {
            path,
            file: Mutex::new(file),
            owns_stdio,
        }
    }

    pub fn line(&self, message: &str) {
        let Ok(mut file) = self.file.lock() else {
            return;
        };
        if file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .is_some_and(|m| m.len() > LOG_LIMIT)
        {
            let mut old = self.path.clone().into_os_string();
            old.push(".1");
            let _ = fs::rename(&self.path, old);
            *file = append(&self.path).ok();
            if let (Some(new), true) = (file.as_ref(), self.owns_stdio) {
                // SAFETY: duplicating a descriptor this log owns onto stdio.
                unsafe {
                    libc::dup2(new.as_raw_fd(), 1);
                    libc::dup2(new.as_raw_fd(), 2);
                }
            }
        }
        if let Some(file) = file.as_mut() {
            let text: String = message.chars().take(1_000).collect();
            let _ = writeln!(file, "{} {}", now_ms(), text.replace('\n', " "));
        }
    }
}

fn append(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

fn same_file(file: &File, fd: RawFd) -> bool {
    // SAFETY: fstat into a zeroed stat.
    let mut info: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut info) } != 0 {
        return false;
    }
    file.metadata()
        .is_ok_and(|m| m.dev() == info.st_dev as u64 && m.ino() == info.st_ino as u64)
}
