//! Explicit release and recovery keep unfinished work and stale leases intact.
use super::*;

/// The base branch that work must reach to count as landed. Fetches origin
/// first when there is one.
fn base_target(lock: &Lock, w: &Workspace) -> Result<String> {
    if git(lock, &w.git_repo, &["remote", "get-url", "origin"]).is_ok() {
        git(lock, &w.git_repo, &["fetch", "--quiet", "origin"])?;
        Ok(format!("refs/remotes/origin/{}", w.base_branch))
    } else {
        Ok(format!("refs/heads/{}", w.base_branch))
    }
}

/// Whether `rev` adds nothing to `target`: it is reachable from it, or a
/// squash or rebase merge left `target` with the same tree.
fn landed(lock: &Lock, w: &Workspace, target: &str, rev: &str) -> bool {
    let ahead: Result<u64> = git(
        lock,
        &w.git_repo,
        &["rev-list", "--count", &format!("{target}..{rev}")],
    )
    .and_then(|s| s.trim().parse().map_err(Into::into));
    if ahead.is_ok_and(|n| n == 0) {
        return true;
    }
    let merged = git(
        lock,
        &w.git_repo,
        &["merge-tree", "--write-tree", target, rev],
    );
    merged.is_ok_and(|m| {
        m.lines().next()
            == git(
                lock,
                &w.git_repo,
                &["rev-parse", &format!("{target}^{{tree}}")],
            )
            .ok()
            .as_deref()
    })
}

/// Keep a commit reachable under the workspace's generation before its
/// checkout is returned to the pool.
fn archive_tip(store: &Store, lock: &Lock, w: &Workspace, rev: &str) -> Result<String> {
    let reference = format!(
        "refs/privateer/workspaces/{}/{}/{}",
        hash(&store.home().to_string_lossy()),
        w.generation,
        rev
    );
    git(lock, &w.git_repo, &["update-ref", &reference, rev])?;
    Ok(reference)
}

/// Ask every MCP broker that opened a window for this workspace to close
/// it, before anything checks the checkout: an upstream's window can write
/// into the worktree and hold files there. Every way out of `ready` goes
/// through here. A running broker that cannot close its window keeps the
/// workspace; a stopped one's window is audited and the workspace goes.
fn release_mcp_windows(store: &Store, w: &Workspace) -> Result<()> {
    let Some(path) = w.path.as_ref() else {
        return Ok(());
    };
    let root = fs::canonicalize(path).unwrap_or_else(|_| path.clone());
    crate::mcp_broker::release_workspace(store, &root)
}

/// An agent releasing a live task's checkout needs the charter's cleanup
/// action. Release grants no new work and archives or proves landed what it
/// returns, so a done or cancelled task's owner may still release what it
/// holds, as long as the charter grants cleanup.
fn release_authority(store: &Store, task: &str) -> Result<()> {
    if !matches!(store.caller(), Caller::Agent { .. }) {
        return Ok(());
    }
    let record = store.task(task)?;
    if !matches!(record["status"].as_str(), Some("done" | "cancelled")) {
        store.authorize_running(task, "cleanup")?;
        return Ok(());
    }
    let charter = store.charter(
        record["charter_id"]
            .as_str()
            .context("task charter missing")?,
    )?;
    ensure!(
        charter["actions"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some("cleanup"))),
        "action cleanup is outside this charter"
    );
    Ok(())
}

