//! Classification foundation only. Selection, pins, compaction and public
//! labels are deliberately NOT activated by this stage of exposure PR 4.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DeliveryClass {
    History,
    SinceSwitchOn,
    Unknown,
}

/// Called exactly once for a NEW reader, in its creation transaction. A reader
/// that predates this schema, or starts without an operator binding, remains
/// unknown. Neither visibility resets nor later enable/re-enable can recapture
/// its boundary. Missing/disabled exposure is not a new grant admission here.
pub(super) fn capture(
    conn: &Connection,
    sub: &Subscription,
    m: &Manifest,
    c: &DelegatedContext,
) -> Result<()> {
    let door = c.resource().strip_prefix("door:").context("denied")?;
    let binding = super::super::desk_exposure::replay_binding(conn, door, m)?;
    conn.execute(
        "INSERT INTO desk_subscription_replay(subscription,binding) VALUES(?1,?2)",
        params![sub.id, binding.map(|b| b.to_string())],
    )?;
    Ok(())
}

pub(super) fn binding(conn: &Connection, sub: &Subscription) -> Result<Option<Value>> {
    let stored: Option<Option<String>> = conn
        .query_row(
            "SELECT binding FROM desk_subscription_replay WHERE subscription=?1",
            [&sub.id],
            |r| r.get(0),
        )
        .optional()?;
    stored
        .flatten()
        .map(|b| Ok(serde_json::from_str(&b)?))
        .transpose()
}

/// The saved complete source evidence is `canonical-barrier-uuid:journal-seq`,
/// not a timestamp, arrival ordinal, or dashboard cursor. Pending and malformed
/// boundaries cannot establish newness. A later source contact does not move
/// an established boundary forward. Pending-source establishment is PR 4b.
fn boundary(binding: &Value, core: &str) -> Option<i64> {
    if binding["replay_policy"] != "open_and_new" {
        return None;
    }
    let source = binding["switch_on"]
        .as_array()?
        .iter()
        .find(|s| s["source"] == core)?;
    if source["state"] != "established" {
        return None;
    }
    let (barrier, order) = source["through"].as_str()?.rsplit_once(':')?;
    let parsed = uuid::Uuid::parse_str(barrier).ok()?;
    if parsed.to_string() != barrier {
        return None;
    }
    let order: i64 = order.parse().ok()?;
    (order >= 0).then_some(order)
}

/// Callers must already authorize the exact observation. This reports original
/// content age, independently of request state and this reader's raw ack state.
/// First application AFTER the boundary does not prove content was born after
/// switch-on: late baseline/copy imports also append journal events. Until a
/// content-origin predicate is settled, that case stays unknown, not new.
/// It does not offer a ref, advance progress, mutate proof, or select any mail.
pub(super) fn delivery_class(
    conn: &Connection,
    sub: &Subscription,
    m: &Manifest,
    o: &Observation,
) -> Result<DeliveryClass> {
    let Some(binding) = binding(conn, sub)? else {
        return Ok(DeliveryClass::Unknown);
    };
    if binding["domain"] != m.domain
        || binding["owner_core"] != m.owner_core
        || binding["generation"] != m.generation
        || binding["incarnation"] != m.incarnation
    {
        return Ok(DeliveryClass::Unknown);
    }
    let Some(boundary) = boundary(&binding, &o.origin.core) else {
        return Ok(DeliveryClass::Unknown);
    };
    let Some(original) = intake::original_order(conn, m, &o.origin)? else {
        return Ok(DeliveryClass::Unknown);
    };
    ensure!(original > 0 && original <= o.order, "integrity_gap");
    Ok(if original <= boundary {
        DeliveryClass::History
    } else {
        DeliveryClass::Unknown
    })
}
