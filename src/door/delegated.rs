//! Explicit delegated execution. No generic operator dispatch or session-global
//! confirmations are reachable; linked calls use a dedicated versioned gateway.
use super::scoped::{Command, DelegatedContext, Denial};
use super::{Session, members, recipient};
use serde_json::{Value, json};

/// Fixed delegated coverage: link and desk names are operator-only metadata.
/// Reader results instead describe their selected mailbox, possibly on a peer.
pub(super) fn mail_delivery() -> Value {
    json!({"mode":"local-delegated","subscriptions":false,"linked_execution":false,
        "summary":"This door reads captain mail stored on this machine only. Mail held only at another machine's captain desk is not included. Reverse-only links cannot be collected from here.",
        "warning":"Local captain mailbox coverage only; not a fleet-wide all-clear."})
}

fn delivery_state(value: &Value) -> &'static str {
    if value["queued"] == true {
        return "queued";
    }
    match value["mail"]["delivery"].as_str() {
        Some("held") => "held",
        Some("pending") => "queued",
        Some("quarantined" | "dropped") => "failed",
        Some("delivered" | "received") => "delivered",
        // Local mailbox rows explicitly have null delivery (no transport hop).
        None if value["mail"].get("delivery") == Some(&Value::Null)
            && value["mail"]["id"].is_string() =>
        {
            "delivered"
        }
        None if value["delivered"] == true => "delivered",
        None if value["delivered"] == false && !value["outcome"].is_null() => "failed",
        _ => "unknown",
    }
}

