//! The placeholder row's spinner: a field of stars flying left to right through
//! the column where the new session's name will go.
//!
//! Drawn in braille, which gives every cell a 2x4 grid of dots — so a field ten
//! cells wide is twenty dots across and four high, and a star is one dot. Braille
//! is one column wide on every terminal (unlike the block elements, which some
//! CJK locales draw two wide), and it changes characters rather than colours, so
//! the row needs nothing from the palette and is correct under `NO_COLOR` by
//! construction, as every screen here is.
//!
//! # Random, and random again
//!
//! No row of dots has a speed of its own. Each star picks a row and a speed when
//! it enters and picks both again every time it comes back in from the left, so
//! the field never settles into lanes. Speeds are log-uniform between
//! [`SLOWEST`] and [`FASTEST`]: most stars drift and a few race, the way near
//! and far ones would, rather than the even spread a uniform pick gives.
//!
//! Re-entry is staggered too — a star comes back somewhere in the few dots
//! before the left edge, not on it — so stars that leave together do not arrive
//! together.
//!
//! # Time
//!
//! The field is moved on by elapsed time ([`Starfield::advance`]), not by frames,
//! for the reason [`crate::fade`] gives: a slow terminal skips frames rather than
//! slowing the stars. It holds state rather than being a function of the clock
//! because where a star is now depends on every row and speed it has picked
//! since it entered.
//!
//! The randomness is a small generator seeded once, from the operating system
//! where it will say. Nothing depends on it but the look of the row.

use std::time::Duration;

/// The slowest a star goes, in dots a second: across a ten-cell field in about
/// a second and a quarter.
const SLOWEST: f32 = 15.0;

/// The fastest, in dots a second: across the same field in a quarter of one.
const FASTEST: f32 = 80.0;

/// How far before the left edge a star may re-enter, and how far past the right
/// one it goes before it does — in dots. The stagger described in the module
/// docs.
const LEAD: f32 = 6.0;

/// Dots of track per star, counting the [`LEAD`] beyond each edge. One star to
/// every three dots is a field that reads as stars rather than as noise.
const SPACING: usize = 3;

/// The narrowest the field is drawn, in cells, however short the session names
/// are: below this there is too little track for a star to be seen to move.
pub const MIN_WIDTH: usize = 6;

/// The dot each row of a braille cell's 2x4 grid lights, left column then
/// right. Braille numbers its dots down the left column and then the right,
/// with the bottom pair added last, which is why the bits are not in order.
const DOTS: [[u32; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];

/// The first braille pattern, which has no dots.
const BRAILLE: u32 = 0x2800;

/// One star: where it is along the track, which row of dots it is on, and how
/// fast it is going.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Star {
    /// In dots from the field's left edge. Negative while it has yet to enter.
    x: f32,
    /// Which of the cell's four rows of dots, from the top.
    row: usize,
    /// Dots a second.
    speed: f32,
}

/// The field. See the module docs.
pub struct Starfield {
    stars: Vec<Star>,
    /// The width, in cells, the stars were laid out for.
    cells: usize,
    rng: Rng,
}

impl Starfield {
    /// An empty field; the first [`Starfield::scatter`] or
    /// [`Starfield::advance`] fills it.
    pub fn new(seed: u64) -> Self {
        Self {
            stars: Vec::new(),
            cells: 0,
            rng: Rng::new(seed),
        }
    }

    /// A field seeded from the operating system, or from a constant if it will
    /// not say. Only the look of the row depends on the seed.
    pub fn seeded() -> Self {
        Self::new(getrandom::u64().unwrap_or(0x9e37_79b9_7f4a_7c15))
    }

    /// Lay out a fresh field `cells` wide, with the stars anywhere along it
    /// rather than all queued at the left edge — so the row is full from its
    /// first frame.
    pub fn scatter(&mut self, cells: usize) {
        self.cells = cells;
        let track = track(cells);
        let count = if cells == 0 {
            0
        } else {
            (track as usize).div_ceil(SPACING)
        };
        self.stars = (0..count)
            .map(|_| {
                let mut star = self.rng.star();
                star.x = self.rng.unit() * track - LEAD / 2.0;
                star
            })
            .collect();
    }

    /// Move every star on by `elapsed`, bringing back in from the left any that
    /// has gone off the right with a new row and speed. A field laid out for a
    /// different width is scattered afresh instead, since its stars were spaced
    /// for a track it no longer has.
    pub fn advance(&mut self, elapsed: Duration, cells: usize) {
        if cells != self.cells {
            self.scatter(cells);
            return;
        }
        let end = dots(cells) + LEAD;
        let secs = elapsed.as_secs_f32();
        for i in 0..self.stars.len() {
            let star = &mut self.stars[i];
            star.x += star.speed * secs;
            // Once, however long the pause: a star that has been gone a minute
            // re-enters like one that left a frame ago.
            if star.x >= end {
                let mut back = self.rng.star();
                back.x = -1.0 - self.rng.unit() * LEAD;
                self.stars[i] = back;
            }
        }
    }

