//! Explicit, local operator boundary for closed legacy observations. A cut is
//! not provenance and never turns an unresolved observation into desk mail.
use super::*;
use crate::machines::desk_placement;
use crate::store::{Caller, required};
use std::{
    path::Path,
    time::{Duration, Instant},
};

const LIMIT: usize = 2000;

pub(super) fn init(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS desk_legacy_cut(
        domain TEXT NOT NULL, source TEXT NOT NULL, digest TEXT NOT NULL, body TEXT NOT NULL,
        PRIMARY KEY(domain,source));
        CREATE TABLE IF NOT EXISTS desk_legacy_pin(
        domain TEXT NOT NULL, source TEXT NOT NULL, mailbox TEXT NOT NULL, id TEXT NOT NULL,
        PRIMARY KEY(domain,source,mailbox,id));",
    )?;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HomeCut {
    pub(super) home: String,
    pub(super) mailbox: Option<String>,
    pub(super) nonce: Option<String>,
    pub(super) mail_upper: Option<i64>,
    count: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Cut {
    binding: String,
    source: String,
    registry_revision: String,
    loss_epoch: i64,
    homes: Vec<HomeCut>,
    pins: Vec<(String, String)>,
}

pub(super) fn load(c: &Connection, m: &Manifest, source: &str) -> Result<Option<(String, Cut)>> {
    let cut = load_raw(c, m, source)?;
    if let Some((_, cut)) = &cut {
        ensure!(
            cut.binding == binding(m)? && cut.source == source,
            "stale legacy cut"
        );
    }
    Ok(cut)
}

fn load_raw(c: &Connection, m: &Manifest, source: &str) -> Result<Option<(String, Cut)>> {
    let raw: Option<(String, String)> = c.query_row(
        "SELECT digest,body FROM desk_legacy_cut WHERE domain=?1 AND source=?2 AND length(CAST(body AS BLOB))<=1048576",
        params![m.domain,source], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
    raw.map(|(digest, raw)| {
        let cut: Cut = serde_json::from_str(&raw)?;
        Ok((digest, cut))
    })
    .transpose()
}

fn epoch(c: &Connection, m: &Manifest, source: &str) -> Result<i64> {
    Ok(c.query_row("SELECT COALESCE((SELECT epoch FROM desk_intake_loss_epoch WHERE domain=?1 AND source=?2),0)", params![m.domain,source], |r|r.get(0))?)
}

pub(super) fn loss_clear(c: &Connection, m: &Manifest, source: &str) -> Result<bool> {
    if let Some((_, cut)) = load(c, m, source)? {
        return Ok(epoch(c, m, source)? == cut.loss_epoch);
    }
    Ok(!c.query_row(
        "SELECT EXISTS(SELECT 1 FROM desk_intake_loss WHERE domain=?1 AND source=?2)",
        params![m.domain, source],
        |r| r.get::<_, bool>(0),
    )?)
}

pub(super) fn home_cut<'a>(cut: Option<&'a Cut>, handle: &str) -> Option<&'a HomeCut> {
    cut?.homes.iter().find(|h| h.home == handle)
}

// Live lifecycle, not the frozen event, guards against reopen/unread. seq is
// the mail AUTOINCREMENT high-water, NOT a count: pruning/re-pinning cannot
// substitute a newer mail row below this boundary.
pub(super) fn blocked(c: &Connection, cut: Option<&HomeCut>) -> Result<bool> {
    Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM mail_source_origin AS origin INDEXED BY mail_source_origin_unresolved
        JOIN mail ON mail.id=origin.id WHERE provenance IN ('\"Unresolved\"','\"Local\"')
        AND (mail.seq>?1 OR mail.state='open' OR
          (mail.recipient='captain' AND mail.seq>COALESCE((SELECT through_seq FROM mail_cursor WHERE address='captain'),0) AND COALESCE(mail.state,'')<>'acknowledged')))",
        [cut.and_then(|h|h.mail_upper).unwrap_or(-1)], |r|r.get(0))?)
}

pub(super) fn unresolved(c: &Connection, m: &Manifest, source: &str) -> Result<bool> {
    Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM desk_home_receipt r WHERE domain=?1 AND source=?2
        AND json_extract(event,'$.provenance') IN ('Unresolved','Local') AND (
          NOT EXISTS(SELECT 1 FROM desk_legacy_pin p WHERE p.domain=r.domain AND p.source=r.source
            AND p.mailbox=json_extract(r.event,'$.mailbox') AND p.id=json_extract(r.event,'$.id'))
          OR json_extract(event,'$.state')='open' OR json_extract(event,'$.unread')<>0 OR json_extract(event,'$.pending')<>0))",
        params![m.domain,source], |r|r.get(0))?)
}

