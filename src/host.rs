//! Hosted first mates: with `first_mate.host = "herdr"`, the dispatcher keeps
//! a ship's first mate as a live interactive session in its own Herdr space
//! instead of waking a headless one per batch of mail. The session holds the
//! ship's presence lease (a `host:` holder the runtime owns, not the
//! captain's) and its attended mail adapter starts turns as mail arrives. If
//! the host is unreachable, the headless wake runs as before; if the session
//! dies, its lease lapses and a later ordinary tick launches another. Failed
//! launches back off durably, including a pane that never adopts its lease.
use crate::store::Store;
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub mod bridge;
pub(crate) mod sleep;
pub mod upgrade;

/// How long a launch may take to adopt its lease before it lapses.
const LAUNCH_TTL_SECS: u64 = 60;

/// Five minutes gives a brief permission/setup prompt or transient status time
/// to clear without paging, while a pane stuck unable to handle mail is noticed
/// within one dispatcher cycle plus a short, bounded grace period.
const BLOCKED_ALERT_AFTER_MS: i64 = 5 * 60 * 1000;

/// Harnesses whose attended adapter starts a turn when mail arrives.
const LIVE_HARNESSES: &[&str] = &["pi", "pi-signed", "claude"];
/// Harnesses whose hosted crew end their session when the work settles.
const CREW_HARNESSES: &[&str] = &["pi", "pi-signed"];

/// Claude's own state directory and user config file, as Claude finds them.
fn claude_config() -> Option<(PathBuf, PathBuf)> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from) {
        let file = dir.join(".claude.json");
        return Some((dir, file));
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    Some((home.join(".claude"), home.join(".claude.json")))
}

/// Whether Claude's folder-trust dialog was accepted for this very folder.
/// An interactive Claude in an untrusted folder stops at that dialog, whose
/// default declines, with nobody at a hosted pane to answer. Trust on a folder
/// above does not do: a ship folder carries `init`'s permission rules in
/// `.claude/settings.json`, and Claude asks again for a folder with its own
/// rules (seen on 2.1.300 under an already trusted parent).
/// Read only: Claude rewrites this file itself, so Privateer never writes it.
fn claude_trusted(folder: &Path) -> bool {
    let Some(config) = claude_config()
        .and_then(|(_, file)| std::fs::read(file).ok())
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
    else {
        return false;
    };
    folder
        .to_str()
        .is_some_and(|dir| config["projects"][dir]["hasTrustDialogAccepted"] == true)
}

/// Bytes read from one transcript while looking for its first entrypoint
/// record. Current Claude Code writes about 105 KB of other records before it.
const TRANSCRIPT_SCAN_BYTES: u64 = 8 * 1024 * 1024;

/// What a folder's Claude transcripts say about `--continue`, which needs an
/// interactive conversation: without one, an interactive `--continue` exits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Conversation {
    /// A transcript's first entrypoint record is `cli`: `--continue` resumes it.
    Interactive,
    /// The folder has no transcript at all.
    NoTranscript,
    /// Transcripts exist, but none holds an interactive conversation: headless
    /// (`sdk-cli`) wakes and errands leave them, and `--continue` passes over.
    HeadlessOnly,
    /// A transcript had no entrypoint record within the scan bound, so it may
    /// still hold an interactive conversation.
    BoundReached,
    /// Transcripts exist, but none has an entrypoint record: an empty file, or
    /// a session that stopped before its first record with one.
    NoEntrypoint,
}

impl Conversation {
    pub(crate) fn continues(self) -> bool {
        self == Self::Interactive
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::NoTranscript => "no transcript",
            Self::HeadlessOnly => "headless only",
            Self::BoundReached => "bound reached",
            Self::NoEntrypoint => "no entrypoint",
        }
    }
}

/// One transcript's verdict, decided by its first record with an `entrypoint`.
enum Scan {
    Interactive,
    Headless,
    /// Read to the end of the file without an entrypoint record.
    Unmarked,
    /// The scan bound ended before any entrypoint record.
    Undecided,
}

