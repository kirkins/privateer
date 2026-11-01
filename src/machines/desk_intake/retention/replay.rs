//! Source-local replay custody only. Nothing here is serialized onto a contact.
//! All mutations run in the caller's registry import/adoption transaction.
use super::super::*;
use crate::mail::source::{HomeFact, Provenance};

mod snapshot;
mod storage;
pub(in crate::machines::desk_intake) use snapshot::observe;
use storage::{omitted, used};

const DETAILS: i64 = 10_000;
const WITNESSES: i64 = 2_000;
const TABLES: &[&str] = &[
    "desk_replay_import",
    "desk_replay_home",
    "desk_replay_witness",
    "desk_replay_omission",
    "desk_replay_budget",
];

pub(in crate::machines::desk_intake) fn ready(c: &Connection) -> Result<bool> {
    if crate::mail::source::missing(c, TABLES)? {
        return Ok(false);
    }
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM desk_replay_budget WHERE name='replay_v4')",
        [],
        |r| r.get(0),
    )?)
}
pub(in crate::machines::desk_intake) fn init(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS desk_replay_import(
         domain TEXT NOT NULL, source TEXT NOT NULL, binding TEXT NOT NULL,
         intent TEXT NOT NULL, home TEXT NOT NULL, mailbox TEXT NOT NULL, nonce TEXT,
         lane TEXT NOT NULL, journal_order INTEGER NOT NULL, origin TEXT NOT NULL,
         digest TEXT NOT NULL, event_digest TEXT NOT NULL, delta_order INTEGER,
         mail_order INTEGER, native INTEGER NOT NULL, adopted INTEGER NOT NULL DEFAULT 0,
         rebuild_cut TEXT,
         PRIMARY KEY(domain,source,intent), UNIQUE(domain,source,journal_order));
         CREATE TABLE IF NOT EXISTS desk_replay_home(
         domain TEXT NOT NULL, source TEXT NOT NULL, home TEXT NOT NULL,
         mail_order INTEGER NOT NULL,
         PRIMARY KEY(domain,source,home));
         CREATE TABLE IF NOT EXISTS desk_replay_witness(
         domain TEXT NOT NULL, source TEXT NOT NULL, intent TEXT NOT NULL,
         body TEXT NOT NULL, adopted INTEGER NOT NULL DEFAULT 0,
         PRIMARY KEY(domain,source,intent));",
    )?;
    storage::init(c)?;
    Ok(())
}

