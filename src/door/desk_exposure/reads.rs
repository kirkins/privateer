//! Scoped local reads and ledger-owned acknowledgments. No
//! network or context-only entry point; actual bearer authority is required.
use super::*;
use crate::door::{
    coverage,
    scoped::{Command, DelegatedContext, Denial},
};
use subscription::Operation;

pub(super) const BYTES: usize = 192 * 1024;
const SCHEMA: &str = "desk.subscription.v1";

fn canonical_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|id| id.hyphenated().to_string() == value)
}

pub(crate) fn operation(command: Command, args: &Value) -> Result<Operation, Denial> {
    use Command::*;
    let fields = args.as_object().ok_or(Denial::Arguments)?;
    let allowed: &[&str] = match command {
        DeskOpen => &[],
        DeskResume => &["subscription"],
        DeskPull => &["subscription", "limit", "continuation"],
        DeskShow => &["subscription", "origin"],
        DeskThread => &["subscription", "origin", "limit", "continuation"],
        DeskAck => &["subscription", "ack_key", "refs"],
        _ => return Err(Denial::UnknownCommand),
    };
    if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(Denial::Arguments);
    }
    if command == DeskOpen {
        return Ok(Operation::Open);
    }
    let subscription = args["subscription"]
        .as_str()
        .filter(|v| canonical_uuid(v))
        .ok_or(Denial::Arguments)?
        .to_owned();
    if command == DeskAck {
        let opaque = |v: &Value| {
            v.as_str()
                .filter(|s| !s.is_empty() && s.len() <= 64 && s.is_ascii())
                .map(str::to_owned)
                .ok_or(Denial::Arguments)
        };
        let key = opaque(&args["ack_key"])?;
        let refs = args["refs"]
            .as_array()
            .filter(|r| (1..=256).contains(&r.len()))
            .ok_or(Denial::Arguments)?
            .iter()
            .map(opaque)
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Operation::Ack {
            subscription,
            key,
            refs,
        });
    }
    let items = match args.get("limit") {
        None => 50,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=50).contains(n))
            .ok_or(Denial::Arguments)? as usize,
    };
    let continuation = match args.get("continuation") {
        None => None,
        Some(v) => Some(
            v.as_str()
                .filter(|s| !s.is_empty() && s.len() <= 256)
                .ok_or(Denial::Arguments)?
                .to_owned(),
        ),
    };
    let origin = || -> Result<coverage::MailIdentity, Denial> {
        let o: coverage::MailIdentity =
            serde_json::from_value(args["origin"].clone()).map_err(|_| Denial::Arguments)?;
        if !canonical_uuid(&o.core) || !canonical_uuid(&o.mailbox) || identifier(&o.id).is_err() {
            return Err(Denial::Arguments);
        }
        Ok(o)
    };
    Ok(match command {
        DeskResume => Operation::Resume { subscription },
        DeskPull => Operation::Pull {
            subscription,
            continuation,
            items,
        },
        DeskShow => Operation::Show {
            subscription,
            origin: origin()?,
        },
        DeskThread => Operation::Thread {
            subscription,
            origin: origin()?,
            continuation,
            items,
        },
        _ => return Err(Denial::UnknownCommand),
    })
}

/// Closed pre-effect classification. Placement detail never becomes public
/// topology, but remains typed all the way to the bounded internal audit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    Placement(ExposureRefusal),
    Disabled,
    StaleBinding,
    Engine(subscription::ReadRefusal),
    Unavailable,
    AckEnvelope,
}
impl Refusal {
    fn internal(self) -> &'static str {
        match self {
            Self::Placement(p) => p.code(),
            Self::Disabled => "desk_disabled",
            Self::StaleBinding => "stale_generation",
            Self::Engine(e) => e.code(),
            Self::Unavailable => "unavailable",
            Self::AckEnvelope => "oversized_item",
        }
    }
    fn public(self, admitted_coverage: Option<Value>) -> Value {
        let (code, message, retry) = match self {
            Self::AckEnvelope => (
                "oversized_item",
                "Acknowledgment cannot fit; nothing acknowledged",
                "operator_diagnosis",
            ),
            Self::Engine(subscription::ReadRefusal::OversizedItem) => (
                "oversized_item",
                "Item cannot fit; nothing skipped",
                "operator_diagnosis",
            ),
            Self::Engine(subscription::ReadRefusal::Backpressure) => (
                "backpressure",
                "Reader custody capacity is full; pending facts retained",
                "resume",
            ),
            Self::Engine(subscription::ReadRefusal::InvalidAck) => (
                "invalid_ack",
                "Invalid acknowledgment batch; nothing acknowledged",
                "resume",
            ),
            Self::Engine(subscription::ReadRefusal::StaleOrUnknown) => (
                "stale_or_unknown",
                "Unknown, expired or stale reader or continuation",
                "resume",
            ),
            // Identical wording for absent, hidden, remote and stale placement.
            // No peer, direction, path or binding data is disclosed.
            _ => (
                "unavailable",
                "unsupported read direction / incomplete feature",
                "operator_diagnosis",
            ),
        };
        json!({"schema":SCHEMA,"ok":false,"error":{"code":code,"message":message,"retry":retry},
            "coverage":admitted_coverage.unwrap_or_else(coverage::unknown_domain)})
    }
}

