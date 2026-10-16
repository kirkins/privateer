//! Model routes: backing off a provider that is throttling.
//!
//! Pi retries within a run, but once a run gives up on a throttled provider
//! (in this fleet, `429 upstream_unavailable`), launching the next crew agent
//! or errand on the same route only fails again. After each harness job ends,
//! the dispatcher reads how its last reply ended; a run that failed on a
//! throttle backs its route off, doubling from 30 seconds to 15 minutes, and a
//! later success clears it. While a route is backed off, new crew and errands
//! on it are refused and headless wakes on it wait. The record lives in the
//! host's ledger, so every home sees the same provider state. Live hosted
//! sessions are the harness's own to retry.
//!
//! Native adapters do not all use Pi's `message_end` shape. OpenCode V1
//! reports a failed turn as a final `type:"error"` event (`error.data.message`)
//! rather than an assistant `message_end`, so those errors are read too. A
//! free-allowance exhaustion backs the route off until the reset it states
//! (or about a day when it says nothing); an ordinary throttle keeps the short
//! doubling window.
use crate::store::Store;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

const FIRST_BACKOFF_MS: i64 = 30_000;
const MAX_BACKOFF_MS: i64 = 15 * 60_000;
const FREE_ALLOWANCE_BACKOFF_MS: i64 = 24 * 60 * 60_000;
const CURSOR: &str = "routes_scanned_ms";

pub fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS model_routes(route TEXT PRIMARY KEY, failures INTEGER NOT NULL,
           until_ms INTEGER NOT NULL, last_error TEXT, updated_ms INTEGER NOT NULL);",
    )?;
    Ok(())
}

/// `harness/provider/model`, from a launch's manifest and options.
pub fn route_of(manifest: &str, options: &Value) -> Option<String> {
    let harness = crate::bundled::harness(manifest)?;
    Some(format!(
        "{harness}/{}/{}",
        options["provider"].as_str().unwrap_or("-"),
        options["model"].as_str().unwrap_or("-")
    ))
}

/// The same route, read back from a harness job's argv
/// (`native_launch.py HARNESS BINARY … --provider P --model M …`).
fn route_of_argv(command: &[Value]) -> Option<String> {
    let args: Vec<&str> = command.iter().filter_map(Value::as_str).collect();
    let at = args.iter().position(|a| a.ends_with("native_launch.py"))?;
    let harness = args.get(at + 1)?;
    let flag = |names: &[&str]| {
        args.windows(2)
            .find(|w| names.contains(&w[0]))
            .map(|w| w[1])
            .unwrap_or("-")
    };
    Some(format!(
        "{harness}/{}/{}",
        flag(&["--provider"]),
        flag(&["--model", "-m"])
    ))
}

/// A provider throttle in how the run's last reply ended, if any.
#[derive(Debug, PartialEq, Eq)]
struct RouteFailure {
    error: String,
    reset_ms: Option<i64>,
    free_allowance: bool,
}

fn reset_delay_ms(error: &str, now: i64) -> Option<i64> {
    let lower = error.to_ascii_lowercase();
    for marker in [
        "retry-after",
        "retry_after",
        "reset_at",
        "reset-at",
        "resets at",
    ] {
        let Some(at) = lower.find(marker) else {
            continue;
        };
        let tail = &lower[at + marker.len()..];
        if let Some(absolute) = iso_timestamp_ms(tail)
            && absolute > now
        {
            return Some(absolute - now);
        }
        let digits = tail.trim_start_matches(|c: char| !c.is_ascii_digit());
        let end = digits
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(digits.len());
        if let Ok(value) = digits[..end].parse::<i64>() {
            let absolute = if value > 1_000_000_000 {
                value.saturating_mul(1000)
            } else {
                now.saturating_add(value.saturating_mul(1000))
            };
            if absolute > now {
                return Some(absolute - now);
            }
        }
    }
    for (unit, multiplier) in [
        ("hour", 3_600_000_i64),
        ("hr", 3_600_000),
        ("day", 86_400_000),
        ("minute", 60_000),
        ("min", 60_000),
        ("second", 1000),
    ] {
        for prefix in ["resets in ", "reset in ", "try again in "] {
            if let Some(at) = lower.find(prefix) {
                let tail = lower[at + prefix.len()..].trim_start();
                let end = tail
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(tail.len());
                if end > 0
                    && tail[end..].trim_start().starts_with(unit)
                    && let Ok(value) = tail[..end].parse::<i64>()
                    && value > 0
                {
                    return Some(value.saturating_mul(multiplier));
                }
            }
        }
    }
    None
}

