use super::*;
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn reserve_key(conn: &Connection, sub: &Subscription, now: i64) -> Result<String> {
    // Open/resume share one unused standalone key rather than burning a slot
    // per reconnect. Committed outcomes remain immutable for receipt retention.
    let key: Option<String> = conn.query_row(
        "SELECT key FROM desk_subscription_receipt WHERE subscription=?1 AND batch IS NULL AND outcome IS NULL LIMIT 1",
        [&sub.id], |r| r.get(0),
    ).optional()?;
    let key = key.unwrap_or_else(uuid);
    reserve_named(conn, sub, &key, None, now)?;
    Ok(key)
}
pub(super) fn reserve_named(
    conn: &Connection,
    sub: &Subscription,
    key: &str,
    snapshot: Option<&str>,
    now: i64,
) -> Result<()> {
    // Keep committed outcomes until their expiry, even if the snapshot is
    // drained. apply retains them for an hour after commit: no shorter than
    // the lifetime of any snapshot that could reissue this deterministic key.
    // While batch is NULL, outcome holds only reservation metadata. apply
    // replaces it with the committed result atomically with the batch.
    conn.execute("DELETE FROM desk_subscription_receipt WHERE subscription=?1 AND batch IS NULL AND outcome IS NOT NULL AND NOT EXISTS(SELECT 1 FROM desk_subscription_snapshot s WHERE s.id=json_extract(desk_subscription_receipt.outcome,'$.snapshot') AND s.subscription=?1)", [&sub.id])?;
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM desk_subscription_receipt WHERE key=?1 AND subscription=?2)",
        params![key, sub.id],
        |r| r.get(0),
    )?;
    if exists {
        if snapshot.is_none() {
            // Open/resume must not hand out a nearly expired unused key.
            // Never extend or otherwise mutate a committed replay outcome.
            conn.execute("UPDATE desk_subscription_receipt SET expires=MAX(expires,?3) WHERE key=?1 AND subscription=?2 AND batch IS NULL AND outcome IS NULL", params![key,sub.id,now.saturating_add(RECEIPT_TTL)])?;
        }
        return Ok(());
    }
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM desk_subscription_receipt WHERE subscription=?1",
        [&sub.id],
        |r| r.get(0),
    )?;
    ensure!(count < RECEIPTS, "backpressure");
    conn.execute(
        "INSERT INTO desk_subscription_receipt(key,subscription,expires,outcome) VALUES(?1,?2,?3,?4)",
        params![key, sub.id, now.saturating_add(RECEIPT_TTL), snapshot.map(|id| json!({"snapshot":id}).to_string())],
    )?;
    Ok(())
}

