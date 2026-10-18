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
use rusqlite::params;
use serde_json::{Value, json};
use std::path::PathBuf;

const MAX_WAITING: usize = 100;
/// How old a captain's message a relayed answer may cite.
const SOURCE_MAX_AGE_MS: i64 = 24 * 60 * 60 * 1000;
const QUOTE_CHARS: usize = 2_000;

/// Active fleet ships whose folders still exist, with their homes.
fn fleet() -> Result<Vec<(String, PathBuf)>> {
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

pub fn dispatch(store: &mut Store, command: &str, _args: Value) -> Result<Value> {
    match command {
        "captain.waiting" => {
            desk(store)?;
            waiting()
        }
        "captain.relay" => relay(store, &_args),
        _ => bail!("unknown captain command {command}"),
    }
}
