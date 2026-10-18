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
//! rather than an assistant `message_end`, so those errors are read too, as is
//! Gemini's `type:"error"`, which carries its text in a top-level `message` and
//! marks a non-fatal notice `severity:"warning"`, and its terminal
//! `type:"result"`, whose `status` and `error` say how the run ended.
//!
//! Claude Code states its own limits: every stream carries a
//! `rate_limit_event` whose `status` is `rejected` once a plan window is
//! spent, with the window's type and its `resetsAt` epoch. That backs the
//! route off until the reset. The five-hour and weekly windows cover the whole
//! account, so they back off every route on that harness and provider, not
//! just the model that ran. A failed `result` (`is_error`) is read as text
//! like any other error.
//!
//! A stated reset is honored wherever it appears, not only for a free
//! allowance. A provider's own limit that says when it reopens, an HTTP
//! `retry-after` and an ISO deadline in a throttle's text all hold the route
//! until then. A throttle that states one holds its route until then, and the
//! short doubling window only ever raises that floor, so a stated reset can
//! lengthen the backoff but never shorten it. A hint read out of free text
//! can be misread, so it is capped at two days; a structured reset (Claude's
//! `resetsAt`) is trusted up to eight.
//!
//! A provider that cannot be reached at all (Pi's `Connection error.`, a
//! refused or unreachable host, a bad gateway) is an outage like a 503: its
//! route backs off on the short doubling, so a model served from a machine
//! that comes and goes, such as a laptop's subscription, is used while it is
//! up and passed over while it is not.
//!
//! A route the captain marks `cost: free` runs on a daily allowance, so a rate
//! limit or quota on it is that allowance spent, however the provider words it
//! (OpenCode's free tier says only "Rate limit exceeded"); an outage on it
//! stays on the short doubling. Without a stated reset it waits for the reset
//! the captain knows (`allowance_resets_utc`, the UTC time it renews each
//! day); knowing neither, or throttled just after that time, it probes again
//! after an hour, then two, doubling to a day, so the allowance is used again
//! soon after it returns.
//!
//! A route the captain gives an `allowance` shape (see `crew_routes`) also
//! keeps a usage row (`route_usage`): when it last succeeded and was throttled,
//! its open rolling window, Claude's reported window use, and whether it is
//! capped. Its times come from the job record, so re-reading a job after a
//! crash changes nothing. On a shaped route an outage stays on the short
//! doubling; allowance wording (or any rate limit on a free route) caps it
//! until the provider's stated reset or, failing that, the shape's own reopen;
//! and a plain rate limit on a paid route is a strike, counted once per burst,
//! whose third in a row moves the route to the hourly probes, capped at the
//! shape's reopen. A weekly reset is used only on evidence: Claude's
//! `seven_day` limit or a stated reset longer than the window. A later throttle
//! never shortens a route's hold, so an outage from a run in flight cannot
//! hand back a route that was capped for hours.
use crate::crew_routes::{Allowance, Shape};
use crate::store::Store;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

const FIRST_BACKOFF_MS: i64 = 30_000;
const MAX_BACKOFF_MS: i64 = 15 * 60_000;
/// A spent allowance that states no reset is probed after an hour, doubling.
const ALLOWANCE_PROBE_MS: i64 = 60 * 60_000;
/// The longest an allowance with no stated reset waits: a day.
const FREE_ALLOWANCE_BACKOFF_MS: i64 = 24 * 60 * 60_000;
/// The longest reset a route is ever parked for: a weekly window, plus slack.
/// A structured reset (Claude's `resetsAt`) is clamped to it, because the
/// harness stated it.
const MAX_STATED_RESET_MS: i64 = 8 * FREE_ALLOWANCE_BACKOFF_MS;
/// The longest a reset read out of free text parks a route for. Text can be
/// misread, so a hint past two days is capped rather than trusted further.
const MAX_TEXT_RESET_MS: i64 = 2 * FREE_ALLOWANCE_BACKOFF_MS;
/// Claude windows that cover the whole account rather than one model.
const ACCOUNT_WINDOWS: [&str; 2] = ["five_hour", "seven_day"];
const CURSOR: &str = "routes_scanned_ms";
/// A usage row not touched for this long belongs to a route no longer shaped.
const USAGE_KEPT_MS: i64 = 30 * FREE_ALLOWANCE_BACKOFF_MS;
/// Plain rate limits in a row, one per burst, that make a paid route a suspected cap.
const STRIKES: i64 = 3;
/// Only the end of a Claude log is read for its windows on success.
const LOG_TAIL_BYTES: u64 = 64 * 1024;

pub fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS model_routes(route TEXT PRIMARY KEY, failures INTEGER NOT NULL,
           until_ms INTEGER NOT NULL, last_error TEXT, updated_ms INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS route_usage(route TEXT PRIMARY KEY, last_succeeded_ms INTEGER,
           last_throttled_ms INTEGER, last_throttle TEXT, strikes INTEGER NOT NULL DEFAULT 0,
           window_ends_ms INTEGER, window_utilization REAL, weekly_ends_ms INTEGER,
           weekly_utilization REAL, capped_until_ms INTEGER, updated_ms INTEGER NOT NULL);",
    )?;
    Ok(())
}

/// A shaped route's usage row.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Usage {
    pub last_succeeded_ms: Option<i64>,
    pub last_throttled_ms: Option<i64>,
    pub last_throttle: Option<String>,
    pub strikes: i64,
    pub window_ends_ms: Option<i64>,
    pub window_utilization: Option<f64>,
    pub weekly_ends_ms: Option<i64>,
    pub weekly_utilization: Option<f64>,
    pub capped_until_ms: Option<i64>,
    pub updated_ms: i64,
}

const USAGE_COLUMNS: &str = "route, last_succeeded_ms, last_throttled_ms, last_throttle, strikes,
    window_ends_ms, window_utilization, weekly_ends_ms, weekly_utilization, capped_until_ms, updated_ms";

fn usage_row(r: &rusqlite::Row) -> rusqlite::Result<(String, Usage)> {
    Ok((
        r.get(0)?,
        Usage {
            last_succeeded_ms: r.get(1)?,
            last_throttled_ms: r.get(2)?,
            last_throttle: r.get(3)?,
            strikes: r.get(4)?,
            window_ends_ms: r.get(5)?,
            window_utilization: r.get(6)?,
            weekly_ends_ms: r.get(7)?,
            weekly_utilization: r.get(8)?,
            capped_until_ms: r.get(9)?,
            updated_ms: r.get(10)?,
        },
    ))
}

