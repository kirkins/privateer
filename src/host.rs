//! Hosted first mates: with `first_mate.host = "herdr"`, the dispatcher keeps
//! a ship's first mate as a live interactive session in its own Herdr space
//! instead of waking a headless one per batch of mail. The session holds the
//! ship's presence lease (a `host:` holder the runtime owns, not the
//! captain's) and its attended mail adapter starts turns as mail arrives. If
//! the host is unreachable, the headless wake runs as before once Herdr has
//! stayed unreachable for a grace, so a dispatcher that starts before Herdr at
//! boot does not take its mail headless; if the session dies, its lease lapses
//! and a later ordinary tick launches another. Failed launches back off
//! durably, including a pane that never adopts its lease, except that Herdr
//! coming back after being unreachable is launched into at once.
use crate::store::Store;
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub(crate) mod approvals;
pub mod bridge;
pub(crate) mod pi_sessions;
pub(crate) mod restored;
pub(crate) mod sleep;
pub mod upgrade;

/// How long a launch may take to adopt its lease before it lapses. Before the
/// pane adopts it, up to three launch-planning plugin calls run one after
/// another (the dispatcher's validate_start, the pane's own, and its tool
/// bundle plan), each allowed the launch ceiling on a loaded machine; the
/// lease must outlast them all, or a slow launch is opened twice.
const LAUNCH_TTL_SECS: u64 = 3 * crate::plugins::LAUNCH_CEILING_MS / 1000 + 60;

/// Five minutes gives a brief permission/setup prompt or transient status time
/// to clear without paging, while a pane stuck unable to handle mail is noticed
/// within one dispatcher cycle plus a short, bounded grace period.
const BLOCKED_ALERT_AFTER_MS: i64 = 5 * 60 * 1000;

/// A second mate space usually goes away on its own as the old one closes, so
/// an overlap raises a request only once it has lasted this long across passes.
const DUPLICATE_MATES_ALERT_AFTER_MS: i64 = BLOCKED_ALERT_AFTER_MS;

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
/// `None` when the file cannot be read or parsed right now, as a read that
/// lands mid-write says nothing about trust: that is no answer to act on.
/// A missing file is an answer: Claude has never been run here, so nothing is trusted.
fn claude_trusted(folder: &Path) -> Option<bool> {
    let Some((_, file)) = claude_config() else {
        return Some(false);
    };
    let raw = match std::fs::read(file) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(false),
        Err(_) => return None,
    };
    let config = serde_json::from_slice::<Value>(&raw).ok()?;
    Some(
        folder
            .to_str()
            .is_some_and(|dir| config["projects"][dir]["hasTrustDialogAccepted"] == true),
    )
}

/// A Claude folder whose trust dialog was never accepted. The launch fails with
/// it, and the captain gets one request naming the folder (see `raise_untrusted`).
#[derive(Debug)]
pub(crate) struct Untrusted(pub(crate) PathBuf);

impl std::fmt::Display for Untrusted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Claude has not been trusted in {}; run `privateer start claude` there once, choose \"Yes, I trust this folder\" and, for permission_mode auto, finish auto mode's setup screen",
            self.0.display()
        )
    }
}

impl std::error::Error for Untrusted {}

/// The one step that clears an untrusted folder, for the captain's own hands.
fn trust_step(folder: &Path) -> String {
    format!(
        "open Claude in {} once (`cd {} && claude`) and choose \"Yes, I trust this folder\"",
        folder.display(),
        shell_quote(&folder.display().to_string())
    )
}

