//! Ship-addressed merged local/linked reader surface. Coverage is for the
//! selected mailbox, never an assertion that unqueried machines were read.
use super::scoped::{Command, DelegatedContext};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

pub(super) fn machine_id() -> Result<String> {
    let conn = crate::fleet::registry()?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS door_reader_identity(only INTEGER PRIMARY KEY CHECK(only=1), id TEXT NOT NULL);")?;
    conn.execute(
        "INSERT OR IGNORE INTO door_reader_identity(only,id) VALUES(1,?1)",
        [uuid::Uuid::new_v4().to_string()],
    )?;
    Ok(conn.query_row(
        "SELECT id FROM door_reader_identity WHERE only=1",
        [],
        |r| r.get(0),
    )?)
}

pub(super) fn request(command: Command, args: &Value, c: &DelegatedContext) -> Result<Value> {
    if command == Command::Ack && args.get("ship").is_none() {
        return super::reader::local(command, args, c);
    }
    let ship = args["ship"].as_str().context("missing mailbox")?;
    ensure!(c.allows(ship), "mailbox not found");
    let local = super::members()?.iter().any(|(s, _)| s == ship);
    let (machine, state, retry, mut result) = if local {
        (
            "here".to_owned(),
            "complete",
            None,
            super::reader::local(command, args, c)?,
        )
    } else {
        let operation = match command {
            Command::Inbox => "inbox",
            Command::Show => "show",
            Command::Thread => "thread",
            Command::Ack => "ack",
            _ => unreachable!("reader command"),
        };
        let body = json!({"version":1,"origin":machine_id()?,"context":c.gateway(),"operation":operation,"args":args});
        let (machine, reply) =
            crate::machines::reader::read(ship, body)?.context("mailbox not found")?;
        use crate::machines::reader::Reply;
        match reply {
            Reply::Complete(result) => (machine, "complete", None, result),
            Reply::Unavailable(retry) => (machine, "unavailable", Some(retry), empty(command)),
            Reply::Unsupported => (machine, "unsupported", Some("upgrade_peer"), empty(command)),
        }
    };
    result["coverage"] = super::coverage::selected_mailbox(&machine, state, retry);
    Ok(result)
}

fn empty(command: Command) -> Value {
    match command {
        Command::Ack => json!({"acknowledged":0,"outcome":"unconfirmed"}),
        Command::Show => json!({"mail":null,"ref":null}),
        _ => json!({"items":[],"continuation":null}),
    }
}
