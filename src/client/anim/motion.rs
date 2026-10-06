//! A window moving: Neovide's `position_animation_length`.
//!
//! When the editor puts a window, a float or the message area somewhere new —
//! a split opening beside it, `<C-w>L`, a float following the cursor, messages
//! pushing up — the grid slides there rather than appearing. Each grid that
//! moves gets a spring per axis for how far it still is from where the editor
//! put it, drawn at the nearest whole cell — the nearest rather than the one
//! towards the target, as a scroll's is (see [`super::scroll`]), so that a
//! window moved and then moved back part way carries on from exactly where
//! it is drawn.
//!
//! A grid that is new, or shown again after being hidden — the window of
//! another tab page — is simply put where it goes: there is nowhere it is
//! coming from.

use super::spring::Spring;

/// One grid's way still to go.
#[derive(Debug, Clone, Copy, Default)]
pub struct Motion {
    row: Spring,
    col: Spring,
}

impl Motion {
    /// The editor moved the grid by `by` (rows, columns): it is that much
    /// further from where it goes now, and its speed is kept.
    pub fn moved(&mut self, by: (i64, i64)) {
        self.row.position -= by.0 as f32;
        self.col.position -= by.1 as f32;
    }

    /// How far it is drawn from where the editor put it this frame, in whole
    /// cells.
    pub fn offset(&self) -> (i64, i64) {
        (self.row.nearest(), self.col.nearest())
    }

    /// Move on `dt` seconds of a slide that settles in `duration`. Says
    /// whether it is still going.
    pub fn step(&mut self, dt: f32, duration: f32) -> bool {
        self.row.step(dt, duration);
        self.col.step(dt, duration);
        self.offset() != (0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Moved ten columns right, it is drawn where it was and slides there.
    #[test]
    fn a_moved_grid_slides_from_where_it_was() {
        let mut m = Motion::default();
        m.moved((0, 10));
        assert_eq!(m.offset(), (0, -10));
        let mut cols = vec![m.offset().1];
        while m.step(0.004, 0.15) {
            cols.push(m.offset().1);
        }
        assert_eq!(m.offset(), (0, 0));
        assert!(
            cols.windows(2).all(|w| w[0] <= w[1]),
            "never back: {cols:?}"
        );
    }

    /// Moved again part way, it carries on from where it is drawn.
    #[test]
    fn moved_again_it_carries_on() {
        let mut m = Motion::default();
        m.moved((4, 0));
        m.step(0.03, 0.15);
        let drawn = m.offset().0;
        m.moved((-4, 0));
        assert_eq!(m.offset().0, drawn + 4);
    }
}
