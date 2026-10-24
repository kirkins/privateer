//! Local owner-issued credentials, distinct from ship-management grants.
//! No RPC/door dispatch entry point: issuance requires a controlling terminal.
//! Env markers and /dev/tty are not a barrier against a same-user agent with
//! a shell; the actual agent barrier is the harness permission policy.
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use subtle::ConstantTimeEq;

use super::scoped::{DelegatedContext, Denial};
use crate::store::{Store, identifier};

const AUDIT_LIMIT: i64 = 4096;
const REQUEST_AUDIT_LIMIT: i64 = 1024;
const REQUEST_BUCKET_MS: i64 = 60_000;
pub(super) const SCOPES: &[&str] = &[
    "fleet:read",
    "mail:read",
    "ship:message",
    "work:manage",
    "file:transfer",
    "approval:answer",
    "fleet:admin",
    "captain:presence",
];
const USAGE: &str = "usage: privateer door client issue DOOR --name NAME [--scopes CSV] [--ships CSV] [--expires-ms UNIX_MS] | list [DOOR] | inspect ID | pause ID | resume ID | revoke ID | rotate ID | expiry ID --expires-ms UNIX_MS|none | audit";

fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS door_clients(
            id TEXT PRIMARY KEY, name TEXT NOT NULL, door TEXT NOT NULL,
            resource TEXT NOT NULL, owner TEXT NOT NULL, registration TEXT NOT NULL,
            scopes TEXT NOT NULL, ships TEXT, expires_ms INTEGER, paused INTEGER NOT NULL DEFAULT 0,
            revoked_ms INTEGER, created_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL,
            last_use_ms INTEGER, expiry_noted INTEGER NOT NULL DEFAULT 0,
            token_hash BLOB NOT NULL UNIQUE CHECK(length(token_hash)=32));
         CREATE TABLE IF NOT EXISTS door_client_audit(
            seq INTEGER PRIMARY KEY, at_ms INTEGER NOT NULL, machine TEXT NOT NULL,
            door TEXT NOT NULL, client TEXT, event TEXT NOT NULL, reason TEXT NOT NULL);",
    )?;
    // Migrate the old mixed ring once, preserving its surviving request facts.
    // SAVEPOINT also works when called from a door-registration transaction.
    let requests_exist: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='door_client_request_audit')",
        [],
        |r| r.get(0),
    )?;
    if !requests_exist {
        conn.execute_batch(
            "SAVEPOINT client_audit_migration;
             CREATE TABLE IF NOT EXISTS door_client_request_audit(
                seq INTEGER NOT NULL, at_ms INTEGER NOT NULL, machine TEXT NOT NULL,
                door TEXT NOT NULL, client TEXT, event TEXT NOT NULL, reason TEXT NOT NULL,
                bucket INTEGER NOT NULL, count INTEGER NOT NULL DEFAULT 1,
                PRIMARY KEY(door,seq));
             CREATE UNIQUE INDEX IF NOT EXISTS door_request_bucket
                ON door_client_request_audit(door,event,reason,bucket) WHERE client IS NULL;
             INSERT OR IGNORE INTO door_client_request_audit(seq,at_ms,machine,door,client,event,reason,bucket)
                SELECT seq,at_ms,machine,door,client,event,reason,seq
                FROM door_client_audit WHERE event IN ('auth.success','auth.failure','request.deny');
             DELETE FROM door_client_audit WHERE event IN ('auth.success','auth.failure','request.deny');
             DELETE FROM door_client_request_audit AS a WHERE seq NOT IN
                (SELECT seq FROM door_client_request_audit WHERE door=a.door ORDER BY seq DESC LIMIT 1024);
             RELEASE client_audit_migration;",
        )?;
    }
    for (column, definition) in [("last_seen_ms", "INTEGER"), ("result_id", "TEXT")] {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('door_client_request_audit') WHERE name=?1)",
            [column], |r| r.get(0))?;
        if !exists {
            let result = conn.execute_batch(&format!(
                "ALTER TABLE door_client_request_audit ADD COLUMN {column} {definition}"
            ));
            if let Err(error) = result {
                if !error.to_string().contains("duplicate column name") {
                    return Err(error.into());
                }
            }
        }
    }
    Ok(())
}

