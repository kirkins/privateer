//! Host-wide admission for resource-intensive jobs.
//!
//! Decisions use live machine readings (load average and available memory)
//! plus the estimates of jobs admitted within a recent ramp window, whose
//! use is not yet visible in those readings. Settings live in the shared
//! resource ledger, so every home on the host applies the same policy, and
//! anyone, agents included, may tune them to the person's wishes: use the
//! whole machine, or leave room for their own work.
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

const GIB: u64 = 1024 * 1024 * 1024;

/// A request that does not fit right now. Unlike other errors it can wait in
/// the capacity queue and succeed later. `blocker` names the one resource the
/// refusal is about, when the refusal is one a queued request may pass.
#[derive(Debug)]
pub struct Refused {
    pub message: String,
    pub blocker: Option<Blocker>,
}
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Refused {}

/// The resource a refused request waits for. A request queued behind it may
/// pass only when its own demand does not use that resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Blocker {
    Disk,
    HeavySlot,
    Cpu,
}

impl Blocker {
    /// Whether a request with this demand may pass a head waiting on this
    /// resource. Its own admission still checks everything it declares.
    pub fn passable_by(self, demand: Option<Demand>, heavy: bool) -> bool {
        match self {
            Blocker::Disk => demand.is_none_or(|d| d.disk_bytes == 0),
            Blocker::HeavySlot => !heavy,
            Blocker::Cpu => demand.is_none_or(|d| d.cpus <= 0.0),
        }
    }

    pub fn resource(self) -> &'static str {
        match self {
            Blocker::Disk => "disk",
            Blocker::HeavySlot => "the heavy-job slot",
            Blocker::Cpu => "CPU",
        }
    }
}

macro_rules! refuse_unless {
    (on $blocker:expr, $condition:expr, $($message:tt)+) => {
        let fits: bool = $condition;
        if !fits {
            return Err(Refused {
                message: format!($($message)+),
                blocker: $blocker,
            }
            .into());
        }
    };
    ($condition:expr, $($message:tt)+) => {
        refuse_unless!(on None, $condition, $($message)+);
    };
}
pub(crate) use refuse_unless;

/// Resources a job is expected to use. Fields left out of a request fall back
/// to the heavy-job default for heavy jobs and to zero for others.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// Free space the job needs beyond the floor, while it runs. Builds write
    /// and keep what they write, so this is space that must stay available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
}

/// A resolved estimate recorded with an admitted reservation. `disk_bytes`
/// defaults to zero so reservations written before jobs declared disk demand
/// stay readable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Demand {
    pub cpus: f64,
    pub memory_bytes: u64,
    #[serde(default)]
    pub disk_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Settings {
    /// Optional cap on concurrent heavy jobs; none by default.
    pub max_heavy_jobs: Option<u64>,
    /// Admit only while load plus recently admitted CPUs stays below this
    /// many runnable tasks per CPU.
    pub max_load_per_cpu: f64,
    /// Keep at least this much memory available after admission.
    pub min_available_memory_bytes: u64,
    /// Keep at least this much space free on the filesystem a job or
    /// workspace uses.
    pub min_free_disk_bytes: u64,
    /// Default estimate for heavy jobs; `cpus` absent means every CPU.
    pub heavy_estimate: Estimate,
    /// How long an admitted job's estimate counts before its use is assumed
    /// to show in live readings (the load average trails by about a minute).
    pub ramp_secs: u64,
    /// How far ahead a falling free-space rate may reach the floor before the
    /// machine's owner is warned. 0 warns only once the floor is crossed.
    pub disk_warn_horizon_secs: u64,
    /// Build output (`target` directories) that released worktree slots of a
    /// pool may keep. Beyond it, the least recently released slots are
    /// trimmed. 0 keeps none.
    pub workspace_cache_budget_bytes: u64,
    /// Memory a crew launch reserves when neither its route, this host's
    /// setting for its harness, nor its harness manifest says how much it
    /// needs.
    pub crew_default_memory_bytes: u64,
    /// Per-harness crew memory, by harness id, overriding the harness
    /// manifest's figure on this host.
    pub crew_memory_bytes: BTreeMap<String, u64>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_heavy_jobs: None,
            max_load_per_cpu: 1.0,
            min_available_memory_bytes: GIB,
            min_free_disk_bytes: 2 * GIB,
            heavy_estimate: Estimate {
                cpus: None,
                memory_bytes: Some(2 * GIB),
                disk_bytes: Some(4 * GIB),
            },
            ramp_secs: 60,
            disk_warn_horizon_secs: 24 * 60 * 60,
            workspace_cache_budget_bytes: 8 * GIB,
            crew_default_memory_bytes: 2 * GIB,
            crew_memory_bytes: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Host {
    pub cpus: f64,
    pub load: f64,
    pub memory_available_bytes: u64,
    pub memory_total_bytes: u64,
    pub disk_available_bytes: u64,
    pub disk_total_bytes: u64,
}

/// A live reservation as admission sees it.
pub struct Held<'a> {
    pub heavy: bool,
    pub home: &'a std::path::Path,
    pub charter: Option<&'a str>,
    pub created_ms: i64,
    pub demand: Option<Demand>,
    /// The filesystem its declared space is on, as `disk_key`, and the free
    /// space measured when it was admitted.
    pub disk_key: Option<&'a str>,
    pub disk_free_at_admit: Option<u64>,
    /// Bytes trimmed from that filesystem since this job was admitted. A trim
    /// frees space this job did not write, so it is added back to the loss.
    pub disk_trimmed: u64,
    /// The job this reservation is for.
    pub job_id: &'a str,
}

/// What a new job asks for.
#[derive(Clone)]
pub struct Request<'a> {
    pub heavy: bool,
    pub home: &'a std::path::Path,
    pub charter: &'a str,
    pub charter_cap: Option<u64>,
    pub demand: Option<Demand>,
    pub charter_min_free_bytes: u64,
    /// The filesystem the job will use, as `disk_key`, with its free and
    /// total space as the caller measured them; the free reading is kept as a
    /// sample for the falling-space trend.
    pub disk_key: &'a str,
    pub disk_available_bytes: u64,
    pub disk_total_bytes: u64,
    /// Load the one-minute average still carries from jobs that just ended,
    /// which no longer run (see `released_load`).
    pub released_load: f64,
    /// The supervised job whose agent started this one, when an agent did.
    /// That job's reservation is not another job competing for memory.
    pub started_by: Option<&'a str>,
}

/// What the one-minute load average still holds of a job that ended
/// `ago_ms` ago: the average forgets with a one-minute time constant. The
/// job's share is its estimated CPUs, but never more than the whole load
/// when it ended.
pub fn released_load(cpus: f64, load_at_release: f64, ago_ms: i64) -> f64 {
    let share = cpus.min(load_at_release).max(0.0);
    share * (-(ago_ms.max(0) as f64) / 60_000.0).exp()
}

pub fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS disk_samples (path TEXT NOT NULL, sampled_ms INTEGER NOT NULL,
           free_bytes INTEGER NOT NULL, trimmed_bytes INTEGER NOT NULL DEFAULT 0,
           PRIMARY KEY (path, sampled_ms));
         CREATE TABLE IF NOT EXISTS disk_trims (disk_key TEXT PRIMARY KEY,
           total_bytes INTEGER NOT NULL, last_ms INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS load_samples (sampled_ms INTEGER PRIMARY KEY,
           load REAL NOT NULL, cpus REAL NOT NULL);",
    )?;
    // A ledger made before trims were recorded has disk_samples without the
    // column; CREATE TABLE IF NOT EXISTS leaves it as it was.
    let has_trimmed = conn
        .prepare("SELECT 1 FROM pragma_table_info('disk_samples') WHERE name='trimmed_bytes'")?
        .exists([])?;
    if !has_trimmed {
        // Another process opening the ledger at the same moment may add it
        // first; its duplicate is the same outcome.
        if let Err(error) = conn.execute_batch(
            "ALTER TABLE disk_samples ADD COLUMN trimmed_bytes INTEGER NOT NULL DEFAULT 0",
        ) && !error.to_string().contains("duplicate column name")
        {
            return Err(error.into());
        }
    }
    Ok(())
}

fn stored(conn: &Connection) -> Result<Map<String, Value>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key='admission'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    match raw {
        Some(raw) => serde_json::from_str(&raw).context("corrupt admission settings"),
        None => Ok(Map::new()),
    }
}

fn positive(value: &Value, key: &str, max: f64) -> Result<f64> {
    value
        .as_f64()
        .filter(|v| v.is_finite() && *v > 0.0 && *v <= max)
        .with_context(|| format!("{key} must be a number above 0 and at most {max}"))
}

fn bytes(value: &Value, key: &str) -> Result<u64> {
    value
        .as_u64()
        .with_context(|| format!("{key} must be a whole number of bytes"))
}

