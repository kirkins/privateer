//! Voyage: an optional animated sea of the fleet, opened with `v`. The scene is
//! a pure function of the snapshot, elapsed time and size; drawing paints the
//! water and then exactly the sprites the scene returns. Nothing here reads
//! state or changes it, and nothing is drawn while the screen is closed.
use super::{
    boat,
    model::{Ship, Snapshot, clean},
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
};
use std::time::Duration;

/// One bottle crossing from the liaison to a recipient, in milliseconds.
const TRIP_MS: i64 = 6_000;
/// A boat moves one cell sideways every SWAY_MS; the water changes every WAVE_MS.
const SWAY_MS: u128 = 1_000;
const WAVE_MS: u128 = 250;
/// Trailing crew boats shown per ship before "+n".
const CREW_BOATS: usize = 3;
/// Column of the first trailing crew boat, relative to the ship's slot.
const CREW_AT: usize = 6;
const CREW_STEP: usize = 3;
const WATER: [char; 8] = ['▁', '▁', '▂', '▂', '▁', '▁', '▁', '▁'];

/// How much of each ship is shown; chosen by the largest that fits every ship.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Sailboat, trailing crew and a name under it.
    Names,
    /// Sailboat and trailing crew; names dropped.
    Boats,
    /// One symbol per ship; crew are not drawn.
    Symbols,
}

impl Mode {
    /// Cell size of one ship's slot in this mode.
    fn slot(self) -> (usize, usize) {
        match self {
            Self::Names => (20, 3),
            Self::Boats => (18, 2),
            Self::Symbols => (1, 1),
        }
    }

    fn capacity(self, width: usize, height: usize) -> usize {
        let (sw, sh) = self.slot();
        (width / sw) * (height / sh)
    }
}

/// The two rows of one ship's sprite. Each sail state has its own shape, so the
/// state reads without colour and without motion.
fn hull(sail: Sail, ascii: bool) -> [&'static str; 2] {
    match (sail, ascii) {
        (Sail::Up, _) => boat::art(ascii),
        (Sail::Anchored, false) => [" ┃  ", "╲⊥▁▁╱"],
        (Sail::Anchored, true) => ["  |  ", "\\T__/"],
        (Sail::Docked, false) => ["     ", "▕╲▁▁▏"],
        (Sail::Docked, true) => ["     ", "|\\__|"],
        (Sail::Error, false) => ["  ! ", "╲✕▁╱"],
        (Sail::Error, true) => ["  ! ", "\\x_/"],
        (Sail::Unknown, false) => ["  ? ", "╲▁▁▁╱"],
        (Sail::Unknown, true) => ["  ? ", "\\___/"],
    }
}

/// Sail state, from recorded facts only. Unknown is never drawn as an all-clear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sail {
    /// The first mate is working or the captain is aboard.
    Up,
    /// Idle and dispatching.
    Anchored,
    /// The dispatcher is stopped.
    Docked,
    /// The ship's home reports an error.
    Error,
    /// No live state for this ship.
    Unknown,
}

pub fn sail(ship: &Ship) -> Sail {
    match &ship.live {
        None => Sail::Unknown,
        Some(live) if live.error.is_some() => Sail::Error,
        Some(live) if !live.dispatcher => Sail::Docked,
        Some(live) if live.attended || live.wakes.iter().any(|w| w.status == "running") => Sail::Up,
        Some(_) => Sail::Anchored,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mark {
    Boat {
        ship: String,
        sail: Sail,
    },
    Name(String),
    Crew {
        ship: String,
    },
    CrewMore {
        ship: String,
        extra: usize,
    },
    Flag {
        ship: String,
    },
    Bottle {
        to: String,
    },
    /// Ships not drawn; they are counted here.
    More(usize),
}

/// A sprite: `rows` drawn top to bottom from (`x`, `y`) in the sea's cells.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    pub x: usize,
    pub y: usize,
    pub rows: Vec<String>,
    pub mark: Mark,
}

impl Placed {
    #[cfg(test)]
    fn width(&self) -> usize {
        self.rows
            .iter()
            .map(|r| r.chars().count())
            .max()
            .unwrap_or(0)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Scene {
    pub mode: Mode,
    pub placed: Vec<Placed>,
    /// Ships not drawn; the same count is on the `Mark::More` sprite.
    pub omitted: usize,
}

/// Liaison first, then by id, so every ship keeps the same slot between refreshes.
fn ordered(snapshot: &Snapshot) -> Vec<&Ship> {
    let mut ships: Vec<_> = snapshot.ships.iter().collect();
    ships.sort_by(|a, b| b.liaison.cmp(&a.liaison).then_with(|| a.id.cmp(&b.id)));
    ships
}

fn clip(text: &str, cells: usize) -> String {
    text.chars().take(cells).collect()
}

fn symbol(sail: Sail, ascii: bool) -> &'static str {
    match (sail, ascii) {
        (Sail::Up, false) => "●",
        (Sail::Up, true) => "*",
        (Sail::Anchored, false) => "○",
        (Sail::Anchored, true) => "o",
        (Sail::Docked, false) => "▪",
        (Sail::Docked, true) => "-",
        (Sail::Error, false) => "✕",
        (Sail::Error, true) => "x",
        (Sail::Unknown, _) => "?",
    }
}

fn flag(ascii: bool) -> &'static str {
    if ascii { "F" } else { "⚑" }
}

fn bottle(ascii: bool) -> &'static str {
    if ascii { "%" } else { "◊" }
}

