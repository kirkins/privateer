//! Needs you, one at a time: Enter on a need opens it full screen with its
//! whole message, the steps it asks of the captain, any report, and its
//! choices with what each one does. ←/→ walks the queue. Decisions from any
//! machine are answered with `captain.reply` and stale asks dismissed with
//! `captain.dismiss`; the runtime checks the ask is still open and that a
//! typed answer names one of its options. This machine's approvals keep the
//! bridge's exact-binding answer path.
use super::{
    bridge::{Answer, Need},
    deck::Ask,
    model::clean,
    ui::App,
};
use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph, Wrap},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// One answer a need offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opt {
    /// The decision's option id; none for plain choices and "Done".
    pub id: Option<String>,
    pub label: String,
    pub what: String,
    pub recommended: bool,
}

/// One need as the queue and the focus view show it.
#[derive(Clone, Debug)]
pub struct Item {
    pub id: String,
    pub ship: String,
    pub machine: String,
    pub kind: String,
    pub hands: bool,
    pub question: String,
    /// The full message, when the captain's waiting list has it.
    pub body: Option<String>,
    pub step: Option<String>,
    pub report: Option<String>,
    pub options: Vec<Opt>,
    pub since_ms: Option<i64>,
    pub task: Option<String>,
    pub urgency: String,
    pub deadline_ms: Option<i64>,
    pub mail_id: Option<String>,
    /// Index in this machine's attention items, which the bridge can act on.
    pub local: Option<usize>,
    pub stale: bool,
    pub raw: Value,
}

impl Item {
    pub fn new(
        raw: &Value,
        machine: &str,
        local: Option<usize>,
        stale: bool,
        asks: &BTreeMap<String, Ask>,
    ) -> Self {
        let text = |key: &str| {
            raw[key]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let mail_id = text("mail_id").or_else(|| {
            (raw["source"] == "mail")
                .then(|| text("source_id"))
                .flatten()
        });
        let ask = mail_id.as_deref().and_then(|id| asks.get(id));
        let hands = raw["needs_hands"] == true;
        let mut options: Vec<Opt> = raw["decision"]["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|o| {
                Some(Opt {
                    id: Some(o["id"].as_str()?.to_owned()),
                    label: o["label"].as_str().unwrap_or("?").to_owned(),
                    what: o["what_happens"].as_str().unwrap_or("").to_owned(),
                    recommended: o["recommended"] == true,
                })
            })
            .collect();
        if options.is_empty() {
            options = raw["choices"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|label| Opt {
                    id: None,
                    label: label.to_owned(),
                    what: String::new(),
                    recommended: false,
                })
                .collect();
        }
        if options.is_empty() && hands && raw["kind"] == "decision" {
            options.push(Opt {
                id: None,
                label: "Done".into(),
                what: "Tell the ship you have done it; add a note below for what it asked you to report.".into(),
                recommended: false,
            });
        }
        let decision_question = raw["decision"]["question"].as_str();
        Self {
            id: raw["id"].as_str().unwrap_or("").to_owned(),
            ship: raw["ship"].as_str().unwrap_or("?").to_owned(),
            machine: raw["machine"].as_str().unwrap_or(machine).to_owned(),
            kind: raw["kind"].as_str().unwrap_or("unknown").to_owned(),
            hands,
            question: decision_question
                .map(str::to_owned)
                .or_else(|| text("question"))
                .or_else(|| text("summary"))
                .unwrap_or_default(),
            body: ask
                .map(|a| a.body.clone())
                .filter(|b| !b.is_empty())
                .or_else(|| decision_question.and(text("question"))),
            step: ask.and_then(|a| a.step.clone()),
            report: ask.and_then(|a| a.report.clone()),
            options,
            since_ms: raw["asked_ms"].as_i64().or(raw["created_ms"].as_i64()),
            task: text("task_id"),
            urgency: text("urgency").unwrap_or_else(|| "normal".into()),
            deadline_ms: raw["deadline_ms"].as_i64(),
            mail_id,
            local,
            stale,
            raw: raw.clone(),
        }
    }

