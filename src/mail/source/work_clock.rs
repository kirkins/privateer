//! Cooperative work clock. Production uses Instant; tests can charge each
//! checkpoint deterministically without sleeps or depending on runner uptime.
use std::time::Instant;

pub(crate) fn now() -> Instant {
    #[cfg(test)]
    if let Some(now) = SLOW.with(|clock| {
        let mut clock = clock.borrow_mut();
        clock.as_mut().map(|(now, step)| {
            let result = *now;
            *now += *step;
            result
        })
    }) {
        return now;
    }
    deadline_now()
}

/// Deadline/VM clock: share the subprocess freeze, but do not charge the
/// unit-test loop clock on SQLite's implementation-dependent VM checkpoints.
pub(crate) fn deadline_now() -> Instant {
    // Only feature-enabled fixture subprocesses can freeze cooperative work.
    // Normal builds do not read this variable or compile the frozen clock.
    #[cfg(feature = "test-intake-budget")]
    if let Some(now) = frozen_now(
        std::env::var("PRIVATEER_TEST_DESK_INTAKE_WORK_CLOCK")
            .ok()
            .as_deref(),
    ) {
        return now;
    }
    Instant::now()
}

#[cfg(feature = "test-intake-budget")]
fn frozen_now(setting: Option<&str>) -> Option<Instant> {
    static NOW: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    (setting == Some("frozen")).then(|| *NOW.get_or_init(Instant::now))
}

#[cfg(test)]
thread_local! {
    static SLOW: std::cell::RefCell<Option<(Instant, std::time::Duration)>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) struct SlowClock;
#[cfg(test)]
impl SlowClock {
    pub(crate) fn start(now: Instant, step: std::time::Duration) -> Self {
        SLOW.with(|clock| {
            assert!(clock.borrow().is_none(), "nested slow work clock");
            *clock.borrow_mut() = Some((now, step));
        });
        Self
    }
}
#[cfg(test)]
impl Drop for SlowClock {
    fn drop(&mut self) {
        SLOW.with(|clock| *clock.borrow_mut() = None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(not(feature = "test-intake-budget"))]
    fn default_build_uses_real_instant_only() {
        let before = Instant::now();
        let work = now();
        let after = Instant::now();
        assert!(before <= work && work <= after);
    }

    #[test]
    fn deadline_clock_does_not_charge_the_unit_test_loop_clock() {
        let start = Instant::now();
        let step = std::time::Duration::from_secs(3600);
        let _clock = SlowClock::start(start, step);
        let before = Instant::now();
        let work = deadline_now();
        let after = Instant::now();
        assert!(before <= work && work <= after);
        assert_eq!(now(), start);
        assert_eq!(now(), start + step);
    }

    #[test]
    #[cfg(feature = "test-intake-budget")]
    fn only_explicit_frozen_setting_selects_a_fixed_instant() {
        for setting in [None, Some(""), Some("real"), Some("invalid")] {
            assert!(frozen_now(setting).is_none());
        }
        let first = frozen_now(Some("frozen")).unwrap();
        assert_eq!(frozen_now(Some("frozen")), Some(first));
        // The deterministic unit-test clock still takes precedence; fixtures
        // never change the environment of the lib test process.
        let _clock = SlowClock::start(first, std::time::Duration::from_millis(1));
        assert_eq!(now(), first);
        assert_eq!(now(), first + std::time::Duration::from_millis(1));
    }
}
