//! The crew role at the runtime boundary (docs/design/crew-role.md).
//!
//! A CLI process with a `_job-supervise` process among its ancestors is a job
//! caller. The nearest such ancestor whose pid and start time are the ones its
//! job records as `supervisor` names the caller's job, from the supervisor's
//! own argv (`--home H _job-supervise J`), never from the environment. The
//! nearest spawn job in that job's lineage decides the role: a wake or an
//! errand is a first mate, any other spawn job is crew. A job that runs as an
//! agent cannot drop its role, and a crew may run only the commands listed in
//! `check`.
//!
//! The rule holds in the job's own world: its home, the homes of this
//! machine's fleet registry, and the fleet-wide state beside that registry. A
//! caller that names another registry and another home works in a world of
//! its own, as test suites run inside a crew's gate job do. A caller with no
//! job ancestor is resolved as before, except that one whose `PRIVATEER_JOB`
//! names a running crew is narrowed to that crew. Under one OS account this is
//! a workflow control, not a security boundary.
use crate::mail::record_crew;
use crate::store::{Caller, Store};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// Parent links followed from a caller to its supervisor.
const ANCESTRY_DEPTH: usize = 64;
/// `started_by` links followed from a job to its spawn job.
const LINEAGE_DEPTH: usize = 16;

/// A crew caller: the job its process runs in, the crew (spawn) job that job
/// belongs to, the crew's task and the agent it runs as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrewCaller {
    pub job: String,
    pub crew: String,
    pub task: String,
    pub actor: String,
}

struct Resolved {
    job: Option<String>,
    crew: Option<CrewCaller>,
}

static RESOLVED: OnceLock<Resolved> = OnceLock::new();

fn env_job() -> Option<String> {
    std::env::var("PRIVATEER_JOB")
        .ok()
        .filter(|job| !job.is_empty())
}

/// The job this process runs in: its proven supervisor's job, or else
/// `PRIVATEER_JOB` as before.
pub(crate) fn job_id() -> Option<String> {
    match RESOLVED.get() {
        Some(resolved) => resolved.job.clone(),
        None => env_job(),
    }
}

/// The crew this process was resolved to, if any.
pub(crate) fn crew() -> Option<CrewCaller> {
    RESOLVED.get().and_then(|resolved| resolved.crew.clone())
}

/// Resolve the caller from its process ancestry, for a command on `target`
/// (the home it names, if any). Refuses a job that drops its role or borrows
/// another job.
pub(crate) fn resolve(target: Option<&Path>, caller: Caller) -> Result<Caller> {
    let env_job = env_job();
    if let Some((home, job, record)) = supervising_job()? {
        if in_world(&home, target)? {
            let Some(actor) = record["agent_actor"].as_str().map(str::to_owned) else {
                // The captain's own job runs as the captain.
                let _ = RESOLVED.set(Resolved {
                    job: Some(job),
                    crew: None,
                });
                return Ok(caller);
            };
            let conn = open_readonly(&home)?;
            let crew = classify(&conn, &job, &actor)?;
            drop(conn);
            // A supervisor's PRIVATEER_JOB is its starter's, not a job it runs
            // in, and lineage it records keeps using it as before.
            let supervisor = supervisor_argv(&std::env::args().collect::<Vec<_>>()).is_some();
            let refusal = match (&caller, &env_job) {
                (_, Some(named)) if named != &job && !supervisor => Some(format!(
                    "this process runs in job {job}; PRIVATEER_JOB cannot name job {named}"
                )),
                (Caller::Operator, _) => Some(format!(
                    "job {job} runs as agent {actor}; a job cannot drop its role"
                )),
                (Caller::Agent { actor: claimed }, _) if claimed != &actor => Some(format!(
                    "job {job} runs as agent {actor}; it cannot act as {claimed}"
                )),
                _ => None,
            };
            if let Some(refusal) = refusal {
                // Recording is best effort: the refusal stands either way.
                let _ = Store::open(&home).and_then(|store| {
                    store.audit(
                        "caller.role_drop_refused",
                        &json!({"job_id": job, "agent": actor, "reason": refusal}),
                    )?;
                    match &crew {
                        Some(crew) => notice(&store, crew, "dropping its role"),
                        None => Ok(()),
                    }
                });
                bail!("{refusal}");
            }
            let _ = RESOLVED.set(Resolved {
                job: if supervisor { env_job } else { Some(job) },
                crew,
            });
            return Ok(Caller::Agent { actor });
        }
    }
    // No supervisor proves the job. Never weaker than before: a process whose
    // PRIVATEER_JOB names a running crew is that crew, whatever its role says.
    let crew = match (&env_job, target) {
        (Some(job), Some(home)) => running_crew(home, job)?,
        _ => None,
    };
    let caller = match &crew {
        Some(crew) => Caller::Agent {
            actor: crew.actor.clone(),
        },
        None => caller,
    };
    let _ = RESOLVED.set(Resolved { job: env_job, crew });
    Ok(caller)
}

