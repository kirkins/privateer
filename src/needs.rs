//! Bounded, read-only local captain attention, derived from typed runtime facts.
use crate::store::{Caller, Store, required};
use anyhow::{Context, Result, ensure};
use rusqlite::params;
use serde_json::{Value, json};
use std::cell::Cell;
use std::collections::HashSet;
use std::time::{Duration, Instant};

// Per-observation only: never shorten ordinary commands' writer wait policy.
// SQLite callbacks bound VM work cooperatively, not OS/filesystem I/O.
pub(crate) struct ObservationBudget<'a> {
    conn: &'a rusqlite::Connection,
    previous_busy_ms: u64,
    deadline: Instant,
    checkpoints: Cell<u32>,
}

impl<'a> ObservationBudget<'a> {
    pub(crate) fn new(conn: &'a rusqlite::Connection) -> Result<Self> {
        let previous_busy_ms = conn.query_row("PRAGMA busy_timeout", [], |r| r.get(0))?;
        conn.busy_timeout(Duration::from_millis(25))?;
        let deadline = Instant::now() + Duration::from_millis(250);
        let mut callbacks = 0_u32;
        conn.progress_handler(
            1000,
            Some(move || {
                callbacks += 1;
                callbacks >= 2000 || Instant::now() >= deadline
            }),
        );
        Ok(Self {
            conn,
            previous_busy_ms,
            deadline,
            checkpoints: Cell::new(0),
        })
    }

    // VM progress callbacks may never fire across repeated small statements.
    // Bound Rust/statement work independently, across the whole observation.
    pub(crate) fn check(&self) -> Result<()> {
        let checkpoints = self.checkpoints.get() + 1;
        self.checkpoints.set(checkpoints);
        ensure!(
            checkpoints <= 4096 && Instant::now() < self.deadline,
            "runtime observation work/deadline exhausted"
        );
        Ok(())
    }
}

impl Drop for ObservationBudget<'_> {
    fn drop(&mut self) {
        self.conn.progress_handler(0, None::<fn() -> bool>);
        // Restore even on unavailable/error/early return. Store owns this
        // connection; it has no persistent progress handler to replace.
        let _ = self
            .conn
            .busy_timeout(Duration::from_millis(self.previous_busy_ms));
    }
}

/// Validate a correlated grant/denial inside core's authority transaction.
pub(crate) fn approval_request(
    store: &Store,
    id: &str,
    args: &Value,
    charter_hash: &str,
) -> Result<Value> {
    store.caller().require_operator("approval response")?;
    ensure!(
        required(args, "approver")? == "captain",
        "correlated approvals require the captain"
    );
    let request = crate::mail::find(store, id)?.context("unknown approval request")?;
    ensure!(
        request["to"] == "captain"
            && request["kind"] == "request"
            && crate::mail::is_approval_kind(request["request_kind"].as_str()),
        "not a typed captain approval request"
    );
    // Each kind keeps to its own artifacts: a production approval never
    // writes a privileged one, and a privileged one is checked by its hash.
    let privileged = request["request_kind"] == crate::privileged::REQUEST_KIND;
    if privileged {
        crate::privileged::check_answerable(store, &request)?;
    } else {
        ensure!(
            !crate::privileged::reserved(
                request["payload"]["artifact"].as_str().unwrap_or(""),
                request["payload"]["environment"].as_str().unwrap_or("")
            ),
            "a production approval cannot name a privileged artifact or host environment"
        );
    }
    ensure!(
        request["task_id"] == args["task_id"]
            && request["payload"]["artifact"] == args["artifact"]
            && request["payload"]["environment"] == args["environment"],
        "approval request binding changed"
    );
    ensure!(
        required(args, "expected_version")? == charter_hash
            && request["payload"]["charter_hash"] == charter_hash,
        "stale approval authority version"
    );
    ensure!(
        matches!(
            request["state"].as_str(),
            Some("open" | "approved" | "denied")
        ),
        "approval request is closed"
    );
    Ok(request)
}

