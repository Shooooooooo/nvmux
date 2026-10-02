//! The sonar: rings rippling out from the session you came back to the picker
//! from, so you can see at a glance which one `Esc` returns to.
//!
//! `<prefix> Space` opens the picker with the cursor already on that session,
//! but a cursor is easy to take for wherever the list happened to start. Three
//! rings leave both ends of its row, a beat apart, and thin out as they go —
//! all within two thirds of a second of the picker coming up, and ended early
//! by any key, since by then the eye has either found the row or moved on.
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

/// The longest the sonar runs: the last ring's start, and then its life.
pub const LENGTH: Duration =
    Duration::from_millis(((RINGS - 1) as u64) * GAP.as_millis() as u64 + LIFE.as_millis() as u64);

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

    pub fn done(&self) -> bool {
        self.age >= LENGTH
    }

    /// The rings out now, oldest — furthest out — first.
    pub fn rings(&self) -> Vec<Ring> {
        (0..RINGS)
            .filter_map(|i| {
                let since = self.age.checked_sub(GAP * i)?;
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
        for ms in (0..LENGTH.as_millis() as u64).step_by(5) {
            for r in at(ms).rings() {
                assert!(braille(r.left) && braille(r.right), "{r:?} at {ms} ms");
            }
        }
        for (l, r) in [ABOVE, BELOW] {
            assert!(braille(l) && braille(r));
        }
    }

    /// It ends on time, with nothing left.
    #[test]
    fn it_ends_on_time() {
        assert!(!at(LENGTH.as_millis() as u64 - 1).done());
        let end = at(LENGTH.as_millis() as u64);
        assert!(end.done());
        assert!(end.rings().is_empty());
        assert!(LENGTH <= Duration::from_millis(700), "{LENGTH:?}");
    }
}
