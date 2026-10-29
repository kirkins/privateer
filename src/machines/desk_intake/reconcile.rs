//! Off-contact, read-only census/history reconciliation. A proof is a dated
//! vector of home read snapshots, NOT a contact barrier or fresh source claim.
//! The completion stage must bind/validate it against its advertised barrier.
//! Paths stay in the local inventory; evidence uses opaque home handles.
use super::*;
use crate::mail::source::{self, enrollment};
use std::{path::Path, time::Instant};

#[cfg(test)]
#[path = "reconcile_tests.rs"]
mod tests;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct HomeProof {
    home: String,
    retired: bool,
    state: String,
    mailbox: Option<String>,
    nonce: Option<String>,
    baseline_upper: Option<i64>,
    delta_upper: Option<i64>,
    unread: Option<i64>,
    open: Option<i64>,
    facts_cached: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct Proof {
    #[serde(default)]
    pub(super) candidate: bool,
    #[serde(default)]
    pub(super) registry_stamp: Option<String>,
    #[serde(default)]
    pub(super) copies: Vec<super::proof::CopyEvidence>,
    binding: String,
    source: String,
    source_upper: i64,
    inventory: String,
    membership_history_gap: bool,
    membership_proved: bool,
    history_proved: bool,
    homes: Vec<HomeProof>,
}
#[cfg(test)]
thread_local! {
    pub(super) static REVALIDATE_VM: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    // Deterministic per-contact proof budget; no sleeps or load assumptions.
    pub(super) static HOME_BUDGET: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

fn check(deadline: Instant) -> Result<()> {
    ensure!(source::work_now() < deadline, "backpressure: census budget");
    Ok(())
}

fn home_proof(
    path: &Path,
    bound: &str,
    proof: &mut HomeProof,
    deadline: Instant,
    counts: bool,
) -> Result<()> {
    crate::store::reject_symlinks(path)?;
    let db = path.join("privateer.sqlite");
    crate::store::reject_symlinks(&db)?;
    let conn = Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let _budget = transport::Budget::new(&conn, deadline)?;
    #[cfg(test)]
    if !counts {
        conn.progress_handler(
            1,
            Some(move || {
                REVALIDATE_VM.with(|n| n.set(n.get() + 1));
                Instant::now() >= deadline
            }),
        );
    }
    let read = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Deferred)?;
    // One home snapshot binds lifecycle counts, health, pointers and intents.
    // Negative uppers are deliberately never published, even with no mail.
    if counts {
        let (mailbox, unread, open): (String, i64, i64) = read.query_row(
        "SELECT mailbox,(SELECT COUNT(*) FROM mail WHERE recipient='captain' AND seq>COALESCE((SELECT through_seq FROM mail_cursor WHERE address='captain'),0) AND COALESCE(state,'')<>'acknowledged'),(SELECT COUNT(*) FROM mail WHERE (sender='captain' OR recipient='captain') AND state='open') FROM mail_source_health WHERE only=1", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        proof.mailbox = Some(mailbox);
        proof.unread = Some(unread);
        proof.open = Some(open);
    } else {
        proof.mailbox = Some(read.query_row(
            "SELECT mailbox FROM mail_source_health WHERE only=1",
            [],
            |r| r.get(0),
        )?);
    }
    if !enrollment::published(&read)? {
        proof.state = "unpublished".into();
        return Ok(());
    }
    let (binding, upper, copied): (String, i64, i64) = read.query_row(
        "SELECT binding,upper_seq,copied_through FROM mail_source_snapshot WHERE only=1 AND upper_seq>=0", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    ensure!(binding == bound, "stale_generation");
    let instance = enrollment::published_nonce(&read, bound)?;
    proof.nonce = instance.clone();
    proof.baseline_upper = Some(upper);
    let head: i64 = read.query_row(
        "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name='mail_source_outbox'),0)",
        [],
        |r| r.get(0),
    )?;
    proof.delta_upper = Some(head);
    if source::lost(&read)? {
        proof.state = "history_gap".into();
        return Ok(());
    }
    // Legacy receipt reclamation could discard unresolved observations, but
    // never the home's origin pin. Check that surviving evidence, not just
    // registry receipts. The partial index is installed only by idle intake
    // setup; missing schema fails closed rather than scanning mail on contact.
    // Enrolled payload pins can outlive history.prune: only extant mail blocks
    // proof, so resolving the pin OR deleting the mail heals this obstacle.
    let unresolved: bool = read.query_row(
        "SELECT EXISTS(SELECT 1 FROM mail_source_origin AS origin INDEXED BY mail_source_origin_unresolved
         WHERE provenance IN ('\"Unresolved\"','\"Local\"')
         AND EXISTS(SELECT 1 FROM mail WHERE mail.id=origin.id))",
        [],
        |r| r.get(0),
    )?;
    if unresolved {
        proof.state = "unresolved".into();
        return Ok(());
    }
    if let Some(instance) = instance {
        // Open read-only: reconciliation may not create/migrate a sidecar or
        // acquire a home writer lock. Reject insecure/symlinked sidecars too.
        let path = enrollment::path(path);
        crate::store::reject_symlinks(&path)?;
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            std::fs::metadata(&path)?.permissions().mode() & 0o077 == 0,
            "unreadable: baseline permissions"
        );
        let sidecar =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let _budget = transport::Budget::new(&sidecar, deadline)?;
        let frozen = rusqlite::Transaction::new_unchecked(&sidecar, TransactionBehavior::Deferred)?;
        ensure!(
            enrollment::frozen(&frozen, bound, &instance)? == Some(upper),
            "integrity_gap: baseline instance"
        );
        if frozen.query_row(
            "SELECT EXISTS(SELECT 1 FROM mail_source_import WHERE receipt IS NULL)",
            [],
            |r| r.get::<_, bool>(0),
        )? {
            proof.state = "baseline_pending".into();
            return Ok(());
        }
    }
    let pending: bool = read.query_row(
        "SELECT EXISTS(SELECT 1 FROM mail_source_import WHERE receipt IS NULL)",
        [],
        |r| r.get(0),
    )?;
    proof.state = if copied != head || pending {
        "delta_pending"
    } else {
        "imported"
    }
    .into();
    Ok(())
}

/// A bounded complete enumeration of *retained* local homes, including retired
/// registrations. Missing/skipped/unreadable homes and inventory loss remain
/// in the vector. No network, Store::open, schema setup or home writes.
/// The caller serializes registry imports while taking these home snapshots;
/// later home writes are outside the vector, not proof of a fresh barrier.
pub(super) fn inspect(
    conn: &Connection,
    m: &Manifest,
    source: &str,
    skipped: &[std::path::PathBuf],
    deadline: Instant,
) -> Result<Proof> {
    participant(m, source)?;
    let _budget = transport::Budget::new(conn, deadline)?;
    let read = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
    let conn = &read;
    let census = home::census_until(conn, skipped, Some(deadline))?;
    let inventory = crate::core::hash(&json!(
        census
            .homes
            .iter()
            .map(|h| (h.path.to_string_lossy().to_string(), h.retired))
            .collect::<Vec<_>>()
    ));
    let mut homes = Vec::new();
    let bound = format!("{}:{source}", binding(m)?);
    for home in census.homes {
        check(deadline)?;
        let mut proof = HomeProof {
            home: crate::core::hash(&json!(home.path)),
            retired: home.retired,
            state: home.state.into(),
            mailbox: None,
            nonce: None,
            baseline_upper: None,
            delta_upper: None,
            unread: None,
            open: None,
            facts_cached: false,
        };
        if home.state == "retained" {
            if let Err(error) = home_proof(&home.path, &bound, &mut proof, deadline, true) {
                proof.state = if error.to_string().starts_with("integrity_gap") {
                    "integrity_gap"
                } else if error.to_string().starts_with("stale_generation") {
                    "stale_generation"
                } else {
                    "unreadable"
                }
                .into();
            }
        }
        homes.push(proof);
    }
    check(deadline)?;
    let local: Vec<String> = conn
        .prepare("SELECT name FROM ships WHERE retired_ms IS NULL ORDER BY name")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut expected = m
        .sources
        .iter()
        .find(|s| s.core == source)
        .context("unknown source")?
        .ships
        .clone();
    expected.sort();
    // Unknown/transitive directory ships and unpinned linked peers cannot be
    // silently absorbed into the fixed manifest. Inventory is not agreement.
    // A matching ship name is insufficient: a transitive/unpinned peer may
    // advertise the same name as an authorized participant. Check both core
    // membership and the ship's participant binding, as on desk status reads.
    let topology_proved = super::completion::topology_proved(conn, m)?;
    let unresolved: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM desk_home_receipt WHERE domain=?1 AND source=?2 AND (json_extract(event,'$.provenance') IN ('Unresolved','Local'))) OR EXISTS(SELECT 1 FROM desk_home_copy WHERE domain=?1 AND source=?2)", params![m.domain, source], |r| r.get(0))?;
    // Imported home pointers cannot discharge registry-side loss (for
    // example a mismatched reclamation receipt). All retained gap kinds
    // remain conservative until explicit reconciliation proves them healed.
    let registry_gap: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM desk_intake_gap WHERE domain=?1 AND source=?2)",
        params![m.domain, source],
        |r| r.get(0),
    )?;
    let membership_proved = !census.membership_history_gap && local == expected && topology_proved;
    let (copies, copies_complete) = super::proof::copies(conn, m, source)?;
    let unresolved_origin: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM desk_home_receipt WHERE domain=?1 AND source=?2 AND json_extract(event,'$.provenance') IN ('Unresolved','Local'))", params![m.domain,source], |r| r.get(0))?;
    let loss: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM desk_intake_loss WHERE domain=?1 AND source=?2)",
        params![m.domain, source],
        |r| r.get(0),
    )?;
    let registry_stamp = super::proof::registry_stamp(conn, m, source)?;
    Ok(Proof {
        candidate: registry_stamp.is_some()
            && membership_proved
            && !loss
            && !unresolved_origin
            && copies_complete
            && homes.iter().all(|h| h.state == "imported"),
        registry_stamp,
        copies,
        binding: binding(m)?,
        source: source.into(),
        source_upper: retention::read_head(conn, m, source)?,
        inventory,
        membership_history_gap: census.membership_history_gap,
        membership_proved,
        history_proved: membership_proved
            && !loss
            && !registry_gap
            && !unresolved
            && homes.iter().all(|h| h.state == "imported"),
        homes,
    })
}