pub fn query(store: &Store, args: &Value) -> Result<Value> {
    ensure!(
        *store.caller() == Caller::Operator,
        "captain needs are operator-only"
    );
    ensure!(
        args.get("address").is_none_or(|v| v == "captain"),
        "runtime.needs is the captain query"
    );
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(40)
        .clamp(1, 100) as usize;
    let after = args.get("after").and_then(Value::as_str).unwrap_or("");
    ensure!(after.len() <= 512, "cursor exceeds 512 bytes");
    let budget = ObservationBudget::new(store.connection())?;
    let observed = observe(store, limit, after, &budget);
    match observed {
        Ok(value) if budget.check().is_ok() => Ok(value),
        _ => Ok(
            json!({"schema_version":1,"available":false,"origin":null,"address":"captain",
            "observed_ms":Store::now_ms(),"items":[],"next_after":null,
            "unknown":["runtime_observation"],
            "reason":"Runtime observation unavailable: locked, over budget, oversized, or invalid source. Not an all-clear."}),
        ),
    }
}

fn queued_workspaces(
    store: &Store,
    before_ms: i64,
    after: &str,
    limit: usize,
) -> Result<Vec<(String, String, i64)>> {
    let path = crate::jobs::ledger_dir(store)?.join("resources.sqlite");
    if !path.is_file() {
        return Ok(vec![]);
    }
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_millis(25))?;
    let has_queue: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='queue')",
        [],
        |r| r.get(0),
    )?;
    if !has_queue {
        return Ok(vec![]);
    }
    let home = store.home().to_string_lossy().to_string();
    let oversized: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM queue WHERE json_extract(record,'$.kind')='workspace'
            AND json_extract(record,'$.home')=?1 AND length(CAST(record AS BLOB))>65536)",
        [&home],
        |r| r.get(0),
    )?;
    ensure!(
        !oversized,
        "workspace queue observation source exceeds byte bound"
    );
    let mut statement = conn.prepare(
        "SELECT json_extract(record,'$.job_id'),json_extract(record,'$.created_ms') FROM queue
         WHERE json_extract(record,'$.kind')='workspace'
           AND json_extract(record,'$.home')=?1
           AND json_extract(record,'$.created_ms')<=?2
           AND 'workspace:'||json_extract(record,'$.job_id')>?3
         ORDER BY json_extract(record,'$.job_id') LIMIT ?4",
    )?;
    let rows = statement.query_map(params![home, before_ms, after, limit as u64], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })?;
    rows.map(|row| {
        let (id, created_ms) = row?;
        Ok(("workspace".to_owned(), id, created_ms))
    })
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(Into::into)
}

