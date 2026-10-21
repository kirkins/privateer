//! The review gate: a top-tier review of a PR's exact head, by a route whose
//! model did not write the code, before a ship merges it. Part B of
//! docs/design/review-gate-and-route-strength.md; this is round B1.
//!
//! `review.start` reads the PR from the forge itself, works out the author
//! set (`authors`) from the PR's head branch, chooses the reviewer clear of
//! it (`crew_routes::choose_reviewer`), prepares a detached, read-only
//! worktree at the head with the diff and the PR's text beside it, builds
//! the whole prompt (`prompt`), and runs it as a fresh errand on the
//! reviewer route under the read-only `reviewer` role. It holds the limits
//! on asking again: one finished verdict per head, two attempts with no
//! verdict, no new review of a file tree that got changes requested, and a
//! notice to the captain at a PR's fourth review.
//!
//! Settlement (from `errand::settle`) reads the verdict from the reply's
//! last line and keeps only the structured findings, filtered (`filter`): a
//! reply carrying a secret or a path outside the worktree is refused and
//! nothing of it is kept, so only filtered records are posted. The worktree
//! is removed then, and the verdict is posted to the forge as the reviewer
//! account (`post`): an approval only while the PR's head is still the one
//! reviewed, or it is `stale`. A post that fails is retried on the next
//! ticks (`tick`), idempotent through the review's marker, and the ship
//! hears when it lands or is given up on.
//!
//! `pr.merge` runs every merge check in order, then merges as the merger
//! account at exactly the head it checked, so a push since makes the forge
//! refuse it. The repository must be in the captain's forges.json
//! (`crate::forges`), which names its forge, protected branch, required
//! checks and accounts.
//!
//! Only a first mate reviews or merges: crew are refused by the crew role's
//! default deny, and an errand is refused here. Under one OS user this is a
//! workflow control, not a security boundary.

pub mod authors;
mod filter;
mod pr;
mod prompt;

use crate::store::{Caller, Store, required};
use anyhow::{Context, Result, bail, ensure};
use filter::Finding;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Characters of the ship's notes a review prompt carries.
const NOTES_CHARS: usize = 2_000;
/// Attempts at one head that may end with no verdict.
const NO_VERDICT_ATTEMPTS: i64 = 2;
/// The review of one PR at which the captain hears of it.
const CAPTAIN_NOTICE_AT: i64 = 4;
/// Reviews shown in a ship's bearings.
const BEARINGS_REVIEWS: i64 = 5;
/// Tries at posting one verdict: at settlement, then on each of the next
/// three ticks.
const POST_TRIES: i64 = 4;
/// Days a settled review's folder (its diff and text) is kept.
const FOLDER_DAYS: i64 = 7;
/// How long one dispatcher tick may spend posting verdicts, retries and new
/// settlements together, so a hung forge cannot stall it: a forge call is
/// stopped at the budget's end, and what is left waits for the next tick.
const POST_BUDGET: Duration = Duration::from_secs(30);
/// The end of the current tick's posting budget, set by `tick` and shared
/// by the settlements that follow it in the same tick.
static POSTING_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

/// The posting budget; a debug build (the test suites) may shorten it with
/// `PRIVATEER_REVIEW_POST_BUDGET_MS`.
fn post_budget() -> Duration {
    if cfg!(debug_assertions)
        && let Some(ms) = std::env::var("PRIVATEER_REVIEW_POST_BUDGET_MS")
            .ok()
            .and_then(|v| v.parse().ok())
    {
        return Duration::from_millis(ms);
    }
    POST_BUDGET
}

/// When posting must stop: the current tick's budget, or a fresh one for a
/// settlement no tick began.
fn posting_until() -> Instant {
    let until = *POSTING_UNTIL.lock().unwrap_or_else(|e| e.into_inner());
    until.unwrap_or_else(|| Instant::now() + post_budget())
}

pub(crate) use filter::clip;
pub(crate) use pr::run_until as run_bounded;

pub fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS reviews(id TEXT PRIMARY KEY, ship TEXT NOT NULL,
           task_id TEXT NOT NULL, forge TEXT NOT NULL, repo TEXT NOT NULL, pr INTEGER NOT NULL,
           base_ref TEXT NOT NULL, base_sha TEXT NOT NULL, head_ref TEXT NOT NULL,
           head_sha TEXT NOT NULL, head_tree TEXT NOT NULL, errand_id TEXT NOT NULL UNIQUE,
           route_name TEXT NOT NULL, route_identity TEXT NOT NULL, route_model TEXT NOT NULL,
           route_tier INTEGER, route_uses TEXT NOT NULL, route_why TEXT NOT NULL,
           overlap TEXT, fresh INTEGER NOT NULL, authors TEXT NOT NULL, declared TEXT NOT NULL,
           folder TEXT NOT NULL, clone TEXT NOT NULL, verdict TEXT NOT NULL, findings TEXT,
           refusal TEXT, post_state TEXT NOT NULL, forge_review_id TEXT, post_error TEXT,
           created_ms INTEGER NOT NULL, finished_ms INTEGER, posted_ms INTEGER,
           merged_ms INTEGER, merge_sha TEXT, cleaned_ms INTEGER);
         CREATE INDEX IF NOT EXISTS review_pr ON reviews(repo, pr, created_ms);
         CREATE INDEX IF NOT EXISTS review_ship ON reviews(ship, created_ms);",
    )?;
    // Round B2: the forges.json entry a review posts through, and its tries.
    let columns: Vec<String> = store
        .connection()
        .prepare("SELECT name FROM pragma_table_info('reviews')")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for (column, definition) in [
        ("forge_key", "TEXT"),
        ("post_attempts", "INTEGER NOT NULL DEFAULT 0"),
        ("folder_removed_ms", "INTEGER"),
    ] {
        if !columns.iter().any(|c| c == column) {
            store.connection().execute_batch(&format!(
                "ALTER TABLE reviews ADD COLUMN {column} {definition};"
            ))?;
        }
    }
    Ok(())
}

