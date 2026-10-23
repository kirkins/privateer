pub mod activation;
pub mod activity;
pub mod admission;
pub mod attention;
mod automode;
pub mod away;
mod block_ask;
mod bundled;
pub mod capabilities;
pub mod captain;
pub mod captain_approvals;
mod claude_denials;
pub mod cli;
pub mod context;
pub mod core;
pub mod credentials;
pub mod crew;
mod crew_events;
mod crew_launch;
pub mod crew_relaunch;
pub mod crew_routes;
pub mod crew_scope;
pub mod cursor_mail;
#[cfg(feature = "dashboard")]
pub mod dashboard;
pub mod digest;
pub mod door;
pub mod errand;
mod executable;
mod feed;
pub mod files;
mod first_mate_wait;
pub mod fleet;
pub mod grants;
pub mod herdr;
pub mod host;
pub mod input;
pub mod jobs;
pub mod machines;
pub mod mail;
pub mod mcp_broker;
pub mod moves;
pub mod needs;
pub mod opencode_mail;
pub mod peers;
pub mod permissions;
pub mod placement;
pub mod plugins;
pub mod presence;
pub mod privileged;
mod profile;
pub mod project;
pub mod project_permissions;
mod readiness;
pub mod releases;
mod reply;
pub mod report;
pub mod review;
pub mod routes;
pub mod scheduling;
pub mod service;
pub mod session;
mod snapshot;
mod stale_tasks;
pub mod store;
pub mod stow;
mod tool_bundles;
mod transcript;
mod usage;
mod usage_carry;
pub mod waits;
mod wake_reason;
pub mod watch;
pub mod workspace;

use anyhow::{Context, Result, bail};
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
    "ship.clean_moved",
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
    "captain_approval.revoke",
    "session.read",
    "session.send",
    "captain.feed",
    "ship.profile",
    "wake.transcript",
    "fleet.snapshot",
    "report.get",
    "file.put",
    "usage.summary",
    "activity.list",
    "privileged.run",
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

/// The argument keys each command takes, as `(command, keys)`, declared
/// beside its handler. Keys starting with `_` are the runtime's own and are
/// not named to callers.
pub type Accepted = &'static [(&'static str, &'static [&'static str])];

/// Every module's accepted argument keys. A command listed in none of these
/// is unknown to `dispatch`.
const ACCEPTED: &[Accepted] = &[
    ACCEPTED_HERE,
    activation::ACCEPTED,
    activity::ACCEPTED,
    attention::ACCEPTED,
    capabilities::ACCEPTED,
    captain::ACCEPTED,
    captain_approvals::ACCEPTED,
    context::ACCEPTED,
    core::ACCEPTED,
    crew::ACCEPTED,
    crew_routes::ACCEPTED,
    errand::ACCEPTED,
    files::ACCEPTED,
    fleet::ACCEPTED,
    grants::ACCEPTED,
    jobs::ACCEPTED,
    machines::ACCEPTED,
    mail::ACCEPTED,
    moves::ACCEPTED,
    needs::ACCEPTED,
    peers::ACCEPTED,
    permissions::ACCEPTED,
    placement::ACCEPTED,
    plugins::ACCEPTED,
    presence::ACCEPTED,
    privileged::ACCEPTED,
    profile::ACCEPTED,
    project::ACCEPTED,
    releases::ACCEPTED,
    releases::requests::ACCEPTED,
    report::ACCEPTED,
    review::ACCEPTED,
    scheduling::ACCEPTED,
    session::ACCEPTED,
    snapshot::ACCEPTED,
    stow::ACCEPTED,
    tool_bundles::ACCEPTED,
    transcript::ACCEPTED,
    usage::ACCEPTED,
    workspace::ACCEPTED,
];

/// The argument keys `command` takes, or None for a command `dispatch` does
/// not know.
pub fn accepted_args(command: &str) -> Option<&'static [&'static str]> {
    ACCEPTED
        .iter()
        .flat_map(|table| table.iter())
        .find(|(name, _)| *name == command)
        .map(|(_, keys)| *keys)
}

/// Refuse an unknown command, and every argument key its handler does not
/// take: a misspelt or invented key is an error, never silently ignored.
fn check_args(command: &str, args: &Value) -> Result<()> {
    let Some(valid) = accepted_args(command) else {
        bail!("unknown command {command}");
    };
    if TOLERANT.contains(&command) {
        return Ok(());
    }
    let unknown: Vec<&str> = args
        .as_object()
        .into_iter()
        .flat_map(|args| args.keys())
        .map(String::as_str)
        .filter(|key| !valid.contains(key))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let named: Vec<&str> = valid
        .iter()
        .copied()
        .filter(|key| !key.starts_with('_'))
        .collect();
    let s = if unknown.len() == 1 { "" } else { "s" };
    if named.is_empty() {
        bail!(
            "unknown argument{s} {} for {command}; it takes none",
            unknown.join(", ")
        );
    }
    bail!(
        "unknown argument{s} {} for {command}; valid: {}",
        unknown.join(", "),
        named.join(", ")
    )
}

