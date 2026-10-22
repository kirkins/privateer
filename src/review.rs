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
//! reply carrying a stored credential value or a path outside the worktree
//! is refused and nothing of it is kept, so round B2 posts only filtered
//! records. The worktree is removed then.
//!
//! After settlement the runtime posts the review to the PR as one comment,
//! built from the record alone (`publish`), when the task's charter allows
//! `publish_review`. An errand the first mate starts with a review purpose
//! (`errand.start` with `"review": {"pr", "head_sha"}`) records into the same
//! table, so `review.list` holds every review and the comment is its copy.
//!
//! `pr.merge` checks the live PR (open, into the protected branch, green at
//! its head), merges it at exactly that head and records which tier-1
//! approval covered it, the approvals that do not cover, any changes
//! requested at or after it, which commits past it no review covered, and
//! who wrote those: the crew record, else their `Co-authored-by` trailers.
//! A review is no gate: a merge without one proceeds and its record says so.
//!
//! Only a first mate reviews or merges: crew are refused by the crew role's
//! default deny, and an errand is refused here. Under one OS user this is a
//! workflow control, not a security boundary.

pub mod authors;
mod filter;
mod pr;
mod prompt;
mod publish;

use crate::store::{Caller, Store, required};
use anyhow::{Context, Result, bail, ensure};
use filter::Finding;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Characters of the ship's notes a review prompt carries.
const NOTES_CHARS: usize = 2_000;
/// Attempts at one head that may end with no verdict.
const NO_VERDICT_ATTEMPTS: i64 = 2;
/// The review of one PR at which the captain hears of it.
const CAPTAIN_NOTICE_AT: i64 = 4;
/// Reviews shown in a ship's bearings.
const BEARINGS_REVIEWS: i64 = 5;

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
         CREATE INDEX IF NOT EXISTS review_ship ON reviews(ship, created_ms);
         CREATE TABLE IF NOT EXISTS merges(id TEXT PRIMARY KEY, ship TEXT NOT NULL,
           task_id TEXT NOT NULL, repo TEXT NOT NULL, pr INTEGER NOT NULL,
           head_sha TEXT NOT NULL, record TEXT NOT NULL, created_ms INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS merge_pr ON merges(repo, pr, created_ms);",
    )?;
    // Checked and added under the write lock so concurrent opens cannot both
    // add it. A review is `gate` (review.start) or `errand` (errand.start).
    let tx = store.write_transaction()?;
    let has_kind: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('reviews') WHERE name='kind')",
        [],
        |r| r.get(0),
    )?;
    if !has_kind {
        tx.execute_batch("ALTER TABLE reviews ADD COLUMN kind TEXT NOT NULL DEFAULT 'gate';")?;
    }
    tx.commit()?;
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

/// The task, its charter and its clone, for a ship that owns the task and,
/// with `action`, may take it on the task's work; without one, under any
/// open charter of the task.
fn task_clone(
    store: &Store,
    ship: &str,
    args: &Value,
    action: Option<&str>,
) -> Result<(String, PathBuf, pr::Clone)> {
    let task_id = required(args, "task_id")?.to_owned();
    let charter = match action {
        Some(action) => store.authorize(&task_id, action)?,
        None => store.active_charter(&task_id)?,
    };
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
    Ok((task_id, charter_repo, clone))
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
    let same_tree: Option<String> = tx
        .query_row(
            "SELECT head_sha FROM reviews WHERE repo=?1 AND pr=?2 AND head_tree=?3
               AND verdict='changes_requested' ORDER BY created_ms DESC LIMIT 1",
            params![repo, number, tree],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(earlier) = same_tree {
        bail!(
            "#{number} at {} has the same file tree as {}, which got changes requested; \
             change the files, not only the commits",
            short(head),
            short(&earlier)
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
    let (task_id, charter_repo, clone) = task_clone(store, &ship, args, Some("merge_green"))?;
    let live = pr::view(&clone, number)?;
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
               post_state, created_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,
               ?23,?24,'running','none',?25)",
            params![
                id,
                ship,
                task_id,
                clone.forge,
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
                Store::now_ms()
            ],
        )?;
        if before + 1 == CAPTAIN_NOTICE_AT {
            crate::mail::record_runtime(
                store,
                &json!({"id": format!("{id}-captain"), "from": ship, "to": crate::mail::CAPTAIN,
                    "kind": "notice", "task_id": task_id, "urgency": "normal", "payload": {"review": id},
                    "body": format!("{ship} has started review {CAPTAIN_NOTICE_AT} of #{number} in {}, \
                        now at {}. Repeated reviews of one PR may mean it is being reviewed until one \
                        approves; review.list shows them.", clone.repo, short(&live.head_sha))}),
            )?;
        }
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
            "SELECT folder FROM reviews WHERE errand_id=?1 AND verdict='running' AND kind='gate'",
            [errand],
            |r| r.get(0),
        )
        .optional()?;
    Ok(folder.map(|f| PathBuf::from(f).join("tree")))
}

