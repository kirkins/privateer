//! Bounded optional frames on the existing collect contact, in either legal
//! intake direction. No RPC, route, reader or ordinary mail receipt is added.
//! All local custody commits precede receipts; lost frames retry durable pages.
use super::*;
use retention::replay::wire::returns as cut_returns;
use std::{
    fs,
    os::fd::AsRawFd,
    path::PathBuf,
    time::{Duration, Instant},
};

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;

const PROTOCOL: &str = "fleet.door-mail.v1";
const WIRE_BYTES: usize = 192 * 1024;
const WORK_MS: u64 = 75;
/// Test-feature builds only, and far under the 30 s link timeout bounding
/// the whole enclosing exchange the intake work rides.
#[cfg(feature = "test-intake-budget")]
const TEST_BUDGET_CEILING_MS: u64 = 10_000;

/// The per-contact wall-clock work budget. A production build has no
/// override at all — the read below is compiled only into test-feature
/// builds (CI enables the feature through `--all-features`; releases never
/// do) — so there the bound is the fixed WORK_MS. In those test builds the
/// fixture suites, whose contacts are real gateway subprocesses on loaded
/// CI runners, may widen the headroom for their own runs through the
/// environment (`PRIVATEER_TEST_DESK_INTAKE_BUDGET_MS`) so a slow machine
/// cannot turn a quiet, in-sync contact into budget backpressure.
#[cfg(feature = "test-intake-budget")]
fn work_ms() -> u64 {
    budget_ms(
        std::env::var("PRIVATEER_TEST_DESK_INTAKE_BUDGET_MS")
            .ok()
            .as_deref(),
    )
}
/// Without the test feature the budget is fixed, with no environment read.
#[cfg(not(feature = "test-intake-budget"))]
const fn work_ms() -> u64 {
    WORK_MS
}
/// Only a value at or above the production bound and inside the ceiling is
/// honoured: the knob can widen headroom, never quietly tighten production
/// or remove the bound. Anything else falls back to the production budget.
#[cfg(feature = "test-intake-budget")]
fn budget_ms(value: Option<&str>) -> u64 {
    value
        .and_then(|ms| ms.parse::<u64>().ok())
        .filter(|ms| (WORK_MS..=TEST_BUDGET_CEILING_MS).contains(ms))
        .unwrap_or(WORK_MS)
}
/// A fresh deadline for one contact's optional intake work. Wall-clock by
/// design: this bounds cooperative work riding an ordinary mail contact,
/// whose enclosing exchange has its own timeout; expiry is reported as
/// backpressure and the durable page retries on a later contact.
pub(super) fn contact_deadline() -> Instant {
    Instant::now() + Duration::from_millis(work_ms())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ack {
    through: i64,
    receipt: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame {
    protocol: String,
    binding: String,
    source: String,
    direction: String,
    role: String,
    page: Option<Page>,
    ack: Option<Ack>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    proof: Option<proof::Evidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replay: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cut_returns: Option<Value>,
    complete: bool,
}

// The error envelope needs no placement data: a busy lock or expired budget
// can prevent reading it. Both peers recognize this strict schema as partial.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialFrame {
    protocol: String,
    status: PartialStatus,
    complete: bool,
}
#[derive(Debug, Serialize, Deserialize)]
enum PartialStatus {
    #[serde(rename = "partial")]
    Partial,
}
fn partial_frame() -> Value {
    json!({"protocol":PROTOCOL,"status":"partial","complete":false})
}

// VM work and Rust loops are bounded cooperatively. Filesystem I/O/fsync is
// not a hard real-time guarantee. Never inherit the ordinary mail lock wait.
pub(super) struct Budget<'a> {
    conn: &'a Connection,
    busy_ms: u64,
}
impl<'a> Budget<'a> {
    pub(super) fn new(conn: &'a Connection, deadline: Instant) -> Result<Self> {
        let busy_ms = conn.query_row("PRAGMA busy_timeout", [], |r| r.get(0))?;
        conn.busy_timeout(Duration::ZERO)?;
        conn.progress_handler(1000, Some(move || Instant::now() >= deadline));
        Ok(Self { conn, busy_ms })
    }
}
impl Drop for Budget<'_> {
    fn drop(&mut self) {
        self.conn.progress_handler(0, None::<fn() -> bool>);
        let _ = self.conn.busy_timeout(Duration::from_millis(self.busy_ms));
    }
}
fn check(deadline: Instant) -> Result<()> {
    ensure!(
        Instant::now() < deadline,
        "backpressure: intake time budget"
    );
    Ok(())
}
pub(super) fn serial() -> Result<fs::File> {
    let f = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(crate::fleet::registry_path()?.with_extension("desk-intake.lock"))?;
    ensure!(
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "intake exchange busy"
    );
    Ok(f)
}

