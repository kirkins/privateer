//! Launches for callers inside a sandbox (docs/design/codex-sandboxed-launch.md).
//!
//! A sandboxed CLI cannot start supervised work: on Linux its PID namespace
//! is not the dispatcher's, and on macOS whatever it spawns inherits its
//! sandbox. It writes the command into a `launch_requests` row instead and
//! execs into `_launch-wait ID`, which keeps its pid and start time and puts
//! the row's id and a digest of its content in its argv. The home's resident
//! dispatcher starts one `_launch-run ID` per row. That launcher finds the
//! requester from outside the sandbox, checks that binding, resolves the
//! requester's caller with the rule a direct call uses, checks that the
//! requester is still the same process, and runs the same `dispatch` as that
//! caller. The waiter prints the answer, or the error, as a direct call would.
//!
//! Choosing this path is not a security decision: the launcher decides who
//! the caller is, from the kernel and the job records, never from the row.
mod identify;

use crate::store::{Caller, Store};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::ffi::{CString, OsString};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The commands a sandboxed CLI hands to the dispatcher: each starts
/// supervised work, writes the host ledger for it, or inspects or signals
/// processes outside the sandbox.
pub const FORWARDED: &[&str] = &[
    "job.start",
    "job.retry",
    "job.cancel",
    "agent.start",
    "errand.start",
    "review.start",
    "crew.resume",
    "verify.run",
    "workspace.acquire",
    "workspace.recover",
    "workspace.release",
    "resource.status",
];

/// How long a request waits for a dispatcher to claim it.
const CLAIM_WINDOW_MS: i64 = 30_000;
/// Requests a home holds before they are claimed or finished.
const IN_FLIGHT: i64 = 16;
/// Launchers a dispatcher runs at once for one home that have not yet
/// claimed their request. One that claimed it carries out its command for as
/// long as that takes and holds no slot: `IN_FLIGHT` bounds those.
const LAUNCHERS: usize = 4;
/// Finished requests are kept this long, and at most `KEEP_ROWS` of them.
const KEEP_MS: i64 = 7 * 24 * 3600 * 1000;
const KEEP_ROWS: i64 = 1000;
/// A launcher marks its row this often while its command runs; a waiter
/// that sees no mark for `STALE_MS` stops waiting.
const HEARTBEAT: Duration = Duration::from_secs(2);
const STALE_MS: i64 = 15_000;
const POLL: Duration = Duration::from_millis(100);
const LAUNCHER_POLL: Duration = Duration::from_millis(250);
/// Only caller context crosses from the requester into the launcher's
/// environment. Configuration, executable lookup and loader settings belong
/// to the dispatcher, not to an untrusted process inside a sandbox.
const REQUESTER_ENV: &[&str] = &["PRIVATEER_ROLE", "PRIVATEER_ACTOR", "PRIVATEER_JOB"];

/// Set in a `_launch-run` process: it carries out requests and never makes one.
static LAUNCHER: AtomicBool = AtomicBool::new(false);

/// Whether a sandboxed caller hands `command` to the dispatcher.
pub fn forwarded(command: &str) -> bool {
    FORWARDED.contains(&command)
}

/// Why this process's launches on `home` go through the home's dispatcher, if
/// they do: `pid_namespace` (Linux: another PID namespace than the one the
/// dispatcher recorded), `seatbelt` (a macOS sandbox) or `forced`
/// (`PRIVATEER_LAUNCH_VIA=dispatcher`). A false positive costs a round trip
/// and gives the same answer, so this may be generous.
pub fn sandbox(home: &Path) -> Result<Option<&'static str>> {
    if LAUNCHER.load(Ordering::Relaxed) {
        return Ok(None);
    }
    if std::env::var_os("PRIVATEER_LAUNCH_VIA").is_some_and(|v| v == "dispatcher") {
        return Ok(Some("forced"));
    }
    #[cfg(target_os = "macos")]
    if seatbelt() || std::env::var_os("CODEX_SANDBOX").is_some_and(|v| v == "seatbelt") {
        return Ok(Some("seatbelt"));
    }
    if foreign_namespace(home) {
        return Ok(Some("pid_namespace"));
    }
    Ok(None)
}

