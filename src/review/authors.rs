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
//! route at all, or a bare alias) leaves the set unknown: the review refuses,
//! naming the job, until the ship declares that job's route. A declaration is
//! keyed to one job: it stands only for the job it names, must fit it (its
//! harness the job's, its model one the job's alias names), and clears no job
//! it does not name. That fit proves only that the declared model carries the
//! job's alias, never which version the alias resolved to, so the alias is
//! kept in the set beside the declared model: a reviewer whose model carries
//! it is excluded too, whatever its version. The gate only ever over-excludes.
use crate::store::Store;
use anyhow::{Result, bail, ensure};
use rusqlite::OptionalExtension;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Which models wrote the code a PR carries, in the two classes the overlap
/// rule compares. Job ids are kept beside their models so a refusal can name
/// the jobs that exclude a reviewer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Authors {
    /// Each model a crew job in scope ran on, with the jobs that ran on it.
    /// A declaration lands here too, naming the job it stands for.
    pub crew_jobs: BTreeMap<String, Vec<String>>,
    /// Each bare alias a declared job ran on, with the jobs that ran on it.
    /// A declaration's fit proves its model carries the job's alias, not
    /// which version the alias resolved to, so the alias is kept: a reviewer
    /// whose model carries it is excluded, whatever its version.
    pub crew_aliases: BTreeMap<String, Vec<String>>,
    /// The models the ship's first mate may have written the code on: its
    /// profile, its `first_mate` list, its wakes and its ordinary errands.
    pub first_mate_models: BTreeSet<String>,
    /// Crew jobs in scope whose model cannot be named. A review refuses
    /// until the ship declares their routes.
    pub unknown_jobs: Vec<String>,
}

/// An author route the ship declares for one crew job whose own route cannot
/// be read: the job it stands for, by id, and the route that job ran on. One
/// declaration stands for its job alone, so it can never clear another job
/// it does not name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    /// The crew job the declaration stands for.
    pub job_id: String,
    /// The full route that job ran on: `harness/provider/model`.
    pub route: String,
}