impl Session<'_> {
    pub(super) fn delegated(
        &mut self,
        command: Command,
        args: &Value,
        context: &DelegatedContext,
        source: Option<std::fs::File>,
    ) -> Result<Value, Denial> {
        use Command::*;
        // Refusals are independent of scopes, including Full Captain grants.
        if command == Approval {
            return Err(Denial::ConfirmationRequired);
        }
        if matches!(command, Away | Held | Digest) || (command == Ack && args.get("ship").is_none())
        {
            return super::desk::request(command, args, context).map_err(|error| {
                error
                    .downcast_ref::<Denial>()
                    .copied()
                    .unwrap_or(Denial::ExecutionFailed)
            });
        }
        if command == Send {
            return self.delegated_send(args, context).map(|mut value| {
                value["message_id"] = value["mail"]["id"].clone();
                value["delivery_state"] = json!(delivery_state(&value));
                value
            });
        }
        if command == FilePut {
            let to = args["to"].as_str().ok_or(Denial::Arguments)?;
            if !context.allows(to) {
                return Err(Denial::Ship);
            }
            let source = source.ok_or(Denial::ExecutionNotStarted)?;
            let home = self.home(to).map_err(|_| Denial::ExecutionFailed)?;
            return crate::files::put_delegated(
                self.store(&home).map_err(|_| Denial::ExecutionFailed)?,
                args,
                source,
                context,
            )
            .map(|value| {
                json!({"file_id":value["file_id"], "ship":to,
                "name":value["name"], "bytes":value["bytes"],
                "delivery_state":"stored", "notified":false, "completed":false})
            })
            .map_err(|e| {
                e.downcast_ref::<Denial>()
                    .copied()
                    .unwrap_or(Denial::ExecutionFailed)
            });
        }
        let mut run = || -> anyhow::Result<Value> {
            match command {
                Capabilities => Ok(context.capabilities()),
                Ships | Status if args.get("schema").is_some() => {
                    super::runtime::read(args, context)
                }
                Ships => self.ships_visible(Some(context)),
                Status => self.status_visible(Some(context)),
                Waiting => {
                    let mut result = crate::captain::waiting_visible(Some(context))?;
                    result["mail_delivery"] = mail_delivery();
                    Ok(result)
                }
                Inbox | Show | Thread | Ack => super::reader::request(command, args, context),
                ReportRead => super::reports::read(args, context),
                MachineCapacity | RuntimeNeeds => super::capacity::request(command, args, context),
                ShipJobs | JobInspect | ShipWork => {
                    super::inspection::request(command, args, context)
                }
                Held | Away | Digest => unreachable!("desk-routed independently"),
                Dismiss => crate::captain::dismiss_delegated(args, context),
                Permissions if args.get("mode").is_none() => crate::permissions::local_status(),
                Permissions | Upgrade => {
                    let (_, home) = members()?
                        .into_iter()
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("no local home"))?;
                    let store = self.store(&home)?;
                    if command == Upgrade {
                        super::upgrade::request(store, args, context)
                    } else {
                        crate::permissions::delegated(store, args, context)
                    }
                }
                Stow => {
                    let (_, home) = members()?
                        .into_iter()
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("no local home"))?;
                    crate::stow::delegated(self.store(&home)?, args, context)
                }
                Send | FilePut | Approval => unreachable!("handled independently"),
            }
        };
        let result = run().map_err(|e| {
            e.downcast_ref::<Denial>()
                .copied()
                .unwrap_or(Denial::ExecutionFailed)
        })?;
        Ok(result)
    }

    fn delegated_send(
        &mut self,
        args: &Value,
        context: &DelegatedContext,
    ) -> Result<Value, Denial> {
        // Unlike trusted v1 send, never call away::back, even after validation
        // fails, and even if this grant also holds captain:presence.
        let local = members().map_err(|_| Denial::ExecutionNotStarted)?;
        let to = args["to"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| recipient(self.door, &local))
            .ok_or(Denial::ExecutionNotStarted)?;
        if !context.allows(&to) {
            return Err(Denial::Ship);
        }
        let Some((_, home)) = local.into_iter().find(|(s, _)| *s == to) else {
            if crate::machines::remote_ships()
                .map_err(|_| Denial::ExecutionNotStarted)?
                .iter()
                .any(|(s, _, _)| *s == to)
            {
                let mut message = args.clone();
                message["to"] = json!(to);
                // Resolve the sender-side audit sink before sending. Once the
                // remote answers successfully, any local failure is uncertain.
                let home = self.home(&to).map_err(|_| Denial::ExecutionNotStarted)?;
                let door = self.door.name.clone();
                let store = self.store(&home).map_err(|_| Denial::ExecutionNotStarted)?;
                let sent = crate::machines::delegated::send_checked(store, &to, message, context)
                    .map_err(|error| {
                    error
                        .downcast_ref::<Denial>()
                        .copied()
                        .unwrap_or(Denial::OutcomeUnknown)
                })?;
                store
                    .with_delegated_audit(context, || {
                        store.audit(
                            "door.send",
                            &json!({"door":door,"to":to,
                        "mail":sent["mail"]["id"],"kind":args["kind"].as_str().unwrap_or("request"),
                        "provenance":context.provenance()}),
                        )
                    })
                    .map_err(|_| Denial::OutcomeUnknown)?;
                return Ok(sent);
            }
            return Err(Denial::ExecutionNotStarted);
        };
        let kind = args["kind"].as_str().unwrap_or("request");
        let mut message = args.clone();
        message["from"] = json!(crate::mail::CAPTAIN);
        message["to"] = json!(to);
        message["kind"] = json!(kind);
        let door = self.door.name.clone();
        let store = self.store(&home).map_err(|_| Denial::ExecutionNotStarted)?;
        let sent = crate::mail::send_delegated(store, &message, context).map_err(|e| {
            e.downcast_ref::<Denial>()
                .copied()
                // send_delegated rolls back pre-commit failures and marks
                // every post-commit failure OutcomeUnknown itself.
                .unwrap_or(Denial::ExecutionNotStarted)
        })?;
        store.audit("door.send", &json!({"door":door,"to":to,"mail":sent["id"],"kind":kind,"provenance":context.provenance()}))
            .map_err(|_| Denial::OutcomeUnknown)?;
        Ok(json!({"ship":to,"mail":sent,"returned":null}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_only_claims_known_states() {
        for (wire, expected) in [
            (json!({"queued":true}), "queued"),
            (json!({"delivered":true}), "delivered"),
            (json!({"delivered":false,"outcome":"refused"}), "failed"),
            (json!({"mail":{"id":"local","delivery":null}}), "delivered"),
            (json!({}), "unknown"),
            (json!({"mail":{"id":"missing-state"}}), "unknown"),
            (json!({"mail":{"delivery":42}}), "unknown"),
        ] {
            assert_eq!(delivery_state(&wire), expected);
        }
        for (state, expected) in [
            ("held", "held"),
            ("pending", "queued"),
            ("quarantined", "failed"),
            ("dropped", "failed"),
            ("delivered", "delivered"),
            ("received", "delivered"),
            ("future-state", "unknown"),
            ("", "unknown"),
        ] {
            assert_eq!(
                delivery_state(&json!({"mail":{"delivery":state}})),
                expected
            );
        }
    }
}
