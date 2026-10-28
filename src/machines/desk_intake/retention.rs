//! Conservative reclamation, not eviction. Exact final desk receipts authorize
//! cleanup of redundant logs, never home mail, origins, copies or desk facts.
use super::*;

pub(super) fn head(conn: &Connection, m: &Manifest, source: &str) -> Result<i64> {
    let bound = binding(m)?;
    conn.execute(
        "INSERT OR IGNORE INTO desk_source_head(domain,source,binding,upper_seq)
        SELECT ?1,?2,?3,COALESCE(MAX(seq),0) FROM desk_source_event WHERE domain=?1 AND source=?2",
        params![m.domain, source, bound],
    )?;
    let (old, upper): (String, i64) = conn.query_row(
        "SELECT binding,upper_seq FROM desk_source_head WHERE domain=?1 AND source=?2",
        params![m.domain, source],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(old == bound, "stale_generation");
    Ok(upper)
}

pub(super) fn disposable(f: &Fact) -> bool {
    !f.unread && !f.pending && f.state.as_deref() != Some("open")
}

pub(super) fn interrupted_or_busy(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        matches!(cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(code, _)) if matches!(code.code,
                rusqlite::ErrorCode::OperationInterrupted | rusqlite::ErrorCode::DatabaseBusy
                | rusqlite::ErrorCode::DatabaseLocked))
    })
}

/// Visit at most 50 rows, with indexed per-origin lookups only for rows actually
/// reached. A persistent keyset cursor lets cleanup pass permanently live facts.
/// Other retained barriers and home retry receipts still pin their custody.
pub(super) fn acknowledged(
    conn: &Connection,
    m: &Manifest,
    p: &Page,
    deadline: std::time::Instant,
) -> Result<()> {
    match acknowledged_batch(conn, m, p, deadline) {
        Err(e) if interrupted_or_busy(&e) => Ok(()),
        other => other,
    }
}

fn acknowledged_batch(
    conn: &Connection,
    m: &Manifest,
    p: &Page,
    deadline: std::time::Instant,
) -> Result<()> {
    ensure!(
        p.checkpoint.binding == binding(m)? && p.through == p.checkpoint.upper,
        "invalid reclamation barrier"
    );
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    head(&tx, m, &p.checkpoint.source)?;
    let other: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM desk_source_barrier
        WHERE domain=?1 AND source=?2 AND id<>?3)",
        params![m.domain, p.checkpoint.source, p.checkpoint.id],
        |r| r.get(0),
    )?;
    if other {
        return Ok(());
    }
    let (cursor, old_upper): (i64, i64) = tx.query_row(
        "SELECT COALESCE((SELECT scan_seq FROM desk_source_cleanup WHERE domain=?1 AND source=?2),0),
                COALESCE((SELECT upper_seq FROM desk_source_cleanup WHERE domain=?1 AND source=?2),0)",
        params![m.domain, p.checkpoint.source], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let upper = if old_upper == 0 {
        p.through
    } else {
        old_upper.min(p.through)
    };
    let mut stmt = tx.prepare(
        "SELECT seq,event,intent FROM desk_source_event
         WHERE domain=?1 AND source=?2 AND seq>?3 AND seq<=?4 ORDER BY seq LIMIT 50",
    )?;
    let rows = stmt
        .query_map(params![m.domain, p.checkpoint.source, cursor, upper], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    let empty = rows.is_empty();
    let mut scanned = cursor;
    let mut pruned = 0;
    for (seq, text, intent) in rows {
        if std::time::Instant::now() >= deadline {
            break;
        }
        let pinned: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM desk_home_receipt WHERE domain=?1 AND source=?2 AND intent=?3)",
            params![m.domain, p.checkpoint.source, intent], |r| r.get(0))?;
        scanned = seq;
        if pinned {
            continue;
        }
        let f: Fact = serde_json::from_str(&text)?;
        let safe = disposable(&f)
            || tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM desk_source_event WHERE domain=?1 AND source=?2
             AND json_extract(event,'$.origin.core')=?3
             AND json_extract(event,'$.origin.mailbox')=?4
             AND json_extract(event,'$.origin.id')=?5 AND seq>?6 AND seq<=?7)",
                params![
                    m.domain,
                    p.checkpoint.source,
                    f.origin.core,
                    f.origin.mailbox,
                    f.origin.id,
                    seq,
                    p.through
                ],
                |r| r.get::<_, bool>(0),
            )?;
        // Preserve a later observation as supersession proof until earlier
        // same-origin rows are gone, even when the cursor reaches it first.
        let earlier: bool = safe
            && tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM desk_source_event WHERE domain=?1 AND source=?2
             AND json_extract(event,'$.origin.core')=?3
             AND json_extract(event,'$.origin.mailbox')=?4
             AND json_extract(event,'$.origin.id')=?5 AND seq<?6)",
                params![
                    m.domain,
                    p.checkpoint.source,
                    f.origin.core,
                    f.origin.mailbox,
                    f.origin.id,
                    seq
                ],
                |r| r.get::<_, bool>(0),
            )?;
        if safe && !earlier {
            tx.execute(
                "DELETE FROM desk_source_event WHERE domain=?1 AND source=?2 AND seq=?3",
                params![m.domain, p.checkpoint.source, seq],
            )?;
            pruned = seq;
        }
    }
    let (next, next_upper) = if empty || scanned >= upper {
        (0, 0)
    } else {
        (scanned, upper)
    };
    if next != cursor || next_upper != old_upper || pruned != 0 {
        tx.execute(
            "INSERT INTO desk_source_cleanup(domain,source,scan_seq,pruned_through,upper_seq) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(domain,source) DO UPDATE SET scan_seq=excluded.scan_seq,upper_seq=excluded.upper_seq,
             pruned_through=MAX(pruned_through,excluded.pruned_through)",
            params![m.domain, p.checkpoint.source, next, pruned, next_upper])?;
    }
    if pruned != 0 {
        // Only the contiguous absent prefix is a reclaimed prefix. Sparse
        // deletions also fence old page attempts: they must resume at the
        // already-receipted checkpoint, not manufacture a history gap.
        tx.execute(
            "UPDATE desk_source_head SET reclaimed_through=MAX(reclaimed_through,
             COALESCE((SELECT MIN(seq)-1 FROM desk_source_event WHERE domain=?1 AND source=?2),?3))
             WHERE domain=?1 AND source=?2",
            params![m.domain, p.checkpoint.source, p.through],
        )?;
    }
    tx.execute(
        "DELETE FROM desk_source_issuance WHERE domain=?1 AND source=?2 AND NOT EXISTS
        (SELECT 1 FROM desk_source_barrier b WHERE b.domain=desk_source_issuance.domain
        AND b.source=desk_source_issuance.source AND b.id=desk_source_issuance.id)",
        params![m.domain, p.checkpoint.source],
    )?;
    tx.commit()?;
    Ok(())
}
