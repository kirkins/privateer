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
    let previous: Option<String> = registry
        .query_row(
            "SELECT home FROM desk_baseline_cursor WHERE domain=?1 AND source=?2",
            params![m.domain, source],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let next: Option<String> = registry
        .query_row(
            "SELECT home FROM (SELECT home FROM ships UNION SELECT home FROM desk_home_inventory)
         ORDER BY home<=COALESCE(?1,''),home LIMIT 1",
            [previous],
            |r| r.get(0),
        )
        .optional()?;
    let Some(path) = next else {
        return Ok(());
    };
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
            let (_fence, current) = super::super::desk_placement::intake_manifest(registry)?;
            ensure!(binding(&current)? == binding(&m)?, "stale_generation");
            Ok(_fence)
        },
    )?;
    Ok(())
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
        let published: bool = home.query_row(
            "SELECT EXISTS(SELECT 1 FROM mail_source_snapshot)",
            [],
            |r| r.get(0),
        )?;
        if published {
            let old: String = home.query_row(
                "SELECT binding FROM mail_source_snapshot WHERE only=1",
                [],
                |r| r.get(0),
            )?;
            ensure!(old == bound, "stale_generation");
            home::baseline_committed(registry, m, source)?;
            enrollment::reclaim_covered_deltas(&home)?;
            return Ok(());
        }
        enrollment::begin(&home, &bound)?;
        let sidecar = enrollment::open(path, true)?;
        let _sidecar_budget = transport::Budget::new(&sidecar, deadline)?;
        enrollment::init_sidecar(&sidecar)?;
        let frozen = enrollment::frozen(&sidecar, &bound)?;
        if frozen.is_none() {
            let attempted: Option<i64> = home.query_row(
                "SELECT attempted_ms FROM mail_source_enrollment WHERE only=1",
                [],
                |r| r.get(0),
            )?;
            if attempted
                .is_some_and(|at| now_ms >= at && now_ms.saturating_sub(at) < enrollment::RETRY_MS)
            {
                return Ok(());
            }
            home.execute(
                "UPDATE mail_source_enrollment SET attempted_ms=?1 WHERE only=1",
                [now_ms],
            )?;
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
        home::baseline_committed(registry, m, source)?;
        Ok(())
    })();
    // Remove interrupt handlers before recording a failure: an exhausted
    // snapshot budget must not suppress the durable diagnostic itself.
    drop(_home_budget);
    let _diagnostic_budget =
        transport::Budget::new(registry, Instant::now() + Duration::from_millis(5))?;
    record_failure(registry, m, source, &result)?;
    result
}

#[cfg(test)]
#[path = "baseline_tests.rs"]
mod tests;
