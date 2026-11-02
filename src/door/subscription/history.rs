//! Operation-separated bounded history handles and receipts. Never invokes
//! automatic selection, pins, progress or compaction, even for the same origin.
use super::*;

pub(super) fn reserve(
    conn: &Connection,
    sub: &Subscription,
    key: &str,
    snapshot: &str,
    now: i64,
) -> Result<()> {
    conn.execute("DELETE FROM desk_subscription_history_receipt WHERE subscription=?1 AND batch IS NULL AND NOT EXISTS(SELECT 1 FROM desk_subscription_snapshot s WHERE s.subscription=?1 AND s.id=json_extract(outcome,'$.snapshot'))", [&sub.id])?;
    let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM desk_subscription_history_receipt WHERE key=?1 AND subscription=?2 AND incarnation=?3)", params![key,sub.id,sub.incarnation], |r|r.get(0))?;
    if !exists {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM desk_subscription_history_receipt WHERE subscription=?1",
            [&sub.id],
            |r| r.get(0),
        )?;
        ensure!(count < RECEIPTS, "backpressure");
        conn.execute("INSERT INTO desk_subscription_history_receipt(key,subscription,incarnation,expires,outcome) VALUES(?1,?2,?3,?4,?5)", params![key,sub.id,sub.incarnation,now.saturating_add(RECEIPT_TTL),json!({"snapshot":snapshot}).to_string()])?;
    }
    Ok(())
}

pub(super) fn acknowledged(conn: &Connection, sub: &Subscription, o: &Observation) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM desk_subscription_history_ref WHERE subscription=?1 AND incarnation=?2 AND core=?3 AND mailbox=?4 AND mail=?5 AND source_order=?6 AND acked=1)", params![sub.id,sub.incarnation,o.origin.core,o.origin.mailbox,o.origin.id,o.order], |r|r.get(0))?)
}

pub(super) fn cleanup(conn: &Connection, sub: &Subscription) -> Result<()> {
    // History is a retained-window lookup, not automatic pending custody. No
    // prefix or raw automatic ack can release/satisfy a history handle.
    conn.execute("DELETE FROM desk_subscription_history_ref AS r WHERE subscription=?1 AND NOT EXISTS(SELECT 1 FROM desk_subscription_snapshot s,json_each(s.body,'$.rows') o WHERE s.subscription=r.subscription AND json_extract(s.body,'$.history')=1 AND json_extract(o.value,'$.origin.core')=r.core AND json_extract(o.value,'$.origin.mailbox')=r.mailbox AND json_extract(o.value,'$.origin.id')=r.mail AND json_extract(o.value,'$.order')=r.source_order)", [&sub.id])?;
    Ok(())
}

pub(super) fn apply(
    conn: &Connection,
    sub: &Subscription,
    m: &Manifest,
    c: &DelegatedContext,
    key: &str,
    refs: &[String],
    now: i64,
) -> Result<Option<Value>> {
    let saved: Option<(Option<String>,Option<String>)> = conn.query_row("SELECT batch,outcome FROM desk_subscription_history_receipt WHERE key=?1 AND subscription=?2 AND incarnation=?3 AND expires>?4", params![key,sub.id,sub.incarnation,now], |r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((batch, outcome)) = saved else {
        return Ok(None);
    };
    let proposed = serde_json::to_string(refs)?;
    if let Some(batch) = batch {
        ensure!(batch == proposed, "invalid_ack");
        return Ok(Some(serde_json::from_str(
            &outcome.context("stale_or_unknown")?,
        )?));
    }
    for reference in refs {
        let body: Option<String> = conn.query_row("SELECT observation FROM desk_subscription_history_ref WHERE ref=?1 AND subscription=?2 AND incarnation=?3", params![reference,sub.id,sub.incarnation], |r|r.get(0)).optional()?;
        let o: Observation = serde_json::from_str(&body.context("invalid_ack")?)?;
        ensure!(visible(m, c, &o), "invalid_ack");
    }
    for reference in refs {
        conn.execute(
            "UPDATE desk_subscription_history_ref SET acked=1 WHERE ref=?1",
            [reference],
        )?;
    }
    let result = json!({"acknowledged":refs.len(),"operation_complete":true,"operation":"history"});
    let snapshot_expiry: Option<i64> = conn.query_row("SELECT expires FROM desk_subscription_snapshot WHERE subscription=?1 AND id=json_extract(?2,'$.snapshot')", params![sub.id,outcome], |r|r.get(0)).optional()?;
    conn.execute(
        "UPDATE desk_subscription_history_receipt SET batch=?2,outcome=?3,expires=?4 WHERE key=?1",
        params![
            key,
            proposed,
            result.to_string(),
            now.saturating_add(SNAPSHOT_TTL)
                .max(snapshot_expiry.unwrap_or(i64::MIN))
        ],
    )?;
    Ok(Some(result))
}
