//! Fleet home presentation: one scannable row per ship and a live summary in
//! the sidebar. Rendering only; every value comes from the read-only snapshot.
use super::{
    captain::Mail,
    model::{Ship, Snapshot, clean, count},
};
use ratatui::{
    layout::Constraint,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, Row},
};

fn attention() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

/// Aboard, not aboard or unavailable, readable without colour.
pub fn presence(ship: &Ship, ascii: bool) -> Span<'static> {
    let (glyph, style) = match &ship.live {
        Some(live) if live.error.is_some() => (if ascii { "x" } else { "✕" }, Color::Red),
        Some(live) if live.attended => (if ascii { "*" } else { "●" }, Color::Green),
        Some(_) => (if ascii { "o" } else { "○" }, Color::DarkGray),
        None => (" ", Color::Reset),
    };
    Span::styled(glyph, Style::default().fg(style))
}

struct Work {
    owned: usize,
    incoming: usize,
    pending: usize,
}

fn work(ship: &Ship, snapshot: &Snapshot) -> Work {
    Work {
        owned: snapshot.tasks.iter().filter(|t| t.owner == ship.id).count(),
        incoming: snapshot
            .tasks
            .iter()
            .filter(|t| t.handoff_to.as_ref() == Some(&ship.id))
            .count(),
        pending: snapshot
            .pending
            .iter()
            .filter(|n| n.owner == ship.id)
            .count(),
    }
}

/// The sidebar's second line: what needs the captain, in a few words.
pub fn summary(ship: &Ship, snapshot: &Snapshot) -> String {
    if ship.live.as_ref().is_some_and(|l| l.error.is_some()) {
        return "home unavailable".into();
    }
    let work = work(ship, snapshot);
    let live = ship.live.as_ref();
    let mut parts = Vec::new();
    if ship.liaison {
        parts.push("liaison".to_owned());
    }
    for (n, singular, plural) in [
        (live.map_or(0, |l| l.requests), "req", "req"),
        (live.map_or(0, |l| l.unread), "unread", "unread"),
        (work.pending, "pending", "pending"),
        (work.owned, "task", "tasks"),
    ] {
        if n > 0 {
            parts.push(count(n, singular, plural));
        }
    }
    if parts.len() == usize::from(ship.liaison) {
        parts.push("quiet".into());
    }
    parts.join(" · ")
}

/// The sidebar's inbox line: what to answer before what to read.
pub fn inbox(mail: &[Mail], errors: usize) -> String {
    let (answer, other): (Vec<_>, Vec<_>) = mail.iter().partition(|m| {
        matches!(m.kind.to_lowercase().as_str(), "request" | "decision") && m.status != "answered"
    });
    let answer = answer.len();
    let unread = other.iter().filter(|m| !m.acknowledged).count();
    let mut parts = Vec::new();
    if answer > 0 {
        parts.push(format!("{answer} to answer"));
    }
    if unread > 0 {
        parts.push(format!("{unread} unread"));
    }
    if parts.is_empty() {
        parts.push(
            if mail.is_empty() {
                "No messages"
            } else {
                "All read"
            }
            .into(),
        );
    }
    if errors > 0 {
        parts.push(count(errors, "error", "errors"));
    }
    parts.join(" · ")
}

/// Columns in the order they are dropped from the right on narrow screens,
/// and whether each needs a live home observation.
const COLUMNS: [(&str, u16, bool); 8] = [
    ("Ship", 0, false),
    ("Aboard", 13, true),
    ("Requests", 8, true),
    ("Unread", 6, true),
    ("Pending", 7, false),
    ("Tasks", 6, false),
    ("Dispatcher", 10, true),
    ("Last wake", 14, true),
];

/// Whether any ship has a live home observation (never in the demo).
pub fn observed(snapshot: &Snapshot) -> bool {
    snapshot.ships.iter().any(|s| s.live.is_some())
}