    /// Answered by choosing or typing (decisions), not by approving.
    fn chooses(&self) -> bool {
        self.kind == "decision" && self.mail_id.is_some()
    }

    /// Choice rows plus the note row.
    fn rows(&self) -> usize {
        if self.chooses() {
            self.options.len() + 1
        } else {
            0
        }
    }
}

/// The focus view's state for the need it shows.
#[derive(Clone, Debug, Default)]
pub struct Focus {
    pub id: String,
    /// This machine's need exactly as it was when opened: an answer never
    /// rebinds to a replaced or copied home's generation of it.
    pub need: Option<Need>,
    pub cursor: usize,
    pub note: String,
    pub dismissing: bool,
    pub deny: bool,
    pub scroll: u16,
}

/// A command the focus view asks the worker to run.
#[derive(Clone, Debug)]
pub struct Send {
    pub verb: &'static str,
    pub args: Value,
    /// The need it settles, so the view moves on once it lands.
    pub need: String,
    pub done: String,
}

impl App {
    /// This machine's needs first, then the fleet's others oldest first,
    /// without the ones answered from here since the last reading.
    pub fn queue(&self) -> Vec<Item> {
        let asks = &self.deck.asks;
        let fleet = self.deck.fleet.as_ref();
        let here = fleet.map_or_else(String::new, |f| f.machine.clone());
        let local = self.attention.items();
        let mut items: Vec<Item> = local
            .iter()
            .enumerate()
            .map(|(i, n)| Item::new(&n.item, &here, Some(i), n.stale, asks))
            .collect();
        let mut others: Vec<Item> = fleet
            .into_iter()
            .flat_map(|f| &f.needs)
            .filter(|n| !local.iter().any(|l| l.id() == n.id()))
            .map(|n| Item::new(&n.0, n.machine(), None, false, asks))
            .collect();
        // Asks oldest first; blockers, which ships clear themselves, last.
        others.sort_by_key(|i| (i.kind == "blocker", i.since_ms.unwrap_or(i64::MAX)));
        items.extend(others);
        items.retain(|i| !self.answered.contains(&i.id));
        items
    }

    pub fn open_focus(&mut self, id: &str) {
        let item = self.queue().into_iter().find(|i| i.id == id);
        let cursor = item
            .as_ref()
            .and_then(|i| i.options.iter().position(|o| o.recommended))
            .unwrap_or(0);
        let need = item
            .and_then(|i| i.local)
            .and_then(|l| self.attention.items().get(l).map(|n| (*n).clone()));
        self.focus = Some(Focus {
            id: id.to_owned(),
            need,
            cursor,
            ..Focus::default()
        });
        self.notice.clear();
    }

