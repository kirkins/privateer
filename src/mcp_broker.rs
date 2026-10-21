//! The MCP broker's machine side: installing its stable binary, reporting
//! it, stopping it, and the machine owner's dispatcher keeping it running.
//! The broker itself (`src/bin/privateer-mcp-broker.rs`) never links this
//! library. See docs/mcp-broker.md.
//!
//! The binary lives at one unversioned path, so a Privateer upgrade changes
//! neither its path nor its hash, and a started broker outlives dispatcher
//! restarts: whatever the upstream allowed (Xcode's Allow) stays allowed.
use crate::store::{Caller, Store};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const BINARY: &str = "privateer-mcp-broker";
/// The one broker this step runs.
pub const NAME: &str = "xcode";
const DEFAULT_UPSTREAM: [&str; 2] = ["/usr/bin/xcrun", "mcpbridge"];
/// The dispatcher looks at most this often.
const ENSURE_EVERY: Duration = Duration::from_secs(30);
const USAGE: &str =
    "usage: privateer mcp-broker install [--upstream /ABS/COMMAND [ARG...]] | status | stop";

/// `$XDG_DATA_HOME/privateer/mcp-broker/bin/privateer-mcp-broker`: outside
/// every release, so upgrades leave it alone.
pub fn stable_binary() -> Result<PathBuf> {
    Ok(crate::releases::data_dir()?
        .join("mcp-broker/bin")
        .join(BINARY))
}

/// The sha256 of the config `install` wrote, kept beside the stable binary
/// and outside the ledger, which sandboxed agents may write. The dispatcher
/// runs the config's upstream only while it still matches, so an agent that
/// rewrites the config cannot have the dispatcher run its own command.
fn config_pin() -> Result<PathBuf> {
    Ok(crate::releases::data_dir()?.join(format!("mcp-broker/{NAME}.json.sha256")))
}

/// `$XDG_DATA_HOME/privateer/mcp-broker/run`: the broker's lock, pid,
/// state, log, socket and config. Sandboxed agents may write the ledger and
/// the ship homes, never this; and a crew connects to the socket here
/// without being able to replace it (docs/mcp-broker.md).
pub fn run_dir() -> Result<PathBuf> {
    Ok(crate::releases::data_dir()?.join("mcp-broker/run"))
}

/// The broker's files in the run directory.
struct Files {
    dir: PathBuf,
    config: PathBuf,
    stopped: PathBuf,
    lock: PathBuf,
    pid: PathBuf,
    socket: PathBuf,
    state: PathBuf,
    log: PathBuf,
}

impl Files {
    fn new(dir: PathBuf) -> Self {
        let at = |suffix: &str| dir.join(format!("{NAME}.{suffix}"));
        Self {
            config: at("json"),
            stopped: at("stopped"),
            lock: at("lock"),
            pid: at("pid"),
            socket: at("sock"),
            state: at("state.json"),
            log: at("log"),
            dir,
        }
    }

    /// The run directory, refused when it lies inside a directory agents
    /// may write: the ledger or the ship's home.
    fn resolve(home: Option<&Path>) -> Result<Self> {
        let dir = run_dir()?;
        let ledger = crate::jobs::ledger_dir_at(home)?;
        for writable in [Some(ledger.as_path()), home].into_iter().flatten() {
            ensure!(
                !inside(&dir, writable),
                "the MCP broker's run directory {} lies inside {}, which agents may write; set XDG_DATA_HOME elsewhere",
                dir.display(),
                writable.display()
            );
        }
        Ok(Self::new(dir))
    }
}

/// Whether `path` is `parent` or below it, as written or once resolved.
fn inside(path: &Path, parent: &Path) -> bool {
    let resolved = |p: &Path| {
        // The deepest existing ancestor, resolved, with the rest appended.
        let mut existing = p.to_path_buf();
        let mut rest = vec![];
        while !existing.exists() {
            match (
                existing.file_name().map(ToOwned::to_owned),
                existing.parent(),
            ) {
                (Some(name), Some(up)) => {
                    rest.push(name);
                    existing = up.to_path_buf();
                }
                _ => break,
            }
        }
        let mut out = fs::canonicalize(&existing).unwrap_or(existing);
        out.extend(rest.into_iter().rev());
        out
    };
    path.starts_with(parent) || resolved(path).starts_with(resolved(parent))
}

