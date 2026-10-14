//! Durable, bounded Unix jobs and a shared per-host admission ledger.
//!
//! This is process supervision, not a privileged sandbox. Unknown/orphaned
//! payloads keep their reservation until their recorded process group is gone.
use crate::store::Store;
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
const START_GRACE_MS: i64 = 30_000;

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
    /// Outcome the job settles to once descendants left by an exited leader
    /// are gone. Absent when the supervisor vanished before recording one.
    #[serde(default)]
    leader_result: Option<LeaderResult>,
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
fn alter(store: &Store, id: &str, f: impl FnOnce(&mut Job)) -> Result<Job> {
    let conn = store.connection();
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        let mut job = load(store, id)?;
        f(&mut job);
        conn.execute(
            "UPDATE jobs SET record=?2 WHERE id=?1",
            params![id, serde_json::to_string(&job)?],
        )?;
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
fn ledger_path(store: &Store) -> Result<PathBuf> {
    let cfg = config(store)?;
    let path = if let Some(p) = cfg.get("resource_home").and_then(Value::as_str) {
        PathBuf::from(p)
    } else if let Ok(p) = std::env::var("PRIVATEER_RESOURCE_HOME") {
        PathBuf::from(p)
    } else {
        PathBuf::from(std::env::var("HOME").context("HOME required for shared resource ledger")?)
            .join(".local/state/privateer-host")
    };
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
    Ok(conn)
}
fn reservations(conn: &Connection) -> Result<Vec<Reservation>> {
    let mut stmt = conn.prepare("SELECT record FROM reservations ORDER BY key")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.map(|raw| Ok(serde_json::from_str(&raw?)?)).collect()
}
fn reservation_key(home: &Path, id: &str, incarnation: &str) -> String {
    format!("{}:{id}:{incarnation}", home.display())
}
fn reserve(conn: &Connection, reservation: &Reservation, max_heavy: u64) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        // Never garbage-collect reservations merely because a wrapper disappeared.
        let count = reservations(conn)?.iter().filter(|r| r.heavy).count() as u64;
        ensure!(
            !reservation.heavy || count < max_heavy,
            "heavy-job capacity exhausted ({count}/{max_heavy})"
        );
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
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
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
fn free_bytes(path: &Path) -> Result<u64> {
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
fn start(store: &Store, args: Value, retry_of: Option<String>) -> Result<Value> {
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
        matches!(action, "implement" | "test" | "recovery" | "cleanup"),
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
    let cwd = fs::canonicalize(
        args.get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| repo.clone()),
    )?;
    ensure!(
        cwd.is_dir() && cwd.starts_with(&repo),
        "cwd must stay inside charter repository"
    );
    let heavy = args.get("heavy").and_then(Value::as_bool).unwrap_or(true);
    let cleanup_descendants = args
        .get("cleanup_descendants")
        .map(|value| {
            value
                .as_bool()
                .context("cleanup_descendants must be boolean")
        })
        .transpose()?
        .unwrap_or(false);
    if let Ok(existing) = load(store, &id) {
        ensure!(
            existing.task_id == task_id
                && existing.command == command
                && existing.cwd == cwd
                && existing.heavy == heavy
                && existing.action == action
                && existing.plugin_binding == plugin_binding
                && existing.cleanup_descendants == cleanup_descendants,
            "job id already belongs to different work"
        );
        return Ok(public_job(&existing));
    }
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
    let cfg = config(store)?;
    let max_heavy = charter
        .get("max_heavy_jobs")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .min(
            cfg.get("max_heavy_jobs")
                .and_then(Value::as_u64)
                .unwrap_or(1),
        );
    ensure!(!heavy || max_heavy > 0, "heavy jobs disabled");
    let ledger_path = ledger_path(store)?;
    let mut job = Job {
        id: id.clone(),
        task_id: task_id.to_owned(),
        incarnation: Uuid::new_v4().to_string(),
        command,
        cwd,
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
        leader_result: None,
    };
    let reservation = Reservation {
        key: reservation_key(store.home(), &id, &job.incarnation),
        home: store.home().to_owned(),
        job_id: id.clone(),
        incarnation: job.incarnation.clone(),
        heavy,
        state: "starting".to_owned(),
        created_ms: job.created_ms,
        supervisor: None,
        payload: None,
    };
    let conn = store.connection();
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let admitted = (|| -> Result<Option<Job>> {
        let existing: Option<String> = conn
            .query_row("SELECT record FROM jobs WHERE id=?1", [&id], |r| r.get(0))
            .optional()?;
        if let Some(raw) = existing {
            let existing: Job = serde_json::from_str(&raw)?;
            ensure!(
                existing.task_id == task_id
                    && existing.command == job.command
                    && existing.cwd == job.cwd
                    && existing.heavy == heavy
                    && existing.action == action
                    && existing.plugin_binding == job.plugin_binding
                    && existing.cleanup_descendants == job.cleanup_descendants,
                "job id already belongs to different work"
            );
            return Ok(Some(existing));
        }
        reserve(&ledger(&ledger_path)?, &reservation, max_heavy)?;
        if let Err(error) = conn.execute(
            "INSERT INTO jobs(id,record) VALUES(?1,?2)",
            params![id, serde_json::to_string(&job)?],
        ) {
            release(store, &job)?;
            return Err(error.into());
        }
        Ok(None)
    })();
    if admitted.is_ok() {
        if let Err(error) = conn.execute_batch("COMMIT") {
            let _ = conn.execute_batch("ROLLBACK");
            release(store, &job)?;
            return Err(error.into());
        }
    } else {
        let _ = conn.execute_batch("ROLLBACK");
    }
    if let Some(existing) = admitted? {
        return Ok(public_job(&existing));
    }
    let executable = std::env::current_exe()?;
    let mut cmd = Command::new(executable);
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
                j.reason = Some(format!("supervisor spawn failed: {error}"));
            })?;
            release(store, &job)?;
            return Err(error.into());
        }
    }
    store.audit(
        "job.started",
        &json!({"job_id": id, "task_id": task_id, "incarnation": job.incarnation}),
    )?;
    Ok(public_job(&job))
}

