//! Dormant Codex hosted-pane custody. No queue, enable path or attachment claim.
//! Lock order: SQLite writer (if needed), then project flock. Poison publication
//! precedes flock. Never perform RPC, wait/reap a child, or acquire SQLite under
//! that flock. A future runner must recheck both pidfds/identities under it.
use crate::store::Store;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

const LIMIT: u64 = 1 << 20;

fn safe(path: &Path, directory: bool) -> Result<fs::Metadata> {
    let m = fs::symlink_metadata(path)?;
    ensure!(
        (if directory { m.is_dir() } else { m.is_file() })
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0,
        "unsafe Codex custody path"
    );
    Ok(m)
}

fn read(path: &Path) -> Result<Value> {
    safe(path, false)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= LIMIT,
        "Codex custody record too large"
    );
    let value: Value = serde_json::from_slice(&bytes)?;
    ensure!(value.is_object(), "invalid Codex custody record");
    Ok(value)
}

fn sync(dir: &Path) -> Result<()> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir)?
        .sync_all()?;
    Ok(())
}

fn atomic(dir: &Path, name: &str, value: &Value) -> Result<()> {
    safe(dir, true)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(&serde_json::to_vec(value)?)?;
    tmp.as_file().sync_all()?;
    tmp.persist(dir.join(name)).map_err(|e| e.error)?;
    sync(dir)
}

fn poison(dir: &Path, reason: &str) -> Result<()> {
    poison_record(dir, reason, None)
}

fn poison_record(dir: &Path, reason: &str, error: Option<&str>) -> Result<()> {
    safe(dir, true)?;
    let path = dir.join("custody-poison.json");
    match safe(&path, false) {
        Ok(_) => fs::remove_file(path)?,
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
        Err(e) => return Err(e),
    }
    sync(dir)?;
    atomic(
        dir,
        "custody-poison.json",
        &json!({"token": uuid::Uuid::new_v4().to_string(), "reason": reason,
            // Written BEFORE the best-effort update: even an unwritable record
            // or a supervisor crash leaves a durable availability diagnostic.
            "record_update": if error.is_some() { "failed" } else { "not_confirmed" },
            "record_error": error,
            "diagnostic": "Custody revoked; custody.json may still say owned. Only the poison token is authoritative."}),
    )
}

fn lock(dir: &Path) -> Result<File> {
    safe(dir, true)?;
    let path = dir.join("lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    safe(&path, false)?;
    let until = Instant::now() + Duration::from_secs(1);
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        let e = std::io::Error::last_os_error();
        ensure!(
            e.kind() == std::io::ErrorKind::WouldBlock,
            "Codex custody flock: {e}"
        );
        ensure!(Instant::now() < until, "Codex custody lock timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn identity(pid: u32) -> Result<Value> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let fields: Vec<_> = stat
        .rsplit_once(") ")
        .context("native process stat")?
        .1
        .split_whitespace()
        .collect();
    ensure!(fields.first() != Some(&"Z"), "native process exited");
    Ok(
        json!({"bootId": fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim(),
        "startTicks": fields.get(19).context("native start time")?.parse::<u64>()?}),
    )
}

fn executable(pid: u32) -> Result<Value> {
    let path = PathBuf::from(format!("/proc/{pid}/exe"));
    let target = fs::read_link(&path)?;
    let m = fs::metadata(&path)?;
    Ok(json!({"path": target, "device": m.dev(), "inode": m.ino()}))
}

fn pidfd(pid: u32) -> Result<OwnedFd> {
    #[cfg(target_os = "linux")]
    {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        ensure!(fd >= 0, "Codex custody requires pidfd_open");
        Ok(unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd as i32) })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        anyhow::bail!("Codex custody requires Linux");
    }
}

fn live(fd: &OwnedFd) -> Result<bool> {
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    ensure!(
        unsafe { libc::poll(&mut poll, 1, 0) } >= 0,
        "Codex custody pidfd poll failed"
    );
    Ok(poll.revents == 0)
}

/// Synchronous holder revocation, called while the SQLite writer is held.
/// Other ships/homes and projects without a custody record are untouched.
pub(crate) fn holder_changed(store: &Store, ship: &str) -> Result<()> {
    if crate::presence::folder_ship(store).as_deref() != Some(ship) {
        return Ok(());
    }
    let dir = store
        .home()
        .parent()
        .context("ship folder")?
        .join(".codex/privateer-attended");
    if dir.join("launch.json").try_exists()? || dir.join("custody.json").try_exists()? {
        revoke(&dir, "holder_changed")?;
    }
    Ok(())
}

