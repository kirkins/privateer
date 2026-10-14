use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct Store {
    home: PathBuf,
    connection: Connection,
}

impl Store {
    pub fn open(home: &Path) -> Result<Self> {
        if !home.is_absolute() {
            bail!("home must be an absolute path");
        }
        reject_symlinks(home)?;
        fs::create_dir_all(home).context("create Privateer home")?;
        fs::set_permissions(home, fs::Permissions::from_mode(0o700))?;
        let home = fs::canonicalize(home)?;
        let database = home.join("privateer.sqlite");
        reject_symlinks(&database)?;
        let connection = Connection::open(&database)?;
        fs::set_permissions(&database, fs::Permissions::from_mode(0o600))?;
        connection.busy_timeout(Duration::from_secs(10))?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS charters(id TEXT PRIMARY KEY,data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS tasks(id TEXT PRIMARY KEY,charter_id TEXT NOT NULL REFERENCES charters(id),
              owner TEXT NOT NULL,title TEXT NOT NULL,status TEXT NOT NULL,parent TEXT REFERENCES tasks(id),
              depth INTEGER NOT NULL,handoff_to TEXT);
            CREATE TABLE IF NOT EXISTS events(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,
              task_id TEXT NOT NULL REFERENCES tasks(id),issue_key TEXT NOT NULL,kind TEXT NOT NULL,payload TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS event_task ON events(task_id,seq);
            CREATE TABLE IF NOT EXISTS pending(task_id TEXT NOT NULL,issue_key TEXT NOT NULL,seq INTEGER NOT NULL REFERENCES events(seq),
              PRIMARY KEY(task_id,issue_key));
            CREATE TABLE IF NOT EXISTS audit(seq INTEGER PRIMARY KEY AUTOINCREMENT,time_ms INTEGER NOT NULL,
              kind TEXT NOT NULL,payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS approvals(id TEXT PRIMARY KEY,task_id TEXT NOT NULL REFERENCES tasks(id),
              artifact TEXT NOT NULL,environment TEXT NOT NULL,charter_hash TEXT NOT NULL,consumed INTEGER NOT NULL DEFAULT 0);")?;
        connection.execute("INSERT OR IGNORE INTO metadata VALUES('schema','1')", [])?;
        let schema: String =
            connection.query_row("SELECT value FROM metadata WHERE key='schema'", [], |r| {
                r.get(0)
            })?;
        if schema != "1" {
            bail!("unsupported state schema {schema}");
        }
        let store = Self { home, connection };
        crate::jobs::init(&store)?;
        crate::peers::init(&store)?;
        Ok(store)
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }
    /// Read-modify-write transaction. IMMEDIATE takes the write lock up front so
    /// concurrent writers wait on busy_timeout instead of failing on upgrade.
    pub fn write_transaction(&self) -> Result<rusqlite::Transaction<'_>> {
        Ok(rusqlite::Transaction::new_unchecked(
            &self.connection,
            rusqlite::TransactionBehavior::Immediate,
        )?)
    }
    pub fn home(&self) -> &Path {
        &self.home
    }
    pub fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(i64::MAX as u128) as i64
    }
    pub fn audit(&self, kind: &str, payload: &Value) -> Result<i64> {
        self.connection.execute(
            "INSERT INTO audit(time_ms,kind,payload) VALUES(?1,?2,?3)",
            params![Self::now_ms(), kind, serde_json::to_string(payload)?],
        )?;
        Ok(self.connection.last_insert_rowid())
    }
    pub fn charter(&self, id: &str) -> Result<Value> {
        let data: Option<String> = self
            .connection
            .query_row("SELECT data FROM charters WHERE id=?1", [id], |r| r.get(0))
            .optional()?;
        serde_json::from_str(&data.ok_or_else(|| anyhow::anyhow!("unknown charter {id}"))?)
            .context("corrupt charter record")
    }
    pub fn task(&self, id: &str) -> Result<Value> {
        self.connection.query_row("SELECT id,charter_id,owner,title,status,parent,depth,handoff_to FROM tasks WHERE id=?1", [id], |r| {
            Ok(json!({"id":r.get::<_,String>(0)?,"charter_id":r.get::<_,String>(1)?,"owner":r.get::<_,String>(2)?,
                "title":r.get::<_,String>(3)?,"status":r.get::<_,String>(4)?,"parent":r.get::<_,Option<String>>(5)?,
                "depth":r.get::<_,i64>(6)?,"handoff_to":r.get::<_,Option<String>>(7)?}))
        }).optional()?.ok_or_else(|| anyhow::anyhow!("unknown task {id}"))
    }
    pub fn authorize(&self, task_id: &str, action: &str) -> Result<Value> {
        self.authorize_inner(task_id, action, false)
    }
    /// Completion prevents admission of new work. A job already running may
    /// finish recording its result; cancellation and revocation still stop it.
    pub fn authorize_running(&self, task_id: &str, action: &str) -> Result<Value> {
        self.authorize_inner(task_id, action, true)
    }
    fn authorize_inner(&self, task_id: &str, action: &str, running: bool) -> Result<Value> {
        let task = self.task(task_id)?;
        if task["status"] == "cancelled" || (!running && task["status"] == "done") {
            bail!("task is terminal");
        }
        let mut current = task.clone();
        while let Some(parent) = current["parent"].as_str() {
            current = self.task(parent)?;
            if current["status"] == "cancelled" {
                bail!("parent task was cancelled");
            }
        }
        let charter = self.charter(task["charter_id"].as_str().unwrap())?;
        if charter["active"] != true {
            bail!("charter was revoked");
        }
        if !charter["actions"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(action)))
        {
            bail!("action {action} is outside this charter");
        }
        Ok(charter)
    }
}

pub fn reject_symlinks(path: &Path) -> Result<()> {
    for parent in path.ancestors() {
        if let Ok(meta) = fs::symlink_metadata(parent) {
            // macOS exposes standard temporary/configuration roots through these
            // OS-owned aliases. Nested application/state symlinks remain refused.
            let os_alias = cfg!(target_os = "macos")
                && [Path::new("/tmp"), Path::new("/var"), Path::new("/etc")].contains(&parent);
            if meta.file_type().is_symlink() && !os_alias {
                bail!("symlink path is not allowed: {}", parent.display());
            }
        }
    }
    Ok(())
}

pub fn identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:@".contains(&b))
    {
        bail!("identifier must contain 1–160 ASCII letters, digits, or -_.:@");
    }
    Ok(())
}

pub fn required<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{name} is required"))
}
