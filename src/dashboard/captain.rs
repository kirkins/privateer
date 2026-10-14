//! Captain-facing presentation. Demo interactions stay in memory; no runtime
//! mailbox, wake, terminal input or approval is synthesized by this module.
use super::{model::clean, ui::App};
use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph, Wrap},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Mail {
    pub id: String,
    pub kind: String,
    pub from: String,
    pub subject: String,
    pub body: String,
    pub status: String,
}

pub fn sample_mail() -> Vec<Mail> {
    vec![
        Mail {
            id: "demo-decision".into(),
            kind: "Decision".into(),
            from: "Research".into(),
            subject: "Choose the next investigation".into(),
            body: "Research is available. Would you like a reliability review or a provider comparison? This is a sample decision; no work has been assigned.".into(),
            status: "Awaiting captain".into(),
        },
        Mail {
            id: "demo-reply".into(),
            kind: "Reply".into(),
            from: "Apps".into(),
            subject: "Build investigation update".into(),
            body: "The sample build investigation is still owned by Apps. Infrastructure has an incoming handoff offer; responsibility changes only after acceptance.".into(),
            status: "Reply to your request".into(),
        },
        Mail {
            id: "demo-notice".into(),
            kind: "Notice".into(),
            from: "Infrastructure".into(),
            subject: "Provider connection needs attention".into(),
            body: "Repeated connection alerts are grouped into one item. Detailed operational facts remain in the ship's Bridge and Logbook.".into(),
            status: "Grouped notice".into(),
        },
    ]
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputMode {
    Status,
    Ask,
    #[default]
    Message,
}
impl InputMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Status => "Status",
            Self::Ask => "Ask",
            Self::Message => "Message",
        }
    }
    pub fn next(self) -> Self {
        match self {
            Self::Status => Self::Ask,
            Self::Ask => Self::Message,
            Self::Message => Self::Status,
        }
    }
}

#[derive(Default)]
pub struct Composer {
    pub recipient: String,
    pub mode: InputMode,
    pub body: String,
}

#[derive(Clone)]
pub struct Exchange {
    pub ship: String,
    pub mode: InputMode,
    pub body: String,
    pub response: String,
}

