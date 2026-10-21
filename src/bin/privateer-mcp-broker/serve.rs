//! `serve`: one broker per state directory and name. It holds the lock, the
//! socket and the upstream, and maps every client's ids onto broker-unique
//! ones so their answers never cross.
use crate::lines::{self, Line};
use crate::route::{self, LIST_WINDOWS, TAB};
use crate::window::{self, Opened};
use crate::{BROKER_VERSION, MAX_LINE, ServeOptions, busy, sys, upstream};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Lines queued for one client before it counts as stuck and is dropped.
const CLIENT_QUEUE: usize = 256;
/// The longest socket path macOS accepts (sun_path is 104 bytes there).
const MAX_SOCKET_PATH: usize = 103;
/// After an open, a workspace whose window has not shown yet is still
/// opening: its calls wait for the window rather than open another.
const OPEN_GRACE: Duration = Duration::from_secs(120);
/// How often a window that is opening is looked for.
const OPEN_POLL: Duration = Duration::from_secs(1);
/// The retry delay a call gets while its window opens.
const OPEN_RETRY_SECS: u64 = 15;

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
    /// A client's XcodeListWindows: its answer is filtered to this root
    /// (to nothing when the client has none).
    pub windows: Option<Option<PathBuf>>,
}

/// A request the broker itself sent upstream; its answer goes to `reply`.
pub struct Internal {
    pub generation: u64,
    pub reply: SyncSender<Value>,
}

/// One workspace's window opening, or its last one.
struct Opening {
    running: bool,
    at: Instant,
    error: Option<String>,
}

struct Client {
    queue: SyncSender<Vec<u8>>,
    stream: UnixStream,
    /// The pid the hello claims, and the one the socket reports.
    pid: Option<i64>,
    peer_pid: Option<i64>,
    /// The cwd the hello claims; only logged.
    cwd: Option<String>,
    /// The peer's working directory as the system reports it, and the
    /// workspace root it lies in: what routing goes by.
    peer_cwd: Option<PathBuf>,
    root: Option<PathBuf>,
    /// The proxy's `--project`, relative to the root.
    project: Option<String>,
}

