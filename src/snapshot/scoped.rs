//! Closed runtime metadata, shared by answering-core reads and host filtering
//! of both delegated replies and internal operator cache sections. No authored
//! text, wait references, route/account data or operator fragments cross this DTO.
use super::*;
use crate::door::scoped::{DelegatedContext, Denial};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeJob {
    pub id: String,
    pub task_id: String,
    pub kind: String,
    pub state: String,
    pub wait: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeShip {
    pub name: String,
    pub presence: String,
    /// Recorded first-mate configuration, not evidence of a live endpoint.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::census::deserialize_harness"
    )]
    pub harness: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_host_mode"
    )]
    pub host_mode: Option<String>,
    pub open_tasks: Option<u64>,
    pub jobs_total: Option<u64>,
    pub jobs: Vec<RuntimeJob>,
    pub job_states: BTreeMap<String, u64>,
    pub jobs_truncated: bool,
    pub recorded_running_jobs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub census: Option<super::census::Census>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeSection {
    pub schema: String,
    pub observed_at: i64,
    pub ships: Vec<RuntimeShip>,
    pub truncated: bool,
}

fn deserialize_host_mode<'de, D>(d: D) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(d)?
        .filter(|mode| ["hosted", "headless"].contains(&mode.as_str())))
}

fn handle(value: &str) -> bool {
    value.len() <= 128 && crate::store::identifier(value).is_ok()
}
fn state(value: &str) -> bool {
    [
        "starting",
        "queued",
        "launching",
        "running",
        "parked",
        "orphaned",
    ]
    .contains(&value)
}
fn kind(value: &str) -> bool {
    ["first_mate", "crew", "errand", "command"].contains(&value)
}

