//! The bridge's command deck: stat tiles, the fleet as a machine → ship tree
//! with what each ship is doing and an hourly activity strip, Needs you, and
//! a live feed of recorded events. Rendering and tree navigation only; every
//! value comes from the deck reading or the local snapshot, and an absent
//! reading shows as absent, never as quiet.
use super::{
    deck::{BUCKETS, Deck, DeckShip, Event, FleetView},
    model::{Snapshot, clean},
    overview,
    ui::App,
};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Cell, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
    },
};

/// One line of the fleet tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeRow {
    Machine {
        name: String,
        here: bool,
    },
    Ship {
        machine: String,
        name: String,
        /// Index in the local snapshot's ships, when the bridge can act on it.
        local: Option<usize>,
        /// Index in the deck's fleet ships, when the fleet view lists it.
        deck: Option<usize>,
    },
}

/// This machine first, then each linked machine, ships by name. Without a
/// fleet reading the tree is the local snapshot's own grouping.
pub fn tree(snapshot: &Snapshot, fleet: Option<&FleetView>, now_ms: i64) -> Vec<TreeRow> {
    let Some(fleet) = fleet.filter(|f| !f.machines.is_empty()) else {
        return overview::fleet_rows(snapshot, now_ms)
            .into_iter()
            .enumerate()
            .map(|(i, row)| match row {
                overview::FleetRow::Heading(name) => TreeRow::Machine { name, here: i == 0 },
                overview::FleetRow::Ship(index) => TreeRow::Ship {
                    machine: snapshot.ships[index].remote.clone().unwrap_or_default(),
                    name: snapshot.ships[index].name.clone(),
                    local: Some(index),
                    deck: None,
                },
            })
            .collect();
    };
    let mut machines: Vec<_> = fleet.machines.iter().collect();
    machines.sort_by_key(|m| m.name != fleet.machine);
    let mut rows = Vec::new();
    for machine in machines {
        let here = machine.name == fleet.machine;
        rows.push(TreeRow::Machine {
            name: machine.name.clone(),
            here,
        });
        let mut ships: Vec<_> = fleet
            .ships
            .iter()
            .enumerate()
            .filter(|(_, s)| s.machine == machine.name)
            .collect();
        ships.sort_by(|a, b| a.1.name.cmp(&b.1.name));
        let mut matched = Vec::new();
        for (index, ship) in ships {
            let local = snapshot.ships.iter().position(|s| {
                s.id == ship.name
                    && if here {
                        s.remote.is_none()
                    } else {
                        s.remote.as_deref() == Some(machine.name.as_str())
                    }
            });
            matched.extend(local);
            rows.push(TreeRow::Ship {
                machine: machine.name.clone(),
                name: ship.name.clone(),
                local,
                deck: Some(index),
            });
        }
        if here {
            // Local ships the fleet view does not list stay reachable.
            for (index, ship) in snapshot.ships.iter().enumerate() {
                if ship.remote.is_none() && !matched.contains(&index) {
                    rows.push(TreeRow::Ship {
                        machine: machine.name.clone(),
                        name: ship.name.clone(),
                        local: Some(index),
                        deck: None,
                    });
                }
            }
        }
    }
    rows
}

/// Compact age: "now", "12m", "3h", "2d".
pub fn short_age(then_ms: i64, now_ms: i64) -> String {
    let minutes = (now_ms - then_ms).max(0) / 60_000;
    match minutes {
        0 => "now".into(),
        1..=59 => format!("{minutes}m"),
        60..=2879 => format!("{}h", minutes / 60),
        _ => format!("{}d", minutes / 1440),
    }
}

fn gib(bytes: f64) -> String {
    let g = bytes / 1_073_741_824.0;
    if g >= 10.0 {
        format!("{g:.0}G")
    } else {
        format!("{g:.1}G")
    }
}

fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{}k", n / 1_000),
        1_000_000..1_000_000_000 => format!("{}M", n / 1_000_000),
        _ => format!("{:.1}B", n as f64 / 1e9),
    }
}

/// Hourly counts as a strip, scaled to `top` so strips compare across rows.
fn strip(values: &[u32; BUCKETS], top: u32, ascii: bool) -> Vec<Span<'static>> {
    const BARS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    const PLAIN: [&str; 8] = [".", ":", "-", "=", "+", "*", "#", "@"];
    let levels = if ascii { PLAIN } else { BARS };
    values
        .iter()
        .map(|&v| {
            if v == 0 {
                Span::styled(levels[0], Style::default().fg(Color::DarkGray))
            } else {
                let level = (v as usize * 7).div_ceil(top.max(1) as usize).clamp(1, 7);
                Span::styled(levels[level], Style::default().fg(Color::Green))
            }
        })
        .collect()
}

fn gauge(share: f64, cells: usize, ascii: bool) -> String {
    let filled = ((share.clamp(0.0, 1.0) * cells as f64).round() as usize).min(cells);
    let (on, off) = if ascii { ("#", "-") } else { ("▰", "▱") };
    format!("{}{}", on.repeat(filled), off.repeat(cells - filled))
}

/// Local wall clock, HH:MM.
fn clock(now_ms: i64) -> String {
    let secs = (now_ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return String::new();
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

fn need_glyph(kind: &str, hands: bool, ascii: bool) -> &'static str {
    match (hands, kind, ascii) {
        (true, _, false) => "✋",
        (true, _, true) => "H",
        (_, "approval", _) => "A",
        (_, "blocker", _) => "!",
        _ => "?",
    }
}

/// The `--ascii` stand-in for a feed symbol.
fn plain(glyph: &str) -> &'static str {
    match glyph {
        "▶" => ">",
        "✓" | "✔" => "+",
        "✗" => "x",
        "■" => "#",
        "✉" => "@",
        "★" => "*",
        "–" => "-",
        _ => "?",
    }
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn block(title: impl Into<String>, ascii: bool) -> Block<'static> {
    Block::bordered()
        .border_type(if ascii {
            BorderType::Plain
        } else {
            BorderType::Rounded
        })
        .title(Span::styled(
            format!(" {} ", title.into()),
            Style::default().add_modifier(Modifier::BOLD),
        ))
}

