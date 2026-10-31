use super::*;
use std::collections::BTreeMap;

#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod tests;

const SNAPSHOT_ROWS: usize = 10_000;
const SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
// A caller may request one row per page or change page size on continuation.
// Bound distinct offsets, not just pages at the initial requested size. Leave
// 32 receipt slots for standalone keys and other retained outcomes; existing
// receipts can still backpressure admission, but one snapshot cannot fill it.
pub(super) const PULL_ROWS: usize = RECEIPTS as usize - 32;
type Order = (String, String, i64, String);

// Keep the lowest ordered prefix of projection AND displaced pending versions.
// A cutoff is sticky: a small later row must not leapfrog a row that did not
// fit. The map never retains more than the snapshot budget plus one input row.
struct Prefix {
    rows: BTreeMap<Order, (Observation, usize)>,
    bytes: usize,
    row_limit: usize,
    cutoff: Option<Order>,
}
impl Prefix {
    fn retain(&mut self, o: Observation) -> Result<()> {
        let key = (
            o.origin.core.clone(),
            o.origin.mailbox.clone(),
            o.order,
            o.origin.id.clone(),
        );
        if self.cutoff.as_ref().is_some_and(|end| &key >= end) || self.rows.contains_key(&key) {
            return Ok(());
        }
        // Include separators and the empty Snapshot envelope in admission.
        let size = serde_json::to_vec(&o)?.len() + 1;
        self.bytes += size;
        self.rows.insert(key, (o, size));
        while self.rows.len() > self.row_limit || self.bytes > SNAPSHOT_BYTES {
            let (key, (_, size)) = self.rows.pop_last().context("oversized_item")?;
            self.bytes -= size;
            self.cutoff = Some(key);
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    secret: String,
    rows: Vec<Observation>,
}
fn continuation(id: &str, secret: &str, offset: usize) -> String {
    format!(
        "{id}:{offset}:{}",
        crate::core::hash(&json!([secret, offset]))
    )
}

/// Durable start tokens recover even a first page whose entire output was
/// lost. The host never supplies a cursor/high-water mark on resume.
pub(super) fn resumable(conn: &Connection, sub: &Subscription, now: i64) -> Result<Vec<Value>> {
    let mut stmt = conn.prepare("SELECT id,operation,body FROM desk_subscription_snapshot WHERE subscription=?1 AND expires>?2 ORDER BY id")?;
    let mut out = Vec::new();
    for row in stmt.query_map(params![sub.id, now], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (id, binding, body) = row?;
        let binding: Value = serde_json::from_str(&binding)?;
        let snapshot: Snapshot = serde_json::from_str(&body)?;
        out.push(json!({"operation":binding[0],"anchor":binding[1],"continuation":continuation(&id,&snapshot.secret,0)}));
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn page(
    conn: &Connection,
    sub: &Subscription,
    m: &Manifest,
    c: &DelegatedContext,
    operation: &str,
    anchor: Option<MailIdentity>,
    token: Option<String>,
    items: usize,
    now: i64,
    coverage: &Value,
) -> Result<Value> {
    ensure!((1..=ITEMS).contains(&items), "invalid_bounds");
    let binding = serde_json::to_string(&json!([
        operation,
        anchor,
        m.manifest_revision,
        sub.incarnation
    ]))?;
    let (id, snapshot, offset, frozen_coverage, key) = if let Some(token) = token {
        ensure!(token.len() <= 256, "stale_or_unknown");
        let parts: Vec<_> = token.split(':').collect();
        ensure!(parts.len() == 3, "stale_or_unknown");
        let offset: usize = parts[1].parse().context("stale_or_unknown")?;
        let stored: Option<(String,String,String,String)> = conn.query_row("SELECT operation,body,coverage,ack_key FROM desk_subscription_snapshot WHERE id=?1 AND subscription=?2 AND expires>?3",params![parts[0],sub.id,now],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        let (bound, body, coverage, key) = stored.context("stale_or_unknown")?;
        ensure!(bound == binding, "stale_or_unknown");
        let snapshot: Snapshot = serde_json::from_str(&body)?;
        ensure!(
            offset <= snapshot.rows.len()
                && token == continuation(parts[0], &snapshot.secret, offset),
            "stale_or_unknown"
        );
        (
            parts[0].to_owned(),
            snapshot,
            offset,
            serde_json::from_str::<Value>(&coverage)?,
            key,
        )
    } else {
        // Only acknowledged/drained snapshots may be reclaimed early. Unread
        // snapshots exert backpressure; expiry never removes pending refs.
        let mut stmt =
            conn.prepare("SELECT id,body FROM desk_subscription_snapshot WHERE subscription=?1")?;
        let prior = stmt
            .query_map([&sub.id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, body) in prior {
            let saved: Snapshot = serde_json::from_str(&body)?;
            let mut drained = true;
            for o in &saved.rows {
                if !acknowledged(conn, sub, o)? {
                    drained = false;
                    break;
                }
            }
            if drained {
                conn.execute("DELETE FROM desk_subscription_snapshot WHERE id=?1", [id])?;
            }
        }
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM desk_subscription_snapshot WHERE subscription=?1",
            [&sub.id],
            |r| r.get(0),
        )?;
        ensure!(count < 2, "backpressure");
        let anchor_row = anchor
            .as_ref()
            .map(|a| intake::observation(conn, m, a))
            .transpose()?
            .flatten()
            .filter(|o| visible(m, c, o));
        ensure!(anchor.is_none() || anchor_row.is_some(), "stale_or_unknown");
        let secret = uuid();
        let mut prefix = Prefix {
            rows: BTreeMap::new(),
            bytes: serde_json::to_vec(&Snapshot {
                secret: secret.clone(),
                rows: Vec::new(),
            })?
            .len(),
            row_limit: if operation == "pull" {
                PULL_ROWS
            } else {
                SNAPSHOT_ROWS
            },
            cutoff: None,
        };
        let mut select = |o: Observation| -> Result<()> {
            if !visible(m, c, &o) {
                return Ok(());
            }
            let eligible = match operation {
                "show" => anchor.as_ref() == Some(&o.origin),
                "thread" => {
                    let a = anchor_row.as_ref().context("stale_or_unknown")?;
                    let root = a.content["correlation"].as_str().unwrap_or(&a.origin.id);
                    o.origin.core == a.origin.core
                        && o.origin.mailbox == a.origin.mailbox
                        && (o.origin.id == root || o.content["correlation"].as_str() == Some(root))
                }
                _ => !acknowledged(conn, sub, &o)?,
            };
            if eligible {
                prefix.retain(o)?;
            }
            Ok(())
        };
        if operation == "show" {
            select(anchor_row.clone().context("stale_or_unknown")?)?;
        } else {
            intake::visit_observations(
                conn,
                m,
                (operation == "pull").then_some(sub.id.as_str()),
                c.ships(),
                anchor_row.as_ref().map(|a| {
                    (
                        &a.origin,
                        a.content["correlation"].as_str().unwrap_or(&a.origin.id),
                    )
                }),
                select,
            )?;
        }
        if operation == "pull" {
            // Pending deliveries survive projection updates and snapshot expiry.
            let mut stmt = conn.prepare(
                "SELECT observation FROM desk_subscription_ref WHERE subscription=?1 AND acked=0",
            )?;
            for row in stmt.query_map([&sub.id], |r| r.get::<_, String>(0))? {
                let o: Observation = serde_json::from_str(&row?)?;
                if visible(m, c, &o) {
                    prefix.retain(o)?;
                }
            }
        }
        let limited = prefix.cutoff.is_some();
        // Pull can advance through a proved prefix on subsequent snapshots.
        // Show/thread have no such cursor: retain their all-or-refuse contract.
        ensure!(!limited || operation == "pull", "backpressure");
        ensure!(!limited || !prefix.rows.is_empty(), "oversized_item");
        let snapshot = Snapshot {
            secret,
            rows: prefix.rows.into_values().map(|(o, _)| o).collect(),
        };
        let body = serde_json::to_string(&snapshot)?;
        ensure!(body.len() <= SNAPSHOT_BYTES, "backpressure");
        let id = uuid();
        let key = uuid();
        ack::reserve_named(conn, sub, &key, Some(&id), now)?;
        let mut frozen = coverage.clone();
        frozen["snapshot"] = json!(id);
        frozen["observed_at"] = json!(now);
        if limited {
            frozen["complete"] = json!(false);
            frozen["retry"] = json!({"action":"pull_after_ack","after_ms":0});
        }
        conn.execute("INSERT INTO desk_subscription_snapshot(id,subscription,operation,expires,body,coverage,ack_key) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![id,sub.id,binding,now.saturating_add(SNAPSHOT_TTL),body,serde_json::to_string(&frozen)?,key])?;
        (id, snapshot, 0, frozen, key)
    };
    // Page-specific server-issued idempotency keys let each page ack once;
    // replaying a page repeats its key. Resume can issue a fresh batch key.
    let page_key = if offset == 0 {
        key
    } else {
        let key = crate::core::hash(&json!([snapshot.secret, "ack", offset]));
        ack::reserve_named(conn, sub, &key, Some(&id), now)?;
        key
    };
    let mut result =
        json!({"items":[],"continuation":null,"coverage":frozen_coverage,"ack_key":page_key});
    let mut carried = Vec::new();
    let mut end = offset;
    for o in snapshot.rows.iter().skip(offset).take(items) {
        ensure!(visible(m, c, o), "stale_or_unknown");
        if operation == "pull" && acknowledged(conn, sub, o)? {
            end += 1;
            continue;
        }
        let existing: Option<String> = conn.query_row("SELECT ref FROM desk_subscription_ref WHERE subscription=?1 AND incarnation=?2 AND core=?3 AND mailbox=?4 AND mail=?5 AND source_order=?6",params![sub.id,sub.incarnation,o.origin.core,o.origin.mailbox,o.origin.id,o.order],|r|r.get(0)).optional()?;
        let reference = existing.clone().unwrap_or_else(uuid);
        let item = json!({"ref":reference,"origin":o.origin,"mail":o.content,"state":o.state});
        carried.push(item);
        // Reserve enough for the entire outer result and continuation. The
        // final engine check additionally measures actual serialized bytes.
        result["items"] = json!(carried);
        if serde_json::to_vec(&result)?.len() + 2048 > BYTES {
            carried.pop();
            ensure!(!carried.is_empty(), "oversized_item");
            break;
        }
        if existing.is_none() {
            let window: i64 = conn.query_row(
                "SELECT COUNT(*) FROM desk_subscription_ref WHERE subscription=?1",
                [&sub.id],
                |r| r.get(0),
            )?;
            ensure!(window < WINDOW, "backpressure");
            conn.execute("INSERT INTO desk_subscription_ref(ref,subscription,incarnation,core,mailbox,mail,source_order,observation) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![reference,sub.id,sub.incarnation,o.origin.core,o.origin.mailbox,o.origin.id,o.order,serde_json::to_string(o)?])?;
        }
        end += 1;
    }
    result["items"] = json!(carried);
    result["continuation"] = if end < snapshot.rows.len() {
        json!(continuation(&id, &snapshot.secret, end))
    } else {
        Value::Null
    };
    // Snapshots are retained even after the last page for lost-output retries.
    // They expire explicitly; no pending served observation is evicted with it.
    Ok(result)
}
