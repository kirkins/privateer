//! The author set of a review: which models wrote the code a PR carries.
//!
//! Routes are compared by model (`routes::model_key`), not by route, so the
//! same model behind two harnesses still overlaps. The runtime finds the task
//! from the PR's head branch itself, never from the ship's word: PR heads are
//! workspace branches, `privateer/<home_hash>/<hash(task)>/<generation>`. The
//! scope is that task's whole tree, its top ancestor and every descendant, so
//! a commit cannot hide its author in a child or sibling task. A branch that
//! is not one of ours falls back to every agent job the ship ran since the
//! merge-base commit was made; the merge-base is on the protected branch, so
//! its time is trustworthy.
//!
//! Each job in scope lands in one of two classes. Crew jobs, and any author
//! routes the ship declares (which can only add), are `crew_models`: a
//! reviewer whose model is one of them is excluded outright, with no
//! exception. The ship's first-mate profile, every route on its
//! `first_mate` list, its wakes and its ordinary errands are
//! `first_mate_models`: a reviewer whose model is only there is allowed,
//! with the overlap recorded. A crew job whose model cannot be named (no
//! route at all, or a bare alias) leaves the set unknown: the review
//! refuses, naming the job, until the ship declares that job's route.
use crate::store::Store;
use anyhow::{Result, bail};
use rusqlite::OptionalExtension;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Which models wrote the code a PR carries, in the two classes the overlap
/// rule compares. Job ids are kept beside their models so a refusal can name
/// the jobs that exclude a reviewer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Authors {
    /// Each model a crew job in scope ran on, with the jobs that ran on it.
    /// Declared author routes land here too, naming the declared route.
    pub crew_jobs: BTreeMap<String, Vec<String>>,
    /// The models the ship's first mate may have written the code on: its
    /// profile, its `first_mate` list, its wakes and its ordinary errands.
    pub first_mate_models: BTreeSet<String>,
    /// Crew jobs in scope whose model cannot be named. A review refuses
    /// until the ship declares their routes.
    pub unknown_jobs: Vec<String>,
}

/// The author set of a PR. `branch` is its head branch as the forge reports
/// it; `merge_base_ms` is the time of its merge-base commit on the protected
/// branch, the fallback scope when the branch is not one of ours. `declared`
/// holds author routes the ship declares (`harness/provider/model`), which
/// can only add, and stand for the jobs whose routes cannot be read.
pub fn compute(
    store: &Store,
    ship: &str,
    branch: &str,
    merge_base_ms: i64,
    declared: &[String],
) -> Result<Authors> {
    let mut authors = Authors::default();
    for route in declared {
        let Some(model) = crate::routes::model_key(route) else {
            bail!(
                "declared author route {route} names no known model; pass its full \
                 harness/provider/model"
            );
        };
        authors
            .crew_jobs
            .entry(model)
            .or_default()
            .push(format!("declared author route {route}"));
    }
    let jobs = match task_of_branch(store, branch)? {
        Some(task) => jobs_of_tree(store, &task)?,
        None => jobs_since(store, merge_base_ms)?,
    };
    for job in jobs {
        let Some(id) = job["id"].as_str().map(str::to_owned) else {
            continue;
        };
        let route = job["model_route"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| crate::routes::route_of_argv(&job["command"]));
        let first_mate = is_errand(store, &id)? || is_wake(store, &id)?;
        match (
            route.as_deref().and_then(crate::routes::model_key),
            first_mate,
        ) {
            // A first mate's own unreadable route adds nothing: the overlap
            // it would record is allowed either way.
            (None, true) => {}
            (Some(model), true) => {
                authors.first_mate_models.insert(model);
            }
            (Some(model), false) => {
                authors.crew_jobs.entry(model).or_default().push(id);
            }
            (None, false) => authors.unknown_jobs.push(id),
        }
    }
    if !declared.is_empty() {
        // The ship's declaration stands for the jobs whose routes it cannot
        // read, so they no longer refuse the review.
        authors.unknown_jobs.clear();
    }
    // The first mate may commit attended, leaving no job at all: its profile
    // route and every route on its first_mate list always count. A home with
    // no ship record or routes file names none of them.
    if let Some(record) = crate::mail::ship(store, ship).ok().flatten() {
        let profile = &record["first_mate"];
        if let Some(route) = profile["manifest"]
            .as_str()
            .and_then(|m| crate::routes::route_of(m, &profile["options"]))
            && let Some(model) = crate::routes::model_key(&route)
        {
            authors.first_mate_models.insert(model);
        }
    }
    if let Some(policy) = crate::crew_routes::for_store(store)? {
        for name in crate::crew_routes::order(&policy, ship, "first_mate") {
            if let Some(model) = crate::crew_routes::identity(&policy["routes"][name.as_str()])
                .as_deref()
                .and_then(crate::routes::model_key)
            {
                authors.first_mate_models.insert(model);
            }
        }
    }
    Ok(authors)
}

