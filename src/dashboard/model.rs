use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crate::store::reject_symlinks;

pub const ROW_LIMIT: usize = 200;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalBinding {
    pub pane_id: String,
    pub terminal_id: String,
}

/// A presentation registry entry. `id` is the runtime's exact owner identity;
/// this does not create a new authority role or a remote runtime home.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ship {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub liaison: bool,
    #[serde(default)]
    pub terminals: Vec<TerminalBinding>,
    #[serde(default)]
    pub live: Option<super::fleet::ShipState>,
}

impl Ship {
    fn demo(id: &str, name: &str, description: &str, agent: Option<&str>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            liaison: id == "navigator",
            live: None,
            terminals: agent
                .map(|_| TerminalBinding {
                    pane_id: format!("demo:{id}"),
                    terminal_id: format!("demo-terminal-{id}"),
                })
                .into_iter()
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Custody {
    #[serde(default)]
    pub home: Option<PathBuf>,
    pub seq: i64,
    pub task_id: String,
    pub actor: String,
    pub kind: String,
    pub owner: String,
}

pub(super) fn read_registry(home: &Path) -> Result<Vec<Ship>> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Registry {
        ships: Vec<Ship>,
    }
    let path = home.join("dashboard.json");
    reject_symlinks(&path)?;
    if let Ok(meta) = std::fs::metadata(&path) {
        ensure!(meta.is_file(), "dashboard registry must be a regular file");
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "dashboard registry exceeds 64 KiB");
    let registry: Registry =
        serde_json::from_slice(&bytes).context("invalid dashboard.json ship registry")?;
    Ok(registry.ships)
}

pub(super) fn validate_registry(ships: &[Ship]) -> Result<()> {
    ensure!(ships.len() <= 200, "dashboard registry exceeds 200 ships");
    let mut ids = BTreeSet::new();
    for ship in ships {
        crate::store::identifier(&ship.id)?;
        ensure!(ids.insert(ship.id.clone()), "duplicate ship identity");
        ensure!(
            !ship.name.is_empty() && ship.name.len() <= 160 && ship.description.len() <= 512,
            "invalid ship name/description length"
        );
        ensure!(
            ship.terminals.len() <= 32,
            "ship has too many terminal bindings"
        );
        for terminal in &ship.terminals {
            ensure!(
                !terminal.pane_id.is_empty()
                    && terminal.pane_id.len() <= 160
                    && !terminal.terminal_id.is_empty()
                    && terminal.terminal_id.len() <= 160,
                "invalid terminal binding"
            );
        }
    }
    validate_navigation(ships)
}

pub(super) fn validate_navigation(ships: &[Ship]) -> Result<()> {
    ensure!(
        ships.iter().filter(|s| s.liaison).count() <= 1,
        "only one ship may be the liaison"
    );
    let mut terminals = BTreeMap::new();
    for ship in ships {
        for terminal in &ship.terminals {
            ensure!(
                terminals
                    .insert(terminal.pane_id.as_str(), ship.id.as_str())
                    .is_none(),
                "a pane can belong to only one ship"
            );
        }
    }
    let mut automatic = BTreeMap::new();
    for ship in ships {
        if let Some(binding) = ship.live.as_ref().and_then(|live| live.binding.as_ref()) {
            ensure!(
                terminals
                    .get(binding.pane.pane_id.as_str())
                    .is_none_or(|owner| *owner == ship.id)
                    && automatic
                        .insert(
                            (binding.socket_identity, binding.pane.pane_id.as_str()),
                            ship.id.as_str()
                        )
                        .is_none_or(|owner| owner == ship.id),
                "a pane can belong to only one ship"
            );
        }
    }
    Ok(())
}

/// A count with its noun, e.g. "1 pending issue" or "2 pending issues".
pub fn count(n: usize, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

/// Terminal escape sequences and control bytes must never reach the renderer.
pub fn clean(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(limit)
        .collect()
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Task {
    #[serde(default)]
    pub home: Option<PathBuf>,
    pub id: String,
    pub owner: String,
    pub title: String,
    pub status: String,
    #[serde(default)]
    pub handoff_to: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Notice {
    #[serde(default)]
    pub home: Option<PathBuf>,
    #[serde(default)]
    pub owner: String,
    pub seq: i64,
    pub task_id: String,
    pub issue_key: String,
    pub kind: String,
    pub summary: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Job {
    #[serde(default)]
    pub home: Option<PathBuf>,
    #[serde(default)]
    pub owner: String,
    pub id: String,
    pub task_id: String,
    pub status: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Snapshot {
    #[serde(default)]
    pub presentation_error: Option<String>,
    pub ships: Vec<Ship>,
    pub custody: Vec<Custody>,
    pub tasks: Vec<Task>,
    pub pending: Vec<Notice>,
    pub activity: Vec<Notice>,
    pub jobs: Vec<Job>,
    pub truncated: bool,
}

impl Snapshot {
    pub fn include_observed_ships(&mut self) {
        let mut observed = BTreeSet::new();
        for task in &self.tasks {
            observed.insert(task.owner.clone());
            if let Some(target) = &task.handoff_to {
                observed.insert(target.clone());
            }
        }
        for notice in self.pending.iter().chain(&self.activity) {
            observed.insert(notice.owner.clone());
        }
        for job in &self.jobs {
            observed.insert(job.owner.clone());
        }
        for custody in &self.custody {
            observed.insert(custody.actor.clone());
            observed.insert(custody.owner.clone());
        }
        for id in observed {
            if !id.is_empty() && !self.ships.iter().any(|s| s.id == id) {
                self.ships.push(Ship {
                    name: id.clone(),
                    id,
                    ..Ship::default()
                });
            }
        }
    }
    /// Open an existing home with SQLite READ_ONLY, avoiding Store::open's
    /// migrations/chmod and job.list's recovery side effects. No resource ledger.
    pub fn read(home: &Path) -> Result<Self> {
        let mut snapshot = Self::read_work(home)?;
        snapshot.ships = read_registry(home)?;
        validate_registry(&snapshot.ships)?;
        snapshot.include_observed_ships();
        Ok(snapshot)
    }

    pub(super) fn read_work(home: &Path) -> Result<Self> {
        ensure!(home.is_absolute(), "dashboard home must be absolute");
        let db = home.join("privateer.sqlite");
        reject_symlinks(&db)?;
        let mut conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(Duration::from_millis(100))?;
        let deadline = Instant::now() + Duration::from_millis(500);
        conn.progress_handler(10_000, Some(move || Instant::now() >= deadline));
        let tx = conn.transaction()?;
        let schema: String =
            tx.query_row("SELECT value FROM metadata WHERE key='schema'", [], |r| {
                r.get(0)
            })?;
        ensure!(schema == "1", "unsupported dashboard state schema {schema}");
        let mut snapshot = Self::default();
        {
            let mut q = tx.prepare("SELECT substr(id,1,160),substr(owner,1,160),substr(title,1,512),status,handoff_to FROM tasks ORDER BY CASE WHEN status IN ('running','blocked','open') THEN 0 ELSE 1 END,id LIMIT 201")?;
            for row in q.query_map([], |r| {
                Ok(Task {
                    home: Some(home.to_path_buf()),
                    id: r.get(0)?,
                    owner: r.get(1)?,
                    title: r.get(2)?,
                    status: r.get(3)?,
                    handoff_to: r.get(4)?,
                })
            })? {
                snapshot.tasks.push(row?);
            }
        }
        snapshot.pending = read_notices(&tx, home, true)?;
        snapshot.activity = read_notices(&tx, home, false)?;
        {
            // Stored outcomes only. We do not probe processes or settle jobs.
            let mut q = tx.prepare("SELECT substr(j.id,1,160),substr(json_extract(j.record,'$.task_id'),1,160),substr(json_extract(j.record,'$.status'),1,40),substr(t.owner,1,160) FROM jobs j JOIN tasks t ON t.id=json_extract(j.record,'$.task_id') ORDER BY j.rowid DESC LIMIT 201")?;
            for row in q.query_map([], |r| {
                Ok(Job {
                    home: Some(home.to_path_buf()),
                    id: r.get(0)?,
                    task_id: r.get(1)?,
                    status: r.get(2)?,
                    owner: r.get(3)?,
                })
            })? {
                snapshot.jobs.push(row?);
            }
        }
        {
            let mut q=tx.prepare("SELECT a.seq,substr(json_extract(a.payload,'$.id'),1,160),substr(json_extract(a.payload,'$.actor'),1,160),a.kind,substr(t.owner,1,160) FROM audit a JOIN tasks t ON t.id=json_extract(a.payload,'$.id') WHERE a.kind IN ('task.handoff','task.accept') ORDER BY a.seq DESC LIMIT 201")?;
            snapshot.custody = q
                .query_map([], |r| {
                    Ok(Custody {
                        home: Some(home.to_path_buf()),
                        seq: r.get(0)?,
                        task_id: r.get(1)?,
                        actor: r.get(2)?,
                        kind: r.get(3)?,
                        owner: r.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        tx.commit()?;
        snapshot.truncated = snapshot.tasks.len() > ROW_LIMIT
            || snapshot.pending.len() > ROW_LIMIT
            || snapshot.activity.len() > ROW_LIMIT
            || snapshot.jobs.len() > ROW_LIMIT
            || snapshot.custody.len() > ROW_LIMIT;
        snapshot.tasks.truncate(ROW_LIMIT);
        snapshot.pending.truncate(ROW_LIMIT);
        snapshot.activity.truncate(ROW_LIMIT);
        snapshot.jobs.truncate(ROW_LIMIT);
        snapshot.custody.truncate(ROW_LIMIT);
        Ok(snapshot)
    }

    pub fn demo() -> Self {
        let mut sample = Self {
            presentation_error: None,
            ships: vec![
                Ship::demo(
                    "navigator",
                    "Navigator",
                    "Your usual point of contact; an equal peer",
                    Some("pi"),
                ),
                Ship::demo(
                    "apps",
                    "Apps",
                    "Application development and builds",
                    Some("codex"),
                ),
                Ship::demo(
                    "infrastructure",
                    "Infrastructure",
                    "Services, connectivity and runtime investigations",
                    Some("pi"),
                ),
                Ship::demo(
                    "research",
                    "Research",
                    "Available for the next investigation",
                    None,
                ),
            ],
            custody: vec![Custody {
                home: None,
                seq: 5,
                task_id: "app-build".into(),
                actor: "apps".into(),
                kind: "task.handoff".into(),
                owner: "apps".into(),
            }],
            tasks: vec![
                Task {
                    home: None,
                    id: "navigation".into(),
                    owner: "navigator".into(),
                    title: "Coordinate the next voyage".into(),
                    status: "running".into(),
                    handoff_to: None,
                },
                Task {
                    home: None,
                    id: "app-build".into(),
                    owner: "apps".into(),
                    title: "Investigate the failing build".into(),
                    status: "running".into(),
                    handoff_to: Some("infrastructure".into()),
                },
                Task {
                    home: None,
                    id: "route-check".into(),
                    owner: "infrastructure".into(),
                    title: "Check the provider route".into(),
                    status: "blocked".into(),
                    handoff_to: None,
                },
            ],
            pending: vec![Notice {
                home: None,
                owner: "infrastructure".into(),
                seq: 50,
                task_id: "route-check".into(),
                issue_key: "connection".into(),
                kind: "blocked".into(),
                summary: "Repeated connection facts appear as one pending issue".into(),
            }],
            activity: vec![Notice {
                home: None,
                owner: "apps".into(),
                seq: 51,
                task_id: "app-build".into(),
                issue_key: "tests".into(),
                kind: "outcome".into(),
                summary: "Sample logbook outcome — demonstration data only".into(),
            }],
            jobs: vec![Job {
                home: None,
                owner: "apps".into(),
                id: "build-1".into(),
                task_id: "app-build".into(),
                status: "running".into(),
            }],
            truncated: false,
        };
        sample.include_observed_ships();
        sample
    }
}

fn read_notices(conn: &Connection, home: &Path, pending: bool) -> Result<Vec<Notice>> {
    let join = if pending {
        "JOIN pending p ON p.seq=e.seq"
    } else {
        ""
    };
    let sql = format!(
        "SELECT e.seq,substr(e.task_id,1,160),substr(e.issue_key,1,160),substr(e.kind,1,80),substr(COALESCE(json_extract(e.payload,'$.summary'),json_extract(e.payload,'$.body'),'Details available through event.history'),1,2048),substr(t.owner,1,160) FROM events e {join} JOIN tasks t ON t.id=e.task_id ORDER BY e.seq DESC LIMIT 201"
    );
    let mut q = conn.prepare(&sql)?;
    Ok(q.query_map([], |r| {
        Ok(Notice {
            home: Some(home.to_path_buf()),
            owner: r.get(5)?,
            seq: r.get(0)?,
            task_id: r.get(1)?,
            issue_key: r.get(2)?,
            kind: r.get(3)?,
            summary: r.get(4)?,
        })
    })?
    .collect::<rusqlite::Result<Vec<_>>>()?)
}
