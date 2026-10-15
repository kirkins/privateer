//! Ship directory and typed mailboxes.
//!
//! A ship is an identity with its own mailbox, first mate and crew. A home
//! hosts one or more ships; the directory records where each ship lives.
//! `captain` is the reserved address of the person. Messages are durable,
//! idempotent and typed: a `request` or `decision` stays open until its single
//! `reply`, so the runtime, not an agent's memory, tracks owed answers.
use crate::store::{Caller, Store, identifier, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, Row, params};
use serde_json::{Value, json};

pub const CAPTAIN: &str = "captain";
/// The runtime's voice for a ship's crew: the outcomes of its jobs.
/// Only the runtime writes as it (see `crew`); nobody can send as it.
pub const CREW: &str = "crew";
const KINDS: &[&str] = &["request", "reply", "notice", "decision"];
const URGENCIES: &[&str] = &["low", "normal", "high"];
const MAX_MESSAGE_BYTES: usize = 65_536;
const MAX_SCOPE_CHARS: usize = 2_000;

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
         CREATE TABLE IF NOT EXISTS mail_cursor(address TEXT PRIMARY KEY, through_seq INTEGER NOT NULL);",
    )?;
    // Delivery state for mail crossing homes: NULL for mail that stays in
    // this home, 'pending' or 'delivered' for outgoing copies, 'received' for
    // mail delivered here by a peer.
    let tx = store.write_transaction()?;
    for (column, declaration) in [
        ("delivery", "TEXT"),
        ("delivery_attempts", "INTEGER NOT NULL DEFAULT 0"),
        ("delivery_error", "TEXT"),
        ("delivered_ms", "INTEGER"),
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
        "CREATE INDEX IF NOT EXISTS mail_pending ON mail(delivery, seq) WHERE delivery='pending';",
    )?;
    tx.commit()?;
    Ok(())
}

/// Where a ship's mailbox lives: None for this home, Some(peer) for another.
fn remote_peer(ship: &Value) -> Option<&str> {
    ship["location"]["peer"].as_str()
}

fn ship(store: &Store, name: &str) -> Result<Option<Value>> {
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
    ensure!(address != CREW, "{CREW} has no mailbox");
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
    Ok(record)
}

/// Write a message from `crew` to a ship hosted here, inside the caller's
/// transaction so it commits with the fact it reports. `message` holds id,
/// to, kind (notice or request), task_id, body, payload and urgency. The id
/// is the idempotency key: an existing message under it is left as it is,
/// and false is returned.
pub(crate) fn record_crew(conn: &rusqlite::Connection, message: &Value) -> Result<bool> {
    let text = |key: &str| message[key].as_str().context("invalid crew message");
    let (kind, urgency) = (text("kind")?, text("urgency")?);
    ensure!(
        matches!(kind, "notice" | "request") && URGENCIES.contains(&urgency),
        "invalid crew message"
    );
    let written = conn.execute(
        "INSERT INTO mail(id,sender,recipient,kind,task_id,body,payload,urgency,created_ms,state)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT(id) DO NOTHING",
        params![
            text("id")?,
            CREW,
            text("to")?,
            kind,
            text("task_id")?,
            text("body")?,
            message["payload"].to_string(),
            urgency,
            Store::now_ms(),
            (kind == "request").then_some("open")
        ],
    )?;
    Ok(written == 1)
}

const COLUMNS: &str = "seq,id,sender,recipient,kind,correlation,task_id,request_kind,body,payload,urgency,deadline_ms,created_ms,state,answered_by,answered_ms";

