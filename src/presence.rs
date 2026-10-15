//! Attended mode: the captain's interactive session is the ship's first mate.
//!
//! A session aboard holds a presence lease, refreshed while it is active.
//! While the lease is live, `serve` does not start a competing headless wake
//! for that ship; new mail is shown to the live session instead, by harness
//! hooks (Claude Code's are built in) or by the session reading its inbox.
//! Each message is presented once, so a first mate that does not acknowledge
//! cannot be prompted in a loop. When the session leaves or its lease lapses,
//! headless wakes resume.
use crate::store::{Store, identifier, required};
use anyhow::{Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;

const MAX_TTL_SECS: u64 = 86_400;
// As much of a message as a wake shows, so crew log tails survive.
const BODY_CHARS: usize = 2_000;

pub fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS presence(ship TEXT PRIMARY KEY, holder TEXT NOT NULL,
           harness TEXT, started_ms INTEGER NOT NULL, expires_ms INTEGER NOT NULL,
           presented_through INTEGER NOT NULL DEFAULT 0);",
    )?;
    Ok(())
}

fn local_ship(store: &Store, ship: &str) -> Result<()> {
    identifier(ship)?;
    let here: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM ships WHERE name=?1 AND json_extract(record,'$.location')='here')",
        [ship],
        |r| r.get(0),
    )?;
    ensure!(here, "{ship} is not a ship hosted by this home");
    Ok(())
}

/// Take or refresh the ship's presence lease. A newer session replaces an
/// older holder; presentation progress carries over.
pub fn attend(
    store: &Store,
    ship: &str,
    holder: &str,
    ttl_secs: u64,
    harness: Option<&str>,
) -> Result<Value> {
    local_ship(store, ship)?;
    ensure!(
        !holder.is_empty() && holder.len() <= 200,
        "holder must name the session"
    );
    ensure!(
        (1..=MAX_TTL_SECS).contains(&ttl_secs),
        "ttl_secs must be 1..{MAX_TTL_SECS}"
    );
    let now = Store::now_ms();
    store.connection().execute(
        "INSERT INTO presence(ship, holder, harness, started_ms, expires_ms) VALUES(?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(ship) DO UPDATE SET
           started_ms = CASE WHEN holder = excluded.holder THEN started_ms ELSE excluded.started_ms END,
           holder = excluded.holder, harness = COALESCE(excluded.harness, harness),
           expires_ms = excluded.expires_ms",
        params![ship, holder, harness, now, now + ttl_secs as i64 * 1000],
    )?;
    presence(store, ship)
}

/// Release the lease if this holder still has it.
pub fn leave(store: &Store, ship: &str, holder: &str) -> Result<bool> {
    Ok(store.connection().execute(
        "DELETE FROM presence WHERE ship=?1 AND holder=?2",
        params![ship, holder],
    )? == 1)
}

/// A heartbeat may extend its own lease, never reclaim a replacement.
pub fn refresh(store: &Store, ship: &str, holder: &str, ttl_secs: u64) -> Result<bool> {
    local_ship(store, ship)?;
    ensure!(
        (1..=MAX_TTL_SECS).contains(&ttl_secs),
        "ttl_secs must be 1..{MAX_TTL_SECS}"
    );
    let now = Store::now_ms();
    Ok(store.connection().execute(
        "UPDATE presence SET expires_ms=?3 WHERE ship=?1 AND holder=?2 AND expires_ms>?4",
        params![ship, holder, now + ttl_secs as i64 * 1000, now],
    )? == 1)
}

/// Whether a live session is aboard.
pub fn attended(store: &Store, ship: &str) -> Result<bool> {
    Ok(store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM presence WHERE ship=?1 AND expires_ms > ?2)",
        params![ship, Store::now_ms()],
        |r| r.get(0),
    )?)
}

fn presence(store: &Store, ship: &str) -> Result<Value> {
    let lease = store
        .connection()
        .query_row(
            "SELECT holder, harness, started_ms, expires_ms FROM presence WHERE ship=?1",
            [ship],
            |r| {
                Ok(json!({"holder": r.get::<_, String>(0)?, "harness": r.get::<_, Option<String>>(1)?,
                    "started_ms": r.get::<_, i64>(2)?, "expires_ms": r.get::<_, i64>(3)?}))
            },
        )
        .optional()?;
    let aboard = lease
        .as_ref()
        .is_some_and(|l| l["expires_ms"].as_i64().unwrap_or(0) > Store::now_ms());
    Ok(json!({"ship": ship, "attended": aboard, "lease": lease}))
}

