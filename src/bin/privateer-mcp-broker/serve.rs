//! `serve`: one broker per state directory and name. It holds the lock, the
//! socket and the upstream, and maps every client's ids onto broker-unique
//! ones so their answers never cross.
use crate::lines::{self, Line};
use crate::{BROKER_VERSION, MAX_LINE, ServeOptions, busy, sys, upstream};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Lines queued for one client before it counts as stuck and is dropped.
const CLIENT_QUEUE: usize = 256;
/// The longest socket path macOS accepts (sun_path is 104 bytes there).
const MAX_SOCKET_PATH: usize = 103;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Spawning the upstream, or waiting for its InitializeResult.
    Starting,
    Ready,
    /// The upstream exited; it restarts at this instant.
    Backoff(Instant),
    Stopping,
}

/// A client request forwarded upstream under a broker id.
pub struct Pending {
    pub client: u64,
    pub id: Value,
    pub method: String,
    pub generation: u64,
    pub token: Option<String>,
}

struct Client {
    queue: SyncSender<Vec<u8>>,
    stream: UnixStream,
    /// The pid the hello claims, and the one the socket reports (none on
    /// macOS, whose getpeereid has no pid).
    pid: Option<i64>,
    peer_pid: Option<i64>,
    cwd: Option<String>,
}

pub struct State {
    pub phase: Phase,
    /// Bumped at each upstream start; pending requests carry theirs.
    pub generation: u64,
    pub init: Option<Value>,
    pub init_id: Option<u64>,
    /// The current upstream's stdin queue, while it runs.
    pub upstream: Option<SyncSender<Vec<u8>>>,
    pub upstream_pid: Option<u32>,
    pub restarts: u64,
    pub last_exit: Option<String>,
    pub supervising: bool,
    pub pending: HashMap<u64, Pending>,
    /// Broker progress token -> (client, the client's own token).
    pub progress: HashMap<String, (u64, Value)>,
    next_id: u64,
    clients: HashMap<u64, Client>,
    next_client: u64,
    /// Client writer threads still running; shutdown waits for them briefly.
    writers: usize,
}

impl State {
    pub fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}

pub struct Broker {
    pub name: String,
    pub label: String,
    pub dir: PathBuf,
    pub socket: PathBuf,
    pub ready_wait: Duration,
    pub upstream_argv: Vec<String>,
    pub state: Mutex<State>,
    pub changed: Condvar,
    pub log: sys::Log,
}

impl Broker {
    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Queue `message` for `client`. A client whose queue is full is not
    /// reading: it is disconnected rather than allowed to hold the broker.
    pub fn deliver(&self, state: &State, client: u64, message: &Value) {
        let Some(target) = state.clients.get(&client) else {
            return;
        };
        let mut line = serde_json::to_vec(message).unwrap_or_default();
        line.push(b'\n');
        if let Err(TrySendError::Full(_)) = target.queue.try_send(line) {
            self.log
                .line(&format!("client {client} is not reading; disconnecting it"));
            let _ = target.stream.shutdown(std::net::Shutdown::Both);
        }
    }

    pub fn broadcast(&self, state: &State, message: &Value) {
        for client in state.clients.keys() {
            self.deliver(state, *client, message);
        }
    }

    /// Why the upstream cannot take a request now, and when to retry.
    pub fn not_ready(&self, state: &State) -> (String, u64) {
        match state.phase {
            Phase::Starting => (
                "the upstream is starting or waiting to be allowed".into(),
                15,
            ),
            Phase::Backoff(until) => (
                "the upstream exited and is restarting".into(),
                until.saturating_duration_since(Instant::now()).as_secs() + 5,
            ),
            Phase::Stopping => ("the broker is stopping".into(), 60),
            Phase::Ready => (String::new(), 0),
        }
    }

