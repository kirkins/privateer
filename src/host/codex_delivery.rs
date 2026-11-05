//! Codex attended-mail delivery grants and the hosted supervisor's runner tick.
//!
//! Delivery needs a captain (operator) grant bound to the ship's exact native
//! session. Every queue turn is possibly unattended: the runner reserves one
//! turn here BEFORE its project flock, and the count is never refunded by ack,
//! rebind, restart or relaunch. Only a new captain grant refills it. Revoking
//! publishes custody poison inside the writer transaction (SQLite writer, then
//! poison, then flock), so it is ordered before any later queue spawn.
use crate::store::{Caller, Store};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) const DEFAULT_BUDGET: i64 = 200;
const MAX_BUDGET: i64 = 1000;

pub(crate) const ACCEPTED: crate::Accepted = &[
    ("attended.codex.grant", &["ship", "session", "budget"]),
    ("attended.codex.revoke", &["ship"]),
    ("attended.codex.status", &["ship"]),
    (
        "attended.codex.reserve",
        &["ship", "holder", "session", "grant_id", "nonce"],
    ),
    (
        "attended.codex.reconcile",
        &["ship", "holder", "session", "notification_id"],
    ),
    (
        "attended.codex.resolve",
        &[
            "ship",
            "holder",
            "session",
            "notification_id",
            "operation",
            "reason",
        ],
    ),
    ("attended.codex.session", &["ship"]),
];

fn init(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS codex_delivery_grants(
           ship TEXT PRIMARY KEY, grant_id TEXT NOT NULL UNIQUE, session TEXT NOT NULL,
           budget INTEGER NOT NULL, spent INTEGER NOT NULL DEFAULT 0,
           granted_ms INTEGER NOT NULL, low_notified INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE IF NOT EXISTS codex_delivery_reservations(
           grant_id TEXT NOT NULL, nonce TEXT NOT NULL, reserved_ms INTEGER NOT NULL,
           PRIMARY KEY(grant_id, nonce));",
    )?;
    Ok(())
}

fn canonical(args: &Value, key: &str) -> Result<String> {
    let raw = args[key]
        .as_str()
        .with_context(|| format!("{key} is required"))?;
    let parsed = uuid::Uuid::parse_str(raw)
        .ok()
        .map(|u| u.to_string())
        .filter(|u| u == raw);
    parsed.with_context(|| format!("{key} must be a canonical lowercase UUID"))
}

fn ship(args: &Value) -> Result<&str> {
    args["ship"].as_str().context("ship is required")
}

fn status(conn: &rusqlite::Connection, ship: &str) -> Result<Value> {
    let grant = conn
        .query_row(
            "SELECT grant_id, session, budget, spent, granted_ms FROM codex_delivery_grants WHERE ship=?1",
            [ship],
            |r| {
                let (budget, spent): (i64, i64) = (r.get(2)?, r.get(3)?);
                Ok(json!({"grant_id": r.get::<_, String>(0)?, "session": r.get::<_, String>(1)?,
                    "budget": budget, "spent": spent, "remaining": budget - spent,
                    "granted_ms": r.get::<_, i64>(4)?}))
            },
        )
        .optional()?;
    Ok(json!({"ship": ship, "grant": grant}))
}

pub(crate) fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    init(store.connection())?;
    match command {
        "attended.codex.grant" => grant(store, &args),
        "attended.codex.revoke" => revoke(store, &args),
        "attended.codex.status" => status(store.connection(), ship(&args)?),
        "attended.codex.reserve" => reserve(store, &args),
        "attended.codex.reconcile" | "attended.codex.resolve" => settle(store, command, &args),
        "attended.codex.session" => session(store, &args),
        _ => bail!("unknown command {command}"),
    }
}

