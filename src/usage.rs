//! `usage.summary`: the tokens harness jobs reported, summed over a period
//! and grouped by route, ship, day or machine.
//!
//! Each job records its own usage as it settles (`jobs::harness_usage`), so a
//! summary reads only job records, through the `jobs_finished` index, never a
//! log. Every home on this machine is read; a machine this one reaches
//! answers for its own homes over the gateway, without asking its links; a
//! machine that reaches this one carries its jobs on each `fleet.collect`
//! (see `usage_carry`). A machine or home that cannot be read is listed, not
//! left out.
use crate::jobs::HARNESS_SQL;
use crate::store::{Caller, Store};
use anyhow::{Result, bail, ensure};
use rusqlite::params;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// The whole local read of one machine's homes.
const LOCAL_BUDGET: Duration = Duration::from_secs(3);
/// The usage fields every group sums, in the order they are shown.
const FIELDS: [&str; 5] = ["input", "output", "cacheRead", "cacheWrite", "totalTokens"];

#[derive(Default)]
struct Group {
    jobs: u64,
    /// None until a job in the group reported usage.
    usage: Option<[u64; 5]>,
    /// Harnesses of jobs in the group that reported nothing.
    gaps: BTreeSet<String>,
    /// Jobs whose log dropped bytes, so their usage may be short.
    partial_jobs: u64,
}

impl Group {
    fn add(&mut self, other: Group) {
        self.jobs += other.jobs;
        if let Some(theirs) = other.usage {
            let ours = self.usage.get_or_insert([0; 5]);
            for (sum, n) in ours.iter_mut().zip(theirs) {
                *sum += n;
            }
        }
        self.gaps.extend(other.gaps);
        self.partial_jobs += other.partial_jobs;
    }

    fn to_json(&self, key: &str) -> Value {
        let mut row = json!({"key": key, "jobs": self.jobs, "usage": usage_json(self.usage)});
        if !self.gaps.is_empty() {
            row["gaps"] = json!(self.gaps);
        }
        if self.partial_jobs > 0 {
            row["partial_jobs"] = json!(self.partial_jobs);
        }
        row
    }