    /// The state once the upstream is ready, waiting at most --ready-wait-secs;
    /// otherwise the busy reason and retry delay.
    fn ready(&self) -> Result<MutexGuard<'_, State>, (String, u64)> {
        let deadline = Instant::now() + self.ready_wait;
        let mut state = self.lock();
        loop {
            if state.phase == Phase::Ready {
                return Ok(state);
            }
            let now = Instant::now();
            if now >= deadline || state.phase == Phase::Stopping {
                return Err(self.not_ready(&state));
            }
            state = self
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    /// Record the broker's state for `privateer mcp-broker status`.
    pub fn publish(&self) {
        let snapshot = {
            let state = self.lock();
            let phase = match state.phase {
                Phase::Starting => "starting",
                Phase::Ready => "ready",
                Phase::Backoff(_) => "restarting",
                Phase::Stopping => "stopping",
            };
            let mut clients: Vec<_> = state
                .clients
                .iter()
                .map(
                    |(id, c)| json!({"id": id, "pid": c.pid, "peer_pid": c.peer_pid, "cwd": c.cwd}),
                )
                .collect();
            clients.sort_by_key(|c| c["id"].as_u64());
            clients.truncate(64);
            json!({
                "version": BROKER_VERSION,
                "pid": std::process::id(),
                "name": self.name,
                "socket": self.socket,
                "upstream": {
                    "command": self.upstream_argv[0],
                    "state": phase,
                    "pid": state.upstream_pid,
                    "generation": state.generation,
                    "restarts": state.restarts,
                    "last_exit": state.last_exit,
                },
                "client_count": state.clients.len(),
                "clients": clients,
                "in_flight": state.pending.len(),
                "updated_ms": sys::now_ms() as u64,
            })
        };
        let path = self.dir.join(format!("{}.state.json", self.name));
        let _ = sys::write_atomic(&path, snapshot.to_string().as_bytes());
    }

    fn reply(&self, client: u64, message: Value) {
        let state = self.lock();
        self.deliver(&state, client, &message);
    }

    fn client(self: Arc<Self>, stream: UnixStream, peer_pid: Option<i64>) {
        let (Ok(writer), Ok(handle)) = (stream.try_clone(), stream.try_clone()) else {
            return;
        };
        let (queue, lines_out) = sync_channel::<Vec<u8>>(CLIENT_QUEUE);
        let id = {
            let mut state = self.lock();
            state.next_client += 1;
            state.writers += 1;
            let id = state.next_client;
            state.clients.insert(
                id,
                Client {
                    queue,
                    stream: handle,
                    pid: None,
                    peer_pid,
                    cwd: None,
                },
            );
            id
        };
        {
            let broker = self.clone();
            std::thread::spawn(move || {
                let mut writer = writer;
                for line in lines_out {
                    if writer.write_all(&line).is_err() {
                        break;
                    }
                }
                let _ = writer.shutdown(std::net::Shutdown::Both);
                broker.lock().writers -= 1;
                broker.changed.notify_all();
            });
        }
        self.log.line(&format!("client {id} connected"));
        self.publish();
        let mut reader = BufReader::with_capacity(64 * 1024, stream);
        let mut first = true;
        loop {
            match lines::next(&mut reader, MAX_LINE) {
                Ok(Some(Line::Text(line))) => self.message(id, &line, first),
                Ok(Some(Line::TooLong)) => {
                    self.log.line(&format!("client {id} sent an oversize line"));
                    self.reply(id, error(&Value::Null, -32600, "message exceeds 16 MiB"));
                }
                Ok(None) | Err(_) => break,
            }
            first = false;
        }
        self.disconnect(id);
    }

    fn message(&self, client: u64, line: &[u8], first: bool) {
        if line.iter().all(u8::is_ascii_whitespace) {
            return;
        }
        let value: Value = match serde_json::from_slice(line) {
            Ok(value) => value,
            Err(_) => return self.reply(client, error(&Value::Null, -32700, "parse error")),
        };
        match value {
            Value::Object(hello) if first && hello.contains_key("privateer_proxy") => {
                self.hello(client, &hello)
            }
            Value::Object(message) => self.request(client, message),
            Value::Array(_) => self.reply(
                client,
                error(&Value::Null, -32600, "batches are not supported"),
            ),
            _ => self.reply(client, error(&Value::Null, -32600, "invalid request")),
        }
    }

    /// The proxy's first line: who is calling, from where. Recorded only;
    /// nothing is routed by it yet. The claimed pid is kept beside the one
    /// the socket reports, and a mismatch is logged.
    fn hello(&self, client: u64, hello: &Map<String, Value>) {
        let pid = hello.get("pid").and_then(Value::as_i64);
        let cwd = hello
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|cwd| cwd.len() <= 4096)
            .map(str::to_owned);
        let peer_pid = {
            let mut state = self.lock();
            state.clients.get_mut(&client).and_then(|c| {
                c.pid = pid;
                c.cwd = cwd.clone();
                c.peer_pid
            })
        };
        let shown = |pid: Option<i64>| pid.map_or("none".to_owned(), |p| p.to_string());
        self.log.line(&format!(
            "client {client} hello pid {} peer pid {} cwd {}",
            shown(pid),
            shown(peer_pid),
            cwd.as_deref()
                .map_or("none".to_owned(), |c| sys::quoted(c, 4096))
        ));
        if let (Some(claimed), Some(seen)) = (pid, peer_pid) {
            if claimed != seen {
                self.log.line(&format!(
                    "client {client} claims pid {claimed} but the socket reports pid {seen}"
                ));
            }
        }
        self.publish();
    }

