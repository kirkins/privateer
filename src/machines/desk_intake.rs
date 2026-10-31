//! Source/intake projection, with bounded optional collect-contact transport.
//!
//! Normal writable startup installs schema. PR 2b-iv(a)'s `transport` module
//! activates capture/pages only on fenced manifest collect/push directions;
//! off-contact reconciliation records bounded retained-home evidence, while
//! optional barrier-bound proof sidecars revalidate that evidence. Source
//! completion is derived separately from placement, availability and freshness.
//! No public verb or aggregate reader is exposed. A journal
//! is an observation projection: it never delivers mail, closes a home request,
//! advances a reader cursor, or creates approval evidence. Linked cores share
//! operator trust; asserted origins are bookkeeping, not authentication.
#![cfg_attr(not(test), allow(dead_code))]
use super::desk_placement::Manifest;
use crate::door::coverage::{self, MailIdentity};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(super) mod baseline;
mod boot_clock;
pub(super) mod completion;
pub(crate) mod home;
pub(super) mod proof;
mod reconcile;
mod retention;
pub(crate) mod subscription;
pub(super) mod transport;

#[cfg(test)]
mod tests;

const ITEMS: usize = 50;
const BYTES: usize = 512 * 1024;
const EVENTS: i64 = 10_000;
const BARRIERS: i64 = 32;
const JOURNAL_BYTES: i64 = 64 * 1024 * 1024;

type StoredFact = (String, Option<String>, String, u64, String);
type StoredContact = (String, i64, i64, String);
type StoredCheckpoint = (String, String, i64, i64, String);
type StoredEvidence = (
    String,
    i64,
    i64,
    Option<i64>,
    Option<String>,
    Option<i64>,
    bool,
    String,
    String,
);