pub fn cli(home: Option<&Path>, args: &[String], caller: &Caller) -> Result<()> {
    let files = Files::resolve(home)?;
    let out = match args.first().map(String::as_str) {
        Some("install") => {
            caller.require_operator("mcp-broker install")?;
            install(&files, &upstream_from(&args[1..])?)?
        }
        Some("status") | None if args.len() <= 1 => status(&files)?,
        Some("stop") if args.len() == 1 => {
            caller.require_operator("mcp-broker stop")?;
            // First, so the dispatcher does not start it again meanwhile.
            private_dir(&files.dir)?;
            write_private(&files.stopped, b"")
                .with_context(|| format!("cannot write {}", files.stopped.display()))?;
            json!({"stopped": stop_running(&files)?, "held_until": "mcp-broker install"})
        }
        _ => bail!(USAGE),
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn upstream_from(args: &[String]) -> Result<Vec<String>> {
    let upstream: Vec<String> = match args.split_first() {
        None => DEFAULT_UPSTREAM.iter().map(|s| s.to_string()).collect(),
        Some((flag, rest)) if flag == "--upstream" && !rest.is_empty() => rest.to_vec(),
        _ => bail!(USAGE),
    };
    check_upstream(&upstream)?;
    Ok(upstream)
}

/// An argv whose executable is an absolute, normalized path.
fn check_upstream(upstream: &[String]) -> Result<()> {
    ensure!(
        (1..=32).contains(&upstream.len()),
        "the upstream command has 1 to 32 arguments"
    );
    ensure!(
        upstream
            .iter()
            .all(|a| a.len() <= 4096 && !a.chars().any(char::is_control)),
        "an upstream argument is too long or holds a control character"
    );
    let program = Path::new(&upstream[0]);
    ensure!(
        program.is_absolute()
            && program.components().all(|c| matches!(
                c,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            ))
            && !upstream[0].ends_with('/'),
        "the upstream command must start with an absolute, normalized path"
    );
    Ok(())
}

/// The broker this Privateer was shipped with: its release's, or the one
/// beside a source build.
fn source_binary() -> Result<PathBuf> {
    let image = crate::releases::running_image()?;
    let path = match crate::releases::Release::from_executable(&image) {
        Some(release) => release.binary(BINARY)?,
        None => image.with_file_name(BINARY),
    };
    ensure!(
        path.is_file(),
        "no {BINARY} beside this Privateer at {}",
        path.display()
    );
    Ok(path)
}

/// What `BINARY --version` prints, trimmed.
fn version_of(path: &Path) -> Result<String> {
    let output = Command::new(path)
        .arg("--version")
        .env_clear()
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("cannot run {} --version", path.display()))?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    ensure!(
        output.status.success() && text.starts_with(&format!("{BINARY} ")) && text.len() <= 100,
        "{} does not report a broker version",
        path.display()
    );
    Ok(text)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    io::copy(&mut File::open(path)?, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Make the run directory (0700, ours, not a symlink, nor its parent).
fn private_dir(dir: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    for part in [dir.parent().context("run directory parent")?, dir] {
        let info = fs::symlink_metadata(part)?;
        ensure!(
            info.is_dir() && info.uid() == unsafe { libc::geteuid() },
            "{} is not a directory this user owns",
            part.display()
        );
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Open a broker file without following a symlink (and without blocking on
/// a FIFO), then refuse anything but a regular file this user owns with
/// one link. The run directory is private; nothing here relies on that alone.
fn open_checked(path: &Path, options: &mut OpenOptions) -> io::Result<File> {
    let file = options
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let info = file.metadata()?;
    // SAFETY: geteuid cannot fail.
    if !info.is_file() || info.uid() != unsafe { libc::geteuid() } || info.nlink() != 1 {
        return Err(io::Error::other(format!(
            "{} is not a private regular file",
            path.display()
        )));
    }
    Ok(file)
}

/// A broker file's bytes, through `open_checked`, at most 1 MiB.
fn read_checked(path: &Path) -> io::Result<Vec<u8>> {
    let file = open_checked(path, OpenOptions::new().read(true))?;
    let mut bytes = vec![];
    io::Read::read_to_end(&mut io::Read::take(file, 1 << 20), &mut bytes)?;
    Ok(bytes)
}

/// Replace `path` through a new temporary file beside it (O_EXCL, 0600), so
/// neither a planted temp nor a symlink at `path` is ever written through.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path.file_name().context("file name")?.to_string_lossy();
    let temp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    // Unlinking only removes a leftover name, never what a link points at.
    let _ = fs::remove_file(&temp);
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temp)?;
        io::Write::write_all(&mut file, bytes)?;
        file.sync_all()?;
    }
    fs::rename(&temp, path)?;
    Ok(())
}

/// Copy the shipped broker to the stable path unless that already holds the
/// same version (then nothing changes, not even the inode), and record the
/// upstream the dispatcher starts it with.
fn install(files: &Files, upstream: &[String]) -> Result<Value> {
    let source = source_binary()?;
    let version = version_of(&source)?;
    let target = stable_binary()?;
    let installed = target.is_file().then(|| version_of(&target).ok()).flatten();
    let action = if installed.as_deref() == Some(version.as_str()) {
        "unchanged"
    } else {
        let dir = target.parent().context("stable broker directory")?;
        fs::create_dir_all(dir)?;
        let temp = dir.join(format!(".{BINARY}.{}.tmp", std::process::id()));
        let _ = fs::remove_file(&temp);
        {
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o755)
                .open(&temp)?;
            io::copy(&mut File::open(&source)?, &mut out)?;
            out.sync_all()?;
        }
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o755))?;
        fs::rename(&temp, &target)?;
        File::open(dir)?.sync_all()?;
        if installed.is_some() {
            "replaced"
        } else {
            "installed"
        }
    };
    private_dir(&files.dir)?;
    let config = serde_json::to_vec(&json!({"upstream": upstream}))?;
    write_private(
        &config_pin()?,
        format!("{:x}\n", Sha256::digest(&config)).as_bytes(),
    )?;
    write_private(&files.config, &config)?;
    let _ = fs::remove_file(&files.stopped);
    let sha256 = sha256_file(&target)?;
    Ok(json!({
        "action": action,
        "binary": target,
        "version": version,
        "sha256": sha256,
        "config": files.config,
        "upstream": upstream,
        "socket": files.socket,
        // What a crew bundle pins: this command, with `sha256`.
        "bundle_command": [target, "proxy", "--socket", files.socket],
        "running": lock_held(&files.lock),
        "note": "a running broker keeps its binary and upstream until it is stopped",
    }))
}

