//! Captain-facing presentation with command-backed live mailbox actions.
//! Demo interactions stay in memory without mailbox changes, wakes, terminal
//! input or approvals.
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
    #[serde(default)]
    pub payload: serde_json::Value,
    pub status: String,
    #[serde(default)]
    pub home: Option<std::path::PathBuf>,
    #[serde(default)]
    pub seq: i64,
    #[serde(default)]
    pub acknowledged: bool,
}

pub fn sample_mail() -> Vec<Mail> {
    vec![
        Mail {
            payload: serde_json::json!({}),
            home: None, seq: 0, acknowledged: false,
            id: "demo-decision".into(),
            kind: "Decision".into(),
            from: "Research".into(),
            subject: "Choose the next investigation".into(),
            body: "Research is available. Would you like a reliability review or a provider comparison? This is a sample decision; no work has been assigned.".into(),
            status: "Awaiting captain".into(),
        },
        Mail {
            payload: serde_json::json!({}),
            home: None, seq: 0, acknowledged: false,
            id: "demo-reply".into(),
            kind: "Reply".into(),
            from: "Apps".into(),
            subject: "Build investigation update".into(),
            body: "The sample build investigation is still owned by Apps. Infrastructure has an incoming handoff offer; responsibility changes only after acceptance.".into(),
            status: "Reply to your request".into(),
        },
        Mail {
            payload: serde_json::json!({}),
            home: None, seq: 0, acknowledged: false,
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
    pub home: Option<std::path::PathBuf>,
    pub correlation: Option<String>,
    pub id: String,
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
        if self.mail_pending {
            self.notice = "Mailbox command pending…".into();
            return;
        }
        match key {
            KeyCode::Esc => self.composer = None,
            KeyCode::Enter if !self.demo => self.submit_live(),
            KeyCode::Enter => self.submit_preview(),
            _ => {
                if let Some(draft) = &mut self.composer {
                    match key {
                        KeyCode::Tab if draft.correlation.is_none() => {
                            draft.mode = draft.mode.next()
                        }
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
            self.notice = self.snapshot.presentation_error.as_ref().map_or_else(
                || "No liaison configured; select any ship directly.".into(),
                |error| format!("Presentation unavailable: {}", clean(error, 512)),
            );
        }
    }
    pub fn compose(&mut self) {
        if self.mail_pending {
            return;
        }
        let Some(ship) = self.ship.clone() else {
            self.open_liaison();
            if self.ship.is_none() {
                return;
            }
            return self.compose();
        };
        let home = self
            .snapshot
            .ships
            .iter()
            .find(|s| s.id == ship)
            .and_then(|s| s.live.as_ref())
            .filter(|l| l.error.is_none())
            .map(|l| l.home.clone());
        if !self.demo && home.is_none() {
            self.notice = "Ship home is unavailable. No message sent.".into();
            return;
        }
        self.composer = Some(Composer {
            recipient: ship,
            home,
            id: uuid::Uuid::new_v4().to_string(),
            ..Composer::default()
        });
    }
    pub fn submit_preview(&mut self) {
        let Some(draft) = self.composer.take() else {
            return;
        };
        if !self.demo || (draft.mode != InputMode::Status && draft.body.trim().is_empty()) {
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
    pub fn reply(&mut self) {
        if self.mail_pending {
            return;
        }
        let Some(mail) = self.mail.get(self.selected) else {
            return;
        };
        if self.demo {
            self.notice = "Demo replies are sample-only.".into();
            return;
        }
        if !matches!(mail.kind.as_str(), "request" | "decision") || mail.status == "answered" {
            self.notice = "Select an open request or decision.".into();
            return;
        }
        self.composer = Some(Composer {
            recipient: mail.from.clone(),
            home: mail.home.clone(),
            correlation: Some(mail.id.clone()),
            id: uuid::Uuid::new_v4().to_string(),
            ..Composer::default()
        });
    }
    pub fn submit_live(&mut self) {
        use serde_json::json;
        let Some(draft) = self.composer.as_ref() else {
            return;
        };
        if self.action.is_some() {
            return;
        }
        if draft.mode == InputMode::Status && draft.correlation.is_none() {
            self.notice = self
                .snapshot
                .ships
                .iter()
                .find(|s| s.id == draft.recipient)
                .and_then(|s| s.live.as_ref())
                .map(|l| {
                    format!(
                        "{} open requests · {} unread mail · {}",
                        l.requests,
                        l.unread,
                        if l.attended { "aboard" } else { "not aboard" }
                    )
                })
                .unwrap_or_else(|| "Ship state unavailable".into());
            self.composer = None;
            return;
        }
        if draft.body.trim().is_empty() {
            return;
        }
        let Some(home) = draft.home.clone() else {
            return;
        };
        self.action = Some(super::mailbox::Action {
            home,
            verb: "mail.send".into(),
            args: json!({"id":draft.id,"from":"captain","to":draft.recipient,"kind":if draft.correlation.is_some() {"reply"} else if draft.mode==InputMode::Ask {"request"} else {"notice"},"correlation":draft.correlation,"body":draft.body}),
        });
    }
    pub fn inbox_lines(&self) -> Vec<Line<'static>> {
        let mut lines: Vec<_> = self
            .mail_errors
            .iter()
            .map(|e| Line::from(format!("Mailbox unavailable: {}", clean(e, 512))))
            .collect();
        let Some(mail) = self.mail.get(self.selected) else {
            lines.push(Line::from("No messages for the captain."));
            return lines;
        };
        lines.extend([
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
        ]);
        lines.extend(
            mail.body
                .split('\n')
                .map(|line| Line::from(clean(line, usize::MAX))),
        );
        if !mail.payload.is_null() && mail.payload != serde_json::json!({}) {
            lines.push(Line::from("Payload (JSON):"));
            lines.push(Line::from(clean(&mail.payload.to_string(), usize::MAX)));
        }
        lines.extend([
            Line::from(""),
            Line::from(if self.demo {
                "Sample mailbox · viewing does not acknowledge or answer this item."
            } else {
                "Enter: mark read · r: reply. A read acknowledges only the contiguous read prefix."
            }),
        ]);
        lines
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
        let block = Block::bordered().title(format!(
            "{}To {}",
            if self.demo { "DEMO · " } else { "" },
            clean(name, 80)
        ));
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
        frame.render_widget(
            Paragraph::new(if draft.correlation.is_some() {
                Line::from("Reply")
            } else {
                Line::from(modes.to_vec())
            }),
            rows[0],
        );
        frame.render_widget(
            Paragraph::new(if draft.correlation.is_some() {
                "Answer this request in its originating home."
            } else { match draft.mode {
                InputMode::Status => "Read recorded state. No agent wake or inference.",
                InputMode::Ask => {
                    "A side question, without assigning work. Live answers may use inference."
                }
                InputMode::Message => {
                    "Address work or conversation to this ship. Live delivery would request a wake."
                }
            }})
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
                "{} / 1024 chars · {}",
                draft.body.chars().count(),
                if self.demo {
                    "local sample only"
                } else {
                    "captain mailbox"
                }
            )),
            rows[3],
        );
        frame.render_widget(
            Paragraph::new(if self.demo {
                "Tab: mode · Enter: preview · Esc: cancel"
            } else if draft.correlation.is_some() {
                "Enter: reply · Esc: cancel"
            } else if draft.mode == InputMode::Status {
                "Tab: mode · Enter: status · Esc: cancel"
            } else {
                "Tab: mode · Enter: send · Esc: cancel"
            }),
            rows[4],
        );
    }
}
