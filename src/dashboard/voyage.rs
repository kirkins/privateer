//! Voyage: an optional animated sea of the fleet, opened with `v`. The scene is
//! a pure function of the snapshot, the ships Herdr reports working, elapsed
//! time and size; drawing paints the water and then exactly the sprites the
//! scene returns. Nothing here reads state or changes it, and nothing is drawn
//! while the screen is closed.
//!
//! Each row of ships is a band: open sky behind the sails, a waterline under
//! the hulls and a calm line for names. This machine's ships come first, then
//! each linked machine's under its own heading. Spare rows become open water
//! between the bands, and the bottom row is a key to what is on screen.
use super::{
    boat,
    model::{Ship, Snapshot, clean},
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

/// One bottle crossing from the liaison to a recipient, in milliseconds.
const TRIP_MS: i64 = 6_000;
/// A boat moves one cell sideways and the open-water swell drifts one cell
/// every SWAY_MS; the waterline changes every WAVE_MS.
const SWAY_MS: u128 = 1_000;
const WAVE_MS: u128 = 250;
/// Trailing crew boats shown per ship before "+n".
const CREW_BOATS: usize = 3;
/// Column of the first trailing crew boat, relative to the ship's slot.
const CREW_AT: usize = 6;
const CREW_STEP: usize = 3;
/// Most rows of open water put between two bands when the sea has room.
const MAX_GAP: usize = 2;
const WATER: [char; 8] = ['▁', '▁', '▂', '▂', '▁', '▁', '▁', '▁'];
/// About one open-water cell in SWELL carries a wave.
const SWELL: usize = 9;

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
            Self::Symbols => (2, 1),
        }
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
        (Sail::Remote, false) => ["  ·  ", "╲▁▁▁╱"],
        (Sail::Remote, true) => ["  .  ", "\\___/"],
    }
}

/// Sail state, from recorded facts only. Unknown is never drawn as an all-clear.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Sail {
    /// The first mate is working: a wake is running, or its own Herdr pane
    /// reports the agent working.
    Up,
    /// Dispatching, with the first mate not working.
    Anchored,
    /// The dispatcher is stopped.
    Docked,
    /// The ship's state could not be read.
    Error,
    /// No live state for this ship.
    Unknown,
    /// A ship on a linked machine; its state is read there, not here.
    Remote,
}

/// `working` holds ships whose own validated Herdr pane reports a working
/// agent. Presence alone does not raise the sails: a hosted first mate holds
/// its ship's presence whenever its pane is open, busy or not.
pub fn sail(ship: &Ship, working: &BTreeSet<String>) -> Sail {
    if ship.remote.is_some() {
        return Sail::Remote;
    }
    match &ship.live {
        None => Sail::Unknown,
        Some(live) if live.error.is_some() => Sail::Error,
        Some(live) if !live.dispatcher => Sail::Docked,
        Some(live)
            if working.contains(&ship.id) || live.wakes.iter().any(|w| w.status == "running") =>
        {
            Sail::Up
        }
        Some(_) => Sail::Anchored,
    }
}

/// An entry in the key along the bottom of the sea.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    Sail(Sail),
    Crew,
    Bottle,
    Flag,
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
    /// The heading over a linked machine's ships.
    Machine(String),
    Key(Key),
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
    /// Rows drawn as a full line of waves, under the hulls.
    pub waterlines: Vec<usize>,
    /// Rows left as open sky: sails, names, headings, the key and symbols.
    /// Every other row is open water with a sparse swell.
    pub calm: Vec<usize>,
}

/// The ships of one machine; `machine` is None for this one.
struct Group<'a> {
    machine: Option<&'a str>,
    ships: Vec<&'a Ship>,
}