/// This folder's ship, on Linux: the controls run the attended adapter's own
/// state machine beside the ship's `.codex/privateer-attended` directory.
fn folder(store: &Store, ship: &str) -> Result<PathBuf> {
    ensure!(
        cfg!(target_os = "linux"),
        "hosted Codex delivery is Linux-only"
    );
    ensure!(
        crate::presence::folder_ship(store).as_deref() == Some(ship),
        "run this with {ship}'s own ship folder home (--home)"
    );
    Ok(store
        .home()
        .parent()
        .context("ship folder")?
        .canonicalize()?)
}

const CONTROL_OUTPUT: u64 = 1 << 16;

/// One bounded adapter child: a JSON request on stdin, one JSON answer on
/// stdout, a clean environment, its own process group and the tick's cap.
fn adapter(folder: &Path, argv: [&str; 2], target: &Path, request: &Value) -> Result<Value> {
    use std::io::{Read, Write};
    let script = crate::cli::plugins_dir()?
        .join("attended-codex")
        .join(argv[0]);
    let mut command = Command::new(PYTHON);
    command
        .args(["-I", "-B"])
        .arg(script)
        .arg(argv[1])
        .arg(target)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .current_dir(folder)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // The reservation talks to the daemon of the CODEX_HOME this runtime's
    // panes inherit; nothing else from this environment reaches the child.
    for key in ["HOME", "CODEX_HOME"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command
        .spawn()
        .context("start the attended Codex adapter")?;
    let stdout = child.stdout.take().context("adapter stdout")?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.take(CONTROL_OUTPUT).read_to_end(&mut out);
        out
    });
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(request.to_string().as_bytes());
    }
    let started = Instant::now();
    while !exited(&child) && started.elapsed() < CAP {
        std::thread::sleep(Duration::from_millis(20));
    }
    let timed_out = !exited(&child);
    kill(&mut child);
    let status = child.wait()?;
    let out = reader.join().unwrap_or_default();
    ensure!(!timed_out, "the attended Codex adapter timed out");
    let answer: Value =
        serde_json::from_slice(&out).context("the attended Codex adapter gave no answer")?;
    if !status.success() {
        bail!(
            "attended Codex adapter refused: {}",
            answer["error"].as_str().unwrap_or("no reason")
        );
    }
    Ok(answer)
}

/// Captain-only reconcile/resolve of one offer. The adapter rechecks current
/// binding, custody and lease; evidence settles first, and a resolution is
/// audited here (who) and in the ledger (why). Nothing is deleted.
fn settle(store: &Store, command: &str, args: &Value) -> Result<Value> {
    store.caller().require_operator(command)?;
    let ship = ship(args)?;
    let folder = folder(store, ship)?;
    let mut request = json!({"ship": ship, "holder": args["holder"].as_str().context("holder is required")?,
        "session": canonical(args, "session")?, "notification_id": canonical(args, "notification_id")?});
    let verb = if command == "attended.codex.resolve" {
        let operation = args["operation"].as_str().unwrap_or_default();
        ensure!(
            matches!(operation, "abandon" | "mark_presented"),
            "operation must be abandon or mark_presented"
        );
        let reason = args["reason"].as_str().unwrap_or_default();
        ensure!(
            !reason.trim().is_empty() && reason.len() <= 512,
            "reason must be 1..512 bytes"
        );
        request["operation"] = json!(operation);
        request["reason"] = json!(reason);
        "resolve"
    } else {
        "reconcile"
    };
    let launch = folder.join(".codex/privateer-attended/launch.json");
    let answer = adapter(&folder, ["codex_runner.py", verb], &launch, &request)?;
    if answer["resolved"] == true {
        store.audit(
            command,
            &json!({"ship": ship, "session": request["session"], "notification_id": request["notification_id"],
                "operation": request["operation"], "reason": request["reason"], "by": "operator",
                "audit_id": answer["audit"]["id"]}),
        )?;
    }
    Ok(answer)
}

