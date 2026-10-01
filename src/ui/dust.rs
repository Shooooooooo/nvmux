//! The dust a killed session's row crumbles into: each character turns into a
//! braille cell that drifts right, loses its dots and goes.
//!
//! Played once a kill has been confirmed and before it runs (see
//! [`crate::ui`]'s kill arm), so by the time the transport is asked, the row is
//! already gone from the screen and the wait for the kill and the fresh
//! listing reads as the list settling rather than as nothing happening. It
//! cannot run *during* the kill: the transport is not `Send` (see
//! [`crate::dirs`]), and a kill over ssh blocks the thread that would draw.
//!
//! # A sweep, not a blast
//!
//! The characters do not all go at once. Each one starts a little after the
//! one to its left, so the row comes apart left to right like a fuse, with a
//! jitter on top so neighbours do not move in step. The sweep is capped at
//! [`SPREAD`] however long the row, so a long name costs no more than a short
//! one: the whole effect is never longer than [`LENGTH`].
//!
//! Each grain shows its own character for a moment, so the eye sees *which*
//! letter is going, then a braille cell of up to six dots that thins to one as
//! it drifts. Its dots are picked afresh every [`FLICKER`], which is what reads
//! as dust rather than as a glyph sliding sideways. The far half of a grain's
//! life is dim, the same step down the trail uses (see [`super::starfield`]).
//!
//! # Braille, and nothing else
//!
//! Like the trail: one column wide on every terminal, no colour, so the effect
//! is correct under `NO_COLOR` by construction. A wide character crumbles into
//! one braille cell in its first column; its second column simply empties.
//!
//! # Time, and the seed
//!
//! Moved on by elapsed time, like everything else that moves here. Nothing is
//! kept per grain: where a grain is and which dots it shows are worked out from
//! its index, the seed and the age, so the same seed and age always draw the
//! same frame — which is what lets a test say what a frame looks like.

use std::time::Duration;

use unicode_width::UnicodeWidthChar;

use super::starfield::Rng;

/// How long one grain lasts once it starts, in seconds.
const LIFE: f32 = 0.30;

/// How long after the row's first grain its last one may start, at most.
const SPREAD: f32 = 0.14;

/// How much later each character starts than the one before it, until
/// [`SPREAD`] caps it.
const PER_CHAR: f32 = 0.022;

/// The most a grain starts late on top of its place in the sweep.
const JITTER: f32 = 0.06;

/// The part of a grain's life it still shows its own character for.
const WHOLE: f32 = 0.1;

/// Past this part of its life a grain is dim.
const FAR: f32 = 0.45;

/// The most dots a grain has, at the start of its drift. Six of eight, so even
/// the densest grain reads as dots and not as a block.
const MOST_DOTS: f32 = 6.0;

/// The furthest right a grain drifts, in cells. Each one picks its own, between
/// one and this.
const MAX_DRIFT: f32 = 4.0;

/// How long a grain keeps one set of dots before picking another.
const FLICKER: f32 = 0.05;

/// The longest the dust can run, however long the row: the last grain's
/// latest start, and then its life.
pub const LENGTH: Duration = Duration::from_millis(((SPREAD + JITTER + LIFE) * 1000.0) as u64);

/// The eight dots of a braille cell, in no particular order.
const DOT_BITS: [u32; 8] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80];

/// The first braille pattern, which has no dots.
const BRAILLE: u32 = 0x2800;

/// One cell of dust, for the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Speck {
    /// Columns from the start of the row.
    pub column: u16,
    pub glyph: char,
    pub dim: bool,
}

/// One row's dust. See the module docs.
#[derive(Debug, Clone)]
pub struct Dust {
    seed: u64,
    /// Seconds since the first grain could start.
    age: f32,
}

impl Dust {
    pub fn new(seed: u64) -> Self {
        Self { seed, age: 0.0 }
    }

    /// Dust seeded from the operating system, or from a constant if it will
    /// not say. Only the look of the dust depends on the seed.
    pub fn seeded() -> Self {
        Self::new(getrandom::u64().unwrap_or(0x2545_f491_4f6c_dd1d))
    }

    pub fn advance(&mut self, elapsed: Duration) {
        self.age += elapsed.as_secs_f32();
    }

    /// Whether every grain has gone, whatever the row was.
    pub fn done(&self) -> bool {
        self.age >= LENGTH.as_secs_f32()
    }

    /// What is left of `text` now: the characters not yet started, where they
    /// were, and a speck for each grain still drifting. Blanks are never
    /// listed, and nor are grains that have burnt out.
    ///
    /// Columns can run past the end of `text` by up to [`MAX_DRIFT`]; the
    /// caller clips them to the screen.
    pub fn render(&self, text: &str) -> Vec<Speck> {
        let mut specks = Vec::new();
        let mut column = 0u16;
        for (i, c) in text.chars().enumerate() {
            let width = c.width().unwrap_or(0) as u16;
            let at = column;
            column += width;
            // A blank has nothing to crumble, and the caller drew the row
            // empty: it only takes up its columns.
            if width == 0 || c == ' ' {
                continue;
            }
            let grain = self.grain(i);
            let life = (self.age - grain.start) / LIFE;
            if life < 0.0 {
                specks.push(Speck {
                    column: at,
                    glyph: c,
                    dim: false,
                });
                continue;
            }
            if life >= 1.0 {
                continue;
            }
            if life < WHOLE {
                specks.push(Speck {
                    column: at,
                    glyph: c,
                    dim: false,
                });
                continue;
            }
            let drift = (grain.drift * ease_out(life)).round() as u16;
            let dots = (MOST_DOTS * (1.0 - life)).round().max(1.0) as usize;
            let flicker = (self.age / FLICKER) as u64;
            specks.push(Speck {
                column: at + drift,
                glyph: self.cell(i, flicker, dots),
                dim: life > FAR,
            });
        }
        specks
    }

