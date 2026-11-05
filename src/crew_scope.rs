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
//! its own, as test suites run inside a crew's gate job do. A refusal is
//! audited and noticed only for the job's home or a home the real registry
//! lists; a call that reaches the world only through the registry is refused
//! and recorded nowhere. A caller with no job ancestor is resolved as before,
//! except that one whose `PRIVATEER_JOB` names a running crew is narrowed to
//! that crew. Under one OS account this is a workflow control, not a security
//! boundary.
mod ancestry;

use crate::mail::record_crew;
use crate::store::{Caller, Store};
use ancestry::Ancestor;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

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

/// Where a command's target home lies relative to a job's world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum World {
    /// Not in the job's world: the caller is resolved as before.
    Outside,
    /// Reached only through the fleet registry in use, which is the real one.
    /// A refusal is made and recorded nowhere.
    Registry,
    /// The job's own home, or a home the real registry lists. A refusal is
    /// audited in the job's home and the first mate is noticed.
    Home,
}

/// Resolve the caller from its process ancestry, for a command on `target`
/// (the home it names, if any). Refuses a job that drops its role or borrows
/// another job. `supervisor` says the command is itself a `_job-supervise`,
/// whose `PRIVATEER_JOB` and role are its starter's: it is never refused.
pub(crate) fn resolve(target: Option<&Path>, caller: Caller, supervisor: bool) -> Result<Caller> {
    let env_job = env_job();
    let proven =
        supervising_job(target).inspect_err(|error| note_refusal(target, &format!("{error:#}")))?;
    if let Some((home, job, record)) = proven {
        let world = world(&home, target)?;
        if world != World::Outside {
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
            // A supervisor is the runtime, not a caller in its ancestor's job:
            // its environment is its starter's. A parked crew's resume is
            // launched from inside the supervisor that settled, which may
            // carry the dispatcher's operator environment beneath an agent's
            // job. It takes that job's agent and is never refused, since a
            // refused supervisor exits before it claims its own job.
            let refusal = match (&caller, &env_job) {
                _ if supervisor => None,
                (_, Some(named)) if named != &job => Some(format!(
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
                if world == World::Home {
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
                }
                note_refusal(target, &refusal);
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

/// Where a command on `target` lands: the job's home or a home the real
/// registry lists, reached only through the registry in use, or outside the
/// job's world. A target with no home is reached only through the registry.
fn world(home: &Path, target: Option<&Path>) -> Result<World> {
    let real = real_registry();
    if let Some(target) = target {
        let target = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_owned());
        if target == home {
            return Ok(World::Home);
        }
        if let Some(registry) = real.as_deref().filter(|r| r.is_file()) {
            let conn = Connection::open_with_flags(registry, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let registered: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM ships WHERE home=?1)",
                [target.to_string_lossy()],
                |r| r.get(0),
            )?;
            if registered {
                return Ok(World::Home);
            }
        }
    }
    let chosen = crate::fleet::registry_path()?;
    Ok(match real {
        Some(real) if same_path(&real, &chosen) => World::Registry,
        _ => World::Outside,
    })
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

/// A job a supervisor runs: the job's home, its id and its record.
type Supervised = (PathBuf, String, Value);

/// The nearest ancestor that is its job's recorded supervisor
/// (docs/design/codex-sandboxed-callers.md). Any other `_job-supervise`
/// process proves nothing and is passed over.
fn supervising_job(target: Option<&Path>) -> Result<Option<Supervised>> {
    let chain = ancestry::ancestors()?;
    let homes = if chain.iter().any(|a| a.argv.is_none()) {
        world_homes(target)
    } else {
        vec![]
    };
    let mut comms = vec!["privateer".to_owned()];
    comms.extend(ancestry::own_comm());
    find_supervisor(&chain, &homes, &comms, Store::now_ms())
}

/// The supervisor among `chain`, nearest first. An ancestor whose argv is
/// visible is proved as before, from the home and job its argv names. One
/// whose argv a sandbox hides is proved by a job in `homes` whose recorded
/// supervisor has its pid and start. A hidden Privateer process (by kernel
/// command name, `comms`) that no record there accounts for may be the
/// caller's supervisor in a home this caller cannot name, so the caller is
/// refused rather than resolved as if it had none.
fn find_supervisor(
    chain: &[Ancestor],
    homes: &[PathBuf],
    comms: &[String],
    now_ms: i64,
) -> Result<Option<Supervised>> {
    let hidden: Vec<&Ancestor> = chain.iter().filter(|a| a.argv.is_none()).collect();
    let recorded = if hidden.is_empty() {
        vec![]
    } else {
        recorded_supervisors(homes, &hidden)?
    };
    for ancestor in chain {
        if let Some(argv) = &ancestor.argv {
            let Some((home, job)) = supervisor_argv(argv) else {
                continue;
            };
            let Ok(conn) = open_readonly(&home) else {
                continue;
            };
            let Some(record) = job_record(&conn, &job)? else {
                continue;
            };
            if proves(&record["supervisor"], ancestor)? {
                let home = std::fs::canonicalize(&home).unwrap_or(home);
                return Ok(Some((home, job, record)));
            }
            continue;
        }
        let mut matches = vec![];
        for supervised in &recorded {
            if proves(&supervised.2["supervisor"], ancestor)? {
                matches.push(supervised);
            }
        }
        match matches.as_slice() {
            [] => {}
            [one] => return Ok(Some((*one).clone())),
            [a, b, ..] => bail!(
                "ancestor process {} is recorded as the supervisor of job {} in {} and of job {} in {}; Privateer refuses a caller whose job is ambiguous",
                ancestor.pid,
                a.1,
                a.0.display(),
                b.1,
                b.0.display()
            ),
        }
        let privateer = ancestor.comm.as_ref().is_some_and(|c| comms.contains(c));
        if privateer && !session_process(homes, ancestor, now_ms) {
            bail!(
                "this caller's sandbox hides the command line of its ancestor process {} ({}), a Privateer process that no job, hosted pane or attended session in the {} home(s) this caller can name records. It may be this caller's supervisor, so Privateer refuses the command rather than guess its job. This is a limit of the sandbox, not a missing Privateer permission: run the command outside the sandbox, or name the job's home with --home.",
                ancestor.pid,
                ancestor.comm.as_deref().unwrap_or("?"),
                homes.len()
            );
        }
    }
    Ok(None)
}

/// Whether a job's recorded supervisor identity is this ancestor: the same
/// pid, start identity and PID namespace. Where the kernel walk read the
/// start (macOS) it is compared here, with no subprocess.
fn proves(recorded: &Value, ancestor: &Ancestor) -> Result<bool> {
    let Some(started) = &ancestor.started else {
        return crate::jobs::is_recorded_supervisor(recorded, ancestor.pid);
    };
    Ok(recorded["pid"].as_u64() == Some(u64::from(ancestor.pid))
        && recorded.get("pid_namespace").is_none_or(Value::is_null)
        && recorded["started"].as_str() == Some(started.as_str()))
}

/// The homes a caller can name: the target and every home the real
/// registry lists, canonical and once each.
fn world_homes(target: Option<&Path>) -> Vec<PathBuf> {
    let mut homes: Vec<PathBuf> = target.map(Path::to_owned).into_iter().collect();
    if let Some(registry) = real_registry().filter(|r| r.is_file()) {
        let listed = Connection::open_with_flags(&registry, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .and_then(|conn| {
                let mut statement = conn.prepare("SELECT home FROM ships")?;
                statement
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            });
        homes.extend(listed.unwrap_or_default().into_iter().map(PathBuf::from));
    }
    let mut canonical: Vec<PathBuf> = vec![];
    for home in homes {
        let home = std::fs::canonicalize(&home).unwrap_or(home);
        if !canonical.contains(&home) {
            canonical.push(home);
        }
    }
    canonical
}

/// The jobs in `homes` whose recorded supervisor has one of these pids. A
/// home that cannot be read records nothing here, which leaves a hidden
/// Privateer ancestor unaccounted for: it is refused, never passed over.
fn recorded_supervisors(homes: &[PathBuf], hidden: &[&Ancestor]) -> Result<Vec<Supervised>> {
    let pids = Value::from(hidden.iter().map(|a| a.pid).collect::<Vec<_>>()).to_string();
    let mut found = vec![];
    for home in homes {
        if !home.join("privateer.sqlite").is_file() {
            continue;
        }
        let Ok(conn) = open_readonly(home) else {
            continue;
        };
        let rows = conn
            .prepare(
                "SELECT id, record FROM jobs WHERE json_extract(record, '$.supervisor.pid')
                   IN (SELECT value FROM json_each(?1))",
            )
            .and_then(|mut statement| {
                statement
                    .query_map([&pids], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            });
        for (id, raw) in rows.unwrap_or_default() {
            found.push((home.clone(), id, serde_json::from_str(&raw)?));
        }
    }
    Ok(found)
}

/// Whether a hidden Privateer ancestor is a first mate's session process,
/// not a supervisor: a hosted pane's `privateer start --hosted` (its pid and
/// start in `hosted_panes`) or an attended `privateer start` (its pid in a
/// live `start:PID:...` presence lease), recorded in one of `homes`.
fn session_process(homes: &[PathBuf], ancestor: &Ancestor, now_ms: i64) -> bool {
    homes.iter().any(|home| {
        let Ok(conn) = open_readonly(home) else {
            return false;
        };
        let hosted = ancestor.started.as_ref().is_some_and(|started| {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM hosted_panes WHERE pid=?1 AND started=?2)",
                params![ancestor.pid, started],
                |r| r.get(0),
            )
            .unwrap_or(false)
        });
        hosted
            || conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM presence WHERE expires_ms>?2
                       AND substr(holder, 1, length(?1)) = ?1)",
                    params![format!("start:{}:", ancestor.pid), now_ms],
                    |r| r.get(0),
                )
                .unwrap_or(false)
    })
}

/// Note, on the running wake `PRIVATEER_JOB` names in the target home, why
/// this caller was refused before any command ran: a wake that settles with
/// no contact names it (design (c)). Best effort and diagnostic only; the
/// environment is unproven here, and nothing reads this but that error.
fn note_refusal(target: Option<&Path>, reason: &str) {
    let (Some(job), Some(home)) = (env_job(), target) else {
        return;
    };
    let path = home.join("privateer.sqlite");
    if !path.is_file() {
        return;
    }
    let Ok(conn) = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE) else {
        return;
    };
    let _ = conn.busy_timeout(Duration::from_secs(2));
    let reason: String = reason.chars().take(1024).collect();
    let _ = conn.execute(
        "UPDATE wakes SET caller_refusal=?2 WHERE job_id=?1 AND status='running' AND contact_ms IS NULL",
        params![job, reason],
    );
}

/// Whether a job the supervisor is about to launch is a crew's: an agent's
/// spawn that is neither a first mate's wake nor an errand.
pub(crate) fn is_crew_launch(
    store: &Store,
    job_id: &str,
    action: &str,
    agent: bool,
) -> Result<bool> {
    if !agent || action != "spawn" {
        return Ok(false);
    }
    let wake: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM wakes WHERE job_id=?1)",
        [job_id],
        |r| r.get(0),
    )?;
    Ok(!wake && !crate::errand::is_errand(store, job_id)?)
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
        "verify.run" | "verify.status" => (text("task_id") != Some(crew.task.as_str()))
            .then(|| format!("crew job {} works only on task {}", crew.crew, crew.task)),
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
    use tempfile::TempDir;

    const FIRST_MATE_START: &str = "Mon Nov 2 09:00:00 2026";
    const CREW_START: &str = "Mon Nov 2 09:05:00 2026";

    /// A ship home with a first mate's wake job (supervised by pid 100) and
    /// its crew's spawn job (supervised by pid 200).
    fn home(temp: &TempDir, name: &str) -> PathBuf {
        ship_home(temp, name, true)
    }

    fn ship_home(temp: &TempDir, name: &str, crew: bool) -> PathBuf {
        let home = temp.path().join(name);
        let store = Store::open(&home).unwrap();
        let job = |id: &str, pid: u32, started: &str| {
            store
                .connection()
                .execute(
                    "INSERT INTO jobs(id, record) VALUES(?1, ?2)",
                    params![
                        format!("{name}-{id}"),
                        json!({"id": format!("{name}-{id}"), "action": "spawn",
                            "agent_actor": "gull", "task_id": "survey", "status": "running",
                            "supervisor": {"pid": pid, "started": started, "pid_namespace": null}})
                        .to_string()
                    ],
                )
                .unwrap();
        };
        job("wake", 100, FIRST_MATE_START);
        if crew {
            job("crew", 200, CREW_START);
        }
        store
            .connection()
            .execute(
                "INSERT INTO wakes(id, ship, focus, through_seq, job_id, mode, status, created_ms)
                 VALUES(?1, 'gull', 'task:survey', 1, ?1, 'fresh', 'running', 0)",
                [format!("{name}-wake")],
            )
            .unwrap();
        std::fs::canonicalize(home).unwrap()
    }

    fn ancestor(pid: u32, started: &str, comm: &str, argv: Option<&str>) -> Ancestor {
        Ancestor {
            pid,
            started: Some(started.into()),
            comm: Some(comm.into()),
            argv: argv.map(|a| a.split(' ').map(str::to_owned).collect()),
        }
    }

    /// A crew's CLI under the crew's harness and supervisor, in a tree whose
    /// argv is visible or hidden; the first mate's supervisor is farther up.
    fn crew_tree(home: &Path, hidden: bool) -> Vec<Ancestor> {
        let supervise = |job: &str| {
            (!hidden).then(|| format!("privateer --home {} _job-supervise {job}", home.display()))
        };
        let name = home.file_name().unwrap().to_string_lossy().into_owned();
        vec![
            ancestor(300, CREW_START, "codex", (!hidden).then_some("codex exec")),
            ancestor(
                200,
                CREW_START,
                "privateer",
                supervise(&format!("{name}-crew")).as_deref(),
            ),
            ancestor(
                100,
                FIRST_MATE_START,
                "privateer",
                supervise(&format!("{name}-wake")).as_deref(),
            ),
        ]
    }

    fn comms() -> Vec<String> {
        vec!["privateer".to_owned()]
    }

    /// The role a found supervisor gives: Some(crew task) or None (a first
    /// mate's job).
    fn role(found: &Supervised) -> Option<String> {
        let conn = open_readonly(&found.0).unwrap();
        classify(&conn, &found.1, "gull")
            .unwrap()
            .map(|crew| crew.task)
    }

    #[test]
    fn argv_visible_finds_the_nearest_recorded_supervisor() {
        let temp = TempDir::new().unwrap();
        let home = home(&temp, "a");
        let found = find_supervisor(&crew_tree(&home, false), &[], &comms(), 0)
            .unwrap()
            .unwrap();
        assert_eq!(found.1, "a-crew");
        assert_eq!(role(&found).as_deref(), Some("survey"), "crew");
        // A `_job-supervise` argv whose start is not the record's proves nothing.
        let mut tree = crew_tree(&home, false);
        tree[1].started = Some("Tue Nov 3 00:00:00 2026".into());
        let found = find_supervisor(&tree, &[], &comms(), 0).unwrap().unwrap();
        assert_eq!(found.1, "a-wake", "passed over to the next proven one");
    }

    #[test]
    fn argv_hidden_with_a_match_gives_the_same_role_as_visible_argv() {
        let temp = TempDir::new().unwrap();
        let home = home(&temp, "a");
        let other = home_elsewhere(&temp);
        let found = find_supervisor(&crew_tree(&home, true), &[other, home.clone()], &comms(), 0)
            .unwrap()
            .unwrap();
        assert_eq!((found.0.clone(), found.1.as_str()), (home, "a-crew"));
        assert_eq!(
            role(&found).as_deref(),
            Some("survey"),
            "crew, never the first mate"
        );
    }

    fn home_elsewhere(temp: &TempDir) -> PathBuf {
        let home = temp.path().join("elsewhere");
        Store::open(&home).unwrap();
        std::fs::canonicalize(home).unwrap()
    }

    #[test]
    fn argv_hidden_with_no_privateer_ancestor_resolves_as_before() {
        let temp = TempDir::new().unwrap();
        let home = home(&temp, "a");
        // The captain's own sandboxed harness: no supervisor could be here.
        let tree = vec![
            ancestor(300, CREW_START, "codex", None),
            ancestor(250, CREW_START, "zsh", None),
        ];
        assert_eq!(find_supervisor(&tree, &[home], &comms(), 0).unwrap(), None);
    }

    #[test]
    fn argv_hidden_with_a_recorded_session_process_resolves_as_before() {
        let temp = TempDir::new().unwrap();
        let home = home(&temp, "a");
        let store = Store::open(&home).unwrap();
        let tree = vec![
            ancestor(300, CREW_START, "codex", None),
            ancestor(250, CREW_START, "privateer", None),
        ];
        // An attended `privateer start`: its pid in a live presence lease.
        store
            .connection()
            .execute(
                "INSERT INTO presence(ship, holder, started_ms, expires_ms) VALUES('gull', 'start:250:x', 0, 1000)",
                [],
            )
            .unwrap();
        assert_eq!(
            find_supervisor(&tree, std::slice::from_ref(&home), &comms(), 999).unwrap(),
            None
        );
        // Once that lease has lapsed, nothing accounts for it.
        assert!(find_supervisor(&tree, std::slice::from_ref(&home), &comms(), 1000).is_err());
        // A hosted pane's `privateer start --hosted`: its pid and start.
        store
            .connection()
            .execute(
                "INSERT INTO hosted_panes(ship, pid, started, executable, recorded_ms) VALUES('gull', 250, ?1, 'x', 0)",
                [CREW_START],
            )
            .unwrap();
        assert_eq!(
            find_supervisor(&tree, &[home], &comms(), 1000).unwrap(),
            None
        );
    }

    #[test]
    fn argv_hidden_with_an_unrecorded_privateer_ancestor_is_refused() {
        let temp = TempDir::new().unwrap();
        // The crew's supervisor is recorded in a home outside the caller's
        // world: the visible-argv rule would find it, so with argv hidden the
        // caller is refused, never resolved as the operator or a first mate.
        let unnamed = home(&temp, "unnamed");
        // The first mate's wake farther up is recorded in the named home.
        let named = ship_home(&temp, "named", false);
        let tree = crew_tree(&unnamed, true);
        let error = find_supervisor(&tree, std::slice::from_ref(&named), &comms(), 0)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("ancestor process 200 (privateer)"),
            "{error}"
        );
        assert!(
            error.contains("not a missing Privateer permission"),
            "{error}"
        );
        assert!(!error.contains("permissions.blocked"), "{error}");
        // A Privateer binary under another name counts by this CLI's own name.
        let mut renamed = crew_tree(&unnamed, true);
        renamed[1].comm = Some("pvt".into());
        let error = find_supervisor(
            &renamed,
            std::slice::from_ref(&named),
            &["privateer".into(), "pvt".into()],
            0,
        );
        assert!(error.is_err());
        // Without that ancestor, the first mate's supervisor is the nearest.
        let found = find_supervisor(&tree[2..], &[named], &comms(), 0)
            .unwrap()
            .unwrap();
        assert_eq!(role(&found), None, "first mate");
    }

    #[test]
    fn argv_hidden_with_an_ambiguous_match_is_refused() {
        let temp = TempDir::new().unwrap();
        let a = home(&temp, "a");
        let b = home(&temp, "b");
        let error = find_supervisor(&crew_tree(&a, true), &[a, b], &comms(), 0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("ambiguous"), "{error}");
    }

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
