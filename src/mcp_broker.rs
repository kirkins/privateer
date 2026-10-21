//! The MCP broker's machine side: installing its stable binary, reporting
//! its named instances, stopping them, the machine owner's dispatcher keeping
//! them running, and asking them to close a released workspace's windows.
//! The broker itself (`src/bin/privateer-mcp-broker.rs`) never links this
//! library, and this side names no use case: each instance's profile says
//! what its upstream is and how its windows route. See docs/mcp-broker.md.
//!
//! The binary lives at one unversioned path, so a Privateer upgrade changes
//! neither its path nor its hash, and a started broker outlives dispatcher
//! restarts: whatever the upstream allowed stays allowed.
use crate::store::{Caller, Store};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
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
/// The longest a release waits for the brokers to close a workspace's
/// windows, however many it asks: longer than the broker's 30 s helper
/// deadline, so a close helper the broker kills is reported as the broker's
/// error, not as silence.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(35);
/// macOS's sun_path: a socket path must be shorter than this.
const SUN_PATH: usize = 104;
/// The dispatcher looks at most this often.
const ENSURE_EVERY: Duration = Duration::from_secs(30);
const USAGE: &str = "usage: privateer mcp-broker install --name NAME [--profile FILE] [--workspace-pool ABS]... [--window-open JSON_ARGV] [--window-close JSON_ARGV] [--upstream /ABS/COMMAND [ARG...]] | status [--name NAME] | stop --name NAME";

/// An instance name: `^[a-z][a-z0-9-]{0,15}$`. It names every file of the
/// instance, so it can never hold a path.
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=16).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// `$XDG_DATA_HOME/privateer/mcp-broker/bin/privateer-mcp-broker`: outside
/// every release, so upgrades leave it alone.
pub fn stable_binary() -> Result<PathBuf> {
    Ok(crate::releases::data_dir()?
        .join("mcp-broker/bin")
        .join(BINARY))
}

/// Where the config pins live, beside the stable binary.
fn pin_dir() -> Result<PathBuf> {
    Ok(crate::releases::data_dir()?.join("mcp-broker"))
}

/// The sha256 of the config `install` wrote for `name`, kept beside the
/// stable binary and outside the ledger, which sandboxed agents may write.
/// The dispatcher runs the config's upstream only while it still matches,
/// so an agent that rewrites the config cannot have the dispatcher run its
/// own command. A name with a pin is an instance.
fn config_pin(name: &str) -> Result<PathBuf> {
    Ok(pin_dir()?.join(format!("{name}.json.sha256")))
}

/// Every instance: each name with a pin, in order.
fn instances() -> Result<Vec<String>> {
    let mut names = suffixed(&pin_dir()?, ".json.sha256");
    names.sort();
    Ok(names)
}

/// The valid instance names of the files in `dir` that end in `suffix`.
fn suffixed(dir: &Path, suffix: &str) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return vec![];
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.strip_suffix(suffix)?.to_owned();
            valid_name(&name).then_some(name)
        })
        .collect();
    names.sort();
    names
}

/// `$XDG_DATA_HOME/privateer/mcp-broker/run`: the brokers' locks, pids,
/// states, logs, sockets and configs. Sandboxed agents may write the ledger
/// and the ship homes, never this; and a crew connects to a socket here
/// without being able to replace it (docs/mcp-broker.md).
pub fn run_dir() -> Result<PathBuf> {
    Ok(crate::releases::data_dir()?.join("mcp-broker/run"))
}

/// The run directory, refused when it lies inside a directory agents may
/// write: the ledger or the ship's home.
fn checked_run_dir(home: Option<&Path>) -> Result<PathBuf> {
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
    Ok(dir)
}

