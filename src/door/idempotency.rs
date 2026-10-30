//! Core-owned at-most-once admission for scoped writes. A durable reservation
//! precedes effects; a crash leaves an uncertain result, never permission to
//! execute again. No registry transaction is held across a home/network write.
use super::scoped::{Command, DelegatedContext, Denial};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(super) const RETENTION_MS: i64 = 86_400_000;
const PER_GRANT: i64 = 256;
const GLOBAL: i64 = 4096;
const MAX_RESULT: usize = 131_072;

// Schema/cleanup DDL changes must also bump clients.rs SCHEMA_MARKER and its fingerprint.
fn schema(conn: &Connection) -> anyhow::Result<()> {
    super::schema::write(conn, || migrate(conn))
}

fn migrate(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS door_write_results(
        grant_id TEXT NOT NULL, command TEXT NOT NULL, key_hash BLOB NOT NULL,
        args_hash BLOB NOT NULL, created_ms INTEGER NOT NULL, result TEXT,
        PRIMARY KEY(grant_id,command,key_hash));
        CREATE TABLE IF NOT EXISTS door_work_keys(
        grant_id TEXT NOT NULL, resource TEXT NOT NULL, key_hash BLOB NOT NULL,
        args_hash BLOB NOT NULL, created_ms INTEGER NOT NULL,
        in_flight INTEGER NOT NULL DEFAULT 0, retained INTEGER NOT NULL DEFAULT 1,
        PRIMARY KEY(grant_id,resource,key_hash));",
    )?;
    // Version-1 pins have no known age: grant them a full retry window from
    // upgrade and retain them as uncertain. The writer lock serializes the
    // column probes and ALTERs with concurrent registry initialization.
    for (column, definition) in [
        (
            "created_ms",
            format!("INTEGER NOT NULL DEFAULT {}", crate::store::Store::now_ms()),
        ),
        ("in_flight", "INTEGER NOT NULL DEFAULT 0".into()),
        ("retained", "INTEGER NOT NULL DEFAULT 1".into()),
    ] {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('door_work_keys') WHERE name=?1)",
            [column],
            |r| r.get(0),
        )?;
        if !exists {
            conn.execute_batch(&format!(
                "ALTER TABLE door_work_keys ADD COLUMN {column} {definition}"
            ))?;
        }
    }
    Ok(())
}

/// Grant lifecycle cleanup is transactional with the registry edit, including
/// OAuth revocation/removal. Expired grants can be renewed, so their retry
/// records survive until retention ends (but do not consume live capacity).
pub(super) fn grant_cleanup(conn: &Connection) -> anyhow::Result<()> {
    super::schema::write(conn, || migrate_cleanup(conn))
}

fn migrate_cleanup(conn: &Connection) -> anyhow::Result<()> {
    schema(conn)?;
    // A new name replaces the legacy expiry-sensitive trigger exactly once.
    // Even a no-op CREATE can acquire SHARED: reserve the writer before it,
    // not in a deferred savepoint that later needs to CREATE or DROP a trigger.
    conn.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS door_write_grant_removed AFTER DELETE ON door_clients
         BEGIN DELETE FROM door_write_results WHERE grant_id=OLD.id; END;
         CREATE TRIGGER IF NOT EXISTS door_write_grant_revoked_only AFTER UPDATE OF revoked_ms ON door_clients
         WHEN NEW.revoked_ms IS NOT NULL
         BEGIN DELETE FROM door_write_results WHERE grant_id=NEW.id; END;
         CREATE TRIGGER IF NOT EXISTS door_work_grant_removed AFTER DELETE ON door_clients
         BEGIN DELETE FROM door_work_keys WHERE grant_id=OLD.id; END;
         CREATE TRIGGER IF NOT EXISTS door_work_grant_revoked AFTER UPDATE OF revoked_ms ON door_clients
         WHEN NEW.revoked_ms IS NOT NULL
         BEGIN DELETE FROM door_work_keys WHERE grant_id=NEW.id; END;
         DROP TRIGGER IF EXISTS door_write_grant_revoked;",
    )?;
    Ok(())
}

