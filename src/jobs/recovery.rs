//! Native, terminal-only reservation recovery. No launch, reconciliation or
//! ordinary store/ledger initialization is reachable from this module.
//!
//! Lock order: resolve existing home/config/ledger first, home writer, ledger
//! writer. The home lock pins terminal custody; the ledger transaction owns the
//! exact hold, permit, result and audit. Never call `load` through reconciliation.
//! This is local operator authority, not a delegated fleet capability. A future
//! receiver must authenticate current grant/visibility and link fences itself.
use super::*;
use crate::admission::resource_actions as durable;
use std::io::{BufRead, BufReader};

const PERMIT_MS: i64 = 600_000;
const OPERATION: &str = "resource.recover.v1";
const EFFECT: &str = "delete_exact_terminal_reservation";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    machine_incarnation: String,
    ledger: String,
    home: String,
    ship: String,
    task: String,
    job: String,
    incarnation: String,
    generation: String,
    fingerprint: String,
}

/// Intended originating authority, committed into the target-side permit.
/// Native calls are operator-only and local (link_incarnation must be null).
/// These fields confer no gateway authority and do not issue a grant.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Authority {
    grant: String,
    resource: String,
    grant_revision: String,
    resource_revision: String,
    visibility_revision: String,
    link_incarnation: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    reference: Reference,
    authority: Authority,
    idempotency_key: String,
}
impl Request {
    fn parse(v: &Value) -> Result<Self> {
        let r: Self =
            serde_json::from_value(v.clone()).map_err(|_| refusal("unsupported_arguments"))?;
        for s in [
            &r.authority.grant,
            &r.authority.resource,
            &r.authority.grant_revision,
            &r.authority.resource_revision,
            &r.authority.visibility_revision,
            &r.idempotency_key,
        ] {
            ensure!(
                !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| (32..=126).contains(&b)),
                refusal("unsupported_arguments")
            );
        }
        // No linked surface exists in PR A. The receiver in PR B must supply
        // authenticated link context, never a client-selected incarnation.
        ensure!(
            r.authority.link_incarnation.is_none(),
            refusal("unsupported")
        );
        Ok(r)
    }
    fn binding(&self) -> Value {
        json!({"operation":OPERATION,"effect":EFFECT,"reference":self.reference,
            "authority":self.authority,"key_hash":core_hash(&json!(self.idempotency_key)),
            "request_digest":core_hash(&json!(self))})
    }
}
fn core_hash(v: &Value) -> String {
    crate::core::hash(v)
}
#[derive(Debug)]
pub(crate) struct Refusal(&'static str);
impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Refusal {}
fn refusal(code: &'static str) -> anyhow::Error {
    Refusal(code).into()
}
fn closed(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<Refusal>().is_some() {
        error
    } else {
        refusal("execution_failed")
    }
}
fn owned(store: &Store, ship: &str) -> Result<()> {
    let local: bool = store.connection().query_row("SELECT EXISTS(SELECT 1 FROM ships WHERE name=?1 AND json_extract(record,'$.location')='here')", [ship], |r| r.get(0))?;
    ensure!(local, refusal("recovery_target_stale"));
    Ok(())
}
fn stale<T>(r: Result<T>) -> Result<T> {
    r.map_err(|_| refusal("recovery_target_stale"))
}
fn file_identity(path: &Path) -> Result<Value> {
    crate::store::reject_symlinks(path)?;
    let m = fs::metadata(path)?;
    ensure!(
        m.is_file() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o022 == 0,
        "unsafe file"
    );
    Ok(json!([m.dev(), m.ino()]))
}
fn home_identity(store: &Store) -> Result<String> {
    let origin: String = store.connection().query_row(
        "SELECT value FROM metadata WHERE key='runtime_origin'",
        [],
        |r| r.get(0),
    )?;
    ensure!(!origin.is_empty(), "home origin missing");
    Ok(core_hash(&json!([
        origin,
        file_identity(&store.home().join("privateer.sqlite"))?,
        store.home()
    ])))
}
struct Target {
    conn: Connection,
    path: PathBuf,
    ledger: String,
    machine: String,
    home: String,
    config: String,
}
impl Target {
    fn open(store: &Store, write: bool) -> Result<Self> {
        let path = ledger_dir(store)?.join("resources.sqlite");
        ensure!(path.is_absolute(), "absolute ledger required");
        let before = file_identity(&path)?;
        let conn = Connection::open_with_flags(
            &path,
            if write {
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            } else {
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            },
        )?;
        conn.busy_timeout(Duration::from_secs(5))?;
        if !write {
            conn.execute_batch("PRAGMA query_only=ON")?;
        } else {
            conn.execute_batch("PRAGMA synchronous=FULL")?;
        }
        ensure!(before == file_identity(&path)?, "ledger replaced");
        let ledger = core_hash(&json!([durable::identity(&conn)?, before]));
        let machine = durable::recovery_identity(&conn)?;
        Ok(Self {
            conn,
            path,
            ledger,
            machine,
            home: home_identity(store)?,
            config: core_hash(&home_config(store.home())?),
        })
    }
    fn recheck(&self, store: &Store) -> Result<()> {
        ensure!(
            ledger_dir(store)?.join("resources.sqlite") == self.path
                && core_hash(&home_config(store.home())?) == self.config
                && home_identity(store)? == self.home
                && core_hash(&json!([
                    durable::identity(&self.conn)?,
                    file_identity(&self.path)?
                ])) == self.ledger
                && durable::recovery_identity(&self.conn)? == self.machine,
            refusal("recovery_target_stale")
        );
        Ok(())
    }
    fn exact(&self, store: &Store, job_id: &str) -> Result<(Reference, Job, Reservation)> {
        let job = load(store, job_id)?;
        let key = reservation_key(store.home(), &job.id, &job.incarnation);
        let (raw, generation): (String, String) = self.conn.query_row(
            "SELECT r.record,g.generation FROM reservations r JOIN reservation_generations g USING(key) WHERE r.key=?1",
            [&key], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let r: Reservation = serde_json::from_str(&raw)?;
        ensure!(
            r.key == key
                && r.home == store.home()
                && r.job_id == job.id
                && r.incarnation == job.incarnation
                && job.ledger == self.path
                && r.task.as_deref() == Some(&job.task_id),
            "mismatched custody"
        );
        let ship = r.ship.as_ref().context("ship missing")?;
        owned(store, ship)?;
        let reference = Reference {
            machine_incarnation: self.machine.clone(),
            ledger: self.ledger.clone(),
            home: self.home.clone(),
            ship: ship.clone(),
            task: job.task_id.clone(),
            job: job.id.clone(),
            incarnation: job.incarnation.clone(),
            generation,
            fingerprint: core_hash(&json!(raw)),
        };
        Ok((reference, job, r))
    }
}

/// Missing/legacy identities are not affirmative absence. Inspect the union of
/// home and ledger evidence: a lagging copy must never erase a live descendant.
fn absent(job: &Job, hold: &Reservation, now: i64) -> Result<()> {
    ensure!(terminal(&job.status), refusal("recovery_not_eligible"));
    ensure!(
        now.saturating_sub(hold.created_ms) > START_GRACE_MS
            && now.saturating_sub(job.created_ms) > START_GRACE_MS
            && job
                .launch_ms
                .is_some_and(|t| now.saturating_sub(t) > START_GRACE_MS),
        refusal("recovery_not_eligible")
    );
    // Native tracking is bounded at 256 escaped identities. A saturated
    // snapshot may have omitted descendants, so it is not proof of absence.
    ensure!(
        job.escaped.len() < 256 && hold.escaped.len() < 256,
        refusal("recovery_identity_unknown")
    );
    let identities = [
        job.supervisor.as_ref(),
        job.payload.as_ref(),
        hold.supervisor.as_ref(),
        hold.payload.as_ref(),
    ];
    ensure!(
        identities.iter().all(|p| p.is_some()),
        refusal("recovery_identity_unknown")
    );
    for p in identities
        .into_iter()
        .flatten()
        .chain(job.escaped.iter())
        .chain(hold.escaped.iter())
    {
        ensure!(
            p.pid > 1 && !p.started.is_empty(),
            refusal("recovery_identity_unknown")
        );
        #[cfg(target_os = "linux")]
        {
            let parsed = p
                .started
                .strip_prefix("linux:")
                .and_then(|s| s.rsplit_once(':'));
            ensure!(
                parsed
                    .is_some_and(|(boot, ticks)| Uuid::parse_str(boot).is_ok()
                        && ticks.parse::<u64>().is_ok())
                    && p.pid_namespace.is_some()
                    && pid_namespace().is_some(),
                refusal("recovery_identity_unknown")
            );
        }
        ensure!(addressable(p), refusal("recovery_identity_unknown"));
        ensure!(
            !same_process(p).map_err(|_| refusal("recovery_identity_unknown"))?,
            refusal("recovery_process_alive")
        );
    }
    for p in [
        job.payload.as_ref().unwrap(),
        hold.payload.as_ref().unwrap(),
    ] {
        ensure!(
            !group_alive(p).map_err(|_| refusal("recovery_identity_unknown"))?,
            refusal("recovery_process_alive")
        );
    }
    Ok(())
}
fn audit(conn: &Connection, now: i64, record: Value) -> Result<()> {
    ensure!(!conn.is_autocommit(), "writer required");
    conn.execute(
        "INSERT INTO resource_recovery_audit(created_ms,record) VALUES(?1,?2)",
        params![now, record.to_string()],
    )?;
    conn.execute("DELETE FROM resource_recovery_audit WHERE seq NOT IN (SELECT seq FROM resource_recovery_audit ORDER BY seq DESC LIMIT ?1)", [durable::AUDIT_KEEP])?;
    Ok(())
}
fn prune(conn: &Connection, now: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM resource_recovery_results WHERE created_ms < ?1",
        [now.saturating_sub(durable::RETENTION_MS)],
    )?;
    conn.execute(
        "DELETE FROM resource_action_results WHERE created_ms < ?1",
        [now.saturating_sub(durable::RETENTION_MS)],
    )?;
    conn.execute(
        "DELETE FROM resource_recovery_permits WHERE expires_ms < ?1",
        [now.saturating_sub(durable::RETENTION_MS)],
    )?;
    Ok(())
}
fn capacity(conn: &Connection, table: &str, grant: &str) -> Result<()> {
    let sql = match table {
        "results" => {
            ensure!(
                durable::has_capacity(conn, grant)?,
                refusal("retry_capacity")
            );
            return Ok(());
        }
        _ => {
            "SELECT COUNT(*),COALESCE(SUM(json_extract(binding,'$.authority.grant')=?1),0) FROM resource_recovery_permits"
        }
    };
    let (all, own): (i64, i64) = conn.query_row(sql, [grant], |r| Ok((r.get(0)?, r.get(1)?)))?;
    ensure!(
        all < durable::GLOBAL && own < durable::PER_GRANT,
        refusal("retry_capacity")
    );
    Ok(())
}
fn issue(store: &Store, request: &Request, now: i64, provenance: &Value) -> Result<Value> {
    let target = stale(Target::open(store, true))?;
    let _home = store.write_transaction()?;
    let tx = rusqlite::Transaction::new_unchecked(
        &target.conn,
        rusqlite::TransactionBehavior::Immediate,
    )?;
    stale(target.recheck(store))?;
    let (reference, job, hold) = stale(target.exact(store, &request.reference.job))?;
    ensure!(
        reference == request.reference,
        refusal("recovery_target_stale")
    );
    absent(&job, &hold, now)?;
    prune(&tx, now)?;
    capacity(&tx, "permits", &request.authority.grant)?;
    let id = Uuid::new_v4().to_string();
    let expires = now.saturating_add(PERMIT_MS);
    let binding = request.binding();
    tx.execute("INSERT INTO resource_recovery_permits(id,created_ms,expires_ms,binding,provenance) VALUES(?1,?2,?3,?4,?5)", params![id,now,expires,binding.to_string(),provenance.to_string()])?;
    audit(
        &tx,
        now,
        json!({"event":"permit_issued","approval_id":id,"binding":binding,"provenance":provenance,"expires_ms":expires}),
    )?;
    stale(target.recheck(store))?;
    tx.commit().map_err(|_| refusal("outcome_unknown"))?;
    Ok(json!({"approval_id":id,"expires_ms":expires,"binding":binding}))
}
fn apply(
    store: &Store,
    request: &Request,
    approval: &str,
    clock: impl FnOnce() -> i64,
) -> Result<Value> {
    let target = stale(Target::open(store, true))?;
    let _home = store.write_transaction()?;
    let tx = rusqlite::Transaction::new_unchecked(
        &target.conn,
        rusqlite::TransactionBehavior::Immediate,
    )?;
    // Lock waits must not extend a permit's lifetime. Sample only once both
    // writers are held; tests supply a clock without wall-time assumptions.
    let now = clock();
    stale(target.recheck(store))?;
    ensure!(
        request.reference.ledger == target.ledger
            && request.reference.home == target.home
            && request.reference.machine_incarnation == target.machine,
        refusal("recovery_target_stale")
    );
    stale(owned(store, &request.reference.ship))?;
    let key = core_hash(&json!(request.idempotency_key));
    let digest = core_hash(&json!([request.binding(), approval]));
    prune(&tx, now)?;
    let old: Option<(String, String)> = tx
        .query_row(
            "SELECT request_hash,result FROM resource_recovery_results WHERE key_hash=?1",
            [&key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((hash, raw)) = old {
        ensure!(hash == digest, refusal("unsupported_arguments"));
        let result: Value = serde_json::from_str(&raw).map_err(|_| refusal("outcome_unknown"))?;
        ensure!(
            result.as_object().is_some_and(|o| o.len() == 6)
                && result["schema"] == "resource.recovery.result.v1"
                && result["operation"] == OPERATION
                && result["applied"] == true
                && result["outcome"] == "recovered"
                && result["reference"] == json!(request.reference)
                && result["result_id"]
                    .as_str()
                    .is_some_and(|s| Uuid::parse_str(s).is_ok()),
            refusal("outcome_unknown")
        );
        stale(target.recheck(store))?;
        tx.commit().map_err(|_| refusal("outcome_unknown"))?;
        return Ok(result);
    }
    struct Permit {
        binding: String,
        expires: i64,
        revoked: Option<i64>,
        consumed: Option<String>,
        provenance: String,
    }
    let permit = tx.query_row(
        "SELECT binding,expires_ms,revoked_ms,consumed_result,provenance FROM resource_recovery_permits WHERE id=?1", [approval],
        |r| Ok(Permit { binding: r.get(0)?, expires: r.get(1)?, revoked: r.get(2)?, consumed: r.get(3)?, provenance: r.get(4)? })).optional()?;
    let Permit {
        binding,
        expires,
        revoked,
        consumed,
        provenance,
    } = permit.ok_or_else(|| refusal("recovery_approval_required"))?;
    let provenance: Value = serde_json::from_str(&provenance)?;
    let binding: Value =
        serde_json::from_str(&binding).map_err(|_| refusal("recovery_approval_stale"))?;
    ensure!(
        expires > now && revoked.is_none() && consumed.is_none() && binding == request.binding(),
        refusal("recovery_approval_stale")
    );
    let (reference, job, hold) = stale(target.exact(store, &request.reference.job))?;
    ensure!(
        reference == request.reference,
        refusal("recovery_target_stale")
    );
    absent(&job, &hold, now)?;
    capacity(&tx, "results", &request.authority.grant)?;
    let id = Uuid::new_v4().to_string();
    let result = json!({"schema":"resource.recovery.result.v1","operation":OPERATION,"result_id":id,
        "reference":reference,"applied":true,"outcome":"recovered"});
    ensure!(
        tx.execute("DELETE FROM reservations WHERE key=?1", [&hold.key])? == 1,
        refusal("recovery_target_stale")
    );
    ensure!(tx.execute("UPDATE resource_recovery_permits SET consumed_result=?2 WHERE id=?1 AND consumed_result IS NULL AND revoked_ms IS NULL", params![approval,id])? == 1, refusal("recovery_approval_stale"));
    tx.execute(
        "INSERT INTO resource_recovery_results VALUES(?1,?2,?3,?4,?5)",
        params![
            key,
            request.authority.grant,
            digest,
            now,
            result.to_string()
        ],
    )?;
    audit(
        &tx,
        now,
        json!({"event":"permit_consumed","operation":OPERATION,"effect":EFFECT,"approval_id":approval,
        "grant":request.authority.grant,"resource":request.authority.resource,"target":reference,"before":"held","after":"absent",
        "result_id":id,"key_hash":key,"provenance":provenance,"binding":request.binding()}),
    )?;
    stale(target.recheck(store))?;
    tx.commit().map_err(|_| refusal("outcome_unknown"))?;
    Ok(result)
}

pub(super) fn dispatch(store: &Store, command: &str, args: &Value) -> Result<Value> {
    store.caller().require_operator(command)?;
    (|| match command {
        "resource.recovery.reference" => {
            let target = stale(Target::open(store, false))?;
            let id = args["job_id"]
                .as_str()
                .ok_or_else(|| refusal("unsupported_arguments"))?;
            let (reference, _, _) = stale(target.exact(store, id))?;
            stale(target.recheck(store))?;
            Ok(json!({"reference":reference}))
        }
        "resource.recovery.apply" => {
            let request = Request::parse(&args["request"])?;
            let approval = args["approval_id"]
                .as_str()
                .ok_or_else(|| refusal("recovery_approval_required"))?;
            apply(store, &request, approval, Store::now_ms)
        }
        "resource.recovery_permit.revoke" => {
            let id = args["approval_id"]
                .as_str()
                .ok_or_else(|| refusal("unsupported_arguments"))?;
            let target = stale(Target::open(store, true))?;
            let tx = rusqlite::Transaction::new_unchecked(
                &target.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            stale(target.recheck(store))?;
            let now = Store::now_ms();
            ensure!(tx.execute("UPDATE resource_recovery_permits SET revoked_ms=COALESCE(revoked_ms,?2) WHERE id=?1", params![id,now])? == 1, refusal("recovery_approval_required"));
            audit(&tx, now, json!({"event":"permit_revoked","approval_id":id}))?;
            tx.commit().map_err(|_| refusal("outcome_unknown"))?;
            Ok(json!({"revoked":true}))
        }
        _ => Err(refusal("unsupported")),
    })().map_err(closed)
}

/// Existing-store CLI entry: issuance is deliberately not a dispatched command
/// (therefore inaccessible over RPC, peers, doors and model approval echoes).
pub fn cli(home: &Path, command: &str, args: &Value, caller: Caller) -> Result<Value> {
    caller.require_operator(command)?;
    let store = stale(if command == "resource.recovery.reference" {
        Store::open_readonly(home, caller)
    } else {
        Store::open_existing_action(home, caller)
    })?;
    // Recovery can wait behind a native writer, but never initializes a store.
    store.connection().busy_timeout(LEDGER_BUSY)?;
    if command != "resource.recovery_permit.issue" {
        ensure!(args.is_object(), refusal("unsupported_arguments"));
        crate::check_args(command, args).map_err(|_| refusal("unsupported_arguments"))?;
        return dispatch(&store, command, args);
    }
    for marker in ["PRIVATEER_ROLE", "PRIVATEER_JOB"] {
        ensure!(
            std::env::var_os(marker).is_none(),
            "permit issuance requires a local operator outside agent/job sessions"
        );
    }
    ensure!(
        args.as_object()
            .is_some_and(|o| o.len() == 1 && o.contains_key("request")),
        "unsupported_arguments"
    );
    let request = Request::parse(&args["request"])?;
    // Refuse stale/ineligible requests before asking for operator consent;
    // issue() repeats all checks under custody and ledger locks after consent.
    let target = stale(Target::open(&store, false))?;
    let (reference, job, hold) = stale(target.exact(&store, &request.reference.job))?;
    ensure!(
        reference == request.reference,
        refusal("recovery_target_stale")
    );
    absent(&job, &hold, Store::now_ms())?;
    stale(target.recheck(&store))?;
    drop(target);
    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("recovery permit requires the operator at a controlling terminal")?;
    let fd = std::os::fd::AsRawFd::as_raw_fd(&tty);
    ensure!(
        unsafe { libc::tcgetpgrp(fd) == libc::getpgrp() },
        "recovery permit requires the operator in the terminal foreground"
    );
    let challenge = Uuid::new_v4().to_string();
    let challenge = &challenge[..8];
    unsafe { libc::tcflush(fd, libc::TCIFLUSH) };
    writeln!(
        tty,
        "Single-use recovery permit (expires in ten minutes):\n{}\nType {challenge} to confirm:",
        serde_json::to_string_pretty(&request.binding())?
    )?;
    tty.flush()?;
    let mut answer = String::new();
    BufReader::new(tty.try_clone()?.take(128)).read_line(&mut answer)?;
    ensure!(answer.trim() == challenge, "recovery permit not confirmed");
    issue(
        &store,
        &request,
        Store::now_ms(),
        &json!({"channel":"target_operator_terminal","confirmation_id":Uuid::new_v4().to_string(),"uid":unsafe { libc::geteuid() }}),
    ).map_err(closed)
}

#[cfg(test)]
mod tests;