/// Water for one cell: a pure function of position and phase.
fn wave(x: usize, y: usize, phase: usize, ascii: bool) -> char {
    if ascii {
        '~'
    } else {
        WATER[(x + 3 * y + phase) % WATER.len()]
    }
}

pub fn scene(
    snapshot: &Snapshot,
    elapsed: Duration,
    width: usize,
    height: usize,
    reduced: bool,
    ascii: bool,
) -> Scene {
    let ships = ordered(snapshot);
    let total = ships.len();
    let mode = [Mode::Names, Mode::Boats, Mode::Symbols]
        .into_iter()
        .find(|m| m.capacity(width, height) >= total)
        .unwrap_or(Mode::Symbols);
    let (sw, sh) = mode.slot();
    let cols = width / sw;
    // When ships overflow, the last grid row is kept for the "+n more ships" line.
    let grid_rows = height / sh;
    let shown = if total <= mode.capacity(width, height) {
        total
    } else {
        cols * grid_rows.saturating_sub(1)
    };
    let t = elapsed.as_millis();
    let slot = |i: usize| ((i % cols) * sw, (i / cols) * sh);
    let mut placed = Vec::new();
    // Slot origin of each drawn ship, for bottles.
    let mut origins: Vec<(&str, usize, usize)> = Vec::new();

    for (i, ship) in ships.iter().take(shown).enumerate() {
        let (x, y) = slot(i);
        origins.push((ship.id.as_str(), x, y));
        let id = ship.id.clone();
        let sail = sail(ship);
        let flagged = snapshot.pending.iter().any(|n| n.owner == ship.id);
        if mode == Mode::Symbols {
            let text = if flagged {
                flag(ascii)
            } else {
                symbol(sail, ascii)
            };
            placed.push(Placed {
                x,
                y,
                rows: vec![text.into()],
                mark: Mark::Boat { ship: id, sail },
            });
            continue;
        }
        // Docked and unknown boats stay still; the rest sway out of step with each other.
        let sways = !reduced && !matches!(sail, Sail::Docked | Sail::Unknown);
        let offset = if sways {
            ((t / SWAY_MS) as usize + i) % 2
        } else {
            0
        };
        placed.push(Placed {
            x: x + offset,
            y,
            rows: hull(sail, ascii).iter().map(|r| (*r).into()).collect(),
            mark: Mark::Boat {
                ship: id.clone(),
                sail,
            },
        });
        let crew = snapshot
            .jobs
            .iter()
            .filter(|j| j.owner == ship.id && j.is_active())
            .count();
        for k in 0..crew.min(CREW_BOATS) {
            placed.push(Placed {
                x: x + CREW_AT + CREW_STEP * k,
                y: y + 1,
                rows: vec![boat::small(ascii).into()],
                mark: Mark::Crew { ship: id.clone() },
            });
        }
        if crew > CREW_BOATS {
            let extra = crew - CREW_BOATS;
            let at = CREW_AT + CREW_STEP * CREW_BOATS;
            placed.push(Placed {
                x: x + at,
                y: y + 1,
                rows: vec![clip(&format!("+{extra}"), sw - at)],
                mark: Mark::CrewMore {
                    ship: id.clone(),
                    extra,
                },
            });
        }
        if flagged {
            placed.push(Placed {
                x: x + sw - 1,
                y,
                rows: vec![flag(ascii).into()],
                mark: Mark::Flag { ship: id.clone() },
            });
        }
        if mode == Mode::Names {
            let name = clean(
                if ship.name.is_empty() {
                    &ship.id
                } else {
                    &ship.name
                },
                256,
            );
            placed.push(Placed {
                x,
                y: y + 2,
                rows: vec![clip(&name, sw - 1)],
                mark: Mark::Name(name),
            });
        }
    }

    let omitted = total - shown;
    if omitted > 0 && grid_rows > 0 {
        placed.push(Placed {
            x: 0,
            y: (grid_rows - 1) * sh,
            rows: vec![clip(&format!("+{omitted} more ships"), width)],
            mark: Mark::More(omitted),
        });
    }

    // Open requests drift as a bottle from the liaison (the first slot) toward
    // each drawn ship that has them. The sender is approximated: the snapshot
    // records a count per ship, not who sent each request. Symbol mode draws
    // no bottles, so they never cover a ship's symbol. Reduced motion holds
    // them halfway.
    let sender = origins
        .first()
        .filter(|_| mode != Mode::Symbols && ships.first().is_some_and(|s| s.liaison));
    if let Some(&(_, fx, fy)) = sender {
        let p = if reduced {
            TRIP_MS / 2
        } else {
            (t % TRIP_MS as u128) as i64
        };
        // Progress is a pure function of time; integer rounding keeps it monotone.
        let at = |from: usize, to: usize| {
            (from as i64 + (to as i64 - from as i64) * p / TRIP_MS) as usize
        };
        for (ship, &(_, tx, ty)) in ships.iter().zip(&origins).skip(1) {
            if ship.live.as_ref().map_or(0, |l| l.requests) == 0 {
                continue;
            }
            placed.push(Placed {
                x: at(fx, tx),
                y: at(fy, ty),
                rows: vec![bottle(ascii).into()],
                mark: Mark::Bottle {
                    to: ship.id.clone(),
                },
            });
        }
    }

    Scene {
        mode,
        placed,
        omitted,
    }
}