/// Validate a requested estimate.
pub fn estimate(value: &Value) -> Result<Estimate> {
    let object = value
        .as_object()
        .context("estimate must be an object with cpus, memory_bytes and/or disk_bytes")?;
    let mut estimate = Estimate::default();
    for (key, value) in object {
        match key.as_str() {
            "cpus" if value.is_null() => {}
            "cpus" => estimate.cpus = Some(positive(value, "estimate.cpus", 1024.0)?),
            "memory_bytes" if value.is_null() => {}
            "memory_bytes" => estimate.memory_bytes = Some(bytes(value, "estimate.memory_bytes")?),
            "disk_bytes" if value.is_null() => {}
            "disk_bytes" => estimate.disk_bytes = Some(bytes(value, "estimate.disk_bytes")?),
            _ => bail!("unknown estimate field {key}"),
        }
    }
    Ok(estimate)
}

fn effective(raw: &Map<String, Value>) -> Result<Settings> {
    let mut settings = Settings::default();
    for (key, value) in raw {
        match key.as_str() {
            "max_heavy_jobs" => {
                let cap = value
                    .as_u64()
                    .filter(|v| (1..=1024).contains(v))
                    .context("max_heavy_jobs must be 1..1024, or null for no cap")?;
                settings.max_heavy_jobs = Some(cap);
            }
            "max_load_per_cpu" => {
                settings.max_load_per_cpu = positive(value, key, 1024.0)?;
            }
            "min_available_memory_bytes" => {
                settings.min_available_memory_bytes = bytes(value, key)?;
            }
            "min_free_disk_bytes" => settings.min_free_disk_bytes = bytes(value, key)?,
            "heavy_estimate" => {
                let requested = estimate(value)?;
                settings.heavy_estimate = Estimate {
                    cpus: requested.cpus,
                    memory_bytes: requested
                        .memory_bytes
                        .or(Settings::default().heavy_estimate.memory_bytes),
                    disk_bytes: requested
                        .disk_bytes
                        .or(Settings::default().heavy_estimate.disk_bytes),
                };
            }
            "ramp_secs" => {
                settings.ramp_secs = value
                    .as_u64()
                    .filter(|v| *v <= 3600)
                    .context("ramp_secs must be 0..3600")?;
            }
            "disk_warn_horizon_secs" => {
                settings.disk_warn_horizon_secs =
                    value.as_u64().filter(|v| *v <= 30 * 86_400).context(
                        "disk_warn_horizon_secs must be 0..2592000 (0 warns only below the floor)",
                    )?;
            }
            "workspace_cache_budget_bytes" => {
                settings.workspace_cache_budget_bytes = bytes(value, key)?;
            }
            "crew_default_memory_bytes" => {
                settings.crew_default_memory_bytes = bytes(value, key)?;
            }
            "crew_memory_bytes" => {
                let by_harness = value
                    .as_object()
                    .context("crew_memory_bytes must map harness ids to bytes")?;
                settings.crew_memory_bytes = by_harness
                    .iter()
                    .map(|(harness, bytes_value)| {
                        ensure!(!harness.is_empty(), "crew_memory_bytes needs harness ids");
                        Ok((
                            harness.clone(),
                            bytes_value.as_u64().filter(|b| *b > 0).with_context(|| {
                                format!(
                                    "crew_memory_bytes.{harness} must be a positive number of bytes"
                                )
                            })?,
                        ))
                    })
                    .collect::<Result<_>>()?;
            }
            _ => bail!("unknown admission setting {key}"),
        }
    }
    Ok(settings)
}

pub fn settings(conn: &Connection) -> Result<Settings> {
    effective(&stored(conn)?)
}

/// Merge changes into the stored settings; a null value restores a default.
pub fn configure(conn: &Connection, changes: &Value) -> Result<Settings> {
    let changes = changes.as_object().context("settings must be an object")?;
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        let mut raw = stored(conn)?;
        for (key, value) in changes {
            if value.is_null() {
                raw.remove(key);
            } else {
                raw.insert(key.clone(), value.clone());
            }
        }
        let settings = effective(&raw)?;
        conn.execute(
            "INSERT INTO settings(key,value) VALUES('admission',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [Value::Object(raw).to_string()],
        )?;
        Ok(settings)
    })();
    if result.is_ok() {
        conn.execute_batch("COMMIT")?;
    } else {
        let _ = conn.execute_batch("ROLLBACK");
    }
    result
}

/// The memory a crew launch reserves: its route's own figure, else this
/// host's setting for its harness, else the harness manifest's, else the
/// default for a manifest that declares none.
pub fn crew_memory(
    settings: &Settings,
    harness: &str,
    manifest: Option<u64>,
    route: Option<u64>,
) -> u64 {
    crew_memory_source(settings, harness, manifest, route).0
}

/// The same figure as `crew_memory`, with the layer it resolved from:
/// "route", "host", "manifest" or "default".
pub fn crew_memory_source(
    settings: &Settings,
    harness: &str,
    manifest: Option<u64>,
    route: Option<u64>,
) -> (u64, &'static str) {
    if let Some(bytes) = route {
        return (bytes, "route");
    }
    if let Some(bytes) = settings.crew_memory_bytes.get(harness).copied() {
        return (bytes, "host");
    }
    match manifest {
        Some(bytes) => (bytes, "manifest"),
        None => (settings.crew_default_memory_bytes, "default"),
    }
}

/// Each harness's crew memory on this host, as a launch with no route would
/// reserve it. `manifests` pairs a harness id with the figure its manifest
/// declares, if any. A route's own figure is set per launch, so it is not here.
pub fn crew_memory_report(settings: &Settings, manifests: &[(&str, Option<u64>)]) -> Value {
    Value::Array(
        manifests
            .iter()
            .map(|(harness, manifest)| {
                let (bytes, source) = crew_memory_source(settings, harness, *manifest, None);
                json!({"harness": harness, "memory_bytes": bytes, "source": source})
            })
            .collect(),
    )
}

/// The demand to record for a job, or None when it is neither heavy nor
/// estimated (such jobs are not subject to admission). The heavy default for
/// disk applies only when the caller asked for heavy work by name
/// (`heavy_declared`): an unlabelled job defaults to heavy for CPU and memory
/// but must not hold gigabytes of disk it never writes.
pub fn demand(
    settings: &Settings,
    host: &Host,
    heavy: bool,
    heavy_declared: bool,
    requested: Option<&Estimate>,
) -> Option<Demand> {
    if !heavy && requested.is_none() {
        return None;
    }
    let requested = requested.cloned().unwrap_or_default();
    let fallback = if heavy {
        Demand {
            cpus: settings.heavy_estimate.cpus.unwrap_or(host.cpus),
            memory_bytes: settings.heavy_estimate.memory_bytes.unwrap_or(0),
            disk_bytes: if heavy_declared {
                settings.heavy_estimate.disk_bytes.unwrap_or(0)
            } else {
                0
            },
        }
    } else {
        Demand::default()
    };
    Some(Demand {
        cpus: requested.cpus.unwrap_or(fallback.cpus),
        memory_bytes: requested.memory_bytes.unwrap_or(fallback.memory_bytes),
        disk_bytes: requested.disk_bytes.unwrap_or(fallback.disk_bytes),
    })
}

/// Disk the held reservations on one filesystem still have to write. Each
/// job's declared space less what the filesystem has lost since that job was
/// admitted, since a reading already shows what was written. The credit is
/// capped at the loss since the oldest of them was admitted, so concurrent
/// jobs cannot all claim the same loss.
pub fn committed_disk(held: &[Held], disk_key: &str, free_now: u64) -> u64 {
    let on_disk: Vec<&Held> = held
        .iter()
        .filter(|h| h.disk_key == Some(disk_key))
        .collect();
    let declared = on_disk
        .iter()
        .filter_map(|h| h.demand)
        .fold(0u64, |sum, d| sum.saturating_add(d.disk_bytes));
    let lost = |h: &Held| {
        h.disk_free_at_admit.map_or(0, |free| {
            free.saturating_add(h.disk_trimmed).saturating_sub(free_now)
        })
    };
    let credit = on_disk
        .iter()
        .filter_map(|h| h.demand.map(|d| d.disk_bytes.min(lost(h))))
        .fold(0u64, |sum, c| sum.saturating_add(c));
    let oldest_loss = on_disk
        .iter()
        .filter(|h| h.disk_free_at_admit.is_some())
        .min_by_key(|h| h.created_ms)
        .map_or(0, |h| lost(h));
    declared.saturating_sub(credit.min(oldest_loss))
}

fn mib(bytes: impl Into<i128>) -> i128 {
    bytes.into() / (1024 * 1024)
}

