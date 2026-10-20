//! Ship directory and typed mailboxes.
//!
//! A ship is an identity with its own mailbox, first mate and crew. A home
//! hosts one or more ships; the directory records where each ship lives.
//! `captain` is the reserved address of the person. Messages are durable,
//! idempotent and typed: a `request` or `decision` stays open until its single
//! `reply` or explicit closure, so the runtime tracks owed answers. A request
//! may record the stow it depends on, and a resume of that stow ends it
//! through the same durable closure path a sender's withdrawal uses.
use crate::store::{Caller, Store, identifier, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, Row, params};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::time::{Duration, Instant};

pub const CAPTAIN: &str = "captain";
/// The runtime's voice for a ship's crew: job outcomes and crew reports.
/// Only the runtime writes as it (see `crew`); nobody can send as it.
pub const CREW: &str = "crew";
const KINDS: &[&str] = &["request", "reply", "notice", "decision"];
const URGENCIES: &[&str] = &["low", "normal", "high"];
const MAX_MESSAGE_BYTES: usize = 65_536;
/// The exact hands-on step a request to the captain may name.
pub(crate) const MAX_HANDS_STEP_BYTES: usize = 1_024;
pub(crate) const MAX_SCOPE_CHARS: usize = 2_000;
/// Extra local classifications cannot turn a bounded flush into a backlog
/// scan. They do not consume its separate allowance for network attempts.
const EXTRA_CLASSIFICATIONS: i64 = 50;

pub fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS ships(name TEXT PRIMARY KEY, record TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS mail(seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL,
           sender TEXT NOT NULL, recipient TEXT NOT NULL, kind TEXT NOT NULL, correlation TEXT,
           task_id TEXT, request_kind TEXT, body TEXT NOT NULL, payload TEXT NOT NULL,
           urgency TEXT NOT NULL, deadline_ms INTEGER, created_ms INTEGER NOT NULL,
           state TEXT, answered_by TEXT, answered_ms INTEGER);
         CREATE INDEX IF NOT EXISTS mail_recipient ON mail(recipient, seq);
         CREATE INDEX IF NOT EXISTS mail_sender ON mail(sender, seq);
         CREATE INDEX IF NOT EXISTS mail_correlation ON mail(correlation);
         CREATE INDEX IF NOT EXISTS mail_time ON mail(created_ms, seq);
         CREATE TABLE IF NOT EXISTS mail_cursor(address TEXT PRIMARY KEY, through_seq INTEGER NOT NULL);",
    )?;
    let tx = store.write_transaction()?;
    for (column, declaration) in [
        ("ended_by", "TEXT"),
        ("ended_ms", "INTEGER"),
        ("end_reason", "TEXT"),
        ("delivery", "TEXT"),
        ("delivery_attempts", "INTEGER NOT NULL DEFAULT 0"),
        ("delivery_error", "TEXT"),
        ("delivered_ms", "INTEGER"),
        // A transport acknowledgement is not proof an old peer applied a closure.
        ("remote_end_state", "TEXT"),
        // The request a follow-up is raised because of; see ROUTINE_MAIL.
        ("because_of", "TEXT"),
    ] {
        let present: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('mail') WHERE name=?1)",
            [column],
            |r| r.get(0),
        )?;
        if !present {
            tx.execute_batch(&format!(
                "ALTER TABLE mail ADD COLUMN {column} {declaration};"
            ))?;
        }
    }
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS mail_pending ON mail(delivery, seq) WHERE delivery='pending';
         CREATE INDEX IF NOT EXISTS mail_because_of ON mail(because_of) WHERE because_of IS NOT NULL;
         CREATE TABLE IF NOT EXISTS mail_delivery_retry(peer TEXT PRIMARY KEY,
           failures INTEGER NOT NULL, next_attempt_ms INTEGER NOT NULL, last_attempt_ms INTEGER NOT NULL);",
    )?;
    tx.commit()?;
    Ok(())
}

/// The one definition of a message that only informs, for every place that
/// decides one: an information-only notice that is in the thread of no open
/// request this mailbox sent or received, and of no request an open follow-up
/// of this mailbox grows from. `because_of` names the thread a follow-up grows
/// from, so a low notice in that thread is actionable for the sender of every
/// still-open follow-up, one level only; the parent may be answered and may
/// live in another home, and the wake names the follow-up.
pub(crate) const ROUTINE_MAIL: &str = "kind='notice' AND urgency='low'
     AND NOT (EXISTS(SELECT 1 FROM mail AS follow_up WHERE follow_up.because_of=mail.correlation
       AND follow_up.sender=mail.recipient AND follow_up.state='open'
       AND follow_up.kind IN ('request','decision'))
       OR EXISTS(SELECT 1 FROM mail AS req WHERE req.id=mail.correlation
       AND req.state='open' AND req.kind IN ('request','decision')
       AND (req.sender=mail.recipient OR req.recipient=mail.recipient)))";

/// The open follow-up of this mailbox that a notice in its thread concerns, if
/// any: the oldest one, so a wake names a bounded id to withdraw or update.
pub(crate) const OPEN_FOLLOW_UP: &str =
    "(SELECT f.id FROM mail AS f WHERE f.because_of=mail.correlation
     AND f.sender=mail.recipient AND f.state='open' AND f.kind IN ('request','decision')
     ORDER BY f.seq LIMIT 1)";

/// Where a ship's mailbox lives: None for this home, Some(peer) for another.
fn remote_peer(ship: &Value) -> Option<&str> {
    ship["location"]["peer"].as_str()
}

pub(crate) fn ship(store: &Store, name: &str) -> Result<Option<Value>> {
    let raw: Option<String> = store
        .connection()
        .query_row("SELECT record FROM ships WHERE name=?1", [name], |r| {
            r.get(0)
        })
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw).context("corrupt ship record"))
        .transpose()
}

/// A mailbox this home holds: the captain's, or a ship hosted here.
fn local_address(store: &Store, address: &str) -> Result<()> {
    ensure!(
        address != CREW,
        "{CREW} has no mailbox; crew read answers with crew.answer"
    );
    if address == CAPTAIN {
        return Ok(());
    }
    match ship(store, address)? {
        Some(ship) if ship["location"] == "here" => Ok(()),
        Some(_) => bail!("ship {address} is not hosted by this home"),
        None => bail!("unknown ship {address}; add it with ship.put"),
    }
}

/// Refuse names no ship may take: the person's and the runtime's crew.
pub fn unreserved(name: &str) -> Result<()> {
    ensure!(name != CAPTAIN, "{CAPTAIN} is reserved for the person");
    ensure!(
        name != CREW,
        "{CREW} is reserved for the runtime's crew mail"
    );
    Ok(())
}

fn ship_put(store: &Store, args: &Value) -> Result<Value> {
    let name = required(args, "name")?;
    identifier(name)?;
    unreserved(name)?;
    let scope = args.get("scope").and_then(Value::as_str).unwrap_or("");
    ensure!(
        scope.chars().count() <= MAX_SCOPE_CHARS,
        "scope is limited to {MAX_SCOPE_CHARS} characters"
    );
    let location = args.get("location").cloned().unwrap_or(json!("here"));
    if location != "here" {
        let peer = location
            .get("peer")
            .and_then(Value::as_str)
            .filter(|_| location.as_object().is_some_and(|o| o.len() == 1))
            .context("location must be \"here\" or {\"peer\": \"<configured peer>\"}")?;
        ensure!(
            crate::peers::configured(store, peer)?,
            "peer {peer} is not configured; add it with peer.put"
        );
    }
    let accepts = args.get("accepts").cloned().unwrap_or(json!([]));
    let kinds = accepts
        .as_array()
        .context("accepts must be a list of request kinds")?;
    for kind in kinds {
        identifier(kind.as_str().context("request kinds are strings")?)?;
    }
    let mut record =
        json!({"name": name, "scope": scope, "location": location, "accepts": accepts});
    // What a ship consumes is set with ship.consumes; a ship.put that does
    // not name it, such as a directory refresh, keeps it.
    let consumes = match args.get("consumes") {
        Some(list) => Some(list.clone()),
        None => ship(store, name)?.and_then(|existing| existing.get("consumes").cloned()),
    };
    if let Some(consumes) = consumes.filter(|c| !c.is_null()) {
        let names = consumes
            .as_array()
            .context("consumes is a list of ship names")?;
        for other in names {
            identifier(other.as_str().context("consumes holds ship names")?)?;
        }
        record["consumes"] = consumes;
    }
    if let Some(profile) = args.get("first_mate").filter(|v| !v.is_null()) {
        ensure!(
            location == "here",
            "only ships hosted here have a first mate to wake"
        );
        record["first_mate"] = crate::activation::profile(store, name, profile)?;
    }
    store.connection().execute(
        "INSERT INTO ships(name, record) VALUES(?1, ?2) ON CONFLICT(name) DO UPDATE SET record=excluded.record",
        params![name, record.to_string()],
    )?;
    store.audit("ship.put", &json!({"name": name}))?;
    // A first mate's profile is read again at each wake, so a running pane
    // keeps its launch options until it sleeps and wakes or is relaunched.
    if args.get("first_mate").is_some_and(|v| !v.is_null()) {
        record["first_mate_applies"] = json!(
            "at the next wake or launch; `privateer host relaunch NAME` applies it to a running hosted pane now"
        );
    }
    Ok(record)
}

/// Write a message from `crew` to a ship hosted here, inside the caller's
/// transaction so it commits with the fact it reports. `message` holds id,
/// to, kind (notice or request), body, payload and urgency, and optionally
/// task_id and correlation. The id
/// is the idempotency key: an existing message under it is left as it is,
/// and false is returned.
pub(crate) fn record_crew(conn: &rusqlite::Connection, message: &Value) -> Result<bool> {
    let text = |key: &str| message[key].as_str().context("invalid crew message");
    let (kind, urgency) = (text("kind")?, text("urgency")?);
    ensure!(
        matches!(kind, "notice" | "request") && URGENCIES.contains(&urgency),
        "invalid crew message"
    );
    // A notice may carry a correlation only to group a thread: it is never a
    // reply, so it answers nothing.
    let written = conn.execute(
        "INSERT INTO mail(id,sender,recipient,kind,task_id,correlation,body,payload,urgency,created_ms,state)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) ON CONFLICT(id) DO NOTHING",
        params![
            text("id")?,
            CREW,
            text("to")?,
            kind,
            message["task_id"].as_str(),
            message["correlation"].as_str(),
            text("body")?,
            message["payload"].to_string(),
            urgency,
            Store::now_ms(),
            (kind == "request").then_some("open")
        ],
    )?;
    Ok(written == 1)
}

/// Record mail the runtime writes for a ship, in the caller's transaction:
/// a block ask to the captain, or a notice to another ship. `message` holds
/// id, from, to, kind, body, payload and urgency, and optionally task_id and
/// correlation. Mail for a ship behind a peer waits as pending for the next
/// flush. The id is the idempotency key; false means it already existed.
pub(crate) fn record_runtime(store: &Store, message: &Value) -> Result<bool> {
    let text = |key: &str| message[key].as_str().context("invalid runtime message");
    let (to, kind, urgency) = (text("to")?, text("kind")?, text("urgency")?);
    ensure!(
        KINDS.contains(&kind) && kind != "reply" && URGENCIES.contains(&urgency),
        "invalid runtime message"
    );
    let peer = if to == CAPTAIN {
        None
    } else {
        ship(store, to)?
            .as_ref()
            .and_then(remote_peer)
            .map(str::to_owned)
    };
    let written = store.connection().execute(
        "INSERT INTO mail(id,sender,recipient,kind,correlation,task_id,body,payload,urgency,created_ms,state,delivery)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12) ON CONFLICT(id) DO NOTHING",
        params![
            text("id")?,
            text("from")?,
            to,
            kind,
            message["correlation"].as_str(),
            message["task_id"].as_str(),
            text("body")?,
            message["payload"].to_string(),
            urgency,
            Store::now_ms(),
            matches!(kind, "request" | "decision").then_some("open"),
            peer.map(|_| "pending")
        ],
    )?;
    Ok(written == 1)
}

/// End the open mail attached to a primary ask that ended, in every fleet
/// home on this machine: each row is ended by the home that sent it.
pub(crate) fn end_attachments(store: &Store, ask: &str, reason: &str) -> Result<usize> {
    let mut ended = 0;
    let why = format!("merged ask {ask} ended: {reason}");
    let mut end_in = |store: &Store| -> Result<()> {
        let tx = store.write_transaction()?;
        let mut statement = store.connection().prepare(&format!(
            "SELECT {COLUMNS} FROM mail WHERE recipient='captain' AND state='open'
             AND kind IN ('request','decision') AND COALESCE(delivery,'') <> 'received'
             AND json_extract(payload,'$.attached_to')=?1"
        ))?;
        let rows = statement
            .query_map([ask], message)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for row in rows {
            end_request(store, &row, "withdrawn", &why)?;
            ended += 1;
        }
        tx.commit()?;
        Ok(())
    };
    end_in(store)?;
    for home in crate::block_ask::other_homes(store.home()) {
        if let Ok(other) = Store::open(&home.1) {
            end_in(&other)?;
        }
    }
    Ok(ended)
}

