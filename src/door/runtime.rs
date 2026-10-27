//! Opt-in metadata projection. No operator snapshot or public_job serialization.
//! V1 observes local records only: execution/session census is a later slice.
use super::{
    coverage,
    scoped::{DelegatedContext, Denial},
};
use crate::{
    needs::ObservationBudget,
    store::{Caller, Store},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    path::Path,
    time::{Duration, Instant},
};

pub(super) const SCHEMA: &str = "runtime.v1";
const SHIP_LIMIT: usize = 200;
const SCAN_LIMIT: usize = 2000;
const READ_BUDGET: Duration = Duration::from_secs(3);

/// Selectors are labels resolved in the core's directory, never paths/endpoints.
pub(super) fn validate_args(args: &Value) -> Result<()> {
    let fields = args
        .as_object()
        .context("runtime arguments must be an object")?;
    ensure!(
        fields
            .keys()
            .all(|k| ["schema", "machine", "ship"].contains(&k.as_str())),
        "invalid runtime arguments"
    );
    ensure!(args["schema"] == SCHEMA, "unsupported runtime schema");
    for field in ["machine", "ship"] {
        if let Some(value) = fields.get(field) {
            let label = value.as_str().context("selector must be a handle")?;
            ensure!(label.len() <= 128, "selector too long");
            crate::store::identifier(label)?;
        }
    }
    Ok(())
}

/// Deliberately closed DTO: no arbitrary record fragments or authored strings.
#[derive(Serialize)]
struct Ship {
    name: String,
    machine: String,
    attended: Option<bool>,
    dispatcher: Option<bool>,
    recorded_running_jobs: Option<u64>,
    open_tasks: Option<u64>,
    activity_evidence: ActivityEvidence,
    working: Working,
    working_reason: &'static str,
    observed_at: Option<i64>,
}

#[derive(Default, Serialize)]
struct ActivityEvidence {
    no_nonterminal_jobs: Option<bool>,
    no_sessions_or_leases: Option<bool>,
    ledger_clear: Option<bool>,
    dispatcher_running: Option<bool>,
    observed_ms: Option<i64>,
}
impl ActivityEvidence {
    fn idle(&self) -> bool {
        [
            self.no_nonterminal_jobs,
            self.no_sessions_or_leases,
            self.ledger_clear,
            self.dispatcher_running,
        ]
        .iter()
        .all(|v| *v == Some(true))
    }
}

#[derive(Clone, Copy)]
enum Working {
    Yes,
    No,
    Unknown,
}
impl Serialize for Working {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Yes => serializer.serialize_bool(true),
            Self::No => serializer.serialize_bool(false),
            Self::Unknown => serializer.serialize_str("unknown"),
        }
    }
}

/// Only affirmative checked execution or complete fresh local negative evidence
/// can decide. V1 supplies no affirmative execution evidence; stored running
/// state alone is not execution evidence.
fn working(executing: bool, census_complete: bool) -> Working {
    if executing {
        Working::Yes
    } else if census_complete {
        Working::No
    } else {
        Working::Unknown
    }
}

fn readonly(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_millis(25))?;
    conn.execute_batch("PRAGMA query_only=ON;")?;
    Ok(conn)
}

struct Entry {
    name: String,
    machine: String,
    home: Option<std::path::PathBuf>,
}

/// Bounded directory read. Filter before consuming the visible result budget.
fn directory(
    conn: &Connection,
    context: &DelegatedContext,
    args: &Value,
) -> Result<(Vec<Entry>, bool)> {
    let budget = ObservationBudget::new(conn)?;
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Deferred)?;
    let mut entries = vec![];
    let mut scanned = 0;
    for (sql, local) in [
        (
            "SELECT name, home FROM ships WHERE retired_ms IS NULL ORDER BY name LIMIT 2001",
            true,
        ),
        (
            "SELECT name, machine FROM machine_ships ORDER BY name, machine LIMIT 2001",
            false,
        ),
    ] {
        let Ok(mut statement) = tx.prepare(sql) else {
            return Ok((entries, false));
        };
        let Ok(rows) =
            statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        else {
            return Ok((entries, false));
        };
        for row in rows {
            if budget.check().is_err() {
                return Ok((entries, false));
            }
            scanned += 1;
            if scanned > SCAN_LIMIT {
                return Ok((entries, false));
            }
            let Ok((name, value)) = row else {
                return Ok((entries, false));
            };
            if !context.allows(&name) {
                continue;
            }
            let machine = if local { "local" } else { &value };
            if args["ship"].as_str().is_some_and(|s| s != name)
                || args["machine"].as_str().is_some_and(|m| m != machine)
            {
                continue;
            }
            if entries.len() == SHIP_LIMIT {
                return Ok((entries, false));
            }
            entries.push(Entry {
                name,
                machine: if local { "local".into() } else { value.clone() },
                home: local.then(|| value.into()),
            });
        }
    }
    Ok((entries, true))
}