/// Decide whether a request fits. Called under the ledger's write lock.
pub fn admit(
    settings: &Settings,
    host: &Host,
    now_ms: i64,
    held: &[Held],
    request: &Request,
) -> Result<()> {
    // The caller measured the filesystem the job will use, so a full root
    // does not block work on a separate volume.
    let available = request.disk_available_bytes;
    let required_disk = settings
        .min_free_disk_bytes
        .max(request.charter_min_free_bytes);
    // A job whose declared space cannot fit even on an empty filesystem would
    // wait at the head of the host-wide queue for its whole wait limit, so it
    // is refused now. A floor set above the disk waits as it always has: the
    // operator may lower it.
    let own_disk = request.demand.map_or(0, |d| d.disk_bytes);
    ensure!(
        own_disk == 0 || required_disk.saturating_add(own_disk) <= request.disk_total_bytes,
        "job never fits: {} MiB must stay free and this job declares {} MiB, but the filesystem holds {} MiB in all; lower estimate.disk_bytes or tune resource.configure (min_free_disk_bytes, heavy_estimate.disk_bytes)",
        mib(required_disk),
        mib(own_disk),
        mib(request.disk_total_bytes)
    );
    refuse_unless!(
        on Some(Blocker::Disk),
        available >= required_disk,
        "disk admission refused: {} MiB disk available, {} MiB must stay free (host floor {} MiB, charter floor {} MiB); retry later or tune resource.configure",
        mib(available),
        mib(required_disk),
        mib(settings.min_free_disk_bytes),
        mib(request.charter_min_free_bytes)
    );
    let Some(demand) = request.demand else {
        return Ok(());
    };
    // Disk a job declares is space it will use and keep, unlike load and
    // memory, whose use shows up in live readings. Every reservation still
    // held on this filesystem counts, not just recent ones: a running build's
    // bytes stay on the disk. A job's own demand always counts, since starting
    // it is what fills the disk.
    let committed = committed_disk(held, request.disk_key, available);
    let needed = (required_disk as i128)
        .saturating_add(committed as i128)
        .saturating_add(demand.disk_bytes as i128);
    refuse_unless!(
        on Some(Blocker::Disk),
        available as i128 >= needed,
        "disk admission refused: {} MiB disk available, {} MiB must stay free (host floor {} MiB, charter floor {} MiB), and {} MiB is declared by running jobs while this one declares {} MiB, so {} MiB are needed; retry later, declare less with estimate.disk_bytes, or tune resource.configure (heavy_estimate.disk_bytes, min_free_disk_bytes)",
        mib(available),
        mib(required_disk),
        mib(settings.min_free_disk_bytes),
        mib(request.charter_min_free_bytes),
        mib(committed),
        mib(demand.disk_bytes),
        mib(needed)
    );
    if request.heavy {
        let heavy = held.iter().filter(|h| h.heavy).count() as u64;
        if let Some(cap) = settings.max_heavy_jobs {
            refuse_unless!(on Some(Blocker::HeavySlot), heavy < cap, "heavy-job capacity exhausted ({heavy}/{cap})");
        }
        if let Some(cap) = request.charter_cap {
            let share = held
                .iter()
                .filter(|h| h.heavy && h.home == request.home && h.charter == Some(request.charter))
                .count() as u64;
            refuse_unless!(
                on Some(Blocker::HeavySlot),
                share < cap,
                "charter heavy-job share exhausted ({share}/{cap})"
            );
        }
    }
    let ramp_start = now_ms.saturating_sub(settings.ramp_secs.saturating_mul(1000) as i64);
    let recent: Vec<Demand> = held
        .iter()
        .filter(|h| h.created_ms >= ramp_start)
        .filter_map(|h| h.demand)
        .collect();
    // + 0.0 turns the empty sum's -0.0 into 0.0 for messages.
    let recent_cpus: f64 = recent.iter().map(|d| d.cpus).sum::<f64>() + 0.0;
    let recent_memory: u64 = recent.iter().map(|d| d.memory_bytes).sum();
    // CPU: there must be headroom now, counting jobs that just started. The
    // job's own estimate then counts against the next request during its
    // ramp, so one job is never refused for its own size alone.
    // A job that just ended still shows in the load average for minutes;
    // it no longer runs, so it does not hold the next one back.
    // Only a job that declares CPUs is judged by them. A crew reserves memory
    // alone: refusing it on a busy machine would stop first mates from
    // starting crews whenever builds are running.
    if demand.cpus > 0.0 {
        let load = (host.load - request.released_load).max(0.0);
        let per_cpu = (load + recent_cpus) / host.cpus;
        refuse_unless!(
            on Some(Blocker::Cpu),
            per_cpu < settings.max_load_per_cpu,
            "admission refused: CPU demand {per_cpu:.2} per CPU is at the limit {} (load {:.2}, {:.1} of it from jobs that just ended, {recent_cpus:.1} CPUs recently admitted, {} CPUs); retry later or tune resource.configure",
            settings.max_load_per_cpu,
            host.load,
            request.released_load.min(host.load),
            host.cpus
        );
    }
    // Memory: running out is worse than waiting, so the job's own estimate
    // must fit above the floor too, while other estimated jobs run. The
    // estimate keeps big jobs from starting together; it must not keep the
    // only one from starting on a small machine whose person is using it, so
    // a job running alone needs only the floor.
    // The crew that started this job holds its own reservation, which is not
    // another estimated job competing for memory.
    let alone = held
        .iter()
        .filter(|h| Some(h.job_id) != request.started_by)
        .all(|h| h.demand.is_none());
    let own = if alone { 0 } else { demand.memory_bytes };
    let left = host.memory_available_bytes as i128 - recent_memory as i128 - own as i128;
    refuse_unless!(
        left >= settings.min_available_memory_bytes as i128,
        "admission refused: {} MiB available, {} MiB recently admitted, {} MiB estimated for this job{}, {} MiB must stay free; retry later or tune resource.configure",
        mib(host.memory_available_bytes),
        mib(recent_memory),
        mib(demand.memory_bytes),
        if alone {
            " (not counted: no other estimated job runs)"
        } else {
            ""
        },
        mib(settings.min_available_memory_bytes)
    );
    Ok(())
}

fn cpus() -> f64 {
    std::thread::available_parallelism()
        .map(|n| n.get() as f64)
        .unwrap_or(1.0)
}

/// The one-minute load per CPU that admission gates on, without reading
/// memory or disk; `None` if the load average cannot be read.
pub fn load_per_cpu() -> Option<f64> {
    let mut load = 0f64;
    (unsafe { libc::getloadavg(&mut load, 1) } >= 1).then(|| load / cpus())
}

pub fn measure() -> Result<Host> {
    let cpus = cpus();
    let mut loads = [0f64; 3];
    ensure!(
        unsafe { libc::getloadavg(loads.as_mut_ptr(), 3) } >= 1,
        "cannot read load average"
    );
    let (memory_available_bytes, memory_total_bytes) = memory()?;
    let (disk_available_bytes, disk_total_bytes) = disk(std::path::Path::new("/"))?;
    Ok(Host {
        cpus,
        load: loads[0],
        memory_available_bytes,
        memory_total_bytes,
        disk_available_bytes,
        disk_total_bytes,
    })
}

#[cfg(target_os = "linux")]
fn memory() -> Result<(u64, u64)> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").context("read /proc/meminfo")?;
    let field = |name: &str| -> Result<u64> {
        let line = meminfo
            .lines()
            .find(|l| l.starts_with(name))
            .with_context(|| format!("{name} missing from /proc/meminfo"))?;
        let kib: u64 = line
            .split_whitespace()
            .nth(1)
            .and_then(|v| v.parse().ok())
            .with_context(|| format!("invalid {name} in /proc/meminfo"))?;
        Ok(kib * 1024)
    };
    Ok((field("MemAvailable:")?, field("MemTotal:")?))
}

#[cfg(unix)]
// statvfs field widths differ across Unix platforms, so the casts are needed
// on those where the fields are narrower than u64.
#[allow(clippy::unnecessary_cast)]
pub fn disk(path: &std::path::Path) -> Result<(u64, u64)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes()).context("disk path contains NUL")?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    ensure!(
        unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } == 0,
        "cannot read free disk space for {}: {}",
        path.to_string_lossy(),
        std::io::Error::last_os_error()
    );
    let stats = unsafe { stats.assume_init() };
    let unit = stats.f_frsize as u64;
    Ok((
        (stats.f_bavail as u64).saturating_mul(unit),
        (stats.f_blocks as u64).saturating_mul(unit),
    ))
}

#[cfg(not(unix))]
pub fn disk(_path: &std::path::Path) -> Result<(u64, u64)> {
    bail!("disk admission is supported on Unix")
}

#[cfg(target_os = "macos")]
fn memory() -> Result<(u64, u64)> {
    use std::process::Command;
    // Absolute paths: services and tests may run with a minimal PATH, and
    // sysctl lives in /usr/sbin.
    let total = Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .context("read hw.memsize")?;
    let total: u64 = String::from_utf8(total.stdout)?
        .trim()
        .parse()
        .context("invalid hw.memsize")?;
    let output = Command::new("/usr/bin/vm_stat")
        .env("LC_ALL", "C")
        .output()
        .context("run vm_stat")?;
    let text = String::from_utf8(output.stdout)?;
    let page: u64 = text
        .split("page size of ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .context("vm_stat page size missing")?;
    // Free, inactive and speculative pages can be handed to new work.
    let mut pages = 0;
    for name in ["Pages free:", "Pages inactive:", "Pages speculative:"] {
        let count: u64 = text
            .lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l[name.len()..].trim().trim_end_matches('.').parse().ok())
            .with_context(|| format!("vm_stat {name} missing"))?;
        pages += count;
    }
    Ok((pages * page, total))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn memory() -> Result<(u64, u64)> {
    bail!("memory admission is supported on Linux and macOS")
}

/// Free space is sampled at most this often for one filesystem: often enough
/// to see a day of falling space, rarely enough that the table stays small.
const DISK_SAMPLE_EVERY_MS: i64 = 5 * 60_000;
/// Samples older than this are dropped, bounding the table however long the
/// ledger lives.
const DISK_SAMPLE_KEEP_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// A rate is read from the readings of this recent window only, so an old
/// fall or rise cannot stand for what the disk is doing now.
const DISK_TREND_WINDOW_MS: i64 = 12 * 60 * 60 * 1000;
/// A falling rate is believed only once the readings span this long, so one
/// build that wrote a gigabyte cannot read as a machine filling up.
const DISK_TREND_MIN_SPAN_MS: i64 = 60 * 60_000;
/// A rise this large between two readings, beyond what trims explain, is a
/// cleanup: the rate is read from the readings after it, so the space freed
/// does not hide a fall that follows.
const DISK_CLEANUP_BYTES: u64 = GIB;

/// How free space on one filesystem has moved over the recent readings.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DiskTrend {
    /// The filesystem the samples describe, as `disk_key`.
    pub path: String,
    /// The first and last reading the rate is read from.
    pub from_ms: i64,
    pub from_free_bytes: u64,
    pub to_ms: i64,
    pub to_free_bytes: u64,
    /// Least-squares slope of free space over those readings, in bytes per
    /// hour; negative while space falls.
    pub bytes_per_hour: f64,
}

