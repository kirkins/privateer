//! Automatic contacts share one monotonic deadline. Explicit operator calls
//! retain their configured per-call limits. Budget exhaustion is not a failed
//! contact: work not attempted remains due, with durable rotation across peers
//! and across directory, mail and captain phases.
use super::*;
use std::cell::Cell;
use std::time::Instant;

const MIN_SLICE: Duration = Duration::from_millis(250);

#[derive(Clone, Copy)]
pub(super) enum Budget<'a> {
    Call(Option<Duration>),
    Pass {
        deadline: Instant,
        // Original pass budget, only for judging whether a timeout is fair.
        // The remaining deadline, never this duration, bounds the RPC.
        allowance: Duration,
        reached: &'a Cell<bool>,
    },
}

#[derive(Debug)]
pub(super) struct Exhausted;

impl std::fmt::Display for Exhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("automatic machine budget exhausted")
    }
}
impl std::error::Error for Exhausted {}

/// A failure before a valid remote response, distinct from a remote refusal
/// or local registry work performed around the RPC.
#[derive(Debug)]
pub(super) struct TransportFailure(pub(super) anyhow::Error);

impl std::fmt::Display for TransportFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}
impl std::error::Error for TransportFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

fn call_timeout(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<crate::peers::CallTimeout>())
}

fn transport_failure(error: &anyhow::Error) -> bool {
    call_timeout(error) || error.chain().any(|cause| cause.is::<TransportFailure>())
}

/// A local captain-phase error before any remote response. Report it without
/// changing the link's schedule or claiming recovery.
#[derive(Debug)]
struct Unreached(anyhow::Error);

impl std::fmt::Display for Unreached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}
impl std::error::Error for Unreached {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

impl Budget<'_> {
    fn minimum_slice(self) -> Duration {
        match self {
            Self::Pass { allowance, .. } => MIN_SLICE.min(allowance / 2),
            Self::Call(_) => MIN_SLICE,
        }
    }

    pub(super) fn call(
        self,
        conn: &Connection,
        machine: &Machine,
        command: &str,
        args: Value,
    ) -> Result<Value> {
        // Keep the default aligned with peers::call_config.
        let normal_ms = machine.peer["timeout_ms"].as_u64().unwrap_or(10_000);
        let timeout = match self {
            Self::Call(timeout) => timeout,
            Self::Pass { deadline, .. } => {
                let left = deadline.saturating_duration_since(now());
                if left.is_zero() || left < self.minimum_slice() {
                    return Err(Exhausted.into());
                }
                Some(left.min(Duration::from_millis(normal_ms)))
            }
        };
        #[cfg(test)]
        let result = tests::intercept(machine, command, &args, timeout)
            .unwrap_or_else(|| call(machine, command, args, timeout));
        #[cfg(not(test))]
        let result = call(machine, command, args, timeout);
        if let Self::Pass {
            allowance, reached, ..
        } = self
        {
            // call() only returns a non-transport error after receiving a
            // remote refusal. It proves contact just as a successful reply does.
            if result.is_ok()
                || result
                    .as_ref()
                    .is_err_and(|error| !transport_failure(error))
            {
                reached.set(true);
            }
            // Real links allow 30 seconds per call, longer than a default
            // pass. Judge a fair slice against the smaller allowance, with
            // one floor of slack for spawn/registry overhead after delivery.
            let fair =
                (Duration::from_millis(normal_ms).min(allowance) / 2).saturating_sub(MIN_SLICE);
            let truncated = result
                .as_ref()
                .is_err_and(|error| call_timeout(error) && timeout.is_some_and(|cap| cap < fair));
            if truncated {
                // Persist every cut-short timeout before deferring. No budget
                // configuration may exempt a dead link from backoff forever.
                // The contact lock serializes this per-link streak, including
                // across dispatcher restarts. No-call deferrals leave it alone.
                let streak: i64 = conn.query_row(
                    "UPDATE machines SET exhausted_timeouts=MIN(COALESCE(exhausted_timeouts,0)+1,3)
                     WHERE name=?1 RETURNING exhausted_timeouts",
                    [&machine.name],
                    |r| r.get(0),
                )?;
                if streak < 3 {
                    return Err(Exhausted.into());
                }
            }
        }
        // Any response, or a real failure (including the third truncated
        // timeout), breaks the streak. Reset per RPC, not per contact:
        // a successful exchange before a late collect timeout matters.
        // Explicit operator calls also prove reachability or a real failure.
        // Load the current counter per RPC, since earlier calls in this
        // contact may have reset it. Avoid acquiring a write lock in the
        // common NULL case. Maintenance must never replace a remote reply
        // (or its original error) with a local registry failure.
        let reset = (|| -> Result<()> {
            let streak: Option<i64> = conn.query_row(
                "SELECT exhausted_timeouts FROM machines WHERE name=?1",
                [&machine.name],
                |r| r.get(0),
            )?;
            if streak.is_some() {
                conn.execute(
                    "UPDATE machines SET exhausted_timeouts=NULL
                     WHERE name=?1 AND exhausted_timeouts IS NOT NULL",
                    [&machine.name],
                )?;
            }
            Ok(())
        })();
        if let Err(error) = reset {
            desk_intake::transport::audit(conn, &machine.name, "timeout_streak.reset", &error);
        }
        result
    }
}

