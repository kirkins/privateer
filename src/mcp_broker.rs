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
/// How a Mac opens a workspace's window: `-g` keeps the captain's focus.
#[cfg(target_os = "macos")]
const DEFAULT_WINDOW_OPEN: &[&str] = &["/usr/bin/open", "-g", "-a", "Xcode"];
/// How a Mac closes it: every workspace document whose path is the recorded
/// project or lies below it (Xcode can report `X.xcodeproj` as
/// `X.xcodeproj/project.xcworkspace`), last first so closing one does not
/// shift the others. The project path is the script's argument, never part
/// of the script; nothing matching exits 0; and an Xcode that is not running
/// is never launched.
#[cfg(any(target_os = "macos", test))]
const DEFAULT_WINDOW_CLOSE: &[&str] = &[
    "/usr/bin/osascript",
    "-e",
    "on run argv",
    "-e",
    "set wantedPath to item 1 of argv",
    "-e",
    "set wantedPrefix to wantedPath & \"/\"",
    "-e",
    "if application \"Xcode\" is running then",
    "-e",
    "tell application \"Xcode\"",
    "-e",
    "set docs to every workspace document",
    "-e",
    "repeat with i from (count of docs) to 1 by -1",
    "-e",
    "set p to path of (item i of docs)",
    "-e",
    "if p is not missing value and (p is wantedPath or p starts with wantedPrefix) then close (item i of docs) saving no",
    "-e",
    "end repeat",
    "-e",
    "end tell",
    "-e",
    "end if",
    "-e",
    "end run",
];
/// Executables a window helper may not be: a shell would run anything.
const SHELLS: [&str; 12] = [
    "sh", "bash", "zsh", "dash", "ksh", "mksh", "fish", "csh", "tcsh", "env", "busybox", "xargs",
];
/// The longest a release waits for the broker to close a window: longer than
/// the broker's 30 s helper deadline, so a close helper the broker kills is
/// reported as the broker's error, not as silence.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(35);
/// The dispatcher looks at most this often.
const ENSURE_EVERY: Duration = Duration::from_secs(30);
const USAGE: &str = "usage: privateer mcp-broker install [--workspace-pool ABS]... [--window-open JSON_ARGV] [--window-close JSON_ARGV] [--upstream /ABS/COMMAND [ARG...]] | status | stop";

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
    windows: PathBuf,
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
            windows: at("windows.json"),
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
            install(&files, &install_options(home, &args[1..])?)?
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

/// What `install` records: the upstream, the workspace pools and the window
/// helpers.
struct Config {
    upstream: Vec<String>,
    pools: Vec<PathBuf>,
    window_open: Option<Vec<String>>,
    window_close: Option<Vec<String>>,
}

impl Config {
    fn json(&self) -> Value {
        let mut config = json!({"upstream": self.upstream, "workspace_pools": self.pools});
        if let Some(open) = &self.window_open {
            config["window_open"] = json!(open);
        }
        if let Some(close) = &self.window_close {
            config["window_close"] = json!(close);
        }
        config
    }

    /// A config `install` wrote, checked again before it is run.
    fn parse(config: &Value) -> Result<Self> {
        let upstream: Vec<String> = serde_json::from_value(config["upstream"].clone())
            .context("mcp-broker config has no upstream argv")?;
        check_upstream(&upstream)?;
        let pools: Vec<PathBuf> = match config.get("workspace_pools") {
            Some(pools) => serde_json::from_value(pools.clone())
                .context("mcp-broker config workspace_pools is not a list of paths")?,
            None => vec![],
        };
        for pool in &pools {
            ensure!(
                normalized(pool),
                "workspace pool {} is not absolute and normalized",
                pool.display()
            );
        }
        let helper = |key: &str| -> Result<Option<Vec<String>>> {
            config
                .get(key)
                .map(|argv| {
                    let argv: Vec<String> = serde_json::from_value(argv.clone())
                        .with_context(|| format!("mcp-broker config {key} is not an argv"))?;
                    check_helper(&argv)?;
                    Ok(argv)
                })
                .transpose()
        };
        Ok(Self {
            upstream,
            pools,
            window_open: helper("window_open")?,
            window_close: helper("window_close")?,
        })
    }
}

