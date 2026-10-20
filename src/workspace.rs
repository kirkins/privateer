//! Built-in Treehouse workspaces. A lease belongs to a task, not a process.
//! External operations have durable intents; an inherited flock fences helpers
//! across caller crashes. No job outcome implicitly discards or releases work.
mod cache;
mod lifecycle;
mod process;
pub(crate) use cache::make_room_for_job;
pub use lifecycle::dispatch;
pub(crate) use lifecycle::sweep;

use crate::store::{Caller, Store, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

pub(crate) struct Lock(File);
impl Lock {
    pub(crate) fn acquire(store: &Store) -> Result<Self> {
        let path = store.home().join("workspace.lock");
        crate::store::reject_symlinks(&path)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        let deadline = Instant::now() + Duration::from_secs(125);
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self(file));
            }
            let error = std::io::Error::last_os_error();
            ensure!(
                matches!(
                    error.raw_os_error(),
                    Some(libc::EWOULDBLOCK) | Some(libc::EINTR)
                ),
                "cannot lock workspaces: {error}"
            );
            ensure!(
                Instant::now() < deadline,
                "another workspace operation is still running"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Workspace {
    pub(crate) task_id: String,
    pub(crate) generation: String,
    pub(crate) charter_repo: PathBuf,
    pub(crate) repo: PathBuf,
    pub(crate) git_repo: PathBuf,
    pub(crate) pool: PathBuf,
    pub(crate) binary: PathBuf,
    pub(crate) holder: String,
    pub(crate) branch: String,
    pub(crate) base_branch: String,
    pub(crate) state: String,
    pub(crate) path: Option<PathBuf>,
    pub(crate) lease_id: Option<String>,
    pub(crate) archive_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) task_archive_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) switched_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) current_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) retained_reason: Option<String>,
}

pub(crate) struct Execution {
    pub(crate) root: PathBuf,
    pub(crate) source_root: PathBuf,
    pub(crate) lease_id: Option<String>,
    pub(crate) _lock: Option<Lock>,
}

pub(crate) fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS workspaces (task_id TEXT PRIMARY KEY REFERENCES tasks(id), record TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS workspace_slot_use (path TEXT PRIMARY KEY, released_ms INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS workspace_alerts (task_id TEXT PRIMARY KEY, raised_ms INTEGER NOT NULL, reason TEXT NOT NULL);",
    )?;
    Ok(())
}

fn load(store: &Store, task: &str) -> Result<Option<Workspace>> {
    store
        .connection()
        .query_row(
            "SELECT record FROM workspaces WHERE task_id=?1",
            [task],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
}
fn save(store: &Store, w: &Workspace) -> Result<()> {
    let tx = store.write_transaction()?;
    tx.execute("INSERT INTO workspaces(task_id,record) VALUES(?1,?2) ON CONFLICT(task_id) DO UPDATE SET record=excluded.record", params![w.task_id, serde_json::to_string(w)?])?;
    store.audit(
        "workspace.state",
        &json!({"task_id":w.task_id,"generation":w.generation,"state":w.state,"path":w.path,"lease_id":w.lease_id,"archive_ref":w.archive_ref,"switched_branch":w.switched_branch,"retained_reason":w.retained_reason}),
    )?;
    tx.commit()?;
    Ok(())
}
fn owner(store: &Store, task: &str) -> Result<Value> {
    let task = store.task(task)?;
    if let Caller::Agent { actor } = store.caller() {
        ensure!(
            task["owner"].as_str() == Some(actor),
            "only the task owner may use this workspace"
        );
    }
    Ok(task)
}
fn public(store: &Store, w: &Workspace) -> Result<Value> {
    let mut value = serde_json::to_value(w)?;
    value["owner"] = owner(store, &w.task_id)?["owner"].clone();
    Ok(value)
}
pub(crate) fn hash(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))[..16].to_owned()
}

