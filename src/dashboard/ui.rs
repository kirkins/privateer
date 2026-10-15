use super::{
    captain::{Composer, Exchange, Mail},
    herdr,
    model::{Snapshot, Task, clean, count},
    overview,
};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem, ListState, Paragraph, Table, TableState, Tabs, Wrap},
};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const TABS: [&str; 4] = ["1 Bridge", "2 Assignments", "3 Terminals", "4 Logbook"];
/// How long an outcome notice stays visible once nothing is waiting on it.
pub const NOTICE_TTL: Duration = Duration::from_secs(8);

fn origin(home: Option<&Path>) -> String {
    home.map_or_else(String::new, |home| {
        format!(" · Home: {}", clean(&home.display().to_string(), 512))
    })
}

/// The shell-quoted command that reaches a ship's first mate without Herdr.
fn direct_start(ship: &super::model::Ship) -> Option<String> {
    let home = ship
        .live
        .as_ref()
        .filter(|live| live.error.is_none())?
        .home
        .clone();
    let home = std::fs::canonicalize(&home).unwrap_or(home);
    Some(format!(
        "privateer --home '{}' start",
        clean(&home.to_string_lossy(), 4096).replace('\'', "'\"'\"'")
    ))
}

fn border(focused: bool) -> Style {
    Style::default().fg(if focused {
        Color::Yellow
    } else {
        Color::DarkGray
    })
}

fn selected_row() -> Style {
    Style::default()
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD)
}

/// Keeps the last hint (quit) and drops optional hints from the end until the line fits.
fn hint_line(hints: &[(&'static str, &'static str)], width: usize) -> Line<'static> {
    let Some((quit, optional)) = hints.split_last() else {
        return Line::default();
    };
    let mut count = optional.len();
    loop {
        let mut spans = vec![Span::raw(" ")];
        for (i, (key, label)) in optional[..count].iter().chain([quit]).enumerate() {
            if i > 0 {
                spans.push(Span::raw(" · "));
            }
            spans.push(Span::styled(
                *key,
                Style::default().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(format!(" {label}")));
        }
        let line = Line::from(spans);
        if line.width() <= width || count == 0 {
            return line;
        }
        count -= 1;
    }
}

#[derive(Default)]
pub struct MouseMap {
    actions: Vec<(Rect, char)>,
    tabs: Vec<(Rect, usize)>,
    ships: Rect,
    ship_offset: usize,
    list: Rect,
    list_offset: usize,
    fleet: Rect,
    fleet_offset: usize,
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
            Self::Onboarding(target) | Self::Liaison(target) => target,
        }
    }
}