fn open_readonly(home: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        home.join("privateer.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .with_context(|| format!("open the job's home {}", home.display()))?;
    connection.busy_timeout(Duration::from_secs(5))?;
    Ok(connection)
}

fn job_record(conn: &Connection, id: &str) -> Result<Option<Value>> {
    let raw: Option<String> = conn
        .query_row("SELECT record FROM jobs WHERE id=?1", [id], |r| r.get(0))
        .optional()?;
    raw.map(|raw| Ok(serde_json::from_str(&raw)?)).transpose()
}

/// The crew a job belongs to, from the nearest spawn job in its lineage. A
/// wake or errand is a first mate's; a lineage with no spawn job is no crew.
fn classify(conn: &Connection, job: &str, actor: &str) -> Result<Option<CrewCaller>> {
    let mut id = job.to_owned();
    let mut seen = vec![];
    for _ in 0..LINEAGE_DEPTH {
        let record = job_record(conn, &id)?.with_context(|| format!("unknown job {id}"))?;
        if record["action"] == "spawn" {
            let first_mate: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM wakes WHERE job_id=?1)
                   OR EXISTS(SELECT 1 FROM errands WHERE id=?1)",
                [&id],
                |r| r.get(0),
            )?;
            if first_mate || record["agent_actor"] != actor {
                return Ok(None);
            }
            return Ok(Some(CrewCaller {
                job: job.to_owned(),
                crew: id,
                task: record["task_id"].as_str().context("job task")?.to_owned(),
                actor: actor.to_owned(),
            }));
        }
        seen.push(id.clone());
        match record["started_by"].as_str() {
            Some(parent) => {
                ensure!(
                    !seen.iter().any(|s| s == parent),
                    "job {job} has a lineage cycle"
                );
                id = parent.to_owned();
            }
            None => return Ok(None),
        }
    }
    bail!("job {job} has a lineage deeper than {LINEAGE_DEPTH}")
}

/// The crew a running job named by the environment belongs to, in `home`.
fn running_crew(home: &Path, job: &str) -> Result<Option<CrewCaller>> {
    if !home.join("privateer.sqlite").is_file() {
        return Ok(None);
    }
    let conn = open_readonly(home)?;
    let Some(record) = job_record(&conn, job)? else {
        return Ok(None);
    };
    if !matches!(record["status"].as_str(), Some("launching" | "running")) {
        return Ok(None);
    }
    match record["agent_actor"].as_str() {
        Some(actor) => classify(&conn, job, actor),
        None => Ok(None),
    }
}

/// Whether a command on `target` reaches the job's world: the job's home, a
/// home in this machine's fleet registry, or the state beside that registry.
fn in_world(home: &Path, target: Option<&Path>) -> Result<bool> {
    let real = real_registry();
    if let Some(target) = target {
        let target = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_owned());
        if target == home {
            return Ok(true);
        }
        if let Some(registry) = real.as_deref().filter(|r| r.is_file()) {
            let conn = Connection::open_with_flags(registry, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let registered: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM ships WHERE home=?1)",
                [target.to_string_lossy()],
                |r| r.get(0),
            )?;
            if registered {
                return Ok(true);
            }
        }
    }
    let chosen = crate::fleet::registry_path()?;
    Ok(real.is_some_and(|real| same_path(&real, &chosen)))
}

