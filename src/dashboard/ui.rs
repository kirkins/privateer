use super::{
    bridge::{Answer, Attention, Binding, FRESH_MS},
    captain::{Composer, HistoryBinding, Mail},
    herdr,
    model::{Snapshot, Task, clean, count},
    voyage,
};
use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const NOTICE_TTL: Duration = Duration::from_secs(8);

#[derive(Default)]
pub struct MouseMap {
    pub(super) fleet: Rect,
    pub(super) fleet_offset: usize,
    pub(super) list: Rect,
    pub(super) list_offset: usize,
    pub(super) details: Rect,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardTarget {
    pub ship: String,
    pub home: PathBuf,
}
impl BoardTarget {
    pub fn from_ship(ship: &super::model::Ship) -> Option<Self> {
        Some(Self {
            ship: ship.id.clone(),
            home: std::fs::canonicalize(&ship.live.as_ref()?.home).ok()?,
        })
    }
}
pub enum PendingBoard {
    Onboarding(BoardTarget),
    Liaison(BoardTarget),
}
impl PendingBoard {
    pub fn target(&self) -> &BoardTarget {
        match self {
            Self::Onboarding(t) | Self::Liaison(t) => t,
        }
    }
}

#[derive(Default)]
pub struct App {
    pub attention: Attention,
    pub need_id: Option<Binding>,
    pub mail_id: Option<HistoryBinding>,
    pub history_read: Option<Mail>,
    pub answer: Option<Answer>,
    pub answer_action: Option<Answer>,
    /// The revoke chooser for the selected ship's captain approvals.
    pub revoke: Option<super::approvals::Chooser>,
    /// A revoke chosen, for the worker.
    pub revoke_action: Option<super::approvals::Revoke>,
    pub needs_focus: bool,
    pub inbox_open: bool,
    pub mail: Vec<Mail>,
    pub mail_errors: Vec<String>,
    pub action: Option<super::mailbox::Action>,
    pub mail_pending: bool,
    pub composer: Option<Composer>,
    pub form: Option<super::forms::Form>,
    // Retained library management helpers; the bridge has no launch/watch controls.
    pub board_requested: bool,
    pub watch_requested: bool,
    pub pending_board: Option<PendingBoard>,
    pub snapshot: Snapshot,
    pub herdr: Option<herdr::View>,
    pub home_error: Option<String>,
    pub herdr_error: Option<String>,
    pub home_age: Option<u64>,
    pub herdr_age: Option<u64>,
    pub ship: Option<String>,
    pub tab: usize,
    pub selected: usize,
    pub scroll: u16,
    pub focus_content: bool,
    pub compact: bool,
    pub ascii: bool,
    /// Still voyage scene: water and sprites hold their first frame.
    pub reduced: bool,
    /// The voyage sea replaces the bridge body while open; `Some` since it opened.
    pub voyage: Option<Instant>,
    pub notice: String,
    pub notice_shown: String,
    pub notice_at: Option<Instant>,
    pub herdr_configured: bool,
    pub selection_invalidated: bool,
    pub mouse: MouseMap,
    /// The fleet-wide reading behind the command deck.
    pub deck: super::deck::Deck,
    pub deck_age: Option<u64>,
    /// The fleet tree's selected ship, by machine and name; it may be one
    /// this bridge only lists.
    pub pick: Option<(String, String)>,
    /// Show the observation details in place of the live feed.
    pub diagnostics: bool,
    /// The full-screen view of one need, while open.
    pub focus: Option<super::focus::Focus>,
    /// A reply or dismissal the focus view asked for, for the worker.
    pub focus_send: Option<super::focus::Send>,
    /// Needs answered from here, hidden until the fleet stops listing them.
    pub answered: std::collections::BTreeSet<String>,
    /// The selected row of Needs you, by need id.
    pub need_cursor: Option<String>,
}
impl App {
    pub fn select_ship(&mut self, id: Option<String>) {
        if id
            .as_ref()
            .is_some_and(|id| !self.snapshot.ships.iter().any(|s| &s.id == id))
        {
            return;
        }
        self.ship = id;
        self.pick = None;
        self.focus = None;
        self.inbox_open = false;
        self.needs_focus = false;
        self.selected = 0;
        self.scroll = 0;
        self.tab = 0;
        self.notice.clear();
        self.selection_invalidated = false;
        self.focus_content = false;
    }
    /// Compatibility for library selection helpers; no per-ship tabs are rendered or keyed.
    pub fn select_tab(&mut self, tab: usize) {
        if self.ship.is_some() && tab < 4 {
            self.tab = tab;
            self.selected = 0;
            self.scroll = 0;
            self.selection_invalidated = false;
            self.focus_content = true;
        }
    }
    pub fn tick_notice(&mut self, now: Instant) -> bool {
        if self.notice != self.notice_shown {
            self.notice_shown = self.notice.clone();
            self.notice_at = Some(now);
            return false;
        }
        let waiting = self.form.is_some()
            || self.composer.is_some()
            || self.answer.is_some()
            || self.revoke.is_some()
            || self.mail_pending;
        if self.notice.is_empty()
            || waiting
            || self
                .notice_at
                .is_some_and(|at| now.duration_since(at) < NOTICE_TTL)
        {
            if waiting {
                self.notice_at = Some(now);
            }
            return false;
        }
        self.notice.clear();
        self.notice_shown.clear();
        true
    }
    pub fn ship_index(&self) -> usize {
        self.ship
            .as_ref()
            .and_then(|id| self.snapshot.ships.iter().position(|s| &s.id == id))
            .unwrap_or(0)
    }
    pub fn assignment_indices(&self) -> Vec<usize> {
        let Some(ship) = self.ship.as_deref() else {
            return Vec::new();
        };
        self.snapshot
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.owner == ship || t.handoff_to.as_deref() == Some(ship))
            .map(|(i, _)| i)
            .collect()
    }
    /// The selected ship when it lives on a linked machine: listed, never opened here.
    fn selected_remote(&self) -> Option<&super::model::Ship> {
        self.ship
            .as_ref()
            .and_then(|id| self.snapshot.ships.iter().find(|ship| &ship.id == id))
            .filter(|ship| ship.remote.is_some())
    }
    /// Enter or a click on a remote ship opens nothing; this says why.
    fn explain_remote(&mut self) {
        if let Some((machine, name)) = self.picked_elsewhere() {
            let here = self
                .deck
                .fleet
                .as_ref()
                .is_some_and(|f| &f.machine == machine);
            self.notice = if here {
                format!(
                    "{} has no space recorded in this bridge's homes.",
                    clean(name, 160)
                )
            } else {
                format!(
                    "{} is on {}; nothing opens from here. Open its space on that machine.",
                    clean(name, 160),
                    clean(machine, 160)
                )
            };
            return;
        }
        let Some(ship) = self.selected_remote() else {
            return;
        };
        self.notice = format!(
            "{} is on {}; nothing opens from here. Open its space on that machine.",
            clean(&ship.name, 160),
            clean(ship.remote.as_deref().unwrap_or(""), 160)
        );
    }
    pub fn selected_task(&self) -> Option<&Task> {
        self.assignment_indices()
            .get(self.selected)
            .map(|&i| &self.snapshot.tasks[i])
    }
    fn claims_pane(ship: &super::model::Ship, view: &herdr::View, pane: &herdr::Pane) -> bool {
        ship.terminals.iter().any(|b| b.pane_id == pane.pane_id)
            || ship
                .live
                .as_ref()
                .and_then(|l| l.binding.as_ref())
                .is_some_and(|b| b.matches(view, pane))
    }
    fn binds(ship: &super::model::Ship, view: &herdr::View, pane: &herdr::Pane) -> bool {
        if let Some(binding) = ship
            .live
            .as_ref()
            .and_then(|l| l.binding.as_ref())
            .filter(|b| b.pane.pane_id == pane.pane_id || b.pane.terminal_id == pane.terminal_id)
        {
            return binding.matches(view, pane);
        }
        ship.terminals
            .iter()
            .any(|b| b.pane_id == pane.pane_id && b.terminal_id == pane.terminal_id)
    }
    /// Ships whose own validated Herdr pane reports its agent working, from a
    /// fresh view only. A pane two ships claim counts for neither.
    pub fn working_ships(&self) -> std::collections::BTreeSet<String> {
        let fresh = self.herdr_error.is_none() && self.herdr_age.is_some_and(|a| a < 10);
        let Some(view) = self.herdr.as_ref().filter(|_| fresh) else {
            return Default::default();
        };
        if self.snapshot.presentation_error.is_some() {
            return Default::default();
        }
        let ships = &self.snapshot.ships;
        ships
            .iter()
            .filter(|ship| {
                view.panes.iter().any(|p| {
                    p.agent_status == "working"
                        && Self::binds(ship, view, p)
                        && !ships
                            .iter()
                            .any(|other| other.id != ship.id && Self::claims_pane(other, view, p))
                })
            })
            .map(|ship| ship.id.clone())
            .collect()
    }
    pub fn binding_ambiguous(&self) -> bool {
        if self.snapshot.presentation_error.is_some() {
            return true;
        }
        let (Some(view), Some(ship)) = (
            &self.herdr,
            self.snapshot
                .ships
                .iter()
                .find(|s| Some(&s.id) == self.ship.as_ref()),
        ) else {
            return false;
        };
        view.panes.iter().any(|p| {
            Self::claims_pane(ship, view, p)
                && self
                    .snapshot
                    .ships
                    .iter()
                    .any(|other| other.id != ship.id && Self::claims_pane(other, view, p))
        })
    }
    pub fn scoped_panes(&self) -> Vec<&herdr::Pane> {
        if self.snapshot.presentation_error.is_some() {
            return Vec::new();
        }
        let (Some(view), Some(ship)) = (
            &self.herdr,
            self.snapshot
                .ships
                .iter()
                .find(|s| Some(&s.id) == self.ship.as_ref()),
        ) else {
            return Vec::new();
        };
        let automatic = ship
            .live
            .as_ref()
            .and_then(|l| l.binding.as_ref())
            .and_then(|b| view.panes.iter().find(|p| b.matches(view, p)));
        automatic
            .into_iter()
            .chain(
                ship.terminals
                    .iter()
                    .filter_map(|b| {
                        view.panes
                            .iter()
                            .find(|p| p.pane_id == b.pane_id && p.terminal_id == b.terminal_id)
                    })
                    .filter(|p| automatic.is_none_or(|a| a.pane_id != p.pane_id)),
            )
            .filter(|p| {
                Self::binds(ship, view, p)
                    && !self
                        .snapshot
                        .ships
                        .iter()
                        .any(|other| other.id != ship.id && Self::claims_pane(other, view, p))
            })
            .collect()
    }
    pub fn selected_pane(&self) -> Option<&herdr::Pane> {
        if !matches!(self.tab, 0 | 2) {
            return None;
        }
        self.scoped_panes()
            .get(if self.tab == 0 { 0 } else { self.selected })
            .copied()
    }
    /// Snapshot + full Herdr identity must both be fresh. Does not create or reorder panes.
    pub fn focus_target(&self) -> anyhow::Result<(herdr::View, herdr::Pane)> {
        anyhow::ensure!(
            self.home_error.is_none() && self.home_age.is_some_and(|a| a < 10),
            "Privateer observation unavailable/stale"
        );
        anyhow::ensure!(
            self.snapshot.scope_complete(),
            "Fleet discovery incomplete/unknown; focus refused"
        );
        anyhow::ensure!(
            self.herdr_error.is_none()
                && self.herdr_age.is_some_and(|a| a < 10)
                && !self.selection_invalidated
                && !self.binding_ambiguous(),
            "Herdr observation/selection changed; refresh and reselect"
        );
        let ship = self
            .snapshot
            .ships
            .iter()
            .find(|s| Some(&s.id) == self.ship.as_ref())
            .ok_or_else(|| anyhow::anyhow!("Select a ship"))?;
        anyhow::ensure!(
            ship.live.as_ref().is_none_or(|l| l.error.is_none()),
            "Ship home unavailable"
        );
        Ok((
            self.herdr
                .clone()
                .ok_or_else(|| anyhow::anyhow!("No Herdr connection"))?,
            (*self
                .scoped_panes()
                .first()
                .ok_or_else(|| anyhow::anyhow!("No existing validated mate space; use Herdr"))?)
            .clone(),
        ))
    }
    pub fn selection_len(&self) -> usize {
        if self.inbox_open {
            self.mail.len()
        } else if self.needs_focus {
            self.queue().len()
        } else {
            self.snapshot.ships.len()
        }
    }
    /// Selects a row of Needs you; a need of this machine also keeps its
    /// exact binding, as the answer path requires.
    fn select_need(&mut self, index: usize) {
        let queue = self.queue();
        let item = queue.get(index);
        self.selected = index;
        self.need_cursor = item.map(|i| i.id.clone());
        self.need_id = item
            .and_then(|i| i.local)
            .and_then(|l| self.attention.items().get(l).map(|n| n.binding()));
    }
    pub fn move_selection(&mut self, down: bool) {
        if self.needs_focus && !self.inbox_open {
            let len = self.queue().len();
            let current = self
                .need_cursor
                .as_ref()
                .and_then(|id| self.queue().iter().position(|i| &i.id == id));
            let next = match current {
                Some(c) if down => (c + 1).min(len.saturating_sub(1)),
                Some(c) => c.saturating_sub(1),
                None => 0,
            };
            self.select_need(next);
            self.scroll = 0;
            return;
        }
        if self.inbox_open || self.needs_focus {
            self.selected = if down {
                self.selected
                    .saturating_add(1)
                    .min(self.selection_len().saturating_sub(1))
            } else {
                self.selected
                    .saturating_sub(1)
                    .min(self.selection_len().saturating_sub(1))
            };
            if self.needs_focus {
                self.need_id = self
                    .attention
                    .items()
                    .get(self.selected)
                    .map(|n| n.binding());
            } else {
                self.mail_id = self.mail.get(self.selected).map(Mail::binding);
            }
        } else {
            self.move_in_tree(down);
        }
        self.scroll = 0;
    }
    /// Only an explicitly selected, still-bound history row can become read intent.
    pub fn selected_history(&self) -> Option<&Mail> {
        self.mail
            .get(self.selected)
            .filter(|m| self.inbox_open && self.mail_id.as_ref() == Some(&m.binding()))
    }
    pub fn update_mail(&mut self, messages: Vec<Mail>, errors: Vec<String>) {
        self.mail = messages;
        self.mail_errors = errors;
        if self.inbox_open {
            let index = self.mail_id.as_ref().and_then(|binding| {
                let selected = self.mail.iter().find(|m| &m.binding() == binding)?;
                if selected.origin.is_some()
                    && self.mail.iter().any(|m| {
                        m.origin == selected.origin
                            && (m.home != selected.home || m.identity != selected.identity)
                    })
                {
                    return None;
                }
                let mut matches = self
                    .mail
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| &m.binding() == binding);
                let first = matches.next().map(|(i, _)| i);
                first.filter(|_| matches.next().is_none())
            });
            self.selected = index.unwrap_or(usize::MAX);
            if index.is_none() {
                self.mail_id = None;
                self.scroll = 0;
            }
        }
    }
    pub fn update_attention(&mut self, attention: Attention) {
        self.attention = attention;
        if self.needs_focus {
            // A need is followed by its id; when it leaves the queue nothing
            // else is silently selected in its place.
            let queue = self.queue();
            let index = self
                .need_cursor
                .as_ref()
                .and_then(|id| queue.iter().position(|i| &i.id == id));
            self.selected = index.unwrap_or(usize::MAX);
            if index.is_none() {
                self.need_cursor = None;
            }
            // Never silently bind the replacement when an actionable row disappears.
            let bound = self.need_id.as_ref().and_then(|binding| {
                if self
                    .attention
                    .sources
                    .iter()
                    .filter(|s| s.origin.as_ref() == Some(&binding.origin))
                    .count()
                    != 1
                {
                    return None;
                }
                let items = self.attention.items();
                let mut matches = items
                    .iter()
                    .enumerate()
                    .filter(|(_, n)| &n.binding() == binding);
                let first = matches.next().map(|(i, _)| i);
                first.filter(|_| matches.next().is_none())
            });
            if bound.is_none() {
                self.need_id = None;
                // A need of this machine whose exact binding went is selected
                // again explicitly, never followed to its replacement.
                if index.is_some_and(|i| queue[i].local.is_some()) {
                    self.need_cursor = None;
                    self.selected = usize::MAX;
                }
            }
        }
    }
    pub fn update_snapshot(&mut self, snapshot: Snapshot) {
        let old_home = self
            .snapshot
            .ships
            .iter()
            .find(|s| Some(&s.id) == self.ship.as_ref())
            .and_then(BoardTarget::from_ship);
        let old_identity = self
            .snapshot
            .ships
            .iter()
            .find(|s| Some(&s.id) == self.ship.as_ref())
            .and_then(|s| s.live.as_ref())
            .and_then(|l| l.identity.clone());
        let task_id = self.selected_task().map(|t| (t.home.clone(), t.id.clone()));
        self.snapshot = snapshot;
        if self
            .ship
            .as_ref()
            .is_some_and(|id| !self.snapshot.ships.iter().any(|s| &s.id == id))
        {
            let notice = std::mem::take(&mut self.notice);
            self.select_ship(None);
            self.notice = notice;
        }
        let new_home = self
            .snapshot
            .ships
            .iter()
            .find(|s| Some(&s.id) == self.ship.as_ref())
            .and_then(BoardTarget::from_ship);
        let new_identity = self
            .snapshot
            .ships
            .iter()
            .find(|s| Some(&s.id) == self.ship.as_ref())
            .and_then(|s| s.live.as_ref())
            .and_then(|l| l.identity.clone());
        let identity_changed = old_home != new_home || old_identity != new_identity;
        self.selection_invalidated |= identity_changed;
        if self.tab == 1 {
            self.selected = task_id
                .and_then(|(home, id)| {
                    self.assignment_indices().iter().position(|&i| {
                        self.snapshot.tasks[i].home == home && self.snapshot.tasks[i].id == id
                    })
                })
                .unwrap_or(0);
        }
    }
    pub fn update_herdr(&mut self, view: herdr::View) {
        let old = self.selected_pane().cloned();
        let old_socket = self.herdr.as_ref().map(|v| v.socket_identity);
        self.herdr = Some(view);
        if let Some(old) = old {
            let panes = self.scoped_panes();
            let index = panes.iter().position(|p| p.pane_id == old.pane_id);
            let changed = index.is_none_or(|i| {
                panes[i].terminal_id != old.terminal_id
                    || panes[i].agent != old.agent
                    || panes[i].agent_session != old.agent_session
            }) || old_socket != self.herdr.as_ref().map(|v| v.socket_identity);
            if self.tab == 2 {
                self.selected = index.unwrap_or(0);
            }
            self.selection_invalidated |= changed;
            if changed {
                self.notice = "Terminal changed; select it again before opening.".into();
            }
        }
    }
    pub fn begin_answer(&mut self) {
        if self.mail_pending {
            self.notice = "No live answer available.".into();
            return;
        }
        let Some(need) = self
            .attention
            .items()
            .get(self.selected)
            .copied()
            .filter(|n| {
                self.need_id
                    .as_ref()
                    .is_some_and(|binding| binding == &n.binding())
            })
        else {
            self.notice = "Select an attention item again.".into();
            return;
        };
        if !self.attention.actionable(need) {
            self.notice = "Attention unavailable/stale; refresh before answering.".into();
            return;
        }
        if need.item["kind"] == "blocker" {
            self.notice = "Recorded evidence only; no generic unblock/cancel action.".into();
            return;
        }
        self.answer = Some(Answer::new(need.clone()));
    }
    /// Actual bridge input path, also exercised by disposable-store behavior tests.
    /// Returns true only for an explicit focus request (the worker validates again).
    pub fn bridge_key(&mut self, key: KeyCode) -> bool {
        if self.voyage.is_some() {
            if matches!(key, KeyCode::Char('v') | KeyCode::Esc) {
                self.voyage = None;
            }
            return false;
        }
        if self.answer.is_some() {
            if self.mail_pending {
                return false;
            }
            let answer = self.answer.as_mut().unwrap();
            let choices = answer.need.item["choices"].as_array().map_or(0, Vec::len);
            match key {
                KeyCode::Esc => self.answer = None,
                KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(8),
                KeyCode::PageDown => self.scroll = self.scroll.saturating_add(8),
                KeyCode::Left | KeyCode::Up => answer.choice = answer.choice.saturating_sub(1),
                KeyCode::Right | KeyCode::Down => {
                    answer.choice = (answer.choice + 1).min(choices.saturating_sub(1))
                }
                KeyCode::Char('a') if answer.need.item["kind"] == "approval" => answer.deny = false,
                KeyCode::Char('d') if answer.need.item["kind"] == "approval" => answer.deny = true,
                KeyCode::Enter => match answer.command() {
                    Ok(_) => self.answer_action = Some(answer.clone()),
                    Err(e) => self.notice = e.to_string(),
                },
                KeyCode::Backspace if choices == 0 => {
                    answer.body.pop();
                }
                KeyCode::Char(c)
                    if choices == 0
                        && answer.need.item["kind"] == "decision"
                        && answer.body.chars().count() < 2000
                        && clean(&c.to_string(), 1) == c.to_string() =>
                {
                    answer.body.push(c)
                }
                _ => {}
            }
            return false;
        }
        if self.revoke.is_some() {
            self.revoke_key(key);
            return false;
        }
        if self.focus.is_some() {
            self.focus_key(key);
            return false;
        }
        match key {
            KeyCode::Char('i') => self.open_inbox(),
            KeyCode::Char('d') if !self.inbox_open => {
                self.diagnostics = !self.diagnostics;
                self.scroll = 0;
            }
            KeyCode::Char('x') if self.revocable().is_some() => self.begin_revoke(),
            KeyCode::Char('v')
                if !self.inbox_open && self.composer.is_none() && self.form.is_none() =>
            {
                self.voyage = Some(Instant::now());
            }
            KeyCode::Esc | KeyCode::Home => self.select_ship(None),
            KeyCode::Tab if !self.inbox_open => {
                self.needs_focus = !self.needs_focus;
                self.selected = 0;
                self.scroll = 0;
                if self.needs_focus {
                    self.select_need(0);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(false),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(true),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(8),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(8),
            KeyCode::Enter if self.inbox_open && !self.mail_pending => {
                self.history_read = self.selected_history().cloned();
                if self.history_read.is_none() {
                    self.notice = "Select a history item again.".into();
                }
            }
            KeyCode::Enter | KeyCode::Char('r') if self.needs_focus => {
                match self.need_cursor.clone() {
                    Some(id) => self.open_focus(&id),
                    None => self.notice = "Select a need with ↑↓.".into(),
                }
            }
            // n opens the oldest need from anywhere on the deck.
            KeyCode::Char('n') if !self.inbox_open => match self.queue().first() {
                Some(item) => {
                    let id = item.id.clone();
                    self.open_focus(&id)
                }
                None => self.notice = "Nothing is waiting on you.".into(),
            },
            KeyCode::Enter
                if !self.inbox_open
                    && (self.selected_remote().is_some() || self.picked_elsewhere().is_some()) =>
            {
                self.explain_remote()
            }
            KeyCode::Enter if !self.inbox_open => return true,
            _ => {}
        }
        false
    }
    pub fn handle_mouse(&mut self, event: MouseEvent) {
        if self.voyage.is_some()
            || self.composer.is_some()
            || self.answer.is_some()
            || self.revoke.is_some()
            || self.focus.is_some()
        {
            return;
        }
        let point = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if self.mouse.fleet.contains(point) => {
                let row = self.mouse.fleet_offset + usize::from(event.row - self.mouse.fleet.y);
                let rows = self.tree();
                if let Some(row) = rows
                    .get(row)
                    .filter(|r| matches!(r, super::deck_ui::TreeRow::Ship { .. }))
                {
                    self.select_row(row);
                    self.explain_remote();
                }
            }
            MouseEventKind::Down(MouseButton::Left) if self.mouse.list.contains(point) => {
                let index = self.mouse.list_offset + usize::from(event.row - self.mouse.list.y);
                if index < self.selection_len() || !self.inbox_open {
                    self.selected = index;
                    self.scroll = 0;
                    if !self.inbox_open {
                        self.needs_focus = true;
                        self.select_need(index);
                    } else {
                        self.mail_id = self.mail.get(index).map(Mail::binding);
                    }
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
                if self.mouse.details.contains(point) =>
            {
                self.scroll = if event.kind == MouseEventKind::ScrollDown {
                    self.scroll.saturating_add(3)
                } else {
                    self.scroll.saturating_sub(3)
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                self.move_selection(event.kind == MouseEventKind::ScrollDown)
            }
            _ => {}
        }
    }
    /// A ship's display name from the registry; unregistered ids show as their id.
    pub(super) fn ship_name(&self, id: &str) -> String {
        self.snapshot
            .ships
            .iter()
            .find(|s| s.id == id)
            .map_or_else(|| clean(id, 160), |s| clean(&s.name, 160))
    }
    /// Names a home by the ship observed at it; never shows the home path in lists.
    fn home_label(&self, home: Option<&Path>) -> String {
        let Some(home) = home else {
            return "sample".into();
        };
        self.snapshot
            .ships
            .iter()
            .find(|s| s.live.as_ref().is_some_and(|l| l.home.as_path() == home))
            .map_or_else(|| "unlisted home".into(), |s| clean(&s.name, 160))
    }

    /// The ship's open tasks, owned or offered to it, with custody and origin.
    /// Work delegated from another home is marked, so it is not mistaken for
    /// the ship's own work.
    pub(super) fn push_ship_work(&self, lines: &mut Vec<Line<'static>>, ship: &super::model::Ship) {
        const SHOWN: usize = 20;
        let own_home = ship.live.as_ref().map(|l| l.home.as_path());
        let open: Vec<&Task> = self
            .snapshot
            .tasks
            .iter()
            .filter(|t| {
                (t.owner == ship.id || t.handoff_to.as_deref() == Some(ship.id.as_str()))
                    && !matches!(t.status.as_str(), "done" | "cancelled")
            })
            .collect();
        if open.is_empty() {
            lines.push(Line::from("Open work: none recorded for this ship"));
        } else {
            lines.push(Line::from(format!(
                "Open work: {}",
                count(open.len(), "task", "tasks")
            )));
        }
        for task in open.iter().take(SHOWN) {
            // On a linked machine every task here is delegated to it from this home.
            let delegated = ship.remote.is_some()
                || matches!(
                    (own_home, task.home.as_deref()),
                    (Some(own), Some(home)) if own != home
                );
            lines.push(Line::from(format!(
                "{} · {} · {} · {} · owner {} · origin {}",
                if delegated { "Delegated" } else { "Own" },
                clean(&task.title, 512),
                clean(&task.id, 160),
                clean(&task.status, 40),
                self.ship_name(&task.owner),
                self.home_label(task.home.as_deref()),
            )));
            if let Some(target) = &task.handoff_to {
                lines.push(Line::from(format!(
                    "  offered to {} (not accepted)",
                    self.ship_name(target)
                )));
            }
        }
        if open.len() > SHOWN {
            lines.push(Line::from(format!("+{} more open", open.len() - SHOWN)));
        }
        lines.extend(super::approvals::lines(ship, crate::store::Store::now_ms()));
        if ship.remote.is_some() {
            let pending = self
                .snapshot
                .pending
                .iter()
                .filter(|n| n.owner == ship.id)
                .count();
            let crew = self
                .snapshot
                .jobs
                .iter()
                .filter(|j| j.owner == ship.id && j.is_active())
                .count();
            lines.push(Line::from(format!(
                "Recorded here: {} pending · {} active · its own live state is not shared",
                count(pending, "notice", "notices"),
                count(crew, "job", "jobs"),
            )));
        }
    }

    pub(super) fn need_details(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if !self.notice.is_empty() {
            lines.push(Line::from(clean(&self.notice, 8192)));
        }
        if let Some(chooser) = &self.revoke {
            lines.extend(self.revoke_lines(chooser));
            return lines;
        }
        if let Some(answer) = &self.answer {
            let item = &answer.need.item;
            lines.push(Line::from(if item["kind"] == "approval" {
                format!(
                    "a approve / d deny: {} · Enter commits exact binding · Esc cancel",
                    if answer.deny { "DENY" } else { "APPROVE" }
                )
            } else {
                "←/→ choice or type text · Enter reply · Esc cancel · PgUp/PgDn scroll".into()
            }));
            if let Some(choices) = item["choices"].as_array().filter(|c| !c.is_empty()) {
                lines.push(Line::from(format!(
                    "Choices: {}",
                    choices
                        .iter()
                        .enumerate()
                        .map(|(i, c)| format!(
                            "{}{}",
                            if i == answer.choice { "> " } else { "" },
                            clean(c.as_str().unwrap_or(""), 200)
                        ))
                        .collect::<Vec<_>>()
                        .join(" · ")
                )));
            } else if item["kind"] == "decision" {
                lines.push(Line::from(format!("> {}", clean(&answer.body, 2000))));
            }
            lines.push(Line::from(format!(
                "{} · home {} · ship {} · version {}",
                clean(answer.need.id(), 512),
                clean(&answer.need.home.display().to_string(), 512),
                clean(item["ship"].as_str().unwrap_or("?"), 160),
                clean(item["version"].as_str().unwrap_or("?"), 160)
            )));
            if item["privileged"].is_object() {
                privileged_lines(&mut lines, &item["privileged"]);
                lines.push(Line::from(
                    "a: approve, then confirm at this machine by typing the hash prefix · d: deny",
                ));
            } else if item["kind"] == "approval" {
                lines.push(Line::from(format!(
                    "Artifact: {} · Environment: {}",
                    clean(item["artifact"].as_str().unwrap_or(""), 1024),
                    clean(item["environment"].as_str().unwrap_or(""), 1024)
                )));
            }
            lines.push(Line::from(clean(
                item["question"].as_str().unwrap_or(""),
                2000,
            )));
            lines.push(Line::from(
                "Revalidated before mutation; unknown live/external evidence is not health.",
            ));
            return lines;
        }
        if let Some(ship) = self
            .ship
            .as_ref()
            .and_then(|id| self.snapshot.ships.iter().find(|ship| &ship.id == id))
        {
            lines.push(Line::from(format!(
                "Selected ship: {} · {}",
                clean(&ship.name, 160),
                clean(&ship.id, 160)
            )));
            // The home path stays in this details pane (not the lists) so a
            // same-named ship in another home can still be told apart.
            if let Some(live) = &ship.live {
                lines.push(Line::from(format!(
                    "origin/home {}",
                    clean(&live.home.display().to_string(), 512)
                )));
                if let Some(error) = &live.error {
                    lines.push(Line::from(format!("Unavailable: {}", clean(error, 512))));
                }
            }
            if let Some(machine) = &ship.remote {
                lines.push(Line::from(format!(
                    "Remote on {}: status not shared here, read-only; open it on that machine",
                    clean(machine, 160)
                )));
            }
            lines.push(Line::from(format!(
                "{} scope: {}",
                "Recorded",
                if ship.description.is_empty() {
                    "not observed".into()
                } else {
                    clean(&ship.description, 512)
                }
            )));
            // Work recorded in local homes is shown for a linked-machine ship too.
            self.push_ship_work(&mut lines, ship);
        }
        if let Some(error) = &self.attention.error {
            lines.push(Line::from(format!("UNAVAILABLE: {}", clean(error, 512))));
        }
        if self.binding_ambiguous() {
            lines.push(Line::from(
                "Ambiguous pane ownership; focus disabled. Inspect exact bindings.",
            ));
        }
        if self.attention.sources.is_empty() {
            lines.push(Line::from(
                "Runtime needs not observed; external evidence unknown (not an all-clear).",
            ));
        }
        for source in &self.attention.sources {
            let fresh =
                source.available && crate::store::Store::now_ms() - source.checked_ms < FRESH_MS;
            lines.push(Line::from(format!(
                "{} · origin {} · {}",
                clean(&source.home.display().to_string(), 512),
                clean(source.origin.as_deref().unwrap_or("unknown"), 160),
                if fresh {
                    "recorded observation"
                } else {
                    "UNAVAILABLE / stale; retained history"
                }
            )));
            if let Some(error) = &source.error {
                lines.push(Line::from(clean(error, 512)));
            }
            // One dim line: the first few names, then how many are left.
            let unknown: Vec<_> = source.unknown.iter().map(|s| clean(s, 80)).collect();
            let text = if unknown.is_empty() {
                "Unknown: none listed".into()
            } else {
                let more = unknown.len().saturating_sub(3);
                format!(
                    "Unknown: {}{}",
                    unknown[..unknown.len() - more].join(", "),
                    if more > 0 {
                        format!(" +{more} more")
                    } else {
                        String::new()
                    }
                )
            };
            lines.push(Line::styled(text, Style::default().fg(Color::DarkGray)));
        }
        if let Some(n) = self
            .attention
            .items()
            .get(self.selected)
            .filter(|_| self.needs_focus)
        {
            lines.push(Line::from(format!(
                "{} · ship {} · task {} · version {}",
                clean(n.id(), 512),
                clean(n.item["ship"].as_str().unwrap_or("?"), 160),
                clean(n.item["task_id"].as_str().unwrap_or("none"), 160),
                clean(n.item["version"].as_str().unwrap_or("?"), 160)
            )));
            lines.push(Line::from(clean(
                n.item["question"]
                    .as_str()
                    .or(n.item["summary"].as_str())
                    .unwrap_or(""),
                2000,
            )));
            if n.item["evidence"] == "scheduling_fault" {
                lines.push(Line::from("Scheduling fault: work needs reassessment before another attempt. No automatic model retry."));
            } else if n.item["evidence"] == "stuck_heavy_capacity" {
                lines.push(Line::from("Waiting behind capacity no live process holds. Check that each job below is gone, then recover it:"));
                for held in n.item["blocking"].as_array().into_iter().flatten() {
                    lines.push(Line::from(format!(
                        "{} · {}",
                        clean(held["job_id"].as_str().unwrap_or("?"), 160),
                        clean(held["recover"].as_str().unwrap_or(""), 1024)
                    )));
                }
            } else if n.item["kind"] == "blocker" {
                lines.push(Line::from(format!("Recorded {} · status {} · incarnation {}. No live process proof; no inline mutation.",n.item["evidence"],n.item["status"],n.item["incarnation"])));
            }
            if n.item["privileged"].is_object() {
                privileged_lines(&mut lines, &n.item["privileged"]);
            } else if n.item["kind"] == "approval" {
                lines.push(Line::from(format!(
                    "Exact artifact: {} · environment: {}",
                    clean(n.item["artifact"].as_str().unwrap_or(""), 1024),
                    clean(n.item["environment"].as_str().unwrap_or(""), 1024)
                )));
            }
        }
        lines
    }
    pub fn draw(&mut self, frame: &mut Frame) {
        self.mouse = MouseMap::default();
        let area = frame.area();
        if area.width < 40 || area.height < 14 {
            frame.render_widget(
                Paragraph::new("PRIVATEER · bridge\nResize to at least 40 x 14.\nq: quit"),
                area,
            );
            return;
        }
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .split(area);
        if self.voyage.is_some() {
            frame.render_widget(
                Paragraph::new(
                    " PRIVATEER · voyage · sea drawn from recorded state, not live health",
                )
                .style(Style::default().fg(Color::Yellow)),
                rows[0],
            );
        } else if self.focus.is_some() {
            frame.render_widget(
                Paragraph::new(self.focus_header()).style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                rows[0],
            );
        } else {
            let (header, clock) = self.header_line(crate::store::Store::now_ms());
            frame.render_widget(Paragraph::new(header), rows[0]);
            frame.render_widget(super::deck_ui::clock_widget(clock), rows[0]);
        }
        let list_style = Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD);
        if let Some(started) = self.voyage {
            voyage::draw(
                frame,
                rows[1],
                &self.snapshot,
                &self.working_ships(),
                started.elapsed(),
                self.reduced,
                self.ascii,
            );
        } else if self.focus.is_some() {
            self.draw_focus(frame, rows[1]);
        } else if self.inbox_open {
            let parts = Layout::vertical([
                Constraint::Length((rows[1].height / 3).max(3)),
                Constraint::Min(1),
            ])
            .split(rows[1]);
            let block = Block::bordered().title("Mail history · separate from Needs you");
            self.mouse.list = block.inner(parts[0]);
            let items: Vec<_> = self
                .mail
                .iter()
                .map(|m| {
                    ListItem::new(format!(
                        "{} · {} · {} · {}",
                        clean(&m.from, 160),
                        clean(&m.status, 80),
                        self.home_label(m.home.as_deref()),
                        clean(&m.subject, 160)
                    ))
                })
                .collect();
            let mut state = ListState::default()
                .with_selected((self.selected < items.len()).then_some(self.selected));
            frame.render_stateful_widget(
                List::new(items).block(block).highlight_style(list_style),
                parts[0],
                &mut state,
            );
            self.mouse.list_offset = state.offset();
            self.mouse.details = parts[1];
            let mut lines = self.inbox_lines();
            if !self.notice.is_empty() {
                lines.insert(0, Line::from(clean(&self.notice, 8192)));
            }
            frame.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .scroll((self.scroll, 0)),
                parts[1],
            );
        } else {
            self.draw_deck(frame, rows[1], list_style);
        }
        let errors: Vec<_> = [
            (
                "PRESENTATION UNAVAILABLE",
                self.snapshot.presentation_error.as_ref(),
            ),
            ("STATE UNAVAILABLE", self.home_error.as_ref()),
            ("HERDR UNAVAILABLE", self.herdr_error.as_ref()),
            ("MAILBOX UNAVAILABLE", self.mail_errors.first()),
        ]
        .into_iter()
        .filter_map(|(label, e)| e.map(|e| format!("{label}: {}", clean(e, 256))))
        .collect();
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(if self.voyage.is_some() {
                    "v / Esc: back to bridge · q quit".to_owned()
                } else if self.focus.is_some() {
                    self.focus_keys().to_owned()
                } else if self.inbox_open {
                    "i history · Enter mark read · PgUp/PgDn scroll · Esc bridge · q quit".to_owned()
                } else if self.revoke.is_some() {
                    "↑↓ pick approval · Enter revoke · Esc cancel".to_owned()
                } else {
                    format!(
                        "↑↓ select · Tab Fleet/Needs · Enter open · n next need · d details · {}i history · v voyage · q quit",
                        if self.revocable().is_some() {
                            "x revoke approval · "
                        } else {
                            ""
                        }
                    )
                }),
                Line::styled(
                    if self.mail_pending {
                        "Command pending; no resolution inferred."
                    } else {
                        "Observation only: recorded facts, not live health; unknown is never an all-clear."
                    },
                    Style::default().fg(Color::Cyan),
                ),
                Line::from(format!(
                    "{}{} · State {} · Herdr {}{}{}",
                    if errors.is_empty() {
                        String::new()
                    } else {
                        format!("{} · ", errors.join(" · "))
                    },
                    super::deck_ui::status(&self.deck, self.deck_age),
                    age(self.home_age),
                    age(self.herdr_age),
                    if self.snapshot.truncated {
                        " · recorded history clipped"
                    } else {
                        ""
                    },
                    if !self.snapshot.scope_complete() {
                        " · fleet discovery incomplete/unknown"
                    } else {
                        ""
                    }
                )),
            ]),
            rows[2],
        );
        self.draw_composer(frame);
        self.draw_form(frame);
    }
}
fn age(value: Option<u64>) -> String {
    value.map_or("not connected".into(), |s| format!("{s}s ago"))
}

