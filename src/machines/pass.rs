//! Automatic contacts share one monotonic deadline. Explicit operator calls
//! retain their configured per-call limits. Budget exhaustion is not a failed
//! contact: work not attempted remains due, with durable rotation across peers
//! and across directory, mail and captain phases.
use super::*;
use std::time::Instant;

#[derive(Clone, Copy)]
pub(super) enum Budget {
    Call(Option<Duration>),
    Pass(Instant),
}

#[derive(Debug)]
pub(super) struct Exhausted;

impl std::fmt::Display for Exhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("automatic machine budget exhausted")
    }
}
impl std::error::Error for Exhausted {}

impl Budget {
    pub(super) fn call(self, machine: &Machine, command: &str, args: Value) -> Result<Value> {
        let timeout = match self {
            Self::Call(timeout) => timeout,
            Self::Pass(end) => {
                let left = end.saturating_duration_since(now());
                // Transport limits are expressed in whole milliseconds. Never
                // start a sub-millisecond call that would round to zero.
                if left.as_millis() == 0 {
                    return Err(Exhausted.into());
                }
                Some(left)
            }
        };
        #[cfg(test)]
        if let Some(result) = tests::intercept(machine, command, &args, timeout) {
            return result;
        }
        call(machine, command, args, timeout)
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
        Ok(crate::fleet::members(&conn)?.iter().any(|m| m.home == home)
            && conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM machines WHERE role='collects'
                   AND COALESCE(next_contact_ms,0)<=?1)",
                [Store::now_ms()],
                |r| r.get::<_, bool>(0),
            )?)
    };
    delivery_end(start, budget, due().unwrap_or(false))
}

fn delivery_end(start: Instant, budget: Duration, due: bool) -> Instant {
    start + if due { budget / 2 } else { budget }
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
    reaching.sort_by_key(|machine| order[&machine.name]);
    Ok(reaching)
}

pub(super) fn contact(
    conn: &Connection,
    machine: &Machine,
    deadline: Instant,
    carried: &OnceCell<Option<Value>>,
) -> Result<Value> {
    let budget = Budget::Pass(deadline);
    let first: i64 = conn.query_row(
        "SELECT contact_phase FROM machines WHERE name=?1",
        [&machine.name],
        |r| r.get(0),
    )?;
    let mut report = json!({"machine":machine.name});
    for offset in 0..4 {
        if deadline.saturating_duration_since(now()).as_millis() == 0 {
            report["deferred"] = json!("network budget exhausted");
            return Ok(report);
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
            2 | 3 => match desk(conn)? {
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
                return Ok(report);
            }
            // Non-transport refresh refusal must still allow collection.
            Err(error) if phase == 0 && !format!("{error:#}").contains("timed out") => {}
            Err(error) => return Err(error),
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
    Ok(report)
}

/// A delivered batch remains locally durable if its receipt could not be
/// sent within the pass. Replay it before the next collection; both delivery
/// and receipts are idempotent. Bind it to the exact link incarnation.
pub(super) fn acknowledge(conn: &Connection, machine: &Machine, budget: Budget) -> Result<()> {
    let raw: Option<String> = conn.query_row(
        "SELECT collect_receipt FROM machines WHERE name=?1",
        [&machine.name],
        |r| r.get(0),
    )?;
    let Some(raw) = raw else { return Ok(()) };
    let receipt: Value = serde_json::from_str(&raw)?;
    if receipt["incarnation"] == link_incarnation(conn, &machine.name)? {
        budget.call(machine, "fleet.collected", receipt["body"].clone())?;
    }
    conn.execute(
        "UPDATE machines SET collect_receipt=NULL WHERE name=?1 AND collect_receipt=?2",
        params![machine.name, raw],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
