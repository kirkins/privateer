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
        object.keys().all(|k| matches!(
            k.as_str(),
            "mode" | "claude_allow" | "forges" | "authorized"
        )),
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
    if let Some(authorized) = value.get("authorized") {
        let ships = authorized
            .as_object()
            .context("authorized must be an object of ship names")?;
        ensure!(ships.len() <= 64, "at most 64 ships with authorizations");
        for (ship, statements) in ships {
            crate::store::identifier(ship)?;
            let statements = statements
                .as_array()
                .context("a ship's authorizations must be a list")?;
            ensure!(statements.len() <= 16, "at most 16 authorizations per ship");
            for statement in statements {
                let statement = statement
                    .as_str()
                    .context("authorizations must be strings")?;
                ensure!(
                    !statement.is_empty()
                        && statement.len() <= 500
                        && statement.trim() == statement
                        && !statement.chars().any(char::is_control),
                    "an authorization is one line of at most 500 characters"
                );
            }
        }
    }
    if let Some(forges) = value.get("forges") {
        let forges_obj = forges.as_object().context("forges must be an object")?;
        for (forge_name, actions) in forges_obj {
            ensure!(
                !forge_name.is_empty()
                    && forge_name.len() <= 64
                    && forge_name.trim() == forge_name
                    && !forge_name.chars().any(char::is_control),
                "invalid forge name"
            );
            let actions_obj = actions
                .as_object()
                .context("forge actions must be an object")?;
            for (action, rule) in actions_obj {
                ensure!(!action.is_empty(), "action name must not be empty");
                let rule = rule.as_str().context("forge rule must be a string")?;
                ensure!(
                    !rule.is_empty()
                        && rule.len() <= 512
                        && rule.trim() == rule
                        && !rule.starts_with('-')
                        && !rule.chars().any(char::is_control),
                    "invalid forge tool rule"
                );
            }
        }
    }
    Ok(())
}

pub fn normalize_forge_name(forge: &str) -> Result<String> {
    let lower = forge.trim().to_lowercase();
    if lower == "gh" || lower == "github" {
        return Ok("github".to_string());
    }
    let policy = load().unwrap_or_else(|_| defaults());
    if let Some(forges) = policy.get("forges").and_then(Value::as_object) {
        if forges.contains_key(&lower) {
            return Ok(lower);
        }
    }
    bail!("forge must be github or a configured fleet forge")
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

fn read_git_config(folder: &std::path::Path) -> Option<String> {
    let git = folder.join(".git");
    let config_path = if git.is_file() {
        let content = fs::read_to_string(&git).ok()?;
        let gitdir_line = content
            .lines()
            .find(|l| l.trim_start().starts_with("gitdir:"))?;
        let gitdir = gitdir_line.trim_start()["gitdir:".len()..].trim();
        let p = std::path::Path::new(gitdir);
        if p.is_absolute() {
            p.join("config")
        } else {
            folder.join(p).join("config")
        }
    } else if git.is_dir() {
        git.join("config")
    } else if folder.join("config").is_file() && folder.join("HEAD").is_file() {
        folder.join("config")
    } else {
        return None;
    };
    fs::read_to_string(config_path).ok()
}

fn detect_folder_forge(
    folder: &std::path::Path,
    custom_forges: &std::collections::HashSet<String>,
) -> Option<String> {
    let content = read_git_config(folder)?;
    let mut current_section = String::new();
    let mut forge = None;
    let mut origin_url = None;

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            current_section = line[1..line.len() - 1].trim().to_lowercase();
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let key = k.trim().to_lowercase();
            let val = v.trim();
            if current_section == "privateer" && key == "forge" {
                forge = Some(val.to_string());
            } else if current_section.starts_with("remote")
                && (current_section.contains("\"origin\"") || current_section.contains("'origin'"))
                && key == "url"
            {
                origin_url = Some(val.to_string());
            }
        }
    }

    if let Some(f) = forge {
        let lower = f.to_lowercase();
        if lower == "gh" || lower == "github" {
            return Some("github".to_string());
        }
        if custom_forges.contains(&lower) {
            return Some(lower);
        }
    }
    if let Some(url) = origin_url {
        if url.to_lowercase().contains("github.com") {
            return Some("github".to_string());
        }
    }
    None
}

pub(crate) fn detect_forges(store: &Store, _ship: &str, charter: &Value) -> Vec<String> {
    let policy = for_store(store).unwrap_or_else(|_| defaults());
    let custom_forges: std::collections::HashSet<String> = policy
        .get("forges")
        .and_then(Value::as_object)
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();

    let mut forges = Vec::new();
    if let Some(f) = charter.get("forge").and_then(Value::as_str) {
        let lower = f.to_lowercase();
        if lower == "gh" || lower == "github" {
            forges.push("github".to_string());
        } else if custom_forges.contains(&lower) {
            forges.push(lower);
        }
    }

    let folder = charter
        .get("repo")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| store.home().parent().map(std::path::Path::to_path_buf));

    if let Some(folder) = &folder {
        let projects_dir = folder.join("projects");
        if let Ok(entries) = fs::read_dir(&projects_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && !entry.file_name().to_string_lossy().starts_with('.') {
                    if let Some(f) = detect_folder_forge(&path, &custom_forges) {
                        if !forges.contains(&f) {
                            forges.push(f);
                        }
                    }
                }
            }
        }
        if let Some(f) = detect_folder_forge(folder, &custom_forges) {
            if !forges.contains(&f) {
                forges.push(f);
            }
        }
    }

    if forges.is_empty() {
        forges.push("github".to_string());
    }
    forges
}

