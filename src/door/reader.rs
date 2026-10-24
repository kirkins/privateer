//! Local scoped pull mail. The registry serializes each reader's served/ack
//! transitions; immutable home mail supplies identity and keyset order. No
//! captain/dashboard, v1 door or other grant cursor is read or written here.
use super::scoped::{Command, DelegatedContext};
use crate::store::Store;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::HashSet;

const WINDOW: i64 = 256;
const PAGE_TOKENS: i64 = 32;
const TOKEN_MS: i64 = 24 * 60 * 60 * 1000;
const PAGE_BYTES: usize = 512 * 1024;

fn registry() -> Result<Connection> {
    let conn = crate::fleet::registry()?;
    schema(&conn)?;
    Ok(conn)
}

fn schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS door_reader_stream(
           client TEXT NOT NULL, resource TEXT NOT NULL, mailbox TEXT NOT NULL,
           generation TEXT NOT NULL, through_seq INTEGER NOT NULL DEFAULT 0,
           PRIMARY KEY(client,resource,mailbox));
         CREATE TABLE IF NOT EXISTS door_reader_served(
           client TEXT NOT NULL, resource TEXT NOT NULL, mailbox TEXT NOT NULL,
           id TEXT NOT NULL, seq INTEGER NOT NULL, ref TEXT NOT NULL UNIQUE,
           acked INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(client,resource,mailbox,id));
         CREATE TABLE IF NOT EXISTS door_reader_pages(
           token TEXT PRIMARY KEY, client TEXT NOT NULL, resource TEXT NOT NULL,
           mailbox TEXT NOT NULL, generation TEXT NOT NULL, mode TEXT NOT NULL,
           anchor TEXT NOT NULL, after_seq INTEGER NOT NULL, upper_seq INTEGER NOT NULL,
           created_ms INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS door_reader_page_owner ON door_reader_pages(client,resource);
         CREATE TABLE IF NOT EXISTS door_reader_digests(
           client TEXT NOT NULL, resource TEXT NOT NULL, id TEXT NOT NULL,
           ref TEXT NOT NULL UNIQUE, PRIMARY KEY(client,resource,id));",
    )?;
    Ok(())
}

fn mailbox(store: &Store) -> Result<String> {
    // Database incarnation, not a path or mutable ship name. Copying an entire
    // home is an operator migration; moving desks is NOT implemented here.
    let conn = store.connection();
    let existing = || -> rusqlite::Result<Option<String>> {
        conn.query_row(
            "SELECT value FROM metadata WHERE key='door_reader_mailbox'",
            [],
            |r| r.get(0),
        )
        .optional()
    };
    if let Some(id) = existing()? {
        return Ok(id);
    }
    conn.execute(
        "INSERT OR IGNORE INTO metadata(key,value) VALUES('door_reader_mailbox',?1)",
        [uuid::Uuid::new_v4().to_string()],
    )?;
    existing()?.context("mailbox identity missing")
}

fn home(args: &Value, context: &DelegatedContext) -> Result<Store> {
    let ship = args["ship"].as_str().context("missing mailbox")?;
    // Hidden and nonexistent homes take the same non-disclosing refusal.
    let (_, path) = super::members()?
        .into_iter()
        .find(|(name, _)| name == ship && context.allows(name))
        .context("mailbox not found")?;
    Store::open(&path)
}

fn visible(mail: &Value, context: &DelegatedContext) -> bool {
    let from = mail["from"].as_str().unwrap_or_default();
    let to = mail["to"].as_str().unwrap_or_default();
    (to == crate::mail::CAPTAIN && context.allows(from))
        || (from == crate::mail::CAPTAIN && context.allows(to))
}

fn exact(store: &Store, id: &str, context: &DelegatedContext) -> Result<Value> {
    crate::mail::find(store, id)?
        .filter(|m| visible(m, context))
        .context("mail not found")
}

fn stream(conn: &Connection, mailbox: &str, c: &DelegatedContext) -> Result<(String, i64)> {
    conn.execute("INSERT OR IGNORE INTO door_reader_stream(client,resource,mailbox,generation) VALUES(?1,?2,?3,?4)",
        params![c.client(),c.resource(),mailbox,uuid::Uuid::new_v4().to_string()])?;
    Ok(conn.query_row("SELECT generation,through_seq FROM door_reader_stream WHERE client=?1 AND resource=?2 AND mailbox=?3",
        params![c.client(),c.resource(),mailbox], |r| Ok((r.get(0)?,r.get(1)?)))?)
}

