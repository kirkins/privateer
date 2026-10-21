//! A selected ship's live captain approvals, and the captain's revoke. The
//! dashboard never creates or extends an approval; a revoke runs through the
//! operator-action worker as `captain_approval.revoke`, never on the draw
//! thread, and only for a row the latest snapshot still lists.
use super::{
    fleet::Approval,
    model::{Ship, Snapshot, clean},
    ui::App,
};
use anyhow::{Result, ensure};
use crossterm::event::KeyCode;
use ratatui::text::Line;
use serde_json::json;
use std::path::{Path, PathBuf};

const CHANGED: &str = "Approval changed; refresh before revoking.";

/// The approvals a revoke chooser offers, as the snapshot showed them.
#[derive(Clone, Debug)]
pub struct Chooser {
    pub ship: String,
    pub home: PathBuf,
    pub approvals: Vec<Approval>,
    pub choice: usize,
}

/// One revoke the captain chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revoke {
    pub ship: String,
    pub home: PathBuf,
    pub id: String,
}

/// The selected ship's live approvals, when it is a local ship whose home
/// was read.
fn local(ship: &Ship) -> Option<(&Path, &[Approval])> {
    let live = ship.live.as_ref()?;
    (ship.remote.is_none() && live.error.is_none())
        .then_some((live.home.as_path(), live.approvals.as_slice()))
}

/// One line per approval: `<n>. <action> · <left> left · only <prefix> · <source>`.
pub fn line(n: usize, approval: &Approval, now: i64) -> String {
    let only = approval
        .command_prefix
        .as_deref()
        .map(|prefix| format!(" · only {}", clean(prefix, 120)))
        .unwrap_or_default();
    format!(
        "{n}. {} · {} left{only} · {}",
        clean(&approval.action, 160),
        crate::captain_approvals::left(approval.expires_ms - now),
        clean(&approval.source, 40)
    )
}

/// The detail lines for a ship's live approvals; none when it has none.
pub fn lines(ship: &Ship, now: i64) -> Vec<Line<'static>> {
    let Some((_, approvals)) = local(ship).filter(|(_, a)| !a.is_empty()) else {
        return vec![];
    };
    let mut lines = vec![Line::from(format!(
        "Captain approvals: {} live",
        approvals.len()
    ))];
    for (n, approval) in approvals.iter().enumerate() {
        lines.push(Line::from(line(n + 1, approval, now)));
    }
    lines
}

impl App {
    /// The selected local ship, when it has a live approval to revoke.
    pub fn revocable(&self) -> Option<&Ship> {
        if self.inbox_open || self.needs_focus || self.mail_pending {
            return None;
        }
        let id = self.ship.as_ref()?;
        self.snapshot
            .ships
            .iter()
            .find(|ship| &ship.id == id)
            .filter(|ship| local(ship).is_some_and(|(_, a)| !a.is_empty()))
    }

    pub fn begin_revoke(&mut self) {
        let Some(ship) = self.revocable() else {
            return;
        };
        let (home, approvals) = local(ship).unwrap();
        self.revoke = Some(Chooser {
            ship: ship.id.clone(),
            home: home.to_path_buf(),
            approvals: approvals.to_vec(),
            choice: 0,
        });
    }

    /// Keys while the chooser is open: Up/Down pick, Enter queues the
    /// revoke for the worker, Esc cancels.
    pub fn revoke_key(&mut self, key: KeyCode) {
        if self.mail_pending {
            return;
        }
        let Some(chooser) = self.revoke.as_mut() else {
            return;
        };
        match key {
            KeyCode::Esc => self.revoke = None,
            KeyCode::Up | KeyCode::Char('k') => chooser.choice = chooser.choice.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                chooser.choice = (chooser.choice + 1).min(chooser.approvals.len().saturating_sub(1))
            }
            KeyCode::Enter => {
                if let Some(approval) = chooser.approvals.get(chooser.choice) {
                    self.revoke_action = Some(Revoke {
                        ship: chooser.ship.clone(),
                        home: chooser.home.clone(),
                        id: approval.id.clone(),
                    });
                }
            }
            _ => {}
        }
    }

    pub fn revoke_lines(&self, chooser: &Chooser) -> Vec<Line<'static>> {
        let now = crate::store::Store::now_ms();
        let mut lines = vec![Line::from(format!(
            "Revoke a captain approval of {} · ↑↓ pick · Enter revoke · Esc cancel",
            clean(&chooser.ship, 160)
        ))];
        for (n, approval) in chooser.approvals.iter().enumerate() {
            lines.push(Line::from(format!(
                "{}{}",
                if n == chooser.choice { "> " } else { "  " },
                line(n + 1, approval, now)
            )));
        }
        lines.push(Line::from(
            "It stops at the ship's next idle point, wake or launch; the session never stops mid-turn.",
        ));
        lines
    }
}

/// Run a chosen revoke against the latest snapshot: refused when the
/// approval is no longer one of that ship's live rows there.
pub fn execute(snapshot: &Snapshot, revoke: &Revoke, binary: &Path) -> Result<String> {
    let listed = snapshot
        .ships
        .iter()
        .filter(|ship| ship.id == revoke.ship)
        .filter_map(local)
        .any(|(home, approvals)| {
            home == revoke.home && approvals.iter().any(|a| a.id == revoke.id)
        });
    ensure!(listed, "{CHANGED}");
    let verb = "captain_approval.revoke";
    let value = super::command::json(binary, &revoke.home, verb, &json!({"id": revoke.id}))?;
    Ok(super::command::summary(verb, &value))
}