    /// The field as `cells` characters: braille where a star is, a space where
    /// no dot in the cell is lit. Clipped to `cells`, which may be narrower than
    /// the width the stars were laid out for when the terminal is.
    pub fn render(&self, cells: usize) -> String {
        let mut bits = vec![0u32; cells];
        for star in &self.stars {
            if star.x < 0.0 {
                continue;
            }
            let dot = star.x as usize;
            if let Some(cell) = bits.get_mut(dot / 2) {
                *cell |= DOTS[star.row][dot % 2];
            }
        }
        bits.into_iter()
            .map(|b| {
                if b == 0 {
                    ' '
                } else {
                    char::from_u32(BRAILLE + b).expect("braille patterns are chars")
                }
            })
            .collect()
    }
}

/// Dots across a field `cells` wide.
fn dots(cells: usize) -> f32 {
    (cells * 2) as f32
}

/// The dots a star travels, counting the lead beyond each edge.
fn track(cells: usize) -> f32 {
    dots(cells) + 2.0 * LEAD
}

/// xorshift64*: small, fast, and plenty for where some dots go.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Zero is the one state xorshift never leaves.
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// A star with a fresh row and speed, not yet placed.
    fn star(&mut self) -> Star {
        let row = (self.next() % 4) as usize;
        let speed = SLOWEST * (FASTEST / SLOWEST).powf(self.unit());
        Star { x: 0.0, row, speed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn field(cells: usize) -> Starfield {
        let mut f = Starfield::new(7);
        f.scatter(cells);
        f
    }

    /// One character a cell, each a space or a braille pattern — which is also
    /// what keeps the name column the width the picker measured it as.
    #[test]
    fn the_field_is_as_many_one_column_cells_as_asked_for() {
        let f = field(10);
        let row = f.render(10);
        assert_eq!(row.chars().count(), 10);
        assert_eq!(row.width(), 10);
        for c in row.chars() {
            assert!(
                c == ' ' || (0x2800..=0x28ff).contains(&(c as u32)),
                "{c:?} is neither a space nor braille"
            );
        }
        assert!(row.chars().any(|c| c != ' '), "no stars at all: {row:?}");
    }

    #[test]
    fn it_can_be_drawn_narrower_than_it_was_laid_out() {
        let f = field(10);
        assert_eq!(f.render(4).chars().count(), 4);
        assert_eq!(f.render(0), "");
    }

    #[test]
    fn a_field_with_no_width_has_no_stars() {
        let mut f = field(0);
        f.advance(Duration::from_millis(500), 0);
        assert_eq!(f.render(0), "");
        assert!(f.stars.is_empty());
    }

    #[test]
    fn stars_fly_left_to_right() {
        let mut f = field(10);
        let before = f.stars.clone();
        f.advance(Duration::from_millis(10), 10);
        for (was, now) in before.iter().zip(&f.stars) {
            // Too short a step for any to have left and come back.
            assert!(now.x > was.x, "{was:?} did not move right: {now:?}");
            assert_eq!(now.row, was.row);
        }
    }

    #[test]
    fn a_star_that_leaves_comes_back_in_from_the_left_with_a_new_pick() {
        let mut f = field(10);
        f.stars = vec![Star {
            x: dots(10) + LEAD - 0.1,
            row: 0,
            speed: FASTEST,
        }];
        f.advance(Duration::from_millis(100), 10);
        let back = f.stars[0];
        assert!(
            back.x < 0.0 && back.x >= -1.0 - LEAD,
            "re-entered at {}",
            back.x
        );
        assert!((SLOWEST..=FASTEST).contains(&back.speed));
        assert!(back.row < 4);
    }

    /// Every speed is picked afresh, so across a few hundred re-entries the
    /// field has used every row and a spread of speeds rather than one per row.
    #[test]
    fn rows_and_speeds_are_not_fixed() {
        let mut f = field(10);
        let mut rows = [false; 4];
        let (mut slow, mut fast) = (false, false);
        for _ in 0..2000 {
            f.advance(Duration::from_millis(50), 10);
            for s in &f.stars {
                assert!((SLOWEST..=FASTEST).contains(&s.speed), "{s:?}");
                rows[s.row] = true;
                slow |= s.speed < 25.0;
                fast |= s.speed > 60.0;
            }
        }
        assert_eq!(rows, [true; 4]);
        assert!(slow && fast, "speeds never spread out");
    }

    /// A long pause — the naming prompt left open — brings each star back once
    /// rather than losing any.
    #[test]
    fn a_long_pause_keeps_every_star() {
        let mut f = field(10);
        let count = f.stars.len();
        f.advance(Duration::from_secs(3600), 10);
        assert_eq!(f.stars.len(), count);
        assert!(f.stars.iter().all(|s| s.x < 0.0), "{:?}", f.stars);
    }

    #[test]
    fn a_new_width_lays_the_field_out_again() {
        let mut f = field(10);
        let narrow = f.stars.len();
        f.advance(Duration::from_millis(16), 30);
        assert!(f.stars.len() > narrow);
        assert_eq!(f.cells, 30);
    }

    #[test]
    fn the_same_seed_draws_the_same_field() {
        let (a, b) = (field(12), field(12));
        assert_eq!(a.render(12), b.render(12));
    }
}