/// This machine's ships first, liaison first and then by id, then each linked
/// machine's by name and id, so every ship keeps its place between refreshes.
fn groups(snapshot: &Snapshot) -> Vec<Group<'_>> {
    let mut local: Vec<&Ship> = Vec::new();
    let mut linked: BTreeMap<&str, Vec<&Ship>> = BTreeMap::new();
    for ship in &snapshot.ships {
        match ship.remote.as_deref() {
            None => local.push(ship),
            Some(machine) => linked.entry(machine).or_default().push(ship),
        }
    }
    local.sort_by(|a, b| b.liaison.cmp(&a.liaison).then_with(|| a.id.cmp(&b.id)));
    let mut groups = Vec::new();
    if !local.is_empty() {
        groups.push(Group {
            machine: None,
            ships: local,
        });
    }
    for (machine, mut ships) in linked {
        ships.sort_by(|a, b| a.id.cmp(&b.id));
        groups.push(Group {
            machine: Some(machine),
            ships,
        });
    }
    groups
}

/// One horizontal band of the sea: a machine heading or a row of ships.
enum Band<'a> {
    Machine(&'a str),
    Ships(&'a [&'a Ship]),
}

impl Band<'_> {
    fn height(&self, mode: Mode) -> usize {
        match self {
            Self::Machine(_) => 1,
            Self::Ships(_) => mode.slot().1,
        }
    }
}

fn bands<'a>(groups: &'a [Group<'a>], cols: usize) -> Vec<Band<'a>> {
    let mut bands = Vec::new();
    for group in groups {
        if let Some(machine) = group.machine {
            bands.push(Band::Machine(machine));
        }
        bands.extend(group.ships.chunks(cols).map(Band::Ships));
    }
    bands
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
        (Sail::Remote, false) => "·",
        (Sail::Remote, true) => ".",
    }
}

fn flag(ascii: bool) -> &'static str {
    if ascii { "F" } else { "⚑" }
}

fn bottle(ascii: bool) -> &'static str {
    if ascii { "%" } else { "◊" }
}

/// The key's text for one entry, using the glyph drawn in this mode.
fn key_text(key: Key, mode: Mode, ascii: bool) -> String {
    let (glyph, label) = match key {
        Key::Sail(sail) => {
            let glyph = match (mode, sail, ascii) {
                (Mode::Symbols, _, _) => symbol(sail, ascii),
                (_, Sail::Up, false) => "◿│◣",
                (_, Sail::Up, true) => "/|\\",
                (_, Sail::Anchored, false) => "⊥",
                (_, Sail::Anchored, true) => "T",
                (_, Sail::Docked, false) => "▕╲▁▁▏",
                (_, Sail::Docked, true) => "|\\__|",
                (_, Sail::Error, false) => "✕",
                (_, Sail::Error, true) => "x",
                (_, Sail::Unknown, _) => "?",
                (_, Sail::Remote, false) => "·",
                (_, Sail::Remote, true) => ".",
            };
            let label = match sail {
                Sail::Up => "working",
                Sail::Anchored => "anchored",
                Sail::Docked => "docked",
                Sail::Error => "unreadable",
                Sail::Unknown => "no state",
                Sail::Remote => "linked machine",
            };
            (glyph, label)
        }
        Key::Crew => (boat::small(ascii), "crew"),
        Key::Bottle => (bottle(ascii), "request"),
        Key::Flag => (flag(ascii), "needs you"),
    };
    format!("{glyph} {label}")
}

/// Water for one waterline cell: a pure function of position and phase.
fn wave(x: usize, y: usize, phase: usize, ascii: bool) -> char {
    if ascii {
        '~'
    } else {
        WATER[(x + 3 * y + phase) % WATER.len()]
    }
}