/// Best-effort bounded audit, including negotiation/omission/receipt failures.
/// The independent stderr diagnostic remains if the registry is unavailable.
pub(in crate::machines) fn audit(
    conn: &Connection,
    peer: &str,
    stage: &str,
    error: &anyhow::Error,
) {
    let deadline = Instant::now() + Duration::from_millis(5);
    if let Ok(_budget) = Budget::new(conn, deadline) {
        audit_in_budget(conn, peer, stage, error);
    } else {
        eprintln!("privateer: desk intake: {peer} {stage}: {error:#}");
    }
}
// Optional-sidecar errors occur *inside* a contact. Reuse its existing zero
// lock wait and VM deadline; nesting a Budget would clear the outer handler
// when the inner guard drops and leave subsequent page work unbounded.
pub(super) fn audit_in_budget(conn: &Connection, peer: &str, stage: &str, error: &anyhow::Error) {
    let body = json!({"peer":peer,"stage":stage,"error":format!("{error:#}").chars().take(1000).collect::<String>()});
    // Page transactions finish before proof work, and contact errors are
    // audited after unwinding their work. Still refuse an outer transaction:
    // an interrupted SQLite write could roll back the caller's work too.
    if !conn.is_autocommit() {
        eprintln!("privateer: desk intake: {body}");
        return;
    }
    let inserted = (|| -> rusqlite::Result<usize> {
        // Serialize writers, and commit the insert and trim together. Neither
        // an interrupted trim nor a competing audit may leave us over the cap.
        // Drop rolls back on every error (including a failed commit), unless
        // SQLite has already rolled back on interruption. Reuse the Budget.
        let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        let inserted = tx.execute(
            "INSERT INTO desk_intake_audit(peer,stage,body,at_ms)
                SELECT ?1,?2,?3,?4 WHERE NOT EXISTS
                (SELECT 1 FROM desk_intake_audit WHERE peer=?1 AND stage=?2 AND body=?3)",
            params![peer, stage, body.to_string(), crate::store::Store::now_ms()],
        )?;
        if inserted != 0 {
            tx.execute("DELETE FROM desk_intake_audit WHERE seq NOT IN (SELECT seq FROM desk_intake_audit ORDER BY seq DESC LIMIT 256)", [])?;
        }
        tx.commit()?;
        Ok(inserted)
    })();
    if matches!(inserted, Ok(0)) {
        return; // Recurring failures neither evict diagnostics nor spam stderr.
    }
    eprintln!("privateer: desk intake: {body}");
}

