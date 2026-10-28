//! Fixed job/work read protocol. Local and linked execution share the same
//! read-only snapshot reader; continuations are bounded, expiring core handles.
use super::scoped::{Command, DelegatedContext, Denial};
use crate::snapshot::inspection::{self as view, Job, JobRef, Work};
use crate::store::{Caller, Store};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

const PAGE_BYTES: usize = 64 * 1024;
const TOKEN_MS: i64 = 600_000;
struct Cursor {
    binding: String,
    grant: String,
    after: i64,
    upper: i64,
    source: String,
    snapshot: String,
    issued: i64,
}
fn page_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS door_inspection_pages(token TEXT PRIMARY KEY,binding TEXT NOT NULL,grant_id TEXT NOT NULL,after_key INTEGER NOT NULL,upper_key INTEGER NOT NULL,source TEXT NOT NULL,snapshot TEXT NOT NULL,issued_ms INTEGER NOT NULL);")?;
    Ok(())
}

/// A generation survives restrict-then-restore, including edits in one clock
/// tick. It is registry authorization bookkeeping, never a home observation write.
pub(super) fn visibility_schema(conn: &Connection) -> Result<()> {
    page_schema(conn)?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS door_runtime_visibility(grant_id TEXT PRIMARY KEY,revision INTEGER NOT NULL);
        CREATE TRIGGER IF NOT EXISTS door_runtime_visibility_changed AFTER UPDATE OF scopes,ships,paused,revoked_ms ON door_clients
        BEGIN INSERT INTO door_runtime_visibility VALUES(NEW.id,1) ON CONFLICT(grant_id) DO UPDATE SET revision=revision+1; DELETE FROM door_inspection_pages WHERE grant_id=NEW.id; END;
        CREATE TRIGGER IF NOT EXISTS door_runtime_visibility_removed AFTER DELETE ON door_clients
        BEGIN DELETE FROM door_runtime_visibility WHERE grant_id=OLD.id; DELETE FROM door_inspection_pages WHERE grant_id=OLD.id; END;")?;
    Ok(())
}
fn operation(command: Command) -> &'static str {
    match command {
        Command::ShipJobs => "ship_jobs",
        Command::JobInspect => "job_inspect",
        Command::ShipWork => "ship_work",
        _ => unreachable!("fixed inspection operation"),
    }
}
pub(super) fn validate_args(command: Command, args: &Value) -> Result<(), Denial> {
    let fields = args.as_object().ok_or(Denial::Arguments)?;
    let allowed: &[&str] = match command {
        Command::ShipJobs => &[
            "ship",
            "state",
            "kind",
            "limit",
            "continuation",
            "include_text",
        ],
        Command::ShipWork => &["ship", "limit", "continuation", "include_text"],
        Command::JobInspect => &["ship", "reference", "include_text"],
        _ => return Err(Denial::Arguments),
    };
    if fields.keys().any(|k| !allowed.contains(&k.as_str()))
        || args["ship"]
            .as_str()
            .is_none_or(|s| s.len() > 128 || !view::handle(s))
        || args
            .get("limit")
            .is_some_and(|v| v.as_u64().is_none_or(|n| !(1..=50).contains(&n)))
        || args
            .get("continuation")
            .is_some_and(|v| v.as_str().is_none_or(|s| uuid::Uuid::parse_str(s).is_err()))
        || args.get("include_text").is_some_and(|v| !v.is_boolean())
        || args.get("state").is_some_and(|v| {
            v.as_str()
                .is_none_or(|s| s != "completed" && !view::STATES.contains(&s))
        })
        || args
            .get("kind")
            .is_some_and(|v| v.as_str().is_none_or(|s| !view::KINDS.contains(&s)))
    {
        return Err(Denial::Arguments);
    }
    if command == Command::JobInspect {
        let r: JobRef =
            serde_json::from_value(args["reference"].clone()).map_err(|_| Denial::Arguments)?;
        if !valid_ref(&r, args["ship"].as_str().unwrap()) {
            return Err(Denial::Arguments);
        }
    }
    Ok(())
}
fn valid_ref(r: &JobRef, ship: &str) -> bool {
    r.ship == ship
        && [&r.origin, &r.incarnation, &r.ship, &r.task_id, &r.id]
            .iter()
            .all(|s| view::handle(s))
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Coverage {
    schema: String,
    scope: String,
    complete: bool,
    other_machines: String,
    machine: String,
    ship: String,
    facet: String,
    state: String,
    mode: String,
    observed_at: Option<i64>,
    counts_complete: bool,
    count_basis: String,
    history: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    schema: String,
    ship: String,
    source: Option<String>,
    snapshot: Option<String>,
    jobs: Vec<Job>,
    work: Vec<Work>,
    job: Option<Job>,
    counts: BTreeMap<String, u64>,
    more: bool,
    continuation: Option<String>,
    coverage: Coverage,
    writes: String,
    cache_writes: String,
}
impl Reply {
    fn empty(ship: &str, command: Command, state: &str, machine: &str) -> Self {
        Self {
            schema: "inspection.v1".into(),
            ship: ship.into(),
            source: None,
            snapshot: None,
            jobs: vec![],
            work: vec![],
            job: None,
            counts: BTreeMap::new(),
            more: false,
            continuation: None,
            coverage: Coverage {
                schema: "coverage.v1".into(),
                scope: "selected-ship".into(),
                complete: false,
                other_machines: "not_requested".into(),
                machine: machine.into(),
                ship: ship.into(),
                facet: if command == Command::ShipWork {
                    "work"
                } else {
                    "jobs"
                }
                .into(),
                state: state.into(),
                mode: "unavailable".into(),
                observed_at: None,
                counts_complete: false,
                count_basis: "authorized_scan_window".into(),
                history: if command == Command::ShipWork {
                    "recorded_tasks"
                } else {
                    "retained_only_prior_history_unknown"
                }
                .into(),
            },
            writes: "none".into(),
            cache_writes: "bounded_registry_continuations_only".into(),
        }
    }
    fn validate(&self, ship: &str, c: &DelegatedContext, show: bool) -> Result<()> {
        ensure!(
            self.schema == "inspection.v1" && self.ship == ship && c.allows(ship),
            Denial::Ship
        );
        ensure!(
            self.jobs.len() <= 50
                && self.work.len() <= 50
                && serde_json::to_vec(self)?.len() <= PAGE_BYTES,
            Denial::Arguments
        );
        ensure!(
            self.source.as_deref().is_none_or(view::handle)
                && self
                    .snapshot
                    .as_deref()
                    .is_none_or(|s| uuid::Uuid::parse_str(s).is_ok())
                && self
                    .continuation
                    .as_deref()
                    .is_none_or(|s| uuid::Uuid::parse_str(s).is_ok())
                && self.writes == "none"
                && self.cache_writes == "bounded_registry_continuations_only",
            Denial::Arguments
        );
        ensure!(
            self.counts
                .keys()
                .all(|k| view::STATES.contains(&k.as_str())
                    || [
                        "completed",
                        "ready",
                        "offered",
                        "claimed",
                        "executing",
                        "waiting",
                        "stopped",
                        "fault",
                        "unscheduled",
                        "scheduled",
                        "unscheduled_total",
                        "backlog"
                    ]
                    .contains(&k.as_str())),
            Denial::Arguments
        );
        let coverage = &self.coverage;
        ensure!(
            coverage.schema == "coverage.v1"
                && coverage.scope == "selected-ship"
                && coverage.ship == ship
                && view::handle(&coverage.machine)
                && coverage.other_machines == "not_requested"
                && ["jobs", "work"].contains(&coverage.facet.as_str())
                && ["covered", "unavailable", "unsupported", "reverse_only"]
                    .contains(&coverage.state.as_str())
                && ["local_live", "remote_live", "unavailable"].contains(&coverage.mode.as_str())
                && coverage.count_basis == "authorized_scan_window"
                && ["recorded_tasks", "retained_only_prior_history_unknown"]
                    .contains(&coverage.history.as_str()),
            Denial::Arguments
        );
        let check = |j: &Job| -> Result<()> {
            ensure!(
                j.owner == ship
                    && valid_ref(&j.reference, ship)
                    && self.source.as_ref() == Some(&j.reference.origin)
                    && view::STATES.contains(&j.state.as_str())
                    && view::KINDS.contains(&j.kind.as_str())
                    && j.worker
                        .as_deref()
                        .is_none_or(|w| view::handle(w) && c.allows(w)),
                Denial::Ship
            );
            ensure!(
                j.harness
                    .as_deref()
                    .is_none_or(|h| view::HARNESSES.contains(&h))
                    && j.liveness == "unknown_recorded_state"
                    && (j.exit == "not_recorded" || view::STATES.contains(&j.exit.as_str())),
                Denial::Arguments
            );
            for age in [&j.created, &j.spawned, &j.finished, &j.state_since] {
                validate_age(age)?;
            }
            for r in [
                &j.parent,
                &j.retry_of,
                &j.resume_of,
                &j.relaunch_of,
                &j.resumed_as,
            ]
            .into_iter()
            .flatten()
            {
                ensure!(
                    valid_ref(r, ship) && r.origin == j.reference.origin,
                    Denial::Ship
                );
            }
            if let Some(w) = &j.wait {
                validate_wait(w, ship, &j.reference.origin, show)?;
            }
            for t in [&j.progress, &j.error].into_iter().flatten() {
                validate_text(t, show)?;
            }
            Ok(())
        };
        for j in self.jobs.iter().chain(self.job.iter()) {
            check(j)?;
        }
        for w in &self.work {
            ensure!(
                w.owner == ship
                    && view::handle(&w.task_id)
                    && w.handoff_to
                        .as_deref()
                        .is_none_or(|s| view::handle(s) && c.allows(s)),
                Denial::Ship
            );
            ensure!(
                view::TASK_STATES.contains(&w.task_state.as_str())
                    && [
                        "ready",
                        "offered",
                        "claimed",
                        "executing",
                        "waiting",
                        "stopped",
                        "fault",
                        "unscheduled"
                    ]
                    .contains(&w.state.as_str()),
                Denial::Arguments
            );
            for age in [&w.created, &w.last_checked, &w.next_check] {
                validate_age(age)?;
            }
            validate_text(&w.reason, show)?;
            if let Some(r) = &w.job {
                ensure!(
                    valid_ref(r, ship) && self.source.as_ref() == Some(&r.origin),
                    Denial::Ship
                );
            }
            if let Some(wait) = &w.wait {
                validate_wait(wait, ship, self.source.as_deref().unwrap_or_default(), show)?;
            }
        }
        Ok(())
    }
}
fn validate_age(age: &view::Age) -> Result<()> {
    ensure!(
        [
            "creation",
            "payload_spawn",
            "finish",
            "state_transition",
            "recorded_text",
            "park_updated",
            "wait_transition_unknown",
            "schedule_creation",
            "schedule_check",
            "not_before"
        ]
        .contains(&age.basis.as_str())
            && age.age_ms.is_none_or(|n| n >= 0),
        Denial::Arguments
    );
    Ok(())
}
fn validate_text(t: &view::Text, show: bool) -> Result<()> {
    validate_age(&t.at)?;
    ensure!(
        [
            "progress",
            "park_next",
            "schedule_reason",
            "legacy_or_draft",
            "job_outcome",
            "runtime_reason"
        ]
        .contains(&t.category.as_str()),
        Denial::Arguments
    );
    ensure!(
        t.text.as_ref().is_none_or(|s| show
            && s.chars().count() <= 512
            && !s.chars().any(char::is_control)
            && !s.chars().any(|c| matches!(c, '/' | '\\' | '@' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')))
            && t.id.as_deref().is_none_or(view::handle),
        Denial::Arguments
    );
    Ok(())
}
fn validate_wait(w: &view::Wait, ship: &str, source: &str, show: bool) -> Result<()> {
    ensure!(
        [
            "capacity",
            "disk",
            "job",
            "task",
            "request",
            "time",
            "remote",
            "attention",
            "manual"
        ]
        .contains(&w.kind.as_str()),
        Denial::Arguments
    );
    validate_age(&w.since)?;
    if let Some(r) = &w.job {
        ensure!(valid_ref(r, ship) && r.origin == source, Denial::Ship);
    }
    ensure!(
        w.task_id.as_deref().is_none_or(view::handle),
        Denial::Arguments
    );
    if let Some(t) = &w.next {
        validate_text(t, show)?;
    }
    Ok(())
}
fn token_clock() -> Result<(String, i64)> {
    #[cfg(target_os = "linux")]
    {
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        ensure!(
            unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) } == 0,
            Denial::ExecutionFailed
        );
        Ok((
            boot,
            ts.tv_sec.saturating_mul(1000) + ts.tv_nsec / 1_000_000,
        ))
    }
    #[cfg(target_os = "macos")]
    {
        // Both values are kernel-wide, so distinct gateway processes can page.
        // Darwin CLOCK_MONOTONIC_RAW is mach_continuous_time in nanoseconds:
        // it includes sleep and is not adjusted by wall-clock synchronization.
        let mut boot = libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        let mut size = std::mem::size_of_val(&boot);
        ensure!(
            unsafe {
                libc::sysctlbyname(
                    c"kern.boottime".as_ptr(),
                    (&mut boot as *mut libc::timeval).cast(),
                    &mut size,
                    std::ptr::null_mut(),
                    0,
                )
            } == 0
                && size == std::mem::size_of_val(&boot),
            Denial::ExecutionFailed
        );
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        ensure!(
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut ts) } == 0,
            Denial::ExecutionFailed
        );
        Ok((
            format!("boot:{}:{}", boot.tv_sec, boot.tv_usec),
            ts.tv_sec.saturating_mul(1000) + ts.tv_nsec / 1_000_000,
        ))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        // No proved reboot-bound clock on this platform: fail closed across
        // processes rather than extend a token's lifetime on a wall-clock jump.
        Ok((format!("process:{}", std::process::id()), Store::now_ms()))
    }
}
fn binding(command: Command, args: &Value, c: &DelegatedContext, clock: &str) -> String {
    let mut authority = c.gateway();
    authority.as_object_mut().unwrap().remove("result_id");
    let mut filter = args.clone();
    filter.as_object_mut().unwrap().remove("continuation");
    crate::core::hash(&json!([authority, operation(command), filter, clock]))
}

