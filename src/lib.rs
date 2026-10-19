pub mod activation;
pub mod activity;
pub mod admission;
pub mod attention;
pub mod away;
mod block_ask;
mod bundled;
pub mod captain;
pub mod cli;
pub mod context;
pub mod core;
pub mod credentials;
pub mod crew;
pub mod crew_routes;
pub mod cursor_mail;
#[cfg(feature = "dashboard")]
pub mod dashboard;
pub mod digest;
pub mod door;
pub mod errand;
mod executable;
mod feed;
pub mod fleet;
pub mod grants;
pub mod herdr;
pub mod host;
pub mod input;
pub mod jobs;
pub mod machines;
pub mod mail;
pub mod moves;
pub mod needs;
pub mod opencode_mail;
pub mod peers;
pub mod permissions;
pub mod plugins;
pub mod presence;
mod profile;
pub mod project;
pub mod releases;
mod reply;
pub mod report;
pub mod routes;
pub mod scheduling;
pub mod service;
pub mod session;
mod snapshot;
pub mod store;
pub mod stow;
mod tool_bundles;
mod transcript;
mod usage;
mod usage_carry;
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
    "ship.move",
    "permissions.set",
    "bundle.configure",
    "credential.set",
    "credential.list",
    "credential.remove",
    "stow.start",
    "stow.status",
    "stow.resume",
    "captain.reply",
    "captain.dismiss",
    "captain.send",
    "session.read",
    "session.send",
    "captain.feed",
    "ship.profile",
    "wake.transcript",
    "fleet.snapshot",
    "report.get",
    "usage.summary",
    "activity.list",
];

