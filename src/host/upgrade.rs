//! Hosted panes and the release each one runs. A pane's wrapper records its
//! own process and the harness it spawns, by start identity rather than bare
//! PID, so drift and exits are judged against the process that exists now. A
//! pane is behind when it runs an older release than `current`, or when its
//! binary was replaced on disk (Linux's ` (deleted)`).
//!
//! Relaunch moves a pane onto the current release at a safe point. Either an
//! explicit `privateer host relaunch` request or an idle pane catching up
//! makes the pane's own supervisor stop its harness, wait for that process to
//! exit, release the lease and end. The dispatcher, or the relaunch command,
//! then starts a fresh pane on the release `current` selects, continuing the
//! ship's conversation. Nothing here interrupts a turn that is running.
use crate::host::pi_sessions::PiSessions;
use crate::host::sleep::FRESH_MS;
use crate::presence;
use crate::releases::{self, Release};
use crate::store::{Caller, Store};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A request is honoured only this long after it was made, so one nobody took
/// cannot relaunch a pane hours later.
pub(crate) const REQUEST_TTL_MS: i64 = 10 * 60 * 1000;

/// How long `host relaunch` waits for its pane to reach a safe point.
const RELAUNCH_WAIT: Duration = Duration::from_secs(90);

/// Idle time before an automatic catch-up when the ship never sleeps.
const CATCH_UP_IDLE_SECS: u64 = 60;

const USAGE: &str = "usage: privateer host relaunch SHIP [--force]";

/// Whether a process running `image` is behind the selected release. Without a
/// selected release nothing is behind. An unversioned image is never behind:
/// relaunching it would only start the same binary again.
pub(crate) fn behind(image: &str, current: Option<&str>) -> bool {
    let Some(current) = current else {
        return false;
    };
    if image.ends_with(" (deleted)") {
        return true;
    }
    Release::from_executable(Path::new(image)).is_some_and(|release| release.commit != current)
}

/// The commit of the release an image belongs to, if it is a versioned one.
fn commit_of(image: &str) -> Option<String> {
    Release::from_executable(Path::new(image)).map(|release| release.commit)
}

/// The executable the kernel reports for a live process. Only Linux exposes
/// the running image; elsewhere the path recorded at launch stands in.
#[cfg(target_os = "linux")]
fn kernel_image(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

#[cfg(not(target_os = "linux"))]
fn kernel_image(_pid: u32) -> Option<String> {
    None
}

fn current_commit() -> Result<Option<String>> {
    Ok(releases::current()?.map(|release| release.commit))
}

/// The release selected for the install a pane's image belongs to. A replaced
/// binary keeps its path under Linux's ` (deleted)` suffix, which is dropped.
fn current_for(image: &str) -> Result<Option<String>> {
    let path = image.strip_suffix(" (deleted)").unwrap_or(image);
    Ok(releases::current_for(Path::new(path))?.map(|release| release.commit))
}

/// Whether this process itself runs behind the selected release.
fn running_behind() -> Result<bool> {
    let image = releases::running_image()?.to_string_lossy().into_owned();
    Ok(behind(&image, current_commit()?.as_deref()))
}

/// Record this pane, the process running `privateer start --hosted`.
pub(crate) fn record(store: &Store, ship: &str) -> Result<()> {
    let pid = std::process::id();
    let started = crate::jobs::process_identity(pid)?.context("pane process identity")?;
    let image = releases::running_image()?.to_string_lossy().into_owned();
    store.connection().execute(
        "INSERT INTO hosted_panes(ship,pid,started,executable,recorded_ms) VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(ship) DO UPDATE SET pid=excluded.pid, started=excluded.started,
           executable=excluded.executable, harness_pid=NULL, harness_started=NULL,
           recorded_ms=excluded.recorded_ms",
        params![ship, pid, started, image, Store::now_ms()],
    )?;
    Ok(())
}

/// The Claude settings a pane's session reads at launch: the operator's user
/// settings and the ship's own. Auto mode reads them only then.
fn settings_paths(store: &Store) -> Vec<PathBuf> {
    let mut paths = vec![];
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(home).join(".claude/settings.json"));
    }
    if let Some(folder) = store.home().parent() {
        paths.push(folder.join(".claude/settings.json"));
    }
    paths
}

/// A digest of each settings file's bytes. A missing file is part of it.
pub(crate) fn settings_digest(paths: &[PathBuf]) -> String {
    let mut hasher = Sha256::new();
    for path in paths {
        hasher.update(path.as_os_str().as_encoded_bytes());
        hasher.update([0]);
        match std::fs::read(path) {
            Ok(bytes) => {
                hasher.update([1]);
                hasher.update(&bytes);
            }
            Err(_) => hasher.update([0]),
        }
    }
    format!("{:x}", hasher.finalize())
}

/// Record the settings this pane's session will launch with. Called after the
/// launch writes its own settings, so that write is not drift.
pub(crate) fn record_settings(store: &Store, ship: &str) -> Result<()> {
    let digest = settings_digest(&settings_paths(store));
    store.connection().execute(
        "UPDATE hosted_panes SET settings_digest=?3 WHERE ship=?1 AND pid=?2",
        params![ship, std::process::id(), digest],
    )?;
    Ok(())
}

