//! Core-owned at-most-once admission for scoped writes. A durable reservation
//! precedes effects; a crash leaves an uncertain result, never permission to
//! execute again. No registry transaction is held across a home/network write.
use super::scoped::{Command, DelegatedContext, Denial};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const RETENTION_MS: i64 = 86_400_000;
const PER_GRANT: i64 = 256;
const GLOBAL: i64 = 4096;
const MAX_RESULT: usize = 131_072;

fn schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS door_write_results(
        grant_id TEXT NOT NULL, command TEXT NOT NULL, key_hash BLOB NOT NULL,
        args_hash BLOB NOT NULL, created_ms INTEGER NOT NULL, result TEXT,
        PRIMARY KEY(grant_id,command,key_hash));",
    )?;
    Ok(())
}

pub(super) fn writable(command: Command, args: &Value) -> bool {
    matches!(command, Command::Send | Command::Dismiss | Command::Ack)
        || (command == Command::Away && matches!(args["action"].as_str(), Some("start" | "back")))
}

/// Args have already passed the fixed command schema. Defaults are explicit so
/// equivalent wire encodings hash identically; serde_json maps sort their keys.
fn normalized(command: Command, args: &Value, context: &DelegatedContext) -> Value {
    let mut args = args.clone();
    if command == Command::Send {
        if args.get("kind").is_none() {
            args["kind"] = json!("request");
        }
        if args.get("urgency").is_none() {
            args["urgency"] = json!("normal");
        }
        if args.get("payload").is_none() {
            args["payload"] = json!({});
        }
    }
    // A restriction edit cannot use a cached result to reveal a now-hidden
    // target. Conservatively refuse retries after any authority change.
    json!({"args":args,"authority":context.capabilities(),"resource":context.resource()})
}

pub(super) fn run(
    command: Command,
    args: &Value,
    key: Option<&str>,
    context: &DelegatedContext,
    execute: impl FnOnce() -> Result<Value, Denial>,
) -> Result<Value, Denial> {
    let Some(key) = key else { return execute() };
    let name = match command {
        Command::Send => "send",
        Command::Dismiss => "dismiss",
        Command::Ack => "ack",
        Command::Away => "away",
        _ => return Err(Denial::Arguments),
    };
    let hash = Sha256::digest(normalized(command, args, context).to_string());
    let key = Sha256::digest(key.as_bytes());
    let mut conn = crate::fleet::registry().map_err(|_| Denial::AuthUnavailable)?;
    let cached = reserve(
        &mut conn,
        context.client(),
        name,
        &key,
        &hash,
        crate::store::Store::now_ms(),
    )
    .map_err(|_| Denial::AuthUnavailable)??;
    if let Some(cached) = cached {
        return decode(&cached);
    }
    let outcome = execute();
    let stored = match &outcome {
        Ok(value) => json!({"ok":value}),
        Err(reason) => json!({"denial":reason.reason()}),
    }
    .to_string();
    // Leave the reservation uncertain if output cannot be saved. Never remove
    // it on a post-effect failure, even if execution returned an error.
    if stored.len() > MAX_RESULT {
        return Err(Denial::OutcomeUnknown);
    }
    conn.execute(
        "UPDATE door_write_results SET result=?4 WHERE grant_id=?1 AND command=?2 AND key_hash=?3",
        params![context.client(), name, key.as_slice(), stored],
    )
    .map_err(|_| Denial::OutcomeUnknown)?;
    outcome
}

fn decode(stored: &str) -> Result<Value, Denial> {
    let value: Value = serde_json::from_str(stored).map_err(|_| Denial::OutcomeUnknown)?;
    if let Some(value) = value.get("ok") {
        return Ok(value.clone());
    }
    // Saved denials include admission-independent execution and audit failures.
    let code = value["denial"].as_str().unwrap_or_default();
    Err(if code == Denial::AuthUnavailable.reason() {
        Denial::AuthUnavailable
    } else {
        Denial::from_code(code).unwrap_or(Denial::OutcomeUnknown)
    })
}

