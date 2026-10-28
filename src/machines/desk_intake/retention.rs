//! Conservative reclamation, not eviction. Only exact final desk receipts
//! authorize log cleanup. Home mail, origins, copy evidence and desk facts are
//! never deleted here. An unsafe prefix stops cleanup and eventually admission.
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

/// At most one page of work; run only with the exact receipted final page.
/// Other retained barriers may still need the old log: do not reclaim it.
/// Receipt proofs still needed by a home also pin their corresponding event.
pub(super) fn acknowledged(
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
    let mut stmt = tx.prepare(
        "SELECT seq,event,EXISTS(SELECT 1 FROM desk_home_receipt r
        WHERE r.domain=e.domain AND r.source=e.source AND r.intent=e.intent),
        EXISTS(SELECT 1 FROM desk_source_event n WHERE n.domain=e.domain AND n.source=e.source
          AND n.seq>e.seq AND n.seq<=?3
          AND json_extract(n.event,'$.origin.core')=json_extract(e.event,'$.origin.core')
          AND json_extract(n.event,'$.origin.mailbox')=json_extract(e.event,'$.origin.mailbox')
          AND json_extract(n.event,'$.origin.id')=json_extract(e.event,'$.origin.id'))
        FROM desk_source_event e WHERE domain=?1 AND source=?2 AND seq<=?3 ORDER BY seq LIMIT 50",
    )?;
    let rows = stmt
        .query_map(params![m.domain, p.checkpoint.source, p.through], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
                r.get::<_, bool>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    let mut through = None;
    for (seq, text, pinned, superseded) in rows {
        ensure!(
            std::time::Instant::now() < deadline,
            "backpressure: source reclamation time budget"
        );
        // A later observation of this exact origin must itself be inside
        // the final receipted range. Never use an unreceived closure/ack to
        // discard custody, and always protect the newest live observation.
        if pinned || (!superseded && !disposable(&serde_json::from_str(&text)?)) {
            break;
        }
        through = Some(seq);
    }
    if let Some(through) = through {
        tx.execute(
            "DELETE FROM desk_source_event WHERE domain=?1 AND source=?2 AND seq<=?3",
            params![m.domain, p.checkpoint.source, through],
        )?;
        tx.execute(
            "UPDATE desk_source_head SET reclaimed_through=MAX(reclaimed_through,?3)
            WHERE domain=?1 AND source=?2",
            params![m.domain, p.checkpoint.source, through],
        )?;
    }
    // AUTOINCREMENT retains the global issuance fence in sqlite_sequence;
    // reclaim only issuance rows whose final-receipt barrier was reclaimed.
    tx.execute(
        "DELETE FROM desk_source_issuance WHERE domain=?1 AND source=?2 AND NOT EXISTS
        (SELECT 1 FROM desk_source_barrier b WHERE b.domain=desk_source_issuance.domain
        AND b.source=desk_source_issuance.source AND b.id=desk_source_issuance.id)",
        params![m.domain, p.checkpoint.source],
    )?;
    tx.commit()?;
    Ok(())
}