/// A feed line's symbol, colour and text, or nothing for events the feed skips.
fn describe(
    event: &Event,
    fleet: Option<&FleetView>,
    snapshot: &Snapshot,
) -> Option<(&'static str, Color, String)> {
    let task = event.task.as_deref();
    let title = || {
        let found = fleet
            .and_then(|f| {
                f.ships
                    .iter()
                    .flat_map(|s| &s.jobs)
                    .find(|j| Some(j.task_id.as_str()) == task)
                    .and_then(|j| j.task_title.clone())
            })
            .or_else(|| {
                snapshot
                    .tasks
                    .iter()
                    .find(|t| Some(t.id.as_str()) == task && t.owner == event.ship)
                    .map(|t| t.title.clone())
            });
        clean(&found.or(task.map(str::to_owned)).unwrap_or_default(), 120)
    };
    Some(match (event.kind.as_str(), event.status.as_deref()) {
        ("job.started", _) if task.is_some() => ("▶", Color::Cyan, format!("crew · {}", title())),
        ("job.started", _) => ("▶", Color::Cyan, "crew started".into()),
        ("job.finished", Some("succeeded")) => ("✓", Color::Green, "crew finished".into()),
        ("job.finished", Some("failed")) => (
            "✗",
            Color::Red,
            match event.exit_code {
                Some(code) => format!("crew failed · exit {code}"),
                None => "crew failed".into(),
            },
        ),
        ("job.finished", status) => (
            "■",
            Color::Yellow,
            format!("crew {}", clean(status.unwrap_or("ended"), 40)),
        ),
        ("task.create", _) => ("+", Color::Blue, format!("new task · {}", title())),
        ("task.update", Some("done")) => ("✔", Color::Green, format!("done · {}", title())),
        ("task.update", Some("cancelled")) => {
            ("–", Color::DarkGray, format!("cancelled · {}", title()))
        }
        ("captain.reply", _) => ("✉", Color::Magenta, "captain answered".into()),
        ("captain.dismiss", _) => ("✉", Color::Magenta, "captain dismissed an ask".into()),
        ("ship.created", _) => ("★", Color::Yellow, "ship created".into()),
        (kind, _) if kind.starts_with("privileged.") => ("#", Color::Yellow, clean(kind, 60)),
        _ => return None,
    })
}

impl App {
    pub fn tree(&self) -> Vec<TreeRow> {
        tree(
            &self.snapshot,
            self.deck.fleet.as_ref(),
            crate::store::Store::now_ms(),
        )
    }

    /// The tree row of the current selection.
    pub fn tree_index(&self, rows: &[TreeRow]) -> Option<usize> {
        if let Some((machine, name)) = &self.pick {
            return rows.iter().position(|r| {
                matches!(r, TreeRow::Ship { machine: m, name: n, .. } if m == machine && n == name)
            });
        }
        let local = self.ship.as_ref().map(|_| self.ship_index())?;
        rows.iter()
            .position(|r| matches!(r, TreeRow::Ship { local: Some(i), .. } if *i == local))
    }

    pub fn select_row(&mut self, row: &TreeRow) {
        let TreeRow::Ship {
            machine,
            name,
            local,
            ..
        } = row
        else {
            return;
        };
        self.select_ship(local.map(|i| self.snapshot.ships[i].id.clone()));
        self.pick = Some((machine.clone(), name.clone()));
    }

    /// Moves to the next or previous ship in the tree, skipping machines.
    pub(super) fn move_in_tree(&mut self, down: bool) {
        let rows = self.tree();
        let ships: Vec<usize> = (0..rows.len())
            .filter(|&i| matches!(rows[i], TreeRow::Ship { .. }))
            .collect();
        let Some(&first) = ships.first() else {
            return;
        };
        let next = match self.tree_index(&rows) {
            None => Some(first),
            // At either end the current ship is selected again, which is how
            // a changed selection is confirmed.
            Some(current) if down => ships
                .iter()
                .copied()
                .find(|&i| i > current)
                .or(Some(current)),
            Some(current) => ships
                .iter()
                .rev()
                .copied()
                .find(|&i| i < current)
                .or(Some(current)),
        };
        if let Some(next) = next {
            self.select_row(&rows[next]);
        }
    }

    /// A picked ship this bridge cannot open: on another machine, or with no
    /// local record. Returns its machine and name.
    pub fn picked_elsewhere(&self) -> Option<&(String, String)> {
        self.pick.as_ref().filter(|_| self.ship.is_none())
    }

    fn deck_ship(&self, row: &TreeRow) -> Option<&DeckShip> {
        match row {
            TreeRow::Ship { deck: Some(i), .. } => self.deck.fleet.as_ref()?.ships.get(*i),
            _ => None,
        }
    }

    pub(super) fn header_line(&self, now: i64) -> (Line<'static>, String) {
        let mut spans = vec![Span::styled(
            " PRIVATEER BRIDGE",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )];
        if let Some(fleet) = &self.deck.fleet {
            let working = fleet.ships.iter().flat_map(|s| s.running());
            let (crews, jobs) = working.fold((0, 0), |(c, j), job| match job.noun() {
                Some("crew") => (c + 1, j),
                Some(_) => (c, j + 1),
                None => (c, j),
            });
            spans.push(Span::styled(
                format!(
                    "  {} · {} · {}",
                    super::model::count(fleet.machines.len(), "machine", "machines"),
                    super::model::count(fleet.ships.len(), "ship", "ships"),
                    match (crews, jobs) {
                        (0, 0) => "nothing running".to_owned(),
                        (c, 0) => format!("{} at work", super::model::count(c, "crew", "crews")),
                        (c, j) => format!(
                            "{} and {} at work",
                            super::model::count(c, "crew", "crews"),
                            super::model::count(j, "job", "jobs")
                        ),
                    }
                ),
                Style::default().fg(Color::Gray),
            ));
        } else {
            spans.push(Span::styled("  reading the fleet…", dim()));
        }
        (Line::from(spans), clock(now))
    }

