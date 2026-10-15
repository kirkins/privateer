use super::{
    captain::{Composer, Exchange, Mail},
    herdr,
    model::{Snapshot, Task, clean},
};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};
use std::path::Path;

const TABS: [&str; 4] = ["1 Bridge", "2 Assignments", "3 Terminals", "4 Logbook"];

fn origin(home: Option<&Path>) -> String {
    home.map_or_else(String::new, |home| {
        format!(" · Home: {}", clean(&home.display().to_string(), 512))
    })
}

#[derive(Default)]
pub struct MouseMap {
    actions: Vec<(Rect, char)>,
    tabs: Vec<(Rect, usize)>,
    ships: Rect,
    ship_offset: usize,
    list: Rect,
    list_offset: usize,
    details: Rect,
}

#[derive(Default)]
pub struct App {
    pub inbox_open: bool,
    pub mail: Vec<Mail>,
    pub mail_errors: Vec<String>,
    pub action: Option<super::mailbox::Action>,
    pub mail_pending: bool,
    pub composer: Option<Composer>,
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
            self.selected = 0;
            self.scroll = 0;
            self.selection_invalidated = false;
        }
        self.focus_content = true;
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
    pub fn scoped_panes(&self) -> Vec<&herdr::Pane> {
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
        // Registry order makes the first binding the ship's primary terminal.
        ship.terminals
            .iter()
            .filter_map(|b| {
                view.panes
                    .iter()
                    .find(|p| p.pane_id == b.pane_id && p.terminal_id == b.terminal_id)
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
            self.selected = index.unwrap_or(0);
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
            self.select_ship(None);
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
                        'l' => self.open_liaison(),
                        'm' => self.compose(),
                        _ => {}
                    }
                } else if self.mouse.ships.contains(point) {
                    let index =
                        self.mouse.ship_offset + usize::from((event.row - self.mouse.ships.y) / 2);
                    if index <= self.snapshot.ships.len() + 1 {
                        self.select_navigation(index);
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
                "{} {} pending issues",
                if self.ascii { "!" } else { "⚓" },
                notices.len()
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
                    "{} ships · {} assignments · {} pending issues",
                    self.snapshot.ships.len(),
                    self.snapshot.tasks.len(),
                    self.snapshot.pending.len()
                )),
                Line::from("Select a ship to enter its workspace. i: Inbox · l: Talk to liaison"),
                Line::from(""),
            ];
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
                let pending = self
                    .snapshot
                    .pending
                    .iter()
                    .filter(|n| n.owner == ship.id)
                    .count();
                lines.push(Line::styled(
                    format!(
                        "{}{}",
                        clean(&ship.name, 160),
                        if ship.liaison { " · Liaison" } else { "" }
                    ),
                    Style::default().fg(Color::Yellow),
                ));
                lines.push(Line::from(format!(
                    "{owned} owned · {incoming} incoming handoffs · {pending} pending issues"
                )));
                lines.push(Line::from(clean(&ship.description, 512)));
                if let Some(live) = &ship.live {
                    lines.extend(super::fleet::lines(live));
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
                    "{owned} owned assignments · {incoming} incoming handoffs"
                )));
                lines.push(Line::from(""));
                lines.push(Line::from(if self.scoped_panes().is_empty() {
                    "Primary terminal is not connected. Configure an exact ship binding."
                } else {
                    "Open the primary terminal with f, or choose Terminals to inspect its crew."
                }));
                if self.demo {
                    lines.push(Line::from(
                        "Conversation and agent launch are not connected in this preview.",
                    ));
                }
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
            ListItem::new(vec![Line::from("Fleet overview"), Line::from("All ships")]),
            ListItem::new(vec![
                Line::from("Captain's inbox"),
                Line::from(if self.demo {
                    "Sample decisions / replies".into()
                } else if self.mail_errors.is_empty() {
                    format!("{} messages", self.mail.len())
                } else {
                    format!(
                        "{} messages · errors: {}",
                        self.mail.len(),
                        self.mail_errors.len()
                    )
                }),
            ]),
        ];
        for s in &self.snapshot.ships {
            let count = self
                .snapshot
                .tasks
                .iter()
                .filter(|t| t.owner == s.id)
                .count();
            items.push(ListItem::new(vec![
                Line::from(clean(&s.name, 160)),
                Line::from(format!(
                    "{} · {count} assignment{}",
                    if s.liaison { "Liaison" } else { "Peer ship" },
                    if count == 1 { "" } else { "s" }
                )),
            ]));
        }
        let block = Block::bordered().title(if self.focus_content {
            "Navigation · ←"
        } else {
            "Navigation"
        });
        self.mouse.ships = block.inner(cols[0]);
        self.mouse.ships.height -= self.mouse.ships.height % 2;
        let mut state = ListState::default().with_selected(Some(self.ship_index()));
        frame.render_stateful_widget(
            List::new(items)
                .block(block)
                .highlight_symbol("> ")
                .highlight_style(Style::default().bg(Color::DarkGray)),
            cols[0],
            &mut state,
        );
        self.mouse.ship_offset = state.offset();
        let mut body = cols[1];
        let sections = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(body);
        let mut actions = vec![("[i Inbox]", 'i'), ("[l Talk to liaison]", 'l')];
        if self.ship.is_some() {
            actions.push(("[m Message ship]", 'm'));
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
                Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).split(body);
            let block = Block::bordered();
            let inner = block.inner(sections[0]);
            let mut x = inner.x;
            for (i, label) in TABS.iter().enumerate() {
                let width =
                    (Line::from(*label).width() as u16 + 2).min(inner.right().saturating_sub(x));
                if width == 0 {
                    break;
                }
                self.mouse
                    .tabs
                    .push((Rect::new(x, inner.y, width, inner.height), i));
                x = x.saturating_add(width + 1);
            }
            frame.render_widget(
                Tabs::new(TABS)
                    .padding(" ", " ")
                    .divider("│")
                    .select(self.tab)
                    .highlight_style(Style::default().fg(Color::Yellow))
                    .block(block),
                sections[0],
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
        if self.inbox_open || (self.ship.is_some() && matches!(self.tab, 1 | 2)) {
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
            let block = Block::bordered().title(if self.inbox_open {
                "For the captain · decisions / replies / notices"
            } else if self.tab == 1 {
                "This ship's assignments / incoming handoffs"
            } else {
                "This ship's terminals"
            });
            self.mouse.list = block.inner(parts[0]);
            self.mouse.list.height -= self.mouse.list.height % 2;
            let mut state =
                ListState::default().with_selected((!entries.is_empty()).then_some(self.selected));
            frame.render_stateful_widget(
                List::new(entries)
                    .block(block)
                    .highlight_symbol("> ")
                    .highlight_style(Style::default().bg(Color::DarkGray)),
                parts[0],
                &mut state,
            );
            self.mouse.list_offset = state.offset();
            body = parts[1];
        }
        let mut lines = self.details();
        if self.snapshot.truncated {
            lines.push(Line::styled(
                "Bounded snapshot; older records remain in the runtime.",
                Style::default().fg(Color::Yellow),
            ));
        }
        let block = Block::bordered().title(title);
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
        let status = error.unwrap_or_else(|| {
            format!(
                "State {} · Herdr {} · {}",
                age(self.home_age),
                age(self.herdr_age),
                clean(&self.notice, 160)
            )
        });
        frame.render_widget(Paragraph::new(vec![Line::from(" i inbox · l liaison · m message · ↑↓ select · ← ships / → content · 0 home · 1–4 views · q quit"),Line::styled(status,Style::default().fg(if self.snapshot.presentation_error.is_some()||self.home_error.is_some()||self.herdr_error.is_some()||!self.mail_errors.is_empty(){Color::Red}else{Color::Gray}))]),rows[2]);
        self.draw_composer(frame);
    }
}
fn age(value: Option<u64>) -> String {
    value.map_or("not connected".into(), |s| format!("{s}s ago"))
}
