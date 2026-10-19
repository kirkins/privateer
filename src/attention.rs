//! A first mate's attention: how full its live context is and how fast it
//! fills, measured rather than capped. The attended Pi adapter reports the
//! context after each run and each compaction. Bearings retain the latest
//! measured fullness separately from the last hour's growth. A measurement
//! counts only for the session that took it; once another session holds the
//! ship, it reads unknown until that session reports.
//! The pressure thresholds remain provisional.
//! A hosted first mate that cannot resume its conversation (Claude with
//! bounded tool bundles) has no quiet compaction to reach for. It is warned
//! in its bearings at `context_warn_percent`, and a settled turn that ends at
//! or past `context_compact_percent` asks its pane to relaunch into a fresh
//! session seeded from its handoff notes.
use crate::presence;
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
    // Only a turn that ended its answer, reported by the session holding the
    // ship, may ask for a fresh session: never mid-turn or with a tool pending.
    let settled = args.get("settled").and_then(Value::as_bool) == Some(true);
    if let Some(holder) = holder.filter(|_| kind == "turn" && settled)
        && args["holder"].as_str() == Some(holder)
    {
        request_fresh_start(store, ship, holder, tokens, window)?;
    }
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
    // A session's context changes only through the turns and compactions it
    // reports, so its own latest sample stays current however old. Age only
    // matters when no session holds the ship.
    let stale = match holder {
        Some(holder) => sample_holder.as_deref() != Some(holder),
        None => now - sample_ms > FRESH_MS,
    };
    if stale {
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
    let unread_crew = unread_crew(store, ship)?;
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
    let fresh = fresh_start(store, ship)?;
    let warning = fresh.is_some_and(|(warn, _)| percent >= warn);
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
        "added_hour": added, "last_summary_chars": last_summary, "unread_crew": unread_crew,
        "fresh_start": fresh.map(|(warn, compact)| json!({"warn_percent": warn, "compact_percent": compact})),
        "fresh_start_warning": warning}),
    )
}

fn unread_crew(store: &Store, ship: &str) -> Result<i64> {
    Ok(store.connection().query_row(
        "SELECT COUNT(*) FROM mail WHERE recipient=?1 AND sender='crew'
         AND COALESCE(delivery,'received')='received'
         AND seq>COALESCE((SELECT through_seq FROM mail_cursor WHERE address=?1),0)",
        [ship],
        |r| r.get(0),
    )?)
}

