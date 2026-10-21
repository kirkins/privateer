//! The one upstream connection: start it, initialize it once for everyone,
//! route what it says, and restart it with backoff when it exits.
use crate::lines::{self, Line};
use crate::serve::{Broker, Phase, exited, filter_windows};
use crate::{BROKER_VERSION, MAX_LINE, route, sys};
use serde_json::{Value, json};
use std::io::{BufReader, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

/// The MCP revision the broker asks for; the upstream's answer is what every
/// client receives.
const PROTOCOL_VERSION: &str = "2025-06-18";
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Ready this long, an exit restarts from the first backoff again.
const STABLE: Duration = Duration::from_secs(60);
/// Lines queued for the upstream's stdin.
const UPSTREAM_QUEUE: usize = 64;

pub fn supervise(broker: Arc<Broker>) {
    let mut backoff = FIRST_BACKOFF;
    loop {
        let generation = {
            let mut state = broker.lock();
            if state.phase == Phase::Stopping {
                break;
            }
            state.generation += 1;
            state.phase = Phase::Starting;
            state.generation
        };
        broker.publish();
        let ready_at = run(&broker, generation, backoff);
        if ready_at.is_some_and(|at| at.elapsed() >= STABLE) {
            backoff = FIRST_BACKOFF;
        }
        let until = Instant::now() + backoff;
        {
            let mut state = broker.lock();
            if state.phase == Phase::Stopping {
                break;
            }
            state.phase = Phase::Backoff(until);
        }
        broker.publish();
        let mut state = broker.lock();
        while state.phase != Phase::Stopping && Instant::now() < until {
            state = broker
                .changed
                .wait_timeout(state, until.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        drop(state);
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
    broker.lock().supervising = false;
    broker.changed.notify_all();
    broker.publish();
}

/// Only `PATH=/usr/bin:/bin`, `HOME`, `LANG`, `LC_*`, `TMPDIR` and
/// `DEVELOPER_DIR`: what the upstream and the window helpers see.
pub fn minimal_env(command: &mut Command) {
    command.env_clear().env("PATH", "/usr/bin:/bin");
    for (name, value) in std::env::vars_os() {
        let text = name.to_string_lossy();
        if matches!(&*text, "HOME" | "LANG" | "TMPDIR" | "DEVELOPER_DIR") || text.starts_with("LC_")
        {
            command.env(&name, value);
        }
    }
}

fn spawn(argv: &[String]) -> std::io::Result<Child> {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        // Not the broker's state directory, nor wherever it was started.
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own group, so stopping it reaches whatever it starts.
        .process_group(0);
    minimal_env(&mut command);
    // SAFETY: signal and prctl are async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            // The broker ignores SIGHUP; the upstream gets the default back.
            libc::signal(libc::SIGHUP, libc::SIG_DFL);
            // The supervisor thread that spawns the upstream lives as long as
            // the broker, so a broker killed outright takes its upstream too.
            #[cfg(target_os = "linux")]
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    command.spawn()
}

/// One upstream lifetime. Returns when it became ready, if it did.
fn run(broker: &Arc<Broker>, generation: u64, backoff: Duration) -> Option<Instant> {
    let mut child = match spawn(&broker.upstream_argv) {
        Ok(child) => child,
        Err(error) => {
            broker
                .log
                .line(&format!("upstream could not start: {}", error.kind()));
            let mut state = broker.lock();
            state.last_exit = Some(format!("could not start: {}", error.kind()));
            state.restarts += 1;
            return None;
        }
    };
    let pid = child.id();
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return None;
    };
    let (queue, lines_in) = sync_channel::<Vec<u8>>(UPSTREAM_QUEUE);
    std::thread::spawn(move || {
        let mut stdin = stdin;
        for line in lines_in {
            if stdin.write_all(&line).and_then(|_| stdin.flush()).is_err() {
                break;
            }
        }
    });
    {
        let broker = broker.clone();
        std::thread::spawn(move || {
            // The upstream's own diagnostics, a bounded prefix of each line.
            let mut reader = BufReader::new(stderr);
            while let Ok(Some(line)) = lines::next(&mut reader, 64 * 1024) {
                if let Line::Text(text) = line {
                    let text = String::from_utf8_lossy(&text);
                    broker
                        .log
                        .line(&format!("upstream stderr: {}", sys::quoted(&text, 200)));
                }
            }
        });
    }
    let initialize = {
        let mut state = broker.lock();
        state.upstream_pid = Some(pid);
        state.upstream = Some(queue.clone());
        let id = state.next_id();
        state.init_id = Some(id);
        json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "privateer-mcp-broker", "version": BROKER_VERSION},
        }})
    };
    broker.log.line(&format!(
        "upstream started: pid {pid}, generation {generation}"
    ));
    broker.publish();
    let _ = queue.send(line(&initialize));
    drop(queue);
    let mut ready_at = None;
    let mut reader = BufReader::with_capacity(64 * 1024, stdout);
    while let Ok(Some(next)) = lines::next(&mut reader, MAX_LINE) {
        match next {
            Line::Text(text) => route(broker, pid, generation, &text, &mut ready_at),
            Line::TooLong => broker.log.line("dropped an oversize upstream line"),
        }
    }
    // Its stdout closed: whatever it was doing is over. Every request it held
    // is answered now, never retried.
    {
        let mut state = broker.lock();
        state.upstream = None;
        state.init_id = None;
        if state.phase != Phase::Stopping {
            state.phase = Phase::Backoff(Instant::now() + backoff);
        }
        let lost: Vec<u64> = state
            .pending
            .iter()
            .filter(|(_, p)| p.generation == generation)
            .map(|(id, _)| *id)
            .collect();
        for id in lost {
            if let Some(pending) = state.pending.remove(&id) {
                if let Some(token) = &pending.token {
                    state.progress.remove(token);
                }
                broker.deliver(&state, pending.client, &exited(&pending));
            }
        }
        // The broker's own requests: their waiters see the upstream gone.
        state.internal.retain(|_, i| i.generation != generation);
        broker.changed.notify_all();
    }
    let status = reap(&mut child, pid);
    broker.log.line(&format!("upstream exited: {status}"));
    {
        let mut state = broker.lock();
        state.upstream_pid = None;
        state.last_exit = Some(status);
        state.restarts += 1;
        broker.changed.notify_all();
    }
    broker.publish();
    ready_at
}

