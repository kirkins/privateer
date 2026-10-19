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
use std::io::{Read, Seek, SeekFrom, Write};
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
/// its most recent bytes (the latest activity and the final usage), in a tail
/// cut back in place; the middle of a longer log is dropped.
const LOG_HEAD: usize = LOG_LIMIT / 4;
const LOG_SEGMENT: usize = (LOG_LIMIT - LOG_HEAD) / 2;
/// The tail is cut back to `LOG_SEGMENT` once it reaches this size; the room
/// left under the limit holds the truncation marker.
const TAIL_FULL: usize = 2 * LOG_SEGMENT - 256;
const START_GRACE_MS: i64 = 30_000;
/// First-mate presence a crew's payload must not inherit (crew-role.md).
const CREW_UNATTENDED: [&str; 4] = [
    "PRIVATEER_PRESENCE_HOLDER",
    "PRIVATEER_HOSTED_HOLDER",
    "PRIVATEER_HOSTED",
    "PRIVATEER_ATTENDED_HARNESS",
];
pub(crate) const DEFAULT_MAX_WAIT_SECS: u64 = 3600;
const QUEUE_POLL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Process {
    pid: u32,
    started: String,
    /// The PID namespace the recorder saw this PID in. PID numbers and process
    /// groups are namespace-local, so another namespace cannot inspect them.
    #[serde(default)]
    pid_namespace: Option<String>,
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
    /// Whether the caller asked for heavy work by name. Only then does the
    /// heavy disk default apply; an unlabelled job is heavy for CPU and memory.
    #[serde(default)]
    heavy_declared: bool,
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
    /// Descendants that left the payload's process group, such as an
    /// `xcodebuild` that starts a session of its own. A job is not over, and
    /// its capacity not free, while one of them lives.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    escaped: Vec<Process>,
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
    /// Why this record was settled as left behind by another machine: its
    /// ledger is not here, so nothing on this machine can hold or release it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    foreign: Option<String>,
    #[serde(default)]
    work_offer: Option<String>,
    /// The parked crew this runtime resume continues. Kept so a claim can
    /// admit the resume's own work through that crew's lineage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resume_of: Option<String>,
    /// Crew hosted in the terminal host: the supervisor runs in its own
    /// space and the harness gets that pane's terminal instead of log pipes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    hosted: bool,
    /// When the supervisor spawned the payload; absent for jobs recorded
    /// before it was kept, and until the payload runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    spawned_ms: Option<i64>,
    /// A harness job's `harness/provider/model`, for provider backoff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_route: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    route_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    route_why: Option<String>,
    /// The tokens a harness job reported, recorded once when it settles
    /// (see `harness_usage`). None while it runs, and for a harness that
    /// reported nothing: never zero for an unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    usage: Option<Value>,
    /// The job's stdout dropped bytes from the middle of its log, so its
    /// usage may be short of what the harness used.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    usage_partial: bool,
    /// Bundled Codex read-only crew must leave a durable report; otherwise a
    /// sandboxed no-op would be indistinguishable from successful work.
    #[serde(default)]
    codex_read_only_crew: bool,
    /// Process run time spent in this incarnation, for the record. Queued and
    /// parked time is not in it.
    #[serde(default, skip_serializing_if = "is_zero")]
    run_ms: u64,
    /// The crew is parked on a heavy job it started (`crew.park`): its payload
    /// has exited, and its outcome resumes it. Names the park record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    park: Option<String>,
    /// The agent launch that started this crew, so a park can resume its
    /// native session or start it fresh. Set by the agent path only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    launch: Option<Value>,
    /// Its outcome is not mailed: the one who ended it already knows (a task's
    /// owner closing the task ends its parked crews and their jobs).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    quiet: bool,
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
    /// The supervised job whose agent started this one, when an agent did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    started_by: Option<String>,
    /// Resolved estimate counted by admission while recently admitted.
    #[serde(default)]
    demand: Option<Demand>,
    /// The filesystem the declared space is on, as `disk_key`, and its free
    /// space when the reservation was admitted. Free space lost since then is
    /// what the job has already written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disk_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disk_free_bytes: Option<u64>,
    /// The trims `disk_key` had seen in total when this was admitted, so the
    /// trims made while it runs can be told apart from its own writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disk_trimmed_at_admit: Option<u64>,
    /// The job's recorded descendants outside its payload group.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    escaped: Vec<Process>,
    /// A first mate's wake, which a fleet stow does not hold back: the
    /// first mate must wake to stow.
    #[serde(default)]
    wake: bool,
    /// The job's own timeout, so a watcher on any home can tell a slow build
    /// from a stuck one without reading another ship's job records.
    #[serde(default)]
    timeout_ms: Option<i64>,
    /// The ship whose task this job serves, recorded so a watcher on another
    /// home can name whose work is holding capacity without opening that
    /// home's records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ship: Option<String>,
    /// A key for the work this reservation runs (its command) and the task
    /// it serves, so runs of the same work on this host can be compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    work: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    task: Option<String>,
}

impl Reservation {
    /// How long this reservation may hold capacity before a watcher judges
    /// it unhealthy. A timeout is the base, since that is the bound its own
    /// supervisor enforces; a run of the same work on this host that took
    /// longer raises it, because work that has always taken twenty minutes
    /// is not unhealthy at twelve.
    fn expected_ms(&self, conn: &Connection) -> Result<Option<i64>> {
        let Some(timeout) = self.timeout_ms else {
            return Ok(None);
        };
        Ok(Some(
            match longest_recorded_run(conn, self.work.as_deref(), self.task.as_deref())? {
                Some(run) => run.max(timeout),
                None => timeout.saturating_mul(EXPECTED_RUN_MULTIPLE),
            },
        ))
    }

    /// The ship whose work holds this reservation, as it was recorded at
    /// admission: the charter that admitted it is the fallback, and a job
    /// from a home this ledger cannot name says so rather than guessing.
    fn ship(&self) -> String {
        self.ship
            .clone()
            .or_else(|| self.charter.clone())
            .unwrap_or_else(|| "another ship".into())
    }
}

/// A job holding this host's capacity, and what can be seen of it from any
/// home: only the ledger and live processes are read, never another ship's
/// records.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Holder {
    pub key: String,
    pub job_id: String,
    /// The ship whose home holds this job, as the fleet records it.
    pub ship: String,
    pub home: PathBuf,
    pub charter: Option<String>,
    pub heavy: bool,
    /// When it began holding capacity, which for a queued job is its
    /// admission, not when it was asked for.
    pub started_ms: i64,
    pub state: String,
    pub supervisor_alive: bool,
    pub payload_alive: bool,
    pub escaped_alive: bool,
    /// How long this kind of work may hold capacity before the owner should
    /// hear about it; None when no job timeout was recorded for it.
    pub expected_ms: Option<i64>,
    /// Why this holder looks unhealthy, or None while it looks ordinary.
    pub unhealthy: Option<String>,
}

impl Holder {
    /// Whether any process is still running for this reservation.
    pub fn live(&self) -> bool {
        self.supervisor_alive || self.payload_alive || self.escaped_alive
    }
    pub fn age_ms(&self, now_ms: i64) -> i64 {
        (now_ms - self.started_ms).max(0)
    }
    pub fn to_json(&self, now_ms: i64) -> Value {
        json!({
            "key": self.key, "job_id": self.job_id, "ship": self.ship, "home": self.home,
            "charter": self.charter, "heavy": self.heavy, "started_ms": self.started_ms,
            "age_ms": self.age_ms(now_ms), "state": self.state,
            "supervisor_alive": self.supervisor_alive, "payload_alive": self.payload_alive,
            "escaped_alive": self.escaped_alive, "expected_ms": self.expected_ms,
            "live": self.live(), "unhealthy": self.unhealthy,
        })
    }
}

/// How long a job may hold capacity before a watcher judges it unhealthy.
///
/// The job's own timeout is what its supervisor enforces, so a holder that
/// outlives it several times over is not a slow build but a stuck one. When
/// this host has run the same command or task before, its longest recorded
/// run is the better yardstick and replaces that multiple; a build that has
/// always taken twenty minutes is not unhealthy at twelve. This is a floor
/// for a first warning only: the owner is told once and the facts stay in
/// `resource.status` and `runtime.needs`.
const EXPECTED_RUN_MULTIPLE: i64 = 3;
/// Recorded runs older than this say little about today's machine.
const RUN_HISTORY_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// Recorded runs kept per ledger, bounded so it cannot grow without limit.
const RUN_HISTORY_ROWS: i64 = 256;