pub(super) fn apply(
    conn: &Connection,
    sub: &Subscription,
    m: &Manifest,
    c: &DelegatedContext,
    key: &str,
    mut refs: Vec<String>,
    now: i64,
) -> Result<Value> {
    ensure!(key.len() <= 64, "stale_or_unknown");
    ensure!(
        !refs.is_empty() && refs.len() <= WINDOW as usize && refs.iter().all(|r| r.len() <= 64),
        "invalid_ack"
    );
    refs.sort();
    refs.dedup();
    let batch = serde_json::to_string(&refs)?;
    let saved: Option<(Option<String>,Option<String>)> = conn.query_row("SELECT batch,outcome FROM desk_subscription_receipt WHERE key=?1 AND subscription=?2 AND expires>?3",params![key,sub.id,now],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let (old_batch, outcome) = saved.context("stale_or_unknown")?;
    if let Some(old_batch) = old_batch {
        ensure!(old_batch == batch, "invalid_ack");
        return Ok(serde_json::from_str(&outcome.context("stale_or_unknown")?)?);
    }
    // Validate the WHOLE batch before writing even one acknowledged fact.
    for reference in &refs {
        let stored: Option<String> = conn.query_row("SELECT observation FROM desk_subscription_ref WHERE ref=?1 AND subscription=?2 AND incarnation=?3",params![reference,sub.id,sub.incarnation],|r|r.get(0)).optional()?;
        let o: Observation = serde_json::from_str(&stored.context("invalid_ack")?)?;
        ensure!(visible(m, c, &o), "invalid_ack");
    }
    for reference in &refs {
        conn.execute(
            "UPDATE desk_subscription_ref SET acked=1 WHERE ref=?1",
            [reference],
        )?;
    }
    compact(conn, sub, m, c)?;
    let result = json!({"acknowledged":refs.len(),"operation_complete":true});
    // Also pin to the actual saved snapshot expiry if the clock moved
    // backwards between reservation and commit. Otherwise a live continuation
    // could recreate an expired deterministic page key for a different batch.
    let snapshot_expiry: Option<i64> = conn.query_row("SELECT s.expires FROM desk_subscription_snapshot s JOIN desk_subscription_receipt r ON s.id=json_extract(r.outcome,'$.snapshot') AND s.subscription=r.subscription WHERE r.key=?1 AND r.subscription=?2", params![key,sub.id], |r| r.get(0)).optional()?;
    let expires = now
        .saturating_add(SNAPSHOT_TTL)
        .max(snapshot_expiry.unwrap_or(i64::MIN));
    conn.execute(
        "UPDATE desk_subscription_receipt SET batch=?2,outcome=?3,expires=?4 WHERE key=?1",
        params![key, batch, serde_json::to_string(&result)?, expires],
    )?;
    Ok(result)
}

fn compact(
    conn: &Connection,
    sub: &Subscription,
    m: &Manifest,
    c: &DelegatedContext,
) -> Result<()> {
    // Stream ALL eligible projected observations above the proved prefix,
    // plus displaced pending versions. Retain only the first unseen order and
    // sparse acknowledged orders (bounded by WINDOW), never historical bodies.
    let mut blocked: BTreeMap<(String, String), i64> = BTreeMap::new();
    let mut candidates: BTreeMap<(String, String), BTreeSet<i64>> = BTreeMap::new();
    let mut visit = |o: Observation| -> Result<()> {
        if !visible(m, c, &o) || o.order <= through(conn, sub, &o)? {
            return Ok(());
        }
        let origin = (o.origin.core.clone(), o.origin.mailbox.clone());
        if acknowledged(conn, sub, &o)? {
            candidates.entry(origin).or_default().insert(o.order);
        } else {
            blocked
                .entry(origin)
                .and_modify(|n| *n = (*n).min(o.order))
                .or_insert(o.order);
        }
        Ok(())
    };
    intake::visit_observations(conn, m, Some(&sub.id), c.ships(), None, &mut visit)?;
    let mut stmt =
        conn.prepare("SELECT observation FROM desk_subscription_ref WHERE subscription=?1")?;
    for row in stmt.query_map([&sub.id], |r| r.get::<_, String>(0))? {
        visit(serde_json::from_str(&row?)?)?;
    }
    for ((core, mailbox), orders) in candidates {
        let boundary = blocked.get(&(core.clone(), mailbox.clone()));
        let Some(order) = orders
            .iter()
            .rev()
            .find(|o| boundary.is_none_or(|b| *o < b))
        else {
            continue;
        };
        conn.execute("INSERT INTO desk_subscription_progress(subscription,core,mailbox,through_order) VALUES(?1,?2,?3,?4) ON CONFLICT(subscription,core,mailbox) DO UPDATE SET through_order=MAX(through_order,excluded.through_order)",params![sub.id,core,mailbox,order])?;
        conn.execute("DELETE FROM desk_subscription_ref WHERE subscription=?1 AND core=?2 AND mailbox=?3 AND source_order<=?4 AND acked=1",params![sub.id,core,mailbox,order])?;
    }
    // Thread windows may reissue already-acknowledged history. Release those
    // replay handles on explicit ack even if there is no new prefix candidate;
    // they never consume sparse custody slots or advance the prefix.
    conn.execute("DELETE FROM desk_subscription_ref AS r WHERE r.subscription=?1 AND r.acked=1 AND EXISTS(SELECT 1 FROM desk_subscription_progress p WHERE p.subscription=r.subscription AND p.core=r.core AND p.mailbox=r.mailbox AND p.through_order>=r.source_order)", [&sub.id])?;
    Ok(())
}
