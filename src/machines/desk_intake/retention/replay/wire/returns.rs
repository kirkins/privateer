//! Desk-authored immutable cut return, separate from the closed A/B1 wire.
//! Legal-contact admission belongs to transport. Rebuild runs only on idle.
use super::*;

#[cfg(test)]
mod tests;
pub(in crate::machines::desk_intake) const KEY: &str = "desk_intake_replay_cuts_v1";
pub(in crate::machines::desk_intake) const WIRE: usize = 16 * 1024;
const RECEIPTS: usize = 8;
const RECEIPT_BYTES: usize = 2048;
const RECORDS: i64 = 10_000;
const CANDIDATES: usize = 50;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u8,
    receipts: Vec<Receipt>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    binding: String,
    source: String,
    exposure: String,
    cut_digest: String,
    checkpoint: Checkpoint,
    vector_digest: String,
    home: Home,
}
impl Receipt {
    fn digest(&self) -> String {
        crate::core::hash(&json!([
            self.exposure,
            self.checkpoint,
            self.vector_digest,
            self.home
        ]))
    }
    fn cut(&self) -> Cut {
        Cut {
            checkpoint_digest: self.cut_digest.clone(),
            binding: self.binding.clone(),
            source: self.source.clone(),
            home: self.home.home.clone(),
            mailbox: self.home.mailbox.clone(),
            nonce: self.home.nonce.clone(),
            journal_upper: self.checkpoint.upper,
            delta_upper: self.home.delta_upper,
            mail_upper: self.home.mail_upper,
        }
    }
    fn key(&self) -> (String, String, String, String) {
        (
            self.exposure.clone(),
            self.home.home.clone(),
            self.home.mailbox.clone(),
            self.home.nonce.clone(),
        )
    }
}
pub(super) fn init(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS desk_replay_cut_send(
         domain TEXT NOT NULL,source TEXT NOT NULL,binding TEXT NOT NULL,cursor TEXT NOT NULL,
         PRIMARY KEY(domain,source));
         CREATE TABLE IF NOT EXISTS desk_replay_cut_return(
         seq INTEGER PRIMARY KEY AUTOINCREMENT,domain TEXT NOT NULL,source TEXT NOT NULL,
         binding TEXT NOT NULL,exposure TEXT NOT NULL,home TEXT NOT NULL,mailbox TEXT NOT NULL,
         nonce TEXT NOT NULL,body TEXT NOT NULL,cursor INTEGER NOT NULL,turn INTEGER NOT NULL DEFAULT 0,
         conflict INTEGER NOT NULL DEFAULT 0,
         UNIQUE(domain,source,binding,exposure,home,mailbox,nonce));
         CREATE INDEX IF NOT EXISTS desk_replay_cut_work ON desk_replay_cut_return(domain,source,binding,conflict,turn,seq);
         CREATE INDEX IF NOT EXISTS desk_replay_rebuild ON desk_replay_import(domain,source,binding,home,mailbox,nonce,journal_order);
         INSERT INTO desk_replay_budget SELECT 'desk_replay_cut_return',COUNT(*) FROM desk_replay_cut_return WHERE true
         ON CONFLICT(name) DO UPDATE SET used=excluded.used;
         CREATE TRIGGER IF NOT EXISTS desk_replay_cut_return_insert AFTER INSERT ON desk_replay_cut_return BEGIN
         UPDATE desk_replay_budget SET used=used+1 WHERE name='desk_replay_cut_return'; END;",
    )?;
    Ok(())
}
pub(in crate::machines::desk_intake) fn marker() -> Value {
    json!({"version":1,"receipts":[]})
}
pub(in crate::machines::desk_intake) fn decode(v: &Value) -> Option<Value> {
    if transport::json_size(v) > WIRE {
        return None;
    }
    let e: Envelope = serde_json::from_value(v.clone()).ok()?;
    (e.version == 1
        && e.receipts.len() <= RECEIPTS
        && e.receipts
            .iter()
            .all(|r| transport::json_size(&json!(r)) <= RECEIPT_BYTES))
    .then(|| json!(e))
}