/// Record the captain approvals, and the launch policy, this pane's session
/// launches with: the approvals as told to its auto-mode classifier, and the
/// fleet policy inputs that classifier reads (this ship's standing
/// authorizations, the fleet's mode and tool rules), so a pane can tell when
/// its launch is stale. Also records whether this pane process may refresh
/// its session in place when they change. Auto mode reads them all only at
/// launch. Returns the approval ids this launch carries for the first time.
///
/// The refresh flag is keyed by this process's pid, so a pane an older binary
/// started, whose `record` rewrites the pid but not this column, never passes
/// for one that can refresh.
pub(crate) fn record_approvals(
    store: &Store,
    ship: &str,
    set: &[(String, String)],
    refreshable: bool,
) -> Result<Vec<String>> {
    let pid = std::process::id();
    let tx = store.write_transaction()?;
    let Some(before) = tx
        .query_row(
            "SELECT approvals_ids FROM hosted_panes WHERE ship=?1 AND pid=?2",
            params![ship, pid],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
    else {
        return Ok(vec![]);
    };
    let before: Vec<String> = before
        .and_then(|ids| serde_json::from_str(&ids).ok())
        .unwrap_or_default();
    let ids: Vec<String> = set.iter().map(|(id, _)| id.clone()).collect();
    let authorized = crate::permissions::authorizations_for(store, ship)?;
    tx.execute(
        "UPDATE hosted_panes SET approvals_digest=?3, approvals_ids=?4, approvals_refresh_pid=?5,
           authorizations_digest=?6, authorizations=?7
         WHERE ship=?1 AND pid=?2",
        params![
            ship,
            pid,
            crate::captain_approvals::set_digest(set),
            serde_json::to_string(&ids)?,
            refreshable.then_some(pid),
            crate::permissions::launch_digest(store, ship)?,
            serde_json::to_string(&authorized)?
        ],
    )?;
    tx.commit()?;
    Ok(ids.into_iter().filter(|id| !before.contains(id)).collect())
}

/// Tell the ship, once each, that an approval it was told would apply at a
/// restart now has: `first` are the ids a launch carried for the first time,
/// and only those recorded as applying at a relaunch are announced. Call
/// after the harness has spawned. A refresh, a wake or an upgrade relaunch
/// may each be the launch that applies one.
pub(crate) fn announce_applied(store: &Store, ship: &str, first: &[String]) -> Result<()> {
    if first.is_empty() {
        return Ok(());
    }
    let tx = store.write_transaction()?;
    for id in first {
        let row: Option<(String, String, String)> = tx
            .query_row(
                "SELECT task_id, request_id, action FROM captain_approvals
                 WHERE id=?1 AND ship=?2 AND applies_at='relaunch'",
                params![id, ship],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((task, request, action)) = row else {
            continue;
        };
        crate::mail::record_crew(
            &tx,
            &json!({
                "id": format!("approval-applied-{id}"),
                "to": ship,
                "kind": "notice",
                "task_id": task,
                "correlation": request,
                "urgency": "normal",
                "body": format!("Captain approval applied in your restarted session: retry {action} once."),
                "payload": {"approval_applied": id},
            }),
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Where a captain approval recorded now reaches the ship's session:
/// `relaunch` when a live, awake hosted pane will refresh its session in
/// place at its next idle point, on a conversation it can continue; `wake`
/// when nothing runs (a reply wakes the ship, and that launch reads
/// approvals) or the hosted pane sleeps; and
/// `next_launch` for a session that reads them only when it next starts.
pub(crate) fn approval_applies(store: &Store, ship: &str) -> Result<&'static str> {
    let lease = presence::presence(store, ship)?;
    if lease["attended"] != true {
        return Ok("wake");
    }
    let holder = lease["lease"]["holder"].as_str().unwrap_or_default();
    if !holder.starts_with(presence::HOSTED) {
        return Ok("next_launch");
    }
    let asleep: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM presence WHERE ship=?1 AND asleep_holder=holder)",
        [ship],
        |r| r.get(0),
    )?;
    if asleep {
        return Ok("wake");
    }
    let Some((pid, _, _)) = live_pane(store, ship)? else {
        return Ok("next_launch");
    };
    let refreshes: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM hosted_panes WHERE ship=?1 AND approvals_refresh_pid=?2)",
        params![ship, pid],
        |r| r.get(0),
    )?;
    // A refresh continues the folder's conversation; without one it waits.
    let continues = refreshes
        && store
            .home()
            .parent()
            .and_then(|folder| folder.canonicalize().ok())
            .is_some_and(|folder| crate::host::claude_conversation(&folder).continues());
    Ok(if continues { "relaunch" } else { "next_launch" })
}

/// Record the native harness this pane spawned, so its exit can be checked.
pub(crate) fn harness(store: &Store, ship: &str, harness: u32) -> Result<()> {
    let started = crate::jobs::process_identity(harness)?;
    store.connection().execute(
        "UPDATE hosted_panes SET harness_pid=?3, harness_started=?4 WHERE ship=?1 AND pid=?2",
        params![ship, std::process::id(), harness, started],
    )?;
    Ok(())
}

