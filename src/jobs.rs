//! Durable, bounded Unix jobs and a shared per-host admission ledger.
//!
//! This is process supervision, not a privileged sandbox. Unknown/orphaned
//! payloads keep their reservation until their recorded process group is gone.
use crate::admission::{self, Demand, Estimate, Refused, refuse_unless};
use crate::store::{Caller, Store};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

const LOG_LIMIT: usize = 1024 * 1024;
/// A log keeps its first bytes (where harnesses announce their session) and
/// its most recent bytes (the latest activity and the final usage), in two
/// rotating tail segments; the middle of a longer log is dropped.
const LOG_HEAD: usize = LOG_LIMIT / 4;
const LOG_SEGMENT: usize = (LOG_LIMIT - LOG_HEAD) / 2;
const START_GRACE_MS: i64 = 30_000;
const DEFAULT_MAX_WAIT_SECS: u64 = 3600;
const QUEUE_POLL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Process {
    pid: u32,
    started: String,
    #[serde(default)]
    zombie: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Job {
    id: String,
    task_id: String,
    incarnation: String,
    command: Vec<String>,
    cwd: PathBuf,
    /// The caller's directory before mapping into its task worktree. Retained
    /// so replaying a completed job ID never acquires a replacement lease.
    #[serde(default)]
    source_cwd: Option<PathBuf>,
    #[serde(default)]
    workspace_lease_id: Option<String>,
    #[serde(default)]
    agent_request_hash: Option<String>,
    action: String,
    heavy: bool,
    timeout_secs: u64,
    ledger: PathBuf,
    status: String,
    created_ms: i64,
    finished_ms: Option<i64>,
    supervisor: Option<Process>,
    payload: Option<Process>,
    cancel_requested: bool,
    exit_code: Option<i32>,
    reason: Option<String>,
    retry_of: Option<String>,
    #[serde(default)]
    group_anchors: Vec<Process>,
    #[serde(default)]
    plugin_binding: Option<Value>,
    #[serde(default)]
    cleanup_descendants: bool,
    /// Bytes dropped from the middle of each log to keep it bounded.
    #[serde(default, skip_serializing_if = "is_zero")]
    stdout_dropped_bytes: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    stderr_dropped_bytes: u64,
    /// When set, the payload runs as this agent (PRIVATEER_ROLE=agent).
    #[serde(default)]
    agent_actor: Option<String>,
    /// Requested resource estimate, if any; see admission.
    #[serde(default)]
    estimate: Option<Estimate>,
    /// Wait in the host capacity queue instead of being refused, for at
    /// most `max_wait_secs`.
    #[serde(default)]
    wait_for_capacity: bool,
    #[serde(default)]
    max_wait_secs: Option<u64>,
    /// Outcome the job settles to once descendants left by an exited leader
    /// are gone. Absent when the supervisor vanished before recording one.
    #[serde(default)]
    leader_result: Option<LeaderResult>,
    /// Who asked to cancel: the agent's name, or "operator".
    #[serde(default)]
    cancelled_by: Option<String>,
    /// The supervised job whose agent started this one, when an agent did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    started_by: Option<String>,
    #[serde(default)]
    work_offer: Option<String>,
    /// Crew hosted in the terminal host: the supervisor runs in its own
    /// space and the harness gets that pane's terminal instead of log pipes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    hosted: bool,
    /// A harness job's `harness/provider/model`, for provider backoff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_route: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct LeaderResult {
    status: String,
    reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Reservation {
    key: String,
    home: PathBuf,
    job_id: String,
    incarnation: String,
    heavy: bool,
    state: String,
    created_ms: i64,
    supervisor: Option<Process>,
    payload: Option<Process>,
    #[serde(default)]
    charter: Option<String>,
    /// Resolved estimate counted by admission while recently admitted.
    #[serde(default)]
    demand: Option<Demand>,
    /// A first mate's wake, which a fleet stow does not hold back: the
    /// first mate must wake to stow.
    #[serde(default)]
    wake: bool,
}

/// A job waiting for capacity, first come first served across homes.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Queued {
    key: String,
    home: PathBuf,
    job_id: String,
    incarnation: String,
    created_ms: i64,
    supervisor: Option<Process>,
}
pub fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS jobs (id TEXT PRIMARY KEY, record TEXT NOT NULL);",
    )?;
    Ok(())
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 120
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        && id != "."
        && id != ".."
}
fn load(store: &Store, id: &str) -> Result<Job> {
    let raw: String = store
        .connection()
        .query_row("SELECT record FROM jobs WHERE id=?1", [id], |r| r.get(0))
        .with_context(|| format!("unknown job {id}"))?;
    Ok(serde_json::from_str(&raw)?)
}
/// Change a job record. Every path that ends a job comes through here, so
/// the first change to a reported status also writes the crew outcome, in
/// the same transaction: the status and its outcome commit together.
fn alter(store: &Store, id: &str, f: impl FnOnce(&mut Job)) -> Result<Job> {
    let conn = store.connection();
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        let mut job = load(store, id)?;
        let before = job.status.clone();
        f(&mut job);
        conn.execute(
            "UPDATE jobs SET record=?2 WHERE id=?1",
            params![id, serde_json::to_string(&job)?],
        )?;
        if reported(&job.status) && !reported(&before) {
            crate::crew::outcome(store, &public_job(&job))?;
        }
        Ok(job)
    })();
    if result.is_ok() {
        conn.execute_batch("COMMIT")?;
    } else {
        let _ = conn.execute_batch("ROLLBACK");
    }
    result
}
fn config(store: &Store) -> Result<Value> {
    let path = store.home().join("config.json");
    if path.exists() {
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    } else {
        Ok(json!({}))
    }
}
/// Where the shared resource ledger lives, without creating anything. A
/// sandboxed first mate must be able to write there to start crew jobs.
pub(crate) fn ledger_dir(store: &Store) -> Result<PathBuf> {
    let cfg = config(store)?;
    Ok(
        if let Some(p) = cfg.get("resource_home").and_then(Value::as_str) {
            PathBuf::from(p)
        } else if let Ok(p) = std::env::var("PRIVATEER_RESOURCE_HOME") {
            PathBuf::from(p)
        } else {
            PathBuf::from(
                std::env::var("HOME").context("HOME required for shared resource ledger")?,
            )
            .join(".local/state/privateer-host")
        },
    )
}
/// Create (if needed) and validate the ledger directory; return the ledger.
pub(crate) fn ensure_ledger(store: &Store) -> Result<PathBuf> {
    ledger_path(store)
}
fn ledger_path(store: &Store) -> Result<PathBuf> {
    let path = ledger_dir(store)?;
    ensure!(path.is_absolute(), "resource_home must be absolute");
    crate::store::reject_symlinks(&path)?;
    fs::create_dir_all(&path)?;
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "resource_home must be a real directory"
    );
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "resource_home must belong to this user"
    );
    ensure!(
        metadata.mode() & 0o022 == 0,
        "resource_home may not be writable by other users"
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    Ok(fs::canonicalize(path)?.join("resources.sqlite"))
}
fn ledger(path: &Path) -> Result<Connection> {
    if path.exists() {
        ensure!(
            !fs::symlink_metadata(path)?.file_type().is_symlink(),
            "resource ledger may not be a symlink"
        );
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS reservations (key TEXT PRIMARY KEY, record TEXT NOT NULL);",
    )?;
    admission::init(&conn)?;
    crate::routes::init(&conn)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS queue (key TEXT PRIMARY KEY, record TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS stows (id TEXT PRIMARY KEY, started_ms INTEGER NOT NULL,
           resumed_ms INTEGER, ships TEXT NOT NULL);",
    )?;
    Ok(conn)
}
/// The host's resource ledger, where a fleet stow holds new jobs.
pub(crate) fn host_ledger(store: &Store) -> Result<Connection> {
    ledger(&ledger_path(store)?)
}
/// The fleet stow holding new jobs on this host, if one is in force.
pub(crate) fn stowed(conn: &Connection) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT id FROM stows WHERE resumed_ms IS NULL ORDER BY started_ms DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?)
}
/// Whether a job is a first mate's wake: `serve` records the wake, with its
/// job id, before starting it.
fn is_wake(store: &Store, id: &str) -> Result<bool> {
    Ok(store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM wakes WHERE job_id=?1)",
        [id],
        |r| r.get(0),
    )?)
}
fn reservations(conn: &Connection) -> Result<Vec<Reservation>> {
    let mut stmt = conn.prepare("SELECT record FROM reservations ORDER BY key")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.map(|raw| Ok(serde_json::from_str(&raw?)?)).collect()
}
fn reservation_key(home: &Path, id: &str, incarnation: &str) -> String {
    format!("{}:{id}:{incarnation}", home.display())
}
/// Queue entries in arrival order, dropping those whose supervisor is gone (or
/// never appeared within the start grace) so they cannot block the line.
fn live_queue(conn: &Connection) -> Result<Vec<Queued>> {
    let mut statement = conn.prepare("SELECT record FROM queue")?;
    let mut entries: Vec<Queued> = statement
        .query_map([], |r| r.get::<_, String>(0))?
        .map(|raw| Ok(serde_json::from_str(&raw?)?))
        .collect::<Result<_>>()?;
    entries.sort_by(|a, b| (a.created_ms, &a.key).cmp(&(b.created_ms, &b.key)));
    let mut live = vec![];
    for entry in entries {
        let alive = match &entry.supervisor {
            Some(p) => same_process(p)?,
            None => Store::now_ms() - entry.created_ms <= START_GRACE_MS,
        };
        if alive {
            live.push(entry);
        } else {
            conn.execute("DELETE FROM queue WHERE key=?1", [&entry.key])?;
        }
    }
    Ok(live)
}
fn dequeue(store: &Store, job: &Job) -> Result<()> {
    ledger(&job.ledger)?.execute(
        "DELETE FROM queue WHERE key=?1",
        [reservation_key(store.home(), &job.id, &job.incarnation)],
    )?;
    Ok(())
}
/// Admit and record a reservation under the ledger's write lock, so homes
/// sharing the host decide one at a time against the same readings.
///
/// A request from the queue must be at its head; a direct request may not
/// jump a non-empty queue. Either way an unfitting request is `Refused`.
fn reserve(
    conn: &Connection,
    reservation: &mut Reservation,
    estimate: Option<&Estimate>,
    charter_cap: Option<u64>,
    from_queue: bool,
) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        // A stowed fleet starts no new work until the captain resumes it;
        // a job waiting for capacity keeps its place in the queue.
        if !reservation.wake {
            if let Some(stow) = stowed(conn)? {
                refuse_unless!(
                    false,
                    "the fleet is stowed ({stow}) while the captain maintains it; new jobs start after `privateer stow resume`"
                );
            }
        }
        if reservation.heavy || estimate.is_some() {
            let queue = live_queue(conn)?;
            let ahead = queue
                .iter()
                .take_while(|q| q.key != reservation.key)
                .count();
            if from_queue {
                refuse_unless!(ahead == 0, "waiting behind {ahead} earlier jobs");
            } else {
                refuse_unless!(
                    queue.is_empty(),
                    "{} jobs are waiting for capacity; pass wait_for_capacity:true to join the queue",
                    queue.len()
                );
            }
            // Never garbage-collect reservations merely because a wrapper disappeared.
            let held = reservations(conn)?;
            let settings = admission::settings(conn)?;
            let host = admission::measure()?;
            reservation.demand = admission::demand(&settings, &host, reservation.heavy, estimate);
            let view: Vec<admission::Held> = held
                .iter()
                .map(|r| admission::Held {
                    heavy: r.heavy,
                    home: &r.home,
                    charter: r.charter.as_deref(),
                    created_ms: r.created_ms,
                    demand: r.demand,
                })
                .collect();
            admission::admit(
                &settings,
                &host,
                Store::now_ms(),
                &view,
                &admission::Request {
                    heavy: reservation.heavy,
                    home: &reservation.home,
                    charter: reservation.charter.as_deref().unwrap_or(""),
                    charter_cap,
                    demand: reservation.demand,
                },
            )?;
            if from_queue {
                conn.execute("DELETE FROM queue WHERE key=?1", [&reservation.key])?;
                // Its ramp starts now, not when it joined the queue.
                reservation.created_ms = Store::now_ms();
            }
        }
        conn.execute(
            "INSERT INTO reservations(key,record) VALUES(?1,?2)",
            params![reservation.key, serde_json::to_string(reservation)?],
        )?;
        Ok(())
    })();
    if result.is_ok() {
        conn.execute_batch("COMMIT")?;
    } else {
        let _ = conn.execute_batch("ROLLBACK");
    }
    result
}
fn update_reservation(store: &Store, job: &Job) -> Result<()> {
    let conn = ledger(&job.ledger)?;
    let key = reservation_key(store.home(), &job.id, &job.incarnation);
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<()> {
        let existing: Option<String> = conn
            .query_row(
                "SELECT record FROM reservations WHERE key=?1",
                [&key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(raw) = existing {
            let mut r: Reservation = serde_json::from_str(&raw)?;
            r.supervisor = job.supervisor.clone().or(r.supervisor);
            r.payload = job.payload.clone().or(r.payload);
            if job.status != "starting" || r.state == "starting" {
                r.state = job.status.clone();
            }
            conn.execute(
                "UPDATE reservations SET record=?2 WHERE key=?1",
                params![key, serde_json::to_string(&r)?],
            )?;
        }
        Ok(())
    })();
    if result.is_ok() {
        conn.execute_batch("COMMIT")?;
    } else {
        let _ = conn.execute_batch("ROLLBACK");
    }
    result
}
fn release(store: &Store, job: &Job) -> Result<()> {
    let conn = ledger(&job.ledger)?;
    conn.execute(
        "DELETE FROM reservations WHERE key=?1",
        [reservation_key(store.home(), &job.id, &job.incarnation)],
    )?;
    Ok(())
}
/// Start identity must not depend on the caller's environment: status, cancel
/// and recovery may run from a shell, SSH session or service with another
/// TZ/locale than the supervisor that recorded it.
#[cfg(target_os = "linux")]
fn identity(pid: u32) -> Result<Option<Process>> {
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        // A process reaped between open and read reports ESRCH: it is gone too.
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ESRCH) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error).context("inspect process identity"),
    };
    // The executable comm may contain spaces/parentheses. Fields after its
    // final ')' start at field 3 (state); kernel start ticks are field 22.
    let fields: Vec<&str> = stat
        .rsplit_once(')')
        .context("invalid kernel process identity")?
        .1
        .split_whitespace()
        .collect();
    let state = fields.first().context("kernel process state missing")?;
    let ticks = fields.get(19).context("kernel process start missing")?;
    // Start ticks count from boot; the boot id keeps them unique across reboots.
    // It is constant until reboot; supervisors poll identity, so read it once.
    static BOOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let boot = match BOOT.get() {
        Some(boot) => boot,
        None => {
            let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .context("inspect boot identity")?;
            BOOT.get_or_init(|| boot.trim().to_owned())
        }
    };
    Ok(Some(Process {
        pid,
        started: format!("linux:{boot}:{ticks}"),
        zombie: matches!(*state, "Z" | "X"),
    }))
}
#[cfg(not(target_os = "linux"))]
fn identity(pid: u32) -> Result<Option<Process>> {
    // lstart is rendered in local time and locale; pin both.
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart=", "-o", "stat="])
        .env("TZ", "UTC0")
        .env("LC_ALL", "C")
        .output()
        .context("inspect process identity")?;
    let raw = String::from_utf8(output.stdout)?;
    let fields: Vec<&str> = raw.split_whitespace().collect();
    let zombie = fields.last().is_some_and(|stat| stat.starts_with('Z'));
    let started = if fields.len() > 1 {
        fields[..fields.len() - 1].join(" ")
    } else {
        String::new()
    };
    // ps returns 1 for a missing PID on Linux and macOS; all other failures refuse.
    ensure!(
        output.status.success() || output.status.code() == Some(1),
        "process inspection failed"
    );
    if started.is_empty() {
        return Ok(None);
    }
    Ok(Some(Process {
        pid,
        started,
        zombie,
    }))
}
/// Compare a recorded start identity with a freshly observed one.
fn same_start(recorded: &str, current: &str) -> bool {
    if recorded == current {
        return true;
    }
    // Records from 0.1.0 used "<local lstart>|ticks:N"; their kernel ticks
    // still identify the process, the environment-dependent prefix does not.
    #[cfg(target_os = "linux")]
    if let (Some((_, old)), Some((_, new))) =
        (recorded.rsplit_once("|ticks:"), current.rsplit_once(':'))
    {
        return old == new;
    }
    false
}
/// Observe whether our child has exited, without reaping it: Some(success)
/// once it is a zombie. Until reaped, the zombie keeps its PID, and therefore
/// its process-group id, reserved, so the group can still be signalled.
pub(crate) fn exited_unreaped(pid: u32) -> Result<Option<bool>> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    ensure!(
        rc == 0,
        "cannot inspect child process: {}",
        std::io::Error::last_os_error()
    );
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    Ok(Some(
        info.si_code == libc::CLD_EXITED && unsafe { info.si_status() } == 0,
    ))
}
fn leader_exited(pid: u32) -> Result<bool> {
    Ok(exited_unreaped(pid)?.is_some())
}
fn same_process(process: &Process) -> Result<bool> {
    Ok(identity(process.pid)?
        .is_some_and(|p| !p.zombie && same_start(&process.started, &p.started)))
}
fn group_members(group: u32) -> Result<Vec<u32>> {
    let output = Command::new("ps")
        .args(["-eo", "pid=,pgid=,stat="])
        .output()?;
    ensure!(output.status.success(), "process group inspection failed");
    let mut members = vec![];
    for line in String::from_utf8(output.stdout)?.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 3
            && fields[1].parse::<u32>().ok() == Some(group)
            && !fields[2].starts_with('Z')
        {
            members.push(fields[0].parse()?);
        }
    }
    Ok(members)
}
fn group_alive(process: &Process) -> Result<bool> {
    // A live original leader anchors identity. If it vanished, an orphaned
    // group still occupies this PGID; conservatively preserve that capacity.
    if let Some(current) = identity(process.pid)? {
        // POSIX forbids reusing a PID that names a live process group until
        // that group's lifetime ends. A different process holding the leader
        // PID therefore proves the recorded group is gone; any group now using
        // that id belongs to the new process.
        if !same_start(&process.started, &current.started) {
            return Ok(false);
        }
    }
    Ok(!group_members(process.pid)?.is_empty())
}
const UNPROVEN_LEADER: &str = "payload leader missing or changed; cannot discover group ownership";
/// Snapshot live payload group members. Returns None when the leader cannot
/// currently be proven to own the group (for example caught mid-exit);
/// inspection failures remain errors.
///
/// `reserved` means the caller holds the leader as an exited but unreaped
/// child (observed with waitid), which by itself keeps the PID and process
/// group id from being reused, so the leader's identity is not re-inspected.
fn capture_anchors(process: &Process, reserved: bool) -> Result<Option<Vec<Process>>> {
    if !reserved {
        // macOS getpgid reports ESRCH for zombies. An unreaped original PID
        // with matching start identity still prevents PGID reuse here.
        let owned = identity(process.pid)?.is_some_and(|leader| {
            same_start(&process.started, &leader.started)
                && (leader.zombie
                    || unsafe { libc::getpgid(process.pid as i32) } == process.pid as i32)
        });
        if !owned {
            return Ok(None);
        }
    }
    let mut anchors = vec![process.clone()];
    for pid in group_members(process.pid)?.into_iter().take(256) {
        if let Some(p) = identity(pid)? {
            if !p.zombie {
                anchors.push(p);
            }
        }
    }
    Ok(Some(anchors))
}
fn signal_group(process: &Process, signal: i32, anchors: &[Process]) -> Result<()> {
    // A still-live recorded member anchors this exact group even if its
    // leader exits during TERM grace. Never signal solely by stale PGID.
    let mut anchored = false;
    for anchor in std::iter::once(process).chain(anchors.iter()) {
        if same_process(anchor)?
            && unsafe { libc::getpgid(anchor.pid as i32) } == process.pid as i32
        {
            anchored = true;
            break;
        }
    }
    ensure!(
        anchored,
        "payload leader/recorded members missing or changed; refusing stale process-group signal"
    );
    let rc = unsafe { libc::kill(-(process.pid as i32), signal) };
    ensure!(
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
        "cannot signal payload group"
    );
    Ok(())
}
#[allow(clippy::unnecessary_cast)] // statvfs field widths differ across supported Unix targets.
pub(crate) fn free_bytes(path: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    ensure!(
        unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } == 0,
        "cannot inspect disk availability"
    );
    let stat = unsafe { stat.assume_init() };
    Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}
