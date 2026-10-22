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
//! equivalent input, raises no ask. An ask whose refusals were all
//! identified is watched: when the session later gets past every one, by a
//! success after its own refusal, it is withdrawn, and its task no longer
//! waits on the captain. Another source's denial joining the ask ends the
//! watch: the pane's retries say nothing of it.
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
        match withdraw_passed(store, projects, folder) {
            Ok(withdrawn) => raised.extend(withdrawn),
            Err(error) => eprintln!("privateer: watched block asks were not read: {error:#}"),
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
        let raw: Option<String> = tx
            .query_row("SELECT record FROM ships WHERE name=?1", [ship], |row| {
                row.get(0)
            })
            .optional()?;
        let duty_task = raw
            .and_then(|record| serde_json::from_str::<Value>(&record).ok())
            .and_then(|record| {
                record["first_mate"]["duty_task"]
                    .as_str()
                    .map(str::to_owned)
            });
        let Some(duty_task) = duty_task else {
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
                let found = scan.as_ref().map(|s| s.denials.clone()).unwrap_or_default();
                let (passed, blocking): (Vec<Value>, Vec<Value>) =
                    found.into_iter().partition(|d| d["passed_ms"].is_i64());
                if blocking.is_empty() && !passed.is_empty() {
                    // The session is already past every refusal: nobody waits.
                    outcome["passed"] = json!(passed.len());
                    store.audit(
                        "permissions.ask_skipped",
                        &json!({"ship": ship, "task_id": duty_task, "reason": PAST_ALREADY,
                            "tool_use_ids": passed.iter().map(|d| &d["tool_use_id"]).collect::<Vec<_>>()}),
                    )?;
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
                    if let Some(id) = ask["ask"].as_str() {
                        watch(
                            store,
                            ship,
                            id,
                            ask["duplicate"] == true,
                            scan.as_ref(),
                            &denials,
                        )?;
                    }
                    outcome["ask"] = ask["ask"].clone();
                }
            }
        }
        tx.commit()?;
        raised.push(outcome);
    }
    Ok(raised)
}

/// Why an incident raised no ask: the session got past every refusal.
const PAST_ALREADY: &str = "the session got past every refusal it showed before the block was seen";

