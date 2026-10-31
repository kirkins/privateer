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
mod schema;
mod snapshot;
#[cfg(test)]
mod tests;
pub(crate) use schema::init;
pub(super) use schema::install;

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
            json!({"ok":false,"error":code,"coverage":coverage::unknown_domain()})
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
    let stored: Option<(String,String,String,String)> = conn.query_row("SELECT id,incarnation,owner,visibility FROM desk_subscription WHERE domain=?1 AND reader=?2",params![m.domain,reader],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let sub = if let Some((id, incarnation, old_owner, old_visibility)) = stored {
        ensure!(old_owner == owner, "stale_generation");
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
        ensure!(count < 16, "backpressure");
        let sub = Subscription {
            id: uuid(),
            incarnation: uuid(),
        };
        conn.execute("INSERT INTO desk_subscription(id,domain,reader,owner,incarnation,visibility) VALUES(?1,?2,?3,?4,?5,?6)",params![sub.id,m.domain,reader,owner,sub.incarnation,visibility])?;
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
    let endpoints: Vec<_> = ["from", "to"]
        .iter()
        .filter_map(|k| o.content[*k].as_str())
        .filter(|s| *s != "captain")
        .collect();
    !endpoints.is_empty()
        && endpoints.iter().all(|s| c.allows(s))
        && endpoints
            .iter()
            .any(|s| source.ships.iter().any(|member| member == s))
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
