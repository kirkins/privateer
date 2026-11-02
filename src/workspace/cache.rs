//! Build output kept in released worktree slots. A slot's `target` directory
//! can run to gigabytes after its task ends, so released slots are trimmed,
//! least recently released first, until the pool is within its budget and
//! the disk has the space being asked for. Only a `target` directory is
//! removed, and only from a slot whose worktree is clean and tracks nothing
//! under `target`, so source, uncommitted and untracked work stay put.
use super::*;
use crate::admission;
use std::{collections::HashSet, os::unix::fs::MetadataExt};

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

/// Slots no task holds and no process is running in, by path. A slot whose
/// processes cannot be read is not free.
fn free_slots(lock: &Lock, pool: &Pool) -> Result<Vec<PathBuf>> {
    let mut cmd = Command::new(&pool.binary);
    cmd.current_dir(&pool.git_repo)
        .arg("--root")
        .arg(&pool.root)
        .args(["status", "--json"])
        .env("GIT_TERMINAL_PROMPT", "0");
    let all: Vec<Value> =
        serde_json::from_slice(&process::Operation::TreehouseStatus.run(cmd, lock.0.as_raw_fd())?)
            .context("Treehouse must support JSON status and durable lease identities")?;
    Ok(all
        .iter()
        .filter(|s| s["lease_id"].as_str().is_none_or(str::is_empty))
        .filter(|s| s["processes"].as_array().is_some_and(Vec::is_empty))
        .filter_map(|s| s["path"].as_str().map(PathBuf::from))
        .collect())
}

/// Disk allocated under a path, without following symlinks. Each file is
/// counted once however many hard links name it, as the disk counts it: a
/// build's binaries are hard-linked, and summing their lengths overstates the
/// space a trim returns.
fn allocated_under(path: &Path, seen: &mut HashSet<(u64, u64)>) -> Result<u64> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        let mut total = 0;
        for entry in fs::read_dir(path)? {
            total += allocated_under(&entry?.path(), seen)?;
        }
        return Ok(total);
    }
    if !seen.insert((meta.dev(), meta.ino())) {
        return Ok(0);
    }
    Ok(meta.blocks() * 512)
}

/// A pool's trim failure is audited and the operation that asked for room
/// goes on: admission decides whether the room is there.
fn audit_trim_failure(store: &Store, pool: &Pool, error: &anyhow::Error) -> Result<()> {
    store.audit(
        "workspace.trim_failed",
        &json!({"pool": pool.root, "error": format!("{error:#}")}),
    )?;
    Ok(())
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
/// its filesystem. A slot that cannot be read or removed is skipped and
/// audited, so one bad slot does not stop the rest. The trimmed space is
/// recorded once, as the smaller of what the slots counted and what free
/// space rose by, so no reservation credits space that was not returned.
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
    let root = fs::canonicalize(&pool.root)?;
    let mut candidates = vec![];
    for slot in free_slots(lock, pool)? {
        // The slot must resolve inside the pool, so a link cannot send a
        // removal elsewhere.
        let Ok(real) = fs::canonicalize(&slot) else {
            continue;
        };
        if !real.starts_with(&root) {
            continue;
        }
        let target = real.join(TARGET);
        // symlink_metadata: a symlinked target is not a directory to remove.
        if !fs::symlink_metadata(&target).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        match allocated_under(&target, &mut HashSet::new()) {
            Ok(bytes) => candidates.push((released_ms(store, &slot)?, real, bytes)),
            Err(error) => audit_skip(store, &slot, &error)?,
        }
    }
    candidates.sort();
    let mut total: u64 = candidates.iter().map(|c| c.2).sum();
    let before = admission::disk(&pool.root)?.0;
    let mut counted = 0;
    for (_, slot, bytes) in candidates {
        // Short is judged on the reading taken before, plus what has been
        // counted as freed since, not a fresh reading per removal.
        let short = target_free.is_some_and(|want| before.saturating_add(counted) < want);
        if total <= budget && !short {
            break;
        }
        if !trimmable(lock, &slot) {
            continue;
        }
        if let Err(error) = fs::remove_dir_all(slot.join(TARGET)) {
            audit_skip(store, &slot, &anyhow::Error::from(error))?;
            continue;
        }
        total -= bytes;
        counted += bytes;
    }
    if counted == 0 {
        return Ok(0);
    }
    let after = admission::disk(&pool.root)?.0;
    let freed = counted.min(after.saturating_sub(before));
    admission::record_trim(&ledger, &disk, freed, after, Store::now_ms())?;
    Ok(freed)
}

/// A slot the trim could not read or clear is left as it is, and audited.
fn audit_skip(store: &Store, slot: &Path, error: &anyhow::Error) -> Result<()> {
    store.audit(
        "workspace.trim_skipped",
        &json!({"slot": slot, "error": format!("{error:#}")}),
    )?;
    Ok(())
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
        if let Err(error) = trim(store, lock, pool, Some(needed)) {
            audit_trim_failure(store, pool, &error)?;
        }
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
    let ledger = crate::jobs::shared_ledger(store)?;
    let floor = admission::settings(&ledger)?
        .min_free_disk_bytes
        .max(charter_floor);
    let key = admission::disk_key(cwd)?;
    let available = admission::disk(cwd)?.0;
    let needed = floor
        .saturating_add(crate::jobs::committed_disk_on(&ledger, &key, available)?)
        .saturating_add(demand.disk_bytes);
    // The workspace lock is taken only when there is something to trim.
    if available >= needed {
        return Ok(());
    }
    let trimmed = match held {
        Some(lock) => trim(store, lock, &pool, Some(needed)),
        None => Lock::acquire(store).and_then(|own| trim(store, &own, &pool, Some(needed))),
    };
    if let Err(error) = trimmed {
        audit_trim_failure(store, &pool, &error)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hard_linked_file_is_counted_once() {
        let temp = tempfile::tempdir().unwrap();
        let one = temp.path().join("one");
        fs::create_dir(&one).unwrap();
        fs::write(one.join("binary"), vec![1u8; 256 * 1024]).unwrap();
        let alone = allocated_under(&one, &mut HashSet::new()).unwrap();
        // A second name for the same file adds no disk.
        fs::hard_link(one.join("binary"), one.join("linked")).unwrap();
        let linked = allocated_under(&one, &mut HashSet::new()).unwrap();
        assert_eq!(linked, alone);
        assert!(alone >= 256 * 1024, "{alone}");
    }
}
