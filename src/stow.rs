//! Stowing a machine: before the captain updates or restarts Privateer, every
//! ship on that machine checkpoints its work and says what it left in flight,
//! and that machine starts no new jobs until the captain resumes.
//!
//! A stow is one record in the host's resource ledger, which every home on
//! the host shares, so job admission in any home sees the hold under the
//! ledger's lock. Each ship gets an ordinary captain request in its own home,
//! which the runtime tracks until the ship replies. First mates' wakes are not
//! held, so they can wake to stow; running jobs finish on their own.
//!
//! Every word about a stow names the machine it is about, because a linked
//! machine keeps its own hold: saying "the fleet" is how a ship on a machine
//! that was still stowed was told the whole fleet was running again. A linked
//! machine this one reaches is asked about its hold over the link; one that
//! reaches this machine cannot be asked, and is named as unknown rather than
//! assumed resumed.
//!
//! The ship the captain named as this machine's owner may stow the machine
//! too, to clear it for the captain or for one big build. Its requests come
//! from the owner, so the replies reach it, and only the owner or the captain
//! resumes the hold it started; the owner cannot end the captain's. Reaching
//! another machine's hold is the captain's alone, on either `--machine NAME`
//! or `--all`.
use crate::fleet::{members, registry};
use crate::store::Store;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

/// What a stowed ship is asked, after the sentence naming this machine.
const STOW: &str = "so new jobs are held until it resumes. Your mail keeps arriving, and jobs already running finish. Before you reply:
1. Finish the step you are on and start nothing new.
2. Commit work in progress in its workspace; do not release workspaces that hold unfinished work.
3. Cancel queued jobs you will not need. One you keep stays queued and starts after this machine resumes.
4. Let crew that are nearly done finish. `job.list` shows what is still running.
5. Replace the handoff note for each focus you hold, pin the exact facts you will need to pick it back up, and put lasting lessons in your ship note.
Then reply to this request with what you left in flight and what you will do first when this machine resumes.";

/// What a resumed ship is told, after the sentence naming this machine.
const RESUME: &str = "new jobs start again here. Carry on from your notes, and re-queue any checks you cancelled for the stow.";

/// Recently resumed stows one machine reports to another, so a request bound
/// to a peer's stow learns of its resume on the next exchange. Bounded: the
/// exchange is a line of JSON in a 1 MiB gateway frame.
const REPORTED: usize = 20;

/// Who asks: the captain, or the machine's owner, which looks after the
/// machine and asks in its own words.
fn asking(store: &Store) -> Option<String> {
    match store.caller() {
        crate::store::Caller::Agent { actor } => Some(actor.clone()),
        crate::store::Caller::Operator => None,
    }
}

fn stow_text(by: Option<&str>, this: &str) -> String {
    let subject = match by {
        None => format!("The captain is stowing this machine ({this}) to update Privateer"),
        Some(owner) => format!(
            "{owner}, which looks after this machine, is stowing it ({this}) to clear it for the captain or for one large build"
        ),
    };
    format!("{subject}, {STOW}")
}

/// The resume notice names the machine that resumed and every linked machine
/// still holding its own ships, so a ship never reads one machine's hold as
/// the whole fleet's. A machine this one cannot ask is named as unknown, never
/// as resumed.
fn resume_text(by: Option<&str>, this: &str, elsewhere: &[crate::machines::Stow]) -> String {
    let subject = match by {
        None => format!("The captain has resumed this machine ({this})"),
        Some(owner) => format!("{owner}, which looks after this machine, has resumed it ({this})"),
    };
    let mut text = format!("{subject}: {RESUME}");
    for line in elsewhere_lines(elsewhere) {
        text.push_str(&line);
    }
    text
}

/// One sentence per linked machine that still holds its own ships, and one per
/// linked machine whose hold this machine cannot know. A machine that reports
/// itself resumed is left out: the notice is not a list of every machine in the
/// fleet.
fn elsewhere_lines(holds: &[crate::machines::Stow]) -> Vec<String> {
    holds
        .iter()
        .filter_map(|hold| match (hold.held, hold.asked) {
            (Some(true), _) => Some(format!(
                " Linked machine {name} is still stowed, so new jobs for its ships are held there: do not wait on a reply from it until it resumes.",
                name = hold.name
            )),
            (Some(false), _) => None,
            (None, true) => Some(format!(
                " Linked machine {name} could not be reached from here, so whether it is still stowed is unknown here.",
                name = hold.name
            )),
            (None, false) => Some(format!(
                " Linked machine {name} reaches this machine and cannot be asked from here, so whether it is still stowed is unknown here.",
                name = hold.name
            )),
        })
        .collect()
}

