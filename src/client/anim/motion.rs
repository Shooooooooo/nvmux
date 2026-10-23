//! Window slides: Neovide's `position_animation_length`.
//!
//! A moved float, message area or rearranged split (`<C-w>x`) slides to its new
//! place; splits opening, closing or resizing animate in [`super::layout`]
//! instead. Each axis is a spring drawn at the nearest cell (not rounded toward
//! the target as in [`super::scroll`]), so a move reversed part way continues
//! from where it is drawn. New or re-shown grids are placed without sliding.

use super::spring::Spring;

/// A grid's remaining offset from where the editor put it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Motion {
    row: Spring,
    col: Spring,
}

impl Motion {
    /// Add a move of `by` (rows, cols) to the distance left; velocity is kept.
    pub fn moved(&mut self, by: (i64, i64)) {
        self.row.position -= by.0 as f32;
        self.col.position -= by.1 as f32;
    }

    /// Drawn offset from the editor's position, in whole cells.
    pub fn offset(&self) -> (i64, i64) {
        (self.row.nearest(), self.col.nearest())
    }

    /// Advance `dt` seconds of a slide lasting `duration`; true while moving.
    pub fn step(&mut self, dt: f32, duration: f32) -> bool {
        self.row.step(dt, duration);
        self.col.step(dt, duration);
        self.offset() != (0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
