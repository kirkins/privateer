//! Build output kept in released worktree slots. A slot's `target` directory
//! can run to gigabytes after its task ends, so released slots are trimmed,
//! least recently released first, until the pool is within its budget and
//! the disk has the space being asked for. Only a `target` directory is
//! removed, and only from a slot whose worktree is clean and tracks nothing
//! under `target`, so source, uncommitted and untracked work stay put.
use super::*;
use crate::admission;

/// The Treehouse pool a trim works on: the helper and repository that list
/// its slots, and the pool root.
pub(crate) struct Pool {
    pub(crate) binary: PathBuf,
    pub(crate) git_repo: PathBuf,
    pub(crate) root: PathBuf,
}

const TARGET: &str = "target";

/// Remember when a slot was released, which orders the trims.
pub(super) fn record_release(store: &Store, slot: &Path) -> Result<()> {
    store.connection().execute(
        "INSERT INTO workspace_slot_use(path,released_ms) VALUES(?1,?2)
         ON CONFLICT(path) DO UPDATE SET released_ms=excluded.released_ms",
        params![slot.to_string_lossy(), Store::now_ms()],
    )?;
    Ok(())
}

fn released_ms(store: &Store, slot: &Path) -> Result<i64> {
    Ok(store
        .connection()
        .query_row(
            "SELECT released_ms FROM workspace_slot_use WHERE path=?1",
            [slot.to_string_lossy()],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

/// The pool a home's workspaces came from, from any recorded workspace.
/// None when the home has never allocated one, so there is nothing to trim.
pub(crate) fn pool_of_home(store: &Store) -> Result<Option<Pool>> {
    let record: Option<String> = store
        .connection()
        .query_row("SELECT record FROM workspaces LIMIT 1", [], |r| r.get(0))
        .optional()?;
    let Some(record) = record else {
        return Ok(None);
    };
    let w: Workspace = serde_json::from_str(&record)?;
    Ok(w.pool.is_dir().then_some(Pool {
        binary: w.binary,
        git_repo: w.git_repo,
        root: w.pool,
    }))
}

/// Slots no task holds, by path.
fn free_slots(lock: &Lock, pool: &Pool) -> Result<Vec<PathBuf>> {
    let mut cmd = Command::new(&pool.binary);
    cmd.current_dir(&pool.git_repo)
        .arg("--root")
        .arg(&pool.root)
        .args(["status", "--json"])
        .env("GIT_TERMINAL_PROMPT", "0");
    let all: Vec<Value> = serde_json::from_slice(&process::run(cmd, lock.0.as_raw_fd())?)
        .context("Treehouse must support JSON status and durable lease identities")?;
    Ok(all
        .iter()
        .filter(|s| s["lease_id"].as_str().is_none_or(str::is_empty))
        .filter_map(|s| s["path"].as_str().map(PathBuf::from))
        .collect())
}

/// Bytes under a path, without following symlinks.
fn bytes_under(path: &Path) -> Result<u64> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return Ok(meta.len());
    }
    let mut total = 0;
    for entry in fs::read_dir(path)? {
        total += bytes_under(&entry?.path())?;
    }
    Ok(total)
}

/// A slot's build output may go only when its worktree is clean and tracks
/// nothing under `target`: untracked and modified work is never trimmed.
fn trimmable(lock: &Lock, slot: &Path) -> bool {
    let clean = |args: &[&str]| git(lock, slot, args).is_ok_and(|out| out.is_empty());
    clean(&["status", "--porcelain=v1", "--untracked-files=all"])
        && clean(&["ls-files", "--", TARGET])
}

/// Trim released slots of `pool`, least recently released first, until the
/// pool is within its budget and, when given, `target_free` bytes are free on
/// its filesystem. Each trim is recorded so no reservation credits it.
/// Returns the bytes trimmed.
pub(crate) fn trim(
    store: &Store,
    lock: &Lock,
    pool: &Pool,
    target_free: Option<u64>,
) -> Result<u64> {
    let ledger = crate::jobs::shared_ledger(store)?;
    let budget = admission::settings(&ledger)?.workspace_cache_budget_bytes;
    let disk = admission::disk_key(&pool.root)?;
    let mut candidates = vec![];
    for slot in free_slots(lock, pool)? {
        let target = slot.join(TARGET);
        // symlink_metadata: a symlinked target is not a directory to remove.
        if !fs::symlink_metadata(&target).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        let bytes = bytes_under(&target)?;
        candidates.push((released_ms(store, &slot)?, slot, bytes));
    }
    candidates.sort();
    let mut total: u64 = candidates.iter().map(|c| c.2).sum();
    let mut freed = 0;
    for (_, slot, bytes) in candidates {
        let short = match target_free {
            Some(want) => admission::disk(&pool.root)?.0 < want,
            None => false,
        };
        if total <= budget && !short {
            break;
        }
        if !trimmable(lock, &slot) {
            continue;
        }
        fs::remove_dir_all(slot.join(TARGET))?;
        total -= bytes;
        freed += bytes;
        let free = admission::disk(&pool.root)?.0;
        admission::record_trim(&ledger, &disk, bytes, free, Store::now_ms())?;
    }
    Ok(freed)
}

/// Before a workspace is allocated: when the pool's filesystem cannot hold
/// the floor plus what held jobs still have to write, trim until it can.
pub(super) fn make_room(store: &Store, lock: &Lock, pool: &Pool, charter_floor: u64) -> Result<()> {
    let ledger = crate::jobs::shared_ledger(store)?;
    let floor = admission::settings(&ledger)?
        .min_free_disk_bytes
        .max(charter_floor);
    let key = admission::disk_key(&pool.root)?;
    let available = admission::disk(&pool.root)?.0;
    let needed = floor.saturating_add(crate::jobs::committed_disk_on(&ledger, &key, available)?);
    if available < needed {
        trim(store, lock, pool, Some(needed))?;
    }
    Ok(())
}

/// Before a job is admitted: when its declared disk would not fit on the
/// filesystem it will use, trim the pool of this home if that is the same
/// filesystem. Best effort; admission still decides.
pub(crate) fn make_room_for_job(
    store: &Store,
    held: Option<&Lock>,
    cwd: &Path,
    charter_floor: u64,
    demand: Option<admission::Demand>,
) -> Result<()> {
    let Some(demand) = demand.filter(|d| d.disk_bytes > 0) else {
        return Ok(());
    };
    let Some(pool) = pool_of_home(store)? else {
        return Ok(());
    };
    if admission::disk_key(cwd)? != admission::disk_key(&pool.root)? {
        return Ok(());
    }
    let own;
    let lock = match held {
        Some(lock) => lock,
        None => {
            own = Lock::acquire(store)?;
            &own
        }
    };
    let ledger = crate::jobs::shared_ledger(store)?;
    let floor = admission::settings(&ledger)?
        .min_free_disk_bytes
        .max(charter_floor);
    let key = admission::disk_key(cwd)?;
    let available = admission::disk(cwd)?.0;
    let needed = floor
        .saturating_add(crate::jobs::committed_disk_on(&ledger, &key, available)?)
        .saturating_add(demand.disk_bytes);
    if available < needed {
        trim(store, lock, &pool, Some(needed))?;
    }
    Ok(())
}