fn source(
    entry: &Entry,
    facet: coverage::RuntimeFacet,
    state: &'static str,
    reason: &'static str,
    at: Option<i64>,
) -> coverage::RuntimeSource {
    coverage::RuntimeSource {
        source: coverage::Source {
            id: format!("{}:{}:{}", entry.machine, entry.name, facet.as_str()),
            state,
            through: None,
            barrier: None,
        },
        machine: entry.machine.clone(),
        ship: Some(entry.name.clone()),
        facet,
        mode: if entry.home.is_some() {
            "local_live"
        } else {
            "directory"
        },
        observed_at: at,
        reason,
    }
}

/// A coherent read of recorded attendance and job counts, not process liveness.
struct HomeFacts {
    attended: bool,
    running: u64,
    open_tasks: u64,
    no_jobs: bool,
    no_sessions: bool,
}
fn home_facts(home: &Path, ship: &str) -> Result<HomeFacts> {
    let store = Store::open_readonly(home, Caller::Operator)?;
    let budget = ObservationBudget::new(store.connection())?;
    let tx = rusqlite::Transaction::new_unchecked(
        store.connection(),
        rusqlite::TransactionBehavior::Deferred,
    )?;
    let owned: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM ships WHERE name=?1 AND json_extract(record,'$.location')='here')", [ship], |r| r.get(0))?;
    ensure!(owned, "home owner changed");
    let attended = crate::presence::attended(&store, ship)?;
    budget.check()?;
    let running = tx.query_row("SELECT COUNT(*) FROM jobs WHERE json_extract(record,'$.status') IN ('running','launching') AND json_extract(record,'$.task_id') IN (SELECT id FROM tasks WHERE owner=?1)", [ship], |r| r.get(0))?;
    budget.check()?;
    // Unknown task custody is an unreadable observation, never idle evidence.
    let custody_known: bool = tx.query_row(&format!("SELECT NOT EXISTS(SELECT 1 FROM jobs WHERE (json_extract(record,'$.status') IS NULL OR json_extract(record,'$.status') NOT IN {}) AND NOT EXISTS(SELECT 1 FROM tasks WHERE id=json_extract(jobs.record,'$.task_id')))", crate::jobs::TERMINAL_SQL), [], |r| r.get(0))?;
    ensure!(custody_known, "job custody unknown");
    let no_jobs = tx.query_row(&format!("SELECT NOT EXISTS(SELECT 1 FROM jobs WHERE (json_extract(record,'$.status') IS NULL OR json_extract(record,'$.status') NOT IN {}) AND json_extract(record,'$.task_id') IN (SELECT id FROM tasks WHERE owner=?1))", crate::jobs::TERMINAL_SQL), [ship], |r| r.get(0))?;
    let no_sessions = tx.query_row("SELECT NOT EXISTS(SELECT 1 FROM presence WHERE ship=?1) AND NOT EXISTS(SELECT 1 FROM crew_panes WHERE ship=?1)", [ship], |r| r.get(0))?;
    let open_tasks = tx.query_row(
        "SELECT COUNT(*) FROM tasks WHERE owner=?1 AND status IN ('open','running','blocked')",
        [ship],
        |r| r.get(0),
    )?;
    budget.check()?;
    Ok(HomeFacts {
        attended,
        running,
        open_tasks,
        no_jobs,
        no_sessions,
    })
}

/// Missing ledger is unknown, not an empty host. Never run ledger initialization,
/// reservations reconciliation, process inspection or expose shared host totals.
fn ledger_clear(home: &Path, ship: &str) -> Result<bool> {
    let store = Store::open_readonly(home, Caller::Operator)?;
    let path = crate::jobs::ledger_dir(&store)?.join("resources.sqlite");
    crate::store::reject_symlinks(&path)?;
    let conn = readonly(&path)?;
    let budget = ObservationBudget::new(&conn)?;
    let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Deferred)?;
    let mut clear = true;
    for table in ["reservations", "queue"] {
        budget.check()?;
        let attributed: bool = tx.query_row(&format!("SELECT NOT EXISTS(SELECT 1 FROM {table} WHERE json_extract(record,'$.ship') IS NULL AND json_extract(record,'$.home') IS NULL)"), [], |r| r.get(0))?;
        ensure!(attributed, "ledger attribution unknown");
        let exists: bool = tx.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE json_extract(record,'$.ship')=?1 OR (json_extract(record,'$.ship') IS NULL AND json_extract(record,'$.home')=?2))"),
            rusqlite::params![ship, store.home().to_string_lossy()], |r| r.get(0))?;
        clear &= !exists;
    }
    Ok(clear)
}

