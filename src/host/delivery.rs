//! Delivery custody is independent of a pane's UI status or lease heartbeats.
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

const MAX_BUSY_TURN_MS: i64 = 30 * 60 * 1000;

pub(super) fn alert(store: &crate::store::Store, now: i64) -> Result<Vec<Value>> {
    let conn = store.connection();
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
          MAX(p.presented_through,COALESCE(c.through_seq,0),COALESCE(h.accepted,0)),
          CASE WHEN MAX(p.presented_through,COALESCE(c.through_seq,0))>COALESCE(d.accepted,0)
            THEN 0 ELSE COALESCE(d.declines,0) END, d.busy_since_ms,
          p.asleep_holder IS p.holder
         FROM presence p JOIN ships s ON s.name=p.ship
         LEFT JOIN mail_cursor c ON c.address=p.ship
         LEFT JOIN hosted_delivery h ON h.ship=p.ship
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
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, bool>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut alerts = Vec::new();
    for (ship, holder, custody, progress, declines, busy_since, asleep) in leases {
        if asleep
            && super::sleep::alarm(
                store,
                &ship,
                &super::sleep::Asleep {
                    holder: holder.clone(),
                    through_seq: progress,
                },
            )?
            .is_some()
        {
            // The wrapper, not an absent harness, owns the next action. A wake
            // due is not acceptance evidence, and must not be called a stall.
            conn.execute("DELETE FROM hosted_delivery_alerts WHERE ship=?1", [&ship])?;
            continue;
        }
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
        // Mail cannot be presented mid-turn. A fresh busy turn holds the
        // spell timer, but one continuous turn has its own hard upper bound.
        let busy_age = busy_since
            .filter(|_| !asleep)
            .map(|start| now.saturating_sub(start));
        let declines = if asleep { 0 } else { declines };
        if busy_age.is_some_and(|age| age < MAX_BUSY_TURN_MS) {
            continue;
        }
        if !alerted
            && (declines >= 3
                || busy_age.is_some_and(|age| age >= MAX_BUSY_TURN_MS)
                || now.saturating_sub(since) >= super::BLOCKED_ALERT_AFTER_MS)
            && let Some(task) = super::duty_task(conn, &ship)?
        {
            let body = if asleep {
                format!(
                    "{ship}'s hosted pane is asleep with pending actionable mail and no automatic wake due. Its harness is stopped; inspect its wake policy and permission holds."
                )
            } else {
                format!(
                    "{ship}'s live hosted session is not accepting actionable mail. Inspect its delivery diagnostics and answer in its pane; a live lease is not delivery evidence."
                )
            };
            crate::mail::record_crew(
                conn,
                &json!({
                    "id":format!("hosted-delivery-{ship}-{holder}-{custody}-{progress}-{since}"),
                    "to":"captain", "kind":"notice", "task_id":task, "urgency":"normal",
                    "body":body,
                    "payload":{"hosted_blocked":ship,"delivery_blocked":true,"blocked_since_ms":since,"decline_count":declines,"asleep":asleep,"wake_due":false}
                }),
            )?;
            conn.execute(
                "UPDATE hosted_delivery_alerts SET alerted=1 WHERE ship=?1",
                [&ship],
            )?;
            alerts.push(json!({"ship":ship,"alerted":true,"delivery_blocked":true,"asleep":asleep,"wake_due":false}));
        }
    }
    Ok(alerts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn fixture() -> (tempfile::TempDir, Store) {
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
        (temp, store)
    }

    #[test]
    fn sleeping_wrapper_with_a_due_wake_is_not_a_stalled_agent() {
        let (_temp, store) = fixture();
        let conn = store.connection();
        conn.execute_batch(
            "UPDATE presence SET asleep_holder=holder,asleep_since_ms=1500;
             INSERT INTO hosted_delivery(ship,holder,holder_since_ms,declines,busy_since_ms)
               VALUES('alpha','host:retired',500,3,500);",
        )
        .unwrap();
        assert!(alert(&store, 2000).unwrap().is_empty());
        assert!(alert(&store, 400000).unwrap().is_empty());
        let spells: i64 = conn
            .query_row("SELECT COUNT(*) FROM hosted_delivery_alerts", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(spells, 0, "automatic wake due, not an acceptance spell");
        let progress: i64 = conn
            .query_row("SELECT accepted FROM hosted_delivery", [], |r| r.get(0))
            .unwrap();
        assert_eq!(progress, 0, "suppression must not fabricate progress");
        // Once awake, the new holder gets its own timer, not old declines/busy.
        conn.execute("UPDATE presence SET asleep_holder=NULL", [])
            .unwrap();
        assert!(alert(&store, 2000).unwrap().is_empty());
        assert!(alert(&store, 301999).unwrap().is_empty());
        assert_eq!(alert(&store, 302000).unwrap().len(), 1);
    }

    #[test]
    fn sleeping_backlog_without_a_wake_still_alerts_with_truthful_state() {
        let (_temp, store) = fixture();
        let conn = store.connection();
        conn.execute_batch(
            "UPDATE presence SET asleep_holder=holder,asleep_since_ms=1500;
             INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,state)
               VALUES('permission','alpha','captain','request','approval needed',
                 '{\"type\":\"tool_permission\",\"block\":{\"scope\":\"wake\"}}','normal',2500,'open');"
        ).unwrap();
        // An unidentified ask compares the policy hash it actually observed;
        // omitting that hash means policy drift, not an active permission hold.
        let policy = crate::permissions::for_store(&store).unwrap();
        let payload = json!({"type":"tool_permission", "policy_hash":crate::permissions::whole_policy_hash(&policy),
            "block":{"scope":"wake"}});
        conn.execute(
            "UPDATE mail SET payload=?1 WHERE id='permission'",
            [payload.to_string()],
        )
        .unwrap();
        assert!(crate::permissions::paused(&store, "alpha").unwrap());
        assert!(alert(&store, 2000).unwrap().is_empty());
        assert!(alert(&store, 301999).unwrap().is_empty());
        let alerts = alert(&store, 302000).unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0]["asleep"], true);
        assert_eq!(alerts[0]["wake_due"], false);
        let (body, payload): (String, String) = conn
            .query_row(
                "SELECT body,payload FROM mail WHERE id LIKE 'hosted-delivery-%'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(
            body.contains("asleep with pending actionable mail"),
            "{body}"
        );
        assert!(body.contains("harness is stopped"), "{body}");
        assert!(!body.contains("live hosted session"));
        assert_eq!(
            serde_json::from_str::<Value>(&payload).unwrap()["asleep"],
            true
        );
        assert!(alert(&store, 400000).unwrap().is_empty(), "once per spell");
        // Fresh mail bypasses the permission hold and makes a wake due again.
        conn.execute_batch(
            "INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms)
               VALUES('fresh','captain','alpha','notice','new action','{}','normal',400001);",
        )
        .unwrap();
        assert!(!crate::permissions::paused(&store, "alpha").unwrap());
        assert!(alert(&store, 800000).unwrap().is_empty());
        let spells: i64 = conn
            .query_row("SELECT COUNT(*) FROM hosted_delivery_alerts", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(spells, 0);
    }

    #[test]
    fn busy_turn_holds_delivery_timer_but_has_a_thirty_minute_bound() {
        let (_temp, store) = fixture();
        let conn = store.connection();
        conn.execute(
            "INSERT INTO hosted_delivery(ship,holder,holder_since_ms,busy_since_ms)
             VALUES('alpha','host:alpha',1000,1500)",
            [],
        )
        .unwrap();
        assert!(alert(&store, 2000).unwrap().is_empty());
        assert!(
            alert(&store, 1500 + 10 * 60 * 1000).unwrap().is_empty(),
            "mail arriving during a healthy ten-minute turn must wait"
        );
        assert!(
            alert(&store, 1500 + MAX_BUSY_TURN_MS - 1)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            alert(&store, 1500 + 31 * 60 * 1000).unwrap().len(),
            1,
            "a stuck turn still alerts"
        );
        assert!(alert(&store, 1500 + 32 * 60 * 1000).unwrap().is_empty());
    }

    #[test]
    fn deterministic_custody_progress_and_retry_bound() {
        let (_temp, store) = fixture();
        let conn = store.connection();
        assert!(alert(&store, 2000).unwrap().is_empty());
        assert!(alert(&store, 301999).unwrap().is_empty());
        assert_eq!(alert(&store, 302000).unwrap().len(), 1);
        conn.execute("UPDATE presence SET expires_ms=199999999", [])
            .unwrap();
        assert!(
            alert(&store, 999999).unwrap().is_empty(),
            "renewal is not progress"
        );
        conn.execute("UPDATE presence SET presented_through=1", [])
            .unwrap();
        assert!(alert(&store, 1000000).unwrap().is_empty());
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
            alert(&store, 1000000).unwrap().len(),
            1,
            "K declines bypass N"
        );
        conn.execute(
            "UPDATE presence SET holder='host:beta',holder_since_ms=1000001",
            [],
        )
        .unwrap();
        assert!(
            alert(&store, 1000001).unwrap().is_empty(),
            "old holder's decline cannot alert"
        );
        assert_eq!(
            alert(&store, 1300001).unwrap().len(),
            1,
            "new holder gets its own bound"
        );
        conn.execute("UPDATE presence SET expires_ms=1300001", [])
            .unwrap();
        assert!(alert(&store, 1300001).unwrap().is_empty());
    }
}
