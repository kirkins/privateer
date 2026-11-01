//! Classification only. Selection, pins, compaction and public labels are NOT
//! activated here. Reads never establish cuts, contacts or copy-first links.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DeliveryClass {
    History,
    SinceSwitchOn,
    Unknown,
}

/// Capture the operator binding once, in reader creation's transaction. Legacy
/// readers without a binding stay unknown even if exposure is enabled later.
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

/// Caller must authorize this exact observation. `delayed_cut` is an internal
/// exclusion reason, not proof of history: the public class remains unknown.
/// This does not offer refs, acknowledge, advance progress or select mail.
pub(super) fn delivery_class(
    conn: &Connection,
    sub: &Subscription,
    m: &Manifest,
    o: &Observation,
) -> Result<DeliveryClass> {
    let Some(binding) = binding(conn, sub)? else {
        return Ok(DeliveryClass::Unknown);
    };
    Ok(match intake::replay_class(conn, m, &binding, &o.origin)? {
        "history" => DeliveryClass::History,
        "since_switch_on" => DeliveryClass::SinceSwitchOn,
        _ => DeliveryClass::Unknown,
    })
}
