//! Single-installation agent intent. No fleet traversal, probes, fetch or build.
use super::{apply, linked};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
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

// Planning must not create a registry or initialize a missing identity.
fn registry_identity(path: &Path) -> Result<Option<String>> {
    if !path.try_exists()? {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let installed: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='door_reader_identity')",
        [],
        |r| r.get(0),
    )?;
    if !installed {
        return Ok(None);
    }
    Ok(conn
        .query_row(
            "SELECT id FROM door_reader_identity WHERE only=1",
            [],
            |r| r.get(0),
        )
        .optional()?)
}

fn os_identity() -> Result<Option<String>> {
    #[cfg(target_os = "linux")]
    match std::fs::read_to_string("/etc/machine-id") {
        Ok(id) if !id.trim().is_empty() => return Ok(Some(id.trim().to_owned())),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(None)
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
    let registry = absolute(&crate::fleet::registry_path()?)?;
    // Bind stable local identities when available; hostname is only an extra,
    // not a unique or immutable identity. These reads never contact peers.
    let binding = json!({"installation":absolute(&linked::data()?)?,
        "machine":crate::machines::hostname(), "uid":unsafe { libc::geteuid() },
        "registry_identity":registry_identity(&registry)?, "os_identity":os_identity()?,
        "home":absolute(&home)?, "config":absolute(&config)?, "state":absolute(&state)?,
        "registry":registry});
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
    record_outcome(state, round);
    Ok(())
}

fn record_outcome(state: &mut Value, round: Value) {
    let original = round["outcome"].as_str().unwrap_or("unknown");
    let outcome = match original {
        "switched" | "switched_with_failures" | "rolled_back" | "already_current" => original,
        _ => {
            state["error"] = json!(format!(
                "local apply ended with outcome {original}; inspect the local round before recovery"
            ));
            "failed"
        }
    };
    state["outcome"] = json!(outcome);
    state["machines"][0]["outcome"] = json!(outcome);
    state["machines"][0]["summary"] = json!(format!("local: {original}"));
    state["machines"][0]["apply"] = round;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn local_outcomes_fail_closed_and_notify_the_captain_once() {
        for original in [
            json!("interrupted"),
            json!("unexpected_outcome"),
            Value::Null,
            json!("switched"),
            json!("already_current"),
            json!("rolled_back"),
            json!("switched_with_failures"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            let store = Store::open(&home).unwrap();
            let mut state = json!({"id":uuid::Uuid::new_v4().to_string(),
                "scope":"local","request_home":home,"request_thread":"thread",
                "requested_by":"requester","commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "machines":[{"name":"local"}]});
            let round = json!({"id":"apply-round","outcome":original});
            record_outcome(&mut state, round.clone());
            let known = matches!(
                original.as_str(),
                Some("switched" | "already_current" | "rolled_back" | "switched_with_failures")
            );
            assert_eq!(
                state["outcome"],
                if known {
                    original.clone()
                } else {
                    json!("failed")
                }
            );
            assert_eq!(state["machines"][0]["outcome"], state["outcome"]);
            assert_eq!(state["machines"][0]["apply"], round);
            if !known {
                assert!(
                    state["error"]
                        .as_str()
                        .unwrap()
                        .contains(original.as_str().unwrap_or("unknown"))
                );
            }
            super::super::requests::notify(&state).unwrap();
            super::super::requests::notify(&state).unwrap();
            let (replies, notices): (i64, i64) = store.connection().query_row(
                "SELECT SUM(kind='reply' AND recipient='requester' AND correlation='thread'),SUM(kind='notice' AND recipient='captain') FROM mail",
                [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
            assert_eq!(replies, 1, "{original}");
            assert_eq!(
                notices,
                i64::from(!matches!(
                    original.as_str(),
                    Some("switched" | "already_current")
                )),
                "{original}"
            );
        }
    }

    #[test]
    fn stable_registry_identity_is_read_only_and_optional() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.sqlite");
        assert_eq!(registry_identity(&path).unwrap(), None);
        assert!(!path.exists());
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL").unwrap();
        assert_eq!(registry_identity(&path).unwrap(), None);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='door_reader_identity'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        let id = crate::door::reader_link::machine_id_in(&conn).unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert_eq!(registry_identity(&path).unwrap(), Some(id));
        conn.execute_batch("ROLLBACK").unwrap();
    }
}
