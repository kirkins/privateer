//! Scoped presence and summary operations have one owner: the existing captain
//! desk. A failed/unsupported/reverse-only route never falls back to local state.
use super::scoped::{Command, DelegatedContext};
use anyhow::{Result, ensure};
use serde_json::{Value, json};

fn needs_legacy_writer(command: Command, args: &Value) -> bool {
    matches!(command, Command::Digest | Command::Ack)
        || (command == Command::Away && matches!(args["action"].as_str(), Some("start" | "back")))
}

pub(crate) fn request(command: Command, args: &Value, c: &DelegatedContext) -> Result<Value> {
    if needs_legacy_writer(command, args) {
        // Admission has run, but pending placement rejects desk routing before
        // local() can check a writer. Preserve that deliberate named refusal.
        // A valid existing route to the owner is not blocked at its source.
        crate::machines::desk_placement::check_legacy_desk_route()?;
    }
    let operation = match command {
        Command::Away => "away",
        Command::Held => "held",
        Command::Digest => "digest",
        Command::Ack => "ack",
        _ => unreachable!("desk operation"),
    };
    let origin = super::reader_link::machine_id()?;
    if let Some((machine, mut result)) =
        crate::machines::delegated_desk_call(operation, args, c, &origin)?
    {
        if command == Command::Ack {
            // An older desk reports unversioned digest-ack coverage.
            super::coverage::stamp_operation(&mut result, "digest_ack");
        }
        result["desk"] = json!({"machine":machine,"state":"complete"});
        return Ok(result);
    }
    let mut result = local(command, args, c)?;
    result["desk"] = json!({"machine":"here","state":"complete"});
    Ok(result)
}

pub(crate) fn local(command: Command, args: &Value, c: &DelegatedContext) -> Result<Value> {
    // Routing and grant admission have already run. Fence at the actual local
    // writer, never at an origin that may legally call the owner. Hold through
    // the operation; read-only held/presence status retain their old behavior.
    let _fence = if needs_legacy_writer(command, args) {
        Some(crate::machines::desk_placement::legacy_writer()?)
    } else {
        None
    };
    if command == Command::Away && matches!(args["action"].as_str(), Some("start" | "back")) {
        ensure!(
            crate::away::liaison()?.is_none_or(|(ship, _)| c.allows(&ship)),
            super::scoped::Denial::Ship
        );
    }
    let mut result = match command {
        Command::Held => crate::away::held_visible(Some(c))?,
        Command::Away => match args["action"].as_str().unwrap_or("status") {
            "start" => {
                let afk = crate::away::parse(args["words"].as_str().unwrap_or(""));
                crate::away::enter_with(&afk.words, afk.until.as_deref(), afk.every_secs, Some(c))?
            }
            "back" => json!({"returned":crate::away::back_with(Some(c))?.is_some()}),
            _ => crate::away::status()?,
        },
        Command::Digest => crate::digest::delegated(c)?,
        Command::Ack => super::reader::local(command, args, c)?,
        _ => unreachable!("desk operation"),
    };
    if command == Command::Away {
        if let Some(period) = result["away"].as_object_mut() {
            for key in ["breakthroughs", "requests", "overflow", "source"] {
                period.remove(key);
            }
        }
    }
    result["provenance"] = c.provenance();
    Ok(result)
}

pub(crate) fn serve(args: &Value, context: &DelegatedContext) -> Result<Value> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Wire {
        origin: String,
        operation: String,
        request: Value,
    }
    let wire: Wire = serde_json::from_value(args.clone())?;
    let origin = uuid::Uuid::parse_str(&wire.origin)?
        .hyphenated()
        .to_string();
    let command = super::scoped::gateway_desk(&wire.operation, &wire.request, context)?;
    let conn = crate::fleet::registry()?;
    ensure!(
        crate::machines::desk_here(&conn),
        super::scoped::Denial::LinkedUnavailable
    );
    let reader;
    let context = if matches!(command, Command::Digest | Command::Ack) {
        reader = context.reader_context(&origin);
        &reader
    } else {
        context
    };
    let result = local(command, &wire.request, context).map_err(|error| {
        error
            .downcast_ref::<super::scoped::Denial>()
            .copied()
            .unwrap_or(
                if command == Command::Digest
                    || (command == Command::Away
                        && matches!(wire.request["action"].as_str(), Some("start" | "back")))
                {
                    super::scoped::Denial::OutcomeUnknown
                } else {
                    super::scoped::Denial::ExecutionFailed
                },
            )
    })?;
    Ok(json!({"version":1,"operation":wire.operation,"result":result}))
}