    fn from_json(row: &Value) -> Result<(String, Group)> {
        let key = row["key"].as_str().unwrap_or_default().to_owned();
        let usage = usage_sums(&row["usage"])?;
        let gaps = row["gaps"]
            .as_array()
            .map(|gaps| {
                gaps.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Ok((
            key,
            Group {
                jobs: row["jobs"].as_u64().unwrap_or(0),
                usage,
                gaps,
                partial_jobs: row["partial_jobs"].as_u64().unwrap_or(0),
            },
        ))
    }
}

fn usage_json(usage: Option<[u64; 5]>) -> Value {
    json!(usage.map(|sums| {
        FIELDS
            .iter()
            .zip(sums)
            .map(|(f, n)| (f.to_string(), json!(n)))
            .collect::<serde_json::Map<_, _>>()
    }))
}

/// A usage object's fields, or None for null.
fn usage_sums(usage: &Value) -> Result<Option<[u64; 5]>> {
    match usage {
        Value::Null => Ok(None),
        Value::Object(fields) => {
            let mut sums = [0; 5];
            for (sum, field) in sums.iter_mut().zip(FIELDS) {
                *sum = fields.get(field).and_then(Value::as_u64).unwrap_or(0);
            }
            Ok(Some(sums))
        }
        other => bail!("usage must be an object or null, not {other}"),
    }
}

/// One settled harness job, as a summary groups it and a reaching machine
/// carries it.
#[derive(Debug, PartialEq)]
pub(crate) struct JobUsage {
    pub job_id: String,
    pub finished_ms: i64,
    pub route: Option<String>,
    pub ship: String,
    pub usage: Option<[u64; 5]>,
    pub partial: bool,
}

impl JobUsage {
    fn harness(&self) -> String {
        harness_of(self.route.as_deref())
    }

    fn key(&self, period: &Period, machine: &str) -> String {
        match period.group_by.as_str() {
            "route" => self.route.clone().unwrap_or_else(|| "unknown".into()),
            "ship" => self.ship.clone(),
            "day" => crate::routes::utc_time(self.finished_ms)[..10].to_owned(),
            _ => machine.to_owned(),
        }
    }

    fn group(&self) -> Group {
        let mut one = Group {
            jobs: 1,
            usage: self.usage,
            partial_jobs: u64::from(self.partial),
            ..Group::default()
        };
        if self.usage.is_none() {
            one.gaps.insert(self.harness());
        }
        one
    }

    pub(crate) fn usage_json(&self) -> Value {
        usage_json(self.usage)
    }

    pub(crate) fn to_carry(&self) -> Value {
        json!({"job_id": self.job_id, "finished_ms": self.finished_ms, "route": self.route,
            "ship": self.ship, "harness": self.harness(), "usage": usage_json(self.usage),
            "partial": self.partial})
    }

    pub(crate) fn from_carry(row: &Value) -> Result<JobUsage> {
        let text = |name: &str, most: usize| -> Result<Option<String>> {
            match &row[name] {
                Value::Null => Ok(None),
                Value::String(s) if !s.is_empty() && s.len() <= most => Ok(Some(s.clone())),
                other => bail!("a carried row's {name} is not a short string: {other}"),
            }
        };
        Ok(JobUsage {
            job_id: text("job_id", 120)?
                .ok_or_else(|| anyhow::anyhow!("a carried row needs its job_id"))?,
            finished_ms: row["finished_ms"]
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("a carried row needs its finished_ms"))?,
            route: text("route", 300)?,
            ship: text("ship", 120)?.unwrap_or_else(|| "unknown".into()),
            usage: usage_sums(&row["usage"])?,
            partial: row["partial"].as_bool().unwrap_or(false),
        })
    }
}

/// Fold jobs into the groups, keyed as the period groups them.
fn fold(
    groups: &mut BTreeMap<String, Group>,
    period: &Period,
    machine: &str,
    jobs: impl IntoIterator<Item = JobUsage>,
) {
    for job in jobs {
        groups
            .entry(job.key(period, machine))
            .or_default()
            .add(job.group());
    }
}

struct Period {
    since_ms: i64,
    until_ms: Option<i64>,
    group_by: String,
}

fn period(args: &Value) -> Result<Period> {
    let since_ms = args["since_ms"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("since_ms is required, in epoch milliseconds"))?;
    let until_ms = match &args["until_ms"] {
        Value::Null => None,
        value => Some(
            value
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("until_ms must be epoch milliseconds"))?,
        ),
    };
    ensure!(
        until_ms.is_none_or(|until| until >= since_ms),
        "until_ms is before since_ms"
    );
    let group_by = args["group_by"].as_str().unwrap_or("route");
    ensure!(
        matches!(group_by, "route" | "ship" | "day" | "machine"),
        "group_by must be route, ship, day or machine"
    );
    Ok(Period {
        since_ms,
        until_ms,
        group_by: group_by.to_owned(),
    })
}

/// The harness a route names, `harness/provider/model`.
fn harness_of(route: Option<&str>) -> String {
    route
        .and_then(|r| r.split('/').next())
        .filter(|h| !h.is_empty())
        .unwrap_or("unknown")
        .to_owned()
}

/// One home's harness jobs matching `bounds`, a condition on
/// `json_extract(record,'$.finished_ms')` written as the `jobs_finished`
/// index has it so SQLite reads through it. A job's ship is the one it ran
/// as; `home_ship` names the home's own for a job without.
fn home_jobs(
    store: &Store,
    home_ship: &str,
    bounds: &str,
    values: impl rusqlite::Params,
) -> Result<Vec<JobUsage>> {
    let sql = format!(
        "SELECT id, json_extract(record,'$.finished_ms'), json_extract(record,'$.model_route'),
                json_extract(record,'$.usage'), json_extract(record,'$.agent_actor'),
                COALESCE(json_extract(record,'$.usage_partial'), 0)
         FROM jobs WHERE {HARNESS_SQL} AND {bounds}"
    );
    let mut statement = store.connection().prepare(&sql)?;
    let rows = statement.query_map(values, |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, bool>(5)?,
        ))
    })?;
    let mut jobs = vec![];
    for row in rows {
        let (job_id, finished_ms, route, usage, actor, partial) = row?;
        let usage = usage
            .and_then(|u| serde_json::from_str::<Value>(&u).ok())
            .filter(Value::is_object)
            .and_then(|u| usage_sums(&u).ok().flatten());
        jobs.push(JobUsage {
            job_id,
            finished_ms,
            route,
            ship: actor.unwrap_or_else(|| home_ship.to_owned()),
            usage,
            partial,
        });
    }
    Ok(jobs)
}

