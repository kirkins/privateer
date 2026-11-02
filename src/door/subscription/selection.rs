//! INACTIVE 4b-2A ledger. No admission, selector, show, ack or contact calls
//! these primitives yet. All mutations require the caller's admitted registry
//! writer transaction; any error must roll that transaction back. Visibility,
//! classification, authoritative lifecycle and snapshot byte admission remain
//! the activating caller's responsibility, not facts inferred by this ledger.
//! Every automatic ack of a span-covered order must call acknowledge_automatic
//! (or acknowledge_carried for an exception) in the ack transaction, even for
//! normal refs. Every pinned closure ack must call acknowledge_closure (also
//! done by those two helpers). History must call none of them. Normal-ref hole
//! splitting can backpressure; carried exceptions reserve their split slot.
use super::*;
pub(super) mod runtime;
mod schema;
pub(super) use schema::{clear, install, ready};

// One extra span is reserved per carried exception: acknowledging a point in
// the middle of a skipped range must always have room to split that range.
// No unbounded per-history tombstone set, TTL eviction or fabricated ack.
const SPANS: i64 = 512;
const EXCEPTION_BYTES: i64 = 16 * 1024 * 1024;
pub(super) const HISTORY: i64 = 1;
pub(super) const DELAYED: i64 = 2;
pub(super) const UNKNOWN: i64 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Span {
    pub core: String,
    pub mailbox: String,
    pub first: i64,
    pub last: i64,
    // Bitmask, not a positive assertion about any order in an enclosing span.
    pub reasons: i64,
    pub checked: i64,
    pub through: i64,
    pub scan: i64,
}
fn writer(conn: &Connection, sub: &Subscription) -> Result<()> {
    ensure!(
        !conn.is_autocommit(),
        "selection ledger requires writer transaction"
    );
    ensure!(
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM desk_subscription WHERE id=?1 AND incarnation=?2)",
            params![sub.id, sub.incarnation],
            |r| r.get::<_, bool>(0)
        )?,
        "stale_or_unknown"
    );
    Ok(())
}
fn count(conn: &Connection, sub: &Subscription, table: &str) -> Result<i64> {
    Ok(conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE subscription=?1 AND incarnation=?2"),
        params![sub.id, sub.incarnation],
        |r| r.get(0),
    )?)
}
fn span_slots(conn: &Connection, sub: &Subscription) -> Result<i64> {
    Ok(count(conn, sub, "desk_subscription_exclusion")?
        + count(conn, sub, "desk_subscription_exception")?)
}
fn class(class: replay::DeliveryClass) -> &'static str {
    match class {
        replay::DeliveryClass::History => "history",
        replay::DeliveryClass::SinceSwitchOn => "since_switch_on",
        replay::DeliveryClass::Unknown => "unknown",
    }
}

pub(super) fn revision(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT revision FROM desk_replay_revision WHERE singleton=1",
        [],
        |r| r.get(0),
    )?)
}
/// Future proof adoption calls this in its own transaction, not on reader
/// access. Intentionally no production hook in this schema-only PR.
pub(super) fn advance_revision(conn: &Connection) -> Result<i64> {
    ensure!(
        !conn.is_autocommit(),
        "selection ledger requires writer transaction"
    );
    let old = revision(conn)?;
    let next = old.checked_add(1).context("backpressure")?;
    conn.execute(
        "UPDATE desk_replay_revision SET revision=?1 WHERE singleton=1",
        [next],
    )?;
    Ok(next)
}