/// A job waiting for capacity, first come first served across homes.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Queued {
    key: String,
    home: PathBuf,
    job_id: String,
    incarnation: String,
    created_ms: i64,
    supervisor: Option<Process>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
}
pub(crate) struct WorkspaceQueueLease {
    ledger: PathBuf,
    key: String,
}
impl Drop for WorkspaceQueueLease {
    fn drop(&mut self) {
        if let Ok(conn) = ledger(&self.ledger) {
            let _ = conn.execute("DELETE FROM queue WHERE key=?1", [&self.key]);
        }
    }
}
pub fn init(store: &Store) -> Result<()> {
    // A park outlives the dispatcher: it is what resumes the crew after restart.
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS jobs (id TEXT PRIMARY KEY, record TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS crew_parks (id TEXT PRIMARY KEY, root TEXT NOT NULL,
           crew_job_id TEXT NOT NULL, child_job_id TEXT NOT NULL, task_id TEXT NOT NULL,
           ship TEXT NOT NULL, state TEXT NOT NULL, next TEXT NOT NULL, harness TEXT,
           session TEXT, resumed_job_id TEXT, attempt INTEGER NOT NULL DEFAULT 0,
           retries INTEGER NOT NULL DEFAULT 0,
           created_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS crew_parks_child ON crew_parks(child_job_id);",
    )?;
    // Live jobs, newest first, without reading every record (`fleet.snapshot`).
    store.connection().execute_batch(&format!(
        "CREATE INDEX IF NOT EXISTS jobs_live ON jobs(json_extract(record,'$.created_ms'))
         WHERE json_extract(record,'$.status') NOT IN {TERMINAL_SQL};"
    ))?;
    // Harness jobs by when they settled, so `usage.summary` reads a period's
    // records without the rest.
    store.connection().execute_batch(&format!(
        "CREATE INDEX IF NOT EXISTS jobs_finished ON jobs(json_extract(record,'$.finished_ms'))
         WHERE {HARNESS_SQL};"
    ))?;
    // The process that claimed a park's resume, so recovery leaves a launch
    // that is still in flight alone (added after the table first shipped).
    let claimer: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('crew_parks') WHERE name='claimer')",
        [],
        |r| r.get(0),
    )?;
    if !claimer {
        store
            .connection()
            .execute_batch("ALTER TABLE crew_parks ADD COLUMN claimer TEXT")?;
    }
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
        // A harness job's usage is read from its log once, as it settles,
        // and commits with the status that settled it.
        if terminal(&job.status) && !terminal(&before) && job.usage.is_none() {
            let (usage, cut) = harness_usage(store, &job);
            job.usage = usage;
            job.usage_partial = is_harness(&job) && (cut || job.stdout_dropped_bytes > 0);
        }
        conn.execute(
            "UPDATE jobs SET record=?2 WHERE id=?1",
            params![id, serde_json::to_string(&job)?],
        )?;
        // A harness denial becomes one captain ask when the job ends; any
        // failure there leaves the ordinary outcome.
        let block = (terminal(&job.status) && job.status != "resumed" && !terminal(&before))
            .then(|| crate::block_ask::settle(store, &public_job(&job)))
            .flatten();
        if reported(&job.status) && !reported(&before) {
            crate::crew::outcome(store, &public_job(&job), block.as_ref())?;
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
/// Whether a job runs a harness: crew, errands and wakes all start through
/// `agent.start`, the only path that records the `spawn` action.
fn is_harness(job: &Job) -> bool {
    job.action == "spawn"
}
/// The tokens a settled harness job reported in its log, read with the
/// parser wakes use and normalized to `input`, `output`, `cacheRead`,
/// `cacheWrite` and `totalTokens`. A hosted job writes to its pane, not its
/// log, so it reports nothing here. Also whether the log was cut: a
/// truncation marker stands where bytes were dropped.
fn harness_usage(store: &Store, job: &Job) -> (Option<Value>, bool) {
    if !is_harness(job) {
        return (None, false);
    }
    let log = store
        .home()
        .join("jobs")
        .join(&job.id)
        .join(&job.incarnation)
        .join("stdout.log");
    // A bounded log can be cut inside a UTF-8 character; keep the rest.
    let Ok(text) = fs::read(log).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()) else {
        return (None, false);
    };
    let usage = crate::activation::usage(&text).and_then(|raw| normalize_usage(&raw));
    (usage, log_cut(&text))
}
/// Whether a bounded log dropped bytes: its truncation marker is a line.
pub(crate) fn log_cut(text: &str) -> bool {
    text.lines().any(|line| {
        line.starts_with('{')
            && line.contains(TRUNCATED)
            && serde_json::from_str::<Value>(line).is_ok_and(|v| v["type"] == TRUNCATED)
    })
}
/// One usage shape for every harness. Native launchers already print it;
/// Codex prints `input_tokens` with its cached tokens inside, so the cached
/// part moves to `cacheRead` and `input` is what was not cached, as the
/// others count it. Reasoning tokens a harness reports apart from its output
/// (OpenCode, agy) are output, as providers bill them. A missing total is the
/// sum of the four. None when no field is recognized: a harness that reports
/// nothing records no usage.
pub(crate) fn normalize_usage(raw: &Value) -> Option<Value> {
    let field = |names: &[&str]| names.iter().find_map(|name| raw[*name].as_u64());
    let mut input = field(&["input", "input_tokens"]);
    let output = match (
        field(&["output", "output_tokens"]),
        raw["reasoning"].as_u64(),
    ) {
        (None, None) => None,
        (output, reasoning) => Some(output.unwrap_or(0) + reasoning.unwrap_or(0)),
    };
    let mut cache_read = field(&["cacheRead", "cache_read_input_tokens"]);
    let cache_write = field(&["cacheWrite", "cache_creation_input_tokens"]);
    if let Some(cached) = raw["cached_input_tokens"].as_u64() {
        input = input.map(|n| n.saturating_sub(cached));
        cache_read = Some(cache_read.unwrap_or(0) + cached);
    }
    let total = field(&["totalTokens", "total_tokens"]);
    if [input, output, cache_read, cache_write, total]
        .iter()
        .all(Option::is_none)
    {
        return None;
    }
    let (input, output) = (input.unwrap_or(0), output.unwrap_or(0));
    let (cache_read, cache_write) = (cache_read.unwrap_or(0), cache_write.unwrap_or(0));
    Some(json!({
        "input": input, "output": output, "cacheRead": cache_read, "cacheWrite": cache_write,
        "totalTokens": total.unwrap_or(input + output + cache_read + cache_write),
    }))
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
        "CREATE TABLE IF NOT EXISTS reservations (key TEXT PRIMARY KEY, record TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS released (key TEXT PRIMARY KEY, cpus REAL NOT NULL,
           load REAL NOT NULL, released_ms INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS runs (key TEXT NOT NULL, work TEXT, task TEXT,
           duration_ms INTEGER NOT NULL, finished_ms INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS runs_finished ON runs(finished_ms);",
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
pub(crate) fn shared_ledger(store: &Store) -> Result<Connection> {
    ledger(&ledger_path(store)?)
}
/// A workspace needs only disk. When the floor is met it is allocated at
/// once, whatever jobs wait in the capacity queue for CPU or memory; only a
/// workspace below the floor joins the queue, and it waits there behind
/// earlier workspaces that are waiting for disk too. The guard holds the
/// queue position across the external Treehouse lease operation.
pub(crate) fn wait_for_workspace_disk(
    store: &Store,
    task: &str,
    pool: &Path,
    charter_floor: u64,
    wait_for_capacity: bool,
    max_wait_secs: u64,
) -> Result<WorkspaceQueueLease> {
    ensure_dispatcher_namespace(store)?;
    let ledger_path = ledger_path(store)?;
    let key = format!(
        "workspace:{}:{task}:{}",
        store.home().display(),
        Uuid::new_v4()
    );
    let lease = WorkspaceQueueLease {
        ledger: ledger_path.clone(),
        key: key.clone(),
    };
    // Only the filesystem this workspace will use: a full root must not
    // block a pool on a separate volume.
    // Space the held jobs on this filesystem still have to write counts too.
    let pool_key = admission::disk_key(pool)?;
    let fits = |conn: &Connection| -> Result<(bool, u64, u64)> {
        let settings = admission::settings(conn)?;
        let available = admission::disk(pool)?.0;
        let required = settings
            .min_free_disk_bytes
            .max(charter_floor)
            .saturating_add(committed_disk_on(conn, &pool_key, available)?);
        Ok((available >= required, available, required))
    };
    let (enough, available, required) = fits(&ledger(&ledger_path)?)?;
    if enough {
        return Ok(lease);
    }
    if !wait_for_capacity {
        bail!(
            "workspace disk admission refused: {available} bytes available, {required} required; pass wait_for_capacity:true to wait for disk"
        );
    }
    let entry = Queued {
        key: key.clone(),
        home: store.home().to_owned(),
        job_id: task.to_owned(),
        incarnation: Uuid::new_v4().to_string(),
        created_ms: Store::now_ms(),
        supervisor: identity(std::process::id())?,
        kind: Some("workspace".into()),
    };
    ensure!(
        entry.supervisor.is_some(),
        "cannot identify workspace queue owner"
    );
    {
        let conn = ledger(&ledger_path)?;
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result: Result<()> = (|| {
            live_queue(&conn)?;
            conn.execute(
                "INSERT INTO queue(key,record) VALUES(?1,?2)",
                params![key, serde_json::to_string(&entry)?],
            )?;
            Ok(())
        })();
        if result.is_ok() {
            conn.execute_batch("COMMIT")?;
        } else {
            let _ = conn.execute_batch("ROLLBACK");
        }
        result?;
    }
    let deadline = Instant::now() + Duration::from_secs(max_wait_secs);
    loop {
        store.authorize(task, "spawn")?;
        let conn = ledger(&ledger_path)?;
        let queue = live_queue(&conn)?;
        let earlier_workspaces = queue
            .iter()
            .take_while(|q| q.key != key)
            .filter(|q| q.kind.as_deref() == Some("workspace"))
            .count();
        if earlier_workspaces == 0 && fits(&conn)?.0 {
            return Ok(lease);
        }
        if Instant::now() >= deadline {
            bail!("no workspace disk capacity within {max_wait_secs} s");
        }
        thread::sleep(QUEUE_POLL);
    }
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
/// Capacity reservations no live process can be shown to hold: the supervisor
/// is gone, and the payload group is gone or was never recorded, past the start
/// grace. Observation only: the ledger is opened read-only and nothing is
/// released. The command to recover each one is the captain's to run.
pub(crate) fn stale_reservations(store: &Store) -> Result<Vec<Value>> {
    let path = ledger_dir(store)?.join("resources.sqlite");
    if !path.is_file() {
        return Ok(vec![]);
    }
    let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let mut stale = vec![];
    for r in reservations(&conn)? {
        if !(r.heavy || r.demand.is_some()) || Store::now_ms() - r.created_ms <= START_GRACE_MS {
            continue;
        }
        let supervisor = match &r.supervisor {
            Some(p) => same_process(p)?,
            None => false,
        };
        let payload = match &r.payload {
            Some(p) => payload_alive(p, &r.escaped)?,
            None => escaped_alive(&r.escaped)?,
        };
        if supervisor || payload {
            continue;
        }
        // Without a recorded payload no process can be proven absent, so
        // recovery needs force after the captain has checked by hand.
        let mut args = json!({"key": r.key});
        if r.payload.is_none() {
            args["force"] = json!(true);
        }
        stale.push(json!({
            "key": r.key, "job_id": r.job_id, "home": r.home, "heavy": r.heavy,
            "created_ms": r.created_ms, "payload_recorded": r.payload.is_some(),
            "recover": format!("privateer --home {} resource.recover --json '{args}'", r.home.display()),
        }));
    }
    Ok(stale)
}
fn reservation_key(home: &Path, id: &str, incarnation: &str) -> String {
    format!("{}:{id}:{incarnation}", home.display())
}
/// Queue entries in arrival order, dropping those whose supervisor is gone (or
/// never appeared within the start grace) so they cannot block the line.
/// How long the oldest live entry in this host's capacity queue has waited.
pub(crate) fn oldest_wait_ms(store: &Store) -> Result<Option<i64>> {
    let conn = host_ledger(store)?;
    Ok(live_queue(&conn)?
        .first()
        .map(|entry| Store::now_ms() - entry.created_ms))
}
/// A stable key for a job's command, so runs of the same work are comparable
/// across homes without recording the command itself in the shared ledger.
pub fn work_key(command: &[String]) -> String {
    crate::core::hash(&json!(command))
}
/// Remember how long a reservation held capacity, so a later job running the
/// same work can be judged against what this host has actually done with it.
/// Bounded by age and by row count, like every other record in the ledger.
fn record_run(conn: &Connection, reservation: &Reservation, now_ms: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO runs(key,work,task,duration_ms,finished_ms) VALUES(?1,?2,?3,?4,?5)",
        params![
            reservation.key,
            reservation.work,
            reservation.task,
            (now_ms - reservation.created_ms).max(0),
            now_ms
        ],
    )?;
    conn.execute(
        "DELETE FROM runs WHERE finished_ms < ?1",
        [now_ms - RUN_HISTORY_MS],
    )?;
    conn.execute(
        "DELETE FROM runs WHERE rowid <= (SELECT COALESCE(MAX(rowid),0)-?1 FROM runs)",
        [RUN_HISTORY_ROWS],
    )?;
    Ok(())
}
/// The longest run this host has recorded for the same command or the same
/// task within the history window, or None when it has run none.
fn longest_recorded_run(
    conn: &Connection,
    work: Option<&str>,
    task: Option<&str>,
) -> Result<Option<i64>> {
    if work.is_none() && task.is_none() {
        return Ok(None);
    }
    let longest: Option<i64> = conn
        .query_row(
            "SELECT MAX(duration_ms) FROM runs WHERE finished_ms >= ?1
               AND ((?2 IS NOT NULL AND work = ?2) OR (?3 IS NOT NULL AND task = ?3))",
            params![
                Store::now_ms() - RUN_HISTORY_MS,
                work,
                task.map(str::to_owned)
            ],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(longest)
}
/// What holds this host's capacity right now, and whether each holder looks
/// like ordinary work. Reads only the shared ledger and live processes, so
/// any home on this machine may ask; nothing is released or changed.
pub(crate) fn capacity_holders(store: &Store) -> Result<Vec<Holder>> {
    capacity_holders_waiting(store, Duration::from_secs(5))
}

/// The host ledger opened read-only, waiting at most `busy` on a writer;
/// None when the ledger has never been written.
pub(crate) fn ledger_readonly(store: &Store, busy: Duration) -> Result<Option<Connection>> {
    let path = ledger_dir(store)?.join("resources.sqlite");
    if !path.is_file() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(busy)?;
    Ok(Some(conn))
}
/// `capacity_holders`, waiting at most `busy` for a writer holding the ledger.
pub(crate) fn capacity_holders_waiting(store: &Store, busy: Duration) -> Result<Vec<Holder>> {
    let Some(conn) = ledger_readonly(store, busy)? else {
        return Ok(vec![]);
    };
    let now = Store::now_ms();
    let mut holders = vec![];
    for r in reservations(&conn)? {
        // Only reservations admission counts hold capacity: light work with
        // no estimate is not waiting for anything.
        if !(r.heavy || r.demand.is_some()) {
            continue;
        }
        let supervisor_alive = match &r.supervisor {
            Some(p) => same_process(p)?,
            None => false,
        };
        let payload_alive = match &r.payload {
            Some(p) => group_alive(p)?,
            None => false,
        };
        let escaped_alive = escaped_alive(&r.escaped)?;
        let expected_ms = r.expected_ms(&conn)?;
        let age = (now - r.created_ms).max(0);
        let starting = age <= START_GRACE_MS;
        // A holder is unhealthy when nothing supervises what still runs, when
        // only descendants that left the payload's group survive it, when no
        // live process can be shown to hold it at all, or when it has run far
        // longer than this work takes here. Ordinary heavy work is healthy
        // however long it has been running, which is the whole point: a busy
        // machine is not a broken one. A reservation still inside the start
        // grace is judged only on what already runs, since its supervisor
        // may not have recorded itself yet.
        let unhealthy = if !supervisor_alive && (payload_alive || escaped_alive) {
            Some(
                "orphaned: its supervisor is gone while its payload or a descendant still runs"
                    .into(),
            )
        } else if supervisor_alive && !payload_alive && escaped_alive {
            Some("only descendants that left its process group still run".into())
        } else if !starting && !supervisor_alive && !payload_alive && !escaped_alive {
            Some("no live process can be shown to hold this reservation".into())
        } else {
            expected_ms
                .filter(|expected| age > *expected)
                .map(|expected| {
                    format!(
                        "it has held capacity for {} minutes, well past the {} minutes this work takes here",
                        age / 60_000,
                        expected / 60_000
                    )
                })
        };
        holders.push(Holder {
            ship: r.ship(),
            key: r.key,
            job_id: r.job_id,
            home: r.home,
            charter: r.charter,
            heavy: r.heavy,
            started_ms: r.created_ms,
            state: r.state,
            supervisor_alive,
            payload_alive,
            escaped_alive,
            expected_ms,
            unhealthy,
        });
    }
    holders.sort_by_key(|h| h.started_ms);
    Ok(holders)
}
/// A job a live process still runs on this host, as the ledger records it.
pub(crate) struct Running {
    pub ship: String,
    pub job_id: String,
    pub heavy: bool,
    /// Admission counts it: heavy, or started with an estimate.
    pub estimated: bool,
}

/// Every job on this host a live process still runs, from any home, oldest
/// first. Only the ledger and live processes are read.
pub(crate) fn running_jobs(store: &Store) -> Result<Vec<Running>> {
    let path = ledger_dir(store)?.join("resources.sqlite");
    if !path.is_file() {
        return Ok(vec![]);
    }
    let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let mut held = reservations(&conn)?;
    held.sort_by_key(|r| r.created_ms);
    let mut running = vec![];
    for r in held {
        let alive = match &r.supervisor {
            Some(p) if same_process(p)? => true,
            _ => match &r.payload {
                Some(p) => group_alive(p)?,
                None => false,
            },
        };
        if alive {
            running.push(Running {
                ship: r.ship(),
                estimated: r.heavy || r.demand.is_some(),
                heavy: r.heavy,
                job_id: r.job_id,
            });
        }
    }
    Ok(running)
}
/// This home's queued jobs and what they wait behind, as bearings show it:
/// one entry per job still waiting for this machine's capacity. Each names
/// the holder it waits for, the ship that holder belongs to, and whether
/// that holder looks unhealthy.
pub(crate) fn capacity_waits(store: &Store, ship: &str) -> Result<Vec<Value>> {
    let mut statement = store.connection().prepare(
        "SELECT json_extract(record,'$.id'), json_extract(record,'$.task_id'),
           json_extract(record,'$.created_ms'), json_extract(record,'$.heavy')
         FROM jobs WHERE json_extract(record,'$.status')='queued'
           AND json_extract(record,'$.task_id') IN (SELECT id FROM tasks WHERE owner=?1)
         ORDER BY json_extract(record,'$.created_ms') LIMIT 20",
    )?;
    let queued: Vec<(String, String, i64, bool)> = statement
        .query_map([ship], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get::<_, Option<bool>>(3)?.unwrap_or(false),
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    if queued.is_empty() {
        return Ok(vec![]);
    }
    let now = Store::now_ms();
    let holders = capacity_holders(store)?;
    // The holder a queued job waits behind is the live job, of any kind, that
    // has held capacity longest: a crew's memory counts as much as a build's.
    // A queue with no live holder is stated as such rather than blamed on an
    // ordinary job.
    let waiting_behind = holders
        .iter()
        .filter(|h| h.live())
        .min_by_key(|h| h.started_ms);
    let mut waits = vec![];
    for (id, task, created_ms, heavy) in queued {
        let behind = waiting_behind.map(|h| h.to_json(now));
        waits.push(json!({
            "id": id, "task_id": task, "heavy": heavy,
            "waited_ms": (now - created_ms).max(0),
            "behind": behind,
            "line": wait_line(&id, waiting_behind),
        }));
    }
    Ok(waits)
}
/// The sentence a first mate reads for one job waiting for this machine.
fn wait_line(id: &str, holder: Option<&Holder>) -> String {
    let Some(holder) = holder else {
        return format!(
            "{id} is waiting in this machine's capacity queue; nothing running here holds capacity"
        );
    };
    let minutes = holder.age_ms(Store::now_ms()) / 60_000;
    let age = if minutes == 0 {
        "under a minute".to_owned()
    } else {
        format!("{minutes} minutes")
    };
    let kind = if holder.heavy { "heavy job" } else { "job" };
    format!(
        "{id} is waiting behind {ship}'s {kind} {job} (started {age} ago)",
        ship = holder.ship,
        job = holder.job_id,
    )
}
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
/// The disk floors for one reservation, measured on the filesystem the work
/// will use rather than the host root.
struct DiskFloor<'a> {
    charter_min_free_bytes: u64,
    path: &'a Path,
}
/// Live reservations as admission sees them. A reservation's own loss counts
/// the trims made on its filesystem while it ran as space it did not write.
fn held_view<'a>(conn: &Connection, held: &'a [Reservation]) -> Result<Vec<admission::Held<'a>>> {
    held.iter()
        .map(|r| {
            let disk_trimmed = match (&r.disk_key, r.disk_trimmed_at_admit) {
                (Some(key), Some(at)) => admission::trim_total(conn, key)?.saturating_sub(at),
                _ => 0,
            };
            Ok(admission::Held {
                heavy: r.heavy,
                home: &r.home,
                charter: r.charter.as_deref(),
                created_ms: r.created_ms,
                demand: r.demand,
                disk_key: r.disk_key.as_deref(),
                disk_free_at_admit: r.disk_free_bytes,
                disk_trimmed,
                job_id: r.job_id.as_str(),
            })
        })
        .collect()
}

/// Disk the live reservations on one filesystem still have to write.
pub(crate) fn committed_disk_on(conn: &Connection, disk_key: &str, free: u64) -> Result<u64> {
    let held = reservations(conn)?;
    Ok(admission::committed_disk(
        &held_view(conn, &held)?,
        disk_key,
        free,
    ))
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
    heavy_declared: bool,
    charter_cap: Option<u64>,
    floor: DiskFloor<'_>,
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
        {
            let settings = admission::settings(conn)?;
            // Capacity-queue fairness applies to work that needs capacity or
            // already holds a queue position. The disk floor is enforced for
            // every job in `admit` below, so light work with free disk is not
            // blocked behind unrelated queued jobs; a light job below the floor
            // joins the queue and returns here as `from_queue`.
            if from_queue || reservation.heavy || estimate.is_some() {
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
            }
            // Never garbage-collect reservations merely because a wrapper disappeared.
            let held = reservations(conn)?;
            let host = admission::measure()?;
            reservation.demand = admission::demand(
                &settings,
                &host,
                reservation.heavy,
                heavy_declared,
                estimate,
            );
            // Only the filesystem this job will use: a full root must not
            // block work on a separate volume. The same reading decides
            // admission and feeds the falling-space trend the owner sees.
            let disk_key = admission::disk_key(floor.path)?;
            let (disk_available_bytes, disk_total_bytes) = admission::disk(floor.path)?;
            let now = Store::now_ms();
            admission::note_disk(conn, &disk_key, disk_available_bytes, now)?;
            let view = held_view(conn, &held)?;
            admission::admit(
                &settings,
                &host,
                now,
                &view,
                &admission::Request {
                    heavy: reservation.heavy,
                    home: &reservation.home,
                    charter: reservation.charter.as_deref().unwrap_or(""),
                    charter_cap,
                    demand: reservation.demand,
                    charter_min_free_bytes: floor.charter_min_free_bytes,
                    disk_key: &disk_key,
                    disk_available_bytes,
                    disk_total_bytes,
                    released_load: released_load(conn, now)?,
                    started_by: reservation.started_by.as_deref(),
                },
            )?;
            reservation.disk_trimmed_at_admit = Some(admission::trim_total(conn, &disk_key)?);
            reservation.disk_key = Some(disk_key);
            reservation.disk_free_bytes = Some(disk_available_bytes);
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
            if !job.escaped.is_empty() {
                r.escaped = job.escaped.clone();
            }
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
/// How long a finished job's load is credited back: by then the one-minute
/// average has forgotten all but a fraction of a percent of it.
const RELEASED_WINDOW_MS: i64 = 600_000;

fn release(store: &Store, job: &Job) -> Result<()> {
    let conn = ledger(&job.ledger)?;
    let key = reservation_key(store.home(), &job.id, &job.incarnation);
    let held: Option<String> = conn
        .query_row(
            "SELECT record FROM reservations WHERE key=?1",
            [&key],
            |r| r.get(0),
        )
        .optional()?;
    let held = held.and_then(|raw| serde_json::from_str::<Reservation>(&raw).ok());
    let cpus = held
        .as_ref()
        .and_then(|r| r.demand)
        .map(|d| d.cpus)
        .filter(|cpus| *cpus > 0.0);
    // Remember what an ended job adds to the load average, so admission
    // does not wait for the average to forget it.
    if let Some(cpus) = cpus {
        let load = admission::measure().map(|host| host.load).unwrap_or(cpus);
        let now = Store::now_ms();
        conn.execute(
            "INSERT OR REPLACE INTO released(key, cpus, load, released_ms) VALUES(?1, ?2, ?3, ?4)",
            params![key, cpus, load, now],
        )?;
        conn.execute(
            "DELETE FROM released WHERE released_ms < ?1",
            [now - RELEASED_WINDOW_MS],
        )?;
    }
    // And how long it held capacity, so the next job running the same work is
    // judged against this host's own record rather than a fixed multiple.
    if let Some(reservation) = held {
        record_run(&conn, &reservation, Store::now_ms())?;
    }
    conn.execute("DELETE FROM reservations WHERE key=?1", [key])?;
    Ok(())
}

/// The load the average still carries from jobs that ended recently.
fn released_load(conn: &Connection, now_ms: i64) -> Result<f64> {
    let mut statement =
        conn.prepare("SELECT cpus, load, released_ms FROM released WHERE released_ms >= ?1")?;
    let rows = statement.query_map([now_ms - RELEASED_WINDOW_MS], |r| {
        Ok((
            r.get::<_, f64>(0)?,
            r.get::<_, f64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    // Jobs that ended together each count, but together never more than the
    // load there was when the latest of them ended.
    let (mut total, mut cap) = (0.0_f64, 0.0_f64);
    for row in rows {
        let (cpus, load, at) = row?;
        total += admission::released_load(cpus, load, now_ms - at);
        cap = cap.max(admission::released_load(load, load, now_ms - at));
    }
    Ok(total.min(cap))
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
    let boot = boot_id()?;
    Ok(Some(Process {
        pid,
        started: format!("linux:{boot}:{ticks}"),
        pid_namespace: pid_namespace().map(str::to_owned),
        zombie: matches!(*state, "Z" | "X"),
    }))
}
/// This kernel's boot id. It is constant until reboot, shared by every PID
/// namespace on the machine; supervisors poll identity, so read it once.
#[cfg(target_os = "linux")]
fn boot_id() -> Result<&'static str> {
    static BOOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    Ok(match BOOT.get() {
        Some(boot) => boot,
        None => {
            let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .context("inspect boot identity")?;
            BOOT.get_or_init(|| boot.trim().to_owned())
        }
    })
}
/// Whether a recorded identity names a process of another boot: a reboot of
/// this machine, or another machine the record moved from. No such process
/// runs here, whatever PID namespace recorded it. Identities without a boot
/// id (legacy, or not Linux) cannot be placed and are not judged here.
#[cfg(target_os = "linux")]
fn other_boot(process: &Process) -> bool {
    let Some((boot, _)) = process
        .started
        .strip_prefix("linux:")
        .and_then(|rest| rest.rsplit_once(':'))
    else {
        return false;
    };
    boot_id().is_ok_and(|current| current != boot)
}
#[cfg(not(target_os = "linux"))]
fn other_boot(process: &Process) -> bool {
    // A Linux kernel's process cannot be running on this one.
    process.started.starts_with("linux:")
}
#[cfg(not(target_os = "linux"))]
fn identity(pid: u32) -> Result<Option<Process>> {
    // lstart is rendered in local time and locale; pin both.
    let output = Command::new("/bin/ps")
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
        pid_namespace: None,
        zombie,
    }))
}
/// The start identity of a live process, or None once it has exited. A PID
/// that was reused for another process has a different identity.
pub(crate) fn process_identity(pid: u32) -> Result<Option<String>> {
    Ok(identity(pid)?.filter(|p| !p.zombie).map(|p| p.started))
}

/// This process's identity, recorded with a claim another process may check.
pub(crate) fn own_identity() -> Result<String> {
    let me = identity(std::process::id())?.context("cannot identify this process")?;
    Ok(serde_json::to_string(&me)?)
}
/// Whether the process an `own_identity` record names is still running. One
/// in another PID namespace of this boot counts as running, since it cannot
/// be checked.
pub(crate) fn identity_alive(recorded: &str) -> Result<bool> {
    same_process(&serde_json::from_str(recorded)?)
}

/// The PID namespace this process sees PID numbers in; constant for its life.
#[cfg(target_os = "linux")]
fn pid_namespace() -> Option<&'static str> {
    static NAMESPACE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    NAMESPACE
        .get_or_init(|| {
            fs::read_link("/proc/self/ns/pid")
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        })
        .as_deref()
}
#[cfg(not(target_os = "linux"))]
fn pid_namespace() -> Option<&'static str> {
    None
}
/// Whether a recorded PID can be inspected here. Records without a namespace
/// (legacy, or not Linux) are probed as before.
fn addressable(process: &Process) -> bool {
    process.pid_namespace.is_none() || process.pid_namespace.as_deref() == pid_namespace()
}
/// Work is recorded only in the PID namespace of this home's dispatcher, which
/// settles it: a record made inside a PID sandbox could never be inspected,
/// settled or cancelled from outside it. Until a dispatcher has run, any
/// namespace may record.
fn ensure_dispatcher_namespace(store: &Store) -> Result<()> {
    let dispatcher: Option<String> = store
        .connection()
        .query_row(
            "SELECT value FROM metadata WHERE key='pid_namespace'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    ensure!(
        dispatcher.is_none() || dispatcher.as_deref() == pid_namespace(),
        "this caller is in another PID namespace than the home's dispatcher; run job and workspace commands outside the PID sandbox"
    );
    Ok(())
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
    if other_boot(process) {
        return Ok(false);
    }
    // Probing the same number here could find an unrelated process, or none,
    // and falsely prove the recorded one gone: treat it as alive.
    if !addressable(process) {
        return Ok(true);
    }
    Ok(identity(process.pid)?
        .is_some_and(|p| !p.zombie && same_start(&process.started, &p.started)))
}
/// Whether the live process `pid` is the one a job record names as its
/// supervisor: the same PID, start identity and PID namespace. A reused PID,
/// or a process in another namespace, is not.
pub(crate) fn is_recorded_supervisor(recorded: &Value, pid: u32) -> Result<bool> {
    let Ok(recorded) = serde_json::from_value::<Process>(recorded.clone()) else {
        return Ok(false);
    };
    if recorded.pid != pid || recorded.pid_namespace.as_deref() != pid_namespace() {
        return Ok(false);
    }
    same_process(&recorded)
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
    if other_boot(process) {
        return Ok(false);
    }
    if !addressable(process) {
        return Ok(true);
    }
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
/// Whether a command line is a Privateer process supervised on its own: a
/// job's supervisor, a dispatcher or a front door. A job started from inside
/// another job belongs to its own supervisor, never to the job that asked.
fn independently_supervised(args: &str) -> bool {
    let words: Vec<&str> = args.split_whitespace().collect();
    words.contains(&"_job-supervise")
        || words.last() == Some(&"serve")
        || words.windows(2).any(|w| w == ["door", "run"])
}
/// Live processes as (pid, parent, process group, independently supervised).
fn process_table() -> Result<Vec<(u32, u32, u32, bool)>> {
    let output = Command::new("ps")
        .args(["-eo", "pid=,ppid=,pgid=,stat=,args="])
        .output()?;
    ensure!(output.status.success(), "process inspection failed");
    let mut table = vec![];
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(parent), Some(group), Some(stat)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if stat.starts_with('Z') {
            continue;
        }
        let args = fields.collect::<Vec<_>>().join(" ");
        table.push((
            pid.parse()?,
            parent.parse()?,
            group.parse()?,
            independently_supervised(&args),
        ));
    }
    Ok(table)
}
/// The payload's descendants outside its process group, with their start
/// identities: those already recorded that still live, and any new ones
/// found from the payload or from them. A process that left the group and
/// whose parent then exited is reparented and can only be found while it
/// was recorded first, so this runs while the job runs.
fn escaped_descendants(payload: &Process, known: &[Process]) -> Result<Vec<Process>> {
    let mut found: Vec<Process> = vec![];
    let mut roots = vec![];
    if same_process(payload)? {
        roots.push(payload.pid);
    }
    for process in known {
        if same_process(process)? {
            roots.push(process.pid);
            found.push(process.clone());
        }
    }
    if roots.is_empty() {
        return Ok(found);
    }
    let table = process_table()?;
    let mut children: std::collections::HashMap<u32, Vec<(u32, u32)>> = Default::default();
    for (pid, parent, group, independent) in table {
        // Independently supervised processes and everything under them are
        // their own jobs' business.
        if !independent {
            children.entry(parent).or_default().push((pid, group));
        }
    }
    let mut seen: std::collections::HashSet<u32> = roots.iter().copied().collect();
    while let Some(parent) = roots.pop() {
        for &(pid, group) in children.get(&parent).map(Vec::as_slice).unwrap_or(&[]) {
            if !seen.insert(pid) {
                continue;
            }
            roots.push(pid);
            if group != payload.pid && found.len() < 256 && !found.iter().any(|f| f.pid == pid) {
                if let Some(process) = identity(pid)?.filter(|p| !p.zombie) {
                    found.push(process);
                }
            }
        }
    }
    Ok(found)
}
fn escaped_alive(escaped: &[Process]) -> Result<bool> {
    for process in escaped {
        if same_process(process)? {
            return Ok(true);
        }
    }
    Ok(false)
}
/// Signal recorded escaped descendants that are still the same processes:
/// a group leader with its whole group, any other alone.
fn signal_escaped(escaped: &[Process], signal: i32) -> Result<()> {
    for process in escaped {
        if same_process(process)? {
            let pid = process.pid as i32;
            let target = if unsafe { libc::getpgid(pid) } == pid {
                -pid
            } else {
                pid
            };
            unsafe { libc::kill(target, signal) };
        }
    }
    Ok(())
}
/// Whether anything the payload started still runs: its group, or a
/// recorded descendant that left it.
fn payload_alive(payload: &Process, escaped: &[Process]) -> Result<bool> {
    Ok(group_alive(payload)? || escaped_alive(escaped)?)
}
const UNPROVEN_LEADER: &str = "payload leader missing or changed; cannot discover group ownership";
/// Snapshot live payload group members. Returns None when the leader cannot
/// currently be proven to own the group (for example caught mid-exit);
/// inspection failures remain errors.
///
/// `reserved` means the caller holds the leader as its own child, not yet
/// reaped (alive, or exited and observed with waitid), which by itself keeps
/// the PID and process group id from being reused, so the leader's identity
/// is not re-inspected.
fn capture_anchors(process: &Process, reserved: bool) -> Result<Option<Vec<Process>>> {
    if !addressable(process) {
        return Ok(None);
    }
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
/// Signal the payload's group. `reserved` means the caller is the leader's
/// parent and has not reaped it: alive or exited, the leader keeps its PID and
/// so its group id from reuse, and nothing else needs to anchor the group. A
/// leader that exits just before the signal is then nothing left to signal,
/// not a stale group.
fn signal_group(process: &Process, signal: i32, anchors: &[Process], reserved: bool) -> Result<()> {
    ensure!(
        addressable(process),
        "payload process group belongs to another PID namespace; refusing signal"
    );
    // A still-live recorded member anchors this exact group even if its
    // leader exits during TERM grace. Never signal solely by stale PGID.
    let mut anchored = reserved;
    if !anchored {
        for anchor in std::iter::once(process).chain(anchors.iter()) {
            if same_process(anchor)?
                && unsafe { libc::getpgid(anchor.pid as i32) } == process.pid as i32
            {
                anchored = true;
                break;
            }
        }
    }
    ensure!(
        anchored,
        "payload leader/recorded members missing or changed; refusing stale process-group signal"
    );
    let rc = unsafe { libc::kill(-(process.pid as i32), signal) };
    let error = std::io::Error::last_os_error().raw_os_error();
    // macOS refuses (EPERM) a group left with only its unreaped leader.
    ensure!(
        rc == 0
            || error == Some(libc::ESRCH)
            || (reserved && error == Some(libc::EPERM) && !group_alive(process)?),
        "cannot signal payload group"
    );
    Ok(())
}
/// Begin ending a supervised payload: record its group's live members and
/// the descendants that left it, then TERM them. `reserved` as for
/// `signal_group`: the supervisor has not reaped the leader.
fn terminate_payload(
    store: &Store,
    id: &str,
    payload: &Process,
    known: &[Process],
    reserved: bool,
) -> Result<Job> {
    let anchors = capture_anchors(payload, reserved)?.context(UNPROVEN_LEADER)?;
    let escaped = escaped_descendants(payload, known)?;
    let job = alter(store, id, |j| {
        j.group_anchors = anchors;
        j.escaped = escaped;
    })?;
    update_reservation(store, &job)?;
    signal_group(payload, libc::SIGTERM, &job.group_anchors, reserved)?;
    // Killing the payload's group is not proof its work stopped.
    signal_escaped(&job.escaped, libc::SIGTERM)?;
    Ok(job)
}
/// `terminal` as an SQL list. A query must repeat the `jobs_live` condition
/// with it word for word for SQLite to use that index.
pub(crate) const TERMINAL_SQL: &str = "('succeeded','failed','cancelled','timed_out','resumed')";
/// `is_harness` in SQL, word for word as the `jobs_finished` index has it.
pub(crate) const HARNESS_SQL: &str = "json_extract(record,'$.action')='spawn'";
fn terminal(status: &str) -> bool {
    // `resumed`: a parked crew whose work moved to the job its park resumed.
    matches!(
        status,
        "succeeded" | "failed" | "cancelled" | "timed_out" | "resumed"
    )
}
/// Statuses a crew outcome reports. An orphan is no longer supervised (its
/// deadline is not enforced), so its first mate hears about it then; the
/// settlement that may follow is not reported again. A parked crew reports
/// nothing here: its park notice is quiet, and its resume carries on.
fn reported(status: &str) -> bool {
    (terminal(status) && status != "resumed") || status == "orphaned"
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
/// The estimate a crew launch is admitted under. Its memory is the route's
/// own figure, else this host's setting for its harness, else the harness
/// manifest's, else the default. A memory figure the request asks for counts
/// too, and the larger of the two is kept, so a request cannot admit a crew
/// its route sized bigger.
pub(crate) fn crew_estimate(
    store: &Store,
    charter: &Value,
    args: &Value,
    route_memory: Option<Value>,
) -> Result<Estimate> {
    let requested = args
        .get("estimate")
        .filter(|v| !v.is_null())
        .map(admission::estimate)
        .transpose()?;
    let (harness, manifest) = crate::plugins::launch_manifest(charter, args)?;
    let route = route_memory
        .map(|v| {
            v.as_u64()
                .filter(|bytes| *bytes > 0)
                .context("options.memory_bytes must be a positive number of bytes")
        })
        .transpose()?;
    let settings = admission::settings(&shared_ledger(store)?)?;
    let memory = admission::crew_memory(&settings, &harness, manifest, route);
    let mut estimate = requested.unwrap_or_default();
    estimate.memory_bytes = Some(
        estimate
            .memory_bytes
            .map_or(memory, |asked| asked.max(memory)),
    );
    Ok(estimate)
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
    ensure_dispatcher_namespace(store)?;
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
    let heavy_declared = args.get("heavy").and_then(Value::as_bool).is_some();
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
    // Descendants left behind by a finished leader are usually leaks (a test
    // helper, a stray watcher) that would hold the job's capacity until
    // someone cancels it. A caller that means to leave them running opts out.
    let cleanup_descendants = args
        .get("cleanup_descendants")
        .map(|value| {
            value
                .as_bool()
                .context("cleanup_descendants must be boolean")
        })
        .transpose()?
        .unwrap_or(true);
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
    // An agent inside a supervised job (crew, or a first mate's wake) is found
    // through its supervisor; remember which of its jobs started this one.
    let started_by = match (store.caller(), &agent_actor) {
        (Caller::Agent { .. }, Some(actor)) => crate::crew_scope::job_id().filter(|parent| {
            load(store, parent).is_ok_and(|p| p.agent_actor.as_deref() == Some(actor))
        }),
        _ => None,
    };
    // A crew's resume names the parked crew it continues. Only the runtime's
    // own launch (an operator caller) may say so.
    let resume_of = matches!(store.caller(), Caller::Operator)
        .then(|| args.get("_resume_of").and_then(Value::as_str))
        .flatten();
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
        crate::workspace::WorkspaceWait {
            wait_for_capacity,
            max_wait_secs: max_wait_secs.unwrap_or(DEFAULT_MAX_WAIT_SECS),
        },
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
        heavy_declared,
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
        escaped: vec![],
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
        foreign: None,
        work_offer: args
            .get("work_offer")
            .and_then(Value::as_str)
            .map(str::to_owned),
        resume_of: resume_of.map(str::to_owned),
        hosted: agent && args.get("hosted").and_then(Value::as_bool) == Some(true),
        spawned_ms: None,
        model_route: args
            .get("model_route")
            .and_then(Value::as_str)
            .filter(|_| agent)
            .map(str::to_owned),
        route_name: args
            .get("route_name")
            .and_then(Value::as_str)
            .filter(|_| agent)
            .map(str::to_owned),
        route_why: args
            .get("route_why")
            .and_then(Value::as_str)
            .filter(|_| agent)
            .map(str::to_owned),
        usage: None,
        usage_partial: false,
        codex_read_only_crew: args
            .get("codex_read_only_crew")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        // Only the agent path records a launch, and only the runtime's own
        // resume carries an earlier run time: see `crew::resume`.
        run_ms: if agent {
            args.get("_run_ms").and_then(Value::as_u64).unwrap_or(0)
        } else {
            0
        },
        park: None,
        launch: agent.then(|| args.get("launch").cloned()).flatten(),
        quiet: false,
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
        started_by: job.started_by.clone(),
        demand: None,
        disk_key: None,
        disk_free_bytes: None,
        disk_trimmed_at_admit: None,
        escaped: vec![],
        wake: is_wake(store, &id)?,
        timeout_ms: Some(job.timeout_secs as i64 * 1000),
        ship: store
            .task(task_id)
            .ok()
            .and_then(|task| task["owner"].as_str().map(str::to_owned)),
        work: Some(work_key(&job.command)),
        task: Some(job.task_id.clone()),
    };
    // Trimming does file I/O, so it runs before the write transaction below.
    let charter_floor = charter
        .get("min_free_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(2 * 1024 * 1024 * 1024);
    let demand = admission::demand(
        &admission::settings(&shared_ledger(store)?)?,
        &admission::measure()?,
        heavy,
        heavy_declared,
        estimate.as_ref(),
    );
    crate::workspace::make_room_for_job(
        store,
        workspace._lock.as_ref(),
        &job.cwd,
        charter_floor,
        demand,
    )?;
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
            resume_of,
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
                    && existing.work_offer == job.work_offer
                    && existing.resume_of == job.resume_of,
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
            heavy_declared,
            charter_cap,
            DiskFloor {
                charter_min_free_bytes: charter
                    .get("min_free_bytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(2 * 1024 * 1024 * 1024),
                path: &job.cwd,
            },
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
                    kind: None,
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
    let mut reply = public_job(&job);
    // A crew waiting on its heavy job parks rather than polls for the outcome.
    if crate::crew::may_park(store) {
        reply["park"] = json!(format!(
            "Do not poll for this job. Run crew.park with {{\"job_id\":\"{id}\",\"next\":\"<one line>\"}} and end your turn; the outcome resumes you."
        ));
    }
    Ok(reply)
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
            started_by: job.started_by.clone(),
            demand: None,
            disk_key: None,
            disk_free_bytes: None,
            disk_trimmed_at_admit: None,
            escaped: vec![],
            wake: is_wake(store, &job.id)?,
            timeout_ms: Some(job.timeout_secs as i64 * 1000),
            ship: store
                .task(&job.task_id)
                .ok()
                .and_then(|task| task["owner"].as_str().map(str::to_owned)),
            work: Some(work_key(&job.command)),
            task: Some(job.task_id.clone()),
        };
        let cap = charter.get("max_heavy_jobs").and_then(Value::as_u64);
        match reserve(
            &ledger(&job.ledger)?,
            &mut reservation,
            job.estimate.as_ref(),
            job.heavy_declared,
            cap,
            DiskFloor {
                charter_min_free_bytes: charter
                    .get("min_free_bytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(2 * 1024 * 1024 * 1024),
                path: &job.cwd,
            },
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

fn persist_reply(line: &[u8], path: &Path) -> Result<()> {
    if let Ok(event) = serde_json::from_slice::<Value>(line)
        && let Some(reply) = crate::reply::text(&event)
    {
        let bounded: String = reply.chars().take(16_000).collect();
        let temporary = path.with_extension("reply.tmp");
        let mut saved = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        saved.write_all(bounded.as_bytes())?;
        saved.sync_all()?;
        fs::rename(&temporary, path)?;
        OpenOptions::new()
            .read(true)
            .open(path.parent().context("reply has no parent")?)?
            .sync_all()?;
    }
    Ok(())
}

/// Copy a pipe into `path`, bounded to about `LOG_LIMIT`. The log file is the
/// live record: each byte read is written to it before the next read, so a
/// deadline, cancel or supervisor kill keeps everything printed so far. The
/// head is kept as written. When the tail fills, its older bytes are dropped
/// and a truncation marker sits between head and tail, rewritten in place.
/// Returns the number of bytes dropped between head and tail.
fn capture(
    mut pipe: impl Read + std::os::fd::AsRawFd + Send + 'static,
    path: PathBuf,
    reply_path: Option<PathBuf>,
    stop: Arc<AtomicBool>,
) -> Result<thread::JoinHandle<Result<u64>>> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(
        flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
        "cannot bound log reader"
    );
    let mut file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)?;
    Ok(thread::spawn(move || {
        let mut head = 0usize;
        // Length of the truncation marker between head and tail, once any.
        let mut marker_len = 0u64;
        let mut tail_len = 0usize;
        let mut dropped = 0u64;
        let mut buf = [0u8; 8192];
        let mut stopping_bytes = 0usize;
        let mut event_line = Vec::new();
        let mut dropping_line = false;
        loop {
            if stop.load(Ordering::Relaxed) && stopping_bytes >= 65536 {
                break;
            }
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Some(reply_path) = &reply_path {
                        for byte in &buf[..n] {
                            if *byte == b'\n' {
                                if !dropping_line {
                                    persist_reply(&event_line, reply_path)?;
                                }
                                event_line.clear();
                                dropping_line = false;
                            } else if !dropping_line {
                                if event_line.len() < 1_048_576 {
                                    event_line.push(*byte);
                                } else {
                                    event_line.clear();
                                    dropping_line = true;
                                }
                            }
                        }
                    }
                    let take = n.min(LOG_HEAD - head);
                    file.write_all(&buf[..take])?;
                    head += take;
                    let mut rest = &buf[take..n];
                    while !rest.is_empty() {
                        let chunk = rest.len().min(TAIL_FULL - tail_len);
                        file.write_all(&rest[..chunk])?;
                        tail_len += chunk;
                        rest = &rest[chunk..];
                        if tail_len >= TAIL_FULL {
                            // Keep the newest LOG_SEGMENT tail bytes and rewrite
                            // them after the head, behind a marker for the cut.
                            let keep = LOG_SEGMENT;
                            let mut kept = vec![0u8; keep];
                            let kept_at = head as u64 + marker_len + (tail_len - keep) as u64;
                            file.seek(SeekFrom::Start(kept_at))?;
                            file.read_exact(&mut kept)?;
                            dropped += (tail_len - keep) as u64;
                            let marker = truncation_marker(dropped);
                            file.set_len(head as u64)?;
                            file.seek(SeekFrom::Start(head as u64))?;
                            file.write_all(&marker)?;
                            file.write_all(&kept)?;
                            marker_len = marker.len() as u64;
                            tail_len = keep;
                        }
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
        if let Some(reply_path) = &reply_path
            && !dropping_line
            && !event_line.is_empty()
        {
            persist_reply(&event_line, reply_path)?;
        }
        file.sync_all()?;
        Ok(dropped)
    }))
}

/// The line between head and tail that says how many bytes were dropped. It
/// is its own line, so JSON-lines readers skip the cut edges.
const TRUNCATED: &str = "privateer.log.truncated";
fn truncation_marker(dropped: u64) -> Vec<u8> {
    format!(
        "\n{}\n",
        json!({"type": TRUNCATED, "dropped_bytes": dropped})
    )
    .into_bytes()
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
                    || job
                        .payload
                        .as_ref()
                        .and_then(|p| payload_alive(p, &job.escaped).ok())
                        == Some(false);
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
    // A crew parked on this job resumes once it settles, whichever path ended
    // it, including a supervisor error that settled it above.
    let settle =
        crate::crew::settle_own_park(store, id).and_then(|()| crate::crew::settled(store, id));
    if let Err(error) = settle {
        let _ = store.audit(
            "crew.resume_failed",
            &json!({"job_id": id, "error": format!("{error:#}")}),
        );
    }
    result
}

#[cfg(test)]
mod capacity_wait_tests {
    use super::*;

    #[test]
    fn a_light_holder_is_named_as_a_job_not_a_heavy_job() {
        let holder = Holder {
            key: "key".into(),
            job_id: "crew-job".into(),
            ship: "apps".into(),
            home: PathBuf::new(),
            charter: None,
            heavy: false,
            started_ms: Store::now_ms(),
            state: "running".into(),
            supervisor_alive: true,
            payload_alive: true,
            escaped_alive: false,
            expected_ms: None,
            unhealthy: None,
        };
        assert_eq!(
            wait_line("queued", Some(&holder)),
            "queued is waiting behind apps's job crew-job (started under a minute ago)"
        );
    }
}

#[cfg(test)]
mod reply_capture_tests {
    use super::*;

    #[test]
    fn rotated_pi_log_needs_the_reply_captured_from_the_intact_stream() {
        let rotated = include_str!("../tests/fixtures/errand-rotated-pi.log");
        // The old fallback treated the progress turn as the verdict. The
        // shared extractor ignores toolUse, and the final record is cut.
        assert_eq!(crate::errand::last_reply(rotated), None);

        let temp = tempfile::tempdir().unwrap();
        let sidecar = temp.path().join("reply");
        let progress = br#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"progress"}],"stopReason":"toolUse"}}"#;
        persist_reply(progress, &sidecar).unwrap();
        assert!(!sidecar.exists());
        let final_reply = br#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"final verdict"}],"stopReason":"stop"}}"#;
        persist_reply(final_reply, &sidecar).unwrap();
        assert_eq!(fs::read_to_string(sidecar).unwrap(), "final verdict");
    }

    #[test]
    fn stream_without_a_completed_reply_does_not_create_a_verdict() {
        let temp = tempfile::tempdir().unwrap();
        let sidecar = temp.path().join("reply");
        persist_reply(br#"{"type":"agent_settled","aborted":false}"#, &sidecar).unwrap();
        assert!(!sidecar.exists());
    }

    #[test]
    fn every_harness_reports_usage_in_one_shape_and_nothing_stays_unknown() {
        let native = json!({"input": 8, "output": 3366, "cacheRead": 308807, "cacheWrite": 4943,
            "totalTokens": 317124});
        assert_eq!(normalize_usage(&native), Some(native.clone()));
        // Codex counts its cached tokens inside its input.
        let codex = json!({"input_tokens": 1200, "cached_input_tokens": 1000, "output_tokens": 80});
        assert_eq!(
            normalize_usage(&codex),
            Some(
                json!({"input": 200, "output": 80, "cacheRead": 1000, "cacheWrite": 0,
                "totalTokens": 1280})
            )
        );
        // OpenCode reports its reasoning apart from its output.
        let opencode =
            json!({"input": 3, "output": 2, "reasoning": 1, "cacheRead": 4, "cacheWrite": 0});
        assert_eq!(
            normalize_usage(&opencode),
            Some(
                json!({"input": 3, "output": 3, "cacheRead": 4, "cacheWrite": 0,
                "totalTokens": 10})
            )
        );
        assert_eq!(normalize_usage(&json!({"thinking": 4})), None);
        assert_eq!(normalize_usage(&json!({})), None);
    }

    #[test]
    fn a_log_that_dropped_bytes_says_so_where_they_were() {
        let marker = String::from_utf8(truncation_marker(4096)).unwrap();
        assert!(log_cut(&format!(
            "{{\"type\":\"turn.started\"}}{marker}tail"
        )));
        assert!(!log_cut(
            "{\"type\":\"turn.completed\",\"note\":\"privateer.log.truncated\"}\n"
        ));
    }
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
    // No job inherits its starter's credential paths or MCP selection. A bounded
    // launch adds only its own selected bundles' paths and servers, after
    // checking their files. Bytes, not str: a non-UTF-8 name must be scrubbed too.
    for (name, _) in std::env::vars_os() {
        let bytes = name.as_encoded_bytes();
        if bytes.starts_with(b"PRIVATEER_CREDENTIAL_") || bytes.starts_with(b"PRIVATEER_MCP_") {
            cmd.env_remove(name);
        }
    }
    // A crew is not attended by its first mate: the presence and harness
    // markers that first mate's session set do not reach its payload.
    if crate::crew_scope::is_crew_launch(store, &job.id, &job.action, job.agent_actor.is_some())? {
        for name in CREW_UNATTENDED {
            cmd.env_remove(name);
        }
        // A crew fetches and commits; its first mate pushes (crew-role.md).
        let gh_config = dir.join("gh");
        fs::create_dir_all(&gh_config)?;
        fs::set_permissions(&gh_config, fs::Permissions::from_mode(0o700))?;
        crate::crew_launch::scrub_push_credentials(&mut cmd, &gh_config);
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
        j.spawned_ms = Some(Store::now_ms());
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
                Some(dir.join("reply")),
                stop.clone(),
            )?),
            Some(capture(
                child.stderr.take().context("stderr missing")?,
                dir.join("stderr.log"),
                None,
                stop.clone(),
            )?),
        )
    };
    let spawned = Instant::now();
    const PARK_GRACE: Duration = Duration::from_secs(5);
    // Each incarnation gets a fresh grant of the charter's budget. Queued and
    // parked time is not in it; the total is bounded by the park and 24 h caps
    // (see `crew::park_at_deadline`), not by summing run time across resumes.
    let deadline = spawned + Duration::from_secs(job.timeout_secs);
    let mut outcome = "succeeded";
    let mut reason = None;
    // Why a crew at its deadline could not park: the job then times out with it.
    let mut park_refused = None;
    // When the crew's park was first seen: its harness has this long to end
    // its turn before the payload is signalled.
    let mut park_seen: Option<Instant> = None;
    let mut terminated_at = None;
    let mut leader_status = None;
    let mut killed = false;
    let mut last_anchor_snapshot = Instant::now() - Duration::from_secs(1);
    let mut last_escape_snapshot = Instant::now() - Duration::from_secs(1);
    let status = loop {
        // Track descendants that leave the payload's group while they can
        // still be found from it; they keep the job alive until they end.
        if terminated_at.is_none() && last_escape_snapshot.elapsed() >= Duration::from_millis(500) {
            let found = escaped_descendants(&payload, &job.escaped)?;
            if found != job.escaped {
                job = alter(store, id, |j| j.escaped = found.clone())?;
                update_reservation(store, &job)?;
            }
            last_escape_snapshot = Instant::now();
        }
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
            let group = group_alive(&payload)?;
            if !group && !escaped_alive(&job.escaped)? {
                break status;
            }
            if terminated_at.is_none() {
                if !job.cleanup_descendants {
                    break status;
                }
                // Harness-owned helper descendants are optional completion
                // cleanup, not a cancelled outcome. Only recorded live group
                // members may anchor TERM/KILL after the leader was reaped.
                if group {
                    if let Err(error) =
                        signal_group(&payload, libc::SIGTERM, &job.group_anchors, false)
                    {
                        if group_alive(&payload)? {
                            reason = Some(format!("descendant cleanup refused: {error}"));
                        }
                        break status;
                    }
                }
                signal_escaped(&job.escaped, libc::SIGTERM)?;
                store.audit("job.descendants_cleanup", &json!({"job_id":id}))?;
                terminated_at = Some(Instant::now());
            }
        }
        let authority_error = store
            .authorize_running(&job.task_id, &job.action)
            .and_then(|charter| check_plugin_binding(&job, &charter))
            .and_then(|()| check_workspace_owner(store, &job))
            .err();
        // A parked crew's payload ends here: its work waits on a child job, and
        // the harness has nothing left to say until that job settles. A crew
        // whose budget runs out parks on the job it waits on, or is held for
        // its first mate when no job is outstanding.
        let parking = match job.park.clone() {
            Some(park) => Some(park),
            None if terminated_at.is_none() && Instant::now() >= deadline => {
                // A park that cannot be opened leaves the job to time out, as
                // it did before; the failure is audited, not a supervisor error.
                match crate::crew::park_at_deadline(store, id) {
                    Ok(park) => park,
                    Err(error) => {
                        store.audit(
                            "crew.park_failed",
                            &json!({"job_id": id, "error": format!("{error:#}")}),
                        )?;
                        park_refused = Some(format!("{error:#}"));
                        None
                    }
                }
            }
            None => None,
        };
        if parking.is_some() && park_seen.is_none() {
            park_seen = Some(Instant::now());
        }
        let grace = park_seen.is_some_and(|seen| seen.elapsed() < PARK_GRACE);
        if terminated_at.is_none()
            && (job.cancel_requested
                || authority_error.is_some()
                || (!grace && (parking.is_some() || Instant::now() >= deadline)))
        {
            outcome = if job.cancel_requested || authority_error.is_some() {
                "cancelled"
            } else if parking.is_some() {
                "parked"
            } else {
                "timed_out"
            };
            reason = Some(if let Some(error) = authority_error {
                format!("authority ended: {error}")
            } else if job.cancel_requested {
                "cancellation requested".into()
            } else if let Some(park) = &parking {
                format!("crew parked: {park}")
            } else if let Some(refused) = &park_refused {
                format!("job deadline reached; {refused}")
            } else {
                "job deadline reached".into()
            });
            // A harness ends its turn after `crew.park`, so its payload often
            // ends on its own just as the park is noticed. A group that is
            // already gone is not signalled: refusing it as stale would fail
            // a park that did what it should.
            // It can also end just after that check. Until this loop reaps
            // the leader its PID and group id stay reserved, so its group is
            // signalled on that proof rather than refused as stale
            // (efe29e60, a7fa0853, 2026-10-19).
            if outcome == "parked" && !payload_alive(&payload, &job.escaped)? {
                terminated_at = Some(Instant::now());
            } else {
                job =
                    terminate_payload(store, id, &payload, &job.escaped, leader_status.is_none())?;
                terminated_at = Some(Instant::now());
            }
        }
        if let Some(at) = terminated_at {
            if !killed && at.elapsed() >= Duration::from_secs(2) {
                if group_alive(&payload)? {
                    signal_group(
                        &payload,
                        libc::SIGKILL,
                        &job.group_anchors,
                        leader_status.is_none(),
                    )?;
                }
                signal_escaped(&job.escaped, libc::SIGKILL)?;
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
    let group_remains = payload_alive(&payload, &job.escaped)?;
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
    // Process time is the crew's budget; the time it waited is not in it.
    let spent = u64::try_from(spawned.elapsed().as_millis()).unwrap_or(u64::MAX);
    job = alter(store, id, |j| j.run_ms = j.run_ms.saturating_add(spent))?;
    // A crew that parked is parked whatever its harness exited with, unless
    // it was cancelled or ran out of budget first.
    if let Some(park) = job
        .park
        .clone()
        .filter(|_| matches!(outcome, "succeeded" | "failed" | "parked"))
    {
        job = alter(store, id, |j| {
            j.status = "parked".into();
            j.exit_code = status.code();
            j.stdout_dropped_bytes = stdout_dropped;
            j.stderr_dropped_bytes = stderr_dropped;
            j.reason = Some(format!("crew parked: {park}"));
        })?;
        // Its capacity is given back while it waits; a group still running
        // keeps its reservation, as for any job.
        if group_remains {
            update_reservation(store, &job)?;
        } else {
            release(store, &job)?;
        }
        store.audit("job.parked", &json!({"job_id": id, "park": park}))?;
        return Ok(());
    }
    if outcome == "succeeded" && !status.success() {
        outcome = "failed";
    }
    if outcome == "succeeded" && job.codex_read_only_crew {
        let reported: bool = store.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM mail WHERE json_extract(payload,'$.job_id')=?1
                AND json_extract(payload,'$.incarnation')=?2 AND json_extract(payload,'$.report') IS NOT NULL)",
            params![job.id, job.incarnation],
            |row| row.get(0),
        )?;
        if !reported {
            outcome = "failed";
            reason = Some("Codex read-only crew exited without a durable crew report".into());
        }
    }
    if group_remains {
        job = alter(store, id, |j| {
            j.status = "orphaned".into();
            j.exit_code = status.code();
            j.finished_ms = Some(Store::now_ms());
            j.stdout_dropped_bytes = stdout_dropped;
            j.stderr_dropped_bytes = stderr_dropped;
            j.leader_result = Some(LeaderResult {
                status: outcome.into(),
                reason: reason.clone(),
            });
            j.reason = Some(reason.unwrap_or_else(|| {
                "payload group or a descendant that left it is still running; reservation preserved".into()
            }));
        })?;
        update_reservation(store, &job)?;
    } else {
        // Release before the terminal status commits, so a reader who sees the
        // job finished also sees its capacity given back. The outcome is
        // recorded as the leader's result first: if the supervisor dies between
        // the two writes, reconcile settles to that outcome, not "disappeared".
        job = alter(store, id, |j| {
            j.leader_result = Some(LeaderResult {
                status: outcome.into(),
                reason: reason.clone(),
            });
        })?;
        release(store, &job)?;
        job = alter(store, id, |j| {
            j.status = outcome.into();
            j.leader_result = None;
            j.exit_code = status.code();
            j.finished_ms = Some(Store::now_ms());
            j.stdout_dropped_bytes = stdout_dropped;
            j.stderr_dropped_bytes = stderr_dropped;
            j.reason = reason;
        })?;
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
            Some(p) => !payload_alive(p, &job.escaped)?,
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
    if job.status == "parked" {
        // A park has no payload to supervise. Its record outlives the
        // dispatcher, and it resumes when the job it waits on settles.
        return Ok(job);
    }
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
        Some(p) => payload_alive(p, &job.escaped)?,
        None => false,
    };
    if !supervisor_alive && job.payload.is_some() && !payload_alive {
        // No supervisor and no recorded group member remains: nothing can hold
        // this capacity. Settle to the leader's result when it was recorded.
        let observed_incarnation = job.incarnation.clone();
        let observed_payload = job.payload.clone();
        let mut settled = false;
        job = alter(store, &job.id, |j| {
            // `job` may be read before its crew parked: the supervisor parks
            // it as its payload ends, then exits. A parked record has no
            // payload to supervise, so it is not settled from that stale read.
            if terminal(&j.status)
                || j.status == "parked"
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
                || j.status == "parked"
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
/// Reconcile jobs whose supervisor is gone, so their outcome is recorded
/// without anyone asking for their status, and release finished jobs' capacity
/// a crash left reserved. Only the dispatcher runs this, and it records its
/// PID namespace as the one work must be recorded in.
pub(crate) fn reconcile_unsupervised(store: &Store) -> Result<Vec<Value>> {
    if let Some(namespace) = pid_namespace() {
        store.connection().execute(
            "INSERT INTO metadata(key, value) VALUES('pid_namespace', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [namespace],
        )?;
    }
    let mut statement = store.connection().prepare("SELECT record FROM jobs")?;
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
            // A live supervisor is checked cheaply and left to finish its job.
            let supervised = !terminal(&job.status)
                && job.supervisor.as_ref().map(same_process).transpose()? == Some(true);
            if supervised {
                return Ok(None);
            }
            // Settled records are final while their ledger remains elsewhere.
            if job.foreign.is_some() && ledger_elsewhere(&job.ledger) {
                return Ok(None);
            }
            // A ledger directory this user cannot have here means the record
            // came from another machine. Only the parent directory counts: a
            // missing file alone is local and SQLite recreates it, while this
            // user's own directory that cannot be opened right now (not
            // writable, say) is retried, not settled.
            if ledger_elsewhere(&job.ledger) {
                let cause = format!(
                    "left behind: its ledger {} is not on this machine",
                    job.ledger.display()
                );
                return Ok(Some(settle_foreign(store, &job.id, &cause)?));
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

/// Whether the directory holding a job's ledger cannot be this user's on this
/// machine: it is absent, this user cannot reach it (another user's home, as
/// on a machine where the ship moved to another account), or another user
/// owns it. This user's own ledger directory is one it created and owns
/// (`ledger_path`), so none of these holds for it.
fn ledger_elsewhere(path: &Path) -> bool {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => return false,
    };
    match fs::metadata(dir) {
        Ok(metadata) => metadata.uid() != unsafe { libc::geteuid() },
        Err(error) => {
            matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) || error.raw_os_error() == Some(libc::ENOTDIR)
        }
    }
}
/// Settle a record that belongs to another machine, once. A live record
/// becomes failed with `cause`, reported like any failure; a finished one
/// keeps its status. The mark stops later ticks from opening its ledger, and
/// one audit row records the settlement.
fn settle_foreign(store: &Store, id: &str, cause: &str) -> Result<Job> {
    let mut settled = false;
    let job = alter(store, id, |j| {
        if j.foreign.is_some() {
            return;
        }
        settled = true;
        j.foreign = Some(cause.into());
        if !terminal(&j.status) {
            j.status = "failed".into();
            j.reason = Some(cause.into());
            j.finished_ms = j.finished_ms.or(Some(Store::now_ms()));
        }
    })?;
    if settled {
        store.audit(
            "job.foreign",
            &json!({"job_id": job.id, "status": job.status, "ledger": job.ledger, "cause": cause}),
        )?;
    }
    Ok(job)
}
/// A ship that moved here from `from`: work it left unfinished there cannot
/// run here, so each unfinished record settles as failed, once.
pub(crate) fn settle_left_behind(store: &Store, from: &str) -> Result<()> {
    let ids: Vec<String> = store
        .connection()
        .prepare(&format!(
            "SELECT id FROM jobs WHERE json_extract(record,'$.status') NOT IN {TERMINAL_SQL}"
        ))?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in ids {
        settle_foreign(
            store,
            &id,
            &format!("left behind when the ship moved from {from}"),
        )?;
    }
    Ok(())
}

/// A job's current durable record, for crew reports.
pub(crate) fn record(store: &Store, id: &str) -> Result<Value> {
    Ok(public_job(&load(store, id)?))
}

/// A parked crew whose work moved to `resumed`: final for this record, with no
/// outcome of its own, since the resumed crew carries on.
pub(crate) fn finish_resumed(store: &Store, crew: &str, resumed: &str) -> Result<()> {
    alter(store, crew, |j| {
        if j.status == "parked" {
            j.status = "resumed".into();
            j.finished_ms = Some(Store::now_ms());
            j.reason = Some(format!("resumed as {resumed}"));
        }
    })?;
    Ok(())
}

/// A parked crew that could not resume: a failure, reported like any other.
pub(crate) fn fail_parked(store: &Store, crew: &str, reason: &str) -> Result<()> {
    alter(store, crew, |j| {
        if j.status == "parked" {
            j.status = "failed".into();
            j.finished_ms = Some(Store::now_ms());
            j.reason = Some(reason.to_owned());
        }
    })?;
    Ok(())
}

/// End a parked or held crew as cancelled, with `reason`. Nothing runs while
/// parked, so the crew ends here, and so does the work it waits on. The park
/// closes first, so the child's settlement does not resume the crew. Its first
/// mate gets the usual cancelled notice for each, unless `quiet`: the task's
/// owner closed the task, and already knows.
pub(crate) fn end_parked(store: &Store, id: &str, reason: &str, quiet: bool) -> Result<Value> {
    let parked = load(store, id)?;
    let child = crate::crew::cancel_park(store, parked.park.as_deref().unwrap_or_default())?;
    if quiet {
        // Marked before any status change, so no outcome is written for either.
        for job in std::iter::once(id).chain(child.as_deref()) {
            alter(store, job, |j| j.quiet = true)?;
        }
    }
    if matches!(parked.status.as_str(), "launching" | "running") {
        // Still ending its turn (the park grace): the park is closed, so the
        // payload's exit cannot park it again. It is cancelled as a running crew.
        if let Some(child) = child {
            cancel(store, &child)?;
        }
        return cancel(store, id);
    }
    let job = alter(store, id, |j| {
        if j.status == "parked" {
            j.status = "cancelled".into();
            j.finished_ms = Some(Store::now_ms());
            j.reason = Some(reason.to_owned());
        }
    })?;
    release(store, &job)?;
    if let Some(child) = child {
        cancel(store, &child)?;
    }
    Ok(public_job(&job))
}

/// Cancel a crew and what it waits on, for a launch that replaces it.
pub(crate) fn cancel_crew(store: &Store, id: &str) -> Result<()> {
    cancel(store, id)?;
    Ok(())
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
    if job.status == "parked" {
        return end_parked(store, id, "cancelled while parked", false);
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
            let escaped = escaped_descendants(&p, &job.escaped)?;
            if payload_alive(&p, &escaped)? {
                let group = group_alive(&p)?;
                let anchors = if !group {
                    vec![]
                } else if same_process(&p)? {
                    capture_anchors(&p, false)?.context(UNPROVEN_LEADER)?
                } else {
                    job.group_anchors.clone()
                };
                let _ = alter(store, id, |j| {
                    j.group_anchors = anchors.clone();
                    j.escaped = escaped.clone();
                })?;
                if group {
                    signal_group(&p, libc::SIGTERM, &anchors, false)?;
                }
                signal_escaped(&escaped, libc::SIGTERM)?;
                let until = Instant::now() + Duration::from_secs(2);
                while Instant::now() < until && payload_alive(&p, &escaped)? {
                    thread::sleep(Duration::from_millis(50));
                }
                if group_alive(&p)? {
                    signal_group(&p, libc::SIGKILL, &anchors, false)?;
                }
                signal_escaped(&escaped, libc::SIGKILL)?;
                // Give killed descendants time to leave the group.
                let until = Instant::now() + Duration::from_secs(2);
                while Instant::now() < until && payload_alive(&p, &escaped)? {
                    thread::sleep(Duration::from_millis(50));
                }
                ensure!(
                    !payload_alive(&p, &escaped)?,
                    "payload group or a descendant that left it remains; reservation preserved"
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
        let held = if job.foreign.is_some() && ledger_elsewhere(&job.ledger) {
            false
        } else {
            if !ledgers.contains_key(&job.ledger) {
                ledgers.insert(job.ledger.clone(), ledger(&job.ledger)?);
            }
            ledgers[&job.ledger].query_row(
                "SELECT EXISTS(SELECT 1 FROM reservations WHERE key=?1)",
                [reservation_key(store.home(), &job.id, &job.incarnation)],
                |r| r.get(0),
            )?
        };
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
    let escaped = job
        .as_ref()
        .map(|j| j.escaped.clone())
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| reservation.escaped.clone());
    ensure!(
        !escaped_alive(&escaped)?,
        "a descendant that left the payload group is still running; use job.cancel"
    );
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

/// Position of a job in `job.list`: non-terminal jobs first (rank 0), then
/// the rest (rank 1), newest first within each, id as the final tiebreak.
#[derive(Debug, PartialEq)]
struct ListKey {
    rank: i64,
    created_ms: i64,
    id: String,
}
impl ListKey {
    fn of(job: &Job) -> Self {
        Self {
            rank: i64::from(terminal(&job.status)),
            created_ms: job.created_ms,
            id: job.id.clone(),
        }
    }
    /// Before any job: the first page starts here.
    fn start() -> Self {
        Self {
            rank: -1,
            created_ms: i64::MAX,
            id: String::new(),
        }
    }
    fn encode(&self) -> String {
        format!("{}:{}:{}", self.rank, self.created_ms, self.id)
    }
    fn decode(raw: &str) -> Result<Self> {
        let mut parts = raw.splitn(3, ':');
        let (rank, created_ms, id) = (parts.next(), parts.next(), parts.next());
        let (Some(rank), Some(created_ms), Some(id)) = (rank, created_ms, id) else {
            bail!("cursor is not one returned by job.list");
        };
        Ok(Self {
            rank: rank
                .parse()
                .context("cursor is not one returned by job.list")?,
            created_ms: created_ms
                .parse()
                .context("cursor is not one returned by job.list")?,
            id: id.to_owned(),
        })
    }
}

/// `job.list`: a page of jobs in `ListKey` order. Returns `jobs`, `more`
/// (whether another page exists) and `next_cursor` (pass it back as `cursor`).
/// `after_id` (a job id) starts after that job's position instead. The order
/// is a keyset, so a page is not shifted by jobs that finished on an earlier
/// page, and live work always lands on the first page.
fn list(store: &Store, args: &Value) -> Result<Value> {
    let limit = args
        .get("limit")
        .map(|v| v.as_u64().context("limit must be a positive integer"))
        .transpose()?
        .unwrap_or(200)
        .clamp(1, 1000) as usize;
    let after = match (args.get("cursor"), args.get("after_id")) {
        (Some(cursor), _) => ListKey::decode(cursor.as_str().context("cursor must be a string")?)?,
        (None, Some(id)) => ListKey::of(&load(
            store,
            id.as_str().context("after_id must be a job id")?,
        )?),
        (None, None) => ListKey::start(),
    };
    // A stopped job's terminal status is what sorts it behind live work; the
    // SQL below must agree with `terminal` above.
    let terminal_sql = ["succeeded", "failed", "cancelled", "timed_out"]
        .map(|s| format!("'{s}'"))
        .join(",");
    let sql = format!(
        "WITH listed AS (
            SELECT record, id,
                   CASE WHEN json_extract(record,'$.status') IN ({terminal_sql}) THEN 1 ELSE 0 END AS rank,
                   json_extract(record,'$.created_ms') AS created
            FROM jobs)
         SELECT record FROM listed
         WHERE rank > ?1 OR (rank = ?1 AND (created < ?2 OR (created = ?2 AND id < ?3)))
         ORDER BY rank, created DESC, id DESC LIMIT ?4"
    );
    let mut stmt = store.connection().prepare(&sql)?;
    let rows = stmt.query_map(
        params![after.rank, after.created_ms, after.id, limit as i64 + 1],
        |r| r.get::<_, String>(0),
    )?;
    let mut page: Vec<Job> = rows
        .map(|r| Ok(serde_json::from_str(&r?)?))
        .collect::<Result<_>>()?;
    let more = page.len() > limit;
    page.truncate(limit);
    let next_cursor = page.last().map(|j| ListKey::of(j).encode());
    Ok(json!({
        "jobs": page.iter().map(public_job).collect::<Vec<_>>(),
        "more": more,
        "next_cursor": next_cursor,
    }))
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "job.start" => start(store, args, None, false),
        "job.status" => {
            let id = args
                .get("id")
                .and_then(Value::as_str)
                .context("id required")?;
            Ok(public_job(&load(store, id)?))
        }
        "job.list" => list(store, &args),
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
            // Retry decisions use the durable status. Reconciliation belongs
            // to the owning home's dispatcher, where process visibility is
            // known; a caller must not settle another namespace by reading.
            let old = load(store, old_id)?;
            ensure!(
                matches!(old.status.as_str(), "failed" | "cancelled" | "timed_out"),
                "retry requires a stopped unsuccessful job; no automatic retry of live/orphan work"
            );
            store.authorize(&old.task_id, "recovery")?;
            // A crew's replay keeps the route its launch was given. While that
            // route is backing off the replay would only fail again, so a fresh
            // crew, which takes the captain's first available route, is the way on.
            // The launch's own record names its route; argv is read only for
            // records written before that field existed.
            let route = old
                .model_route
                .clone()
                .or_else(|| crate::routes::route_of_argv(&json!(old.command)));
            if let (true, Some(ship), Some(route)) = (
                old.action == "spawn" && old.agent_actor.is_some(),
                old.agent_actor.as_deref(),
                route.as_deref(),
            ) {
                let refusal = crate::routes::refusal(store, route)?
                    .or(crate::crew_routes::retry_refusal(store, ship, route)?);
                if let Some(refusal) = refusal {
                    bail!(
                        "{refusal}; start a fresh crew with agent.start to take the first available route"
                    );
                }
            }
            let mut next = json!({"task_id": old.task_id, "command": old.command, "cwd": old.cwd, "heavy": old.heavy, "action": old.action, "timeout_secs": old.timeout_secs, "cleanup_descendants":old.cleanup_descendants, "agent_actor": old.agent_actor, "estimate": old.estimate, "wait_for_capacity": old.wait_for_capacity, "max_wait_secs": old.max_wait_secs, "codex_read_only_crew": old.codex_read_only_crew});
            if let Some(route) = &route {
                next["model_route"] = json!(route);
            }
            if let Some(name) = &old.route_name {
                next["route_name"] = json!(name);
            }
            if let Some(why) = &old.route_why {
                next["route_why"] = json!(why);
            }
            if old.hosted {
                next["hosted"] = json!(true);
            }
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
                let escaped_running = escaped_alive(&r.escaped)?;
                let mut value = serde_json::to_value(&r)?;
                value["supervisor_alive"] = json!(supervisor_alive);
                value["payload_group_alive"] = json!(payload_group_alive);
                value["escaped_alive"] = json!(escaped_running);
                value["orphaned"] = json!(
                    !supervisor_alive
                        && (payload_group_alive || Store::now_ms() - r.created_ms > START_GRACE_MS)
                );
                values.push(value);
            }
            let mut host = admission::measure()?;
            // Report the disk this home's work uses, as admission measures it.
            (host.disk_available_bytes, host.disk_total_bytes) = admission::disk(store.home())?;
            let mut status = admission::report(&settings, &host);
            status["ledger"] = json!(path);
            status["heavy_reserved"] = json!(values.iter().filter(|v| v["heavy"] == true).count());
            status["reservations"] = json!(values);
            status["queue"] = json!(live_queue(&conn)?);
            // What a crew of each bundled harness reserves here, and from where.
            let manifests = crate::bundled::manifests()
                .iter()
                .map(|(id, raw)| {
                    let manifest: Value = serde_json::from_str(raw)?;
                    Ok((*id, crate::plugins::manifest_memory(&manifest)?))
                })
                .collect::<Result<Vec<_>>>()?;
            status["crew_memory"] = admission::crew_memory_report(&settings, &manifests);
            status["disk_floor_met"] =
                json!(host.disk_available_bytes >= settings.min_free_disk_bytes);
            // How fast free space has been falling here, and when that rate
            // would reach the floor. Both are null until enough readings
            // exist to describe a rate.
            let trend = admission::disk_trend(&conn, &admission::disk_key(store.home())?)?;
            status["disk_trend"] = json!(trend);
            status["disk_forecast"] = json!(
                trend
                    .as_ref()
                    .and_then(|t| admission::disk_forecast(settings.min_free_disk_bytes, t))
            );
            status["model_routes"] = json!(crate::routes::list(&conn)?);
            let now = Store::now_ms();
            status["released_load"] = json!(released_load(&conn, now)?);
            status["recently_ended"] = json!(conn.query_row(
                "SELECT COUNT(*) FROM released WHERE released_ms >= ?1",
                [now - RELEASED_WINDOW_MS],
                |r| r.get::<_, i64>(0)
            )?);
            Ok(status)
        }
        _ => bail!("unknown jobs command {command}"),
    }
}

#[cfg(test)]
mod escape_tests {
    use super::independently_supervised;

    #[test]
    fn jobs_dispatchers_and_doors_are_never_another_jobs_descendants() {
        for args in [
            "/home/me/.cargo/bin/privateer --home /ships/apps/.privateer _job-supervise 1f2e",
            "/Users/me/.local/bin/privateer --home /ships/apps/.privateer serve",
            "/Users/me/.local/bin/privateer door run telegram",
        ] {
            assert!(independently_supervised(args), "{args}");
        }
        for args in [
            "/Applications/Xcode.app/Contents/Developer/usr/bin/xcodebuild -scheme App test",
            "/usr/bin/python3 -c print('serve the tests')",
            "/home/me/.cargo/bin/privateer --home /ships/apps/.privateer job.list --json {}",
        ] {
            assert!(!independently_supervised(args), "{args}");
        }
    }
}

#[cfg(test)]
mod stale_reconcile_tests {
    use super::*;

    /// A process that has run and been reaped: its identity names nothing.
    fn ended_process() -> Process {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let process = identity(child.id()).unwrap().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        process
    }

    /// The dispatcher reads every job, then checks each one's supervisor. A
    /// crew's supervisor can park it and exit in between: the read still says
    /// running, the supervisor is gone, and the payload has ended. That read
    /// must not settle the parked crew as failed, or its child's outcome finds
    /// no crew to resume (crew-resume-park-1ab330c6-...-1-1, 2026-10-19).
    #[test]
    fn a_crew_parked_after_the_dispatcher_read_it_stays_parked() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let supervisor = ended_process();
        let payload = ended_process();
        let running: Job = serde_json::from_value(json!({
            "id": "crew-resume-park-crew-1-1-1", "task_id": "fix", "incarnation": "one",
            "command": ["claude"], "cwd": temp.path(), "action": "spawn", "heavy": false,
            "timeout_secs": 60, "ledger": temp.path().join("ledger.sqlite"),
            "status": "running", "created_ms": Store::now_ms(), "finished_ms": null,
            "supervisor": supervisor, "payload": payload, "cancel_requested": false,
            "exit_code": null, "reason": null, "retry_of": null,
            "agent_actor": "engineering", "park": "park-crew-1-2",
        }))
        .unwrap();
        // What the supervisor wrote as the payload ended, after that read.
        let mut parked = running.clone();
        parked.status = "parked".into();
        parked.exit_code = Some(0);
        parked.reason = Some("crew parked: park-crew-1-2".into());
        store
            .connection()
            .execute(
                "INSERT INTO jobs(id, record) VALUES(?1, ?2)",
                params![parked.id, serde_json::to_string(&parked).unwrap()],
            )
            .unwrap();
        let job = reconcile(&store, running, &mut Reserved::default()).unwrap();
        assert_eq!(job.status, "parked", "{:?}", job.reason);
        let stored = load(&store, &job.id).unwrap();
        assert_eq!(stored.status, "parked", "{:?}", stored.reason);
        assert_eq!(stored.reason.as_deref(), Some("crew parked: park-crew-1-2"));
        assert_eq!(stored.finished_ms, None);
    }
}

#[cfg(test)]
mod park_end_signal_tests {
    use super::*;

    /// A harness ends its turn after `crew.park`, so its payload leader can
    /// exit just after the supervisor saw it alive at the end of the park's
    /// grace. The leader is then the supervisor's unreaped child: its group id
    /// is still reserved, and ending it is nothing left to signal. Refusing it
    /// as stale failed parked crews with a supervisor error 5 s after they
    /// parked (efe29e60, a7fa0853, 2026-10-19).
    #[test]
    fn a_payload_that_exits_as_its_park_is_noticed_is_not_a_stale_group() {
        // Started as the supervisor starts a payload: leader of its own group.
        let mut command = Command::new("sh");
        command.args(["-c", "read line"]).stdin(Stdio::piped());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let payload = identity(child.id()).unwrap().unwrap();
        // The supervisor's last snapshot, while the leader still ran.
        let anchors = capture_anchors(&payload, false).unwrap().unwrap();
        drop(child.stdin.take());
        let until = Instant::now() + Duration::from_secs(10);
        while exited_unreaped(payload.pid).unwrap().is_none() {
            assert!(Instant::now() < until, "payload did not exit");
            thread::sleep(Duration::from_millis(10));
        }
        // What the incidents hit: no recorded member is live any more.
        let refused = signal_group(&payload, libc::SIGTERM, &anchors, false).unwrap_err();
        assert!(
            format!("{refused:#}").contains("refusing stale process-group signal"),
            "{refused:#}"
        );

        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let running: Job = serde_json::from_value(json!({
            "id": "crew-1", "task_id": "fix", "incarnation": "one",
            "command": ["claude"], "cwd": temp.path(), "action": "spawn", "heavy": false,
            "timeout_secs": 60, "ledger": temp.path().join("ledger.sqlite"),
            "status": "running", "created_ms": Store::now_ms(), "finished_ms": null,
            "supervisor": identity(std::process::id()).unwrap(), "payload": payload,
            "group_anchors": anchors, "cancel_requested": false,
            "exit_code": null, "reason": null, "retry_of": null,
            "agent_actor": "engineering", "park": "park-crew-1-1",
        }))
        .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO jobs(id, record) VALUES(?1, ?2)",
                params![running.id, serde_json::to_string(&running).unwrap()],
            )
            .unwrap();
        // The supervisor's park stop, before it reaps the leader.
        let job = terminate_payload(&store, "crew-1", &payload, &[], true).unwrap();
        assert_eq!(job.status, "running");
        assert_eq!(job.park.as_deref(), Some("park-crew-1-1"));
        assert_eq!(job.group_anchors.first(), Some(&payload));
        assert!(!payload_alive(&payload, &job.escaped).unwrap());

        // Once reaped, its PID may be reused: a bare group id is never signalled.
        child.wait().unwrap();
        let refused = signal_group(
            &payload,
            libc::SIGTERM,
            std::slice::from_ref(&payload),
            false,
        );
        assert!(refused.is_err());
    }
}
