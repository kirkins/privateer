//! Host-wide admission for resource-intensive jobs.
//!
//! Decisions use live machine readings (load average and available memory)
//! plus the estimates of jobs admitted within a recent ramp window, whose
//! use is not yet visible in those readings. Settings live in the shared
//! resource ledger, so every home on the host applies the same policy, and
//! anyone, agents included, may tune them to the person's wishes: use the
//! whole machine, or leave room for their own work.
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension};
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
}

/// A resolved estimate recorded with an admitted reservation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Demand {
    pub cpus: f64,
    pub memory_bytes: u64,
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
    /// Default estimate for heavy jobs; `cpus` absent means every CPU.
    pub heavy_estimate: Estimate,
    /// How long an admitted job's estimate counts before its use is assumed
    /// to show in live readings (the load average trails by about a minute).
    pub ramp_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_heavy_jobs: None,
            max_load_per_cpu: 1.0,
            min_available_memory_bytes: GIB,
            heavy_estimate: Estimate {
                cpus: None,
                memory_bytes: Some(2 * GIB),
            },
            ramp_secs: 60,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Host {
    pub cpus: f64,
    pub load: f64,
    pub memory_available_bytes: u64,
    pub memory_total_bytes: u64,
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
pub struct Request<'a> {
    pub heavy: bool,
    pub home: &'a std::path::Path,
    pub charter: &'a str,
    pub charter_cap: Option<u64>,
    pub demand: Option<Demand>,
}

pub fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
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
        .context("estimate must be an object with cpus and/or memory_bytes")?;
    let mut estimate = Estimate::default();
    for (key, value) in object {
        match key.as_str() {
            "cpus" if value.is_null() => {}
            "cpus" => estimate.cpus = Some(positive(value, "estimate.cpus", 1024.0)?),
            "memory_bytes" if value.is_null() => {}
            "memory_bytes" => estimate.memory_bytes = Some(bytes(value, "estimate.memory_bytes")?),
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
            "heavy_estimate" => {
                let requested = estimate(value)?;
                settings.heavy_estimate = Estimate {
                    cpus: requested.cpus,
                    memory_bytes: requested
                        .memory_bytes
                        .or(Settings::default().heavy_estimate.memory_bytes),
                };
            }
            "ramp_secs" => {
                settings.ramp_secs = value
                    .as_u64()
                    .filter(|v| *v <= 3600)
                    .context("ramp_secs must be 0..3600")?;
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
        }
    } else {
        Demand::default()
    };
    Some(Demand {
        cpus: requested.cpus.unwrap_or(fallback.cpus),
        memory_bytes: requested.memory_bytes.unwrap_or(fallback.memory_bytes),
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
    let Some(demand) = request.demand else {
        return Ok(());
    };
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
    let per_cpu = (host.load + recent_cpus) / host.cpus;
    refuse_unless!(
        per_cpu < settings.max_load_per_cpu,
        "admission refused: CPU demand {per_cpu:.2} per CPU is at the limit {} (load {:.2}, {recent_cpus:.1} CPUs recently admitted, {} CPUs); retry later or tune resource.configure",
        settings.max_load_per_cpu,
        host.load,
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
    Ok(Host {
        cpus,
        load: loads[0],
        memory_available_bytes,
        memory_total_bytes,
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

/// Settings and live readings for resource.status.
pub fn report(settings: &Settings, host: &Host) -> Value {
    json!({"settings": settings, "host": host})
}