pub(super) fn read(args: &Value, context: &DelegatedContext) -> Result<Value> {
    validate_args(args).map_err(|_| Denial::Arguments)?;
    if args["ship"].as_str().is_some_and(|s| !context.allows(s)) {
        return Err(Denial::Ship.into());
    }
    let started = Instant::now();
    let path = crate::fleet::registry_path()?;
    let directory = readonly(&path).and_then(|c| directory(&c, context, args));
    let (entries, inventory_complete) = directory.unwrap_or_default();
    let selected: Vec<_> = entries
        .into_iter()
        .filter(|e| {
            args["ship"].as_str().is_none_or(|s| e.name == s)
                && args["machine"].as_str().is_none_or(|m| e.machine == m)
        })
        .collect();
    // Unknown and forbidden selectors have exactly the same nonrevealing denial.
    if selected.is_empty() && (args.get("ship").is_some() || args.get("machine").is_some()) {
        return Err(Denial::Ship.into());
    }
    let mut ships = vec![];
    let mut sources = vec![];
    for entry in &selected {
        use coverage::RuntimeFacet::*;
        let mut row = Ship {
            name: entry.name.clone(),
            machine: entry.machine.clone(),
            attended: None,
            dispatcher: None,
            recorded_running_jobs: None,
            open_tasks: None,
            activity_evidence: ActivityEvidence::default(),
            working: working(false, false),
            working_reason: "census_deferred",
            observed_at: None,
        };
        sources.push(source(
            entry,
            Inventory,
            if entry.home.is_some() {
                "covered"
            } else {
                "cached"
            },
            if entry.home.is_some() {
                "local_directory"
            } else {
                "directory_only"
            },
            entry.home.as_ref().map(|_| Store::now_ms()),
        ));
        let mut reason = "directory_only";
        if let Some(home) = &entry.home {
            reason = "read_budget";
            if started.elapsed() < READ_BUDGET {
                match home_facts(home, &entry.name) {
                    Ok(facts) => {
                        row.attended = Some(facts.attended);
                        row.recorded_running_jobs = Some(facts.running);
                        row.open_tasks = Some(facts.open_tasks);
                        // Missing/unreadable service/ledger evidence stays unknown.
                        row.dispatcher = service(home).ok();
                        let ledger = ledger_clear(home, &entry.name).ok();
                        row.activity_evidence = ActivityEvidence {
                            no_nonterminal_jobs: Some(facts.no_jobs),
                            no_sessions_or_leases: Some(facts.no_sessions),
                            ledger_clear: ledger,
                            dispatcher_running: row.dispatcher,
                            observed_ms: Some(Store::now_ms()),
                        };
                        row.observed_at = row.activity_evidence.observed_ms;
                        row.working = working(false, row.activity_evidence.idle());
                        reason = if row.dispatcher.is_none() {
                            "service_unreadable"
                        } else if ledger.is_none() {
                            "ledger_unreadable"
                        } else if row.activity_evidence.idle() {
                            "fresh_local_negative_evidence"
                        } else if row.dispatcher == Some(false) {
                            "dispatcher_not_running"
                        } else {
                            "census_deferred"
                        };
                    }
                    Err(_) => reason = "home_unreadable",
                }
            }
        }
        row.working_reason = reason;
        for facet in [Workers, Sessions, Jobs, Work, Capacity, Needs, Mail] {
            let (state, why) = match facet {
                Jobs if row.recorded_running_jobs.is_some() => ("covered", "recorded_state_only"),
                Workers if row.activity_evidence.idle() => ("covered", reason),
                Workers | Sessions => ("unavailable", reason),
                _ => ("unsupported", "facet_deferred"),
            };
            sources.push(source(
                entry,
                facet,
                state,
                why,
                if state == "covered" {
                    row.observed_at
                } else {
                    None
                },
            ));
        }
        ships.push(row);
    }
    let observed_running: u64 = ships.iter().filter_map(|s| s.recorded_running_jobs).sum();
    let counts_complete =
        inventory_complete && ships.iter().all(|s| s.recorded_running_jobs.is_some());
    let machines: std::collections::BTreeSet<_> = ships.iter().map(|s| s.machine.clone()).collect();
    let contributors: Vec<_> = sources
        .iter()
        .filter(|s| s.facet.as_str() == "jobs" && s.source.state == "covered")
        .map(|s| s.source.id.clone())
        .collect();
    Ok(
        json!({"schema":SCHEMA, "coverage":coverage::runtime_domain(&sources, inventory_complete),
        "selection":{"machine":args.get("machine"),"ship":args.get("ship")},
        "machines":machines, "ships":ships,
        "observed_totals":{"scope":if args.get("machine").is_some() || args.get("ship").is_some() { "selected" } else { "local" },
            "recorded_running_jobs":{"value":observed_running,"complete":counts_complete,"sources":contributors},
            "active_workers":{"value":null,"complete":false,"sources":[]}},
        "writes":"none"}),
    )
}