/// Why a call is not forwarded: busy (retry later) or refused.
pub enum Refusal {
    Busy(String, u64),
    Error(String),
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
    /// Each upstream tool's inputSchema, by name; `None` until listed, and
    /// again whenever the upstream's tools change (`tools_epoch` counts).
    pub tools: Option<Arc<HashMap<String, Value>>>,
    pub tools_epoch: u64,
    pub internal: HashMap<u64, Internal>,
    /// Each workspace root's window, as the last window list showed it.
    tabs: HashMap<PathBuf, String>,
    /// The windows the broker opened (`NAME.windows.json`).
    windows: Vec<Opened>,
    opening: HashMap<PathBuf, Opening>,
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
    pub pools: Vec<PathBuf>,
    pub window_open: Option<Vec<String>>,
    pub window_close: Option<Vec<String>>,
    pub windows_file: PathBuf,
    /// Held while the window record is written, so writes land in order.
    record_io: Mutex<()>,
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
        self.ready_until(Instant::now() + self.ready_wait)
    }

    fn ready_until(&self, deadline: Instant) -> Result<MutexGuard<'_, State>, (String, u64)> {
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
                .map(|(id, c)| {
                    let tab = c.root.as_ref().and_then(|root| state.tabs.get(root));
                    json!({"id": id, "pid": c.pid, "peer_pid": c.peer_pid, "cwd": c.cwd,
                        "peer_cwd": c.peer_cwd, "root": c.root, "tab": tab})
                })
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
                "workspace_pools": self.pools,
                "windows": state.windows.iter().take(256).map(Opened::json).collect::<Vec<_>>(),
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
        // Who the client is comes from the socket and the system, read before
        // the hello: its pid's working directory and that one's workspace.
        let peer_cwd = peer_pid.and_then(sys::cwd_of);
        let root = peer_cwd
            .as_deref()
            .and_then(|cwd| route::workspace_root(cwd, &self.pools));
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
                    peer_cwd: peer_cwd.clone(),
                    root: root.clone(),
                    project: None,
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
        let shown = |path: &Option<PathBuf>| {
            path.as_ref().map_or("none".to_owned(), |p| {
                sys::quoted(&p.to_string_lossy(), 4096)
            })
        };
        self.log.line(&format!(
            "client {id} connected: peer pid {} cwd {} root {}",
            peer_pid.map_or("none".to_owned(), |p| p.to_string()),
            shown(&peer_cwd),
            if root.is_some() {
                shown(&root)
            } else {
                "none (unrouted)".to_owned()
            }
        ));
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
            Value::Object(control) if first && control.contains_key("privateer_control") => {
                self.control(client, &control)
            }
            Value::Object(message) => self.request(client, message),
            Value::Array(_) => self.reply(
                client,
                error(&Value::Null, -32600, "batches are not supported"),
            ),
            _ => self.reply(client, error(&Value::Null, -32600, "invalid request")),
        }
    }

    /// The proxy's first line: who it says it is, and from where. Its pid
    /// and cwd are claims, recorded and logged beside what the socket and the
    /// system report, never routed by. Its `project` picks the project a
    /// window opens, and only inside the client's own root.
    fn hello(&self, client: u64, hello: &Map<String, Value>) {
        let pid = hello.get("pid").and_then(Value::as_i64);
        let cwd = hello
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|cwd| cwd.len() <= 4096)
            .map(str::to_owned);
        let project = hello
            .get("project")
            .and_then(Value::as_str)
            .filter(|p| p.len() <= 1024)
            .map(str::to_owned);
        let (peer_pid, peer_cwd) = {
            let mut state = self.lock();
            state
                .clients
                .get_mut(&client)
                .map(|c| {
                    c.pid = pid;
                    c.cwd = cwd.clone();
                    c.project = project.clone();
                    (c.peer_pid, c.peer_cwd.clone())
                })
                .unwrap_or((None, None))
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
        let verified = peer_cwd.as_ref().map(|p| p.to_string_lossy().into_owned());
        if let Some(claimed) = &cwd {
            if verified.as_ref() != Some(claimed) {
                self.log.line(&format!(
                    "client {client} claims cwd {} but its working directory is {}",
                    sys::quoted(claimed, 4096),
                    verified.map_or("unknown".to_owned(), |v| sys::quoted(&v, 4096))
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
                    "tools/call" => self.call(client, id, message),
                    _ => self.forward(client, id, method, message, None),
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

    fn forward(
        &self,
        client: u64,
        id: Value,
        method: String,
        mut message: Map<String, Value>,
        windows: Option<Option<PathBuf>>,
    ) {
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
                    windows,
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

    /// A client's `tools/call`: its path arguments checked, a window-bound
    /// tool given the client's own window, and XcodeListWindows filtered to
    /// the client's root. Nothing refused is forwarded.
    fn call(&self, client: u64, id: Value, mut message: Map<String, Value>) {
        let deadline = Instant::now() + self.ready_wait;
        let method = "tools/call".to_owned();
        let Some(root) = self.lock().clients.get(&client).map(|c| c.root.clone()) else {
            return;
        };
        let name = message
            .get("params")
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let arguments = message
            .get("params")
            .and_then(|p| p.get("arguments"))
            .cloned()
            .unwrap_or(Value::Null);
        if let Err(why) = route::check_paths(&arguments, root.as_deref()) {
            return self.refuse(client, &id, &name, Refusal::Error(why));
        }
        if name == LIST_WINDOWS {
            return self.forward(client, id, method, message, Some(root));
        }
        let tools = match self.tools(deadline) {
            Ok(tools) => tools,
            Err(refusal) => return self.refuse(client, &id, &name, refusal),
        };
        // A tool the list does not show yet is bound when it names a window.
        let bound =
            tools.get(&name).map_or(false, route::window_bound) || arguments.get(TAB).is_some();
        if !bound {
            return self.forward(client, id, method, message, None);
        }
        let Some(root) = root else {
            return self.refuse(
                client,
                &id,
                &name,
                Refusal::Error(
                    "Xcode window tools work only from a Privateer task workspace".into(),
                ),
            );
        };
        let tab = match self.window_for(client, &root, deadline) {
            Ok(tab) => tab,
            Err(refusal) => return self.refuse(client, &id, &name, refusal),
        };
        match arguments.get(TAB) {
            None | Some(Value::Null) => {
                let params = message
                    .entry("params")
                    .or_insert_with(|| json!({}))
                    .as_object_mut();
                let Some(params) = params else {
                    return self.forward(client, id, method, message, None);
                };
                let arguments = params.entry("arguments").or_insert_with(|| json!({}));
                if !arguments.is_object() {
                    *arguments = json!({});
                }
                arguments[TAB] = json!(tab);
            }
            Some(given) if given.as_str() == Some(tab.as_str()) => {}
            Some(given) => {
                let shown: String = given
                    .as_str()
                    .map_or_else(|| given.to_string(), str::to_owned)
                    .chars()
                    .take(200)
                    .collect();
                return self.refuse(
                    client,
                    &id,
                    &name,
                    Refusal::Error(format!(
                        "tabIdentifier {shown} is not this workspace's Xcode window; call XcodeListWindows"
                    )),
                );
            }
        }
        self.forward(client, id, method, message, None)
    }

    fn refuse(&self, client: u64, id: &Value, tool: &str, refusal: Refusal) {
        let answer = match refusal {
            Refusal::Busy(reason, after) => {
                busy::busy(id, "tools/call", &self.label, &reason, after)
            }
            Refusal::Error(text) => {
                self.log.line(&format!(
                    "client {client}: refused {}: {}",
                    sys::quoted(tool, 64),
                    sys::quoted(&text, 300)
                ));
                json!({"jsonrpc": "2.0", "id": id, "result": {"isError": true,
                    "content": [{"type": "text", "text": text}]}})
            }
        };
        self.reply(client, answer)
    }

    /// A request of the broker's own, under a broker id no client sees; its
    /// result, or why there is none by `deadline`.
    fn ask(&self, method: &str, params: Value, deadline: Instant) -> Result<Value, Refusal> {
        let (id, answer, queue) = {
            let mut state = self
                .ready_until(deadline)
                .map_err(|(reason, after)| Refusal::Busy(reason, after))?;
            let id = state.next_id();
            let (reply, answer) = sync_channel(1);
            let generation = state.generation;
            state.internal.insert(id, Internal { generation, reply });
            (id, answer, state.upstream.clone())
        };
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut line = serde_json::to_vec(&request).unwrap_or_default();
        line.push(b'\n');
        let Some(queue) = queue.filter(|queue| queue.send(line).is_ok()) else {
            self.lock().internal.remove(&id);
            return Err(Refusal::Error("upstream exited; retry the call".into()));
        };
        match answer.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(answer) => match answer.get("error") {
                Some(error) => Err(Refusal::Error(format!(
                    "{method} failed upstream: {}",
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("no message")
                        .chars()
                        .take(300)
                        .collect::<String>()
                ))),
                None => Ok(answer.get("result").cloned().unwrap_or(Value::Null)),
            },
            Err(RecvTimeoutError::Timeout) => {
                self.lock().internal.remove(&id);
                let mut line = serde_json::to_vec(&json!({"jsonrpc": "2.0",
                    "method": "notifications/cancelled",
                    "params": {"requestId": id, "reason": "timed out"}}))
                .unwrap_or_default();
                line.push(b'\n');
                let _ = queue.try_send(line);
                Err(Refusal::Busy(
                    format!("the upstream did not answer {method} in time"),
                    15,
                ))
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(Refusal::Error("upstream exited; retry the call".into()))
            }
        }
    }

    /// The upstream's tools and their input schemas, listed once and again
    /// after `notifications/tools/list_changed`.
    fn tools(&self, deadline: Instant) -> Result<Arc<HashMap<String, Value>>, Refusal> {
        let epoch = {
            let state = self.lock();
            if let Some(tools) = &state.tools {
                return Ok(tools.clone());
            }
            state.tools_epoch
        };
        let mut tools = HashMap::new();
        let mut cursor: Option<Value> = None;
        for _ in 0..32 {
            let params = cursor.take().map_or(json!({}), |c| json!({"cursor": c}));
            let page = self.ask("tools/list", params, deadline)?;
            for tool in page
                .get("tools")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(name) = tool.get("name").and_then(Value::as_str) {
                    let schema = tool.get("inputSchema").cloned().unwrap_or(Value::Null);
                    tools.insert(name.to_owned(), schema);
                }
            }
            cursor = page.get("nextCursor").filter(|c| !c.is_null()).cloned();
            if cursor.is_none() {
                break;
            }
        }
        let bound = tools.values().filter(|s| route::window_bound(s)).count();
        let tools = Arc::new(tools);
        let mut state = self.lock();
        if state.tools_epoch == epoch {
            state.tools = Some(tools.clone());
            drop(state);
            self.log.line(&format!(
                "upstream lists {} tools, {bound} window-bound",
                tools.len()
            ));
        }
        Ok(tools)
    }

    /// List the upstream's tools now, off the calling thread.
    pub fn refresh_tools(self: &Arc<Self>) {
        let broker = self.clone();
        std::thread::spawn(move || {
            if let Err(Refusal::Error(why) | Refusal::Busy(why, _)) =
                broker.tools(Instant::now() + broker.ready_wait)
            {
                broker.log.line(&format!(
                    "cannot list upstream tools: {}",
                    sys::quoted(&why, 300)
                ));
            }
        });
    }

    /// The tab of `root`'s window: the cached one, else the one a window
    /// list shows, else the one a window opened for it shows.
    fn window_for(&self, client: u64, root: &Path, deadline: Instant) -> Result<String, Refusal> {
        if let Some(tab) = self.lock().tabs.get(root).cloned() {
            return Ok(tab);
        }
        if let Some(tab) = self.list_for(root, deadline)? {
            return Ok(tab);
        }
        self.open(client, root, deadline)?;
        let opening = || {
            Refusal::Busy(
                "Xcode is opening this workspace's window".into(),
                OPEN_RETRY_SECS,
            )
        };
        loop {
            match self.list_for(root, deadline) {
                Ok(Some(tab)) => return Ok(tab),
                Ok(None) => {}
                // Out of time while asking: the window is what it waits for.
                Err(Refusal::Busy(..)) => return Err(opening()),
                Err(refusal) => return Err(refusal),
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(opening());
            }
            std::thread::sleep(OPEN_POLL.min(deadline - now));
        }
    }

    /// Ask the upstream for its windows; `root`'s tab, if one is there.
    fn list_for(&self, root: &Path, deadline: Instant) -> Result<Option<String>, Refusal> {
        let result = self.ask(
            "tools/call",
            json!({"name": LIST_WINDOWS, "arguments": {}}),
            deadline,
        )?;
        if result.get("isError") == Some(&json!(true)) {
            let text = result
                .pointer("/content/0/text")
                .and_then(Value::as_str)
                .unwrap_or("no message");
            return Err(Refusal::Error(format!(
                "cannot list Xcode's windows: {}",
                text.chars().take(300).collect::<String>()
            )));
        }
        let (tab, record) = {
            let mut state = self.lock();
            let record = self.observe(&mut state, &result);
            (state.tabs.get(root).cloned(), record)
        };
        if record {
            self.save_windows();
        }
        self.publish();
        Ok(tab)
    }

    /// Learn each known root's window from a window list: the cache gains
    /// what it shows and loses what it no longer shows, and a window the
    /// broker opened gets its tab recorded. True when the record changed.
    pub fn observe(&self, state: &mut State, result: &Value) -> bool {
        let seen: Vec<(String, Option<PathBuf>)> = route::windows(result)
            .into_iter()
            .map(|(tab, path)| (tab, route::resolve(Path::new(&path))))
            .collect();
        let in_root = |root: &Path| -> Vec<String> {
            seen.iter()
                .filter(|(_, path)| path.as_deref().is_some_and(|p| route::inside(p, root)))
                .map(|(tab, _)| tab.clone())
                .collect()
        };
        let mut changed = false;
        for opened in state.windows.iter_mut() {
            let shown = seen
                .iter()
                .find(|(_, path)| path.as_deref() == Some(opened.project.as_path()))
                .map(|(tab, _)| tab.clone())
                .or_else(|| in_root(&opened.root).into_iter().next());
            if opened.tab.is_none() && shown.is_some() {
                opened.tab = shown;
                changed = true;
            }
        }
        let mut roots: Vec<PathBuf> = state
            .clients
            .values()
            .filter_map(|c| c.root.clone())
            .collect();
        roots.extend(state.windows.iter().map(|w| w.root.clone()));
        roots.extend(state.tabs.keys().cloned());
        roots.sort();
        roots.dedup();
        for root in roots {
            let mine = in_root(&root);
            let ours = state
                .windows
                .iter()
                .filter(|w| w.root == root)
                .filter_map(|w| w.tab.clone())
                .find(|tab| mine.contains(tab));
            match ours.or_else(|| mine.into_iter().next()) {
                Some(tab) => state.tabs.insert(root, tab),
                None => state.tabs.remove(&root),
            };
        }
        changed
    }

    /// Open `root`'s window: one open per root at a time, the others wait
    /// on it. The project is the client's `--project` or the one discovery
    /// finds. A failed open is the call's error, never retried within it.
    fn open(&self, client: u64, root: &Path, deadline: Instant) -> Result<(), Refusal> {
        let (cwd, chosen) = {
            let state = self.lock();
            let c = state.clients.get(&client);
            (
                c.and_then(|c| c.peer_cwd.clone()),
                c.and_then(|c| c.project.clone()),
            )
        };
        let project = match chosen {
            Some(rel) => route::chosen_project(&rel, root),
            None => {
                let mut bases: Vec<&Path> = cwd.iter().map(PathBuf::as_path).collect();
                bases.push(root);
                route::discover(&bases, root)
            }
        }
        .map_err(Refusal::Error)?;
        let Some(argv) = self.window_open.as_ref() else {
            return Err(Refusal::Error(
                "no window helper configured: cannot open this workspace's Xcode window".into(),
            ));
        };
        let mut waited = false;
        let mut state = self.lock();
        loop {
            match state.opening.get(root) {
                Some(o) if o.running => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(Refusal::Busy(
                            "Xcode is opening this workspace's window".into(),
                            OPEN_RETRY_SECS,
                        ));
                    }
                    waited = true;
                    state = self
                        .changed
                        .wait_timeout(state, deadline - now)
                        .unwrap_or_else(|p| p.into_inner())
                        .0;
                }
                Some(o) if waited => {
                    return o.error.clone().map_or(Ok(()), |e| Err(Refusal::Error(e)));
                }
                Some(o) if o.error.is_none() && o.at.elapsed() < OPEN_GRACE => return Ok(()),
                _ => break,
            }
        }
        state.opening.insert(
            root.to_path_buf(),
            Opening {
                running: true,
                at: Instant::now(),
                error: None,
            },
        );
        drop(state);
        self.log.line(&format!(
            "opening the window of {} with {}",
            sys::quoted(&root.to_string_lossy(), 4096),
            sys::quoted(&project.to_string_lossy(), 4096)
        ));
        let outcome = window::run(argv, &project)
            .map_err(|why| format!("could not open this workspace's Xcode window: {why}"));
        {
            let mut state = self.lock();
            if outcome.is_ok() {
                state
                    .windows
                    .retain(|w| !(w.root == root && w.project == project));
                state.windows.push(Opened {
                    root: root.to_path_buf(),
                    project: project.clone(),
                    tab: None,
                    opened_ms: sys::now_ms() as u64,
                });
            }
            state.opening.insert(
                root.to_path_buf(),
                Opening {
                    running: false,
                    at: Instant::now(),
                    error: outcome.as_ref().err().cloned(),
                },
            );
            self.changed.notify_all();
        }
        if let Err(why) = &outcome {
            self.log.line(&sys::quoted(why, 400));
        } else {
            self.save_windows();
            self.publish();
        }
        outcome.map_err(Refusal::Error)
    }

    /// Write the window record as it is now.
    pub fn save_windows(&self) {
        let _order = self.record_io.lock().unwrap_or_else(|p| p.into_inner());
        let windows = self.lock().windows.clone();
        if let Err(error) = window::save(&self.windows_file, &windows) {
            self.log
                .line(&format!("cannot write the window record: {error}"));
        }
    }

    /// A control line instead of a hello. Only `close_workspace` exists:
    /// close every window the broker opened under that root.
    fn control(&self, client: u64, control: &Map<String, Value>) {
        let answer = match control.get("close_workspace").and_then(Value::as_str) {
            Some(root) if root.starts_with('/') && root.len() <= 4096 => {
                self.close_workspace(client, Path::new(root))
            }
            _ => {
                json!({"error": "expected {\"privateer_control\":1,\"close_workspace\":ABS_ROOT}"})
            }
        };
        self.reply(client, answer);
    }

    fn close_workspace(&self, client: u64, given: &Path) -> Value {
        let mut roots = vec![given.to_path_buf()];
        if let Ok(real) = fs::canonicalize(given) {
            roots.push(real);
        }
        let under = |path: &Path| roots.iter().any(|root| route::inside(path, root));
        let targets: Vec<Opened> = self
            .lock()
            .windows
            .iter()
            .filter(|w| under(&w.root))
            .cloned()
            .collect();
        let mut closed = vec![];
        let mut failure = None;
        for target in targets {
            let outcome = match &self.window_close {
                Some(argv) => window::run(argv, &target.project),
                None => Err("no window helper configured".into()),
            };
            match outcome {
                Ok(()) => {
                    let mut state = self.lock();
                    state
                        .windows
                        .retain(|w| !(w.root == target.root && w.project == target.project));
                    state.opening.remove(&target.root);
                    closed.push(target.project);
                }
                Err(why) => {
                    failure = Some(format!(
                        "could not close the Xcode window of {}: {why}",
                        target.project.display()
                    ));
                    break;
                }
            }
        }
        self.lock().tabs.retain(|root, _| !under(root));
        if !closed.is_empty() {
            self.save_windows();
        }
        self.log.line(&format!(
            "client {client}: close_workspace {}: closed {}{}",
            sys::quoted(&given.to_string_lossy(), 4096),
            closed.len(),
            failure
                .as_deref()
                .map_or(String::new(), |f| format!("; {}", sys::quoted(f, 400)))
        ));
        self.publish();
        match failure {
            Some(error) => json!({"error": error}),
            None => json!({"closed": closed}),
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

/// A window list as one client may see it: only the windows inside its
/// root, and none for a client without one.
pub fn filter_windows(result: &mut Value, root: Option<&Path>) {
    route::filter_windows(result, |_, path| {
        root.is_some_and(|root| {
            route::resolve(Path::new(path)).is_some_and(|real| route::inside(&real, root))
        })
    });
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
        pools,
        window_open,
        window_close,
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
    let windows_file = dir.join(format!("{name}.windows.json"));
    // A broker started after another still closes what that one opened.
    let windows = window::load(&windows_file);
    // Clients' directories are resolved, so the pools are too.
    let pools: Vec<PathBuf> = pools
        .into_iter()
        .map(|pool| fs::canonicalize(&pool).unwrap_or(pool))
        .collect();
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
        pools,
        window_open,
        window_close,
        windows_file,
        record_io: Mutex::new(()),
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
            tools: None,
            tools_epoch: 0,
            internal: HashMap::new(),
            tabs: HashMap::new(),
            windows,
            opening: HashMap::new(),
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
