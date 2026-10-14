use anyhow::{Context, Result, bail};
use privateer::{
    dispatch, rpc,
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
    let mut role = None;
    let mut actor = None;
    while let Some(token) = tokens.next() {
        match token.as_str() {
            "--role" => role = Some(tokens.next().context("--role requires a role")?),
            "--actor" => actor = Some(tokens.next().context("--actor requires an identity")?),
            "--home" => {
                home = Some(PathBuf::from(
                    tokens.next().context("--home requires a path")?,
                ))
            }
            "--json" => {
                args = serde_json::from_str(&tokens.next().context("--json requires JSON")?)?
            }
            "--version" => {
                println!("privateer {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                println!(
                    "Privateer — autonomous peers under a charter\n\nprivateer --home /absolute/path COMMAND --json '{{...}}'\nprivateer --home /absolute/path rpc\n\nCommands: init, status, charter.put/show/revoke, task.create/list/show/update/handoff/accept,\nevent.emit/pending/history/ack, message.send/inbox, job.start/status/list/cancel/retry,\nresource.status/recover, peer.put/list/call, plugin.validate/command, agent.brief/start,\ndelivery.check, approval.grant/consume, audit.list, history.prune\n\nRPC accepts one JSON object per line: {{\"id\":\"1\",\"command\":\"status\",\"args\":{{}}}}\n\nAgents run with PRIVATEER_ROLE=agent and PRIVATEER_ACTOR (or --role agent --actor NAME):\noperator commands are refused and actor names are bound to that identity.\nLocal homes contain private state; credentials stay with your chosen harness."
                );
                return Ok(());
            }
            _ if command.is_none() => command = Some(token),
            _ if command.as_deref() == Some("_job-supervise") && internal_job.is_none() => {
                internal_job = Some(token)
            }
            _ => bail!("unexpected argument {token}"),
        }
    }
    let home = home
        .or_else(|| std::env::var_os("PRIVATEER_HOME").map(PathBuf::from))
        .context("--home or PRIVATEER_HOME is required")?;
    let command = command.context("command is required; use --help")?;
    let mut store = Store::open(&home)?;
    let env = |name: &str| -> Result<Option<String>> {
        match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => bail!("{name} must be valid UTF-8"),
        }
    };
    store.set_caller(Caller::resolve(
        env("PRIVATEER_ROLE")?.as_deref(),
        env("PRIVATEER_ACTOR")?.as_deref(),
        role.as_deref(),
        actor.as_deref(),
    )?);
    if command == "_job-supervise" {
        return privateer::jobs::supervise(
            &mut store,
            &internal_job.context("job ID is required")?,
        );
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
    if let Err(error) = run() {
        eprintln!("{}", json!({"ok":false,"error":error.to_string()}));
        std::process::exit(1);
    }
}
