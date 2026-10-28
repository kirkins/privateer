//! Sanitized observation of one host ledger. Never opens its writable lifecycle,
//! prunes the queue, checks/releases processes, or counts a ledger per ship.
use super::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Policy {
    pub configured_host_heavy_cap: Option<u64>,
    pub resolved_host_heavy_cap: Option<u64>,
    pub max_load_per_cpu: f64,
    pub min_available_memory_bytes: u64,
    pub min_free_disk_bytes: u64,
    pub ramp_secs: u64,
    pub heavy_estimate: admission::Estimate,
    pub crew_default_memory_bytes: u64,
    pub crew_memory: Vec<CrewMemory>,
    pub sources: BTreeMap<String, String>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CrewMemory {
    pub harness: String,
    pub memory_bytes: u64,
    pub source: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Hold {
    pub ship: Option<String>,
    pub job_id: Option<String>,
    pub incarnation: Option<String>,
    pub task_id: Option<String>,
    pub heavy: bool,
    pub admitted_ms: i64,
    pub demand: Option<Demand>,
    pub liveness: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Queue {
    pub job_id: Option<String>,
    pub created_ms: i64,
    pub blocked: Option<String>,
    pub blocked_at_ms: Option<i64>,
    pub blocker_basis: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Disk {
    pub filesystem: String,
    pub available_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub committed_bytes: Option<u64>,
    pub state: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Readings {
    pub cpus: f64,
    pub load: f64,
    pub memory_available_bytes: u64,
    pub memory_total_bytes: u64,
    pub released_load: f64,
    pub recent_cpus: f64,
    pub recent_memory_bytes: u64,
    pub disks: Vec<Disk>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Capacity {
    pub ledger: String,
    pub policy: Option<Policy>,
    pub charter_heavy_cap: Option<u64>,
    pub charter_policy_state: String,
    pub charter_policy_source: String,
    pub charter_occupied_heavy_slots: Option<u64>,
    pub occupied_heavy_slots: u64,
    pub free_heavy_slots: Option<u64>,
    pub headroom_basis: String,
    pub reservation_count: u64,
    pub memory_reservation_count: u64,
    pub holds: Vec<Hold>,
    pub queue_depth: u64,
    pub queue: Vec<Queue>,
    pub capacity_blocked: bool,
    pub readings: Option<Readings>,
    pub route_availability: Option<BTreeMap<String, u64>>,
    pub route_state: String,
    pub shared_host: String,
}
impl Hold {
    /// A hold outlives its job (retention, retry, custody changes). Keep the
    /// reservation conservatively, but never let an unprovable reference turn
    /// it into an error or disclose a job/task now belonging to another ship.
    pub(crate) fn recheck(
        &mut self,
        store: &Store,
        ship: &str,
        source: &str,
        context: &crate::door::scoped::DelegatedContext,
    ) {
        let task_visible = self.task_id.as_deref().is_some_and(|task| {
            store
                .connection()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1 AND owner=?2)",
                    params![task, ship],
                    |r| r.get::<_, bool>(0),
                )
                .unwrap_or(false)
        });
        let reference = match (&self.job_id, &self.incarnation, &self.task_id) {
            (Some(id), Some(incarnation), Some(task_id)) if task_visible => {
                Some(crate::snapshot::inspection::JobRef {
                    origin: source.into(),
                    ship: ship.into(),
                    id: id.clone(),
                    incarnation: incarnation.clone(),
                    task_id: task_id.clone(),
                })
            }
            _ => None,
        };
        if reference.is_none_or(|r| {
            crate::snapshot::inspection::check_reference(store, ship, &r, context).is_err()
        }) {
            self.job_id = None;
            self.incarnation = None;
        }
        if !task_visible {
            self.task_id = None;
        }
    }
}
fn handle(s: &str) -> bool {
    crate::snapshot::inspection::handle(s)
}

/// A selected ship's entries are restricted in SQL *before* JSON expansion and
/// aggregation. In particular we never compute a hidden total then subtract.
pub(crate) fn read(
    store: Option<&Store>,
    ship: Option<&str>,
    task: Option<&str>,
    shared: bool,
    context: &crate::door::scoped::DelegatedContext,
) -> Result<Capacity> {
    ensure!(shared || ship.is_some(), "selected ship required");
    let home = store.map(Store::home);
    let _home_budget = store
        .map(|s| crate::needs::ObservationBudget::new(s.connection()))
        .transpose()?;
    let path = ledger_dir_at(home)?.join("resources.sqlite");
    crate::store::reject_symlinks(&path)?;
    ensure!(path.is_file(), "ledger unavailable");
    let conn = Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    if !shared {
        // Cross-file custody must be filtered in SQL before the queue LIMIT,
        // not by first bounding every (possibly hidden) queue ID in this home.
        let database = store
            .context("selected home missing")?
            .home()
            .join("privateer.sqlite");
        crate::store::reject_symlinks(&database)?;
        let mut uri = String::from("file:");
        use std::fmt::Write as _;
        for byte in database.as_os_str().as_encoded_bytes() {
            if byte.is_ascii_alphanumeric() || b"/._-~".contains(byte) {
                uri.push(char::from(*byte));
            } else {
                write!(&mut uri, "%{byte:02X}")?;
            }
        }
        uri.push_str("?mode=ro");
        conn.execute("ATTACH DATABASE ?1 AS custody", [&uri])?;
    }
    let budget = crate::needs::ObservationBudget::new(&conn)?;
    let tx = conn.unchecked_transaction()?;
    // Bounded, conservative: all recorded holds count, including stale, escaped
    // and unverified payloads. Reading is not identity-checked recovery.
    let filter = if shared {
        "1"
    } else {
        "json_extract(record,'$.ship')=?1 AND json_extract(record,'$.home')=?2"
    };
    let home_string = home.map(|p| p.to_string_lossy().into_owned());
    let source = if shared {
        None
    } else {
        Some(crate::snapshot::inspection::origin(
            store.context("selected home missing")?,
        )?)
    };
    let mut records = vec![];
    let mut holds = vec![];
    let sql = format!(
        "SELECT CASE WHEN length(CAST(record AS BLOB))<=65536 THEN record ELSE '{{}}' END FROM reservations WHERE {filter} ORDER BY key LIMIT 2001"
    );
    let mut stmt = tx.prepare(&sql)?;
    let mut rows = if shared {
        stmt.query([])?
    } else {
        stmt.query(params![ship, home_string])?
    };
    while let Some(row) = rows.next()? {
        budget.check()?;
        let raw: String = row.get(0)?;
        ensure!(raw.len() <= 65536 && records.len() < 2000, "capacity bound");
        let mut r: Reservation = serde_json::from_str(&raw)?;
        let mut hold = Hold {
            ship: r.ship.as_ref().filter(|s| handle(s)).cloned(),
            job_id: handle(&r.job_id).then(|| r.job_id.clone()),
            incarnation: handle(&r.incarnation).then(|| r.incarnation.clone()),
            task_id: r.task.as_ref().filter(|s| handle(s)).cloned(),
            heavy: r.heavy,
            admitted_ms: r.created_ms,
            demand: r.demand,
            liveness: "unknown_held_conservatively".into(),
        };
        if let Some(source) = &source {
            hold.recheck(store.unwrap(), ship.unwrap(), source, context);
            // Heavy-slot counting does not need shared disk-trim history.
            r.disk_trimmed_at_admit = None;
        }
        holds.push(hold);
        records.push(r);
    }
    drop(rows);
    drop(stmt);
    let mut queue = vec![];
    // Queue records do not carry ship ownership: join exact local job/task
    // custody, never expose a global position or the shared queue head.
    let (visible_jobs, visible_tasks) = if !shared {
        let s = store.context("selected home missing")?;
        // Current selected custody, including supervised waiters whose task
        // closed. Neither hidden queues nor lifetime task history consume the
        // selected result budget. The attached home is explicitly read-only.
        let mut stmt = tx.prepare("SELECT json_extract(q.record,'$.job_id') FROM queue q WHERE json_extract(q.record,'$.home')=?1 AND json_extract(q.record,'$.kind')='workspace' AND EXISTS (SELECT 1 FROM custody.tasks t WHERE t.id=json_extract(q.record,'$.job_id') AND t.owner=?2 AND (t.status NOT IN ('done','cancelled') OR json_type(q.record,'$.supervisor')='object')) ORDER BY q.key LIMIT 2001")?;
        let ids = stmt
            .query_map(params![home_string, ship], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(ids.len() <= 2000, "queue custody budget");
        (
            crate::snapshot::inspection::visible_job_ids(s, ship.unwrap(), &["queued"], context)?,
            serde_json::to_string(&ids)?,
        )
    } else {
        (String::new(), String::new())
    };
    let sql = if shared {
        "SELECT CASE WHEN length(CAST(record AS BLOB))<=65536 THEN record ELSE '{}' END FROM queue ORDER BY json_extract(record,'$.created_ms'),key LIMIT 2001"
    } else {
        "SELECT CASE WHEN length(CAST(record AS BLOB))<=65536 THEN record ELSE '{}' END FROM queue WHERE json_extract(record,'$.home')=?1 AND ((json_extract(record,'$.kind')='workspace' AND json_extract(record,'$.job_id') IN (SELECT value FROM json_each(?3))) OR (json_extract(record,'$.kind') IS NOT 'workspace' AND json_extract(record,'$.job_id') IN (SELECT value FROM json_each(?2)))) ORDER BY json_extract(record,'$.created_ms'),key LIMIT 2001"
    };
    let mut stmt = tx.prepare(sql)?;
    let mut rows = if shared {
        stmt.query([])?
    } else {
        stmt.query(params![home_string, visible_jobs, visible_tasks])?
    };
    while let Some(row) = rows.next()? {
        budget.check()?;
        let raw: String = row.get(0)?;
        ensure!(raw.len() <= 65536 && queue.len() < 2000, "queue bound");
        let q: Queued = serde_json::from_str(&raw)?;
        if !shared {
            let s = store.context("selected home missing")?;
            let owner: Option<String> = if q.kind.as_deref() == Some("workspace") {
                s.connection()
                    .query_row("SELECT owner FROM tasks WHERE id=?1", [&q.job_id], |r| {
                        r.get(0)
                    })
                    .optional()?
            } else {
                s.connection().query_row("SELECT t.owner FROM jobs j JOIN tasks t ON t.id=json_extract(j.record,'$.task_id') WHERE j.id=?1 AND json_extract(j.record,'$.incarnation')=?2", params![q.job_id,q.incarnation], |r| r.get(0)).optional()?
            };
            if owner.as_deref() != ship {
                continue;
            }
            if q.kind.as_deref() == Some("workspace") && q.supervisor.is_none() {
                let closed: bool = s.connection().query_row(
                    "SELECT status IN ('done','cancelled') FROM tasks WHERE id=?1",
                    [&q.job_id],
                    |r| r.get(0),
                )?;
                if closed {
                    continue;
                }
            }
            if q.kind.as_deref() != Some("workspace") {
                let task_id: String = s.connection().query_row(
                    "SELECT json_extract(record,'$.task_id') FROM jobs WHERE id=?1",
                    [&q.job_id],
                    |r| r.get(0),
                )?;
                let reference = crate::snapshot::inspection::JobRef {
                    origin: crate::snapshot::inspection::origin(s)?,
                    incarnation: q.incarnation.clone(),
                    ship: ship.unwrap().into(),
                    task_id,
                    id: q.job_id.clone(),
                };
                if crate::snapshot::inspection::check_reference(
                    s,
                    ship.unwrap(),
                    &reference,
                    context,
                )
                .is_err()
                {
                    continue;
                }
            }
        }
        queue.push(Queue {
            job_id: handle(&q.job_id).then_some(q.job_id),
            created_ms: q.created_ms,
            blocked: if shared {
                q.blocked.map(|b| {
                    match b {
                        Blocker::Disk => "disk",
                        Blocker::Cpu => "cpu",
                        Blocker::HeavySlot => "heavy_slot",
                    }
                    .to_owned()
                })
            } else {
                Some("capacity".into())
            },
            blocked_at_ms: if shared { q.blocked_ms } else { None },
            blocker_basis: if shared {
                "last_head_classification_not_current_diagnosis"
            } else {
                "unattributed_capacity_wait"
            }
            .into(),
        });
    }
    drop(rows);
    drop(stmt);
    let held = held_view(&tx, &records)?;
    let settings = if shared {
        let oversized:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM settings WHERE key='admission' AND length(CAST(value AS BLOB))>65536)",[],|r|r.get(0))?;
        ensure!(!oversized, "settings byte budget");
        Some(admission::settings(&tx)?)
    } else {
        None
    };
    let charter = if let Some(task) = task {
        let s = store.context("selected home missing")?;
        ensure!(s.task(task)?["owner"].as_str() == ship, "task not allowed");
        if shared {
            let mut seen = BTreeSet::new();
            s.observe_active_charter(task, |id| {
                budget.check()?;
                ensure!(seen.insert(id.to_owned()), "cyclic ancestry");
                Ok(())
            })?
        } else {
            // Withheld policy must not be read: hidden ancestor/charter data
            // cannot change the availability of selected-ship occupancy.
            None
        }
    } else {
        None
    };
    let charter_cap = charter.as_ref().and_then(|c| c["max_heavy_jobs"].as_u64());
    let charter_id = charter
        .as_ref()
        .and_then(|c| c["id"].as_str())
        .unwrap_or("");
    let (occupied, share, free) = admission::heavy_slots(
        &held,
        home.unwrap_or(Path::new("")),
        charter_id,
        settings.as_ref().and_then(|s| s.max_heavy_jobs),
        charter_cap,
    );
    let policy = if let Some(s) = &settings {
        let keys = admission::configured_keys(&tx)?;
        let sources = [
            "max_heavy_jobs",
            "max_load_per_cpu",
            "min_available_memory_bytes",
            "min_free_disk_bytes",
            "ramp_secs",
            "heavy_estimate",
            "crew_default_memory_bytes",
        ]
        .into_iter()
        .map(|k| {
            (
                k.into(),
                if keys.iter().any(|s| s == k) {
                    "host_configuration"
                } else {
                    "runtime_default"
                }
                .into(),
            )
        })
        .collect();
        Some(Policy {
            configured_host_heavy_cap: if keys.iter().any(|k| k == "max_heavy_jobs") {
                s.max_heavy_jobs
            } else {
                None
            },
            resolved_host_heavy_cap: s.max_heavy_jobs,
            max_load_per_cpu: s.max_load_per_cpu,
            min_available_memory_bytes: s.min_available_memory_bytes,
            min_free_disk_bytes: s.min_free_disk_bytes,
            ramp_secs: s.ramp_secs,
            heavy_estimate: s.heavy_estimate.clone(),
            crew_default_memory_bytes: s.crew_default_memory_bytes,
            crew_memory: crate::bundled::manifests()
                .iter()
                .filter(|(id, _)| crate::snapshot::inspection::HARNESSES.contains(id))
                .map(|(id, raw)| {
                    let manifest: Value = serde_json::from_str(raw)?;
                    let memory = crate::plugins::manifest_memory(&manifest)?;
                    let (memory_bytes, source) = admission::crew_memory_source(s, id, memory, None);
                    Ok(CrewMemory {
                        harness: (*id).into(),
                        memory_bytes,
                        source: source.into(),
                    })
                })
                .collect::<Result<_>>()?,
            sources,
        })
    } else {
        None
    };
    let readings = if let Some(settings) = &settings {
        let host = admission::measure()?;
        let now = Store::now_ms();
        let (recent_cpus, recent_memory_bytes) = admission::recent_demand(settings, now, &held);
        let mut paths = BTreeMap::new();
        let base = home.unwrap_or(Path::new("/"));
        paths.insert(admission::disk_key(base)?, base.to_owned());
        for r in &records {
            budget.check()?;
            if let Ok(key) = admission::disk_key(&r.home) {
                paths.entry(key).or_insert_with(|| r.home.clone());
            }
        }
        let mut keys: BTreeSet<String> = paths.keys().cloned().collect();
        keys.extend(records.iter().filter_map(|r| r.disk_key.clone()));
        let mut disks = vec![];
        for key in keys {
            budget.check()?;
            let measured = paths.get(&key).and_then(|p| admission::disk(p).ok());
            disks.push(Disk {
                filesystem: crate::core::hash(&json!(key)),
                available_bytes: measured.map(|p| p.0),
                total_bytes: measured.map(|p| p.1),
                committed_bytes: measured.map(|p| admission::committed_disk(&held, &key, p.0)),
                state: if measured.is_some() {
                    "covered"
                } else {
                    "unknown"
                }
                .into(),
            });
        }
        Some(Readings {
            cpus: host.cpus,
            load: host.load,
            memory_available_bytes: host.memory_available_bytes,
            memory_total_bytes: host.memory_total_bytes,
            released_load: released_load(&tx, now)?,
            recent_cpus,
            recent_memory_bytes,
            disks,
        })
    } else {
        None
    };
    let (route_availability, route_state) = if shared {
        let policy = crate::crew_routes::load()?;
        match policy {
            Some(p) => match crate::crew_routes::states(&p, &tx, Store::now_ms()) {
                Ok(states) => {
                    let mut counts = BTreeMap::new();
                    for s in states {
                        *counts.entry(s.state.as_str().to_owned()).or_default() += 1;
                    }
                    (Some(counts), "covered")
                }
                Err(_) => (None, "unavailable"),
            },
            None => (None, "unknown"),
        }
    } else {
        (None, "withheld")
    };
    budget.check()?;
    Ok(Capacity {
        ledger: if shared {
            crate::core::hash(&json!(path))
        } else {
            "selected_ship".into()
        },
        policy,
        charter_heavy_cap: if shared { charter_cap } else { None },
        charter_policy_state: if !shared {
            "withheld"
        } else if task.is_none() {
            "not_requested"
        } else if charter.is_some() {
            "known"
        } else {
            "unknown"
        }
        .into(),
        charter_policy_source: if shared && charter.is_some() {
            "active_task_charter"
        } else {
            "unknown"
        }
        .into(),
        charter_occupied_heavy_slots: (shared && charter.is_some()).then_some(share),
        occupied_heavy_slots: occupied,
        free_heavy_slots: if shared && (task.is_none() || charter.is_some()) {
            free
        } else {
            None
        },
        headroom_basis: "heavy_slots_only_not_agent_capacity".into(),
        reservation_count: records.len() as u64,
        memory_reservation_count: records
            .iter()
            .filter(|r| r.demand.is_some_and(|d| d.memory_bytes > 0))
            .count() as u64,
        holds,
        queue_depth: queue.len() as u64,
        capacity_blocked: !queue.is_empty(),
        queue,
        readings,
        route_availability,
        route_state: route_state.into(),
        shared_host: if shared { "visible" } else { "withheld" }.into(),
    })
}
