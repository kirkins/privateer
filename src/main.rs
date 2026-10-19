use anyhow::{Context, Result, bail, ensure};
use privateer::{
    dispatch, input, rpc,
    store::{Caller, Store},
};
use serde_json::{Value, json};
use std::io::{self, BufRead, Read, Write};
use std::path::PathBuf;

fn run() -> Result<()> {
    let mut tokens = std::env::args().skip(1);
    let mut home = None;
    let mut command = None;
    let mut args = json!({});
    let mut internal_job = None;
    let mut positional: Vec<String> = vec![];
    let mut json_given = false;
    let mut file_given = false;
    let mut yes = false;
    let mut role = None;
    let mut actor = None;
    while let Some(token) = tokens.next() {
        if token == "--" && command.as_deref() == Some("start") {
            positional.push(token);
            positional.extend(tokens);
            break;
        }
        match token.as_str() {
            "--yes" | "-y" => yes = true,
            "--role" => {
                let value = tokens.next().context("--role requires a role")?;
                ensure!(
                    role.as_ref().is_none_or(|r| r == &value),
                    "conflicting role flags"
                );
                role = Some(value);
            }
            "--actor" => {
                let value = tokens.next().context("--actor requires an identity")?;
                ensure!(
                    actor.as_ref().is_none_or(|a| a == &value),
                    "conflicting actor flags"
                );
                actor = Some(value);
            }
            "--fleet" => privateer::fleet::use_registry(PathBuf::from(
                tokens.next().context("--fleet requires a registry path")?,
            ))?,
            "--home" => {
                home = Some(PathBuf::from(
                    tokens.next().context("--home requires a path")?,
                ))
            }
            "--json" => {
                ensure!(!file_given, "choose --json or --file, not both");
                json_given = true;
                args = serde_json::from_str(&tokens.next().context("--json requires JSON")?)?
            }
            "--file" => {
                ensure!(!file_given, "--file cannot be repeated");
                ensure!(!json_given, "choose --json or --file, not both");
                file_given = true;
                let path = PathBuf::from(tokens.next().context("--file requires a path")?);
                args = input::read_file(&path)?;
            }
            "--version" => {
                println!("privateer {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                println!(
                    "Privateer — autonomous peers under a charter\n\nprivateer --home /absolute/path COMMAND --json '{{...}}'\nprivateer --home /absolute/path COMMAND --file args.yaml   YAML/YML/JSON object (64 KiB)\nprivateer --home /absolute/path rpc\nprivateer --home /absolute/path serve [--json '{{\"once\":true}}']   schedule prepared work and wake first mates for mail\n\nprivateer          open the dashboard when installed (ordinary terminals or Herdr)\nprivateer start    talk to the current ship's first mate directly\nIf the dashboard is missing: get aboard the current ship, offering to initialize a new folder first (or --yes).\nIn a ship folder, --home is found automatically:\nprivateer init [--ship NAME] [--harness ID] [--liaison]\nprivateer start | status | ships | ask SHIP TEXT | tell SHIP TEXT | inbox | reply ID TEXT | dismiss ID [REASON] | wakes | stow [status|resume] [--machine NAME | --all] | afk [WORDS] | back | backlog [TASK RANK] |\n          project add URL [NAME] [--ship SHIP] | service [status|start|stop]\nprivateer upgrade status   read-only release identity, hosted pane drift and upgrade history (no upgrade execution yet)\nprivateer host relaunch SHIP [--force]   move a hosted first mate onto the current release at its next safe point (refuses a running turn unless forced)\nprivateer permissions [show|standard|yolo|allow TOOL_RULE]   fleet-wide launch permissions\nprivateer routes [show|set FILE.json]   the captain's crew and errand routes, in the order ships try them\nprivateer door add NAME [--config PATH] [--to SHIP] | start | stop | status | list | remove   the captain's front doors (e.g. telegram)\nprivateer machine add NAME SSH_TARGET --executable PATH | list | refresh | collect | remove NAME   link another machine's fleet\n\nCommands: init, status, charter.put/show/revoke, task.create/list/show/update/handoff/accept/rank/backlog/enqueue/defer,\nevent.emit/pending/history/ack, ship.put/list/show/bearings, ship.create/actions/scope/retire, project.add, wake.list/transcript, work.status/poll/started/settled/release,\nnote.put/get/list/pin, focus.digest, upgrade.status, permissions.show/set/blocked, routes.show/set,\nmail.send/inbox/outbox/pending/ack/show/requests/flush/deliver/withdraw, runtime.needs, message.send/inbox, job.start/status/list/cancel/retry,\nworkspace.acquire/status/list/recover/release, crew.report/answer, resource.status/configure/recover, peer.put/list/call, plugin.validate/command, agent.brief/start,\ndelivery.check, approval.grant/deny/consume, audit.list, activity.list, history.prune, stow.start/status/resume, captain.waiting/relay/reply/dismiss/feed, report.put/get\n\nRPC accepts one JSON object per line: {{\"id\":\"1\",\"command\":\"status\",\"args\":{{}}}}\n\nAgents run with PRIVATEER_ROLE=agent and PRIVATEER_ACTOR (or --role agent --actor NAME):\noperator commands are refused and actor names are bound to that identity.\nLocal homes contain private state; credentials stay with your chosen harness."
                );
                return Ok(());
            }
            _ if command.is_none() => command = Some(token),
            _ if command.as_deref() == Some("_job-supervise") && internal_job.is_none() => {
                internal_job = Some(token)
            }
            // Human commands take positional arguments and flags of their own.
            _ if command
                .as_deref()
                .is_some_and(|c| privateer::cli::VERBS.contains(&c)) =>
            {
                positional.push(token)
            }
            _ => bail!("unexpected argument {token}"),
        }
    }
    ensure!(
        !file_given || command.is_some(),
        "--file requires a command"
    );
    ensure!(
        !file_given || !matches!(command.as_deref(), Some("rpc" | "hook" | "_job-supervise")),
        "--file supplies command arguments; RPC, hooks and internal supervisors use their own input"
    );
    // The ship home: --home, PRIVATEER_HOME, or the nearest .privateer above
    // the current directory.
    let home = home
        .or_else(|| std::env::var_os("PRIVATEER_HOME").map(PathBuf::from))
        .or_else(privateer::cli::discover_home);
    let supervisor = command.as_deref() == Some("_job-supervise");
    let caller = Caller::from_env_at(
        home.as_deref(),
        role.as_deref(),
        actor.as_deref(),
        supervisor,
    );
    // Harness hooks must never fail the harness: they exit with the code
    // the harness expects and log their own errors.
    if command.as_deref() == Some("hook") {
        let event = positional.get(1).map(String::as_str).unwrap_or("");
        let code = match positional.first().map(String::as_str) {
            Some("claude") => privateer::presence::claude_hook(home.as_deref(), event, caller),
            Some("cursor") => privateer::cursor_mail::hook(home.as_deref(), event, caller),
            _ => bail!("usage: privateer hook claude|cursor EVENT"),
        };
        std::process::exit(code);
    }
    let caller = caller?;
    if command.as_deref() == Some("permissions") && !json_given && !file_given {
        return privateer::permissions::cli(&positional, &caller);
    }
    if command.as_deref() == Some("routes") && !json_given && !file_given {
        return privateer::crew_routes::cli(&positional, &caller);
    }
    // A folder a ship moved away from runs nothing.
    if let Some(home) = &home {
        if command.as_deref() != Some("gateway") {
            privateer::moves::refuse_moved(home)?;
        }
    }
    let Some(command) = command else {
        return privateer::cli::board(home, yes, caller);
    };
    // Release status belongs to the installation, not to a ship home.
    if command == "upgrade" && !json_given && !file_given {
        if positional == ["status"] {
            println!(
                "{}",
                serde_json::to_string_pretty(&upgrade_status(home.as_deref(), &caller)?)?
            );
            return Ok(());
        }
        return privateer::releases::cli(&positional);
    }
    if command == "upgrade.status" {
        println!("{}", upgrade_status(home.as_deref(), &caller)?);
        return Ok(());
    }
    // A hosted first mate's pane is relaunched onto the current release by its
    // home's dispatcher or by the operator, never by an ordinary command.
    if command == "host" && !json_given && !file_given {
        return privateer::host::upgrade::cli(home, &positional, &caller);
    }
    // Being away is the captain's, for the whole fleet, from any folder.
    if matches!(command.as_str(), "afk" | "back" | "away") && !json_given && !file_given {
        caller.require_operator(&command)?;
        return privateer::away::cli(&command, &positional);
    }
    // A machine's gateway and its links belong to the fleet, not to a home.
    if command == "gateway" {
        return privateer::machines::gateway(caller);
    }
    if command == "machine" && !json_given && !file_given {
        caller.require_operator("machine")?;
        return privateer::machines::cli(&positional);
    }
    if command == "grant" && !json_given && !file_given {
        caller.require_operator("grant")?;
        return privateer::grants::cli(&positional);
    }
    // Front doors belong to the captain's whole fleet, not to one ship home.
    if command == "door" && !json_given && !file_given {
        caller.require_operator("door")?;
        return privateer::door::cli(&positional);
    }
    // Credentials belong to the captain's fleet, not to a home; a secret is
    // read from stdin alone, never from argv or JSON arguments.
    if command.starts_with("credential.") {
        caller.require_operator(&command)?;
        ensure!(
            positional.is_empty(),
            "a credential is read from stdin, never argv"
        );
        let result = privateer::credentials::cli_from_stdin(&command, &args)?;
        println!("{}", serde_json::to_string(&result)?);
        return Ok(());
    }
    if !json_given && !file_given && privateer::cli::VERBS.contains(&command.as_str()) {
        return privateer::cli::run(home, &command, &positional, caller);
    }
    ensure!(
        positional.is_empty(),
        "unexpected argument {}",
        positional.first().map(String::as_str).unwrap_or("")
    );
    if privateer::operator_only(&caller, &command) || command == "serve" {
        caller.require_operator(&command)?;
    }
    let home = home.context(
        "not inside a ship folder (no .privateer here or above); run `privateer init`, or pass --home",
    )?;
    let mut store = if command == "runtime.needs" {
        Store::open_readonly(&home, caller)?
    } else {
        Store::open_as(&home, caller)?
    };
    if command == "_job-supervise" {
        store.as_runtime();
        return privateer::jobs::supervise(
            &mut store,
            &internal_job.context("job ID is required")?,
        );
    }
    if command == "serve" {
        return privateer::activation::serve(&mut store, &args);
    }
    if command == "rpc" {
        let stdin = io::stdin();
        let mut input = stdin.lock();
        let mut out = io::stdout().lock();
        loop {
            // read_until via a capped Take prevents unbounded allocation by a malformed peer.
            let mut line = Vec::new();
            let read = input
                .by_ref()
                .take(1024 * 1024 + 1)
                .read_until(b'\n', &mut line)?;
            if read == 0 {
                break;
            }
            if line.len() > 1024 * 1024 {
                bail!("RPC line exceeds 1 MiB");
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let response = match serde_json::from_slice::<Value>(&line) {
                Ok(request) => rpc(&mut store, request),
                Err(_) => json!({"id":null,"ok":false,"error":"invalid JSON request"}),
            };
            serde_json::to_writer(&mut out, &response)?;
            out.write_all(b"\n")?;
            out.flush()?;
        }
        return Ok(());
    }
    let result = dispatch(&mut store, &command, args)?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

fn main() {
    // Pin this image before an upgrade can move `current`.
    let _ = privateer::releases::running_image();
    if let Err(error) = run() {
        eprintln!("{}", json!({"ok":false,"error":error.to_string()}));
        std::process::exit(1);
    }
}

/// Release identity and history, plus each hosted pane's release when the
/// operator's ship home is at hand. Pane drift is best effort: a home from an
/// older release, or a dangling `current`, must not hide the release status.
fn upgrade_status(home: Option<&std::path::Path>, caller: &Caller) -> Result<Value> {
    let mut status = privateer::releases::status()?;
    if let (Some(home), Caller::Operator) = (home, caller) {
        match Store::open_readonly(home, caller.clone())
            .and_then(|store| privateer::host::upgrade::panes(&store))
        {
            Ok(panes) => status["hosted_panes"] = panes,
            Err(error) => status["hosted_panes_error"] = json!(format!("{error:#}")),
        }
    }
    Ok(status)
}