pub(super) fn now() -> Instant {
    #[cfg(test)]
    if let Some(now) = tests::clock() {
        return now;
    }
    Instant::now()
}

pub(crate) fn delivery_deadline(
    home: &std::path::Path,
    start: Instant,
    budget: Duration,
) -> Instant {
    let due = || -> Result<bool> {
        let path = crate::fleet::registry_path()?;
        if !path.is_file() || !ever_linked(&path) {
            return Ok(false);
        }
        let conn = crate::fleet::registry()?;
        due_contact(&conn, &path, home)
    };
    delivery_end(start, budget, due().unwrap_or(false))
}

fn due_contact(conn: &Connection, path: &std::path::Path, home: &std::path::Path) -> Result<bool> {
    if !registered_home(conn, home)? {
        return Ok(false);
    }
    // This nonblocking probe briefly owns each available contact lock. A
    // sibling tick racing the probe may skip that link for one pass; it stays
    // due and will be retried, since probing never schedules the contact.
    for machine in machines(conn)? {
        if machine.role == "collects"
            && machine
                .next_contact_ms
                .is_none_or(|at| at <= Store::now_ms())
            && contact_lock(path, &machine.name)?.is_some()
        {
            return Ok(true);
        }
    }
    // A sibling dispatcher holding the link lock already owns its work.
    // Do not halve every home's delivery reservation while it is busy.
    Ok(false)
}

fn delivery_end(start: Instant, budget: Duration, due: bool) -> Instant {
    start + if due { budget / 2 } else { budget }
}

pub(super) fn registered_home(conn: &Connection, home: &std::path::Path) -> Result<bool> {
    let Ok(home) = fs::canonicalize(home) else {
        return Ok(false);
    };
    Ok(crate::fleet::members(conn)?
        .iter()
        .any(|m| fs::canonicalize(&m.home).is_ok_and(|registered| registered == home)))
}

pub(super) fn contact_lock(path: &std::path::Path, name: &str) -> Result<Option<fs::File>> {
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_file_name(format!("{}.lock", peer_name(name))))?;
    Ok(
        (unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0)
            .then_some(lock),
    )
}

pub(super) fn ordered(conn: &Connection) -> Result<Vec<Machine>> {
    let mut reaching: Vec<Machine> = machines(conn)?
        .into_iter()
        .filter(|m| m.role == "collects")
        .collect();
    let order: std::collections::HashMap<String, i64> = conn
        .prepare("SELECT name, contact_order FROM machines")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    // A link may be removed between the two reads. Never panic in a resident.
    reaching.sort_by_key(|machine| order.get(&machine.name).copied().unwrap_or(0));
    Ok(reaching)
}

