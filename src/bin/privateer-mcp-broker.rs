//! The MCP broker: one stdio connection to an upstream MCP server (on a Mac,
//! `xcrun mcpbridge`), shared by many crew clients over a local socket, so
//! the upstream only ever sees one client.
//!
//! It is its own small binary, independent of the Privateer library, and is
//! installed at one unversioned path: Privateer upgrades change neither its
//! path nor its hash, so whatever the upstream allowed stays allowed.
//!
//! ```text
//! privateer-mcp-broker serve --state-dir DIR --name NAME [--ready-wait-secs 60] -- UPSTREAM...
//! privateer-mcp-broker proxy --socket ABS [--connect-wait-secs 60]
//! privateer-mcp-broker --version
//! ```
//!
//! See docs/mcp-broker.md.
#[path = "privateer-mcp-broker/busy.rs"]
mod busy;
#[path = "privateer-mcp-broker/lines.rs"]
mod lines;
#[path = "privateer-mcp-broker/proxy.rs"]
mod proxy;
#[path = "privateer-mcp-broker/serve.rs"]
mod serve;
#[path = "privateer-mcp-broker/sys.rs"]
mod sys;
#[path = "privateer-mcp-broker/upstream.rs"]
mod upstream;

use std::path::PathBuf;
use std::time::Duration;

/// Bump on any behaviour change: `privateer mcp-broker install` replaces the
/// installed copy only when this differs.
pub const BROKER_VERSION: &str = "0.1.0";
/// The longest JSON-RPC line either side may send.
pub const MAX_LINE: usize = 16 * 1024 * 1024;

const USAGE: &str = "usage: privateer-mcp-broker serve --state-dir DIR --name NAME [--ready-wait-secs N] -- UPSTREAM...\n       privateer-mcp-broker proxy --socket ABS [--connect-wait-secs N]\n       privateer-mcp-broker --version";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("--version") if args.len() == 1 => {
            println!("privateer-mcp-broker {BROKER_VERSION}");
            0
        }
        Some("serve") => match serve_options(&args[1..]) {
            Ok(options) => serve::run(options),
            Err(error) => usage(&error),
        },
        Some("proxy") => match proxy_options(&args[1..]) {
            Ok((socket, wait)) => proxy::run(&socket, wait),
            Err(error) => usage(&error),
        },
        _ => usage("expected serve, proxy or --version"),
    };
    std::process::exit(code);
}

fn usage(error: &str) -> i32 {
    eprintln!("privateer-mcp-broker: {error}\n{USAGE}");
    2
}

pub struct ServeOptions {
    pub dir: PathBuf,
    pub name: String,
    pub ready_wait: Duration,
    pub upstream: Vec<String>,
}

fn serve_options(args: &[String]) -> Result<ServeOptions, String> {
    let mut dir = None;
    let mut name = None;
    let mut ready_wait = 60;
    let mut rest = args.iter();
    let mut upstream = vec![];
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--state-dir" => dir = Some(PathBuf::from(value(&mut rest, arg)?)),
            "--name" => name = Some(value(&mut rest, arg)?.to_string()),
            "--ready-wait-secs" => ready_wait = seconds(value(&mut rest, arg)?)?,
            "--" => {
                upstream = rest.by_ref().cloned().collect();
                break;
            }
            other => return Err(format!("unexpected argument {other}")),
        }
    }
    let dir = dir.ok_or("--state-dir is required")?;
    if !dir.is_absolute() {
        return Err("--state-dir must be absolute".into());
    }
    let name = name.ok_or("--name is required")?;
    let valid = !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !valid {
        return Err("--name is a lowercase slug of at most 32 characters".into());
    }
    if upstream.is_empty() || !upstream[0].starts_with('/') {
        return Err("the upstream command after -- must start with an absolute path".into());
    }
    Ok(ServeOptions {
        dir,
        name,
        ready_wait: Duration::from_secs(ready_wait),
        upstream,
    })
}

fn proxy_options(args: &[String]) -> Result<(PathBuf, Duration), String> {
    let mut socket = None;
    let mut wait = 60;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--socket" => socket = Some(PathBuf::from(value(&mut rest, arg)?)),
            "--connect-wait-secs" => wait = seconds(value(&mut rest, arg)?)?,
            other => return Err(format!("unexpected argument {other}")),
        }
    }
    let socket = socket.ok_or("--socket is required")?;
    if !socket.is_absolute() {
        return Err("--socket must be absolute".into());
    }
    Ok((socket, Duration::from_secs(wait)))
}

fn value<'a>(rest: &mut std::slice::Iter<'a, String>, flag: &str) -> Result<&'a str, String> {
    rest.next()
        .map(String::as_str)
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn seconds(text: &str) -> Result<u64, String> {
    text.parse::<u64>()
        .ok()
        .filter(|n| *n <= 86_400)
        .ok_or_else(|| format!("{text} is not a number of seconds up to 86400"))
}