fn iso_timestamp_ms(value: &str) -> Option<i64> {
    let value = value.trim_start_matches(|c: char| !c.is_ascii_digit());
    let (date, time) = value
        .split_once('T')
        .or_else(|| value.split_once('t'))
        .or_else(|| value.split_once(' '))?;
    let mut date = date.split('-');
    let year = date.next()?.parse::<i64>().ok()?;
    let month = date.next()?.parse::<i64>().ok()?;
    let day = date.next()?.parse::<i64>().ok()?;
    let time = time.trim_end_matches(['Z', 'z']);
    let (clock, offset) = time
        .find(['+', '-'])
        .map(|at| (&time[..at], Some(&time[at..])))
        .unwrap_or((time, None));
    let mut clock = clock.split(':');
    let hour = clock.next()?.parse::<i64>().ok()?;
    let minute = clock.next()?.parse::<i64>().ok()?;
    let second = clock.next()?.split('.').next()?.parse::<i64>().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let adjusted_year = year - if month <= 2 { 1 } else { 0 };
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let mut seconds = days * 86_400 + hour * 3600 + minute * 60 + second;
    if let Some(offset) = offset {
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let mut parts = offset[1..].split(':');
        let offset_hour = parts.next()?.parse::<i64>().ok()?;
        let offset_minute = parts.next()?.parse::<i64>().ok()?;
        seconds -= sign * (offset_hour * 3600 + offset_minute * 60);
    }
    Some(seconds * 1000)
}

fn classify(log: &str) -> Option<RouteFailure> {
    let mut last = None;
    let mut native_error = None;
    for event in crate::reply::events(log) {
        if event["type"] == "message_end" && event["message"]["role"] == "assistant" {
            last = Some(event["message"].clone());
        } else if event["type"] == "error" {
            // Native adapters (OpenCode V1) report a failed turn as a final
            // error event instead of Pi's assistant message_end.
            let text = event["error"]["data"]["message"]
                .as_str()
                .or_else(|| event["error"]["message"].as_str())
                .or_else(|| event["error"].as_str())
                .map(str::to_owned);
            if let Some(text) = text.filter(|text| !text.is_empty()) {
                native_error = Some(text);
            }
        }
    }
    let error = if let Some(native) = native_error {
        native
    } else {
        let message = last?;
        if message["stopReason"] != "error" {
            return None;
        }
        message["errorMessage"].as_str().unwrap_or("").to_owned()
    };
    let lower = error.to_ascii_lowercase();
    // Only a stated free-allowance message earns the long, reset-aware
    // backoff; a paid quota or usage error still throttles on the short one.
    let free_allowance = lower.contains("free")
        && [
            "allowance",
            "usage limit",
            "quota",
            "credits",
            "limit exceeded",
        ]
        .iter()
        .any(|sign| lower.contains(sign));
    let throttled = free_allowance
        || ["429", "503", "529"]
            .iter()
            .any(|code| lower.starts_with(code))
        || [
            "rate limit",
            "rate_limit",
            "overloaded",
            "upstream_unavailable",
            "quota",
            "timed out",
        ]
        .iter()
        .any(|sign| lower.contains(sign));
    throttled.then(|| RouteFailure {
        error: error.chars().take(200).collect(),
        reset_ms: reset_delay_ms(&error, Store::now_ms()),
        free_allowance,
    })
}

/// A provider throttle in how the run's last reply ended, if any.
pub fn throttle(log: &str) -> Option<String> {
    classify(log).map(|failure| failure.error)
}

