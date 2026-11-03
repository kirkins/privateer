//! Closed, bounded failure metadata. Never serialize host errors or bindings.
use crate::herdr::{Binding, Pane, View};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Failure {
    BindingMissing,
    BindingInvalid,
    AdapterIdentityMissing,
    HostUnavailable,
    ObservationBudgetExhausted,
    SocketReplaced,
    EndpointReplaced,
    AdapterSessionReplaced,
    LeaseExpired,
    LeaseMissing,
    JobIncarnationMismatch,
    JobBindingMissing,
    SessionCustodyMissing,
    AsleepWorkingConflict,
    AdapterStatusUnknown,
    SessionRoleUnknown,
}

// New peers may add categories. Retain only the known allowlist, rather than
// discarding their entire census or reflecting an unknown peer-provided label.
// Counts still decode as u64 and known counts pass Census::validate's bounds.
pub(super) fn deserialize_counts<'de, D>(
    d: D,
) -> Result<std::collections::BTreeMap<Failure, u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(std::collections::BTreeMap::<String, u64>::deserialize(d)?
        .into_iter()
        .filter_map(|(key, count)| {
            serde_json::from_value(serde_json::Value::String(key))
                .ok()
                .map(|category| (category, count))
        })
        .collect())
}

pub(super) fn checked<'a>(binding: &Binding, view: &'a View) -> Result<&'a Pane, Failure> {
    if binding.socket_identity != view.socket_identity {
        return Err(Failure::SocketReplaced);
    }
    let pane = view
        .panes
        .iter()
        .find(|p| {
            p.pane_id == binding.pane.pane_id
                && p.terminal_id == binding.pane.terminal_id
                && p.workspace_id == binding.pane.workspace_id
                && p.tab_id == binding.pane.tab_id
        })
        .ok_or(Failure::EndpointReplaced)?;
    if [&binding.pane, pane].iter().any(|p| {
        p.agent.as_ref().is_none_or(String::is_empty)
            || p.agent_session
                .as_ref()
                .is_none_or(serde_json::Value::is_null)
    }) {
        return Err(Failure::AdapterIdentityMissing);
    }
    if pane.agent != binding.pane.agent || pane.agent_session != binding.pane.agent_session {
        return Err(Failure::AdapterSessionReplaced);
    }
    // Do not weaken the navigation binding check when proving activity.
    if !binding.matches(view, pane) {
        return Err(Failure::EndpointReplaced);
    }
    Ok(pane)
}
