//! The swap: what a session in flight does to the rows it trades places with.
//!
//! A move is one key and one jump. The session takes its new row, the rows it
//! passed close up behind it, and drawn as that and nothing more the eye is
//! left to work out which way everything went. So each move also puts three
//! things on the screen, all of them over within a quarter of a second:
//!
//! 1. **Sidestep** ([`SIDESTEP`]): the session lands [`ASIDE`] cells to the
//!    right of its column, every session it passed lands as far to the left,
//!    and both slide back in: two rows that stepped aside to get past each
//!    other. They are back in the column long before the next key at an
//!    ordinary pace; under a held key the session rides a cell or two out of
//!    line until it stops.
//! 2. **Echo** ([`ECHO`]): the rows the session crossed keep its bar for a
//!    moment and let it go under the sessions that moved into them. This is the
//!    cursor's afterglow ([`super::effects`]) on every row the move crossed,
//!    each a little behind the one before, so the row next to the session is
//!    the last to let go.
//! 3. **Sparks**: a burst of dots out of both ends of the seam the two rows
//!    slid past each other on, along the rows of dots either side of it, flying
//!    out and apart and dimming as they slow. A burst outlives the move that
//!    made it, so a quick run of moves leaves several in the air, never more
//!    than [`BURSTS`].
//!
//! A new move takes the sidestep and the echo over from the last one, the way
//! the next move of the cursor takes its glow: only the latest move's rows are
//! out of line or lit.
//!
//! # Cells, braille and colour
//!
//! The sidestep moves whole cells and the sparks are braille, which is one
//! column wide on every terminal (see [`super::starfield`]); neither sets a
//! colour. The echo is the one part in colour, and falls back to a modifier
//! without one, as the afterglow does.
//!
//! # Time, and the seed
//!
//! Moved on by elapsed time. Nothing is kept per spark: where one is and how
//! bright it is are worked out from its burst's seed and age, so the same seed
//! and age always draw the same frame.

use std::time::Duration;

use super::landing::Side;
use super::starfield::Rng;

/// How far a sidestep takes a row out of the column, in cells.
pub const ASIDE: u16 = 2;

/// How long the rows take to slide back into the column. Most of the way back
/// within the first third: the step aside is the answer to the key, and the
/// slide back is only it settling.
pub const SIDESTEP: Duration = Duration::from_millis(120);

/// How long a row the session crossed holds its bar. A little longer than the
/// cursor's afterglow, since a move is the bigger thing to follow.
pub const ECHO: Duration = Duration::from_millis(120);

/// How far behind the row before it each row the session crossed lets go,
/// on a move of more than one row.
const ECHO_STAGGER: Duration = Duration::from_millis(18);

/// Sparks to a burst, shared between the two ends and the two rows of dots.
const SPARKS: usize = 10;

/// The most bursts in the air at once. A held key makes one every repeat, and
/// the oldest is let go to keep the seam from filling up with them.
pub const BURSTS: usize = 4;

/// The nearest a spark gets, and how much further it may, in dots.
const NEAREST: f32 = 4.0;
const FURTHER: f32 = 13.0;

/// The shortest a spark lasts, and how much longer it may, in seconds.
const SHORTEST: f32 = 0.11;
const LONGER: f32 = 0.09;

/// The most a spark leaves after the move, in seconds.
const STAGGER: f32 = 0.025;

/// The most a spark drifts away from the seam as it goes, in rows of dots:
/// up off the row above it, down off the row below.
const DRIFT: f32 = 3.0;

/// The part of its life a spark is drawn with a second dot behind it.
const STREAK: f32 = 0.3;

/// Past this part of its life a spark is dim.
const FAR: f32 = 0.45;

/// The longest a burst lasts, from the move to the last spark going out.
pub const SPARKING: Duration =
    Duration::from_millis(((STAGGER + SHORTEST + LONGER) * 1000.0) as u64);

/// The latest move: the session carried, the sessions it moved past, nearest
/// first, and how long ago.
#[derive(Debug, Clone)]
struct Pass {
    carried: String,
    passed: Vec<String>,
    age: Duration,
}

/// One burst of sparks: where it went off, its seed, and how long ago.
#[derive(Debug, Clone)]
struct Burst {
    seam: usize,
    seed: u64,
    age: Duration,
}

/// One dot of a spark in the air, for the renderer, placed against the seam
/// it flew from and the end of the list it left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dot {
    /// The seam: the top edge of the visible row with this index.
    pub seam: usize,
    pub side: Side,
    /// Columns of dots out from the end of the list: 0 is the column of dots
    /// next to it.
    pub out: u16,
    /// Rows of dots below the seam: 0 is the top row of dots of the row under
    /// it, -1 the bottom row of the row over it.
    pub level: i16,
    pub dim: bool,
}

/// The swap under way. See the module docs.
pub struct Swap {
    pass: Option<Pass>,
    bursts: Vec<Burst>,
    rng: Rng,
}

impl Swap {
    pub fn new(seed: u64) -> Self {
        Self {
            pass: None,
            bursts: Vec::new(),
            rng: Rng::new(seed),
        }
    }

