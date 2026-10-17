//! Sleeping hosted first mates. A hosted session whose mail adapter has
//! reported it idle for `first_mate.sleep_after_secs` (default 30 minutes; 0
//! keeps it running) has its native harness stopped to free memory, while
//! `start --hosted` keeps the pane, the ship's presence lease and its place in
//! Herdr, where the pane stays listed as the ship's agent, asleep. New
//! actionable mail, ready scheduled work, Enter in the pane or the captain
//! moving to it in Herdr starts the harness again on the same conversation.
//! The dispatcher sees an attended, hosted ship throughout, so it neither
//! opens another space nor starts a headless wake.
use crate::store::Store;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

/// Idle reports arrive every five seconds; an older one says nothing about now.
const FRESH_MS: i64 = 15_000;

/// Past the prompt cache's life and quiet compaction, so a first mate the
/// captain is talking to never sleeps, and waking it costs little extra.
pub(crate) const DEFAULT_AFTER_SECS: u64 = 1_800;

/// A stopped harness, held under a new holder so its mail adapter, if it is
/// somehow still running, no longer owns the ship.
pub(crate) struct Asleep {
    pub holder: String,
    /// Mail through this sequence was there before sleeping and wakes nothing.
    pub through_seq: i64,
}

/// Whether this session's adapter has reported it idle, without a break, for
/// `after_secs`, with nothing ready for it. Read only; `sleep` decides.
pub(crate) fn due(store: &Store, ship: &str, holder: &str, after_secs: u64) -> Result<bool> {
    if after_secs == 0 {
        return Ok(false);
    }
    let now = Store::now_ms();
    let idle_since: Option<i64> = store
        .connection()
        .query_row(
            "SELECT idle_since_ms FROM work_sessions
             WHERE ship=?1 AND holder=?2 AND idle=1 AND checked_ms>?3",
            params![ship, holder, now - FRESH_MS],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(
        idle_since.is_some_and(|since| now - since >= (after_secs as i64).saturating_mul(1000))
            && !crate::scheduling::has_ready(store, ship)?,
    )
}

/// Take the ship's lease for a sleeping session, in one transaction with the
/// last check, before the harness is stopped. The adapter checks ownership
/// before each delivery, so once this commits it starts no new turn.
pub(crate) fn sleep(
    store: &Store,
    ship: &str,
    holder: &str,
    after_secs: u64,
) -> Result<Option<Asleep>> {
    let tx = store.write_transaction()?;
    let presence = crate::presence::presence(store, ship)?;
    if presence["attended"] != true
        || presence["lease"]["holder"] != holder
        || crate::permissions::paused(store, ship)?
        || !due(store, ship, holder, after_secs)?
    {
        return Ok(None);
    }
    let asleep = format!("{}{}", crate::presence::HOSTED, uuid::Uuid::new_v4());
    tx.execute(
        "UPDATE presence SET holder=?3 WHERE ship=?1 AND holder=?2",
        params![ship, holder, asleep],
    )?;
    // Launch backoff follows the session from one holder to the next.
    tx.execute(
        "UPDATE host_launches SET holder=?3 WHERE ship=?1 AND holder=?2",
        params![ship, holder, asleep],
    )?;
    tx.execute("DELETE FROM work_sessions WHERE ship=?1", [ship])?;
    let through_seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq),0) FROM mail WHERE recipient=?1",
        [ship],
        |r| r.get(0),
    )?;
    store.audit(
        "host.slept",
        &json!({"ship": ship, "idle_secs": after_secs, "through_seq": through_seq}),
    )?;
    tx.commit()?;
    Ok(Some(Asleep {
        holder: asleep,
        through_seq,
    }))
}

/// What wakes a sleeping first mate, other than the captain at its pane:
/// actionable mail that arrived after it slept, or scheduled work now ready.
pub(crate) fn alarm(store: &Store, ship: &str, asleep: &Asleep) -> Result<Option<&'static str>> {
    let mail: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM mail WHERE recipient=?1 AND seq>?2
           AND COALESCE(delivery,'received')='received' AND (kind<>'notice' OR urgency<>'low'))",
        params![ship, asleep.through_seq],
        |r| r.get(0),
    )?;
    if mail {
        return Ok(Some("mail"));
    }
    Ok(crate::scheduling::has_ready(store, ship)?.then_some("scheduled work"))
}

