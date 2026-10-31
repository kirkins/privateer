//! Source-only, dated observation status. Never a subscription or fleet claim.
//! Derive every result; no persisted `complete` bit can outlive its predicates.
use super::*;
use crate::mail::source;
use std::time::Instant;

pub(in crate::machines) fn status(conn: &Connection, m: &Manifest) -> Vec<Value> {
    let deadline = transport::contact_deadline();
    let run = || -> Result<Vec<Value>> {
        let _read_only = ReadOnly::new(conn)?;
        let _budget = transport::Budget::new(conn, deadline)?;
        let (_fence, current) = super::super::desk_placement::intake_manifest(conn)?;
        // One registry snapshot for validity, origin comparison and clock
        // evidence; concurrent invalidation/import cannot be mixed piecemeal.
        let read = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
        let conn = &read;
        ensure!(binding(m)? == binding(&current)?, "stale_generation");
        ensure!(
            crate::door::reader_link::machine_id_in(conn)? == m.owner_core,
            "not_owner"
        );
        let clock = transport::process_clock();
        let sources = evaluate_sources(conn, m, &clock, deadline);
        // A read-only snapshot needs no commit; expiry during teardown must
        // not discard source results already evaluated successfully.
        drop(read);
        Ok(sources)
    };
    run().unwrap_or_else(|_| {
        m.sources
            .iter()
            .map(|s| fallback(conn, m, &s.core, "placement_or_status_unavailable"))
            .collect()
    })
}

// source_evidence retains a writable dormant API for other callers. Operator
// evaluation must not persist invalidation or acquire a registry writer lock.
pub(super) struct ReadOnly<'a> {
    conn: &'a Connection,
    previous: bool,
}
impl<'a> ReadOnly<'a> {
    pub(super) fn new(conn: &'a Connection) -> Result<Self> {
        let previous = conn.query_row("PRAGMA query_only", [], |r| r.get(0))?;
        conn.pragma_update(None, "query_only", true)?;
        Ok(Self { conn, previous })
    }
}
impl Drop for ReadOnly<'_> {
    fn drop(&mut self) {
        let _ = self.conn.pragma_update(None, "query_only", self.previous);
    }
}

fn retained_facts(conn: &Connection, m: &Manifest, source: &str, out: &mut Value) -> Result<()> {
    if let Some(mut homes) = proof::retained_homes(conn, m, source)? {
        for home in homes.as_array_mut().into_iter().flatten() {
            home["facts_cached"] = json!(true);
        }
        out["homes"] = homes;
    }
    Ok(())
}
fn fallback(conn: &Connection, m: &Manifest, source: &str, reason: &str) -> Value {
    let mut out = json!({"id":source,"scope":"source","complete":false,"state":"unavailable","reason":reason});
    let _ = retained_facts(conn, m, source, &mut out);
    out
}
fn evaluate_sources(
    conn: &Connection,
    m: &Manifest,
    clock: &Clock<'_>,
    deadline: Instant,
) -> Vec<Value> {
    // Retain dated counts before any individual local revalidation can consume
    // the budget. A single malformed/unavailable source never hides its peers.
    let fallbacks: Vec<_> = m
        .sources
        .iter()
        .map(|s| fallback(conn, m, &s.core, "source_status_unavailable"))
        .collect();
    m.sources
        .iter()
        .zip(fallbacks)
        .map(|(s, mut fallback)| {
            let expired = || source::work_now() >= deadline;
            if !expired()
                && let Ok(out) = snapshot(conn, m, &s.core, clock, deadline)
            {
                return out;
            }
            if expired() {
                fallback["reason"] = json!("status_budget_exhausted");
            }
            fallback
        })
        .collect()
}

