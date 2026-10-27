//! `fleet.snapshot`: one operator JSON of the fleet across linked machines.
//!
//! Each machine reads its own section (its homes' ships, live jobs and needs),
//! hashes it and keeps the time its content last changed in the registry's
//! `fleet_snapshots`. A machine this one reaches answers for its own section
//! over the gateway; a machine that reaches this one carries its section on
//! the `fleet.collect` it already makes. Nothing in a section is computed
//! from the current time, so it hashes the same while nothing changes. See
//! docs/design/fleet-snapshot.md.
pub(crate) mod census;
pub(crate) mod scoped;

use crate::jobs::TERMINAL_SQL;
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

pub(crate) fn init(conn: &Connection) -> Result<()> {
    if column(conn, "fleet_snapshots", "received_ms")? {
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    // Recheck under the write lock rather than matching duplicate-column errors.
    if !column(&tx, "fleet_snapshots", "received_ms")? {
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS fleet_snapshots(machine TEXT PRIMARY KEY,
               digest TEXT NOT NULL, changed_ms INTEGER NOT NULL, as_of_ms INTEGER NOT NULL,
               via TEXT NOT NULL, body TEXT NOT NULL, received_ms INTEGER);",
        )?;
        if !column(&tx, "fleet_snapshots", "received_ms")? {
            tx.execute_batch("ALTER TABLE fleet_snapshots ADD COLUMN received_ms INTEGER")?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// One machine's section as this registry stored it.
#[derive(Clone, Debug)]
pub(crate) struct Stored {
    pub changed_ms: i64,
    /// When the section was read, on the clock of the machine that read it.
    pub as_of_ms: i64,
    /// When this registry recorded it, on this machine's clock: reuse and
    /// freshness are judged by it, never by another machine's clock. None
    /// for a row stored before it was kept.
    pub received_ms: Option<i64>,
    pub via: String,
    pub section: Value,
}

pub(crate) fn stored(conn: &Connection, machine: &str) -> Result<Option<Stored>> {
    let row: Option<(i64, i64, Option<i64>, String, String)> = conn
        .query_row(
            "SELECT changed_ms, as_of_ms, received_ms, via, body FROM fleet_snapshots
             WHERE machine=?1",
            [machine],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    // A body that does not parse reads as never read.
    Ok(
        row.and_then(|(changed_ms, as_of_ms, received_ms, via, body)| {
            if body.len() > SECTION_BYTES {
                return None;
            }
            serde_json::from_str(&body).ok().map(|section| Stored {
                changed_ms,
                as_of_ms,
                received_ms,
                via,
                section,
            })
        }),
    )
}

/// This machine's own section, recorded and returned as just read:
/// `changed_ms` moves only when the content does, and never backwards, even
/// when the clock steps back. Each read replaces the stored one in its write
/// transaction; nothing is ordered by the clock, so a step back never freezes
/// the stored section. Two reads racing may store the older one, a change
/// with its own `changed_ms` that the next read replaces.
fn record_own(conn: &Connection, machine: &str, section: &Value, now: i64) -> Result<Stored> {
    let digest = crate::core::hash(section);
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let previous: Option<(String, i64)> = tx
        .query_row(
            "SELECT digest, changed_ms FROM fleet_snapshots WHERE machine=?1",
            [machine],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let changed_ms = match &previous {
        Some((old, changed)) if *old == digest => *changed,
        Some((_, changed)) => now.max(changed + 1),
        None => now,
    };
    tx.execute(
        "INSERT INTO fleet_snapshots(machine, digest, changed_ms, as_of_ms, via, body, received_ms)
         VALUES(?1, ?2, ?3, ?4, 'direct', ?5, ?4)
         ON CONFLICT(machine) DO UPDATE SET digest=excluded.digest, changed_ms=excluded.changed_ms,
           as_of_ms=excluded.as_of_ms, via=excluded.via, body=excluded.body,
           received_ms=excluded.received_ms",
        params![machine, digest, changed_ms, now, section.to_string()],
    )?;
    tx.commit()?;
    Ok(Stored {
        changed_ms,
        as_of_ms: now,
        received_ms: Some(now),
        via: "direct".into(),
        section: section.clone(),
    })
}

/// A section another machine read about itself, received here at
/// `received_ms` on this clock. It keeps that machine's `changed_ms` and
/// replaces the stored one: the last received wins, so neither clock's steps
/// can hold an old section in place. A reply without a section (unchanged
/// since the stored one) only dates it, when the stored one is still that
/// `changed_ms`. Returns what to show: the section just received, or the
/// stored one it confirmed; None when it confirmed nothing stored.
fn record_remote(
    conn: &Connection,
    machine: &str,
    via: &str,
    as_of_ms: i64,
    changed_ms: i64,
    section: Option<&Value>,
    received_ms: i64,
) -> Result<Option<Stored>> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    match section {
        Some(section) => {
            let body = section.to_string();
            ensure!(body.len() <= SECTION_BYTES, "section exceeds its bound");
            tx.execute(
                "INSERT INTO fleet_snapshots(machine, digest, changed_ms, as_of_ms, via, body,
                   received_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(machine) DO UPDATE SET digest=excluded.digest,
                   changed_ms=excluded.changed_ms, as_of_ms=excluded.as_of_ms,
                   via=excluded.via, body=excluded.body, received_ms=excluded.received_ms",
                params![
                    machine,
                    crate::core::hash(section),
                    changed_ms,
                    as_of_ms,
                    via,
                    body,
                    received_ms
                ],
            )?;
        }
        None => {
            tx.execute(
                "UPDATE fleet_snapshots SET as_of_ms=?2, via=?3, received_ms=?5
                 WHERE machine=?1 AND changed_ms=?4",
                params![machine, as_of_ms, via, changed_ms, received_ms],
            )?;
        }
    }
    let shown = match section {
        Some(section) => Some(Stored {
            changed_ms,
            as_of_ms,
            received_ms: Some(received_ms),
            via: via.to_owned(),
            section: section.clone(),
        }),
        None => stored(&tx, machine)?.filter(|s| s.changed_ms == changed_ms),
    };
    tx.commit()?;
    Ok(shown)
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

/// This machine's routes: each one the captain names and each ledger hold
/// none of them is, as `crew_routes::states` reads them at `now`. Stored
/// facts only: `until_ms` and the allowance's ends are absolute, so a route
/// reads the same until `now` crosses one of them. None when the routes file
/// or the ledger cannot be read.
fn routes(store: &Store, now: i64) -> Option<Vec<Value>> {
    let Some(policy) = crate::crew_routes::load().ok()? else {
        return Some(vec![]);
    };
    let states = match crate::jobs::ledger_readonly(store, Duration::from_millis(100)).ok()? {
        Some(ledger) => crate::crew_routes::states(&policy, &ledger, now).ok()?,
        None => {
            let empty = Connection::open_in_memory().ok()?;
            crate::routes::init(&empty).ok()?;
            crate::crew_routes::states(&policy, &empty, now).ok()?
        }
    };
    Some(states.into_iter().map(route_json).collect())
}

fn route_json(route: crate::crew_routes::RouteState) -> Value {
    let allowance = route.allowance.map(|allowance| {
        let used = route.usage.clone().unwrap_or_default();
        json!({"shape": crate::crew_routes::describe(allowance.shape),
            "window_ends_ms": used.window_ends_ms, "weekly_ends_ms": used.weekly_ends_ms,
            "window_used": used.window_utilization, "week_used": used.weekly_utilization,
            "runs": used.runs, "runs_budget": allowance.runs,
            "last_used_ms": used.last_succeeded_ms})
    });
    json!({"name": route.name, "route": route.route, "cost": route.cost, "tier": route.tier,
        "state": route.state.as_str(), "until_ms": route.until_ms,
        "error": route.error.map(|e| e.chars().take(ERROR_CHARS).collect::<String>()),
        "allowance": allowance,
        // The share of the plan spent, whatever the harness reports: a route
        // refused for want of headroom says so here rather than looking clear.
        "used": route.spent.map(|spent| json!({"share": spent.share, "window": spent.what}))})
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

/// A ship's open tasks, by the `tasks_owner` index.
const OPEN_TASKS: &str = "FROM tasks WHERE owner=?1 AND status IN ('open','running','blocked')";

/// A home's live jobs, by the `jobs_live` index.
fn live_jobs() -> String {
    format!("FROM jobs WHERE json_extract(record,'$.status') NOT IN {TERMINAL_SQL}")
}

/// Whether a job is a wake's, by the `wake_job` index.
const WAKE_OF_JOB: &str = "SELECT EXISTS(SELECT 1 FROM wakes WHERE job_id=?1)";

/// A stored lease: holder, started, expires, the holder that slept, since when.
type Lease = (String, i64, i64, Option<String>, Option<i64>);

/// One ship in its own home, read in one deferred transaction under the
/// observation budget. Every value is a stored fact; `now` only decides
/// whether a stored lease still holds and which open tasks are stale.
fn read_ship(store: &Store, ship: &str, holders: Option<&Holders>, now: i64) -> Result<Value> {
    read_ship_mode(store, ship, holders, now, false)
}

fn read_ship_mode(
    store: &Store,
    ship: &str,
    holders: Option<&Holders>,
    now: i64,
    scoped: bool,
) -> Result<Value> {
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
    let open_tasks_total: i64 =
        conn.query_row(&format!("SELECT COUNT(*) {OPEN_TASKS}"), [ship], |r| {
            r.get(0)
        })?;
    let open_tasks = conn
        .prepare(&format!(
            "SELECT id, title, status, handoff_to {OPEN_TASKS} ORDER BY rowid DESC LIMIT ?2"
        ))?
        .query_map(params![ship, TASKS_SHOWN], |r| {
            Ok(json!({"id": r.get::<_, String>(0)?,
                "title": r.get::<_, String>(1)?.chars().take(500).collect::<String>(),
                "status": r.get::<_, String>(2)?, "handoff_to": r.get::<_, Option<String>>(3)?}))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    budget.check()?;
    // Scoped reads attribute before counting/limiting. Preserve the operator
    // home's historical view of all jobs (including unknown task custody), but
    // add independently attributed totals/owners for safe cached projections.
    let owned_from = format!(
        "{} AND json_extract(record,'$.task_id') IN (SELECT id FROM tasks WHERE owner=?1)",
        live_jobs()
    );
    let owned_jobs_total: i64 =
        conn.query_row(&format!("SELECT COUNT(*) {owned_from}"), [ship], |r| {
            r.get(0)
        })?;
    budget.check()?;
    let jobs_from = if scoped {
        owned_from
    } else {
        format!("{} AND ?1 IS NOT NULL", live_jobs())
    };
    let jobs_total: i64 = conn.query_row(&format!("SELECT COUNT(*) {jobs_from}"), [ship], |r| {
        r.get(0)
    })?;
    let live: Vec<Value> = conn
        .prepare(&format!(
            "SELECT record {jobs_from} ORDER BY json_extract(record,'$.created_ms') DESC, id LIMIT ?2"
        ))?
        .query_map(params![ship, JOBS_SHOWN], |r| r.get::<_, String>(0))?
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
    let has_crew_panes = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='crew_panes')",
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
        // By the `wake_job` index, and `errands` by its key.
        let kind = if has_wakes && exists(WAKE_OF_JOB)? {
            "first_mate"
        } else if has_errands && exists("SELECT EXISTS(SELECT 1 FROM errands WHERE id=?1)")? {
            "errand"
        } else if job["action"] == "spawn" && job["agent_actor"].is_string() {
            "crew"
        } else {
            "command"
        };
        let task: Option<(String, String)> = conn
            .query_row(
                "SELECT title, owner FROM tasks WHERE id=?1",
                [task_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let task_title: Option<String> = task
            .as_ref()
            .map(|(title, _)| title.chars().take(500).collect());
        let owner = task.as_ref().map(|(_, owner)| owner);
        let park: Option<(String, String, i64, String)> = if status == "parked" && !scoped {
            conn.query_row(
                "SELECT child_job_id, next, updated_ms, root FROM crew_parks
                 WHERE crew_job_id=?1 ORDER BY updated_ms DESC LIMIT 1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
        } else {
            None
        };
        let doing = if scoped {
            Value::Null
        } else {
            match &park {
                Some((_, next, at, _)) => {
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
            }
        };
        let wait = match (status, &park) {
            ("parked", _) if scoped => json!({"for":"job"}),
            ("parked", Some((child, _, _, root))) => json!({"for": "job", "job_id": child,
                "park": crate::crew::park_count(store, root, task_id)?}),
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
        // A crew whose space was recorded can be read with `session.read`.
        let session = if has_crew_panes {
            conn.query_row("SELECT viewer FROM crew_panes WHERE job_id=?1", [id], |r| {
                r.get::<_, bool>(0)
            })
            .optional()?
            .map(|viewer| if viewer { "viewer" } else { "live" })
        } else {
            None
        };
        jobs.push(json!({
            "id": id, "task_id": task_id, "task_title": task_title, "kind": kind, "owner": owner,
            "session": session,
            "state": status, "heavy": job["heavy"].as_bool().unwrap_or(false),
            "created_ms": job["created_ms"], "spawned_ms": job.get("spawned_ms").cloned().unwrap_or(Value::Null),
            "started_ms": holders.and_then(|h| h.by_job.get(id)),
            "model_route": job.get("model_route").cloned().unwrap_or(Value::Null),
            "doing": doing, "wait": wait,
        }));
    }
    budget.check()?;
    // Last, on what is left of the budget, and never fatal: a failed or
    // over-budget warning reads null and leaves the ship on the deck.
    let stale_tasks = crate::stale_tasks::read(conn, ship, now)
        .ok()
        .flatten()
        .map(|s| s.total);
    drop(tx);
    Ok(json!({
        "name": ship, "liaison": crate::door::is_liaison(store.home()),
        "harness": harness, "hosted": crate::host::hosted(first_mate),
        "presence": presence, "presence_since_ms": since,
        "unread": unread, "open_requests": open_requests,
        "open_tasks": open_tasks, "open_tasks_total": open_tasks_total,
        "stale_tasks": stale_tasks,
        "jobs": jobs, "jobs_total": jobs_total,
        "owned_jobs_total": owned_jobs_total, "jobs_owner": scoped.then_some(ship),
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
    let mut routes_read: Option<Vec<Value>> = None;
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
            routes_read = routes(&store, now);
            if routes_read.is_none() {
                unknown.push("routes".into());
            }
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
            "truncated": truncated, "unknown": unknown,
            "routes": routes_read.unwrap_or_default()},
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
    record_remote(
        conn,
        machine,
        "collect",
        as_of_ms,
        changed_ms,
        section,
        Store::now_ms(),
    )?;
    Ok(())
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
            "stowed": null, "truncated": false, "unknown": [], "routes": []}),
        section: Some(json!({"ships": ships.iter().map(|(ship, _)| json!({
            "name": ship, "machine": name, "presence": "unknown"})).collect::<Vec<_>>(),
            "needs": []})),
        live: false,
    }
}

/// Whether a section received at `received_ms` on this clock is younger
/// than `window` at `now`. A receive time ahead of `now` (this clock stepped
/// back) is not young: the next receipt dates it again.
fn received_within(received_ms: Option<i64>, now: i64, window: i64) -> bool {
    received_ms.is_some_and(|at| at <= now && now - at < window)
}

/// Ask a machine this one reaches for its own section, or reuse a read
/// received here less than `REUSE_MS` ago; fall back to the stored one when
/// it does not answer. The answer just received is what is shown.
fn reached(conn: &Connection, name: &str, ships: &[(String, String)], now: i64) -> Result<Shown> {
    let previous = stored(conn, name)?;
    if let Some(previous) = previous
        .as_ref()
        .filter(|p| p.via != "collect" && received_within(p.received_ms, now, REUSE_MS))
    {
        let mut reused = previous.clone();
        reused.via = "reused".into();
        return Ok(shown(name, "reaches", &reused, true, None));
    }
    let since = previous.as_ref().map(|p| p.changed_ms);
    let answered = crate::machines::snapshot_of(name, json!({"since": since})).and_then(|answer| {
        let (Some(as_of), Some(changed)) =
            (answer["as_of_ms"].as_i64(), answer["changed_ms"].as_i64())
        else {
            anyhow::bail!("machine {name} answered without a section");
        };
        let section = answer["section"].as_object().map(|_| &answer["section"]);
        record_remote(
            conn,
            name,
            "direct",
            as_of,
            changed,
            section,
            Store::now_ms(),
        )?
        .with_context(|| format!("machine {name} answered without a section"))
    });
    match answered {
        Ok(answer) => Ok(shown(name, "reaches", &answer, true, None)),
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

/// A machine that reaches this one, from what it carried on `fleet.collect`,
/// fresh while received here within two contacts.
fn reaching(conn: &Connection, name: &str, ships: &[(String, String)], now: i64) -> Result<Shown> {
    Ok(match stored(conn, name)? {
        Some(carried) => {
            let fresh = received_within(carried.received_ms, now, 2 * crate::machines::CONTACT_MS);
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

/// Which of the requests `ids` a home here holds no longer open: answered
/// or closed here. One read per home for all of them, the whole pass under
/// the local budget and each home under its observation budget; a home not
/// read is named in `unknown`, and its copies count as open.
fn closed_here(conn: &Connection, ids: &[&str], unknown: &mut Vec<String>) -> HashSet<String> {
    let mut closed = HashSet::new();
    if ids.is_empty() {
        return closed;
    }
    let Ok(members) = crate::fleet::members(conn) else {
        unknown.push("answered".into());
        return closed;
    };
    let started = Instant::now();
    for member in members.iter().take(SHIP_LIMIT) {
        if !member.home.is_dir() {
            continue;
        }
        let read = || -> Result<Vec<String>> {
            ensure!(started.elapsed() < LOCAL_BUDGET, "past the local budget");
            let store = Store::open_readonly(&member.home, Caller::Operator)?;
            let budget = ObservationBudget::new(store.connection())?;
            let mut found = vec![];
            for chunk in ids.chunks(500) {
                budget.check()?;
                let marks = vec!["?"; chunk.len()].join(",");
                found.extend(
                    store
                        .connection()
                        .prepare(&format!(
                            "SELECT id FROM mail WHERE id IN ({marks}) AND state IS NOT 'open'"
                        ))?
                        .query_map(rusqlite::params_from_iter(chunk), |r| r.get(0))?
                        .collect::<rusqlite::Result<Vec<String>>>()?,
                );
            }
            Ok(found)
        };
        match read() {
            Ok(found) => closed.extend(found),
            Err(_) => unknown.push(format!("answered:{}", member.name)),
        }
    }
    closed
}

/// The argument keys of each command here; see `crate::Accepted`.
pub(crate) const ACCEPTED: crate::Accepted = &[("fleet.snapshot", &["since"])];

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
    // Needs are always returned in full: answering a need here, or a machine
    // going from live to cached, changes them without moving its
    // `changed_ms`. `since` skips only a machine's ships and routes.
    let mut sections: Vec<(String, bool, Value, Vec<Value>)> = vec![];
    for shown in all {
        let mut row = shown.row;
        let name = row["name"].as_str().unwrap_or("").to_owned();
        let unchanged = since
            .get(&name)
            .and_then(Value::as_i64)
            .is_some_and(|s| row["changed_ms"].as_i64() == Some(s));
        row["unchanged"] = json!(unchanged);
        if unchanged && let Some(fields) = row.as_object_mut() {
            fields.remove("routes");
        }
        let section = shown.section.unwrap_or(Value::Null);
        if !unchanged {
            ships.extend(section["ships"].as_array().cloned().unwrap_or_default());
        }
        let needs = section["needs"].as_array().cloned().unwrap_or_default();
        sections.push((name, shown.live, row["as_of_ms"].clone(), needs));
        machines.push(row);
    }
    let remote_ids: Vec<&str> = sections
        .iter()
        .filter(|(name, ..)| *name != here)
        .flat_map(|(_, _, _, needs)| needs.iter().filter_map(|n| n["mail_id"].as_str()))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let mut unknown_here = vec![];
    let closed = closed_here(&conn, &remote_ids, &mut unknown_here);
    let mut needs: Vec<Value> = vec![];
    // mail id -> (from the asking machine's own live read, index in needs)
    let mut by_mail: HashMap<String, (bool, usize)> = HashMap::new();
    for (name, live, as_of_ms, items) in &sections {
        let remote = *name != here;
        for mut need in items.iter().cloned() {
            let mail_id = need["mail_id"].as_str().map(str::to_owned);
            if remote && mail_id.as_ref().is_some_and(|id| closed.contains(id)) {
                continue;
            }
            if *live {
                need["as_of_ms"] = Value::Null;
            } else {
                need["as_of_ms"] = as_of_ms.clone();
                need["actions"] = json!([]);
            }
            let asker = need["ship"]
                .as_str()
                .and_then(|s| asking_machine(s, &here, &local, &directory));
            let own_read = asker.as_deref().is_none_or(|a| a == name) && *live;
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
    if let Some(row) = machines.iter_mut().find(|m| m["name"] == here.as_str())
        && let Some(listed) = row["unknown"].as_array_mut()
    {
        listed.extend(unknown_here.into_iter().map(Value::from));
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

    #[test]
    fn live_jobs_open_tasks_and_wakes_are_read_through_their_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("home")).unwrap();
        let plan = |sql: &str| -> String {
            let conn = store.connection();
            let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let rows = statement
                .query_map([], |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            rows.join("; ")
        };
        for sql in [
            format!("SELECT COUNT(*) {}", live_jobs()),
            format!(
                "SELECT record {} ORDER BY json_extract(record,'$.created_ms') DESC, id LIMIT 20",
                live_jobs()
            ),
        ] {
            assert!(plan(&sql).contains("jobs_live"), "{}", plan(&sql));
        }
        let tasks =
            format!("SELECT id {OPEN_TASKS} ORDER BY rowid DESC LIMIT 20").replace("?1", "'a'");
        assert!(plan(&tasks).contains("tasks_owner"), "{}", plan(&tasks));
        let wake = WAKE_OF_JOB.replace("?1", "'j'");
        assert!(plan(&wake).contains("wake_job"), "{}", plan(&wake));
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
    fn a_local_clock_step_back_shows_and_keeps_the_section_just_read() {
        let (_dir, conn) = registry();
        let before = json!({"ships": [{"name": "a", "presence": "hosted"}]});
        record_own(&conn, "m", &before, 3_600_000).unwrap();
        // The clock steps back an hour and the ship goes to sleep.
        let after = json!({"ships": [{"name": "a", "presence": "asleep"}]});
        let read = record_own(&conn, "m", &after, 1_000).unwrap();
        assert_eq!(read.section, after, "the read just made is shown");
        assert_eq!((read.as_of_ms, read.changed_ms), (1_000, 3_600_001));
        let kept = stored(&conn, "m").unwrap().unwrap();
        assert_eq!(kept.section, after, "and stored, not held behind the step");
        assert_eq!(kept.changed_ms, 3_600_001);
        // Still a step behind, nothing changed: the same changed_ms.
        let again = record_own(&conn, "m", &after, 2_000).unwrap();
        assert_eq!(again.changed_ms, 3_600_001);
    }

    #[test]
    fn a_received_section_is_shown_and_kept_whatever_either_clock_says() {
        let (_dir, conn) = registry();
        let first = json!({"machine": {"name": "mac"}, "ships": [], "needs": []});
        let shown = record_remote(&conn, "mac", "collect", 2_000, 1_500, Some(&first), 10_000)
            .unwrap()
            .unwrap();
        assert_eq!((shown.section, shown.received_ms), (first, Some(10_000)));
        // The mac's clock steps back: its next answer is still the one shown
        // and kept.
        let second = json!({"machine": {"name": "mac"}, "ships": [{"name": "a"}], "needs": []});
        let shown = record_remote(&conn, "mac", "direct", 1_000, 900, Some(&second), 11_000)
            .unwrap()
            .unwrap();
        assert_eq!(shown.section, second);
        let kept = stored(&conn, "mac").unwrap().unwrap();
        assert_eq!((kept.as_of_ms, kept.changed_ms), (1_000, 900));
        assert_eq!(
            (kept.via.as_str(), kept.received_ms),
            ("direct", Some(11_000))
        );
        // An unchanged answer only dates the stored section.
        let dated = record_remote(&conn, "mac", "direct", 3_000, 900, None, 12_000)
            .unwrap()
            .unwrap();
        assert_eq!((dated.as_of_ms, dated.received_ms), (3_000, Some(12_000)));
        assert_eq!(dated.section, second);
        // Unchanged since a section no longer stored confirms nothing.
        assert!(
            record_remote(&conn, "mac", "direct", 4_000, 1_500, None, 13_000)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            stored(&conn, "mac").unwrap().unwrap().received_ms,
            Some(12_000)
        );
    }

    #[test]
    fn reuse_and_freshness_follow_this_clock_not_the_remote_one() {
        let (_dir, conn) = registry();
        let now = 1_800_000_000_000;
        let section = json!({"machine": {"name": "mac"}, "ships": [], "needs": []});
        // The mac's clock runs an hour ahead of this one.
        let ahead = now + 3_600_000;
        record_remote(&conn, "mac", "collect", ahead, ahead, Some(&section), now).unwrap();
        let contact = crate::machines::CONTACT_MS;
        assert_eq!(
            reaching(&conn, "mac", &[], now + 1_000).unwrap().row["reachable"],
            true
        );
        // Two contacts with nothing carried: stale, though its as_of_ms is
        // still ahead of this clock.
        let quiet = reaching(&conn, "mac", &[], now + 2 * contact).unwrap();
        assert_eq!(quiet.row["reachable"], false);
        assert_eq!(quiet.row["as_of_ms"], ahead);
        // This clock steps back past the receipt: not fresh until the next.
        assert_eq!(
            reaching(&conn, "mac", &[], now - 60_000).unwrap().row["reachable"],
            false
        );

        // A direct read from that clock is reused while received lately.
        record_remote(&conn, "mac", "direct", ahead, ahead, Some(&section), now).unwrap();
        let reused = reached(&conn, "mac", &[], now + 1_000).unwrap();
        assert_eq!(reused.row["via"], "reused");
        assert!(reused.live);
        assert!(received_within(Some(now), now + REUSE_MS - 1, REUSE_MS));
        assert!(!received_within(Some(now), now + REUSE_MS, REUSE_MS));
        assert!(!received_within(Some(now), now - 1, REUSE_MS));
        assert!(!received_within(None, now, REUSE_MS));
    }

    #[test]
    fn an_older_table_gains_the_receive_time() {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("fleet.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE fleet_snapshots(machine TEXT PRIMARY KEY,
               digest TEXT NOT NULL, changed_ms INTEGER NOT NULL, as_of_ms INTEGER NOT NULL,
               via TEXT NOT NULL, body TEXT NOT NULL);
             INSERT INTO fleet_snapshots VALUES('mac', 'd', 1, 2, 'direct', '{}');",
        )
        .unwrap();
        init(&conn).unwrap();
        init(&conn).unwrap();
        let old = stored(&conn, "mac").unwrap().unwrap();
        assert_eq!(old.received_ms, None);
        assert_eq!(
            reaching(&conn, "mac", &[], 3).unwrap().row["reachable"],
            false
        );
    }
}
