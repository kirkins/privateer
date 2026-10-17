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

const GIB: u64 = 1024 * 1024 * 1024;

/// A request that does not fit right now. Unlike other errors it can wait in
/// the capacity queue and succeed later.
#[derive(Debug)]
pub struct Refused(pub String);
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Refused {}

macro_rules! refuse_unless {
    ($condition:expr, $($message:tt)+) => {
        let fits: bool = $condition;
        if !fits {
            return Err(Refused(format!($($message)+)).into());
        }
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
    /// Free space on the filesystem the job will use, as the caller measured
    /// it; the same reading is kept as a sample for the falling-space trend.
    pub disk_available_bytes: u64,
    /// Load the one-minute average still carries from jobs that just ended,
    /// which no longer run (see `released_load`).
    pub released_load: f64,
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
           free_bytes INTEGER NOT NULL, PRIMARY KEY (path, sampled_ms));",
    )?;
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

/// The demand to record for a job, or None when it is neither heavy nor
/// estimated (such jobs are not subject to admission).
pub fn demand(
    settings: &Settings,
    host: &Host,
    heavy: bool,
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
            disk_bytes: settings.heavy_estimate.disk_bytes.unwrap_or(0),
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
    refuse_unless!(
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
    // held counts, not just recent ones: a running build's bytes are still on
    // the filesystem, and no reading reports them. A job's own demand always
    // counts, since starting it is what fills the disk.
    let committed: u64 = held
        .iter()
        .filter_map(|h| h.demand)
        .fold(0u64, |held, d| held.saturating_add(d.disk_bytes));
    let needed = (required_disk as i128)
        .saturating_add(committed as i128)
        .saturating_add(demand.disk_bytes as i128);
    refuse_unless!(
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
            refuse_unless!(heavy < cap, "heavy-job capacity exhausted ({heavy}/{cap})");
        }
        if let Some(cap) = request.charter_cap {
            let share = held
                .iter()
                .filter(|h| h.heavy && h.home == request.home && h.charter == Some(request.charter))
                .count() as u64;
            refuse_unless!(
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
    let load = (host.load - request.released_load).max(0.0);
    let per_cpu = (load + recent_cpus) / host.cpus;
    refuse_unless!(
        per_cpu < settings.max_load_per_cpu,
        "admission refused: CPU demand {per_cpu:.2} per CPU is at the limit {} (load {:.2}, {:.1} of it from jobs that just ended, {recent_cpus:.1} CPUs recently admitted, {} CPUs); retry later or tune resource.configure",
        settings.max_load_per_cpu,
        host.load,
        request.released_load.min(host.load),
        host.cpus
    );
    // Memory: running out is worse than waiting, so the job's own estimate
    // must fit above the floor too, while other estimated jobs run. The
    // estimate keeps big jobs from starting together; it must not keep the
    // only one from starting on a small machine whose person is using it, so
    // a job running alone needs only the floor.
    let alone = held.iter().all(|h| h.demand.is_none());
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

pub fn measure() -> Result<Host> {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get() as f64)
        .unwrap_or(1.0);
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
/// A falling rate is believed only once the samples span this long, so one
/// build that wrote a gigabyte cannot read as a machine filling up.
const DISK_TREND_MIN_SPAN_MS: i64 = 30 * 60_000;

/// How free space on one filesystem has moved across the kept samples.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DiskTrend {
    /// The filesystem the samples describe, as `disk_key`.
    pub path: String,
    pub from_ms: i64,
    pub from_free_bytes: u64,
    pub to_ms: i64,
    pub to_free_bytes: u64,
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
/// so a later reading can say how fast space is falling. Callers pass the
/// reading they just decided against, so the trend and the decision agree.
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
    conn.execute(
        "INSERT INTO disk_samples(path,sampled_ms,free_bytes) VALUES(?1,?2,?3)
         ON CONFLICT(path,sampled_ms) DO UPDATE SET free_bytes=excluded.free_bytes",
        params![path, now_ms, free.min(i64::MAX as u64) as i64],
    )?;
    conn.execute(
        "DELETE FROM disk_samples WHERE path=?1 AND sampled_ms < ?2",
        params![path, now_ms - DISK_SAMPLE_KEEP_MS],
    )?;
    Ok(())
}

/// The oldest and newest kept samples for a filesystem, when they span long
/// enough to describe a rate. None until readings that far apart exist, so a
/// caller cannot read a rate out of one build's writing.
pub fn disk_trend(conn: &Connection, path: &str) -> Result<Option<DiskTrend>> {
    let mut statement = conn.prepare(
        "SELECT sampled_ms, free_bytes FROM disk_samples WHERE path=?1 ORDER BY sampled_ms",
    )?;
    let samples = statement
        .query_map([path], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u64))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let Some(&(from_ms, from_free_bytes)) = samples.first() else {
        return Ok(None);
    };
    let &(to_ms, to_free_bytes) = samples.last().expect("a first sample implies a last");
    if to_ms.saturating_sub(from_ms) < DISK_TREND_MIN_SPAN_MS {
        return Ok(None);
    }
    Ok(Some(DiskTrend {
        path: path.to_owned(),
        from_ms,
        from_free_bytes,
        to_ms,
        to_free_bytes,
    }))
}

/// Project the observed rate onto the floor. None when space is not falling,
/// when the floor is already crossed (the floor's own alarm covers that), or
/// when the samples are too close together to describe a rate.
pub fn disk_forecast(floor: u64, trend: &DiskTrend) -> Option<DiskForecast> {
    let span_ms = trend.to_ms.saturating_sub(trend.from_ms);
    if span_ms < DISK_TREND_MIN_SPAN_MS || trend.to_free_bytes <= floor {
        return None;
    }
    let lost = trend.from_free_bytes.checked_sub(trend.to_free_bytes)? as f64;
    let per_hour = lost / (span_ms as f64 / 3_600_000.0);
    if per_hour <= 0.0 {
        return None;
    }
    Some(DiskForecast {
        gib_per_hour: per_hour / GIB as f64,
        hours_to_floor: (trend.to_free_bytes - floor) as f64 / per_hour,
        span_ms,
    })
}

/// The forecast worth warning about: the one where the observed rate would
/// reach the floor within `horizon_secs`. A horizon of 0 warns only once the
/// floor is crossed, which `disk_forecast` already excludes.
pub fn disk_warning(floor: u64, horizon_secs: u64, trend: &DiskTrend) -> Option<DiskForecast> {
    let forecast = disk_forecast(floor, trend)?;
    (forecast.hours_to_floor * 3_600.0 <= horizon_secs as f64).then_some(forecast)
}

/// Settings and live readings for resource.status.
pub fn report(settings: &Settings, host: &Host) -> Value {
    json!({"settings": settings, "host": host})
}

#[cfg(test)]
mod tests {
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
            disk_available_bytes: 100 * GIB,
            released_load,
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
    fn released_load_fades_with_the_one_minute_average_and_never_exceeds_it() {
        assert_eq!(released_load(10.0, 14.0, 0), 10.0);
        assert_eq!(released_load(10.0, 4.0, 0), 4.0);
        let minute = released_load(10.0, 14.0, 60_000);
        assert!((minute - 10.0 / std::f64::consts::E).abs() < 1e-9);
        assert!(released_load(10.0, 14.0, 600_000) < 0.001);
    }

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
        }
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
            demand(&settings, &host, true, None).unwrap().disk_bytes,
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
            Some(&estimate(&json!({"disk_bytes": 4 * GIB})).unwrap()),
        );
        declared.disk_available_bytes = 6 * GIB;
        assert!(admit(&settings, &host, 0, &[], &declared).is_ok());
        declared.disk_available_bytes = 6 * GIB - 1;
        assert!(admit(&settings, &host, 0, &[], &declared).is_err());
    }

    #[test]
    fn free_space_samples_are_bounded_and_say_how_fast_space_is_falling() {
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
        // The Mac's day: 36 GiB down to 8.6 GiB in nineteen hours.
        let end = 19 * 3_600_000;
        let last = 8_600 * 1024 * 1024;
        note_disk(&conn, "/data", last, end).unwrap();
        let trend = disk_trend(&conn, "/data").unwrap().expect("a rate");
        assert_eq!(trend.path, "/data");
        assert_eq!(trend.from_ms, 0);
        assert_eq!(trend.from_free_bytes, 36 * GIB);
        assert_eq!(trend.to_ms, end);
        assert_eq!(trend.to_free_bytes, last);
        let forecast = disk_forecast(2 * GIB, &trend).expect("a falling rate is read");
        let lost_per_hour = (36 * GIB - last) as f64 / GIB as f64 / (end as f64 / 3_600_000.0);
        assert!(
            (forecast.gib_per_hour - lost_per_hour).abs() < 1e-9,
            "{forecast:?}"
        );
        assert_eq!(forecast.span_ms, end);
        // Samples older than the retention window are dropped, so the table
        // stays bounded however long the ledger lives.
        note_disk(&conn, "/data", 30 * GIB, end + DISK_SAMPLE_KEEP_MS + 1).unwrap();
        assert_eq!(samples(&conn, "/data"), 1);
    }

    #[test]
    fn a_falling_rate_warns_before_the_floor_but_not_after_it() {
        let hour = 3_600_000;
        // The Mac a day earlier: 36 GiB free at dawn, on the way to 8.6 GiB,
        // with a 15 GiB floor for its iOS workload. Under the floor already,
        // which is the floor's own alarm rather than a forecast.
        let yesterday = trend_of(0, 36 * GIB, 19 * hour, 8_600 * 1024 * 1024);
        assert!(disk_warning(15 * GIB, 86_400, &yesterday).is_none());
        // Half a day in, still above the floor with hours to spare: the owner
        // hears about it while there is still time to act.
        let midday = trend_of(0, 36 * GIB, 6 * hour, 20 * GIB);
        let forecast = disk_warning(15 * GIB, 86_400, &midday).expect("a warning worth sending");
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
        assert!(disk_warning(15 * GIB, 86_400, &week).is_none());
        // Space recovered, so nothing is falling.
        let recovered = trend_of(0, 36 * GIB, 20 * hour, 40 * GIB);
        assert!(disk_warning(15 * GIB, 86_400, &recovered).is_none());
        // Falling, but too slowly to reach the floor within the horizon.
        let slow = trend_of(0, 36 * GIB, 20 * hour, 35 * GIB);
        assert!(disk_warning(15 * GIB, 86_400, &slow).is_none());
        assert!(disk_warning(15 * GIB, 86_400 * 30, &slow).is_some());
        // A horizon of zero warns only once the floor is crossed, which the
        // forecast itself excludes.
        assert!(disk_warning(15 * GIB, 0, &midday).is_none());
        // Two readings minutes apart do not describe a rate.
        let brief = trend_of(0, 36 * GIB, 60_000, 20 * GIB);
        assert!(disk_warning(15 * GIB, 86_400 * 30, &brief).is_none());
    }

    fn trend_of(from_ms: i64, from: u64, to_ms: i64, to: u64) -> DiskTrend {
        DiskTrend {
            path: "/data".into(),
            from_ms,
            from_free_bytes: from,
            to_ms,
            to_free_bytes: to,
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
}
