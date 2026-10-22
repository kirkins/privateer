//! Versioned installation identities and upgrade reporting. This module
//! switches, prunes and restarts nothing; `fetch` prepares a release without
//! selecting it, and `adopt` selects the first one on a machine that had
//! none. A running image always belongs to its own immutable release.
pub mod adopt;
pub mod fetch;

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_KEEP: usize = 3;

pub fn valid_commit(commit: &str) -> bool {
    matches!(commit.len(), 40 | 64)
        && commit
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// `$XDG_DATA_HOME/privateer`, or `~/.local/share/privateer`.
pub fn data_dir() -> Result<PathBuf> {
    let data = match std::env::var_os("XDG_DATA_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => {
            PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".local/share")
        }
    };
    ensure!(
        data.is_absolute(),
        "the release data directory must be absolute"
    );
    Ok(data.join("privateer"))
}

/// The running image's real path. Linux reports it already resolved; macOS
/// reports the invoked path (often through `current`), so it is resolved once,
/// before an upgrade can move `current`. Entry points call this at startup.
pub fn running_image() -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        Ok(std::env::current_exe()?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        static IMAGE: std::sync::OnceLock<std::result::Result<PathBuf, String>> =
            std::sync::OnceLock::new();
        IMAGE
            .get_or_init(|| {
                std::env::current_exe()
                    .and_then(fs::canonicalize)
                    .map_err(|e| e.to_string())
            })
            .clone()
            .map_err(|e| anyhow::anyhow!("cannot locate the running Privateer image: {e}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub root: PathBuf,
    pub commit: String,
}

/// Every executable a complete release ships in `bin/`.
pub const BINARIES: [&str; 3] = ["privateer", "privateer-dashboard", "privateer-mcp-broker"];

impl Release {
    pub fn new(data: &Path, commit: &str) -> Result<Self> {
        ensure!(
            data.is_absolute(),
            "the release data directory must be absolute"
        );
        ensure!(
            valid_commit(commit),
            "release commit must be a full lowercase Git object id"
        );
        Ok(Self {
            root: data.join("releases").join(commit),
            commit: commit.into(),
        })
    }

    /// `executable` must be the real image path, not the `current` symlink.
    pub fn from_executable(executable: &Path) -> Option<Self> {
        let bin = executable.parent()?;
        if bin.file_name()? != "bin" {
            return None;
        }
        let root = bin.parent()?;
        if root.parent()?.file_name()? != "releases" {
            return None;
        }
        let commit = root.file_name()?.to_str()?;
        valid_commit(commit).then(|| Self {
            root: root.into(),
            commit: commit.into(),
        })
    }

    pub fn binary(&self, name: &str) -> Result<PathBuf> {
        ensure!(BINARIES.contains(&name), "unknown release binary");
        Ok(self.root.join("bin").join(name))
    }

    pub fn plugins(&self) -> PathBuf {
        self.root.join("share/privateer/plugins")
    }

    pub fn entrypoint(&self, name: &str) -> Result<PathBuf> {
        self.binary(name)?;
        Ok(self
            .root
            .parent()
            .context("releases directory")?
            .parent()
            .context("release data directory")?
            .join("current/bin")
            .join(name))
    }
}

/// Stored in the fleet registry by the future upgrade orchestrator. Each ship
/// outcome is explicit; an attended session is not reported as restarted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShipOutcome {
    pub ship: String,
    pub outcome: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpgradeRecord {
    pub id: String,
    /// `adopt` for a machine's move onto releases; absent for an upgrade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub from_commit: Option<String>,
    pub to_commit: String,
    pub actor: String,
    pub started_ms: i64,
    pub completed_ms: i64,
    pub ships: Vec<ShipOutcome>,
}

pub(crate) fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS upgrades(
        seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL, record TEXT NOT NULL);",
    )?;
    Ok(())
}

/// Internal persistence seam for the guarded orchestrator, not an agent or
/// operator command. Conflicting retries cannot overwrite a recorded outcome.
pub fn record_upgrade(conn: &Connection, record: &UpgradeRecord) -> Result<()> {
    ensure!(
        valid_commit(&record.to_commit) && record.from_commit.as_deref().is_none_or(valid_commit),
        "upgrade history requires exact commits"
    );
    ensure!(
        !record.id.is_empty() && !record.actor.is_empty(),
        "upgrade id and actor are required"
    );
    ensure!(
        record.started_ms <= record.completed_ms,
        "upgrade completion precedes start"
    );
    init(conn)?;
    let raw = serde_json::to_string(record)?;
    conn.execute(
        "INSERT INTO upgrades(id,record) VALUES(?1,?2) ON CONFLICT(id) DO NOTHING",
        params![record.id, raw],
    )?;
    let saved: String = conn.query_row(
        "SELECT record FROM upgrades WHERE id=?1",
        [&record.id],
        |r| r.get(0),
    )?;
    ensure!(
        saved == raw,
        "upgrade id already records a different outcome"
    );
    Ok(())
}

