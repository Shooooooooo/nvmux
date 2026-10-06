//! A window scrolling: Neovide's smooth scroll, a row at a time.
//!
//! Neovim scrolls a window by sending its new lines — moving the ones it can
//! (`grid_scroll`), redrawing the rest — and saying in `win_viewport` how far
//! the view moved (`scroll_delta`). By then the lines the view left are gone
//! from the grid. So the client keeps, for each window scrolling, the lines
//! either side of what it shows — the view as it was, slid by the delta — and
//! draws the window from those and its own, offset by a spring that starts at
//! the delta and settles at nought. What the eye sees is the text sliding
//! through the window until it is where Neovim put it.
//!
//! The offset is whole rows — a terminal cannot draw text between them — and
//! it rounds towards where it is going, so the first row of a scroll moves at
//! once and a scroll of one row is no slower than none: there is nothing to
//! animate in one step, and a delay would be all there was to see.
//!
//! A jump further than the window is tall leaves a gap between the view that
//! was and the one that is, which nothing is known about. As Neovide's
//! `scroll_animation_far_lines` does, only that many rows of such a jump are
//! animated (`[effects.scroll] far_lines`), and they are drawn blank.
//!
//! Without `ext_multigrid` there are no windows to speak of, only grid 1 —
//! but `grid_scroll` there names the window's rectangle, and the scroll is
//! the window's when its rows match the current window's `scroll_delta` (see
//! `super::Animator::before`). It is the same animation over that rectangle.

use std::collections::VecDeque;

use super::spring::Spring;
use crate::client::compose::Scrolling;
use crate::client::grid::Cell;

/// A line of a scroll's rectangle, or `None` for one nothing is known of.
type Line = Option<Vec<Cell>>;

/// One rectangle scrolling.
#[derive(Debug)]
pub struct Scroll {
    /// `top..bot` × `left..right`, in its grid.
    rect: (usize, usize, usize, usize),
    /// The lines above the rectangle, nearest first.
    above: VecDeque<Line>,
    /// The rectangle's own, as Neovim last drew them.
    current: Vec<Vec<Cell>>,
    /// The lines below it, nearest first.
    below: VecDeque<Line>,
    /// How many rows the view is behind: above nought it shows lines above
    /// the rectangle's top — it has scrolled down the buffer and is catching
    /// up — below nought lines below its bottom.
    spring: Spring,
    shown: i64,
}

impl Scroll {
    /// A scroll of `delta` rows over `rect`, from the lines it showed before
    /// to the ones it shows now.
    pub fn new(
        rect: (usize, usize, usize, usize),
        before: Vec<Vec<Cell>>,
        after: Vec<Vec<Cell>>,
        delta: i64,
        far: usize,
    ) -> Self {
        let mut s = Self {
            rect,
            above: VecDeque::new(),
            current: before,
            below: VecDeque::new(),
            spring: Spring::default(),
            shown: 0,
        };
        s.scrolled(after, delta, far);
        s
    }

    pub fn rect(&self) -> (usize, usize, usize, usize) {
        self.rect
    }

    /// The view moved `delta` more rows, and its rectangle now holds
    /// `after`: the lines that left it are kept either side, and the offset
    /// grows by as much.
    pub fn scrolled(&mut self, after: Vec<Vec<Cell>>, delta: i64, far: usize) {
        let h = self.current.len();
        let d = delta.unsigned_abs() as usize;
        let old = std::mem::replace(&mut self.current, after);
        if d > h {
            // Past the whole view: nothing is known between the two, and
            // only `far` rows of it are animated.
            self.above.clear();
            self.below.clear();
            let rows = far.min(h) as f32;
            self.spring = Spring::at(if delta > 0 { rows } else { -rows });
            self.shown = self.spring.cells();
            return;
        }
        if delta > 0 {
            // The text moved up: its top `d` lines are now the nearest above.
            for line in old.into_iter().take(d) {
                self.above.push_front(Some(line));
            }
            self.below.drain(..d.min(self.below.len()));
        } else {
            for line in old.into_iter().skip(h - d).rev() {
                self.below.push_front(Some(line));
            }
            self.above.drain(..d.min(self.above.len()));
        }
        self.spring.position = (self.spring.position + delta as f32).clamp(-(h as f32), h as f32);
        // Kept as far as the view could reach, and no further.
        self.above.truncate(h + 1);
        self.below.truncate(h + 1);
        self.shown = self.spring.cells();
    }

    /// Neovim redrew the rectangle without scrolling it.
    pub fn refresh(&mut self, after: Vec<Vec<Cell>>) {
        if after.len() == self.current.len() {
            self.current = after;
        }
    }