/// Watch an ask whose refusals were all read from one transcript session,
/// so it can be withdrawn when that session gets past them. A refusal that
/// was not identified, or another transcript, ends the watch: nothing can
/// then show that the session got past the whole ask.
fn watch(
    store: &Store,
    ship: &str,
    ask: &str,
    duplicate: bool,
    scan: Option<&crate::claude_denials::Scan>,
    denials: &[Value],
) -> Result<()> {
    let existing: Option<(String, String, String)> = store
        .connection()
        .query_row(
            "SELECT transcript, session, keys FROM hosted_automode_watch WHERE ask=?1",
            [ask],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    // A duplicate the watch did not start with was raised another way.
    if duplicate && existing.is_none() {
        return Ok(());
    }
    let mut keys: Vec<Value> = existing
        .as_ref()
        .and_then(|(_, _, keys)| serde_json::from_str(keys).ok())
        .unwrap_or_default();
    let session = denials.first().and_then(|d| d["session"].as_str());
    let path = scan.map(|s| s.transcript.to_string_lossy().into_owned());
    let mut watchable = scan.is_some() && session.is_some();
    if let Some((transcript, watched, _)) = &existing {
        watchable &= path.as_deref() == Some(transcript) && session == Some(watched);
    }
    // Each key keeps when it was last refused: an incident joining the ask
    // brings refusals later than the watch's place, and only a success after
    // its own refusal gets past one. A key refused again is blocking again.
    for denial in denials {
        let key = crate::claude_denials::retry_key(
            denial["tool_name"].as_str().unwrap_or_default(),
            &denial["tool_input"],
        );
        let denied = denial["denied_ms"].as_i64();
        match (key, denied) {
            (Some(key), Some(denied)) if denial["session"].as_str() == session => {
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
            _ => watchable = false,
        }
    }
    if !watchable {
        return unwatch(store, ask);
    }
    let (Some(scan), Some(session)) = (scan, session) else {
        return Ok(());
    };
    // A retry already running when the incident was read is behind where
    // the watch reads on from: it waits for its result by its id, which
    // may pass its key or, for a key already past, refuse it again.
    for (id, key, of) in &scan.running {
        let Some(known) = keys.iter_mut().find(|k| k["key"] == key.as_str()) else {
            continue;
        };
        if of.as_deref().is_some_and(|of| of != session) {
            continue;
        }
        let pending = known["pending"].as_array().cloned().unwrap_or_default();
        if !pending.iter().any(|p| p == id.as_str())
            && pending.len() < crate::claude_denials::MAX_PENDING
        {
            let mut pending = pending;
            pending.push(json!(id));
            known["pending"] = json!(pending);
        }
    }
    // A new watch reads on from where the incident's read ended; one already
    // reading keeps its place.
    store.connection().execute(
        "INSERT INTO hosted_automode_watch(ask,ship,transcript,session,offset,keys)
         VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(ask) DO UPDATE SET keys=excluded.keys",
        params![
            ask,
            ship,
            scan.transcript.to_string_lossy(),
            session,
            i64::try_from(scan.end).unwrap_or(i64::MAX),
            serde_json::to_string(&keys)?
        ],
    )?;
    Ok(())
}

/// One watched ask, as its row was read.
struct Watch {
    ask: String,
    ship: String,
    transcript: String,
    session: String,
    offset: i64,
    keys: String,
}

/// What one read of a watched ask's transcript found: where the next read
/// starts, its keys after the read, and, once its session got past every
/// refusal on it, the open ask and the reason to withdraw it with.
struct ReadOn {
    next: u64,
    keys: Vec<Value>,
    passed: Option<(Value, String)>,
}

/// The watched asks, as their rows are now.
fn watches(store: &Store) -> Result<Vec<Watch>> {
    let mut statement = store.connection().prepare(
        "SELECT ask, ship, transcript, session, offset, keys FROM hosted_automode_watch",
    )?;
    let watches = statement
        .query_map([], |r| {
            Ok(Watch {
                ask: r.get(0)?,
                ship: r.get(1)?,
                transcript: r.get(2)?,
                session: r.get(3)?,
                offset: r.get(4)?,
                keys: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(watches)
}

/// Read each watched ask's transcript on from where it was last read, and
/// withdraw an ask once its session has got past every refusal on it.
fn withdraw_passed(
    store: &Store,
    projects: &std::path::Path,
    folder: &std::path::Path,
) -> Result<Vec<Value>> {
    let mut withdrawn = vec![];
    for watch in watches(store)? {
        let (id, ship) = (&watch.ask, &watch.ship);
        // One bad watch must not stop the pass, or the panes' new blocks.
        let settled = read_on(store, projects, folder, &watch).and_then(|read| match read {
            Some(read) => settle(store, &watch, read),
            None => Ok(None),
        });
        match settled {
            Ok(Some(reason)) => {
                withdrawn.push(json!({"ship": ship, "withdrawn": id, "reason": reason}));
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!("privateer: the watch on {ship}'s block ask {id} failed: {error:#}");
            }
        }
    }
    Ok(withdrawn)
}

/// Read one watched ask's transcript on from its place. None when there is
/// nothing to settle: the ask is no longer open (its watch ends), or the
/// transcript cannot be read now.
fn read_on(
    store: &Store,
    projects: &std::path::Path,
    folder: &std::path::Path,
    watch: &Watch,
) -> Result<Option<ReadOn>> {
    let ask = crate::mail::find(store, &watch.ask)?.filter(|ask| ask["state"] == "open");
    let path = std::path::PathBuf::from(&watch.transcript);
    // Only a transcript of this home's pane is read.
    let Some(ask) = ask.filter(|_| path.starts_with(projects)) else {
        unwatch(store, &watch.ask)?;
        return Ok(None);
    };
    let mut keys: Vec<Value> = serde_json::from_str(&watch.keys).unwrap_or_default();
    // Each key from when it was last refused, or got past: a success after
    // its refusal passes it, and a refusal after its pass blocks it again.
    let wanted: std::collections::HashMap<String, i64> = keys
        .iter()
        .filter_map(|k| {
            let key = k["key"].as_str()?;
            let since = k["passed_ms"].as_i64().or(k["denied_ms"].as_i64());
            Some((key.to_owned(), since.unwrap_or_default()))
        })
        .collect();
    // The calls read earlier whose results were not written yet.
    let mut pending: Vec<(String, String)> = keys
        .iter()
        .flat_map(|k| {
            let key = k["key"].as_str().unwrap_or_default().to_owned();
            k["pending"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|id| id.as_str())
                .map(move |id| (id.to_owned(), key.clone()))
        })
        .collect();
    let offset = u64::try_from(watch.offset).unwrap_or_default();
    let Some(Retried { found, next, whole }) = crate::claude_denials::retried(
        &path,
        folder,
        &watch.session,
        offset,
        &wanted,
        &mut pending,
    ) else {
        return Ok(None);
    };
    // In the order written, so a key's last result decides it.
    for (key, result) in found {
        let Some(k) = keys.iter_mut().find(|k| k["key"] == key.as_str()) else {
            continue;
        };
        let denied = k["denied_ms"].as_i64().unwrap_or_default();
        match result {
            Retry::Passed(ms) if ms > denied && !k["passed_ms"].is_i64() => {
                k["passed_ms"] = json!(ms);
            }
            Retry::Passed(_) => {}
            Retry::Refused(ms) => {
                k["denied_ms"] = json!(denied.max(ms));
                k.as_object_mut().map(|k| k.remove("passed_ms"));
            }
        }
    }
    for k in &mut keys {
        let key = k["key"].as_str().unwrap_or_default().to_owned();
        let waiting: Vec<&str> = pending
            .iter()
            .filter(|(_, of)| *of == key)
            .map(|(id, _)| id.as_str())
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
    // Only a read that reached the end of what was written withdraws: a
    // record past it may refuse a call again. Short of that, its place and
    // keys are kept, and the next pass reads on from there.
    let passed = whole && !keys.is_empty() && keys.iter().all(|k| k["passed_ms"].is_i64());
    if !passed {
        return Ok(Some(ReadOn {
            next,
            keys,
            passed: None,
        }));
    }
    let latest = keys
        .iter()
        .filter_map(|k| k["passed_ms"].as_i64())
        .max()
        .unwrap_or_default();
    let mut tools: Vec<&str> = vec![];
    for tool in keys.iter().filter_map(|k| k["tool"].as_str()) {
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
    Ok(Some(ReadOn {
        next,
        keys,
        passed: Some((ask, reason)),
    }))
}

/// Keep one read's place and keys, or withdraw its ask once its session got
/// past it: the reason it was withdrawn with, or None while it still
/// blocks. Neither is done when the watch changed since it was read: an
/// incident that joined the ask in between brought refusals the read did
/// not weigh, or another source's block ended the watch. The next pass reads
/// on with the watch as it is then.
fn settle(store: &Store, watch: &Watch, read: ReadOn) -> Result<Option<String>> {
    let unchanged = || -> Result<bool> {
        let now: Option<(i64, String)> = store
            .connection()
            .query_row(
                "SELECT offset, keys FROM hosted_automode_watch WHERE ask=?1",
                [&watch.ask],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(now.is_some_and(|(offset, keys)| offset == watch.offset && keys == watch.keys))
    };
    let Some((ask, reason)) = read.passed else {
        let tx = store.write_transaction()?;
        if unchanged()? {
            store.connection().execute(
                "UPDATE hosted_automode_watch SET offset=?2, keys=?3 WHERE ask=?1",
                params![
                    watch.ask,
                    i64::try_from(read.next).unwrap_or(i64::MAX),
                    serde_json::to_string(&read.keys)?
                ],
            )?;
        }
        tx.commit()?;
        return Ok(None);
    };
    if !block_ask::withdraw_passed(store, &ask, &reason, unchanged)? {
        return Ok(None);
    }
    unwatch(store, &watch.ask)?;
    Ok(Some(reason))
}

/// End the watch on an ask: nothing will withdraw it for its session.
pub(crate) fn unwatch(store: &Store, ask: &str) -> Result<()> {
    store
        .connection()
        .execute("DELETE FROM hosted_automode_watch WHERE ask=?1", [ask])?;
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
            count(&store, "SELECT COUNT(*) FROM hosted_automode_watch"),
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
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM hosted_automode_watch"),
            0
        );
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
            count(store, "SELECT COUNT(*) FROM hosted_automode_watch"),
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
                "SELECT keys FROM hosted_automode_watch WHERE ask=?1",
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
                    "SELECT keys FROM hosted_automode_watch WHERE ask=?1",
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
        watch(&store, "privateer", &id, true, Some(&scan), &[refused_edit]).unwrap();
        append(&store, &path, |folder| {
            push(folder, "toolu_push_retry", "2026-10-20T00:30:00.000Z")
        });
        alert_in(&store, &observe("$ cargo test\nok"), Some(projects.path())).unwrap();
        // Then the push is refused again and joins: it is not past any more.
        let refused_push = json!({"tool_name": "Bash", "tool_input": {"command": "git push origin main"},
            "session": session, "denied_ms": crate::routes::iso_timestamp_ms("2026-10-20T00:31:00Z").unwrap()});
        watch(&store, "privateer", &id, true, Some(&scan), &[refused_push]).unwrap();
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
        watch(&store, "privateer", &id, true, Some(&scan), &[refused_edit]).unwrap();
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
            .query_row("SELECT offset FROM hosted_automode_watch", [], |r| r.get(0))
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
            assert!(read.passed.is_some());
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
        watch(&store, "privateer", &id, true, Some(&scan), &[refused_edit]).unwrap();
        assert_eq!(settle(&store, &watched, passed).unwrap(), None);
        assert_eq!(open_asks(&store), vec![id.clone()]);
        // Nor does a pass that read before a join write its keys over it.
        let watched = watches(&store).unwrap().remove(0);
        let stale = read_on(&store, projects.path(), &folder, &watched)
            .unwrap()
            .unwrap();
        assert!(stale.passed.is_none());
        let later = json!({"tool_name": "Bash", "tool_input": {"command": "git push origin main"},
            "session": "00000000-0000-4000-8000-0000000000f1",
            "denied_ms": crate::routes::iso_timestamp_ms("2026-10-20T00:33:00Z").unwrap()});
        watch(&store, "privateer", &id, true, Some(&scan), &[later]).unwrap();
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
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM hosted_automode_watch"),
            0
        );
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

    #[test]
    fn the_matcher_does_not_match_its_own_source() {
        assert!(read(include_str!("automode.rs")).is_none());
    }
}