    /// Keys while the focus view is open.
    pub(super) fn focus_key(&mut self, key: KeyCode) {
        let queue = self.queue();
        let Some(focus) = self.focus.as_mut() else {
            return;
        };
        let pos = queue.iter().position(|i| i.id == focus.id);
        let item = pos.map(|p| &queue[p]);
        let rows = item.map_or(0, Item::rows);
        let on_note = focus.dismissing || (rows > 0 && focus.cursor + 1 == rows);
        match key {
            KeyCode::Esc if focus.dismissing => {
                focus.dismissing = false;
                focus.note.clear();
            }
            KeyCode::Esc => self.focus = None,
            KeyCode::Left | KeyCode::Right => {
                let next = match (pos, key) {
                    (Some(p), KeyCode::Left) => p.checked_sub(1),
                    (Some(p), _) => Some(p + 1).filter(|n| *n < queue.len()),
                    (None, _) => (!queue.is_empty()).then_some(0),
                };
                if let Some(next) = next {
                    let id = queue[next].id.clone();
                    self.open_focus(&id);
                }
            }
            KeyCode::PageUp => focus.scroll = focus.scroll.saturating_sub(8),
            KeyCode::PageDown => focus.scroll = focus.scroll.saturating_add(8),
            KeyCode::Up if !focus.dismissing => focus.cursor = focus.cursor.saturating_sub(1),
            KeyCode::Down if !focus.dismissing => {
                focus.cursor = (focus.cursor + 1).min(rows.saturating_sub(1))
            }
            KeyCode::Backspace => {
                focus.note.pop();
            }
            KeyCode::Enter if !self.mail_pending => {
                if let Some(item) = item {
                    self.focus_submit(item.clone());
                }
            }
            KeyCode::Char(c) if clean(&c.to_string(), 1) == c.to_string() => {
                let approval = item.is_some_and(|i| i.kind == "approval");
                if on_note {
                    if focus.note.chars().count() < 2000 {
                        focus.note.push(c);
                    }
                } else if approval && matches!(c, 'a' | 'd') {
                    focus.deny = c == 'd';
                } else if let Some(n) = c.to_digit(10).filter(|n| (1..=9).contains(n)) {
                    if (n as usize) < rows {
                        focus.cursor = n as usize - 1;
                    }
                } else if c == 'x' && item.is_some_and(|i| i.mail_id.is_some()) {
                    focus.dismissing = true;
                    focus.note.clear();
                } else if rows > 0 {
                    focus.cursor = rows - 1;
                    focus.note.push(c);
                }
            }
            _ => {}
        }
    }

    fn focus_submit(&mut self, item: Item) {
        let Some(focus) = self.focus.as_ref() else {
            return;
        };
        let note = focus.note.trim().to_owned();
        let ship = clean(&item.ship, 60);
        if focus.dismissing {
            let Some(mail_id) = item.mail_id.clone() else {
                return;
            };
            self.focus_send = Some(Send {
                verb: "captain.dismiss",
                args: json!({"id": mail_id, "note": note}),
                need: item.id,
                done: format!("Dismissed {ship}'s ask."),
            });
            return;
        }
        // This machine's needs keep the exact-binding answer path.
        if let Some(need) = focus.need.clone() {
            if !matches!(item.kind.as_str(), "approval" | "decision") {
                self.notice =
                    "A recorded blocker: nothing to answer here; the ship clears it.".into();
                return;
            }
            // Execution revalidates the exact item, home and version.
            let mut answer = Answer::new(need);
            answer.deny = focus.deny;
            let real = answer.need.item["choices"].as_array().map_or(0, Vec::len);
            if focus.cursor < real {
                answer.choice = focus.cursor;
                answer.note = note;
            } else if focus.cursor < item.options.len() {
                // The hands-on "Done" this view adds.
                answer.body = if note.is_empty() {
                    "Done.".into()
                } else {
                    format!("Done.\n\n{note}")
                };
            } else if note.is_empty() && answer.need.item["kind"] == "decision" {
                self.notice = "Choose an answer or type a note first.".into();
                return;
            } else {
                answer.body = note;
            }
            self.answer_action = Some(answer);
            return;
        }
        match item.kind.as_str() {
            "approval" => match item.local {
                Some(_) => {
                    self.notice = "Select the need again; it changed since you opened it.".into()
                }
                None => {
                    self.notice = format!(
                        "Approvals are granted on the machine that holds them: open {}'s bridge.",
                        clean(&item.machine, 60)
                    )
                }
            },
            _ if !item.chooses() => {
                self.notice =
                    "A recorded blocker: nothing to answer here; the ship clears it.".into()
            }
            _ => {
                let mail_id = item.mail_id.clone().unwrap_or_default();
                let chosen = item.options.get(focus.cursor);
                let (body, answer) = match chosen {
                    Some(option) => (
                        if note.is_empty() {
                            option.label.clone()
                        } else {
                            format!("{}\n\n{note}", option.label)
                        },
                        option.id.as_ref().map(|id| {
                            let mut answer = json!({"option_id": id});
                            if !note.is_empty() {
                                answer["text"] = json!(note);
                            }
                            answer
                        }),
                    ),
                    None if note.is_empty() => {
                        self.notice = "Choose an answer or type a note first.".into();
                        return;
                    }
                    None => (
                        note.clone(),
                        item.options
                            .iter()
                            .any(|o| o.id.is_some())
                            .then(|| json!({"text": note})),
                    ),
                };
                let mut args = json!({"id": mail_id, "body": body});
                if let Some(answer) = answer {
                    args["payload"] = json!({"answer": answer});
                }
                self.focus_send = Some(Send {
                    verb: "captain.reply",
                    args,
                    need: item.id,
                    done: format!("Answered {ship}."),
                });
            }
        }
    }