fn terminal(status: &str) -> bool {
    matches!(status, "succeeded" | "failed" | "cancelled" | "timed_out")
}
/// Statuses a crew outcome reports. An orphan is no longer supervised (its
/// deadline is not enforced), so its first mate hears about it then; the
/// settlement that may follow is not reported again.
fn reported(status: &str) -> bool {
    terminal(status) || status == "orphaned"
}
fn public_job(job: &Job) -> Value {
    let mut v = serde_json::to_value(job).unwrap_or(Value::Null);
    v["log_limit_bytes"] = json!(LOG_LIMIT);
    v
}
fn check_plugin_binding(job: &Job, charter: &Value) -> Result<()> {
    if let Some(binding) = &job.plugin_binding {
        ensure!(
            charter.get("provider_plugin") == Some(binding),
            "provider plugin binding changed; fresh validated planning required"
        );
    }
    Ok(())
}
fn check_workspace_owner(store: &Store, job: &Job) -> Result<()> {
    if job.workspace_lease_id.is_some() {
        if let Some(actor) = &job.agent_actor {
            ensure!(
                store.task(&job.task_id)?["owner"].as_str() == Some(actor),
                "task custody changed; the old workspace worker must stop"
            );
        }
    }
    Ok(())
}
/// Only the validated agent.start path may start a harness under spawn.
/// Public job.start cannot use this to bypass implementation authority.
pub(crate) fn start_agent(store: &Store, mut args: Value) -> Result<Value> {
    args["action"] = json!("spawn");
    start(store, args, None, true)
}