/// The model route, `harness/provider/model`, of the ship folder's session a
/// pane's continue resumes; None before any pane recorded one. A fresh launch
/// takes that place only once its harness has written its session: one that
/// never took a turn left nothing, so a continue would still find the session
/// before it, on its own route. `pi` is where a Pi pane's sessions are.
pub(crate) fn session_route(
    store: &Store,
    ship: &str,
    folder: &Path,
    pi: Option<&PiSessions>,
) -> Result<Option<String>> {
    let Some(launched) = launched(store, ship)? else {
        return Ok(None);
    };
    match (&launched.fresh, launched.fresh_ms) {
        (Some(fresh), Some(since)) if written(folder, fresh, since, &launched, pi) => {
            store.connection().execute(
                "UPDATE hosted_panes SET session_route=fresh_route, session_id=fresh_id,
                   fresh_route=NULL, fresh_ms=NULL, fresh_id=NULL
                 WHERE ship=?1 AND fresh_ms=?2",
                params![ship, since],
            )?;
            Ok(Some(fresh.clone()))
        }
        _ => Ok(launched.session),
    }
}

/// Whether the pane's fresh Claude session has yet to write anything, so it
/// has no conversation of its own to continue. Read only.
pub(crate) fn fresh_unwritten(store: &Store, ship: &str, folder: &Path) -> Result<bool> {
    Ok(
        launched(store, ship)?.is_some_and(|launched| match (&launched.fresh, launched.fresh_ms) {
            (Some(fresh), Some(since)) => !written(folder, fresh, since, &launched, None),
            _ => false,
        }),
    )
}

/// The Pi session a pane launching on `route` gives Pi to open, by id, and
/// whether that continues a session the pane's route ran: its fresh session
/// on that route, which may not be written yet; else the route's written
/// session; else, for a pane that never named one, the session `--continue`
/// would have resumed, except one an errand or wake of the ship recorded;
/// else a new one. Never another route's session, and never the folder's
/// newest just for being newest: an errand writes there too.
pub(crate) fn pi_session(
    store: &Store,
    ship: &str,
    folder: &Path,
    route: Option<&str>,
    pi: &PiSessions,
) -> Result<(String, bool)> {
    session_route(store, ship, folder, Some(pi))?;
    let launched = launched(store, ship)?.unwrap_or_default();
    let on = |recorded: &Option<String>| recorded.as_deref() == route;
    if let Some(id) = launched.fresh_id.clone().filter(|_| on(&launched.fresh)) {
        return Ok((id, false));
    }
    if launched.session.is_none() || on(&launched.session) {
        if let Some(id) = launched.session_id {
            return Ok((id, true));
        }
        if launched.fresh_id.is_none()
            && let Some(id) = pi.newest(&headless_sessions(store, ship))
        {
            return Ok((id, true));
        }
    }
    Ok((super::pi_sessions::new_id(), false))
}

