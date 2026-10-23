//! Critically damped spring driving every animation.
//!
//! It animates the remaining distance (target 0), so retargeting mid-way adds
//! to the distance and keeps the velocity; an easing curve restarted on each
//! `j` repeat would lurch. Critical damping never overshoots.

/// Time constants to settle: at `ω t = 6.64` a spring released from rest is
/// within 1% of its target, so `duration` is the settling time.
const SETTLE: f32 = 6.64;

/// Distance still to go, and how fast it is closing.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Spring {
    pub position: f32,
    pub velocity: f32,
}

impl Spring {
    pub fn at(position: f32) -> Self {
        Self {
            position,
            velocity: 0.0,
        }
    }

    /// Advance `dt` seconds towards settling in `duration`; returns whether
    /// still moving (once stopped, it is exactly on target). Exact solution,
    /// so a late frame loses no accuracy.
    pub fn step(&mut self, dt: f32, duration: f32) -> bool {
        if duration <= 0.0 || !duration.is_finite() {
            *self = Spring::default();
            return false;
        }
        let omega = SETTLE / duration;
        let a = self.position;
        let b = self.velocity + omega * a;
        let decay = (-omega * dt).exp();
        self.position = (a + b * dt) * decay;
        self.velocity = (self.velocity - omega * b * dt) * decay;
        // Under 0.01 cell and 0.5 cell/s: nothing visible is left to move.
        if self.position.abs() < 0.01 && self.velocity.abs() < 0.5 {
            *self = Spring::default();
            return false;
        }
        true
    }

    pub fn moving(&self) -> bool {
        self.position != 0.0 || self.velocity != 0.0
    }

    /// Whole cells to go, rounded towards the target.
    pub fn cells(&self) -> i64 {
        self.position.trunc() as i64
    }

    /// Whole cells to go, rounded to the nearest. Unlike [`Spring::cells`],
    /// whole cells added mid-way, either way, do not shift what is drawn (see
    /// [`super::motion`]).
    pub fn nearest(&self) -> i64 {
        self.position.round() as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spring_settles_in_its_duration_without_overshooting() {
        let mut s = Spring::at(10.0);
        let mut t = 0.0;
        let mut last = 10.0;
        while s.step(0.001, 0.2) {
            t += 0.001;
            assert!(s.position >= 0.0, "overshot at {t}: {}", s.position);
            assert!(s.position <= last, "turned back at {t}");
            last = s.position;
            if (t - 0.2f32).abs() < 0.0005 {
                assert!(s.position < 0.11, "{} left at the duration", s.position);
            }
        }
        assert!(t < 0.35, "still moving at {t}");
        assert_eq!(s, Spring::default());
    }

    #[test]
    fn a_late_frame_costs_no_accuracy() {
        let mut one = Spring::at(5.0);
        let mut many = Spring::at(5.0);
        one.step(0.05, 0.3);
        for _ in 0..50 {
            many.step(0.001, 0.3);
        }
        assert!((one.position - many.position).abs() < 1e-3);
        assert!((one.velocity - many.velocity).abs() < 1e-2);
    }

    #[test]
    fn a_new_distance_keeps_the_speed() {
        let mut s = Spring::at(4.0);
        s.step(0.02, 0.2);
        let v = s.velocity;
        s.position += 4.0;
        assert_eq!(s.velocity, v);
        assert!(s.step(0.01, 0.2));
    }

    #[test]
    fn whole_cells_round_towards_the_target_or_the_nearest() {
        assert_eq!(Spring::at(3.9).cells(), 3);
        assert_eq!(Spring::at(0.9).cells(), 0);
        assert_eq!(Spring::at(-2.5).cells(), -2);
        assert_eq!(Spring::at(3.9).nearest(), 4);
        assert_eq!(Spring::at(-2.4).nearest(), -2);
        // Shifted by whole cells, the nearest shifts by as many.
        for x in [-2.46f32, -0.3, 0.7, 1.2] {
            assert_eq!(Spring::at(x + 4.0).nearest(), Spring::at(x).nearest() + 4);
        }
    }

    #[test]
    fn no_duration_is_no_animation() {
        let mut s = Spring::at(3.0);
        assert!(!s.step(0.016, 0.0));
        assert_eq!(s, Spring::default());
    }
}