pub(super) fn load(conn: &Connection, m: &Manifest, source: &str) -> Result<Option<Proof>> {
    // Do not deserialize a history-sized idle vector on a mail contact.
    let raw: Option<String> = conn
        .query_row(
            "SELECT CASE WHEN length(CAST(proof AS BLOB))<=65536 THEN proof END FROM desk_source_reconciliation WHERE domain=?1 AND source=?2",
            params![m.domain, source],
            |r| r.get(0),
        )
        .optional()?.flatten();
    raw.map(|raw| Ok(serde_json::from_str(&raw)?)).transpose()
}

pub(super) fn bound(p: &Proof, m: &Manifest, source: &str, upper: i64) -> Result<bool> {
    Ok(p.binding == binding(m)?
        && p.source == source
        && p.source_upper == upper
        && p.homes.len() <= 50
        && p.membership_proved
        && !p.membership_history_gap
        && p.homes
            .iter()
            .all(|h| h.state == "imported" && !h.facts_cached))
}

/// No mail-row or lifecycle-count scan on a contact. Recheck only the saved
/// inventory and each home's published instance, health and durable delta head.
/// These are a vector of home snapshots, not a global simultaneous instant.
/// Later home writes are deliberately outside this barrier's observation.
pub(super) fn revalidate(
    conn: &Connection,
    m: &Manifest,
    source: &str,
    p: &Proof,
    deadline: Instant,
) -> Result<bool> {
    check(deadline)?;
    Ok(revalidate_step(conn, m, source, p, deadline, 0)?.0)
}