/// Replays of an agent request are historical facts, including after release.
/// Compare the original request before allocating or re-planning a workspace.
pub(crate) fn agent_replay(store: &Store, args: &Value, hash: &str) -> Result<Option<Value>> {
    let Some(id) = args.get("id").and_then(Value::as_str) else {
        return Ok(None);
    };
    let raw: Option<String> = store
        .connection()
        .query_row("SELECT record FROM jobs WHERE id=?1", [id], |r| r.get(0))
        .optional()?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let existing: Job = serde_json::from_str(&raw)?;
    if existing.agent_request_hash.is_none() {
        return Ok(None);
    }
    ensure!(
        existing.action == "spawn"
            && existing.agent_request_hash.as_deref() == Some(hash)
            && args["task_id"].as_str() == Some(&existing.task_id),
        "job id already belongs to different work"
    );
    let owner = store.task(&existing.task_id)?["owner"]
        .as_str()
        .context("task owner missing")?
        .to_owned();
    if let Caller::Agent { actor } = store.caller() {
        ensure!(actor == &owner, "agent can only replay its own task jobs");
    }
    let charter = store.authorize(&existing.task_id, "spawn")?;
    check_plugin_binding(&existing, &charter)?;
    Ok(Some(public_job(&existing)))
}

/// Reconcile recorded groups before release; a dead supervisor is not proof
/// that its payload stopped.
pub(crate) fn workspace_idle(store: &Store, task: &str, lease: &str) -> Result<()> {
    workspace_jobs_idle(store, task, lease, None)
}

pub(crate) fn workspace_previous_owner_idle(store: &Store, task: &str, lease: &str) -> Result<()> {
    let owner = store.task(task)?["owner"]
        .as_str()
        .context("task owner missing")?
        .to_owned();
    workspace_jobs_idle(store, task, lease, Some(&owner))
}

fn workspace_jobs_idle(
    store: &Store,
    task: &str,
    lease: &str,
    current_owner: Option<&str>,
) -> Result<()> {
    let mut stmt = store.connection().prepare("SELECT record FROM jobs WHERE json_extract(record,'$.task_id')=?1 AND json_extract(record,'$.workspace_lease_id')=?2")?;
    let raw = stmt.query_map(params![task, lease], |r| r.get::<_, String>(0))?;
    let jobs: Vec<Job> = raw
        .map(|r| Ok(serde_json::from_str(&r?)?))
        .collect::<Result<_>>()?;
    let mut reserved = Reserved::default();
    for job in jobs {
        if current_owner.is_some_and(|owner| {
            job.agent_actor
                .as_deref()
                .is_none_or(|actor| actor == owner)
        }) {
            continue;
        }
        let job = reconcile(store, job, &mut reserved)?;
        ensure!(
            terminal(&job.status),
            "workspace has an unfinished job {}; stop or settle it before release",
            job.id
        );
        ensure!(
            job.supervisor.as_ref().map(same_process).transpose()? != Some(true),
            "workspace supervisor is still running"
        );
        ensure!(
            job.payload.as_ref().map(group_alive).transpose()? != Some(true),
            "workspace payload group is still running"
        );
    }
    Ok(())
}

