//! The sonar: rings rippling out from the session you came back to the picker
//! from, so you can see at a glance which one `Esc` returns to.
//!
//! `<prefix> Space` opens the picker with the cursor already on that session,
//! but a cursor is easy to take for wherever the list happened to start. Three
//! rings leave both ends of its row, a beat apart, and thin out as they go:
//! a pulse of two thirds of a second, sent out again every [`PERIOD`] for as
//! long as the picker is up and the session is listed.
//!
//! Keys do not stop it. `Esc` goes back to that session after a move, a
//! filter or a rename just as it does on arrival, so the row is worth marking
//! for as long as that is true — and a ring only ever lands on the terminal's
//! own background, never on a row, so it is in nobody's way. Between pulses
//! nothing moves, and the picker sleeps until the next is due rather than
//! drawing frames that would all be the same (see [`Sonar::until_next`]).
//!
//! # Braille, in three weights
//!
//! A ring's edge is a column of braille dots: all four of the cell's column
//! near the row, then the middle two, then one, as it spreads (`⡇` `⠆` `⠂`
//! on the left, mirrored on the right). Once it is two cells out, the ring
//! also curves over the rows above and below, one cell further in, as a
//! single dot on the side nearest the row (`⡀` above, `⠁` below), so the three
//! rows together read as a ring rather than as two brackets. Braille is one
//! column on every terminal and sets no colour; the colour, where there is
//! any, is the post-pass's (see [`super::effects`]).
//!
//! # Time
//!
//! Moved on by elapsed time. Nothing is kept per ring: where each is and what
//! it shows are worked out from the age, so the same age always draws the same
//! frame.

use std::time::Duration;

/// How many rings leave the row.
const RINGS: u32 = 3;

/// How long after one ring the next leaves.
const GAP: Duration = Duration::from_millis(150);

/// How long one ring lasts, from the row to where it fades out.
const LIFE: Duration = Duration::from_millis(320);

/// The furthest a ring gets, in cells past the one next to the row.
const REACH: f32 = 7.0;

/// The parts of a ring's life it is drawn in its heavier weights: the full
/// column of dots until the first, the middle two until the second, and one
/// after that.
const FULL: f32 = 0.3;
const HALF: f32 = 0.65;

/// How far out a ring has to be before it curves over the rows beside its own.
const ARC_FROM: u16 = 2;

/// One pulse: the last ring's start, and then its life.
pub const PULSE: Duration =
    Duration::from_millis(((RINGS - 1) as u64) * GAP.as_millis() as u64 + LIFE.as_millis() as u64);

/// From the start of one pulse to the start of the next. Long enough that the
/// rest is most of it — a ping, not a flicker going on beside the list — and
/// short enough that a glance at the picker meets one before long.
pub const PERIOD: Duration = Duration::from_millis(2500);

/// A ring's edge on the row's own line, left of the row and right of it.
const EDGES: [(char, char); 3] = [('⡇', '⢸'), ('⠆', '⠰'), ('⠂', '⠐')];

/// A ring's arc on the row above, left and right: the bottom dot nearest the
/// row.
pub const ABOVE: (char, char) = ('⡀', '⢀');

/// A ring's arc on the row below, left and right: the top dot nearest the row.
pub const BELOW: (char, char) = ('⠁', '⠈');

/// One ring now, for the renderer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ring {
    /// Cells out from the row's ends, past the cell next to it: 0 is that
    /// cell.
    pub out: u16,
    /// Its edge on the row's own line, left and right.
    pub left: char,
    pub right: char,
    /// How far through its life it is, `0..1`, for the renderer's fade.
    pub life: f32,
}

impl Ring {
    /// How far out its arcs over the rows above and below are, on the same
    /// measure as [`Ring::out`], once it has spread far enough to have them.
    pub fn arc(&self) -> Option<u16> {
        (self.out >= ARC_FROM).then(|| self.out - 1)
    }
}

/// One sonar. See the module docs.
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

    /// How far into the current period it is: in a pulse while this is under
    /// [`PULSE`], resting after.
    fn phase(&self) -> Duration {
        let nanos = self.age.as_nanos() % PERIOD.as_nanos();
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(0))
    }

    /// Whether a pulse is going out now, rather than the sonar resting.
    pub fn pulsing(&self) -> bool {
        self.phase() < PULSE
    }

    /// How long until the next pulse starts — nothing, while one is going.
    /// What lets the caller sleep through a rest rather than draw it.
    pub fn until_next(&self) -> Duration {
        let phase = self.phase();
        if phase < PULSE {
            Duration::ZERO
        } else {
            PERIOD - phase
        }
    }

    /// The rings out now, oldest — furthest out — first. None while resting.
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

/// Quick off the row and slowing, the way a ripple spreads.
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

    /// The rings leave a beat apart: one at first, then two, then three.
    #[test]
    fn the_rings_leave_one_after_another() {
        assert_eq!(at(0).rings().len(), 1);
        assert_eq!(at(160).rings().len(), 2);
        assert_eq!(at(310).rings().len(), 3);
    }

    /// A ring only ever spreads, and thins as it does: the full column of dots
    /// near the row, one dot by the end.
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

    /// The arcs come only once a ring is clear of the row, and sit a cell
    /// further in than its edge.
    #[test]
    fn arcs_follow_a_ring_once_it_is_clear() {
        let fresh = at(0).rings()[0];
        assert_eq!(fresh.arc(), None);
        let out = at(200).rings()[0];
        assert_eq!(out.arc(), Some(out.out - 1));
    }

    /// Every glyph is braille, which is one column wide on every terminal.
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

    /// A pulse runs its course and the sonar rests — no rings, and the time
    /// to the next one — and then the same pulse goes out again, on the
    /// period.
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
