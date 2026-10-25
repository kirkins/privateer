//! Door-only captain report reads. No operator report.get or linked fallback.
use super::scoped::DelegatedContext;
use crate::store::{Caller, Store, identifier, required};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

fn query_budget(conn: &Connection, deadline: Instant) -> Result<()> {
    conn.busy_timeout(Duration::from_millis(25))?;
    // Bound SQLite VM work cooperatively, not filesystem I/O or a single JSON
    // function invocation. All allowed homes share the same call deadline.
    conn.progress_handler(1000, Some(move || Instant::now() >= deadline));
    Ok(())
}

pub(super) fn read(args: &Value, context: &DelegatedContext) -> Result<Value> {
    let id = required(args, "report_id")?;
    identifier(id)?;
    let deadline = Instant::now() + Duration::from_millis(250);
    for (ship, home) in super::members()? {
        if !context.allows(&ship) {
            continue;
        }
        ensure!(Instant::now() < deadline, "report query budget exhausted");
        let store = Store::open_readonly(&home, Caller::Operator)?;
        query_budget(store.connection(), deadline)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_scan_budget_interrupts_work_and_keeps_small_reads() {
        let conn = Connection::open_in_memory().unwrap();
        query_budget(&conn, Instant::now() + Duration::from_secs(10)).unwrap();
        assert_eq!(
            conn.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        query_budget(&conn, Instant::now() - Duration::from_secs(1)).unwrap();
        let error = conn
            .query_row(
                "WITH RECURSIVE scan(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM scan WHERE n<10000)
                 SELECT sum(n) FROM scan",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_err();
        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::OperationInterrupted)
        );
        let busy_ms: u64 = conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(busy_ms, 25);
    }
}
