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

/// Grant lifecycle cleanup is transactional with the registry edit, including
/// OAuth revocation/removal. Expired grants can be renewed, so their retry
/// records survive until retention ends (but do not consume live capacity).
pub(super) fn grant_cleanup(conn: &Connection) -> anyhow::Result<()> {
    schema(conn)?;
    conn.execute_batch(
        "SAVEPOINT door_write_cleanup;
         CREATE TRIGGER IF NOT EXISTS door_write_grant_removed AFTER DELETE ON door_clients
         BEGIN DELETE FROM door_write_results WHERE grant_id=OLD.id; END;
         DROP TRIGGER IF EXISTS door_write_grant_revoked;
         CREATE TRIGGER door_write_grant_revoked AFTER UPDATE OF revoked_ms ON door_clients
         WHEN NEW.revoked_ms IS NOT NULL
         BEGIN DELETE FROM door_write_results WHERE grant_id=NEW.id; END;
         RELEASE door_write_cleanup;",
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
    replayed: &mut bool,
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
        *replayed = true;
        return decode(&cached);
    }
    save(&conn, context.client(), name, &key, execute())
}

fn save(
    conn: &Connection,
    grant: &str,
    command: &str,
    key: &[u8],
    outcome: Result<Value, Denial>,
) -> Result<Value, Denial> {
    // ExecutionFailed is deliberately NOT listed: some callers can fail after
    // effects. Early execution failures must explicitly attest no effect.
    if matches!(
        outcome,
        Err(Denial::AuthUnavailable | Denial::ExecutionNotStarted)
    ) {
        conn.execute(
            "DELETE FROM door_write_results WHERE grant_id=?1 AND command=?2 AND key_hash=?3",
            params![grant, command, key],
        )
        .map_err(|_| Denial::OutcomeUnknown)?;
        return outcome;
    }
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
    let updated = conn.execute(
        "UPDATE door_write_results SET result=?4 WHERE grant_id=?1 AND command=?2 AND key_hash=?3",
        params![grant, command, key, stored],
    )
    .map_err(|_| Denial::OutcomeUnknown)?;
    if updated != 1 {
        return Err(Denial::OutcomeUnknown);
    }
    outcome
}

