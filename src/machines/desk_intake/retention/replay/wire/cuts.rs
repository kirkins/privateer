//! Immutable desk-owned replay boundaries. Only enable and legal contacts
//! write these records. Classification is read-only and never selects mail.
use super::*;

mod classify;
#[cfg(test)]
mod tests;
pub(crate) use classify::ReadContext;
pub(in crate::machines::desk_intake) use classify::classify;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct SavedCut {
    pub(super) checkpoint: Checkpoint,
    pub(super) vector_digest: String,
    pub(super) home: Home,
    received_at: i64,
    session: String,
    tick_ms: i64,
    adopted_at: i64,
    age_at_adoption_ms: i64,
    max_source_age_ms: i64,
    delayed: bool,
    switch_on_ms: i64,
}
impl SavedCut {
    fn age(&self, clock: &Clock<'_>) -> Option<i64> {
        measured_age(clock, &self.session, self.received_at, self.tick_ms)
    }
}
fn measured_age(clock: &Clock<'_>, session: &str, received: i64, tick: i64) -> Option<i64> {
    let age = clock.wall_ms.checked_sub(received)?;
    let elapsed = clock.tick_ms.checked_sub(tick)?;
    (!session.is_empty()
        && clock.session == session
        && age >= 0
        && elapsed >= 0
        && age.abs_diff(elapsed) <= 1000)
        .then_some(age)
}
pub(super) fn matches_binding(b: &Value, m: &Manifest) -> Result<bool> {
    Ok(b["domain"] == m.domain
        && b["owner_core"] == m.owner_core
        && b["generation"] == m.generation
        && b["incarnation"] == m.incarnation
        && b["manifest_revision"] == m.manifest_revision
        && b["manifest_hash"] == crate::core::hash(&json!(m))
        && b["replay_policy"] == "open_and_new"
        && b["switch_on_ms"].as_i64().is_some())
}
fn exists(c: &Connection, table: &str) -> Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |r| r.get(0),
    )?)
}
fn load(c: &Connection, key: &str, source: &str) -> Result<Vec<SavedCut>> {
    // Diagnostics are bounded even after many home/enrollment transitions.
    c.prepare("SELECT body FROM desk_exposure_cut WHERE binding=?1 AND source=?2 ORDER BY home,mailbox,nonce LIMIT 51")?
        .query_map(params![key,source], |r|r.get::<_,String>(0))?
        .map(|s| Ok(serde_json::from_str(&s?)?)).collect()
}
fn find(c: &Connection, key: &str, source: &str, home: &Home) -> Result<Option<SavedCut>> {
    let raw: Option<String> = c.query_row("SELECT body FROM desk_exposure_cut WHERE binding=?1 AND source=?2 AND home=?3 AND mailbox=?4 AND nonce=?5",
        params![key,source,home.home,home.mailbox,home.nonce],|r|r.get(0)).optional()?;
    raw.map(|raw| Ok(serde_json::from_str(&raw)?)).transpose()
}
fn order_cut(
    c: &Connection,
    key: &str,
    source: &str,
    native: Option<(&str, i64, Option<&Witness>)>,
) -> Result<Option<SavedCut>> {
    // Separate indexed queries avoid scanning cuts from past enrollments.
    let raw: Option<String> = if let Some((mailbox, order, Some(w))) = native {
        c.query_row("SELECT body FROM desk_exposure_cut WHERE binding=?1 AND source=?2 AND home=?3 AND mailbox=?4 AND nonce=?5 AND journal_upper>=?6",
            params![key,source,w.home,mailbox,w.nonce,order],|r|r.get(0)).optional()?
    } else if let Some((mailbox, order, None)) = native {
        c.query_row("SELECT body FROM desk_exposure_cut WHERE binding=?1 AND source=?2 AND mailbox=?3 AND journal_upper>=?4 ORDER BY journal_upper LIMIT 1",
            params![key,source,mailbox,order],|r|r.get(0)).optional()?
    } else {
        c.query_row("SELECT body FROM desk_exposure_cut WHERE binding=?1 AND source=?2 ORDER BY journal_upper LIMIT 1",params![key,source],|r|r.get(0)).optional()?
    };
    raw.map(|raw| Ok(serde_json::from_str(&raw)?)).transpose()
}
fn latest(c: &Connection, m: &Manifest, source: &str) -> Result<Option<Batch>> {
    let raw: Option<String> = c
        .query_row(
            "SELECT body FROM desk_replay_vector WHERE domain=?1 AND source=?2 AND binding=?3",
            params![m.domain, source, binding(m)?],
            |r| r.get(0),
        )
        .optional()?;
    raw.map(|s| Ok(serde_json::from_str(&s)?)).transpose()
}
// Revalidate the snapshot against the latest accepted v2 vector, not just its
// old valid bit. Local homes also require the existing bounded live recheck.
fn current(c: &Connection, m: &Manifest, source: &str, b: &Batch) -> Result<bool> {
    let raw: Option<String> = c
        .query_row(
            "SELECT body FROM desk_intake_proof WHERE domain=?1 AND source=?2 AND valid=1",
            params![m.domain, source],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let Some(raw) = raw else { return Ok(false) };
    let e: proof::Evidence = serde_json::from_str(&raw)?;
    if json!(e) != json!(b.evidence) || !valid_batch(c, m, source, b)? {
        return Ok(false);
    }
    let gap: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM desk_intake_gap WHERE domain=?1 AND source=?2)",
        params![m.domain, source],
        |r| r.get(0),
    )?;
    if gap {
        return Ok(false);
    }
    if source == m.owner_core {
        return Ok(matches!(
            proof::local_current(c, m, source, transport::contact_deadline())?,
            proof::LocalCurrent::Current
        ));
    }
    Ok(true)
}

