//! Keeping a ship's dispatcher (`privateer serve`) running in the background.
//!
//! Where a systemd user manager is available the dispatcher is installed as a
//! user service, and on macOS with a login session as a launchd agent, so it
//! survives logouts and reboots; elsewhere it runs as a detached process for
//! this login. A dispatcher holding `<home>/serve.lock` counts as running
//! however it was started, so nothing starts twice.
//! `PRIVATEER_SERVICE=systemd|launchd|detached|none` overrides the choice, and
//! `PRIVATEER_SYSTEMCTL` and `PRIVATEER_LAUNCHCTL` the binaries (tests use them).
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const LOG_LIMIT: u64 = 1024 * 1024;
const DETACHED_PID: &str = "PRIVATEER_DETACHED_PID";
static DETACHED_CUSTODY: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

/// Called once by the CLI at single-threaded startup, after removing the
/// custody variable from its environment. Descendants must not inherit it.
pub fn detached_custody(path: Option<PathBuf>) {
    let _ = DETACHED_CUSTODY.set(path);
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PidIdentity {
    pid: i32,
    started: String,
}

fn identity_path(path: &Path) -> PathBuf {
    path.with_extension("pid.identity")
}

fn recorded_pid(path: &Path) -> Result<i32> {
    let pid: i32 = fs::read_to_string(path)?.trim().parse()?;
    ensure!(pid > 0, "invalid service pid");
    match fs::read(identity_path(path)) {
        Ok(bytes) => {
            let record: PidIdentity = serde_json::from_slice(&bytes)?;
            // An older starter may have replaced only the numeric pid. It
            // still gets the legacy held-lock/command checks at the call site.
            if record.pid == pid {
                ensure!(
                    crate::jobs::process_identity(pid as u32)?.as_deref() == Some(&record.started),
                    "recorded service process identity no longer matches pid {pid}"
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(pid)
}

fn publish_pid(path: &Path, pid: u32) -> Result<()> {
    let started = crate::jobs::process_identity(pid)?.context("cannot identify service process")?;
    let record = PidIdentity {
        pid: pid as i32,
        started,
    };
    // Keep the numeric file readable by rollback targets predating this
    // protocol. Publish its identity first, then the pid, then sync the folder.
    crate::cli::write_atomic(&identity_path(path), &serde_json::to_vec(&record)?, 0o600)?;
    crate::cli::write_atomic(path, pid.to_string().as_bytes(), 0o600)?;
    fs::File::open(path.parent().context("service pid directory")?)?.sync_all()?;
    Ok(())
}

fn publication_lock(pid: &Path) -> Result<fs::File> {
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(pid.with_extension("publication.lock"))?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0,
        "cannot lock service custody publication"
    );
    Ok(lock)
}

fn canonical_pid(path: &Path) -> Result<PathBuf> {
    Ok(path
        .parent()
        .context("service pid directory")?
        .canonicalize()?
        .join(path.file_name().context("service pid filename")?))
}

/// Take the lifetime lock and publish managed custody as one serialized step.
/// The parent fallback also takes the publication lock, so it cannot overwrite
/// an orphan that has taken the lifetime lock but has not yet published.
pub(crate) fn resident_lock(lock: &Path, pid: &Path, occupied: &str) -> Result<fs::File> {
    let _publication = publication_lock(pid)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock)?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "{occupied}"
    );
    if let Some(Some(expected)) = DETACHED_CUSTODY.get()
        && canonical_pid(expected)? == canonical_pid(pid)?
    {
        publish_pid(pid, std::process::id())?;
    }
    Ok(lock)
}

/// Whether a dispatcher currently holds the home's serve lock.
pub fn running(home: &Path) -> Result<bool> {
    held(&home.join("serve.lock"))
}

/// Whether some process holds an exclusive lock on `path`.
pub(crate) fn held(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let lock = fs::OpenOptions::new().write(true).open(path)?;
    let free = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    // Closing the descriptor releases a lock taken just to test it.
    Ok(!free)
}

/// A background process Privateer keeps running: a ship's dispatcher or a
/// captain's front door. It holds `lock` for its whole life, so a held lock
/// means running however it was started.
#[derive(Clone)]
struct Service {
    lock: PathBuf,
    unit: String,
    description: String,
    args: Vec<String>,
    environment: &'static [&'static str],
    /// A PATH to record instead of this process's own.
    path: Option<String>,
    /// Resolved only on install, never during an upgrade restart.
    python: Option<String>,
    log: PathBuf,
    pid: PathBuf,
    /// Text the command line of the detached process must contain.
    marks: Vec<String>,
    what: String,
    /// Wakes have independent supervisors; stopping a dispatcher is not a
    /// cancellation. Doors, in contrast, must stop their adapters too.
    preserve_children: bool,
}

fn dispatcher(home: &Path, ship: &str) -> Service {
    // Preserve the registry's raw home in argv and identity marks: running
    // dispatchers from older releases may name a symlink or /var alias here.
    // Only custody publication canonicalizes paths, not process recognition.
    let home_text = home.to_string_lossy().into_owned();
    Service {
        lock: home.join("serve.lock"),
        unit: unit_name(home, ship),
        description: format!("Privateer dispatcher for ship {ship}"),
        args: vec!["--home".into(), home_text.clone(), "serve".into()],
        environment: &[
            "PATH",
            crate::python::VARIABLE,
            "PRIVATEER_RESOURCE_HOME",
            "PRIVATEER_PLUGINS",
            "PRIVATEER_FLEET",
            "XDG_STATE_HOME",
        ],
        path: None,
        python: None,
        log: home.join("serve.log"),
        pid: home.join("serve.pid"),
        marks: vec!["serve".into(), home_text],
        what: "the dispatcher".into(),
        preserve_children: true,
    }
}

fn door(name: &str) -> Result<Service> {
    let dir = crate::door::directory(name)?;
    let registry = crate::fleet::registry_path()?;
    let digest = Sha256::digest(registry.to_string_lossy().as_bytes());
    let short: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    Ok(Service {
        lock: dir.join("lock"),
        unit: format!("privateer-door-{name}-{short}.service"),
        description: format!("Privateer front door {name}"),
        args: vec!["door".into(), "run".into(), name.into()],
        environment: &[
            "PATH",
            crate::python::VARIABLE,
            "PRIVATEER_FLEET",
            "PRIVATEER_RESOURCE_HOME",
            "PRIVATEER_PLUGINS",
            "XDG_STATE_HOME",
        ],
        path: None,
        python: None,
        log: dir.join("supervisor.log"),
        pid: dir.join("supervisor.pid"),
        marks: vec!["door".into(), "run".into(), name.into()],
        what: format!("the front door {name}"),
        preserve_children: false,
    })
}

fn systemctl() -> String {
    std::env::var("PRIVATEER_SYSTEMCTL").unwrap_or_else(|_| "systemctl".into())
}

fn launchctl() -> String {
    std::env::var("PRIVATEER_LAUNCHCTL").unwrap_or_else(|_| "launchctl".into())
}

/// The launchd domain of this user's login session.
fn gui_domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

fn mode() -> String {
    if let Ok(mode) = std::env::var("PRIVATEER_SERVICE") {
        return mode;
    }
    // Agents load into the login session's domain, which exists only while
    // the user is logged in at the Mac, including when reached over SSH.
    if cfg!(target_os = "macos") {
        let session = Command::new(launchctl())
            .args(["print", &gui_domain()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        return if session { "launchd" } else { "detached" }.into();
    }
    let user_manager = std::env::var_os("XDG_RUNTIME_DIR").is_some()
        && Command::new(systemctl())
            .args(["--user", "show-environment"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
    if user_manager { "systemd" } else { "detached" }.into()
}

/// A stable unit name for a ship home.
fn unit_name(home: &Path, ship: &str) -> String {
    let digest = Sha256::digest(home.to_string_lossy().as_bytes());
    let short: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("privateer-{ship}-{short}.service")
}

fn unit_dir() -> Result<PathBuf> {
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(config.join("systemd/user"))
}

fn agent_dir() -> Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
            .join("Library/LaunchAgents"),
    )
}

/// The launchd label shares the systemd unit's stable name.
fn label(service: &Service) -> &str {
    service.unit.trim_end_matches(".service")
}

fn agent_path(service: &Service) -> Result<PathBuf> {
    Ok(agent_dir()?.join(format!("{}.plist", label(service))))
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The value a service records for one environment variable.
fn recorded(service: &Service, name: &str) -> Option<String> {
    match (&service.path, name) {
        (Some(path), "PATH") => Some(path.clone()),
        (_, crate::python::VARIABLE) => service.python.clone(),
        _ => std::env::var(name).ok(),
    }
}

/// A user service. A service does not see the login shell's PATH, where
/// harnesses usually live, so it records the current one.
fn unit(service: &Service, binary: &Path) -> String {
    let mut environment = String::new();
    for name in service.environment {
        if let Some(value) = recorded(service, name) {
            environment.push_str(&format!(
                "Environment={}\n",
                quote(&format!("{name}={value}"))
            ));
        }
    }
    // Paths are quoted; flags and names are plain words.
    let args: Vec<String> = service
        .args
        .iter()
        .map(|a| {
            if a.starts_with('/') {
                quote(a)
            } else {
                a.clone()
            }
        })
        .collect();
    let kill_mode = if service.preserve_children {
        "process"
    } else {
        "control-group"
    };
    format!(
        "[Unit]\nDescription={}\n\n[Service]\nExecStart={} {}\n{environment}KillMode={kill_mode}\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
        service.description,
        quote(&binary.to_string_lossy()),
        args.join(" ")
    )
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A launchd agent with the same policy as the systemd unit: it records the
/// current PATH, restarts only after a failure, and for a dispatcher leaves
/// the rest of its process group alone when it stops (`KillMode=process`).
fn property_list(service: &Service, binary: &Path) -> String {
    let string = |value: &str| format!("<string>{}</string>", xml(value));
    let mut arguments = vec![string(&binary.to_string_lossy())];
    arguments.extend(service.args.iter().map(|a| string(a)));
    let mut environment = String::new();
    for name in service.environment {
        if let Some(value) = recorded(service, name) {
            environment.push_str(&format!("<key>{name}</key>{}", string(&value)));
        }
    }
    let log = string(&service.log.to_string_lossy());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key>{}\n<key>ProgramArguments</key><array>{}</array>\n<key>EnvironmentVariables</key><dict>{environment}</dict>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n<key>ThrottleInterval</key><integer>5</integer>\n<key>AbandonProcessGroup</key><{}/>\n<key>StandardOutPath</key>{log}\n<key>StandardErrorPath</key>{log}\n</dict></plist>\n",
        string(label(service)),
        arguments.join(""),
        service.preserve_children,
    )
}

fn rotate(log: &Path) -> Result<()> {
    if fs::metadata(log).is_ok_and(|m| m.len() > LOG_LIMIT) {
        fs::rename(log, log.with_extension("log.1"))?;
    }
    Ok(())
}

// True if we spawned a child, false if another start already won.
fn detached(service: &Service, binary: &Path) -> Result<bool> {
    // Serialize starters as well as resident children. The legacy fallback
    // below must never publish the pid of a losing concurrent spawn.
    let starting = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(service.pid.with_extension("start.lock"))?;
    // Allow the first starter's ten-second courtesy wait to finish before
    // a contender gives up on the start lock.
    let start_deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while unsafe { libc::flock(starting.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        ensure!(error.kind() == std::io::ErrorKind::WouldBlock, "{error}");
        ensure!(
            std::time::Instant::now() < start_deadline,
            "another detached start of {} is in progress",
            service.what
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    {
        let _publication = publication_lock(&service.pid)?;
        if held(&service.lock)? {
            return Ok(false);
        }
        // Only clear stale custody while serialized with both other starters
        // and resident publication, never in restart's unlocked caller.
        let _ = fs::remove_file(&service.pid);
        let _ = fs::remove_file(identity_path(&service.pid));
    }
    if let Some(python) = &service.python {
        crate::cli::write_atomic(
            &service.pid.with_extension("python.json"),
            &serde_json::to_vec(python)?,
            0o600,
        )?;
    }
    let cwd = service
        .log
        .parent()
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| Path::new("/"));
    let log = &service.log;
    rotate(log)?;
    let out = fs::OpenOptions::new().create(true).append(true).open(log)?;
    let mut command = Command::new(binary);
    command.current_dir(cwd);
    for name in [
        "PRIVATEER_ROLE",
        "PRIVATEER_ACTOR",
        "PRIVATEER_JOB",
        "PRIVATEER_HOME",
        "PRIVATEER_PRESENCE_HOLDER",
        "PRIVATEER_WAKE",
        "PRIVATEER_CONTROL_CLI",
        "PRIVATEER_ATTENDED_HARNESS",
        "PRIVATEER_CLAUDE_MAIL_UNTIL",
    ] {
        command.env_remove(name);
    }
    if let Some(path) = &service.path {
        command.env("PATH", path);
    }
    // Detached services have no manager-owned environment. Keep their one
    // installation-time choice across upgrade restarts, without re-resolving.
    if let Some(python) = detached_python(service)? {
        command.env(crate::python::VARIABLE, python);
    }
    command
        .args(&service.args)
        .env(DETACHED_PID, canonical_pid(&service.pid)?)
        .env_remove("PRIVATEER_TEST_DETACHED_AFTER_SPAWN")
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out);
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("start {}", service.what))?;
    // A deterministic crash boundary for subprocess regressions. The child
    // must publish custody even if this starter never runs another instruction.
    #[cfg(debug_assertions)]
    if let Some(marker) = std::env::var_os("PRIVATEER_TEST_DETACHED_AFTER_SPAWN") {
        crate::cli::write_atomic(Path::new(&marker), child.id().to_string().as_bytes(), 0o600)?;
        unsafe { libc::raise(libc::SIGSTOP) };
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        {
            let _publication = publication_lock(&service.pid)?;
            if held(&service.lock)? {
                // An orphan from an interrupted starter can win while this
                // child opens its store. Never replace that winner's custody.
                if let Ok(pid) = recorded_pid(&service.pid)
                    && pid != child.id() as i32
                    && unsafe { libc::kill(pid, 0) } == 0
                {
                    return Ok(false);
                }
                if child.try_wait()?.is_none() {
                    // Legacy rollback targets do not publish from the child.
                    // Only publish after the lock is held, with no other live
                    // custodian, and serialized with new children's publication.
                    publish_pid(&service.pid, child.id())?;
                    return Ok(true);
                }
            }
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "{} exited before publishing its pid: {status}",
                service.what
            );
        }
        if std::time::Instant::now() >= deadline {
            // Opening/migrating the store can take longer than this courtesy
            // wait. Custody belongs to the child; callers (notably upgrade)
            // apply their own health deadline. A live starter is not failure.
            return Ok(true);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Make sure the ship's dispatcher is running; report how.
pub fn ensure(home: &Path, ship: &str) -> Result<String> {
    crate::cli::refresh_claude_settings(home);
    started(&dispatcher(home, ship), home)
}

/// Start the ship's dispatcher with `path` as its PATH. A process reached
/// over SSH has a bare PATH that would not find the harness.
pub(crate) fn ensure_with_path(home: &Path, ship: &str, path: &str) -> Result<String> {
    crate::cli::refresh_claude_settings(home);
    let mut service = dispatcher(home, ship);
    service.path = Some(path.to_owned());
    started(&service, home)
}

/// Start a dispatcher, after checking the tools it needs on the PATH its
/// unit records. What fails is appended to the report; nothing is refused.
/// A dispatcher already running checks on its own PATH, not this command's,
/// so then the report shows what it last found.
fn started(service: &Service, home: &Path) -> Result<String> {
    let path = (!held(&service.lock)?).then(|| recorded(service, "PATH").unwrap_or_default());
    let mut service = service.clone();
    if let Some(path) = &path
        && matches!(mode().as_str(), "systemd" | "launchd" | "detached")
    {
        service.python = Some(
            crate::python::resolve(path.as_ref())?
                .to_string_lossy()
                .into_owned(),
        );
    }
    let readiness =
        crate::readiness::ensure_summary(home, path.as_deref(), service.python.as_deref());
    let report = start(&service, &home.join("serve.log"))?;
    Ok(match readiness {
        Some(summary) => format!("{report}. {summary}"),
        None => report,
    })
}

/// The PATH a dispatcher started here records: this process's own, then the
/// login shell's, since a gateway reached over SSH has a bare PATH that
/// would not find the harness.
pub(crate) fn login_path() -> String {
    login_path_by(std::time::Instant::now() + LOGIN_TIMEOUT)
}

/// `login_path`, with the login shell stopped by `deadline` if that is
/// sooner than its own timeout: a check with a budget passes its deadline.
pub(crate) fn login_path_by(deadline: std::time::Instant) -> String {
    let own = std::time::Instant::now() + LOGIN_TIMEOUT;
    login_path_from(std::env::var("SHELL").ok().as_deref(), deadline.min(own)).0
}

/// How long a login shell may take to print its PATH, the same as a
/// capability probe: an rc file that prompts or a stalled home must not hold
/// a dispatcher pass, a move or an install.
const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// `login_path` for a dispatcher unit that keeps it: a ship made for a
/// linked machine, or a move's arrival. A PATH missing the login shell's
/// entries stays with the unit, so the shell gets longer, and when its PATH
/// could not be read the second value says why, for the reply to show.
pub(crate) fn login_path_to_record() -> (String, Option<String>) {
    let (path, why) = login_path_from(
        std::env::var("SHELL").ok().as_deref(),
        std::time::Instant::now() + RECORD_TIMEOUT,
    );
    let why = why.map(|why| {
        let after = if why == LOGIN_TIMED_OUT {
            format!(" after {} s", RECORD_TIMEOUT.as_secs())
        } else {
            String::new()
        };
        format!(
            "{why}{after}, so the dispatcher records only this process's PATH, which \
             may not find the harness; run `privateer service start` from a login shell \
             once it prints its PATH"
        )
    });
    (path, why)
}

/// How long a login shell may take when its PATH is kept: inside the 30 s a
/// linked machine waits for a ship to be made there.
const RECORD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// `login_path` with `shell` as the login shell, which is killed with all it
/// started at `deadline`. When its PATH could not be read (it timed out,
/// failed, or printed no PATH) only this process's PATH is used, and the
/// second value says why; with no login shell to ask there is none.
pub(crate) fn login_path_from(
    shell: Option<&str>,
    deadline: std::time::Instant,
) -> (String, Option<String>) {
    let mut entries: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let ran = shell.filter(|shell| shell.starts_with('/')).map(|shell| {
        let mut command = Command::new(shell);
        command.args(["-lc", &format!("printf '%s%s\\n' '{PATH_MARK}' \"$PATH\"")]);
        crate::capabilities::run_bounded(command, "the login shell", deadline, true)
    });
    let (login, why) = match ran {
        None => (None, None),
        Some(Ok(output)) => match marked_path(&output) {
            Some(login) => (Some(login), None),
            None => (None, Some("the login shell printed no PATH".to_owned())),
        },
        Some(Err(why)) if why == crate::capabilities::TIMED_OUT => {
            (None, Some(LOGIN_TIMED_OUT.to_owned()))
        }
        Some(Err(why)) => (None, Some(why)),
    };
    for entry in login.iter().flat_map(std::env::split_paths) {
        if !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    let path = std::env::join_paths(entries)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    (path, why)
}

/// Why `login_path_from` used only this process's PATH, when the login shell
/// was still running at its deadline.
const LOGIN_TIMED_OUT: &str = "the login shell timed out";

/// Printed just before the login shell's PATH, so text an rc file prints
/// first (a banner, a motd, a fortune) is never read as PATH entries.
const PATH_MARK: &str = "privateer-login-path:";

/// The PATH after the last `PATH_MARK` in a login shell's output, up to the
/// end of that line, so what an EXIT trap or logout file prints after it
/// is left out; none when the mark is missing (the shell never ran it).
fn marked_path(output: &str) -> Option<String> {
    let (_, path) = output.rsplit_once(PATH_MARK)?;
    Some(
        path.lines()
            .next()
            .unwrap_or_default()
            .trim_end_matches('\r')
            .to_owned(),
    )
}

/// Make sure a front door's supervisor is running; report how.
pub fn ensure_door(name: &str) -> Result<String> {
    let service = door(name)?;
    let log = crate::door::directory(name)?.join("door.log");
    start(&service, &log)
}

fn detached_python(service: &Service) -> Result<Option<String>> {
    match fs::read(service.pid.with_extension("python.json")) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn start(service: &Service, log: &Path) -> Result<String> {
    if held(&service.lock)? {
        return Ok("already running".into());
    }
    let mode = mode();
    if mode == "none" {
        return Ok("not started (PRIVATEER_SERVICE=none)".into());
    }
    ensure!(
        matches!(mode.as_str(), "systemd" | "launchd" | "detached"),
        "PRIVATEER_SERVICE must be systemd, launchd, detached or none, not {mode}"
    );
    let mut configured = service.clone();
    let path = recorded(service, "PATH").unwrap_or_default();
    if configured.python.is_none() {
        configured.python = Some(
            crate::python::resolve(path.as_ref())?
                .to_string_lossy()
                .into_owned(),
        );
    }
    let service = &configured;
    let binary = crate::executable::installed()?;
    match mode.as_str() {
        "none" => Ok("not started (PRIVATEER_SERVICE=none)".into()),
        "systemd" => {
            let name = &service.unit;
            let dir = unit_dir()?;
            fs::create_dir_all(&dir)?;
            fs::write(dir.join(name), unit(service, &binary))?;
            for args in [
                vec!["--user", "daemon-reload"],
                vec!["--user", "enable", "--now", name.as_str()],
            ] {
                let status = Command::new(systemctl()).args(&args).status()?;
                ensure!(status.success(), "systemctl {} failed", args.join(" "));
            }
            Ok(format!("installed as user service {name}"))
        }
        "launchd" => {
            let plist = agent_path(service)?;
            fs::create_dir_all(plist.parent().context("agent directory")?)?;
            rotate(&service.log)?;
            fs::write(&plist, property_list(service, &binary))?;
            let target = format!("{}/{}", gui_domain(), label(service));
            // An agent still loaded from an earlier run, not holding the lock
            // (so not running), would refuse the new bootstrap.
            let _ = Command::new(launchctl())
                .args(["bootout", &target])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let plist_text = plist.to_string_lossy();
            for args in [
                vec!["enable", target.as_str()],
                vec!["bootstrap", &gui_domain(), &plist_text],
            ] {
                let status = Command::new(launchctl()).args(&args).status()?;
                ensure!(status.success(), "launchctl {} failed", args.join(" "));
            }
            Ok(format!("installed as launchd agent {}", label(service)))
        }
        "detached" => {
            if detached(service, &binary)? {
                Ok(format!(
                    "started in the background (log: {})",
                    log.display()
                ))
            } else {
                Ok("already running".into())
            }
        }
        other => bail!("PRIVATEER_SERVICE must be systemd, launchd, detached or none, not {other}"),
    }
}

/// Describe the ship's dispatcher.
pub fn status(home: &Path, ship: &str) -> Result<String> {
    Ok(format!(
        "Dispatcher for {ship}: {}.",
        describe(&dispatcher(home, ship))?
    ))
}

/// Describe a front door's supervisor.
pub fn door_status(name: &str) -> Result<String> {
    describe(&door(name)?)
}

fn describe(service: &Service) -> Result<String> {
    let (how, python) = if unit_dir()?.join(&service.unit).exists() {
        let text = fs::read_to_string(unit_dir()?.join(&service.unit))?;
        (
            format!("user service {}", service.unit),
            configured_python(&text, false),
        )
    } else if agent_path(service)?.exists() {
        let text = fs::read_to_string(agent_path(service)?)?;
        (
            format!("launchd agent {}", label(service)),
            configured_python(&text, true),
        )
    } else {
        ("no user service".into(), detached_python(service)?)
    };
    let python = python.map_or_else(
        || "Python: python3 on PATH (legacy/unrecorded)".to_owned(),
        |python| format!("Python: {python:?}"),
    );
    Ok(format!(
        "{} ({how}); {python}",
        if held(&service.lock)? {
            "running"
        } else {
            "not running"
        }
    ))
}

/// Read only the Python setting Privateer writes, never other environment
/// values (which may be credentials). Older configs have no such setting.
fn configured_python(text: &str, launchd: bool) -> Option<String> {
    if launchd {
        let (_, rest) = text.split_once("<key>PRIVATEER_PYTHON</key><string>")?;
        return Some(unxml(rest.split_once("</string>")?.0));
    }
    let value = text
        .lines()
        .find_map(|line| line.strip_prefix("Environment=\"PRIVATEER_PYTHON="))?;
    let mut chars = value.chars();
    let mut value = String::new();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(value),
            '\\' => value.extend(chars.next()),
            c => value.push(c),
        }
    }
    None
}

/// Stop the ship's dispatcher: disable and remove its user service, or end
/// the detached process recorded in serve.pid if it is still that process.
pub fn stop(home: &Path, ship: &str) -> Result<String> {
    halt(
        &dispatcher(home, ship),
        "perhaps `privateer serve` in a terminal",
    )
}

/// Stop a front door's supervisor, which stops its adapter.
pub fn stop_door(name: &str) -> Result<String> {
    halt(&door(name)?, "perhaps `privateer door run` in a terminal")
}

fn halt(service: &Service, elsewhere: &str) -> Result<String> {
    let name = &service.unit;
    let unit = unit_dir()?.join(name);
    if unit.exists() {
        if service.preserve_children {
            // Older generated units used systemd's control-group default.
            // Reload the safe stop policy BEFORE asking systemd to stop one;
            // preserve its original executable, arguments and environment.
            let previous = fs::read_to_string(&unit)?;
            let safe = dispatcher_stop_policy(&previous)?;
            fs::write(&unit, safe)?;
            let status = Command::new(systemctl())
                .args(["--user", "daemon-reload"])
                .status()?;
            ensure!(
                status.success(),
                "systemctl --user daemon-reload failed; not stopping dispatcher"
            );
        }
        let status = Command::new(systemctl())
            .args(["--user", "disable", "--now", name.as_str()])
            .status()?;
        ensure!(
            status.success(),
            "systemctl --user disable --now {name} failed"
        );
        fs::remove_file(&unit)?;
        let _ = Command::new(systemctl())
            .args(["--user", "daemon-reload"])
            .status();
        return Ok(format!("Stopped and removed user service {name}."));
    }
    let plist = agent_path(service)?;
    if plist.exists() {
        let label = label(service);
        // bootout sends the agent SIGTERM; a dispatcher's agent abandons its
        // process group, so job supervisors keep running.
        let status = Command::new(launchctl())
            .args(["bootout", &format!("{}/{label}", gui_domain())])
            .status()?;
        ensure!(
            status.success() || !held(&service.lock)?,
            "launchctl bootout {label} failed; {} is still running",
            service.what
        );
        fs::remove_file(&plist)?;
        ensure!(
            released(service)?,
            "{} did not stop within 10 seconds of launchctl bootout",
            service.what
        );
        return Ok(format!("Stopped and removed launchd agent {label}."));
    }
    if let Ok(pid) = recorded_pid(&service.pid) {
        // Only signal the recorded process while it still holds the lock and
        // is this service's process, never a reused PID.
        let command = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()?;
        let line = String::from_utf8_lossy(&command.stdout);
        if held(&service.lock)? && service.marks.iter().all(|m| line.contains(m.as_str())) {
            unsafe { libc::kill(pid, libc::SIGTERM) };
            let released = released(service)?;
            let _ = fs::remove_file(&service.pid);
            let _ = fs::remove_file(identity_path(&service.pid));
            ensure!(
                released,
                "{} (pid {pid}) did not stop within 10 seconds",
                service.what
            );
            return Ok(format!("Stopped {} (pid {pid}).", service.what));
        }
        let _ = fs::remove_file(&service.pid);
        let _ = fs::remove_file(identity_path(&service.pid));
    }
    if held(&service.lock)? {
        bail!(
            "{} is running but Privateer did not start it ({elsewhere}); stop it there",
            service.what
        )
    }
    let _ = fs::remove_file(&service.pid);
    let _ = fs::remove_file(identity_path(&service.pid));
    Ok(format!("{} was not running.", capitalized(&service.what)))
}

/// Whether a stopped service lets go of its lock within ten seconds: only
/// then has it ended.
fn released(service: &Service) -> Result<bool> {
    for _ in 0..100 {
        if !held(&service.lock)? {
            return Ok(true);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(false)
}

/// A dispatcher or front door as an upgrade restarts it.
pub(crate) struct Restartable(Service);

/// How a running service is kept: its user unit, its launchd agent, or a
/// detached process recorded in its pid file.
pub(crate) enum Managed {
    Unit(PathBuf),
    Agent(PathBuf),
    Detached,
}

pub(crate) fn restartable_dispatcher(home: &Path, ship: &str) -> Restartable {
    Restartable(dispatcher(home, ship))
}

pub(crate) fn restartable_door(name: &str) -> Result<Restartable> {
    Ok(Restartable(door(name)?))
}

impl Restartable {
    pub(crate) fn running(&self) -> Result<bool> {
        held(&self.0.lock)
    }

    /// Where it is kept, as `halt` finds it: a unit, else an agent, else the
    /// detached process.
    pub(crate) fn managed(&self) -> Result<Managed> {
        let unit = unit_dir()?.join(&self.0.unit);
        if unit.exists() {
            return Ok(Managed::Unit(unit));
        }
        let plist = agent_path(&self.0)?;
        if plist.exists() {
            return Ok(Managed::Agent(plist));
        }
        Ok(Managed::Detached)
    }

    /// The pid of the detached process Privateer started, while it still
    /// holds its lock and is this service's process (never a reused pid);
    /// otherwise why it is not, to restart by hand.
    pub(crate) fn detached_pid(&self) -> Result<i32> {
        let service = &self.0;
        let pid = recorded_pid(&service.pid).with_context(|| {
            format!(
                "{} is running but records no valid pid; restart it by hand",
                service.what
            )
        })?;
        let command = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()?;
        let line = String::from_utf8_lossy(&command.stdout);
        ensure!(
            held(&service.lock)? && service.marks.iter().all(|m| line.contains(m.as_str())),
            "{} is running but is not the process Privateer started (pid {pid}); restart it by hand",
            service.what
        );
        Ok(pid)
    }

    /// Stop and start it again through what keeps it, from `current` when
    /// this process runs a release. A dispatcher's stop never ends its wakes:
    /// its unit must already stop the main process alone
    /// (`KillMode=process`), and its agent abandon its process group (job
    /// supervisors run in their own sessions either way). A detached one is
    /// signalled only while it still holds its lock and is this service's
    /// process; it must let go of its lock before the new one starts. A
    /// systemd unit or detached process that is not running is started.
    pub(crate) fn restart(&self) -> Result<String> {
        let service = &self.0;
        match self.managed()? {
            Managed::Unit(unit) => {
                if service.preserve_children {
                    let text = fs::read_to_string(&unit)?;
                    ensure!(
                        text.lines().any(|l| l.trim() == "KillMode=process"),
                        "{} has no KillMode=process; not restarting it",
                        unit.display()
                    );
                }
                let name = &service.unit;
                let status = Command::new(systemctl())
                    .args(["--user", "restart", name.as_str()])
                    .status()
                    .context("cannot run systemctl")?;
                ensure!(status.success(), "systemctl --user restart {name} failed");
                Ok(format!("restarted user service {name}"))
            }
            Managed::Agent(plist) => {
                if service.preserve_children {
                    let text = fs::read_to_string(&plist)?;
                    ensure!(
                        text.contains("<key>AbandonProcessGroup</key><true/>"),
                        "{} does not abandon its process group; not restarting it",
                        plist.display()
                    );
                }
                let label = label(service);
                let target = format!("{}/{label}", gui_domain());
                // bootout sends the agent SIGTERM; bootstrap reads the agent
                // again, so a changed one is read now.
                let status = Command::new(launchctl())
                    .args(["bootout", &target])
                    .status()
                    .context("cannot run launchctl")?;
                ensure!(
                    status.success() || !held(&service.lock)?,
                    "launchctl bootout {label} failed; {} is still running",
                    service.what
                );
                ensure!(
                    released(service)?,
                    "{} did not stop within 10 seconds of launchctl bootout",
                    service.what
                );
                rotate(&service.log)?;
                let plist_text = plist.to_string_lossy();
                for args in [
                    vec!["enable", target.as_str()],
                    vec!["bootstrap", &gui_domain(), &plist_text],
                ] {
                    let status = Command::new(launchctl()).args(&args).status()?;
                    ensure!(status.success(), "launchctl {} failed", args.join(" "));
                }
                Ok(format!("restarted launchd agent {label}"))
            }
            // One that already exited (a release that failed to start, being
            // rolled back) is started again.
            Managed::Detached if !held(&service.lock)? => {
                if detached(service, &crate::executable::installed()?)? {
                    Ok("started in the background".into())
                } else {
                    Ok("already running".into())
                }
            }
            Managed::Detached => {
                let pid = self.detached_pid()?;
                unsafe { libc::kill(pid, libc::SIGTERM) };
                ensure!(
                    released(service)?,
                    "{} (pid {pid}) did not stop within 10 seconds",
                    service.what
                );
                detached(service, &crate::executable::installed()?)?;
                Ok(format!("restarted in the background (was pid {pid})"))
            }
        }
    }
}

/// The unit or agent text for a dispatcher with the safe stop policy, if it
/// lacks it; its program, arguments and environment are kept.
pub(crate) fn with_stop_policy(installed: &Installed) -> Result<Option<String>> {
    let text = &installed.text;
    if !installed.dispatcher {
        return Ok(None);
    }
    if installed.launchd {
        let abandons = "<key>AbandonProcessGroup</key><false/>";
        return Ok(text
            .contains(abandons)
            .then(|| text.replace(abandons, "<key>AbandonProcessGroup</key><true/>")));
    }
    if text.lines().any(|l| l.trim() == "KillMode=process") {
        return Ok(None);
    }
    dispatcher_stop_policy(text).map(Some)
}

/// Upgrade only the stop policy of a previously generated dispatcher unit.
fn dispatcher_stop_policy(unit: &str) -> Result<String> {
    let mut service = false;
    let mut found = false;
    let mut lines = Vec::new();
    for line in unit.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            service = trimmed == "[Service]";
            if service {
                found = true;
                lines.push(line);
                lines.push("KillMode=process");
                continue;
            }
        }
        if service
            && trimmed
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == "KillMode")
        {
            continue;
        }
        lines.push(line);
    }
    ensure!(
        found,
        "dispatcher unit has no [Service] section; not stopping it"
    );
    Ok(lines.join("\n") + "\n")
}

/// A dispatcher or door unit (systemd) or agent (launchd) as `start` writes
/// it, found where `start` puts it.
pub(crate) struct Installed {
    pub path: PathBuf,
    pub launchd: bool,
    pub dispatcher: bool,
    pub text: String,
}

/// Every dispatcher and door unit or agent in this user's unit and agent
/// directories, recognised by the name and description (or arguments)
/// `start` gives it; anything else there is not Privateer's and is skipped.
pub(crate) fn installed_services() -> Result<Vec<Installed>> {
    let mut found = Vec::new();
    for (dir, launchd) in [(unit_dir()?, false), (agent_dir()?, true)] {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();
        for path in paths {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let suffix = if launchd { ".plist" } else { ".service" };
            if !name.starts_with("privateer-")
                || !name.ends_with(suffix)
                || !fs::symlink_metadata(&path).is_ok_and(|m| m.is_file())
            {
                continue;
            }
            let text = fs::read_to_string(&path)?;
            if let Some(dispatcher) = dispatcher_or_door(&text, launchd) {
                found.push(Installed {
                    path,
                    launchd,
                    dispatcher,
                    text,
                });
            }
        }
    }
    Ok(found)
}

/// Whether a unit or agent `start` wrote runs a dispatcher (true) or a door
/// (false). A unit says so in its description; an agent by its exact
/// arguments, `--home HOME serve` or `door run NAME`, so a door named
/// `serve` is still a door.
fn dispatcher_or_door(text: &str, launchd: bool) -> Option<bool> {
    if launchd {
        let arguments = agent_arguments(text)?;
        return match arguments.iter().map(String::as_str).collect::<Vec<_>>()[..] {
            [_, "door", "run", _] => Some(false),
            [_, "--home", _, "serve"] => Some(true),
            _ => None,
        };
    }
    let described = |prefix: &str| text.lines().any(|l| l.trim_start().starts_with(prefix));
    if described("Description=Privateer dispatcher for ship ") {
        Some(true)
    } else {
        described("Description=Privateer front door ").then_some(false)
    }
}

/// An agent's ProgramArguments, as `property_list` writes them.
fn agent_arguments(text: &str) -> Option<Vec<String>> {
    let mark = "<key>ProgramArguments</key><array>";
    let rest = &text[text.find(mark)? + mark.len()..];
    let mut array = &rest[..rest.find("</array>")?];
    let mut arguments = Vec::new();
    while !array.is_empty() {
        let value = array.strip_prefix("<string>")?;
        let end = value.find("</string>")?;
        arguments.push(unxml(&value[..end]));
        array = &value[end + "</string>".len()..];
    }
    Some(arguments)
}

fn unxml(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// The program an installed unit or agent runs.
pub(crate) fn program(installed: &Installed) -> Result<PathBuf> {
    Ok(PathBuf::from(program_span(installed)?.1))
}

/// Where the program lies in the text, and the program itself.
fn program_span(installed: &Installed) -> Result<(std::ops::Range<usize>, String)> {
    let text = &installed.text;
    if installed.launchd {
        let mark = "<key>ProgramArguments</key><array><string>";
        let start = text
            .find(mark)
            .context("the agent has no ProgramArguments")?
            + mark.len();
        let end = start
            + text[start..]
                .find("</string>")
                .context("the agent's program is unterminated")?;
        let program = unxml(&text[start..end]);
        return Ok((start..end, program));
    }
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        // `ExecStart = VALUE`, with any blanks systemd allows around it.
        let blanks = |s: &str| s.trim_start_matches([' ', '\t']).to_owned();
        let Some(value) = blanks(line)
            .strip_prefix("ExecStart")
            .map(blanks)
            .and_then(|rest| rest.strip_prefix('=').map(blanks))
        else {
            offset += line.len();
            continue;
        };
        let start = offset + line.len() - value.len();
        let (len, program) = match value.strip_prefix('"') {
            Some(rest) => {
                // `quote` escapes backslashes and double quotes.
                let mut program = String::new();
                let mut chars = rest.char_indices();
                let mut end = None;
                while let Some((i, c)) = chars.next() {
                    match c {
                        '\\' => program.extend(chars.next().map(|(_, c)| c)),
                        '"' => {
                            end = Some(i + 2);
                            break;
                        }
                        c => program.push(c),
                    }
                }
                (
                    end.context("the unit's ExecStart is unterminated")?,
                    program,
                )
            }
            None => {
                let word = value.split_whitespace().next().unwrap_or_default();
                (word.len(), word.to_owned())
            }
        };
        // A prefix (`-`, `@`, `+`, `!`), a single quote, an escape or a
        // relative program is not what `start` writes: left as it is.
        ensure!(
            (value.starts_with('"') || value.starts_with('/'))
                && Path::new(&program).is_absolute()
                && (value.starts_with('"') || !program.contains(['\\', '\'', '"', '%'])),
            "the unit's ExecStart program {program:?} is neither quoted nor an absolute path; change it by hand"
        );
        return Ok((start..start + len, program));
    }
    bail!("the unit has no ExecStart")
}

/// The unit or agent's text running `binary`, with a dispatcher's stop
/// policy as `start` writes it (`KillMode=process`, or an agent that
/// abandons its process group); None when it already does both.
pub(crate) fn repointed(installed: &Installed, binary: &Path) -> Result<Option<String>> {
    let (span, program) = program_span(installed)?;
    let text = &installed.text;
    let abandons = "<key>AbandonProcessGroup</key><false/>";
    let unsafe_stop = installed.dispatcher
        && if installed.launchd {
            text.contains(abandons)
        } else {
            !text.lines().any(|l| l.trim() == "KillMode=process")
        };
    if Path::new(&program) == binary && !unsafe_stop {
        return Ok(None);
    }
    let binary = binary.to_string_lossy();
    let replacement = if installed.launchd {
        xml(&binary)
    } else {
        quote(&binary)
    };
    let mut out = format!("{}{replacement}{}", &text[..span.start], &text[span.end..]);
    if installed.dispatcher {
        out = if installed.launchd {
            out.replace(abandons, "<key>AbandonProcessGroup</key><true/>")
        } else {
            dispatcher_stop_policy(&out)?
        };
    }
    Ok(Some(out))
}

/// Have the systemd user manager read changed unit files; nothing restarts.
pub(crate) fn daemon_reload() -> Result<()> {
    let status = Command::new(systemctl())
        .args(["--user", "daemon-reload"])
        .status()
        .context("cannot run systemctl --user daemon-reload")?;
    ensure!(status.success(), "systemctl --user daemon-reload failed");
    Ok(())
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};

    #[test]
    fn python_recordings_render_and_survive_service_repointing() {
        let mut service = super::dispatcher(std::path::Path::new("/alpha"), "alpha");
        let binary = std::path::Path::new("/beta/privateer");
        let python = "/alpha/python & \\\"3";
        for launchd in [false, true] {
            let render = |service: &super::Service| {
                if launchd {
                    super::property_list(service, binary)
                } else {
                    super::unit(service, binary)
                }
            };
            // Upgrade's config patchers must not silently pin legacy units.
            service.python = None;
            let text = render(&service);
            assert_eq!(super::configured_python(&text, launchd), None);
            service.python = Some(python.into());
            let text = render(&service);
            assert_eq!(
                super::configured_python(&text, launchd).as_deref(),
                Some(python)
            );
            let installed = super::Installed {
                path: "/alpha/unit".into(),
                launchd,
                dispatcher: true,
                text,
            };
            let changed = super::repointed(&installed, std::path::Path::new("/gamma/privateer"))
                .unwrap()
                .unwrap();
            assert_eq!(
                super::configured_python(&changed, launchd).as_deref(),
                Some(python)
            );
        }
    }

    #[test]
    fn detached_python_state_is_optional_and_does_not_resolve_again() {
        let dir = tempfile::tempdir().unwrap();
        let service = super::dispatcher(dir.path(), "alpha");
        assert_eq!(super::detached_python(&service).unwrap(), None);
        let path = "/alpha/no-longer-installed/python3";
        std::fs::write(
            service.pid.with_extension("python.json"),
            serde_json::to_vec(path).unwrap(),
        )
        .unwrap();
        assert_eq!(
            super::detached_python(&service).unwrap().as_deref(),
            Some(path)
        );
    }

    #[test]
    fn a_concurrent_starter_cannot_spawn_or_overwrite_custody() {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let mut service = super::dispatcher(dir.path(), "alpha");
        service.python = Some("/alpha/loser/python3".into());
        let python_record = service.pid.with_extension("python.json");
        std::fs::write(&python_record, b"\"/beta/winner/python3\"").unwrap();
        let lock = std::fs::File::create(service.pid.with_extension("start.lock")).unwrap();
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        std::fs::write(&service.pid, "alpha-custody").unwrap();
        std::thread::scope(|scope| {
            let contender = scope
                .spawn(|| super::detached(&service, std::path::Path::new("/not-an-executable")));
            // The first starter establishes a resident before releasing the
            // start lock. Its contender must observe that resident, not fail
            // immediately and not attempt the deliberately invalid executable.
            let resident = std::fs::File::create(&service.lock).unwrap();
            assert_eq!(
                unsafe { libc::flock(resident.as_raw_fd(), libc::LOCK_EX) },
                0
            );
            drop(lock);
            assert!(!contender.join().unwrap().unwrap());
        });
        assert_eq!(
            std::fs::read_to_string(&service.pid).unwrap(),
            "alpha-custody"
        );
        assert_eq!(
            super::detached_python(&service).unwrap().as_deref(),
            Some("/beta/winner/python3")
        );
    }

    #[test]
    fn a_legacy_raw_symlink_home_is_recognised_and_halted() {
        use std::io::BufRead;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("alpha");
        let alias = dir.path().join("beta");
        std::fs::create_dir(&home).unwrap();
        std::os::unix::fs::symlink(&home, &alias).unwrap();
        let service = super::Restartable(super::dispatcher(&alias, "alpha"));
        assert_eq!(service.0.args[1], alias.to_string_lossy());
        // Model an older dispatcher already holding the lock with the raw
        // symlink in argv. Store::open rejects application symlinks, so this
        // subprocess exercises custody recognition without opening a store.
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = Child(
            std::process::Command::new("python3")
                .args([
                    "-c",
                    "import fcntl,pathlib,signal,sys\nf=(pathlib.Path(sys.argv[2])/'serve.lock').open('w')\nfcntl.flock(f,fcntl.LOCK_EX)\nprint('ready',flush=True)\nsignal.pause()",
                    "--home",
                ])
                .arg(&alias)
                .arg("serve")
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut ready = String::new();
        std::io::BufReader::new(child.0.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        std::fs::write(&service.0.pid, child.0.id().to_string()).unwrap();
        assert_eq!(service.detached_pid().unwrap(), child.0.id() as i32);
        let stopped = super::halt(&service.0, "legacy dispatcher").unwrap();
        assert!(stopped.contains("Stopped the dispatcher"), "{stopped}");
        assert!(!super::held(&service.0.lock).unwrap());
        assert!(!service.0.pid.exists());
        child.0.wait().unwrap();
    }

    #[test]
    fn detached_identity_rejects_reused_pids_and_retains_legacy_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.pid");
        let pid = std::process::id() as i32;
        let started = crate::jobs::process_identity(pid as u32).unwrap().unwrap();
        std::fs::write(&path, pid.to_string()).unwrap();
        std::fs::write(
            super::identity_path(&path),
            serde_json::to_vec(&super::PidIdentity { pid, started }).unwrap(),
        )
        .unwrap();
        assert_eq!(super::recorded_pid(&path).unwrap(), pid);
        std::fs::write(
            super::identity_path(&path),
            serde_json::to_vec(&super::PidIdentity {
                pid,
                started: "alpha-stale-identity".into(),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(
            super::recorded_pid(&path)
                .unwrap_err()
                .to_string()
                .contains("identity no longer matches")
        );
        std::fs::remove_file(super::identity_path(&path)).unwrap();
        assert_eq!(super::recorded_pid(&path).unwrap(), pid);
        // A legacy starter can replace the numeric pid without the sidecar.
        std::fs::write(
            super::identity_path(&path),
            serde_json::to_vec(&super::PidIdentity {
                pid: pid + 1,
                started: "alpha-previous-process".into(),
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(super::recorded_pid(&path).unwrap(), pid);
        for invalid in ["0", "-1", "{}", "{\"pid\":1}"] {
            std::fs::write(&path, invalid).unwrap();
            assert!(super::recorded_pid(&path).is_err());
        }
    }

    /// Write an executable script, and wait until it can run: a test on
    /// another thread that forks while it is open for writing holds it
    /// busy (ETXTBSY) until that child execs. A spawn that starts proves no
    /// writer is left; it is killed with what it started.
    pub(crate) fn script(file: &std::path::Path, body: &str) -> String {
        std::fs::write(file, body).unwrap();
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755)).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            match std::process::Command::new(file)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .process_group(0)
                .spawn()
            {
                Ok(mut child) => {
                    unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
                    let _ = child.wait();
                    break;
                }
                Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) => {
                    assert!(Instant::now() < until, "{} stayed busy", file.display());
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("{}: {error}", file.display()),
            }
        }
        file.to_string_lossy().into_owned()
    }

    /// A login shell whose rc file prints a banner before running the
    /// command, as many server images do.
    fn shell_printing(dir: &std::path::Path, banner: &str, path: &str) -> String {
        script(
            &dir.join("shell"),
            // `-c` alone, not `-lc`: a profile that resets PATH must not
            // decide this test.
            &format!(
                "#!/bin/sh\nprintf '%s\\n' '{banner}'\nPATH='{path}' exec /bin/sh -c \"$2\"\n"
            ),
        )
    }

    #[test]
    fn a_login_shell_banner_never_becomes_path_entries() {
        let dir = tempfile::tempdir().unwrap();
        let shell = shell_printing(dir.path(), "Welcome to: the server", "/opt/login/bin");
        let (path, why) =
            super::login_path_from(Some(&shell), Instant::now() + Duration::from_secs(5));
        assert_eq!(why, None);
        let entries: Vec<_> = std::env::split_paths(&path).collect();
        assert!(
            entries.contains(&std::path::PathBuf::from("/opt/login/bin")),
            "{path}"
        );
        assert!(!path.contains("Welcome"), "{path}");
        assert!(!path.contains("the server"), "{path}");
    }

    #[test]
    fn text_printed_after_the_command_never_joins_the_last_entry() {
        let dir = tempfile::tempdir().unwrap();
        let shell = script(
            &dir.path().join("shell"),
            "#!/bin/sh\nPATH='/opt/login/bin' exec /bin/sh -c \"trap 'echo bye' EXIT; $2\"\n",
        );
        let (path, why) =
            super::login_path_from(Some(&shell), Instant::now() + Duration::from_secs(5));
        assert_eq!(why, None);
        let entries: Vec<_> = std::env::split_paths(&path).collect();
        assert!(
            entries.contains(&std::path::PathBuf::from("/opt/login/bin")),
            "{path}"
        );
        assert!(!path.contains("bye"), "{path}");
    }

    #[test]
    fn a_login_shell_slower_than_a_probe_is_waited_for_until_its_deadline() {
        let dir = tempfile::tempdir().unwrap();
        // An rc file that takes longer than a capability probe may.
        let shell = script(
            &dir.path().join("shell"),
            "#!/bin/sh\nsleep 6\nPATH='/opt/slow/bin' exec /bin/sh -c \"$2\"\n",
        );
        let (path, why) =
            super::login_path_from(Some(&shell), Instant::now() + Duration::from_secs(20));
        assert_eq!(why, None, "{path}");
        let entries: Vec<_> = std::env::split_paths(&path).collect();
        assert!(
            entries.contains(&std::path::PathBuf::from("/opt/slow/bin")),
            "{path}"
        );
    }

    #[test]
    fn a_login_shell_past_its_deadline_says_it_timed_out() {
        let dir = tempfile::tempdir().unwrap();
        let shell = script(&dir.path().join("shell"), "#!/bin/sh\nexec sleep 30\n");
        let begun = Instant::now();
        let (path, why) =
            super::login_path_from(Some(&shell), Instant::now() + Duration::from_millis(300));
        assert!(begun.elapsed() < Duration::from_secs(3));
        assert_eq!(why.as_deref(), Some("the login shell timed out"));
        assert_eq!(path, super::login_path_from(None, Instant::now()).0);
    }

    #[test]
    fn a_login_shell_that_fails_or_prints_no_path_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let own = super::login_path_from(None, Instant::now()).0;
        let deadline = || Instant::now() + Duration::from_secs(5);
        let shell = script(&dir.path().join("failing"), "#!/bin/sh\nexit 3\n");
        let (path, why) = super::login_path_from(Some(&shell), deadline());
        assert_eq!(why.as_deref(), Some("the login shell exited with 3"));
        assert_eq!(path, own);
        // An rc file that exits before the command runs.
        let shell = script(&dir.path().join("silent"), "#!/bin/sh\necho motd\n");
        let (path, why) = super::login_path_from(Some(&shell), deadline());
        assert_eq!(why.as_deref(), Some("the login shell printed no PATH"));
        assert_eq!(path, own);
        // No login shell to ask is no failure.
        assert_eq!(super::login_path_from(Some("sh"), deadline()), (own, None));
    }

    #[test]
    fn adoption_repoints_what_start_writes_and_then_leaves_it() {
        let home = std::path::Path::new("/ships/a \"b\"");
        let old = std::path::Path::new("/old/bin/privateer");
        let new = std::path::Path::new("/data/privateer/current/bin/privateer");
        let dispatcher = super::dispatcher(home, "a");
        for (launchd, text) in [
            (false, super::unit(&dispatcher, old)),
            (true, super::property_list(&dispatcher, old)),
        ] {
            let mut installed = super::Installed {
                path: "/u".into(),
                launchd,
                dispatcher: true,
                text,
            };
            assert_eq!(super::program(&installed).unwrap(), old);
            let repointed = super::repointed(&installed, new).unwrap().unwrap();
            installed.text = repointed;
            assert_eq!(super::program(&installed).unwrap(), new);
            // The arguments, the home's escaping included, are kept.
            let home = home.to_string_lossy();
            let kept = if launchd {
                super::xml(&home)
            } else {
                super::quote(&home)
            };
            assert!(installed.text.contains(&kept), "{}", installed.text);
            assert_eq!(super::repointed(&installed, new).unwrap(), None);
        }
        // An older dispatcher unit with systemd's control-group default gets
        // the safe stop policy too, even when it already runs `new`.
        let legacy =
            super::unit(&dispatcher, new).replace("KillMode=process", "KillMode=control-group");
        let installed = super::Installed {
            path: "/u".into(),
            launchd: false,
            dispatcher: true,
            text: legacy,
        };
        let fixed = super::repointed(&installed, new).unwrap().unwrap();
        assert!(fixed.contains("\nKillMode=process\n") && !fixed.contains("control-group"));
    }

    #[test]
    fn adoption_finds_an_unquoted_program_after_blanks_and_refuses_the_rest() {
        let new = std::path::Path::new("/data/privateer/current/bin/privateer");
        let unit = |exec: &str| super::Installed {
            path: "/u".into(),
            launchd: false,
            dispatcher: false,
            text: format!("[Service]\n{exec}\nKillMode=control-group\n"),
        };
        for exec in [
            "ExecStart=  /old/bin/privateer door run tg",
            "ExecStart = \t/old/bin/privateer door run tg",
            " ExecStart=\"/old/bin/privateer\" door run tg",
        ] {
            let installed = unit(exec);
            assert_eq!(
                super::program(&installed).unwrap(),
                std::path::Path::new("/old/bin/privateer"),
                "{exec}"
            );
            let text = super::repointed(&installed, new).unwrap().unwrap();
            assert!(
                text.contains(&format!(
                    "{} door run tg\n",
                    super::quote(&new.to_string_lossy())
                )) && !text.contains("/old/"),
                "{exec}: {text}"
            );
        }
        for exec in [
            "ExecStart=-/old/bin/privateer door run tg",
            "ExecStart=privateer door run tg",
            "ExecStart='/old/bin/privateer' door run tg",
            "ExecStart=/old/bin/priv\\x20ateer door run tg",
            "ExecStart=",
        ] {
            let error = super::program(&unit(exec)).unwrap_err().to_string();
            assert!(error.contains("change it by hand"), "{exec}: {error}");
            assert!(super::repointed(&unit(exec), new).is_err(), "{exec}");
        }
    }

    #[test]
    fn an_agent_is_a_dispatcher_or_a_door_by_its_exact_arguments() {
        let old = std::path::Path::new("/old/bin/privateer");
        let dispatcher = super::property_list(&super::dispatcher("/ships/a".as_ref(), "a"), old);
        assert_eq!(super::dispatcher_or_door(&dispatcher, true), Some(true));
        // A door named `serve` is a door: its process group stops with it.
        let door = |name: &str| {
            dispatcher.replace(
                "<string>--home</string><string>/ships/a</string><string>serve</string>",
                &format!("<string>door</string><string>run</string><string>{name}</string>"),
            )
        };
        assert_eq!(super::dispatcher_or_door(&door("serve"), true), Some(false));
        assert_eq!(super::dispatcher_or_door(&door("tg"), true), Some(false));
        let other = dispatcher.replace("<string>serve</string>", "<string>rpc</string>");
        assert_eq!(super::dispatcher_or_door(&other, true), None);
    }

    #[test]
    fn output_without_the_mark_adds_nothing() {
        assert_eq!(super::marked_path("Welcome\n/usr/bin"), None);
        assert_eq!(
            super::marked_path("motd\nprivateer-login-path:/a:/b\n").as_deref(),
            Some("/a:/b")
        );
        // An EXIT trap or a logout file printing after the command.
        assert_eq!(
            super::marked_path("privateer-login-path:/a:/b\nbye\n").as_deref(),
            Some("/a:/b")
        );
        assert_eq!(
            super::marked_path("privateer-login-path:").as_deref(),
            Some("")
        );
    }
}