    fn request(&self, client: u64, message: Map<String, Value>) {
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match (method, message.get("id").cloned()) {
            (Some(method), Some(id)) => {
                if !(id.is_string() || id.is_number()) {
                    return self.reply(
                        client,
                        error(&Value::Null, -32600, "a request id is a string or number"),
                    );
                }
                match method.as_str() {
                    "initialize" => {
                        let answer = match self.ready() {
                            Ok(state) => {
                                json!({"jsonrpc": "2.0", "id": id, "result": state.init.clone()})
                            }
                            Err((reason, after)) => {
                                busy::busy(&id, &method, &self.label, &reason, after)
                            }
                        };
                        self.reply(client, answer)
                    }
                    "ping" => self.reply(client, json!({"jsonrpc": "2.0", "id": id, "result": {}})),
                    _ => self.forward(client, id, method, message),
                }
            }
            (Some(method), None) => match method.as_str() {
                "notifications/initialized" => {}
                "notifications/cancelled" => self.cancel(client, message),
                other => self.log.line(&format!(
                    "client {client}: dropped notification {}",
                    sys::quoted(other, 64)
                )),
            },
            // An answer to a server request: the broker answers those itself.
            (None, Some(_)) => {}
            (None, None) => self.reply(client, error(&Value::Null, -32600, "invalid request")),
        }
    }

    fn forward(&self, client: u64, id: Value, method: String, mut message: Map<String, Value>) {
        let (broker_id, queue) = {
            let mut state = match self.ready() {
                Ok(state) => state,
                Err((reason, after)) => {
                    return self.reply(
                        client,
                        busy::busy(&id, &method, &self.label, &reason, after),
                    );
                }
            };
            if !state.clients.contains_key(&client) {
                return;
            }
            let broker_id = state.next_id();
            message.insert("id".into(), json!(broker_id));
            let token = message
                .get_mut("params")
                .and_then(|p| p.get_mut("_meta"))
                .and_then(|m| m.get_mut("progressToken"))
                .filter(|t| t.is_string() || t.is_number())
                .map(|token| {
                    let ours = format!("privateer-{broker_id}");
                    let theirs = std::mem::replace(token, json!(ours));
                    (ours, theirs)
                });
            let token = token.map(|(ours, theirs)| {
                state.progress.insert(ours.clone(), (client, theirs));
                ours
            });
            let generation = state.generation;
            state.pending.insert(
                broker_id,
                Pending {
                    client,
                    id,
                    method,
                    generation,
                    token,
                },
            );
            (broker_id, state.upstream.clone())
        };
        let mut line = serde_json::to_vec(&message).unwrap_or_default();
        line.push(b'\n');
        let sent = queue.is_some_and(|queue| queue.send(line).is_ok());
        if !sent {
            let mut state = self.lock();
            if let Some(pending) = state.pending.remove(&broker_id) {
                if let Some(token) = &pending.token {
                    state.progress.remove(token);
                }
                self.deliver(&state, pending.client, &exited(&pending));
            }
        }
    }

    /// A client's cancellation, under the id the upstream knows.
    fn cancel(&self, client: u64, mut message: Map<String, Value>) {
        let Some(theirs) = message.get("params").and_then(|p| p.get("requestId")) else {
            return;
        };
        let (broker_id, queue) = {
            let mut state = self.lock();
            let Some(broker_id) = state
                .pending
                .iter()
                .find(|(_, p)| p.client == client && p.id == *theirs)
                .map(|(id, _)| *id)
            else {
                return;
            };
            let pending = state.pending.remove(&broker_id);
            if let Some(token) = pending.and_then(|p| p.token) {
                state.progress.remove(&token);
            }
            (broker_id, state.upstream.clone())
        };
        message["params"]["requestId"] = json!(broker_id);
        let mut line = serde_json::to_vec(&message).unwrap_or_default();
        line.push(b'\n');
        if let Some(queue) = queue {
            let _ = queue.send(line);
        }
    }