/// The fleet registry of this OS account, from its password entry rather
/// than from HOME or XDG variables a caller can change.
fn real_registry() -> Option<PathBuf> {
    // SAFETY: getpwuid returns a pointer into static storage or null; the
    // directory is copied out before any other call can overwrite it.
    let dir = unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() || (*entry).pw_dir.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr((*entry).pw_dir)
            .to_string_lossy()
            .into_owned()
    };
    Some(PathBuf::from(dir).join(".local/state/privateer/fleet.sqlite"))
}

fn same_path(a: &Path, b: &Path) -> bool {
    let canonical = |p: &Path| {
        p.parent()
            .and_then(|dir| std::fs::canonicalize(dir).ok())
            .zip(p.file_name())
            .map(|(dir, name)| dir.join(name))
            .unwrap_or_else(|| p.to_owned())
    };
    canonical(a) == canonical(b)
}

/// The nearest ancestor that is its job's recorded supervisor: the home and
/// job from its argv, and the job's record. Any other `_job-supervise`
/// process proves nothing and is passed over.
fn supervising_job() -> Result<Option<(PathBuf, String, Value)>> {
    for (pid, argv) in ancestors()? {
        let Some((home, job)) = supervisor_argv(&argv) else {
            continue;
        };
        let Ok(conn) = open_readonly(&home) else {
            continue;
        };
        let Some(record) = job_record(&conn, &job)? else {
            continue;
        };
        if crate::jobs::is_recorded_supervisor(&record["supervisor"], pid)? {
            let home = std::fs::canonicalize(&home).unwrap_or(home);
            return Ok(Some((home, job, record)));
        }
    }
    Ok(None)
}

/// `--home H _job-supervise J` from a supervisor's argv.
fn supervisor_argv(argv: &[String]) -> Option<(PathBuf, String)> {
    let at = argv.iter().position(|a| a == "_job-supervise")?;
    let job = argv.get(at + 1)?.clone();
    let home = argv
        .windows(2)
        .find(|w| w[0] == "--home")
        .map(|w| PathBuf::from(&w[1]))?;
    home.is_absolute().then_some((home, job))
}

/// This process's ancestors, nearest first, with their argv.
#[cfg(target_os = "linux")]
fn ancestors() -> Result<Vec<(u32, Vec<String>)>> {
    let parent = |pid: u32| -> Option<u32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Fields after the command's final ')' start at the state; the
        // parent PID follows it.
        stat.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    let mut found = vec![];
    let mut pid = std::process::id();
    for _ in 0..ANCESTRY_DEPTH {
        match parent(pid) {
            Some(next) if next > 1 => pid = next,
            _ => break,
        }
        let argv = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        found.push((
            pid,
            argv.split(|b| *b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect(),
        ));
    }
    Ok(found)
}

/// This process's ancestors, nearest first, from one process table. ps joins
/// argv with spaces, so a home path with a space is not recognised.
#[cfg(not(target_os = "linux"))]
fn ancestors() -> Result<Vec<(u32, Vec<String>)>> {
    let output = std::process::Command::new("ps")
        .args(["-axww", "-o", "pid=,ppid=,args="])
        .env("LC_ALL", "C")
        .output()
        .context("inspect process ancestry")?;
    ensure!(
        output.status.success(),
        "process ancestry inspection failed"
    );
    let mut table = std::collections::HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split_whitespace();
        let (Some(Ok(pid)), Some(Ok(parent))) = (
            fields.next().map(str::parse::<u32>),
            fields.next().map(str::parse::<u32>),
        ) else {
            continue;
        };
        table.insert(pid, (parent, fields.map(str::to_owned).collect::<Vec<_>>()));
    }
    let mut found = vec![];
    let mut pid = std::process::id();
    for _ in 0..ANCESTRY_DEPTH {
        match table.get(&pid) {
            Some((next, _)) if *next > 1 => pid = *next,
            _ => break,
        }
        let argv = table.get(&pid).map(|(_, a)| a.clone()).unwrap_or_default();
        found.push((pid, argv));
    }
    Ok(found)
}

