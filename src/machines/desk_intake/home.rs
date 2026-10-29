//! Home-to-registry import. There is no distributed transaction:
//! prepare commits stable home intents, one registry transaction commits the
//! event and receipt evidence, and finally the home records its receipt.
//! Every intermediate crash retries the same identity and immutable content.
use super::*;
use crate::mail::source::{self, HomeFact, Provenance};
use std::path::{Path, PathBuf};

pub(crate) fn retain_home(conn: &Connection, home: &Path) -> Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM desk_home_inventory WHERE home=?1)",
        [home.to_string_lossy().as_ref()],
        |r| r.get(0),
    )?;
    if !exists {
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM desk_home_inventory", [], |r| r.get(0))?;
        ensure!(count < source::LIMIT, "capture home inventory backpressure");
        conn.execute(
            "INSERT INTO desk_home_inventory(home) VALUES(?1)",
            [home.to_string_lossy().as_ref()],
        )?;
    }
    Ok(())
}

/// Inventory is an optional projection, never a registration prerequisite.
/// Keep a bounded, independent sticky marker if a retained path cannot be
/// saved. Even a failure writing that marker must not refuse registration.
pub(crate) fn retain_home_best_effort(conn: &Connection, home: &Path) {
    if retain_home(conn, home).is_err() {
        let _ = conn.execute(
            "INSERT OR IGNORE INTO desk_home_inventory_gap(only,state) VALUES(1,'history_gap')",
            [],
        );
    }
}

#[derive(Debug)]
pub(crate) struct Home {
    pub path: PathBuf,
    pub retired: bool,
    pub state: &'static str,
}
#[derive(Debug)]
pub(crate) struct Census {
    pub homes: Vec<Home>,
    // A registry cannot prove unregistered/transitive membership, nor recover
    // pre-upgrade overwritten home paths. Never infer fleet completeness.
    pub membership_gap: bool,
    pub membership_history_gap: bool,
    pub complete: bool,
}

/// Includes retired ships and every path retained before a move/re-register.
/// No Store::open: an inventory read never creates or migrates a home.
pub(crate) fn census(conn: &Connection, skipped: &[PathBuf]) -> Result<Census> {
    census_until(conn, skipped, None)
}

pub(super) fn census_until(
    conn: &Connection,
    skipped: &[PathBuf],
    deadline: Option<std::time::Instant>,
) -> Result<Census> {
    let mut stmt = conn.prepare(
        "SELECT home,MAX(active) FROM (
        SELECT home,retired_ms IS NULL AS active FROM ships
        UNION ALL SELECT home,0 AS active FROM desk_home_inventory)
        GROUP BY home ORDER BY home LIMIT 10001",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        rows.len() <= source::LIMIT as usize,
        "backpressure: home inventory"
    );
    let mut homes = Vec::new();
    for (path, active) in rows {
        ensure!(
            deadline.is_none_or(|d| source::work_now() < d),
            "backpressure: census budget"
        );
        let path = PathBuf::from(path);
        let database = path.join("privateer.sqlite");
        let state = if skipped.contains(&path) {
            "skipped"
        } else if !database.exists() {
            "missing"
        } else if crate::store::reject_symlinks(&path).is_err()
            || crate::store::reject_symlinks(&database).is_err()
        {
            "unreadable"
        } else {
            match Connection::open_with_flags(&database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            {
                Ok(c) => {
                    c.busy_timeout(std::time::Duration::ZERO)?;
                    let _budget = deadline
                        .map(|d| super::transport::Budget::new(&c, d))
                        .transpose()?;
                    if c.query_row("SELECT COUNT(*) FROM mail", [], |r| r.get::<_, i64>(0))
                        .is_ok()
                    {
                        "retained"
                    } else {
                        "unreadable"
                    }
                }
                Err(_) => "unreadable",
            }
        };
        homes.push(Home {
            path,
            retired: !active,
            state,
        });
    }
    Ok(Census {
        homes,
        membership_gap: true,
        membership_history_gap: conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM desk_home_inventory_gap)",
            [],
            |r| r.get(0),
        )?,
        complete: false,
    })
}

/// Report census gaps durably, rather than dropping homes a directory omitted.
pub(in crate::machines) fn enumerate(
    conn: &Connection,
    m: &Manifest,
    source: &str,
    skipped: &[PathBuf],
) -> Result<Census> {
    participant(m, source)?;
    let census = census(conn, skipped)?;
    let state = if census.homes.iter().any(|h| h.state == "unreadable") {
        "unreadable"
    } else {
        "history_gap"
    };
    // Membership is still unproved even when every known home was readable.
    gap(conn, m, source, state)?;
    Ok(census)
}

