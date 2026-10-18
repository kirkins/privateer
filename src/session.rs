//! The captain looks into a ship's live first-mate session and types into
//! it, from any of the fleet's machines.
//!
//! A hosted first mate runs as an interactive harness in a terminal host
//! pane, bound to the ship's presence lease. `session.read` returns that
//! pane's recent screen (with its colours, and a revision that changes when
//! the screen does) and the agent's state; `session.send` types text and
//! keys into it. Both are the operator's. The binding is checked against
//! the host before use, so a pane that has been reused for something else is
//! never read or typed into. A ship on a linked machine is reached over the
//! gateway.

use crate::herdr::{Binding, Client};
use crate::store::{Caller, Store, identifier, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::path::PathBuf;

/// Lines of screen and scrollback read by default, and at most.
const LINES: u32 = 200;
const MAX_LINES: u32 = 2000;
const MAX_TEXT_CHARS: usize = 8192;
const MAX_KEYS: usize = 16;

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    ensure!(
        *store.caller() == Caller::Operator,
        "only the captain reads and types into a ship's session"
    );
    let ship = required(&args, "ship")?;
    identifier(ship)?;
    check(command, &args)?;
    if let Some(home) = home_of(ship)? {
        return here(&home, ship, command, &args);
    }
    crate::machines::session_on_linked(command, &args)?
        .with_context(|| format!("no ship named {ship} in the fleet has a live session"))
}

/// A session command from a machine that links to this one: this machine's
/// homes only, never asked further.
pub(crate) fn serve(command: &str, args: &Value) -> Result<Value> {
    let ship = required(args, "ship")?;
    identifier(ship)?;
    check(command, args)?;
    let home = home_of(ship)?.with_context(|| format!("no ship named {ship} on this machine"))?;
    here(&home, ship, command, args)
}

fn check(command: &str, args: &Value) -> Result<()> {
    match command {
        "session.read" => {
            if let Some(lines) = args.get("lines").filter(|v| !v.is_null()) {
                let lines = lines.as_u64().context("lines must be a whole number")?;
                ensure!(
                    (1..=u64::from(MAX_LINES)).contains(&lines),
                    "lines must be 1..{MAX_LINES}"
                );
            }
        }
        "session.send" => {
            let text = args.get("text").and_then(Value::as_str).unwrap_or("");
            ensure!(
                text.chars().count() <= MAX_TEXT_CHARS,
                "text is longer than {MAX_TEXT_CHARS} characters"
            );
            let keys = keys(args)?;
            ensure!(
                !text.is_empty() || !keys.is_empty(),
                "send text, keys or both"
            );
        }
        _ => bail!("unknown session command {command}"),
    }
    Ok(())
}

fn keys(args: &Value) -> Result<Vec<String>> {
    let Some(keys) = args.get("keys").filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let keys: Vec<String> = keys
        .as_array()
        .context("keys must be a list")?
        .iter()
        .map(|k| {
            k.as_str()
                .map(str::to_owned)
                .context("each key is a name, such as Enter")
        })
        .collect::<Result<_>>()?;
    ensure!(keys.len() <= MAX_KEYS, "at most {MAX_KEYS} keys at once");
    ensure!(
        keys.iter()
            .all(|k| !k.is_empty() && k.len() <= 32 && k.chars().all(|c| c.is_ascii_graphic())),
        "a key is a short name, such as Enter or C-c"
    );
    Ok(keys)
}

/// The home on this machine that hosts `ship`, if any.
fn home_of(ship: &str) -> Result<Option<PathBuf>> {
    Ok(crate::fleet::members(&crate::fleet::registry()?)?
        .into_iter()
        .find(|m| m.name == ship && m.home.is_dir())
        .map(|m| m.home))
}

fn here(home: &std::path::Path, ship: &str, command: &str, args: &Value) -> Result<Value> {
    let store = Store::open(home)?;
    let (binding, holder) = live_binding(&store, ship)?;
    let mut client = Client::connect(&binding.socket)?;
    let view = client.snapshot()?;
    let pane = view
        .panes
        .iter()
        .find(|p| binding.matches(&view, p))
        .with_context(|| format!("{ship}'s session is no longer in its pane"))?;
    match command {
        "session.read" => {
            let lines = args["lines"].as_u64().map_or(LINES, |n| n as u32);
            let read = client.read_screen(&pane.pane_id, lines)?;
            Ok(json!({
                "ship": ship,
                "pane_id": pane.pane_id,
                "agent_status": pane.agent_status,
                "revision": read["revision"],
                "text": read["text"],
                "truncated": read["truncated"],
            }))
        }
        _ => {
            let text = args.get("text").and_then(Value::as_str).unwrap_or("");
            let keys = keys(args)?;
            client.type_into(&pane.pane_id, text, &keys)?;
            store.audit(
                "session.send",
                &json!({"ship": ship, "holder": holder, "chars": text.chars().count(), "keys": keys}),
            )?;
            Ok(json!({"ship": ship, "pane_id": pane.pane_id, "sent": true}))
        }
    }
}

/// The terminal host binding of the ship's live lease.
fn live_binding(store: &Store, ship: &str) -> Result<(Binding, String)> {
    let row: Option<(Option<String>, String)> = store
        .connection()
        .query_row(
            "SELECT binding, holder FROM presence WHERE ship=?1 AND expires_ms > ?2",
            rusqlite::params![ship, Store::now_ms()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (binding, holder) = row.with_context(|| format!("{ship} has no live session"))?;
    let binding = binding.with_context(|| format!("{ship}'s session is not in a terminal host"))?;
    Ok((serde_json::from_str(&binding)?, holder))
}
