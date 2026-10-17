pub mod activation;
pub mod admission;
pub mod attention;
pub mod away;
mod bundled;
pub mod captain;
pub mod cli;
pub mod context;
pub mod core;
pub mod crew;
pub mod crew_routes;
pub mod cursor_mail;
#[cfg(feature = "dashboard")]
pub mod dashboard;
pub mod door;
pub mod errand;
mod executable;
pub mod fleet;
pub mod herdr;
pub mod host;
pub mod input;
pub mod jobs;
pub mod machines;
pub mod mail;
pub mod needs;
pub mod opencode_mail;
pub mod peers;
pub mod permissions;
pub mod plugins;
pub mod presence;
pub mod project;
pub mod releases;
mod reply;
pub mod routes;
pub mod scheduling;
pub mod service;
pub mod store;
pub mod stow;
mod tool_bundles;
pub mod watch;
pub mod workspace;

use anyhow::{Result, bail};
use serde_json::Value;
use store::{Caller, Store};

/// Default a bundled Codex crew's sandbox when the caller chose none. Absent
/// or malformed options are left for the plugin layer to refuse cleanly rather
/// than panicking on an index assignment here. Returns whether this is a
/// bundled Codex launch.
fn codex_crew_sandbox_default(args: &mut Value) -> bool {
    if args["manifest"] != "bundled:codex" {
        return false;
    }
    if args["options"].is_null() {
        args["options"] = serde_json::json!({});
    }
    if args["options"].is_object() && args["options"].get("sandbox").is_none() {
        args["options"]["sandbox"] = serde_json::json!("danger-full-access");
    }
    true
}

