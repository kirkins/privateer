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
    waiting_visible(None)
}

pub(crate) fn waiting_visible(
    context: Option<&crate::door::scoped::DelegatedContext>,
) -> Result<Value> {
    let homes = crate::fleet::members(&crate::fleet::registry()?)?
        .into_iter()
        .map(|m| (m.name, m.home))
        .collect();
    waiting_homes(context, homes)
}

fn waiting_homes(
    context: Option<&crate::door::scoped::DelegatedContext>,
    homes: Vec<(String, PathBuf)>,
) -> Result<Value> {
    let now = Store::now_ms();
    let mut waiting = vec![];
    let mut hands_on = vec![];
    let mut reasons = vec![];
    for (ship, home) in homes {
        if context.is_some_and(|c| !c.allows(&ship)) {
            continue;
        }
        if !home.is_dir() {
            reasons.push("missing_home");
            continue;
        }
        let mut store = Store::open(&home)?;
        let open = crate::mail::dispatch(
            &mut store,
            "mail.requests",
            // The default page is 100, oldest first; a long list would drop the newest asks.
            json!({"address": crate::mail::CAPTAIN, "state": "open", "limit": 1000}),
        )?;
        // mail.requests has no continuation. At its scan limit we cannot
        // prove that every open ask was read, even if there are exactly 1000.
        if open["requests"].as_array().is_some_and(|r| r.len() >= 1000) {
            reasons.push("request_scan_limit");
        }
        for mail in open["requests"].as_array().into_iter().flatten() {
            if context.is_some_and(|c| !c.allows(mail["from"].as_str().unwrap_or_default())) {
                continue;
            }
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
    if waiting.len() > MAX_WAITING || hands_on.len() > MAX_WAITING {
        reasons.push("waiting_limit");
    }
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
    reasons.sort_unstable();
    reasons.dedup();
    let mut coverage = crate::door::coverage::local(reasons.is_empty());
    if !reasons.is_empty() {
        coverage["reasons"] = json!(reasons);
    }
    Ok(
        json!({"now_ms": now, "hands_on": hands_on, "waiting": waiting,
        "coverage": coverage}),
    )
}

/// Answer an open captain request with what the captain told the liaison.
fn relay(store: &Store, args: &Value) -> Result<Value> {
    let liaison = desk(store)?.context("the operator answers as the captain directly")?;
    let source = crate::mail::find(store, required(args, "source")?)?.context("source missing")?;
    let context = crate::mail::delegated_context(store, required(args, "source")?)?;
    ensure!(
        source["payload"].get("provenance").is_none() || context.is_some(),
        "delegated liaison provenance has no runtime grant context"
    );
    if let Some(context) = &context {
        ensure!(context.has("ship:message"), "insufficient_scope");
        ensure!(context.allows(&liaison), "ship_not_allowed");
    }
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
        if context.as_ref().is_some_and(|c| !c.allows(&ship)) {
            continue;
        }
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
        if let Some(context) = &context {
            ensure!(context.allows(&asker), "ship_not_allowed");
        }
        ensure!(
            !crate::mail::is_approval_kind(request_kind.as_deref()),
            "production approvals and privileged commands require the operator's exact grant or denial; they cannot be relayed"
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
        let mut message = json!({"from": crate::mail::CAPTAIN, "to": asker, "kind": "reply",
            "correlation": request_id, "body": body,
            "payload": {"relayed_by": liaison, "source": source_id, "captain_said": quote}});
        let reply = if let Some(context) = &context {
            // This is a new write. Admitted source mail is not recalled, but
            // cannot lend authority after its local grant stops being live.
            // Remote grants cannot be checked here and fail closed.
            context.recheck_relay()?;
            message["payload"]["source_provenance"] = source["payload"]["provenance"].clone();
            crate::mail::send_delegated(&target, &message, context)?
        } else {
            crate::mail::send_relayed(&target, &message)?
        };
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
fn locate(
    query: &str,
    notices: bool,
    context: Option<&crate::door::scoped::DelegatedContext>,
) -> Result<Item> {
    identifier(query)?;
    let kinds = if notices {
        "('request','decision','notice')"
    } else {
        "('request','decision')"
    };
    let mut found: Vec<Item> = vec![];
    let mut closed: Option<String> = None;
    for (ship, home) in fleet()? {
        if context.is_some_and(|c| !c.allows(&ship)) {
            continue;
        }
        let store = Store::open(&home)?;
        // An exact id names that item even once it has closed: a retried or
        // double-tapped dismiss must not fall through to a longer id.
        let state: Option<(Option<String>, String)> = store
            .connection()
            .query_row(
                "SELECT state, sender FROM mail WHERE recipient=?1 AND id=?2",
                params![crate::mail::CAPTAIN, query],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((Some(state), sender)) = state
            && context.is_none_or(|c| c.allows(&sender))
            && state != "open"
        {
            closed.get_or_insert(state);
        }
        let mut statement = store.connection().prepare(&format!(
            "SELECT id, sender FROM mail WHERE recipient=?1 AND kind IN {kinds}
               AND (state='open' OR (kind='notice' AND state IS NULL))
               AND substr(id,1,length(?2))=?2
             ORDER BY seq"
        ))?;
        let ids = statement
            .query_map(params![crate::mail::CAPTAIN, query], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .filter_map(|row| match row {
                Ok((_, sender)) if context.is_some_and(|c| !c.allows(&sender)) => None,
                row => Some(row.map(|(id, _)| id)),
            })
            .take(MATCHES_SHOWN + 1)
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
    let item = locate(required(args, "id")?, true, None)?;
    let mut target = Store::open(&item.home)?;
    let ask = crate::mail::find(&target, &item.id)?.context("ask vanished")?;
    ensure!(
        !crate::mail::is_approval_kind(ask["request_kind"].as_str()),
        "{} is a production approval or privileged command; answer it with approval.grant or approval.deny",
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
    let sent = crate::dispatch(&mut target, "mail.send", message)?;
    let record = json!({"id": item.id, "ship": item.ship, "to": ask["from"], "reply": sent["id"]});
    target.audit("captain.reply", &record)?;
    Ok(json!({"id": item.id, "ship": item.ship, "to": ask["from"], "reply": sent}))
}

/// A recipient on this machine, with its folder still present. Routing and
/// delivery must use the same lookup so a remote desk never hides a local ship.
fn local_recipient(to: &str) -> Result<Option<(String, PathBuf)>> {
    Ok(fleet()?.into_iter().find(|(name, _)| name == to))
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
    crate::mail::validate_captain_body(body)?;

    let (holder, home) = match local_recipient(to)? {
        Some(member) => member,
        None => crate::machines::captain_home(&crate::fleet::registry()?)?
            .context("no home on this machine holds the captain's mail")?,
    };
    let mut message = json!({"from": crate::mail::CAPTAIN, "to": to, "kind": kind, "body": body});
    if let Some(urgency) = args.get("urgency").filter(|v| !v.is_null()) {
        message["urgency"] = urgency.clone();
    }
    if let Some(payload) = args.get("payload").filter(|v| !v.is_null()) {
        message["payload"] = payload.clone();
    }
    let mut store = Store::open(&home)?;
    let sent = crate::dispatch(&mut store, "mail.send", message)?;
    Ok(json!({"to": to, "home": holder, "mail": sent}))
}

/// The captain's `mail.send` to a ship that lives behind a peer other than a
/// machine's: when that ship is on this machine, it is written in the ship's
/// own home, as `tell` from its folder would write it. Any other ship is
/// refused with what to run.
pub(crate) fn send_local(args: &Value) -> Result<Value> {
    let to = required(args, "to")?;
    let Some((_, home)) = local_recipient(to)? else {
        bail!(
            "{to} is not a ship on this machine with its folder present, so the captain's mail \
             cannot reach it from this home: the captain's mail goes to ships on this machine \
             directly and to ships on a linked machine over its link. If {to} lives on another \
             machine, link that machine with `privateer machine add NAME SSH_TARGET --executable \
             PATH` and send again; if its folder was moved or deleted, retire it with \
             `privateer ship retire {to}`"
        );
    };
    let mut store = Store::open(&home)?;
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
    let item = locate(required(args, "id")?, true, None)?;
    let target = Store::open(&item.home)?;
    let ask = crate::mail::find(&target, &item.id)?.context("item vanished")?;
    ensure!(
        !crate::mail::is_approval_kind(ask["request_kind"].as_str()),
        "{} is a production approval or privileged command; deny it with approval.deny",
        item.id
    );
    let mail = crate::mail::dismiss(&target, &item.id, note)?;
    Ok(json!({"id": item.id, "ship": item.ship, "state": mail["state"], "mail": mail}))
}

/// Local-only lookup and explicit mail operation: never route to a desk as an operator.
pub(crate) fn dismiss_delegated(
    args: &Value,
    context: &crate::door::scoped::DelegatedContext,
) -> Result<Value> {
    let item = locate(required(args, "id")?, true, Some(context))?;
    let target = Store::open(&item.home)?;
    let mail = crate::mail::dismiss_delegated(
        &target,
        &item.id,
        args["note"].as_str().unwrap_or(""),
        context,
    )?;
    Ok(json!({"id":item.id,"ship":item.ship,"state":mail["state"],"mail":mail}))
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

/// The argument keys of each command here; see `crate::Accepted`.
pub(crate) const ACCEPTED: crate::Accepted = &[
    ("captain.dismiss", &["id", "note"]),
    (
        "captain.feed",
        &[
            "after",
            "before",
            "limit",
            "ship",
            "include_ship_mail",
            "states",
            "open_only",
        ],
    ),
    ("captain.relay", &["request", "source", "body"]),
    ("captain.reply", &["id", "body", "payload"]),
    (
        "captain.send",
        &["to", "kind", "body", "urgency", "payload"],
    ),
    ("captain.waiting", &[]),
];

pub fn dispatch(store: &mut Store, command: &str, _args: Value) -> Result<Value> {
    // The captain's mail gathers at the desk; when `machine captain` keeps it
    // on another machine, the captain reads it there. An ask held here is
    // still answered here (the desk's copy closes on the next pass); one only
    // the desk holds is answered there. Sends to ships here stay here: the
    // desk may not have a link back. Other sends still use the desk's routes.
    // The liaison is at the desk already.
    if *store.caller() == Caller::Operator {
        let at_desk = match command {
            "captain.waiting" | "captain.feed" => true,
            "captain.send" => local_recipient(required(&_args, "to")?)?.is_none(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn asks(home: &std::path::Path, count: usize) {
        let store = Store::open(home).unwrap();
        let tx = store.connection().unchecked_transaction().unwrap();
        for n in 0..count {
            tx.execute(
                "INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,state)
                 VALUES(?1,'survey','captain','request','Question','{}','normal',?2,'open')",
                params![format!("ask-{n}"), n as i64],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }

    fn read(home: PathBuf) -> Value {
        let result = waiting_homes(None, vec![("survey".into(), home)]).unwrap();
        crate::door::coverage::validate(&result["coverage"], &[]).unwrap();
        result
    }

    #[test]
    fn waiting_over_one_hundred_open_asks_is_incomplete() {
        let temp = tempfile::tempdir().unwrap();
        asks(temp.path(), MAX_WAITING + 1);
        let result = read(temp.path().to_owned());
        assert_eq!(result["waiting"].as_array().unwrap().len(), MAX_WAITING);
        assert_eq!(result["coverage"]["complete"], false);
        assert_eq!(result["coverage"]["reasons"], json!(["waiting_limit"]));
    }

    #[test]
    fn waiting_at_the_request_scan_limit_is_incomplete_even_when_asks_collapse() {
        let temp = tempfile::tempdir().unwrap();
        asks(temp.path(), 1001);
        let store = Store::open(temp.path()).unwrap();
        // The final list is small, but the scan still cannot prove coverage.
        store
            .connection()
            .execute(
                "UPDATE mail SET payload='{\"attached_to\":\"ask-0\"}' WHERE id!='ask-0'",
                [],
            )
            .unwrap();
        let result = read(temp.path().to_owned());
        assert_eq!(result["waiting"].as_array().unwrap().len(), 1);
        assert_eq!(result["waiting"][0]["attached"], 999);
        assert_eq!(result["coverage"]["complete"], false);
        assert_eq!(result["coverage"]["reasons"], json!(["request_scan_limit"]));
    }

    #[test]
    fn waiting_missing_member_home_is_incomplete_without_exposing_its_path() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        let result = read(missing.clone());
        assert_eq!(result["coverage"]["complete"], false);
        assert_eq!(result["coverage"]["reasons"], json!(["missing_home"]));
        assert!(!result.to_string().contains(missing.to_str().unwrap()));
        assert!(
            !missing.exists(),
            "reading must not recreate a missing home"
        );
    }

    #[test]
    fn waiting_ignores_missing_homes_outside_the_admitted_grant() {
        let temp = tempfile::tempdir().unwrap();
        asks(temp.path(), 1);
        let context = crate::door::scoped::DelegatedContext::from_gateway(&json!({
            "version":1,"client":uuid::Uuid::new_v4().to_string(),"resource":"door:fixture",
            "scopes":["mail:read"],"ships":["survey"],
            "result_id":uuid::Uuid::new_v4().to_string()
        }))
        .unwrap();
        let result = waiting_homes(
            Some(&context),
            vec![
                ("survey".into(), temp.path().to_owned()),
                ("hidden".into(), temp.path().join("missing-hidden-home")),
            ],
        )
        .unwrap();
        assert_eq!(result["coverage"], crate::door::coverage::local(true));
        crate::door::coverage::validate(&result["coverage"], &["hidden"]).unwrap();
    }

    #[test]
    fn waiting_with_every_ask_read_is_complete_only_locally() {
        let temp = tempfile::tempdir().unwrap();
        asks(temp.path(), MAX_WAITING);
        let result = read(temp.path().to_owned());
        assert_eq!(result["coverage"], crate::door::coverage::local(true));
    }
}