fn message(r: &Row) -> rusqlite::Result<Value> {
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

fn send(store: &Store, args: &Value) -> Result<Value> {
    ensure!(
        args.to_string().len() <= MAX_MESSAGE_BYTES,
        "message exceeds 64 KiB"
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
    ensure!(to != CREW, "{CREW} has no mailbox");
    local_address(store, from)?;
    let recipient = if to == CAPTAIN {
        None
    } else {
        ship(store, to)?
    };
    ensure!(
        to == CAPTAIN || recipient.is_some(),
        "unknown ship {to}; add it with ship.put"
    );
    ensure!(from != to, "a mailbox cannot write to itself");
    let peer = recipient.as_ref().and_then(remote_peer).map(str::to_owned);
    if peer.is_some() {
        // The captain's mailbox is per home until the captain's front door.
        ensure!(
            from != CAPTAIN,
            "the captain's mail stays in this home for now"
        );
    }
    let id = optional_str(args, "id")?
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    identifier(&id)?;
    let body = args.get("body").and_then(Value::as_str).unwrap_or("");
    let payload = args.get("payload").cloned().unwrap_or(json!({}));
    ensure!(payload.is_object(), "payload must be an object");
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
        store.task(task)?;
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

    let tx = store.write_transaction()?;
    if let Some(existing) = by_id(store, &id)? {
        let same = existing["from"] == from
            && existing["to"] == to
            && existing["kind"] == kind
            && existing["correlation"] == json!(correlation)
            && existing["task_id"] == json!(task_id)
            && existing["request_kind"] == json!(request_kind)
            && existing["body"] == body
            && existing["payload"] == payload;
        ensure!(same, "message id already holds a different message");
        tx.commit()?;
        let mut existing = annotate(existing, Store::now_ms());
        existing["duplicate"] = json!(true);
        return Ok(existing);
    }
    if let Some(correlation) = correlation {
        let thread = by_id(store, correlation)?
            .with_context(|| format!("unknown correlated message {correlation}"))?;
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
        }
    }
    let now = Store::now_ms();
    tx.execute(
        "INSERT INTO mail(id,sender,recipient,kind,correlation,task_id,request_kind,body,payload,urgency,deadline_ms,created_ms,state,delivery)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
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
            peer.as_ref().map(|_| "pending")
        ],
    )?;
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
        deliver_one(store, &id, &peer)?;
    }
    Ok(annotate(
        by_id(store, &id)?.context("message vanished")?,
        Store::now_ms(),
    ))
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
    ] {
        envelope[key] = message[key].clone();
    }
    envelope
}

/// Try once to deliver a pending outgoing message; record the outcome.
fn deliver_one(store: &Store, id: &str, peer: &str) -> Result<bool> {
    let message = by_id(store, id)?.with_context(|| format!("unknown message {id}"))?;
    let outcome = crate::peers::call_peer(
        store,
        peer,
        &format!("deliver-{id}"),
        "mail.deliver",
        envelope(&message),
    )
    .and_then(|response| match response["ok"].as_bool() {
        Some(true) => Ok(()),
        _ => bail!(
            "{}",
            response["error"]
                .as_str()
                .unwrap_or("peer refused delivery")
        ),
    });
    let delivered = outcome.is_ok();
    store.connection().execute(
        "UPDATE mail SET delivery=CASE WHEN ?2 THEN 'delivered' ELSE delivery END,
           delivered_ms=CASE WHEN ?2 THEN ?3 ELSE delivered_ms END,
           delivery_attempts=delivery_attempts+1, delivery_error=?4
         WHERE id=?1 AND delivery='pending'",
        params![
            id,
            delivered,
            Store::now_ms(),
            outcome.err().map(|e| format!("{e:#}"))
        ],
    )?;
    Ok(delivered)
}

