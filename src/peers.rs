//! A peer is an address, not an authority level. Native SSH carries the same
//! one-request JSON-lines protocol used by the local CLI.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Component, Path};
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

use crate::store::{Caller, Store};

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Peer {
    name: String,
    #[serde(default = "ssh_transport")]
    transport: String,
    #[serde(default)]
    host: Option<String>,
    executable: String,
    home: String,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
    #[serde(default = "default_response_limit")]
    max_response_bytes: usize,
}

fn ssh_transport() -> String {
    "ssh".into()
}
fn default_timeout() -> u64 {
    10_000
}
fn default_response_limit() -> usize {
    1_048_576
}

pub fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS peer_endpoints (name TEXT PRIMARY KEY, config_json TEXT NOT NULL);"
    )?;
    Ok(())
}

fn token(value: &str, maximum: usize, what: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= maximum,
        "invalid {what} length"
    );
    ensure!(
        value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)),
        "invalid {what}"
    );
    ensure!(!value.starts_with('-'), "{what} cannot start with '-'");
    Ok(())
}

fn absolute_path(value: &str, what: &str) -> Result<()> {
    ensure!(
        value.len() <= 4096 && !value.chars().any(char::is_control),
        "invalid {what}"
    );
    let path = Path::new(value);
    ensure!(path.is_absolute(), "{what} must be an absolute path");
    ensure!(
        !path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir)),
        "{what} cannot contain dot components"
    );
    ensure!(
        path.components().count() > 1,
        "{what} cannot be filesystem root"
    );
    Ok(())
}

fn validate(peer: &Peer) -> Result<()> {
    token(&peer.name, 64, "peer name")?;
    ensure!(
        matches!(peer.transport.as_str(), "ssh" | "local"),
        "unsupported peer transport"
    );
    absolute_path(&peer.executable, "peer executable")?;
    absolute_path(&peer.home, "peer home")?;
    ensure!(
        (1..=120_000).contains(&peer.timeout_ms),
        "peer timeout must be 1..120000 milliseconds"
    );
    ensure!(
        (128..=16_777_216).contains(&peer.max_response_bytes),
        "peer response limit must be 128..16777216 bytes"
    );
    if peer.transport == "ssh" {
        let host = peer.host.as_deref().context("SSH peer requires host")?;
        ensure!(host.len() <= 255, "SSH host too long");
        let parts: Vec<_> = host.split('@').collect();
        ensure!(parts.len() <= 2, "invalid SSH host");
        for part in parts {
            token(part, 255, "SSH host/user")?;
        }
    } else {
        ensure!(peer.host.is_none(), "local peer cannot specify SSH host");
    }
    Ok(())
}

/// SSH invokes a remote shell even when the local invocation is an argv array.
/// Quote every remote token; never concatenate unquoted user-provided paths.
fn posix_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn read_bounded(mut reader: impl Read, limit: usize, overflow: Arc<AtomicBool>) -> Result<Vec<u8>> {
    let mut data = Vec::with_capacity(limit.min(4096));
    let mut block = [0; 4096];
    loop {
        let n = reader.read(&mut block)?;
        if n == 0 {
            return Ok(data);
        }
        if data.len().saturating_add(n) > limit {
            overflow.store(true, Ordering::Release);
            return Ok(data);
        }
        data.extend_from_slice(&block[..n]);
    }
}