/// Reconstruct, never forward arbitrary cached/operator JSON. Older operator
/// sections lacking owner attribution cannot establish job counts or expose IDs.
pub(crate) fn from_operator(raw: &Value) -> Option<RuntimeShip> {
    let name = raw["name"].as_str()?;
    if !handle(name) {
        return None;
    }
    let presence = raw["presence"]
        .as_str()
        .filter(|s| {
            [
                "hosted",
                "asleep",
                "session",
                "absent",
                "unavailable",
                "unknown",
            ]
            .contains(s)
        })
        .unwrap_or("unknown");
    let attributed = raw["jobs_owner"] == name;
    let total = raw["owned_jobs_total"]
        .as_u64()
        .or_else(|| attributed.then(|| raw["jobs_total"].as_u64()).flatten());
    let mut jobs = vec![];
    for job in raw["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .take(JOBS_SHOWN)
    {
        if !attributed && job["owner"] != name {
            continue;
        }
        let (Some(id), Some(task), Some(k), Some(s)) = (
            job["id"].as_str(),
            job["task_id"].as_str(),
            job["kind"].as_str(),
            job["state"].as_str(),
        ) else {
            continue;
        };
        if !handle(id) || !handle(task) || !state(s) || !kind(k) {
            continue;
        }
        let wait = job["wait"]["for"]
            .as_str()
            .filter(|v| ["job", "capacity", "disk"].contains(v))
            .map(str::to_owned);
        jobs.push(RuntimeJob {
            id: id.into(),
            task_id: task.into(),
            kind: k.into(),
            state: s.into(),
            wait,
        });
    }
    let mut job_states = BTreeMap::new();
    for job in &jobs {
        *job_states.entry(job.state.clone()).or_insert(0) += 1;
    }
    let truncated = total != Some(jobs.len() as u64);
    let running = (!truncated).then(|| {
        jobs.iter()
            .filter(|j| matches!(j.state.as_str(), "running" | "launching"))
            .count() as u64
    });
    Some(RuntimeShip {
        name: name.into(),
        presence: presence.into(),
        harness: raw["harness"]
            .as_str()
            .and_then(super::census::safe_harness),
        host_mode: raw["hosted"]
            .as_bool()
            .map(|hosted| if hosted { "hosted" } else { "headless" }.into()),
        open_tasks: raw["open_tasks_total"].as_u64(),
        jobs_total: total,
        jobs,
        job_states,
        jobs_truncated: truncated,
        recorded_running_jobs: running,
        census: raw
            .get("census")
            .and_then(|v| serde_json::from_value::<super::census::Census>(v.clone()).ok())
            .filter(|c| c.validate().is_ok()),
    })
}

impl RuntimeSection {
    /// Receiving core scope check is independent of door admission. Reads only
    /// allowed local homes, never their linked directory or operator snapshot.
    pub(crate) fn read(
        conn: &Connection,
        c: &DelegatedContext,
        selected: Option<&str>,
    ) -> Result<Self> {
        Self::read_mode(conn, c, selected, Some(2))
    }

    pub(crate) fn read_mode(
        conn: &Connection,
        c: &DelegatedContext,
        selected: Option<&str>,
        census_version: Option<u64>,
    ) -> Result<Self> {
        ensure!(c.has("fleet:read"), Denial::Scope);
        ensure!(selected.is_none_or(|s| c.allows(s)), Denial::Ship);
        let started = Instant::now();
        let now = Store::now_ms();
        let mut stmt = conn.prepare(
            "SELECT name,home FROM ships WHERE retired_ms IS NULL ORDER BY name LIMIT 2001",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut ships = vec![];
        let mut truncated = false;
        for (n, row) in rows.enumerate() {
            if n == 2000 {
                truncated = true;
                break;
            }
            let (name, home) = row?;
            if !c.allows(&name) || selected.is_some_and(|s| s != name) {
                continue;
            }
            if ships.len() == SHIP_LIMIT {
                truncated = true;
                break;
            }
            let read = || -> Result<Value> {
                ensure!(started.elapsed() < LOCAL_BUDGET, "read budget");
                let store = Store::open_readonly(std::path::Path::new(&home), Caller::Operator)?;
                let owned: bool = store.connection().query_row("SELECT EXISTS(SELECT 1 FROM ships WHERE name=?1 AND json_extract(record,'$.location')='here')", [&name], |r| r.get(0))?;
                ensure!(owned, "home owner changed");
                let mut raw =
                    read_ship_mode(&store, &name, None, now, true, Some(started + LOCAL_BUDGET))?;
                if census_version.is_some()
                    && started.elapsed() < LOCAL_BUDGET
                    && let Ok(mut census) =
                        super::census::read_mode(&store, &name, census_version == Some(2))
                {
                    // A complete negative activity claim also needs dispatcher
                    // and ledger observations; positive execution stands alone.
                    census.activity_complete &= crate::door::runtime::service(store.home()).ok()
                        == Some(true)
                        && crate::door::runtime::ledger_clear(store.home(), &name).ok()
                            == Some(true);
                    raw["census"] = serde_json::to_value(census)?;
                }
                Ok(raw)
            };
            let raw = read().unwrap_or_else(|_| unavailable(&name));
            if let Some(row) = from_operator(&raw) {
                ships.push(row);
            }
        }
        Self {
            schema: "runtime.v1".into(),
            observed_at: now,
            ships,
            truncated,
        }
        .bounded(census_version)
    }

    /// Shape the negotiated DTO before enforcing its bound. Optional detail may
    /// be omitted to preserve summary facts; absence never means zero sessions.
    fn bounded(mut self, census_version: Option<u64>) -> Result<Self> {
        if census_version != Some(2) {
            for ship in &mut self.ships {
                ship.harness = None;
                ship.host_mode = None;
                if census_version.is_none() {
                    ship.census = None;
                } else if let Some(census) = &mut ship.census {
                    census.session_details = None;
                }
            }
        }
        if serde_json::to_vec(&self)?.len() > SECTION_BYTES {
            for ship in &mut self.ships {
                if let Some(census) = &mut ship.census {
                    census.session_details = None;
                }
            }
        }
        ensure!(
            serde_json::to_vec(&self)?.len() <= SECTION_BYTES,
            "runtime section bound"
        );
        Ok(self)
    }

    /// Treat wire data as untrusted: closed deserialization plus derived counts
    /// and allowlists, followed by current host grant/directory selection.
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == "runtime.v1"
                && self.observed_at >= 0
                && self.ships.len() <= SHIP_LIMIT
                && serde_json::to_vec(self)?.len() <= SECTION_BYTES,
            "invalid runtime section"
        );
        let mut names = HashSet::new();
        for s in &self.ships {
            ensure!(
                handle(&s.name) && names.insert(&s.name),
                "invalid runtime ship"
            );
            ensure!(
                [
                    "hosted",
                    "asleep",
                    "session",
                    "absent",
                    "unavailable",
                    "unknown"
                ]
                .contains(&s.presence.as_str()),
                "invalid presence"
            );
            ensure!(
                s.harness
                    .as_deref()
                    .is_none_or(|h| super::census::safe_harness(h).is_some())
                    && s.host_mode
                        .as_deref()
                        .is_none_or(|m| ["hosted", "headless"].contains(&m)),
                "invalid runtime configuration"
            );
            if let Some(census) = &s.census {
                census.validate()?;
            }
            ensure!(s.jobs.len() <= JOBS_SHOWN, "runtime job bound");
            let mut states = BTreeMap::new();
            let mut ids = HashSet::new();
            for j in &s.jobs {
                ensure!(
                    handle(&j.id)
                        && handle(&j.task_id)
                        && ids.insert(&j.id)
                        && state(&j.state)
                        && kind(&j.kind),
                    "invalid runtime job"
                );
                ensure!(
                    j.wait
                        .as_deref()
                        .is_none_or(|w| ["job", "capacity", "disk"].contains(&w)),
                    "invalid wait"
                );
                *states.entry(j.state.clone()).or_insert(0) += 1;
            }
            ensure!(
                s.jobs_total
                    .is_none_or(|total| total >= s.jobs.len() as u64),
                "invalid runtime total"
            );
            let truncated = s.jobs_total != Some(s.jobs.len() as u64);
            let running = (!truncated).then(|| {
                s.jobs
                    .iter()
                    .filter(|j| matches!(j.state.as_str(), "running" | "launching"))
                    .count() as u64
            });
            ensure!(
                s.job_states == states
                    && s.jobs_truncated == truncated
                    && s.recorded_running_jobs == running,
                "invalid runtime totals"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn operator_projection_filters_owners_before_counts_and_ignores_global_totals() {
        let raw = json!({"name":"survey","presence":"absent","jobs_total":99,"owned_jobs_total":1,
        "jobs":[
            {"id":"relay-job","task_id":"relay-task","owner":"relay","kind":"crew","state":"running"},
            {"id":"survey-job","task_id":"survey-task","owner":"survey","kind":"command","state":"queued"}
        ]});
        let projected = from_operator(&raw).unwrap();
        assert_eq!(projected.jobs_total, Some(1));
        assert_eq!(projected.jobs.len(), 1);
        assert_eq!(projected.jobs[0].id, "survey-job");
        assert_eq!(projected.recorded_running_jobs, Some(0));
        assert!(!projected.jobs_truncated);
        assert!(!serde_json::to_string(&projected).unwrap().contains("relay"));
        let mut capped = raw;
        capped["jobs"] = json!([]);
        let capped = from_operator(&capped).unwrap();
        assert!(capped.jobs_truncated);
        assert_eq!(capped.recorded_running_jobs, None);
    }

    #[test]
    fn runtime_configuration_is_closed_allowlisted_and_not_liveness() {
        let raw = json!({"name":"alpha","presence":"absent","harness":"pi-signed","hosted":true,
            "options":{"model":"secret-canary"},"argv":["argv-canary"]});
        let ship = from_operator(&raw).unwrap();
        assert_eq!(ship.harness.as_deref(), Some("pi-signed"));
        assert_eq!(ship.host_mode.as_deref(), Some("hosted"));
        assert!(ship.census.is_none());
        let section = RuntimeSection {
            schema: "runtime.v1".into(),
            observed_at: 0,
            ships: vec![ship],
            truncated: false,
        };
        section.validate().unwrap();
        for (key, value) in [
            ("harness", json!("secret-canary")),
            ("host_mode", json!("socket-canary")),
        ] {
            let mut wire = serde_json::to_value(&section).unwrap();
            wire["ships"][0][key] = value;
            let received: RuntimeSection = serde_json::from_value(wire).unwrap();
            received.validate().unwrap();
            assert!(received.ships[0].harness.is_none() || key != "harness");
            assert!(received.ships[0].host_mode.is_none() || key != "host_mode");
            assert!(!serde_json::to_string(&received).unwrap().contains("canary"));
        }
        for (key, value) in [
            ("harness", json!(42)),
            ("host_mode", json!({})),
            ("argv", json!([])),
        ] {
            let mut bad = serde_json::to_value(&section).unwrap();
            bad["ships"][0][key] = value;
            assert!(serde_json::from_value::<RuntimeSection>(bad).is_err());
        }
        let mut custom = raw;
        custom["harness"] = json!("/private/harness-canary");
        custom["hosted"] = Value::Null;
        let custom = from_operator(&custom).unwrap();
        assert!(custom.harness.is_none() && custom.host_mode.is_none());
        let text = serde_json::to_string(&custom).unwrap();
        for canary in ["secret-canary", "argv-canary", "harness-canary"] {
            assert!(!text.contains(canary));
        }
        let old = json!({"schema":"runtime.v1","observed_at":0,"ships":[{"name":"alpha","presence":"unknown",
            "open_tasks":null,"jobs_total":null,"jobs":[],"job_states":{},"jobs_truncated":true,"recorded_running_jobs":null}],"truncated":false});
        let old: RuntimeSection = serde_json::from_value(old).unwrap();
        old.validate().unwrap();
        assert!(old.ships[0].harness.is_none() && old.ships[0].host_mode.is_none());
    }

    #[test]
    fn negotiated_section_bound_preserves_summaries_when_details_do_not_fit() {
        let rows: Vec<_> = (0..201)
            .map(|n| {
                json!({"reference":format!("{n:064x}"),"role":"first_mate",
                "state":"idle","harness":"pi","host_mode":"hosted"})
            })
            .collect();
        let census = json!({"observed_at":0,"executing":false,"activity_complete":true,
            "workers_complete":true,"sessions_complete":true,"active_workers":0,
            "first_mate":0,"crew":0,"errand":0,"commands":0,"uncertain_jobs":0,
            "sessions":{"live":201,"idle":201,"asleep":0,"blocked":0,"viewer":0,"unknown":0},
            "session_details":rows,"anomalies":{},"reason":"identity_checked_census"});
        let ships = (0..SHIP_LIMIT)
            .map(|n| {
                from_operator(&json!({"name":format!("alpha-{n}"),"presence":"hosted",
                "harness":"pi","hosted":true,"census":census}))
                .unwrap()
            })
            .collect();
        let large = RuntimeSection {
            schema: "runtime.v1".into(),
            observed_at: 0,
            ships,
            truncated: false,
        };
        assert!(serde_json::to_vec(&large).unwrap().len() > SECTION_BYTES);
        for version in [None, Some(1), Some(2)] {
            let bounded = large.clone().bounded(version).unwrap();
            bounded.validate().unwrap();
            assert_eq!(bounded.ships.len(), SHIP_LIMIT);
            assert!(!bounded.truncated, "summary rows were not truncated");
            for ship in &bounded.ships {
                if version.is_none() {
                    assert!(ship.census.is_none());
                } else {
                    let census = ship.census.as_ref().unwrap();
                    assert_eq!(
                        census.sessions.live, 201,
                        "omitted detail is not zero sessions"
                    );
                    assert_eq!(census.sessions.idle, 201);
                    assert!(census.sessions_complete && census.activity_complete);
                    assert!(census.session_details.is_none());
                }
                assert_eq!(ship.harness.is_some(), version == Some(2));
                assert_eq!(ship.host_mode.is_some(), version == Some(2));
            }
        }
        // The same negotiated path retains details when they do fit.
        let mut small = large;
        small.ships.truncate(1);
        let small = small.bounded(Some(2)).unwrap();
        assert_eq!(
            small.ships[0]
                .census
                .as_ref()
                .unwrap()
                .session_details
                .as_ref()
                .unwrap()
                .len(),
            201
        );
    }

    #[test]
    fn operator_projection_drops_text_hidden_waits_and_unattributed_counts() {
        let raw = json!({"name":"survey","presence":"hosted","jobs_owner":"survey","jobs_total":21,"jobs":[{"id":"survey-job","task_id":"survey-task","kind":"crew","state":"queued","command":["secret"],"doing":{"text":"secret"},"wait":{"for":"capacity","behind":[{"ship":"lookout","job_id":"hidden"}]}}],"routes":["secret"]});
        let projected = from_operator(&raw).unwrap();
        assert!(projected.jobs_truncated);
        assert_eq!(projected.recorded_running_jobs, None);
        let text = serde_json::to_string(&projected).unwrap();
        for hidden in ["secret", "lookout", "hidden", "behind"] {
            assert!(!text.contains(hidden));
        }
        let mut old = raw;
        old.as_object_mut().unwrap().remove("jobs_owner");
        let old = from_operator(&old).unwrap();
        assert!(old.jobs.is_empty());
        assert_eq!(old.jobs_total, None);
    }
}
