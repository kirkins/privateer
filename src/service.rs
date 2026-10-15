//! Keeping a ship's dispatcher (`privateer serve`) running in the background.
//!
//! Where a systemd user manager is available the dispatcher is installed as a
//! user service, so it survives logouts and reboots; elsewhere it runs as a
//! detached process for this login. A dispatcher holding `<home>/serve.lock`
//! counts as running however it was started, so nothing starts twice.
//! `PRIVATEER_SERVICE=systemd|detached|none` overrides the choice and
//! `PRIVATEER_SYSTEMCTL` the systemctl binary (tests use both).
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const LOG_LIMIT: u64 = 1024 * 1024;

/// Whether a dispatcher currently holds the home's serve lock.
pub fn running(home: &Path) -> Result<bool> {
    let path = home.join("serve.lock");
    if !path.exists() {
        return Ok(false);
    }
    let lock = fs::OpenOptions::new().write(true).open(&path)?;
    let free = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    // Closing the descriptor releases a lock taken just to test it.
    Ok(!free)
}

fn systemctl() -> String {
    std::env::var("PRIVATEER_SYSTEMCTL").unwrap_or_else(|_| "systemctl".into())
}

fn mode() -> String {
    if let Ok(mode) = std::env::var("PRIVATEER_SERVICE") {
        return mode;
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

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A user service for the ship's dispatcher. A service does not see the
/// login shell's PATH, where harnesses usually live, so it records the
/// current one.
fn unit(home: &Path, ship: &str, binary: &Path) -> String {
    let mut environment = String::new();
    for name in ["PATH", "PRIVATEER_RESOURCE_HOME", "PRIVATEER_PLUGINS"] {
        if let Ok(value) = std::env::var(name) {
            environment.push_str(&format!(
                "Environment={}\n",
                quote(&format!("{name}={value}"))
            ));
        }
    }
    format!(
        "[Unit]\nDescription=Privateer dispatcher for ship {ship}\n\n[Service]\nExecStart={} --home {} serve\n{environment}Restart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
        quote(&binary.to_string_lossy()),
        quote(&home.to_string_lossy())
    )
}

fn detached(home: &Path, binary: &Path) -> Result<()> {
    let log = home.join("serve.log");
    if fs::metadata(&log).is_ok_and(|m| m.len() > LOG_LIMIT) {
        fs::rename(&log, home.join("serve.log.1"))?;
    }
    let out = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;
    let mut command = Command::new(binary);
    command
        .arg("--home")
        .arg(home)
        .arg("serve")
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
    let child = command.spawn().context("start the dispatcher")?;
    fs::write(home.join("serve.pid"), child.id().to_string())?;
    Ok(())
}

/// Make sure the ship's dispatcher is running; report how.
pub fn ensure(home: &Path, ship: &str) -> Result<String> {
    if running(home)? {
        return Ok("already running".into());
    }
    let binary = std::env::current_exe()?;
    match mode().as_str() {
        "none" => Ok("not started (PRIVATEER_SERVICE=none)".into()),
        "systemd" => {
            let name = unit_name(home, ship);
            let dir = unit_dir()?;
            fs::create_dir_all(&dir)?;
            fs::write(dir.join(&name), unit(home, ship, &binary))?;
            for args in [
                vec!["--user", "daemon-reload"],
                vec!["--user", "enable", "--now", name.as_str()],
            ] {
                let status = Command::new(systemctl()).args(&args).status()?;
                ensure!(status.success(), "systemctl {} failed", args.join(" "));
            }
            Ok(format!("installed as user service {name}"))
        }
        "detached" => {
            detached(home, &binary)?;
            Ok(format!(
                "started in the background (log: {})",
                home.join("serve.log").display()
            ))
        }
        other => bail!("PRIVATEER_SERVICE must be systemd, detached or none, not {other}"),
    }
}

/// Describe the ship's dispatcher.
pub fn status(home: &Path, ship: &str) -> Result<String> {
    let unit = unit_dir()?.join(unit_name(home, ship));
    let how = if unit.exists() {
        format!("user service {}", unit_name(home, ship))
    } else {
        "no user service".into()
    };
    Ok(format!(
        "Dispatcher for {ship}: {} ({how}).",
        if running(home)? {
            "running"
        } else {
            "not running"
        }
    ))
}

/// Stop the ship's dispatcher: disable and remove its user service, or end
/// the detached process recorded in serve.pid if it is still that process.
pub fn stop(home: &Path, ship: &str) -> Result<String> {
    let name = unit_name(home, ship);
    let unit = unit_dir()?.join(&name);
    if unit.exists() {
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
    let pid_file = home.join("serve.pid");
    if let Some(pid) = fs::read_to_string(&pid_file)
        .ok()
        .and_then(|p| p.trim().parse::<i32>().ok())
    {
        // Only signal the recorded process while it still holds the lock and
        // is this home's dispatcher, never a reused PID.
        let command = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()?;
        let line = String::from_utf8_lossy(&command.stdout);
        if running(home)? && line.contains("serve") && line.contains(&*home.to_string_lossy()) {
            unsafe { libc::kill(pid, libc::SIGTERM) };
            for _ in 0..50 {
                if !running(home)? {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let _ = fs::remove_file(&pid_file);
            return Ok(format!("Stopped the dispatcher (pid {pid})."));
        }
        let _ = fs::remove_file(&pid_file);
    }
    if running(home)? {
        bail!(
            "a dispatcher is running that Privateer did not start (perhaps `privateer serve` in a terminal); stop it there"
        )
    }
    Ok("The dispatcher was not running.".into())
}
