use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(test)]
thread_local! {
    /// Write transactions this thread has opened, so a test can tell a pass
    /// that took the write lock from one that only read.
    pub(crate) static WRITE_TRANSACTIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

// Only these known lifecycle refusals may be omitted from an observation.
// Parse, SQL, missing-record and invalid-ancestry errors must remain unavailable.
#[derive(Debug)]
struct InactiveAuthority(&'static str);

impl std::fmt::Display for InactiveAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for InactiveAuthority {}

pub struct Store {
    home: PathBuf,
    connection: Connection,
    caller: Caller,
    crew: Option<crate::crew_scope::CrewCaller>,
}

/// Who is issuing commands. Agents launched by Privateer run with a role
/// marker that refuses operator commands and binds actor names to their own
/// identity. This guards against confused agents; it is a workflow control
/// under one OS account, not a security boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caller {
    Operator,
    Agent { actor: String },
}

impl Caller {
    pub fn from_env(flag_role: Option<&str>, flag_actor: Option<&str>) -> Result<Self> {
        Self::from_env_at(None, flag_role, flag_actor, false)
    }

    /// The caller of a command on `home`. Inside a supervised job the job's
    /// records decide: a job that runs as an agent cannot drop its role, and
    /// a crew is narrowed to crew commands (see `crew_scope`). `supervisor`
    /// says the command is `_job-supervise` itself.
    pub fn from_env_at(
        home: Option<&Path>,
        flag_role: Option<&str>,
        flag_actor: Option<&str>,
        supervisor: bool,
    ) -> Result<Self> {
        let env = |name: &str| -> Result<Option<String>> {
            match std::env::var(name) {
                Ok(value) => Ok(Some(value)),
                Err(std::env::VarError::NotPresent) => Ok(None),
                Err(std::env::VarError::NotUnicode(_)) => bail!("{name} must be valid UTF-8"),
            }
        };
        let caller = Self::resolve(
            env("PRIVATEER_ROLE")?.as_deref(),
            env("PRIVATEER_ACTOR")?.as_deref(),
            flag_role,
            flag_actor,
        )?;
        crate::crew_scope::resolve(home, caller, supervisor)
    }

    pub fn require_operator(&self, command: &str) -> Result<()> {
        if matches!(self, Self::Agent { .. }) {
            bail!("{command} is an operator command; agents cannot run it");
        }
        Ok(())
    }

    /// Combine the role marker from the environment (`PRIVATEER_ROLE`,
    /// `PRIVATEER_ACTOR`) with optional CLI flags. Either source can narrow the
    /// caller to an agent; neither can widen an agent back to an operator.
    pub fn resolve(
        env_role: Option<&str>,
        env_actor: Option<&str>,
        flag_role: Option<&str>,
        flag_actor: Option<&str>,
    ) -> Result<Self> {
        let mut agent = false;
        for role in [env_role, flag_role].into_iter().flatten() {
            match role {
                "agent" => agent = true,
                "operator" => {}
                _ => bail!("role must be operator or agent"),
            }
        }
        let actor = match (env_actor, flag_actor) {
            (Some(a), Some(b)) if a != b => bail!("conflicting actor identities"),
            (a, b) => a.or(b),
        };
        if !agent {
            return Ok(Self::Operator);
        }
        let actor = actor.context("agent role requires an actor identity")?;
        identifier(actor)?;
        Ok(Self::Agent {
            actor: actor.to_owned(),
        })
    }
}

impl Store {
    /// Observation-only commands must not initialize or migrate a home.
    pub fn open_readonly(home: &Path, caller: Caller) -> Result<Self> {
        caller.require_operator("read-only captain observation")?;
        if !home.is_absolute() {
            bail!("home must be an absolute path");
        }
        reject_symlinks(home)?;
        let home = fs::canonicalize(home).context("existing Privateer home required")?;
        let database = home.join("privateer.sqlite");
        reject_symlinks(&database)?;
        let connection =
            Connection::open_with_flags(&database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .context("existing Privateer database required")?;
        // Observation-only open must not inherit ordinary writers' ten-second
        // lock wait. The query installs its own scoped VM/deadline budget.
        connection.busy_timeout(Duration::from_millis(25))?;
        connection.execute_batch("PRAGMA query_only=ON;")?;
        Ok(Self {
            home,
            connection,
            caller,
            crew: None,
        })
    }

    /// Explicit local operator intent only. Unlike normal startup, never create,
    /// migrate, chmod, repair, or change journal mode. Callers pin and revalidate
    /// filesystem identity and origin around this open and before dispatch.
    #[cfg(feature = "dashboard")]
    pub(crate) fn open_existing_action(home: &Path, caller: Caller) -> Result<Self> {
        caller.require_operator("existing-home captain action")?;
        if !home.is_absolute() {
            bail!("home must be absolute");
        }
        reject_symlinks(home)?;
        let home = fs::canonicalize(home)?;
        let database = home.join("privateer.sqlite");
        reject_symlinks(&database)?;
        let connection =
            Connection::open_with_flags(&database, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(Duration::from_millis(250))?;
        let schema: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM metadata WHERE key='schema' AND value='1') AND EXISTS(SELECT 1 FROM metadata WHERE key='runtime_origin' AND length(value) BETWEEN 1 AND 160) AND (SELECT COUNT(*) FROM pragma_table_info('mail') WHERE name IN ('ended_by','ended_ms','end_reason'))=3 AND EXISTS(SELECT 1 FROM pragma_table_info('approvals') WHERE name='approver')", [], |r| r.get(0))?;
        // A captain's tap may record an approval, whose table this open
        // cannot migrate: one from before block cards' approvals is refused
        // here rather than by its check mid-tap.
        let approvals: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('captain_approvals') WHERE name='applies_at')
               AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='captain_approvals'
                 AND instr(sql, '''block''') > 0)",
            [],
            |r| r.get(0),
        )?;
        if !schema || !approvals {
            bail!("incompatible existing action schema; use normal upgrade workflow");
        }
        let mode: String = connection.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        if mode != "wal" {
            bail!("existing action requires the normal WAL schema; no repair performed");
        }
        connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")?;
        Ok(Self {
            home,
            connection,
            caller,
            crew: None,
        })
    }

    pub fn open_as(home: &Path, caller: Caller) -> Result<Self> {
        if matches!(caller, Caller::Agent { .. }) && !home.join("privateer.sqlite").is_file() {
            bail!("agents require an existing ship home");
        }
        let mut store = Self::open(home)?;
        store.set_caller(caller);
        store.crew = crate::crew_scope::crew();
        Ok(store)
    }

    pub fn open(home: &Path) -> Result<Self> {
        if !home.is_absolute() {
            bail!("home must be an absolute path");
        }
        reject_symlinks(home)?;
        private_dirs(home).context("create Privateer home")?;
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
            CREATE INDEX IF NOT EXISTS audit_time ON audit(time_ms,seq);
            CREATE TABLE IF NOT EXISTS approvals(id TEXT PRIMARY KEY,task_id TEXT NOT NULL REFERENCES tasks(id),
              artifact TEXT NOT NULL,environment TEXT NOT NULL,charter_hash TEXT NOT NULL,consumed INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS captain_approvals(id TEXT PRIMARY KEY,ship TEXT NOT NULL,
              task_id TEXT NOT NULL REFERENCES tasks(id),request_id TEXT NOT NULL,reply_id TEXT NOT NULL UNIQUE,
              action TEXT NOT NULL,command_prefix TEXT,created_ms INTEGER NOT NULL,expires_ms INTEGER NOT NULL,
              revoked_ms INTEGER,source TEXT NOT NULL CHECK(source IN ('reply','option','block')));
            CREATE INDEX IF NOT EXISTS captain_approvals_ship ON captain_approvals(ship,expires_ms);")?;
        // Columns added after 0.1.0. Check and add them under the write lock
        // so concurrent opens cannot both add one.
        let migrate = rusqlite::Transaction::new_unchecked(
            &connection,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        for (table, column, declaration) in [
            ("approvals", "approver", "TEXT"),
            // Events written before this column existed have no time and are
            // treated as older than any retention cutoff.
            ("events", "time_ms", "INTEGER"),
            // A ship's backlog order: open tasks with a rank start first.
            ("tasks", "rank", "INTEGER"),
            // The ships a task is shared with, as JSON: empty (NULL) unless the
            // owner sets it, so nothing becomes visible on upgrade.
            ("tasks", "shared_with", "TEXT"),
            // Where a captain approval was told it applies: relaunch, wake or
            // next_launch.
            ("captain_approvals", "applies_at", "TEXT"),
        ] {
            let present: bool = migrate.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name=?2)",
                [table, column],
                |r| r.get(0),
            )?;
            if !present {
                migrate.execute_batch(&format!(
                    "ALTER TABLE {table} ADD COLUMN {column} {declaration};"
                ))?;
            }
        }
        // A block card's tap records an approval with source 'block', which
        // the table's first check refused. SQLite cannot change a check, so
        // the table is rebuilt once, rows and all.
        let approvals: String = migrate.query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='captain_approvals'",
            [],
            |r| r.get(0),
        )?;
        if !approvals.contains("'block'") {
            migrate.execute_batch(
                "CREATE TABLE captain_approvals_next(id TEXT PRIMARY KEY,ship TEXT NOT NULL,
                   task_id TEXT NOT NULL REFERENCES tasks(id),request_id TEXT NOT NULL,reply_id TEXT NOT NULL UNIQUE,
                   action TEXT NOT NULL,command_prefix TEXT,created_ms INTEGER NOT NULL,expires_ms INTEGER NOT NULL,
                   revoked_ms INTEGER,source TEXT NOT NULL CHECK(source IN ('reply','option','block')),
                   applies_at TEXT);
                 INSERT INTO captain_approvals_next SELECT id,ship,task_id,request_id,reply_id,action,
                   command_prefix,created_ms,expires_ms,revoked_ms,source,applies_at FROM captain_approvals;
                 DROP TABLE captain_approvals;
                 ALTER TABLE captain_approvals_next RENAME TO captain_approvals;
                 CREATE INDEX captain_approvals_ship ON captain_approvals(ship,expires_ms);",
            )?;
        }
        // Inbox reads look messages up by recipient instead of scanning events;
        // `fleet.snapshot` finds a ship's open tasks by owner and status.
        migrate.execute_batch(
            "CREATE INDEX IF NOT EXISTS event_recipient ON events(json_extract(payload,'$.to'),seq) WHERE kind='message';
             CREATE INDEX IF NOT EXISTS tasks_owner ON tasks(owner, status);",
        )?;
        migrate.commit()?;
        connection.execute("INSERT OR IGNORE INTO metadata VALUES('schema','1')", [])?;
        connection.execute(
            "INSERT OR IGNORE INTO metadata VALUES('runtime_origin',?1)",
            [uuid::Uuid::new_v4().to_string()],
        )?;
        let schema: String =
            connection.query_row("SELECT value FROM metadata WHERE key='schema'", [], |r| {
                r.get(0)
            })?;
        if schema != "1" {
            bail!("unsupported state schema {schema}");
        }
        let store = Self {
            home,
            connection,
            caller: Caller::Operator,
            crew: None,
        };
        crate::jobs::init(&store)?;
        crate::workspace::init(&store)?;
        crate::peers::init(&store)?;
        crate::mail::init(&store)?;
        crate::report::init(&store)?;
        crate::files::init(&store)?;
        crate::bundled::migrate(&store)?;
        crate::activation::init(&store)?;
        crate::session::init(&store)?;
        crate::errand::init(&store)?;
        crate::review::init(&store)?;
        crate::context::init(&store)?;
        crate::attention::init(&store)?;
        crate::presence::init(&store)?;
        crate::scheduling::init(&store)?;
        crate::block_ask::init(&store)?;
        crate::waits::init(&store)?;
        crate::readiness::init(&store)?;
        Ok(store)
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }
    pub fn caller(&self) -> &Caller {
        &self.caller
    }
    /// The crew this CLI caller was resolved to, if any.
    pub fn crew(&self) -> Option<&crate::crew_scope::CrewCaller> {
        self.crew.as_ref()
    }
    /// The runtime acting for a job, such as its supervisor resuming a parked
    /// crew, is not held to the crew's commands.
    pub fn as_runtime(&mut self) {
        self.crew = None;
    }
    pub fn set_caller(&mut self, caller: Caller) {
        self.caller = caller;
    }
    /// Read-modify-write transaction. IMMEDIATE takes the write lock up front so
    /// concurrent writers wait on busy_timeout instead of failing on upgrade.
    pub fn write_transaction(&self) -> Result<rusqlite::Transaction<'_>> {
        #[cfg(test)]
        WRITE_TRANSACTIONS.with(|count| count.set(count.get() + 1));
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
        let read = |shared_with: &str| {
            self.connection.query_row(&format!("SELECT id,charter_id,owner,title,status,parent,depth,handoff_to,{shared_with} FROM tasks WHERE id=?1"), [id], |r| {
                // A shared_with column the runtime cannot parse denies everyone,
                // which is the safe direction for a visibility list.
                let shared: Option<String> = r.get(8)?;
                Ok(json!({"id":r.get::<_,String>(0)?,"charter_id":r.get::<_,String>(1)?,"owner":r.get::<_,String>(2)?,
                    "title":r.get::<_,String>(3)?,"status":r.get::<_,String>(4)?,"parent":r.get::<_,Option<String>>(5)?,
                    "depth":r.get::<_,i64>(6)?,"handoff_to":r.get::<_,Option<String>>(7)?,
                    "shared_with": shared.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(json!([]))}))
            }).optional()
        };
        // A home opened without migrating (the dashboard's open) may predate
        // shared_with: there the task is shared with nobody.
        let task = match read("shared_with") {
            Err(error) if error.to_string().contains("no such column: shared_with") => read("NULL"),
            read => read,
        };
        task?.ok_or_else(|| anyhow::anyhow!("unknown task {id}"))
    }
    pub fn authorize(&self, task_id: &str, action: &str) -> Result<Value> {
        self.authorize_inner(task_id, Some(action), false)
    }
    /// The effective charter of an open task, without requiring any charter
    /// action. For decisions that grant no new work by themselves, such as
    /// accepting custody or recording an explicit approval.
    pub fn active_charter(&self, task_id: &str) -> Result<Value> {
        self.authorize_inner(task_id, None, false)
    }
    /// Use exactly the grant's authority/hash semantics, with observation-only
    /// checkpoints across short SQL statements and Rust ancestry work. Only
    /// explicit lifecycle refusals are stale; all invalid source errors survive.
    pub(crate) fn observe_active_charter(
        &self,
        task_id: &str,
        mut checkpoint: impl FnMut(&str) -> Result<()>,
    ) -> Result<Option<Value>> {
        match self.authorize_checked(task_id, None, false, &mut checkpoint) {
            Ok(charter) => Ok(Some(charter)),
            Err(error) if error.is::<InactiveAuthority>() => Ok(None),
            Err(error) => Err(error),
        }
    }
    /// Completion prevents admission of new work. A job already running may
    /// finish recording its result; cancellation and revocation still stop it.
    pub fn authorize_running(&self, task_id: &str, action: &str) -> Result<Value> {
        self.authorize_inner(task_id, Some(action), true)
    }
    fn authorize_inner(&self, task_id: &str, action: Option<&str>, running: bool) -> Result<Value> {
        self.authorize_checked(task_id, action, running, &mut |_| Ok(()))
    }
    fn authorize_checked(
        &self,
        task_id: &str,
        action: Option<&str>,
        running: bool,
        checkpoint: &mut impl FnMut(&str) -> Result<()>,
    ) -> Result<Value> {
        checkpoint(task_id)?;
        let task = self.task(task_id)?;
        if task["status"] == "cancelled" || (!running && task["status"] == "done") {
            return Err(InactiveAuthority("task is terminal").into());
        }
        let mut current = task.clone();
        while let Some(parent) = current["parent"].as_str() {
            checkpoint(parent)?;
            current = self.task(parent)?;
            if current["status"] == "cancelled" {
                return Err(InactiveAuthority("parent task was cancelled").into());
            }
        }
        let charter = self.charter(task["charter_id"].as_str().unwrap())?;
        let active = charter["active"]
            .as_bool()
            .context("invalid charter active flag")?;
        if !active {
            return Err(InactiveAuthority("charter was revoked").into());
        }
        if let Some(action) = action {
            if !charter["actions"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(action)))
            {
                bail!("action {action} is outside this charter");
            }
        }
        Ok(charter)
    }
}

/// Make `path` and any missing parents 0700. The umask can only narrow the
/// mode a builder asks for, so a folder made here passes the checks that refuse
/// one others can write (`create_dir_all` under umask 002 makes it 0775).
/// A folder that already exists is left as it is, for those checks to judge.
pub fn private_dirs(path: &Path) -> std::io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
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
