//! Ship-addressed merged local/linked reader surface. Coverage is for the
//! selected mailbox, never an assertion that unqueried machines were read.
use super::scoped::{Command, DelegatedContext};
use anyhow::{Context, Result, ensure};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};

pub(super) fn machine_id() -> Result<String> {
    let conn = crate::fleet::registry()?;
    machine_id_in(&conn)
}

pub(crate) fn machine_id_in(conn: &rusqlite::Connection) -> Result<String> {
    // Existing identity reads must work under a registry reader/writer lock
    // and PRAGMA query_only. Even INSERT OR IGNORE takes a writer lock.
    let installed: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='door_reader_identity')", [], |r| r.get(0))?;
    if installed
        && let Some(id) = conn
            .query_row(
                "SELECT id FROM door_reader_identity WHERE only=1",
                [],
                |r| r.get(0),
            )
            .optional()?
    {
        return Ok(id);
    }
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

#[cfg(test)]
mod identity_tests {
    use super::*;

    #[test]
    fn existing_identity_lookup_is_read_only_with_an_independent_writer() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("alpha.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL").unwrap();
        let identity = machine_id_in(&conn).unwrap();
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        conn.pragma_update(None, "query_only", true).unwrap();
        let before = conn.total_changes();
        assert_eq!(machine_id_in(&conn).unwrap(), identity);
        assert_eq!(conn.total_changes(), before);
        writer.execute_batch("ROLLBACK").unwrap();
        let read_only = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        assert_eq!(machine_id_in(&read_only).unwrap(), identity);
        assert_eq!(read_only.total_changes(), 0);
    }

    #[test]
    fn missing_identity_cannot_be_initialized_by_query_only_status() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "query_only", true).unwrap();
        assert!(machine_id_in(&conn).is_err());
        assert_eq!(conn.total_changes(), 0);
        assert!(
            !conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='door_reader_identity')",
                    [],
                    |r| r.get::<_, bool>(0)
                )
                .unwrap()
        );
    }
}

fn empty(command: Command) -> Value {
    match command {
        Command::Ack => json!({"acknowledged":0,"outcome":"unconfirmed"}),
        Command::Show => json!({"mail":null,"ref":null}),
        _ => json!({"items":[],"continuation":null}),
    }
}