fn start(store: &Store, args: Value, retry_of: Option<String>, agent: bool) -> Result<Value> {
    ensure!(
        args.get("production").and_then(Value::as_bool) != Some(true),
        "production execution is not supported by the ordinary job runner"
    );
    let task_id = args
        .get("task_id")
        .and_then(Value::as_str)
        .context("task_id required")?;
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("implement");
    ensure!(
        matches!(action, "implement" | "test" | "recovery" | "cleanup")
            || (agent && action == "spawn"),
        "ordinary jobs require implement/test/recovery/cleanup action"
    );
    let charter = store.authorize(task_id, action)?;
    let plugin_binding = args.get("plugin_binding").cloned();
    if let Some(binding) = &plugin_binding {
        ensure!(
            !binding.is_null() && charter.get("provider_plugin") == Some(binding),
            "provider plugin binding changed; fresh validated planning required"
        );
    }
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    ensure!(valid_id(&id), "invalid job id");
    let command: Vec<String> = serde_json::from_value(
        args.get("command")
            .cloned()
            .context("command array required")?,
    )?;
    ensure!(
        !command.is_empty() && !command[0].is_empty(),
        "command must name an executable"
    );
    ensure!(
        command.iter().all(|s| !s.contains('\0')),
        "command contains NUL"
    );
    let repo = fs::canonicalize(
        charter
            .get("repo")
            .and_then(Value::as_str)
            .context("charter repo missing")?,
    )?;
    let requested_cwd = fs::canonicalize(
        args.get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| repo.clone()),
    )?;
    let heavy = args.get("heavy").and_then(Value::as_bool).unwrap_or(true);
    let estimate = args
        .get("estimate")
        .filter(|v| !v.is_null())
        .map(admission::estimate)
        .transpose()?;
    let wait_for_capacity = args
        .get("wait_for_capacity")
        .map(|v| v.as_bool().context("wait_for_capacity must be boolean"))
        .transpose()?
        .unwrap_or(false);
    let max_wait_secs = match args.get("max_wait_secs").filter(|v| !v.is_null()) {
        Some(v) => Some(
            v.as_u64()
                .filter(|s| (1..=86_400).contains(s))
                .context("max_wait_secs must be 1..86400")?,
        ),
        None if wait_for_capacity => Some(DEFAULT_MAX_WAIT_SECS),
        None => None,
    };
    ensure!(
        wait_for_capacity || max_wait_secs.is_none(),
        "max_wait_secs requires wait_for_capacity"
    );
    let cleanup_descendants = args
        .get("cleanup_descendants")
        .map(|value| {
            value
                .as_bool()
                .context("cleanup_descendants must be boolean")
        })
        .transpose()?
        .unwrap_or(false);
    let requested_actor = args
        .get("agent_actor")
        .filter(|v| !v.is_null())
        .map(|v| v.as_str().context("agent_actor must be a string"))
        .transpose()?;
    if let Some(actor) = requested_actor {
        crate::store::identifier(actor)?;
    }
    // Work started by an agent stays agent work, for the agent's own tasks.
    let agent_actor = match store.caller() {
        Caller::Agent { actor } => {
            ensure!(
                store.task(task_id)?["owner"] == actor.as_str(),
                "agent {actor} can only start work on tasks it owns"
            );
            ensure!(
                requested_actor.is_none_or(|a| a == actor),
                "agent {actor} cannot launch work as another actor"
            );
            Some(actor.clone())
        }
        Caller::Operator => requested_actor.map(str::to_owned),
    };
    // An agent inside a supervised job (crew, or a first mate's wake) is named
    // by PRIVATEER_JOB; remember which of its jobs started this one.
    let started_by = match (store.caller(), &agent_actor) {
        (Caller::Agent { .. }, Some(actor)) => {
            std::env::var("PRIVATEER_JOB").ok().filter(|parent| {
                load(store, parent).is_ok_and(|p| p.agent_actor.as_deref() == Some(actor))
            })
        }
        _ => None,
    };
    let expected_workspace = args
        .get("workspace_lease_id")
        .map(|v| v.as_str().context("workspace_lease_id must be a string"))
        .transpose()?;
    let agent_request_hash = if agent {
        args.get("agent_request_hash")
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .context("agent_request_hash must be text")
            })
            .transpose()?
    } else {
        None
    };
    if let Ok(existing) = load(store, &id) {
        ensure!(
            existing.task_id == task_id
                && existing.command == command
                && existing.source_cwd.as_ref().unwrap_or(&existing.cwd) == &requested_cwd
                && expected_workspace
                    .is_none_or(|lease| existing.workspace_lease_id.as_deref() == Some(lease))
                && existing.heavy == heavy
                && existing.action == action
                && existing.plugin_binding == plugin_binding
                && existing.cleanup_descendants == cleanup_descendants
                && existing.agent_actor == agent_actor
                && existing.estimate == estimate
                && existing.wait_for_capacity == wait_for_capacity,
            "job id already belongs to different work"
        );
        return Ok(public_job(&existing));
    }
    ensure!(
        requested_cwd.starts_with(&repo)
            || crate::workspace::contains_cwd(store, task_id, &repo, &requested_cwd)?,
        "cwd must stay inside charter repository or its task workspace"
    );
    let workspace = crate::workspace::execution(
        store,
        task_id,
        &repo,
        &requested_cwd,
        !(agent && crate::workspace::is_wake(store, &args)?),
        expected_workspace,
    )?;
    let cwd = if requested_cwd.starts_with(&workspace.root) {
        requested_cwd.clone()
    } else if requested_cwd == repo {
        workspace.root.clone()
    } else {
        workspace.root.join(
            requested_cwd
                .strip_prefix(&workspace.source_root)
                .context("cwd must stay inside charter repository or its task workspace")?,
        )
    };
    let cwd = fs::canonicalize(cwd)?;
    ensure!(
        cwd.is_dir() && cwd.starts_with(&workspace.root),
        "cwd must stay inside charter repository or its task workspace"
    );
    let workspace_lease_id = workspace.lease_id.clone();
    let charter_timeout = charter
        .get("job_timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(900);
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(charter_timeout);
    ensure!(
        timeout_secs > 0 && timeout_secs <= charter_timeout,
        "job timeout exceeds charter"
    );
    let min_free = charter
        .get("min_free_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(2 * 1024 * 1024 * 1024);
    let available = free_bytes(&cwd)?;
    ensure!(
        available >= min_free,
        "disk admission refused: {available} available, {min_free} required"
    );
    // An optional per-charter share of the host's heavy work.
    let charter_cap = charter.get("max_heavy_jobs").and_then(Value::as_u64);
    let executable = crate::executable::supervisor()?;
    let ledger_path = ledger_path(store)?;
    let mut job = Job {
        id: id.clone(),
        task_id: task_id.to_owned(),
        incarnation: Uuid::new_v4().to_string(),
        command,
        cwd,
        source_cwd: Some(requested_cwd),
        workspace_lease_id,
        agent_request_hash,
        action: action.to_owned(),
        heavy,
        timeout_secs,
        ledger: ledger_path.clone(),
        status: "starting".to_owned(),
        created_ms: Store::now_ms(),
        finished_ms: None,
        supervisor: None,
        payload: None,
        cancel_requested: false,
        exit_code: None,
        reason: None,
        retry_of,
        group_anchors: vec![],
        plugin_binding,
        cleanup_descendants,
        stdout_dropped_bytes: 0,
        stderr_dropped_bytes: 0,
        agent_actor,
        estimate: estimate.clone(),
        wait_for_capacity,
        max_wait_secs,
        leader_result: None,
        cancelled_by: None,
        started_by,
        work_offer: args
            .get("work_offer")
            .and_then(Value::as_str)
            .map(str::to_owned),
        hosted: agent && args.get("hosted").and_then(Value::as_bool) == Some(true),
        model_route: args
            .get("model_route")
            .and_then(Value::as_str)
            .filter(|_| agent)
            .map(str::to_owned),
    };
    let mut reservation = Reservation {
        key: reservation_key(store.home(), &id, &job.incarnation),
        home: store.home().to_owned(),
        job_id: id.clone(),
        incarnation: job.incarnation.clone(),
        heavy,
        state: "starting".to_owned(),
        created_ms: job.created_ms,
        supervisor: None,
        payload: None,
        charter: charter.get("id").and_then(Value::as_str).map(str::to_owned),
        demand: None,
        wake: is_wake(store, &id)?,
    };
    let conn = store.connection();
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let admitted = (|| -> Result<Option<Job>> {
        let current = store.authorize(task_id, action)?;
        ensure!(
            fs::canonicalize(current["repo"].as_str().context("charter repo missing")?)? == repo,
            "charter repository changed during workspace allocation"
        );
        crate::scheduling::admit_job(
            store,
            task_id,
            &id,
            job.started_by.as_deref(),
            job.work_offer.as_deref(),
        )?;
        check_plugin_binding(&job, &current)?;
        check_workspace_owner(store, &job)?;
        let existing: Option<String> = conn
            .query_row("SELECT record FROM jobs WHERE id=?1", [&id], |r| r.get(0))
            .optional()?;
        if let Some(raw) = existing {
            let existing: Job = serde_json::from_str(&raw)?;
            ensure!(
                existing.task_id == task_id
                    && existing.command == job.command
                    && existing.cwd == job.cwd
                    && existing.source_cwd == job.source_cwd
                    && existing.workspace_lease_id == job.workspace_lease_id
                    && existing.agent_request_hash == job.agent_request_hash
                    && existing.heavy == heavy
                    && existing.action == action
                    && existing.plugin_binding == job.plugin_binding
                    && existing.cleanup_descendants == job.cleanup_descendants
                    && existing.agent_actor == job.agent_actor
                    && existing.estimate == job.estimate
                    && existing.wait_for_capacity == job.wait_for_capacity
                    && existing.hosted == job.hosted
                    && existing.work_offer == job.work_offer,
                "job id already belongs to different work"
            );
            return Ok(Some(existing));
        }
        if job.action == "spawn" && job.workspace_lease_id.is_some() {
            let occupied: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM jobs WHERE json_extract(record,'$.workspace_lease_id')=?1
                 AND json_extract(record,'$.action')='spawn'
                 AND json_extract(record,'$.status') IN ('starting','queued','launching','running','orphaned'))",
                [&job.workspace_lease_id], |r| r.get(0),
            )?;
            ensure!(
                !occupied,
                "this task already has a workspace harness; create a child task for an independent coding worker"
            );
        }
        let ledger = ledger(&ledger_path)?;
        match reserve(
            &ledger,
            &mut reservation,
            estimate.as_ref(),
            charter_cap,
            false,
        ) {
            Ok(()) => {}
            // No capacity now: join the queue; the supervisor waits there.
            Err(error) if wait_for_capacity && error.downcast_ref::<Refused>().is_some() => {
                let entry = Queued {
                    key: reservation.key.clone(),
                    home: reservation.home.clone(),
                    job_id: id.clone(),
                    incarnation: job.incarnation.clone(),
                    created_ms: job.created_ms,
                    supervisor: None,
                };
                ledger.execute(
                    "INSERT INTO queue(key,record) VALUES(?1,?2)",
                    params![entry.key, serde_json::to_string(&entry)?],
                )?;
                job.status = "queued".into();
                job.reason = Some(format!("waiting for capacity: {error}"));
            }
            Err(error) => return Err(error),
        }
        if let Err(error) = conn.execute(
            "INSERT INTO jobs(id,record) VALUES(?1,?2)",
            params![id, serde_json::to_string(&job)?],
        ) {
            release(store, &job)?;
            dequeue(store, &job)?;
            return Err(error.into());
        }
        Ok(None)
    })();
    if admitted.is_ok() {
        if let Err(error) = conn.execute_batch("COMMIT") {
            let _ = conn.execute_batch("ROLLBACK");
            release(store, &job)?;
            dequeue(store, &job)?;
            return Err(error.into());
        }
    } else {
        let _ = conn.execute_batch("ROLLBACK");
    }
    if let Some(existing) = admitted? {
        return Ok(public_job(&existing));
    }
    if job.hosted {
        // The supervisor claims the job from its own space; a space that never
        // runs it leaves the job unsupervised, and reconciliation fails it.
        if let Err(error) = crate::host::supervise(
            store,
            &job.id,
            &job.task_id,
            job.agent_actor.as_deref().unwrap_or_default(),
            &job.cwd,
        ) {
            job = alter(store, &id, |j| {
                j.status = "failed".into();
                j.finished_ms = Some(Store::now_ms());
                j.reason = Some(format!("terminal host refused the crew: {error:#}"));
            })?;
            release(store, &job)?;
            dequeue(store, &job)?;
            return Err(error).context("open the crew's space in the terminal host");
        }
        store.audit(
            "job.started",
            &json!({"job_id": id, "task_id": task_id, "incarnation": job.incarnation, "hosted": true}),
        )?;
        return Ok(public_job(&job));
    }
    let mut cmd = Command::new(&executable);
    cmd.arg("--home")
        .arg(store.home())
        .arg("_job-supervise")
        .arg(&id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(mut child) => {
            // The supervisor also records its own identity; parent must not
            // overwrite a payload/status already written by that supervisor.
            let process = identity(child.id())?;
            // Long-lived RPC callers must reap completed supervisors; CLI
            // callers can still exit and the detached child is adopted.
            thread::spawn(move || {
                let _ = child.wait();
            });
            job = alter(store, &id, |j| {
                if j.supervisor.is_none() {
                    j.supervisor = process;
                }
            })?;
            update_reservation(store, &job)?;
        }
        Err(error) => {
            job = alter(store, &id, |j| {
                j.status = "failed".into();
                j.finished_ms = Some(Store::now_ms());
                j.reason = Some(format!(
                    "supervisor spawn failed using {}: {error}",
                    executable.display()
                ));
            })?;
            release(store, &job)?;
            dequeue(store, &job)?;
            return Err(error).with_context(|| {
                format!("spawn Privateer supervisor using {}", executable.display())
            });
        }
    }
    store.audit(
        "job.started",
        &json!({"job_id": id, "task_id": task_id, "incarnation": job.incarnation}),
    )?;
    Ok(public_job(&job))
}