pub(super) fn plan(c: &Connection, m: &Manifest, source: &str) -> Result<Value> {
    participant(m, source)?;
    let local: String = c.query_row(
        "SELECT id FROM door_reader_identity WHERE only=1",
        [],
        |r| r.get(0),
    )?;
    ensure!(local == source, "cut must be planned on its source");
    let deadline = Instant::now() + Duration::from_secs(10);
    let vector = reconcile::inspect_legacy(c, m, source, deadline)?;
    ensure!(
        vector.membership_proved,
        "legacy cut requires proved membership"
    );
    ensure!(
        vector
            .homes
            .iter()
            .all(|h| matches!(h.state.as_str(), "imported" | "released")),
        "legacy cut requires imported or released homes"
    );
    let integrity: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM desk_intake_gap WHERE domain=?1 AND source=?2 AND state='integrity_gap') OR EXISTS(SELECT 1 FROM desk_intake_loss WHERE domain=?1 AND source=?2 AND state='integrity_gap')", params![m.domain,source], |r|r.get(0))?;
    ensure!(!integrity, "integrity_gap cannot be cut");
    let paths = c.prepare("SELECT home FROM (SELECT home FROM ships UNION SELECT home FROM desk_home_inventory) ORDER BY home LIMIT 51")?
        .query_map([], |r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        paths.len() == vector.homes.len() && paths.len() <= 50,
        "legacy home bound"
    );
    let mut homes = Vec::new();
    let mut total = 0;
    for (path, h) in paths.iter().zip(&vector.homes) {
        if h.state == "released" {
            // An absent released home has no observable mailbox/high-water.
            homes.push(HomeCut {
                home: h.home.clone(),
                mailbox: None,
                nonce: None,
                mail_upper: None,
                count: 0,
            });
            continue;
        }
        ensure!(Instant::now() < deadline, "backpressure: legacy plan");
        crate::store::reject_symlinks(Path::new(path))?;
        let db = Path::new(path).join("privateer.sqlite");
        crate::store::reject_symlinks(&db)?;
        let conn = Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let _budget = transport::Budget::new(&conn, deadline)?;
        let read = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Deferred)?;
        let (mailbox,nonce): (String,Option<String>) = read.query_row("SELECT mailbox,(SELECT nonce FROM mail_source_snapshot WHERE only=1) FROM mail_source_health WHERE only=1", [], |r|Ok((r.get(0)?,r.get(1)?)))?;
        ensure!(
            h.mailbox.as_ref() == Some(&mailbox) && h.nonce == nonce,
            "home changed during legacy plan"
        );
        let mail_upper = read.query_row(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name='mail'),0)",
            [],
            |r| r.get(0),
        )?;
        let count: usize = read.query_row("SELECT COUNT(*) FROM (SELECT 1 FROM mail_source_origin o JOIN mail ON mail.id=o.id WHERE provenance IN ('\"Unresolved\"','\"Local\"') LIMIT 2001)", [], |r|r.get(0))?;
        total += count;
        ensure!(total <= LIMIT, "legacy pin bound exceeded");
        let home = HomeCut {
            home: h.home.clone(),
            mailbox: Some(mailbox),
            nonce,
            mail_upper: Some(mail_upper),
            count,
        };
        ensure!(
            !blocked(&read, Some(&home))?,
            "legacy pins must be closed and read"
        );
        homes.push(home);
    }
    let rows = c.prepare("SELECT event FROM desk_home_receipt WHERE domain=?1 AND source=?2 AND json_extract(event,'$.provenance') IN ('Unresolved','Local') ORDER BY intent LIMIT 2001")?
        .query_map(params![m.domain,source], |r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(rows.len() <= LIMIT, "legacy receipt bound exceeded");
    let mut pins = std::collections::BTreeSet::new();
    for raw in rows {
        let event: crate::mail::source::HomeFact = serde_json::from_str(&raw)?;
        ensure!(
            event.state.as_deref() != Some("open") && !event.unread && !event.pending,
            "legacy receipts must be closed, read and not pending"
        );
        pins.insert((event.mailbox, event.id));
    }
    let cut = Cut {
        binding: binding(m)?,
        source: source.into(),
        registry_revision: proof::registry_stamp(c, m, source)?.context("legacy registry bound")?,
        loss_epoch: epoch(c, m, source)?,
        homes,
        pins: pins.into_iter().collect(),
    };
    let body = serde_json::to_value(cut)?;
    Ok(json!({"digest":crate::core::hash(&body),"plan":body}))
}