fn git(lock: &Lock, repo: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    Ok(String::from_utf8(process::run(cmd, lock.0.as_raw_fd())?)?
        .trim()
        .to_owned())
}
fn treehouse(lock: &Lock, w: &Workspace, args: &[&str]) -> Result<Vec<u8>> {
    let mut cmd = Command::new(&w.binary);
    cmd.current_dir(&w.git_repo)
        .arg("--root")
        .arg(&w.pool)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    process::run(cmd, lock.0.as_raw_fd())
}
fn slots(lock: &Lock, w: &Workspace) -> Result<Vec<Value>> {
    serde_json::from_slice(&treehouse(lock, w, &["status", "--json"])?)
        .context("Treehouse must support JSON status and durable lease identities")
}
fn common(lock: &Lock, repo: &Path) -> Result<PathBuf> {
    Ok(fs::canonicalize(git(
        lock,
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?)?)
}
fn root(w: &Workspace) -> Result<PathBuf> {
    Ok(w.path
        .as_ref()
        .context("workspace allocation is incomplete")?
        .join(w.repo.strip_prefix(&w.git_repo)?))
}
fn validate_path(lock: &Lock, w: &Workspace) -> Result<()> {
    let path = w.path.as_ref().context("workspace path missing")?;
    crate::store::reject_symlinks(path)?;
    ensure!(
        path.is_absolute()
            && fs::canonicalize(path)? == *path
            && path.starts_with(&w.pool)
            && path != &w.git_repo,
        "Treehouse path is outside the managed pool or aliases the source checkout"
    );
    ensure!(
        fs::canonicalize(git(lock, path, &["rev-parse", "--show-toplevel"])?)? == *path,
        "workspace must be a Git worktree root"
    );
    ensure!(
        common(lock, path)? == common(lock, &w.git_repo)?,
        "workspace belongs to another Git clone"
    );
    ensure!(
        root(w)?.is_dir(),
        "charter directory is missing from the workspace"
    );
    Ok(())
}
pub(crate) fn sync_branch(lock: &Lock, w: &mut Workspace) -> bool {
    let Some(path) = &w.path else {
        return false;
    };
    if let Ok(head_branch) = git(lock, path, &["symbolic-ref", "--short", "HEAD"]) {
        let switched = if head_branch != w.branch {
            Some(head_branch.clone())
        } else {
            None
        };
        let current = Some(head_branch);
        if w.switched_branch != switched || w.current_branch != current {
            w.switched_branch = switched;
            w.current_branch = current;
            return true;
        }
    }
    false
}
fn validate(lock: &Lock, w: &Workspace) -> Result<()> {
    ensure!(
        w.state == "ready",
        "workspace is {}; recover with workspace.acquire or workspace.release",
        w.state
    );
    validate_path(lock, w)?;
    let all = slots(lock, w)?;
    ensure!(
        all.iter()
            .any(|slot| slot["lease_id"].as_str() == w.lease_id.as_deref()
                && slot["lease_holder"] == w.holder
                && slot["path"].as_str() == w.path.as_ref().and_then(|p| p.to_str())),
        "workspace lease changed or disappeared; refusing stale workspace identity"
    );
    Ok(())
}
fn binary(store: &Store) -> Result<PathBuf> {
    let config = store.home().join("config.json");
    let cfg: Value = if config.exists() {
        serde_json::from_slice(&fs::read(config)?)?
    } else {
        json!({})
    };
    let name = match cfg.get("treehouse_binary") {
        Some(v) => v
            .as_str()
            .context("treehouse_binary must be an executable path")?,
        None => "treehouse",
    };
    let path = Path::new(name);
    let candidates: Vec<PathBuf> = if path.is_absolute() {
        vec![path.to_owned()]
    } else {
        ensure!(
            !name.contains('/') && !name.is_empty(),
            "treehouse_binary must be an absolute path or executable name"
        );
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|p| p.join(name))
            .collect()
    };
    for path in candidates {
        if fs::metadata(&path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0) {
            return Ok(fs::canonicalize(path)?);
        }
    }
    bail!(
        "Treehouse is required for Git coding tasks; install Treehouse with get --lease --json support"
    )
}

