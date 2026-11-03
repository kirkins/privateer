//! Compose existing inspection pages; never infer execution from recorded work.
//! Scan-window counts and returned-page counts deliberately have different bases.
use super::{
    inspection,
    scoped::{Command, DelegatedContext},
};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const SHIPS: usize = 12;
const BUDGET: Duration = Duration::from_secs(3);
const METRICS: &[(&str, &str)] = &[
    ("inspected_nonterminal_jobs", "jobs"),
    ("retained_terminal_jobs", "jobs"),
    ("retained_job_states", "jobs"),
    ("scheduled_tasks", "work"),
    ("unscheduled_tasks", "work"),
    ("task_backlog", "work"),
    ("scheduled_backlog", "work"),
    ("unscheduled_backlog", "work"),
];

fn metric(value: Value, complete: bool, source: &str, basis: &str) -> Value {
    json!({"value":value,"complete":complete,"sources":if value.is_null() { vec![] } else { vec![source] },
        "count_basis":basis})
}

// Only the existing reader resolves the target. Do not send to a different
// machine when the directory has duplicate ship handles or a local shadow.
fn same_target(ship: &str, machine: &str) -> bool {
    let Ok(conn) = super::runtime::registry_readonly() else {
        return false;
    };
    let local = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM ships WHERE name=?1 AND retired_ms IS NULL)",
        [ship],
        |r| r.get::<_, bool>(0),
    );
    match local {
        Ok(true) => machine == "local",
        Ok(false) => conn
            .query_row(
                "SELECT machine FROM machine_ships WHERE name=?1 ORDER BY machine LIMIT 1",
                [ship],
                |r| r.get::<_, String>(0),
            )
            .is_ok_and(|m| m == machine),
        Err(_) => false,
    }
}

fn page(reply: Option<&Value>, reason: &str) -> Value {
    match reply {
        Some(r) if r["coverage"]["state"] == "covered" => json!({
            "state":"read","reason":null,"counts":r["counts"],
            "scan_complete":r["coverage"]["counts_complete"],
            "count_basis":r["coverage"]["count_basis"],"history":r["coverage"]["history"],
            "more":r["more"],"continuation":r["continuation"],"snapshot":r["snapshot"],
            "observed_at":r["coverage"]["observed_at"],"mode":r["coverage"]["mode"],
            "paging_tool":if r["coverage"]["facet"] == "work" { "ship_work" } else { "ship_jobs" },
            "paging_limit":50,"page_counts_additive":false}),
        _ => json!({"state":"missing","reason":reason,"counts":null,"scan_complete":false,
            "more":null,"continuation":null,"snapshot":null}),
    }
}

fn ship(row: &mut Value, jobs: Option<&Value>, work: Option<&Value>, reason: &str) {
    let mut metrics = json!({});
    for (name, facet) in METRICS {
        let source = format!(
            "{}:{}:{facet}",
            row["machine"].as_str().unwrap_or(""),
            row["name"].as_str().unwrap_or("")
        );
        let reply = if *facet == "jobs" { jobs } else { work };
        let reply = reply.filter(|r| r["coverage"]["state"] == "covered");
        let page_basis = matches!(*name, "scheduled_backlog" | "unscheduled_backlog");
        let value = reply.map_or(Value::Null, |r| {
            let count = |key: &str| r["counts"][key].as_u64().unwrap_or(0);
            match *name {
                "inspected_nonterminal_jobs" => json!(
                    crate::snapshot::inspection::STATES
                        .iter()
                        .filter(
                            |s| !["succeeded", "failed", "cancelled", "timed_out", "resumed"]
                                .contains(s)
                        )
                        .map(|s| count(s))
                        .sum::<u64>()
                ),
                "retained_terminal_jobs" => json!(count("completed")),
                "retained_job_states" => r["counts"].clone(),
                "scheduled_tasks" => json!(count("scheduled")),
                "unscheduled_tasks" => json!(count("unscheduled_total")),
                "task_backlog" => json!(count("backlog")),
                _ => json!(
                    r["work"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|w| w["backlog"] == true
                            && w["scheduled"] == (*name == "scheduled_backlog"))
                        .count()
                ),
            }
        });
        let complete = reply.is_some_and(|r| {
            r["coverage"]["counts_complete"] == true && (!page_basis || r["more"] == false)
        });
        metrics[*name] = metric(
            value,
            complete,
            &source,
            if page_basis {
                "returned_page"
            } else {
                "authorized_scan_window"
            },
        );
    }
    let census = &row["runtime"]["facts"]["census"];
    let checked = row["runtime"]["current"] == true && census.is_object();
    let checked = json!({"workers":{
        "value":if checked { json!({"active_workers":census["active_workers"],"commands":census["commands"],
            "first_mate":census["first_mate"],"crew":census["crew"],"errand":census["errand"]}) } else { Value::Null },
        "complete":checked && census["workers_complete"] == true},
        "sessions":{"value":if checked { census["sessions"].clone() } else { Value::Null },
            "complete":checked && census["sessions_complete"] == true}});
    row["rollup"] = json!({"checked":checked,"metrics":metrics,"jobs":page(jobs, reason),"work":page(work, reason),
        "paging_notice":"Scan-window counts overlap continuation windows; never sum page counts. Backlog splits count returned rows only. Prior retained history is unknown."});
}