pub(crate) fn woke(store: &Store, ship: &str, reason: &str) {
    let _ = store.audit("host.woke", &json!({"ship": ship, "reason": reason}));
}

/// How the native harness ended under `supervise`.
pub(crate) enum Ended {
    Exited(std::process::ExitStatus),
    Asleep(Asleep),
}

/// Wait for a hosted harness, refreshing the lease, and stop it once the ship
/// may sleep. Exec keeps the harness's own process identity, so the wait
/// below is evidence the payload stopped, not just a wrapper.
pub(crate) fn supervise(
    store: &Store,
    ship: &str,
    holder: &str,
    child: &mut std::process::Child,
    after_secs: u64,
) -> Result<Ended> {
    use std::time::{Duration, Instant};
    let mut refreshed = Instant::now();
    let mut checked = Instant::now();
    let mut leased = true;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Ended::Exited(status));
        }
        if leased && refreshed.elapsed() >= Duration::from_secs(30) {
            refreshed = Instant::now();
            // A replaced lease is never reclaimed; the session just runs on.
            leased = crate::presence::refresh(store, ship, holder, 90).unwrap_or(false);
        }
        if leased && after_secs > 0 && checked.elapsed() >= Duration::from_secs(5) {
            checked = Instant::now();
            if let Ok(Some(asleep)) = sleep(store, ship, holder, after_secs) {
                stop(child)?;
                return Ok(Ended::Asleep(asleep));
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Ask the harness to end as on logout, then insist; return once it has.
fn stop(child: &mut std::process::Child) -> Result<()> {
    use std::time::{Duration, Instant};
    let pid = child.id() as libc::pid_t;
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// The pane's terminal as it was before any harness changed it.
pub(crate) struct Terminal(Option<libc::termios>);

impl Terminal {
    pub(crate) fn save() -> Self {
        let mut mode = unsafe { std::mem::zeroed::<libc::termios>() };
        Self((unsafe { libc::tcgetattr(0, &mut mode) } == 0).then_some(mode))
    }

    /// Undo what a stopped full-screen harness may have left behind: its
    /// raw mode, alternate screen, hidden cursor, mouse and key reporting.
    fn restore(&self) {
        use std::io::Write;
        if let Some(mode) = &self.0 {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, mode) };
        }
        let mut out = std::io::stdout();
        let _ = out.write_all(
            b"\x1b[?1049l\x1b[?25h\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[<u\n",
        );
        let _ = out.flush();
    }
}

/// The agent a sleeping first mate reports as in its pane. Herdr drops a report
/// naming the agent it recorded as exited there, and clears that record only
/// when it detects a new agent process, so the harness's own name never works
/// here. `herdr agent prompt` cannot reach a pane under this label either: a
/// sleeping mate wakes by mail, focus, or input sent to its pane.
pub(crate) const PANE_AGENT: &str = "privateer";

/// Hold a sleeping ship's lease in its pane until something wakes it. Return
/// why it woke, or None once the lease is gone, so the pane can close.
pub(crate) fn rest(
    store: &Store,
    ship: &str,
    asleep: &Asleep,
    terminal: &Terminal,
) -> Result<Option<&'static str>> {
    terminal.restore();
    println!(
        "⚓ {ship}'s first mate is asleep to free memory. New mail or scheduled work wakes it.\n  Type a message and press Enter to mail it and wake the first mate, or just press Enter."
    );
    // Herdr lists only panes it sees an agent in, so the sleeping pane reports
    // itself as one under PANE_AGENT. Herdr's own detection takes over once woken.
    let mut herdr =
        crate::watch::Herdr::from_env(ship, PANE_AGENT, &format!("{ship} first mate · asleep"));
    let woke = wait(store, ship, asleep, terminal, herdr.as_mut());
    // Released before the harness starts again, or Herdr keeps the label.
    if let Some(herdr) = herdr.as_mut() {
        match herdr.release() {
            Ok(()) => {
                let read = read_back(herdr);
                if read["agent"] == PANE_AGENT || read.get("error").is_some() {
                    ignored(store, ship, "release", json!({"agent": PANE_AGENT}), read);
                }
            }
            Err(error) => {
                let error = format!("{error:#}");
                println!("✗ Herdr did not release {ship}'s pane: {error}");
                ignored(
                    store,
                    ship,
                    "release",
                    json!({"agent": PANE_AGENT}),
                    json!({"error": error}),
                );
            }
        }
    }
    woke
}