/// The ship a review or merge command acts for. A crew never reaches here
/// (the crew role's default deny refuses it), and an errand is refused: only
/// a first mate reviews or merges.
fn first_mate(store: &Store, command: &str) -> Result<String> {
    if let Some(job) = crate::crew_scope::job_id()
        && crate::errand::is_errand(store, &job)?
    {
        bail!("{command} is the first mate's own; errand {job} cannot review or merge");
    }
    match store.caller() {
        Caller::Agent { actor } => Ok(actor.clone()),
        Caller::Operator => crate::presence::folder_ship(store)
            .with_context(|| format!("run {command} from a ship's folder")),
    }
}

fn pr_number(args: &Value) -> Result<u64> {
    args.get("pr")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .context("pr must be a pull request number")
}

/// The task, its charter, its clone and the clone's forges.json entry, for a
/// ship that owns the task and may merge its work.
fn task_clone(
    store: &Store,
    ship: &str,
    args: &Value,
) -> Result<(String, PathBuf, pr::Clone, crate::forges::Repo)> {
    let task_id = required(args, "task_id")?.to_owned();
    let charter = store.authorize(&task_id, "merge_green")?;
    ensure!(
        store.task(&task_id)?["owner"] == ship,
        "task {task_id} is not {ship}'s"
    );
    let charter_repo = std::fs::canonicalize(charter["repo"].as_str().context("charter repo")?)?;
    let requested = args
        .get("repo")
        .filter(|r| !r.is_null())
        .map(|r| r.as_str().context("repo must be owner/name"))
        .transpose()?;
    let clone = pr::locate(&charter_repo, requested)?;
    let forge = crate::forges::for_origin(clone.host.as_deref(), &clone.repo)?;
    Ok((task_id, charter_repo, clone, forge))
}

/// The author routes a ship declares, one per crew job whose own route
/// cannot be read. Accepted as declared (captain answer Q3) and recorded.
fn declarations(args: &Value) -> Result<Vec<authors::Declaration>> {
    let Some(list) = args.get("author_routes").filter(|v| !v.is_null()) else {
        return Ok(vec![]);
    };
    list.as_array()
        .context("author_routes must be a list of {job_id, route}")?
        .iter()
        .map(|d| {
            Ok(authors::Declaration {
                job_id: d["job_id"]
                    .as_str()
                    .context("each author route names its job_id")?
                    .to_owned(),
                route: d["route"]
                    .as_str()
                    .context("each author route names its route")?
                    .to_owned(),
            })
        })
        .collect()
}

fn declared_json(declared: &[authors::Declaration]) -> Value {
    json!(
        declared
            .iter()
            .map(|d| json!({"job_id": d.job_id, "route": d.route}))
            .collect::<Vec<_>>()
    )
}