struct Contact {
    manifest: Manifest,
    source: String,
    owner: bool,
    direction: String,
    _fence: fs::File,
}
fn contact(conn: &Connection, peer: &str, initiating: bool) -> Result<Option<Contact>> {
    // Unenrolled installations do not acquire a fence or create intake state.
    if super::super::desk_placement::routing(conn)?.is_none() {
        return Ok(None);
    }
    let (fence, manifest) = super::super::desk_placement::intake_manifest(conn)?;
    let local = crate::door::reader_link::machine_id_in(conn)?;
    let remote: Option<String> = conn
        .query_row(
            "SELECT core FROM desk_placement_peer WHERE machine=?1",
            [peer],
            |r| r.get(0),
        )
        .optional()?;
    let Some(remote) = remote else {
        return Ok(None);
    };
    let owner = local == manifest.owner_core;
    let source = if owner { remote.clone() } else { local };
    // A contact to a third participant cannot relay source data to the owner.
    if !owner && remote != manifest.owner_core {
        return Ok(None);
    }
    let Some(s) = manifest.sources.iter().find(|s| s.core == source) else {
        return Ok(None);
    };
    let legal = match s.intake_direction.as_str() {
        "collect" => owner == initiating,
        "push" => owner != initiating,
        _ => false,
    };
    ensure!(legal, "illegal intake contact direction");
    let direction = s.intake_direction.clone();
    Ok(Some(Contact {
        manifest,
        source,
        owner,
        direction,
        _fence: fence,
    }))
}
fn state(conn: &Connection, c: &Contact) -> Result<(i64, Option<String>)> {
    let m = &c.manifest;
    let bound = binding(m)?;
    let current: Option<(String, i64, Option<String>)> = conn.query_row(
        "SELECT binding,after_seq,pending FROM desk_intake_transport WHERE domain=?1 AND source=?2",
        params![m.domain, c.source], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    if let Some((old, after, pending)) = current
        && old == bound
    {
        return Ok((after, pending));
    }
    conn.execute(
        "INSERT INTO desk_intake_transport(domain,source,binding) VALUES(?1,?2,?3)
         ON CONFLICT(domain,source) DO UPDATE SET binding=excluded.binding,
         after_seq=0,pending=NULL,home=NULL,supported=0,receipted=0,last_receipt=NULL
         WHERE binding<>excluded.binding",
        params![m.domain, c.source, bound],
    )?;
    let (old, after, pending): (String, i64, Option<String>) = conn.query_row(
        "SELECT binding,after_seq,pending FROM desk_intake_transport WHERE domain=?1 AND source=?2",
        params![m.domain, c.source],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    ensure!(old == bound, "stale_generation");
    Ok((after, pending))
}
fn save(conn: &Connection, c: &Contact, after: i64, pending: &str) -> Result<()> {
    conn.execute("UPDATE desk_intake_transport SET after_seq=?3,pending=?4,receipted=0 WHERE domain=?1 AND source=?2",
        params![c.manifest.domain,c.source,after,pending])?;
    Ok(())
}
fn frame(c: &Contact) -> Result<Frame> {
    Ok(Frame {
        protocol: PROTOCOL.into(),
        binding: binding(&c.manifest)?,
        source: c.source.clone(),
        direction: c.direction.clone(),
        role: if c.owner { "owner" } else { "source" }.into(),
        page: None,
        ack: None,
        proof: None,
        replay: None,
        cut_returns: None,
        complete: false,
    })
}
fn validate(c: &Contact, wire: &Value) -> Result<Frame> {
    ensure!(
        !wire.is_null(),
        "unsupported: peer has no desk intake protocol"
    );
    ensure!(
        json_size(wire)
            <= WIRE_BYTES + 4096 + proof::WIRE + retention::replay::wire::WIRE + cut_returns::WIRE,
        "backpressure: intake frame bytes"
    );
    if wire.get("status").is_some() {
        let partial: PartialFrame = serde_json::from_value(wire.clone())
            .context("unsupported: intake partial negotiation")?;
        ensure!(
            partial.protocol == PROTOCOL && !partial.complete,
            "unsupported: inconsistent intake partial negotiation"
        );
        anyhow::bail!("backpressure: peer intake partial");
    }
    let f: Frame =
        serde_json::from_value(wire.clone()).context("unsupported: intake negotiation")?;
    ensure!(
        f.protocol == PROTOCOL
            && f.binding == binding(&c.manifest)?
            && f.source == c.source
            && f.direction == c.direction
            && f.role == if c.owner { "source" } else { "owner" }
            && !f.complete,
        "unsupported: inconsistent intake negotiation"
    );
    ensure!(
        if c.owner {
            f.ack.is_none()
        } else {
            f.page.is_none()
        },
        "invalid intake operation"
    );
    Ok(f)
}

/// Capture only one already-existing home per contact, rotating fairly.
/// Membership proof and history completion belong to 2b-iv(b).
fn capture_one(conn: &Connection, c: &Contact, deadline: Instant) -> Result<()> {
    // Membership is deliberately unproved in (a). Do not open the whole
    // census on a mail contact: select just one retained path, including old
    // and retired registrations, and let the SQL VM budget bound the scan.
    if !conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM desk_intake_gap WHERE domain=?1 AND source=?2)",
        params![c.manifest.domain, c.source],
        |r| r.get::<_, bool>(0),
    )? {
        obstacle(conn, &c.manifest, &c.source, "history_gap")?;
    }
    let previous: Option<String> = conn.query_row(
        "SELECT home FROM desk_intake_transport WHERE domain=?1 AND source=?2",
        params![c.manifest.domain, c.source],
        |r| r.get(0),
    )?;
    let next: Option<String> = conn
        .query_row(
            "SELECT home FROM (SELECT home FROM ships UNION SELECT home FROM desk_home_inventory)
         ORDER BY home<=COALESCE(?1,''),home LIMIT 1",
            [previous.as_deref()],
            |r| r.get(0),
        )
        .optional()?;
    let Some(path) = next else {
        return Ok(());
    };
    check(deadline)?;
    // Advance fairness even when this home is unreadable or exceeds budget.
    if previous.as_ref() != Some(&path) {
        conn.execute(
            "UPDATE desk_intake_transport SET home=?3 WHERE domain=?1 AND source=?2",
            params![c.manifest.domain, c.source, path],
        )?;
    }
    let path = PathBuf::from(path);
    crate::store::reject_symlinks(&path)?;
    let db = path.join("privateer.sqlite");
    crate::store::reject_symlinks(&db)?;
    let home = Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let _budget = Budget::new(&home, deadline)?;
    check(deadline)?;
    home::capture_ready_until(&path, &home, conn, &c.manifest, &c.source, deadline)?;
    Ok(())
}
fn source_frame(conn: &Connection, c: &Contact, ready: bool, deadline: Instant) -> Result<Frame> {
    source_frame_at(conn, c, ready, deadline, WIRE_BYTES, scheduling_wall_ms())
}
fn source_frame_at(
    conn: &Connection,
    c: &Contact,
    ready: bool,
    deadline: Instant,
    bytes: usize,
    now: i64,
) -> Result<Frame> {
    let mut f = frame(c)?;
    let (after, pending) = state(conn, c)?;
    if !ready {
        return Ok(f);
    } // explicit offer before any push capture
    let mut completed = None;
    if let Some(raw) = pending {
        let old: Page = serde_json::from_str(&raw)?;
        let receipted: bool = conn.query_row(
            "SELECT receipted FROM desk_intake_transport WHERE domain=?1 AND source=?2",
            params![c.manifest.domain, c.source],
            |r| r.get(0),
        )?;
        if !receipted {
            f.proof = proof::outgoing(conn, &c.manifest, &old.checkpoint, deadline).unwrap_or(None);
            f.page = Some(old); // unreceipted custody is immutable across restart
            return Ok(f);
        }
        if old.through < old.checkpoint.upper {
            let next = page(conn, &c.manifest, &old.checkpoint, after, ITEMS, bytes)?;
            save(conn, c, after, &serde_json::to_string(&next)?)?;
            f.proof =
                proof::outgoing(conn, &c.manifest, &next.checkpoint, deadline).unwrap_or(None);
            f.page = Some(next);
            return Ok(f);
        }
        completed = Some(old);
    }
    // A revalidated completed vector proves that every home is already
    // imported. Do not rotate capture/cleanup cursors on quiet contacts,
    // including multi-home sources. Redundant custody remains safe to reclaim
    // when work next advances; periodic push barriers still run as scheduled.
    if let Some(old) = &completed {
        let evidence =
            proof::outgoing(conn, &c.manifest, &old.checkpoint, deadline).unwrap_or(None);
        if evidence.as_ref().is_some_and(|e| e.is_valid())
            && !refresh_due(conn, c, now)?
            && !proof::changed(conn, &c.manifest, &old.checkpoint, deadline).unwrap_or(false)
        {
            f.proof = evidence;
            return Ok(f);
        }
    }
    capture_one(conn, c, deadline)?;
    check(deadline)?;
    if let Some(old) = &completed {
        retention::acknowledged(conn, &c.manifest, old, deadline)?;
        check(deadline)?;
        let upper = retention::head(conn, &c.manifest, &c.source)?;
        let due = refresh_due(conn, c, now)?;
        if upper == old.checkpoint.upper
            && !due
            && !proof::changed(conn, &c.manifest, &old.checkpoint, deadline).unwrap_or(false)
        {
            // No new page is needed. Resume preparation for the completed
            // barrier only after capture/page admission decisions, so proof
            // work cannot consume the budget a new page would need.
            if let Err(error) = proof::issue(conn, &c.manifest, &old.checkpoint, deadline) {
                audit_in_budget(conn, &c.source, "proof.prepare", &error);
            }
            f.proof = proof::outgoing(conn, &c.manifest, &old.checkpoint, deadline).unwrap_or(None);
            return Ok(f); // no page to re-send/re-hash, no freshness claim
        }
    }
    let result = (|| {
        let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        if let Some(old) = completed {
            // Only this final page's verified receipt authorizes reclamation.
            // Free its slot before admission, even on an already-full journal.
            // SQLite's issuance high-water and the binding-bound source head
            // survive reclamation; unproved/pending source custody remains.
            tx.execute("DELETE FROM desk_source_barrier WHERE domain=?1 AND source=?2 AND id=?3 AND binding=?4",
                params![c.manifest.domain,c.source,old.checkpoint.id,old.checkpoint.binding])?;
            // Reclaim this exact final issuance even when another interrupted
            // barrier pins the log. Its AUTOINCREMENT fence is not reset.
            tx.execute(
                "DELETE FROM desk_source_proof WHERE domain=?1 AND source=?2 AND barrier=?3",
                params![c.manifest.domain, c.source, old.checkpoint.id],
            )?;
            tx.execute(
                "DELETE FROM desk_source_issuance WHERE domain=?1 AND source=?2 AND id=?3",
                params![c.manifest.domain, c.source, old.checkpoint.id],
            )?;
        }
        let cp = issue_checkpoint(
            &tx,
            &c.manifest,
            &c.source,
            &uuid::Uuid::new_v4().to_string(),
        )?;
        let p = page_inner(&tx, &c.manifest, &cp, after, ITEMS, bytes)?;
        tx.execute(
            "UPDATE desk_source_head SET issued_at=?3 WHERE domain=?1 AND source=?2",
            params![c.manifest.domain, c.source, now],
        )?;
        save(&tx, c, after, &serde_json::to_string(&p)?)?;
        tx.commit()?;
        Ok(p)
    })();
    record_failure(conn, &c.manifest, &c.source, &result)?;
    let p = result?;
    // The durable page cannot roll back or starve on optional proof work.
    if let Err(error) = proof::issue(conn, &c.manifest, &p.checkpoint, deadline) {
        audit_in_budget(conn, &c.source, "proof.prepare", &error);
    }
    f.proof = proof::outgoing(conn, &c.manifest, &p.checkpoint, deadline).unwrap_or(None);
    f.page = Some(p);
    Ok(f)
}
// Source wall time schedules work only, never freshness proof. Desk clock
// continuity and a new completed receipt still gate freshness.
fn refresh_due(conn: &Connection, c: &Contact, now: i64) -> Result<bool> {
    if c.direction != "push" {
        return Ok(false);
    }
    let issued_at: Option<i64> = conn.query_row(
        "SELECT issued_at FROM desk_source_head WHERE domain=?1 AND source=?2",
        params![c.manifest.domain, c.source],
        |r| r.get(0),
    )?;
    Ok(issued_at.is_none_or(|at| {
        now < at || now.saturating_sub(at) >= (c.manifest.max_source_age_ms / 2).max(1)
    }))
}

