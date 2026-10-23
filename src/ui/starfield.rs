//! Trail of stars flying off the end of a picked-up session's name.
//!
//! Drawn in braille: each cell is a 2x4 dot grid and a star is one dot.
//! Braille is one column wide everywhere (unlike block elements in some CJK
//! locales) and needs no colour, so it works under `NO_COLOR`. Weighting is
//! the renderer's job (see [`super::draw`]).
//!
//! Each star picks a fresh row, speed and reach when it starts, so there are
//! no lanes. Speeds are log-uniform in [`SLOWEST`]..[`FASTEST`]; reaches are
//! squared, so most stars burn out near the name and the trail thins towards
//! its end. Replacements start slightly before the edge so they do not come
//! back in step.
//!
//! [`Starfield::ignite`] holds every star at the name and releases them over
//! [`IGNITE`] at a rising rate, so the trail builds up rather than appearing.
//!
//! Stars start straight off the name's bar: ramps between the two (quadrant
//! blocks, `▓▒░`, fading braille) and more stars were tried and read worse.
//!
//! Advanced by elapsed time, not frames (see [`crate::fade`]). Stateful, since
//! positions depend on every pick so far.

use std::time::Duration;

/// Trail length, in cells.
pub const TRAIL: usize = 6;

/// Star speed range, with [`FASTEST`], in dots per second.
const SLOWEST: f32 = 15.0;

const FASTEST: f32 = 80.0;

/// Minimum reach, in dots, so every star visibly moves.
const SHORTEST: f32 = 2.0;

/// Max reach past the trail's end, in dots, so a few stars fly off it.
const OVERSHOOT: f32 = 4.0;

/// Max distance before the left edge a new star starts, in dots.
const STAGGER: f32 = 2.0;

/// Time for every star to leave the name after [`Starfield::ignite`]. Short
/// enough to finish before the first move, long enough to be seen to build.
pub const IGNITE: Duration = Duration::from_millis(300);

/// More would turn the crowd by the name into a solid block.
const STARS_PER_CELL: usize = 2;

