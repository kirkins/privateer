//! Home-to-registry import. There is no distributed transaction:
//! prepare commits stable home intents, append commits the registry event,
//! receipt commits registry evidence, and finally the home records its receipt.
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
/// origin event, append's stable intent is itself the retry proof if a crash
/// occurs before this second registry commit. A remote-origin desk copy is
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
    if let Some(fact) = &fact
        && !is_remote_copy
    {
        append(conn, m, source, intent, fact)?;
    }
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
pub(in crate::machines) fn capture(
    home: &Connection,
    registry: &Connection,
    m: &Manifest,
    source: &str,
) -> Result<Value> {
    capture_until(home, registry, m, source, None)
}

pub(in crate::machines) fn capture_until(
    home: &Connection,
    registry: &Connection,
    m: &Manifest,
    source: &str,
    deadline: Option<std::time::Instant>,
) -> Result<Value> {
    participant(m, source)?;
    let result = (|| {
        let upper = source::snapshot_until(home, &format!("{}:{source}", binding(m)?), deadline)?;
        let lost: bool = home.query_row(
            "SELECT lost OR EXISTS(SELECT 1 FROM metadata WHERE key='mail_source_loss') FROM mail_source_health WHERE only=1",
            [],
            |r| r.get(0),
        )?;
        if lost {
            gap(registry, m, source, "history_gap")?;
        }
        source::stage_deltas(home)?;
        let mut stmt = home.prepare("SELECT intent,event FROM mail_source_import WHERE receipt IS NULL ORDER BY seq LIMIT 50")?;
        let pending = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut imported = 0;
        for (intent, event) in pending {
            ensure!(
                deadline.is_none_or(|d| std::time::Instant::now() < d),
                "backpressure: capture time budget"
            );
            let receipt = import_one(registry, m, source, &intent, &event)?;
            home.execute(
                "UPDATE mail_source_import SET receipt=?2 WHERE intent=?1 AND receipt IS NULL",
                params![intent, receipt],
            )?;
            imported += 1;
        }
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
    record_failure(registry, m, source, &result)?;
    // Missing/corrupt/locked homes are explicit gaps too, not silent skips.
    if let Err(e) = &result
        && ![
            "backpressure",
            "integrity_gap",
            "stale_generation",
            "history_gap",
        ]
        .iter()
        .any(|s| e.to_string().starts_with(s))
    {
        gap(registry, m, source, "unreadable")
            .with_context(|| format!("capture failed ({e:#}); failed recording unreadable"))?;
    }
    result
}

#[cfg(test)]
mod tests;