fn owner_frame(conn: &Connection, c: &Contact) -> Result<Frame> {
    let mut f = frame(c)?;
    let (_, pending) = state(conn, c)?;
    f.ack = pending.map(|raw| serde_json::from_str(&raw)).transpose()?;
    Ok(f)
}
#[cfg(test)]
fn receive(conn: &Connection, c: &Contact, f: Frame) -> Result<()> {
    receive_at(conn, c, f, contact_deadline())
}
fn receive_at(conn: &Connection, c: &Contact, f: Frame, deadline: Instant) -> Result<()> {
    if !c.owner {
        receive_replay(conn, c, f.replay.as_ref());
    }
    let (after, pending) = state(conn, c)?;
    if !conn.query_row(
        "SELECT supported=1 FROM desk_intake_transport WHERE domain=?1 AND source=?2",
        params![c.manifest.domain, c.source],
        |r| r.get::<_, bool>(0),
    )? {
        conn.execute(
            "UPDATE desk_intake_transport SET supported=1 WHERE domain=?1 AND source=?2",
            params![c.manifest.domain, c.source],
        )?;
    }
    if c.owner {
        if let Some(p) = f.page {
            ensure!(
                p.checkpoint.source == c.source,
                "integrity_gap: wrong page source"
            );
            let clock = process_clock();
            accept(conn, &c.manifest, &p, &clock)?;
            // Separate commit is intentional. Crash here replays the same page
            // and obtains the same hash; ordinary mail is already delivered.
            let ack = Ack {
                through: p.through,
                receipt: crate::core::hash(&serde_json::to_value(&p)?),
            };
            let latest: String = conn.query_row(
                "SELECT barrier FROM desk_intake_order WHERE domain=?1 AND source=?2",
                params![c.manifest.domain, c.source],
                |r| r.get(0),
            )?;
            if latest == p.checkpoint.id && p.through >= after {
                save(conn, c, p.through, &serde_json::to_string(&ack)?)?;
            }
        }
        receive_proof(conn, c, f.proof);
        receive_replay(conn, c, f.replay.as_ref());
    } else if let Some(ack) = f.ack {
        let last: Option<String> = conn.query_row(
            "SELECT last_receipt FROM desk_intake_transport WHERE domain=?1 AND source=?2",
            params![c.manifest.domain, c.source],
            |r| r.get(0),
        )?;
        // A collect request may repeat the prior receipt while our next page
        // is still unaccepted. Never confuse it with custody of that page.
        if ack.through == after && last.as_deref() == Some(&ack.receipt) {
            receive_cuts(conn, c, f.cut_returns.as_ref(), deadline);
            return Ok(());
        }
        let raw = pending.context("stale_or_unknown: no pending source page")?;
        let p: Page = serde_json::from_str(&raw)?;
        ensure!(
            ack.through == p.through
                && ack.receipt == crate::core::hash(&serde_json::to_value(&p)?),
            "integrity_gap: invalid intake receipt"
        );
        ensure!(ack.through >= after, "stale_or_unknown: regressed receipt");
        save(conn, c, ack.through, &raw)?;
        conn.execute("UPDATE desk_intake_transport SET receipted=1,last_receipt=?3 WHERE domain=?1 AND source=?2",
            params![c.manifest.domain,c.source,ack.receipt])?;
    }
    // Home validation is optional and always follows ordinary custody.
    receive_cuts(conn, c, f.cut_returns.as_ref(), deadline);
    Ok(())
}
fn receive_replay(conn: &Connection, c: &Contact, wire: Option<&Value>) {
    if let Some(wire) = wire
        && let Err(error) =
            retention::replay::wire::receive(conn, &c.manifest, &c.source, c.owner, wire)
    {
        audit_in_budget(conn, &c.source, "replay.receive", &error);
    }
}