/// POSIX single-quote one word, so a folder with spaces survives the shell.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Raise the captain's one request for an untrusted folder. The same folder
/// raises nothing twice until `clear_untrusted` withdraws it. A ship without a
/// duty task has nobody to tell, as with the other host alerts.
pub(crate) fn raise_untrusted(store: &Store, ship: &str, folder: &Path) -> Result<()> {
    let folder = folder.display().to_string();
    let tx = store.write_transaction()?;
    let open: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT folder, mail_id FROM hosted_untrusted WHERE ship=?1",
            [ship],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if matches!(&open, Some((known, Some(_))) if *known == folder) {
        return Ok(());
    }
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
    let Some(duty_task) = duty_task else {
        return Ok(());
    };
    let now = Store::now_ms();
    let id = format!("hosted-untrusted-{ship}-{now}");
    let step = trust_step(Path::new(&folder));
    let body = format!(
        "{ship}'s first mate cannot start in Herdr: Claude has not trusted {folder}. Trust it once, then Privateer starts the mate on its next attempt."
    );
    crate::mail::record_crew(
        &tx,
        &json!({
            "id": id,
            "to": crate::mail::CAPTAIN,
            "kind": "request",
            "task_id": duty_task,
            "urgency": "normal",
            "body": body,
            "payload": {
                "hosted_untrusted": ship,
                "folder": folder,
                "needs_hands": {"step": step}
            }
        }),
    )?;
    tx.execute(
        "INSERT INTO hosted_untrusted(ship,folder,ms,mail_id) VALUES(?1,?2,?3,?4)
         ON CONFLICT(ship) DO UPDATE SET folder=excluded.folder, ms=excluded.ms, mail_id=excluded.mail_id",
        params![ship, folder, now, id],
    )?;
    tx.commit()?;
    Ok(())
}

/// Clear a ship's untrusted-folder state once its folder is trusted, and
/// withdraw the captain's request as the duplicate-mates alert does.
pub(crate) fn clear_untrusted(store: &Store, ship: &str) -> Result<()> {
    let tx = store.write_transaction()?;
    let open: Option<Option<String>> = tx
        .query_row(
            "SELECT mail_id FROM hosted_untrusted WHERE ship=?1",
            [ship],
            |r| r.get(0),
        )
        .optional()?;
    let Some(mail_id) = open else {
        return Ok(());
    };
    tx.execute("DELETE FROM hosted_untrusted WHERE ship=?1", [ship])?;
    tx.commit()?;
    if let Some(id) = mail_id {
        let task: Option<String> = store
            .connection()
            .query_row("SELECT task_id FROM mail WHERE id=?1", [&id], |r| r.get(0))
            .optional()?
            .flatten();
        let request = json!({
            "id": id,
            "from": crate::mail::CREW,
            "to": crate::mail::CAPTAIN,
            "task_id": task,
        });
        crate::mail::end_request(
            store,
            &request,
            "withdrawn",
            "Claude trusts the ship's folder now",
        )?;
    }
    Ok(())
}

/// Serve's view of every untrusted folder still awaiting the captain.
pub(crate) fn untrusted_pending(store: &Store) -> Result<Vec<Value>> {
    let mut query = store
        .connection()
        .prepare("SELECT ship, folder FROM hosted_untrusted ORDER BY ship")?;
    let rows = query
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|(ship, folder)| {
            json!({"ship": ship, "untrusted": folder, "step": trust_step(Path::new(&folder))})
        })
        .collect())
}

/// `hosted_blocked` for serve: the block alerts, plus each untrusted folder
/// until it is trusted. A failed block read keeps its error beside the list.
pub(crate) fn with_untrusted(store: &Store, blocked: Value) -> Value {
    let pending = untrusted_pending(store).unwrap_or_default();
    if pending.is_empty() {
        return blocked;
    }
    match blocked {
        Value::Array(mut items) => {
            items.extend(pending);
            Value::Array(items)
        }
        error => json!({"error": error, "untrusted": pending}),
    }
}