/// Reads records in order, stopping at the first with an `entrypoint` field,
/// so the decision never depends on a byte window a long preamble can overrun.
/// A line that does not parse, such as one cut at the bound, is skipped.
fn scan_transcript(path: &Path) -> std::io::Result<Scan> {
    use std::io::{BufRead, BufReader, Read};
    let mut reader = BufReader::new(std::fs::File::open(path)?.take(TRANSCRIPT_SCAN_BYTES));
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            // The bound ended the read only if the file is longer than it.
            let exhausted = reader.get_ref().limit() == 0;
            let longer = std::fs::metadata(path)?.len() > TRANSCRIPT_SCAN_BYTES;
            return Ok(if exhausted && longer {
                Scan::Undecided
            } else {
                Scan::Unmarked
            });
        }
        let Ok(record) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if let Some(entrypoint) = record["entrypoint"].as_str() {
            return Ok(if entrypoint == "cli" {
                Scan::Interactive
            } else {
                Scan::Headless
            });
        }
    }
}

/// Claude keeps a folder's transcripts under its path with every other
/// character a dash. Only a transcript whose first entrypoint record names the
/// `cli` entrypoint counts as a conversation `--continue` can resume.
pub(crate) fn claude_conversation(folder: &Path) -> Conversation {
    let Some((dir, _)) = claude_config() else {
        return Conversation::NoTranscript;
    };
    let name: String = folder
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let Ok(entries) = std::fs::read_dir(dir.join("projects").join(name)) else {
        return Conversation::NoTranscript;
    };
    let mut found = false;
    let mut unmarked = false;
    let mut undecided = false;
    for path in entries.flatten().map(|e| e.path()) {
        if path.extension().is_none_or(|x| x != "jsonl") {
            continue;
        }
        match scan_transcript(&path) {
            Ok(Scan::Interactive) => return Conversation::Interactive,
            Ok(Scan::Headless) => found = true,
            Ok(Scan::Unmarked) => unmarked = true,
            Ok(Scan::Undecided) => undecided = true,
            Err(_) => {}
        }
    }
    if undecided {
        Conversation::BoundReached
    } else if found {
        Conversation::HeadlessOnly
    } else if unmarked {
        Conversation::NoEntrypoint
    } else {
        Conversation::NoTranscript
    }
}

/// What a hosted first mate's launch would resume, for a Claude first mate
/// without a bounded tool plan (`None` where no `--continue` applies at all).
pub(crate) fn continuation(folder: &Path, first_mate: &Value) -> Option<Conversation> {
    (first_mate["manifest"]
        .as_str()
        .and_then(crate::bundled::harness)
        .as_deref()
        == Some("claude")
        && first_mate.get("tool_bundles").is_none())
    .then(|| claude_conversation(folder))
}

pub(crate) fn validate(host: &Value) -> Result<Value> {
    ensure!(
        host.as_str() == Some("herdr"),
        "first_mate.host must be \"herdr\""
    );
    Ok(host.clone())
}

pub(crate) fn hosted(profile: &Value) -> bool {
    profile["host"] == "herdr"
}

/// Herdr's socket, as panes see it or at its default location.
fn socket() -> Option<PathBuf> {
    let configured = std::env::var_os("HERDR_SOCKET_PATH").map(PathBuf::from);
    let default = || {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(config.join("herdr/herdr.sock"))
    };
    configured
        .or_else(default)
        .filter(|path| path.is_absolute())
}

/// Repair presentation for this dispatcher's live hosted mates, even when
/// they were opened before workspace metadata existed. Return exact-pane
/// statuses from the same snapshot; presentation and observation are best effort.
pub(crate) fn repair(store: &Store) -> Vec<(String, String)> {
    if let Some(socket) = socket() {
        return crate::herdr::sidebar::repair(store, &socket).unwrap_or_default();
    }
    vec![]
}