/// Tell the first mate once per crew job that its crew tried a first mate's
/// command. Later refusals are in the audit only.
fn notice(store: &Store, crew: &CrewCaller, what: &str) -> Result<()> {
    record_crew(
        store.connection(),
        &json!({
            "id": format!("crew-{}-refused", crew.crew), "to": crew.actor, "kind": "notice",
            "task_id": crew.task, "urgency": "low",
            "body": format!(
                "Crew job {} tried {what}, which is the first mate's, and was refused and told to report with crew.report. Further refusals of this crew are audited as crew.refused.",
                crew.crew
            ),
            "payload": {"job_id": crew.crew, "refused": what},
        }),
    )?;
    Ok(())
}

/// The one refusal a crew gets for a first mate's command.
fn first_mates(command: &str) -> String {
    format!(
        "{command} is the first mate's, not crew's. Report instead: crew.report {{\"kind\":\"progress\",\"body\":\"...\"}} (or \"decision\" to ask). Your first mate mails, updates tasks, pushes and merges."
    )
}

/// Default deny: what a crew caller may run, checked in `dispatch`.
pub(crate) fn check(store: &Store, command: &str, args: &Value) -> Result<()> {
    let Some(crew) = store.crew() else {
        return Ok(());
    };
    let text = |key: &str| args.get(key).and_then(Value::as_str);
    let own_job = |id: Option<&str>| -> Result<Option<String>> {
        let Some(id) = id else {
            return Ok(Some("a job id is required".into()));
        };
        let job = crate::jobs::record(store, id)?;
        Ok((job["task_id"] != crew.task.as_str())
            .then(|| format!("crew job {} works only on task {}", crew.crew, crew.task)))
    };
    let own_focus = || {
        let focus = text("focus");
        let own = [
            format!("task:{}", crew.task),
            format!("assignment:{}", crew.task),
        ];
        (!focus.is_some_and(|f| own.iter().any(|o| o == f))).then(|| {
            format!(
                "crew job {} reads only task:{} and its assignment",
                crew.crew, crew.task
            )
        })
    };
    let refusal = match command {
        "crew.report" | "crew.answer" | "crew.park" => None,
        "workspace.status" | "resource.status" | "runtime.needs" | "ship.list" | "ship.show"
        | "charter.show" | "upgrade.status" | "job.list" | "note.list" => None,
        "job.start" => (text("task_id") != Some(crew.task.as_str()))
            .then(|| format!("crew job {} works only on task {}", crew.crew, crew.task)),
        "job.status" => own_job(text("id"))?,
        "job.cancel" | "job.retry" => {
            let id = text("id").context("id required")?;
            let job = crate::jobs::record(store, id)?;
            let mine = crate::crew::lineage(store, &crew.crew)?;
            let started_by = job["started_by"].as_str();
            (job["action"] == "spawn"
                || !started_by.is_some_and(|s| s == crew.job || mine.iter().any(|m| m == s)))
            .then(|| {
                format!(
                    "crew job {} may cancel or retry only the jobs it started, never a launch; {}",
                    crew.crew,
                    first_mates(command)
                )
            })
        }
        "note.get" | "focus.digest" => own_focus(),
        "task.show" => (text("id") != Some(crew.task.as_str()))
            .then(|| format!("crew job {} works only on task {}", crew.crew, crew.task)),
        _ => Some(first_mates(command)),
    };
    let Some(refusal) = refusal else {
        return Ok(());
    };
    store.audit(
        "crew.refused",
        &json!({"job_id": crew.job, "crew_job_id": crew.crew, "command": command}),
    )?;
    notice(store, crew, command)?;
    bail!("{refusal}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_supervisor_argv_names_its_home_and_job() {
        let argv = |s: &str| s.split(' ').map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(
            supervisor_argv(&argv(
                "/bin/privateer --home /ships/a/.privateer _job-supervise j1"
            )),
            Some((PathBuf::from("/ships/a/.privateer"), "j1".to_owned()))
        );
        assert_eq!(
            supervisor_argv(&argv("privateer --home rel _job-supervise j1")),
            None
        );
        assert_eq!(
            supervisor_argv(&argv("privateer job.start --json {}")),
            None
        );
        assert_eq!(
            supervisor_argv(&argv("privateer --home /h _job-supervise")),
            None
        );
    }
}
