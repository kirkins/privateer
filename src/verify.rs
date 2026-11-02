//! Crew close their own loop (docs/design/crew-verification.md).
//!
//! A first mate states, as data, how a crew task proves it is done:
//! `verify.set` records a list of checks (argv, optional environment, heavy
//! or light), taken inline or from the repository's `.privateer-verify.json`
//! as it stands at the charter repository's HEAD. A crew cannot change them:
//! the crew role refuses `verify.set` like every command it does not list.
//!
//! `verify.run` lets a crew run those checks at its committed head. It is
//! refused for a dirty tree, while another run of the task is unfinished, and
//! for a check that already failed `ATTEMPTS` times under the current set.
//! One supervised `test` job runs the runtime's own runner (`_verify-run`),
//! which runs each check in turn in the crew's workspace and records its
//! result. The runner refuses to run unless its parent is the job's recorded
//! supervisor, so a crew cannot run it by hand. A crew parks on the job and
//! its resume reads the results.
//!
//! A crew's outcome then reports each check at the crew's final head, from
//! these records and the job table alone: passed, failed, interrupted,
//! running, stale (it ran only at an earlier head) or not run. A crew that
//! exited cleanly with checks set but not all passed at its head is reported
//! with high urgency. Under one OS account this is a workflow control, not a
//! security boundary.
use crate::store::{Caller, Store, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Checks one task may set.
const MAX_CHECKS: usize = 16;
const MAX_ARGS: usize = 64;
const MAX_ARG_BYTES: usize = 4_096;
const MAX_ENV: usize = 32;
const NAME_CHARS: usize = 64;
const REASON_CHARS: usize = 300;
/// Failed runs of one check under one check set before the crew must ask
/// its first mate instead.
pub(crate) const ATTEMPTS: i64 = 3;
const MANIFEST: &str = ".privateer-verify.json";
const MANIFEST_BYTES: usize = 64 * 1024;
/// Job statuses after which a run's runner no longer writes.
const FINISHED: &str = "('succeeded','failed','cancelled','timed_out')";

pub fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS verify_checks(
            task_id TEXT PRIMARY KEY,
            revision INTEGER NOT NULL,
            checks TEXT NOT NULL,
            none_reason TEXT,
            set_by TEXT NOT NULL,
            updated_ms INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS verify_runs(
            id TEXT PRIMARY KEY,
            task_id TEXT NOT NULL,
            revision INTEGER NOT NULL,
            crew_job_id TEXT NOT NULL,
            head TEXT NOT NULL,
            cwd TEXT NOT NULL,
            checks TEXT NOT NULL,
            created_ms INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS verify_runs_task ON verify_runs(task_id, created_ms);
         CREATE TABLE IF NOT EXISTS verify_results(
            run_id TEXT NOT NULL,
            position INTEGER NOT NULL,
            name TEXT NOT NULL,
            status TEXT NOT NULL,
            exit_code INTEGER,
            started_ms INTEGER,
            finished_ms INTEGER,
            PRIMARY KEY(run_id, position));",
    )?;
    Ok(())
}

fn clip(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        text.to_owned()
    } else {
        format!("{}…", text.chars().take(chars).collect::<String>())
    }
}

fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .output()
        .context("running git")?;
    ensure!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The commit checked out at `cwd`, if git can read it.
