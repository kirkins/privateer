//! Scoped presence and summary operations have one owner: the existing captain
//! desk. A failed/unsupported/reverse-only route never falls back to local state.
use super::scoped::{Command, DelegatedContext};
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub(crate) fn request(command: Command, args: &Value, c: &DelegatedContext) -> Result<Value> {
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
        result["desk"] = json!({"machine":machine,"state":"complete"});
        return Ok(result);
    }
    let mut result = local(command, args, c)?;
    result["desk"] = json!({"machine":"here","state":"complete"});
    Ok(result)
}

pub(crate) fn local(command: Command, args: &Value, c: &DelegatedContext) -> Result<Value> {
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
    uuid::Uuid::parse_str(&wire.origin)?;
    let command = super::scoped::gateway_desk(&wire.operation, &wire.request, context)?;
    let reader;
    let context = if matches!(command, Command::Digest | Command::Ack) {
        reader = context.reader_context(&wire.origin);
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