impl App {
    pub fn edit_composer(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => self.composer = None,
            KeyCode::Enter => self.submit_preview(),
            _ => {
                if let Some(draft) = &mut self.composer {
                    match key {
                        KeyCode::Tab => draft.mode = draft.mode.next(),
                        KeyCode::Backspace => {
                            draft.body.pop();
                        }
                        KeyCode::Char(c)
                            if draft.body.chars().count() < 1024
                                && clean(&c.to_string(), 1) == c.to_string() =>
                        {
                            draft.body.push(c)
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    pub fn open_inbox(&mut self) {
        self.select_ship(None);
        self.inbox_open = true;
        self.selected = 0;
        self.scroll = 0;
        self.focus_content = true;
    }
    pub fn open_liaison(&mut self) {
        if let Some(id) = self
            .snapshot
            .ships
            .iter()
            .find(|s| s.liaison)
            .map(|s| s.id.clone())
        {
            self.select_ship(Some(id));
            self.select_tab(0);
        } else {
            self.notice = "No liaison configured; select any ship directly.".into();
        }
    }
    pub fn compose(&mut self) {
        let Some(ship) = self.ship.clone() else {
            self.open_liaison();
            if self.ship.is_none() {
                return;
            }
            return self.compose();
        };
        if !self.demo {
            self.notice =
                "Messaging unavailable: captain's front door is not connected. No message sent."
                    .into();
            return;
        }
        self.composer = Some(Composer {
            recipient: ship,
            ..Composer::default()
        });
    }
    pub fn submit_preview(&mut self) {
        let Some(draft) = self.composer.take() else {
            return;
        };
        if !self.demo || draft.body.trim().is_empty() {
            self.notice = "No message sent.".into();
            return;
        }
        let response = match draft.mode {
            InputMode::Status => format!(
                "Recorded sample state: {} owned assignments, {} pending issues. No model call or wake.",
                self.snapshot.tasks.iter().filter(|t| t.owner == draft.recipient).count(),
                self.snapshot.pending.iter().filter(|n| n.owner == draft.recipient).count()
            ),
            InputMode::Ask => "Side-question preview only. A connected harness would answer without creating fleet work; it may use inference.".into(),
            InputMode::Message => "Sample queued locally for preview. No agent was woken and no live message was delivered.".into(),
        };
        if self.exchanges.len() >= 20 {
            self.exchanges.drain(..self.exchanges.len() - 19);
        }
        self.select_ship(Some(draft.recipient.clone()));
        self.select_tab(0);
        self.exchanges.push(Exchange {
            ship: draft.recipient,
            mode: draft.mode,
            body: draft.body,
            response,
        });
        self.notice = "Demo interaction only · discarded on exit · no inference".into();
    }
    pub fn inbox_lines(&self) -> Vec<Line<'static>> {
        if !self.demo {
            return vec![
                Line::from("Captain's inbox is not connected to a runtime mailbox yet."),
                Line::from("Ship operational alerts remain in their own workspaces."),
                Line::from("No captain decisions or replies are inferred from task events."),
            ];
        }
        let Some(mail) = self.mail.get(self.selected) else {
            return vec![Line::from("No messages for the captain.")];
        };
        vec![
            Line::styled(
                clean(&mail.subject, 160),
                Style::default().fg(Color::Yellow),
            ),
            Line::from(format!(
                "{} · From {} · {}",
                clean(&mail.kind, 40),
                clean(&mail.from, 160),
                clean(&mail.status, 160)
            )),
            Line::from(""),
            Line::from(clean(&mail.body, 2048)),
            Line::from(""),
            Line::from("Sample mailbox · viewing does not acknowledge or answer this item."),
            Line::from("Live replies and decisions await the captain's front door."),
        ]
    }
    pub fn draw_composer(&self, frame: &mut Frame) {
        let Some(draft) = &self.composer else {
            return;
        };
        let area = frame.area();
        let width = area.width.saturating_sub(4).min(84);
        let height = area.height.saturating_sub(2).min(13);
        let popup = Rect::new(
            (area.width - width) / 2,
            (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);
        let name = self
            .snapshot
            .ships
            .iter()
            .find(|s| s.id == draft.recipient)
            .map_or(draft.recipient.as_str(), |s| s.name.as_str());
        let block = Block::bordered().title(format!("DEMO · To {}", clean(name, 80)));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
        let modes = [InputMode::Status, InputMode::Ask, InputMode::Message].map(|mode| {
            Span::styled(
                format!(" {} ", mode.label()),
                Style::default().fg(if draft.mode == mode {
                    Color::Yellow
                } else {
                    Color::Gray
                }),
            )
        });
        frame.render_widget(Paragraph::new(Line::from(modes.to_vec())), rows[0]);
        frame.render_widget(
            Paragraph::new(match draft.mode {
                InputMode::Status => "Read recorded state. No agent wake or inference.",
                InputMode::Ask => {
                    "A side question, without assigning work. Live answers may use inference."
                }
                InputMode::Message => {
                    "Address work or conversation to this ship. Live delivery would request a wake."
                }
            })
            .wrap(Wrap { trim: false }),
            rows[1],
        );
        // Keep the end of a long draft visible and leave controls on screen.
        // Allow for double-width characters in the available input rows.
        let limit = usize::from(rows[2].width.saturating_sub(4) / 2) * usize::from(rows[2].height);
        let tail: String = draft
            .body
            .chars()
            .rev()
            .take(limit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        frame.render_widget(
            Paragraph::new(format!(
                "> {}{}",
                if tail.len() < draft.body.len() {
                    "…"
                } else {
                    ""
                },
                tail
            ))
            .wrap(Wrap { trim: false }),
            rows[2],
        );
        frame.render_widget(
            Paragraph::new(format!(
                "{} / 1024 chars · local sample only",
                draft.body.chars().count()
            )),
            rows[3],
        );
        frame.render_widget(
            Paragraph::new("Tab: mode · Enter: preview · Esc: cancel"),
            rows[4],
        );
    }
}