pub(in crate::machines::desk_intake) fn capture(
    c: &Connection,
    m: &Manifest,
    b: &Value,
) -> Result<()> {
    capture_at(c, m, b, false, &transport::process_clock())
}
pub(super) fn capture_at(
    c: &Connection,
    m: &Manifest,
    b: &Value,
    delayed: bool,
    clock: &Clock<'_>,
) -> Result<()> {
    if !matches_binding(b, m)? || !exists(c, "desk_exposure_cut")? {
        return Ok(());
    }
    for source in &m.sources {
        let Some(latest) = latest(c, m, &source.core)? else {
            continue;
        };
        adopt(c, m, b, &source.core, &latest, delayed, clock)?;
    }
    Ok(())
}
fn adopt(
    c: &Connection,
    m: &Manifest,
    b: &Value,
    source: &str,
    latest: &Batch,
    delayed: bool,
    clock: &Clock<'_>,
) -> Result<()> {
    let key = crate::core::hash(b);
    // Check only this bounded vector's exact keys, never historical cuts.
    let missing = latest
        .homes
        .iter()
        .map(|h| Ok((h, find(c, &key, source, h)?.is_none())))
        .collect::<Result<Vec<_>>>()?;
    // Aged boundaries are kept forever; only missing enrollment tuples need
    // freshness. A newer home vector never replaces an established cut.
    if missing.iter().all(|(_, missing)| !missing) {
        return Ok(());
    }
    if !current(c, m, source, latest)? {
        return Ok(());
    }
    let receipt: Option<(i64,String,i64,bool)> = c.query_row(
        "SELECT received_at,session,tick_ms,clock_valid FROM desk_intake_checkpoint WHERE domain=?1 AND source=?2 AND barrier=?3 AND upper_seq=through_seq AND received_at IS NOT NULL AND session IS NOT NULL AND tick_ms IS NOT NULL",
        params![m.domain,source,latest.evidence.checkpoint.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let Some((received_at, session, tick_ms, true)) = receipt else {
        return Ok(());
    };
    let Some(age) = measured_age(clock, &session, received_at, tick_ms)
        .filter(|age| *age <= m.max_source_age_ms)
    else {
        return Ok(());
    };
    let enabled = b["switch_on_ms"]
        .as_i64()
        .context("missing switch-on time")?;
    // Never revive a pre-enable checkpoint that was unusable at enable.
    // Same-millisecond contacts are fenced by their immutable checkpoint ID.
    if delayed
        && (received_at < enabled
            || b["switch_on"].as_array().is_some_and(|v| {
                v.iter().any(|s| {
                    s["source"] == source
                        && s["through"].as_str().is_some_and(|through| {
                            through
                                .rsplit_once(':')
                                .is_some_and(|(id, _)| id == latest.evidence.checkpoint.id)
                        })
                })
            }))
    {
        return Ok(());
    }
    for (h, missing) in missing {
        if !missing {
            continue;
        }
        let cut = SavedCut {
            checkpoint: latest.evidence.checkpoint.clone(),
            vector_digest: latest.vector_digest.clone(),
            home: h.clone(),
            received_at,
            session: session.clone(),
            tick_ms,
            adopted_at: clock.wall_ms,
            age_at_adoption_ms: age,
            max_source_age_ms: m.max_source_age_ms,
            delayed,
            switch_on_ms: enabled,
        };
        revision::write(c, || {
            Ok(c.execute(
                "INSERT OR IGNORE INTO desk_exposure_cut VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    key,
                    source,
                    h.home,
                    h.mailbox,
                    h.nonce,
                    cut.checkpoint.upper,
                    serde_json::to_string(&cut)?
                ],
            )?)
        })?;
    }
    Ok(())
}
fn same_home(a: &Home, b: &Home) -> bool {
    a.home == b.home && a.mailbox == b.mailbox && a.nonce == b.nonce
}

// Called inside the provisional-adoption transaction, after the latest vector
// is saved. Failure rolls back optional proof only, never ordinary page custody.
pub(super) fn contact(c: &Connection, m: &Manifest, source: &str, latest: &Batch) -> Result<()> {
    if !exists(c, "door_desk_exposure")? || !exists(c, "desk_exposure_cut")? {
        return Ok(());
    }
    let bindings = c
        .prepare("SELECT binding FROM door_desk_exposure ORDER BY door LIMIT 51")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(bindings.len() <= 50, "backpressure: replay bindings");
    let active: Vec<_> = bindings
        .iter()
        .filter_map(|raw| serde_json::from_str::<Value>(raw).ok())
        .map(|b| crate::core::hash(&b))
        .collect();
    // Retired door bindings cannot keep retry cursors forever. Never remove
    // their adopted evidence or additive links, only disposable scan state.
    c.execute(
        "DELETE FROM desk_replay_link_retry WHERE rowid IN
        (SELECT rowid FROM desk_replay_link_retry WHERE domain=?1 AND exposure NOT IN
         (SELECT value FROM json_each(?2)) LIMIT 50)",
        params![m.domain, serde_json::to_string(&active)?],
    )?;
    let clock = transport::process_clock();
    let mut active = Vec::new();
    for raw in bindings {
        if let Ok(b) = serde_json::from_str::<Value>(&raw)
            && matches_binding(&b, m)?
        {
            active.push(b);
        }
    }
    let mut remaining = 50_usize;
    let count = active.len();
    for (index, b) in active.into_iter().enumerate() {
        adopt(c, m, &b, source, latest, true, &clock)?;
        for w in &latest.witnesses {
            classify::link(c, m, &b, w)?;
        }
        // A single contact budget, shared fairly even when every binding
        // has a full retry queue. Each binding retains its own keyset cursor.
        let share = remaining / (count - index);
        remaining -= retry_links(c, m, &b, source, share)?;
    }
    Ok(())
}

// Keyset rotation bounds both SQL work and live validation to 50 retained
// copy-first witnesses in total per contact. Visit linked entries too: an
// anti-join LIMIT could scan the entire adopted store. The retry index ends
// in SQLite's implicit rowid, and the cursor survives contacts/restarts.
fn retry_links(
    c: &Connection,
    m: &Manifest,
    b: &Value,
    source: &str,
    limit: usize,
) -> Result<usize> {
    let key = crate::core::hash(b);
    let cursor: i64 = c
        .query_row(
            "SELECT cursor FROM desk_replay_link_retry WHERE domain=?1 AND exposure=?2 AND source=?3",
            params![m.domain, key, source],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    let mut remaining = limit;
    let mut next = cursor;
    for sql in [
        "SELECT rowid,body FROM desk_replay_adopted INDEXED BY desk_replay_adopted_retry WHERE domain=?1 AND binding=?2 AND core=?3 AND first_kind='copy' AND rowid>?4 ORDER BY rowid LIMIT ?5",
        "SELECT rowid,body FROM desk_replay_adopted INDEXED BY desk_replay_adopted_retry WHERE domain=?1 AND binding=?2 AND core=?3 AND first_kind='copy' AND rowid<=?4 ORDER BY rowid LIMIT ?5",
    ] {
        let rows = c
            .prepare(sql)?
            .query_map(
                params![m.domain, binding(m)?, source, cursor, remaining],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        remaining -= rows.len();
        for (rowid, raw) in rows {
            next = rowid;
            let w: Witness = serde_json::from_str(&raw)?;
            let linked: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM desk_exposure_upgrade WHERE binding=?1 AND core=?2 AND mailbox=?3 AND id=?4)",
                params![key,w.origin.core,w.origin.mailbox,w.origin.id], |r|r.get(0))?;
            if !linked {
                classify::link(c, m, b, &w)?;
            }
        }
    }
    if next != cursor {
        c.execute("INSERT INTO desk_replay_link_retry VALUES(?1,?2,?3,?4,?5) ON CONFLICT(domain,exposure,source) DO UPDATE SET binding=excluded.binding,cursor=excluded.cursor",
            params![m.domain,binding(m)?,key,source,next])?;
    }
    Ok(limit - remaining)
}

pub(in crate::machines::desk_intake) fn diagnostics(
    c: &Connection,
    m: &Manifest,
    b: &Value,
) -> Result<Value> {
    let key = crate::core::hash(b);
    let clock = transport::process_clock();
    let mut sources = Vec::new();
    if !exists(c, "desk_exposure_cut")? {
        return Ok(json!({"sources":[],"state":"switch_on_pending"}));
    }
    for source in &m.sources {
        let mut saved = load(c, &key, &source.core)?;
        let truncated = saved.len() > 50;
        saved.truncate(50);
        let current = latest(c, m, &source.core)?;
        let mut pending = current.is_none();
        if let Some(current) = current {
            for h in &current.homes {
                pending |= find(c, &key, &source.core, h)?.is_none();
            }
        }
        let cuts: Vec<_> = saved.iter().map(|s|json!({"checkpoint":s.checkpoint,"home":s.home,"received_at":s.received_at,
            "age_ms":s.age(&clock),"age_at_adoption_ms":s.age_at_adoption_ms,"max_source_age_ms":s.max_source_age_ms,
            "adopted_at":s.adopted_at,"delayed":s.delayed,"unproved_interval":if s.delayed {json!({"from_ms":s.switch_on_ms,"through_received_at":s.received_at})} else {Value::Null}})).collect();
        sources.push(json!({"source":source.core,"state":if pending {"switch_on_pending"} else {"established"},"cuts":cuts,"retained_cuts_omitted":truncated}));
    }
    Ok(json!({"sources":sources}))
}