/// The sessions the ship's errands and wakes ran, which are not a pane's.
fn headless_sessions(store: &Store, ship: &str) -> Vec<String> {
    let ids = || -> rusqlite::Result<Vec<String>> {
        let mut statement = store.connection().prepare(
            "SELECT session_id FROM sessions WHERE ship=?1
             UNION SELECT session_id FROM errands WHERE ship=?1 AND session_id IS NOT NULL",
        )?;
        statement
            .query_map([ship], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()
    };
    ids().unwrap_or_default()
}

/// The recorded session route and Pi session id, and a fresh launch's route,
/// spawn time and Pi session id.
#[derive(Default)]
struct Launched {
    session: Option<String>,
    session_id: Option<String>,
    fresh: Option<String>,
    fresh_ms: Option<i64>,
    fresh_id: Option<String>,
}

fn launched(store: &Store, ship: &str) -> Result<Option<Launched>> {
    Ok(store
        .connection()
        .query_row(
            "SELECT session_route, session_id, fresh_route, fresh_ms, fresh_id
             FROM hosted_panes WHERE ship=?1",
            [ship],
            |r| {
                Ok(Launched {
                    session: r.get(0)?,
                    session_id: r.get(1)?,
                    fresh: r.get(2)?,
                    fresh_ms: r.get(3)?,
                    fresh_id: r.get(4)?,
                })
            },
        )
        .optional()?)
}

/// Whether the fresh launch on `route` has written its session: Claude's
/// interactive transcript after the spawn, or the Pi session with the id the
/// pane gave it (any session after the spawn, for a launch recorded before
/// panes named theirs).
fn written(
    folder: &Path,
    route: &str,
    since_ms: i64,
    launched: &Launched,
    pi: Option<&PiSessions>,
) -> bool {
    match route.split('/').next().unwrap_or_default() {
        "claude" => crate::host::claude_session_since(folder, since_ms),
        "pi" | "pi-signed" => pi.is_some_and(|pi| match &launched.fresh_id {
            Some(id) => pi.has(id),
            None => pi.written_since(since_ms),
        }),
        _ => false,
    }
}

/// Record the model route this pane launched its harness on, spawned at
/// `spawned_ms`, and the Pi session id it gave: the session's own when it
/// continued it, otherwise a fresh one, which stays apart until it writes a
/// session (`session_route`).
pub(crate) fn record_launch(
    store: &Store,
    ship: &str,
    route: Option<&str>,
    continued: bool,
    spawned_ms: i64,
    session_id: Option<&str>,
) -> Result<()> {
    let pid = std::process::id();
    if continued {
        store.connection().execute(
            "UPDATE hosted_panes SET session_route=?3, session_id=?4,
               fresh_route=NULL, fresh_ms=NULL, fresh_id=NULL
             WHERE ship=?1 AND pid=?2",
            params![ship, pid, route, session_id],
        )?;
    } else {
        store.connection().execute(
            "UPDATE hosted_panes SET fresh_route=?3, fresh_ms=?4, fresh_id=?5
             WHERE ship=?1 AND pid=?2",
            params![ship, pid, route, spawned_ms, session_id],
        )?;
    }
    Ok(())
}

/// The recorded pane for a ship, if its process is still the one recorded.
pub(crate) fn live_pane(store: &Store, ship: &str) -> Result<Option<(u32, String, String)>> {
    let row: Option<(u32, String, String)> = store
        .connection()
        .query_row(
            "SELECT pid, started, executable FROM hosted_panes WHERE ship=?1",
            [ship],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(row.filter(|(pid, started, _)| {
        crate::jobs::process_identity(*pid)
            .ok()
            .flatten()
            .as_deref()
            == Some(started.as_str())
    }))
}

/// A pane's release and whether it is behind, read from the live process.
pub(crate) fn view(store: &Store, ship: &str) -> Result<Option<Value>> {
    let Some((pid, _, recorded)) = live_pane(store, ship)? else {
        return Ok(None);
    };
    let image = kernel_image(pid).unwrap_or(recorded);
    let current = current_for(&image)?;
    // A pane recorded before settings, approvals or authorizations were
    // tracked has no digest to judge.
    let (launched, approvals, authorizations, refreshed): (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
    ) = store
        .connection()
        .query_row(
            "SELECT settings_digest, approvals_digest, authorizations_digest,
               approvals_refreshed_ms FROM hosted_panes
             WHERE ship=?1",
            [ship],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?
        .unwrap_or_default();
    let settings_changed =
        launched.is_some_and(|launched| launched != settings_digest(&settings_paths(store)));
    // An approval recorded, revoked, expired or ended with its task since
    // launch, or the launch policy the session's auto-mode check reads
    // (its own standing authorizations, the fleet's mode and tool rules)
    // changed. A pane that can refresh applies it at its next safe point
    // (host::refresh); any other pane applies it at its next launch.
    let approvals_changed = match approvals {
        Some(approvals) => {
            approvals != crate::captain_approvals::current_digest(store, ship, Store::now_ms())?
        }
        None => false,
    };
    let authorizations_changed = match authorizations {
        Some(authorizations) => authorizations != crate::permissions::launch_digest(store, ship)?,
        None => false,
    };
    Ok(Some(json!({
        "pid": pid,
        "executable": image,
        "commit": commit_of(&image),
        "current_commit": current,
        "drift": behind(&image, current.as_deref()),
        "settings_changed": settings_changed,
        "approvals_changed": approvals_changed,
        "authorizations_changed": authorizations_changed,
        "approvals_refreshed_ms": refreshed,
    })))
}

/// Every live hosted pane in this home, for `upgrade status`.
pub fn panes(store: &Store) -> Result<Value> {
    let mut statement = store
        .connection()
        .prepare("SELECT ship FROM hosted_panes ORDER BY ship")?;
    let ships = statement
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = vec![];
    for ship in ships {
        if let Some(mut pane) = view(store, &ship)? {
            pane["ship"] = json!(ship);
            out.push(pane);
        }
    }
    Ok(json!(out))
}

/// Where a ship's live hosted pane stands after an upgrade moved `current`,
/// for its summary; None without one. Nothing is asked of the pane and its
/// lease is not touched: a pane behind `current` catches up on its own at
/// its next safe point (asleep at once, awake once idle for its threshold),
/// so `pending` is an idle pane that will relaunch there and `catching_up` a
/// busy one that will once its turn ends. `current` is a pane on the
/// selected release, and `unversioned` one that is never behind (relaunching
/// it would start the same binary again).
pub(crate) fn pane_outcome(store: &Store, ship: &str) -> Result<Option<&'static str>> {
    let Some(pane) = view(store, ship)? else {
        return Ok(None);
    };
    Ok(Some(if pane["drift"] == true {
        if busy(store, ship)? {
            "catching_up"
        } else {
            "pending"
        }
    } else if pane["commit"].is_null() {
        "unversioned"
    } else {
        "current"
    }))
}

/// `pane_outcome` for every live hosted pane in this home, by ship.
pub(crate) fn pane_outcomes(store: &Store) -> Result<Vec<(String, &'static str)>> {
    let mut statement = store
        .connection()
        .prepare("SELECT ship FROM hosted_panes ORDER BY ship")?;
    let ships = statement
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = vec![];
    for ship in ships {
        if let Some(outcome) = pane_outcome(store, &ship)? {
            out.push((ship, outcome));
        }
    }
    Ok(out)
}

/// The pending relaunch for this holder, with its force flag, if still fresh.
fn requested(store: &Store, ship: &str, holder: &str) -> Result<Option<bool>> {
    Ok(store
        .connection()
        .query_row(
            "SELECT force FROM hosted_relaunches WHERE ship=?1 AND holder=?2 AND expires_ms>?3",
            params![ship, holder, Store::now_ms()],
            |r| r.get::<_, bool>(0),
        )
        .optional()?)
}

/// Whether a fresh report says a turn is running. Silence is not busy.
pub(crate) fn busy(store: &Store, ship: &str) -> Result<bool> {
    Ok(store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM work_sessions WHERE ship=?1 AND idle=0 AND checked_ms>?2)",
        params![ship, Store::now_ms() - FRESH_MS],
        |r| r.get(0),
    )?)
}

