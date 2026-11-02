//! Per-read replay classification and frozen, sanitized selection diagnostics.
//! Serving shares one prepared proof verdict; reads never establish cuts,
//! contacts or copy-first links.
use super::*;
mod diagnostics;
pub(super) use diagnostics::Diagnostics;

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

// NULL bindings cannot prove newness, but new automatic pulls still use the
// same selection/pin/ack custody. Legacy explicit lookups remain legacy until
// such a pull has created its selection state.
pub(super) fn activated(conn: &Connection, sub: &Subscription) -> Result<bool> {
    Ok(binding(conn, sub)?.is_some() || conn.query_row("SELECT EXISTS(SELECT 1 FROM desk_subscription_selection WHERE subscription=?1 AND incarnation=?2)", params![sub.id,sub.incarnation], |r|r.get::<_,bool>(0))?)
}

/// One writer/read transaction's shared classifier. No per-item local home
/// opens; unknown/delayed diagnostics remain separate from the public label.
pub(super) struct Read<'a> {
    prepared: Option<intake::ReadContext<'a>>,
    #[cfg(test)]
    deferred_validation: bool,
}
impl<'a> Read<'a> {
    pub(super) fn prepare(
        conn: &'a Connection,
        sub: &Subscription,
        m: &'a Manifest,
    ) -> Result<Self> {
        Ok(Self {
            prepared: binding(conn, sub)?
                .map(|b| intake::ReadContext::prepare(conn, m, &b))
                .transpose()?,
            #[cfg(test)]
            deferred_validation: false,
        })
    }
    #[cfg(test)]
    pub(super) fn defer_validation_for_test(&mut self) {
        self.deferred_validation = true;
    }
    pub(super) fn indeterminate(&self) -> Result<bool> {
        #[cfg(test)]
        if self.deferred_validation {
            return Ok(true);
        }
        self.prepared
            .as_ref()
            .map(|p| p.indeterminate())
            .unwrap_or(Ok(false))
    }
    pub(super) fn source_current(&self, source: &str) -> Result<bool> {
        self.prepared
            .as_ref()
            .map(|p| p.source_current(source))
            .unwrap_or(Ok(false))
    }
    pub(super) fn classify(&self, o: &Observation) -> Result<(DeliveryClass, i64)> {
        ensure!(!self.indeterminate()?, "indeterminate");
        Ok(
            match self
                .prepared
                .as_ref()
                .map(|p| p.classify(&o.origin))
                .transpose()?
            {
                Some("since_switch_on") => (DeliveryClass::SinceSwitchOn, 0),
                Some("history") => (DeliveryClass::History, selection::HISTORY),
                Some("delayed_cut") => (DeliveryClass::Unknown, selection::DELAYED),
                _ => (DeliveryClass::Unknown, selection::UNKNOWN),
            },
        )
    }
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
