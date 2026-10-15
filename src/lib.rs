pub mod activation;
pub mod admission;
pub mod cli;
pub mod context;
pub mod core;
pub mod crew;
pub mod cursor_mail;
#[cfg(feature = "dashboard")]
pub mod dashboard;
pub mod door;
mod executable;
pub mod fleet;
pub mod herdr;
pub mod input;
pub mod jobs;
pub mod mail;
pub mod opencode_mail;
pub mod peers;
pub mod plugins;
pub mod presence;
pub mod project;
pub mod service;
pub mod store;
pub mod workspace;

use anyhow::{Result, bail};
use serde_json::Value;
use store::{Caller, Store};

/// Commands that change authority, configuration or capacity. Agents
/// launched by Privateer cannot run them; see `store::Caller`.
pub const OPERATOR_COMMANDS: &[&str] = &[
    "init",
    "charter.put",
    "charter.revoke",
    "approval.grant",
    "approval.consume",
    "peer.put",
    "resource.recover",
    "history.prune",
    "ship.put",
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
        "job" | "resource" => jobs::dispatch(store, command, args),
        "peer" => peers::dispatch(store, command, args),
        "plugin" => plugins::dispatch(store, command, args),
        "workspace" => workspace::dispatch(store, command, args),
        "ship" if command == "ship.bearings" => context::dispatch(store, command, args),
        "fleet" => fleet::dispatch(store, command, args),
        "project" => project::dispatch(store, command, args),
        "ship" if matches!(command, "ship.create" | "ship.retire" | "ship.actions") => {
            fleet::dispatch(store, command, args)
        }
        "ship" if matches!(command, "ship.attend" | "ship.leave" | "ship.presence") => {
            presence::dispatch(store, command, args)
        }
        "note" | "focus" => context::dispatch(store, command, args),
        "ship" | "mail" => mail::dispatch(store, command, args),
        "wake" => activation::dispatch(store, command, args),
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
            )?;
            if let Some(lease) = &execution.lease_id {
                args["_workspace_lease_id"] = serde_json::json!(lease);
            } else {
                args["_workspace_cwd"] = serde_json::json!(execution.root);
            }
            drop(execution);
            let plan = plugins::dispatch(store, "plugin.command", args.clone())?;
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
            for key in ["id", "estimate", "wait_for_capacity", "max_wait_secs"] {
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