/// A hosted Codex first mate's `first_mate.options.effort`, if any. Codex
/// 0.166 drops a thread's effort at `codex resume`; the pane sets it again
/// with `-c model_reasoning_effort`, which the binding pairs with an explicit
/// `--remote` to the verified shared daemon (a bare `-c` leaves the daemon).
/// Only Codex's supported effort values become TOML strings in argv;
/// anything else is refused, never quoted or dropped.
pub(crate) fn effort(options: &Value) -> Result<Option<&str>> {
    let Some(value) = options.get("effort").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let effort = value
        .as_str()
        .filter(|e| matches!(*e, "minimal" | "low" | "medium" | "high" | "xhigh" | "none"))
        .context(
            "first_mate.options.effort for a hosted Codex pane must be minimal, low, medium, high, xhigh or none",
        )?;
    Ok(Some(effort))
}

/// Captain-only: create the ship's exact native session on the verified
/// shared daemon (durable, zero-turn), make it first_mate.codex_session and
/// relaunch the hosted pane onto `codex resume <UUID>`. A grant naming the
/// previous session is revoked first. Only the trusted resume callback binds.
fn session(store: &mut Store, args: &Value) -> Result<Value> {
    store.caller().require_operator("attended.codex.session")?;
    let ship = ship(args)?.to_owned();
    let folder = folder(store, &ship)?;
    let record = ship_record(store.connection(), &ship)?;
    let profile = &record["first_mate"];
    ensure!(
        profile["manifest"]
            .as_str()
            .and_then(crate::bundled::harness)
            .as_deref()
            == Some("codex"),
        "{ship}'s first mate is not bundled:codex"
    );
    // The pane applies its own --model and effort at attach (`codex resume`
    // re-applies them); thread/start records them too, for the read-back.
    let effort = effort(&profile["options"])?;
    let model = profile["options"]["model"].clone();
    let request = json!({"ship": ship, "cwd": folder.to_str().context("ship folder path")?,
        "model": model, "effort": effort, "name": format!("{ship} first mate"),
        "current": profile["codex_session"]});
    let target = folder.join(".codex/privateer-attended/reservation.json");
    let reserved = adapter(&folder, ["codex_reserve.py", "create"], &target, &request)?;
    let (session, record) = assign(store, &ship, &folder, &profile["manifest"], &reserved)?;
    let launch = if crate::host::hosted(&record["first_mate"]) {
        match crate::host::upgrade::relaunch(store, &ship, false) {
            Ok(report) => report,
            Err(error) => json!({"error": format!("{error:#}")}),
        }
    } else {
        json!("next_launch")
    };
    Ok(
        json!({"ship": ship, "session": session, "reservation": reserved, "launch": launch,
        "settings": "the hosted pane applies first_mate.options model (--model) and effort (-c model_reasoning_effort over an explicit --remote to the verified shared daemon) at attach; the resume binding reads both back"}),
    )
}