fn style(mark: &Mark) -> Style {
    match mark {
        Mark::Boat { sail, .. } => Style::default().fg(match sail {
            Sail::Up => Color::Green,
            Sail::Anchored => Color::Cyan,
            Sail::Docked => Color::Gray,
            Sail::Error => Color::Red,
            Sail::Unknown => Color::DarkGray,
        }),
        Mark::Name(_) => Style::default().fg(Color::Gray),
        Mark::Crew { .. } | Mark::CrewMore { .. } => Style::default().fg(Color::Yellow),
        Mark::Flag { .. } => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        Mark::Bottle { .. } => Style::default().fg(Color::Magenta),
        Mark::More(_) => Style::default().fg(Color::DarkGray),
    }
}

/// Paints the water, then the scene's sprites, inside `area` only.
pub fn draw(
    frame: &mut Frame,
    area: Rect,
    snapshot: &Snapshot,
    elapsed: Duration,
    reduced: bool,
    ascii: bool,
) {
    let scene = scene(
        snapshot,
        elapsed,
        area.width as usize,
        area.height as usize,
        reduced,
        ascii,
    );
    let phase = if reduced {
        0
    } else {
        (elapsed.as_millis() / WAVE_MS) as usize
    };
    let buf = frame.buffer_mut();
    let water = Style::default().fg(Color::Blue);
    for dy in 0..area.height as usize {
        let row: String = (0..area.width as usize)
            .map(|dx| wave(dx, dy, phase, ascii))
            .collect();
        buf.set_string(area.x, area.y + dy as u16, row, water);
    }
    for p in &scene.placed {
        for (dy, text) in p.rows.iter().enumerate() {
            buf.set_string(
                area.x + p.x as u16,
                area.y + (p.y + dy) as u16,
                text,
                style(&p.mark),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::{
        fleet::{ShipState, Wake},
        model::{Job, Notice, Snapshot},
    };
    use ratatui::{Terminal, backend::TestBackend};
    use std::path::PathBuf;

    fn live(requests: usize) -> ShipState {
        ShipState {
            home: PathBuf::from("/nonexistent/home"),
            identity: None,
            liaison: false,
            binding: None,
            harness: "test".into(),
            attended: false,
            dispatcher: true,
            requests,
            unread: 0,
            wakes: Vec::new(),
            error: None,
        }
    }

    fn ship(id: &str, liaison: bool, state: Option<ShipState>) -> Ship {
        Ship {
            id: id.into(),
            name: id.into(),
            description: String::new(),
            liaison,
            terminals: Vec::new(),
            live: state,
        }
    }

    fn job(owner: &str, status: &str) -> Job {
        Job {
            home: None,
            owner: owner.into(),
            id: format!("{owner}-{status}"),
            task_id: "t".into(),
            status: status.into(),
        }
    }

    fn fleet(count: usize) -> Snapshot {
        let mut snapshot = Snapshot::default();
        if count == 0 {
            return snapshot;
        }
        snapshot.ships.push(ship("liaison", true, Some(live(0))));
        for i in 0..count - 1 {
            snapshot
                .ships
                .push(ship(&format!("ship-{i:02}"), false, Some(live(0))));
        }
        snapshot
    }

    fn boats(scene: &Scene) -> Vec<String> {
        scene
            .placed
            .iter()
            .filter_map(|p| match &p.mark {
                Mark::Boat { ship, .. } => Some(ship.clone()),
                _ => None,
            })
            .collect()
    }

    fn inside(scene: &Scene, width: usize, height: usize) -> bool {
        scene
            .placed
            .iter()
            .all(|p| p.x + p.width() <= width && p.y + p.rows.len() <= height)
    }

    #[test]
    fn every_ship_is_drawn_once_or_counted() {
        for count in [0, 1, 3, 7, 12, 40] {
            let snapshot = fleet(count);
            for (w, h) in [(40, 14), (60, 20), (100, 30), (300, 100)] {
                let scene = scene(&snapshot, Duration::from_secs(3), w, h, false, false);
                let drawn = boats(&scene);
                let mut unique = drawn.clone();
                unique.sort();
                unique.dedup();
                assert_eq!(unique.len(), drawn.len(), "a ship was drawn twice");
                assert_eq!(drawn.len() + scene.omitted, count, "{w}x{h} with {count}");
                if scene.omitted > 0 {
                    assert!(
                        scene
                            .placed
                            .iter()
                            .any(|p| p.mark == Mark::More(scene.omitted))
                    );
                }
            }
        }
    }

    #[test]
    fn liaison_is_drawn_first_and_order_is_stable() {
        let snapshot = fleet(5);
        let first = scene(&snapshot, Duration::ZERO, 60, 20, false, false);
        let later = scene(&snapshot, Duration::from_secs(9), 60, 20, true, false);
        assert_eq!(boats(&first)[0], "liaison");
        assert_eq!(boats(&first), boats(&later));
    }

    #[test]
    fn crew_boats_trail_running_jobs_then_plus_n() {
        for active in 0..=6 {
            let mut snapshot = fleet(1);
            for _ in 0..active {
                snapshot.jobs.push(job("liaison", "running"));
            }
            snapshot.jobs.push(job("liaison", "succeeded"));
            let scene = scene(&snapshot, Duration::ZERO, 100, 30, false, false);
            let crew = scene
                .placed
                .iter()
                .filter(|p| matches!(p.mark, Mark::Crew { .. }))
                .count();
            let extra: usize = scene
                .placed
                .iter()
                .map(|p| match p.mark {
                    Mark::CrewMore { extra, .. } => extra,
                    _ => 0,
                })
                .sum();
            assert_eq!(crew + extra, active);
            assert!(crew <= CREW_BOATS);
        }
    }

    #[test]
    fn a_bottle_moves_monotonically_from_liaison_to_recipient() {
        let mut snapshot = fleet(1);
        snapshot.ships.push(ship("reader", false, Some(live(2))));
        let at = |ms: u64| {
            scene(&snapshot, Duration::from_millis(ms), 100, 30, false, false)
                .placed
                .into_iter()
                .find(|p| matches!(&p.mark, Mark::Bottle { to } if to == "reader"))
                .map(|p| (p.x, p.y))
                .expect("bottle drawn")
        };
        let from = at(0);
        let to = scene(&snapshot, Duration::ZERO, 100, 30, false, false)
            .placed
            .into_iter()
            .find(|p| matches!(&p.mark, Mark::Boat { ship, .. } if ship == "reader"))
            .map(|p| (p.x, p.y))
            .unwrap();
        let mut last = from;
        for ms in (0..TRIP_MS as u64).step_by(50) {
            let here = at(ms);
            assert!(
                here.0 >= last.0 && here.1 >= last.1,
                "moved backwards at {ms}"
            );
            last = here;
        }
        assert!(last.0 <= to.0 && last.1 <= to.1);
        assert!(last != from);
    }

    #[test]
    fn requests_and_flags_are_per_ship_not_per_sprite() {
        let mut snapshot = fleet(2);
        snapshot.ships.push(ship("reader", false, Some(live(4))));
        snapshot.pending.push(Notice {
            owner: "reader".into(),
            ..Notice::default()
        });
        let scene = scene(&snapshot, Duration::ZERO, 100, 30, false, false);
        let bottles = scene
            .placed
            .iter()
            .filter(|p| matches!(p.mark, Mark::Bottle { .. }))
            .count();
        let flags = scene
            .placed
            .iter()
            .filter(|p| matches!(p.mark, Mark::Flag { .. }))
            .count();
        assert_eq!((bottles, flags), (1, 1));
    }

    #[test]
    fn narrow_terminals_drop_names_then_show_symbols() {
        let snapshot = fleet(10);
        assert_eq!(
            scene(&snapshot, Duration::ZERO, 120, 40, false, false).mode,
            Mode::Names
        );
        let narrow = scene(&snapshot, Duration::ZERO, 40, 14, false, false);
        assert_eq!(narrow.mode, Mode::Boats);
        assert!(
            !narrow
                .placed
                .iter()
                .any(|p| matches!(p.mark, Mark::Name(_)))
        );
        assert_eq!(
            scene(&snapshot, Duration::ZERO, 20, 6, false, false).mode,
            Mode::Symbols
        );
    }

    #[test]
    fn nothing_is_placed_outside_the_area() {
        for count in [1, 5, 30] {
            let snapshot = fleet(count);
            for w in (40..=300).step_by(13) {
                for h in (14..=100).step_by(9) {
                    let scene = scene(&snapshot, Duration::from_millis(1234), w, h, false, false);
                    assert!(inside(&scene, w, h), "{w}x{h} with {count}");
                }
            }
        }
    }

    #[test]
    fn the_same_inputs_give_the_same_scene() {
        let snapshot = fleet(9);
        let a = scene(&snapshot, Duration::from_millis(777), 90, 25, false, true);
        let b = scene(&snapshot, Duration::from_millis(777), 90, 25, false, true);
        assert_eq!(a, b);
    }

    #[test]
    fn reduced_motion_holds_the_scene_still() {
        let mut snapshot = fleet(4);
        snapshot.ships.push(ship("reader", false, Some(live(2))));
        snapshot.jobs.push(job("ship-00", "running"));
        for (w, h) in [(80, 24), (40, 14)] {
            let still = scene(&snapshot, Duration::ZERO, w, h, true, false);
            assert!(
                still
                    .placed
                    .iter()
                    .any(|p| matches!(p.mark, Mark::Bottle { .. }))
            );
            for ms in [1_000, 2_500, 5_000, 9_999] {
                let later = scene(&snapshot, Duration::from_millis(ms), w, h, true, false);
                assert_eq!(still.placed, later.placed, "{w}x{h} at {ms}ms");
            }
        }
    }

    #[test]
    fn each_sail_state_has_its_own_text_shape() {
        let states = [
            Sail::Up,
            Sail::Anchored,
            Sail::Docked,
            Sail::Error,
            Sail::Unknown,
        ];
        for ascii in [false, true] {
            for (i, a) in states.iter().enumerate() {
                for b in &states[i + 1..] {
                    assert_ne!(hull(*a, ascii), hull(*b, ascii), "{a:?} vs {b:?}");
                }
            }
            let error = hull(Sail::Error, ascii).join("");
            assert!(error.contains('!'), "{error}");
            assert!(
                hull(Sail::Anchored, ascii)
                    .join("")
                    .contains(if ascii { 'T' } else { '⊥' })
            );
        }
        for sprite in states.iter().flat_map(|s| hull(*s, true)) {
            assert!(sprite.is_ascii(), "{sprite}");
        }
    }

    #[test]
    fn symbol_mode_draws_no_bottles() {
        let mut snapshot = fleet(3);
        snapshot.ships.push(ship("reader", false, Some(live(2))));
        let symbols = scene(&snapshot, Duration::from_millis(3_000), 10, 4, false, false);
        assert_eq!(symbols.mode, Mode::Symbols);
        assert!(
            !symbols
                .placed
                .iter()
                .any(|p| matches!(p.mark, Mark::Bottle { .. }))
        );
        let boats = scene(
            &snapshot,
            Duration::from_millis(3_000),
            60,
            20,
            false,
            false,
        );
        assert_ne!(boats.mode, Mode::Symbols);
        assert!(
            boats
                .placed
                .iter()
                .any(|p| matches!(p.mark, Mark::Bottle { .. }))
        );
    }

    #[test]
    fn overflow_count_is_written_out_in_full() {
        for (w, h) in [(40, 14), (60, 20), (20, 6)] {
            // Twice the cells, so every mode overflows.
            let snapshot = fleet(2 * w * h);
            let scene = scene(&snapshot, Duration::ZERO, w, h, false, false);
            assert!(scene.omitted > 0, "{w}x{h}");
            let text = format!("+{} more ships", scene.omitted);
            let line = scene
                .placed
                .iter()
                .find(|p| p.mark == Mark::More(scene.omitted))
                .expect("count drawn");
            assert_eq!(line.rows, vec![text], "{w}x{h}");
            assert_eq!(line.y, (h / scene.mode.slot().1 - 1) * scene.mode.slot().1);
        }
    }

    #[test]
    fn names_stop_one_cell_short_of_the_next_slot() {
        let mut snapshot = Snapshot::default();
        for i in 0..4 {
            snapshot.ships.push(ship(
                &format!("ship-{i}-with-a-very-long-name-indeed"),
                i == 0,
                Some(live(0)),
            ));
        }
        let scene = scene(&snapshot, Duration::ZERO, 100, 30, false, false);
        assert_eq!(scene.mode, Mode::Names);
        let names: Vec<_> = scene
            .placed
            .iter()
            .filter(|p| matches!(p.mark, Mark::Name(_)))
            .collect();
        assert_eq!(names.len(), 4);
        for name in names {
            assert!(name.width() < 20, "{:?}", name.rows);
        }
    }

    #[test]
    fn sail_follows_recorded_state() {
        let mut state = live(0);
        assert_eq!(sail(&ship("a", false, None)), Sail::Unknown);
        assert_eq!(sail(&ship("a", false, Some(state.clone()))), Sail::Anchored);
        state.wakes.push(Wake {
            mode: "run".into(),
            status: "running".into(),
        });
        assert_eq!(sail(&ship("a", false, Some(state.clone()))), Sail::Up);
        state.dispatcher = false;
        assert_eq!(sail(&ship("a", false, Some(state.clone()))), Sail::Docked);
        state.error = Some("missing".into());
        assert_eq!(sail(&ship("a", false, Some(state))), Sail::Error);
    }

    #[test]
    fn sea_never_panics_and_stays_in_bounds_from_40x14_to_300x100() {
        let snapshot = fleet(12);
        for w in 40..=300 {
            for h in 14..=100 {
                let scene = scene(&snapshot, Duration::from_millis(4321), w, h, false, false);
                assert!(inside(&scene, w, h), "{w}x{h}");
                assert_eq!(boats(&scene).len() + scene.omitted, 12);
            }
        }
    }

    #[test]
    fn the_sea_renders_at_coarse_sizes_without_panicking() {
        let snapshot = fleet(8);
        for w in (40..=300).step_by(37) {
            for h in (14..=100).step_by(29) {
                let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
                terminal
                    .draw(|frame| {
                        let area = frame.area();
                        draw(
                            frame,
                            area,
                            &snapshot,
                            Duration::from_millis(900),
                            false,
                            true,
                        );
                    })
                    .unwrap();
            }
        }
    }
}