fn receive_cuts(conn: &Connection, c: &Contact, wire: Option<&Value>, deadline: Instant) {
    if let Some(wire) = wire
        && let Err(error) =
            cut_returns::receive(conn, &c.manifest, &c.source, c.owner, wire, deadline)
    {
        audit_in_budget(conn, &c.source, "replay.cut_return", &error);
    }
}
fn prepare_cuts(conn: &Connection, c: &Contact) -> Value {
    cut_returns::outgoing(conn, &c.manifest, &c.source, c.owner).unwrap_or_else(|error| {
        audit_in_budget(conn, &c.source, "replay.cut_return", &error);
        cut_returns::marker()
    })
}

fn pack(conn: &Connection, c: &Contact, f: Frame, deadline: Instant) -> Result<Value> {
    // Optional work is after ordinary page custody. Any failure emits only
    // support, not a false adoption receipt or a failed ordinary exchange.
    let replay = retention::replay::wire::outgoing(
        conn,
        &c.manifest,
        &c.source,
        c.owner,
        f.proof.as_ref(),
        deadline,
    )
    .unwrap_or_else(|error| {
        audit_in_budget(conn, &c.source, "replay.prepare", &error);
        retention::replay::wire::marker()
    });
    let mut value = serde_json::to_value(f)?;
    value["replay"] = replay;
    value["cut_returns"] = prepare_cuts(conn, c);
    Ok(value)
}