fn release(store: &Store, args: &Value) -> Result<Value> {
    let task = required(args, "task_id")?;
    owner(store, task)?;
    release_authority(store, task)?;
    let lock = Lock::acquire(store)?;
    owner(store, task)?;
    release_authority(store, task)?;
    let mut w = load(store, task)?.context("task has no workspace")?;
    ensure!(
        w.lease_id.as_deref() == Some(required(args, "lease_id")?),
        "stale workspace release; lease_id does not match"
    );
    if w.state == "released" {
        return public(store, &w);
    }
    crate::jobs::workspace_idle(store, task, w.lease_id.as_deref().unwrap())?;
    let all = slots(&lock, &w)?;
    if w.state == "releasing"
        && !all
            .iter()
            .any(|s| s["lease_id"].as_str() == w.lease_id.as_deref())
    {
        w.state = "released".into();
        w.retained_reason = None;
        save(store, &w)?;
        let _ = store.connection().execute(
            "DELETE FROM workspace_alerts WHERE task_id=?1",
            [&w.task_id],
        );
        return public(store, &w);
    }
    ensure!(
        matches!(w.state.as_str(), "ready" | "releasing"),
        "finish workspace.acquire recovery before releasing"
    );
    release_mcp_windows(store, &w)?;
    w.state = "ready".into();
    validate(&lock, &w)?;
    sync_branch(&lock, &mut w);
    let path = w.path.clone().unwrap();
    ensure!(
        git(
            &lock,
            &path,
            &["status", "--porcelain=v1", "--untracked-files=all"]
        )?
        .is_empty(),
        "workspace has uncommitted work; commit or preserve it before release"
    );
    ensure!(
        slots(&lock, &w)?
            .iter()
            .find(|s| s["lease_id"].as_str() == w.lease_id.as_deref())
            .and_then(|s| s["processes"].as_array())
            .is_some_and(Vec::is_empty),
        "workspace still has processes or their absence cannot be verified"
    );
    let archive = args
        .get("archive")
        .map(|v| v.as_bool().context("archive must be boolean"))
        .transpose()?
        .unwrap_or(false);
    let head = git(&lock, &path, &["rev-parse", "HEAD"])?;
    // The recorded task branch can hold commits HEAD does not reach, whether
    // HEAD is on a branch, detached, or on another branch. A missing branch
    // after a switch cannot be verified, so it counts as unlanded.
    let task_tip = git(
        &lock,
        &path,
        &["rev-parse", "--verify", &format!("refs/heads/{}", w.branch)],
    )
    .ok();
    let task_check = w.switched_branch.is_some() || task_tip.as_deref().is_some_and(|t| t != head);
    if archive {
        w.archive_ref = Some(archive_tip(store, &lock, &w, &head)?);
        if let Some(tip) = task_tip.as_deref() {
            w.task_archive_ref = Some(archive_tip(store, &lock, &w, tip)?);
        }
    } else {
        let target = base_target(&lock, &w)?;
        ensure!(
            landed(&lock, &w, &target, &head),
            "workspace changes are not landed; use archive:true to preserve committed work"
        );
        if task_check {
            ensure!(
                task_tip
                    .as_deref()
                    .is_some_and(|tip| landed(&lock, &w, &target, tip)),
                "task branch {} changes are not landed; use archive:true to preserve committed work",
                w.branch
            );
        }
    }
    w.state = "releasing".into();
    owner(store, task)?;
    release_authority(store, task)?;
    save(store, &w)?;
    treehouse(
        &lock,
        &w,
        &[
            "return",
            "--if-lease-id",
            w.lease_id.as_deref().unwrap(),
            "--if-lease-holder",
            &w.holder,
            path.to_str().context("workspace path is not UTF-8")?,
        ],
    )?;
    ensure!(
        !slots(&lock, &w)?
            .iter()
            .any(|s| s["lease_id"].as_str() == w.lease_id.as_deref()),
        "Treehouse has not released the recorded lease"
    );
    w.state = "released".into();
    w.retained_reason = None;
    save(store, &w)?;
    let _ = store.connection().execute(
        "DELETE FROM workspace_alerts WHERE task_id=?1",
        [&w.task_id],
    );
    cache::record_release(store, &path)?;
    // The release stands even if trimming fails; the failure is kept.
    let pool = cache::Pool {
        binary: w.binary.clone(),
        git_repo: w.git_repo.clone(),
        root: w.pool.clone(),
    };
    if let Err(error) = cache::trim(store, &lock, &pool, None) {
        store.audit(
            "workspace.trim_failed",
            &json!({"pool": w.pool, "error": format!("{error:#}")}),
        )?;
    }
    public(store, &w)
}