    /// A swap seeded from the operating system, or from a constant if it will
    /// not say. Only the look of the sparks depends on the seed.
    pub fn seeded() -> Self {
        Self::new(getrandom::u64().unwrap_or(0xd1b5_4a32_d192_ed03))
    }

    /// The session `carried` has just moved past the sessions `passed`,
    /// nearest first, and the two rows that traded places last meet at the
    /// top edge of visible row `seam`.
    pub fn start(&mut self, carried: String, passed: Vec<String>, seam: usize) {
        self.pass = Some(Pass {
            carried,
            passed,
            age: Duration::ZERO,
        });
        if self.bursts.len() == BURSTS {
            self.bursts.remove(0);
        }
        let seed = self.rng.next();
        self.bursts.push(Burst {
            seam,
            seed,
            age: Duration::ZERO,
        });
    }

    /// Everything off the screen at once: the rows it was drawn over have gone,
    /// or gone back to where they were.
    pub fn clear(&mut self) {
        self.pass = None;
        self.bursts.clear();
    }

    pub fn advance(&mut self, elapsed: Duration) {
        if let Some(pass) = &mut self.pass {
            pass.age += elapsed;
            if pass.age >= SIDESTEP.max(ECHO) {
                self.pass = None;
            }
        }
        for burst in &mut self.bursts {
            burst.age += elapsed;
        }
        self.bursts.retain(|b| b.age < SPARKING);
    }

    /// Whether anything is still moving.
    pub fn moving(&self) -> bool {
        self.pass.is_some() || !self.bursts.is_empty()
    }

    /// How many cells the row of session `id` is drawn out of the column now:
    /// right for the session carried, left for one it passed, and 0 for any
    /// other row or once they have slid back.
    pub fn aside(&self, id: &str) -> i16 {
        let Some(pass) = &self.pass else {
            return 0;
        };
        let way = if pass.carried == id {
            1
        } else if pass.passed.iter().any(|p| p == id) {
            -1
        } else {
            return 0;
        };
        let k = pass.age.as_secs_f32() / SIDESTEP.as_secs_f32();
        way * (f32::from(ASIDE) * (1.0 - ease_out_cubic(k))).round() as i16
    }

    /// The sessions on the rows the move crossed, each with how far its row is
    /// through letting the bar go, `0..1`. The row next to the session carried
    /// starts last; one that has let go is left out.
    pub fn echoes(&self) -> impl Iterator<Item = (&str, f32)> {
        self.pass.iter().flat_map(|pass| {
            pass.passed.iter().enumerate().filter_map(move |(i, id)| {
                let age = pass.age + ECHO_STAGGER * i as u32;
                let progress = age.as_secs_f32() / ECHO.as_secs_f32();
                (progress < 1.0).then_some((id.as_str(), progress))
            })
        })
    }

    /// Every dot of every spark in the air now.
    pub fn sparks(&self) -> Vec<Dot> {
        let mut dots = Vec::new();
        for burst in &self.bursts {
            let since = burst.age.as_secs_f32();
            for i in 0..SPARKS {
                let mut rng =
                    Rng::new(burst.seed ^ (i as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15));
                let reach = NEAREST + rng.unit() * FURTHER;
                let life = SHORTEST + rng.unit() * LONGER;
                let delay = rng.unit() * STAGGER;
                let drift = rng.unit() * DRIFT;
                let k = (since - delay) / life;
                if !(0.0..1.0).contains(&k) {
                    continue;
                }
                let side = if i % 2 == 0 {
                    Side::After
                } else {
                    Side::Before
                };
                // Two of every four on the row of dots over the seam, drifting
                // up; the other two on the row under it, drifting down.
                let over = i % 4 < 2;
                let level = if over {
                    -1 - (drift * ease_out(k)).round() as i16
                } else {
                    (drift * ease_out(k)).round() as i16
                };
                let out = (ease_out(k) * reach).round() as u16;
                let dim = k > FAR;
                let mut dot = Dot {
                    seam: burst.seam,
                    side,
                    out,
                    level,
                    dim,
                };
                dots.push(dot);
                if k < STREAK && out > 0 {
                    dot.out = out - 1;
                    dots.push(dot);
                }
            }
        }
        dots
    }
}

/// Fast and slowing, the way thrown sparks do.
fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t) * (1.0 - t)
}