    /// A focus command landed: hide the need and move on to the next one.
    pub fn focus_done(&mut self, need: &str, notice: String) {
        let queue = self.queue();
        let next = queue
            .iter()
            .position(|i| i.id == need)
            .and_then(|p| {
                queue
                    .get(p + 1)
                    .or_else(|| p.checked_sub(1).and_then(|q| queue.get(q)))
            })
            .map(|i| i.id.clone());
        self.answered.insert(need.to_owned());
        if self.focus.as_ref().is_some_and(|f| f.id == need) {
            match next {
                Some(id) => self.open_focus(&id),
                None => self.focus = None,
            }
        }
        self.notice = if self.queue().is_empty() {
            format!("{notice} Nothing else is waiting on you.")
        } else {
            notice
        };
    }

    /// Header text for the focus view: where this need sits in the queue.
    pub(super) fn focus_header(&self) -> String {
        let queue = self.queue();
        let Some(focus) = &self.focus else {
            return String::new();
        };
        match queue.iter().position(|i| i.id == focus.id) {
            Some(p) => {
                let item = &queue[p];
                let here = self
                    .deck
                    .fleet
                    .as_ref()
                    .is_some_and(|f| f.machine == item.machine);
                format!(
                    " NEEDS YOU · {} of {} · {}{}",
                    p + 1,
                    queue.len(),
                    clean(&item.ship, 60),
                    if here || item.machine.is_empty() {
                        String::new()
                    } else {
                        format!("@{}", clean(&item.machine, 40))
                    }
                )
            }
            None => format!(" NEEDS YOU · {} waiting", queue.len()),
        }
    }

