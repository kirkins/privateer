//! Desk receipt continuity across CLI/gateway processes, never across boots.
//! Unsupported/unavailable platform clocks fail closed without refusing pages.
use super::Clock;
use std::sync::OnceLock;

#[cfg(feature = "test-intake-budget")]
#[derive(serde::Deserialize)]
struct Fixture {
    boot_id: String,
    boot_ms: i64,
    wall_ms: i64,
}
#[cfg(feature = "test-intake-budget")]
fn fixture() -> Option<Fixture> {
    serde_json::from_str(&std::env::var("PRIVATEER_TEST_DESK_BOOT_CLOCK").ok()?).ok()
}

pub(super) fn process_clock() -> Clock<'static> {
    #[cfg(feature = "test-intake-budget")]
    if let Ok(raw) = std::env::var("PRIVATEER_TEST_DESK_INTAKE_CLOCK")
        && let Ok([wall_ms, tick_ms]) = serde_json::from_str::<[i64; 2]>(&raw)
    {
        return Clock {
            session: "alpha-test-continuity",
            wall_ms,
            tick_ms,
        };
    }
    // Cache only the boot identity, not a process-relative epoch. The injected
    // seam replaces the platform inputs and follows the same construction.
    static SESSION: OnceLock<Option<String>> = OnceLock::new();
    let session = SESSION.get_or_init(|| {
        #[cfg(feature = "test-intake-budget")]
        if let Some(f) = fixture() {
            return identity(&f.boot_id);
        }
        platform_identity()
    });
    #[cfg(feature = "test-intake-budget")]
    if let Some(f) = fixture() {
        return reading(session.as_deref(), f.wall_ms, Some(f.boot_ms));
    }
    reading(
        session.as_deref(),
        crate::store::Store::now_ms(),
        platform_tick(),
    )
}

fn identity(raw: &str) -> Option<String> {
    let id = raw.trim();
    (!id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_graphic()))
        .then(|| format!("boot-v1:{id}"))
}
fn reading(session: Option<&str>, wall_ms: i64, tick: Option<i64>) -> Clock<'_> {
    match (session, tick.filter(|t| *t >= 0)) {
        (Some(session), Some(tick_ms)) => Clock {
            session,
            wall_ms,
            tick_ms,
        },
        // Empty session is explicitly ineligible in both freshness evaluators.
        // Keep the page acceptance clock valid so unavailable optional clock
        // evidence cannot starve durable intake progress.
        _ => Clock {
            session: "",
            wall_ms,
            tick_ms: 0,
        },
    }
}
#[cfg(target_os = "linux")]
fn platform_identity() -> Option<String> {
    identity(&std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?)
}
#[cfg(target_os = "macos")]
fn platform_identity() -> Option<String> {
    let mut bytes = [0u8; 128];
    let mut len = bytes.len();
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            bytes.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len > bytes.len() {
        return None;
    }
    identity(
        std::str::from_utf8(&bytes[..len])
            .ok()?
            .trim_end_matches('\0'),
    )
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_identity() -> Option<String> {
    None
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn platform_tick() -> Option<i64> {
    #[cfg(target_os = "linux")]
    let clock = libc::CLOCK_BOOTTIME; // Includes suspend.
    #[cfg(target_os = "macos")]
    let clock = libc::CLOCK_MONOTONIC_RAW; // Darwin continuous raw clock.
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(clock, &mut ts) } != 0 {
        return None;
    }
    milliseconds(ts.tv_sec as i128, ts.tv_nsec as i128)
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_tick() -> Option<i64> {
    None
}

#[cfg(any(test, target_os = "linux", target_os = "macos"))]
fn milliseconds(seconds: i128, nanos: i128) -> Option<i64> {
    if seconds < 0 || !(0..1_000_000_000).contains(&nanos) {
        return None;
    }
    i64::try_from(seconds.checked_mul(1000)?.checked_add(nanos / 1_000_000)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unavailable_boot_inputs_fail_closed_without_invalid_page_clocks() {
        for (session, tick) in [
            (None, Some(10)),
            (Some("alpha"), None),
            (Some("alpha"), Some(-1)),
        ] {
            let c = reading(session, 10000, tick);
            assert!(c.session.is_empty());
            assert_eq!(c.tick_ms, 0);
        }
        assert_eq!(identity(" alpha\n").as_deref(), Some("boot-v1:alpha"));
        assert!(identity("").is_none());
        assert!(identity("alpha beta").is_none());
        assert_eq!(milliseconds(7, 999_999_999), Some(7999));
        for (sec, ns) in [(-1, 0), (0, -1), (0, 1_000_000_000), (i128::MAX, 0)] {
            assert_eq!(milliseconds(sec, ns), None);
        }
    }
}
