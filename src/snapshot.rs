//! `fleet.snapshot`: one operator JSON of the fleet across linked machines.
//!
//! Each machine reads its own section (its homes' ships, live jobs and needs),
//! hashes it and keeps the time its content last changed in the registry's
//! `fleet_snapshots`. A machine this one reaches answers for its own section
//! over the gateway; a machine that reaches this one carries its section on
//! the `fleet.collect` it already makes. Nothing in a section is computed
//! from the current time, so it hashes the same while nothing changes. See
//! docs/design/fleet-snapshot.md.
use crate::needs::ObservationBudget;
use crate::store::{Caller, Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// A gateway read this young is shown again rather than asked for again.
pub(crate) const REUSE_MS: i64 = 5_000;
/// The largest section stored, answered or shown for one machine.
const SECTION_BYTES: usize = 768 * 1024;
/// The largest section carried on a `fleet.collect`.
pub(crate) const CARRY_BYTES: usize = 256 * 1024;
/// The whole local read of one machine's homes.
const LOCAL_BUDGET: Duration = Duration::from_secs(3);
const SHIP_LIMIT: usize = 200;
const TASKS_SHOWN: usize = 20;
const JOBS_SHOWN: usize = 20;
const NEEDS_PER_HOME: u64 = 40;
const DOING_CHARS: usize = 200;
const ERROR_CHARS: usize = 500;
const TERMINAL: &str = "('succeeded','failed','cancelled','timed_out','resumed')";

pub(crate) fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS fleet_snapshots(machine TEXT PRIMARY KEY,
           digest TEXT NOT NULL, changed_ms INTEGER NOT NULL, as_of_ms INTEGER NOT NULL,
           via TEXT NOT NULL, body TEXT NOT NULL);",
    )?;
    Ok(())
}

/// One machine's section as this registry stored it.
#[derive(Clone, Debug)]
pub(crate) struct Stored {
    pub changed_ms: i64,
    pub as_of_ms: i64,
    pub via: String,
    pub section: Value,
}