fn status(files: &Files) -> Result<Value> {
    let binary = stable_binary()?;
    let installed = binary.is_file();
    let running = lock_held(&files.lock);
    let read = |path: &Path| -> Value {
        read_checked(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(Value::Null)
    };
    Ok(json!({
        "binary": binary,
        "installed": installed,
        "version": installed.then(|| version_of(&binary).ok()).flatten(),
        "sha256": installed.then(|| sha256_file(&binary).ok()).flatten(),
        "run_dir": files.dir,
        "config": files.config,
        "upstream": read(&files.config)["upstream"],
        "stopped": files.stopped.exists(),
        "running": running,
        "pid": running.then(|| read_pid(&files.pid)).flatten(),
        "socket": files.socket,
        "log": files.log,
        // The broker's own report: upstream state, pid, restarts, clients.
        "broker": if running { read(&files.state) } else { Value::Null },
    }))
}

/// Whether a broker holds the lock. Holding it here, for the instant this
/// takes, never stops a broker start: serve retries for half a second.
fn lock_held(path: &Path) -> bool {
    let Ok(file) = open_checked(path, OpenOptions::new().read(true)) else {
        return false;
    };
    // SAFETY: a descriptor this function owns; closing it drops the lock.
    let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    !taken && io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK)
}

fn read_pid(path: &Path) -> Option<i32> {
    String::from_utf8(read_checked(path).ok()?)
        .ok()?
        .trim()
        .parse()
        .ok()
        .filter(|pid| *pid > 1)
}

/// Whether `pid` runs the broker binary, so a stale pid file never aims a
/// signal at someone else's process.
fn is_broker(pid: i32) -> bool {
    #[cfg(target_os = "linux")]
    {
        fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .and_then(|exe| exe.file_name().map(|n| n.to_string_lossy().into_owned()))
            .is_some_and(|name| name.starts_with(BINARY))
    }
    #[cfg(target_os = "macos")]
    {
        let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: the buffer is as long as the length passed.
        let len =
            unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
        len > 0
            && Path::new(&*String::from_utf8_lossy(&buffer[..len as usize]))
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(BINARY))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        true
    }
}