/// Commit a verified reservation: revoke (poison) a grant naming another
/// session, set first_mate.codex_session and audit, in one writer transaction.
fn assign(
    store: &Store,
    ship: &str,
    folder: &Path,
    manifest: &Value,
    reserved: &Value,
) -> Result<(String, Value)> {
    ensure!(
        reserved["state"] == "verified",
        "Codex session reservation was not verified"
    );
    let session = canonical(reserved, "session")?;
    let tx = store.write_transaction()?;
    let mut record = ship_record(&tx, ship)?;
    ensure!(
        record["first_mate"]["manifest"] == *manifest,
        "{ship}'s first mate changed during the reservation"
    );
    let granted: Option<String> = tx
        .query_row(
            "SELECT session FROM codex_delivery_grants WHERE ship=?1",
            [ship],
            |r| r.get(0),
        )
        .optional()?;
    let revoked = granted.is_some_and(|granted| granted != session);
    if revoked {
        tx.execute(
            "DELETE FROM codex_delivery_reservations WHERE grant_id IN
               (SELECT grant_id FROM codex_delivery_grants WHERE ship=?1)",
            [ship],
        )?;
        tx.execute("DELETE FROM codex_delivery_grants WHERE ship=?1", [ship])?;
        super::codex_custody::delivery_revoked(store, ship)?;
    }
    let previous = record["first_mate"]["codex_session"].take();
    record["first_mate"]["codex_session"] = json!(session);
    tx.execute(
        "UPDATE ships SET record=?2 WHERE name=?1",
        params![ship, record.to_string()],
    )?;
    store.audit(
        "attended.codex.session",
        &json!({"ship": ship, "session": session, "previous": previous,
            "generation": reserved["generation"], "rollout": reserved["rollout"],
            "settings": reserved["settings"], "grant_revoked": revoked}),
    )?;
    tx.commit()?;
    // Only after the writer commit is durable, mark the reservation assigned so
    // claim never adopts this UUID again. A crash before this marker leaves it
    // verified, and the next same-commit retry continues with that exact UUID.
    let target = folder.join(".codex/privateer-attended/reservation.json");
    let cwd = folder.to_str().context("ship folder path")?;
    let assignment = adapter(
        folder,
        ["codex_reserve.py", "assign"],
        &target,
        &json!({"cwd": cwd, "generation": reserved["generation"], "session": session}),
    )
    .with_context(|| {
        format!(
            "first_mate.codex_session was already committed as {session}; retrying this same \
             session control continues that exact session"
        )
    })?;
    ensure!(
        assignment["assigned"] == true && assignment["session"] == session.as_str(),
        "the attended Codex adapter did not record the assignment, but \
         first_mate.codex_session was already committed as {session}; retrying this same \
         session control continues that exact session"
    );
    Ok((session, record))
}

fn ship_record(conn: &rusqlite::Connection, ship: &str) -> Result<Value> {
    let record: Option<String> = conn
        .query_row("SELECT record FROM ships WHERE name=?1", [ship], |r| {
            r.get(0)
        })
        .optional()?;
    let record: Value = serde_json::from_str(&record.context("unknown ship")?)?;
    ensure!(
        record["location"] == "here",
        "{ship} is not a ship hosted by this home"
    );
    Ok(record)
}

/// Captain-only: authorize delivery to the ship's configured exact session,
/// with an explicit possibly-unattended turn budget. A grant for another
/// session first revokes (poisons) the old one.
fn grant(store: &Store, args: &Value) -> Result<Value> {
    store.caller().require_operator("attended.codex.grant")?;
    let ship = ship(args)?;
    let session = canonical(args, "session")?;
    let budget = match args.get("budget") {
        None | Some(Value::Null) => DEFAULT_BUDGET,
        Some(v) => v
            .as_i64()
            .filter(|b| (1..=MAX_BUDGET).contains(b))
            .with_context(|| format!("budget must be 1..{MAX_BUDGET}"))?,
    };
    let tx = store.write_transaction()?;
    let record = ship_record(&tx, ship)?;
    ensure!(
        record["first_mate"]["codex_session"] == session.as_str(),
        "the grant must name {ship}'s first_mate.codex_session"
    );
    let old: Option<String> = tx
        .query_row(
            "SELECT session FROM codex_delivery_grants WHERE ship=?1",
            [ship],
            |r| r.get(0),
        )
        .optional()?;
    if old.as_deref().is_some_and(|old| old != session) {
        super::codex_custody::delivery_revoked(store, ship)?;
    }
    tx.execute(
        "DELETE FROM codex_delivery_reservations WHERE grant_id IN
           (SELECT grant_id FROM codex_delivery_grants WHERE ship=?1)",
        [ship],
    )?;
    let grant_id = uuid::Uuid::new_v4().to_string();
    tx.execute(
        "INSERT OR REPLACE INTO codex_delivery_grants(ship,grant_id,session,budget,spent,granted_ms)
         VALUES(?1,?2,?3,?4,0,?5)",
        params![ship, grant_id, session, budget, Store::now_ms()],
    )?;
    store.audit(
        "attended.codex.grant",
        &json!({"ship": ship, "session": session, "grant_id": grant_id, "budget": budget}),
    )?;
    tx.commit()?;
    status(store.connection(), ship)
}

