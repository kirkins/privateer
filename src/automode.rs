//! Auto-mode lockouts in a hosted first mate's pane.
//!
//! Claude Code's auto mode denies actions its classifier judges unsafe. After
//! three consecutive blocks it refuses every tool, even reads, and stops with
//! "3 consecutive actions were blocked". Its settings load only at launch, so a
//! retry before a relaunch is judged by the old policy. The host reads each
//! hosted pane's recent screen. When it shows the lockout line, or repeated
//! auto-mode denials, one captain notice per incident offers a relaunch, and
//! the denials become one block ask. The screen names no tool, so the ask
//! names it "unknown" until the transcript shape is captured.
use crate::block_ask;
use crate::mail;
use crate::store::Store;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The line Claude prints when the lockout starts.
pub(crate) const LOCKOUT: &str = "consecutive actions were blocked";
/// Denial lines on one screen that count as a block without the lockout line.
const REPEATED: usize = 2;
/// Lines of recent screen read per hosted pane.
pub(crate) const LINES: u32 = 200;

/// What one screen shows of an auto-mode block. `incident` identifies the
/// exact set of lines, so a screen that still shows it raises nothing again.
#[derive(Debug, PartialEq)]
pub(crate) struct Finding {
    pub(crate) lockout: bool,
    pub(crate) denials: Vec<String>,
    pub(crate) incident: String,
}

/// A line that reports a denied action. Auto mode's classifier labels its
/// reasons in brackets, such as `[Auto-Mode Bypass]`, and a denial names auto mode.
fn denial(line: &str) -> bool {
    if line.contains("[Auto-Mode") || line.contains("[Security ") {
        return true;
    }
    let lower = line.to_lowercase();
    (lower.contains("auto mode") || lower.contains("auto-mode"))
        && (lower.contains("denied") || lower.contains("blocked"))
}

/// The block on a screen, or None when it shows neither the lockout line nor
/// enough denials.
pub(crate) fn read(screen: &str) -> Option<Finding> {
    let mut lockout = false;
    let mut denials = vec![];
    for line in screen.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if line.to_lowercase().contains(LOCKOUT) {
            lockout = true;
        }
        if denial(line) {
            denials.push(line.to_owned());
        }
    }
    if !lockout && denials.len() < REPEATED {
        return None;
    }
    let mut hasher = Sha256::new();
    for line in &denials {
        hasher.update(line.as_bytes());
        hasher.update([0]);
    }
    hasher.update([u8::from(lockout)]);
    let incident = format!("{:x}", hasher.finalize());
    Some(Finding {
        lockout,
        denials,
        incident,
    })
}

/// Raise one captain notice, and one block ask for the denials, for each new
/// incident on a hosted pane. The last incident is kept per ship, so the same
/// screen raises nothing twice. A ship without a duty task has nobody to tell.
pub(crate) fn alert(store: &Store, observed: &[(String, String, String)]) -> Result<Vec<Value>> {
    let mut raised = vec![];
    for (ship, _, screen) in observed {
        let Some(finding) = read(screen) else {
            continue;
        };
        let tx = store.write_transaction()?;
        let raw: Option<String> = tx
            .query_row("SELECT record FROM ships WHERE name=?1", [ship], |row| {
                row.get(0)
            })
            .optional()?;
        let duty_task = raw
            .and_then(|record| serde_json::from_str::<Value>(&record).ok())
            .and_then(|record| record["first_mate"]["duty_task"].as_str().map(str::to_owned));
        let Some(duty_task) = duty_task else {
            continue;
        };
        let changed = tx.execute(
            "INSERT INTO hosted_automode(ship,incident,ms) VALUES(?1,?2,?3)
             ON CONFLICT(ship) DO UPDATE SET incident=excluded.incident, ms=excluded.ms
             WHERE hosted_automode.incident != excluded.incident",
            params![ship, finding.incident, Store::now_ms()],
        )?;
        if changed == 0 {
            continue;
        }
        let count = finding.denials.len();
        let body = format!(
            "{ship}'s first mate hit Claude's auto mode{}: {count} denied action{}. Its auto-mode settings load only at launch, so retries stay judged by the old policy. Relaunch it to reload them: `privateer host relaunch {ship}`.",
            if finding.lockout { " lockout" } else { "" },
            if count == 1 { "" } else { "s" },
        );
        mail::record_crew(
            &tx,
            &json!({
                "id": format!("hosted-automode-{ship}-{}", &finding.incident[..16]),
                "to": "captain",
                "kind": "notice",
                "task_id": duty_task,
                "urgency": "normal",
                "body": body,
                "payload": {"hosted_automode": ship, "incident": finding.incident,
                            "lockout": finding.lockout, "denied": count}
            }),
        )?;
        if !finding.denials.is_empty() {
            if let Ok(task) = store.task(&duty_task) {
                let (records, more) = block_ask::records(
                    &[json!({"tool_name": "unknown", "tool_input": {}})],
                    store.home().parent(),
                );
                block_ask::raise(store, ship, &duty_task, &task, None, "pane", records, more)?;
            }
        }
        tx.commit()?;
        raised.push(json!({"ship": ship, "incident": finding.incident, "lockout": finding.lockout}));
    }
    Ok(raised)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lockout_line_is_a_block() {
        let screen = "● Bash(git push)\n  ⎿  denied\n3 consecutive actions were blocked. Please review the transcript before continuing.";
        let finding = read(screen).unwrap();
        assert!(finding.lockout);
    }

    #[test]
    fn repeated_auto_mode_denials_are_a_block_without_the_lockout_line() {
        let screen = "Auto mode denied this action [Auto-Mode Bypass]\nAuto mode denied this action [Security Test Removal]";
        let finding = read(screen).unwrap();
        assert!(!finding.lockout);
        assert_eq!(finding.denials.len(), 2);
    }

    #[test]
    fn one_ordinary_error_is_not_a_block() {
        assert!(read("error: could not compile crate\nauto mode is on").is_none());
        assert!(read("Permission denied (publickey).").is_none());
    }

    #[test]
    fn the_same_screen_names_the_same_incident() {
        let screen = "3 consecutive actions were blocked.";
        assert_eq!(read(screen), read(screen));
        assert_ne!(
            read(screen).unwrap().incident,
            read("Auto mode denied [Auto-Mode Bypass]\nAuto mode denied [Security Test Removal]\n3 consecutive actions were blocked.")
                .unwrap()
                .incident
        );
    }

    #[test]
    fn one_incident_raises_one_notice_and_one_ask() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO ships(name,record) VALUES('privateer',?1)",
                [json!({"first_mate": {"duty_task": "privateer-automode-lockout"}}).to_string()],
            )
            .unwrap();
        let observed = vec![(
            "privateer".to_owned(),
            "blocked".to_owned(),
            "3 consecutive actions were blocked.".to_owned(),
        )];
        assert_eq!(alert(&store, &observed).unwrap().len(), 1);
        // The same screen, still showing the lockout, raises nothing.
        assert!(alert(&store, &observed).unwrap().is_empty());
        let notices: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM mail WHERE body LIKE '%auto mode lockout%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(notices, 1);
    }
}
