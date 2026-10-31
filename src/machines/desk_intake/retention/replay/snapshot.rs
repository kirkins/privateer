//! Read-only home snapshot checks. No schema changes or mutable projection
//! reconstruction can manufacture a witness for an already-receipted import.
use super::*;
use crate::mail::source;

pub(in crate::machines::desk_intake) fn mail_high_water(c: &Connection) -> Result<Option<i64>> {
    let sql: Option<String> = c
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='mail'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if !sql.is_some_and(|s| s.to_ascii_uppercase().contains("AUTOINCREMENT")) {
        return Ok(None);
    }
    let seq_is_key: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('mail') WHERE name='seq' AND upper(type)='INTEGER' AND pk=1)",[],|r|r.get(0))?;
    if !seq_is_key {
        return Ok(None);
    }
    let sequence: Option<i64> = c
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name='mail'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let max: Option<i64> = c.query_row("SELECT MAX(seq) FROM mail", [], |r| r.get(0))?;
    Ok(match (sequence, max) {
        (Some(s), max) if s >= 0 && max.is_none_or(|m| m <= s) => Some(s),
        (None, None) => Some(0),
        _ => None,
    })
}

fn home_handle(c: &Connection) -> Result<String> {
    let path: String = c.query_row(
        "SELECT file FROM pragma_database_list WHERE name='main'",
        [],
        |r| r.get(0),
    )?;
    let parent = std::path::Path::new(&path)
        .parent()
        .context("unreadable: home path")?;
    Ok(crate::core::hash(&json!(parent)))
}

fn current_content(c: &Connection, id: &str) -> Result<Option<Value>> {
    let row = c
        .query_row(
            &format!(
                "SELECT {} FROM mail WHERE id=?1 AND (sender='captain' OR recipient='captain')",
                crate::mail::COLUMNS
            ),
            [id],
            crate::mail::message,
        )
        .optional()?;
    Ok(row.map(|mut row| {
        for key in [
            "seq",
            "state",
            "answered_by",
            "answered_ms",
            "ended_by",
            "ended_ms",
            "end_reason",
            "delivery",
            "delivery_attempts",
            "delivery_error",
            "delivered_ms",
            "remote_end_state",
        ] {
            row.as_object_mut().unwrap().remove(key);
        }
        row
    }))
}

pub(super) fn native_now(
    c: &Connection,
    m: &Manifest,
    source: &str,
    origin: &MailIdentity,
    digest: &str,
) -> Result<Option<(String, String, i64)>> {
    if origin.core != source || source::lost(c)? || mail_high_water(c)?.is_none() {
        return Ok(None);
    }
    let Some(nonce) = source::enrollment::published_nonce(c, &format!("{}:{source}", binding(m)?))?
    else {
        return Ok(None);
    };
    let mailbox: String = c.query_row(
        "SELECT mailbox FROM mail_source_health WHERE only=1",
        [],
        |r| r.get(0),
    )?;
    if mailbox != origin.mailbox {
        return Ok(None);
    }
    let pin: Option<(String, String)> = c
        .query_row(
            "SELECT provenance,content FROM mail_source_origin WHERE id=?1",
            [&origin.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((provenance, content)) = pin else {
        return Ok(None);
    };
    if serde_json::from_str::<Provenance>(&provenance)? != Provenance::Origin(origin.clone())
        || crate::core::hash(&serde_json::from_str::<Value>(&content)?) != digest
        || current_content(c, &origin.id)?
            .as_ref()
            .map(crate::core::hash)
            .as_deref()
            != Some(digest)
    {
        return Ok(None);
    }
    let order: Option<i64> = c
        .query_row("SELECT seq FROM mail WHERE id=?1", [&origin.id], |r| {
            r.get(0)
        })
        .optional()?;
    order
        .map(|order| Ok((home_handle(c)?, nonce, order)))
        .transpose()
}

/// Caller holds a deferred home read transaction until registry commit. That
/// snapshot binds pending intent, stable outbox D, enrollment and insertion M.
/// Baseline sidecars have no mail/outbox and can only supply negative evidence.
pub(in crate::machines::desk_intake) fn observe(
    c: &Connection,
    m: &Manifest,
    source: &str,
    intent: &str,
    event: &str,
) -> Result<Observation> {
    let saved: String = c.query_row(
        "SELECT event FROM mail_source_import WHERE intent=?1 AND receipt IS NULL",
        [intent],
        |r| r.get(0),
    )?;
    ensure!(saved == event, "integrity_gap: pending intent changed");
    let f: HomeFact = serde_json::from_str(event)?;
    let bound = format!("{}:{source}", binding(m)?);
    let (saved_binding, upper, nonce): (String, i64, Option<String>) = c.query_row(
        "SELECT binding,upper_seq,nonce FROM mail_source_snapshot WHERE only=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    ensure!(saved_binding == bound && upper >= 0, "stale_generation");
    let has_mail = !source::missing(c, &["mail"])?;
    let mut o = Observation {
        home: home_handle(c)?,
        mailbox: f.mailbox.clone(),
        nonce,
        lane: if has_mail { "unknown" } else { "baseline" }.into(),
        delta_order: None,
        mail_order: None,
        native: false,
    };
    if !has_mail {
        return Ok(o);
    }
    let outbox: Option<(i64, String)> = c
        .query_row(
            "SELECT seq,event FROM mail_source_outbox WHERE intent=?1",
            [intent],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((d, saved)) = outbox {
        ensure!(saved == event, "integrity_gap: outbox intent changed");
        if d > upper {
            o.lane = "delta".into();
            o.delta_order = Some(d);
        }
    } else if o.nonce.is_none() {
        // Inline 4a baselines have no instance nonce, hence no positive proof.
        o.lane = "baseline".into();
    }
    if let Provenance::Origin(origin) = &f.provenance
        && origin == &MailIdentity::new(source, &f.mailbox, &f.id)?
        && let Some((home, nonce, mail_order)) =
            native_now(c, m, source, origin, &crate::core::hash(&f.content))?
        && home == o.home
        && Some(&nonce) == o.nonce.as_ref()
    {
        o.mail_order = Some(mail_order);
        o.native = o.lane == "delta";
    }
    Ok(o)
}