/// The key one filesystem's samples are kept under: the device it lives on,
/// so every path on that filesystem feeds the same series and a home's owner
/// sees what work anywhere else on that disk has written.
#[cfg(unix)]
pub fn disk_key(path: &std::path::Path) -> Result<String> {
    use std::os::unix::fs::MetadataExt;
    Ok(format!(
        "dev:{}",
        std::fs::metadata(path)
            .with_context(|| format!("cannot read disk of {}", path.to_string_lossy()))?
            .dev()
    ))
}

#[cfg(not(unix))]
pub fn disk_key(_path: &std::path::Path) -> Result<String> {
    bail!("disk admission is supported on Unix")
}

/// When a falling rate would take free space down to the floor.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct DiskForecast {
    /// Free space lost per hour at the observed rate.
    pub gib_per_hour: f64,
    /// Hours until the floor is reached at that rate.
    pub hours_to_floor: f64,
    /// How long the samples behind the rate span.
    pub span_ms: i64,
}

/// Keep one reading of free space, at most one every `DISK_SAMPLE_EVERY_MS`,
/// so a later reading can say how fast space is falling. Each reading keeps
/// the trim total at the time, so a trim's space can be taken back out of the
/// readings after it. Callers pass the reading they just decided against, so
/// the trend and the decision agree. Readings past the retention window are
/// dropped for every filesystem.
pub fn note_disk(conn: &Connection, path: &str, free: u64, now_ms: i64) -> Result<()> {
    let last: Option<i64> = conn
        .query_row(
            "SELECT sampled_ms FROM disk_samples WHERE path=?1 ORDER BY sampled_ms DESC LIMIT 1",
            [path],
            |r| r.get(0),
        )
        .optional()?;
    if last.is_some_and(|at| now_ms.saturating_sub(at) < DISK_SAMPLE_EVERY_MS) {
        return Ok(());
    }
    let trimmed = trim_total(conn, path)?;
    conn.execute(
        "INSERT INTO disk_samples(path,sampled_ms,free_bytes,trimmed_bytes) VALUES(?1,?2,?3,?4)
         ON CONFLICT(path,sampled_ms) DO UPDATE SET free_bytes=excluded.free_bytes,
           trimmed_bytes=excluded.trimmed_bytes",
        params![
            path,
            now_ms,
            free.min(i64::MAX as u64) as i64,
            trimmed.min(i64::MAX as u64) as i64
        ],
    )?;
    conn.execute(
        "DELETE FROM disk_samples WHERE sampled_ms < ?1",
        [now_ms - DISK_SAMPLE_KEEP_MS],
    )?;
    Ok(())
}

/// Bytes trimmed from one filesystem over its whole life. A reservation keeps
/// the total it saw at admission, so the difference is what was trimmed while
/// it ran. It only grows, so no retention window can lose a trim.
pub fn trim_total(conn: &Connection, disk_key: &str) -> Result<u64> {
    let total: Option<i64> = conn
        .query_row(
            "SELECT total_bytes FROM disk_trims WHERE disk_key=?1",
            [disk_key],
            |r| r.get(0),
        )
        .optional()?;
    Ok(total.unwrap_or(0).max(0) as u64)
}

/// Record that `bytes` of build output were trimmed from one filesystem and
/// read `free` bytes free afterwards. The reading carries the new trim total.
/// It is written even when a sample was taken moments ago, so the trend's
/// readings after the trim are not missing it. A trim is not a reset: the
/// trend keeps its readings and takes the trimmed bytes back out of them.
pub fn record_trim(
    conn: &Connection,
    disk_key: &str,
    bytes: u64,
    free: u64,
    now_ms: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO disk_trims(disk_key,total_bytes,last_ms) VALUES(?1,?2,?3)
         ON CONFLICT(disk_key) DO UPDATE SET total_bytes=total_bytes+excluded.total_bytes,
           last_ms=excluded.last_ms",
        params![disk_key, bytes.min(i64::MAX as u64) as i64, now_ms],
    )?;
    let trimmed = trim_total(conn, disk_key)?;
    conn.execute(
        "INSERT INTO disk_samples(path,sampled_ms,free_bytes,trimmed_bytes) VALUES(?1,?2,?3,?4)
         ON CONFLICT(path,sampled_ms) DO UPDATE SET free_bytes=excluded.free_bytes,
           trimmed_bytes=excluded.trimmed_bytes",
        params![
            disk_key,
            now_ms,
            free.min(i64::MAX as u64) as i64,
            trimmed.min(i64::MAX as u64) as i64
        ],
    )?;
    Ok(())
}

/// The rate at which free space has been moving over the recent readings of
/// one filesystem. The window ends at the newest reading and starts at the
/// last cleanup in it. Trimmed bytes are taken back out of each reading, so a
/// trim is neither a rise nor a reset: builds still falling under regular
/// trims keep their rate. The slope is a least-squares fit, and the rate is
/// None until those readings span `DISK_TREND_MIN_SPAN_MS`, so a caller cannot
/// read a rate out of one build's writing.
pub fn disk_trend(conn: &Connection, path: &str) -> Result<Option<DiskTrend>> {
    let newest: Option<i64> = conn.query_row(
        "SELECT MAX(sampled_ms) FROM disk_samples WHERE path=?1",
        [path],
        |r| r.get(0),
    )?;
    let Some(newest) = newest else {
        return Ok(None);
    };
    let mut statement = conn.prepare(
        "SELECT sampled_ms,free_bytes,trimmed_bytes FROM disk_samples
         WHERE path=?1 AND sampled_ms>=?2 ORDER BY sampled_ms",
    )?;
    let samples = statement
        .query_map(params![path, newest - DISK_TREND_WINDOW_MS], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // (time, free as read, free without the trims behind the reading).
    let readings: Vec<(i64, i64, i64)> = samples
        .iter()
        .map(|&(ms, free, trimmed)| (ms, free, free.saturating_sub(trimmed)))
        .collect();
    let after_cleanup = readings
        .windows(2)
        .rposition(|w| w[1].2.saturating_sub(w[0].2) >= DISK_CLEANUP_BYTES as i64)
        .map_or(0, |i| i + 1);
    let readings = &readings[after_cleanup..];
    let (Some(&(from_ms, from_free, _)), Some(&(to_ms, to_free, _))) =
        (readings.first(), readings.last())
    else {
        return Ok(None);
    };
    if to_ms - from_ms < DISK_TREND_MIN_SPAN_MS {
        return Ok(None);
    }
    let hours = |ms: i64| (ms - from_ms) as f64 / 3_600_000.0;
    let count = readings.len() as f64;
    let mean_hours = readings.iter().map(|&(ms, _, _)| hours(ms)).sum::<f64>() / count;
    let mean_free = readings
        .iter()
        .map(|&(_, _, kept)| kept as f64)
        .sum::<f64>()
        / count;
    let (covariance, variance) = readings.iter().fold((0.0, 0.0), |(c, v), &(ms, _, kept)| {
        let x = hours(ms) - mean_hours;
        (c + x * (kept as f64 - mean_free), v + x * x)
    });
    Ok(Some(DiskTrend {
        path: path.to_owned(),
        from_ms,
        from_free_bytes: from_free as u64,
        to_ms,
        to_free_bytes: to_free as u64,
        bytes_per_hour: covariance / variance,
    }))
}

/// Project the observed rate onto the floor. None when space is not falling,
/// when the floor is already crossed (the floor's own alarm covers that), or
/// when the readings are too close together to describe a rate.
pub fn disk_forecast(floor: u64, trend: &DiskTrend) -> Option<DiskForecast> {
    let span_ms = trend.to_ms.saturating_sub(trend.from_ms);
    if span_ms < DISK_TREND_MIN_SPAN_MS || trend.to_free_bytes <= floor {
        return None;
    }
    let per_hour = -trend.bytes_per_hour;
    if per_hour.is_nan() || per_hour <= 0.0 {
        return None;
    }
    Some(DiskForecast {
        gib_per_hour: per_hour / GIB as f64,
        hours_to_floor: (trend.to_free_bytes - floor) as f64 / per_hour,
        span_ms,
    })
}

/// The forecast worth warning about: the one where the observed rate would
/// reach the floor within `horizon_secs`. A warning already `raised` clears
/// only once the floor is more than twice that horizon away, so a forecast
/// that hovers at the horizon does not flap. A horizon of 0 warns only once
/// the floor is crossed, which `disk_forecast` already excludes.
pub fn disk_warning(
    floor: u64,
    horizon_secs: u64,
    raised: bool,
    trend: &DiskTrend,
) -> Option<DiskForecast> {
    let forecast = disk_forecast(floor, trend)?;
    let reach = horizon_secs as f64 * if raised { 2.0 } else { 1.0 };
    (forecast.hours_to_floor * 3_600.0 <= reach).then_some(forecast)
}

/// Load is sampled at most this often, by the machine owner's alert pass.
const LOAD_SAMPLE_EVERY_MS: i64 = 60_000;
/// Readings further apart than this do not make one stretch: nothing was
/// watching the machine in between, so nothing says the load held.
const LOAD_SAMPLE_GAP_MS: i64 = 3 * 60_000;
/// Readings older than this are dropped: an hour of one-minute readings at
/// most, however long the ledger lives.
const LOAD_SAMPLE_KEEP_MS: i64 = 60 * 60_000;
/// Load per CPU must stay at or above the admission gate across readings
/// spanning this long before the machine's owner hears of it, so a build's
/// burst of a few minutes is not an alarm.
pub const LOAD_SUSTAINED_MS: i64 = 10 * 60_000;
/// A raised load alarm clears once load per CPU has stayed below the gate
/// across readings spanning this long, so one quiet minute between jobs does
/// not clear it and raise it again.
pub const LOAD_SETTLED_MS: i64 = 5 * 60_000;

/// A stretch of high load: the newest reading, and how long the readings
/// before it have stayed at or above the gate.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct LoadSpell {
    pub load: f64,
    pub cpus: f64,
    pub per_cpu: f64,
    pub span_ms: i64,
}