fn served(
    conn: &Connection,
    mailbox: &str,
    cursor: i64,
    mail: &Value,
    c: &DelegatedContext,
    bound: i64,
    copies: &Copies,
) -> Result<Option<String>> {
    let seq = mail["seq"].as_i64().context("missing sequence")?;
    // History and outgoing conversation are readable but not new inbox mail.
    if mail["to"] != crate::mail::CAPTAIN || seq <= cursor {
        return Ok(None);
    }
    let id = mail["id"].as_str().context("missing id")?;
    if let Some((reference, acked)) = conn.query_row(
        "SELECT ref,acked FROM door_reader_served WHERE client=?1 AND resource=?2 AND mailbox=?3 AND id=?4",
        params![c.client(),c.resource(),mailbox,id], |r| Ok((r.get::<_,String>(0)?,r.get::<_,bool>(1)?))).optional()? {
        return Ok((!acked).then_some(reference));
    }
    if copy_known(conn, copies, mailbox, id, c, false)? {
        return Ok(None);
    }
    // Count ALL outstanding sparse state, not merely unacknowledged entries.
    // A client cannot ack arbitrarily far ahead to allocate unbounded holes.
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM door_reader_served WHERE client=?1 AND resource=?2",
        params![c.client(), c.resource()],
        |r| r.get(0),
    )?;
    ensure!(
        count < bound,
        "reader window full; acknowledge earlier inbox mail"
    );
    let reference = uuid::Uuid::new_v4().to_string();
    conn.execute("INSERT INTO door_reader_served(client,resource,mailbox,id,seq,ref) VALUES(?1,?2,?3,?4,?5,?6)",
        params![c.client(),c.resource(),mailbox,id,seq,reference])?;
    Ok(Some(reference))
}

/// Previously acknowledged copies are proved by their existing home mail and
/// compact cursor, not an unbounded delivered-ID ledger. Only allowed active
/// local homes participate; failure to read needed history fails the call.
type Copies = [(Store, i64)];

fn copies(conn: &Connection, current: &Store, c: &DelegatedContext) -> Result<Vec<(Store, i64)>> {
    let mut copies = vec![];
    for (ship, path) in super::members()? {
        if path == current.home() || !c.allows(&ship) {
            continue;
        }
        let source = Store::open_readonly(&path, crate::store::Caller::Operator)?;
        let identity: Option<String> = source
            .connection()
            .query_row(
                "SELECT value FROM metadata WHERE key='door_reader_mailbox'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let Some(identity) = identity else {
            continue;
        };
        let through: Option<i64> = conn.query_row("SELECT through_seq FROM door_reader_stream WHERE client=?1 AND resource=?2 AND mailbox=?3",
            params![c.client(),c.resource(),identity], |r| r.get(0)).optional()?;
        if let Some(through) = through.filter(|seq| *seq > 0) {
            copies.push((source, through));
        }
    }
    Ok(copies)
}

fn copy_known(
    conn: &Connection,
    copies: &Copies,
    mailbox: &str,
    id: &str,
    c: &DelegatedContext,
    acked_only: bool,
) -> Result<bool> {
    let served: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM door_reader_served WHERE client=?1 AND resource=?2 AND mailbox!=?3 AND id=?4 AND (?5=0 OR acked=1))",
        params![c.client(),c.resource(),mailbox,id,acked_only], |r| r.get(0))?;
    if served {
        return Ok(true);
    }
    let ships = c.ships().map(|s| json!(s).to_string());
    for (source, through) in copies {
        let known: bool = source.connection().query_row("SELECT EXISTS(SELECT 1 FROM mail WHERE id=?1 AND seq<=?2 AND recipient='captain' AND (?3 IS NULL OR sender IN (SELECT value FROM json_each(?3))))",
            params![id,through,ships], |r| r.get(0))?;
        if known {
            return Ok(true);
        }
    }
    Ok(false)
}

fn coverage() -> Value {
    json!({"scope":"local-mailbox","complete":true,
        "remote":{"complete":false,"state":"unavailable","retry":"remote_subscriptions_not_implemented"}})
}

