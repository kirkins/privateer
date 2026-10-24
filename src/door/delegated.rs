//! Explicit delegated execution. No generic operator dispatch or session-global
//! confirmations are reachable; linked calls use a dedicated versioned gateway.
use super::scoped::{Command, DelegatedContext, Denial};
use super::{Session, members, recipient};
use serde_json::{Value, json};

impl Session<'_> {
    pub(super) fn delegated(
        &mut self,
        command: Command,
        args: &Value,
        context: &DelegatedContext,
    ) -> Result<Value, Denial> {
        use Command::*;
        // Refusals are independent of scopes, including Full Captain grants.
        match command {
            Upgrade => return Err(Denial::UpgradeUnavailable),
            Approval => return Err(Denial::ConfirmationRequired),
            Permissions if args.get("mode").is_some() => return Err(Denial::AdminUnavailable),
            Stow if args["action"].as_str().is_some_and(|a| a != "status") => {
                return Err(Denial::AdminUnavailable);
            }
            Stow if args.get("machine").is_some() || args["all"] == true => {
                return Err(Denial::LinkedUnavailable);
            }
            _ => {}
        }
        if command == Away
            && matches!(args["action"].as_str(), Some("start" | "back"))
            && crate::away::liaison()
                .map_err(|_| Denial::ExecutionFailed)?
                .is_some_and(|(ship, _)| !context.allows(&ship))
        {
            // Presence writes state before mailing the liaison. Check the route
            // first so a denied grant cannot change presence as a side effect.
            return Err(Denial::Ship);
        }
        if command == Send {
            return self.delegated_send(args, context);
        }
        if command == FilePut {
            let to = args["to"].as_str().ok_or(Denial::Arguments)?;
            if !context.allows(to) {
                return Err(Denial::Ship);
            }
            let source = super::handoff::open(
                &self.dir.join("state"),
                std::path::Path::new(args["content_file"].as_str().ok_or(Denial::Arguments)?),
            )
            .map_err(|_| Denial::ExecutionFailed)?;
            let home = self.home(to).map_err(|_| Denial::ExecutionFailed)?;
            return crate::files::put_delegated(
                self.store(&home).map_err(|_| Denial::ExecutionFailed)?,
                args,
                source,
                context,
            )
            .map_err(|e| {
                e.downcast_ref::<Denial>()
                    .copied()
                    .unwrap_or(Denial::ExecutionFailed)
            });
        }
        let mut run = || -> anyhow::Result<Value> {
            match command {
                Ships => self.ships_visible(Some(context)),
                Status => self.status_visible(Some(context)),
                Waiting => crate::captain::waiting_visible(Some(context)),
                Held => crate::away::held_visible(Some(context)),
                Away => match args["action"].as_str().unwrap_or("status") {
                    "start" => {
                        let afk = crate::away::parse(args["words"].as_str().unwrap_or(""));
                        crate::away::enter_with(
                            &afk.words,
                            afk.until.as_deref(),
                            afk.every_secs,
                            Some(context),
                        )
                    }
                    "back" => {
                        Ok(json!({"returned":crate::away::back_with(Some(context))?.is_some()}))
                    }
                    _ => crate::away::status(),
                },
                Digest => crate::digest::delegated(context),
                Dismiss => crate::captain::dismiss_delegated(args, context),
                Permissions => crate::permissions::local_status(),
                Stow => {
                    let (_, home) = members()?
                        .into_iter()
                        .find(|(ship, _)| context.allows(ship))
                        .ok_or_else(|| anyhow::anyhow!("no visible local home"))?;
                    let mut result = crate::stow::status_local(self.store(&home)?)?;
                    if let Some(ships) = result["ships"].as_array_mut() {
                        ships.retain(|s| s["ship"].as_str().is_some_and(|s| context.allows(s)));
                    }
                    Ok(result)
                }
                Send | FilePut | Approval | Upgrade => unreachable!("handled independently"),
            }
        };
        let mut result = run().map_err(|e| {
            e.downcast_ref::<Denial>()
                .copied()
                .unwrap_or(Denial::ExecutionFailed)
        })?;
        if command == Away {
            if let Some(period) = result["away"].as_object_mut() {
                for key in ["breakthroughs", "requests", "overflow", "source"] {
                    period.remove(key);
                }
            }
        }
        Ok(result)
    }

    fn delegated_send(
        &mut self,
        args: &Value,
        context: &DelegatedContext,
    ) -> Result<Value, Denial> {
        // Unlike trusted v1 send, never call away::back, even after validation
        // fails, and even if this grant also holds captain:presence.
        let local = members().map_err(|_| Denial::ExecutionFailed)?;
        let to = args["to"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| recipient(self.door, &local))
            .ok_or(Denial::ExecutionFailed)?;
        if !context.allows(&to) {
            return Err(Denial::Ship);
        }
        let Some((_, home)) = local.into_iter().find(|(s, _)| *s == to) else {
            if crate::machines::remote_ships()
                .map_err(|_| Denial::ExecutionFailed)?
                .iter()
                .any(|(s, _, _)| *s == to)
            {
                let mut message = args.clone();
                message["to"] = json!(to);
                return crate::machines::delegated_call(&to, "send", message, context)
                    .map_err(|_| Denial::LinkedUnavailable);
            }
            return Err(Denial::ExecutionFailed);
        };
        let kind = args["kind"].as_str().unwrap_or("request");
        let mut message = args.clone();
        message["from"] = json!(crate::mail::CAPTAIN);
        message["to"] = json!(to);
        message["kind"] = json!(kind);
        let door = self.door.name.clone();
        let store = self.store(&home).map_err(|_| Denial::ExecutionFailed)?;
        let sent = crate::mail::send_delegated(store, &message, context).map_err(|e| {
            e.downcast_ref::<Denial>()
                .copied()
                .unwrap_or(Denial::ExecutionFailed)
        })?;
        store.audit("door.send", &json!({"door":door,"to":to,"mail":sent["id"],"kind":kind,"provenance":context.provenance()}))
            .map_err(|_| Denial::OutcomeUnknown)?;
        Ok(json!({"ship":to,"mail":sent,"returned":null}))
    }
}