/// Whether this process is in another PID namespace than the one the home's
/// dispatcher recorded. A home whose dispatcher never ran records none, and
/// a home this process cannot read is not judged here.
fn foreign_namespace(home: &Path) -> bool {
    let Some(own) = crate::jobs::pid_namespace() else {
        return false;
    };
    let Ok(conn) = Connection::open_with_flags(
        home.join("privateer.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return false;
    };
    let _ = conn.busy_timeout(Duration::from_secs(5));
    conn.query_row(
        "SELECT value FROM metadata WHERE key='pid_namespace'",
        [],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .is_some_and(|dispatcher| dispatcher != own)
}

/// Whether this process runs in a Seatbelt sandbox, by libsystem_sandbox's
/// `sandbox_check` with no operation. A missing symbol is no sandbox.
#[cfg(target_os = "macos")]
fn seatbelt() -> bool {
    identify::seatbelt_state(std::process::id()) == Some(true)
}

/// What a requester took from its environment and flags, as a direct call
/// would: inputs to caller resolution, never its result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claimed {
    pub caller: Caller,
    pub env_job: Option<String>,
    pub fleet: Option<PathBuf>,
    pub sandbox: String,
}

impl Claimed {
    fn to_json(&self) -> Value {
        let (role, actor) = match &self.caller {
            Caller::Operator => ("operator", None),
            Caller::Agent { actor } => ("agent", Some(actor.as_str())),
        };
        json!({"role": role, "actor": actor, "env_job": self.env_job,
            "fleet": self.fleet, "sandbox": self.sandbox})
    }

    fn from_json(value: &Value) -> Result<Self> {
        let caller = match value["role"].as_str() {
            Some("operator") => Caller::Operator,
            Some("agent") => Caller::Agent {
                actor: value["actor"]
                    .as_str()
                    .context("an agent request names its actor")?
                    .to_owned(),
            },
            _ => bail!("a launch request names a role"),
        };
        Ok(Self {
            caller,
            env_job: value["env_job"].as_str().map(str::to_owned),
            fleet: value["fleet"].as_str().map(PathBuf::from),
            sandbox: value["sandbox"].as_str().unwrap_or("forced").to_owned(),
        })
    }
}

fn ensure_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS launch_requests(id TEXT PRIMARY KEY, command TEXT NOT NULL,
           args TEXT NOT NULL, requester TEXT NOT NULL, claimed TEXT NOT NULL, cwd TEXT,
           state TEXT NOT NULL, created_ms INTEGER NOT NULL, deadline_ms INTEGER NOT NULL,
           launcher TEXT, resolved TEXT, outcome TEXT, error TEXT, claimed_ms INTEGER,
           heartbeat_ms INTEGER, finished_ms INTEGER);
         CREATE INDEX IF NOT EXISTS launch_requests_state ON launch_requests(state, created_ms);",
    )?;
    Ok(())
}

#[derive(Clone, Debug)]
struct Row {
    id: String,
    command: String,
    args: Value,
    requester: Value,
    claimed: Value,
    cwd: Option<String>,
    state: String,
    created_ms: i64,
    launcher: Option<String>,
    resolved: Option<Value>,
    outcome: Option<Value>,
    error: Option<String>,
    claimed_ms: Option<i64>,
    heartbeat_ms: Option<i64>,
    finished_ms: Option<i64>,
}

fn load(conn: &Connection, id: &str) -> Result<Option<Row>> {
    let raw = conn
        .query_row(
            "SELECT id, command, args, requester, claimed, cwd, state, created_ms, launcher,
               resolved, outcome, error, claimed_ms, heartbeat_ms, finished_ms
             FROM launch_requests WHERE id=?1",
            [id],
            |r| {
                Ok((
                    (
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, i64>(7)?,
                    ),
                    (
                        r.get::<_, Option<String>>(8)?,
                        r.get::<_, Option<String>>(9)?,
                        r.get::<_, Option<String>>(10)?,
                        r.get::<_, Option<String>>(11)?,
                        r.get::<_, Option<i64>>(12)?,
                        r.get::<_, Option<i64>>(13)?,
                        r.get::<_, Option<i64>>(14)?,
                    ),
                ))
            },
        )
        .optional()?;
    let Some((
        (id, command, args, requester, claimed, cwd, state, created_ms),
        (launcher, resolved, outcome, error, claimed_ms, heartbeat_ms, finished_ms),
    )) = raw
    else {
        return Ok(None);
    };
    let parse = |raw: Option<String>| raw.map(|r| serde_json::from_str(&r)).transpose();
    Ok(Some(Row {
        id,
        command,
        args: serde_json::from_str(&args)?,
        requester: serde_json::from_str(&requester)?,
        claimed: serde_json::from_str(&claimed)?,
        cwd,
        state,
        created_ms,
        launcher,
        resolved: parse(resolved)?,
        outcome: parse(outcome)?,
        error,
        claimed_ms,
        heartbeat_ms,
        finished_ms,
    }))
}

/// The digest a waiter's argv binds: the row's command, arguments, claim and
/// directory as the requester recorded them. A row rewritten after that is
/// no longer its waiter's request.
fn digest(command: &str, args: &Value, claimed: &Value, cwd: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    let content = json!([command, args, claimed, cwd]).to_string();
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

fn row_digest(row: &Row) -> String {
    digest(&row.command, &row.args, &row.claimed, row.cwd.as_deref())
}

/// The ids a launch would draw for itself, drawn before the request so the
/// work can be found by them whatever happens to the answer.
fn fill_ids(command: &str, args: &mut Value) {
    let key = match command {
        "job.start" | "agent.start" => "id",
        "job.retry" => "new_id",
        _ => return,
    };
    if let Some(object) = args.as_object_mut() {
        if object.get(key).is_none_or(Value::is_null) {
            object.insert(key.into(), json!(uuid::Uuid::new_v4().to_string()));
        }
    }
}

/// How the refusals name the requester's sandbox.
fn sandbox_phrase(sandbox: &str) -> &'static str {
    match sandbox {
        "seatbelt" => "a macOS sandbox",
        "pid_namespace" => "a sandbox (another PID namespace than the home's dispatcher)",
        _ => "a sandbox",
    }
}