/// Whether a job is a `review.start` review's errand, which runs read only
/// on a prompt the runtime built whole. An errand started with a review
/// purpose stays an ordinary errand of its ship.
pub(crate) fn is_review_errand(store: &Store, errand: &str) -> Result<bool> {
    Ok(store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM reviews WHERE errand_id=?1 AND kind='gate')",
        [errand],
        |r| r.get(0),
    )?)
}

/// A review's verdict notice to its ship, and the payload beside it.
pub(crate) struct Settled {
    pub body: String,
    pub payload: Value,
    pub urgency: &'static str,
    /// An errand started with a review purpose: its reply and session are
    /// kept as any errand's, with the review's notice beside them.
    pub errand: bool,
}

/// What an errand that reviews a PR is asked to end with, so its verdict and
/// findings are read into its review record.
fn errand_ask(id: &str, repo: &str, number: u64, head: &str) -> String {
    format!(
        "This errand is review {id} of pull request #{number} in {repo} at head {head}: review \
         exactly that commit. Put each finding on a line of its own as `FINDING <must-fix|should-fix|nit> \
         <path>:<line> <text>`, the path relative to the repository's root, about the code only. End \
         your reply with one last line, exactly `VERDICT: APPROVE` or `VERDICT: CHANGES_REQUESTED`; \
         an approval with a must-fix finding counts as no verdict. The runtime records the verdict \
         and the findings and posts them to the pull request; nothing else in your reply is posted."
    )
}

/// A review an errand runs, recorded before the errand starts: its id and
/// the ask to put after the errand's brief.
pub(crate) struct ErrandReview {
    pub id: String,
    pub ask: String,
}

/// `errand.start` with `"review": {"pr", "head_sha", "repo"?}`: record the
/// review the errand runs on its route. The head is the first mate's word,
/// checked against the forge: a commit in the clone (fetched from origin
/// when missing) that is the PR's head or one of its commits. The review
/// covers exactly that sha. Recording grants no work, so it needs only the
/// task's open charter; posting the comment needs `publish_review`, and
/// without it the review is recorded and its comment withheld.
pub(crate) fn record_errand(
    store: &Store,
    ship: &str,
    task_id: &str,
    errand: &str,
    review: &Value,
    route: &Value,
    fresh: bool,
) -> Result<ErrandReview> {
    if let Some(job) = crate::crew_scope::job_id()
        && crate::errand::is_errand(store, &job)?
    {
        bail!("a review errand is the first mate's own; errand {job} cannot review or merge");
    }
    let number = pr_number(review)?;
    let head = review["head_sha"]
        .as_str()
        .map(str::to_ascii_lowercase)
        .filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .context("review.head_sha must be the 40-character sha of the head the errand reviews")?;
    let args = json!({"task_id": task_id, "repo": review.get("repo")});
    let (task_id, _, clone) = task_clone(store, ship, &args, None)?;
    let live = pr::view(&clone, number)?;
    pr::fetch(&clone, number, &[&live.base_sha, &live.head_sha])?;
    pr::fetch(&clone, number, &[&head]).with_context(|| {
        format!(
            "review.head_sha {head} is not a commit in the clone of {} or its origin",
            clone.repo
        )
    })?;
    let ancestor =
        |a: &str, b: &str| pr::git(&clone.dir, &["merge-base", "--is-ancestor", a, b]).is_ok();
    ensure!(
        head == live.head_sha
            || (ancestor(&head, &live.head_sha) && !ancestor(&head, &live.base_sha)),
        "review.head_sha {} is neither #{number}'s head ({}) nor one of its commits",
        short(&head),
        short(&live.head_sha)
    );
    let tree = pr::tree_of(&clone.dir, &head)?;
    let identity = route["identity"].as_str().unwrap_or_default();
    let model = crate::routes::model_key(identity).unwrap_or_default();
    let id = format!("review-{}", uuid::Uuid::new_v4());
    store.connection().execute(
        "INSERT INTO reviews(id, ship, task_id, forge, repo, pr, base_ref, base_sha, head_ref,
           head_sha, head_tree, errand_id, route_name, route_identity, route_model, route_tier,
           route_uses, route_why, fresh, authors, declared, folder, clone, verdict, post_state,
           created_ms, kind)
         VALUES(?1,?2,?3,?4,?5,?6,?19,?20,?21,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,'{}','[]','',
           ?17,'running','none',?18,'errand')",
        params![
            id,
            ship,
            task_id,
            clone.forge,
            clone.repo,
            number,
            head,
            tree,
            errand,
            route["name"].as_str().unwrap_or_default(),
            identity,
            model,
            route["tier"].as_i64(),
            route["uses"].to_string(),
            route["why"].as_str().unwrap_or_default(),
            fresh,
            clone.dir.to_string_lossy(),
            Store::now_ms(),
            live.base,
            live.base_sha,
            live.head_ref
        ],
    )?;
    Ok(ErrandReview {
        ask: errand_ask(&id, &clone.repo, number, &head),
        id,
    })
}