// Sidecar version skew, corruption or a busy optional write cannot turn an
// accepted page into a partial response or skip capability bookkeeping.
fn receive_proof(conn: &Connection, c: &Contact, evidence: Option<proof::Evidence>) {
    if let Err(error) = proof::receive(conn, &c.manifest, &c.source, evidence) {
        let _ = proof::receive(conn, &c.manifest, &c.source, None);
        // Independent fallback when an optional proof-specific write fails.
        // Never let an old valid body survive as fresh eligibility.
        let _ = conn.execute("UPDATE desk_intake_checkpoint SET clock_valid=0 WHERE domain=?1 AND source=?2 AND clock_valid<>0", params![c.manifest.domain,c.source]);
        audit_in_budget(conn, &c.source, "proof.receive", &error);
    }
}
#[cfg(feature = "test-intake-budget")]
fn test_clock() -> Option<[i64; 2]> {
    serde_json::from_str(&std::env::var("PRIVATEER_TEST_DESK_INTAKE_CLOCK").ok()?).ok()
}
fn scheduling_wall_ms() -> i64 {
    #[cfg(feature = "test-intake-budget")]
    if let Some([wall_ms, _]) = test_clock() {
        return wall_ms;
    }
    crate::store::Store::now_ms()
}
pub(super) use super::boot_clock::process_clock;

/// The owner's own homes use the same bounded capture/page/accept protocol,
/// on idle work only. No synthetic remote route or ordinary-mail receipt.
pub(super) fn local(
    conn: &Connection,
    manifest: Manifest,
    source: String,
    fence: fs::File,
    deadline: Instant,
) -> Result<()> {
    let _serial = serial()?;
    let c = Contact {
        manifest,
        source,
        owner: false,
        direction: "local".into(),
        _fence: fence,
    };
    let f = match source_frame_at(conn, &c, true, deadline, WIRE_BYTES, scheduling_wall_ms()) {
        Ok(f) => f,
        Err(error) => {
            let _ = proof::receive(conn, &c.manifest, &c.source, None);
            return Err(error);
        }
    };
    if let Some(page) = f.page {
        if let Err(error) = accept(conn, &c.manifest, &page, &process_clock()) {
            let _ = proof::receive(conn, &c.manifest, &c.source, None);
            return Err(error);
        }
        let mut ack = frame(&c)?;
        ack.ack = Some(Ack {
            through: page.through,
            receipt: crate::core::hash(&serde_json::to_value(&page)?),
        });
        receive_at(conn, &c, ack, deadline)?;
    }
    // Empty local frames carry invalidation too, just like remote contacts.
    let replay = retention::replay::wire::outgoing(
        conn,
        &c.manifest,
        &c.source,
        false,
        f.proof.as_ref(),
        deadline,
    )
    .unwrap_or_else(|error| {
        audit_in_budget(conn, &c.source, "replay.prepare", &error);
        retention::replay::wire::marker()
    });
    receive_proof(conn, &c, f.proof);
    let owner = Contact { owner: true, ..c };
    receive_replay(conn, &owner, Some(&replay));
    let receipts = retention::replay::wire::outgoing(
        conn,
        &owner.manifest,
        &owner.source,
        true,
        None,
        deadline,
    );
    let cuts = prepare_cuts(conn, &owner);
    match receipts {
        Ok(receipts) => {
            let source = Contact {
                owner: false,
                ..owner
            };
            receive_replay(conn, &source, Some(&receipts));
            receive_cuts(conn, &source, Some(&cuts), deadline);
        }
        Err(error) => audit_in_budget(conn, &owner.source, "replay.prepare", &error),
    }
    Ok(())
}

