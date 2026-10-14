use super::{
    herdr,
    model::{Snapshot, clean},
};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};

const TAB_LABELS: [&str; 4] = ["1 Bridge", "2 Fleet", "3 Logbook", "4 Terminals"];

/// Hit targets from the last rendered frame, including the list's actual offset.
#[derive(Default)]
pub struct MouseMap {
    tabs: Vec<(Rect, usize)>,
    list: Rect,
    details: Rect,
    list_offset: usize,
}

#[derive(Default)]
pub struct App {
    pub snapshot: Snapshot,
    pub herdr: Option<herdr::View>,
    pub home_error: Option<String>,
    pub herdr_error: Option<String>,
    pub home_age: Option<u64>,
    pub herdr_age: Option<u64>,
    pub tab: usize,
    pub selected: usize,
    pub scroll: u16,
    pub compact: bool,
    pub ascii: bool,
    pub demo: bool,
    pub notice: String,
    pub selection_invalidated: bool,
    pub mouse: MouseMap,
}

impl App {
    pub fn select_tab(&mut self, tab: usize) {
        if tab < TAB_LABELS.len() && tab != self.tab {
            self.tab = tab;
            self.selected = 0;
            self.scroll = 0;
            self.selection_invalidated = false;
        }
    }

    /// Mouse interaction only changes the view. Opening terminals remains an
    /// explicit keyboard action through the existing identity checks.
    pub fn handle_mouse(&mut self, event: MouseEvent) {
        let point = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, tab)) = self
                    .mouse
                    .tabs
                    .iter()
                    .find(|(area, _)| area.contains(point))
                {
                    self.select_tab(*tab);
                } else if self.mouse.list.contains(point) {
                    let index =
                        self.mouse.list_offset + usize::from((event.row - self.mouse.list.y) / 2);
                    if index < self.selection_len() {
                        self.selected = index;
                        self.scroll = 0;
                        self.selection_invalidated = false;
                    }
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let down = event.kind == MouseEventKind::ScrollDown;
                if self.mouse.details.contains(point) {
                    self.scroll = if down {
                        self.scroll.saturating_add(3)
                    } else {
                        self.scroll.saturating_sub(3)
                    };
                } else if self.mouse.list.contains(point) {
                    self.move_selection(down);
                }
            }
            _ => {}
        }
    }

    pub fn selection_len(&self) -> usize {
        if self.tab == 3 {
            self.herdr.as_ref().map_or(0, |h| h.panes.len())
        } else {
            self.snapshot.tasks.len()
        }
    }

    pub fn move_selection(&mut self, down: bool) {
        self.selection_invalidated = false;
        let len = self.selection_len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = if down {
            (self.selected + 1).min(len - 1)
        } else {
            self.selected.saturating_sub(1)
        };
        self.scroll = 0;
    }

    pub fn update_herdr(&mut self, view: herdr::View) {
        if self.tab == 3 {
            if let Some(old) = self.herdr.as_ref().and_then(|v| v.panes.get(self.selected)) {
                if let Some(index) = view.panes.iter().position(|p| p.pane_id == old.pane_id) {
                    let new = &view.panes[index];
                    self.selection_invalidated |= self
                        .herdr
                        .as_ref()
                        .is_some_and(|v| v.socket_identity != view.socket_identity)
                        || old.terminal_id != new.terminal_id
                        || old.agent != new.agent
                        || old.agent_session != new.agent_session;
                    self.selected = index;
                } else {
                    self.selected = 0;
                    self.selection_invalidated = true;
                }
                if self.selection_invalidated {
                    self.notice =
                        "Terminal selection changed; select an agent again before opening.".into();
                }
            }
        }
        self.herdr = Some(view);
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        self.mouse = MouseMap::default();
        let area = frame.area();
        if area.width < 40 || area.height < 11 {
            frame.render_widget(
                Paragraph::new("PRIVATEER\nResize to at least 40 x 11.\nq: quit")
                    .wrap(Wrap { trim: false }),
                area,
            );
            return;
        }
        let rows = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(area);
        let suffix = if self.demo {
            "DEMO / sample data"
        } else {
            "READ-ONLY / local home"
        };
        let title = Line::from(vec![
            Span::styled(
                " PRIVATEER ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  {suffix}")),
        ]);
        frame.render_widget(
            Paragraph::new(vec![
                title,
                Line::from(" Independent peers. One clear view of the voyage."),
            ]),
            rows[0],
        );
        let tabs_block = Block::bordered();
        let tab_area = tabs_block.inner(rows[1]);
        let mut x = tab_area.x;
        for (tab, label) in TAB_LABELS.iter().enumerate() {
            let width =
                (Line::from(*label).width() as u16 + 2).min(tab_area.right().saturating_sub(x));
            if width == 0 {
                break;
            }
            self.mouse
                .tabs
                .push((Rect::new(x, tab_area.y, width, tab_area.height), tab));
            x = x.saturating_add(width + 1); // one-cell divider
        }
        frame.render_widget(
            Tabs::new(TAB_LABELS)
                .padding(" ", " ")
                .divider("│")
                .select(self.tab)
                .highlight_style(Style::default().fg(Color::Yellow))
                .block(tabs_block),
            rows[1],
        );
        let cols = Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)])
            .split(rows[2]);
        self.mouse.list = Block::bordered().inner(cols[0]);
        // Two-row list items never occupy a leftover single row.
        self.mouse.list.height -= self.mouse.list.height % 2;
        self.mouse.details = Block::bordered().inner(cols[1]);
        let mut items = Vec::new();
        if self.tab == 3 {
            if let Some(h) = &self.herdr {
                for p in &h.panes {
                    items.push(ListItem::new(vec![
                        Line::from(format!(
                            "{} · {}",
                            clean(&p.pane_id, 40),
                            clean(p.agent.as_deref().unwrap_or("shell"), 40)
                        )),
                        Line::styled(
                            clean(&p.agent_status, 30),
                            Style::default().fg(status_color(&p.agent_status)),
                        ),
                    ]));
                }
            }
        } else {
            for task in &self.snapshot.tasks {
                items.push(ListItem::new(vec![
                    Line::from(clean(&task.title, 80)),
                    Line::styled(
                        format!("{} · {}", clean(&task.owner, 40), clean(&task.status, 30)),
                        Style::default().fg(status_color(&task.status)),
                    ),
                ]));
            }
        }
        if items.is_empty() {
            items.push(ListItem::new("No entries"));
        }
        let mut state =
            ListState::default().with_selected(Some(self.selected.min(items.len() - 1)));
        frame.render_stateful_widget(
            List::new(items)
                .block(Block::bordered().title(if self.tab == 3 {
                    "Herdr terminals"
                } else {
                    "Assignments"
                }))
                .highlight_style(Style::default().bg(Color::DarkGray))
                .highlight_symbol("> "),
            cols[0],
            &mut state,
        );
        self.mouse.list_offset = state.offset();

        let mut details = Vec::<Line>::new();
        if let Some(error) = &self.home_error {
            details.push(Line::styled(
                format!("STATE UNAVAILABLE: {}", clean(error, 256)),
                Style::default().fg(Color::Red),
            ));
        }
        if let Some(error) = &self.herdr_error {
            details.push(Line::styled(
                format!("HERDR UNAVAILABLE: {}", clean(error, 256)),
                Style::default().fg(Color::Red),
            ));
        }
        details.push(Line::styled(
            format!(
                "State refresh: {} | Herdr refresh: {}",
                age(self.home_age),
                age(self.herdr_age)
            ),
            Style::default().fg(Color::DarkGray),
        ));
        details.push(Line::from(""));
        match self.tab {
            0 => {
                if let Some(task) = self.snapshot.tasks.get(self.selected) {
                    details.push(Line::styled(
                        clean(&task.title, 512),
                        Style::default().fg(Color::Yellow),
                    ));
                    details.push(Line::from(format!(
                        "Owner: {}   Task: {}",
                        clean(&task.owner, 160),
                        clean(&task.id, 160)
                    )));
                    details.push(Line::from(format!(
                        "Recorded state: {}",
                        clean(&task.status, 40)
                    )));
                } else {
                    details.push(Line::from("No assignments in this home."));
                }
                details.push(Line::from(""));
                details.push(Line::from(
                    "Bridge preview: inspect assignments and open native agent terminals.",
                ));
                details.push(Line::from(
                    "Navigator chat and agent launch are not connected in this slice.",
                ));
                details.push(Line::from(""));
                self.pending_lines(&mut details, None);
                if !self.compact {
                    details.push(Line::from("Recent activity:"));
                    for event in self.snapshot.activity.iter().take(20) {
                        details.push(Line::from(format!(
                            "#{} {}: {}",
                            event.seq,
                            clean(&event.task_id, 160),
                            clean(&event.summary, 2048)
                        )));
                    }
                }
            }
            1 => {
                details.push(Line::from(
                    "Recorded jobs (observation only; no process reconciliation):",
                ));
                for job in &self.snapshot.jobs {
                    details.push(Line::from(format!(
                        "{}  {}  {}",
                        clean(&job.id, 120),
                        clean(&job.task_id, 160),
                        clean(&job.status, 40)
                    )));
                }
                details.push(Line::from(""));
                self.pending_lines(&mut details, None);
            }
            2 => {
                details.push(Line::from("Latest durable event facts (newest first):"));
                for event in &self.snapshot.activity {
                    details.push(Line::styled(
                        format!(
                            "#{} {} · {}",
                            event.seq,
                            clean(&event.task_id, 160),
                            clean(&event.kind, 80)
                        ),
                        Style::default().fg(Color::Cyan),
                    ));
                    details.push(Line::from(clean(&event.summary, 2048)));
                }
            }
            _ => {
                if let Some(h) = &self.herdr {
                    details.push(Line::from(format!(
                        "Herdr {} / protocol {}",
                        clean(&h.version, 40),
                        h.protocol
                    )));
                    if let Some(p) = h.panes.get(self.selected) {
                        details.push(Line::from(format!(
                            "Pane: {}  Terminal: {}",
                            clean(&p.pane_id, 80),
                            clean(&p.terminal_id, 160)
                        )));
                        details.push(Line::from(format!(
                            "Workspace: {}  Tab: {}",
                            clean(&p.workspace_id, 80),
                            clean(&p.tab_id, 80)
                        )));
                        details.push(Line::from(format!(
                            "Agent: {}  Observed state: {}",
                            clean(p.agent.as_deref().unwrap_or("not detected"), 80),
                            clean(&p.agent_status, 40)
                        )));
                        details.push(Line::from(format!(
                            "Title: {}",
                            clean(p.title.as_deref().unwrap_or("—"), 256)
                        )));
                        details.push(Line::from(""));
                        details.push(Line::from("Enter/f: focus this recognized agent in Herdr."));
                        details.push(Line::from("Return using Herdr's tab/workspace navigation."));
                        details.push(Line::from(
                            "Terminal observations do not verify task completion.",
                        ));
                    }
                } else {
                    details.push(Line::from(
                        "Open inside Herdr or pass --herdr-socket /absolute/path.",
                    ));
                }
            }
        }
        if self.snapshot.truncated {
            details.push(Line::styled(
                "View bounded to 200 rows per section; older data remains in the runtime.",
                Style::default().fg(Color::Yellow),
            ));
        }
        frame.render_widget(
            Paragraph::new(details)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0))
                .block(Block::bordered().title(if self.compact {
                    "Summary · c for detail"
                } else {
                    "Activity and evidence"
                })),
            cols[1],
        );

        let feedback = if self.notice.is_empty() {
            "Viewing never acknowledges alerts or restarts workers."
        } else {
            &self.notice
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(
                    " Click/1–4 views · ↑↓ select · wheel/PgUp/PgDn scroll · c details · q quit",
                ),
                Line::from(clean(feedback, 256)),
            ]),
            rows[3],
        );
    }

    fn pending_lines(&self, lines: &mut Vec<Line<'static>>, limit: Option<usize>) {
        let anchor = if self.ascii { "!" } else { "⚓" };
        lines.push(Line::styled(
            format!(
                "{anchor} {} grouped pending issues (not acknowledged here)",
                self.snapshot.pending.len()
            ),
            Style::default().fg(Color::Yellow),
        ));
        for event in self
            .snapshot
            .pending
            .iter()
            .take(limit.unwrap_or(usize::MAX))
        {
            lines.push(Line::from(format!(
                "{} / {} [#{}]",
                clean(&event.task_id, 160),
                clean(&event.issue_key, 160),
                event.seq
            )));
            lines.push(Line::from(clean(&event.summary, 2048)));
        }
        if limit.is_some_and(|n| self.snapshot.pending.len() > n) {
            lines.push(Line::from("More issues in Fleet; c expands this view."));
        }
    }
}

fn age(value: Option<u64>) -> String {
    value.map_or("never".into(), |s| format!("{s}s ago"))
}
fn status_color(status: &str) -> Color {
    match status {
        "failed" | "blocked" => Color::Red,
        "running" | "working" => Color::Cyan,
        "done" => Color::Green,
        _ => Color::Gray,
    }
}
