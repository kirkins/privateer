//! Explicit socket only: never discovers a focused session, creates a server,
//! or kills processes. Launch input is restricted to a freshly created pane.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::fd::AsRawFd,
    os::unix::{fs::MetadataExt, net::UnixStream},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub(crate) mod errands;
pub(crate) mod sidebar;

/// An explicit, ID-checked refusal is different from an unknown outcome after
/// disconnect/timeout. Only the former can clear a reserved create intent.
#[derive(Debug)]
pub(crate) struct Refusal {
    pub method: String,
    error: Value,
}
impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Herdr refused {}: {}", self.method, self.error)
    }
}
impl std::error::Error for Refusal {}

const RESPONSE_LIMIT: usize = 1_048_576;
const DEADLINE: Duration = Duration::from_millis(800);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Pane {
    pub pane_id: String,
    pub terminal_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    pub agent_status: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub agent_session: Option<Value>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct View {
    pub version: String,
    pub protocol: u64,
    pub panes: Vec<Pane>,
    pub socket_identity: (u64, u64),
}

impl View {
    /// Clearly marked sample terminals; demo mode never connects to a socket.
    pub fn demo() -> Self {
        Self {
            version: "demo".into(),
            protocol: 20,
            panes: [
                ("navigator", "pi"),
                ("apps", "codex"),
                ("infrastructure", "pi"),
            ]
            .into_iter()
            .map(|(id, agent)| Pane {
                pane_id: format!("demo:{id}"),
                terminal_id: format!("demo-terminal-{id}"),
                workspace_id: format!("demo-{id}"),
                tab_id: format!("demo-{id}:tab"),
                agent: Some(agent.into()),
                agent_status: "idle".into(),
                title: Some(format!("{id} primary")),
                agent_session: None,
            })
            .collect(),
            ..Self::default()
        }
    }
}

/// A navigation binding recorded by a live presence holder. The socket
/// incarnation and terminal identity are part of the binding, never a label.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Binding {
    pub socket: PathBuf,
    pub socket_identity: (u64, u64),
    pub pane: Pane,
}
impl Binding {
    pub fn capture() -> Result<Option<Self>> {
        if std::env::var("HERDR_ENV").as_deref() != Ok("1") {
            return Ok(None);
        }
        let Some(socket) = std::env::var_os("HERDR_SOCKET_PATH").map(PathBuf::from) else {
            return Ok(None);
        };
        let Some(id) = std::env::var("HERDR_PANE_ID").ok() else {
            return Ok(None);
        };
        let mut client = Client::connect(&socket)?;
        let view = client.snapshot()?;
        let pane = view
            .panes
            .into_iter()
            .find(|p| p.pane_id == id)
            .context("caller pane missing from Herdr snapshot")?;
        for (key, observed) in [
            ("HERDR_WORKSPACE_ID", &pane.workspace_id),
            ("HERDR_TAB_ID", &pane.tab_id),
        ] {
            if let Ok(inherited) = std::env::var(key) {
                ensure!(&inherited == observed, "Herdr caller context changed");
            }
        }
        Ok(Some(Self {
            socket,
            socket_identity: view.socket_identity,
            pane,
        }))
    }
    pub fn matches(&self, view: &View, pane: &Pane) -> bool {
        self.socket_identity == view.socket_identity
            && self.pane.pane_id == pane.pane_id
            && self.pane.terminal_id == pane.terminal_id
            && self.pane.workspace_id == pane.workspace_id
            && self.pane.tab_id == pane.tab_id
            && self
                .pane
                .agent_session
                .as_ref()
                .is_none_or(|s| pane.agent_session.as_ref() == Some(s))
    }
}

/// Herdr answers one request per connection and then closes it, so each
/// request opens its own stream to the socket this client first observed.
pub struct Client {
    path: PathBuf,
    identity: (u64, u64),
    next_id: u64,
}