/// Record a caller-proved skipped interval BEFORE compacting across it. Gaps
/// can be conservatively enclosed only if they include no acknowledged order.
/// Below the automatic prefix, only existing skipped evidence may be covered:
/// the complement is actually acknowledged and must never be resurrected.
pub(super) fn exclude(conn: &Connection, sub: &Subscription, span: &Span) -> Result<()> {
    writer(conn, sub)?;
    ensure!(
        span.first > 0
            && span.last >= span.first
            && (1..=7).contains(&span.reasons)
            && span.checked >= 0
            && span.checked <= revision(conn)?,
        "invalid exclusion"
    );
    let rows = spans(conn, sub, &span.core, &span.mailbox)?;
    let prefix: i64 = conn.query_row("SELECT through_order FROM desk_subscription_progress WHERE subscription=?1 AND core=?2 AND mailbox=?3", params![sub.id,span.core,span.mailbox], |r|r.get(0)).optional()?.unwrap_or(0);
    let mut covered = span.first - 1;
    for old in &rows {
        if old.first <= covered.saturating_add(1) && old.last >= span.first {
            covered = covered.max(old.last);
        }
    }
    ensure!(
        span.first > prefix || covered >= span.last.min(prefix),
        "acknowledged exclusion hole"
    );
    // Existing unacked normal refs have no reserved split slot. Enclosing one
    // could strand its ack at SPANS capacity; never create that obligation.
    // New admissions into an existing span must first reserve a carry slot.
    let retained: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM desk_subscription_ref WHERE subscription=?1 AND incarnation=?6 AND core=?2 AND mailbox=?3 AND source_order BETWEEN ?4 AND ?5)", params![sub.id,span.core,span.mailbox,span.first,span.last,sub.incarnation], |r|r.get(0))?;
    ensure!(!retained, "automatic custody exclusion hole");
    let mut merged = span.clone();
    let mut remove = Vec::new();
    for old in rows {
        if old.first <= merged.last.saturating_add(1) && old.last.saturating_add(1) >= merged.first
        {
            merged.first = merged.first.min(old.first);
            merged.last = merged.last.max(old.last);
            merged.reasons |= old.reasons;
            merged.checked = merged.checked.min(old.checked);
            remove.push(old);
        }
    }
    // A repeat scan/proof cannot rewind a partially completed revisit.
    if remove.len() == 1
        && remove[0].first == merged.first
        && remove[0].last == merged.last
        && remove[0].reasons == merged.reasons
        && remove[0].checked == merged.checked
    {
        return Ok(());
    }
    ensure!(
        span_slots(conn, sub)? - (remove.len() as i64) < SPANS,
        "backpressure"
    );
    for old in remove {
        conn.execute("DELETE FROM desk_subscription_exclusion WHERE subscription=?1 AND incarnation=?5 AND core=?2 AND mailbox=?3 AND first_order=?4", params![sub.id,old.core,old.mailbox,old.first,sub.incarnation])?;
    }
    conn.execute(
        "INSERT INTO desk_subscription_exclusion VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?7,?9)",
        params![
            sub.id,
            merged.core,
            merged.mailbox,
            merged.first,
            merged.last,
            merged.reasons,
            merged.checked,
            merged.first - 1,
            sub.incarnation
        ],
    )?;
    Ok(())
}
fn spans(conn: &Connection, sub: &Subscription, core: &str, mailbox: &str) -> Result<Vec<Span>> {
    Ok(conn.prepare("SELECT first_order,last_order,reasons,checked_revision,through_order,scan_revision FROM desk_subscription_exclusion WHERE subscription=?1 AND incarnation=?5 AND core=?2 AND mailbox=?3 ORDER BY first_order LIMIT ?4")?
        .query_map(params![sub.id,core,mailbox,SPANS+1,sub.incarnation], |r| Ok(Span {core:core.into(),mailbox:mailbox.into(),first:r.get(0)?,last:r.get(1)?,reasons:r.get(2)?,checked:r.get(3)?,through:r.get(4)?,scan:r.get(5)?}))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(super) fn start_revisit(conn: &Connection, sub: &Subscription) -> Result<i64> {
    writer(conn, sub)?;
    conn.execute(
        "INSERT OR IGNORE INTO desk_subscription_selection(subscription,incarnation) VALUES(?1,?2)",
        params![sub.id, sub.incarnation],
    )?;
    conn.execute("UPDATE desk_subscription_selection SET target_revision=?2 WHERE subscription=?1 AND incarnation=?3 AND target_revision IS NULL", params![sub.id,revision(conn)?,sub.incarnation])?;
    Ok(conn.query_row(
        "SELECT target_revision FROM desk_subscription_selection WHERE subscription=?1 AND incarnation=?2",
        params![sub.id, sub.incarnation],
        |r| r.get(0),
    )?)
}
/// The activating caller scans this interval in bounded source-order chunks,
/// without the ordinary prefix filter, authorizes each origin and reserves all
/// newly eligible exceptions BEFORE checkpointing it. No revision is consumed
/// while any span still needs that work.
pub(super) fn revisit(conn: &Connection, sub: &Subscription) -> Result<Option<Span>> {
    writer(conn, sub)?;
    Ok(conn.query_row("SELECT core,mailbox,first_order,last_order,reasons,checked_revision,CASE WHEN scan_revision<(SELECT target_revision FROM desk_subscription_selection WHERE subscription=?1 AND incarnation=?2) THEN first_order-1 ELSE through_order END,scan_revision FROM desk_subscription_exclusion WHERE subscription=?1 AND incarnation=?2 AND checked_revision<(SELECT target_revision FROM desk_subscription_selection WHERE subscription=?1 AND incarnation=?2) ORDER BY checked_revision,core,mailbox,first_order LIMIT 1", params![sub.id,sub.incarnation], |r| Ok(Span {core:r.get(0)?,mailbox:r.get(1)?,first:r.get(2)?,last:r.get(3)?,reasons:r.get(4)?,checked:r.get(5)?,through:r.get(6)?,scan:r.get(7)?})).optional()?)
}
pub(super) fn checkpoint(
    conn: &Connection,
    sub: &Subscription,
    span: &Span,
    through: i64,
) -> Result<()> {
    writer(conn, sub)?;
    ensure!(
        through >= span.through && through <= span.last,
        "invalid revisit checkpoint"
    );
    let changed = conn.execute("UPDATE desk_subscription_exclusion SET through_order=?7,scan_revision=(SELECT target_revision FROM desk_subscription_selection WHERE subscription=?1 AND incarnation=?10),checked_revision=CASE WHEN ?7=last_order THEN (SELECT target_revision FROM desk_subscription_selection WHERE subscription=?1 AND incarnation=?10) ELSE checked_revision END WHERE subscription=?1 AND incarnation=?10 AND core=?2 AND mailbox=?3 AND first_order=?4 AND last_order=?5 AND (CASE WHEN scan_revision<(SELECT target_revision FROM desk_subscription_selection WHERE subscription=?1 AND incarnation=?10) THEN first_order-1 ELSE through_order END)=?6 AND checked_revision=?8 AND scan_revision=?9 AND checked_revision<(SELECT target_revision FROM desk_subscription_selection WHERE subscription=?1 AND incarnation=?10)", params![sub.id,span.core,span.mailbox,span.first,span.last,span.through,through,span.checked,span.scan,sub.incarnation])?;
    ensure!(changed == 1, "stale_or_unknown");
    Ok(())
}
pub(super) fn finish_revisit(conn: &Connection, sub: &Subscription) -> Result<()> {
    writer(conn, sub)?;
    ensure!(revisit(conn, sub)?.is_none(), "backpressure");
    let changed = conn.execute("UPDATE desk_subscription_selection SET consumed_revision=target_revision,target_revision=NULL WHERE subscription=?1 AND incarnation=?2 AND target_revision IS NOT NULL", params![sub.id,sub.incarnation])?;
    ensure!(changed == 1, "stale_or_unknown");
    Ok(())
}

/// Exceptions retain exact observations, independent of raw refs and snapshot
/// expiry. The spare span slot is reserved in this same transaction; it is not
/// consumed by later unrelated work, so ack can punch a permanent hole.
pub(super) fn carry(
    conn: &Connection,
    sub: &Subscription,
    o: &Observation,
    delivery: replay::DeliveryClass,
    cause: &str,
) -> Result<()> {
    writer(conn, sub)?;
    ensure!(
        o.order > 0 && matches!(cause, "proof" | "offer" | "closure"),
        "invalid exception"
    );
    let body = serde_json::to_string(o)?;
    let old: Option<(String,String)> = conn.query_row("SELECT observation,delivery_class FROM desk_subscription_exception WHERE subscription=?1 AND incarnation=?6 AND core=?2 AND mailbox=?3 AND mail=?4 AND source_order=?5", params![sub.id,o.origin.core,o.origin.mailbox,o.origin.id,o.order,sub.incarnation], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((old, _frozen_class)) = old {
        ensure!(old == body, "conflicting exception");
        return Ok(()); // New proof never changes an already frozen label.
    }
    let prefix = through(conn, sub, o)?;
    if o.order <= prefix {
        let skipped: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM desk_subscription_exclusion WHERE subscription=?1 AND incarnation=?5 AND core=?2 AND mailbox=?3 AND first_order<=?4 AND last_order>=?4)", params![sub.id,o.origin.core,o.origin.mailbox,o.order,sub.incarnation], |r|r.get(0))?;
        ensure!(skipped, "already acknowledged");
    }
    let acked: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM desk_subscription_ref WHERE subscription=?1 AND incarnation=?5 AND core=?2 AND mailbox=?3 AND source_order=?4 AND acked=1)", params![sub.id,o.origin.core,o.origin.mailbox,o.order,sub.incarnation], |r|r.get(0))?;
    ensure!(!acked, "already acknowledged");
    ensure!(
        count(conn, sub, "desk_subscription_exception")? < WINDOW && span_slots(conn, sub)? < SPANS,
        "backpressure"
    );
    let bytes: i64 = conn.query_row(
        "SELECT COALESCE(SUM(bytes),0) FROM desk_subscription_exception WHERE subscription=?1 AND incarnation=?2",
        params![sub.id, sub.incarnation],
        |r| r.get(0),
    )?;
    let size = serde_json::to_vec(&json!({"subscription":sub.id,"incarnation":sub.incarnation,"origin":o.origin,"observation":o,"delivery_class":class(delivery),"cause":cause}))?.len() as i64;
    ensure!(bytes + size <= EXCEPTION_BYTES, "backpressure");
    conn.execute(
        "INSERT INTO desk_subscription_exception VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            sub.id,
            o.origin.core,
            o.origin.mailbox,
            o.origin.id,
            o.order,
            body,
            size,
            class(delivery),
            cause,
            sub.incarnation
        ],
    )?;
    Ok(())
}
/// Call only for an actually acknowledged AUTOMATIC carried observation. A
/// history read/ack must never call this. Splitting excludes the acknowledged
/// point permanently, even after its raw ref compacts into the prefix.
pub(super) fn acknowledge_carried(
    conn: &Connection,
    sub: &Subscription,
    o: &Observation,
) -> Result<()> {
    writer(conn, sub)?;
    let removed = conn.execute("DELETE FROM desk_subscription_exception WHERE subscription=?1 AND incarnation=?6 AND core=?2 AND mailbox=?3 AND mail=?4 AND source_order=?5", params![sub.id,o.origin.core,o.origin.mailbox,o.origin.id,o.order,sub.incarnation])?;
    ensure!(removed == 1, "stale_or_unknown");
    acknowledge_automatic(conn, sub, o)
}