pub(super) fn request(command: Command, args: &Value, c: &DelegatedContext) -> Result<Value> {
    validate_args(command, args)?;
    ensure!(c.has("fleet:read"), Denial::Scope);
    let ship = args["ship"].as_str().ok_or(Denial::Arguments)?;
    ensure!(c.allows(ship), Denial::Ship);
    let conn = super::runtime::registry_readonly()?;
    let local: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM ships WHERE name=?1 AND retired_ms IS NULL)",
        [ship],
        |r| r.get(0),
    )?;
    let mut reply = if local {
        local_read(&crate::fleet::registry()?, command, args, c)?
    } else {
        let machine: Option<String> = conn
            .query_row(
                "SELECT machine FROM machine_ships WHERE name=?1 ORDER BY machine LIMIT 1",
                [ship],
                |r| r.get(0),
            )
            .optional()?;
        let machine = machine.ok_or(Denial::Ship)?;
        let callable: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM machines WHERE name=?1 AND role='collects')",
            [&machine],
            |r| r.get(0),
        )?;
        if !callable {
            Reply::empty(ship, command, "reverse_only", &machine)
        } else {
            match crate::machines::inspection_of(
                &machine,
                &json!({"context":c.gateway(),"operation":operation(command),"args":args}),
            ) {
                Ok(v) => {
                    let mut r: Reply = serde_json::from_value(v)?;
                    r.validate(ship, c, args["include_text"] == true && c.has("mail:read"))?;
                    r.coverage.machine = machine.clone();
                    r.coverage.mode = "remote_live".into();
                    r
                }
                Err(e) if e.downcast_ref::<Denial>().is_some() => return Err(e),
                Err(e) => Reply::empty(
                    ship,
                    command,
                    if e.to_string().contains("unknown gateway command") {
                        "unsupported"
                    } else {
                        "unavailable"
                    },
                    &machine,
                ),
            }
        }
    };
    reply.validate(ship, c, args["include_text"] == true && c.has("mail:read"))?;
    // A remote does not define our local receipt age. This read deliberately
    // does not reuse/cache replies or make a process-liveness claim.
    reply.coverage.ship = ship.into();
    Ok(serde_json::to_value(reply)?)
}