/// Drop the record of an errand's review whose errand never ran.
pub(crate) fn forget_errand(store: &Store, errand: &str) -> Result<()> {
    store.connection().execute(
        "DELETE FROM reviews WHERE errand_id=?1 AND kind='errand' AND verdict='running'",
        [errand],
    )?;
    Ok(())
}

/// Post a finished review's comment, once.
fn publish(store: &Store, id: &str) -> Result<()> {
    let review = record(store, id)?;
    if review["post_state"] != "pending" {
        return Ok(());
    }
    let task = review["task_id"].as_str().unwrap_or_default();
    let (state, comment, error) = match publish::allowed(store, task) {
        Err(why) => ("withheld", None, Some(why)),
        Ok(()) => {
            let redact = publish::Redact::of(store, review["repo"].as_str().unwrap_or_default());
            let resolved = if review["verdict"] == "approve" {
                publish::resolved(store, &review).unwrap_or_default()
            } else {
                vec![]
            };
            let body = publish::review_body(&review, &resolved, &redact);
            let clone = pr::Clone {
                dir: PathBuf::from(review["clone"].as_str().unwrap_or_default()),
                forge: review["forge"].as_str().unwrap_or_default().to_owned(),
                repo: review["repo"].as_str().unwrap_or_default().to_owned(),
            };
            let number = review["pr"].as_u64().unwrap_or_default();
            match pr::comment(&clone, number, &publish::marker(id), &body) {
                Ok(comment) => ("posted", Some(comment), None),
                Err(error) => ("failed", None, Some(format!("{error:#}"))),
            }
        }
    };
    store.connection().execute(
        "UPDATE reviews SET post_state=?2, forge_review_id=?3, post_error=?4,
           posted_ms=CASE WHEN ?2='posted' THEN ?5 END WHERE id=?1",
        params![id, state, comment, error, Store::now_ms()],
    )?;
    Ok(())
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
    let row: Option<(String, String, String, String, String)> = store
        .connection()
        .query_row(
            "SELECT id, verdict, folder, clone, kind FROM reviews WHERE errand_id=?1",
            [errand],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((id, verdict, folder, clone, kind)) = row else {
        return Ok(None);
    };
    let from_errand = kind == "errand";
    let folder = PathBuf::from(folder);
    if verdict == "running" {
        // An errand's review read the ship's folder: its findings are about
        // the PR's clone.
        let tree = if from_errand {
            PathBuf::from(&clone)
        } else {
            folder.join("tree")
        };
        let scan = if from_errand {
            filter::findings_lenient
        } else {
            filter::findings
        };
        let (verdict, findings, refusal) = match (outcome, reply) {
            ("succeeded", Some(reply)) => match crate::credentials::stored_values() {
                Err(error) => (
                    "failed",
                    vec![],
                    Some(format!(
                        "refused: the stored credentials could not be read to filter the reply ({error:#})"
                    )),
                ),
                Ok(secrets) => match scan(reply, &tree, &secrets) {
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
        let recorded = store.connection().execute(
            "UPDATE reviews SET verdict=?2, findings=?3, refusal=?4, post_state='pending',
               finished_ms=?5 WHERE id=?1 AND verdict='running'",
            params![
                id,
                verdict,
                serde_json::to_string(&findings)?,
                refusal,
                Store::now_ms()
            ],
        )?;
        if recorded == 1 {
            publish(store, &id)?;
        }
    }
    if !from_errand {
        pr::remove(Some(Path::new(&clone)), &folder);
    }
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
        errand: from_errand,
    }))
}

/// What became of a finished review's comment.
fn posting(review: &Value) -> String {
    let error = review["post_error"].as_str().unwrap_or_default();
    match review["post_state"].as_str() {
        Some("posted") => "posted to the pull request as a comment".to_owned(),
        Some("withheld") => format!("not posted: {error}"),
        Some("failed") => format!("not posted, the forge refused it: {error}"),
        _ => "not posted yet".to_owned(),
    }
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
            posting(review)
        ),
        "none" => format!(
            "Review of #{number} at {}: no verdict, because {}; {}.\n",
            short(&head),
            text("refusal"),
            posting(review)
        ),
        _ => format!(
            "Review of #{number} at {} failed: {}; {}.\n",
            short(&head),
            text("refusal"),
            posting(review)
        ),
    };
    body.push_str(&format!(
        "Reviewer: {} ({}, tier {}), in a {} session{}.\n",
        text("route_name"),
        text("route_identity"),
        review["route_tier"],
        if review["fresh"] == true {
            "fresh"
        } else {
            "resumed"
        },
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

const COLUMNS: &str = "id, ship, task_id, forge, repo, pr, base_ref, base_sha, head_ref, head_sha,
    errand_id, route_name, route_identity, route_model, route_tier, route_uses, route_why, overlap,
    fresh, authors, declared, verdict, findings, refusal, post_state, created_ms, finished_ms,
    cleaned_ms, clone, kind, forge_review_id, post_error, posted_ms, merged_ms, merge_sha";

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
        "cleaned_ms": r.get::<_, Option<i64>>(27)?, "clone": r.get::<_, String>(28)?,
        "kind": r.get::<_, String>(29)?, "forge_comment": r.get::<_, Option<String>>(30)?,
        "post_error": r.get::<_, Option<String>>(31)?, "posted_ms": r.get::<_, Option<i64>>(32)?,
        "merged_ms": r.get::<_, Option<i64>>(33)?, "merge_sha": r.get::<_, Option<String>>(34)?,
    }))
}