/// The warning and compaction fullness of a first mate whose conversation
/// cannot resume: hosted, on Claude, with bounded tool bundles, so every
/// launch starts fresh. None for any other first mate, which keeps one
/// conversation and so has no fresh start to warn about.
fn fresh_start(store: &Store, ship: &str) -> Result<Option<(f64, f64)>> {
    let record: Option<String> = store
        .connection()
        .query_row("SELECT record FROM ships WHERE name=?1", [ship], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(record) = record else {
        return Ok(None);
    };
    let first = serde_json::from_str::<Value>(&record)?["first_mate"].clone();
    let claude = first["manifest"]
        .as_str()
        .and_then(crate::bundled::harness)
        .as_deref()
        == Some("claude");
    if !(claude && crate::host::hosted(&first) && first.get("tool_bundles").is_some()) {
        return Ok(None);
    }
    let warn = first["context_warn_percent"].as_f64().unwrap_or(40.0);
    let compact = first["context_compact_percent"].as_f64().unwrap_or(50.0);
    Ok(Some((warn.min(compact), compact)))
}

/// Asks the hosted pane to relaunch into a fresh session once a settled turn
/// ends at or past the compaction fullness, whether or not the first mate is
/// idle: the supervisor takes the request at the next safe point. One crossing
/// asks once: a request still pending holds the next one back, and the
/// relaunch ends the session that crossed. A captain's open tool question or
/// an unread crew report holds it too, so no decision is cut off.
fn request_fresh_start(
    store: &Store,
    ship: &str,
    holder: &str,
    tokens: i64,
    window: i64,
) -> Result<()> {
    let Some((_, compact)) = fresh_start(store, ship)? else {
        return Ok(());
    };
    let percent = tokens as f64 * 100.0 / window as f64;
    if percent < compact
        || !holder.starts_with(presence::HOSTED)
        || crate::permissions::paused(store, ship)?
    {
        return Ok(());
    }
    let now = Store::now_ms();
    let pending: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM hosted_relaunches WHERE ship=?1 AND expires_ms>?2)",
        params![ship, now],
        |r| r.get(0),
    )?;
    if pending || unread_crew(store, ship)? > 0 {
        return Ok(());
    }
    store.connection().execute(
        "INSERT INTO hosted_relaunches(ship,holder,force,requested_ms,expires_ms) VALUES(?1,?2,0,?3,?4)
         ON CONFLICT(ship) DO UPDATE SET holder=excluded.holder, force=0,
           requested_ms=excluded.requested_ms, expires_ms=excluded.expires_ms",
        params![ship, holder, now, now + crate::host::upgrade::REQUEST_TTL_MS],
    )?;
    store.audit(
        "host.context_compact",
        &json!({"ship": ship, "holder": holder, "tokens": tokens, "window": window,
            "percent": percent.round(), "compact_percent": compact}),
    )?;
    Ok(())
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
    if summary["fresh_start_warning"] == true {
        line.push_str(&format!(
            ". Write your handoff notes now: at {}% full, the turn that crosses it ends this session, and a fresh one starts from those notes.",
            summary["fresh_start"]["compact_percent"].as_f64().unwrap_or(50.0) as i64,
        ));
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

#[cfg(test)]
mod tests {
    use super::*;

    const SHIP: &str = "storefront";
    const HOLDER: &str = "host:pane";
    /// A 200k window: 50% is 100k tokens, 40% is 80k.
    const WINDOW: i64 = 200_000;

    /// A hosted Claude first mate on bounded tool bundles: every launch starts fresh.
    fn bounded_claude(store: &Store) {
        let record = json!({"location": "here", "first_mate": {"manifest": "bundled:claude",
            "host": "herdr", "tool_bundles": [], "context_warn_percent": 40, "context_compact_percent": 50}});
        store
            .connection()
            .execute(
                "INSERT INTO ships(name, record) VALUES(?1, ?2)",
                params![SHIP, record.to_string()],
            )
            .unwrap();
        crate::presence::attend(store, SHIP, HOLDER, 90, None).unwrap();
    }

    fn report_turn(store: &Store, tokens: i64, settled: bool) -> Value {
        report(
            store,
            &json!({"ship": SHIP, "kind": "turn", "tokens": tokens, "window": WINDOW,
                "holder": HOLDER, "settled": settled}),
        )
        .unwrap()
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.connection().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn requests(store: &Store) -> i64 {
        count(store, "SELECT COUNT(*) FROM hosted_relaunches")
    }

    fn audited(store: &Store) -> i64 {
        count(
            store,
            "SELECT COUNT(*) FROM audit WHERE kind='host.context_compact'",
        )
    }

    /// A busy first mate is never idle, yet a settled turn over the line asks
    /// for the fresh session at once; a turn still running, or with a tool
    /// pending, does not.
    #[test]
    fn a_settled_turn_past_the_line_asks_for_a_fresh_session_even_when_busy() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        bounded_claude(&store);
        report_turn(&store, 110_000, false);
        assert_eq!(requests(&store), 0, "mid-turn reports never ask");
        report_turn(&store, 110_000, true);
        assert_eq!(requests(&store), 1);
        let row: (String, bool) = store
            .connection()
            .query_row(
                "SELECT holder, force FROM hosted_relaunches WHERE ship=?1",
                [SHIP],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (HOLDER.to_owned(), false),
            "an idle-waiting request, never forced"
        );
    }

    /// Under the line nothing is asked, but the warning shows from 40%.
    #[test]
    fn under_the_line_the_warning_shows_and_nothing_is_asked() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        bounded_claude(&store);
        let quiet = report_turn(&store, 60_000, true);
        assert_eq!(quiet["fresh_start_warning"], false);
        assert_eq!(requests(&store), 0);
        let warned = report_turn(&store, 90_000, true);
        assert_eq!(warned["fresh_start_warning"], true);
        assert_eq!(warned["fresh_start"]["compact_percent"], 50.0);
        assert_eq!(requests(&store), 0);
        let text = line(&warned).unwrap();
        assert!(text.contains("Write your handoff notes now"), "{text}");
    }

    /// One crossing asks once, however many settled turns end past the line
    /// while the request waits; a new session below the line re-arms it.
    #[test]
    fn a_crossing_asks_once() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        bounded_claude(&store);
        report_turn(&store, 110_000, true);
        report_turn(&store, 120_000, true);
        report_turn(&store, 130_000, true);
        assert_eq!(requests(&store), 1);
        assert_eq!(audited(&store), 1);
        // The relaunch spends the request and ends the session that crossed.
        store
            .connection()
            .execute("DELETE FROM hosted_relaunches WHERE ship=?1", [SHIP])
            .unwrap();
        report_turn(&store, 30_000, true);
        assert_eq!(requests(&store), 0);
        report_turn(&store, 110_000, true);
        assert_eq!(requests(&store), 1);
        assert_eq!(audited(&store), 2);
    }

    /// A first mate that can resume its conversation, or is not Claude, has
    /// no fresh start: no warning and no request.
    #[test]
    fn a_first_mate_that_can_resume_is_never_asked_to_start_fresh() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let record = json!({"location": "here", "first_mate": {"manifest": "bundled:claude",
            "host": "herdr", "context_warn_percent": 40, "context_compact_percent": 50}});
        store
            .connection()
            .execute(
                "INSERT INTO ships(name, record) VALUES(?1, ?2)",
                params![SHIP, record.to_string()],
            )
            .unwrap();
        crate::presence::attend(&store, SHIP, HOLDER, 90, None).unwrap();
        let summary = report_turn(&store, 150_000, true);
        assert!(summary["fresh_start"].is_null(), "{summary}");
        assert_eq!(summary["fresh_start_warning"], false);
        assert_eq!(requests(&store), 0);
    }
}
