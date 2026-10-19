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
use crate::host::sleep::FRESH_MS;
use crate::presence;
use crate::releases::{self, Release};
use crate::store::{Caller, Store};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
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

/// Record the native harness this pane spawned, so its exit can be checked.
pub(crate) fn harness(store: &Store, ship: &str, harness: u32) -> Result<()> {
    let started = crate::jobs::process_identity(harness)?;
    store.connection().execute(
        "UPDATE hosted_panes SET harness_pid=?3, harness_started=?4 WHERE ship=?1 AND pid=?2",
        params![ship, std::process::id(), harness, started],
    )?;
    Ok(())
}

/// The recorded pane for a ship, if its process is still the one recorded.
fn live_pane(store: &Store, ship: &str) -> Result<Option<(u32, String, String)>> {
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
    Ok(Some(json!({
        "pid": pid,
        "executable": image,
        "commit": commit_of(&image),
        "current_commit": current,
        "drift": behind(&image, current.as_deref()),
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
    let moved = tx.execute(
        "UPDATE presence SET holder=?3 WHERE ship=?1 AND holder=?2",
        params![ship, holder, fenced],
    )?;
    if moved != 1 {
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
    let before = view(store, ship)?;
    let prior_commit = before
        .as_ref()
        .and_then(|pane| pane["commit"].as_str().map(str::to_owned));
    let prior_harness = harness_of(store, ship)?;
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
    let harness_exited = prior_harness.is_none_or(|(pid, started)| {
        crate::jobs::process_identity(pid).ok().flatten() != Some(started)
    });
    // Never two harnesses on one conversation: a pane that left while its
    // harness still runs (its lease lapsed or moved) keeps that harness.
    ensure!(
        harness_exited,
        "{ship}'s harness is still running after its pane let go of the ship; refusing to launch a second one on this conversation. Stop the old harness, then retry"
    );
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
    }))
}

/// Whether the live pane recorded its release, which a relaunch needs. A row
/// for an earlier process does not count.
fn recorded(store: &Store, ship: &str) -> Result<bool> {
    Ok(live_pane(store, ship)?.is_some())
}

/// The native harness a live pane spawned, with its start identity.
fn harness_of(store: &Store, ship: &str) -> Result<Option<(u32, String)>> {
    Ok(store
        .connection()
        .query_row(
            "SELECT harness_pid, harness_started FROM hosted_panes WHERE ship=?1",
            [ship],
            |r| Ok((r.get::<_, Option<u32>>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()?
        .and_then(|(pid, started)| Some((pid?, started?))))
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
