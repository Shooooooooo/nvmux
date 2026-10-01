//! The trail a session in flight leaves: a short field of stars flying left to
//! right off the end of its name, for as long as it is picked up.
//!
//! Drawn in braille, which gives every cell a 2x4 grid of dots — so a trail
//! [`TRAIL`] cells long is twelve dots across and four high, and a star is one
//! dot. Braille is one column wide on every terminal (unlike the block
//! elements, which some CJK locales draw two wide), and it changes characters
//! rather than colours, so the trail needs nothing from the palette and is
//! correct under `NO_COLOR` by construction, as every screen here is. How it is
//! weighted, bright to dim, is the renderer's business (see
//! [`super::draw`]).
//!
//! # Random, and random again
//!
//! No row of dots has a speed of its own. Each star picks a row and a speed when
//! it starts, and the one that replaces it picks its own, so the field never
//! settles into lanes. Speeds are log-uniform between [`SLOWEST`] and
//! [`FASTEST`]: most stars drift and a few race, the way near and far ones
//! would, rather than the even spread a uniform pick gives.
//!
//! # Thick by the name, thin at the end
//!
//! A star goes only so far before it burns out — its reach, picked with its row
//! and speed — and a new one starts at the name in its place. Reaches are
//! picked squared, so short lives are common and long ones rare: most stars die
//! a few dots out, and only a few make it to the end of the trail and off it.
//! The field is therefore thickest where the stars leave the name and thins
//! steadily towards the end, the way sparks thin out behind a fuse, and the
//! renderer's step from plain to dim carries the same fall-off on.
//!
//! A new star starts a little before the edge rather than on it, so stars that
//! burn out together do not come back in step.
//!
//! The stars start straight off the end of the name's reversed bar. Three ramps
//! between the two were tried and dropped — quadrant blocks stepping down a
//! quarter of the cell at a time, the shades `▓▒░`, and braille dots lit at a
//! falling rate — and so was simply drawing more stars; the plain field read
//! best.
//!
//! # Time
//!
//! The field is moved on by elapsed time ([`Starfield::advance`]), not by frames,
//! for the reason [`crate::fade`] gives: a slow terminal skips frames rather than
//! slowing the stars. It holds state rather than being a function of the clock
//! because where a star is now depends on every pick made since the field was
//! laid out.
//!
//! The randomness is a small generator seeded once, from the operating system
//! where it will say. Nothing depends on it but the look of the trail.

use std::time::Duration;

/// How long the trail is, in cells. Short: it is a flourish on the row being
/// moved, not a second thing on it to read.
pub const TRAIL: usize = 6;

/// The slowest a star goes, in dots a second: the length of the trail in about
/// four fifths of a second.
const SLOWEST: f32 = 15.0;

/// The fastest, in dots a second: the same in about a sixth of one.
const FASTEST: f32 = 80.0;

/// The least a star travels before it burns out, in dots: enough for even the
/// shortest-lived to be seen to move.
const SHORTEST: f32 = 2.0;

/// How far past the end of the trail the longest-lived stars get, in dots, so a
/// few leave off the end rather than every one burning out inside it.
const OVERSHOOT: f32 = 4.0;

/// How far before the left edge a new star may start, in dots — the stagger in
/// the module docs.
const STAGGER: f32 = 2.0;

/// Stars to a cell of trail. Burning out early, most of them crowd its first
/// few dots, which is the point; this many makes that crowd a thick start to
/// the trail rather than a solid block.
const STARS_PER_CELL: usize = 2;