/// Faster off the mark than [`ease_out`], so a row is most of the way back
/// in the column within the first third of the sidestep.
fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(carried: &str, passed: &[&str], seam: usize) -> Swap {
        let mut s = Swap::new(5);
        s.start(
            carried.to_string(),
            passed.iter().map(|p| p.to_string()).collect(),
            seam,
        );
        s
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// On the key the session is all the way out to the right and the rows it
    /// passed all the way out to the left; one step in a moment later; back in
    /// the column once the sidestep is over. Nothing else moves.
    #[test]
    fn the_rows_step_aside_and_slide_back() {
        let mut s = started("b", &["a"], 1);
        assert_eq!(s.aside("b"), ASIDE as i16, "the session carried, right");
        assert_eq!(s.aside("a"), -(ASIDE as i16), "the session passed, left");
        assert_eq!(s.aside("c"), 0, "anything else stays put");

        s.advance(ms(20));
        assert_eq!(s.aside("b"), 1, "a cell back in");
        assert_eq!(s.aside("a"), -1);

        s.advance(SIDESTEP);
        assert_eq!(s.aside("b"), 0, "back in the column");
        assert_eq!(s.aside("a"), 0);
    }

    /// Each row the session crossed lets its bar go a little behind the one
    /// before it, so the nearest row is the last to; all of them are plain once
    /// the echo is over.
    #[test]
    fn the_rows_crossed_let_go_nearest_last() {
        let mut s = started("d", &["c", "b", "a"], 3);
        let echoes: Vec<(&str, f32)> = s.echoes().collect();
        assert_eq!(
            echoes.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            ["c", "b", "a"]
        );
        assert_eq!(echoes[0].1, 0.0, "the nearest row has only just been left");
        assert!(echoes[0].1 < echoes[1].1 && echoes[1].1 < echoes[2].1);

        s.advance(ECHO);
        assert_eq!(s.echoes().count(), 0, "every row has let go");
    }

    /// A new move takes the sidestep and the echo over from the last one.
    #[test]
    fn the_next_move_takes_the_rows_over() {
        let mut s = started("b", &["a"], 1);
        s.advance(ms(10));
        s.start("b".to_string(), vec!["c".to_string()], 2);
        assert_eq!(s.aside("a"), 0, "the last move's row is back in");
        assert_eq!(s.aside("c"), -(ASIDE as i16));
        let echoes: Vec<&str> = s.echoes().map(|(id, _)| id).collect();
        assert_eq!(echoes, ["c"]);
    }

    /// Sparks go off both ends of the seam, on the rows of dots either side of
    /// it, and only ever outwards and apart: the row over the seam's drift up
    /// and the row under it's drift down. Every one is out by [`SPARKING`].
    #[test]
    fn sparks_fly_out_of_both_ends_of_the_seam() {
        let mut s = started("b", &["a"], 4);
        let mut sides = (false, false);
        let mut seen = 0;
        while s.moving() {
            s.advance(ms(16));
            for dot in s.sparks() {
                seen += 1;
                assert_eq!(dot.seam, 4);
                assert!(
                    (-1 - DRIFT as i16..=DRIFT as i16).contains(&dot.level),
                    "a spark strayed from the seam: {dot:?}"
                );
                match dot.side {
                    Side::After => sides.0 = true,
                    Side::Before => sides.1 = true,
                }
            }
        }
        assert!(seen > 0, "no sparks");
        assert!(sides.0 && sides.1, "sparks off one end only");
        assert!(s.sparks().is_empty());
    }

    /// A spark slows as it goes and dims late in its life: its dots get further
    /// out, and the far ones are the dim ones.
    #[test]
    fn sparks_go_out_and_dim() {
        let mut s = started("b", &["a"], 1);
        s.advance(ms(30));
        let early = s.sparks();
        s.advance(ms(80));
        let late = s.sparks();
        let furthest = |dots: &[Dot]| dots.iter().map(|d| d.out).max().unwrap_or(0);
        assert!(
            furthest(&late) > furthest(&early),
            "{early:?} then {late:?}"
        );
        assert!(
            early.iter().all(|d| !d.dim),
            "dim from the start: {early:?}"
        );
        assert!(late.iter().any(|d| d.dim), "never dims: {late:?}");
    }

    /// A held key makes a burst every repeat, and the oldest goes once there
    /// are [`BURSTS`].
    #[test]
    fn a_held_key_keeps_only_the_latest_bursts() {
        let mut s = Swap::new(9);
        for seam in 0..BURSTS + 3 {
            s.start("b".to_string(), vec!["a".to_string()], seam);
            s.advance(ms(1));
        }
        assert_eq!(s.bursts.len(), BURSTS);
        let seams: Vec<usize> = s.bursts.iter().map(|b| b.seam).collect();
        assert_eq!(seams, (3..BURSTS + 3).collect::<Vec<_>>());
    }

    /// The same seed and age draw the same sparks: nothing is kept per spark.
    #[test]
    fn the_same_seed_and_age_draw_the_same_sparks() {
        let mut one = started("b", &["a"], 2);
        let mut two = started("b", &["a"], 2);
        one.advance(ms(40));
        two.advance(ms(40));
        assert_eq!(one.sparks(), two.sparks());
    }

    /// Once everything is over, nothing is moving and nothing is drawn; and
    /// clearing ends it all at once.
    #[test]
    fn it_ends_and_can_be_cleared() {
        let mut s = started("b", &["a"], 1);
        s.advance(SPARKING.max(SIDESTEP).max(ECHO));
        assert!(!s.moving());
        assert_eq!(s.aside("b"), 0);

        let mut s = started("b", &["a"], 1);
        s.clear();
        assert!(!s.moving());
        assert_eq!(s.echoes().count(), 0);
        assert!(s.sparks().is_empty());
    }
}
