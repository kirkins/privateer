//! Durable launch claims, separate from completed usage. Admission writes these
//! under the resource ledger's write lock. A scan transfers a claim to completed
//! usage in the same transaction that marks it accounted, so a dispatcher crash
//! before its home cursor commit cannot spend the run twice.
use super::*;

pub(super) fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS route_runs (
           key TEXT PRIMARY KEY, route TEXT NOT NULL, created_ms INTEGER NOT NULL,
           window_ends_ms INTEGER, accounted INTEGER NOT NULL DEFAULT 0, accounted_ms INTEGER);
         CREATE INDEX IF NOT EXISTS route_runs_route ON route_runs(route, accounted);",
    )?;
    if conn.prepare("SELECT accounted_ms FROM route_runs").is_err() {
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        if tx.prepare("SELECT accounted_ms FROM route_runs").is_err() {
            tx.execute_batch("ALTER TABLE route_runs ADD COLUMN accounted_ms INTEGER;")?;
            tx.execute(
                "UPDATE route_runs SET accounted_ms=?1 WHERE accounted=1",
                [Store::now_ms()],
            )?;
        }
        tx.commit()?;
    }
    Ok(())
}

/// The caller has already validated this runtime resume. Read its durable
/// incarnation rather than guessing a key from a reusable job id.
pub(crate) fn predecessor_key(store: &Store, resume_of: Option<&str>) -> Result<Option<String>> {
    resume_of
        .map(|id| {
            let incarnation: String = store.connection().query_row(
                "SELECT json_extract(record,'$.incarnation') FROM jobs WHERE id=?1",
                [id],
                |r| r.get(0),
            )?;
            Ok(crate::jobs::reservation_key(store.home(), id, &incarnation))
        })
        .transpose()
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
    exclude: Option<&str>,
) -> Result<()> {
    let now = Store::now_ms();
    let mut stmt = conn.prepare(
        "SELECT route, window_ends_ms, COUNT(*) FROM route_runs
         WHERE accounted=0 AND window_ends_ms > ?1 AND (?2 IS NULL OR key != ?2)
         GROUP BY route, window_ends_ms
         ORDER BY window_ends_ms",
    )?;
    let rows = stmt.query_map(params![now, exclude], |r| {
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
    predecessor: Option<&str>,
) -> Result<()> {
    // A parked predecessor never spends a completed run. Settle it in the
    // same transaction as its continuation, so a refusal restores the claim.
    if let Some(key) = predecessor {
        ignored(conn, key)?;
    }
    let limits = policy.map_or_else(Limits::default, |p| {
        crate::crew_routes::named_limits(p, route)
    });
    let now = Store::now_ms();
    if let Some(reason) = refusal_at(conn, route, who, &limits, now)? {
        return Err(anyhow::Error::new(crate::admission::Refused {
            message: reason.clone(),
            blocker: Some(crate::admission::Blocker::Route),
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
    Ok(())
}

/// A failed run that never reached the provider spends nothing. Keep a marker
/// so replay after the cursor's interrupted commit remains idempotent.
pub(super) fn ignored(conn: &Connection, key: &str) -> Result<()> {
    conn.execute(
        "UPDATE route_runs SET accounted=1, accounted_ms=COALESCE(accounted_ms,?2) WHERE key=?1",
        params![key, Store::now_ms()],
    )?;
    Ok(())
}

/// Do not prune replay markers while an unresolved claim can pin scan's
/// lookback before them. Retention starts at settlement, not launch: even a
/// very old orphan can finish today and be replayed tomorrow.
pub(super) fn prune(conn: &Connection) -> Result<()> {
    conn.execute(
        "DELETE FROM route_runs WHERE accounted=1 AND accounted_ms < ?1
         AND NOT EXISTS (SELECT 1 FROM route_runs WHERE accounted=0 AND created_ms < ?1)",
        [Store::now_ms() - USAGE_KEPT_MS],
    )?;
    Ok(())
}

pub(crate) fn forget(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM route_runs WHERE key=?1 AND accounted=0", [key])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusing_a_continuation_rolls_back_its_predecessor_settlement() {
        let conn = Connection::open_in_memory().unwrap();
        super::super::init(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE reservations(key TEXT PRIMARY KEY, record TEXT NOT NULL);
            INSERT INTO reservations VALUES('holder','{\"model_route\":\"codex/-/example\"}');
            INSERT INTO route_runs(key,route,created_ms) VALUES('parked','codex/-/example',0);
            BEGIN IMMEDIATE;",
        )
        .unwrap();
        let policy = json!({"routes":{"limited":{"manifest":"bundled:codex",
            "options":{"model":"example"},"max_running":1}}});
        let error = admit(
            &conn,
            "continuation",
            "codex/-/example",
            Who::Crew,
            Some(&policy),
            Store::now_ms(),
            Some("parked"),
        )
        .unwrap_err();
        assert!(error.downcast_ref::<AdmissionChanged>().is_some());
        conn.execute_batch("ROLLBACK").unwrap();
        let accounted: bool = conn
            .query_row(
                "SELECT accounted FROM route_runs WHERE key='parked'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !accounted,
            "refused admission must not settle another job's claim"
        );
    }

    #[test]
    fn an_old_orphan_keeps_replay_markers_until_the_scan_can_advance() {
        let conn = Connection::open_in_memory().unwrap();
        super::super::init(&conn).unwrap();
        let now = Store::now_ms();
        let ancient = now - 2 * USAGE_KEPT_MS;
        conn.execute(
            "INSERT INTO route_runs(key,route,created_ms) VALUES('orphan','codex/-/example',?1)",
            [ancient],
        )
        .unwrap();
        let failure = || {
            Some(RouteFailure {
                error: "429 too many requests".into(),
                reset_ms: None,
                allowance: false,
                account: false,
                ceiling_ms: None,
                probe_step: None,
            })
        };
        record_job_once(&conn, "codex/-/example", failure(), None, Some("finished")).unwrap();
        conn.execute(
            "UPDATE route_runs SET created_ms=?1, accounted_ms=?1 WHERE key='finished'",
            [ancient + 1],
        )
        .unwrap();
        prune(&conn).unwrap();
        record_job_once(&conn, "codex/-/example", failure(), None, Some("finished")).unwrap();
        let failures: i64 = conn
            .query_row(
                "SELECT failures FROM model_routes WHERE route='codex/-/example'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            failures, 1,
            "a pinned scan must not replay a settled failure's backoff"
        );
        ignored(&conn, "orphan").unwrap();
        prune(&conn).unwrap();
        let kept: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM route_runs WHERE key='finished'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            kept, 0,
            "old markers may be pruned once lookback is no longer pinned"
        );
    }
}
