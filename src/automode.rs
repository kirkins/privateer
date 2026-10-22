//! Auto-mode lockouts in a hosted first mate's pane.
//!
//! Claude Code's auto mode denies actions its classifier judges unsafe. After
//! three consecutive blocks it refuses every tool, even reads, and stops. Its
//! settings load only at launch, so a retry before a relaunch is judged by the
//! old policy. The host reads each hosted pane's visible screen as plain text.
//! When it shows the stop line, one captain notice per incident offers a
//! relaunch. When it shows the stop line or a denial line, one block ask is
//! raised for the incident. The screen names no tool, so the ask names the
//! tool uses refused since the last incident, with their reasons, from the
//! pane's own Claude transcript (see `claude_denials`). When that cannot be
//! read, the ask says the denied tool was not identified and offers no Allow.
//!
//! The screen shows a denial as Claude's footer notice, for a few seconds,
//! when Claude shows it: Claude holds a notice while its pane holds notices
//! or a diff panel is open, and shows it later. So an incident can be seen
//! long after its refusal, and the session may be past it by then. A refusal
//! the session got past, by a later successful call of the same tool with an
//! equivalent input, raises no ask.
//!
//! The same footer seen again is the same incident, so the screen raises
//! nothing more while it shows. Each incident is watched instead: the watch
//! reads the pane's transcript on from the last whole record its incident's
//! read ended at, whether or not an ask is open, for as long as the incident
//! shows, and after that for as long as its ask stays open. When the session
//! gets past every refusal on an ask whose refusals were all identified, by
//! a success after its own refusal, the ask is withdrawn, and its task no
//! longer waits on the captain. A refusal of any other call read meanwhile
//! must be got past too, and the read must have reached the end of what was
//! written (see `passed`). A transcript cut or replaced by a shorter one is
//! read again from its start, and only a success read in it again gets past
//! a refusal. A withdrawn or answered ask leaves the watch reading on
//! without one, and a refusal it then reads that no success of the same
//! call follows raises a new ask, named from the transcript (see
//! `unasked`). Another source's denial joining the ask holds the watch: the
//! pane's retries say nothing of it, so it never withdraws that ask.
//!
//! A relaunch is a new incident, and a relaunched or woken pane can redraw
//! the footer of a refusal from before its launch. With nothing refused since
//! the launch, the footer's refusal is the newest one before it in the same
//! transcript. One an ask already held raises nothing new (see `held_by`).
//!
//! Matches are anchored to the start of a line, after bullet and box glyphs, so
//! a diff, a grep, a quoted instruction or a discussion never matches.
use crate::block_ask;
use crate::claude_denials::{Retried, Retry};
use crate::mail;
use crate::store::Store;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The stop line text, after its count. Claude prints it as "<N> consecutive
/// actions were blocked. Please review the transcript before continuing." when
/// the lockout starts.
const LOCKOUT: &str = "consecutive actions were blocked";
/// The rest of the stop line, on it or on the line after it.
const REVIEW: &str = "Please review the transcript";
/// A denial's shape in the transcript: `bash denied by auto mode · [<label>]`.
const DENIED: &str = " denied by auto mode";
/// The docs' shape for a blocked call: `Denied by auto mode classifier <reason>`.
const CLASSIFIER: &str = "Denied by auto mode classifier";
/// Bullet and box glyphs that Claude and its tool results print before a line.
/// A diff's `+` and `-` and a quote's `>` are not among them.
const GLYPHS: &str = "●⏺⎿│├└╰╭╮╯─•·›▸■□✗✘❯";

/// What one screen shows of an auto-mode block. `digest` identifies the
/// matched lines, so the same block seen again, scrolled or recoloured, is the
/// same incident.
#[derive(Debug, PartialEq)]
pub(crate) struct Finding {
    pub(crate) lockout: bool,
    pub(crate) denials: Vec<String>,
    pub(crate) digest: String,
}

/// A line's text with its leading glyphs and spaces removed.
fn text(line: &str) -> &str {
    line.trim_end()
        .trim_start_matches(|c: char| c.is_whitespace() || GLYPHS.contains(c))
}

/// "<N> consecutive actions were blocked", at the start of the line.
fn lockout_line(line: &str) -> bool {
    let digits = line.trim_start_matches(|c: char| c.is_ascii_digit());
    digits.len() < line.len()
        && digits
            .strip_prefix(' ')
            .is_some_and(|rest| rest.starts_with(LOCKOUT))
}

/// A tool result that reports a denial: `<word> denied by auto mode · [<label>]`
/// or the docs' `Denied by auto mode classifier <reason>`. One line is enough.
fn denial_line(line: &str) -> bool {
    if line.starts_with(CLASSIFIER) {
        return true;
    }
    let Some((tool, rest)) = line.split_once(DENIED) else {
        return false;
    };
    let label = rest.trim_start().strip_prefix('·').map(str::trim_start);
    !tool.is_empty()
        && tool
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && label.is_some_and(|label| label.starts_with('[') && label.contains(']'))
}

/// The block on a screen, or None when it shows neither the stop line nor a
/// denial. The screen shows only the latest denial, so one is enough.
pub(crate) fn read(screen: &str) -> Option<Finding> {
    let lines: Vec<&str> = screen
        .lines()
        .map(text)
        .filter(|line| !line.is_empty())
        .collect();
    let mut lockout = false;
    let mut denials = vec![];
    for (index, line) in lines.iter().enumerate() {
        if lockout_line(line) {
            let next = lines.get(index + 1).copied().unwrap_or_default();
            lockout |= line.contains(REVIEW) || next.contains(REVIEW);
        } else if denial_line(line) {
            denials.push((*line).to_owned());
        }
    }
    if !lockout && denials.is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update([u8::from(lockout)]);
    for line in &denials {
        hasher.update(line.as_bytes());
        hasher.update([0]);
    }
    Some(Finding {
        lockout,
        denials,
        digest: format!("{:x}", hasher.finalize()),
    })
}