type Reservation = Result<Option<String>, Denial>;
fn reserve(
    conn: &mut Connection,
    grant: &str,
    command: &str,
    key: &[u8],
    hash: &[u8],
    now: i64,
) -> anyhow::Result<Reservation> {
    schema(conn)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "DELETE FROM door_write_results WHERE created_ms < ?1",
        [now - RETENTION_MS],
    )?;
    let prior: Option<(Vec<u8>, Option<String>)> = tx.query_row(
        "SELECT args_hash,result FROM door_write_results WHERE grant_id=?1 AND command=?2 AND key_hash=?3",
        params![grant,command,key], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
    let result = if let Some((prior_hash, result)) = prior {
        if prior_hash != hash {
            Err(Denial::Arguments)
        } else {
            result.map(Some).ok_or(Denial::OutcomeUnknown)
        }
    } else {
        let (all, own): (i64, i64) = tx.query_row(
            "SELECT COUNT(*),COALESCE(SUM(grant_id=?1),0) FROM door_write_results",
            [grant],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if all >= GLOBAL || own >= PER_GRANT {
            Err(Denial::OutcomeUnknown)
        } else {
            tx.execute("INSERT INTO door_write_results(grant_id,command,key_hash,args_hash,created_ms) VALUES(?1,?2,?3,?4,?5)",
                params![grant,command,key,hash,now])?;
            Ok(None)
        }
    };
    tx.commit()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reservation_restart_conflict_retention_and_bounds() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("registry.sqlite");
        let now = RETENTION_MS * 2;
        let mut conn = Connection::open(&path).unwrap();
        assert_eq!(
            reserve(&mut conn, "g", "send", b"k", b"hash", now)
                .unwrap()
                .unwrap(),
            None
        );
        drop(conn);
        let mut conn = Connection::open(&path).unwrap();
        assert_eq!(
            reserve(&mut conn, "g", "send", b"k", b"hash", now).unwrap(),
            Err(Denial::OutcomeUnknown)
        );
        assert_eq!(
            reserve(&mut conn, "g", "send", b"k", b"changed", now).unwrap(),
            Err(Denial::Arguments)
        );
        conn.execute("UPDATE door_write_results SET result='saved'", [])
            .unwrap();
        assert_eq!(
            reserve(&mut conn, "g", "send", b"k", b"hash", now)
                .unwrap()
                .unwrap(),
            Some("saved".into())
        );
        for n in 1..PER_GRANT {
            reserve(
                &mut conn,
                "g",
                "send",
                n.to_string().as_bytes(),
                b"hash",
                now,
            )
            .unwrap()
            .unwrap();
        }
        assert_eq!(
            reserve(&mut conn, "g", "send", b"full", b"hash", now).unwrap(),
            Err(Denial::OutcomeUnknown)
        );
        assert!(
            reserve(&mut conn, "other", "send", b"full", b"hash", now)
                .unwrap()
                .is_ok()
        );
        assert_eq!(
            reserve(&mut conn, "g", "send", b"k", b"new", now + RETENTION_MS + 1)
                .unwrap()
                .unwrap(),
            None
        );
    }

    #[test]
    fn global_capacity_and_result_roundtrip() {
        let mut conn = Connection::open_in_memory().unwrap();
        schema(&conn).unwrap();
        let now = RETENTION_MS * 2;
        conn.execute(
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<?1)
            INSERT INTO door_write_results(grant_id,command,key_hash,args_hash,created_ms)
            SELECT 'g-'||x,'send',x,x,?2 FROM n",
            params![GLOBAL, now],
        )
        .unwrap();
        assert_eq!(
            reserve(&mut conn, "new", "send", b"k", b"hash", now).unwrap(),
            Err(Denial::OutcomeUnknown)
        );
        let original = json!({"message_id":"stable", "provenance":{"result_id":"original"}});
        assert_eq!(
            decode(&json!({"ok": original}).to_string()).unwrap(),
            original
        );
        assert_eq!(
            decode(&json!({"denial":Denial::AuthUnavailable.reason()}).to_string()),
            Err(Denial::AuthUnavailable)
        );
    }

    #[test]
    fn racing_connections_reserve_once() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("registry.sqlite");
        schema(&Connection::open(&path).unwrap()).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut conn = Connection::open(path).unwrap();
                    conn.busy_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                    barrier.wait();
                    reserve(&mut conn, "g", "send", b"k", b"hash", RETENTION_MS * 2).unwrap()
                })
            })
            .collect();
        let outcomes: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(outcomes.iter().filter(|r| **r == Ok(None)).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|r| **r == Err(Denial::OutcomeUnknown))
                .count(),
            1
        );
    }
}
