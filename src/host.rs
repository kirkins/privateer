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

/// How long a launch may take to adopt its lease before it lapses.
const LAUNCH_TTL_SECS: u64 = 60;

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

/// Whether Claude holds an interactive conversation for this folder, which
/// `--continue` needs: without one, an interactive `--continue` exits. Claude
/// keeps a folder's transcripts under its path with every other character a
/// dash, and `--continue` passes over headless (`claude -p`, entrypoint
/// `sdk-cli`) ones, which a ship's wakes and errands leave there; only a
/// transcript whose opening records name the `cli` entrypoint counts.
pub(crate) fn claude_has_conversation(folder: &Path) -> bool {
    use std::io::Read;
    let Some((dir, _)) = claude_config() else {
        return false;
    };
    let name: String = folder
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let interactive = |path: &Path| {
        let mut head = Vec::new();
        std::fs::File::open(path)
            .and_then(|file| file.take(64 * 1024).read_to_end(&mut head))
            .is_ok_and(|_| {
                let mark = br#""entrypoint":"cli""#;
                head.windows(mark.len()).any(|w| w == mark)
            })
    };
    std::fs::read_dir(dir.join("projects").join(name)).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            let path = e.path();
            path.extension().is_some_and(|x| x == "jsonl") && interactive(&path)
        })
    })
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
/// they were opened before workspace metadata existed. Best effort only.
pub(crate) fn repair(store: &Store) {
    if let Some(socket) = socket() {
        let _ = crate::herdr::sidebar::repair(store, &socket);
    }
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
        let previous: Option<(i64, i64)> = tx
            .query_row(
                "SELECT failures, retry_ms FROM host_launches WHERE ship=?1",
                [ship],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let now = Store::now_ms();
        if let Some((_, until)) = previous.filter(|(_, until)| *until > now) {
            return Ok(Attempt::Backoff(until));
        }
        let failures = previous.map_or(1, |(n, _)| n.saturating_add(1).clamp(1, 7));
        tx.execute(
            "INSERT INTO host_launches(ship,holder,failures,retry_ms,error) VALUES(?1,?2,?3,?4,'awaiting hosted startup')
             ON CONFLICT(ship) DO UPDATE SET holder=excluded.holder, failures=excluded.failures,
               retry_ms=excluded.retry_ms, error=excluded.error",
            params![ship, holder, failures, now + delay(failures)])?;
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
        let binary = crate::executable::installed()?;
        let pane = client.space(
            folder,
            ship,
            &binary,
            store.home(),
            &[("PRIVATEER_HOSTED_HOLDER", &holder)],
            &["start", "--hosted"],
        )?;
        settle(store, &mut client, &pane.workspace_id, "mate", ship);
        let launched =
            json!({"holder": holder, "pane": pane.pane_id, "workspace": pane.workspace_id});
        store.audit(
            "host.launched",
            &json!({"ship": ship, "launched": launched}),
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
    store.connection().execute(
        "UPDATE host_launches SET failures=0,error=NULL WHERE ship=?1 AND holder=?2",
        params![ship, holder],
    )?;
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
    pub holder: &'a str,
}
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let _ = crate::presence::leave(self.store, self.ship, self.holder);
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
