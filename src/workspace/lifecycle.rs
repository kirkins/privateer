//! Explicit release and recovery keep unfinished work and stale leases intact.
use super::*;

fn release(store: &Store, args: &Value) -> Result<Value> {
    let task = required(args, "task_id")?;
    owner(store, task)?;
    if matches!(store.caller(), Caller::Agent { .. }) {
        store.authorize_running(task, "cleanup")?;
    }
    let lock = Lock::acquire(store)?;
    owner(store, task)?;
    if matches!(store.caller(), Caller::Agent { .. }) {
        store.authorize_running(task, "cleanup")?;
    }
    let mut w = load(store, task)?.context("task has no workspace")?;
    ensure!(
        w.lease_id.as_deref() == Some(required(args, "lease_id")?),
        "stale workspace release; lease_id does not match"
    );
    if w.state == "released" {
        return public(store, &w);
    }
    crate::jobs::workspace_idle(store, task, w.lease_id.as_deref().unwrap())?;
    if w.state == "releasing"
        && !slots(&lock, &w)?
            .iter()
            .any(|s| s["lease_id"].as_str() == w.lease_id.as_deref())
    {
        w.state = "released".into();
        save(store, &w)?;
        return public(store, &w);
    }
    ensure!(
        matches!(w.state.as_str(), "ready" | "releasing"),
        "finish workspace.acquire recovery before releasing"
    );
    w.state = "ready".into();
    validate(&lock, &w)?;
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
    if archive {
        let reference = format!(
            "refs/privateer/workspaces/{}/{}",
            hash(&store.home().to_string_lossy()),
            w.generation
        );
        git(&lock, &w.git_repo, &["update-ref", &reference, &head])?;
        w.archive_ref = Some(reference);
    } else {
        let remote = format!("refs/remotes/origin/{}", w.base_branch);
        let local = format!("refs/heads/{}", w.base_branch);
        let target = if git(&lock, &w.git_repo, &["remote", "get-url", "origin"]).is_ok() {
            git(&lock, &w.git_repo, &["fetch", "--quiet", "origin"])?;
            remote
        } else {
            local
        };
        let merged = git(
            &lock,
            &w.git_repo,
            &["merge-tree", "--write-tree", &target, &head],
        )
        .context(
            "workspace changes are not proven landed; use archive:true to preserve committed work",
        )?;
        ensure!(
            merged.lines().next()
                == Some(
                    git(
                        &lock,
                        &w.git_repo,
                        &["rev-parse", &format!("{target}^{{tree}}")]
                    )?
                    .as_str()
                ),
            "workspace changes are not landed; use archive:true to preserve committed work"
        );
    }
    w.state = "releasing".into();
    owner(store, task)?;
    if matches!(store.caller(), Caller::Agent { .. }) {
        store.authorize_running(task, "cleanup")?;
    }
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
    save(store, &w)?;
    public(store, &w)
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "workspace.acquire" => {
            let task = required(&args, "task_id")?;
            let charter = store.authorize(task, "spawn")?;
            let repo = fs::canonicalize(required(&charter, "repo")?)?;
            let exec = execution(store, task, &repo, true, None)?;
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
            match load(store, task)? {
                Some(w) => public(store, &w),
                None => Ok(Value::Null),
            }
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
            let mut statement = store.connection().prepare("SELECT w.record FROM workspaces w JOIN tasks t ON t.id=w.task_id WHERE w.task_id>?1 AND (?2 IS NULL OR t.owner=?2) ORDER BY w.task_id LIMIT ?3")?;
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
