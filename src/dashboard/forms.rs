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
    Onboard,
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
        if self.snapshot.ships.is_empty() {
            self.onboard();
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
            Kind::Onboard => return self.onboard(),
        };
        self.form = Some(Form {
            kind,
            ship: ship.id.clone(),
            home: live.home.clone(),
            fields,
            selected: 0,
        });
    }
    pub fn onboard(&mut self) {
        if self.demo || self.mail_pending {
            return;
        }
        let harnesses = crate::cli::installed_harnesses().unwrap_or_default();
        let Some(root) = std::env::var_os("HOME").map(PathBuf::from) else {
            self.notice = "HOME is needed for the suggested ship folder.".into();
            return;
        };
        self.form = Some(Form {
            kind: Kind::Onboard,
            ship: "Welcome aboard Privateer".into(),
            home: root.join("ships/liaison/.privateer"),
            fields: vec![
                ("Name", "liaison".into()),
                (
                    "Folder",
                    root.join("ships/liaison").to_string_lossy().into_owned(),
                ),
                ("Harness", harnesses.first().cloned().unwrap_or_default()),
                ("Role: liaison / ship", "liaison".into()),
            ],
            selected: 0,
        });
        self.notice = format!(
            "Suggested: a liaison helps you set up the fleet. Installed harnesses: {}. Esc skips setup.",
            if harnesses.is_empty() {
                "none; install a supported harness first".into()
            } else {
                harnesses.join(", ")
            }
        );
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
        let old_name = form.fields[0].1.clone();
        match key {
            KeyCode::Delete => form.fields[form.selected].1.clear(),
            KeyCode::Left | KeyCode::Right
                if matches!(form.kind, Kind::Onboard) && form.selected == 2 =>
            {
                let harnesses = crate::cli::installed_harnesses().unwrap_or_default();
                if let Some(next) =
                    cycle_harness(&harnesses, &form.fields[2].1, key == KeyCode::Left)
                {
                    form.fields[2].1 = next;
                }
            }
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
        if matches!(form.kind, Kind::Onboard) && form.selected == 0 {
            if let Some(root) = std::env::var_os("HOME").map(PathBuf::from) {
                if std::path::Path::new(&form.fields[1].1) == root.join("ships").join(&old_name) {
                    form.fields[1].1 = root
                        .join("ships")
                        .join(&form.fields[0].1)
                        .to_string_lossy()
                        .into_owned();
                }
            }
        }
    }
    pub fn submit_form(&mut self) {
        let Some(form) = &self.form else { return };
        let (verb, args) = match form.kind {
            Kind::Onboard => {
                let name = form.fields[0].1.trim();
                let folder = PathBuf::from(form.fields[1].1.trim());
                let harness = form.fields[2].1.trim();
                let role = form.fields[3].1.trim();
                if crate::store::identifier(name).is_err()
                    || name == "captain"
                    || !folder.is_absolute()
                {
                    self.notice = "Choose a valid ship name and absolute folder.".into();
                    return;
                }
                if !crate::cli::installed_harnesses()
                    .unwrap_or_default()
                    .iter()
                    .any(|h| h == harness)
                {
                    self.notice =
                        "Choose an installed harness; use ←/→ on Harness to cycle.".into();
                    return;
                }
                if !matches!(role, "liaison" | "ship") {
                    self.notice = "Choose role liaison or ship.".into();
                    return;
                }
                self.action = Some(Action {
                    home: folder.join(crate::cli::STATE_DIR),
                    verb: "init".into(),
                    args: json!({"folder":folder,"name":name,"harness":harness,"liaison":role=="liaison"}),
                });
                return;
            }
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
            Kind::Onboard => "Welcome · your first ship (optional)",
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
            "Tab: field · Delete/Ctrl-U: clear · ←/→ harness · Enter: submit · Esc: skip/cancel"
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

fn cycle_harness(harnesses: &[String], current: &str, backward: bool) -> Option<String> {
    let len = harnesses.len();
    if len == 0 {
        return None;
    }
    let index = harnesses.iter().position(|h| h == current).unwrap_or(0);
    let next = if backward {
        (index + len - 1) % len
    } else {
        (index + 1) % len
    };
    Some(harnesses[next].clone())
}

#[cfg(test)]
mod tests {
    use super::cycle_harness;

    #[test]
    fn harness_arrows_cycle_backward_and_forward_with_wrap() {
        let harnesses = ["codex", "claude", "grok"].map(String::from);
        assert_eq!(
            cycle_harness(&harnesses, "codex", false).as_deref(),
            Some("claude")
        );
        assert_eq!(
            cycle_harness(&harnesses, "grok", false).as_deref(),
            Some("codex")
        );
        assert_eq!(
            cycle_harness(&harnesses, "codex", true).as_deref(),
            Some("grok")
        );
        assert_eq!(
            cycle_harness(&harnesses, "claude", true).as_deref(),
            Some("codex")
        );
        assert_eq!(cycle_harness(&[], "codex", true), None);
    }
}