fn authors_json(authors: &authors::Authors) -> Value {
    json!({
        "crew_models": authors.crew_jobs,
        "crew_aliases": authors.crew_aliases,
        "first_mate_models": authors.first_mate_models,
    })
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

/// The limits on asking for a review again, checked under the write lock
/// that records the new one: one review at a time, one finished verdict per
/// head, two attempts with no verdict, and no review of a file tree that
/// already got changes requested.
fn check_limits(
    tx: &rusqlite::Connection,
    repo: &str,
    number: u64,
    head: &str,
    tree: &str,
) -> Result<()> {
    let at_head = |verdicts: &str| -> Result<Vec<String>> {
        let mut statement = tx.prepare(&format!(
            "SELECT id FROM reviews WHERE repo=?1 AND pr=?2 AND head_sha=?3 AND verdict IN ({verdicts})
             ORDER BY created_ms"
        ))?;
        let ids = statement.query_map(params![repo, number, head], |r| r.get(0))?;
        Ok(ids.collect::<rusqlite::Result<_>>()?)
    };
    if let Some(running) = at_head("'running'")?.first() {
        bail!(
            "#{number} at {} is already under review ({running})",
            short(head)
        );
    }
    if let Some(done) = at_head("'approve','changes_requested'")?.first() {
        bail!(
            "#{number} at {} already has a finished verdict ({done}); a new review needs a new head",
            short(head)
        );
    }
    let attempts = at_head("'none','failed'")?;
    ensure!(
        (attempts.len() as i64) < NO_VERDICT_ATTEMPTS,
        "#{number} at {} has had {} reviews end with no verdict ({}); push a new head",
        short(head),
        attempts.len(),
        attempts.join(", ")
    );
    // An approval that carried a must-fix is recorded as no verdict, but it
    // found the same fault: it blocks the same files as a change request
    // does, so an empty commit cannot buy a fresh look at identical code.
    let same_tree: Option<(String, String)> = tx
        .query_row(
            "SELECT head_sha, verdict FROM reviews WHERE repo=?1 AND pr=?2 AND head_tree=?3
               AND (verdict='changes_requested' OR EXISTS(SELECT 1 FROM json_each(
                 CASE WHEN json_valid(findings) THEN findings ELSE '[]' END)
                 WHERE json_extract(value,'$.severity')='must-fix'))
               ORDER BY created_ms DESC LIMIT 1",
            params![repo, number, tree],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((earlier, verdict)) = same_tree {
        bail!(
            "#{number} at {} has the same file tree as {}, which got {}; \
             change the files, not only the commits",
            short(head),
            short(&earlier),
            if verdict == "changes_requested" {
                "changes requested"
            } else {
                "a must-fix finding"
            }
        );
    }
    Ok(())
}

/// The findings of earlier reviews of a PR, for the next prompt.
fn earlier(store: &Store, repo: &str, number: u64) -> Result<Vec<(String, String, Vec<String>)>> {
    let mut statement = store.connection().prepare(
        "SELECT head_sha, verdict, findings FROM reviews WHERE repo=?1 AND pr=?2
           AND verdict NOT IN ('running') ORDER BY created_ms",
    )?;
    let rows = statement.query_map(params![repo, number], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut found = vec![];
    for row in rows {
        let (head, verdict, findings) = row?;
        let findings: Vec<Finding> = findings
            .and_then(|f| serde_json::from_str(&f).ok())
            .unwrap_or_default();
        found.push((
            head,
            verdict_word(&verdict).to_owned(),
            finding_lines(&findings),
        ));
    }
    Ok(found)
}

fn verdict_word(verdict: &str) -> &str {
    match verdict {
        "approve" => "APPROVE",
        "changes_requested" => "CHANGES_REQUESTED",
        "none" => "no verdict",
        "running" => "running",
        _ => "failed",
    }
}

fn finding_lines(findings: &[Finding]) -> Vec<String> {
    findings
        .iter()
        .map(|f| format!("{} {}:{} {}", f.severity, f.file, f.line, f.text))
        .collect()
}

/// `review.start {"task_id", "pr", "repo"?, "notes"?, "author_routes"?}`.
fn start(store: &mut Store, args: &Value) -> Result<Value> {
    let ship = first_mate(store, "review.start")?;
    let number = pr_number(args)?;
    let notes = args
        .get("notes")
        .filter(|n| !n.is_null())
        .map(|n| n.as_str().context("notes must be text"))
        .transpose()?;
    ensure!(
        notes.is_none_or(|n| n.chars().count() <= NOTES_CHARS),
        "notes are at most {NOTES_CHARS} characters"
    );
    let declared = declarations(args)?;
    let (task_id, charter_repo, clone, forge) = task_clone(store, &ship, args)?;
    let required = crate::forges::required_checks(&forge)?;
    let live = pr::view(&forge, number, &required, None)?;
    ensure!(
        live.state == "open",
        "#{number} in {} is {}; only an open pull request is reviewed",
        clone.repo,
        live.state
    );
    pr::fetch(&clone, number, &[&live.base_sha, &live.head_sha])?;
    let tree = pr::tree_of(&clone.dir, &live.head_sha)?;
    check_limits(
        store.connection(),
        &clone.repo,
        number,
        &live.head_sha,
        &tree,
    )?;
    let merge_base = pr::merge_base_ms(&clone.dir, &live.base_sha, &live.head_sha)?;
    let authors = authors::compute(store, &ship, &live.head_ref, merge_base, &declared)?;
    let reviewer = crate::crew_routes::choose_reviewer(store, &ship, &authors)?;
    let choice = &reviewer.choice;
    let identity = choice
        .identity
        .clone()
        .context("the reviewer route names no route")?;
    let model =
        crate::routes::model_key(&identity).context("the reviewer's model cannot be named")?;
    let uuid = uuid::Uuid::new_v4();
    let id = format!("review-{uuid}");
    let errand_id = format!("errand-{uuid}");
    // Inside the charter repository, where a launch may run: the home's own
    // reviews folder.
    let reviews = store.home().join("reviews");
    std::fs::create_dir_all(&reviews)?;
    let folder = std::fs::canonicalize(&reviews)?.join(&id);
    ensure!(
        folder.starts_with(&charter_repo),
        "the review folder {} is outside the charter repository {}",
        folder.display(),
        charter_repo.display()
    );
    let files = match pr::prepare(&clone, &folder, &live, number) {
        Ok(files) => files,
        Err(error) => {
            pr::remove(Some(&clone.dir), &folder);
            return Err(error);
        }
    };
    let path = |p: &Path| p.to_string_lossy().into_owned();
    let earlier = earlier(store, &clone.repo, number)?;
    let text = prompt::build(&prompt::Facts {
        id: &id,
        repo: &clone.repo,
        number,
        base: &live.base,
        base_sha: &live.base_sha,
        head_sha: &live.head_sha,
        listing: &files.listing,
        tree: &path(&files.tree),
        diff: &path(&files.diff),
        stats: &path(&files.stats),
        pr_md: &path(&files.pr_md),
        earlier: &earlier,
        title: &live.title,
        body: &live.body,
        notes,
        cut: &files.cut,
    });
    let recorded = (|| -> Result<i64> {
        let tx = store.write_transaction()?;
        // Again under the lock: another start may have recorded one since.
        check_limits(&tx, &clone.repo, number, &live.head_sha, &tree)?;
        let before: i64 = tx.query_row(
            "SELECT COUNT(*) FROM reviews WHERE repo=?1 AND pr=?2",
            params![clone.repo, number],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO reviews(id, ship, task_id, forge, repo, pr, base_ref, base_sha, head_ref,
               head_sha, head_tree, errand_id, route_name, route_identity, route_model, route_tier,
               route_uses, route_why, overlap, fresh, authors, declared, folder, clone, verdict,
               post_state, created_ms, forge_key)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,
               ?23,?24,'running','none',?25,?26)",
            params![
                id,
                ship,
                task_id,
                forge.forge,
                clone.repo,
                number,
                live.base,
                live.base_sha,
                live.head_ref,
                live.head_sha,
                tree,
                errand_id,
                choice.name,
                identity,
                model,
                choice.tier,
                json!(choice.uses).to_string(),
                choice.why,
                reviewer.overlap,
                reviewer.fresh,
                authors_json(&authors).to_string(),
                declared_json(&declared).to_string(),
                path(&folder),
                path(&clone.dir),
                Store::now_ms(),
                forge.key
            ],
        )?;
        tx.commit()?;
        Ok(before + 1)
    })();
    let attempt = match recorded {
        Ok(attempt) => attempt,
        Err(error) => {
            pr::remove(Some(&clone.dir), &folder);
            return Err(error);
        }
    };
    let launched = crate::errand::start_review(
        store,
        &crate::errand::ReviewLaunch {
            id: &errand_id,
            ship: &ship,
            task_id: &task_id,
            brief: &format!(
                "review of #{number} in {} at {}",
                clone.repo,
                short(&live.head_sha)
            ),
            choice,
            prompt: &text,
        },
    );
    if let Err(error) = launched {
        // A launch that never ran is no attempt.
        store
            .connection()
            .execute("DELETE FROM reviews WHERE id=?1", [&id])?;
        pr::remove(Some(&clone.dir), &folder);
        return Err(error);
    }
    // Only once the review runs: a launch that failed is no attempt, and the
    // next one is still this PR's fourth.
    if attempt == CAPTAIN_NOTICE_AT {
        crate::mail::record_runtime(
            store,
            &json!({"id": format!("{id}-captain"), "from": ship, "to": crate::mail::CAPTAIN,
                "kind": "notice", "task_id": task_id, "urgency": "normal", "payload": {"review": id},
                "body": format!("{ship} has started review {CAPTAIN_NOTICE_AT} of #{number} in {}, \
                    now at {}. Repeated reviews of one PR may mean it is being reviewed until one \
                    approves; review.list shows them.", clone.repo, short(&live.head_sha))}),
        )?;
    }
    store.audit(
        "review.start",
        &json!({"review": id, "errand": errand_id, "ship": ship, "repo": clone.repo,
            "pr": number, "head": live.head_sha, "route": choice.name}),
    )?;
    Ok(json!({
        "review": id,
        "errand": errand_id,
        "repo": clone.repo,
        "pr": number,
        "head": live.head_sha,
        "attempt": attempt,
        "reviewer": {
            "route": choice.name,
            "identity": identity,
            "tier": choice.tier,
            "overlap": reviewer.overlap,
            "fresh": reviewer.fresh,
            "why": choice.why,
        },
        "declared": declared_json(&declared),
        "authors": authors_json(&authors),
        "status": "running",
    }))
}

/// The worktree a running review errand reads, so its launch runs there.
pub(crate) fn tree_of_errand(store: &Store, errand: &str) -> Result<Option<PathBuf>> {
    let folder: Option<String> = store
        .connection()
        .query_row(
            "SELECT folder FROM reviews WHERE errand_id=?1 AND verdict='running'",
            [errand],
            |r| r.get(0),
        )
        .optional()?;
    Ok(folder.map(|f| PathBuf::from(f).join("tree")))
}

/// Whether a job is a review's errand.
pub(crate) fn is_review_errand(store: &Store, errand: &str) -> Result<bool> {
    Ok(store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM reviews WHERE errand_id=?1)",
        [errand],
        |r| r.get(0),
    )?)
}