/// Commands that change authority, configuration or capacity. Agents
/// launched by Privateer cannot run them; see `store::Caller`.
pub const OPERATOR_COMMANDS: &[&str] = &[
    "init",
    "charter.put",
    "charter.revoke",
    "approval.grant",
    "approval.consume",
    "approval.deny",
    "peer.put",
    "resource.recover",
    "history.prune",
    "ship.put",
    "permissions.set",
    "bundle.configure",
    "stow.start",
    "stow.status",
    "stow.resume",
];

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    if !args.is_object() {
        bail!("command arguments must be a JSON object");
    }
    if let Caller::Agent { actor } = store.caller() {
        if OPERATOR_COMMANDS.contains(&command) {
            bail!("{command} is an operator command; agents cannot run it");
        }
        // Agents speak, and read mail, only as themselves.
        for key in ["actor", "from", "address", "ship"] {
            if let Some(claimed) = args.get(key) {
                if claimed.as_str() != Some(actor) {
                    bail!("agent {actor} cannot act as {claimed}");
                }
            }
        }
    }
    match command.split('.').next().unwrap_or("") {
        "runtime" if command == "runtime.needs" => needs::query(store, &args),
        "upgrade" if command == "upgrade.status" => releases::status(),
        "bundle" if command == "bundle.configure" => tool_bundles::configure(store, args),
        "permissions" => permissions::dispatch(store, command, args),
        "routes" => crew_routes::dispatch(store, command, args),
        "work" => scheduling::dispatch(store, command, args),
        "task" if matches!(command, "task.enqueue" | "task.defer") => {
            scheduling::dispatch(store, command, args)
        }
        "job" | "resource" => jobs::dispatch(store, command, args),
        "peer" => peers::dispatch(store, command, args),
        "plugin" => plugins::dispatch(store, command, args),
        "workspace" => workspace::dispatch(store, command, args),
        "ship" if command == "ship.bearings" => context::dispatch(store, command, args),
        "fleet" => fleet::dispatch(store, command, args),
        "machine" => machines::dispatch(store, command, args),
        "stow" => stow::dispatch(store, command, args),
        "captain" => captain::dispatch(store, command, args),
        "project" => project::dispatch(store, command, args),
        "ship"
            if matches!(
                command,
                "ship.create" | "ship.retire" | "ship.actions" | "ship.scope"
            ) =>
        {
            fleet::dispatch(store, command, args)
        }
        "ship" if matches!(command, "ship.attend" | "ship.leave" | "ship.presence") => {
            presence::dispatch(store, command, args)
        }
        "note" | "focus" => context::dispatch(store, command, args),
        "ship" | "mail" => mail::dispatch(store, command, args),
        "wake" => activation::dispatch(store, command, args),
        "errand" => errand::dispatch(store, command, args),
        "attention" => attention::dispatch(store, command, args),
        "crew" => crew::dispatch(store, command, args),
        "agent" if command == "agent.start" => {
            let mut args = args;
            let task = args
                .get("task_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("task_id is required"))?
                .to_owned();
            let charter = store.authorize(&task, "spawn")?;
            let owner = store.task(&task)?["owner"].clone();
            // Plan against the task checkout; first-mate wakes have a durable
            // wake row and keep their existing ship repository.
            args.as_object_mut().unwrap().remove("_workspace_lease_id");
            args.as_object_mut().unwrap().remove("_workspace_cwd");
            use sha2::{Digest, Sha256};
            let request_hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&args)?));
            if let Some(existing) = jobs::agent_replay(store, &args, &request_hash)? {
                return Ok(existing);
            }
            // The captain's routes come first: crew take the route they name
            // or the first listed one that is not backing off, and agents
            // launch nothing unlisted. Wakes and errands bring their own route.
            let requested = args
                .as_object_mut()
                .unwrap()
                .remove("route")
                .map(|r| {
                    r.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| anyhow::anyhow!("route must be a route name"))
                })
                .transpose()?;
            if charter.get("provider_plugin").is_none_or(Value::is_null)
                && !workspace::is_wake(store, &args)?
                && let Some(ship) = owner.as_str()
            {
                match args.get("manifest").and_then(Value::as_str) {
                    Some(manifest) => {
                        anyhow::ensure!(requested.is_none(), "pass route or manifest, not both");
                        crew_routes::check(store, ship, "crew", manifest, &args["options"])?;
                    }
                    None => {
                        if let Some(choice) =
                            crew_routes::choose(store, ship, "crew", requested.as_deref())?
                        {
                            // The agent may add settings such as effort; the
                            // route decides the provider and model.
                            let mut options = choice.options;
                            if let Some(extra) = args.get("options").and_then(Value::as_object) {
                                for (key, value) in extra {
                                    if !matches!(key.as_str(), "provider" | "model") {
                                        options[key] = value.clone();
                                    }
                                }
                            }
                            args["manifest"] = choice.manifest;
                            args["options"] = options;
                        }
                    }
                }
            } else {
                anyhow::ensure!(requested.is_none(), "route applies only to crew launches");
            }
            // Crew default to their ship's first-mate harness and model, the
            // route the captain chose, unless the charter pins a provider.
            if args.get("manifest").is_none()
                && charter.get("provider_plugin").is_none_or(Value::is_null)
            {
                let profile = owner
                    .as_str()
                    .and_then(|ship| {
                        mail::dispatch(store, "ship.show", serde_json::json!({"name": ship})).ok()
                    })
                    .map(|ship| ship["first_mate"].clone())
                    .filter(|profile| profile["manifest"].is_string());
                if let Some(profile) = profile {
                    args["manifest"] = profile["manifest"].clone();
                    if args.get("options").is_none() && profile["options"].is_object() {
                        args["options"] = profile["options"].clone();
                    }
                }
            }
            // Codex crew need network and supervised nested jobs, like the Pi
            // crew. Keep an explicit sandbox choice, including read-only.
            let codex = codex_crew_sandbox_default(&mut args);
            let codex_read_only = codex
                && args["options"].get("sandbox").and_then(Value::as_str) == Some("read-only");
            // A provider that is throttling this route only fails another run.
            let model_route = args["manifest"]
                .as_str()
                .and_then(|m| routes::route_of(m, &args["options"]));
            if let Some(route) = &model_route {
                if let Some(refusal) = routes::refusal(store, route)? {
                    anyhow::bail!(refusal);
                }
            }
            // Crew of a hosted ship run live in their own space, where the
            // captain can watch and steer them, when the host answers.
            let hosted = !workspace::is_wake(store, &args)? && {
                let profile = owner
                    .as_str()
                    .and_then(|ship| {
                        mail::dispatch(store, "ship.show", serde_json::json!({"name": ship})).ok()
                    })
                    .map(|ship| ship["first_mate"].clone())
                    .unwrap_or_default();
                args["manifest"]
                    .as_str()
                    .and_then(bundled::harness)
                    .is_some_and(|harness| host::hosts_crew(&profile, &harness))
            };
            if hosted {
                if !args["options"].is_object() {
                    args["options"] = serde_json::json!({});
                }
                args["options"]["mode"] = serde_json::json!("interactive");
            }
            let repo = std::fs::canonicalize(
                charter["repo"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("charter repo missing"))?,
            )?;
            let selected = match args.get("cwd") {
                Some(cwd) => std::fs::canonicalize(
                    cwd.as_str()
                        .ok_or_else(|| anyhow::anyhow!("cwd must be a directory path"))?,
                )?,
                None => repo.clone(),
            };
            let execution = workspace::execution(
                store,
                &task,
                &repo,
                &selected,
                !workspace::is_wake(store, &args)?,
                None,
                workspace::WorkspaceWait::none(),
            )?;
            if let Some(lease) = &execution.lease_id {
                args["_workspace_lease_id"] = serde_json::json!(lease);
            } else {
                args["_workspace_cwd"] = serde_json::json!(execution.root);
            }
            drop(execution);
            let mut plan = plugins::dispatch(store, "plugin.command", args.clone())?;
            if hosted {
                // The crew extension ends the session when its work settles.
                let plugins = cli::plugins_dir()?;
                let extension = plugins.join("attended/crew.ts");
                let calm = plugins.join("attended/calm.ts");
                anyhow::ensure!(calm.is_file(), "Pi calm extension missing");
                let argv = plan["argv"]
                    .as_array_mut()
                    .ok_or_else(|| anyhow::anyhow!("plan without argv"))?;
                let at = argv
                    .iter()
                    .rposition(|a| a == "--")
                    .ok_or_else(|| anyhow::anyhow!("interactive plan without a prompt"))?;
                argv.splice(
                    at..at,
                    [
                        serde_json::json!("-e"),
                        serde_json::json!(extension),
                        serde_json::json!("-e"),
                        serde_json::json!(calm),
                    ],
                );
            }
            // The launched harness acts as the task owner, under the agent role.
            let mut job = serde_json::json!({
                "task_id": task,
                "command": plan["argv"],
                "cwd": plan["cwd"],
                "cleanup_descendants": true,
                "agent_actor": owner,
                "agent_request_hash": request_hash,
                // The harness mostly waits on its model; builds it runs go
                // through job.start with heavy and wait_for_capacity.
                "heavy": args.get("heavy").cloned().unwrap_or(Value::Bool(false))
            });
            if codex_read_only {
                job["codex_read_only_crew"] = Value::Bool(true);
            }
            for key in [
                "id",
                "estimate",
                "wait_for_capacity",
                "max_wait_secs",
                "work_offer",
            ] {
                if let Some(value) = args.get(key) {
                    job[key] = value.clone();
                }
            }
            if let Some(lease) = args.get("_workspace_lease_id") {
                job["workspace_lease_id"] = lease.clone();
            }
            if let Some(binding) = charter.get("provider_plugin").filter(|v| !v.is_null()) {
                job["plugin_binding"] = binding.clone();
            }
            if hosted {
                job["hosted"] = serde_json::json!(true);
            }
            if let Some(route) = model_route {
                job["model_route"] = serde_json::json!(route);
            }
            jobs::start_agent(store, job)
        }
        "agent" => plugins::dispatch(store, command, args),
        _ => core::dispatch(store, command, args),
    }
}