fn stop(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn call(peer: &Peer, id: &str, command: &str, args: Value, caller: &Caller) -> Result<Value> {
    token(command, 128, "RPC command")?;
    ensure!(args.is_object(), "RPC args must be an object");
    ensure!(
        !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control),
        "invalid correlation id"
    );
    let request = serde_json::to_vec(&json!({"id":id,"command":command,"args":args}))?;
    ensure!(request.len() <= 1_048_576, "peer request exceeds 1 MiB");
    // An agent stays an agent at the remote home: the role travels as CLI
    // flags, which the remote side can only use to narrow its caller.
    let mut remote_argv = vec![peer.executable.as_str(), "--home", peer.home.as_str()];
    if let Caller::Agent { actor } = caller {
        remote_argv.extend(["--role", "agent", "--actor", actor.as_str()]);
    }
    remote_argv.push("rpc");
    let mut child_command = if peer.transport == "local" {
        let mut cmd = Command::new(&peer.executable);
        // As over SSH, the identity is the flags above, never this
        // process's inherited environment.
        cmd.args(&remote_argv[1..])
            .env_remove("PRIVATEER_ROLE")
            .env_remove("PRIVATEER_ACTOR");
        cmd
    } else {
        let ssh = std::env::var_os("PRIVATEER_SSH_EXECUTABLE").unwrap_or_else(|| "ssh".into());
        let mut cmd = Command::new(ssh);
        let remote = remote_argv
            .iter()
            .map(|token| posix_quote(token))
            .collect::<Vec<_>>()
            .join(" ");
        cmd.args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "--",
            peer.host.as_deref().unwrap(),
            &remote,
        ]);
        cmd
    };
    child_command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            child_command.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = child_command.spawn().context("launch peer transport")?;
    let overflow = Arc::new(AtomicBool::new(false));
    let output_flag = overflow.clone();
    let error_flag = overflow.clone();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let limit = peer.max_response_bytes;
    let (output_tx, output_rx) = mpsc::channel();
    let (errors_tx, errors_rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = output_tx.send(read_bounded(stdout, limit, output_flag));
    });
    thread::spawn(move || {
        let _ = errors_tx.send(read_bounded(stderr, 65_536, error_flag));
    });
    // A writer thread ensures an unresponsive peer cannot block before the
    // deadline loop starts when a larger request fills its stdin pipe.
    let mut stdin = child.stdin.take().unwrap();
    let (writer_tx, writer_rx) = mpsc::channel();
    thread::spawn(move || {
        let result = (|| -> std::io::Result<()> {
            stdin.write_all(&request)?;
            stdin.write_all(b"\n")?;
            Ok(())
        })();
        drop(stdin);
        let _ = writer_tx.send(result);
    });
    let started = Instant::now();
    let mut output = None;
    let mut errors = None;
    let mut write_result = None;
    let status = loop {
        if overflow.load(Ordering::Acquire) {
            stop(&mut child);
            bail!("peer output exceeded configured byte limit");
        }
        if started.elapsed() >= Duration::from_millis(peer.timeout_ms) {
            stop(&mut child);
            bail!("peer call timed out after {} milliseconds", peer.timeout_ms);
        }
        // Keep the child unreaped while a descendant may hold inherited pipes.
        // Its PID then cannot be reused before timeout cleanup of the group.
        if output.is_none() {
            output = output_rx.try_recv().ok();
        }
        if errors.is_none() {
            errors = errors_rx.try_recv().ok();
        }
        if write_result.is_none() {
            write_result = writer_rx.try_recv().ok();
        }
        if output.is_some() && errors.is_some() && write_result.is_some() {
            if let Some(status) = child.try_wait()? {
                break status;
            }
        }
        thread::sleep(Duration::from_millis(5));
    };
    write_result.context("peer request writer failed")??;
    let output = output.context("peer response reader failed")??;
    let errors = errors.context("peer diagnostic reader failed")??;
    ensure!(
        !overflow.load(Ordering::Acquire),
        "peer output exceeded configured byte limit"
    );
    ensure!(
        status.success(),
        "peer transport exited with {status}: {}",
        String::from_utf8_lossy(&errors).trim()
    );
    let lines: Vec<&[u8]> = output
        .split(|c| *c == b'\n')
        .filter(|line| !line.is_empty())
        .collect();
    ensure!(
        lines.len() == 1,
        "peer must return exactly one JSON response"
    );
    let response: Value = serde_json::from_slice(lines[0]).context("invalid peer JSON response")?;
    ensure!(
        response.get("id").and_then(Value::as_str) == Some(id),
        "peer correlation id mismatch"
    );
    match response.get("ok").and_then(Value::as_bool) {
        Some(true) => ensure!(
            response.get("result").is_some(),
            "peer success response missing result"
        ),
        Some(false) => ensure!(
            response.get("error").and_then(Value::as_str).is_some(),
            "peer error response missing error"
        ),
        None => bail!("peer response missing boolean ok"),
    }
    Ok(response)
}

/// Send one correlated RPC to a configured peer as the current caller and
/// return its envelope (a remote error is an `ok:false` envelope, not Err).
pub(crate) fn call_peer(
    store: &Store,
    name: &str,
    id: &str,
    command: &str,
    args: Value,
) -> Result<Value> {
    init(store)?;
    token(name, 64, "peer name")?;
    let config: String = store
        .connection()
        .query_row(
            "SELECT config_json FROM peer_endpoints WHERE name=?1",
            [name],
            |row| row.get(0),
        )
        .context("peer not configured")?;
    let peer: Peer = serde_json::from_str(&config)?;
    validate(&peer)?;
    let response = call(&peer, id, command, args, store.caller())?;
    store.audit(
        "peer.called",
        &json!({"name":name,"command":command,"correlation_id":id,"ok":response["ok"]}),
    )?;
    Ok(response)
}

/// Whether a peer with this name is configured.
pub(crate) fn configured(store: &Store, name: &str) -> Result<bool> {
    init(store)?;
    Ok(store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM peer_endpoints WHERE name=?1)",
        [name],
        |r| r.get(0),
    )?)
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    init(store)?;
    match command {
        "peer.put" => {
            let peer: Peer = serde_json::from_value(args)?;
            validate(&peer)?;
            store.connection().execute("INSERT INTO peer_endpoints(name,config_json) VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET config_json=excluded.config_json", rusqlite::params![peer.name, serde_json::to_string(&peer)?])?;
            store.audit(
                "peer.configured",
                &json!({"name":peer.name,"transport":peer.transport}),
            )?;
            Ok(serde_json::to_value(peer)?)
        }
        "peer.list" => {
            let mut statement = store
                .connection()
                .prepare("SELECT config_json FROM peer_endpoints ORDER BY name")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            let peers: Vec<Value> = rows
                .map(|row| -> Result<Value> { Ok(serde_json::from_str(&row?)?) })
                .collect::<Result<_>>()?;
            Ok(json!({"peers":peers}))
        }
        "peer.call" => {
            let name = args
                .get("name")
                .and_then(Value::as_str)
                .context("peer.call requires name")?;
            let command = args
                .get("command")
                .and_then(Value::as_str)
                .context("peer.call requires command")?;
            let id = args
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            call_peer(
                store,
                name,
                &id,
                command,
                args.get("args").cloned().unwrap_or_else(|| json!({})),
            )
        }
        _ => bail!("unknown peer command: {command}"),
    }
}