/// Keep one reading of the load average, at most one every
/// `LOAD_SAMPLE_EVERY_MS`, so a later pass can tell sustained load from a
/// burst. Readings past the retention window are dropped.
pub fn note_load(conn: &Connection, host: &Host, now_ms: i64) -> Result<()> {
    let last: Option<i64> =
        conn.query_row("SELECT MAX(sampled_ms) FROM load_samples", [], |r| r.get(0))?;
    if last.is_some_and(|at| now_ms.saturating_sub(at) < LOAD_SAMPLE_EVERY_MS) {
        return Ok(());
    }
    conn.execute(
        "INSERT OR REPLACE INTO load_samples(sampled_ms,load,cpus) VALUES(?1,?2,?3)",
        params![now_ms, host.load, host.cpus],
    )?;
    conn.execute(
        "DELETE FROM load_samples WHERE sampled_ms < ?1",
        [now_ms - LOAD_SAMPLE_KEEP_MS],
    )?;
    Ok(())
}

/// The load alarm worth raising or keeping. Load per CPU is the measure
/// admission gates CPU-declaring jobs on (`max_load_per_cpu`), read from the
/// raw load average. Not yet `raised`, it needs readings at or above `gate`
/// spanning `LOAD_SUSTAINED_MS`, with no gap longer than
/// `LOAD_SAMPLE_GAP_MS`, up to a fresh one. Once raised, it stays until the
/// readings have stayed below the gate for `LOAD_SETTLED_MS`.
pub fn load_alarm(
    conn: &Connection,
    gate: f64,
    raised: bool,
    now_ms: i64,
) -> Result<Option<LoadSpell>> {
    let mut statement = conn.prepare(
        "SELECT sampled_ms,load,cpus FROM load_samples WHERE sampled_ms>=?1
         ORDER BY sampled_ms DESC",
    )?;
    let readings = statement
        .query_map([now_ms - LOAD_SAMPLE_KEEP_MS], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, f64>(2)?.max(1.0),
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let Some(&(newest_ms, load, cpus)) = readings.first() else {
        return Ok(None);
    };
    if now_ms - newest_ms > LOAD_SAMPLE_GAP_MS {
        return Ok(None);
    }
    // How long the newest readings have stayed on one side of the gate.
    let stretch = |high: bool| -> i64 {
        let mut oldest = newest_ms;
        for &(ms, load, cpus) in &readings {
            if (load / cpus >= gate) != high || oldest - ms > LOAD_SAMPLE_GAP_MS {
                break;
            }
            oldest = ms;
        }
        newest_ms - oldest
    };
    let high = stretch(true);
    let alarm = if raised {
        stretch(false) < LOAD_SETTLED_MS
    } else {
        high >= LOAD_SUSTAINED_MS
    };
    Ok(alarm.then_some(LoadSpell {
        load,
        cpus,
        per_cpu: load / cpus,
        span_ms: high,
    }))
}

/// Settings and live readings for resource.status.
pub fn report(settings: &Settings, host: &Host) -> Value {
    json!({"settings": settings, "host": host})
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_older_ledger_gains_the_trimmed_column() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE disk_samples (path TEXT NOT NULL, sampled_ms INTEGER NOT NULL,
               free_bytes INTEGER NOT NULL, PRIMARY KEY (path, sampled_ms));
             INSERT INTO disk_samples VALUES('k', 1, 5);",
        )
        .unwrap();
        init(&conn).unwrap();
        init(&conn).unwrap();
        conn.execute(
            "INSERT INTO disk_samples(path,sampled_ms,free_bytes,trimmed_bytes) VALUES('k',2,4,1)",
            [],
        )
        .unwrap();
        let old: i64 = conn
            .query_row(
                "SELECT trimmed_bytes FROM disk_samples WHERE sampled_ms=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old, 0);
    }

    use super::*;

    fn host(load: f64) -> Host {
        Host {
            cpus: 10.0,
            load,
            memory_available_bytes: 8 * GIB,
            memory_total_bytes: 16 * GIB,
            disk_available_bytes: 0,
            disk_total_bytes: 0,
        }
    }

    fn heavy(released_load: f64) -> Request<'static> {
        Request {
            heavy: true,
            home: std::path::Path::new("/"),
            charter: "app",
            charter_cap: None,
            demand: Some(Demand {
                cpus: 10.0,
                memory_bytes: 2 * GIB,
                disk_bytes: 4 * GIB,
            }),
            charter_min_free_bytes: 0,
            disk_key: "dev:1",
            disk_available_bytes: 100 * GIB,
            disk_total_bytes: 200 * GIB,
            released_load,
            started_by: None,
        }
    }

    fn light(released_load: f64) -> Request<'static> {
        Request {
            heavy: false,
            demand: None,
            ..heavy(released_load)
        }
    }

    fn ledger() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        conn
    }

    #[test]
    fn a_build_that_just_ended_does_not_hold_back_the_next_one() {
        let settings = Settings {
            min_free_disk_bytes: 0,
            ..Settings::default()
        };
        // The average still shows the last build: 14 on 10 CPUs.
        let lingering = host(14.0);
        assert!(admit(&settings, &lingering, 0, &[], &heavy(0.0)).is_err());
        let credit = released_load(10.0, 14.0, 5_000);
        assert!(admit(&settings, &lingering, 0, &[], &heavy(credit)).is_ok());
        // Other load still counts: credit only covers what the job could
        // have caused.
        let busy = host(25.0);
        assert!(admit(&settings, &busy, 0, &[], &heavy(credit)).is_err());
    }

    #[test]
    fn a_build_under_its_own_crew_runs_alone_on_the_floor() {
        let settings = Settings::default();
        // The crew holds 6 GiB of the 7 GiB free; the build needs 2 GiB more.
        let host = Host {
            memory_available_bytes: 7 * GIB,
            ..host(0.0)
        };
        let crew = |job: &'static str| Held {
            heavy: false,
            job_id: job,
            demand: Some(Demand {
                cpus: 1.0,
                memory_bytes: 6 * GIB,
                disk_bytes: 0,
            }),
            ..held(0)
        };
        let build = Request {
            started_by: Some("crew-job"),
            demand: Some(Demand {
                cpus: 1.0,
                memory_bytes: 2 * GIB,
                disk_bytes: 0,
            }),
            ..heavy(0.0)
        };
        // Its own crew's reservation is not another job: the build runs alone
        // on the floor, and is admitted.
        assert!(admit(&settings, &host, 0, &[crew("crew-job")], &build).is_ok());
        // Another job's crew still counts, so the build waits.
        assert!(admit(&settings, &host, 0, &[crew("other-job")], &build).is_err());
    }

    #[test]
    fn a_crew_memory_resolves_route_host_manifest_then_default() {
        let mut settings = Settings {
            crew_default_memory_bytes: GIB,
            ..Settings::default()
        };
        // A manifest that declares none reserves the host's default, and
        // lowering that default moves it.
        assert_eq!(
            crew_memory_source(&settings, "pi", None, None),
            (GIB, "default")
        );
        settings.crew_default_memory_bytes = GIB / 4;
        assert_eq!(
            crew_memory_source(&settings, "pi", None, None),
            (GIB / 4, "default")
        );
        // A declared manifest figure outranks the default.
        assert_eq!(
            crew_memory_source(&settings, "pi", Some(3 * GIB), None),
            (3 * GIB, "manifest")
        );
        // This host's figure for the harness outranks the manifest, and a
        // route's own figure outranks both.
        settings.crew_memory_bytes.insert("pi".to_owned(), GIB / 2);
        assert_eq!(
            crew_memory_source(&settings, "pi", Some(3 * GIB), None),
            (GIB / 2, "host")
        );
        assert_eq!(
            crew_memory_source(&settings, "pi", Some(3 * GIB), Some(GIB / 8)),
            (GIB / 8, "route")
        );
        let report = crew_memory_report(&settings, &[("pi", None), ("claude", Some(3 * GIB))]);
        assert_eq!(
            report,
            json!([
                {"harness":"pi","memory_bytes":GIB / 2,"source":"host"},
                {"harness":"claude","memory_bytes":3 * GIB,"source":"manifest"},
            ])
        );
    }

    #[test]
    fn a_crew_starts_on_a_busy_machine_when_its_memory_fits() {
        let settings = Settings {
            min_free_disk_bytes: 0,
            ..Settings::default()
        };
        // Load 21.77 on 10 CPUs is far past the limit of 1.0 per CPU.
        let busy = host(21.77);
        let crew = |memory_bytes: u64| Request {
            heavy: false,
            demand: Some(Demand {
                cpus: 0.0,
                memory_bytes,
                disk_bytes: 0,
            }),
            ..heavy(0.0)
        };
        assert!(admit(&settings, &busy, 0, &[], &crew(2 * GIB)).is_ok());
        // Memory still gates a crew: another estimated job holds 7 of the 8 GiB.
        let other = Held {
            heavy: false,
            home: std::path::Path::new("/"),
            charter: None,
            created_ms: 0,
            demand: Some(Demand {
                cpus: 0.0,
                memory_bytes: 7 * GIB,
                disk_bytes: 0,
            }),
            disk_key: None,
            disk_free_at_admit: None,
            disk_trimmed: 0,
            job_id: "other",
        };
        let refused = admit(&settings, &busy, 0, &[other], &crew(2 * GIB)).unwrap_err();
        assert!(refused.to_string().contains("MiB available"), "{refused}");
        // A heavy job declares CPUs, so the same load still holds it back.
        let refused = admit(&settings, &busy, 0, &[], &heavy(0.0)).unwrap_err();
        assert!(refused.to_string().contains("CPU demand"), "{refused}");
    }

    #[test]
    fn released_load_fades_with_the_one_minute_average_and_never_exceeds_it() {
        assert_eq!(released_load(10.0, 14.0, 0), 10.0);
        assert_eq!(released_load(10.0, 4.0, 0), 4.0);
        let minute = released_load(10.0, 14.0, 60_000);
        assert!((minute - 10.0 / std::f64::consts::E).abs() < 1e-9);
        assert!(released_load(10.0, 14.0, 600_000) < 0.001);
    }

    /// A job admitted with `disk_bytes` declared, when 36 GiB were free.
    fn held(disk_bytes: u64) -> Held<'static> {
        Held {
            heavy: true,
            home: std::path::Path::new("/"),
            charter: Some("app"),
            created_ms: 0,
            demand: Some(Demand {
                cpus: 1.0,
                memory_bytes: 0,
                disk_bytes,
            }),
            disk_key: Some("dev:1"),
            disk_free_at_admit: Some(36 * GIB),
            disk_trimmed: 0,
            job_id: "job",
        }
    }

    /// A held job on `disk_key` that declared `disk_bytes`, admitted when
    /// `free_at_admit` was free.
    fn held_on(disk_key: &'static str, disk_bytes: u64, free_at_admit: u64) -> Held<'static> {
        Held {
            disk_key: Some(disk_key),
            disk_free_at_admit: Some(free_at_admit),
            ..held(disk_bytes)
        }
    }

    #[test]
    fn space_a_running_job_already_wrote_is_not_counted_twice() {
        let settings = Settings {
            min_free_disk_bytes: 2 * GIB,
            max_load_per_cpu: 1024.0,
            ..Settings::default()
        };
        let host = host(0.0);
        // A build declared 4 GiB at 36 GiB free and has since written it: the
        // disk now reads 32 GiB free. Its declared space is no longer ahead of
        // the reading, so a second 4 GiB job still fits.
        let mut request = heavy(0.0);
        request.disk_available_bytes = 32 * GIB;
        assert!(
            admit(
                &settings,
                &host,
                0,
                &[held_on("dev:1", 4 * GIB, 36 * GIB)],
                &request
            )
            .is_ok()
        );
        // Nothing written yet, the same job is counted in full: at 9 GiB free
        // a 4 GiB build with a 2 GiB floor and a 4 GiB build held do not fit.
        request.disk_available_bytes = 9 * GIB;
        assert!(
            admit(
                &settings,
                &host,
                0,
                &[held_on("dev:1", 4 * GIB, 9 * GIB)],
                &request
            )
            .is_err()
        );
        // Two builds admitted at 36 GiB, 4 GiB each, and 4 GiB lost since: the
        // loss is one build's worth, so 4 GiB is still ahead of them, not 8
        // and not 0. A 28 GiB job then needs 2 + 4 + 28 = 34 GiB, not 32.
        let two = [
            held_on("dev:1", 4 * GIB, 36 * GIB),
            held_on("dev:1", 4 * GIB, 36 * GIB),
        ];
        request.demand = Some(Demand {
            cpus: 1.0,
            memory_bytes: 0,
            disk_bytes: 28 * GIB,
        });
        request.disk_available_bytes = 32 * GIB;
        assert!(admit(&settings, &host, 0, &two, &request).is_err());
        request.demand.as_mut().unwrap().disk_bytes = 26 * GIB;
        assert!(admit(&settings, &host, 0, &two, &request).is_ok());
    }

    #[test]
    fn a_trim_is_neither_a_rise_nor_a_reset_of_the_trend() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        let hour = 3_600_000;
        // Space falls by half a gibibyte every half hour for three hours.
        for step in 0..=6 {
            let free = 20 * GIB - step * GIB / 2;
            note_disk(&conn, "dev:1", free, step as i64 * hour / 2).unwrap();
        }
        // A trim frees half a gibibyte at three hours; the disk reads 17.5 GiB
        // free after it, and falls on from there.
        record_trim(&conn, "dev:1", GIB / 2, 17 * GIB + GIB / 2, 3 * hour).unwrap();
        note_disk(&conn, "dev:1", 17 * GIB, 3 * hour + hour / 2).unwrap();
        note_disk(&conn, "dev:1", 16 * GIB + GIB / 2, 4 * hour).unwrap();
        let trend = disk_trend(&conn, "dev:1").unwrap().unwrap();
        // The window still starts at the first reading: the trim did not reset
        // it, and the half gibibyte it returned is not a rise.
        assert_eq!(trend.from_ms, 0);
        assert_eq!(trend.from_free_bytes, 20 * GIB);
        assert!((trend.bytes_per_hour + GIB as f64).abs() < 1.0, "{trend:?}");
        assert_eq!(trim_total(&conn, "dev:1").unwrap(), GIB / 2);
    }

    #[test]
    fn a_falling_disk_with_regular_trims_still_warns_and_a_trim_does_not_clear_it() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        let hour = 3_600_000;
        let floor = 2 * GIB;
        // Builds take two gibibytes an hour. Every hour a trim gives one back,
        // so the disk reads one gibibyte an hour lower, with a trim each hour.
        let mut free = 30 * GIB;
        for step in 0..=72i64 {
            let ms = step * DISK_SAMPLE_EVERY_MS;
            if step > 0 {
                free -= GIB / 6;
            }
            if step > 0 && step % 12 == 0 {
                free += GIB;
                record_trim(&conn, "dev:1", GIB, free, ms).unwrap();
            } else {
                note_disk(&conn, "dev:1", free, ms).unwrap();
            }
        }
        let trend = disk_trend(&conn, "dev:1").unwrap().expect("a rate");
        // Six hours of readings, none lost to the trims.
        assert_eq!(trend.from_ms, 0);
        assert_eq!(trend.to_ms, 6 * hour);
        assert!(
            (trend.bytes_per_hour + 2.0 * GIB as f64).abs() < GIB as f64 / 100.0,
            "{trend:?}"
        );
        // Still eleven hours from the floor at the rate that is left after the
        // trims, so the owner is warned in time, and a raised warning holds.
        let warning = disk_warning(floor, 24 * 3600, false, &trend).expect("a warning");
        assert!((warning.hours_to_floor - 11.0).abs() < 0.1, "{warning:?}");
        assert!(disk_warning(floor, 24 * 3600, true, &trend).is_some());
    }

    #[test]
    fn a_trim_during_a_held_job_is_not_credited_to_it() {
        let settings = Settings {
            min_free_disk_bytes: 2 * GIB,
            max_load_per_cpu: 1024.0,
            ..Settings::default()
        };
        let host = host(0.0);
        // A 4 GiB build admitted at 36 GiB free writes 1 GiB, while a trim
        // frees 3 GiB, so the disk reads 38 GiB free. The build has written
        // 1 GiB, so 3 GiB of its declared space is still ahead: a 33 GiB job
        // fits in 38 GiB with the 2 GiB floor. Reading the trim as the build's
        // own gain would credit it with 4 GiB and refuse this job.
        let mut trimmed = held_on("dev:1", 4 * GIB, 36 * GIB);
        trimmed.disk_trimmed = 3 * GIB;
        let mut request = heavy(0.0);
        request.demand = Some(Demand {
            cpus: 1.0,
            memory_bytes: 0,
            disk_bytes: 33 * GIB,
        });
        request.disk_available_bytes = 38 * GIB;
        assert!(admit(&settings, &host, 0, &[trimmed], &request).is_ok());
        let untrimmed = held_on("dev:1", 4 * GIB, 36 * GIB);
        assert!(admit(&settings, &host, 0, &[untrimmed], &request).is_err());
    }

    #[test]
    fn a_volume_only_counts_the_reservations_on_it() {
        let settings = Settings {
            min_free_disk_bytes: 2 * GIB,
            max_load_per_cpu: 1024.0,
            ..Settings::default()
        };
        let host = host(0.0);
        let mut request = heavy(0.0);
        request.disk_available_bytes = 6 * GIB;
        // A 31 GiB build on another filesystem leaves this one alone.
        assert!(
            admit(
                &settings,
                &host,
                0,
                &[held_on("dev:2", 31 * GIB, 36 * GIB)],
                &request
            )
            .is_ok()
        );
        assert!(
            admit(
                &settings,
                &host,
                0,
                &[held_on("dev:1", 31 * GIB, 36 * GIB)],
                &request
            )
            .is_err()
        );
    }

    #[test]
    fn a_refusal_names_the_resource_a_queued_job_may_pass_over() {
        let settings = Settings {
            min_free_disk_bytes: 0,
            max_load_per_cpu: 1024.0,
            ..Settings::default()
        };
        let host = host(0.0);
        // Four GiB declared on three GiB free: refused on disk.
        let mut request = heavy(0.0);
        request.disk_available_bytes = 3 * GIB;
        let refused = admit(&settings, &host, 0, &[], &request).unwrap_err();
        let refused = refused.downcast_ref::<Refused>().unwrap();
        assert_eq!(refused.blocker, Some(Blocker::Disk), "{refused}");
        // A light job on the same machine is not refused for disk at all.
        let light_job = light(0.0);
        assert!(admit(&settings, &host, 0, &[], &light_job).is_ok());
        // Memory is not a resource a queued job can pass over.
        let settings = Settings {
            min_available_memory_bytes: 16 * GIB,
            ..settings
        };
        let memory = Request {
            demand: Some(Demand {
                cpus: 0.0,
                memory_bytes: GIB,
                disk_bytes: 0,
            }),
            ..light(0.0)
        };
        let refused = admit(&settings, &host, 0, &[], &memory).unwrap_err();
        let refused = refused.downcast_ref::<Refused>().unwrap();
        assert_eq!(refused.blocker, None, "{refused}");
    }

    #[test]
    fn a_head_passes_over_only_work_that_leaves_its_resource_alone() {
        let disk = Some(Demand {
            cpus: 0.0,
            memory_bytes: GIB,
            disk_bytes: GIB,
        });
        let cpu = Some(Demand {
            cpus: 2.0,
            memory_bytes: 0,
            disk_bytes: 0,
        });
        let memory = Some(Demand {
            cpus: 0.0,
            memory_bytes: GIB,
            disk_bytes: 0,
        });
        assert!(!Blocker::Disk.passable_by(disk, false));
        assert!(Blocker::Disk.passable_by(memory, false));
        assert!(Blocker::Disk.passable_by(None, true));
        assert!(!Blocker::Cpu.passable_by(cpu, false));
        assert!(Blocker::Cpu.passable_by(memory, false));
        assert!(!Blocker::HeavySlot.passable_by(None, true));
        assert!(Blocker::HeavySlot.passable_by(memory, false));
    }

    #[test]
    fn a_job_that_can_never_fit_is_refused_rather_than_queued() {
        let settings = Settings {
            min_free_disk_bytes: 2 * GIB,
            max_load_per_cpu: 1024.0,
            ..Settings::default()
        };
        let host = host(0.0);
        let mut request = heavy(0.0);
        // A 4 GiB declaration and the 2 GiB floor need 6 GiB; a 5 GiB disk
        // never has that, however long the job waits.
        request.disk_total_bytes = 5 * GIB;
        request.disk_available_bytes = 5 * GIB;
        let refused = admit(&settings, &host, 0, &[], &request).unwrap_err();
        assert!(refused.downcast_ref::<Refused>().is_none(), "{refused}");
        assert!(refused.to_string().contains("never fits"), "{refused}");
        request.disk_total_bytes = 6 * GIB;
        request.disk_available_bytes = 6 * GIB;
        assert!(admit(&settings, &host, 0, &[], &request).is_ok());
    }

    #[test]
    fn only_a_job_declared_heavy_takes_the_heavy_disk_default() {
        let settings = Settings::default();
        let host = host(0.0);
        // Unlabelled work is heavy for CPU and memory but declares no disk.
        assert_eq!(
            demand(&settings, &host, true, false, None)
                .unwrap()
                .disk_bytes,
            0
        );
        assert_eq!(
            demand(&settings, &host, true, false, None)
                .unwrap()
                .memory_bytes,
            2 * GIB
        );
        // A declared demand still counts either way.
        let declared = estimate(&json!({"disk_bytes": GIB})).unwrap();
        assert_eq!(
            demand(&settings, &host, true, false, Some(&declared))
                .unwrap()
                .disk_bytes,
            GIB
        );
    }

    #[test]
    fn a_heavy_job_counts_its_disk_demand_and_the_disk_running_jobs_hold() {
        let settings = Settings {
            min_free_disk_bytes: 2 * GIB,
            max_load_per_cpu: 1024.0,
            ..Settings::default()
        };
        let host = host(0.0);
        assert_eq!(
            demand(&settings, &host, true, true, None)
                .unwrap()
                .disk_bytes,
            4 * GIB
        );
        // 36 GiB free, a 2 GiB floor and the 4 GiB heavy default: it fits.
        let mut request = heavy(0.0);
        request.disk_available_bytes = 36 * GIB;
        assert!(admit(&settings, &host, 0, &[], &request).is_ok());
        // One byte short of the floor plus the job's own demand, it does not.
        request.disk_available_bytes = 6 * GIB - 1;
        let refused = admit(&settings, &host, 0, &[], &request).unwrap_err();
        let message = refused.to_string();
        assert!(message.contains("disk available"), "{message}");
        assert!(message.contains("declared by running jobs"), "{message}");
        assert!(message.contains("heavy_estimate.disk_bytes"), "{message}");
        // A build already running holds its space for as long as it runs, not
        // only through the ramp window a live reading would cover.
        request.disk_available_bytes = 36 * GIB;
        assert!(admit(&settings, &host, 0, &[], &request).is_ok());
        assert!(admit(&settings, &host, 0, &[held(31 * GIB)], &request).is_err());
        assert!(admit(&settings, &host, 0, &[held(29 * GIB)], &request).is_ok());
        // A light job with no estimate still obeys only the floor.
        let mut light = light(0.0);
        light.disk_available_bytes = 2 * GIB;
        assert!(admit(&settings, &host, 0, &[], &light).is_ok());
        light.disk_available_bytes = 2 * GIB - 1;
        assert!(admit(&settings, &host, 0, &[], &light).is_err());
        // A light job may declare disk without declaring CPU or memory.
        let mut declared = light.clone();
        declared.demand = demand(
            &settings,
            &host,
            false,
            false,
            Some(&estimate(&json!({"disk_bytes": 4 * GIB})).unwrap()),
        );
        declared.disk_available_bytes = 6 * GIB;
        assert!(admit(&settings, &host, 0, &[], &declared).is_ok());
        declared.disk_available_bytes = 6 * GIB - 1;
        assert!(admit(&settings, &host, 0, &[], &declared).is_err());
    }

    #[test]
    fn free_space_samples_are_bounded_and_need_a_spread_to_read_a_rate() {
        let conn = ledger();
        // One reading, or two minutes apart, is not a rate.
        note_disk(&conn, "/data", 36 * GIB, 0).unwrap();
        assert!(disk_trend(&conn, "/data").unwrap().is_none());
        note_disk(&conn, "/data", 35 * GIB, 2 * 60_000).unwrap();
        assert!(disk_trend(&conn, "/data").unwrap().is_none());
        // Within the sampling interval a reading is not kept twice.
        note_disk(
            &conn,
            "/data",
            34 * GIB,
            2 * 60_000 + DISK_SAMPLE_EVERY_MS - 1,
        )
        .unwrap();
        assert_eq!(samples(&conn, "/data"), 2);
        // Half an hour of readings still describes no rate.
        note_disk(&conn, "/data", 33 * GIB, 30 * 60_000).unwrap();
        assert!(disk_trend(&conn, "/data").unwrap().is_none());
        // Samples older than the retention window are dropped, for every
        // filesystem, so the table stays bounded however long the ledger lives.
        note_disk(&conn, "/other", 30 * GIB, 0).unwrap();
        let later = DISK_SAMPLE_KEEP_MS + 30 * 60_000 + 1;
        note_disk(&conn, "/data", 30 * GIB, later).unwrap();
        assert_eq!(samples(&conn, "/data"), 1);
        assert_eq!(samples(&conn, "/other"), 0);
    }

    /// Free space in GiB at `ms` for a day's readings: 36 GiB falling 1 GiB an
    /// hour, and after `cleanup_ms` a 20 GiB cleanup and a fall of 4 GiB an hour.
    fn falling_day(ms: i64, cleanup_ms: Option<i64>) -> u64 {
        let hours = ms as f64 / 3_600_000.0;
        let gib = match cleanup_ms {
            Some(at) if ms >= at => {
                let since = (ms - at) as f64 / 3_600_000.0;
                26.0 + 20.0 - 4.0 * since
            }
            _ => 36.0 - hours,
        };
        (gib * GIB as f64) as u64
    }

    #[test]
    fn the_rate_comes_from_the_recent_window_only() {
        let conn = ledger();
        let end = 19 * 3_600_000;
        for ms in (0..=end).step_by(DISK_SAMPLE_EVERY_MS as usize) {
            note_disk(&conn, "/data", falling_day(ms, None), ms).unwrap();
        }
        let trend = disk_trend(&conn, "/data").unwrap().expect("a rate");
        assert_eq!(trend.path, "/data");
        // The last twelve hours: the readings before the window do not count.
        assert_eq!(trend.from_ms, end - 12 * 3_600_000);
        assert_eq!(trend.to_ms, end);
        assert_eq!(trend.to_free_bytes, falling_day(end, None));
        // A straight fall of one GiB an hour fits exactly.
        assert!((trend.bytes_per_hour + GIB as f64).abs() < 1.0, "{trend:?}");
        let forecast = disk_forecast(2 * GIB, &trend).expect("a falling rate is read");
        assert!((forecast.gib_per_hour - 1.0).abs() < 1e-6, "{forecast:?}");
        assert_eq!(forecast.span_ms, 12 * 3_600_000);
    }

    #[test]
    fn a_fall_after_a_cleanup_is_still_seen() {
        let conn = ledger();
        let cleanup = 10 * 3_600_000;
        let end = 12 * 3_600_000;
        for ms in (0..=end).step_by(DISK_SAMPLE_EVERY_MS as usize) {
            note_disk(&conn, "/data", falling_day(ms, Some(cleanup)), ms).unwrap();
        }
        // The 20 GiB freed at ten hours would flatten a fit across the window,
        // but the rate is read from the readings after it: four GiB an hour.
        let trend = disk_trend(&conn, "/data").unwrap().expect("a rate");
        assert_eq!(trend.from_ms, cleanup);
        assert!(
            (trend.bytes_per_hour + 4.0 * GIB as f64).abs() < 1.0,
            "{trend:?}"
        );
        assert!(disk_warning(GIB, 86_400, false, &trend).is_some());
    }

    #[test]
    fn a_warning_clears_only_once_the_floor_is_well_beyond_the_horizon() {
        let hour = 3_600_000;
        // Thirty hours to the floor: outside a day's horizon, so no warning
        // yet, but a raised warning holds until forty-eight hours.
        let thirty = trend_of(0, 21 * GIB, 6 * hour, 20 * GIB);
        assert!(disk_warning(15 * GIB, 86_400, false, &thirty).is_none());
        assert!(disk_warning(15 * GIB, 86_400, true, &thirty).is_some());
        let sixty = trend_of(0, 21 * GIB, 12 * hour, 20 * GIB);
        assert!(disk_warning(15 * GIB, 86_400, true, &sixty).is_none());
    }

    #[test]
    fn a_falling_rate_warns_before_the_floor_but_not_after_it() {
        let hour = 3_600_000;
        // A day's fall from 36 GiB free to 8.6 GiB against a 15 GiB floor.
        // Under the floor already, which is the floor's own alarm rather than
        // a forecast.
        let yesterday = trend_of(0, 36 * GIB, 19 * hour, 8_600 * 1024 * 1024);
        assert!(disk_warning(15 * GIB, 86_400, false, &yesterday).is_none());
        // Half a day in, still above the floor with hours to spare: the owner
        // hears about it while there is still time to act.
        let midday = trend_of(0, 36 * GIB, 6 * hour, 20 * GIB);
        let forecast =
            disk_warning(15 * GIB, 86_400, false, &midday).expect("a warning worth sending");
        assert!(
            (forecast.gib_per_hour - 16.0 / 6.0).abs() < 0.01,
            "{forecast:?}"
        );
        assert!(
            (forecast.hours_to_floor - 5.0 / (16.0 / 6.0)).abs() < 0.05,
            "{forecast:?}"
        );
        // A week at the same rate is not this afternoon's problem.
        let week = trend_of(0, 36 * GIB, 7 * 24 * hour, 20 * GIB);
        assert!(disk_warning(15 * GIB, 86_400, false, &week).is_none());
        // Space recovered, so nothing is falling.
        let recovered = trend_of(0, 36 * GIB, 20 * hour, 40 * GIB);
        assert!(disk_warning(15 * GIB, 86_400, false, &recovered).is_none());
        // Falling, but too slowly to reach the floor within the horizon.
        let slow = trend_of(0, 36 * GIB, 20 * hour, 35 * GIB);
        assert!(disk_warning(15 * GIB, 86_400, false, &slow).is_none());
        assert!(disk_warning(15 * GIB, 86_400 * 30, false, &slow).is_some());
        // A horizon of zero warns only once the floor is crossed, which the
        // forecast itself excludes.
        assert!(disk_warning(15 * GIB, 0, false, &midday).is_none());
        // Two readings minutes apart do not describe a rate.
        let brief = trend_of(0, 36 * GIB, 60_000, 20 * GIB);
        assert!(disk_warning(15 * GIB, 86_400 * 30, false, &brief).is_none());
    }

    /// A two-reading trend, slope taken between the two.
    fn trend_of(from_ms: i64, from: u64, to_ms: i64, to: u64) -> DiskTrend {
        let hours = (to_ms - from_ms) as f64 / 3_600_000.0;
        DiskTrend {
            path: "/data".into(),
            from_ms,
            from_free_bytes: from,
            to_ms,
            to_free_bytes: to,
            bytes_per_hour: (to as f64 - from as f64) / hours,
        }
    }

    fn samples(conn: &Connection, path: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM disk_samples WHERE path=?1",
            [path],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// One load reading a minute from `from_min` to `to_min`, each at `load`
    /// on 10 CPUs.
    fn loads(conn: &Connection, from_min: i64, to_min: i64, load: f64) {
        for minute in from_min..=to_min {
            note_load(conn, &Host { load, ..host(0.0) }, minute * 60_000).unwrap();
        }
    }

    #[test]
    fn only_load_held_above_the_gate_raises_the_alarm_and_only_settled_load_clears_it() {
        let conn = ledger();
        let at = |minute: i64| minute * 60_000;
        // A burst of five minutes over a quiet machine is not an alarm.
        loads(&conn, 0, 9, 1.0);
        loads(&conn, 10, 14, 80.0);
        assert_eq!(load_alarm(&conn, 1.0, false, at(14)).unwrap(), None);
        // Held for ten minutes it is, with the load, CPUs and span.
        loads(&conn, 15, 20, 80.0);
        let spell = load_alarm(&conn, 1.0, false, at(20)).unwrap().unwrap();
        assert_eq!(spell.load, 80.0);
        assert_eq!(spell.cpus, 10.0);
        assert_eq!(spell.per_cpu, 8.0);
        assert_eq!(spell.span_ms, at(10));
        // A raised alarm survives a quiet minute or two between jobs.
        loads(&conn, 21, 23, 1.0);
        assert!(load_alarm(&conn, 1.0, true, at(23)).unwrap().is_some());
        // Five quiet minutes clear it.
        loads(&conn, 24, 26, 1.0);
        assert_eq!(load_alarm(&conn, 1.0, true, at(26)).unwrap(), None);
        // The gate is the one admission uses now: a looser gate is no alarm.
        loads(&conn, 27, 40, 80.0);
        assert!(load_alarm(&conn, 1.0, false, at(40)).unwrap().is_some());
        assert_eq!(load_alarm(&conn, 16.0, false, at(40)).unwrap(), None);
        // A stale reading says nothing about the machine now.
        assert_eq!(load_alarm(&conn, 1.0, true, at(44)).unwrap(), None);
    }

    #[test]
    fn load_readings_across_a_gap_are_not_one_stretch_and_the_ring_is_bounded() {
        let conn = ledger();
        let at = |minute: i64| minute * 60_000;
        // Nobody watched between minutes 6 and 12, so the load is not known
        // to have held across them.
        loads(&conn, 0, 6, 80.0);
        loads(&conn, 12, 15, 80.0);
        assert_eq!(load_alarm(&conn, 1.0, false, at(15)).unwrap(), None);
        // Within the sampling interval a reading is not kept twice.
        note_load(&conn, &host(80.0), at(15) + LOAD_SAMPLE_EVERY_MS - 1).unwrap();
        let count = |conn: &Connection| -> i64 {
            conn.query_row("SELECT COUNT(*) FROM load_samples", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count(&conn), 11);
        // Readings past the retention window are dropped.
        loads(&conn, 16, 200, 80.0);
        assert_eq!(count(&conn), LOAD_SAMPLE_KEEP_MS / 60_000 + 1);
    }
}