    pub(super) fn draw_deck(&mut self, frame: &mut Frame, area: Rect, list_style: Style) {
        let now = crate::store::Store::now_ms();
        let tiles = area.height >= 22 && area.width >= 60;
        let [tiles_area, main] = Layout::vertical([
            Constraint::Length(if tiles { 4 } else { 0 }),
            Constraint::Min(0),
        ])
        .areas(area);
        if tiles {
            self.draw_tiles(frame, tiles_area, now);
        }
        let wide = main.width >= 100;
        let [fleet_area, right] = if wide {
            Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(main)
        } else {
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(main)
        };
        // The tree takes what it needs; the models panel fills what is left.
        let tree_h = self.tree().len() as u16 + 2;
        let models = self.deck.usage.is_some() && fleet_area.height >= tree_h + 6;
        let [tree_area, models_area] = Layout::vertical([
            Constraint::Length(if models { tree_h } else { fleet_area.height }),
            Constraint::Min(0),
        ])
        .areas(fleet_area);
        self.draw_tree(frame, tree_area, list_style, now);
        if models {
            self.draw_models(frame, models_area);
        }
        let needs = self.queue().len();
        let needs_h = (needs as u16 + 2).clamp(3, (right.height / 2).max(3));
        let [needs_area, lower] =
            Layout::vertical([Constraint::Length(needs_h), Constraint::Min(0)]).areas(right);
        self.draw_needs(frame, needs_area, list_style, now);
        self.draw_lower(frame, lower, now);
    }

    /// Where needs could not be read: homes the fleet view lists as unknown,
    /// and local homes the attention pass found unavailable or stale.
    fn unseen_needs(&self) -> Vec<String> {
        let mut unseen: Vec<String> = self
            .deck
            .fleet
            .iter()
            .flat_map(|f| &f.machines)
            .flat_map(|m| m.unknown.iter())
            .filter_map(|u| u.strip_prefix("needs:"))
            .map(|ship| clean(ship, 40))
            .collect();
        if self.attention.error.is_some() {
            unseen.push("this machine".into());
        }
        unseen.sort();
        unseen.dedup();
        unseen
    }