// Placement is checked once by status under its nonblocking shared fence.
// Tests also call this private evaluator with a supplied clock and manifest.
pub(super) fn snapshot(
    conn: &Connection,
    m: &Manifest,
    source: &str,
    clock: &Clock<'_>,
    deadline: Instant,
) -> Result<Value> {
    let (evidence, _) = source_evidence(conn, m, source, clock)?;
    let mut out = json!({"id":source,"scope":"source","complete":false,"state":evidence.state,"reason":"proof_unavailable","through":evidence.through});
    if let Some(b) = &evidence.barrier {
        out["barrier_mode"] = json!(b.mode);
        out["received_at"] = json!(b.received_at);
        out["age_ms"] = json!(b.age_ms);
        out["max_source_age_ms"] = json!(b.max_source_age_ms);
        out["as_of"] = json!(if b.mode == "last_received_push" {
            "last_source_contact"
        } else {
            "source_barrier"
        });
    }
    // Preserve last-known home counts even if current proof/contact is absent.
    // Copy payloads and paths are never included in these operator diagnostics.
    retained_facts(conn, m, source, &mut out)?;
    let reject = |mut out: Value, state: &str, reason: &str| {
        out["state"] = json!(state);
        out["reason"] = json!(reason);
        out
    };
    if evidence.state == "stale_generation" {
        return Ok(reject(
            out,
            "stale_generation",
            "checkpoint_binding_changed",
        ));
    }
    let direction = m
        .sources
        .iter()
        .find(|s| s.core == source)
        .context("unknown source")?
        .intake_direction
        .as_str();
    if source != m.owner_core {
        let peer: Option<(String, i64)> = conn.query_row(
            "SELECT m.role,m.failures FROM machines m JOIN desk_placement_peer p ON p.machine=m.name WHERE p.core=?1", [source], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some((role, failures)) = peer else {
            return Ok(reject(out, "unavailable", "source_contact_missing"));
        };
        if !matches!(
            (direction, role.as_str()),
            ("collect", "collects") | ("push", "collected")
        ) {
            return Ok(reject(out, "stale_generation", "intake_direction_changed"));
        }
        if failures > 0 {
            return Ok(reject(out, "unavailable", "source_unreachable"));
        }
        let supported: i64 = conn.query_row("SELECT COALESCE((SELECT supported FROM desk_intake_transport WHERE domain=?1 AND source=?2 AND binding=?3),0)", params![m.domain,source,binding(m)?], |r|r.get(0))?;
        if supported != 1 {
            return Ok(reject(
                out,
                if supported == 0 {
                    "unsupported"
                } else {
                    "backpressure"
                },
                "source_contact_not_validated",
            ));
        }
    } else if direction != "local" {
        return Ok(reject(out, "stale_generation", "intake_direction_changed"));
    }
    if !topology_proved(conn, m)? {
        return Ok(reject(out, "history_gap", "unknown_membership"));
    }
    // `candidate` proves home history independently of the old diagnostic
    // history_proved flag, which deliberately includes recoverable obstacles.
    // eligible() then checks copies against authoritative origin content and
    // the independent sticky loss record. No diagnostic is cleared here.
    if !proof::proved(conn, m, source)? {
        return Ok(out);
    }
    // Remote homes were revalidated by that source on its last legal contact.
    // Our own homes can change during idle time without a contact; recheck them
    // read-only, in a separate bounded subphase, before using the old barrier.
    if source == m.owner_core {
        match proof::local_current(conn, m, source, deadline)? {
            proof::LocalCurrent::Current => {}
            proof::LocalCurrent::Changed => {
                return Ok(reject(out, "history_gap", "local_vector_changed"));
            }
            proof::LocalCurrent::Budget => {
                return Ok(reject(
                    out,
                    "backpressure",
                    "local_revalidation_budget_exhausted",
                ));
            }
        }
    }
    if !fresh(conn, m, source, clock)? {
        return Ok(reject(out, "cached", "contact_not_fresh"));
    }
    out["complete"] = json!(true);
    out["state"] = json!("covered");
    out["reason"] = json!("proved_source_barrier");
    Ok(out)
}

pub(super) fn topology_proved(conn: &Connection, m: &Manifest) -> Result<bool> {
    let mut peers = conn.prepare("SELECT m.name,p.core FROM machines m LEFT JOIN desk_placement_peer p ON p.machine=m.name LIMIT 51")?;
    let rows = peers
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.len() > 50
        || rows.iter().any(|(_, core)| {
            core.as_ref()
                .is_none_or(|core| !m.sources.iter().any(|s| &s.core == core))
        })
    {
        return Ok(false);
    }
    let outside: bool = conn
        .prepare("SELECT machine,name FROM machine_ships LIMIT 51")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .enumerate()
        .any(|(i, row)| {
            i >= 50
                || row.map_or(true, |(machine, name)| {
                    !rows.iter().any(|(peer, core)| {
                        peer == &machine
                            && m.sources
                                .iter()
                                .any(|s| Some(&s.core) == core.as_ref() && s.ships.contains(&name))
                    })
                })
        });
    Ok(!outside)
}

fn fresh(conn: &Connection, m: &Manifest, source: &str, clock: &Clock<'_>) -> Result<bool> {
    let row: Option<(i64,i64,Option<String>,i64,bool)> = conn.query_row("SELECT received_at,tick_ms,session,upper_seq=through_seq,clock_valid FROM desk_intake_checkpoint WHERE domain=?1 AND source=?2 AND received_at IS NOT NULL AND tick_ms IS NOT NULL",params![m.domain,source],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
    Ok(row.is_some_and(|(received, tick, session, done, valid)| {
        let age = clock.wall_ms.checked_sub(received);
        let elapsed = clock.tick_ms.checked_sub(tick);
        contact_fresh(
            valid
                && !clock.session.is_empty()
                && session.as_deref() == Some(clock.session)
                && matches!((age,elapsed),(Some(a),Some(e)) if e>=0 && a.abs_diff(e)<=1000),
            done == 1,
            age,
            m.max_source_age_ms,
        )
    }))
}

#[cfg(test)]
mod tests;
