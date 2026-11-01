//! Bounded negative evidence and transactionally maintained admission counters.
use super::*;

const RANGES: i64 = 16;

pub(super) fn init(c: &Connection) -> Result<()> {
    // Stale upgrade counters must not underflow while releasing old details.
    for table in [
        "desk_replay_import",
        "desk_replay_anchor",
        "desk_replay_witness",
        "desk_replay_omission",
    ] {
        c.execute_batch(&format!(
            "DROP TRIGGER IF EXISTS {table}_insert; DROP TRIGGER IF EXISTS {table}_delete;"
        ))?;
    }
    // Upgrade the first source-only schema without discarding lasting gaps.
    let old: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('desk_replay_omission') WHERE name='first_order' AND pk=0)",
        [], |r| r.get(0),
    )?;
    if old {
        c.execute_batch("ALTER TABLE desk_replay_omission RENAME TO desk_replay_omission_old;")?;
    }
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS desk_replay_omission(
         domain TEXT NOT NULL, source TEXT NOT NULL, binding TEXT NOT NULL,
         first_order INTEGER NOT NULL, last_order INTEGER NOT NULL,
         reason TEXT NOT NULL, enclosing INTEGER NOT NULL CHECK(enclosing IN (0,1)),
         PRIMARY KEY(domain,source,binding,reason,first_order));
         CREATE TABLE IF NOT EXISTS desk_replay_budget(name TEXT PRIMARY KEY, used INTEGER NOT NULL CHECK(used>=0));",
    )?;
    if old {
        c.execute_batch(
            "INSERT INTO desk_replay_omission SELECT * FROM desk_replay_omission_old;
                         DROP TABLE desk_replay_omission_old;",
        )?;
    }
    // Old source-only binaries retained baseline/lifecycle details forever.
    // Release only non-candidates with no outstanding witness custody.
    c.execute_batch(
        "DELETE FROM desk_replay_import AS i
        WHERE NOT EXISTS(SELECT 1 FROM desk_replay_witness w
            WHERE w.domain=i.domain AND w.source=i.source AND w.intent=i.intent)
        AND NOT EXISTS(SELECT 1 FROM desk_replay_anchor a
            WHERE a.domain=i.domain AND a.source=i.source AND a.origin=i.origin
            AND a.first_order=i.journal_order AND a.binding=i.binding
            AND a.digest=i.digest AND a.home=i.home AND a.nonce=i.nonce
            AND a.mail_order=i.mail_order AND a.lane='delta'
            AND i.lane='delta' AND i.native=1 AND i.delta_order IS NOT NULL);",
    )?;
    for table in [
        "desk_replay_import",
        "desk_replay_anchor",
        "desk_replay_witness",
        "desk_replay_omission",
    ] {
        // Only an upgrade/initialization scans tables. Triggers cover every
        // insert/delete (including cleanup), in the same registry transaction.
        c.execute_batch(&format!(
            "INSERT INTO desk_replay_budget SELECT '{table}',COUNT(*) FROM {table} WHERE true
             ON CONFLICT(name) DO UPDATE SET used=excluded.used;
             CREATE TRIGGER IF NOT EXISTS {table}_insert AFTER INSERT ON {table} BEGIN
             UPDATE desk_replay_budget SET used=used+1 WHERE name='{table}'; END;
             CREATE TRIGGER IF NOT EXISTS {table}_delete AFTER DELETE ON {table} BEGIN
             UPDATE desk_replay_budget SET used=used-1 WHERE name='{table}'; END;"
        ))?;
    }
    // Readiness forces existing 1a registries through this counter repair once.
    c.execute(
        "INSERT OR IGNORE INTO desk_replay_budget VALUES('replay_v3',0)",
        [],
    )?;
    Ok(())
}

pub(super) fn used(c: &Connection, table: &str) -> Result<i64> {
    Ok(c.query_row(
        "SELECT used FROM desk_replay_budget WHERE name=?1",
        [table],
        |r| r.get(0),
    )?)
}

/// Keep exact ranges separately per reason, bridging only orders that retain
/// no candidate or witness detail (e.g. non-native and lifecycle imports). Only after a key's
/// range budget fills may it become an explicitly enclosing unknown range.
/// The global row budget has a reserved fallback per reason, never a merged
/// multiple-reasons span. These are not baseline or positive evidence.
pub(super) fn omitted(
    c: &Connection,
    m: &Manifest,
    source: &str,
    order: i64,
    why: &str,
) -> Result<()> {
    let bound = binding(m)?;
    let key_exists: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM desk_replay_omission WHERE domain=?1 AND source=?2 AND binding=?3 AND reason=?4)",
        params![m.domain,source,bound,why], |r| r.get(0),
    )?;
    let fallback = !key_exists && used(c, "desk_replay_omission")? >= DETAILS;
    let (domain, source, bound) = if fallback {
        ("*", "*", "*")
    } else {
        (m.domain.as_str(), source, bound.as_str())
    };
    let rows: Vec<(i64, i64, bool)> = c
        .prepare(
            "SELECT first_order,last_order,enclosing FROM desk_replay_omission
         WHERE domain=?1 AND source=?2 AND binding=?3 AND reason=?4 ORDER BY first_order",
        )?
        .query_map(params![domain, source, bound, why], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut merge = Vec::with_capacity(rows.len());
    for (first, last, _) in &rows {
        let (lower, upper) = if order < *first {
            (order, *first)
        } else {
            (*last, order)
        };
        let blocked: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM desk_replay_import
                WHERE domain=?1 AND source=?2 AND journal_order>?3 AND journal_order<?4)
             OR EXISTS(SELECT 1 FROM desk_replay_witness
                WHERE domain=?1 AND source=?2
                AND json_extract(body,'$.journal_order')>?3 AND json_extract(body,'$.journal_order')<?4)",
            params![domain,source,lower,upper], |r| r.get(0),
        )?;
        merge.push(!blocked);
    }
    let can_merge = merge.iter().any(|yes| *yes);
    let widen = fallback
        || rows.iter().any(|r| r.2)
        || (rows.len() as i64 >= RANGES && !can_merge)
        || (used(c, "desk_replay_omission")? >= DETAILS && !can_merge);
    let mut first = order;
    let mut last = order;
    for ((a, b, _), merge) in rows.into_iter().zip(merge) {
        if widen || merge {
            first = first.min(a);
            last = last.max(b);
            c.execute("DELETE FROM desk_replay_omission WHERE domain=?1 AND source=?2 AND binding=?3 AND reason=?4 AND first_order=?5",
                params![domain,source,bound,why,a])?;
        }
    }
    c.execute(
        "INSERT INTO desk_replay_omission VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![domain, source, bound, first, last, why, widen],
    )?;
    Ok(())
}

#[cfg(test)]
pub(super) fn range_budget() -> i64 {
    RANGES
}
