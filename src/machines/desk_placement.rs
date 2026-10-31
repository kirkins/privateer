//! Operator-only generation-one agreement. Frames relay agreement facts, never
//! mail or arbitrary RPC. Peers share operator trust; UUIDs are not credentials.
use crate::store::{Store, identifier, required};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, os::fd::AsRawFd, path::PathBuf};

const LIMIT: usize = 16;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Contact {
    core: String,
    role: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Description {
    core: String,
    name: String,
    ships: Vec<String>,
    contacts: Vec<Contact>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Source {
    pub(crate) core: String,
    pub(crate) name: String,
    pub(crate) ships: Vec<String>,
    pub(crate) intake_direction: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Manifest {
    pub(crate) domain: String,
    pub(crate) manifest_revision: i64,
    pub(crate) max_source_age_ms: i64,
    pub(crate) owner_core: String,
    pub(crate) generation: u64,
    pub(crate) incarnation: String,
    pub(crate) coordinator: String,
    pub(crate) sources: Vec<Source>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Proposal {
    proposal_id: String,
    manifest_hash: String,
    manifest: Manifest,
    decision: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Certificate {
    proposal: Proposal,
    accepted: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
    proposal: Proposal,
    #[serde(default)]
    local_role: String,
    #[serde(default)]
    intake_direction: String,
    approval: Option<bool>,
    accepted: Vec<String>,
    certificate: Option<Certificate>,
    state: String,
}
#[derive(Serialize, Deserialize)]
struct Guard {
    proposal: Proposal,
    certificate: Option<Certificate>,
}

pub(super) fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS desk_placement_name(only INTEGER PRIMARY KEY CHECK(only=1), name TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS desk_placement_peer(machine TEXT PRIMARY KEY, core TEXT NOT NULL, description TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS desk_placement_topology(core TEXT PRIMARY KEY, description TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS desk_placement_record(domain TEXT PRIMARY KEY, proposal TEXT NOT NULL UNIQUE, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS desk_placement_audit(seq INTEGER PRIMARY KEY, event TEXT NOT NULL, body TEXT NOT NULL, at_ms INTEGER NOT NULL);")?;
    Ok(())
}
fn core(conn: &Connection) -> Result<String> {
    crate::door::reader_link::machine_id_in(conn)
}
fn path(suffix: &str) -> Result<PathBuf> {
    Ok(crate::fleet::registry_path()?.with_extension(suffix))
}
fn lock(exclusive: bool) -> Result<fs::File> {
    lock_mode(exclusive, false)
}
fn lock_mode(exclusive: bool, nonblocking: bool) -> Result<fs::File> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path("desk-placement.lock")?)?;
    ensure!(
        unsafe {
            libc::flock(
                file.as_raw_fd(),
                (if exclusive {
                    libc::LOCK_EX
                } else {
                    libc::LOCK_SH
                }) | if nonblocking { libc::LOCK_NB } else { 0 },
            )
        } == 0,
        "desk placement lock unavailable"
    );
    Ok(file)
}
fn guard() -> Result<Option<Guard>> {
    match fs::read(path("desk-placement-owner.json")?) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn save_guard(g: &Guard) -> Result<()> {
    // Write-ahead fence outside the registry: restoring a registry alone cannot
    // erase enrollment/certification. A crash here fails closed and exact replay
    // can finish the interrupted installation. Not protection from stale clones.
    if let Some(old) = guard()? {
        ensure!(
            old.proposal == g.proposal,
            "restore_guard: rival enrollment"
        );
        if let Some(certificate) = old.certificate {
            ensure!(
                g.certificate.as_ref().is_none_or(|c| *c == certificate),
                "restore_guard: rival certificate"
            );
            if g.certificate.is_none() {
                return Ok(());
            } // Never downgrade a certified fence.
        }
    }
    let target = path("desk-placement-owner.json")?;
    let temp = path("desk-placement-owner.tmp")?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)?;
    use std::io::Write;
    file.write_all(&serde_json::to_vec(g)?)?;
    file.sync_all()?;
    fs::rename(temp, &target)?;
    fs::File::open(target.parent().context("guard directory")?)?.sync_all()?;
    Ok(())
}
fn records(conn: &Connection) -> Result<Vec<Record>> {
    let mut s =
        conn.prepare("SELECT record FROM desk_placement_record ORDER BY domain LIMIT 17")?;
    let strings = s
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(strings.len() <= LIMIT, "desk placement backpressure");
    strings
        .into_iter()
        .map(|s| Ok(serde_json::from_str(&s)?))
        .collect()
}
fn save(conn: &Connection, r: &Record) -> Result<()> {
    conn.execute("INSERT INTO desk_placement_record(domain,proposal,record) VALUES(?1,?2,?3) ON CONFLICT(domain) DO UPDATE SET proposal=excluded.proposal,record=excluded.record", params![r.proposal.manifest.domain, r.proposal.proposal_id, serde_json::to_string(r)?])?;
    Ok(())
}
fn audit(conn: &Connection, event: &str, p: &Proposal) -> Result<()> {
    conn.execute(
        "INSERT INTO desk_placement_audit(event,body,at_ms) VALUES(?1,?2,?3)",
        params![event, serde_json::to_string(p)?, Store::now_ms()],
    )?;
    // Bounded diagnostic history; immutable agreement/certificate is separate.
    conn.execute("DELETE FROM desk_placement_audit WHERE seq NOT IN (SELECT seq FROM desk_placement_audit ORDER BY seq DESC LIMIT 256)", [])?;
    Ok(())
}
fn description(conn: &Connection, name: &str) -> Result<Description> {
    let mut contacts = vec![];
    for m in super::machines(conn)? {
        let id: Option<String> = conn
            .query_row(
                "SELECT core FROM desk_placement_peer WHERE machine=?1",
                [&m.name],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(core) = id {
            contacts.push(Contact { core, role: m.role });
        }
    }
    Ok(Description {
        core: core(conn)?,
        name: name.into(),
        ships: crate::fleet::members(conn)?
            .into_iter()
            .map(|m| m.name)
            .collect(),
        contacts,
    })
}
pub(super) fn set_name(conn: &Connection, name: &str) -> Result<()> {
    identifier(name)?;
    conn.execute(
        "INSERT INTO desk_placement_name(only,name) VALUES(1,?1) ON CONFLICT(only) DO NOTHING",
        [name],
    )?;
    Ok(())
}
fn self_name(conn: &Connection) -> Result<String> {
    let name: Option<String> = conn
        .query_row(
            "SELECT name FROM desk_placement_name WHERE only=1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(name
        .or(super::machines(conn)?.into_iter().find_map(|m| m.known_as))
        .unwrap_or_else(|| "here".into()))
}
fn topology(conn: &Connection) -> Result<Vec<Description>> {
    let mut s =
        conn.prepare("SELECT description FROM desk_placement_topology ORDER BY core LIMIT 16")?;
    s.query_map([], |r| r.get::<_, String>(0))?
        .map(|r| Ok(serde_json::from_str(&r?)?))
        .collect()
}
fn local_source<'a>(p: &'a Proposal, id: &str) -> Result<&'a Source> {
    p.manifest
        .sources
        .iter()
        .find(|s| s.core == id)
        .context("not a placement participant")
}
fn peer_for(conn: &Connection, id: &str) -> Result<Option<super::Machine>> {
    let name: Option<String> = conn
        .query_row(
            "SELECT machine FROM desk_placement_peer WHERE core=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    name.map(|n| super::machine(conn, &n)).transpose()
}
fn check_direction(conn: &Connection, p: &Proposal, id: &str) -> Result<()> {
    let s = local_source(p, id)?;
    let ships: Vec<_> = crate::fleet::members(conn)?
        .into_iter()
        .map(|m| m.name)
        .collect();
    ensure!(ships == s.ships, "source manifest membership changed");
    if id == p.manifest.owner_core {
        ensure!(s.intake_direction == "local", "illegal source direction");
        for source in p.manifest.sources.iter().filter(|s| s.core != id) {
            let contact = peer_for(conn, &source.core)?
                .context("no_legal_contact: owner needs a direct source link")?;
            ensure!(
                source.intake_direction
                    == if contact.role == "collects" {
                        "collect"
                    } else {
                        "push"
                    },
                "illegal source direction"
            );
        }
    } else {
        let m = peer_for(conn, &p.manifest.owner_core)?
            .context("no_legal_contact: source needs a direct link to owner")?;
        ensure!(
            s.intake_direction
                == if m.role == "collects" {
                    "push"
                } else {
                    "collect"
                },
            "illegal source direction"
        );
    }
    Ok(())
}
fn validate(p: &Proposal) -> Result<()> {
    identifier(&p.manifest.domain)?;
    uuid::Uuid::parse_str(&p.proposal_id)?;
    uuid::Uuid::parse_str(&p.manifest.incarnation)?;
    ensure!(
        p.manifest.generation == 1 && p.manifest.manifest_revision == 1,
        "bootstrap must be generation/revision 1"
    );
    ensure!(
        p.manifest_hash == crate::core::hash(&serde_json::to_value(&p.manifest)?),
        "manifest_hash mismatch"
    );
    ensure!(
        !p.manifest.sources.is_empty() && p.manifest.sources.len() <= LIMIT,
        "source limit"
    );
    let mut cores = std::collections::BTreeSet::new();
    let mut names = std::collections::BTreeSet::new();
    let mut ships = std::collections::BTreeSet::new();
    for s in &p.manifest.sources {
        uuid::Uuid::parse_str(&s.core)?;
        identifier(&s.name)?;
        ensure!(
            cores.insert(&s.core) && names.insert(&s.name),
            "duplicate source"
        );
        ensure!(s.ships.len() <= 256, "source ship limit");
        for ship in &s.ships {
            identifier(ship)?;
            ensure!(ships.insert(ship), "duplicate source ship");
        }
        ensure!(
            ["local", "push", "collect"].contains(&s.intake_direction.as_str()),
            "illegal source direction"
        );
    }
    ensure!(
        cores.contains(&p.manifest.owner_core) && cores.contains(&p.manifest.coordinator),
        "owner/coordinator not a participant"
    );
    ensure!(
        (1..=86_400_000).contains(&p.manifest.max_source_age_ms),
        "invalid source age bound"
    );
    Ok(())
}
fn no_imports(conn: &Connection) -> Result<()> {
    for (table, filter) in [
        ("away", "returned_ms IS NULL"),
        ("away_digest", "state='pending'"),
    ] {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |r| r.get(0),
        )?;
        if exists {
            let count: i64 = conn.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE {filter}"),
                [],
                |r| r.get(0),
            )?;
            ensure!(
                count == 0,
                "unresolved_import: reconcile legacy {table} before accepting placement"
            );
        }
    }
    Ok(())
}
fn write_decision(conn: &Connection, p: &Proposal) -> Result<()> {
    let member = crate::fleet::members(conn)?
        .into_iter()
        .find(|m| m.home.is_dir())
        .context("coordinator needs a local ship for the decision")?;
    let store = Store::open(&member.home)?;
    if crate::mail::find(&store, &p.decision)?.is_some() {
        return Ok(());
    }
    let mut payload = json!({"desk_placement":{"proposal_id":p.proposal_id,"manifest_hash":p.manifest_hash},
        "decision":{"question":format!("Place desk domain {} at {}?",p.manifest.domain,local_source(p, &p.manifest.owner_core)?.name),
        "why":format!("Generation 1; manifest {}. Sources and intake directions: {}",p.manifest_hash,serde_json::to_string(&p.manifest.sources)?),
        "options":[{"id":"accept","label":"Accept placement","what_happens":"Install this exact generation-1 agreement on every participant. Pending bootstrap pauses legacy mail, away, digest and cursor service. Once certified, only the local owner resumes those independent legacy channels until migration; sources stay fenced with placement cards only. No desk subscriptions are exposed."},
        {"id":"reject","label":"Reject placement","what_happens":"Abort before certification; do not change the desk."}]}});
    crate::mail::typed_payload("decision", &mut payload)?;
    crate::mail::record_runtime(
        &store,
        &json!({"id":p.decision,"from":member.name,"to":"captain","kind":"decision",
        "body":format!("Desk placement {}: {}",p.manifest.domain,serde_json::to_string(&p.manifest)?),"payload":payload,"urgency":"normal"}),
    )?;
    Ok(())
}

fn clear_pending_guard(p: &Proposal) -> Result<()> {
    if guard()?.is_some_and(|g| g.proposal == *p && g.certificate.is_none()) {
        let target = path("desk-placement-owner.json")?;
        fs::remove_file(&target)?;
        fs::File::open(target.parent().context("guard directory")?)?.sync_all()?;
    }
    Ok(())
}

/// Called under the exclusive desk lock, without any network call.
fn absorb_record(conn: &Connection, incoming: &Record) -> Result<()> {
    let p = &incoming.proposal;
    validate(p)?;
    let id = core(conn)?;
    if local_source(p, &id).is_err() {
        return Ok(());
    }
    let existing = records(conn)?;
    // The bootstrap inventory is immutable, not a permanent membership fence.
    // Active/aborted replays must survive ordinary ship creation and cleanup,
    // and a rejection must always be able to release the reservation.
    let rejecting = incoming.approval == Some(false) && incoming.certificate.is_none();
    if !rejecting
        && !existing
            .iter()
            .any(|r| r.proposal == *p && matches!(r.state.as_str(), "active" | "aborted"))
    {
        check_direction(conn, p, &id)?;
    }
    if existing
        .iter()
        .any(|r| r.proposal == *p && r.state == "aborted")
    {
        ensure!(incoming.certificate.is_none(), "aborted proposal");
        clear_pending_guard(p)?;
        return Ok(());
    }
    for r in &existing {
        ensure!(
            r.proposal == *p || r.state == "aborted",
            "conflicting_proposal: this desk is reserved or owned"
        );
    }
    if let Some(g) = guard()? {
        ensure!(
            g.proposal == *p,
            "restore_guard: enrollment/owner does not match registry; no re-bootstrap"
        );
        if let Some(c) = g.certificate {
            ensure!(
                incoming.certificate.as_ref() == Some(&c)
                    || existing.iter().any(|r| r.certificate.as_ref() == Some(&c)),
                "restore_guard: certified owner requires exact certificate recovery"
            );
        }
    }
    if guard()?.is_none() {
        no_imports(conn)?;
        // Validate the existing coverage projection before reserving/fencing.
        // Do not import or change membership until certified installation.
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        crate::door::coverage::install_placement_manifest(
            &tx,
            &p.manifest.domain,
            p.manifest.max_source_age_ms,
            &p.manifest
                .sources
                .iter()
                .map(|s| (s.core.clone(), s.ships.clone()))
                .collect::<Vec<_>>(),
        )?;
        tx.rollback()?;
        save_guard(&Guard {
            proposal: p.clone(),
            certificate: None,
        })?;
    }
    let mut r = existing
        .into_iter()
        .find(|r| r.proposal == *p)
        .unwrap_or(Record {
            proposal: p.clone(),
            local_role: String::new(),
            intake_direction: String::new(),
            approval: None,
            accepted: vec![],
            certificate: None,
            state: "pending".into(),
        });
    r.local_role = if id == p.manifest.owner_core {
        "owner"
    } else {
        "source"
    }
    .into();
    r.intake_direction = local_source(p, &id)?.intake_direction.clone();
    if r.approval.is_none() {
        r.approval = incoming.approval;
    } else if let Some(approval) = incoming.approval {
        ensure!(r.approval == Some(approval), "conflicting approval");
    }
    for accepted in &incoming.accepted {
        local_source(p, accepted)?;
        // Never infer our own acceptance from a remote claim.
        if accepted != &id && !r.accepted.contains(accepted) {
            r.accepted.push(accepted.clone());
        }
    }
    if r.approval == Some(false) {
        ensure!(
            r.certificate.is_none() && incoming.certificate.is_none(),
            "already certified"
        );
        r.state = "aborted".into();
        save(conn, &r)?;
        clear_pending_guard(p)?;
        audit(conn, "desk.placement.aborted", p)?;
        return Ok(());
    }
    if r.approval == Some(true) && !r.accepted.contains(&id) {
        no_imports(conn)?;
        save_guard(&Guard {
            proposal: p.clone(),
            certificate: None,
        })?;
        r.accepted.push(id.clone());
        audit(conn, "desk.placement.accepted", p)?;
    }
    if let Some(c) = &incoming.certificate {
        let mut all: Vec<_> = p.manifest.sources.iter().map(|s| s.core.clone()).collect();
        all.sort();
        ensure!(
            c.proposal == *p
                && c.accepted == all
                && r.approval == Some(true)
                && r.accepted.contains(&id),
            "invalid certificate or missing local acceptance"
        );
        if let Some(old) = &r.certificate {
            ensure!(old == c, "rival certificate");
        }
        r.certificate = Some(c.clone());
    }
    if id == p.manifest.coordinator
        && r.certificate.is_none()
        && r.approval == Some(true)
        && p.manifest
            .sources
            .iter()
            .all(|s| r.accepted.contains(&s.core))
    {
        let mut accepted = r.accepted.clone();
        accepted.sort();
        r.certificate = Some(Certificate {
            proposal: p.clone(),
            accepted,
        });
    }
    if let Some(c) = &r.certificate {
        save_guard(&Guard {
            proposal: p.clone(),
            certificate: Some(c.clone()),
        })?;
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        let owner = if id == p.manifest.owner_core {
            "here".to_owned()
        } else {
            peer_for(&tx, &p.manifest.owner_core)?
                .context("owner contact disappeared")?
                .name
        };
        tx.execute("INSERT INTO captain_desk(only,machine,set_ms) VALUES(1,?1,?2) ON CONFLICT(only) DO UPDATE SET machine=excluded.machine,set_ms=excluded.set_ms", params![owner,Store::now_ms()])?;
        crate::door::coverage::install_placement_manifest(
            &tx,
            &p.manifest.domain,
            p.manifest.max_source_age_ms,
            &p.manifest
                .sources
                .iter()
                .map(|s| (s.core.clone(), s.ships.clone()))
                .collect::<Vec<_>>(),
        )?;
        r.state = "active".into();
        save(&tx, &r)?;
        audit(&tx, "desk.placement.committed", p)?;
        tx.commit()?;
    } else {
        let new = !records(conn)?.iter().any(|old| old.proposal == *p);
        save(conn, &r)?;
        if new {
            audit(conn, "desk.placement.proposed", p)?;
        }
    }
    if id == p.manifest.coordinator {
        write_decision(conn, p)?;
    }
    Ok(())
}

/// Separate from ship-move `placement`. Bounded gossip carries only topology
/// and identical proposal/approval/acceptance/certificate records.
pub(super) fn frame(conn: &Connection, peer: &str) -> Result<Value> {
    let name = super::machine(conn, peer)
        .ok()
        .and_then(|m| m.known_as)
        .unwrap_or(self_name(conn)?);
    Ok(
        json!({"version":1,"you":peer,"description":description(conn,&name)?,"topology":topology(conn)?,"records":records(conn)?}),
    )
}
pub(super) fn absorb(conn: &Connection, peer: &str, frame: &Value) -> Result<()> {
    absorb_frame(conn, peer, frame)?.map_or(Ok(()), Err)
}
/// A frame-level error is returned; record errors are already recorded and
/// only the last is handed back, after every record within the bound ran.
fn absorb_frame(conn: &Connection, peer: &str, frame: &Value) -> Result<Option<anyhow::Error>> {
    if frame.is_null() {
        return Ok(None);
    }
    ensure!(
        frame["version"] == 1 && serde_json::to_vec(frame)?.len() <= 256 * 1024,
        "unsupported or oversized desk_placement frame"
    );
    let desc: Description = serde_json::from_value(frame["description"].clone())?;
    uuid::Uuid::parse_str(&desc.core)?;
    identifier(&desc.name)?;
    let _lock = lock(true)?;
    let name = required(frame, "you")?;
    identifier(name)?;
    conn.execute(
        "INSERT INTO desk_placement_name(only,name) VALUES(1,?1) ON CONFLICT(only) DO NOTHING",
        [name],
    )?;
    let old: Option<String> = conn
        .query_row(
            "SELECT core FROM desk_placement_peer WHERE machine=?1",
            [peer],
            |r| r.get(0),
        )
        .optional()?;
    ensure!(
        old.is_none_or(|old| old == desc.core),
        "peer core identity changed"
    );
    conn.execute("INSERT INTO desk_placement_peer(machine,core,description) VALUES(?1,?2,?3) ON CONFLICT(machine) DO UPDATE SET description=excluded.description", params![peer,desc.core,serde_json::to_string(&desc)?])?;
    let topo: Vec<Description> = serde_json::from_value(frame["topology"].clone())?;
    ensure!(
        topo.len() <= LIMIT && desc.contacts.len() <= LIMIT,
        "topology limit"
    );
    let own = core(conn)?;
    for d in topo.iter().chain(std::iter::once(&desc)) {
        if d.core == own {
            continue;
        }
        uuid::Uuid::parse_str(&d.core)?;
        ensure!(
            d.contacts.len() <= LIMIT && d.ships.len() <= 256,
            "description limit"
        );
        conn.execute("INSERT INTO desk_placement_topology(core,description) VALUES(?1,?2) ON CONFLICT(core) DO UPDATE SET description=excluded.description", params![d.core,serde_json::to_string(d)?])?;
    }
    let rows = frame["records"]
        .as_array()
        .context("placement records must be an array")?;
    let mut failure = None;
    // Bound work, but do not let one malformed/conflicting record prevent the
    // remaining records from converging. Facts stay at the sender for retry.
    for value in rows.iter().take(LIMIT) {
        let result = serde_json::from_value(value.clone())
            .map_err(anyhow::Error::from)
            .and_then(|r| absorb_record(conn, &r));
        if let Err(error) = result {
            record_error(conn, peer, Some(value), &error);
            failure = Some(error);
        }
    }
    if rows.len() > LIMIT {
        let error = anyhow::anyhow!("proposal limit");
        record_error(conn, peer, None, &error);
        failure = Some(error);
    }
    Ok(failure)
}

fn record_error(conn: &Connection, peer: &str, record: Option<&Value>, error: &anyhow::Error) {
    let body = json!({"peer":peer,"proposal_id":record.map(|r| &r["proposal"]["proposal_id"]),"error":format!("{error:#}")});
    eprintln!("privateer: desk placement: {body}");
    let _ = conn.execute(
        "INSERT INTO desk_placement_audit(event,body,at_ms) VALUES('desk.placement.exchange_failed',?1,?2)",
        params![body.to_string(), Store::now_ms()],
    );
    let _ = conn.execute("DELETE FROM desk_placement_audit WHERE seq NOT IN (SELECT seq FROM desk_placement_audit ORDER BY seq DESC LIMIT 256)", []);
}

/// Placement is a side channel: never abort ordinary directory/mail exchange.
/// Dedicated placement RPCs still return errors for operator diagnostics.
pub(super) fn absorb_mail(conn: &Connection, peer: &str, frame: &Value) {
    if let Err(error) = absorb_frame(conn, peer, frame) {
        record_error(conn, peer, None, &error);
    }
}
pub(super) fn mail_frame(conn: &Connection, peer: &str) -> Value {
    frame(conn, peer).unwrap_or_else(|error| {
        record_error(conn, peer, None, &error);
        Value::Null
    })
}

fn drive(conn: &Connection) -> Vec<Value> {
    let mut outcomes = vec![];
    if let Ok(machines) = super::machines(conn) {
        for m in machines
            .into_iter()
            .filter(|m| m.role == "collects")
            .take(LIMIT)
        {
            let result = (|| -> Result<()> {
                let frame = frame(conn, &m.name)?;
                let verb = if records(conn)?.iter().any(|r| r.certificate.is_some()) {
                    "desk.placement.commit"
                } else if records(conn)?.iter().any(|r| r.approval.is_some()) {
                    "desk.placement.accept"
                } else if records(conn)?.iter().any(|r| r.state == "pending") {
                    "desk.placement.propose"
                } else {
                    "desk.placement.status"
                };
                let response = super::call(
                    &m,
                    verb,
                    json!({"machine":m.known_as,"desk_placement":frame}),
                    Some(std::time::Duration::from_secs(10)),
                )?;
                absorb(conn, &m.name, &response["desk_placement"])
            })();
            outcomes.push(json!({"machine":m.name,"error":result.err().map(|e|format!("{e:#}"))}));
        }
    }
    outcomes
}
fn status(conn: &Connection) -> Result<Value> {
    let rows = records(conn)?;
    let g = guard()?;
    let restore_guard = match g.as_ref() {
        Some(g) => !rows.iter().any(|r| {
            r.proposal == g.proposal
                && r.certificate == g.certificate
                && (g.certificate.is_none() || r.state == "active")
        }),
        None => rows.iter().any(|r| r.state != "aborted"),
    };
    let recovery = g.as_ref().filter(|_|restore_guard).map(|g|json!({"proposal_id":g.proposal.proposal_id,"manifest_hash":g.proposal.manifest_hash,"certified":g.certificate.is_some(),"action":if g.proposal.manifest.sources.len() == 1 { "exact_local_commit" } else { "participant_certificate_replay" }}));
    Ok(
        json!({"version":1,"core":core(conn)?,"records":rows,"restore_guard":restore_guard,"recovery":recovery,"subscriptions":false,"coverage_complete":false,"desk_here":super::desk_here(conn)}),
    )
}
pub(crate) fn dispatch(command: &str, args: &Value) -> Result<Value> {
    let conn = crate::fleet::registry()?;
    match command {
        "machine.desk.propose" => {
            // A contact pass learns owner-only sources before any staging.
            let _ = drive(&conn);
            let _lock = lock(true)?;
            ensure!(
                guard()?.is_none() && records(&conn)?.iter().all(|r| r.state == "aborted"),
                "restore_guard: desk already enrolled/reserved/owned"
            );
            let own = core(&conn)?;
            let mut topo: BTreeMap<String, Description> = BTreeMap::new();
            for d in topology(&conn)? {
                ensure!(
                    topo.insert(d.name.clone(), d).is_none(),
                    "duplicate machine name in topology"
                );
            }
            let mut here = description(&conn, &self_name(&conn)?)?;
            // A reached machine learns its name from the peer's contact mapping.
            if here.name == "here" {
                if let Some(name) = args.get("here").and_then(Value::as_str) {
                    identifier(name)?;
                    here.name = name.into();
                }
            }
            for name in [here.name.as_str(), "here"] {
                ensure!(
                    topo.get(name).is_none_or(|d| d.core == own),
                    "duplicate machine name in topology"
                );
                topo.insert(name.to_owned(), here.clone());
            }
            let owner_name = required(args, "owner")?;
            let owner = topo
                .get(owner_name)
                .context("no_legal_contact: unknown owner; configure/contact a direct link")?;
            let names = args["sources"]
                .as_array()
                .context("sources must be an explicit array")?;
            ensure!(!names.is_empty() && names.len() <= LIMIT, "source limit");
            let selected: Vec<&Description> = names.iter().map(|n| topo.get(n.as_str().context("source must be a name")?).context("no_legal_contact: unknown source; link source directly to owner and refresh")).collect::<Result<_>>()?;
            ensure!(
                selected.iter().any(|s| s.core == own)
                    && selected.iter().any(|s| s.core == owner.core),
                "proposer and owner must be sources"
            );
            let contacts_all = |d: &Description| {
                selected.iter().all(|s| {
                    s.core == d.core
                        || d.contacts.iter().any(|c| c.core == s.core)
                        || s.contacts.iter().any(|c| c.core == d.core)
                })
            };
            ensure!(
                own == owner.core || here.contacts.iter().any(|c| c.core == owner.core),
                "no_legal_contact: proposer needs a direct link to owner"
            );
            let coordinator = if contacts_all(&here) {
                own.clone()
            } else {
                ensure!(
                    contacts_all(owner),
                    "no_legal_contact: owner {} must contact sources {}; configure source -> owner (push) or owner -> source (collect)",
                    owner.name,
                    selected
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                owner.core.clone()
            };
            let missing: Vec<_> = selected
                .iter()
                .filter(|s| {
                    s.core != owner.core
                        && !owner.contacts.iter().any(|c| c.core == s.core)
                        && !s.contacts.iter().any(|c| c.core == owner.core)
                })
                .map(|s| s.name.as_str())
                .collect();
            ensure!(
                missing.is_empty(),
                "no_legal_contact: sources {} need source -> {} (push) or {} -> source (collect)",
                missing.join(", "),
                owner.name,
                owner.name
            );
            let mut sources = vec![];
            for s in selected {
                let direction = if s.core == owner.core {
                    "local"
                } else if let Some(link) = owner.contacts.iter().find(|c| c.core == s.core) {
                    if link.role == "collects" {
                        "collect"
                    } else {
                        "push"
                    }
                } else {
                    let link = s.contacts.iter().find(|c|c.core == owner.core).context("no_legal_contact: link source -> owner (push) or owner -> source (collect)")?;
                    if link.role == "collects" {
                        "push"
                    } else {
                        "collect"
                    }
                };
                sources.push(Source {
                    core: s.core.clone(),
                    name: s.name.clone(),
                    ships: s.ships.clone(),
                    intake_direction: direction.into(),
                });
            }
            sources.sort_by(|a, b| a.core.cmp(&b.core));
            let manifest = Manifest {
                domain: required(args, "domain")?.into(),
                manifest_revision: 1,
                max_source_age_ms: args["max_source_age_ms"]
                    .as_i64()
                    .unwrap_or(crate::door::coverage::DEFAULT_MAX_SOURCE_AGE_MS),
                owner_core: owner.core.clone(),
                generation: 1,
                incarnation: uuid::Uuid::new_v4().to_string(),
                coordinator,
                sources,
            };
            let proposal = Proposal {
                proposal_id: uuid::Uuid::new_v4().to_string(),
                manifest_hash: crate::core::hash(&serde_json::to_value(&manifest)?),
                manifest,
                decision: uuid::Uuid::new_v4().to_string(),
            };
            absorb_record(
                &conn,
                &Record {
                    proposal,
                    local_role: String::new(),
                    intake_direction: String::new(),
                    approval: None,
                    accepted: vec![],
                    certificate: None,
                    state: "pending".into(),
                },
            )?;
            drop(_lock);
        }
        "machine.desk.accept" => {
            approve(&conn, args, true)?;
        }
        "machine.desk.reject" => {
            approve(&conn, args, false)?;
        }
        "machine.desk.commit" if args.get("proposal_id").is_some() => {
            let _lock = lock(true)?;
            let g = guard()?.context("restore_guard: no durable enrollment")?;
            ensure!(
                required(args, "proposal_id")? == g.proposal.proposal_id
                    && required(args, "manifest_hash")? == g.proposal.manifest_hash,
                "restore_guard: exact recovery binding required"
            );
            ensure!(
                g.proposal.manifest.sources.len() == 1
                    && g.proposal.manifest.owner_core == core(&conn)?,
                "restore_guard: linked domains require participant certificate replay"
            );
            let approval = g.certificate.as_ref().map(|_| true);
            let accepted = g
                .certificate
                .as_ref()
                .map(|c| c.accepted.clone())
                .unwrap_or_default();
            absorb_record(
                &conn,
                &Record {
                    proposal: g.proposal,
                    local_role: String::new(),
                    intake_direction: String::new(),
                    approval,
                    accepted,
                    certificate: g.certificate,
                    state: "pending".into(),
                },
            )?;
        }
        "machine.desk.status" | "machine.desk.commit" => {}
        _ => anyhow::bail!("unknown desk placement command"),
    }
    let contacts = drive(&conn);
    let mut result = status(&conn)?;
    if command == "machine.desk.status" {
        let sources = records(&conn)?
            .iter()
            .find(|r| r.state != "aborted")
            .map(|r| super::desk_intake::completion::status(&conn, &r.proposal.manifest))
            .unwrap_or_default();
        result["source_status"] = json!(sources);
    }
    result["contacts"] = json!(contacts);
    Ok(result)
}
fn approve(conn: &Connection, args: &Value, yes: bool) -> Result<()> {
    let _lock = lock(true)?;
    let id = required(args, "proposal_id")?;
    let hash = required(args, "manifest_hash")?;
    let mut r = records(conn)?
        .into_iter()
        .find(|r| r.proposal.proposal_id == id)
        .context("unknown proposal")?;
    ensure!(r.proposal.manifest_hash == hash, "manifest_hash mismatch");
    ensure!(r.state != "aborted" || !yes, "aborted proposal");
    if r.state == "aborted" {
        return clear_pending_guard(&r.proposal);
    }
    ensure!(r.certificate.is_none() || yes, "already certified");
    let g = guard()?.context("restore_guard: missing external enrollment fence")?;
    ensure!(
        g.proposal == r.proposal && g.certificate == r.certificate,
        "restore_guard: recover exact certificate before local approval"
    );
    r.approval = Some(yes);
    absorb_record(conn, &r)?;
    audit(
        conn,
        if yes {
            "desk.placement.approved"
        } else {
            "desk.placement.rejected"
        },
        &r.proposal,
    )
}
pub(crate) fn answered(store: &Store, request: &str, reply: &Value) {
    let result = (|| -> Result<()> {
        let Some(choice) = reply["payload"]["answer"]["option_id"]
            .as_str()
            .filter(|c| ["accept", "reject"].contains(c))
        else {
            return Ok(());
        };
        let Some(card) = crate::mail::find(store, request)? else {
            return Ok(());
        };
        let conn = crate::fleet::registry()?;
        if !matches_card(&conn, &card)? {
            return Ok(());
        }
        approve(
            &conn,
            &card["payload"]["desk_placement"],
            choice == "accept",
        )
    })();
    if let Err(e) = result {
        let _ = store.audit(
            "desk.placement.approval_failed",
            &json!({"error":format!("{e:#}")}),
        );
    }
}
pub(super) fn serve(conn: &Connection, command: &str, args: &Value) -> Result<Value> {
    let peer = required(args, "machine")?;
    super::machine(conn, peer)?;
    // Dedicated verbs carry records, never caller-controlled operator dispatch.
    ensure!(
        [
            "desk.placement.propose",
            "desk.placement.accept",
            "desk.placement.commit",
            "desk.placement.status"
        ]
        .contains(&command),
        "unsupported desk placement verb"
    );
    if matches!(command, "desk.placement.status" | "desk.placement.propose") {
        for r in args["desk_placement"]["records"]
            .as_array()
            .into_iter()
            .flatten()
        {
            ensure!(
                r["approval"].is_null() && r["certificate"].is_null(),
                "status/propose is not acceptance: use desk.placement.accept or commit"
            );
        }
    }
    absorb(conn, peer, &args["desk_placement"])?;
    let mut result = status(conn)?;
    result["desk_placement"] = frame(conn, peer)?;
    Ok(result)
}

/// None is a legacy installation, Some(here/peer) is explicit active routing.
/// Pending records and restored registries cannot fall through to inference.
pub(super) fn routing(conn: &Connection) -> Result<Option<String>> {
    let rows = records(conn)?;
    let active = rows.iter().find(|r| r.state != "aborted");
    let g = guard()?;
    if active.is_none() && g.is_none() {
        return Ok(None);
    }
    let r = active.context("restore_guard: missing enrolled placement")?;
    let g = g.context("restore_guard: missing external enrollment fence")?;
    ensure!(
        r.state == "active"
            && r.certificate.is_some()
            && g.proposal == r.proposal
            && g.certificate == r.certificate,
        "pending placement or restore_guard: owner unavailable"
    );
    let id = core(conn)?;
    if id == r.proposal.manifest.owner_core {
        Ok(Some("here".into()))
    } else {
        Ok(Some(
            peer_for(conn, &r.proposal.manifest.owner_core)?
                .context("owner contact missing")?
                .name,
        ))
    }
}
/// Intake holds this shared fence for its whole local transaction. No network
/// operation may run while it is held. The manifest is the placement contract,
/// not a second independently configured source/owner list.
pub(super) fn intake_manifest(conn: &Connection) -> Result<(fs::File, Manifest)> {
    let fence = lock_mode(false, true)?;
    routing(conn)?.context("intake requires explicit placement")?;
    let manifest = records(conn)?
        .into_iter()
        .find(|r| r.state == "active")
        .context("missing active placement")?
        .proposal
        .manifest;
    Ok((fence, manifest))
}

/// Hold the placement fence until the subscription transaction commits. No
/// inferred owner, remote fallback, bootstrap or network call is permitted.
pub(crate) fn subscription_owner(conn: &Connection) -> Result<(fs::File, Manifest)> {
    let (fence, manifest) = intake_manifest(conn)?;
    ensure!(core(conn)? == manifest.owner_core, "owner_unavailable");
    Ok((fence, manifest))
}

/// Typed local-exposure diagnosis; never infer an owner or contact a peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExposureRefusal {
    NoPlacement,
    PlacementPending,
    RestoreGuard,
    RemoteOwner { reverse_only: bool },
    StaleGeneration,
}
impl ExposureRefusal {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::NoPlacement => "no_placement",
            Self::PlacementPending => "placement_pending",
            Self::RestoreGuard => "restore_guard",
            Self::RemoteOwner { .. } => "remote_owner",
            Self::StaleGeneration => "stale_generation",
        }
    }
}

/// Caller retains this shared fence through its registry transaction. Recheck
/// inside the transaction as well: waiting for the writer is not admission.
pub(crate) fn exposure_placement(
    conn: &Connection,
) -> Result<(fs::File, std::result::Result<Manifest, ExposureRefusal>)> {
    let fence = lock_mode(false, true)?;
    let rows = records(conn)?;
    let guard = match guard() {
        Ok(guard) => guard,
        Err(_) => return Ok((fence, Err(ExposureRefusal::RestoreGuard))),
    };
    let current: Vec<_> = rows.iter().filter(|r| r.state != "aborted").collect();
    let refusal = |fence, reason| Ok((fence, Err(reason)));
    if current.is_empty() && guard.is_none() {
        return refusal(fence, ExposureRefusal::NoPlacement);
    }
    let [record] = current.as_slice() else {
        return refusal(fence, ExposureRefusal::RestoreGuard);
    };
    let Some(guard) = guard else {
        return refusal(fence, ExposureRefusal::RestoreGuard);
    };
    if guard.proposal != record.proposal || guard.certificate != record.certificate {
        return refusal(fence, ExposureRefusal::RestoreGuard);
    }
    let p = &record.proposal;
    if p.manifest.generation != 1 {
        return refusal(fence, ExposureRefusal::StaleGeneration);
    }
    if record.state != "active" || record.certificate.is_none() {
        return refusal(fence, ExposureRefusal::PlacementPending);
    }
    let mut participants: Vec<_> = p.manifest.sources.iter().map(|s| s.core.clone()).collect();
    participants.sort();
    if validate(p).is_err()
        || record.approval != Some(true)
        || record
            .certificate
            .as_ref()
            .is_none_or(|c| c.proposal != *p || c.accepted != participants)
    {
        return refusal(fence, ExposureRefusal::StaleGeneration);
    }
    let id = core(conn)?;
    if p.manifest.owner_core != id {
        let reverse_only =
            peer_for(conn, &p.manifest.owner_core)?.is_some_and(|peer| peer.role == "collected");
        return refusal(fence, ExposureRefusal::RemoteOwner { reverse_only });
    }
    if !legacy_owner(record, &guard, &id) {
        return refusal(fence, ExposureRefusal::StaleGeneration);
    }
    Ok((fence, Ok(p.manifest.clone())))
}

/// Diagnostic only, including a manifest the current protocol cannot serve.
/// Never use this comparison as placement or reader admission.
pub(crate) fn exposure_manifest_changed(
    conn: &Connection,
    domain: &str,
    revision: i64,
    hash: &str,
) -> Result<bool> {
    for r in records(conn)? {
        let m = &r.proposal.manifest;
        if r.state != "aborted" && m.domain == domain {
            return Ok(m.manifest_revision != revision
                || crate::core::hash(&serde_json::to_value(m)?) != hash);
        }
    }
    Ok(false)
}

pub(super) fn owner_tuple(conn: &Connection) -> Result<Value> {
    if routing(conn).is_err() {
        return Ok(Value::Null);
    }
    Ok(records(conn)?.into_iter().find(|r|r.state == "active").map(|r| json!({"domain":r.proposal.manifest.domain,"owner_core":r.proposal.manifest.owner_core,"generation":1,"incarnation":r.proposal.manifest.incarnation,"manifest_hash":r.proposal.manifest_hash})).unwrap_or(Value::Null))
}
pub(super) fn collect_allowed(conn: &Connection, peer: &str, tuple: &Value) -> Result<bool> {
    match routing(conn) {
        Ok(None) => Ok(true),
        Ok(Some(owner)) => Ok(owner == peer && *tuple == owner_tuple(conn)?),
        Err(_) => Ok(false),
    }
}
pub(super) fn captain_allowed(conn: &Connection, peer: &str, tuple: &Value) -> Result<()> {
    match routing(conn)? {
        None => Ok(()),
        Some(owner) => {
            ensure!(
                owner == "here" && *tuple == owner_tuple(conn)?,
                "stale_generation: captain intake requires agreed owner tuple"
            );
            let id: String = conn.query_row(
                "SELECT core FROM desk_placement_peer WHERE machine=?1",
                [peer],
                |r| r.get(0),
            )?;
            let r = records(conn)?
                .into_iter()
                .find(|r| r.state == "active")
                .context("missing active placement")?;
            ensure!(
                local_source(&r.proposal, &id)?.intake_direction == "push",
                "illegal source direction"
            );
            Ok(())
        }
    }
}

/// Whether legacy service is fenced, not merely whether placement is recorded.
/// The certified generation-one local owner keeps its independent legacy
/// channels until migration; sources and pending/restored placements do not.
/// Bootstrap control receipts never advance a legacy or aggregate cursor.
pub(crate) fn enrolled() -> Result<bool> {
    let _lock = lock(false)?;
    legacy_fenced(&crate::fleet::registry()?)
}

fn legacy_fenced(conn: &Connection) -> Result<bool> {
    let rows = records(conn)?;
    // A damaged/unreadable external restore fence is not permission to serve.
    // Operator placement status retains the underlying diagnostic.
    let guard = match guard() {
        Ok(guard) => guard,
        Err(_) => return Ok(true),
    };
    let current: Vec<_> = rows.iter().filter(|r| r.state != "aborted").collect();
    if current.is_empty() && guard.is_none() {
        return Ok(false);
    }
    let [record] = current.as_slice() else {
        return Ok(true);
    };
    let Some(guard) = guard else {
        return Ok(true);
    };
    Ok(!legacy_owner(record, &guard, &core(conn)?))
}

fn legacy_owner(record: &Record, guard: &Guard, local_core: &str) -> bool {
    let p = &record.proposal;
    let Some(certificate) = record.certificate.as_ref() else {
        return false;
    };
    let mut participants: Vec<_> = p.manifest.sources.iter().map(|s| s.core.clone()).collect();
    participants.sort();
    record.state == "active"
        && record.local_role == "owner"
        && record.intake_direction == "local"
        && record.approval == Some(true)
        && p.manifest.owner_core == local_core
        && validate(p).is_ok()
        && local_source(p, local_core).is_ok_and(|s| s.intake_direction == "local")
        && certificate.proposal == *p
        && certificate.accepted == participants
        && guard.proposal == *p
        && guard.certificate.as_ref() == Some(certificate)
}
fn matches_card(conn: &Connection, card: &Value) -> Result<bool> {
    Ok(card["kind"] == "decision"
        && card["to"] == "captain"
        && records(conn)?.iter().any(|r| {
            r.state != "aborted"
                && card["id"] == r.proposal.decision
                && card["payload"]["desk_placement"]["proposal_id"] == r.proposal.proposal_id
                && card["payload"]["desk_placement"]["manifest_hash"] == r.proposal.manifest_hash
        }))
}
pub(crate) fn control_reply(store: &Store, args: &Value) -> Result<bool> {
    if args["kind"] != "reply" {
        return Ok(false);
    }
    let Some(id) = args["correlation"].as_str() else {
        return Ok(false);
    };
    let Some(card) = crate::mail::find(store, id)? else {
        return Ok(false);
    };
    matches_card(&crate::fleet::registry()?, &card)
}
pub(crate) fn control_cards() -> Result<Vec<(String, Value)>> {
    let conn = crate::fleet::registry()?;
    let mut cards = vec![];
    for r in records(&conn)?.into_iter().filter(|r| r.state != "aborted") {
        for member in crate::fleet::members(&conn)? {
            let Ok(store) = Store::open(&member.home) else {
                continue;
            };
            if let Some(card) = crate::mail::find(&store, &r.proposal.decision)? {
                if card["state"] == "open" && matches_card(&conn, &card)? {
                    cards.push((member.name, card));
                    break;
                }
            }
        }
    }
    Ok(cards)
}

/// Pending/restored placement can refuse routing before a writer is reached.
/// Preserve its named fence at that pre-effect boundary, while leaving valid
/// existing routes to a remote owner unchanged. Never hold this lock on contact.
pub(crate) fn check_legacy_desk_route() -> Result<()> {
    let _lock = lock(false)?;
    let conn = crate::fleet::registry()?;
    ensure!(
        !legacy_fenced(&conn)? || routing(&conn).is_ok(),
        crate::door::scoped::Denial::LegacyWriterFenced
    );
    Ok(())
}

/// Placement-changing legacy commands do not get the owner channel exception.
/// Caller must hold the shared legacy writer lock through its transaction.
pub(super) fn require_unenrolled(conn: &Connection) -> Result<()> {
    ensure!(
        guard()?.is_none() && records(conn)?.iter().all(|r| r.state == "aborted"),
        "legacy_writer_fenced: desk domain enrolled; migration required"
    );
    Ok(())
}

pub(crate) fn legacy_writer() -> Result<fs::File> {
    let _lock = lock(false)?;
    let conn = crate::fleet::registry()?;
    ensure!(
        !legacy_fenced(&conn)?,
        crate::door::scoped::Denial::LegacyWriterFenced
    );
    Ok(_lock)
}

#[cfg(test)]
mod legacy_fence_tests {
    use super::*;
    const OWNER: &str = "00000000-0000-4000-8000-00000000000b";
    const SOURCE: &str = "00000000-0000-4000-8000-00000000000a";

    fn fixture() -> (Record, Guard) {
        let manifest: Manifest =
            serde_json::from_value(json!({"domain":"beta-domain","manifest_revision":1,
            "max_source_age_ms":900000,"owner_core":OWNER,"generation":1,
            "incarnation":"00000000-0000-4000-8000-00000000000c","coordinator":SOURCE,
            "sources":[{"core":SOURCE,"name":"alpha","ships":["survey"],"intake_direction":"push"},
                       {"core":OWNER,"name":"beta","ships":["relay"],"intake_direction":"local"}]}))
            .unwrap();
        let proposal = Proposal {
            proposal_id: "00000000-0000-4000-8000-00000000000d".into(),
            manifest_hash: crate::core::hash(&serde_json::to_value(&manifest).unwrap()),
            manifest,
            decision: "placement-card".into(),
        };
        let certificate = Certificate {
            proposal: proposal.clone(),
            accepted: vec![SOURCE.into(), OWNER.into()],
        };
        let record = Record {
            proposal: proposal.clone(),
            local_role: "owner".into(),
            intake_direction: "local".into(),
            approval: Some(true),
            accepted: certificate.accepted.clone(),
            certificate: Some(certificate.clone()),
            state: "active".into(),
        };
        (
            record,
            Guard {
                proposal,
                certificate: Some(certificate),
            },
        )
    }

    #[test]
    fn legacy_writer_lock_survives_owner_exception_and_restart_refences() {
        use std::io::{BufRead, Write};
        use std::process::{Command, Stdio};
        const CHILD: &str = "PRIVATEER_TEST_LEGACY_FENCE_CHILD";
        if let Ok(mode) = std::env::var(CHILD) {
            if mode == "fenced" {
                let error = legacy_writer().unwrap_err();
                assert_eq!(
                    error.downcast_ref::<crate::door::scoped::Denial>(),
                    Some(&crate::door::scoped::Denial::LegacyWriterFenced)
                );
                assert!(enrolled().unwrap());
            } else {
                let _fence = legacy_writer().unwrap();
                assert!(!enrolled().unwrap());
                println!("legacy-writer-ready");
                std::io::stdout().flush().unwrap();
                let mut line = String::new();
                std::io::stdin().read_line(&mut line).unwrap();
            }
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let registry = dir.path().join("fleet.sqlite");
        let conn = Connection::open(&registry).unwrap();
        crate::machines::init(&conn).unwrap();
        core(&conn).unwrap();
        conn.execute("UPDATE door_reader_identity SET id=?1", [OWNER])
            .unwrap();
        let (mut record, guard) = fixture();
        save(&conn, &record).unwrap();
        fs::write(
            registry.with_extension("desk-placement-owner.json"),
            serde_json::to_vec(&guard).unwrap(),
        )
        .unwrap();
        let child = |mode: &str| {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.args(["--exact", "machines::desk_placement::legacy_fence_tests::legacy_writer_lock_survives_owner_exception_and_restart_refences", "--nocapture"])
                .env(CHILD, mode).env("PRIVATEER_FLEET", &registry).env("HOME", dir.path());
            command
        };
        let mut writer = child("writer")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = std::io::BufReader::new(writer.stdout.take().unwrap());
        loop {
            let mut line = String::new();
            assert!(
                output.read_line(&mut line).unwrap() > 0,
                "writer exited before acquiring fence"
            );
            // libtest may put its `test ...` progress prefix on this line.
            if line.trim_end().ends_with("legacy-writer-ready") {
                break;
            }
        }
        let exclusive = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(registry.with_extension("desk-placement.lock"))
            .unwrap();
        // Positive IPC barrier, not a timing assumption: the actual helper is
        // holding its shared lock. A placement writer cannot pass it.
        assert_ne!(
            unsafe { libc::flock(exclusive.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        writeln!(writer.stdin.take().unwrap(), "finish").unwrap();
        assert!(writer.wait().unwrap().success());
        assert_eq!(
            unsafe { libc::flock(exclusive.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        record.state = "pending".into();
        save(&conn, &record).unwrap();
        drop(exclusive);
        // A stale restarted legacy writer must recheck the durable facts.
        assert!(child("fenced").status().unwrap().success());
    }

    #[test]
    fn only_exact_certified_generation_one_local_owner_gets_legacy_service() {
        let (record, guard) = fixture();
        assert!(legacy_owner(&record, &guard, OWNER));
        assert!(!legacy_owner(&record, &guard, SOURCE));
        for state in ["pending", "aborted", "moving", "unknown"] {
            let mut r = record.clone();
            r.state = state.into();
            assert!(!legacy_owner(&r, &guard, OWNER));
        }
        for field in [
            "role",
            "direction",
            "approval",
            "certificate",
            "participants",
            "generation",
            "hash",
        ] {
            let mut r = record.clone();
            match field {
                "role" => r.local_role = "source".into(),
                "direction" => r.intake_direction = "push".into(),
                "approval" => r.approval = None,
                "certificate" => r.certificate = None,
                "participants" => r.certificate.as_mut().unwrap().accepted.clear(),
                "generation" => r.proposal.manifest.generation = 2,
                "hash" => r.proposal.manifest_hash = "wrong".into(),
                _ => unreachable!(),
            }
            assert!(!legacy_owner(&r, &guard, OWNER), "{field}");
        }
        // Even a mutually matching future certificate must not lift this
        // generation-one exception. Do not rely on the guard mismatch alone.
        let mut future = record.clone();
        future.proposal.manifest.generation = 2;
        future.proposal.manifest_hash =
            crate::core::hash(&serde_json::to_value(&future.proposal.manifest).unwrap());
        future.certificate.as_mut().unwrap().proposal = future.proposal.clone();
        let future_guard = Guard {
            proposal: future.proposal.clone(),
            certificate: future.certificate.clone(),
        };
        assert!(!legacy_owner(&future, &future_guard, OWNER));
        let mut missing_certificate = Guard {
            proposal: guard.proposal.clone(),
            certificate: None,
        };
        assert!(!legacy_owner(&record, &missing_certificate, OWNER));
        missing_certificate.certificate = guard.certificate;
        missing_certificate.proposal.manifest.incarnation = SOURCE.into();
        assert!(!legacy_owner(&record, &missing_certificate, OWNER));
    }
}