/// A resumable sweep checks the same immutable vector, registry revision and
/// source head on every attempt. Earlier home checks are dated observations,
/// not simultaneous snapshots; outgoing evidence still gets a full contact
/// revalidation before it can be advertised as valid.
pub(super) fn revalidate_step(
    conn: &Connection,
    m: &Manifest,
    source: &str,
    p: &Proof,
    deadline: Instant,
    start: usize,
) -> Result<(bool, usize)> {
    if !p.candidate
        || !bound(p, m, source, retention::read_head(conn, m, source)?)?
        || p.registry_stamp.is_none()
        || p.registry_stamp != super::proof::registry_stamp(conn, m, source)?
    {
        return Ok((false, 0));
    }
    let paths = conn.prepare("SELECT home,MAX(active) FROM (SELECT home,retired_ms IS NULL AS active FROM ships UNION ALL SELECT home,0 FROM desk_home_inventory) GROUP BY home ORDER BY home LIMIT 51")?
        .query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,bool>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    if paths.len() != p.homes.len() || start > paths.len() {
        return Ok((false, 0));
    }
    let bound = format!("{}:{source}", binding(m)?);
    for (index, ((path, active), old)) in paths.into_iter().zip(&p.homes).enumerate().skip(start) {
        #[cfg(test)]
        let exhausted =
            HOME_BUDGET.with(|budget| budget.get().is_some_and(|limit| index - start >= limit));
        #[cfg(not(test))]
        let exhausted = false;
        if exhausted || source::work_now() >= deadline {
            return Ok((false, index));
        }
        if old.home != crate::core::hash(&json!(path))
            || old.retired == active
            || old.state != "imported"
            || old.facts_cached
        {
            return Ok((false, 0));
        }
        let mut new = old.clone();
        if home_proof(Path::new(&path), &bound, &mut new, deadline, false).is_err() {
            return Ok((
                false,
                if source::work_now() >= deadline {
                    index
                } else {
                    0
                },
            ));
        }
        if new != *old {
            return Ok((false, 0));
        }
    }
    Ok((true, 0))
}

