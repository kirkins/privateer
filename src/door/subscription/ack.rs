use super::*;
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn reserve_key(conn: &Connection, sub: &Subscription, now: i64) -> Result<String> {
    let key = uuid();
    reserve_named(conn, sub, &key, now)?;
    Ok(key)
}
pub(super) fn reserve_named(
    conn: &Connection,
    sub: &Subscription,
    key: &str,
    now: i64,
) -> Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM desk_subscription_receipt WHERE key=?1 AND subscription=?2)",
        params![key, sub.id],
        |r| r.get(0),
    )?;
    if exists {
        return Ok(());
    }
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM desk_subscription_receipt WHERE subscription=?1",
        [&sub.id],
        |r| r.get(0),
    )?;
    ensure!(count < RECEIPTS, "backpressure");
    conn.execute(
        "INSERT INTO desk_subscription_receipt(key,subscription,expires) VALUES(?1,?2,?3)",
        params![key, sub.id, now.saturating_add(RECEIPT_TTL)],
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
    conn.execute(
        "UPDATE desk_subscription_receipt SET batch=?2,outcome=?3 WHERE key=?1",
        params![key, batch, serde_json::to_string(&result)?],
    )?;
    Ok(result)
}

fn compact(
    conn: &Connection,
    sub: &Subscription,
    m: &Manifest,
    c: &DelegatedContext,
) -> Result<()> {
    // Prove the prefix against ALL eligible projected observations, not just
    // this page/thread. Include older pending versions displaced by intake.
    let mut observations = intake::observations(conn, m)?;
    let mut stmt =
        conn.prepare("SELECT observation FROM desk_subscription_ref WHERE subscription=?1")?;
    for row in stmt.query_map([&sub.id], |r| r.get::<_, String>(0))? {
        observations.push(serde_json::from_str(&row?)?);
    }
    observations.sort_by(|a, b| {
        (&a.origin.core, &a.origin.mailbox, a.order, &a.origin.id).cmp(&(
            &b.origin.core,
            &b.origin.mailbox,
            b.order,
            &b.origin.id,
        ))
    });
    let mut blocked = BTreeSet::new();
    let mut prefixes = BTreeMap::new();
    for o in &observations {
        if !visible(m, c, o) {
            continue;
        }
        let origin = (o.origin.core.clone(), o.origin.mailbox.clone());
        if blocked.contains(&origin) {
            continue;
        }
        if acknowledged(conn, sub, o)? {
            prefixes.insert(origin, o.order);
        } else {
            blocked.insert(origin);
        }
    }
    for ((core, mailbox), order) in prefixes {
        conn.execute("INSERT INTO desk_subscription_progress(subscription,core,mailbox,through_order) VALUES(?1,?2,?3,?4) ON CONFLICT(subscription,core,mailbox) DO UPDATE SET through_order=MAX(through_order,excluded.through_order)",params![sub.id,core,mailbox,order])?;
        conn.execute("DELETE FROM desk_subscription_ref WHERE subscription=?1 AND core=?2 AND mailbox=?3 AND source_order<=?4 AND acked=1",params![sub.id,core,mailbox,order])?;
    }
    Ok(())
}