/// A message by id, for runtime modules that check it against the caller.
pub(crate) fn find(store: &Store, id: &str) -> Result<Option<Value>> {
    Ok(by_id(store, id)?.map(|m| annotate(m, Store::now_ms())))
}

pub(crate) const COLUMNS: &str = "seq,id,sender,recipient,kind,correlation,task_id,request_kind,body,payload,urgency,deadline_ms,created_ms,state,answered_by,answered_ms,delivery,delivery_attempts,delivery_error,delivered_ms,ended_by,ended_ms,end_reason,because_of";

pub(crate) fn message(r: &Row) -> rusqlite::Result<Value> {
    let payload: String = r.get(9)?;
    Ok(json!({
        "seq": r.get::<_, i64>(0)?, "id": r.get::<_, String>(1)?,
        "from": r.get::<_, String>(2)?, "to": r.get::<_, String>(3)?,
        "kind": r.get::<_, String>(4)?, "correlation": r.get::<_, Option<String>>(5)?,
        "task_id": r.get::<_, Option<String>>(6)?, "request_kind": r.get::<_, Option<String>>(7)?,
        "body": r.get::<_, String>(8)?,
        "payload": serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null),
        "urgency": r.get::<_, String>(10)?, "deadline_ms": r.get::<_, Option<i64>>(11)?,
        "created_ms": r.get::<_, i64>(12)?, "state": r.get::<_, Option<String>>(13)?,
        "answered_by": r.get::<_, Option<String>>(14)?, "answered_ms": r.get::<_, Option<i64>>(15)?,
        "delivery": r.get::<_, Option<String>>(16)?, "delivery_attempts": r.get::<_, i64>(17)?,
        "delivery_error": r.get::<_, Option<String>>(18)?, "delivered_ms": r.get::<_, Option<i64>>(19)?,
        "ended_by": r.get::<_, Option<String>>(20)?, "ended_ms": r.get::<_, Option<i64>>(21)?,
        "end_reason": r.get::<_, Option<String>>(22)?,
        "because_of": r.get::<_, Option<String>>(23)?,
    }))
}

fn by_id(store: &Store, id: &str) -> Result<Option<Value>> {
    Ok(store
        .connection()
        .query_row(
            &format!("SELECT {COLUMNS} FROM mail WHERE id=?1"),
            [id],
            message,
        )
        .optional()?)
}

/// Overdue is derived from the deadline when read, never stored.
fn annotate(mut message: Value, now: i64) -> Value {
    let overdue = message["state"] == "open"
        && message["deadline_ms"]
            .as_i64()
            .is_some_and(|deadline| deadline < now);
    message["overdue"] = json!(overdue);
    message
}

fn optional_str<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>> {
    args.get(key)
        .filter(|v| !v.is_null())
        .map(|v| {
            v.as_str()
                .with_context(|| format!("{key} must be a string"))
        })
        .transpose()
}

/// Allocate only runtime outcome IDs, under the caller's write lock. Client
/// IDs remain untouched. The committed request link, not a computed ID, is the
/// durable idempotency key. Bound retries even in a pathological collision.
fn outcome_id(store: &Store, prefix: &str) -> Result<String> {
    for _ in 0..8 {
        let id = format!("{prefix}-{}", uuid::Uuid::new_v4());
        let occupied: bool = store.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM mail WHERE id=?1)",
            [&id],
            |r| r.get(0),
        )?;
        if !occupied {
            return Ok(id);
        }
    }
    bail!("could not allocate a runtime outcome ID")
}

/// Record a sender-authored closure as an ordinary correlated notice. Old
/// peers can retain the notice without understanding it; new peers apply it.
/// The caller holds the write transaction. Delivery uses the existing outbox.
pub(crate) fn end_request(store: &Store, request: &Value, state: &str, reason: &str) -> Result<()> {
    let id = outcome_id(store, "end")?;
    let peer = ship(store, request["to"].as_str().context("request recipient")?)?
        .as_ref()
        .and_then(remote_peer)
        .map(str::to_owned);
    let payload = json!({"request_end":{"version":1,"state":state,"reason":reason}});
    let now = Store::now_ms();
    store.connection().execute(
        "INSERT INTO mail(id,sender,recipient,kind,correlation,task_id,body,payload,urgency,created_ms,delivery)
         VALUES(?1,?2,?3,'notice',?4,?5,?6,?7,'low',?8,?9)",
        params![id,request["from"].as_str(),request["to"].as_str(),request["id"].as_str(),
        request["task_id"].as_str(),reason,payload.to_string(),now,peer.map(|_| "pending")])?;
    store.connection().execute(
        "UPDATE mail SET state=?2,ended_by=?3,ended_ms=?4,end_reason=?5 WHERE id=?1 AND state='open'",
        params![request["id"].as_str(),state,id,now,reason])?;
    Ok(())
}

