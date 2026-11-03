//! Additive, directory-relative summaries, never a claim of fleet coverage.
//! Read only the closed projection: no probes, provider routing or new rollups.
use serde_json::{Value, json};
use std::collections::BTreeSet;

fn provenance(row: &Value) -> Value {
    let runtime = &row["runtime"];
    // Directory/home-fact timestamps are not evidence the census was read.
    let mode = runtime["mode"].as_str().unwrap_or("directory");
    json!({"machine":row["machine"],"ship":row["name"],
        "kind":match mode {
            "local_live" | "remote_live" => "fresh",
            "cache" | "carried" | "reused" => "cached",
            _ => "directory_only",
        },
        "age_ms":if mode == "local_live" { json!(0) } else { runtime["age_ms"].clone() }})
}

fn layer(value: Value, complete: bool, reason: &str, provenance: Value) -> Value {
    json!({"value":value,"complete":complete,
        "reason":if complete { None } else { Some(reason) },"provenance":provenance})
}

pub(super) fn summarize(view: &mut Value) {
    let rows = view["ships"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let inventory_complete = view["coverage"]["inventory_complete"] == true;
    let provenance = json!(rows.iter().map(provenance).collect::<Vec<_>>());
    let directory = json!(
        rows.iter()
            .map(|r| json!({"machine":r["machine"],
        "ship":r["name"],"kind":"directory_only","age_ms":null}))
            .collect::<Vec<_>>()
    );
    let mut machines = BTreeSet::new();
    let mut fresh_machines = BTreeSet::new();
    // Contact evidence is machine-level; a peer can answer without reading a
    // selected home. Use sources as well as rows for that distinction.
    for row in rows {
        if let Some(machine) = row["machine"].as_str() {
            machines.insert(machine);
            if machine == "local" {
                fresh_machines.insert(machine);
            }
        }
    }
    for source in view["coverage"]["sources"].as_array().into_iter().flatten() {
        if source["mode"] == "remote_live"
            && source["state"] == "covered"
            && let Some(machine) = source["machine"].as_str()
        {
            fresh_machines.insert(machine);
        }
    }
    let mut contact_provenance = vec![];
    for machine in &machines {
        let source = view["coverage"]["sources"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s["machine"] == *machine && s["facet"] == "inventory");
        let mode = source
            .and_then(|s| s["mode"].as_str())
            .unwrap_or("directory");
        contact_provenance.push(json!({"machine":machine,"kind":
            if fresh_machines.contains(machine) { "fresh" }
            else if matches!(mode,"reused"|"cache"|"carried") { "cached" }
            else { "directory_only" },
            "age_ms":if *machine == "local" { json!(0) }
                else { source.map_or(Value::Null, |s| s["age_ms"].clone()) }}));
    }
    let mut workers = 0u64;
    let mut commands = 0u64;
    let mut sessions = 0u64;
    let mut unknown_sessions = 0u64;
    let mut uncertain_jobs = 0u64;
    let mut unknown_ships = 0u64;
    let mut census_rows = 0usize;
    let mut sessions_complete = inventory_complete;
    let mut workers_complete = inventory_complete;
    let mut backlog = 0u64;
    let mut backlog_rows = 0usize;
    let mut backlog_complete = inventory_complete;
    for row in rows {
        let runtime = &row["runtime"];
        let census = &runtime["facts"]["census"];
        let current = runtime["current"] == true;
        let checked = current && census.is_object();
        sessions_complete &= checked && census["sessions_complete"] == true;
        workers_complete &= checked && census["workers_complete"] == true;
        if checked {
            census_rows += 1;
            workers += census["active_workers"].as_u64().unwrap_or(0);
            commands += census["commands"].as_u64().unwrap_or(0);
            sessions += census["sessions"]["live"].as_u64().unwrap_or(0);
            unknown_sessions += census["sessions"]["unknown"].as_u64().unwrap_or(0);
            uncertain_jobs += census["uncertain_jobs"].as_u64().unwrap_or(0);
        }
        // A presence lease is deliberately not consulted here.
        if !checked
            || row["working"] == "unknown"
            || census["sessions_complete"] != true
            || census["workers_complete"] != true
        {
            unknown_ships += 1;
        }
        // Local directory reads can supply recorded backlog even when census
        // failed. Cached remote records must not turn into current backlog.
        let tasks = row["open_tasks"]
            .as_u64()
            .filter(|_| row["machine"] == "local" || current);
        backlog_complete &= tasks.is_some();
        if let Some(tasks) = tasks {
            backlog += tasks;
            backlog_rows += 1;
        }
    }
    let gap = if inventory_complete {
        "observation_incomplete"
    } else {
        "inventory_incomplete"
    };
    let observed = |count: usize| count > 0 || (rows.is_empty() && inventory_complete);
    let totals = &view["observed_totals"];
    let jobs = json!({"recorded_running":totals["recorded_running_jobs"],
        "nonterminal":totals["nonterminal_jobs"],"retained":null});
    let mut layers = json!({"scope":"selected_directory",
        "inventory":layer(json!(rows.len()), inventory_complete, "inventory_incomplete", directory),
        "reachability":layer(json!({"fresh_machines":fresh_machines.len(),
            "known_machines":machines.len()}), inventory_complete && machines == fresh_machines,
            if inventory_complete { "fresh_contact_missing" } else { "inventory_incomplete" }, json!(contact_provenance)),
        "sessions":layer(if observed(census_rows) { json!(sessions) } else { Value::Null },
            sessions_complete, gap, provenance.clone()),
        "workers":layer(if observed(census_rows) { json!({"active_workers":workers,"commands":commands}) } else { Value::Null },
            workers_complete, gap, provenance.clone()),
        "jobs":layer(jobs, false, "retained_history_not_observed", provenance.clone()),
        "backlog":layer(if observed(backlog_rows) { json!(backlog) } else { Value::Null },
            backlog_complete, gap, provenance.clone()),
        "unknown":layer(Value::Null, false, "runtime_domain_unconfigured", provenance)
    });
    layers["unknown"]["observed_unknown_ships"] = json!(unknown_ships);
    let census_observed = observed(census_rows);
    layers["unknown"]["observed_unknown_sessions"] = if census_observed {
        json!(unknown_sessions)
    } else {
        Value::Null
    };
    layers["unknown"]["observed_uncertain_jobs"] = if census_observed {
        json!(uncertain_jobs)
    } else {
        Value::Null
    };
    view["layers"] = layers;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(mode: &str, census_complete: bool, executing: bool) -> Value {
        let current = matches!(mode, "local_live" | "remote_live" | "reused");
        let machine = if mode == "local_live" {
            "local"
        } else {
            "beta"
        };
        json!({"coverage":{"inventory_complete":true,"sources":[{
            "machine":machine,"facet":"inventory","mode":mode,
            "state":if current { "covered" } else { "cached" },"age_ms":42}]},
            "ships":[{"name":"survey","machine":machine,"open_tasks":3,
                "attended":true,"working":if executing { json!(true) } else if census_complete && current { json!(false) } else { json!("unknown") },
                "runtime":{"mode":mode,"current":current,"age_ms":42,"facts":{"census":{
                    "workers_complete":census_complete,"sessions_complete":census_complete,
                    "active_workers":u64::from(executing),"commands":0,
                    "sessions":{"live":u64::from(census_complete),"unknown":u64::from(!census_complete)},
                    "uncertain_jobs":u64::from(!census_complete)}}}}],
            "observed_totals":{"recorded_running_jobs":{"value":1,"complete":true},
                "nonterminal_jobs":{"value":1,"complete":true}}})
    }

    fn assert_contract(view: &Value, expected: [bool; 7]) {
        for (name, complete) in [
            "inventory",
            "reachability",
            "sessions",
            "workers",
            "jobs",
            "backlog",
            "unknown",
        ]
        .into_iter()
        .zip(expected)
        {
            let layer = &view["layers"][name];
            assert_eq!(layer["complete"], complete, "{name}: {view}");
            assert_eq!(layer["reason"].is_null(), complete, "{name}");
            assert!(layer["provenance"].is_array(), "{name}");
            assert!(layer.get("value").is_some(), "{name}");
        }
        assert!(
            view["layers"]["unknown"]["value"].is_null(),
            "unknown universe is never zero"
        );
    }

    #[test]
    fn active_and_verified_idle_do_not_imply_domain_completeness() {
        for executing in [true, false] {
            let mut view = fixture("local_live", true, executing);
            summarize(&mut view);
            assert_contract(&view, [true, true, true, true, false, true, false]);
            assert_eq!(
                view["layers"]["workers"]["value"]["active_workers"],
                u64::from(executing)
            );
            assert_eq!(view["layers"]["backlog"]["value"], 3);
            assert_eq!(view["layers"]["unknown"]["observed_unknown_ships"], 0);
        }
    }

    #[test]
    fn unverified_lease_is_not_a_checked_session() {
        let mut view = fixture("local_live", false, false);
        summarize(&mut view);
        assert_contract(&view, [true, true, false, false, false, true, false]);
        assert_eq!(view["layers"]["unknown"]["observed_unknown_ships"], 1);
        assert_eq!(view["layers"]["unknown"]["observed_unknown_sessions"], 1);
    }

    #[test]
    fn directory_and_cached_remotes_are_not_current_zeroes() {
        for mode in ["directory", "cache", "carried"] {
            let mut view = fixture(mode, true, true);
            if mode == "directory" {
                view["ships"][0].as_object_mut().unwrap().remove("runtime");
            }
            summarize(&mut view);
            assert_contract(&view, [true, false, false, false, false, false, false]);
            assert!(view["layers"]["sessions"]["value"].is_null());
            assert!(view["layers"]["workers"]["value"].is_null());
            assert!(view["layers"]["backlog"]["value"].is_null());
            assert_eq!(view["layers"]["unknown"]["observed_unknown_ships"], 1);
            let origin = &view["layers"]["sessions"]["provenance"][0];
            assert_eq!(
                origin["kind"],
                if mode == "directory" {
                    "directory_only"
                } else {
                    "cached"
                }
            );
            assert_eq!(
                origin["age_ms"],
                if mode == "directory" {
                    Value::Null
                } else {
                    json!(42)
                }
            );
        }
    }

    #[test]
    fn local_home_facts_without_census_are_not_fresh_census_provenance() {
        let mut view = fixture("local_live", true, false);
        view["ships"][0].as_object_mut().unwrap().remove("runtime");
        view["ships"][0]["observed_at"] = json!(1234);
        summarize(&mut view);
        for name in ["sessions", "workers"] {
            let layer = &view["layers"][name];
            assert_eq!(layer["provenance"][0]["kind"], "directory_only");
            assert!(layer["provenance"][0]["age_ms"].is_null());
            assert!(layer["value"].is_null());
            assert_eq!(layer["complete"], false);
        }
    }

    #[test]
    fn reused_census_is_current_but_not_new_machine_contact() {
        let mut view = fixture("reused", true, true);
        summarize(&mut view);
        assert_contract(&view, [true, false, true, true, false, true, false]);
        assert_eq!(
            view["layers"]["reachability"]["provenance"][0]["kind"],
            "cached"
        );
    }

    #[test]
    fn truncated_inventory_or_census_cannot_be_complete() {
        let mut view = fixture("local_live", true, false);
        view["coverage"]["inventory_complete"] = json!(false);
        summarize(&mut view);
        assert_contract(&view, [false; 7]);
        let mut view = fixture("local_live", false, false);
        summarize(&mut view);
        assert_contract(&view, [true, true, false, false, false, true, false]);
        view["ships"] = json!([]);
        view["coverage"]["inventory_complete"] = json!(false);
        summarize(&mut view);
        assert_contract(&view, [false; 7]);
        for name in ["sessions", "workers", "backlog"] {
            assert!(view["layers"][name]["value"].is_null());
        }
    }
}