/// The incident for a block on one pane launch. A relaunch is a new launch,
/// so the same block after it alerts again.
fn incident(launch: &str, finding: &Finding) -> String {
    let mut hasher = Sha256::new();
    hasher.update(launch.as_bytes());
    hasher.update([0]);
    hasher.update(finding.digest.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// The captain notice. It must never read as a denial or a stop line itself.
fn notice(ship: &str, finding: &Finding) -> String {
    let stop = if finding.lockout {
        " after repeated blocks"
    } else {
        ""
    };
    let refusals = match finding.denials.len() {
        0 => String::new(),
        1 => " 1 action was refused.".to_owned(),
        count => format!(" {count} actions were refused."),
    };
    format!(
        "{ship}'s first mate was stopped by Claude's automatic permission checks{stop}.{refusals} Its permission settings load only at launch, so retries stay judged by the old policy. Relaunch it to reload them: `privateer host relaunch {ship}`."
    )
}

/// Raise a captain notice for each new lockout, and one block ask for each new
/// lockout or denial, on a hosted pane. The last incident is kept per ship, so the same
/// screen raises nothing twice, and a readable screen with no block clears it.
/// A ship without a duty task has nobody to tell.
pub(crate) fn alert(store: &Store, observed: &[(String, String, String)]) -> Result<Vec<Value>> {
    alert_in(store, observed, crate::host::claude_projects().as_deref())
}

/// `alert`, reading pane transcripts under Claude's `projects` folder.
fn alert_in(
    store: &Store,
    observed: &[(String, String, String)],
    projects: Option<&std::path::Path>,
) -> Result<Vec<Value>> {
    let mut raised = vec![];
    let folder = store
        .home()
        .parent()
        .map(|f| f.canonicalize().unwrap_or(f.into()));
    if let Some((projects, folder)) = projects.zip(folder.as_deref()) {
        match follow(store, projects, folder) {
            Ok(followed) => raised.extend(followed),
            Err(error) => eprintln!("privateer: watched pane blocks were not read: {error:#}"),
        }
    }
    for (ship, _, screen) in observed {
        let tx = store.write_transaction()?;
        let Some(finding) = read(screen) else {
            // An empty screen is a failed read, not a clear one. A clear keeps
            // when it was first seen: the next incident's denials come after it.
            if !screen.trim().is_empty() {
                tx.execute(
                    "UPDATE hosted_automode SET incident='', ms=?2 WHERE ship=?1 AND incident<>''",
                    params![ship, Store::now_ms()],
                )?;
                // The incident is over: a watch with no ask ends with it.
                tx.execute(
                    "DELETE FROM hosted_automode_watches WHERE ship=?1 AND ask IS NULL",
                    [ship],
                )?;
            }
            tx.commit()?;
            continue;
        };
        let (launch, launched_ms): (String, i64) = tx
            .query_row(
                "SELECT pid, started, recorded_ms FROM hosted_panes WHERE ship=?1",
                [ship],
                |row| {
                    Ok((
                        format!("{}:{}", row.get::<_, i64>(0)?, row.get::<_, String>(1)?),
                        row.get(2)?,
                    ))
                },
            )
            .optional()?
            .unwrap_or_default();
        // The last incident, or the clear after it, bounds this one's denials.
        let last_ms: i64 = tx
            .query_row(
                "SELECT ms FROM hosted_automode WHERE ship=?1",
                [ship],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or_default();
        let incident = incident(&launch, &finding);
        let Some(duty_task) = duty_task(store, ship)? else {
            continue;
        };
        let changed = tx.execute(
            "INSERT INTO hosted_automode(ship,incident,ms) VALUES(?1,?2,?3)
             ON CONFLICT(ship) DO UPDATE SET incident=excluded.incident, ms=excluded.ms
             WHERE hosted_automode.incident != excluded.incident",
            params![ship, incident, Store::now_ms()],
        )?;
        if changed == 0 {
            continue;
        }
        let count = finding.denials.len();
        // Only the stop line offers a relaunch. A lone denial is a block ask.
        if finding.lockout {
            mail::record_crew(
                &tx,
                &json!({
                    "id": format!("hosted-automode-{ship}-{}", &incident[..16]),
                    "to": "captain",
                    "kind": "notice",
                    "task_id": duty_task,
                    "urgency": "normal",
                    "body": notice(ship, &finding),
                    "payload": {"hosted_automode": ship, "incident": incident,
                                "lockout": finding.lockout, "denied": count}
                }),
            )?;
        }
        let mut outcome = json!({"ship": ship, "incident": incident, "lockout": finding.lockout});
        if finding.lockout || count > 0 {
            if let Ok(task) = store.task(&duty_task) {
                let scan = projects
                    .zip(folder.as_deref())
                    .and_then(|(projects, folder)| {
                        crate::claude_denials::since(projects, folder, last_ms.max(launched_ms))
                    });
                let mut found = scan.as_ref().map(|s| s.denials.clone()).unwrap_or_default();
                // A relaunched or woken pane redraws the footer it showed
                // before its launch. With no refusal since the launch, the
                // footer's is the newest one before it, in the same transcript.
                let before = found.is_empty() && launched_ms > last_ms;
                if before {
                    found = scan
                        .as_ref()
                        .zip(folder.as_deref())
                        .and_then(|(s, folder)| {
                            crate::claude_denials::latest(&s.transcript, folder)
                        })
                        .and_then(|s| s.denials.into_iter().next())
                        .into_iter()
                        .collect();
                }
                let (passed, blocking): (Vec<Value>, Vec<Value>) =
                    found.into_iter().partition(|d| d["passed_ms"].is_i64());
                let asked = match blocking.first() {
                    Some(denial) if before => held_by(store, ship, denial)?,
                    _ => None,
                };
                if let Some((id, open)) = asked {
                    // That refusal was put to the captain already. Its ask
                    // holds it while open; once ended, the footer is seen and
                    // the watch reads on from here with no ask.
                    store.audit(
                        "permissions.ask_skipped",
                        &json!({"ship": ship, "task_id": duty_task, "reason": ASKED_ALREADY,
                            "ask": id, "tool_use_ids": [blocking[0]["tool_use_id"]]}),
                    )?;
                    let (ask, held) = if open {
                        (Some(id.as_str()), &blocking[..])
                    } else {
                        (None, &[][..])
                    };
                    watch(store, ship, &incident, ask, false, scan.as_ref(), held)?;
                    outcome["asked"] = json!(id);
                } else if blocking.is_empty() && !passed.is_empty() {
                    // The session is already past every refusal: nobody waits.
                    // The watch reads on from the last whole record the scan
                    // read, so a refusal after it, even one still being
                    // written, raises the ask when it is read.
                    outcome["passed"] = json!(passed.len());
                    store.audit(
                        "permissions.ask_skipped",
                        &json!({"ship": ship, "task_id": duty_task, "reason": PAST_ALREADY,
                            "tool_use_ids": passed.iter().map(|d| &d["tool_use_id"]).collect::<Vec<_>>()}),
                    )?;
                    watch(store, ship, &incident, None, false, scan.as_ref(), &[])?;
                } else {
                    let denials = if blocking.is_empty() {
                        vec![block_ask::unidentified()]
                    } else {
                        blocking
                    };
                    let (records, more) = block_ask::records(&denials, folder.as_deref());
                    let ask = block_ask::raise(
                        store, ship, &duty_task, &task, None, "pane", records, more,
                    )?;
                    // A declined ask holds nothing for the watch to follow.
                    let id = ask["ask"].as_str();
                    let held = if id.is_some() { &denials[..] } else { &[] };
                    let duplicate = ask["duplicate"] == true;
                    watch(store, ship, &incident, id, duplicate, scan.as_ref(), held)?;
                    outcome["ask"] = ask["ask"].clone();
                }
            }
        }
        tx.commit()?;
        raised.push(outcome);
    }
    Ok(raised)
}

/// The duty task a ship's block asks are raised on.
fn duty_task(store: &Store, ship: &str) -> Result<Option<String>> {
    let raw: Option<String> = store
        .connection()
        .query_row("SELECT record FROM ships WHERE name=?1", [ship], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(raw
        .and_then(|record| serde_json::from_str::<Value>(&record).ok())
        .and_then(|record| {
            record["first_mate"]["duty_task"]
                .as_str()
                .map(str::to_owned)
        }))
}

/// Why an incident raised no ask: the session got past every refusal.
const PAST_ALREADY: &str = "the session got past every refusal it showed before the block was seen";
/// Why a relaunched pane's block raised no ask: an ask already held the
/// refusal its footer shows.
const ASKED_ALREADY: &str = "an ask before the pane's launch already held the refusal it shows";

/// The pane's ask that held a refusal, by its tool use, open ones first, and
/// whether it is still open. None when no ask of the ship's ever held it.
fn held_by(store: &Store, ship: &str, denial: &Value) -> Result<Option<(String, bool)>> {
    let Some(id) = denial["tool_use_id"].as_str().filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    Ok(store
        .connection()
        .query_row(
            "SELECT id, state='open' FROM mail WHERE sender=?1 AND recipient='captain'
             AND json_extract(payload,'$.type')='tool_permission'
             AND EXISTS(SELECT 1 FROM json_each(payload,'$.block.denials')
                        WHERE json_extract(value,'$.tool_use_id')=?2)
             ORDER BY state='open' DESC, seq DESC LIMIT 1",
            params![ship, id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// Watch a ship's pane transcript for one incident. The watch belongs to the
/// incident, not to its ask: it lasts while the incident still shows, and
/// after that only while its ask is open. `ask` is the incident's open ask,
/// or None when it raised none (every refusal got past, or the captain
/// declined them). An open ask already watched keeps its watch, its place
/// and its keys, and `denials` join them; otherwise the watch starts where
/// the incident's read ended. A watch whose ask holds a refusal it cannot
/// see got past (not identified, of another session or transcript, or an
/// ask raised another way) is held: it never withdraws that ask.
fn watch(
    store: &Store,
    ship: &str,
    incident: &str,
    ask: Option<&str>,
    duplicate: bool,
    scan: Option<&crate::claude_denials::Scan>,
    denials: &[Value],
) -> Result<()> {
    let existing = watch_of(store, ship)?;
    let joins = match existing.as_ref().and_then(|w| w.ask.as_deref()) {
        Some(watched) if ask.is_none_or(|ask| ask == watched) => open(store, watched)?.is_some(),
        _ => false,
    };
    if let Some(existing) = existing.filter(|_| joins) {
        let mut keys: Vec<Value> = serde_json::from_str(&existing.keys).unwrap_or_default();
        let path = scan.map(|s| s.transcript.to_string_lossy().into_owned());
        let same = path.as_deref() == Some(existing.transcript.as_str());
        let mut held = existing.held;
        if !denials.is_empty() {
            held |= !same || !add(&mut keys, denials, &existing.session);
        }
        if let Some(scan) = scan.filter(|_| same) {
            wait_for(&mut keys, scan, &existing.session);
        }
        keys.retain(|k| k["denied_ms"].is_i64() || k["pending"].is_array());
        store.connection().execute(
            "UPDATE hosted_automode_watches SET incident=?2, held=?3, keys=?4 WHERE ship=?1",
            params![ship, incident, held, serde_json::to_string(&keys)?],
        )?;
        return Ok(());
    }
    let session = denials
        .first()
        .and_then(|d| d["session"].as_str())
        .or(scan.and_then(|s| s.session.as_deref()));
    let (Some(scan), Some(session)) = (scan, session) else {
        // No transcript of the pane's to read: nothing can follow it.
        store
            .connection()
            .execute("DELETE FROM hosted_automode_watches WHERE ship=?1", [ship])?;
        return Ok(());
    };
    let mut keys = vec![];
    // An ask raised another way holds what the pane's session cannot get past.
    let held = ask.is_some() && (duplicate || !add(&mut keys, denials, session));
    wait_for(&mut keys, scan, session);
    keys.retain(|k| k["denied_ms"].is_i64() || k["pending"].is_array());
    store.connection().execute(
        "INSERT OR REPLACE INTO hosted_automode_watches(ship,incident,ask,held,transcript,session,offset,keys)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            ship,
            incident,
            ask,
            held,
            scan.transcript.to_string_lossy(),
            session,
            i64::try_from(scan.end).unwrap_or(i64::MAX),
            serde_json::to_string(&keys)?
        ],
    )?;
    Ok(())
}

/// Add an incident's refusals to a watch's keys. Each key keeps when it was
/// last refused: an incident joining the ask brings refusals later than the
/// watch's place, and only a success after its own refusal gets past one. A
/// key refused again is blocking again. Whether every refusal could be
/// added: one with no tool or time, or of another session, cannot be seen
/// got past.
fn add(keys: &mut Vec<Value>, denials: &[Value], session: &str) -> bool {
    let mut all = true;
    for denial in denials {
        let key = crate::claude_denials::retry_key(
            denial["tool_name"].as_str().unwrap_or_default(),
            &denial["tool_input"],
        );
        let denied = denial["denied_ms"].as_i64();
        match (key, denied) {
            (Some(key), Some(denied)) if denial["session"].as_str() == Some(session) => {
                match keys.iter_mut().find(|k| k["key"] == key) {
                    Some(known) => {
                        let latest = known["denied_ms"].as_i64().unwrap_or_default().max(denied);
                        known["denied_ms"] = json!(latest);
                        if known["passed_ms"].as_i64().is_some_and(|ms| ms <= latest) {
                            known.as_object_mut().map(|k| k.remove("passed_ms"));
                        }
                    }
                    None => keys.push(
                        json!({"key": key, "tool": denial["tool_name"], "denied_ms": denied}),
                    ),
                }
            }
            _ => all = false,
        }
    }
    all
}

/// Keep the session's calls still running when an incident was read: they
/// are behind where the watch reads on from, so each waits for its result by
/// its id, which may pass its key, refuse it again, or refuse another call.
/// A key with no refusal yet is kept only for its running calls.
fn wait_for(keys: &mut Vec<Value>, scan: &crate::claude_denials::Scan, session: &str) {
    for (id, key, tool, of) in &scan.running {
        if of.as_deref().is_some_and(|of| of != session) {
            continue;
        }
        let waiting: usize = keys
            .iter()
            .filter_map(|k| k["pending"].as_array())
            .map(Vec::len)
            .sum();
        let at = match keys.iter().position(|k| k["key"] == key.as_str()) {
            Some(at) => at,
            None => {
                keys.push(json!({"key": key, "tool": tool}));
                keys.len() - 1
            }
        };
        let known = &mut keys[at];
        let mut pending = known["pending"].as_array().cloned().unwrap_or_default();
        if !pending.iter().any(|p| p == id.as_str()) && waiting < crate::claude_denials::MAX_PENDING
        {
            pending.push(json!(id));
            known["pending"] = json!(pending);
        }
    }
}

/// What an ask held is settled once it ends, and what an ask-less watch read
/// got past needs no ask: refusals and passes are dropped, and only the
/// calls still running are kept, so only a refusal read later raises one.
fn forget(keys: &mut Vec<Value>) {
    for k in keys.iter_mut().filter_map(Value::as_object_mut) {
        k.remove("denied_ms");
        k.remove("passed_ms");
    }
    keys.retain(|k| k["pending"].is_array());
}

/// A ship's incident watch, as its row was read. Its keys are the retry
/// keys of the calls its session was refused since its ask was raised, any
/// call, each as `{key, tool, denied_ms, passed_ms?, pending?}`, and of the
/// session's calls still running, as `{key, tool, pending}`.
#[derive(Debug, PartialEq)]
struct Watch {
    ship: String,
    incident: String,
    ask: Option<String>,
    held: bool,
    transcript: String,
    session: String,
    offset: i64,
    keys: String,
}

const WATCH: &str = "SELECT ship, incident, ask, held, transcript, session, offset, keys FROM hosted_automode_watches";

fn watch_row(r: &rusqlite::Row) -> rusqlite::Result<Watch> {
    Ok(Watch {
        ship: r.get(0)?,
        incident: r.get(1)?,
        ask: r.get(2)?,
        held: r.get(3)?,
        transcript: r.get(4)?,
        session: r.get(5)?,
        offset: r.get(6)?,
        keys: r.get(7)?,
    })
}

/// Every ship's watch, as its row is now.
fn watches(store: &Store) -> Result<Vec<Watch>> {
    let mut statement = store.connection().prepare(WATCH)?;
    let watches = statement
        .query_map([], watch_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(watches)
}

/// One ship's watch, as its row is now.
fn watch_of(store: &Store, ship: &str) -> Result<Option<Watch>> {
    Ok(store
        .connection()
        .query_row(&format!("{WATCH} WHERE ship=?1"), [ship], watch_row)
        .optional()?)
}

/// An ask, while it is open.
fn open(store: &Store, ask: &str) -> Result<Option<Value>> {
    Ok(crate::mail::find(store, ask)?.filter(|ask| ask["state"] == "open"))
}

/// Whether a watch's incident still shows: it is the last one seen on its
/// ship's pane, and that pane is still hosted.
fn live(store: &Store, watch: &Watch) -> Result<bool> {
    Ok(store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM hosted_automode a JOIN hosted_panes p ON p.ship=a.ship
         WHERE a.ship=?1 AND a.incident=?2)",
        params![watch.ship, watch.incident],
        |r| r.get(0),
    )?)
}

/// What one read of a watch's transcript found: where the next read starts,
/// its keys after the read, the ask it keeps (None once that ended), and
/// what to do.
struct ReadOn {
    next: u64,
    keys: Vec<Value>,
    ask: Option<String>,
    held: bool,
    /// Whether the watch's incident still shows.
    live: bool,
    then: Then,
}

enum Then {
    /// Keep the place and keys; the next pass reads on from there.
    Keep,
    /// Withdraw the open ask, with this reason: its session got past every
    /// refusal on it.
    Withdraw(Value, String),
    /// Raise an ask for refusals read while none was open, as
    /// `block_ask::records` gives them, and how many more there were.
    Raise(Vec<Value>, usize),
}

/// Read each ship's watch on from where it was last read: withdraw an ask
/// once its session got past every refusal on it, and raise one for a
/// refusal read while none was open.
fn follow(
    store: &Store,
    projects: &std::path::Path,
    folder: &std::path::Path,
) -> Result<Vec<Value>> {
    let mut followed = vec![];
    for watch in watches(store)? {
        let ship = &watch.ship;
        // One bad watch must not stop the pass, or the panes' new blocks.
        let settled = read_on(store, projects, folder, &watch).and_then(|read| match read {
            Some(read) => settle(store, &watch, read),
            None => Ok(None),
        });
        match settled {
            Ok(Some(outcome)) => followed.push(outcome),
            Ok(None) => {}
            Err(error) => eprintln!("privateer: the watch on {ship}'s pane failed: {error:#}"),
        }
    }
    Ok(followed)
}

/// Each refused key of a watch's from when it was last refused, or got
/// past: a success after its refusal passes it, and a refusal from its pass
/// blocks it again.
fn wanted(keys: &[Value]) -> std::collections::HashMap<String, i64> {
    keys.iter()
        .filter_map(|k| {
            let since = k["passed_ms"].as_i64().or(k["denied_ms"].as_i64())?;
            Some((k["key"].as_str()?.to_owned(), since))
        })
        .collect()
}

/// The calls a watch read earlier whose results were not written yet.
fn waiting(keys: &[Value]) -> Vec<(String, String, String)> {
    keys.iter()
        .flat_map(|k| {
            let key = k["key"].as_str().unwrap_or_default().to_owned();
            let tool = k["tool"].as_str().unwrap_or_default().to_owned();
            k["pending"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|id| id.as_str())
                .map(move |id| (id.to_owned(), key.clone(), tool.clone()))
        })
        .collect()
}

/// Read one watch's transcript on from its place: one read of at most one
/// step, and only when the transcript grew past it, or from its start when
/// it is now shorter than that place. None when there is nothing to settle: the watch ended (its incident is over and no ask of
/// its is open), or the transcript cannot be read now.
fn read_on(
    store: &Store,
    projects: &std::path::Path,
    folder: &std::path::Path,
    watch: &Watch,
) -> Result<Option<ReadOn>> {
    let live = live(store, watch)?;
    let ask = match &watch.ask {
        Some(id) => open(store, id)?,
        None => None,
    };
    let path = std::path::PathBuf::from(&watch.transcript);
    // Only a transcript of this home's pane is read.
    if !path.starts_with(projects) || (ask.is_none() && !live) {
        end(store, watch)?;
        return Ok(None);
    }
    let mut keys: Vec<Value> = serde_json::from_str(&watch.keys).unwrap_or_default();
    // An ask that ended (answered, withdrawn, ended by a policy change)
    // settled what it held; the watch reads on without it.
    if ask.is_none() {
        forget(&mut keys);
    }
    let offset = u64::try_from(watch.offset).unwrap_or_default();
    let mut pending = waiting(&keys);
    let mut read = crate::claude_denials::retried(
        &path,
        folder,
        &watch.session,
        offset,
        &wanted(&keys),
        &mut pending,
    );
    // Shorter than its place, the transcript was cut or replaced: it is
    // read again from its start. What the watch made of the old one passes
    // nothing in it, and its calls are not this one's: only the refusals
    // are kept, and a pass comes only from what is read again.
    if read.as_ref().is_some_and(|read| read.next < offset) {
        for k in keys.iter_mut().filter_map(Value::as_object_mut) {
            k.remove("passed_ms");
            k.remove("pending");
        }
        keys.retain(|k| k["denied_ms"].is_i64());
        pending.clear();
        read = crate::claude_denials::retried(
            &path,
            folder,
            &watch.session,
            0,
            &wanted(&keys),
            &mut pending,
        );
    }
    let Some(Retried { found, next, whole }) = read else {
        return Ok(None);
    };
    let entry = |keys: &mut Vec<Value>, key: &str, tool: &str| match keys
        .iter()
        .position(|k| k["key"] == key)
    {
        Some(at) => at,
        None => {
            keys.push(json!({"key": key, "tool": tool}));
            keys.len() - 1
        }
    };
    // In the order written, so a key's last result decides it. A refusal of
    // any call is kept with the ask's own: the ask cannot name it (its card
    // was raised for the refusals read then, as a joining incident's would
    // be), but it is withdrawn only once the session is past that one too.
    for (key, tool, result) in found {
        let at = entry(&mut keys, &key, &tool);
        let k = &mut keys[at];
        let denied = k["denied_ms"].as_i64();
        match result {
            Retry::Passed(ms) if denied.is_some_and(|d| ms > d) && !k["passed_ms"].is_i64() => {
                k["passed_ms"] = json!(ms);
            }
            Retry::Passed(_) => {}
            Retry::Refused(ms) => {
                k["denied_ms"] = json!(denied.unwrap_or(ms).max(ms));
                k.as_object_mut().map(|k| k.remove("passed_ms"));
            }
        }
    }
    for (_, key, tool) in &pending {
        entry(&mut keys, key, tool);
    }
    for k in &mut keys {
        let key = k["key"].as_str().unwrap_or_default().to_owned();
        let waiting: Vec<&str> = pending
            .iter()
            .filter(|(_, of, _)| *of == key)
            .map(|(id, _, _)| id.as_str())
            .collect();
        match k.as_object_mut() {
            Some(k) if waiting.is_empty() => {
                k.remove("pending");
            }
            Some(k) => {
                k.insert("pending".into(), json!(waiting));
            }
            None => {}
        }
    }
    keys.retain(|k| k["denied_ms"].is_i64() || k["pending"].is_array());
    let then = match &ask {
        Some(ask) if !watch.held => passed(ask, &keys, whole),
        Some(_) => Then::Keep,
        None => unasked(&path, folder, &mut keys),
    };
    Ok(Some(ReadOn {
        next,
        keys,
        held: watch.held && ask.is_some(),
        ask: watch.ask.clone().filter(|_| ask.is_some()),
        live,
        then,
    }))
}

/// Whether to withdraw an open ask. It is withdrawn only when the read
/// ended at the end of what was written, at a whole record, and every
/// refusal the watch has read, of any call, was followed by a success of
/// that call. Short of that, its place and keys are kept, and the next pass
/// reads on from there.
fn passed(ask: &Value, keys: &[Value], whole: bool) -> Then {
    let refused: Vec<&Value> = keys.iter().filter(|k| k["denied_ms"].is_i64()).collect();
    if !whole || refused.is_empty() || !refused.iter().all(|k| k["passed_ms"].is_i64()) {
        return Then::Keep;
    }
    let latest = refused
        .iter()
        .filter_map(|k| k["passed_ms"].as_i64())
        .max()
        .unwrap_or_default();
    let mut tools: Vec<&str> = vec![];
    for tool in refused.iter().filter_map(|k| k["tool"].as_str()) {
        if !tools.contains(&tool) {
            tools.push(tool);
        }
    }
    let reason = format!(
        "{}: the same {} call succeeded on retry at {}, so this decision is no longer needed",
        block_ask::PASSED,
        tools.join(", "),
        crate::routes::utc_time(latest)
    );
    Then::Withdraw(ask.clone(), reason)
}

/// What a watch with no open ask does with what it read. Each refusal not
/// followed by a success of its call is named from the transcript, as an
/// incident's are, newest first, and an ask is raised for those the
/// transcript does not show got past by now; one it cannot name is raised
/// as not identified. A refusal got past by now needs no ask, and is
/// forgotten with the rest once none is raised.
fn unasked(path: &std::path::Path, folder: &std::path::Path, keys: &mut Vec<Value>) -> Then {
    let blocking: Vec<usize> = (0..keys.len())
        .filter(|&at| keys[at]["denied_ms"].is_i64() && !keys[at]["passed_ms"].is_i64())
        .collect();
    let since = blocking
        .iter()
        .filter_map(|&at| keys[at]["denied_ms"].as_i64())
        .filter(|ms| *ms > 0)
        .min();
    let found = match since {
        Some(since) if !blocking.is_empty() => crate::claude_denials::scan(path, folder, since)
            .map(|scan| scan.denials)
            .unwrap_or_default(),
        _ => vec![],
    };
    let mut denials: Vec<Value> = vec![];
    let mut past = vec![];
    for at in blocking {
        let key = keys[at]["key"].as_str().unwrap_or_default();
        // The newest refusal of that call the transcript holds, by its
        // retry key, or by its id when the watch could not see its call.
        let named = found.iter().find(|d| {
            let id = d["tool_use_id"].as_str().unwrap_or_default();
            crate::claude_denials::unpassable(id) == key
                || crate::claude_denials::retry_key(
                    d["tool_name"].as_str().unwrap_or_default(),
                    &d["tool_input"],
                )
                .is_some_and(|k| k == key)
        });
        let denial = match named {
            Some(d) if d["passed_ms"].is_i64() => {
                past.push(at);
                continue;
            }
            Some(d) => d.clone(),
            None => {
                let mut unread = block_ask::unidentified();
                unread["denied_ms"] = keys[at]["denied_ms"].clone();
                unread
            }
        };
        let id = &denial["tool_use_id"];
        if id.is_null() || !denials.iter().any(|d| d["tool_use_id"] == *id) {
            denials.push(denial);
        }
    }
    if denials.is_empty() {
        forget(keys);
        return Then::Keep;
    }
    for at in past {
        if let Some(k) = keys[at].as_object_mut() {
            k.remove("denied_ms");
        }
    }
    keys.retain(|k| k["denied_ms"].is_i64() || k["pending"].is_array());
    let (records, more) = block_ask::records(&denials, Some(folder));
    Then::Raise(records, more)
}

/// Settle one read: withdraw its ask, raise one, or keep its place and
/// keys. Nothing is done when the watch changed since it was read: an
/// incident that joined its ask in between brought refusals the read did
/// not weigh, another source's block holds it, or a new incident replaced
/// it. The next pass reads on with the watch as it is then. Nor is an ask
/// withdrawn when its transcript grew since the read: what was written
/// since may refuse a call again; its place and keys are kept, and the next
/// pass reads it. What was done, or None.
fn settle(store: &Store, watch: &Watch, read: ReadOn) -> Result<Option<Value>> {
    let ReadOn {
        next,
        keys,
        ask,
        held,
        live,
        then,
    } = read;
    let ship = &watch.ship;
    let unchanged = || -> Result<bool> { Ok(watch_of(store, ship)?.as_ref() == Some(watch)) };
    let put = |ask: Option<&str>, held: bool, keys: &[Value]| -> Result<()> {
        store.connection().execute(
            "UPDATE hosted_automode_watches SET ask=?2, held=?3, offset=?4, keys=?5 WHERE ship=?1",
            params![
                ship,
                ask,
                held,
                i64::try_from(next).unwrap_or(i64::MAX),
                serde_json::to_string(keys)?
            ],
        )?;
        Ok(())
    };
    match then {
        Then::Withdraw(open, reason) => {
            let transcript = std::path::Path::new(&watch.transcript);
            // In the withdrawal's own transaction: the watch reads on
            // without the ask while its incident still shows, or ends.
            let mut after = keys.clone();
            forget(&mut after);
            let still = || -> Result<bool> {
                if !unchanged()? || crate::claude_denials::written(transcript) != Some(next) {
                    return Ok(false);
                }
                if live {
                    put(None, false, &after)?;
                } else {
                    store
                        .connection()
                        .execute("DELETE FROM hosted_automode_watches WHERE ship=?1", [ship])?;
                }
                Ok(true)
            };
            if block_ask::withdraw_passed(store, &open, &reason, still)? {
                return Ok(Some(
                    json!({"ship": ship, "withdrawn": open["id"], "reason": reason}),
                ));
            }
        }
        Then::Raise(records, more) => {
            let tx = store.write_transaction()?;
            let mut raised = None;
            if unchanged()? {
                let duty = duty_task(store, ship)?;
                let task = duty.as_deref().and_then(|duty| store.task(duty).ok());
                let mut keys = keys;
                match duty.zip(task) {
                    Some((duty, task)) => {
                        // A card that cannot name a refusal is never
                        // withdrawn by the watch, nor is one raised another way.
                        let unnamed = records.iter().any(|r| r["unidentified"] == true);
                        let ask = block_ask::raise(
                            store, ship, &duty, &task, None, "pane", records, more,
                        )?;
                        let id = ask["ask"].as_str();
                        if id.is_none() {
                            // Declined: the captain chose to do without it.
                            forget(&mut keys);
                        }
                        put(id, unnamed || ask["duplicate"] == true, &keys)?;
                        raised = Some(json!({"ship": ship, "incident": watch.incident,
                            "ask": ask["ask"], "read_on": true}));
                    }
                    // Nobody to tell.
                    None => {
                        forget(&mut keys);
                        put(None, false, &keys)?;
                    }
                }
            }
            tx.commit()?;
            return Ok(raised);
        }
        Then::Keep => {}
    }
    let same = i64::try_from(next).ok() == Some(watch.offset)
        && serde_json::to_string(&keys)? == watch.keys
        && ask == watch.ask
        && held == watch.held;
    if !same {
        let tx = store.write_transaction()?;
        if unchanged()? {
            put(ask.as_deref(), held, &keys)?;
        }
        tx.commit()?;
    }
    Ok(None)
}

/// End a watch, unless it changed since it was read.
fn end(store: &Store, watch: &Watch) -> Result<()> {
    let tx = store.write_transaction()?;
    if watch_of(store, &watch.ship)?.as_ref() == Some(watch) {
        store.connection().execute(
            "DELETE FROM hosted_automode_watches WHERE ship=?1",
            [&watch.ship],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Hold the watch on an ask another source's block joined: the pane's
/// session getting past its own refusals says nothing of that one, so the
/// watch never withdraws it. It still reads on, and once the ask ends, it
/// follows the incident without it.
pub(crate) fn hold(store: &Store, ask: &str) -> Result<()> {
    store.connection().execute(
        "UPDATE hosted_automode_watches SET held=1 WHERE ask=?1",
        [ask],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrap one line at whole words, as a narrow pane or an editor would.
    fn wrap(line: &str, width: usize) -> String {
        let mut out = String::new();
        let mut col = 0;
        for word in line.split_whitespace() {
            let len = word.chars().count();
            if col > 0 && col + 1 + len > width {
                out.push('\n');
                col = 0;
            } else if col > 0 {
                out.push(' ');
                col += 1;
            }
            out.push_str(word);
            col += len;
        }
        out
    }

    fn wrapped(text: &str, width: usize) -> String {
        text.lines()
            .map(|line| wrap(line, width))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_stop_line_is_a_block() {
        let screen = "● Bash(git push)\n  ⎿  Error\n3 consecutive actions were blocked. Please review the transcript before continuing.";
        assert!(read(screen).unwrap().lockout);
        // The review phrase may wrap onto the next line.
        let wrapped = "│ 3 consecutive actions were blocked.\n│ Please review the transcript before continuing.";
        assert!(read(wrapped).unwrap().lockout);
    }

    #[test]
    fn a_count_without_the_review_phrase_is_not_the_stop_line() {
        assert!(read("  ⎿  3 consecutive actions were blocked.").is_none());
        assert!(read("3 consecutive actions were blocked.\nok").is_none());
    }

    #[test]
    fn a_real_denial_line_is_a_block_on_its_own() {
        let screen = "bash denied by auto mode · [Security Test Removal] · /permissions";
        let finding = read(screen).unwrap();
        assert!(!finding.lockout);
        assert_eq!(finding.denials.len(), 1);
        // Claude's box glyphs before the line do not hide it.
        assert!(
            read("│ bash denied by auto mode · [Security Test Removal] · /permissions").is_some()
        );
    }

    #[test]
    fn a_real_lockout_screen_is_a_block() {
        // Each line opens with a quote, so the source never matches itself.
        let screen = concat!(
            "bash denied by auto mode · [Security Test Removal] · /permissions\n",
            "│ 3 consecutive actions were blocked. Please review the transcript before continuing.\n",
            "│ Latest blocked action: [Auto-Mode Bypass]"
        );
        let finding = read(screen).unwrap();
        assert!(finding.lockout);
        assert_eq!(finding.denials.len(), 1);
    }

    #[test]
    fn the_docs_denial_row_is_a_block() {
        let screen =
            "  ⎿  Denied by auto mode classifier Reads production data without a stated need";
        let finding = read(screen).unwrap();
        assert!(!finding.lockout);
        assert_eq!(finding.denials.len(), 1);
    }

    #[test]
    fn one_ordinary_error_is_not_a_block() {
        assert!(read("error: could not compile crate\nauto mode is on").is_none());
        assert!(read("Permission denied (publickey).").is_none());
        assert!(read("[Auto-Mode Bypass] Bash denied").is_none());
        assert!(read("bash denied by auto mode").is_none());
        assert!(read("bash denied by auto mode · Security Test Removal").is_none());
        assert!(read("Two tools denied by auto mode · [Security Test Removal]").is_none());
    }

    #[test]
    fn a_diff_of_this_module_is_not_a_block() {
        let diff = "+const LOCKOUT: &str = \"consecutive actions were blocked\";\n\
                    +    if line.contains(\"[Security \") {\n\
                    -3 consecutive actions were blocked.\n\
                    +[Auto-Mode Bypass] Bash denied\n\
                    @@ -37,7 +37,9 @@";
        assert!(read(diff).is_none());
    }

    #[test]
    fn a_grep_of_the_source_is_not_a_block() {
        let grep = "src/automode.rs:20:const LOCKOUT: &str = \"consecutive actions were blocked\";\n\
                    src/automode.rs:24:const LABELS: [&str; 2] = [\"[Auto-Mode \", \"[Security \"];\n\
                    src/cli.rs:311:- Never retry a denied action.";
        assert!(read(grep).is_none());
    }

    #[test]
    fn prose_about_the_block_is_not_a_block() {
        let prose = "The stop reads 3 consecutive actions were blocked when auto mode\n\
                     refuses. The log shows [Auto-Mode Bypass] and [Security Test Removal]\n\
                     mid-sentence, which a reader must not take for a live denial.";
        assert!(read(prose).is_none());
    }

    #[test]
    fn the_first_mate_instructions_are_not_a_block_at_any_width() {
        let instructions = instructions_text();
        for width in [80, 100, 140] {
            assert!(
                read(&wrapped(&instructions, width)).is_none(),
                "width {width}"
            );
        }
    }

    /// The template tells a first mate how to ask for a captain approval and
    /// what a block card offers, without reading as a block.
    #[test]
    fn the_first_mate_instructions_say_how_to_ask_for_a_captain_approval() {
        let instructions = instructions_text();
        for named in [
            "payload.approves",
            "ttl_secs",
            "\"approves\":true",
            "Approve for 24 h",
            "As approved",
        ] {
            assert!(instructions.contains(named), "{named}");
        }
    }

    #[test]
    fn the_captain_notice_is_not_a_block() {
        let finding = read(
            "3 consecutive actions were blocked. Please review the transcript before continuing.",
        )
        .unwrap();
        assert!(read(&notice("privateer", &finding)).is_none());
        let denied =
            read("bash denied by auto mode · [Security Test Removal] · /permissions").unwrap();
        assert!(read(&notice("privateer", &denied)).is_none());
    }

    fn instructions_text() -> String {
        crate::cli::instructions("privateer", "", "privateer --home /tmp/privateer")
    }

    fn store_with_duty(temp: &tempfile::TempDir) -> Store {
        let store = Store::open(temp.path()).unwrap();
        store
            .connection()
            .execute_batch(
                "INSERT INTO charters(id,data) VALUES('privateer','{}');
                 INSERT INTO tasks(id,charter_id,owner,title,status,parent,depth)
                 VALUES('privateer-automode-lockout','privateer','privateer','Lockout','open',NULL,0);",
            )
            .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO ships(name,record) VALUES('privateer',?1)",
                [json!({"first_mate": {"duty_task": "privateer-automode-lockout"}}).to_string()],
            )
            .unwrap();
        store
    }

    fn observe(screen: &str) -> Vec<(String, String, String)> {
        vec![(
            "privateer".to_owned(),
            "blocked".to_owned(),
            screen.to_owned(),
        )]
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.connection().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// `alert` with no transcripts to read: no test reads the real
    /// `~/.claude`.
    fn alert(store: &Store, observed: &[(String, String, String)]) -> Result<Vec<Value>> {
        alert_in(store, observed, None)
    }

    fn launch(store: &Store, pid: i64, started: &str) {
        store
            .connection()
            .execute(
                "INSERT OR REPLACE INTO hosted_panes(ship,pid,started,executable,recorded_ms)
                 VALUES('privateer',?1,?2,'privateer',0)",
                params![pid, started],
            )
            .unwrap();
    }

    #[test]
    fn one_incident_raises_one_notice_and_the_same_screen_again_raises_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        launch(&store, 10, "a");
        let first =
            "3 consecutive actions were blocked. Please review the transcript before continuing.";
        // Scrolled, with other output above, the same matched line is the same incident.
        let scrolled = format!("old output\nmore output\n{first}\n");
        assert_eq!(alert(&store, &observe(first)).unwrap().len(), 1);
        assert!(alert(&store, &observe(&scrolled)).unwrap().is_empty());
        let notices = count(
            &store,
            "SELECT COUNT(*) FROM mail WHERE body LIKE '%automatic permission checks%'",
        );
        assert_eq!(notices, 1);
    }

    #[test]
    fn a_relaunch_alerts_the_same_block_again() {
        let temp = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        launch(&store, 10, "a");
        let screen =
            "3 consecutive actions were blocked. Please review the transcript before continuing.";
        assert_eq!(alert(&store, &observe(screen)).unwrap().len(), 1);
        launch(&store, 11, "b");
        assert_eq!(alert(&store, &observe(screen)).unwrap().len(), 1);
    }

    #[test]
    fn a_clean_screen_clears_the_incident_so_the_next_block_alerts() {
        let temp = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        launch(&store, 10, "a");
        let screen =
            "3 consecutive actions were blocked. Please review the transcript before continuing.";
        assert_eq!(alert(&store, &observe(screen)).unwrap().len(), 1);
        assert!(
            alert(&store, &observe("$ cargo test\nok"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM hosted_automode WHERE incident<>''"
            ),
            0
        );
        assert_eq!(alert(&store, &observe(screen)).unwrap().len(), 1);
    }

    #[test]
    fn an_empty_read_does_not_clear_the_incident() {
        let temp = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        launch(&store, 10, "a");
        let screen =
            "3 consecutive actions were blocked. Please review the transcript before continuing.";
        assert_eq!(alert(&store, &observe(screen)).unwrap().len(), 1);
        assert!(alert(&store, &observe("")).unwrap().is_empty());
        assert!(alert(&store, &observe(screen)).unwrap().is_empty());
    }

    #[test]
    fn a_denial_raises_the_block_ask_and_no_notice() {
        let temp = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        launch(&store, 10, "a");
        let screen = "bash denied by auto mode · [Security Test Removal] · /permissions";
        assert_eq!(alert(&store, &observe(screen)).unwrap().len(), 1);
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM mail WHERE body LIKE '%automatic permission checks%'"
            ),
            0
        );
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM mail WHERE json_extract(payload,'$.block') IS NOT NULL"
            ),
            1
        );
    }

    #[test]
    fn a_lockout_only_screen_raises_the_notice_and_the_ask() {
        let temp = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        launch(&store, 10, "a");
        // The lockout box with no denial row above it.
        let screen = concat!(
            "│ 3 consecutive actions were blocked.\n",
            "│ Please review the transcript before continuing."
        );
        assert_eq!(alert(&store, &observe(screen)).unwrap().len(), 1);
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM mail WHERE body LIKE '%automatic permission checks%'"
            ),
            1
        );
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM mail WHERE json_extract(payload,'$.block') IS NOT NULL"
            ),
            1
        );
    }

    /// The pane's transcript, for the ship folder of `store`, under `projects`.
    fn transcript(store: &Store, projects: &std::path::Path, lines: usize) {
        let folder = store.home().parent().unwrap().canonicalize().unwrap();
        let dir = projects.join(crate::host::claude_project_name(&folder));
        std::fs::create_dir_all(&dir).unwrap();
        // The pane works in the ship's folder.
        let fixture = crate::claude_denials::tests::FIXTURE
            .replace("/ships/sysadmin", folder.to_str().unwrap());
        let text: Vec<&str> = fixture.lines().take(lines).collect();
        std::fs::write(dir.join("pane.jsonl"), text.join("\n") + "\n").unwrap();
    }

    /// The one block ask raised for the ship.
    fn ask(store: &Store) -> Value {
        let (body, payload): (String, String) = store
            .connection()
            .query_row(
                "SELECT body, payload FROM mail WHERE json_extract(payload,'$.block') IS NOT NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let mut payload: Value = serde_json::from_str(&payload).unwrap();
        payload["body"] = json!(body);
        payload
    }

    fn option_ids(ask: &Value) -> Vec<String> {
        ask["decision"]["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["id"].as_str().unwrap().to_owned())
            .collect()
    }

    const DENIED_SCREEN: &str =
        "bash denied by auto mode · [Credential Materialization] · /permissions";

    #[test]
    fn a_classifier_denied_compound_command_is_named_and_offers_authorize() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        launch(&store, 10, "a");
        // Only the credential copy: the prompt, its tool use and its refusal.
        transcript(&store, projects.path(), 3);
        let raised = alert_in(&store, &observe(DENIED_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(raised.len(), 1);
        let ask = ask(&store);
        let denial = &ask["block"]["denials"][0];
        assert_eq!(denial["tool"], "Bash");
        assert!(denial["rule"].is_null(), "{denial}");
        assert_eq!(denial["reason"], "[Credential Materialization]");
        let preview = denial["preview"].as_str().unwrap();
        assert!(
            preview.starts_with("cp ~/.config/sysadmin/registry.env"),
            "{preview}"
        );
        assert!(preview.contains("REGISTRY_TOKEN=•••"), "{preview}");
        assert!(!ask.to_string().contains("fake0token1value2"), "{ask}");
        assert_eq!(option_ids(&ask), ["authorize", "without", "stop"]);
        let text = ask["block"]["authorize"].as_str().unwrap();
        assert_eq!(
            text,
            "Running cp and echo commands its assigned work needs, even when the auto-mode check flags them as Credential Materialization."
        );
        let body = ask["body"].as_str().unwrap();
        assert!(
            body.contains("(refused: [Credential Materialization])"),
            "{body}"
        );
        assert!(
            body.contains(&format!(
                "privateer permissions authorize privateer '{text}'"
            )),
            "{body}"
        );
        assert!(!body.contains("unknown"), "{body}");
    }

    #[test]
    fn a_simple_denied_command_offers_its_allow_rule() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        // The pane launched after the credential copy, so only the later
        // refusal is this incident's.
        let launched = crate::routes::iso_timestamp_ms("2026-10-20T19:03:30Z").unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO hosted_panes(ship,pid,started,executable,recorded_ms)
                 VALUES('privateer',10,'a','privateer',?1)",
                [launched],
            )
            .unwrap();
        transcript(&store, projects.path(), usize::MAX);
        let screen = "│ 3 consecutive actions were blocked.\n│ Please review the transcript before continuing.";
        assert_eq!(
            alert_in(&store, &observe(screen), Some(projects.path()))
                .unwrap()
                .len(),
            1
        );
        let ask = ask(&store);
        let denials = ask["block"]["denials"].as_array().unwrap();
        assert_eq!(denials.len(), 1, "{denials:?}");
        assert_eq!(denials[0]["tool"], "Bash");
        assert_eq!(denials[0]["rule"], "Bash(cargo publish *)");
        assert_eq!(denials[0]["preview"], "cargo publish --dry-run");
        assert_eq!(option_ids(&ask), ["allow", "without", "stop"]);
        assert_eq!(
            ask["decision"]["options"][0]["label"],
            "Allow Bash(cargo publish *)"
        );
    }

    #[test]
    fn a_missing_transcript_names_no_tool_and_offers_no_allow() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        launch(&store, 10, "a");
        assert_eq!(
            alert_in(&store, &observe(DENIED_SCREEN), Some(projects.path()))
                .unwrap()
                .len(),
            1
        );
        let ask = ask(&store);
        let denial = &ask["block"]["denials"][0];
        assert_eq!(denial["tool"], "");
        assert!(denial["rule"].is_null());
        assert_eq!(denial["preview"], block_ask::UNIDENTIFIED);
        assert_eq!(option_ids(&ask), ["without", "stop"]);
        assert!(!ask.to_string().contains("unknown"), "{ask}");
        let body = ask["body"].as_str().unwrap();
        assert!(body.contains("denied tool not identified"), "{body}");
    }

    #[test]
    fn the_next_incident_reads_only_denials_after_the_clear() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        launch(&store, 10, "a");
        transcript(&store, projects.path(), usize::MAX);
        let observed = observe(DENIED_SCREEN);
        assert_eq!(
            alert_in(&store, &observed, Some(projects.path()))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(ask(&store)["block"]["denials"][0]["tool"], "Bash");
        alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        let (incident, ms): (String, i64) = store
            .connection()
            .query_row("SELECT incident, ms FROM hosted_automode", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(incident, "");
        assert!(ms > 0);
    }

    use crate::claude_denials::tests::{filler, retries};

    const BYPASS_SCREEN: &str = "bash denied by auto mode · [Auto-Mode Bypass] · /permissions";

    /// The pane's transcript, in fitforge's shape, keeping the fixture lines
    /// `keep` keeps; returns its path.
    fn pane(
        store: &Store,
        projects: &std::path::Path,
        keep: impl Fn(usize) -> bool,
    ) -> std::path::PathBuf {
        let folder = store.home().parent().unwrap().canonicalize().unwrap();
        let dir = projects.join(crate::host::claude_project_name(&folder));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("00000000-0000-4000-8000-0000000000f1.jsonl");
        std::fs::write(&path, retries(&folder, keep)).unwrap();
        path
    }

    fn append(store: &Store, path: &std::path::Path, text: impl Fn(&std::path::Path) -> String) {
        use std::io::Write;
        let folder = store.home().parent().unwrap().canonicalize().unwrap();
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(text(&folder).as_bytes()).unwrap();
    }

    fn asks(store: &Store) -> i64 {
        count(
            store,
            "SELECT COUNT(*) FROM mail WHERE json_extract(payload,'$.type')='tool_permission' AND kind='decision'",
        )
    }

    #[test]
    fn an_identified_bash_denial_names_its_tool_and_input() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        launch(&store, 10, "a");
        pane(&store, projects.path(), |n| n <= 3);
        alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        let ask = ask(&store);
        let denial = &ask["block"]["denials"][0];
        assert_eq!(denial["tool"], "Bash", "{denial}");
        assert!(denial["unidentified"].is_null(), "{denial}");
        assert_eq!(denial["tool_use_id"], "toolu_nativest_bash");
        assert_eq!(denial["reason"], "[Auto-Mode Bypass]");
        assert!(
            denial["preview"]
                .as_str()
                .unwrap()
                .contains("scratch/nativest.json"),
            "{denial}"
        );
        let body = ask["body"].as_str().unwrap();
        assert!(!body.contains(block_ask::UNIDENTIFIED), "{body}");
    }

    #[test]
    fn an_edit_denial_names_its_file() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        launch(&store, 10, "a");
        // Only the Edit's refusal.
        pane(&store, projects.path(), |n| n == 0 || (4..=6).contains(&n));
        alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        let ask = ask(&store);
        let denial = &ask["block"]["denials"][0];
        assert_eq!(denial["tool"], "Edit", "{denial}");
        assert_eq!(denial["tool_use_id"], "toolu_nativest_edit");
        assert!(
            denial["preview"]
                .as_str()
                .unwrap()
                .ends_with("scratch/plan.json"),
            "{denial}"
        );
        assert!(
            denial["rule"].as_str().unwrap().starts_with("Edit("),
            "{denial}"
        );
    }

    #[test]
    fn a_denial_the_session_retried_successfully_raises_no_ask() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        launch(&store, 10, "a");
        // Both refusals and both successful retries; no push.
        pane(&store, projects.path(), |n| !(7..=9).contains(&n));
        let raised = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(raised.len(), 1);
        assert_eq!(raised[0]["passed"], 2, "{raised:?}");
        assert_eq!(asks(&store), 0);
        assert!(
            block_ask::bearings_lines(&store, "privateer")
                .unwrap()
                .is_empty()
        );
        let skipped = count(
            &store,
            "SELECT COUNT(*) FROM audit WHERE kind='permissions.ask_skipped'",
        );
        assert_eq!(skipped, 1);
        // The same screen again is the same incident: still nothing.
        assert!(
            alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path()))
                .unwrap()
                .is_empty()
        );
        assert_eq!(asks(&store), 0);
    }

    #[test]
    fn a_raised_ask_is_withdrawn_when_the_session_gets_past_it() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let mut store = store_with_duty(&temp);
        launch(&store, 10, "a");
        let path = pane(&store, projects.path(), |n| n <= 3);
        alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        let id = ask(&store)["block"]["key"].clone();
        assert!(id.is_string());
        let open = block_ask::open_for_task(&store, "privateer", "privateer-automode-lockout")
            .unwrap()
            .unwrap();
        assert_eq!(
            block_ask::bearings_lines(&store, "privateer")
                .unwrap()
                .len(),
            1
        );
        // Work goes on; nothing is past it yet.
        append(&store, &path, |folder| {
            filler(folder, "2026-10-20T00:30:00.000Z", 64 * 1024)
        });
        let quiet = alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        assert!(quiet.is_empty(), "{quiet:?}");
        assert_eq!(asks(&store), 1);
        // A different command succeeding is not its retry.
        append(&store, &path, |folder| {
            retries(folder, |n| (10..=12).contains(&n)).replace("calories']=2400", "calories']=1")
        });
        alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        assert!(
            block_ask::open_for_task(&store, "privateer", "privateer-automode-lockout")
                .unwrap()
                .is_some()
        );
        let duty = "privateer-automode-lockout";
        // A scheduled task of the ship's waits on the ask, under a charter
        // that lets it be scheduled.
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        crate::dispatch(
            &mut store,
            "charter.put",
            json!({"id": "duty", "objective": "Keep the duty", "repo": repo, "min_free_bytes": 0}),
        )
        .unwrap();
        store
            .connection()
            .execute("UPDATE tasks SET charter_id='duty' WHERE id=?1", [duty])
            .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO task_schedule(task_id,record) VALUES('privateer-automode-lockout',?1)",
                [json!({"task_id": "privateer-automode-lockout", "owner": "privateer", "revision": 1,
                    "brief": "Keep the duty.", "state": "ready", "wait": null, "not_before_ms": 0,
                    "offer_id": null, "holder": null, "job_id": null, "profile": null,
                    "charter_hash": null, "attempts": 0, "created_ms": 0, "last_checked_ms": 0,
                    "lease_until_ms": 0, "reason": null})
                .to_string()],
            )
            .unwrap();
        let reason = "tool permission: Bash";
        let parked =
            crate::scheduling::park_on_ask(&store, duty, open["id"].as_str().unwrap(), reason);
        assert!(parked.unwrap());
        // Not offered once it resumes: this home has no profile to offer it with.
        store
            .connection()
            .execute(
                "UPDATE task_schedule SET record=json_set(record,'$.not_before_ms',?1)",
                [i64::MAX / 2],
            )
            .unwrap();
        // The same command succeeds: the ask is withdrawn and nothing waits.
        append(&store, &path, |folder| {
            retries(folder, |n| (10..=12).contains(&n))
        });
        let withdrawn =
            alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        assert_eq!(withdrawn.len(), 1, "{withdrawn:?}");
        assert_eq!(withdrawn[0]["withdrawn"], open["id"]);
        let ended = crate::mail::find(&store, open["id"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(ended["state"], "withdrawn");
        let reason = ended["end_reason"].as_str().unwrap();
        assert!(reason.starts_with(block_ask::PASSED), "{reason}");
        assert!(
            reason.contains("Bash call succeeded on retry at 2026-10-20 00:45 UTC"),
            "{reason}"
        );
        assert!(
            block_ask::open_for_task(&store, "privateer", "privateer-automode-lockout")
                .unwrap()
                .is_none()
        );
        assert!(
            block_ask::bearings_lines(&store, "privateer")
                .unwrap()
                .is_empty()
        );
        assert!(
            block_ask::pausing_ask(&store, "privateer")
                .unwrap()
                .is_none()
        );
        // The task waiting on it resumes, told why, instead of faulting.
        crate::scheduling::tick(&mut store).unwrap();
        let record: String = store
            .connection()
            .query_row(
                "SELECT record FROM task_schedule WHERE task_id=?1",
                [duty],
                |r| r.get(0),
            )
            .unwrap();
        let work: Value = serde_json::from_str(&record).unwrap();
        assert_eq!(work["state"], "ready", "{work}");
        assert!(work["wait"].is_null(), "{work}");
        let resumed = work["resumed_by"].as_str().unwrap();
        assert!(resumed.contains(block_ask::PASSED), "{resumed}");
        assert!(resumed.contains("Carry on"), "{resumed}");
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM hosted_automode_watches"),
            0
        );
    }

    #[test]
    fn a_denial_still_blocking_is_raised_without_the_ones_got_past() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        launch(&store, 10, "a");
        // Everything: the nativest edits were retried, the push was not.
        let path = pane(&store, projects.path(), |_| true);
        alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        let raised = ask(&store);
        let denials = raised["block"]["denials"].as_array().unwrap();
        assert_eq!(denials.len(), 1, "{denials:?}");
        assert_eq!(denials[0]["tool_use_id"], "toolu_push");
        assert_eq!(denials[0]["preview"], "git push origin main");
        // Later work that is not the push keeps it open.
        append(&store, &path, |folder| {
            filler(folder, "2026-10-20T00:50:00.000Z", 64 * 1024)
                + &retries(folder, |n| (10..=12).contains(&n))
        });
        alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM mail WHERE kind='decision' AND state='open'"
            ),
            1
        );
        // An unidentified ask is never withdrawn by a guess.
        let temp = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        launch(&store, 10, "a");
        let path = pane(&store, projects.path(), |n| n == 0 || n == 3);
        alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(ask(&store)["block"]["denials"][0]["unidentified"], true);
        assert_eq!(held(&store), Some(true));
        // Not even when the very command it may have been then succeeds.
        append(&store, &path, |folder| {
            retries(folder, |n| (10..=12).contains(&n))
        });
        let quiet = alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        assert!(quiet.is_empty(), "{quiet:?}");
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM mail WHERE kind='decision' AND state='open'"
            ),
            1
        );
    }

    /// One call of the pane's and its result, written at `at`: a success,
    /// or with `refused`, the classifier's refusal.
    fn call(
        folder: &std::path::Path,
        id: &str,
        tool: &str,
        input: Value,
        at: &str,
        refused: bool,
    ) -> String {
        let session = "00000000-0000-4000-8000-0000000000f1";
        let content = if refused {
            "Permission for this action was denied by the Claude Code auto mode classifier. Reason: [Auto-Mode Bypass]."
        } else {
            ""
        };
        let use_ = json!({"cwd": folder, "entrypoint": "cli", "type": "assistant", "sessionId": session,
            "timestamp": at, "message": {"role": "assistant", "content": [{"type": "tool_use",
            "id": id, "name": tool, "input": input}]}});
        let result = json!({"cwd": folder, "entrypoint": "cli", "type": "user", "sessionId": session,
            "timestamp": at, "message": {"role": "user", "content": [{"type": "tool_result",
            "tool_use_id": id, "content": content, "is_error": refused}]}});
        format!("{use_}\n{result}\n")
    }

    fn push(folder: &std::path::Path, id: &str, at: &str) -> String {
        call(
            folder,
            id,
            "Bash",
            json!({"command": "git push origin main"}),
            at,
            false,
        )
    }

    fn edit(folder: &std::path::Path, id: &str, at: &str, refused: bool) -> String {
        let file = folder.join("scratch/plan.json");
        call(
            folder,
            id,
            "Edit",
            json!({"file_path": file, "old_string": "1", "new_string": "2"}),
            at,
            refused,
        )
    }

    /// A pane incident's refusal joining the watched ask `id`, as its
    /// incident's scan read it.
    fn join(store: &Store, id: &str, scan: &crate::claude_denials::Scan, denial: Value) {
        let incident = watch_of(store, "privateer").unwrap().unwrap().incident;
        watch(
            store,
            "privateer",
            &incident,
            Some(id),
            true,
            Some(scan),
            &[denial],
        )
        .unwrap();
    }

    /// Whether the ship's watch is held, or None when it has none.
    fn held(store: &Store) -> Option<bool> {
        watch_of(store, "privateer").unwrap().map(|w| w.held)
    }

    /// The open block asks, by id.
    fn open_asks(store: &Store) -> Vec<String> {
        let mut statement = store
            .connection()
            .prepare("SELECT id FROM mail WHERE kind='decision' AND state='open' ORDER BY seq")
            .unwrap();
        statement
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<String>>>()
            .unwrap()
    }

    /// The pane's ask for its refused `git push`, watched; returns the ask
    /// and the transcript.
    fn push_refused(store: &Store, projects: &std::path::Path) -> (String, std::path::PathBuf) {
        launch(store, 10, "a");
        let path = pane(store, projects, |n| n == 0 || (7..=9).contains(&n));
        alert_in(store, &observe(BYPASS_SCREEN), Some(projects)).unwrap();
        let asks = open_asks(store);
        assert_eq!(asks.len(), 1);
        assert_eq!(
            count(store, "SELECT COUNT(*) FROM hosted_automode_watches"),
            1
        );
        (asks[0].clone(), path)
    }

    #[test]
    fn a_retry_still_running_when_the_ask_is_raised_withdraws_it_when_it_succeeds() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        launch(&store, 10, "a");
        // The refusal, and its retry written with no result yet.
        let path = pane(&store, projects.path(), |n| n <= 3 || n == 10 || n == 11);
        alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        let id = open_asks(&store).remove(0);
        // It runs a while; other work is written meanwhile.
        for _ in 0..3 {
            append(&store, &path, |folder| {
                filler(folder, "2026-10-20T00:45:46.000Z", 4 * 1024)
            });
            let quiet =
                alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
            assert!(quiet.is_empty(), "{quiet:?}");
        }
        append(&store, &path, |folder| retries(folder, |n| n == 12));
        let withdrawn =
            alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        assert_eq!(withdrawn.len(), 1, "{withdrawn:?}");
        assert_eq!(withdrawn[0]["withdrawn"], id.as_str());
    }

    #[test]
    fn a_refusal_joining_a_watched_ask_is_not_passed_by_an_earlier_success() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        let (id, path) = push_refused(&store, projects.path());
        // The screen clears; the next incident's refusals come after 00:20.
        alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        let since = crate::routes::iso_timestamp_ms("2026-10-20T00:20:00Z").unwrap();
        store
            .connection()
            .execute("UPDATE hosted_automode SET ms=?1", [since])
            .unwrap();
        // More than one read of work, an edit of plan.json that succeeds,
        // then a later edit of it the classifier refuses.
        append(&store, &path, |folder| {
            filler(folder, "2026-10-20T00:25:00.000Z", 9 * 1024 * 1024)
                + &edit(
                    folder,
                    "toolu_edit_early",
                    "2026-10-20T00:30:00.000Z",
                    false,
                )
                + &edit(
                    folder,
                    "toolu_edit_refused",
                    "2026-10-20T00:35:00.000Z",
                    true,
                )
        });
        // The edit's refusal joins the push's open ask, while the watch has
        // not yet read up to the earlier success.
        alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(open_asks(&store), vec![id.clone()]);
        let keys: String = store
            .connection()
            .query_row(
                "SELECT keys FROM hosted_automode_watches WHERE ask=?1",
                [&id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<Value>>(&keys).unwrap().len(),
            2,
            "{keys}"
        );
        // The push gets through: the refused edit still blocks.
        append(&store, &path, |folder| {
            push(folder, "toolu_push_retry", "2026-10-20T00:40:00.000Z")
        });
        for _ in 0..3 {
            let quiet =
                alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
            assert!(quiet.is_empty(), "{quiet:?}");
        }
        assert_eq!(open_asks(&store), vec![id.clone()]);
        // Refused again after a pass, a key blocks again.
        let watched =
            crate::claude_denials::retry_key("Bash", &json!({"command": "git push origin main"}))
                .unwrap();
        let keys: Vec<Value> = serde_json::from_str(
            &store
                .connection()
                .query_row::<String, _, _>(
                    "SELECT keys FROM hosted_automode_watches WHERE ask=?1",
                    [&id],
                    |r| r.get(0),
                )
                .unwrap(),
        )
        .unwrap();
        let pushed = keys.iter().find(|k| k["key"] == watched.as_str()).unwrap();
        assert!(pushed["passed_ms"].is_i64(), "{keys:?}");
        // The edit gets through too: now the session is past the whole ask.
        append(&store, &path, |folder| {
            edit(
                folder,
                "toolu_edit_retry",
                "2026-10-20T00:45:00.000Z",
                false,
            )
        });
        let withdrawn =
            alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        assert_eq!(withdrawn.len(), 1, "{withdrawn:?}");
        assert_eq!(withdrawn[0]["withdrawn"], id.as_str());
        let reason = withdrawn[0]["reason"].as_str().unwrap();
        assert!(
            reason.contains("the same Bash, Edit call succeeded on retry at 2026-10-20 00:45 UTC"),
            "{reason}"
        );
        assert!(open_asks(&store).is_empty());
    }

    #[test]
    fn a_key_refused_again_after_its_retry_blocks_again() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        let (id, path) = push_refused(&store, projects.path());
        let session = "00000000-0000-4000-8000-0000000000f1";
        let folder = store.home().parent().unwrap().canonicalize().unwrap();
        let scan = crate::claude_denials::since(projects.path(), &folder, 0).unwrap();
        // Half the ask passes: an edit refusal is watched with the push.
        let refused_edit = json!({"tool_name": "Edit", "tool_input": {"file_path": folder.join("scratch/plan.json")},
            "session": session, "denied_ms": crate::routes::iso_timestamp_ms("2026-10-20T00:10:00Z").unwrap()});
        join(&store, &id, &scan, refused_edit);
        append(&store, &path, |folder| {
            push(folder, "toolu_push_retry", "2026-10-20T00:30:00.000Z")
        });
        alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        // Then the push is refused again and joins: it is not past any more.
        let refused_push = json!({"tool_name": "Bash", "tool_input": {"command": "git push origin main"},
            "session": session, "denied_ms": crate::routes::iso_timestamp_ms("2026-10-20T00:31:00Z").unwrap()});
        join(&store, &id, &scan, refused_push);
        append(&store, &path, |folder| {
            edit(
                folder,
                "toolu_edit_retry",
                "2026-10-20T00:32:00.000Z",
                false,
            )
        });
        let quiet = alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        assert!(quiet.is_empty(), "{quiet:?}");
        assert_eq!(open_asks(&store), vec![id.clone()]);
        // Its own retry after that refusal passes it.
        append(&store, &path, |folder| {
            push(folder, "toolu_push_again", "2026-10-20T00:33:00.000Z")
        });
        let withdrawn =
            alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        assert_eq!(withdrawn.len(), 1, "{withdrawn:?}");
    }

    fn push_denied(folder: &std::path::Path, id: &str, at: &str) -> String {
        call(
            folder,
            id,
            "Bash",
            json!({"command": "git push origin main"}),
            at,
            true,
        )
    }

    #[test]
    fn a_retry_refused_again_before_the_next_read_keeps_the_ask() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        let (id, path) = push_refused(&store, projects.path());
        // The push gets through, then is refused again, before the watch
        // reads on; the footer still shows the same notice, so no new
        // incident raises anything.
        append(&store, &path, |folder| {
            push(folder, "toolu_push_retry", "2026-10-20T00:30:00.000Z")
                + &push_denied(folder, "toolu_push_again", "2026-10-20T00:31:00.000Z")
        });
        for _ in 0..2 {
            let quiet = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
            assert!(quiet.is_empty(), "{quiet:?}");
        }
        assert_eq!(open_asks(&store), vec![id.clone()]);
        // Only a success after that refusal gets past it.
        append(&store, &path, |folder| {
            push(folder, "toolu_push_through", "2026-10-20T00:32:00.000Z")
        });
        let withdrawn = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(withdrawn.len(), 1, "{withdrawn:?}");
        assert_eq!(withdrawn[0]["withdrawn"], id.as_str());
        let reason = withdrawn[0]["reason"].as_str().unwrap();
        assert!(reason.contains("at 2026-10-20 00:32 UTC"), "{reason}");
        // Got past, then refused again in a later read: blocking again.
        let temp = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        let (id, path) = push_refused(&store, projects.path());
        let folder = store.home().parent().unwrap().canonicalize().unwrap();
        let scan = crate::claude_denials::since(projects.path(), &folder, 0).unwrap();
        let refused_edit = json!({"tool_name": "Edit", "tool_input": {"file_path": folder.join("scratch/plan.json")},
            "session": "00000000-0000-4000-8000-0000000000f1",
            "denied_ms": crate::routes::iso_timestamp_ms("2026-10-20T00:10:00Z").unwrap()});
        join(&store, &id, &scan, refused_edit);
        append(&store, &path, |folder| {
            push(folder, "toolu_push_retry", "2026-10-20T00:30:00.000Z")
        });
        alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        append(&store, &path, |folder| {
            push_denied(folder, "toolu_push_again", "2026-10-20T00:31:00.000Z")
                + &edit(
                    folder,
                    "toolu_edit_retry",
                    "2026-10-20T00:32:00.000Z",
                    false,
                )
        });
        let quiet = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert!(quiet.is_empty(), "{quiet:?}");
        assert_eq!(open_asks(&store), vec![id]);
    }

    #[test]
    fn a_refusal_more_than_one_read_after_a_success_keeps_the_ask() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        let (id, path) = push_refused(&store, projects.path());
        // The push gets through, more than one read of work goes by, and it
        // is refused again, all before the watch reads on; the footer still
        // shows the same notice, so no new incident raises anything.
        append(&store, &path, |folder| {
            push(folder, "toolu_push_retry", "2026-10-20T00:30:00.000Z")
                + &filler(folder, "2026-10-20T00:31:00.000Z", 9 * 1024 * 1024)
                + &push_denied(folder, "toolu_push_again", "2026-10-20T00:40:00.000Z")
        });
        for _ in 0..3 {
            let quiet = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
            assert!(quiet.is_empty(), "{quiet:?}");
            assert_eq!(open_asks(&store), vec![id.clone()]);
        }
        // Read to the end, a success after that refusal gets past it.
        append(&store, &path, |folder| {
            push(folder, "toolu_push_through", "2026-10-20T00:45:00.000Z")
        });
        let withdrawn = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(withdrawn.len(), 1, "{withdrawn:?}");
        assert_eq!(withdrawn[0]["withdrawn"], id.as_str());
    }

    #[test]
    fn a_result_cut_at_the_first_read_is_read_whole_later() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        launch(&store, 10, "a");
        // The refusal, its retry, and half its result's line when the
        // incident is read.
        let path = pane(&store, projects.path(), |n| n <= 3 || n == 10 || n == 11);
        let folder = store.home().parent().unwrap().canonicalize().unwrap();
        let result = retries(&folder, |n| n == 12);
        let (head, rest) = result.split_at(result.len() / 2);
        append(&store, &path, |_| head.to_owned());
        alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        let id = open_asks(&store).remove(0);
        let offset: i64 = store
            .connection()
            .query_row("SELECT offset FROM hosted_automode_watches", [], |r| {
                r.get(0)
            })
            .unwrap();
        let written = std::fs::metadata(&path).unwrap().len();
        assert_eq!(offset as u64, written - head.len() as u64);
        // The rest of it is written: the watch reads the whole line.
        append(&store, &path, |_| rest.to_owned());
        let withdrawn =
            alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        assert_eq!(withdrawn.len(), 1, "{withdrawn:?}");
        assert_eq!(withdrawn[0]["withdrawn"], id.as_str());
    }

    /// A force push of the pane's, written at `at`: a success, or with
    /// `refused`, refused with the same label as the push.
    fn force(folder: &std::path::Path, id: &str, at: &str, refused: bool) -> String {
        call(
            folder,
            id,
            "Bash",
            json!({"command": "git push --force origin main"}),
            at,
            refused,
        )
    }

    /// A pass over a screen that still shows the same footer: it raises
    /// nothing, and leaves the open asks as `open`.
    fn still_open(store: &Store, projects: &std::path::Path, open: &str) {
        let quiet = alert_in(store, &observe(BYPASS_SCREEN), Some(projects)).unwrap();
        assert!(quiet.is_empty(), "{quiet:?}");
        assert_eq!(open_asks(store), vec![open.to_owned()]);
    }

    #[test]
    fn a_refusal_of_another_command_keeps_the_ask_until_it_too_is_got_past() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        let (id, path) = push_refused(&store, projects.path());
        // The push gets through, and another command is refused with the
        // same label, before the watch reads on: the footer is unchanged,
        // so no new incident raises an ask for it.
        append(&store, &path, |folder| {
            push(folder, "toolu_push_retry", "2026-10-20T00:30:00.000Z")
                + &force(folder, "toolu_force", "2026-10-20T00:31:00.000Z", true)
        });
        for _ in 0..2 {
            still_open(&store, projects.path(), &id);
        }
        // Once it too gets through, the ask is withdrawn.
        append(&store, &path, |folder| {
            force(
                folder,
                "toolu_force_retry",
                "2026-10-20T00:32:00.000Z",
                false,
            )
        });
        let withdrawn = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(withdrawn.len(), 1, "{withdrawn:?}");
        assert_eq!(withdrawn[0]["withdrawn"], id.as_str());
        let reason = withdrawn[0]["reason"].as_str().unwrap();
        assert!(reason.contains("at 2026-10-20 00:32 UTC"), "{reason}");
    }

    #[test]
    fn a_refusal_of_a_call_read_a_pass_earlier_is_followed_by_its_id() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        let (id, path) = push_refused(&store, projects.path());
        let folder = store.home().parent().unwrap().canonicalize().unwrap();
        let refused = force(&folder, "toolu_force", "2026-10-20T00:31:00.000Z", true);
        let (call, result) = refused.split_at(refused.find('\n').unwrap() + 1);
        // The force push is written while the push still blocks; its
        // refusal comes after that read, with the push's success.
        append(&store, &path, |_| call.to_owned());
        still_open(&store, projects.path(), &id);
        append(&store, &path, |folder| {
            push(folder, "toolu_push_retry", "2026-10-20T00:30:00.000Z") + result
        });
        still_open(&store, projects.path(), &id);
        // Its retry's success is matched to it.
        append(&store, &path, |folder| {
            force(
                folder,
                "toolu_force_retry",
                "2026-10-20T00:32:00.000Z",
                false,
            )
        });
        let withdrawn = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(withdrawn.len(), 1, "{withdrawn:?}");
        assert_eq!(withdrawn[0]["withdrawn"], id.as_str());
    }

    #[test]
    fn a_refusal_cut_by_the_read_keeps_the_ask() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        let (id, path) = push_refused(&store, projects.path());
        let folder = store.home().parent().unwrap().canonicalize().unwrap();
        // The push gets through and is refused again, and the read comes
        // while that refusal's line is half written.
        let again = push_denied(&folder, "toolu_push_again", "2026-10-20T00:31:00.000Z");
        let (head, rest) = again.split_at(again.len() - 40);
        append(&store, &path, |folder| {
            push(folder, "toolu_push_retry", "2026-10-20T00:30:00.000Z") + head
        });
        still_open(&store, projects.path(), &id);
        append(&store, &path, |_| rest.to_owned());
        still_open(&store, projects.path(), &id);
        append(&store, &path, |folder| {
            push(folder, "toolu_push_through", "2026-10-20T00:32:00.000Z")
        });
        let withdrawn = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(withdrawn.len(), 1, "{withdrawn:?}");
    }

    #[test]
    fn a_refusal_the_watch_cannot_follow_keeps_the_ask() {
        let projects = tempfile::tempdir().unwrap();
        // A refusal whose call is nowhere the watch read, and one in
        // another session of the transcript: neither can be seen got past.
        let unseen = |folder: &std::path::Path| {
            let refused = force(folder, "toolu_unseen", "2026-10-20T00:31:00.000Z", true);
            refused.lines().nth(1).unwrap().to_owned() + "\n"
        };
        let elsewhere = |folder: &std::path::Path| {
            force(folder, "toolu_elsewhere", "2026-10-20T00:31:00.000Z", true)
                .replace("0000000000f1", "0000000000f2")
        };
        for refusal in [&unseen as &dyn Fn(&std::path::Path) -> String, &elsewhere] {
            let temp = tempfile::tempdir().unwrap();
            let store = store_with_duty(&temp);
            let (id, path) = push_refused(&store, projects.path());
            append(&store, &path, |folder| {
                push(folder, "toolu_push_retry", "2026-10-20T00:30:00.000Z") + &refusal(folder)
            });
            still_open(&store, projects.path(), &id);
            append(&store, &path, |folder| {
                force(
                    folder,
                    "toolu_force_retry",
                    "2026-10-20T00:32:00.000Z",
                    false,
                )
            });
            still_open(&store, projects.path(), &id);
        }
    }

    #[test]
    fn a_refusal_written_between_the_read_and_the_withdrawal_keeps_the_ask() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        let (id, path) = push_refused(&store, projects.path());
        append(&store, &path, |folder| {
            push(folder, "toolu_push_retry", "2026-10-20T00:30:00.000Z")
        });
        let folder = store.home().parent().unwrap().canonicalize().unwrap();
        let watched = watches(&store).unwrap().remove(0);
        let read = read_on(&store, projects.path(), &folder, &watched)
            .unwrap()
            .unwrap();
        assert!(matches!(read.then, Then::Withdraw(..)));
        // The push is refused again after the read, before the withdrawal.
        append(&store, &path, |folder| {
            push_denied(folder, "toolu_push_again", "2026-10-20T00:31:00.000Z")
        });
        assert_eq!(settle(&store, &watched, read).unwrap(), None);
        assert_eq!(open_asks(&store), vec![id.clone()]);
        // The next pass reads on from the success and finds the refusal.
        still_open(&store, projects.path(), &id);
        append(&store, &path, |folder| {
            push(folder, "toolu_push_through", "2026-10-20T00:32:00.000Z")
        });
        let withdrawn = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(withdrawn.len(), 1, "{withdrawn:?}");
    }

    #[test]
    fn a_block_joining_between_the_read_and_the_withdrawal_keeps_the_ask() {
        let projects = tempfile::tempdir().unwrap();
        // The pass reads the watch and the push's success.
        let read = |store: &Store| {
            let folder = store.home().parent().unwrap().canonicalize().unwrap();
            let watch = watches(store).unwrap().remove(0);
            let read = read_on(store, projects.path(), &folder, &watch)
                .unwrap()
                .unwrap();
            assert!(matches!(read.then, Then::Withdraw(..)));
            (watch, read)
        };
        let temp = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        let (id, path) = push_refused(&store, projects.path());
        append(&store, &path, |folder| {
            push(folder, "toolu_push_retry", "2026-10-20T00:30:00.000Z")
        });
        let (watched, passed) = read(&store);
        // Before it withdraws, the wake's own refusal joins the ask.
        let task = store.task("privateer-automode-lockout").unwrap();
        let (records, more) = block_ask::records(
            &[json!({"tool_name": "Bash", "tool_input": {"command": "cargo publish"}})],
            None,
        );
        let joined = block_ask::raise(
            &store,
            "privateer",
            "privateer-automode-lockout",
            &task,
            None,
            "wake",
            records,
            more,
        )
        .unwrap();
        assert_eq!(joined["ask"], id.as_str());
        assert_eq!(settle(&store, &watched, passed).unwrap(), None);
        assert_eq!(open_asks(&store), vec![id]);
        // A pane incident joining in between adds a refusal the read did
        // not weigh: the ask stays open, and the watch keeps that refusal.
        let temp = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        let (id, path) = push_refused(&store, projects.path());
        append(&store, &path, |folder| {
            push(folder, "toolu_push_retry", "2026-10-20T00:30:00.000Z")
        });
        let (watched, passed) = read(&store);
        let folder = store.home().parent().unwrap().canonicalize().unwrap();
        let scan = crate::claude_denials::since(projects.path(), &folder, 0).unwrap();
        let refused_edit = json!({"tool_name": "Edit", "tool_input": {"file_path": folder.join("scratch/plan.json")},
            "session": "00000000-0000-4000-8000-0000000000f1",
            "denied_ms": crate::routes::iso_timestamp_ms("2026-10-20T00:10:00Z").unwrap()});
        join(&store, &id, &scan, refused_edit);
        assert_eq!(settle(&store, &watched, passed).unwrap(), None);
        assert_eq!(open_asks(&store), vec![id.clone()]);
        // Nor does a pass that read before a join write its keys over it.
        let watched = watches(&store).unwrap().remove(0);
        let stale = read_on(&store, projects.path(), &folder, &watched)
            .unwrap()
            .unwrap();
        assert!(matches!(stale.then, Then::Keep));
        let later = json!({"tool_name": "Bash", "tool_input": {"command": "git push origin main"},
            "session": "00000000-0000-4000-8000-0000000000f1",
            "denied_ms": crate::routes::iso_timestamp_ms("2026-10-20T00:33:00Z").unwrap()});
        join(&store, &id, &scan, later);
        let joined = watches(&store).unwrap().remove(0);
        assert_eq!(settle(&store, &watched, stale).unwrap(), None);
        let kept = watches(&store).unwrap().remove(0);
        assert_eq!(
            (kept.offset, kept.keys.as_str()),
            (joined.offset, joined.keys.as_str())
        );
        // The pass after reads on with both refusals: the edit still blocks.
        let quiet = alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        assert!(quiet.is_empty(), "{quiet:?}");
        assert_eq!(open_asks(&store), vec![id.clone()]);
        // Once both get through, it is withdrawn.
        append(&store, &path, |folder| {
            push(folder, "toolu_push_late", "2026-10-20T00:34:00.000Z")
                + &edit(
                    folder,
                    "toolu_edit_retry",
                    "2026-10-20T00:35:00.000Z",
                    false,
                )
        });
        let withdrawn =
            alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        assert_eq!(withdrawn.len(), 1, "{withdrawn:?}");
    }

    #[test]
    fn another_sources_block_joining_a_pane_ask_keeps_it_open() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        let (id, path) = push_refused(&store, projects.path());
        // The first mate's own wake is refused something else on its duty task.
        let task = store.task("privateer-automode-lockout").unwrap();
        let (records, more) = block_ask::records(
            &[json!({"tool_name": "Bash", "tool_input": {"command": "cargo publish"}})],
            None,
        );
        let joined = block_ask::raise(
            &store,
            "privateer",
            "privateer-automode-lockout",
            &task,
            None,
            "wake",
            records,
            more,
        )
        .unwrap();
        assert_eq!(joined["duplicate"], true);
        assert_eq!(joined["ask"], id.as_str());
        assert_eq!(held(&store), Some(true));
        // The pane's push then succeeds: the publish may still be blocked.
        append(&store, &path, |folder| {
            push(folder, "toolu_push_retry", "2026-10-20T00:30:00.000Z")
        });
        let quiet = alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        assert!(quiet.is_empty(), "{quiet:?}");
        assert_eq!(open_asks(&store), vec![id]);
    }

    #[test]
    fn another_ships_ask_attached_to_a_passed_one_is_detached_not_withdrawn() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        launch(&store, 10, "a");
        // A rule refused the pane's edit of plan.json: a rule ships share.
        let path = pane(&store, projects.path(), |n| n == 0 || (4..=6).contains(&n));
        alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        let id = open_asks(&store).remove(0);
        // Another ship of this home is refused the same edit and joins it.
        store
            .connection()
            .execute_batch(
                "INSERT INTO tasks(id,charter_id,owner,title,status,parent,depth)
                 VALUES('sysadmin-duty','privateer','sysadmin','Duty','open',NULL,0);",
            )
            .unwrap();
        let task = store.task("sysadmin-duty").unwrap();
        let folder = store.home().parent().unwrap().canonicalize().unwrap();
        let (records, more) = block_ask::records(
            &[
                json!({"tool_name": "Edit", "tool_input": {"file_path": folder.join("scratch/plan.json")}}),
            ],
            Some(&folder),
        );
        let theirs = block_ask::raise(
            &store,
            "sysadmin",
            "sysadmin-duty",
            &task,
            None,
            "wake",
            records,
            more,
        )
        .unwrap();
        let theirs = theirs["ask"].as_str().unwrap().to_owned();
        let attached = crate::mail::find(&store, &theirs).unwrap().unwrap();
        assert_eq!(attached["payload"]["attached_to"], id.as_str());
        // The pane's edit succeeds: its ask is withdrawn, theirs stands alone.
        append(&store, &path, |folder| {
            edit(
                folder,
                "toolu_edit_retry",
                "2026-10-20T00:30:00.000Z",
                false,
            )
        });
        let withdrawn =
            alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        assert_eq!(withdrawn.len(), 1, "{withdrawn:?}");
        assert_eq!(open_asks(&store), vec![theirs.clone()]);
        let detached = crate::mail::find(&store, &theirs).unwrap().unwrap();
        assert!(detached["payload"]["attached_to"].is_null(), "{detached}");
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM audit WHERE kind='permissions.detached'"
            ),
            1
        );
    }

    #[test]
    fn a_block_seen_long_after_its_refusal_is_named_and_dated() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        // The pane launched before the refusal; the screen shows the notice
        // forty minutes and several MiB of transcript later.
        let launched = crate::routes::iso_timestamp_ms("2026-10-20T00:00:00Z").unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO hosted_panes(ship,pid,started,executable,recorded_ms)
                 VALUES('privateer',10,'a','privateer',?1)",
                [launched],
            )
            .unwrap();
        let path = pane(&store, projects.path(), |n| n <= 3);
        append(&store, &path, |folder| {
            filler(folder, "2026-10-20T00:30:00.000Z", 3 * 1024 * 1024)
        });
        alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        let ask = ask(&store);
        let denial = &ask["block"]["denials"][0];
        assert_eq!(denial["tool"], "Bash", "{denial}");
        assert_eq!(denial["tool_use_id"], "toolu_nativest_bash");
        let denied = crate::routes::iso_timestamp_ms("2026-10-20T00:06:16Z").unwrap();
        assert_eq!(denial["denied_ms"], denied);
        let body = ask["body"].as_str().unwrap();
        assert!(
            body.contains("(refused at 2026-10-20 00:06 UTC, "),
            "{body}"
        );
        // And had the session got past it first, it is not filed at all.
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        launch(&store, 10, "a");
        let path = pane(&store, projects.path(), |n| n <= 3);
        append(&store, &path, |folder| {
            filler(folder, "2026-10-20T00:30:00.000Z", 3 * 1024 * 1024)
                + &retries(folder, |n| (10..=12).contains(&n))
        });
        alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(asks(&store), 0);
    }

    /// The open block ask's card: its id and its denials' tool use ids.
    fn open_card(store: &Store) -> (String, Vec<Value>) {
        let (id, payload): (String, String) = store
            .connection()
            .query_row(
                "SELECT id, payload FROM mail WHERE kind='decision' AND state='open'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let payload: Value = serde_json::from_str(&payload).unwrap();
        let ids = payload["block"]["denials"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["tool_use_id"].clone())
            .collect();
        (id, ids)
    }

    #[test]
    fn a_refusal_still_being_written_when_every_earlier_one_was_got_past_raises_its_ask() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        launch(&store, 10, "a");
        // The push is refused, its retry succeeds, and the same push is
        // refused again, its result half written when the screen is read.
        let path = pane(&store, projects.path(), |n| n == 0 || (7..=9).contains(&n));
        let folder = store.home().parent().unwrap().canonicalize().unwrap();
        let again = push_denied(&folder, "toolu_push_again", "2026-10-20T00:31:00.000Z");
        let (head, rest) = again.split_at(again.len() - 40);
        append(&store, &path, |folder| {
            push(folder, "toolu_push_retry", "2026-10-20T00:30:00.000Z") + head
        });
        let raised = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(raised[0]["passed"], 1, "{raised:?}");
        assert!(open_asks(&store).is_empty());
        // The refusal is whole; the footer still shows the same notice and
        // the screen never clears.
        append(&store, &path, |_| rest.to_owned());
        let raised = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(raised.len(), 1, "{raised:?}");
        let (id, denials) = open_card(&store);
        assert_eq!(denials, vec![json!("toolu_push_again")]);
        assert_eq!(raised[0]["ask"], id.as_str());
        // The same footer again raises nothing more.
        still_open(&store, projects.path(), &id);
    }

    #[test]
    fn a_refusal_after_the_ask_was_withdrawn_raises_a_new_one_under_the_same_footer() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        let (id, path) = push_refused(&store, projects.path());
        append(&store, &path, |folder| {
            push(folder, "toolu_push_retry", "2026-10-20T00:30:00.000Z")
        });
        let withdrawn = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(withdrawn.len(), 1, "{withdrawn:?}");
        assert_eq!(withdrawn[0]["withdrawn"], id.as_str());
        assert!(open_asks(&store).is_empty());
        // Another refusal with the same label: the footer is unchanged, and
        // the screen never clears.
        append(&store, &path, |folder| {
            force(folder, "toolu_force", "2026-10-20T00:31:00.000Z", true)
        });
        let raised = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(raised.len(), 1, "{raised:?}");
        let (again, denials) = open_card(&store);
        assert_ne!(again, id);
        assert_eq!(denials, vec![json!("toolu_force")]);
        // Its own retry withdraws that one too.
        append(&store, &path, |folder| {
            force(
                folder,
                "toolu_force_retry",
                "2026-10-20T00:32:00.000Z",
                false,
            )
        });
        let withdrawn = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(withdrawn.len(), 1, "{withdrawn:?}");
        assert_eq!(withdrawn[0]["withdrawn"], again.as_str());
    }

    #[test]
    fn a_refusal_in_a_shorter_transcript_after_the_ask_was_withdrawn_raises_a_new_one() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        let (id, path) = push_refused(&store, projects.path());
        append(&store, &path, |folder| {
            push(folder, "toolu_push_retry", "2026-10-20T00:30:00.000Z")
        });
        let withdrawn = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(withdrawn.len(), 1, "{withdrawn:?}");
        assert_eq!(withdrawn[0]["withdrawn"], id.as_str());
        // The transcript is replaced by a shorter one holding a new refusal;
        // the footer is unchanged, and the screen never clears.
        let offset = watch_of(&store, "privateer").unwrap().unwrap().offset as u64;
        let folder = store.home().parent().unwrap().canonicalize().unwrap();
        let replaced = force(&folder, "toolu_force", "2026-10-20T00:31:00.000Z", true);
        assert!((replaced.len() as u64) < offset);
        std::fs::write(&path, replaced).unwrap();
        let raised = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(raised.len(), 1, "{raised:?}");
        let (again, denials) = open_card(&store);
        assert_ne!(again, id);
        assert_eq!(denials, vec![json!("toolu_force")]);
        still_open(&store, projects.path(), &again);
    }

    #[test]
    fn a_shorter_transcript_does_not_withdraw_the_ask_on_what_the_old_one_passed() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        let (id, path) = push_refused(&store, projects.path());
        // The push gets through, and the read comes while a later line is
        // half written: the ask waits for the end.
        let folder = store.home().parent().unwrap().canonicalize().unwrap();
        let later = force(&folder, "toolu_force", "2026-10-20T00:31:00.000Z", true);
        append(&store, &path, |folder| {
            push(folder, "toolu_push_retry", "2026-10-20T00:30:00.000Z") + &later[..40]
        });
        still_open(&store, projects.path(), &id);
        // The transcript is replaced by a shorter one, in which the push is
        // refused again: what the old one passed passes nothing in it.
        let offset = watch_of(&store, "privateer").unwrap().unwrap().offset as u64;
        let replaced = push_denied(&folder, "toolu_push_again", "2026-10-20T00:32:00.000Z");
        assert!((replaced.len() as u64) < offset);
        std::fs::write(&path, replaced).unwrap();
        for _ in 0..3 {
            still_open(&store, projects.path(), &id);
        }
        // A success in it after that refusal gets past it.
        append(&store, &path, |folder| {
            push(folder, "toolu_push_through", "2026-10-20T00:33:00.000Z")
        });
        let withdrawn = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(withdrawn.len(), 1, "{withdrawn:?}");
        assert_eq!(withdrawn[0]["withdrawn"], id.as_str());
    }

    #[test]
    fn a_watch_with_no_ask_raises_one_only_for_a_refusal_not_got_past() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        launch(&store, 10, "a");
        // Both nativest refusals were retried successfully: no ask, and a
        // watch with none.
        let path = pane(&store, projects.path(), |n| !(7..=9).contains(&n));
        alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(asks(&store), 0);
        let watched = watch_of(&store, "privateer").unwrap().unwrap();
        assert_eq!(watched.ask, None);
        assert_eq!(
            watched.offset as u64,
            std::fs::metadata(&path).unwrap().len()
        );
        // Nothing written: nothing read, nothing raised.
        let quiet = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert!(quiet.is_empty(), "{quiet:?}");
        // A refusal its retry got past in the same read raises nothing.
        append(&store, &path, |folder| {
            force(folder, "toolu_force", "2026-10-20T00:50:00.000Z", true)
                + &force(
                    folder,
                    "toolu_force_retry",
                    "2026-10-20T00:51:00.000Z",
                    false,
                )
        });
        let quiet = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert!(quiet.is_empty(), "{quiet:?}");
        assert_eq!(asks(&store), 0);
        assert_eq!(watch_of(&store, "privateer").unwrap().unwrap().keys, "[]");
        // One that nothing follows is raised, named, and watched.
        append(&store, &path, |folder| {
            push_denied(folder, "toolu_push_late", "2026-10-20T00:52:00.000Z")
        });
        let raised = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(raised.len(), 1, "{raised:?}");
        let (id, denials) = open_card(&store);
        assert_eq!(denials, vec![json!("toolu_push_late")]);
        let card = ask(&store);
        assert_eq!(
            card["block"]["denials"][0]["preview"],
            "git push origin main"
        );
        assert_eq!(
            watch_of(&store, "privateer").unwrap().unwrap().ask,
            Some(id.clone())
        );
        still_open(&store, projects.path(), &id);
    }

    #[test]
    fn the_watch_ends_with_its_incident_once_no_ask_of_its_is_open() {
        let projects = tempfile::tempdir().unwrap();
        let watching = |store: &Store| watch_of(store, "privateer").unwrap().is_some();
        let skipped = |store: &Store| {
            launch(store, 10, "a");
            let path = pane(store, projects.path(), |n| !(7..=9).contains(&n));
            alert_in(store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
            assert!(watching(store));
            path
        };
        // A clear screen ends the incident and the watch with no ask; a
        // refusal after it is the next incident's, raised when it shows.
        let temp = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        let path = skipped(&store);
        alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        assert!(!watching(&store));
        append(&store, &path, |folder| {
            push_denied(folder, "toolu_push_late", "2026-10-20T00:52:00.000Z")
        });
        let quiet = alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        assert!(quiet.is_empty(), "{quiet:?}");
        assert_eq!(asks(&store), 0);
        // A pane no longer hosted ends it at the next pass.
        let temp = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        skipped(&store);
        store
            .connection()
            .execute("DELETE FROM hosted_panes", [])
            .unwrap();
        alert_in(&store, &[], Some(projects.path())).unwrap();
        assert!(!watching(&store));
        // A watched ask outlives the footer; the watch ends with the ask.
        let temp = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        let (id, _) = push_refused(&store, projects.path());
        alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        assert!(watching(&store));
        store
            .connection()
            .execute(
                "UPDATE mail SET state='withdrawn', end_reason='policy changed' WHERE id=?1",
                [&id],
            )
            .unwrap();
        alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        assert!(!watching(&store));
        // An ask answered while its footer still shows leaves the watch
        // reading on without it: what it held is not raised again.
        let temp = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        let (id, path) = push_refused(&store, projects.path());
        store
            .connection()
            .execute("UPDATE mail SET state='answered' WHERE id=?1", [&id])
            .unwrap();
        let quiet = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert!(quiet.is_empty(), "{quiet:?}");
        let watched = watch_of(&store, "privateer").unwrap().unwrap();
        assert_eq!((watched.ask, watched.keys.as_str()), (None, "[]"));
        append(&store, &path, |folder| {
            push_denied(folder, "toolu_push_again", "2026-10-20T00:31:00.000Z")
        });
        let raised = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(raised.len(), 1, "{raised:?}");
        assert_eq!(open_card(&store).1, vec![json!("toolu_push_again")]);
    }

    /// The pane relaunched, or woken with `claude --continue`, after its last
    /// incident and everything in its transcript so far.
    fn relaunch(store: &Store, pid: i64) {
        let last = count(store, "SELECT COALESCE(MAX(ms),0) FROM hosted_automode");
        store
            .connection()
            .execute(
                "INSERT OR REPLACE INTO hosted_panes(ship,pid,started,executable,recorded_ms)
                 VALUES('privateer',?1,'b','privateer',?2)",
                params![pid, Store::now_ms().max(last + 1)],
            )
            .unwrap();
    }

    #[test]
    fn a_relaunch_redrawing_a_refusal_already_asked_about_raises_no_new_ask() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        launch(&store, 10, "a");
        // The Bash refusal is filed with its tool named, and the footer
        // goes; the captain authorizes the card.
        let path = pane(&store, projects.path(), |n| n <= 3);
        alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        let (id, denials) = open_card(&store);
        assert_eq!(denials, vec![json!("toolu_nativest_bash")]);
        alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        store
            .connection()
            .execute("UPDATE mail SET state='answered' WHERE id=?1", [&id])
            .unwrap();
        // The pane is woken, and redraws the same footer: nothing new.
        relaunch(&store, 11);
        let raised = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(raised.len(), 1, "{raised:?}");
        assert_eq!(raised[0]["asked"], id.as_str());
        assert!(raised[0]["ask"].is_null(), "{raised:?}");
        assert_eq!(asks(&store), 1);
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM audit WHERE kind='permissions.ask_skipped'"
            ),
            1
        );
        let watched = watch_of(&store, "privateer").unwrap().unwrap();
        assert_eq!((watched.ask, watched.keys.as_str()), (None, "[]"));
        assert_eq!(
            watched.offset as u64,
            std::fs::metadata(&path).unwrap().len()
        );
        // A refusal after the wake is still raised, under the same footer.
        append(&store, &path, |folder| {
            push_denied(folder, "toolu_push_late", "2026-10-20T00:52:00.000Z")
        });
        let raised = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(raised.len(), 1, "{raised:?}");
        assert_eq!(open_card(&store).1, vec![json!("toolu_push_late")]);
        // Woken again while that card is open: the redraw joins it.
        let (open, _) = open_card(&store);
        relaunch(&store, 12);
        let raised = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(raised[0]["asked"], open.as_str(), "{raised:?}");
        assert_eq!(asks(&store), 2);
        assert_eq!(
            watch_of(&store, "privateer").unwrap().unwrap().ask,
            Some(open.clone())
        );
        still_open(&store, projects.path(), &open);
    }

    #[test]
    fn a_relaunch_redrawing_a_refusal_never_asked_about_raises_it_named() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        // The footer was never seen before the pane was relaunched.
        pane(&store, projects.path(), |n| n <= 3);
        relaunch(&store, 11);
        let raised = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(raised.len(), 1, "{raised:?}");
        assert_eq!(asks(&store), 1);
        let (id, denials) = open_card(&store);
        assert_eq!(raised[0]["ask"], id.as_str());
        assert_eq!(denials, vec![json!("toolu_nativest_bash")]);
        let card = ask(&store);
        assert_eq!(card["block"]["denials"][0]["tool"], "Bash");
        let body = card["body"].as_str().unwrap();
        assert!(!body.contains(block_ask::UNIDENTIFIED), "{body}");
    }

    #[test]
    fn a_relaunch_redrawing_a_footer_with_no_refusal_anywhere_raises_it_not_identified() {
        let temp = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store_with_duty(&temp);
        // Only the prompt, and a busy session's records after it.
        let path = pane(&store, projects.path(), |n| n == 0);
        append(&store, &path, |folder| {
            filler(folder, "2026-10-20T00:30:00.000Z", 1024 * 1024)
        });
        relaunch(&store, 11);
        let raised = alert_in(&store, &observe(BYPASS_SCREEN), Some(projects.path())).unwrap();
        assert_eq!(raised.len(), 1, "{raised:?}");
        assert_eq!(asks(&store), 1);
        let card = ask(&store);
        assert_eq!(
            card["block"]["denials"][0]["preview"],
            block_ask::UNIDENTIFIED
        );
        assert_eq!(option_ids(&card), ["without", "stop"]);
    }

    #[test]
    fn the_matcher_does_not_match_its_own_source() {
        assert!(read(include_str!("automode.rs")).is_none());
    }
}