/// Wait at the capacity queue until admitted (Some, now launching) or until
/// cancellation, lost authority or max_wait_secs ends the job (None). No
/// payload exists while a job is queued.
fn wait_in_queue(store: &Store, mut job: Job) -> Result<Option<Job>> {
    let key = reservation_key(store.home(), &job.id, &job.incarnation);
    // Other homes judge the entry's liveness by this supervisor.
    {
        let conn = ledger(&job.ledger)?;
        let raw: Option<String> = conn
            .query_row("SELECT record FROM queue WHERE key=?1", [&key], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(raw) = raw {
            let mut entry: Queued = serde_json::from_str(&raw)?;
            entry.supervisor = job.supervisor.clone();
            conn.execute(
                "UPDATE queue SET record=?2 WHERE key=?1",
                params![key, serde_json::to_string(&entry)?],
            )?;
        }
    }
    let max_wait = job.max_wait_secs.unwrap_or(DEFAULT_MAX_WAIT_SECS);
    let deadline = job.created_ms + (max_wait as i64) * 1000;
    let end = |job: &Job, status: &str, reason: String| -> Result<Option<Job>> {
        let status = status.to_owned();
        let ended = alter(store, &job.id, |j| {
            j.status = status;
            j.finished_ms = Some(Store::now_ms());
            j.reason = Some(reason);
        })?;
        dequeue(store, &ended)?;
        store.audit(
            "job.finished",
            &json!({"job_id": ended.id, "status": ended.status, "exit_code": null}),
        )?;
        Ok(None)
    };
    loop {
        job = load(store, &job.id)?;
        if job.cancel_requested {
            return end(
                &job,
                "cancelled",
                "cancelled while waiting for capacity".into(),
            );
        }
        let charter = match store
            .authorize(&job.task_id, &job.action)
            .and_then(|c| check_plugin_binding(&job, &c).map(|()| c))
        {
            Ok(charter) => charter,
            Err(error) => {
                return end(
                    &job,
                    "failed",
                    format!("authority ended while waiting: {error}"),
                );
            }
        };
        if Store::now_ms() >= deadline {
            let last = job.reason.clone().unwrap_or_default();
            return end(
                &job,
                "failed",
                format!("no capacity within {max_wait} s ({last})"),
            );
        }
        let mut reservation = Reservation {
            key: key.clone(),
            home: store.home().to_owned(),
            job_id: job.id.clone(),
            incarnation: job.incarnation.clone(),
            heavy: job.heavy,
            state: "starting".to_owned(),
            created_ms: Store::now_ms(),
            supervisor: job.supervisor.clone(),
            payload: None,
            charter: charter.get("id").and_then(Value::as_str).map(str::to_owned),
            demand: None,
            wake: is_wake(store, &job.id)?,
        };
        let cap = charter.get("max_heavy_jobs").and_then(Value::as_u64);
        match reserve(
            &ledger(&job.ledger)?,
            &mut reservation,
            job.estimate.as_ref(),
            cap,
            true,
        ) {
            Ok(()) => {
                let admitted = alter(store, &job.id, |j| {
                    j.status = "launching".into();
                    j.reason = None;
                })?;
                return Ok(Some(admitted));
            }
            Err(error) if error.downcast_ref::<Refused>().is_some() => {
                let reason = format!("waiting for capacity: {error}");
                if job.reason.as_deref() != Some(reason.as_str()) {
                    job = alter(store, &job.id, |j| j.reason = Some(reason))?;
                }
            }
            Err(error) => {
                return end(&job, "failed", format!("capacity queue error: {error:#}"));
            }
        }
        thread::sleep(QUEUE_POLL);
    }
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Copy a pipe into `path`, bounded to about `LOG_LIMIT`: the head goes
/// straight to the file, later bytes to two rotating tail segments that are
/// appended after a truncation marker when the pipe ends. Returns the number
/// of bytes dropped between head and tail.
fn capture(
    mut pipe: impl Read + std::os::fd::AsRawFd + Send + 'static,
    path: PathBuf,
    stop: Arc<AtomicBool>,
) -> Result<thread::JoinHandle<Result<u64>>> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(
        flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
        "cannot bound log reader"
    );
    let open_new = |path: &Path| {
        OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)
    };
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)?;
    let segment_path = move |n: u64| {
        let mut name = path.clone().into_os_string();
        name.push(format!(".tail{}", n % 2));
        PathBuf::from(name)
    };
    Ok(thread::spawn(move || {
        let mut head = 0usize;
        let mut segments = 0u64;
        let mut segment: Option<(fs::File, usize)> = None;
        let mut dropped = 0u64;
        let mut buf = [0u8; 8192];
        let mut stopping_bytes = 0usize;
        loop {
            if stop.load(Ordering::Relaxed) && stopping_bytes >= 65536 {
                break;
            }
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let take = n.min(LOG_HEAD - head);
                    file.write_all(&buf[..take])?;
                    head += take;
                    let mut rest = &buf[take..n];
                    while !rest.is_empty() {
                        if segment.as_ref().is_none_or(|(_, len)| *len >= LOG_SEGMENT) {
                            // The segment two rotations back is overwritten.
                            if segments >= 2 {
                                dropped +=
                                    fs::metadata(segment_path(segments)).map_or(0, |m| m.len());
                            }
                            segment = Some((open_new(&segment_path(segments))?, 0));
                            segments += 1;
                        }
                        let (out, len) = segment.as_mut().expect("segment opened above");
                        let chunk = rest.len().min(LOG_SEGMENT - *len);
                        out.write_all(&rest[..chunk])?;
                        *len += chunk;
                        rest = &rest[chunk..];
                    }
                    if stop.load(Ordering::Relaxed) {
                        stopping_bytes += n;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        drop(segment);
        if segments > 0 {
            if dropped > 0 {
                // Its own line, so JSON-lines readers skip the cut edges.
                writeln!(
                    file,
                    "\n{}",
                    json!({"type": "privateer.log.truncated", "dropped_bytes": dropped})
                )?;
            }
            for n in segments.saturating_sub(2)..segments {
                let tail = segment_path(n);
                std::io::copy(&mut fs::File::open(&tail)?, &mut file)?;
                fs::remove_file(&tail)?;
            }
        }
        file.sync_all()?;
        Ok(dropped)
    }))
}

pub fn supervise(store: &mut Store, id: &str) -> Result<()> {
    let result = supervise_inner(store, id);
    if let Err(error) = &result {
        // CLI stderr can be detached. Persist bounded diagnostics so process
        // failures remain explainable after the coordinator disappears.
        if let Ok(job) = load(store, id) {
            let diagnostic: String = format!("{error:#}").chars().take(16_384).collect();
            let dir = store
                .home()
                .join("jobs")
                .join(&job.id)
                .join(&job.incarnation);
            if fs::create_dir_all(&dir).is_ok() {
                let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
                if let Ok(mut file) = OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .mode(0o600)
                    .open(dir.join("supervisor.log"))
                {
                    let _ = file.write_all(diagnostic.as_bytes());
                    let _ = file.sync_all();
                }
            }
            // A duplicate internal invocation cannot alter the real owner's
            // record. Unknown payload ownership conservatively keeps capacity.
            if job
                .supervisor
                .as_ref()
                .is_some_and(|p| p.pid == std::process::id())
            {
                // A queued job has no payload; an unspawned one holds nothing.
                let stopped = job.status == "queued"
                    || job.payload.as_ref().and_then(|p| group_alive(p).ok()) == Some(false);
                if let Ok(updated) = alter(store, id, |j| {
                    if !terminal(&j.status) {
                        j.status = if stopped {
                            "failed".into()
                        } else {
                            "orphaned".into()
                        };
                        j.reason = Some(format!("supervisor error: {diagnostic}"));
                        if stopped {
                            j.finished_ms = Some(Store::now_ms());
                        }
                    }
                }) {
                    if stopped {
                        let _ = release(store, &updated);
                        let _ = dequeue(store, &updated);
                    } else {
                        let _ = update_reservation(store, &updated);
                    }
                    let _ = store.audit(
                        "job.supervisor_failed",
                        &json!({"job_id":id,"error":diagnostic}),
                    );
                }
            }
        }
    }
    result
}

fn supervise_inner(store: &mut Store, id: &str) -> Result<()> {
    let me = identity(std::process::id())?.context("cannot identify supervisor")?;
    let conn = store.connection();
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let claimed = (|| -> Result<Job> {
        let mut job = load(store, id)?;
        // start() records this supervisor's identity after spawning it; any
        // other supervisor claiming a queued job is a duplicate.
        let ours = job
            .supervisor
            .as_ref()
            .is_none_or(|p| p.pid == me.pid && same_start(&p.started, &me.started));
        ensure!(
            job.status == "starting" || (job.status == "queued" && ours),
            "job already supervised or terminal"
        );
        job.supervisor = Some(me);
        if job.status == "starting" {
            job.status = "launching".into();
        }
        conn.execute(
            "UPDATE jobs SET record=?2 WHERE id=?1",
            params![id, serde_json::to_string(&job)?],
        )?;
        Ok(job)
    })();
    if claimed.is_ok() {
        conn.execute_batch("COMMIT")?;
    } else {
        let _ = conn.execute_batch("ROLLBACK");
    }
    let mut job = claimed?;
    if job.status == "queued" {
        match wait_in_queue(store, job)? {
            Some(admitted) => job = admitted,
            None => return Ok(()),
        }
    }
    update_reservation(store, &job)?;
    if job.cancel_requested {
        job = alter(store, id, |j| {
            j.status = "cancelled".into();
            j.finished_ms = Some(Store::now_ms());
        })?;
        release(store, &job)?;
        return Ok(());
    }
    // Re-check authority after detach, before any payload starts.
    let spawn_authority = (|| -> Result<Option<crate::workspace::Lock>> {
        let charter = store.authorize(&job.task_id, &job.action)?;
        check_plugin_binding(&job, &charter)?;
        check_workspace_owner(store, &job)?;
        let repo = fs::canonicalize(charter["repo"].as_str().context("charter repo missing")?)?;
        let lock = crate::workspace::validate_job(
            store,
            &job.task_id,
            &repo,
            &job.cwd,
            job.workspace_lease_id.as_deref(),
        )?;
        // Waiting on an allocation/release lock may outlive the earlier read.
        let current = store.authorize(&job.task_id, &job.action)?;
        ensure!(
            fs::canonicalize(current["repo"].as_str().context("charter repo missing")?)? == repo,
            "charter repository changed before payload launch"
        );
        check_plugin_binding(&job, &current)?;
        check_workspace_owner(store, &job)?;
        Ok(lock)
    })();
    let workspace_lock = match spawn_authority {
        Ok(lock) => lock,
        Err(error) => {
            job = alter(store, id, |j| {
                j.status = "failed".into();
                j.finished_ms = Some(Store::now_ms());
                j.reason = Some(format!("authority refused before spawn: {error}"));
            })?;
            release(store, &job)?;
            return Ok(());
        }
    };
    let dir = store
        .home()
        .join("jobs")
        .join(&job.id)
        .join(&job.incarnation);
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    let mut cmd = Command::new(&job.command[0]);
    match &job.agent_actor {
        Some(actor) => {
            cmd.env("PRIVATEER_ROLE", "agent")
                .env("PRIVATEER_ACTOR", actor);
        }
        None => {
            cmd.env_remove("PRIVATEER_ROLE")
                .env_remove("PRIVATEER_ACTOR");
        }
    }
    // Crew report through crew.report, which checks this job.
    cmd.env("PRIVATEER_JOB", &job.id)
        .args(&job.command[1..])
        .current_dir(&job.cwd)
        .env("PWD", &job.cwd);
    if job.hosted {
        // The harness keeps the pane's terminal, in its own foreground
        // process group, so group signals still reach exactly its work.
        cmd.env("PRIVATEER_HOSTED_CREW", "1")
            // A crew's checkout is not its operational home. Calm preferences
            // are shared with this home's first mate, never inferred from cwd.
            .env("PRIVATEER_HOME", store.home());
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::isatty(0) == 1 {
                    libc::signal(libc::SIGTTOU, libc::SIG_IGN);
                    libc::tcsetpgrp(0, libc::getpid());
                    libc::signal(libc::SIGTTOU, libc::SIG_DFL);
                }
                Ok(())
            });
        }
    } else {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            job = alter(store, id, |j| {
                j.status = "failed".into();
                j.finished_ms = Some(Store::now_ms());
                j.reason = Some(format!("payload spawn failed: {error}"));
            })?;
            release(store, &job)?;
            return Ok(());
        }
    };
    let payload = identity(child.id())?.context("payload exited before identity captured")?;
    job = alter(store, id, |j| {
        j.payload = Some(payload.clone());
        j.status = "running".into();
    })?;
    update_reservation(store, &job)?;
    drop(workspace_lock);
    let stop = Arc::new(AtomicBool::new(false));
    // A hosted harness writes to its pane; its history is its own session.
    let (out, err) = if job.hosted {
        fs::File::create(dir.join("stdout.log"))?;
        fs::File::create(dir.join("stderr.log"))?;
        (None, None)
    } else {
        (
            Some(capture(
                child.stdout.take().context("stdout missing")?,
                dir.join("stdout.log"),
                stop.clone(),
            )?),
            Some(capture(
                child.stderr.take().context("stderr missing")?,
                dir.join("stderr.log"),
                stop.clone(),
            )?),
        )
    };
    let deadline = Instant::now() + Duration::from_secs(job.timeout_secs);
    let mut outcome = "succeeded";
    let mut reason = None;
    let mut terminated_at = None;
    let mut leader_status = None;
    let mut killed = false;
    let mut last_anchor_snapshot = Instant::now() - Duration::from_secs(1);
    let status = loop {
        if leader_status.is_none() {
            // Snapshot before reaping: an unreaped original leader (including
            // a zombie) proves group ownership. This records ordinary orphan
            // descendants too, without trusting a bare PGID after leader exit.
            // Exit is observed without reaping, so the leader cannot be reaped
            // between this snapshot and the try_wait below.
            let exited = leader_exited(payload.pid)?;
            if exited || last_anchor_snapshot.elapsed() >= Duration::from_millis(500) {
                // A leader caught mid-exit may not be provable yet (macOS
                // reports ESRCH for its process group); periodic snapshots are
                // retried, and the exit-time one is proven by waitid.
                if let Some(anchors) = capture_anchors(&payload, exited)? {
                    let _ = alter(store, id, |j| j.group_anchors = anchors)?;
                    last_anchor_snapshot = Instant::now();
                }
            }
            if exited {
                leader_status = child.try_wait()?;
            }
        }
        job = load(store, id)?;
        if let Some(status) = leader_status {
            if !group_alive(&payload)? {
                break status;
            }
            if terminated_at.is_none() {
                if !job.cleanup_descendants {
                    break status;
                }
                // Harness-owned helper descendants are optional completion
                // cleanup, not a cancelled outcome. Only recorded live group
                // members may anchor TERM/KILL after the leader was reaped.
                if let Err(error) = signal_group(&payload, libc::SIGTERM, &job.group_anchors) {
                    if group_alive(&payload)? {
                        reason = Some(format!("descendant cleanup refused: {error}"));
                    }
                    break status;
                }
                store.audit("job.descendants_cleanup", &json!({"job_id":id}))?;
                terminated_at = Some(Instant::now());
            }
        }
        let authority_error = store
            .authorize_running(&job.task_id, &job.action)
            .and_then(|charter| check_plugin_binding(&job, &charter))
            .and_then(|()| check_workspace_owner(store, &job))
            .err();
        if terminated_at.is_none()
            && (job.cancel_requested || authority_error.is_some() || Instant::now() >= deadline)
        {
            outcome = if job.cancel_requested || authority_error.is_some() {
                "cancelled"
            } else {
                "timed_out"
            };
            reason = Some(if let Some(error) = authority_error {
                format!("authority ended: {error}")
            } else if job.cancel_requested {
                "cancellation requested".into()
            } else {
                "job deadline reached".into()
            });
            let anchors =
                capture_anchors(&payload, leader_exited(payload.pid)?)?.context(UNPROVEN_LEADER)?;
            job = alter(store, id, |j| j.group_anchors = anchors)?;
            signal_group(&payload, libc::SIGTERM, &job.group_anchors)?;
            terminated_at = Some(Instant::now());
        }
        if let Some(at) = terminated_at {
            if !killed && at.elapsed() >= Duration::from_secs(2) && group_alive(&payload)? {
                signal_group(&payload, libc::SIGKILL, &job.group_anchors)?;
                killed = true;
            }
            if at.elapsed() >= Duration::from_secs(4) {
                if let Some(status) = leader_status {
                    break status;
                }
            }
        }
        thread::sleep(Duration::from_millis(50));
    };
    // A payload can leave children behind after its leader exits. Do not free
    // capacity or signal a potentially reused PGID; expose it as an orphan.
    let group_remains = group_alive(&payload)?;
    stop.store(true, Ordering::Relaxed);
    let stdout_dropped = match out {
        Some(out) => out
            .join()
            .map_err(|_| anyhow::anyhow!("stdout capture panicked"))??,
        None => 0,
    };
    let stderr_dropped = match err {
        Some(err) => err
            .join()
            .map_err(|_| anyhow::anyhow!("stderr capture panicked"))??,
        None => 0,
    };
    if outcome == "succeeded" && !status.success() {
        outcome = "failed";
    }
    job = alter(store, id, |j| {
        j.status = if group_remains {
            "orphaned".into()
        } else {
            outcome.into()
        };
        j.exit_code = status.code();
        j.finished_ms = Some(Store::now_ms());
        j.stdout_dropped_bytes = stdout_dropped;
        j.stderr_dropped_bytes = stderr_dropped;
        if group_remains {
            j.leader_result = Some(LeaderResult {
                status: outcome.into(),
                reason: reason.clone(),
            });
        }
        j.reason = if group_remains {
            Some(reason.unwrap_or_else(|| {
                "payload group still has descendants; reservation preserved".into()
            }))
        } else {
            reason
        };
    })?;
    if group_remains {
        update_reservation(store, &job)?;
    } else {
        release(store, &job)?;
    }
    store.audit(
        "job.finished",
        &json!({"job_id": id, "status": job.status, "exit_code": job.exit_code}),
    )?;
    Ok(())
}