/// `install`'s arguments. The workspace pool Privateer itself uses,
/// `<ledger>/worktrees`, is always there; `--workspace-pool` adds others,
/// each absolute, normalized and existing. The helpers default to the Mac's
/// `open` and `osascript`, and to none elsewhere.
fn install_options(home: Option<&Path>, args: &[String]) -> Result<Config> {
    let own = crate::jobs::ledger_dir_at(home)?.join("worktrees");
    ensure!(own.is_absolute(), "resource_home must be absolute");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&own)
        .with_context(|| format!("cannot make {}", own.display()))?;
    let mut pools = vec![fs::canonicalize(&own)?];
    #[cfg(target_os = "macos")]
    let defaults = |argv: &[&str]| Some(argv.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    #[cfg(target_os = "macos")]
    let (mut window_open, mut window_close) = (
        defaults(DEFAULT_WINDOW_OPEN),
        defaults(DEFAULT_WINDOW_CLOSE),
    );
    #[cfg(not(target_os = "macos"))]
    let (mut window_open, mut window_close) = (None, None);
    let mut upstream: Vec<String> = DEFAULT_UPSTREAM.iter().map(|s| s.to_string()).collect();
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let mut value = || rest.next().cloned().context(USAGE);
        match flag.as_str() {
            "--workspace-pool" => {
                let pool = PathBuf::from(value()?);
                ensure!(
                    normalized(&pool),
                    "--workspace-pool {} must be an absolute, normalized path",
                    pool.display()
                );
                ensure!(
                    pool.is_dir(),
                    "--workspace-pool {} does not exist",
                    pool.display()
                );
                let real = fs::canonicalize(&pool)?;
                ensure!(pools.len() < 16, "at most 16 workspace pools");
                if !pools.contains(&real) {
                    pools.push(real);
                }
            }
            "--window-open" | "--window-close" => {
                let argv: Vec<String> = serde_json::from_str(&value()?)
                    .with_context(|| format!("{flag} takes a JSON array of strings"))?;
                check_helper(&argv)?;
                if flag == "--window-open" {
                    window_open = Some(argv);
                } else {
                    window_close = Some(argv);
                }
            }
            "--upstream" => {
                upstream = rest.by_ref().cloned().collect();
                ensure!(!upstream.is_empty(), USAGE);
            }
            _ => bail!(USAGE),
        }
    }
    check_upstream(&upstream)?;
    Ok(Config {
        upstream,
        pools,
        window_open,
        window_close,
    })
}

