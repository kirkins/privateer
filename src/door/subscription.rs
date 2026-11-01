//! Fenced LOCAL desk subscriptions. Internal core API only: no adapter command,
//! transport, placement, migration, presence or request-lifecycle authority.
//! The caller admits a fresh local grant for every call; this engine rechecks
//! that exact authority inside its writer transaction. PR 3b must supply its
//! own host admission/release boundary rather than pretending remote grants
//! live in this registry.
#![cfg_attr(not(test), allow(dead_code))]
use super::{coverage, scoped::DelegatedContext};
use crate::machines::{desk_intake::subscription as intake, desk_placement::Manifest};
use anyhow::{Context, Result, ensure};
use coverage::MailIdentity;
use intake::Observation;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

mod ack;
mod replay;
mod schema;
// Inactive 4b-2A storage primitives; no engine operation calls this module.
mod selection;
mod snapshot;
#[cfg(test)]
mod tests;
pub(crate) use schema::init;
pub(super) use schema::install;

pub(super) const SLOTS_PER_DOMAIN: i64 = 16;

const ITEMS: usize = 50;
const BYTES: usize = 192 * 1024;
const WINDOW: i64 = 256;
const RECEIPTS: i64 = 128;
const SNAPSHOT_TTL: i64 = 60 * 60 * 1000;
const RECEIPT_TTL: i64 = 24 * 60 * 60 * 1000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Owner {
    pub domain: String,
    pub core: String,
    pub generation: u64,
    pub incarnation: String,
}
impl Owner {
    pub(crate) fn of(m: &Manifest) -> Self {
        Self {
            domain: m.domain.clone(),
            core: m.owner_core.clone(),
            generation: m.generation,
            incarnation: m.incarnation.clone(),
        }
    }
}

pub(crate) enum Operation {
    Open,
    Resume {
        subscription: String,
    },
    Pull {
        subscription: String,
        continuation: Option<String>,
        items: usize,
    },
    Show {
        subscription: String,
        origin: MailIdentity,
    },
    Thread {
        subscription: String,
        origin: MailIdentity,
        continuation: Option<String>,
        items: usize,
    },
    Ack {
        subscription: String,
        key: String,
        refs: Vec<String>,
    },
}
impl Operation {
    fn subscription(&self) -> Option<&str> {
        match self {
            Self::Open => None,
            Self::Resume { subscription }
            | Self::Pull { subscription, .. }
            | Self::Show { subscription, .. }
            | Self::Thread { subscription, .. }
            | Self::Ack { subscription, .. } => Some(subscription),
        }
    }
}

/// Every failure has a non-disclosing refusal envelope. No raw SQL, paths,
/// source names, hidden counts, or owner routing hints cross this boundary.
pub(crate) fn execute(
    conn: &Connection,
    c: &DelegatedContext,
    expected: &Owner,
    op: Operation,
) -> Value {
    let run = || -> Result<Value> {
        let (_fence, m) = crate::machines::desk_placement::subscription_owner(conn)?;
        let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        // Recheck after acquiring the registry writer, not just before waiting.
        let (_check, current) = crate::machines::desk_placement::subscription_owner(&tx)?;
        ensure!(
            Owner::of(&m) == *expected && Owner::of(&current) == *expected,
            "stale_generation"
        );
        super::clients::subscription_admission(&tx, c)?;
        let value = run_admitted(&tx, c, &m, op, crate::store::Store::now_ms())?;
        tx.commit()?;
        Ok(value)
    };
    match run() {
        Ok(v) => v,
        Err(e) => {
            let code = match e.to_string().as_str() {
                "backpressure" => "backpressure",
                "stale_or_unknown" => "stale_or_unknown",
                "invalid_ack" => "invalid_ack",
                "oversized_item" => "oversized_item",
                _ => "unavailable",
            };
            let coverage = coverage::unknown_domain();
            json!({"ok":false,"error":code,"coverage":coverage})
        }
    }
}

