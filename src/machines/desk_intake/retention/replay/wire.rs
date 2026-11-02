//! Replay custody over existing legal contacts. Witness adoption is provisional;
//! `cuts` separately freezes age-checked enable boundaries and classifies.
//! Neither is permission to select mail. Strict coverage/frame wires survive.
use super::*;

pub(in crate::machines::desk_intake) mod cuts;
pub(in crate::machines::desk_intake) mod observation;
pub(in crate::machines::desk_intake) mod returns;
pub(in crate::machines::desk_intake) mod revision;
#[cfg(test)]
pub(in crate::machines::desk_intake) mod tests;
use std::{path::Path, time::Instant};

pub(in crate::machines::desk_intake) const WIRE: usize = 64 * 1024;
const WITNESS_SLOTS: usize = 50;
const WITNESS_SCAN: usize = 100;
const BATCHES: i64 = 32;
const ADOPTED: i64 = 10_000;
const RETRY_CONTACTS: i64 = 2;
pub(in crate::machines::desk_intake) const KEY: &str = "desk_intake_replay_v1";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u8,
    proof: Option<Batch>,
    receipts: Vec<Receipt>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Home {
    home: String,
    mailbox: String,
    nonce: String,
    baseline_upper: i64,
    delta_upper: i64,
    mail_upper: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    evidence: proof::Evidence,
    vector_digest: String,
    homes: Vec<Home>,
    witnesses: Vec<Witness>,
    digest: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Receipt {
    binding: String,
    source: String,
    digest: String,
    adopted: Vec<String>,
    rejected: Vec<Rejection>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Rejection {
    intent: String,
    reason: RejectionReason,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum RejectionReason {
    InvalidWitness,
    MissingOriginal,
    UnknownFirstKind,
    OriginalMismatch,
    ConflictingWitness,
}
impl RejectionReason {
    fn omission(&self) -> &'static str {
        match self {
            Self::InvalidWitness => "rejected_invalid_witness",
            Self::MissingOriginal => "rejected_missing_original",
            Self::UnknownFirstKind => "rejected_unknown_first_kind",
            Self::OriginalMismatch => "rejected_original_mismatch",
            Self::ConflictingWitness => "rejected_conflicting_witness",
        }
    }
}

pub(in crate::machines::desk_intake) fn ready(c: &Connection) -> Result<bool> {
    Ok(!crate::mail::source::missing(
        c,
        &[
            "desk_replay_send",
            "desk_replay_batch",
            "desk_replay_release",
            "desk_replay_adopted",
            "desk_replay_receipt",
            "desk_replay_vector",
            "desk_replay_member",
            "desk_replay_home_cache",
            "desk_replay_link_retry",
            "desk_replay_cut_return",
            "desk_replay_cut_send",
        ],
    )? && returns::ready(c)?)
}

pub(in crate::machines::desk_intake) fn init(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS desk_replay_send(
         domain TEXT NOT NULL, source TEXT NOT NULL, binding TEXT NOT NULL,
         cursor TEXT NOT NULL DEFAULT '', contact INTEGER NOT NULL DEFAULT 0,
         serial INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(domain,source));
         CREATE TABLE IF NOT EXISTS desk_replay_batch(
         domain TEXT NOT NULL, source TEXT NOT NULL, digest TEXT NOT NULL,
         serial INTEGER NOT NULL, body TEXT NOT NULL, binding TEXT NOT NULL,
         PRIMARY KEY(domain,source,digest));
         CREATE TABLE IF NOT EXISTS desk_replay_member(
         domain TEXT NOT NULL, source TEXT NOT NULL, digest TEXT NOT NULL, intent TEXT NOT NULL,
         PRIMARY KEY(domain,source,digest,intent));
         CREATE INDEX IF NOT EXISTS desk_replay_member_intent ON desk_replay_member(domain,source,intent);
         CREATE TRIGGER IF NOT EXISTS desk_replay_batch_delete AFTER DELETE ON desk_replay_batch BEGIN
         DELETE FROM desk_replay_member WHERE domain=old.domain AND source=old.source AND digest=old.digest; END;
         CREATE TABLE IF NOT EXISTS desk_replay_home_cache(
         domain TEXT NOT NULL, source TEXT NOT NULL, binding TEXT NOT NULL,
         home TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY(domain,source,home));
         CREATE TABLE IF NOT EXISTS desk_replay_release(
         domain TEXT NOT NULL, source TEXT NOT NULL, intent TEXT NOT NULL,
         after_contact INTEGER NOT NULL, PRIMARY KEY(domain,source,intent));
         CREATE TABLE IF NOT EXISTS desk_replay_adopted(
         domain TEXT NOT NULL, core TEXT NOT NULL, mailbox TEXT NOT NULL, id TEXT NOT NULL,
         binding TEXT NOT NULL, first_source TEXT NOT NULL, first_order INTEGER NOT NULL,
         first_kind TEXT NOT NULL, body TEXT NOT NULL,
         PRIMARY KEY(domain,core,mailbox,id));
         CREATE INDEX IF NOT EXISTS desk_replay_adopted_retry ON desk_replay_adopted(domain,binding,core,first_kind);
         CREATE TABLE IF NOT EXISTS desk_replay_link_retry(
         domain TEXT NOT NULL, binding TEXT NOT NULL, exposure TEXT NOT NULL,
         source TEXT NOT NULL, cursor INTEGER NOT NULL,
         PRIMARY KEY(domain,exposure,source));
         CREATE TABLE IF NOT EXISTS desk_replay_receipt(
         seq INTEGER PRIMARY KEY AUTOINCREMENT, domain TEXT NOT NULL, source TEXT NOT NULL,
         binding TEXT NOT NULL, digest TEXT NOT NULL, body TEXT,
         UNIQUE(domain,source,digest));
         CREATE TABLE IF NOT EXISTS desk_replay_vector(
         domain TEXT NOT NULL, source TEXT NOT NULL, binding TEXT NOT NULL,
         body TEXT NOT NULL, PRIMARY KEY(domain,source));",
    )?;
    // Upgrade provisional 1b registries. Legacy whole-batch receipts have no
    // subset authority and must be reissued after revalidation.
    if !has_column(c, "desk_replay_batch", "binding")? {
        c.execute_batch(
            "ALTER TABLE desk_replay_batch ADD COLUMN binding TEXT NOT NULL DEFAULT '';
            DELETE FROM desk_replay_batch;",
        )?;
    }
    if !has_column(c, "desk_replay_receipt", "body")? {
        c.execute_batch(
            "ALTER TABLE desk_replay_receipt ADD COLUMN body TEXT;
            DELETE FROM desk_replay_receipt;",
        )?;
    }
    returns::init(c)?;
    Ok(())
}
fn has_column(c: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name=?2)",
        params![table, column],
        |r| r.get(0),
    )?)
}