/// Absolute, with no `.`, `..` or trailing slash.
fn normalized(path: &Path) -> bool {
    let text = path.to_string_lossy();
    path.is_absolute()
        && path.components().all(|c| {
            matches!(
                c,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
        && (text == "/" || !text.ends_with('/'))
        && !text.contains("//")
        && !text.split('/').any(|part| part == "." || part == "..")
}

/// A window helper: checked like the upstream, and never a shell.
fn check_helper(argv: &[String]) -> Result<()> {
    check_upstream(argv).context("a window helper is checked like the upstream")?;
    let base = Path::new(&argv[0])
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    ensure!(
        !SHELLS.contains(&base.as_str()),
        "a window helper may not be {base}"
    );
    Ok(())
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
fn install(files: &Files, wanted: &Config) -> Result<Value> {
    let upstream = &wanted.upstream;
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
    let config = serde_json::to_vec(&wanted.json())?;
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
        "workspace_pools": wanted.pools,
        "window_open": wanted.window_open,
        "window_close": wanted.window_close,
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
        "workspace_pools": read(&files.config)["workspace_pools"],
        // The windows the broker opened and will close on release.
        "windows": read(&files.windows)["windows"],
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
    // On macOS a session can belong to launchd (pid 1), so a broker that
    // is init would match every process in it.
    broker > 1
        && pgid > 1
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

/// What asking the broker to close a workspace's windows came to.
#[derive(Debug)]
pub enum CloseOutcome {
    /// The broker closed these projects' windows; none when the record
    /// lists no window under the root, in which case it was not asked.
    Closed(Vec<PathBuf>),
    /// No broker runs; any window it opened stays open (and is audited).
    NotRunning,
}

/// The windows `xcode.windows.json` records under `root`, read under the
/// same link and owner checks as every broker file. Unreadable means none.
fn recorded_under(files: &Files, root: &Path) -> Vec<Value> {
    read_checked(&files.windows)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|record| record["windows"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|w| {
            w["root"]
                .as_str()
                .is_some_and(|r| Path::new(r).starts_with(root))
        })
        .collect()
}

/// Ask the running broker to close every Xcode window it opened under
/// `root` (`{"privateer_control":1,"close_workspace":ROOT}`), within 35 s.
/// The broker is asked only when its record lists a window under `root`,
/// so a broker that hangs holds up only the release of a workspace it
/// opened a window for. A broker that answers with an error, or does not
/// answer in time, is an error.
pub(crate) fn close_workspace(store: &Store, root: &Path) -> Result<CloseOutcome> {
    // No run directory this home could use means no broker to ask.
    let Ok(files) = Files::resolve(Some(store.home())) else {
        return Ok(CloseOutcome::NotRunning);
    };
    let left = recorded_under(&files, root);
    if left.is_empty() {
        return Ok(CloseOutcome::Closed(vec![]));
    }
    if fs::symlink_metadata(&files.socket).is_err() || !lock_held(&files.lock) {
        store.audit(
            "workspace.xcode_window_unclosed",
            &json!({"root": root, "windows": left, "reason": "the MCP broker is not running"}),
        )?;
        return Ok(CloseOutcome::NotRunning);
    }
    let deadline = Instant::now() + CLOSE_TIMEOUT;
    let silent = || {
        format!(
            "the MCP broker did not answer within {} s",
            CLOSE_TIMEOUT.as_secs()
        )
    };
    let left = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .with_context(silent)
    };
    let mut stream = connect_by(&files.socket, deadline).map_err(|error| {
        if error.kind() == io::ErrorKind::TimedOut {
            anyhow::anyhow!(
                "could not connect to the MCP broker at {} within {} s",
                files.socket.display(),
                CLOSE_TIMEOUT.as_secs()
            )
        } else {
            anyhow::anyhow!(
                "cannot reach the MCP broker at {}: {error}",
                files.socket.display()
            )
        }
    })?;
    ensure!(
        peer_uid(stream.as_raw_fd())? == unsafe { libc::geteuid() },
        "the MCP broker socket is not this user's"
    );
    stream.set_write_timeout(Some(left()?))?;
    let line = json!({"privateer_control": 1, "close_workspace": root});
    io::Write::write_all(&mut stream, format!("{line}\n").as_bytes())
        .context("cannot write to the MCP broker")?;
    let mut answer = vec![];
    let mut byte = [0u8; 4096];
    while !answer.contains(&b'\n') {
        stream.set_read_timeout(Some(left()?))?;
        let read = match io::Read::read(&mut stream, &mut byte) {
            Ok(0) => bail!("the MCP broker closed the connection without answering"),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                bail!(silent())
            }
            Err(error) => return Err(error).context("cannot read the MCP broker's answer"),
        };
        answer.extend_from_slice(&byte[..read]);
        ensure!(
            answer.len() <= 1 << 20,
            "the MCP broker's answer is too long"
        );
    }
    let end = answer
        .iter()
        .position(|b| *b == b'\n')
        .unwrap_or(answer.len());
    let answer: Value =
        serde_json::from_slice(&answer[..end]).context("the MCP broker's answer is not JSON")?;
    if let Some(error) = answer.get("error") {
        bail!("{}", error.as_str().unwrap_or("the MCP broker refused"));
    }
    let closed: Vec<PathBuf> = serde_json::from_value(answer["closed"].clone())
        .context("the MCP broker's answer names no closed windows")?;
    Ok(CloseOutcome::Closed(closed))
}

/// Connect to the Unix socket at `path` by `deadline` without ever
/// blocking past it: a blocking connect waits as long as the listener's
/// backlog stays full, which a hung broker never drains. The stream comes
/// back blocking.
// sun_path is i8 on some targets and u8 on others; sa_family_t's width
// differs between Linux and macOS.
#[allow(clippy::unnecessary_cast)]
fn connect_by(path: &Path, deadline: Instant) -> io::Result<std::os::unix::net::UnixStream> {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: an all-zero sockaddr_un is a valid, empty address.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the socket path is too long",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let length = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    {
        address.sun_len = length as u8;
    }
    // SAFETY: socket returns a new descriptor or -1.
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a new descriptor nothing else owns.
    let socket = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: fcntl on a descriptor this function owns.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    let set = unsafe {
        libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) == 0
            && libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) == 0
    };
    if flags < 0 || !set {
        return Err(io::Error::last_os_error());
    }
    let timed_out = || io::Error::new(io::ErrorKind::TimedOut, "connect timed out");
    loop {
        // SAFETY: `address` is a valid sockaddr_un of `length` bytes.
        let done = unsafe {
            libc::connect(
                raw,
                (&address as *const libc::sockaddr_un).cast(),
                length as libc::socklen_t,
            )
        };
        if done == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => {}
            // Linux: the backlog is full. Try again until the deadline.
            Some(libc::EAGAIN) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(timed_out());
                }
                std::thread::sleep(left.min(Duration::from_millis(50)));
            }
            // Pending: wait for it to become writable, then read its outcome.
            Some(libc::EINPROGRESS) => {
                let mut poll = libc::pollfd {
                    fd: raw,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                loop {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(timed_out());
                    }
                    let millis = left.as_millis().clamp(1, 60_000) as libc::c_int;
                    // SAFETY: one valid pollfd.
                    match unsafe { libc::poll(&mut poll, 1, millis) } {
                        0 => {}
                        n if n > 0 => break,
                        _ if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
                        _ => return Err(io::Error::last_os_error()),
                    }
                }
                let mut code: libc::c_int = 0;
                let mut size = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
                // SAFETY: `code` and `size` describe a writable int.
                let got = unsafe {
                    libc::getsockopt(
                        raw,
                        libc::SOL_SOCKET,
                        libc::SO_ERROR,
                        (&mut code as *mut libc::c_int).cast(),
                        &mut size,
                    )
                };
                if got != 0 {
                    return Err(io::Error::last_os_error());
                }
                if code != 0 {
                    return Err(io::Error::from_raw_os_error(code));
                }
                break;
            }
            _ => return Err(error),
        }
    }
    // SAFETY: fcntl on a descriptor this function owns.
    if unsafe { libc::fcntl(raw, libc::F_SETFL, flags & !libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(std::os::unix::net::UnixStream::from(socket))
}

/// The uid of a connected Unix socket's peer.
fn peer_uid(fd: std::os::fd::RawFd) -> Result<u32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
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
        ensure!(
            done == 0,
            "no peer credentials: {}",
            io::Error::last_os_error()
        );
        Ok(cred.uid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let mut uid = 0;
        let mut gid = 0;
        // SAFETY: both out-pointers are valid.
        let done = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
        ensure!(
            done == 0,
            "no peer credentials: {}",
            io::Error::last_os_error()
        );
        Ok(uid)
    }
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
    let config = Config::parse(&serde_json::from_slice(&bytes)?)?;
    private_dir(&files.dir)?;
    Ok(json!({"started": start(&binary, &files, &config)?}))
}