/// Whether Herdr lists the pane as this ship's asleep first mate.
fn listed(pane: &Value, ship: &str) -> bool {
    pane["agent"] == PANE_AGENT
        && pane["agent_status"] != "unknown"
        && pane["display_agent"] == ship
}

/// The pane as Herdr now lists it, or the reason Herdr could not say.
fn read_back(herdr: &mut crate::watch::Herdr) -> Value {
    herdr
        .pane()
        .unwrap_or_else(|error| json!({"error": format!("{error:#}")}))
}

/// A report or release Herdr answered ok but did not take. Warned about on
/// the pane once, and audited once per sleep, with what was sent and read back.
fn ignored(store: &Store, ship: &str, step: &str, sent: Value, read: Value) {
    println!(
        "⚠ Herdr does not list {ship}'s asleep first mate in this pane ({step}); Herdr may have dropped the report."
    );
    let _ = store.audit(
        "host.report_ignored",
        &json!({"ship": ship, "step": step, "sent": sent, "read": read}),
    );
}

fn wait(
    store: &Store,
    ship: &str,
    asleep: &Asleep,
    terminal: &Terminal,
    mut herdr: Option<&mut crate::watch::Herdr>,
) -> Result<Option<&'static str>> {
    use std::time::{Duration, Instant};
    let mut keyboard = terminal.0.is_some();
    let mut refreshed = Instant::now();
    let mut reported: Option<Instant> = None;
    let mut unlisted = false;
    // Focus wakes it only when the captain moves to the pane: a pane that
    // was focused as it fell asleep, or whose focus Herdr cannot report,
    // waits for Enter.
    let mut focused = herdr
        .as_deref_mut()
        .and_then(crate::watch::Herdr::focused)
        .unwrap_or(true);
    loop {
        if refreshed.elapsed() >= Duration::from_secs(30) {
            refreshed = Instant::now();
            if !crate::presence::refresh(store, ship, &asleep.holder, 90)? {
                return Ok(None);
            }
        }
        if let Some(reason) = alarm(store, ship, asleep)? {
            return Ok(Some(reason));
        }
        if let Some(herdr) = herdr.as_deref_mut() {
            if reported.is_none_or(|at| at.elapsed() >= Duration::from_secs(20)) {
                let message = format!("{ship}: asleep · focus or Enter wakes it");
                herdr.report("idle", &message);
                reported = Some(Instant::now());
                if !unlisted {
                    let read = read_back(herdr);
                    if !listed(&read, ship) {
                        unlisted = true;
                        let sent =
                            json!({"agent": PANE_AGENT, "display_agent": ship, "state": "idle"});
                        ignored(store, ship, "report", sent, read);
                    }
                }
            }
            match herdr.focused() {
                Some(true) if !focused => return Ok(Some("focus")),
                Some(now) => focused = now,
                None => {}
            }
        }
        // Poll rather than read on a thread, so no reader is left behind to
        // take the captain's first keys from the woken harness.
        let mut fd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let wait_ms = if keyboard { 1_000 } else { 0 };
        if keyboard && unsafe { libc::poll(&mut fd, 1, wait_ms) } > 0 {
            let mut line = [0u8; 8192];
            let read = unsafe { libc::read(0, line.as_mut_ptr().cast(), line.len()) };
            if read > 0 {
                unsafe { libc::tcflush(0, libc::TCIFLUSH) };
                let typed = String::from_utf8_lossy(&line[..read as usize]);
                say(store, ship, typed.trim());
                return Ok(Some("captain"));
            }
            keyboard = false; // closed input: only mail, work and focus wake it
        }
        if !keyboard {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

/// What the captain typed to a sleeping first mate reaches it as their mail,
/// as `privateer watch` sends it, so the woken session reads it first.
fn say(store: &Store, ship: &str, body: &str) {
    if body.is_empty() {
        return;
    }
    let sent = Store::open(store.home()).and_then(|mut store| {
        crate::dispatch(
            &mut store,
            "mail.send",
            json!({"from": "captain", "to": ship, "kind": "notice", "body": body}),
        )
    });
    match sent {
        Ok(_) => println!("→ you: {body}"),
        Err(error) => println!("✗ not sent: {error:#}"),
    }
}
