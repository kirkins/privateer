//! Durable launch claims, separate from completed usage. Admission writes these
//! under the resource ledger's write lock. A scan transfers a claim to completed
//! usage in the same transaction that marks it accounted, so a dispatcher crash
//! before its home cursor commit cannot spend the run twice.
use super::*;

pub(super) fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS route_runs (
           key TEXT PRIMARY KEY, route TEXT NOT NULL, created_ms INTEGER NOT NULL,
           window_ends_ms INTEGER, accounted INTEGER NOT NULL DEFAULT 0);
         CREATE INDEX IF NOT EXISTS route_runs_route ON route_runs(route, accounted);",
    )?;
    Ok(())
}

/// A retryable change between route selection and transactional admission.
#[derive(Debug)]
pub(crate) struct AdmissionChanged(pub String);
impl std::fmt::Display for AdmissionChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for AdmissionChanged {}

pub(crate) fn running(conn: &Connection, route: &str) -> Result<u64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM reservations WHERE json_extract(record,'$.model_route')=?1",
        [route],
        |r| r.get(0),
    )?)
}

pub(crate) fn running_refusal(
    conn: &Connection,
    route: &str,
    max_running: Option<u64>,
) -> Result<Option<String>> {
    let Some(max) = max_running else {
        return Ok(None);
    };
    let count = running(conn, route)?;
    Ok((count >= max).then(|| format!(
        "model route {route} is full ({count} running, max_running {max}); new launches take the next route or use wait_for_capacity:true"
    )))
}

/// Lay unaccounted launches over completed usage, ignoring closed windows.
/// Also opens an idle rolling window on its first launch, rather than finish.
pub(super) fn overlay(
    conn: &Connection,
    usage: &mut std::collections::HashMap<String, Usage>,
) -> Result<()> {
    let now = Store::now_ms();
    let mut stmt = conn.prepare(
        "SELECT route, window_ends_ms, COUNT(*) FROM route_runs
         WHERE accounted=0 AND window_ends_ms > ?1 GROUP BY route, window_ends_ms
         ORDER BY window_ends_ms",
    )?;
    let rows = stmt.query_map([now], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (route, ends, count) = row?;
        let used = usage.entry(route).or_default();
        if used.runs_window_ends_ms.is_none_or(|end| end <= now) {
            used.runs = 0;
            used.runs_window_ends_ms = Some(ends);
        }
        if used.runs_window_ends_ms == Some(ends) {
            used.runs += count;
        }
        if used.window_ends_ms.is_none_or(|end| end <= now) {
            used.window_ends_ms = Some(ends);
        }
    }
    Ok(())
}

/// Called only inside reserve's IMMEDIATE transaction, after resource checks.
pub(crate) fn admit(
    conn: &Connection,
    key: &str,
    route: &str,
    who: Who,
    policy: Option<&Value>,
    created_ms: i64,
) -> Result<()> {
    let limits = policy.map_or_else(Limits::default, |p| {
        crate::crew_routes::named_limits(p, route)
    });
    let now = Store::now_ms();
    if let Some(reason) = refusal_at(conn, route, who, &limits, now)? {
        return Err(anyhow::Error::new(crate::admission::Refused {
            message: reason.clone(),
            blocker: None,
        })
        .context(AdmissionChanged(reason)));
    }
    let allowance = policy
        .and_then(|p| p["routes"].as_object())
        .and_then(|routes| {
            routes
                .values()
                .find(|r| crate::crew_routes::identity(r).as_deref() == Some(route))
        })
        .and_then(crate::crew_routes::allowance);
    let ends = allowance
        .filter(|a| a.runs.is_some())
        .map(|a| {
            let used = usage(conn)?.remove(route).unwrap_or_default();
            Ok::<_, anyhow::Error>(
                used.runs_window_ends_ms
                    .filter(|end| *end > now)
                    .unwrap_or_else(|| window_ends(a.shape, created_ms)),
            )
        })
        .transpose()?;
    conn.execute(
        "INSERT INTO route_runs(key,route,created_ms,window_ends_ms) VALUES(?1,?2,?3,?4)",
        params![key, route, created_ms, ends],
    )?;
    // Keep replay markers for the same period as completed usage. A live claim
    // is never pruned; a failed unsupervised start is settled by reconciliation.
    conn.execute(
        "DELETE FROM route_runs WHERE accounted=1 AND created_ms < ?1",
        [now - USAGE_KEPT_MS],
    )?;
    Ok(())
}

/// A failed run that never reached the provider spends nothing. Keep a marker
/// so replay after the cursor's interrupted commit remains idempotent.
pub(super) fn ignored(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("UPDATE route_runs SET accounted=1 WHERE key=?1", [key])?;
    Ok(())
}

pub(crate) fn forget(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM route_runs WHERE key=?1 AND accounted=0", [key])?;
    Ok(())
}