/// Every shaped route's usage, by route identity.
pub(crate) fn usage(conn: &Connection) -> Result<std::collections::HashMap<String, Usage>> {
    let mut statement = conn.prepare(&format!("SELECT {USAGE_COLUMNS} FROM route_usage"))?;
    let rows = statement.query_map([], usage_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn usage_of(conn: &Connection, route: &str) -> Result<Usage> {
    Ok(conn
        .query_row(
            &format!("SELECT {USAGE_COLUMNS} FROM route_usage WHERE route=?1"),
            [route],
            usage_row,
        )
        .optional()?
        .map(|(_, usage)| usage)
        .unwrap_or_default())
}

/// The windows a Claude run last reported as allowed: the five-hour window's
/// end and use, and the seven-day window's.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct ClaudeWindows {
    window_ends_ms: Option<i64>,
    window_utilization: Option<f64>,
    weekly_ends_ms: Option<i64>,
    weekly_utilization: Option<f64>,
}

fn claude_windows(log: &str) -> Option<ClaudeWindows> {
    let info = crate::reply::events(log)
        .filter(|event| {
            event["type"] == "rate_limit_event" && event["rate_limit_info"]["status"] == "allowed"
        })
        .last()?;
    let windows = &info["rate_limit_info"]["unifiedWindows"];
    let ends = |window: &str| windows[window]["resetsAt"].as_i64().map(|at| at * 1000);
    let used = |window: &str| windows[window]["utilization"].as_f64();
    let found = ClaudeWindows {
        window_ends_ms: ends("five_hour"),
        window_utilization: used("five_hour"),
        weekly_ends_ms: ends("seven_day"),
        weekly_utilization: used("seven_day"),
    };
    (found != ClaudeWindows::default()).then_some(found)
}

/// The last bytes of a log, where a run's final events are.
fn log_tail(path: &std::path::Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let length = file.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(length.saturating_sub(LOG_TAIL_BYTES)));
    let mut raw = Vec::new();
    let _ = file.take(LOG_TAIL_BYTES).read_to_end(&mut raw);
    String::from_utf8_lossy(&raw).into_owned()
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
/// (`native_launch.py [PREFIX…] HARNESS BINARY … --provider P --model M …`).
/// Prefixes are the launcher's own flags: `--control JSON` on every Claude
/// launch, `--tool-bundles PLAN`, `--attended-policy JSON`, `--opencode-yolo`.
/// OpenCode V2 appends `#effort` to the model, which is not part of the route.
pub(crate) fn route_of_argv(command: &Value) -> Option<String> {
    let args: Vec<&str> = command
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let mut at = args.iter().position(|a| a.ends_with("native_launch.py"))? + 1;
    while let Some(arg) = args.get(at) {
        at += match *arg {
            "--control" | "--tool-bundles" | "--attended-policy" => 2,
            "--opencode-yolo" => 1,
            _ => break,
        };
    }
    let harness = args.get(at)?;
    let flag = |names: &[&str]| {
        args.windows(2)
            .find(|w| names.contains(&w[0]))
            .map(|w| w[1].split('#').next().unwrap_or(w[1]))
            .unwrap_or("-")
    };
    Some(format!(
        "{harness}/{}/{}",
        flag(&["--provider"]),
        flag(&["--model", "-m"])
    ))
}

/// A provider throttle in how the run's last reply ended, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RouteFailure {
    error: String,
    reset_ms: Option<i64>,
    /// A spent allowance: back off until its reset, not on the short doubling.
    allowance: bool,
    /// The limit covers every model on this harness and provider.
    account: bool,
    /// The longest a spent allowance with no stated reset is probed for: its
    /// shape's reopen.
    ceiling_ms: Option<i64>,
    /// Where on the hourly probe ladder a suspected cap starts, when not at
    /// the route's failure count.
    probe_step: Option<i64>,
}

/// The ledger key that stands for every model on a route's harness and provider.
fn account_key(route: &str) -> Option<String> {
    route
        .rsplit_once('/')
        .map(|(account, _)| format!("{account}/*"))
}

/// A Claude plan window the stream reports as spent.
fn claude_limit(info: &Value, now: i64) -> RouteFailure {
    let window = info["rateLimitType"].as_str().unwrap_or("usage");
    RouteFailure {
        error: format!("Claude {window} limit reached"),
        reset_ms: info["resetsAt"]
            .as_i64()
            .map(|at| at * 1000 - now)
            .filter(|ms| *ms > 0),
        allowance: true,
        account: ACCOUNT_WINDOWS.contains(&window),
        ceiling_ms: None,
        probe_step: None,
    }
}

