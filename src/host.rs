//! Hosted first mates: with `first_mate.host = "herdr"`, the dispatcher keeps
//! a ship's first mate as a live interactive session in its own Herdr space
//! instead of waking a headless one per batch of mail. The session holds the
//! ship's presence lease (a `host:` holder the runtime owns, not the
//! captain's) and its attended mail adapter starts turns as mail arrives. If
//! the host is unreachable, the headless wake runs as before; if the session
//! dies, its lease lapses and the next mail launches another.
use crate::store::Store;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// How long a launch may take to adopt its lease before it lapses.
const LAUNCH_TTL_SECS: u64 = 60;

/// Harnesses whose attended adapter starts a turn when mail arrives.
const LIVE_HARNESSES: &[&str] = &["pi", "pi-signed"];

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

/// Launch the ship's first mate in its own background Herdr space. The lease
/// is claimed first, so a racing launch or captain's session is refused, and
/// released again if the host refuses.
pub(crate) fn launch(store: &Store, ship: &str, harness: &str) -> Result<Value> {
    ensure!(
        LIVE_HARNESSES.contains(&harness),
        "{harness} cannot be hosted: its attended adapter does not start turns on mail"
    );
    let socket = socket().context("no Herdr socket configured")?;
    let mut client = crate::herdr::Client::connect(&socket)?;
    let holder = format!("{}{}", crate::presence::HOSTED, uuid::Uuid::new_v4());
    crate::presence::claim(store, ship, &holder, LAUNCH_TTL_SECS, Some(harness))?;
    let folder = store.home().parent().context("ship folder")?;
    let binary = crate::executable::installed()?;
    match client.space(
        folder,
        ship,
        &binary,
        store.home(),
        &[("PRIVATEER_HOSTED_HOLDER", &holder)],
        &["start", "--hosted"],
    ) {
        Ok(pane) => {
            let launched = json!({"holder": holder, "pane": pane.pane_id,
                "workspace": pane.workspace_id});
            store.audit(
                "host.launched",
                &json!({"ship": ship, "launched": launched}),
            )?;
            Ok(launched)
        }
        Err(error) => {
            crate::presence::leave(store, ship, &holder)?;
            Err(error)
        }
    }
}

/// Whether a ship's crew run live: its first mate is hosted, its harness can
/// end a crew when the work settles, and the terminal host answers.
pub(crate) fn hosts_crew(profile: &Value, harness: &str) -> bool {
    hosted(profile)
        && LIVE_HARNESSES.contains(&harness)
        && socket()
            .and_then(|s| crate::herdr::Client::connect(&s).ok())
            .is_some_and(|mut c| c.snapshot().is_ok())
}

/// Run a crew job's supervisor in its own background space, labelled under
/// its task. The supervisor claims the job there and gives the harness the
/// pane's terminal.
pub(crate) fn supervise(store: &Store, job: &str, task: &str, cwd: &Path) -> Result<()> {
    let socket = socket().context("no Herdr socket configured")?;
    let mut client = crate::herdr::Client::connect(&socket)?;
    let binary = crate::executable::installed()?;
    let pane = client.space(
        cwd,
        &format!("└ {task}"),
        &binary,
        store.home(),
        &[],
        &["_job-supervise", job],
    )?;
    store.audit(
        "host.crew",
        &json!({"job_id": job, "task_id": task, "pane": pane.pane_id, "workspace": pane.workspace_id}),
    )?;
    Ok(())
}
