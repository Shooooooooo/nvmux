//! Closes any distance in a fixed time; drives window scrolling (see
//! [`super::scroll`]).
//!
//! Unlike a [`super::spring`], adding distance mid-flight restarts the glide
//! from its current position, so repeated page keys still land one duration
//! after the last key instead of piling up. Cubic ease-out: fastest at first,
//! stops exactly at the target without overshoot.

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Glide {
    /// Distance at the last (re)start, and seconds since then.
    from: f32,
    elapsed: f32,
    /// Distance remaining.
    position: f32,
}

impl Glide {
    /// Start `position` away from the target.
    pub fn at(position: f32) -> Self {
        Self {
            from: position,
            elapsed: 0.0,
            position,
        }
    }

    pub fn position(&self) -> f32 {
        self.position
    }

    /// Add `distance` and restart, covering it plus what was left in one
    /// duration from now.
    pub fn add(&mut self, distance: f32) {
        if distance != 0.0 {
            *self = Self::at(self.position + distance);
        }
    }

    /// Like [`Glide::add`], but sets the remaining distance absolutely.
    pub fn set(&mut self, position: f32) {
        if position != self.position {
            *self = Self::at(position);
        }
    }

    /// Advance `dt` seconds of a glide lasting `duration`. Returns whether it
    /// is still moving; once stopped it is exactly at the target. Position is
    /// computed from elapsed time, so late frames lose no accuracy.
    pub fn step(&mut self, dt: f32, duration: f32) -> bool {
        if duration <= 0.0 || !duration.is_finite() {
            *self = Self::default();
            return false;
        }
        self.elapsed += dt;
        let left = 1.0 - self.elapsed / duration;
        if left <= 0.0 {
            *self = Self::default();
            return false;
        }
        self.position = self.from * left * left * left;
        true
    }

    pub fn moving(&self) -> bool {
        self.position != 0.0
    }

    /// Whole cells left, rounded towards the target. An exact whole distance
    /// counts one cell less, so a scroll shows its first row immediately and
    /// a one-row scroll is not slower than none.
    pub fn cells(&self) -> i64 {
        let whole = self.position.trunc();
        let cells = if whole == self.position && whole != 0.0 {
            whole - self.position.signum()
        } else {
            whole
        };
        cells as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Positions every `dt` until it stops, and the time taken.
    fn run(g: &mut Glide, dt: f32, duration: f32) -> (Vec<f32>, f32) {
        let (mut seen, mut t) = (vec![g.position()], 0.0);
        while g.step(dt, duration) {
            seen.push(g.position());
            t += dt;
        }
        (seen, t + dt)
    }

    #[test]
    fn a_glide_ends_on_time_however_far() {
        for from in [1.0, 20.0, -20.0, 500.0] {
            let mut g = Glide::at(from);
            let (seen, took) = run(&mut g, 0.001, 0.15);
            assert!((took - 0.15).abs() < 0.0015, "{from}: took {took}");
            assert_eq!(g.position(), 0.0);
            let steps: Vec<f32> = seen.windows(2).map(|w| (w[0] - w[1]).abs()).collect();
            assert!(
                steps.windows(2).all(|w| w[1] <= w[0] + 1e-4),
                "{from}: not slowing all the way"
            );
            assert!(
                seen.iter().all(|p| p * from >= 0.0),
                "{from}: past the target"
            );
        }
    }

    #[test]
    fn a_distance_added_part_way_ends_a_duration_after_it() {
        let mut g = Glide::at(20.0);
        for _ in 0..10 {
            for _ in 0..50 {
                g.step(0.001, 0.15);
            }
            let left = g.position();
            assert!(left > 0.0);
            g.add(20.0);
            assert_eq!(g.position(), left + 20.0);
        }
        let (_, took) = run(&mut g, 0.001, 0.15);
        assert!((took - 0.15).abs() < 0.0015, "took {took}");
        // Reversing direction mid-glide.
        let mut g = Glide::at(20.0);
        g.step(0.05, 0.15);
        g.add(-20.0);
        assert!(g.position() < 0.0);
        let (seen, _) = run(&mut g, 0.001, 0.15);
        assert!(seen.iter().all(|p| *p <= 0.0), "past the target");
    }

    #[test]
    fn a_late_frame_costs_no_accuracy() {
        let (mut a, mut b) = (Glide::at(30.0), Glide::at(30.0));
        a.step(0.06, 0.15);
        for _ in 0..6 {
            b.step(0.01, 0.15);
        }
        assert!((a.position() - b.position()).abs() < 1e-4);
    }

    #[test]
    fn whole_cells_round_towards_the_target() {
        assert_eq!(Glide::at(20.0).cells(), 19);
        assert_eq!(Glide::at(-20.0).cells(), -19);
        assert_eq!(Glide::at(1.0).cells(), 0);
        assert_eq!(Glide::at(-2.5).cells(), -2);
        assert_eq!(Glide::at(0.4).cells(), 0);
        assert_eq!(Glide::default().cells(), 0);
        let mut g = Glide::at(20.0);
        g.step(0.001, 0.15);
        assert_eq!(g.cells(), 19, "a hair in, as it showed");
        let mut g = Glide::at(5.0);
        g.set(5.0);
        assert_eq!(g, Glide::at(5.0), "the same distance is no new start");
    }

    #[test]
    fn no_duration_is_no_animation() {
        let mut g = Glide::at(9.0);
        assert!(!g.step(0.016, 0.0));
        assert!(!g.moving());
    }
}
