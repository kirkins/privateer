//! A first mate's attention: how full its live context is and how fast it
//! fills, measured rather than capped. The attended Pi adapter reports the
//! context after each run and each compaction. Bearings retain the latest
//! measured fullness separately from the last hour's growth. Only a fresh
//! measurement from the current session can qualify hosted work delivery.
//! The pressure thresholds remain provisional.
use crate::store::{Store, required};
use anyhow::{Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

const HOUR_MS: i64 = 60 * 60 * 1000;
/// Prune history after this interval, retaining the latest fullness sample.
const KEEP_MS: i64 = 24 * HOUR_MS;
const FRESH_MS: i64 = 300_000;
/// Provisional limits for high pressure.
const HIGH_PERCENT: f64 = 75.0;
const HIGH_UNREAD_CREW: i64 = 5;

pub fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS attention(ship TEXT NOT NULL, at_ms INTEGER NOT NULL,
           kind TEXT NOT NULL, tokens INTEGER NOT NULL, window INTEGER NOT NULL,
           summary_chars INTEGER);
         CREATE INDEX IF NOT EXISTS attention_ship ON attention(ship, at_ms);",
    )?;
    let tx = store.write_transaction()?;
    let present: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('attention') WHERE name='session_holder')",
        [],
        |r| r.get(0),
    )?;
    if !present {
        tx.execute_batch("ALTER TABLE attention ADD COLUMN session_holder TEXT;")?;
    }
    tx.commit()?;
    Ok(())
}

/// `attention.report {ship, kind: turn|compaction|sample, holder?, tokens, window, summary_chars?}`
fn report(store: &Store, args: &Value) -> Result<Value> {
    let ship = required(args, "ship")?;
    crate::context::check_focus(store, ship, "ship")?;
    let kind = required(args, "kind")?;
    ensure!(
        matches!(kind, "turn" | "compaction" | "sample"),
        "kind is turn, compaction or sample"
    );
    let number = |key: &str| -> Result<i64> {
        args.get(key)
            .and_then(Value::as_i64)
            .filter(|n| *n >= 0)
            .ok_or_else(|| anyhow::anyhow!("{key} must be a whole number"))
    };
    let (tokens, window) = (number("tokens")?, number("window")?);
    ensure!(window > 0, "window must be positive");
    let presence = crate::presence::presence(store, ship)?;
    let holder = presence["lease"]["holder"]
        .as_str()
        .filter(|_| presence["attended"] == true);
    if kind == "sample" || args.get("holder").is_some() {
        ensure!(
            holder.is_some() && args["holder"].as_str() == holder,
            "attention sample requires the current presence holder"
        );
    }
    let summary = args.get("summary_chars").and_then(Value::as_i64);
    let now = Store::now_ms();
    store.connection().execute(
        "INSERT INTO attention(ship, at_ms, kind, tokens, window, summary_chars, session_holder)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![ship, now, kind, tokens, window, summary, holder],
    )?;
    store.connection().execute(
        "DELETE FROM attention WHERE ship=?1 AND at_ms < ?2 AND rowid<>(SELECT rowid FROM attention
         WHERE ship=?1 AND kind IN ('turn','sample') ORDER BY at_ms DESC,rowid DESC LIMIT 1)",
        params![ship, now - KEEP_MS],
    )?;
    summary_for(store, ship)
}