/// Alert the captain once for each sustained blocked spell. Unknown/missing
/// observations preserve the current spell; only a known live status clears it.
pub(crate) fn alert_blocked(store: &Store, observed: &[(String, String)]) -> Result<Value> {
    let now = Store::now_ms();
    let tx = store.write_transaction()?;
    // Lease expiry ends custody even if Herdr is unavailable this tick.
    tx.execute(
        "DELETE FROM hosted_block_alerts WHERE ship NOT IN
         (SELECT ship FROM presence WHERE expires_ms>?1 AND holder LIKE 'host:%')",
        [now],
    )?;
    let mut alerts = Vec::new();
    for (ship, status) in observed {
        match status.as_str() {
            "blocked" => {
                let state: Option<(i64, bool)> = tx
                    .query_row(
                        "SELECT blocked_since_ms, alerted FROM hosted_block_alerts WHERE ship=?1",
                        [ship],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                let (since, alerted) = match state {
                    Some((since, alerted)) => (since, alerted),
                    None => {
                        tx.execute(
                            "INSERT INTO hosted_block_alerts(ship,blocked_since_ms,alerted) VALUES(?1,?2,0)",
                            params![ship, now],
                        )?;
                        (now, false)
                    }
                };
                if !alerted && now.saturating_sub(since) >= BLOCKED_ALERT_AFTER_MS {
                    let raw: Option<String> = tx
                        .query_row("SELECT record FROM ships WHERE name=?1", [ship], |row| {
                            row.get(0)
                        })
                        .optional()?;
                    let duty_task = raw
                        .and_then(|record| serde_json::from_str::<Value>(&record).ok())
                        .and_then(|record| {
                            record["first_mate"]["duty_task"]
                                .as_str()
                                .map(str::to_owned)
                        });
                    if let Some(duty_task) = duty_task {
                        let id = format!("hosted-blocked-{ship}-{since}");
                        let body = format!(
                            "{ship}'s live session is waiting for input in Herdr. Answer the prompt in its pane to resume the first mate."
                        );
                        crate::mail::record_crew(
                            &tx,
                            &json!({
                                "id": id,
                                "to": "captain",
                                "kind": "notice",
                                "task_id": duty_task,
                                "urgency": "normal",
                                "body": body,
                                "payload": {"hosted_blocked":ship,"blocked_since_ms":since}
                            }),
                        )?;
                        tx.execute(
                            "UPDATE hosted_block_alerts SET alerted=1 WHERE ship=?1",
                            [ship],
                        )?;
                        alerts.push(json!({"ship":ship,"alerted":true}));
                    }
                }
            }
            "idle" | "working" | "done" => {
                tx.execute("DELETE FROM hosted_block_alerts WHERE ship=?1", [ship])?;
            }
            _ => {} // unknown status is neither evidence of a block nor a clear
        }
    }
    tx.commit()?;
    Ok(json!(alerts))
}

pub(crate) enum Attempt {
    Launched(Value),
    Backoff(i64),
    Busy,
}

/// Retry at most once a minute, exponentially up to an hour. This bound is
/// independent of headless wake retry settings (which may legitimately be 0).
fn delay(failures: i64) -> i64 {
    (60_000_i64 << failures.saturating_sub(1).clamp(0, 6)).min(3_600_000)
}

/// No inference: check the same charter/provider adapter and option validation
/// as a headless launch. Native auth and provider environments remain untouched.
/// Interactive starts cannot silently substitute a custom adapter or its config.
/// Rechecked by `start --hosted`, since revocation may win after pane creation.
pub(crate) fn validate_start(store: &mut Store, ship: &str, profile: &Value) -> Result<()> {
    ensure!(hosted(profile), "ship is no longer hosted");
    let harness = profile["manifest"]
        .as_str()
        .and_then(crate::bundled::harness)
        .context("hosting requires a bundled native harness")?;
    ensure!(
        LIVE_HARNESSES.contains(&harness.as_str()),
        "{harness} cannot be hosted: its attended adapter does not start turns on mail"
    );
    let folder = store
        .home()
        .parent()
        .context("ship folder")?
        .canonicalize()?;
    ensure!(
        harness != "claude" || claude_trusted(&folder),
        "Claude has not been trusted in {}; run `privateer start claude` there once, choose \"Yes, I trust this folder\" and, for permission_mode auto, finish auto mode's setup screen",
        folder.display()
    );
    ensure!(
        crate::presence::folder_ship(store).as_deref() == Some(ship),
        "hosted start must use this folder's ship"
    );
    ensure!(
        !crate::permissions::paused(store, ship)?,
        "tool permission question awaits captain or a policy change"
    );
    let duty = profile["duty_task"]
        .as_str()
        .context("first mate duty task")?;
    ensure!(
        store.task(duty)?["owner"] == ship,
        "first mate duty owner changed"
    );
    let charter = store.authorize(duty, "spawn")?;
    let repo = Path::new(charter["repo"].as_str().context("charter repo")?).canonicalize()?;
    ensure!(
        folder.is_dir() && folder.starts_with(repo),
        "hosted cwd escapes charter repository"
    );
    ensure!(
        profile.get("config").is_none(),
        "hosted native start cannot apply plugin config"
    );
    crate::plugins::dispatch(
        store,
        "plugin.validate",
        json!({
            "task_id": duty, "manifest": profile["manifest"], "options": profile["options"]
        }),
    )?;
    Ok(())
}

/// Reserve both the launch lease and its next retry BEFORE any host I/O.
/// A crash, missing binary, rejected pane or unadopted lease therefore cannot
/// turn an ordinary tick (or dispatcher restart) into an unbounded retry loop.
/// The one row per ship and bounded error also bound persistent state.
pub(crate) fn launch(store: &mut Store, ship: &str, profile: &Value) -> Result<Attempt> {
    let holder = format!("{}{}", crate::presence::HOSTED, uuid::Uuid::new_v4());
    let asleep: bool;
    {
        let tx = store.write_transaction()?;
        let running: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM wakes WHERE ship=?1 AND status IN ('offered','running'))",
            [ship],
            |r| r.get(0),
        )?;
        if running || crate::presence::attended(store, ship)? {
            return Ok(Attempt::Busy);
        }
        let previous: Option<(i64, i64, bool, Option<i64>)> = tx
            .query_row(
                "SELECT failures, retry_ms, start_asleep, start_through FROM host_launches WHERE ship=?1",
                [ship],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let now = Store::now_ms();
        if let Some((_, until, _, _)) = previous.filter(|(_, until, _, _)| *until > now) {
            return Ok(Attempt::Backoff(until));
        }
        let failures = previous.map_or(1, |(n, _, _, _)| n.saturating_add(1).clamp(1, 7));
        // The flag and the mail it has seen stay until a pane starts on them.
        let carried = previous.filter(|(_, _, flag, _)| *flag);
        asleep = carried.is_some();
        let through = carried.and_then(|(_, _, _, through)| through);
        tx.execute(
            "INSERT INTO host_launches(ship,holder,failures,retry_ms,error,start_asleep,start_through) VALUES(?1,?2,?3,?4,'awaiting hosted startup',?5,?6)
             ON CONFLICT(ship) DO UPDATE SET holder=excluded.holder, failures=excluded.failures,
               retry_ms=excluded.retry_ms, error=excluded.error, start_asleep=excluded.start_asleep,
               start_through=excluded.start_through",
            params![ship, holder, failures, now + delay(failures), asleep, through])?;
        // Under the same lock as the attended check; no nested claim transaction.
        let harness = profile["manifest"]
            .as_str()
            .and_then(crate::bundled::harness);
        crate::presence::attend(store, ship, &holder, LAUNCH_TTL_SECS, harness.as_deref())?;
        tx.commit()?;
    }
    let result = (|| -> Result<Value> {
        validate_start(store, ship, profile)?;
        let socket = socket().context("no Herdr socket configured")?;
        let mut client = crate::herdr::Client::connect(&socket)?;
        let folder = store.home().parent().context("ship folder")?;
        // Decided before the pane starts: its own Claude may write a first
        // entrypoint record that would make a fresh launch look resumed.
        let continuation = continuation(folder, profile);
        let binary = crate::executable::installed()?;
        let mut env = vec![("PRIVATEER_HOSTED_HOLDER", holder.as_str())];
        if asleep {
            env.push(("PRIVATEER_HOSTED_START_ASLEEP", "1"));
        }
        let pane = client.space(
            folder,
            ship,
            &binary,
            store.home(),
            &env,
            &["start", "--hosted"],
        )?;
        settle(store, &mut client, &pane.workspace_id, "mate", ship);
        let launched =
            json!({"holder": holder, "pane": pane.pane_id, "workspace": pane.workspace_id});
        store.audit(
            "host.launched",
            &json!({"ship": ship, "launched": launched,
                "continued": continuation.map(Conversation::continues),
                "continuation": continuation.map(Conversation::label)}),
        )?;
        Ok(launched)
    })();
    match result {
        Ok(pane) => Ok(Attempt::Launched(pane)),
        Err(error) => {
            failed(store, ship, &holder, &format!("{error:#}"))?;
            crate::presence::leave(store, ship, &holder)?;
            Err(error)
        }
    }
}

