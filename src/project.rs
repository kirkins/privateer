//! Repositories a ship works on, cloned into its `projects/` folder.
//!
//! A ship may add projects to itself and to ships it created; the captain to
//! any ship in the fleet. The clone is staged beside its destination and
//! published under the caller's write lock after authority is checked again,
//! so a revoke committed during a long clone leaves nothing behind.
use crate::fleet::{caller_ship, managed, own_actions};
use crate::store::{Caller, Store, identifier, reject_symlinks, required};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The project name a repository address suggests: its last path segment.
pub fn default_name(url: &str) -> String {
    url.trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or("")
        .trim_end_matches(".git")
        .to_owned()
}

/// The address with any HTTP(S) userinfo hidden, for records and replies.
fn redacted(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    match authority.rsplit_once('@') {
        Some((_, host)) if scheme.to_ascii_lowercase().starts_with("http") => {
            format!("{scheme}://***@{host}/{path}")
        }
        _ => url.to_owned(),
    }
}

/// The ship home the caller may add a project to.
fn target_home(store: &Store, target: &str, own: Option<&str>) -> Result<PathBuf> {
    if let Caller::Agent { actor } = store.caller() {
        let actions = own_actions(store, actor)?;
        if target != actor {
            ensure!(
                actions.iter().any(|a| a == "spawn"),
                "adding a project to another ship requires spawn in {actor}'s charter"
            );
        }
    }
    if Some(target) == own {
        return Ok(store.home().to_path_buf());
    }
    Ok(managed(store, target, "add projects to")?.home)
}

fn add(store: &mut Store, args: &Value) -> Result<Value> {
    let url = required(args, "url")?;
    ensure!(
        !url.starts_with('-') && !url.chars().any(char::is_control),
        "url must be a repository address"
    );
    let scheme = url.to_ascii_lowercase();
    if scheme.starts_with("ext::") || scheme.starts_with("fd::") {
        bail!("repository transports that run commands are not allowed");
    }
    let name = match args.get("name").filter(|v| !v.is_null()) {
        Some(name) => name
            .as_str()
            .context("name must be a project name")?
            .to_owned(),
        None => default_name(url),
    };
    identifier(&name).context("pass a project name")?;
    ensure!(!name.starts_with('.'), "a project name cannot start with .");
    let own = caller_ship(store);
    let target = match args.get("target").filter(|v| !v.is_null()) {
        Some(ship) => ship
            .as_str()
            .context("target must be a ship name")?
            .to_owned(),
        None => own
            .clone()
            .context("this home's ship is unknown; pass target")?,
    };
    let home = target_home(store, &target, own.as_deref())?;
    let projects = home.parent().context("ship folder")?.join("projects");
    reject_symlinks(&projects)?;
    fs::create_dir_all(&projects)?;
    let destination = projects.join(&name);
    ensure!(
        !destination.exists(),
        "{} already exists",
        destination.display()
    );
    let staging = projects.join(format!(".{name}-{}.clone", uuid::Uuid::new_v4()));
    let shown = redacted(url);
    let agent = matches!(store.caller(), Caller::Agent { .. });
    let mut git = Command::new("git");
    git.args(["-c", "protocol.ext.allow=never", "clone"]);
    if agent {
        // A headless first mate has no one to answer a credential prompt.
        git.arg("--quiet")
            .stdin(Stdio::null())
            .env("GIT_TERMINAL_PROMPT", "0");
        if std::env::var_os("GIT_SSH_COMMAND").is_none() {
            git.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
        }
    }
    // Replies are JSON on stdout; git reports on stderr.
    git.args(["--", url]).arg(&staging).stdout(Stdio::null());
    let cloned = git.status().context("run git clone");
    if !cloned.as_ref().is_ok_and(|s| s.success()) {
        let _ = fs::remove_dir_all(&staging);
        cloned?;
        bail!("git clone of {shown} failed");
    }
    let published = (|| -> Result<Value> {
        let tx = store.write_transaction()?;
        let again = target_home(store, &target, own.as_deref())?;
        ensure!(again == home, "{target}'s home changed during the clone");
        ensure!(
            !destination.exists(),
            "{} already exists",
            destination.display()
        );
        fs::rename(&staging, &destination)?;
        let by = own.clone().unwrap_or_else(|| "captain".into());
        let result = json!({"ship": target, "project": name, "folder": destination,
            "url": shown, "by": by});
        store.audit("project.added", &result)?;
        tx.commit()?;
        Ok(result)
    })();
    if published.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    published
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "project.add" => add(store, &args),
        _ => bail!("unknown project command {command}"),
    }
}
