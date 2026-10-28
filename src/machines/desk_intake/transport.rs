//! Bounded optional frames on the existing collect contact, in either legal
//! intake direction. No RPC, route, reader or ordinary mail receipt is added.
//! All local custody commits precede receipts; lost frames retry durable pages.
use super::*;
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
    complete: bool,
}

// VM work and Rust loops are bounded cooperatively. Filesystem I/O/fsync is
// not a hard real-time guarantee. Never inherit the ordinary mail lock wait.
struct Budget<'a> {
    conn: &'a Connection,
    busy_ms: u64,
}
impl<'a> Budget<'a> {
    fn new(conn: &'a Connection, deadline: Instant) -> Result<Self> {
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
fn serial() -> Result<fs::File> {
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
fn audit(conn: &Connection, peer: &str, stage: &str, error: &anyhow::Error) {
    let body = json!({"peer":peer,"stage":stage,"error":format!("{error:#}").chars().take(1000).collect::<String>()});
    let deadline = Instant::now() + Duration::from_millis(5);
    if let Ok(_budget) = Budget::new(conn, deadline) {
        let inserted = conn.execute(
            "INSERT INTO desk_intake_audit(peer,stage,body,at_ms)
            SELECT ?1,?2,?3,?4 WHERE NOT EXISTS
            (SELECT 1 FROM desk_intake_audit WHERE peer=?1 AND stage=?2 AND body=?3)",
            params![peer, stage, body.to_string(), crate::store::Store::now_ms()],
        );
        if matches!(inserted, Ok(0)) {
            return; // Recurring failures neither evict diagnostics nor spam stderr.
        }
        if inserted.is_ok() {
            let _ = conn.execute("DELETE FROM desk_intake_audit WHERE seq NOT IN (SELECT seq FROM desk_intake_audit ORDER BY seq DESC LIMIT 256)", []);
        }
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
        complete: false,
    })
}
fn validate(c: &Contact, wire: &Value) -> Result<Frame> {
    ensure!(
        !wire.is_null(),
        "unsupported: peer has no desk intake protocol"
    );
    ensure!(
        serde_json::to_vec(wire)?.len() <= WIRE_BYTES + 4096,
        "backpressure: intake frame bytes"
    );
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
    conn.execute(
        "INSERT OR IGNORE INTO desk_intake_gap(domain,source,state) VALUES(?1,?2,'history_gap')",
        params![c.manifest.domain, c.source],
    )?;
    let previous: Option<String> = conn.query_row(
        "SELECT home FROM desk_intake_transport WHERE domain=?1 AND source=?2",
        params![c.manifest.domain, c.source],
        |r| r.get(0),
    )?;
    let next: Option<String> = conn
        .query_row(
            "SELECT home FROM (SELECT home FROM ships UNION SELECT home FROM desk_home_inventory)
         ORDER BY home<=COALESCE(?1,''),home LIMIT 1",
            [previous],
            |r| r.get(0),
        )
        .optional()?;
    let Some(path) = next else {
        return Ok(());
    };
    check(deadline)?;
    // Advance fairness even when this home is unreadable or exceeds budget.
    conn.execute(
        "UPDATE desk_intake_transport SET home=?3 WHERE domain=?1 AND source=?2",
        params![c.manifest.domain, c.source, path],
    )?;
    let path = PathBuf::from(path);
    crate::store::reject_symlinks(&path)?;
    let db = path.join("privateer.sqlite");
    crate::store::reject_symlinks(&db)?;
    let home = Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let _budget = Budget::new(&home, deadline)?;
    check(deadline)?;
    home::capture_until(&home, conn, &c.manifest, &c.source, Some(deadline))?;
    Ok(())
}
fn source_frame(conn: &Connection, c: &Contact, ready: bool, deadline: Instant) -> Result<Frame> {
    source_frame_at(
        conn,
        c,
        ready,
        deadline,
        WIRE_BYTES,
        crate::store::Store::now_ms(),
    )
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
            f.page = Some(old); // unreceipted custody is immutable across restart
            return Ok(f);
        }
        if old.through < old.checkpoint.upper {
            let next = page(conn, &c.manifest, &old.checkpoint, after, ITEMS, bytes)?;
            save(conn, c, after, &serde_json::to_string(&next)?)?;
            f.page = Some(next);
            return Ok(f);
        }
        completed = Some(old);
    }
    capture_one(conn, c, deadline)?;
    check(deadline)?;
    if let Some(old) = &completed {
        retention::acknowledged(conn, &c.manifest, old, deadline)?;
        check(deadline)?;
        let upper = retention::head(conn, &c.manifest, &c.source)?;
        let issued_at: Option<i64> = conn.query_row(
            "SELECT issued_at FROM desk_source_head WHERE domain=?1 AND source=?2",
            params![c.manifest.domain, c.source],
            |r| r.get(0),
        )?;
        // Source wall time schedules work only, never freshness proof. Desk
        // continuity and completed receipt still gate freshness. Restart alone
        // must not spam barriers on every gateway subprocess contact.
        let due = c.direction == "push"
            && issued_at.is_none_or(|at| {
                now < at || now.saturating_sub(at) >= (c.manifest.max_source_age_ms / 2).max(1)
            });
        if upper == old.checkpoint.upper && !due {
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
    f.page = Some(result?);
    Ok(f)
}
fn owner_frame(conn: &Connection, c: &Contact) -> Result<Frame> {
    let mut f = frame(c)?;
    let (_, pending) = state(conn, c)?;
    f.ack = pending.map(|raw| serde_json::from_str(&raw)).transpose()?;
    Ok(f)
}
fn receive(conn: &Connection, c: &Contact, f: Frame) -> Result<()> {
    let (after, pending) = state(conn, c)?;
    conn.execute(
        "UPDATE desk_intake_transport SET supported=1 WHERE domain=?1 AND source=?2",
        params![c.manifest.domain, c.source],
    )?;
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
    } else if let Some(ack) = f.ack {
        let last: Option<String> = conn.query_row(
            "SELECT last_receipt FROM desk_intake_transport WHERE domain=?1 AND source=?2",
            params![c.manifest.domain, c.source],
            |r| r.get(0),
        )?;
        // A collect request may repeat the prior receipt while our next page
        // is still unaccepted. Never confuse it with custody of that page.
        if ack.through == after && last.as_deref() == Some(&ack.receipt) {
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
    Ok(())
}
fn process_clock() -> Clock<'static> {
    static SESSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    Clock {
        session: SESSION.get_or_init(|| uuid::Uuid::new_v4().to_string()),
        wall_ms: crate::store::Store::now_ms(),
        tick_ms: START.get_or_init(Instant::now).elapsed().as_millis() as i64,
    }
}

/// Prepare before the contact, with no network under the placement fence.
/// Source pushes initially offer only; later contacts retry a durable page.
pub(in crate::machines) fn outgoing(conn: &Connection, peer: &str) -> Value {
    let result = (|| -> Result<Value> {
        let deadline = Instant::now() + Duration::from_millis(WORK_MS);
        let _budget = Budget::new(conn, deadline)?;
        let Some(c) = contact(conn, peer, true)? else {
            return Ok(Value::Null);
        };
        let _serial = serial()?;
        let f = if c.owner {
            owner_frame(conn, &c)?
        } else {
            // A validated response is retained independently of an empty page.
            let ready: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM desk_intake_transport WHERE domain=?1 AND source=?2 AND supported=1)",
                params![c.manifest.domain,c.source], |r| r.get(0))?;
            source_frame(conn, &c, ready, deadline)?
        };
        Ok(serde_json::to_value(f)?)
    })();
    match result {
        Ok(v) => v,
        Err(e) => {
            audit(conn, peer, "prepare", &e);
            Value::Null
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
    let result = (|| -> Result<Value> {
        let deadline = Instant::now() + Duration::from_millis(WORK_MS);
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
        receive(conn, &c, f)?;
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
                crate::store::Store::now_ms(),
            )?
        };
        Ok(serde_json::to_value(reply)?)
    })();
    match result {
        Ok(v) => v,
        Err(e) => {
            audit(conn, peer, "receive", &e);
            json!({"protocol":PROTOCOL,"status":"partial","complete":false})
        }
    }
}
// Omission by a previously negotiated peer is partial, not a legacy
// downgrade. Do not overwrite a more specific retained diagnostic.
fn negotiation_gap(conn: &Connection, c: &Contact, wire: &Value) -> Result<()> {
    let supported: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM desk_intake_transport WHERE domain=?1 AND source=?2 AND supported=1)",
        params![c.manifest.domain,c.source], |r| r.get(0))?;
    if supported && (wire.is_null() || wire["status"] == "partial") {
        conn.execute("INSERT OR IGNORE INTO desk_intake_gap(domain,source,state) VALUES(?1,?2,'backpressure')",
            params![c.manifest.domain,c.source])?;
        return Ok(());
    }
    gap(conn, &c.manifest, &c.source, "unsupported")
}

/// Initiator: called only AFTER mail delivery and fleet.collected receipts.
pub(in crate::machines) fn finish(conn: &Connection, peer: &str, wire: &Value) {
    let result = (|| -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(WORK_MS);
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
        receive(conn, &c, f)
    })();
    if let Err(e) = result {
        audit(conn, peer, "receipt", &e);
    }
}

/// Do not let an optional frame overflow the advertised enclosing response
/// (or the request line). Reserve 1 KiB for the RPC wrapper, including a
/// maximally escaped 128-byte peer correlation ID and newline. Measure the
/// actual ordinary-mail body plus frame, never a fixed mail-only threshold.
/// Omitted pages retain source custody; ordinary mail is untouched.
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
    let mut candidate = body.clone();
    candidate["desk_intake"] = wire;
    if serde_json::to_vec(&candidate).map_or(true, |b| b.len().saturating_add(1024) > limit) {
        audit(
            conn,
            peer,
            "envelope",
            &anyhow::anyhow!("backpressure: no intake envelope room"),
        );
        return;
    }
    *body = candidate;
}