/// Latest fullness and rolling growth; stale or replaced sessions are unknown.
pub fn summary_for(store: &Store, ship: &str) -> Result<Value> {
    let now = Store::now_ms();
    let since = now - HOUR_MS;
    let presence = crate::presence::presence(store, ship)?;
    let holder = presence["lease"]["holder"]
        .as_str()
        .filter(|_| presence["attended"] == true);
    let latest: Option<(i64, i64, i64, Option<String>)> = store.connection().query_row(
        "SELECT at_ms,tokens,window,session_holder FROM attention WHERE ship=?1 AND kind IN ('turn','sample') ORDER BY at_ms DESC,rowid DESC LIMIT 1",
        [ship], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let Some((sample_ms, tokens, window, sample_holder)) = latest else {
        return Ok(if holder.is_some() {
            json!({"pressure":"unknown","fresh":false,"reason":"Current session has no context sample"})
        } else {
            Value::Null
        });
    };
    if now - sample_ms > FRESH_MS || (holder.is_some() && holder != sample_holder.as_deref()) {
        return Ok(
            json!({"pressure":"unknown","fresh":false,"sample_ms":sample_ms,
            "last_tokens":tokens,"last_window":window,"reason":"Fresh context sample required for the current session"}),
        );
    }
    let mut statement = store.connection().prepare(
        "SELECT kind,tokens,window,summary_chars FROM attention WHERE ship=?1 AND at_ms>=?2
         AND session_holder IS ?3 ORDER BY at_ms,rowid",
    )?;
    let rows: Vec<(String, i64, i64, Option<i64>)> = statement
        .query_map(params![ship, since, sample_holder], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let unread_crew: i64 = store.connection().query_row(
        "SELECT COUNT(*) FROM mail WHERE recipient=?1 AND sender='crew'
         AND COALESCE(delivery,'received')='received'
         AND seq>COALESCE((SELECT through_seq FROM mail_cursor WHERE address=?1),0)",
        [ship],
        |r| r.get(0),
    )?;
    let compactions = rows.iter().filter(|r| r.0 == "compaction").count() as i64;
    let last_summary = rows
        .iter()
        .rev()
        .find(|r| r.0 == "compaction")
        .and_then(|r| r.3);
    // Context added in the hour: every rise between consecutive turns. A
    // compaction resets the baseline rather than counting as a fall.
    let mut added = 0;
    let mut previous: Option<i64> = None;
    for (kind, tokens, _, _) in &rows {
        if kind == "compaction" {
            previous = None;
            continue;
        }
        if let Some(before) = previous {
            added += (tokens - before).max(0);
        }
        previous = Some(*tokens);
    }
    let percent = tokens as f64 * 100.0 / window as f64;
    let mut reasons = vec![];
    if compactions > 0 {
        reasons.push(format!("compacted {compactions}× in the last hour"));
    }
    if percent >= HIGH_PERCENT {
        reasons.push(format!("context {percent:.0}% full"));
    }
    if added * 2 >= window {
        reasons.push(format!("{}k tokens added in the last hour", added / 1000));
    }
    if unread_crew >= HIGH_UNREAD_CREW {
        reasons.push(format!("{unread_crew} crew reports unread"));
    }
    let pressure = if reasons.is_empty() { "normal" } else { "high" };
    Ok(
        json!({"pressure": pressure, "fresh": true, "sample_ms":sample_ms, "reasons": reasons, "percent": percent.round(),
        "tokens": tokens, "window": window, "compactions_hour": compactions,
        "added_hour": added, "last_summary_chars": last_summary, "unread_crew": unread_crew}),
    )
}

/// One line for the bearings, or None with no reports.
pub fn line(summary: &Value) -> Option<String> {
    if summary.is_null() {
        return None;
    }
    if summary["pressure"] == "unknown" {
        return Some("Attention: unknown (fresh sample required; stale measurements are not healthy context)".into());
    }
    let mut line = format!(
        "Attention: {} (context {}% of {}k, {}k added in the last hour, {} compaction(s), {} crew reports unread)",
        summary["pressure"].as_str().unwrap_or(""),
        summary["percent"].as_f64().unwrap_or(0.0) as i64,
        summary["window"].as_i64().unwrap_or(0) / 1000,
        summary["added_hour"].as_i64().unwrap_or(0) / 1000,
        summary["compactions_hour"],
        summary["unread_crew"],
    );
    if summary["pressure"] == "high" {
        line.push_str(
            ". Start no new task: send pending reviews to errands, finish or record waits, until it eases.",
        );
    }
    Some(line)
}

pub fn dispatch(store: &Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "attention.report" => report(store, &args),
        "attention.show" => summary_for(store, required(&args, "ship")?),
        _ => bail!("unknown attention command {command}"),
    }
}
