//! Door-only captain report reads. No operator report.get or linked fallback.
use super::scoped::DelegatedContext;
use crate::store::{Caller, Store, identifier, required};
use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

pub(super) fn read(args: &Value, context: &DelegatedContext) -> Result<Value> {
    let id = required(args, "report_id")?;
    identifier(id)?;
    for (ship, home) in super::members()? {
        if !context.allows(&ship) {
            continue;
        }
        let store = Store::open_readonly(&home, Caller::Operator)?;
        // A stored report alone is not publication. Require immutable incoming
        // mail in this same home, from the stored author to the captain. Never
        // trust a foreign report reference or a path in a mail payload.
        let found = store
            .connection()
            .query_row(
                "SELECT r.author,r.title,r.format,r.content,r.created_ms FROM reports r
             WHERE r.id=?1 AND length(CAST(r.content AS BLOB)) BETWEEN 1 AND 262144
             AND EXISTS(SELECT 1 FROM mail m WHERE m.recipient=?2 AND m.sender=r.author
               AND json_extract(m.payload,'$.report.report_id')=r.id)",
                params![id, crate::mail::CAPTAIN],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        if let Some((author, title, format, content, created_ms)) =
            found.filter(|r| context.allows(&r.0))
        {
            return Ok(
                json!({"report_id":id,"ship":author,"title":title,"format":format,
                "bytes":content.len(),"content":content,"created_ms":created_ms,
                "coverage":{"scope":"local-active-homes","remote":"unavailable"}}),
            );
        }
    }
    None::<Value>.context("report not found")
}
