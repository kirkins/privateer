//! One bounded idle enrollment unit, never called by collect/push/gateway.
//! Resident dispatchers launch it on a separate thread; `serve --once` runs it
//! only after completing the mail/local-work pass. Processes serialize on a
//! different, nonblocking lock from transport and placement.
use super::*;
use crate::mail::source::{self, enrollment};
use std::{
    fs,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const WORK: Duration = Duration::from_secs(5);
static RUNNING: AtomicBool = AtomicBool::new(false);
struct Running;
impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::Release);
    }
}

pub(in crate::machines) fn idle(resident: bool) {
    if RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let run = || {
        let _running = Running;
        let _ = (|| -> Result<()> {
            let path = crate::fleet::registry_path()?;
            if !path.is_file() {
                return Ok(());
            }
            let lock = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .mode(0o600)
                .open(path.with_extension("desk-baseline.lock"))?;
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Ok(());
            }
            let registry = crate::fleet::registry()?;
            let deadline = Instant::now() + WORK;
            let _budget = transport::Budget::new(&registry, deadline)?;
            let result = step(&registry, deadline);
            if let Err(error) = &result {
                transport::audit(&registry, "local", "baseline", error);
            }
            result
        })();
    };
    if resident {
        if std::thread::Builder::new()
            .name("desk-baseline".into())
            .spawn(run)
            .is_err()
        {
            RUNNING.store(false, Ordering::Release);
        }
    } else {
        run();
    }
}

fn step(registry: &Connection, deadline: Instant) -> Result<()> {
    if super::super::desk_placement::routing(registry)?.is_none() {
        return Ok(());
    }
    let (fence, m) = super::super::desk_placement::intake_manifest(registry)?;
    let source = crate::door::reader_link::machine_id_in(registry)?;
    participant(&m, &source)?;
    drop(fence); // No placement/contact lock is held over snapshot work.
    let next = next_work(registry, &m.domain, &source, deadline)?;
    let enrollment = (|| -> Result<()> {
        if let Some(path) = next {
            registry.execute(
                "INSERT INTO desk_baseline_cursor(domain,source,home) VALUES(?1,?2,?3)
        ON CONFLICT(domain,source) DO UPDATE SET home=excluded.home WHERE home<>excluded.home",
                params![m.domain, source, path],
            )?;
            enroll_home(
                Path::new(&path),
                registry,
                &m,
                &source,
                deadline,
                crate::store::Store::now_ms(),
                || {
                    // Recheck the fenced active binding immediately before publication.
                    let (_fence, current) =
                        super::super::desk_placement::intake_manifest(registry)?;
                    ensure!(binding(&current)? == binding(&m)?, "stale_generation");
                    Ok(_fence)
                },
            )?;
        }
        Ok(())
    })();
    // Local-owner ingestion was absent from remote collect frames. Run its
    // ordinary bounded projection protocol off-contact, never rerouting mail.
    let local = (|| -> Result<()> {
        if source == m.owner_core {
            let (fence, current) = super::super::desk_placement::intake_manifest(registry)?;
            ensure!(binding(&current)? == binding(&m)?, "stale_generation");
            let phase = deadline.min(Instant::now() + Duration::from_millis(75));
            let _budget = transport::Budget::new(registry, phase)?;
            transport::local(registry, current, source.clone(), fence, phase)?;
        }
        Ok(())
    })();
    // A dated vector is retained independently of fresh contact evidence.
    // Serialize with registry imports, not with mail writers or network I/O.
    let reconciliation = (|| -> Result<()> {
        let _serial = transport::serial()?;
        let (_fence, current) = super::super::desk_placement::intake_manifest(registry)?;
        ensure!(binding(&current)? == binding(&m)?, "stale_generation");
        reconcile::save(registry, &m, &source, deadline)
    })();
    // A failed/skipped home must still appear in the census. Preserve the
    // original work failure even when optional reconciliation also fails.
    enrollment.and(local).and(reconciliation)
}