/// Open water: a sparse, scattered swell that drifts one cell per `drift`.
fn swell(x: usize, y: usize, drift: usize) -> char {
    // SplitMix64's finaliser, so neighbouring cells look unrelated.
    let mut h = ((x + drift) as u64) << 32 | y as u64;
    h = (h ^ (h >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    if (h ^ (h >> 31)) % SWELL as u64 == 0 {
        '~'
    } else {
        ' '
    }
}

/// Everything shared while placing ships.
struct Sea<'a> {
    snapshot: &'a Snapshot,
    working: &'a BTreeSet<String>,
    mode: Mode,
    t: u128,
    reduced: bool,
    ascii: bool,
    placed: Vec<Placed>,
    keys: BTreeSet<Key>,
    /// Waterline of each drawn ship on this machine, for bottles.
    harbour: Vec<(&'a Ship, usize, usize)>,
}

impl<'a> Sea<'a> {
    /// Places one ship's sprites with its slot at (`x`, `y`). `index` is its
    /// place in the fleet, so neighbours sway out of step.
    fn ship(&mut self, ship: &'a Ship, index: usize, x: usize, y: usize) {
        let (sw, _) = self.mode.slot();
        let id = ship.id.clone();
        let sail = sail(ship, self.working);
        let flagged = self.snapshot.pending.iter().any(|n| n.owner == ship.id);
        if ship.remote.is_none() {
            self.harbour.push((ship, x, y));
        }
        if self.mode == Mode::Symbols {
            let text = if flagged {
                self.keys.insert(Key::Flag);
                flag(self.ascii)
            } else {
                self.keys.insert(Key::Sail(sail));
                symbol(sail, self.ascii)
            };
            self.placed.push(Placed {
                x,
                y,
                rows: vec![text.into()],
                mark: Mark::Boat { ship: id, sail },
            });
            return;
        }
        self.keys.insert(Key::Sail(sail));
        // Only a boat under way sways; the rest ride still.
        let offset = if sail == Sail::Up && !self.reduced {
            ((self.t / SWAY_MS) as usize + index) % 2
        } else {
            0
        };
        self.placed.push(Placed {
            x: x + offset,
            y,
            rows: hull(sail, self.ascii).iter().map(|r| (*r).into()).collect(),
            mark: Mark::Boat {
                ship: id.clone(),
                sail,
            },
        });
        let crew = self
            .snapshot
            .jobs
            .iter()
            .filter(|j| j.owner == ship.id && j.is_active())
            .count();
        if crew > 0 {
            self.keys.insert(Key::Crew);
        }
        for k in 0..crew.min(CREW_BOATS) {
            self.placed.push(Placed {
                x: x + CREW_AT + CREW_STEP * k,
                y: y + 1,
                rows: vec![boat::small(self.ascii).into()],
                mark: Mark::Crew { ship: id.clone() },
            });
        }
        if crew > CREW_BOATS {
            let extra = crew - CREW_BOATS;
            let at = CREW_AT + CREW_STEP * CREW_BOATS;
            self.placed.push(Placed {
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
            self.keys.insert(Key::Flag);
            self.placed.push(Placed {
                x: x + sw - 1,
                y,
                rows: vec![flag(self.ascii).into()],
                mark: Mark::Flag { ship: id.clone() },
            });
        }
        if self.mode == Mode::Names {
            let name = clean(
                if ship.name.is_empty() {
                    &ship.id
                } else {
                    &ship.name
                },
                256,
            );
            self.placed.push(Placed {
                x,
                y: y + 2,
                rows: vec![clip(&name, sw - 1)],
                mark: Mark::Name(name),
            });
        }
    }

    /// Open requests drift as a bottle along the waterline from the liaison
    /// toward each drawn ship here that has them. The sender is approximated:
    /// the snapshot records a count per ship, not who sent each request.
    /// Symbol mode draws no bottles, so they never cover a ship's symbol.
    /// Reduced motion holds them halfway. Bottles go first, so they pass
    /// behind boats rather than over them.
    fn bottles(&mut self) -> Vec<Placed> {
        let Some(&(liaison, fx, fy)) = self.harbour.first() else {
            return Vec::new();
        };
        if self.mode == Mode::Symbols || !liaison.liaison {
            return Vec::new();
        }
        let p = if self.reduced {
            TRIP_MS / 2
        } else {
            (self.t % TRIP_MS as u128) as i64
        };
        // Progress is a pure function of time; integer rounding keeps it monotone.
        let at = |from: usize, to: usize| {
            (from as i64 + (to as i64 - from as i64) * p / TRIP_MS) as usize
        };
        let (from_x, from_y) = (fx + 5, fy + 1);
        let mut bottles = Vec::new();
        for &(ship, tx, ty) in &self.harbour[1..] {
            if ship.live.as_ref().map_or(0, |l| l.requests) == 0 {
                continue;
            }
            self.keys.insert(Key::Bottle);
            bottles.push(Placed {
                x: at(from_x, tx.saturating_sub(1)),
                y: at(from_y, ty + 1),
                rows: vec![bottle(self.ascii).into()],
                mark: Mark::Bottle {
                    to: ship.id.clone(),
                },
            });
        }
        bottles
    }
}

pub fn scene(
    snapshot: &Snapshot,
    working: &BTreeSet<String>,
    elapsed: Duration,
    width: usize,
    height: usize,
    reduced: bool,
    ascii: bool,
) -> Scene {
    let groups = groups(snapshot);
    let total: usize = groups.iter().map(|g| g.ships.len()).sum();
    let fitted = [Mode::Names, Mode::Boats, Mode::Symbols]
        .into_iter()
        .find_map(|mode| {
            let cols = width / mode.slot().0;
            if cols == 0 {
                return None;
            }
            let bands = bands(&groups, cols);
            let needed: usize = bands.iter().map(|b| b.height(mode)).sum();
            (needed <= height).then_some((mode, bands, needed))
        });
    let mut sea = Sea {
        snapshot,
        working,
        mode: fitted.as_ref().map_or(Mode::Symbols, |f| f.0),
        t: elapsed.as_millis(),
        reduced,
        ascii,
        placed: Vec::new(),
        keys: BTreeSet::new(),
        harbour: Vec::new(),
    };
    let (mut waterlines, mut calm) = (Vec::new(), Vec::new());

    let Some((mode, bands, needed)) = fitted else {
        // Too many ships for any mode: one symbol each, in fleet order, with
        // the last row kept for "+n more ships".
        let cols = width / Mode::Symbols.slot().0;
        let shown = (cols * height.saturating_sub(1)).min(total);
        let ships = groups.iter().flat_map(|g| g.ships.iter().copied());
        for (i, ship) in ships.take(shown).enumerate() {
            sea.ship(ship, i, (i % cols) * 2, i / cols);
        }
        let omitted = total - shown;
        if omitted > 0 && height > 0 {
            sea.placed.push(Placed {
                x: 0,
                y: height - 1,
                rows: vec![clip(&format!("+{omitted} more ships"), width)],
                mark: Mark::More(omitted),
            });
        }
        return Scene {
            mode: Mode::Symbols,
            placed: sea.placed,
            omitted,
            waterlines,
            calm: (0..height).collect(),
        };
    };

    // Spare rows: the bottom one for the key, the rest as open water spread
    // evenly between bands (a heading stays on its ships) and around them.
    let key_row = height > needed;
    let room = height - needed - usize::from(key_row);
    let spaced = |i: usize| {
        i > 0 && !(matches!(bands[i], Band::Ships(_)) && matches!(bands[i - 1], Band::Machine(_)))
    };
    let gaps = (0..bands.len()).filter(|&i| spaced(i)).count();
    let gap = (room / (gaps + 2)).min(MAX_GAP);
    let mut y = (room - gap * gaps) / 2;
    let (sw, _) = mode.slot();
    let mut index = 0;
    let mut headings = Vec::new();
    for (i, band) in bands.iter().enumerate() {
        if spaced(i) {
            y += gap;
        }
        match band {
            Band::Machine(machine) => {
                let rule = if ascii { "--" } else { "──" };
                let sep = if ascii { "-" } else { "·" };
                headings.push(Placed {
                    x: 0,
                    y,
                    rows: vec![clip(
                        &format!(
                            "{rule} {} {sep} linked machine, state read there",
                            clean(machine, 64)
                        ),
                        width,
                    )],
                    mark: Mark::Machine(clean(machine, 64)),
                });
                calm.push(y);
            }
            Band::Ships(row) => {
                for (c, ship) in row.iter().enumerate() {
                    sea.ship(ship, index, c * sw, y);
                    index += 1;
                }
                calm.push(y);
                if mode != Mode::Symbols {
                    waterlines.push(y + 1);
                }
                if mode == Mode::Names {
                    calm.push(y + 2);
                }
            }
        }
        y += band.height(mode);
    }

    let mut placed = sea.bottles();
    placed.append(&mut sea.placed);
    placed.append(&mut headings);
    if key_row {
        let mut x = 0;
        for &key in &sea.keys {
            let text = key_text(key, mode, ascii);
            let cells = text.chars().count();
            if x + cells > width {
                break;
            }
            placed.push(Placed {
                x,
                y: height - 1,
                rows: vec![text],
                mark: Mark::Key(key),
            });
            x += cells + 2;
        }
        calm.push(height - 1);
    }

    Scene {
        mode,
        placed,
        omitted: 0,
        waterlines,
        calm,
    }
}

fn sail_style(sail: Sail) -> Style {
    Style::default().fg(match sail {
        Sail::Up => Color::Green,
        Sail::Anchored => Color::Cyan,
        Sail::Docked => Color::Gray,
        Sail::Error => Color::Red,
        Sail::Unknown => Color::DarkGray,
        Sail::Remote => Color::Blue,
    })
}

fn style(mark: &Mark) -> Style {
    let crew = Style::default().fg(Color::Yellow);
    let flag = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let bottle = Style::default().fg(Color::Magenta);
    match mark {
        Mark::Boat { sail, .. } | Mark::Key(Key::Sail(sail)) => sail_style(*sail),
        Mark::Name(_) => Style::default().fg(Color::Gray),
        Mark::Crew { .. } | Mark::CrewMore { .. } | Mark::Key(Key::Crew) => crew,
        Mark::Flag { .. } | Mark::Key(Key::Flag) => flag,
        Mark::Bottle { .. } | Mark::Key(Key::Bottle) => bottle,
        Mark::Machine(_) | Mark::More(_) => Style::default().fg(Color::DarkGray),
    }
}

/// Paints the water, then the scene's sprites, inside `area` only.
pub fn draw(
    frame: &mut Frame,
    area: Rect,
    snapshot: &Snapshot,
    working: &BTreeSet<String>,
    elapsed: Duration,
    reduced: bool,
    ascii: bool,
) {
    let scene = scene(
        snapshot,
        working,
        elapsed,
        area.width as usize,
        area.height as usize,
        reduced,
        ascii,
    );
    let (phase, drift) = if reduced {
        (0, 0)
    } else {
        let ms = elapsed.as_millis();
        ((ms / WAVE_MS) as usize, (ms / SWAY_MS) as usize)
    };
    let buf = frame.buffer_mut();
    let water = Style::default().fg(Color::Blue);
    let open = water.add_modifier(Modifier::DIM);
    for dy in 0..area.height as usize {
        let (row, style): (String, _) = if scene.waterlines.contains(&dy) {
            let row = (0..area.width as usize)
                .map(|dx| wave(dx, dy, phase, ascii))
                .collect();
            (row, water)
        } else if scene.calm.contains(&dy) {
            (" ".repeat(area.width as usize), water)
        } else {
            let row = (0..area.width as usize)
                .map(|dx| swell(dx, dy, drift))
                .collect();
            (row, open)
        };
        buf.set_string(area.x, area.y + dy as u16, row, style);
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
            remote: None,
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

    fn none() -> BTreeSet<String> {
        BTreeSet::new()
    }

    fn remote(id: &str, machine: &str) -> Ship {
        Ship {
            remote: Some(machine.into()),
            ..ship(id, false, None)
        }
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
                let scene = scene(
                    &snapshot,
                    &none(),
                    Duration::from_secs(3),
                    w,
                    h,
                    false,
                    false,
                );
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
        let first = scene(&snapshot, &none(), Duration::ZERO, 60, 20, false, false);
        let later = scene(
            &snapshot,
            &none(),
            Duration::from_secs(9),
            60,
            20,
            true,
            false,
        );
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
            let scene = scene(&snapshot, &none(), Duration::ZERO, 100, 30, false, false);
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
    fn a_bottle_closes_steadily_on_its_recipient() {
        // One recipient beside the liaison, one on the next row back at the left.
        let mut snapshot = fleet(1);
        snapshot.ships.push(ship("reader", false, Some(live(2))));
        for i in 0..3 {
            snapshot
                .ships
                .push(ship(&format!("s{i}"), false, Some(live(0))));
        }
        snapshot.ships.push(ship("z-below", false, Some(live(1))));
        for recipient in ["reader", "z-below"] {
            let boat = scene(&snapshot, &none(), Duration::ZERO, 80, 30, false, false)
                .placed
                .into_iter()
                .find(|p| matches!(&p.mark, Mark::Boat { ship, .. } if ship == recipient))
                .map(|p| (p.x as i64, p.y as i64 + 1))
                .unwrap();
            let distance = |ms: u64| {
                scene(
                    &snapshot,
                    &none(),
                    Duration::from_millis(ms),
                    80,
                    30,
                    false,
                    false,
                )
                .placed
                .into_iter()
                .find(|p| matches!(&p.mark, Mark::Bottle { to } if to == recipient))
                .map(|p| (p.x as i64 - boat.0).abs() + (p.y as i64 - boat.1).abs())
                .expect("bottle drawn")
            };
            let start = distance(0);
            let mut last = start;
            for ms in (0..TRIP_MS as u64).step_by(50) {
                let here = distance(ms);
                assert!(here <= last, "{recipient}: drifted away at {ms}");
                last = here;
            }
            assert!(4 * last <= start, "{recipient}: {start} -> {last}");
        }
    }

    #[test]
    fn requests_and_flags_are_per_ship_not_per_sprite() {
        let mut snapshot = fleet(2);
        snapshot.ships.push(ship("reader", false, Some(live(4))));
        snapshot.pending.push(Notice {
            owner: "reader".into(),
            ..Notice::default()
        });
        let scene = scene(&snapshot, &none(), Duration::ZERO, 100, 30, false, false);
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
            scene(&snapshot, &none(), Duration::ZERO, 120, 40, false, false).mode,
            Mode::Names
        );
        let narrow = scene(&snapshot, &none(), Duration::ZERO, 40, 14, false, false);
        assert_eq!(narrow.mode, Mode::Boats);
        assert!(
            !narrow
                .placed
                .iter()
                .any(|p| matches!(p.mark, Mark::Name(_)))
        );
        assert_eq!(
            scene(&snapshot, &none(), Duration::ZERO, 20, 6, false, false).mode,
            Mode::Symbols
        );
    }

    #[test]
    fn nothing_is_placed_outside_the_area() {
        for count in [1, 5, 30] {
            let snapshot = fleet(count);
            for w in (40..=300).step_by(13) {
                for h in (14..=100).step_by(9) {
                    let scene = scene(
                        &snapshot,
                        &none(),
                        Duration::from_millis(1234),
                        w,
                        h,
                        false,
                        false,
                    );
                    assert!(inside(&scene, w, h), "{w}x{h} with {count}");
                }
            }
        }
    }

    #[test]
    fn the_same_inputs_give_the_same_scene() {
        let snapshot = fleet(9);
        let a = scene(
            &snapshot,
            &none(),
            Duration::from_millis(777),
            90,
            25,
            false,
            true,
        );
        let b = scene(
            &snapshot,
            &none(),
            Duration::from_millis(777),
            90,
            25,
            false,
            true,
        );
        assert_eq!(a, b);
    }

    #[test]
    fn reduced_motion_holds_the_scene_still() {
        let mut snapshot = fleet(4);
        snapshot.ships.push(ship("reader", false, Some(live(2))));
        snapshot.jobs.push(job("ship-00", "running"));
        for (w, h) in [(80, 24), (40, 14)] {
            let still = scene(&snapshot, &none(), Duration::ZERO, w, h, true, false);
            assert!(
                still
                    .placed
                    .iter()
                    .any(|p| matches!(p.mark, Mark::Bottle { .. }))
            );
            for ms in [1_000, 2_500, 5_000, 9_999] {
                let later = scene(
                    &snapshot,
                    &none(),
                    Duration::from_millis(ms),
                    w,
                    h,
                    true,
                    false,
                );
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
            Sail::Remote,
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
        let symbols = scene(
            &snapshot,
            &none(),
            Duration::from_millis(3_000),
            10,
            4,
            false,
            false,
        );
        assert_eq!(symbols.mode, Mode::Symbols);
        assert!(
            !symbols
                .placed
                .iter()
                .any(|p| matches!(p.mark, Mark::Bottle { .. }))
        );
        let boats = scene(
            &snapshot,
            &none(),
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
            let scene = scene(&snapshot, &none(), Duration::ZERO, w, h, false, false);
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
        let scene = scene(&snapshot, &none(), Duration::ZERO, 100, 30, false, false);
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
        let none = none();
        let working = BTreeSet::from(["a".to_string()]);
        let mut state = live(0);
        assert_eq!(sail(&ship("a", false, None), &none), Sail::Unknown);
        assert_eq!(sail(&remote("a", "mac"), &working), Sail::Remote);
        assert_eq!(
            sail(&ship("a", false, Some(state.clone())), &none),
            Sail::Anchored
        );
        // A hosted first mate holds presence whether or not it is busy.
        state.attended = true;
        assert_eq!(
            sail(&ship("a", false, Some(state.clone())), &none),
            Sail::Anchored
        );
        assert_eq!(
            sail(&ship("a", false, Some(state.clone())), &working),
            Sail::Up
        );
        state.attended = false;
        state.wakes.push(Wake {
            mode: "run".into(),
            status: "running".into(),
        });
        assert_eq!(
            sail(&ship("a", false, Some(state.clone())), &none),
            Sail::Up
        );
        state.dispatcher = false;
        assert_eq!(
            sail(&ship("a", false, Some(state.clone())), &working),
            Sail::Docked
        );
        state.error = Some("missing".into());
        assert_eq!(sail(&ship("a", false, Some(state)), &working), Sail::Error);
    }

    /// A busy mixed fleet: this machine with crew, requests and a flag, plus
    /// two linked machines.
    fn mixed() -> Snapshot {
        let mut snapshot = fleet(5);
        snapshot.ships[2].live.as_mut().unwrap().requests = 1;
        snapshot.pending.push(Notice {
            owner: "ship-02".into(),
            ..Notice::default()
        });
        for _ in 0..5 {
            snapshot.jobs.push(job("ship-00", "running"));
        }
        for (id, machine) in [("mac-b", "mac"), ("mac-a", "mac"), ("pi-1", "pi")] {
            snapshot.ships.push(remote(id, machine));
        }
        snapshot
    }

    #[test]
    fn linked_machines_sail_under_their_own_heading_after_this_one() {
        let snapshot = mixed();
        let scene = scene(&snapshot, &none(), Duration::ZERO, 100, 40, false, false);
        let order: Vec<_> = scene
            .placed
            .iter()
            .filter_map(|p| match &p.mark {
                Mark::Boat { ship, sail } => Some((p.y, ship.as_str(), Some(*sail))),
                Mark::Machine(m) => Some((p.y, m.as_str(), None)),
                _ => None,
            })
            .collect();
        let mut by_row = order.clone();
        by_row.sort_by_key(|(y, ..)| *y);
        let names: Vec<_> = by_row.iter().map(|(_, n, _)| *n).collect();
        let at = |n: &str| names.iter().position(|x| *x == n).unwrap();
        assert!(at("ship-03") < at("mac") && at("mac") < at("mac-a"));
        assert!(at("mac-a") < at("pi") && at("pi") < at("pi-1"));
        for (_, ship, sail) in &order {
            if ship.starts_with("mac-") || ship.starts_with("pi-") {
                assert_eq!(*sail, Some(Sail::Remote), "{ship}");
            }
        }
    }

    #[test]
    fn the_key_names_exactly_what_is_on_screen() {
        let snapshot = mixed();
        let working = BTreeSet::from(["liaison".to_string()]);
        for (w, h, ascii) in [(200, 40, false), (200, 40, true), (30, 20, false)] {
            let scene = scene(&snapshot, &working, Duration::ZERO, w, h, false, ascii);
            let keys: BTreeSet<Key> = scene
                .placed
                .iter()
                .filter_map(|p| match p.mark {
                    Mark::Key(k) => Some(k),
                    _ => None,
                })
                .collect();
            let mut drawn = BTreeSet::new();
            for p in &scene.placed {
                match (&p.mark, scene.mode) {
                    (Mark::Boat { .. }, Mode::Symbols) if p.rows[0] == flag(ascii) => {
                        drawn.insert(Key::Flag)
                    }
                    (Mark::Boat { sail, .. }, _) => drawn.insert(Key::Sail(*sail)),
                    (Mark::Crew { .. }, _) => drawn.insert(Key::Crew),
                    (Mark::Bottle { .. }, _) => drawn.insert(Key::Bottle),
                    (Mark::Flag { .. }, _) => drawn.insert(Key::Flag),
                    _ => false,
                };
            }
            if w >= 200 {
                assert_eq!(keys, drawn, "{w}x{h}");
            } else {
                // A narrow key keeps the entries that fit, in order.
                assert!(!keys.is_empty() && keys.is_subset(&drawn), "{w}x{h}");
            }
        }
    }

    #[test]
    fn sprites_of_different_ships_never_overlap() {
        let snapshot = mixed();
        let owner = |m: &Mark| match m {
            Mark::Boat { ship, .. }
            | Mark::Crew { ship }
            | Mark::CrewMore { ship, .. }
            | Mark::Flag { ship } => ship.clone(),
            Mark::Bottle { .. } => String::new(),
            other => format!("{other:?}"),
        };
        for w in (40..=200).step_by(7) {
            for h in (6..=60).step_by(5) {
                let scene = scene(
                    &snapshot,
                    &none(),
                    Duration::from_secs(1),
                    w,
                    h,
                    false,
                    false,
                );
                let mut cells: BTreeMap<(usize, usize), String> = BTreeMap::new();
                for p in scene
                    .placed
                    .iter()
                    .filter(|p| !matches!(p.mark, Mark::Bottle { .. }))
                {
                    for (dy, row) in p.rows.iter().enumerate() {
                        for (dx, c) in row.chars().enumerate() {
                            if c == ' ' {
                                continue;
                            }
                            let who = if let Mark::Name(_) = p.mark {
                                format!("name@{},{}", p.x, p.y)
                            } else {
                                owner(&p.mark)
                            };
                            if let Some(prev) = cells.insert((p.x + dx, p.y + dy), who.clone()) {
                                assert_eq!(prev, who, "{w}x{h} at {:?}", (p.x + dx, p.y + dy));
                            }
                        }
                    }
                }
                for y in &scene.waterlines {
                    assert!(!scene.calm.contains(y), "{w}x{h} row {y}");
                }
            }
        }
    }

    #[test]
    fn sea_never_panics_and_stays_in_bounds_from_40x14_to_300x100() {
        let snapshot = fleet(12);
        for w in 40..=300 {
            for h in 14..=100 {
                let scene = scene(
                    &snapshot,
                    &none(),
                    Duration::from_millis(4321),
                    w,
                    h,
                    false,
                    false,
                );
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
                            &none(),
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