/// The task a PR head branch was cut from, when it is one of this home's
/// workspace branches: `privateer/<home_hash>/<hash(task)>/<generation>`.
/// The ship's word for the task is never taken.
pub fn task_of_branch(store: &Store, branch: &str) -> Result<Option<String>> {
    let parts: Vec<&str> = branch.split('/').collect();
    if parts.len() != 4
        || parts[0] != "privateer"
        || parts[1] != crate::workspace::hash(&store.home().to_string_lossy())
    {
        return Ok(None);
    }
    let mut statement = store.connection().prepare("SELECT id FROM tasks")?;
    let tasks = statement.query_map([], |r| r.get::<_, String>(0))?;
    Ok(tasks
        .filter_map(|t| t.ok())
        .find(|id| crate::workspace::hash(id) == parts[2]))
}

/// A task's whole tree: its top ancestor and every descendant of that
/// ancestor. A corrupt parent cycle stops at a task already seen.
fn task_tree(store: &Store, task: &str) -> Result<BTreeSet<String>> {
    let mut root = task.to_owned();
    let mut seen = BTreeSet::from([root.clone()]);
    loop {
        let parent = store
            .connection()
            .query_row("SELECT parent FROM tasks WHERE id=?1", [&root], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()?
            .flatten();
        match parent {
            Some(parent) if seen.insert(parent.clone()) => root = parent,
            _ => break,
        }
    }
    let mut statement = store.connection().prepare(
        "WITH RECURSIVE tree(id) AS (
           SELECT id FROM tasks WHERE id=?1
           UNION
           SELECT t.id FROM tasks t JOIN tree ON t.parent = tree.id
         ) SELECT id FROM tree",
    )?;
    let tasks = statement.query_map([&root], |r| r.get::<_, String>(0))?;
    Ok(tasks.filter_map(|t| t.ok()).collect())
}