    /// Where grain `i` sits in the sweep and how far it goes.
    fn grain(&self, i: usize) -> Grain {
        let mut rng = Rng::new(self.seed ^ (i as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let start = (i as f32 * PER_CHAR).min(SPREAD) + rng.unit() * JITTER;
        let drift = 1.0 + rng.unit() * (MAX_DRIFT - 1.0);
        Grain { start, drift }
    }

    /// A braille cell with `dots` of its eight dots lit, picked by grain and by
    /// flicker step.
    fn cell(&self, i: usize, flicker: u64, dots: usize) -> char {
        let mut rng = Rng::new(
            self.seed ^ (i as u64 + 1).wrapping_mul(0xbf58_476d_1ce4_e5b9) ^ flicker << 32,
        );
        let mut pool = DOT_BITS;
        let mut bits = 0;
        // A partial shuffle: each pick swaps a fresh dot to the front of what
        // is left, so the dots are distinct and exactly `dots` are lit.
        for k in 0..dots.min(pool.len()) {
            let pick = k + (rng.next() % (pool.len() - k) as u64) as usize;
            pool.swap(k, pick);
            bits |= pool[k];
        }
        char::from_u32(BRAILLE + bits).expect("braille patterns are chars")
    }
}

struct Grain {
    /// Seconds after the dust starts.
    start: f32,
    /// Cells it drifts by the end of its life.
    drift: f32,
}

/// Fast off the mark and slowing, the way something blown away would.
fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t) * (1.0 - t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Dust {
        let mut d = Dust::new(11);
        d.advance(Duration::from_millis(ms));
        d
    }

    fn is_braille(c: char) -> bool {
        (0x2801..=0x28ff).contains(&(c as u32))
    }

    /// Before anything starts the row is exactly itself, blanks left out.
    #[test]
    fn at_the_start_the_row_is_whole() {
        let specks = at(0).render("▸ 3  notes");
        let drawn: String = specks.iter().map(|s| s.glyph).collect();
        assert_eq!(drawn, "▸3notes");
        assert_eq!(specks[0].column, 0);
        assert_eq!(specks[1].column, 2);
        assert_eq!(specks[2].column, 5, "the name starts after the gap");
        assert!(specks.iter().all(|s| !s.dim));
    }

    /// Partway through, the row is coming apart from the left: braille near the
    /// start, letters still standing at the end.
    #[test]
    fn it_comes_apart_from_the_left() {
        let text = "api-server-with-a-long-name";
        let specks = at(150).render(text);
        assert!(
            specks.iter().any(|s| is_braille(s.glyph)),
            "nothing crumbling: {specks:?}"
        );
        let last = specks.last().expect("something left");
        assert_eq!(last.glyph, 'e', "the end of the row is still standing");
        assert!(
            !specks.iter().any(|s| s.glyph == 'a' && s.column == 0),
            "the first letter has gone to dust"
        );
    }

    /// Every speck is one column, and grains only ever drift right, by at most
    /// the drift limit.
    #[test]
    fn specks_are_one_column_and_drift_only_right() {
        let text = "dotfiles";
        for ms in (0..=LENGTH.as_millis() as u64).step_by(10) {
            for s in at(ms).render(text) {
                assert_eq!(s.glyph.width(), Some(1), "{s:?} at {ms} ms");
                assert!(
                    s.column < text.len() as u16 + MAX_DRIFT as u16,
                    "{s:?} drifted too far at {ms} ms"
                );
            }
        }
    }

    /// The dust thins and dims: late in a grain's life it has fewer dots than
    /// early, and it is dim.
    #[test]
    fn grains_thin_out_and_dim() {
        let dots = |c: char| (c as u32 - BRAILLE).count_ones();
        let d = Dust::new(5);
        let early = d.cell(0, 0, 6);
        let late = d.cell(0, 0, 1);
        assert_eq!(dots(early), 6);
        assert_eq!(dots(late), 1);

        let late_frame = at(LENGTH.as_millis() as u64 - 20).render(&"x".repeat(20));
        assert!(!late_frame.is_empty(), "the last grains are still going");
        assert!(late_frame.iter().all(|s| s.dim && is_braille(s.glyph)));
    }

    /// The effect has a fixed end whatever the row, and at the end nothing is
    /// left of it.
    #[test]
    fn it_ends_on_time_with_nothing_left() {
        let long = "x".repeat(48);
        assert!(!at(LENGTH.as_millis() as u64 - 1).done());
        let end = at(LENGTH.as_millis() as u64);
        assert!(end.done());
        assert!(end.render(&long).is_empty(), "{:?}", end.render(&long));
        assert!(LENGTH <= Duration::from_millis(600), "{LENGTH:?}");
    }

    /// A wide character crumbles into one cell; the column after it is not
    /// counted twice.
    #[test]
    fn a_wide_character_is_one_grain() {
        let specks = at(0).render("日本");
        assert_eq!(specks.len(), 2);
        assert_eq!(specks[1].column, 2);
    }

    /// The same seed and age draw the same frame.
    #[test]
    fn a_frame_is_a_function_of_seed_and_age() {
        assert_eq!(at(200).render("scratch"), at(200).render("scratch"));
    }
}