fn acquire_locked(
    store: &Store,
    task: &str,
    charter_repo: &Path,
    repo: &Path,
    lock: &Lock,
    wait_for_capacity: bool,
    max_wait_secs: u64,
) -> Result<Workspace> {
    owner(store, task)?;
    let task_record = store.task(task)?;
    let old = load(store, task)?;
    let mut disk_queue = None;
    let mut w = if let Some(w) = old.filter(|w| w.state != "released") {
        ensure!(
            w.charter_repo == charter_repo && w.repo == repo,
            "charter or task project changed; release its previous workspace first"
        );
        w
    } else {
        let git_repo = fs::canonicalize(git(lock, repo, &["rev-parse", "--show-toplevel"])?)?;
        let base_branch = git(
            lock,
            &git_repo,
            &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        )
        .map(|s| s.strip_prefix("origin/").unwrap_or(&s).to_owned())
        .or_else(|_| {
            if git(lock, &git_repo, &["remote", "get-url", "origin"]).is_ok() {
                let advertised = git(
                    lock,
                    &git_repo,
                    &["ls-remote", "--symref", "origin", "HEAD"],
                )?;
                advertised
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("ref: refs/heads/")
                            .and_then(|s| s.split_whitespace().next())
                            .map(str::to_owned)
                    })
                    .context("cannot determine origin's default branch")
            } else {
                git(lock, &git_repo, &["symbolic-ref", "--short", "HEAD"])
            }
        })?;
        let home_hash = hash(&store.home().to_string_lossy());
        let pool = crate::jobs::ensure_ledger(store)?
            .parent()
            .context("ledger parent missing")?
            .join("worktrees")
            .join(&home_hash);
        crate::store::reject_symlinks(&pool)?;
        fs::create_dir_all(&pool)?;
        fs::set_permissions(&pool, fs::Permissions::from_mode(0o700))?;
        let charter_floor = store
            .charter(required(&task_record, "charter_id")?)?
            .get("min_free_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(2 * 1024 * 1024 * 1024);
        let binary = binary(store)?;
        let pool_root = fs::canonicalize(&pool)?;
        cache::make_room(
            store,
            lock,
            &cache::Pool {
                binary: binary.clone(),
                git_repo: git_repo.clone(),
                root: pool_root.clone(),
            },
            charter_floor,
        )?;
        disk_queue = Some(crate::jobs::wait_for_workspace_disk(
            store,
            task,
            &pool,
            charter_floor,
            wait_for_capacity,
            max_wait_secs,
        )?);
        disk_admission(store, task, &pool)?;
        let generation = uuid::Uuid::new_v4().simple().to_string();
        let w = Workspace {
            task_id: task.into(),
            generation: generation.clone(),
            charter_repo: charter_repo.into(),
            repo: repo.into(),
            git_repo,
            pool: fs::canonicalize(pool)?,
            binary,
            holder: format!("privateer:{home_hash}:{generation}"),
            branch: format!("privateer/{home_hash}/{}/{generation}", hash(task)),
            base_branch,
            state: "acquiring".into(),
            path: None,
            lease_id: None,
            archive_ref: None,
            task_archive_ref: None,
            switched_branch: None,
            current_branch: None,
            retained_reason: None,
        };
        save(store, &w)?;
        w
    };
    if w.state == "acquiring" {
        // The inherited lock proves that a prior helper has finished before
        // absence from status can permit another allocation with this holder.
        let held: Vec<Value> = slots(lock, &w)?
            .into_iter()
            .filter(|s| s["lease_holder"] == w.holder)
            .collect();
        ensure!(
            held.len() <= 1,
            "multiple Treehouse leases claim this task; preserve them for inspection"
        );
        let lease = match held.into_iter().next() {
            Some(v) => v,
            None => {
                let _queue = if disk_queue.is_none() {
                    let task_record = store.task(task)?;
                    let charter_floor = store
                        .charter(required(&task_record, "charter_id")?)?
                        .get("min_free_bytes")
                        .and_then(Value::as_u64)
                        .unwrap_or(2 * 1024 * 1024 * 1024);
                    cache::make_room(
                        store,
                        lock,
                        &cache::Pool {
                            binary: w.binary.clone(),
                            git_repo: w.git_repo.clone(),
                            root: w.pool.clone(),
                        },
                        charter_floor,
                    )?;
                    Some(crate::jobs::wait_for_workspace_disk(
                        store,
                        task,
                        &w.pool,
                        charter_floor,
                        wait_for_capacity,
                        max_wait_secs,
                    )?)
                } else {
                    None
                };
                disk_admission(store, task, &w.pool)?;
                serde_json::from_slice(&treehouse(
                    lock,
                    &w,
                    &[
                        "get",
                        "--lease",
                        "--json",
                        "--lease-holder",
                        &w.holder,
                        "--base",
                        &w.base_branch,
                        "--branch",
                        &w.branch,
                        "--worktree-path",
                        "{pool}/{slot}/{repo}",
                    ],
                )?)
                .context("invalid Treehouse lease response")?
            }
        };
        w.path = Some(PathBuf::from(required(&lease, "path")?));
        let id = required(&lease, "lease_id")?;
        ensure!(
            id.len() <= 128 && !id.contains('\0'),
            "invalid Treehouse lease identity"
        );
        w.lease_id = Some(id.to_owned());
        ensure!(
            lease["lease_holder"] == w.holder,
            "Treehouse lease holder mismatch"
        );
        validate_path(lock, &w)?;
        w.state = "ready".into();
        save(store, &w)?;
    }
    validate(lock, &w)?;
    Ok(w)
}

