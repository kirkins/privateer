use crate::store::{Store, identifier, reject_symlinks, required};
use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const ACTIONS: &[&str] = &[
    "inspect",
    "spawn",
    "implement",
    "test",
    "publish_pr",
    "merge_green",
    "message",
    "recovery",
    "cleanup",
    "deploy_production",
];
const DEFAULT_ACTIONS: &[&str] = &[
    "inspect",
    "spawn",
    "implement",
    "test",
    "publish_pr",
    "merge_green",
    "message",
    "recovery",
    "cleanup",
];

fn limit(args: &Value, default: u64) -> u64 {
    args.get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(default)
        .clamp(1, 1000)
}

fn hash(value: &Value) -> String {
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}

fn charter_put(store: &Store, args: Value) -> Result<Value> {
    let id = required(&args, "id")?;
    identifier(id)?;
    let tx = rusqlite::Transaction::new_unchecked(
        store.connection(),
        rusqlite::TransactionBehavior::Immediate,
    )?;
    let previous: Option<String> = tx
        .query_row("SELECT data FROM charters WHERE id=?1", [id], |r| r.get(0))
        .optional()?;
    let previous = previous
        .map(|s| serde_json::from_str::<Value>(&s))
        .transpose()?;
    let mut charter = previous.unwrap_or_else(|| json!({
        "id":id,"actions":DEFAULT_ACTIONS,"active":true,"production_requires_approval":true,
        "max_delegation_depth":4,"job_timeout_secs":900,"min_free_bytes":2147483648_u64,"max_heavy_jobs":1
    }));
    charter["revision"] = json!(charter.get("revision").and_then(Value::as_u64).unwrap_or(0) + 1);
    for key in [
        "objective",
        "repo",
        "actions",
        "max_delegation_depth",
        "job_timeout_secs",
        "min_free_bytes",
        "max_heavy_jobs",
        "provider_plugin",
        "active",
    ] {
        if let Some(v) = args.get(key) {
            charter[key] = v.clone();
        }
    }
    if !charter["active"].is_boolean() {
        bail!("active must be a boolean");
    }
    if let Some(plugin) = charter.get("provider_plugin") {
        if !plugin.is_null() {
            if !plugin.is_object() {
                bail!("provider_plugin must be an object with manifest and optional config paths");
            }
            for key in ["manifest", "config"] {
                if key == "config" && plugin.get(key).is_none() {
                    continue;
                }
                let file = Path::new(required(plugin, key)?);
                if !file.is_absolute() || !file.is_file() {
                    bail!("provider_plugin {key} must be an existing absolute file");
                }
            }
        }
    }
    if args
        .get("production_requires_approval")
        .is_some_and(|v| v != true)
    {
        bail!("production approval is required in this version");
    }
    required(&charter, "objective")?;
    let repo = Path::new(required(&charter, "repo")?);
    if !repo.is_absolute() || !repo.is_dir() {
        bail!("repo must be an existing absolute directory");
    }
    charter["repo"] = json!(fs::canonicalize(repo)?.to_string_lossy());
    let actions = charter["actions"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("actions must be an array"))?;
    if actions.is_empty()
        || actions
            .iter()
            .any(|a| !a.as_str().is_some_and(|a| ACTIONS.contains(&a)))
    {
        bail!("actions include an unknown action or are empty");
    }
    for (key, min, max) in [
        ("max_delegation_depth", 0, 16),
        ("job_timeout_secs", 1, 86400),
        ("max_heavy_jobs", 1, 64),
        ("min_free_bytes", 0, u64::MAX),
    ] {
        let n = charter[key]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("{key} must be an unsigned integer"))?;
        if n < min || n > max {
            bail!("{key} is outside its supported range");
        }
    }
    tx.execute("INSERT INTO charters(id,data) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET data=excluded.data",params![id,charter.to_string()])?;
    store.audit("charter.put", &json!({"id":id,"hash":hash(&charter)}))?;
    tx.commit()?;
    Ok(charter)
}