/// Only this attempt may settle its state; a stale pane cannot reset a newer
/// launch's backoff. Keep the reserved deadline even after adoption, so a
/// harness that immediately exits cannot be relaunched on every tick.
pub(crate) fn started(store: &Store, ship: &str, holder: &str) -> Result<()> {
    let tx = store.write_transaction()?;
    tx.execute(
        "UPDATE host_launches SET failures=0,error=NULL,start_asleep=0 WHERE ship=?1 AND holder=?2",
        params![ship, holder],
    )?;
    tx.execute("DELETE FROM hosted_block_alerts WHERE ship=?1", [ship])?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn failed(store: &Store, ship: &str, holder: &str, error: &str) -> Result<()> {
    let tx = store.write_transaction()?;
    let failures: Option<i64> = tx
        .query_row(
            "SELECT MAX(failures,1) FROM host_launches WHERE ship=?1 AND holder=?2",
            params![ship, holder],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(failures) = failures {
        tx.execute(
            "UPDATE host_launches SET failures=?3,retry_ms=MAX(retry_ms,?4),error=?5 WHERE ship=?1 AND holder=?2",
            params![ship, holder, failures, Store::now_ms() + delay(failures),
                error.chars().take(2_000).collect::<String>()])?;
    }
    tx.commit()?;
    Ok(())
}

/// Release even when hosted startup fails before spawning its native process.
/// Successful starts still explicitly release before exiting the wrapper.
pub(crate) struct Lease<'a> {
    pub store: &'a Store,
    pub ship: &'a str,
    /// The current holder, which changes when the first mate sleeps.
    pub holder: String,
}
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let _ = crate::presence::leave(self.store, self.ship, &self.holder);
    }
}

