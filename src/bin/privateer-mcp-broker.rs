//! The MCP broker: one stdio connection to an upstream MCP server, shared by
//! many crew clients over a local socket, so the upstream only ever sees one
//! client. Each instance has a name; its profile may declare how tools bind
//! to the windows of task workspaces (see `profile.rs`).
//!
//! It is its own small binary, independent of the Privateer library, and is
//! installed at one unversioned path: Privateer upgrades change neither its
//! path nor its hash, so whatever the upstream allowed stays allowed.
//!
//! ```text
//! privateer-mcp-broker serve --state-dir DIR --name NAME [--label TEXT] [--ready-wait-secs 60]
//!     [--workspace-pool ABS]... [--path-arguments JSON] [--env-pass JSON] [--routing JSON]
//!     -- UPSTREAM...
//! privateer-mcp-broker proxy --socket ABS [--connect-wait-secs 60] [--project REL]
//! privateer-mcp-broker check-profile < PROFILE.json
//! privateer-mcp-broker --version
//! ```
//!
//! See docs/mcp-broker.md.
#[path = "privateer-mcp-broker/busy.rs"]
mod busy;
#[path = "privateer-mcp-broker/lines.rs"]
mod lines;
#[path = "privateer-mcp-broker/profile.rs"]
mod profile;
#[path = "privateer-mcp-broker/proxy.rs"]
mod proxy;
#[path = "privateer-mcp-broker/route.rs"]
mod route;
#[path = "privateer-mcp-broker/serve.rs"]
mod serve;
#[path = "privateer-mcp-broker/sys.rs"]
mod sys;
#[path = "privateer-mcp-broker/upstream.rs"]
mod upstream;
#[path = "privateer-mcp-broker/window.rs"]
mod window;

use std::path::PathBuf;
use std::time::Duration;

/// Bump on any behaviour change: `privateer mcp-broker install` replaces the
/// installed copy only when this differs.
pub const BROKER_VERSION: &str = "0.3.0";
/// The longest JSON-RPC line either side may send.
pub const MAX_LINE: usize = 16 * 1024 * 1024;

const USAGE: &str = "usage: privateer-mcp-broker serve --state-dir DIR --name NAME [--label TEXT] [--ready-wait-secs N] [--workspace-pool ABS]... [--path-arguments JSON] [--env-pass JSON] [--routing JSON] -- UPSTREAM...\n       privateer-mcp-broker proxy --socket ABS [--connect-wait-secs N] [--project REL]\n       privateer-mcp-broker check-profile < PROFILE.json\n       privateer-mcp-broker --version";

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
        Some("check-profile") if args.len() == 1 => profile::check_stdin(),
        Some("proxy") => match proxy_options(&args[1..]) {
            Ok((socket, wait, project)) => proxy::run(&socket, wait, project.as_deref()),
            Err(error) => usage(&error),
        },
        _ => usage("expected serve, proxy, check-profile or --version"),
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
    /// How busy answers and refusals name the upstream; the name by default.
    pub label: String,
    pub ready_wait: Duration,
    pub upstream: Vec<String>,
    /// Variables passed through to the upstream and the window helpers.
    pub env_pass: Vec<String>,
    /// Directories whose git checkouts are task workspaces.
    pub pools: Vec<PathBuf>,
    /// Argument names that hold paths, beyond the built-in ones.
    pub path_arguments: profile::PathArguments,
    /// How tools bind to windows; none for a plain shared broker.
    pub routing: Option<profile::Routing>,
}

/// An instance name: `^[a-z][a-z0-9-]{0,15}$`, as install takes it.
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=16).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

fn serve_options(args: &[String]) -> Result<ServeOptions, String> {
    let mut dir = None;
    let mut name = None;
    let mut ready_wait = 60;
    let mut rest = args.iter();
    let mut upstream = vec![];
    let mut pools = vec![];
    let mut label = None;
    let mut path_arguments = profile::PathArguments::default();
    let mut routing = None;
    let mut env_pass = vec![];
    let json = |text: &str, flag: &str| {
        serde_json::from_str::<serde_json::Value>(text)
            .map_err(|_| format!("{flag} takes a JSON object"))
    };
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--state-dir" => dir = Some(PathBuf::from(value(&mut rest, arg)?)),
            "--name" => name = Some(value(&mut rest, arg)?.to_string()),
            "--label" => label = Some(profile::label(value(&mut rest, arg)?)?),
            "--ready-wait-secs" => ready_wait = seconds(value(&mut rest, arg)?)?,
            "--workspace-pool" => {
                let pool = PathBuf::from(value(&mut rest, arg)?);
                if !pool.is_absolute() || pools.len() >= 16 {
                    return Err("--workspace-pool is absolute, at most 16 of them".into());
                }
                pools.push(pool);
            }
            "--path-arguments" => {
                path_arguments = profile::PathArguments::parse(&json(value(&mut rest, arg)?, arg)?)?
            }
            "--env-pass" => {
                let text = value(&mut rest, arg)?;
                let list = serde_json::from_str::<serde_json::Value>(text)
                    .map_err(|_| format!("{arg} takes a JSON array"))?;
                env_pass = profile::env_pass(&list)?;
            }
            "--routing" => {
                routing = Some(profile::Routing::parse(&json(
                    value(&mut rest, arg)?,
                    arg,
                )?)?)
            }
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
    if !valid_name(&name) {
        return Err(
            "--name starts with a lowercase letter, then at most 15 lowercase letters, digits or -"
                .into(),
        );
    }
    profile::check_argv(&upstream, "the upstream command after --")?;
    Ok(ServeOptions {
        dir,
        label: label.unwrap_or_else(|| name.clone()),
        name,
        ready_wait: Duration::from_secs(ready_wait),
        upstream,
        env_pass,
        pools,
        path_arguments,
        routing,
    })
}

type ProxyOptions = (PathBuf, Duration, Option<String>);

fn proxy_options(args: &[String]) -> Result<ProxyOptions, String> {
    let mut socket = None;
    let mut wait = 60;
    let mut project = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--socket" => socket = Some(PathBuf::from(value(&mut rest, arg)?)),
            "--connect-wait-secs" => wait = seconds(value(&mut rest, arg)?)?,
            "--project" => {
                let rel = value(&mut rest, arg)?;
                if rel.is_empty() || rel.starts_with('/') || rel.len() > 1024 {
                    return Err("--project is a relative path inside the workspace".into());
                }
                project = Some(rel.to_string());
            }
            other => return Err(format!("unexpected argument {other}")),
        }
    }
    let socket = socket.ok_or("--socket is required")?;
    if !socket.is_absolute() {
        return Err("--socket must be absolute".into());
    }
    Ok((socket, Duration::from_secs(wait), project))
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