pub(super) fn save(conn: &Connection, m: &Manifest, source: &str, deadline: Instant) -> Result<()> {
    let mut proof = inspect(conn, m, source, &[], deadline)?;
    // Quiet reconciliation is read-only, even with a different writer active.
    let old: Option<String> = conn
        .query_row(
            "SELECT proof FROM desk_source_reconciliation WHERE domain=?1 AND source=?2",
            params![m.domain, source],
            |r| r.get(0),
        )
        .optional()?;
    // An unavailable home cannot erase its last known unread/open facts.
    // These counts are explicitly cached, never evidence of current emptiness.
    if let Some(previous) = old
        .as_deref()
        .map(serde_json::from_str::<Proof>)
        .transpose()?
    {
        let previous: std::collections::BTreeMap<_, _> = previous
            .homes
            .into_iter()
            .map(|home| (home.home.clone(), home))
            .collect();
        for home in &mut proof.homes {
            check(deadline)?;
            if home.unread.is_none()
                && home.open.is_none()
                && let Some(old) = previous.get(&home.home)
            {
                home.unread = old.unread;
                home.open = old.open;
                home.facts_cached = home.unread.is_some() || home.open.is_some();
            }
        }
    }
    let body = serde_json::to_string(&proof)?;
    if old.as_deref() != Some(&body) {
        conn.execute("INSERT INTO desk_source_reconciliation(domain,source,proof) VALUES(?1,?2,?3) ON CONFLICT(domain,source) DO UPDATE SET proof=excluded.proof", params![m.domain,source,body])?;
    }
    Ok(())
}
