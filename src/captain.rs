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
pub fn waiting() -> Result<Value> {
    let now = Store::now_ms();
    let mut waiting = vec![];
    for (ship, home) in fleet()? {
        let mut store = Store::open(&home)?;
        let open = crate::mail::dispatch(
            &mut store,
            "mail.requests",
            json!({"address": crate::mail::CAPTAIN, "state": "open"}),
        )?;
        for mail in open["requests"].as_array().into_iter().flatten() {
            let asked = mail["created_ms"].as_i64().unwrap_or(now);
            waiting.push(json!({"ship": ship, "asked_ms": asked,
                "age_ms": (now - asked).max(0), "mail": mail}));
        }
    }
    waiting.sort_by_key(|w| w["asked_ms"].as_i64().unwrap_or(0));
    waiting.truncate(MAX_WAITING);
    Ok(json!({"now_ms": now, "waiting": waiting}))
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
        let mut target = Store::open(&home)?;
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
        let reply = crate::mail::dispatch(
            &mut target,
            "mail.send",
            json!({"from": crate::mail::CAPTAIN, "to": asker, "kind": "reply",
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