/// Reservation keys per ledger, read once per command. A finished job never
/// gains a reservation, so a key absent here stays absent.
#[derive(Default)]
struct Reserved(std::collections::HashMap<PathBuf, std::collections::HashSet<String>>);
impl Reserved {
    fn holds(&mut self, store: &Store, job: &Job) -> Result<bool> {
        if !self.0.contains_key(&job.ledger) {
            let conn = ledger(&job.ledger)?;
            let keys = conn
                .prepare("SELECT key FROM reservations")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            self.0.insert(job.ledger.clone(), keys);
        }
        Ok(self.0[&job.ledger].contains(&reservation_key(store.home(), &job.id, &job.incarnation)))
    }
}
fn reconcile(store: &Store, mut job: Job, reserved: &mut Reserved) -> Result<Job> {
    if terminal(&job.status) {
        // Repair only the narrow crash window after durable completion and
        // before release, with actual process-group proof rather than age.
        // Released jobs, nearly all of them, need no process inspection.
        if !reserved.holds(store, &job)? {
            return Ok(job);
        }
        if match &job.payload {
            Some(p) => !group_alive(p)?,
            None => true,
        } {
            release(store, &job)?;
        }
        return Ok(job);
    }
    let supervisor_alive = match &job.supervisor {
        Some(p) => same_process(p)?,
        None => false,
    };
    if job.status == "queued" {
        // Nothing runs while queued, so a vanished supervisor ends the job.
        if !supervisor_alive
            && (job.supervisor.is_some() || Store::now_ms() - job.created_ms > START_GRACE_MS)
        {
            let observed = job.supervisor.clone();
            job = alter(store, &job.id, |j| {
                if j.status == "queued" && j.supervisor == observed {
                    j.status = "failed".into();
                    j.finished_ms = Some(Store::now_ms());
                    j.reason = Some("supervisor disappeared while waiting for capacity".into());
                }
            })?;
            dequeue(store, &job)?;
        }
        return Ok(job);
    }
    let payload_alive = match &job.payload {
        Some(p) => group_alive(p)?,
        None => false,
    };
    if !supervisor_alive && job.payload.is_some() && !payload_alive {
        // No supervisor and no recorded group member remains: nothing can hold
        // this capacity. Settle to the leader's result when it was recorded.
        let observed_incarnation = job.incarnation.clone();
        let observed_payload = job.payload.clone();
        let mut settled = false;
        job = alter(store, &job.id, |j| {
            if terminal(&j.status)
                || j.incarnation != observed_incarnation
                || j.payload != observed_payload
            {
                return;
            }
            settled = true;
            match j.leader_result.take() {
                Some(result) => {
                    j.status = result.status;
                    j.reason = result.reason;
                }
                None if j.cancel_requested => {
                    j.status = "cancelled".into();
                    j.reason = Some("cancellation requested".into());
                }
                None => {
                    j.status = "failed".into();
                    j.reason = Some(
                        "supervisor disappeared; payload ended without a recorded result".into(),
                    );
                }
            }
            j.finished_ms = j.finished_ms.or(Some(Store::now_ms()));
        })?;
        if terminal(&job.status) {
            release(store, &job)?;
        }
        if settled {
            store.audit(
                "job.settled",
                &json!({"job_id": job.id, "status": job.status, "exit_code": job.exit_code}),
            )?;
        }
        return Ok(job);
    }
    if !supervisor_alive
        && (job.payload.is_some() || Store::now_ms() - job.created_ms > START_GRACE_MS)
    {
        let observed_incarnation = job.incarnation.clone();
        let observed_supervisor = job.supervisor.clone();
        let observed_payload = job.payload.clone();
        job = alter(store, &job.id, |j| {
            if terminal(&j.status)
                || (j.status == "orphaned" && j.reason.is_some())
                || j.incarnation != observed_incarnation
                || j.supervisor != observed_supervisor
                || j.payload != observed_payload
            {
                return;
            }
            j.status = "orphaned".into();
            j.reason = Some(
                if payload_alive {
                    "supervisor disappeared; payload capacity preserved"
                } else {
                    "supervisor disappeared; inspect and cancel before retry"
                }
                .into(),
            );
        })?;
        update_reservation(store, &job)?;
    }
    Ok(job)
}
/// Reconcile unfinished jobs whose supervisor is gone, so their outcome is
/// recorded without anyone asking for their status. A live supervisor is
/// checked cheaply and left to finish its own job.
pub(crate) fn reconcile_unsupervised(store: &Store) -> Result<Vec<Value>> {
    let mut statement = store.connection().prepare(
        "SELECT record FROM jobs WHERE json_extract(record, '$.status')
         IN ('starting', 'queued', 'launching', 'running')",
    )?;
    let jobs: Vec<Job> = statement
        .query_map([], |r| r.get::<_, String>(0))?
        .map(|raw| Ok(serde_json::from_str(&raw?)?))
        .collect::<Result<_>>()?;
    let mut reserved = Reserved::default();
    let mut settled = vec![];
    for job in jobs {
        // One job that cannot be inspected must not stop the dispatcher; it
        // is reported and tried again on the next pass.
        let id = job.id.clone();
        let before = job.status.clone();
        let result = (|| -> Result<Option<Job>> {
            if job.supervisor.as_ref().map(same_process).transpose()? == Some(true) {
                return Ok(None);
            }
            Ok(Some(reconcile(store, job, &mut reserved)?))
        })();
        match result {
            Ok(Some(job)) if job.status != before => {
                settled.push(json!({"job_id": id, "status": job.status}));
            }
            Ok(_) => {}
            Err(error) => settled.push(json!({"job_id": id, "error": format!("{error:#}")})),
        }
    }
    Ok(settled)
}

