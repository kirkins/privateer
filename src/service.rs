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
struct Service {
    lock: PathBuf,
    unit: String,
    description: String,
    args: Vec<String>,
    environment: &'static [&'static str],
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
    let home_text = home.to_string_lossy().into_owned();
    Service {
        lock: home.join("serve.lock"),
        unit: unit_name(home, ship),
        description: format!("Privateer dispatcher for ship {ship}"),
        args: vec!["--home".into(), home_text.clone(), "serve".into()],
        environment: &[
            "PATH",
            "PRIVATEER_RESOURCE_HOME",
            "PRIVATEER_PLUGINS",
            "PRIVATEER_FLEET",
            "XDG_STATE_HOME",
        ],
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
            "PRIVATEER_FLEET",
            "PRIVATEER_RESOURCE_HOME",
            "PRIVATEER_PLUGINS",
            "XDG_STATE_HOME",
        ],
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

/// A user service. A service does not see the login shell's PATH, where
/// harnesses usually live, so it records the current one.
fn unit(service: &Service, binary: &Path) -> String {
    let mut environment = String::new();
    for name in service.environment {
        if let Ok(value) = std::env::var(name) {
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
        if let Ok(value) = std::env::var(name) {
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

fn detached(service: &Service, binary: &Path) -> Result<()> {
    let log = &service.log;
    rotate(log)?;
    let out = fs::OpenOptions::new().create(true).append(true).open(log)?;
    let mut command = Command::new(binary);
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
    command
        .args(&service.args)
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
    let child = command
        .spawn()
        .with_context(|| format!("start {}", service.what))?;
    fs::write(&service.pid, child.id().to_string())?;
    Ok(())
}

/// Make sure the ship's dispatcher is running; report how.
pub fn ensure(home: &Path, ship: &str) -> Result<String> {
    start(&dispatcher(home, ship), &home.join("serve.log"))
}

/// Make sure a front door's supervisor is running; report how.
pub fn ensure_door(name: &str) -> Result<String> {
    let service = door(name)?;
    let log = crate::door::directory(name)?.join("door.log");
    start(&service, &log)
}

fn start(service: &Service, log: &Path) -> Result<String> {
    if held(&service.lock)? {
        return Ok("already running".into());
    }
    let binary = crate::executable::installed()?;
    match mode().as_str() {
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
            detached(service, &binary)?;
            Ok(format!(
                "started in the background (log: {})",
                log.display()
            ))
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
    let how = if unit_dir()?.join(&service.unit).exists() {
        format!("user service {}", service.unit)
    } else if agent_path(service)?.exists() {
        format!("launchd agent {}", label(service))
    } else {
        "no user service".into()
    };
    Ok(format!(
        "{} ({how})",
        if held(&service.lock)? {
            "running"
        } else {
            "not running"
        }
    ))
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
    if let Some(pid) = fs::read_to_string(&service.pid)
        .ok()
        .and_then(|p| p.trim().parse::<i32>().ok())
    {
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
            ensure!(
                released,
                "{} (pid {pid}) did not stop within 10 seconds",
                service.what
            );
            return Ok(format!("Stopped {} (pid {pid}).", service.what));
        }
        let _ = fs::remove_file(&service.pid);
    }
    if held(&service.lock)? {
        bail!(
            "{} is running but Privateer did not start it ({elsewhere}); stop it there",
            service.what
        )
    }
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

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}
