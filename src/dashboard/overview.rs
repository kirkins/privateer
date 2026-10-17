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

struct Work<'a> {
    owned: usize,
    incoming: usize,
    pending: usize,
    /// Task ids of jobs recorded as launching or running for this ship.
    crew: Vec<&'a str>,
}

fn work<'a>(ship: &Ship, snapshot: &'a Snapshot) -> Work<'a> {
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
        crew: snapshot
            .jobs
            .iter()
            .filter(|j| j.owner == ship.id && j.is_active())
            .map(|j| j.task_id.as_str())
            .collect(),
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
const COLUMNS: [(&str, u16, bool); 9] = [
    ("Ship", 0, false),
    ("Aboard", 13, true),
    ("Requests", 8, true),
    ("Unread", 6, true),
    ("Pending", 7, false),
    ("Tasks", 6, false),
    ("Running crew", 16, false),
    ("Dispatcher", 10, true),
    ("Last wake", 14, true),
];

/// Whether any ship has a live home observation (never in the demo).
pub fn observed(snapshot: &Snapshot) -> bool {
    snapshot.ships.iter().any(|s| s.live.is_some())
}

/// Longest ship name shown in the table; the liaison star takes two more columns.
const NAME_CHARS: usize = 22;

/// A name cut to `NAME_CHARS` with an ellipsis (ASCII dots in `--ascii`).
fn short_name(name: &str, ascii: bool) -> String {
    let name = clean(name, 160);
    if name.chars().count() <= NAME_CHARS {
        return name;
    }
    let mark = if ascii { "..." } else { "…" };
    let kept: String = name
        .chars()
        .take(NAME_CHARS - mark.chars().count())
        .collect();
    format!("{kept}{mark}")
}

/// Empty cell marker: a middle dot, or a hyphen in `--ascii`.
fn empty(ascii: bool) -> &'static str {
    if ascii { "-" } else { "·" }
}

/// One row of the fleet table: a heading naming a machine, or a ship by its
/// index in `snapshot.ships`.
#[derive(Debug, PartialEq, Eq)]
pub enum FleetRow {
    Heading(String),
    Ship(usize),
}

/// This machine's ships first, then one group per linked machine. Headings
/// appear only when a machine is linked, so an unlinked fleet keeps its plain
/// table.
pub fn fleet_rows(snapshot: &Snapshot, now_ms: i64) -> Vec<FleetRow> {
    let local = snapshot
        .ships
        .iter()
        .enumerate()
        .filter(|(_, ship)| ship.remote.is_none())
        .map(|(index, _)| FleetRow::Ship(index));
    if snapshot.machines.is_empty() {
        return local.collect();
    }
    let host = clean(&crate::machines::hostname(), 160);
    let mut rows = vec![FleetRow::Heading(format!("This machine · {host}"))];
    rows.extend(local);
    for machine in &snapshot.machines {
        let reached = match machine.last_reached_ms {
            Some(ms) => format!("last reached {}", super::ui::ago(ms, now_ms)),
            None => "never reached".into(),
        };
        rows.push(FleetRow::Heading(format!(
            "{} · {reached}",
            clean(&machine.name, 160)
        )));
        rows.extend(
            snapshot
                .ships
                .iter()
                .enumerate()
                .filter(|(_, ship)| ship.remote.as_deref() == Some(machine.name.as_str()))
                .map(|(index, _)| FleetRow::Ship(index)),
        );
    }
    rows
}

/// The table row showing `ship`, an index into `snapshot.ships`.
pub fn row_of(rows: &[FleetRow], ship: usize) -> Option<usize> {
    rows.iter().position(|row| *row == FleetRow::Ship(ship))
}