struct Subscription {
    id: String,
    incarnation: String,
}
fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn visibility(c: &DelegatedContext) -> String {
    crate::core::hash(&json!([c.capabilities(), c.visibility_revision()]))
}
fn reader(m: &Manifest, c: &DelegatedContext) -> String {
    // Resource includes registered door; grant UUID, not application or token.
    json!([
        m.owner_core,
        c.resource().strip_prefix("door:"),
        c.resource(),
        c.provenance()["grant"]
    ])
    .to_string()
}

/// Slot authority is terminal only when its grant is revoked, expired or gone.
/// Pausing and visibility changes do not release custody. Status and admission
/// share this snapshot so reclaimable rows never appear as live slot usage.
pub(super) struct Slot {
    pub id: String,
    pub door: Option<String>,
    pub grant: Option<String>,
    pub live: bool,
}
pub(super) fn slots(conn: &Connection, domain: &str, now: i64) -> Result<Vec<Slot>> {
    let mut stmt = conn.prepare("SELECT s.id,json_extract(s.reader,'$[1]'),json_extract(s.reader,'$[3]'),EXISTS(SELECT 1 FROM door_clients c WHERE c.id=json_extract(s.reader,'$[3]') AND c.revoked_ms IS NULL AND (c.expires_ms IS NULL OR c.expires_ms>?2)) FROM desk_subscription s WHERE s.domain=?1")?;
    Ok(stmt
        .query_map(params![domain, now], |r| {
            Ok(Slot {
                id: r.get(0)?,
                door: r.get(1)?,
                grant: r.get(2)?,
                live: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn run_admitted(
    conn: &Connection,
    c: &DelegatedContext,
    m: &Manifest,
    op: Operation,
    now: i64,
) -> Result<Value> {
    ensure!(c.has("mail:read"), "denied");
    let reader = reader(m, c);
    let owner = serde_json::to_string(&Owner::of(m))?;
    let visibility = visibility(c);
    // Re-consent creates new grants. Retire only terminal authority, never a
    // paused grant, inside the same writer transaction as new admission. Once
    // retired, even an expiry extension starts a new reader from zero.
    for slot in slots(conn, &m.domain, now)? {
        if !slot.live {
            retire(conn, &slot.id)?;
        }
    }
    let mut stored: Option<(String,String,String,String)> = conn.query_row("SELECT id,incarnation,owner,visibility FROM desk_subscription WHERE domain=?1 AND reader=?2",params![m.domain,reader],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    if let Some((id, _, old_owner, _)) = &stored
        && old_owner != &owner
        && matches!(op, Operation::Open)
    {
        // This does not install ownership or migrate a cursor: execute has
        // already fenced the new certified local tuple. Replay from zero.
        retire(conn, id)?;
        stored = None;
    }
    let sub = if let Some((id, incarnation, old_owner, old_visibility)) = stored {
        ensure!(old_owner == owner, "stale_or_unknown");
        if let Some(requested) = op.subscription() {
            ensure!(id == requested, "stale_or_unknown");
        }
        if old_visibility != visibility {
            // Conservative replay on both restriction and expansion. Never let
            // an earlier restricted prefix suppress newly authorized history.
            for table in [
                "desk_subscription_snapshot",
                "desk_subscription_ref",
                "desk_subscription_progress",
                "desk_subscription_receipt",
            ] {
                conn.execute(&format!("DELETE FROM {table} WHERE subscription=?1"), [&id])?;
            }
            selection::clear(conn, &id)?;
            let incarnation = uuid();
            conn.execute(
                "UPDATE desk_subscription SET incarnation=?2,visibility=?3 WHERE id=?1",
                params![id, incarnation, visibility],
            )?;
            Subscription { id, incarnation }
        } else {
            Subscription { id, incarnation }
        }
    } else {
        ensure!(matches!(op, Operation::Open), "stale_or_unknown");
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM desk_subscription WHERE domain=?1",
            [&m.domain],
            |r| r.get(0),
        )?;
        ensure!(count < SLOTS_PER_DOMAIN, "backpressure");
        let sub = Subscription {
            id: uuid(),
            incarnation: uuid(),
        };
        conn.execute("INSERT INTO desk_subscription(id,domain,reader,owner,incarnation,visibility) VALUES(?1,?2,?3,?4,?5,?6)",params![sub.id,m.domain,reader,owner,sub.incarnation,visibility])?;
        replay::capture(conn, &sub, m, c)?;
        sub
    };
    conn.execute(
        "DELETE FROM desk_subscription_snapshot WHERE subscription=?1 AND expires<=?2",
        params![sub.id, now],
    )?;
    conn.execute(
        "DELETE FROM desk_subscription_receipt WHERE subscription=?1 AND expires<=?2",
        params![sub.id, now],
    )?;
    let vector = intake::source_vector(conn, m);
    let coverage = coverage::subscription_coverage(conn, m, c, &vector)?;
    let mut value = match op {
        Operation::Open | Operation::Resume { .. } => {
            let key = ack::reserve_key(conn, &sub, now)?;
            json!({"ack_key":key,"pending_snapshots":snapshot::resumable(conn,&sub,now)?})
        }
        Operation::Ack { key, refs, .. } => ack::apply(conn, &sub, m, c, &key, refs, now)?,
        Operation::Pull {
            continuation,
            items,
            ..
        } => snapshot::page(
            conn,
            &sub,
            m,
            c,
            "pull",
            None,
            continuation,
            items,
            now,
            &coverage,
        )?,
        Operation::Show { origin, .. } => snapshot::page(
            conn,
            &sub,
            m,
            c,
            "show",
            Some(origin),
            None,
            1,
            now,
            &coverage,
        )?,
        Operation::Thread {
            origin,
            continuation,
            items,
            ..
        } => snapshot::page(
            conn,
            &sub,
            m,
            c,
            "thread",
            Some(origin),
            continuation,
            items,
            now,
            &coverage,
        )?,
    };
    value["ok"] = json!(true);
    value["subscription"] = json!(sub.id);
    value["incarnation"] = json!(sub.incarnation);
    if value.get("coverage").is_none() {
        value["coverage"] = coverage;
    }
    coverage::validate(&value["coverage"], &[])?;
    ensure!(serde_json::to_vec(&value)?.len() <= BYTES, "backpressure");
    Ok(value)
}

fn visible(m: &Manifest, c: &DelegatedContext, o: &Observation) -> bool {
    // Both conversation endpoints and the origin source's ship membership
    // must be authorized. A desk copy never widens either boundary.
    let Some(source) = m.sources.iter().find(|s| s.core == o.origin.core) else {
        return false;
    };
    let (Some(from), Some(to)) = (o.content["from"].as_str(), o.content["to"].as_str()) else {
        return false;
    };
    if (from == "captain") == (to == "captain") {
        return false;
    }
    let ship = if from == "captain" { to } else { from };
    c.allows(ship) && source.ships.iter().any(|member| member == ship)
}
fn retire(conn: &Connection, id: &str) -> Result<()> {
    for table in [
        "desk_subscription_replay",
        "desk_subscription_snapshot",
        "desk_subscription_ref",
        "desk_subscription_progress",
        "desk_subscription_receipt",
    ] {
        conn.execute(&format!("DELETE FROM {table} WHERE subscription=?1"), [id])?;
    }
    selection::clear(conn, id)?;
    conn.execute("DELETE FROM desk_subscription WHERE id=?1", [id])?;
    Ok(())
}

fn through(conn: &Connection, sub: &Subscription, o: &Observation) -> Result<i64> {
    Ok(conn.query_row("SELECT through_order FROM desk_subscription_progress WHERE subscription=?1 AND core=?2 AND mailbox=?3",params![sub.id,o.origin.core,o.origin.mailbox],|r|r.get(0)).optional()?.unwrap_or(0))
}
fn acknowledged(conn: &Connection, sub: &Subscription, o: &Observation) -> Result<bool> {
    if o.order <= through(conn, sub, o)? {
        return Ok(true);
    }
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM desk_subscription_ref WHERE subscription=?1 AND incarnation=?2 AND core=?3 AND mailbox=?4 AND mail=?5 AND source_order=?6 AND acked=1)",params![sub.id,sub.incarnation,o.origin.core,o.origin.mailbox,o.origin.id,o.order],|r|r.get(0))?)
}