/// Captain-only: end delivery. Poison is published before commit; a failed
/// poison aborts the revoke rather than report a disable that did not fence.
fn revoke(store: &Store, args: &Value) -> Result<Value> {
    store.caller().require_operator("attended.codex.revoke")?;
    let ship = ship(args)?;
    let tx = store.write_transaction()?;
    tx.execute(
        "DELETE FROM codex_delivery_reservations WHERE grant_id IN
           (SELECT grant_id FROM codex_delivery_grants WHERE ship=?1)",
        [ship],
    )?;
    let removed = tx.execute("DELETE FROM codex_delivery_grants WHERE ship=?1", [ship])?;
    super::codex_custody::delivery_revoked(store, ship)?;
    store.audit(
        "attended.codex.revoke",
        &json!({"ship": ship, "removed": removed}),
    )?;
    tx.commit()?;
    Ok(json!({"ship": ship, "revoked": removed == 1}))
}

/// The hosted holder reserves one possibly-unattended turn for one prepared
/// offer. Idempotent per nonce, so a lost response never spends twice.
fn reserve(store: &Store, args: &Value) -> Result<Value> {
    let ship = ship(args)?;
    match store.caller() {
        Caller::Agent { actor } if actor == ship => {}
        _ => bail!("attended.codex.reserve is the hosted first mate's own"),
    }
    let holder = args["holder"].as_str().context("holder is required")?;
    let session = canonical(args, "session")?;
    let grant_id = canonical(args, "grant_id")?;
    let nonce = canonical(args, "nonce")?;
    let tx = store.write_transaction()?;
    let lease = crate::presence::session(store, ship)?;
    ensure!(
        holder.starts_with(crate::presence::HOSTED)
            && lease.is_some_and(|l| l.holder == holder && !l.asleep),
        "attended.codex.reserve requires the current hosted presence lease"
    );
    let (current, granted, budget, spent, notified): (String, String, i64, i64, bool) = tx
        .query_row(
            "SELECT grant_id, session, budget, spent, low_notified FROM codex_delivery_grants WHERE ship=?1",
            [ship],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?
        .context("Codex delivery is not granted for this ship")?;
    ensure!(
        current == grant_id && granted == session,
        "Codex delivery grant changed"
    );
    let known: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM codex_delivery_reservations WHERE grant_id=?1 AND nonce=?2)",
        params![grant_id, nonce],
        |r| r.get(0),
    )?;
    let spent = if known {
        spent
    } else {
        ensure!(
            spent < budget,
            "Codex possibly-unattended turn budget exhausted; a new captain grant refills it"
        );
        tx.execute(
            "INSERT INTO codex_delivery_reservations(grant_id,nonce,reserved_ms) VALUES(?1,?2,?3)",
            params![grant_id, nonce, Store::now_ms()],
        )?;
        tx.execute(
            "UPDATE codex_delivery_grants SET spent=spent+1 WHERE ship=?1",
            [ship],
        )?;
        spent + 1
    };
    let remaining = budget - spent;
    if !notified && remaining <= (budget / 10).max(1) {
        if let Some(task) = super::duty_task(&tx, ship)? {
            crate::mail::record_crew(
                &tx,
                &json!({"id": format!("codex-turns-low-{grant_id}"), "to": "captain", "kind": "notice",
                    "task_id": task, "urgency": "normal",
                    "body": format!("{ship}'s Codex mail delivery has {remaining} of {budget} possibly-unattended turns left. Grant again (attended.codex.grant) to refill, or let it pause."),
                    "payload": {"codex_turns_low": ship, "grant_id": grant_id, "budget": budget, "remaining": remaining}}),
            )?;
        }
        tx.execute(
            "UPDATE codex_delivery_grants SET low_notified=1 WHERE ship=?1",
            [ship],
        )?;
    }
    tx.commit()?;
    Ok(
        json!({"reserved": true, "grant_id": grant_id, "session": session, "nonce": nonce,
        "budget": budget, "spent": spent, "remaining": remaining}),
    )
}

