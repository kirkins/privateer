use anyhow::{Context, Result, ensure};
use std::cell::Cell;
use std::time::{Duration, Instant};

const CPU_LIMIT: Duration = Duration::from_millis(250);
const WALL_LIMIT: Duration = Duration::from_secs(2);

// Per-observation only: never shorten ordinary commands' writer wait policy.
// SQLite callbacks bound VM work cooperatively, not OS/filesystem I/O.
pub(crate) struct ObservationBudget<'a> {
    conn: &'a rusqlite::Connection,
    previous_busy_ms: u64,
    cpu_deadline: Duration,
    wall_deadline: Instant,
    checkpoints: Cell<u32>,
}

// Observations run synchronously on the connection's current thread. Charge
// that thread's work, not time descheduled on a loaded host (nor CPU used
// by another reader). A separate, generous elapsed backstop bounds stalled
// readers cooperatively without making short scheduler pauses exhaust CPU work.
#[derive(Clone, Copy)]
struct ClockReading {
    cpu: Duration,
    wall: Instant,
}

fn observation_clock() -> Result<ClockReading> {
    #[cfg(test)]
    if let Some(reading) = test_clock::override_reading() {
        return reading;
    }
    Ok(ClockReading {
        cpu: thread_cpu_time()?,
        wall: Instant::now(),
    })
}

/// The observation clock's wall reading, for callers that bound their own work
/// beside the budget. It reads the same (test-injectable) clock, so an
/// injected clock freezes every time bound, not only the budget's own.
pub(crate) fn observation_wall() -> Result<Instant> {
    Ok(observation_clock()?.wall)
}

/// The observation clock as tests inject it, crate-wide: frozen at install and
/// moved only by `FakeClock::advance`, so budget-bound tests stay deterministic
/// on a loaded runner and assume nothing about the passage of wall time.
#[cfg(test)]
pub(crate) mod test_clock {
    use super::ClockReading;
    use anyhow::{Result, ensure};
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    #[derive(Clone, Copy)]
    struct Injected {
        reading: ClockReading,
        failed: bool,
    }

    thread_local! {
        static CLOCK: Cell<Option<Injected>> = const { Cell::new(None) };
    }

    pub(super) fn override_reading() -> Option<Result<ClockReading>> {
        CLOCK.get().map(|clock| {
            ensure!(!clock.failed, "injected observation clock failure");
            Ok(clock.reading)
        })
    }

    pub(crate) struct FakeClock;

    impl FakeClock {
        pub(crate) fn install() -> Self {
            assert!(CLOCK.get().is_none());
            CLOCK.set(Some(Injected {
                reading: ClockReading {
                    cpu: Duration::ZERO,
                    wall: Instant::now(),
                },
                failed: false,
            }));
            Self
        }

        pub(crate) fn advance(cpu: Duration, wall: Duration) {
            let mut clock = CLOCK.get().unwrap();
            clock.reading.cpu += cpu;
            clock.reading.wall += wall;
            CLOCK.set(Some(clock));
        }

        pub(crate) fn fail() {
            let mut clock = CLOCK.get().unwrap();
            clock.failed = true;
            CLOCK.set(Some(clock));
        }
    }

    impl Drop for FakeClock {
        fn drop(&mut self) {
            CLOCK.set(None);
        }
    }
}

fn within_deadlines(cpu_deadline: Duration, wall_deadline: Instant) -> Result<bool> {
    let now = observation_clock()?;
    Ok(now.cpu < cpu_deadline && now.wall < wall_deadline)
}

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
        Self::with_deadline(conn, None)
    }

    pub(crate) fn with_deadline(
        conn: &'a rusqlite::Connection,
        deadline: Option<Instant>,
    ) -> Result<Self> {
        let now = observation_clock()?;
        let cpu_deadline = now.cpu + CPU_LIMIT;
        let wall_deadline =
            deadline.map_or(now.wall + WALL_LIMIT, |d| d.min(now.wall + WALL_LIMIT));
        let previous_busy_ms = conn.query_row("PRAGMA busy_timeout", [], |r| r.get(0))?;
        conn.busy_timeout(Duration::from_millis(25))?;
        let mut callbacks = 0_u32;
        conn.progress_handler(
            1000,
            Some(move || {
                callbacks += 1;
                callbacks >= 2000 || !within_deadlines(cpu_deadline, wall_deadline).unwrap_or(false)
            }),
        );
        Ok(Self {
            conn,
            previous_busy_ms,
            cpu_deadline,
            wall_deadline,
            checkpoints: Cell::new(0),
        })
    }

    pub(crate) fn wall_deadline(&self) -> Instant {
        self.wall_deadline
    }

    // VM progress callbacks may never fire across repeated small statements.
    // Bound Rust/statement work independently, across the whole observation.
    pub(crate) fn check(&self) -> Result<()> {
        let checkpoints = self.checkpoints.get() + 1;
        self.checkpoints.set(checkpoints);
        ensure!(
            checkpoints <= 4096 && within_deadlines(self.cpu_deadline, self.wall_deadline)?,
            "runtime observation work/CPU/wall budget exhausted"
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