/// When a route may be used again, and why it may not now.
pub fn blocked(conn: &Connection, route: &str) -> Result<Option<(i64, String)>> {
    Ok(conn
        .query_row(
            "SELECT until_ms, COALESCE(last_error,'') FROM model_routes WHERE route=?1 AND until_ms > ?2",
            params![route, Store::now_ms()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// A refusal message for a backed-off route, or None when it is clear.
pub fn refusal(store: &Store, route: &str) -> Result<Option<String>> {
    let conn = crate::jobs::host_ledger(store)?;
    Ok(blocked(&conn, route)?.map(|(until, error)| {
        let secs = ((until - Store::now_ms()) / 1000).max(1);
        format!(
            "model route {route} is backing off for {secs}s after the provider throttled ({error}); \
             new crew and errands on it wait. Record the wait (note.pin key waiting) and continue \
             once it clears"
        )
    }))
}

fn record(conn: &Connection, route: &str, failure: Option<&RouteFailure>) -> Result<()> {
    let now = Store::now_ms();
    match failure {
        Some(failure) => {
            let failures: i64 = conn
                .query_row(
                    "SELECT failures FROM model_routes WHERE route=?1",
                    [route],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0)
                + 1;
            let backoff = if failure.free_allowance {
                failure.reset_ms.unwrap_or(FREE_ALLOWANCE_BACKOFF_MS)
            } else {
                FIRST_BACKOFF_MS
                    .saturating_mul(1i64 << (failures - 1).min(10))
                    .min(MAX_BACKOFF_MS)
            };
            conn.execute(
                "INSERT INTO model_routes(route, failures, until_ms, last_error, updated_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(route) DO UPDATE SET failures=excluded.failures,
                   until_ms=excluded.until_ms, last_error=excluded.last_error,
                   updated_ms=excluded.updated_ms",
                params![route, failures, now + backoff, failure.error, now],
            )?;
        }
        None => {
            conn.execute("DELETE FROM model_routes WHERE route=?1", [route])?;
        }
    }
    Ok(())
}

/// Read the harness jobs that ended since the last pass and back off or clear
/// their routes. Called by each dispatcher pass; no model call.
pub fn scan(store: &Store) -> Result<Vec<Value>> {
    let since: i64 = store
        .connection()
        .query_row("SELECT value FROM metadata WHERE key=?1", [CURSOR], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(Store::now_ms);
    let mut statement = store.connection().prepare(
        "SELECT record FROM jobs WHERE json_extract(record,'$.action')='spawn'
           AND json_extract(record,'$.finished_ms') > ?1
         ORDER BY json_extract(record,'$.finished_ms')",
    )?;
    let jobs: Vec<Value> = statement
        .query_map([since], |r| r.get::<_, String>(0))?
        .filter_map(|raw| raw.ok().and_then(|raw| serde_json::from_str(&raw).ok()))
        .collect();
    drop(statement);
    let mut through = since;
    let mut seen = vec![];
    let conn = if jobs.is_empty() {
        None
    } else {
        Some(crate::jobs::host_ledger(store)?)
    };
    for job in jobs {
        through = through.max(job["finished_ms"].as_i64().unwrap_or(through));
        let route = job["model_route"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| job["command"].as_array().and_then(|c| route_of_argv(c)));
        let Some(route) = route else {
            continue;
        };
        let status = job["status"].as_str().unwrap_or("");
        let throttled = if status == "succeeded" {
            None
        } else {
            let log = store
                .home()
                .join("jobs")
                .join(job["id"].as_str().unwrap_or(""))
                .join(job["incarnation"].as_str().unwrap_or(""))
                .join("stdout.log");
            match classify(&std::fs::read_to_string(log).unwrap_or_default()) {
                Some(error) => Some(error),
                // A failure for any other reason says nothing about the route.
                None => continue,
            }
        };
        if let Some(conn) = &conn {
            record(conn, &route, throttled.as_ref())?;
        }
        seen.push(json!({"route": route, "throttled": throttled.is_some()}));
    }
    store.connection().execute(
        "INSERT INTO metadata(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![CURSOR, through.to_string()],
    )?;
    Ok(seen)
}

/// Backed-off routes, for status views.
pub fn list(conn: &Connection) -> Result<Vec<Value>> {
    let mut statement = conn.prepare(
        "SELECT route, failures, until_ms, last_error FROM model_routes WHERE until_ms > ?1 ORDER BY route",
    )?;
    let rows = statement.query_map([Store::now_ms()], |r| {
        Ok(
            json!({"route": r.get::<_, String>(0)?, "failures": r.get::<_, i64>(1)?,
            "until_ms": r.get::<_, i64>(2)?, "last_error": r.get::<_, Option<String>>(3)?}),
        )
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error_log(message: &str) -> String {
        serde_json::to_string(&json!({"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":message}})).unwrap()
    }

    #[test]
    fn free_allowance_uses_reset_hint_or_full_allowance_window() {
        let reset = classify(&error_log("Free usage limit exceeded; retry-after: 7200")).unwrap();
        assert!(reset.free_allowance);
        assert_eq!(reset.reset_ms, Some(7_200_000));
        let fallback = classify(&error_log("Free allowance exhausted")).unwrap();
        assert!(fallback.free_allowance);
        assert_eq!(fallback.reset_ms, None);

        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        let now = Store::now_ms();
        record(&conn, "opencode/-/space-bunny-free", Some(&reset)).unwrap();
        let until: i64 = conn
            .query_row("SELECT until_ms FROM model_routes", [], |r| r.get(0))
            .unwrap();
        assert!((until - now - 7_200_000).abs() < 100);
        record(&conn, "opencode/-/space-bunny-free", Some(&fallback)).unwrap();
        let until: i64 = conn
            .query_row("SELECT until_ms FROM model_routes", [], |r| r.get(0))
            .unwrap();
        assert!((until - Store::now_ms() - FREE_ALLOWANCE_BACKOFF_MS).abs() < 100);
    }

    #[test]
    fn ordinary_throttle_keeps_doubling_and_fifteen_minute_cap() {
        let failure = classify(&error_log("429 upstream_unavailable")).unwrap();
        assert!(!failure.free_allowance);
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        record(&conn, "opencode-go/deepseek-v4.1-flash", Some(&failure)).unwrap();
        let first: i64 = conn
            .query_row("SELECT until_ms FROM model_routes", [], |r| r.get(0))
            .unwrap();
        assert!((first - Store::now_ms() - FIRST_BACKOFF_MS).abs() < 100);
        conn.execute("UPDATE model_routes SET failures=20", [])
            .unwrap();
        record(&conn, "opencode-go/deepseek-v4.1-flash", Some(&failure)).unwrap();
        let capped: i64 = conn
            .query_row("SELECT until_ms FROM model_routes", [], |r| r.get(0))
            .unwrap();
        assert!((capped - Store::now_ms() - MAX_BACKOFF_MS).abs() < 100);
    }

    #[test]
    fn quota_exhaustion_is_long_backoff_but_plain_rate_limit_is_short() {
        // A stated FREE allowance is the long, reset-aware case.
        let free = classify(&error_log(
            "HTTP 429 free quota exhausted; resets in 2 hours",
        ))
        .unwrap();
        assert!(free.free_allowance);
        assert_eq!(free.reset_ms, Some(7_200_000));
        // A bare paid quota error still throttles, but on the short schedule.
        let paid = classify(&error_log("429 quota exhausted")).unwrap();
        assert!(!paid.free_allowance);
        assert_eq!(paid.reset_ms, None);
        let rate = classify(&error_log("429 rate limit exceeded")).unwrap();
        assert!(!rate.free_allowance);
        assert_eq!(rate.reset_ms, None);
    }

    #[test]
    fn reset_timestamp_iso8601_is_parsed_as_an_absolute_time() {
        assert_eq!(
            iso_timestamp_ms("2026-10-17T00:00:00Z"),
            Some(1_792_195_200_000)
        );
        assert_eq!(
            iso_timestamp_ms("2026-10-17T02:00:00+02:00"),
            Some(1_792_195_200_000)
        );
    }

    fn native_error_log(message: &str) -> String {
        serde_json::to_string(&json!({
            "type":"error","timestamp":1,
            "error":{"name":"UnknownError","data":{"message":message}}
        }))
        .unwrap()
    }

    #[test]
    fn native_error_events_are_classified_and_paid_quota_stays_short() {
        let free = classify(&native_error_log(
            "Free usage limit exceeded; resets in 5 hours",
        ))
        .unwrap();
        assert!(free.free_allowance);
        assert_eq!(free.reset_ms, Some(5 * 3_600_000));

        let paid = classify(&native_error_log("429 quota exceeded")).unwrap();
        assert!(!paid.free_allowance);
        assert_eq!(paid.reset_ms, None);

        let rate = classify(&native_error_log("429 rate limit exceeded")).unwrap();
        assert!(!rate.free_allowance);

        // The route's real, non-throttle upstream failure is not a backoff.
        assert!(
            classify(&native_error_log(
                "Unexpected server error. Check server logs for details."
            ))
            .is_none()
        );
    }

    #[test]
    fn reset_delay_parses_an_iso_deadline_and_ignores_a_zero_window() {
        let now = 1_792_190_000_000;
        assert_eq!(
            reset_delay_ms("retry-after: 2026-10-17T00:00:00Z", now),
            Some(1_792_195_200_000 - now)
        );
        assert_eq!(reset_delay_ms("resets in 0 hours", now), None);
    }
}
