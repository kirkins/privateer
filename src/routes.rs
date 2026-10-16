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
use crate::store::Store;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

const FIRST_BACKOFF_MS: i64 = 30_000;
const MAX_BACKOFF_MS: i64 = 15 * 60_000;
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
pub fn throttle(log: &str) -> Option<String> {
    let mut last = None;
    for line in log.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if event["type"] == "message_end" && event["message"]["role"] == "assistant" {
            last = Some(event["message"].clone());
        }
    }
    let message = last?;
    if message["stopReason"] != "error" {
        return None;
    }
    let error = message["errorMessage"].as_str().unwrap_or("");
    let lower = error.to_ascii_lowercase();
    let throttled = ["429", "503", "529"]
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
    throttled.then(|| error.chars().take(200).collect())
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

fn record(conn: &Connection, route: &str, throttled: Option<&str>) -> Result<()> {
    let now = Store::now_ms();
    match throttled {
        Some(error) => {
            let failures: i64 = conn
                .query_row(
                    "SELECT failures FROM model_routes WHERE route=?1",
                    [route],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0)
                + 1;
            let backoff = FIRST_BACKOFF_MS
                .saturating_mul(1i64 << (failures - 1).min(10))
                .min(MAX_BACKOFF_MS);
            conn.execute(
                "INSERT INTO model_routes(route, failures, until_ms, last_error, updated_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(route) DO UPDATE SET failures=excluded.failures,
                   until_ms=excluded.until_ms, last_error=excluded.last_error,
                   updated_ms=excluded.updated_ms",
                params![route, failures, now + backoff, error, now],
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
            match throttle(&std::fs::read_to_string(log).unwrap_or_default()) {
                Some(error) => Some(error),
                // A failure for any other reason says nothing about the route.
                None => continue,
            }
        };
        if let Some(conn) = &conn {
            record(conn, &route, throttled.as_deref())?;
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
