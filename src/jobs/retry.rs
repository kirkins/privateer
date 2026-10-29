//! Native retry eligibility/request reconstruction, shared with start's phases.
//! No caller-supplied launch overrides beyond native CLI's optional new ID.
use super::*;

pub(super) struct Request {
    pub(super) args: Value,
    pub(super) old_id: String,
    pub(super) agent: bool,
}

/// Read the durable status, recovery charter and original route's backoff;
/// never reconcile another process namespace just to decide whether to retry.
/// This helper owns no transaction and may run under a caller's IMMEDIATE
/// admission lock. Preparation (including workspace I/O) stays outside that
/// lock. A future guarded caller must revalidate the exact old job/authority
/// and this eligibility under its admission lock before committing intent.
pub(super) fn request(store: &Store, args: &Value) -> Result<Request> {
    let old_id = args
        .get("id")
        .and_then(Value::as_str)
        .context("id required")?;
    let old = load(store, old_id)?;
    ensure!(
        matches!(old.status.as_str(), "failed" | "cancelled" | "timed_out"),
        "retry requires a stopped unsuccessful job; no automatic retry of live/orphan work"
    );
    store.authorize(&old.task_id, "recovery")?;
    // Replays keep the route of the original launch. A fresh crew, not retry,
    // is how a caller takes the captain's first available route during backoff.
    let route = old
        .model_route
        .clone()
        .or_else(|| crate::routes::route_of_argv(&json!(old.command)));
    if let (true, Some(ship), Some(route)) = (
        old.action == "spawn" && old.agent_actor.is_some(),
        old.agent_actor.as_deref(),
        route.as_deref(),
    ) {
        let refusal = crate::routes::refusal(store, route)?
            .or(crate::crew_routes::retry_refusal(store, ship, route)?);
        if let Some(refusal) = refusal {
            bail!(
                "{refusal}; start a fresh crew with agent.start to take the first available route"
            );
        }
    }
    let mut next = json!({"task_id": old.task_id, "command": old.command, "cwd": old.cwd, "heavy": old.heavy, "action": old.action, "timeout_secs": old.timeout_secs, "cleanup_descendants":old.cleanup_descendants, "agent_actor": old.agent_actor, "estimate": old.estimate, "wait_for_capacity": old.wait_for_capacity, "max_wait_secs": old.max_wait_secs, "codex_read_only_crew": old.codex_read_only_crew});
    if let Some(route) = &route {
        next["model_route"] = json!(route);
    }
    if let Some(name) = &old.route_name {
        next["route_name"] = json!(name);
    }
    if let Some(why) = &old.route_why {
        next["route_why"] = json!(why);
    }
    if old.hosted {
        next["hosted"] = json!(true);
    }
    if let Some(id) = args.get("new_id") {
        next["id"] = id.clone();
    }
    if let Some(binding) = &old.plugin_binding {
        next["plugin_binding"] = binding.clone();
    }
    if let Some(lease) = &old.workspace_lease_id {
        next["workspace_lease_id"] = json!(lease);
    }
    Ok(Request {
        args: next,
        old_id: old_id.to_owned(),
        agent: old.action == "spawn",
    })
}