/// A job's current record, for crew reports.
pub(crate) fn record(store: &Store, id: &str) -> Result<Value> {
    Ok(public_job(&load(store, id)?))
}

fn cancel(store: &Store, id: &str) -> Result<Value> {
    let initial = load(store, id)?;
    // Stopping already-owned work never grants new execution authority and
    // must remain possible after revocation or a terminal task.
    if terminal(&initial.status) {
        return Ok(public_job(&initial));
    }
    let by = match store.caller() {
        Caller::Agent { actor } => actor.clone(),
        Caller::Operator => "operator".into(),
    };
    let mut job = alter(store, id, |j| {
        if !terminal(&j.status) && !j.cancel_requested {
            j.cancel_requested = true;
            j.cancelled_by = Some(by);
        }
    })?;
    if terminal(&job.status) {
        return Ok(public_job(&job));
    }
    let supervisor_alive = match &job.supervisor {
        Some(p) => same_process(p)?,
        None => false,
    };
    // A live queued supervisor leaves the queue itself within a poll. Without
    // one, a queued job has no payload and simply ends.
    if job.status == "queued" && !supervisor_alive {
        job = alter(store, id, |j| {
            if j.status == "queued" {
                j.status = "cancelled".into();
                j.finished_ms = Some(Store::now_ms());
                j.reason = Some("cancelled while waiting for capacity".into());
            }
        })?;
        dequeue(store, &job)?;
    }
    if !supervisor_alive && !terminal(&job.status) {
        if let Some(p) = job.payload.clone() {
            if group_alive(&p)? {
                let anchors = if same_process(&p)? {
                    capture_anchors(&p, false)?.context(UNPROVEN_LEADER)?
                } else {
                    job.group_anchors.clone()
                };
                let _ = alter(store, id, |j| j.group_anchors = anchors.clone())?;
                signal_group(&p, libc::SIGTERM, &anchors)?;
                let until = Instant::now() + Duration::from_secs(2);
                while Instant::now() < until && group_alive(&p)? {
                    thread::sleep(Duration::from_millis(50));
                }
                if group_alive(&p)? {
                    signal_group(&p, libc::SIGKILL, &anchors)?;
                }
                // Give killed descendants time to leave the group.
                let until = Instant::now() + Duration::from_secs(2);
                while Instant::now() < until && group_alive(&p)? {
                    thread::sleep(Duration::from_millis(50));
                }
                ensure!(
                    !group_alive(&p)?,
                    "payload group remains; reservation preserved"
                );
            }
            job = alter(store, id, |j| {
                j.status = "cancelled".into();
                j.finished_ms = Some(Store::now_ms());
                j.reason = Some("cancellation requested".into());
                j.leader_result = None;
            })?;
            release(store, &job)?;
        } else {
            // A missing identity in the detach/spawn window is not proof that
            // no process exists. Keep the reservation for manual inspection.
            bail!(
                "supervisor/payload identity unavailable; cancellation recorded, reservation preserved; after checking for the payload, release it with resource.recover and force:true"
            );
        }
    }
    store.audit("job.cancel_requested", &json!({"job_id": id}))?;
    Ok(public_job(&job))
}
/// Finished jobs that ended before `cutoff_ms` and no longer hold a ledger
/// reservation; capacity is never forgotten by pruning. Deletes their records
/// unless `dry_run` and returns (pruned, still_reserved, log directories),
/// leaving the directories for the caller to remove after committing.
pub(crate) fn prune(
    store: &Store,
    cutoff_ms: i64,
    dry_run: bool,
) -> Result<(u64, u64, Vec<PathBuf>)> {
    let mut statement = store.connection().prepare("SELECT record FROM jobs")?;
    let jobs: Vec<Job> = statement
        .query_map([], |r| r.get::<_, String>(0))?
        .map(|raw| Ok(serde_json::from_str(&raw?)?))
        .collect::<Result<_>>()?;
    let mut ledgers = std::collections::HashMap::new();
    let (mut pruned, mut reserved, mut directories) = (0, 0, vec![]);
    for job in jobs {
        if !terminal(&job.status) || job.finished_ms.is_none_or(|t| t >= cutoff_ms) {
            continue;
        }
        if !ledgers.contains_key(&job.ledger) {
            ledgers.insert(job.ledger.clone(), ledger(&job.ledger)?);
        }
        let held: bool = ledgers[&job.ledger].query_row(
            "SELECT EXISTS(SELECT 1 FROM reservations WHERE key=?1)",
            [reservation_key(store.home(), &job.id, &job.incarnation)],
            |r| r.get(0),
        )?;
        if held {
            reserved += 1;
            continue;
        }
        if !dry_run {
            store
                .connection()
                .execute("DELETE FROM jobs WHERE id=?1", [&job.id])?;
        }
        pruned += 1;
        directories.push(store.home().join("jobs").join(&job.id));
    }
    Ok((pruned, reserved, directories))
}

