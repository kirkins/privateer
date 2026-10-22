//! `privateer upgrade adopt [--dry-run] [--commit SHA]`: move a machine onto
//! versioned releases once. It creates `current` (never switches it), turns
//! the unversioned launchers into links to `current/bin/<name>` and points
//! the dispatcher and door units at `current/bin/privateer`, so every path
//! already recorded keeps working and the next upgrade only moves `current`.
//! It builds, downloads and restarts nothing, and leaves the MCP broker's
//! installed copy alone. Every replacement is made beside its file and
//! renamed over it, so each file is old or new, never missing.
use super::{BINARIES, Release, UpgradeRecord, installation, running_image, selected};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::time::Duration;

const USAGE: &str = "usage: privateer upgrade adopt [--dry-run] [--commit COMMIT]";
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub dry_run: bool,
    pub commit: Option<String>,
}

impl Options {
    pub fn parse(args: &[String]) -> Result<Self> {
        let mut options = Self {
            dry_run: false,
            commit: None,
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--dry-run" => options.dry_run = true,
                "--commit" => {
                    let commit = args.next().context(USAGE)?;
                    ensure!(
                        super::valid_commit(commit),
                        "upgrade adopt takes a full lowercase commit, not {commit:?}"
                    );
                    options.commit = Some(commit.clone());
                }
                _ => bail!("{USAGE}"),
            }
        }
        Ok(options)
    }
}

/// Where a launcher may be: cargo's bin, the user's bin, and the running
/// executable's directory when it is not a release. The MCP broker's
/// installed copy and anything inside the installation are never launchers.
pub(super) fn launcher_dirs(image: &Path, data: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".cargo/bin"));
        dirs.push(home.join(".local/bin"));
    }
    if Release::from_executable(image).is_none() {
        dirs.extend(image.parent().map(Path::to_path_buf));
    }
    let real = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let data = real(data);
    let mut seen = Vec::new();
    dirs.retain(|dir| {
        let key = real(dir);
        let keep = dir.is_absolute() && !key.starts_with(&data) && !seen.contains(&key);
        seen.push(key);
        keep
    });
    dirs
}

/// Whether a file starts like a native executable (ELF or Mach-O), so a
/// script or wrapper at a launcher path is never run or replaced.
pub(super) fn native(path: &Path) -> bool {
    let mut magic = [0u8; 4];
    fs::File::open(path).is_ok_and(|mut f| f.read_exact(&mut magic).is_ok())
        && (magic == *b"\x7fELF"
            || [0xfeedface_u32, 0xfeedfacf, 0xcafebabe]
                .iter()
                .any(|m| magic == m.to_be_bytes() || magic == m.to_le_bytes()))
}

/// Prove a real file at a launcher path is this Privateer binary `name`,
/// or say why not. It is the running image itself, or a native executable
/// that, run in a throwaway HOME with no service manager, answers as that
/// binary: `privateer upgrade status` naming this very file as its
/// `running_executable`, the dashboard's help banner, the broker's version.
fn proven(path: &Path, name: &str, image: &Path, sandbox: &Path) -> Result<String, String> {
    let meta = fs::metadata(path).map_err(|e| e.to_string())?;
    if fs::metadata(image).is_ok_and(|i| i.dev() == meta.dev() && i.ino() == meta.ino()) {
        return Ok("the running image".into());
    }
    if !native(path) {
        return Err("not a native executable (a script or another file)".into());
    }
    let what = format!("{} {name}", path.display());
    let mut command = super::fetch::sandboxed(path, sandbox);
    match name {
        "privateer" => command.args(["upgrade", "status"]),
        "privateer-dashboard" => command.arg("--help"),
        _ => command.arg("--version"),
    };
    fs::create_dir_all(sandbox.join("user")).map_err(|e| e.to_string())?;
    let not_it = format!("it did not answer as Privateer's {name}");
    let output = super::fetch::checked(command, PROBE_TIMEOUT, &what)
        .map_err(|e| format!("{not_it} ({e:#})"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let ok = match name {
        "privateer" => serde_json::from_str::<Value>(&stdout).is_ok_and(|status| {
            status.get("keep_releases").is_some()
                && status["running_executable"]
                    .as_str()
                    .and_then(|p| fs::canonicalize(p).ok())
                    .is_some_and(|p| Some(p) == fs::canonicalize(path).ok())
        }),
        "privateer-dashboard" => stdout.starts_with("Privateer · bridge"),
        _ => stdout.starts_with("privateer-mcp-broker "),
    };
    if ok {
        Ok(format!("{name} answered as itself"))
    } else {
        Err(not_it)
    }
}

/// A change adopt makes, in order.
enum Step {
    Current { path: PathBuf, target: PathBuf },
    Launcher { path: PathBuf, entry: PathBuf },
    Unit { path: PathBuf, text: String },
    Reload,
}

impl Step {
    fn run(&self) -> Result<()> {
        match self {
            Step::Current { path, target }
            | Step::Launcher {
                path,
                entry: target,
            } => {
                let new = beside(path);
                clear(&new)?;
                symlink(target, &new)?;
                fs::rename(&new, path)?;
            }
            Step::Unit { path, text } => {
                let new = beside(path);
                clear(&new)?;
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&new)?;
                file.write_all(text.as_bytes())?;
                file.sync_all()?;
                fs::rename(&new, path)?;
            }
            // A rerun finds the units already rewritten and plans no reload.
            Step::Reload => crate::service::daemon_reload().context(
                "the units are rewritten; run `systemctl --user daemon-reload` once it works",
            )?,
        }
        Ok(())
    }
}

/// `<file>.new`, where a replacement is made before it is renamed over.
fn beside(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".new");
    path.with_file_name(name)
}