/// Header, rows and widths of the fleet table that fit in `width` columns.
pub fn table(
    snapshot: &Snapshot,
    width: u16,
    ascii: bool,
) -> (Row<'static>, Vec<Row<'static>>, Vec<Constraint>) {
    let name = snapshot
        .ships
        .iter()
        .map(|s| Line::from(clean(&s.name, 160)).width() as u16 + 2)
        .max()
        .unwrap_or(4)
        .clamp(6, 24);
    let live_columns = observed(snapshot);
    let mut used = name;
    let shown: Vec<usize> = (0..COLUMNS.len())
        .filter(|&i| live_columns || !COLUMNS[i].2)
        .take_while(|&i| {
            used += if i == 0 { 0 } else { COLUMNS[i].1 + 1 };
            used <= width
        })
        .collect();
    let widths = shown
        .iter()
        .map(|&i| Constraint::Length(if i == 0 { name } else { COLUMNS[i].1 }))
        .collect();
    let header = Row::new(shown.iter().map(|&i| COLUMNS[i].0))
        .style(Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED));
    let number = |n: usize| {
        if n == 0 {
            Cell::from("·").style(Style::default().fg(Color::DarkGray))
        } else {
            Cell::from(n.to_string()).style(attention())
        }
    };
    let rows = snapshot
        .ships
        .iter()
        .map(|ship| {
            let work = work(ship, snapshot);
            let live = ship.live.as_ref().filter(|l| l.error.is_none());
            let aboard = match &ship.live {
                Some(l) if l.error.is_some() => "unavailable",
                Some(l) if l.attended => "aboard",
                Some(_) => "—",
                None => "",
            };
            let mut cells = [
                Cell::from(Line::from(vec![
                    Span::raw(clean(&ship.name, 160)),
                    Span::styled(
                        match (ship.liaison, ascii) {
                            (false, _) => "",
                            (true, false) => " ★",
                            (true, true) => " *",
                        },
                        Style::default().fg(Color::Cyan),
                    ),
                ])),
                Cell::from(Line::from(vec![
                    presence(ship, ascii),
                    Span::raw(format!(" {aboard}")),
                ])),
                number(live.map_or(0, |l| l.requests)),
                number(live.map_or(0, |l| l.unread)),
                number(work.pending),
                Cell::from(if work.incoming > 0 {
                    format!("{} +{}", work.owned, work.incoming)
                } else {
                    work.owned.to_string()
                }),
                Cell::from(live.map_or("—", |l| if l.dispatcher { "running" } else { "stopped" }))
                    .style(Style::default().fg(if live.is_some_and(|l| !l.dispatcher) {
                        Color::DarkGray
                    } else {
                        Color::Reset
                    })),
                Cell::from(live.and_then(|l| l.wakes.first()).map_or("—".into(), |w| {
                    format!("{} {}", clean(&w.mode, 40), clean(&w.status, 40))
                })),
            ];
            Row::new(
                shown
                    .iter()
                    .map(|&i| std::mem::take(&mut cells[i]))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    (header, rows, widths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mail(kind: &str, status: &str, acknowledged: bool) -> Mail {
        Mail {
            id: kind.into(),
            kind: kind.into(),
            from: "apps".into(),
            subject: String::new(),
            body: String::new(),
            payload: serde_json::json!({}),
            status: status.into(),
            home: None,
            seq: 0,
            acknowledged,
        }
    }

    #[test]
    fn inbox_line_puts_answers_before_unread_and_counts_errors() {
        assert_eq!(inbox(&[], 0), "No messages");
        assert_eq!(inbox(&[], 2), "No messages · 2 errors");
        let mixed = [
            mail("request", "open", true),
            mail("Decision", "Awaiting captain", false),
            mail("decision", "answered", false),
            mail("notice", "unread", false),
            mail("reply", "unread", true),
        ];
        assert_eq!(inbox(&mixed, 1), "2 to answer · 2 unread · 1 error");
        assert_eq!(inbox(&[mail("notice", "unread", true)], 0), "All read");
    }

    #[test]
    fn sidebar_summary_names_what_needs_attention_or_says_quiet() {
        let mut ship = Ship {
            id: "apps".into(),
            name: "Apps".into(),
            ..Ship::default()
        };
        assert_eq!(summary(&ship, &Snapshot::default()), "quiet");
        ship.liaison = true;
        assert_eq!(summary(&ship, &Snapshot::default()), "liaison · quiet");
        ship.live = Some(super::super::fleet::ShipState {
            requests: 2,
            unread: 1,
            ..Default::default()
        });
        assert_eq!(
            summary(&ship, &Snapshot::default()),
            "liaison · 2 req · 1 unread"
        );
        ship.live.as_mut().unwrap().error = Some("missing".into());
        assert_eq!(summary(&ship, &Snapshot::default()), "home unavailable");
        assert_eq!(presence(&ship, true).content, "x");
    }
}
