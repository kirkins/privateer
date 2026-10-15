//! Small operator forms. A submitted form produces a command, never a DB write.
use super::{mailbox::Action, model::clean, ui::App};
use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Block, Clear, Paragraph, Wrap},
};
use serde_json::json;
use std::path::PathBuf;

#[derive(Clone, Copy)]
pub enum Kind {
    New,
    Retire,
    Dispatcher,
}
pub struct Form {
    pub kind: Kind,
    pub ship: String,
    pub home: PathBuf,
    pub fields: Vec<(&'static str, String)>,
    pub selected: usize,
}
impl App {
    pub fn manage(&mut self, kind: Kind) {
        if self.demo {
            self.notice = "Demo only; no fleet changes.".into();
            return;
        }
        if self.mail_pending {
            return;
        }
        let ship = self
            .snapshot
            .ships
            .iter()
            .find(|s| Some(&s.id) == self.ship.as_ref())
            .or_else(|| {
                self.snapshot
                    .ships
                    .iter()
                    .find(|s| s.live.as_ref().is_some_and(|l| l.error.is_none()))
            });
        let Some(ship) = ship else {
            self.notice = "Create your first ship through onboarding.".into();
            return;
        };
        let Some(live) = ship.live.as_ref().filter(|l| l.error.is_none()) else {
            self.notice = "Ship home is unavailable.".into();
            return;
        };
        let fields = match kind {
            Kind::New => vec![
                ("Name", String::new()),
                ("Scope", String::new()),
                ("Folder (empty = sibling)", String::new()),
                ("Harness", live.harness.clone()),
            ],
            Kind::Retire => vec![("Type the ship name to confirm", String::new())],
            Kind::Dispatcher => vec![("Action: status / start / stop", "status".into())],
        };
        self.form = Some(Form {
            kind,
            ship: ship.id.clone(),
            home: live.home.clone(),
            fields,
            selected: 0,
        });
    }
    pub fn edit_form(&mut self, key: KeyCode) {
        if self.mail_pending {
            return;
        }
        if key == KeyCode::Esc {
            self.form = None;
            return;
        }
        if key == KeyCode::Enter {
            self.submit_form();
            return;
        }
        let Some(form) = &mut self.form else { return };
        match key {
            KeyCode::Tab | KeyCode::Down => form.selected = (form.selected + 1) % form.fields.len(),
            KeyCode::Up => {
                form.selected = (form.selected + form.fields.len() - 1) % form.fields.len()
            }
            KeyCode::Backspace => {
                form.fields[form.selected].1.pop();
            }
            KeyCode::Char(c)
                if clean(&c.to_string(), 1) == c.to_string()
                    && form.fields[form.selected].1.chars().count() < 512 =>
            {
                form.fields[form.selected].1.push(c)
            }
            _ => {}
        }
    }
    pub fn submit_form(&mut self) {
        let Some(form) = &self.form else { return };
        let (verb, args) = match form.kind {
            Kind::New => {
                let name = form.fields[0].1.trim();
                if crate::store::identifier(name).is_err() || name == "captain" {
                    self.notice = "Choose a valid ship name.".into();
                    return;
                }
                let mut args =
                    json!({"name":name,"scope":form.fields[1].1,"harness":form.fields[3].1});
                if !form.fields[2].1.trim().is_empty() {
                    args["folder"] = json!(form.fields[2].1.trim());
                }
                ("ship.create", args)
            }
            Kind::Retire => {
                if form.fields[0].1 != form.ship {
                    self.notice =
                        "Type the exact ship name to confirm retirement. Its folder is kept."
                            .into();
                    return;
                }
                (
                    "ship.retire",
                    json!({"name":form.ship,"expected_home":form.home}),
                )
            }
            Kind::Dispatcher => {
                let action = form.fields[0].1.trim();
                if !matches!(action, "status" | "start" | "stop") {
                    self.notice = "Dispatcher action must be status, start or stop.".into();
                    return;
                }
                ("service", json!({"action":action}))
            }
        };
        self.action = Some(Action {
            home: form.home.clone(),
            verb: verb.into(),
            args,
        });
    }
    pub fn draw_form(&self, frame: &mut Frame) {
        let Some(form) = &self.form else { return };
        let area = frame.area();
        let width = area.width.saturating_sub(4).min(88);
        let height = area.height.saturating_sub(2).min(18);
        let popup = Rect::new(
            (area.width - width) / 2,
            (area.height - height) / 2,
            width,
            height,
        );
        let title = match form.kind {
            Kind::New => "New ship",
            Kind::Retire => "Retire ship · folder is kept",
            Kind::Dispatcher => "Dispatcher",
        };
        frame.render_widget(Clear, popup);
        let mut lines = vec![Line::from(clean(&form.ship, 100))];
        for (i, (label, value)) in form.fields.iter().enumerate() {
            lines.push(Line::styled(
                format!(
                    "{} {label}: {}",
                    if i == form.selected { ">" } else { " " },
                    clean(value, 512)
                ),
                Style::default().fg(if i == form.selected {
                    Color::Yellow
                } else {
                    Color::Gray
                }),
            ));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(if self.mail_pending {
            "Command pending…"
        } else {
            "Tab/↑↓: field · Enter: submit · Esc: cancel"
        }));
        lines.push(Line::from(clean(&self.notice, 512)));
        frame.render_widget(
            Paragraph::new(lines)
                .block(Block::bordered().title(title))
                .wrap(Wrap { trim: false }),
            popup,
        );
    }
}