/// The record as it is now, if it still carries the snapshot's lease. The
/// sweep reads its candidates before taking the lock, so a release and
/// reacquire may have replaced the record since. Callers hold the lock.
fn current(store: &Store, _lock: &Lock, snapshot: &Workspace) -> Result<Option<Workspace>> {
    Ok(load(store, &snapshot.task_id)?.filter(|w| w.lease_id == snapshot.lease_id))
}

/// Refresh the crew's branch on the record as it is now under the lock. The
/// caller's copy may predate the lock, so only a ready record with the same
/// lease is written. Callers hold the lock.
fn refresh_branch(store: &Store, lock: &Lock, snapshot: &Workspace) -> Result<Option<Workspace>> {
    let Some(mut w) = current(store, lock, snapshot)? else {
        return Ok(None);
    };
    if w.state == "ready" && sync_branch(lock, &mut w) {
        let _ = save(store, &w);
    }
    Ok(Some(w))
}

/// Record why a workspace is kept. Only a ready record is written, so a
/// release that has begun is never overwritten, and a reason is written and
/// audited only when it changes. Returns the record as it is now.
fn keep(
    store: &Store,
    lock: &Lock,
    snapshot: &Workspace,
    reason: &str,
) -> Result<Option<Workspace>> {
    let Some(mut w) = current(store, lock, snapshot)? else {
        return Ok(None);
    };
    if w.state != "ready" {
        return Ok(Some(w));
    }
    let synced = sync_branch(lock, &mut w);
    if w.retained_reason.as_deref() != Some(reason) {
        w.retained_reason = Some(reason.to_owned());
        save(store, &w)?;
        store.audit(
            "workspace.retained",
            &json!({"task_id": w.task_id, "path": w.path, "reason": reason}),
        )?;
    } else if synced {
        save(store, &w)?;
    }
    Ok(Some(w))
}

/// Raise one alert per task and reason. A changed reason replaces the alert
/// and is sent again, so the owner always sees the current cause.
fn alert_unreleasable(store: &Store, w: &Workspace, reason: &str) -> Result<()> {
    let db = store.connection();
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS workspace_alerts (task_id TEXT PRIMARY KEY, raised_ms INTEGER NOT NULL, reason TEXT NOT NULL);",
    )?;
    let raised: Option<String> = db
        .query_row(
            "SELECT reason FROM workspace_alerts WHERE task_id=?1",
            [&w.task_id],
            |r| r.get(0),
        )
        .optional()?;
    if raised.as_deref() == Some(reason) {
        return Ok(());
    }
    let now = Store::now_ms();
    db.execute(
        "INSERT INTO workspace_alerts (task_id, raised_ms, reason) VALUES (?1, ?2, ?3)
         ON CONFLICT(task_id) DO UPDATE SET raised_ms=excluded.raised_ms, reason=excluded.reason",
        params![w.task_id, now, reason],
    )?;
    store.audit(
        "workspace.alert",
        &json!({"task_id": w.task_id, "reason": reason, "path": w.path}),
    )?;
    let owner = match store.task(&w.task_id) {
        Ok(t) => t["owner"].as_str().unwrap_or("").to_owned(),
        Err(_) => String::new(),
    };
    if !owner.is_empty() {
        let tx = store.write_transaction()?;
        let notice = json!({
            "id": format!("workspace-alert-{}-{}", w.task_id, now),
            "from": "runtime",
            "to": owner,
            "kind": "notice",
            "task_id": w.task_id,
            "urgency": "normal",
            "body": format!("Workspace for task {} cannot be released: {}", w.task_id, reason),
            "payload": {
                "workspace_alert": w.task_id,
                "reason": reason,
                "path": w.path,
            }
        });
        crate::mail::record_crew(&tx, &notice)?;
        tx.commit()?;
    }
    Ok(())
}