/// Prepare before the contact, with no network under the placement fence.
/// Source pushes initially offer only; later contacts retry a durable page.
pub(in crate::machines) fn outgoing(conn: &Connection, peer: &str) -> Value {
    let result = (|| -> Result<Value> {
        let deadline = contact_deadline();
        let _budget = Budget::new(conn, deadline)?;
        let Some(c) = contact(conn, peer, true)? else {
            return Ok(Value::Null);
        };
        let _serial = serial()?;
        let f = if c.owner {
            owner_frame(conn, &c)?
        } else {
            // A validated response is retained independently of an empty page.
            let ready: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM desk_intake_transport WHERE domain=?1 AND source=?2 AND supported>0)",
                params![c.manifest.domain,c.source], |r| r.get(0))?;
            source_frame(conn, &c, ready, deadline)?
        };
        pack(conn, &c, f, deadline)
    })();
    match result {
        Ok(v) => v,
        Err(e) => {
            audit(conn, peer, "prepare", &e);
            partial_frame()
        }
    }
}
/// Gateway: called only AFTER ordinary mail has been gathered. Errors become
/// partial/unsupported frames, not errors in the enclosing mail exchange.
pub(in crate::machines) fn incoming(
    conn: &Connection,
    peer: &str,
    wire: &Value,
    bytes: usize,
) -> Value {
    let deadline = contact_deadline();
    let result = (|| -> Result<Value> {
        let _budget = Budget::new(conn, deadline)?;
        let Some(c) = contact(conn, peer, false)? else {
            return Ok(Value::Null);
        };
        let _serial = serial()?;
        let f = match validate(&c, wire) {
            Ok(f) => f,
            Err(e) => {
                negotiation_gap(conn, &c, wire)?;
                return Err(e);
            }
        };
        receive_at(conn, &c, f, deadline)?;
        check(deadline)?;
        let reply = if c.owner {
            owner_frame(conn, &c)?
        } else {
            source_frame_at(
                conn,
                &c,
                true,
                deadline,
                bytes.min(WIRE_BYTES),
                scheduling_wall_ms(),
            )?
        };
        pack(conn, &c, reply, deadline)
    })();
    match result {
        Ok(v) => v,
        Err(e) => {
            invalidate_contact(conn, peer, false, deadline);
            audit(conn, peer, "receive", &e);
            partial_frame()
        }
    }
}
// Omission by a previously negotiated peer is partial, not a legacy
// downgrade. Do not overwrite a more specific retained diagnostic.
fn negotiation_gap(conn: &Connection, c: &Contact, wire: &Value) -> Result<()> {
    if c.owner {
        receive_proof(conn, c, None);
    }
    let supported: i64 = conn.query_row("SELECT COALESCE((SELECT supported FROM desk_intake_transport WHERE domain=?1 AND source=?2),0)",
        params![c.manifest.domain,c.source], |r| r.get(0))?;
    let partial = serde_json::from_value::<PartialFrame>(wire.clone())
        .is_ok_and(|f| f.protocol == PROTOCOL && !f.complete);
    if wire.is_null() && supported > 0 {
        // Three consecutive omissions forget capability. A valid contact
        // resets this counter to one. Explicit partial replies never do so.
        conn.execute(
            "UPDATE desk_intake_transport SET supported=?3 WHERE domain=?1 AND source=?2",
            params![
                c.manifest.domain,
                c.source,
                if supported < 3 { supported + 1 } else { 0 }
            ],
        )?;
    }
    let state = if partial || (wire.is_null() && (1..3).contains(&supported)) {
        "backpressure"
    } else {
        "unsupported"
    };
    // Capability loss must not replace a specific history/integrity diagnosis.
    conn.execute(
        "INSERT INTO desk_intake_gap(domain,source,state) VALUES(?1,?2,?3)
        ON CONFLICT(domain,source) DO UPDATE SET state=excluded.state
        WHERE state IN ('backpressure','unsupported')",
        params![c.manifest.domain, c.source, state],
    )?;
    Ok(())
}

/// Failed ordinary contacts invalidate observation eligibility best effort,
/// after mail has already returned its original outcome. No retries or network.
pub(in crate::machines) fn failed_contact(conn: &Connection, peer: &str) {
    invalidate_contact(conn, peer, true, contact_deadline());
}
fn invalidate_contact(conn: &Connection, peer: &str, initiating: bool, deadline: Instant) {
    let _ = (|| -> Result<()> {
        check(deadline)?;
        let _budget = Budget::new(conn, deadline)?;
        if let Some(c) = contact(conn, peer, initiating)?
            && c.owner
        {
            receive_proof(conn, &c, None);
        }
        Ok(())
    })();
}

