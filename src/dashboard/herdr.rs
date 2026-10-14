//! Explicit socket only: never discovers a focused session, creates a server,
//! starts a harness, sends keystrokes, or kills processes.
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

pub struct Client {
    path: PathBuf,
    reader: BufReader<UnixStream>,
    identity: (u64, u64),
    next_id: u64,
}

impl Client {
    pub fn connect(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute(), "Herdr socket must be an absolute path");
        let before = std::fs::metadata(path).context("Herdr socket unavailable")?;
        let stream = UnixStream::connect(path).context("connect to configured Herdr socket")?;
        let after = std::fs::metadata(path)?;
        ensure!(
            (before.dev(), before.ino()) == (after.dev(), after.ino()),
            "Herdr socket changed during connection; refresh"
        );
        stream
            .set_write_timeout(Some(DEADLINE))
            .context("set Herdr write deadline")?;
        stream
            .set_read_timeout(Some(DEADLINE))
            .context("set Herdr read deadline")?;
        Ok(Self {
            path: path.to_owned(),
            reader: BufReader::new(stream),
            identity: (after.dev(), after.ino()),
            next_id: 0,
        })
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = format!("privateer-dashboard-{}", self.next_id);
        let mut request = serde_json::to_vec(&json!({"id":id,"method":method,"params":params}))?;
        request.push(b'\n');
        self.reader.get_mut().write_all(&request)?;
        let deadline = Instant::now() + DEADLINE;
        let mut response = Vec::new();
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .context("Herdr response deadline exceeded")?;
            // poll enforces the total deadline without repeatedly resetting
            // SO_RCVTIMEO (macOS can refuse that after the peer has closed,
            // even while a large reply remains buffered).
            if self.reader.buffer().is_empty() {
                let mut fd = libc::pollfd {
                    fd: self.reader.get_ref().as_raw_fd(),
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
            let block = self.reader.fill_buf().context("read Herdr response")?;
            ensure!(!block.is_empty(), "Herdr closed the response stream");
            let end = block.iter().position(|b| *b == b'\n').map(|i| i + 1);
            let n = end.unwrap_or(block.len());
            ensure!(
                response.len() + n <= RESPONSE_LIMIT,
                "Herdr response exceeds 1 MiB"
            );
            response.extend_from_slice(&block[..n]);
            self.reader.consume(n);
            if end.is_some() {
                break;
            }
        }
        let value: Value = serde_json::from_slice(&response).context("invalid Herdr JSON")?;
        ensure!(value["id"] == id, "Herdr response ID mismatch");
        ensure!(
            value.get("error").is_none(),
            "Herdr refused {method}: {}",
            value["error"]
        );
        value
            .get("result")
            .cloned()
            .context("Herdr response missing result")
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
        for pane in &panes {
            ensure!(
                !pane.pane_id.is_empty() && !pane.terminal_id.is_empty(),
                "Herdr snapshot has an incomplete endpoint identity"
            );
        }
        Ok(View {
            version: snapshot["version"].as_str().unwrap_or("unknown").into(),
            protocol,
            panes,
            socket_identity: self.identity,
        })
    }

    /// Navigation only. A second snapshot and socket incarnation check reject a
    /// stale selection; these checks do not authorize input or process control.
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
