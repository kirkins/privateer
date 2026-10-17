//! Original nautical working indicator, inspired by First Mate's Calm behavior.
//! No upstream sprite implementation copied. Rendering ticks stay in the UI.
#[derive(Default)]
pub struct Boat {
    column: usize,
    left: bool,
    tick: u64,
    moved_at: u64,
}

/// The sailboat's two rows (sail, then hull), shared with the voyage sea.
pub fn art(ascii: bool) -> [&'static str; 2] {
    if ascii {
        [" /|\\ ", "\\___/"]
    } else {
        [" ◿│◣ ", "╲▁▁▁╱"]
    }
}

/// A small trailing boat: hull only, drawn on the hull row.
pub fn small(ascii: bool) -> &'static str {
    if ascii { "\\_/" } else { "╲▁╱" }
}

impl Boat {
    pub fn advance(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    pub fn frame(&mut self, width: usize, active: bool, reduced: bool, ascii: bool) -> [String; 2] {
        let width = width.min(4096);
        let span = width.saturating_sub(5);
        self.column = self.column.min(span);
        if active && !reduced && self.tick % 4 == 0 && self.tick != self.moved_at {
            self.moved_at = self.tick;
            if self.left {
                self.column = self.column.saturating_sub(1);
            } else {
                self.column = (self.column + 1).min(span);
            }
            if self.column == span {
                self.left = true;
            }
            if self.column == 0 {
                self.left = false;
            }
        }
        if width < 5 {
            return [
                " ".repeat(width),
                if ascii { "~" } else { "▁" }.repeat(width),
            ];
        }
        let mut sail = vec![' '; width];
        let mut water = (0..width)
            .map(|i| {
                if ascii {
                    '~'
                } else {
                    ['▁', '▁', '▂', '▂', '▁', '▁', '▁', '▁']
                        [((i as u64 + self.tick / 2) % 8) as usize]
                }
            })
            .collect::<Vec<_>>();
        let [top, hull] = art(ascii);
        for (i, c) in top.chars().enumerate() {
            sail[self.column + i] = c;
        }
        for (i, c) in hull.chars().enumerate() {
            water[self.column + i] = c;
        }
        [sail.into_iter().collect(), water.into_iter().collect()]
    }
}
