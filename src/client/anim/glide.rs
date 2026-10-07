//! A distance closed in a set time, however far: what a window's scroll
//! slides by (see [`super::scroll`]).
//!
//! A spring (see [`super::spring`]) settles in about its duration from rest,
//! but a distance added to one part way carries on at the speed it had, and
//! the further it has to go the longer it takes: pages typed one after
//! another pile up, and the last lands well after its key. A glide ends a set
//! time after it last set off, whatever it had to cover: a distance added
//! part way sets it off again from where it is, going as much faster as it
//! takes to cover what was left and what was added in that time. However
//! many pages are typed, the last lands that long after it was.
//!
//! It eases out, as a cubic does: fastest at first — the text answers the key
//! at once — and slowing to a stop exactly at the end, never past it.

/// A distance still to go, and how it is being closed.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Glide {
    /// What there was to go when it last set off, and how long ago that was,
    /// in seconds.
    from: f32,
    elapsed: f32,
    /// What there is to go now.
    position: f32,
}

impl Glide {
    /// `position` from the target, setting off now.
    pub fn at(position: f32) -> Self {
        Self {
            from: position,
            elapsed: 0.0,
            position,
        }
    }

    /// What there is still to go.
    pub fn position(&self) -> f32 {
        self.position
    }

    /// `distance` more to go: it sets off again, to cover that and what it
    /// had left in a duration from now.
    pub fn add(&mut self, distance: f32) {
        if distance != 0.0 {
            *self = Self::at(self.position + distance);
        }
    }

    /// `position` to go, from now: as [`Glide::add`], to a distance rather
    /// than by one.
    pub fn set(&mut self, position: f32) {
        if position != self.position {
            *self = Self::at(position);
        }
    }

    /// Move on `dt` seconds of a glide that ends `duration` seconds after it
    /// sets off. Says whether it is still moving; one that has stopped is
    /// exactly at its target.
    ///
    /// Where it is follows from how long ago it set off, not from where it
    /// was a frame ago: a frame that comes late costs no accuracy.
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

    /// The whole cells still to go, rounded towards the target: a scroll
    /// shows its first row as soon as it has moved at all, and a scroll of
    /// one row is no slower than none. See [`super::scroll`].
    pub fn cells(&self) -> i64 {
        self.position.trunc() as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Where a glide from `from` is every `dt` until it stops, and how long
    /// that took.
    fn run(g: &mut Glide, dt: f32, duration: f32) -> (Vec<f32>, f32) {
        let (mut seen, mut t) = (vec![g.position()], 0.0);
        while g.step(dt, duration) {
            seen.push(g.position());
            t += dt;
        }
        (seen, t + dt)
    }

    /// However far it has to go, a glide ends when its duration is up, and
    /// never sooner: fastest at first, slower all the way, never past.
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

    /// A distance added part way is covered with what was left by the time
    /// the glide would have ended had it set off then: pages one after
    /// another land a duration after the last, however many there were.
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
        // The other way: what was left is undone as it goes back.
        let mut g = Glide::at(20.0);
        g.step(0.05, 0.15);
        g.add(-20.0);
        assert!(g.position() < 0.0);
        let (seen, _) = run(&mut g, 0.001, 0.15);
        assert!(seen.iter().all(|p| *p <= 0.0), "past the target");
    }

    /// Where it is depends only on when it set off.
    #[test]
    fn a_late_frame_costs_no_accuracy() {
        let (mut a, mut b) = (Glide::at(30.0), Glide::at(30.0));
        a.step(0.06, 0.15);
        for _ in 0..6 {
            b.step(0.01, 0.15);
        }
        assert!((a.position() - b.position()).abs() < 1e-4);
    }

    /// Whole cells round towards the target: a row's glide shows its row
    /// gone at once.
    #[test]
    fn whole_cells_round_towards_the_target() {
        let mut g = Glide::at(1.0);
        assert_eq!(g.cells(), 1);
        g.step(0.001, 0.15);
        assert_eq!(g.cells(), 0);
        assert_eq!(Glide::at(-2.5).cells(), -2);
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