/// The refusal for a request no dispatcher took: nothing was started.
fn unclaimed(command: &str, sandbox: &str, why: &str) -> String {
    format!(
        "{command} was not started: this caller runs inside {}, so the home's dispatcher starts its work, and {why}. No work was started, so retrying the same command is safe. This is a limit of the sandbox, not a missing Privateer permission: start the dispatcher (privateer service start) or run the command outside the sandbox.",
        sandbox_phrase(sandbox)
    )
}

fn no_dispatcher(command: &str, sandbox: &str, home: &Path) -> String {
    unclaimed(
        command,
        sandbox,
        &format!("no dispatcher is running for {}", home.display()),
    )
}

fn not_claimed(command: &str, sandbox: &str, window_ms: i64) -> String {
    unclaimed(
        command,
        sandbox,
        &format!(
            "no dispatcher took the request within {} s",
            (window_ms + 999) / 1000
        ),
    )
}

fn unconfirmed(command: &str, reason: &str) -> String {
    format!(
        "{command} was not started: the dispatcher could not confirm the requesting process from outside its sandbox ({reason}). No work was started, so retrying the same command is safe. This is a limit of the sandbox, not a missing Privateer permission: run the command outside the sandbox."
    )
}

/// How `contained` names a proven job whose record this home cannot load.
const UNREADABLE: &str =
    "under a record the dispatcher cannot load from the home that proved it (it may be read-only)";

fn contained_refusal(command: &str, job: &str, what: &str) -> String {
    format!(
        "{command} was not started: job {job} runs {what}, and its sandbox stays closed: Privateer does not start work outside it. This is a limit of that job's sandbox, not a missing Privateer permission."
    )
}

fn stopped(row: &Row) -> String {
    let ids: Vec<String> = ["id", "new_id"]
        .iter()
        .filter_map(|key| row.args[*key].as_str())
        .map(|id| format!("job {id}"))
        .collect();
    let found = if ids.is_empty() {
        String::new()
    } else {
        format!(
            " {} may have started: read it with job.status.",
            ids.join(" or ")
        )
    };
    format!(
        "the dispatcher's launcher for request {} stopped before recording the outcome of {}.{found} Check before retrying.",
        row.id, row.command
    )
}

/// Record `command` as a launch request and exec into its waiter. Returns
/// only an error: the request could not be recorded or the waiter not
/// started. `precheck` is the caller this process resolved (or, where its
/// sandbox hid the ancestry, claimed); `check_dispatch` runs as it, as a
/// quick refusal before anything is written. The launcher checks again.
pub fn forward(
    home: &Path,
    command: &str,
    mut args: Value,
    precheck: Caller,
    claimed: Claimed,
) -> Result<std::convert::Infallible> {
    ensure!(args.is_object(), "command arguments must be a JSON object");
    fill_ids(command, &mut args);
    let store = Store::open_as(home, precheck)?;
    crate::activation::mark_contact(&store);
    crate::check_dispatch(&store, command, &args)?;
    let home = store.home().to_owned();
    let conn = store.connection();
    ensure_table(conn)?;
    let requester = identify::own()?;
    let id = uuid::Uuid::new_v4().to_string();
    let now = Store::now_ms();
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|d| d.to_str().map(str::to_owned));
    let claimed = claimed.to_json();
    let bound = digest(command, &args, &claimed, cwd.as_deref());
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let in_flight: i64 = tx.query_row(
        "SELECT COUNT(*) FROM launch_requests WHERE state IN ('recorded','pending','claimed')",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        in_flight < IN_FLIGHT,
        "{command} was not started: {in_flight} launch requests from sandboxed callers are already in flight for this home, the most it holds. No work was started; retry once they finish."
    );
    tx.execute(
        "INSERT INTO launch_requests(id, command, args, requester, claimed, cwd, state, created_ms, deadline_ms)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, 'recorded', ?7, ?8)",
        params![
            id,
            command,
            args.to_string(),
            requester.to_string(),
            claimed.to_string(),
            cwd,
            now,
            now + CLAIM_WINDOW_MS
        ],
    )?;
    tx.commit()?;
    use std::os::unix::process::CommandExt;
    let error = Command::new(std::env::current_exe().context("find this executable")?)
        .arg("--home")
        .arg(&home)
        .arg("_launch-wait")
        .arg(&id)
        .arg(&bound)
        .exec();
    let text = format!(
        "{command} was not started: could not wait for the dispatcher ({error}). No work was started, so retrying the same command is safe."
    );
    let _ = conn.execute(
        "UPDATE launch_requests SET state='failed', error=?2, finished_ms=?3 WHERE id=?1 AND state='recorded'",
        params![id, text, Store::now_ms()],
    );
    bail!("{text}")
}