/// Whether a command is the operator's alone for this caller. The ship the
/// captain named as this machine's owner may also stow the machine.
pub fn operator_only(caller: &Caller, command: &str) -> bool {
    if !OPERATOR_COMMANDS.contains(&command) {
        return false;
    }
    match caller {
        Caller::Agent { actor } => !(command.starts_with("stow.") && machines::is_owner(actor)),
        Caller::Operator => true,
    }
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    if !args.is_object() {
        bail!("command arguments must be a JSON object");
    }
    if let Caller::Agent { actor } = store.caller() {
        if operator_only(store.caller(), command) {
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
        // Never over RPC or a peer: the secret is read from the captain's stdin.
        "credential" => {
            bail!("credential commands run only from the captain's CLI, with the secret on stdin")
        }
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
        "fleet" if command == "fleet.snapshot" => {
            store.caller().require_operator(command)?;
            snapshot::snapshot(&args)
        }
        "fleet" => fleet::dispatch(store, command, args),
        "usage" if command == "usage.summary" => {
            store.caller().require_operator(command)?;
            usage::summary(store, &args)
        }
        "activity" if command == "activity.list" => {
            store.caller().require_operator(command)?;
            activity::list(store, &args)
        }
        "machine" => machines::dispatch(store, command, args),
        "grant" => grants::dispatch(store, command, args),
        "stow" => stow::dispatch(store, command, args),
        "captain" => captain::dispatch(store, command, args),
        "session" => session::dispatch(store, command, args),
        "report" => report::dispatch(store, command, args),
        "project" => project::dispatch(store, command, args),
        "ship" if command == "ship.move" => moves::dispatch(store, command, args),
        "ship" if command == "ship.profile" => {
            store.caller().require_operator(command)?;
            profile::profile(&args)
        }
        "ship"
            if matches!(
                command,
                "ship.create"
                    | "ship.retire"
                    | "ship.actions"
                    | "ship.scope"
                    | "ship.consumes"
                    | "ship.consumers"
            ) =>
        {
            fleet::dispatch(store, command, args)
        }
        "ship" if matches!(command, "ship.attend" | "ship.leave" | "ship.presence") => {
            presence::dispatch(store, command, args)
        }
        "note" | "focus" => context::dispatch(store, command, args),
        "ship" | "mail" => mail::dispatch(store, command, args),
        "wake" if command == "wake.transcript" => {
            store.caller().require_operator(command)?;
            transcript::transcript(&args)
        }
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
            // A task with a crew parked on a heavy job (or held) is not launched
            // over: a second crew would run beside it. `replace` cancels it first.
            let replace = match args.as_object_mut().unwrap().remove("replace") {
                Some(value) => value
                    .as_bool()
                    .ok_or_else(|| anyhow::anyhow!("replace must be boolean"))?,
                None => false,
            };
            // Plan against the task checkout; first-mate wakes have a durable
            // wake row and keep their existing ship repository.
            args.as_object_mut().unwrap().remove("_workspace_lease_id");
            args.as_object_mut().unwrap().remove("_workspace_cwd");
            use sha2::{Digest, Sha256};
            let request_hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&args)?));
            if let Some(existing) = jobs::agent_replay(store, &args, &request_hash)? {
                return Ok(existing);
            }
            // A new launch is not made over a parked crew (an identical repeat
            // above returns the job it already started instead).
            if !workspace::is_wake(store, &args)? {
                let parked = crew::parked_crews(store, &task)?;
                if !parked.is_empty() {
                    if replace {
                        for crew_job in &parked {
                            jobs::cancel_crew(store, crew_job)?;
                        }
                    } else {
                        anyhow::bail!(
                            "task {task} has a parked crew ({}); it resumes when its heavy job settles, or crew.resume if it is held. Pass replace:true to cancel it and the jobs it waits on instead",
                            parked.join(", ")
                        );
                    }
                }
            }
            // A crew denied a tool waits on its captain ask; launching another
            // into the same denial only repeats it. `replace` tries another way.
            if !workspace::is_wake(store, &args)?
                && !replace
                && let Some((ask, rules)) = block_ask::crew_block(store, &task)?
            {
                anyhow::bail!(
                    "task {task} waits on captain decision {ask} (Claude was denied {rules}); it resumes when the captain answers or the fleet policy covers it. Pass replace:true to start a crew that works another way; the ask stays open"
                );
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
            let require = crew_routes::parse_require(
                args.as_object_mut().unwrap().remove("require").as_ref(),
            )?;
            // Whether the options are the chosen route's or profile's, so the
            // memory figure they carry is theirs; caller options never are.
            let mut route_options = false;
            let mut route_choice: Option<crew_routes::Choice> = None;
            if charter.get("provider_plugin").is_none_or(Value::is_null)
                && !workspace::is_wake(store, &args)?
                && let Some(ship) = owner.as_str()
            {
                match args.get("manifest").and_then(Value::as_str) {
                    Some(manifest) => {
                        anyhow::ensure!(requested.is_none(), "pass route or manifest, not both");
                        let resume_why = args
                            .get("_resume_why")
                            .and_then(Value::as_str)
                            .filter(|_| matches!(store.caller(), Caller::Operator));
                        route_choice = crew_routes::check(
                            store,
                            ship,
                            "crew",
                            manifest,
                            &args["options"],
                            require.as_ref(),
                            resume_why,
                        )?;
                    }
                    None => {
                        if let Some(choice) = crew_routes::choose(
                            store,
                            ship,
                            "crew",
                            requested.as_deref(),
                            require.as_ref(),
                        )? {
                            // The agent may add settings such as effort; the
                            // route decides the provider, model and memory.
                            let mut options = choice.options.clone();
                            if let Some(extra) = args.get("options").and_then(Value::as_object) {
                                for (key, value) in extra {
                                    if !matches!(
                                        key.as_str(),
                                        "provider" | "model" | "memory_bytes"
                                    ) {
                                        options[key] = value.clone();
                                    }
                                }
                            }
                            args["manifest"] = choice.manifest.clone();
                            args["options"] = options;
                            route_options = true;
                            route_choice = Some(choice);
                        }
                    }
                }
            } else {
                anyhow::ensure!(
                    requested.is_none() && require.is_none(),
                    "route and require apply only to crew launches"
                );
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
                        route_options = true;
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
            // captain can watch and steer them, when the host answers. Crew on
            // a harness that cannot be hosted run headless with a viewer space.
            let (hosted, viewed) = if workspace::is_wake(store, &args)? {
                (false, false)
            } else {
                let profile = owner
                    .as_str()
                    .and_then(|ship| {
                        mail::dispatch(store, "ship.show", serde_json::json!({"name": ship})).ok()
                    })
                    .map(|ship| ship["first_mate"].clone())
                    .unwrap_or_default();
                let harness = args["manifest"].as_str().and_then(bundled::harness);
                let hosted = harness
                    .as_deref()
                    .is_some_and(|harness| host::hosts_crew(&profile, harness));
                let viewed = !hosted
                    && harness
                        .as_deref()
                        .is_some_and(|harness| host::views_crew(&profile, harness));
                (hosted, viewed)
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
            // A route's memory figure sizes admission; the harness never sees it.
            // Only a chosen route or profile may set it: a memory figure the
            // caller passes with an explicit manifest or no route is dropped,
            // so a caller cannot shrink its own crew's reservation.
            let caller_memory = args
                .get_mut("options")
                .and_then(Value::as_object_mut)
                .and_then(|options| options.remove("memory_bytes"));
            let route_memory = if route_options { caller_memory } else { None };
            let mut plan = plugins::plan_launch(store, "plugin.command", args.clone())?;
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
            // The budget a resumed crew has spent; only the runtime's resume sets it.
            if let Some(spent) = args
                .get("_run_ms")
                .filter(|_| matches!(store.caller(), Caller::Operator))
            {
                job["_run_ms"] = spent.clone();
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
            let is_wake = workspace::is_wake(store, &args)?;
            let route_name = route_choice.as_ref().map(|c| c.name.clone()).or_else(|| {
                if is_wake {
                    args.get("route_name")
                        .and_then(Value::as_str)
                        .map(String::from)
                } else {
                    None
                }
            });
            let route_why = route_choice.as_ref().map(|c| c.why.clone()).or_else(|| {
                if is_wake {
                    args.get("route_why")
                        .and_then(Value::as_str)
                        .map(String::from)
                } else {
                    None
                }
            });
            if let Some(name) = &route_name {
                job["route_name"] = serde_json::json!(name);
            }
            if let Some(why) = &route_why {
                job["route_why"] = serde_json::json!(why);
            }
            // Crew reserve memory like any estimated job; a first mate's own
            // wake is not crew and stays outside admission. Errands are wakes
            // too, so they stay outside admission as well: they are short and
            // the first mate's own.
            if !is_wake {
                // A resume reuses the estimate its crew was admitted under: the
                // runtime's own resume only (see `crew::resume`).
                let estimate = match args
                    .get("_resume_estimate")
                    .filter(|_| matches!(store.caller(), Caller::Operator))
                {
                    Some(recorded) => serde_json::from_value(recorded.clone())?,
                    None => jobs::crew_estimate(store, &charter, &args, route_memory)?,
                };
                job["estimate"] = serde_json::to_value(estimate)?;
                // What a park resumes: this launch again, through admission.
                job["launch"] = serde_json::json!({
                    "manifest": args.get("manifest"), "options": args.get("options"),
                    "config": args.get("config"), "tool_bundles": args.get("tool_bundles"),
                    "cwd": args.get("cwd"),
                    "heavy": args.get("heavy").cloned().unwrap_or(Value::Bool(false)),
                    "require": require.as_ref().map(|r| r.to_json()),
                });
            }
            let mut started = jobs::start_agent(store, job.clone())?;
            if viewed {
                // Presentation only: a refused space never stops the crew.
                let viewer = host::view_crew(
                    store,
                    started["id"].as_str().unwrap_or_default(),
                    &task,
                    owner.as_str().unwrap_or_default(),
                    std::path::Path::new(started["cwd"].as_str().unwrap_or_default()),
                );
                started["viewer"] = serde_json::json!(viewer.is_ok());
            }
            started["route"] = route_choice
                .as_ref()
                .map(|c| c.to_route_json())
                .unwrap_or_else(|| {
                    serde_json::json!({
                        "name": route_name,
                        "identity": job.get("model_route").cloned().unwrap_or(Value::Null),
                        "tier": Value::Null,
                        "uses": [],
                        "why": route_why,
                    })
                });
            Ok(started)
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
