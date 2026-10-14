use anyhow::{Result, ensure};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    time::{Duration, Instant},
};

use crate::store::reject_symlinks;

pub const ROW_LIMIT: usize = 200;

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
    pub id: String,
    pub owner: String,
    pub title: String,
    pub status: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Notice {
    pub seq: i64,
    pub task_id: String,
    pub issue_key: String,
    pub kind: String,
    pub summary: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Job {
    pub id: String,
    pub task_id: String,
    pub status: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Snapshot {
    pub tasks: Vec<Task>,
    pub pending: Vec<Notice>,
    pub activity: Vec<Notice>,
    pub jobs: Vec<Job>,
    pub truncated: bool,
}

impl Snapshot {
    /// Open an existing home with SQLite READ_ONLY, avoiding Store::open's
    /// migrations/chmod and job.list's recovery side effects. No resource ledger.
    pub fn read(home: &Path) -> Result<Self> {
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
            let mut q = tx.prepare("SELECT substr(id,1,160),substr(owner,1,160),substr(title,1,512),status FROM tasks ORDER BY CASE WHEN status IN ('running','blocked','open') THEN 0 ELSE 1 END,id LIMIT 201")?;
            for row in q.query_map([], |r| {
                Ok(Task {
                    id: r.get(0)?,
                    owner: r.get(1)?,
                    title: r.get(2)?,
                    status: r.get(3)?,
                })
            })? {
                snapshot.tasks.push(row?);
            }
        }
        snapshot.pending = read_notices(&tx, true)?;
        snapshot.activity = read_notices(&tx, false)?;
        {
            // Stored outcomes only. We do not probe processes or settle jobs.
            let mut q = tx.prepare("SELECT substr(id,1,160),substr(json_extract(record,'$.task_id'),1,160),substr(json_extract(record,'$.status'),1,40) FROM jobs ORDER BY rowid DESC LIMIT 201")?;
            for row in q.query_map([], |r| {
                Ok(Job {
                    id: r.get(0)?,
                    task_id: r.get(1)?,
                    status: r.get(2)?,
                })
            })? {
                snapshot.jobs.push(row?);
            }
        }
        tx.commit()?;
        snapshot.truncated = snapshot.tasks.len() > ROW_LIMIT
            || snapshot.pending.len() > ROW_LIMIT
            || snapshot.activity.len() > ROW_LIMIT
            || snapshot.jobs.len() > ROW_LIMIT;
        snapshot.tasks.truncate(ROW_LIMIT);
        snapshot.pending.truncate(ROW_LIMIT);
        snapshot.activity.truncate(ROW_LIMIT);
        snapshot.jobs.truncate(ROW_LIMIT);
        Ok(snapshot)
    }

    pub fn demo() -> Self {
        Self {
            tasks: vec![
                Task {
                    id: "navigation".into(),
                    owner: "navigator".into(),
                    title: "Coordinate the next voyage".into(),
                    status: "running".into(),
                },
                Task {
                    id: "app-build".into(),
                    owner: "apps".into(),
                    title: "Investigate the failing build".into(),
                    status: "running".into(),
                },
                Task {
                    id: "route-check".into(),
                    owner: "infrastructure".into(),
                    title: "Check the provider route".into(),
                    status: "blocked".into(),
                },
            ],
            pending: vec![Notice {
                seq: 50,
                task_id: "route-check".into(),
                issue_key: "connection".into(),
                kind: "blocked".into(),
                summary: "Repeated connection facts appear as one pending issue".into(),
            }],
            activity: vec![Notice {
                seq: 51,
                task_id: "app-build".into(),
                issue_key: "tests".into(),
                kind: "outcome".into(),
                summary: "Sample logbook outcome — demonstration data only".into(),
            }],
            jobs: vec![Job {
                id: "build-1".into(),
                task_id: "app-build".into(),
                status: "running".into(),
            }],
            truncated: false,
        }
    }
}

fn read_notices(conn: &Connection, pending: bool) -> Result<Vec<Notice>> {
    let join = if pending {
        "JOIN pending p ON p.seq=e.seq"
    } else {
        ""
    };
    let sql = format!(
        "SELECT e.seq,substr(e.task_id,1,160),substr(e.issue_key,1,160),substr(e.kind,1,80),substr(COALESCE(json_extract(e.payload,'$.summary'),json_extract(e.payload,'$.body'),'Details available through event.history'),1,2048) FROM events e {join} ORDER BY e.seq DESC LIMIT 201"
    );
    let mut q = conn.prepare(&sql)?;
    Ok(q.query_map([], |r| {
        Ok(Notice {
            seq: r.get(0)?,
            task_id: r.get(1)?,
            issue_key: r.get(2)?,
            kind: r.get(3)?,
            summary: r.get(4)?,
        })
    })?
    .collect::<rusqlite::Result<Vec<_>>>()?)
}