// Inputs are exclusively core-generated identifiers or validated registry names
// and fixed enums, never request IDs, tokens, arguments or adapter stderr.
fn audit(
    conn: &Connection,
    door: &str,
    client: Option<&str>,
    event: &str,
    reason: &str,
) -> Result<()> {
    if matches!(event, "auth.success" | "auth.failure" | "request.deny") {
        return request_audit(conn, door, client, event, reason);
    }
    conn.execute(
        "INSERT INTO door_client_audit(at_ms,machine,door,client,event,reason) VALUES(?1,?2,?3,?4,?5,?6)",
        params![Store::now_ms(), crate::machines::this_machine(), door, client, event, reason],
    )?;
    conn.execute(
        "DELETE FROM door_client_audit WHERE seq NOT IN (SELECT seq FROM door_client_audit ORDER BY seq DESC LIMIT ?1)",
        [AUDIT_LIMIT],
    )?;
    Ok(())
}

fn request_audit(
    conn: &Connection,
    door: &str,
    client: Option<&str>,
    event: &str,
    reason: &str,
) -> Result<()> {
    let now = Store::now_ms();
    // Unattributed denials coalesce by door/reason/event/minute. Neither token
    // nor request-controlled identifiers enter the key. One write lock/request.
    conn.execute(
        "INSERT INTO door_client_request_audit(seq,at_ms,machine,door,client,event,reason,bucket)
         VALUES((SELECT COALESCE(MAX(seq),0)+1 FROM door_client_request_audit WHERE door=?1),?2,?3,?1,?4,?5,?6,?7)
         ON CONFLICT(door,event,reason,bucket) WHERE client IS NULL
         DO UPDATE SET count=count+1,last_seen_ms=excluded.at_ms",
        params![door, now, crate::machines::this_machine(), client, event, reason, now / REQUEST_BUCKET_MS],
    )?;
    conn.execute(
        "DELETE FROM door_client_request_audit WHERE door=?1 AND seq NOT IN
         (SELECT seq FROM door_client_request_audit WHERE door=?1 ORDER BY seq DESC LIMIT ?2)",
        params![door, REQUEST_AUDIT_LIMIT],
    )?;
    Ok(())
}

pub(super) fn denial(door: &str, reason: Denial) -> Result<()> {
    let mut conn = crate::fleet::registry()?;
    init(&conn)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    audit(&tx, door, None, "request.deny", reason.reason())?;
    tx.commit()?;
    Ok(())
}