fn revoke(dir: &Path, reason: &str) -> Result<()> {
    // The sticky token is already unusable even if flock/registry update fails.
    poison(dir, reason)?;
    let update = || -> Result<()> {
        let _lock = lock(dir)?;
        if !dir.join("custody.json").try_exists()? {
            return Ok(());
        }
        let mut custody = read(&dir.join("custody.json"))?;
        custody["state"] = json!("revoked");
        custody["reason"] = json!(reason);
        atomic(dir, "custody.json", &custody)
    };
    if let Err(error) = update() {
        // The durable poison is the safety boundary. A diagnostic rewrite must
        // not roll back attend/leave/handover and strand the presence lease.
        eprintln!("Codex custody poisoned ({reason}); record update failed: {error:#}");
        // Publish a NEW revoked token, never rewrite a token read earlier: a
        // pre-lock callback may already have replaced it. Failure here leaves
        // the first poison (or a missing token), neither of which grants custody.
        let detail: String = format!("{error:#}").chars().take(512).collect();
        if let Err(error) = poison_record(dir, reason, Some(&detail)) {
            eprintln!("Codex custody diagnostic persistence failed: {error:#}");
        }
    }
    Ok(())
}

/// Only constructed by the hosted supervisor while it owns the unreaped Child.
/// Never reconstructed from a registry PID. Drop revokes on supervision errors.
pub(crate) struct Custody {
    dir: PathBuf,
    pid: u32,
    process: Value,
    fd: OwnedFd,
    held: Option<Value>,
    revoked: bool,
}

impl Custody {
    pub(crate) fn new(child: &Child, folder: &Path) -> Result<Self> {
        let fd = pidfd(child.id())?;
        let process = identity(child.id())?;
        ensure!(live(&fd)?, "Codex pane already exited");
        Ok(Self {
            dir: folder.join(".codex/privateer-attended"),
            pid: child.id(),
            process,
            fd,
            held: None,
            revoked: false,
        })
    }

    pub(crate) fn observe(&mut self, store: &Store, ship: &str, holder: &str) -> Result<()> {
        if self.revoked {
            return Ok(());
        }
        if !live(&self.fd)? {
            return self.revoke("pane_exit");
        }
        // SQLite reads precede flock. No RPC or wait operation under the lock.
        let presence = crate::presence::session(store, ship)?;
        if !presence.is_some_and(|p| p.holder == holder && !p.asleep) {
            return self.revoke("holder_changed");
        }
        if !self.dir.join("launch.json").try_exists()? {
            return Ok(());
        }
        let _lock = lock(&self.dir)?;
        let record = read(&self.dir.join("launch.json"))?;
        if self.held.is_none()
            && (record["pid"] != self.pid || record["process_identity"] != self.process)
        {
            return Ok(()); // Old launch or wrapper not prepared yet: never adopt it.
        }
        if let Some(held) = &self.held {
            let valid = record["generation"] == held["generation"]
                && record["session"] == held["session"]
                && record["holder"] == held["holder"]
                && record["home"] == held["home"]
                && record["cwd"] == held["cwd"]
                && record["codex_home"] == held["codex_home"]
                && record["pid"] == self.pid
                && record["process_identity"] == self.process
                && executable(self.pid)? == held["executable"]
                && identity(self.pid)? == self.process
                && read(&self.dir.join("custody-poison.json"))?["token"] == held["token"]
                && daemon(&record, &held["daemon"])?;
            drop(_lock);
            if !valid {
                self.revoke("identity_or_daemon_changed")?;
            }
            return Ok(());
        }
        if record["state"] != "bound" {
            return Ok(());
        }
        ensure!(
            record["hosted"] == true
                && record["holder"] == holder
                && record["ship"] == ship
                && fs::canonicalize(record["home"].as_str().context("Codex custody home")?)
                    .context("resolve Codex custody home")?
                    == fs::canonicalize(store.home()).context("resolve Privateer home")?,
            "Codex custody launch context mismatch"
        );
        let exe = executable(self.pid)?;
        // prepare runs before exec: do not mistake the Python wrapper for Codex.
        if record["executable"] != exe["path"] {
            return Ok(());
        }
        ensure!(
            identity(self.pid)? == self.process && live(&self.fd)?,
            "Codex pane identity changed"
        );
        let binding = &record["binding"];
        ensure!(
            binding["session"] == record["session"]
                && binding["fence_version"] == 1
                && read(&self.dir.join("invalidate.json"))?["token"]
                    == binding["invalidation_token"],
            "Codex binding is poisoned"
        );
        let socket = fs::symlink_metadata(binding["socket"].as_str().context("daemon socket")?)?;
        let native = json!({"pid": binding["daemon_pid"], "identity": binding["daemon_identity"],
            "executable": executable(binding["daemon_pid"].as_u64().context("daemon pid")? as u32)?,
            "device": socket.dev(), "inode": socket.ino(),
            "socket": binding["socket"], "rendezvous": binding["rendezvous"]});
        ensure!(daemon(&record, &native)?, "Codex daemon identity changed");
        let fence = read(&self.dir.join("custody-poison.json"))?;
        ensure!(
            fence["generation"] == record["generation"] && fence.get("reason").is_none(),
            "Codex custody already poisoned"
        );
        let token = fence["token"].as_str().context("custody token")?;
        ensure!(
            uuid::Uuid::parse_str(token)?.to_string() == token,
            "invalid custody token"
        );
        let held = json!({"version": 1, "state": "owned", "token": token,
            "epoch": uuid::Uuid::new_v4().to_string(), "generation": record["generation"], "session": record["session"],
            "home": record["home"], "cwd": record["cwd"], "codex_home": record["codex_home"],
            "holder": holder, "ship": ship, "pid": self.pid, "process_identity": self.process,
            "executable": exe, "daemon": native, "supervisor_pid": std::process::id(),
            "supervisor_identity": identity(std::process::id())?,
            "supervisor_executable": executable(std::process::id())?, "delivery_enabled": false});
        // Never mint a fresh token on callbacks, daemon replacement or holder change.
        atomic(&self.dir, "custody.json", &held)?;
        self.held = Some(held);
        Ok(())
    }

