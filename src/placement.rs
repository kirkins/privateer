//! Owner-proposed moves: a machine under persistent disk pressure may have
//! one of its idle ships live on the other end of one of its links. The
//! machine's owner proposes; the captain decides; nothing moves without the
//! captain's own yes.
//!
//! - **Pressure.** The owner's disk alerts (`disk_floor`, `disk_trend`) are
//!   kept with the machine, in `machine_pressure` beside the fleet registry,
//!   so the clock does not travel with the owner. A key raised continuously
//!   for `pressure_after_secs` (from its raising, or from the last cleanup of
//!   a moved ship's folder) is persistent. Other alerts never count.
//! - **Candidates.** `ship.placement` lists the pressure, whatever holds
//!   proposals back, and for each ship here whether it may be proposed and
//!   which linked machines could host it, from what they last reported in the
//!   exchange: their offers, their own pressure and their free disk.
//! - **The proposal.** `ship.propose_move` checks all of it again and gets a
//!   live `move.check` from the target: through the gateway when this machine
//!   reaches it, otherwise from the target's next collect pass. Only then is
//!   one captain decision written from the owner, on its duty task.
//! - **The yes.** The operator's own `move_clean` or `move_keep` reply records
//!   an approval in the registry of the machine where it was written
//!   (`answered`); mail never does. The exchange carries approvals from the
//!   machine holding the captain's desk to the machine on the link's linking
//!   side, which runs exactly that move once (`tick`), deletes the old folder
//!   when told to, and says how it ended. The proposing machine applies the
//!   outcome: notices, backoff and cooldowns.
use crate::store::{Caller, Store, identifier, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The alerts that count as pressure: disk only.
pub(crate) const COUNTING: &[&str] = &["disk_floor", "disk_trend"];
const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 60 * MINUTE_MS;
const DAY_MS: i64 = 24 * HOUR_MS;
/// A proposal waiting for a target that cannot be reached from here.
const CHECKING_MS: i64 = 10 * MINUTE_MS;
/// An unanswered proposal expires, which counts as "not now".
const OPEN_MS: i64 = 48 * HOUR_MS;
/// How long the captain's yes may wait for the machine that runs the move.
const APPROVAL_MS: i64 = 6 * HOUR_MS;
/// A yes the proposing machine has seen, with no outcome after this long,
/// lapsed on the way.
const LAPSE_MS: i64 = APPROVAL_MS + HOUR_MS;
/// After "not now", doubling per consecutive one.
const BACKOFF_MS: i64 = DAY_MS;
const MAX_BACKOFF_MS: i64 = 7 * DAY_MS;
/// After a move refused when it ran.
const SHIP_COOLDOWN_MS: i64 = DAY_MS;
const MACHINE_COOLDOWN_MS: i64 = 6 * HOUR_MS;
/// Eligibility windows.
const MOVED_RECENTLY_MS: i64 = DAY_MS;
const WOKE_RECENTLY_MS: i64 = HOUR_MS;
const DEFERRAL_WINDOW_MS: i64 = DAY_MS;
/// Closed proposals the exchange still carries, so the other side learns how
/// they ended, and how long an answered check is kept for it.
const CARRY_CLOSED_MS: i64 = 7 * DAY_MS;
const CHECK_KEPT_MS: i64 = HOUR_MS;
const MAX_CARRIED: usize = 16;
/// Target checks run in one pass, for proposals from machines this one reaches.
const CHECKS_PER_PASS: usize = 2;
const MAX_WHY: usize = 500;
/// A gateway call for a check or an eligibility test, which probes and lists
/// remotes, as `move.check` does inside a move.
const SLOW_CALL: Duration = Duration::from_secs(120);
/// Proposal states. The proposing machine owns the first set; the machine
/// running the move owns `moving`, `moved` and `refused`.
const OPEN_STATES: &[&str] = &["checking", "open", "approved", "moving"];
const PROPOSER_STATES: &[&str] = &[
    "checking",
    "open",
    "approved",
    "not_now",
    "never",
    "expired",
    "unreachable",
    "unfit",
];
const MOVER_STATES: &[&str] = &["moving", "moved", "refused", "interrupted"];
const MOVE_OPTIONS: &[&str] = &["move_clean", "move_keep"];
/// A pin "Never this ship" sets.
const NEVER_REASON: &str = "the captain declined moving it";

pub(crate) fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS machine_pressure(key TEXT PRIMARY KEY,
           raised_ms INTEGER NOT NULL, restarted_ms INTEGER, told_ms INTEGER);
         CREATE TABLE IF NOT EXISTS placement_proposals(id TEXT PRIMARY KEY, home_machine TEXT,
           ship TEXT NOT NULL, from_machine TEXT NOT NULL, to_machine TEXT NOT NULL,
           state TEXT NOT NULL, record TEXT NOT NULL, created_ms INTEGER NOT NULL,
           updated_ms INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS placement_approvals(proposal TEXT PRIMARY KEY,
           ship TEXT NOT NULL, from_machine TEXT NOT NULL, to_machine TEXT NOT NULL,
           binding TEXT NOT NULL, cleanup TEXT NOT NULL CHECK(cleanup IN ('move_clean','move_keep')),
           approved_ms INTEGER NOT NULL, expires_ms INTEGER NOT NULL, used_ms INTEGER, origin TEXT);
         CREATE TABLE IF NOT EXISTS placement_holds(only INTEGER PRIMARY KEY CHECK(only=1),
           not_now INTEGER NOT NULL DEFAULT 0, backoff_until_ms INTEGER, cooldown_until_ms INTEGER);
         CREATE TABLE IF NOT EXISTS placement_cooldowns(ship TEXT PRIMARY KEY, until_ms INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS placement_checks(id TEXT PRIMARY KEY, machine TEXT NOT NULL,
           answer TEXT NOT NULL, checked_ms INTEGER NOT NULL);",
    )?;
    Ok(())
}

fn immediate(conn: &Connection) -> Result<Transaction<'_>> {
    Ok(Transaction::new_unchecked(
        conn,
        TransactionBehavior::Immediate,
    )?)
}

fn has_table(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |r| r.get(0),
    )?)
}

/// A size as the captain reads it.
fn size(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    let mib = bytes as f64 / MIB;
    if mib >= 1024.0 {
        format!("{:.1} GiB", mib / 1024.0)
    } else {
        format!("{} MiB", mib.round() as u64)
    }
}

/// A span as the captain reads it.
fn span(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 2 * HOUR_MS {
        let minutes = (ms + MINUTE_MS - 1) / MINUTE_MS;
        format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" })
    } else if ms < 3 * DAY_MS {
        format!("{} hours", ms / HOUR_MS)
    } else {
        format!("{} days", ms / DAY_MS)
    }
}

/// The bytes under a path, without following symbolic links.
pub(crate) fn tree_bytes(path: &Path) -> u64 {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !metadata.is_dir() {
        return metadata.len();
    }
    fs::read_dir(path)
        .map(|entries| entries.flatten().map(|e| tree_bytes(&e.path())).sum())
        .unwrap_or(0)
}

/// This machine's admission settings, read from its host ledger through any
/// home here; the defaults when none can be read.
fn settings(conn: &Connection) -> crate::admission::Settings {
    (|| -> Result<crate::admission::Settings> {
        let Some(member) = crate::fleet::members(conn)?
            .into_iter()
            .find(|m| m.home.is_dir())
        else {
            return Ok(crate::admission::Settings::default());
        };
        let store = Store::open(&member.home)?;
        crate::admission::settings(&crate::jobs::host_ledger(&store)?)
    })()
    .unwrap_or_default()
}

/// Free disk where the fleet keeps its registry and ships, and the floor
/// this machine keeps free.
pub(crate) fn room(conn: &Connection) -> (Option<u64>, u64) {
    let free = crate::fleet::registry_path().ok().and_then(|registry| {
        crate::admission::disk(registry.parent()?)
            .ok()
            .map(|(free, _)| free)
    });
    (free, settings(conn).min_free_disk_bytes)
}

// ---- Pressure -------------------------------------------------------------

