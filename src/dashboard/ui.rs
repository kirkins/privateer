use super::{
    bridge::{Answer, Attention, Binding, FRESH_MS, RECENT_MS},
    captain::{Composer, Exchange, HistoryBinding, Mail},
    herdr,
    model::{Snapshot, Task, clean},
};
use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

pub const NOTICE_TTL: Duration = Duration::from_secs(8);

#[derive(Default)]
pub struct MouseMap {
    fleet: Rect,
    fleet_offset: usize,
    list: Rect,
    list_offset: usize,
    details: Rect,
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
    pub exchanges: Vec<Exchange>,
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
    pub demo: bool,
    pub notice: String,
    pub notice_shown: String,
    pub notice_at: Option<Instant>,
    pub herdr_configured: bool,
    pub selection_invalidated: bool,
    pub mouse: MouseMap,
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
        anyhow::ensure!(!self.demo, "Demo only; no live focus");
        anyhow::ensure!(
            self.home_error.is_none() && self.home_age.is_some_and(|a| a < 10),
            "Privateer observation unavailable/stale"
        );
        anyhow::ensure!(!self.snapshot.truncated, "Fleet scope truncated; focus refused");
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
            self.attention.items().len()
        } else {
            self.snapshot.ships.len()
        }
    }
    pub fn move_selection(&mut self, down: bool) {
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
        } else if !self.snapshot.ships.is_empty() {
            let current = self.ship_index();
            let next = if down && self.ship.is_some() {
                (current + 1).min(self.snapshot.ships.len() - 1)
            } else if down {
                0
            } else {
                current.saturating_sub(1)
            };
            self.select_ship(Some(self.snapshot.ships[next].id.clone()));
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
            // Never silently select the replacement when an actionable row disappears.
            let index = self.need_id.as_ref().and_then(|binding| {
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
            self.selected = index.unwrap_or(usize::MAX);
            if index.is_none() {
                self.need_id = None;
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
        if self.mail_pending || self.demo {
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
        match key {
            KeyCode::Char('i') => self.open_inbox(),
            KeyCode::Esc | KeyCode::Home => self.select_ship(None),
            KeyCode::Tab if !self.inbox_open => {
                self.needs_focus = !self.needs_focus;
                self.selected = 0;
                self.scroll = 0;
                if self.needs_focus {
                    self.need_id = self.attention.items().first().map(|n| n.binding());
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
            KeyCode::Enter | KeyCode::Char('r') if self.needs_focus => self.begin_answer(),
            KeyCode::Enter if !self.inbox_open => return true,
            _ => {}
        }
        false
    }
    pub fn handle_mouse(&mut self, event: MouseEvent) {
        if self.composer.is_some() || self.answer.is_some() {
            return;
        }
        let point = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if self.mouse.fleet.contains(point) => {
                let index = self.mouse.fleet_offset + usize::from(event.row - self.mouse.fleet.y);
                if let Some(ship) = self.snapshot.ships.get(index) {
                    self.select_ship(Some(ship.id.clone()));
                }
            }
            MouseEventKind::Down(MouseButton::Left) if self.mouse.list.contains(point) => {
                let index = self.mouse.list_offset + usize::from(event.row - self.mouse.list.y);
                if index < self.selection_len() || !self.inbox_open {
                    self.selected = index;
                    self.scroll = 0;
                    if !self.inbox_open {
                        self.needs_focus = true;
                        self.need_id = self.attention.items().get(index).map(|n| n.binding());
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
    fn fleet_rows(&self) -> Vec<ListItem<'static>> {
        self.snapshot
            .ships
            .iter()
            .map(|s| {
                let home = s.live.as_ref().map(|l| &l.home);
                let tasks: Vec<_> = self
                    .snapshot
                    .tasks
                    .iter()
                    .filter(|t| {
                        (t.owner == s.id || t.handoff_to.as_ref() == Some(&s.id))
                            && !matches!(t.status.as_str(), "done" | "cancelled")
                    })
                    .collect();
                let work = tasks
                    .iter()
                    .take(3)
                    .map(|t| {
                        format!(
                            "{} {} [{}; owner {}{}; origin/home {}]",
                            clean(&t.id, 160),
                            clean(&t.title, 512),
                            clean(&t.status, 40),
                            clean(&t.owner, 160),
                            t.handoff_to.as_ref().map_or(String::new(), |to| format!(
                                "; offered to {} (not accepted)",
                                clean(to, 160)
                            )),
                            clean(
                                &t.home
                                    .as_ref()
                                    .map_or("sample".into(), |h| h.display().to_string()),
                                512
                            )
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" · ");
                let location = home.map_or("origin/home unobserved".into(), |h| {
                    clean(&h.display().to_string(), 512)
                });
                let state = s.live.as_ref().and_then(|l| l.error.as_ref()).map_or_else(
                    || {
                        if tasks.is_empty() {
                            "no active work in bounded record".into()
                        } else {
                            work
                        }
                    },
                    |e| format!("UNAVAILABLE {}", clean(e, 256)),
                );
                ListItem::new(format!("{} · {} · {}", clean(&s.id, 160), location, state))
            })
            .collect()
    }
    fn need_details(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if !self.notice.is_empty() {
            lines.push(Line::from(clean(&self.notice, 8192)));
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
            if item["kind"] == "approval" {
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
            lines.push(Line::from(format!(
                "Unknown: {}",
                source
                    .unknown
                    .iter()
                    .map(|s| clean(s, 80))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
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
            if n.item["kind"] == "blocker" {
                lines.push(Line::from(format!("Recorded {} · status {} · incarnation {}. No live process proof; no inline mutation.",n.item["evidence"],n.item["status"],n.item["incarnation"])));
            }
            if n.item["kind"] == "approval" {
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
        frame.render_widget(
            Paragraph::new(if self.demo {
                " PRIVATEER · bridge · DEMO / sample only"
            } else {
                " PRIVATEER · bridge · recorded facts, not live health"
            })
            .style(Style::default().fg(Color::Yellow)),
            rows[0],
        );
        let list_style = Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD);
        if self.inbox_open {
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
                        clean(
                            &m.home
                                .as_ref()
                                .map_or("sample".into(), |h| h.display().to_string()),
                            512
                        ),
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
            let parts = Layout::vertical([
                Constraint::Length(
                    (self.snapshot.ships.len() as u16 + 2).min((rows[1].height / 3).max(3)),
                ),
                Constraint::Length(4),
                Constraint::Min(3),
                Constraint::Length((rows[1].height / 5).max(2)),
            ])
            .split(rows[1]);
            let block =
                Block::bordered().title("Fleet · recorded work · Enter: focus existing mate space");
            self.mouse.fleet = block.inner(parts[0]);
            let mut state =
                ListState::default().with_selected(self.ship.as_ref().map(|_| self.ship_index()));
            frame.render_stateful_widget(
                List::new(self.fleet_rows())
                    .block(block)
                    .highlight_style(list_style),
                parts[0],
                &mut state,
            );
            self.mouse.fleet_offset = state.offset();
            let needs = self.attention.items();
            let items: Vec<_> = needs
                .iter()
                .map(|n| {
                    ListItem::new(format!(
                        "{}{} · {} · {}",
                        if n.stale { "STALE " } else { "" },
                        clean(n.item["kind"].as_str().unwrap_or("unknown"), 40),
                        clean(n.item["ship"].as_str().unwrap_or("?"), 160),
                        clean(
                            n.item["question"]
                                .as_str()
                                .or(n.item["summary"].as_str())
                                .unwrap_or(""),
                            2000
                        )
                    ))
                })
                .collect();
            let block = Block::bordered().title(format!(
                "Needs you ({}) · Tab: select · Enter: answer",
                needs.len()
            ));
            self.mouse.list = block.inner(parts[1]);
            let mut state = ListState::default().with_selected(
                (self.needs_focus && self.selected < items.len()).then_some(self.selected),
            );
            frame.render_stateful_widget(
                List::new(items).block(block).highlight_style(list_style),
                parts[1],
                &mut state,
            );
            self.mouse.list_offset = state.offset();
            self.mouse.details = parts[2];
            frame.render_widget(
                Paragraph::new(self.need_details())
                    .wrap(Wrap { trim: false })
                    .scroll((self.scroll, 0)),
                parts[2],
            );
            let now = crate::store::Store::now_ms();
            let recent: Vec<_> = self
                .snapshot
                .recent
                .iter()
                .filter(|r| now >= r.completed_ms && now - r.completed_ms < RECENT_MS)
                .map(|r| {
                    Line::styled(
                        format!(
                            "{} · {} · {} · {} · recorded task completion {}ms",
                            clean(&r.owner, 160),
                            clean(&r.task_id, 160),
                            clean(&r.title, 512),
                            clean(&r.home.display().to_string(), 512),
                            r.completed_ms
                        ),
                        Style::default().fg(if now - r.completed_ms > RECENT_MS / 2 {
                            Color::DarkGray
                        } else {
                            Color::Gray
                        }),
                    )
                })
                .collect();
            frame.render_widget(
                Paragraph::new(if recent.is_empty() {
                    vec![Line::from(
                        "No timestamped completions in the last three hours (bounded evidence).",
                    )]
                } else {
                    recent
                })
                .block(
                    Block::bordered()
                        .title("Recently done · evidenced finished tasks, not inferred merges"),
                )
                .wrap(Wrap { trim: false }),
                parts[3],
            );
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
                Line::from(if self.inbox_open {
                    "i history · Enter mark read · PgUp/PgDn scroll · Esc bridge · q quit"
                } else {
                    "↑↓ select · Tab Fleet/Needs · Enter focus/answer · i history · q quit"
                }),
                Line::styled(
                    if self.mail_pending {
                        "Command pending; no resolution inferred."
                    } else {
                        "Observation only; unknown is never an all-clear."
                    },
                    Style::default().fg(Color::Cyan),
                ),
                Line::from(format!(
                    "{} · State {} · Herdr {}{}",
                    errors.join(" · "),
                    age(self.home_age),
                    age(self.herdr_age),
                    if self.snapshot.truncated {
                        " · bounded snapshot; older evidence unknown"
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
