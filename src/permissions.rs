//! Captain-owned launch policy. This changes harness approvals, not charters.
use crate::store::{Caller, Store, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

fn path() -> Result<PathBuf> {
    let path = crate::fleet::registry_path()?.with_extension("permissions.json");
    ensure!(path.is_absolute(), "fleet permission path must be absolute");
    Ok(path)
}

fn defaults() -> Value {
    json!({"mode":"standard", "claude_allow":[]})
}

fn validate(value: &Value) -> Result<()> {
    let object = value.as_object().context("permissions must be an object")?;
    ensure!(
        object
            .keys()
            .all(|k| matches!(k.as_str(), "mode" | "claude_allow")),
        "unknown permission setting"
    );
    ensure!(
        matches!(value["mode"].as_str(), Some("standard" | "yolo")),
        "mode must be standard or yolo"
    );
    let rules = value["claude_allow"]
        .as_array()
        .context("claude_allow must be a list")?;
    ensure!(rules.len() <= 64, "at most 64 additional tool rules");
    for rule in rules {
        let rule = rule.as_str().context("tool rules must be strings")?;
        ensure!(
            !rule.is_empty()
                && rule.len() <= 512
                && rule.trim() == rule
                && !rule.starts_with('-')
                && !rule.chars().any(char::is_control),
            "invalid tool rule"
        );
    }
    Ok(())
}

pub fn load() -> Result<Value> {
    let path = path()?;
    match fs::File::open(&path) {
        Ok(file) => {
            let mut raw = Vec::new();
            file.take(65_537).read_to_end(&mut raw)?;
            ensure!(raw.len() <= 65_536, "fleet permissions file too large");
            let value: Value =
                serde_json::from_slice(&raw).context("invalid fleet permissions file")?;
            validate(&value)?;
            Ok(value)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(defaults()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn for_store(store: &Store) -> Result<Value> {
    // Classic task-only homes and isolated runtime pilots are not fleet ships.
    if crate::presence::folder_ship(store).is_some() {
        load()
    } else {
        Ok(defaults())
    }
}

pub fn set(caller: &Caller, args: &Value) -> Result<Value> {
    caller.require_operator("permissions.set")?;
    ensure!(
        args.as_object().is_some_and(|o| o
            .keys()
            .all(|k| matches!(k.as_str(), "mode" | "claude_allow" | "claude_allow_add"))),
        "unknown permission setting"
    );
    // Use the fleet registry's write lock so two captain updates cannot lose
    // each other's fields. The temporary file is private and atomically renamed.
    let mut conn = crate::fleet::registry()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut value = load()?;
    for key in ["mode", "claude_allow"] {
        if let Some(v) = args.get(key) {
            value[key] = v.clone();
        }
    }
    if let Some(rule) = args.get("claude_allow_add") {
        let rules = value["claude_allow"]
            .as_array_mut()
            .context("claude_allow must be a list")?;
        if !rules.contains(rule) {
            rules.push(rule.clone());
        }
    }
    validate(&value)?;
    let path = path()?;
    let temp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(&value)?)?;
        file.sync_all()?;
        fs::rename(&temp, &path)?;
        fs::File::open(path.parent().context("permission directory")?)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result?;
    tx.commit()?;
    Ok(value)
}

pub fn cli(args: &[String], caller: &Caller) -> Result<()> {
    let value = match args {
        [] => load()?,
        [mode] if mode == "show" => load()?,
        [mode] if matches!(mode.as_str(), "standard" | "yolo") => {
            set(caller, &json!({"mode":mode}))?
        }
        [verb, rule] if verb == "allow" => {
            caller.require_operator("permissions.set")?;
            set(caller, &json!({"claude_allow_add":rule}))?
        }
        _ => bail!("usage: privateer permissions [show|standard|yolo|allow 'Tool(rule)']"),
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

/// Turn a native denial into one pending captain question per owning task.
/// No denied input/command is copied into mail, and no permission is granted.
fn blocked(store: &mut Store, args: Value) -> Result<Value> {
    let job_id = required(&args, "job_id")?;
    let job = crate::jobs::dispatch(store, "job.status", json!({"id":job_id}))?;
    let task_id = job["task_id"].as_str().context("job has no task")?;
    store.authorize(task_id, "message")?;
    let owner = store.task(task_id)?["owner"]
        .as_str()
        .context("task owner")?
        .to_owned();
    if let Caller::Agent { actor } = store.caller() {
        ensure!(
            actor == &owner,
            "only the task owner may report a permission block"
        );
    }
    let policy_hash = policy_hash(store)?;
    let existing: Option<String> = store.connection().query_row(
        "SELECT id FROM mail WHERE sender=?1 AND recipient='captain' AND task_id=?2 AND kind='request' AND state='open' AND json_extract(payload,'$.type')='tool_permission' AND json_extract(payload,'$.policy_hash')=?3 LIMIT 1",
        [&owner,task_id,&policy_hash], |r|r.get(0)).optional()?;
    if let Some(id) = existing {
        return Ok(json!({"request_id":id,"duplicate":true}));
    }
    let episode: i64 = store.connection().query_row(
        "SELECT COUNT(*) FROM mail WHERE sender=?1 AND recipient='captain' AND task_id=?2 AND state <> 'open' AND json_extract(payload,'$.type')='tool_permission'", [&owner,task_id], |r|r.get(0))?;
    let id = format!(
        "permissions-{:x}",
        Sha256::digest(format!("{owner}\0{task_id}\0{policy_hash}\0{episode}"))
    );
    crate::mail::dispatch(
        store,
        "mail.send",
        json!({"id":id,"from":owner,"to":"captain","kind":"request","task_id":task_id,
        "request_kind":"tool-permission","body":"Claude could not run an unapproved tool. How should this task proceed? The captain can add a fleet rule with `privateer permissions allow 'Tool(rule)'`, or enable all first mates with `privateer permissions yolo` (Telegram: /yolo on). No settings-file edit is needed. Retry is paused until the policy changes or you answer this request.","payload":{"type":"tool_permission","policy_hash":policy_hash}}),
    )?;
    Ok(json!({"request_id":id,"duplicate":false}))
}

fn policy_hash(store: &Store) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(for_store(store)?.to_string())
    ))
}

pub(crate) fn paused(store: &Store, owner: &str) -> Result<bool> {
    let pending: Option<(i64, Option<String>)> = store.connection().query_row(
        "SELECT seq,task_id FROM mail WHERE sender=?1 AND recipient='captain' AND kind='request' AND state='open' AND json_extract(payload,'$.type')='tool_permission' AND json_extract(payload,'$.policy_hash')=?2 ORDER BY seq DESC LIMIT 1",
        [owner,&policy_hash(store)?], |r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((question, task)) = pending else {
        return Ok(false);
    };
    // Stop retries of already-observed blocked work, not fresh incoming mail.
    // A failed wake covers that new mail too, so it earns one attempt only.
    let through: i64 = store.connection().query_row(
        "SELECT COALESCE(MAX(through_seq),0) FROM wakes WHERE ship=?1",
        [owner],
        |r| r.get(0),
    )?;
    let fresh: bool = store.connection().query_row("SELECT EXISTS(SELECT 1 FROM mail WHERE recipient=?1 AND seq>?2 AND (sender<>'crew' OR task_id IS NOT ?3))",rusqlite::params![owner,question.max(through),task],|r|r.get(0))?;
    Ok(!fresh)
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "permissions.show" => load(),
        "permissions.set" => {
            let value = set(store.caller(), &args)?;
            store.audit("permissions.set", &value)?;
            Ok(value)
        }
        "permissions.blocked" => blocked(store, args),
        _ => bail!("unknown permissions command"),
    }
}