    /// Move on `dt` seconds of a scroll that settles in `duration`. Says
    /// whether it is still going.
    ///
    /// Over once the offset is under a row: a spring that does not overshoot
    /// has nothing left to show after that.
    pub fn step(&mut self, dt: f32, duration: f32) -> bool {
        self.spring.step(dt, duration);
        self.shown = self.spring.cells();
        self.shown != 0
    }

    /// The whole rows the view is behind this frame.
    pub fn offset(&self) -> i64 {
        self.shown
    }
}

impl Scrolling for Scroll {
    fn rect(&self) -> (usize, usize, usize, usize) {
        self.rect
    }

    fn row(&self, r: usize) -> Option<&[Cell]> {
        let h = self.current.len() as i64;
        let i = r as i64 - self.shown;
        if i < 0 {
            self.above.get((-i - 1) as usize)?.as_deref()
        } else if i >= h {
            self.below.get((i - h) as usize)?.as_deref()
        } else {
            self.current.get(i as usize).map(Vec::as_slice)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::grid::Text;

    /// Lines named by a number, so a test can say which one a row shows.
    fn lines(range: std::ops::Range<u32>) -> Vec<Vec<Cell>> {
        range
            .map(|n| {
                vec![Cell {
                    text: Text::Char(char::from_digit(n % 36, 36).unwrap()),
                    hl: n,
                }]
            })
            .collect()
    }

    fn shows(s: &Scroll) -> Vec<Option<u32>> {
        (0..s.current.len())
            .map(|r| s.row(r).and_then(|l| l.first()).map(|c| c.hl))
            .collect()
    }

    /// Scrolled down three: at first the view is the old one, and it slides
    /// up a row at a time until it is the new one.
    #[test]
    fn the_view_starts_where_it_was_and_slides_to_where_it_is() {
        let mut s = Scroll::new((0, 5, 0, 1), lines(10..15), lines(13..18), 3, 1);
        assert_eq!(shows(&s), [10, 11, 12, 13, 14].map(Some));
        let mut seen = vec![shows(&s)];
        while s.step(0.004, 0.3) {
            if seen.last() != Some(&shows(&s)) {
                seen.push(shows(&s));
            }
        }
        if seen.last() != Some(&shows(&s)) {
            seen.push(shows(&s));
        }
        assert_eq!(
            seen,
            vec![
                [10, 11, 12, 13, 14].map(Some).to_vec(),
                [11, 12, 13, 14, 15].map(Some).to_vec(),
                [12, 13, 14, 15, 16].map(Some).to_vec(),
                [13, 14, 15, 16, 17].map(Some).to_vec(),
            ]
        );
    }

    /// Up as well as down.
    #[test]
    fn scrolling_up_slides_the_other_way() {
        let s = Scroll::new((0, 4, 0, 1), lines(10..14), lines(8..12), -2, 1);
        assert_eq!(shows(&s), [10, 11, 12, 13].map(Some));
    }

    /// A row's scroll is no slower than none: it is shown at once.
    #[test]
    fn a_single_row_is_shown_at_once() {
        let mut s = Scroll::new((0, 4, 0, 1), lines(0..4), lines(1..5), 1, 1);
        s.step(0.001, 0.3);
        assert_eq!(shows(&s), [1, 2, 3, 4].map(Some));
    }

    /// A second scroll before the first has settled carries on from where
    /// the view had got to.
    #[test]
    fn a_scroll_on_top_of_a_scroll_carries_on() {
        let mut s = Scroll::new((0, 4, 0, 1), lines(0..4), lines(2..6), 2, 1);
        s.scrolled(lines(4..8), 2, 1);
        assert_eq!(shows(&s), [0, 1, 2, 3].map(Some));
        // And back again: what the first scroll put above is found below.
        let mut s = Scroll::new((0, 4, 0, 1), lines(0..4), lines(2..6), 2, 1);
        s.scrolled(lines(0..4), -2, 1);
        assert_eq!(s.offset(), 0);
        assert_eq!(shows(&s), [0, 1, 2, 3].map(Some));
    }

    /// Past the whole view, only `far` rows are animated, and drawn blank.
    #[test]
    fn a_jump_past_the_view_animates_only_far_rows() {
        let s = Scroll::new((0, 4, 0, 1), lines(0..4), lines(50..54), 50, 2);
        assert_eq!(s.offset(), 2);
        assert_eq!(shows(&s), vec![None, None, Some(50), Some(51)]);
        let s = Scroll::new((0, 4, 0, 1), lines(0..4), lines(50..54), 50, 0);
        assert_eq!(s.offset(), 0);
    }
}
