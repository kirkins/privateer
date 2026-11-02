//! Why a first mate woke: each wake, hosted ones included, leaves one
//! `wake.started` audit event naming its reason and the causes behind it, so
//! `activity.list` can show it beside the rest of the ship's activity.
//!
//! The reason is the kind of the oldest actionable cause, the rule a headless
//! wake uses for its primary focus. Runtime notices all come from `crew`; their
//! payload keys tell them apart, and anything unrecognised is `runtime`.
use crate::store::{Caller, Store, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::params;
use serde_json::{Value, json};

/// Causes kept on one record; the rest are counted in `more`.
const MAX_CAUSES: usize = 8;
/// Mail rows read for one record.
const MAX_MAIL: i64 = 200;

/// The kind of one piece of mail, as a wake reason.
pub(crate) fn classify(sender: &str, payload: &Value) -> &'static str {
    if sender == crate::mail::CAPTAIN {
        return "captain";
    }
    if sender != crate::mail::CREW {
        return "mail";
    }
    let has = |key: &str| payload.get(key).is_some_and(|v| !v.is_null());
    if has("backlog_alarm") {
        "backlog_alarm"
    } else if has("report") {
        "crew_report"
    } else if has("errand") {
        "errand"
    } else if has("job_id") && has("status") {
        "crew_outcome"
    } else {
        // Scheduling faults, hosted-pane alerts, cleared blocks, relaunches,
        // machine alerts and the rest of the runtime's own notices.
        "runtime"
    }
}

/// One cause of a wake: a piece of mail, by kind.
pub(crate) fn mail_cause(
    sender: &str,
    id: &str,
    seq: i64,
    task_id: Option<&str>,
    payload: &Value,
) -> Value {
    json!({"kind": classify(sender, payload), "from": sender, "mail_id": id, "seq": seq,
        "task_id": task_id, "job_id": payload.get("job_id").and_then(Value::as_str)})
}

/// A wake's cause from its causes, oldest first: the oldest one's kind is the
/// reason, or `fallback` when there are none.
pub(crate) fn summary(mut causes: Vec<Value>, fallback: &str) -> Value {
    causes.sort_by_key(|c| c["seq"].as_i64().unwrap_or(i64::MAX));
    let reason = causes
        .first()
        .and_then(|c| c["kind"].as_str())
        .unwrap_or(fallback)
        .to_owned();
    let more = causes.len().saturating_sub(MAX_CAUSES);
    causes.truncate(MAX_CAUSES);
    json!({"reason": reason, "causes": causes, "more": more})
}