    /// Cancel upstream whatever the client left in flight, and drop their
    /// answers when they come.
    fn disconnect(&self, client: u64) {
        let (cancelled, queue) = {
            let mut state = self.lock();
            if let Some(c) = state.clients.remove(&client) {
                let _ = c.stream.shutdown(std::net::Shutdown::Both);
            }
            let generation = state.generation;
            let mine: Vec<u64> = state
                .pending
                .iter()
                .filter(|(_, p)| p.client == client)
                .map(|(id, _)| *id)
                .collect();
            let mut cancelled = vec![];
            for id in mine {
                if let Some(pending) = state.pending.remove(&id) {
                    if pending.generation == generation {
                        cancelled.push(id);
                    }
                }
            }
            state.progress.retain(|_, (owner, _)| *owner != client);
            (cancelled, state.upstream.clone())
        };
        if let Some(queue) = queue {
            for id in &cancelled {
                let mut line = serde_json::to_vec(&json!({"jsonrpc": "2.0",
                    "method": "notifications/cancelled",
                    "params": {"requestId": id, "reason": "client disconnected"}}))
                .unwrap_or_default();
                line.push(b'\n');
                let _ = queue.send(line);
            }
        }
        self.log.line(&format!(
            "client {client} disconnected; cancelled {} in flight",
            cancelled.len()
        ));
        self.publish();
    }
}

pub fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// The answer to a request the upstream took and then died on. The call is
/// never retried: it may have run.
pub fn exited(pending: &Pending) -> Value {
    if pending.method == "tools/call" {
        json!({"jsonrpc": "2.0", "id": pending.id, "result": {"isError": true, "content": [
            {"type": "text", "text": "upstream exited; the call may or may not have run"}]}})
    } else {
        error(
            &pending.id,
            -32603,
            "upstream exited; the request may or may not have run",
        )
    }
}

pub fn run(options: ServeOptions) -> i32 {
    let ServeOptions {
        dir,
        name,
        ready_wait,
        upstream,
    } = options;
    if let Err(error) = sys::private_dir(&dir) {
        eprintln!("privateer-mcp-broker: {error}");
        return 1;
    }
    let lock_path = dir.join(format!("{name}.lock"));
    // A few short tries, so a status probe holding the lock for an instant
    // does not stop a start; a running broker still refuses within 0.5 s.
    let mut lock = None;
    for _ in 0..10 {
        match sys::try_lock(&lock_path) {
            Ok(Some(file)) => {
                lock = Some(file);
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                eprintln!(
                    "privateer-mcp-broker: cannot lock {}: {error}",
                    lock_path.display()
                );
                return 1;
            }
        }
    }
    let Some(lock) = lock else {
        eprintln!(
            "privateer-mcp-broker: another broker holds {}",
            lock_path.display()
        );
        return 1;
    };
    let log = sys::Log::open(dir.join(format!("{name}.log")));
    let socket = dir.join(format!("{name}.sock"));
    let pid_file = dir.join(format!("{name}.pid"));
    let state_file = dir.join(format!("{name}.state.json"));
    let listener = match listen(&socket) {
        Ok(listener) => listener,
        Err(error) => {
            log.line(&format!("cannot listen: {error}"));
            eprintln!(
                "privateer-mcp-broker: cannot listen on {}: {error}",
                socket.display()
            );
            return 1;
        }
    };
    let signals = match sys::stop_signals() {
        Ok(fd) => fd,
        Err(error) => {
            eprintln!("privateer-mcp-broker: cannot handle signals: {error}");
            return 1;
        }
    };
    if let Err(error) = sys::write_atomic(&pid_file, format!("{}\n", std::process::id()).as_bytes())
    {
        eprintln!(
            "privateer-mcp-broker: cannot write {}: {error}",
            pid_file.display()
        );
        return 1;
    }
    log.line(&format!(
        "broker {BROKER_VERSION} pid {} serving {}",
        std::process::id(),
        socket.display()
    ));
    let broker = Arc::new(Broker {
        label: busy::label(&name),
        name,
        dir,
        socket: socket.clone(),
        ready_wait,
        upstream_argv: upstream,
        state: Mutex::new(State {
            phase: Phase::Starting,
            generation: 0,
            init: None,
            init_id: None,
            upstream: None,
            upstream_pid: None,
            restarts: 0,
            last_exit: None,
            supervising: true,
            pending: HashMap::new(),
            progress: HashMap::new(),
            next_id: 0,
            clients: HashMap::new(),
            next_client: 0,
            writers: 0,
        }),
        changed: Condvar::new(),
        log,
    });
    {
        let broker = broker.clone();
        std::thread::spawn(move || upstream::supervise(broker));
    }
    let me = sys::euid();
    'serve: loop {
        let ready = match sys::wait_readable(&[listener.as_raw_fd(), signals]) {
            Ok(ready) => ready,
            Err(error) => {
                broker.log.line(&format!("poll failed: {error}"));
                break;
            }
        };
        if ready.contains(&signals) {
            break;
        }
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    match sys::peer(stream.as_raw_fd()) {
                        Ok((uid, peer_pid)) if uid == me => {
                            let broker = broker.clone();
                            std::thread::spawn(move || broker.client(stream, peer_pid));
                        }
                        Ok((uid, _)) => broker
                            .log
                            .line(&format!("refused a connection from uid {uid}")),
                        Err(error) => broker
                            .log
                            .line(&format!("refused a connection: no peer uid: {error}")),
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => {
                    broker.log.line(&format!("accept failed: {error}"));
                    break 'serve;
                }
            }
        }
    }
    stop(&broker);
    flush_clients(&broker, Duration::from_secs(1));
    let _ = fs::remove_file(&socket);
    let _ = fs::remove_file(&state_file);
    let _ = fs::remove_file(&pid_file);
    broker.log.line("stopped");
    drop(lock);
    0
}