/// `ship.create` for a hosted Claude ship: say now which folder to trust, so
/// the captain is not first told at a launch. True when a request was raised.
pub(crate) fn raise_untrusted_at_create(store: &Store, ship: &str, folder: &Path) -> Result<bool> {
    let folder = folder.canonicalize()?;
    // Only a parsed, untrusted answer raises; an unreadable config waits for the launch.
    if claude_trusted(&folder) != Some(false) {
        return Ok(false);
    }
    raise_untrusted(store, ship, &folder)?;
    Ok(true)
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
    /// An interactive conversation exists, but a pane launched it on another
    /// model route than this launch takes: it is not resumed on a new model.
    Rerouted,
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
            Self::Rerouted => "rerouted",
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

/// Where Claude keeps its transcripts, one folder per working folder.
pub(crate) fn claude_projects() -> Option<PathBuf> {
    claude_config().map(|(dir, _)| dir.join("projects"))
}

/// Claude keeps a folder's transcripts under its path with every other
/// character a dash.
pub(crate) fn claude_project_name(folder: &Path) -> String {
    folder
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Only a transcript whose first entrypoint record names the
/// `cli` entrypoint counts as a conversation `--continue` can resume.
pub(crate) fn claude_conversation(folder: &Path) -> Conversation {
    let Some(projects) = claude_projects() else {
        return Conversation::NoTranscript;
    };
    let Ok(entries) = std::fs::read_dir(projects.join(claude_project_name(folder))) else {
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

/// Whether Claude wrote an interactive transcript in the ship folder at or
/// after `since_ms`: one its `--continue` there resumes now, as the most
/// recent. Pi sessions are found by the id the pane gave them
/// (`pi_sessions`).
pub(crate) fn claude_session_since(folder: &Path, since_ms: i64) -> bool {
    let since = u128::try_from(since_ms).unwrap_or(0);
    let written = |path: &Path| {
        path.extension().is_some_and(|x| x == "jsonl")
            && std::fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                .is_some_and(|at| at.as_millis() >= since)
    };
    let canonical = folder
        .canonicalize()
        .unwrap_or_else(|_| folder.to_path_buf());
    claude_projects().is_some_and(|projects| {
        std::fs::read_dir(projects.join(claude_project_name(&canonical))).is_ok_and(|entries| {
            entries.flatten().map(|e| e.path()).any(|path| {
                written(&path) && matches!(scan_transcript(&path), Ok(Scan::Interactive))
            })
        })
    })
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
pub(crate) fn repair(store: &Store) -> Vec<(String, String, String)> {
    if let Some(socket) = socket() {
        return crate::herdr::sidebar::repair(store, &socket).unwrap_or_default();
    }
    vec![]
}

/// Alert the captain once for each sustained blocked spell. Unknown/missing
/// observations preserve the current spell; only a known live status clears it.
pub(crate) fn alert_blocked(store: &Store, observed: &[(String, String, String)]) -> Result<Value> {
    let now = Store::now_ms();
    let tx = store.write_transaction()?;
    // Lease expiry ends custody even if Herdr is unavailable this tick.
    tx.execute(
        "DELETE FROM hosted_block_alerts WHERE ship NOT IN
         (SELECT ship FROM presence WHERE expires_ms>?1 AND holder LIKE 'host:%')",
        [now],
    )?;
    let mut alerts = Vec::new();
    for (ship, status, _) in observed {
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

/// Ships whose first mate Herdr hosts on this home, the same filter the sidebar
/// uses. Every home sees all of Herdr's spaces, so only the owning home may
/// raise a duplicate for a ship: one duplicate must not reach the captain once
/// per home.
fn local_herdr_ships(conn: &rusqlite::Connection) -> Result<Vec<String>> {
    let mut query = conn.prepare(
        "SELECT name FROM ships WHERE json_extract(record,'$.location')='here'
           AND json_extract(record,'$.first_mate.host')='herdr'",
    )?;
    let names = query
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(names)
}

/// The host pass's duplicate check: one listing of Herdr's mate spaces, read
/// only when this home hosts a first mate in Herdr. With no socket there is
/// nothing to observe, so nothing is cleared either.
pub(crate) fn check_duplicate_mates(store: &Store) -> Result<Value> {
    let Some(socket) = socket() else {
        return Ok(json!([]));
    };
    let spaces = if local_herdr_ships(store.connection())?.is_empty() {
        vec![]
    } else {
        let mut client = crate::herdr::Client::connect(&socket)?;
        client.mate_spaces()?
    };
    reconcile_duplicate_mates(store, &spaces, true)
}

/// Raise one owner request per local ship that has more than one mate space,
/// naming them, once the overlap has lasted DUPLICATE_MATES_ALERT_AFTER_MS.
/// `raise` is false when settle observes a space: it records the first sighting
/// and clears what has gone, but never raises. The request is withdrawn once
/// one or none is left. Runtime.needs shows it, since it cannot query Herdr live.
fn reconcile_duplicate_mates(
    store: &Store,
    spaces: &[(String, String)],
    raise: bool,
) -> Result<Value> {
    let local = local_herdr_ships(store.connection())?;
    let mut by_ship: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (ship, workspace) in spaces.iter().filter(|(ship, _)| local.contains(ship)) {
        by_ship
            .entry(ship.as_str())
            .or_default()
            .push(workspace.as_str());
    }
    let now = Store::now_ms();
    let tx = store.write_transaction()?;
    let tracked: Vec<(String, Option<String>)> = tx
        .prepare("SELECT ship, mail_id FROM duplicate_mate_alerts ORDER BY ship")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut alerts = vec![];
    for (ship, mail_id) in tracked {
        if by_ship.get(ship.as_str()).map_or(0, Vec::len) > 1 {
            continue;
        }
        if let Some(mail_id) = mail_id {
            let task: Option<String> = tx
                .query_row("SELECT task_id FROM mail WHERE id=?1", [&mail_id], |r| {
                    r.get(0)
                })
                .optional()?
                .flatten();
            let request = json!({
                "id": mail_id,
                "from": crate::mail::CREW,
                "to": crate::mail::CAPTAIN,
                "task_id": task,
            });
            crate::mail::end_request(
                store,
                &request,
                "withdrawn",
                "Herdr shows one mate space for this ship again",
            )?;
        }
        tx.execute("DELETE FROM duplicate_mate_alerts WHERE ship=?1", [&ship])?;
        alerts.push(json!({"ship": ship, "cleared": true}));
    }
    for (ship, workspaces) in &by_ship {
        if workspaces.len() < 2 {
            continue;
        }
        // The first sighting is kept; only the listed spaces refresh.
        tx.execute(
            "INSERT INTO duplicate_mate_alerts(ship,first_seen_ms,workspaces,mail_id) VALUES(?1,?2,?3,NULL)
             ON CONFLICT(ship) DO UPDATE SET workspaces=excluded.workspaces",
            params![ship, now, json!(workspaces).to_string()],
        )?;
        let (first_seen, mail_id): (i64, Option<String>) = tx.query_row(
            "SELECT first_seen_ms, mail_id FROM duplicate_mate_alerts WHERE ship=?1",
            [ship],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if !raise
            || mail_id.is_some()
            || now.saturating_sub(first_seen) < DUPLICATE_MATES_ALERT_AFTER_MS
        {
            continue;
        }
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
        let Some(duty_task) = duty_task else {
            continue;
        };
        let id = format!("duplicate-mates-{ship}-{now}");
        let listed = workspaces.join(", ");
        let body = format!(
            "{ship} has {} Herdr mate spaces ({listed}). Privateer does not close either one; check which pane is live in Herdr and close the other space yourself.",
            workspaces.len()
        );
        crate::mail::record_crew(
            &tx,
            &json!({
                "id": id,
                "to": crate::mail::CAPTAIN,
                "kind": "request",
                "task_id": duty_task,
                "urgency": "normal",
                "body": body,
                "payload": {
                    "duplicate_mates": ship,
                    "workspaces": workspaces,
                    "needs_hands": {"step": format!(
                        "in Herdr, close the extra mate space for {ship} ({listed}), keeping the one with the live pane"
                    )}
                }
            }),
        )?;
        tx.execute(
            "UPDATE duplicate_mate_alerts SET mail_id=?2 WHERE ship=?1",
            params![ship, id],
        )?;
        alerts.push(json!({"ship": ship, "workspaces": workspaces}));
    }
    tx.commit()?;
    Ok(json!(alerts))
}

pub(crate) enum Attempt {
    Launched(Value),
    Backoff(i64),
    Busy,
}

/// How long a hosted ship waits for an unreachable Herdr before a headless
/// wake takes its mail, by default.
pub(crate) const HERDR_GRACE_SECS: u64 = 180;

/// The end of the grace a hosted ship gives an unreachable Herdr before a
/// headless wake, while it lasts: from when a launch first found Herdr
/// unreachable, not from the age of the mail.
pub(crate) fn awaiting_herdr(store: &Store, ship: &str, grace_ms: i64) -> Result<Option<i64>> {
    let since: Option<i64> = store
        .connection()
        .query_row(
            "SELECT unreachable_ms FROM host_launches WHERE ship=?1",
            [ship],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(since
        .map(|since| since.saturating_add(grace_ms))
        .filter(|until| *until > Store::now_ms()))
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
    // A ship that no longer needs Claude's folder trust has no trust request left to keep.
    let bundled = profile["manifest"]
        .as_str()
        .and_then(crate::bundled::harness);
    if !hosted(profile) || bundled.as_deref() != Some("claude") {
        clear_untrusted(store, ship)?;
    }
    ensure!(hosted(profile), "ship is no longer hosted");
    let harness = bundled.context("hosting requires a bundled native harness")?;
    ensure!(
        LIVE_HARNESSES.contains(&harness.as_str()),
        "{harness} cannot be hosted: its attended adapter does not start turns on mail"
    );
    let folder = store
        .home()
        .parent()
        .context("ship folder")?
        .canonicalize()?;
    if harness == "claude" {
        match claude_trusted(&folder) {
            Some(true) => clear_untrusted(store, ship)?,
            Some(false) => {
                raise_untrusted(store, ship, &folder)?;
                return Err(Untrusted(folder).into());
            }
            // Unreadable right now: block the launch, but neither raise nor clear.
            None => return Err(Untrusted(folder).into()),
        }
    }
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
    crate::plugins::plan_launch(
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
        let previous: Option<(i64, i64, bool, Option<i64>, bool)> = tx
            .query_row(
                "SELECT failures, retry_ms, start_asleep, start_through, unreachable_ms IS NOT NULL
                 FROM host_launches WHERE ship=?1",
                [ship],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let now = Store::now_ms();
        // Backoff is for launches that fail; Herdr that was down and now
        // answers is launched into at once, so its space is not left waiting.
        if let Some((_, until, _, _, unreachable)) =
            previous.filter(|(_, until, _, _, _)| *until > now)
            && !(unreachable && answers())
        {
            // A launch backing off while Herdr is still missing starts the
            // grace now, so a dispatcher that wakes first at boot waits it out.
            if !answers() {
                tx.execute(
                    "UPDATE host_launches SET unreachable_ms=COALESCE(unreachable_ms, ?2) WHERE ship=?1",
                    params![ship, now],
                )?;
                tx.commit()?;
            }
            return Ok(Attempt::Backoff(until));
        }
        let failures = previous.map_or(1, |(n, _, _, _, _)| n.saturating_add(1).clamp(1, 7));
        // The flag and the mail it has seen stay until a pane starts on them.
        let carried = previous.filter(|(_, _, flag, _, _)| *flag);
        asleep = carried.is_some();
        let through = carried.and_then(|(_, _, _, through, _)| through);
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
    // Past admission, a failure may be Herdr's own.
    let mut at_host = false;
    let result = (|| -> Result<Value> {
        validate_start(store, ship, profile)?;
        at_host = true;
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
            ship,
            &binary,
            store.home(),
            &env,
            &["start", "--hosted"],
        )?;
        restored::record(store, &socket, &pane, "mate", ship, folder);
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
    // The grace before a headless wake runs from when a launch first found
    // Herdr unreachable, and ends at a launch it does not stop.
    let unreachable = result.is_err() && at_host && !answers();
    let tx = store.write_transaction()?;
    tx.execute(
        "UPDATE host_launches SET unreachable_ms=CASE WHEN ?2 THEN COALESCE(unreachable_ms, ?3) END
         WHERE ship=?1",
        params![ship, unreachable, Store::now_ms()],
    )?;
    tx.commit()?;
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
    crew_space(
        store,
        &Of {
            job,
            task,
            ship,
            cwd,
        },
        &[],
        &["_job-supervise", job],
        &Space::Crew,
    )
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
        &Of {
            job,
            task,
            ship,
            cwd,
        },
        &[("PRIVATEER_ROLE", "operator")],
        &["watch", "--job", job],
        &Space::Crew,
    )
}

/// How long a job runs before it earns a space in the terminal host. Shorter
/// jobs (most gates, checks and erases) never open one, so the sidebar does
/// not churn with every command a ship runs.
pub(crate) const JOB_SPACE_AFTER_MS: i64 = 120_000;
/// Spaces opened per tick, so a burst of long jobs does not flood the sidebar.
const JOB_SPACES_PER_TICK: usize = 3;

/// What a space opened under a ship holds.
pub(crate) enum Space {
    Crew,
    /// A long command job, under the crew that started it (by job id) when
    /// there is one, else under the first mate.
    Job {
        parent: Option<String>,
        label: String,
    },
}

/// A running command job of a hosted ship here that has run long enough for
/// a space and has none yet.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LongJob {
    pub id: String,
    pub task: String,
    pub ship: String,
    pub cwd: PathBuf,
    pub parent: Option<String>,
    pub label: String,
}

/// Command jobs spawned at or before `cutoff_ms` and still running, of ships
/// hosted in Herdr here, that have no recorded space. Wakes, errands and crew
/// are not command jobs: they have their own spaces or none.
pub(crate) fn long_jobs(store: &Store, cutoff_ms: i64) -> Result<Vec<LongJob>> {
    let conn = store.connection();
    let mut query = conn.prepare(
        "SELECT j.id, j.record FROM jobs j
         WHERE json_extract(j.record,'$.status')='running'
           AND json_extract(j.record,'$.spawned_ms') <= ?1
           AND COALESCE(json_extract(j.record,'$.action'),'') != 'spawn'
           AND json_extract(j.record,'$.model_route') IS NULL
           AND NOT EXISTS(SELECT 1 FROM wakes w WHERE w.job_id=j.id)
           AND NOT EXISTS(SELECT 1 FROM errands e WHERE e.id=j.id)
           AND NOT EXISTS(SELECT 1 FROM crew_panes c WHERE c.job_id=j.id)
         ORDER BY json_extract(j.record,'$.spawned_ms') LIMIT 50",
    )?;
    let rows: Vec<(String, String)> = query
        .query_map([cutoff_ms], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = vec![];
    for (id, raw) in rows {
        let job: Value = serde_json::from_str(&raw)?;
        let task = job["task_id"].as_str().unwrap_or_default().to_owned();
        let ship = match job["agent_actor"].as_str() {
            Some(ship) => ship.to_owned(),
            None => conn
                .query_row("SELECT owner FROM tasks WHERE id=?1", [&task], |r| r.get(0))
                .optional()?
                .unwrap_or_default(),
        };
        let hosted: bool = conn
            .query_row(
                "SELECT json_extract(record,'$.location')='here'
                    AND json_extract(record,'$.first_mate.host')='herdr' FROM ships WHERE name=?1",
                [&ship],
                |r| r.get::<_, Option<bool>>(0),
            )
            .optional()?
            .flatten()
            .unwrap_or(false);
        if !hosted {
            continue;
        }
        out.push(LongJob {
            label: job_label(&job["command"], &task),
            parent: job["started_by"].as_str().map(str::to_owned),
            cwd: job["cwd"]
                .as_str()
                .map(PathBuf::from)
                .unwrap_or_else(|| store.home().to_path_buf()),
            id,
            task,
            ship,
        });
    }
    Ok(out)
}

/// A short name for a job's space: the script or program it runs, past any
/// interpreter, environment assignment or niceness, else its task. Inline
/// code (`sh -c '…'`, `node -e '…'`) is no name.
fn job_label(command: &Value, task: &str) -> String {
    const INTERPRETERS: [&str; 8] = [
        "bash", "sh", "zsh", "env", "python", "python3", "node", "nice",
    ];
    let mut name = None;
    for arg in command
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let inline = arg == "-e"
            || arg == "--eval"
            || (arg.starts_with('-') && !arg.starts_with("--") && arg.ends_with('c'));
        if inline {
            break;
        }
        let base = arg.rsplit('/').next().unwrap_or(arg);
        let skipped = arg.starts_with('-')
            || arg.contains('=')
            || arg.parse::<i32>().is_ok()
            || INTERPRETERS.contains(&base);
        if !skipped {
            name = Some(base);
            break;
        }
    }
    match name {
        Some(name) => format!("{task} · {}", name.chars().take(40).collect::<String>()),
        None => task.to_owned(),
    }
}

/// Opens a read-only space for each long job of a hosted ship here, under
/// whoever started it. Presentation only: a refused space never touches the
/// job, and a job is tried once per dispatcher process.
pub(crate) fn view_long_jobs(store: &Store) -> Value {
    static TRIED: std::sync::Mutex<std::collections::BTreeSet<String>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    let jobs = match long_jobs(store, Store::now_ms() - JOB_SPACE_AFTER_MS) {
        Ok(jobs) => jobs,
        Err(error) => return json!({"error": format!("{error:#}")}),
    };
    let mut tried = TRIED.lock().unwrap_or_else(|e| e.into_inner());
    let fresh: Vec<LongJob> = jobs
        .into_iter()
        .filter(|job| !tried.contains(&job.id))
        .take(JOB_SPACES_PER_TICK)
        .collect();
    if fresh.is_empty() || !answers() {
        return json!([]);
    }
    let mut opened = vec![];
    for job in fresh {
        tried.insert(job.id.clone());
        let result = crew_space(
            store,
            &Of {
                job: &job.id,
                task: &job.task,
                ship: &job.ship,
                cwd: &job.cwd,
            },
            &[("PRIVATEER_ROLE", "operator")],
            &["watch", "--job", &job.id],
            &Space::Job {
                parent: job.parent.clone(),
                label: job.label.clone(),
            },
        );
        opened.push(json!({"job_id": job.id, "ship": job.ship,
            "error": result.err().map(|e| format!("{e:#}"))}));
    }
    json!(opened)
}

pub(crate) fn crew_tab_title(ship: &str, task: &str) -> String {
    if ship.is_empty() {
        task.to_string()
    } else {
        format!("{ship} {task}")
    }
}

/// The job a crew or job space follows, and where it opens.
struct Of<'a> {
    job: &'a str,
    task: &'a str,
    ship: &'a str,
    cwd: &'a Path,
}

fn crew_space(
    store: &Store,
    of: &Of,
    env: &[(&str, &str)],
    command: &[&str],
    kind: &Space,
) -> Result<()> {
    let Of {
        job,
        task,
        ship,
        cwd,
    } = *of;
    let socket = socket().context("no Herdr socket configured")?;
    let mut client = crate::herdr::Client::connect(&socket)?;
    let binary = crate::executable::installed()?;
    let tab_title = crew_tab_title(ship, task);
    let (label, role, tokens) = match kind {
        Space::Crew => (format!("└ {task}"), "crew", json!({"job": job})),
        Space::Job { parent, label } => (
            format!("  └ {label}"),
            "job",
            json!({"job": job, "parent": parent}),
        ),
    };
    let pane = client.space(cwd, &label, &tab_title, &binary, store.home(), env, command)?;
    restored::record(store, &socket, &pane, role, ship, cwd);
    let viewer = command[0] == "watch";
    // Where the captain finds this crew's session. Presentation only: a
    // pane that is not recorded still runs the crew.
    let binding = crate::herdr::Binding {
        socket,
        socket_identity: client.identity(),
        pane: pane.clone(),
    };
    let recorded = crate::session::record_crew(store, job, ship, &binding, viewer);
    settle_tagged(store, &mut client, &pane.workspace_id, role, ship, &tokens);
    store.audit(
        if role == "job" {
            "host.job"
        } else {
            "host.crew"
        },
        &json!({"job_id": job, "task_id": task, "pane": pane.pane_id,
            "workspace": pane.workspace_id, "viewer": viewer,
            "unrecorded": recorded.err().map(|e| format!("{e:#}"))}),
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
    settle_tagged(store, client, workspace, role, ship, &json!({}));
}

/// As `settle`, with tokens that place a crew or job space under its parent.
fn settle_tagged(
    store: &Store,
    client: &mut crate::herdr::Client,
    workspace: &str,
    role: &str,
    ship: &str,
    extra: &Value,
) {
    let mates = crate::herdr::sidebar::mates(store.connection()).unwrap_or_default();
    let mut tokens = json!({"privateer": role, "ship": ship});
    for (key, value) in extra.as_object().into_iter().flatten() {
        if let Some(text) = value.as_str() {
            tokens[key] = json!(text);
        }
    }
    let _ = client
        .tag_tokens(workspace, tokens)
        .and_then(|()| client.arrange_with_mates(&mates));
    // The new space is tagged now, so a second mate space for this ship shows.
    // Settle only records the sighting: the old space may still be closing.
    if let Ok(spaces) = client.mate_spaces() {
        let _ = reconcile_duplicate_mates(store, &spaces, false);
    }
}

#[cfg(test)]
mod tests {
    use super::shell_quote;

    #[test]
    fn the_trust_step_folder_survives_spaces_and_quotes() {
        assert_eq!(shell_quote("/home/a b"), "'/home/a b'");
        assert_eq!(shell_quote("/home/it's"), r"'/home/it'\''s'");
    }

    /// Only a hosted ship's command job that has run past the threshold and
    /// has no space gets one; crew, wakes, short or recorded jobs never do.
    #[test]
    fn only_long_running_command_jobs_of_hosted_ships_earn_a_space() {
        use crate::store::Store;
        use serde_json::json;
        let temp = tempfile::TempDir::new().unwrap();
        let store = Store::open(&temp.path().join("home")).unwrap();
        crate::jobs::init(&store).unwrap();
        let conn = store.connection();
        for (name, host) in [("apps", "herdr"), ("web", "none")] {
            conn.execute(
                "INSERT INTO ships(name, record) VALUES(?1, ?2)",
                rusqlite::params![
                    name,
                    json!({"name": name, "location": "here", "first_mate": {"host": host}})
                        .to_string()
                ],
            )
            .unwrap();
        }
        let now = 10_000_000;
        let job = |id: &str, ship: &str, spawned: i64, extra: serde_json::Value| {
            let mut record = json!({"id": id, "task_id": "fix", "agent_actor": ship, "status": "running",
                "spawned_ms": spawned, "cwd": "/repo",
                "command": ["bash", "/repo/target/gates.sh", "/repo"], "started_by": "crew-1"});
            for (k, v) in extra.as_object().unwrap() {
                record[k] = v.clone();
            }
            conn.execute(
                "INSERT INTO jobs(id, record) VALUES(?1, ?2)",
                rusqlite::params![id, record.to_string()],
            )
            .unwrap();
        };
        job("long", "apps", now - 5 * 60_000, json!({}));
        job("short", "apps", now - 30_000, json!({}));
        job("crew", "apps", now - 5 * 60_000, json!({"action": "spawn"}));
        job(
            "done",
            "apps",
            now - 5 * 60_000,
            json!({"status": "succeeded"}),
        );
        job("unhosted", "web", now - 5 * 60_000, json!({}));
        job("shown", "apps", now - 5 * 60_000, json!({}));
        conn.execute(
            "INSERT INTO crew_panes(job_id, ship, binding, viewer, recorded_ms) VALUES('shown','apps','{}',1,0)",
            [],
        )
        .unwrap();
        let found = super::long_jobs(&store, now - super::JOB_SPACE_AFTER_MS).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].id, "long");
        assert_eq!(found[0].label, "fix · gates.sh");
        assert_eq!(found[0].parent.as_deref(), Some("crew-1"));
        assert_eq!(found[0].cwd, std::path::PathBuf::from("/repo"));
    }

    #[test]
    fn a_job_space_names_its_program_and_never_inline_code() {
        use serde_json::json;
        let label = |argv: serde_json::Value| super::job_label(&argv, "fix");
        assert_eq!(
            label(json!([
                "env",
                "-i",
                "PATH=/usr/bin",
                "nice",
                "-n",
                "10",
                "/repo/gates.sh"
            ])),
            "fix · gates.sh"
        );
        assert_eq!(
            label(json!(["sh", "-c", "cd /repo && cargo test 2>&1"])),
            "fix"
        );
        assert_eq!(label(json!(["bash", "-lc", "gh pr checks 7"])), "fix");
    }

    #[test]
    fn crew_tab_title_formats_ship_and_task() {
        assert_eq!(
            super::crew_tab_title("infra", "infra-fix"),
            "infra infra-fix"
        );
        assert_eq!(super::crew_tab_title("", "infra-fix"), "infra-fix");
    }
}