// Only the bounded current exposure list is enumerated. Each cut query is a
// primary-key range within ONE exposure/source, never a scan of historic cuts.
pub(in crate::machines::desk_intake) fn outgoing(
    c: &Connection,
    m: &Manifest,
    source: &str,
    owner: bool,
) -> Result<Value> {
    if !owner || crate::mail::source::missing(c, &["door_desk_exposure", "desk_exposure_cut"])? {
        return Ok(marker());
    }
    let raw = c
        .prepare("SELECT binding FROM door_desk_exposure ORDER BY door LIMIT 51")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(raw.len() <= 50, "backpressure: cut return bindings");
    let mut keys = std::collections::BTreeSet::new();
    for raw in raw {
        let b: Value = serde_json::from_str(&raw)?;
        if cuts::matches_binding(&b, m)? {
            keys.insert(crate::core::hash(&b));
        }
    }
    let bound = binding(m)?;
    let raw: Option<String> = c
        .query_row(
            "SELECT cursor FROM desk_replay_cut_send WHERE domain=?1 AND source=?2 AND binding=?3",
            params![m.domain, source, bound],
            |r| r.get(0),
        )
        .optional()?;
    let cursor: (String, String, String, String) = raw
        .map(|s| serde_json::from_str(&s))
        .transpose()?
        .unwrap_or_default();
    let mut e = Envelope {
        version: 1,
        receipts: Vec::new(),
    };
    let mut next = cursor.clone();
    for wrap in [false, true] {
        for key in &keys {
            if (!wrap && key < &cursor.0) || (wrap && key > &cursor.0) {
                continue;
            }
            let lower = !wrap && key == &cursor.0;
            let upper = wrap && key == &cursor.0;
            let range = if lower {
                "AND (home,mailbox,nonce)>(?3,?4,?5)"
            } else if upper {
                "AND (home,mailbox,nonce)<=(?3,?4,?5)"
            } else {
                ""
            };
            let sql = format!(
                "SELECT body FROM desk_exposure_cut WHERE binding=?1 AND source=?2 {range} ORDER BY home,mailbox,nonce LIMIT ?6"
            );
            let rows = c
                .prepare(&sql)?
                .query_map(
                    params![
                        key,
                        source,
                        cursor.1,
                        cursor.2,
                        cursor.3,
                        RECEIPTS - e.receipts.len()
                    ],
                    |r| r.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for raw in rows {
                let cut: cuts::SavedCut = serde_json::from_str(&raw)?;
                let mut r = Receipt {
                    binding: bound.clone(),
                    source: source.into(),
                    exposure: key.clone(),
                    cut_digest: String::new(),
                    checkpoint: cut.checkpoint,
                    vector_digest: cut.vector_digest,
                    home: cut.home,
                };
                r.cut_digest = r.digest();
                next = r.key();
                ensure!(
                    transport::json_size(&json!(r)) <= RECEIPT_BYTES,
                    "backpressure: cut receipt bytes"
                );
                e.receipts.push(r);
            }
            if e.receipts.len() == RECEIPTS {
                break;
            }
        }
        if e.receipts.len() == RECEIPTS {
            break;
        }
    }
    if next != cursor {
        c.execute("INSERT INTO desk_replay_cut_send VALUES(?1,?2,?3,?4)
            ON CONFLICT(domain,source) DO UPDATE SET binding=excluded.binding,cursor=excluded.cursor",
            params![m.domain,source,bound,serde_json::to_string(&next)?])?;
    }
    let v = json!(e);
    ensure!(
        transport::json_size(&v) <= WIRE,
        "backpressure: cut return bytes"
    );
    Ok(v)
}

fn home_path(c: &Connection, handle: &str) -> Result<std::path::PathBuf> {
    let paths = c.prepare("SELECT home FROM (SELECT home FROM ships UNION SELECT home FROM desk_home_inventory) ORDER BY home LIMIT 51")?
        .query_map([],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(paths.len() <= 50, "backpressure: cut home inventory");
    paths
        .into_iter()
        .map(std::path::PathBuf::from)
        .find(|p| crate::core::hash(&json!(p)) == handle)
        .context("cut return home not in source inventory")
}
fn open_home(path: &Path) -> Result<Connection> {
    crate::store::reject_symlinks(path)?;
    let db = path.join("privateer.sqlite");
    crate::store::reject_symlinks(&db)?;
    Ok(Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
}
// The source accepts desk assertions only after CURRENT legal-contact fencing,
// then rechecks its own home snapshot. A local checkpoint is never substituted.
fn validate(
    c: &Connection,
    m: &Manifest,
    source: &str,
    r: &Receipt,
    deadline: Instant,
) -> Result<()> {
    ensure!(
        r.binding == binding(m)?
            && r.source == source
            && r.checkpoint.binding == r.binding
            && r.checkpoint.source == source
            && r.cut_digest == r.digest()
            && r.exposure.len() == 64
            && r.exposure.bytes().all(|b| b.is_ascii_hexdigit())
            && r.vector_digest.len() == 64
            && r.vector_digest.bytes().all(|b| b.is_ascii_hexdigit())
            && r.home.baseline_upper >= 0
            && r.home.delta_upper >= r.home.baseline_upper
            && r.home.mail_upper >= 0
            && r.checkpoint.upper >= 0,
        "invalid returned cut binding/digest"
    );
    participant(m, source)?;
    uuid::Uuid::parse_str(&r.checkpoint.id)?;
    ensure!(
        r.checkpoint.upper <= retention::head(c, m, source)?,
        "returned cut ahead of source J"
    );
    let path = home_path(c, &r.home.home)?;
    let home = open_home(&path)?;
    let _budget = transport::Budget::new(&home, deadline)?;
    let read = rusqlite::Transaction::new_unchecked(&home, TransactionBehavior::Deferred)?;
    ensure!(!crate::mail::source::lost(&read)?, "returned cut home lost");
    let nonce = crate::mail::source::enrollment::published_nonce(
        &read,
        &format!("{}:{source}", r.binding),
    )?;
    let (mailbox,baseline,delta): (String,i64,i64) = read.query_row(
        "SELECT mailbox,(SELECT upper_seq FROM mail_source_snapshot WHERE only=1),COALESCE((SELECT seq FROM sqlite_sequence WHERE name='mail_source_outbox'),0) FROM mail_source_health WHERE only=1",
        [],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
    ensure!(
        mailbox == r.home.mailbox
            && nonce.as_deref() == Some(&r.home.nonce)
            && baseline == r.home.baseline_upper
            && delta >= r.home.delta_upper
            && snapshot::mail_high_water(&read)?.is_some_and(|v| v >= r.home.mail_upper),
        "returned cut home/nonce/head mismatch"
    );
    // Cached sent tuples remain useful after ordinary checkpoint reclamation.
    // Only an exact checkpoint match can constrain that cut's vector digest.
    let raw: Option<String> = c
        .query_row(
            "SELECT body FROM desk_source_proof WHERE domain=?1 AND source=?2 AND barrier=?3",
            params![m.domain, source, r.checkpoint.id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(raw) = raw {
        let evidence: proof::Evidence = serde_json::from_str(&raw)?;
        ensure!(
            evidence.checkpoint == r.checkpoint
                && crate::core::hash(&json!(evidence)["vector"]) == r.vector_digest,
            "returned cut vector mismatch"
        );
    }
    Ok(())
}

pub(in crate::machines::desk_intake) fn receive(
    c: &Connection,
    m: &Manifest,
    source: &str,
    owner: bool,
    v: &Value,
    deadline: Instant,
) -> Result<()> {
    let wire = decode(v).context("malformed cut return envelope")?;
    let e: Envelope = serde_json::from_value(wire)?;
    ensure!(!owner || e.receipts.is_empty(), "source sent desk cuts");
    for r in e.receipts {
        let body = serde_json::to_string(&r)?;
        let old: Option<String> = c.query_row("SELECT body FROM desk_replay_cut_return WHERE domain=?1 AND source=?2 AND binding=?3 AND exposure=?4 AND home=?5 AND mailbox=?6 AND nonce=?7",
            params![m.domain,source,binding(m)?,r.exposure,r.home.home,r.home.mailbox,r.home.nonce],|row|row.get(0)).optional()?;
        // Idempotent resend does not reopen homes, write or enqueue work.
        if old.as_ref() == Some(&body) {
            continue;
        }
        if let Err(error) = validate(c, m, source, &r, deadline) {
            transport::audit_in_budget(c, source, "replay.cut_return", &error);
            continue;
        }
        if old.is_some() {
            c.execute("UPDATE desk_replay_cut_return SET conflict=1 WHERE domain=?1 AND source=?2 AND binding=?3 AND exposure=?4 AND home=?5 AND mailbox=?6 AND nonce=?7 AND conflict=0",
                params![m.domain,source,binding(m)?,r.exposure,r.home.home,r.home.mailbox,r.home.nonce])?;
            transport::audit_in_budget(
                c,
                source,
                "replay.cut_return",
                &anyhow::anyhow!("conflicting immutable returned cut"),
            );
            continue;
        }
        let tx = rusqlite::Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
        ensure!(
            used(&tx, "desk_replay_cut_return")? < RECORDS,
            "backpressure: returned cut records"
        );
        tx.execute("INSERT OR IGNORE INTO desk_replay_cut_return(domain,source,binding,exposure,home,mailbox,nonce,body,cursor) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![m.domain,source,r.binding,r.exposure,r.home.home,r.home.mailbox,r.home.nonce,body,r.checkpoint.upper])?;
        tx.commit()?;
    }
    Ok(())
}

// One cut/home per idle unit; at most 50 indexed candidate visits. The durable
// least-served order gives every accepted cut a turn even with continuous tail
// imports. Witness pressure does not advance past the blocked candidate.
pub(in crate::machines::desk_intake) fn step(
    c: &Connection,
    m: &Manifest,
    source: &str,
    deadline: Instant,
) -> Result<usize> {
    let row: Option<(i64,String,i64)> = c.query_row("SELECT seq,body,cursor FROM desk_replay_cut_return WHERE domain=?1 AND source=?2 AND binding=?3 AND conflict=0 ORDER BY turn,seq LIMIT 1",
        params![m.domain,source,binding(m)?],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((seq, body, cursor)) = row else {
        return Ok(0);
    };
    let r: Receipt = serde_json::from_str(&body)?;
    let cut = r.cut();
    let rows = c.prepare("SELECT intent,journal_order FROM desk_replay_import INDEXED BY desk_replay_rebuild WHERE domain=?1 AND source=?2 AND binding=?3 AND home=?4 AND mailbox=?5 AND nonce=?6 AND journal_order>?7 ORDER BY journal_order LIMIT ?8")?
        .query_map(params![m.domain,source,r.binding,r.home.home,r.home.mailbox,r.home.nonce,cursor,CANDIDATES],|row|Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let path = home_path(c, &r.home.home);
    let home = path.as_ref().ok().and_then(|p| open_home(p).ok());
    let _budget = home
        .as_ref()
        .map(|h| transport::Budget::new(h, deadline))
        .transpose()?;
    let read = home
        .as_ref()
        .map(|h| rusqlite::Transaction::new_unchecked(h, TransactionBehavior::Deferred))
        .transpose()?;
    let tx = rusqlite::Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
    let mut next = cursor;
    let mut visited = 0;
    for (intent, order) in rows {
        ensure!(
            crate::mail::source::work_now() < deadline,
            "backpressure: rebuild time budget"
        );
        let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM desk_replay_witness WHERE domain=?1 AND source=?2 AND intent=?3)",params![m.domain,source,intent],|row|row.get(0))?;
        visited += 1;
        if !exists {
            if used(&tx, "desk_replay_witness")? >= WITNESSES {
                break;
            }
            if let (Ok(path), Some(home)) = (&path, &read) {
                if let Err(error) = rebuild(path, &tx, home, m, source, &intent, &cut) {
                    if retention::interrupted_or_busy(&error) {
                        return Err(error);
                    }
                    omitted(&tx, m, source, order, "rebuild_unavailable")?;
                    transport::audit_in_budget(&tx, source, "replay.rebuild", &error);
                }
            } else {
                omitted(&tx, m, source, order, "rebuild_unavailable")?;
            }
        }
        next = order;
    }
    tx.execute("UPDATE desk_replay_cut_return SET cursor=?2,turn=(SELECT COALESCE(MAX(turn),0)+1 FROM desk_replay_cut_return WHERE domain=?3 AND source=?4 AND binding=?5 AND conflict=0) WHERE seq=?1",
        params![seq,next,m.domain,source,binding(m)?])?;
    tx.commit()?;
    Ok(visited)
}