/// Snapshot-derived import metadata, never an eligibility decision. Missing
/// nonce/native/M is deliberately representable for legacy and baseline homes.
#[derive(Clone, Debug)]
pub(in crate::machines::desk_intake) struct Observation {
    home: String,
    mailbox: String,
    nonce: Option<String>,
    lane: String,
    delta_order: Option<i64>,
    mail_order: Option<i64>,
    native: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(in crate::machines::desk_intake) struct Witness {
    binding: String,
    source: String,
    home: String,
    mailbox: String,
    nonce: String,
    intent: String,
    journal_order: i64,
    origin: MailIdentity,
    digest: String,
    delta_order: i64,
    mail_order: i64,
}

/// Future cut-adoption code must supply the SAME immutable cut on rebuild;
/// this source-only slice neither establishes nor persists any desk cut.
#[derive(Clone, Debug, Serialize)]
pub(in crate::machines::desk_intake) struct Cut {
    checkpoint_digest: String,
    binding: String,
    source: String,
    home: String,
    mailbox: String,
    nonce: String,
    journal_upper: i64,
    delta_upper: i64,
    mail_upper: i64,
}
impl Witness {
    fn beyond(&self, cut: &Cut) -> bool {
        self.binding == cut.binding
            && self.source == cut.source
            && self.home == cut.home
            && self.mailbox == cut.mailbox
            && self.nonce == cut.nonce
            && self.journal_order > cut.journal_upper
            && self.delta_order > cut.delta_upper
            && self.mail_order > cut.mail_upper
    }
}

#[cfg(test)]
fn count(c: &Connection, table: &str) -> Result<i64> {
    Ok(c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?)
}

/// Atomic with append + retry receipt. Legacy already-receipted imports never
/// enter here; ordinary page receipts do not adopt or delete these records.
pub(in crate::machines::desk_intake) fn record(
    c: &Connection,
    m: &Manifest,
    source: &str,
    intent: &str,
    event: &str,
    order: i64,
    observation: Option<&Observation>,
) -> Result<()> {
    let f: HomeFact = serde_json::from_str(event)?;
    let digest = crate::core::hash(&f.content);
    let event_digest = crate::core::hash(&json!(event));
    let bound = binding(m)?;
    if let Some((old_binding, old_digest, old_event, old_order)) = c.query_row(
        "SELECT binding,digest,event_digest,journal_order FROM desk_replay_import WHERE domain=?1 AND source=?2 AND intent=?3",
        params![m.domain,source,intent], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?)),
    ).optional()? {
        ensure!(old_binding == bound && old_digest == digest && old_event == event_digest && old_order == order,
            "integrity_gap: conflicting replay intent");
        return Ok(());
    }
    // Copies consume no native proof storage; a later own-source native
    // import is the explicitly permitted copy-first path.
    let native_identity = matches!(&f.provenance, Provenance::Origin(id)
        if id.core == source && id.mailbox == f.mailbox && id.id == f.id);
    if !native_identity {
        // Ordinary non-native mail is not failed native proof and needs no
        // scarce rebuild detail. Its existing journal/receipt owns custody.
        return Ok(());
    }
    // Superseded custody cannot be adopted. Release it incrementally before
    // admission so a binding transition cannot permanently exhaust budgets.
    release(c, m, source, false)?;
    let Some(o) = observation else {
        omitted(c, m, source, order, "missing_snapshot")?;
        return Ok(());
    };
    let origin = serde_json::to_string(&MailIdentity::new(source, &f.mailbox, &f.id)?)?;
    // Baseline-era rows are vetoed by the cut's AUTOINCREMENT M, not by a
    // source per-origin tombstone. See §3.5.1 for the desk first_order veto.
    if o.lane != "delta" {
        return Ok(());
    }
    if !o.native || o.nonce.is_none() || o.delta_order.is_none() || o.mail_order.is_none() {
        omitted(c, m, source, order, "unproved_native")?;
        return Ok(());
    }
    let mail_order = o.mail_order.unwrap();
    // One monotonic mark per home, carried across binding/nonce changes. This
    // is stricter than a per-enrollment mark: restores may withhold more, never
    // mint an old candidate again. Advance even when detail admission fails.
    let advanced = c.execute(
        "INSERT INTO desk_replay_home VALUES(?1,?2,?3,?4)
         ON CONFLICT(domain,source,home) DO UPDATE SET mail_order=excluded.mail_order
         WHERE excluded.mail_order>desk_replay_home.mail_order",
        params![m.domain, source, o.home, mail_order],
    )?;
    if advanced == 0 {
        return Ok(());
    }
    if used(c, "desk_replay_import")? >= DETAILS {
        omitted(c, m, source, order, "detail_budget")?;
        return Ok(());
    }
    c.execute("INSERT INTO desk_replay_import(domain,source,binding,intent,home,mailbox,nonce,lane,journal_order,origin,digest,event_digest,delta_order,mail_order,native)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
        params![m.domain,source,bound,intent,o.home,o.mailbox,o.nonce,o.lane,order,origin,digest,event_digest,o.delta_order,o.mail_order,o.native])?;
    if let Some(w) = candidate(c, m, source, intent)? {
        if !save(c, m, source, &w)? {
            omitted(c, m, source, order, "witness_budget")?;
        }
    }
    Ok(())
}

fn candidate(c: &Connection, m: &Manifest, source: &str, intent: &str) -> Result<Option<Witness>> {
    let row = c.query_row(
        "SELECT i.binding,i.home,i.mailbox,i.nonce,i.journal_order,i.origin,i.digest,i.delta_order,i.mail_order
         FROM desk_replay_import i
         WHERE i.domain=?1 AND i.source=?2 AND i.intent=?3 AND i.native=1 AND i.lane='delta'
         AND i.nonce IS NOT NULL AND i.delta_order IS NOT NULL AND i.mail_order IS NOT NULL
         AND i.binding=?4",
        params![m.domain,source,intent,binding(m)?], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,i64>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,i64>(7)?,r.get::<_,i64>(8)?)),
    ).optional()?;
    row.map(
        |(
            binding,
            home,
            mailbox,
            nonce,
            journal_order,
            origin,
            digest,
            delta_order,
            mail_order,
        )| {
            Ok(Witness {
                binding,
                source: source.into(),
                home,
                mailbox,
                nonce,
                intent: intent.into(),
                journal_order,
                origin: serde_json::from_str(&origin)?,
                digest,
                delta_order,
                mail_order,
            })
        },
    )
    .transpose()
}