impl Client {
    pub fn connect(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute(), "Herdr socket must be an absolute path");
        let metadata = std::fs::metadata(path).context("Herdr socket unavailable")?;
        Ok(Self {
            path: path.to_owned(),
            identity: (metadata.dev(), metadata.ino()),
            next_id: 0,
        })
    }

    /// The socket inode this client was created for.
    pub fn identity(&self) -> (u64, u64) {
        self.identity
    }

    /// A fresh stream to the same socket inode this client was created for.
    fn open(&self) -> Result<BufReader<UnixStream>> {
        let before = std::fs::metadata(&self.path).context("Herdr socket unavailable")?;
        ensure!(
            (before.dev(), before.ino()) == self.identity,
            "Herdr socket changed; refresh"
        );
        let stream =
            UnixStream::connect(&self.path).context("connect to configured Herdr socket")?;
        let after = std::fs::metadata(&self.path)?;
        ensure!(
            (after.dev(), after.ino()) == self.identity,
            "Herdr socket changed during connection; refresh"
        );
        stream
            .set_write_timeout(Some(DEADLINE))
            .context("set Herdr write deadline")?;
        stream
            .set_read_timeout(Some(DEADLINE))
            .context("set Herdr read deadline")?;
        Ok(BufReader::new(stream))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = format!("privateer-dashboard-{}", self.next_id);
        let mut request = serde_json::to_vec(&json!({"id":id,"method":method,"params":params}))?;
        request.push(b'\n');
        let mut reader = self.open()?;
        reader.get_mut().write_all(&request)?;
        let deadline = Instant::now() + DEADLINE;
        let mut response = Vec::new();
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .context("Herdr response deadline exceeded")?;
            // poll enforces the total deadline without repeatedly resetting
            // SO_RCVTIMEO (macOS can refuse that after the peer has closed,
            // even while a large reply remains buffered).
            if reader.buffer().is_empty() {
                let mut fd = libc::pollfd {
                    fd: reader.get_ref().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let wait_ms = remaining.as_millis().saturating_add(1).min(800) as i32;
                let ready = unsafe { libc::poll(&mut fd, 1, wait_ms) };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error.into());
                }
                ensure!(ready > 0, "Herdr response deadline exceeded");
            }
            let block = reader.fill_buf().context("read Herdr response")?;
            ensure!(!block.is_empty(), "Herdr closed the response stream");
            let end = block.iter().position(|b| *b == b'\n').map(|i| i + 1);
            let n = end.unwrap_or(block.len());
            ensure!(
                response.len() + n <= RESPONSE_LIMIT,
                "Herdr response exceeds 1 MiB"
            );
            response.extend_from_slice(&block[..n]);
            reader.consume(n);
            if end.is_some() {
                break;
            }
        }
        let value: Value = serde_json::from_slice(&response).context("invalid Herdr JSON")?;
        ensure!(value["id"] == id, "Herdr response ID mismatch");
        if let Some(error) = value.get("error") {
            return Err(Refusal {
                method: method.into(),
                error: error.clone(),
            }
            .into());
        }
        value
            .get("result")
            .cloned()
            .context("Herdr response missing result")
    }

    /// The pane the captain's Herdr client has focused, when Herdr says.
    pub fn focused_pane(&mut self) -> Result<Option<String>> {
        let result = self.request("session.snapshot", json!({}))?;
        Ok(result["snapshot"]["focused_pane_id"]
            .as_str()
            .map(str::to_owned))
    }

    pub fn snapshot(&mut self) -> Result<View> {
        let result = self.request("session.snapshot", json!({}))?;
        let snapshot = &result["snapshot"];
        let protocol = snapshot["protocol"]
            .as_u64()
            .context("missing Herdr protocol")?;
        ensure!(
            protocol >= 20,
            "Herdr protocol {protocol} lacks the verified snapshot contract (20+ required)"
        );
        let panes: Vec<Pane> = serde_json::from_value(snapshot["panes"].clone())
            .context("invalid Herdr pane snapshot")?;
        ensure!(
            panes.len() <= 1000,
            "Herdr snapshot has more than 1000 panes"
        );
        let mut ids = std::collections::BTreeSet::new();
        let mut terminals = std::collections::BTreeSet::new();
        for pane in &panes {
            ensure!(
                !pane.pane_id.is_empty()
                    && !pane.terminal_id.is_empty()
                    && !pane.workspace_id.is_empty()
                    && !pane.tab_id.is_empty(),
                "Herdr snapshot has an incomplete endpoint identity"
            );
            ensure!(
                ids.insert(&pane.pane_id) && terminals.insert(&pane.terminal_id),
                "Herdr snapshot has ambiguous endpoint identities"
            );
        }
        Ok(View {
            version: snapshot["version"].as_str().unwrap_or("unknown").into(),
            protocol,
            panes,
            socket_identity: self.identity,
        })
    }

    /// Create and launch only in the returned new shell pane. Every path is
    /// quoted as a POSIX shell token because pane.send_input runs shell text.
    pub fn board(
        &mut self,
        observed: &View,
        folder: &Path,
        name: &str,
        binary: &Path,
        home: &Path,
    ) -> Result<Pane> {
        self.launch(observed, folder, name, binary, home, &["start"])
    }

    /// Open a background space running `privateer --home HOME COMMAND...`:
    /// a hosted first mate or a crew member's supervisor.
    pub fn space(
        &mut self,
        folder: &Path,
        label: &str,
        binary: &Path,
        home: &Path,
        env: &[(&str, &str)],
        command: &[&str],
    ) -> Result<Pane> {
        let observed = self.snapshot()?;
        self.spawn(
            &observed,
            "workspace.create",
            folder,
            label,
            false,
            env,
            binary,
            home,
            command,
        )
    }

    /// Host-owned bridge launch. Persist the exact new endpoint BEFORE input,
    /// including when input fails or its acknowledgement is lost. Never replay
    /// that input into an existing pane after a dispatcher restart.
    pub(crate) fn bridge_space(
        &mut self,
        folder: &Path,
        binary: &Path,
        home: &Path,
        registry: &Path,
        token: &str,
        record: impl FnMut(Option<&Pane>) -> Result<()>,
    ) -> Result<Pane> {
        let observed = self.snapshot()?;
        self.spawn_recorded(
            &observed,
            "workspace.create",
            folder,
            "bridge",
            false,
            &[
                ("PRIVATEER_BRIDGE_TOKEN", token),
                // Herdr may have a different HOME/XDG environment from its
                // dispatchers. Pin the resolved registry even without an
                // explicit PRIVATEER_FLEET override in the launching service.
                (
                    "PRIVATEER_FLEET",
                    registry.to_str().context("fleet path must be UTF-8")?,
                ),
            ],
            binary,
            home,
            &[],
            false,
            record,
        )
    }

    /// Open a tab following the ship's first mate (`privateer watch`).
    pub fn watch(
        &mut self,
        observed: &View,
        folder: &Path,
        name: &str,
        binary: &Path,
        home: &Path,
    ) -> Result<Pane> {
        self.launch(
            observed,
            folder,
            &format!("{name} watch"),
            binary,
            home,
            &["watch", name],
        )
    }

    fn launch(
        &mut self,
        observed: &View,
        folder: &Path,
        name: &str,
        binary: &Path,
        home: &Path,
        command: &[&str],
    ) -> Result<Pane> {
        self.spawn(
            observed,
            "tab.create",
            folder,
            name,
            true,
            &[],
            binary,
            home,
            command,
        )
    }

    /// Create a tab or space and launch only in its returned new shell pane.
    #[allow(clippy::too_many_arguments)]
    fn spawn(
        &mut self,
        observed: &View,
        method: &str,
        folder: &Path,
        name: &str,
        focus: bool,
        extra_env: &[(&str, &str)],
        binary: &Path,
        home: &Path,
        command: &[&str],
    ) -> Result<Pane> {
        self.spawn_recorded(
            observed,
            method,
            folder,
            name,
            focus,
            extra_env,
            binary,
            home,
            command,
            true,
            |_| Ok(()),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_recorded(
        &mut self,
        observed: &View,
        method: &str,
        folder: &Path,
        name: &str,
        focus: bool,
        extra_env: &[(&str, &str)],
        binary: &Path,
        home: &Path,
        command: &[&str],
        home_argument: bool,
        mut record: impl FnMut(Option<&Pane>) -> Result<()>,
    ) -> Result<Pane> {
        ensure!(
            observed.socket_identity == self.identity,
            "Herdr server socket changed; refresh"
        );
        ensure!(
            folder.is_dir() && home.join("privateer.sqlite").is_file(),
            "ship folder is missing"
        );
        let metadata = std::fs::metadata(&self.path)?;
        ensure!(
            (metadata.dev(), metadata.ino()) == self.identity,
            "Herdr socket changed before tab creation"
        );
        self.snapshot()?; // protocol gate before the first mutation
        let mut env = serde_json::Map::new();
        for key in [
            "PRIVATEER_FLEET",
            "PRIVATEER_SERVICE",
            "PRIVATEER_RESOURCE_HOME",
            "PRIVATEER_PLUGINS",
            "PRIVATEER_ROLE",
            "PRIVATEER_ACTOR",
        ] {
            if let Ok(value) = std::env::var(key) {
                env.insert(key.into(), json!(value));
            }
        }
        env.insert("PRIVATEER_HOME".into(), json!(home));
        for (key, value) in extra_env {
            env.insert((*key).into(), json!(value));
        }
        record(None)?; // durable create intent, only after all preflight reads
        let made = self.request(
            method,
            json!({"cwd":folder,"label":name,"focus":focus,"env":env}),
        )?;
        let pane: Pane = serde_json::from_value(made["root_pane"].clone())
            .with_context(|| format!("{method} did not return an exact root pane"))?;
        ensure!(
            !pane.pane_id.is_empty()
                && !pane.terminal_id.is_empty()
                && !pane.workspace_id.is_empty()
                && !pane.tab_id.is_empty(),
            "incomplete new pane identity"
        );
        ensure!(
            !observed
                .panes
                .iter()
                .any(|p| p.pane_id == pane.pane_id || p.terminal_id == pane.terminal_id),
            "host returned an existing pane; no command sent"
        );
        record(Some(&pane))?;
        let fresh = self.snapshot()?;
        ensure!(
            fresh.panes.iter().any(|p| p == &pane && p.agent.is_none()),
            "new shell pane changed before launch; no command sent"
        );
        let metadata = std::fs::metadata(&self.path)?;
        ensure!(
            (metadata.dev(), metadata.ino()) == self.identity,
            "Herdr socket changed before launch"
        );
        fn quote(path: &Path) -> Result<String> {
            let path = path.to_str().context("launch path must be UTF-8")?;
            ensure!(!path.chars().any(char::is_control), "invalid launch path");
            Ok(format!("'{}'", path.replace('\'', "'\"'\"'")))
        }
        let words = command
            .iter()
            .map(|word| {
                ensure!(
                    word.chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)),
                    "invalid launch argument"
                );
                Ok(*word)
            })
            .collect::<Result<Vec<_>>>()?;
        let text = if home_argument {
            format!(
                "exec {} --home {} {}",
                quote(binary)?,
                quote(home)?,
                words.join(" ")
            )
        } else {
            // The machine bridge observes the fleet, not whichever home won
            // contention. Its adoption token suppresses PRIVATEER_HOME fallback.
            format!("exec {}", quote(binary)?)
        };
        self.request(
            "pane.send_input",
            json!({"pane_id":pane.pane_id,"text":text,"keys":["Enter"]}),
        )?;
        Ok(pane)
    }

    /// A pane's recent screen, with its colours, and its revision.
    pub(crate) fn read_screen(&mut self, pane: &str, lines: u32) -> Result<Value> {
        let result = self.request(
            "pane.read",
            json!({"pane_id": pane, "source": "recent", "format": "ansi",
                   "strip_ansi": false, "lines": lines}),
        )?;
        Ok(result["read"].clone())
    }

    /// Type into a pane: text, then keys such as `Enter`.
    pub(crate) fn type_into(&mut self, pane: &str, text: &str, keys: &[String]) -> Result<()> {
        self.request(
            "pane.send_input",
            json!({"pane_id": pane, "text": text, "keys": keys}),
        )?;
        Ok(())
    }

    /// Navigation only. A second snapshot and socket incarnation check reject a
    /// stale selection; these checks do not authorize input or process control.
    /// Report a pane as an agent Privateer supervises: `agent` is its
    /// harness, `display` the label Herdr lists it under.
    pub fn report_agent(
        &mut self,
        pane: &str,
        agent: &str,
        display: &str,
        title: &str,
        state: &str,
        message: &str,
    ) -> Result<()> {
        self.request(
            "pane.report_agent",
            json!({"pane_id":pane,"source":"privateer","agent":agent,"state":state,"message":message}),
        )?;
        self.request(
            "pane.report_metadata",
            json!({"pane_id":pane,"source":"privateer","agent":agent,"display_agent":display,"title":title}),
        )?;
        Ok(())
    }

    /// Mark a space as Privateer's: `role` is `bridge`, `mate` or `crew`,
    /// and `ship` the ship it belongs to.
    pub fn tag(&mut self, workspace: &str, role: &str, ship: &str) -> Result<()> {
        self.request(
            "workspace.report_metadata",
            json!({"workspace_id":workspace,"source":"privateer",
                "tokens":{"privateer":role,"ship":ship}}),
        )?;
        Ok(())
    }

    /// Keep Privateer's spaces together at the top of the sidebar: the bridge
    /// first, then each ship's first mate followed by its crew. Ships keep
    /// their first mates' current order, then name for ships without a mate.
    /// Orphan crew follow all first mates; other spaces are left alone below.
    pub fn arrange(&mut self) -> Result<()> {
        self.arrange_with_mates(&[])
    }

    pub(crate) fn arrange_with_mates(&mut self, local: &[(String, Binding)]) -> Result<()> {
        self.arrange_observed_with_mates(local).map(|_| ())
    }

    pub(crate) fn arrange_observed_with_mates(
        &mut self,
        local: &[(String, Binding)],
    ) -> Result<Option<View>> {
        let listed = self.request("workspace.list", json!({}))?;
        let mut spaces: Vec<(String, String, String)> = listed["workspaces"]
            .as_array()
            .context("workspace list missing")?
            .iter()
            .filter_map(|w| {
                Some((
                    w["workspace_id"].as_str()?.to_owned(),
                    w["tokens"]["privateer"].as_str().unwrap_or("").to_owned(),
                    w["tokens"]["ship"].as_str().unwrap_or("").to_owned(),
                ))
            })
            .collect();
        // Metadata is presentation state and can disappear on a host restart.
        // Recover mates from leases, but only while the bound terminal still
        // exists on this socket incarnation. Never guess from a space's label.
        let mut mates = sidebar::fleet_mates();
        mates.extend_from_slice(local);
        let view = if !mates.is_empty() {
            Some(self.snapshot()?)
        } else {
            None
        };
        if let Some(view) = &view {
            for (ship, binding) in mates {
                if binding.socket != self.path
                    || !view.panes.iter().any(|p| binding.matches(view, p))
                {
                    continue;
                }
                if let Some((id, role, owner)) = spaces
                    .iter_mut()
                    .find(|(id, _, _)| id == &binding.pane.workspace_id)
                {
                    if role == "mate" && owner == &ship {
                        continue;
                    }
                    self.tag(id, "mate", &ship)?;
                    *role = "mate".into();
                    *owner = ship;
                }
            }
        }
        let mut ships: Vec<&str> = vec![];
        for (_, role, ship) in &spaces {
            if matches!(role.as_str(), "mate" | "crew") && !ships.contains(&ship.as_str()) {
                ships.push(ship);
            }
        }
        ships.sort_by_key(|ship| {
            (
                spaces
                    .iter()
                    .position(|(_, role, owner)| role == "mate" && owner.as_str() == *ship)
                    .unwrap_or(usize::MAX),
                *ship,
            )
        });
        let of = |role: &str, ship: Option<&str>| -> Vec<String> {
            spaces
                .iter()
                .filter(|(_, r, s)| r == role && ship.is_none_or(|ship| s == ship))
                .map(|(id, _, _)| id.clone())
                .collect()
        };
        let mut wanted: Vec<String> = of("bridge", None);
        for ship in &ships {
            wanted.extend(of("mate", Some(ship)));
            wanted.extend(of("crew", Some(ship)));
        }
        let current: Vec<&String> = spaces
            .iter()
            .map(|(id, _, _)| id)
            .take(wanted.len())
            .collect();
        if wanted.is_empty() || current.iter().copied().eq(wanted.iter()) {
            return Ok(view);
        }
        let before = spaces
            .iter()
            .map(|(id, _, _)| id)
            .find(|id| !wanted.contains(id));
        self.request(
            "workspace.move_block",
            json!({"workspace_ids":wanted,"before_workspace_id":before}),
        )?;
        Ok(view)
    }

    pub fn release_agent(&mut self, pane: &str, agent: &str) -> Result<()> {
        self.request(
            "pane.release_agent",
            json!({"pane_id":pane,"source":"privateer","agent":agent}),
        )?;
        Ok(())
    }

    /// The pane as Herdr lists it now, read back to confirm a report or release
    /// took effect: Herdr answers ok to a report it then drops.
    pub fn pane_get(&mut self, pane: &str) -> Result<Value> {
        let result = self.request("pane.get", json!({"pane_id":pane}))?;
        Ok(result.get("pane").cloned().unwrap_or(result))
    }

    /// Navigation to a non-agent bridge, authorized by its separate durable
    /// launch claim and live dashboard lease. This grants no input authority.
    pub(crate) fn focus_bridge(&mut self, binding: &Binding) -> Result<()> {
        ensure!(binding.socket == self.path, "bridge socket scope changed");
        let fresh = self.snapshot()?;
        ensure!(
            fresh.panes.iter().any(|p| {
                binding.matches(&fresh, p)
                    && p.agent == binding.pane.agent
                    && p.agent_session == binding.pane.agent_session
            }),
            "bridge terminal occupant changed"
        );
        self.request("pane.focus", json!({"pane_id": binding.pane.pane_id}))?;
        Ok(())
    }

    pub fn focus(&mut self, observed: &View, pane: &Pane) -> Result<()> {
        ensure!(
            observed.socket_identity == self.identity,
            "Herdr server socket changed; refresh before opening a terminal"
        );
        let metadata = std::fs::metadata(&self.path)?;
        ensure!(
            (metadata.dev(), metadata.ino()) == self.identity,
            "Herdr server socket changed; refresh"
        );
        let fresh = self.snapshot()?;
        let current = fresh
            .panes
            .iter()
            .find(|p| p.pane_id == pane.pane_id)
            .context("selected pane no longer exists")?;
        ensure!(
            current.terminal_id == pane.terminal_id
                && current.workspace_id == pane.workspace_id
                && current.tab_id == pane.tab_id
                && current.agent == pane.agent
                && current.agent_session == pane.agent_session,
            "selected terminal occupant changed; refresh"
        );
        ensure!(
            current.agent.is_some(),
            "only recognized agent terminals support focus in this preview"
        );
        self.request("agent.focus", json!({"target":pane.pane_id}))?;
        Ok(())
    }
}