/// The installation a process belongs to: its own release's, or the default
/// data directory for an unversioned executable.
fn installation(running: Option<&Release>) -> Result<PathBuf> {
    match running {
        Some(release) => Ok(release
            .root
            .parent()
            .and_then(Path::parent)
            .context("release data directory")?
            .to_path_buf()),
        None => data_dir(),
    }
}

/// The release `current` selects in an installation, or None if it is unset.
/// A dangling or foreign `current` is an error; nothing here repairs it.
fn selected(data: &Path) -> Result<Option<Release>> {
    let current = data.join("current");
    match fs::canonicalize(&current) {
        Ok(root) => {
            let release = Release::from_executable(&root.join("bin/privateer"))
                .context("current does not point to a versioned release")?;
            ensure!(
                release.root.parent() == Some(fs::canonicalize(data.join("releases"))?.as_path()),
                "current points outside this installation's releases directory"
            );
            Ok(Some(release))
        }
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && fs::symlink_metadata(&current).is_err() =>
        {
            Ok(None)
        }
        Err(error) => Err(error).context("cannot resolve the current release"),
    }
}

/// The release `current` selects for the installation this process belongs to.
pub fn current() -> Result<Option<Release>> {
    current_for(&running_image()?)
}

/// The release `current` selects for the installation `executable` belongs
/// to, so a process reports drift against its own install, not the caller's.
pub fn current_for(executable: &Path) -> Result<Option<Release>> {
    let running = Release::from_executable(executable);
    selected(&installation(running.as_ref())?)
}

/// Read-only status works outside a ship and never creates a fleet registry.
/// `current_commit` is the selected release; `running_commit` may be older.
pub fn status() -> Result<Value> {
    let executable = running_image()?;
    let running = Release::from_executable(&executable);
    let data = installation(running.as_ref())?;
    let current = data.join("current");
    let selected = selected(&data)?;
    let registry = crate::fleet::registry_path()?;
    ensure!(
        registry.is_absolute(),
        "the fleet registry path must be absolute"
    );
    let mut history = Vec::<UpgradeRecord>::new();
    if registry.exists() {
        let conn = Connection::open_with_flags(registry, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        let present: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='upgrades'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if present.is_some() {
            let mut statement =
                conn.prepare("SELECT record FROM upgrades ORDER BY seq DESC LIMIT 100")?;
            for raw in statement.query_map([], |r| r.get::<_, String>(0))? {
                history.push(serde_json::from_str(&raw?)?);
            }
        }
    }
    // Once `current` is set, a native executable at a launcher path (a later
    // `cargo install`) runs outside the releases until adopted again.
    let mut unversioned = Vec::new();
    if selected.is_some() {
        for dir in adopt::launcher_dirs(&executable, &data) {
            for name in BINARIES {
                let path = dir.join(name);
                if fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) && adopt::native(&path) {
                    unversioned.push(path);
                }
            }
        }
    }
    Ok(
        json!({"current_commit": selected.as_ref().map(|r| &r.commit),
        "running_commit": running.as_ref().map(|r| &r.commit),
        "current": current, "running_executable": executable,
        "launcher_unversioned": unversioned,
        "history": history, "history_limit": 100, "keep_releases": DEFAULT_KEEP}),
    )
}

pub fn cli(args: &[String], caller: &crate::store::Caller) -> Result<()> {
    let result = match args.first().map(String::as_str) {
        Some("status") if args.len() == 1 => status()?,
        // Preparing a release is the operator's; it switches nothing.
        Some("fetch") => {
            caller.require_operator("upgrade fetch")?;
            fetch::fetch(&fetch::Options::parse(&args[1..])?)?
        }
        // Adopting changes launchers and units: the operator's too.
        Some("adopt") => {
            caller.require_operator("upgrade adopt")?;
            adopt::adopt(&adopt::Options::parse(&args[1..])?, "operator")?
        }
        _ => bail!(
            "usage: privateer upgrade status | fetch COMMIT [--build] [--source CHECKOUT] [--repo OWNER/NAME] | adopt [--dry-run] [--commit COMMIT] (switching to a release is not implemented yet)"
        ),
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    if let Some(error) = result["error"].as_str() {
        bail!("upgrade adopt stopped: {error}; the report above says what was done");
    }
    Ok(())
}
