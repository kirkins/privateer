//! Optional, operator-link copy evidence, separate from the mail envelope.
//! Old gateways ignore these top-level fields. Missing, malformed or oversized
//! evidence never changes mail admission/order/receipts; it leaves an unresolved
//! copy. Payload claims are never interpreted as runtime provenance.
use super::*;
use crate::{door::coverage::MailIdentity, mail::source::Provenance};

const BYTES: usize = 16 * 1024;

#[cfg(test)]
#[path = "mail_copies_tests.rs"]
mod tests;

pub(super) fn add(copies: &mut Value, store: &Store, envelope: &Value) {
    if copies.as_object().is_none_or(|o| o.len() >= COLLECT_LIMIT) {
        return;
    }
    let Some(id) = envelope["id"].as_str() else {
        return;
    };
    // Zero optional lock wait, bounded SELECTs only; no migration or writes.
    let conn = store.connection();
    let Ok(timeout) = conn.query_row("PRAGMA busy_timeout", [], |r| r.get::<_, u64>(0)) else {
        return;
    };
    if conn.busy_timeout(Duration::ZERO).is_err() {
        return;
    }
    let provenance = crate::mail::source::copy_provenance(conn, id);
    let _ = conn.busy_timeout(Duration::from_millis(timeout));
    if let Provenance::Copy(origin) = provenance {
        let evidence = json!({"origin":origin,"envelope":crate::core::hash(envelope)});
        // All metadata is optional, including an individually unfit record.
        let current = desk_intake::transport::json_size_bound(copies);
        if current
            .saturating_add(desk_intake::transport::json_size_bound(&evidence))
            .saturating_add(id.len() * 6 + 8)
            <= BYTES
        {
            copies[id] = evidence;
        }
    }
}

pub(super) fn provenance(body: &Value, envelope: &Value) -> Provenance {
    let result = (|| -> Result<Provenance> {
        ensure!(body["mail_copy_v1"] == true, "unsupported copy evidence");
        let copies = &body["mail_copies"];
        ensure!(
            copies.as_object().is_some_and(|o| o.len() <= COLLECT_LIMIT)
                && desk_intake::transport::json_size_bound(copies) <= BYTES,
            "copy bounds"
        );
        let id = required(envelope, "id")?;
        let record = &copies[id];
        ensure!(
            record["envelope"] == crate::core::hash(envelope),
            "copy envelope mismatch"
        );
        let origin: MailIdentity = serde_json::from_value(record["origin"].clone())?;
        MailIdentity::new(&origin.core, &origin.mailbox, &origin.id)?;
        ensure!(origin.id == id, "copy id mismatch");
        Ok(Provenance::Copy(origin))
    })();
    result.unwrap_or(Provenance::Unresolved)
}

/// Direct mail.deliver is explicitly forward-tolerant, including in older
/// binaries. Include metadata only when the ENTIRE old-peer envelope still
/// fits mail's existing limit; otherwise send the original bytes unchanged.
pub(super) fn direct(store: &Store, envelope: &mut Value) {
    let mut copies = json!({});
    add(&mut copies, store, envelope);
    if copies.as_object().is_some_and(|o| !o.is_empty())
        && desk_intake::transport::json_size_bound(envelope)
            .saturating_add(desk_intake::transport::json_size_bound(&copies))
            .saturating_add(64)
            <= crate::mail::MAX_MESSAGE_BYTES
    {
        envelope["mail_copy_v1"] = json!(true);
        envelope["mail_copies"] = copies;
    }
}

pub(super) fn attach(body: &mut Value, copies: Value, limit: usize) {
    if desk_intake::transport::json_size_bound(body).saturating_add(1088) > limit {
        return; // Even the offer is optional at the enclosing byte boundary.
    }
    body["mail_copy_v1"] = json!(true);
    if desk_intake::transport::json_size_bound(body)
        .saturating_add(desk_intake::transport::json_size_bound(&copies))
        .saturating_add(1040)
        <= limit
    {
        body["mail_copies"] = copies;
    }
}