fn capture(
    mut pipe: impl Read + std::os::fd::AsRawFd + Send + 'static,
    path: PathBuf,
    stop: Arc<AtomicBool>,
) -> Result<thread::JoinHandle<Result<()>>> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(
        flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
        "cannot bound log reader"
    );
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    Ok(thread::spawn(move || {
        let mut kept = 0usize;
        let mut buf = [0u8; 8192];
        let mut stopping_bytes = 0usize;
        loop {
            if stop.load(Ordering::Relaxed) && stopping_bytes >= 65536 {
                break;
            }
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let take = n.min(LOG_LIMIT.saturating_sub(kept));
                    file.write_all(&buf[..take])?;
                    kept += take;
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
        file.sync_all()?;
        Ok(())
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
                let stopped = job.payload.as_ref().and_then(|p| group_alive(p).ok()) == Some(false);
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
        ensure!(
            job.status == "starting",
            "job already supervised or terminal"
        );
        job.supervisor = Some(me);
        job.status = "launching".into();
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
    let spawn_authority = (|| -> Result<()> {
        let charter = store.authorize(&job.task_id, &job.action)?;
        check_plugin_binding(&job, &charter)?;
        let repo = fs::canonicalize(charter["repo"].as_str().context("charter repo missing")?)?;
        ensure!(
            fs::canonicalize(&job.cwd)? == job.cwd && job.cwd.starts_with(repo),
            "job cwd changed outside charter before spawn"
        );
        Ok(())
    })();
    if let Err(error) = spawn_authority {
        job = alter(store, id, |j| {
            j.status = "failed".into();
            j.finished_ms = Some(Store::now_ms());
            j.reason = Some(format!("authority refused before spawn: {error}"));
        })?;
        release(store, &job)?;
        return Ok(());
    }
    let dir = store
        .home()
        .join("jobs")
        .join(&job.id)
        .join(&job.incarnation);
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    let mut cmd = Command::new(&job.command[0]);
    cmd.args(&job.command[1..])
        .current_dir(&job.cwd)
        .stdin(Stdio::null())
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
    let stop = Arc::new(AtomicBool::new(false));
    let out = capture(
        child.stdout.take().context("stdout missing")?,
        dir.join("stdout.log"),
        stop.clone(),
    )?;
    let err = capture(
        child.stderr.take().context("stderr missing")?,
        dir.join("stderr.log"),
        stop.clone(),
    )?;
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
    out.join()
        .map_err(|_| anyhow::anyhow!("stdout capture panicked"))??;
    err.join()
        .map_err(|_| anyhow::anyhow!("stderr capture panicked"))??;
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

fn reconcile(store: &Store, mut job: Job) -> Result<Job> {
    if terminal(&job.status) {
        // Repair only the narrow crash window after durable completion and
        // before release, with actual process-group proof rather than age.
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
fn cancel(store: &Store, id: &str) -> Result<Value> {
    let initial = load(store, id)?;
    // Stopping already-owned work never grants new execution authority and
    // must remain possible after revocation or a terminal task.
    if terminal(&initial.status) {
        return Ok(public_job(&initial));
    }
    let mut job = alter(store, id, |j| {
        if !terminal(&j.status) {
            j.cancel_requested = true;
        }
    })?;
    if terminal(&job.status) {
        return Ok(public_job(&job));
    }
    let supervisor_alive = match &job.supervisor {
        Some(p) => same_process(p)?,
        None => false,
    };
    if !supervisor_alive {
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
        "job.start" => start(store, args, None),
        "job.status" => {
            let id = args
                .get("id")
                .and_then(Value::as_str)
                .context("id required")?;
            Ok(public_job(&reconcile(store, load(store, id)?)?))
        }
        "job.list" => {
            let mut stmt = store
                .connection()
                .prepare("SELECT record FROM jobs ORDER BY id")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let all: Vec<Job> = rows
                .map(|r| Ok(serde_json::from_str(&r?)?))
                .collect::<Result<_>>()?;
            Ok(json!(
                all.into_iter()
                    .map(|j| reconcile(store, j).map(|j| public_job(&j)))
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
            let old = reconcile(store, load(store, old_id)?)?;
            ensure!(
                matches!(old.status.as_str(), "failed" | "cancelled" | "timed_out"),
                "retry requires a stopped unsuccessful job; no automatic retry of live/orphan work"
            );
            store.authorize(&old.task_id, "recovery")?;
            let mut next = json!({"task_id": old.task_id, "command": old.command, "cwd": old.cwd, "heavy": old.heavy, "action": old.action, "timeout_secs": old.timeout_secs, "cleanup_descendants":old.cleanup_descendants});
            if let Some(id) = args.get("new_id") {
                next["id"] = id.clone();
            }
            if let Some(binding) = &old.plugin_binding {
                next["plugin_binding"] = binding.clone();
            }
            start(store, next, Some(old_id.to_owned()))
        }
        "resource.recover" => recover(store, &args),
        "resource.status" => {
            let path = ledger_path(store)?;
            let records = reservations(&ledger(&path)?)?;
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
            Ok(
                json!({"ledger": path, "heavy_reserved": values.iter().filter(|v| v["heavy"] == true).count(), "reservations": values}),
            )
        }
        _ => bail!("unknown jobs command {command}"),
    }
}