fn task_create(store: &Store, args: Value) -> Result<Value> {
    let id = required(&args, "id")?;
    let owner = required(&args, "owner")?;
    let title = required(&args, "title")?;
    identifier(id)?;
    identifier(owner)?;
    let parent = args.get("parent").and_then(Value::as_str);
    let tx = store.connection().unchecked_transaction()?;
    let (charter_id, depth) = if let Some(parent) = parent {
        let task = store.task(parent)?;
        store.authorize(parent, "spawn")?;
        let cid = task["charter_id"].as_str().unwrap().to_owned();
        if args
            .get("charter_id")
            .and_then(Value::as_str)
            .is_some_and(|v| v != cid)
        {
            bail!("a child inherits its parent's charter");
        }
        (cid, task["depth"].as_i64().unwrap() + 1)
    } else {
        (required(&args, "charter_id")?.to_owned(), 0)
    };
    let charter = store.charter(&charter_id)?;
    if charter["active"] != true {
        bail!("charter was revoked");
    }
    if depth as u64 > charter["max_delegation_depth"].as_u64().unwrap_or(4) {
        bail!("delegation depth exhausted");
    }
    if let Ok(existing) = store.task(id) {
        if existing["owner"] != owner
            || existing["title"] != title
            || existing["charter_id"] != charter_id
            || existing["parent"] != json!(parent)
        {
            bail!("task id already belongs to a different assignment");
        }
        tx.commit()?;
        return Ok(existing);
    }
    tx.execute("INSERT INTO tasks(id,charter_id,owner,title,status,parent,depth) VALUES(?1,?2,?3,?4,'open',?5,?6)",
        params![id,charter_id,owner,title,parent,depth])?;
    store.audit(
        "task.create",
        &json!({"id":id,"owner":owner,"parent":parent,"charter_id":charter_id}),
    )?;
    tx.commit()?;
    store.task(id)
}

fn task_change(store: &Store, command: &str, args: Value) -> Result<Value> {
    let id = required(&args, "id")?;
    let actor = required(&args, "actor")?;
    let tx = store.connection().unchecked_transaction()?;
    let task = store.task(id)?;
    if command == "task.accept" {
        if task["handoff_to"].as_str() != Some(actor) {
            bail!("no handoff offered to this actor");
        }
        store.authorize(id, "inspect")?;
        tx.execute(
            "UPDATE tasks SET owner=?2,handoff_to=NULL WHERE id=?1",
            params![id, actor],
        )?;
    } else {
        if task["owner"] != actor {
            bail!("only the task owner can change this task");
        }
        if task["status"] == "done" || task["status"] == "cancelled" {
            bail!("task is terminal");
        }
        if command == "task.handoff" {
            let target = required(&args, "target")?;
            identifier(target)?;
            if target == actor {
                bail!("handoff target is already the owner");
            }
            tx.execute(
                "UPDATE tasks SET handoff_to=?2 WHERE id=?1",
                params![id, target],
            )?;
        } else {
            let status = required(&args, "status")?;
            if !["open", "running", "blocked", "done", "cancelled"].contains(&status) {
                bail!("invalid task status");
            }
            tx.execute(
                "UPDATE tasks SET status=?2 WHERE id=?1",
                params![id, status],
            )?;
        }
    }
    store.audit(command, &json!({"id":id,"actor":actor}))?;
    tx.commit()?;
    store.task(id)
}

fn event_emit(store: &Store, args: Value) -> Result<Value> {
    let id = required(&args, "id")?;
    let task_id = required(&args, "task_id")?;
    let issue = required(&args, "issue_key")?;
    let kind = required(&args, "kind")?;
    identifier(id)?;
    identifier(issue)?;
    identifier(kind)?;
    if args.to_string().len() > 65536 {
        bail!("event exceeds 64 KiB");
    }
    store.task(task_id)?;
    let payload = args.get("payload").cloned().unwrap_or_else(|| json!({}));
    let tx = store.connection().unchecked_transaction()?;
    let old = tx
        .query_row(
            "SELECT seq,task_id,issue_key,kind,payload FROM events WHERE id=?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    if let Some((seq, t, i, k, p)) = old {
        if t != task_id || i != issue || k != kind || serde_json::from_str::<Value>(&p)? != payload
        {
            bail!("event id has different content");
        }
        tx.commit()?;
        return Ok(json!({"seq":seq,"duplicate":true}));
    }
    tx.execute(
        "INSERT INTO events(id,task_id,issue_key,kind,payload) VALUES(?1,?2,?3,?4,?5)",
        params![id, task_id, issue, kind, payload.to_string()],
    )?;
    let seq = tx.last_insert_rowid();
    tx.execute("INSERT INTO pending(task_id,issue_key,seq) VALUES(?1,?2,?3) ON CONFLICT(task_id,issue_key) DO UPDATE SET seq=excluded.seq",params![task_id,issue,seq])?;
    tx.commit()?;
    Ok(json!({"seq":seq,"duplicate":false}))
}

fn event_list(store: &Store, command: &str, args: Value) -> Result<Value> {
    let task_id = args.get("task_id").and_then(Value::as_str);
    let after = args
        .get("after_seq")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    let sql = if command == "event.pending" {
        "SELECT e.seq,e.id,e.task_id,e.issue_key,e.kind,e.payload FROM events e JOIN pending p ON p.seq=e.seq WHERE (?1 IS NULL OR e.task_id=?1) AND e.seq>?2 ORDER BY e.seq LIMIT ?3"
    } else {
        "SELECT e.seq,e.id,e.task_id,e.issue_key,e.kind,e.payload FROM events e WHERE (?1 IS NULL OR e.task_id=?1) AND e.seq>?2 ORDER BY e.seq LIMIT ?3"
    };
    let mut statement = store.connection().prepare(sql)?;
    let mut events = Vec::new();
    for row in statement.query_map(params![task_id, after, limit(&args, 40)], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
        ))
    })? {
        let (seq, id, task_id, issue_key, kind, payload) = row?;
        events.push(json!({"seq":seq,"id":id,"task_id":task_id,"issue_key":issue_key,"kind":kind,"payload":serde_json::from_str::<Value>(&payload)?}));
    }
    Ok(json!({"events":events}))
}