/// One home's harness jobs settled in the period, grouped.
fn read_home(
    store: &Store,
    home_ship: &str,
    machine: &str,
    period: &Period,
    groups: &mut BTreeMap<String, Group>,
) -> Result<()> {
    let jobs = home_jobs(
        store,
        home_ship,
        "json_extract(record,'$.finished_ms') >= ?1 AND json_extract(record,'$.finished_ms') < ?2",
        params![period.since_ms, period.until_ms.unwrap_or(i64::MAX)],
    )?;
    fold(groups, period, machine, jobs);
    Ok(())
}

/// One home's harness jobs after `after` (finished_ms, job id), settled
/// before `before_ms`, in that order: at most `limit`.
pub(crate) fn home_jobs_after(
    store: &Store,
    home_ship: &str,
    after: (i64, &str),
    before_ms: i64,
    limit: usize,
) -> Result<Vec<JobUsage>> {
    home_jobs(
        store,
        home_ship,
        "json_extract(record,'$.finished_ms') >= ?1 AND json_extract(record,'$.finished_ms') < ?3
           AND (json_extract(record,'$.finished_ms') > ?1 OR id > ?2)
         ORDER BY json_extract(record,'$.finished_ms'), id LIMIT ?4",
        params![
            after.0,
            after.1,
            before_ms,
            i64::try_from(limit).unwrap_or(i64::MAX)
        ],
    )
}

/// The ship a home answers for: the fleet's name for it, or the ship it
/// hosts itself when the fleet does not list it.
pub(crate) fn home_ship(store: &Store, members: &[crate::fleet::Member]) -> String {
    let same = |home: &std::path::Path| home.canonicalize().unwrap_or_else(|_| home.to_owned());
    let here = same(store.home());
    if let Some(member) = members.iter().find(|m| same(&m.home) == here) {
        return member.name.clone();
    }
    store
        .connection()
        .query_row(
            "SELECT name FROM ships WHERE json_extract(record,'$.location')='here'
             ORDER BY name LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "unknown".into())
}

/// This machine's own homes only: what a gateway answers. Homes are read
/// under one time budget; one not read is named in `unknown`.
pub(crate) fn local(args: &Value) -> Result<Value> {
    let period = period(args)?;
    let machine = crate::machines::this_machine();
    let (groups, unknown) = local_groups(&period, &machine, None)?;
    Ok(answer(
        &period,
        &groups,
        json!([{"name": machine, "reached": true, "unknown": unknown}]),
    ))
}

/// Every fleet home on this machine, and the asking home when given (it may
/// not be in the registry), each read once.
fn local_groups(
    period: &Period,
    machine: &str,
    asking: Option<&Store>,
) -> Result<(BTreeMap<String, Group>, Vec<String>)> {
    let conn = crate::fleet::registry()?;
    let started = Instant::now();
    let mut groups = BTreeMap::new();
    let mut unknown = vec![];
    let mut seen = BTreeSet::new();
    let same = |home: &std::path::Path| home.canonicalize().unwrap_or_else(|_| home.to_owned());
    let members = crate::fleet::members(&conn)?;
    if let Some(store) = asking {
        seen.insert(same(store.home()));
        let ship = home_ship(store, &members);
        if read_home(store, &ship, machine, period, &mut groups).is_err() {
            unknown.push("home:here".to_owned());
        }
    }
    for member in members {
        // Two ships sharing a home share its jobs: read them once.
        if !member.home.is_dir() || !seen.insert(same(&member.home)) {
            continue;
        }
        if started.elapsed() >= LOCAL_BUDGET {
            unknown.push(format!("home:{}", member.name));
            continue;
        }
        let mut read = BTreeMap::new();
        let result = Store::open_readonly(&member.home, Caller::Operator)
            .and_then(|store| read_home(&store, &member.name, machine, period, &mut read));
        match result {
            Ok(()) => {
                for (key, group) in read {
                    groups.entry(key).or_insert_with(Group::default).add(group);
                }
            }
            Err(_) => unknown.push(format!("home:{}", member.name)),
        }
    }
    Ok((groups, unknown))
}

