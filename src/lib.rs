pub mod core;
pub mod jobs;
pub mod peers;
pub mod plugins;
pub mod store;

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
];

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    if !args.is_object() {
        bail!("command arguments must be a JSON object");
    }
    if let Caller::Agent { actor } = store.caller() {
        if OPERATOR_COMMANDS.contains(&command) {
            bail!("{command} is an operator command; agents cannot run it");
        }
        for key in ["actor", "from"] {
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
        "agent" if command == "agent.start" => {
            let task = args
                .get("task_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("task_id is required"))?
                .to_owned();
            let charter = store.authorize(&task, "spawn")?;
            let owner = store.task(&task)?["owner"].clone();
            let plan = plugins::dispatch(store, "plugin.command", args.clone())?;
            // The launched harness acts as the task owner, under the agent role.
            let mut job = serde_json::json!({
                "task_id": task,
                "command": plan["argv"],
                "cwd": plan["cwd"],
                "cleanup_descendants": true,
                "agent_actor": owner,
                "heavy": args.get("heavy").cloned().unwrap_or(Value::Bool(true))
            });
            if let Some(id) = args.get("id") {
                job["id"] = id.clone();
            }
            if let Some(binding) = charter.get("provider_plugin").filter(|v| !v.is_null()) {
                job["plugin_binding"] = binding.clone();
            }
            jobs::dispatch(store, "job.start", job)
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
