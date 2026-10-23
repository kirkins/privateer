//! Front doors: the captain's conversation, carried to wherever the captain is.
//!
//! A front door is a long-running trusted adapter, such as a chat service, a
//! phone app or voice, that the captain registers once for the whole fleet.
//! `privateer door run NAME` supervises it and owns the conversation's state:
//! which captain mail the door has carried, and which ship's home a message
//! from the captain belongs in. The adapter only translates; it owns no fleet
//! state. Registering a door is an operator act, because a door speaks as the
//! captain.
//!
//! Protocol version 1 is JSON lines over the adapter's stdin and stdout.
//!
//! Runtime to door:
//! - `{"type":"hello","version":1,"door":NAME,"config_path":PATH|null,"state_dir":PATH,"recipient":SHIP|null}`, once, first;
//! - `{"type":"mail","ref":REF,"ship":SHIP,"mail":{...}}` for each message to the captain, in order per ship;
//! - `{"type":"response","id":ID,"ok":true,"result":{...}}`, or `"ok":false,"error":TEXT`.
//!
//! Door to runtime:
//! - `{"type":"delivered","ref":REF}`: the captain has it; it is not sent again;
//! - `{"type":"request","id":ID,"command":"send|ships|status|waiting|file.put|approval","args":{...}}`.
//!   `approval {id, action: "grant"|"deny", hash?}` answers a privileged
//!   command held in a home on this machine, as the captain, while its hash
//!   starts with `hash` (what the phone showed); nothing else.
//!   `file.put {to, name, content_file}` stores a file the captain sent in the
//!   receiving ship's own home, on this machine or a linked one, and answers
//!   with its `file_id` for the mail's `payload.files`.
//!
//! Mail is delivered at least once: when the adapter restarts, everything
//! after its last `delivered` is sent again, so a door dedupes by mail id.
//! A door exits when its stdin closes.
use crate::store::{Store, identifier};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

pub const KIND: &str = "front-door";
const VERSION: u64 = 1;
const MAX_LINE: usize = 1024 * 1024;
const LOG_LIMIT: u64 = 1024 * 1024;
const QUEUE: usize = 256;
const POLL: Duration = Duration::from_secs(1);
const PAGE: i64 = 20;
const USAGE: &str = "usage: privateer door add NAME [--manifest PATH] [--config PATH] [--to SHIP] | list | start NAME | stop NAME | status [NAME] | remove NAME | run NAME";

#[derive(Deserialize)]
struct Manifest {
    schema_version: u32,
    id: String,
    #[serde(default)]
    kind: Option<String>,
    executable: Vec<String>,
    /// The adapter cannot run without private configuration.
    #[serde(default)]
    config_required: bool,
    /// The adapter renders `digest` events; others hold notices while away.
    #[serde(default)]
    digests: bool,
}

struct Door {
    name: String,
    manifest: PathBuf,
    config: Option<PathBuf>,
    recipient: Option<String>,
}