/// Bit for each (row, column) of a braille cell. Braille numbers dots down
/// the left column, then the right, with the bottom pair added last, hence
/// the order.
pub(super) const DOTS: [[u32; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];

/// Empty braille pattern.
pub(super) const BRAILLE: u32 = 0x2800;

#[derive(Debug, Clone, Copy, PartialEq)]
struct Star {
    /// Dots from the left edge; negative before it enters.
    x: f32,
    /// Dot row within the cell, from the top.
    row: usize,
    /// Dots per second.
    speed: f32,
    /// Dots from the left edge at which it burns out.
    reach: f32,
}

pub struct Starfield {
    stars: Vec<Star>,
    /// Width, in cells, the stars were laid out for.
    cells: usize,
    rng: Rng,
}

impl Starfield {
    /// Empty until the first [`Starfield::scatter`], [`Starfield::ignite`] or
    /// [`Starfield::advance`].
    pub fn new(seed: u64) -> Self {
        Self {
            stars: Vec::new(),
            cells: 0,
            rng: Rng::new(seed),
        }
    }

    /// Seeded from the OS, or a constant if that fails.
    pub fn seeded() -> Self {
        Self::new(getrandom::u64().unwrap_or(0x9e37_79b9_7f4a_7c15))
    }

    /// Lay out a field `cells` wide with stars at random points in their lives,
    /// so the trail has its shape from the first frame.
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

    /// Lay out a field `cells` wide with every star at the name, each leaving
    /// within [`IGNITE`].
    ///
    /// A star delayed by `delay` starts `speed * delay` behind the edge.
    /// Delays are `sqrt(uniform)`, so departures get denser over time.
    pub fn ignite(&mut self, cells: usize) {
        self.cells = cells;
        let dots = dots(cells);
        let build = IGNITE.as_secs_f32();
        self.stars = (0..cells * STARS_PER_CELL)
            .map(|_| {
                let mut star = self.rng.star(dots);
                let delay = self.rng.unit().sqrt() * build;
                star.x = -star.speed * delay;
                star
            })
            .collect();
    }

    /// Move stars on by `elapsed`, replacing burnt-out ones at the name. A
    /// width change re-scatters instead, since reaches depend on the width.
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
            // Replaced once however long the pause.
            if star.x >= star.reach {
                let mut new = self.rng.star(dots);
                new.x = -self.rng.unit() * STAGGER;
                self.stars[i] = new;
            }
        }
    }

    /// `cells` characters (braille or space), stars flying right. May be
    /// narrower than the laid-out width; the cells nearest the start are kept.
    pub fn render(&self, cells: usize) -> String {
        self.draw(cells, 0.0, |dot| dot)
    }

    /// Like [`Starfield::render`] but flying left. Mirrored per dot: reversing
    /// cells would leave each cell's two dot columns unswapped.
    pub fn render_mirrored(&self, cells: usize) -> String {
        self.draw(cells, 0.0, |dot| cells * 2 - 1 - dot)
    }

    /// Every star drawn `pull` (`0..1`) of the way back to its start, for the
    /// landing (see [`super::landing`]).
    pub fn render_pulled(&self, cells: usize, pull: f32, mirrored: bool) -> String {
        if mirrored {
            self.draw(cells, pull, |dot| cells * 2 - 1 - dot)
        } else {
            self.draw(cells, pull, |dot| dot)
        }
    }

    /// `place` maps a dot column to its drawn column.
    fn draw(&self, cells: usize, pull: f32, place: impl Fn(usize) -> usize) -> String {
        let mut bits = vec![0u32; cells];
        for star in &self.stars {
            if star.x < 0.0 {
                continue;
            }
            let dot = (star.x * (1.0 - pull.clamp(0.0, 1.0))) as usize;
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

fn dots(cells: usize) -> f32 {
    (cells * 2) as f32
}

/// xorshift64*; also used by [`super::landing`] and [`super::swap`].
pub(super) struct Rng(u64);

impl Rng {
    pub(super) fn new(seed: u64) -> Self {
        // xorshift is stuck at zero.
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

    /// A star with fresh picks for a field `dots` across, at `x = 0`.
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

    /// Mirroring also swaps the dot columns within a cell.
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

    fn lit(f: &Starfield, cells: usize) -> u32 {
        f.render(cells)
            .chars()
            .map(|c| (c as u32).saturating_sub(BRAILLE).count_ones())
            .sum()
    }

    #[test]
    fn an_ignited_field_starts_dark_and_has_caught_by_the_end() {
        let mut f = Starfield::new(7);
        f.ignite(TRAIL);
        assert_eq!(lit(&f, TRAIL), 0, "{:?}", f.render(TRAIL));
        assert_eq!(f.stars.len(), TRAIL * STARS_PER_CELL);
        let start = f.stars.clone();
        f.advance(IGNITE, TRAIL);
        for (was, now) in start.iter().zip(&f.stars) {
            assert!(
                now.reach != was.reach || now.x >= 0.0,
                "{was:?} was still waiting at the name: {now:?}"
            );
        }
    }

    #[test]
    fn an_ignited_field_builds_up() {
        let at = |ms: u64| -> u32 {
            (0..200)
                .map(|seed| {
                    let mut f = Starfield::new(seed);
                    f.ignite(TRAIL);
                    f.advance(Duration::from_millis(ms), TRAIL);
                    lit(&f, TRAIL)
                })
                .sum()
        };
        let (early, half, caught) = (at(16), at(150), at(400));
        assert!(early < half && half < caught, "{early} {half} {caught}");
    }

    #[test]
    fn the_same_seed_draws_the_same_field() {
        let (a, b) = (field(12), field(12));
        assert_eq!(a.render(12), b.render(12));
    }
}