pub(super) fn contact(
    conn: &Connection,
    machine: &Machine,
    deadline: Instant,
    allowance: Duration,
    carried: &OnceCell<Option<Value>>,
) -> Result<Value> {
    let reached = Cell::new(false);
    let budget = Budget::Pass {
        deadline,
        allowance,
        reached: &reached,
    };
    let first: i64 = conn.query_row(
        "SELECT contact_phase FROM machines WHERE name=?1",
        [&machine.name],
        |r| r.get(0),
    )?;
    let mut report = json!({"machine":machine.name});
    for offset in 0..4 {
        let left = deadline.saturating_duration_since(now());
        if left.is_zero() || left < budget.minimum_slice() {
            report["deferred"] = json!("network budget exhausted");
            break;
        }
        let phase = (first + offset) % 4;
        // Save the next phase before I/O: a restart or a slow/refused refresh
        // cannot keep collection (or captain mail) behind it forever.
        conn.execute(
            "UPDATE machines SET contact_phase=?2,
               contact_order=(SELECT COALESCE(MAX(contact_order),0)+1 FROM machines)
               WHERE name=?1",
            params![machine.name, (phase + 1) % 4],
        )?;
        let outcome = match phase {
            0 => {
                let current = super::machine(conn, &machine.name)?;
                if current
                    .refreshed_ms
                    .is_none_or(|at| Store::now_ms() - at >= REFRESH_MS)
                {
                    exchange(conn, machine, budget, false).map(|_| json!({}))
                } else {
                    Ok(json!({}))
                }
            }
            1 => collect(conn, machine, budget, carried),
            2 | 3 => match desk(conn).unwrap_or(None) {
                Some((name, since)) if name == machine.name => {
                    push_captain(conn, machine, since, budget, phase == 3)
                }
                _ => Ok(json!({})),
            },
            _ => unreachable!(),
        };
        match outcome {
            Err(error) if error.is::<Exhausted>() => {
                // A phase may already have done useful work (collecting a
                // batch or replaying a receipt). Keep the successor first so
                // a busy collection cannot starve captain mail. A collection
                // needing a directory explicitly selects refresh instead.
                report["deferred"] = json!("network budget exhausted");
                break;
            }
            // Non-transport refresh refusal must still allow collection.
            Err(error) if phase == 0 && !call_timeout(&error) => {}
            Err(error) if phase >= 2 && !transport_failure(&error) => {
                // A captain refusal or local recording error is not evidence
                // of an unreachable link. Surface it without link backoff.
                if !reached.get() {
                    return Err(Unreached(error).into());
                }
                report["error"] = json!(format!("{error:#}"));
                return Ok(report);
            }
            Err(error) => {
                // Collection records its own failure, including explicit
                // calls. Other phases invalidate reachability only for an
                // actual transport failure (including the timeout backstop),
                // never for a remote refusal or local registry error.
                if phase != 1 && transport_failure(&error) {
                    desk_intake::transport::failed_contact(conn, &machine.name);
                }
                return Err(error);
            }
            Ok(value) => {
                if let Some(fields) = value.as_object() {
                    for (key, value) in fields {
                        if key == "skipped_homes" {
                            let kept = report
                                .as_object_mut()
                                .unwrap()
                                .entry(key)
                                .or_insert_with(|| json!([]))
                                .as_array_mut()
                                .unwrap();
                            for home in value.as_array().into_iter().flatten() {
                                if !kept.contains(home) {
                                    kept.push(home.clone());
                                }
                            }
                        } else {
                            report[key] = value.clone();
                        }
                    }
                }
            }
        }
    }
    if report["deferred"].is_string() && reached.get() && machine.failures > 0 {
        // Successful partial contact proves recovery, even if later work was
        // deferred. Keep it due instead of scheduling away unfinished phases.
        conn.execute(
            "UPDATE machines SET failures=0 WHERE name=?1",
            [&machine.name],
        )?;
        link_back(conn, &machine.name);
    }
    Ok(report)
}

/// Only completed contacts are scheduled; budget deferrals remain due.
pub(super) fn record(
    conn: &Connection,
    machine: &Machine,
    outcome: Result<Value>,
) -> Result<Value> {
    if outcome
        .as_ref()
        .is_ok_and(|report| report["deferred"].is_string())
    {
        return outcome;
    }
    if let Err(error) = &outcome
        && error.is::<Unreached>()
    {
        return Ok(json!({"machine": machine.name, "error": format!("{error:#}")}));
    }
    schedule(conn, machine, outcome.is_ok())?;
    if outcome.is_ok() && machine.failures > 0 {
        link_back(conn, &machine.name);
    }
    Ok(outcome
        .unwrap_or_else(|error| json!({"machine": machine.name, "error": format!("{error:#}")})))
}

/// A delivered batch remains locally durable if its receipt could not be
/// sent within the pass. Replay it before the next collection; both delivery
/// and receipts are idempotent. Bind it to the exact link incarnation.
pub(super) fn acknowledge(conn: &Connection, machine: &Machine, budget: Budget<'_>) -> Result<()> {
    let raw: Option<String> = conn.query_row(
        "SELECT collect_receipt FROM machines WHERE name=?1",
        [&machine.name],
        |r| r.get(0),
    )?;
    let Some(raw) = raw else { return Ok(()) };
    let receipt: Value = serde_json::from_str(&raw)?;
    if receipt["incarnation"] == link_incarnation(conn, &machine.name)? {
        budget.call(conn, machine, "fleet.collected", receipt["body"].clone())?;
    }
    conn.execute(
        "UPDATE machines SET collect_receipt=NULL WHERE name=?1 AND collect_receipt=?2",
        params![machine.name, raw],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
