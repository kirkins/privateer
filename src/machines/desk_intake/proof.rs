//! Optional barrier-bound census sidecar. This is internal evidence, never a
//! completion claim. Old peers see exactly the old strict intake frame. A
//! missing/invalid sidecar invalidates eligibility, not mail or page custody.
use super::*;
use std::time::Instant;

const HOMES: usize = 50;
pub(super) const WIRE: usize = 64 * 1024;
const VERSION: u8 = 2;

// Mail delivery deliberately records a local created_ms. It is not the
// origin's immutable creation time; never use it to reject or overwrite the
// origin. Compare all other content fields exactly, retaining the origin's
// full timestamp and lifecycle in desk_intake_fact. Version 1 hashed the local
// timestamp too and cannot reconcile genuine copies after delivery.
fn copy_content(content: &Value) -> String {
    let mut content = content.clone();
    if let Some(fields) = content.as_object_mut() {
        fields.remove("created_ms");
    }
    crate::core::hash(&content)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct CopyEvidence {
    origin: MailIdentity,
    alias: MailIdentity,
    content: String,
}

pub(super) fn copies(
    conn: &Connection,
    m: &Manifest,
    source: &str,
) -> Result<(Vec<CopyEvidence>, bool)> {
    let mut stmt = conn.prepare(
        "SELECT event FROM desk_home_copy WHERE domain=?1 AND source=?2 ORDER BY intent LIMIT 51",
    )?;
    let rows = stmt.query_map(params![m.domain, source], |r| r.get::<_, String>(0))?;
    let mut copies = Vec::new();
    for row in rows {
        let f: crate::mail::source::HomeFact = serde_json::from_str(&row?)?;
        let origin = match f.provenance {
            crate::mail::source::Provenance::Copy(origin)
            | crate::mail::source::Provenance::Origin(origin) => origin,
            _ => anyhow::bail!("integrity_gap: invalid retained copy"),
        };
        copies.push(CopyEvidence {
            origin,
            alias: MailIdentity::new(source, &f.mailbox, &f.id)?,
            content: copy_content(&f.content),
        });
    }
    let complete = copies.len() <= HOMES;
    copies.truncate(HOMES);
    Ok((copies, complete))
}

// All mutable registry predicates are covered, including imports which append
// only copy evidence rather than advancing the source event head. Retained copy
// rows and unresolved receipts are pinned; their IDs cannot be reclaimed/reused.
// Topology is hashed locally; paths/names never become wire provenance.
pub(super) fn registry_stamp(
    conn: &Connection,
    m: &Manifest,
    source: &str,
) -> Result<Option<String>> {
    let mut parts = Vec::new();
    for sql in [
        "SELECT json_array(home,retired_ms IS NULL,name) FROM ships ORDER BY home,name LIMIT 51",
        "SELECT json_array(home) FROM desk_home_inventory ORDER BY home LIMIT 51",
        "SELECT json_array(m.name,p.core) FROM machines m LEFT JOIN desk_placement_peer p ON p.machine=m.name ORDER BY m.name LIMIT 51",
        "SELECT json_array(machine,name) FROM machine_ships ORDER BY machine,name LIMIT 51",
    ] {
        parts.push(
            conn.prepare(sql)?
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }
    if parts.iter().any(|rows| rows.len() > HOMES) {
        return Ok(None);
    }
    let predicates: String = conn.query_row(
        "SELECT json_array(
        (SELECT revision FROM desk_source_revision WHERE domain=?1 AND source=?2),
        (SELECT state FROM desk_intake_gap WHERE domain=?1 AND source=?2),
        (SELECT state FROM desk_intake_loss WHERE domain=?1 AND source=?2),
        EXISTS(SELECT 1 FROM desk_home_inventory_gap))",
        params![m.domain, source],
        |r| r.get(0),
    )?;
    Ok(Some(crate::core::hash(&json!([parts, predicates]))))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct Evidence {
    version: u8,
    pub(super) checkpoint: Checkpoint,
    vector: reconcile::Proof,
    // Rechecked at each source contact; not persisted in the immutable issue.
    valid: bool,
}

impl Evidence {
    pub(super) fn is_valid(&self) -> bool {
        self.valid
    }
}

// Exactly the same admission predicate for issuance and quiet-change detection:
// the envelope, not just the saved vector, must fit the sidecar wire budget.
fn candidate(conn: &Connection, m: &Manifest, cp: &Checkpoint) -> Result<Option<Evidence>> {
    let Some(vector) = reconcile::load(conn, m, &cp.source)? else {
        return Ok(None);
    };
    if !vector.candidate || !reconcile::bound(&vector, m, &cp.source, cp.upper)? {
        return Ok(None);
    }
    let e = Evidence {
        version: VERSION,
        checkpoint: cp.clone(),
        vector,
        valid: true,
    };
    Ok((serde_json::to_vec(&e)?.len() <= WIRE).then_some(e))
}

fn sub_deadline(deadline: Instant) -> Option<Instant> {
    let now = crate::mail::source::work_now();
    let remaining = deadline.saturating_duration_since(now);
    (remaining >= std::time::Duration::from_millis(2)).then(|| now + remaining / 2)
}

pub(super) fn issue(
    conn: &Connection,
    m: &Manifest,
    cp: &Checkpoint,
    deadline: Instant,
) -> Result<()> {
    // Reserve at least half the remaining contact for pages/receipts; skip
    // altogether when less than 2 ms is available. No proof home I/O occurs
    // under the issuance transaction. This is cooperative, not hard real time.
    let Some(deadline) = sub_deadline(deadline) else {
        return Ok(());
    };
    let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM desk_source_proof WHERE domain=?1 AND source=?2 AND barrier=?3)", params![m.domain,cp.source,cp.id], |r|r.get(0))?;
    if exists {
        return Ok(());
    }
    let Some(e) = candidate(conn, m, cp)? else {
        return Ok(());
    };
    let vector = crate::core::hash(&serde_json::to_value(&e.vector)?);
    let old: Option<(String, usize)> = conn
        .query_row(
            "SELECT vector,next_home FROM desk_source_proof_work WHERE domain=?1 AND source=?2",
            params![m.domain, cp.source],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let start = old
        .as_ref()
        .filter(|(key, _)| key == &vector)
        .map_or(0, |(_, next)| *next);
    let (done, next) = reconcile::revalidate_step(conn, m, &cp.source, &e.vector, deadline, start)?;
    if done {
        conn.execute(
            "INSERT INTO desk_source_proof(domain,source,barrier,body) VALUES(?1,?2,?3,?4)",
            params![m.domain, cp.source, cp.id, serde_json::to_string(&e)?],
        )?;
        conn.execute(
            "DELETE FROM desk_source_proof_work WHERE domain=?1 AND source=?2",
            params![m.domain, cp.source],
        )?;
    } else if old.as_ref() != Some(&(vector.clone(), next)) {
        conn.execute("INSERT INTO desk_source_proof_work(domain,source,vector,next_home) VALUES(?1,?2,?3,?4) ON CONFLICT(domain,source) DO UPDATE SET vector=excluded.vector,next_home=excluded.next_home",params![m.domain,cp.source,vector,next])?;
    }
    Ok(())
}

pub(super) fn changed(
    conn: &Connection,
    m: &Manifest,
    cp: &Checkpoint,
    deadline: Instant,
) -> Result<bool> {
    let Some(deadline) = sub_deadline(deadline) else {
        return Ok(false);
    };
    let Some(current) = candidate(conn, m, cp)? else {
        return Ok(false);
    };
    let old: Option<String> = conn
        .query_row(
            "SELECT body FROM desk_source_proof WHERE domain=?1 AND source=?2 AND barrier=?3",
            params![m.domain, cp.source, cp.id],
            |r| r.get(0),
        )
        .optional()?;
    let old = old
        .map(|raw| serde_json::from_str::<Evidence>(&raw))
        .transpose()?;
    Ok(
        old.is_none_or(|old| old.version != VERSION || old.vector != current.vector)
            && reconcile::revalidate(conn, m, &cp.source, &current.vector, deadline)?,
    )
}

pub(super) fn outgoing(
    conn: &Connection,
    m: &Manifest,
    cp: &Checkpoint,
    deadline: Instant,
) -> Result<Option<Evidence>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT body FROM desk_source_proof WHERE domain=?1 AND source=?2 AND barrier=?3",
            params![m.domain, cp.source, cp.id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(raw) = raw else { return Ok(None) };
    let mut e: Evidence = serde_json::from_str(&raw)?;
    e.valid = e.checkpoint == *cp
        && sub_deadline(deadline).is_some_and(|deadline| {
            reconcile::revalidate(conn, m, &cp.source, &e.vector, deadline).unwrap_or(false)
        });
    Ok(Some(e))
}

// Copy observations can establish aliases only. Lifecycle/unread/pending always
// come from the authoritative origin source. Never close or acknowledge a fact
// using a desk copy, even when its immutable content agrees.
fn copies_proved(conn: &Connection, m: &Manifest, e: &Evidence) -> Result<bool> {
    for copy in &e.vector.copies {
        if MailIdentity::new(&copy.origin.core, &copy.origin.mailbox, &copy.origin.id).is_err()
            || MailIdentity::new(&copy.alias.core, &copy.alias.mailbox, &copy.alias.id).is_err()
        {
            return Ok(false);
        }
        if copy.alias.core != e.checkpoint.source
            || !m.sources.iter().any(|s| s.core == copy.origin.core)
        {
            return Ok(false);
        }
        let content: Option<String> = conn.query_row("SELECT content FROM desk_intake_fact WHERE domain=?1 AND core=?2 AND mailbox=?3 AND id=?4 AND owner_core=?5 AND generation=?6 AND incarnation=?7",params![m.domain,copy.origin.core,copy.origin.mailbox,copy.origin.id,m.owner_core,m.generation,m.incarnation],|r|r.get(0)).optional()?;
        let Some(content) = content else {
            return Ok(false);
        };
        if copy_content(&serde_json::from_str::<Value>(&content)?) != copy.content
            || coverage::origin_of(conn, &copy.origin)? != copy.origin
        {
            return Ok(false);
        }
        // Unknown aliases remain in the proof for later reader integration.
        // Even trusted evidence cannot relabel an already observed origin.
        let alias = coverage::origin_of(conn, &copy.alias)?;
        if alias != copy.alias && alias != copy.origin {
            return Ok(false);
        }
        let was_origin: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM desk_intake_fact WHERE domain=?1 AND core=?2 AND mailbox=?3 AND id=?4)",params![m.domain,copy.alias.core,copy.alias.mailbox,copy.alias.id],|r|r.get(0))?;
        if was_origin && copy.alias != copy.origin {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn eligible(
    conn: &Connection,
    m: &Manifest,
    source: &str,
    e: &Evidence,
) -> Result<bool> {
    if e.version != VERSION
        || !e.valid
        || e.checkpoint.source != source
        || e.checkpoint.binding != binding(m)?
        || !reconcile::bound(&e.vector, m, source, e.checkpoint.upper)?
        || !e.vector.candidate
        || e.vector.copies.len() > HOMES
        || serde_json::to_vec(e)?.len() > WIRE
    {
        return Ok(false);
    }
    let done: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM desk_intake_checkpoint c JOIN desk_intake_order o USING(domain,source) WHERE c.domain=?1 AND c.source=?2 AND c.binding=?3 AND c.barrier=?4 AND c.upper_seq=?5 AND c.through_seq=c.upper_seq AND o.barrier=c.barrier AND o.issued=?6)",params![m.domain,source,e.checkpoint.binding,e.checkpoint.id,e.checkpoint.upper,e.checkpoint.issued],|r|r.get(0))?;
    let loss: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM desk_intake_loss WHERE domain=?1 AND source=?2)",
        params![m.domain, source],
        |r| r.get(0),
    )?;
    Ok(done && !loss && copies_proved(conn, m, e)?)
}

pub(super) fn receive(
    conn: &Connection,
    m: &Manifest,
    source: &str,
    evidence: Option<Evidence>,
) -> Result<()> {
    // Missing/malformed/legacy evidence removes eligibility without deleting
    // the last dated vector (including unavailable-home unread/open counts).
    // Persist the incoming validity claim, but derive eligibility on each read
    // so later origin imports/conflicts and incomplete barriers are rechecked.
    let evidence = evidence.filter(|e| {
        e.version == VERSION
            && e.checkpoint.source == source
            && e.checkpoint.binding == binding(m).unwrap_or_default()
    });
    let old: Option<(Option<String>, bool)> = conn
        .query_row(
            "SELECT body,valid FROM desk_intake_proof WHERE domain=?1 AND source=?2",
            params![m.domain, source],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let mut valid = evidence.as_ref().is_some_and(|e| e.valid);
    let body = if let Some(e) = &evidence {
        let latest: Option<(i64, String)> = conn
            .query_row(
                "SELECT issued,barrier FROM desk_intake_order WHERE domain=?1 AND source=?2",
                params![m.domain, source],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if latest != Some((e.checkpoint.issued, e.checkpoint.id.clone())) {
            valid = false;
            old.as_ref().and_then(|(body, _)| body.clone())
        } else {
            if let Some((Some(raw), _)) = &old {
                let previous: Evidence = serde_json::from_str(raw)?;
                if previous.checkpoint == e.checkpoint && previous.vector != e.vector {
                    gap(conn, m, source, "integrity_gap")?;
                    valid = false;
                }
            }
            Some(serde_json::to_string(e)?)
        }
    } else {
        old.as_ref().and_then(|(body, _)| body.clone())
    };
    if old.as_ref() != Some(&(body.clone(), valid)) {
        conn.execute("INSERT INTO desk_intake_proof(domain,source,body,valid) VALUES(?1,?2,?3,?4) ON CONFLICT(domain,source) DO UPDATE SET body=excluded.body,valid=excluded.valid",params![m.domain,source,body,valid])?;
    }
    Ok(())
}

pub(super) fn retained_homes(
    conn: &Connection,
    m: &Manifest,
    source: &str,
) -> Result<Option<Value>> {
    let raw: Option<String> = conn.query_row("SELECT body FROM desk_intake_proof WHERE domain=?1 AND source=?2 AND length(CAST(body AS BLOB))<=65536", params![m.domain,source], |r|r.get(0)).optional()?.flatten();
    raw.map(|raw| Ok(serde_json::from_str::<Value>(&raw)?["vector"]["homes"].clone()))
        .transpose()
}

pub(super) enum LocalCurrent {
    Current,
    Changed,
    Budget,
}

pub(super) fn local_current(
    conn: &Connection,
    m: &Manifest,
    source: &str,
    deadline: Instant,
) -> Result<LocalCurrent> {
    let raw: Option<String> = conn.query_row("SELECT body FROM desk_intake_proof WHERE domain=?1 AND source=?2 AND valid=1 AND length(CAST(body AS BLOB))<=65536", params![m.domain,source], |r|r.get(0)).optional()?.flatten();
    let Some(raw) = raw else {
        return Ok(LocalCurrent::Changed);
    };
    let e: Evidence = serde_json::from_str(&raw)?;
    let Some(deadline) = sub_deadline(deadline) else {
        return Ok(LocalCurrent::Budget);
    };
    let result = reconcile::revalidate(conn, m, source, &e.vector, deadline);
    if crate::mail::source::work_now() >= deadline {
        return Ok(LocalCurrent::Budget);
    }
    result.map(|current| {
        if current {
            LocalCurrent::Current
        } else {
            LocalCurrent::Changed
        }
    })
}

pub(super) fn proved(conn: &Connection, m: &Manifest, source: &str) -> Result<bool> {
    let body: Option<String> = conn
        .query_row(
            "SELECT body FROM desk_intake_proof WHERE domain=?1 AND source=?2 AND valid=1",
            params![m.domain, source],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    body.map(|body| eligible(conn, m, source, &serde_json::from_str(&body)?))
        .unwrap_or(Ok(false))
}

// Pull the optional sidecar back into our private frame representation. Never
// put it on the legacy strict Frame wire, even when talking to a new peer.
pub(in crate::machines) fn with_sidecar(frame: &Value, sidecar: &Value, siblings: &Value) -> Value {
    // Borrow only the named sidecars, never decode the whole collect body.
    let replay = &siblings[retention::replay::wire::KEY];
    let cuts = &siblings[retention::replay::wire::returns::KEY];
    let mut frame = frame.clone();
    if frame.is_object()
        && frame.get("status").is_none()
        && !sidecar.is_null()
        && serde_json::to_vec(sidecar).is_ok_and(|v| v.len() <= WIRE)
    {
        // Malformed optional evidence must not reject a valid intake page.
        if let Ok(e) = serde_json::from_value::<Evidence>(sidecar.clone()) {
            frame["proof"] = json!(e);
        }
    }
    if frame.is_object()
        && frame.get("status").is_none()
        && let Some(replay) = retention::replay::wire::decode(replay)
    {
        frame["replay"] = replay;
    }
    if frame.is_object()
        && frame.get("status").is_none()
        && let Some(cuts) = retention::replay::wire::returns::decode(cuts)
    {
        frame["cut_returns"] = cuts;
    }
    frame
}

#[cfg(test)]
mod tests;