/// Removing a door must not resurrect credentials if its name is reused.
/// Called inside the same registry transaction as removal/downgrade.
pub(super) fn revoke_door(conn: &Connection, door: &str, reason: &'static str) -> Result<()> {
    init(conn)?;
    let ids = {
        let mut stmt =
            conn.prepare("SELECT id FROM door_clients WHERE door=?1 AND revoked_ms IS NULL")?;
        stmt.query_map([door], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    conn.execute(
        "UPDATE door_clients SET revoked_ms=?2,updated_ms=?2 WHERE door=?1 AND revoked_ms IS NULL",
        params![door, Store::now_ms()],
    )?;
    for id in ids {
        audit(conn, door, Some(&id), "revoke", reason)?;
    }
    Ok(())
}

fn hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn token() -> String {
    // Two independently OS-random UUID v4 values: 244 random bits. UUID's
    // fixed version/variant bits are not counted as entropy.
    format!(
        "pv_local_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

struct Grant {
    id: String,
    door: String,
    resource: String,
    scopes: Vec<String>,
    ships: Option<Vec<String>>,
    expires: Option<i64>,
    paused: bool,
    revoked: bool,
    expiry_noted: bool,
    hash: Vec<u8>,
}

fn resolve(
    conn: &Connection,
    door: &str,
    token: &str,
    required: &[&str],
) -> Result<Result<DelegatedContext, Denial>> {
    let digest = hash(token);
    let row = conn.query_row(
        "SELECT id,door,resource,scopes,ships,expires_ms,paused,revoked_ms IS NOT NULL,expiry_noted,token_hash
         FROM door_clients WHERE token_hash=?1", [digest.as_slice()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
                r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?, r.get::<_, Option<i64>>(5)?,
                r.get::<_, bool>(6)?, r.get::<_, bool>(7)?, r.get::<_, bool>(8)?, r.get::<_, Vec<u8>>(9)?))
        }).optional()?;
    let Some((
        id,
        bound_door,
        resource,
        scopes,
        ships,
        expires,
        paused,
        revoked,
        expiry_noted,
        stored_hash,
    )) = row
    else {
        audit(
            conn,
            door,
            None,
            "auth.failure",
            Denial::InvalidCredential.reason(),
        )?;
        return Ok(Err(Denial::InvalidCredential));
    };
    let grant = Grant {
        id,
        door: bound_door,
        resource,
        scopes: serde_json::from_str(&scopes)?,
        ships: ships.map(|s| serde_json::from_str(&s)).transpose()?,
        expires,
        paused,
        revoked,
        expiry_noted,
        hash: stored_hash,
    };
    // ct_eq follows an indexed equality lookup, so it adds no timing
    // protection. The lookup leak is acceptable for 244-bit random tokens.
    let denied = if !bool::from(grant.hash.as_slice().ct_eq(digest.as_slice())) {
        Some(Denial::InvalidCredential)
    } else if grant.door != door || grant.resource != format!("door:{door}") {
        Some(Denial::WrongDoor)
    } else if grant.revoked {
        Some(Denial::Revoked)
    } else if grant.paused {
        Some(Denial::Paused)
    } else if grant
        .expires
        .is_some_and(|expiry| expiry <= Store::now_ms())
    {
        if !grant.expiry_noted {
            conn.execute(
                "UPDATE door_clients SET expiry_noted=1 WHERE id=?1",
                [&grant.id],
            )?;
            audit(
                conn,
                &grant.door,
                Some(&grant.id),
                "expiry",
                "expired_credential",
            )?;
        }
        Some(Denial::Expired)
    } else if required
        .iter()
        .any(|s| !grant.scopes.iter().any(|g| g == s))
    {
        Some(Denial::Scope)
    } else {
        None
    };
    if let Some(reason) = denied {
        audit(conn, door, Some(&grant.id), "auth.failure", reason.reason())?;
        return Ok(Err(reason));
    }
    conn.execute(
        "UPDATE door_clients SET last_use_ms=?2 WHERE id=?1",
        params![grant.id, Store::now_ms()],
    )?;
    let context =
        DelegatedContext::resolved(grant.id.clone(), grant.resource, grant.scopes, grant.ships);
    audit(conn, door, Some(&grant.id), "auth.success", "admitted")?;
    conn.execute("UPDATE door_client_request_audit SET result_id=?2 WHERE door=?1 AND seq=(SELECT MAX(seq) FROM door_client_request_audit WHERE door=?1)",
        params![door, context.provenance()["result_id"].as_str()])?;
    Ok(Ok(context))
}

pub(super) fn check(
    door: &str,
    token: &str,
    required: &[&str],
) -> Result<DelegatedContext, Denial> {
    // Serialize with rotate/revoke/pause. No credential or grant cache survives
    // a request. This transaction only admits credentials, NOT execution.
    let check = || -> Result<Result<DelegatedContext, Denial>> {
        let mut conn = crate::fleet::registry()?;
        init(&conn)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let result = resolve(&tx, door, token, required)?;
        tx.commit()?;
        Ok(result)
    };
    check().unwrap_or(Err(Denial::AuthUnavailable))
}

/// Finalize the admission row rather than adding a second row per request.
/// An admitted row left by a crash means outcome unknown, not a safe retry.
pub(super) fn outcome(
    door: &str,
    context: &DelegatedContext,
    denial: Option<Denial>,
) -> Result<()> {
    let conn = crate::fleet::registry()?;
    conn.execute("UPDATE door_client_request_audit SET event=?3,reason=?4,last_seen_ms=?5 WHERE door=?1 AND result_id=?2",
        params![door, context.provenance()["result_id"].as_str(),
            if denial.is_some() { "request.deny" } else { "request.success" },
            denial.map_or("executed", Denial::reason), Store::now_ms()])?;
    Ok(())
}

pub(super) fn has_live(conn: &Connection, door: &str) -> Result<bool> {
    init(conn)?;
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM door_clients WHERE door=?1 AND revoked_ms IS NULL)",
        [door],
        |r| r.get(0),
    )?)
}