/// Withdraw an unclaimed request; false if a launcher claimed it first.
fn withdraw(conn: &Connection, id: &str, text: &str) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE launch_requests SET state='expired', error=?2, finished_ms=?3
         WHERE id=?1 AND state IN ('recorded','pending')",
        params![id, text, Store::now_ms()],
    )? == 1)
}

/// The requester's own claim window: `PRIVATEER_LAUNCH_CLAIM_MS` may shorten
/// it (tests), never lengthen it.
fn claim_window() -> i64 {
    std::env::var("PRIVATEER_LAUNCH_CLAIM_MS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|ms| (1..CLAIM_WINDOW_MS).contains(ms))
        .unwrap_or(CLAIM_WINDOW_MS)
}

/// `_launch-wait ID DIGEST`: the requester after its exec. Binds the row to
/// this process, waits a bounded time for a dispatcher to claim it, then for
/// its outcome, which it returns as the command's answer or error.
pub fn wait(home: &Path, id: &str, bound: &str) -> Result<Value> {
    let store = Store::open(home)?;
    let conn = store.connection();
    ensure_table(conn)?;
    let row = load(conn, id)?.with_context(|| format!("no launch request {id}"))?;
    ensure!(
        identify::same_process(&row.requester, &identify::own()?),
        "launch request {id} belongs to another process"
    );
    let sandbox = Claimed::from_json(&row.claimed)?.sandbox;
    if row_digest(&row) != bound {
        let text = unconfirmed(
            &row.command,
            &format!("request {id} was changed after this process recorded it"),
        );
        if withdraw(conn, id, &text)? {
            bail!("{text}");
        }
    }
    conn.execute(
        "UPDATE launch_requests SET state='pending' WHERE id=?1 AND state='recorded'",
        [id],
    )?;
    if !crate::service::running(store.home())? {
        let text = no_dispatcher(&row.command, &sandbox, store.home());
        if withdraw(conn, id, &text)? {
            bail!("{text}");
        }
    }
    let window = claim_window();
    let deadline = row.created_ms + window;
    loop {
        let row = load(conn, id)?.with_context(|| format!("launch request {id} is gone"))?;
        let now = Store::now_ms();
        match row.state.as_str() {
            "recorded" | "pending" if now >= deadline => {
                let text = not_claimed(&row.command, &sandbox, window);
                if withdraw(conn, id, &text)? {
                    bail!("{text}");
                }
            }
            "claimed" => {
                let seen = row
                    .heartbeat_ms
                    .or(row.claimed_ms)
                    .unwrap_or(row.created_ms);
                if now - seen > STALE_MS {
                    bail!("{}", stopped(&row));
                }
            }
            "done" => return Ok(row.outcome.unwrap_or(Value::Null)),
            "failed" | "refused" | "expired" => {
                bail!("{}", row.error.clone().unwrap_or_else(|| stopped(&row)))
            }
            _ => {}
        }
        std::thread::sleep(POLL);
    }
}

/// The result a launcher records.
enum Carried {
    Done(Value),
    Failed(String),
    Refused(String),
}

/// `_launch-run ID`: carry out one request from outside the sandbox. Never
/// resolves its own ancestry, which is the dispatcher's.
pub fn run(home: &Path, id: &str) -> Result<()> {
    LAUNCHER.store(true, Ordering::Relaxed);
    let store = Store::open(home)?;
    let home = store.home().to_owned();
    // A launcher must never start work into a PID sandbox or a Seatbelt one.
    crate::jobs::ensure_dispatcher_namespace(&store)?;
    #[cfg(target_os = "macos")]
    ensure!(
        !seatbelt(),
        "a launcher inside a macOS sandbox would start work into it"
    );
    let conn = store.connection();
    ensure_table(conn)?;
    let me = crate::jobs::own_identity()?;
    let now = Store::now_ms();
    if conn.execute(
        "UPDATE launch_requests SET state='claimed', launcher=?2, claimed_ms=?3, heartbeat_ms=?3
         WHERE id=?1 AND state='pending' AND deadline_ms > ?3",
        params![id, me, now],
    )? != 1
    {
        return Ok(());
    }
    let row = load(conn, id)?.context("launch request vanished")?;
    let result = carry_out(&home, conn, &row, &me);
    let (state, outcome, error) = match result {
        Ok(Carried::Done(value)) => ("done", Some(value.to_string()), None),
        Ok(Carried::Failed(text)) => ("failed", None, Some(text)),
        Ok(Carried::Refused(text)) => ("refused", None, Some(text)),
        Err(error) => ("failed", None, Some(error.to_string())),
    };
    conn.execute(
        "UPDATE launch_requests SET state=?3, outcome=?4, error=?5, finished_ms=?6
         WHERE id=?1 AND state='claimed' AND launcher=?2",
        params![id, me, state, outcome, error, Store::now_ms()],
    )?;
    Ok(())
}

fn carry_out(home: &Path, conn: &Connection, row: &Row, me: &str) -> Result<Carried> {
    let claimed = Claimed::from_json(&row.claimed)?;
    let found = match identify::requester(&row.requester, home, &row.id, &row_digest(row)) {
        Ok(found) => found,
        Err(error) => {
            return Ok(Carried::Refused(unconfirmed(
                &row.command,
                &error.to_string(),
            )));
        }
    };
    #[cfg(feature = "test-launch-gap")]
    test_gap(row, &found);
    // A sandboxed requester whose ancestry never leaves its sandbox was not
    // started by anything outside it (a double fork out of its job's tree):
    // the walk could not tell its job, and its claim never stands in. It is
    // refused before its caller is resolved; a requester that exited
    // meanwhile is reported as that.
    if let Err(error) = identify::ensure_leaves_sandbox(&found, &claimed.sandbox) {
        let reason = match identify::ensure_still(&row.requester, &found) {
            Err(gone) => gone,
            Ok(()) => error,
        };
        return Ok(Carried::Refused(unconfirmed(
            &row.command,
            &reason.to_string(),
        )));
    }
    // Use the environment a direct launch from the dispatcher would inherit,
    // with only caller context from the kernel's view of the requester. Its
    // session, PATH, loaders and runtime configuration must not control the
    // unsandboxed launcher. The cwd is kernel-checked; an explicit registry
    // flag remains a command input, as on the direct path.
    adopt_environment(&found.env);
    heartbeat(home, &row.id, me);
    let _ = std::env::set_current_dir(found.cwd.as_deref().unwrap_or(home));
    if let Some(fleet) = &claimed.fleet {
        crate::fleet::use_registry(fleet.clone())?;
    }
    // A requester that has gone is refused by the walk itself, and its
    // `PRIVATEER_JOB` is the kernel's copy of its environment, now this
    // process's own.
    let resolved = crate::crew_scope::resolve_from(
        found.pid,
        Some(home),
        claimed.caller.clone(),
        crate::crew_scope::env_job(),
    );
    // The walk proves the requester's caller only if the requester is still
    // that process: one that exited, or whose pid was reused, while it ran
    // proves nothing, whatever the walk returned.
    if let Err(error) = identify::ensure_still(&row.requester, &found) {
        return Ok(Carried::Refused(unconfirmed(
            &row.command,
            &error.to_string(),
        )));
    }
    let caller = match resolved {
        Ok(caller) => caller,
        Err(error) => return Ok(Carried::Refused(error.to_string())),
    };
    let job = crate::crew_scope::job_id();
    let crew = crate::crew_scope::crew();
    let resolved = json!({
        "actor": match &caller { Caller::Agent { actor } => Some(actor.as_str()), Caller::Operator => None },
        "job": job, "crew": crew.as_ref().map(|c| c.crew.clone()),
    });
    conn.execute(
        "UPDATE launch_requests SET resolved=?2 WHERE id=?1",
        params![row.id, resolved.to_string()],
    )?;
    let mut store = Store::open_as(home, caller)?;
    // The walk's job, and the crew a job record names, are proven; a job
    // only `PRIVATEER_JOB` names is not, and a direct call tolerates one
    // that does not exist.
    let walked = crate::crew_scope::proven_job();
    let jobs: Vec<(String, bool)> = job
        .iter()
        .map(|id| (id.clone(), walked.as_ref() == Some(id)))
        .chain(crew.as_ref().map(|c| (c.crew.clone(), true)))
        .collect();
    let proven_home = crate::crew_scope::proven_home();
    // A visible `_job-supervise` argv names the home it proves in, and from
    // outside every argv is visible, so a home is trusted only when the
    // caller could name it anyway: this one, or one the real registry lists.
    let listed = match &proven_home {
        Some(_) => crate::crew_scope::world_homes(Some(home)),
        None => vec![],
    };
    if let Some((id, what)) = containment(&store, proven_home.as_deref(), &listed, &jobs)? {
        return Ok(Carried::Refused(contained_refusal(&row.command, &id, what)));
    }
    store.audit(
        "launch.forwarded",
        &json!({"request": row.id, "command": row.command, "requester": found.pid,
            "sandbox": claimed.sandbox, "resolved": resolved}),
    )?;
    Ok(
        match crate::dispatch(&mut store, &row.command, row.args.clone()) {
            Ok(value) => Carried::Done(value),
            Err(error) => Carried::Failed(error.to_string()),
        },
    )
}

/// Whether the caller's job keeps its sandbox closed: a review errand, or a
/// read-only launch. A `proven` job whose record this home cannot load is
/// refused: it may be read-only (in another home, or in a record that no
/// longer parses), and an unknown containment is never read as none.
fn contained(store: &Store, job: &str, proven: bool) -> Result<Option<&'static str>> {
    match crate::review::is_review_errand(store, job) {
        Ok(true) => return Ok(Some("as a read-only review errand")),
        Ok(false) => {}
        Err(_) if proven => return Ok(Some(UNREADABLE)),
        Err(error) => return Err(error),
    }
    let record = match crate::jobs::record(store, job) {
        Ok(record) => record,
        Err(_) if proven => return Ok(Some(UNREADABLE)),
        Err(_) => return Ok(None),
    };
    if record["codex_read_only_crew"] == json!(true)
        || record["launch"]["options"]["sandbox"] == json!("read-only")
    {
        return Ok(Some("read-only"));
    }
    Ok(None)
}