/// service::held uses Path::exists (which hides stat errors) and treats all
/// flock failures as contention. Observation must distinguish those failures.
pub(super) fn service(home: &Path) -> Result<bool> {
    use std::os::fd::AsRawFd;
    ensure!(
        std::fs::symlink_metadata(home)?.file_type().is_dir(),
        "invalid service home"
    );
    let path = home.join("serve.lock");
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        result => ensure!(result?.file_type().is_file(), "invalid service lock"),
    }
    let file = std::fs::OpenOptions::new().read(true).open(path)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(false);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(true)
    } else {
        Err(error.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn working_requires_execution_or_complete_census() {
        assert_eq!(json!(working(true, false)), true);
        assert_eq!(json!(working(false, true)), false);
        assert_eq!(json!(working(false, false)), "unknown");
    }
    #[test]
    fn coverage_runtime_validation_and_redaction() {
        let entry = Entry {
            name: "survey".into(),
            machine: "local".into(),
            home: Some("/unused".into()),
        };
        let sources = [source(
            &entry,
            coverage::RuntimeFacet::Workers,
            "unavailable",
            "census_deferred",
            None,
        )];
        let good = coverage::runtime_domain(&sources, true);
        assert!(coverage::validate(&good, &["relay"]).is_ok());
        assert!(coverage::validate(&good, &["survey"]).is_err());
        for (key, value) in [
            ("complete", json!(true)),
            ("owner", json!({})),
            ("schema", json!("coverage.v2")),
        ] {
            let mut bad = good.clone();
            bad[key] = value;
            assert!(coverage::validate(&bad, &[]).is_err());
        }
        let mut bad = good.clone();
        bad["sources"][0]["argv"] = json!(["canary"]);
        assert!(coverage::validate(&bad, &[]).is_err());
        let mut bad = good;
        bad["sources"][0]["facet"] = json!("invented");
        assert!(coverage::validate(&bad, &[]).is_err());
    }
    #[test]
    fn restricted_directory_keeps_known_inventory_on_partial_failure() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE ships(name TEXT,home TEXT,retired_ms INTEGER);
            INSERT INTO ships VALUES('survey','/unused',NULL),('relay','/unused',NULL);",
        )
        .unwrap();
        let context = DelegatedContext::resolved(
            "lookout".into(),
            "door:lookout".into(),
            vec!["fleet:read".into()],
            Some(vec!["survey".into()]),
        );
        let (entries, complete) = directory(&conn, &context, &json!({})).unwrap();
        assert!(!complete, "missing remote directory is unknown, not empty");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "survey");
    }
    #[test]
    fn strict_version_and_handles() {
        for args in [
            json!({}),
            json!({"schema":"runtime.v2"}),
            json!({"schema":SCHEMA,"ship":"/tmp/survey"}),
            json!({"schema":SCHEMA,"cwd":"canary"}),
        ] {
            assert!(validate_args(&args).is_err());
        }
        assert!(validate_args(&json!({"schema":SCHEMA,"machine":"alpha","ship":"survey"})).is_ok());
    }
    #[test]
    fn local_facts_fail_closed_and_do_not_initialize() {
        let dir = tempfile::tempdir().unwrap();
        assert!(home_facts(dir.path(), "survey").is_err());
        assert!(!dir.path().join("privateer.sqlite").exists());
        assert!(!service(dir.path()).unwrap());
        std::fs::create_dir(dir.path().join("serve.lock")).unwrap();
        assert!(service(dir.path()).is_err());
    }
}