/// Explicit writable initialization only; reads never create/migrate tables.
/// Normal writable registry startup only; current schemas take no write lock.
pub(super) fn init(conn: &Connection) -> Result<()> {
    let tables = [
        "desk_source_event",
        "desk_source_head",
        "desk_source_cleanup",
        "desk_source_barrier",
        "desk_source_issuance",
        "desk_intake_fact",
        "desk_intake_original",
        "desk_intake_checkpoint",
        "desk_intake_contact",
        "desk_intake_gap",
        "desk_home_receipt",
        "desk_home_copy",
        "desk_home_inventory",
        "desk_home_inventory_gap",
        "desk_intake_order",
        "desk_intake_transport",
        "desk_baseline_cursor",
        "desk_baseline_home",
        "desk_intake_audit",
        "desk_source_reconciliation",
        "desk_source_proof",
        "desk_intake_proof",
        "desk_intake_loss",
        "desk_source_revision",
        "desk_source_proof_work",
    ];
    let has_source = |c: &Connection| -> Result<bool> {
        Ok(c.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('desk_intake_original') WHERE name='source')",
            [], |r| r.get(0),
        )?)
    };
    if !crate::mail::source::missing(conn, &tables)?
        && has_source(conn)?
        && retention::replay::ready(conn)?
    {
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    // Legacy first-capture/history-not-yet-proved and availability markers
    // are recoverable. Preserve explicit loss and unknown diagnostics below.
    let legacy = crate::mail::source::missing(&tx, &["desk_intake_loss"])?;
    if crate::mail::source::missing(&tx, &tables)? {
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS desk_source_head(
           domain TEXT NOT NULL, source TEXT NOT NULL, binding TEXT NOT NULL,
           upper_seq INTEGER NOT NULL, reclaimed_through INTEGER NOT NULL DEFAULT 0,
           issued_at INTEGER, PRIMARY KEY(domain,source));
         CREATE TABLE IF NOT EXISTS desk_source_event(
           domain TEXT NOT NULL, source TEXT NOT NULL, generation INTEGER NOT NULL,
           incarnation TEXT NOT NULL, seq INTEGER NOT NULL, intent TEXT NOT NULL,
           event TEXT NOT NULL, PRIMARY KEY(domain,source,seq),
           UNIQUE(domain,source,intent));
         CREATE TABLE IF NOT EXISTS desk_source_cleanup(
           domain TEXT NOT NULL, source TEXT NOT NULL, scan_seq INTEGER NOT NULL,
           upper_seq INTEGER NOT NULL DEFAULT 0, pruned_through INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(domain,source));
         CREATE INDEX IF NOT EXISTS desk_source_origin_seq ON desk_source_event(
           domain,source,json_extract(event,'$.origin.core'),
           json_extract(event,'$.origin.mailbox'),json_extract(event,'$.origin.id'),seq);
         CREATE TABLE IF NOT EXISTS desk_source_barrier(
           domain TEXT NOT NULL, source TEXT NOT NULL, id TEXT NOT NULL,
           binding TEXT NOT NULL, upper_seq INTEGER NOT NULL,
           PRIMARY KEY(domain,source,id));
         CREATE TABLE IF NOT EXISTS desk_source_issuance(
           issued INTEGER PRIMARY KEY AUTOINCREMENT, domain TEXT NOT NULL,
           source TEXT NOT NULL, id TEXT NOT NULL, UNIQUE(domain,source,id));
         CREATE TABLE IF NOT EXISTS desk_intake_fact(
           domain TEXT NOT NULL, owner_core TEXT NOT NULL, generation INTEGER NOT NULL,
           incarnation TEXT NOT NULL, source TEXT NOT NULL, core TEXT NOT NULL,
           mailbox TEXT NOT NULL, id TEXT NOT NULL, source_order INTEGER NOT NULL,
           content TEXT NOT NULL, state TEXT, unread INTEGER NOT NULL, pending INTEGER NOT NULL,
           PRIMARY KEY(domain,core,mailbox,id));
         CREATE TABLE IF NOT EXISTS desk_intake_original(
           domain TEXT NOT NULL, owner_core TEXT NOT NULL, generation INTEGER NOT NULL,
           incarnation TEXT NOT NULL, core TEXT NOT NULL, mailbox TEXT NOT NULL,
           id TEXT NOT NULL, first_order INTEGER NOT NULL CHECK(first_order>0),
           PRIMARY KEY(domain,core,mailbox,id));
         CREATE TABLE IF NOT EXISTS desk_intake_checkpoint(
           domain TEXT NOT NULL, source TEXT NOT NULL, binding TEXT NOT NULL,
           barrier TEXT NOT NULL, upper_seq INTEGER NOT NULL, through_seq INTEGER NOT NULL,
           receipt TEXT NOT NULL, received_at INTEGER, session TEXT, tick_ms INTEGER,
           clock_valid INTEGER NOT NULL DEFAULT 0, mode TEXT NOT NULL,
           PRIMARY KEY(domain,source));
         CREATE TABLE IF NOT EXISTS desk_intake_contact(
           domain TEXT NOT NULL, source TEXT NOT NULL, id TEXT NOT NULL,
           binding TEXT NOT NULL, upper_seq INTEGER NOT NULL, through_seq INTEGER NOT NULL,
           receipt TEXT NOT NULL, PRIMARY KEY(domain,source,id));
         CREATE TABLE IF NOT EXISTS desk_intake_gap(
           domain TEXT NOT NULL, source TEXT NOT NULL, state TEXT NOT NULL,
           PRIMARY KEY(domain,source));
         CREATE TABLE IF NOT EXISTS desk_home_receipt(
           domain TEXT NOT NULL, source TEXT NOT NULL, intent TEXT NOT NULL,
           binding TEXT NOT NULL, event TEXT NOT NULL, receipt TEXT NOT NULL,
           PRIMARY KEY(domain,source,intent));
         CREATE TABLE IF NOT EXISTS desk_home_copy(
           domain TEXT NOT NULL, source TEXT NOT NULL, intent TEXT NOT NULL,
           event TEXT NOT NULL, PRIMARY KEY(domain,source,intent));
         CREATE TABLE IF NOT EXISTS desk_home_inventory(
           home TEXT PRIMARY KEY);
         CREATE TABLE IF NOT EXISTS desk_home_inventory_gap(
           only INTEGER PRIMARY KEY CHECK(only=1), state TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS desk_intake_order(
           domain TEXT NOT NULL, source TEXT NOT NULL, issued INTEGER NOT NULL,
           barrier TEXT NOT NULL, PRIMARY KEY(domain,source));
         CREATE TABLE IF NOT EXISTS desk_intake_transport(
           domain TEXT NOT NULL, source TEXT NOT NULL, binding TEXT NOT NULL,
           after_seq INTEGER NOT NULL DEFAULT 0, pending TEXT, home TEXT,
           supported INTEGER NOT NULL DEFAULT 0, receipted INTEGER NOT NULL DEFAULT 0,
           last_receipt TEXT,
           PRIMARY KEY(domain,source));
         CREATE TABLE IF NOT EXISTS desk_baseline_home(
           domain TEXT NOT NULL, source TEXT NOT NULL, home TEXT NOT NULL,
           state TEXT NOT NULL, attempted_ms INTEGER NOT NULL,
           PRIMARY KEY(domain,source,home));
         CREATE TABLE IF NOT EXISTS desk_baseline_cursor(
           domain TEXT NOT NULL, source TEXT NOT NULL, home TEXT,
           PRIMARY KEY(domain,source));
         CREATE TABLE IF NOT EXISTS desk_source_reconciliation(
           domain TEXT NOT NULL, source TEXT NOT NULL, proof TEXT NOT NULL,
           PRIMARY KEY(domain,source));
         CREATE TABLE IF NOT EXISTS desk_source_proof(
           domain TEXT NOT NULL, source TEXT NOT NULL, barrier TEXT NOT NULL,
           body TEXT NOT NULL, PRIMARY KEY(domain,source,barrier));
         CREATE TABLE IF NOT EXISTS desk_intake_proof(
           domain TEXT NOT NULL, source TEXT NOT NULL, body TEXT, valid INTEGER NOT NULL DEFAULT 0,
           PRIMARY KEY(domain,source));
         CREATE TABLE IF NOT EXISTS desk_source_proof_work(
           domain TEXT NOT NULL, source TEXT NOT NULL, vector TEXT NOT NULL,
           next_home INTEGER NOT NULL, PRIMARY KEY(domain,source));
         CREATE TABLE IF NOT EXISTS desk_source_revision(
           domain TEXT NOT NULL, source TEXT NOT NULL, revision INTEGER NOT NULL,
           PRIMARY KEY(domain,source));
         CREATE TABLE IF NOT EXISTS desk_intake_loss(
           domain TEXT NOT NULL, source TEXT NOT NULL, state TEXT NOT NULL,
           PRIMARY KEY(domain,source));
         CREATE TABLE IF NOT EXISTS desk_intake_audit(
           seq INTEGER PRIMARY KEY AUTOINCREMENT, peer TEXT NOT NULL,
           stage TEXT NOT NULL, body TEXT NOT NULL, at_ms INTEGER NOT NULL,
           UNIQUE(peer,stage,body));",
        )?;
    }
    if !has_source(&tx)? {
        tx.execute_batch("ALTER TABLE desk_intake_original ADD COLUMN source TEXT")?;
    }
    retention::replay::init(&tx)?;
    if legacy {
        tx.execute("INSERT OR IGNORE INTO desk_intake_loss
            SELECT domain,source,state FROM desk_intake_gap
            WHERE state NOT IN ('history_gap','backpressure','unsupported','unreadable','baseline_too_large')", [])?;
        // Legacy history_gap also meant first capture/not yet proved. Sparse
        // events are not loss evidence: acknowledged reclamation deletes safe
        // rows individually, even beyond a retained live row, while advancing
        // reclaimed_through only over the absent prefix. Keep this ambiguous
        // diagnostic recoverable; home/inventory loss and unresolved provenance
        // remain independent proof blockers.
    }
    tx.commit()?;
    Ok(())
}

/// Immutable content includes correlation and outgoing/incoming participants.
/// Lifecycle is separate: a closure is another ordered fact, not a mutation of
/// the captain request at either home. A copy claim is explicit, never guessed
/// from matching text, paths, IDs alone or copied mailbox sequences.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Fact {
    origin: MailIdentity,
    copy: Option<MailIdentity>,
    content: Value,
    state: Option<String>,
    unread: bool,
    pending: bool,
}
impl Fact {
    fn validate(&self, source: &str) -> Result<()> {
        ensure!(
            self.origin
                == MailIdentity::new(&self.origin.core, &self.origin.mailbox, &self.origin.id)?,
            "invalid origin"
        );
        ensure!(
            self.origin.core == source,
            "integrity_gap: transitive origin"
        );
        if let Some(copy) = &self.copy {
            ensure!(
                *copy == MailIdentity::new(&copy.core, &copy.mailbox, &copy.id)?,
                "invalid copy"
            );
        }
        ensure!(self.content.is_object(), "invalid content");
        ensure!(
            matches!(self.state.as_deref(), None | Some("open" | "closed")),
            "invalid lifecycle state"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Event {
    order: i64,
    fact: Fact,
}

/// Uses the actual PR 2a manifest/owner generation, not path or machine name.
fn binding(m: &Manifest) -> Result<String> {
    Ok(crate::core::hash(&serde_json::to_value(m)?))
}
fn participant(m: &Manifest, source: &str) -> Result<()> {
    ensure!(m.sources.iter().any(|s| s.core == source), "unknown source");
    Ok(())
}
fn gap(conn: &Connection, m: &Manifest, source: &str, state: &str) -> Result<()> {
    ensure!(
        [
            "integrity_gap",
            "backpressure",
            "history_gap",
            "unreadable",
            "unsupported",
            "baseline_too_large"
        ]
        .contains(&state),
        "invalid intake gap"
    );
    if matches!(state, "integrity_gap" | "history_gap") {
        conn.execute("INSERT INTO desk_intake_loss(domain,source,state) VALUES(?1,?2,?3)
            ON CONFLICT(domain,source) DO UPDATE SET state=excluded.state WHERE state<>'integrity_gap'",
            params![m.domain, source, state])?;
    }
    obstacle(conn, m, source, state)
}

// A dated obstacle is not evidence of lost custody. Only explicit proof may
// discharge it; this helper cannot modify the independent sticky loss record.
fn obstacle(conn: &Connection, m: &Manifest, source: &str, state: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO desk_intake_gap(domain,source,state) VALUES(?1,?2,?3)
        ON CONFLICT(domain,source) DO UPDATE SET state=CASE
          WHEN state='integrity_gap' OR excluded.state='history_gap'
            OR (state='baseline_too_large' AND excluded.state IN ('backpressure','unsupported'))
          THEN state ELSE excluded.state END",
        params![m.domain, source, state],
    )?;
    Ok(())
}

/// Home/registry boundary: capture supplies a stable intent ID, persisted with
/// the immutable event. A crash after this commit but before the home receipt
/// simply retries that same intent. No distributed SQL transaction is claimed.
/// Production entry points must obtain `desk_placement::intake_manifest` and
/// hold its fence; `transport` does so without a network call under the fence.
fn append(conn: &Connection, m: &Manifest, source: &str, intent: &str, fact: &Fact) -> Result<i64> {
    participant(m, source)?;
    uuid::Uuid::parse_str(intent)?;
    let result = (|| {
        let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        let order = append_in(&tx, m, source, intent, fact)?;
        tx.commit()?;
        Ok(order)
    })();
    record_failure(conn, m, source, &result)?;
    result
}

// Also used by home import so event custody and its retry proof commit together.
fn append_in(
    tx: &Connection,
    m: &Manifest,
    source: &str,
    intent: &str,
    fact: &Fact,
) -> Result<i64> {
    fact.validate(source)?;
    let content = serde_json::to_string(fact)?;
    if let Some((order, old, generation, incarnation)) = tx.query_row(
            "SELECT seq,event,generation,incarnation FROM desk_source_event WHERE domain=?1 AND source=?2 AND intent=?3",
            params![m.domain, source, intent], |r| Ok((r.get::<_, i64>(0)?,r.get::<_, String>(1)?,r.get::<_, u64>(2)?,r.get::<_, String>(3)?)),
        ).optional()? {
            ensure!(generation == m.generation && incarnation == m.incarnation, "stale_generation");
            ensure!(old == content, "integrity_gap: conflicting import intent");
            return Ok(order);
        }
    let (count, used, stale): (i64, i64, bool) = tx.query_row("SELECT COUNT(*),COALESCE(SUM(length(CAST(event AS BLOB))),0),COALESCE(MAX(generation<>?3 OR incarnation<>?4),0) FROM desk_source_event WHERE domain=?1 AND source=?2", params![m.domain, source,m.generation,m.incarnation], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    ensure!(!stale, "stale_generation");
    // Reserve the entire one-event wire envelope before accepting custody.
    // Use the widest valid UUID spelling and sequence fields so a later
    // barrier can always page this fact, even at the sequence boundary.
    let single = Page {
        checkpoint: Checkpoint {
            binding: binding(m)?,
            source: source.into(),
            id: "urn:uuid:ffffffff-ffff-ffff-ffff-ffffffffffff".into(),
            upper: i64::MAX,
            issued: i64::MAX,
        },
        after: i64::MAX - 1,
        through: i64::MAX,
        events: vec![Event {
            order: i64::MAX,
            fact: fact.clone(),
        }],
    };
    ensure!(
        count < EVENTS
            && serde_json::to_vec(&single)?.len() <= BYTES
            && used + content.len() as i64 <= JOURNAL_BYTES,
        "backpressure"
    );
    // The durable head survives acknowledged-prefix reclamation. Sequence
    // numbers must never be reused, including when the log becomes empty.
    let order = retention::head(tx, m, source)?
        .checked_add(1)
        .context("backpressure: source order exhausted")?;
    tx.execute(
        "UPDATE desk_source_head SET upper_seq=?3 WHERE domain=?1 AND source=?2",
        params![m.domain, source, order],
    )?;
    tx.execute("INSERT INTO desk_source_event(domain,source,generation,incarnation,seq,intent,event) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![m.domain, source, m.generation, m.incarnation, order, intent, content])?;
    Ok(order)
}

fn record_failure<T>(
    conn: &Connection,
    m: &Manifest,
    source: &str,
    result: &Result<T>,
) -> Result<()> {
    if let Err(e) = result {
        let text = e.to_string();
        for state in [
            "backpressure",
            "integrity_gap",
            "history_gap",
            "unreadable",
            "unsupported",
            "baseline_too_large",
        ] {
            if text.starts_with(state) {
                gap(conn, m, source, state).with_context(|| {
                    format!("intake failed ({e:#}); failed to record {state} gap")
                })?;
                break;
            }
        }
    }
    Ok(())
}

/// An immutable upper checkpoint. New events cannot extend an existing contact.
/// Empty contacts are real barriers. IDs identify contacts, not page retries.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    binding: String,
    source: String,
    id: String,
    upper: i64,
    issued: i64,
}
fn checkpoint(conn: &Connection, m: &Manifest, source: &str, id: &str) -> Result<Checkpoint> {
    let result = (|| {
        let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        let cp = issue_checkpoint(&tx, m, source, id)?;
        tx.commit()?;
        Ok(cp)
    })();
    record_failure(conn, m, source, &result)?;
    result
}

// Also used inside transport's atomic final-receipt reclamation/page commit.
fn issue_checkpoint(
    tx: &rusqlite::Transaction<'_>,
    m: &Manifest,
    source: &str,
    id: &str,
) -> Result<Checkpoint> {
    participant(m, source)?;
    uuid::Uuid::parse_str(id)?;
    let bound = binding(m)?;
    if let Some((old, upper)) = tx.query_row("SELECT binding,upper_seq FROM desk_source_barrier WHERE domain=?1 AND source=?2 AND id=?3", params![m.domain, source, id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).optional()? {
            ensure!(old == bound, "stale_generation");
            let issued: i64 = tx.query_row("SELECT issued FROM desk_source_issuance WHERE domain=?1 AND source=?2 AND id=?3",params![m.domain,source,id],|r|r.get(0))?;
            return Ok(Checkpoint { binding: old, source: source.into(), id: id.into(), upper, issued });
        }
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM desk_source_barrier WHERE domain=?1 AND source=?2",
        params![m.domain, source],
        |r| r.get(0),
    )?;
    ensure!(count < BARRIERS, "backpressure");
    let (_, stale): (i64, bool) = tx.query_row(
            "SELECT COALESCE(MAX(seq),0),COALESCE(MAX(generation<>?3 OR incarnation<>?4),0) FROM desk_source_event WHERE domain=?1 AND source=?2",
            params![m.domain, source, m.generation, m.incarnation],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
    ensure!(!stale, "stale_generation");
    let upper = retention::head(tx, m, source)?;
    tx.execute("INSERT INTO desk_source_barrier(domain,source,id,binding,upper_seq) VALUES(?1,?2,?3,?4,?5)", params![m.domain, source, id, bound, upper])?;
    tx.execute(
        "INSERT INTO desk_source_issuance(domain,source,id) VALUES(?1,?2,?3)",
        params![m.domain, source, id],
    )?;
    let issued = tx.last_insert_rowid();
    Ok(Checkpoint {
        binding: bound,
        source: source.into(),
        id: id.into(),
        upper,
        issued,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Page {
    checkpoint: Checkpoint,
    after: i64,
    through: i64,
    events: Vec<Event>,
}
fn page(
    conn: &Connection,
    m: &Manifest,
    cp: &Checkpoint,
    after: i64,
    items: usize,
    bytes: usize,
) -> Result<Page> {
    participant(m, &cp.source)?;
    let result = page_inner(conn, m, cp, after, items, bytes);
    record_failure(conn, m, &cp.source, &result)?;
    result
}

fn page_inner(
    conn: &Connection,
    m: &Manifest,
    cp: &Checkpoint,
    after: i64,
    items: usize,
    bytes: usize,
) -> Result<Page> {
    ensure!(cp.binding == binding(m)?, "stale_generation");
    ensure!(
        (0..=cp.upper).contains(&after)
            && (1..=ITEMS).contains(&items)
            && (1..=BYTES).contains(&bytes),
        "invalid page bounds"
    );
    let stored: (String, i64, i64) = conn.query_row(
        "SELECT b.binding,b.upper_seq,i.issued FROM desk_source_barrier b JOIN desk_source_issuance i USING(domain,source,id) WHERE b.domain=?1 AND b.source=?2 AND b.id=?3",
        params![m.domain, cp.source, cp.id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    ensure!(
        stored == (cp.binding.clone(), cp.upper, cp.issued),
        "invalid checkpoint"
    );
    let reclaimed: i64 = conn.query_row(
        "SELECT COALESCE((SELECT reclaimed_through FROM desk_source_head WHERE domain=?1 AND source=?2),0)",
        params![m.domain, cp.source], |r| r.get(0))?;
    ensure!(
        after >= reclaimed
            && after >= conn.query_row(
                "SELECT COALESCE((SELECT pruned_through FROM desk_source_cleanup WHERE domain=?1 AND source=?2),0)",
                params![m.domain, cp.source], |r| r.get::<_, i64>(0))?,
        "stale_or_unknown: reclaimed source prefix"
    );
    let mut p = Page {
        checkpoint: cp.clone(),
        after,
        through: after,
        events: vec![],
    };
    ensure!(
        serde_json::to_vec(&p)?.len() <= bytes,
        "invalid page bounds: metadata exceeds byte limit"
    );
    let mut stmt = conn.prepare("SELECT seq,event FROM desk_source_event WHERE domain=?1 AND source=?2 AND seq>?3 AND seq<=?4 ORDER BY seq LIMIT ?5")?;
    let rows = stmt.query_map(
        params![m.domain, cp.source, after, cp.upper, items as i64],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
    )?;
    for row in rows {
        let (order, text) = row?;
        ensure!(order == p.through + 1, "history_gap");
        p.events.push(Event {
            order,
            fact: serde_json::from_str(&text)?,
        });
        p.through = order;
        if serde_json::to_vec(&p)?.len() > bytes {
            p.events.pop();
            p.through = order - 1;
            ensure!(!p.events.is_empty(), "backpressure: oversized fact");
            break;
        }
    }
    ensure!(p.through == cp.upper || !p.events.is_empty(), "history_gap");
    Ok(p)
}

/// Supplied by the desk process, never from wire time. `session` changes on
/// restart; monotonic ticks and wall time must agree before evidence is fresh.
struct Clock<'a> {
    session: &'a str,
    wall_ms: i64,
    tick_ms: i64,
}
fn mode(m: &Manifest, source: &str) -> Result<&'static str> {
    let s = m
        .sources
        .iter()
        .find(|s| s.core == source)
        .context("unknown source")?;
    match s.intake_direction.as_str() {
        "local" | "collect" => Ok("on_demand"),
        "push" => Ok("last_received_push"),
        _ => anyhow::bail!("illegal source direction"),
    }
}

fn apply_fact(tx: &Connection, m: &Manifest, source: &str, e: &Event) -> Result<()> {
    let f = &e.fact;
    f.validate(source)?;
    let id = &f.origin;
    ensure!(
        coverage::origin_of(tx, id)? == *id,
        "integrity_gap: origin is already a copy"
    );
    let content = serde_json::to_string(&f.content)?;
    let old: Option<StoredFact> = tx.query_row(
        "SELECT content,state,owner_core,generation,incarnation FROM desk_intake_fact WHERE domain=?1 AND core=?2 AND mailbox=?3 AND id=?4",
        params![m.domain, id.core, id.mailbox, id.id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)),
    ).optional()?;
    let first_content = old.is_none();
    if let Some((old_content, state, owner, generation, incarnation)) = old {
        ensure!(
            old_content == content,
            "integrity_gap: conflicting origin content"
        );
        ensure!(
            owner == m.owner_core && generation == m.generation && incarnation == m.incarnation,
            "stale_generation"
        );
        ensure!(
            !(state.as_deref() == Some("closed") && f.state.as_deref() != Some("closed")),
            "integrity_gap: lifecycle reopened"
        );
    } else {
        let (count, used): (i64, i64) = tx.query_row("SELECT COUNT(*),COALESCE(SUM(length(CAST(content AS BLOB))),0) FROM desk_intake_fact WHERE domain=?1", [&m.domain], |r| Ok((r.get(0)?,r.get(1)?)))?;
        ensure!(
            count < EVENTS && used + content.len() as i64 <= JOURNAL_BYTES,
            "backpressure"
        );
    }
    if let Some(copy) = &f.copy {
        // An identity already observed as an origin cannot be reassigned.
        let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM desk_intake_fact WHERE domain=?1 AND core=?2 AND mailbox=?3 AND id=?4)", params![m.domain, copy.core, copy.mailbox, copy.id], |r| r.get(0))?;
        ensure!(!exists || *copy == *id, "integrity_gap: copy was an origin");
        ensure!(
            coverage::record_alias_in(tx, copy, id)? != coverage::Alias::Conflict,
            "integrity_gap: conflicting copy provenance"
        );
    }
    tx.execute("INSERT INTO desk_intake_fact(domain,owner_core,generation,incarnation,source,core,mailbox,id,source_order,content,state,unread,pending)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
        ON CONFLICT(domain,core,mailbox,id) DO UPDATE SET source_order=excluded.source_order,state=excluded.state,unread=excluded.unread,pending=excluded.pending",
        params![m.domain,m.owner_core,m.generation,m.incarnation,source,id.core,id.mailbox,id.id,e.order,content,f.state,f.unread,f.pending])?;
    if first_content {
        // Owner-side first applied CONTENT order, not mutable observation order.
        // Contiguous journal intake preserves it across later lifecycle/ack
        // facts. Never backfill legacy origins from their latest observation:
        // that would manufacture 'new' content after an upgrade or rollback.
        tx.execute("INSERT INTO desk_intake_original(domain,owner_core,generation,incarnation,core,mailbox,id,first_order,source) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![m.domain,m.owner_core,m.generation,m.incarnation,id.core,id.mailbox,id.id,e.order,source])?;
    }
    Ok(())
}

/// Atomic facts + aliases + progress + receipt. Only the last page of a NEW
/// barrier dates a completed contact; replays and partial pages never do.
/// This is internal protocol evidence, NOT activated source completeness.
fn accept(conn: &Connection, m: &Manifest, p: &Page, clock: &Clock<'_>) -> Result<Value> {
    participant(m, &p.checkpoint.source)?;
    let result = accept_inner(conn, m, p, clock);
    record_failure(conn, m, &p.checkpoint.source, &result)?;
    result
}

fn accept_inner(conn: &Connection, m: &Manifest, p: &Page, clock: &Clock<'_>) -> Result<Value> {
    ensure!(p.checkpoint.binding == binding(m)?, "stale_generation");
    uuid::Uuid::parse_str(&p.checkpoint.id)?;
    ensure!(
        clock.wall_ms >= 0 && clock.tick_ms >= 0,
        "invalid desk clock"
    );
    ensure!(
        p.events.len() <= ITEMS && serde_json::to_vec(p)?.len() <= BYTES,
        "backpressure: page bounds"
    );
    ensure!(
        0 <= p.after && p.after <= p.through && p.through <= p.checkpoint.upper,
        "invalid page order"
    );
    ensure!(p.events.len() as i64 == p.through - p.after, "history_gap");
    for (i, event) in p.events.iter().enumerate() {
        ensure!(event.order == p.after + i as i64 + 1, "history_gap");
    }
    let source = &p.checkpoint.source;
    let receipt = crate::core::hash(&serde_json::to_value(p)?);
    (|| {
        let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        let contact: Option<StoredContact> = tx.query_row("SELECT binding,upper_seq,through_seq,receipt FROM desk_intake_contact WHERE domain=?1 AND source=?2 AND id=?3", params![m.domain,source,p.checkpoint.id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        if let Some((bound, upper, progress, old_receipt)) = contact {
            ensure!(
                bound == p.checkpoint.binding && upper == p.checkpoint.upper,
                "integrity_gap: changed contact binding"
            );
            if old_receipt == receipt {
                return Ok(
                    json!({"accepted":true,"replayed":true,"through":progress,"complete":false}),
                );
            }
            ensure!(progress < upper, "stale_or_unknown");
        }
        ensure!(p.checkpoint.issued > 0, "invalid barrier issuance");
        if let Some((issued, barrier)) = tx
            .query_row(
                "SELECT issued,barrier FROM desk_intake_order WHERE domain=?1 AND source=?2",
                params![m.domain, source],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
        {
            ensure!(
                p.checkpoint.issued >= issued,
                "stale_or_unknown: superseded barrier"
            );
            ensure!(
                p.checkpoint.issued != issued || p.checkpoint.id == barrier,
                "integrity_gap: conflicting barrier issuance"
            );
        }
        // Completed, superseded receipts can be forgotten: desk_intake_order
        // still rejects their old ordinals. Never reclaim an interrupted page's
        // custody, nor events/facts. Rollback restores receipts on any failure.
        tx.execute(
            "DELETE FROM desk_intake_contact WHERE domain=?1 AND source=?2 AND id<>?3
            AND binding=?4 AND through_seq=upper_seq",
            params![m.domain, source, p.checkpoint.id, p.checkpoint.binding],
        )?;
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM desk_intake_contact WHERE domain=?1 AND source=?2 AND id<>?3",
            params![m.domain, source, p.checkpoint.id],
            |r| r.get(0),
        )?;
        ensure!(count < BARRIERS, "backpressure");
        let old: Option<StoredCheckpoint> = tx.query_row(
            "SELECT binding,barrier,upper_seq,through_seq,receipt FROM desk_intake_checkpoint WHERE domain=?1 AND source=?2", params![m.domain,source],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)),
        ).optional()?;
        let mut through = 0;
        let mut new_barrier = true;
        if let Some((bound, barrier, upper, progress, old_receipt)) = old {
            ensure!(bound == p.checkpoint.binding, "stale_generation");
            if old_receipt == receipt {
                return Ok(
                    json!({"accepted":true,"replayed":true,"through":progress,"complete":false}),
                );
            }
            through = progress;
            new_barrier = barrier != p.checkpoint.id;
            if !new_barrier {
                ensure!(
                    upper == p.checkpoint.upper,
                    "integrity_gap: changed barrier upper"
                );
                ensure!(progress < upper, "stale_or_unknown");
            } else {
                ensure!(
                    p.checkpoint.upper >= upper,
                    "stale_or_unknown: regressed barrier; resume after {progress}"
                );
            }
        }
        ensure!(
            p.after == through,
            "stale_or_unknown: noncontiguous page; resume after {through}"
        );
        for event in &p.events {
            apply_fact(&tx, m, source, event)?;
        }
        // A continuation that completes a new contact also dates it; until
        // then clock_valid stays false. Replayed final pages returned above.
        let done = p.through == p.checkpoint.upper;
        let receipt_time = done.then_some(clock.wall_ms);
        let session = done.then_some(clock.session);
        let tick = done.then_some(clock.tick_ms);
        tx.execute("INSERT INTO desk_intake_checkpoint(domain,source,binding,barrier,upper_seq,through_seq,receipt,received_at,session,tick_ms,clock_valid,mode)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
            ON CONFLICT(domain,source) DO UPDATE SET binding=excluded.binding,barrier=excluded.barrier,upper_seq=excluded.upper_seq,through_seq=excluded.through_seq,receipt=excluded.receipt,received_at=COALESCE(excluded.received_at,desk_intake_checkpoint.received_at),session=COALESCE(excluded.session,desk_intake_checkpoint.session),tick_ms=COALESCE(excluded.tick_ms,desk_intake_checkpoint.tick_ms),clock_valid=excluded.clock_valid,mode=excluded.mode",
            params![m.domain,source,p.checkpoint.binding,p.checkpoint.id,p.checkpoint.upper,p.through,receipt,receipt_time,session,tick,done,mode(m,source)?])?;
        tx.execute("INSERT INTO desk_intake_order(domain,source,issued,barrier) VALUES(?1,?2,?3,?4)
            ON CONFLICT(domain,source) DO UPDATE SET issued=excluded.issued,barrier=excluded.barrier",
            params![m.domain,source,p.checkpoint.issued,p.checkpoint.id])?;
        tx.execute("INSERT INTO desk_intake_contact(domain,source,id,binding,upper_seq,through_seq,receipt) VALUES(?1,?2,?3,?4,?5,?6,?7)
            ON CONFLICT(domain,source,id) DO UPDATE SET through_seq=excluded.through_seq,receipt=excluded.receipt", params![m.domain,source,p.checkpoint.id,p.checkpoint.binding,p.checkpoint.upper,p.through,receipt])?;
        tx.commit()?;
        Ok(
            json!({"accepted":true,"replayed":false,"new_barrier":new_barrier,"through":p.through,"complete":false}),
        )
    })()
}

fn contact_fresh(continuous: bool, completed: bool, age: Option<i64>, max: i64) -> bool {
    continuous && completed && age.is_some_and(|a| (0..=max).contains(&a))
}

/// Evidence is persisted, but restart/clock discontinuity invalidates freshness
/// until another contact completes. Writable reads persist invalidation;
/// read-only connections report it without modifying stored evidence.
/// Handoff import MUST NOT call
/// accept: imported rows retain their old session and are therefore cached.
/// Dormant foundation: even eligible evidence is advertised as cached/partial.
fn source_coverage(
    conn: &Connection,
    m: &Manifest,
    source: &str,
    clock: &Clock<'_>,
) -> Result<coverage::Source> {
    Ok(source_evidence(conn, m, source, clock)?.0)
}

/// The private boolean evaluates only push-contact age policy for later
/// activation; it is not a coverage-completeness claim or an on-demand read.
fn source_evidence(
    conn: &Connection,
    m: &Manifest,
    source: &str,
    clock: &Clock<'_>,
) -> Result<(coverage::Source, bool)> {
    participant(m, source)?;
    let state: Option<String> = conn
        .query_row(
            "SELECT state FROM desk_intake_gap WHERE domain=?1 AND source=?2",
            params![m.domain, source],
            |r| r.get(0),
        )
        .optional()?;
    let row: Option<StoredEvidence> = conn.query_row(
        "SELECT barrier,upper_seq,through_seq,received_at,session,tick_ms,clock_valid,mode,binding FROM desk_intake_checkpoint WHERE domain=?1 AND source=?2", params![m.domain,source],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?)),
    ).optional()?;
    let mut result = coverage::Source {
        id: source.into(),
        state: "unavailable",
        through: None,
        barrier: None,
    };
    let mut push_fresh = false;
    if let Some((barrier, upper, through, received, session, tick, valid, mode, bound)) = row {
        if bound != binding(m)? {
            result.state = "stale_generation";
            return Ok((result, false));
        }
        result.through = Some(format!("{barrier}:{through}"));
        result.state = "cached";
        if let (Some(received), Some(tick)) = (received, tick) {
            let age = clock.wall_ms.checked_sub(received).filter(|v| *v >= 0);
            let elapsed = clock.tick_ms.checked_sub(tick).filter(|v| *v >= 0);
            let continuous = valid
                && !clock.session.is_empty()
                && session.as_deref() == Some(clock.session)
                && matches!((age,elapsed), (Some(a),Some(e)) if a.abs_diff(e) <= 1_000);
            let query_only: bool = conn.query_row("PRAGMA query_only", [], |r| r.get(0))?;
            if !continuous && !query_only && !conn.is_readonly(rusqlite::DatabaseName::Main)? {
                conn.execute(
                    "UPDATE desk_intake_checkpoint SET clock_valid=0 WHERE domain=?1 AND source=?2 AND clock_valid<>0",
                    params![m.domain, source],
                )?;
            }
            // On-demand evidence also remains cached until activation.
            push_fresh = mode == "last_received_push"
                && contact_fresh(continuous, through == upper, age, m.max_source_age_ms);
            // A new incomplete barrier must not be displayed beside the
            // previous completed contact's retained clock evidence.
            if through == upper {
                result.barrier = Some(coverage::Barrier {
                    mode: if mode == "last_received_push" {
                        "last_received_push"
                    } else {
                        "on_demand"
                    },
                    received_at: received,
                    age_ms: age.unwrap_or(i64::MAX),
                    max_source_age_ms: m.max_source_age_ms,
                });
            }
        }
    }
    if let Some(state) = state {
        push_fresh = false;
        result.state = match state.as_str() {
            "backpressure" | "baseline_too_large" => "backpressure",
            "unreadable" => "unreadable",
            "unsupported" => "unsupported",
            _ => "history_gap", // integrity is a coverage gap, never suppression.
        };
    }
    Ok((result, push_fresh))
}
