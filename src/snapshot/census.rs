//! Bounded, metadata-only execution observations. No screen, transcript,
//! presentation repair, process reconciliation or presence mutation lives here.
use crate::{
    herdr::{Binding, Client, Pane, View},
    needs::ObservationBudget,
    store::Store,
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    time::{Duration, Instant},
};

const LIMIT: usize = 200;
const BUDGET: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Sessions {
    pub live: u64,
    pub idle: u64,
    pub asleep: u64,
    pub blocked: u64,
    pub viewer: u64,
    pub unknown: u64,
}
/// Anonymous metadata for a checked live endpoint, never a session.read handle.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Session {
    pub reference: String,
    pub role: String,
    pub state: String,
    pub harness: Option<String>,
    pub host_mode: String,
}

pub(crate) fn safe_harness(value: &str) -> Option<String> {
    (super::inspection::HARNESSES.contains(&value) || value == "pi-signed")
        .then(|| value.to_owned())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Census {
    pub observed_at: i64,
    pub executing: bool,
    pub activity_complete: bool,
    pub workers_complete: bool,
    pub sessions_complete: bool,
    pub active_workers: u64,
    pub first_mate: u64,
    pub crew: u64,
    pub errand: u64,
    pub commands: u64,
    pub uncertain_jobs: u64,
    pub sessions: Sessions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_details: Option<Vec<Session>>,
    pub anomalies: BTreeMap<String, u64>,
    pub reason: String,
}
impl Census {
    fn empty() -> Self {
        Self {
            observed_at: Store::now_ms(),
            executing: false,
            activity_complete: true,
            workers_complete: true,
            sessions_complete: true,
            active_workers: 0,
            first_mate: 0,
            crew: 0,
            errand: 0,
            commands: 0,
            uncertain_jobs: 0,
            sessions: Sessions::default(),
            session_details: None,
            anomalies: BTreeMap::new(),
            reason: "identity_checked_census".into(),
        }
    }
    fn gap(&mut self, reason: &str) {
        self.activity_complete = false;
        self.workers_complete = false;
        self.reason = reason.into();
    }
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.observed_at >= 0
                && [
                    self.active_workers,
                    self.first_mate,
                    self.crew,
                    self.errand,
                    self.commands,
                    self.sessions.live,
                    self.sessions.idle,
                    self.sessions.asleep,
                    self.sessions.blocked,
                    self.sessions.viewer,
                    self.sessions.unknown
                ]
                .iter()
                .all(|v| *v <= (LIMIT * 2) as u64)
                && self.active_workers == self.first_mate + self.crew + self.errand
                && self.active_workers <= (LIMIT * 2) as u64
                && self.commands <= LIMIT as u64
                && self.uncertain_jobs <= LIMIT as u64
                && self.sessions.live <= (LIMIT + 1) as u64
                && self.sessions.unknown <= (LIMIT + 2) as u64
                && self.sessions.idle
                    + self.sessions.asleep
                    + self.sessions.blocked
                    + self.sessions.viewer
                    <= self.sessions.live
                && (!self.activity_complete
                    || (self.workers_complete
                        && self.sessions_complete
                        && self.uncertain_jobs == 0))
                && (self.executing == (self.active_workers + self.commands > 0))
                && [
                    "identity_checked_census",
                    "census_truncated",
                    "turn_unbound",
                    "session_unverified",
                    "payload_unverified",
                    "duplicate_spaces",
                    "host_unavailable"
                ]
                .contains(&self.reason.as_str())
                && self.anomalies.len() <= 1
                && self
                    .anomalies
                    .iter()
                    .all(|(k, v)| k == "duplicate_spaces" && *v > 1 && *v <= 1000),
            "invalid runtime census"
        );
        if let Some(rows) = &self.session_details {
            ensure!(
                rows.len() as u64 == self.sessions.live,
                "invalid session rows"
            );
            let mut refs = HashSet::new();
            let (mut idle, mut asleep, mut blocked, mut viewer, mut working, mut unknown) =
                (0, 0, 0, 0, 0, 0);
            for row in rows {
                ensure!(
                    row.reference.len() == 64
                        && row.reference.bytes().all(|b| b.is_ascii_hexdigit())
                        && refs.insert(&row.reference)
                        && [
                            "first_mate",
                            "crew",
                            "errand",
                            "command",
                            "viewer",
                            "unknown"
                        ]
                        .contains(&row.role.as_str())
                        && row.host_mode == "hosted"
                        && row
                            .harness
                            .as_deref()
                            .is_none_or(|h| safe_harness(h).is_some())
                        && ((row.role == "viewer") == (row.state == "viewer"))
                        && (row.state != "working"
                            || ["first_mate", "crew", "errand", "command"]
                                .contains(&row.role.as_str())),
                    "invalid session metadata"
                );
                match row.state.as_str() {
                    "idle" => idle += 1,
                    "asleep" => asleep += 1,
                    "blocked" => blocked += 1,
                    "viewer" => viewer += 1,
                    "working" => working += 1,
                    "unknown" => unknown += 1,
                    _ => anyhow::bail!("invalid session state"),
                }
            }
            ensure!(
                (idle, asleep, blocked, viewer)
                    == (
                        self.sessions.idle,
                        self.sessions.asleep,
                        self.sessions.blocked,
                        self.sessions.viewer
                    )
                    && unknown <= self.sessions.unknown
                    && working <= self.active_workers + self.commands,
                "invalid session totals"
            );
        }
        Ok(())
    }
    fn session(&mut self, binding: &Binding, role: &str, state: &str, namespace: Option<&str>) {
        if let (Some(rows), Some(namespace)) = (&mut self.session_details, namespace) {
            rows.push(Session {
                reference: crate::core::hash(&serde_json::json!([
                    namespace,
                    binding.socket,
                    binding.socket_identity,
                    binding.pane.pane_id,
                    binding.pane.terminal_id,
                    binding.pane.workspace_id,
                    binding.pane.tab_id,
                    binding.pane.agent,
                    binding.pane.agent_session
                ])),
                role: role.into(),
                state: state.into(),
                harness: binding.pane.agent.as_deref().and_then(safe_harness),
                host_mode: "hosted".into(),
            });
        }
    }
    fn worker(&mut self, kind: &str) {
        match kind {
            "first_mate" => self.first_mate += 1,
            "crew" => self.crew += 1,
            "errand" => self.errand += 1,
            _ => {
                self.commands += 1;
                self.executing = true;
                return;
            }
        }
        self.active_workers += 1;
        self.executing = true;
    }
}

