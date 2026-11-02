use super::*;
use std::collections::BTreeMap;

#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod tests;

const SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
// A caller may request one row per page or change page size on continuation.
// Bound distinct offsets, not just pages at the initial requested size. Leave
// 32 receipt slots for standalone keys and other retained outcomes; existing
// receipts can still backpressure admission, but one snapshot cannot fill it.
pub(super) const PAGE_ROWS: usize = RECEIPTS as usize - 32;
type Order = (String, String, i64, String);
fn order(o: &Observation) -> Order {
    (
        o.origin.core.clone(),
        o.origin.mailbox.clone(),
        o.order,
        o.origin.id.clone(),
    )
}

// Keep the lowest ordered prefix of projection AND displaced pending versions.
// A cutoff is sticky: a small later row must not leapfrog a row that did not
// fit. The map never retains more than the snapshot budget plus one input row.
struct Prefix {
    rows: BTreeMap<Order, (Observation, usize, selection::runtime::Cost)>,
    bytes: usize,
    row_limit: usize,
    cutoff: Option<Order>,
    tail: bool,
    capacity: Option<selection::runtime::Cost>,
    used: selection::runtime::Cost,
    pressure: bool,
}
impl Prefix {
    fn retain(&mut self, o: Observation) -> Result<()> {
        self.retain_cost(o, selection::runtime::Cost::default())
    }
    fn retain_cost(&mut self, o: Observation, cost: selection::runtime::Cost) -> Result<()> {
        let key = order(&o);
        if self
            .cutoff
            .as_ref()
            .is_some_and(|end| if self.tail { &key <= end } else { &key >= end })
            || self.rows.contains_key(&key)
        {
            return Ok(());
        }
        // Include separators and the empty Snapshot envelope in admission.
        // Include frozen delivery-class overhead in the retained-body bound.
        let size = serde_json::to_vec(&o)?.len() + 65;
        let previous_cutoff = self.cutoff.clone();
        self.bytes += size;
        self.used.add(cost);
        self.rows.insert(key.clone(), (o, size, cost));
        let mut discarded = Vec::new();
        while self.rows.len() > self.row_limit || self.bytes > SNAPSHOT_BYTES {
            let removed = if self.tail {
                self.rows.pop_first()
            } else {
                self.rows.pop_last()
            };
            let (removed_key, row) = removed.context("oversized_item")?;
            self.bytes -= row.1;
            self.used.remove(row.2);
            self.cutoff = Some(removed_key.clone());
            discarded.push((removed_key, row));
        }
        if self
            .capacity
            .is_some_and(|capacity| !capacity.fits(self.used))
        {
            let (_, size, cost) = self.rows.remove(&key).context("invalid admission plan")?;
            self.bytes -= size;
            self.used.remove(cost);
            for (key, row) in discarded {
                self.bytes += row.1;
                self.used.add(row.2);
                self.rows.insert(key, row);
            }
            self.cutoff = previous_cutoff;
            self.pressure = true;
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    secret: String,
    rows: Vec<Observation>,
    #[serde(default)]
    older: bool,
    // Allows an older-window request to replay its child after the drained
    // parent has been reclaimed. Only the two bounded snapshots are searched.
    #[serde(default)]
    predecessor: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    classes: Vec<replay::DeliveryClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replay_selection: Option<Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    history: bool,
}
fn continuation(id: &str, secret: &str, offset: usize) -> String {
    format!(
        "{id}:{offset}:{}",
        crate::core::hash(&json!([secret, offset]))
    )
}

// A terminal thread token advances to an independent older window. Validate
// the parent exactly like a page token; never interpret a caller-supplied key
// as a boundary. A retained child also proves the request for lost-output replay.
#[allow(clippy::type_complexity)]
fn thread_start(
    conn: &Connection,
    sub: &Subscription,
    binding: &str,
    token: Option<String>,
    now: i64,
) -> Result<(Option<String>, Option<Order>, Option<String>)> {
    let Some(token) = token else {
        return Ok((None, None, None));
    };
    ensure!(token.len() <= 256, "stale_or_unknown");
    let parts: Vec<_> = token.split(':').collect();
    ensure!(parts.len() == 3, "stale_or_unknown");
    let offset: usize = parts[1].parse().context("stale_or_unknown")?;
    // Ordinary page continuations need only their own snapshot, not every
    // potentially 16-MiB body with this binding.
    let body: Option<String> = conn.query_row("SELECT body FROM desk_subscription_snapshot WHERE id=?1 AND subscription=?2 AND operation=?3 AND expires>?4", params![parts[0],sub.id,binding,now], |r| r.get(0)).optional()?;
    let parent = body
        .map(|body| serde_json::from_str::<Snapshot>(&body))
        .transpose()?;
    if let Some(parent) = &parent {
        ensure!(
            offset <= parent.rows.len() && token == continuation(parts[0], &parent.secret, offset),
            "stale_or_unknown"
        );
        if offset != parent.rows.len() || !parent.older {
            return Ok((Some(token), None, None));
        }
    }
    // Search for a child only at a window transition (or after reclamation).
    // Its exact saved predecessor proves a lost-output replay even without
    // the parent; no caller-supplied order is ever trusted.
    let child: Option<(String, String)> = conn.query_row("SELECT id,body FROM desk_subscription_snapshot WHERE subscription=?1 AND operation=?2 AND expires>?3 AND json_extract(body,'$.predecessor')=?4", params![sub.id,binding,now,token], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((id, body)) = child {
        let saved: Snapshot = serde_json::from_str(&body)?;
        return Ok((Some(continuation(&id, &saved.secret, 0)), None, None));
    }
    let parent = parent.context("stale_or_unknown")?;
    let before = order(parent.rows.first().context("stale_or_unknown")?);
    Ok((None, Some(before), Some(token)))
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
    let active = replay::binding(conn, sub)?.is_some();
    let binding = serde_json::to_string(&json!([
        operation,
        anchor,
        m.manifest_revision,
        sub.incarnation,
        Owner::of(m),
        visibility(c)
    ]))?;
    let (token, before, predecessor) = if operation == "thread" {
        thread_start(conn, sub, &binding, token, now)?
    } else {
        (token, None, None)
    };
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
        // Always reclaim a drained parent when advancing windows: its child
        // saves the predecessor for lost-output replay, while unused parent
        // page-key reservations would otherwise consume the receipt budget.
        // Reclaim other drained snapshots only when admission needs a slot,
        // preferring pull/show over live threads. Unread snapshots backpressure.
        let parent_id = predecessor.as_ref().and_then(|s| s.split(':').next());
        let mut stmt = conn.prepare("SELECT id,body FROM desk_subscription_snapshot WHERE subscription=?1 ORDER BY (id=?2) DESC,(json_extract(operation,'$[0]')='thread'),rowid")?;
        let prior = stmt
            .query_map(params![sub.id, parent_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut count = prior.len();
        for (id, body) in prior {
            if count < 2 && parent_id != Some(id.as_str()) {
                continue;
            }
            let saved: Snapshot = serde_json::from_str(&body)?;
            let mut drained = true;
            for o in &saved.rows {
                let acked = if saved.history {
                    history::acknowledged(conn, sub, o)?
                } else if saved.replay_selection.is_some() {
                    selection::runtime::acknowledged(conn, sub, o)?
                } else {
                    acknowledged(conn, sub, o)?
                };
                if !acked {
                    drained = false;
                    break;
                }
            }
            if drained || (saved.history && parent_id == Some(id.as_str())) {
                conn.execute("DELETE FROM desk_subscription_snapshot WHERE id=?1", [id])?;
                count -= 1;
            }
        }
        ensure!(count < 2, "backpressure");
        // Prefix history refs are replay/ack handles, not sparse custody.
        // Keep them only while a retained snapshot can reissue them, bounding
        // historical handles to at most two snapshots (192 observations).
        if !(active && operation == "thread") {
            conn.execute("DELETE FROM desk_subscription_ref AS r WHERE r.subscription=?1 AND EXISTS(SELECT 1 FROM desk_subscription_progress p WHERE p.subscription=r.subscription AND p.core=r.core AND p.mailbox=r.mailbox AND p.through_order>=r.source_order) AND NOT EXISTS(SELECT 1 FROM desk_subscription_exclusion e WHERE e.subscription=r.subscription AND e.incarnation=r.incarnation AND e.core=r.core AND e.mailbox=r.mailbox AND r.source_order BETWEEN e.first_order AND e.last_order) AND NOT EXISTS(SELECT 1 FROM desk_subscription_snapshot s,json_each(s.body,'$.rows') o WHERE s.subscription=r.subscription AND json_extract(o.value,'$.origin.core')=r.core AND json_extract(o.value,'$.origin.mailbox')=r.mailbox AND json_extract(o.value,'$.origin.id')=r.mail AND json_extract(o.value,'$.order')=r.source_order)", [&sub.id])?;
        }
        history::cleanup(conn, sub)?;
        let history = active && operation == "thread";
        let automatic = operation == "pull" || (active && operation == "show");
        let read = if active || automatic {
            Some(selection::runtime::Read::prepare(conn, sub, m, c)?)
        } else {
            None
        };
        let mut diagnostics = replay::Diagnostics::default();
        if let Some(read) = &read
            && !history
        {
            diagnostics.work_pending = !read.revisit(128, 50)?;
            read.refresh_closures()?;
        }
        let anchor_row = anchor
            .as_ref()
            .map(|a| intake::observation(conn, m, a))
            .transpose()?
            .flatten()
            .filter(|o| visible(m, c, o));
        ensure!(anchor.is_none() || anchor_row.is_some(), "stale_or_unknown");
        let secret = uuid();
        let envelope = serde_json::to_vec(&Snapshot {
            secret: secret.clone(),
            rows: Vec::new(),
            older: false,
            predecessor: predecessor.clone(),
            classes: Vec::new(),
            replay_selection: None,
            history,
        })?
        .len();
        let mut prefix = Prefix {
            rows: BTreeMap::new(),
            // Any servable frozen diagnostic is <= the whole page budget.
            bytes: envelope + if read.is_some() { BYTES } else { 0 },
            row_limit: PAGE_ROWS,
            cutoff: None,
            tail: operation == "thread",
            capacity: if automatic {
                read.as_ref().map(|r| r.capacity()).transpose()?
            } else {
                None
            },
            used: selection::runtime::Cost::default(),
            pressure: false,
        };
        let mut item_coverage = coverage.clone();
        item_coverage["snapshot"] = json!("0".repeat(36));
        item_coverage["observed_at"] = json!(now);
        item_coverage["complete"] = json!(false);
        item_coverage["retry"] = json!({"action":"thread_continue","after_ms":0});
        let mut select = |o: Observation| -> Result<()> {
            if !visible(m, c, &o) {
                return Ok(());
            }
            let eligible = match operation {
                "show" => {
                    if let Some(read) = &read {
                        match read.show(&o)? {
                            selection::runtime::Choice::Eligible(_) => {}
                            selection::runtime::Choice::Indeterminate => {
                                anyhow::bail!("backpressure")
                            }
                            _ => anyhow::bail!("stale_or_unknown"),
                        }
                    }
                    anchor.as_ref() == Some(&o.origin)
                }
                "thread" => {
                    let a = anchor_row.as_ref().context("stale_or_unknown")?;
                    let root = a.content["correlation"].as_str().unwrap_or(&a.origin.id);
                    o.origin.core == a.origin.core
                        && o.origin.mailbox == a.origin.mailbox
                        && (o.origin.id == root || o.content["correlation"].as_str() == Some(root))
                }
                _ => {
                    if let Some(read) = &read {
                        match read.choice(&o)? {
                            selection::runtime::Choice::Eligible(_) => true,
                            selection::runtime::Choice::Excluded(reasons) => {
                                read.exclude(&o)?;
                                diagnostics.exclude(reasons);
                                false
                            }
                            selection::runtime::Choice::Indeterminate => {
                                diagnostics.work_pending = true;
                                false
                            }
                            _ => false,
                        }
                    } else {
                        !acknowledged(conn, sub, &o)?
                    }
                }
            };
            if eligible && before.as_ref().is_none_or(|end| order(&o) < *end) {
                // A single oversized observation is not a window boundary.
                // Check before discarding rows to meet either snapshot cap.
                if operation == "thread" {
                    ensure!(
                        serde_json::to_vec(&o)?.len() + 1 + envelope <= SNAPSHOT_BYTES,
                        "oversized_item"
                    );
                    // A row that cannot fit even a one-item page must not
                    // strand the reader between windows. Use the same item
                    // envelope and reserve as serving below, with worst-case
                    // window coverage, before issuing any snapshot or ref.
                    let item = json!({"ref":"0".repeat(36),"origin":o.origin,"mail":o.content,"state":o.state,"delivery_class":"history"});
                    let result = json!({"items":[item],"continuation":null,"coverage":item_coverage,"ack_key":"0".repeat(64)});
                    ensure!(
                        serde_json::to_vec(&result)?.len() + 2048 <= BYTES,
                        "oversized_item"
                    );
                }
                if automatic {
                    let cost = read.as_ref().context("missing automatic read")?.cost(&o)?;
                    prefix.retain_cost(o, cost)?;
                } else {
                    prefix.retain(o)?;
                }
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
                let acked = if read.is_some() {
                    selection::runtime::acknowledged(conn, sub, &o)?
                } else {
                    acknowledged(conn, sub, &o)?
                };
                if visible(m, c, &o) && !acked {
                    let cost = read.as_ref().context("missing automatic read")?.cost(&o)?;
                    prefix.retain_cost(o, cost)?;
                }
            }
        }
        if operation == "pull" {
            let mut stmt = conn.prepare("SELECT observation FROM desk_subscription_exception WHERE subscription=?1 AND incarnation=?2")?;
            for row in
                stmt.query_map(params![sub.id, sub.incarnation], |r| r.get::<_, String>(0))?
            {
                let o: Observation = serde_json::from_str(&row?)?;
                if visible(m, c, &o) && !selection::runtime::acknowledged(conn, sub, &o)? {
                    let cost = read.as_ref().context("missing automatic read")?.cost(&o)?;
                    prefix.retain_cost(o, cost)?;
                }
            }
        }
        let limited = prefix.cutoff.is_some();
        diagnostics.work_pending |= prefix.pressure;
        ensure!(operation != "show" || !prefix.pressure, "backpressure");
        ensure!(!limited || !prefix.rows.is_empty(), "oversized_item");
        ensure!(
            !limited || operation == "pull" || operation == "thread",
            "backpressure"
        );
        let rows: Vec<Observation> = prefix.rows.into_values().map(|(o, _, _)| o).collect();
        let mut classes = Vec::new();
        if history {
            classes.resize(rows.len(), replay::DeliveryClass::History);
        } else if let Some(read) = &read {
            for o in &rows {
                // Durable snapshot retention is an offer, even if output is lost.
                let class = if selection::runtime::acknowledged(conn, sub, o)? {
                    let selection::runtime::Choice::Eligible(class) = read.show(o)? else {
                        anyhow::bail!("stale_or_unknown")
                    };
                    class
                } else {
                    read.offer(o)?
                };
                classes.push(class);
            }
        }
        diagnostics.work_pending |= limited;
        let replay_selection = read
            .as_ref()
            .map(|read| diagnostics.freeze(conn, sub, m, c, read, coverage))
            .transpose()?;
        let snapshot = Snapshot {
            secret,
            rows,
            older: limited && operation == "thread",
            predecessor,
            classes,
            replay_selection,
            history,
        };
        let body = serde_json::to_string(&snapshot)?;
        ensure!(body.len() <= SNAPSHOT_BYTES, "backpressure");
        let id = uuid();
        let key = uuid();
        if history {
            history::reserve(conn, sub, &key, &id, now)?;
        } else {
            ack::reserve_named(conn, sub, &key, Some(&id), now)?;
        }
        let mut frozen = coverage.clone();
        frozen["snapshot"] = json!(id);
        frozen["observed_at"] = json!(now);
        if limited {
            frozen["complete"] = json!(false);
            frozen["retry"] = json!({"action":if operation == "thread" { "thread_continue" } else { "pull_after_ack" },"after_ms":0});
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
        if snapshot.history {
            history::reserve(conn, sub, &key, &id, now)?;
        } else {
            ack::reserve_named(conn, sub, &key, Some(&id), now)?;
        }
        key
    };
    let mut result =
        json!({"items":[],"continuation":null,"coverage":frozen_coverage,"ack_key":page_key});
    let mut carried = Vec::new();
    let mut end = offset;
    if let Some(replay) = &snapshot.replay_selection {
        result["replay_selection"] = replay.clone();
    }
    for (position, o) in snapshot.rows.iter().enumerate().skip(offset).take(items) {
        ensure!(visible(m, c, o), "stale_or_unknown");
        let acked = if snapshot.replay_selection.is_some() || replay::activated(conn, sub)? {
            selection::runtime::acknowledged(conn, sub, o)?
        } else {
            acknowledged(conn, sub, o)?
        };
        if operation == "pull" && acked {
            end += 1;
            continue;
        }
        let table = if snapshot.history {
            "desk_subscription_history_ref"
        } else {
            "desk_subscription_ref"
        };
        let existing: Option<String> = conn.query_row(&format!("SELECT ref FROM {table} WHERE subscription=?1 AND incarnation=?2 AND core=?3 AND mailbox=?4 AND mail=?5 AND source_order=?6"),params![sub.id,sub.incarnation,o.origin.core,o.origin.mailbox,o.origin.id,o.order],|r|r.get(0)).optional()?;
        let reference = existing.clone().unwrap_or_else(uuid);
        let mut item = json!({"ref":reference,"origin":o.origin,"mail":o.content,"state":o.state});
        if let Some(class) = snapshot.classes.get(position) {
            item["delivery_class"] = json!(class);
            item["state"] = json!(match o.state.as_deref() {
                Some("open") => "open",
                Some("closed") => "closed",
                _ => "unknown",
            });
        }
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
            // Old frozen pages keep their exact wire shape, but a newly issued
            // automatic ref still needs a split reservation if policy has been
            // activated since that snapshot was frozen.
            if !snapshot.history
                && snapshot.classes.is_empty()
                && !acked
                && replay::activated(conn, sub)?
            {
                selection::carry(conn, sub, o, replay::DeliveryClass::Unknown, "offer")?;
                if operation != "thread"
                    && o.state.as_deref() == Some("open")
                    && o.content["to"] == "captain"
                    && matches!(o.content["kind"].as_str(), Some("request" | "decision"))
                {
                    selection::offer_pin(conn, sub, o, replay::DeliveryClass::Unknown)?;
                }
            }
            let already_acked = !snapshot.history && acked;
            if !snapshot.history && !already_acked {
                let window: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM desk_subscription_ref r WHERE subscription=?1 AND (NOT EXISTS(SELECT 1 FROM desk_subscription_progress p WHERE p.subscription=r.subscription AND p.core=r.core AND p.mailbox=r.mailbox AND p.through_order>=r.source_order) OR (r.acked=0 AND EXISTS(SELECT 1 FROM desk_subscription_exclusion e WHERE e.subscription=r.subscription AND e.incarnation=r.incarnation AND e.core=r.core AND e.mailbox=r.mailbox AND r.source_order BETWEEN e.first_order AND e.last_order)))",
                    [&sub.id],
                    |r| r.get(0),
                )?;
                ensure!(window < WINDOW, "backpressure");
            }
            conn.execute(&format!("INSERT INTO {table}(ref,subscription,incarnation,core,mailbox,mail,source_order,observation,acked) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)"),params![reference,sub.id,sub.incarnation,o.origin.core,o.origin.mailbox,o.origin.id,o.order,serde_json::to_string(o)?,already_acked])?;
        }
        end += 1;
    }
    result["items"] = json!(carried);
    result["continuation"] = if end < snapshot.rows.len() || snapshot.older {
        json!(continuation(&id, &snapshot.secret, end))
    } else {
        Value::Null
    };
    // Snapshots are retained even after the last page for lost-output retries.
    // They expire explicitly; no pending served observation is evicted with it.
    Ok(result)
}
