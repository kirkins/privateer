//! Single-installation agent intent. No fleet traversal, probes, fetch or build.
use super::{apply, linked};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

// Resolve existing ancestors too: config/state directories need not exist yet.
fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    if path.exists() {
        return Ok(std::fs::canonicalize(path)?);
    }
    let parent = path.parent().context("local binding has no parent")?;
    Ok(absolute(parent)?.join(path.file_name().context("local binding has no name")?))
}

pub(super) fn plan(commit: &str) -> Result<Value> {
    ensure!(
        super::valid_commit(commit),
        "upgrade needs an exact lowercase commit"
    );
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"));
    // hostname is local OS identity, not topology's mutable peer aliases.
    let binding = json!({"installation":absolute(&linked::data()?)?,
        "machine":crate::machines::hostname(), "uid":unsafe { libc::geteuid() },
        "home":absolute(&home)?, "config":absolute(&config)?, "state":absolute(&state)?,
        "registry":absolute(&crate::fleet::registry_path()?)?});
    Ok(json!({"commit":commit,"scope":"local",
        "environment":format!("upgrade-local:{}",crate::core::hash(&binding)),
        "machines":[{"name":"local"}]}))
}

pub(super) fn execute(state: &mut Value) -> Result<()> {
    let commit = state["commit"].as_str().context("commit")?.to_owned();
    ensure!(
        plan(&commit)?["environment"] == state["environment"],
        "local installation changed; request a fresh approval"
    );
    let selected = json!(super::current()?.map(|release| release.commit));
    ensure!(
        selected == state["selected_at_request"] || selected == commit,
        "selected release changed while upgrade was queued; request a fresh approval"
    );
    let release = super::Release::new(&linked::data()?, &commit)?;
    ensure!(
        std::fs::symlink_metadata(&release.root).is_ok(),
        "local agent upgrade requires a prior operator fetch; run `privateer upgrade fetch {commit}` first (nothing fetched or built)"
    );
    super::fetch::existing(&release.root, "exists")?;
    // An already-selected release is a no-op unless local apply has an
    // interrupted switch to reconcile. Let the existing worker decide that.
    let options = apply::Options {
        commit: Some(commit.clone()),
        dry_run: false,
    };
    state["machines"][0]["outcome"] = json!("applying");
    linked::save(
        Path::new(state["state_path"].as_str().context("state path")?),
        state,
    )?;
    let round = match apply::detached(&options) {
        Ok(round) => round,
        Err(error) if error.to_string().contains("nothing to switch") => {
            state["outcome"] = json!("already_current");
            state["machines"][0]["outcome"] = json!("already_current");
            state["machines"][0]["summary"] = json!(format!("local: already_current ({commit})"));
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    state["machines"][0]["apply"] = round.clone();
    state["machines"][0]["outcome"] = json!("applying");
    // Record custody before waiting, using the coordinator's normal journal.
    linked::save(
        Path::new(state["state_path"].as_str().context("state path")?),
        state,
    )?;
    let round = linked::wait(&json!({"name":"local"}), "apply", round)?;
    state["outcome"] = round["outcome"].clone();
    state["machines"][0]["outcome"] = round["outcome"].clone();
    state["machines"][0]["summary"] = json!(format!(
        "local: {}",
        round["outcome"].as_str().unwrap_or("unknown")
    ));
    state["machines"][0]["apply"] = round;
    Ok(())
}