/// The linked machines' own answers, kept as records beside this machine's so
/// the captain can check a machine-scoped claim before repeating it.
fn machines_json(holds: &[crate::machines::Stow]) -> Value {
    json!(
        holds
            .iter()
            .map(|hold| {
                let mut one = json!({"machine": hold.name, "asked": hold.asked, "held": hold.held});
                if let Some(error) = &hold.error {
                    one["error"] = json!(error);
                }
                one
            })
            .collect::<Vec<_>>()
    )
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
        bail!("this machine is already stowed ({active}); see `privateer stow status`");
    }
    let id = format!("stow-{}", Store::now_ms());
    let by = asking(store);
    let this = crate::machines::this_machine();
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
    ensure!(!ships.is_empty(), "this machine has no ships to stow");
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
            "kind": "request", "body": stow_text(by.as_deref(), &this)}),
        );
        asked.push(match sent {
            Ok(_) => json!({"ship": name, "asked": true}),
            Err(error) => json!({"ship": name, "asked": false, "error": format!("{error:#}")}),
        });
    }
    store.audit("stow.start", &json!({"stow": id}))?;
    Ok(json!({"stow": id, "held": true, "this_machine": this, "ships": asked}))
}

/// Which ships have replied and which jobs still run, from records only, and
/// what each linked machine reports about its own hold, from that machine.
fn status(store: &Store) -> Result<Value> {
    let this = crate::machines::this_machine();
    let holds = crate::machines::elsewhere("stow.status")?;
    let ledger = crate::jobs::host_ledger(store)?;
    let Some(id) = ledger
        .query_row(
            "SELECT id FROM stows ORDER BY started_ms DESC LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    else {
        // This machine is not stowed; a linked machine may still be.
        return Ok(json!({"stow": null, "held": false, "this_machine": this,
            "machines": machines_json(&holds)}));
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
        "started_ms": stow["started_ms"], "resumed_ms": stow["resumed_ms"],
        "this_machine": this, "machines": machines_json(&holds), "ships": ships}))
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

/// This machine's stow state, as one exchange carries it to a linked machine:
/// the stows in force and the ones resumed most recently, bounded. From
/// records only. `None` when the ledger cannot be read, so the exchange omits
/// the field rather than claiming no stows.
pub(crate) fn state(store: &Store) -> Option<Value> {
    let ledger = crate::jobs::host_ledger(store).ok()?;
    let read = || -> Result<Value> {
        let mut active = vec![];
        let mut statement = ledger
            .prepare("SELECT id FROM stows WHERE resumed_ms IS NULL ORDER BY started_ms DESC")?;
        for row in statement.query_map([], |r| r.get::<_, String>(0))? {
            active.push(json!(row?));
        }
        let mut resumed = vec![];
        let mut statement = ledger.prepare(
            "SELECT id, resumed_ms FROM stows WHERE resumed_ms IS NOT NULL
             ORDER BY resumed_ms DESC LIMIT ?1",
        )?;
        for row in statement.query_map([REPORTED as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })? {
            let (id, at) = row?;
            resumed.push(json!({"id": id, "resumed_ms": at}));
        }
        Ok(json!({"active": active, "resumed": resumed}))
    };
    read().ok()
}

/// Lift this machine's hold and tell every stowed ship to carry on. The
/// notice names the machines it can speak for and, for each linked machine it
/// cannot, says so rather than claiming the whole fleet is running again.
fn resume_here(store: &Store, holds: &[crate::machines::Stow]) -> Result<Value> {
    let this = crate::machines::this_machine();
    let ledger = crate::jobs::host_ledger(store)?;
    let Some(id) = crate::jobs::stowed(&ledger)? else {
        return Ok(
            json!({"stow": Value::Null, "held": false, "this_machine": this,
            "ships": [], "machines": machines_json(holds)}),
        );
    };
    let stow = record(&ledger, &id)?;
    let started_by = stow["ships"][0]["from"].as_str().map(str::to_owned);
    if let Some(owner) = asking(store) {
        ensure!(
            started_by.as_deref() == Some(owner.as_str()),
            "only the captain resumes a stow the captain started"
        );
    }
    let resumed_ms = Store::now_ms();
    ledger.execute(
        "UPDATE stows SET resumed_ms=?2 WHERE id=?1",
        params![id, resumed_ms],
    )?;
    // The hold lifts first, then the requests bound to it end. A request stored
    // before this point is open and ends here; one stored after is refused,
    // because the stow is no longer in force. A home that cannot be opened is
    // skipped and named, as an exchange skips one: its bound requests stay open.
    let reason = format!("stow {id} on {this} resumed at {resumed_ms}");
    let mut ended = 0;
    let mut skipped = vec![];
    // The hold has already lifted, so nothing here may abort the resume: an
    // error is recorded and the ships are still told.
    let members = crate::fleet::registry()
        .and_then(|registry| crate::fleet::members(&registry))
        .unwrap_or_else(|error| {
            skipped.push(format!("fleet registry: {error:#}"));
            vec![]
        });
    for member in members {
        if !member.home.is_dir() {
            continue;
        }
        let Ok(home) = Store::open(&member.home) else {
            skipped.push(member.home.display().to_string());
            continue;
        };
        match crate::mail::end_stow_bound(&home, None, |stow| {
            (stow == id.as_str()).then(|| reason.clone())
        }) {
            Ok(count) => ended += count,
            Err(error) => skipped.push(format!("{}: {error:#}", member.home.display())),
        }
    }
    let body = resume_text(started_by.as_deref(), &this, holds);
    let mut told = vec![];
    for ship in stow["ships"].as_array().into_iter().flatten() {
        let name = ship["name"].as_str().unwrap_or_default();
        let sent = send(
            ship,
            started_by.as_deref(),
            json!({"id": format!("{id}-resume-{name}"),
            "to": name, "kind": "notice", "body": body}),
        );
        told.push(json!({"ship": name, "told": sent.is_ok()}));
    }
    store.audit(
        "stow.resume",
        &json!({"stow": id, "requests_ended": ended, "homes_skipped": skipped}),
    )?;
    Ok(json!({"stow": id, "held": false, "this_machine": this,
        "requests_ended": ended, "homes_skipped": skipped,
        "ships": told, "machines": machines_json(holds)}))
}