fn expiry(value: &str) -> Result<Option<i64>> {
    if value == "none" {
        return Ok(None);
    }
    let n: i64 = value
        .parse()
        .context("expiry must be Unix milliseconds or none")?;
    ensure!(n > 0, "expiry must be positive Unix milliseconds");
    Ok(Some(n))
}

fn widens_expiry(old: Option<i64>, new: Option<i64>) -> bool {
    new.is_none() || matches!((old, new), (Some(old), Some(new)) if new > old)
}

fn csv(value: Option<&String>) -> Result<Vec<String>> {
    let mut values = vec![];
    if let Some(value) = value {
        ensure!(value.len() <= 4096, "grant list too long");
        for item in value.split(',') {
            ensure!(!item.is_empty() && values.len() < 128, "invalid grant list");
            if !values.iter().any(|v| v == item) {
                values.push(item.to_owned());
            }
        }
    }
    Ok(values)
}

fn show(conn: &Connection, id: Option<&str>, door: Option<&str>) -> Result<Value> {
    let mut stmt = conn.prepare(
        "SELECT id,name,door,resource,owner,registration,scopes,ships,expires_ms,paused,revoked_ms,created_ms,updated_ms,last_use_ms
         FROM door_clients WHERE (?1 IS NULL OR id=?1) AND (?2 IS NULL OR door=?2) ORDER BY created_ms,id")?;
    let rows = stmt.query_map(params![id, door], |r| {
        Ok(json!({"id":r.get::<_,String>(0)?, "name":r.get::<_,String>(1)?, "door":r.get::<_,String>(2)?,
            "resource":r.get::<_,String>(3)?, "owner":r.get::<_,String>(4)?, "registration":r.get::<_,String>(5)?,
            "scopes":r.get::<_,String>(6)?, "ships":r.get::<_,Option<String>>(7)?, "expires_ms":r.get::<_,Option<i64>>(8)?,
            "paused":r.get::<_,bool>(9)?, "revoked_ms":r.get::<_,Option<i64>>(10)?, "created_ms":r.get::<_,i64>(11)?,
            "updated_ms":r.get::<_,i64>(12)?, "last_use_ms":r.get::<_,Option<i64>>(13)?}))
    })?;
    let mut clients = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    for client in &mut clients {
        client["scopes"] =
            serde_json::from_str(client["scopes"].as_str().context("invalid stored scopes")?)?;
        client["ships"] = match client["ships"].as_str() {
            Some(s) => serde_json::from_str(s)?,
            None => Value::Null,
        };
    }
    if id.is_some() {
        ensure!(clients.len() == 1, "unknown client");
    }
    Ok(json!({"clients":clients}))
}

fn terminal(summary: &Value, action: &str) -> Result<fs::File> {
    let mut tty = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("local credentials require the captain at a controlling terminal")?;
    writeln!(
        tty,
        "Local client consent (not OAuth enrollment):\n{}\nType {action} to confirm:",
        serde_json::to_string_pretty(summary)?
    )?;
    tty.flush()?;
    let mut line = String::new();
    BufReader::new(tty.try_clone()?.take(128)).read_line(&mut line)?;
    ensure!(line.trim() == action, "client consent not confirmed");
    Ok(tty)
}

pub(super) fn cli(args: &[String]) -> Result<()> {
    // Defense in depth only: a same-user shell can unset markers and open a
    // PTY. The harness permission policy must forbid agent lifecycle access.
    for marker in ["PRIVATEER_ROLE", "PRIVATEER_JOB"] {
        ensure!(
            std::env::var_os(marker).is_none(),
            "client lifecycle is captain-only, outside agent/job sessions"
        );
    }
    let flags = crate::cli::flags(args, &["name", "scopes", "ships", "expires-ms"])?;
    ensure!(flags.switches.is_empty(), USAGE);
    let verb = flags
        .positional
        .first()
        .map(String::as_str)
        .context(USAGE)?;
    ensure!(flags.positional.len() <= 2, USAGE);
    let target = flags.positional.get(1).map(String::as_str);
    let mut conn = crate::fleet::registry()?;
    init(&conn)?;
    match verb {
        "issue" => {
            let door = target.context(USAGE)?;
            let name = flags.values.get("name").context(USAGE)?;
            identifier(name)?;
            let scopes = csv(flags.values.get("scopes"))?;
            ensure!(
                scopes.iter().all(|s| SCOPES.contains(&s.as_str())),
                "unknown client scope"
            );
            let ships = flags
                .values
                .get("ships")
                .map(|s| csv(Some(s)))
                .transpose()?;
            if let Some(ships) = &ships {
                for ship in ships {
                    identifier(ship)?;
                }
            }
            let expires = flags
                .values
                .get("expires-ms")
                .map(|s| expiry(s))
                .transpose()?
                .flatten();
            let registration = super::registration(door)?.context("unknown door")?;
            ensure!(
                registration.protocol == 2,
                "client grants require a scoped v2 door"
            );
            let id = uuid::Uuid::new_v4().to_string();
            let summary = json!({"id":id,"name":name,"door":door,"resource":format!("door:{door}"),"scopes":scopes,"ships":ships,"expires_ms":expires});
            let mut tty = terminal(&summary, "issue")?;
            let secret = token();
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            // Revalidate registration after consent, under the writer lock.
            let protocol: Option<u32> = tx
                .query_row("SELECT protocol FROM doors WHERE name=?1", [door], |r| {
                    r.get(0)
                })
                .optional()?;
            ensure!(
                protocol == Some(2),
                "client grants require a scoped v2 door"
            );
            tx.execute("INSERT INTO door_clients(id,name,door,resource,owner,registration,scopes,ships,expires_ms,created_ms,updated_ms,token_hash)
                VALUES(?1,?2,?3,?4,'local-captain','local-terminal',?5,?6,?7,?8,?8,?9)",
                params![id,name,door,format!("door:{door}"),serde_json::to_string(&scopes)?,ships.map(|s| serde_json::to_string(&s)).transpose()?,expires,Store::now_ms(),hash(&secret).as_slice()])?;
            audit(&tx, door, Some(&id), "issue", "owner_consent")?;
            tx.commit()?;
            writeln!(tty, "Token (shown once): {secret}")?;
            println!("{}", show(&conn, Some(&id), None)?);
        }
        "list" | "inspect" => {
            ensure!(flags.values.is_empty(), USAGE);
            let result = if verb == "inspect" {
                show(&conn, Some(target.context(USAGE)?), None)?
            } else {
                show(&conn, None, target)?
            };
            println!("{result}");
        }
        "pause" | "resume" | "revoke" | "rotate" | "expiry" => {
            let id = target.context(USAGE)?;
            let expected = if verb == "expiry" { 1 } else { 0 };
            ensure!(flags.values.len() == expected, USAGE);
            let expires = if verb == "expiry" {
                expiry(flags.values.get("expires-ms").context(USAGE)?)?
            } else {
                None
            };
            let summary = show(&conn, Some(id), None)?;
            ensure!(
                summary["clients"][0]["revoked_ms"].is_null() || verb == "revoke",
                "client is revoked"
            );
            let old_expiry = summary["clients"][0]["expires_ms"].as_i64();
            let needs_consent = matches!(verb, "rotate" | "resume")
                || (verb == "expiry" && widens_expiry(old_expiry, expires));
            let mut consent_summary = summary.clone();
            if verb == "expiry" {
                consent_summary["proposed_expires_ms"] = json!(expires);
            }
            let mut tty = if needs_consent {
                Some(terminal(&consent_summary, verb)?)
            } else {
                None
            };
            let secret = if verb == "rotate" {
                Some(token())
            } else {
                None
            };
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let (door, revoked, current_expiry): (String, bool, Option<i64>) = tx
                .query_row(
                    "SELECT door,revoked_ms IS NOT NULL,expires_ms FROM door_clients WHERE id=?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?
                .context("unknown client")?;
            ensure!(!revoked || verb == "revoke", "client is revoked");
            // A concurrent shortening must not turn a noninteractive expiry
            // change into an unconsented extension. Fail closed; retry consent.
            ensure!(
                verb != "expiry" || !widens_expiry(current_expiry, expires) || tty.is_some(),
                "expiry changed concurrently; retry with terminal consent"
            );
            match verb {
                "pause" | "resume" => {
                    tx.execute(
                        "UPDATE door_clients SET paused=?2 WHERE id=?1",
                        params![id, verb == "pause"],
                    )?;
                }
                "revoke" => {
                    tx.execute(
                        "UPDATE door_clients SET revoked_ms=COALESCE(revoked_ms,?2) WHERE id=?1",
                        params![id, Store::now_ms()],
                    )?;
                }
                "expiry" => {
                    tx.execute(
                        "UPDATE door_clients SET expires_ms=?2,expiry_noted=0 WHERE id=?1",
                        params![id, expires],
                    )?;
                }
                "rotate" => {
                    tx.execute(
                        "UPDATE door_clients SET token_hash=?2 WHERE id=?1",
                        params![
                            id,
                            hash(secret.as_deref().context("missing token")?).as_slice()
                        ],
                    )?;
                }
                _ => unreachable!(),
            }
            tx.execute(
                "UPDATE door_clients SET updated_ms=?2 WHERE id=?1",
                params![id, Store::now_ms()],
            )?;
            audit(&tx, &door, Some(id), verb, "owner_action")?;
            tx.commit()?;
            if let (Some(tty), Some(secret)) = (&mut tty, secret) {
                writeln!(tty, "Token (shown once): {secret}")?;
            }
            println!("{}", show(&conn, Some(id), None)?);
        }
        "audit" => {
            ensure!(target.is_none() && flags.values.is_empty(), USAGE);
            let mut stmt = conn.prepare("SELECT seq,at_ms,machine,door,client,event,reason,1 AS count,'lifecycle' AS retention,at_ms AS last_seen_ms,NULL AS result_id FROM door_client_audit
                UNION ALL SELECT seq,at_ms,machine,door,client,event,reason,count,'request',COALESCE(last_seen_ms,at_ms),result_id FROM door_client_request_audit ORDER BY at_ms,retention,door,seq")?;
            let rows = stmt.query_map([], |r| Ok(json!({"seq":r.get::<_,i64>(0)?,"at_ms":r.get::<_,i64>(1)?,"machine":r.get::<_,String>(2)?,"door":r.get::<_,String>(3)?,"client":r.get::<_,Option<String>>(4)?,"event":r.get::<_,String>(5)?,"reason":r.get::<_,String>(6)?,"count":r.get::<_,i64>(7)?,"retention":r.get::<_,String>(8)?,"last_seen_ms":r.get::<_,i64>(9)?,"result_id":r.get::<_,Option<String>>(10)?,"grant":r.get::<_,Option<String>>(4)?})))?;
            println!(
                "{}",
                json!({"events":rows.collect::<rusqlite::Result<Vec<_>>>()?})
            );
        }
        _ => bail!(USAGE),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_widening_includes_clear_and_extension_but_not_shortening() {
        assert!(widens_expiry(Some(10), None));
        assert!(widens_expiry(None, None));
        assert!(widens_expiry(Some(10), Some(11)));
        assert!(!widens_expiry(Some(10), Some(10)));
        assert!(!widens_expiry(Some(10), Some(9)));
        assert!(!widens_expiry(None, Some(10)));
    }

    #[test]
    fn request_retention_is_per_door_and_cannot_evict_lifecycle() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        audit(&conn, "quiet", Some("client"), "issue", "owner_consent").unwrap();
        audit(&conn, "quiet", Some("client"), "revoke", "owner_action").unwrap();
        for _ in 0..5000 {
            audit(&conn, "quiet", None, "request.deny", "unknown_command").unwrap();
        }
        let occurrences: i64 = conn
            .query_row(
                "SELECT SUM(count) FROM door_client_request_audit WHERE door='quiet'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(occurrences, 5000);
        for _ in 0..5000 {
            assert!(matches!(
                resolve(&conn, "busy", "bogus", &[]).unwrap(),
                Err(Denial::InvalidCredential)
            ));
        }
        // Authenticated request rows do not coalesce, so exercise the cap too.
        for _ in 0..(REQUEST_AUDIT_LIMIT + 20) {
            audit(
                &conn,
                "busy",
                Some("client"),
                "auth.failure",
                "revoked_grant",
            )
            .unwrap();
        }
        let count = |table: &str, door: &str| -> i64 {
            conn.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE door=?1"),
                [door],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            count("door_client_request_audit", "busy"),
            REQUEST_AUDIT_LIMIT
        );
        assert!(count("door_client_request_audit", "quiet") <= 2);
        assert_eq!(count("door_client_audit", "quiet"), 2);
    }

    #[test]
    fn sparse_sequences_preserve_row_count_and_coalescing_records_last_seen() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO door_client_audit VALUES
            (1,1,'test','door','client','issue','owner_consent'),
            (99999,2,'test','door','client','pause','owner_action');",
        )
        .unwrap();
        audit(&conn, "door", Some("client"), "resume", "owner_action").unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM door_client_audit", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 3);
        audit(&conn, "door", None, "request.deny", "unknown_command").unwrap();
        audit(&conn, "door", None, "request.deny", "unknown_command").unwrap();
        let seen: i64 = conn.query_row("SELECT COUNT(*) FROM door_client_request_audit WHERE count>1 AND last_seen_ms>=at_ms", [], |r| r.get(0)).unwrap();
        assert_eq!(seen, 1);
    }

    #[test]
    fn mixed_ring_migration_preserves_surviving_facts_and_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE door_client_audit(
            seq INTEGER PRIMARY KEY, at_ms INTEGER NOT NULL, machine TEXT NOT NULL,
            door TEXT NOT NULL, client TEXT, event TEXT NOT NULL, reason TEXT NOT NULL);
            INSERT INTO door_client_audit VALUES
                (1,1,'test','door','client','issue','owner_consent'),
                (2,2,'test','door',NULL,'auth.failure','invalid_credential'),
                (3,3,'test','door','client','revoke','owner_action');",
        )
        .unwrap();
        init(&conn).unwrap();
        init(&conn).unwrap();
        let lifecycle: i64 = conn
            .query_row("SELECT COUNT(*) FROM door_client_audit", [], |r| r.get(0))
            .unwrap();
        let requests: i64 = conn
            .query_row(
                "SELECT SUM(count) FROM door_client_request_audit",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(lifecycle, 2);
        assert_eq!(requests, 1);
    }
}