fn admitted_binding(conn: &Connection, door: &str) -> Result<(Manifest, Value), Refusal> {
    let (_, placement) =
        desk_placement::exposure_placement(conn).map_err(|_| Refusal::Unavailable)?;
    let m = placement.map_err(Refusal::Placement)?;
    let (enabled, binding) = stored(conn, door)
        .map_err(|_| Refusal::Unavailable)?
        .ok_or(Refusal::Disabled)?;
    if !enabled {
        return Err(Refusal::Disabled);
    }
    if !binding.matches(&m).map_err(|_| Refusal::Unavailable)? {
        return Err(Refusal::StaleBinding);
    }
    Ok((
        m,
        serde_json::to_value(binding).map_err(|_| Refusal::Unavailable)?,
    ))
}

/// Resolve in the caller's writer transaction without generating a second
/// request/audit row. A copied context or rotated/revoked token is not authority.
fn credential(
    conn: &Connection,
    door: &str,
    token: &str,
    audience: Option<&str>,
    c: &DelegatedContext,
) -> Result<(), Denial> {
    conn.execute_batch("SAVEPOINT desk_read_credential")
        .map_err(|_| Denial::AuthUnavailable)?;
    let fresh = crate::door::clients::resolve(conn, door, token, &["mail:read"], audience);
    conn.execute_batch("ROLLBACK TO desk_read_credential; RELEASE desk_read_credential")
        .map_err(|_| Denial::AuthUnavailable)?;
    let fresh = fresh.map_err(|_| Denial::AuthUnavailable)??;
    if fresh.capabilities() != c.capabilities()
        || fresh.visibility_revision() != c.visibility_revision()
        || fresh.client() != c.client()
        || fresh.resource() != c.resource()
    {
        return Err(Denial::Scope);
    }
    Ok(())
}

/// Optional discovery is observational, never an open/resume or cached
/// authority. Probe failures hide only the descriptor, not unrelated tools or
/// base capabilities authenticated by the caller.
pub(crate) fn capabilities(
    door: &str,
    token: &str,
    audience: Option<&str>,
    c: &DelegatedContext,
) -> Value {
    let mut value = c.capabilities();
    if !c.has("mail:read") {
        return value;
    }
    let admitted = || -> Option<()> {
        let conn = crate::fleet::registry().ok()?;
        // Read only the flag: absent/disabled doors do not need placement
        // parsing, the external fence or either writer transaction. This is a
        // fast negative, never authority for advertising or serving a tool.
        let enabled: bool = conn
            .query_row(
                "SELECT enabled FROM door_desk_exposure WHERE door=?1",
                [door],
                |r| r.get(0),
            )
            .optional()
            .ok()??;
        if !enabled {
            return None;
        }
        let (_fence, _) = desk_placement::exposure_placement(&conn).ok()?;
        // Pin the actual binding across both credential-checked writer
        // transactions, not just the flag. Disclose no placement inventory.
        let binding_check = || -> Option<Value> {
            let tx =
                rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).ok()?;
            credential(&tx, door, token, audience, c).ok()?;
            let binding = admitted_binding(&tx, door).ok()?.1;
            tx.rollback().ok()?;
            Some(binding)
        };
        (binding_check()? == binding_check()?).then_some(())
    };
    if admitted().is_some() {
        value["desk_subscriptions"] = json!({"schema":"desk.subscription-capability.v1",
            "mode":"local_raw","operations":["open","resume","pull","show","thread","ack"],
            "remote_owner":false,"moves":false,"waiting":false,"presence":false,"digests":false});
    }
    value
}