fn answers() -> bool {
    socket()
        .and_then(|s| crate::herdr::Client::connect(&s).ok())
        .is_some_and(|mut c| c.snapshot().is_ok())
}

/// Whether a ship's crew run live: its first mate is hosted, its harness can
/// end a crew when the work settles, and the terminal host answers.
pub(crate) fn hosts_crew(profile: &Value, harness: &str) -> bool {
    hosted(profile) && CREW_HARNESSES.contains(&harness) && answers()
}

/// Whether a hosted ship's crew on another harness get a viewer space. They
/// run headless, which no folder-trust or setup dialog can stall and whose
/// end is confirmed from their event stream, and the space follows their log.
pub(crate) fn views_crew(profile: &Value, harness: &str) -> bool {
    hosted(profile) && !CREW_HARNESSES.contains(&harness) && answers()
}

/// Run a crew job's supervisor in its own background space, labelled under
/// its task. The supervisor claims the job there and gives the harness the
/// pane's terminal.
pub(crate) fn supervise(
    store: &Store,
    job: &str,
    task: &str,
    ship: &str,
    cwd: &Path,
) -> Result<()> {
    crew_space(store, job, task, ship, cwd, &[], &["_job-supervise", job])
}

/// Follow a headless crew job in its own background space under its task:
/// `privateer watch --job`, which closes the space when the job ends.
pub(crate) fn view_crew(
    store: &Store,
    job: &str,
    task: &str,
    ship: &str,
    cwd: &Path,
) -> Result<()> {
    // A first mate starts its crew, but the view belongs to the captain's
    // terminal: its fixed command only reads records and mails what the
    // captain types there, so it runs as the operator, not the ship.
    crew_space(
        store,
        job,
        task,
        ship,
        cwd,
        &[("PRIVATEER_ROLE", "operator")],
        &["watch", "--job", job],
    )
}

fn crew_space(
    store: &Store,
    job: &str,
    task: &str,
    ship: &str,
    cwd: &Path,
    env: &[(&str, &str)],
    command: &[&str],
) -> Result<()> {
    let socket = socket().context("no Herdr socket configured")?;
    let mut client = crate::herdr::Client::connect(&socket)?;
    let binary = crate::executable::installed()?;
    let pane = client.space(
        cwd,
        &format!("└ {task}"),
        &binary,
        store.home(),
        env,
        command,
    )?;
    settle(store, &mut client, &pane.workspace_id, "crew", ship);
    store.audit(
        "host.crew",
        &json!({"job_id": job, "task_id": task, "pane": pane.pane_id,
            "workspace": pane.workspace_id, "viewer": command[0] == "watch"}),
    )?;
    Ok(())
}

/// Tag a new space and put it in its place in the sidebar. Presentation only:
/// a refusal never undoes the launch.
fn settle(
    store: &Store,
    client: &mut crate::herdr::Client,
    workspace: &str,
    role: &str,
    ship: &str,
) {
    let mates = crate::herdr::sidebar::mates(store.connection()).unwrap_or_default();
    let _ = client
        .tag(workspace, role, ship)
        .and_then(|()| client.arrange_with_mates(&mates));
}