/// New mail for the ship that the live session has not been shown yet;
/// marks it shown.
fn present(store: &Store, ship: &str) -> Result<Vec<Value>> {
    let tx = store.write_transaction()?;
    let shown: i64 = tx
        .query_row(
            "SELECT presented_through FROM presence WHERE ship=?1",
            [ship],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    let acknowledged: i64 = tx
        .query_row(
            "SELECT through_seq FROM mail_cursor WHERE address=?1",
            [ship],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    let mut statement = tx.prepare(
        "SELECT seq, id, sender, kind, correlation, body, state FROM mail
         WHERE recipient=?1 AND COALESCE(delivery,'received')='received' AND seq > ?2
         ORDER BY seq LIMIT 50",
    )?;
    let messages = statement
        .query_map(params![ship, shown.max(acknowledged)], |r| {
            Ok(
                json!({"seq": r.get::<_, i64>(0)?, "id": r.get::<_, String>(1)?,
                "from": r.get::<_, String>(2)?, "kind": r.get::<_, String>(3)?,
                "correlation": r.get::<_, Option<String>>(4)?, "body": r.get::<_, String>(5)?,
                "state": r.get::<_, Option<String>>(6)?}),
            )
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    if let Some(last) = messages.last().and_then(|m| m["seq"].as_i64()) {
        tx.execute(
            "UPDATE presence SET presented_through=?2 WHERE ship=?1",
            params![ship, last],
        )?;
    }
    tx.commit()?;
    Ok(messages)
}

fn render(ship: &str, messages: &[Value]) -> String {
    let mut text = format!("New mail for {ship}:\n");
    for m in messages {
        let body: String = m["body"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(BODY_CHARS)
            .collect();
        let correlation = m["correlation"]
            .as_str()
            .map(|c| format!(" correlation {c}"))
            .unwrap_or_default();
        text.push_str(&format!(
            "- seq {} id {} from {} kind {}{correlation}\n  {}\n",
            m["seq"],
            m["id"].as_str().unwrap_or(""),
            m["from"].as_str().unwrap_or(""),
            m["kind"].as_str().unwrap_or(""),
            body
        ));
    }
    let last = messages
        .last()
        .map(|m| m["seq"].clone())
        .unwrap_or(json!(0));
    text.push_str(&format!(
        "Handle it as {ship}'s first mate (see AGENTS.md), then mail.ack {{\"address\":\"{ship}\",\"through_seq\":{last}}}.\n"
    ));
    text
}

/// The ship this home's folder is, recorded by `privateer init`.
pub fn folder_ship(store: &Store) -> Option<String> {
    let raw = std::fs::read(store.home().join("config.json")).ok()?;
    let config: Value = serde_json::from_slice(&raw).ok()?;
    config["ship"].as_str().map(str::to_owned)
}

/// How long a hook-held lease lasts without activity. Hooks refresh it on
/// every prompt and turn end; an idle session that was walked away from
/// stops holding off headless wakes after this.
const HOOK_TTL_SECS: u64 = 1_800;

/// `privateer hook claude <Event>`: Claude Code hook entry point. It reads
/// the hook payload on stdin and never breaks the session: any failure
/// allows the event and is logged to `<home>/hooks.log`.
pub fn claude_hook(home: Option<&Path>, event: &str) -> i32 {
    let mut payload = String::new();
    let _ = std::io::stdin().take(1 << 20).read_to_string(&mut payload);
    let Some(home) = home else {
        return 0;
    };
    match claude_hook_inner(home, event, &payload) {
        Ok(code) => code,
        Err(error) => {
            let line = format!("{} {event}: {error:#}\n", Store::now_ms());
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(home.join("hooks.log"))
                .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
            0
        }
    }
}

fn claude_hook_inner(home: &Path, event: &str, payload: &str) -> Result<i32> {
    // A supervised crew job may inherit this project's Claude settings and
    // the captain's environment. It has a brief, not an attended mailbox.
    if std::env::var_os("PRIVATEER_JOB").is_some()
        || std::env::var("PRIVATEER_ATTENDED_HARNESS").as_deref() == Ok("cursor")
    {
        return Ok(0);
    }
    let input: Value = serde_json::from_str(payload).unwrap_or(json!({}));
    // Cursor can import this project's Claude compatibility hooks even when
    // started directly. Its native payload must never claim a Claude lease
    // or consume Claude's presentation cursor behind the attended adapter.
    if input.get("cursor_version").is_some() {
        return Ok(0);
    }
    let mut store = Store::open(home)?;
    store.set_caller(crate::store::Caller::resolve(
        std::env::var("PRIVATEER_ROLE").ok().as_deref(),
        std::env::var("PRIVATEER_ACTOR").ok().as_deref(),
        None,
        None,
    )?);
    let Some(ship) = folder_ship(&store) else {
        return Ok(0);
    };
    if let crate::store::Caller::Agent { actor } = store.caller() {
        ensure!(actor == &ship, "hook actor must be this folder's ship");
    }
    let wrapped = std::env::var("PRIVATEER_PRESENCE_HOLDER").ok();
    let holder = wrapped.clone().unwrap_or_else(|| {
        format!(
            "claude:{}",
            input["session_id"].as_str().unwrap_or("unknown")
        )
    });
    if wrapped.is_some() && !refresh(&store, &ship, &holder, 90)? {
        return Ok(0);
    }
    // The native mod sets this only after it has loaded and verified this
    // session's lease. A refused/disabled mod never disables the old hooks;
    // a crashed or unloaded mod's heartbeat expires without a config rewrite.
    let now = Store::now_ms();
    let native_mail = wrapped.is_some()
        && std::env::var("PRIVATEER_CLAUDE_MAIL_UNTIL")
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
            .is_some_and(|until| until > now && until <= now + 15_000);
    match event {
        "SessionStart" => {
            if wrapped.is_none() {
                attend(&store, &ship, &holder, HOOK_TTL_SECS, Some("claude"))?;
            }
            let bearings = crate::context::bearings(&store, &ship)?;
            print!(
                "You are the first mate of the ship {ship}; the captain is aboard.\n{}",
                bearings["text"].as_str().unwrap_or("")
            );
            let mail = if native_mail {
                vec![]
            } else {
                present(&store, &ship)?
            };
            if !mail.is_empty() {
                print!("{}", render(&ship, &mail));
            }
            Ok(0)
        }
        "UserPromptSubmit" => {
            if native_mail {
                return Ok(0);
            }
            if wrapped.is_none() {
                attend(&store, &ship, &holder, HOOK_TTL_SECS, Some("claude"))?;
            }
            let mail = present(&store, &ship)?;
            if !mail.is_empty() {
                print!("{}", render(&ship, &mail));
            }
            Ok(0)
        }
        "Stop" => {
            if native_mail {
                return Ok(0);
            }
            if wrapped.is_none() {
                attend(&store, &ship, &holder, HOOK_TTL_SECS, Some("claude"))?;
            }
            let mail = present(&store, &ship)?;
            if mail.is_empty() {
                return Ok(0);
            }
            // Exit status 2 keeps Claude working, with stderr as the reason.
            eprint!("{}", render(&ship, &mail));
            Ok(2)
        }
        "SessionEnd" => {
            if wrapped.is_none() {
                leave(&store, &ship, &holder)?;
            }
            Ok(0)
        }
        _ => Ok(0),
    }
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "ship.attend" => {
            let ship = required(&args, "ship")?;
            let holder = required(&args, "holder")?;
            let ttl = args
                .get("ttl_secs")
                .and_then(Value::as_u64)
                .unwrap_or(HOOK_TTL_SECS);
            attend(
                store,
                ship,
                holder,
                ttl,
                args.get("harness").and_then(Value::as_str),
            )
        }
        "ship.leave" => {
            let ship = required(&args, "ship")?;
            let left = leave(store, ship, required(&args, "holder")?)?;
            Ok(json!({"ship": ship, "left": left}))
        }
        "ship.presence" => {
            let ship = required(&args, "ship")?;
            local_ship(store, ship)?;
            presence(store, ship)
        }
        _ => bail!("unknown presence command {command}"),
    }
}
