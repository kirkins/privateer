//! `usage.summary`: the tokens harness jobs reported, summed over a period
//! and grouped by route, ship, day or machine.
//!
//! Each job records its own usage as it settles (`jobs::harness_usage`), so a
//! summary reads only job records, through the `jobs_finished` index, never a
//! log. Every home on this machine is read; a machine this one reaches
//! answers for its own homes over the gateway, without asking its links. A
//! machine or home that cannot be read is listed, not left out.
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
    }

    fn to_json(&self, key: &str) -> Value {
        let mut row = json!({"key": key, "jobs": self.jobs, "usage": self.usage.map(|sums| {
            FIELDS.iter().zip(sums).map(|(f, n)| (f.to_string(), json!(n))).collect::<serde_json::Map<_, _>>()
        })});
        if !self.gaps.is_empty() {
            row["gaps"] = json!(self.gaps);
        }
        row
    }

    fn from_json(row: &Value) -> Result<(String, Group)> {
        let key = row["key"].as_str().unwrap_or_default().to_owned();
        let usage = match &row["usage"] {
            Value::Null => None,
            Value::Object(fields) => {
                let mut sums = [0; 5];
                for (sum, field) in sums.iter_mut().zip(FIELDS) {
                    *sum = fields.get(field).and_then(Value::as_u64).unwrap_or(0);
                }
                Some(sums)
            }
            other => bail!("a group's usage must be an object or null, not {other}"),
        };
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
            },
        ))
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

/// One home's harness jobs settled in the period, grouped.
fn read_home(
    store: &Store,
    ship: &str,
    machine: &str,
    period: &Period,
    groups: &mut BTreeMap<String, Group>,
) -> Result<()> {
    // The bounds and the index condition are written as `jobs_finished` has
    // them, so SQLite reads the period through it.
    let sql = format!(
        "SELECT json_extract(record,'$.finished_ms'), json_extract(record,'$.model_route'),
                json_extract(record,'$.usage')
         FROM jobs WHERE {HARNESS_SQL}
           AND json_extract(record,'$.finished_ms') >= ?1
           AND json_extract(record,'$.finished_ms') < ?2"
    );
    let mut statement = store.connection().prepare(&sql)?;
    let rows = statement.query_map(
        params![period.since_ms, period.until_ms.unwrap_or(i64::MAX)],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        },
    )?;
    for row in rows {
        let (finished_ms, route, usage) = row?;
        let key = match period.group_by.as_str() {
            "route" => route.clone().unwrap_or_else(|| "unknown".into()),
            "ship" => ship.to_owned(),
            "day" => crate::routes::utc_time(finished_ms)[..10].to_owned(),
            _ => machine.to_owned(),
        };
        let usage = usage
            .and_then(|u| serde_json::from_str::<Value>(&u).ok())
            .filter(Value::is_object);
        let mut one = Group {
            jobs: 1,
            ..Group::default()
        };
        match usage {
            Some(usage) => {
                let mut sums = [0; 5];
                for (sum, field) in sums.iter_mut().zip(FIELDS) {
                    *sum = usage[field].as_u64().unwrap_or(0);
                }
                one.usage = Some(sums);
            }
            None => {
                one.gaps.insert(harness_of(route.as_deref()));
            }
        }
        groups.entry(key).or_default().add(one);
    }
    Ok(())
}

/// This machine's own homes only: what a gateway answers. Homes are read
/// under one time budget; one not read is named in `unknown`.
pub(crate) fn local(args: &Value) -> Result<Value> {
    let period = period(args)?;
    let machine = crate::machines::this_machine();
    let groups = local_groups(&period, &machine)?;
    Ok(answer(
        &period,
        &groups.0,
        json!([{
            "name": machine, "reached": true, "unknown": groups.1,
        }]),
    ))
}

fn local_groups(period: &Period, machine: &str) -> Result<(BTreeMap<String, Group>, Vec<String>)> {
    let conn = crate::fleet::registry()?;
    let started = Instant::now();
    let mut groups = BTreeMap::new();
    let mut unknown = vec![];
    let mut seen = BTreeSet::new();
    for member in crate::fleet::members(&conn)? {
        // Two ships sharing a home share its jobs: read them once.
        if !member.home.is_dir() || !seen.insert(member.home.clone()) {
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
pub fn summary(args: &Value) -> Result<Value> {
    let period = period(args)?;
    let here = crate::machines::this_machine();
    let (mut groups, unknown) = local_groups(&period, &here)?;
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
    for (name, role, _) in &linked {
        if role == "collected" {
            machines.push(json!({
                "name": name, "reached": false,
                "error": format!("links one way; {name} reaches this machine, which cannot ask it"),
            }));
        }
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
        let row = group.to_json("k");
        assert_eq!(row["jobs"], 3);
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