fn stored(conn: &Connection, machine: &str) -> Result<Option<Stored>> {
    let row: Option<(i64, i64, String, String)> = conn
        .query_row(
            "SELECT changed_ms, as_of_ms, via, body FROM fleet_snapshots WHERE machine=?1",
            [machine],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    // A body that does not parse reads as never read.
    Ok(row.and_then(|(changed_ms, as_of_ms, via, body)| {
        serde_json::from_str(&body).ok().map(|section| Stored {
            changed_ms,
            as_of_ms,
            via,
            section,
        })
    }))
}

/// This machine's own section, recorded: `changed_ms` moves only when the
/// content does, never backwards, and a read older than the stored one
/// changes nothing (a concurrent call already wrote a later one).
fn record_own(conn: &Connection, machine: &str, section: &Value, now: i64) -> Result<Stored> {
    let digest = crate::core::hash(section);
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let previous: Option<(String, i64, i64)> = tx
        .query_row(
            "SELECT digest, changed_ms, as_of_ms FROM fleet_snapshots WHERE machine=?1",
            [machine],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let changed_ms = match &previous {
        Some((_, _, as_of)) if *as_of >= now => {
            drop(tx);
            return stored(conn, machine)?.context("snapshot row vanished");
        }
        Some((old, changed, _)) if *old == digest => *changed,
        Some((_, changed, _)) => now.max(changed + 1),
        None => now,
    };
    tx.execute(
        "INSERT INTO fleet_snapshots(machine, digest, changed_ms, as_of_ms, via, body)
         VALUES(?1, ?2, ?3, ?4, 'direct', ?5)
         ON CONFLICT(machine) DO UPDATE SET digest=excluded.digest, changed_ms=excluded.changed_ms,
           as_of_ms=excluded.as_of_ms, via=excluded.via, body=excluded.body",
        params![machine, digest, changed_ms, now, section.to_string()],
    )?;
    tx.commit()?;
    Ok(Stored {
        changed_ms,
        as_of_ms: now,
        via: "direct".into(),
        section: section.clone(),
    })
}

/// A section another machine read about itself. It keeps that machine's
/// `changed_ms`; only a read newer than the stored one replaces it. A reply
/// without a section (unchanged since the stored one) only dates it.
fn record_remote(
    conn: &Connection,
    machine: &str,
    via: &str,
    as_of_ms: i64,
    changed_ms: i64,
    section: Option<&Value>,
) -> Result<()> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let previous: Option<i64> = tx
        .query_row(
            "SELECT as_of_ms FROM fleet_snapshots WHERE machine=?1",
            [machine],
            |r| r.get(0),
        )
        .optional()?;
    if previous.is_some_and(|at| at >= as_of_ms) {
        return Ok(());
    }
    match section {
        Some(section) => {
            let body = section.to_string();
            ensure!(body.len() <= SECTION_BYTES, "section exceeds its bound");
            tx.execute(
                "INSERT INTO fleet_snapshots(machine, digest, changed_ms, as_of_ms, via, body)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(machine) DO UPDATE SET digest=excluded.digest,
                   changed_ms=excluded.changed_ms, as_of_ms=excluded.as_of_ms,
                   via=excluded.via, body=excluded.body",
                params![
                    machine,
                    crate::core::hash(section),
                    changed_ms,
                    as_of_ms,
                    via,
                    body
                ],
            )?;
        }
        None => {
            tx.execute(
                "UPDATE fleet_snapshots SET as_of_ms=?2, via=?3 WHERE machine=?1 AND changed_ms=?4",
                params![machine, as_of_ms, via, changed_ms],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn forget(conn: &Connection, machine: &str) -> Result<()> {
    conn.execute("DELETE FROM fleet_snapshots WHERE machine=?1", [machine])?;
    Ok(())
}

/// Whether a lease, as stored, holds now, and what holds it.
pub(crate) fn presence_state(
    holder: &str,
    expires_ms: i64,
    asleep_holder: Option<&str>,
    now: i64,
) -> &'static str {
    if expires_ms <= now {
        "absent"
    } else if asleep_holder == Some(holder) {
        "asleep"
    } else if holder.starts_with(crate::presence::HOSTED) {
        "hosted"
    } else {
        "session"
    }
}

/// Holders of this machine's capacity, read once, by job id.
struct Holders {
    by_job: HashMap<String, i64>,
    /// The live holder a queued job waits behind: (job id, ship, unhealthy).
    first: Option<(String, String, bool)>,
}

fn holders(store: &Store) -> Option<Holders> {
    let holders = crate::jobs::capacity_holders_waiting(store, Duration::from_millis(100)).ok()?;
    Some(Holders {
        by_job: holders
            .iter()
            .map(|h| (h.job_id.clone(), h.started_ms))
            .collect(),
        first: holders
            .iter()
            .filter(|h| h.live())
            .min_by_key(|h| h.started_ms)
            .map(|h| (h.job_id.clone(), h.ship.clone(), h.unhealthy.is_some())),
    })
}

/// The first line of a crew progress report, after the runtime's lead line.
fn doing_text(body: &str) -> String {
    let text = match body.split_once('\n') {
        Some((lead, rest)) if lead.starts_with("Crew job ") => rest,
        _ => body,
    };
    text.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .chars()
        .take(DOING_CHARS)
        .collect()
}

fn column(conn: &Connection, table: &str, name: &str) -> Result<bool> {
    Ok(conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name=?1)"),
        [name],
        |r| r.get(0),
    )?)
}

/// A stored lease: holder, started, expires, the holder that slept, since when.
type Lease = (String, i64, i64, Option<String>, Option<i64>);

/// One ship in its own home, read in one deferred transaction under the
/// observation budget. Every value is a stored fact; `now` only decides
/// whether a stored lease still holds.
fn read_ship(store: &Store, ship: &str, holders: Option<&Holders>, now: i64) -> Result<Value> {
    let budget = ObservationBudget::new(store.connection())?;
    let tx = rusqlite::Transaction::new_unchecked(
        store.connection(),
        rusqlite::TransactionBehavior::Deferred,
    )?;
    let conn: &Connection = &tx;
    budget.check()?;
    let record = crate::mail::ship(store, ship)?.unwrap_or(Value::Null);
    let first_mate = &record["first_mate"];
    let harness = first_mate["manifest"]
        .as_str()
        .and_then(crate::bundled::harness);
    let asleep_columns = column(conn, "presence", "asleep_holder")?;
    let lease: Option<Lease> = conn
        .query_row(
            &format!(
                "SELECT holder, started_ms, expires_ms, {} FROM presence WHERE ship=?1",
                if asleep_columns {
                    "asleep_holder, asleep_since_ms"
                } else {
                    "NULL, NULL"
                }
            ),
            [ship],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let (presence, since) = match &lease {
        Some((holder, started, expires, asleep, asleep_since)) => {
            match presence_state(holder, *expires, asleep.as_deref(), now) {
                "asleep" => ("asleep", asleep_since.or(Some(*started))),
                "absent" => ("absent", None),
                state => (state, Some(*started)),
            }
        }
        None => ("absent", None),
    };
    budget.check()?;
    let (unread, open_requests): (i64, i64) = conn.query_row(
        "SELECT
           (SELECT COUNT(*) FROM mail WHERE recipient=?1 AND seq >
              COALESCE((SELECT through_seq FROM mail_cursor WHERE address=?1), 0)),
           (SELECT COUNT(*) FROM mail WHERE recipient=?1 AND kind='request' AND state='open')",
        [ship],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    budget.check()?;
    let open = "status IN ('open','running','blocked')";
    let open_tasks_total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM tasks WHERE owner=?1 AND {open}"),
        [ship],
        |r| r.get(0),
    )?;
    let open_tasks = conn
        .prepare(&format!(
            "SELECT id, title, status, handoff_to FROM tasks WHERE owner=?1 AND {open}
             ORDER BY rowid DESC LIMIT ?2"
        ))?
        .query_map(params![ship, TASKS_SHOWN], |r| {
            Ok(json!({"id": r.get::<_, String>(0)?,
                "title": r.get::<_, String>(1)?.chars().take(500).collect::<String>(),
                "status": r.get::<_, String>(2)?, "handoff_to": r.get::<_, Option<String>>(3)?}))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    budget.check()?;
    let jobs_total: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM jobs WHERE json_extract(record,'$.status') NOT IN {TERMINAL}"
        ),
        [],
        |r| r.get(0),
    )?;
    let live: Vec<Value> = conn
        .prepare(&format!(
            "SELECT record FROM jobs WHERE json_extract(record,'$.status') NOT IN {TERMINAL}
             ORDER BY json_extract(record,'$.created_ms') DESC, id LIMIT ?1"
        ))?
        .query_map([JOBS_SHOWN], |r| r.get::<_, String>(0))?
        .map(|raw| Ok(serde_json::from_str(&raw?)?))
        .collect::<Result<_>>()?;
    let has_errands = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='errands')",
        [],
        |r| r.get::<_, bool>(0),
    )?;
    let has_wakes = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='wakes')",
        [],
        |r| r.get::<_, bool>(0),
    )?;
    let mut jobs = vec![];
    for job in live {
        budget.check()?;
        let id = job["id"].as_str().unwrap_or("");
        let task_id = job["task_id"].as_str().unwrap_or("");
        let status = job["status"].as_str().unwrap_or("");
        let exists = |sql: &str| -> Result<bool> { Ok(conn.query_row(sql, [id], |r| r.get(0))?) };
        let kind = if has_wakes && exists("SELECT EXISTS(SELECT 1 FROM wakes WHERE job_id=?1)")? {
            "first_mate"
        } else if has_errands && exists("SELECT EXISTS(SELECT 1 FROM errands WHERE id=?1)")? {
            "errand"
        } else if job["action"] == "spawn" && job["agent_actor"].is_string() {
            "crew"
        } else {
            "command"
        };
        let task_title: Option<String> = conn
            .query_row("SELECT title FROM tasks WHERE id=?1", [task_id], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .map(|t| t.chars().take(500).collect());
        let park: Option<(String, String, i64)> = if status == "parked" {
            conn.query_row(
                "SELECT child_job_id, next, updated_ms FROM crew_parks
                 WHERE crew_job_id=?1 ORDER BY updated_ms DESC LIMIT 1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
        } else {
            None
        };
        let doing = match &park {
            Some((_, next, at)) => {
                json!({"text": doing_text(next), "at_ms": at, "report_id": null})
            }
            None => conn
                .query_row(
                    "SELECT id, body, created_ms FROM mail
                     WHERE id >= ?1 AND id < ?2 AND json_extract(payload,'$.job_id')=?3
                       AND json_extract(payload,'$.report')='progress'
                     ORDER BY seq DESC LIMIT 1",
                    params![format!("crew-{id}-"), format!("crew-{id}."), id],
                    |r| {
                        Ok(json!({"report_id": r.get::<_, String>(0)?,
                            "text": doing_text(&r.get::<_, String>(1)?),
                            "at_ms": r.get::<_, i64>(2)?}))
                    },
                )
                .optional()?
                .unwrap_or(Value::Null),
        };
        let wait = match (status, &park) {
            ("parked", Some((child, _, _))) => json!({"for": "job", "job_id": child}),
            ("queued", _) => {
                let disk = job["reason"]
                    .as_str()
                    .is_some_and(|r| r.contains("disk available"));
                let behind: Vec<Value> = holders
                    .and_then(|h| h.first.as_ref())
                    .filter(|_| !disk)
                    .map(|(job_id, ship, unhealthy)| {
                        json!({"job_id": job_id, "ship": ship, "unhealthy": unhealthy})
                    })
                    .into_iter()
                    .collect();
                json!({"for": if disk { "disk" } else { "capacity" },
                    "since_ms": job["created_ms"], "behind": behind})
            }
            _ => Value::Null,
        };
        jobs.push(json!({
            "id": id, "task_id": task_id, "task_title": task_title, "kind": kind,
            "state": status, "heavy": job["heavy"].as_bool().unwrap_or(false),
            "created_ms": job["created_ms"], "spawned_ms": job.get("spawned_ms").cloned().unwrap_or(Value::Null),
            "started_ms": holders.and_then(|h| h.by_job.get(id)),
            "model_route": job.get("model_route").cloned().unwrap_or(Value::Null),
            "doing": doing, "wait": wait,
        }));
    }
    budget.check()?;
    drop(tx);
    Ok(json!({
        "name": ship, "liaison": crate::door::is_liaison(store.home()),
        "harness": harness, "hosted": crate::host::hosted(first_mate),
        "presence": presence, "presence_since_ms": since,
        "unread": unread, "open_requests": open_requests,
        "open_tasks": open_tasks, "open_tasks_total": open_tasks_total,
        "jobs": jobs, "jobs_total": jobs_total,
    }))
}

/// A ship that could not be read within its budget.
fn unavailable(ship: &str) -> Value {
    json!({"name": ship, "presence": "unavailable"})
}

/// One home's typed needs, as `runtime.needs` reads them under its own
/// budget and transaction, before the ship read opens its own.
fn read_needs(store: &Store) -> Option<Vec<Value>> {
    let needs = crate::needs::query(store, &json!({"limit": NEEDS_PER_HOME})).ok()?;
    if needs["available"] != true {
        return None;
    }
    Some(
        needs["items"]
            .as_array()
            .into_iter()
            .flatten()
            .cloned()
            .map(|mut item| {
                if let Some(fields) = item.as_object_mut() {
                    fields.remove("observed_ms");
                    fields.remove("waited_ms");
                    fields.entry("mail_id").or_insert(Value::Null);
                }
                item
            })
            .collect(),
    )
}

/// This machine's own section: its row, its ships and its needs. Every
/// member home is read; a home that cannot be read in its budget, or after
/// the machine's whole budget, is `unavailable` and named in `unknown`.
pub(crate) fn local_section(conn: &Connection, machine: &str, now: i64) -> Result<Value> {
    let started = Instant::now();
    let members = crate::fleet::members(conn)?;
    let truncated = members.len() > SHIP_LIMIT;
    let mut unknown: Vec<String> = vec![];
    let mut ships = vec![];
    let mut needs = vec![];
    let mut stowed = Value::Null;
    let mut holders_read: Option<Option<Holders>> = None;
    for member in members.into_iter().take(SHIP_LIMIT) {
        if !member.home.is_dir() {
            continue;
        }
        if started.elapsed() >= LOCAL_BUDGET {
            unknown.push(format!("home:{}", member.name));
            ships.push(unavailable(&member.name));
            continue;
        }
        let Ok(store) = Store::open_readonly(&member.home, Caller::Operator) else {
            unknown.push(format!("home:{}", member.name));
            ships.push(unavailable(&member.name));
            continue;
        };
        if holders_read.is_none() {
            let read = holders(&store);
            if read.is_none() {
                unknown.push("ledger".into());
            }
            holders_read = Some(read);
            stowed = crate::stow::state(&store)
                .map(|s| json!(s["active"].as_array().is_some_and(|a| !a.is_empty())))
                .unwrap_or(Value::Null);
        }
        match read_needs(&store) {
            Some(items) => needs.extend(items.into_iter().map(|mut item| {
                item["machine"] = json!(machine);
                item
            })),
            None => unknown.push(format!("needs:{}", member.name)),
        }
        match read_ship(
            &store,
            &member.name,
            holders_read.as_ref().and_then(Option::as_ref),
            now,
        ) {
            Ok(mut ship) => {
                ship["machine"] = json!(machine);
                ships.push(ship);
            }
            Err(_) => {
                unknown.push(format!("home:{}", member.name));
                ships.push(unavailable(&member.name));
            }
        }
    }
    let desk = crate::machines::desk_here(conn);
    let mut section = json!({
        "machine": {"name": machine, "desk": desk, "stowed": stowed,
            "truncated": truncated, "unknown": unknown},
        "ships": ships, "needs": needs,
    });
    bound(&mut section);
    Ok(section)
}

/// Keep a section under its byte bound: open tasks go first, then jobs past
/// five per ship. Marks it truncated when anything was dropped.
fn bound(section: &mut Value) {
    let size = |s: &Value| s.to_string().len();
    if size(section) <= SECTION_BYTES {
        return;
    }
    section["machine"]["truncated"] = json!(true);
    for ship in section["ships"].as_array_mut().into_iter().flatten() {
        ship["open_tasks"] = json!([]);
    }
    if size(section) <= SECTION_BYTES {
        return;
    }
    for ship in section["ships"].as_array_mut().into_iter().flatten() {
        if let Some(jobs) = ship["jobs"].as_array_mut() {
            jobs.truncate(5);
        }
    }
    if size(section) > SECTION_BYTES {
        section["ships"] = json!([]);
        section["needs"] = json!([]);
    }
}

/// What this machine answers about itself: read, recorded and dated. Omits
/// the section when the caller's `since` equals its `changed_ms`.
pub(crate) fn own(conn: &Connection, since: Option<i64>) -> Result<Value> {
    let machine = crate::machines::this_machine();
    let now = Store::now_ms();
    let section = local_section(conn, &machine, now)?;
    let stored = record_own(conn, &machine, &section, now)?;
    let unchanged = since == Some(stored.changed_ms);
    Ok(json!({"machine": machine, "as_of_ms": stored.as_of_ms,
        "changed_ms": stored.changed_ms,
        "section": if unchanged { Value::Null } else { stored.section }}))
}

/// This machine's section for a `fleet.collect` it makes, bounded.
pub(crate) fn carried(conn: &Connection) -> Option<Value> {
    let mut answer = own(conn, None).ok()?;
    if answer["section"].to_string().len() > CARRY_BYTES {
        answer["section"] = Value::Null;
        answer["truncated"] = json!(true);
    }
    Some(answer)
}

/// Keep the section a reaching machine carried on its `fleet.collect`.
/// A peer too old to carry one sends no field: nothing is recorded.
pub(crate) fn absorb_carried(conn: &Connection, machine: &str, carried: &Value) -> Result<()> {
    let (Some(as_of_ms), Some(changed_ms)) =
        (carried["as_of_ms"].as_i64(), carried["changed_ms"].as_i64())
    else {
        return Ok(());
    };
    let section = carried["section"].as_object().map(|_| &carried["section"]);
    if section.is_some_and(|s| s.to_string().len() > CARRY_BYTES) {
        return Ok(());
    }
    record_remote(conn, machine, "collect", as_of_ms, changed_ms, section)
}

/// A section's ships and needs renamed to the machine this registry knows it
/// by, and its needs marked as read elsewhere.
fn rename(section: &mut Value, name: &str) {
    section["machine"]["name"] = json!(name);
    for list in ["ships", "needs"] {
        for item in section[list].as_array_mut().into_iter().flatten() {
            item["machine"] = json!(name);
        }
    }
}

struct Shown {
    row: Value,
    section: Option<Value>,
    /// Whether its needs may be acted on: read live within the reuse window.
    live: bool,
}

fn shown(name: &str, link: &str, stored: &Stored, reachable: bool, error: Option<String>) -> Shown {
    let mut section = stored.section.clone();
    rename(&mut section, name);
    let mut row = section["machine"].clone();
    row["link"] = json!(link);
    row["reachable"] = json!(reachable);
    row["error"] = json!(error);
    row["via"] = json!(stored.via);
    row["as_of_ms"] = json!(stored.as_of_ms);
    row["changed_ms"] = json!(stored.changed_ms);
    Shown {
        live: reachable && matches!(stored.via.as_str(), "direct" | "reused"),
        row,
        section: Some(section),
    }
}

fn directory_only(name: &str, link: &str, error: &str, ships: &[(String, String)]) -> Shown {
    Shown {
        row: json!({"name": name, "link": link, "reachable": false, "error": error,
            "via": "directory", "as_of_ms": null, "changed_ms": null, "desk": null,
            "stowed": null, "truncated": false, "unknown": []}),
        section: Some(json!({"ships": ships.iter().map(|(ship, _)| json!({
            "name": ship, "machine": name, "presence": "unknown"})).collect::<Vec<_>>(),
            "needs": []})),
        live: false,
    }
}

/// Ask a machine this one reaches for its own section, or reuse a read
/// younger than `REUSE_MS`; fall back to the stored one when it does not
/// answer.
fn reached(conn: &Connection, name: &str, ships: &[(String, String)], now: i64) -> Result<Shown> {
    let previous = stored(conn, name)?;
    if let Some(previous) = previous
        .as_ref()
        .filter(|p| p.via != "collect" && now - p.as_of_ms < REUSE_MS && p.as_of_ms <= now)
    {
        let mut reused = previous.clone();
        reused.via = "reused".into();
        return Ok(shown(name, "reaches", &reused, true, None));
    }
    let since = previous.as_ref().map(|p| p.changed_ms);
    match crate::machines::snapshot_of(name, json!({"since": since})) {
        Ok(answer) => {
            let (Some(as_of), Some(changed)) =
                (answer["as_of_ms"].as_i64(), answer["changed_ms"].as_i64())
            else {
                anyhow::bail!("machine {name} answered without a section");
            };
            let section = answer["section"].as_object().map(|_| &answer["section"]);
            ensure!(
                section.is_some() || previous.is_some(),
                "machine {name} answered without a section"
            );
            record_remote(conn, name, "direct", as_of, changed, section)?;
            let now_stored = stored(conn, name)?.context("snapshot row vanished")?;
            Ok(shown(name, "reaches", &now_stored, true, None))
        }
        Err(error) => {
            let error: String = format!("{error:#}").chars().take(ERROR_CHARS).collect();
            Ok(match stored(conn, name)? {
                Some(previous) => {
                    let mut cached = previous;
                    cached.via = "cache".into();
                    shown(name, "reaches", &cached, false, Some(error))
                }
                None => directory_only(name, "reaches", &error, ships),
            })
        }
    }
}

/// A machine that reaches this one, from what it carried on `fleet.collect`.
fn reaching(conn: &Connection, name: &str, ships: &[(String, String)], now: i64) -> Result<Shown> {
    Ok(match stored(conn, name)? {
        Some(carried) => {
            let fresh = now - carried.as_of_ms < 2 * crate::machines::CONTACT_MS;
            let error = (!fresh).then(|| format!("nothing carried from {name} lately"));
            shown(name, "reached_by", &carried, fresh, error)
        }
        None => directory_only(
            name,
            "reached_by",
            &format!("links one way; nothing carried from {name} yet"),
            ships,
        ),
    })
}

/// The machine a need belongs to: the asking ship's, from the directory.
fn asking_machine(
    ship: &str,
    here: &str,
    local: &HashSet<String>,
    directory: &HashMap<String, String>,
) -> Option<String> {
    if local.contains(ship) {
        Some(here.to_owned())
    } else {
        directory.get(ship).cloned()
    }
}

/// Whether a request held here was already answered or closed here.
fn closed_here(conn: &Connection, mail_id: &str) -> bool {
    let Ok(members) = crate::fleet::members(conn) else {
        return false;
    };
    members.iter().filter(|m| m.home.is_dir()).any(|m| {
        Store::open_readonly(&m.home, Caller::Operator)
            .ok()
            .and_then(|store| {
                store
                    .connection()
                    .query_row("SELECT state FROM mail WHERE id=?1", [mail_id], |r| {
                        r.get::<_, Option<String>>(0)
                    })
                    .optional()
                    .ok()
                    .flatten()
            })
            .is_some_and(|state| state.as_deref() != Some("open"))
    })
}

pub fn snapshot(args: &Value) -> Result<Value> {
    let since = match args.get("since") {
        None | Some(Value::Null) => serde_json::Map::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(_) => anyhow::bail!("since must be an object of machine name to changed_ms"),
    };
    let conn = crate::fleet::registry()?;
    let here = crate::machines::this_machine();
    let generated_ms = Store::now_ms();
    let section = local_section(&conn, &here, generated_ms)?;
    let own = record_own(&conn, &here, &section, generated_ms)?;
    let mut all = vec![{
        let mut s = shown(&here, "here", &own, true, None);
        s.row["via"] = json!("direct");
        s
    }];
    let linked = crate::machines::linked_for_snapshot(&conn)?;
    let reached: Vec<Result<Shown>> = std::thread::scope(|scope| {
        let handles: Vec<_> = linked
            .iter()
            .filter(|(_, role, _)| role == "collects")
            .map(|(name, _, ships)| {
                scope.spawn(move || {
                    let conn = crate::fleet::registry()?;
                    reached(&conn, name, ships, Store::now_ms())
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("panicked")))
            })
            .collect()
    });
    for (machine, result) in linked
        .iter()
        .filter(|(_, role, _)| role == "collects")
        .zip(reached)
    {
        all.push(result.unwrap_or_else(|error| {
            directory_only(&machine.0, "reaches", &format!("{error:#}"), &machine.2)
        }));
    }
    for (name, role, ships) in &linked {
        if role == "collected" {
            all.push(reaching(&conn, name, ships, generated_ms)?);
        }
    }
    all.sort_by(|a, b| a.row["name"].as_str().cmp(&b.row["name"].as_str()));

    let local: HashSet<String> = crate::fleet::members(&conn)?
        .into_iter()
        .map(|m| m.name)
        .collect();
    let directory: HashMap<String, String> = linked
        .iter()
        .flat_map(|(machine, _, ships)| {
            ships.iter().map(move |(s, _)| (s.clone(), machine.clone()))
        })
        .collect();
    let mut machines = vec![];
    let mut ships = vec![];
    // mail id -> (from the asking machine's own live read, item)
    let mut needs: Vec<Value> = vec![];
    let mut by_mail: HashMap<String, (bool, usize)> = HashMap::new();
    for shown in all {
        let mut row = shown.row;
        let name = row["name"].as_str().unwrap_or("").to_owned();
        let unchanged = since
            .get(&name)
            .and_then(Value::as_i64)
            .is_some_and(|s| row["changed_ms"].as_i64() == Some(s));
        row["unchanged"] = json!(unchanged);
        machines.push(row.clone());
        if unchanged {
            continue;
        }
        let Some(section) = shown.section else {
            continue;
        };
        ships.extend(section["ships"].as_array().cloned().unwrap_or_default());
        let remote = name != here;
        for mut need in section["needs"].as_array().cloned().unwrap_or_default() {
            let mail_id = need["mail_id"].as_str().map(str::to_owned);
            if remote
                && let Some(id) = &mail_id
                && closed_here(&conn, id)
            {
                continue;
            }
            if !shown.live {
                need["as_of_ms"] = row["as_of_ms"].clone();
                need["actions"] = json!([]);
            } else {
                need["as_of_ms"] = Value::Null;
            }
            let asker = need["ship"]
                .as_str()
                .and_then(|s| asking_machine(s, &here, &local, &directory));
            let own_read = asker.as_deref().is_none_or(|a| a == name) && shown.live;
            if let Some(asker) = asker {
                need["machine"] = json!(asker);
            }
            match mail_id {
                Some(id) => match by_mail.get(&id) {
                    Some((true, _)) => {}
                    Some((false, at)) if own_read => needs[*at] = need,
                    Some(_) => {}
                    None => {
                        by_mail.insert(id, (own_read, needs.len()));
                        needs.push(need);
                    }
                },
                None => needs.push(need),
            }
        }
    }
    Ok(json!({
        "schema_version": 1, "generated_ms": generated_ms, "machine": here,
        "machines": machines, "ships": ships, "needs": needs,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_follows_the_stored_lease_and_the_holder_that_slept() {
        assert_eq!(presence_state("host:a", 100, Some("host:a"), 50), "asleep");
        // A new holder ends sleep without anyone clearing it.
        assert_eq!(presence_state("host:b", 100, Some("host:a"), 50), "hosted");
        assert_eq!(presence_state("captain-tty", 100, None, 50), "session");
        assert_eq!(presence_state("host:a", 100, Some("host:a"), 100), "absent");
    }

    #[test]
    fn a_progress_line_drops_the_runtime_lead_and_is_bounded() {
        assert_eq!(
            doing_text("Crew job j1 reports progress:\nbuilt the index\nmore"),
            "built the index"
        );
        assert_eq!(doing_text(&"x".repeat(500)).len(), DOING_CHARS);
    }

    fn registry() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("fleet.sqlite")).unwrap();
        init(&conn).unwrap();
        (dir, conn)
    }

    #[test]
    fn changed_ms_moves_only_with_content_and_never_back() {
        let (_dir, conn) = registry();
        let quiet = json!({"ships": [{"name": "a", "presence": "absent"}]});
        let first = record_own(&conn, "m", &quiet, 1_000).unwrap();
        assert_eq!(first.changed_ms, 1_000);
        // Time passes with nothing changed: the same section, the same time.
        let later = record_own(&conn, "m", &quiet, 1_000 + 3_600_000).unwrap();
        assert_eq!(later.changed_ms, 1_000);
        assert_eq!(later.as_of_ms, 3_601_000);
        // An older read than the stored one writes nothing.
        let stale = record_own(&conn, "m", &json!({"ships": []}), 2_000).unwrap();
        assert_eq!(stale.as_of_ms, 3_601_000);
        assert_eq!(stale.changed_ms, 1_000);
        // A change moves it forward even when the stored time is ahead.
        conn.execute(
            "UPDATE fleet_snapshots SET changed_ms=9_000_000 WHERE machine='m'",
            [],
        )
        .unwrap();
        let moved = record_own(&conn, "m", &json!({"ships": []}), 4_000_000).unwrap();
        assert_eq!(moved.changed_ms, 9_000_001);
    }

    #[test]
    fn a_remote_read_older_than_the_stored_one_is_ignored() {
        let (_dir, conn) = registry();
        let section = json!({"machine": {"name": "mac"}, "ships": [], "needs": []});
        record_remote(&conn, "mac", "collect", 2_000, 1_500, Some(&section)).unwrap();
        record_remote(&conn, "mac", "direct", 1_000, 900, Some(&json!({}))).unwrap();
        let kept = stored(&conn, "mac").unwrap().unwrap();
        assert_eq!((kept.as_of_ms, kept.changed_ms), (2_000, 1_500));
        assert_eq!(kept.via, "collect");
        // An unchanged answer only dates the stored section.
        record_remote(&conn, "mac", "direct", 3_000, 1_500, None).unwrap();
        let dated = stored(&conn, "mac").unwrap().unwrap();
        assert_eq!((dated.as_of_ms, dated.via.as_str()), (3_000, "direct"));
        assert_eq!(dated.section, section);
    }
}