fn resolved(f: &HomeFact, source: &str) -> Result<Option<Fact>> {
    let copy = MailIdentity::new(source, &f.mailbox, &f.id)?;
    let origin = match &f.provenance {
        // An unpinned local marker is not origin proof (for example a home
        // moved before source enrollment). Never relabel it from the caller.
        Provenance::Local => return Ok(None),
        Provenance::Origin(origin) => origin.clone(),
        Provenance::Copy(origin) => origin.clone(),
        Provenance::Unresolved => return Ok(None),
    };
    Ok(Some(Fact {
        copy: (origin != copy).then_some(copy),
        origin,
        content: f.content.clone(),
        state: f.state.clone(),
        unread: f.unread,
        pending: f.pending,
    }))
}

fn known_receipt(
    conn: &Connection,
    m: &Manifest,
    source: &str,
    intent: &str,
    event: &str,
) -> Result<Option<String>> {
    let row: Option<(String,String,String)> = conn.query_row("SELECT binding,event,receipt FROM desk_home_receipt WHERE domain=?1 AND source=?2 AND intent=?3", params![m.domain,source,intent], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    if let Some((bound, old, receipt)) = row {
        ensure!(bound == binding(m)?, "stale_generation");
        ensure!(old == event, "integrity_gap: conflicting home intent");
        return Ok(Some(receipt));
    }
    Ok(None)
}

/// Registry receipt bound to the exact home event and generation. For an
/// origin event, the event and retry proof commit atomically. There is no
/// reclaimable event without a receipt pin. A remote-origin desk copy is
/// kept as explicit alias/evidence, NEVER appended under the wrong source or
/// allowed to close the remote source's projected request.
fn import_one(
    conn: &Connection,
    m: &Manifest,
    source: &str,
    intent: &str,
    event: &str,
) -> Result<String> {
    participant(m, source)?;
    uuid::Uuid::parse_str(intent)?;
    if let Some(receipt) = known_receipt(conn, m, source, intent, event)? {
        return Ok(receipt);
    }
    let f: HomeFact = serde_json::from_str(event)?;
    let fact = resolved(&f, source)?;
    let is_remote_copy = fact.as_ref().is_some_and(|f| f.origin.core != source);
    let bound = binding(m)?;
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    if let Some(receipt) = known_receipt(&tx, m, source, intent, event)? {
        return Ok(receipt);
    }
    let (count,bytes): (i64,i64) = tx.query_row("SELECT COUNT(*),COALESCE(SUM(length(CAST(event AS BLOB))),0) FROM desk_home_receipt WHERE domain=?1 AND source=?2", params![m.domain,source], |r| Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(
        count < source::LIMIT * 2 && bytes + event.len() as i64 <= JOURNAL_BYTES * 2,
        "backpressure: home receipts"
    );
    if let Some(fact) = &fact
        && !is_remote_copy
    {
        append_in(&tx, m, source, intent, fact)?;
    }
    if let Some(fact) = &fact
        && is_remote_copy
    {
        // Even origins outside this fixed manifest are retained as explicit
        // copy evidence, with a gap and a retryable receipt. They cannot be
        // projected as source facts or block every later event in this home.
        // Validate identities without weakening Fact::validate(source).
        MailIdentity::new(&fact.origin.core, &fact.origin.mailbox, &fact.origin.id)?;
        let copy = fact
            .copy
            .as_ref()
            .context("integrity_gap: missing copy identity")?;
        ensure!(
            coverage::origin_of(&tx, &fact.origin)? == fact.origin,
            "integrity_gap: origin is a copy"
        );
        let was_origin: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM desk_intake_fact WHERE domain=?1 AND core=?2 AND mailbox=?3 AND id=?4)", params![m.domain,copy.core,copy.mailbox,copy.id], |r| r.get(0))?;
        ensure!(!was_origin, "integrity_gap: copy was an origin");
        ensure!(
            coverage::record_alias_in(&tx, copy, &fact.origin)? != coverage::Alias::Conflict,
            "integrity_gap: conflicting copy"
        );
        tx.execute(
            "INSERT INTO desk_home_copy(domain,source,intent,event) VALUES(?1,?2,?3,?4)",
            params![m.domain, source, intent, event],
        )?;
        gap(&tx, m, source, "history_gap")?;
    } else if fact.is_none() {
        // Keep unresolved raw evidence in the receipt, not as an invented
        // origin. An equal ID/seq/body in another home proves nothing.
        gap(&tx, m, source, "history_gap")?;
    }
    let receipt = crate::core::hash(&json!({"binding":bound,"intent":intent,"event":event}));
    tx.execute("INSERT INTO desk_home_receipt(domain,source,intent,binding,event,receipt) VALUES(?1,?2,?3,?4,?5,?6)", params![m.domain,source,intent,bound,event,receipt])?;
    tx.commit()?;
    Ok(receipt)
}

/// One bounded work unit. Caller supplies an authorized, already-existing
/// writable home connection and a fenced placement manifest. Transport uses
/// the deadline-bound variant below; no public command calls this directly.
/// Read-only opens cannot initialize any schema.
#[cfg(test)]
pub(in crate::machines) fn capture(
    home: &Connection,
    registry: &Connection,
    m: &Manifest,
    source: &str,
) -> Result<Value> {
    capture_until(home, registry, m, source, None)
}

#[cfg(test)]
pub(in crate::machines) fn capture_until(
    home: &Connection,
    registry: &Connection,
    m: &Manifest,
    source: &str,
    deadline: Option<std::time::Instant>,
) -> Result<Value> {
    capture_inner(home, registry, m, source, deadline, true)
}

pub(super) fn baseline_committed(registry: &Connection, m: &Manifest, source: &str) -> Result<()> {
    // Only the idle worker clears a per-home obstacle after publishing that
    // exact home. A quiet contact of another home cannot hide it. SELECT first:
    // even a no-op UPDATE would acquire the registry writer lock.
    let obstacle: bool = registry.query_row("SELECT EXISTS(SELECT 1 FROM desk_intake_gap WHERE domain=?1 AND source=?2 AND state='baseline_too_large')", params![m.domain,source], |r| r.get(0))?;
    if !obstacle {
        return Ok(());
    }
    let clear: bool = registry.query_row("SELECT NOT EXISTS(SELECT 1 FROM desk_baseline_home WHERE domain=?1 AND source=?2 AND state='baseline_too_large' AND home IN (SELECT home FROM ships UNION SELECT home FROM desk_home_inventory))", params![m.domain,source], |r| r.get(0))?;
    if clear {
        registry.execute("UPDATE desk_intake_gap SET state='history_gap' WHERE domain=?1 AND source=?2 AND state='baseline_too_large' AND NOT EXISTS(SELECT 1 FROM desk_baseline_home WHERE domain=?1 AND source=?2 AND state='baseline_too_large' AND home IN (SELECT home FROM ships UNION SELECT home FROM desk_home_inventory))", params![m.domain,source])?;
    }
    Ok(())
}

/// Production contacts only import a published baseline. In particular an
/// almost-exhausted contact budget is never mistaken for an oversized home.
pub(super) fn capture_ready_until(
    path: &Path,
    home: &Connection,
    registry: &Connection,
    m: &Manifest,
    source: &str,
    deadline: std::time::Instant,
) -> Result<Value> {
    let bound = format!("{}:{source}", binding(m)?);
    let (old, upper): (String, i64) = home
        .query_row(
            "SELECT binding,upper_seq FROM mail_source_snapshot WHERE only=1 AND upper_seq>=0",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .context("backpressure: baseline enrollment pending off contact")?;
    ensure!(old == bound, "stale_generation");
    if let Some(instance) = source::enrollment::published_nonce(home, &bound)? {
        let baseline = source::enrollment::open(path, false)?;
        let _budget = super::transport::Budget::new(&baseline, deadline)?;
        ensure!(
            source::enrollment::frozen(&baseline, &bound, &instance)? == Some(upper),
            "integrity_gap: missing frozen baseline"
        );
        baseline_committed(registry, m, source)?;
        let (imported, cleanup_upper) =
            import_pending(&baseline, registry, m, source, Some(deadline))?;
        // Frozen history never gains later observations. Cleanup is useful
        // while receipts advance, but rescanning its pinned live prefix on
        // every quiet contact would just rewrite cursors forever. Remaining
        // bounded baseline proofs are retained conservatively.
        if imported > 0 {
            reclaim_imports(
                &baseline,
                registry,
                m,
                source,
                cleanup_upper,
                Some(deadline),
            )?;
        }
        let pending: bool = baseline.query_row(
            "SELECT EXISTS(SELECT 1 FROM mail_source_import WHERE receipt IS NULL)",
            [],
            |r| r.get(0),
        )?;
        if imported > 0 || pending {
            return Ok(
                json!({"complete":false,"snapshot_upper":upper,"imported":imported,"baseline_pending":pending}),
            );
        }
    }
    capture_inner(home, registry, m, source, Some(deadline), false)
}

fn capture_inner(
    home: &Connection,
    registry: &Connection,
    m: &Manifest,
    source: &str,
    deadline: Option<std::time::Instant>,
    allow_snapshot: bool,
) -> Result<Value> {
    participant(m, source)?;
    let result: Result<Value> = (|| {
        if allow_snapshot {
            source::init_intake(home)?;
        } else {
            ensure!(
                source::intake_ready(home)?,
                "backpressure: intake upgrade pending off contact"
            );
        }
        let enrolled = source::enrollment::published(home)?;
        ensure!(
            enrolled || allow_snapshot,
            "backpressure: baseline enrollment pending off contact"
        );
        let upper = source::snapshot_until(home, &format!("{}:{source}", binding(m)?), deadline)
            .map_err(|e| {
                let interrupted = e.chain().any(|cause| {
                    matches!(
                        cause.downcast_ref::<rusqlite::Error>(),
                        Some(rusqlite::Error::SqliteFailure(code, _))
                            if code.code == rusqlite::ErrorCode::OperationInterrupted
                    )
                });
                if !enrolled && (interrupted || e.to_string().starts_with("backpressure")) {
                    e.context(
                        "baseline_too_large: atomic snapshot exceeds exchange budget or bounds",
                    )
                } else {
                    e
                }
            })?;
        baseline_committed(registry, m, source)?;
        let lost = source::lost(home)?;
        if lost {
            gap(registry, m, source, "history_gap")?;
        }
        // Leave part of this phase for imports even under a steady delta
        // stream; staging commits a prefix instead of restarting a 50-row
        // transaction on every slow contact.
        let staging_deadline = deadline.map(|end| {
            let now = source::work_now();
            now + end.saturating_duration_since(now) / 2
        });
        source::stage_deltas_until(home, staging_deadline)?;
        let (imported, cleanup_upper) = import_pending(home, registry, m, source, deadline)?;
        // Pending imports get priority over optional cleanup. Two batched
        // commits replace up to 100 per-row fsyncs; expiration is a quiet stop.
        reclaim_imports(home, registry, m, source, cleanup_upper, deadline)?;
        let membership_history_gap: bool = registry.query_row(
            "SELECT EXISTS(SELECT 1 FROM desk_home_inventory_gap)",
            [],
            |r| r.get(0),
        )?;
        if membership_history_gap {
            gap(registry, m, source, "history_gap")?;
        }
        Ok(
            json!({"complete":false,"snapshot_upper":upper,"imported":imported,"history_gap":lost,
            "membership":{"state":if membership_history_gap {"history_gap"} else {"unproved"}}}),
        )
    })();
    let result = result.map_err(|e| {
        if !e.to_string().starts_with("baseline_too_large")
            && super::retention::interrupted_or_busy(&e)
        {
            e.context("backpressure: capture busy or interrupted")
        } else {
            e
        }
    });
    record_failure(registry, m, source, &result)?;
    // Missing/corrupt homes are explicit gaps too, not silent skips.
    if let Err(e) = &result
        && ![
            "backpressure",
            "integrity_gap",
            "stale_generation",
            "history_gap",
            "baseline_too_large",
        ]
        .iter()
        .any(|s| e.to_string().starts_with(s))
    {
        gap(registry, m, source, "unreadable")
            .with_context(|| format!("capture failed ({e:#}); failed recording unreadable"))?;
    }
    result
}

fn import_pending(
    home: &Connection,
    registry: &Connection,
    m: &Manifest,
    source: &str,
    deadline: Option<std::time::Instant>,
) -> Result<(usize, i64)> {
    let cleanup_upper = home.query_row(
        "SELECT COALESCE(MAX(seq),0) FROM mail_source_import WHERE receipt IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    let mut stmt = home.prepare(
        "SELECT intent,event FROM mail_source_import WHERE receipt IS NULL ORDER BY seq LIMIT 50",
    )?;
    let pending = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut imported = 0;
    for (intent, event) in pending {
        if deadline.is_some_and(|d| source::work_now() >= d) {
            break;
        }
        let receipt = import_one(registry, m, source, &intent, &event)?;
        home.execute(
            "UPDATE mail_source_import SET receipt=?2 WHERE intent=?1 AND receipt IS NULL",
            params![intent, receipt],
        )?;
        imported += 1;
    }
    Ok((imported, cleanup_upper))
}

/// Discharge a bounded batch only after the home has durably recorded the
/// exact registry receipt. Delete the registry proof FIRST: a crash leaves a
/// receipted home row, which is never re-imported, and cleanup safely retries.
/// Delete home outbox+intent and adjust admission counters in one transaction.
/// Unresolved/remote copies and the newest unread/open/pending fact stay
/// pinned. Older observations may be discharged by a later receipted import.
fn reclaim_imports(
    home: &Connection,
    registry: &Connection,
    m: &Manifest,
    source: &str,
    cleanup_upper: i64,
    deadline: Option<std::time::Instant>,
) -> Result<()> {
    if deadline.is_some_and(|d| source::work_now() >= d) {
        return Ok(());
    }
    let result = reclaim_import_batch(home, registry, m, source, cleanup_upper, deadline);
    match result {
        Err(e) if super::retention::interrupted_or_busy(&e) => Ok(()),
        other => other,
    }
}

fn reclaim_import_batch(
    home: &Connection,
    registry: &Connection,
    m: &Manifest,
    source: &str,
    cleanup_upper: i64,
    deadline: Option<std::time::Instant>,
) -> Result<()> {
    let (cursor, old_upper): (i64, i64) = home.query_row(
        "SELECT COALESCE((SELECT scan_seq FROM mail_source_cleanup WHERE only=1),0),
                COALESCE((SELECT upper_seq FROM mail_source_cleanup WHERE only=1),0)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    // Freeze each sweep's upper: a continuously advancing tail must not
    // starve older rows that were receipt-pinned on the previous sweep.
    let upper = if old_upper == 0 {
        cleanup_upper
    } else {
        old_upper.min(cleanup_upper)
    };
    // Both selection and supersession use expression indexes. A durable
    // keyset cursor bounds work even with thousands of newest live facts.
    let mut stmt = home.prepare(
        "SELECT seq,intent,event,receipt FROM mail_source_import
        WHERE receipt IS NOT NULL AND seq<=?2 AND seq>?3
        AND COALESCE(json_extract(event,'$.provenance.Origin.core'),
                     json_extract(event,'$.provenance.Copy.core'))=?1
        ORDER BY seq LIMIT 50",
    )?;
    let rows = stmt
        .query_map(params![source, upper, cursor], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    if deadline.is_some_and(|d| source::work_now() >= d) {
        return Ok(());
    }
    if rows.is_empty() {
        if cursor != 0 {
            source::reclaim_imports(home, &[], 0, 0)?;
        }
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(registry, TransactionBehavior::Immediate)?;
    let mut discharged = Vec::new();
    let mut scanned = cursor;
    for (seq, intent, event, receipt) in rows {
        if deadline.is_some_and(|d| source::work_now() >= d) {
            break;
        }
        let f: HomeFact = serde_json::from_str(&event)?;
        let superseded: bool = home.query_row(
            "SELECT EXISTS(SELECT 1 FROM mail_source_import WHERE receipt IS NOT NULL
             AND json_extract(event,'$.mailbox')=?1 AND json_extract(event,'$.id')=?2
             AND json_extract(event,'$.provenance')=json(?3) AND seq>?4)",
            params![f.mailbox, f.id, serde_json::to_string(&f.provenance)?, seq],
            |r| r.get(0),
        )?;
        scanned = seq;
        if !superseded && (f.unread || f.pending || f.state.as_deref() == Some("open")) {
            continue;
        }
        let earlier: bool = home.query_row(
            "SELECT EXISTS(SELECT 1 FROM mail_source_import WHERE receipt IS NOT NULL
             AND json_extract(event,'$.mailbox')=?1 AND json_extract(event,'$.id')=?2
             AND json_extract(event,'$.provenance')=json(?3) AND seq<?4)",
            params![f.mailbox, f.id, serde_json::to_string(&f.provenance)?, seq],
            |r| r.get(0),
        )?;
        if earlier {
            continue; // Keep this later observation as proof for older rows.
        }
        if let Some(known) = known_receipt(&tx, m, source, &intent, &event)? {
            ensure!(receipt == known, "integrity_gap: home reclamation receipt");
            tx.execute(
                "DELETE FROM desk_home_receipt WHERE domain=?1 AND source=?2 AND intent=?3",
                params![m.domain, source, intent],
            )?;
        }
        discharged.push((intent, receipt));
    }
    tx.commit()?;
    let (next, next_upper) = if scanned >= upper {
        (0, 0)
    } else {
        (scanned, upper)
    };
    if !discharged.is_empty() || next != cursor || next_upper != old_upper {
        // Persist the visited prefix even when the loop used its budget.
        // Otherwise every retry can discharge the same registry proofs and
        // never advance the home cursor. SQLite's VM deadline still bounds
        // this final transaction; interruption safely retries the receipts.
        source::reclaim_imports(home, &discharged, next, next_upper)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