/// The author set of a PR. `branch` is its head branch as the forge reports
/// it; `merge_base_ms` is the time of its merge-base commit on the protected
/// branch, the fallback scope when the branch is not one of ours. `declared`
/// holds the author routes the ship declares for single jobs whose own routes
/// cannot be read: each must fit the job it names, and can only add.
pub fn compute(
    store: &Store,
    ship: &str,
    branch: &str,
    merge_base_ms: i64,
    declared: &[Declaration],
) -> Result<Authors> {
    let mut authors = Authors::default();
    let jobs = match task_of_branch(store, branch)? {
        Some(task) => jobs_of_tree(store, &task)?,
        None => jobs_since(store, merge_base_ms)?,
    };
    // The crew jobs whose model cannot be named, each with the route it ran
    // on (None when no route could be read at all), so each declaration can
    // be checked against the one job it names.
    let mut unknown: BTreeMap<String, Option<String>> = BTreeMap::new();
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
            (None, false) => {
                unknown.insert(id, route);
            }
        }
    }
    // One declaration per job: a second for the same job is refused outright,
    // naming the job, so no ship can hold two versions of one job's route.
    let mut declared_ids = BTreeSet::new();
    for declaration in declared {
        ensure!(
            declared_ids.insert(declaration.job_id.clone()),
            "job {} is declared twice in author_routes: one declaration stands \
             for one job",
            declaration.job_id
        );
        let Some(model) = crate::routes::model_key(&declaration.route) else {
            bail!(
                "declared author route {} for job {} names no known model; pass its \
                 full harness/provider/model",
                declaration.route,
                declaration.job_id
            );
        };
        // One declaration stands for the job it names and nothing else, so it
        // must name a crew job in scope whose model cannot be named: a
        // declaration for any other job never clears this one.
        let Some(job_route) = unknown.remove(&declaration.job_id) else {
            bail!(
                "declared author route {} does not fit job {}: no crew job in scope \
                 with that id has a model that cannot be named",
                declaration.route,
                declaration.job_id
            );
        };
        if let Some(job_route) = job_route {
            // The declaration must read as the job it names: its harness the
            // job's, and its model one the job's bare alias can stand for
            // (alias opus inside claude-opus-5-5), so a route for another
            // model, whatever its harness, never fits.
            let job_harness = job_route.split('/').next().unwrap_or_default();
            ensure!(
                declaration.route.split('/').next().unwrap_or_default() == job_harness,
                "declared author route {} does not fit job {}: the job ran on the \
                 {job_harness} harness",
                declaration.route,
                declaration.job_id
            );
            let alias = crate::routes::variant_free(
                job_route
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .split('#')
                    .next()
                    .unwrap_or_default(),
            )
            .to_ascii_lowercase();
            if !alias.is_empty() && alias != "-" {
                ensure!(
                    model.contains(&alias),
                    "declared author route {} does not fit job {}: its model {model} does \
                     not carry the job's alias {alias}",
                    declaration.route,
                    declaration.job_id
                );
                // The fit above proves only that the declared model carries
                // the job's alias, never which version the alias resolved to
                // when the job ran: a job on the bare alias opus declared as
                // claude/-/claude-opus-4-7 still fits. The alias is kept
                // beside the declared model, so the review gate can
                // over-exclude every model that carries it.
                authors
                    .crew_aliases
                    .entry(alias)
                    .or_default()
                    .push(declaration.job_id.clone());
            }
        }
        // The declaration can only add: the model lands in the crew class
        // with the job it stands for named beside it.
        authors
            .crew_jobs
            .entry(model)
            .or_default()
            .push(declaration.job_id.clone());
    }
    authors.unknown_jobs = unknown.into_keys().collect();
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
/// fallback scope when the head branch is not one of ours. A job started
/// earlier that was still running then counts too (a job still running has
/// no finish recorded). That only rescues crews still running when the new
/// merge-base commit was made: a rebase after a crew finished still moves
/// the merge-base past it and drops it. How branches that are not ours
/// should count earlier crews is a captain decision.
fn jobs_since(store: &Store, since_ms: i64) -> Result<Vec<Value>> {
    let mut statement = store.connection().prepare(
        "SELECT record FROM jobs WHERE json_extract(record,'$.action')='spawn' \
         AND (json_extract(record,'$.created_ms') >= ?1 \
              OR json_extract(record,'$.finished_ms') >= ?1 \
              OR json_extract(record,'$.finished_ms') IS NULL) \
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
        finished_ms: Option<i64>,
    ) {
        store
            .connection()
            .execute(
                "INSERT INTO jobs(id,record) VALUES(?1,?2)",
                rusqlite::params![
                    id,
                    json!({"id": id, "task_id": task_id, "action": "spawn",
                        "created_ms": created_ms, "finished_ms": finished_ms,
                        "model_route": route, "command": command})
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
        job(store, id, task_id, 950, Some(route), json!([]), None);
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
        job(store, id, task_id, 950, Some(route), json!([]), None);
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
            None,
        );
        job(
            &store,
            "crew-grand",
            "grandchild",
            2000,
            Some("pi/chatgpt-home/gpt-6.1-sol"),
            json!([]),
            None,
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
            None,
        );
        job(
            &store,
            "crew-other",
            "other",
            3000,
            Some("codex/-/gpt-6-luna"),
            json!([]),
            None,
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
    fn jobs_land_in_their_two_classes_and_declarations_fit_their_jobs() {
        let (_temp, store) = home();
        task(&store, "fix", None);
        job(
            &store,
            "crew-a",
            "fix",
            1000,
            Some("claude/-/claude-opus-5-5"),
            json!([]),
            Some(1400),
        );
        // An errand and a wake are the first mate's, whatever their model.
        errand(
            &store,
            "errand-1",
            "fix",
            "pi/opencode-go/deepseek-v4.1-flash",
        );
        wake(&store, "wake-1", "fix", "pi/chatgpt-home/gpt-6.1-sol");
        // A crew job whose model cannot be named (a bare alias), and, for the
        // merge-base fallback, one that finished before the merge base (left
        // out), one that started before it but finished after it, and one
        // still running (no finish recorded): the last two were running at or
        // after the merge-base time, so a rebase that moves it forward cannot
        // drop them.
        job(
            &store,
            "crew-alias",
            "fix",
            1200,
            Some("claude/-/opus"),
            json!([]),
            Some(1500),
        );
        job(
            &store,
            "crew-old",
            "fix",
            500,
            Some("codex/-/gpt-6-luna"),
            json!([]),
            Some(600),
        );
        job(
            &store,
            "crew-spanning",
            "fix",
            700,
            Some("codex/-/gpt-6-coda"),
            json!([]),
            Some(1000),
        );
        job(
            &store,
            "crew-running",
            "fix",
            800,
            Some("pi/opencode-go/deepseek-v4.1-flash-free"),
            json!([]),
            None,
        );
        let authors = compute(&store, "ship", "feature/not-ours", 900, &[]).unwrap();
        assert_eq!(
            authors.crew_jobs,
            BTreeMap::from([
                ("claude-opus-5-5".to_owned(), vec!["crew-a".to_owned()]),
                // The free variant of a route is still its model.
                (
                    "deepseek-v4.1-flash".to_owned(),
                    vec!["crew-running".to_owned()]
                ),
                ("gpt-6-coda".to_owned(), vec!["crew-spanning".to_owned()]),
            ])
        );
        assert_eq!(
            authors.first_mate_models,
            BTreeSet::from(["deepseek-v4.1-flash".to_owned(), "gpt-6.1-sol".to_owned()])
        );
        assert_eq!(authors.unknown_jobs, vec!["crew-alias".to_owned()]);

        // A declaration that names a different model than the job is refused,
        // naming the job: another harness (a route that ran no part of it),
        // and a model that does not carry the job's alias.
        let declaration = |job_id: &str, route: &str| Declaration {
            job_id: job_id.to_owned(),
            route: route.to_owned(),
        };
        for route in ["pi/chatgpt-home/gpt-6.1-sol", "claude/-/gpt-6-coda"] {
            let refused = compute(
                &store,
                "ship",
                "feature/not-ours",
                900,
                &[declaration("crew-alias", route)],
            )
            .unwrap_err();
            let text = format!("{refused:#}");
            assert!(text.contains("crew-alias"), "{route}: {text}");
            assert!(text.contains("does not fit"), "{route}: {text}");
        }
        // A declaration naming a model that cannot be named at all is refused
        // for that instead, still naming the job.
        let refused = compute(
            &store,
            "ship",
            "feature/not-ours",
            900,
            &[declaration("crew-alias", "claude/-/opus")],
        )
        .unwrap_err();
        let text = format!("{refused:#}");
        assert!(text.contains("crew-alias"), "{text}");
        assert!(text.contains("names no known model"), "{text}");
        // So is one for a job that is not waiting on a declaration: a crew
        // job whose model is named, and a job that is not there at all.
        for job_id in ["crew-a", "no-such-job"] {
            let refused = compute(
                &store,
                "ship",
                "feature/not-ours",
                900,
                &[declaration(job_id, "claude/-/claude-opus-5-5")],
            )
            .unwrap_err();
            let text = format!("{refused:#}");
            assert!(text.contains(job_id), "{text}");
            assert!(text.contains("does not fit"), "{text}");
        }
        // The fitting declaration stands for that job alone, and can only
        // add crew models.
        let declared = compute(
            &store,
            "ship",
            "feature/not-ours",
            900,
            &[declaration("crew-alias", "claude/-/claude-opus-5-5")],
        )
        .unwrap();
        assert!(declared.unknown_jobs.is_empty());
        assert_eq!(
            declared.crew_jobs["claude-opus-5-5"],
            vec!["crew-a".to_owned(), "crew-alias".to_owned()]
        );
    }

    #[test]
    fn a_declaration_keeps_its_jobs_alias_and_one_stands_per_job() {
        let (_temp, store) = home();
        task(&store, "fix", None);
        // A job on a bare alias, one with no route at all, and one whose
        // model is named: only the first two wait on declarations.
        job(
            &store,
            "crew-alias",
            "fix",
            1000,
            Some("claude/-/opus"),
            json!([]),
            None,
        );
        job(&store, "crew-none", "fix", 1100, None, json!([]), None);
        job(
            &store,
            "crew-full",
            "fix",
            1200,
            Some("codex/-/gpt-6-luna"),
            json!([]),
            None,
        );
        let authors = compute(&store, "ship", "feature/not-ours", 0, &[]).unwrap();
        assert_eq!(authors.unknown_jobs, vec!["crew-alias", "crew-none"]);
        assert!(authors.crew_aliases.is_empty());
        let declaration = |job_id: &str, route: &str| Declaration {
            job_id: job_id.to_owned(),
            route: route.to_owned(),
        };

        // A repeated declaration for one job is refused outright, naming the
        // job: one declaration stands for one job, so no ship can hold two
        // versions of its route in play.
        let refused = compute(
            &store,
            "ship",
            "feature/not-ours",
            0,
            &[
                declaration("crew-alias", "claude/-/claude-opus-4-7"),
                declaration("crew-alias", "claude/-/claude-opus-5-5"),
            ],
        )
        .unwrap_err();
        let text = format!("{refused:#}");
        assert!(text.contains("crew-alias"), "{text}");
        assert!(text.contains("declared twice"), "{text}");

        // The wrong-version case: the job ran on the bare alias opus, and the
        // ship declares it claude-opus-4-7. The fit holds, since the declared
        // model carries the alias, but it proves no version, so the alias is
        // kept beside the declared model for the gate to over-exclude on.
        let declared = compute(
            &store,
            "ship",
            "feature/not-ours",
            0,
            &[declaration("crew-alias", "claude/-/claude-opus-4-7")],
        )
        .unwrap();
        assert_eq!(
            declared.crew_jobs["claude-opus-4-7"],
            vec!["crew-alias".to_owned()]
        );
        assert_eq!(
            declared.crew_aliases,
            BTreeMap::from([("opus".to_owned(), vec!["crew-alias".to_owned()])])
        );
        assert_eq!(declared.unknown_jobs, vec!["crew-none".to_owned()]);

        // A job with no route at all clears on its declaration too, but no
        // alias can be kept for it: nothing about its model is known to
        // over-exclude on, so only the declared model lands in the set.
        let declared = compute(
            &store,
            "ship",
            "feature/not-ours",
            0,
            &[
                declaration("crew-alias", "claude/-/claude-opus-4-7"),
                declaration("crew-none", "codex/-/gpt-6-luna"),
            ],
        )
        .unwrap();
        assert!(declared.unknown_jobs.is_empty());
        assert_eq!(declared.crew_aliases.len(), 1);
        // The declared model lands in the crew class like any job's own: here
        // it joins the job that already ran on it.
        assert_eq!(
            declared.crew_jobs["gpt-6-luna"],
            vec!["crew-full".to_owned(), "crew-none".to_owned()]
        );
    }
}