    fn draw_tiles(&self, frame: &mut Frame, area: Rect, now: i64) {
        let deck = &self.deck;
        let fleet = deck.fleet.as_ref();
        let bold = |text: String, color: Color| {
            Span::styled(
                text,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )
        };
        let mut tiles: Vec<(&str, Vec<Line<'static>>)> = Vec::new();

        // Needs you: the whole fleet when read, else this machine's runtime needs.
        let (count, oldest, hands) = match fleet {
            Some(f) => (
                f.needs.len(),
                f.needs.iter().filter_map(|n| n.since_ms()).min(),
                f.needs.iter().filter(|n| n.needs_hands()).count(),
            ),
            None => (self.attention.items().len(), None, 0),
        };
        tiles.push((
            "Needs you",
            vec![
                Line::from(vec![
                    bold(
                        count.to_string(),
                        if count > 0 {
                            Color::Yellow
                        } else {
                            Color::Green
                        },
                    ),
                    Span::styled(
                        oldest.map_or(String::new(), |o| format!("  oldest {}", short_age(o, now))),
                        dim(),
                    ),
                ]),
                Line::styled(
                    match (fleet.is_some(), count, hands) {
                        (false, _, _) => "fleet not read yet".into(),
                        (_, 0, _) => "nothing waiting".into(),
                        (_, _, 0) => "n opens the oldest".into(),
                        (_, _, h) => format!("{h} need your hands · n opens"),
                    },
                    dim(),
                ),
            ],
        ));

        // Crew and jobs at work, per machine.
        let mut per_machine: Vec<(String, usize, usize)> = Vec::new();
        let (mut crew_n, mut job_n) = (0, 0);
        for ship in fleet.into_iter().flat_map(|f| &f.ships) {
            let working: Vec<_> = ship.running().filter(|j| j.noun().is_some()).collect();
            crew_n += working.iter().filter(|j| j.noun() == Some("crew")).count();
            job_n += working.iter().filter(|j| j.noun() != Some("crew")).count();
            let running = working.len();
            let heavy = working.iter().filter(|j| j.heavy).count();
            match per_machine.iter_mut().find(|(m, _, _)| *m == ship.machine) {
                Some(entry) => {
                    entry.1 += running;
                    entry.2 += heavy;
                }
                None => per_machine.push((ship.machine.clone(), running, heavy)),
            }
        }
        let heavy: usize = per_machine.iter().map(|m| m.2).sum();
        tiles.push((
            "At work",
            if fleet.is_none() {
                vec![Line::styled("not read yet", dim())]
            } else {
                vec![
                    Line::from(vec![
                        bold(crew_n.to_string(), Color::Cyan),
                        Span::styled(" crew  ", dim()),
                        bold(job_n.to_string(), Color::Cyan),
                        Span::styled(format!(" jobs · {heavy} heavy"), dim()),
                    ]),
                    Line::styled(
                        per_machine
                            .iter()
                            .filter(|m| m.1 > 0)
                            .map(|(m, n, _)| format!("{} {n}", clean(m, 20)))
                            .collect::<Vec<_>>()
                            .join(" · "),
                        dim(),
                    ),
                ]
            },
        ));

        // Crew runs over the window.
        let (ok, bad) = deck.outcomes(now);
        let mut line = vec![Span::raw(" ")];
        line.extend(strip(
            &deck.launches(now, |_| true),
            deck.launches(now, |_| true).into_iter().max().unwrap_or(1),
            self.ascii,
        ));
        tiles.push((
            "Crew runs · 12 h",
            vec![
                Line::from(vec![
                    bold(format!("✓ {ok}"), Color::Green),
                    Span::raw("  "),
                    bold(
                        format!("✗ {bad}"),
                        if bad > 0 { Color::Red } else { Color::DarkGray },
                    ),
                    Span::styled(if deck.caught_up { "" } else { "  reading…" }, dim()),
                ]),
                Line::from(line),
            ],
        ));

        // The most-used shared plan, across machines.
        let routes = fleet
            .into_iter()
            .flat_map(|f| &f.machines)
            .flat_map(|m| &m.routes);
        let busiest = routes
            .clone()
            .filter_map(|r| r.used.as_ref().map(|u| (r, u)))
            .max_by(|a, b| a.1.share.total_cmp(&b.1.share));
        let mut held: Vec<&str> = routes
            .filter(|r| !matches!(r.state.as_str(), "available" | "idle" | ""))
            .map(|r| r.name.as_str())
            .collect();
        held.sort();
        held.dedup();
        tiles.push((
            "Plan use",
            match busiest {
                Some((route, used)) => vec![
                    Line::from(vec![
                        Span::raw(format!("{} ", clean(&route.name, 16))),
                        Span::styled(
                            gauge(used.share, 8, self.ascii),
                            Style::default().fg(Color::Cyan),
                        ),
                        bold(format!(" {:.0}%", used.share * 100.0), Color::Cyan),
                    ]),
                    Line::styled(
                        if held.is_empty() {
                            clean(used.window.trim_start_matches("weekly window, "), 40)
                        } else {
                            format!("held: {}", held.join(", "))
                        },
                        dim(),
                    ),
                ],
                None => vec![Line::styled("no recorded plan use", dim())],
            },
        ));

        // Tokens over the last day.
        tiles.push((
            "Tokens · 24 h",
            match &deck.usage {
                Some(usage) => {
                    let total: u64 = usage
                        .groups
                        .iter()
                        .filter_map(|g| g.usage.as_ref())
                        .map(|u| u.total)
                        .sum();
                    let jobs: u64 = usage.groups.iter().map(|g| g.jobs).sum();
                    let top = usage
                        .groups
                        .iter()
                        .filter_map(|g| Some((g, g.usage.as_ref()?.total)))
                        .max_by_key(|(_, t)| *t);
                    vec![
                        Line::from(vec![
                            bold(tokens(total), Color::Magenta),
                            Span::styled(format!("  {jobs} runs"), dim()),
                        ]),
                        Line::styled(
                            top.map_or(String::new(), |(g, t)| {
                                format!(
                                    "{}% {}",
                                    t * 100 / total.max(1),
                                    clean(g.key.rsplit('/').next().unwrap_or(&g.key), 24)
                                )
                            }),
                            dim(),
                        ),
                    ]
                }
                None => vec![Line::styled("not read yet", dim())],
            },
        ));

        let shown = if area.width >= 110 { 5 } else { 3 };
        let areas = Layout::horizontal(vec![Constraint::Ratio(1, shown as u32); shown]).split(area);
        for ((title, lines), area) in tiles.into_iter().take(shown).zip(areas.iter()) {
            frame.render_widget(Paragraph::new(lines).block(block(title, self.ascii)), *area);
        }
    }

    /// Tokens per model over the last day, then each machine's routes.
    fn draw_models(&self, frame: &mut Frame, area: Rect) {
        let inner_w = area.width.saturating_sub(2) as usize;
        let mut lines = Vec::new();
        let mut groups: Vec<_> = self
            .deck
            .usage
            .iter()
            .flat_map(|u| &u.groups)
            .map(|g| (g, g.usage.as_ref().map_or(0, |u| u.total)))
            .collect();
        groups.sort_by_key(|(g, t)| (std::cmp::Reverse(*t), std::cmp::Reverse(g.jobs)));
        let top = groups.first().map_or(1, |(_, t)| *t).max(1);
        let bar_w = inner_w.saturating_sub(26 + 18).clamp(4, 60);
        let routes_h = self.deck.fleet.as_ref().map_or(0, |f| f.machines.len() + 1);
        let room = (area.height as usize).saturating_sub(2 + routes_h);
        for (group, total) in groups.iter().take(room.max(1)) {
            // Keys are harness/provider/model; one model can run under two harnesses.
            let model = group.key.rsplit('/').next().unwrap_or(&group.key);
            let harness = group.key.split('/').next().unwrap_or("");
            let cells = if *total == 0 {
                0
            } else {
                ((*total as f64 / top as f64) * bar_w as f64).ceil() as usize
            };
            let (on, off) = if self.ascii { ("#", " ") } else { ("█", " ") };
            lines.push(Line::from(vec![
                Span::raw(format!("{:<17.17} ", clean(model, 17))),
                Span::styled(format!("{:<7.7} ", clean(harness, 7)), dim()),
                Span::styled(on.repeat(cells), Style::default().fg(Color::Magenta)),
                Span::raw(off.repeat(bar_w - cells.min(bar_w))),
                Span::styled(
                    format!(
                        " {:>5} · {}",
                        if group.usage.is_some() {
                            tokens(*total)
                        } else {
                            "—".into()
                        },
                        super::model::count(group.jobs as usize, "run", "runs")
                    ),
                    dim(),
                ),
            ]));
        }
        if let Some(fleet) = &self.deck.fleet {
            lines.push(Line::raw(""));
            let mut machines: Vec<_> = fleet.machines.iter().collect();
            machines.sort_by_key(|m| m.name != fleet.machine);
            for machine in machines {
                let mut spans = vec![Span::styled(
                    format!("{:<8.8} ", clean(&machine.name, 8)),
                    Style::default().fg(Color::Yellow),
                )];
                for route in &machine.routes {
                    let (glyph, color) = match route.state.as_str() {
                        "available" => (if self.ascii { "+" } else { "●" }, Color::Green),
                        "idle" => (if self.ascii { "o" } else { "◐" }, Color::Cyan),
                        _ => (if self.ascii { "x" } else { "○" }, Color::Red),
                    };
                    spans.push(Span::styled(glyph, Style::default().fg(color)));
                    spans.push(Span::raw(clean(&route.name, 24)));
                    if let Some(used) = &route.used {
                        spans.push(Span::styled(format!(" {:.0}%", used.share * 100.0), dim()));
                    }
                    spans.push(Span::raw("  "));
                }
                lines.push(Line::from(spans));
            }
        }
        frame.render_widget(
            Paragraph::new(lines).block(block("Models · 24 h", self.ascii)),
            area,
        );
    }

    fn machine_line(&self, name: &str, here: bool, now: i64) -> String {
        let Some(fleet) = &self.deck.fleet else {
            return String::new();
        };
        let Some(machine) = fleet.machines.iter().find(|m| m.name == name) else {
            return String::new();
        };
        let mut parts = Vec::new();
        if here {
            parts.push("here".to_owned());
            if let Some(r) = &self.deck.resources {
                let h = &r.host;
                parts.push(format!("load {:.1}/{}", h.load, h.cpus as u32));
                parts.push(format!("mem {} free", gib(h.memory_available_bytes as f64)));
                let falling = r
                    .disk_trend
                    .as_ref()
                    .map(|t| t.bytes_per_hour)
                    .filter(|b| *b > 50e6)
                    .map_or(String::new(), |b| format!(" ↓{}/h", gib(b)));
                parts.push(format!(
                    "disk {}{falling}",
                    gib(h.disk_available_bytes as f64)
                ));
                if !r.queue.is_empty() {
                    parts.push(format!("{} queued", r.queue.len()));
                }
            }
        } else if !machine.reachable {
            parts.push(match &machine.error {
                Some(e) => format!("unreachable · {}", clean(e, 60)),
                None => "unreachable".into(),
            });
        } else if let Some(at) = machine.as_of_ms {
            parts.push(match short_age(at, now).as_str() {
                "now" => "read just now".to_owned(),
                age => format!("read {age} ago"),
            });
        }
        if machine.stowed {
            parts.push("stowed".into());
        }
        parts.join(" · ")
    }

    fn draw_tree(&mut self, frame: &mut Frame, area: Rect, list_style: Style, now: i64) {
        let rows = self.tree();
        let working = self.working_ships();
        let here = self.deck.fleet.as_ref().map(|f| f.machine.clone());
        let strips: Vec<Option<[u32; BUCKETS]>> = rows
            .iter()
            .map(|row| match row {
                TreeRow::Ship {
                    machine,
                    name,
                    deck: Some(_),
                    ..
                } => Some(
                    self.deck
                        .launches(now, |e| &e.machine == machine && &e.ship == name),
                ),
                _ => None,
            })
            .collect();
        let top = strips
            .iter()
            .flatten()
            .flat_map(|s| s.iter().copied())
            .max()
            .unwrap_or(1);
        let block = block("Fleet · Enter opens a ship's space", self.ascii);
        let inner = block.inner(area);
        let show_strip = inner.width >= 64 && self.deck.fleet.is_some();
        let name_w = rows
            .iter()
            .filter_map(|r| match r {
                TreeRow::Ship { name, .. } => Some(name.chars().count() as u16),
                TreeRow::Machine { .. } => None,
            })
            .max()
            .unwrap_or(8)
            .clamp(8, 22)
            + 6;
        // Without a fleet reading a heading names its machine and when it was
        // reached; it may widen the column, up to half the width.
        let heading_w = if self.deck.fleet.is_none() {
            rows.iter()
                .filter_map(|r| match r {
                    TreeRow::Machine { name, .. } => Some(name.chars().count() as u16 + 2),
                    TreeRow::Ship { .. } => None,
                })
                .max()
                .unwrap_or(0)
                .min(inner.width / 2)
        } else {
            0
        };
        let name_w = name_w.max(heading_w);
        let table_rows: Vec<Row> = rows
            .iter()
            .zip(&strips)
            .map(|(row, strip_values)| match row {
                TreeRow::Machine {
                    name,
                    here: is_here,
                } => {
                    let reachable = self
                        .deck
                        .fleet
                        .as_ref()
                        .and_then(|f| f.machines.iter().find(|m| &m.name == name))
                        .is_none_or(|m| m.reachable);
                    // Without a fleet reading the heading is the snapshot's
                    // "name · detail" text.
                    let (head, detail) = match &self.deck.fleet {
                        Some(_) => (name.as_str(), self.machine_line(name, *is_here, now)),
                        None => (name.as_str(), String::new()),
                    };
                    let machine_strip = self
                        .deck
                        .fleet
                        .as_ref()
                        .map(|_| self.deck.launches(now, |e| &e.machine == name));
                    Row::new(vec![
                        Cell::from(Span::styled(
                            format!("{} {}", if self.ascii { "v" } else { "▾" }, clean(head, 80)),
                            Style::default()
                                .fg(if reachable { Color::Yellow } else { Color::Red })
                                .add_modifier(Modifier::BOLD),
                        )),
                        Cell::from(Span::styled(detail, dim())),
                        Cell::from(Line::from(machine_strip.map_or(vec![], |s| {
                            strip(&s, s.into_iter().max().unwrap_or(1), self.ascii)
                        }))),
                    ])
                }
                TreeRow::Ship {
                    machine,
                    name,
                    local,
                    ..
                } => {
                    let local_ship = local.map(|i| &self.snapshot.ships[i]);
                    let deck_ship = self.deck_ship(row);
                    let is_working = local_ship.is_some_and(|s| working.contains(&s.id));
                    let (glyph, color, doing, doing_style) =
                        self.ship_state(deck_ship, local_ship, is_working, now);
                    let mut name_spans = vec![
                        Span::raw("  "),
                        Span::styled(glyph, Style::default().fg(color)),
                        Span::raw(" "),
                        Span::styled(
                            overview::short_name(name, self.ascii),
                            if Some(machine) == here.as_ref() || local.is_some() {
                                Style::default()
                            } else {
                                Style::default().fg(Color::Gray)
                            },
                        ),
                    ];
                    if deck_ship.is_some_and(|s| s.liaison) || local_ship.is_some_and(|s| s.liaison)
                    {
                        name_spans.push(Span::styled(" ★", Style::default().fg(Color::Yellow)));
                    }
                    Row::new(vec![
                        Cell::from(Line::from(name_spans)),
                        Cell::from(Span::styled(doing, doing_style)),
                        Cell::from(Line::from(
                            strip_values.map_or(vec![], |s| strip(&s, top, self.ascii)),
                        )),
                    ])
                }
            })
            .collect();
        let widths = [
            Constraint::Length(name_w),
            Constraint::Fill(1),
            Constraint::Length(if show_strip { BUCKETS as u16 } else { 0 }),
        ];
        self.mouse.fleet = inner;
        let mut state = TableState::default().with_selected(self.tree_index(&rows));
        frame.render_stateful_widget(
            Table::new(table_rows, widths)
                .block(block)
                .column_spacing(1)
                .row_highlight_style(list_style),
            area,
            &mut state,
        );
        self.mouse.fleet_offset = state.offset();
    }

    /// Glyph, its colour, and the "what is it doing" text for a ship row.
    fn ship_state(
        &self,
        deck: Option<&DeckShip>,
        local: Option<&super::model::Ship>,
        working: bool,
        now: i64,
    ) -> (&'static str, Color, String, Style) {
        let a = self.ascii;
        if local
            .and_then(|s| s.live.as_ref())
            .is_some_and(|l| l.error.is_some())
        {
            return (
                if a { "x" } else { "✕" },
                Color::Red,
                "home unavailable".into(),
                Style::default().fg(Color::Red),
            );
        }
        let Some(ship) = deck else {
            let crew: Vec<&str> = local.map_or(vec![], |s| {
                self.snapshot
                    .jobs
                    .iter()
                    .filter(|j| j.owner == s.id && j.is_active())
                    .map(|j| j.task_id.as_str())
                    .collect()
            });
            let mut parts = Vec::new();
            match crew.as_slice() {
                [] => {}
                [one] => parts.push(format!("crew · {}", clean(one, 80))),
                [first, ..] => parts.push(format!("{} crew · {}", crew.len(), clean(first, 80))),
            }
            // Counts first: on a narrow tree the location note is what clips.
            parts.extend(local.map(|s| overview::summary(s, &self.snapshot)));
            if let Some(machine) = local.and_then(|s| s.remote.as_deref()) {
                parts.push(format!("on {} · not shared here", clean(machine, 40)));
            }
            let mut summary = parts.join(" · ");
            if a {
                summary = summary.replace('·', "-");
            }
            let (glyph, color) = if working || !crew.is_empty() {
                (if a { "@" } else { "◉" }, Color::Green)
            } else {
                (if a { "o" } else { "○" }, Color::DarkGray)
            };
            return (glyph, color, summary, dim());
        };
        let running: Vec<_> = ship.running().collect();
        let mut extras = Vec::new();
        if ship.open_requests > 0 {
            extras.push(format!("{} req", ship.open_requests));
        }
        if ship.unread > 0 {
            extras.push(format!("{} unread", ship.unread));
        }
        if ship.open_tasks_total > 0 {
            extras.push(super::model::count(ship.open_tasks_total, "task", "tasks"));
        }
        // Crew lead; jobs and errands follow, or lead when no crew runs.
        let crew: Vec<_> = running
            .iter()
            .filter(|j| j.noun() == Some("crew"))
            .collect();
        let others: Vec<_> = running
            .iter()
            .filter(|j| matches!(j.noun(), Some("job" | "errand")))
            .collect();
        if let Some(job) = crew.first().or(others.first()) {
            let title = clean(job.task_title.as_deref().unwrap_or(&job.task_id), 80);
            let noun = job.noun().unwrap_or("crew");
            let same = if crew.is_empty() {
                others.len()
            } else {
                crew.len()
            };
            let mut lead = match same {
                1 => noun.to_owned(),
                n => format!("{n} {noun}{}", if noun == "crew" { "" } else { "s" }),
            };
            if !crew.is_empty() && !others.is_empty() {
                lead.push_str(&format!(" +{} job", others.len()));
            }
            return (
                if a { "@" } else { "◉" },
                Color::Green,
                format!("{lead} · {title}"),
                Style::default().fg(Color::Green),
            );
        }
        let since = ship
            .presence_since_ms
            .map(|s| short_age(s, now))
            .filter(|age| age != "now")
            .map_or(String::new(), |age| format!(" {age}"));
        let (glyph, color, state) = match (working, ship.presence.as_str()) {
            (true, _) => (
                if a { "@" } else { "◉" },
                Color::Green,
                "working".to_owned(),
            ),
            (_, "hosted") => (
                if a { "*" } else { "●" },
                Color::Cyan,
                "on watch".to_owned(),
            ),
            (_, "asleep") => (
                if a { "o" } else { "○" },
                Color::DarkGray,
                format!("asleep{since}"),
            ),
            (_, "stowed") => (
                if a { "-" } else { "◌" },
                Color::DarkGray,
                "stowed".to_owned(),
            ),
            (_, other) => (if a { "o" } else { "○" }, Color::DarkGray, clean(other, 20)),
        };
        let text = std::iter::once(state)
            .chain(extras)
            .collect::<Vec<_>>()
            .join(" · ");
        (glyph, color, text, Style::default().fg(Color::Gray))
    }

    fn draw_needs(&mut self, frame: &mut Frame, area: Rect, list_style: Style, now: i64) {
        let queue = self.queue();
        let width = area.width.saturating_sub(2) as usize;
        let here = self.deck.fleet.as_ref().map(|f| f.machine.clone());
        let mut items: Vec<ListItem> = queue
            .iter()
            .map(|item| {
                let age = item.since_ms.map_or("  ".into(), |s| short_age(s, now));
                let ship = if Some(&item.machine) == here.as_ref() || item.machine.is_empty() {
                    clean(&item.ship, 30)
                } else {
                    format!("{}@{}", clean(&item.ship, 30), clean(&item.machine, 20))
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!(
                            "{} {age:>3} ",
                            need_glyph(&item.kind, item.hands, self.ascii)
                        ),
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::styled(
                        format!("{}{ship}", if item.stale { "STALE " } else { "" }),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(" {}", clean(&item.question, width.max(8)))),
                ]))
            })
            .collect();
        // Homes whose needs were not read: an empty list is no all-clear.
        let unseen = self.unseen_needs();
        if items.is_empty() {
            items.push(ListItem::new(Span::styled(
                if unseen.is_empty() && !self.attention.sources.is_empty() {
                    "Nothing waiting on you.".to_owned()
                } else if unseen.is_empty() {
                    "Runtime needs not observed; not an all-clear.".to_owned()
                } else {
                    format!(
                        "Not an all-clear: needs not observed for {}",
                        unseen.join(", ")
                    )
                },
                dim(),
            )));
        } else if !unseen.is_empty() {
            items.push(ListItem::new(Span::styled(
                format!("Not observed: {} (not an all-clear)", unseen.join(", ")),
                dim(),
            )));
        }
        let block = block(
            if area.width >= 60 {
                format!("Needs you · {} · Tab select · Enter open", queue.len())
            } else {
                format!("Needs you · {}", queue.len())
            },
            self.ascii,
        );
        self.mouse.list = block.inner(area);
        let selected = self
            .need_cursor
            .as_ref()
            .filter(|_| self.needs_focus)
            .and_then(|id| queue.iter().position(|i| &i.id == id));
        let mut state = ListState::default().with_selected(selected);
        frame.render_stateful_widget(
            List::new(items).block(block).highlight_style(list_style),
            area,
            &mut state,
        );
        self.mouse.list_offset = state.offset();
    }

    /// The selected need in brief, under the list; Enter opens it in full.
    fn need_preview(&self, width: u16, now: i64) -> (String, Vec<Line<'static>>) {
        let queue = self.queue();
        let Some(item) = self
            .need_cursor
            .as_ref()
            .and_then(|id| queue.iter().find(|i| &i.id == id))
        else {
            return (
                "Needs you".into(),
                vec![Line::styled("Select a need with ↑↓.", dim())],
            );
        };
        let mut lines = vec![Line::styled(
            format!(
                "{} · {}{}",
                clean(&item.kind, 20),
                item.since_ms.map_or(String::new(), |s| format!(
                    "asked {} ago",
                    short_age(s, now)
                )),
                item.task
                    .as_ref()
                    .map_or(String::new(), |t| format!(" · task {}", clean(t, 60)))
            ),
            dim(),
        )];
        lines.push(Line::raw(""));
        let body = item.body.as_deref().unwrap_or(&item.question);
        let room = (width as usize).saturating_sub(2).max(20);
        lines.extend(body.lines().take(12).map(|l| {
            let text = clean(l, 4096);
            Line::raw(if text.chars().count() > room * 3 {
                format!("{}…", text.chars().take(room * 3 - 1).collect::<String>())
            } else {
                text
            })
        }));
        for (i, option) in item.options.iter().enumerate() {
            lines.push(Line::from(vec![
                Span::styled(format!("{}  ", i + 1), dim()),
                Span::styled(
                    clean(&option.label, 120),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if option.recommended { " ★" } else { "" },
                    Style::default().fg(Color::Yellow),
                ),
            ]));
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Enter opens it to read in full and answer.",
            Style::default().fg(Color::Cyan),
        ));
        (
            format!("{}@{}", clean(&item.ship, 40), clean(&item.machine, 40)),
            lines,
        )
    }

    /// Details while answering, selecting a need or asked for (d); a ship's
    /// card while one is picked; otherwise the live feed.
    fn draw_lower(&mut self, frame: &mut Frame, area: Rect, now: i64) {
        self.mouse.details = area;
        let details = self.answer.is_some() || self.revoke.is_some() || self.diagnostics;
        let (title, lines, wrap) = if details {
            (
                if self.answer.is_some() {
                    "Answer"
                } else if self.diagnostics {
                    "Details · d to close"
                } else {
                    "Details"
                }
                .to_owned(),
                self.need_details(),
                true,
            )
        } else if self.needs_focus {
            let (title, mut lines) = self.need_preview(area.width, now);
            if !self.notice.is_empty() {
                lines.insert(
                    0,
                    Line::styled(clean(&self.notice, 512), Style::default().fg(Color::Yellow)),
                );
            }
            (title, lines, true)
        } else if let Some(row) = self
            .tree_index(&self.tree())
            .map(|i| self.tree()[i].clone())
        {
            let (title, lines) = self.ship_card(&row, area.width, now);
            (title, lines, false)
        } else {
            let mut lines = self.feed(None, area.width, now);
            if !self.notice.is_empty() {
                lines.insert(
                    0,
                    Line::styled(clean(&self.notice, 512), Style::default().fg(Color::Yellow)),
                );
            }
            ("Live".to_owned(), lines, false)
        };
        // Details wrap; feed lines are one event each and clip at the edge.
        let mut paragraph = Paragraph::new(lines)
            .block(block(title, self.ascii))
            .scroll((self.scroll, 0));
        if wrap {
            paragraph = paragraph.wrap(Wrap { trim: false });
        }
        frame.render_widget(paragraph, area);
    }

    /// Recorded events, newest first, for one ship or the fleet.
    fn feed(&self, ship: Option<(&str, &str)>, width: u16, now: i64) -> Vec<Line<'static>> {
        let fleet = self.deck.fleet.as_ref();
        let multi = fleet.is_some_and(|f| f.machines.len() > 1) && ship.is_none() && width >= 56;
        let mut entries: Vec<(i64, String, String, &'static str, Color, String)> = self
            .deck
            .events
            .iter()
            .filter(|e| ship.is_none_or(|(m, s)| e.machine == m && e.ship == s))
            .filter_map(|e| {
                let (glyph, color, text) = describe(e, fleet, &self.snapshot)?;
                Some((
                    e.at_ms,
                    e.machine.clone(),
                    e.ship.clone(),
                    glyph,
                    color,
                    text,
                ))
            })
            .collect();
        // Completions recorded in local homes, unless the feed already has them.
        let here = fleet.map_or(String::new(), |f| f.machine.clone());
        for recent in &self.snapshot.recent {
            let owner = self.ship_name(&recent.owner);
            if ship.is_some_and(|(m, s)| m != here || (s != owner && s != recent.owner)) {
                continue;
            }
            let seen = self.deck.events.iter().any(|e| {
                e.kind == "task.update" && e.task.as_deref() == Some(recent.task_id.as_str())
            });
            if !seen && now - recent.completed_ms < super::deck::WINDOW_MS {
                entries.push((
                    recent.completed_ms,
                    here.clone(),
                    owner,
                    "✔",
                    Color::Green,
                    format!("done · {}", clean(&recent.title, 120)),
                ));
            }
        }
        entries.sort_by_key(|e| std::cmp::Reverse(e.0));
        let lines: Vec<Line> = entries
            .into_iter()
            .take(200)
            .map(|(at, machine, ship_name, glyph, color, text)| {
                let mut spans = vec![Span::styled(format!("{:>3} ", short_age(at, now)), dim())];
                if multi {
                    spans.push(Span::styled(format!("{:<8.8}", clean(&machine, 8)), dim()));
                }
                if ship.is_none() {
                    spans.push(Span::styled(
                        format!("{:<13.13}", clean(&ship_name, 13)),
                        Style::default().add_modifier(Modifier::BOLD),
                    ));
                }
                let glyph = if self.ascii { plain(glyph) } else { glyph };
                spans.push(Span::styled(
                    format!("{glyph} "),
                    Style::default().fg(color),
                ));
                spans.push(Span::raw(text));
                Line::from(spans)
            })
            .collect();
        if lines.is_empty() {
            return vec![Line::styled(
                if self.deck.caught_up {
                    "No recorded events in the last 12 hours."
                } else {
                    "Reading the last 12 hours of fleet activity…"
                },
                dim(),
            )];
        }
        lines
    }

    fn ship_card(&self, row: &TreeRow, width: u16, now: i64) -> (String, Vec<Line<'static>>) {
        let TreeRow::Ship {
            machine,
            name,
            local,
            ..
        } = row
        else {
            return ("Live".into(), vec![]);
        };
        let mut lines = Vec::new();
        let label = |text: &str| Span::styled(format!("{text:<9}"), dim());
        if let Some(ship) = self.deck_ship(row) {
            let since = ship
                .presence_since_ms
                .map_or(String::new(), |s| format!(" for {}", short_age(s, now)));
            lines.push(Line::from(vec![
                label("state"),
                Span::raw(format!(
                    "{}{since} · {} · {}",
                    clean(&ship.presence, 20),
                    clean(&ship.harness, 20),
                    if ship.hosted {
                        "hosted"
                    } else {
                        "woken on demand"
                    }
                )),
            ]));
            lines.push(Line::from(vec![
                label("work"),
                Span::raw(format!(
                    "{} · {} to the captain · {} unread",
                    super::model::count(ship.open_tasks_total, "open task", "open tasks"),
                    ship.open_requests,
                    ship.unread
                )),
            ]));
            for job in ship.running().filter(|j| j.noun().is_some()) {
                lines.push(Line::from(vec![
                    label(job.noun().unwrap_or("crew")),
                    Span::styled(
                        format!(
                            "{}{}{}",
                            clean(job.task_title.as_deref().unwrap_or(&job.task_id), 120),
                            if job.heavy { " · heavy" } else { "" },
                            job.started_ms
                                .map_or(String::new(), |s| format!(" · {}", short_age(s, now)))
                        ),
                        Style::default().fg(Color::Green),
                    ),
                ]));
            }
        }
        let local_ship = local.map(|i| &self.snapshot.ships[i]);
        if let Some(ship) = local_ship {
            if !ship.description.is_empty() {
                lines.push(Line::from(vec![
                    label("scope"),
                    Span::raw(clean(&ship.description, 512)),
                ]));
            }
            // The home path stays in this card, never the lists, so a
            // same-named ship in another home can still be told apart.
            if let Some(live) = &ship.live {
                lines.push(Line::from(vec![
                    label("home"),
                    Span::styled(clean(&live.home.display().to_string(), 512), dim()),
                ]));
                if let Some(error) = &live.error {
                    lines.push(Line::styled(
                        format!("Unavailable: {}", clean(error, 512)),
                        Style::default().fg(Color::Red),
                    ));
                }
            }
            self.push_ship_work(&mut lines, ship);
        }
        if self.binding_ambiguous() {
            lines.push(Line::styled(
                "Ambiguous pane ownership; focus disabled. Inspect exact bindings.",
                Style::default().fg(Color::Red),
            ));
        }
        lines.push(Line::styled(
            if local_ship.is_some_and(|s| s.remote.is_none()) {
                "Enter opens its space here.".to_owned()
            } else {
                format!(
                    "Lives on {}; open its space on that machine.",
                    clean(machine, 60)
                )
            },
            dim(),
        ));
        if !self.notice.is_empty() {
            lines.insert(
                0,
                Line::styled(clean(&self.notice, 512), Style::default().fg(Color::Yellow)),
            );
        }
        lines.push(Line::raw(""));
        lines.extend(self.feed(Some((machine, name)), width, now));
        (
            format!("{} · {}", clean(name, 40), clean(machine, 40)),
            lines,
        )
    }
}