pub(super) fn request(command: Command, args: &Value, c: &DelegatedContext) -> Result<Value> {
    if command == Command::Ack && args.get("ship").is_none() {
        return ack_digests(args, c);
    }
    let store = home(args, c)?;
    let mailbox = mailbox(&store)?;
    let mut conn = registry()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let (generation, cursor) = stream(&tx, &mailbox, c)?;
    let copies = copies(&tx, &store, c)?;
    // Prior acknowledgments in another home may now prove a duplicate gap.
    // Compact those facts before admission; this never acknowledges new mail.
    compact(&tx, &store, &mailbox, cursor, c, &copies)?;
    let (_, cursor) = stream(&tx, &mailbox, c)?;
    let result = match command {
        Command::Ack => ack(&tx, &store, &mailbox, cursor, args, c, &copies)?,
        Command::Show => {
            let mail = exact(&store, args["id"].as_str().context("missing id")?, c)?;
            let reference = served(&tx, &mailbox, cursor, &mail, c, WINDOW / 2, &copies)?;
            json!({"mail":mail,"ref":reference})
        }
        Command::Inbox | Command::Thread => page(
            &tx,
            &store,
            &mailbox,
            &generation,
            cursor,
            command,
            args,
            c,
            &copies,
        )?,
        _ => unreachable!("reader command table"),
    };
    tx.commit()?;
    let mut result = result;
    result["coverage"] = coverage();
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn page(
    conn: &Connection,
    store: &Store,
    mailbox: &str,
    generation: &str,
    cursor: i64,
    command: Command,
    args: &Value,
    c: &DelegatedContext,
    copies: &Copies,
) -> Result<Value> {
    let mode = if command == Command::Inbox {
        "inbox"
    } else {
        "thread"
    };
    let anchor = if command == Command::Thread {
        let mail = exact(store, args["id"].as_str().context("missing id")?, c)?;
        mail["correlation"]
            .as_str()
            .or(mail["id"].as_str())
            .context("missing thread")?
            .to_owned()
    } else {
        String::new()
    };
    let now = Store::now_ms();
    let (after, upper) = if let Some(token) = args["continuation"].as_str() {
        conn.query_row("SELECT after_seq,upper_seq FROM door_reader_pages WHERE token=?1 AND client=?2 AND resource=?3 AND mailbox=?4 AND generation=?5 AND mode=?6 AND anchor=?7 AND created_ms>?8",
            params![token,c.client(),c.resource(),mailbox,generation,mode,anchor,now-TOKEN_MS],
            |r| Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?))).optional()?.context("invalid continuation")?
    } else {
        let upper: i64 =
            store
                .connection()
                .query_row("SELECT COALESCE(MAX(seq),0) FROM mail", [], |r| r.get(0))?;
        (if command == Command::Inbox { cursor } else { 0 }, upper)
    };
    let limit = args["limit"].as_u64().unwrap_or(25) as usize;
    let mut stmt = store.connection().prepare(&format!(
        "SELECT {} FROM mail WHERE seq>?1 AND seq<=?2
         AND ((?3='inbox' AND recipient='captain') OR (?3='thread' AND (id=?4 OR correlation=?4)))
         AND (recipient='captain' OR sender='captain')
         AND (?5 IS NULL OR (CASE WHEN sender='captain' THEN recipient ELSE sender END) IN (SELECT value FROM json_each(?5)))
         ORDER BY seq LIMIT ?6", crate::mail::COLUMNS))?;
    let ships = c.ships().map(|s| json!(s).to_string());
    // At most WINDOW acked sparse records can be skipped before a full page.
    let rows = stmt.query_map(
        params![
            after.max(if command == Command::Inbox { cursor } else { 0 }),
            upper,
            mode,
            anchor,
            ships,
            WINDOW + limit as i64 + 1
        ],
        crate::mail::message,
    )?;
    let mut items = vec![];
    let mut last = after;
    let mut more = false;
    let mut bytes = 0;
    let mut scanned = 0;
    for mail in rows {
        scanned += 1;
        let mail = mail?;
        let seq = mail["seq"].as_i64().context("missing sequence")?;
        let acked: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM door_reader_served WHERE client=?1 AND resource=?2 AND mailbox=?3 AND id=?4 AND acked=1)",
            params![c.client(),c.resource(),mailbox,mail["id"].as_str()], |r| r.get(0))?;
        let own: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM door_reader_served WHERE client=?1 AND resource=?2 AND mailbox=?3 AND id=?4)",
            params![c.client(),c.resource(),mailbox,mail["id"].as_str()], |r| r.get(0))?;
        let duplicate = !own
            && mail["to"] == crate::mail::CAPTAIN
            && copy_known(
                conn,
                copies,
                mailbox,
                mail["id"].as_str().context("missing id")?,
                c,
                false,
            )?;
        if command == Command::Inbox && (acked || duplicate) {
            last = seq;
            continue;
        }
        let size = mail.to_string().len();
        if items.len() == limit || bytes + size > PAGE_BYTES {
            more = true;
            break;
        }
        // Reserve half the window for inbox progress, so sparse show/thread
        // acknowledgments cannot fill every slot behind an unseen gap.
        let bound = if command == Command::Inbox {
            WINDOW
        } else {
            WINDOW / 2
        };
        let reference = served(conn, mailbox, cursor, &mail, c, bound, copies)?;
        bytes += size;
        last = seq;
        items.push(json!({"mail":mail,"ref":reference}));
    }
    // Copy suppression can skip more than WINDOW records. Keep scan work per
    // page bounded and explicitly continue, even for an all-duplicate page.
    more |= scanned == WINDOW + limit as i64 + 1 && last < upper;
    let continuation = if more {
        conn.execute("DELETE FROM door_reader_pages WHERE client=?1 AND resource=?2 AND (created_ms<=?3 OR token NOT IN (SELECT token FROM door_reader_pages WHERE client=?1 AND resource=?2 ORDER BY created_ms DESC,rowid DESC LIMIT ?4))",
            params![c.client(),c.resource(),now-TOKEN_MS,PAGE_TOKENS-1])?;
        let token = uuid::Uuid::new_v4().to_string();
        conn.execute("INSERT INTO door_reader_pages(token,client,resource,mailbox,generation,mode,anchor,after_seq,upper_seq,created_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![token,c.client(),c.resource(),mailbox,generation,mode,anchor,last,upper,now])?;
        Some(token)
    } else {
        None
    };
    Ok(json!({"items":items,"continuation":continuation}))
}