/// The first of the caller's `jobs` (id, proven) that keeps its sandbox
/// closed, each judged from the home that holds its record: a proven job,
/// and the crew classified with it, from `proven_home`, the home the walk
/// proved it in, which may be another ship's than this one; any other job
/// from this home. A proven home is believed only when it is this home or
/// one of `listed` (canonical: this home and those the real registry
/// lists): the walk takes a visible supervisor's home from its argv, which
/// a requester can forge with a process of its own pointed at a database it
/// wrote. A proven home that is not listed, or cannot be opened, is an
/// unknown containment, never none.
fn containment(
    store: &Store,
    proven_home: Option<&Path>,
    listed: &[PathBuf],
    jobs: &[(String, bool)],
) -> Result<Option<(String, &'static str)>> {
    let elsewhere = proven_home
        .filter(|home| *home != store.home())
        .map(|home| {
            let canonical = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_owned());
            ensure!(
                listed.contains(&canonical),
                "home {} is neither this home nor one the fleet registry lists",
                home.display()
            );
            Store::open_readonly(home, Caller::Operator)
        });
    for (id, proven) in jobs {
        let what = match (&elsewhere, proven) {
            (Some(Ok(other)), true) => contained(other, id, true)?,
            (Some(Err(_)), true) => Some(UNREADABLE),
            _ => contained(store, id, *proven)?,
        };
        if let Some(what) = what {
            return Ok(Some((id.clone(), what)));
        }
    }
    Ok(None)
}