/// What a running pane's supervisor should do now, if anything: relaunch on a
/// request (never mid-turn unless forced), or catch up when it is behind and
/// idle for the sleep threshold. Idleness is required for both.
pub(crate) fn wanted(
    store: &Store,
    ship: &str,
    holder: &str,
    sleep_after: u64,
) -> Result<Option<&'static str>> {
    if let Some(force) = requested(store, ship, holder)? {
        if (force || !busy(store, ship)?) && !crate::permissions::paused(store, ship)? {
            return Ok(Some("relaunch"));
        }
    }
    let threshold = if sleep_after > 0 {
        sleep_after
    } else {
        CATCH_UP_IDLE_SECS
    };
    // A pending tool-permission question blocks the replacement launch, so the
    // pane stays up until the captain answers.
    if !crate::permissions::paused(store, ship)?
        && running_behind()?
        && crate::host::sleep::idle_for(store, ship, holder, threshold)?
    {
        return Ok(Some("upgrade"));
    }
    Ok(None)
}

/// Take the ship's lease from a running harness before it is stopped, in one
/// transaction that rechecks the reason. The adapter checks ownership before
/// each delivery, so once this commits it starts no turn, and stopping the
/// harness ends nothing a turn was using. None if nothing is due any more. The
/// mail sequence seen at the fence comes back too: a pane that starts asleep
/// from it wakes on any mail after it.
pub(crate) fn fence(
    store: &Store,
    ship: &str,
    holder: &str,
    sleep_after: u64,
) -> Result<Option<(&'static str, String, i64)>> {
    let tx = store.write_transaction()?;
    let Some(reason) = wanted(store, ship, holder, sleep_after)? else {
        return Ok(None);
    };
    let fenced = format!("{}{}", presence::HOSTED, uuid::Uuid::new_v4());
    if !presence::hand_over(store, ship, holder, &fenced)? {
        return Ok(None);
    }
    let through: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq),0) FROM mail WHERE recipient=?1",
        [ship],
        |r| r.get(0),
    )?;
    tx.execute(
        "UPDATE hosted_relaunches SET holder=?3 WHERE ship=?1 AND holder=?2",
        params![ship, holder, fenced],
    )?;
    tx.execute(
        "UPDATE host_launches SET holder=?3 WHERE ship=?1 AND holder=?2",
        params![ship, holder, fenced],
    )?;
    tx.commit()?;
    Ok(Some((reason, fenced, through)))
}

/// A sleeping pane has no turn to lose, so a request or any drift ends it.
pub(crate) fn wanted_asleep(
    store: &Store,
    ship: &str,
    holder: &str,
) -> Result<Option<&'static str>> {
    if requested(store, ship, holder)?.is_some() {
        return Ok(Some("relaunch"));
    }
    let upgrade = !crate::permissions::paused(store, ship)? && running_behind()?;
    Ok(upgrade.then_some("upgrade"))
}

/// Where the pane that replaces a retired one starts. Only a catch-up on a ship
/// that sleeps starts asleep, from the mail the retired pane had seen; a ship
/// that never sleeps starts awake, and so does a requested relaunch.
pub(crate) fn start_point(reason: &str, sleep_after: u64, through: i64) -> Option<i64> {
    (reason == "upgrade" && sleep_after > 0).then_some(through)
}

/// The pane is leaving on purpose: release its lease, spend its request, and
/// drop the launch backoff its launch reserved, since a planned exit is not a
/// failed launch. Call only after the harness has exited, so the lease never
/// outlives it. The lease goes first: a relaunch waiting on the request reads
/// the request's disappearance as the pane having left, and must not launch
/// while the lease is still held. `start` carries to the next pane: `Some` of
/// the mail sequence it has seen starts that pane asleep, and `None` starts it
/// awake.
pub(crate) fn retire(store: &Store, ship: &str, holder: &str, reason: &str, start: Option<i64>) {
    let start_asleep = start.is_some();
    let _ = store.connection().execute(
        "UPDATE host_launches SET retry_ms=0, start_asleep=?3, start_through=?4 WHERE ship=?1 AND holder=?2",
        params![ship, holder, start_asleep, start],
    );
    let _ = presence::leave(store, ship, holder);
    let _ = store.connection().execute(
        "DELETE FROM hosted_relaunches WHERE ship=?1 AND holder=?2",
        params![ship, holder],
    );
    let _ = store.audit(
        "host.relaunch",
        &json!({"ship": ship, "reason": reason, "start_asleep": start_asleep}),
    );
}