/// How often the dispatcher looks for landed workspaces, and how long a
/// task's jobs must have been finished first.
const SWEEP_EVERY_MS: i64 = 10 * 60 * 1000;
const SWEEP_GRACE_MS: i64 = 10 * 60 * 1000;

/// Return workspaces whose work has landed, with no model call. A candidate
/// is ready, its jobs have been finished for the grace period, and either its
/// task is done or cancelled, or its branch has commits of its own (a fresh
/// or fast-forwarded checkout proves nothing). `release` makes the decision:
/// anything uncommitted, running or not landed is kept.
pub(crate) fn sweep(store: &Store) -> Result<Vec<Value>> {
    let now = Store::now_ms();
    let last: Option<i64> = store
        .connection()
        .query_row(
            "SELECT value FROM metadata WHERE key='workspace_sweep_ms'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .and_then(|v| v.parse().ok());
    if last.is_some_and(|at| now - at < SWEEP_EVERY_MS) {
        return Ok(vec![]);
    }
    store.connection().execute(
        "INSERT INTO metadata(key,value) VALUES('workspace_sweep_ms',?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [now.to_string()],
    )?;
    let records: Vec<String> = store
        .connection()
        .prepare("SELECT record FROM workspaces WHERE json_extract(record,'$.state')='ready'")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut released = vec![];
    for record in records {
        let snapshot: Workspace = serde_json::from_str(&record)?;
        let (Some(lease), Some(path)) = (snapshot.lease_id.clone(), snapshot.path.clone()) else {
            continue;
        };
        let lock = match Lock::acquire(store) {
            Ok(l) => l,
            Err(_) => continue,
        };
        // Every write below uses the record as it is under the lock.
        let Some(mut w) = current(store, &lock, &snapshot)?.filter(|w| w.state == "ready") else {
            continue;
        };
        if let Ok(all_slots) = slots(&lock, &w) {
            let on_lease: Vec<&Value> = all_slots
                .iter()
                .filter(|slot| slot["lease_id"].as_str() == Some(&lease))
                .collect();
            if on_lease.is_empty() {
                // Another task's live lease on the same checkout owns its
                // window now: closing it would close that task's window.
                let path_text = path.to_str().unwrap_or_default();
                let reused = all_slots.iter().any(|slot| {
                    slot["path"].as_str() == Some(path_text)
                        && slot["lease_id"]
                            .as_str()
                            .is_some_and(|other| !other.is_empty() && other != lease)
                });
                let closed = if reused {
                    Ok(())
                } else {
                    release_mcp_windows(store, &w)
                };
                if let Err(error) = closed {
                    let reason = format!("{error:#}");
                    if let Some(kept) = keep(store, &lock, &w, &reason)? {
                        alert_unreleasable(store, &kept, &reason)?;
                    }
                    continue;
                }
                w.state = "released".into();
                w.retained_reason = None;
                let _ = save(store, &w);
                let _ = store.audit(
                    "workspace.superseded",
                    &json!({"task_id": w.task_id, "path": path, "lease_id": lease, "reason": "lease moved to another task"}),
                );
                let _ = store.connection().execute(
                    "DELETE FROM workspace_alerts WHERE task_id=?1",
                    [&w.task_id],
                );
                continue;
            }
            let path_text = path.to_str().unwrap_or_default();
            if !on_lease.iter().any(|slot| {
                slot["lease_holder"] == w.holder && slot["path"].as_str() == Some(path_text)
            }) {
                let reason = "lease holder or path no longer matches its Treehouse slot";
                if let Some(kept) = keep(store, &lock, &w, reason)? {
                    alert_unreleasable(store, &kept, reason)?;
                }
                continue;
            }
        }
        sync_branch(&lock, &mut w);

        let (unfinished, last_finished): (i64, Option<i64>) = store.connection().query_row(
            "SELECT COUNT(*) FILTER (WHERE json_extract(record,'$.finished_ms') IS NULL),
                    MAX(json_extract(record,'$.finished_ms'))
             FROM jobs WHERE json_extract(record,'$.task_id')=?1",
            [&w.task_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if unfinished > 0 {
            keep(store, &lock, &w, "jobs still running")?;
            continue;
        }
        if last_finished.is_some_and(|at| now - at < SWEEP_GRACE_MS) {
            keep(store, &lock, &w, "jobs finished within grace period")?;
            continue;
        }
        let finished = matches!(
            store.task(&w.task_id)?["status"].as_str(),
            Some("done" | "cancelled")
        );
        if !finished {
            let own = (|| -> Result<u64> {
                let target = base_target(&lock, &w)?;
                Ok(git(
                    &lock,
                    &path,
                    &["rev-list", "--count", &format!("{target}..HEAD")],
                )?
                .parse()?)
            })();
            if !own.is_ok_and(|n| n > 0) {
                keep(store, &lock, &w, "active task without branch commits")?;
                continue;
            }
        }
        // Closed here too, so a failure reaches the owner whatever the task's
        // status; release closes again, finding nothing left.
        if let Err(error) = release_mcp_windows(store, &w) {
            let reason = format!("{error:#}");
            if let Some(kept) = keep(store, &lock, &w, &reason)? {
                alert_unreleasable(store, &kept, &reason)?;
            }
            continue;
        }
        drop(lock);

        match release(store, &json!({"task_id": w.task_id, "lease_id": lease})) {
            Ok(_) => {
                let current_branch = w.switched_branch.as_deref().unwrap_or(&w.branch);
                let entry = json!({"task_id": w.task_id, "path": path, "branch": current_branch});
                store.audit("workspace.auto_released", &entry)?;
                released.push(entry);
            }
            Err(err) => {
                let reason = format!("{err:#}");
                // release may have saved "releasing" before it failed.
                let Ok(lock) = Lock::acquire(store) else {
                    continue;
                };
                if let Some(kept) = keep(store, &lock, &w, &reason)? {
                    if finished {
                        alert_unreleasable(store, &kept, &reason)?;
                    }
                }
            }
        }
    }
    Ok(released)
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "workspace.acquire" => {
            let task = required(&args, "task_id")?;
            let charter = store.authorize(task, "spawn")?;
            let repo = fs::canonicalize(required(&charter, "repo")?)?;
            let selected = args
                .get("cwd")
                .map(|v| v.as_str().context("cwd must be an absolute path"))
                .transpose()?
                .unwrap_or(repo.to_str().context("repository path is not UTF-8")?);
            ensure!(Path::new(selected).is_absolute(), "cwd must be absolute");
            let selected = fs::canonicalize(selected)?;
            let wait_for_capacity = args
                .get("wait_for_capacity")
                .map(|v| v.as_bool().context("wait_for_capacity must be boolean"))
                .transpose()?
                .unwrap_or(false);
            let max_wait_secs = match args.get("max_wait_secs") {
                Some(value) => value
                    .as_u64()
                    .filter(|seconds| (1..=86_400).contains(seconds))
                    .context("max_wait_secs must be 1..86400")?,
                None => 3600,
            };
            ensure!(
                wait_for_capacity || args.get("max_wait_secs").is_none(),
                "max_wait_secs requires wait_for_capacity"
            );
            let exec = execution(
                store,
                task,
                &repo,
                &selected,
                true,
                None,
                WorkspaceWait {
                    wait_for_capacity,
                    max_wait_secs,
                },
            )?;
            ensure!(
                exec.lease_id.is_some(),
                "workspace.acquire requires a Git repository"
            );
            public(store, &load(store, task)?.context("workspace missing")?)
        }
        "workspace.recover" => {
            let task = required(&args, "task_id")?;
            let lock = Lock::acquire(store)?;
            owner(store, task)?;
            let mut w = load(store, task)?.context("task has no workspace")?;
            ensure!(
                w.generation == required(&args, "generation")?,
                "stale workspace recovery generation"
            );
            if w.state == "acquiring" {
                let held: Vec<Value> = slots(&lock, &w)?
                    .into_iter()
                    .filter(|s| s["lease_holder"] == w.holder)
                    .collect();
                ensure!(held.len() <= 1, "multiple leases claim this workspace");
                if let Some(lease) = held.first() {
                    w.path = Some(PathBuf::from(required(lease, "path")?));
                    w.lease_id = Some(required(lease, "lease_id")?.to_owned());
                    validate_path(&lock, &w)?;
                    w.state = "ready".into();
                    validate(&lock, &w)?;
                } else {
                    w.state = "released".into();
                }
                save(store, &w)?;
            } else if w.state == "releasing" {
                crate::jobs::workspace_idle(
                    store,
                    task,
                    w.lease_id.as_deref().context("workspace lease missing")?,
                )?;
                if !slots(&lock, &w)?
                    .iter()
                    .any(|s| s["lease_id"].as_str() == w.lease_id.as_deref())
                {
                    w.state = "released".into();
                    save(store, &w)?;
                }
            }
            public(store, &w)
        }
        "workspace.status" => {
            let task = required(&args, "task_id")?;
            owner(store, task)?;
            let Some(mut w) = load(store, task)? else {
                return Ok(Value::Null);
            };
            if w.state == "ready" && w.path.as_ref().is_some_and(|p| p.is_dir()) {
                if let Ok(lock) = Lock::acquire(store) {
                    if let Some(fresh) = refresh_branch(store, &lock, &w)? {
                        w = fresh;
                    }
                }
            }
            public(store, &w)
        }
        "workspace.list" => {
            let limit = args
                .get("limit")
                .map(|v| v.as_u64().context("limit must be positive"))
                .transpose()?
                .unwrap_or(100)
                .clamp(1, 1000);
            let after = args
                .get("after_id")
                .map(|v| v.as_str().context("after_id must be a task id"))
                .transpose()?
                .unwrap_or("");
            let actor = match store.caller() {
                Caller::Agent { actor } => Some(actor.as_str()),
                Caller::Operator => None,
            };
            let active_only = args.get("active") == Some(&json!(true));
            let query = if active_only {
                "SELECT w.record FROM workspaces w JOIN tasks t ON t.id=w.task_id WHERE w.task_id>?1 AND (?2 IS NULL OR t.owner=?2) AND json_extract(w.record,'$.state') != 'released' ORDER BY w.task_id LIMIT ?3"
            } else {
                "SELECT w.record FROM workspaces w JOIN tasks t ON t.id=w.task_id WHERE w.task_id>?1 AND (?2 IS NULL OR t.owner=?2) ORDER BY w.task_id LIMIT ?3"
            };
            let mut statement = store.connection().prepare(query)?;
            let raw =
                statement.query_map(params![after, actor, limit], |r| r.get::<_, String>(0))?;
            raw.map(|r| public(store, &serde_json::from_str::<Workspace>(&r?)?))
                .collect::<Result<Vec<_>>>()
                .map(|v| json!(v))
        }
        "workspace.release" => release(store, &args),
        _ => bail!("unknown workspace command {command}"),
    }
}

#[cfg(test)]
mod stale_write_tests {
    use super::*;

    fn record(lease: &str) -> Workspace {
        Workspace {
            task_id: "one".into(),
            generation: "gen1".into(),
            charter_repo: PathBuf::from("/unused/charter"),
            repo: PathBuf::from("/unused/repo"),
            git_repo: PathBuf::from("/unused/repo"),
            pool: PathBuf::from("/unused/pool"),
            binary: PathBuf::from("/unused/treehouse"),
            holder: "privateer:ship:gen1".into(),
            branch: "privateer/one".into(),
            base_branch: "main".into(),
            state: "ready".into(),
            path: Some(PathBuf::from("/unused/pool/slot")),
            lease_id: Some(lease.into()),
            archive_ref: None,
            task_archive_ref: None,
            switched_branch: None,
            current_branch: None,
            retained_reason: None,
        }
    }

    #[test]
    fn a_stale_sweep_copy_never_overwrites_a_reacquired_or_releasing_record() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&temp.path().join("home")).unwrap();
        crate::dispatch(&mut store, "charter.put", json!({"id":"mission","objective":"stale write fixture","repo":temp.path(),"min_free_bytes":0})).unwrap();
        crate::dispatch(&mut store, "ship.put", json!({"name":"ship"})).unwrap();
        crate::dispatch(
            &mut store,
            "task.create",
            json!({"id":"one","charter_id":"mission","owner":"ship","title":"one"}),
        )
        .unwrap();

        // The sweep read lease-one, then the task was released and reacquired.
        save(&store, &record("lease-two")).unwrap();
        let stale = record("lease-one");
        let lock = Lock::acquire(&store).unwrap();
        assert!(current(&store, &lock, &stale).unwrap().is_none());
        assert!(
            keep(&store, &lock, &stale, "stale reason")
                .unwrap()
                .is_none()
        );
        let after = load(&store, "one").unwrap().unwrap();
        assert_eq!(after.lease_id.as_deref(), Some("lease-two"));
        assert_eq!(after.state, "ready");
        assert_eq!(after.retained_reason, None);

        // A release that has saved "releasing" is not written back to ready.
        let mut releasing = record("lease-two");
        releasing.state = "releasing".into();
        save(&store, &releasing).unwrap();
        let fresh = record("lease-two");
        assert!(
            keep(&store, &lock, &fresh, "jobs still running")
                .unwrap()
                .is_some()
        );
        drop(lock);
        let after = load(&store, "one").unwrap().unwrap();
        assert_eq!(after.state, "releasing");
        assert_eq!(after.retained_reason, None);
    }

    #[test]
    fn a_stale_status_copy_never_saves_over_a_newer_record() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&temp.path().join("home")).unwrap();
        crate::dispatch(&mut store, "charter.put", json!({"id":"mission","objective":"stale write fixture","repo":temp.path(),"min_free_bytes":0})).unwrap();
        crate::dispatch(&mut store, "ship.put", json!({"name":"ship"})).unwrap();
        crate::dispatch(
            &mut store,
            "task.create",
            json!({"id":"one","charter_id":"mission","owner":"ship","title":"one"}),
        )
        .unwrap();

        // A real slot whose HEAD has switched away from the task branch.
        let slot = temp.path().join("slot");
        fs::create_dir(&slot).unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["symbolic-ref", "HEAD", "refs/heads/fix/elsewhere"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(&args)
                    .current_dir(&slot)
                    .status()
                    .unwrap()
                    .success()
            );
        }

        // The copy was read as ready, then a release began under the lock.
        let mut releasing = record("lease-two");
        releasing.path = Some(slot.clone());
        releasing.state = "releasing".into();
        save(&store, &releasing).unwrap();
        let mut stale = record("lease-two");
        stale.path = Some(slot.clone());
        let lock = Lock::acquire(&store).unwrap();
        let fresh = refresh_branch(&store, &lock, &stale).unwrap().unwrap();
        drop(lock);
        assert_eq!(fresh.state, "releasing");
        assert_eq!(fresh.switched_branch, None);
        let after = load(&store, "one").unwrap().unwrap();
        assert_eq!(after.state, "releasing");
        assert_eq!(after.switched_branch, None);

        // A ready record with the same lease takes the branch change.
        let mut ready = record("lease-two");
        ready.path = Some(slot.clone());
        save(&store, &ready).unwrap();
        let lock = Lock::acquire(&store).unwrap();
        let fresh = refresh_branch(&store, &lock, &stale).unwrap().unwrap();
        drop(lock);
        assert_eq!(fresh.switched_branch.as_deref(), Some("fix/elsewhere"));
        let after = load(&store, "one").unwrap().unwrap();
        assert_eq!(after.switched_branch.as_deref(), Some("fix/elsewhere"));
    }
}