fn framed(value: &Value, id: &Value, c: &DelegatedContext) -> Value {
    let mut result = value.clone();
    result["schema"] = json!(SCHEMA);
    result["provenance"] = c.provenance();
    json!({"type":"response","id":id,"ok":true,"result":result})
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn request(
    door: &str,
    token: &str,
    audience: Option<&str>,
    c: &DelegatedContext,
    command: Command,
    args: &Value,
    id: &Value,
) -> Result<Value, Denial> {
    let op = operation(command, args)?;
    let ack = command == Command::DeskAck;
    let conn = crate::fleet::registry().map_err(|_| Denial::AuthUnavailable)?;
    // Fence order matches intake/placement and survives both admission and
    // release. No network occurs, and registry transactions stay separate.
    let (_fence, _) =
        desk_placement::exposure_placement(&conn).map_err(|_| Denial::AuthUnavailable)?;
    let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)
        .map_err(|_| Denial::AuthUnavailable)?;
    credential(&tx, door, token, audience, c)?;
    let binding = admitted_binding(&tx, door);
    let mut value = match &binding {
        Ok((m, binding)) => {
            // Reserve the maximum admitted serialized ID (256 bytes), not just
            // this ID: frozen diagnostics must also fit a later continuation.
            // Merging into a nonempty engine object adds a comma, hence -1.
            // Runtime provenance is stable-sized within this reader identity.
            let overhead = serde_json::to_vec(&framed(&json!({}), id, c))
                .map_err(|_| Denial::ExecutionFailed)?
                .len()
                .saturating_sub(1)
                .saturating_add(
                    256usize.saturating_sub(
                        serde_json::to_vec(id)
                            .map_err(|_| Denial::ExecutionFailed)?
                            .len(),
                    ),
                );
            match subscription::read_admitted(
                &tx,
                c,
                m,
                binding,
                op,
                BYTES.saturating_sub(overhead),
            ) {
                Ok(v) => v,
                Err(e) => {
                    let safe = if e == subscription::ReadRefusal::Unavailable {
                        None
                    } else {
                        coverage::subscription_coverage(
                            &tx,
                            m,
                            c,
                            &crate::machines::desk_intake::subscription::source_vector(&tx, m),
                        )
                        .ok()
                    };
                    // Roll back the WHOLE operation, including partial ack work.
                    tx.rollback().map_err(|_| Denial::AuthUnavailable)?;
                    #[cfg(test)]
                    tests::before_refusal(&conn);
                    return release_refusal(
                        &conn,
                        door,
                        token,
                        audience,
                        c,
                        Refusal::Engine(e),
                        safe.map(|coverage| (coverage, binding.clone())),
                        id,
                    );
                }
            }
        }
        Err(reason) => {
            let reason = *reason;
            tx.rollback().map_err(|_| Denial::AuthUnavailable)?;
            return release_refusal(&conn, door, token, audience, c, reason, None, id);
        }
    };
    value["schema"] = json!(SCHEMA);
    if serde_json::to_vec(&framed(&value, id, c))
        .map_err(|_| Denial::ExecutionFailed)?
        .len()
        > BYTES
    {
        // This check still precedes commit; no oversized committed page escapes.
        tx.rollback().map_err(|_| Denial::AuthUnavailable)?;
        return release_refusal(
            &conn,
            door,
            token,
            audience,
            c,
            if ack {
                Refusal::AckEnvelope
            } else {
                Refusal::Engine(subscription::ReadRefusal::OversizedItem)
            },
            None,
            id,
        );
    }
    // From the commit attempt onward, an ack can no longer claim no effect.
    let uncertain = |denial| if ack { Denial::OutcomeUnknown } else { denial };
    #[cfg(test)]
    tests::before_commit(&conn);
    tx.commit()
        .map_err(|_| uncertain(Denial::AuthUnavailable))?;
    #[cfg(test)]
    tests::after_commit(&conn);
    let release = || {
        let release = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)
            .map_err(|_| Denial::AuthUnavailable)?;
        credential(&release, door, token, audience, c)?;
        let fresh = admitted_binding(&release, door);
        if fresh.as_ref().map(|(_, b)| b).ok() != binding.as_ref().map(|(_, b)| b).ok() {
            return Err(Denial::Scope);
        }
        // Resolve bookkeeping is not custody; rollback cannot undo the ack.
        release.rollback().map_err(|_| Denial::AuthUnavailable)
    };
    release().map_err(uncertain)?;
    Ok(value)
}

#[allow(clippy::too_many_arguments)]
fn release_refusal(
    conn: &Connection,
    door: &str,
    token: &str,
    audience: Option<&str>,
    c: &DelegatedContext,
    reason: Refusal,
    protected: Option<(Value, Value)>,
    id: &Value,
) -> Result<Value, Denial> {
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|_| Denial::AuthUnavailable)?;
    credential(&tx, door, token, audience, c)?;
    if let Some((_, binding)) = &protected
        && admitted_binding(&tx, door).as_ref().map(|(_, b)| b).ok() != Some(binding)
    {
        return Err(Denial::Scope);
    }
    let mut value = reason.public(protected.map(|(coverage, _)| coverage));
    if serde_json::to_vec(&framed(&value, id, c))
        .map_err(|_| Denial::ExecutionFailed)?
        .len()
        > BYTES
    {
        value = reason.public(None);
    }
    if serde_json::to_vec(&framed(&value, id, c))
        .map_err(|_| Denial::ExecutionFailed)?
        .len()
        > BYTES
    {
        return Err(Denial::Envelope);
    }
    crate::door::clients::audit(
        &tx,
        door,
        Some(c.client()),
        "desk.read.refused",
        reason.internal(),
    )
    .map_err(|_| Denial::AuthUnavailable)?;
    tx.commit().map_err(|_| Denial::AuthUnavailable)?;
    Ok(value)
}

#[cfg(test)]
mod tests;