/// Wait for the upstream after its stdout closed: SIGTERM to its group, up to
/// 2 s, then SIGKILL; and SIGKILL to anything left in its group.
fn reap(child: &mut Child, pid: u32) -> String {
    sys::signal_group(pid, libc::SIGTERM);
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => break None,
        }
    };
    let status = match status {
        Some(status) => {
            sys::signal_group(pid, libc::SIGKILL);
            status
        }
        None => {
            sys::signal_group(pid, libc::SIGKILL);
            match child.wait() {
                Ok(status) => status,
                Err(error) => return format!("lost: {}", error.kind()),
            }
        }
    };
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit {code}"),
        (_, Some(signal)) => format!("signal {signal}"),
        _ => "unknown".into(),
    }
}

fn line(message: &Value) -> Vec<u8> {
    let mut line = serde_json::to_vec(message).unwrap_or_default();
    line.push(b'\n');
    line
}

/// One message from the upstream.
fn route(
    broker: &Arc<Broker>,
    pid: u32,
    generation: u64,
    text: &[u8],
    ready_at: &mut Option<Instant>,
) {
    let Ok(Value::Object(mut message)) = serde_json::from_slice::<Value>(text) else {
        broker
            .log
            .line("dropped an upstream line that is not a JSON-RPC object");
        return;
    };
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut state = broker.lock();
    match (method, message.get("id").cloned()) {
        // A server-to-client request: no client may answer for the others.
        (Some(method), Some(id)) => {
            broker.log.line(&format!(
                "refused upstream request {}",
                sys::quoted(&method, 64)
            ));
            if let Some(queue) = &state.upstream {
                let _ = queue.try_send(line(&json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": "Method not found"}})));
            }
        }
        (Some(method), None) => match method.as_str() {
            "notifications/progress" => {
                let token = message
                    .get("params")
                    .and_then(|p| p.get("progressToken"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if let Some((owner, theirs)) = state.progress.get(token).cloned() {
                    message["params"]["progressToken"] = theirs;
                    broker.deliver(&state, owner, &Value::Object(message));
                }
            }
            // Cancels a server request the broker already refused.
            "notifications/cancelled" => {}
            "notifications/tools/list_changed" => {
                broker.broadcast(&state, &Value::Object(message));
                state.tools = None;
                state.tools_epoch += 1;
                drop(state);
                broker.log.line("upstream tools changed");
                broker.refresh_tools();
            }
            _ => broker.broadcast(&state, &Value::Object(message)),
        },
        (None, Some(id)) => {
            let Some(id) = id.as_u64() else {
                return;
            };
            if state.init_id == Some(id) {
                state.init_id = None;
                match message.get("result").filter(|r| r.is_object()) {
                    Some(result) => {
                        state.init = Some(result.clone());
                        // Queued before any request: the upstream hears it first.
                        if let Some(queue) = &state.upstream {
                            let _ = queue.try_send(line(&json!({"jsonrpc": "2.0",
                                "method": "notifications/initialized", "params": {}})));
                        }
                        // A stop that began meanwhile stands.
                        if state.phase == Phase::Starting {
                            state.phase = Phase::Ready;
                        }
                        // A new upstream may offer other tools.
                        state.tools = None;
                        state.tools_epoch += 1;
                        *ready_at = Some(Instant::now());
                        broker.changed.notify_all();
                        drop(state);
                        broker.log.line("upstream ready");
                        broker.publish();
                        broker.refresh_tools();
                    }
                    None => {
                        broker.log.line("upstream refused initialize");
                        sys::signal_group(pid, libc::SIGTERM);
                    }
                }
            } else if let Some(pending) = state.pending.remove(&id) {
                if let Some(token) = &pending.token {
                    state.progress.remove(token);
                }
                message.insert("id".into(), pending.id.clone());
                // A client's window list: seen whole, delivered for its root.
                let mut record = false;
                if let (Some(root), Some(result)) = (&pending.windows, message.get_mut("result")) {
                    // Resolved with the state unlocked: a realpath can be slow.
                    drop(state);
                    let resolved = route::resolved_paths(result);
                    state = broker.lock();
                    record = broker.observe(&mut state, result, &resolved);
                    filter_windows(result, root.as_deref(), &resolved);
                }
                broker.deliver(&state, pending.client, &Value::Object(message));
                drop(state);
                if record {
                    broker.save_windows();
                }
            } else if let Some(internal) = state.internal.remove(&id) {
                if internal.generation == generation {
                    let _ = internal.reply.try_send(Value::Object(message));
                }
            }
        }
        _ => broker
            .log
            .line("dropped an upstream message with no method or id"),
    }
}