/// A review's verdict notice to its ship, and the payload beside it.
pub(crate) struct Settled {
    pub body: String,
    pub payload: Value,
    pub urgency: &'static str,
}

/// Record a review errand's verdict, keeping only its filtered findings, and
/// remove its worktree. None when the errand is not a review's. Idempotent:
/// a review already settled gives its notice again.
pub(crate) fn settle(
    store: &Store,
    errand: &str,
    status: &str,
    outcome: &str,
    reply: Option<&str>,
) -> Result<Option<Settled>> {
    let row: Option<(String, String, String, String)> = store
        .connection()
        .query_row(
            "SELECT id, verdict, folder, clone FROM reviews WHERE errand_id=?1",
            [errand],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((id, verdict, folder, clone)) = row else {
        return Ok(None);
    };
    let folder = PathBuf::from(folder);
    if verdict == "running" {
        let tree = folder.join("tree");
        let home = std::env::var_os("HOME");
        let (verdict, findings, refusal) = match (outcome, reply) {
            ("succeeded", Some(reply)) => match crate::credentials::stored_values() {
                Err(error) => (
                    "failed",
                    vec![],
                    Some(format!(
                        "refused: the stored credentials could not be read to filter the reply ({error:#})"
                    )),
                ),
                Ok(secrets) => match filter::findings(reply, filter::Allowed {
                    tree: &tree,
                    folder: &folder,
                    home: home.as_deref().map(Path::new),
                }, &secrets) {
                    Err(why) => ("failed", vec![], Some(format!("refused by the filter: {why}"))),
                    Ok(found) => match filter::verdict(reply) {
                        "approve" if found.iter().any(|f| f.severity == "must-fix") => (
                            "none",
                            found,
                            Some("it approved with a must-fix finding, which counts as no verdict".to_owned()),
                        ),
                        "none" => (
                            "none",
                            found,
                            Some("its last line was not exactly VERDICT: APPROVE or VERDICT: CHANGES_REQUESTED".to_owned()),
                        ),
                        verdict => (verdict, found, None),
                    },
                },
            },
            _ => (
                "failed",
                vec![],
                Some(format!("the review errand {status} without a reply")),
            ),
        };
        let post_state = if matches!(verdict, "approve" | "changes_requested") {
            "pending"
        } else {
            "withheld"
        };
        store.connection().execute(
            "UPDATE reviews SET verdict=?2, findings=?3, refusal=?4, post_state=?5, finished_ms=?6
             WHERE id=?1 AND verdict='running'",
            params![
                id,
                verdict,
                serde_json::to_string(&findings)?,
                refusal,
                post_state,
                Store::now_ms()
            ],
        )?;
        pr::remove(Some(Path::new(&clone)), &folder);
        post(store, &id, posting_until())?;
    }
    pr::remove(Some(Path::new(&clone)), &folder);
    store.connection().execute(
        "UPDATE reviews SET cleaned_ms=?2 WHERE id=?1 AND cleaned_ms IS NULL",
        params![id, Store::now_ms()],
    )?;
    let review = record(store, &id)?;
    let verdict = review["verdict"].as_str().unwrap_or_default();
    Ok(Some(Settled {
        body: summary(&review),
        payload: json!({"review": id, "verdict": verdict, "pr": review["pr"],
            "head": review["head_sha"], "post_state": review["post_state"]}),
        urgency: if verdict == "failed" {
            "high"
        } else {
            "normal"
        },
    }))
}

/// The verdict notice: the head, the verdict and what becomes of it, the
/// reviewer, any declared author routes, and the kept findings.
fn summary(review: &Value) -> String {
    let text = |key: &str| review[key].as_str().unwrap_or_default().to_owned();
    let number = review["pr"].as_u64().unwrap_or_default();
    let head = text("head_sha");
    let verdict = text("verdict");
    let mut body = match verdict.as_str() {
        "approve" | "changes_requested" => format!(
            "Review of #{number} at {}: {}, {}.\n",
            short(&head),
            verdict_word(&verdict),
            post_words(review)
        ),
        "none" => format!(
            "Review of #{number} at {}: no verdict, because {}. Nothing will be posted.\n",
            short(&head),
            text("refusal")
        ),
        _ => format!(
            "Review of #{number} at {} failed: {}. Nothing will be posted.\n",
            short(&head),
            text("refusal")
        ),
    };
    body.push_str(&format!(
        "Reviewer: {} ({}, tier {}), in a fresh session{}.\n",
        text("route_name"),
        text("route_identity"),
        review["route_tier"],
        if review["overlap"] == "first_mate" {
            "; its model overlaps only the first mate's"
        } else {
            ""
        }
    ));
    if let Some(declared) = review["declared"].as_array().filter(|d| !d.is_empty()) {
        body.push_str(&format!(
            "Declared author routes, taken as declared and recorded: {}.\n",
            declared
                .iter()
                .map(|d| format!(
                    "{} as {}",
                    d["job_id"].as_str().unwrap_or_default(),
                    d["route"].as_str().unwrap_or_default()
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let findings: Vec<Finding> =
        serde_json::from_value(review["findings"].clone()).unwrap_or_default();
    if !findings.is_empty() {
        body.push_str("Findings:\n");
        for line in finding_lines(&findings) {
            body.push_str(&format!("- {line}\n"));
        }
    }
    body.push_str(&format!(
        "Review {} (errand {}); review.list shows it.\n",
        text("id"),
        text("errand_id")
    ));
    body
}

/// What became of a verdict's post, for its notices.
fn post_words(review: &Value) -> String {
    let error = review["post_error"].as_str().unwrap_or_default();
    match review["post_state"].as_str().unwrap_or_default() {
        "posted" => "posted".to_owned(),
        "stale" => format!(
            "not posted: {error}; the approval is stale, and a new review.start reviews the new head"
        ),
        "failed" => {
            let left = POST_TRIES - review["post_attempts"].as_i64().unwrap_or_default();
            if left > 0 {
                format!("not posted yet: {error}; it is tried again {left} more time(s)")
            } else {
                format!(
                    "not posted: {error}; posting gave up after {POST_TRIES} tries, so the \
                     captain's reviewer token or the forge needs attention"
                )
            }
        }
        _ => "not posted yet".to_owned(),
    }
}

const COLUMNS: &str = "id, ship, task_id, forge, repo, pr, base_ref, base_sha, head_ref, head_sha,
    errand_id, route_name, route_identity, route_model, route_tier, route_uses, route_why, overlap,
    fresh, authors, declared, verdict, findings, refusal, post_state, created_ms, finished_ms,
    cleaned_ms, forge_key, post_attempts, forge_review_id, post_error, posted_ms, merged_ms,
    merge_sha, folder";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    let parsed = |at: usize| -> rusqlite::Result<Value> {
        Ok(r.get::<_, Option<String>>(at)?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null))
    };
    Ok(json!({
        "id": r.get::<_, String>(0)?, "ship": r.get::<_, String>(1)?,
        "task_id": r.get::<_, String>(2)?, "forge": r.get::<_, String>(3)?,
        "repo": r.get::<_, String>(4)?, "pr": r.get::<_, i64>(5)?,
        "base_ref": r.get::<_, String>(6)?, "base_sha": r.get::<_, String>(7)?,
        "head_ref": r.get::<_, String>(8)?, "head_sha": r.get::<_, String>(9)?,
        "errand_id": r.get::<_, String>(10)?, "route_name": r.get::<_, String>(11)?,
        "route_identity": r.get::<_, String>(12)?, "route_model": r.get::<_, String>(13)?,
        "route_tier": r.get::<_, Option<i64>>(14)?, "route_uses": parsed(15)?,
        "route_why": r.get::<_, String>(16)?, "overlap": r.get::<_, Option<String>>(17)?,
        "fresh": r.get::<_, bool>(18)?, "authors": parsed(19)?, "declared": parsed(20)?,
        "verdict": r.get::<_, String>(21)?, "findings": parsed(22)?,
        "refusal": r.get::<_, Option<String>>(23)?, "post_state": r.get::<_, String>(24)?,
        "created_ms": r.get::<_, i64>(25)?, "finished_ms": r.get::<_, Option<i64>>(26)?,
        "cleaned_ms": r.get::<_, Option<i64>>(27)?,
        "forge_key": r.get::<_, Option<String>>(28)?,
        "post_attempts": r.get::<_, i64>(29)?,
        "forge_review_id": r.get::<_, Option<String>>(30)?,
        "post_error": r.get::<_, Option<String>>(31)?,
        "posted_ms": r.get::<_, Option<i64>>(32)?,
        "merged_ms": r.get::<_, Option<i64>>(33)?,
        "merge_sha": r.get::<_, Option<String>>(34)?,
        "folder": r.get::<_, String>(35)?,
    }))
}

fn record(store: &Store, id: &str) -> Result<Value> {
    Ok(store.connection().query_row(
        &format!("SELECT {COLUMNS} FROM reviews WHERE id=?1"),
        [id],
        row,
    )?)
}

/// The review as the forge shows it: the verdict, the reviewer route, the
/// filtered findings, and the marker that makes posting idempotent. Built
/// from the stored record alone; nothing of the reply beyond its filtered
/// findings is in it.
fn post_body(review: &Value) -> String {
    let text = |key: &str| review[key].as_str().unwrap_or_default();
    let mut body = format!(
        "**Privateer review: {}** at `{}`\n\nReviewed by route {} ({}, tier {}) in a fresh, \
         read-only session.\n",
        verdict_word(text("verdict")),
        text("head_sha"),
        text("route_name"),
        text("route_identity"),
        review["route_tier"]
    );
    let findings: Vec<Finding> =
        serde_json::from_value(review["findings"].clone()).unwrap_or_default();
    if findings.is_empty() {
        body.push_str("\nNo findings.\n");
    } else {
        body.push_str("\nFindings:\n");
        for f in &findings {
            body.push_str(&format!(
                "- {} {} {}\n",
                f.severity,
                code_span(&format!("{}:{}", f.file, f.line)),
                f.text.replace("<!--", "&lt;!--")
            ));
        }
    }
    body.push_str(&format!("\n<!-- privateer-review {} -->\n", text("id")));
    body
}

/// Text as a markdown code span, so nothing in it (a backtick, `*`, `_`,
/// `[`, `<`) is read as markdown: fenced by one more backtick than its
/// longest run of them, and padded when it starts or ends with one.
fn code_span(text: &str) -> String {
    let longest = text
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    let fence = "`".repeat(longest + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{text}{pad}{fence}")
}

/// Post a finished verdict to the forge as the reviewer account, once: an
/// approval only while the PR is open at the reviewed head, else it is
/// `stale`; a change request at the reviewed head whatever the PR's head is
/// now. A failure is recorded as `failed` with its try counted; the forge
/// adapter finds the review's marker among the reviewer's reviews before it
/// posts, so a try after a post whose reply was lost posts nothing twice.
/// Each forge call stops at `until`; past it, nothing is tried and the
/// verdict waits, untried, for the next tick.
fn post(store: &Store, id: &str, until: Instant) -> Result<()> {
    let review = record(store, id)?;
    let verdict = review["verdict"].as_str().unwrap_or_default();
    if !matches!(review["post_state"].as_str(), Some("pending" | "failed"))
        || !matches!(verdict, "approve" | "changes_requested")
        || Instant::now() >= until
    {
        return Ok(());
    }
    let head = review["head_sha"].as_str().unwrap_or_default().to_owned();
    let number = review["pr"].as_u64().unwrap_or_default();
    let tried = (|| -> Result<std::result::Result<Option<String>, String>> {
        let forge = crate::forges::for_key(
            review["forge_key"]
                .as_str()
                .context("the review names no forges.json entry")?,
        )?;
        if verdict == "approve" {
            // Posting needs the PR's state and head, not its checks.
            let live = pr::view(&forge, number, &[], Some(until))?;
            if live.state != "open" {
                return Ok(Err(format!("#{number} is {}", live.state)));
            }
            if live.head_sha != head {
                return Ok(Err(format!(
                    "the head moved to {} before it was posted",
                    short(&live.head_sha)
                )));
            }
        }
        let reply = crate::forges::call_until(
            &forge,
            &forge.reviewer,
            "review.post",
            json!({"pr": number, "commit_id": head,
                "event": if verdict == "approve" { "APPROVE" } else { "REQUEST_CHANGES" },
                "body": post_body(&review), "reviewer": forge.reviewer.account}),
            Some(until),
        )?;
        Ok(Ok(match &reply["review_id"] {
            Value::Null => None,
            Value::String(id) => Some(id.clone()),
            other => Some(other.to_string()),
        }))
    })();
    let connection = store.connection();
    match tried {
        Ok(Ok(forge_id)) => connection.execute(
            "UPDATE reviews SET post_state='posted', forge_review_id=?2, posted_ms=?3,
               post_error=NULL, post_attempts=post_attempts+1
             WHERE id=?1 AND post_state IN ('pending','failed')",
            params![id, forge_id, Store::now_ms()],
        )?,
        Ok(Err(why)) => connection.execute(
            "UPDATE reviews SET post_state='stale', post_error=?2
             WHERE id=?1 AND post_state IN ('pending','failed')",
            params![id, why],
        )?,
        Err(error) => connection.execute(
            "UPDATE reviews SET post_state='failed', post_error=?2, post_attempts=post_attempts+1
             WHERE id=?1 AND post_state IN ('pending','failed')",
            params![id, filter::clip(&format!("{error:#}"), 300)],
        )?,
    };
    Ok(())
}

/// Each tick: try again the verdicts whose post failed or never ran (a
/// restart between settlement and posting, or a tick's budget spent),
/// telling the ship when one lands late, goes stale or is given up on; and
/// remove the folders of reviews settled more than a week ago. Posting
/// stops when the tick's budget is spent, and the rest waits.
pub(crate) fn tick(store: &Store) -> Result<Vec<Value>> {
    let until = Instant::now() + post_budget();
    *POSTING_UNTIL.lock().unwrap_or_else(|e| e.into_inner()) = Some(until);
    let due: Vec<(String, String)> = store
        .connection()
        .prepare(
            "SELECT id, post_state FROM reviews WHERE post_state IN ('pending','failed')
               AND verdict IN ('approve','changes_requested') AND post_attempts < ?1
             ORDER BY finished_ms LIMIT 10",
        )?
        .query_map([POST_TRIES], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut tried = vec![];
    for (id, before) in due {
        if Instant::now() >= until {
            break;
        }
        post(store, &id, until)?;
        let review = record(store, &id)?;
        let after = review["post_state"].as_str().unwrap_or_default().to_owned();
        let attempts = review["post_attempts"].as_i64().unwrap_or_default();
        let notice = match after.as_str() {
            "posted" if before != "posted" => Some(("posted", "normal")),
            "stale" => Some(("stale", "normal")),
            "failed" if attempts >= POST_TRIES => Some(("post-failed", "normal")),
            _ => None,
        };
        if let Some((what, urgency)) = notice {
            crate::mail::record_crew(
                store.connection(),
                &json!({"id": format!("{id}-{what}"), "to": review["ship"], "kind": "notice",
                    "task_id": review["task_id"], "urgency": urgency,
                    "payload": {"review": id, "post_state": after},
                    "body": format!("Review of #{} at {}: {}, {}.\nReview {id}; review.list shows it.\n",
                        review["pr"], short(review["head_sha"].as_str().unwrap_or_default()),
                        verdict_word(review["verdict"].as_str().unwrap_or_default()),
                        post_words(&review))}),
            )?;
        }
        tried.push(json!({"review": id, "post_state": after}));
    }
    // Old review folders: only this home's own, by the path recorded.
    let reviews = store.home().join("reviews");
    let old = store
        .connection()
        .prepare(
            "SELECT id, folder FROM reviews WHERE cleaned_ms IS NOT NULL AND finished_ms < ?1
               AND folder_removed_ms IS NULL ORDER BY finished_ms LIMIT 20",
        )?
        .query_map([Store::now_ms() - FOLDER_DAYS * 86_400_000], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<rusqlite::Result<Vec<(String, String)>>>()?;
    for (id, folder) in old {
        store.connection().execute(
            "UPDATE reviews SET folder_removed_ms=?2 WHERE id=?1",
            params![id, Store::now_ms()],
        )?;
        let folder = PathBuf::from(folder);
        if folder.parent().and_then(|p| std::fs::canonicalize(p).ok())
            == std::fs::canonicalize(&reviews).ok()
            && folder.is_dir()
            && !folder.join("tree").exists()
        {
            let _ = std::fs::remove_dir_all(&folder);
        }
    }
    Ok(tried)
}

/// `review.list {"ship"?, "pr"?, "repo"?}`: an agent sees its own ship's.
fn list(store: &Store, args: &Value) -> Result<Value> {
    let ship = match store.caller() {
        Caller::Agent { actor } => Some(actor.clone()),
        Caller::Operator => args.get("ship").and_then(Value::as_str).map(str::to_owned),
    };
    let number = args.get("pr").and_then(Value::as_u64);
    let repo = args.get("repo").and_then(Value::as_str);
    let mut statement = store.connection().prepare(&format!(
        "SELECT {COLUMNS} FROM reviews WHERE (?1 IS NULL OR ship=?1) AND (?2 IS NULL OR pr=?2)
           AND (?3 IS NULL OR repo=?3) ORDER BY created_ms DESC LIMIT 50"
    ))?;
    let reviews = statement
        .query_map(params![ship, number, repo], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"reviews": reviews}))
}

/// A ship's latest reviews, one line each, for its bearings.
pub(crate) fn bearings_lines(store: &Store, ship: &str) -> Result<Vec<String>> {
    let mut statement = store.connection().prepare(&format!(
        "SELECT {COLUMNS} FROM reviews WHERE ship=?1 ORDER BY created_ms DESC LIMIT ?2"
    ))?;
    let reviews = statement
        .query_map(params![ship, BEARINGS_REVIEWS], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(reviews
        .iter()
        .map(|r| {
            let text = |key: &str| r[key].as_str().unwrap_or_default();
            let head = short(text("head_sha"));
            let number = &r["pr"];
            let by = format!("by {} (tier {})", text("route_name"), r["route_tier"]);
            let declared = if r["declared"].as_array().is_some_and(|d| !d.is_empty()) {
                ", with declared author routes"
            } else {
                ""
            };
            let findings = r["findings"].as_array().map_or(0, Vec::len);
            match text("verdict") {
                "running" => format!("#{number} {head} review running {by} ({})", text("errand_id")),
                "approve" if r["merged_ms"].is_i64() => {
                    format!("#{number} {head} APPROVE {by}, posted and merged{declared}")
                }
                "approve" => format!(
                    "#{number} {head} APPROVE {by}, {}{declared}",
                    filter::clip(&post_words(r), 160)
                ),
                "changes_requested" => format!(
                    "#{number} {head} CHANGES_REQUESTED {by}: {findings} finding(s) in {}{declared}",
                    text("id")
                ),
                "none" => format!("#{number} {head} no verdict {by} ({})", text("id")),
                _ => format!(
                    "#{number} {head} review failed {by}: {}",
                    filter::clip(text("refusal"), 120)
                ),
            }
        })
        .collect())
}

/// `pr.merge {"task_id", "pr", "repo"?, "deploys_production"?, "environment"?}`:
/// every merge check, in order, then the merge itself at the checked head.
fn merge(store: &mut Store, args: &Value) -> Result<Value> {
    let ship = first_mate(store, "pr.merge")?;
    let number = pr_number(args)?;
    let (task_id, _, clone, forge) = task_clone(store, &ship, args)?;
    let mut checks = vec![];
    // 1. The live PR, read from the forge now, its required checks from
    // forges.json or branch protection, never every check.
    let checks_needed = crate::forges::required_checks(&forge)?;
    let live = pr::view(&forge, number, &checks_needed, None)?;
    let head = live.head_sha.clone();
    ensure!(
        live.state == "open",
        "#{number} in {} is {}, not open",
        clone.repo,
        live.state
    );
    // The protected branch is the captain's, from forges.json: never the
    // clone's origin/HEAD, which a ship can change.
    let branch = forge.branch.clone();
    ensure!(
        live.base == branch,
        "#{number} merges into {}, not the protected branch {branch}",
        live.base
    );
    ensure!(
        live.checks_ok,
        "#{number}'s required checks are not green at {}: {}",
        short(&head),
        live.checks_detail
    );
    checks.push(
        json!({"check": "live_pr", "head": head, "base": branch, "required_checks": checks_needed}),
    );
    // 2. A finished approving review at that exact head.
    let latest: Option<String> = store
        .connection()
        .query_row(
            "SELECT id FROM reviews WHERE repo=?1 AND pr=?2 ORDER BY created_ms DESC LIMIT 1",
            params![clone.repo, number],
            |r| r.get(0),
        )
        .optional()?;
    let at_head: Option<String> = store
        .connection()
        .query_row(
            "SELECT id FROM reviews WHERE repo=?1 AND pr=?2 AND head_sha=?3 AND verdict!='running'
             ORDER BY finished_ms DESC LIMIT 1",
            params![clone.repo, number, head],
            |r| r.get(0),
        )
        .optional()?;
    let Some(review_id) = at_head else {
        let last = match &latest {
            Some(id) => {
                let last = record(store, id)?;
                format!(
                    " (the last review, {id}, was of {}: {})",
                    short(last["head_sha"].as_str().unwrap_or_default()),
                    verdict_word(last["verdict"].as_str().unwrap_or_default())
                )
            }
            None => String::new(),
        };
        bail!(
            "#{number} head {} has no approved top-tier review; start one with review.start{last}",
            short(&head)
        );
    };
    let review = record(store, &review_id)?;
    ensure!(
        review["ship"] == ship.as_str() && review["task_id"] == task_id.as_str(),
        "#{number} head {}: its review {review_id} was asked by {} for task {}, not by {ship} \
         for {task_id}",
        short(&head),
        review["ship"].as_str().unwrap_or_default(),
        review["task_id"].as_str().unwrap_or_default()
    );
    let verdict = review["verdict"].as_str().unwrap_or_default();
    ensure!(
        verdict == "approve",
        "#{number} head {}: its review {review_id} is {}, not APPROVE",
        short(&head),
        verdict_word(verdict)
    );
    ensure!(
        review["post_state"] == "posted",
        "#{number}'s approval {review_id} is {}: the forge must hold the reviewer's approval \
         at the head before a merge",
        post_words(&review)
    );
    let name = review["route_name"].as_str().unwrap_or_default().to_owned();
    let recorded_uses = review["route_uses"].as_array().cloned().unwrap_or_default();
    ensure!(
        review["route_tier"] == 1 && recorded_uses.iter().any(|u| u == "review"),
        "review {review_id} ran on {name}, which was not at tier 1 with the review use"
    );
    checks.push(
        json!({"check": "approved_review", "review": review_id, "route": name,
        "post_state": review["post_state"]}),
    );
    // 3. The reviewer route still qualifies in the current routes file.
    let policy = crate::crew_routes::for_store(store)?
        .context("the captain's routes file is gone; the reviewer route no longer qualifies")?;
    let route = &policy["routes"][name.as_str()];
    ensure!(
        route.is_object(),
        "reviewer route {name} is no longer in the routes file; a new review is needed"
    );
    let require = crate::crew_routes::Require {
        r#use: Some("review".to_owned()),
        tier: Some(1),
    };
    ensure!(
        crate::crew_routes::meets(route, Some(&require)),
        "reviewer route {name} no longer meets {}: it is {}; a new review is needed",
        require.summary(),
        crate::crew_routes::describe_strength(route)
    );
    let model = crate::crew_routes::identity(route)
        .as_deref()
        .and_then(crate::routes::model_key);
    ensure!(
        model.as_deref() == review["route_model"].as_str(),
        "reviewer route {name} now runs {}, not the model that reviewed ({}); a new review is needed",
        model.as_deref().unwrap_or("an unnamed model"),
        review["route_model"].as_str().unwrap_or_default()
    );
    checks.push(json!({"check": "route_qualifies", "route": name}));
    // 4. The author set worked out again now: a crew may have run since.
    pr::fetch(&clone, number, &[&live.base_sha, &head])?;
    let merge_base = pr::merge_base_ms(&clone.dir, &live.base_sha, &head)?;
    let declared: Vec<authors::Declaration> = review["declared"]
        .as_array()
        .map(|d| {
            d.iter()
                .map(|d| authors::Declaration {
                    job_id: d["job_id"].as_str().unwrap_or_default().to_owned(),
                    route: d["route"].as_str().unwrap_or_default().to_owned(),
                })
                .collect()
        })
        .unwrap_or_default();
    let authors = authors::compute(store, &ship, &live.head_ref, merge_base, &declared)?;
    ensure!(
        authors.unknown_jobs.is_empty(),
        "the model of crew job(s) {} cannot be named, since the review; declare their routes \
         in a new review.start",
        authors.unknown_jobs.join(", ")
    );
    let order = crate::crew_routes::order_reviewers(std::slice::from_ref(&name), &policy, &authors);
    if let Some((_, why)) = order.excluded.first() {
        bail!(
            "reviewer route {name} is now excluded by the code's authors: {why}; a new review is needed"
        );
    }
    checks.push(json!({"check": "authors", "authors": authors_json(&authors)}));
    // 5. Production: a merge that deploys needs an approval of this exact
    // artifact and environment, used up by the merge.
    let mut production = None;
    if args.get("deploys_production") == Some(&json!(true)) {
        let environment = required(args, "environment")?;
        let charter = store.authorize(&task_id, "merge_green")?;
        let approval: Option<String> = store
            .connection()
            .query_row(
                "SELECT id FROM approvals WHERE task_id=?1 AND artifact=?2 AND environment=?3
                   AND charter_hash=?4 AND consumed=0 ORDER BY id LIMIT 1",
                params![task_id, head, environment, crate::core::hash(&charter)],
                |r| r.get(0),
            )
            .optional()?;
        let Some(approval) = approval else {
            bail!("production requires approval of this exact artifact and environment");
        };
        checks.push(json!({"check": "production", "approval": approval}));
        production = Some((
            approval,
            environment.to_owned(),
            crate::core::hash(&charter),
        ));
    }
    // 6. The merge, as the merger account, passing the expected head: a push
    // since step 1 makes the forge refuse it.
    let merged = crate::forges::call(
        &forge,
        &forge.merger,
        "pr.merge",
        json!({"pr": number, "head_sha": head, "merge_method": "merge"}),
    )
    .with_context(|| format!("merge #{number} at {}", short(&head)))?;
    ensure!(
        merged["merged"] != false,
        "the forge did not merge #{number}: {}",
        merged["message"].as_str().unwrap_or("no reason given")
    );
    let merge_sha = merged["merge_sha"].as_str().map(str::to_owned);
    let tx = store.write_transaction()?;
    if let Some((approval, environment, charter_hash)) = &production {
        let used = tx.execute(
            "UPDATE approvals SET consumed=1 WHERE id=?1 AND task_id=?2 AND artifact=?3
               AND environment=?4 AND charter_hash=?5 AND consumed=0",
            params![approval, task_id, head, environment, charter_hash],
        )?;
        ensure!(
            used == 1,
            "#{number} merged at {}, but production approval {approval} was used up meanwhile; \
             tell the captain",
            short(&head)
        );
    }
    tx.execute(
        "UPDATE reviews SET merged_ms=?2, merge_sha=?3 WHERE id=?1",
        params![review_id, Store::now_ms(), merge_sha],
    )?;
    tx.commit()?;
    store.audit(
        "pr.merge",
        &json!({"ship": ship, "repo": forge.key, "pr": number, "head": head, "review": review_id,
            "merge_sha": merge_sha, "production": production.as_ref().map(|p| &p.0)}),
    )?;
    Ok(json!({
        "merged": true,
        "repo": clone.repo,
        "pr": number,
        "head": head,
        "merge_sha": merge_sha,
        "review": review_id,
        "checks": checks,
    }))
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "review.start" => start(store, &args),
        "review.list" => list(store, &args),
        "pr.merge" => merge(store, &args),
        _ => bail!("unknown review command {command}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_posted_file_name_is_a_code_span_and_a_finding_cannot_add_a_marker() {
        assert_eq!(code_span("src/a.rs:1"), "`src/a.rs:1`");
        assert_eq!(code_span("src/a`b.rs:2"), "``src/a`b.rs:2``");
        assert_eq!(code_span("`x``:3"), "``` `x``:3 ```");
        assert_eq!(code_span("src/*_[x](y)<z>.rs:4"), "`src/*_[x](y)<z>.rs:4`");
        let review = json!({"id": "review-1", "verdict": "approve", "head_sha": "a".repeat(40),
            "route_name": "opus", "route_identity": "claude/-/claude-opus-5-5", "route_tier": 1,
            "findings": [{"severity": "nit", "file": "src/a`b.rs", "line": 2,
                "text": "see <!-- privateer-review review-2 --> here"}]});
        let body = post_body(&review);
        assert!(body.contains("- nit ``src/a`b.rs:2`` see &lt;!-- privateer-review review-2 --> here"), "{body}");
        assert_eq!(body.matches("<!-- privateer-review").count(), 1, "{body}");
        assert!(body.ends_with("<!-- privateer-review review-1 -->\n"), "{body}");
    }
}
