//! Dated desk-side diagnostics evidence, not replay proof or coverage.
//! One latest observation per (domain, source), independent of custody. Old
//! readers ignore this optional v6 table; recording never installs it in an
//! older registry. Recording failure must not fail intake or fabricate support.
use super::*;

// Retain departed sources/domains without eviction. New keys backpressure at
// this registry-wide bound; an existing key can always record another contact.
const ROWS: i64 = 4096;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(in crate::machines::desk_intake) enum Sidecar {
    Absent,
    Marker,
    Proof,
    Unsupported,
    Invalid,
    Oversized,
}
impl Sidecar {
    pub(in crate::machines::desk_intake) fn inspect(wire: &Value) -> Self {
        if wire.is_null() {
            Self::Absent
        } else if transport::json_size(wire) > WIRE {
            Self::Oversized
        } else if wire
            .get("version")
            .and_then(Value::as_u64)
            .is_some_and(|v| v != 1)
        {
            Self::Unsupported
        } else if let Some(decoded) = decode(wire) {
            if decoded["proof"].is_null() {
                Self::Marker
            } else {
                Self::Proof
            }
        } else {
            Self::Invalid
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Marker => "marker",
            Self::Proof => "proof",
            Self::Unsupported => "unsupported",
            Self::Invalid => "invalid",
            Self::Oversized => "oversized",
        }
    }
}

pub(crate) fn install(c: &Connection) -> Result<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS desk_replay_observation(
        domain TEXT NOT NULL, source TEXT NOT NULL, binding TEXT NOT NULL,
        support TEXT NOT NULL CHECK(support IN ('unknown','supported','unsupported')),
        sidecar TEXT NOT NULL CHECK(sidecar IN ('absent','marker','proof','unsupported','invalid','oversized')),
        observed_at INTEGER NOT NULL, session TEXT NOT NULL, tick_ms INTEGER NOT NULL,
        PRIMARY KEY(domain,source));")?;
    // Refuse an incompatible preexisting object before installing the v6
    // marker. No rebuilding, truncation or reinterpretation of old data.
    c.prepare("SELECT domain,source,binding,support,sidecar,observed_at,session,tick_ms FROM desk_replay_observation LIMIT 0")?;
    Ok(())
}

/// Caller has already admitted this legal contact and validated its strict
/// intake frame. No network, authority discovery, DDL or proof adoption here.
/// Syntactic support is not proof validity, readiness, freshness or completeness.
/// Absence/corruption preserves earlier support knowledge in the SAME binding;
/// an explicit unsupported version records unsupported, not an absent marker.
pub(in crate::machines::desk_intake) fn record(
    c: &Connection,
    m: &Manifest,
    source: &str,
    sidecar: Sidecar,
) -> Result<()> {
    record_at(c, m, source, sidecar, &transport::process_clock())
}
fn record_at(
    c: &Connection,
    m: &Manifest,
    source: &str,
    sidecar: Sidecar,
    clock: &Clock<'_>,
) -> Result<()> {
    ensure!(m.sources.iter().any(|s| s.core == source), "unknown source");
    let installed: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='desk_replay_observation')",
        [],
        |r| r.get(0),
    )?;
    if !installed {
        return Ok(());
    }
    let tx = c
        .is_autocommit()
        .then(|| rusqlite::Transaction::new_unchecked(c, TransactionBehavior::Immediate))
        .transpose()?;
    let old: Option<(String, String)> = c
        .query_row(
            "SELECT binding,support FROM desk_replay_observation WHERE domain=?1 AND source=?2",
            params![m.domain, source],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if old.is_none() {
        let rows: i64 = c.query_row("SELECT COUNT(*) FROM desk_replay_observation", [], |r| {
            r.get(0)
        })?;
        ensure!(rows < ROWS, "backpressure: replay observation rows");
    }
    let bound = binding(m)?;
    let support = match sidecar {
        Sidecar::Marker | Sidecar::Proof => "supported",
        Sidecar::Unsupported => "unsupported",
        _ => old
            .as_ref()
            .filter(|(b, _)| b == &bound)
            .map(|(_, s)| s.as_str())
            .unwrap_or("unknown"),
    };
    c.execute("INSERT INTO desk_replay_observation(domain,source,binding,support,sidecar,observed_at,session,tick_ms)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
        ON CONFLICT(domain,source) DO UPDATE SET binding=excluded.binding,support=excluded.support,sidecar=excluded.sidecar,
        observed_at=excluded.observed_at,session=excluded.session,tick_ms=excluded.tick_ms",
        params![m.domain,source,bound,support,sidecar.as_str(),clock.wall_ms,clock.session,clock.tick_ms])?;
    if let Some(tx) = tx {
        tx.commit()?;
    }
    Ok(())
}

/// Dated diagnostic evidence only. A v5 writer cannot update this row, so a
/// newer intake receipt, changed binding, lost clock continuity or age expiry
/// makes it stale. No caller may derive current support from row presence.
pub(in crate::machines::desk_intake) fn diagnostic(
    c: &Connection,
    m: &Manifest,
    source: &str,
    clock: &Clock<'_>,
) -> Result<Value> {
    let unknown = json!({"support":"unknown","sidecar":"unknown","observed_at":null,"age_ms":null,"current":false});
    let installed: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='desk_replay_observation')", [], |r|r.get(0))?;
    if !installed {
        return Ok(unknown);
    }
    let row: Option<(String,String,String,i64,String,i64)> = c.query_row("SELECT binding,support,sidecar,observed_at,session,tick_ms FROM desk_replay_observation WHERE domain=?1 AND source=?2", params![m.domain,source], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
    let Some((bound, support, sidecar, observed, session, tick)) = row else {
        return Ok(unknown);
    };
    if bound != binding(m)? {
        return Ok(unknown);
    }
    let age = cuts::measured_age(clock, &session, observed, tick);
    // Check against independent ordinary custody metadata. A retained support
    // row never establishes either source coverage or proof adoption.
    let contact: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM desk_intake_checkpoint WHERE domain=?1 AND source=?2 AND binding=?3 AND clock_valid=1 AND upper_seq=through_seq AND session=?4 AND received_at<=?5 AND tick_ms<=?6)", params![m.domain,source,bound,session,observed,tick], |r|r.get(0))?;
    let current = contact && age.is_some_and(|age| age <= m.max_source_age_ms);
    Ok(
        json!({"support":if current {support.as_str()} else {"unknown"},
        "sidecar":if current {sidecar.as_str()} else {"stale"},
        "observed_at":observed,"age_ms":age,"current":current}),
    )
}

#[cfg(test)]
mod tests;