fn withdraw(store: &Store, args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    identifier(id)?;
    let reason = optional_str(args, "reason")?.unwrap_or("");
    ensure!(reason.len() <= 2000, "withdrawal reason exceeds 2000 bytes");
    let tx = store.write_transaction()?;
    let request = by_id(store, id)?.context("unknown request")?;
    // Omitted actor keeps the public captain-desk API. Agents always resolve
    // to themselves, never to the message's sender. Operators may name the
    // actual sender explicitly, or select it through the request ID.
    let actor = optional_str(args, "actor")?.unwrap_or(match store.caller() {
        Caller::Agent { actor } => actor.as_str(),
        Caller::Operator => request["from"].as_str().context("request sender")?,
    });
    if let Caller::Agent { actor: caller } = store.caller() {
        ensure!(actor == caller, "agent can withdraw only as itself");
    }
    local_address(store, actor)?;
    ensure!(
        actor != CAPTAIN || *store.caller() == Caller::Operator,
        "only operator speaks as captain"
    );
    ensure!(
        request["from"] == actor,
        "only the sender can withdraw its request"
    );
    ensure!(
        matches!(request["kind"].as_str(), Some("request" | "decision")),
        "only requests can be withdrawn"
    );
    if request["state"] == "withdrawn" {
        ensure!(
            request["end_reason"] == reason,
            "withdrawal already has a different reason"
        );
    } else {
        ensure!(request["state"] == "open", "request is already closed");
        end_request(store, &request, "withdrawn", reason)?;
        store.audit(
            "mail.withdraw",
            &json!({"id":id,"actor":actor,"reason":reason}),
        )?;
    }
    tx.commit()?;
    // Items added to a withdrawn block ask end with it.
    if request["payload"]["type"] == "tool_permission" && request["to"] == CAPTAIN {
        let why = if reason.is_empty() {
            "withdrawn"
        } else {
            reason
        };
        if let Err(error) = end_attachments(store, id, why) {
            eprintln!("privateer: could not end the items added to {id}: {error:#}");
        }
    }
    // Bounded and retryable, like mail.send. Task closure only enqueues.
    // Preserve FIFO and retry delays even when this request is retried.
    let peer = ship(store, request["to"].as_str().context("request recipient")?)?
        .as_ref()
        .and_then(remote_peer)
        .map(str::to_owned);
    if let Some(peer) = peer.as_deref() {
        flush_pending(store, 1, false, None, Some(peer))?;
    }
    let mut result = by_id(store, id)?.context("request vanished")?;
    result["remote"] = if peer.is_some() {
        let (applied, delivery, error): (Option<String>, Option<String>, Option<String>) =
            store.connection().query_row(
                "SELECT remote_end_state,delivery,delivery_error FROM mail WHERE id=?1",
                [result["ended_by"].as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
        // Only a verified application receipt may supply a state token.
        // Keep the raw error durable, but distinguish it in this result.
        json!(
            applied
                .or_else(|| error.map(|e| format!("error: {e}")))
                .unwrap_or_else(|| if delivery.as_deref() == Some("delivered") {
                    "unknown".to_owned()
                } else {
                    "pending".to_owned()
                })
        )
    } else {
        Value::Null
    };
    Ok(result)
}

/// The captain ends one item addressed to them without answering it. An open
/// request or decision ends as `dismissed` with the note, and its sender gets
/// a notice saying so, carried to a ship behind a machine link like a reply;
/// a notice is marked acknowledged. The operator's lookup (`captain.dismiss`)
/// has chosen this home.
pub(crate) fn dismiss(store: &Store, id: &str, note: &str) -> Result<Value> {
    ensure!(
        *store.caller() == Caller::Operator,
        "only the operator speaks as the captain"
    );
    ensure!(note.len() <= 2000, "dismissal note exceeds 2000 bytes");
    let tx = store.write_transaction()?;
    let item = by_id(store, id)?.with_context(|| format!("unknown message {id}"))?;
    ensure!(
        item["to"] == CAPTAIN,
        "{id} is not addressed to the captain"
    );
    let now = Store::now_ms();
    let mut peer = None;
    match item["kind"].as_str() {
        Some("notice") => {
            ensure!(item["state"].is_null(), "{id} is already acknowledged");
            tx.execute(
                "UPDATE mail SET state='acknowledged',ended_by=?2,ended_ms=?3,end_reason=?4 WHERE id=?1 AND state IS NULL",
                params![id, CAPTAIN, now, note],
            )?;
        }
        Some("request" | "decision") => {
            ensure!(item["state"] == "open", "{id} is no longer open");
            let asker = item["from"].as_str().context("request sender")?;
            peer = ship(store, asker)?
                .as_ref()
                .and_then(remote_peer)
                .map(str::to_owned);
            let notice = outcome_id(store, "dismiss")?;
            let body = if note.is_empty() {
                format!("The captain closed {id} without an answer.")
            } else {
                format!("The captain closed {id} without an answer: {note}")
            };
            let payload = json!({"request_end":{"version":1,"state":"dismissed","reason":note}});
            tx.execute(
                "INSERT INTO mail(id,sender,recipient,kind,correlation,task_id,body,payload,urgency,created_ms,delivery)
                 VALUES(?1,?2,?3,'notice',?4,?5,?6,?7,'normal',?8,?9)",
                params![notice, CAPTAIN, asker, id, item["task_id"].as_str(), body,
                    payload.to_string(), now, peer.as_ref().map(|_| "pending")],
            )?;
            tx.execute(
                "UPDATE mail SET state='dismissed',ended_by=?2,ended_ms=?3,end_reason=?4 WHERE id=?1 AND state='open'",
                params![id, notice, now, note],
            )?;
        }
        other => bail!(
            "{id} is a {}, which the captain cannot dismiss",
            other.unwrap_or("message")
        ),
    }
    store.audit("captain.dismiss", &json!({"id": id, "note": note}))?;
    tx.commit()?;
    // Items added to a dismissed block ask end with it.
    if item["payload"]["type"] == "tool_permission" && item["kind"] != "notice" {
        let why = if note.is_empty() { "dismissed" } else { note };
        if let Err(error) = end_attachments(store, id, why) {
            eprintln!("privateer: could not end the items added to {id}: {error:#}");
        }
    }
    // Bounded and retryable, like mail.send: a failed attempt stays pending.
    if let Some(peer) = peer.as_deref() {
        flush_pending(store, 1, false, None, Some(peer))?;
    }
    Ok(annotate(
        by_id(store, id)?.context("message vanished")?,
        Store::now_ms(),
    ))
}

/// A task's terminal state, not its current owner, ends its linked requests.
/// Runs in task.update's transaction; never invents a reply or changes custody.
pub(crate) fn close_task(store: &Store, task: &str, status: &str) -> Result<()> {
    let mut statement = store.connection().prepare(&format!(
        "SELECT {COLUMNS} FROM mail WHERE task_id=?1 AND state='open'"
    ))?;
    let requests = statement
        .query_map([task], message)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for request in requests {
        end_request(
            store,
            &request,
            &format!("task_{status}"),
            &format!("linked task {task} is {status}"),
        )?;
    }
    Ok(())
}

/// Complete an exact-artifact approval request in the grant/deny transaction.
pub(crate) fn approval_answer(
    store: &Store,
    request: &Value,
    state: &str,
    args: &Value,
) -> Result<Value> {
    let payload = json!({"approval_outcome":state,"approval_id":args.get("id"),
        "artifact":args["artifact"],"environment":args["environment"]});
    if request["state"] != "open" {
        ensure!(
            request["state"] == state,
            "approval response differs from recorded outcome"
        );
        let id = request["answered_by"]
            .as_str()
            .context("closed approval has no recorded answer")?;
        let existing = by_id(store, id)?.context("recorded approval answer missing")?;
        ensure!(
            existing["from"] == CAPTAIN
                && existing["to"] == request["from"]
                && existing["kind"] == "reply"
                && existing["correlation"] == request["id"]
                && existing["task_id"] == request["task_id"]
                && existing["payload"] == payload,
            "approval response differs from recorded outcome"
        );
        return Ok(existing);
    }
    let id = outcome_id(store, "approval-answer")?;
    let now = Store::now_ms();
    store.connection().execute(
        "INSERT INTO mail(id,sender,recipient,kind,correlation,task_id,body,payload,urgency,created_ms)
         VALUES(?1,'captain',?2,'reply',?3,?4,?5,?6,'normal',?7)",
        params![id,request["from"].as_str(),request["id"].as_str(),request["task_id"].as_str(),state,payload.to_string(),now])?;
    store.connection().execute("UPDATE mail SET state=?2,answered_by=?3,answered_ms=?4,ended_by=?3,ended_ms=?4,end_reason=?2 WHERE id=?1 AND state='open'",
        params![request["id"].as_str(),state,id,now])?;
    store.audit("approval.response", &json!({"request_id":request["id"],"state":state,"answer_id":id,"approval_id":args.get("id")}))?;
    by_id(store, &id)?.context("approval answer vanished")
}

/// The hands-on step a payload marks, if any. Only a request to the captain
/// may carry the marker, and the step must be a real, bounded instruction: the
/// captain's list shows it beside the ask. Checked on every path a message
/// enters this home, so a collected or delivered record cannot skip it.
pub(crate) fn hands_step(kind: &str, to: &str, payload: &Value) -> Result<Option<String>> {
    let Some(hands) = payload.get("needs_hands") else {
        return Ok(None);
    };
    ensure!(
        kind == "request" && to == CAPTAIN,
        "needs_hands marks a request to the captain"
    );
    let step = hands
        .get("step")
        .and_then(Value::as_str)
        .context("needs_hands needs a step: the exact thing the captain must do")?;
    ensure!(
        !step.trim().is_empty() && step.len() <= MAX_HANDS_STEP_BYTES,
        "needs_hands step must be 1 to {MAX_HANDS_STEP_BYTES} bytes"
    );
    Ok(Some(step.to_owned()))
}

/// End every open request in this home bound to a stow, as withdrawn on the
/// sender's behalf, with the reason `why` gives that stow. `machine` selects
/// the binding: `None` is a stow on this machine, `Some` one on a linked
/// machine. `why` returning `None` leaves that request open, which is how an
/// exchange keeps a request bound to a stow the peer reports as in force.
///
/// Only open requests are touched, so a repeated resume or exchange writes no
/// second closing notice. The runtime owns this closure: the sender asked for
/// it by recording the binding, and `withdrawn` is a state a peer accepts.
pub(crate) fn end_stow_bound<F>(store: &Store, machine: Option<&str>, why: F) -> Result<usize>
where
    F: Fn(&str) -> Option<String>,
{
    // A received copy is the sender's request: the sender's home closes it and
    // the closing notice carries the end here. So only rows this home sent, or
    // that a ship hosted here sent, are ended by this home.
    // One write covers the scan and the ends: a sender that passed its stow
    // check either committed before this scan or reads the resumed ledger.
    let tx = store.write_transaction()?;
    let ids = {
        let mut statement = store.connection().prepare(
            "SELECT id FROM mail WHERE state='open'
             AND json_extract(payload,'$.depends_on.stow') IS NOT NULL
             AND json_extract(payload,'$.depends_on.machine') IS ?1
             AND (delivery IS NOT 'received' OR sender IN
               (SELECT name FROM ships WHERE json_extract(record,'$.location')='here'))",
        )?;
        statement
            .query_map([machine], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut ended = 0;
    for id in ids {
        if let Some(request) = by_id(store, &id)?.filter(|r| r["state"] == "open") {
            let stow = request["payload"]["depends_on"]["stow"]
                .as_str()
                .unwrap_or_default();
            if let Some(reason) = why(stow) {
                end_request(store, &request, "withdrawn", &reason)?;
                ended += 1;
            }
        }
    }
    tx.commit()?;
    Ok(ended)
}

/// A request may record the stow it depends on, so a resume ends it instead of
/// leaving it open to be relayed as current fact. Explicit only: nothing is
/// read from the body. `{"stow": id}` is a stow on this machine, which must
/// exist and still be in force; `machine` names a linked machine, whose stow
/// is accepted even if this home has not heard of it yet, because a stow id
/// reaches this home only through an exchange.
fn bind(payload: &mut Value, opens: bool) -> Result<()> {
    let Some(binding) = payload.get("depends_on").cloned() else {
        return Ok(());
    };
    ensure!(opens, "depends_on applies to requests and decisions");
    ensure!(binding.is_object(), "depends_on must be an object");
    let stow = required(&binding, "stow")?;
    ensure!(
        !stow.is_empty() && stow.len() <= 200,
        "a stow id is a short string"
    );
    // A binding that names this machine is this machine's stow: one spelling
    // for one stow, so the local check and the local resume both find it.
    let this = crate::machines::this_machine();
    let machine = match binding.get("machine").filter(|v| !v.is_null()) {
        None => None,
        Some(machine) => {
            let machine = machine
                .as_str()
                .context("depends_on.machine must be a name")?;
            identifier(machine)?;
            if machine == this {
                None
            } else {
                ensure!(
                    crate::machines::linked(&crate::fleet::registry()?, machine)?,
                    "no linked machine named {machine}"
                );
                Some(machine.to_owned())
            }
        }
    };
    payload["depends_on"] = match machine {
        Some(machine) => json!({"stow": stow, "machine": machine}),
        None => json!({"stow": stow}),
    };
    Ok(())
}

/// A binding to a stow on this machine must name one that exists and is still
/// in force. Called inside the write transaction that stores the request: a
/// resume lifts the hold before it ends bound requests, so a request stored
/// after the lift is refused here, and one stored before it is ended there.
fn confirm_stow(store: &Store, payload: &Value) -> Result<()> {
    let Some(binding) = payload.get("depends_on") else {
        return Ok(());
    };
    if binding.get("machine").is_some() {
        return Ok(());
    }
    let stow = binding["stow"].as_str().context("depends_on.stow")?;
    let ledger = crate::jobs::host_ledger(store)?;
    let known: Option<Option<i64>> = ledger
        .query_row("SELECT resumed_ms FROM stows WHERE id=?1", [stow], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(resumed_ms) = known else {
        bail!("no stow {stow} on this machine");
    };
    ensure!(
        resumed_ms.is_none(),
        "stow {stow} on this machine has resumed"
    );
    Ok(())
}

const DECISION_FIELDS: &[&str] = &[
    "question",
    "why",
    "blocked",
    "options",
    "free_text_ok",
    "group",
];
const OPTION_FIELDS: &[&str] = &["id", "label", "what_happens", "cost", "recommended"];
const MAX_DECISION_OPTIONS: usize = 8;

/// A string whose length in characters is within `min..=max`. Errors name the
/// field and its bounds, never the value.
fn bounded_text<'a>(value: &'a Value, field: &str, min: usize, max: usize) -> Result<&'a str> {
    let text = value
        .as_str()
        .with_context(|| format!("{field} must be a string"))?;
    let len = text.chars().count();
    ensure!(
        (min..=max).contains(&len),
        "{field} must be {min} to {max} characters"
    );
    Ok(text)
}

/// A short identifier: letters, digits, `-` and `_`.
fn decision_identifier<'a>(value: &'a Value, field: &str, max: usize) -> Result<&'a str> {
    let id = bounded_text(value, field, 1, max)?;
    ensure!(
        id.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "{field} takes letters, digits, - and _"
    );
    Ok(id)
}

/// The card a captain answers with one tap. Its shape is checked here, before
/// anything is stored, so every reader gets the same well-formed decision.
fn validate_decision(decision: &Value) -> Result<()> {
    let fields = decision.as_object().context("decision must be an object")?;
    ensure!(
        fields.keys().all(|k| DECISION_FIELDS.contains(&k.as_str())),
        "decision takes only {}",
        DECISION_FIELDS.join(", ")
    );
    bounded_text(
        decision
            .get("question")
            .context("decision.question is required")?,
        "decision.question",
        1,
        200,
    )?;
    if let Some(why) = decision.get("why") {
        bounded_text(why, "decision.why", 0, 500)?;
    }
    if let Some(blocked) = decision.get("blocked") {
        let items = blocked
            .as_array()
            .context("decision.blocked must be a list")?;
        ensure!(items.len() <= 10, "decision.blocked takes at most 10 items");
        for item in items {
            bounded_text(item, "decision.blocked items", 0, 200)?;
        }
    }
    if let Some(free_text_ok) = decision.get("free_text_ok") {
        ensure!(
            free_text_ok.is_boolean(),
            "decision.free_text_ok must be true or false"
        );
    }
    if let Some(group) = decision.get("group") {
        decision_identifier(group, "decision.group", 64)?;
    }
    let options = decision
        .get("options")
        .and_then(Value::as_array)
        .context("decision.options must be a list")?;
    ensure!(
        (2..=MAX_DECISION_OPTIONS).contains(&options.len()),
        "decision.options takes 2 to {MAX_DECISION_OPTIONS} options"
    );
    let mut ids = HashSet::new();
    let mut recommended = 0;
    for option in options {
        let fields = option
            .as_object()
            .context("decision.options items must be objects")?;
        ensure!(
            fields.keys().all(|k| OPTION_FIELDS.contains(&k.as_str())),
            "decision.options items take only {}",
            OPTION_FIELDS.join(", ")
        );
        let id = decision_identifier(
            option
                .get("id")
                .context("decision.options items need an id")?,
            "decision.options id",
            32,
        )?;
        ensure!(ids.insert(id), "decision.options ids must be unique");
        bounded_text(
            option
                .get("label")
                .context("decision.options items need a label")?,
            "decision.options label",
            1,
            80,
        )?;
        bounded_text(
            option
                .get("what_happens")
                .context("decision.options items need what_happens")?,
            "decision.options what_happens",
            1,
            400,
        )?;
        if let Some(cost) = option.get("cost") {
            bounded_text(cost, "decision.options cost", 0, 80)?;
        }
        if let Some(flag) = option.get("recommended") {
            let flag = flag
                .as_bool()
                .context("decision.options recommended must be true or false")?;
            recommended += usize::from(flag);
        }
    }
    ensure!(
        recommended <= 1,
        "decision.options takes at most one recommended option"
    );
    Ok(())
}

/// Check the typed parts of a message's payload, and fill in what older doors
/// read. Shared by sending and by delivery, so a home refuses the same shapes
/// whichever way they arrive.
pub(crate) fn typed_payload(kind: &str, payload: &mut Value) -> Result<()> {
    // Only a decision message carries a typed decision. Other mail keeps any
    // payload it had, including a key named decision or answer, as before.
    if kind != "decision" {
        return Ok(());
    }
    if let Some(decision) = payload.get("decision").cloned() {
        validate_decision(&decision)?;
        // Older doors show only plain choices, so the labels stand in for them.
        if payload.get("choices").is_none() {
            payload["choices"] = json!(
                decision["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|option| option["label"].clone())
                    .collect::<Vec<_>>()
            );
        }
    }
    Ok(())
}

/// The typed decision a reply answers, if the thread is one.
fn typed_decision(thread: &Value) -> Option<&Value> {
    (thread["kind"] == "decision")
        .then(|| thread["payload"].get("decision"))
        .flatten()
}

/// The option ids a decision offers, in its own order.
fn decision_option_ids(decision: &Value) -> Vec<&str> {
    decision["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|option| option["id"].as_str())
        .collect()
}

/// A captain's typed answer to a decision: one of its option ids, free text, or
/// both. Free text is always accepted, whatever the decision's `free_text_ok`.
fn check_answer(decision: &Value, answer: &Value) -> Result<()> {
    let fields = answer.as_object().context("answer must be an object")?;
    ensure!(
        fields
            .keys()
            .all(|k| matches!(k.as_str(), "option_id" | "text")),
        "answer takes only option_id and text"
    );
    ensure!(
        fields.contains_key("option_id") || fields.contains_key("text"),
        "answer needs option_id, text or both"
    );
    if let Some(option_id) = fields.get("option_id") {
        let option_id = option_id
            .as_str()
            .context("answer.option_id must be a string")?;
        ensure!(
            decision_option_ids(decision).contains(&option_id),
            "answer.option_id must be one of the decision's option ids"
        );
    }
    if let Some(text) = fields.get("text") {
        // As long as a Telegram message, so a typed reply to a card always fits.
        bounded_text(text, "answer.text", 1, 4096)?;
    }
    Ok(())
}

/// A payload without the fields the runtime adds for its topic, which may
/// differ on a replay once the ask it named has closed.
fn without_topic(payload: &Value) -> Value {
    let mut payload = payload.clone();
    if let Some(fields) = payload.as_object_mut() {
        for key in ["about", "attached_to", "attached_ship"] {
            fields.remove(key);
        }
    }
    payload
}

/// Whose words a captain reply carries, for the operator marker that alone
/// lets an answer widen fleet permissions.
#[derive(Clone, Copy)]
pub(crate) enum Mark {
    /// The caller's own: the operator's reply is marked.
    Caller,
    /// Someone passing the captain's words on: never marked.
    Relayed,
    /// A runtime copy of another reply, which keeps that reply's marker.
    Keep(bool),
}

fn send(store: &Store, args: &Value) -> Result<Value> {
    send_with(store, args, Mark::Caller)
}

/// `mail.send` for a captain reply that relays the captain's words: it is
/// stored as a reply, but carries no operator marker.
pub(crate) fn send_relayed(store: &Store, args: &Value) -> Result<Value> {
    send_with(store, args, Mark::Relayed)
}

/// `mail.send` for a runtime copy of a captain reply (a fan-out to an
/// attached ask), keeping the original's operator marker.
pub(crate) fn send_copy(store: &Store, args: &Value, operator: bool) -> Result<Value> {
    send_with(store, args, Mark::Keep(operator))
}

fn send_with(store: &Store, args: &Value, mark: Mark) -> Result<Value> {
    ensure!(
        args.to_string().len() <= MAX_MESSAGE_BYTES,
        "message exceeds 64 KiB{}",
        if args["payload"]["report"]["content"].is_string() {
            "; put a report this large with report.put and send payload.report {report_id}"
        } else {
            ""
        }
    );
    let from = required(args, "from")?;
    let to = required(args, "to")?;
    let kind = required(args, "kind")?;
    ensure!(
        KINDS.contains(&kind),
        "kind must be request, reply, notice or decision"
    );
    if from == CAPTAIN {
        ensure!(
            *store.caller() == Caller::Operator,
            "only the operator speaks as the captain"
        );
    }
    ensure!(from != CREW, "only the runtime speaks as {CREW}");
    // Crew have no mailbox: a first mate reaches them by answering their
    // requests, which they read with crew.answer.
    ensure!(
        to != CREW || kind == "reply",
        "{CREW} is answered only by replying to its requests"
    );
    local_address(store, from)?;
    let recipient = if to == CAPTAIN || to == CREW {
        None
    } else {
        ship(store, to)?
    };
    ensure!(
        to == CAPTAIN || to == CREW || recipient.is_some(),
        "unknown ship {to}; add it with ship.put"
    );
    ensure!(from != to, "a mailbox cannot write to itself");
    let peer = recipient.as_ref().and_then(remote_peer).map(str::to_owned);
    if let Some(peer) = &peer
        && from == CAPTAIN
        && !crate::peers::machine(store, peer)?
    {
        // The captain's mailbox is per home. A ship on this machine takes the
        // captain's mail in its own home, as a tell from its folder would; a
        // ship behind any other peer is reached only over a machine link.
        return crate::captain::send_local(args);
    }
    let id = optional_str(args, "id")?
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    identifier(&id)?;
    let body = args.get("body").and_then(Value::as_str).unwrap_or("");
    let mut payload = args.get("payload").cloned().unwrap_or(json!({}));
    ensure!(payload.is_object(), "payload must be an object");
    ensure!(
        payload.get("request_end").is_none(),
        "request_end is reserved for runtime closure"
    );
    for reserved in ["operator", "attached_to", "attached_ship"] {
        ensure!(
            payload.get(reserved).is_none(),
            "payload.{reserved} is set by the runtime"
        );
    }
    // Block asks and their links are the runtime's own: a ship that could
    // write them could hide a card behind a real one and have its rules
    // granted by the captain's answer to that one. Only a runtime copy keeps
    // what the reply it copies carried.
    if !matches!(mark, Mark::Keep(_)) {
        ensure!(
            payload["type"] != "tool_permission",
            "tool-permission asks are raised by the runtime"
        );
        for reserved in [
            "block",
            "about",
            "fanned_from",
            "block_notice",
            "block_cleared",
        ] {
            ensure!(
                payload.get(reserved).is_none(),
                "payload.{reserved} is set by the runtime; pass about as an argument"
            );
        }
    }
    // Only the operator's own answer to a block ask is marked; a relay or a
    // bridge is not. Other replies keep the payload they were given.
    let operator = from == CAPTAIN
        && kind == "reply"
        && match mark {
            Mark::Caller => {
                *store.caller() == Caller::Operator && payload.get("relayed_by").is_none()
            }
            Mark::Relayed => false,
            Mark::Keep(operator) => operator,
        };
    let about = optional_str(args, "about")?;
    if let Some(about) = about {
        identifier(about)?;
    }
    if args["request_kind"] == "production_approval" {
        ensure!(
            kind == "request" && to == CAPTAIN,
            "production approvals are requests to captain"
        );
        let task = required(args, "task_id")?;
        ensure!(
            required(&payload, "artifact")?.len() <= 1024
                && required(&payload, "environment")?.len() <= 1024,
            "approval artifact and environment are limited to 1024 bytes"
        );
        let charter = store.active_charter(task)?;
        payload["charter_hash"] = json!(crate::core::hash(&charter));
    }
    typed_payload(kind, &mut payload)?;
    if let Some(choices) = payload.get("choices") {
        let choices = choices.as_array().context("choices must be an array")?;
        ensure!(
            choices.len() <= 20
                && choices.iter().all(|v| v
                    .as_str()
                    .is_some_and(|s| !s.is_empty() && s.chars().count() <= 200)),
            "choices must contain at most 20 bounded strings"
        );
    }
    hands_step(kind, to, &payload)?;
    crate::report::check_report(store, from, to, &mut payload)?;
    crate::files::check_files(store, from, to, &mut payload)?;
    crate::report::check_annotations(from, kind, &payload)?;
    ensure!(
        !body.is_empty() || payload.as_object().is_some_and(|p| !p.is_empty()),
        "a message needs a body or a payload"
    );
    let urgency = optional_str(args, "urgency")?.unwrap_or("normal");
    ensure!(
        URGENCIES.contains(&urgency),
        "urgency must be low, normal or high"
    );
    let task_id = optional_str(args, "task_id")?;
    if let Some(task) = task_id {
        store.task(task).map_err(|error| {
            anyhow::anyhow!(
                "{error:#}: task ids belong to the ship that holds them; to link another \
                 ship's work, reply to its request with correlation instead of naming its task"
            )
        })?;
    }
    let request_kind = optional_str(args, "request_kind")?;
    if let Some(request_kind) = request_kind {
        ensure!(kind == "request", "request_kind applies to requests");
        identifier(request_kind)?;
        let accepts = recipient
            .as_ref()
            .and_then(|s| s["accepts"].as_array().cloned())
            .unwrap_or_default();
        ensure!(
            accepts.is_empty() || accepts.iter().any(|k| k == request_kind),
            "{to} does not accept {request_kind} requests"
        );
    }
    if kind == "decision" {
        ensure!(to == CAPTAIN, "decisions are addressed to the captain");
    }
    let opens = matches!(kind, "request" | "decision");
    bind(&mut payload, opens)?;
    let deadline_ms = match args.get("reply_within_secs").filter(|v| !v.is_null()) {
        Some(v) => {
            ensure!(opens, "reply_within_secs applies to requests and decisions");
            let secs = v
                .as_u64()
                .filter(|s| (1..=30 * 86_400).contains(s))
                .context("reply_within_secs must be 1..2592000")?;
            Some(Store::now_ms() + secs as i64 * 1000)
        }
        None => None,
    };
    let correlation = optional_str(args, "correlation")?;
    ensure!(
        kind != "reply" || correlation.is_some(),
        "a reply names the request it answers as correlation"
    );
    let because_of = optional_str(args, "because_of")?;
    if let Some(parent) = because_of {
        identifier(parent)?;
        ensure!(
            kind == "request",
            "because_of is the request a follow-up asks about; only a request carries it"
        );
        ensure!(parent != id, "a request cannot be raised because of itself");
    }

    let tx = store.write_transaction()?;
    if operator && correlation.is_some_and(|c| crate::block_ask::is_ask_id(store, c)) {
        payload["operator"] = json!(true);
    }
    // The topic is the runtime's: the captain decision this mail is about.
    // Captain mail on an open block ask's topic is attached to it.
    let mut urgency = urgency;
    let mut attached = None;
    if kind != "reply" && from != CAPTAIN {
        if let Some(topic) = crate::block_ask::topic(store, about, correlation, task_id, kind)? {
            payload["about"] = json!(topic);
            let primary = crate::block_ask::open_primary(store, &topic)?;
            if let Some((ask, ship)) = &primary {
                if to == CAPTAIN
                    && request_kind != Some("production_approval")
                    && payload.get("needs_hands").is_none()
                {
                    payload["attached_to"] = json!(ask);
                    payload["attached_ship"] = json!(ship);
                    attached = primary.clone();
                } else if kind == "notice" && ship == to {
                    // "Still blocked?" to the blocked ship waits for its next real wake.
                    urgency = "low";
                }
            }
        }
    }
    if let Some(existing) = by_id(store, &id)? {
        let same = existing["from"] == from
            && existing["to"] == to
            && existing["kind"] == kind
            && existing["correlation"] == json!(correlation)
            && existing["because_of"] == json!(because_of)
            && existing["task_id"] == json!(task_id)
            && existing["request_kind"] == json!(request_kind)
            && existing["body"] == body
            && without_topic(&existing["payload"]) == without_topic(&payload);
        ensure!(same, "message id already holds a different message");
        tx.commit()?;
        let mut existing = annotate(existing, Store::now_ms());
        existing["duplicate"] = json!(true);
        return Ok(existing);
    }
    confirm_stow(store, &payload)?;
    if let Some(task) = task_id {
        let linked = store.task(task)?;
        ensure!(
            !opens || !matches!(linked["status"].as_str(), Some("done" | "cancelled")),
            "task is terminal"
        );
        if request_kind == Some("production_approval") {
            ensure!(
                payload["charter_hash"] == crate::core::hash(&store.active_charter(task)?),
                "approval authority changed"
            );
        }
    }
    // Only a gateway link delivers now. Over a collected link the reply waits
    // for the other machine to collect it, so it cannot be judged here.
    let direct = match &peer {
        Some(peer) => crate::peers::direct_machine(store, peer)?,
        None => false,
    };
    if let Some(correlation) = correlation {
        match by_id(store, correlation)? {
            Some(thread) => {
                if kind == "reply" {
                    ensure!(
                        matches!(thread["kind"].as_str(), Some("request" | "decision")),
                        "only requests and decisions take replies"
                    );
                    ensure!(
                        thread["to"] == from && thread["from"] == to,
                        "only the recipient of {correlation} can answer it, to its sender"
                    );
                    ensure!(
                        thread["state"] == "open",
                        "{correlation} was already answered"
                    );
                    // Only an answer to a typed decision is checked against its options.
                    if let (Some(answer), Some(decision)) =
                        (payload.get("answer"), typed_decision(&thread))
                    {
                        check_answer(decision, answer)?;
                    }
                    crate::block_ask::check_answer(&thread, &payload)?;
                }
            }
            // A notice only groups a thread, and the thread it corrects may
            // live in the other home: a ship that never stored that id is
            // exactly the one whose correction must still arrive. Replies and
            // requests still name something this home can judge.
            None => ensure!(
                (kind == "notice" && peer.is_some())
                    || (from == CAPTAIN && kind == "reply" && direct),
                "unknown correlated message {correlation}"
            ),
        }
    }
    let now = Store::now_ms();
    tx.execute(
        "INSERT INTO mail(id,sender,recipient,kind,correlation,task_id,request_kind,body,payload,urgency,deadline_ms,created_ms,state,delivery,because_of)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
        params![
            id,
            from,
            to,
            kind,
            correlation,
            task_id,
            request_kind,
            body,
            payload.to_string(),
            urgency,
            deadline_ms,
            now,
            opens.then_some("open"),
            peer.as_ref().map(|_| "pending"),
            because_of
        ],
    )?;
    if peer.is_some() {
        let message = by_id(store, &id)?.context("message vanished")?;
        ensure!(
            envelope(&message).to_string().len() <= MAX_MESSAGE_BYTES,
            "message exceeds 64 KiB"
        );
    }
    if kind == "reply" {
        tx.execute(
            "UPDATE mail SET state='answered', answered_by=?2, answered_ms=?3 WHERE id=?1",
            params![correlation, id, now],
        )?;
    }
    tx.commit()?;
    // Recorded first, delivered second: a failed attempt leaves the message
    // pending for mail.flush rather than losing it.
    if let Some(peer) = peer {
        flush_pending(store, 1, false, None, Some(&peer))?;
    }
    let stored = by_id(store, &id)?.context("message vanished")?;
    if kind == "reply"
        && let Some(correlation) = correlation
    {
        crate::block_ask::answered(store, correlation, &stored, true);
    }
    if kind == "reply" && from == CAPTAIN {
        record_reply_at_desk(store, &stored);
    }
    let mut result = annotate(stored, Store::now_ms());
    if let Some((ask, ship)) = attached {
        result["attached_to"] = json!(ask);
        result["note"] = json!(format!(
            "The captain already has this as {ask} ({ship}, {}). Your message was added to {ask}; you will get the captain's answer. Do not resend.",
            crate::block_ask::rules_of(store, &ask).unwrap_or_default()
        ));
    } else if to == CAPTAIN
        // The mail is stored: a preview that cannot read the registry must
        // not report the send as failed, or the ship resends it.
        && let Some((delivery, note)) = crate::away::preview(&result).ok().flatten()
    {
        result["delivery"] = json!(delivery);
        result["note"] = json!(note);
    }
    Ok(result)
}

/// The envelope another home receives. Task ids are local to a home and do
/// not travel.
fn envelope(message: &Value) -> Value {
    let mut envelope = json!({});
    for key in [
        "id",
        "from",
        "to",
        "kind",
        "correlation",
        "request_kind",
        "body",
        "payload",
        "urgency",
        "deadline_ms",
        "created_ms",
        // An old peer keeps the message and ignores the link; the follow-up's
        // own home is where the wake happens.
        "because_of",
    ] {
        envelope[key] = message[key].clone();
    }
    envelope
}

enum Delivery {
    Delivered,
    Failed,
    Quarantined,
}

/// Classify a legacy wire-invalid record without changing its facts or
/// treating a local protocol violation as a failure of the peer.
fn quarantine(store: &Store, id: &str, wire_bytes: usize) -> Result<()> {
    let tx = store.write_transaction()?;
    let changed = tx.execute(
        "UPDATE mail SET delivery='quarantined', delivery_error=?2
         WHERE id=?1 AND delivery='pending'",
        params![
            id,
            format!(
                "normalized envelope is {wire_bytes} bytes; limit is {MAX_MESSAGE_BYTES} bytes"
            )
        ],
    )?;
    if changed == 1 {
        store.audit(
            "mail.quarantined",
            &json!({"id":id,"reason":"envelope_too_large",
            "wire_bytes":wire_bytes,"limit_bytes":MAX_MESSAGE_BYTES}),
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// The request-closure state a receiving home reports having applied for
/// this message. Only a receipt for this exact closure proves application;
/// legacy peers accept the notice too, but do not return this evidence.
pub(crate) fn applied_end(message: &Value, result: &Value) -> Option<String> {
    let receipt = &result["request_end"];
    let end = &message["payload"]["request_end"];
    (end.is_object()
        && receipt["id"] == message["correlation"]
        && receipt["ended_by"] == message["id"]
        && receipt["state"] == end["state"])
        .then(|| receipt["state"].as_str().map(str::to_owned))
        .flatten()
}

/// Try once to deliver a pending outgoing message; record the outcome.
fn deliver_one(store: &Store, id: &str, peer: &str, timeout: Option<Duration>) -> Result<Delivery> {
    let message = by_id(store, id)?.with_context(|| format!("unknown message {id}"))?;
    let mut envelope = envelope(&message);
    // A machine names itself to the one it delivers to, which checks the name
    // against its link before trusting a captain reply's operator marker.
    if let Some(name) = crate::machines::known_as_by_peer(peer) {
        envelope["via"] = json!(name);
    }
    let wire_bytes = envelope.to_string().len();
    if wire_bytes > MAX_MESSAGE_BYTES {
        quarantine(store, id, wire_bytes)?;
        return Ok(Delivery::Quarantined);
    }
    let response = crate::peers::call_peer_with_timeout(
        store,
        peer,
        &format!("deliver-{}", uuid::Uuid::new_v4()),
        "mail.deliver",
        envelope,
        timeout,
    );
    // A peer that answered and refused the message, as opposed to one that
    // could not be reached; only a refusal is the captain's to hear about.
    let refusal = match &response {
        Ok(response) if response["ok"] != json!(true) => Some(
            response["error"]
                .as_str()
                .unwrap_or("peer refused delivery")
                .to_owned(),
        ),
        _ => None,
    };
    let outcome = response.and_then(|response| match response["ok"].as_bool() {
        Some(true) => Ok(applied_end(&message, &response["result"])),
        _ => bail!(
            "{}",
            response["error"]
                .as_str()
                .unwrap_or("peer refused delivery")
        ),
    });
    let delivered = outcome.is_ok();
    let now = Store::now_ms();
    let tx = store.write_transaction()?;
    tx.execute(
        "UPDATE mail SET delivery=CASE WHEN ?2 THEN 'delivered' ELSE delivery END,
           delivered_ms=CASE WHEN ?2 THEN ?3 ELSE delivered_ms END,
           delivery_attempts=delivery_attempts+1, delivery_error=?4, remote_end_state=?5
         WHERE id=?1 AND delivery='pending'",
        params![
            id,
            delivered,
            now,
            outcome
                .as_ref()
                .err()
                .map(|e| format!("{e:#}").chars().take(4096).collect::<String>()),
            outcome.as_ref().ok().and_then(|state| state.as_deref())
        ],
    )?;
    // One peer's failure pauses its whole queue. Logical attempt ordering
    // also distinguishes several attempts made within one clock millisecond.
    let failures: u32 = tx.query_row(
        "SELECT COALESCE((SELECT failures FROM mail_delivery_retry WHERE peer=?1), 0)",
        [peer],
        |r| r.get(0),
    )?;
    let (failures, next) = if delivered {
        (0, 0)
    } else {
        let delay = 1_000_i64.saturating_mul(1_i64 << failures.min(6));
        (
            failures.saturating_add(1).min(7),
            now.saturating_add(delay.min(60_000)),
        )
    };
    tx.execute(
        "INSERT INTO mail_delivery_retry(peer, failures, next_attempt_ms, last_attempt_ms)
         VALUES(?1, ?2, ?3, ?4) ON CONFLICT(peer) DO UPDATE SET
           failures=excluded.failures, next_attempt_ms=excluded.next_attempt_ms,
           last_attempt_ms=MAX(excluded.last_attempt_ms, last_attempt_ms+1)",
        params![peer, failures, next, now],
    )?;
    tx.commit()?;
    // A captain's message the peer refused is quarantined: left pending it
    // would hold back every later message to that peer, and retrying a refusal
    // would not change it. The peer answered, so it is not backed off for it.
    // The captain is told, and may send it again.
    if let Some(reason) = refusal.filter(|_| message["from"] == CAPTAIN) {
        let tx = store.write_transaction()?;
        tx.execute(
            "UPDATE mail SET delivery='quarantined' WHERE id=?1 AND delivery='pending'",
            [id],
        )?;
        tx.execute("DELETE FROM mail_delivery_retry WHERE peer=?1", [peer])?;
        tx.commit()?;
        surface_refusal(
            store,
            id,
            "runtime",
            CAPTAIN,
            message["to"].as_str().unwrap_or(peer),
            &reason,
        )?;
        return Ok(Delivery::Quarantined);
    }
    Ok(if delivered {
        Delivery::Delivered
    } else {
        Delivery::Failed
    })
}

/// Tell the captain that a message was refused, so the refusal is not only
/// recorded where nobody reads it. The notice goes in this home's captain
/// mailbox, once per message id, however many times the refusal recurs.
pub(crate) fn surface_refusal(
    store: &Store,
    id: &str,
    sender: &str,
    from: &str,
    to: &str,
    reason: &str,
) -> Result<()> {
    let reason: String = reason.chars().take(2_000).collect();
    let body = format!("Message {id} from {from} to {to} was refused: {reason}");
    let payload = json!({"refused": {"id": id, "from": from, "to": to, "reason": reason}});
    let now = Store::now_ms();
    let tx = store.write_transaction()?;
    let written = tx.execute(
        "INSERT OR IGNORE INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,delivery,delivered_ms)
         VALUES(?1,?2,?3,'notice',?4,?5,'high',?6,'received',?6)",
        params![
            format!("refusal-{id}"),
            sender,
            CAPTAIN,
            body,
            payload.to_string(),
            now
        ],
    )?;
    tx.commit()?;
    if written == 1 {
        store.audit("captain.refused", &payload)?;
    }
    Ok(())
}

/// Fair delivery across peers, FIFO within each peer. Explicit flushes
/// bypass retry delays, but never retry one failed peer twice in a pass.
fn flush_pending(
    store: &Store,
    max_attempts: i64,
    manual: bool,
    budget: Option<Duration>,
    only_peer: Option<&str>,
) -> Result<Value> {
    // An agent delivers only its own ship's mail; the receiving home would
    // refuse anything else from it.
    let sender = match store.caller() {
        Caller::Agent { actor } => Some(actor.clone()),
        Caller::Operator => None,
    };
    let deadline = budget.map(|budget| Instant::now() + budget);
    // Mail behind a collected peer waits for its machine to take it.
    let collected = json!(crate::peers::collected(store)?).to_string();
    let mut failed = vec![];
    let (mut delivered, mut attempted, mut examined) = (0, 0, 0);
    while attempted < max_attempts && examined < max_attempts + EXTRA_CLASSIFICATIONS {
        let remaining = deadline.map(|end| end.saturating_duration_since(Instant::now()));
        if remaining.is_some_and(|left| left.is_zero()) {
            break;
        }
        let next: Option<(String, String)> = store.connection().query_row(
            "SELECT m.id, json_extract(s.record, '$.location.peer')
             FROM mail m JOIN ships s ON s.name=m.recipient
             LEFT JOIN mail_delivery_retry r ON r.peer=json_extract(s.record, '$.location.peer')
             WHERE m.delivery='pending' AND (?1 IS NULL OR m.sender=?1)
               AND json_extract(s.record, '$.location.peer') IS NOT NULL
               AND (?2 OR COALESCE(r.next_attempt_ms, 0)<=?3)
               AND (?4 IS NULL OR json_extract(s.record, '$.location.peer')=?4)
               AND json_extract(s.record, '$.location.peer') NOT IN (SELECT value FROM json_each(?5))
               AND json_extract(s.record, '$.location.peer') NOT IN (SELECT value FROM json_each(?6))
               AND NOT EXISTS (
                 SELECT 1 FROM mail older JOIN ships os ON os.name=older.recipient
                 WHERE older.delivery='pending' AND older.seq<m.seq
                   AND json_extract(os.record, '$.location.peer')=json_extract(s.record, '$.location.peer'))
             ORDER BY COALESCE(r.last_attempt_ms, 0), m.seq LIMIT 1",
            params![sender, manual, Store::now_ms(), only_peer, json!(failed).to_string(), collected],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?;
        let Some((id, peer)) = next else { break };
        let remaining = deadline.map(|end| end.saturating_duration_since(Instant::now()));
        if remaining.is_some_and(|left| left.is_zero()) {
            break;
        }
        examined += 1;
        match deliver_one(store, &id, &peer, remaining)? {
            Delivery::Delivered => {
                attempted += 1;
                delivered += 1;
            }
            Delivery::Failed => {
                attempted += 1;
                failed.push(peer);
            }
            Delivery::Quarantined => {}
        }
    }
    let (pending, quarantined): (i64, i64) = store.connection().query_row(
        "SELECT COALESCE(SUM(delivery='pending'),0), COALESCE(SUM(delivery='quarantined'),0)
         FROM mail WHERE delivery IN ('pending','quarantined') AND (?1 IS NULL OR sender=?1)",
        [sender],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(
        json!({"delivered": delivered, "pending": pending, "attempted": attempted, "quarantined":quarantined}),
    )
}

/// Pending mail for ships behind a collected peer, oldest first, as the
/// envelopes another machine collects: at most `limit` messages and about
/// `max_bytes` of them.
pub(crate) fn awaiting(
    store: &Store,
    peer: &str,
    limit: usize,
    max_bytes: usize,
) -> Result<Vec<Value>> {
    let mut statement = store.connection().prepare(
        "SELECT m.id FROM mail m JOIN ships s ON s.name=m.recipient
         WHERE m.delivery='pending' AND json_extract(s.record, '$.location.peer')=?1
         ORDER BY m.seq LIMIT ?2",
    )?;
    let ids: Vec<String> = statement
        .query_map(params![peer, limit as i64], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut envelopes = vec![];
    let mut bytes = 0;
    for id in ids {
        let envelope = envelope(&by_id(store, &id)?.context("message vanished")?);
        bytes += envelope.to_string().len();
        if bytes > max_bytes && !envelopes.is_empty() {
            break;
        }
        envelopes.push(envelope);
    }
    Ok(envelopes)
}

/// Record what another machine collected from `peer`: delivered messages,
/// with the request-closure state it applied, and messages it refused, which
/// are quarantined with the reason rather than offered again.
pub(crate) fn collected(
    store: &Store,
    peer: &str,
    delivered: &[(String, Option<String>)],
    refused: &[(String, String)],
) -> Result<usize> {
    let tx = store.write_transaction()?;
    let now = Store::now_ms();
    let behind = "AND recipient IN (SELECT name FROM ships WHERE json_extract(record, '$.location.peer')=?2)";
    let mut changed = 0;
    for (id, end_state) in delivered {
        changed += tx.execute(
            &format!(
                "UPDATE mail SET delivery='delivered', delivered_ms=?3, delivery_attempts=delivery_attempts+1,
                   delivery_error=NULL, remote_end_state=?4 WHERE id=?1 AND delivery='pending' {behind}"
            ),
            params![id, peer, now, end_state],
        )?;
    }
    for (id, error) in refused {
        changed += tx.execute(
            &format!(
                "UPDATE mail SET delivery='quarantined', delivery_attempts=delivery_attempts+1,
                   delivery_error=?3 WHERE id=?1 AND delivery='pending' {behind}"
            ),
            params![id, peer, error.chars().take(4096).collect::<String>()],
        )?;
    }
    tx.commit()?;
    Ok(changed)
}

/// Captain requests from ships here, delivered to the machine behind `peer`,
/// that have since been answered, withdrawn or ended here. Each is offered
/// once: a closure the peer applies keeps its copy from staying open there.
pub(crate) fn closed_captain(store: &Store, peer: &str, limit: usize) -> Result<Vec<Value>> {
    collected_table(store)?;
    let mut statement = store.connection().prepare(
        "SELECT m.id, m.state, m.end_reason, m.answered_by, m.answered_ms, m.ended_by, m.ended_ms
         FROM mail m JOIN ships s ON s.name=m.sender
         JOIN machine_collected c ON c.peer=?1 AND c.id=m.id AND c.error IS NULL
         WHERE m.recipient=?2 AND m.kind IN ('request','decision')
           AND json_extract(s.record,'$.location')='here'
           AND m.state IN ('answered','approved','denied','withdrawn','dismissed','task_done','task_cancelled')
           AND NOT EXISTS (SELECT 1 FROM machine_closed d WHERE d.peer=?1 AND d.id=m.id)
         ORDER BY m.seq LIMIT ?3",
    )?;
    let rows = statement.query_map(params![peer, CAPTAIN, limit as i64], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<i64>>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, Option<i64>>(6)?,
        ))
    })?;
    let mut closures = vec![];
    for row in rows {
        let (id, state, end_reason, answered_by, answered_ms, ended_by, ended_ms) = row?;
        let (reason, by, ms) = match state.as_str() {
            "answered" => (String::new(), answered_by, answered_ms),
            "approved" | "denied" => (state.clone(), answered_by, answered_ms),
            _ => (end_reason.unwrap_or_default(), ended_by, ended_ms),
        };
        let mut closure = json!({"id": id, "state": state, "reason": reason, "by": by, "ms": ms});
        // The answer itself travels with the closure when it was given where
        // the ask lived, so the desk can record it beside its closed copy and
        // the captain's feed there shows what the captain said.
        if matches!(state.as_str(), "answered" | "approved" | "denied")
            && let Some(by) = closure["by"].as_str()
            && let Some(reply) = find(store, by)?
        {
            let reply = envelope(&reply);
            // A stored envelope can exceed the wire limit even when the send
            // arguments fit it. The closure still ends the desk's copy; only
            // the answer is left out, so it cannot strand the copy forever.
            if reply.to_string().len() <= MAX_MESSAGE_BYTES {
                closure["reply"] = reply;
            }
        }
        closures.push(closure);
    }
    Ok(closures)
}

/// Record the closures the machine behind `peer` applied, so none is sent twice.
pub(crate) fn collected_closed(store: &Store, peer: &str, ids: &[String]) -> Result<usize> {
    collected_table(store)?;
    let tx = store.write_transaction()?;
    let mut changed = 0;
    for id in ids {
        changed += tx.execute(
            "INSERT OR IGNORE INTO machine_closed(peer, id, closed_ms) VALUES(?1, ?2, ?3)",
            params![peer, id, Store::now_ms()],
        )?;
    }
    tx.commit()?;
    Ok(changed)
}

/// Check the shape of a closure. A malformed one can never apply, so the desk
/// refuses it for good; nothing else about applying it is permanent.
pub(crate) fn check_closure(closure: &Value) -> Result<()> {
    identifier(required(closure, "id")?)?;
    let state = required(closure, "state")?;
    ensure!(
        matches!(
            state,
            "answered"
                | "approved"
                | "denied"
                | "withdrawn"
                | "dismissed"
                | "task_done"
                | "task_cancelled"
        ),
        "invalid closure state"
    );
    let reason = closure["reason"].as_str().unwrap_or("");
    ensure!(reason.len() <= 2000, "closure reason exceeds 2000 bytes");
    if let Some(reply) = closure.get("reply").filter(|v| !v.is_null()) {
        ensure!(reply.is_object(), "a closure reply must be an object");
        let reply_id = required(reply, "id")?;
        identifier(reply_id)?;
        identifier(required(reply, "to")?)?;
        ensure!(
            reply["from"] == CAPTAIN,
            "a closure reply is from the captain"
        );
        ensure!(reply["kind"] == "reply", "a closure reply is a reply");
        ensure!(
            reply["correlation"].as_str() == Some(closure["id"].as_str().unwrap_or_default()),
            "a closure reply answers the closed item"
        );
        ensure!(
            closure["by"].as_str() == Some(reply_id),
            "a closure reply is the answer the closure records"
        );
    }
    Ok(())
}

/// Record a message in this home as an idle copy: it is never delivered,
/// woken on or counted unread. An existing copy under the same id is left as
/// it is, so a copy that arrived normally is not overwritten.
fn record_copy(conn: &rusqlite::Connection, message: &Value, delivered_ms: i64) -> Result<usize> {
    let payload = if message["payload"].is_object() {
        message["payload"].clone()
    } else {
        json!({})
    };
    Ok(conn.execute(
        "INSERT OR IGNORE INTO mail(id,sender,recipient,kind,correlation,task_id,request_kind,body,payload,urgency,deadline_ms,created_ms,delivery,delivered_ms,because_of)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,'delivered',?13,?14)",
        params![
            required(message, "id")?,
            required(message, "from")?,
            required(message, "to")?,
            required(message, "kind")?,
            message["correlation"].as_str(),
            message["task_id"].as_str(),
            message["request_kind"].as_str(),
            message["body"].as_str().unwrap_or(""),
            payload.to_string(),
            message["urgency"].as_str().unwrap_or("normal"),
            message["deadline_ms"].as_i64(),
            message["created_ms"].as_i64().unwrap_or(delivered_ms),
            delivered_ms,
            message["because_of"].as_str(),
        ],
    )?)
}

/// A captain's reply written in the asking ship's own home here is also
/// recorded in any other fleet home on this machine that holds a copy of the
/// ask (the desk's), and that copy closes too, so the captain's feed there
/// shows the answer. The copy is idle: it wakes nothing and is delivered
/// nowhere. Best effort; the reply has already been recorded and delivered.
pub(crate) fn record_reply_at_desk(store: &Store, reply: &Value) {
    let result = (|| -> Result<()> {
        let (Some(correlation), Some(asker)) =
            (reply["correlation"].as_str(), reply["to"].as_str())
        else {
            return Ok(());
        };
        if reply["from"] != CAPTAIN || reply["kind"] != "reply" {
            return Ok(());
        }
        for (_, home) in crate::block_ask::other_homes(store.home()) {
            let Ok(other) = Store::open(&home) else {
                continue;
            };
            // Read first: most homes on the machine hold no copy of this ask,
            // and a read lock is enough to tell. Take the write lock only in
            // the home that will be changed.
            let holds: bool = other.connection().query_row(
                "SELECT EXISTS(SELECT 1 FROM mail WHERE id=?1 AND sender=?2 AND recipient=?3
                   AND kind IN ('request','decision'))",
                params![correlation, asker, CAPTAIN],
                |r| r.get(0),
            )?;
            if !holds {
                continue;
            }
            let tx = other.write_transaction()?;
            let now = Store::now_ms();
            record_copy(&tx, reply, now)?;
            tx.execute(
                "UPDATE mail SET state='answered', answered_by=?2, answered_ms=?3
                 WHERE id=?1 AND state='open'",
                params![correlation, reply["id"].as_str().unwrap_or_default(), now],
            )?;
            tx.commit()?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("privateer: the captain's reply was not copied to the desk: {error:#}");
    }
}

/// Apply a closure a machine behind `peer` sent for a captain request this
/// home holds from one of that machine's ships: the open copy closes with the
/// same state and end reason, so the captain's waiting drops it. A copy that
/// is already closed is left as it is. Returns whether a copy was closed.
/// The closure must have passed `check_closure`.
pub(crate) fn close_captain_request(store: &Store, peer: &str, closure: &Value) -> Result<bool> {
    let id = required(closure, "id")?;
    let state = required(closure, "state")?;
    let reason = closure["reason"].as_str().unwrap_or("");
    let by = closure["by"].as_str();
    let ms = closure["ms"].as_i64().unwrap_or_else(Store::now_ms);
    let from_peer = "SELECT s.name FROM ships s WHERE json_extract(s.record,'$.location.peer')=?2";
    let tx = store.write_transaction()?;
    let changed = if state == "answered" {
        tx.execute(
            &format!(
                "UPDATE mail SET state='answered', answered_by=?3, answered_ms=?4
                 WHERE id=?1 AND state='open' AND recipient=?5 AND kind IN ('request','decision')
                   AND sender IN ({from_peer})"
            ),
            params![id, peer, by, ms, CAPTAIN],
        )?
    } else if matches!(state, "approved" | "denied") {
        // As approval_answer writes an approval outcome on the machine that granted it.
        tx.execute(
            &format!(
                "UPDATE mail SET state=?3, answered_by=?4, answered_ms=?5, ended_by=?4, ended_ms=?5, end_reason=?3
                 WHERE id=?1 AND state='open' AND recipient=?6 AND kind IN ('request','decision')
                   AND sender IN ({from_peer})"
            ),
            params![id, peer, state, by, ms, CAPTAIN],
        )?
    } else {
        tx.execute(
            &format!(
                "UPDATE mail SET state=?3, ended_by=?4, ended_ms=?5, end_reason=?6
                 WHERE id=?1 AND state='open' AND recipient=?7 AND kind IN ('request','decision')
                   AND sender IN ({from_peer})"
            ),
            params![id, peer, state, by, ms, reason, CAPTAIN],
        )?
    };
    // The answer given where the ask lived travels with the closure: record
    // it beside the desk's copy, once, so the captain's feed here shows it.
    // The home must hold the copy the closure closes, not merely a same id,
    // and that copy's sender must be the ship the reply answers: a linked
    // machine cannot address its reply to some other ship hosted here.
    if let Some(reply) = closure.get("reply").filter(|v| !v.is_null()) {
        let asker = reply["to"].as_str().unwrap_or_default();
        let holds: bool = tx.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM mail
                 WHERE id=?1 AND recipient=?3 AND kind IN ('request','decision')
                   AND sender IN ({from_peer}) AND sender=?4)"
            ),
            params![id, peer, CAPTAIN, asker],
            |r| r.get(0),
        )?;
        // A reply too large for the wire is dropped, but the closure still
        // ends the copy; refusing it would leave the copy open forever.
        if holds && reply.to_string().len() <= MAX_MESSAGE_BYTES {
            record_copy(&tx, reply, ms)?;
        }
    }
    tx.commit()?;
    Ok(changed > 0)
}

fn collected_table(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS machine_collected(peer TEXT NOT NULL, id TEXT NOT NULL,
           collected_ms INTEGER NOT NULL, error TEXT, PRIMARY KEY(peer, id));
         CREATE TABLE IF NOT EXISTS machine_closed(peer TEXT NOT NULL, id TEXT NOT NULL,
           closed_ms INTEGER NOT NULL, PRIMARY KEY(peer, id));",
    )?;
    Ok(())
}

/// Mail from ships here to the captain, written since `since_ms`, that the
/// machine behind `peer` has not collected yet: the captain reads it there.
/// With `open_asks`, the desk push, still-open asks written earlier go too,
/// so an ask from before the desk moved is not stranded. It stays in this
/// home's captain mailbox as well.
pub(crate) fn awaiting_captain(
    store: &Store,
    peer: &str,
    since_ms: i64,
    open_asks: bool,
    limit: usize,
    max_bytes: usize,
) -> Result<Vec<Value>> {
    collected_table(store)?;
    let mut statement = store.connection().prepare(
        "SELECT m.id FROM mail m JOIN ships s ON s.name=m.sender
         WHERE m.recipient=?1 AND json_extract(s.record, '$.location')='here'
           AND (m.created_ms>=?2
                OR (?5 AND m.kind IN ('request','decision') AND m.state='open'))
           AND NOT EXISTS (SELECT 1 FROM machine_collected c WHERE c.peer=?3 AND c.id=m.id)
         ORDER BY m.seq LIMIT ?4",
    )?;
    let ids: Vec<String> = statement
        .query_map(
            params![CAPTAIN, since_ms, peer, limit as i64, open_asks],
            |r| r.get(0),
        )?
        .collect::<rusqlite::Result<_>>()?;
    let mut envelopes = vec![];
    let mut bytes = 0;
    for id in ids {
        let envelope = envelope(&by_id(store, &id)?.context("message vanished")?);
        bytes += envelope.to_string().len();
        if bytes > max_bytes && !envelopes.is_empty() {
            break;
        }
        envelopes.push(envelope);
    }
    Ok(envelopes)
}

/// Record captain mail the machine behind `peer` collected, delivered or
/// refused (with the reason), so it is not offered again.
pub(crate) fn collected_captain(
    store: &Store,
    peer: &str,
    ids: &[(String, Option<String>)],
) -> Result<usize> {
    collected_table(store)?;
    let tx = store.write_transaction()?;
    let mut changed = 0;
    for (id, error) in ids {
        changed += tx.execute(
            "INSERT OR IGNORE INTO machine_collected(peer, id, collected_ms, error)
             SELECT ?1, id, ?3, ?4 FROM mail WHERE id=?2 AND recipient=?5",
            params![peer, id, Store::now_ms(), error, CAPTAIN],
        )?;
    }
    tx.commit()?;
    Ok(changed)
}

fn flush(store: &Store, args: &Value) -> Result<Value> {
    flush_pending(store, limit(args, 50), true, None, None)
}

pub(crate) fn flush_dispatcher(store: &Store, budget: Duration) -> Result<Value> {
    flush_pending(store, 50, false, Some(budget), None)
}

/// Application evidence, including on redelivery after a lost transport ack.
/// Read the stored closure, not an untrusted duplicate envelope's payload.
fn closure_receipt(store: &Store, closure: &Value) -> Result<Value> {
    if !closure["payload"]["request_end"].is_object() {
        return Ok(Value::Null);
    }
    let Some(request) = by_id(
        store,
        closure["correlation"]
            .as_str()
            .context("closure correlation")?,
    )?
    else {
        // Retention may remove an acknowledged closed request before its
        // unread closure notice. A duplicate remains a transport no-op, but
        // missing history cannot supply application evidence.
        return Ok(Value::Null);
    };
    Ok(json!({"id":request["id"],"state":request["state"],"ended_by":request["ended_by"]}))
}

/// Accept mail a peer delivers for a ship hosted here. The sender must be a
/// ship this home knows to live behind a peer; replies close this home's copy
/// of the request they answer.
fn deliver(store: &Store, args: &Value) -> Result<Value> {
    ensure!(
        args.to_string().len() <= MAX_MESSAGE_BYTES,
        "message exceeds 64 KiB"
    );
    let id = required(args, "id")?;
    identifier(id)?;
    let from = required(args, "from")?;
    let to = required(args, "to")?;
    let kind = required(args, "kind")?;
    ensure!(KINDS.contains(&kind), "unknown message kind {kind}");
    ensure!(
        from != CREW && to != CREW,
        "crew reports stay in their own home"
    );
    ensure!(
        kind != "decision" || to == CAPTAIN,
        "decisions are addressed to the captain"
    );
    // The captain's mail crosses only a link between machines, which the
    // operator made, and arrives only from the operator's side of it: the
    // captain's own words from the door, or a ship's mail to the captain
    // collected from the other machine.
    if from == CAPTAIN || to == CAPTAIN {
        ensure!(
            matches!(store.caller(), Caller::Operator),
            "the captain's mail stays in its own home unless the operator carries it across machines"
        );
    }
    if from != CAPTAIN {
        let sender = ship(store, from)?.with_context(|| format!("unknown ship {from}"))?;
        let peer = remote_peer(&sender)
            .with_context(|| format!("{from} is hosted here; local mail is sent with mail.send"))?;
        ensure!(
            to != CAPTAIN || crate::peers::machine(store, peer)?,
            "the captain's mail stays in its own home for now"
        );
    }
    local_address(store, to)?;
    let correlation = optional_str(args, "correlation")?;
    // A follow-up keeps its link where it was raised: the sender's home is
    // where its wake happens, and the other home only reads the message.
    let because_of = optional_str(args, "because_of")?;
    if let Some(parent) = because_of {
        identifier(parent)?;
        ensure!(
            kind == "request",
            "because_of is the request a follow-up asks about; only a request carries it"
        );
    }
    let request_kind = optional_str(args, "request_kind")?;
    let body = args.get("body").and_then(Value::as_str).unwrap_or("");
    let mut payload = args.get("payload").cloned().unwrap_or(json!({}));
    ensure!(payload.is_object(), "payload must be an object");
    typed_payload(kind, &mut payload)?;
    hands_step(kind, to, &payload)?;
    // Files cross a link only on the captain's mail to a ship, and this home,
    // which hosts the ship, keeps only what its own rows say about them. A
    // redelivery of mail already stored was checked when it was placed.
    if payload.get("files").is_some() && by_id(store, id)?.is_none() {
        ensure!(
            from == CAPTAIN,
            "payload.files is the captain's, on mail to a ship"
        );
        crate::files::check_delivered(store, to, &mut payload)?;
    }
    // The operator marker crosses a link only on a captain reply that the
    // machine holding this machine's captain desk delivered.
    let via = optional_str(args, "via")?;
    if payload.get("operator").is_some()
        && !(from == CAPTAIN && kind == "reply" && via.is_some_and(crate::machines::holds_desk))
        && let Some(fields) = payload.as_object_mut()
    {
        fields.remove("operator");
    }
    let urgency = optional_str(args, "urgency")?.unwrap_or("normal");
    ensure!(URGENCIES.contains(&urgency), "invalid urgency");
    let deadline_ms = args.get("deadline_ms").and_then(Value::as_i64);
    let tx = store.write_transaction()?;
    // A desk copy is attached here, against the asks this machine holds.
    if to == CAPTAIN && kind != "reply" && from != CAPTAIN && by_id(store, id)?.is_none() {
        if let Some(fields) = payload.as_object_mut() {
            fields.remove("attached_to");
            fields.remove("attached_ship");
        }
        let topic = match payload["about"].as_str() {
            Some(about) => Some(about.to_owned()),
            None => crate::block_ask::fleet_key_primary(store, from, &payload)?,
        };
        if let Some(topic) = topic
            && request_kind != Some("production_approval")
            && payload.get("needs_hands").is_none()
            && let Some((ask, ship)) = crate::block_ask::open_primary(store, &topic)?
            && ask != id
        {
            payload["attached_to"] = json!(ask);
            payload["attached_ship"] = json!(ship);
        }
    }
    if let Some(existing) = by_id(store, id)? {
        // Redelivery after a lost acknowledgement is a no-op.
        ensure!(
            existing["from"] == from && existing["to"] == to && existing["kind"] == kind,
            "message id already holds a different message"
        );
        let receipt = closure_receipt(store, &existing)?;
        tx.commit()?;
        return Ok(json!({"id": id, "duplicate": true, "request_end": receipt}));
    }
    // A ship's reply or closure collected for the captain may answer a
    // request asked on the sending machine, which closes it there; no home on
    // the collecting machine holds it. The captain still reads the message.
    let elsewhere = |request: Option<&str>| -> Result<bool> {
        Ok(to == CAPTAIN
            && args["unmatched"] == true
            && match request {
                Some(request) => by_id(store, request)?.is_none(),
                None => false,
            })
    };
    let answers_here = kind == "reply" && !elsewhere(correlation)?;
    if answers_here {
        let correlation = correlation.context("a reply names the request it answers")?;
        let request =
            by_id(store, correlation)?.with_context(|| format!("unknown request {correlation}"))?;
        ensure!(
            request["from"] == to && request["to"] == from,
            "only the recipient of {correlation} can answer it"
        );
        ensure!(
            matches!(
                request["state"].as_str(),
                Some("open" | "withdrawn" | "task_done" | "task_cancelled")
            ),
            "{correlation} was already answered"
        );
        // Only an answer to a typed decision is checked against its options.
        if let (Some(answer), Some(decision)) = (payload.get("answer"), typed_decision(&request)) {
            check_answer(decision, answer)?;
        }
    }
    let now = Store::now_ms();
    tx.execute(
        "INSERT INTO mail(id,sender,recipient,kind,correlation,request_kind,body,payload,urgency,deadline_ms,created_ms,state,delivery,delivered_ms,because_of)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,'received',?11,?13)",
        params![
            id,
            from,
            to,
            kind,
            correlation,
            request_kind,
            body,
            payload.to_string(),
            urgency,
            deadline_ms,
            now,
            matches!(kind, "request" | "decision").then_some("open"),
            because_of
        ],
    )?;
    if answers_here {
        tx.execute(
            "UPDATE mail SET state='answered', answered_by=?2, answered_ms=?3 WHERE id=?1",
            params![correlation, id, now],
        )?;
    }
    if let Some(end) = payload.get("request_end")
        && !elsewhere(correlation)?
    {
        ensure!(
            kind == "notice" && end["version"] == 1,
            "invalid request closure"
        );
        let request_id = correlation.context("closure requires correlation")?;
        let request = by_id(store, request_id)?.context("unknown closed request")?;
        let reason = end
            .get("reason")
            .and_then(Value::as_str)
            .context("closure reason must be a string")?;
        ensure!(reason.len() <= 2000, "closure reason exceeds 2000 bytes");
        let state = required(end, "state")?;
        ensure!(
            matches!(request["kind"].as_str(), Some("request" | "decision")),
            "only requests and decisions are closed"
        );
        // The captain's desk on another machine dismisses a request to the
        // captain; only its sender ends it any other way.
        if state == "dismissed" {
            ensure!(
                from == CAPTAIN && request["from"] == to && request["to"] == CAPTAIN,
                "only the captain dismisses a request to the captain"
            );
        } else {
            ensure!(
                request["from"] == from && request["to"] == to,
                "only the sender can close its request"
            );
            ensure!(
                matches!(state, "withdrawn" | "task_done" | "task_cancelled"),
                "invalid closure state"
            );
        }
        tx.execute("UPDATE mail SET state=?2,ended_by=?3,ended_ms=?4,end_reason=?5 WHERE id=?1 AND state='open'", params![request_id,state,id,now,reason])?;
    }
    let receipt = closure_receipt(
        store,
        &by_id(store, id)?.context("delivered mail disappeared")?,
    )?;
    tx.commit()?;
    if answers_here && let Some(correlation) = correlation {
        let stored = by_id(store, id)?.context("delivered mail disappeared")?;
        crate::block_ask::answered(store, correlation, &stored, false);
    }
    Ok(json!({"id": id, "duplicate": false, "request_end": receipt}))
}

fn cursor(store: &Store, address: &str) -> Result<i64> {
    Ok(store
        .connection()
        .query_row(
            "SELECT through_seq FROM mail_cursor WHERE address=?1",
            [address],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

fn limit(args: &Value, default: i64) -> i64 {
    args.get("limit")
        .and_then(Value::as_i64)
        .unwrap_or(default)
        .clamp(1, 1000)
}

fn inbox(store: &Store, args: &Value) -> Result<Value> {
    let address = required(args, "address")?;
    local_address(store, address)?;
    let acknowledged = cursor(store, address)?;
    // By default only unacknowledged mail; after_seq pages through history.
    let (after, unread) = match args.get("after_seq").and_then(Value::as_i64) {
        Some(after) => (after.max(0), false),
        None if args.get("all") == Some(&json!(true)) => (0, false),
        None => (acknowledged, true),
    };
    // A notice the captain dismissed one by one is read, though the cursor is behind it.
    let dismissed = if unread {
        " AND COALESCE(state,'') <> 'acknowledged'"
    } else {
        ""
    };
    let now = Store::now_ms();
    let mut statement = store.connection().prepare(&format!(
        "SELECT {COLUMNS} FROM mail WHERE recipient=?1 AND seq>?2{dismissed} ORDER BY seq LIMIT ?3"
    ))?;
    let messages = statement
        .query_map(params![address, after, limit(args, 40)], message)?
        .map(|m| Ok(annotate(m?, now)))
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({"address": address, "acknowledged_through": acknowledged, "messages": messages}))
}

/// A bounded notification snapshot, not a read/acknowledgement of message
/// bodies. One native wake can point at an entire arrival burst.
fn pending(store: &Store, args: &Value) -> Result<Value> {
    let address = required(args, "address")?;
    local_address(store, address)?;
    let acknowledged = cursor(store, address)?;
    let after = args
        .get("after_seq")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(acknowledged)
        .max(0);
    let (count, through, actionable): (i64,i64,i64) = store.connection().query_row(
        &format!(
            "SELECT COUNT(*), COALESCE(MAX(seq), ?2), COALESCE(SUM(CASE WHEN {ROUTINE_MAIL} THEN 0 ELSE 1 END),0)
             FROM mail WHERE recipient=?1 AND seq>?2 AND COALESCE(delivery,'received')='received'"
        ),
        params![address,after], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    let mut statement=store.connection().prepare(
        "SELECT seq,id,sender,kind,urgency,task_id FROM mail WHERE recipient=?1 AND seq>?2 AND seq<=?3
         AND COALESCE(delivery,'received')='received'
         ORDER BY seq DESC LIMIT 6")?;
    let headers=statement.query_map(params![address,after,through], |r| Ok(json!({
        "seq":r.get::<_,i64>(0)?,"id":r.get::<_,String>(1)?,"from":r.get::<_,String>(2)?,
        "kind":r.get::<_,String>(3)?,"urgency":r.get::<_,String>(4)?,"task_id":r.get::<_,Option<String>>(5)?
    })))?.collect::<rusqlite::Result<Vec<_>>>()?;
    // The current one-line reading travels with the notice so a hosted first
    // mate acts on the measured value, not a stale belief.
    let attention = crate::attention::summary_for(store, address)
        .ok()
        .and_then(|summary| crate::attention::line(&summary));
    Ok(
        json!({"address":address,"after_seq":after,"acknowledged_through":acknowledged,
        "count":count,"through_seq":through,"actionable":actionable,"headers":headers,
        "attention":attention}),
    )
}

fn ack(store: &Store, args: &Value) -> Result<Value> {
    let address = required(args, "address")?;
    local_address(store, address)?;
    let through = args
        .get("through_seq")
        .and_then(Value::as_i64)
        .filter(|v| *v > 0)
        .context("positive through_seq is required")?;
    let tx = store.write_transaction()?;
    // Acknowledge only what was observed: the sequence must be this mailbox's.
    let observed: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM mail WHERE seq=?1 AND recipient=?2)",
        params![through, address],
        |r| r.get(0),
    )?;
    ensure!(observed, "ack must name a message in this mailbox");
    tx.execute(
        "INSERT INTO mail_cursor(address, through_seq) VALUES(?1, ?2)
         ON CONFLICT(address) DO UPDATE SET through_seq=MAX(through_seq, excluded.through_seq)",
        params![address, through],
    )?;
    tx.commit()?;
    Ok(json!({"address": address, "acknowledged_through": cursor(store, address)?}))
}

fn show(store: &Store, args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    let message = by_id(store, id)?.with_context(|| format!("unknown message {id}"))?;
    if let Caller::Agent { actor } = store.caller() {
        ensure!(
            message["from"] == actor.as_str() || message["to"] == actor.as_str(),
            "agent {actor} can read only its own mail"
        );
    }
    Ok(annotate(message, Store::now_ms()))
}

/// Outgoing headers are discoverable without loading large legacy bodies.
/// Agents see only their own outgoing mail, including when no filter is set.
fn outbox(store: &Store, args: &Value) -> Result<Value> {
    let requested = optional_str(args, "from")?;
    let sender = match store.caller() {
        Caller::Agent { actor } => {
            ensure!(
                requested.is_none_or(|from| from == actor),
                "agents read only their own outbox"
            );
            Some(actor.as_str())
        }
        Caller::Operator => requested,
    };
    let delivery = optional_str(args, "delivery")?;
    ensure!(
        delivery.is_none_or(|d| matches!(d, "pending" | "quarantined" | "delivered")),
        "delivery must be pending, quarantined or delivered"
    );
    let after = args
        .get("after_seq")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    let count = limit(args, 40).min(100) as usize;
    let mut statement = store.connection().prepare(
        "SELECT seq,id,sender,recipient,kind,correlation,task_id,request_kind,urgency,
           deadline_ms,created_ms,state,delivery,delivery_attempts,substr(delivery_error,1,1024),delivered_ms
         FROM mail WHERE delivery IN ('pending','quarantined','delivered')
           AND (?1 IS NULL OR sender=?1) AND (?2 IS NULL OR delivery=?2) AND seq>?3
         ORDER BY seq LIMIT ?4",
    )?;
    let mut messages: Vec<Value> = statement
        .query_map(params![sender, delivery, after, count + 1], |r| {
            Ok(json!({"seq":r.get::<_,i64>(0)?,"id":r.get::<_,String>(1)?,
            "from":r.get::<_,String>(2)?,"to":r.get::<_,String>(3)?,"kind":r.get::<_,String>(4)?,
            "correlation":r.get::<_,Option<String>>(5)?,"task_id":r.get::<_,Option<String>>(6)?,
            "request_kind":r.get::<_,Option<String>>(7)?,"urgency":r.get::<_,String>(8)?,
            "deadline_ms":r.get::<_,Option<i64>>(9)?,"created_ms":r.get::<_,i64>(10)?,
            "state":r.get::<_,Option<String>>(11)?,"delivery":r.get::<_,String>(12)?,
            "delivery_attempts":r.get::<_,i64>(13)?,"delivery_error":r.get::<_,Option<String>>(14)?,
            "delivered_ms":r.get::<_,Option<i64>>(15)?}))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let more = messages.len() > count;
    messages.truncate(count);
    let next = more.then(|| messages.last().unwrap()["seq"].clone());
    Ok(json!({"messages":messages,"next_after_seq":next}))
}

fn requests(store: &Store, args: &Value) -> Result<Value> {
    let address = required(args, "address")?;
    local_address(store, address)?;
    let column = match optional_str(args, "direction")?.unwrap_or("incoming") {
        "incoming" => "recipient",
        "outgoing" => "sender",
        _ => bail!("direction must be incoming or outgoing"),
    };
    let state = optional_str(args, "state")?;
    ensure!(
        state.is_none_or(|s| matches!(
            s,
            "open"
                | "answered"
                | "overdue"
                | "withdrawn"
                | "task_done"
                | "task_cancelled"
                | "approved"
                | "denied"
        )),
        "invalid request state"
    );
    let now = Store::now_ms();
    let mut statement = store.connection().prepare(&format!(
        "SELECT {COLUMNS} FROM mail WHERE {column}=?1 AND kind IN ('request','decision')
         AND (?2 IS NULL OR state=?2) ORDER BY seq LIMIT ?3"
    ))?;
    let stored_state = state.map(|s| if s == "overdue" { "open" } else { s });
    let found = statement
        .query_map(params![address, stored_state, limit(args, 100)], message)?
        .map(|m| Ok(annotate(m?, now)))
        .collect::<Result<Vec<_>>>()?;
    let found: Vec<Value> = found
        .into_iter()
        .filter(|m| state != Some("overdue") || m["overdue"] == true)
        .collect();
    Ok(json!({"address": address, "requests": found}))
}

/// Counts for `status`: unacknowledged mail and requests still owed a reply.
pub fn summary(store: &Store) -> Result<Value> {
    let unread: i64 = store.connection().query_row(
        "SELECT COUNT(*) FROM mail m LEFT JOIN mail_cursor c ON c.address=m.recipient
         WHERE COALESCE(m.delivery, 'received')='received' AND m.seq > COALESCE(c.through_seq, 0)
           AND m.recipient <> 'crew'",
        [],
        |r| r.get(0),
    )?;
    let open: i64 =
        store
            .connection()
            .query_row("SELECT COUNT(*) FROM mail WHERE state='open'", [], |r| {
                r.get(0)
            })?;
    let (undelivered, quarantined): (i64, i64) = store.connection().query_row(
        "SELECT COUNT(*), COALESCE(SUM(delivery='quarantined'),0) FROM mail
         WHERE delivery IN ('pending','quarantined')",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(
        json!({"unacknowledged_mail": unread, "open_requests": open, "undelivered_mail": undelivered,
            "quarantined_mail":quarantined}),
    )
}

/// Retention for `history.prune`: acknowledged mail older than the cutoff,
/// never unread mail or a request still owed its reply.
pub fn prune(store: &Store, cutoff_ms: i64, dry_run: bool) -> Result<i64> {
    // Mail held here counts once acknowledged; an outgoing copy once delivered.
    // Replies to crew have no mailbox to acknowledge them.
    let stale = "created_ms < ?1 AND COALESCE(delivery,'received')<>'quarantined'
        AND COALESCE(state, '') <> 'open' AND (delivery='delivered'
        OR recipient='crew'
        OR (COALESCE(delivery, 'received')='received'
            AND seq <= COALESCE((SELECT through_seq FROM mail_cursor WHERE address=mail.recipient), 0)))";
    let count: i64 = store.connection().query_row(
        &format!("SELECT COUNT(*) FROM mail WHERE {stale}"),
        [cutoff_ms],
        |r| r.get(0),
    )?;
    if !dry_run {
        store
            .connection()
            .execute(&format!("DELETE FROM mail WHERE {stale}"), [cutoff_ms])?;
    }
    Ok(count)
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "ship.put" => ship_put(store, &args),
        "ship.show" => {
            let name = required(&args, "name")?;
            ship(store, name)?.with_context(|| format!("unknown ship {name}"))
        }
        "ship.list" => {
            let mut statement = store
                .connection()
                .prepare("SELECT record FROM ships ORDER BY name")?;
            let ships = statement
                .query_map([], |r| r.get::<_, String>(0))?
                .map(|raw| Ok(serde_json::from_str::<Value>(&raw?)?))
                .collect::<Result<Vec<_>>>()?;
            Ok(json!({"ships": ships}))
        }
        "mail.send" => send(store, &args),
        "mail.withdraw" => withdraw(store, &args),
        "mail.inbox" => inbox(store, &args),
        "mail.outbox" => outbox(store, &args),
        "mail.pending" => pending(store, &args),
        "mail.ack" => ack(store, &args),
        "mail.show" => show(store, &args),
        "mail.requests" => requests(store, &args),
        "mail.deliver" => deliver(store, &args),
        "mail.flush" => flush(store, &args),
        _ => bail!("unknown mailbox command {command}"),
    }
}

#[cfg(test)]
mod hands_tests {
    use super::*;

    #[test]
    fn only_a_bounded_step_on_a_request_to_the_captain_marks_hands_on() {
        let marked = |step: Value| json!({"needs_hands": {"step": step}});
        assert_eq!(
            hands_step("request", CAPTAIN, &marked(json!("Run it"))).unwrap(),
            Some("Run it".to_owned())
        );
        assert_eq!(hands_step("request", CAPTAIN, &json!({})).unwrap(), None);
        // Every other kind, recipient, and malformed step is refused.
        assert!(hands_step("decision", CAPTAIN, &marked(json!("Run it"))).is_err());
        assert!(hands_step("reply", CAPTAIN, &marked(json!("Run it"))).is_err());
        assert!(hands_step("notice", CAPTAIN, &marked(json!("Run it"))).is_err());
        assert!(hands_step("request", "apps", &marked(json!("Run it"))).is_err());
        for step in [json!(""), json!("   "), json!(null), json!(3), json!(["x"])] {
            assert!(
                hands_step("request", CAPTAIN, &marked(step.clone())).is_err(),
                "{step}"
            );
        }
        let long = "x".repeat(MAX_HANDS_STEP_BYTES + 1);
        assert!(hands_step("request", CAPTAIN, &marked(json!(long))).is_err());
        assert!(hands_step("request", CAPTAIN, &json!({"needs_hands": null})).is_err());
        assert!(hands_step("request", CAPTAIN, &json!({"needs_hands": "run it"})).is_err());
    }
}