/// Rebuild from the dispatcher's direct-launch baseline, allowing only
/// caller context from the requester. A duplicate keeps its first value,
/// just as `getenv` does. Missing requester context never falls back to the
/// dispatcher's own role or job. No requester configuration is adopted.
fn environment_block(
    dispatcher: &[(OsString, OsString)],
    requester: &[(OsString, OsString)],
) -> Vec<CString> {
    use std::os::unix::ffi::OsStrExt;
    let context = |name: &OsString| {
        REQUESTER_ENV
            .iter()
            .any(|s| s.as_bytes() == name.as_bytes())
    };
    let baseline = dispatcher.iter().filter(|(name, _)| !context(name));
    let caller = requester.iter().filter(|(name, _)| context(name));
    let mut seen = HashSet::with_capacity(dispatcher.len());
    let mut block = Vec::with_capacity(dispatcher.len());
    for (name, value) in baseline.chain(caller) {
        let bytes = name.as_bytes();
        if bytes.is_empty()
            || bytes.contains(&b'=')
            || bytes.starts_with(b"__MISE_")
            // Strip known Codex session/tool-call and sandbox markers, not
            // all CODEX_* names: HOME, CA_CERTIFICATE, API_KEY and future
            // dispatcher-owned configuration must remain available. The
            // sandbox prefix covers its network and platform variants.
            || matches!(
                bytes,
                b"CODEX_SESSION_ID"
                    | b"CODEX_THREAD_ID"
                    | b"CODEX_TOOL_CALL_ID"
                    | b"CODEX_TURN_ID"
                    | b"CODEX_CI"
                    | b"CODEX_VERSION"
                    | b"CODEX_INTERNAL_ORIGINATOR_OVERRIDE"
            )
            || bytes.starts_with(b"CODEX_SANDBOX")
            || bytes == b"PRIVATEER_LAUNCH_VIA"
            || !seen.insert(bytes)
        {
            continue;
        }
        let path;
        let value = if bytes == b"PATH" {
            path = launch_path(value);
            path.as_slice()
        } else {
            value.as_bytes()
        };
        let mut entry = Vec::with_capacity(bytes.len() + value.len() + 1);
        entry.extend_from_slice(bytes);
        entry.push(b'=');
        entry.extend_from_slice(value);
        if let Ok(entry) = CString::new(entry) {
            block.push(entry);
        }
    }
    block
}

/// Remove Codex's ephemeral arg0 executable directories without interpreting
/// the requester's HOME or touching the filesystem. Work on Unix bytes so a
/// non-UTF-8 PATH cannot bypass the filter. Keep other entries and their order.
fn launch_path(value: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    let mut path = Vec::with_capacity(value.len());
    let mut first = true;
    for entry in value.as_bytes().split(|b| *b == b':') {
        let components: Vec<_> = Path::new(std::ffi::OsStr::from_bytes(entry))
            .components()
            .map(|part| part.as_os_str().as_bytes())
            .collect();
        let arg0 = components.windows(3).any(|parts| {
            (parts[0] == b".codex" && parts[1] == b"tmp" && parts[2].starts_with(b"arg0"))
                || (parts[0] == b"tmp"
                    && parts[1].starts_with(b"arg0")
                    && parts[2].starts_with(b"codex-arg0"))
        });
        if arg0 {
            continue;
        }
        if !first {
            path.push(b':');
        }
        first = false;
        path.extend_from_slice(entry);
    }
    path
}