/// Remove a `.new` link or file a failed attempt left; never a directory.
fn clear(new: &Path) -> Result<()> {
    match fs::symlink_metadata(new) {
        Ok(meta) if meta.is_dir() => bail!("{} is a directory; remove it", new.display()),
        Ok(_) => fs::remove_file(new)?,
        Err(_) => {}
    }
    Ok(())
}

/// Serialises adoptions (and, later, switches) of one installation.
fn lock(data: &Path) -> Result<fs::File> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(data.join("upgrade.lock"))?;
    let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    ensure!(taken, "another upgrade of this installation is running");
    Ok(file)
}

/// Plan and, unless `dry_run`, make the adoption; one JSON object reports
/// each action. A failure stops at that action, and the report (with an
/// `error`) says which were done.
pub fn adopt(options: &Options, actor: &str) -> Result<Value> {
    let started_ms = crate::store::Store::now_ms();
    let image = running_image()?;
    let running = Release::from_executable(&image);
    let data = installation(running.as_ref())?;
    let commit = match (&running, &options.commit) {
        (Some(own), Some(commit)) if *commit != own.commit => bail!(
            "this privateer runs release {}; adopt onto its own release, or run releases/{commit}/bin/privateer upgrade adopt",
            own.commit
        ),
        (Some(own), _) => own.commit.clone(),
        (None, Some(commit)) => commit.clone(),
        (None, None) => bail!(
            "{} is not a versioned release; pass --commit COMMIT of a release `privateer upgrade fetch COMMIT` prepared",
            image.display()
        ),
    };
    let release = Release::new(&data, &commit)?;
    ensure!(
        fs::symlink_metadata(&release.root).is_ok(),
        "{} is missing; run `privateer upgrade fetch {commit}` first (adopt builds and downloads nothing)",
        release.root.display()
    );
    super::fetch::existing(&release.root, "exists")?;
    let _lock = if options.dry_run {
        None
    } else {
        Some(lock(&data)?)
    };

    let mut planned: Vec<(Value, Option<Step>)> = Vec::new();
    let current = data.join("current");
    match selected(&data).map_err(|e| anyhow!("{e:#}; adopt does not repair current"))? {
        Some(selected) if selected.commit == commit => planned.push((
            json!({"kind": "current", "path": current, "target": selected.root, "state": "already"}),
            None,
        )),
        Some(selected) => bail!(
            "current already selects {}, not {commit}; switching releases is not adoption's",
            selected.commit
        ),
        None => {
            let target = Path::new("releases").join(&commit);
            planned.push((
                json!({"kind": "current", "path": current, "target": target}),
                Some(Step::Current {
                    path: current.clone(),
                    target,
                }),
            ));
        }
    }

    let probe =
        std::env::temp_dir().join(format!("privateer-adopt-{}", uuid::Uuid::new_v4().simple()));
    let _probe = super::fetch::Staging(probe.clone());
    for dir in launcher_dirs(&image, &data) {
        for name in BINARIES {
            let path = dir.join(name);
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            let entry = release.entrypoint(name)?;
            let mut action = json!({"kind": "launcher", "path": path, "target": entry});
            let step = if meta.file_type().is_symlink() {
                let target = fs::read_link(&path)?;
                if dir.join(&target) == entry {
                    action["state"] = json!("already");
                } else {
                    action["state"] = json!("left");
                    action["reason"] = json!(format!(
                        "a link to {}, not to current; change it by hand",
                        target.display()
                    ));
                }
                None
            } else if !meta.is_file() {
                action["state"] = json!("left");
                action["reason"] = json!("not a file");
                None
            } else {
                match proven(&path, name, &image, &probe.join(name)) {
                    Ok(how) => {
                        action["proof"] = json!(how);
                        Some(Step::Launcher {
                            path: path.clone(),
                            entry,
                        })
                    }
                    Err(why) => {
                        action["state"] = json!("left");
                        action["reason"] = json!(why);
                        None
                    }
                }
            };
            planned.push((action, step));
        }
    }

    let binary = release.entrypoint("privateer")?;
    let mut reload = false;
    for installed in crate::service::installed_services()? {
        let manager = if installed.launchd {
            "launchd"
        } else {
            "systemd"
        };
        let service = if installed.dispatcher {
            "dispatcher"
        } else {
            "door"
        };
        let mut action = json!({"kind": "unit", "path": installed.path, "manager": manager,
            "service": service, "to": binary});
        let program = crate::service::program(&installed);
        let step = match &program {
            Ok(program) if program.file_name() != Some(std::ffi::OsStr::new("privateer")) => {
                action["from"] = json!(program);
                action["state"] = json!("left");
                action["reason"] = json!("it does not run privateer");
                None
            }
            Ok(program) => {
                action["from"] = json!(program);
                match crate::service::repointed(&installed, &binary)? {
                    None => {
                        action["state"] = json!("already");
                        None
                    }
                    Some(text) => {
                        reload |= !installed.launchd;
                        Some(Step::Unit {
                            path: installed.path.clone(),
                            text,
                        })
                    }
                }
            }
            Err(error) => {
                action["state"] = json!("left");
                action["reason"] = json!(format!("{error:#}"));
                None
            }
        };
        planned.push((action, step));
    }
    if reload {
        planned.push((
            json!({"kind": "reload", "manager": "systemd", "command": "systemctl --user daemon-reload"}),
            Some(Step::Reload),
        ));
    }

    let changes = planned.iter().filter(|(_, step)| step.is_some()).count();
    let mut actions = Vec::new();
    let mut error = None;
    for (mut action, step) in planned {
        if let Some(step) = step {
            action["state"] = json!(if options.dry_run {
                "would"
            } else if error.is_some() {
                "not_done"
            } else {
                match step.run() {
                    Ok(()) => "done",
                    Err(e) => {
                        action["error"] = json!(format!("{e:#}"));
                        error = Some(format!("{}: {e:#}", action["kind"]));
                        "failed"
                    }
                }
            });
        }
        actions.push(action);
    }
    let mut report = json!({"commit": commit, "release": release.root, "current": current,
    "dry_run": options.dry_run, "noop": changes == 0, "actions": actions,
    "restarted": [],
    "note": if changes == 0 {
        "already adopted; nothing to do"
    } else if options.dry_run {
        "dry run: nothing was changed"
    } else {
        "nothing was restarted: each service runs current/bin/privateer from its next restart; launchd reads a changed agent at its next bootstrap"
    }});
    if let Some(error) = error {
        report["error"] = json!(error);
    } else if changes > 0 && !options.dry_run {
        let record = UpgradeRecord {
            id: uuid::Uuid::new_v4().to_string(),
            kind: Some("adopt".into()),
            from_commit: None,
            to_commit: commit.clone(),
            actor: actor.into(),
            started_ms,
            completed_ms: crate::store::Store::now_ms().max(started_ms),
            ships: Vec::new(),
        };
        match crate::fleet::registry().and_then(|conn| super::record_upgrade(&conn, &record)) {
            Ok(()) => report["history"] = json!(record.id),
            Err(e) => report["error"] = json!(format!("recording the adoption: {e:#}")),
        }
    }
    Ok(report)
}