/// One instance's files in the run directory.
struct Files {
    name: String,
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
    fn new(dir: PathBuf, name: &str) -> Self {
        let at = |suffix: &str| dir.join(format!("{name}.{suffix}"));
        Self {
            name: name.to_owned(),
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
    let dir = checked_run_dir(home)?;
    let named = |args: &[String]| -> Result<Option<String>> {
        match args {
            [] => Ok(None),
            [flag, name] if flag == "--name" => Ok(Some(checked_name(name)?)),
            _ => bail!(USAGE),
        }
    };
    let out = match args.first().map(String::as_str) {
        Some("install") => {
            caller.require_operator("mcp-broker install")?;
            let (name, config) = install_options(home, &dir, &args[1..])?;
            install(&Files::new(dir, &name), &config)?
        }
        Some("status") => status(&dir, named(&args[1..])?.as_deref())?,
        None => status(&dir, None)?,
        Some("stop") => {
            caller.require_operator("mcp-broker stop")?;
            let name = named(&args[1..])?.context("mcp-broker stop needs --name NAME")?;
            let files = Files::new(dir, &name);
            // First, so the dispatcher does not start it again meanwhile.
            private_dir(&files.dir)?;
            write_private(&files.stopped, b"")
                .with_context(|| format!("cannot write {}", files.stopped.display()))?;
            json!({"name": name, "stopped": stop_running(&files)?, "held_until": "mcp-broker install"})
        }
        _ => bail!(USAGE),
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn checked_name(name: &str) -> Result<String> {
    ensure!(
        valid_name(name),
        "--name {name:?} must start with a lowercase letter, then at most 15 lowercase letters, digits or -"
    );
    Ok(name.to_owned())
}

/// What `install` records for an instance: its upstream, the workspace
/// pools, and the profile's label, path arguments and routing, as
/// check-profile wrote them. A config of #346's shape (upstream only) is
/// still a plain instance.
struct Config {
    upstream: Vec<String>,
    pools: Vec<PathBuf>,
    label: Option<String>,
    path_arguments: Option<Value>,
    routing: Option<Value>,
}

impl Config {
    fn json(&self) -> Value {
        let mut config = json!({"upstream": self.upstream, "workspace_pools": self.pools});
        for (key, value) in [
            ("label", self.label.as_ref().map(|label| json!(label))),
            ("path_arguments", self.path_arguments.clone()),
            ("routing", self.routing.clone()),
        ] {
            if let Some(value) = value {
                config[key] = value;
            }
        }
        config
    }

    /// A config `install` wrote. The broker validates the profile parts
    /// again when it starts; here only their shape is read.
    fn parse(config: &Value) -> Result<Self> {
        let map = config
            .as_object()
            .context("mcp-broker config is not a JSON object")?;
        let keys = [
            "upstream",
            "workspace_pools",
            "label",
            "path_arguments",
            "routing",
        ];
        if let Some(key) = map.keys().find(|key| !keys.contains(&key.as_str())) {
            bail!("mcp-broker config has an unknown key {key:?}");
        }
        let upstream: Vec<String> = serde_json::from_value(config["upstream"].clone())
            .context("mcp-broker config has no upstream argv")?;
        ensure!(
            upstream
                .first()
                .is_some_and(|program| program.starts_with('/')),
            "mcp-broker config upstream must start with an absolute path"
        );
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
        let label = match config.get("label") {
            Some(label) => Some(
                label
                    .as_str()
                    .context("mcp-broker config label is not a string")?
                    .to_owned(),
            ),
            None => None,
        };
        let part = |key: &str| -> Result<Option<Value>> {
            match config.get(key) {
                Some(value) if value.is_object() => Ok(Some(value.clone())),
                Some(_) => bail!("mcp-broker config {key} is not an object"),
                None => Ok(None),
            }
        };
        Ok(Self {
            upstream,
            pools,
            label,
            path_arguments: part("path_arguments")?,
            routing: part("routing")?,
        })
    }
}

/// `install`'s arguments: the instance's name, and its config. The profile
/// file is read and the flags override it (`--upstream` its upstream,
/// `--window-open` and `--window-close` its routing's helpers); then the
/// shipped broker's `check-profile`, the one validator, gives the canonical
/// form that is pinned. The workspace pool Privateer itself uses,
/// `<ledger>/worktrees`, is always there; `--workspace-pool` adds others,
/// each absolute, normalized and existing.
fn install_options(home: Option<&Path>, dir: &Path, args: &[String]) -> Result<(String, Config)> {
    let mut name = None;
    let mut profile_file = None;
    let mut extra_pools = vec![];
    let mut helpers: Vec<(&str, Value)> = vec![];
    let mut upstream = None;
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let mut value = || rest.next().cloned().context(USAGE);
        match flag.as_str() {
            "--name" => name = Some(checked_name(&value()?)?),
            "--profile" => profile_file = Some(PathBuf::from(value()?)),
            "--workspace-pool" => extra_pools.push(PathBuf::from(value()?)),
            "--window-open" | "--window-close" => {
                let argv: Vec<String> = serde_json::from_str(&value()?)
                    .with_context(|| format!("{flag} takes a JSON array of strings"))?;
                let key = if flag == "--window-open" {
                    "open"
                } else {
                    "close"
                };
                helpers.push((key, json!(argv)));
            }
            "--upstream" => {
                let argv: Vec<String> = rest.by_ref().cloned().collect();
                ensure!(!argv.is_empty(), USAGE);
                upstream = Some(argv);
            }
            _ => bail!(USAGE),
        }
    }
    let name = name.context("mcp-broker install needs --name NAME")?;
    let socket = Files::new(dir.to_path_buf(), &name).socket;
    ensure!(
        socket.as_os_str().len() < SUN_PATH,
        "the socket path {} is {} bytes, and macOS allows at most {}; set XDG_DATA_HOME to a shorter directory",
        socket.display(),
        socket.as_os_str().len(),
        SUN_PATH - 1
    );
    let mut profile = match &profile_file {
        Some(path) => {
            let bytes = fs::read(path)
                .with_context(|| format!("cannot read the profile {}", path.display()))?;
            ensure!(bytes.len() <= 1 << 20, "the profile is larger than 1 MiB");
            let profile: Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("the profile {} is not JSON", path.display()))?;
            ensure!(
                profile.is_object(),
                "the profile {} is not a JSON object",
                path.display()
            );
            profile
        }
        None => json!({}),
    };
    if let Some(argv) = upstream {
        profile["upstream"] = json!(argv);
    }
    for (key, argv) in helpers {
        let routing = profile
            .get_mut("routing")
            .and_then(Value::as_object_mut)
            .with_context(|| {
                format!("--window-{key} needs a profile with routing (--profile FILE)")
            })?;
        routing.insert(key.to_owned(), argv);
    }
    if profile.get("label").is_none() {
        profile["label"] = json!(name);
    }
    let canonical = check_profile(&source_binary()?, &profile)?;
    let upstream: Vec<String> = match canonical.get("upstream") {
        Some(argv) => serde_json::from_value(argv.clone())?,
        None => bail!(
            "no upstream for {name}: pass --upstream /ABS/COMMAND [ARG...] or set upstream in the profile"
        ),
    };
    let own = crate::jobs::ledger_dir_at(home)?.join("worktrees");
    ensure!(own.is_absolute(), "resource_home must be absolute");
    let mut pools = vec![];
    for pool in extra_pools {
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
        ensure!(pools.len() < 15, "at most 16 workspace pools");
        if !pools.contains(&real) {
            pools.push(real);
        }
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&own)
        .with_context(|| format!("cannot make {}", own.display()))?;
    let own = fs::canonicalize(&own)?;
    pools.retain(|pool| *pool != own);
    pools.insert(0, own);
    Ok((
        name,
        Config {
            upstream,
            pools,
            label: canonical["label"].as_str().map(str::to_owned),
            path_arguments: canonical.get("path_arguments").cloned(),
            routing: canonical.get("routing").cloned(),
        },
    ))
}

/// Run the broker's `check-profile` on `profile`: its canonical JSON, or
/// the one line it refused the profile with.
fn check_profile(binary: &Path, profile: &Value) -> Result<Value> {
    let mut child = Command::new(binary)
        .arg("check-profile")
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run {} check-profile", binary.display()))?;
    if let Some(mut stdin) = child.stdin.take() {
        io::Write::write_all(&mut stdin, profile.to_string().as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().next().unwrap_or("").trim();
        bail!(
            "the profile is refused: {}",
            first.chars().take(500).collect::<String>()
        );
    }
    serde_json::from_slice(&output.stdout).context("check-profile printed no JSON")
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
/// same version (then nothing changes, not even the inode), and record and
/// pin the instance's config, which the dispatcher starts it with.
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
        &config_pin(&files.name)?,
        format!("{:x}\n", Sha256::digest(&config)).as_bytes(),
    )?;
    write_private(&files.config, &config)?;
    let _ = fs::remove_file(&files.stopped);
    let sha256 = sha256_file(&target)?;
    Ok(json!({
        "name": files.name,
        "action": action,
        "binary": target,
        "version": version,
        "sha256": sha256,
        "config": files.config,
        "upstream": upstream,
        "workspace_pools": wanted.pools,
        "label": wanted.label,
        "path_arguments": wanted.path_arguments,
        "routing": wanted.routing,
        "socket": files.socket,
        // What a crew bundle pins: this command, with `sha256`.
        "bundle_command": [target, "proxy", "--socket", files.socket],
        "running": lock_held(&files.lock),
        "note": "a running broker keeps its binary and upstream until it is stopped",
    }))
}

/// The stable binary, and every instance (or only `only`): its config,
/// files, and the running broker's own report.
fn status(dir: &Path, only: Option<&str>) -> Result<Value> {
    let binary = stable_binary()?;
    let installed = binary.is_file();
    let names = match only {
        Some(name) => {
            ensure!(
                config_pin(name)?.exists(),
                "no MCP broker instance {name}: privateer mcp-broker install --name {name}"
            );
            vec![name.to_owned()]
        }
        None => instances()?,
    };
    let mut reports = Map::new();
    for name in names {
        let files = Files::new(dir.to_path_buf(), &name);
        reports.insert(name, instance_status(&files));
    }
    Ok(json!({
        "binary": binary,
        "installed": installed,
        "version": installed.then(|| version_of(&binary).ok()).flatten(),
        "sha256": installed.then(|| sha256_file(&binary).ok()).flatten(),
        "run_dir": dir,
        "instances": reports,
    }))
}

fn instance_status(files: &Files) -> Value {
    let running = lock_held(&files.lock);
    let read = |path: &Path| -> Value {
        read_checked(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(Value::Null)
    };
    let config = read(&files.config);
    json!({
        "config": files.config,
        "configured": !config.is_null(),
        "label": config.get("label").cloned().unwrap_or_else(|| json!(files.name)),
        "upstream": config["upstream"],
        "workspace_pools": config["workspace_pools"],
        "routing": config.get("routing").is_some(),
        // The windows the broker opened and will close on release.
        "windows": read(&files.windows)["windows"],
        "stopped": files.stopped.exists(),
        "running": running,
        "pid": running.then(|| read_pid(&files.pid)).flatten(),
        "socket": files.socket,
        "log": files.log,
        // The broker's own report: upstream state, pid, restarts, clients.
        "broker": if running { read(&files.state) } else { Value::Null },
    })
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

/// The windows `NAME.windows.json` records under `root`, read under the
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

/// Ask every broker that records a window under `root` to close it, before
/// the workspace is released. Each instance's `NAME.windows.json` is read
/// first, and an instance that records nothing under `root` is never
/// asked, so a broker that hangs holds up only the release of a workspace
/// it opened a window for. A recorded but stopped instance cannot close
/// anything: its windows are audited (`workspace.mcp_window_unclosed`) and
/// the release goes on. The running ones are asked in parallel under one
/// deadline, so the release waits at most 35 s however many there are; an
/// instance that answers with an error, or not in time, refuses it.
pub(crate) fn release_workspace(store: &Store, root: &Path) -> Result<()> {
    // No run directory this home could use means no broker to ask.
    let Ok(dir) = checked_run_dir(Some(store.home())) else {
        return Ok(());
    };
    let mut asked = vec![];
    for name in suffixed(&dir, ".windows.json") {
        let files = Files::new(dir.clone(), &name);
        let left = recorded_under(&files, root);
        if left.is_empty() {
            continue;
        }
        if fs::symlink_metadata(&files.socket).is_err() || !lock_held(&files.lock) {
            store.audit(
                "workspace.mcp_window_unclosed",
                &json!({"instance": name, "root": root, "windows": left,
                    "reason": "the MCP broker is not running"}),
            )?;
            continue;
        }
        asked.push(files);
    }
    let deadline = Instant::now() + CLOSE_TIMEOUT;
    let failures: Vec<String> = std::thread::scope(|scope| {
        let asking: Vec<_> = asked
            .iter()
            .map(|files| {
                (
                    &files.name,
                    scope.spawn(move || ask_close(files, root, deadline)),
                )
            })
            .collect();
        asking
            .into_iter()
            .filter_map(|(name, asking)| {
                let error = match asking.join() {
                    Ok(Ok(_)) => return None,
                    Ok(Err(error)) => format!("{error:#}"),
                    Err(_) => "the close panicked".to_owned(),
                };
                Some(format!(
                    "could not close the {name} window for this workspace: {error}"
                ))
            })
            .collect()
    });
    ensure!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}

/// Ask the running instance `files` names to close every window it opened
/// under `root` (`{"privateer_control":1,"close_workspace":ROOT}`) by
/// `deadline`: the projects it closed, or why not.
fn ask_close(files: &Files, root: &Path, deadline: Instant) -> Result<Vec<PathBuf>> {
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
    serde_json::from_value(answer["closed"].clone())
        .context("the MCP broker's answer names no closed windows")
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

/// Keep every configured instance running, from the machine owner's
/// dispatcher only, at most every 30 s for all of them. It starts the
/// installed binary for an instance none runs, never installs or replaces
/// it, and stops an instance whose config is gone. Each instance's outcome
/// is its own: one's error never skips another. Null when nothing happened.
pub(crate) fn ensure(store: &Store) -> Value {
    if let Ok(mut last) = LAST_ENSURE.lock() {
        if last.is_some_and(|at| at.elapsed() < ENSURE_EVERY) {
            return Value::Null;
        }
        *last = Some(Instant::now());
    }
    ensure_all(store).unwrap_or_else(|error| json!({"error": clipped(&error)}))
}

fn clipped(error: &anyhow::Error) -> String {
    format!("{error:#}").chars().take(2_000).collect()
}

fn ensure_all(store: &Store) -> Result<Value> {
    if crate::machines::owner_here(store)?.is_none() {
        return Ok(Value::Null);
    }
    let dir = checked_run_dir(Some(store.home()))?;
    let mut outcomes = Map::new();
    for name in instances()? {
        let files = Files::new(dir.clone(), &name);
        match ensure_one(&files) {
            Ok(Value::Null) => {}
            Ok(outcome) => {
                outcomes.insert(name, outcome);
            }
            Err(error) => {
                outcomes.insert(name, json!({"error": clipped(&error)}));
            }
        }
    }
    Ok(if outcomes.is_empty() {
        Value::Null
    } else {
        Value::Object(outcomes)
    })
}

/// One instance: stopped if its config is gone, left alone while stopped
/// or running, and otherwise started from its pinned config.
fn ensure_one(files: &Files) -> Result<Value> {
    if !files.config.exists() {
        return Ok(if lock_held(&files.lock) {
            json!({"stopped": stop_running(files)?})
        } else {
            Value::Null
        });
    }
    if files.stopped.exists() || lock_held(&files.lock) {
        return Ok(Value::Null);
    }
    let binary = stable_binary()?;
    if !binary.is_file() {
        bail!("the broker is configured but not installed: privateer mcp-broker install");
    }
    let bytes = read_checked(&files.config)
        .with_context(|| format!("cannot read {}", files.config.display()))?;
    let pinned = read_checked(&config_pin(&files.name)?)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    ensure!(
        pinned.trim() == format!("{:x}", Sha256::digest(&bytes)),
        "the {} config differs from what mcp-broker install wrote; not starting it",
        files.name
    );
    let config = Config::parse(&serde_json::from_slice(&bytes)?)?;
    private_dir(&files.dir)?;
    Ok(json!({"started": start(&binary, files, &config)?}))
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
        .arg(&files.name);
    if let Some(label) = &config.label {
        command.arg("--label").arg(label);
    }
    for pool in &config.pools {
        command.arg("--workspace-pool").arg(pool);
    }
    for (flag, part) in [
        ("--path-arguments", &config.path_arguments),
        ("--routing", &config.routing),
    ] {
        if let Some(part) = part {
            command.arg(flag).arg(part.to_string());
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
        let path = temp.path().join("fake.json");
        // The temp name this process would use, and the target, both links.
        let planted = temp
            .path()
            .join(format!(".fake.json.{}.tmp", std::process::id()));
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
    fn instance_names_are_short_lowercase_slugs() {
        for good in ["fake", "two", "a", "x1-y", "abcdefghijklmnop"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in [
            "",
            "Fake",
            "1x",
            "-x",
            "../x",
            "a/b",
            "abcdefghijklmnopq",
            "a_b",
            "a.b",
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_config_of_the_first_shape_parses_and_unknown_keys_do_not() {
        let plain = Config::parse(&json!({"upstream": ["/opt/x/mcp", "serve"]})).unwrap();
        assert!(plain.routing.is_none() && plain.label.is_none() && plain.pools.is_empty());
        assert!(Config::parse(&json!({"upstream": ["/x"], "window_open": ["/y"]})).is_err());
        assert!(Config::parse(&json!({"upstream": ["x"]})).is_err());
        assert!(Config::parse(&json!({"upstream": ["/x"], "routing": []})).is_err());
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