fn ack(
    conn: &Connection,
    store: &Store,
    mailbox: &str,
    cursor: i64,
    args: &Value,
    c: &DelegatedContext,
    copies: &Copies,
) -> Result<Value> {
    let refs = args["refs"].as_array().context("missing references")?;
    let mut unique = HashSet::new();
    // Validate the entire batch before changing any row. Never accept a client
    // supplied seq/cursor; opaque refs identify exactly one durable served row.
    for reference in refs {
        let reference = reference.as_str().context("invalid reference")?;
        ensure!(unique.insert(reference), "duplicate reference");
        let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM door_reader_served WHERE client=?1 AND resource=?2 AND mailbox=?3 AND ref=?4 AND acked=0 AND seq>?5)",
            params![c.client(),c.resource(),mailbox,reference,cursor], |r| r.get(0))?;
        ensure!(exists, "unserved or stale reference");
    }
    for reference in refs {
        conn.execute(
            "UPDATE door_reader_served SET acked=1 WHERE ref=?1",
            [reference.as_str().unwrap()],
        )?;
    }
    compact(conn, store, mailbox, cursor, c, copies)?;
    Ok(json!({"acknowledged":refs.len()}))
}

fn compact(
    conn: &Connection,
    store: &Store,
    mailbox: &str,
    cursor: i64,
    c: &DelegatedContext,
    copies: &Copies,
) -> Result<()> {
    // Compact only a contiguous prefix of authorized incoming mail. show or
    // thread may serve far-future mail: acknowledging it cannot skip unseen IDs.
    let high: i64 = conn.query_row("SELECT COALESCE(MAX(seq),?4) FROM door_reader_served WHERE client=?1 AND resource=?2 AND mailbox=?3 AND acked=1",
        params![c.client(),c.resource(),mailbox,cursor], |r| r.get(0))?;
    let acked: HashSet<String> = {
        let mut stmt = conn.prepare("SELECT id FROM door_reader_served WHERE client=?1 AND resource=?2 AND mailbox=?3 AND acked=1")?;
        stmt.query_map(params![c.client(), c.resource(), mailbox], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    // Copy IDs use other home cursors as proof. Stream this range without
    // materializing its history; only the bounded sparse set is in memory.
    let mut stmt = store.connection().prepare("SELECT seq,id FROM mail WHERE recipient='captain' AND seq>?1 AND seq<=?2 AND (?3 IS NULL OR sender IN (SELECT value FROM json_each(?3))) ORDER BY seq")?;
    let ships = c.ships().map(|s| json!(s).to_string());
    let mut through = high;
    for row in stmt.query_map(params![cursor, high, ships], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })? {
        let (seq, id) = row?;
        if !acked.contains(&id) && !copy_known(conn, copies, mailbox, &id, c, true)? {
            through = seq - 1;
            break;
        }
    }
    conn.execute("UPDATE door_reader_stream SET through_seq=MAX(through_seq,?4) WHERE client=?1 AND resource=?2 AND mailbox=?3", params![c.client(),c.resource(),mailbox,through])?;
    conn.execute(
        "DELETE FROM door_reader_served WHERE client=?1 AND resource=?2 AND mailbox=?3 AND seq<=?4",
        params![c.client(), c.resource(), mailbox, through],
    )?;
    Ok(())
}

/// Digests retain their separate grant-owned summary cursor. A digest ack
/// never consumes raw inbox mail, nor does raw mail ack consume a digest.
/// Both use durable, grant/resource-bound served refs and one registry commit.
pub(crate) fn serve_digest(c: &DelegatedContext, digest: &Value) -> Result<String> {
    let id = digest["id"].as_str().context("missing digest id")?;
    let mut conn = registry()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute("DELETE FROM door_reader_digests WHERE client=?1 AND resource=?2 AND id NOT IN (SELECT id FROM away_digest WHERE state='pending')", params![c.client(),c.resource()])?;
    let reader = format!("client/{}", c.client());
    let pending: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM away_digest WHERE id=?1 AND door=?2 AND state='pending')",
        params![id, reader],
        |r| r.get(0),
    )?;
    ensure!(pending, "digest no longer pending");
    let reference = if let Some(reference) = tx
        .query_row(
            "SELECT ref FROM door_reader_digests WHERE client=?1 AND resource=?2 AND id=?3",
            params![c.client(), c.resource(), id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        reference
    } else {
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM door_reader_digests WHERE client=?1 AND resource=?2",
            params![c.client(), c.resource()],
            |r| r.get(0),
        )?;
        ensure!(count < 128, "digest reader window full");
        let reference = uuid::Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO door_reader_digests(client,resource,id,ref) VALUES(?1,?2,?3,?4)",
            params![c.client(), c.resource(), id, reference],
        )?;
        reference
    };
    tx.commit()?;
    Ok(reference)
}