pub(super) fn apply(
    c: &Connection,
    caller: &Caller,
    m: &Manifest,
    source: &str,
    args: &Value,
) -> Result<Value> {
    caller.require_operator("legacy cut apply")?;
    let reason = reason(args)?;
    let tx = rusqlite::Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
    ensure!(
        load(&tx, m, source)?.is_none(),
        "legacy cut already exists; revoke first"
    );
    let current = plan(&tx, m, source)?;
    ensure!(
        current["digest"].as_str() == Some(required(args, "digest")?),
        "legacy plan digest mismatch"
    );
    let cut: Cut = serde_json::from_value(current["plan"].clone())?;
    tx.execute(
        "INSERT INTO desk_legacy_cut VALUES(?1,?2,?3,?4)",
        params![
            m.domain,
            source,
            required(args, "digest")?,
            current["plan"].to_string()
        ],
    )?;
    for (mailbox, id) in cut.pins {
        tx.execute(
            "INSERT INTO desk_legacy_pin VALUES(?1,?2,?3,?4)",
            params![m.domain, source, mailbox, id],
        )?;
    }
    audit(
        &tx,
        "apply",
        &json!({"cut":current,"reason":reason,"actor":"captain"}),
    )?;
    tx.commit()?;
    Ok(current)
}

fn reason(args: &Value) -> Result<&str> {
    let reason = required(args, "reason")?;
    ensure!(
        !reason.trim().is_empty() && reason.len() <= 4096,
        "bounded reason required"
    );
    Ok(reason)
}
fn audit(c: &Connection, operation: &str, body: &Value) -> Result<()> {
    // Reapplying/revoking the same boundary is a distinct operator decision.
    let now = crate::store::Store::now_ms();
    let body = json!({"decision":body,"at_ms":now,"id":uuid::Uuid::new_v4().to_string()});
    c.execute(
        "INSERT INTO desk_intake_audit(peer,stage,body,at_ms) VALUES('operator',?1,?2,?3)",
        params![
            format!("legacy.{operation}"),
            body.to_string(),
            crate::store::Store::now_ms()
        ],
    )?;
    Ok(())
}
pub(super) fn revoke(
    c: &Connection,
    caller: &Caller,
    m: &Manifest,
    source: &str,
    args: &Value,
) -> Result<Value> {
    caller.require_operator("legacy cut revoke")?;
    let reason = reason(args)?;
    let tx = rusqlite::Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
    // Revocation must remain possible after the placement binding changes.
    let (digest, cut) = load_raw(&tx, m, source)?.context("no legacy cut")?;
    let body = json!({"digest":digest,"cut":cut,"reason":reason,"actor":"captain"});
    tx.execute(
        "DELETE FROM desk_legacy_pin WHERE domain=?1 AND source=?2",
        params![m.domain, source],
    )?;
    tx.execute(
        "DELETE FROM desk_legacy_cut WHERE domain=?1 AND source=?2",
        params![m.domain, source],
    )?;
    audit(&tx, "revoke", &body)?;
    tx.commit()?;
    Ok(body)
}

pub(crate) fn dispatch(caller: &Caller, command: &str, args: &Value) -> Result<Value> {
    let planning = command == "machine.desk.legacy.plan";
    if !planning {
        caller.require_operator(command)?;
    }
    let path = crate::fleet::registry_path()?;
    crate::store::reject_symlinks(&path)?;
    let flags = if planning {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let c = Connection::open_with_flags(path, flags)?;
    c.busy_timeout(Duration::from_secs(10))?;
    let (_fence, m) = if planning {
        desk_placement::intake_manifest_readonly(&c)?
    } else {
        desk_placement::intake_manifest(&c)?
    };
    let source = required(args, "source")?;
    if planning {
        let read = rusqlite::Transaction::new_unchecked(&c, TransactionBehavior::Deferred)?;
        plan(&read, &m, source)
    } else {
        match command {
            "machine.desk.legacy.apply" => apply(&c, caller, &m, source, args),
            "machine.desk.legacy.revoke" => revoke(&c, caller, &m, source, args),
            _ => anyhow::bail!("unknown legacy command"),
        }
    }
}

#[cfg(test)]
mod tests;