/// Read-only scan: a completely published, reclaimed inventory must not rotate
/// a persistent cursor (and fsync the registry) on every resident idle tick.
fn next_work(
    registry: &Connection,
    domain: &str,
    source: &str,
    deadline: Instant,
) -> Result<Option<String>> {
    let previous: Option<String> = registry
        .query_row(
            "SELECT home FROM desk_baseline_cursor WHERE domain=?1 AND source=?2",
            params![domain, source],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let mut stmt = registry.prepare("SELECT home FROM (SELECT home FROM ships UNION SELECT home FROM desk_home_inventory) ORDER BY home<=COALESCE(?1,''),home LIMIT 10001")?;
    let paths = stmt.query_map([previous], |r| r.get::<_, String>(0))?;
    for path in paths {
        ensure!(
            Instant::now() < deadline,
            "backpressure: idle inventory budget"
        );
        let path = path?;
        let pending: bool = registry.query_row("SELECT EXISTS(SELECT 1 FROM desk_baseline_home WHERE domain=?1 AND source=?2 AND home=?3 AND state<>'published')", params![domain,source,path], |r| r.get(0))?;
        if pending {
            return Ok(Some(path));
        }
        let quiet = (|| -> Result<bool> {
            crate::store::reject_symlinks(Path::new(&path))?;
            let database = Path::new(&path).join("privateer.sqlite");
            crate::store::reject_symlinks(&database)?;
            let h = Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            h.busy_timeout(Duration::ZERO)?;
            Ok(h.query_row("SELECT EXISTS(SELECT 1 FROM mail_source_snapshot WHERE upper_seq>=0) AND NOT EXISTS(SELECT 1 FROM mail_source_outbox WHERE seq<=(SELECT upper_seq FROM mail_source_snapshot WHERE only=1))", [], |r| r.get(0))?)
        })().unwrap_or(false);
        if !quiet {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// `fence` must retain the current placement fence through publication. Tests
/// inject a unit guard without configuring any real machine or route.
pub(super) fn enroll_home<G>(
    path: &Path,
    registry: &Connection,
    m: &Manifest,
    source: &str,
    deadline: Instant,
    now_ms: i64,
    fence: impl FnOnce() -> Result<G>,
) -> Result<()> {
    participant(m, source)?;
    crate::store::reject_symlinks(path)?;
    let database = path.join("privateer.sqlite");
    crate::store::reject_symlinks(&database)?;
    let home = Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let _home_budget = transport::Budget::new(&home, deadline)?;
    let result = (|| -> Result<()> {
        // Additive upgrade only here/normal writable startup, not the contact.
        source::init(&home)?;
        {
            // Existing home-index setup remains a short, zero-wait optional
            // write, not a five-second home writer lock.
            let _setup_budget = transport::Budget::new(
                &home,
                deadline.min(Instant::now() + Duration::from_millis(75)),
            )?;
            source::init_intake(&home)?;
        }
        let _work_budget = transport::Budget::new(&home, deadline)?;
        let bound = format!("{}:{source}", binding(m)?);
        if enrollment::published(&home)? {
            let old: String = home.query_row(
                "SELECT binding FROM mail_source_snapshot WHERE only=1",
                [],
                |r| r.get(0),
            )?;
            ensure!(old == bound, "stale_generation");
            enrollment::published_nonce(&home, &bound)?;
            published_home(registry, m, source, path)?;
            enrollment::reclaim_covered_deltas(&home)?;
            return Ok(());
        }
        // A crash between activation and freeze must not keep its nonce across
        // units (including backoff). Only an already frozen sidecar can resume.
        // Otherwise deactivate before cleanup/backoff and capture anew later.
        let active: bool = home.query_row(
            "SELECT EXISTS(SELECT 1 FROM mail_source_enrollment)",
            [],
            |r| r.get(0),
        )?;
        let resume = if active && enrollment::path(path).is_file() {
            let instance = enrollment::nonce(&home, &bound)?;
            let sidecar = enrollment::open(path, false)?;
            enrollment::frozen(&sidecar, &bound, &instance)?.is_some()
        } else {
            false
        };
        if !resume {
            enrollment::deactivate(&home)?;
        }
        // Reclaim at most 50 observations before beginning a fresh instance.
        if !enrollment::clean_inactive(&home)? {
            return Ok(());
        }
        let attempted: Option<i64> = registry.query_row("SELECT attempted_ms FROM desk_baseline_home WHERE domain=?1 AND source=?2 AND home=?3 AND state<>'published'", params![m.domain,source,path.to_string_lossy()], |r| r.get(0)).optional()?;
        if attempted
            .is_some_and(|at| now_ms >= at && now_ms.saturating_sub(at) < enrollment::RETRY_MS)
        {
            return Ok(());
        }
        registry.execute("INSERT INTO desk_baseline_home(domain,source,home,state,attempted_ms) VALUES(?1,?2,?3,'pending',?4) ON CONFLICT(domain,source,home) DO UPDATE SET attempted_ms=excluded.attempted_ms", params![m.domain,source,path.to_string_lossy(),now_ms])?;
        if !resume {
            enrollment::begin(&home, &bound)?;
        }
        let instance = enrollment::nonce(&home, &bound)?;
        let sidecar = enrollment::open(path, true)?;
        let _sidecar_budget = transport::Budget::new(&sidecar, deadline)?;
        enrollment::init_sidecar(&sidecar)?;
        let frozen = enrollment::frozen(&sidecar, &bound, &instance)?;
        if frozen.is_none() {
            enrollment::freeze(&home, &sidecar, &bound, deadline).map_err(|e| {
                if retention::interrupted_or_busy(&e) && Instant::now() >= deadline {
                    e.context("baseline_too_large: snapshot work budget")
                } else {
                    e
                }
            })?;
        }
        let _fence = fence()?;
        enrollment::publish(&home, &sidecar, &bound)?;
        enrollment::reclaim_covered_deltas(&home)?;
        published_home(registry, m, source, path)?;
        Ok(())
    })();
    // Remove interrupt handlers before recording a failure: an exhausted
    // snapshot budget must not suppress the durable diagnostic itself.
    drop(_home_budget);
    let _diagnostic_budget =
        transport::Budget::new(registry, Instant::now() + Duration::from_millis(75)).ok();
    if let Err(e) = &result {
        // Deactivate before diagnostics: registry contention must not leave
        // failed pre-publication tracking enabled. Cleanup has its own budget.
        let _ = (|| -> Result<()> {
            let _cleanup =
                transport::Budget::new(&home, Instant::now() + Duration::from_millis(75))?;
            if !source::missing(
                &home,
                &[
                    "mail_source_enrollment",
                    "mail_source_snapshot",
                    "mail_source_outbox",
                ],
            )? {
                enrollment::deactivate(&home)?;
                enrollment::clean_inactive(&home)?;
            }
            Ok(())
        })();
        // Preserve a specific obstacle until this same home publishes.
        let state = if e.to_string().starts_with("baseline_too_large") {
            "baseline_too_large"
        } else {
            "pending"
        };
        let _ = registry.execute("INSERT INTO desk_baseline_home(domain,source,home,state,attempted_ms) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(domain,source,home) DO UPDATE SET state=CASE WHEN state='baseline_too_large' THEN state ELSE excluded.state END,attempted_ms=excluded.attempted_ms", params![m.domain,source,path.to_string_lossy(),state,now_ms]);
    }
    // The original failure remains the outcome even if optional diagnostics
    // also contend or fail. Always try the source-level diagnostic.
    let _ = record_failure(registry, m, source, &result);
    result
}

fn published_home(registry: &Connection, m: &Manifest, source: &str, path: &Path) -> Result<()> {
    let pending: bool = registry.query_row("SELECT EXISTS(SELECT 1 FROM desk_baseline_home WHERE domain=?1 AND source=?2 AND home=?3 AND state<>'published')", params![m.domain,source,path.to_string_lossy()], |r| r.get(0))?;
    if pending {
        registry.execute("UPDATE desk_baseline_home SET state='published' WHERE domain=?1 AND source=?2 AND home=?3", params![m.domain,source,path.to_string_lossy()])?;
    }
    home::baseline_committed(registry, m, source)
}

#[cfg(test)]
#[path = "baseline_tests.rs"]
mod tests;