fn observe(
    store: &Store,
    limit: usize,
    after: &str,
    budget: &ObservationBudget<'_>,
) -> Result<Value> {
    budget.check()?;
    // The budget already covers metadata/schema and all item reads.
    // A deferred read transaction gives a consistent view without a writer lock.
    let tx = rusqlite::Transaction::new_unchecked(
        store.connection(),
        rusqlite::TransactionBehavior::Deferred,
    )?;
    let now = Store::now_ms();
    let origin = tx.query_row::<String, _, _>(
        "SELECT value FROM metadata WHERE key='runtime_origin' AND length(CAST(value AS BLOB))<=160
         AND EXISTS(SELECT 1 FROM metadata WHERE key='schema' AND value='1')
         AND (SELECT COUNT(*) FROM pragma_table_info('mail') WHERE name IN ('ended_by','ended_ms','end_reason'))=3",
        [], |r| r.get(0));
    let Ok(origin) = origin else {
        return Ok(
            json!({"schema_version":1,"available":false,"origin":null,"address":"captain",
            "observed_ms":now,"items":[],"next_after":null,"unknown":["runtime_schema"],
            "reason":"Runtime needs schema unavailable; initialize or upgrade through the normal operator workflow."}),
        );
    };
    // Bound JSON/text materialization too: a single SQLite JSON opcode or Rust
    // parse cannot be interrupted by a VM progress callback. Refuse, never skip
    // oversized sources and accidentally return a quiet partial observation.
    budget.check()?;
    let oversized: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM jobs WHERE length(CAST(record AS BLOB))>65536
            OR length(CAST(id AS BLOB))>160)
         OR EXISTS(SELECT 1 FROM mail WHERE length(CAST(payload AS BLOB))>65536
            OR length(CAST(body AS BLOB))>65536 OR length(CAST(id AS BLOB))>160)
         OR EXISTS(SELECT 1 FROM charters WHERE length(CAST(data AS BLOB))>65536
            OR length(CAST(id AS BLOB))>160)
         OR EXISTS(SELECT 1 FROM tasks WHERE length(CAST(title AS BLOB))>65536
            OR length(CAST(id AS BLOB))>160 OR length(CAST(owner AS BLOB))>160
            OR length(CAST(parent AS BLOB))>160 OR length(CAST(charter_id AS BLOB))>160
            OR length(CAST(handoff_to AS BLOB))>160 OR length(CAST(status AS BLOB))>160)",
        [],
        |r| r.get(0),
    )?;
    ensure!(!oversized, "runtime observation source exceeds byte bound");
    budget.check()?;
    let schedule_union = if crate::scheduling::available(store)? {
        let oversized: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM task_schedule WHERE length(CAST(record AS BLOB))>65536)",
            [],
            |r| r.get(0),
        )?;
        ensure!(
            !oversized,
            "scheduling observation source exceeds byte bound"
        );
        format!(
            "UNION ALL SELECT 'schedule',s.task_id FROM task_schedule s JOIN tasks t ON t.id=s.task_id WHERE json_extract(s.record,'$.state')='fault' AND t.status NOT IN ('done','cancelled')
             UNION ALL SELECT 'remote_wait',s.task_id FROM task_schedule s JOIN tasks t ON t.id=s.task_id
               LEFT JOIN ships sh ON sh.name=json_extract(s.record,'$.owner') WHERE {}",
            crate::scheduling::remote_wait_stale_where("?4")
        )
    } else {
        // The query binds the time as ?4 either way.
        "UNION ALL SELECT 'schedule','' WHERE ?4 IS NULL".to_owned()
    };
    let mut statement = tx.prepare(&format!(
        "SELECT source,id FROM (
          SELECT 'mail' source,m.id FROM mail m LEFT JOIN tasks t ON t.id=m.task_id
            WHERE m.recipient='captain' AND m.state='open' AND m.kind IN ('request','decision')
              AND (m.task_id IS NULL OR t.status NOT IN ('done','cancelled'))
          UNION ALL SELECT 'job',j.id FROM jobs j LEFT JOIN tasks t ON t.id=json_extract(j.record,'$.task_id')
            WHERE json_extract(j.record,'$.status')='orphaned' OR
              (json_extract(j.record,'$.status')='queued' AND json_extract(j.record,'$.created_ms')<=?1
               AND t.status NOT IN ('done','cancelled'))
          {schedule_union}
        ) WHERE source||':'||id>?2 ORDER BY source||':'||id LIMIT ?3"))?;
    let mut rows = statement
        .query_map(params![now - 60_000, after, limit + 1, now], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .map(|row| row.map(|(source, id)| (source, id, None)))
        .collect::<rusqlite::Result<Vec<_>>>()?;
    budget.check()?;
    rows.extend(
        queued_workspaces(store, now - 60_000, after, limit + 1)?
            .into_iter()
            .map(|(source, id, created)| (source, id, Some(created))),
    );
    budget.check()?;
    rows.sort_by_key(|(source, id, _)| format!("{source}:{id}"));
    rows.truncate(limit + 1);
    let more = rows.len() > limit;
    let mut items = vec![];
    let mut cursor = None;
    // Read once, and only when a job is waiting: reservations no live process
    // holds, which keep queued work waiting until the captain recovers them.
    let mut stale: Option<Vec<Value>> = None;
    for (source, id, created_ms) in rows.into_iter().take(limit) {
        budget.check()?;
        let key = format!("{source}:{id}");
        cursor = Some(key.clone());
        let mut item = json!({"id":format!("{origin}:{key}"),"origin":origin,"source":source,"source_id":id,"observed_ms":now});
        match source.as_str() {
            "mail" => {
                let message = crate::mail::find(store, &id)?.context("request vanished")?;
                let approval = crate::mail::is_approval_kind(message["request_kind"].as_str());
                let privileged = message["request_kind"] == crate::privileged::REQUEST_KIND;
                let payload = &message["payload"];
                if privileged && !crate::privileged::local(store, &message)? {
                    // A copy carried from the machine that staged it: shown,
                    // but its program, sudo and terminal are there.
                    if crate::privileged::verify(&message).is_err() {
                        continue;
                    }
                    item["artifact"] = payload["artifact"].clone();
                    item["environment"] = payload["environment"].clone();
                    item["version"] = json!(crate::core::hash(&json!([id, "elsewhere"])));
                    item["actions"] = json!([]);
                    item["privileged"] = crate::privileged::summary(&message);
                    item["privileged"]["held_elsewhere"] = json!(true);
                } else if approval {
                    let Some(task) = message["task_id"].as_str() else {
                        continue;
                    };
                    if !["artifact", "environment"].iter().all(|key| {
                        payload[*key]
                            .as_str()
                            .is_some_and(|s| !s.is_empty() && s.len() <= 1024)
                    }) {
                        continue;
                    }
                    // Share production's effective authority/hash, but bound
                    // every ancestry read even if no VM callback fires. Invalid
                    // source is unavailable; only explicit revocation/lifecycle
                    // refusal (or a replaced hash below) is a stale omission.
                    let mut ancestors = HashSet::new();
                    let charter = store.observe_active_charter(task, |id| {
                        budget.check()?;
                        ensure!(ancestors.insert(id.to_owned()), "cyclic task ancestry");
                        Ok(())
                    })?;
                    budget.check()?;
                    let Some(charter) = charter else {
                        continue;
                    };
                    if payload["charter_hash"] != crate::core::hash(&charter) {
                        continue;
                    }
                    if privileged {
                        // Its binding, machine and expiry, as a grant checks them.
                        if crate::privileged::check_answerable(store, &message).is_err() {
                            continue;
                        }
                        item["privileged"] = crate::privileged::summary(&message);
                    } else if crate::privileged::reserved(
                        payload["artifact"].as_str().unwrap_or(""),
                        payload["environment"].as_str().unwrap_or(""),
                    ) {
                        continue;
                    }
                    item["artifact"] = payload["artifact"].clone();
                    item["environment"] = payload["environment"].clone();
                    item["version"] = payload["charter_hash"].clone();
                    item["actions"] = json!(["approval.grant", "approval.deny"]);
                } else {
                    item["version"] = json!(crate::core::hash(&json!([id, "open"])));
                    item["actions"] = json!(["mail.send"]);
                }
                item["kind"] = json!(if approval { "approval" } else { "decision" });
                item["ship"] = message["from"].clone();
                item["task_id"] = message["task_id"].clone();
                item["question"] = json!(
                    message["body"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(2000)
                        .collect::<String>()
                );
                item["choices"] = json!(
                    payload["choices"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .filter(|s| !s.is_empty() && s.len() <= 200)
                        .take(20)
                        .collect::<Vec<_>>()
                );
                // Only a decision message carries a typed card.
                if message["kind"] == "decision" && !payload["decision"].is_null() {
                    item["decision"] = payload["decision"].clone();
                }
                item["correlation"] = json!(id);
                item["created_ms"] = message["created_ms"].clone();
                // The mail it is, to sort by and to join with captain.waiting.
                item["mail_id"] = json!(id);
                item["mail_kind"] = message["kind"].clone();
                item["urgency"] = message["urgency"].clone();
                item["deadline_ms"] = message["deadline_ms"].clone();
                item["asked_ms"] = message["created_ms"].clone();
                item["needs_hands"] = json!(payload.get("needs_hands").is_some());
            }
            "job" => {
                let raw: String =
                    tx.query_row("SELECT record FROM jobs WHERE id=?1", [&id], |r| r.get(0))?;
                let job: Value = serde_json::from_str(&raw)?;
                item["kind"] = json!("blocker");
                let blocking = if job["status"] == "queued" {
                    if stale.is_none() {
                        stale = Some(crate::jobs::stale_reservations(store)?);
                    }
                    stale.clone().unwrap_or_default()
                } else {
                    vec![]
                };
                item["evidence"] = json!(match job["status"].as_str() {
                    Some("queued")
                        if job["reason"]
                            .as_str()
                            .unwrap_or("")
                            .contains("disk available") =>
                        "disk_floor_wait",
                    Some("queued") if !blocking.is_empty() => "stuck_heavy_capacity",
                    Some("queued") => "capacity_wait",
                    _ => "orphaned_job",
                });
                item["task_id"] = job["task_id"].clone();
                item["ship"] =
                    store.task(job["task_id"].as_str().context("job task missing")?)?["owner"]
                        .clone();
                item["status"] = job["status"].clone();
                item["incarnation"] = job["incarnation"].clone();
                item["summary"] = json!(
                    job["reason"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(2000)
                        .collect::<String>()
                );
                item["created_ms"] = job["created_ms"].clone();
                if job["status"] == "queued" {
                    item["waited_ms"] =
                        json!(now.saturating_sub(job["created_ms"].as_i64().unwrap_or(now)));
                }
                if !blocking.is_empty() {
                    item["blocking"] = json!(blocking.iter().take(5).collect::<Vec<_>>());
                }
                let blocking_keys: Vec<&Value> = blocking.iter().map(|r| &r["key"]).collect();
                item["version"] = json!(crate::core::hash(&json!([
                    job["incarnation"],
                    job["status"],
                    job["reason"],
                    blocking_keys
                ])));
                item["actions"] = json!([]);
            }
            "schedule" => {
                let state = crate::scheduling::fault_item(store, &id)?;
                item["kind"] = json!("blocker");
                item["evidence"] = json!("scheduling_fault");
                for key in [
                    "ship",
                    "task_id",
                    "status",
                    "summary",
                    "created_ms",
                    "version",
                ] {
                    item[key] = state[key].clone();
                }
                item["actions"] = json!([]);
            }
            "remote_wait" => {
                // A wait on another ship's task that no report has ended:
                // shown, never resolved, so the captain can judge the far end.
                let state = crate::scheduling::remote_wait_item(store, &id, now)?;
                item["kind"] = json!("blocker");
                item["evidence"] = json!("stale_remote_wait");
                for key in [
                    "ship",
                    "task_id",
                    "status",
                    "summary",
                    "created_ms",
                    "waited_ms",
                    "far",
                    "link",
                    "version",
                ] {
                    item[key] = state[key].clone();
                }
                item["actions"] = json!([]);
            }
            "workspace" => {
                let task = store.task(&id)?;
                item["kind"] = json!("blocker");
                item["evidence"] = json!("workspace_disk_wait");
                item["task_id"] = json!(id);
                item["ship"] = task["owner"].clone();
                item["status"] = json!("queued");
                item["created_ms"] = json!(created_ms.unwrap_or(now));
                item["waited_ms"] = json!(now.saturating_sub(created_ms.unwrap_or(now)));
                item["summary"] =
                    json!("Workspace allocation is waiting in the host disk capacity queue.");
                item["version"] = json!(crate::core::hash(&json!([id, "workspace_disk_wait"])));
                item["actions"] = json!([]);
            }
            _ => unreachable!(),
        }
        items.push(item);
    }
    budget.check()?;
    drop(statement);
    tx.commit()?;
    Ok(
        json!({"schema_version":1,"available":true,"origin":origin,"address":"captain","observed_ms":now,
        "items":items,"next_after":if more { cursor } else { None },
        "unknown":["live_process_liveness","wake_backoff","hourly_wake_budget","dispatcher_liveness","herdr_reachability","attended_mail_pause","ci","provider_health","remote_homes","legacy_unvalidated_approval_requests","task_captain_actionability" ]}),
    )
}
