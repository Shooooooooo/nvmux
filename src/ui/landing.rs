//! The landing: what happens to a session in flight when it is put down.
//!
//! The trail ([`super::starfield`]) says a session is in the air; this says it
//! has come down, and has to be seen to, or a move ends on the trail simply
//! stopping. Four beats, all within half a second of the key:
//!
//! 1. **Pull** ([`PULL`]): the stars of both trails are drawn back into the
//!    row, as if the row caught them. The row still looks in flight — the `⇕`
//!    and the blank number column — until the end of this beat.
//! 2. **Impact** ([`IMPACT`]): for a moment the reversed bar is two cells wider
//!    at each end, and on that same frame the marker and the numbers come
//!    back. It is the frame the eye reads as the landing.
//! 3. **Flinch** ([`FLINCH`]): the rows directly above and below go dim, as if
//!    jolted, and come back.
//! 4. **Spray**: dust shoots straight out of both ends of the row, level with
//!    the text, and settles.
//!
//! # Level with the text
//!
//! The spray is braille built from the two middle rows of dots only: `⠶` at
//! first, thinning to one row (`⠒` or `⠤`) and then to one dot. Half the
//! grains keep to the upper middle row and half to the lower, so the spray is
//! centred on the name rather than hanging above or below it. Braille is one
//! column on every terminal and sets no colour, so, like the trail, this is
//! correct under `NO_COLOR` by construction; the impact and the flinch are the
//! reversed and dim modifiers.
//!
//! # Time, and the seed
//!
//! Moved on by elapsed time. Nothing is kept per grain: where a grain is and
//! what it shows are worked out from the seed and the age, so the same seed
//! and age always draw the same frame.

use std::time::Duration;

use super::starfield::Rng;

/// How long the trail takes to be pulled into the row.
pub const PULL: Duration = Duration::from_millis(70);

/// How long the bar stays widened, from the end of the pull.
pub const IMPACT: Duration = Duration::from_millis(55);

/// How long the rows beside it stay dim, from the end of the pull.
pub const FLINCH: Duration = Duration::from_millis(90);

/// How many cells the bar widens by at each end on impact.
pub const WIDEN: u16 = 2;

/// Grains of dust, shared between the two ends.
const GRAINS: usize = 14;

/// The nearest a grain settles to the row, and how much further it may go,
/// in cells.
const NEAREST: f32 = 2.0;
const FURTHER: f32 = 7.0;

/// The shortest a grain lasts, and how much longer it may, in seconds.
const SHORTEST: f32 = 0.20;
const LONGER: f32 = 0.12;

/// The most a grain leaves after the impact, in seconds.
const STAGGER: f32 = 0.04;

/// Past this part of its life a grain is dim.
const FAR: f32 = 0.45;

/// A grain's glyph through its life, flying right, for each of the two lanes:
/// both middle rows of dots, then one, then a single dot at the leading edge.
const UPPER: [char; 5] = ['⠶', '⠒', '⠒', '⠂', '⠐'];
const LOWER: [char; 5] = ['⠶', '⠤', '⠤', '⠄', '⠠'];

/// The longest a landing runs, from the key to the last grain settling.
pub const LENGTH: Duration =
    Duration::from_millis(70 + ((STAGGER + SHORTEST + LONGER) * 1000.0) as u64);

/// Which end of the row a grain leaves from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Off the marker, flying left.
    Before,
    /// Off the end of the name, flying right.
    After,
}

/// One grain of the spray, for the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grain {
    pub side: Side,
    /// Cells out from the end of the row it left, past the cell next to it:
    /// 1 leaves that one cell clear.
    pub offset: u16,
    pub glyph: char,
    pub dim: bool,
}

/// One landing. See the module docs.
#[derive(Debug, Clone)]
pub struct Landing {
    seed: u64,
    age: Duration,
}