fn save(c: &Connection, m: &Manifest, source: &str, w: &Witness) -> Result<bool> {
    let text = serde_json::to_string(w)?;
    let old: Option<String> = c
        .query_row(
            "SELECT body FROM desk_replay_witness WHERE domain=?1 AND source=?2 AND intent=?3",
            params![m.domain, source, w.intent],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(old) = old {
        ensure!(old == text, "integrity_gap: conflicting witness");
        return Ok(true);
    }
    if used(c, "desk_replay_witness")? >= WITNESSES {
        return Ok(false);
    }
    c.execute(
        "INSERT INTO desk_replay_witness(domain,source,intent,body) VALUES(?1,?2,?3,?4)",
        params![m.domain, source, w.intent, text],
    )?;
    Ok(true)
}

/// Rebuild only a retained original native candidate, not a legacy receipt.
/// Caller supplies a current read-only home snapshot and an already-proved cut.
/// No network, no receipt/adoption, no selection and no home writes.
pub(in crate::machines::desk_intake) fn rebuild(
    path: &std::path::Path,
    c: &Connection,
    home: &Connection,
    m: &Manifest,
    source: &str,
    intent: &str,
    cut: &Cut,
) -> Result<bool> {
    let Some(w) = candidate(c, m, source, intent)? else {
        return Ok(false);
    };
    let cut_digest = crate::core::hash(&serde_json::to_value(cut)?);
    c.execute("UPDATE desk_replay_import SET rebuild_cut=?4 WHERE domain=?1 AND source=?2 AND intent=?3 AND rebuild_cut IS NULL",params![m.domain,source,intent,cut_digest])?;
    let pinned: String = c.query_row(
        "SELECT rebuild_cut FROM desk_replay_import WHERE domain=?1 AND source=?2 AND intent=?3",
        params![m.domain, source, intent],
        |r| r.get(0),
    )?;
    if pinned != cut_digest {
        omitted(c, m, source, w.journal_order, "rebuild_cut_mismatch")?;
        return Ok(false);
    }
    let current = snapshot::native_now(path, home, m, source, &w.origin, &w.digest)?;
    let head: i64 = home.query_row(
        "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name='mail_source_outbox'),0)",
        [],
        |r| r.get(0),
    )?;
    if !w.beyond(cut)
        || head < w.delta_order
        || current.as_ref() != Some(&(w.home.clone(), w.nonce.clone(), w.mail_order))
    {
        omitted(c, m, source, w.journal_order, "rebuild_unavailable")?;
        return Ok(false);
    }
    save(c, m, source, &w)
}

/// Exact adoption is an internal primitive for 1b, not wired to page receipts.
/// A future batch receipt must validate the entire batch digest before calling.
pub(in crate::machines::desk_intake) fn adopt(
    c: &Connection,
    m: &Manifest,
    w: &Witness,
) -> Result<()> {
    ensure!(w.binding == binding(m)?, "stale_generation");
    let saved: Option<String> = c
        .query_row(
            "SELECT body FROM desk_replay_witness WHERE domain=?1 AND source=?2 AND intent=?3",
            params![m.domain, w.source, w.intent],
            |r| r.get(0),
        )
        .optional()?;
    ensure!(
        saved.as_deref() == Some(serde_json::to_string(w)?.as_str()),
        "invalid witness adoption"
    );
    c.execute(
        "UPDATE desk_replay_witness SET adopted=1 WHERE domain=?1 AND source=?2 AND intent=?3",
        params![m.domain, w.source, w.intent],
    )?;
    c.execute(
        "UPDATE desk_replay_import SET adopted=1 WHERE domain=?1 AND source=?2 AND intent=?3",
        params![m.domain, w.source, w.intent],
    )?;
    Ok(())
}

/// At most fifty adopted or superseded details per call. Current unadopted
/// custody is retained. Superseded unadopted evidence can never be adopted;
/// disclose its old-binding order before releasing it. Home marks and lasting
/// omissions survive both cases. The retry window for adopted custody belongs
/// to 1b's batch/receipt ledger and must elapse before this helper is invoked.
pub(in crate::machines::desk_intake) fn cleanup(
    c: &Connection,
    m: &Manifest,
    source: &str,
) -> Result<usize> {
    release(c, m, source, true)
}

fn release(c: &Connection, m: &Manifest, source: &str, adopted: bool) -> Result<usize> {
    let bound = binding(m)?;
    let mut q = c.prepare(
        "SELECT source,intent,binding,journal_order FROM desk_replay_import
        WHERE domain=?1 AND (binding!=?3 OR (source=?2 AND adopted=1 AND ?4))
        ORDER BY journal_order,source LIMIT 50",
    )?;
    let rows = q
        .query_map(params![m.domain, source, bound, adopted], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // Superseded sources may no longer be members of this manifest. Their
    // stale custody must not pin the shared budgets waiting for a contact
    // that can never happen. Current-binding adoption remains source-local.
    for (source, intent, old_binding, order) in &rows {
        if old_binding != &bound {
            storage::omitted_bound(c, m, source, old_binding, *order, "superseded_binding")?;
        }
        c.execute(
            "DELETE FROM desk_replay_witness WHERE domain=?1 AND source=?2 AND intent=?3",
            params![m.domain, source, intent],
        )?;
        c.execute(
            "DELETE FROM desk_replay_import WHERE domain=?1 AND source=?2 AND intent=?3",
            params![m.domain, source, intent],
        )?;
    }
    Ok(rows.len())
}

#[cfg(test)]
mod tests;
