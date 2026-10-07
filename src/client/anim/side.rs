//! A window scrolled sideways ahead of Neovim: see [`crate::client::predict`].
//!
//! Neovim scrolls a window that does not wrap sideways by drawing every row
//! of it again from another column, and says nothing more of it to a UI —
//! `win_viewport` has no column for it. So until it has, the client draws
//! the window as it will be: the rows it shows, moved, and what comes into
//! view drawn from the lines' text (see `predict::shifted`). The agent the
//! client leaves in the editor says when Neovim moves a view's first column,
//! just before Neovim draws it moved (see `super::super::App`), and that is
//! what lands a prediction.
//!
//! Nothing slides: a terminal's columns are as coarse as its rows, and
//! nobody animates a scroll sideways.

use std::collections::VecDeque;
use std::time::Instant;

use crate::client::compose::Scrolling;
use crate::client::grid::Cell;

/// One window's rows, as they will be once Neovim has made the scrolls
/// sideways the client has made ahead of it.
#[derive(Debug)]
pub struct Side {
    /// `top..bot` × `left..right`, in its grid.
    rect: (usize, usize, usize, usize),
    rows: Vec<Vec<Cell>>,
    /// The first columns the view has been moved to ahead of Neovim, oldest
    /// first, each with when it is given up on.
    ahead: VecDeque<(usize, Instant)>,
    /// Where the cursor is drawn in its grid, if it is in this window.
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

    /// One more scroll sideways, to first column `left`, given up on
    /// `until` then: the window shows `rows`, the cursor at `cursor`.
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

    /// What the window shows, Neovim having drawn it again.
    pub fn refresh(&mut self, rows: Vec<Vec<Cell>>, cursor: Option<(usize, usize)>) {
        self.rows = rows;
        self.cursor = cursor;
    }

    /// The first column the view is going to, if it is ahead of Neovim.
    pub fn left(&self) -> Option<usize> {
        self.ahead.back().map(|(left, _)| *left)
    }

    /// Neovim's view now starts at column `left`: the scrolls ahead up to
    /// the one that went there are made. One that went nowhere it went is
    /// a scroll the client did not see coming, and the rest go with it. Says
    /// whether any is left.
    pub fn landed(&mut self, left: usize) -> bool {
        match self.ahead.iter().position(|(l, _)| *l == left) {
            Some(i) => {
                self.ahead.drain(..=i);
            }
            None => self.ahead.clear(),
        }
        !self.ahead.is_empty()
    }

    /// When the oldest prediction is given up on.
    pub fn until(&self) -> Option<Instant> {
        self.ahead.front().map(|(_, until)| *until)
    }

    /// Where the cursor is drawn in its grid, if this window has it.
    pub fn cursor(&self) -> Option<(usize, usize)> {
        self.cursor
    }
}

impl Scrolling for Side {
    fn rect(&self) -> (usize, usize, usize, usize) {
        self.rect
    }

    fn row(&self, r: usize) -> Option<&[Cell]> {
        self.rows.get(r).map(Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Neovim's view landing where the client put it lands every scroll up
    /// to that one; landing somewhere else lands none, and ends them all.
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
}
