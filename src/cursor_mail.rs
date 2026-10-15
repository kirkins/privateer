//! Optional Cursor attended-mail hook. Native follow-ups, never terminal input.
use crate::store::{Caller, Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

const RECEIPT: &str = "[privateer-mail ";
const PARK_SECS: u64 = 28_790;

#[derive(Default, Deserialize, Serialize)]
struct State {
    holder: String,
    session: String,
    generation: String,
    epoch: String,
    phase: String,
    paused: bool,
    pending: Option<i64>,
    // Presentation receipts only, never mailbox acknowledgements. Native
    // transcript recovery covers a resumed conversation evicted from this list.
    seen: Vec<(String, i64)>,
}

impl State {
    fn through(&self, session: &str) -> i64 {
        self.seen
            .iter()
            .find(|(s, _)| s == session)
            .map_or(0, |(_, n)| *n)
    }
    fn remember(&mut self, session: &str, through: i64) {
        let through = through.max(self.through(session));
        self.seen.retain(|(s, _)| s != session);
        self.seen.push((session.into(), through));
        if self.seen.len() > 128 {
            self.seen.remove(0);
        }
    }
}

fn load(tx: &Transaction<'_>, ship: &str) -> Result<State> {
    let raw: Option<String> = tx
        .query_row("SELECT data FROM cursor_mail WHERE ship=?1", [ship], |r| {
            r.get(0)
        })
        .optional()?;
    match raw {
        Some(raw) => {
            ensure!(raw.len() <= 65_536, "Cursor mail state too large");
            Ok(serde_json::from_str(&raw)?)
        }
        None => Ok(State::default()),
    }
}

fn save(tx: &Transaction<'_>, ship: &str, state: &State) -> Result<()> {
    let raw = serde_json::to_string(state)?;
    ensure!(raw.len() <= 65_536, "Cursor mail state too large");
    tx.execute("INSERT INTO cursor_mail(ship,data) VALUES(?1,?2) ON CONFLICT(ship) DO UPDATE SET data=excluded.data", params![ship,raw])?;
    Ok(())
}

fn uuid(input: &Value, name: &str) -> Result<String> {
    let s = input[name].as_str().context("missing Cursor identity")?;
    ensure!(
        s.len() == 36 && uuid::Uuid::parse_str(s)?.to_string() == s,
        "invalid Cursor identity"
    );
    Ok(s.into())
}

fn owned(tx: &Transaction<'_>, ship: &str, holder: &str) -> Result<bool> {
    Ok(tx.query_row("SELECT EXISTS(SELECT 1 FROM presence WHERE ship=?1 AND holder=?2 AND harness='cursor' AND expires_ms>?3)", params![ship,holder,Store::now_ms()], |r| r.get(0))?)
}

/// Read only the bounded tail of this native conversation's transcript in
/// Cursor's project storage. Never follow a supplied path into arbitrary files.
fn receipts(
    input: &Value,
    home: &Path,
    ship: &str,
    session: &str,
    ceiling: i64,
) -> Result<Option<i64>> {
    let Some(raw) = input["transcript_path"].as_str() else {
        return Ok(None);
    };
    let Some(user_home) = std::env::var_os("HOME") else {
        return Ok(None);
    };
    let path = Path::new(raw);
    if !path.is_absolute()
        || path.file_name().and_then(|s| s.to_str()) != Some(&format!("{session}.jsonl"))
    {
        return Ok(None);
    }
    let Some(parent) = path.parent() else {
        return Ok(None);
    };
    if parent.file_name().and_then(|s| s.to_str()) != Some(session)
        || parent
            .parent()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            != Some("agent-transcripts")
    {
        return Ok(None);
    }
    let base = Path::new(&user_home).join(".cursor/projects");
    let (Ok(base), Ok(real), Ok(meta)) = (
        base.canonicalize(),
        path.canonicalize(),
        path.symlink_metadata(),
    ) else {
        return Ok(None);
    };
    if meta.file_type().is_symlink() || !meta.is_file() || !real.starts_with(&base) {
        return Ok(None);
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(real)?;
    let start = file.metadata()?.len().saturating_sub(1024 * 1024);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(1024 * 1024).read_to_end(&mut bytes)?;
    let tail = if start > 0 {
        bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(&[][..], |at| &bytes[at + 1..])
    } else {
        &bytes
    };
    let mut seen = 0;
    for line in tail
        .rsplit(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .take(128)
    {
        if line.len() > 65_536 {
            continue;
        }
        let Ok(row) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if row["role"] != "user" {
            continue;
        }
        let Some(content) = row["message"]["content"].as_array() else {
            continue;
        };
        for part in content {
            if part["type"] != "text" {
                continue;
            }
            let Some(text) = part["text"].as_str() else {
                continue;
            };
            for line in text.lines().take(128) {
                let Some(raw) = line.strip_prefix(RECEIPT).and_then(|s| s.strip_suffix(']')) else {
                    continue;
                };
                let Ok(tag) = serde_json::from_str::<Value>(raw) else {
                    continue;
                };
                if tag["home"].as_str() == home.to_str()
                    && tag["ship"] == ship
                    && tag["session"] == session
                {
                    if let Some(n) = tag["through"].as_i64().filter(|n| *n >= 0 && *n <= ceiling) {
                        seen = seen.max(n)
                    }
                }
            }
        }
    }
    Ok(Some(seen))
}

/// Errors are fail-open for the captain's native prompt, without copying hook
/// payloads, account information or message bodies into diagnostics.
pub fn hook(home: Option<&Path>, event: &str, caller: Result<Caller>) -> i32 {
    let mut bytes = Vec::new();
    let result = std::io::stdin().take(65_537).read_to_end(&mut bytes);
    let output = (|| -> Result<Value> {
        ensure!(
            result.is_ok() && bytes.len() <= 65_536,
            "invalid Cursor hook input"
        );
        let input: Value = serde_json::from_slice(&bytes)?;
        let Some(home) = home else {
            return Ok(json!({"continue":true}));
        };
        handle(home, event, &input, caller?)
    })();
    match output {
        Ok(output) => println!("{output}"),
        Err(error) => {
            eprintln!(
                "Privateer Cursor mail hook unavailable: {}",
                format!("{error:#}").chars().take(512).collect::<String>()
            );
            println!("{{\"continue\":true}}");
        }
    }
    0
}

fn handle(home: &Path, event: &str, input: &Value, caller: Caller) -> Result<Value> {
    let empty = || json!({"continue":true});
    if std::env::var_os("PRIVATEER_JOB").is_some()
        || std::env::var("PRIVATEER_ATTENDED_HARNESS").as_deref() != Ok("cursor")
        || input["is_background_agent"] == true
    {
        return Ok(empty());
    }
    let holder = std::env::var("PRIVATEER_PRESENCE_HOLDER")?;
    ensure!(
        holder.starts_with("start:") && holder.len() <= 200,
        "Cursor wrapper identity missing"
    );
    let mut store = Store::open_as(home, caller)?;
    let ship = crate::presence::folder_ship(&store).context("Cursor ship unknown")?;
    ensure!(
        matches!(store.caller(),Caller::Agent { actor } if actor == &ship),
        "Cursor hook actor mismatch"
    );
    let session = uuid(input, "conversation_id")?;
    let generation = uuid(input, "generation_id")?;
    ensure!(
        input["hook_event_name"] == event
            && (input["session_id"].is_null() || input["session_id"] == session),
        "Cursor hook identity mismatch"
    );
    let folder = store
        .home()
        .parent()
        .context("Cursor ship folder")?
        .canonicalize()?;
    ensure!(
        std::env::current_dir()?.canonicalize()? == folder,
        "Cursor hook workspace mismatch"
    );
    ensure!(
        input["workspace_roots"]
            .as_array()
            .is_some_and(
                |roots| roots.iter().any(|p| p.as_str().is_some_and(|p| Path::new(p)
                    .canonicalize()
                    .ok()
                    .as_ref()
                    == Some(&folder)))
            ),
        "Cursor hook roots mismatch"
    );
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS cursor_mail(ship TEXT PRIMARY KEY,data TEXT NOT NULL);",
    )?;
    let (maximum, acknowledged): (i64,i64) = store.connection().query_row("SELECT COALESCE(MAX(seq),0),COALESCE((SELECT through_seq FROM mail_cursor WHERE address=?1),0) FROM mail WHERE recipient=?1",[&ship],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let ceiling = maximum.max(acknowledged);
    let receipt = receipts(input, store.home(), &ship, &session, ceiling)?;
    let recovered = receipt.unwrap_or(0);
    let ticket = uuid::Uuid::new_v4().to_string();
    {
        let tx = store.write_transaction()?;
        if !owned(&tx, &ship, &holder)? {
            return Ok(empty());
        }
        let mut state = load(&tx, &ship)?;
        // A fire-and-forget sessionStart may arrive after a captain prompt;
        // it must never roll that prompt's generation back or reclaim an old
        // conversation. Only an actual new captain prompt switches sessions.
        let claim = event == "beforeSubmitPrompt"
            || (event == "sessionStart" && (state.holder != holder || state.session.is_empty()));
        if claim && (state.holder != holder || state.session != session) {
            if state.session != session {
                state.paused = false;
            }
            state.holder = holder.clone();
            state.session = session.clone();
            state.generation.clear();
            state.epoch = ticket.clone();
            state.phase = "idle".into();
            state.pending = None;
        }
        if state.holder != holder || state.session != session {
            return Ok(empty());
        }
        state.remember(&session, recovered);
        // A long native turn may push its user receipt out of the bounded
        // transcript tail. An explicit durable inbox acknowledgement is also
        // sufficient; notification/queue admission itself never is.
        let delivered = state
            .pending
            .is_some_and(|n| recovered >= n || acknowledged >= n);
        if delivered {
            state.remember(&session, state.pending.unwrap());
            state.pending = None;
        }
        match event {
            "sessionStart" => {
                save(&tx, &ship, &state)?;
                tx.commit()?;
                let bearings = crate::context::bearings(&store, &ship)?;
                return Ok(
                    json!({"additional_context":format!("You are the first mate of {ship}; the captain is aboard. {}",bearings["text"].as_str().unwrap_or(""))}),
                );
            }
            "beforeSubmitPrompt" => {
                // Native automatic follow-ups bypass this hook. A new captain
                // prompt explicitly permits retrying an unconfirmed/cancelled
                // notification; confirmed receipts have already advanced seen.
                state.pending = None;
                state.generation = generation;
                state.epoch = ticket;
                state.phase = "busy".into();
                state.paused = false;
                save(&tx, &ship, &state)?;
                tx.commit()?;
                return Ok(empty());
            }
            "sessionEnd" => {
                state.epoch = ticket;
                state.phase = "ended".into();
                save(&tx, &ship, &state)?;
                tx.commit()?;
                return Ok(empty());
            }
            "stop" => {
                if state.generation != generation {
                    if state.phase != "queued" || !delivered {
                        return Ok(empty());
                    }
                    state.generation = generation.clone();
                }
                if input["status"] != "completed" || receipt.is_none() {
                    state.paused = true;
                    state.phase = "paused".into();
                    state.epoch = ticket;
                    save(&tx, &ship, &state)?;
                    tx.commit()?;
                    return Ok(empty());
                }
                if state.paused || state.pending.is_some() || state.phase == "ended" {
                    return Ok(empty());
                }
                state.epoch = ticket.clone();
                state.phase = "park".into();
                save(&tx, &ship, &state)?;
                tx.commit()?;
            }
            _ => return Ok(empty()),
        }
    }
    // One native hook process sleeps without model inference. No database
    // write lock is held while parked; a captain prompt replaces its epoch.
    let secs = std::env::var("PRIVATEER_CURSOR_PARK_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| (1..=PARK_SECS).contains(n))
        .unwrap_or(PARK_SECS);
    let end = Instant::now() + Duration::from_secs(secs);
    let parent = unsafe { libc::getppid() };
    while Instant::now() < end {
        let after = {
            let tx = store.write_transaction()?;
            let state = load(&tx, &ship)?;
            if !owned(&tx, &ship, &holder)?
                || state.holder != holder
                || state.session != session
                || state.epoch != ticket
                || state.phase != "park"
                || unsafe { libc::getppid() } != parent
            {
                return Ok(empty());
            }
            state.through(&session)
        };
        let pending = crate::dispatch(
            &mut store,
            "mail.pending",
            json!({"address":ship,"after_seq":after}),
        )?;
        if pending["actionable"].as_u64().unwrap_or(0) > 0 {
            let through = pending["through_seq"]
                .as_i64()
                .context("Cursor mailbox sequence")?;
            let tx = store.write_transaction()?;
            let mut state = load(&tx, &ship)?;
            if !owned(&tx, &ship, &holder)? || state.epoch != ticket || state.phase != "park" {
                return Ok(empty());
            }
            state.pending = Some(through);
            state.phase = "queued".into();
            save(&tx, &ship, &state)?;
            tx.commit()?;
            let tag = json!({"home":store.home(),"ship":ship,"session":session,"through":through});
            let cli = crate::executable::installed()?;
            let inbox = json!([
                cli,
                "--home",
                store.home(),
                "mail.inbox",
                "--json",
                json!({"address":ship,"after_seq":after,"limit":40}).to_string()
            ]);
            return Ok(
                json!({"followup_message":format!("{RECEIPT}{tag}]\n{} new mailbox arrivals. This is a notification, not an acknowledgement. Read message bodies with argv {inbox}; page as needed, handle requests, reply with their correlation IDs, and acknowledge only handled mail. Your charter still applies. Headers: {}",pending["count"],pending["headers"])}),
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(empty())
}
