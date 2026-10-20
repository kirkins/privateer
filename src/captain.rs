//! The captain's desk: what waits on the captain across the fleet, and the
//! liaison relaying the captain's answers to it.
//!
//! A ship's request to the captain lives in that ship's home, and only a
//! reply from the captain closes it. The captain often answers through the
//! liaison instead (from a phone, in conversation), which left those requests
//! open forever. The liaison acts for the captain, so it may see what waits on
//! the captain and record an answer the captain gave it, but only by citing the
//! captain's own message, which the reply keeps. Production approvals are a
//! separate operator grant and are never relayed.
//!
//! The list carries each request's age, so a request read hours after it was
//! asked is not restated to the captain as if it had just arrived.
use crate::store::{Caller, Store, identifier, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::path::PathBuf;

const MAX_WAITING: usize = 100;
/// How old a captain's message a relayed answer may cite.
const SOURCE_MAX_AGE_MS: i64 = 24 * 60 * 60 * 1000;
const QUOTE_CHARS: usize = 2_000;

/// Active fleet ships whose folders still exist, with their homes.
pub(crate) fn fleet() -> Result<Vec<(String, PathBuf)>> {
    Ok(crate::fleet::members(&crate::fleet::registry()?)?
        .into_iter()
        .filter(|m| m.home.is_dir())
        .map(|m| (m.name, m.home))
        .collect())
}

/// The operator, or the liaison speaking from its own home.
fn desk(store: &Store) -> Result<Option<String>> {
    match store.caller() {
        Caller::Operator => Ok(None),
        Caller::Agent { actor } => {
            ensure!(
                crate::door::is_liaison(store.home())
                    && crate::presence::folder_ship(store).as_deref() == Some(actor.as_str()),
                "only the captain or the captain's liaison sees and answers what waits on the captain"
            );
            Ok(Some(actor.clone()))
        }
    }
}

/// Every open request or decision addressed to the captain, in every fleet
/// home, oldest first. Each entry carries when it was asked (`asked_ms`) and
/// how long ago that was (`age_ms`), so whoever reads the list can say "X asked
/// at T" instead of restating a request body as if it were news. Ages are
/// computed once here, against `now_ms`, and never stored.
///
/// Asks only the captain's hands can do (marked `needs_hands` by the sender)
/// also appear in `hands_on`, each with its exact `step`, so they are never
/// read as one more report. The marker is read from the record, never guessed
/// from the text.
pub fn waiting() -> Result<Value> {
    let now = Store::now_ms();
    let mut waiting = vec![];
    let mut hands_on = vec![];
    for (ship, home) in fleet()? {
        let mut store = Store::open(&home)?;
        let open = crate::mail::dispatch(
            &mut store,
            "mail.requests",
            // The default page is 100, oldest first; a long list would drop the newest asks.
            json!({"address": crate::mail::CAPTAIN, "state": "open", "limit": 1000}),
        )?;
        for mail in open["requests"].as_array().into_iter().flatten() {
            let asked = mail["created_ms"].as_i64().unwrap_or(now);
            // A record that does not pass the send-side rules is not a hands-on ask.
            let step = crate::mail::hands_step(
                mail["kind"].as_str().unwrap_or_default(),
                crate::mail::CAPTAIN,
                &mail["payload"],
            )
            .ok()
            .flatten();
            let item = json!({"ship": ship, "asked_ms": asked,
                "age_ms": (now - asked).max(0), "step": step, "mail": mail});
            if item["step"].is_string() {
                hands_on.push(item.clone());
            }
            waiting.push(item);
        }
    }
    // Mail attached to an open ask is answered with it: the ask shows once,
    // with how many items were added to it.
    let primaries: Vec<String> = waiting
        .iter()
        .filter(|w| w["mail"]["payload"]["attached_to"].is_null())
        .filter_map(|w| w["mail"]["id"].as_str().map(str::to_owned))
        .collect();
    let mut attached: std::collections::HashMap<String, u64> = Default::default();
    waiting.retain(|w| match w["mail"]["payload"]["attached_to"].as_str() {
        Some(primary) if primaries.iter().any(|p| p == primary) => {
            *attached.entry(primary.to_owned()).or_default() += 1;
            false
        }
        _ => true,
    });
    for item in &mut waiting {
        if let Some(count) = item["mail"]["id"].as_str().and_then(|id| attached.get(id)) {
            item["attached"] = json!(count);
        }
    }
    // Every hands-on ask is also in `waiting`, placed before the oldest others
    // are cut to the cap, so the two lists never disagree about an ask. Ids are
    // chosen by each ship, so an ask is named by its home and id.
    hands_on.sort_by_key(|w| w["asked_ms"].as_i64().unwrap_or(0));
    hands_on.truncate(MAX_WAITING);
    let key = |w: &Value| (w["ship"].clone(), w["mail"]["id"].clone());
    let hands_keys: Vec<_> = hands_on.iter().map(key).collect();
    let mut others: Vec<Value> = waiting
        .into_iter()
        .filter(|w| !hands_keys.contains(&key(w)))
        .collect();
    others.sort_by_key(|w| w["asked_ms"].as_i64().unwrap_or(0));
    others.truncate(MAX_WAITING - hands_on.len());
    let mut waiting = hands_on.clone();
    waiting.extend(others);
    waiting.sort_by_key(|w| w["asked_ms"].as_i64().unwrap_or(0));
    Ok(json!({"now_ms": now, "hands_on": hands_on, "waiting": waiting}))
}

/// Answer an open captain request with what the captain told the liaison.
fn relay(store: &Store, args: &Value) -> Result<Value> {
    let liaison = desk(store)?.context("the operator answers as the captain directly")?;
    let request_id = required(args, "request")?;
    let source_id = required(args, "source")?;
    identifier(request_id)?;
    identifier(source_id)?;
    let body = required(args, "body")?;
    // The captain's own words, in the liaison's mailbox.
    let (from, to, said, created): (String, String, String, i64) = store
        .connection()
        .query_row(
            "SELECT sender, recipient, body, created_ms FROM mail WHERE id=?1",
            [source_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .with_context(|| format!("no message {source_id} in {liaison}'s mailbox"))?;
    ensure!(
        from == crate::mail::CAPTAIN && to == liaison,
        "{source_id} is not the captain's message to {liaison}"
    );
    ensure!(
        Store::now_ms() - created <= SOURCE_MAX_AGE_MS,
        "{source_id} is more than a day old; ask the captain again"
    );
    for (ship, home) in fleet()? {
        let target = Store::open(&home)?;
        let request: Option<(String, String, Option<String>, Option<String>)> = target
            .connection()
            .query_row(
                "SELECT sender, kind, state, request_kind FROM mail WHERE id=?1 AND recipient=?2",
                params![request_id, crate::mail::CAPTAIN],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok();
        let Some((asker, kind, state, request_kind)) = request else {
            continue;
        };
        ensure!(
            request_kind.as_deref() != Some("production_approval"),
            "production approvals require the operator's exact artifact/environment grant or denial; they cannot be relayed"
        );
        ensure!(
            matches!(kind.as_str(), "request" | "decision"),
            "{request_id} is a {kind}, not a request"
        );
        ensure!(
            state.as_deref() == Some("open"),
            "{request_id} is no longer open"
        );
        let quote: String = said.chars().take(QUOTE_CHARS).collect();
        // A relay carries the captain's words but not the operator's marker,
        // so it cannot grant a tool permission.
        let reply = crate::mail::send_relayed(
            &target,
            &json!({"from": crate::mail::CAPTAIN, "to": asker, "kind": "reply",
                "correlation": request_id, "body": body,
                "payload": {"relayed_by": liaison, "source": source_id, "captain_said": quote}}),
        )?;
        let record = json!({"request": request_id, "ship": ship, "reply": reply["id"],
            "relayed_by": liaison, "source": source_id});
        target.audit("captain.relay", &record)?;
        store.audit("captain.relay", &record)?;
        return Ok(record);
    }
    bail!("no open captain request {request_id} in the fleet")
}

/// How many ids an ambiguous prefix lists.
const MATCHES_SHOWN: usize = 10;

/// The shortest prefix that may stand for an id, so a typo names nothing.
const MIN_PREFIX: usize = 4;

/// One open captain item, found in the fleet home that holds it.
struct Item {
    ship: String,
    home: PathBuf,
    id: String,
}

/// The open captain item `query` names, in any fleet home on this machine: an
/// exact id, or else a prefix that names exactly one item (as people quote the
/// first characters `/waiting` shows). Asks only, or notices too.
fn locate(query: &str, notices: bool) -> Result<Item> {
    identifier(query)?;
    let kinds = if notices {
        "('request','decision','notice')"
    } else {
        "('request','decision')"
    };
    let mut found: Vec<Item> = vec![];
    let mut closed: Option<String> = None;
    for (ship, home) in fleet()? {
        let store = Store::open(&home)?;
        // An exact id names that item even once it has closed: a retried or
        // double-tapped dismiss must not fall through to a longer id.
        let state: Option<Option<String>> = store
            .connection()
            .query_row(
                "SELECT state FROM mail WHERE recipient=?1 AND id=?2",
                params![crate::mail::CAPTAIN, query],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(Some(state)) = state
            && state != "open"
        {
            closed.get_or_insert(state);
        }
        let mut statement = store.connection().prepare(&format!(
            "SELECT id FROM mail WHERE recipient=?1 AND kind IN {kinds}
               AND (state='open' OR (kind='notice' AND state IS NULL))
               AND substr(id,1,length(?2))=?2
             ORDER BY seq LIMIT ?3"
        ))?;
        let ids = statement
            .query_map(
                params![crate::mail::CAPTAIN, query, MATCHES_SHOWN as i64 + 1],
                |r| r.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        found.extend(ids.into_iter().map(|id| Item {
            ship: ship.clone(),
            home: home.clone(),
            id,
        }));
    }
    if let Some(at) = found.iter().position(|item| item.id == query) {
        return Ok(found.swap_remove(at));
    }
    if let Some(state) = closed {
        bail!("{query} is no longer open ({state})");
    }
    ensure!(
        query.len() >= MIN_PREFIX,
        "give at least {MIN_PREFIX} characters of the id, or all of it"
    );
    // One ask may be held in two homes (a desk copy): it is still one item.
    let mut seen = std::collections::HashSet::new();
    found.retain(|item| seen.insert(item.id.clone()));
    let ids: Vec<String> = found
        .iter()
        .map(|item| format!("{} ({})", item.id, item.ship))
        .collect();
    match ids.len() {
        0 => bail!("no open captain item {query} in the fleet"),
        1 => Ok(found.swap_remove(0)),
        n => {
            let shown: Vec<_> = ids.into_iter().take(MATCHES_SHOWN).collect();
            bail!(
                "{query} names {} open captain items; give more of the id: {}",
                if n > MATCHES_SHOWN {
                    format!("more than {MATCHES_SHOWN}")
                } else {
                    n.to_string()
                },
                shown.join(", ")
            )
        }
    }
}

/// The desk's list, plus the open asks only this machine holds: those
/// written before the desk moved, or not handed to it yet. The desk's copy
/// of an ask keeps its id, so an ask is listed once.
fn with_local_asks(mut desk: Value) -> Result<Value> {
    let local = waiting()?;
    for list in ["waiting", "hands_on"] {
        let ids: std::collections::HashSet<String> = desk[list]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|w| w["mail"]["id"].as_str().map(str::to_owned))
            .collect();
        let extra: Vec<Value> = local[list]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|w| w["mail"]["id"].as_str().is_some_and(|id| !ids.contains(id)))
            .cloned()
            .collect();
        if extra.is_empty() {
            continue;
        }
        let merged = desk[list]
            .as_array_mut()
            .context("the desk's list is not a list")?;
        merged.extend(extra);
        merged.sort_by_key(|w| w["asked_ms"].as_i64().unwrap_or(0));
    }
    Ok(desk)
}

/// Whether a home on this machine holds captain mail whose id starts with
/// `query`, open or not.
fn held_here(query: &str) -> Result<bool> {
    identifier(query)?;
    for (_, home) in fleet()? {
        let held: bool = Store::open(&home)?.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM mail WHERE recipient=?1 AND substr(id,1,length(?2))=?2)",
            params![crate::mail::CAPTAIN, query],
            |r| r.get(0),
        )?;
        if held {
            return Ok(true);
        }
    }
    Ok(false)
}

fn operator(store: &Store) -> Result<()> {
    ensure!(
        *store.caller() == Caller::Operator,
        "only the captain answers or dismisses what waits on the captain"
    );
    Ok(())
}

/// The captain answers an open ask by id, in whichever home asked it. A ship
/// behind a machine link is answered through that link, as any captain reply
/// from this home is. A typed decision's answer is checked by `mail.send`.
///
/// A notice is not answered, but one that carries a report takes the
/// captain's annotations: they go back as a notice in its thread, which wakes
/// the ship and leaves the report notice as it was.
fn reply(store: &Store, args: &Value) -> Result<Value> {
    operator(store)?;
    answer(args)
}

fn answer(args: &Value) -> Result<Value> {
    let item = locate(required(args, "id")?, true)?;
    let mut target = Store::open(&item.home)?;
    let ask = crate::mail::find(&target, &item.id)?.context("ask vanished")?;
    ensure!(
        ask["request_kind"] != "production_approval",
        "{} is a production approval; answer it with approval.grant or approval.deny",
        item.id
    );
    let kind = if ask["kind"] == "notice" {
        ensure!(
            !ask["payload"]["report"].is_null(),
            "{} is a notice, which takes no reply; only a notice carrying a report takes the captain's annotations",
            item.id
        );
        ensure!(
            args["payload"]
                .as_object()
                .is_some_and(|p| p.len() == 1 && p.contains_key("annotations")),
            "{} is a report notice; the captain answers it only with payload.annotations",
            item.id
        );
        "notice"
    } else {
        "reply"
    };
    let mut message = json!({"from": crate::mail::CAPTAIN, "to": ask["from"],
        "kind": kind, "correlation": item.id});
    for key in ["body", "payload"] {
        if let Some(value) = args.get(key).filter(|v| !v.is_null()) {
            message[key] = value.clone();
        }
    }
    if let Some(payload) = message.get_mut("payload") {
        crate::files::resolve_elsewhere(ask["from"].as_str().unwrap_or_default(), payload)?;
    }
    let sent = crate::dispatch(&mut target, "mail.send", message)?;
    let record = json!({"id": item.id, "ship": item.ship, "to": ask["from"], "reply": sent["id"]});
    target.audit("captain.reply", &record)?;
    Ok(json!({"id": item.id, "ship": item.ship, "to": ask["from"], "reply": sent}))
}

/// The captain writes to a ship, from any of this machine's homes: in the
/// ship's own home when it is here, where its captain mailbox is, otherwise
/// from the captain's home here over the machine link that reaches it.
fn send(args: &Value) -> Result<Value> {
    let to = required(args, "to")?;
    identifier(to)?;
    let kind = match args.get("kind").filter(|v| !v.is_null()) {
        Some(kind) => kind.as_str().context("kind must be notice or request")?,
        None => "notice",
    };
    ensure!(
        matches!(kind, "notice" | "request"),
        "the captain sends a notice or a request"
    );
    let body = required(args, "body")?;
    ensure!(!body.trim().is_empty(), "write something to send");

    let conn = crate::fleet::registry()?;
    let here = crate::fleet::members(&conn)?
        .into_iter()
        .find(|m| m.name == to && m.home.is_dir());
    let (holder, home) = match here {
        Some(member) => (member.name, member.home),
        None => crate::machines::captain_home(&conn)?
            .context("no home on this machine holds the captain's mail")?,
    };
    let mut message = json!({"from": crate::mail::CAPTAIN, "to": to, "kind": kind, "body": body});
    if let Some(urgency) = args.get("urgency").filter(|v| !v.is_null()) {
        message["urgency"] = urgency.clone();
    }
    if let Some(payload) = args.get("payload").filter(|v| !v.is_null()) {
        message["payload"] = payload.clone();
        crate::files::resolve_elsewhere(to, &mut message["payload"])?;
    }
    let mut store = Store::open(&home)?;
    let sent = crate::dispatch(&mut store, "mail.send", message)?;
    store.audit("captain.send", &json!({"to": to, "id": sent["id"]}))?;
    Ok(json!({"to": to, "home": holder, "mail": sent}))
}

/// The captain's `mail.send` to a ship that lives behind a peer other than a
/// machine's: when that ship is on this machine, it is written in the ship's
/// own home, as `tell` from its folder would write it. Any other ship is
/// refused with what to run.
pub(crate) fn send_local(args: &Value) -> Result<Value> {
    let to = required(args, "to")?;
    let conn = crate::fleet::registry()?;
    let member = crate::fleet::members(&conn)?
        .into_iter()
        .find(|m| m.name == to && m.home.is_dir());
    let Some(member) = member else {
        bail!(
            "{to} is not a ship on this machine with its folder present, so the captain's mail \
             cannot reach it from this home: the captain's mail goes to ships on this machine \
             directly and to ships on a linked machine over its link. If {to} lives on another \
             machine, link that machine with `privateer machine add NAME SSH_TARGET --executable \
             PATH` and send again; if its folder was moved or deleted, retire it with \
             `privateer ship retire {to}`"
        );
    };
    let mut store = Store::open(&member.home)?;
    crate::dispatch(&mut store, "mail.send", args.clone())
}

/// The captain ends one item without answering it, in whichever home holds it.
fn dismiss(store: &Store, args: &Value) -> Result<Value> {
    operator(store)?;
    end(args)
}

fn end(args: &Value) -> Result<Value> {
    let note = match args.get("note").filter(|v| !v.is_null()) {
        Some(note) => note.as_str().context("note must be a string")?,
        None => "",
    };
    let item = locate(required(args, "id")?, true)?;
    let target = Store::open(&item.home)?;
    let ask = crate::mail::find(&target, &item.id)?.context("item vanished")?;
    ensure!(
        ask["request_kind"] != "production_approval",
        "{} is a production approval; deny it with approval.deny",
        item.id
    );
    let mail = crate::mail::dismiss(&target, &item.id, note)?;
    Ok(json!({"id": item.id, "ship": item.ship, "state": mail["state"], "mail": mail}))
}

/// One of the captain's commands, sent by the operator over a machine link
/// to this machine, which holds the captain's desk. The gateway has checked
/// the caller is the operator.
pub(crate) fn serve(command: &str, args: &Value) -> Result<Value> {
    match command {
        "captain.waiting" => waiting(),
        "captain.feed" => crate::feed::feed(args),
        "captain.send" => send(args),
        "captain.reply" => answer(args),
        "captain.dismiss" => end(args),
        _ => bail!("{command} is not served at the captain's desk"),
    }
}

pub fn dispatch(store: &mut Store, command: &str, _args: Value) -> Result<Value> {
    // The captain's mail gathers at the desk; when `machine captain` keeps it
    // on another machine, the captain reads it there. An ask held here is
    // still answered here (the desk's copy closes on the next pass); one only
    // the desk holds is answered there. The liaison is at the desk already.
    if *store.caller() == Caller::Operator {
        let at_desk = match command {
            "captain.waiting" | "captain.feed" | "captain.send" => true,
            "captain.reply" | "captain.dismiss" => !held_here(required(&_args, "id")?)?,
            _ => false,
        };
        if at_desk && let Some(result) = crate::machines::at_desk(command, &_args)? {
            return if command == "captain.waiting" {
                with_local_asks(result)
            } else {
                Ok(result)
            };
        }
    }
    match command {
        "captain.waiting" => {
            desk(store)?;
            waiting()
        }
        "captain.relay" => relay(store, &_args),
        "captain.send" => {
            operator(store)?;
            send(&_args)
        }
        "captain.reply" => reply(store, &_args),
        "captain.dismiss" => dismiss(store, &_args),
        "captain.feed" => {
            ensure!(
                *store.caller() == Caller::Operator,
                "only the captain reads the captain's mail feed"
            );
            crate::feed::feed(&_args)
        }
        _ => bail!("unknown captain command {command}"),
    }
}