/// Initiator: called only AFTER mail delivery and fleet.collected receipts.
pub(in crate::machines) fn finish(conn: &Connection, peer: &str, wire: &Value) {
    let deadline = contact_deadline();
    let result = (|| -> Result<()> {
        let _budget = Budget::new(conn, deadline)?;
        let Some(c) = contact(conn, peer, true)? else {
            return Ok(());
        };
        let _serial = serial()?;
        let f = match validate(&c, wire) {
            Ok(f) => f,
            Err(e) => {
                negotiation_gap(conn, &c, wire)?;
                return Err(e);
            }
        };
        receive_at(conn, &c, f, deadline)
    })();
    if let Err(e) = result {
        invalidate_contact(conn, peer, true, deadline);
        audit(conn, peer, "receipt", &e);
    }
}

/// Do not let an optional frame overflow the advertised enclosing response
/// (or the request line). Reserve 1 KiB for the RPC wrapper, including a
/// maximally escaped 128-byte peer correlation ID and newline. Measure the
/// actual ordinary-mail body plus frame, never a fixed mail-only threshold.
/// Pages that do not fit retain source custody; send a small partial frame
/// instead whenever there is room, so size pressure is not a legacy downgrade.
pub(in crate::machines) fn attach(
    conn: &Connection,
    peer: &str,
    body: &mut Value,
    wire: Value,
    limit: usize,
) {
    if wire.is_null() {
        return;
    }
    let limit = limit.min(1_048_576);
    let mut wire = wire;
    let mut proof = wire.as_object_mut().and_then(|f| f.remove("proof"));
    let mut replay = wire
        .as_object_mut()
        .and_then(|f| f.remove("replay"))
        .and_then(|v| retention::replay::wire::decode(&v))
        .unwrap_or_else(retention::replay::wire::marker);
    let mut cuts = wire
        .as_object_mut()
        .and_then(|f| f.remove("cut_returns"))
        .and_then(|v| cut_returns::decode(&v));
    let size = |wire: &Value, proof: &Option<Value>, replay: &Value, cuts: &Option<Value>| {
        json_size(body)
            .saturating_add(json_size(wire))
            .saturating_add(proof.as_ref().map_or(0, json_size))
            .saturating_add(json_size(replay))
            .saturating_add(cuts.as_ref().map_or(0, json_size))
            // All sibling keys, separators, escaping and the complete RPC
            // wrapper, including its injected max_response_bytes, fit here.
            .saturating_add(128 + 1024)
    };
    // New cut returns are optional too; mixed peers retain the identical A
    // sidecar. Nothing removed here relinquishes witness/page/cut custody.
    if size(&wire, &proof, &replay, &cuts) > limit {
        cuts = None;
    }
    if size(&wire, &proof, &replay, &cuts) > limit {
        replay["proof"] = Value::Null;
    }
    if size(&wire, &proof, &replay, &cuts) > limit {
        replay = retention::replay::wire::marker();
    }
    if size(&wire, &proof, &replay, &cuts) > limit {
        proof = None;
    }
    if size(&wire, &proof, &replay, &cuts) > limit {
        audit(
            conn,
            peer,
            "envelope",
            &anyhow::anyhow!("backpressure: no intake envelope room"),
        );
        let partial = partial_frame();
        if json_size(body)
            .saturating_add(json_size(&partial))
            .saturating_add(json_size(&retention::replay::wire::marker()))
            .saturating_add(128 + 1024)
            <= limit
        {
            body["desk_intake"] = partial;
            body[retention::replay::wire::KEY] = retention::replay::wire::marker();
        }
        return;
    }
    if let Some(proof) = proof {
        body["desk_intake_proof"] = proof;
    }
    body[retention::replay::wire::KEY] = replay;
    if let Some(cuts) = cuts {
        body[cut_returns::KEY] = cuts;
    }
    body["desk_intake"] = wire;
}

pub(super) fn json_size(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |v| v.len())
}

/// Non-failing allocation-free upper bound without serializing mail again.
/// Count escapes in every string byte: linear work in the already-bounded
/// ordinary body, without pessimistic expansion that starves large snapshots.
pub(in crate::machines) fn json_size_bound(value: &Value) -> usize {
    match value {
        Value::Null => 4,
        Value::Bool(_) => 5,
        Value::Number(_) => 32,
        Value::String(s) => string_size(s),
        Value::Array(a) => a.iter().fold(2usize, |n, v| {
            n.saturating_add(json_size_bound(v)).saturating_add(1)
        }),
        Value::Object(o) => o.iter().fold(2usize, |n, (k, v)| {
            n.saturating_add(string_size(k))
                .saturating_add(2)
                .saturating_add(json_size_bound(v))
        }),
    }
}
fn string_size(s: &str) -> usize {
    s.as_bytes().iter().fold(2usize, |n, &b| {
        n.saturating_add(match b {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 8 | 12 => 2,
            0..=31 => 6,
            _ => 1,
        })
    })
}