fn disk_admission(store: &Store, task: &str, pool: &Path) -> Result<()> {
    let task = store.task(task)?;
    let charter = store.charter(required(&task, "charter_id")?)?;
    let minimum = charter
        .get("min_free_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(2 * 1024 * 1024 * 1024);
    let ledger = crate::jobs::shared_ledger(store)?;
    let host_floor = crate::admission::settings(&ledger)?.min_free_disk_bytes;
    let host_available = crate::admission::disk(Path::new("/"))?.0;
    let pool_available = crate::admission::disk(pool)?.0;
    let available = host_available.min(pool_available);
    // Space that running jobs on this filesystem still have to write counts too.
    let held = crate::jobs::committed_disk_on(
        &ledger,
        &crate::admission::disk_key(pool)?,
        pool_available,
    )?;
    let required = minimum.max(host_floor).saturating_add(held);
    ensure!(
        available >= required,
        "workspace disk admission refused: {available} available, {required} required (host floor {host_floor}, charter floor {minimum}, {held} held by running jobs)"
    );
    Ok(())
}

pub(crate) fn contains_cwd(store: &Store, task: &str, repo: &Path, cwd: &Path) -> Result<bool> {
    match load(store, task)? {
        Some(w) if w.state == "ready" && w.charter_repo == repo => Ok(cwd.starts_with(root(&w)?)),
        _ => Ok(false),
    }
}

/// First-mate wake jobs and errands stay in their ship repository, headless.
/// Their rows are written before agent.start; a caller-supplied wake string is
/// not proof.
pub(crate) fn is_wake(store: &Store, args: &Value) -> Result<bool> {
    let Some(id) = args.get("id").and_then(Value::as_str) else {
        return Ok(false);
    };
    let wake: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM wakes WHERE job_id=?1 AND status='running')",
        [id],
        |r| r.get(0),
    )?;
    Ok(wake || crate::errand::running(store, id)?)
}
/// Whether and how long a workspace acquisition waits in the host disk queue.
pub(crate) struct WorkspaceWait {
    pub wait_for_capacity: bool,
    pub max_wait_secs: u64,
}
impl WorkspaceWait {
    pub(crate) fn none() -> Self {
        Self {
            wait_for_capacity: false,
            max_wait_secs: 0,
        }
    }
}
pub(crate) fn execution(
    store: &Store,
    task: &str,
    repo: &Path,
    selected: &Path,
    isolate: bool,
    expected: Option<&str>,
    wait: WorkspaceWait,
) -> Result<Execution> {
    owner(store, task)?;
    ensure!(selected.is_dir(), "cwd must be an existing directory");
    if !isolate {
        return Ok(Execution {
            root: repo.into(),
            source_root: repo.into(),
            lease_id: None,
            _lock: None,
        });
    }
    let lock = Lock::acquire(store)?;
    let exists = load(store, task)?;
    ensure!(
        selected.starts_with(repo)
            || exists.as_ref().is_some_and(
                |w| w.charter_repo == repo && root(w).is_ok_and(|r| selected.starts_with(r))
            ),
        "cwd must stay inside charter repository or its task workspace"
    );
    if let Some(expected) = expected {
        ensure!(
            exists.as_ref().and_then(|w| w.lease_id.as_deref()) == Some(expected)
                && exists.as_ref().is_some_and(|w| w.state == "ready"),
            "stale workspace lease; fresh agent planning required"
        );
    }
    // Ordinary non-Git charters remain usable without a Treehouse install.
    // A present .git ancestor that Git cannot read is an error, not fallback.
    let selected = if let Some(w) = &exists {
        ensure!(
            w.charter_repo == repo,
            "charter repository changed; release its previous workspace first"
        );
        if w.state != "released" {
            ensure!(
                selected == repo
                    || selected.starts_with(&w.repo)
                    || root(w).is_ok_and(|path| selected.starts_with(path)),
                "this task is bound to another project; create a child task or release its workspace first"
            );
            w.repo.clone()
        } else if selected == repo {
            w.repo.clone()
        } else {
            selected.to_owned()
        }
    } else {
        selected.to_owned()
    };
    let has_marker = selected
        .ancestors()
        .any(|p| p.join(".git").symlink_metadata().is_ok());
    if !has_marker && exists.as_ref().is_none_or(|w| w.state == "released") {
        return Ok(Execution {
            root: selected.clone(),
            source_root: selected,
            lease_id: None,
            _lock: Some(lock),
        });
    }
    let w = acquire_locked(
        store,
        task,
        repo,
        &selected,
        &lock,
        wait.wait_for_capacity,
        wait.max_wait_secs,
    )?;
    crate::jobs::workspace_previous_owner_idle(
        store,
        task,
        w.lease_id.as_deref().context("workspace lease missing")?,
    )?;
    Ok(Execution {
        root: root(&w)?,
        source_root: w.repo,
        lease_id: w.lease_id,
        _lock: Some(lock),
    })
}
pub(crate) fn planning_root(
    store: &Store,
    task: &str,
    repo: &Path,
    lease: &str,
) -> Result<PathBuf> {
    let exec = execution(
        store,
        task,
        repo,
        repo,
        true,
        Some(lease),
        WorkspaceWait::none(),
    )?;
    Ok(exec.root)
}
pub(crate) fn validate_job(
    store: &Store,
    task: &str,
    repo: &Path,
    cwd: &Path,
    lease: Option<&str>,
) -> Result<Option<Lock>> {
    if let Some(lease) = lease {
        let lock = Lock::acquire(store)?;
        let mut w = load(store, task)?.context("job workspace record missing")?;
        ensure!(
            w.charter_repo == repo && w.lease_id.as_deref() == Some(lease),
            "job workspace lease or charter changed"
        );
        validate(&lock, &w)?;
        if sync_branch(&lock, &mut w) {
            let _ = save(store, &w);
        }
        ensure!(
            fs::canonicalize(cwd)? == cwd && cwd.starts_with(root(&w)?),
            "job cwd escaped its task workspace"
        );
        Ok(Some(lock))
    } else {
        ensure!(
            fs::canonicalize(cwd)? == cwd && cwd.starts_with(repo),
            "job cwd changed outside charter before spawn"
        );
        Ok(None)
    }
}