/// `privateer host relaunch SHIP [--force]`: the operator's, or this
/// machine's owner's. Refuses a running turn unless forced.
pub fn cli(home: Option<PathBuf>, positional: &[String], caller: &Caller) -> Result<()> {
    match caller {
        Caller::Operator => {}
        Caller::Agent { actor } => ensure!(
            crate::machines::is_owner(actor),
            "host relaunch is the operator's, or this machine's owner's; agents cannot run it"
        ),
    }
    let (verb, ship, flags) = match positional {
        [verb, ship, flags @ ..] => (verb.as_str(), ship.as_str(), flags),
        _ => bail!("{USAGE}"),
    };
    let force = match flags {
        [] => false,
        [flag] if flag == "--force" => true,
        _ => bail!("{USAGE}"),
    };
    ensure!(verb == "relaunch", "{USAGE}");
    let home = home.context(
        "not inside a ship folder (no .privateer here or above); run `privateer init`, or pass --home",
    )?;
    let mut store = Store::open_as(&home, caller.clone())?;
    let report = relaunch(&mut store, ship, force)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// Ask the hosted pane to reach its safe point, wait for it to release the
/// lease, then launch a fresh pane on the current release.
pub(crate) fn relaunch(store: &mut Store, ship: &str, force: bool) -> Result<Value> {
    let profile = crate::dispatch(store, "ship.show", json!({"name": ship}))?["first_mate"].clone();
    ensure!(
        crate::host::hosted(&profile),
        "{ship}'s first mate is not hosted"
    );
    ensure!(
        !crate::permissions::paused(store, ship)?,
        "tool permission question awaits captain or a policy change"
    );
    // A pane is never stopped for a relaunch that has no route to take.
    if let crate::crew_routes::Listed::Unavailable(why) =
        crate::crew_routes::listed_first_mate(store, ship, &profile)?
    {
        bail!("{why}; {ship}'s pane keeps running until a route clears");
    }
    let before = view(store, ship)?;
    let prior_commit = before
        .as_ref()
        .and_then(|pane| pane["commit"].as_str().map(str::to_owned));
    // The pane this relaunch asks to stop, read before any request goes out: a
    // new pane recorded during the wait must not stand in for it.
    let prior_pane = pane_record(store, ship)?;
    let lease = presence::presence(store, ship)?;
    // A pane is running only under a hosted holder; anything else is an
    // ordinary launch, with nothing to relaunch.
    let holder = (lease["attended"] == true)
        .then(|| lease["lease"]["holder"].as_str())
        .flatten()
        .filter(|h| h.starts_with(presence::HOSTED))
        .map(str::to_owned);
    if let Some(holder) = holder {
        ensure!(
            recorded(store, ship)?,
            "{ship}'s first mate predates relaunch support: its pane never recorded its release, so it cannot be moved. Close its Herdr pane; the dispatcher starts it again on the current release once the old lease lapses"
        );
        ensure!(
            force || !busy(store, ship)?,
            "{ship}'s first mate is in a turn; wait for it to go idle, or pass --force"
        );
        let tx = store.write_transaction()?;
        tx.execute(
            "INSERT INTO hosted_relaunches(ship,holder,force,requested_ms,expires_ms) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(ship) DO UPDATE SET holder=excluded.holder, force=excluded.force,
               requested_ms=excluded.requested_ms, expires_ms=excluded.expires_ms",
            params![ship, holder, force, Store::now_ms(), Store::now_ms() + REQUEST_TTL_MS],
        )?;
        // The operator's own act does not wait out a failed launch's backoff.
        tx.execute("UPDATE host_launches SET retry_ms=0 WHERE ship=?1", [ship])?;
        tx.commit()?;
        let deadline = Instant::now() + RELAUNCH_WAIT;
        loop {
            // The pane has left once its request is spent, or the lease it
            // holds is no longer held under the request's holder: honoured,
            // exited on its own, or already replaced by a new launch.
            let requested_by: Option<String> = store
                .connection()
                .query_row(
                    "SELECT holder FROM hosted_relaunches WHERE ship=?1",
                    [ship],
                    |r| r.get(0),
                )
                .optional()?;
            let lease = presence::presence(store, ship)?;
            let still_held = requested_by.as_deref().is_some_and(|by| {
                lease["attended"] == true && lease["lease"]["holder"].as_str() == Some(by)
            });
            if !still_held {
                break;
            }
            if Instant::now() >= deadline {
                store
                    .connection()
                    .execute("DELETE FROM hosted_relaunches WHERE ship=?1", [ship])?;
                bail!(
                    "{ship}'s first mate did not reach a safe point within {} seconds; its request is withdrawn and the pane keeps running",
                    RELAUNCH_WAIT.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        // Whatever ended the pane, its request is spent now.
        store
            .connection()
            .execute("DELETE FROM hosted_relaunches WHERE ship=?1", [ship])?;
    }
    // Checked after the pane has let go, so its own harness has had its chance to stop.
    let harness_exited = !harness_running(prior_pane)?;
    // Never two harnesses on one conversation: a pane that left while its
    // harness still runs (its lease lapsed or moved) keeps that harness.
    ensure!(
        harness_exited,
        "{ship}'s harness is still running after its pane let go of the ship; refusing to launch a second one on this conversation. Stop the old harness, then retry"
    );
    // A profile that names no model relaunches on the first_mate list's
    // route, chosen now as any launch chooses it; with none clear the first
    // mate waits, as its dispatcher pass does.
    let mut profile = profile;
    let route = match crate::crew_routes::listed_first_mate(store, ship, &profile)? {
        crate::crew_routes::Listed::Unlisted => None,
        crate::crew_routes::Listed::Route(choice) => {
            crate::crew_routes::onto(&mut profile, &choice);
            Some(choice.to_route_json())
        }
        crate::crew_routes::Listed::Unavailable(why) => bail!(
            "{ship}'s pane has stopped, but its first mate cannot relaunch: {why}. It waits for a route to clear"
        ),
    };
    let launch = match crate::host::launch(store, ship, &profile)? {
        crate::host::Attempt::Launched(pane) => json!({"launched": pane}),
        crate::host::Attempt::Backoff(until) => json!({"backoff_until_ms": until}),
        crate::host::Attempt::Busy => json!({"attended": true}),
    };
    Ok(json!({
        "ship": ship,
        "from_commit": prior_commit,
        "to_commit": current_commit()?,
        "harness_exited": harness_exited,
        "launch": launch,
        "route": route,
    }))
}

/// Whether the live pane recorded its release, which a relaunch needs. A row
/// for an earlier process does not count.
fn recorded(store: &Store, ship: &str) -> Result<bool> {
    Ok(live_pane(store, ship)?.is_some())
}

/// A pane's `hosted_panes` row: its pid and start identity, then the harness
/// it recorded, if any.
type PaneRecord = (u32, String, Option<u32>, Option<String>);

fn pane_record(store: &Store, ship: &str) -> Result<Option<PaneRecord>> {
    Ok(store
        .connection()
        .query_row(
            "SELECT pid, started, harness_pid, harness_started FROM hosted_panes WHERE ship=?1",
            [ship],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?)
}

/// Whether a process the ship's pane started still runs, for the pane recorded
/// in `record`. The recorded harness is checked by identity, and the pane's
/// process tree is walked from the pane, because its wrapper may have spawned
/// the native process rather than exec'd into it. A pane that is itself gone
/// has no tree to walk, and its orphans reparent away from it, so only the
/// recorded harness can name them.
fn harness_running(record: Option<PaneRecord>) -> Result<bool> {
    let Some((pane, pane_started, harness, harness_started)) = record else {
        return Ok(false);
    };
    if let (Some(pid), Some(started)) = (harness, harness_started)
        && crate::jobs::process_identity(pid)?.as_deref() == Some(started.as_str())
    {
        return Ok(true);
    }
    if crate::jobs::process_identity(pane)?.as_deref() != Some(pane_started.as_str()) {
        return Ok(false);
    }
    tree_alive(pane)
}

/// Whether any process under `root` is alive, walking the kernel's parent links
/// breadth first. Bounded, so a runaway tree is refused rather than walked on.
fn tree_alive(root: u32) -> Result<bool> {
    const LIMIT: usize = 4_096;
    let mut pending = vec![root];
    for _ in 0..LIMIT {
        let Some(parent) = pending.pop() else {
            return Ok(false);
        };
        for child in crate::jobs::child_pids(parent)? {
            if crate::jobs::process_identity(child)?.is_some() {
                return Ok(true);
            }
            pending.push(child);
        }
    }
    bail!("process tree under pid {root} exceeds {LIMIT} processes")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A relaunch waiting on its request reads the request's disappearance as
    /// the pane having left. The lease must already be released by then, or
    /// the relaunch launches into Busy and exits with no pane.
    #[test]
    fn retire_releases_the_lease_before_its_request_is_spent() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let (ship, holder) = ("infra", "host:pane");
        store
            .connection()
            .execute(
                "INSERT INTO ships(name, record) VALUES(?1, '{\"location\":\"here\"}')",
                [ship],
            )
            .unwrap();
        crate::presence::attend(&store, ship, holder, 90, None).unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO hosted_relaunches(ship,holder,force,requested_ms,expires_ms) VALUES(?1,?2,0,0,?3)",
                params![ship, holder, Store::now_ms() + REQUEST_TTL_MS],
            )
            .unwrap();
        store
            .connection()
            .execute_batch(
                "CREATE TABLE probe(lease_held INTEGER NOT NULL);
                 CREATE TRIGGER probe_request_spent AFTER DELETE ON hosted_relaunches
                 BEGIN
                   INSERT INTO probe SELECT EXISTS(SELECT 1 FROM presence WHERE ship=OLD.ship AND holder=OLD.holder);
                 END;",
            )
            .unwrap();

        retire(&store, ship, holder, "relaunch", None);

        let held: Vec<i64> = store
            .connection()
            .prepare("SELECT lease_held FROM probe")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(held, vec![0]);
        assert!(!crate::presence::attended(&store, ship).unwrap());
    }

    /// A harness the pane spawned, rather than exec'd into, is still running
    /// under the pane even though no harness pid was recorded for it.
    #[test]
    fn a_child_still_running_under_the_pane_counts_as_its_harness() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let ship = "infra";
        // A stand-in pane whose child, the stand-in harness, outlives the shell.
        let mut pane = std::process::Command::new("sh")
            .args(["-c", "sleep 2; true"])
            .spawn()
            .unwrap();
        let started = crate::jobs::process_identity(pane.id()).unwrap().unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO hosted_panes(ship,pid,started,executable,recorded_ms) VALUES(?1,?2,?3,'x',0)",
                params![ship, pane.id(), started],
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while crate::jobs::child_pids(pane.id()).unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(harness_running(pane_record(&store, ship).unwrap()).unwrap());
        // Once the pane is gone, nothing under it is found.
        pane.wait().unwrap();
        assert!(!harness_running(pane_record(&store, ship).unwrap()).unwrap());
    }

    /// A relaunch judges the pane it asked to stop, even when a new pane has
    /// recorded its row during the wait: the old pane's harness still runs, and
    /// the new pane's row names no harness of its own.
    #[test]
    fn a_relaunch_judges_the_pane_it_asked_to_stop_not_its_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let ship = "infra";
        let mut old_pane = std::process::Command::new("sh")
            .args(["-c", "sleep 3; true"])
            .spawn()
            .unwrap();
        let old_started = crate::jobs::process_identity(old_pane.id())
            .unwrap()
            .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO hosted_panes(ship,pid,started,executable,recorded_ms) VALUES(?1,?2,?3,'x',0)",
                params![ship, old_pane.id(), old_started],
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while crate::jobs::child_pids(old_pane.id()).unwrap().is_empty()
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let prior = pane_record(&store, ship).unwrap();
        // The replacement pane records itself over the old row, as record() does.
        let mut new_pane = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let new_started = crate::jobs::process_identity(new_pane.id())
            .unwrap()
            .unwrap();
        store
            .connection()
            .execute(
                "UPDATE hosted_panes SET pid=?2, started=?3, harness_pid=NULL, harness_started=NULL WHERE ship=?1",
                params![ship, new_pane.id(), new_started],
            )
            .unwrap();
        assert!(harness_running(prior).unwrap());
        assert!(!harness_running(pane_record(&store, ship).unwrap()).unwrap());
        new_pane.kill().unwrap();
        new_pane.wait().unwrap();
        old_pane.wait().unwrap();
    }

    /// After an upgrade moved `current`, an idle pane behind it is pending
    /// its relaunch at its next safe point and a busy one is catching up;
    /// neither is asked anything and no lease moves. A pane already on the
    /// selected release is current.
    #[test]
    fn an_upgrade_reports_idle_panes_pending_and_busy_ones_catching_up() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let store = Store::open(&root.join("home")).unwrap();
        let data = root.join("data/privateer");
        let (old, new) = ("a".repeat(40), "b".repeat(40));
        // Panes are stand-in processes running from a release's bin.
        let mut children = vec![];
        for (ship, commit) in [("infra", &old), ("apps", &old), ("docs", &new)] {
            let bin = data.join("releases").join(commit).join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let image = bin.join("privateer");
            if !image.exists() {
                std::fs::copy("/bin/sleep", &image).unwrap();
            }
            let child = std::process::Command::new(&image)
                .arg("60")
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while kernel_image(child.id()).is_some_and(|i| !i.ends_with("/bin/privateer"))
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            let started = crate::jobs::process_identity(child.id()).unwrap().unwrap();
            store
                .connection()
                .execute(
                    "INSERT INTO hosted_panes(ship,pid,started,executable,recorded_ms) VALUES(?1,?2,?3,?4,0)",
                    params![ship, child.id(), started, image.to_string_lossy()],
                )
                .unwrap();
            children.push(child);
        }
        std::os::unix::fs::symlink(Path::new("releases").join(&new), data.join("current")).unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO work_sessions(ship,holder,idle,checked_ms) VALUES('apps','host:apps',0,?1)",
                [Store::now_ms()],
            )
            .unwrap();
        let outcomes = pane_outcomes(&store).unwrap();
        for mut child in children {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert_eq!(
            outcomes,
            [
                ("apps".to_owned(), "catching_up"),
                ("docs".to_owned(), "current"),
                ("infra".to_owned(), "pending"),
            ]
        );
        let requests: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM hosted_relaunches", [], |r| r.get(0))
            .unwrap();
        assert_eq!(requests, 0);
    }

    /// A catch-up on a ship that never sleeps starts awake: only a ship that
    /// sleeps is left asleep by an upgrade, and a request always wakes it.
    #[test]
    fn a_ship_that_never_sleeps_is_not_left_asleep_by_an_upgrade() {
        assert_eq!(start_point("upgrade", 0, 7), None);
        assert_eq!(start_point("upgrade", 1800, 7), Some(7));
        assert_eq!(start_point("relaunch", 1800, 7), None);
        assert_eq!(start_point("relaunch", 0, 7), None);
    }
}

#[cfg(test)]
mod settings_tests {
    use super::settings_digest;

    /// A settings edit changes the digest; the same bytes, or a file that
    /// stays missing, do not. Auto mode reads the file only at launch.
    #[test]
    fn the_digest_moves_only_when_a_settings_file_changes() {
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user.json");
        let ship = temp.path().join("ship.json");
        let paths = [user.clone(), ship.clone()];
        let missing = settings_digest(&paths);
        std::fs::write(&user, r#"{"autoMode":{"allow":[]}}"#).unwrap();
        let written = settings_digest(&paths);
        assert_ne!(missing, written);
        assert_eq!(written, settings_digest(&paths));
        std::fs::write(&user, r#"{"autoMode":{"allow":["Bash(git *)"]}}"#).unwrap();
        assert_ne!(written, settings_digest(&paths));
    }
}