pub fn set(caller: &Caller, args: &Value) -> Result<Value> {
    set_with_change(caller, args).map(|(value, _)| value)
}

fn set_with_change(caller: &Caller, args: &Value) -> Result<(Value, bool)> {
    caller.require_operator("permissions.set")?;
    ensure!(
        args.as_object().is_some_and(|o| o.keys().all(|k| matches!(
            k.as_str(),
            "mode"
                | "claude_allow"
                | "claude_allow_add"
                | "forges"
                | "authorized"
                | "authorize"
                | "unauthorize"
        ))),
        "unknown permission setting"
    );
    // Use the fleet registry's write lock so two captain updates cannot lose
    // each other's fields. The temporary file is private and atomically renamed.
    let mut conn = crate::fleet::registry()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut value = load()?;
    let previous_mode = value["mode"].clone();
    for key in ["mode", "claude_allow", "forges", "authorized"] {
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
    if let Some(grant) = args.get("authorize") {
        let (ship, text) = authorization(grant)?;
        let list = value["authorized"][ship]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if !list.iter().any(|s| s == text) {
            value["authorized"][ship] = json!([list, vec![json!(text)]].concat());
        }
    }
    if let Some(grant) = args.get("unauthorize") {
        let (ship, text) = authorization(grant)?;
        let list = value["authorized"][ship]
            .as_array()
            .cloned()
            .unwrap_or_default();
        ensure!(
            list.iter().any(|s| s == text),
            "{ship} has no such authorization"
        );
        let kept: Vec<Value> = list.into_iter().filter(|s| s != text).collect();
        let ships = value["authorized"].as_object_mut().context("authorized")?;
        if kept.is_empty() {
            ships.remove(ship);
        } else {
            ships.insert(ship.to_owned(), json!(kept));
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
    let changed = previous_mode != value["mode"];
    Ok((value, changed))
}

/// Operator gateway verbs only touch this machine, never its links. The write
/// accepts exactly one boolean; paths and general policy edits cannot cross it.
pub(crate) fn serve(caller: &Caller, command: &str, args: &Value) -> Result<Value> {
    caller.require_operator(command)?;
    match command {
        "permissions.status" => {
            ensure!(
                args.as_object().is_some_and(|o| o.is_empty()),
                "status takes no arguments"
            );
            Ok(json!({"mode": load()?["mode"]}))
        }
        "permissions.yolo" => {
            ensure!(
                args.as_object()
                    .is_some_and(|o| o.len() == 1 && o.contains_key("on")),
                "yolo takes only on: boolean"
            );
            let on = args["on"].as_bool().context("on must be a boolean")?;
            let (value, changed) = set_with_change(
                caller,
                &json!({"mode": if on { "yolo" } else { "standard" }}),
            )?;
            Ok(json!({"mode":value["mode"], "changed":changed}))
        }
        _ => bail!("unknown permissions gateway command"),
    }
}

/// One bounded gateway call per direct link. Reverse-only links stay visible,
/// but have no operator transport. A missing acknowledgement is not success:
/// a timed-out write might have landed and must be checked with status.
pub(crate) fn across_machines(caller: &Caller, on: Option<bool>) -> Result<Value> {
    let (command, args) = match on {
        Some(on) => ("permissions.yolo", json!({"on":on})),
        None => ("permissions.status", json!({})),
    };
    caller.require_operator(command)?;
    let targets = crate::machines::permission_targets()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut rows = Vec::new();
    let local = serve(caller, command, &args);
    rows.push(machine_result(
        &crate::machines::this_machine(),
        true,
        on,
        local,
    ));
    for (name, reachable) in targets {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let result = if !reachable {
            Err(anyhow::anyhow!("reverse-only link; no operator transport"))
        } else if remaining.is_zero() {
            Err(anyhow::anyhow!(
                "round time budget exhausted; not contacted"
            ))
        } else {
            crate::machines::call_reached(
                &name,
                command,
                args.clone(),
                Some(remaining.min(std::time::Duration::from_secs(5))),
            )
        };
        rows.push(machine_result(&name, false, on, result));
    }
    Ok(json!({"machines":rows}))
}

fn machine_result(name: &str, local: bool, on: Option<bool>, result: Result<Value>) -> Value {
    let result = result.and_then(|value| {
        ensure!(
            matches!(value["mode"].as_str(), Some("standard" | "yolo")),
            "invalid gateway mode"
        );
        if let Some(on) = on {
            ensure!(
                value["mode"] == (if on { "yolo" } else { "standard" })
                    && value["changed"].is_boolean(),
                "unconfirmed gateway write"
            );
        }
        Ok(value)
    });
    match result {
        Ok(value) => {
            let mut row =
                json!({"name":name, "local":local, "mode":value["mode"], "confirmed":true});
            if on.is_some() {
                row["changed"] = value["changed"].clone();
            }
            row
        }
        Err(error) => {
            let error = format!("{error:#}");
            let remote_error = error.strip_prefix(&format!("machine {name}: "));
            let reason = if error.contains("unknown gateway command") {
                "older version"
            } else if error.contains("reverse-only link") {
                "reverse-only link"
            } else if error.contains("time budget exhausted") {
                "not contacted (time limit)"
            } else if error == "link has no local name" {
                "link has no local name"
            } else if local {
                "local error"
            } else if matches!(
                error.as_str(),
                "invalid gateway mode" | "unconfirmed gateway write"
            ) {
                "invalid reply"
            } else if let Some(message) = remote_error {
                return json!({"name":name, "local":local, "mode":null, "confirmed":false,
                    "reason":remote_error_reason(message), "error":error});
            } else {
                "unreachable"
            };
            json!({"name":name, "local":local, "mode":null, "confirmed":false, "reason":reason, "error":error})
        }
    }
}

/// Keep gateway diagnostics short and path-free in phone replies. Only an
/// explicit authorization denial is a refusal, not a failed policy operation.
fn remote_error_reason(message: &str) -> String {
    if message.ends_with(" is an operator command; agents cannot run it") {
        return "refused".to_owned();
    }
    let safe = message
        .split_whitespace()
        .map(|word| {
            if word.contains(['/', '\\']) {
                "[path]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let short: String = safe.chars().filter(|c| !c.is_control()).take(80).collect();
    format!("error: {short}")
}

/// `{"ship","text"}` of one standing authorization.
fn authorization(grant: &Value) -> Result<(&str, &str)> {
    Ok((required(grant, "ship")?, required(grant, "text")?))
}

pub fn cli(args: &[String], caller: &Caller) -> Result<()> {
    let mut write = false;
    let value = match args {
        [verb, rest @ ..] if verb == "project" => crate::project_permissions::cli(rest, caller)?,
        [] => load()?,
        [mode] if mode == "show" => load()?,
        [mode] if matches!(mode.as_str(), "standard" | "yolo") => {
            write = true;
            across_machines(caller, Some(mode == "yolo"))?
        }
        [verb, action] if verb == "yolo" && action == "status" => across_machines(caller, None)?,
        [verb, action] if verb == "yolo" && matches!(action.as_str(), "on" | "off") => {
            write = true;
            across_machines(caller, Some(action == "on"))?
        }
        [verb, rule] if verb == "allow" => {
            caller.require_operator("permissions.set")?;
            set(caller, &json!({"claude_allow_add":rule}))?
        }
        [verb, ship, text] if matches!(verb.as_str(), "authorize" | "unauthorize") => {
            let mut request = json!({});
            request[verb.as_str()] = json!({"ship": ship, "text": text});
            set(caller, &request)?
        }
        _ => bail!(
            "usage: privateer permissions [project SHIP/NAME (accept [RULE ...]|show|decline|clear)|show|standard|yolo [on|off|status]|allow 'Tool(rule)'|authorize SHIP TEXT|unauthorize SHIP TEXT|approvals [list [SHIP]|show ID|revoke ID]]"
        ),
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    if let Some(rows) = value["machines"].as_array() {
        ensure!(
            !rows
                .iter()
                .any(|row| row["confirmed"] != true && (write || row["local"] == true)),
            if write {
                "permission change not confirmed on every machine"
            } else {
                "could not read this machine's permissions"
            }
        );
    }
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
        vec![crate::block_ask::unidentified()]
    } else {
        denials
    };
    let folder = store.home().parent().map(std::path::Path::to_path_buf);
    let (records, more) = crate::block_ask::records(&denials, folder.as_deref());
    // A crew's or an errand's job is asked about as theirs, never as a wake.
    let scope = match &job_id {
        Some(job_id) => crate::block_ask::job_scope(store, job_id)?,
        None => "wake",
    };
    let tx = store.write_transaction()?;
    let raised = crate::block_ask::raise(
        store,
        &owner,
        &task_id,
        &task,
        job_id.as_deref(),
        scope,
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

/// The argument keys of each command here; see `crate::Accepted`.
pub(crate) const ACCEPTED: crate::Accepted = &[
    (
        "permissions.blocked",
        &["job_id", "task_id", "tool", "input"],
    ),
    (
        "permissions.set",
        &[
            "mode",
            "claude_allow",
            "claude_allow_add",
            "forges",
            "authorized",
            "authorize",
            "unauthorize",
        ],
    ),
    ("permissions.show", &[]),
];

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