pub fn rpc(store: &mut Store, request: Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let result = (|| -> Result<Value> {
        if !request.is_object() {
            bail!("request must be an object");
        }
        let command = request
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("command is required"))?;
        dispatch(
            store,
            command,
            request
                .get("args")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({})),
        )
    })();
    match result {
        Ok(result) => serde_json::json!({"id":id,"ok":true,"result":result}),
        Err(error) => serde_json::json!({"id":id,"ok":false,"error":error.to_string()}),
    }
}

#[cfg(test)]
mod codex_crew_sandbox_default_tests {
    use super::codex_crew_sandbox_default;
    use serde_json::json;

    #[test]
    fn defaults_only_when_absent_and_never_panics_on_malformed_options() {
        let mut absent = json!({"manifest":"bundled:codex","options":{}});
        assert!(codex_crew_sandbox_default(&mut absent));
        assert_eq!(absent["options"]["sandbox"], json!("danger-full-access"));

        let mut null = json!({"manifest":"bundled:codex","options":null});
        assert!(codex_crew_sandbox_default(&mut null));
        assert_eq!(null["options"]["sandbox"], json!("danger-full-access"));

        let mut chosen = json!({"manifest":"bundled:codex","options":{"sandbox":"read-only"}});
        assert!(codex_crew_sandbox_default(&mut chosen));
        assert_eq!(chosen["options"]["sandbox"], json!("read-only"));

        let mut malformed = json!({"manifest":"bundled:codex","options":"nope"});
        assert!(codex_crew_sandbox_default(&mut malformed));
        assert_eq!(malformed["options"], json!("nope"));

        let mut other = json!({"manifest":"bundled:pi","options":{}});
        assert!(!codex_crew_sandbox_default(&mut other));
        assert!(other["options"].get("sandbox").is_none());
    }
}