pub(super) fn writable(command: Command, args: &Value) -> bool {
    matches!(
        command,
        Command::Send
            | Command::Dismiss
            | Command::Ack
            | Command::FilePut
            | Command::JobCancel
            | Command::JobRetry
            | Command::ResourceConfigure
            | Command::WorkRank
            | Command::WorkEnqueue
            | Command::WorkDefer
    ) || (command == Command::Away && matches!(args["action"].as_str(), Some("start" | "back")))
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
    // V6a records results transactionally in the affected home, including
    // linked calls. Registry reservations cannot recover commit/response loss.
    if super::actions::is_action(command) || command == Command::ResourceConfigure {
        let key = key.ok_or(Denial::Arguments)?;
        // Pin keys globally at the originating core too: per-home result rows
        // alone would permit the same key to mutate two different homes/peers.
        // A matching pin ALWAYS reaches the home to recover commit/response
        // loss; it is never an uncertain reservation or an operator fallback.
        let mut conn = crate::fleet::registry().map_err(|_| Denial::AuthUnavailable)?;
        pin_work_key(&mut conn, command, args, key, context)
            .map_err(|_| Denial::AuthUnavailable)??;
        let outcome = execute();
        // Concurrent equal calls may be recovering the same committed result.
        // A pre-effect denial must not remove another call's active/saved pin.
        finish_work_key(
            &mut conn,
            key,
            context,
            &outcome,
            crate::store::Store::now_ms(),
        )
        .map_err(|_| Denial::OutcomeUnknown)?;
        return outcome;
    }
    let Some(key) = key else { return execute() };
    let name = match command {
        Command::Send => "send",
        Command::FilePut => "file.put",
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

fn pin_work_key(
    conn: &mut Connection,
    command: Command,
    args: &Value,
    key: &str,
    c: &DelegatedContext,
) -> anyhow::Result<Result<(), Denial>> {
    pin_work_key_at(conn, command, args, key, c, crate::store::Store::now_ms())
}

fn pin_work_key_at(
    conn: &mut Connection,
    command: Command,
    args: &Value,
    key: &str,
    c: &DelegatedContext,
    now: i64,
) -> anyhow::Result<Result<(), Denial>> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    schema(&tx)?;
    tx.execute(
        "DELETE FROM door_work_keys WHERE created_ms < ?1 OR NOT EXISTS
        (SELECT 1 FROM door_clients c WHERE c.id=door_work_keys.grant_id AND c.revoked_ms IS NULL)",
        [now - RETENTION_MS],
    )?;
    let key = Sha256::digest(key.as_bytes());
    let hash = Sha256::digest(
        json!([
            if command == Command::ResourceConfigure {
                "resource_configure"
            } else {
                super::actions::name(command)
            },
            args,
            c.capabilities(),
            c.visibility_revision()
        ])
        .to_string(),
    );
    let prior: Option<Vec<u8>> = tx.query_row("SELECT args_hash FROM door_work_keys WHERE grant_id=?1 AND resource=?2 AND key_hash=?3", params![c.client(), c.resource(), key.as_slice()], |r| r.get(0)).optional()?;
    let result = if let Some(prior) = prior {
        if prior == hash.as_slice() {
            tx.execute("UPDATE door_work_keys SET in_flight=in_flight+1,created_ms=?4 WHERE grant_id=?1 AND resource=?2 AND key_hash=?3",
                params![c.client(), c.resource(), key.as_slice(), now])?;
            Ok(())
        } else {
            Err(Denial::Arguments)
        }
    } else {
        let (total, own): (u64, u64) = tx.query_row(
            "SELECT count(*),COALESCE(sum(grant_id=?1),0) FROM door_work_keys
             WHERE EXISTS (SELECT 1 FROM door_clients c WHERE c.id=door_work_keys.grant_id
               AND c.revoked_ms IS NULL AND (c.expires_ms IS NULL OR c.expires_ms>?2))",
            params![c.client(), now],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if total >= GLOBAL as u64 || own >= PER_GRANT as u64 {
            Err(Denial::RetryCapacity)
        } else {
            tx.execute(
                "INSERT INTO door_work_keys(grant_id,resource,key_hash,args_hash,created_ms,in_flight,retained) VALUES(?1,?2,?3,?4,?5,1,0)",
                params![c.client(), c.resource(), key.as_slice(), hash.as_slice(), now],
            )?;
            Ok(())
        }
    };
    tx.commit()?;
    Ok(result)
}

fn finish_work_key(
    conn: &mut Connection,
    key: &str,
    c: &DelegatedContext,
    outcome: &Result<Value, Denial>,
    now: i64,
) -> anyhow::Result<()> {
    // Actions attest pre-commit failures by rolling back the home transaction;
    // transport/commit/post-response failures must use OutcomeUnknown instead.
    let retain = outcome.is_ok() || matches!(outcome, Err(Denial::OutcomeUnknown));
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let key = Sha256::digest(key.as_bytes());
    // Finish time conservatively covers the home's entire retry window, not
    // just the interval since the originating admission. Revocation can have
    // removed the row already, and must never resurrect it.
    tx.execute("UPDATE door_work_keys SET in_flight=max(0,in_flight-1),retained=max(retained,?4),created_ms=?5
        WHERE grant_id=?1 AND resource=?2 AND key_hash=?3",
        params![c.client(), c.resource(), key.as_slice(), retain, now])?;
    tx.execute("DELETE FROM door_work_keys WHERE grant_id=?1 AND resource=?2 AND key_hash=?3 AND in_flight=0 AND retained=0",
        params![c.client(), c.resource(), key.as_slice()])?;
    tx.commit()?;
    Ok(())
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
        Err(Denial::AuthUnavailable | Denial::ExecutionNotStarted | Denial::MessageTooLarge)
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
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    schema(&tx)?;
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
    fn resource_pins_bind_ledger_and_link_replacement_and_share_global_keys() {
        let mut conn = Connection::open_in_memory().unwrap();
        setup(&conn);
        let c = DelegatedContext::resolved(
            "g".into(),
            "door:alpha".into(),
            vec!["fleet:admin".into()],
            None,
        );
        let args = json!({"machine":"beta","changes":{"max_heavy_jobs":1},"idempotency_key":"alpha","_binding":{"ledger":"alpha","incarnation":"beta"}});
        pin_work_key(&mut conn, Command::ResourceConfigure, &args, "alpha", &c)
            .unwrap()
            .unwrap();
        for field in ["ledger", "incarnation"] {
            let mut changed = args.clone();
            changed["_binding"][field] = json!("gamma");
            assert_eq!(
                pin_work_key(&mut conn, Command::ResourceConfigure, &changed, "alpha", &c).unwrap(),
                Err(Denial::Arguments)
            );
        }
        assert_eq!(
            pin_work_key(&mut conn, Command::WorkRank, &args, "alpha", &c).unwrap(),
            Err(Denial::Arguments)
        );
        pin_work_key(&mut conn, Command::ResourceConfigure, &args, "alpha", &c)
            .unwrap()
            .unwrap();
    }

    #[test]
    fn admissions_acquire_the_writer_once_including_schema_migration() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        for (migrate_needed, pin) in [(true, true), (false, true), (true, false), (false, false)] {
            let mut conn = Connection::open_in_memory().unwrap();
            conn.execute_batch("CREATE TABLE door_clients(id TEXT PRIMARY KEY, revoked_ms INTEGER, expires_ms INTEGER);
                INSERT INTO door_clients(id) VALUES('g');").unwrap();
            if !migrate_needed {
                schema(&conn).unwrap();
            }
            let c = DelegatedContext::resolved(
                "g".into(),
                "door:alpha".into(),
                vec!["work:manage".into()],
                None,
            );
            let begins = Arc::new(AtomicUsize::new(0));
            let observed = begins.clone();
            // Count top-level BEGINs, not nested SAVEPOINTs. All admission
            // transactions explicitly use Immediate; a separate schema()
            // transaction before admission would make this count two.
            conn.authorizer(Some(move |ctx: AuthContext<'_>| {
                if matches!(
                    ctx.action,
                    AuthAction::Transaction {
                        operation: TransactionOperation::Begin
                    }
                ) {
                    observed.fetch_add(1, Ordering::SeqCst);
                }
                Authorization::Allow
            }));
            for attempt in 0..2 {
                if pin {
                    assert_eq!(
                        pin_work_key_at(&mut conn, Command::WorkRank, &json!({}), "key", &c, 0)
                            .unwrap(),
                        Ok(())
                    );
                } else {
                    let reservation = reserve(&mut conn, "g", "send", b"key", b"hash", 0).unwrap();
                    assert_eq!(
                        reservation,
                        if attempt == 0 {
                            Ok(None)
                        } else {
                            Err(Denial::OutcomeUnknown)
                        }
                    );
                }
                assert_eq!(begins.swap(0, Ordering::SeqCst), 1);
                assert!(conn.is_autocommit());
            }
        }
    }

    #[test]
    fn work_key_pins_bind_operations_homes_and_authority_without_uncertain_reservations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.sqlite");
        let mut conn = Connection::open(&path).unwrap();
        setup(&conn);
        let c = DelegatedContext::resolved(
            uuid::Uuid::new_v4().to_string(),
            "door:alpha".into(),
            vec!["work:manage".into()],
            None,
        );
        conn.execute("INSERT INTO door_clients(id) VALUES(?1)", [c.client()])
            .unwrap();
        let a = json!({"ship":"survey","reference":{"ship":"survey","task_id":"alpha","revision":1},"rank":1});
        assert!(
            pin_work_key(&mut conn, Command::WorkRank, &a, "alpha", &c)
                .unwrap()
                .is_ok()
        );
        drop(conn);
        let mut conn = Connection::open(path).unwrap();
        assert!(
            pin_work_key(&mut conn, Command::WorkRank, &a, "alpha", &c)
                .unwrap()
                .is_ok(),
            "restart retries must still reach the home's recorded result"
        );
        let mut other = a.clone();
        other["ship"] = json!("relay");
        assert_eq!(
            pin_work_key(&mut conn, Command::WorkRank, &other, "alpha", &c).unwrap(),
            Err(Denial::Arguments)
        );
        assert_eq!(
            pin_work_key(&mut conn, Command::WorkEnqueue, &a, "alpha", &c).unwrap(),
            Err(Denial::Arguments)
        );
        for n in 1..PER_GRANT {
            conn.execute(
                "INSERT INTO door_work_keys(grant_id,resource,key_hash,args_hash,created_ms) VALUES(?1,?2,?3,X'00',?4)",
                params![c.client(), c.resource(), n.to_string().as_bytes(), crate::store::Store::now_ms()],
            )
            .unwrap();
        }
        assert_eq!(
            pin_work_key(&mut conn, Command::WorkRank, &a, "new", &c).unwrap(),
            Err(Denial::RetryCapacity)
        );
        assert!(
            pin_work_key(&mut conn, Command::WorkRank, &a, "alpha", &c)
                .unwrap()
                .is_ok(),
            "full capacity must not lose a previously pinned retry"
        );
        let revised = c.with_visibility_revision(1);
        assert_eq!(
            pin_work_key(&mut conn, Command::WorkRank, &a, "alpha", &revised).unwrap(),
            Err(Denial::Arguments)
        );
    }

    fn work_fixture(conn: &Connection) -> (DelegatedContext, Value) {
        let c = DelegatedContext::resolved(
            uuid::Uuid::new_v4().to_string(),
            "door:alpha".into(),
            vec!["work:manage".into()],
            None,
        );
        conn.execute("INSERT INTO door_clients(id) VALUES(?1)", [c.client()])
            .unwrap();
        (c, json!({"ship":"survey","rank":1}))
    }

    #[test]
    fn work_pins_expire_after_window_for_busy_grants_and_full_registry() {
        let mut conn = Connection::open_in_memory().unwrap();
        setup(&conn);
        let now = RETENTION_MS * 10;
        let (c, a) = work_fixture(&conn);
        for (round, at) in [
            (0, now),
            (1, now + RETENTION_MS + 1),
            (2, now + 2 * (RETENTION_MS + 1)),
        ] {
            for n in 0..PER_GRANT {
                let key = format!("{round}-{n}");
                pin_work_key_at(&mut conn, Command::WorkRank, &a, &key, &c, at)
                    .unwrap()
                    .unwrap();
                finish_work_key(&mut conn, &key, &c, &Ok(json!({"saved":true})), at).unwrap();
            }
            assert_eq!(
                pin_work_key_at(&mut conn, Command::WorkRank, &a, "full", &c, at).unwrap(),
                Err(Denial::RetryCapacity)
            );
        }
        assert_eq!(
            conn.query_row("SELECT count(*) FROM door_work_keys", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            PER_GRANT
        );
        conn.execute("DELETE FROM door_work_keys", []).unwrap();
        let mut contexts = vec![];
        for _ in 0..16 {
            let (c, a) = work_fixture(&conn);
            for n in 0..PER_GRANT {
                let key = n.to_string();
                pin_work_key_at(&mut conn, Command::WorkRank, &a, &key, &c, now)
                    .unwrap()
                    .unwrap();
                finish_work_key(&mut conn, &key, &c, &Ok(json!({})), now).unwrap();
            }
            contexts.push((c, a));
        }
        let (fresh, a) = work_fixture(&conn);
        assert_eq!(
            pin_work_key_at(&mut conn, Command::WorkRank, &a, "full", &fresh, now).unwrap(),
            Err(Denial::RetryCapacity)
        );
        let (c, a) = &contexts[0];
        pin_work_key_at(&mut conn, Command::WorkRank, a, "0", c, now + RETENTION_MS)
            .unwrap()
            .unwrap();
        assert_eq!(
            pin_work_key_at(
                &mut conn,
                Command::WorkRank,
                a,
                "changed",
                &fresh,
                now + RETENTION_MS
            )
            .unwrap(),
            Err(Denial::RetryCapacity)
        );
        pin_work_key_at(
            &mut conn,
            Command::WorkRank,
            a,
            "after",
            &fresh,
            now + RETENTION_MS + 1,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM door_work_keys", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn denied_work_does_not_pin_but_concurrent_success_and_crash_remain_protected() {
        let mut conn = Connection::open_in_memory().unwrap();
        setup(&conn);
        let (c, a) = work_fixture(&conn);
        let now = RETENTION_MS * 10;
        for denial in [
            Denial::Ship,
            Denial::Scope,
            Denial::Arguments,
            Denial::ExecutionFailed,
            Denial::AuthUnavailable,
            Denial::Unsupported,
        ] {
            for n in 0..300 {
                let key = n.to_string();
                pin_work_key_at(&mut conn, Command::WorkRank, &a, &key, &c, now)
                    .unwrap()
                    .unwrap();
                finish_work_key(&mut conn, &key, &c, &Err(denial), now).unwrap();
            }
        }
        assert_eq!(
            conn.query_row("SELECT count(*) FROM door_work_keys", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        // Two equal requests: a denied one must not recycle the other's pin.
        for _ in 0..2 {
            pin_work_key_at(&mut conn, Command::WorkRank, &a, "equal", &c, now)
                .unwrap()
                .unwrap();
        }
        finish_work_key(&mut conn, "equal", &c, &Err(Denial::Ship), now).unwrap();
        assert_eq!(
            pin_work_key_at(&mut conn, Command::WorkEnqueue, &a, "equal", &c, now).unwrap(),
            Err(Denial::Arguments)
        );
        finish_work_key(&mut conn, "equal", &c, &Ok(json!({})), now).unwrap();
        pin_work_key_at(&mut conn, Command::WorkRank, &a, "equal", &c, now)
            .unwrap()
            .unwrap();
        finish_work_key(&mut conn, "equal", &c, &Err(Denial::Ship), now).unwrap();
        assert_eq!(
            pin_work_key_at(&mut conn, Command::WorkEnqueue, &a, "equal", &c, now).unwrap(),
            Err(Denial::Arguments)
        );
        pin_work_key_at(&mut conn, Command::WorkRank, &a, "uncertain", &c, now)
            .unwrap()
            .unwrap();
        finish_work_key(
            &mut conn,
            "uncertain",
            &c,
            &Err(Denial::OutcomeUnknown),
            now,
        )
        .unwrap();
        assert_eq!(
            pin_work_key_at(&mut conn, Command::WorkEnqueue, &a, "uncertain", &c, now).unwrap(),
            Err(Denial::Arguments)
        );
        // Crash before finishing the reservation also remains conservatively pinned.
        pin_work_key_at(&mut conn, Command::WorkRank, &a, "crash", &c, now)
            .unwrap()
            .unwrap();
        assert_eq!(
            pin_work_key_at(&mut conn, Command::WorkEnqueue, &a, "crash", &c, now).unwrap(),
            Err(Denial::Arguments)
        );
    }

    #[test]
    fn legacy_work_pin_upgrade_and_grant_lifecycle_cleanup() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE door_clients(id TEXT PRIMARY KEY,revoked_ms INTEGER,expires_ms INTEGER);
            CREATE TABLE door_work_keys(grant_id TEXT,resource TEXT,key_hash BLOB,args_hash BLOB,PRIMARY KEY(grant_id,resource,key_hash));
            INSERT INTO door_clients(id) VALUES('alpha'),('beta');
            INSERT INTO door_work_keys VALUES('alpha','door:alpha',X'01',X'02'),('beta','door:alpha',X'02',X'03');").unwrap();
        let before = crate::store::Store::now_ms();
        grant_cleanup(&conn).unwrap();
        let created: i64 = conn
            .query_row("SELECT min(created_ms) FROM door_work_keys", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(created >= before);
        assert_eq!(
            conn.query_row("SELECT sum(retained) FROM door_work_keys", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        conn.execute("UPDATE door_clients SET expires_ms=1 WHERE id='alpha'", [])
            .unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM door_work_keys", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        conn.execute("UPDATE door_clients SET revoked_ms=1 WHERE id='alpha'", [])
            .unwrap();
        conn.execute("DELETE FROM door_clients WHERE id='beta'", [])
            .unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM door_work_keys", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        // Finishing an admitted operation cannot resurrect a revoked key.
        let (c, a) = work_fixture(&conn);
        pin_work_key_at(&mut conn, Command::WorkRank, &a, "revoke", &c, before)
            .unwrap()
            .unwrap();
        conn.execute(
            "UPDATE door_clients SET revoked_ms=1 WHERE id=?1",
            [c.client()],
        )
        .unwrap();
        finish_work_key(&mut conn, "revoke", &c, &Ok(json!({})), before).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM door_work_keys", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn repeated_cleanup_does_not_change_schema() {
        let conn = Connection::open_in_memory().unwrap();
        setup(&conn);
        let version: i64 = conn
            .pragma_query_value(None, "schema_version", |r| r.get(0))
            .unwrap();
        for _ in 0..2 {
            grant_cleanup(&conn).unwrap();
            assert_eq!(
                conn.pragma_query_value::<i64, _>(None, "schema_version", |r| r.get(0))
                    .unwrap(),
                version
            );
        }
    }

    #[test]
    fn legacy_cleanup_upgrade_preserves_rows_and_changes_schema_only_once() {
        let mut conn = Connection::open_in_memory().unwrap();
        setup(&conn);
        conn.execute_batch(
            "DROP TRIGGER door_write_grant_revoked_only;
             CREATE TRIGGER door_write_grant_revoked AFTER UPDATE OF revoked_ms,expires_ms ON door_clients
             WHEN NEW.revoked_ms IS NOT NULL OR NEW.expires_ms <= CAST(strftime('%s','now') AS INTEGER)*1000
             BEGIN DELETE FROM door_write_results WHERE grant_id=NEW.id; END;",
        ).unwrap();
        let now = crate::store::Store::now_ms();
        reserve(&mut conn, "g", "send", b"k", b"h", now)
            .unwrap()
            .unwrap();
        let original = json!({"message_id":"original"});
        save(&conn, "g", "send", b"k", Ok(original.clone())).unwrap();
        let before: i64 = conn
            .pragma_query_value(None, "schema_version", |r| r.get(0))
            .unwrap();
        grant_cleanup(&conn).unwrap();
        let after: i64 = conn
            .pragma_query_value(None, "schema_version", |r| r.get(0))
            .unwrap();
        assert!(after > before);
        for _ in 0..2 {
            grant_cleanup(&conn).unwrap();
            assert_eq!(
                conn.pragma_query_value::<i64, _>(None, "schema_version", |r| r.get(0))
                    .unwrap(),
                after
            );
        }
        conn.execute("UPDATE door_clients SET expires_ms=1 WHERE id='g'", [])
            .unwrap();
        conn.execute("UPDATE door_clients SET expires_ms=NULL WHERE id='g'", [])
            .unwrap();
        let cached = reserve(&mut conn, "g", "send", b"k", b"h", now)
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(decode(&cached), Ok(original));
        conn.execute("UPDATE door_clients SET revoked_ms=?1 WHERE id='g'", [now])
            .unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM door_write_results", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
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
        for reason in [
            Denial::AuthUnavailable,
            Denial::ExecutionNotStarted,
            Denial::MessageTooLarge,
        ] {
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