/// A door's own directory: its supervisor lock and logs, and `state/` for
/// the adapter. It sits beside the fleet registry, so a test fleet's doors
/// stay in that fleet.
pub fn directory(name: &str) -> Result<PathBuf> {
    identifier(name)?;
    let registry = crate::fleet::registry_path()?;
    let doors = registry
        .parent()
        .context("the fleet registry has no directory")?
        .join("doors");
    let dir = doors.join(name);
    for path in [&doors, &dir, &dir.join("state")] {
        crate::store::private_dirs(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

fn table(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS doors(name TEXT PRIMARY KEY, manifest TEXT NOT NULL,
           config TEXT, recipient TEXT, created_ms INTEGER NOT NULL);",
    )?;
    Ok(())
}

fn doors() -> Result<Vec<Door>> {
    let conn = crate::fleet::registry()?;
    table(&conn)?;
    let mut statement =
        conn.prepare("SELECT name, manifest, config, recipient FROM doors ORDER BY name")?;
    let rows = statement.query_map([], |r| {
        Ok(Door {
            name: r.get(0)?,
            manifest: PathBuf::from(r.get::<_, String>(1)?),
            config: r.get::<_, Option<String>>(2)?.map(PathBuf::from),
            recipient: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Every registered front door's name.
pub(crate) fn names() -> Result<Vec<String>> {
    Ok(doors()?.into_iter().map(|d| d.name).collect())
}

/// Registered doors whose adapters take away digests.
pub(crate) fn digest_doors() -> Result<Vec<String>> {
    Ok(doors()?
        .into_iter()
        .filter(|d| load_manifest(&d.manifest).is_ok_and(|m| m.digests))
        .map(|d| d.name)
        .collect())
}

/// Move a door's delivery cursor in one home forward to `through`, as if
/// the captain had been sent everything up to it.
pub(crate) fn advance(store: &Store, door: &str, through: i64) -> Result<()> {
    cursor(store, door)?;
    store.connection().execute(
        "UPDATE door_cursor SET through_seq=MAX(through_seq, ?2) WHERE door=?1",
        params![door, through],
    )?;
    Ok(())
}

fn registration(name: &str) -> Result<Option<Door>> {
    Ok(doors()?.into_iter().find(|d| d.name == name))
}

fn load_manifest(path: &Path) -> Result<Manifest> {
    ensure!(path.is_absolute(), "a door manifest path must be absolute");
    ensure!(path.is_file(), "no door manifest at {}", path.display());
    let raw = fs::read(path).context("cannot read the door manifest")?;
    ensure!(raw.len() <= 65_536, "door manifest too large");
    let manifest: Manifest = serde_json::from_slice(&raw).context("invalid door manifest")?;
    ensure!(
        manifest.schema_version == 1,
        "unsupported door manifest version"
    );
    ensure!(
        manifest.kind.as_deref() == Some(KIND),
        "{} is not a front-door plugin (its manifest needs \"kind\": \"{KIND}\")",
        path.display()
    );
    crate::plugins::valid_identity(&manifest.id, &manifest.executable)?;
    Ok(manifest)
}

fn absolute(path: &str) -> Result<PathBuf> {
    let path = Path::new(path);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    fs::canonicalize(&path).with_context(|| format!("{} does not exist", path.display()))
}

/// Whether a ship home was set up as the captain's liaison.
pub(crate) fn is_liaison(home: &Path) -> bool {
    let mut raw = Vec::new();
    let read =
        fs::File::open(home.join("config.json")).and_then(|f| f.take(65_537).read_to_end(&mut raw));
    read.is_ok()
        && raw.len() <= 65_536
        && serde_json::from_slice::<Value>(&raw).is_ok_and(|c| c["preset"] == "liaison")
}

/// Active fleet members whose folders still exist.
fn members() -> Result<Vec<(String, PathBuf)>> {
    let conn = crate::fleet::registry()?;
    Ok(crate::fleet::members(&conn)?
        .into_iter()
        .filter(|m| m.home.is_dir())
        .map(|m| (m.name, m.home))
        .collect())
}

/// Where the captain's messages go when they name no ship: the door's own
/// choice, else the fleet's one liaison, else a fleet's only ship.
fn recipient(door: &Door, members: &[(String, PathBuf)]) -> Option<String> {
    if let Some(ship) = &door.recipient {
        return Some(ship.clone());
    }
    let liaisons: Vec<_> = members.iter().filter(|(_, h)| is_liaison(h)).collect();
    match (liaisons.as_slice(), members) {
        ([(ship, _)], _) => Some(ship.clone()),
        ([], [(ship, _)]) => Some(ship.clone()),
        _ => None,
    }
}

/// The door's delivery cursor in one home: captain mail through this
/// sequence has reached the captain through this door. A new door starts at
/// what the captain has already acknowledged, so unread mail still arrives.
fn cursor(store: &Store, door: &str) -> Result<i64> {
    let tx = store.write_transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS door_cursor(door TEXT PRIMARY KEY, through_seq INTEGER NOT NULL);",
    )?;
    let current: Option<i64> = tx
        .query_row(
            "SELECT through_seq FROM door_cursor WHERE door=?1",
            [door],
            |r| r.get(0),
        )
        .optional()?;
    let through = match current {
        Some(through) => through,
        None => {
            let read: i64 = tx.query_row(
                "SELECT COALESCE((SELECT through_seq FROM mail_cursor WHERE address=?1), 0)",
                [crate::mail::CAPTAIN],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO door_cursor(door, through_seq) VALUES(?1, ?2)",
                params![door, read],
            )?;
            read
        }
    };
    tx.commit()?;
    Ok(through)
}

fn log_line(path: &Path, text: &str) {
    if fs::metadata(path).is_ok_and(|m| m.len() > LOG_LIMIT) {
        let _ = fs::rename(path, path.with_extension("log.1"));
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} {text}", Store::now_ms());
    }
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn request_stop(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// Run a front door until stopped: start its adapter, carry the captain's
/// mail both ways, and restart the adapter with backoff when it exits.
/// One supervisor runs per door, guarded by `<door dir>/lock`.
pub fn supervise(name: &str) -> Result<()> {
    let dir = directory(name)?;
    let _ = std::env::set_current_dir(&dir);
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("lock"))?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "the front door {name} is already running"
    );
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        unsafe { libc::signal(signal, request_stop as *const () as libc::sighandler_t) };
    }
    let log = dir.join("door.log");
    let mut backoff = 1;
    while !STOP.load(Ordering::SeqCst) {
        let Some(door) = registration(name)? else {
            log_line(
                &log,
                "supervisor: the door is no longer registered; stopping",
            );
            return Ok(());
        };
        let started = Instant::now();
        let outcome = Session::start(&door, &dir).and_then(|mut session| session.run());
        let note = match outcome {
            Ok(note) => note,
            Err(error) => format!("{error:#}"),
        };
        log_line(&log, &format!("supervisor: {note}"));
        eprintln!("privateer door {name}: {note}");
        if started.elapsed() > Duration::from_secs(60) {
            backoff = 1;
        }
        let resume = Instant::now() + Duration::from_secs(backoff);
        while !STOP.load(Ordering::SeqCst) && Instant::now() < resume {
            thread::sleep(Duration::from_millis(100));
        }
        backoff = (backoff * 2).min(60);
    }
    log_line(&log, "supervisor: stopped");
    Ok(())
}

enum Incoming {
    Line(Vec<u8>),
    TooLong,
    End,
}

struct Session<'a> {
    door: &'a Door,
    dir: PathBuf,
    child: Child,
    pid: u32,
    to_door: Option<SyncSender<String>>,
    from_door: Receiver<Incoming>,
    stores: HashMap<PathBuf, Store>,
    homes: HashMap<String, PathBuf>,
    /// Captain mail sent to the adapter this session, per ship.
    sent: HashMap<String, i64>,
    /// The latest mail the adapter was actually sent this session, per ship;
    /// unlike `sent`, a return does not reset it, so a late acknowledgement
    /// of mail sent at once while away is still accepted.
    carried: HashMap<String, i64>,
    failing: HashSet<String>,
    /// When the away period this session last saw began.
    away_since: Option<i64>,
    /// The adapter renders digests.
    digests: bool,
    /// Digests and catch-ups sent to the adapter this session.
    emitted: HashSet<String>,
    /// When to look for held mail again after finding none due.
    digest_check: Option<Instant>,
    /// The captain asked for a digest now (`/digest`).
    digest_now: bool,
    /// One short-lived, single-use operator confirmation, lost on restart.
    upgrade_confirm: Option<(String, Value, Instant)>,
}

impl<'a> Session<'a> {
    fn start(door: &'a Door, dir: &Path) -> Result<Self> {
        let manifest = load_manifest(&door.manifest)?;
        let config = door
            .config
            .as_deref()
            .map(crate::plugins::private_config)
            .transpose()?;
        let mut command = Command::new(&manifest.executable[0]);
        // An adapter is not a ship or an agent: it inherits no Privateer role.
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("PRIVATEER_") {
                command.env_remove(name);
            }
        }
        command
            .args(&manifest.executable[1..])
            .current_dir(door.manifest.parent().context("manifest needs a parent")?)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        #[cfg(target_os = "linux")]
        unsafe {
            // A supervisor killed outright takes its adapter with it.
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("cannot start the {} adapter", manifest.id))?;
        let pid = child.id();
        let stdin = child.stdin.take().context("adapter stdin missing")?;
        let stdout = child.stdout.take().context("adapter stdout missing")?;
        let stderr = child.stderr.take().context("adapter stderr missing")?;

        let (to_door, outgoing) = mpsc::sync_channel::<String>(QUEUE);
        thread::spawn(move || {
            let mut stdin = stdin;
            for line in outgoing {
                if stdin.write_all(line.as_bytes()).is_err()
                    || stdin.write_all(b"\n").is_err()
                    || stdin.flush().is_err()
                {
                    break;
                }
            }
            // Dropping stdin here closes the adapter's input.
        });
        let (incoming, from_door) = mpsc::sync_channel(QUEUE);
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                match reader
                    .by_ref()
                    .take(MAX_LINE as u64 + 1)
                    .read_until(b'\n', &mut line)
                {
                    Ok(0) | Err(_) => {
                        let _ = incoming.send(Incoming::End);
                        break;
                    }
                    Ok(_) if line.len() > MAX_LINE => {
                        let _ = incoming.send(Incoming::TooLong);
                        break;
                    }
                    Ok(_) => {
                        if incoming.send(Incoming::Line(line)).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let log = dir.join("door.log");
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                log_line(&log, &line);
            }
        });

        let session = Session {
            door,
            dir: dir.to_path_buf(),
            child,
            pid,
            to_door: Some(to_door),
            from_door,
            stores: HashMap::new(),
            homes: HashMap::new(),
            sent: HashMap::new(),
            carried: HashMap::new(),
            failing: HashSet::new(),
            away_since: None,
            digests: manifest.digests,
            emitted: HashSet::new(),
            upgrade_confirm: None,
            digest_check: None,
            digest_now: false,
        };
        let members = members()?;
        let hello = json!({"type": "hello", "version": VERSION, "door": door.name,
            "adapter": manifest.id, "config_path": config,
            "state_dir": dir.join("state"), "recipient": recipient(door, &members)});
        ensure!(session.emit(&hello)?, "adapter is not reading its input");
        Ok(session)
    }

    fn run(&mut self) -> Result<String> {
        let mut next_poll = Instant::now();
        loop {
            if STOP.load(Ordering::SeqCst) {
                return Ok("stopping on request".into());
            }
            if let Some(success) = crate::jobs::exited_unreaped(self.pid)? {
                return Ok(format!(
                    "adapter exited{}",
                    if success { "" } else { " with a failure" }
                ));
            }
            let wait = next_poll
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100));
            match self.from_door.recv_timeout(wait) {
                Ok(Incoming::Line(line)) => {
                    if !line.iter().all(u8::is_ascii_whitespace) {
                        self.handle(&line)?;
                    }
                }
                Ok(Incoming::TooLong) => bail!("adapter sent a line over 1 MiB"),
                Ok(Incoming::End) | Err(RecvTimeoutError::Disconnected) => {
                    // Usually the adapter exiting: report that when it is.
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while Instant::now() < deadline {
                        if let Some(success) = crate::jobs::exited_unreaped(self.pid)? {
                            return Ok(format!(
                                "adapter exited{}",
                                if success { "" } else { " with a failure" }
                            ));
                        }
                        thread::sleep(Duration::from_millis(50));
                    }
                    return Ok("adapter closed its output".into());
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            if Instant::now() >= next_poll {
                self.poll()?;
                next_poll = Instant::now() + POLL;
            }
        }
    }

    /// Queue one line for the adapter; false when its input is full.
    fn emit(&self, value: &Value) -> Result<bool> {
        let sender = self.to_door.as_ref().context("adapter input closed")?;
        match sender.try_send(serde_json::to_string(value)?) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => Ok(false),
            Err(TrySendError::Disconnected(_)) => bail!("adapter input closed"),
        }
    }

    fn store(&mut self, home: &Path) -> Result<&mut Store> {
        if !self.stores.contains_key(home) {
            let store = Store::open(home)?;
            self.stores.insert(home.to_path_buf(), store);
        }
        Ok(self.stores.get_mut(home).expect("store just opened"))
    }

    fn home(&mut self, ship: &str) -> Result<PathBuf> {
        if !self.homes.contains_key(ship) {
            for (name, home) in members()? {
                self.homes.insert(name, home);
            }
        }
        if let Some(home) = self.homes.get(ship) {
            return Ok(home.clone());
        }
        // The captain writes to a ship on a linked machine from the captain's
        // home here; the mail goes out through that machine's gateway.
        if crate::machines::remote_ships()?
            .iter()
            .any(|(name, _, _)| name == ship)
        {
            if let Some((_, home)) = crate::machines::captain_home(&crate::fleet::registry()?)? {
                return Ok(home);
            }
        }
        bail!("no active ship named {ship} in the fleet")
    }

    /// Send the adapter captain mail it has not yet been sent, oldest first
    /// in each ship's home.
    fn poll(&mut self) -> Result<()> {
        // An away period that ended elsewhere moved the cursors past the held
        // mail; start again from them rather than from this session's view.
        let away = crate::away::active()?;
        let since = away.as_ref().and_then(|a| a["since_ms"].as_i64());
        if self.away_since.is_some() && self.away_since != since {
            self.sent.clear();
        }
        self.away_since = since;
        let members = members()?;
        self.homes = members.iter().cloned().collect();
        let settings = crate::away::settings();
        // Where delivery stood when the period began: digests start after it,
        // so mail delivered before the period is never digested.
        if let Some(since) = since
            && !crate::digest::has_baseline(&self.door.name, since)?
        {
            let door = self.door.name.clone();
            let mut through = std::collections::BTreeMap::new();
            for (ship, home) in &members {
                if let Ok(seq) = self.store(home).and_then(|store| cursor(store, &door)) {
                    through.insert(ship.clone(), seq);
                }
            }
            crate::digest::baseline(&door, since, &through)?;
        }
        for (ship, home) in members {
            let page = (|| -> Result<Value> {
                let door = self.door.name.clone();
                let known = self.sent.get(&ship).copied();
                let store = self.store(&home)?;
                let through = match known {
                    Some(through) => through,
                    None => cursor(store, &door)?,
                };
                let page = crate::mail::dispatch(
                    store,
                    "mail.inbox",
                    json!({"address": crate::mail::CAPTAIN, "after_seq": through, "limit": PAGE}),
                )?;
                self.sent.insert(ship.clone(), through);
                Ok(page)
            })();
            let page = match page {
                Ok(page) => {
                    if self.failing.remove(&ship) {
                        log_line(
                            &self.dir.join("door.log"),
                            &format!("supervisor: {ship} is readable again"),
                        );
                    }
                    page
                }
                Err(error) => {
                    // One unreadable home must not silence the rest.
                    if self.failing.insert(ship.clone()) {
                        log_line(
                            &self.dir.join("door.log"),
                            &format!("supervisor: cannot read {ship}'s captain mail: {error:#}"),
                        );
                    }
                    continue;
                }
            };
            for message in page["messages"].as_array().into_iter().flatten() {
                let seq = message["seq"].as_i64().context("mail without sequence")?;
                let mut event = json!({"type": "mail", "ref": format!("{ship}:{seq}"),
                    "ship": ship, "mail": message});
                let repeat = crate::digest::told(message, away.is_some(), &settings)?;
                // While the captain is away, requests, card edits and urgent
                // mail within its budget reach them at once; the rest waits
                // in the mailbox for a digest or the return.
                match crate::away::route(message, &ship)? {
                    None => {
                        if repeat {
                            event["again"] = json!(true);
                            event["quiet"] = json!(true);
                        }
                    }
                    Some(crate::away::Route::Card) => {}
                    Some(crate::away::Route::Immediate { quiet }) => {
                        event["away"] = json!(true);
                        if quiet {
                            event["quiet"] = json!(true);
                        }
                    }
                    Some(crate::away::Route::Breakthrough) => event["breakthrough"] = json!(true),
                    Some(crate::away::Route::Overflow | crate::away::Route::Digest) => {
                        self.sent.insert(ship.clone(), seq);
                        continue;
                    }
                }
                if !self.emit(&event)? {
                    return Ok(());
                }
                self.sent.insert(ship.clone(), seq);
                self.carried.insert(ship.clone(), seq);
            }
        }
        if self.digests {
            self.digest(away.as_ref(), &settings)?;
        }
        Ok(())
    }

    /// Send pending digests and catch-ups, then build the next digest when
    /// one is due. A built digest is re-sent verbatim until acknowledged.
    fn digest(&mut self, away: Option<&Value>, settings: &crate::away::Settings) -> Result<()> {
        let door = self.door.name.clone();
        let mut pending = crate::digest::pending(&door)?;
        let now = Instant::now();
        if let Some(period) = away
            && (self.digest_now || self.digest_check.is_none_or(|at| now >= at))
        {
            let due = crate::digest::due_ms(&door, period, settings)? <= Store::now_ms();
            if self.digest_now || due {
                self.digest_now = false;
                match crate::digest::build(&door, period, settings)? {
                    Some(row) => pending.push(row),
                    None if due => self.digest_check = Some(now + Duration::from_secs(5)),
                    None => {}
                }
            }
        }
        for row in pending {
            let id = row["id"].as_str().unwrap_or_default().to_owned();
            if self.emitted.contains(&id) {
                continue;
            }
            let event = json!({"type": "digest", "ref": format!("digest:{id}"), "id": id,
                "kind": row["kind"], "lines": row["lines"], "loud": row["loud"]});
            if !self.emit(&event)? {
                return Ok(());
            }
            self.emitted.insert(id);
        }
        Ok(())
    }

    fn handle(&mut self, line: &[u8]) -> Result<()> {
        let message: Value =
            serde_json::from_slice(line).context("adapter sent a line that is not JSON")?;
        match message["type"].as_str() {
            Some("delivered") => self.delivered(&message["ref"]),
            Some("request") => {
                let id = message["id"].clone();
                ensure!(
                    id.is_string() || id.is_number(),
                    "adapter request needs an id"
                );
                let command = message["command"].as_str().unwrap_or("");
                let args = message.get("args").cloned().unwrap_or_else(|| json!({}));
                let response = match self.request(command, &args) {
                    Ok(result) => {
                        json!({"type": "response", "id": id, "ok": true, "result": result})
                    }
                    Err(error) => {
                        json!({"type": "response", "id": id, "ok": false, "error": format!("{error:#}")})
                    }
                };
                ensure!(self.emit(&response)?, "adapter is not reading its input");
                Ok(())
            }
            other => bail!("unknown message type from adapter: {other:?}"),
        }
    }

    /// The captain has a message: advance this door's cursor in its home.
    /// A door acknowledges only mail it was sent.
    fn delivered(&mut self, reference: &Value) -> Result<()> {
        let reference = reference.as_str().context("delivered needs a ref")?;
        if let Some(id) = reference.strip_prefix("digest:") {
            ensure!(
                self.emitted.contains(id),
                "adapter acknowledged {reference}, which it was not sent"
            );
            return crate::digest::delivered(&self.door.name, id);
        }
        let (ship, seq) = reference.rsplit_once(':').context("malformed mail ref")?;
        let seq: i64 = seq.parse().context("malformed mail ref")?;
        ensure!(
            self.carried.get(ship).is_some_and(|sent| seq <= *sent),
            "adapter acknowledged {reference}, which it was not sent"
        );
        let home = self.home(ship)?;
        let door = self.door.name.clone();
        let store = self.store(&home)?;
        store.connection().execute(
            "UPDATE door_cursor SET through_seq=MAX(through_seq, ?2) WHERE door=?1",
            params![door, seq],
        )?;
        Ok(())
    }

    fn request(&mut self, command: &str, args: &Value) -> Result<Value> {
        ensure!(args.is_object(), "request args must be an object");
        match command {
            "send" => self.send(args),
            "ships" => self.ships(),
            "status" => self.status(),
            "upgrade" => match args["action"].as_str().unwrap_or("plan") {
                "plan" => {
                    let commit = args["commit"].as_str().context("upgrade needs a commit")?;
                    let mut plan = crate::releases::linked::plan(commit)?;
                    let token = uuid::Uuid::new_v4().to_string();
                    self.upgrade_confirm = Some((token.clone(), plan.clone(), Instant::now()));
                    plan["confirmation"] = json!(token);
                    plan["dry_run"] = json!(true);
                    Ok(plan)
                }
                "confirm" => {
                    let (token, plan, at) = self
                        .upgrade_confirm
                        .take()
                        .context("no pending upgrade; request a new dry run")?;
                    ensure!(
                        args["confirmation"] == token && at.elapsed() < Duration::from_secs(300),
                        "upgrade confirmation expired or changed"
                    );
                    let commit = plan["commit"].as_str().context("upgrade commit")?;
                    ensure!(
                        crate::releases::linked::plan(commit)? == plan,
                        "upgrade targets changed; request a new dry run"
                    );
                    let (_, home) = members()?
                        .into_iter()
                        .next()
                        .context("the fleet has no ships")?;
                    crate::dispatch(
                        self.store(&home)?,
                        "upgrade.start",
                        json!({"commit":commit}),
                    )
                }
                _ => bail!("upgrade action must be plan or confirm"),
            },
            // Away mode: peeking at status or what is held leaves the captain away.
            "away" => match args
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("status")
            {
                "start" => {
                    let text = args.get("words").and_then(Value::as_str).unwrap_or("");
                    let afk = crate::away::parse(text);
                    crate::away::enter(&afk.words, afk.until.as_deref(), afk.every_secs)
                }
                "back" => {
                    let returned = crate::away::back()?;
                    self.sent.clear();
                    Ok(json!({"returned": returned}))
                }
                "status" => crate::away::status(),
                other => bail!("unknown away action {other:?}"),
            },
            "held" => crate::away::held(),
            // `/digest`: what is held now, leaving the captain away.
            "digest" => {
                let away = crate::away::active()?.is_some();
                // Present, nothing is held: no flag left for the next away period.
                self.digest_now = away;
                Ok(json!({"digests": self.digests, "away": away}))
            }
            "waiting" => self.waiting(),
            // The captain closes one item without answering it, wherever it waits.
            "dismiss" => {
                let (_, home) = members()?
                    .into_iter()
                    .next()
                    .context("the fleet has no ships")?;
                let mut request = json!({"id": args.get("id").cloned().unwrap_or(Value::Null)});
                if let Some(note) = args.get("note").filter(|v| !v.is_null()) {
                    request["note"] = note.clone();
                }
                crate::dispatch(self.store(&home)?, "captain.dismiss", request)
            }
            // The captain's Approve or Deny button on a privileged command;
            // every other approval is answered at the bridge.
            "approval" => crate::privileged::door_answer(args),
            // The captain sent a file: the door puts it in the receiving
            // ship's own home, here or on a linked machine, and mails its id.
            "file.put" => {
                let to = args
                    .get("to")
                    .and_then(Value::as_str)
                    .context("file.put needs the ship it is for")?
                    .to_owned();
                identifier(&to)?;
                let home = self.home(&to)?;
                let store = self.store(&home)?;
                crate::dispatch(store, "file.put", args.clone())
            }
            // Registered front doors carry the captain's explicit commands.
            "permissions" if args.as_object().is_some_and(|a| a.is_empty()) => {
                crate::permissions::across_machines(&crate::store::Caller::Operator, None)
            }
            "permissions" => {
                ensure!(
                    args.as_object().is_some_and(|a| a.len() == 1),
                    "permissions takes only mode"
                );
                let on = match args["mode"].as_str() {
                    Some("yolo") => true,
                    Some("standard") => false,
                    _ => bail!("mode must be standard or yolo"),
                };
                crate::permissions::across_machines(&crate::store::Caller::Operator, Some(on))
            }
            // The captain stows or resumes the whole fleet from the door.
            "stow" => {
                let command = match args
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("status")
                {
                    "start" => "stow.start",
                    "status" => "stow.status",
                    "resume" => "stow.resume",
                    other => bail!("unknown stow action {other:?}"),
                };
                let (_, home) = members()?
                    .into_iter()
                    .next()
                    .context("the fleet has no ships")?;
                // `/stow mac` names a linked machine to stow over its link, and
                // `/stow resume all` lifts every machine this one reaches.
                let mut request = json!({});
                if let Some(machine) = args.get("machine").filter(|v| !v.is_null()) {
                    request["machine"] = machine.clone();
                }
                if args
                    .get("all")
                    .is_some_and(|all| all.as_bool() == Some(true))
                {
                    request["all"] = json!(true);
                }
                crate::dispatch(self.store(&home)?, command, request)
            }
            _ => bail!("unknown door command {command:?}"),
        }
    }

    /// The captain speaks: mail a ship in that ship's own home.
    fn send(&mut self, args: &Value) -> Result<Value> {
        // The captain's first ordinary message while away is their return:
        // the summary request reaches the liaison before the message itself.
        let returned = crate::away::back()?;
        if returned.is_some() {
            self.sent.clear();
        }
        let to = match args.get("to").filter(|v| !v.is_null()) {
            Some(to) => to.as_str().context("to must be a ship name")?.to_owned(),
            None => recipient(self.door, &members()?).context(
                "no ship named and no default recipient: create a liaison, or register the door with --to SHIP",
            )?,
        };
        identifier(&to)?;
        let kind = args
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("request");
        ensure!(
            matches!(kind, "request" | "notice" | "reply"),
            "the captain sends requests, notices and replies"
        );
        let mut mail = json!({"from": crate::mail::CAPTAIN, "to": to, "kind": kind});
        for key in [
            "id",
            "body",
            "payload",
            "correlation",
            "urgency",
            "reply_within_secs",
        ] {
            if let Some(value) = args.get(key).filter(|v| !v.is_null()) {
                mail[key] = value.clone();
            }
        }
        let home = self.home(&to)?;
        let door = self.door.name.clone();
        let store = self.store(&home)?;
        let sent = crate::mail::dispatch(store, "mail.send", mail)?;
        store.audit(
            "door.send",
            &json!({"door": door, "to": to, "mail": sent["id"], "kind": kind}),
        )?;
        Ok(json!({"ship": to, "mail": sent, "returned": returned}))
    }

    fn ships(&mut self) -> Result<Value> {
        let members = members()?;
        let default = recipient(self.door, &members);
        let mut ships = vec![];
        for (ship, home) in members {
            let liaison = is_liaison(&home);
            let dispatcher = crate::service::running(&home).unwrap_or(false);
            let store = self.store(&home)?;
            let scope = crate::mail::dispatch(store, "ship.show", json!({"name": ship}))
                .ok()
                .and_then(|s| s["scope"].as_str().map(str::to_owned))
                .unwrap_or_default();
            let attended = crate::presence::attended(store, &ship).unwrap_or(false);
            ships.push(json!({"name": ship, "scope": scope, "liaison": liaison,
                "attended": attended, "dispatcher": dispatcher}));
        }
        for (ship, machine, scope) in crate::machines::remote_ships()? {
            ships.push(json!({"name": ship, "scope": scope, "machine": machine,
                "liaison": false, "attended": null, "dispatcher": null}));
        }
        Ok(json!({"ships": ships, "recipient": default}))
    }

    /// What is happening, from records only: no wake and no model call.
    fn status(&mut self) -> Result<Value> {
        let mut ships = vec![];
        for (ship, home) in members()? {
            let liaison = is_liaison(&home);
            let dispatcher = crate::service::running(&home).unwrap_or(false);
            let row = (|| -> Result<Value> {
                let store = self.store(&home)?;
                let status = crate::dispatch(store, "status", json!({}))?;
                let wakes =
                    crate::dispatch(store, "wake.list", json!({"ship": ship, "limit": 20}))?;
                let waking = wakes["wakes"]
                    .as_array()
                    .map(|w| w.iter().filter(|w| w["status"] == "running").count())
                    .unwrap_or(0);
                let owed = crate::mail::dispatch(
                    store,
                    "mail.requests",
                    json!({"address": crate::mail::CAPTAIN, "state": "open"}),
                )?;
                Ok(
                    json!({"name": ship, "liaison": liaison, "dispatcher": dispatcher,
                    "attended": crate::presence::attended(store, &ship).unwrap_or(false),
                    "working": waking > 0,
                    "open_tasks": status["open_tasks"],
                    "unacknowledged_mail": status["unacknowledged_mail"],
                    "waiting_on_captain": owed["requests"].as_array().map_or(0, Vec::len)}),
                )
            })();
            ships.push(
                row.unwrap_or_else(|error| json!({"name": ship, "error": format!("{error:#}")})),
            );
        }
        Ok(json!({"ships": ships}))
    }

    /// Requests and decisions the captain still owes an answer, fleet-wide.
    /// The same list the liaison reads with `captain.waiting`.
    fn waiting(&mut self) -> Result<Value> {
        crate::captain::waiting()
    }
}