pub(in crate::machines::desk_intake) fn marker() -> Value {
    json!({"version":1,"proof":null,"receipts":[]})
}

// The private reconstructed frame accepts only bounded, closed v1 objects.
// Malformed optional evidence never changes ordinary page admission.
pub(in crate::machines::desk_intake) fn decode(v: &Value) -> Option<Value> {
    if transport::json_size(v) > WIRE {
        return None;
    }
    let e: Envelope = serde_json::from_value(v.clone()).ok()?;
    (e.version == 1
        && e.receipts.len() <= BATCHES as usize
        && e.receipts
            .iter()
            .all(|r| r.adopted.len() + r.rejected.len() <= WITNESS_SLOTS)
        && e.proof
            .as_ref()
            .is_none_or(|b| b.homes.len() <= 50 && b.witnesses.len() <= WITNESS_SLOTS))
    .then(|| json!(e))
}

fn batch_digest(b: &Batch) -> String {
    crate::core::hash(&json!([b.evidence, b.vector_digest, b.homes, b.witnesses]))
}

// Additional home data has its own wire. Never add M to legacy HomeProof.
// Each tuple is read in one home snapshot and matched to the revalidated v2
// vector. An intervening import/enrollment/head change simply omits replay.
fn homes(
    c: &Connection,
    m: &Manifest,
    source: &str,
    e: &proof::Evidence,
    deadline: Instant,
) -> Result<Vec<Home>> {
    let vector = json!(e)["vector"].clone();
    let expected = vector["homes"]
        .as_array()
        .context("missing replay vector")?;
    let paths = c.prepare("SELECT home FROM (SELECT home FROM ships UNION SELECT home FROM desk_home_inventory) ORDER BY home LIMIT 51")?
        .query_map([], |r| r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        paths.len() == expected.len() && paths.len() <= 50,
        "replay inventory changed"
    );
    let mut result = Vec::new();
    for (path, old) in paths.iter().zip(expected) {
        ensure!(
            crate::mail::source::work_now() < deadline,
            "backpressure: replay homes"
        );
        let handle = crate::core::hash(&json!(Path::new(path)));
        let load = || -> Result<Option<Home>> {
            let raw: Option<String> = c.query_row(
                "SELECT body FROM desk_replay_home_cache WHERE domain=?1 AND source=?2 AND binding=?3 AND home=?4",
                params![m.domain,source,binding(m)?,handle], |r| r.get(0)).optional()?;
            let h: Option<Home> = raw.map(|s| serde_json::from_str(&s)).transpose()?;
            Ok(h.filter(|h| {
                old["home"] == h.home
                    && old["mailbox"] == h.mailbox
                    && old["nonce"] == h.nonce
                    && old["baseline_upper"] == h.baseline_upper
                    && old["delta_upper"] == h.delta_upper
                    && old["state"] == "imported"
            }))
        };
        let mut h = load()?;
        if h.is_none() {
            // Refresh every stale tuple within the shared contact deadline,
            // after page custody. Several busy homes can advance together;
            // a one-refresh limit would withhold their proof every contact.
            let path = Path::new(path);
            crate::store::reject_symlinks(path)?;
            let db = path.join("privateer.sqlite");
            crate::store::reject_symlinks(&db)?;
            let home = Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let _budget = transport::Budget::new(&home, deadline)?;
            cache_home(c, m, source, path, &home)?;
            h = load()?;
        }
        result.push(h.context("replay cached home changed")?);
    }
    ensure!(
        vector["registry_stamp"].as_str() == proof::registry_stamp(c, m, source)?.as_deref(),
        "replay ledger changed"
    );
    Ok(result)
}

