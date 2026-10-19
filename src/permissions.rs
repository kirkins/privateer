//! Captain-owned launch policy. This changes harness approvals, not charters.
use crate::store::{Caller, Store, required};
use anyhow::{Context, Result, bail, ensure};
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
    // Asks the new policy covers end now; the rest stay open under its hash.
    crate::block_ask::sweep_fleet();
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

/// A denial a ship reports itself: a hosted or attended first mate, whose
/// harness result the runtime cannot read, names the task, tool and input;
/// a job names itself and its denial line is read from its log. Either way
/// it becomes the same one captain decision. No permission is granted.
fn blocked(store: &mut Store, args: Value) -> Result<Value> {
    let (task_id, job_id) = match args.get("job_id").and_then(Value::as_str) {
        Some(job_id) => {
            let job = crate::jobs::dispatch(store, "job.status", json!({"id":job_id}))?;
            let task = job["task_id"]
                .as_str()
                .context("job has no task")?
                .to_owned();
            (task, Some(job_id.to_owned()))
        }
        None => (required(&args, "task_id")?.to_owned(), None),
    };
    store.authorize(&task_id, "message")?;
    let task = store.task(&task_id)?;
    let owner = task["owner"].as_str().context("task owner")?.to_owned();
    if let Caller::Agent { actor } = store.caller() {
        ensure!(
            actor == &owner,
            "only the task owner may report a permission block"
        );
    }
    let denials = match &job_id {
        Some(job_id) => {
            let job = crate::jobs::record(store, job_id)?;
            let log = store
                .home()
                .join("jobs")
                .join(job_id)
                .join(job["incarnation"].as_str().unwrap_or_default())
                .join("stdout.log");
            crate::block_ask::logged_denials(&log)
        }
        None => {
            let tool = required(&args, "tool")?;
            let input = args.get("input").cloned().unwrap_or(json!({}));
            ensure!(
                tool.len() <= 128 && input.to_string().len() <= 16_384,
                "tool is limited to 128 bytes and input to 16 KiB"
            );
            vec![json!({"tool_name": tool, "tool_input": input})]
        }
    };
    let denials = if denials.is_empty() {
        vec![json!({"tool_name": "unknown", "tool_input": {}})]
    } else {
        denials
    };
    let folder = store.home().parent().map(std::path::Path::to_path_buf);
    let (records, more) = crate::block_ask::records(&denials, folder.as_deref());
    let tx = store.write_transaction()?;
    let raised = crate::block_ask::raise(
        store,
        &owner,
        &task_id,
        &task,
        job_id.as_deref(),
        "wake",
        records,
        more,
    )?;
    tx.commit()?;
    Ok(
        json!({"request_id": raised["ask"], "duplicate": raised["duplicate"],
        "declined": raised["declined"], "rules": raised["rules"]}),
    )
}

pub(crate) fn policy_hash(store: &Store) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(for_store(store)?.to_string())
    ))
}

/// Whether a ship's own denial holds back its automatic wakes: an open ask
/// the policy does not yet cover, and no fresh mail since it. A crew's or an
/// errand's ask holds only its task (see `block_ask`).
pub(crate) fn paused(store: &Store, owner: &str) -> Result<bool> {
    let Some((question, task)) = crate::block_ask::pausing_ask(store, owner)? else {
        return Ok(false);
    };
    // Stop retries of already-observed blocked work, not fresh incoming mail.
    // A failed wake covers that new mail too, so it earns one attempt only.
    // A low notice ("still blocked?", a requester's note) wakes nobody, so it
    // is not fresh either.
    let through: i64 = store.connection().query_row(
        "SELECT COALESCE(MAX(through_seq),0) FROM wakes WHERE ship=?1",
        [owner],
        |r| r.get(0),
    )?;
    let fresh: bool = store.connection().query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM mail WHERE recipient=?1 AND seq>?2
               AND (sender<>'crew' OR task_id IS NOT ?3) AND NOT ({}))",
            crate::mail::ROUTINE_MAIL
        ),
        rusqlite::params![owner, question.max(through), task],
        |r| r.get(0),
    )?;
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