/// Retry delivery of pending outgoing mail, oldest first.
fn flush(store: &Store, args: &Value) -> Result<Value> {
    // An agent delivers only its own ship's mail; the receiving home would
    // refuse anything else from it.
    let sender = match store.caller() {
        Caller::Agent { actor } => Some(actor.clone()),
        Caller::Operator => None,
    };
    let mut statement = store.connection().prepare(
        "SELECT id, recipient FROM mail WHERE delivery='pending' AND (?1 IS NULL OR sender=?1)
         ORDER BY seq LIMIT ?2",
    )?;
    let pending: Vec<(String, String)> = statement
        .query_map(params![sender, limit(args, 50)], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let (mut delivered, mut still_pending) = (0, 0);
    for (id, recipient) in pending {
        let peer = ship(store, &recipient)?
            .as_ref()
            .and_then(remote_peer)
            .map(str::to_owned);
        let sent = match peer {
            Some(peer) => deliver_one(store, &id, &peer)?,
            None => false,
        };
        if sent {
            delivered += 1;
        } else {
            still_pending += 1;
        }
    }
    Ok(json!({"delivered": delivered, "pending": still_pending}))
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
        kind != "decision" && from != CAPTAIN && to != CAPTAIN,
        "the captain's mail stays in its own home for now"
    );
    ensure!(
        from != CREW && to != CREW,
        "crew reports stay in their own home"
    );
    let sender = ship(store, from)?.with_context(|| format!("unknown ship {from}"))?;
    ensure!(
        remote_peer(&sender).is_some(),
        "{from} is hosted here; local mail is sent with mail.send"
    );
    local_address(store, to)?;
    let correlation = optional_str(args, "correlation")?;
    let request_kind = optional_str(args, "request_kind")?;
    let body = args.get("body").and_then(Value::as_str).unwrap_or("");
    let payload = args.get("payload").cloned().unwrap_or(json!({}));
    ensure!(payload.is_object(), "payload must be an object");
    let urgency = optional_str(args, "urgency")?.unwrap_or("normal");
    ensure!(URGENCIES.contains(&urgency), "invalid urgency");
    let deadline_ms = args.get("deadline_ms").and_then(Value::as_i64);
    let tx = store.write_transaction()?;
    if let Some(existing) = by_id(store, id)? {
        // Redelivery after a lost acknowledgement is a no-op.
        ensure!(
            existing["from"] == from && existing["to"] == to && existing["kind"] == kind,
            "message id already holds a different message"
        );
        tx.commit()?;
        return Ok(json!({"id": id, "duplicate": true}));
    }
    if kind == "reply" {
        let correlation = correlation.context("a reply names the request it answers")?;
        let request =
            by_id(store, correlation)?.with_context(|| format!("unknown request {correlation}"))?;
        ensure!(
            request["from"] == to && request["to"] == from,
            "only the recipient of {correlation} can answer it"
        );
        ensure!(
            request["state"] == "open",
            "{correlation} was already answered"
        );
    }
    let now = Store::now_ms();
    tx.execute(
        "INSERT INTO mail(id,sender,recipient,kind,correlation,request_kind,body,payload,urgency,deadline_ms,created_ms,state,delivery,delivered_ms)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,'received',?11)",
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
            (kind == "request").then_some("open")
        ],
    )?;
    if kind == "reply" {
        tx.execute(
            "UPDATE mail SET state='answered', answered_by=?2, answered_ms=?3 WHERE id=?1",
            params![correlation, id, now],
        )?;
    }
    tx.commit()?;
    Ok(json!({"id": id, "duplicate": false}))
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
    let after = match args.get("after_seq").and_then(Value::as_i64) {
        Some(after) => after.max(0),
        None if args.get("all") == Some(&json!(true)) => 0,
        None => acknowledged,
    };
    let now = Store::now_ms();
    let mut statement = store.connection().prepare(&format!(
        "SELECT {COLUMNS} FROM mail WHERE recipient=?1 AND seq>?2 ORDER BY seq LIMIT ?3"
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
        "SELECT COUNT(*), COALESCE(MAX(seq), ?2), COALESCE(SUM(CASE WHEN kind <> 'notice' OR urgency <> 'low' THEN 1 ELSE 0 END),0)
         FROM mail WHERE recipient=?1 AND seq>?2 AND COALESCE(delivery,'received')='received'",
        params![address,after], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    let mut statement=store.connection().prepare(
        "SELECT seq,id,sender,kind,urgency,task_id FROM mail WHERE recipient=?1 AND seq>?2 AND seq<=?3
         AND COALESCE(delivery,'received')='received'
         ORDER BY seq DESC LIMIT 6")?;
    let headers=statement.query_map(params![address,after,through], |r| Ok(json!({
        "seq":r.get::<_,i64>(0)?,"id":r.get::<_,String>(1)?,"from":r.get::<_,String>(2)?,
        "kind":r.get::<_,String>(3)?,"urgency":r.get::<_,String>(4)?,"task_id":r.get::<_,Option<String>>(5)?
    })))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(
        json!({"address":address,"after_seq":after,"acknowledged_through":acknowledged,
        "count":count,"through_seq":through,"actionable":actionable,"headers":headers}),
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
        state.is_none_or(|s| matches!(s, "open" | "answered" | "overdue")),
        "state must be open, answered or overdue"
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
         WHERE COALESCE(m.delivery, 'received')='received' AND m.seq > COALESCE(c.through_seq, 0)",
        [],
        |r| r.get(0),
    )?;
    let open: i64 =
        store
            .connection()
            .query_row("SELECT COUNT(*) FROM mail WHERE state='open'", [], |r| {
                r.get(0)
            })?;
    let undelivered: i64 = store.connection().query_row(
        "SELECT COUNT(*) FROM mail WHERE delivery='pending'",
        [],
        |r| r.get(0),
    )?;
    Ok(
        json!({"unacknowledged_mail": unread, "open_requests": open, "undelivered_mail": undelivered}),
    )
}

/// Retention for `history.prune`: acknowledged mail older than the cutoff,
/// never unread mail or a request still owed its reply.
pub fn prune(store: &Store, cutoff_ms: i64, dry_run: bool) -> Result<i64> {
    // Mail held here counts once acknowledged; an outgoing copy once delivered.
    let stale = "created_ms < ?1 AND COALESCE(state, '') <> 'open' AND (delivery='delivered'
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
        "mail.inbox" => inbox(store, &args),
        "mail.pending" => pending(store, &args),
        "mail.ack" => ack(store, &args),
        "mail.show" => show(store, &args),
        "mail.requests" => requests(store, &args),
        "mail.deliver" => deliver(store, &args),
        "mail.flush" => flush(store, &args),
        _ => bail!("unknown mailbox command {command}"),
    }
}
