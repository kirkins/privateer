//! Bounded, read-only local captain attention, derived from typed runtime facts.
use crate::store::{Caller, Store, required};
use anyhow::{Context, Result, ensure};
use rusqlite::params;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

// Per-observation only: never shorten ordinary commands' writer wait policy.
// SQLite callbacks bound VM work cooperatively, not OS/filesystem I/O.
struct ObservationBudget<'a> {
    conn: &'a rusqlite::Connection,
    previous_busy_ms: u64,
    deadline: Instant,
}

impl<'a> ObservationBudget<'a> {
    fn new(conn: &'a rusqlite::Connection) -> Result<Self> {
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
        })
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
            && request["request_kind"] == "production_approval",
        "not a typed captain approval request"
    );
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
    let observed = observe(store, limit, after);
    match observed {
        Ok(value) if Instant::now() < budget.deadline => Ok(value),
        _ => Ok(
            json!({"schema_version":1,"available":false,"origin":null,"address":"captain",
            "observed_ms":Store::now_ms(),"items":[],"next_after":null,
            "unknown":["runtime_observation"],
            "reason":"Runtime observation unavailable: locked, over budget, oversized, or invalid source. Not an all-clear."}),
        ),
    }
}

fn observe(store: &Store, limit: usize, after: &str) -> Result<Value> {
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
    let oversized: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM jobs WHERE length(CAST(record AS BLOB))>65536
            OR length(CAST(id AS BLOB))>160)
         OR EXISTS(SELECT 1 FROM mail WHERE length(CAST(payload AS BLOB))>65536
            OR length(CAST(body AS BLOB))>65536 OR length(CAST(id AS BLOB))>160)
         OR EXISTS(SELECT 1 FROM charters WHERE length(CAST(data AS BLOB))>65536)
         OR EXISTS(SELECT 1 FROM tasks WHERE length(CAST(title AS BLOB))>65536
            OR length(CAST(id AS BLOB))>160 OR length(CAST(owner AS BLOB))>160)",
        [],
        |r| r.get(0),
    )?;
    ensure!(!oversized, "runtime observation source exceeds byte bound");
    let mut statement = tx.prepare(
        "SELECT source,id FROM (
          SELECT 'mail' source,m.id FROM mail m LEFT JOIN tasks t ON t.id=m.task_id
            WHERE m.recipient='captain' AND m.state='open' AND m.kind IN ('request','decision')
              AND (m.task_id IS NULL OR t.status NOT IN ('done','cancelled'))
          UNION ALL SELECT 'job',j.id FROM jobs j LEFT JOIN tasks t ON t.id=json_extract(j.record,'$.task_id')
            WHERE json_extract(j.record,'$.status')='orphaned' OR
              (json_extract(j.record,'$.status')='queued' AND json_extract(j.record,'$.created_ms')<=?1
               AND t.status NOT IN ('done','cancelled'))
        ) WHERE source||':'||id>?2 ORDER BY source||':'||id LIMIT ?3")?;
    let rows = statement
        .query_map(params![now - 60_000, after, limit + 1], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let more = rows.len() > limit;
    let mut items = vec![];
    let mut cursor = None;
    for (source, id) in rows.into_iter().take(limit) {
        let key = format!("{source}:{id}");
        cursor = Some(key.clone());
        let mut item = json!({"id":format!("{origin}:{key}"),"origin":origin,"source":source,"source_id":id,"observed_ms":now});
        match source.as_str() {
            "mail" => {
                let message = crate::mail::find(store, &id)?.context("request vanished")?;
                let approval = message["request_kind"] == "production_approval";
                let payload = &message["payload"];
                if approval {
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
                    // Revoked or replaced authority cannot present an actionable grant.
                    let charter = match store.active_charter(task) {
                        Ok(charter) => charter,
                        Err(error) => {
                            // Revoked authority is not actionable, but SQL
                            // interruption/lock failure is unavailable, not quiet.
                            if error.downcast_ref::<rusqlite::Error>().is_some() {
                                return Err(error);
                            }
                            continue;
                        }
                    };
                    if payload["charter_hash"] != crate::core::hash(&charter) {
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
                item["correlation"] = json!(id);
                item["created_ms"] = message["created_ms"].clone();
            }
            "job" => {
                let raw: String =
                    tx.query_row("SELECT record FROM jobs WHERE id=?1", [&id], |r| r.get(0))?;
                let job: Value = serde_json::from_str(&raw)?;
                item["kind"] = json!("blocker");
                item["evidence"] = json!(if job["status"] == "queued" {
                    "capacity_wait"
                } else {
                    "orphaned_job"
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
                item["version"] = json!(crate::core::hash(&json!([
                    job["incarnation"],
                    job["status"],
                    job["reason"]
                ])));
                item["actions"] = json!([]);
            }
            _ => unreachable!(),
        }
        items.push(item);
    }
    drop(statement);
    tx.commit()?;
    Ok(
        json!({"schema_version":1,"available":true,"origin":origin,"address":"captain","observed_ms":now,
        "items":items,"next_after":if more { cursor } else { None },
        "unknown":["live_process_liveness","wake_backoff","hourly_wake_budget","dispatcher_liveness","herdr_reachability","attended_mail_pause","ci","provider_health","remote_homes","legacy_unvalidated_approval_requests","task_captain_actionability" ]}),
    )
}