/// Replace this process's environment with the rebuilt launch block in one
/// pass: the C library's `environ` is pointed at a new array, where `setenv`
/// per variable would scan the environment each time. The array is never
/// freed: a launcher adopts once.
fn adopt_environment(env: &[(OsString, OsString)]) {
    let dispatcher: Vec<_> = std::env::vars_os().collect();
    let mut block: Vec<*mut libc::c_char> = environment_block(&dispatcher, env)
        .into_iter()
        .map(CString::into_raw)
        .collect();
    block.push(std::ptr::null_mut());
    let block = Box::leak(block.into_boxed_slice()).as_mut_ptr();
    // SAFETY: a launcher is single-threaded until `heartbeat`, which starts
    // after this, so nothing reads or writes the environment meanwhile.
    // `block` is a NULL-terminated array of NUL-terminated `NAME=value`
    // strings that lives for the rest of the process, as `environ` must.
    unsafe { *environ() = block };
}

/// The C library's `environ`, which `getenv`, `setenv` and the standard
/// library's environment and process spawning all read.
#[cfg(target_os = "macos")]
fn environ() -> *mut *mut *mut libc::c_char {
    // SAFETY: `_NSGetEnviron` has no preconditions.
    unsafe { libc::_NSGetEnviron() }
}

#[cfg(not(target_os = "macos"))]
fn environ() -> *mut *mut *mut libc::c_char {
    unsafe extern "C" {
        static mut environ: *mut *mut libc::c_char;
    }
    &raw mut environ
}

