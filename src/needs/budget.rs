use anyhow::{Context, Result, ensure};
use std::cell::Cell;
use std::time::Duration;

// Per-observation only: never shorten ordinary commands' writer wait policy.
// SQLite callbacks bound VM work cooperatively, not OS/filesystem I/O.
pub(crate) struct ObservationBudget<'a> {
    conn: &'a rusqlite::Connection,
    previous_busy_ms: u64,
    cpu_deadline: Duration,
    checkpoints: Cell<u32>,
}

// Observations run synchronously on the connection's current thread. Charge
// only that thread's work, not time descheduled on a loaded host (nor CPU used
// by another reader). An Instant deadline discarded even tiny valid needs
// observations after a scheduler pause. Lock waits remain bounded separately.
fn thread_cpu_time() -> Result<Duration> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: time is a valid writable timespec; the clock names this thread.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } != 0 {
        return Err(std::io::Error::last_os_error()).context("read observation thread CPU clock");
    }
    Ok(Duration::new(
        time.tv_sec.try_into()?,
        time.tv_nsec.try_into()?,
    ))
}

impl<'a> ObservationBudget<'a> {
    pub(crate) fn new(conn: &'a rusqlite::Connection) -> Result<Self> {
        Self::with_cpu_limit(conn, Duration::from_millis(250))
    }

    fn with_cpu_limit(conn: &'a rusqlite::Connection, limit: Duration) -> Result<Self> {
        let cpu_deadline = thread_cpu_time()? + limit;
        let previous_busy_ms = conn.query_row("PRAGMA busy_timeout", [], |r| r.get(0))?;
        conn.busy_timeout(Duration::from_millis(25))?;
        let mut callbacks = 0_u32;
        conn.progress_handler(
            1000,
            Some(move || {
                callbacks += 1;
                callbacks >= 2000 || !thread_cpu_time().is_ok_and(|now| now < cpu_deadline)
            }),
        );
        Ok(Self {
            conn,
            previous_busy_ms,
            cpu_deadline,
            checkpoints: Cell::new(0),
        })
    }

    // VM progress callbacks may never fire across repeated small statements.
    // Bound Rust/statement work independently, across the whole observation.
    pub(crate) fn check(&self) -> Result<()> {
        let checkpoints = self.checkpoints.get() + 1;
        self.checkpoints.set(checkpoints);
        ensure!(
            checkpoints <= 4096 && thread_cpu_time()? < self.cpu_deadline,
            "runtime observation work/CPU budget exhausted"
        );
        Ok(())
    }
}

impl Drop for ObservationBudget<'_> {
    fn drop(&mut self) {
        self.conn.progress_handler(0, None::<fn() -> bool>);
        // Restore even on unavailable/error/early return. Store owns this
        // connection; it has no persistent progress handler to replace.
        let _ = self
            .conn
            .busy_timeout(Duration::from_millis(self.previous_busy_ms));
    }
}

#[cfg(test)]
#[path = "budget_tests.rs"]
mod tests;