impl Landing {
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            age: Duration::ZERO,
        }
    }

    /// A landing seeded from the operating system, or from a constant if it
    /// will not say. Only the look of the spray depends on the seed.
    pub fn seeded() -> Self {
        Self::new(getrandom::u64().unwrap_or(0x94d0_49bb_1331_11eb))
    }

    pub fn advance(&mut self, elapsed: Duration) {
        self.age += elapsed;
    }

    pub fn done(&self) -> bool {
        self.age >= LENGTH
    }

    /// How far the trail has been pulled in, `0..1`, while it is being —
    /// `None` once the row has landed.
    pub fn pulling(&self) -> Option<f32> {
        (self.age < PULL).then(|| self.age.as_secs_f32() / PULL.as_secs_f32())
    }

    /// Whether this is the moment of impact, when the bar is widened.
    pub fn impact(&self) -> bool {
        self.since_impact().is_some_and(|t| t < IMPACT)
    }

    /// Whether the rows beside the landed one are flinching.
    pub fn flinch(&self) -> bool {
        self.since_impact().is_some_and(|t| t < FLINCH)
    }

    /// The dust in the air now.
    pub fn spray(&self) -> Vec<Grain> {
        let Some(since) = self.since_impact() else {
            return Vec::new();
        };
        let since = since.as_secs_f32();
        (0..GRAINS)
            .filter_map(|i| {
                let mut rng =
                    Rng::new(self.seed ^ (i as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15));
                let reach = NEAREST + rng.unit() * FURTHER;
                let life = SHORTEST + rng.unit() * LONGER;
                let delay = rng.unit() * STAGGER;
                let k = (since - delay) / life;
                if !(0.0..1.0).contains(&k) {
                    return None;
                }
                let side = if i % 2 == 0 {
                    Side::After
                } else {
                    Side::Before
                };
                let lane = if (i / 2) % 2 == 0 { &UPPER } else { &LOWER };
                let glyph = lane[((k * lane.len() as f32) as usize).min(lane.len() - 1)];
                Some(Grain {
                    side,
                    offset: 1 + (ease_out(k) * reach).round() as u16,
                    glyph: match side {
                        Side::After => glyph,
                        Side::Before => mirror(glyph),
                    },
                    dim: k > FAR,
                })
            })
            .collect()
    }

    fn since_impact(&self) -> Option<Duration> {
        self.age.checked_sub(PULL)
    }
}

/// The same braille cell with its two columns of dots swapped, so a grain
/// flying left leads with the dot on its left.
fn mirror(c: char) -> char {
    let bits = c as u32 - 0x2800;
    let swapped = [(0x01, 0x08), (0x02, 0x10), (0x04, 0x20), (0x40, 0x80)]
        .iter()
        .fold(0, |acc, &(left, right)| {
            acc | if bits & left != 0 { right } else { 0 }
                | if bits & right != 0 { left } else { 0 }
        });
    char::from_u32(0x2800 + swapped).expect("braille patterns are chars")
}

/// Fast off the row and slowing, the way thrown dust settles.
fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t) * (1.0 - t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Landing {
        let mut l = Landing::new(3);
        l.advance(Duration::from_millis(ms));
        l
    }

    /// The beats come in order: pulling, then the impact and the flinch
    /// together, the flinch outlasting the impact, then only the spray.
    #[test]
    fn the_beats_come_in_order() {
        let pull = at(30);
        assert!(pull.pulling().is_some_and(|p| p > 0.0 && p < 1.0));
        assert!(!pull.impact() && !pull.flinch());
        assert!(pull.spray().is_empty(), "no dust before the impact");

        let impact = at(80);
        assert_eq!(impact.pulling(), None);
        assert!(impact.impact() && impact.flinch());

        let flinch = at(140);
        assert!(!flinch.impact() && flinch.flinch());

        let spray = at(250);
        assert!(!spray.impact() && !spray.flinch());
        assert!(!spray.spray().is_empty(), "dust in the air");
    }

    /// Every grain is braille lit only in the two middle rows of dots, so the
    /// spray stays level with the text.
    #[test]
    fn the_spray_is_level_with_the_text() {
        const MIDDLE: u32 = 0x02 | 0x04 | 0x10 | 0x20;
        for ms in (0..=LENGTH.as_millis() as u64).step_by(5) {
            for g in at(ms).spray() {
                let bits = g.glyph as u32 - 0x2800;
                assert!(bits != 0 && bits & !MIDDLE == 0, "{g:?} at {ms} ms");
            }
        }
    }

    /// Both ends throw dust, it goes outwards and stays within its reach, and
    /// it dims before it goes.
    #[test]
    fn dust_flies_out_of_both_ends_and_dims() {
        let early = at(90).spray();
        assert!(early.iter().any(|g| g.side == Side::After));
        assert!(early.iter().any(|g| g.side == Side::Before));
        for ms in (70..=LENGTH.as_millis() as u64).step_by(5) {
            for g in at(ms).spray() {
                assert!(g.offset >= 1 && g.offset <= 1 + (NEAREST + FURTHER) as u16);
            }
        }
        let late = at(LENGTH.as_millis() as u64 - 15).spray();
        assert!(late.iter().all(|g| g.dim), "{late:?}");
    }

    /// A grain flying left is the mirror image of one flying right.
    #[test]
    fn grains_flying_left_are_mirrored() {
        assert_eq!(mirror('⠂'), '⠐');
        assert_eq!(mirror('⠐'), '⠂');
        assert_eq!(mirror('⠶'), '⠶');
        assert_eq!(mirror('⠄'), '⠠');
    }

    /// It ends on time, with nothing left in the air.
    #[test]
    fn it_ends_on_time() {
        assert!(!at(LENGTH.as_millis() as u64 - 1).done());
        let end = at(LENGTH.as_millis() as u64);
        assert!(end.done());
        assert!(end.spray().is_empty());
        assert!(LENGTH <= Duration::from_millis(450), "{LENGTH:?}");
    }
}