/// Keep the machine's pressure clock in step with the owner's alerts: a
/// counting key raised now keeps the row from when it was first raised, and
/// one no longer raised loses its row, so raising it again starts over.
pub(crate) fn note_pressure(conn: &Connection, raised: &[&str]) -> Result<()> {
    let now = Store::now_ms();
    let tx = immediate(conn)?;
    for key in COUNTING {
        if raised.contains(key) {
            tx.execute(
                "INSERT INTO machine_pressure(key, raised_ms) VALUES(?1, ?2)
                 ON CONFLICT(key) DO NOTHING",
                params![key, now],
            )?;
        } else {
            tx.execute("DELETE FROM machine_pressure WHERE key=?1", [key])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// One counting alert as the machine keeps it, and when its clock started.
struct Pressed {
    key: String,
    since_ms: i64,
    told: bool,
}

fn pressed(conn: &Connection) -> Result<Vec<Pressed>> {
    let mut statement = conn.prepare(
        "SELECT key, MAX(raised_ms, COALESCE(restarted_ms, 0)), told_ms IS NOT NULL
         FROM machine_pressure ORDER BY key",
    )?;
    let rows = statement.query_map([], |r| {
        Ok(Pressed {
            key: r.get(0)?,
            since_ms: r.get(1)?,
            told: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The counting keys raised on this machine now, for a check or an exchange.
pub(crate) fn pressure_keys(conn: &Connection) -> Vec<String> {
    pressed(conn)
        .map(|all| all.into_iter().map(|p| p.key).collect())
        .unwrap_or_default()
}

/// The counting key held longest past the threshold: the machine is under
/// persistent pressure.
fn persistent(conn: &Connection, now: i64) -> Result<Option<Pressed>> {
    let after = settings(conn).pressure_after_secs as i64 * 1000;
    Ok(pressed(conn)?
        .into_iter()
        .filter(|p| now - p.since_ms >= after)
        .min_by_key(|p| p.since_ms))
}

/// Counting keys that became persistent since the owner was last told, now
/// marked told: each one's notice gains the sentence about `ship.placement`.
pub(crate) fn newly_persistent(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let now = Store::now_ms();
    let after = settings(conn).pressure_after_secs as i64 * 1000;
    let mut told = vec![];
    for pressed in pressed(conn)? {
        if !pressed.told && now - pressed.since_ms >= after {
            conn.execute(
                "UPDATE machine_pressure SET told_ms=?2 WHERE key=?1",
                params![pressed.key, now],
            )?;
            told.push((pressed.key, now - pressed.since_ms));
        }
    }
    Ok(told)
}

/// The sentence a persistent key's notice gains.
pub(crate) fn persistent_sentence(held_ms: i64) -> String {
    format!(
        "This has lasted {}; `ship.placement` lists ships another machine could host.",
        span(held_ms)
    )
}

/// After a moved ship's old folder is deleted, every pressure clock here
/// starts again: another proposal waits a full `pressure_after_secs` from
/// the cleanup, not from the first alert.
fn restart_clocks(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE machine_pressure SET restarted_ms=?1, told_ms=NULL",
        [Store::now_ms()],
    )?;
    Ok(())
}

// ---- Proposals ------------------------------------------------------------

/// A proposal as a registry keeps it: proposed here (`home` none), or a copy
/// of one a linked machine proposed, kept by the machine that would run it.
#[derive(Clone, Debug)]
struct Proposal {
    id: String,
    home: Option<String>,
    state: String,
    record: Value,
    created_ms: i64,
    updated_ms: i64,
}

impl Proposal {
    fn text(&self, key: &str) -> &str {
        self.record[key].as_str().unwrap_or("")
    }
}

const PROPOSAL_COLUMNS: &str = "id, home_machine, state, record, created_ms, updated_ms";

fn proposal_row(r: &rusqlite::Row) -> rusqlite::Result<(Proposal, String)> {
    Ok((
        Proposal {
            id: r.get(0)?,
            home: r.get(1)?,
            state: r.get(2)?,
            record: Value::Null,
            created_ms: r.get(4)?,
            updated_ms: r.get(5)?,
        },
        r.get(3)?,
    ))
}

fn proposals(
    conn: &Connection,
    condition: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<Proposal>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {PROPOSAL_COLUMNS} FROM placement_proposals WHERE {condition}
         ORDER BY updated_ms DESC LIMIT 64"
    ))?;
    let rows = statement.query_map(args, proposal_row)?;
    rows.map(|row| {
        let (mut proposal, record) = row?;
        proposal.record = serde_json::from_str(&record)?;
        Ok(proposal)
    })
    .collect()
}

fn load(conn: &Connection, id: &str) -> Result<Option<Proposal>> {
    Ok(proposals(conn, "id=?1", &[&id])?.into_iter().next())
}

fn save(conn: &Connection, proposal: &Proposal) -> Result<()> {
    let mut record = proposal.record.clone();
    record["state"] = json!(proposal.state);
    conn.execute(
        "INSERT INTO placement_proposals(id, home_machine, ship, from_machine, to_machine, state,
           record, created_ms, updated_ms) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(id) DO UPDATE SET state=excluded.state, record=excluded.record,
           updated_ms=excluded.updated_ms",
        params![
            proposal.id,
            proposal.home,
            proposal.text("ship"),
            proposal.text("from"),
            proposal.text("to"),
            proposal.state,
            record.to_string(),
            proposal.created_ms,
            Store::now_ms()
        ],
    )?;
    Ok(())
}

/// Change a proposal only if it is still in one of `expect`, re-read under
/// the registry's write lock: the proposing machine's owner pass and the
/// mover's pass may both look at the same row.
fn update(
    conn: &Connection,
    id: &str,
    expect: &[&str],
    change: impl FnOnce(&mut Proposal),
) -> Result<Option<Proposal>> {
    let tx = immediate(conn)?;
    let Some(mut proposal) = load(&tx, id)? else {
        return Ok(None);
    };
    if !expect.contains(&proposal.state.as_str()) {
        return Ok(None);
    }
    change(&mut proposal);
    save(&tx, &proposal)?;
    tx.commit()?;
    Ok(Some(proposal))
}

/// What the captain's yes is bound to: the proposal, its two machines and
/// the target's check answer the captain was shown.
fn binding(record: &Value) -> String {
    crate::core::hash(&json!([
        record["id"],
        record["ship"],
        record["from"],
        record["to"],
        record["check"]
    ]))
}

/// The role a linked machine has in this registry: `collects` when this one
/// reaches it, `collected` when it reaches this one.
fn role(conn: &Connection, machine: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT role FROM machines WHERE name=?1", [machine], |r| {
            r.get(0)
        })
        .optional()?)
}

/// The name `machine` knows this one by: the name this machine gave when it
/// linked it, or the name the linking machine says it uses for this one.
fn known_there(conn: &Connection, machine: &str) -> Result<String> {
    let (known_as, calls_here): (Option<String>, Option<String>) = conn.query_row(
        "SELECT known_as, calls_here FROM machines WHERE name=?1",
        [machine],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(known_as
        .or(calls_here)
        .unwrap_or_else(crate::machines::this_machine))
}

/// The ship looking after this machine and its home, if it is here.
fn owner_home(conn: &Connection) -> Result<Option<(String, PathBuf)>> {
    let Some(owner) = crate::machines::owner(conn)? else {
        return Ok(None);
    };
    Ok(crate::fleet::members(conn)?
        .into_iter()
        .find(|m| m.name == owner && m.home.is_dir())
        .map(|m| (m.name, m.home)))
}

fn duty_task(store: &Store, ship: &str) -> Option<String> {
    crate::mail::ship(store, ship)
        .ok()
        .flatten()
        .and_then(|s| s["first_mate"]["duty_task"].as_str().map(str::to_owned))
        .filter(|duty| !duty.is_empty())
}

/// What holds this machine back from proposing now.
fn holds(conn: &Connection, now: i64) -> Result<Vec<String>> {
    let mut holds = vec![];
    for open in proposals(
        conn,
        "home_machine IS NULL AND state IN ('checking','open','approved','moving')",
        &[],
    )? {
        holds.push(format!(
            "proposal {} to move {} to {} is {}; one proposal is open at a time",
            open.id,
            open.text("ship"),
            open.text("to"),
            open.state
        ));
    }
    let (streak, backoff, cooldown): (i64, Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT not_now, backoff_until_ms, cooldown_until_ms FROM placement_holds",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .unwrap_or((0, None, None));
    if let Some(until) = backoff.filter(|until| *until > now) {
        holds.push(format!(
            "the captain said not now ({streak} in a row); nothing is proposed for another {}",
            span(until - now)
        ));
    }
    if let Some(until) = cooldown.filter(|until| *until > now) {
        holds.push(format!(
            "a move refused when it ran holds proposals for another {}",
            span(until - now)
        ));
    }
    for moved in proposals(
        conn,
        "home_machine IS NULL AND state='moved' AND json_extract(record,'$.cleaned_ms') IS NULL",
        &[],
    )? {
        let ship = moved.text("ship");
        holds.push(format!(
            "the folder {ship} left behind when it moved is still here; `privateer ship clean-moved {ship}` deletes it"
        ));
    }
    Ok(holds)
}

/// After "not now", the hold for the `streak`th consecutive one.
fn backoff_ms(streak: i64) -> i64 {
    (BACKOFF_MS << (streak - 1).clamp(0, 8)).min(MAX_BACKOFF_MS)
}

fn ensure_holds(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO placement_holds(only, not_now) VALUES(1, 0) ON CONFLICT(only) DO NOTHING",
        [],
    )?;
    Ok(())
}

// ---- Eligibility and targets ------------------------------------------------

fn count(
    conn: &Connection,
    table: &str,
    condition: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<i64> {
    if !has_table(conn, table)? {
        return Ok(0);
    }
    Ok(conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE {condition}"),
        args,
        |r| r.get(0),
    )?)
}

/// Whether a ship on this machine may be proposed, why not, its size and
/// what a move of it carries. Served to the machine running an approved
/// move as `move.eligible`, so it can test the ship again just before.
pub(crate) fn eligibility(conn: &Connection, ship: &str, now: i64) -> Result<Value> {
    identifier(ship)?;
    let member = crate::fleet::members(conn)?
        .into_iter()
        .find(|m| m.name == ship && m.home.is_dir())
        .with_context(|| format!("no ship named {ship} on this machine"))?;
    let inspected = crate::moves::serve("move.inspect", &json!({"ship": ship}))?;
    let mut reasons: Vec<String> = inspected["not_idle"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|r| format!("not idle: {r}"))
        .collect();
    let store = Store::open(&member.home)?;
    let db = store.connection();
    let running = count(db, "tasks", "owner=?1 AND status='running'", &[&ship])?;
    if running > 0 {
        reasons.push(format!("not idle: {running} of its tasks running"));
    }
    let woke = count(
        db,
        "wakes",
        "ship=?1 AND created_ms>=?2",
        &[&ship, &(now - WOKE_RECENTLY_MS)],
    )?;
    if woke > 0 {
        reasons.push("not idle: its first mate woke within the last hour".into());
    }
    let ready = count(
        db,
        "task_schedule",
        "json_extract(record,'$.owner')=?1
           AND json_extract(record,'$.state') IN ('ready','offered','claimed','executing')",
        &[&ship],
    )?;
    if ready > 0 {
        reasons.push(format!("not idle: {ready} scheduled work ready or running"));
    }
    let due = count(
        db,
        "task_schedule",
        "json_extract(record,'$.owner')=?1 AND json_extract(record,'$.state')='waiting'
           AND json_extract(record,'$.not_before_ms')>0
           AND json_extract(record,'$.not_before_ms')<=?2",
        &[&ship, &(now + DEFERRAL_WINDOW_MS)],
    )?;
    if due > 0 {
        reasons.push(format!("not idle: {due} deferred tasks due within a day"));
    }
    if let Some(pin) = inspected["pinned"].as_object() {
        reasons.push(format!(
            "pinned by {}: {}",
            pin.get("by").and_then(Value::as_str).unwrap_or("someone"),
            pin.get("reason").and_then(Value::as_str).unwrap_or("")
        ));
    }
    if let Some(moved) = inspected["last_moved_ms"]
        .as_i64()
        .filter(|at| now - at < MOVED_RECENTLY_MS)
    {
        reasons.push(format!("moved here {} ago", span(now - moved)));
    }
    let cooling: Option<i64> = conn
        .query_row(
            "SELECT until_ms FROM placement_cooldowns WHERE ship=?1 AND until_ms>?2",
            params![ship, now],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(until) = cooling {
        reasons.push(format!(
            "a move of it was refused when it ran; not proposed for another {}",
            span(until - now)
        ));
    }
    if crate::machines::owner(conn)?.as_deref() == Some(ship) {
        reasons.push("it is this machine's owner".into());
    }
    if crate::machines::captain_home(conn)?.is_some_and(|(captain, _)| captain == ship) {
        reasons.push("it is the captain's home (the liaison)".into());
    }
    let last_worked: Option<i64> = if has_table(db, "wakes")? {
        db.query_row(
            "SELECT MAX(created_ms) FROM wakes WHERE ship=?1",
            [ship],
            |r| r.get(0),
        )?
    } else {
        None
    };
    let folder = member.home.parent().context("ship folder")?;
    let project_bytes = tree_bytes(&folder.join("projects"));
    Ok(
        json!({"ship": ship, "eligible": reasons.is_empty(), "reasons": reasons,
        "folder_bytes": inspected["folder_bytes"], "project_bytes": project_bytes,
        "project_count": inspected["project_count"], "requires": inspected["requires"],
        "first_mate": inspected["first_mate"], "projects": inspected["projects"],
        "last_worked_ms": last_worked, "jobs_last_day": jobs_last_day(&store, now)}),
    )
}

/// A ship's share of the host ledger's last day of jobs.
fn jobs_last_day(store: &Store, now: i64) -> Value {
    (|| -> Result<Value> {
        let ledger = crate::jobs::host_ledger(store)?;
        let prefix = format!("{}:", store.home().display());
        let (runs, ms): (i64, i64) = ledger.query_row(
            "SELECT COUNT(*), COALESCE(SUM(duration_ms), 0) FROM runs
             WHERE finished_ms>=?1 AND substr(key, 1, length(?2))=?2",
            params![now - DAY_MS, prefix],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let all: i64 = ledger.query_row(
            "SELECT COALESCE(SUM(duration_ms), 0) FROM runs WHERE finished_ms>=?1",
            [now - DAY_MS],
            |r| r.get(0),
        )?;
        Ok(json!({"runs": runs, "duration_ms": ms,
            "share": if all > 0 { ms as f64 / all as f64 } else { 0.0 }}))
    })()
    .unwrap_or(Value::Null)
}

fn names(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// The disk a move of an eligibility report needs on its target.
fn needed(eligible: &Value) -> u64 {
    eligible["folder_bytes"].as_u64().unwrap_or(0) + eligible["project_bytes"].as_u64().unwrap_or(0)
}

/// Each machine at the other end of one of this machine's links, and
/// whether it could host a ship requiring `requires` and needing `bytes`,
/// from what it last reported.
fn targets(conn: &Connection, requires: &[String], bytes: u64) -> Result<Vec<Value>> {
    let mut statement =
        conn.prepare("SELECT name, role, offers, placement FROM machines ORDER BY name")?;
    let rows = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = vec![];
    for (name, role, offers, report) in rows {
        let mut why = vec![];
        let offers: Option<Vec<String>> = offers
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .map(|o| names(&o["offers"]));
        if !requires.is_empty() {
            match &offers {
                None => why.push("has not said what it offers".to_owned()),
                Some(offers) => {
                    let missing: Vec<&str> = requires
                        .iter()
                        .filter(|r| !offers.contains(r))
                        .map(String::as_str)
                        .collect();
                    if !missing.is_empty() {
                        why.push(format!("does not offer {}", missing.join(", ")));
                    }
                }
            }
        }
        match report.and_then(|raw| serde_json::from_str::<Value>(&raw).ok()) {
            None => why.push("has not reported its disk (it may run an older privateer)".into()),
            Some(report) => {
                let pressure = names(&report["pressure"]);
                if !pressure.is_empty() {
                    why.push(format!(
                        "is under disk pressure itself ({})",
                        pressure.join(", ")
                    ));
                }
                match (
                    report["free_disk_bytes"].as_u64(),
                    report["floor_bytes"].as_u64(),
                ) {
                    (Some(free), Some(floor)) if free < floor.saturating_add(bytes) => {
                        why.push(format!(
                            "has no room: {} free, {} must stay free, and the ship needs {}",
                            size(free),
                            size(floor),
                            size(bytes)
                        ));
                    }
                    (Some(_), Some(_)) => {}
                    _ => why.push("has not reported its free disk".into()),
                }
            }
        }
        out.push(json!({"machine": name,
            "runs_move": if role == "collects" { "this machine".to_owned() } else { name.clone() },
            "eligible": why.is_empty(), "why": why}));
    }
    Ok(out)
}

/// What in a target's check answer keeps the move from happening; `None`
/// when it may.
fn check_refusal(check: &Value, bytes: u64, to: &str) -> Option<String> {
    if let Some(error) = check["error"].as_str() {
        return Some(format!("{to} could not check: {error}"));
    }
    let mut reasons = vec![];
    if check["unmet"].as_array().is_some_and(|u| !u.is_empty()) {
        reasons.push(format!(
            "{to} does not offer {}",
            crate::moves::unmet_text(&check["unmet"])
        ));
    }
    reasons.extend(names(&check["problems"]));
    let pressure = names(&check["pressure"]);
    if !pressure.is_empty() {
        reasons.push(format!(
            "{to} is under disk pressure itself ({})",
            pressure.join(", ")
        ));
    }
    if let (Some(free), Some(floor)) = (
        check["free_disk_bytes"].as_u64(),
        check["floor_bytes"].as_u64(),
    ) && free < floor.saturating_add(bytes)
    {
        reasons.push(format!(
            "{to} has no room: {} free, {} must stay free, and the ship needs {}",
            size(free),
            size(floor),
            size(bytes)
        ));
    }
    (!reasons.is_empty()).then(|| reasons.join("; "))
}

// ---- The owner's commands ---------------------------------------------------

/// The owner, or the operator.
fn authorize(conn: &Connection, store: &Store, command: &str) -> Result<Option<String>> {
    match store.caller() {
        Caller::Operator => Ok(None),
        Caller::Agent { actor } => {
            ensure!(
                crate::machines::owner(conn)?.as_deref() == Some(actor.as_str()),
                "{command} is for this machine's owner; {actor} is not it"
            );
            Ok(Some(actor.clone()))
        }
    }
}

/// `ship.placement {}`: the pressure, the holds, and for each ship here
/// whether it may be proposed and which linked machines could host it.
fn placement(store: &Store) -> Result<Value> {
    let conn = crate::fleet::registry()?;
    authorize(&conn, store, "ship.placement")?;
    let now = Store::now_ms();
    let after = settings(&conn).pressure_after_secs;
    let pressure: Vec<Value> = pressed(&conn)?
        .into_iter()
        .map(|p| {
            json!({"key": p.key, "since_ms": p.since_ms, "held_secs": (now - p.since_ms) / 1000,
                "persistent": now - p.since_ms >= after as i64 * 1000})
        })
        .collect();
    let mut ships = vec![];
    for member in crate::fleet::members(&conn)? {
        if !member.home.is_dir() {
            continue;
        }
        let mut entry = eligibility(&conn, &member.name, now).unwrap_or_else(|error| {
            json!({"ship": member.name, "eligible": false, "reasons": [format!("{error:#}")]})
        });
        if entry["eligible"] == true {
            let all = targets(&conn, &names(&entry["requires"]), needed(&entry))?;
            entry["targets"] = json!(all);
        }
        for key in ["first_mate", "projects"] {
            if let Some(fields) = entry.as_object_mut() {
                fields.remove(key);
            }
        }
        ships.push(entry);
    }
    let recent = proposals(
        &conn,
        "home_machine IS NULL AND updated_ms>=?1",
        &[&(now - CARRY_CLOSED_MS)],
    )?;
    Ok(json!({"machine": crate::machines::this_machine(),
        "pressure_after_secs": after, "pressure": pressure,
        "persistent": persistent(&conn, now)?.is_some(),
        "holds": holds(&conn, now)?, "ships": ships,
        "proposals": recent.iter().map(|p| p.record.clone()).collect::<Vec<_>>()}))
}

/// `ship.propose_move {name, to, why}`, for the machine's owner only: check
/// the holds, the ship and the target again, get the target's live check,
/// and write one captain decision; or, when the target reaches this machine
/// and cannot be asked from here, wait for its next pass to check.
fn propose(store: &Store, args: &Value) -> Result<Value> {
    let conn = crate::fleet::registry()?;
    let Caller::Agent { actor } = store.caller() else {
        bail!("ship.propose_move is for this machine's owner, in its own name");
    };
    let owner = actor.clone();
    ensure!(
        crate::machines::owner(&conn)?.as_deref() == Some(owner.as_str()),
        "ship.propose_move is for this machine's owner; {owner} is not it"
    );
    // `name`, as ship.move takes it: an agent's `ship` argument names itself.
    let ship = required(args, "name")?;
    identifier(ship)?;
    let to = required(args, "to")?;
    identifier(to)?;
    let why = required(args, "why")?.trim();
    ensure!(
        !why.is_empty() && why.chars().count() <= MAX_WHY && !why.chars().any(char::is_control),
        "why is one line of 1 to {MAX_WHY} characters"
    );
    let now = Store::now_ms();
    let pressed = persistent(&conn, now)?.with_context(|| {
        format!(
            "this machine is not under persistent disk pressure (an alert raised for {}); nothing is proposed",
            span(settings(&conn).pressure_after_secs as i64 * 1000)
        )
    })?;
    let held = holds(&conn, now)?;
    ensure!(
        held.is_empty(),
        "nothing is proposed now: {}",
        held.join("; ")
    );
    let eligible = eligibility(&conn, ship, now)?;
    ensure!(
        eligible["eligible"] == true,
        "{ship} may not be proposed: {}",
        names(&eligible["reasons"]).join("; ")
    );
    let requires = names(&eligible["requires"]);
    let bytes = needed(&eligible);
    let target = targets(&conn, &requires, bytes)?
        .into_iter()
        .find(|t| t["machine"] == to)
        .with_context(|| {
            format!("{to} is not linked with this machine; a ship moves only across one link")
        })?;
    ensure!(
        target["eligible"] == true,
        "{to} cannot host {ship}: {}",
        names(&target["why"]).join("; ")
    );
    let reaches = role(&conn, to)?.as_deref() == Some("collects");
    let (free, floor) = room(&conn);
    let id = uuid::Uuid::new_v4().to_string();
    let record = json!({"id": id, "ship": ship, "from": known_there(&conn, to)?, "to": to,
        "why": why, "owner": owner, "created_ms": now,
        "pressure": {"key": pressed.key, "held_secs": (now - pressed.since_ms) / 1000,
            "free_disk_bytes": free, "floor_bytes": floor},
        "carries": {"requires": requires, "first_mate": eligible["first_mate"],
            "projects": eligible["projects"], "folder_bytes": eligible["folder_bytes"],
            "project_bytes": eligible["project_bytes"], "project_count": eligible["project_count"]},
        "last_worked_ms": eligible["last_worked_ms"]});
    let mut proposal = Proposal {
        id: id.clone(),
        home: None,
        state: "checking".into(),
        record,
        created_ms: now,
        updated_ms: now,
    };
    if reaches {
        let check = crate::machines::call_reached(
            to,
            "move.check",
            check_request(&proposal.record),
            Some(SLOW_CALL),
        )?;
        if let Some(refused) = check_refusal(&check, bytes, to) {
            bail!("{ship} cannot move to {to}: {refused}");
        }
        proposal.record["check"] = check;
        proposal.record["binding"] = json!(binding(&proposal.record));
        proposal.record["decision"] = json!(uuid::Uuid::new_v4().to_string());
        proposal.state = "open".into();
    }
    // One open proposal per machine, decided under the registry's write lock.
    let tx = immediate(&conn)?;
    let held = holds(&tx, now)?;
    ensure!(
        held.is_empty(),
        "nothing is proposed now: {}",
        held.join("; ")
    );
    save(&tx, &proposal)?;
    tx.commit()?;
    if proposal.state == "open"
        && let Err(error) = write_decision(store, &proposal)
    {
        conn.execute("DELETE FROM placement_proposals WHERE id=?1", [&id])?;
        return Err(error);
    }
    store.audit("placement.proposed", &proposal.record)?;
    Ok(
        json!({"proposal": id, "state": proposal.state, "ship": ship, "to": to,
        "decision": proposal.record["decision"],
        "note": if reaches {
            "The captain has the decision; nothing moves without the captain's yes."
        } else {
            "The target reaches this machine and checks at its next pass; the decision is written once it answers."
        }}),
    )
}

/// The arguments of a target's `move.check` for a proposal.
fn check_request(record: &Value) -> Value {
    let carries = &record["carries"];
    json!({"ship": record["ship"], "requires": carries["requires"],
        "first_mate": carries["first_mate"], "projects": carries["projects"]})
}

/// The decision the captain answers, composed by the runtime so every
/// proposal has the same shape, written from the owner on its duty task.
fn write_decision(owner_store: &Store, proposal: &Proposal) -> Result<()> {
    let record = &proposal.record;
    let (ship, from, to) = (
        proposal.text("ship"),
        proposal.text("from"),
        proposal.text("to"),
    );
    let owner = proposal.text("owner");
    let carries = &record["carries"];
    let folder = carries["folder_bytes"].as_u64().unwrap_or(0);
    let projects = carries["project_bytes"].as_u64().unwrap_or(0);
    let count = carries["project_count"].as_u64().unwrap_or(0);
    let pressure = &record["pressure"];
    let held = span(pressure["held_secs"].as_i64().unwrap_or(0) * 1000);
    let disk = format!(
        "{} free, {} floor",
        pressure["free_disk_bytes"]
            .as_u64()
            .map_or_else(|| "unknown".into(), size),
        size(pressure["floor_bytes"].as_u64().unwrap_or(0))
    );
    let pressed = if pressure["key"] == "disk_trend" {
        format!("{from}'s free disk has been falling toward its floor for {held} ({disk}).")
    } else {
        format!("{from} has been below its disk floor for {held} ({disk}).")
    };
    let worked = match record["last_worked_ms"].as_i64() {
        Some(at) => format!("last worked {} ago", span(Store::now_ms() - at)),
        None => "with no recorded work".into(),
    };
    let requires = names(&carries["requires"]);
    let offers = if requires.is_empty() {
        format!("{ship} requires nothing in particular of a machine.")
    } else {
        format!(
            "{to} offers everything it requires ({}).",
            requires.join(", ")
        )
    };
    let minutes = 2 + count * 2 + folder / (50 * 1024 * 1024);
    let clones = if count == 0 {
        "it has no projects to clone again".to_owned()
    } else {
        format!(
            "its projects are cloned again on {to} (about {} there)",
            size(projects)
        )
    };
    let question = format!("Move {ship} from {from} to {to}?");
    let body = format!(
        "**{question}** {pressed} {ship} is idle, {worked}. {offers}\n\
         **Cost:** {ship} is down for the move (about {minutes} minutes for {} and {count} project{}); \
         {clones}; build caches start cold; its first mate's session starts fresh on {to}. \
         **Disk on {from} is freed only when the old folder is deleted** ({}: the folder and its projects; \
         all projects are pushed, so only ignored files and caches are lost). *Why ({owner}):* {}",
        size(folder),
        if count == 1 { "" } else { "s" },
        size(folder + projects),
        proposal.text("why")
    );
    let summary: String = format!("{pressed} Why ({owner}): {}", proposal.text("why"))
        .chars()
        .take(500)
        .collect();
    let mut payload = json!({
        "decision": {"question": question, "why": summary, "options": [
            {"id": "move_clean", "label": "Move it and delete the old folder",
             "what_happens": format!("The runtime moves {ship} to {to} once, then deletes its old folder on {from}, freeing about {}.", size(folder + projects))},
            {"id": "move_keep", "label": "Move it, keep the old folder",
             "what_happens": format!("The runtime moves {ship} to {to} once; its old folder stays on {from} until `privateer ship clean-moved {ship}` deletes it, and {from} proposes nothing until then.")},
            {"id": "not_now", "label": "Not now",
             "what_happens": format!("Nothing moves; {from} proposes nothing for a day, longer after each \"not now\" in a row.")},
            {"id": "never", "label": "Never this ship",
             "what_happens": format!("Nothing moves; {ship} is pinned where it is by the captain, and only the captain unpins it.")}
        ]},
        "placement": {"id": proposal.id, "ship": ship, "from": from, "to": to,
            "pressure": {"key": pressure["key"], "held_secs": pressure["held_secs"]},
            "folder_bytes": folder, "project_bytes": projects, "project_count": count,
            "check": record["check"], "binding": record["binding"]}
    });
    crate::mail::typed_payload("decision", &mut payload)?;
    let mut message = json!({"id": record["decision"], "from": owner, "to": crate::mail::CAPTAIN,
        "kind": "decision", "body": body, "payload": payload, "urgency": "normal"});
    if let Some(duty) = duty_task(owner_store, owner) {
        message["task_id"] = json!(duty);
    }
    crate::mail::record_runtime(owner_store, &message)?;
    Ok(())
}

// ---- The captain's yes --------------------------------------------------------

/// After the operator's own reply commits, in the process where the operator
/// wrote it: a `move_clean` or `move_keep` answer to a placement decision
/// records an approval in this machine's registry, bound to what the
/// decision showed. A reply that arrives as mail never comes here. Failures
/// are logged; the reply stands.
pub(crate) fn answered(store: &Store, request: &str, reply: &Value) {
    let result = (|| -> Result<Option<Value>> {
        let Some(choice) = reply["payload"]["answer"]["option_id"]
            .as_str()
            .filter(|c| MOVE_OPTIONS.contains(c))
        else {
            return Ok(None);
        };
        let Some(thread) = crate::mail::find(store, request)? else {
            return Ok(None);
        };
        let facts = &thread["payload"]["placement"];
        if thread["kind"] != "decision"
            || thread["to"] != crate::mail::CAPTAIN
            || !facts.is_object()
        {
            return Ok(None);
        }
        let field = |key: &str| -> Result<&str> {
            let value = required(facts, key)?;
            identifier(value)?;
            Ok(value)
        };
        let (id, ship, from, to) = (field("id")?, field("ship")?, field("from")?, field("to")?);
        let binding = required(facts, "binding")?;
        ensure!(
            binding.len() == 64 && binding.chars().all(|c| c.is_ascii_hexdigit()),
            "the decision's binding is not a digest"
        );
        let now = Store::now_ms();
        let conn = crate::fleet::registry()?;
        conn.execute(
            "INSERT INTO placement_approvals(proposal, ship, from_machine, to_machine, binding,
               cleanup, approved_ms, expires_ms) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(proposal) DO NOTHING",
            params![id, ship, from, to, binding, choice, now, now + APPROVAL_MS],
        )?;
        Ok(Some(
            json!({"proposal": id, "ship": ship, "from": from, "to": to,
            "cleanup": choice, "reply": reply["id"]}),
        ))
    })();
    match result {
        Ok(Some(approval)) => {
            let _ = store.audit("placement.approved", &approval);
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!(
                "privateer: the captain's answer to {request} was recorded but no move was approved: {error:#}"
            );
            let _ = store.audit(
                "placement.approval_failed",
                &json!({"request": request, "error": format!("{error:#}")}),
            );
        }
    }
}

/// One approval as a registry holds it.
struct Approval {
    proposal: String,
    ship: String,
    from: String,
    to: String,
    binding: String,
    cleanup: String,
    expires_ms: i64,
}

// ---- The exchange ---------------------------------------------------------------

/// What this machine tells `peer` about placement in each exchange and
/// collect, both ways: its own pressure and room, the proposals between the
/// two, the approvals written here, and checks it answered for the peer.
/// `None` when it cannot be read, so the field is left out.
pub(crate) fn frame(conn: &Connection, peer: &str) -> Option<Value> {
    (|| -> Result<Value> {
        let now = Store::now_ms();
        let (free, floor) = room(conn);
        let carried: Vec<Value> = proposals(
            conn,
            "((home_machine IS NULL AND to_machine=?1) OR home_machine=?1)
               AND (state IN ('checking','open','approved','moving') OR updated_ms>=?2)",
            &[&peer, &(now - CARRY_CLOSED_MS)],
        )?
        .into_iter()
        .take(MAX_CARRIED)
        .map(|p| {
            let mut record = p.record;
            record["state"] = json!(p.state);
            record
        })
        .collect();
        let mut statement = conn.prepare(
            "SELECT proposal, ship, from_machine, to_machine, binding, cleanup, approved_ms, expires_ms
             FROM placement_approvals WHERE origin IS NULL AND used_ms IS NULL AND expires_ms>?1
             ORDER BY approved_ms LIMIT 16",
        )?;
        let approvals = statement
            .query_map([now], |r| {
                Ok(json!({"proposal": r.get::<_, String>(0)?, "ship": r.get::<_, String>(1)?,
                    "from": r.get::<_, String>(2)?, "to": r.get::<_, String>(3)?,
                    "binding": r.get::<_, String>(4)?, "cleanup": r.get::<_, String>(5)?,
                    "approved_ms": r.get::<_, i64>(6)?, "expires_ms": r.get::<_, i64>(7)?}))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut statement = conn.prepare(
            "SELECT id, answer FROM placement_checks WHERE machine=?1 AND checked_ms>=?2 LIMIT 16",
        )?;
        let checks = statement
            .query_map(params![peer, now - CHECK_KEPT_MS], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter_map(|(id, answer)| {
                Some(json!({"id": id, "answer": serde_json::from_str::<Value>(&answer).ok()?}))
            })
            .collect::<Vec<_>>();
        Ok(json!({"you": peer, "pressure": pressure_keys(conn), "free_disk_bytes": free,
            "floor_bytes": floor, "proposals": carried, "approvals": approvals, "checks": checks}))
    })()
    .ok()
}

/// Take in what a linked machine said about placement. A peer too old to say
/// sends nothing, which changes nothing.
pub(crate) fn absorb(conn: &Connection, from: &str, frame: &Value) -> Result<()> {
    if !frame.is_object() {
        return Ok(());
    }
    let role = role(conn, from)?.context("no such linked machine")?;
    let pressure: Vec<String> = names(&frame["pressure"])
        .into_iter()
        .filter(|k| identifier(k).is_ok())
        .take(8)
        .collect();
    conn.execute(
        "UPDATE machines SET placement=?2 WHERE name=?1",
        params![
            from,
            json!({"pressure": pressure, "free_disk_bytes": frame["free_disk_bytes"].as_u64(),
                "floor_bytes": frame["floor_bytes"].as_u64(), "reported_ms": Store::now_ms()})
            .to_string()
        ],
    )?;
    // The machine that links this one says what it calls this one.
    if role == "collected"
        && let Some(you) = frame["you"].as_str().filter(|y| identifier(y).is_ok())
    {
        conn.execute(
            "UPDATE machines SET calls_here=?2 WHERE name=?1",
            params![from, you],
        )?;
    }
    // Approvals only from the machine holding this machine's captain desk.
    if crate::machines::holds_desk(from) {
        for approval in frame["approvals"].as_array().into_iter().flatten().take(16) {
            if let Err(error) = keep_approval(conn, from, approval) {
                eprintln!("privateer: an approval from {from} was not kept: {error:#}");
            }
        }
    }
    for carried in frame["proposals"]
        .as_array()
        .into_iter()
        .flatten()
        .take(MAX_CARRIED)
    {
        if let Err(error) = absorb_proposal(conn, from, carried) {
            eprintln!("privateer: a proposal from {from} was not kept: {error:#}");
        }
    }
    for check in frame["checks"].as_array().into_iter().flatten().take(16) {
        let Some(id) = check["id"].as_str() else {
            continue;
        };
        if let Err(error) = accept_check(conn, from, id, &check["answer"]) {
            eprintln!("privateer: the check {from} answered for {id} was not used: {error:#}");
        }
    }
    // Proposals from a machine this one reaches, waiting for this one's check.
    if role == "collects" {
        run_checks(conn, from)?;
    }
    Ok(())
}

fn keep_approval(conn: &Connection, origin: &str, approval: &Value) -> Result<()> {
    let field = |key: &str| -> Result<&str> {
        let value = required(approval, key)?;
        identifier(value)?;
        Ok(value)
    };
    let binding = required(approval, "binding")?;
    ensure!(
        binding.len() == 64 && binding.chars().all(|c| c.is_ascii_hexdigit()),
        "binding is not a digest"
    );
    let cleanup = required(approval, "cleanup")?;
    ensure!(MOVE_OPTIONS.contains(&cleanup), "unknown cleanup {cleanup}");
    let approved = approval["approved_ms"].as_i64().context("approved_ms")?;
    // Never longer than an approval lasts where it was written.
    let expires = approval["expires_ms"]
        .as_i64()
        .context("expires_ms")?
        .min(approved + APPROVAL_MS);
    conn.execute(
        "INSERT INTO placement_approvals(proposal, ship, from_machine, to_machine, binding, cleanup,
           approved_ms, expires_ms, origin) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(proposal) DO NOTHING",
        params![
            field("proposal")?,
            field("ship")?,
            field("from")?,
            field("to")?,
            binding,
            cleanup,
            approved,
            expires,
            origin
        ],
    )?;
    Ok(())
}

fn absorb_proposal(conn: &Connection, from: &str, carried: &Value) -> Result<()> {
    let id = required(carried, "id")?;
    identifier(id)?;
    for key in ["ship", "from", "to"] {
        identifier(required(carried, key)?)?;
    }
    let state = required(carried, "state")?;
    ensure!(
        PROPOSER_STATES.contains(&state) || MOVER_STATES.contains(&state),
        "unknown proposal state {state}"
    );
    ensure!(
        carried.to_string().len() <= 64 * 1024,
        "a proposal is limited to 64 KiB"
    );
    match load(conn, id)? {
        // Ours: the machine it targets ran it, or refused it.
        Some(ours) if ours.home.is_none() => {
            if ours.text("to") == from && matches!(state, "moved" | "refused" | "interrupted") {
                conclude(conn, id, state, carried["outcome"].clone())?;
            }
        }
        // A copy kept here: the proposing machine's own states replace it,
        // unless this machine already ran it.
        Some(copy) if copy.home.as_deref() == Some(from) => {
            if PROPOSER_STATES.contains(&state) && !MOVER_STATES.contains(&copy.state.as_str()) {
                let mut record = carried.clone();
                if let Some(fields) = record.as_object_mut() {
                    fields.remove("outcome");
                }
                let next = state.to_owned();
                update(conn, id, PROPOSER_STATES, move |p| {
                    p.record = record;
                    p.state = next;
                })?;
            }
        }
        Some(_) => {}
        None => {
            if PROPOSER_STATES.contains(&state) {
                let mut record = carried.clone();
                if let Some(fields) = record.as_object_mut() {
                    fields.remove("outcome");
                }
                let now = Store::now_ms();
                save(
                    conn,
                    &Proposal {
                        id: id.to_owned(),
                        home: Some(from.to_owned()),
                        state: state.to_owned(),
                        record,
                        created_ms: now,
                        updated_ms: now,
                    },
                )?;
            }
        }
    }
    Ok(())
}

/// Check proposals a machine this one reaches made to move a ship here,
/// which it could not ask itself; the answers ride the next exchange back.
fn run_checks(conn: &Connection, from: &str) -> Result<()> {
    let waiting = proposals(
        conn,
        "home_machine=?1 AND state='checking' AND id NOT IN (SELECT id FROM placement_checks)",
        &[&from],
    )?;
    for proposal in waiting.into_iter().take(CHECKS_PER_PASS) {
        let answer = crate::moves::serve("move.check", &check_request(&proposal.record))
            .unwrap_or_else(|error| json!({"error": format!("{error:#}")}));
        conn.execute(
            "INSERT OR REPLACE INTO placement_checks(id, machine, answer, checked_ms)
             VALUES(?1, ?2, ?3, ?4)",
            params![proposal.id, from, answer.to_string(), Store::now_ms()],
        )?;
    }
    conn.execute(
        "DELETE FROM placement_checks WHERE checked_ms<?1",
        [Store::now_ms() - CARRY_CLOSED_MS],
    )?;
    Ok(())
}

/// The target answered the check for one of this machine's proposals: write
/// the decision, or close the proposal when the target cannot host the ship.
fn accept_check(conn: &Connection, from: &str, id: &str, answer: &Value) -> Result<()> {
    let Some(proposal) = load(conn, id)? else {
        return Ok(());
    };
    if proposal.home.is_some() || proposal.state != "checking" || proposal.text("to") != from {
        return Ok(());
    }
    ensure!(answer.is_object(), "a check answer is an object");
    let carries = &proposal.record["carries"];
    let bytes = carries["folder_bytes"].as_u64().unwrap_or(0)
        + carries["project_bytes"].as_u64().unwrap_or(0);
    if let Some(refused) = check_refusal(answer, bytes, from) {
        conclude(conn, id, "unfit", json!({"why": refused}))?;
        return Ok(());
    }
    let Some((_, home)) = owner_home(conn)? else {
        conclude(
            conn,
            id,
            "unfit",
            json!({"why": "this machine has no owner here to propose it"}),
        )?;
        return Ok(());
    };
    let answer = answer.clone();
    let Some(opened) = update(conn, id, &["checking"], move |p| {
        p.record["check"] = answer;
        p.record["binding"] = json!(binding(&p.record));
        p.record["decision"] = json!(uuid::Uuid::new_v4().to_string());
        p.state = "open".into();
    })?
    else {
        return Ok(());
    };
    // Written once; the owner's pass writes it again if this failed.
    write_decision(&Store::open(&home)?, &opened)
}

// ---- Outcomes ----------------------------------------------------------------------

/// Close one of this machine's proposals, apply what its ending holds, and
/// tell the owner, and the captain on the decision's thread when a move ran
/// or was refused. Only an open proposal closes; the first ending stands.
fn conclude(conn: &Connection, id: &str, state: &str, outcome: Value) -> Result<bool> {
    let now = Store::now_ms();
    let tx = immediate(conn)?;
    let Some(mut proposal) = load(&tx, id)? else {
        return Ok(false);
    };
    if proposal.home.is_some() || !OPEN_STATES.contains(&proposal.state.as_str()) {
        return Ok(false);
    }
    proposal.state = state.to_owned();
    proposal.record["outcome"] = outcome;
    proposal.record["closed_ms"] = json!(now);
    ensure_holds(&tx)?;
    match state {
        "moved" => {
            tx.execute(
                "UPDATE placement_holds SET not_now=0, backoff_until_ms=NULL",
                [],
            )?;
            if proposal.record["outcome"]["cleaned"] == true {
                proposal.record["cleaned_ms"] = json!(now);
            }
        }
        "refused" | "interrupted" => {
            tx.execute(
                "INSERT INTO placement_cooldowns(ship, until_ms) VALUES(?1, ?2)
                 ON CONFLICT(ship) DO UPDATE SET until_ms=excluded.until_ms",
                params![proposal.text("ship"), now + SHIP_COOLDOWN_MS],
            )?;
            tx.execute(
                "UPDATE placement_holds SET cooldown_until_ms=?1",
                [now + MACHINE_COOLDOWN_MS],
            )?;
        }
        "not_now" | "expired" => {
            let streak: i64 =
                tx.query_row("SELECT not_now FROM placement_holds", [], |r| r.get(0))?;
            let streak = streak + 1;
            tx.execute(
                "UPDATE placement_holds SET not_now=?1, backoff_until_ms=?2",
                params![streak, now + backoff_ms(streak)],
            )?;
            proposal.record["outcome"]["backoff_ms"] = json!(backoff_ms(streak));
        }
        _ => {}
    }
    save(&tx, &proposal)?;
    tx.commit()?;
    if state == "never"
        && let Err(error) = pin_by_captain(conn, proposal.text("ship"))
    {
        eprintln!(
            "privateer: the captain said never to moving {}, but it could not be pinned: {error:#}",
            proposal.text("ship")
        );
    }
    if let Err(error) = notify(conn, &proposal) {
        eprintln!("privateer: proposal {id} closed as {state}, but its notices failed: {error:#}");
    }
    Ok(true)
}

fn pin_by_captain(conn: &Connection, ship: &str) -> Result<()> {
    let member = crate::fleet::members(conn)?
        .into_iter()
        .find(|m| m.name == ship && m.home.is_dir())
        .with_context(|| format!("{ship} is no longer on this machine"))?;
    let mut store = Store::open(&member.home)?;
    crate::fleet::dispatch(
        &mut store,
        "ship.requires",
        json!({"name": ship, "pinned": NEVER_REASON}),
    )?;
    Ok(())
}

/// The sentence a closed proposal ends with.
fn outcome_text(proposal: &Proposal) -> String {
    let (ship, from, to) = (
        proposal.text("ship"),
        proposal.text("from"),
        proposal.text("to"),
    );
    let outcome = &proposal.record["outcome"];
    let why = outcome["why"].as_str().unwrap_or("no reason given");
    match proposal.state.as_str() {
        "moved" => {
            let cleanup = if outcome["cleaned"] == true {
                format!(
                    "Its old folder on {from} was deleted, freeing {}.",
                    size(outcome["freed_bytes"].as_u64().unwrap_or(0))
                )
            } else if let Some(error) = outcome["clean_error"].as_str() {
                format!(
                    "Its old folder on {from} could not be deleted ({error}); `privateer ship clean-moved {ship}` there deletes it."
                )
            } else {
                format!(
                    "Its old folder stays on {from} until `privateer ship clean-moved {ship}` deletes it; {from} proposes nothing until then."
                )
            };
            format!(
                "{ship} moved from {from} to {to} (move {}), as the captain approved. {cleanup}",
                outcome["move"].as_str().unwrap_or("?")
            )
        }
        "refused" => format!(
            "{ship} was not moved from {from} to {to}: {why}. It is not proposed again for a day, and {from} proposes nothing for 6 hours."
        ),
        "interrupted" => format!(
            "The move of {ship} from {from} to {to} was interrupted: {why}. {ship} may be stopped. On {}, `{}` finishes the move, or says why it cannot.",
            outcome["runs_on"].as_str().unwrap_or(to),
            outcome["finish"].as_str().unwrap_or("privateer ship move")
        ),
        "not_now" => format!(
            "The captain said not now to moving {ship} to {to}; {from} proposes nothing for {}.",
            span(outcome["backoff_ms"].as_i64().unwrap_or(BACKOFF_MS))
        ),
        "expired" => format!(
            "The proposal to move {ship} to {to} went unanswered for 48 hours, which counts as not now; {from} proposes nothing for {}.",
            span(outcome["backoff_ms"].as_i64().unwrap_or(BACKOFF_MS))
        ),
        "never" => format!(
            "The captain said never to moving {ship}; it is pinned by the captain, and only the captain unpins it."
        ),
        "unreachable" => format!(
            "{to} did not check whether it can host {ship} within 10 minutes; the proposal closed as target unreachable, with no backoff."
        ),
        "unfit" => format!("{to} cannot host {ship} now: {why}. The proposal closed."),
        other => format!("The proposal to move {ship} to {to} ended as {other}."),
    }
}

fn notify(conn: &Connection, proposal: &Proposal) -> Result<()> {
    let Some((owner, home)) = owner_home(conn)? else {
        return Ok(());
    };
    let store = Store::open(&home)?;
    let duty = duty_task(&store, &owner);
    let body = outcome_text(proposal);
    let facts = json!({"id": proposal.id, "ship": proposal.text("ship"), "state": proposal.state,
        "outcome": proposal.record["outcome"]});
    if let Some(duty) = &duty {
        crate::mail::record_crew(
            store.connection(),
            &json!({"id": format!("placement-{}-{}", proposal.id, proposal.state), "to": owner,
                "kind": "notice", "task_id": duty, "urgency": "normal", "body": body,
                "payload": {"placement_outcome": facts}}),
        )?;
    }
    // The captain hears how a yes ended, on the decision's thread.
    if matches!(proposal.state.as_str(), "moved" | "refused" | "interrupted")
        && let Some(decision) = proposal.record["decision"].as_str()
    {
        let mut notice = json!({"id": format!("placement-{}-{}-captain", proposal.id, proposal.state),
            "from": owner, "to": crate::mail::CAPTAIN, "kind": "notice", "correlation": decision,
            "body": body, "payload": {"placement_outcome": facts},
            "urgency": if proposal.state == "interrupted" { "high" } else { "normal" }});
        if let Some(duty) = duty {
            notice["task_id"] = json!(duty);
        }
        crate::mail::record_runtime(&store, &notice)?;
    }
    Ok(())
}

/// The owner's pass over this machine's proposals: close a check that never
/// came, read the captain's answers, expire what went unanswered and notice
/// a yes that never ran. Only the owner's own dispatcher runs it.
pub(crate) fn upkeep(conn: &Connection, owner_store: &Store) -> Result<Value> {
    let now = Store::now_ms();
    let mut changed = vec![];
    for proposal in proposals(
        conn,
        "home_machine IS NULL AND state IN ('checking','open','approved')",
        &[],
    )? {
        let id = proposal.id.clone();
        let ending: Option<(&str, Value)> = match proposal.state.as_str() {
            "checking" if now - proposal.created_ms >= CHECKING_MS => Some((
                "unreachable",
                json!({"why": "the target did not check within 10 minutes"}),
            )),
            "open" => {
                let decision = proposal.text("decision").to_owned();
                match crate::mail::find(owner_store, &decision)? {
                    None => {
                        write_decision(owner_store, &proposal)?;
                        None
                    }
                    Some(mail) if matches!(mail["state"].as_str(), Some("open" | "answered")) => {
                        // Only the captain answers it: a reply from anyone
                        // else, however it arrived, is no answer.
                        let reply = mail["answered_by"]
                            .as_str()
                            .map(|r| crate::mail::find(owner_store, r))
                            .transpose()?
                            .flatten()
                            .filter(|r| r["from"] == crate::mail::CAPTAIN);
                        match reply {
                            None if now - proposal.created_ms >= OPEN_MS => {
                                if mail["state"] == "open" {
                                    let tx = owner_store.write_transaction()?;
                                    crate::mail::end_request(
                                        owner_store,
                                        &mail,
                                        "expired",
                                        "unanswered for 48 hours; it counts as not now",
                                    )?;
                                    tx.commit()?;
                                }
                                Some(("expired", json!({"why": "unanswered for 48 hours"})))
                            }
                            None => None,
                            Some(reply) => {
                                let choice = reply["payload"]["answer"]["option_id"]
                                    .as_str()
                                    .unwrap_or("");
                                match choice {
                                    "move_clean" | "move_keep" => {
                                        let choice = choice.to_owned();
                                        update(conn, &id, &["open"], move |p| {
                                            p.record["answer"] = json!(choice);
                                            p.record["approved_seen_ms"] = json!(now);
                                            p.state = "approved".into();
                                        })?;
                                        changed.push(json!({"proposal": id, "state": "approved"}));
                                        None
                                    }
                                    "never" => {
                                        Some(("never", json!({"why": "the captain said never"})))
                                    }
                                    "not_now" => Some((
                                        "not_now",
                                        json!({"why": "the captain said not now"}),
                                    )),
                                    _ => Some((
                                        "not_now",
                                        json!({"why": "the captain answered without choosing a move"}),
                                    )),
                                }
                            }
                        }
                    }
                    Some(mail) => Some((
                        "not_now",
                        json!({"why": format!("the decision was {}", mail["state"].as_str().unwrap_or("closed"))}),
                    )),
                }
            }
            "approved"
                if now
                    - proposal.record["approved_seen_ms"]
                        .as_i64()
                        .unwrap_or(proposal.updated_ms)
                    >= LAPSE_MS =>
            {
                Some((
                    "refused",
                    json!({"why": "the captain's yes lapsed before the move could run"}),
                ))
            }
            _ => None,
        };
        if let Some((state, outcome)) = ending
            && conclude(conn, &id, state, outcome)?
        {
            changed.push(json!({"proposal": id, "state": state}));
        }
    }
    Ok(json!(changed))
}

// ---- Running an approved move -------------------------------------------------------

/// When this process last refreshed the offers cache.
static LAST_OFFERS: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
const OFFERS_EVERY: Duration = Duration::from_secs(300);

/// The machine's placement lock, beside its registry. A runner holds it for
/// the whole of its moves, so whoever holds it knows no move is running.
fn placement_lock(registry: &Path, wait: bool) -> Result<Option<fs::File>> {
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(registry.with_file_name("placement.lock"))?;
    use std::os::fd::AsRawFd;
    let how = if wait {
        libc::LOCK_EX
    } else {
        libc::LOCK_EX | libc::LOCK_NB
    };
    Ok((unsafe { libc::flock(lock.as_raw_fd(), how) } == 0).then_some(lock))
}

/// One dispatcher pass: keep this machine's offers fresh for the exchange,
/// close moves a crash interrupted, and start the runner when an approved
/// move here is ready. The move never runs in a dispatcher: moving a ship
/// stops the ship's own dispatcher, which may be the one ticking.
pub(crate) fn tick(store: &Store) -> Value {
    let work = || -> Result<Value> {
        let path = crate::fleet::registry_path()?;
        if !path.is_file() || !crate::machines::ever_linked(&path) {
            return Ok(Value::Null);
        }
        let conn = crate::fleet::registry()?;
        if !crate::fleet::members(&conn)?
            .iter()
            .any(|m| m.home == store.home())
        {
            return Ok(Value::Null);
        }
        let Some(lock) = placement_lock(&path, false)? else {
            return Ok(Value::Null);
        };
        let refresh = LAST_OFFERS
            .lock()
            .map(|mut last| {
                let due = last.is_none_or(|at| at.elapsed() >= OFFERS_EVERY);
                if due {
                    *last = Some(std::time::Instant::now());
                }
                due
            })
            .unwrap_or(false);
        if refresh {
            crate::capabilities::refresh(&conn);
        }
        let mut report = json!({});
        let interrupted = interrupted(&conn)?;
        if !interrupted.is_empty() {
            report["interrupted"] = json!(interrupted);
        }
        if !runnable(&conn)?.is_empty() {
            // Let go first: the runner waits for the lock, then holds it.
            drop(lock);
            report["runner"] = json!(spawn_runner(&path, store.home())?);
        }
        Ok(if report == json!({}) {
            Value::Null
        } else {
            report
        })
    };
    work().unwrap_or_else(
        |error| json!({"error": format!("{error:#}").chars().take(2_000).collect::<String>()}),
    )
}

/// Start `privateer _placement-run` in a session of its own, so stopping the
/// dispatcher that started it, which a move may do, never stops it.
fn spawn_runner(registry: &Path, home: &Path) -> Result<u32> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let executable = crate::executable::supervisor()?;
    let mut command = Command::new(&executable);
    command
        .arg("--fleet")
        .arg(registry)
        .arg("--home")
        .arg(home)
        .arg("_placement-run")
        // Never inside a ship folder: the move may delete it.
        .current_dir(registry.parent().unwrap_or(Path::new("/")))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("start the placement runner {}", executable.display()))?;
    let pid = child.id();
    // A long-lived dispatcher reaps it; a one-pass one exits and it is adopted.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(pid)
}

/// `privateer _placement-run`, started by a dispatcher: wait for the
/// placement lock, close moves a crash interrupted, and run each approved
/// move this machine is the one to run, holding the lock throughout. Its
/// outcomes are recorded on the proposals; this returns them as well.
pub fn runner() -> Result<Value> {
    let path = crate::fleet::registry_path()?;
    let conn = crate::fleet::registry()?;
    let _lock = placement_lock(&path, true)?.context("the placement lock")?;
    let interrupted = interrupted(&conn)?;
    let ran = run_approvals(&conn)?;
    conn.execute(
        "DELETE FROM placement_approvals WHERE expires_ms<?1",
        [Store::now_ms() - CARRY_CLOSED_MS],
    )?;
    Ok(json!({"interrupted": interrupted, "ran": ran}))
}

/// Moves a crash or a restart interrupted. A runner holds the placement lock
/// for the whole of its moves, so a proposal still `moving` here, seen by
/// whoever holds the lock, has no move running. One whose move finished
/// since it began closes as moved; any other as interrupted, and the captain
/// is told the command that finishes it.
fn interrupted(conn: &Connection) -> Result<Vec<Value>> {
    let mut closed = vec![];
    for proposal in proposals(conn, "state='moving'", &[])? {
        let ship = proposal.text("ship").to_owned();
        let since = proposal.record["moving_ms"]
            .as_i64()
            .unwrap_or(proposal.updated_ms);
        let run: Option<(String, String)> = conn
            .query_row(
                "SELECT id, state FROM moves WHERE role='run' AND ship=?1 AND updated_ms>=?2
                 ORDER BY updated_ms DESC LIMIT 1",
                params![ship, since],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let cleanup: Option<String> = conn
            .query_row(
                "SELECT cleanup FROM placement_approvals WHERE proposal=?1",
                [&proposal.id],
                |r| r.get(0),
            )
            .optional()?;
        let outcome = match &run {
            Some((id, state)) if state == "done" => json!({"state": "moved", "move": id,
                "cleanup": cleanup, "cleaned": false,
                "note": "the move finished, but what ran it stopped before it said so"}),
            _ => {
                // The finishing command, on the machine that ran the move.
                let (finish, on) = match &proposal.home {
                    None => (
                        format!("privateer ship move {ship} --to {}", proposal.text("to")),
                        proposal.text("from"),
                    ),
                    Some(_) => (
                        format!("privateer ship move {ship} --here"),
                        proposal.text("to"),
                    ),
                };
                json!({"state": "interrupted", "move": run.as_ref().map(|(id, _)| id),
                    "move_state": run.as_ref().map(|(_, state)| state),
                    "why": "what ran the move stopped before it finished (a crash or a restart)",
                    "finish": finish, "runs_on": on})
            }
        };
        let state = outcome["state"]
            .as_str()
            .unwrap_or("interrupted")
            .to_owned();
        finish(conn, &proposal, &state, outcome.clone())?;
        closed.push(
            json!({"proposal": proposal.id, "ship": ship, "state": state,
            "outcome": outcome}),
        );
    }
    Ok(closed)
}

fn run_approvals(conn: &Connection) -> Result<Vec<Value>> {
    let mut ran = vec![];
    for (approval, proposal) in runnable(conn)? {
        let now = Store::now_ms();
        let claimed = conn.execute(
            "UPDATE placement_approvals SET used_ms=?2 WHERE proposal=?1 AND used_ms IS NULL",
            params![approval.proposal, now],
        )?;
        if claimed == 0 {
            continue;
        }
        let outcome = run_one(conn, &approval, &proposal, now);
        let state = outcome["state"].as_str().unwrap_or("refused").to_owned();
        finish(conn, &proposal, &state, outcome.clone())?;
        ran.push(
            json!({"proposal": proposal.id, "ship": approval.ship, "state": state,
            "outcome": outcome}),
        );
    }
    Ok(ran)
}

/// Record how a move this machine ran ended: on its own proposal, which
/// closes it, or on the copy, which the exchange carries back.
fn finish(conn: &Connection, proposal: &Proposal, state: &str, outcome: Value) -> Result<()> {
    match &proposal.home {
        None => {
            conclude(conn, &proposal.id, state, outcome)?;
        }
        Some(_) => {
            let next = state.to_owned();
            update(
                conn,
                &proposal.id,
                &["open", "approved", "moving"],
                move |p| {
                    p.record["outcome"] = outcome;
                    p.state = next;
                },
            )?;
        }
    }
    Ok(())
}

/// The approvals this machine is the one to run, each with its proposal, once
/// that proposal is open or approved here. One whose proposal has not arrived
/// or is not runnable yet stays unused until it is, or until it expires.
fn runnable(conn: &Connection) -> Result<Vec<(Approval, Proposal)>> {
    let mut statement = conn.prepare(
        "SELECT proposal, ship, from_machine, to_machine, binding, cleanup, expires_ms
         FROM placement_approvals WHERE used_ms IS NULL ORDER BY approved_ms",
    )?;
    let approvals = statement
        .query_map([], |r| {
            Ok(Approval {
                proposal: r.get(0)?,
                ship: r.get(1)?,
                from: r.get(2)?,
                to: r.get(3)?,
                binding: r.get(4)?,
                cleanup: r.get(5)?,
                expires_ms: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    let mut out = vec![];
    for approval in approvals {
        let Some(proposal) = load(conn, &approval.proposal)? else {
            continue;
        };
        if !matches!(proposal.state.as_str(), "open" | "approved") {
            continue;
        }
        // Only the linking side of the proposal's link runs it.
        let mover = match &proposal.home {
            None => role(conn, proposal.text("to"))?.as_deref() == Some("collects"),
            Some(home) => role(conn, home)?.as_deref() == Some("collects"),
        };
        if mover {
            out.push((approval, proposal));
        }
    }
    Ok(out)
}

/// Run the one move an approval names, once: refuse when it lapsed, when the
/// proposal no longer matches what the captain was shown, when the ship is no
/// longer eligible or the target can no longer host it. Then the same `run`
/// `ship.move` calls, and the old folder's deletion when the captain chose it.
fn run_one(conn: &Connection, approval: &Approval, proposal: &Proposal, now: i64) -> Value {
    let refused = |why: String| json!({"state": "refused", "why": why});
    if now > approval.expires_ms {
        return refused(
            "the captain's yes lapsed: it waited more than 6 hours for this machine".into(),
        );
    }
    let matches = binding(&proposal.record) == approval.binding
        && proposal.record["binding"] == json!(approval.binding)
        && proposal.text("ship") == approval.ship
        && proposal.text("from") == approval.from
        && proposal.text("to") == approval.to;
    if !matches {
        return refused(
            "the proposal no longer matches what the captain approved; nothing moved".into(),
        );
    }
    let ship = proposal.text("ship").to_owned();
    let moving = update(conn, &proposal.id, &["open", "approved"], |p| {
        p.record["moving_ms"] = json!(Store::now_ms());
        p.state = "moving".into();
    });
    if !matches!(moving, Ok(Some(_))) {
        return refused("the proposal closed before the move began".into());
    }
    let result = (|| -> Result<Value> {
        // The ship, tested again where it is.
        let eligible = match &proposal.home {
            None => eligibility(conn, &ship, now)?,
            Some(home) => crate::machines::call_reached(
                home,
                "move.eligible",
                json!({"ship": ship}),
                Some(SLOW_CALL),
            )?,
        };
        if eligible["eligible"] != true {
            return Ok(refused(format!(
                "{ship} is no longer eligible: {}",
                names(&eligible["reasons"]).join("; ")
            )));
        }
        let request = json!({"ship": ship, "requires": eligible["requires"],
            "first_mate": eligible["first_mate"], "projects": eligible["projects"]});
        // The target, checked again live.
        let (check, to) = match &proposal.home {
            None => (
                crate::machines::call_reached(
                    proposal.text("to"),
                    "move.check",
                    request,
                    Some(SLOW_CALL),
                )?,
                proposal.text("to").to_owned(),
            ),
            Some(_) => (
                crate::moves::serve("move.check", &request)?,
                crate::machines::this_machine(),
            ),
        };
        if let Some(why) = check_refusal(&check, needed(&eligible), &to) {
            return Ok(refused(why));
        }
        let moved = crate::moves::run(&json!({"name": ship,
            "to": if proposal.home.is_none() { proposal.text("to") } else { "here" }}))?;
        let id = moved["move"].as_str().unwrap_or("").to_owned();
        let mut outcome = json!({"state": "moved", "move": id, "cleanup": approval.cleanup,
            "cleaned": false});
        if approval.cleanup == "move_clean" {
            let cleaned = match &proposal.home {
                None => crate::moves::serve("move.clean", &json!({"id": id})),
                Some(home) => {
                    crate::machines::call_reached(home, "move.clean", json!({"id": id}), None)
                }
            };
            match cleaned {
                Ok(cleaned) => {
                    outcome["cleaned"] = cleaned["cleaned"].clone();
                    outcome["freed_bytes"] = cleaned["freed_bytes"].clone();
                }
                Err(error) => outcome["clean_error"] = json!(format!("{error:#}")),
            }
        }
        Ok(outcome)
    })();
    result.unwrap_or_else(|error| refused(format!("{error:#}")))
}

/// A move's old folder was deleted here: the pressure clocks start again,
/// and this machine's proposals of that ship are no longer held for it.
pub(crate) fn cleaned(conn: &Connection, ship: &str) -> Result<()> {
    restart_clocks(conn)?;
    conn.execute(
        "UPDATE placement_proposals SET record=json_set(record,'$.cleaned_ms',?2)
         WHERE home_machine IS NULL AND ship=?1 AND json_extract(record,'$.cleaned_ms') IS NULL",
        params![ship, Store::now_ms()],
    )?;
    Ok(())
}

/// `ship.placement` and `ship.propose_move`.
pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "ship.placement" => placement(store),
        "ship.propose_move" => propose(store, &args),
        _ => bail!("unknown placement command {command}"),
    }
}

/// `move.eligible {ship}` over the gateway, for the machine about to run an
/// approved move.
pub(crate) fn serve_eligible(args: &Value) -> Result<Value> {
    let conn = crate::fleet::registry()?;
    eligibility(&conn, required(args, "ship")?, Store::now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        conn
    }

    #[test]
    fn only_disk_alerts_count_and_a_cleared_one_starts_over() {
        let conn = registry();
        note_pressure(
            &conn,
            &["disk_floor", "capacity_wait", "route:fast", "load"],
        )
        .unwrap();
        let keys = pressure_keys(&conn);
        assert_eq!(keys, vec!["disk_floor"]);
        conn.execute("UPDATE machine_pressure SET raised_ms=1", [])
            .unwrap();
        // Still raised: the clock keeps its start.
        note_pressure(&conn, &["disk_floor"]).unwrap();
        assert_eq!(pressed(&conn).unwrap()[0].since_ms, 1);
        // Cleared and raised again: it starts over.
        note_pressure(&conn, &[]).unwrap();
        assert!(pressure_keys(&conn).is_empty());
        note_pressure(&conn, &["disk_floor"]).unwrap();
        assert!(pressed(&conn).unwrap()[0].since_ms > 1);
        // A cleanup restarts it too.
        conn.execute("UPDATE machine_pressure SET raised_ms=1", [])
            .unwrap();
        restart_clocks(&conn).unwrap();
        assert!(pressed(&conn).unwrap()[0].since_ms > 1);
    }

    #[test]
    fn backoff_doubles_to_a_week() {
        assert_eq!(backoff_ms(1), DAY_MS);
        assert_eq!(backoff_ms(2), 2 * DAY_MS);
        assert_eq!(backoff_ms(3), 4 * DAY_MS);
        assert_eq!(backoff_ms(4), 7 * DAY_MS);
        assert_eq!(backoff_ms(40), 7 * DAY_MS);
    }

    #[test]
    fn the_binding_covers_the_target_and_its_answer() {
        let record = json!({"id": "p", "ship": "s", "from": "mac", "to": "laptop",
            "check": {"met": [], "unmet": []}, "why": "full disk"});
        let bound = binding(&record);
        let mut changed = record.clone();
        changed["to"] = json!("studio");
        assert_ne!(binding(&changed), bound);
        let mut changed = record.clone();
        changed["check"]["met"] = json!(["xcode"]);
        assert_ne!(binding(&changed), bound);
        // What the binding does not cover may change.
        let mut changed = record;
        changed["why"] = json!("still full");
        assert_eq!(binding(&changed), bound);
    }

    #[test]
    fn a_check_refuses_unmet_needs_problems_pressure_and_no_room() {
        let fine = json!({"met": [], "unmet": [], "problems": [], "pressure": [],
            "free_disk_bytes": 10u64 << 30, "floor_bytes": 2u64 << 30});
        assert_eq!(check_refusal(&fine, 1 << 30, "laptop"), None);
        let tight = check_refusal(&fine, 9 << 30, "laptop").unwrap();
        assert!(tight.contains("laptop has no room"), "{tight}");
        let mut strained = fine.clone();
        strained["pressure"] = json!(["disk_trend"]);
        assert!(
            check_refusal(&strained, 0, "laptop")
                .unwrap()
                .contains("under disk pressure itself (disk_trend)")
        );
        let mut lacking = fine;
        lacking["unmet"] = json!([{"need": "xcode", "why": "not declared here"}]);
        assert!(
            check_refusal(&lacking, 0, "laptop")
                .unwrap()
                .contains("laptop does not offer xcode (not declared here)")
        );
    }

    #[test]
    fn an_ending_holds_proposals_back_as_it_should() {
        let conn = registry();
        let open = |id: &str, ship: &str| Proposal {
            id: id.into(),
            home: None,
            state: "open".into(),
            record: json!({"id": id, "ship": ship, "from": "mac", "to": "laptop"}),
            created_ms: 1,
            updated_ms: 1,
        };
        save(&conn, &open("a", "rover")).unwrap();
        let now = Store::now_ms();
        assert!(holds(&conn, now).unwrap()[0].contains("one proposal is open at a time"));
        conclude(&conn, "a", "not_now", json!({})).unwrap();
        let held = holds(&conn, now).unwrap();
        assert!(held[0].contains("not now (1 in a row)"), "{held:?}");
        save(&conn, &open("b", "rover")).unwrap();
        conclude(&conn, "b", "expired", json!({})).unwrap();
        let until: i64 = conn
            .query_row("SELECT backoff_until_ms FROM placement_holds", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            until - now >= 2 * DAY_MS - MINUTE_MS,
            "an expiry counts as not now"
        );
        // A move resets the backoff, and holds until the folder is cleaned.
        save(&conn, &open("c", "rover")).unwrap();
        conclude(&conn, "c", "moved", json!({"cleaned": false})).unwrap();
        let held = holds(&conn, now).unwrap();
        assert_eq!(held.len(), 1, "{held:?}");
        assert!(held[0].contains("clean-moved rover"), "{held:?}");
        cleaned(&conn, "rover").unwrap();
        assert!(holds(&conn, now).unwrap().is_empty());
        // A refusal when it ran cools the ship and the machine, without backoff.
        save(&conn, &open("d", "rover")).unwrap();
        conclude(&conn, "d", "refused", json!({"why": "busy"})).unwrap();
        let held = holds(&conn, now).unwrap();
        assert!(held[0].contains("refused when it ran"), "{held:?}");
        let streak: i64 = conn
            .query_row("SELECT not_now FROM placement_holds", [], |r| r.get(0))
            .unwrap();
        assert_eq!(streak, 0);
        // A closed proposal does not close again.
        assert!(!conclude(&conn, "d", "moved", json!({})).unwrap());
    }
}