/// Punch a normal-ref automatic ack through skipped evidence without requiring
/// an exception row. The caller must commit the actual ack in this transaction,
/// and roll back on backpressure (a middle split needs one free span slot).
pub(super) fn acknowledge_automatic(
    conn: &Connection,
    sub: &Subscription,
    o: &Observation,
) -> Result<()> {
    writer(conn, sub)?;
    for span in spans(conn, sub, &o.origin.core, &o.origin.mailbox)? {
        if span.first > o.order || span.last < o.order {
            continue;
        }
        if span.first < o.order && o.order < span.last {
            ensure!(span_slots(conn, sub)? < SPANS, "backpressure");
        }
        conn.execute("DELETE FROM desk_subscription_exclusion WHERE subscription=?1 AND incarnation=?5 AND core=?2 AND mailbox=?3 AND first_order=?4", params![sub.id,span.core,span.mailbox,span.first,sub.incarnation])?;
        for (first, last) in [
            (span.first, o.order - 1),
            (o.order.saturating_add(1), span.last),
        ] {
            if first <= last && first != o.order {
                conn.execute(
                    "INSERT INTO desk_subscription_exclusion VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                    params![
                        sub.id,
                        span.core,
                        span.mailbox,
                        first,
                        last,
                        span.reasons,
                        span.checked,
                        span.through.clamp(first - 1, last),
                        span.scan,
                        sub.incarnation
                    ],
                )?;
            }
        }
    }
    acknowledge_closure(conn, sub, o)
}