fn decode(stored: &str) -> Result<Value, Denial> {
    let value: Value = serde_json::from_str(stored).map_err(|_| Denial::OutcomeUnknown)?;
    if let Some(value) = value.get("ok") {
        return Ok(value.clone());
    }
    // Saved denials include uncertain execution and audit failures.
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
        "DELETE FROM door_write_results WHERE created_ms < ?1 OR NOT EXISTS
         (SELECT 1 FROM door_clients c WHERE c.id=door_write_results.grant_id
          AND c.revoked_ms IS NULL)",
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
            "SELECT COUNT(*),COALESCE(SUM(grant_id=?1),0) FROM door_write_results
             WHERE EXISTS (SELECT 1 FROM door_clients c WHERE c.id=door_write_results.grant_id
               AND c.revoked_ms IS NULL AND (c.expires_ms IS NULL OR c.expires_ms>?2))",
            params![grant, now],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if all >= GLOBAL || own >= PER_GRANT {
            Err(Denial::RetryCapacity)
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

    fn setup(conn: &Connection) {
        conn.execute_batch("CREATE TABLE door_clients(id TEXT PRIMARY KEY, revoked_ms INTEGER, expires_ms INTEGER);
            INSERT INTO door_clients(id) VALUES('g'),('other'),('new');").unwrap();
        grant_cleanup(conn).unwrap();
    }

    #[test]
    fn reservation_restart_conflict_retention_and_bounds() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("registry.sqlite");
        let now = RETENTION_MS * 2;
        let mut conn = Connection::open(&path).unwrap();
        setup(&conn);
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
            Err(Denial::RetryCapacity)
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
        setup(&conn);
        let now = RETENTION_MS * 2;
        conn.execute(
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<?1)
            INSERT INTO door_write_results(grant_id,command,key_hash,args_hash,created_ms)
            SELECT 'g-'||x,'send',x,x,?2 FROM n",
            params![GLOBAL, now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO door_clients(id) SELECT grant_id FROM door_write_results",
            [],
        )
        .unwrap();
        assert_eq!(
            reserve(&mut conn, "new", "send", b"k", b"hash", now).unwrap(),
            Err(Denial::RetryCapacity)
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
    fn grant_revocation_and_removal_purge_but_expiry_preserves_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        setup(&conn);
        let now = crate::store::Store::now_ms();
        for grant in ["g", "other", "new"] {
            reserve(&mut conn, grant, "send", b"k", b"h", now)
                .unwrap()
                .unwrap();
        }
        conn.execute("UPDATE door_clients SET revoked_ms=?1 WHERE id='g'", [now])
            .unwrap();
        conn.execute("DELETE FROM door_clients WHERE id='other'", [])
            .unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM door_write_results", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        // Expiry must not erase retry protection, whether time passes or the
        // operator shortens the expiry directly.
        conn.execute(
            "UPDATE door_clients SET expires_ms=?1 WHERE id='new'",
            [now + 10],
        )
        .unwrap();
        conn.execute("INSERT INTO door_clients(id) VALUES('live')", [])
            .unwrap();
        reserve(&mut conn, "live", "send", b"k", b"h", now + 11)
            .unwrap()
            .unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM door_write_results WHERE grant_id='new'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        conn.execute("UPDATE door_clients SET expires_ms=1 WHERE id='live'", [])
            .unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM door_write_results", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn expired_grants_keep_results_but_do_not_consume_capacity() {
        let mut conn = Connection::open_in_memory().unwrap();
        setup(&conn);
        let now = crate::store::Store::now_ms();
        // Model an upgrade from the previous cleanup trigger.
        conn.execute_batch(
            "DROP TRIGGER door_write_grant_revoked;
             CREATE TRIGGER door_write_grant_revoked AFTER UPDATE OF revoked_ms,expires_ms ON door_clients
             WHEN NEW.revoked_ms IS NOT NULL OR NEW.expires_ms <= CAST(strftime('%s','now') AS INTEGER)*1000
             BEGIN DELETE FROM door_write_results WHERE grant_id=NEW.id; END;",
        ).unwrap();
        grant_cleanup(&conn).unwrap();
        conn.execute(
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<?1)
             INSERT INTO door_write_results(grant_id,command,key_hash,args_hash,created_ms,result)
             SELECT 'g','send',CAST(x AS BLOB),CAST(x AS BLOB),?2,'{\"ok\":{\"message_id\":\"original\"}}' FROM n",
            params![GLOBAL, now],
        ).unwrap();
        conn.execute("UPDATE door_clients SET expires_ms=1 WHERE id='g'", [])
            .unwrap();
        // Another grant's keyed write must neither be blocked by the expired
        // grant's rows nor purge them before that grant can be renewed.
        assert_eq!(
            reserve(&mut conn, "other", "send", b"k", b"h", now).unwrap(),
            Ok(None)
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM door_write_results WHERE grant_id='g'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            GLOBAL
        );
        conn.execute("UPDATE door_clients SET expires_ms=NULL WHERE id='g'", [])
            .unwrap();
        let cached = reserve(&mut conn, "g", "send", b"1", b"1", now)
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(decode(&cached), Ok(json!({"message_id":"original"})));
    }

    #[test]
    fn pre_effect_cleanup_failure_is_uncertain_on_first_call_and_retry() {
        let mut conn = Connection::open_in_memory().unwrap();
        setup(&conn);
        let now = RETENTION_MS * 2;
        reserve(&mut conn, "g", "send", b"k", b"h", now)
            .unwrap()
            .unwrap();
        conn.execute_batch("CREATE TRIGGER fail_delete BEFORE DELETE ON door_write_results BEGIN SELECT RAISE(FAIL,'fixture'); END;").unwrap();
        assert_eq!(
            save(&conn, "g", "send", b"k", Err(Denial::ExecutionNotStarted)),
            Err(Denial::OutcomeUnknown)
        );
        assert_eq!(
            reserve(&mut conn, "g", "send", b"k", b"h", now).unwrap(),
            Err(Denial::OutcomeUnknown)
        );
    }

    #[test]
    fn pre_effect_errors_release_but_uncertain_errors_and_save_failures_do_not() {
        let mut conn = Connection::open_in_memory().unwrap();
        setup(&conn);
        let now = RETENTION_MS * 2;
        for reason in [Denial::AuthUnavailable, Denial::ExecutionNotStarted] {
            assert_eq!(
                reserve(&mut conn, "g", "send", b"k", b"h", now).unwrap(),
                Ok(None)
            );
            assert_eq!(save(&conn, "g", "send", b"k", Err(reason)), Err(reason));
        }
        assert_eq!(
            reserve(&mut conn, "g", "send", b"k", b"h", now).unwrap(),
            Ok(None)
        );
        assert_eq!(
            save(&conn, "g", "send", b"k", Err(Denial::ExecutionFailed)),
            Err(Denial::ExecutionFailed)
        );
        let cached = reserve(&mut conn, "g", "send", b"k", b"h", now)
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(decode(&cached), Err(Denial::ExecutionFailed));
        for key in [b"oversize".as_slice(), b"db-failure"] {
            reserve(&mut conn, "g", "send", key, b"h", now)
                .unwrap()
                .unwrap();
            let value = if key == b"oversize" {
                json!({"body":"x".repeat(MAX_RESULT)})
            } else {
                conn.execute_batch("CREATE TRIGGER fail_save BEFORE UPDATE ON door_write_results BEGIN SELECT RAISE(FAIL,'fixture'); END;").unwrap();
                json!({"committed":true})
            };
            assert_eq!(
                save(&conn, "g", "send", key, Ok(value)),
                Err(Denial::OutcomeUnknown)
            );
            assert_eq!(
                reserve(&mut conn, "g", "send", key, b"h", now).unwrap(),
                Err(Denial::OutcomeUnknown)
            );
        }
    }

    #[test]
    fn racing_connections_reserve_once() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("registry.sqlite");
        setup(&Connection::open(&path).unwrap());
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