/// Header, rows and widths of the fleet table that fit in `width` columns.
pub fn table(
    snapshot: &Snapshot,
    width: u16,
    ascii: bool,
    now_ms: i64,
) -> (Row<'static>, Vec<Row<'static>>, Vec<Constraint>) {
    let rows_layout = fleet_rows(snapshot, now_ms);
    let headings = rows_layout
        .iter()
        .filter_map(|row| match row {
            FleetRow::Heading(text) => Some(Line::from(text.as_str()).width() as u16),
            FleetRow::Ship(_) => None,
        })
        .max()
        .unwrap_or(0);
    // A heading names its machine and when it was reached; it may widen the
    // name column, up to half the width, so it is not cut short.
    let name = snapshot
        .ships
        .iter()
        .map(|s| Line::from(short_name(&s.name, ascii)).width() as u16 + 2)
        .max()
        .unwrap_or(4)
        .clamp(6, 24)
        .max(headings.min(width / 2));
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
            Cell::from(empty(ascii)).style(Style::default().fg(Color::DarkGray))
        } else {
            Cell::from(n.to_string()).style(attention())
        }
    };
    let dash = if ascii { "-" } else { "—" };
    let rows = rows_layout
        .into_iter()
        .map(|row| match row {
            FleetRow::Heading(text) => heading(&text, &shown),
            FleetRow::Ship(index) => {
                let ship = &snapshot.ships[index];
                if ship.remote.is_some() {
                    return remote_row(ship, &shown, ascii);
                }
                let work = work(ship, snapshot);
                let live = ship.live.as_ref().filter(|l| l.error.is_none());
                let aboard = match &ship.live {
                    Some(l) if l.error.is_some() => "unavailable",
                    Some(l) if l.attended => "aboard",
                    Some(_) if ascii => "-",
                    Some(_) => "—",
                    None => "",
                };
                let mut cells = [
                    Cell::from(Line::from(vec![
                        Span::raw(short_name(&ship.name, ascii)),
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
                    Cell::from(match work.crew.as_slice() {
                        [] => empty(ascii).to_owned(),
                        [task] => format!("1 {}", clean(task, 40)),
                        crew => format!("{} running", crew.len()),
                    })
                    .style(if work.crew.is_empty() {
                        Style::default().fg(Color::DarkGray)
                    } else {
                        attention()
                    }),
                    Cell::from(
                        live.map_or(dash, |l| if l.dispatcher { "running" } else { "stopped" }),
                    )
                    .style(Style::default().fg(
                        if live.is_some_and(|l| !l.dispatcher) {
                            Color::DarkGray
                        } else {
                            Color::Reset
                        },
                    )),
                    Cell::from(live.and_then(|l| l.wakes.first()).map_or(dash.into(), |w| {
                        format!("{} {}", clean(&w.mode, 40), clean(&w.status, 40))
                    })),
                ];
                Row::new(
                    shown
                        .iter()
                        .map(|&i| std::mem::take(&mut cells[i]))
                        .collect::<Vec<_>>(),
                )
            }
        })
        .collect();
    (header, rows, widths)
}

/// A machine's heading: its text in the first column, the rest empty.
fn heading(text: &str, shown: &[usize]) -> Row<'static> {
    let style = Style::default().add_modifier(Modifier::BOLD);
    Row::new(
        shown
            .iter()
            .map(|&i| {
                if i == 0 {
                    Cell::from(Line::from(Span::styled(text.to_owned(), style)))
                } else {
                    Cell::default()
                }
            })
            .collect::<Vec<_>>(),
    )
}

/// A ship on a linked machine: marked as remote, with dashes where its live
/// state would have to cross the link. Its scope is in the details pane.
fn remote_row(ship: &Ship, shown: &[usize], ascii: bool) -> Row<'static> {
    let dash = if ascii { "-" } else { "—" };
    let grey = Style::default().fg(Color::DarkGray);
    let mut cells = vec![
        Cell::from(Line::from(vec![
            Span::raw(short_name(&ship.name, ascii)),
            Span::styled(" · remote", grey),
        ])),
        Cell::from("not shared").style(grey),
    ];
    cells.extend((2..COLUMNS.len()).map(|_| Cell::from(dash).style(grey)));
    Row::new(
        shown
            .iter()
            .map(|&i| std::mem::take(&mut cells[i]))
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mail(kind: &str, status: &str, acknowledged: bool) -> Mail {
        Mail {
            identity: None,
            origin: None,
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
    fn long_names_end_in_an_ellipsis_and_ascii_uses_plain_marks() {
        let short = "Apps";
        assert_eq!(short_name(short, false), short);
        let long = "Repository maintenance and release coordination crew";
        let cut = short_name(long, false);
        assert_eq!(cut.chars().count(), NAME_CHARS);
        assert!(cut.ends_with('…') && long.starts_with(cut.trim_end_matches('…')));
        let plain = short_name(long, true);
        assert_eq!(plain.chars().count(), NAME_CHARS);
        assert!(plain.ends_with("..."));
        assert_eq!(empty(false), "·");
        assert_eq!(empty(true), "-");
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

    #[test]
    fn fleet_rows_group_remote_ships_under_their_machine() {
        use crate::dashboard::model::LinkedMachine;
        let ship = |id: &str, remote: Option<&str>| Ship {
            id: id.into(),
            name: id.into(),
            remote: remote.map(Into::into),
            ..Ship::default()
        };
        let now = 10_000_000;
        let mut snapshot = Snapshot {
            ships: vec![
                ship("apps", None),
                ship("infra", None),
                ship("mini-a", Some("mac")),
                ship("mini-b", Some("mac")),
            ],
            ..Snapshot::default()
        };
        // No linked machine: plain rows, no headings.
        snapshot.ships.truncate(2);
        assert_eq!(
            fleet_rows(&snapshot, now),
            vec![FleetRow::Ship(0), FleetRow::Ship(1)]
        );
        snapshot
            .ships
            .extend([ship("mini-a", Some("mac")), ship("mini-b", Some("mac"))]);
        snapshot.machines = vec![LinkedMachine {
            name: "mac".into(),
            last_reached_ms: Some(now - 12 * 60_000),
        }];
        let rows = fleet_rows(&snapshot, now);
        assert!(matches!(&rows[0], FleetRow::Heading(t) if t.starts_with("This machine · ")));
        assert_eq!(&rows[1..3], &[FleetRow::Ship(0), FleetRow::Ship(1)]);
        assert_eq!(
            rows[3],
            FleetRow::Heading("mac · last reached 12 min ago".into())
        );
        assert_eq!(&rows[4..], &[FleetRow::Ship(2), FleetRow::Ship(3)]);
        assert_eq!(row_of(&rows, 3), Some(5));

        snapshot.machines[0].last_reached_ms = None;
        assert_eq!(
            fleet_rows(&snapshot, now)[3],
            FleetRow::Heading("mac · never reached".into())
        );
    }
}