fn answer(period: &Period, groups: &BTreeMap<String, Group>, machines: Value) -> Value {
    json!({
        "since_ms": period.since_ms, "until_ms": period.until_ms, "group_by": period.group_by,
        "groups": groups.iter().map(|(key, group)| group.to_json(key)).collect::<Vec<_>>(),
        "machines": machines,
    })
}

/// Fold a reached machine's answer into the fleet's groups. Grouped by
/// machine, its groups take the name this machine knows it by.
fn absorb(
    groups: &mut BTreeMap<String, Group>,
    name: &str,
    period: &Period,
    answered: &Value,
) -> Result<Vec<Value>> {
    let rows = answered["groups"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("machine {name} answered without groups"))?;
    let mut read = vec![];
    for row in rows {
        let (key, group) = Group::from_json(row)?;
        let key = if period.group_by == "machine" {
            name.to_owned()
        } else {
            key
        };
        read.push((key, group));
    }
    for (key, group) in read {
        groups.entry(key).or_default().add(group);
    }
    Ok(answered["machines"]
        .as_array()
        .and_then(|m| m.first())
        .and_then(|m| m["unknown"].as_array())
        .cloned()
        .unwrap_or_default())
}

/// The fleet's usage over the period: this machine's homes, and each machine
/// this one reaches asked for its own in parallel under the gateway timeout.
pub fn summary(store: &Store, args: &Value) -> Result<Value> {
    let period = period(args)?;
    let here = crate::machines::this_machine();
    let (mut groups, unknown) = local_groups(&period, &here, Some(store))?;
    let mut machines = vec![json!({"name": here, "reached": true, "unknown": unknown})];
    let conn = crate::fleet::registry()?;
    let linked = crate::machines::linked_for_snapshot(&conn)?;
    let asked = json!({
        "since_ms": period.since_ms, "until_ms": period.until_ms, "group_by": period.group_by,
    });
    let reached: Vec<(String, Result<Value>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = linked
            .iter()
            .filter(|(_, role, _)| role == "collects")
            .map(|(name, _, _)| {
                let asked = asked.clone();
                (
                    name.clone(),
                    scope.spawn(move || crate::machines::usage_of(name, asked)),
                )
            })
            .collect();
        handles
            .into_iter()
            .map(|(name, h)| {
                let result = h
                    .join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("panicked")));
                (name, result)
            })
            .collect()
    });
    for (name, result) in reached {
        let row = match result.and_then(|answered| absorb(&mut groups, &name, &period, &answered)) {
            Ok(unknown) => json!({"name": name, "reached": true, "unknown": unknown}),
            Err(error) => json!({
                "name": name, "reached": false,
                "error": format!("{error:#}").chars().take(500).collect::<String>(),
            }),
        };
        machines.push(row);
    }
    // A machine that reaches this one cannot be asked: what it carried on
    // its `fleet.collect` counts instead, dated by its own read.
    let now = Store::now_ms();
    for (name, role, _) in &linked {
        if role != "collected" {
            continue;
        }
        let row = match crate::usage_carry::carried(&conn, name, period.since_ms, period.until_ms) {
            Ok((jobs, contact)) => {
                fold(&mut groups, &period, name, jobs);
                let fresh = contact.as_ref().is_some_and(|c| c.fresh(now));
                let mut row = json!({"name": name, "reached": fresh, "via": "collect",
                    "as_of_ms": contact.as_ref().and_then(|c| c.as_of_ms)});
                if !fresh {
                    row["error"] = json!(match contact {
                        Some(_) => format!("nothing carried from {name} lately"),
                        None => format!("links one way; nothing carried from {name} yet"),
                    });
                }
                row
            }
            Err(error) => json!({
                "name": name, "reached": false, "via": "collect", "as_of_ms": null,
                "error": format!("{error:#}").chars().take(500).collect::<String>(),
            }),
        };
        machines.push(row);
    }
    machines.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(answer(&period, &groups, json!(machines)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_period_is_read_through_the_finished_index() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let plan: Vec<String> = store
            .connection()
            .prepare(&format!(
                "EXPLAIN QUERY PLAN SELECT record FROM jobs WHERE {HARNESS_SQL}
                   AND json_extract(record,'$.finished_ms') >= 1
                   AND json_extract(record,'$.finished_ms') < 2"
            ))
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(plan.iter().any(|p| p.contains("jobs_finished")), "{plan:?}");
    }

    #[test]
    fn a_carry_reads_after_its_cursor_through_the_finished_index_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let plan: Vec<String> = store
            .connection()
            .prepare(&format!(
                "EXPLAIN QUERY PLAN SELECT id FROM jobs WHERE {HARNESS_SQL}
                   AND json_extract(record,'$.finished_ms') >= 1
                   AND json_extract(record,'$.finished_ms') < 9
                   AND (json_extract(record,'$.finished_ms') > 1 OR id > 'a')
                 ORDER BY json_extract(record,'$.finished_ms'), id LIMIT 5"
            ))
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(plan.iter().any(|p| p.contains("jobs_finished")), "{plan:?}");
        for (id, at) in [("b", 1), ("a", 1), ("c", 2), ("d", 3), ("e", 9)] {
            let record = json!({"id": id, "action": "spawn", "status": "succeeded",
                "finished_ms": at, "agent_actor": null, "usage_partial": id == "c"});
            store
                .connection()
                .execute(
                    "INSERT INTO jobs(id, record) VALUES(?1, ?2)",
                    params![id, record.to_string()],
                )
                .unwrap();
        }
        let after = |at, id, limit| -> Vec<String> {
            home_jobs_after(&store, "home", (at, id), 9, limit)
                .unwrap()
                .into_iter()
                .map(|j| j.job_id)
                .collect()
        };
        assert_eq!(after(1, "a", 5), ["b", "c", "d"]);
        assert_eq!(after(0, "", 2), ["a", "b"]);
        let jobs = home_jobs_after(&store, "home", (2, "c"), 9, 5).unwrap();
        assert_eq!((jobs[0].ship.as_str(), jobs[0].partial), ("home", false));
        assert!(home_jobs_after(&store, "home", (1, "b"), 9, 1).unwrap()[0].partial);
    }

    #[test]
    fn a_carried_row_reads_back_as_it_was_carried() {
        let job = JobUsage {
            job_id: "j".into(),
            finished_ms: 5,
            route: Some("pi/zai/glm".into()),
            ship: "liaison".into(),
            usage: None,
            partial: true,
        };
        let row = job.to_carry();
        assert_eq!(row["harness"], "pi");
        assert_eq!(JobUsage::from_carry(&row).unwrap(), job);
        assert!(JobUsage::from_carry(&json!({"job_id": "j"})).is_err());
        assert!(
            JobUsage::from_carry(&json!({"job_id": "j", "finished_ms": 1, "usage": 3})).is_err()
        );
    }

    #[test]
    fn groups_sum_reported_usage_and_name_the_harnesses_that_reported_nothing() {
        let mut group = Group::default();
        let (_, reported) = Group::from_json(&json!({"key": "k", "jobs": 2,
            "usage": {"input": 1, "output": 2, "cacheRead": 3, "cacheWrite": 4, "totalTokens": 10}}))
        .unwrap();
        group.add(reported);
        let (_, silent) =
            Group::from_json(&json!({"key": "k", "jobs": 1, "usage": null, "gaps": ["pi"]}))
                .unwrap();
        group.add(silent);
        let (_, cut) =
            Group::from_json(&json!({"key": "k", "jobs": 1, "usage": null, "partial_jobs": 1}))
                .unwrap();
        group.add(cut);
        let row = group.to_json("k");
        assert_eq!(row["jobs"], 4);
        assert_eq!(row["partial_jobs"], 1);
        assert_eq!(row["usage"]["totalTokens"], 10);
        assert_eq!(row["gaps"], json!(["pi"]));
        let none = Group {
            jobs: 1,
            ..Group::default()
        };
        assert_eq!(none.to_json("k")["usage"], Value::Null);
    }

    #[test]
    fn a_period_needs_a_start_before_its_end_and_a_known_grouping() {
        assert!(period(&json!({})).is_err());
        assert!(period(&json!({"since_ms": 5, "until_ms": 4})).is_err());
        assert!(period(&json!({"since_ms": 5, "group_by": "week"})).is_err());
        assert_eq!(period(&json!({"since_ms": 5})).unwrap().group_by, "route");
    }
}