fn reset_delay_ms(error: &str, now: i64) -> Option<i64> {
    let lower = error.to_ascii_lowercase();
    for marker in [
        "retry-after",
        "retry_after",
        "retry after",
        "reset_at",
        "reset-at",
        "resets at",
    ] {
        for (at, _) in lower.match_indices(marker) {
            let tail = &lower[at + marker.len()..];
            if let Some(absolute) = iso_timestamp_ms(tail)
                && absolute > now
            {
                return Some(absolute - now);
            }
            if let Some(delay) = stated_delay_ms(tail, now) {
                return Some(delay);
            }
        }
    }
    for (unit, multiplier) in [
        ("hour", 3_600_000_i64),
        ("hr", 3_600_000),
        ("day", 86_400_000),
        ("week", 604_800_000),
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

/// The delay a retry marker states, read only from the number that directly
/// follows it (`: 120`, `=120`, ` 3h`). A `_ms` or `-ms` suffix makes the
/// number milliseconds; a unit word scales it; a bare number is seconds, as an
/// HTTP `retry-after` is, unless it is an epoch. Anything else reads as nothing.
fn stated_delay_ms(tail: &str, now: i64) -> Option<i64> {
    let (milliseconds, rest) = match tail
        .strip_prefix("_ms")
        .or_else(|| tail.strip_prefix("-ms"))
    {
        Some(rest) => (true, rest),
        None => (false, tail),
    };
    let rest = rest.trim_start_matches([':', '=', ' ', '"']);
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let value = rest[..end].parse::<i64>().ok().filter(|value| *value > 0)?;
    if milliseconds {
        return Some(value);
    }
    let word: String = rest[end..]
        .trim_start_matches(' ')
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect();
    let ms = match word.as_str() {
        "" if value > 1_000_000_000 => {
            // An epoch in seconds, as a retry-after date is stated.
            return Some(value.saturating_mul(1000) - now).filter(|ms| *ms > 0);
        }
        "" | "s" | "sec" | "secs" | "second" | "seconds" => value.saturating_mul(1000),
        "ms" => value,
        "m" | "min" | "mins" | "minute" | "minutes" => value.saturating_mul(60_000),
        "h" | "hr" | "hrs" | "hour" | "hours" => value.saturating_mul(3_600_000),
        "d" | "day" | "days" => value.saturating_mul(86_400_000),
        _ => return None,
    };
    Some(ms)
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
    let mut rejected = None;
    for event in crate::reply::events(log) {
        if event["type"] == "rate_limit_event" {
            let info = &event["rate_limit_info"];
            rejected = (info["status"] == "rejected").then(|| info.clone());
        } else if event["type"] == "result" && event["is_error"] == true {
            // Claude Code ends a failed run with an error result.
            if let Some(text) = event["result"].as_str().filter(|text| !text.is_empty()) {
                native_error = Some(text.to_owned());
            }
        } else if event["type"] == "result"
            && event["status"].is_string()
            && (event["status"] != "success" || !event["error"].is_null())
        {
            // Gemini ends a run with a result event whose status and error say
            // how it went; a successful one is never a throttle.
            let text = event["error"]["message"]
                .as_str()
                .or_else(|| event["error"].as_str())
                .or_else(|| event["result"].as_str())
                .filter(|text| !text.is_empty());
            if let Some(text) = text {
                native_error = Some(text.to_owned());
            }
        } else if event["type"] == "message_end" && event["message"]["role"] == "assistant" {
            last = Some(event["message"].clone());
        } else if event["type"] == "error" && event["severity"] != "warning" {
            // Native adapters (OpenCode V1, Gemini) report a failed turn as a
            // final error event instead of Pi's assistant message_end. Gemini
            // keeps the text in a top-level message, and a notice it marks as a
            // warning did not end the run, so it says nothing about the route.
            let text = event["error"]["data"]["message"]
                .as_str()
                .or_else(|| event["error"]["message"].as_str())
                .or_else(|| event["error"].as_str())
                .or_else(|| event["message"].as_str())
                .map(str::to_owned);
            if let Some(text) = text.filter(|text| !text.is_empty()) {
                native_error = Some(text);
            }
        } else if event["event"] == "result" && event["result"]["status"] != "SUCCESS" {
            // AGY ends a run with a result event instead of Pi's message_end.
            // Only a failed result carries an error; a SUCCESS reply is never a throttle.
            let text = ["errorMessage", "error_message", "error", "response"]
                .iter()
                .find_map(|field| event["result"][field].as_str())
                .filter(|text| !text.is_empty());
            if let Some(text) = text {
                native_error = Some(text.to_owned());
            }
        }
    }
    if let Some(info) = rejected {
        return Some(claude_limit(&info, Store::now_ms()));
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
        || ["429", "502", "503", "504", "529"]
            .iter()
            .any(|code| lower.starts_with(code))
        || [
            "rate limit",
            "rate_limit",
            "overloaded",
            "upstream_unavailable",
            "quota",
            "timed out",
            "usage limit",
            "hit your limit",
            // The provider cannot be reached at all, such as a model served
            // from a laptop that is asleep: an outage, on the short doubling.
            "connection error",
            "connection refused",
            "econnrefused",
            "ehostunreach",
            "enetunreach",
            "fetch failed",
            "socket hang up",
        ]
        .iter()
        .any(|sign| lower.contains(sign));
    throttled.then(|| RouteFailure {
        error: error.chars().take(200).collect(),
        // A hint read out of free text can be misread, so it is capped at two
        // days: a misread never parks its route for longer than that.
        reset_ms: reset_delay_ms(&error, Store::now_ms()).map(|ms| ms.min(MAX_TEXT_RESET_MS)),
        allowance: free_allowance,
        account: false,
        ceiling_ms: None,
        probe_step: None,
    })
}

/// A rate limit or quota on a route the captain marks free: its allowance is
/// spent. An outage on it (503, overloaded, timed out) stays on the short
/// doubling. A reset the provider stated wins; otherwise the daily reset the
/// captain knows (minute of the UTC day), unless the throttle came just after
/// that reset, which shows the knowledge is off and leaves it to the probes.
fn free_allowance(mut failure: RouteFailure, resets_utc: Option<u32>, now: i64) -> RouteFailure {
    const DAY_MS: i64 = 86_400_000;
    const JUST_AFTER_MS: i64 = 15 * 60_000;
    let lower = failure.error.to_ascii_lowercase();
    let spent = lower.starts_with("429")
        || [
            "rate limit",
            "rate_limit",
            "quota",
            "limit exceeded",
            "usage limit",
            "allowance",
            "credits",
            "hit your limit",
        ]
        .iter()
        .any(|sign| lower.contains(sign));
    if !spent {
        return failure;
    }
    failure.allowance = true;
    if failure.reset_ms.is_none()
        && let Some(minute) = resets_utc
    {
        let delay = (i64::from(minute) * 60_000 - now).rem_euclid(DAY_MS);
        if delay > 0 && delay < DAY_MS - JUST_AFTER_MS {
            failure.reset_ms = Some(delay);
        }
    }
    failure
}

/// How a throttle on a shaped route counts.
#[derive(Debug, PartialEq, Eq)]
enum Throttle {
    /// The provider is down or overloaded: the short doubling, never a cap.
    Outage,
    /// A rate limit with no word of an allowance.
    Plain,
    /// The allowance is spent.
    Spent,
}

fn throttle_kind(failure: &RouteFailure) -> Throttle {
    let lower = failure.error.to_ascii_lowercase();
    if failure.allowance
        || [
            "quota",
            "usage limit",
            "allowance",
            "exhausted",
            "hit your limit",
            "limit reached",
        ]
        .iter()
        .any(|sign| lower.contains(sign))
    {
        Throttle::Spent
    } else if lower.starts_with("429") || lower.contains("rate limit") || lower.contains("rate_limit")
    {
        Throttle::Plain
    } else {
        Throttle::Outage
    }
}

/// A throttle on a route with a declared allowance shape, judged against its
/// usage: whether it caps the route and until when, and whether it is a new
/// strike. A strike is a plain rate limit on a paid route from a job launched
/// after the route's last throttle, so one burst of concurrent runs counts
/// once.
fn allowance_spent(
    mut failure: RouteFailure,
    allowance: &Allowance,
    usage: &Usage,
    created_ms: i64,
    now: i64,
) -> (RouteFailure, bool) {
    const JUST_AFTER_MS: i64 = 15 * 60_000;
    let kind = throttle_kind(&failure);
    // When the shape reopens, if it can tell.
    let reopen = match allowance.shape {
        Shape::Daily(minute) => {
            let next = crate::crew_routes::next_daily(minute, now);
            // Throttled just after the declared reset: the time is off.
            (next - now < FREE_ALLOWANCE_BACKOFF_MS - JUST_AFTER_MS).then_some(next - now)
        }
        Shape::Rolling { .. } => usage
            .window_ends_ms
            .filter(|end| *end > now)
            .map(|end| end - now),
    };
    let ceiling = match allowance.shape {
        Shape::Daily(_) => reopen,
        Shape::Rolling { window_ms, .. } => Some(reopen.unwrap_or(window_ms)),
    };
    match kind {
        Throttle::Outage => return (failure, false),
        Throttle::Plain if !allowance.free => {
            let strike = usage.last_throttled_ms.is_none_or(|at| created_ms > at);
            let strikes = usage.strikes + i64::from(strike);
            if strikes >= STRIKES && failure.reset_ms.is_none() {
                failure.allowance = true;
                failure.probe_step = Some(strikes - STRIKES);
                failure.ceiling_ms = ceiling;
            }
            return (failure, strike);
        }
        Throttle::Plain | Throttle::Spent => {}
    }
    failure.allowance = true;
    let weekly = match allowance.shape {
        Shape::Rolling {
            window_ms,
            weekly: Some(minute),
        } if failure.error.contains("seven_day")
            || failure.reset_ms.is_some_and(|reset| reset > window_ms) =>
        {
            Some(crate::crew_routes::next_weekly(minute, now) - now)
        }
        _ => None,
    };
    if let Some(weekly) = weekly {
        // The captain declared the weekly reset, so it is trusted past the
        // two days a reset read from text may hold a route.
        failure.reset_ms = Some(weekly.max(failure.reset_ms.unwrap_or(0)));
    } else if failure.reset_ms.is_none() {
        failure.reset_ms = reopen;
        failure.ceiling_ms = ceiling;
    }
    (failure, false)
}

/// A provider throttle in how the run's last reply ended, if any.
pub fn throttle(log: &str) -> Option<String> {
    classify(log).map(|failure| failure.error)
}

/// When a route may be used again, and why it may not now.
pub fn blocked(conn: &Connection, route: &str) -> Result<Option<(i64, String)>> {
    Ok(conn
        .query_row(
            "SELECT until_ms, COALESCE(last_error,'') FROM model_routes
             WHERE route IN (?1, ?3) AND until_ms > ?2 ORDER BY until_ms DESC LIMIT 1",
            params![route, Store::now_ms(), account_key(route)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// A refusal message for a backed-off or capped route, or None when it is clear.
pub fn refusal(store: &Store, route: &str) -> Result<Option<String>> {
    let conn = crate::jobs::host_ledger(store)?;
    let Some((until, error)) = blocked(&conn, route)? else {
        return Ok(None);
    };
    let now = Store::now_ms();
    let wait = "new crew and errands on it wait. Record the wait (note.pin key waiting) and \
                continue once it clears";
    Ok(Some(match capped_until(&conn, route, now)? {
        Some(_) => format!(
            "model route {route} is capped until {} (allowance spent: {error}); {wait}",
            utc_time(until)
        ),
        None => format!(
            "model route {route} is backing off for {}s after the provider throttled ({error}); {wait}",
            ((until - now) / 1000).max(1)
        ),
    }))
}

/// When a shaped route's spent allowance reopens, while it has not.
fn capped_until(conn: &Connection, route: &str, now: i64) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT capped_until_ms FROM route_usage WHERE route=?1 AND capped_until_ms > ?2",
            params![route, now],
            |r| r.get(0),
        )
        .optional()?)
}

/// `YYYY-MM-DD HH:MM UTC`.
pub(crate) fn utc_time(ms: i64) -> String {
    let minutes = ms.div_euclid(60_000);
    let days = minutes.div_euclid(1440);
    // Civil date from days since the epoch (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let minute = minutes.rem_euclid(1440);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        minute / 60,
        minute % 60
    )
}

/// A finished job on a route with a declared allowance shape.
struct Shaped<'a> {
    allowance: &'a Allowance,
    created_ms: i64,
    finished_ms: i64,
    windows: Option<ClaudeWindows>,
}

#[cfg(test)]
fn record(conn: &Connection, route: &str, failure: Option<&RouteFailure>) -> Result<()> {
    record_job(conn, route, failure.cloned(), None)
}

/// Back a route off or clear it, and for a shaped route update its usage, in
/// one transaction. A hold is never shortened: the later `until_ms` stays.
fn record_job(
    conn: &Connection,
    route: &str,
    failure: Option<RouteFailure>,
    shaped: Option<Shaped>,
) -> Result<()> {
    let now = Store::now_ms();
    let tx = conn.unchecked_transaction()?;
    let mut usage = match &shaped {
        Some(_) => usage_of(&tx, route)?,
        None => Usage::default(),
    };
    let (failure, strike) = match (failure, &shaped) {
        (Some(failure), Some(shaped)) => {
            let (failure, strike) =
                allowance_spent(failure, shaped.allowance, &usage, shaped.created_ms, now);
            (Some(failure), strike)
        }
        (failure, _) => (failure, false),
    };
    let mut held_until = None;
    match &failure {
        Some(failure) => {
            let account = failure.account.then(|| account_key(route)).flatten();
            let key = account.as_deref().unwrap_or(route);
            let (previous, held): (i64, i64) = tx
                .query_row(
                    "SELECT failures, until_ms FROM model_routes WHERE route=?1",
                    [key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
                .unwrap_or((0, 0));
            let failures = previous + 1;
            let short = FIRST_BACKOFF_MS
                .saturating_mul(1i64 << (failures - 1).min(10))
                .min(MAX_BACKOFF_MS);
            // A stated reset is the provider's own word on when to try again, so
            // it holds the route until then however plain the throttle looks;
            // the doubling only ever raises that floor, never lowers it. With no
            // stated reset a spent allowance is probed hourly, doubling to a
            // day (and no later than its shape reopens), and an ordinary
            // throttle keeps doubling on the short window.
            let probe = ALLOWANCE_PROBE_MS
                .saturating_mul(1i64 << failure.probe_step.unwrap_or(failures - 1).min(10))
                .min(FREE_ALLOWANCE_BACKOFF_MS);
            let probe = failure
                .ceiling_ms
                .map_or(probe, |ceiling| probe.min(ceiling.max(short)));
            let backoff = match (failure.reset_ms, failure.allowance) {
                (Some(reset), _) => reset.max(short),
                (None, true) => probe,
                (None, false) => short,
            }
            .min(MAX_STATED_RESET_MS);
            let until = (now + backoff).max(held);
            tx.execute(
                "INSERT INTO model_routes(route, failures, until_ms, last_error, updated_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(route) DO UPDATE SET failures=excluded.failures,
                   until_ms=excluded.until_ms, last_error=excluded.last_error,
                   updated_ms=excluded.updated_ms",
                params![key, failures, until, failure.error, now],
            )?;
            held_until = Some(until);
        }
        None => {
            // A success shows the whole account is usable again.
            tx.execute(
                "DELETE FROM model_routes WHERE route IN (?1, ?2)",
                params![route, account_key(route)],
            )?;
        }
    }
    if let Some(shaped) = shaped {
        let at = shaped.finished_ms;
        let later = |old: Option<i64>| Some(old.map_or(at, |old| old.max(at)));
        match &failure {
            None => {
                usage.last_succeeded_ms = later(usage.last_succeeded_ms);
                usage.strikes = 0;
                usage.capped_until_ms = None;
                if let Shape::Rolling { window_ms, .. } = shaped.allowance.shape
                    && usage
                        .window_ends_ms
                        .is_none_or(|end| shaped.created_ms >= end)
                {
                    usage.window_ends_ms = Some(shaped.created_ms + window_ms);
                }
                if let Some(windows) = shaped.windows {
                    usage.window_ends_ms = windows.window_ends_ms.or(usage.window_ends_ms);
                    usage.window_utilization = windows.window_utilization;
                    usage.weekly_ends_ms = windows.weekly_ends_ms;
                    usage.weekly_utilization = windows.weekly_utilization;
                }
            }
            Some(failure) => {
                usage.last_throttled_ms = later(usage.last_throttled_ms);
                usage.last_throttle = Some(failure.error.clone());
                usage.strikes += i64::from(strike);
                if failure.allowance {
                    usage.capped_until_ms = held_until;
                }
            }
        }
        usage.updated_ms = usage.updated_ms.max(at);
        tx.execute(
            &format!(
                "INSERT OR REPLACE INTO route_usage({USAGE_COLUMNS})
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
            ),
            params![
                route,
                usage.last_succeeded_ms,
                usage.last_throttled_ms,
                usage.last_throttle,
                usage.strikes,
                usage.window_ends_ms,
                usage.window_utilization,
                usage.weekly_ends_ms,
                usage.weekly_utilization,
                usage.capped_until_ms,
                usage.updated_ms
            ],
        )?;
    }
    tx.commit()?;
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
    let (conn, free, shaped) = if jobs.is_empty() {
        (None, Default::default(), Default::default())
    } else {
        // A routes file that cannot be read must not stop the backoff itself.
        let free = crate::crew_routes::free_allowances(store).unwrap_or_default();
        let shaped = crate::crew_routes::allowances(store).unwrap_or_default();
        let conn = crate::jobs::host_ledger(store)?;
        conn.execute(
            "DELETE FROM route_usage WHERE updated_ms < ?1",
            [Store::now_ms() - USAGE_KEPT_MS],
        )?;
        (Some(conn), free, shaped)
    };
    for job in jobs {
        let finished_ms = job["finished_ms"].as_i64().unwrap_or(through);
        through = through.max(finished_ms);
        let route = job["model_route"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| route_of_argv(&job["command"]));
        let Some(route) = route else {
            continue;
        };
        let log = store
            .home()
            .join("jobs")
            .join(job["id"].as_str().unwrap_or(""))
            .join(job["incarnation"].as_str().unwrap_or(""))
            .join("stdout.log");
        let allowance = shaped.get(&route);
        let status = job["status"].as_str().unwrap_or("");
        let throttled = if status == "succeeded" {
            None
        } else {
            match classify(&std::fs::read_to_string(&log).unwrap_or_default()) {
                Some(failure) if allowance.is_some() => Some(failure),
                Some(failure) => Some(match free.get(&route) {
                    Some(resets) => free_allowance(failure, *resets, Store::now_ms()),
                    None => failure,
                }),
                // A failure for any other reason says nothing about the route.
                None => continue,
            }
        };
        let shaped = allowance.map(|allowance| Shaped {
            allowance,
            created_ms: job["created_ms"].as_i64().unwrap_or(finished_ms),
            finished_ms,
            windows: (throttled.is_none() && route.starts_with("claude/"))
                .then(|| claude_windows(&log_tail(&log)))
                .flatten(),
        });
        let throttled_now = throttled.is_some();
        if let Some(conn) = &conn {
            record_job(conn, &route, throttled, shaped)?;
        }
        seen.push(json!({"route": route, "throttled": throttled_now}));
    }
    store.connection().execute(
        "INSERT INTO metadata(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![CURSOR, through.to_string()],
    )?;
    Ok(seen)
}

/// Backed-off routes, for status views; a capped one says until when.
pub fn list(conn: &Connection) -> Result<Vec<Value>> {
    let now = Store::now_ms();
    let mut statement = conn.prepare(
        "SELECT m.route, m.failures, m.until_ms, m.last_error, u.capped_until_ms
         FROM model_routes m LEFT JOIN route_usage u
           ON u.route = m.route AND u.capped_until_ms > ?1
         WHERE m.until_ms > ?1 ORDER BY m.route",
    )?;
    let rows = statement.query_map([now], |r| {
        let mut row = json!({"route": r.get::<_, String>(0)?, "failures": r.get::<_, i64>(1)?,
            "until_ms": r.get::<_, i64>(2)?, "last_error": r.get::<_, Option<String>>(3)?});
        if let Some(capped) = r.get::<_, Option<i64>>(4)? {
            row["capped_until_ms"] = json!(capped);
        }
        Ok(row)
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
    fn a_launch_argv_reads_back_its_route_past_the_launcher_prefixes() {
        let argv = |items: &[&str]| json!(items);
        let claude = argv(&[
            "python3",
            "/p/native_launch.py",
            "--tool-bundles",
            "{}",
            "--control",
            "{\"x\":1}",
            "claude",
            "/bin/claude",
            "--model",
            "haiku",
        ]);
        assert_eq!(route_of_argv(&claude).as_deref(), Some("claude/-/haiku"));
        let yolo = argv(&[
            "python3",
            "/p/native_launch.py",
            "--opencode-yolo",
            "opencode",
            "/bin/opencode",
            "--provider",
            "acme",
            "--model",
            "fast#high",
        ]);
        assert_eq!(route_of_argv(&yolo).as_deref(), Some("opencode/acme/fast"));
    }

    #[test]
    fn an_unreachable_provider_backs_its_route_off_as_an_outage() {
        for text in ["Connection error.", "fetch failed", "502 Bad Gateway"] {
            let failure = classify(&error_log(text)).unwrap_or_else(|| panic!("{text}"));
            assert!(!failure.allowance, "{text}");
            assert_eq!(failure.reset_ms, None, "{text}");
        }
        assert!(classify(&error_log("Tool call arguments were invalid")).is_none());
    }

    #[test]
    fn free_allowance_uses_reset_hint_or_full_allowance_window() {
        let reset = classify(&error_log("Free usage limit exceeded; retry-after: 7200")).unwrap();
        assert!(reset.allowance);
        assert_eq!(reset.reset_ms, Some(7_200_000));
        let fallback = classify(&error_log("Free allowance exhausted")).unwrap();
        assert!(fallback.allowance);
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
        // The second spent allowance in a row, stating no reset, is probed
        // again after two hours.
        assert!((parked_ms(&conn) - 2 * ALLOWANCE_PROBE_MS).abs() < 100);

        // A misread hint (here, seconds for what reads like an epoch) is capped
        // at two days rather than trusted, so it never parks the route longer.
        let misread = classify(&error_log(
            "Free usage limit exceeded; retry-after: 99999999999",
        ))
        .unwrap();
        assert_eq!(misread.reset_ms, Some(MAX_TEXT_RESET_MS));
        record(&conn, "opencode/-/space-bunny-free", Some(&misread)).unwrap();
        assert!((parked_ms(&conn) - MAX_TEXT_RESET_MS).abs() < 100);
    }

    #[test]
    fn a_throttle_on_a_free_route_spends_its_allowance_until_the_known_or_probed_reset() {
        // OpenCode's free tier words its spent allowance as a plain rate limit.
        let plain =
            || classify(&error_log("Rate limit exceeded. Please try again later.")).unwrap();
        assert!(!plain().allowance);
        // 2026-10-17T19:50:00Z: four hours ten minutes before a midnight reset.
        let now = 1_792_266_600_000;
        let known = free_allowance(plain(), Some(0), now);
        assert!(known.allowance);
        assert_eq!(known.reset_ms, Some((4 * 60 + 10) * 60_000));
        // A reset the provider states wins over the captain's knowledge, and a
        // throttle just after the known reset falls back to probing.
        let stated = classify(&error_log("Rate limit exceeded; retry-after: 600")).unwrap();
        assert_eq!(free_allowance(stated, Some(0), now).reset_ms, Some(600_000));
        assert_eq!(
            free_allowance(plain(), Some(19 * 60 + 45), now).reset_ms,
            None
        );
        // An outage on a free route is not its allowance.
        let outage = classify(&error_log("503 upstream_unavailable")).unwrap();
        assert!(!free_allowance(outage, Some(0), now).allowance);

        // Knowing no reset, the route is probed after an hour, then two, then
        // four, and never waits more than a day; a success clears it.
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        let route = "opencode/-/opencode/space-bunny-free";
        for hours in [1, 2, 4] {
            record(&conn, route, Some(&free_allowance(plain(), None, now))).unwrap();
            assert!((parked_ms(&conn) - hours * ALLOWANCE_PROBE_MS).abs() < 100);
        }
        conn.execute("UPDATE model_routes SET failures=20", [])
            .unwrap();
        record(&conn, route, Some(&free_allowance(plain(), None, now))).unwrap();
        assert!((parked_ms(&conn) - FREE_ALLOWANCE_BACKOFF_MS).abs() < 100);
        record(&conn, route, None).unwrap();
        assert!(blocked(&conn, route).unwrap().is_none());
    }

    #[test]
    fn ordinary_throttle_keeps_doubling_and_fifteen_minute_cap() {
        let failure = classify(&error_log("429 upstream_unavailable")).unwrap();
        assert!(!failure.allowance);
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
        assert!(free.allowance);
        assert_eq!(free.reset_ms, Some(7_200_000));
        // A bare paid quota error still throttles, but on the short schedule.
        let paid = classify(&error_log("429 quota exhausted")).unwrap();
        assert!(!paid.allowance);
        assert_eq!(paid.reset_ms, None);
        let rate = classify(&error_log("429 rate limit exceeded")).unwrap();
        assert!(!rate.allowance);
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
        assert!(free.allowance);
        assert_eq!(free.reset_ms, Some(5 * 3_600_000));

        let paid = classify(&native_error_log("429 quota exceeded")).unwrap();
        assert!(!paid.allowance);
        assert_eq!(paid.reset_ms, None);

        let rate = classify(&native_error_log("429 rate limit exceeded")).unwrap();
        assert!(!rate.allowance);

        // The route's real, non-throttle upstream failure is not a backoff.
        assert!(
            classify(&native_error_log(
                "Unexpected server error. Check server logs for details."
            ))
            .is_none()
        );
    }

    #[test]
    fn claude_windows_back_off_the_account_or_one_model_until_reset() {
        let spent = |window: &str| {
            let reset = Store::now_ms() / 1000 + 4 * 24 * 3600;
            serde_json::to_string(&json!({"type":"rate_limit_event","rate_limit_info":
                {"status":"rejected","rateLimitType":window,"resetsAt":reset}}))
            .unwrap()
        };
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        let (opus, haiku) = ("claude/-/claude-opus-5-5", "claude/-/claude-haiku-5-5");

        // A model's own weekly window leaves the account's other models alone.
        let model = classify(&spent("seven_day_opus")).unwrap();
        assert!(model.allowance && !model.account);
        record(&conn, opus, Some(&model)).unwrap();
        assert!(blocked(&conn, haiku).unwrap().is_none());
        let (until, _) = blocked(&conn, opus).unwrap().unwrap();
        assert!((until - Store::now_ms() - 4 * FREE_ALLOWANCE_BACKOFF_MS).abs() < 2000);

        // The weekly window stops every Claude model; a later success clears it.
        record(&conn, opus, Some(&classify(&spent("seven_day")).unwrap())).unwrap();
        assert!(blocked(&conn, haiku).unwrap().is_some());
        record(&conn, haiku, None).unwrap();
        assert!(blocked(&conn, haiku).unwrap().is_none());

        // A window still open says nothing about the route.
        let open = spent("five_hour").replace("rejected", "allowed");
        assert!(classify(&open).is_none());
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

    fn agy_result_log(status: &str, response: &str) -> String {
        serde_json::to_string(&json!({
            "event":"result",
            "result":{"status":status,"response":response,"num_turns":1}
        }))
        .unwrap()
    }

    #[test]
    fn agy_result_quota_error_throttles_its_own_model_until_its_stated_reset() {
        let failure = classify(&agy_result_log(
            "ERROR",
            "429 quota exceeded; retry-after: 7200",
        ))
        .unwrap();
        // Not a stated free allowance and not an account window, so it covers only
        // its own model; the two hours it states hold the route past the first step.
        assert!(!failure.allowance && !failure.account);
        assert_eq!(failure.reset_ms, Some(7_200_000));

        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        record(&conn, "agy/-/muse-max", Some(&failure)).unwrap();
        assert!((parked_ms(&conn) - 7_200_000).abs() < 100);
    }

    #[test]
    fn retry_markers_read_only_the_number_that_follows_them() {
        let now = 1_792_190_000_000;
        // `_ms` and `-ms` suffixes state milliseconds, never seconds.
        assert_eq!(reset_delay_ms("retry-after-ms: 30000", now), Some(30_000));
        assert_eq!(reset_delay_ms("retry_after_ms: 600000", now), Some(600_000));
        // A marker with no number directly after it reads nothing, rather than
        // the 429 further along the text.
        assert_eq!(reset_delay_ms("retry-after missing ... 429", now), None);
        // The space form and unit words are read too.
        assert_eq!(reset_delay_ms("retry after 3h", now), Some(10_800_000));
        assert_eq!(reset_delay_ms("retry-after: 2 minutes", now), Some(120_000));
        assert_eq!(reset_delay_ms("retry-after: 2 requests", now), None);
    }

    #[test]
    fn free_text_resets_are_capped_at_two_days_and_structured_ones_are_not() {
        let week = classify(&error_log("429 quota exceeded; resets in 1 week")).unwrap();
        assert_eq!(week.reset_ms, Some(MAX_TEXT_RESET_MS));
        let days = classify(&error_log("429 try again in 7 days")).unwrap();
        assert_eq!(days.reset_ms, Some(MAX_TEXT_RESET_MS));
        let ms = classify(&error_log("429 retry-after-ms: 30000")).unwrap();
        assert_eq!(ms.reset_ms, Some(30_000));

        // Claude's structured resetsAt keeps the eight-day ceiling.
        let claude = claude_limit(
            &json!({"rateLimitType":"seven_day","resetsAt": Store::now_ms() / 1000 + 7 * 86_400}),
            Store::now_ms(),
        );
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        record(&conn, "claude/-/opus", Some(&claude)).unwrap();
        assert!((parked_ms(&conn) - 7 * 86_400_000).abs() < 2000);
    }

    #[test]
    fn a_short_stated_reset_keeps_the_doubling_window() {
        let failure = classify(&error_log("429 quota exceeded; retry-after: 60")).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        // Three failures have doubled the window to two minutes; a one-minute
        // stated reset is below it, so the window stands.
        conn.execute(
            "INSERT INTO model_routes(route, failures, until_ms, last_error, updated_ms)
             VALUES('gemini/-/gemini-flash', 2, 0, '', 0)",
            [],
        )
        .unwrap();
        record(&conn, "gemini/-/gemini-flash", Some(&failure)).unwrap();
        assert!((parked_ms(&conn) - 120_000).abs() < 100);
    }

    #[test]
    fn agy_result_non_throttle_errors_and_successes_are_ignored() {
        assert!(classify(&agy_result_log("ERROR", "permission denied")).is_none());
        assert!(classify(&agy_result_log("SUCCESS", "429 quota in the reply text")).is_none());
    }

    /// A pause of this many milliseconds, read back from the ledger.
    fn parked_ms(conn: &Connection) -> i64 {
        let until: i64 = conn
            .query_row("SELECT until_ms FROM model_routes", [], |r| r.get(0))
            .unwrap();
        until - Store::now_ms()
    }

    #[test]
    fn a_stated_reset_holds_a_plain_throttle_until_the_provider_reopens() {
        // Gemini's five-hour and weekly limits are not phrased as a free
        // allowance, and they say when they reopen. That stated reset, not the
        // fifteen-minute cap, is what holds the route.
        let five_hour = classify(&error_log(
            "429 RESOURCE_EXHAUSTED: rate limit exceeded; resets in 5 hours",
        ))
        .unwrap();
        assert!(!five_hour.allowance && !five_hour.account);
        let weekly = classify(&error_log(
            "429 RESOURCE_EXHAUSTED: weekly quota exceeded; resets in 1 week",
        ))
        .unwrap();
        assert!(!weekly.allowance);
        // A week stated in free text is capped at two days.
        assert_eq!(weekly.reset_ms, Some(MAX_TEXT_RESET_MS));

        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        let route = "gemini/-/gemini-flash";
        record(&conn, route, Some(&five_hour)).unwrap();
        assert!((parked_ms(&conn) - 5 * 3_600_000).abs() < 2000);
        record(&conn, route, Some(&weekly)).unwrap();
        assert!((parked_ms(&conn) - MAX_TEXT_RESET_MS).abs() < 2000);

        // An absolute deadline far past any real window is capped at two days
        // rather than trusted, so the route is never parked for years.
        let absurd = classify(&error_log(
            "429 rate limit exceeded; resets at 2100-01-01T00:00:00Z",
        ))
        .unwrap();
        assert_eq!(absurd.reset_ms, Some(MAX_TEXT_RESET_MS));
        conn.execute("UPDATE model_routes SET failures=0", [])
            .unwrap();
        record(&conn, route, Some(&absurd)).unwrap();
        assert!((parked_ms(&conn) - MAX_TEXT_RESET_MS).abs() < 2000);

        // A later success on the route clears it, as for any throttle.
        record(&conn, route, None).unwrap();
        assert!(blocked(&conn, route).unwrap().is_none());
    }

    #[test]
    fn a_stated_reset_never_shortens_a_backoff_that_already_grew() {
        let failure = classify(&error_log("429 quota exceeded; retry-after: 60")).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        record(&conn, "gemini/-/gemini-flash", Some(&failure)).unwrap();
        // Repeated failures have already reached the cap; a minute the provider
        // stated must not hand the route back earlier than the cap does.
        conn.execute("UPDATE model_routes SET failures=20", [])
            .unwrap();
        record(&conn, "gemini/-/gemini-flash", Some(&failure)).unwrap();
        assert!((parked_ms(&conn) - MAX_BACKOFF_MS).abs() < 100);
    }

    fn gemini_error_log(severity: &str, message: &str) -> String {
        serde_json::to_string(&json!({
            "type":"error","timestamp":1,"severity":severity,"message":message
        }))
        .unwrap()
    }

    fn gemini_result_log(status: &str, error: Option<Value>) -> String {
        serde_json::to_string(&json!({
            "type":"result","session_id":"4fa4f4bc-5b2d-4553-8ae1-eb09e501ea63",
            "status":status,"error":error
        }))
        .unwrap()
    }

    #[test]
    fn gemini_errors_are_read_from_their_own_event_shape() {
        // Gemini keeps the text in a top-level message, so a rate limit there
        // backs the route off instead of passing as an ordinary failure.
        let throttle = classify(&gemini_error_log(
            "error",
            "429 Too Many Requests. Gemini Flash quota exceeded; resets in 5 hours",
        ))
        .unwrap();
        assert_eq!(throttle.reset_ms, Some(5 * 3_600_000));
        assert!(!throttle.allowance && !throttle.account);

        // A notice Gemini marks as a warning did not end the run.
        assert!(
            classify(&gemini_error_log(
                "warning",
                "429 quota exceeded; resets in 5 hours"
            ))
            .is_none()
        );

        // Its terminal result says how the run ended, and a success is not one.
        let result = classify(&gemini_result_log(
            "error",
            Some(json!({"type":"ApiError","message":"429 rate limit exceeded; resets in 2 hours"})),
        ))
        .unwrap();
        assert_eq!(result.reset_ms, Some(2 * 3_600_000));
        assert!(classify(&gemini_result_log("success", None)).is_none());
        assert!(classify(&gemini_result_log("error", None)).is_none());
        // An error alongside a success status still failed the run.
        assert!(
            classify(&gemini_result_log(
                "success",
                Some(json!("429 quota exceeded"))
            ))
            .is_some()
        );
    }
}

#[cfg(test)]
mod allowance_tests {
    use super::*;

    const HOUR: i64 = 3_600_000;

    fn ledger() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        conn
    }

    fn failure(error: &str) -> RouteFailure {
        RouteFailure {
            error: error.to_owned(),
            reset_ms: None,
            allowance: false,
            account: false,
            ceiling_ms: None,
            probe_step: None,
        }
    }

    fn rolling() -> Allowance {
        Allowance {
            shape: Shape::Rolling {
                window_ms: 5 * HOUR,
                weekly: Some(0),
            },
            free: false,
        }
    }

    fn job(
        conn: &Connection,
        route: &str,
        error: Option<&str>,
        allowance: &Allowance,
        created_ms: i64,
        finished_ms: i64,
    ) {
        let shaped = Shaped {
            allowance,
            created_ms,
            finished_ms,
            windows: None,
        };
        record_job(conn, route, error.map(failure), Some(shaped)).unwrap();
    }

    fn held(conn: &Connection, route: &str) -> i64 {
        blocked(conn, route).unwrap().map_or(0, |(until, _)| until) - Store::now_ms()
    }

    #[test]
    fn a_spent_allowance_holds_until_its_window_ends_and_a_later_outage_never_shortens_it() {
        let conn = ledger();
        let route = "codex/-/plan";
        let now = Store::now_ms();
        job(&conn, route, None, &rolling(), now - HOUR, now - HOUR + 1000);
        let usage = usage_of(&conn, route).unwrap();
        assert_eq!(usage.window_ends_ms, Some(now + 4 * HOUR));
        job(&conn, route, Some("You've hit your limit"), &rolling(), now - 60_000, now);
        assert!((held(&conn, route) - 4 * HOUR).abs() < 2000);
        let usage = usage_of(&conn, route).unwrap();
        assert!(usage.capped_until_ms.is_some_and(|c| (c - now - 4 * HOUR).abs() < 2000));
        // A run still in flight when the cap was set ends on an outage.
        job(&conn, route, Some("503 overloaded"), &rolling(), now - 30_000, now + 1);
        assert!((held(&conn, route) - 4 * HOUR).abs() < 2000, "{}", held(&conn, route));
        // A success clears the hold and the cap.
        job(&conn, route, None, &rolling(), now, now + 2);
        assert!(blocked(&conn, route).unwrap().is_none());
        assert_eq!(usage_of(&conn, route).unwrap().capped_until_ms, None);
    }

    #[test]
    fn plain_throttles_on_a_paid_route_strike_once_per_burst_then_probe_hourly_within_the_window() {
        let conn = ledger();
        let route = "codex/-/plan";
        let now = Store::now_ms();
        job(&conn, route, None, &rolling(), now - 2 * HOUR, now - 2 * HOUR);
        // Three runs launched together hit one per-minute limit: one strike.
        for at in 0..3 {
            job(&conn, route, Some("429 rate limit"), &rolling(), now - 10_000, now - 5000 + at);
        }
        assert_eq!(usage_of(&conn, route).unwrap().strikes, 1);
        assert!(usage_of(&conn, route).unwrap().capped_until_ms.is_none());
        // Re-reading the same jobs after a crash counts nothing again.
        job(&conn, route, Some("429 rate limit"), &rolling(), now - 10_000, now - 5000);
        assert_eq!(usage_of(&conn, route).unwrap().strikes, 1);
        // The second burst is a second strike, still the short doubling.
        conn.execute("UPDATE model_routes SET failures=1, until_ms=0", []).unwrap();
        job(&conn, route, Some("429 rate limit"), &rolling(), now - 4000, now - 3000);
        assert_eq!(usage_of(&conn, route).unwrap().strikes, 2);
        assert!((held(&conn, route) - 60_000).abs() < 4000, "{}", held(&conn, route));
        // The third goes to the hourly probe, not the end of the window.
        job(&conn, route, Some("429 rate limit"), &rolling(), now - 2000, now - 1000);
        assert!((held(&conn, route) - HOUR).abs() < 2000, "{}", held(&conn, route));
        // An outage is never a strike.
        job(&conn, route, Some("connection error"), &rolling(), now - 500, now);
        assert_eq!(usage_of(&conn, route).unwrap().strikes, 3);
    }

    #[test]
    fn the_probe_ladder_stops_at_the_windows_end() {
        let usage = Usage {
            window_ends_ms: Some(Store::now_ms() + 30 * 60_000),
            strikes: 2,
            ..Usage::default()
        };
        let now = Store::now_ms();
        let (judged, strike) = allowance_spent(failure("429 rate limit"), &rolling(), &usage, now, now);
        assert!(strike && judged.allowance);
        assert_eq!(judged.probe_step, Some(0));
        assert!(judged.ceiling_ms.is_some_and(|c| (c - 30 * 60_000).abs() < 2000));
    }

    #[test]
    fn a_weekly_reset_needs_evidence() {
        let now = Store::now_ms();
        let usage = Usage::default();
        // A spent allowance with no window known and no stated reset probes,
        // no longer than the window.
        let (plain, _) = allowance_spent(failure("usage limit reached"), &rolling(), &usage, now, now);
        assert_eq!((plain.reset_ms, plain.ceiling_ms), (None, Some(5 * HOUR)));
        // A stated reset longer than the window holds until the weekly reset.
        let mut stated = failure("usage limit reached; resets in 30 hours");
        stated.reset_ms = Some(30 * HOUR);
        let (weekly, _) = allowance_spent(stated, &rolling(), &usage, now, now);
        let monday = crate::crew_routes::next_weekly(0, now) - now;
        assert_eq!(weekly.reset_ms, Some(monday.max(30 * HOUR)));
        // A stated reset within the window is just that reset.
        let mut short = failure("usage limit reached");
        short.reset_ms = Some(HOUR);
        assert_eq!(allowance_spent(short, &rolling(), &usage, now, now).0.reset_ms, Some(HOUR));
    }

    #[test]
    fn a_daily_allowance_caps_until_its_reset_and_a_stale_cap_is_never_shown() {
        let conn = ledger();
        let route = "agy/-/flash";
        let now = Store::now_ms();
        let minute = ((now + 3 * HOUR) / 60_000 % 1440) as u32;
        let daily = Allowance {
            shape: Shape::Daily(minute),
            free: false,
        };
        job(&conn, route, Some("quota exhausted"), &daily, now - 1000, now);
        assert!((held(&conn, route) - 3 * HOUR).abs() < 61_000, "{}", held(&conn, route));
        assert!(list(&conn).unwrap()[0]["capped_until_ms"].is_i64());
        // Once the hold has passed, the cap is neither honoured nor shown.
        conn.execute("UPDATE model_routes SET until_ms=?1", [now - 1]).unwrap();
        conn.execute("UPDATE route_usage SET capped_until_ms=?1", [now - 1]).unwrap();
        assert!(blocked(&conn, route).unwrap().is_none());
        assert!(capped_until(&conn, route, Store::now_ms()).unwrap().is_none());
        assert!(list(&conn).unwrap().is_empty());
    }

    #[test]
    fn claude_success_reads_both_windows_from_the_last_allowed_event() {
        let log = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1,"rateLimitType":"five_hour","unifiedWindows":{"five_hour":{"utilization":0.11,"resetsAt":1792214400},"seven_day":{"utilization":0.03,"resetsAt":1792504800}}}}
{"type":"result","is_error":false,"result":"done"}"#;
        let windows = claude_windows(log).unwrap();
        assert_eq!(windows.window_ends_ms, Some(1_792_214_400_000));
        assert_eq!(windows.weekly_ends_ms, Some(1_792_504_800_000));
        assert_eq!(windows.window_utilization, Some(0.11));
        assert!(claude_windows("{\"type\":\"result\"}").is_none());
        let conn = ledger();
        let now = Store::now_ms();
        let shaped = Shaped {
            allowance: &rolling(),
            created_ms: now,
            finished_ms: now,
            windows: Some(windows),
        };
        record_job(&conn, "claude/-/opus", None, Some(shaped)).unwrap();
        let usage = usage_of(&conn, "claude/-/opus").unwrap();
        assert_eq!(usage.window_ends_ms, Some(1_792_214_400_000));
        assert_eq!(usage.weekly_utilization, Some(0.03));
    }

    #[test]
    fn utc_time_reads_a_civil_date() {
        assert_eq!(utc_time(1_792_266_600_000), "2026-10-17 19:50 UTC");
    }
}