fn record(store: &Store, id: &str) -> Result<Value> {
    Ok(store.connection().query_row(
        &format!("SELECT {COLUMNS} FROM reviews WHERE id=?1"),
        [id],
        row,
    )?)
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
    let mut statement = store.connection().prepare(
        "SELECT record FROM merges WHERE (?1 IS NULL OR ship=?1) AND (?2 IS NULL OR pr=?2)
           AND (?3 IS NULL OR repo=?3) ORDER BY created_ms DESC LIMIT 50",
    )?;
    let merges = statement
        .query_map(params![ship, number, repo], |r| r.get::<_, String>(0))?
        .map(|raw| Ok(serde_json::from_str::<Value>(&raw?)?))
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({"reviews": reviews, "merges": merges}))
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
                "approve" => format!(
                    "#{number} {head} APPROVE {by}, comment {}{declared}",
                    text("post_state")
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

/// The model a `Co-authored-by` trailer names, spelled as a model key:
/// `Claude Opus 5.5 <noreply@anthropic.com>` is `claude-opus-5-5`.
fn trailer_model(value: &str) -> Option<String> {
    let name = value.split('<').next()?.trim().to_ascii_lowercase();
    let spelled: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let model = spelled
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    (!model.is_empty()).then_some(model)
}

/// The tier-1 review routes' models in the captain's routes file: the
/// models whose review, or whose writing, the merge rule accepts.
fn top_tier_models(policy: Option<&Value>) -> BTreeSet<String> {
    let require = crate::crew_routes::Require {
        r#use: Some("review".to_owned()),
        tier: Some(1),
    };
    policy
        .and_then(|p| p["routes"].as_object())
        .map(|routes| {
            routes
                .values()
                .filter(|route| crate::crew_routes::meets(route, Some(&require)))
                .filter_map(|route| {
                    crate::crew_routes::identity(route)
                        .as_deref()
                        .and_then(crate::routes::model_key)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A review as a merge record names it.
fn review_brief(review: &Value) -> Value {
    json!({"id": review["id"], "head_sha": review["head_sha"], "verdict": review["verdict"],
        "kind": review["kind"], "route_name": review["route_name"],
        "route_model": review["route_model"], "route_tier": review["route_tier"]})
}

/// What a merge at the live head carries, and the coverage that makes.
///
/// Only an APPROVE by a route that is a tier-1 review route in the routes
/// file now covers the head or the commit it approved; any other approval
/// of a carried commit is recorded as not covering, with why. A
/// CHANGES_REQUESTED at the merged head, or at a commit after the covering
/// approval, is recorded and makes the coverage `changes_requested`.
///
/// The commits past the covering approval (all of the PR's without one) are
/// recorded with the models that wrote them. The runtime's own crew record
/// (the models of the crew jobs on the PR's task tree, `authors`) wins over
/// a `Co-authored-by` trailer, which crews add whatever route they run on:
/// a trailer is read only when no crew record exists, and a trailer naming
/// a model the crew record does not is recorded as a disagreement, never
/// counted as written by a tier-1 model.
fn coverage(
    store: &Store,
    ship: &str,
    clone: &pr::Clone,
    number: u64,
    live: &pr::Pr,
    notes: &mut Vec<String>,
) -> Result<Value> {
    let head = live.head_sha.as_str();
    let ancestor =
        |a: &str, b: &str| pr::git(&clone.dir, &["merge-base", "--is-ancestor", a, b]).is_ok();
    let carried = |sha: &str| sha == head || ancestor(sha, head);
    let mut statement = store.connection().prepare(&format!(
        "SELECT {COLUMNS} FROM reviews WHERE repo=?1 AND pr=?2
           AND verdict IN ('approve','changes_requested') ORDER BY finished_ms DESC"
    ))?;
    let finished = statement
        .query_map(params![clone.repo, number], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let policy = crate::crew_routes::for_store(store)?;
    let top = top_tier_models(policy.as_ref());
    let require = crate::crew_routes::Require {
        r#use: Some("review".to_owned()),
        tier: Some(1),
    };
    let mut covering: Option<&Value> = None;
    let mut not_covering = vec![];
    for review in finished.iter().filter(|r| r["verdict"] == "approve") {
        if !carried(review["head_sha"].as_str().unwrap_or_default()) {
            continue;
        }
        let name = review["route_name"].as_str().unwrap_or_default();
        let route = policy
            .as_ref()
            .map(|p| &p["routes"][name])
            .filter(|r| !name.is_empty() && r.is_object());
        let why = match route {
            None if name.is_empty() => {
                Some("it ran on the first mate's own route, which is no review route".to_owned())
            }
            None => Some(format!("route {name} is not in the routes file now")),
            Some(route) if !crate::crew_routes::meets(route, Some(&require)) => Some(format!(
                "route {name} does not meet {} now: it is {}",
                require.summary(),
                crate::crew_routes::describe_strength(route)
            )),
            Some(_) => None,
        };
        match why {
            None => {
                covering.get_or_insert(review);
            }
            Some(why) => {
                let mut brief = review_brief(review);
                brief["why"] = json!(why);
                not_covering.push(brief);
            }
        }
    }
    let changes: Vec<Value> = finished
        .iter()
        .filter(|r| r["verdict"] == "changes_requested")
        .filter(|r| {
            let sha = r["head_sha"].as_str().unwrap_or_default();
            sha == head
                || covering.is_some_and(|c| {
                    let at = c["head_sha"].as_str().unwrap_or_default();
                    carried(sha)
                        && if sha == at {
                            r["finished_ms"].as_i64() > c["finished_ms"].as_i64()
                        } else {
                            ancestor(at, sha)
                        }
                })
        })
        .map(review_brief)
        .collect();
    let from = covering
        .and_then(|r| r["head_sha"].as_str())
        .unwrap_or(&live.base_sha);
    let commits = pr::git(
        &clone.dir,
        &["rev-list", "--reverse", &format!("{from}..{head}")],
    )?;
    let authors = pr::merge_base_ms(&clone.dir, &live.base_sha, head)
        .and_then(|at| authors::compute(store, ship, &live.head_ref, at, &[]));
    let mut crew: BTreeSet<String> = BTreeSet::new();
    match &authors {
        Ok(authors) => {
            crew.extend(authors.crew_jobs.keys().cloned());
            if !authors.unknown_jobs.is_empty() {
                // A crew job whose model cannot be named leaves the record
                // unable to say all its commits were written on tier 1.
                crew.insert("unknown".to_owned());
                notes.push(format!(
                    "the model of crew job(s) {} cannot be named",
                    authors.unknown_jobs.join(", ")
                ));
            }
        }
        Err(error) => notes.push(format!("the crew record could not be read: {error:#}")),
    }
    let unreviewed: Vec<Value> = commits
        .lines()
        .map(|sha| {
            let trailers = pr::git(
                &clone.dir,
                &[
                    "log",
                    "-1",
                    "--format=%(trailers:key=Co-authored-by,valueonly,separator=%x1f)",
                    sha,
                ],
            )
            .unwrap_or_default();
            let named: Vec<String> = trailers.split('\x1f').filter_map(trailer_model).collect();
            let (written, from, disagrees) = if !crew.is_empty() {
                let disagrees = named.iter().any(|m| !crew.contains(m));
                (
                    crew.iter().cloned().collect(),
                    json!("crew record"),
                    disagrees,
                )
            } else if !named.is_empty() {
                (named.clone(), json!("commit trailer"), false)
            } else {
                (vec![], Value::Null, false)
            };
            json!({"sha": sha, "written_by": written, "from": from, "trailer": named,
                "disagrees": disagrees})
        })
        .collect();
    let disagreeing: Vec<&str> = unreviewed
        .iter()
        .filter(|c| c["disagrees"] == true)
        .filter_map(|c| c["sha"].as_str())
        .collect();
    if !disagreeing.is_empty() {
        notes.push(format!(
            "the Co-authored-by trailer of {} names a model the crew record does not ({}); the crew record is taken",
            disagreeing.iter().map(|s| short(s)).collect::<Vec<_>>().join(", "),
            crew.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    let coverage = if !changes.is_empty() {
        "changes_requested"
    } else if unreviewed.is_empty() {
        "reviewed"
    } else if unreviewed.iter().all(|c| {
        c["disagrees"] == false
            && c["written_by"].as_array().is_some_and(|w| {
                !w.is_empty()
                    && w.iter()
                        .all(|m| m.as_str().is_some_and(|m| top.contains(m)))
            })
    }) {
        "written_by_top_tier"
    } else {
        "unreviewed"
    };
    let review = match covering {
        None => Value::Null,
        Some(review) => {
            let id = review["id"].as_str().unwrap_or_default();
            let name = review["route_name"].as_str().unwrap_or_default();
            if let (Ok(authors), Some(policy)) = (&authors, &policy) {
                let order =
                    crate::crew_routes::order_reviewers(&[name.to_owned()], policy, authors);
                if let Some((_, why)) = order.excluded.first() {
                    notes.push(format!(
                        "review {id}'s route {name} is now excluded by the code's authors: {why}"
                    ));
                }
            }
            review_brief(review)
        }
    };
    Ok(
        json!({"review": review, "coverage": coverage, "unreviewed": unreviewed,
        "approvals_not_covering": not_covering, "changes_requested": changes}),
    )
}

/// `pr.merge {"task_id", "pr", "repo"?, "deploys_production"?, "environment"?}`:
/// merge the live PR at its head once it is open into the protected branch
/// and green, and record which review covered the merged head. A review is
/// not required: a merge without one is recorded as unreviewed.
fn merge(store: &mut Store, args: &Value) -> Result<Value> {
    let ship = first_mate(store, "pr.merge")?;
    let number = pr_number(args)?;
    let (task_id, _, clone) = task_clone(store, &ship, args, Some("merge_green"))?;
    // 1. The live PR, read from the forge now.
    let live = pr::view(&clone, number)?;
    let head = live.head_sha.clone();
    ensure!(
        live.state == "open",
        "#{number} in {} is {}, not open",
        clone.repo,
        live.state
    );
    let branch = pr::default_branch(&clone.dir)?;
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
    // 2. What the head carries, recorded whatever it is.
    pr::fetch(&clone, number, &[&live.base_sha, &head])?;
    let mut notes = vec![];
    let covered = coverage(store, &ship, &clone, number, &live, &mut notes)?;
    // 3. Production: a merge that deploys needs an approval of this exact
    // artifact and environment, used up in the one write transaction that
    // finds it, so two merges at once cannot both use it. It stays used if
    // the merge then fails: the captain approves again.
    let approval = if args.get("deploys_production") == Some(&json!(true)) {
        let environment = required(args, "environment")?;
        let tx = store.write_transaction()?;
        let charter = store.authorize(&task_id, "merge_green")?;
        let approval: Option<String> = tx
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
        let used = tx.execute(
            "UPDATE approvals SET consumed=1 WHERE id=?1 AND consumed=0",
            [&approval],
        )?;
        ensure!(used == 1, "approval {approval} was used by another merge");
        tx.commit()?;
        store.audit(
            "approval.consumed_by_delivery",
            &json!({"id": approval, "task_id": task_id, "artifact": head, "environment": environment}),
        )?;
        Some(approval)
    } else {
        None
    };
    // 4. The merge at exactly that head: a push since step 1 makes the
    // forge refuse it.
    let merge_sha = pr::merge(&clone, number, &head)?;
    let now = Store::now_ms();
    let id = format!("merge-{}", uuid::Uuid::new_v4());
    let mut record = json!({"id": id, "ship": ship, "task_id": task_id, "repo": clone.repo,
        "pr": number, "head_sha": head, "merge_sha": merge_sha, "review": covered["review"],
        "coverage": covered["coverage"], "unreviewed": covered["unreviewed"],
        "approvals_not_covering": covered["approvals_not_covering"],
        "changes_requested": covered["changes_requested"], "notes": notes, "created_ms": now});
    if let Some(approval) = &approval {
        record["production_approval"] = json!(approval);
    }
    if let Some(review) = record["review"]["id"].as_str() {
        store.connection().execute(
            "UPDATE reviews SET merged_ms=?2, merge_sha=?3 WHERE id=?1",
            params![review, now, merge_sha],
        )?;
    }
    let comment = match publish::allowed(store, &task_id) {
        Err(why) => json!({"state": "withheld", "error": why}),
        Ok(()) => {
            let body = publish::merge_body(&id, &record, &publish::Redact::of(store, &clone.repo));
            match pr::comment(&clone, number, &publish::marker(&id), &body) {
                Ok(posted) => json!({"state": "posted", "id": posted}),
                Err(error) => json!({"state": "failed", "error": format!("{error:#}")}),
            }
        }
    };
    record["comment"] = comment;
    store.connection().execute(
        "INSERT INTO merges(id, ship, task_id, repo, pr, head_sha, record, created_ms)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            id,
            ship,
            task_id,
            clone.repo,
            number,
            head,
            record.to_string(),
            now
        ],
    )?;
    store.audit(
        "pr.merge",
        &json!({"merge": id, "ship": ship, "repo": clone.repo, "pr": number, "head": head,
            "review": record["review"]["id"], "coverage": record["coverage"]}),
    )?;
    let mut reply = record;
    reply["merged"] = json!(true);
    Ok(reply)
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "review.start" => start(store, &args),
        "review.list" => list(store, &args),
        "pr.merge" => merge(store, &args),
        _ => bail!("unknown review command {command}"),
    }
}