/// The deck's own status for the footer: its age and the first error.
pub fn status(deck: &Deck, age: Option<u64>) -> String {
    let mut text = match age {
        Some(s) => format!("Fleet {s}s ago"),
        None => "Fleet not read yet".into(),
    };
    if let Some(error) = deck.errors.first() {
        text.push_str(&format!(" · FLEET PARTIAL: {}", clean(error, 160)));
    }
    text
}

/// Right-aligned clock for the header row.
pub fn clock_widget(text: String) -> Paragraph<'static> {
    Paragraph::new(Span::styled(format!("{text} "), dim())).alignment(Alignment::Right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::{deck::DeckShip, model::Ship};

    fn fleet() -> FleetView {
        let machine = |name: &str, link: &str| super::super::deck::Machine {
            name: name.into(),
            link: link.into(),
            reachable: true,
            ..Default::default()
        };
        let ship = |machine: &str, name: &str| DeckShip {
            machine: machine.into(),
            name: name.into(),
            ..Default::default()
        };
        FleetView {
            machine: "laptop".into(),
            machines: vec![machine("mac", "reaches"), machine("laptop", "here")],
            ships: vec![
                ship("mac", "ios"),
                ship("laptop", "web"),
                ship("laptop", "api"),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn tree_puts_this_machine_first_and_walks_every_ship_but_opens_only_local_ones() {
        let snapshot = Snapshot {
            ships: vec![Ship {
                id: "web".into(),
                name: "web".into(),
                ..Ship::default()
            }],
            ..Snapshot::default()
        };
        let mut app = App {
            snapshot,
            ..App::default()
        };
        app.deck.fleet = Some(fleet());
        let rows = app.tree();
        let names: Vec<_> = rows
            .iter()
            .map(|r| match r {
                TreeRow::Machine { name, .. } | TreeRow::Ship { name, .. } => name.as_str(),
            })
            .collect();
        assert_eq!(names, ["laptop", "api", "web", "mac", "ios"]);

        // Down walks ships only, across machines, and stops at the last.
        app.move_in_tree(true);
        assert_eq!(
            app.picked_elsewhere(),
            Some(&("laptop".into(), "api".into()))
        );
        app.move_in_tree(true);
        assert_eq!(app.ship.as_deref(), Some("web"));
        assert!(app.picked_elsewhere().is_none());
        app.move_in_tree(true);
        app.move_in_tree(true);
        assert_eq!(app.picked_elsewhere(), Some(&("mac".into(), "ios".into())));
        // Enter on a ship on another machine explains instead of focusing.
        assert!(!app.bridge_key(crossterm::event::KeyCode::Enter));
        assert!(app.notice.contains("mac"), "{}", app.notice);
        app.move_in_tree(false);
        assert_eq!(app.ship.as_deref(), Some("web"));
    }
}
