//! Versioned raw-mail reader over the configured, operator-authenticated link.
//! No ordinary mail/operator fallback and no reverse-link queue.
use super::*;
use crate::door::scoped::Denial;

pub(crate) enum Reply {
    Complete(Value),
    Unavailable(&'static str),
    Unsupported,
}

pub(crate) fn read(ship: &str, body: Value) -> Result<Option<(String, Reply)>> {
    let conn = crate::fleet::registry()?;
    let holders: Vec<String> = conn
        .prepare("SELECT machine FROM machine_ships WHERE name=?1 ORDER BY machine")?
        .query_map([ship], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ensure!(holders.len() <= 1, "ambiguous mailbox route");
    let Some(name) = holders.first() else {
        return Ok(None);
    };
    let target = machine(&conn, name)?;
    if target.role != "collects" {
        return Ok(Some((name.clone(), Reply::Unavailable("reverse_only"))));
    }
    let operation = body["operation"].as_str().unwrap_or_default().to_owned();
    let args = body["args"].clone();
    let response = crate::peers::call_config(
        &target.peer,
        &format!("reader-{}", uuid::Uuid::new_v4()),
        "fleet.reader.v1",
        body,
        &Caller::Operator,
        Some(Duration::from_secs(15)),
    );
    let reply = match response {
        Err(_) => Reply::Unavailable("reconnect_then_list"),
        Ok(response) if response["ok"] == true => {
            let result = &response["result"];
            if result["version"] != 1 || !valid_result(&operation, &args, &result["result"]) {
                Reply::Unavailable("invalid_reader_response")
            } else {
                Reply::Complete(result["result"].clone())
            }
        }
        // Exact legacy gateway refusal, not arbitrary error-string authority.
        Ok(response)
            if response["ok"] == false
                && response["error"] == "unknown gateway command fleet.reader.v1" =>
        {
            Reply::Unsupported
        }
        Ok(response) if response["ok"] == false => {
            return Err(response["denial_code"]
                .as_str()
                .and_then(Denial::from_code)
                .unwrap_or(Denial::ExecutionFailed)
                .into());
        }
        Ok(_) => Reply::Unavailable("invalid_reader_response"),
    };
    Ok(Some((name.clone(), reply)))
}

fn valid_result(operation: &str, args: &Value, result: &Value) -> bool {
    let reference = |v: &Value| v.is_null() || v.is_string();
    let mail = |v: &Value| {
        v["id"].is_string() && v["seq"].is_i64() && v["from"].is_string() && v["to"].is_string()
    };
    if !result.is_object()
        || !["machine", "mailbox", "generation"].iter().all(|key| {
            result["origin"][*key]
                .as_str()
                .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
        })
    {
        return false;
    }
    match operation {
        "inbox" | "thread" => {
            result["items"].as_array().is_some_and(|items| {
                items.len() <= args["limit"].as_u64().unwrap_or(25) as usize
                    && items
                        .iter()
                        .all(|i| mail(&i["mail"]) && i.get("ref").is_some_and(reference))
            }) && result.get("continuation").is_some_and(reference)
        }
        "show" => {
            mail(&result["mail"])
                && result["mail"]["id"] == args["id"]
                && result.get("ref").is_some_and(reference)
        }
        "ack" => {
            result["acknowledged"].as_u64() == args["refs"].as_array().map(|refs| refs.len() as u64)
        }
        _ => false,
    }
}

pub(super) fn serve(body: &Value) -> Result<Value> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Wire {
        version: u8,
        origin: String,
        context: Value,
        operation: String,
        args: Value,
    }
    let wire: Wire = serde_json::from_value(body.clone())?;
    ensure!(wire.version == 1, "unsupported reader version");
    let origin = uuid::Uuid::parse_str(&wire.origin)?
        .hyphenated()
        .to_string();
    let context = crate::door::scoped::DelegatedContext::from_gateway(&wire.context)?;
    let command = crate::door::scoped::gateway_reader(&wire.operation, &wire.args, &context)?;
    let reader = context.reader_context(&origin);
    let result = crate::door::reader::local(command, &wire.args, &reader)?;
    Ok(json!({"version":1,"result":result}))
}