/// Tests only: hold a launcher between identifying its requester and
/// resolving its caller, the gap a requester that exits must not slip
/// through. `PRIVATEER_TEST_LAUNCH_GAP` names a directory: a request whose
/// `id` argument is J, while `J.hold` exists there, writes the requester's
/// pid here to `J.identified` and waits until `J.hold` is gone, for at most
/// 60 s.
#[cfg(feature = "test-launch-gap")]
fn test_gap(row: &Row, found: &identify::Found) {
    let (Some(dir), Some(job)) = (
        std::env::var_os("PRIVATEER_TEST_LAUNCH_GAP"),
        row.args["id"].as_str(),
    ) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let hold = dir.join(format!("{job}.hold"));
    if !hold.exists() {
        return;
    }
    let _ = std::fs::write(dir.join(format!("{job}.identified")), found.pid.to_string());
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while hold.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Mark the row while the command runs, so its waiter can tell a launcher
/// that died from one that is still working.
fn heartbeat(home: &Path, id: &str, me: &str) {
    let (database, id, me) = (home.join("privateer.sqlite"), id.to_owned(), me.to_owned());
    std::thread::spawn(move || {
        let Ok(conn) = Connection::open(&database) else {
            return;
        };
        let _ = conn.busy_timeout(Duration::from_secs(5));
        loop {
            std::thread::sleep(HEARTBEAT);
            let marked = conn.execute(
                "UPDATE launch_requests SET heartbeat_ms=?3 WHERE id=?1 AND state='claimed' AND launcher=?2",
                params![id, me, Store::now_ms()],
            );
            if !matches!(marked, Ok(1)) {
                return;
            }
        }
    });
}

/// Start the resident dispatcher's launcher thread for `home`: every 250 ms
/// it starts a `_launch-run` for each claimable request, at most `LAUNCHERS`
/// at once, expires requests past their window, fails claimed ones whose
/// launcher died without a result, and prunes finished ones.
pub(crate) fn start_launcher(home: PathBuf) {
    std::thread::spawn(move || {
        let mut running = Launchers::default();
        let mut conn: Option<Connection> = None;
        loop {
            if conn.is_none() {
                conn = Connection::open(home.join("privateer.sqlite"))
                    .and_then(|c| c.busy_timeout(Duration::from_secs(5)).map(|()| c))
                    .ok();
            }
            if let Some(c) = &conn {
                if let Err(error) = launcher_pass(&home, c, &mut running) {
                    eprintln!("privateer: launch requests: {error:#}");
                    conn = None;
                }
            }
            std::thread::sleep(LAUNCHER_POLL);
        }
    });
}

/// Launchers that may not have claimed their request yet, and how many
/// were started for each pending request.
#[derive(Default)]
struct Launchers {
    running: HashMap<String, Child>,
    attempts: HashMap<String, u32>,
}

/// Launchers started for one request that ended without claiming it.
const ATTEMPTS: u32 = 3;

fn launcher_pass(home: &Path, conn: &Connection, launchers: &mut Launchers) -> Result<()> {
    launchers
        .running
        .retain(|_, child| matches!(child.try_wait(), Ok(None)));
    ensure_table(conn)?;
    let now = Store::now_ms();
    let unclaimed: Vec<(String, String, String)> = {
        let mut statement = conn.prepare(
            "SELECT id, command, claimed FROM launch_requests
             WHERE state IN ('recorded','pending') AND deadline_ms <= ?1",
        )?;
        statement
            .query_map([now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    for (id, command, claimed) in unclaimed {
        let sandbox = serde_json::from_str::<Value>(&claimed)
            .ok()
            .and_then(|c| c["sandbox"].as_str().map(str::to_owned))
            .unwrap_or_default();
        withdraw(conn, &id, &not_claimed(&command, &sandbox, CLAIM_WINDOW_MS))?;
    }
    let claimed: Vec<(String, String)> = {
        let mut statement = conn.prepare(
            "SELECT id, launcher FROM launch_requests WHERE state='claimed' AND launcher IS NOT NULL",
        )?;
        statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    for (id, launcher) in claimed {
        if launchers.running.contains_key(&id)
            || crate::jobs::identity_alive(&launcher).unwrap_or(true)
        {
            continue;
        }
        if let Some(row) = load(conn, &id)? {
            conn.execute(
                "UPDATE launch_requests SET state='failed', error=?3, finished_ms=?4
                 WHERE id=?1 AND state='claimed' AND launcher=?2",
                params![id, launcher, stopped(&row), now],
            )?;
        }
    }
    conn.execute(
        "DELETE FROM launch_requests WHERE state IN ('done','failed','refused','expired')
           AND (finished_ms < ?1 OR id NOT IN (SELECT id FROM launch_requests
             WHERE state IN ('done','failed','refused','expired')
             ORDER BY finished_ms DESC LIMIT ?2))",
        params![now - KEEP_MS, KEEP_ROWS],
    )?;
    let pending: Vec<String> = {
        let mut statement = conn.prepare(
            "SELECT id FROM launch_requests WHERE state='pending' AND deadline_ms > ?1
             ORDER BY created_ms",
        )?;
        statement
            .query_map([now], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    launchers.attempts.retain(|id, _| pending.contains(id));
    // Only launchers that have not claimed yet hold a slot: four commands
    // that block (`wait_for_capacity`) never starve a later `job.cancel`.
    let mut unclaimed = launchers
        .running
        .keys()
        .filter(|id| pending.contains(id))
        .count();
    for id in pending {
        if unclaimed >= LAUNCHERS {
            break;
        }
        if launchers.running.contains_key(&id) {
            continue;
        }
        let attempts = launchers.attempts.entry(id.clone()).or_default();
        if *attempts >= ATTEMPTS {
            if let Some(row) = load(conn, &id)? {
                conn.execute(
                    "UPDATE launch_requests SET state='failed', error=?2, finished_ms=?3
                     WHERE id=?1 AND state='pending'",
                    params![
                        id,
                        format!(
                            "{} was not started: the home's dispatcher could not start a launcher for request {id}. No work was started, so retrying the same command is safe.",
                            row.command
                        ),
                        now
                    ],
                )?;
            }
            continue;
        }
        *attempts += 1;
        let child = spawn_launcher(home, &id)?;
        launchers.running.insert(id, child);
        unclaimed += 1;
    }
    Ok(())
}

fn spawn_launcher(home: &Path, id: &str) -> Result<Child> {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new(crate::executable::supervisor()?);
    command
        .arg("--home")
        .arg(home)
        .arg("_launch-run")
        .arg(id)
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe; the launcher leaves the
    // dispatcher's session so a restart does not end it.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command.spawn()?)
}

/// The argument keys of each command here; see `crate::Accepted`.
pub(crate) const ACCEPTED: crate::Accepted = &[("launch.status", &["id"])];

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "launch.status" => status(store, &args),
        _ => bail!("unknown launch command {command}"),
    }
}

/// `launch.status {id}`: a request's state and its answer, for the caller
/// it was carried out as (a crew: its own requests only).
fn status(store: &Store, args: &Value) -> Result<Value> {
    let id = args["id"].as_str().context("id required")?;
    let conn = store.connection();
    ensure_table(conn)?;
    let row = load(conn, id)?.with_context(|| format!("no launch request {id}"))?;
    let claimed = Claimed::from_json(&row.claimed)?;
    let resolved = row.resolved.clone().unwrap_or(Value::Null);
    let visible = match (store.crew(), store.caller()) {
        (Some(crew), _) => {
            resolved["job"].as_str() == Some(crew.job.as_str())
                || resolved["crew"].as_str() == Some(crew.crew.as_str())
        }
        (None, Caller::Operator) => true,
        (None, Caller::Agent { actor }) => match resolved["actor"].as_str() {
            Some(proven) => proven == actor,
            None => resolved.is_null() && claimed.caller == store.caller().clone(),
        },
    };
    ensure!(visible, "launch request {id} is not this caller's");
    Ok(json!({
        "id": row.id, "command": row.command, "state": row.state,
        "outcome": row.outcome, "error": row.error, "created_ms": row.created_ms,
        "claimed_ms": row.claimed_ms, "finished_ms": row.finished_ms,
        "requester_pid": row.requester["pid"], "launcher_alive": row.launcher.as_deref()
            .map(|l| crate::jobs::identity_alive(l).unwrap_or(true)),
    }))
}

#[cfg(test)]
mod tests;
