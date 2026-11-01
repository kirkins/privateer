//! Delivery custody is independent of a pane's UI status or lease heartbeats.
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

pub(super) fn alert(conn: &rusqlite::Connection, now: i64) -> Result<Vec<Value>> {
    conn.execute(
        "DELETE FROM hosted_delivery_alerts WHERE NOT EXISTS
         (SELECT 1 FROM presence p WHERE p.ship=hosted_delivery_alerts.ship
          AND p.holder=hosted_delivery_alerts.holder
          AND COALESCE(p.holder_since_ms,p.started_ms)=hosted_delivery_alerts.holder_since_ms
          AND p.expires_ms>?1 AND p.holder LIKE 'host:%')",
        [now],
    )?;
    let leases = conn
        .prepare(
            "SELECT p.ship,p.holder,COALESCE(p.holder_since_ms,p.started_ms),
          MAX(p.presented_through,COALESCE(c.through_seq,0),COALESCE(d.accepted,0)),
          CASE WHEN MAX(p.presented_through,COALESCE(c.through_seq,0))>COALESCE(d.accepted,0)
            THEN 0 ELSE COALESCE(d.declines,0) END
         FROM presence p JOIN ships s ON s.name=p.ship
         LEFT JOIN mail_cursor c ON c.address=p.ship
         LEFT JOIN hosted_delivery d ON d.ship=p.ship AND d.holder=p.holder
          AND d.holder_since_ms=COALESCE(p.holder_since_ms,p.started_ms)
         WHERE p.expires_ms>?1 AND p.holder LIKE 'host:%'
          AND json_extract(s.record,'$.location')='here'
          AND json_extract(s.record,'$.first_mate.host')='herdr'",
        )?
        .query_map([now], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut alerts = Vec::new();
    for (ship, holder, custody, progress, declines) in leases {
        let oldest: Option<i64> = conn.query_row(
            &format!(
                "SELECT MIN(created_ms) FROM mail WHERE recipient=?1
              AND COALESCE(delivery,'received')='received' AND seq>?2 AND NOT ({})",
                crate::mail::ROUTINE_MAIL
            ),
            params![ship, progress],
            |r| r.get(0),
        )?;
        let state: Option<(i64,i64,bool)> = conn.query_row(
            "SELECT progress,blocked_since_ms,alerted FROM hosted_delivery_alerts WHERE ship=?1",
            [&ship], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
        ).optional()?;
        let Some(oldest) = oldest else {
            conn.execute("DELETE FROM hosted_delivery_alerts WHERE ship=?1", [&ship])?;
            continue;
        };
        let (since, alerted) = match state {
            Some((previous, since, alerted)) if previous == progress => (since, alerted),
            _ => {
                // New custody waits at most N from its start. Actual progress
                // rearms the bound even if another older arrival remains.
                let since = if state.is_some() {
                    now
                } else {
                    oldest.max(custody)
                };
                conn.execute(
                    "INSERT OR REPLACE INTO hosted_delivery_alerts(ship,holder,holder_since_ms,progress,blocked_since_ms,alerted)
                     VALUES(?1,?2,?3,?4,?5,0)", params![ship,holder,custody,progress,since],
                )?;
                (since, false)
            }
        };
        if !alerted
            && (declines >= 3 || now.saturating_sub(since) >= super::BLOCKED_ALERT_AFTER_MS)
            && let Some(task) = super::duty_task(conn, &ship)?
        {
            crate::mail::record_crew(
                conn,
                &json!({
                    "id":format!("hosted-delivery-{ship}-{holder}-{custody}-{progress}-{since}"),
                    "to":"captain", "kind":"notice", "task_id":task, "urgency":"normal",
                    "body":format!("{ship}'s live hosted session is not accepting actionable mail. Inspect its delivery diagnostics and answer in its pane; a live lease is not delivery evidence."),
                    "payload":{"hosted_blocked":ship,"delivery_blocked":true,"blocked_since_ms":since,"decline_count":declines}
                }),
            )?;
            conn.execute(
                "UPDATE hosted_delivery_alerts SET alerted=1 WHERE ship=?1",
                [&ship],
            )?;
            alerts.push(json!({"ship":ship,"alerted":true,"delivery_blocked":true}));
        }
    }
    Ok(alerts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn deterministic_custody_progress_and_retry_bound() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let conn = store.connection();
        conn.execute(
            "INSERT INTO ships(name,record) VALUES('alpha',?1)",
            [
                json!({"location":"here","first_mate":{"host":"herdr","duty_task":"alpha-duty"}})
                    .to_string(),
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO presence(ship,holder,started_ms,holder_since_ms,expires_ms)
            VALUES('alpha','host:alpha',1000,1000,99999999)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms)
            VALUES('alpha-mail','captain','alpha','request','action','{}','normal',2000)",
            [],
        )
        .unwrap();
        assert!(alert(conn, 2000).unwrap().is_empty());
        assert!(alert(conn, 301999).unwrap().is_empty());
        assert_eq!(alert(conn, 302000).unwrap().len(), 1);
        conn.execute("UPDATE presence SET expires_ms=199999999", [])
            .unwrap();
        assert!(
            alert(conn, 999999).unwrap().is_empty(),
            "renewal is not progress"
        );
        conn.execute("UPDATE presence SET presented_through=1", [])
            .unwrap();
        assert!(alert(conn, 1000000).unwrap().is_empty());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM hosted_delivery_alerts", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
        conn.execute(
            "INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms)
            VALUES('beta-mail','captain','alpha','request','next','{}','normal',1000000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO hosted_delivery(ship,holder,holder_since_ms,accepted,declines)
            VALUES('alpha','host:alpha',1000,1,3)",
            [],
        )
        .unwrap();
        assert_eq!(
            alert(conn, 1000000).unwrap().len(),
            1,
            "K declines bypass N"
        );
        conn.execute(
            "UPDATE presence SET holder='host:beta',holder_since_ms=1000001",
            [],
        )
        .unwrap();
        assert!(
            alert(conn, 1000001).unwrap().is_empty(),
            "old holder's decline cannot alert"
        );
        assert_eq!(
            alert(conn, 1300001).unwrap().len(),
            1,
            "new holder gets its own bound"
        );
        conn.execute("UPDATE presence SET expires_ms=1300001", [])
            .unwrap();
        assert!(alert(conn, 1300001).unwrap().is_empty());
    }
}