/// Dedicated linked-core allowlist, never a client-selected RPC/argv tunnel.
pub(crate) fn serve(body: &Value) -> Result<Value> {
    ensure!(
        body.as_object().is_some_and(|o| o.len() == 3
            && ["context", "operation", "args"]
                .iter()
                .all(|k| o.contains_key(*k))),
        Denial::Arguments
    );
    let c = DelegatedContext::from_gateway(&body["context"])?;
    ensure!(c.has("fleet:read"), Denial::Scope);
    let command = match body["operation"].as_str() {
        Some("ship_jobs") => Command::ShipJobs,
        Some("job_inspect") => Command::JobInspect,
        Some("ship_work") => Command::ShipWork,
        _ => return Err(Denial::Arguments.into()),
    };
    validate_args(command, &body["args"])?;
    let conn = crate::fleet::registry()?;
    Ok(serde_json::to_value(local_read(
        &conn,
        command,
        &body["args"],
        &c,
    )?)?)
}
fn local_read(
    conn: &Connection,
    command: Command,
    args: &Value,
    c: &DelegatedContext,
) -> Result<Reply> {
    let started = Instant::now();
    let ship = args["ship"].as_str().ok_or(Denial::Arguments)?;
    ensure!(c.has("fleet:read") && c.allows(ship), Denial::Ship);
    page_schema(conn)?;
    let home: Option<String> = conn
        .query_row(
            "SELECT home FROM ships WHERE name=?1 AND retired_ms IS NULL",
            [ship],
            |r| r.get(0),
        )
        .optional()?;
    let home = home.ok_or(Denial::Ship)?;
    use std::os::unix::fs::MetadataExt;
    let path = std::path::Path::new(&home).join("privateer.sqlite");
    crate::store::reject_symlinks(&path)?;
    let before = std::fs::metadata(&path)?;
    let store = Store::open_readonly(std::path::Path::new(&home), Caller::Operator)?;
    let after = std::fs::metadata(&path)?;
    ensure!(
        (before.dev(), before.ino()) == (after.dev(), after.ino()),
        Denial::Ship
    );
    let source = view::origin(&store)?;
    let now = Store::now_ms();
    let show = args["include_text"] == true && c.has("mail:read");
    let tx = rusqlite::Transaction::new_unchecked(
        store.connection(),
        rusqlite::TransactionBehavior::Deferred,
    )?;
    let mut reply = Reply::empty(ship, command, "covered", "local");
    reply.source = Some(source.clone());
    reply.coverage.mode = "local_live".into();
    reply.coverage.observed_at = Some(now);
    if command == Command::JobInspect {
        let wanted: JobRef = serde_json::from_value(args["reference"].clone())?;
        let _budget = crate::needs::ObservationBudget::new(store.connection())?;
        reply.job = Some(view::inspect(&store, ship, &wanted, c, show, now)?);
        reply.coverage.complete = true;
    } else {
        let (clock, token_now) = token_clock()?;
        let binding = binding(command, args, c, &clock);
        let cursor = if let Some(token) = args["continuation"].as_str() {
            conn.query_row("SELECT after_key,upper_key,snapshot,issued_ms FROM door_inspection_pages WHERE token=?1 AND binding=?2 AND source=?3 AND grant_id=?4 AND issued_ms>?5 AND issued_ms<=?6",params![token,binding,source,c.client(),token_now-TOKEN_MS,token_now],|r|Ok(Cursor {binding:binding.clone(),grant:c.client().into(),after:r.get(0)?,upper:r.get(1)?,source:source.clone(),snapshot:r.get(2)?,issued:r.get(3)?})).optional()?.ok_or(Denial::ContinuationInvalid)?
        } else {
            let sql = if command == Command::ShipWork {
                "SELECT COALESCE(MAX(rowid),0) FROM tasks"
            } else {
                "SELECT COALESCE(MAX(rowid),0) FROM jobs"
            };
            Cursor {
                binding,
                grant: c.client().into(),
                after: 0,
                // Internal opaque high-water bound, never a public count.
                upper: store.connection().query_row(sql, [], |r| r.get(0))?,
                source: source.clone(),
                snapshot: uuid::Uuid::new_v4().to_string(),
                issued: token_now,
            }
        };
        let scan = view::scan(
            &store,
            ship,
            c,
            command == Command::ShipWork,
            cursor.after,
            cursor.upper,
            args["state"].as_str(),
            args["kind"].as_str(),
            show,
            now,
        )?;
        reply.counts = scan.counts;
        reply.coverage.counts_complete = scan.complete;
        // Complete here means the recorded selected window, not drained retained
        // history, live workers, or complete scheduling/mail across the fleet.
        reply.coverage.complete = scan.complete;
        reply.snapshot = Some(cursor.snapshot.clone());
        let limit = args["limit"].as_u64().unwrap_or(25) as usize;
        let mut last = cursor.after;
        let mut bytes = 4096;
        let mut more = false;
        if command == Command::ShipWork {
            for (key, row) in scan.work {
                let size = serde_json::to_vec(&row)?.len();
                if reply.work.len() == limit || bytes + size > PAGE_BYTES {
                    more = true;
                    break;
                }
                bytes += size;
                last = key;
                reply.work.push(row);
            }
        } else {
            for (key, row) in scan.jobs {
                let size = serde_json::to_vec(&row)?.len();
                if reply.jobs.len() == limit || bytes + size > PAGE_BYTES {
                    more = true;
                    break;
                }
                bytes += size;
                last = key;
                reply.jobs.push(row);
            }
        }
        if !more {
            last = scan.last;
            more = !scan.drained;
        }
        reply.more = more;
        if more {
            ensure!(last > cursor.after, Denial::MessageTooLarge);
            let tx = rusqlite::Transaction::new_unchecked(
                conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute(
                "DELETE FROM door_inspection_pages WHERE issued_ms<=?1 OR issued_ms>?2",
                params![token_now - TOKEN_MS, token_now],
            )?;
            tx.execute("DELETE FROM door_inspection_pages WHERE grant_id=?1 AND token NOT IN (SELECT token FROM door_inspection_pages WHERE grant_id=?1 ORDER BY issued_ms DESC,rowid DESC LIMIT 31)",[&cursor.grant])?;
            tx.execute("DELETE FROM door_inspection_pages WHERE token NOT IN (SELECT token FROM door_inspection_pages ORDER BY issued_ms DESC,rowid DESC LIMIT 127)",[])?;
            let token = uuid::Uuid::new_v4().to_string();
            tx.execute(
                "INSERT INTO door_inspection_pages VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    token,
                    cursor.binding,
                    cursor.grant,
                    last,
                    cursor.upper,
                    cursor.source,
                    cursor.snapshot,
                    cursor.issued
                ],
            )?;
            tx.commit()?;
            reply.continuation = Some(token);
        }
    }
    tx.rollback()?;
    ensure!(
        view::origin(&store)? == source && view::owned(&store, ship)?,
        Denial::Ship
    );
    // Recheck custody outside the page transaction. A finish is harmless; an
    // ownership or worker move must not release a formerly-visible reference.
    let _budget = crate::needs::ObservationBudget::new(store.connection())?;
    let check_ref = |r: &JobRef| -> Result<()> {
        ensure!(
            started.elapsed() < Duration::from_secs(3),
            Denial::ExecutionFailed
        );
        view::check_reference(&store, ship, r, c)?;
        Ok(())
    };
    for j in reply.jobs.iter().chain(reply.job.iter()) {
        check_ref(&j.reference)?;
        for r in [
            &j.parent,
            &j.retry_of,
            &j.resume_of,
            &j.relaunch_of,
            &j.resumed_as,
        ]
        .into_iter()
        .flatten()
        {
            check_ref(r)?;
        }
        if let Some(w) = &j.wait
            && let Some(r) = &w.job
        {
            check_ref(r)?;
        }
    }
    for w in &reply.work {
        ensure!(
            store.connection().query_row(
                "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1 AND owner=?2)",
                rusqlite::params![w.task_id, ship],
                |r| r.get::<_, bool>(0)
            )?,
            Denial::Ship
        );
        if let Some(r) = &w.job {
            check_ref(r)?;
        }
        if let Some(wait) = &w.wait
            && let Some(id) = &wait.task_id
        {
            ensure!(
                store.connection().query_row(
                    "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1 AND owner=?2)",
                    rusqlite::params![id, ship],
                    |r| r.get::<_, bool>(0)
                )?,
                Denial::Ship
            );
        }
    }
    reply.validate(ship, c, show)?;
    Ok(reply)
}

#[cfg(test)]
mod tests;