fn head(cwd: &Path) -> Option<String> {
    git(cwd, &["rev-parse", "HEAD"])
        .ok()
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// The repository's check manifest at HEAD, never the working tree: a branch
/// cannot change what a check named from it runs.
fn manifest(repo: &Path) -> Result<Value> {
    let text = git(repo, &["show", &format!("HEAD:{MANIFEST}")])
        .with_context(|| format!("no {MANIFEST} at HEAD of {}", repo.display()))?;
    ensure!(text.len() <= MANIFEST_BYTES, "{MANIFEST} is over 64 KiB");
    let value: Value =
        serde_json::from_str(&text).with_context(|| format!("{MANIFEST} is not JSON"))?;
    ensure!(
        value["checks"].is_object(),
        "{MANIFEST} needs a \"checks\" object"
    );
    Ok(value)
}

/// One check, validated: `{name, command, env, heavy}`.
fn check(entry: &Value, manifest: &mut dyn FnMut() -> Result<Value>) -> Result<Value> {
    let (name, source) = match entry.get("use") {
        Some(used) => {
            let used = used.as_str().context("use names a manifest check")?;
            let manifest = manifest()?;
            let found = manifest["checks"]
                .get(used)
                .filter(|c| c.is_object())
                .with_context(|| format!("{MANIFEST} has no check {used}"))?
                .clone();
            let name = entry
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(used)
                .to_owned();
            (name, found)
        }
        None => (
            required(entry, "name")
                .context("each check needs a name")?
                .to_owned(),
            entry.clone(),
        ),
    };
    ensure!(
        !name.is_empty()
            && name.chars().count() <= NAME_CHARS
            && name.chars().all(|c| !c.is_control()),
        "a check name is 1 to {NAME_CHARS} printable characters"
    );
    let command: Vec<String> = serde_json::from_value(
        source
            .get("command")
            .cloned()
            .with_context(|| format!("check {name} needs a command array"))?,
    )
    .with_context(|| format!("check {name}: command is an array of strings"))?;
    ensure!(
        !command.is_empty() && !command[0].is_empty(),
        "check {name}: command must name an executable"
    );
    ensure!(
        command.len() <= MAX_ARGS
            && command
                .iter()
                .all(|a| a.len() <= MAX_ARG_BYTES && !a.contains('\0')),
        "check {name}: at most {MAX_ARGS} arguments of {MAX_ARG_BYTES} bytes, without NUL"
    );
    let env = source.get("env").cloned().unwrap_or_else(|| json!({}));
    let env_map = env
        .as_object()
        .with_context(|| format!("check {name}: env is an object of strings"))?;
    ensure!(
        env_map.len() <= MAX_ENV,
        "check {name}: at most {MAX_ENV} env entries"
    );
    for (key, value) in env_map {
        ensure!(
            !key.is_empty()
                && !key.contains('=')
                && !key.contains('\0')
                && !key.starts_with("PRIVATEER_"),
            "check {name}: env name {key} is not allowed"
        );
        ensure!(
            value.as_str().is_some_and(|v| !v.contains('\0')),
            "check {name}: env {key} must be a string"
        );
    }
    let heavy = match source.get("heavy") {
        None => true,
        Some(v) => v.as_bool().context("heavy must be boolean")?,
    };
    Ok(json!({"name": name, "command": command, "env": env, "heavy": heavy}))
}

/// `verify.set`: the task's owner (or the captain) states how its crew prove
/// the work is done. Each set starts a new revision, with fresh attempts.
fn set(store: &Store, args: &Value) -> Result<Value> {
    let task_id = required(args, "task_id")?;
    let task = store.task(task_id)?;
    let by = match store.caller() {
        Caller::Agent { actor } => {
            ensure!(
                task["owner"] == actor.as_str(),
                "agent {actor} can only set checks on tasks it owns"
            );
            actor.clone()
        }
        Caller::Operator => "operator".to_owned(),
    };
    let charter = store.active_charter(task_id)?;
    let repo = std::path::PathBuf::from(
        charter["repo"]
            .as_str()
            .context("charter repository missing")?,
    );
    let mut loaded: Option<Value> = None;
    let mut load_manifest = || -> Result<Value> {
        if loaded.is_none() {
            loaded = Some(manifest(&repo)?);
        }
        Ok(loaded.clone().unwrap_or_default())
    };
    let (checks, none_reason) = match (args.get("checks"), args.get("none")) {
        (Some(_), Some(_)) => bail!("give checks or none, not both"),
        (None, None) => bail!(
            "give checks (a list, or \"default\" for {MANIFEST}) or none with the reason no checks apply"
        ),
        (None, Some(reason)) => {
            let reason = reason
                .as_str()
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .context("none is the reason no checks apply")?;
            (vec![], Some(clip(reason, REASON_CHARS)))
        }
        (Some(Value::String(word)), None) => {
            ensure!(
                word == "default",
                "checks is a list, or \"default\" for {MANIFEST}"
            );
            let manifest = load_manifest()?;
            let names: Vec<String> = serde_json::from_value(
                manifest
                    .get("default")
                    .cloned()
                    .with_context(|| format!("{MANIFEST} has no \"default\" list"))?,
            )
            .with_context(|| format!("{MANIFEST}: default is a list of check names"))?;
            let checks = names
                .iter()
                .map(|n| check(&json!({"use": n}), &mut load_manifest))
                .collect::<Result<Vec<_>>>()?;
            (checks, None)
        }
        (Some(Value::Array(entries)), None) => (
            entries
                .iter()
                .map(|e| check(e, &mut load_manifest))
                .collect::<Result<Vec<_>>>()?,
            None,
        ),
        (Some(_), None) => bail!("checks is a list, or \"default\" for {MANIFEST}"),
    };
    ensure!(
        none_reason.is_some() || !checks.is_empty(),
        "checks is empty; give none with the reason instead"
    );
    ensure!(
        checks.len() <= MAX_CHECKS,
        "at most {MAX_CHECKS} checks per task"
    );
    let mut names: Vec<&str> = checks.iter().filter_map(|c| c["name"].as_str()).collect();
    names.sort_unstable();
    ensure!(
        names.windows(2).all(|w| w[0] != w[1]),
        "check names must be unique"
    );
    let conn = store.connection();
    let revision: i64 = conn
        .query_row(
            "SELECT revision FROM verify_checks WHERE task_id=?1",
            [task_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0)
        + 1;
    conn.execute(
        "INSERT INTO verify_checks(task_id, revision, checks, none_reason, set_by, updated_ms)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(task_id) DO UPDATE SET revision=excluded.revision, checks=excluded.checks,
           none_reason=excluded.none_reason, set_by=excluded.set_by, updated_ms=excluded.updated_ms",
        params![
            task_id,
            revision,
            Value::Array(checks.clone()).to_string(),
            none_reason,
            by,
            Store::now_ms()
        ],
    )?;
    let names: Vec<&Value> = checks.iter().map(|c| &c["name"]).collect();
    store.audit(
        "verify.set",
        &json!({"task_id": task_id, "revision": revision, "checks": names, "none": none_reason, "by": by}),
    )?;
    Ok(json!({"task_id": task_id, "revision": revision, "checks": checks, "none": none_reason}))
}

struct Set {
    revision: i64,
    checks: Vec<Value>,
    none_reason: Option<String>,
}

fn load_set(store: &Store, task_id: &str) -> Result<Option<Set>> {
    let row: Option<(i64, String, Option<String>)> = store
        .connection()
        .query_row(
            "SELECT revision, checks, none_reason FROM verify_checks WHERE task_id=?1",
            [task_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    row.map(|(revision, checks, none_reason)| {
        Ok(Set {
            revision,
            checks: serde_json::from_str(&checks)?,
            none_reason,
        })
    })
    .transpose()
}

/// Failed results of one check under one revision of a task's checks.
fn failures(store: &Store, task_id: &str, revision: i64, name: &str) -> Result<i64> {
    Ok(store.connection().query_row(
        "SELECT COUNT(*) FROM verify_results x JOIN verify_runs r ON r.id=x.run_id
         WHERE r.task_id=?1 AND r.revision=?2 AND x.name=?3 AND x.status='failed'",
        params![task_id, revision, name],
        |r| r.get(0),
    )?)
}

/// `verify.run`: a crew runs its first mate's checks at its committed head.
fn run(store: &Store, args: &Value) -> Result<Value> {
    let crew = store
        .crew()
        .cloned()
        .context("verify.run is for crew: it runs the task's checks in the crew's workspace")?;
    let task_id = required(args, "task_id")?;
    ensure!(
        task_id == crew.task,
        "crew job {} works only on task {}",
        crew.crew,
        crew.task
    );
    let set = load_set(store, task_id)?.with_context(|| {
        format!(
            "no checks were set for task {task_id}; ask your first mate with crew.report kind decision"
        )
    })?;
    if let Some(reason) = &set.none_reason {
        bail!("your first mate set no checks for task {task_id}: {reason}");
    }
    let wanted: Option<Vec<String>> = args
        .get("names")
        .map(|n| serde_json::from_value(n.clone()).context("names is a list of check names"))
        .transpose()?;
    let checks: Vec<Value> = match &wanted {
        None => set.checks.clone(),
        Some(names) => {
            for name in names {
                ensure!(
                    set.checks.iter().any(|c| c["name"] == name.as_str()),
                    "task {task_id} has no check {name}"
                );
            }
            set.checks
                .iter()
                .filter(|c| names.iter().any(|n| c["name"] == n.as_str()))
                .cloned()
                .collect()
        }
    };
    ensure!(!checks.is_empty(), "names selects no checks");
    let unfinished: Option<String> = store
        .connection()
        .query_row(
            &format!(
                "SELECT r.id FROM verify_runs r JOIN jobs j ON j.id=r.id
                 WHERE r.task_id=?1 AND json_extract(j.record,'$.status') NOT IN {FINISHED}
                 LIMIT 1"
            ),
            [task_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(run) = unfinished {
        bail!("verification {run} of task {task_id} is still running; park on it or wait for it");
    }
    let mut spent = vec![];
    for c in &checks {
        let name = c["name"].as_str().unwrap_or_default();
        if failures(store, task_id, set.revision, name)? >= ATTEMPTS {
            spent.push(name.to_owned());
        }
    }
    if !spent.is_empty() {
        bail!(
            "check {} failed {ATTEMPTS} times for task {task_id}; report with crew.report kind decision: say what fails and what you need",
            spent.join(", ")
        );
    }
    let crew_job = crate::jobs::record(store, &crew.crew)?;
    let cwd = std::path::PathBuf::from(
        crew_job["cwd"]
            .as_str()
            .context("crew job has no directory")?,
    );
    let status = git(&cwd, &["status", "--porcelain"])
        .context("verification needs the crew's workspace to be a git checkout")?;
    ensure!(
        status.trim().is_empty(),
        "commit first: a verification covers one commit, and this workspace has uncommitted changes"
    );
    let head = head(&cwd).context("the workspace has no commit to verify")?;
    let conn = store.connection();
    let ordinal: i64 = conn.query_row(
        "SELECT COUNT(*) FROM verify_runs WHERE crew_job_id=?1",
        [&crew.crew],
        |r| r.get(0),
    )?;
    let id = format!("verify-{}-{}", crew.crew, ordinal + 1);
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let recorded = (|| -> Result<()> {
        conn.execute(
            "INSERT INTO verify_runs(id, task_id, revision, crew_job_id, head, cwd, checks, created_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                task_id,
                set.revision,
                crew.crew,
                head,
                cwd.to_string_lossy(),
                Value::Array(checks.clone()).to_string(),
                Store::now_ms()
            ],
        )?;
        for (position, c) in checks.iter().enumerate() {
            conn.execute(
                "INSERT INTO verify_results(run_id, position, name, status) VALUES(?1, ?2, ?3, 'pending')",
                params![id, position as i64, c["name"].as_str().unwrap_or_default()],
            )?;
        }
        Ok(())
    })();
    match recorded {
        Ok(()) => conn.execute_batch("COMMIT")?,
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(error);
        }
    }
    let executable = crate::executable::supervisor()?;
    let heavy = checks.iter().any(|c| c["heavy"] == true);
    let started = crate::jobs::start_job(
        store,
        json!({
            "id": id, "task_id": task_id, "action": "test", "heavy": heavy,
            "wait_for_capacity": true, "cwd": cwd,
            "command": [executable, "--home", store.home(), "_verify-run", id],
        }),
    );
    let started = match started {
        Ok(started) => started,
        Err(error) => {
            let _ = conn.execute("DELETE FROM verify_results WHERE run_id=?1", [&id]);
            let _ = conn.execute("DELETE FROM verify_runs WHERE id=?1", [&id]);
            return Err(error);
        }
    };
    let names: Vec<&Value> = checks.iter().map(|c| &c["name"]).collect();
    store.audit(
        "verify.run",
        &json!({"id": id, "task_id": task_id, "crew_job_id": crew.crew, "head": head, "checks": names}),
    )?;
    Ok(json!({
        "run_id": id, "job_id": id, "head": head, "checks": names, "job": started,
        "next": format!(
            "Park on it: crew.park {{\"job_id\":\"{id}\",\"next\":\"read the verification\"}} and end your turn. Its outcome lists each check."
        ),
    }))
}

/// The runtime's runner for one verification: `privateer --home H
/// _verify-run RUN`, the payload of the run's own job. Returns the exit code.
pub fn runner(store: &Store, id: &str) -> Result<i32> {
    let conn = store.connection();
    let (cwd, checks): (String, String) = conn
        .query_row(
            "SELECT cwd, checks FROM verify_runs WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .with_context(|| format!("unknown verification {id}"))?;
    let job = crate::jobs::record(store, id)?;
    ensure!(
        crate::jobs::is_recorded_supervisor(
            &job["supervisor"],
            std::os::unix::process::parent_id()
        )?,
        "verification {id} runs only as its own job's payload"
    );
    ensure!(
        matches!(job["status"].as_str(), Some("launching" | "running")),
        "verification {id} is not running"
    );
    let checks: Vec<Value> = serde_json::from_str(&checks)?;
    let total = checks.len();
    let mut passed = 0;
    let mut out = std::io::stdout();
    for (position, c) in checks.iter().enumerate() {
        let name = c["name"].as_str().unwrap_or_default();
        let fresh = conn.execute(
            "UPDATE verify_results SET status='running', started_ms=?3
             WHERE run_id=?1 AND position=?2 AND status='pending'",
            params![id, position as i64, Store::now_ms()],
        )?;
        ensure!(fresh == 1, "verification {id} already ran check {name}");
        writeln!(
            out,
            "== privateer verify: {name} ({}/{total}) ==",
            position + 1
        )?;
        out.flush()?;
        let argv: Vec<String> = serde_json::from_value(c["command"].clone())?;
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]).current_dir(&cwd).stdin(Stdio::null());
        if let Some(env) = c["env"].as_object() {
            for (key, value) in env {
                cmd.env(key, value.as_str().unwrap_or("").replace("{cwd}", &cwd));
            }
        }
        let (status, code) = match cmd.status() {
            Ok(s) if s.success() => ("passed", s.code()),
            Ok(s) => ("failed", s.code()),
            Err(error) => {
                eprintln!("privateer verify: {name} did not start: {error}");
                ("failed", None)
            }
        };
        conn.execute(
            "UPDATE verify_results SET status=?3, exit_code=?4, finished_ms=?5
             WHERE run_id=?1 AND position=?2",
            params![id, position as i64, status, code, Store::now_ms()],
        )?;
        if status == "passed" {
            passed += 1;
        }
        writeln!(
            out,
            "== privateer verify: {name} {status}{} ==",
            code.map(|c| format!(" (exit {c})")).unwrap_or_default()
        )?;
        out.flush()?;
    }
    writeln!(out, "== privateer verify: {passed} of {total} passed ==")?;
    Ok(if passed == total { 0 } else { 1 })
}

/// One check's result as recorded: its state given the run's job, its head,
/// its exit code and duration.
fn result_state(stored: &str, job_status: Option<&str>) -> &'static str {
    match stored {
        "passed" => "passed",
        "failed" => "failed",
        // Pending or running, but the run's job is over: it never finished.
        _ if job_status.is_none_or(|s| {
            matches!(
                s,
                "succeeded" | "failed" | "cancelled" | "timed_out" | "orphaned"
            )
        }) =>
        {
            "interrupted"
        }
        _ => "running",
    }
}

/// The task's checks at `head`: each check's state, and whether every one
/// passed there. None when no checks were ever set.
pub(crate) fn state(store: &Store, task_id: &str, head: Option<&str>) -> Result<Option<Value>> {
    let Some(set) = load_set(store, task_id)? else {
        return Ok(None);
    };
    if let Some(reason) = &set.none_reason {
        return Ok(Some(json!({
            "revision": set.revision, "none": reason, "head": head,
            "verified": false, "checks": [],
        })));
    }
    let mut rows = vec![];
    let mut all = true;
    for c in &set.checks {
        let name = c["name"].as_str().unwrap_or_default();
        let latest = |at_head: bool| -> Result<Option<Value>> {
            Ok(store
                .connection()
                .query_row(
                    &format!(
                        "SELECT x.run_id, x.status, x.exit_code, x.started_ms, x.finished_ms, r.head,
                                json_extract(j.record,'$.status')
                         FROM verify_results x JOIN verify_runs r ON r.id=x.run_id
                         LEFT JOIN jobs j ON j.id=r.id
                         WHERE r.task_id=?1 AND r.revision=?2 AND x.name=?3 {}
                         ORDER BY r.created_ms DESC, r.id DESC LIMIT 1",
                        if at_head { "AND r.head=?4" } else { "AND (?4 IS NULL OR r.head<>?4)" }
                    ),
                    params![task_id, set.revision, name, head],
                    |r| {
                        let stored: String = r.get(1)?;
                        let job_status: Option<String> = r.get(6)?;
                        let started: Option<i64> = r.get(3)?;
                        let finished: Option<i64> = r.get(4)?;
                        Ok(json!({
                            "run_id": r.get::<_, String>(0)?,
                            "state": result_state(&stored, job_status.as_deref()),
                            "exit_code": r.get::<_, Option<i64>>(2)?,
                            "duration_ms": started.zip(finished).map(|(s, f)| f - s),
                            "head": r.get::<_, String>(5)?,
                        }))
                    },
                )
                .optional()?)
        };
        let mut row = match head.map(|_| latest(true)).transpose()?.flatten() {
            Some(found) => found,
            None => match latest(false)? {
                Some(mut older) => {
                    older["state"] = json!("stale");
                    older
                }
                None => json!({"state": "not run"}),
            },
        };
        row["name"] = json!(name);
        row["failures"] = json!(failures(store, task_id, set.revision, name)?);
        all &= row["state"] == "passed";
        rows.push(row);
    }
    Ok(Some(json!({
        "revision": set.revision, "head": head, "verified": all, "checks": rows,
    })))
}

/// The lines a crew outcome adds about verification, its structured copy, and
/// whether the crew's work is unverified although checks were asked for.
pub(crate) fn outcome_block(store: &Store, job: &Value) -> Result<Option<(String, Value, bool)>> {
    let task_id = job["task_id"].as_str().unwrap_or_default();
    let head = job["cwd"].as_str().map(Path::new).and_then(head);
    let Some(state) = state(store, task_id, head.as_deref())? else {
        return Ok(Some((
            "Unverified: no checks were set for this task (verify.set).\n".to_owned(),
            json!({"verified": false, "set": false}),
            false,
        )));
    };
    if let Some(reason) = state["none"].as_str() {
        return Ok(Some((
            format!("Verification: none requested ({reason}).\n"),
            state,
            false,
        )));
    }
    let checks = state["checks"].as_array().cloned().unwrap_or_default();
    let passed = checks.iter().filter(|c| c["state"] == "passed").count();
    let at = head
        .as_deref()
        .map(short)
        .unwrap_or("an unknown head")
        .to_owned();
    let mut text = format!(
        "Verification at {at}: {passed} of {} passed{}.\n",
        checks.len(),
        if state["verified"] == true {
            ", verified"
        } else {
            ", unverified"
        }
    );
    for c in &checks {
        text.push_str(&check_line(c));
    }
    let unverified = state["verified"] != true;
    Ok(Some((text, state, unverified)))
}

fn check_line(c: &Value) -> String {
    let mut line = format!(
        "  {} {}",
        clip(c["name"].as_str().unwrap_or("?"), NAME_CHARS),
        c["state"].as_str().unwrap_or("?")
    );
    if let Some(code) = c["exit_code"].as_i64().filter(|_| c["state"] == "failed") {
        line.push_str(&format!(" exit {code}"));
    }
    if let Some(ms) = c["duration_ms"].as_i64() {
        line.push_str(&format!(" {:.1} s", ms as f64 / 1000.0));
    }
    if c["state"] == "stale"
        && let Some(at) = c["head"].as_str()
    {
        line.push_str(&format!(" (ran at {})", short(at)));
    }
    if let Some(run) = c["run_id"].as_str().filter(|_| c["state"] != "passed") {
        line.push_str(&format!(" [{run}]"));
    }
    line.push('\n');
    line
}

/// For a verification's own job: each check's result in that run, as lines.
pub(crate) fn run_lines(store: &Store, job: &Value) -> Result<Option<String>> {
    let Some(id) = job["id"].as_str() else {
        return Ok(None);
    };
    let head: Option<String> = store
        .connection()
        .query_row("SELECT head FROM verify_runs WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(head) = head else {
        return Ok(None);
    };
    let mut statement = store.connection().prepare(
        "SELECT name, status, exit_code, started_ms, finished_ms FROM verify_results
         WHERE run_id=?1 ORDER BY position",
    )?;
    let job_status = job["status"].as_str();
    let rows = statement
        .query_map([id], |r| {
            let stored: String = r.get(1)?;
            let started: Option<i64> = r.get(3)?;
            let finished: Option<i64> = r.get(4)?;
            Ok(json!({
                "name": r.get::<_, String>(0)?,
                "state": result_state(&stored, job_status),
                "exit_code": r.get::<_, Option<i64>>(2)?,
                "duration_ms": started.zip(finished).map(|(s, f)| f - s),
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut text = format!("Checks at {}:\n", short(&head));
    for row in &rows {
        text.push_str(&check_line(row));
    }
    Ok(Some(text))
}

/// `verify.status`: the task's checks and their state at the crew's head (a
/// crew), or at the head of the task's latest verification.
fn status(store: &Store, args: &Value) -> Result<Value> {
    let task_id = required(args, "task_id")?;
    store.task(task_id)?;
    let cwd: Option<String> = match store.crew() {
        Some(crew) => crate::jobs::record(store, &crew.crew)?["cwd"]
            .as_str()
            .map(str::to_owned),
        None => store
            .connection()
            .query_row(
                "SELECT cwd FROM verify_runs WHERE task_id=?1 ORDER BY created_ms DESC, id DESC LIMIT 1",
                [task_id],
                |r| r.get(0),
            )
            .optional()?,
    };
    let head = cwd.as_deref().map(Path::new).and_then(head);
    Ok(state(store, task_id, head.as_deref())?
        .map(|mut s| {
            s["task_id"] = json!(task_id);
            s["attempts"] = json!(ATTEMPTS);
            s
        })
        .unwrap_or_else(|| json!({"task_id": task_id, "set": false, "checks": []})))
}

pub(crate) const ACCEPTED: crate::Accepted = &[
    ("verify.set", &["task_id", "checks", "none"]),
    ("verify.run", &["task_id", "names"]),
    ("verify.status", &["task_id"]),
];

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "verify.set" => set(store, &args),
        "verify.run" => run(store, &args),
        "verify.status" => status(store, &args),
        _ => bail!("unknown verify command {command}"),
    }
}