/// Bind the socket 0600, replacing a stale one: the caller holds the lock,
/// so no live broker owns it.
fn listen(socket: &std::path::Path) -> std::io::Result<UnixListener> {
    if socket.as_os_str().len() > MAX_SOCKET_PATH {
        return Err(std::io::Error::other("socket path is too long"));
    }
    if fs::symlink_metadata(socket).is_ok() {
        fs::remove_file(socket)?;
    }
    // SAFETY: umask only swaps the process mask; nothing else runs yet.
    let old = unsafe { libc::umask(0o177) };
    let bound = UnixListener::bind(socket);
    unsafe { libc::umask(old) };
    let listener = bound?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Give each client what is already queued for it (the "upstream exited"
/// answers to calls the stop cut short), waiting at most `within`; then
/// close every connection.
fn flush_clients(broker: &Broker, within: Duration) {
    let clients = std::mem::take(&mut broker.lock().clients);
    // Dropping each queue ends its writer thread once the queue is written.
    let streams: Vec<UnixStream> = clients.into_values().map(|c| c.stream).collect();
    let deadline = Instant::now() + within;
    let mut state = broker.lock();
    while state.writers > 0 && Instant::now() < deadline {
        state = broker
            .changed
            .wait_timeout(state, deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|p| p.into_inner())
            .0;
    }
    drop(state);
    for stream in streams {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
}

/// End the upstream (its whole process group) before the broker exits:
/// SIGTERM, then SIGKILL after 3 s.
fn stop(broker: &Broker) {
    let pid = {
        let mut state = broker.lock();
        state.phase = Phase::Stopping;
        broker.changed.notify_all();
        state.upstream_pid
    };
    broker.log.line("stopping");
    if let Some(pid) = pid {
        sys::signal_group(pid, libc::SIGTERM);
    }
    for (grace, signal) in [(3, libc::SIGKILL), (2, 0)] {
        let deadline = Instant::now() + Duration::from_secs(grace);
        let mut state = broker.lock();
        while state.supervising && Instant::now() < deadline {
            state = broker
                .changed
                .wait_timeout(state, Duration::from_millis(100))
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        if !state.supervising {
            return;
        }
        if let (Some(pid), true) = (state.upstream_pid, signal != 0) {
            sys::signal_group(pid, signal);
        }
    }
}