impl Drop for Session<'_> {
    /// Close the adapter's input, give it a moment to exit by itself, then
    /// end its whole process group. The adapter is reaped only afterwards,
    /// so its process ID still names that group when it is signalled.
    fn drop(&mut self) {
        self.to_door.take();
        let exited = |pid| crate::jobs::exited_unreaped(pid).ok().flatten().is_some();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !exited(self.pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        let group = -(self.pid as libc::pid_t);
        if !exited(self.pid) {
            unsafe { libc::kill(group, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(2);
            while !exited(self.pid) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(50));
            }
        }
        unsafe { libc::kill(group, libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

fn describe(door: &Door) -> String {
    let state = crate::service::door_status(&door.name).unwrap_or_else(|e| format!("{e:#}"));
    let to = door
        .recipient
        .clone()
        .unwrap_or_else(|| "the liaison".into());
    format!(
        "{}: {state}\n  manifest {}\n  config {}\n  messages go to {to} unless they name a ship",
        door.name,
        door.manifest.display(),
        door.config
            .as_ref()
            .map_or("none".into(), |c| c.display().to_string()),
    )
}

/// `privateer door ...`: register, run and stop the captain's front doors.
pub fn cli(args: &[String]) -> Result<()> {
    let flags = crate::cli::flags(args, &["manifest", "config", "to"])?;
    let verb = flags.positional.first().map(String::as_str);
    let name = flags.positional.get(1);
    let named = || -> Result<&String> {
        let name = name.context(USAGE)?;
        identifier(name)?;
        Ok(name)
    };
    let registered = |name: &str| -> Result<Door> {
        registration(name)?.with_context(|| {
            format!("no front door named {name}; add it with `privateer door add {name}`")
        })
    };
    match verb {
        Some("add") => {
            let name = named()?;
            let manifest = match flags.values.get("manifest") {
                Some(path) => absolute(path)?,
                None => {
                    let bundled = crate::cli::plugins_dir()?.join(name).join("manifest.json");
                    ensure!(
                        bundled.is_file(),
                        "no bundled front door named {name}; pass --manifest PATH"
                    );
                    fs::canonicalize(bundled)?
                }
            };
            let adapter = load_manifest(&manifest)?;
            let config = flags
                .values
                .get("config")
                .map(|path| crate::plugins::private_config(&absolute(path)?))
                .transpose()?;
            ensure!(
                config.is_some() || !adapter.config_required,
                "the {} front door needs its private configuration: pass --config PATH",
                adapter.id
            );
            let recipient = flags.values.get("to").cloned();
            if let Some(ship) = &recipient {
                identifier(ship)?;
                ensure!(
                    members()?.iter().any(|(name, _)| name == ship),
                    "no active ship named {ship} in the fleet"
                );
            }
            let conn = crate::fleet::registry()?;
            table(&conn)?;
            conn.execute(
                "INSERT INTO doors(name, manifest, config, recipient, created_ms) VALUES(?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(name) DO UPDATE SET manifest=excluded.manifest, config=excluded.config,
                   recipient=excluded.recipient",
                params![
                    name,
                    manifest.to_string_lossy(),
                    config.as_ref().map(|c| c.to_string_lossy().into_owned()),
                    recipient,
                    Store::now_ms()
                ],
            )?;
            let running = crate::service::held(&directory(name)?.join("lock"))?;
            println!(
                "Front door {name} registered.\n{}\n{}",
                describe(&registered(name)?),
                if running {
                    format!(
                        "It is running: restart it to apply (privateer door stop {name} && privateer door start {name})."
                    )
                } else {
                    format!("Start it with: privateer door start {name}")
                }
            );
        }
        Some("list") | Some("status") if verb == Some("list") || name.is_none() => {
            let doors = doors()?;
            if doors.is_empty() {
                println!("No front doors. Add one with: privateer door add telegram --config PATH");
            }
            for door in doors {
                println!("{}", describe(&door));
            }
        }
        Some("status") => println!("{}", describe(&registered(named()?)?)),
        Some("start") => {
            let door = registered(named()?)?;
            load_manifest(&door.manifest)?;
            if let Some(config) = &door.config {
                crate::plugins::private_config(config)?;
            }
            println!(
                "Front door {}: {}",
                door.name,
                crate::service::ensure_door(&door.name)?
            );
        }
        Some("stop") => println!("{}", crate::service::stop_door(named()?)?),
        Some("remove") => {
            let name = named()?;
            registered(name)?;
            let stopped = crate::service::stop_door(name)?;
            let conn = crate::fleet::registry()?;
            conn.execute("DELETE FROM doors WHERE name=?1", [name])?;
            println!(
                "{stopped}\nFront door {name} removed; its state stays in {}.",
                directory(name)?.display()
            );
        }
        Some("run") => supervise(named()?)?,
        _ => bail!(USAGE),
    }
    Ok(())
}
