use crate::store::{Store, identifier, reject_symlinks, required};
use anyhow::{Context, Result, bail, ensure};
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
    "publish_review",
    "merge_green",
    "message",
    "recovery",
    "cleanup",
    "deploy_production",
];
/// A new charter's actions: everything but production deployment.
pub const DEFAULT_ACTIONS: &[&str] = &[
    "inspect",
    "spawn",
    "implement",
    "test",
    "publish_pr",
    "publish_review",
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

pub(crate) fn hash(value: &Value) -> String {
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}

fn charter_put(store: &Store, args: Value) -> Result<Value> {
    let id = required(&args, "id")?;
    identifier(id)?;
    let tx = store.write_transaction()?;
    let previous: Option<String> = tx
        .query_row("SELECT data FROM charters WHERE id=?1", [id], |r| r.get(0))
        .optional()?;
    let previous = previous
        .map(|s| serde_json::from_str::<Value>(&s))
        .transpose()?;
    let mut charter = previous.unwrap_or_else(|| {
        json!({
            "id":id,"actions":DEFAULT_ACTIONS,"active":true,"production_requires_approval":true,
            "max_delegation_depth":4,"job_timeout_secs":900,"min_free_bytes":2147483648_u64
        })
    });
    charter["revision"] = json!(charter.get("revision").and_then(Value::as_u64).unwrap_or(0) + 1);
    for key in [
        "objective",
        "repo",
        "actions",
        "forge",
        "max_delegation_depth",
        "job_timeout_secs",
        "min_free_bytes",
        "max_heavy_jobs",
        "crew_max_parks",
        "crew_max_elapsed_secs",
        "provider_plugin",
        "active",
    ] {
        if let Some(v) = args.get(key) {
            charter[key] = v.clone();
        }
    }
    if let Some(forge) = charter.get("forge").filter(|v| !v.is_null()) {
        let forge_str = forge.as_str().context("forge must be a string")?;
        let normalized = crate::permissions::normalize_forge_name(forge_str)?;
        charter["forge"] = json!(normalized);
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
        ("max_heavy_jobs", 1, 1024),
        ("min_free_bytes", 0, u64::MAX),
        ("crew_max_parks", 0, 64),
        ("crew_max_elapsed_secs", 1, 7 * 86400),
    ] {
        // Optional per-charter settings: a share of the host's heavy work, and
        // the crew park caps, which default when absent.
        if matches!(
            key,
            "max_heavy_jobs" | "crew_max_parks" | "crew_max_elapsed_secs"
        ) && charter.get(key).is_none_or(Value::is_null)
        {
            continue;
        }
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
    let tx = store.write_transaction()?;
    // A ship creates only tasks it will own: custody moves by `task.handoff`
    // and the recipient's `task.accept`, never by assignment.
    if let crate::store::Caller::Agent { actor } = store.caller() {
        ensure!(
            owner == actor,
            "{actor} cannot create a task owned by {owner}; create it as your own and hand it off with task.handoff"
        );
    }
    let (charter_id, depth) = if let Some(parent) = parent {
        let task = store.task(parent)?;
        if let crate::store::Caller::Agent { actor } = store.caller() {
            ensure!(
                task["owner"] == *actor,
                "{actor} does not own task {parent}, so it cannot delegate under it"
            );
        }
        store.authorize(parent, "spawn")?;
        let cid = task["charter_id"]
            .as_str()
            .context("parent task charter missing")?
            .to_owned();
        if args
            .get("charter_id")
            .and_then(Value::as_str)
            .is_some_and(|v| v != cid)
        {
            bail!("a child inherits its parent's charter");
        }
        let depth = task["depth"]
            .as_i64()
            .context("parent task depth missing")?;
        (cid, depth + 1)
    } else {
        let cid = required(&args, "charter_id")?.to_owned();
        // A ship's charter is named after it; the operator may also have
        // placed a task of the ship under another, which binds it as well.
        if let crate::store::Caller::Agent { actor } = store.caller() {
            let bound = cid == *actor
                || store
                    .connection()
                    .query_row(
                        "SELECT 1 FROM tasks WHERE charter_id=?1 AND owner=?2 LIMIT 1",
                        params![cid, actor],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
            ensure!(
                bound,
                "{actor} holds no task under charter {cid}; create the task under one of its own"
            );
        }
        (cid, 0)
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

/// `task.rank {id, rank}`: where an open task stands in its ship's backlog,
/// 1 first; a null rank returns it to oldest-first among the unranked. The
/// owner orders its own backlog; the operator may order any.
pub(crate) fn task_rank(store: &Store, args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    let task = store.task(id)?;
    let owner = task["owner"].as_str().unwrap_or_default().to_owned();
    if let crate::store::Caller::Agent { actor } = store.caller() {
        anyhow::ensure!(*actor == owner, "only {owner} orders its own backlog");
    }
    anyhow::ensure!(
        task["status"] == "open",
        "only open tasks wait in the backlog; {id} is {}",
        task["status"].as_str().unwrap_or("")
    );
    let rank = match args.get("rank").filter(|v| !v.is_null()) {
        None => None,
        Some(value) => Some(
            value
                .as_i64()
                .filter(|n| (1..=10_000).contains(n))
                .ok_or_else(|| anyhow::anyhow!("rank is 1..10000, or null to clear it"))?,
        ),
    };
    store
        .connection()
        .execute("UPDATE tasks SET rank=?2 WHERE id=?1", params![id, rank])?;
    store.audit("task.rank", &json!({"id": id, "rank": rank}))?;
    Ok(json!({"id": id, "rank": rank, "backlog": crate::context::backlog(store, &owner)?}))
}

fn task_change(store: &Store, command: &str, args: Value) -> Result<Value> {
    let id = required(&args, "id")?;
    let actor = required(&args, "actor")?;
    let tx = store.write_transaction()?;
    let task = store.task(id)?;
    let mut closed = None;
    if command == "task.accept" {
        if task["handoff_to"].as_str() != Some(actor) {
            bail!("no handoff offered to this actor");
        }
        // Custody grants no new work; the new owner's actions stay bounded by
        // the charter.
        store.active_charter(id)?;
        tx.execute(
            "UPDATE tasks SET owner=?2,handoff_to=NULL WHERE id=?1",
            params![id, actor],
        )?;
        // Ships waiting on this task named its old owner, which will never
        // report its end; and a remote wait this task held was its old
        // owner's subscription.
        let old_owner = task["owner"].as_str().context("task owner missing")?;
        crate::waits::handed_off(store, id, old_owner, actor)?;
        crate::scheduling::settle(store, id)?;
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
            // `task.update` may instead set the ships a task is shared with:
            // the owner's visibility decision for cross-ship waits, not a
            // status change. A null list clears it. Terminal tasks still
            // refuse every update.
            let shared = match args.get("shared_with") {
                Some(value) if command == "task.update" => {
                    let empty = json!([]);
                    Some(crate::waits::shared_names(if value.is_null() {
                        &empty
                    } else {
                        value
                    })?)
                }
                _ => None,
            };
            if let Some(names) = &shared {
                tx.execute(
                    "UPDATE tasks SET shared_with=?2 WHERE id=?1",
                    params![id, names.to_string()],
                )?;
                store.audit(
                    "task.share",
                    &json!({"id":id,"actor":actor,"shared_with":names}),
                )?;
                crate::waits::unshared(store, id, actor, names)?;
            }
            match args.get("status").and_then(Value::as_str) {
                Some(status) => {
                    if !["open", "running", "blocked", "done", "cancelled"].contains(&status) {
                        bail!("invalid task status");
                    }
                    tx.execute(
                        "UPDATE tasks SET status=?2 WHERE id=?1",
                        params![id, status],
                    )?;
                    if matches!(status, "done" | "cancelled") {
                        crate::mail::close_task(store, id, status)?;
                        crate::waits::task_ended(store, id, status)?;
                        crate::scheduling::settle(store, id)?;
                        closed = Some(status.to_owned());
                    }
                    // A status the owner records supersedes a schedule
                    // fault: the owner has taken the task over, and a stale
                    // delivery fault must not linger as a fleet blocker.
                    crate::scheduling::supersede_fault(store, id, status)?;
                }
                None => {
                    // A malformed status is a refused update, not one half
                    // applied: `required` keeps the old message for agents.
                    if args.get("status").is_some() {
                        required(&args, "status")?;
                    }
                    ensure!(
                        shared.is_some(),
                        "task.update needs a status or shared_with"
                    );
                }
            }
        }
    }
    // The status a task.update set, so the fleet's activity can show work
    // finishing on any machine.
    let mut record = json!({"id":id,"actor":actor});
    if command == "task.update"
        && let Some(status) = args.get("status").and_then(Value::as_str)
    {
        record["status"] = json!(status);
    }
    store.audit(command, &record)?;
    tx.commit()?;
    // A closed task ends its parked crews, and the heavy jobs they wait on.
    // This runs after the close commits, since it stops processes. When the
    // owner closed it, their cancelled notices are not mailed back to it.
    if let Some(status) = closed {
        let quiet = store.task(id)?["owner"].as_str() == Some(actor);
        for crew in crate::crew::parked_crews(store, id)? {
            crate::jobs::end_parked(store, &crew, &format!("task {id} is {status}"), quiet)?;
        }
    }
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
    let tx = store.write_transaction()?;
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
        "INSERT INTO events(id,task_id,issue_key,kind,payload,time_ms) VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            id,
            task_id,
            issue,
            kind,
            payload.to_string(),
            Store::now_ms()
        ],
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
    // Presence (including null) opts into the complete typed correlation pair.
    // Validate before any mutation; unsupported commands cannot ignore it.
    let correlated = args.get("request_id").is_some() || args.get("expected_version").is_some();
    let request_id = if correlated {
        ensure!(
            matches!(command, "approval.grant" | "approval.deny"),
            "request correlation applies only to grant or deny"
        );
        let id = required(&args, "request_id")?;
        identifier(id)?;
        required(&args, "expected_version")?;
        if command == "approval.grant" {
            required(&args, "id")?;
        }
        Some(id)
    } else {
        None
    };
    let task = required(&args, "task_id")?;
    let artifact = required(&args, "artifact")?;
    let environment = required(&args, "environment")?;
    // A privileged approval is written only by a correlated grant of a
    // privileged request, and used up only by `privileged.run`.
    ensure!(
        (request_id.is_some() && command != "approval.consume")
            || !crate::privileged::reserved(artifact, environment),
        "artifacts starting privileged: and environments starting host: are privileged approvals: only a correlated grant writes one and only privileged.run uses it"
    );
    // Serialize the authority read with the grant or consumption: a charter
    // revoked while this command waits for the write lock must not authorize it.
    let tx = store.write_transaction()?;
    // The explicit approval supplies authority for this one artifact/environment.
    // It must not require widening every task's standing charter first.
    let charter = store.active_charter(task)?;
    let charter_hash = hash(&charter);
    // Authority and request binding are rechecked under the same write lock.
    let request = request_id
        .map(|id| crate::needs::approval_request(store, id, &args, &charter_hash))
        .transpose()?;
    if command == "approval.deny" {
        let request = request.context("approval.deny requires request_id")?;
        let result = crate::mail::approval_answer(store, &request, "denied", &args)?;
        tx.commit()?;
        return Ok(result);
    }
    let mut approver = None;
    let expires = if command == "approval.grant" {
        let expires = match args.get("expires_ms") {
            Some(value) => Some(value.as_i64().context("expires_ms must be an integer")?),
            None if environment.starts_with("upgrade:")
                || environment.starts_with("upgrade-local:") =>
            {
                Some(Store::now_ms() + 86_400_000)
            }
            None => None,
        };
        ensure!(
            expires.is_none_or(|time| time > Store::now_ms()),
            "approval expiry must be in the future"
        );
        expires
    } else {
        None
    };
    let id = if command == "approval.grant" {
        // The approver is recorded and must be someone other than the task's
        // owner, who would otherwise approve its own production change.
        let named = required(&args, "approver")?;
        identifier(named)?;
        if store.task(task)?["owner"] == named {
            bail!("the task owner cannot approve its own production change");
        }
        approver = Some(named);
        let id = args
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        identifier(&id)?;
        // Replaying a grant can never reset an already consumed approval.
        let existing = tx.query_row("SELECT task_id,artifact,environment,charter_hash,consumed,approver FROM approvals WHERE id=?1",[&id],|r| {
            Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,i64>(4)?,r.get::<_,Option<String>>(5)?))
        }).optional()?;
        if let Some((t, a, e, h, c, by)) = existing {
            if t != task
                || a != artifact
                || e != environment
                || h != charter_hash
                || c != 0
                || by.as_deref() != approver
            {
                bail!("approval id cannot be reused");
            }
        } else {
            tx.execute("INSERT INTO approvals(id,task_id,artifact,environment,charter_hash,approver,expires_ms) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![id,task,artifact,environment,charter_hash,approver,expires])?;
        }
        id
    } else {
        let id = required(&args, "id")?.to_owned();
        let count = tx.execute("UPDATE approvals SET consumed=1 WHERE id=?1 AND task_id=?2 AND artifact=?3 AND environment=?4 AND charter_hash=?5 AND consumed=0 AND (expires_ms IS NULL OR expires_ms>?6)",params![id,task,artifact,environment,charter_hash,Store::now_ms()])?;
        if count != 1 {
            bail!("no matching unused approval");
        }
        id
    };
    let actual_expiry: Option<i64> =
        tx.query_row("SELECT expires_ms FROM approvals WHERE id=?1", [&id], |r| {
            r.get(0)
        })?;
    store.audit(
        command,
        &json!({"id":id,"task_id":task,"artifact":artifact,"environment":environment,"approver":approver,"expires_ms":actual_expiry}),
    )?;
    if let Some(request) = request {
        ensure!(
            command == "approval.grant",
            "request_id applies only to grant or deny"
        );
        crate::mail::approval_answer(store, &request, "approved", &args)?;
    }
    tx.commit()?;
    let mut result = json!({"id":id,"task_id":task,"artifact":artifact,"environment":environment,"consumed":command=="approval.consume","expires_ms":actual_expiry});
    if let Some(approver) = approver {
        result["approver"] = json!(approver);
    }
    Ok(result)
}

/// Operator-run retention. Removes event facts that no longer back a pending
/// notification, audit rows, finished, released jobs with their logs, and
/// finished wakes, all older than the cutoff. Pending notifications, live
/// jobs and the wakes they or `unwoken` still need are kept.
/// Pruned event and job ids can be reused afterwards.
fn history_prune(store: &Store, args: &Value) -> Result<Value> {
    let cutoff = match (args.get("before_ms"), args.get("older_than_days")) {
        (Some(v), None) => v
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("before_ms must be an integer"))?,
        (None, Some(v)) => {
            let days = v
                .as_f64()
                .filter(|d| d.is_finite() && *d > 0.0)
                .ok_or_else(|| anyhow::anyhow!("older_than_days must be a positive number"))?;
            Store::now_ms().saturating_sub((days * 86_400_000.0) as i64)
        }
        _ => bail!("pass exactly one of before_ms or older_than_days"),
    };
    let dry_run = match args.get("dry_run") {
        None => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| anyhow::anyhow!("dry_run must be a boolean"))?,
    };
    let tx = store.write_transaction()?;
    let stale_events = "(time_ms IS NULL OR time_ms < ?1) AND seq NOT IN (SELECT seq FROM pending)";
    let events: i64 = tx.query_row(
        &format!("SELECT COUNT(*) FROM events WHERE {stale_events}"),
        [cutoff],
        |r| r.get(0),
    )?;
    let audit: i64 = tx.query_row(
        "SELECT COUNT(*) FROM audit WHERE time_ms < ?1",
        [cutoff],
        |r| r.get(0),
    )?;
    if !dry_run {
        tx.execute(
            &format!("DELETE FROM events WHERE {stale_events}"),
            [cutoff],
        )?;
        tx.execute("DELETE FROM audit WHERE time_ms < ?1", [cutoff])?;
    }
    let (jobs, jobs_still_reserved, directories) = crate::jobs::prune(store, cutoff, dry_run)?;
    let mail = crate::mail::prune(store, cutoff, dry_run)?;
    let notes = crate::context::prune(store, cutoff, dry_run)?;
    let wakes = crate::activation::prune(store, cutoff, dry_run)?;
    let mut result = json!({"before_ms":cutoff,"dry_run":dry_run,"events":events,"audit":audit,
        "jobs":jobs,"jobs_still_reserved":jobs_still_reserved,"mail":mail,"notes":notes,"wakes":wakes});
    if dry_run {
        return Ok(result);
    }
    store.audit("history.prune", &result)?;
    tx.commit()?;
    // Job logs go only after their records are durably gone.
    let mut failed = 0;
    for directory in directories {
        let removed =
            reject_symlinks(&directory).and_then(|()| match fs::remove_dir_all(&directory) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                _ => Ok(()),
            });
        if removed.is_err() {
            failed += 1;
        }
    }
    result["log_directories_failed"] = json!(failed);
    Ok(result)
}

