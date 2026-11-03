//! Per-home read completeness is distinct from machine contact and freshness.
//! All inputs have already passed the closed runtime projection.
use serde_json::{Value, json};

#[cfg(test)]
#[path = "runtime_facets_tests.rs"]
mod tests;

pub(super) fn omitted(machine: &str, reason: &str) -> Value {
    json!({"machine":machine,"attempted":false,"successful":false,"failed":false,
        "reused":false,"reverse_only":false,"reason":reason,
        "provenance":"directory_only","contact_age_ms":null,"received_at":null})
}

fn facet(state: &str, current: bool, provenance: &str, age: Value, reason: &str) -> Value {
    json!({"state":state,"current":current,"provenance":provenance,
        "age_ms":age,"reason":reason})
}

pub(super) fn ship(row: &mut Value, section_truncated: bool, contact: Option<&Value>) {
    let local = row["machine"] == "local";
    let runtime = &row["runtime"];
    let facts = &runtime["facts"];
    let current = runtime["current"] == true;
    let provenance = match runtime["mode"].as_str() {
        Some("local_live" | "remote_live") => "fresh",
        Some("reused" | "cache") => "cached",
        Some("carried") => "carried",
        _ => "directory_only",
    };
    let age = if local && facts.is_object() {
        json!(0)
    } else {
        runtime["age_ms"].clone()
    };
    let missing = if section_truncated {
        "truncated"
    } else {
        "missing"
    };
    let missing_reason = if section_truncated {
        "section_truncated"
    } else {
        "not_observed"
    };
    let census = &facts["census"];
    let census_state = if !census.is_object() {
        missing
    } else if census["sessions_complete"] == true
        && census["workers_complete"] == true
        && census["activity_complete"] == true
    {
        "read"
    } else {
        "truncated"
    };
    let jobs_state = if !facts["jobs_total"].is_u64() {
        missing
    } else if facts["jobs_truncated"] == true {
        "truncated"
    } else {
        "read"
    };
    let backlog_read = facts["open_tasks"].is_u64() || (local && row["open_tasks"].is_u64());
    let contact = if local {
        // This core is not a remote contact attempt.
        facet("read", true, "fresh", json!(0), "local_core")
    } else if let Some(c) = contact {
        let current = c["successful"] == true || c["reused"] == true;
        facet(
            if current || c["received_at"].is_i64() {
                "read"
            } else {
                "missing"
            },
            current,
            c["provenance"].as_str().unwrap_or("directory_only"),
            c["contact_age_ms"].clone(),
            c["reason"].as_str().unwrap_or("not_observed"),
        )
    } else {
        facet(
            "missing",
            false,
            "directory_only",
            Value::Null,
            "not_observed",
        )
    };
    row["facets"] = json!({
        "directory":facet("read", true, "directory_only", Value::Null, "selected_directory"),
        "contact":contact,
        "census":facet(census_state, current && census.is_object(), provenance, age.clone(),
            census["reason"].as_str().unwrap_or(missing_reason)),
        "jobs":facet(jobs_state, current && facts["jobs_total"].is_u64(), provenance, age.clone(),
            if jobs_state == "truncated" { if facts["jobs_total"].is_u64() { "state_counts_truncated" } else { missing_reason } }
            else if jobs_state == "read" { "recorded_nonterminal_only" } else { missing_reason }),
        "backlog":facet(if backlog_read { "read" } else { missing },
            backlog_read && (local || current), if local && backlog_read { "fresh" } else { provenance },
            if local && backlog_read { json!(0) } else { age },
            if backlog_read { "recorded_open_tasks_only" } else { missing_reason })});
}