/// The actionable mail a ship received after `after_seq` through
/// `through_seq`, as a wake's cause. Routine notices wait for a real wake, so
/// they are never its cause.
pub(crate) fn of_mail(
    conn: &rusqlite::Connection,
    ship: &str,
    after_seq: i64,
    through_seq: i64,
    fallback: &str,
) -> Result<Value> {
    let mut statement = conn.prepare(&format!(
        "SELECT seq, id, sender, task_id, payload FROM mail
         WHERE recipient=?1 AND COALESCE(delivery, 'received')='received'
           AND seq>?2 AND seq<=?3 AND NOT ({})
         ORDER BY seq LIMIT ?4",
        crate::mail::ROUTINE_MAIL
    ))?;
    let causes = statement
        .query_map(params![ship, after_seq, through_seq, MAX_MAIL], |r| {
            let payload: Option<String> = r.get(4)?;
            let payload = payload
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .unwrap_or(Value::Null);
            Ok(mail_cause(
                &r.get::<_, String>(2)?,
                &r.get::<_, String>(1)?,
                r.get(0)?,
                r.get::<_, Option<String>>(3)?.as_deref(),
                &payload,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(summary(causes, fallback))
}

/// Record that a ship's first mate woke, in the caller's transaction when it
/// has one. `cause` is a `summary`; `extra` adds fields such as the mail
/// sequence the wake covers.
pub(crate) fn record(
    store: &Store,
    ship: &str,
    wake: Option<&str>,
    mode: &str,
    cause: &Value,
    extra: &Value,
) -> Result<i64> {
    let mut payload = json!({"ship": ship, "wake": wake, "mode": mode,
        "reason": cause["reason"], "causes": cause["causes"], "more": cause["more"]});
    if let Some(extra) = extra.as_object() {
        for (key, value) in extra {
            payload[key] = value.clone();
        }
    }
    store.audit("wake.started", &payload)
}

/// A non-negative sequence argument, if given.
fn seq(args: &Value, key: &str) -> Result<Option<i64>> {
    match args.get(key).filter(|v| !v.is_null()) {
        Some(value) => {
            let n = value
                .as_i64()
                .filter(|n| *n >= 0)
                .with_context(|| format!("{key} must be a non-negative whole number"))?;
            Ok(Some(n))
        }
        None => Ok(None),
    }
}

/// `wake.hosted`: the current hosted holder reports an accepted turn or its
/// delivery state. Accepted ranges are receipts; delivery reports carry the
/// session's diagnostic watermark, pause, busy turn and bounded decline count,
/// never delivery progress. Neither creates a `wakes` row or spends the wake budget.
pub(crate) fn hosted(store: &Store, args: &Value) -> Result<Value> {
    let ship = required(args, "ship")?;
    let holder = required(args, "holder")?;
    let trigger = required(args, "trigger")?;
    if args.get("session").is_some() {
        let session = required(args, "session")?;
        ensure!(session.len() <= 200, "session is too long");
    }
    match store.caller() {
        Caller::Agent { actor } if actor == ship => {}
        _ => bail!("wake.hosted is the hosted first mate's own, as its ship's agent"),
    }
    let after = seq(args, "after_seq")?;
    let through = seq(args, "through_seq")?;
    let range = match (after, through) {
        (Some(after), Some(through)) => {
            ensure!(after <= through, "after_seq must not exceed through_seq");
            Some((after, through))
        }
        (None, None) => None,
        _ => bail!("give both after_seq and through_seq, or neither"),
    };
    let tx = store.write_transaction()?;
    let presence = crate::presence::presence(store, ship)?;
    ensure!(
        holder.starts_with(crate::presence::HOSTED)
            && presence["attended"] == true
            && presence["lease"]["holder"] == holder,
        "wake.hosted requires the current hosted presence lease"
    );
    let custody = crate::presence::session(store, ship)?.context("hosted session")?;
    tx.execute(
        "DELETE FROM hosted_delivery WHERE ship=?1 AND (holder<>?2 OR holder_since_ms<>?3)",
        params![ship, holder, custody.since_ms],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO hosted_delivery(ship,holder,holder_since_ms) VALUES(?1,?2,?3)",
        params![ship, holder, custody.since_ms],
    )?;
    if trigger == "delivery_bind" {
        let session = required(args, "session")?;
        // A new native conversation may keep its launch holder. Bind its
        // reports without forgetting mail already accepted in that custody,
        // or clearing the watchdog's spell. Delivery-state reports must match
        // the binding; a bind or accepted receipt may replace it under the
        // same agent identity.
        tx.execute(
            "UPDATE hosted_delivery SET session=?2,
             report_accepted=CASE WHEN session IS ?2 THEN report_accepted ELSE 0 END,
             declines=CASE WHEN session IS ?2 THEN declines ELSE 0 END,
             paused=CASE WHEN session IS ?2 THEN paused ELSE 0 END,
             reason=CASE WHEN session IS ?2 THEN reason ELSE NULL END,
             busy_since_ms=CASE WHEN session IS ?2 THEN busy_since_ms ELSE NULL END WHERE ship=?1",
            params![ship, session],
        )?;
        tx.commit()?;
        return Ok(json!({"recorded":true}));
    }
    if trigger == "delivery" {
        let session = required(args, "session")?;
        let accepted = seq(args, "accepted_through")?.context("accepted_through required")?;
        let declines = seq(args, "decline_count")?.context("decline_count required")?;
        ensure!(declines <= 3, "decline_count must be at most 3");
        let paused = args["paused"].as_bool().context("paused must be boolean")?;
        let busy = match args.get("busy") {
            Some(v) => v.as_bool().context("busy must be boolean")?,
            None => false, // Older adapters have no busy report.
        };
        let now = Store::now_ms();
        let busy_since = if busy {
            Some(
                seq(args, "turn_started_ms")?
                    .context("busy requires turn_started_ms")?
                    .max(custody.since_ms)
                    .min(now),
            )
        } else {
            None
        };
        let reason = args["decline_reason"].as_str().unwrap_or("other");
        ensure!(
            matches!(
                reason,
                "native_stopped" | "guard_paused" | "guard_not_ours" | "guard_not_owned" | "other"
            ),
            "invalid decline_reason"
        );
        // Ending a busy turn resumes the timer from this edge, without
        // rearming an alerted spell. Invalid reports roll this back too.
        tx.execute(
            "UPDATE hosted_delivery_alerts SET blocked_since_ms=MAX(blocked_since_ms,?2)
             WHERE ship=?1 AND ?3 IS NULL AND EXISTS
               (SELECT 1 FROM hosted_delivery d WHERE d.ship=?1 AND d.busy_since_ms IS NOT NULL)",
            params![ship, now, busy_since],
        )?;
        let changed = tx.execute(
            "UPDATE hosted_delivery SET session=?2, report_accepted=?3,
             paused=?4, reason=?5, declines=?6,
             busy_since_ms=CASE WHEN ?7 IS NULL THEN NULL
               ELSE MIN(COALESCE(busy_since_ms,?7),?7) END
             WHERE ship=?1 AND (session IS NULL OR session=?2) AND report_accepted<=?3",
            params![
                ship, session, accepted, paused, reason, declines, busy_since
            ],
        )?;
        ensure!(
            changed == 1,
            "delivery report belongs to another session or predates accepted progress"
        );
        tx.commit()?;
        let alerts = crate::host::alert_blocked(store, &[])?;
        return Ok(json!({"recorded":true,"alerts":alerts}));
    }
    let cause = match (trigger, range) {
        ("mail", Some((after, through))) => of_mail(&tx, ship, after, through, "mail")?,
        ("mail", None) => bail!("a mail wake names after_seq and through_seq"),
        ("captain", range) => {
            let mut cause = match range {
                Some((after, through)) => of_mail(&tx, ship, after, through, "captain_pane")?,
                None => summary(vec![], "captain_pane"),
            };
            // The captain's prompt started the turn; the mail rode along.
            cause["reason"] = json!("captain_pane");
            cause
        }
        ("follow", _) => summary(vec![], "follow_up"),
        _ => bail!("trigger must be mail, captain, follow, delivery_bind or delivery"),
    };
    let (after, through) = range.unzip();
    tx.execute(
        "UPDATE hosted_delivery SET accepted=MAX(accepted,COALESCE(?2,0)), declines=0, paused=0,
         report_accepted=CASE WHEN ?3 IS NOT NULL AND session IS NOT ?3 THEN COALESCE(?2,0)
           ELSE MAX(report_accepted,COALESCE(?2,0)) END,
         session=COALESCE(?3,session) WHERE ship=?1",
        params![ship, through, args.get("session").and_then(Value::as_str)],
    )?;
    record(
        store,
        ship,
        None,
        "hosted",
        &cause,
        &json!({"trigger": trigger, "after_seq": after, "through_seq": through}),
    )?;
    tx.commit()?;
    Ok(json!({"recorded": true, "reason": cause["reason"]}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_of_mail_has_its_reason() {
        let none = json!({});
        assert_eq!(classify("captain", &none), "captain");
        assert_eq!(
            classify("captain", &json!({"job_id": "j", "status": "failed"})),
            "captain"
        );
        assert_eq!(classify("apps", &none), "mail");
        assert_eq!(classify("apps", &json!({"backlog_alarm": {}})), "mail");
        let crew = [
            (json!({"backlog_alarm": {"fires": 1}}), "backlog_alarm"),
            (
                json!({"job_id": "j", "incarnation": "i", "report": "progress"}),
                "crew_report",
            ),
            (
                json!({"errand": "errand-1", "focus": "f", "status": "succeeded"}),
                "errand",
            ),
            (
                json!({"job_id": "j", "status": "succeeded", "exit_code": 0}),
                "crew_outcome",
            ),
            (json!({"scheduling_fault": "t", "revision": 1}), "runtime"),
            (json!({"about": "ask", "block_cleared": true}), "runtime"),
            (json!({"job_id": "j", "relaunched_as": "k"}), "runtime"),
            (json!({"job_id": "j", "refused": "agent.start"}), "runtime"),
            (json!({"placement_outcome": {}}), "runtime"),
            (json!({"job_id": "j", "status": null}), "runtime"),
            (Value::Null, "runtime"),
            (none.clone(), "runtime"),
        ];
        for (payload, reason) in crew {
            assert_eq!(classify("crew", &payload), reason, "{payload}");
        }
    }

    #[test]
    fn the_oldest_cause_is_the_reason_and_the_rest_are_bounded() {
        let causes: Vec<Value> = (0..11)
            .rev()
            .map(|n| {
                let sender = if n == 0 { "captain" } else { "apps" };
                mail_cause(sender, &format!("m{n}"), n + 1, None, &json!({}))
            })
            .collect();
        let cause = summary(causes, "mail");
        assert_eq!(cause["reason"], "captain");
        assert_eq!(cause["causes"].as_array().unwrap().len(), MAX_CAUSES);
        assert_eq!(cause["causes"][0]["mail_id"], "m0");
        assert_eq!(cause["more"], 3);
        let empty = summary(vec![], "follow_up");
        assert_eq!(empty["reason"], "follow_up");
        assert_eq!(empty["more"], 0);
    }
}