pub(super) fn compose(view: &mut Value, c: &DelegatedContext) {
    let started = Instant::now();
    let inventory_complete = view["coverage"]["inventory_complete"] == true;
    let Some(rows) = view["ships"].as_array_mut() else {
        return;
    };
    for (index, row) in rows.iter_mut().enumerate() {
        let name = row["name"].as_str().unwrap_or("").to_owned();
        let machine = row["machine"].as_str().unwrap_or("").to_owned();
        let reason = if index >= SHIPS || started.elapsed() >= BUDGET {
            "rollup_budget"
        } else if machine != "local" && row["runtime"]["current"] != true {
            "not_current"
        } else if !same_target(&name, &machine) {
            "ambiguous_target"
        } else {
            "inspection_unavailable"
        };
        let read = |command| {
            if reason != "inspection_unavailable" || started.elapsed() >= BUDGET {
                return (None, false);
            }
            (
                inspection::request(command, &json!({"ship":name,"limit":50}), c)
                    .ok()
                    .filter(|r| {
                        r["coverage"]["machine"] == machine
                            && r["counts"].as_object().is_some_and(|counts| {
                                counts.values().all(|n| {
                                    n.as_u64().is_some_and(|n| {
                                        n <= crate::snapshot::inspection::SCAN as u64
                                    })
                                })
                            })
                    }),
                true,
            )
        };
        let (jobs, jobs_attempted) = read(Command::ShipJobs);
        let (work, work_attempted) = read(Command::ShipWork);
        ship(row, jobs.as_ref(), work.as_ref(), reason);
        for (facet, attempted) in [("jobs", jobs_attempted), ("work", work_attempted)] {
            row["rollup"][facet]["attempted"] = json!(attempted);
            row["rollup"][facet]["successful"] = json!(row["rollup"][facet]["state"] == "read");
            if !attempted && reason == "inspection_unavailable" {
                row["rollup"][facet]["reason"] = json!("rollup_budget");
            }
        }
    }
    let rows = view["ships"].as_array().unwrap();
    let mut totals = json!({});
    for (name, _) in METRICS {
        let mut sum = json!(0);
        if *name == "retained_job_states" {
            sum = json!({});
        }
        let mut sources = vec![];
        let mut complete = inventory_complete;
        for row in rows {
            let m = &row["rollup"]["metrics"][*name];
            complete &= m["complete"] == true;
            if m["value"].is_null() {
                continue;
            }
            if *name == "retained_job_states" {
                for (key, count) in m["value"].as_object().into_iter().flatten() {
                    sum[key] = json!(sum[key].as_u64().unwrap_or(0) + count.as_u64().unwrap_or(0));
                }
            } else {
                sum = json!(sum.as_u64().unwrap_or(0) + m["value"].as_u64().unwrap_or(0));
            }
            sources.extend(m["sources"].as_array().into_iter().flatten().cloned());
        }
        totals[*name] = json!({"value":if sources.is_empty() && !(rows.is_empty() && inventory_complete) { Value::Null } else { sum },
            "complete":complete,"sources":sources,
            "count_basis":if matches!(*name,"scheduled_backlog"|"unscheduled_backlog") { "returned_page" } else { "authorized_scan_window" }});
    }
    for (name, value) in totals.as_object().unwrap() {
        view["observed_totals"][name] = value.clone();
    }
    view["contact_scope"] = json!("runtime_section_only_inspections_reported_in_ship_rollup");
    view["inspection_cache_writes"] = json!("bounded_registry_continuations_only");
    view["rollup_scope"] = json!("selected_directory_retained_records_not_execution");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_counts_are_not_page_lengths_and_split_is_partial_when_paged() {
        let mut row = json!({"name":"alpha","machine":"local"});
        let jobs = json!({"coverage":{"state":"covered","counts_complete":true},
            "counts":{"running":4,"succeeded":7,"failed":2,"completed":9},"jobs":[{}],"more":true});
        let work = json!({"coverage":{"state":"covered","counts_complete":true},
            "counts":{"scheduled":40,"unscheduled_total":20,"backlog":59},"more":true,
            "work":[{"scheduled":true,"backlog":true},{"scheduled":false,"backlog":true},{"scheduled":true,"backlog":false}]});
        ship(&mut row, Some(&jobs), Some(&work), "unused");
        let m = &row["rollup"]["metrics"];
        assert_eq!(m["retained_terminal_jobs"]["value"], 9);
        assert_eq!(m["inspected_nonterminal_jobs"]["value"], 4);
        assert_eq!(m["scheduled_tasks"]["value"], 40);
        assert_eq!(m["task_backlog"]["value"], 59);
        assert_eq!(m["retained_terminal_jobs"]["complete"], true);
        assert_eq!(m["scheduled_backlog"]["value"], 1);
        assert_eq!(m["unscheduled_backlog"]["value"], 1);
        assert_eq!(m["scheduled_backlog"]["complete"], false);
        assert_eq!(row["rollup"]["jobs"]["more"], true);
        assert_eq!(row["rollup"]["jobs"]["page_counts_additive"], false);
        assert_eq!(
            m["retained_terminal_jobs"]["sources"],
            json!(["local:alpha:jobs"])
        );
        let mut truncated = jobs;
        truncated["coverage"]["counts_complete"] = json!(false);
        ship(&mut row, Some(&truncated), None, "rollup_budget");
        assert_eq!(
            row["rollup"]["metrics"]["retained_terminal_jobs"]["complete"],
            false
        );
        assert!(row["rollup"]["metrics"]["scheduled_tasks"]["value"].is_null());
    }

    #[test]
    fn fleet_omissions_do_not_become_complete_zeroes() {
        let c = DelegatedContext::resolved(
            "alpha".into(),
            "door:alpha".into(),
            vec!["fleet:read".into()],
            None,
        );
        let mut view = json!({"coverage":{"inventory_complete":true},
            "ships":(0..13).map(|n| json!({"name":format!("alpha-{n}"),"machine":"beta"})).collect::<Vec<_>>(),
            "observed_totals":{}});
        compose(&mut view, &c);
        assert_eq!(
            view["ships"][12]["rollup"]["jobs"]["reason"],
            "rollup_budget"
        );
        for (name, _) in METRICS {
            let metric = &view["observed_totals"][*name];
            assert!(metric["value"].is_null());
            assert_eq!(metric["complete"], false);
        }
        view["ships"] = json!([]);
        compose(&mut view, &c);
        assert_eq!(
            view["observed_totals"]["retained_terminal_jobs"]["value"],
            0
        );
        assert_eq!(
            view["observed_totals"]["retained_terminal_jobs"]["complete"],
            true
        );
    }

    #[test]
    fn unobserved_records_remain_unknown_and_do_not_change_working() {
        let mut row = json!({"name":"alpha","machine":"beta","working":"unknown"});
        ship(&mut row, None, None, "not_current");
        for (name, _) in METRICS {
            let m = &row["rollup"]["metrics"][*name];
            assert!(m["value"].is_null());
            assert_eq!(m["complete"], false);
            assert_eq!(m["sources"], json!([]));
        }
        assert_eq!(row["working"], "unknown");
    }
}