fn wait_unlocked(lock: &Path, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while lock_held(lock) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

/// The parent of `pid`, if it can be read.
fn parent_of(pid: i32) -> Option<i32> {
    #[cfg(target_os = "linux")]
    {
        // The fields after the command's closing parenthesis: state, ppid.
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: proc_pidinfo fills at most the struct's size.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        (got == size).then_some(info.pbi_ppid as i32)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// Whether `pgid` names the upstream of the verified broker `broker`: a
/// group leader in the broker's session (a dispatcher-started broker leads
/// its own) or the broker's child. Whatever the state file says, nothing
/// else is signalled.
fn upstream_group_of(broker: i32, pgid: i32) -> bool {
    // SAFETY: getpgid and getsid only read.
    let (group, session) = unsafe { (libc::getpgid(pgid), libc::getsid(pgid)) };
    pgid > 1
        && pgid != broker
        && group == pgid
        && (session == broker || parent_of(pgid) == Some(broker))
}

/// Stop a running broker and prove its upstream went with it: SIGTERM (the
/// broker ends its upstream's process group), then SIGKILL to both after
/// 10 s. Killing the broker alone is not proof: the upstream group is
/// checked by itself, and signalled only once it is verified as the
/// broker's (`upstream_group_of`), before the broker goes.
fn stop_running(files: &Files) -> Result<Value> {
    if !lock_held(&files.lock) {
        return Ok(json!({"was_running": false}));
    }
    let pid = read_pid(&files.pid).context("a broker holds the lock but left no pid")?;
    ensure!(
        is_broker(pid),
        "pid {pid} in {} is not the broker; not signalling it",
        files.pid.display()
    );
    let claimed = read_checked(&files.state)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|state| state["upstream"]["pid"].as_i64())
        .and_then(|pid| i32::try_from(pid).ok());
    // Verified while the broker still runs, so its parenthood can be seen.
    let upstream = claimed.filter(|pgid| upstream_group_of(pid, *pgid));
    let refused = claimed.filter(|_| upstream.is_none());
    // Checked again before SIGKILL. After the broker exits only its session
    // still ties the group to it: a broker that does not lead its own
    // session (one started by hand) leaves a surviving group unsignalled.
    let still_ours = |pgid: i32| upstream_group_of(pid, pgid);
    // SAFETY: kill has no memory effects.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let mut forced = false;
    if !wait_unlocked(&files.lock, Duration::from_secs(10)) {
        forced = true;
        unsafe { libc::kill(pid, libc::SIGKILL) };
        wait_unlocked(&files.lock, Duration::from_secs(3));
    }
    let group_alive = |pgid: i32| unsafe { libc::kill(-pgid, 0) } == 0;
    // Unknown (null) when no upstream group was verified.
    let mut upstream_gone = None;
    if let Some(pgid) = upstream {
        let deadline = Instant::now() + Duration::from_secs(3);
        while group_alive(pgid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        if group_alive(pgid) && still_ours(pgid) {
            forced = true;
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
            std::thread::sleep(Duration::from_millis(200));
        }
        upstream_gone = Some(!group_alive(pgid));
    }
    Ok(json!({
        "was_running": true,
        "pid": pid,
        "running": lock_held(&files.lock),
        "upstream_pid": upstream,
        "upstream_gone": upstream_gone,
        // A state-file pgid that is not the broker's upstream: never signalled.
        "unverified_upstream_pid": refused,
        "forced": forced,
    }))
}

/// When this process last looked.
static LAST_ENSURE: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

/// Keep the configured broker running, from the machine owner's dispatcher
/// only, at most every 30 s. It starts the installed binary when none runs,
/// never installs or replaces it, and stops a broker whose config is gone.
pub(crate) fn ensure(store: &Store) -> Value {
    if let Ok(mut last) = LAST_ENSURE.lock() {
        if last.is_some_and(|at| at.elapsed() < ENSURE_EVERY) {
            return Value::Null;
        }
        *last = Some(Instant::now());
    }
    ensure_now(store).unwrap_or_else(
        |error| json!({"error": format!("{error:#}").chars().take(2_000).collect::<String>()}),
    )
}

fn ensure_now(store: &Store) -> Result<Value> {
    if crate::machines::owner_here(store)?.is_none() {
        return Ok(Value::Null);
    }
    let files = Files::resolve(Some(store.home()))?;
    if !files.config.exists() {
        return Ok(if lock_held(&files.lock) {
            json!({"stopped": stop_running(&files)?})
        } else {
            Value::Null
        });
    }
    if files.stopped.exists() || lock_held(&files.lock) {
        return Ok(Value::Null);
    }
    let binary = stable_binary()?;
    if !binary.is_file() {
        return Ok(
            json!({"error": "the broker is configured but not installed: privateer mcp-broker install"}),
        );
    }
    let bytes = read_checked(&files.config)
        .with_context(|| format!("cannot read {}", files.config.display()))?;
    let pinned = read_checked(&config_pin()?)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    if pinned.trim() != format!("{:x}", Sha256::digest(&bytes)) {
        return Ok(
            json!({"error": "the broker config differs from what mcp-broker install wrote; not starting it"}),
        );
    }
    let config: Value = serde_json::from_slice(&bytes)?;
    let upstream: Vec<String> = serde_json::from_value(config["upstream"].clone())
        .context("mcp-broker config has no upstream argv")?;
    check_upstream(&upstream)?;
    private_dir(&files.dir)?;
    Ok(json!({"started": start(&binary, &files, &upstream)?}))
}

/// Start `serve` detached: its own session, stdio on the log, no descriptor
/// of the dispatcher's, and only the environment its upstream may see.
fn start(binary: &Path, files: &Files, upstream: &[String]) -> Result<u32> {
    let log = open_checked(
        &files.log,
        OpenOptions::new().create(true).append(true).mode(0o600),
    )
    .with_context(|| format!("cannot open {}", files.log.display()))?;
    let mut command = Command::new(binary);
    command
        .arg("serve")
        .arg("--state-dir")
        .arg(&files.dir)
        .arg("--name")
        .arg(NAME)
        .arg("--")
        .args(upstream)
        .current_dir(&files.dir)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    for (name, value) in std::env::vars_os() {
        let text = name.to_string_lossy();
        if matches!(&*text, "HOME" | "LANG" | "TMPDIR" | "DEVELOPER_DIR") || text.starts_with("LC_")
        {
            command.env(&name, value);
        }
    }
    let mut limit = unsafe { std::mem::zeroed::<libc::rlimit>() };
    // SAFETY: getrlimit fills the struct.
    let highest = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
        limit.rlim_cur.clamp(256, 65_536) as i32
    } else {
        1_024
    };
    // SAFETY: setsid, fcntl and close are async-signal-safe. Descriptors
    // marked close-on-exec (including std's own exec-status pipe) are left
    // for exec to close.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            for fd in 3..highest {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                    libc::close(fd);
                }
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot start {}", binary.display()))?;
    let pid = child.id();
    // Reaped here if it exits while this dispatcher runs; otherwise init
    // inherits it.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn writes_never_follow_a_planted_temp_or_target_link() {
        let temp = tempfile::tempdir().unwrap();
        let victim = temp.path().join("victim");
        fs::write(&victim, "untouched").unwrap();
        let path = temp.path().join("xcode.json");
        // The temp name this process would use, and the target, both links.
        let planted = temp
            .path()
            .join(format!(".xcode.json.{}.tmp", std::process::id()));
        symlink(&victim, &planted).unwrap();
        symlink(&victim, &path).unwrap();
        write_private(&path, b"{}").unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");
        assert!(fs::symlink_metadata(&path).unwrap().is_file());
        assert_eq!(read_checked(&path).unwrap(), b"{}");
        assert!(!planted.exists());
        // Reads refuse a link and a second hard link.
        let link = temp.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(read_checked(&link).is_err());
        fs::hard_link(&path, temp.path().join("hard")).unwrap();
        assert!(read_checked(&path).is_err());
    }

    #[test]
    fn only_the_brokers_own_group_counts_as_its_upstream() {
        let mut child = Command::new("sleep");
        child.arg("30").process_group(0);
        let mut child = child.spawn().unwrap();
        let pgid = child.id() as i32;
        let me = std::process::id() as i32;
        assert!(upstream_group_of(me, pgid), "this process's child");
        assert!(!upstream_group_of(1, pgid), "not init's");
        assert!(!upstream_group_of(me, me), "never the broker itself");
        let _ = child.kill();
        let _ = child.wait();
        assert!(!upstream_group_of(me, pgid), "gone");
    }
}