    pub(super) fn focus_keys(&self) -> &'static str {
        let queue = self.queue();
        let item = self
            .focus
            .as_ref()
            .and_then(|f| queue.iter().find(|i| i.id == f.id));
        match (self.focus.as_ref(), item) {
            (Some(f), _) if f.dismissing => "type a reason · Enter dismiss · Esc keep the ask",
            (_, Some(i)) if i.kind == "approval" => {
                "a approve · d deny · Enter confirm · ←/→ other needs · PgUp/PgDn scroll · Esc deck"
            }
            (_, Some(i)) if i.chooses() => {
                "↑↓/1-9 choose · type a note · Enter send · x dismiss · ←/→ other needs · PgUp/PgDn scroll · Esc deck"
            }
            _ => "←/→ other needs · PgUp/PgDn scroll · Esc deck",
        }
    }

    pub(super) fn draw_focus(&self, frame: &mut Frame, area: Rect) {
        let queue = self.queue();
        let Some(focus) = &self.focus else {
            return;
        };
        let border = if self.ascii {
            BorderType::Plain
        } else {
            BorderType::Rounded
        };
        let dim = Style::default().fg(Color::DarkGray);
        let Some(item) = queue.iter().find(|i| i.id == focus.id) else {
            let mut lines = vec![Line::from("This ask is no longer open.")];
            if !self.notice.is_empty() {
                lines.push(Line::styled(
                    clean(&self.notice, 512),
                    Style::default().fg(Color::Yellow),
                ));
            }
            lines.push(Line::styled(
                "←/→ for the others · Esc back to the deck",
                dim,
            ));
            frame.render_widget(
                Paragraph::new(lines).block(Block::bordered().border_type(border)),
                area,
            );
            return;
        };
        let now = crate::store::Store::now_ms();
        let age = |ms: i64| super::deck_ui::short_age(ms, now);

        // The message: body, the steps asked of the captain, any report.
        let mut message: Vec<Line> = Vec::new();
        let body = item.body.as_deref().unwrap_or(&item.question);
        for line in body.lines() {
            message.push(Line::raw(clean(line, 4096)));
        }
        if item.body.is_some()
            && !item.question.is_empty()
            && !body.contains(item.question.as_str())
        {
            message.push(Line::raw(""));
            message.push(Line::styled(
                clean(&item.question, 2000),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
        if let Some(step) = &item.step {
            message.push(Line::raw(""));
            message.push(Line::styled(
                "Your hands",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
            message.extend(step.lines().map(|l| Line::raw(clean(l, 4096))));
        }
        if let Some(report) = &item.report {
            message.push(Line::raw(""));
            message.push(Line::styled(
                "Report",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            message.extend(report.lines().map(|l| Line::raw(clean(l, 4096))));
        }
        if item.kind == "blocker" {
            message.push(Line::raw(""));
            message.push(Line::styled(
                format!(
                    "Recorded {} · status {}. The ship clears blockers; nothing to answer here.",
                    clean(item.raw["evidence"].as_str().unwrap_or("evidence"), 80),
                    clean(item.raw["status"].as_str().unwrap_or("?"), 40)
                ),
                dim,
            ));
        }
        let glyph = match (item.hands, item.kind.as_str(), self.ascii) {
            (true, _, false) => "✋",
            (true, _, true) => "H",
            (_, "approval", _) => "A",
            (_, "blocker", _) => "!",
            _ => "?",
        };
        let mut title = vec![format!("{glyph} {}", clean(&item.kind, 20))];
        if let Some(since) = item.since_ms {
            title.push(format!("asked {} ago", age(since)));
        }
        if let Some(task) = &item.task {
            title.push(format!("task {}", clean(task, 60)));
        }
        if item.urgency != "normal" {
            title.push(clean(&item.urgency, 20));
        }
        if let Some(deadline) = item.deadline_ms {
            title.push(if deadline > now {
                format!("due in {}", super::deck_ui::short_age(now, deadline))
            } else {
                format!("overdue by {}", age(deadline))
            });
        }
        if item.stale {
            title.push("STALE".into());
        }

        // The answer: choices, the note, and what happens on Enter.
        let mut answer: Vec<Line> = Vec::new();
        let pick = |i: usize| focus.cursor == i && !focus.dismissing;
        let marker = |on: bool| {
            if on {
                if self.ascii { "> " } else { "▸ " }
            } else {
                "  "
            }
        };
        if focus.dismissing {
            answer.push(Line::from(
                "Dismiss this ask without answering it. The ship is told, with your reason.",
            ));
            answer.push(Line::from(vec![
                Span::styled("reason: ", dim),
                Span::raw(clean(&focus.note, 2000)),
                Span::styled("█", Style::default().fg(Color::Yellow)),
            ]));
        } else if item.chooses() {
            let label_w = item
                .options
                .iter()
                .map(|o| clean(&o.label, 200).chars().count() + if o.recommended { 2 } else { 0 })
                .max()
                .unwrap_or(0);
            for (i, option) in item.options.iter().enumerate() {
                let mut spans = vec![
                    Span::styled(marker(pick(i)), Style::default().fg(Color::Yellow)),
                    Span::styled(format!("{}  ", i + 1), dim),
                    Span::styled(
                        clean(&option.label, 200),
                        if pick(i) {
                            Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
                        } else {
                            Style::default().add_modifier(Modifier::BOLD)
                        },
                    ),
                ];
                if option.recommended {
                    spans.push(Span::styled(
                        if self.ascii { " *" } else { " ★" },
                        Style::default().fg(Color::Yellow),
                    ));
                }
                if !option.what.is_empty() {
                    let used = clean(&option.label, 200).chars().count()
                        + if option.recommended { 2 } else { 0 };
                    spans.push(Span::styled(
                        format!(
                            "{}   {}",
                            " ".repeat(label_w - used),
                            clean(&option.what, 600)
                        ),
                        dim,
                    ));
                }
                answer.push(Line::from(spans));
            }
            let note_row = item.options.len();
            answer.push(Line::from(vec![
                Span::styled(marker(pick(note_row)), Style::default().fg(Color::Yellow)),
                Span::styled(
                    if self.ascii {
                        "~  note: "
                    } else {
                        "✎  note: "
                    },
                    dim,
                ),
                Span::raw(clean(&focus.note, 2000)),
                Span::styled(
                    if pick(note_row) { "█" } else { "" },
                    Style::default().fg(Color::Yellow),
                ),
            ]));
        } else if item.kind == "approval" {
            if item.raw["privileged"].is_object() {
                let p = &item.raw["privileged"];
                answer.push(Line::from(format!(
                    "Runs as root on {}: {}",
                    clean(p["machine"].as_str().unwrap_or("?"), 60),
                    p["argv"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(|a| clean(a, 400))
                        .collect::<Vec<_>>()
                        .join(" ")
                )));
                answer.push(Line::styled(
                    format!(
                        "in {} · hash {}",
                        clean(p["cwd"].as_str().unwrap_or(""), 400),
                        clean(p["hash"].as_str().unwrap_or(""), 64)
                    ),
                    dim,
                ));
            } else {
                answer.push(Line::from(format!(
                    "Artifact {} · environment {}",
                    clean(item.raw["artifact"].as_str().unwrap_or("?"), 400),
                    clean(item.raw["environment"].as_str().unwrap_or("?"), 200)
                )));
            }
            answer.push(Line::from(if item.local.is_some() {
                format!("{} on Enter", if focus.deny { "DENY" } else { "APPROVE" })
            } else {
                format!("Approve it on {}'s bridge.", clean(&item.machine, 60))
            }));
        }
        if self.mail_pending {
            answer.push(Line::styled("Sending…", Style::default().fg(Color::Cyan)));
        } else if !self.notice.is_empty() {
            answer.push(Line::styled(
                clean(&self.notice, 512),
                Style::default().fg(Color::Yellow),
            ));
        }

        let inner_w = area.width.saturating_sub(2).max(1) as usize;
        let wrapped: usize = answer
            .iter()
            .map(|l| l.width().max(1).div_ceil(inner_w))
            .sum();
        let answer_h = if answer.is_empty() {
            0
        } else {
            (wrapped as u16 + 2).clamp(3, (area.height / 2).max(3))
        };
        let [message_area, answer_area] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(answer_h)]).areas(area);
        frame.render_widget(
            Paragraph::new(message)
                .block(Block::bordered().border_type(border).title(Span::styled(
                    format!(" {} ", title.join(" · ")),
                    Style::default().add_modifier(Modifier::BOLD),
                )))
                .wrap(Wrap { trim: false })
                .scroll((focus.scroll, 0)),
            message_area,
        );
        if answer_h > 0 {
            frame.render_widget(
                Paragraph::new(answer)
                    .block(Block::bordered().border_type(border).title(Span::styled(
                        if focus.dismissing {
                            " Dismiss "
                        } else {
                            " Answer "
                        },
                        Style::default().add_modifier(Modifier::BOLD),
                    )))
                    .wrap(Wrap { trim: false }),
                answer_area,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::deck::{DeckNeed, FleetView, Machine};

    fn decision(id: &str, ship: &str, asked_ms: i64) -> DeckNeed {
        DeckNeed(json!({
            "id": id, "kind": "decision", "ship": ship, "machine": "mac",
            "mail_id": format!("mail-{id}"), "asked_ms": asked_ms,
            "question": "Short summary",
            "decision": {"question": "What should I do with the simulator?", "options": [
                {"id": "a", "label": "Erase it", "recommended": true, "what_happens": "Back to 17 MB."},
                {"id": "b", "label": "Delete it", "what_happens": "Gone."}]}
        }))
    }

    fn app() -> App {
        let mut app = App::default();
        app.deck.fleet = Some(FleetView {
            machine: "laptop".into(),
            machines: vec![Machine {
                name: "laptop".into(),
                ..Default::default()
            }],
            needs: vec![
                decision("n2", "fitforge", 20),
                DeckNeed(
                    json!({"id": "n1", "kind": "decision", "ship": "cosmic", "machine": "mac",
                    "mail_id": "mail-n1", "asked_ms": 10, "needs_hands": true, "question": "Run the check"}),
                ),
            ],
            ..Default::default()
        });
        app.deck.asks.insert(
            "mail-n1".into(),
            Ask {
                body: "Build 17 is on TestFlight.".into(),
                step: Some("1. Install build 17".into()),
                report: None,
            },
        );
        app
    }

    #[test]
    fn needs_read_whole_with_their_options_and_hands_on_asks_offer_done() {
        let app = app();
        let queue = app.queue();
        // Oldest first; the hands-on ask takes its message and steps from the waiting list.
        assert_eq!(queue[0].id, "n1");
        assert_eq!(queue[0].body.as_deref(), Some("Build 17 is on TestFlight."));
        assert_eq!(queue[0].step.as_deref(), Some("1. Install build 17"));
        assert_eq!(queue[0].options[0].label, "Done");
        let typed = &queue[1];
        assert_eq!(typed.question, "What should I do with the simulator?");
        assert_eq!(typed.options.len(), 2);
        assert!(typed.options[0].recommended);
    }

    #[test]
    fn a_choice_with_a_note_replies_with_its_option_and_the_queue_moves_on() {
        let mut app = app();
        app.open_focus("n2");
        // Opens on the recommended option; 2 picks the other, typing writes the note.
        assert_eq!(app.focus.as_ref().unwrap().cursor, 0);
        app.focus_key(KeyCode::Char('2'));
        app.focus_key(KeyCode::Down);
        for c in "keep the logs".chars() {
            app.focus_key(KeyCode::Char(c));
        }
        app.focus_key(KeyCode::Up);
        app.focus_key(KeyCode::Enter);
        let send = app.focus_send.take().unwrap();
        assert_eq!(send.verb, "captain.reply");
        assert_eq!(send.args["id"], "mail-n2");
        assert_eq!(
            send.args["payload"]["answer"],
            json!({"option_id": "b", "text": "keep the logs"})
        );
        assert_eq!(send.args["body"], "Delete it\n\nkeep the logs");

        // Once it lands the need leaves the queue and the view shows the next.
        app.focus_done("n2", "Answered fitforge.".into());
        assert_eq!(app.focus.as_ref().unwrap().id, "n1");
        assert!(app.queue().iter().all(|i| i.id != "n2"));

        // x dismisses the ask with the typed reason; Esc first backs out of it.
        app.focus_key(KeyCode::Char('x'));
        for c in "done already".chars() {
            app.focus_key(KeyCode::Char(c));
        }
        app.focus_key(KeyCode::Enter);
        let send = app.focus_send.take().unwrap();
        assert_eq!(send.verb, "captain.dismiss");
        assert_eq!(send.args, json!({"id": "mail-n1", "note": "done already"}));
        app.focus_key(KeyCode::Esc);
        assert!(!app.focus.as_ref().unwrap().dismissing);
        app.focus_key(KeyCode::Esc);
        assert!(app.focus.is_none());
    }
}