    pub(crate) fn retire(&mut self, reason: &str) {
        if let Err(error) = self.revoke(reason) {
            eprintln!("Codex custody revocation failed; delivery must refuse: {error:#}");
        }
    }

    pub(crate) fn revoke(&mut self, reason: &str) -> Result<()> {
        if self.revoked {
            return Ok(());
        }
        if self.held.is_some() {
            revoke(&self.dir, reason)?;
        }
        // A failed poison write must remain retryable by observe/retire/Drop.
        self.revoked = true;
        Ok(())
    }
}

impl Drop for Custody {
    fn drop(&mut self) {
        let _ = self.revoke("supervision_lost");
    }
}

fn daemon(record: &Value, expected: &Value) -> Result<bool> {
    let pid = expected["pid"].as_u64().context("daemon pid")? as u32;
    let home = Path::new(record["codex_home"].as_str().context("Codex home")?);
    let current = read(&home.join("app-server-daemon/daemon.pid"))?;
    let socket = Path::new(expected["socket"].as_str().context("daemon socket")?);
    let rendezvous = Path::new(
        expected["rendezvous"]
            .as_str()
            .context("daemon rendezvous")?,
    );
    let meta = fs::symlink_metadata(socket)?;
    Ok(current["pid"] == pid
        && current["processIdentity"] == expected["identity"]
        && identity(pid)? == expected["identity"]
        && executable(pid)? == expected["executable"]
        && fs::read_link(rendezvous)? == socket
        && meta.file_type().is_socket()
        && meta.uid() == unsafe { libc::geteuid() }
        && meta.mode() & 0o077 == 0
        && json!(meta.dev()) == expected["device"]
        && json!(meta.ino()) == expected["inode"])
}

// Custody needs pidfd_open and /proc, so its tests run only where custody can exist.
#[cfg(all(test, target_os = "linux"))]
mod tests;

#[cfg(all(test, not(target_os = "linux")))]
#[test]
fn codex_custody_non_linux_refuses_without_changing_presence() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(&tmp.path().join(".privateer")).unwrap();
    fs::write(store.home().join("config.json"), br#"{"ship":"alpha"}"#).unwrap();
    store
        .connection()
        .execute(
            "INSERT INTO ships(name,record) VALUES('alpha','{\"location\":\"here\"}')",
            [],
        )
        .unwrap();
    crate::presence::attend(&store, "alpha", "host:alpha", 90, None).unwrap();
    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let result = Custody::new(&child, tmp.path());
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("Codex custody requires Linux")
    );
    holder_changed(&store, "alpha").unwrap();
    assert_eq!(
        crate::presence::session(&store, "alpha")
            .unwrap()
            .unwrap()
            .holder,
        "host:alpha"
    );
    assert!(!tmp.path().join(".codex/privateer-attended").exists());
}
