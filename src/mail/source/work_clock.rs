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
    Instant::now()
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