#[derive(Debug, PartialEq)]
struct Input {
    jobs: Vec<(String, String)>, // selected private identity fields/kind, never serialized
    lease: Option<(String, i64, Option<String>, Option<String>)>,
    panes: Vec<(String, String, bool, bool)>,
    unbound: bool,
    hosted_without_lease: bool,
    owner: Option<String>,
    session_gap: bool,
}
fn input(conn: &Connection, ship: &str) -> Result<Input> {
    let budget = ObservationBudget::new(conn)?;
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Deferred)?;
    let jobs = tx.prepare(&format!("SELECT json_object('id',j.id,'incarnation',json_extract(j.record,'$.incarnation'),'task_id',json_extract(j.record,'$.task_id'),'status',json_extract(j.record,'$.status'),'payload',json_extract(j.record,'$.payload'),'hosted',json_extract(j.record,'$.hosted')), CASE
        WHEN EXISTS(SELECT 1 FROM wakes WHERE job_id=j.id) THEN 'first_mate'
        WHEN EXISTS(SELECT 1 FROM errands WHERE id=j.id) THEN 'errand'
        WHEN json_extract(j.record,'$.action')='spawn' AND json_extract(j.record,'$.agent_actor') IS NOT NULL THEN 'crew'
        ELSE 'command' END FROM jobs j JOIN tasks t ON t.id=json_extract(j.record,'$.task_id')
        WHERE t.owner=?1 AND json_extract(j.record,'$.status') NOT IN {} ORDER BY j.id LIMIT 201", crate::jobs::TERMINAL_SQL))?
        .query_map([ship], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    budget.check()?;
    let lease = tx
        .query_row(
            "SELECT holder, expires_ms, binding, asleep_holder FROM presence WHERE ship=?1",
            [ship],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let incarnation = if super::column(&tx, "crew_panes", "incarnation")? {
        "COALESCE(c.incarnation=json_extract(j.record,'$.incarnation'),0)"
    } else {
        "0"
    };
    let panes = tx.prepare(&format!("SELECT c.job_id,c.binding,c.viewer,{incarnation} FROM crew_panes c JOIN jobs j ON j.id=c.job_id JOIN tasks t ON t.id=json_extract(j.record,'$.task_id') WHERE c.ship=?1 AND t.owner=?1 ORDER BY c.job_id LIMIT 201"))?
        .query_map([ship], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    // Custody without a job is not idle. A wake and its job are one execution.
    let unbound = tx.query_row(&format!("SELECT EXISTS(SELECT 1 FROM wakes w WHERE ship=?1 AND status IN ('offered','running') AND NOT EXISTS(SELECT 1 FROM jobs j WHERE j.id=w.job_id AND json_extract(j.record,'$.status') IN ('running','launching','parked','queued'))) OR EXISTS(SELECT 1 FROM errands e WHERE ship=?1 AND status='running' AND NOT EXISTS(SELECT 1 FROM jobs j WHERE j.id=e.id AND json_extract(j.record,'$.status') IN ('running','launching','parked','queued'))) OR EXISTS(SELECT 1 FROM jobs j WHERE (json_extract(j.record,'$.status') IS NULL OR json_extract(j.record,'$.status') NOT IN {}) AND NOT EXISTS(SELECT 1 FROM tasks t WHERE t.id=json_extract(j.record,'$.task_id'))) OR EXISTS(SELECT 1 FROM jobs j JOIN tasks t ON t.id=json_extract(j.record,'$.task_id') WHERE t.owner=?1 AND json_extract(j.record,'$.status') IS NULL)", crate::jobs::TERMINAL_SQL), [ship], |r| r.get(0))?;
    let hosted_without_lease = tx.query_row("SELECT EXISTS(SELECT 1 FROM hosted_panes WHERE ship=?1) AND NOT EXISTS(SELECT 1 FROM presence WHERE ship=?1)", [ship], |r| r.get(0))?;
    let session_gap = tx.query_row("SELECT EXISTS(SELECT 1 FROM crew_panes c WHERE c.ship=?1 AND NOT EXISTS(SELECT 1 FROM jobs j JOIN tasks t ON t.id=json_extract(j.record,'$.task_id') WHERE j.id=c.job_id AND t.owner=?1))", [ship], |r| r.get(0))?;
    let owner = tx
        .query_row(
            "SELECT json_extract(record,'$.location') FROM ships WHERE name=?1",
            [ship],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    budget.check()?;
    Ok(Input {
        jobs,
        lease,
        panes,
        unbound,
        hosted_without_lease,
        owner,
        session_gap,
    })
}

// Cache a bounded host snapshot per explicit socket for this observation only.
// Binding::matches intentionally permits older missing session IDs for operator
// navigation. Positive activity needs the stricter adapter-session identity.
fn checked<'a>(binding: &Binding, view: &'a View) -> Option<&'a Pane> {
    binding
        .pane
        .agent_session
        .as_ref()
        .filter(|s| !s.is_null())?;
    binding.pane.agent.as_ref()?;
    view.panes.iter().find(|p| {
        binding.matches(view, p)
            && p.agent == binding.pane.agent
            && p.agent_session == binding.pane.agent_session
    })
}
struct Host {
    view: View,
    spaces: Vec<(String, String)>,
}
fn observe_binding(
    raw: &str,
    hosts: &mut HashMap<PathBuf, Option<Host>>,
    started: Instant,
) -> Option<(Binding, Pane, usize)> {
    let binding: Binding = serde_json::from_str(raw).ok()?;
    if started.elapsed() >= BUDGET {
        return None;
    }
    let host = hosts
        .entry(binding.socket.clone())
        .or_insert_with(|| {
            let mut client = Client::connect(&binding.socket).ok()?;
            let view = client.snapshot().ok()?;
            let spaces = client.mate_spaces().ok()?;
            Some(Host { view, spaces })
        })
        .as_ref()?;
    let pane = checked(&binding, &host.view)?.clone();
    Some((binding, pane, host.spaces.len()))
}

pub(crate) fn read(store: &Store, ship: &str) -> Result<Census> {
    read_with(store, ship, || {})
}
fn read_with(store: &Store, ship: &str, after: impl FnOnce()) -> Result<Census> {
    let started = Instant::now();
    use std::os::unix::fs::MetadataExt;
    let identity = || -> Result<(u64, u64)> {
        let path = store.home().join("privateer.sqlite");
        crate::store::reject_symlinks(&path)?;
        let metadata = std::fs::metadata(path)?;
        Ok((metadata.dev(), metadata.ino()))
    };
    let home_identity = identity()?;
    // Existing random home origin plus database identity; never create one on a
    // read. Legacy homes without it retain summary counts but no row references.
    let origin = super::inspection::origin(store).ok();
    let namespace = origin
        .as_ref()
        .map(|origin| crate::core::hash(&serde_json::json!([origin, ship])));
    let before = input(store.connection(), ship)?;
    ensure!(
        before.owner.as_deref() == Some("here"),
        "census home owner changed"
    );
    let (mut c, lease_sessions, lease_workers) =
        observe(&before, ship, started, namespace.as_deref());
    after();
    // Heartbeats change expiry, not identity. Compare holder/binding/asleep
    // holder, then separately enforce the refreshed lease's expiry. A lapsed
    // lease cannot authorize its turn, but does not discard headless evidence.
    let mut fresh = input(store.connection(), ship)?;
    let fresh_expiry = fresh.lease.as_ref().map(|l| l.1);
    let mut comparable = before;
    if let Some(lease) = &mut comparable.lease {
        lease.1 = 0;
    }
    if let Some(lease) = &mut fresh.lease {
        lease.1 = 0;
    }
    ensure!(
        started.elapsed() < BUDGET
            && home_identity == identity()?
            && origin == super::inspection::origin(store).ok()
            && comparable == fresh,
        "census changed during observation"
    );
    if fresh_expiry.is_some_and(|expires| expires <= Store::now_ms()) {
        // Remove only the original lease contribution. Never do host/process
        // I/O again after the input recheck: that would introduce an unchecked
        // second observation (and could adopt a rebound or finished payload).
        c.first_mate -= lease_workers;
        c.active_workers -= lease_workers;
        c.executing = c.active_workers + c.commands > 0;
        c.sessions.live -= lease_sessions.live;
        c.sessions.idle -= lease_sessions.idle;
        c.sessions.asleep -= lease_sessions.asleep;
        c.sessions.blocked -= lease_sessions.blocked;
        if lease_sessions.live > 0
            && let Some(rows) = &mut c.session_details
        {
            rows.remove(0);
        }
        if lease_sessions.idle + lease_sessions.asleep + lease_sessions.blocked + lease_workers > 0
        {
            c.sessions.unknown += 1;
        }
        c.sessions_complete = false;
        c.gap("session_unverified");
    }
    c.validate()?;
    Ok(c)
}
fn observe(
    input: &Input,
    ship: &str,
    started: Instant,
    namespace: Option<&str>,
) -> (Census, Sessions, u64) {
    let mut c = Census::empty();
    c.session_details = namespace.map(|_| vec![]);
    let mut hosts = HashMap::new();
    let mut sessions = HashSet::new();
    let mut executions = HashSet::new();
    if input.jobs.len() > LIMIT || input.panes.len() > LIMIT {
        c.gap("census_truncated");
        c.sessions_complete = false;
    }
    if input.unbound {
        c.gap("turn_unbound");
    }
    if input.session_gap {
        c.gap("session_unverified");
        c.sessions_complete = false;
    }
    if input.hosted_without_lease {
        c.gap("session_unverified");
        c.sessions_complete = false;
        c.sessions.unknown += 1;
    }
    if let Some((holder, expires, raw, asleep)) = &input.lease {
        let observation = raw
            .as_deref()
            .and_then(|r| observe_binding(r, &mut hosts, started));
        if let Some((binding, pane, _)) = observation.filter(|_| *expires > Store::now_ms()) {
            sessions.insert(session_key(&binding));
            c.sessions.live += 1;
            let state = if asleep.as_ref() == Some(holder) {
                if pane.agent_status == "working" {
                    c.sessions.unknown += 1;
                    c.sessions_complete = false;
                    c.gap("session_unverified");
                    "unknown"
                } else {
                    c.sessions.asleep += 1;
                    "asleep"
                }
            } else {
                session_status(&mut c, &pane, "first_mate")
            };
            c.session(&binding, "first_mate", state, namespace);
        } else {
            c.sessions.unknown += 1;
            c.sessions_complete = false;
            c.gap("session_unverified");
        }
    }
    let lease_sessions = c.sessions.clone();
    let lease_workers = c.first_mate;
    for (record, kind) in input.jobs.iter().take(LIMIT) {
        if started.elapsed() >= BUDGET {
            c.gap("census_truncated");
            break;
        }
        let Ok(job) = serde_json::from_str::<Value>(record) else {
            c.uncertain_jobs += 1;
            c.gap("payload_unverified");
            continue;
        };
        // Parked/queued coordinators are waiting, never executing workers.
        if ["parked", "queued"].contains(&job["status"].as_str().unwrap_or("")) {
            continue;
        }
        let pane = input.panes.iter().find(|(id, _, _, _)| job["id"] == *id);
        if let Some((_, _, false, false)) = pane {
            c.uncertain_jobs += 1;
            c.gap("session_unverified");
        } else if let Some((_, raw, false, true)) = pane {
            if let Some((binding, observed, _)) = observe_binding(raw, &mut hosts, started) {
                if sessions.insert(session_key(&binding)) {
                    c.sessions.live += 1;
                    let state = session_status(&mut c, &observed, kind);
                    c.session(&binding, kind, state, namespace);
                }
            } else {
                c.sessions.unknown += 1;
                c.sessions_complete = false;
                c.uncertain_jobs += 1;
                c.gap("session_unverified");
            }
        } else if job["hosted"] == true || job["hosted"] == 1 {
            c.uncertain_jobs += 1;
            c.gap("session_unverified");
        } else if ["running", "launching"].contains(&job["status"].as_str().unwrap_or("")) {
            match crate::jobs::census_payload(&job) {
                Ok(Some(identity)) => {
                    if executions.insert(identity) {
                        c.worker(kind);
                    }
                }
                _ => {
                    c.uncertain_jobs += 1;
                    c.gap("payload_unverified");
                }
            }
        } else {
            c.uncertain_jobs += 1;
            c.gap("payload_unverified");
        }
    }
    // Viewer panes are sessions, never additional workers, even if their host
    // adapter says working. Include parked/terminal session rows conservatively.
    for (_, raw, viewer, _) in input.panes.iter().take(LIMIT) {
        if let Some((binding, _, _)) = observe_binding(raw, &mut hosts, started) {
            if sessions.insert(session_key(&binding)) {
                c.sessions.live += 1;
                if *viewer {
                    c.sessions.viewer += 1;
                    c.session(&binding, "viewer", "viewer", namespace);
                } else {
                    c.sessions.unknown += 1;
                    c.sessions_complete = false;
                    c.gap("session_unverified");
                    c.session(&binding, "unknown", "unknown", namespace);
                }
            }
        } else if *viewer {
            c.sessions.unknown += 1;
            c.sessions_complete = false;
        }
    }
    for host in hosts.values() {
        if let Some(host) = host {
            let spaces = host
                .spaces
                .iter()
                .filter(|(s, _)| s == ship)
                .map(|(_, w)| w)
                .collect::<HashSet<_>>()
                .len();
            if spaces > 1 {
                c.anomalies.insert("duplicate_spaces".into(), spaces as u64);
                c.sessions_complete = false;
                c.gap("duplicate_spaces");
            }
        } else {
            c.gap("host_unavailable");
            c.sessions_complete = false;
        }
    }
    if !c.sessions_complete {
        c.activity_complete = false;
    }
    (c, lease_sessions, lease_workers)
}
fn session_key(binding: &Binding) -> String {
    format!(
        "{:?}:{:?}:{}:{}",
        binding.socket, binding.socket_identity, binding.pane.terminal_id, binding.pane.pane_id
    )
}
#[cfg(test)]
#[path = "census_tests.rs"]
mod tests;

fn session_status(c: &mut Census, pane: &Pane, kind: &str) -> &'static str {
    match pane.agent_status.as_str() {
        "working" => {
            c.worker(kind);
            "working"
        }
        "idle" | "done" => {
            c.sessions.idle += 1;
            "idle"
        }
        "blocked" => {
            c.sessions.blocked += 1;
            "blocked"
        }
        _ => {
            c.sessions.unknown += 1;
            c.sessions_complete = false;
            c.gap("session_unverified");
            "unknown"
        }
    }
}
