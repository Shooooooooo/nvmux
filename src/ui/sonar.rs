//! Sonar: rings rippling out from both ends of a picker row, one pulse every
//! [`PERIOD`].
//!
//! Currently unused; kept with its renderer ([`super::effects`]) for reuse.
//! Rings land only on the terminal background, never on a row. Between pulses
//! nothing moves, so callers can sleep (see [`Sonar::until_next`]).
//!
//! A ring's edge is a braille column that thins from four dots to two to one
//! (`⡇` `⠆` `⠂`, mirrored on the right). From two cells out it also arcs over
//! the rows above and below, one cell further in (`⡀` above, `⠁` below).
//! Braille is one column wide everywhere; colour comes from the post-pass.
//!
//! State is just the age: the same age always draws the same frame.

use std::time::Duration;

const RINGS: u32 = 3;

/// Delay between ring starts.
const GAP: Duration = Duration::from_millis(150);

const LIFE: Duration = Duration::from_millis(320);

/// Max distance, in cells past the one next to the row.
const REACH: f32 = 7.0;

/// Life fractions at which the edge drops from four dots to two, then one.
const FULL: f32 = 0.3;
const HALF: f32 = 0.65;

/// Distance at which a ring starts arcing over adjacent rows.
const ARC_FROM: u16 = 2;

/// Last ring's start plus its life.
pub const PULSE: Duration =
    Duration::from_millis(((RINGS - 1) as u64) * GAP.as_millis() as u64 + LIFE.as_millis() as u64);

/// Pulse start to pulse start; mostly rest, so it reads as a ping.
pub const PERIOD: Duration = Duration::from_millis(2500);

/// Edge glyphs (left, right) by weight.
const EDGES: [(char, char); 3] = [('⡇', '⢸'), ('⠆', '⠰'), ('⠂', '⠐')];

/// Arc on the row above (left, right): the bottom dot nearest the row.
pub const ABOVE: (char, char) = ('⡀', '⢀');

/// Arc on the row below (left, right): the top dot nearest the row.
pub const BELOW: (char, char) = ('⠁', '⠈');

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ring {
    /// Cells out from the row's ends; 0 is the adjacent cell.
    pub out: u16,
    pub left: char,
    pub right: char,
    /// Fraction of its life elapsed, `0..1`, for the fade.
    pub life: f32,
}

impl Ring {
    /// Distance of the arcs over adjacent rows (same measure as
    /// [`Ring::out`]), once the ring is far enough out to have them.
    pub fn arc(&self) -> Option<u16> {
        (self.out >= ARC_FROM).then(|| self.out - 1)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Sonar {
    age: Duration,
}

impl Sonar {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn advance(&mut self, elapsed: Duration) {
        self.age += elapsed;
    }

    /// Time into the current period; pulsing while under [`PULSE`].
    fn phase(&self) -> Duration {
        let nanos = self.age.as_nanos() % PERIOD.as_nanos();
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(0))
    }

    pub fn pulsing(&self) -> bool {
        self.phase() < PULSE
    }

    /// Time until the next pulse; zero while one is running.
    pub fn until_next(&self) -> Duration {
        let phase = self.phase();
        if phase < PULSE {
            Duration::ZERO
        } else {
            PERIOD - phase
        }
    }

    /// Current rings, oldest (furthest out) first; empty while resting.
    pub fn rings(&self) -> Vec<Ring> {
        let phase = self.phase();
        (0..RINGS)
            .filter_map(|i| {
                let since = phase.checked_sub(GAP * i)?;
                let life = since.as_secs_f32() / LIFE.as_secs_f32();
                if life >= 1.0 {
                    return None;
                }
                let (left, right) = if life < FULL {
                    EDGES[0]
                } else if life < HALF {
                    EDGES[1]
                } else {
                    EDGES[2]
                };
                Some(Ring {
                    out: (ease_out(life) * REACH).round() as u16,
                    left,
                    right,
                    life,
                })
            })
            .collect()
    }
}

/// Quadratic ease-out.
fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t) * (1.0 - t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Sonar {
        let mut s = Sonar::new();
        s.advance(Duration::from_millis(ms));
        s
    }

    #[test]
    fn the_rings_leave_one_after_another() {
        assert_eq!(at(0).rings().len(), 1);
        assert_eq!(at(160).rings().len(), 2);
        assert_eq!(at(310).rings().len(), 3);
    }

    #[test]
    fn a_ring_spreads_and_thins() {
        let near = at(10).rings()[0];
        let far = at(300).rings()[0];
        assert!(far.out > near.out, "{near:?} then {far:?}");
        assert!(far.out <= REACH as u16);
        assert_eq!((near.left, near.right), ('⡇', '⢸'));
        assert_eq!((far.left, far.right), ('⠂', '⠐'));
        let dots = |c: char| (c as u32 - 0x2800).count_ones();
        assert!(dots(far.left) < dots(near.left));
    }

    #[test]
    fn arcs_follow_a_ring_once_it_is_clear() {
        let fresh = at(0).rings()[0];
        assert_eq!(fresh.arc(), None);
        let out = at(200).rings()[0];
        assert_eq!(out.arc(), Some(out.out - 1));
    }

    /// Braille is one column wide on every terminal.
    #[test]
    fn every_glyph_is_braille() {
        let braille = |c: char| (0x2801..=0x28ff).contains(&(c as u32));
        for ms in (0..PULSE.as_millis() as u64).step_by(5) {
            for r in at(ms).rings() {
                assert!(braille(r.left) && braille(r.right), "{r:?} at {ms} ms");
            }
        }
        for (l, r) in [ABOVE, BELOW] {
            assert!(braille(l) && braille(r));
        }
    }

    #[test]
    fn it_pulses_rests_and_pulses_again() {
        let pulse = PULSE.as_millis() as u64;
        let period = PERIOD.as_millis() as u64;
        assert!(at(pulse - 1).pulsing());
        let resting = at(pulse);
        assert!(!resting.pulsing());
        assert!(resting.rings().is_empty());
        assert_eq!(resting.until_next(), PERIOD - PULSE);
        assert!(at(period - 1).rings().is_empty(), "still resting");
        assert!(at(period).pulsing(), "on the period");
        assert_eq!(at(period + 10).rings(), at(10).rings(), "the same pulse");
        assert_eq!(at(3 * period + 200).rings(), at(200).rings(), "and again");
        assert_eq!(at(10).until_next(), Duration::ZERO, "mid-pulse");
        assert!(PULSE <= Duration::from_millis(700), "{PULSE:?}");
        assert!(PERIOD > PULSE * 2, "more rest than pulse");
    }
}