#[derive(Default)]
pub struct App {
    pub inbox_open: bool,
    pub mail: Vec<Mail>,
    pub mail_errors: Vec<String>,
    pub action: Option<super::mailbox::Action>,
    pub mail_pending: bool,
    pub composer: Option<Composer>,
    pub form: Option<super::forms::Form>,
    pub board_requested: bool,
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
        if self.ship != id || self.inbox_open {
            if self.ship.is_none() || id.is_none() {
                self.tab = 0;
            }
            self.notice.clear();
            self.ship = id;
            self.selected = 0;
            self.scroll = 0;
            self.selection_invalidated = false;
        }
        self.inbox_open = false;
        self.focus_content = false;
    }
    pub fn select_tab(&mut self, tab: usize) {
        if self.ship.is_none() {
            self.notice = "Choose a ship to open its workspace.".into();
            return;
        }
        if tab < TABS.len() && self.tab != tab {
            self.tab = tab;
            self.notice.clear();
            self.selected = 0;
            self.scroll = 0;
            self.selection_invalidated = false;
        }
        self.focus_content = true;
    }
    /// Expire an outcome notice after `NOTICE_TTL`, unless a form, draft or
    /// command still depends on it. Returns whether the notice was cleared.
    pub fn tick_notice(&mut self, now: Instant) -> bool {
        if self.notice != self.notice_shown {
            self.notice_shown = self.notice.clone();
            self.notice_at = Some(now);
            return false;
        }
        let waiting = self.form.is_some() || self.composer.is_some() || self.mail_pending;
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
            .map_or(usize::from(self.inbox_open), |i| i + 2)
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
            .and_then(|live| live.binding.as_ref())
            .filter(|binding| {
                binding.pane.pane_id == pane.pane_id || binding.pane.terminal_id == pane.terminal_id
            })
        {
            return binding.matches(view, pane);
        }
        ship.terminals.iter().any(|binding| {
            binding.pane_id == pane.pane_id && binding.terminal_id == pane.terminal_id
        })
    }
    pub fn binding_ambiguous(&self) -> bool {
        if self.snapshot.presentation_error.is_some() {
            return true;
        }
        let Some(view) = &self.herdr else {
            return false;
        };
        let Some(ship) = self
            .snapshot
            .ships
            .iter()
            .find(|s| Some(&s.id) == self.ship.as_ref())
        else {
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
        let Some(ship) = self
            .snapshot
            .ships
            .iter()
            .find(|s| Some(&s.id) == self.ship.as_ref())
        else {
            return Vec::new();
        };
        let Some(view) = &self.herdr else {
            return Vec::new();
        };
        // Live presence is primary; manual bindings retain registry order.
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
    pub fn selection_len(&self) -> usize {
        if self.inbox_open {
            return self.mail.len();
        }
        match self.tab {
            1 => self.assignment_indices().len(),
            2 => self.scoped_panes().len(),
            _ => 0,
        }
    }
    pub fn move_selection(&mut self, down: bool) {
        if !self.focus_content {
            let current = self.ship_index();
            let max = self.snapshot.ships.len() + 1;
            let next = if down {
                (current + 1).min(max)
            } else {
                current.saturating_sub(1)
            };
            self.select_navigation(next);
            self.focus_content = false;
        } else if self.selection_len() > 0 {
            self.selected = if down {
                (self.selected + 1).min(self.selection_len() - 1)
            } else {
                self.selected.saturating_sub(1)
            };
            self.selection_invalidated = false;
            self.scroll = 0;
        } else {
            self.scroll = if down {
                self.scroll.saturating_add(1)
            } else {
                self.scroll.saturating_sub(1)
            };
        }
    }
    fn select_navigation(&mut self, index: usize) {
        match index {
            0 => self.select_ship(None),
            1 => self.open_inbox(),
            _ => {
                if let Some(ship) = self.snapshot.ships.get(index - 2) {
                    self.select_ship(Some(ship.id.clone()));
                }
            }
        }
    }
    pub fn update_mail(&mut self, messages: Vec<Mail>, errors: Vec<String>) {
        let selected = self
            .mail
            .get(self.selected)
            .map(|m| (m.home.clone(), m.id.clone()));
        self.mail = messages;
        self.mail_errors = errors;
        if self.inbox_open {
            let index = selected
                .and_then(|(home, id)| self.mail.iter().position(|m| m.home == home && m.id == id));
            // A message that left the inbox (read or answered) yields to the
            // one that took its place, not to the top of the list.
            self.selected = index.unwrap_or(self.selected.min(self.mail.len().saturating_sub(1)));
            if index.is_none() {
                self.scroll = 0;
            }
        }
    }
    pub fn update_snapshot(&mut self, snapshot: Snapshot) {
        let task_id = self.selected_task().map(|t| (t.home.clone(), t.id.clone()));
        self.snapshot = snapshot;
        if self
            .ship
            .as_ref()
            .is_some_and(|id| !self.snapshot.ships.iter().any(|s| &s.id == id))
        {
            // A refresh is not navigation: keep the outcome that removed the ship.
            let notice = std::mem::take(&mut self.notice);
            self.select_ship(None);
            self.notice = notice;
        }
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
            self.selected = index.unwrap_or(0);
            self.selection_invalidated |= changed;
            if changed {
                self.notice = "Terminal changed; select it again before opening.".into();
            }
        }
    }
    pub fn handle_mouse(&mut self, event: MouseEvent) {
        if self.composer.is_some() {
            return;
        }
        let point = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, action)) =
                    self.mouse.actions.iter().find(|(r, _)| r.contains(point))
                {
                    match action {
                        'i' => self.open_inbox(),
                        'l' => self.talk_to_liaison(),
                        'm' => self.compose(),
                        'b' => self.board_requested = true,
                        'n' => self.manage(super::forms::Kind::New),
                        'x' => self.manage(super::forms::Kind::Retire),
                        'd' => self.manage(super::forms::Kind::Dispatcher),
                        _ => {}
                    }
                } else if self.mouse.ships.contains(point) {
                    let index =
                        self.mouse.ship_offset + usize::from((event.row - self.mouse.ships.y) / 2);
                    if index <= self.snapshot.ships.len() + 1 {
                        self.select_navigation(index);
                    }
                } else if self.mouse.fleet.contains(point) {
                    let index =
                        self.mouse.fleet_offset + usize::from(event.row - self.mouse.fleet.y);
                    if index < self.snapshot.ships.len() {
                        self.select_navigation(index + 2);
                    }
                } else if let Some((_, tab)) =
                    self.mouse.tabs.iter().find(|(r, _)| r.contains(point))
                {
                    self.select_tab(*tab);
                } else if self.mouse.list.contains(point) {
                    let index =
                        self.mouse.list_offset + usize::from((event.row - self.mouse.list.y) / 2);
                    if index < self.selection_len() {
                        self.selected = index;
                        self.focus_content = true;
                        self.scroll = 0;
                        self.selection_invalidated = false;
                    }
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let down = event.kind == MouseEventKind::ScrollDown;
                if self.mouse.ships.contains(point) {
                    self.focus_content = false;
                    self.move_selection(down);
                } else if self.mouse.list.contains(point) {
                    self.focus_content = true;
                    self.move_selection(down);
                } else if self.mouse.details.contains(point) {
                    self.scroll = if down {
                        self.scroll.saturating_add(3)
                    } else {
                        self.scroll.saturating_sub(3)
                    };
                }
            }
            _ => {}
        }
    }
    fn pending_lines(&self) -> Vec<Line<'static>> {
        let notices = self
            .snapshot
            .pending
            .iter()
            .filter(|n| self.ship.as_ref().is_none_or(|id| &n.owner == id))
            .collect::<Vec<_>>();
        let mut lines = vec![Line::styled(
            format!(
                "{} {}",
                if self.ascii { "!" } else { "⚓" },
                count(notices.len(), "pending issue", "pending issues")
            ),
            Style::default().fg(Color::Yellow),
        )];
        for n in notices {
            lines.push(Line::from(format!(
                "{}{} / {} [#{}]{}",
                if self.ship.is_none() {
                    format!("{} / ", clean(&n.owner, 160))
                } else {
                    String::new()
                },
                clean(&n.task_id, 160),
                clean(&n.issue_key, 160),
                n.seq,
                origin(n.home.as_deref())
            )));
            lines.push(Line::from(clean(&n.summary, 2048)));
        }
        lines
    }
    fn details(&self) -> Vec<Line<'static>> {
        if self.inbox_open {
            return self.inbox_lines();
        }
        let Some(ship) = self
            .snapshot
            .ships
            .iter()
            .find(|s| Some(&s.id) == self.ship.as_ref())
        else {
            let mut lines = vec![
                Line::from(format!(
                    "{} · {} · {}",
                    count(self.snapshot.ships.len(), "ship", "ships"),
                    count(self.snapshot.tasks.len(), "assignment", "assignments"),
                    count(
                        self.snapshot.pending.len(),
                        "pending issue",
                        "pending issues"
                    )
                )),
                Line::from(if self.snapshot.ships.is_empty() && !self.demo {
                    "No ships yet · n: create your first ship (a liaison is suggested)"
                } else if !overview::observed(&self.snapshot) {
                    "Select a ship to enter its workspace."
                } else if self.ascii {
                    "Select a ship to enter its workspace; * aboard · o not aboard · x unavailable"
                } else {
                    "Select a ship to enter its workspace; ● aboard · ○ not aboard · ✕ unavailable"
                }),
            ];
            if let Some(liaison) = self.snapshot.ships.iter().find(|s| s.liaison) {
                let name = clean(&liaison.name, 160);
                lines.push(Line::styled(
                    if self.demo {
                        format!("l: talk to {name} · launch is not connected in this preview")
                    } else if let Some(start) = direct_start(liaison).filter(|_| !self.herdr_configured) {
                        format!("Talk to {name} directly: {start}")
                    } else {
                        format!("l: talk to {name} · opens its first mate in a Herdr tab; b boards any ship")
                    },
                    Style::default().fg(Color::Cyan),
                ));
            }
            lines.push(Line::from(""));
            lines.push(Line::styled(
                "Captain's inbox",
                Style::default().fg(Color::Cyan),
            ));
            {
                for mail in self.mail.iter().take(3) {
                    lines.push(Line::from(format!(
                        "{} · {} · {}",
                        clean(&mail.kind, 40),
                        clean(&mail.from, 80),
                        clean(&mail.subject, 160)
                    )));
                }
                lines.push(Line::from(if self.demo {
                    "Sample messages · i to inspect; no live mailbox connected"
                } else {
                    "i: inspect mail · Enter: mark read · r: reply"
                }));
                for error in &self.mail_errors {
                    lines.push(Line::from(format!(
                        "Mailbox unavailable: {}",
                        clean(error, 256)
                    )));
                }
            }
            lines.push(Line::from(""));
            for ship in &self.snapshot.ships {
                if let Some(error) = ship.live.as_ref().and_then(|l| l.error.as_ref()) {
                    lines.push(Line::styled(
                        format!(
                            "{} unavailable: {}",
                            clean(&ship.name, 160),
                            clean(error, 512)
                        ),
                        Style::default().fg(Color::Red),
                    ));
                }
            }
            lines.extend(self.pending_lines());
            return lines;
        };
        let mut lines = Vec::new();
        match self.tab {
            0 => {
                lines.push(Line::styled(
                    clean(&ship.name, 160),
                    Style::default().fg(Color::Yellow),
                ));
                lines.push(Line::from(clean(&ship.description, 512)));
                if let Some(live) = &ship.live {
                    lines.extend(super::fleet::lines(live));
                }
                lines.push(Line::from(if ship.liaison {
                    "Liaison · your default contact; a peer ship, not a commander."
                } else {
                    "Independent peer · first mate and crew work within this ship."
                }));
                let owned = self
                    .snapshot
                    .tasks
                    .iter()
                    .filter(|t| t.owner == ship.id)
                    .count();
                let incoming = self
                    .snapshot
                    .tasks
                    .iter()
                    .filter(|t| t.handoff_to.as_ref() == Some(&ship.id))
                    .count();
                lines.push(Line::from(format!(
                    "{} · {}",
                    count(owned, "owned assignment", "owned assignments"),
                    count(incoming, "incoming handoff", "incoming handoffs")
                )));
                lines.push(Line::from(""));
                if self.binding_ambiguous() {
                    lines.push(Line::styled(
                        "Ambiguous pane ownership; Board/focus disabled. Inspect exact bindings.",
                        Style::default().fg(Color::Red),
                    ));
                }
                lines.push(Line::from(if self.demo {
                    "Conversation and agent launch are not connected in this preview.".into()
                } else if let Some(start) = direct_start(ship).filter(|_| !self.herdr_configured) {
                    format!("Board needs Herdr. Talk to its first mate directly: {start}")
                } else if self.scoped_panes().is_empty() {
                    "b: Board · launch its first mate in a new Herdr tab".into()
                } else {
                    "b or f: open its first mate's terminal · 3: Terminals for its crew".into()
                }));

                lines.push(Line::styled(
                    if self.demo {
                        "m: Try Status / Ask / Message · local demo only"
                    } else {
                        "m: Status / Ask / Message · delivered through the ship's mailbox"
                    },
                    Style::default().fg(Color::Cyan),
                ));
                for exchange in self.exchanges.iter().filter(|e| e.ship == ship.id) {
                    lines.push(Line::from(format!(
                        "Captain [{}]: {}",
                        exchange.mode.label(),
                        clean(&exchange.body, 1024)
                    )));
                    lines.push(Line::from(clean(&exchange.response, 1024)));
                }
                lines.push(Line::from(""));
                lines.extend(self.pending_lines());
                if !self.compact {
                    for job in self.snapshot.jobs.iter().filter(|j| j.owner == ship.id) {
                        lines.push(Line::from(format!(
                            "Job {} · {}{}",
                            clean(&job.id, 160),
                            clean(&job.status, 40),
                            origin(job.home.as_deref())
                        )));
                    }
                }
            }
            1 => {
                if let Some(task) = self.selected_task() {
                    lines.push(Line::styled(
                        clean(&task.title, 512),
                        Style::default().fg(Color::Yellow),
                    ));
                    lines.push(Line::from(format!(
                        "Task: {} · Owner: {} · {}{}",
                        clean(&task.id, 160),
                        clean(&task.owner, 160),
                        clean(&task.status, 40),
                        origin(task.home.as_deref())
                    )));
                    if let Some(target) = &task.handoff_to {
                        lines.push(Line::from(format!(
                            "Offered to {} · awaiting acceptance; {} still owns it",
                            clean(target, 160),
                            clean(&task.owner, 160)
                        )));
                    }
                    for job in self
                        .snapshot
                        .jobs
                        .iter()
                        .filter(|j| j.home == task.home && j.task_id == task.id)
                    {
                        lines.push(Line::from(format!(
                            "Job {} · {}{}",
                            clean(&job.id, 160),
                            clean(&job.status, 40),
                            origin(job.home.as_deref())
                        )));
                    }
                } else {
                    lines.push(Line::from(
                        "No assignments or incoming handoffs for this ship.",
                    ));
                }
            }
            2 => {
                if let Some(p) = self.selected_pane() {
                    lines.push(Line::from(format!(
                        "{} · {}",
                        clean(p.agent.as_deref().unwrap_or("shell"), 80),
                        clean(&p.agent_status, 40)
                    )));
                    lines.push(Line::from(format!(
                        "Pane: {} · Terminal: {}",
                        clean(&p.pane_id, 160),
                        clean(&p.terminal_id, 160)
                    )));
                    lines.push(Line::from(clean(p.title.as_deref().unwrap_or(""), 256)));
                    lines.push(Line::from(if self.demo {
                        "Sample terminal only; no live agent is running."
                    } else {
                        "Enter/f: open this agent in Herdr. Use Herdr navigation to return."
                    }));
                } else {
                    lines.push(Line::from("No connected terminals bound to this ship."));
                }
                lines.push(Line::from(
                    "Terminal state does not verify assignment completion.",
                ));
            }
            _ => {
                lines.push(Line::from("Assignment events (newest first):"));
                for n in self.snapshot.activity.iter().filter(|n| n.owner == ship.id) {
                    lines.push(Line::styled(
                        format!(
                            "#{} {} · {}{}",
                            n.seq,
                            clean(&n.task_id, 160),
                            clean(&n.kind, 80),
                            origin(n.home.as_deref())
                        ),
                        Style::default().fg(Color::Cyan),
                    ));
                    lines.push(Line::from(clean(&n.summary, 2048)));
                }
                lines.push(Line::from(""));
                lines.push(Line::from("Custody history:"));
                for c in self
                    .snapshot
                    .custody
                    .iter()
                    .filter(|c| c.actor == ship.id || c.owner == ship.id)
                {
                    lines.push(Line::from(format!(
                        "{} · {} {} · current owner {}{}",
                        clean(&c.task_id, 160),
                        clean(&c.actor, 160),
                        if c.kind == "task.accept" {
                            "accepted responsibility"
                        } else {
                            "offered a handoff"
                        },
                        clean(&c.owner, 160),
                        origin(c.home.as_deref())
                    )));
                }
            }
        }
        lines
    }
    /// Priority order; the final hint (quit) is always kept.
    fn footer_hints(&self) -> Vec<(&'static str, &'static str)> {
        let mut hints = if self.inbox_open {
            vec![
                ("Enter", "mark read"),
                ("r", "reply"),
                ("↑↓", "select"),
                ("PgUp/PgDn", "scroll"),
                ("0", "home"),
            ]
        } else if self.ship.is_none() {
            let mut hints = Vec::new();
            if self.snapshot.ships.iter().any(|s| s.liaison) {
                hints.push(("l", "talk to liaison"));
            }
            hints.extend([("↑↓", "select ship"), ("i", "inbox")]);
            if !self.demo {
                hints.push(("n", "new ship"));
            }
            hints
        } else {
            match self.tab {
                0 => vec![
                    ("b", "board"),
                    ("m", "message"),
                    ("f", "open terminal"),
                    ("1–4", "views"),
                    ("d", "dispatcher"),
                    ("x", "retire"),
                    ("c", "details"),
                    ("0", "home"),
                ],
                1 => vec![
                    ("↑↓", "select"),
                    ("1–4", "views"),
                    ("m", "message"),
                    ("b", "board"),
                    ("0", "home"),
                ],
                2 => vec![
                    ("Enter", "open terminal"),
                    ("↑↓", "select"),
                    ("1–4", "views"),
                    ("b", "board"),
                    ("0", "home"),
                ],
                _ => vec![
                    ("PgUp/PgDn", "scroll"),
                    ("1–4", "views"),
                    ("m", "message"),
                    ("0", "home"),
                ],
            }
        };
        if self.demo {
            hints.retain(|(key, _)| !matches!(*key, "n" | "x" | "d"));
        }
        hints.push(("q", "quit"));
        hints
    }
    /// Observation health only; outcome notices render in the details pane.
    fn status_text(&self) -> String {
        format!(
            "State {} · Herdr {}",
            age(self.home_age),
            age(self.herdr_age)
        )
    }
    pub fn draw(&mut self, frame: &mut Frame) {
        self.mouse = MouseMap::default();
        let area = frame.area();
        if area.width < 50 || area.height < 14 {
            frame.render_widget(
                Paragraph::new("PRIVATEER\nResize to at least 50 x 14.\nq: quit"),
                area,
            );
            return;
        }
        let rows = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(area);
        let scope = self
            .ship
            .as_ref()
            .and_then(|id| self.snapshot.ships.iter().find(|s| &s.id == id))
            .map_or(
                if self.inbox_open {
                    "Captain / Inbox"
                } else {
                    "Fleet overview"
                }
                .into(),
                |s| format!("{} / {}", clean(&s.name, 160), TABS[self.tab]),
            );
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(
                        " PRIVATEER ",
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(if self.demo {
                        " DEMO / sample data"
                    } else {
                        " LIVE / fleet · captain mail"
                    }),
                ]),
                Line::from(format!(" {scope}")),
            ]),
            rows[0],
        );
        let cols = Layout::horizontal([Constraint::Percentage(27), Constraint::Percentage(73)])
            .split(rows[1]);
        let mut items = vec![
            ListItem::new(vec![
                Line::from("Fleet overview"),
                Line::from(count(self.snapshot.ships.len(), "ship", "ships")),
            ]),
            ListItem::new(vec![
                Line::from("Captain's inbox"),
                Line::from(overview::inbox(&self.mail, self.mail_errors.len())),
            ]),
        ];
        for s in &self.snapshot.ships {
            items.push(ListItem::new(vec![
                Line::from(vec![
                    overview::presence(s, self.ascii),
                    Span::raw(" "),
                    Span::raw(clean(&s.name, 160)),
                ]),
                Line::from(format!("  {}", overview::summary(s, &self.snapshot))),
            ]));
        }
        let block = Block::bordered()
            .title("Navigation")
            .border_style(border(!self.focus_content));
        self.mouse.ships = block.inner(cols[0]);
        self.mouse.ships.height -= self.mouse.ships.height % 2;
        let mut state = ListState::default().with_selected(Some(self.ship_index()));
        frame.render_stateful_widget(
            List::new(items)
                .block(block)
                .highlight_symbol("> ")
                .highlight_style(selected_row()),
            cols[0],
            &mut state,
        );
        self.mouse.ship_offset = state.offset();
        let mut body = cols[1];
        let sections = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(body);
        let mut actions = vec![("[i Inbox]", 'i')];
        if self.snapshot.ships.iter().any(|s| s.liaison) {
            actions.push(("[l Talk to liaison]", 'l'));
        }
        if !self.demo {
            actions.push(("[n New ship]", 'n'));
        }
        if self.ship.is_some() {
            actions.push(("[b Board]", 'b'));
            actions.push((
                if self.demo {
                    "[m Message ship]"
                } else {
                    "[m Message]"
                },
                'm',
            ));
            if !self.demo {
                actions.push(("[x Retire]", 'x'));
                actions.push(("[d Dispatcher]", 'd'));
            }
        }
        let mut x = sections[0].x;
        let mut spans = Vec::new();
        for (label, action) in actions {
            let width = (label.len() as u16).min(sections[0].right().saturating_sub(x));
            if width == 0 {
                break;
            }
            self.mouse
                .actions
                .push((Rect::new(x, sections[0].y, width, 1), action));
            spans.push(Span::styled(label, Style::default().fg(Color::Cyan)));
            spans.push(Span::raw(" "));
            x = x.saturating_add(width + 1);
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), sections[0]);
        body = sections[1];
        if self.ship.is_some() {
            let sections =
                Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(body);
            let row = sections[0];
            let mut x = row.x;
            for (i, label) in TABS.iter().enumerate() {
                let width =
                    (Line::from(*label).width() as u16 + 2).min(row.right().saturating_sub(x));
                if width == 0 {
                    break;
                }
                self.mouse.tabs.push((Rect::new(x, row.y, width, 1), i));
                x = x.saturating_add(width + 1);
            }
            frame.render_widget(
                Tabs::new(TABS)
                    .padding(" ", " ")
                    .divider("│")
                    .select(self.tab)
                    .highlight_style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD | Modifier::REVERSED),
                    ),
                row,
            );
            body = sections[1];
        }
        let title = if self.inbox_open {
            "Captain's inbox · message details"
        } else if self.ship.is_none() {
            "Fleet overview"
        } else {
            match self.tab {
                0 => "Bridge",
                1 => "Assignment details",
                2 => "Terminal details",
                _ => "Ship logbook",
            }
        };
        let has_list = self.inbox_open || (self.ship.is_some() && matches!(self.tab, 1 | 2));
        if !self.inbox_open && self.ship.is_none() && !self.snapshot.ships.is_empty() {
            let rows = self.snapshot.ships.len() as u16;
            let parts = Layout::vertical([
                Constraint::Length((rows + 3).min((body.height / 2).max(5))),
                Constraint::Min(1),
            ])
            .split(body);
            let block = Block::bordered().title("Ships").border_style(border(false));
            let inner = block.inner(parts[0]);
            let (header, rows, widths) = overview::table(&self.snapshot, inner.width, self.ascii);
            let mut state = TableState::default();
            frame.render_stateful_widget(
                Table::new(rows, widths).header(header).block(block),
                parts[0],
                &mut state,
            );
            self.mouse.fleet = Rect::new(
                inner.x,
                inner.y.saturating_add(1),
                inner.width,
                inner.height.saturating_sub(1),
            );
            self.mouse.fleet_offset = state.offset();
            body = parts[1];
        }
        if has_list {
            let parts = Layout::vertical([
                Constraint::Length((body.height / 2).max(4)),
                Constraint::Min(1),
            ])
            .split(body);
            let entries = if self.inbox_open {
                {
                    self.mail
                        .iter()
                        .map(|m| {
                            ListItem::new(vec![
                                Line::from(format!(
                                    "{} · {}",
                                    clean(&m.kind, 40),
                                    clean(&m.subject, 160)
                                )),
                                Line::from(format!(
                                    "{} · {}",
                                    clean(&m.from, 80),
                                    clean(&m.status, 80)
                                )),
                            ])
                        })
                        .collect::<Vec<_>>()
                }
            } else if self.tab == 1 {
                self.assignment_indices()
                    .iter()
                    .map(|&i| {
                        let t = &self.snapshot.tasks[i];
                        let role = if Some(&t.owner) == self.ship.as_ref() {
                            "Owned"
                        } else {
                            "Incoming"
                        };
                        ListItem::new(vec![
                            Line::from(clean(&t.title, 160)),
                            Line::from(format!(
                                "{role} · {}{}",
                                clean(&t.status, 40),
                                origin(t.home.as_deref())
                            )),
                        ])
                    })
                    .collect::<Vec<_>>()
            } else {
                self.scoped_panes()
                    .iter()
                    .map(|p| {
                        ListItem::new(vec![
                            Line::from(clean(p.title.as_deref().unwrap_or(&p.pane_id), 160)),
                            Line::from(format!(
                                "{} · {}",
                                clean(p.agent.as_deref().unwrap_or("shell"), 80),
                                clean(&p.agent_status, 40)
                            )),
                        ])
                    })
                    .collect::<Vec<_>>()
            };
            let block = Block::bordered()
                .title(if self.inbox_open {
                    "For the captain · decisions / replies / notices"
                } else if self.tab == 1 {
                    "This ship's assignments / incoming handoffs"
                } else {
                    "This ship's terminals"
                })
                .border_style(border(self.focus_content));
            self.mouse.list = block.inner(parts[0]);
            self.mouse.list.height -= self.mouse.list.height % 2;
            let mut state =
                ListState::default().with_selected((!entries.is_empty()).then_some(self.selected));
            frame.render_stateful_widget(
                List::new(entries)
                    .block(block)
                    .highlight_symbol("> ")
                    .highlight_style(selected_row()),
                parts[0],
                &mut state,
            );
            self.mouse.list_offset = state.offset();
            body = parts[1];
        }
        let mut lines = self.details();
        if !self.notice.is_empty() {
            lines.insert(
                0,
                Line::styled(
                    clean(&self.notice, 8192),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            );
            lines.insert(1, Line::from(""));
        }
        if self.snapshot.truncated {
            lines.push(Line::styled(
                "Bounded snapshot; older records remain in the runtime.",
                Style::default().fg(Color::Yellow),
            ));
        }
        let block = Block::bordered()
            .title(title)
            .border_style(border(self.focus_content && !has_list));
        self.mouse.details = block.inner(body);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0))
                .block(block),
            body,
        );
        let error = self
            .snapshot
            .presentation_error
            .as_ref()
            .map(|e| format!("PRESENTATION UNAVAILABLE: {}", clean(e, 180)))
            .or_else(|| {
                self.home_error
                    .as_ref()
                    .map(|e| format!("STATE UNAVAILABLE: {}", clean(e, 180)))
            })
            .or_else(|| {
                self.herdr_error
                    .as_ref()
                    .map(|e| format!("HERDR UNAVAILABLE: {}", clean(e, 180)))
            })
            .or_else(|| {
                self.mail_errors
                    .first()
                    .map(|e| format!("MAILBOX UNAVAILABLE: {}", clean(e, 180)))
            });
        let failed = self.snapshot.presentation_error.is_some()
            || self.home_error.is_some()
            || self.herdr_error.is_some()
            || !self.mail_errors.is_empty();
        let status = error.unwrap_or_else(|| self.status_text());
        frame.render_widget(
            Paragraph::new(vec![
                hint_line(&self.footer_hints(), usize::from(rows[2].width)),
                Line::styled(
                    status,
                    Style::default().fg(if failed { Color::Red } else { Color::Gray }),
                ),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn footer_hints_drop_trailing_hints_but_keep_quit() {
        let app = App {
            ship: Some("apps".into()),
            tab: 0,
            ..App::default()
        };
        let line = hint_line(&app.footer_hints(), 50);
        let shown = text(&line);
        assert!(line.width() <= 50, "{shown:?}");
        assert!(shown.ends_with("q quit"), "{shown:?}");
        assert!(shown.contains("b board") && shown.contains("m message"));
        assert!(!shown.contains("0 home"), "{shown:?}");
        assert!(!shown.contains("1–4 views"), "{shown:?}");
        assert!(shown.contains("f open terminal"), "{shown:?}");
        let wide = text(&hint_line(&app.footer_hints(), 200));
        assert!(
            wide.contains("0 home") && wide.ends_with("q quit"),
            "{wide:?}"
        );
    }

    #[test]
    fn demo_footer_omits_disabled_management_keys() {
        let app = App {
            ship: Some("apps".into()),
            tab: 0,
            demo: true,
            ..App::default()
        };
        let keys: Vec<_> = app.footer_hints().iter().map(|(k, _)| *k).collect();
        assert!(
            !keys.iter().any(|k| matches!(*k, "n" | "x" | "d")),
            "{keys:?}"
        );
        assert_eq!(keys.last(), Some(&"q"));
    }

    #[test]
    fn status_line_reports_health_without_a_trailing_separator() {
        let app = App {
            notice: "Recording read…".into(),
            ..App::default()
        };
        assert_eq!(
            app.status_text(),
            "State not connected · Herdr not connected"
        );
    }
}