/// The dot each row of a braille cell's 2x4 grid lights, left column then
/// right. Braille numbers its dots down the left column and then the right,
/// with the bottom pair added last, which is why the bits are not in order.
const DOTS: [[u32; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];

/// The first braille pattern, which has no dots.
const BRAILLE: u32 = 0x2800;

/// One star: where it is, which row of dots it is on, how fast it is going, and
/// how far it gets.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Star {
    /// In dots from the field's left edge. Negative while it has yet to enter.
    x: f32,
    /// Which of the cell's four rows of dots, from the top.
    row: usize,
    /// Dots a second.
    speed: f32,
    /// In dots from the left edge: where it burns out.
    reach: f32,
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
    /// not say. Only the look of the trail depends on the seed.
    pub fn seeded() -> Self {
        Self::new(getrandom::u64().unwrap_or(0x9e37_79b9_7f4a_7c15))
    }

    /// Lay out a fresh field `cells` wide, with each star already somewhere
    /// along its own life rather than all of them starting at the edge — so the
    /// trail has its shape from its first frame.
    pub fn scatter(&mut self, cells: usize) {
        self.cells = cells;
        let dots = dots(cells);
        self.stars = (0..cells * STARS_PER_CELL)
            .map(|_| {
                let mut star = self.rng.star(dots);
                star.x = self.rng.unit() * (star.reach + STAGGER) - STAGGER;
                star
            })
            .collect();
    }

    /// Move every star on by `elapsed`, replacing any that has burnt out with a
    /// new one at the name. A field laid out for a different width is scattered
    /// afresh instead, since its stars' reaches were picked for a trail it no
    /// longer has.
    pub fn advance(&mut self, elapsed: Duration, cells: usize) {
        if cells != self.cells {
            self.scatter(cells);
            return;
        }
        let dots = dots(cells);
        let secs = elapsed.as_secs_f32();
        for i in 0..self.stars.len() {
            let star = &mut self.stars[i];
            star.x += star.speed * secs;
            // Once, however long the pause: a star that burnt out a minute ago
            // is replaced like one that burnt out a frame ago.
            if star.x >= star.reach {
                let mut new = self.rng.star(dots);
                new.x = -self.rng.unit() * STAGGER;
                self.stars[i] = new;
            }
        }
    }

    /// The field as `cells` characters, the stars leaving from the left-hand
    /// end and flying right: braille where a star is, a space where no dot in
    /// the cell is lit. Clipped to `cells`, which may be narrower than the
    /// width the stars were laid out for when the terminal is; the cells kept
    /// are the ones nearest where the stars leave.
    pub fn render(&self, cells: usize) -> String {
        self.draw(cells, |dot| dot)
    }

    /// The same field run the other way: leaving from the right-hand end and
    /// flying left, clipped the same way.
    ///
    /// Mirrored dot by dot rather than by reversing [`Starfield::render`]'s
    /// cells, which would leave each cell's own pair of dot columns the right
    /// way round: a star would step left from cell to cell and right within
    /// each one.
    pub fn render_mirrored(&self, cells: usize) -> String {
        self.draw(cells, |dot| cells * 2 - 1 - dot)
    }

    /// Light each star's dot, at the column `place` puts it in, and spell the
    /// cells out.
    fn draw(&self, cells: usize, place: impl Fn(usize) -> usize) -> String {
        let mut bits = vec![0u32; cells];
        for star in &self.stars {
            if star.x < 0.0 {
                continue;
            }
            let dot = star.x as usize;
            if dot >= cells * 2 {
                continue;
            }
            let dot = place(dot);
            bits[dot / 2] |= DOTS[star.row][dot % 2];
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

/// xorshift64*: small, fast, and plenty for where some dots go. Shared with
/// [`super::dust`], which needs no more of its randomness than this does.
pub(super) struct Rng(u64);

impl Rng {
    pub(super) fn new(seed: u64) -> Self {
        // Zero is the one state xorshift never leaves.
        Self(seed | 1)
    }

    pub(super) fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Uniform in `[0, 1)`.
    pub(super) fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// A star with a fresh row, speed and reach for a field `dots` across, not
    /// yet placed. The reach is squared towards the short end — see the module
    /// docs.
    fn star(&mut self, dots: f32) -> Star {
        let row = (self.next() % 4) as usize;
        let speed = SLOWEST * (FASTEST / SLOWEST).powf(self.unit());
        let reach = SHORTEST + (dots + OVERSHOOT - SHORTEST) * self.unit().powi(2);
        Star {
            x: 0.0,
            row,
            speed,
            reach,
        }
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
    /// what keeps the trail the length the renderer measured it as.
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

    /// Mirrored dot for dot: every lit dot of the field drawn one way is lit
    /// in the opposite place drawn the other — including within a cell, where
    /// the left and right dot columns swap.
    #[test]
    fn the_mirrored_field_is_the_same_stars_the_other_way_round() {
        let mut f = Starfield::new(3);
        f.stars = vec![
            Star {
                x: 0.5,
                row: 0,
                speed: SLOWEST,
                reach: 9.0,
            },
            Star {
                x: 3.2,
                row: 2,
                speed: SLOWEST,
                reach: 9.0,
            },
        ];
        // Dot 0 of cell 0 and dot 3 (cell 1, right column).
        assert_eq!(f.render(3), "\u{2801}\u{2820} ");
        // Mirrored over six dots: dot 5 (cell 2, right column) and dot 2
        // (cell 1, left column).
        assert_eq!(f.render_mirrored(3), " \u{2804}\u{2808}");
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
        f.advance(Duration::from_millis(1), 10);
        for (was, now) in before.iter().zip(&f.stars) {
            if now.reach != was.reach {
                continue; // burnt out and replaced in that step
            }
            assert!(now.x > was.x, "{was:?} did not move right: {now:?}");
            assert_eq!(now.row, was.row);
        }
    }

    #[test]
    fn a_star_that_burns_out_is_replaced_at_the_name_with_new_picks() {
        let mut f = field(TRAIL);
        f.stars = vec![Star {
            x: 4.9,
            row: 0,
            speed: FASTEST,
            reach: 5.0,
        }];
        f.advance(Duration::from_millis(100), TRAIL);
        let new = f.stars[0];
        assert!(new.x < 0.0 && new.x >= -STAGGER, "started at {}", new.x);
        assert!((SLOWEST..=FASTEST).contains(&new.speed));
        assert!(new.row < 4);
        assert!((SHORTEST..=dots(TRAIL) + OVERSHOOT).contains(&new.reach));
    }

    /// Every pick is made afresh, so across a few hundred replacements the
    /// field has used every row and a spread of speeds and reaches.
    #[test]
    fn rows_speeds_and_reaches_are_not_fixed() {
        let mut f = field(TRAIL);
        let mut rows = [false; 4];
        let (mut slow, mut fast, mut short, mut long) = (false, false, false, false);
        for _ in 0..2000 {
            f.advance(Duration::from_millis(50), TRAIL);
            for s in &f.stars {
                assert!((SLOWEST..=FASTEST).contains(&s.speed), "{s:?}");
                rows[s.row] = true;
                slow |= s.speed < 25.0;
                fast |= s.speed > 60.0;
                short |= s.reach < 4.0;
                long |= s.reach > dots(TRAIL);
            }
        }
        assert_eq!(rows, [true; 4]);
        assert!(slow && fast, "speeds never spread out");
        assert!(short && long, "reaches never spread out");
    }

    /// What the reach is for: over time, the half of the trail by the name
    /// holds well over twice the stars of the far half.
    #[test]
    fn the_field_is_thickest_by_the_name() {
        let mut f = field(TRAIL);
        let half = dots(TRAIL) / 2.0;
        let (mut near, mut far) = (0usize, 0usize);
        for _ in 0..5000 {
            f.advance(Duration::from_millis(16), TRAIL);
            for s in &f.stars {
                if (0.0..half).contains(&s.x) {
                    near += 1;
                } else if (half..2.0 * half).contains(&s.x) {
                    far += 1;
                }
            }
        }
        assert!(far > 0, "nothing ever reached the far half");
        assert!(near > 2 * far, "near {near}, far {far}");
    }

    /// A long pause — a frame the terminal was too busy to draw — replaces each
    /// star once rather than losing any.
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