fn approval(store: &Store, command: &str, args: Value) -> Result<Value> {
    let task = required(&args, "task_id")?;
    let artifact = required(&args, "artifact")?;
    let environment = required(&args, "environment")?;
    // The explicit approval supplies authority for this one artifact/environment.
    // It must not require widening every task's standing charter first.
    let charter = store.authorize(task, "inspect")?;
    let charter_hash = hash(&charter);
    let tx = store.connection().unchecked_transaction()?;
    let id = if command == "approval.grant" {
        let id = args
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        identifier(&id)?;
        // Replaying a grant can never reset an already consumed approval.
        let existing = tx.query_row("SELECT task_id,artifact,environment,charter_hash,consumed FROM approvals WHERE id=?1",[&id],|r| {
            Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,i64>(4)?))
        }).optional()?;
        if let Some((t, a, e, h, c)) = existing {
            if t != task || a != artifact || e != environment || h != charter_hash || c != 0 {
                bail!("approval id cannot be reused");
            }
        } else {
            tx.execute("INSERT INTO approvals(id,task_id,artifact,environment,charter_hash) VALUES(?1,?2,?3,?4,?5)",params![id,task,artifact,environment,charter_hash])?;
        }
        id
    } else {
        let id = required(&args, "id")?.to_owned();
        let count = tx.execute("UPDATE approvals SET consumed=1 WHERE id=?1 AND task_id=?2 AND artifact=?3 AND environment=?4 AND charter_hash=?5 AND consumed=0",params![id,task,artifact,environment,charter_hash])?;
        if count != 1 {
            bail!("no matching unused approval");
        }
        id
    };
    store.audit(
        command,
        &json!({"id":id,"task_id":task,"artifact":artifact,"environment":environment}),
    )?;
    tx.commit()?;
    Ok(
        json!({"id":id,"task_id":task,"artifact":artifact,"environment":environment,"consumed":command=="approval.consume"}),
    )
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "init" => {
            let config = store.home().join("config.json");
            reject_symlinks(&config)?;
            let mut data = if config.exists() {
                serde_json::from_slice::<Value>(&fs::read(&config)?)?
            } else {
                json!({"schema_version":1})
            };
            if let Some(path) = args.get("resource_home") {
                let path = Path::new(
                    path.as_str()
                        .ok_or_else(|| anyhow::anyhow!("resource_home must be a path"))?,
                );
                if !path.is_absolute() {
                    bail!("resource_home must be absolute");
                }
                reject_symlinks(path)?;
                data["resource_home"] = json!(path);
            }
            let temp = store
                .home()
                .join(format!("config-{}.tmp", uuid::Uuid::new_v4()));
            fs::write(&temp, serde_json::to_vec_pretty(&data)?)?;
            fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
            fs::rename(temp, config)?;
            Ok(json!({"home":store.home(),"version":env!("CARGO_PKG_VERSION"),"config":data}))
        }
        "charter.put" => charter_put(store, args),
        "charter.show" => store.charter(required(&args, "id")?),
        "charter.revoke" => {
            let id = required(&args, "id")?;
            let tx = store.connection().unchecked_transaction()?;
            let mut c = store.charter(id)?;
            c["active"] = json!(false);
            c["revision"] = json!(c.get("revision").and_then(Value::as_u64).unwrap_or(0) + 1);
            tx.execute(
                "UPDATE charters SET data=?2 WHERE id=?1",
                params![id, c.to_string()],
            )?;
            store.audit(command, &json!({"id":id}))?;
            tx.commit()?;
            Ok(c)
        }
        "task.create" => task_create(store, args),
        "task.show" => store.task(required(&args, "id")?),
        "task.update" | "task.handoff" | "task.accept" => task_change(store, command, args),
        "task.list" => {
            let mut s=store.connection().prepare("SELECT id FROM tasks WHERE (?1 IS NULL OR owner=?1) AND (?2 IS NULL OR status=?2) ORDER BY id LIMIT ?3")?;
            let mut tasks = Vec::new();
            for id in s.query_map(
                params![
                    args.get("owner").and_then(Value::as_str),
                    args.get("status").and_then(Value::as_str),
                    limit(&args, 100)
                ],
                |r| r.get::<_, String>(0),
            )? {
                tasks.push(store.task(&id?)?);
            }
            Ok(json!({"tasks":tasks}))
        }
        "event.emit" => event_emit(store, args),
        "event.pending" | "event.history" => event_list(store, command, args),
        "event.ack" => {
            let task = required(&args, "task_id")?;
            let issue = required(&args, "issue_key")?;
            let seq = args
                .get("through_seq")
                .and_then(Value::as_i64)
                .filter(|v| *v > 0)
                .ok_or_else(|| anyhow::anyhow!("positive through_seq is required"))?;
            let tx = store.connection().unchecked_transaction()?;
            let observed: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM events WHERE seq=?1 AND task_id=?2 AND issue_key=?3)",
                params![seq, task, issue],
                |r| r.get(0),
            )?;
            if !observed {
                bail!("ack must refer to an observed event for this issue");
            }
            let removed = tx.execute(
                "DELETE FROM pending WHERE task_id=?1 AND issue_key=?2 AND seq<=?3",
                params![task, issue, seq],
            )?;
            tx.commit()?;
            Ok(json!({"acknowledged":removed==1,"through_seq":seq}))
        }
        "message.send" => {
            let task = required(&args, "task_id")?;
            store.authorize(task, "message")?;
            let from = required(&args, "from")?;
            let to = required(&args, "to")?;
            let body = required(&args, "body")?;
            identifier(from)?;
            identifier(to)?;
            let id = required(&args, "id")?;
            event_emit(
                store,
                json!({"id":id,"task_id":task,"issue_key":format!("message:{to}:{id}"),"kind":"message","payload":{"from":from,"to":to,"body":body}}),
            )
        }
        "message.inbox" => {
            let actor = required(&args, "actor")?;
            let mut s=store.connection().prepare("SELECT seq,id,task_id,payload FROM events WHERE kind='message' AND json_extract(payload,'$.to')=?1 AND seq>?2 ORDER BY seq LIMIT ?3")?;
            let mut messages = Vec::new();
            for row in s.query_map(
                params![
                    actor,
                    args.get("after_seq").and_then(Value::as_i64).unwrap_or(0),
                    limit(&args, 40)
                ],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )? {
                let (seq, id, task_id, payload) = row?;
                messages.push(json!({"seq":seq,"id":id,"task_id":task_id,"payload":serde_json::from_str::<Value>(&payload)?}));
            }
            Ok(json!({"messages":messages}))
        }
        "approval.grant" | "approval.consume" => approval(store, command, args),
        "delivery.check" => {
            let task = required(&args, "task_id")?;
            let charter = store.authorize(task, "merge_green")?;
            let head = required(&args, "current_head")?;
            if args.get("tested_head").and_then(Value::as_str) != Some(head)
                || args.get("checks_passed") != Some(&json!(true))
            {
                bail!("passing checks must apply to the current head");
            }
            if args.get("deploys_production") == Some(&json!(true)) {
                let env = required(&args, "environment")?;
                let matching: bool=store.connection().query_row("SELECT EXISTS(SELECT 1 FROM approvals WHERE task_id=?1 AND artifact=?2 AND environment=?3 AND charter_hash=?4 AND consumed=0)",params![task,head,env,hash(&charter)],|r|r.get(0))?;
                if !matching {
                    bail!("production requires approval of this exact artifact and environment");
                }
            }
            Ok(
                json!({"allowed":true,"task_id":task,"head":head,"evidence":"caller-supplied; forge adapter must verify live checks"}),
            )
        }
        "audit.list" => {
            let mut s = store.connection().prepare(
                "SELECT seq,time_ms,kind,payload FROM audit WHERE seq>?1 ORDER BY seq LIMIT ?2",
            )?;
            let mut events = Vec::new();
            for row in s.query_map(
                params![
                    args.get("after_seq").and_then(Value::as_i64).unwrap_or(0),
                    limit(&args, 100)
                ],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )? {
                let (seq, time, kind, payload) = row?;
                events.push(json!({"seq":seq,"time_ms":time,"kind":kind,"payload":serde_json::from_str::<Value>(&payload)?}));
            }
            Ok(json!({"events":events}))
        }
        "status" => {
            let tasks: i64 = store.connection().query_row(
                "SELECT COUNT(*) FROM tasks WHERE status NOT IN ('done','cancelled')",
                [],
                |r| r.get(0),
            )?;
            let facts: i64 =
                store
                    .connection()
                    .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?;
            let pending: i64 =
                store
                    .connection()
                    .query_row("SELECT COUNT(*) FROM pending", [], |r| r.get(0))?;
            Ok(
                json!({"version":env!("CARGO_PKG_VERSION"),"home":store.home(),"open_tasks":tasks,"event_facts":facts,"pending_notifications":pending}),
            )
        }
        _ => bail!("unknown command {command}"),
    }
}