/// Coarse age of a recorded event, e.g. "just now", "12 min ago", "3 h ago".
pub(super) fn ago(then_ms: i64, now_ms: i64) -> String {
    let minutes = (now_ms - then_ms).max(0) / 60_000;
    match minutes {
        0 => "just now".into(),
        1..=59 => format!("{minutes} min ago"),
        _ => format!("{} h ago", minutes / 60),
    }
}

/// A privileged command as the captain reads it: the argv one per line, as
/// the runtime escaped it, after why and any warnings, then where and the
/// hash they will confirm.
fn privileged_lines(lines: &mut Vec<Line<'static>>, privileged: &serde_json::Value) {
    lines.push(Line::from(format!(
        "Reason: {}",
        clean(privileged["reason"].as_str().unwrap_or(""), 4200)
    )));
    for warning in privileged["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .take(64)
    {
        lines.push(Line::from(clean(warning.as_str().unwrap_or(""), 4400)));
    }
    lines.push(Line::from(format!(
        "Runs as root on {}{}:",
        clean(privileged["machine"].as_str().unwrap_or("?"), 160),
        if privileged["held_elsewhere"] == true {
            " (staged there; approve and run it on that machine)"
        } else {
            ""
        }
    )));
    for arg in privileged["argv"].as_array().into_iter().flatten().take(65) {
        lines.push(Line::from(format!(
            "    {}",
            clean(arg.as_str().unwrap_or(""), 4200)
        )));
    }
    lines.push(Line::from(format!(
        "in {}",
        clean(privileged["cwd"].as_str().unwrap_or(""), 4200)
    )));
    lines.push(Line::from(format!(
        "Hash {}",
        clean(privileged["hash"].as_str().unwrap_or(""), 64)
    )));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_completions_read_as_relative_ages() {
        let now = 10_000_000_000;
        assert_eq!(ago(now, now), "just now");
        assert_eq!(ago(now - 59_999, now), "just now");
        assert_eq!(ago(now - 12 * 60_000, now), "12 min ago");
        assert_eq!(ago(now - 3 * 3_600_000 - 5 * 60_000, now), "3 h ago");
        // A future timestamp (clock skew) never shows a negative age.
        assert_eq!(ago(now + 60_000, now), "just now");
    }
}