/// Commands that carry another machine's record, which may be a newer
/// release's: an older machine keeps the message and ignores what it does not
/// know (see `envelope` in mail.rs), so these keys are listed but not enforced.
pub const TOLERANT: &[&str] = &["mail.deliver"];

/// Commands `dispatch` handles here rather than in a module.
const ACCEPTED_HERE: Accepted = &[(
    "agent.start",
    &[
        "task_id",
        "replace",
        "route",
        "require",
        "manifest",
        "options",
        "config",
        "tool_bundles",
        "cwd",
        "id",
        "heavy",
        "estimate",
        "wait_for_capacity",
        "max_wait_secs",
        "work_offer",
        "timeout_secs",
        "wake",
        "wake_only",
        // A wake's or errand's own route, honoured for wakes only.
        "route_name",
        "route_why",
        // Set by the runtime's resume and relaunch, honoured for the operator only.
        "_resume_why",
        "_resume_estimate",
        "_run_ms",
        "_resume_of",
        "_relaunch_of",
    ],
)];

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
    // Never over RPC or a peer: the secret is read from the captain's stdin.
    if command.starts_with("credential.") {
        bail!("credential commands run only from the captain's CLI, with the secret on stdin")
    }
    check_args(command, &args)?;
    crew_scope::check(store, command, &args)?;
    match command.split('.').next().unwrap_or("") {
        "runtime" if command == "runtime.needs" => needs::query(store, &args),
        "upgrade" if command == "upgrade.status" => releases::status(),
        "upgrade" if matches!(command, "upgrade.plan" | "upgrade.start") => {
            releases::requests::dispatch(store, command, &args)
        }
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
        "capability" => capabilities::dispatch(store, command, args),
        "grant" => grants::dispatch(store, command, args),
        "stow" => stow::dispatch(store, command, args),
        "captain" => captain::dispatch(store, command, args),
        "captain_approval" => captain_approvals::dispatch(store, command, args),
        "privileged" => privileged::dispatch(store, command, args),
        "session" => session::dispatch(store, command, args),
        "report" => report::dispatch(store, command, args),
        "file" => files::dispatch(store, command, args),
        "project" => project::dispatch(store, command, args),
        "ship" if matches!(command, "ship.move" | "ship.clean_moved") => {
            moves::dispatch(store, command, args)
        }
        "ship" if matches!(command, "ship.placement" | "ship.propose_move") => {
            placement::dispatch(store, command, args)
        }
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
                    | "ship.requires"
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
        "review" => review::dispatch(store, command, args),
        "pr" if command == "pr.merge" => review::dispatch(store, command, args),
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
            // route the captain chose, unless the charter pins a provider: the
            // first_mate list's route when the profile names no model, as the
            // first mate's own wakes take.
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
                if let Some(mut profile) = profile {
                    let ship = owner.as_str().unwrap_or_default();
                    // Options naming their own provider or model are explicit.
                    let named = args["options"].get("provider").is_some()
                        || args["options"].get("model").is_some();
                    // Crew are judged against each route's crew share from
                    // the start, so a later route that can take them is
                    // taken; a wake keeps the first mate's.
                    let who = if workspace::is_wake(store, &args)? {
                        routes::Who::FirstMate
                    } else {
                        routes::Who::Crew
                    };
                    let listed = if named {
                        crew_routes::Listed::Unlisted
                    } else {
                        crew_routes::listed_for(store, ship, &profile, who)?
                    };
                    match listed {
                        crew_routes::Listed::Unlisted => {}
                        crew_routes::Listed::Route(choice) => {
                            crew_routes::onto(&mut profile, &choice);
                            // The agent may add settings such as effort, as on
                            // a crew route; the route decides the rest.
                            if let Some(extra) = args.get("options").and_then(Value::as_object) {
                                for (key, value) in extra {
                                    if !matches!(
                                        key.as_str(),
                                        "provider" | "model" | "memory_bytes"
                                    ) {
                                        profile["options"][key] = value.clone();
                                    }
                                }
                            }
                            args["options"] = profile["options"].clone();
                            route_options = true;
                            route_choice = Some(crew_routes::Choice {
                                manifest: profile["manifest"].clone(),
                                options: profile["options"].clone(),
                                ..choice
                            });
                        }
                        crew_routes::Listed::Unavailable(why) => anyhow::bail!(
                            "{why}; nothing is queued: retry once one clears, or name a route or manifest"
                        ),
                    }
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
            // A provider that is throttling this route only fails another run,
            // and a plan past the share this route keeps for the kind of launch
            // this is only fails it too. A first mate's wake is judged as one:
            // it keeps the route crew are refused, and moves on its own.
            let model_route = args["manifest"]
                .as_str()
                .and_then(|m| routes::route_of(m, &args["options"]));
            let wake = workspace::is_wake(store, &args)?;
            if let Some(route) = &model_route {
                let who = if wake {
                    routes::Who::FirstMate
                } else {
                    routes::Who::Crew
                };
                let limits = crew_routes::limits_of(store, route)?;
                if let Some(refusal) = routes::refusal_for(store, route, who, limits)? {
                    anyhow::bail!(refusal);
                }
            }
            // Crew of a hosted ship run live in their own space, where the
            // captain can watch and steer them, when the host answers. Crew on
            // a harness that cannot be hosted run headless with a viewer space.
            let (hosted, viewed) = if wake {
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
            } else if let Some(tree) = args
                .get("id")
                .and_then(Value::as_str)
                .map(|id| review::tree_of_errand(store, id))
                .transpose()?
                .flatten()
            {
                // A review's errand reads its own read-only worktree.
                args["_workspace_cwd"] = serde_json::json!(tree);
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
            // A first mate may shorten a crew's deadline, never past the
            // charter's; absent keeps the charter's job_timeout_secs.
            if let Some(requested) = args.get("timeout_secs").filter(|v| !v.is_null()) {
                let secs = requested.as_u64().filter(|secs| *secs > 0).ok_or_else(|| {
                    anyhow::anyhow!("timeout_secs must be a positive whole number")
                })?;
                let cap = charter
                    .get("job_timeout_secs")
                    .and_then(Value::as_u64)
                    .unwrap_or(900);
                anyhow::ensure!(
                    secs <= cap,
                    "timeout_secs {secs} exceeds the charter's job_timeout_secs cap of {cap}"
                );
                job["timeout_secs"] = serde_json::json!(secs);
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
            // The crew a resume continues; only the runtime's resume sets it.
            if let Some(resumed) = args
                .get("_resume_of")
                .filter(|_| matches!(store.caller(), Caller::Operator))
            {
                job["_resume_of"] = resumed.clone();
            }
            // The crew a relaunch replaces; only the runtime's relaunch sets it.
            if let Some(relaunched) = args
                .get("_relaunch_of")
                .filter(|_| matches!(store.caller(), Caller::Operator))
            {
                job["_relaunch_of"] = relaunched.clone();
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
                    "timeout_secs": job.get("timeout_secs"),
                });
                // A bounded crew's plan: its session resumes only under it.
                if let Some(digest) = plan.get("plan_digest") {
                    job["launch"]["plan_digest"] = digest.clone();
                }
            }
            let mut started = jobs::start_agent(store, job.clone())?;
            if viewed {
                // Presentation only: a refused space never stops the crew,
                // and neither does a record missing what the space needs.
                let field = |record: &Value, name: &str| -> Result<String> {
                    record[name]
                        .as_str()
                        .map(str::to_owned)
                        .with_context(|| format!("started job record has no {name}"))
                };
                let viewer = field(&started, "id").and_then(|id| {
                    let ship = owner
                        .as_str()
                        .with_context(|| format!("task {task} has no owner"))?;
                    let cwd = field(&started, "cwd")?;
                    host::view_crew(store, &id, &task, ship, std::path::Path::new(&cwd))
                });
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

#[cfg(test)]
mod accepted_args_tests {
    use super::{ACCEPTED, accepted_args, check_args};
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::path::Path;

    /// The functions whose `match command` arms dispatch commands.
    fn dispatches(file: &Path, line: &str) -> bool {
        let line = line
            .trim_start_matches("pub(crate) ")
            .trim_start_matches("pub ");
        line.starts_with("fn dispatch(")
            || (file.ends_with("plugins.rs") && line.starts_with("fn request("))
            || (file.ends_with("session.rs") && line.starts_with("fn check("))
    }

    /// The string literals of `body` a command is matched against: arms
    /// (`"a" | "b" =>`, with or without a guard), `matches!(command, ..)`
    /// and `command == "a"`.
    fn arms(body: &str) -> Vec<String> {
        let word = |c: char| c.is_ascii_lowercase() || c == '_' || c == '.';
        let mut found = vec![];
        let mut code = String::new();
        let mut chars = body.chars().peekable();
        while let Some(c) = chars.next() {
            // A char literal such as '"' opens no string.
            if c == '\'' && chars.peek() == Some(&'"') {
                chars.next();
                code.push_str("'\"");
                continue;
            }
            if c != '"' {
                code.push(c);
                continue;
            }
            let mut literal = String::new();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        literal.push(c);
                        literal.extend(chars.next());
                    }
                    '"' => break,
                    _ => literal.push(c),
                }
            }
            let mut ahead = chars.clone();
            while ahead.next_if(|c| c.is_whitespace()).is_some() {}
            let next: String = ahead.take(3).collect();
            let prior: String = code.chars().filter(|c| !c.is_whitespace()).collect();
            let is_arm = next.starts_with("=>")
                || (next.starts_with('|') && !next.starts_with("||"))
                || next.starts_with("if ")
                || (prior.ends_with('|') && !prior.ends_with("||"))
                || prior.ends_with("command==")
                || prior.ends_with("matches!(command,");
            // Commands are `area.verb`, but for the two bare ones.
            let named = literal.contains('.') || matches!(literal.as_str(), "init" | "status");
            if is_arm && named && literal.chars().all(word) {
                found.push(literal);
            }
            code.push_str("\"\"");
        }
        found
    }

    /// Every command a dispatcher in `src` matches, read from the source.
    fn dispatched() -> BTreeSet<String> {
        fn walk(dir: &Path, out: &mut BTreeSet<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                let lines: Vec<&str> = text.lines().collect();
                let mut at = 0;
                while at < lines.len() {
                    if !dispatches(&path, lines[at]) {
                        at += 1;
                        continue;
                    }
                    let end = (at..lines.len()).find(|&i| lines[i] == "}").unwrap();
                    out.extend(arms(&lines[at..end].join("\n")));
                    at = end;
                }
            }
        }
        let mut out = BTreeSet::new();
        walk(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut out);
        out
    }

    #[test]
    fn every_dispatched_command_declares_the_arguments_it_takes() {
        let dispatched = dispatched();
        // The scan itself sees the dispatchers.
        for command in ["captain.feed", "mail.send", "agent.start", "init", "status"] {
            assert!(dispatched.contains(command), "{command}: {dispatched:?}");
        }
        let missing: Vec<&String> = dispatched
            .iter()
            .filter(|command| accepted_args(command).is_none())
            .collect();
        assert!(
            missing.is_empty(),
            "dispatched commands with no accepted-keys list: {missing:?}"
        );
        let mut declared = BTreeSet::new();
        for (command, keys) in ACCEPTED.iter().flat_map(|table| table.iter()) {
            assert!(declared.insert(*command), "{command} is declared twice");
            assert!(
                dispatched.contains(*command),
                "{command} is declared but no dispatcher matches it"
            );
            let unique: BTreeSet<&&str> = keys.iter().collect();
            assert_eq!(unique.len(), keys.len(), "{command} repeats a key");
        }
    }

    #[test]
    fn an_unknown_key_is_refused_naming_the_valid_ones() {
        let error = check_args("captain.feed", &json!({"since_ms": 1}))
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("unknown argument since_ms for captain.feed; valid: "),
            "{error}"
        );
        for key in ["after", "before", "limit", "ship", "include_ship_mail"] {
            assert!(error.contains(key), "{error}");
        }
        check_args("captain.feed", &json!({"after": 1, "limit": 5})).unwrap();
        let error = check_args("mail.inbox", &json!({"zz": 1, "aa": 2}))
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("unknown arguments aa, zz for mail.inbox"),
            "{error}"
        );
        let error = check_args("status", &json!({"verbose": true}))
            .unwrap_err()
            .to_string();
        assert_eq!(error, "unknown argument verbose for status; it takes none");
        // A newer machine's envelope key is not refused.
        check_args("mail.deliver", &json!({"id": "m", "newer_key": 1})).unwrap();
        let error = check_args("mail.nonsense", &json!({}))
            .unwrap_err()
            .to_string();
        assert_eq!(error, "unknown command mail.nonsense");
        // The runtime's own keys are taken but never named to callers.
        let error = check_args("agent.start", &json!({"nope": 1}))
            .unwrap_err()
            .to_string();
        assert!(!error.contains("_resume_of"), "{error}");
        check_args("agent.start", &json!({"_resume_of": "job"})).unwrap();
    }
}
