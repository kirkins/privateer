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
//! Matches are anchored to the start of a line, after bullet and box glyphs, so
//! a diff, a grep, a quoted instruction or a discussion never matches.
use crate::block_ask;
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
        if finding.lockout || count > 0 {
            if let Ok(task) = store.task(&duty_task) {
                let folder = store.home().parent().map(|f| f.canonicalize().unwrap_or(f.into()));
                let denials = projects
                    .zip(folder.as_deref())
                    .and_then(|(projects, folder)| {
                        crate::claude_denials::since(projects, folder, last_ms.max(launched_ms))
                    })
                    .filter(|denials| !denials.is_empty())
                    .unwrap_or_else(|| vec![block_ask::unidentified()]);
                let (records, more) = block_ask::records(&denials, folder.as_deref());
                block_ask::raise(store, ship, &duty_task, &task, None, "pane", records, more)?;
            }
        }
        tx.commit()?;
        raised.push(json!({"ship": ship, "incident": incident, "lockout": finding.lockout}));
    }
    Ok(raised)
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
        assert_eq!(count(&store, "SELECT COUNT(*) FROM hosted_automode"), 0);
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

    #[test]
    fn the_matcher_does_not_match_its_own_source() {
        assert!(read(include_str!("automode.rs")).is_none());
    }
}