// Cache only consistent read-snapshot tuples; exact vector matching above
// makes stale entries inert. Refresh work shares the contact deadline and
// visits at most the 50 homes in the already revalidated vector, never mail rows.
fn cache_home(
    c: &Connection,
    m: &Manifest,
    source: &str,
    path: &Path,
    home: &Connection,
) -> Result<()> {
    crate::store::reject_symlinks(path)?;
    let read = rusqlite::Transaction::new_unchecked(home, TransactionBehavior::Deferred)?;
    let bound = format!("{}:{source}", binding(m)?);
    let nonce = crate::mail::source::enrollment::published_nonce(&read, &bound)?
        .context("unpublished replay home")?;
    let (mailbox, baseline, delta): (String, i64, i64) = read.query_row(
        "SELECT mailbox,(SELECT upper_seq FROM mail_source_snapshot WHERE only=1),
             COALESCE((SELECT seq FROM sqlite_sequence WHERE name='mail_source_outbox'),0)
             FROM mail_source_health WHERE only=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let h = Home {
        home: crate::core::hash(&json!(path)),
        mailbox,
        nonce,
        baseline_upper: baseline,
        delta_upper: delta,
        mail_upper: snapshot::mail_high_water(&read)?.context("unproved replay M")?,
    };
    ensure!(!crate::mail::source::lost(&read)?, "replay home lost");
    c.execute("INSERT INTO desk_replay_home_cache(domain,source,binding,home,body) VALUES(?1,?2,?3,?4,?5)
            ON CONFLICT(domain,source,home) DO UPDATE SET binding=excluded.binding,body=excluded.body
            WHERE binding!=excluded.binding OR body!=excluded.body",
            params![m.domain,source,binding(m)?,h.home,serde_json::to_string(&h)?])?;
    Ok(())
}

// Superseded transport metadata has no positive authority. Release it in
// bounded increments even for removed sources that will never contact again.
// Never touch adopted originals, home marks, or unconfirmed witness custody.
fn retire(c: &Connection, m: &Manifest) -> Result<()> {
    let bound = binding(m)?;
    for table in [
        "desk_replay_receipt",
        "desk_replay_vector",
        "desk_replay_send",
        "desk_replay_home_cache",
        "desk_replay_link_retry",
    ] {
        c.execute(
            &format!(
                "DELETE FROM {table} WHERE rowid IN
            (SELECT rowid FROM {table} WHERE domain=?1 AND binding!=?2 LIMIT 50)"
            ),
            params![m.domain, bound],
        )?;
    }
    c.execute(
        "DELETE FROM desk_replay_batch WHERE rowid IN
        (SELECT rowid FROM desk_replay_batch WHERE domain=?1
         AND binding!=?2 LIMIT 50)",
        params![m.domain, bound],
    )?;
    c.execute("DELETE FROM desk_replay_release WHERE rowid IN
        (SELECT r.rowid FROM desk_replay_release r LEFT JOIN desk_replay_import i USING(domain,source,intent)
         WHERE r.domain=?1 AND (i.binding IS NULL OR i.binding!=?2) LIMIT 50)",params![m.domain,bound])?;
    Ok(())
}

fn send_state(c: &Connection, m: &Manifest, source: &str) -> Result<(String, i64)> {
    c.execute(
        "INSERT INTO desk_replay_send(domain,source,binding) VALUES(?1,?2,?3)
        ON CONFLICT(domain,source) DO UPDATE SET binding=excluded.binding,cursor=''
        WHERE binding!=excluded.binding",
        params![m.domain, source, binding(m)?],
    )?;
    Ok(c.query_row(
        "SELECT cursor,contact FROM desk_replay_send WHERE domain=?1 AND source=?2",
        params![m.domain, source],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

pub(in crate::machines::desk_intake) fn outgoing(
    c: &Connection,
    m: &Manifest,
    source: &str,
    owner: bool,
    evidence: Option<&proof::Evidence>,
    deadline: Instant,
) -> Result<Value> {
    retire(c, m)?;
    let mut e = Envelope {
        version: 1,
        proof: None,
        receipts: Vec::new(),
    };
    if owner {
        e.receipts = c
            .prepare(
                "SELECT body FROM desk_replay_receipt
            WHERE domain=?1 AND source=?2 AND binding=?3 ORDER BY seq DESC LIMIT 32",
            )?
            .query_map(params![m.domain, source, binding(m)?], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .map(|s| serde_json::from_str(s))
            .collect::<serde_json::Result<_>>()?;
    } else if let Some(evidence) = evidence.filter(|e| e.is_valid()) {
        let homes = homes(c, m, source, evidence, deadline)?;
        let (cursor, _) = send_state(c, m, source)?;
        // All witness slots/bytes serve unadopted custody (stronger than the
        // half-share requirement). A persistent rotating keyset, not a newest
        // tail query, prevents starvation across batches/contacts.
        let mut witnesses = Vec::new();
        let mut scanned = 0;
        let mut scanned_cursor = cursor.clone();
        for sql in [
            "SELECT w.intent,w.body FROM desk_replay_witness w JOIN desk_replay_import i USING(domain,source,intent)
             WHERE w.domain=?1 AND w.source=?2 AND i.binding=?3 AND w.adopted=0 AND w.intent>?4 AND i.journal_order<=?5 ORDER BY w.intent LIMIT ?6",
            "SELECT w.intent,w.body FROM desk_replay_witness w JOIN desk_replay_import i USING(domain,source,intent)
             WHERE w.domain=?1 AND w.source=?2 AND i.binding=?3 AND w.adopted=0 AND w.intent<=?4 AND i.journal_order<=?5 ORDER BY w.intent LIMIT ?6",
        ] {
            if witnesses.len() == WITNESS_SLOTS || scanned == WITNESS_SCAN {
                break;
            }
            let upper = json!(evidence)["checkpoint"]["upper"].as_i64().context("missing checkpoint upper")?;
            let raw = c.prepare(sql)?.query_map(params![m.domain,source,binding(m)?,cursor,upper,WITNESS_SCAN - scanned], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (intent, raw) in raw {
                if witnesses.len() == WITNESS_SLOTS {
                    break;
                }
                let witness = serde_json::from_str::<Witness>(&raw)?;
                scanned += 1;
                scanned_cursor = intent.clone();
                if witness.intent != intent || candidate(c, m, source, &intent)?.as_ref() != Some(&witness) {
                    transport::audit_in_budget(c, source, "replay.witness", &anyhow::anyhow!(
                        "integrity_gap: replay witness differs from committed import ledger: {}", witness.intent
                    ));
                    continue;
                }
                witnesses.push(witness);
            }
        }
        let mut b = Batch {
            evidence: evidence.clone(),
            vector_digest: crate::core::hash(&json!(evidence)["vector"]),
            homes,
            witnesses,
            digest: String::new(),
        };
        loop {
            b.digest = batch_digest(&b);
            e.proof = Some(b.clone());
            if transport::json_size(&json!(e)) <= WIRE {
                break;
            }
            if b.witnesses.pop().is_none() {
                return Ok(marker());
            }
        }
        if b.witnesses.is_empty() {
            // Even an entirely unusable bounded window must make progress.
            // The next contact can reach valid custody behind it.
            c.execute("UPDATE desk_replay_send SET cursor=?3 WHERE domain=?1 AND source=?2 AND cursor<>?3",
                params![m.domain,source,scanned_cursor])?;
            return Ok(json!(e));
        }
        // Custody must precede send. Batch eviction never deletes witnesses;
        // a late receipt for an evicted batch is ignored, then proof retries.
        let tx = rusqlite::Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
        // Small queues rotate their starting entry too; a permanently bad
        // witness cannot lock subsequent contacts to one batch revision.
        let next = if b.witnesses.len() == WITNESS_SLOTS {
            // Leave one overlapping entry so even an exactly-full queue
            // changes its batch ordering/digest on the following contact.
            b.witnesses.get(WITNESS_SLOTS - 2)
        } else {
            b.witnesses.first()
        };
        let cursor = next.map(|w| w.intent.as_str()).unwrap_or(&cursor);
        tx.execute(
            "UPDATE desk_replay_send SET cursor=?3,serial=serial+1 WHERE domain=?1 AND source=?2",
            params![m.domain, source, cursor],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO desk_replay_batch(domain,source,digest,serial,body,binding)
            SELECT ?1,?2,?3,serial,?4,?5 FROM desk_replay_send WHERE domain=?1 AND source=?2",
            params![
                m.domain,
                source,
                b.digest,
                serde_json::to_string(&b)?,
                binding(m)?
            ],
        )?;
        for w in &b.witnesses {
            tx.execute(
                "INSERT OR IGNORE INTO desk_replay_member VALUES(?1,?2,?3,?4)",
                params![m.domain, source, b.digest, w.intent],
            )?;
        }
        tx.execute("DELETE FROM desk_replay_batch WHERE domain=?1 AND source=?2 AND digest NOT IN
            (SELECT digest FROM desk_replay_batch WHERE domain=?1 AND source=?2 ORDER BY serial DESC LIMIT 32)", params![m.domain,source])?;
        tx.commit()?;
    }
    while transport::json_size(&json!(e)) > WIRE && !e.receipts.is_empty() {
        e.receipts.pop();
    }
    Ok(json!(e))
}

fn valid_batch(c: &Connection, m: &Manifest, source: &str, b: &Batch) -> Result<bool> {
    if b.digest != batch_digest(b)
        || b.vector_digest != crate::core::hash(&json!(b.evidence)["vector"])
        || !proof::eligible(c, m, source, &b.evidence)?
    {
        return Ok(false);
    }
    let evidence = json!(b.evidence);
    let expected = evidence["vector"]["homes"]
        .as_array()
        .context("missing replay homes")?;
    if expected.len() != b.homes.len() {
        return Ok(false);
    }
    for (h, old) in b.homes.iter().zip(expected) {
        if old["home"] != h.home
            || old["mailbox"] != h.mailbox
            || old["nonce"] != h.nonce
            || old["baseline_upper"] != h.baseline_upper
            || old["delta_upper"] != h.delta_upper
            || h.mail_upper < 0
            || h.baseline_upper < 0
            || h.delta_upper < h.baseline_upper
        {
            return Ok(false);
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    Ok(b.witnesses.iter().all(|w| seen.insert(&w.intent)))
}

fn rejection(
    c: &Connection,
    m: &Manifest,
    source: &str,
    b: &Batch,
    w: &Witness,
) -> Result<Option<RejectionReason>> {
    let evidence = json!(b.evidence);
    if w.binding != binding(m)?
        || w.source != source
        || w.origin.core != source
        || w.origin.mailbox != w.mailbox
        || w.intent.is_empty()
        || w.journal_order <= 0
        || Some(w.journal_order) > evidence["checkpoint"]["upper"].as_i64()
        || !b.homes.iter().any(|h| {
            h.home == w.home
                && h.mailbox == w.mailbox
                && h.nonce == w.nonce
                && w.delta_order > h.baseline_upper
                && w.delta_order <= h.delta_upper
                && w.mail_order > 0
                && w.mail_order <= h.mail_upper
        })
    {
        return Ok(Some(RejectionReason::InvalidWitness));
    }
    let row: Option<(String,Option<String>,i64,Option<String>)> = c.query_row(
            "SELECT f.content,o.source,o.first_order,o.first_kind FROM desk_intake_original o
             JOIN desk_intake_fact f USING(domain,core,mailbox,id)
             WHERE o.domain=?1 AND o.core=?2 AND o.mailbox=?3 AND o.id=?4
             AND o.owner_core=?5 AND o.generation=?6 AND o.incarnation=?7
             AND f.owner_core=o.owner_core AND f.generation=o.generation AND f.incarnation=o.incarnation",
            params![m.domain,w.origin.core,w.origin.mailbox,w.origin.id,m.owner_core,m.generation,m.incarnation],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let Some((content, first_source, first_order, kind)) = row else {
        return Ok(Some(RejectionReason::MissingOriginal));
    };
    if !matches!(kind.as_deref(), Some("native" | "copy")) {
        return Ok(Some(RejectionReason::UnknownFirstKind));
    }
    if crate::core::hash(&serde_json::from_str::<Value>(&content)?) != w.digest
        || first_source.is_none()
        || (kind.as_deref() == Some("native")
            && (first_source.as_deref() != Some(source) || first_order != w.journal_order))
        || coverage::origin_of(c, &w.origin)? != w.origin
    {
        return Ok(Some(RejectionReason::OriginalMismatch));
    }
    Ok(None)
}

fn receive_batch(c: &Connection, m: &Manifest, source: &str, b: Batch) -> Result<()> {
    let tx = rusqlite::Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
    ensure!(
        valid_batch(&tx, m, source, &b)?,
        "invalid provisional replay batch"
    );
    // A receipt is an immutable decision about this exact batch, including
    // rejected intents. Replays cannot silently change that decision.
    if tx.query_row("SELECT EXISTS(SELECT 1 FROM desk_replay_receipt WHERE domain=?1 AND source=?2 AND digest=?3)",
        params![m.domain,source,b.digest], |r| r.get::<_,bool>(0))? {
        return Ok(());
    }
    let used: i64 = tx.query_row("SELECT COUNT(*) FROM desk_replay_adopted", [], |r| r.get(0))?;
    ensure!(
        b.witnesses.is_empty() || used < ADOPTED,
        "backpressure: adopted replay budget"
    );
    let mut receipt = Receipt {
        binding: binding(m)?,
        source: source.into(),
        digest: b.digest.clone(),
        adopted: Vec::new(),
        rejected: Vec::new(),
    };
    let mut valid = Vec::new();
    let mut additional = 0;
    let mut origins = std::collections::BTreeSet::new();
    for w in &b.witnesses {
        let mut why = rejection(&tx, m, source, &b, w)?;
        let origin = (&w.origin.core, &w.origin.mailbox, &w.origin.id);
        if why.is_none() && origins.contains(&origin) {
            // Even copy-first adoption retains only one witness per original.
            why = Some(RejectionReason::ConflictingWitness);
        }
        if why.is_none() {
            let old: Option<String> = tx.query_row("SELECT body FROM desk_replay_adopted WHERE domain=?1 AND core=?2 AND mailbox=?3 AND id=?4",
                params![m.domain,w.origin.core,w.origin.mailbox,w.origin.id],|r|r.get(0)).optional()?;
            match old {
                Some(old) if old != serde_json::to_string(w)? => {
                    why = Some(RejectionReason::ConflictingWitness)
                }
                None => additional += 1,
                _ => (),
            }
        }
        if let Some(reason) = why {
            receipt.rejected.push(Rejection {
                intent: w.intent.clone(),
                reason,
            });
        } else {
            // Reserve only after stored-body validation. A rejected peer must
            // not poison a later valid witness for this same original.
            origins.insert(origin);
            receipt.adopted.push(w.intent.clone());
            valid.push(w);
        }
    }
    ensure!(
        used + additional <= ADOPTED,
        "backpressure: adopted replay budget"
    );
    for w in valid {
        revision::write(&tx, || {
            Ok(tx.execute("INSERT OR IGNORE INTO desk_replay_adopted(domain,core,mailbox,id,binding,first_source,first_order,first_kind,body)
            SELECT domain,core,mailbox,id,?5,source,first_order,first_kind,?6 FROM desk_intake_original
            WHERE domain=?1 AND core=?2 AND mailbox=?3 AND id=?4",
            params![m.domain,w.origin.core,w.origin.mailbox,w.origin.id,binding(m)?,serde_json::to_string(w)?])?)
        })?;
    }
    revision::write(&tx, || {
        Ok(tx.execute(
            "INSERT INTO desk_replay_vector(domain,source,binding,body) VALUES(?1,?2,?3,?4)
        ON CONFLICT(domain,source) DO UPDATE SET binding=excluded.binding,body=excluded.body
        WHERE binding!=excluded.binding OR body!=excluded.body",
            params![m.domain, source, binding(m)?, serde_json::to_string(&b)?],
        )?)
    })?;
    tx.execute_batch("SAVEPOINT replay_cuts")?;
    if let Err(error) = cuts::contact(&tx, m, source, &b) {
        tx.execute_batch("ROLLBACK TO replay_cuts")?;
        transport::audit_in_budget(&tx, source, "replay.cuts", &error);
    }
    tx.execute_batch("RELEASE replay_cuts")?;
    if b.witnesses.is_empty() {
        tx.commit()?;
        return Ok(());
    }
    tx.execute("INSERT OR IGNORE INTO desk_replay_receipt(domain,source,binding,digest,body) VALUES(?1,?2,?3,?4,?5)",params![m.domain,source,binding(m)?,b.digest,serde_json::to_string(&receipt)?])?;
    tx.execute("DELETE FROM desk_replay_receipt WHERE domain=?1 AND source=?2 AND seq NOT IN
        (SELECT seq FROM desk_replay_receipt WHERE domain=?1 AND source=?2 ORDER BY seq DESC LIMIT 32)",params![m.domain,source])?;
    tx.commit()?;
    Ok(())
}

fn receive_receipts(
    c: &Connection,
    m: &Manifest,
    source: &str,
    receipts: &[Receipt],
) -> Result<()> {
    let releases: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM desk_replay_release WHERE domain=?1 AND source=?2)",
        params![m.domain, source],
        |r| r.get(0),
    )?;
    let mut known = false;
    for r in receipts {
        ensure!(
            r.binding == binding(m)? && r.source == source,
            "stale replay receipt"
        );
        known |= c.query_row("SELECT EXISTS(SELECT 1 FROM desk_replay_batch WHERE domain=?1 AND source=?2 AND digest=?3)",params![m.domain,source,r.digest],|r|r.get::<_,bool>(0))?;
    }
    if !known && !releases {
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
    let (_, contact) = send_state(&tx, m, source)?;
    tx.execute(
        "UPDATE desk_replay_send SET contact=contact+1 WHERE domain=?1 AND source=?2",
        params![m.domain, source],
    )?;
    for r in receipts {
        ensure!(
            r.binding == binding(m)? && r.source == source,
            "stale replay receipt"
        );
        let raw: Option<String> = tx
            .query_row(
                "SELECT body FROM desk_replay_batch WHERE domain=?1 AND source=?2 AND digest=?3",
                params![m.domain, source, r.digest],
                |r| r.get(0),
            )
            .optional()?;
        let Some(raw) = raw else { continue };
        let b: Batch = serde_json::from_str(&raw)?;
        ensure!(
            r.digest == batch_digest(&b),
            "invalid replay receipt digest"
        );
        let mut seen = std::collections::BTreeSet::new();
        for (intent, why) in r
            .adopted
            .iter()
            .map(|i| (i, None))
            .chain(r.rejected.iter().map(|r| (&r.intent, Some(&r.reason))))
        {
            ensure!(seen.insert(intent), "duplicate replay receipt intent");
            let w = b
                .witnesses
                .iter()
                .find(|w| &w.intent == intent)
                .context("unknown replay receipt intent")?;
            // Confirmed negative decisions are disclosed before releasing
            // detail. The lasting home M mark prevents lifecycle re-mints.
            adopt(&tx, m, w)?;
            if let Some(why) = why {
                storage::omitted_bound(
                    &tx,
                    m,
                    source,
                    &w.binding,
                    w.journal_order,
                    why.omission(),
                )?;
            }
            tx.execute("INSERT OR IGNORE INTO desk_replay_release(domain,source,intent,after_contact) VALUES(?1,?2,?3,?4)",
                params![m.domain,source,w.intent,contact.saturating_add(RETRY_CONTACTS)])?;
        }
        ensure!(seen.len() == b.witnesses.len(), "incomplete replay receipt");
        // Receipt replay after cleanup is harmless; deleting the batch does
        // not remove any unconfirmed witness from custody.
        tx.execute(
            "DELETE FROM desk_replay_batch WHERE domain=?1 AND source=?2 AND digest=?3",
            params![m.domain, source, r.digest],
        )?;
    }
    let release = tx.prepare("SELECT intent FROM desk_replay_release WHERE domain=?1 AND source=?2 AND after_contact<=?3 ORDER BY intent LIMIT 50")?
        .query_map(params![m.domain,source,contact],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for intent in release {
        tx.execute(
            "DELETE FROM desk_replay_batch WHERE domain=?1 AND source=?2 AND digest IN
            (SELECT digest FROM desk_replay_member WHERE domain=?1 AND source=?2 AND intent=?3)",
            params![m.domain, source, intent],
        )?;
        release_current(&tx, m, source, true, Some(&intent))?;
        tx.execute(
            "DELETE FROM desk_replay_release WHERE domain=?1 AND source=?2 AND intent=?3",
            params![m.domain, source, intent],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub(in crate::machines::desk_intake) fn receive(
    c: &Connection,
    m: &Manifest,
    source: &str,
    owner: bool,
    wire: &Value,
) -> Result<()> {
    let Some(wire) = decode(wire) else {
        return Ok(());
    };
    let e: Envelope = serde_json::from_value(wire)?;
    if owner {
        ensure!(
            e.receipts.is_empty(),
            "source sent replay adoption receipts"
        );
        if let Some(b) = e.proof {
            receive_batch(c, m, source, b)?;
        }
    } else {
        ensure!(e.proof.is_none(), "desk sent replay source proof");
        receive_receipts(c, m, source, &e.receipts)?;
    }
    Ok(())
}