/// One bounded runner pass at a time, as the supervisor's own child, so the
/// runner can check the supervisor's pidfd. Output never reaches the pane.
pub(crate) struct Ticker {
    script: PathBuf,
    child: Option<(Child, Instant)>,
    last: Option<Instant>,
    spawn_failure_logged: bool,
    #[cfg(test)]
    stderr: Option<PathBuf>,
}

const EVERY: Duration = Duration::from_secs(5);
/// Above the runner's own bounds (3 s per control/RPC call, 3 s queue drain).
const CAP: Duration = Duration::from_secs(45);
const PYTHON: &str = "/usr/bin/python3";

impl Ticker {
    pub(crate) fn new(script: PathBuf) -> Self {
        Self {
            script,
            child: None,
            last: None,
            spawn_failure_logged: false,
            #[cfg(test)]
            stderr: None,
        }
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn capture_stderr(&mut self, path: PathBuf) {
        self.stderr = Some(path);
    }

    /// Reap a finished pass, or one past its cap. Either way its whole process
    /// group is killed first, while the unreaped leader still pins the group.
    pub(crate) fn reap(&mut self) {
        if let Some((child, started)) = self.child.as_mut() {
            if !exited(child) && started.elapsed() < CAP {
                return;
            }
            kill(child);
            self.child = None;
        }
    }

    pub(crate) fn poll(&mut self, dir: &Path) {
        self.reap();
        if self.child.is_some() || self.last.is_some_and(|l| l.elapsed() < EVERY) {
            return;
        }
        self.last = Some(Instant::now());
        let mut command = Command::new(PYTHON);
        command
            .args(["-I", "-B"])
            .arg(&self.script)
            .arg("tick")
            .arg(dir.join("launch.json"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        #[cfg(test)]
        if let Some(path) = &self.stderr {
            command.stderr(Stdio::from(
                std::fs::File::create(path).expect("create runner stderr capture"),
            ));
        } else {
            command.stderr(Stdio::null());
        }
        #[cfg(not(test))]
        command.stderr(Stdio::null());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        match command.spawn() {
            Ok(child) => self.child = Some((child, Instant::now())),
            Err(error) => {
                // A missing interpreter or script is retried each pass; report
                // it once per custody instead of every five seconds.
                if !self.spawn_failure_logged {
                    eprintln!("Codex mail runner unavailable: {error}");
                    self.spawn_failure_logged = true;
                }
            }
        }
    }

    /// Before the pane stops: let an in-flight pass finish within its cap
    /// (it owns its queue child's cleanup), then kill and reap it.
    pub(crate) fn finish(&mut self) {
        if let Some((child, started)) = self.child.as_mut() {
            while started.elapsed() < CAP && !exited(child) {
                std::thread::sleep(Duration::from_millis(50));
            }
            kill(child);
        }
        self.child = None;
    }
}

/// Whether the pass has exited, WITHOUT reaping it: a zombie leader keeps its
/// process group ID from naming any other group until `kill` reaps it.
fn exited(child: &Child) -> bool {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    let id = child.id() as libc::id_t;
    // An error (already reaped) counts as exited; no event leaves si_signo 0.
    unsafe { libc::waitid(libc::P_PID, id, &mut info, flags) != 0 || info.si_signo != 0 }
}

/// Kill the pass's whole process group, after a timeout and after a normal
/// exit alike (a member it left behind dies too), then reap the leader.
fn kill(child: &mut Child) {
    unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
    let _ = child.wait();
}

#[cfg(test)]
mod tests;
