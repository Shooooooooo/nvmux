//! A window scrolled sideways ahead of Neovim (see [`crate::client::predict`]).
//!
//! Neovim redraws every row of a nowrap window on a sideways scroll and
//! `win_viewport` reports no column, so the client draws the predicted view
//! (shifted rows plus text from the lines, see `predict::shifted`) until the
//! editor-side agent reports the new first column. The same applies to a
//! vertical scroll that will make Neovim scroll sideways to show the cursor
//! (see `predict::showing`). Nothing is animated.

use std::collections::VecDeque;
use std::time::Instant;

use crate::client::compose::Scrolling;
use crate::client::grid::{Cell, Grid};
use crate::client::model::Held;

/// One window's rows as they will be once Neovim makes the predicted
/// sideways scrolls.
#[derive(Debug)]
pub struct Side {
    /// `top..bot` × `left..right`, in its grid.
    rect: (usize, usize, usize, usize),
    rows: Vec<Vec<Cell>>,
    /// Predicted first columns, oldest first, each with its give-up time.
    ahead: VecDeque<(usize, Instant)>,
    /// Cursor position in its grid, if it is in this window.
    cursor: Option<(usize, usize)>,
}

impl Side {
    pub fn new(rect: (usize, usize, usize, usize)) -> Self {
        Self {
            rect,
            rows: Vec::new(),
            ahead: VecDeque::new(),
            cursor: None,
        }
    }

    pub fn rect(&self) -> (usize, usize, usize, usize) {
        self.rect
    }

    /// Keeps the rows' highlights alive; see
    /// [`crate::client::model::Model::reclaim`].
    pub fn hold(&self, held: &mut Held) {
        for row in &self.rows {
            held.cells(row);
        }
    }

    /// Add a predicted scroll to first column `left`, abandoned at `until`.
    pub fn push(
        &mut self,
        rows: Vec<Vec<Cell>>,
        left: usize,
        until: Instant,
        cursor: Option<(usize, usize)>,
    ) {
        self.ahead.push_back((left, until));
        self.refresh(rows, cursor);
    }

    /// Replace the rows after Neovim redraws the window.
    pub fn refresh(&mut self, rows: Vec<Vec<Cell>>, cursor: Option<(usize, usize)>) {
        self.rows = rows;
        self.cursor = cursor;
    }

    /// The latest predicted first column, if any.
    pub fn left(&self) -> Option<usize> {
        self.ahead.back().map(|(left, _)| *left)
    }

    /// Neovim's view now starts at `left`: drop predictions up to the one
    /// for `left`, or all of them if none matches (an unpredicted scroll).
    /// Returns whether any remain.
    pub fn landed(&mut self, left: usize) -> bool {
        match self.ahead.iter().position(|(l, _)| *l == left) {
            Some(i) => {
                self.ahead.drain(..=i);
            }
            None => self.ahead.clear(),
        }
        !self.ahead.is_empty()
    }

    /// When the oldest prediction is abandoned.
    pub fn until(&self) -> Option<Instant> {
        self.ahead.front().map(|(_, until)| *until)
    }

    /// Keep every prediction until at least `until`: Neovim is still busy
    /// with the keys behind them.
    pub fn defer(&mut self, until: Instant) {
        for (_, at) in &mut self.ahead {
            *at = (*at).max(until);
        }
    }

    pub fn cursor(&self) -> Option<(usize, usize)> {
        self.cursor
    }
}

impl Scrolling for Side {
    fn rect(&self) -> (usize, usize, usize, usize) {
        self.rect
    }

    fn row<'a>(&'a self, r: usize, _grid: &'a Grid) -> Option<&'a [Cell]> {
        self.rows.get(r).map(Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn neovim_lands_the_scrolls_up_to_its_own() {
        let t = Instant::now() + Duration::from_secs(60);
        let mut s = Side::new((0, 2, 0, 4));
        s.push(Vec::new(), 1, t, None);
        s.push(Vec::new(), 7, t, None);
        assert_eq!(s.left(), Some(7));
        assert!(s.landed(1), "the second is still ahead");
        assert!(!s.landed(7));
        s.push(Vec::new(), 3, t, None);
        assert!(!s.landed(5), "not one the client made");
        assert_eq!(s.left(), None);
    }

    /// `defer` only ever extends deadlines.
    #[test]
    fn a_scroll_sideways_waits_while_neovim_is_at_work_on_its_key() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut s = Side::new((0, 2, 0, 4));
        s.push(Vec::new(), 1, t0 + ms(500), None);
        s.push(Vec::new(), 7, t0 + ms(510), None);
        s.defer(t0 + ms(900));
        assert_eq!(s.until(), Some(t0 + ms(900)));
        s.defer(t0 + ms(100));
        assert_eq!(s.until(), Some(t0 + ms(900)), "not brought sooner");
        assert!(s.landed(1));
        assert_eq!(s.until(), Some(t0 + ms(900)), "the next as well");
    }
}
