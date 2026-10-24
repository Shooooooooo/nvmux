//! Animation for a session moved past other rows in the picker, so the eye
//! can follow which way things went. Each move shows, for under 250 ms:
//!
//! 1. **Sidestep** ([`SIDESTEP`]): the moved session starts [`ASIDE`] cells
//!    right, the passed ones as far left, and both slide back.
//! 2. **Echo** ([`ECHO`]): the crossed rows keep the bar briefly, staggered so
//!    the row next to the session fades last (the cursor afterglow from
//!    [`super::effects`]).
//! 3. **Sparks**: braille dots fly out of both ends of the seam between the
//!    swapped rows. Bursts outlive their move, up to [`BURSTS`] at once.
//!
//! A new move replaces the previous sidestep and echo. Only the echo uses
//! colour (falling back to a modifier). Sparks are derived from their burst's
//! seed and age, so the same seed and age draw the same frame.

use std::time::Duration;

use super::landing::Side;
use super::starfield::Rng;

/// Sidestep offset, in cells.
pub const ASIDE: u16 = 2;

pub const SIDESTEP: Duration = Duration::from_millis(120);

/// A little longer than the cursor's afterglow.
pub const ECHO: Duration = Duration::from_millis(120);

/// Delay between successive crossed rows' echoes.
const ECHO_STAGGER: Duration = Duration::from_millis(18);

/// Sparks per burst, split across both ends and both dot rows.
const SPARKS: usize = 10;

/// Max bursts at once; a held key makes one per repeat and the oldest is
/// dropped.
pub const BURSTS: usize = 4;

/// Spark reach: minimum and extra random range, in dots.
const NEAREST: f32 = 4.0;
const FURTHER: f32 = 13.0;

/// Spark life: minimum and extra random range, in seconds.
const SHORTEST: f32 = 0.11;
const LONGER: f32 = 0.09;

/// Max spark start delay, in seconds.
const STAGGER: f32 = 0.025;

/// Max vertical drift away from the seam, in dot rows.
const DRIFT: f32 = 3.0;

/// Life fraction during which a spark has a trailing second dot.
const STREAK: f32 = 0.3;

/// Life fraction after which a spark is dim.
const FAR: f32 = 0.45;

/// Max burst duration.
pub const SPARKING: Duration =
    Duration::from_millis(((STAGGER + SHORTEST + LONGER) * 1000.0) as u64);

/// The latest move; `passed` is nearest first.
#[derive(Debug, Clone)]
struct Pass {
    carried: String,
    passed: Vec<String>,
    age: Duration,
}

#[derive(Debug, Clone)]
struct Burst {
    seam: usize,
    seed: u64,
    age: Duration,
}

/// One spark dot, positioned relative to its seam and list end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dot {
    /// Top edge of the visible row with this index.
    pub seam: usize,
    pub side: Side,
    /// Dot columns out from the list end; 0 is adjacent.
    pub out: u16,
    /// Dot rows below the seam: 0 is the top dot row of the row below, -1 the
    /// bottom dot row of the row above.
    pub level: i16,
    pub dim: bool,
}

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

    /// Seeded from the OS, or a constant if that fails. The seed only affects
    /// the sparks.
    pub fn seeded() -> Self {
        Self::new(getrandom::u64().unwrap_or(0xd1b5_4a32_d192_ed03))
    }

    /// `carried` just moved past `passed` (nearest first); the last two rows
    /// to swap meet at the top edge of visible row `seam`.
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

    /// Stop everything, e.g. when the rows drawn over are gone.
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

    pub fn moving(&self) -> bool {
        self.pass.is_some() || !self.bursts.is_empty()
    }

    /// Current column offset of `id`'s row: positive for the carried session,
    /// negative for passed ones, else 0.
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

    /// Crossed sessions with their echo progress `0..1`; finished ones are
    /// omitted.
    pub fn echoes(&self) -> impl Iterator<Item = (&str, f32)> {
        self.pass.iter().flat_map(|pass| {
            pass.passed.iter().enumerate().filter_map(move |(i, id)| {
                let age = pass.age + ECHO_STAGGER * i as u32;
                let progress = age.as_secs_f32() / ECHO.as_secs_f32();
                (progress < 1.0).then_some((id.as_str(), progress))
            })
        })
    }

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
                // Half drift up from the dot row above the seam, half down
                // from the one below.
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

/// Quadratic ease-out.
fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t) * (1.0 - t)
}

/// Gets a row most of the way back within the first third of the sidestep.
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

    #[test]
    fn the_same_seed_and_age_draw_the_same_sparks() {
        let mut one = started("b", &["a"], 2);
        let mut two = started("b", &["a"], 2);
        one.advance(ms(40));
        two.advance(ms(40));
        assert_eq!(one.sparks(), two.sparks());
    }

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
