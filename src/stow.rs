//! Stowing the fleet: before the captain updates or restarts Privateer, every
//! ship checkpoints its work and says what it left in flight, and the host
//! starts no new jobs until the captain resumes.
//!
//! A stow is one record in the host's resource ledger, which every home on
//! the host shares, so job admission in any home sees the hold under the
//! ledger's lock. Each ship gets an ordinary captain request in its own home,
//! which the runtime tracks until the ship replies. First mates' wakes are not
//! held, so they can wake to stow; running jobs finish on their own.
//!
//! The ship the captain named as this machine's owner may stow the machine
//! too, to clear it for the captain or for one big build. Its requests come
//! from the owner, so the replies reach it, and only the owner or the captain
//! resumes the hold it started; the owner cannot end the captain's.
use crate::fleet::{members, registry};
use crate::store::Store;
use anyhow::{Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

const STOW: &str = "The captain is stowing the fleet to update Privateer, so new jobs are held until the fleet resumes. Your mail keeps arriving, and jobs already running finish. Before you reply:
1. Finish the step you are on and start nothing new.
2. Commit work in progress in its workspace; do not release workspaces that hold unfinished work.
3. Cancel queued jobs you will not need. One you keep stays queued and starts after the fleet resumes.
4. Let crew that are nearly done finish. `job.list` shows what is still running.
5. Replace the handoff note for each focus you hold, pin the exact facts you will need to pick it back up, and put lasting lessons in your ship note.
Then reply to this request with what you left in flight and what you will do first when the fleet resumes.";

const RESUME: &str = "The captain has resumed the fleet: new jobs start again. Carry on from your notes, and re-queue any checks you cancelled for the stow.";

/// Who asks: the captain, or the machine's owner, which looks after the
/// machine and asks in its own words.
fn asking(store: &Store) -> Option<String> {
    match store.caller() {
        crate::store::Caller::Agent { actor } => Some(actor.clone()),
        crate::store::Caller::Operator => None,
    }
}

fn stow_text(by: Option<&str>) -> String {
    match by {
        None => STOW.into(),
        Some(owner) => STOW.replacen(
            "The captain is stowing the fleet to update Privateer, so new jobs are held until the fleet resumes.",
            &format!("{owner}, which looks after this machine, is stowing it to clear it for the captain or for one large build, so new jobs are held until it resumes."),
            1,
        ),
    }
}

fn resume_text(by: Option<&str>) -> String {
    match by {
        None => RESUME.into(),
        Some(owner) => RESUME.replacen(
            "The captain has resumed the fleet",
            &format!("{owner} has resumed the machine"),
            1,
        ),
    }
}

fn record(conn: &rusqlite::Connection, id: &str) -> Result<Value> {
    let (started, resumed, ships): (i64, Option<i64>, String) = conn.query_row(
        "SELECT started_ms, resumed_ms, ships FROM stows WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(
        json!({"id": id, "started_ms": started, "resumed_ms": resumed,
        "ships": serde_json::from_str::<Value>(&ships)?}),
    )
}

/// Hold new jobs, then ask every active ship to stow.
fn start(store: &Store) -> Result<Value> {
    let ledger = crate::jobs::host_ledger(store)?;
    if let Some(active) = crate::jobs::stowed(&ledger)? {
        bail!("the fleet is already stowed ({active}); see `privateer stow status`");
    }
    let id = format!("stow-{}", Store::now_ms());
    let by = asking(store);
    // The owner does not ask itself.
    let ships: Vec<Value> = members(&registry()?)?
        .into_iter()
        .filter(|m| m.home.is_dir() && by.as_deref() != Some(m.name.as_str()))
        .map(|m| {
            let mut ship =
                json!({"name": m.name, "home": m.home, "request": format!("{id}-{}", m.name)});
            if let Some(owner) = &by {
                ship["from"] = json!(owner);
                ship["from_home"] = json!(store.home());
            }
            ship
        })
        .collect();
    ensure!(!ships.is_empty(), "the fleet has no ships to stow");
    // The hold comes first, so no new job slips in while ships are asked.
    ledger.execute(
        "INSERT INTO stows(id, started_ms, ships) VALUES(?1, ?2, ?3)",
        params![id, Store::now_ms(), json!(ships).to_string()],
    )?;
    let mut asked = vec![];
    for ship in &ships {
        let name = ship["name"].as_str().unwrap_or_default();
        let sent = send(
            ship,
            by.as_deref(),
            json!({"id": ship["request"], "to": name,
            "kind": "request", "body": stow_text(by.as_deref())}),
        );
        asked.push(match sent {
            Ok(_) => json!({"ship": name, "asked": true}),
            Err(error) => json!({"ship": name, "asked": false, "error": format!("{error:#}")}),
        });
    }
    store.audit("stow.start", &json!({"stow": id}))?;
    Ok(json!({"stow": id, "held": true, "ships": asked}))
}

/// Which ships have replied and which jobs still run, from records only.
fn status(store: &Store) -> Result<Value> {
    let ledger = crate::jobs::host_ledger(store)?;
    let Some(id) = ledger
        .query_row(
            "SELECT id FROM stows ORDER BY started_ms DESC LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    else {
        return Ok(json!({"stow": null, "held": false}));
    };
    let stow = record(&ledger, &id)?;
    let mut ships = vec![];
    let mut ready = true;
    for ship in stow["ships"].as_array().into_iter().flatten() {
        let name = ship["name"].as_str().unwrap_or_default();
        let home = std::path::Path::new(ship["home"].as_str().unwrap_or_default());
        if !home.is_dir() {
            ships.push(json!({"ship": name, "error": "its folder is gone"}));
            continue;
        }
        let home = Store::open(home)?;
        let conn = home.connection();
        let request = ship["request"].as_str().unwrap_or_default();
        let (state, answered_by): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT state, answered_by FROM mail WHERE id=?1",
                [request],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((None, None));
        let reply: Option<String> = match &answered_by {
            Some(reply) => conn
                .query_row("SELECT body FROM mail WHERE id=?1", [reply], |r| r.get(0))
                .optional()?,
            None => None,
        };
        let mut statement = conn.prepare(
            "SELECT json_extract(record,'$.id'), json_extract(record,'$.task_id'),
               json_extract(record,'$.status')
             FROM jobs WHERE json_extract(record,'$.status')
               IN ('starting','queued','launching','running','orphaned')
             ORDER BY json_extract(record,'$.created_ms')",
        )?;
        let jobs = statement
            .query_map([], |r| {
                Ok(
                    json!({"id": r.get::<_, String>(0)?, "task_id": r.get::<_, String>(1)?,
                    "status": r.get::<_, String>(2)?}),
                )
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let replied = state.as_deref() == Some("answered");
        // Queued jobs wait out the hold; only work in motion blocks a stop.
        let moving = jobs.iter().filter(|j| j["status"] != "queued").count();
        ready &= replied && moving == 0;
        ships.push(json!({"ship": name, "replied": replied, "reply": reply,
            "jobs": jobs, "jobs_moving": moving}));
    }
    let held = stow["resumed_ms"].is_null();
    Ok(json!({"stow": id, "held": held, "ready": held && ready,
        "started_ms": stow["started_ms"], "resumed_ms": stow["resumed_ms"], "ships": ships}))
}

/// Mail one stowed ship: from the captain, in its own home, or from the
/// owner that asked, in the owner's home, so the reply reaches the owner.
fn send(ship: &Value, by: Option<&str>, mut mail: Value) -> Result<Value> {
    let home = match by {
        Some(owner) => {
            mail["from"] = json!(owner);
            ship["from_home"].as_str().unwrap_or_default()
        }
        None => {
            mail["from"] = json!(crate::mail::CAPTAIN);
            ship["home"].as_str().unwrap_or_default()
        }
    };
    let mut home = Store::open(std::path::Path::new(home))?;
    crate::mail::dispatch(&mut home, "mail.send", mail)
}

/// Lift the hold and tell every stowed ship to carry on.
fn resume(store: &Store) -> Result<Value> {
    let ledger = crate::jobs::host_ledger(store)?;
    let Some(id) = crate::jobs::stowed(&ledger)? else {
        bail!("the fleet is not stowed");
    };
    let stow = record(&ledger, &id)?;
    let started_by = stow["ships"][0]["from"].as_str().map(str::to_owned);
    if let Some(owner) = asking(store) {
        ensure!(
            started_by.as_deref() == Some(owner.as_str()),
            "only the captain resumes a stow the captain started"
        );
    }
    ledger.execute(
        "UPDATE stows SET resumed_ms=?2 WHERE id=?1",
        params![id, Store::now_ms()],
    )?;
    let mut told = vec![];
    for ship in stow["ships"].as_array().into_iter().flatten() {
        let name = ship["name"].as_str().unwrap_or_default();
        let sent = send(
            ship,
            started_by.as_deref(),
            json!({"id": format!("{id}-resume-{name}"),
            "to": name, "kind": "notice", "body": resume_text(started_by.as_deref())}),
        );
        told.push(json!({"ship": name, "told": sent.is_ok()}));
    }
    store.audit("stow.resume", &json!({"stow": id}))?;
    Ok(json!({"stow": id, "held": false, "ships": told}))
}

pub fn dispatch(store: &mut Store, command: &str, _args: Value) -> Result<Value> {
    match command {
        "stow.start" => start(store),
        "stow.status" => status(store),
        "stow.resume" => resume(store),
        _ => bail!("unknown stow command {command}"),
    }
}