/// The argument keys of each command here; see `crate::Accepted`.
pub(crate) const ACCEPTED: crate::Accepted = &[
    ("init", &["resource_home", "ship", "preset"]),
    (
        "charter.put",
        &[
            "id",
            "objective",
            "repo",
            "actions",
            "forge",
            "max_delegation_depth",
            "job_timeout_secs",
            "min_free_bytes",
            "max_heavy_jobs",
            "crew_max_parks",
            "crew_max_elapsed_secs",
            "provider_plugin",
            "active",
            "production_requires_approval",
            // A charter.show record put back: counted again on each put.
            "revision",
        ],
    ),
    ("charter.show", &["id"]),
    ("charter.revoke", &["id"]),
    (
        "task.create",
        &["id", "owner", "title", "parent", "charter_id"],
    ),
    ("task.show", &["id"]),
    ("task.update", &["id", "actor", "status", "shared_with"]),
    ("task.handoff", &["id", "actor", "target"]),
    ("task.accept", &["id", "actor"]),
    ("task.rank", &["id", "rank"]),
    ("task.backlog", &["ship"]),
    ("task.list", &["owner", "status", "limit"]),
    (
        "event.emit",
        &["id", "task_id", "issue_key", "kind", "payload"],
    ),
    ("event.pending", &["task_id", "after_seq", "limit"]),
    ("event.history", &["task_id", "after_seq", "limit"]),
    ("event.ack", &["task_id", "issue_key", "through_seq"]),
    ("message.send", &["task_id", "from", "to", "body", "id"]),
    ("message.inbox", &["actor", "after_seq", "limit"]),
    (
        "approval.grant",
        &[
            "id",
            "task_id",
            "artifact",
            "environment",
            "approver",
            "request_id",
            "expected_version",
            "expires_ms",
        ],
    ),
    (
        "approval.consume",
        &["id", "task_id", "artifact", "environment"],
    ),
    (
        "approval.deny",
        &[
            "id",
            "task_id",
            "artifact",
            "environment",
            "approver",
            "request_id",
            "expected_version",
        ],
    ),
    (
        "delivery.check",
        &[
            "task_id",
            "current_head",
            "tested_head",
            "checks_passed",
            "deploys_production",
            "environment",
        ],
    ),
    (
        "history.prune",
        &["before_ms", "older_than_days", "dry_run"],
    ),
    ("audit.list", &["after_seq", "limit"]),
    ("status", &[]),
];

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
            // The ship this folder is, for commands and hooks run inside it.
            if let Some(ship) = args.get("ship") {
                let ship = ship
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("ship must be a name"))?;
                identifier(ship)?;
                data["ship"] = json!(ship);
            }
            if let Some(preset) = args.get("preset") {
                if preset != "liaison" {
                    bail!("unknown ship preset");
                }
                data["preset"] = preset.clone();
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
            let tx = store.write_transaction()?;
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
        "task.rank" => task_rank(store, &args),
        "task.backlog" => {
            let ship = required(&args, "ship")?;
            Ok(json!({"ship": ship, "backlog": crate::context::backlog(store, ship)?}))
        }
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
            let tx = store.write_transaction()?;
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
        "approval.grant" | "approval.consume" | "approval.deny" => approval(store, command, args),
        "delivery.check" => {
            let task = required(&args, "task_id")?;
            // Read authority and match/consume an approval under the same write
            // lock as charter revocation and task changes.
            let tx = store.write_transaction()?;
            let charter = store.authorize(task, "merge_green")?;
            let head = required(&args, "current_head")?;
            if args.get("tested_head").and_then(Value::as_str) != Some(head)
                || args.get("checks_passed") != Some(&json!(true))
            {
                bail!("passing checks must apply to the current head");
            }
            let mut result = json!({"allowed":true,"task_id":task,"head":head,"evidence":"caller-supplied; forge adapter must verify live checks"});
            if args.get("deploys_production") == Some(&json!(true)) {
                let env = required(&args, "environment")?;
                ensure!(
                    !crate::privileged::reserved(head, env),
                    "a privileged approval is used only by privileged.run"
                );
                // The approval authorises one deployment: allowing it uses the
                // approval up in the same transaction.
                let approval: Option<String> = tx.query_row("SELECT id FROM approvals WHERE task_id=?1 AND artifact=?2 AND environment=?3 AND charter_hash=?4 AND consumed=0 AND (expires_ms IS NULL OR expires_ms>?5) ORDER BY id LIMIT 1",params![task,head,env,hash(&charter),Store::now_ms()],|r|r.get(0)).optional()?;
                let Some(approval) = approval else {
                    bail!("production requires approval of this exact artifact and environment");
                };
                tx.execute(
                    "UPDATE approvals SET consumed=1 WHERE id=?1 AND consumed=0",
                    [&approval],
                )?;
                store.audit(
                    "approval.consumed_by_delivery",
                    &json!({"id":approval,"task_id":task,"artifact":head,"environment":env}),
                )?;
                result["approval_id"] = json!(approval);
            }
            tx.commit()?;
            Ok(result)
        }
        "history.prune" => history_prune(store, &args),
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
            let mut status = json!({"version":env!("CARGO_PKG_VERSION"),"home":store.home(),"open_tasks":tasks,"event_facts":facts,"pending_notifications":pending});
            for (key, value) in crate::mail::summary(store)?.as_object().unwrap() {
                status[key] = value.clone();
            }
            Ok(status)
        }
        _ => bail!("unknown command {command}"),
    }
}