/// Explicit operator recovery for a shared-ledger reservation that no job can
/// settle: its job record is missing (a crash between ledger and home
/// commits), belongs to another home, or never recorded a payload identity.
/// Live supervisors and live payload groups are never released, even with
/// force; those are stopped through job.cancel.
fn recover(store: &Store, args: &Value) -> Result<Value> {
    let key = args
        .get("key")
        .and_then(Value::as_str)
        .context("key required (see resource.status)")?;
    let force = args
        .get("force")
        .map(|v| v.as_bool().context("force must be boolean"))
        .transpose()?
        .unwrap_or(false);
    let conn = ledger(&ledger_path(store)?)?;
    let raw: String = conn
        .query_row("SELECT record FROM reservations WHERE key=?1", [key], |r| {
            r.get(0)
        })
        .optional()?
        .context("unknown reservation")?;
    let reservation: Reservation = serde_json::from_str(&raw)?;
    // Prefer this home's job record; the ledger copy may lag behind it.
    let job = if reservation.home == store.home() {
        load(store, &reservation.job_id)
            .ok()
            .filter(|j| j.incarnation == reservation.incarnation)
    } else {
        None
    };
    let supervisor = job
        .as_ref()
        .and_then(|j| j.supervisor.clone())
        .or(reservation.supervisor.clone());
    let payload = job
        .as_ref()
        .and_then(|j| j.payload.clone())
        .or(reservation.payload.clone());
    if let Some(p) = &supervisor {
        ensure!(
            !same_process(p)?,
            "supervisor is still running; use job.cancel"
        );
    }
    match &payload {
        Some(p) => ensure!(
            !group_alive(p)?,
            "payload group is still running; use job.cancel"
        ),
        None => {
            ensure!(
                Store::now_ms() - reservation.created_ms > START_GRACE_MS,
                "reservation is still within its start grace period"
            );
            ensure!(
                force,
                "no payload identity was recorded, so no process can be proven absent; check for job {} manually, then pass force:true",
                reservation.job_id
            );
        }
    }
    if let Some(job) = job.filter(|j| !terminal(&j.status)) {
        alter(store, &job.id, |j| {
            if j.incarnation == reservation.incarnation && !terminal(&j.status) {
                j.status = "failed".into();
                j.finished_ms = Some(Store::now_ms());
                j.reason = Some("reservation released by resource.recover".into());
                j.leader_result = None;
            }
        })?;
    }
    conn.execute("DELETE FROM reservations WHERE key=?1", [key])?;
    store.audit(
        "resource.recovered",
        &json!({"key": key, "home": reservation.home, "job_id": reservation.job_id,
            "incarnation": reservation.incarnation, "forced": payload.is_none()}),
    )?;
    Ok(json!({"released": key, "job_id": reservation.job_id, "forced": payload.is_none()}))
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "job.start" => start(store, args, None, false),
        "job.status" => {
            let id = args
                .get("id")
                .and_then(Value::as_str)
                .context("id required")?;
            Ok(public_job(&reconcile(
                store,
                load(store, id)?,
                &mut Reserved::default(),
            )?))
        }
        "job.list" => {
            // Pages in id order: pass the last id seen as after_id.
            let limit = args
                .get("limit")
                .map(|v| v.as_u64().context("limit must be a positive integer"))
                .transpose()?
                .unwrap_or(200)
                .clamp(1, 1000);
            let after = args
                .get("after_id")
                .map(|v| v.as_str().context("after_id must be a job id"))
                .transpose()?
                .unwrap_or("");
            let mut stmt = store
                .connection()
                .prepare("SELECT record FROM jobs WHERE id>?1 ORDER BY id LIMIT ?2")?;
            let rows = stmt.query_map(params![after, limit], |r| r.get::<_, String>(0))?;
            let all: Vec<Job> = rows
                .map(|r| Ok(serde_json::from_str(&r?)?))
                .collect::<Result<_>>()?;
            let mut reserved = Reserved::default();
            Ok(json!(
                all.into_iter()
                    .map(|j| reconcile(store, j, &mut reserved).map(|j| public_job(&j)))
                    .collect::<Result<Vec<_>>>()?
            ))
        }
        "job.cancel" => cancel(
            store,
            args.get("id")
                .and_then(Value::as_str)
                .context("id required")?,
        ),
        "job.retry" => {
            let old_id = args
                .get("id")
                .and_then(Value::as_str)
                .context("id required")?;
            let old = reconcile(store, load(store, old_id)?, &mut Reserved::default())?;
            ensure!(
                matches!(old.status.as_str(), "failed" | "cancelled" | "timed_out"),
                "retry requires a stopped unsuccessful job; no automatic retry of live/orphan work"
            );
            store.authorize(&old.task_id, "recovery")?;
            let mut next = json!({"task_id": old.task_id, "command": old.command, "cwd": old.cwd, "heavy": old.heavy, "action": old.action, "timeout_secs": old.timeout_secs, "cleanup_descendants":old.cleanup_descendants, "agent_actor": old.agent_actor, "estimate": old.estimate, "wait_for_capacity": old.wait_for_capacity, "max_wait_secs": old.max_wait_secs});
            if let Some(id) = args.get("new_id") {
                next["id"] = id.clone();
            }
            if let Some(binding) = &old.plugin_binding {
                next["plugin_binding"] = binding.clone();
            }
            if let Some(lease) = &old.workspace_lease_id {
                next["workspace_lease_id"] = json!(lease);
            }
            start(store, next, Some(old_id.to_owned()), old.action == "spawn")
        }
        "resource.recover" => recover(store, &args),
        // Host-wide and open to agents, which tune it to their person's wishes.
        "resource.configure" => {
            let settings = admission::configure(&ledger(&ledger_path(store)?)?, &args)?;
            let caller = match store.caller() {
                Caller::Operator => json!("operator"),
                Caller::Agent { actor } => json!({"agent": actor}),
            };
            store.audit(
                "resource.configured",
                &json!({"changes": args, "by": caller}),
            )?;
            Ok(admission::report(&settings, &admission::measure()?))
        }
        "resource.status" => {
            let path = ledger_path(store)?;
            let conn = ledger(&path)?;
            let records = reservations(&conn)?;
            let settings = admission::settings(&conn)?;
            let mut values = vec![];
            for r in records {
                let supervisor_alive = match &r.supervisor {
                    Some(p) => same_process(p)?,
                    None => false,
                };
                let payload_group_alive = match &r.payload {
                    Some(p) => group_alive(p)?,
                    None => false,
                };
                let mut value = serde_json::to_value(&r)?;
                value["supervisor_alive"] = json!(supervisor_alive);
                value["payload_group_alive"] = json!(payload_group_alive);
                value["orphaned"] = json!(
                    !supervisor_alive
                        && (payload_group_alive || Store::now_ms() - r.created_ms > START_GRACE_MS)
                );
                values.push(value);
            }
            let mut status = admission::report(&settings, &admission::measure()?);
            status["ledger"] = json!(path);
            status["heavy_reserved"] = json!(values.iter().filter(|v| v["heavy"] == true).count());
            status["reservations"] = json!(values);
            status["queue"] = json!(live_queue(&conn)?);
            status["model_routes"] = json!(crate::routes::list(&conn)?);
            Ok(status)
        }
        _ => bail!("unknown jobs command {command}"),
    }
}