/// Lift this machine's hold, naming the linked machines still holding theirs.
fn resume(store: &Store) -> Result<Value> {
    let ledger = crate::jobs::host_ledger(store)?;
    ensure!(
        crate::jobs::stowed(&ledger)?.is_some(),
        "this machine is not stowed; if a linked machine is the one stowed, \
         `privateer stow resume --all` lifts it first; see `privateer stow status`"
    );
    let holds = crate::machines::elsewhere("stow.status")?;
    resume_here(store, &holds)
}

/// Lift every linked machine this one reaches first, then this one, so the
/// notice each ship's first mate reads names no machine that is still stowed.
/// A machine that could not be reached stays in the notice as unknown: a
/// resume that could not ask is not a resume.
fn resume_all(store: &Store) -> Result<Value> {
    let mut holds = crate::machines::elsewhere("stow.status")?;
    for hold in &mut holds {
        if hold.held != Some(true) {
            continue;
        }
        // The linked machine lifts its own hold under its own ledger, and its
        // own ships hear it there, before anything is said on this machine.
        hold.held = match crate::machines::stow_linked(&hold.name, "stow.resume") {
            Ok(_) => Some(false),
            Err(error) => {
                // A resume that could not be asked lifts nothing, and a
                // timeout can arrive after the other machine really lifted
                // its hold. So the hold is unknown, never still stowed: the
                // notice says so and the error travels with it.
                hold.error = Some(format!("{error:#}"));
                None
            }
        };
    }
    let mut out = resume_here(store, &holds)?;
    out["all"] = json!(true);
    Ok(out)
}

/// The argument keys of each command here; see `crate::Accepted`.
pub(crate) const ACCEPTED: crate::Accepted = &[
    ("stow.start", &["machine", "all"]),
    ("stow.status", &["machine", "all"]),
    ("stow.resume", &["machine", "all"]),
];

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    let named = args.get("machine").filter(|v| !v.is_null());
    // `--all` reaches every linked machine this one reaches, so it is the
    // captain's alone in the same way `--machine NAME` is.
    let all = args
        .get("all")
        .is_some_and(|all| all.as_bool() == Some(true));
    if all {
        store.caller().require_operator(command)?;
        ensure!(
            named.is_none(),
            "`stow --all` already covers every linked machine this one reaches; drop --machine NAME"
        );
        ensure!(
            command == "stow.resume",
            "`stow --all` is for resuming: a machine is stowed one machine at a time"
        );
    }
    // A linked machine is stowed over its gateway, by the captain alone: even
    // the machine's owner stows only this machine.
    if let Some(name) = named {
        store.caller().require_operator(command)?;
        let name = name.as_str().context("machine must be a name")?;
        let mut out = crate::machines::stow_linked(name, command)?;
        // That answer is the other machine's: its `this_machine` and its own
        // linked machines describe that machine's fleet. Dropped rather than
        // passed through, so no surface here can read the other machine's view
        // as this one's. Ask for this machine's machines without naming one.
        if let Some(answer) = out.as_object_mut() {
            answer.remove("machines");
            answer.remove("this_machine");
        }
        out["machine"] = json!(name);
        return Ok(out);
    }
    match command {
        "stow.start" => start(store),
        "stow.status" => status(store),
        "stow.resume" if all => resume_all(store),
        "stow.resume" => resume(store),
        _ => bail!("unknown stow command {command}"),
    }
}