fn ack_digests(args: &Value, c: &DelegatedContext) -> Result<Value> {
    let mut conn = registry()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let refs = args["refs"].as_array().context("missing references")?;
    let mut unique = HashSet::new();
    for reference in refs {
        let reference = reference.as_str().context("invalid reference")?;
        ensure!(unique.insert(reference), "duplicate reference");
        let id: String = tx
            .query_row(
                "SELECT id FROM door_reader_digests WHERE client=?1 AND resource=?2 AND ref=?3",
                params![c.client(), c.resource(), reference],
                |r| r.get(0),
            )
            .optional()?
            .context("unserved digest")?;
        let changed = tx.execute("UPDATE away_digest SET state='delivered',delivered_ms=?3 WHERE id=?1 AND door=?2 AND state='pending'", params![id,format!("client/{}",c.client()),Store::now_ms()])?;
        ensure!(changed == 1, "stale digest reference");
        tx.execute("DELETE FROM door_reader_digests WHERE ref=?1", [reference])?;
    }
    tx.commit()?;
    Ok(json!({"acknowledged":refs.len(),"coverage":coverage()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> DelegatedContext {
        DelegatedContext::resolved(
            "test-reader".into(),
            "door:test".into(),
            vec!["mail:read".into()],
            None,
        )
    }

    #[test]
    fn racing_acknowledgments_have_one_durable_winner() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let store = Store::open(&home).unwrap();
        crate::mail::init(&store).unwrap();
        crate::mail::record_runtime(&store, &json!({"id":"one","from":"ship","to":"captain","kind":"notice","body":"one","payload":{},"urgency":"normal"})).unwrap();
        let path = temp.path().join("reader.sqlite");
        let mut conn = Connection::open(&path).unwrap();
        schema(&conn).unwrap();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        stream(&tx, "mailbox", &context()).unwrap();
        let mail = crate::mail::find(&store, "one").unwrap().unwrap();
        let reference = served(&tx, "mailbox", 0, &mail, &context(), WINDOW, &[])
            .unwrap()
            .unwrap();
        tx.commit().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let (path, home, reference, barrier) = (
                    path.clone(),
                    home.clone(),
                    reference.clone(),
                    barrier.clone(),
                );
                std::thread::spawn(move || {
                    let store = Store::open(&home).unwrap();
                    let mut conn = Connection::open(path).unwrap();
                    conn.busy_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                    barrier.wait();
                    let tx = conn
                        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                        .unwrap();
                    let (_, cursor) = stream(&tx, "mailbox", &context()).unwrap();
                    let result = ack(
                        &tx,
                        &store,
                        "mailbox",
                        cursor,
                        &json!({"refs":[reference]}),
                        &context(),
                        &[],
                    );
                    if result.is_ok() {
                        tx.commit().unwrap();
                    }
                    result.is_ok()
                })
            })
            .collect();
        barrier.wait();
        assert_eq!(
            threads
                .into_iter()
                .map(|t| usize::from(t.join().unwrap()))
                .sum::<usize>(),
            1
        );
        let (_, cursor) = stream(&conn, "mailbox", &context()).unwrap();
        assert_eq!(cursor, mail["seq"].as_i64().unwrap());
    }
}