/// A pinned closure can be delivered and acknowledged through a normal ref,
/// without ever having a carried exception. History acknowledgments cannot use
/// this helper; raw ack still makes no assertion about authoritative lifecycle.
pub(super) fn acknowledge_closure(
    conn: &Connection,
    sub: &Subscription,
    o: &Observation,
) -> Result<()> {
    writer(conn, sub)?;
    conn.execute("UPDATE desk_subscription_pin SET closure_acked=1 WHERE subscription=?1 AND incarnation=?6 AND core=?2 AND mailbox=?3 AND mail=?4 AND closure_order=?5", params![sub.id,o.origin.core,o.origin.mailbox,o.origin.id,o.order,sub.incarnation])?;
    Ok(())
}

/// Origin pins are offered custody, never a side effect of scanning/history.
/// Caller must freeze snapshot custody in this SAME transaction. Re-offering
/// or newer proof cannot change the original class of this carried origin.
pub(super) fn offer_pin(
    conn: &Connection,
    sub: &Subscription,
    o: &Observation,
    delivery: replay::DeliveryClass,
) -> Result<()> {
    writer(conn, sub)?;
    let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM desk_subscription_pin WHERE subscription=?1 AND incarnation=?5 AND core=?2 AND mailbox=?3 AND mail=?4)", params![sub.id,o.origin.core,o.origin.mailbox,o.origin.id,sub.incarnation], |r|r.get(0))?;
    if !exists {
        ensure!(
            o.state.as_deref() == Some("open"),
            "pin requires authoritative open origin"
        );
        ensure!(
            count(conn, sub, "desk_subscription_pin")? < WINDOW,
            "backpressure"
        );
        conn.execute("INSERT INTO desk_subscription_pin(subscription,core,mailbox,mail,delivery_class,offered_order,incarnation) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![sub.id,o.origin.core,o.origin.mailbox,o.origin.id,class(delivery),o.order,sub.incarnation])?;
    } else if o.state.as_deref() == Some("closed") {
        conn.execute("UPDATE desk_subscription_pin SET closure_order=?5,closure_acked=0 WHERE subscription=?1 AND incarnation=?6 AND core=?2 AND mailbox=?3 AND mail=?4 AND ?5>=offered_order AND (closure_order IS NULL OR closure_order<?5)", params![sub.id,o.origin.core,o.origin.mailbox,o.origin.id,o.order,sub.incarnation])?;
    }
    Ok(())
}
pub(super) fn retire_closed_pins(conn: &Connection, sub: &Subscription) -> Result<()> {
    writer(conn, sub)?;
    conn.execute("DELETE FROM desk_subscription_pin AS p WHERE subscription=?1 AND incarnation=?2 AND closure_acked=1 AND NOT EXISTS(SELECT 1 FROM desk_subscription_exception e WHERE e.subscription=p.subscription AND e.incarnation=p.incarnation AND e.core=p.core AND e.mailbox=p.mailbox AND e.mail=p.mail) AND NOT EXISTS(SELECT 1 FROM desk_subscription_ref r WHERE r.subscription=p.subscription AND r.incarnation=p.incarnation AND r.core=p.core AND r.mailbox=p.mailbox AND r.mail=p.mail AND r.acked=0)", params![sub.id,sub.incarnation])?;
    Ok(())
}