/// Every agent job (harness launch) the ship ran on a task tree, oldest
/// first. A record too corrupt to parse cannot be classified, so it is
/// passed over rather than guessed at.
fn jobs_of_tree(store: &Store, task: &str) -> Result<Vec<Value>> {
    let tree: Vec<String> = task_tree(store, task)?.into_iter().collect();
    let slots = (1..=tree.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut statement = store.connection().prepare(&format!(
        "SELECT record FROM jobs WHERE json_extract(record,'$.action')='spawn' \
         AND json_extract(record,'$.task_id') IN ({slots}) \
         ORDER BY json_extract(record,'$.created_ms')"
    ))?;
    let jobs = statement.query_map(rusqlite::params_from_iter(tree), |r| r.get::<_, String>(0))?;
    Ok(jobs
        .filter_map(|j| j.ok())
        .filter_map(|j| serde_json::from_str(&j).ok())
        .collect())
}

/// Every agent job the ship ran at or after a time, oldest first: the
/// fallback scope when the head branch is not one of ours.
fn jobs_since(store: &Store, since_ms: i64) -> Result<Vec<Value>> {
    let mut statement = store.connection().prepare(
        "SELECT record FROM jobs WHERE json_extract(record,'$.action')='spawn' \
         AND json_extract(record,'$.created_ms') >= ?1 \
         ORDER BY json_extract(record,'$.created_ms')",
    )?;
    let jobs = statement.query_map([since_ms], |r| r.get::<_, String>(0))?;
    Ok(jobs
        .filter_map(|j| j.ok())
        .filter_map(|j| serde_json::from_str(&j).ok())
        .collect())
}

/// Whether a job is one of the ship's errands: errands are the first mate's
/// thinking, so their routes land in the first-mate class.
fn is_errand(store: &Store, job: &str) -> Result<bool> {
    crate::errand::is_errand(store, job)
}

/// Whether a job is one of the ship's wakes: `serve` records the wake, with
/// its job id, before starting it.
fn is_wake(store: &Store, job: &str) -> Result<bool> {
    Ok(store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM wakes WHERE job_id=?1)",
        [job],
        |r| r.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn home() -> (tempfile::TempDir, Store) {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("home")).unwrap();
        (temp, store)
    }

    fn task(store: &Store, id: &str, parent: Option<&str>) {
        store
            .connection()
            .execute(
                "INSERT OR IGNORE INTO charters(id,data) VALUES('charter','{}')",
                [],
            )
            .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO tasks(id,charter_id,owner,title,status,parent,depth) \
                 VALUES(?1,'charter','ship',?1,'open',?2,0)",
                rusqlite::params![id, parent],
            )
            .unwrap();
    }

    fn job(
        store: &Store,
        id: &str,
        task_id: &str,
        created_ms: i64,
        route: Option<&str>,
        command: Value,
    ) {
        store
            .connection()
            .execute(
                "INSERT INTO jobs(id,record) VALUES(?1,?2)",
                rusqlite::params![
                    id,
                    json!({"id": id, "task_id": task_id, "action": "spawn",
                        "created_ms": created_ms, "model_route": route, "command": command})
                    .to_string()
                ],
            )
            .unwrap();
    }

    fn errand(store: &Store, id: &str, task_id: &str, route: &str) {
        store
            .connection()
            .execute(
                "INSERT INTO errands(id, ship, focus, task_id, brief, mode, status, created_ms)
                 VALUES(?1,'ship',?2,?3,'brief','fresh','finished',1)",
                rusqlite::params![id, format!("task:{task_id}"), task_id],
            )
            .unwrap();
        job(store, id, task_id, 950, Some(route), json!([]));
    }

    fn wake(store: &Store, id: &str, task_id: &str, route: &str) {
        store
            .connection()
            .execute(
                "INSERT INTO wakes(id, ship, focus, through_seq, job_id, mode, status, created_ms)
                 VALUES(?1,'ship',?2,1,?1,'fresh','finished',1)",
                rusqlite::params![id, format!("task:{task_id}")],
            )
            .unwrap();
        job(store, id, task_id, 950, Some(route), json!([]));
    }

    #[test]
    fn a_pr_head_branch_resolves_to_its_task_tree() {
        let (_temp, store) = home();
        task(&store, "fix", None);
        task(&store, "child", Some("fix"));
        task(&store, "grandchild", Some("child"));
        task(&store, "other", None);
        let branch = |task: &str| {
            format!(
                "privateer/{}/{}/gen",
                crate::workspace::hash(&store.home().to_string_lossy()),
                crate::workspace::hash(task)
            )
        };
        // The middle part names the task the branch was cut from.
        assert_eq!(
            task_of_branch(&store, &branch("child")).unwrap().as_deref(),
            Some("child")
        );
        assert_eq!(
            task_of_branch(&store, &branch("fix")).unwrap().as_deref(),
            Some("fix")
        );
        // Not a workspace branch, another home's branch, or a task this home
        // does not have: the merge-base fallback applies.
        assert_eq!(task_of_branch(&store, "feature/thing").unwrap(), None);
        assert_eq!(
            task_of_branch(&store, "privateer/0011223344556677/feed/other").unwrap(),
            None
        );
        assert_eq!(
            task_of_branch(&store, &branch("no-such-task")).unwrap(),
            None
        );

        // The scope is the whole tree: the top ancestor, descendants and
        // siblings under it, never a task outside it.
        job(
            &store,
            "crew-child",
            "child",
            1000,
            Some("claude/-/claude-opus-5-5"),
            json!([]),
        );
        job(
            &store,
            "crew-grand",
            "grandchild",
            2000,
            Some("pi/chatgpt-home/gpt-6.1-sol"),
            json!([]),
        );
        job(
            &store,
            "crew-sibling",
            "fix",
            1500,
            None,
            json!([
                "python3",
                "/p/native_launch.py",
                "codex",
                "/bin/codex",
                "--model",
                "gpt-6-luna"
            ]),
        );
        job(
            &store,
            "crew-other",
            "other",
            3000,
            Some("codex/-/gpt-6-luna"),
            json!([]),
        );
        let authors = compute(&store, "ship", &branch("child"), 0, &[]).unwrap();
        assert_eq!(
            authors.crew_jobs,
            BTreeMap::from([
                ("claude-opus-5-5".to_owned(), vec!["crew-child".to_owned()]),
                // A job with no recorded route is read back from its argv.
                ("gpt-6-luna".to_owned(), vec!["crew-sibling".to_owned()]),
                ("gpt-6.1-sol".to_owned(), vec!["crew-grand".to_owned()]),
            ])
        );
        assert!(authors.unknown_jobs.is_empty());
    }

    #[test]
    fn jobs_land_in_their_two_classes_and_unknown_crew_jobs_need_a_declaration() {
        let (_temp, store) = home();
        task(&store, "fix", None);
        job(
            &store,
            "crew-a",
            "fix",
            1000,
            Some("claude/-/claude-opus-5-5"),
            json!([]),
        );
        // An errand and a wake are the first mate's, whatever their model.
        errand(
            &store,
            "errand-1",
            "fix",
            "pi/opencode-go/deepseek-v4.1-flash",
        );
        wake(&store, "wake-1", "fix", "pi/chatgpt-home/gpt-6.1-sol");
        // A crew job whose model cannot be named (a bare alias), and one from
        // before the merge base, which the fallback scope leaves out.
        job(
            &store,
            "crew-alias",
            "fix",
            1200,
            Some("claude/-/opus"),
            json!([]),
        );
        job(
            &store,
            "crew-old",
            "fix",
            500,
            Some("codex/-/gpt-6-luna"),
            json!([]),
        );
        let authors = compute(&store, "ship", "feature/not-ours", 900, &[]).unwrap();
        assert_eq!(
            authors.crew_jobs,
            BTreeMap::from([("claude-opus-5-5".to_owned(), vec!["crew-a".to_owned()])])
        );
        assert_eq!(
            authors.first_mate_models,
            BTreeSet::from(["deepseek-v4.1-flash".to_owned(), "gpt-6.1-sol".to_owned()])
        );
        assert_eq!(authors.unknown_jobs, vec!["crew-alias".to_owned()]);

        // A declaration stands for the jobs whose routes cannot be read, and
        // can only add crew models.
        let declared = compute(
            &store,
            "ship",
            "feature/not-ours",
            900,
            &["codex/-/gpt-6-luna".to_owned()],
        )
        .unwrap();
        assert!(declared.unknown_jobs.is_empty());
        assert_eq!(
            declared.crew_jobs["gpt-6-luna"],
            vec!["declared author route codex/-/gpt-6-luna".to_owned()]
        );
        // A declaration that names no known model is refused.
        assert!(
            compute(
                &store,
                "ship",
                "feature/not-ours",
                900,
                &["claude/-/opus".to_owned()]
            )
            .is_err()
        );
    }
}