/// Start `serve` detached: its own session, stdio on the log, no descriptor
/// of the dispatcher's, and only the environment its upstream may see.
fn start(binary: &Path, files: &Files, config: &Config) -> Result<u32> {
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
        .arg(NAME);
    for pool in &config.pools {
        command.arg("--workspace-pool").arg(pool);
    }
    for (flag, helper) in [
        ("--window-open", &config.window_open),
        ("--window-close", &config.window_close),
    ] {
        if let Some(argv) = helper {
            command.arg(flag).arg(serde_json::to_string(argv)?);
        }
    }
    command
        .arg("--")
        .args(&config.upstream)
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
    fn a_full_backlog_never_blocks_a_connect_past_its_deadline() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("full.sock");
        // A listener that never accepts, with the smallest backlog.
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let mut held = vec![];
        let started = Instant::now();
        let failed = loop {
            let attempt = Instant::now();
            match connect_by(&path, attempt + Duration::from_millis(300)) {
                Ok(stream) => held.push(stream),
                Err(error) => break (error, attempt.elapsed()),
            }
            assert!(held.len() < 512, "the backlog never filled");
        };
        assert!(failed.1 < Duration::from_secs(2), "{failed:?}");
        assert!(started.elapsed() < Duration::from_secs(20));
        drop(listener);
        // Nothing listening: refused at once.
        let gone = connect_by(
            &temp.path().join("none.sock"),
            Instant::now() + CLOSE_TIMEOUT,
        );
        assert!(gone.is_err());
    }

    #[test]
    fn the_mac_close_script_is_a_valid_helper_that_never_interpolates() {
        let argv: Vec<String> = DEFAULT_WINDOW_CLOSE.iter().map(|s| s.to_string()).collect();
        check_helper(&argv).unwrap();
        assert!(
            argv.iter()
                .all(|line| !line.contains('{') && !line.contains("do shell"))
        );
        assert!(
            argv.iter()
                .any(|line| line == "set wantedPath to item 1 of argv")
        );
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
