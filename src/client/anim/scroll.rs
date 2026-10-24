//! Smooth window scrolling, as in Neovide, in whole rows.
//!
//! Neovim scrolls with `grid_scroll` plus redraws and reports the distance as
//! `win_viewport`'s `scroll_delta`; by then the lines that left the view are
//! gone from the grid. So each scrolling window keeps the lines either side of
//! its view, captured from the grid just before each scroll (see [`leaving`]),
//! and is drawn offset by a glide from the delta to zero (see
//! [`super::glide`]). A scroll during a slide takes over what is left of it,
//! so both end together, as late after the last as one alone would.
//!
//! The offset rounds towards its target, so the first row moves at once. For
//! a jump taller than the window the gap is unknown: only `far` rows
//! (Neovide's `scroll_animation_far_lines`) are animated, drawn blank.
//!
//! Without `ext_multigrid`, `grid_scroll` on grid 1 still names the window's
//! rectangle; it is the window's scroll when its rows match the current
//! window's `scroll_delta` (see `super::Animator::before`).

use std::collections::VecDeque;
use std::time::Instant;

use super::glide::Glide;
use crate::client::compose::Scrolling;
use crate::client::grid::{Cell, Grid};
use crate::client::model::Held;

/// An out-of-view line; `None` if unknown.
type Line = Option<Kept>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub cells: Vec<Cell>,
    pub by: Drawer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drawer {
    /// Neovim, with its cursor on this buffer line if known. The cursor line
    /// decides `'relativenumber'` numbers and the `'cursorline'` bar.
    Neovim(Option<i64>),
    /// The client, for a line uncovered by a scroll ahead (see [`Fill`]).
    Client,
}

/// Rows a Neovim scroll took out of view (see [`leaving`]), and its cursor's
/// buffer line when it drew them, if known.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Gone {
    pub rows: Vec<Vec<Cell>>,
    pub cursor: Option<i64>,
}

impl From<Vec<Vec<Cell>>> for Gone {
    fn from(rows: Vec<Vec<Cell>>) -> Self {
        Self { rows, cursor: None }
    }
}

/// Rows `top..bot` (grid coordinates) of `rect` that a scroll of `delta`
/// takes out of view: the top ones scrolling down the buffer, the bottom ones
/// up, all of them past the view height. This is all [`Scroll::scrolled`]
/// keeps of the old view; rows still in view are read from the grid.
pub fn leaving(rect: (usize, usize, usize, usize), delta: i64) -> (usize, usize) {
    let (top, bot, _, _) = rect;
    let n = usize::try_from(delta.unsigned_abs()).map_or(bot - top, |n| n.min(bot - top));
    if delta > 0 {
        (top, top + n)
    } else {
        (bot - n, bot)
    }
}

/// While predicting, how far a view keeps lines either side, in view heights
/// (doubled in [`Scroll::scrolled`]), so a scroll ahead can come back over
/// them. Without prediction only what a slide shows is kept.
const KEEP: i64 = 8;

/// Max distance ahead of Neovim, in view heights; only a bound on growth.
/// A scroll past the lines the client has is held and drawn once they
/// arrive, not refused: refusing would make every later scroll wait a round
/// trip. The agent keeps fewer lines than this (see
/// `crate::client::predict::Predictor::reach`).
pub const HOLD: i64 = 32;

/// Draws a line uncovered by a scroll ahead, indexed from the rectangle's top
/// as Neovim has it (`h` is the first below, `-1` the first above); `None` if
/// not available yet.
pub type Fill<'a> = dyn FnMut(i64) -> Option<Vec<Cell>> + 'a;

fn filled(fill: &mut Fill, i: i64) -> Line {
    fill(i).map(|cells| Kept {
        cells,
        by: Drawer::Client,
    })
}

/// Computes the cursor column after a scroll ahead (see [`Land`]), given the
/// wanted display column from an earlier scroll, if any. Returns
/// `Some(Some((grid col, wanted display col)))`; `Some(None)` if Neovim would
/// scroll the view sideways, which drops the scroll (see [`Scroll::predict`]);
/// or `None` if the line has not been sent yet.
pub type Lander<'a> = dyn FnMut(Land, Option<usize>) -> Option<Option<(usize, usize)>> + 'a;

/// Where a scroll ahead leaves the cursor. The column is computed from the
/// line's text once earlier scrolls are drawn (see [`Scroll::fill_in`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Land {
    /// 0-based buffer line.
    pub line: i64,
    /// First non-blank instead of the wanted column.
    pub first: bool,
    /// Insert mode: the cursor may sit past the end of the line.
    pub insert: bool,
}

/// A scroll the client makes ahead of Neovim (see [`Scroll::predict`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ahead {
    /// Positive is down the buffer, as `scroll_delta` counts.
    pub rows: i64,
    /// Lines the cursor moves (positive is down), before `'scrolloff'` is
    /// applied by [`super::Animator::cursor_at`]. For a cursor step (see
    /// [`super::chain`]), counted from where the previous step left it.
    pub cursor: i64,
    /// New cursor grid column, if it changes, and the wanted display column
    /// from then on (Neovim's `curswant`).
    pub col: Option<(usize, usize)>,
    /// Set while `col` is still to be computed.
    pub land: Option<Land>,
    /// Dropped if Neovim has not made it by then; extended while Neovim is
    /// still processing typed-ahead keys (see [`Scroll::defer`]).
    pub until: Instant,
    pub scrolloff: usize,
    /// Client sequence number of the key or wheel turn that made it, when the
    /// agent reports what Neovim made of each (see
    /// `crate::client::predict::Sync`).
    pub seq: Option<u64>,
}

/// One rectangle scrolling, or scrolled ahead of Neovim by prediction.
#[derive(Debug)]
pub struct Scroll {
    /// `top..bot` × `left..right`, in its grid.
    rect: (usize, usize, usize, usize),
    /// Lines above the rectangle, nearest first; lines inside are read from
    /// the grid.
    above: VecDeque<Line>,
    /// Lines below, nearest first.
    below: VecDeque<Line>,
    /// Rows the view is behind: positive shows lines above the top (it
    /// scrolled down and is catching up), negative lines below the bottom.
    glide: Glide,
    shown: i64,
    /// Off: no slide; scrolls exist only ahead of Neovim and jump.
    animate: bool,
    /// Predicted scrolls Neovim has not made yet, oldest first (see
    /// [`crate::client::predict`]).
    ahead: VecDeque<Ahead>,
    /// How many of `ahead` are drawn. A scroll uncovering a line not yet
    /// received is kept, but it and those after it are drawn only once the
    /// line arrives.
    drawn: usize,
}

impl Scroll {
    /// `gone` holds the rows [`leaving`] names, from before the scroll.
    pub fn new(
        rect: (usize, usize, usize, usize),
        gone: impl Into<Gone>,
        delta: i64,
        far: usize,
    ) -> Self {
        let mut s = Self::still(rect, true);
        // A new view only has the lines it left, never more than a slide
        // shows.
        s.scrolled(gone, delta, far, false);
        s
    }

    /// Not moving yet; the starting point for prediction.
    pub fn still(rect: (usize, usize, usize, usize), animate: bool) -> Self {
        Self {
            rect,
            above: VecDeque::new(),
            below: VecDeque::new(),
            glide: Glide::default(),
            shown: 0,
            animate,
            ahead: VecDeque::new(),
            drawn: 0,
        }
    }

    pub fn rect(&self) -> (usize, usize, usize, usize) {
        self.rect
    }

    fn height(&self) -> i64 {
        let (top, bot, _, _) = self.rect;
        (bot - top) as i64
    }

    /// Mark kept lines' highlights as in use (see
    /// [`crate::client::model::Model::reclaim`]).
    pub fn hold(&self, held: &mut Held) {
        for line in self.above.iter().chain(&self.below).flatten() {
            held.cells(&line.cells);
        }
    }

    /// Rows the view is ahead of Neovim's, drawn or not.
    pub fn ahead(&self) -> i64 {
        self.ahead.iter().map(|a| a.rows).sum()
    }

    /// Lines the cursor is ahead of Neovim's, drawn or not.
    pub fn cursor(&self) -> i64 {
        self.ahead.iter().map(|a| a.cursor).sum()
    }

    /// Cursor (grid column, wanted column) from the latest scroll ahead that
    /// sets one, drawn or not.
    pub fn col(&self) -> Option<(usize, usize)> {
        self.ahead.iter().rev().find_map(|a| a.col)
    }

    fn shows(&self) -> impl DoubleEndedIterator<Item = &Ahead> {
        self.ahead.iter().take(self.drawn)
    }

    /// Like [`Scroll::ahead`], drawn scrolls only.
    pub fn shown_ahead(&self) -> i64 {
        self.shows().map(|a| a.rows).sum()
    }

    /// Like [`Scroll::cursor`], drawn scrolls only.
    pub fn shown_cursor(&self) -> i64 {
        self.shows().map(|a| a.cursor).sum()
    }

    /// Like [`Scroll::col`], drawn scrolls only.
    pub fn shown_col(&self) -> Option<(usize, usize)> {
        self.shows().rev().find_map(|a| a.col)
    }

    /// `'scrolloff'` as of the last drawn prediction.
    pub fn scrolloff(&self) -> usize {
        self.shows().next_back().map_or(0, |a| a.scrolloff)
    }

    /// Whether Neovim has yet to make a scroll ahead from input `k` or
    /// earlier.
    pub fn unmade(&self, k: u64) -> bool {
        self.ahead.iter().any(|a| a.seq.is_some_and(|s| s <= k))
    }

    /// Sequence numbers of the inputs behind the scrolls ahead.
    pub fn seqs(&self) -> Vec<u64> {
        self.ahead.iter().filter_map(|a| a.seq).collect()
    }

    /// Whether some scrolls ahead are not drawn yet.
    pub fn held(&self) -> bool {
        self.drawn < self.ahead.len()
    }

    /// Deadline of the oldest prediction.
    pub fn until(&self) -> Option<Instant> {
        self.ahead.front().map(|a| a.until)
    }

    /// Neovim scrolled `delta` more rows, taking out `gone` (the rows
    /// [`leaving`] names). They are kept as Neovim's, and the offset grows by
    /// `delta` less what was already scrolled ahead, which this confirms.
    ///
    /// Scrolls ahead are confirmed oldest first, as many as `delta` covers
    /// (Neovim may make two in one redraw). A scroll in the other direction
    /// was not predicted, so the view first returns to Neovim's.
    ///
    /// Only when `predicting` (see [`crate::client::predict::Predictor`]) are
    /// more lines kept than a slide shows.
    pub fn scrolled(&mut self, gone: impl Into<Gone>, delta: i64, far: usize, predicting: bool) {
        let gone = gone.into();
        if self
            .ahead
            .front()
            .is_some_and(|a| a.rows.signum() != delta.signum())
        {
            self.give_up();
        }
        let shown = self.shown_ahead();
        // The part of `delta` no scroll ahead predicted.
        let mut left = delta;
        while left != 0 {
            match self.ahead.front_mut() {
                Some(a) if a.rows.signum() == left.signum() => {
                    if a.rows.abs() <= left.abs() {
                        left -= a.rows;
                        self.ahead.pop_front();
                        self.drawn = self.drawn.saturating_sub(1);
                    } else {
                        a.rows -= left;
                        left = 0;
                    }
                }
                _ => break,
            }
        }
        let h = self.height();
        if delta.abs() > h && left == delta {
            // Unpredicted jump past the whole view: the gap is unknown.
            // Animate only `far` rows, starting one further since a glide
            // drops the first row at once.
            self.above.clear();
            self.below.clear();
            self.ahead.clear();
            self.drawn = 0;
            let rows = if self.animate && far > 0 {
                far.min(h as usize) as f32 + 1.0
            } else {
                0.0
            };
            self.glide = Glide::at(if delta > 0 { rows } else { -rows });
            self.shown = self.glide.cells();
            return;
        }
        // All known lines in order (above from the furthest, old view,
        // below), re-split around the new view, however far it moved. Of the
        // old view only the rows that left it are known.
        let above = self.above.len() as i64;
        let (lo, hi) = self.extent();
        let mut old: Vec<Line> = vec![None; h as usize];
        let (from, to) = leaving((0, h as usize, 0, 0), delta);
        let by = Drawer::Neovim(gone.cursor);
        for (slot, cells) in old[from..to].iter_mut().zip(gone.rows) {
            *slot = Some(Kept { cells, by });
        }
        let mut known: Vec<Line> = std::mem::take(&mut self.above)
            .into_iter()
            .rev()
            .chain(old)
            .chain(std::mem::take(&mut self.below))
            .collect();
        let n = known.len() as i64;
        let at = above + delta;
        let mut take = |i: i64| {
            (0..n)
                .contains(&i)
                .then(|| known[i as usize].take())
                .flatten()
        };
        // Predicting: as far as a view is usually ahead, and as much again.
        // Otherwise what a slide shows (the glide is clamped to the height
        // below). Either way, cover the pending scrolls ahead.
        let keep = if predicting { 2 * KEEP * h + 1 } else { h + 1 };
        self.above = (1..=keep.max(-lo))
            .map(|k| at - k)
            .take_while(|i| *i >= 0)
            .map(&mut take)
            .collect();
        self.below = (0..keep.max(hi))
            .map(|k| at + h + k)
            .take_while(|i| *i < n)
            .map(&mut take)
            .collect();
        // Slide by `delta` less what was already drawn ahead; a predicted
        // scroll not yet drawn slides like an unpredicted one.
        let slide = delta - (shown - self.shown_ahead());
        if self.animate && slide != 0 {
            let to = self.glide.position() + slide as f32;
            self.glide.set(to.clamp(-(h as f32), h as f32));
        }
        self.shown = self.glide.cells();
        // Neovim's lines may be what a held scroll ahead was waiting for.
        self.advance(&mut |_, _| None);
    }

    /// Scroll ahead of Neovim by `go`, up to [`HOLD`] view heights. `fill`
    /// draws uncovered lines and `land` places the cursor; if either cannot
    /// yet, the scroll is held until [`Scroll::fill_in`] can draw it.
    ///
    /// Returns false if not made, including when it or an earlier held scroll
    /// lands the cursor where Neovim would scroll the view sideways, which a
    /// vertical scroll cannot draw. That scroll and those after it are
    /// dropped for Neovim to draw.
    pub fn predict(&mut self, go: Ahead, fill: &mut Fill, land: &mut Lander) -> bool {
        let h = self.height();
        let target = self.ahead() + go.rows;
        if go.rows == 0 || target.abs() > HOLD * h {
            return false;
        }
        self.ahead.push_back(go);
        self.reach(fill);
        !self.advance(land)
    }

    /// Fill lines `fill` could not draw before and draw held scrolls as far
    /// as possible, dropping sideways ones as [`Scroll::predict`] does.
    /// Returns whether more scrolls are drawn now.
    pub fn fill_in(&mut self, fill: &mut Fill, land: &mut Lander) -> bool {
        let h = self.height();
        for (k, line) in self.below.iter_mut().enumerate() {
            if line.is_none() {
                *line = filled(fill, h + k as i64);
            }
        }
        for (k, line) in self.above.iter_mut().enumerate() {
            if line.is_none() {
                *line = filled(fill, -1 - k as i64);
            }
        }
        self.reach(fill);
        let drawn = self.drawn;
        self.advance(land);
        self.drawn != drawn
    }

    /// Extend the kept lines, via `fill`, as far as the scrolls ahead go.
    fn reach(&mut self, fill: &mut Fill) {
        let h = self.height();
        let (lo, hi) = self.extent();
        while (self.below.len() as i64) < hi {
            let line = filled(fill, h + self.below.len() as i64);
            self.below.push_back(line);
        }
        while (self.above.len() as i64) < -lo {
            let line = filled(fill, -1 - self.above.len() as i64);
            self.above.push_back(line);
        }
    }

    /// Furthest reach of the scrolls ahead above the top (`<= 0`) and below
    /// it (`>= 0`).
    fn extent(&self) -> (i64, i64) {
        let (mut lo, mut hi, mut at) = (0, 0, 0);
        for a in &self.ahead {
            at += a.rows;
            (lo, hi) = (lo.min(at), hi.max(at));
        }
        (lo, hi)
    }

    fn known(&self, rows: i64) -> bool {
        let side = if rows > 0 { &self.below } else { &self.above };
        let n = rows.unsigned_abs() as usize;
        side.len() >= n && side.iter().take(n).all(Option::is_some)
    }

    /// Draw held scrolls ahead, oldest first, while their lines are known and
    /// `land` can place the cursor (each from the wanted column after the
    /// previous). Returns whether one was dropped, with those after it (see
    /// [`Scroll::predict`]).
    fn advance(&mut self, land: &mut Lander) -> bool {
        let mut at = self.shown_ahead();
        let mut want = self.shown_col().map(|(_, want)| want);
        let mut let_go = false;
        while let Some(a) = self.ahead.get(self.drawn).copied() {
            if !self.known(at + a.rows) {
                break;
            }
            let mut col = a.col;
            if let Some(l) = a.land {
                let Some(landed) = land(l, want) else {
                    break;
                };
                if landed.is_none() {
                    self.ahead.truncate(self.drawn);
                    let_go = true;
                    break;
                }
                col = landed;
                let a = &mut self.ahead[self.drawn];
                (a.col, a.land) = (col, None);
            }
            if let Some((_, w)) = col {
                want = Some(w);
            }
            at += a.rows;
            self.drawn += 1;
            if self.animate {
                // For a sequenced scroll the client draws the cursor line
                // (see `crate::client::App::repaint`); a sub-row remainder of
                // earlier slides would hold back its first row and draw the
                // cursor over the wrong line.
                if a.seq.is_some() && self.glide.cells() == 0 {
                    self.glide = Glide::default();
                }
                self.glide.add(a.rows as f32);
            }
        }
        self.shown = self.glide.cells();
        let_go
    }

    /// Rows shown ahead of Neovim's once the slide ends (in-view rows read
    /// from `grid`); `None` if any is unknown.
    pub fn rows_ahead(&self, grid: &Grid) -> Option<Vec<Vec<Cell>>> {
        let at = self.shown_ahead();
        (0..self.height())
            .map(|r| self.line(r + at, grid).map(<[Cell]>::to_vec))
            .collect()
    }

    /// Jump to the end of the slide.
    pub fn settle(&mut self) {
        self.glide = Glide::default();
        self.shown = 0;
    }

    /// Line `i` rows from the rectangle's top as Neovim has it: from `grid`
    /// if in view, else kept; `None` if unknown.
    fn line<'a>(&'a self, i: i64, grid: &'a Grid) -> Option<&'a [Cell]> {
        if (0..self.height()).contains(&i) {
            let (top, _, left, right) = self.rect;
            let row = grid.row(top + i as usize);
            row.get(left.min(row.len())..right.min(row.len()))
        } else {
            self.kept(i).map(|k| k.cells.as_slice())
        }
    }

    /// Kept line `i` rows from the top; `None` if in view or unknown.
    pub fn kept(&self, i: i64) -> Option<&Kept> {
        let h = self.height();
        if i < 0 {
            self.above.get((-i - 1) as usize)?.as_ref()
        } else if i >= h {
            self.below.get((i - h) as usize)?.as_ref()
        } else {
            None
        }
    }

    /// Drop all scrolls ahead; the view returns (sliding, if animated) to
    /// Neovim's.
    pub fn give_up(&mut self) {
        let shown = self.shown_ahead();
        self.ahead.clear();
        self.drawn = 0;
        if self.animate {
            self.glide.add(-shown as f32);
            self.shown = self.glide.cells();
        }
    }

    pub fn expire(&mut self, now: Instant) {
        if self.until().is_some_and(|until| until <= now) {
            self.give_up();
        }
    }

    /// Keep every scroll ahead until at least `until`: Neovim is still
    /// processing the keys behind them.
    pub fn defer(&mut self, until: Instant) {
        for a in &mut self.ahead {
            a.until = a.until.max(until);
        }
    }

    /// Advance `dt` seconds of a slide lasting `duration`. Returns whether
    /// the scroll is still needed (sliding or ahead of Neovim). The slide ends
    /// once the offset is under a row, as the glide does not overshoot.
    pub fn step(&mut self, dt: f32, duration: f32) -> bool {
        self.glide.step(dt, duration);
        self.shown = self.glide.cells();
        self.shown != 0 || !self.ahead.is_empty()
    }

    pub fn moving(&self) -> bool {
        self.glide.moving()
    }

    /// Whole rows the view is behind this frame.
    pub fn offset(&self) -> i64 {
        self.shown
    }
}

impl Scrolling for Scroll {
    fn rect(&self) -> (usize, usize, usize, usize) {
        self.rect
    }

    fn row<'a>(&'a self, r: usize, grid: &'a Grid) -> Option<&'a [Cell]> {
        self.line(r as i64 - self.shown + self.shown_ahead(), grid)
    }

    fn own_row(&self, r: usize) -> Option<usize> {
        let i = r as i64 - self.shown + self.shown_ahead();
        (0..self.height()).contains(&i).then_some(i as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::grid::Text;
    use crate::client::redraw::LineCell;

    /// Lines labelled by number, so a test can tell which one a row shows.
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

    fn grid(view: &[Vec<Cell>]) -> Grid {
        let mut g = Grid::new(1, view.len());
        for (r, line) in view.iter().enumerate() {
            let runs: Vec<LineCell> = line
                .iter()
                .map(|c| LineCell {
                    text: c.text.clone(),
                    hl: Some(c.hl),
                    repeat: 1,
                })
                .collect();
            g.put(r, 0, &runs);
        }
        g
    }

    fn shows(s: &Scroll, view: &[Vec<Cell>]) -> Vec<Option<u32>> {
        let g = grid(view);
        (0..view.len())
            .map(|r| s.row(r, &g).and_then(|l| l.first()).map(|c| c.hl))
            .collect()
    }

    fn gone(rect: (usize, usize, usize, usize), view: &[Vec<Cell>], delta: i64) -> Vec<Vec<Cell>> {
        let (top, bot) = leaving(rect, delta);
        view[top - rect.0..bot - rect.0].to_vec()
    }

    /// No frame shows the old view: the first row moves at once.
    #[test]
    fn the_view_starts_a_row_on_and_slides_to_where_it_is() {
        let mut s = Scroll::new((0, 5, 0, 1), gone((0, 5, 0, 1), &lines(10..15), 3), 3, 1);
        assert_eq!(shows(&s, &lines(13..18)), [11, 12, 13, 14, 15].map(Some));
        let mut seen = vec![shows(&s, &lines(13..18))];
        while s.step(0.004, 0.3) {
            if seen.last() != Some(&shows(&s, &lines(13..18))) {
                seen.push(shows(&s, &lines(13..18)));
            }
        }
        if seen.last() != Some(&shows(&s, &lines(13..18))) {
            seen.push(shows(&s, &lines(13..18)));
        }
        assert_eq!(
            seen,
            vec![
                [11, 12, 13, 14, 15].map(Some).to_vec(),
                [12, 13, 14, 15, 16].map(Some).to_vec(),
                [13, 14, 15, 16, 17].map(Some).to_vec(),
            ]
        );
    }

    /// Rows read from the grid report their grid row and kept lines report
    /// none, since overflow past a row's end follows grid rows (see
    /// `crate::client::wide`).
    #[test]
    fn a_sliding_view_names_the_grids_rows_it_shows() {
        let view = lines(13..18);
        let g = grid(&view);
        let s = Scroll::new((0, 5, 0, 1), gone((0, 5, 0, 1), &lines(10..15), 3), 3, 1);
        assert_eq!(shows(&s, &view), [11, 12, 13, 14, 15].map(Some));
        let own: Vec<Option<usize>> = (0..5).map(|r| s.own_row(r)).collect();
        assert_eq!(own, [None, None, Some(0), Some(1), Some(2)]);
        for r in 0..5 {
            if let Some(i) = s.own_row(r) {
                assert_eq!(s.row(r, &g), Some(g.row(i)), "row {r}");
            }
        }
    }

    #[test]
    fn a_scroll_takes_out_of_view_the_rows_it_moves_past() {
        let rect = (2, 7, 0, 1);
        assert_eq!(leaving(rect, 3), (2, 5), "down the buffer");
        assert_eq!(leaving(rect, -2), (5, 7), "up it");
        assert_eq!(leaving(rect, 9), (2, 7), "past the view");
        assert_eq!(leaving(rect, -9), (2, 7));
    }

    #[test]
    fn only_a_view_that_predicts_keeps_more_than_a_slide_shows() {
        let rect = (0, 5, 0, 1);
        for (predicting, kept) in [(false, 6), (true, 50)] {
            let mut s = Scroll::still(rect, true);
            for k in 0..10 {
                s.scrolled(gone(rect, &lines(k * 5..k * 5 + 5), 5), 5, 1, predicting);
            }
            assert_eq!(s.above.len(), kept, "predicting: {predicting}");
            let behind = s.offset() as u32;
            assert!(
                (1..=5).contains(&behind),
                "sliding, no more than its height"
            );
            assert_eq!(
                shows(&s, &lines(50..55)),
                (50 - behind..55 - behind).map(Some).collect::<Vec<_>>(),
                "predicting: {predicting}"
            );
        }
    }

    #[test]
    fn scrolling_up_slides_the_other_way() {
        let s = Scroll::new((0, 4, 0, 1), gone((0, 4, 0, 1), &lines(10..14), -2), -2, 1);
        assert_eq!(shows(&s, &lines(8..12)), [9, 10, 11, 12].map(Some));
    }

    #[test]
    fn a_single_row_is_shown_at_once() {
        let mut s = Scroll::new((0, 4, 0, 1), gone((0, 4, 0, 1), &lines(0..4), 1), 1, 1);
        s.step(0.001, 0.3);
        assert_eq!(shows(&s, &lines(1..5)), [1, 2, 3, 4].map(Some));
    }

    /// A second scroll before the first settles continues from where the view
    /// had got to.
    #[test]
    fn a_scroll_on_top_of_a_scroll_carries_on() {
        let mut s = Scroll::new((0, 4, 0, 1), gone((0, 4, 0, 1), &lines(0..4), 2), 2, 1);
        assert_eq!(shows(&s, &lines(2..6)), [1, 2, 3, 4].map(Some));
        s.scrolled(gone((0, 4, 0, 1), &lines(2..6), 2), 2, 1, false);
        assert_eq!(shows(&s, &lines(4..8)), [1, 2, 3, 4].map(Some));
        // Back again: lines the first scroll put above are found below.
        let mut s = Scroll::new((0, 4, 0, 1), gone((0, 4, 0, 1), &lines(0..4), 2), 2, 1);
        s.scrolled(gone((0, 4, 0, 1), &lines(2..6), -2), -2, 1, false);
        assert_eq!(s.offset(), 0);
        assert_eq!(shows(&s, &lines(0..4)), [0, 1, 2, 3].map(Some));
    }

    /// A `Fill` whose rectangle top shows line `top`; nothing past `last`.
    fn fill(top: u32, last: u32) -> impl FnMut(i64) -> Option<Vec<Cell>> {
        move |i| {
            let n = u32::try_from(i64::from(top) + i).ok()?;
            (n <= last).then(|| lines(n..n + 1).remove(0))
        }
    }

    fn go(rows: i64) -> Ahead {
        Ahead {
            rows,
            cursor: 0,
            col: None,
            land: None,
            until: Instant::now() + std::time::Duration::from_secs(60),
            scrolloff: 0,
            seq: None,
        }
    }

    #[test]
    fn a_prediction_shows_at_once_and_neovim_lands_on_it() {
        let mut s = Scroll::still((0, 5, 0, 1), false);
        assert!(s.predict(go(2), &mut fill(10, 99), &mut |_, _| None));
        assert_eq!(shows(&s, &lines(10..15)), [12, 13, 14, 15, 16].map(Some));
        s.scrolled(gone((0, 5, 0, 1), &lines(10..15), 2), 2, 1, true);
        assert_eq!(s.ahead(), 0);
        assert_eq!(shows(&s, &lines(12..17)), [12, 13, 14, 15, 16].map(Some));
        assert!(!s.step(0.01, 0.0), "nothing left to do");
        let mut s = Scroll::still((0, 5, 0, 1), false);
        assert!(s.predict(go(-3), &mut fill(10, 99), &mut |_, _| None));
        assert_eq!(shows(&s, &lines(10..15)), [7, 8, 9, 10, 11].map(Some));
    }

    /// A one-row scroll ahead during a settling slide shows at once if
    /// sequenced (the sub-row remainder is dropped), else is held back by it.
    #[test]
    fn a_row_scrolled_ahead_on_a_slide_settling_shows_at_once_where_judged() {
        for (seq, held) in [(Some(1), 0), (None, 1)] {
            let mut s = Scroll::still((0, 5, 0, 1), true);
            let go = |rows| Ahead { seq, ..go(rows) };
            assert!(s.predict(go(1), &mut fill(10, 99), &mut |_, _| None));
            assert_eq!(s.offset(), 0, "{seq:?}: at once");
            s.step(0.03, 0.1);
            assert!(s.moving());
            assert!(s.predict(go(1), &mut fill(10, 99), &mut |_, _| None));
            assert_eq!(s.offset(), held, "{seq:?}");
        }
    }

    /// Two wheel turns in one redraw are both confirmed; a third stays ahead.
    #[test]
    fn neovim_confirms_the_oldest_first() {
        let mut s = Scroll::still((0, 8, 0, 1), false);
        for _ in 0..3 {
            assert!(s.predict(go(2), &mut fill(0, 99), &mut |_, _| None));
        }
        assert_eq!(shows(&s, &lines(0..8))[0], Some(6));
        s.scrolled(gone((0, 8, 0, 1), &lines(0..8), 4), 4, 1, true);
        assert_eq!(s.ahead(), 2);
        assert_eq!(
            shows(&s, &lines(4..12))[0],
            Some(6),
            "still where the client put it"
        );
        s.scrolled(gone((0, 8, 0, 1), &lines(4..12), 2), 2, 1, true);
        assert_eq!(s.ahead(), 0);
        assert_eq!(shows(&s, &lines(6..14))[0], Some(6));
    }

    #[test]
    fn down_and_back_up_never_echoes() {
        let mut s = Scroll::still((0, 5, 0, 1), false);
        s.predict(go(3), &mut fill(10, 99), &mut |_, _| None);
        s.predict(go(-3), &mut fill(10, 99), &mut |_, _| None);
        assert_eq!(shows(&s, &lines(10..15)), [10, 11, 12, 13, 14].map(Some));
        s.scrolled(gone((0, 5, 0, 1), &lines(10..15), 3), 3, 1, true);
        assert_eq!(shows(&s, &lines(13..18)), [10, 11, 12, 13, 14].map(Some));
        s.scrolled(gone((0, 5, 0, 1), &lines(13..18), -3), -3, 1, true);
        assert_eq!(shows(&s, &lines(10..15)), [10, 11, 12, 13, 14].map(Some));
        assert_eq!(s.ahead(), 0);
    }

    #[test]
    fn a_scroll_the_other_way_takes_the_predictions_with_it() {
        let mut s = Scroll::still((0, 5, 0, 1), false);
        s.predict(go(3), &mut fill(10, 99), &mut |_, _| None);
        s.scrolled(gone((0, 5, 0, 1), &lines(10..15), -2), -2, 1, true);
        assert_eq!(s.ahead(), 0);
        assert_eq!(shows(&s, &lines(8..13)), [8, 9, 10, 11, 12].map(Some));
    }

    #[test]
    fn an_unconfirmed_prediction_slides_back() {
        let t0 = Instant::now();
        let until = t0 + std::time::Duration::from_millis(500);
        let mut s = Scroll::still((0, 5, 0, 1), true);
        let go = Ahead { until, ..go(2) };
        s.predict(go, &mut fill(10, 99), &mut |_, _| None);
        assert_eq!(
            shows(&s, &lines(10..15)),
            [11, 12, 13, 14, 15].map(Some),
            "a row on from where it was"
        );
        while s.step(0.01, 0.3) && s.moving() {}
        assert_eq!(
            shows(&s, &lines(10..15)),
            [12, 13, 14, 15, 16].map(Some),
            "to the prediction"
        );
        s.expire(until - std::time::Duration::from_millis(1));
        assert_eq!(s.ahead(), 2, "not yet");
        s.expire(until);
        assert_eq!(s.ahead(), 0);
        assert_eq!(
            shows(&s, &lines(10..15)),
            [11, 12, 13, 14, 15].map(Some),
            "back from there, a row at once"
        );
        while s.step(0.01, 0.3) {}
        assert_eq!(shows(&s, &lines(10..15)), [10, 11, 12, 13, 14].map(Some));
    }

    /// `defer` only extends deadlines, and then all expire together.
    #[test]
    fn a_prediction_waits_while_neovim_is_at_work_on_its_key() {
        let t0 = Instant::now();
        let ms = std::time::Duration::from_millis;
        let mut s = Scroll::still((0, 5, 0, 1), false);
        for until in [t0 + ms(500), t0 + ms(510)] {
            let go = Ahead { until, ..go(2) };
            assert!(s.predict(go, &mut fill(10, 99), &mut |_, _| None));
        }
        s.defer(t0 + ms(900));
        s.expire(t0 + ms(500));
        assert_eq!(s.ahead(), 4, "still at work");
        s.defer(t0 + ms(100));
        s.expire(t0 + ms(899));
        assert_eq!(s.ahead(), 4, "not brought sooner");
        s.expire(t0 + ms(900));
        assert_eq!(s.ahead(), 0, "given up on");
        assert_eq!(shows(&s, &lines(10..15)), [10, 11, 12, 13, 14].map(Some));
    }

    /// Bounded by [`HOLD`]; a scroll whose lines are missing is made but
    /// drawn, with those after it, only once they arrive.
    #[test]
    fn a_prediction_waits_for_its_lines() {
        let mut s = Scroll::still((0, 5, 0, 1), false);
        let far = 5 * HOLD;
        assert!(!s.predict(go(far + 1), &mut fill(10, 999), &mut |_, _| None));
        assert!(s.predict(go(far), &mut fill(10, 999), &mut |_, _| None));
        assert_eq!(shows(&s, &lines(10..15))[0], Some(10 + far as u32));

        // Past the lines received: held, not refused; later scrolls continue
        // from it.
        let mut s = Scroll::still((0, 5, 0, 1), false);
        let reach = 5 * KEEP;
        assert!(
            s.predict(go(reach), &mut fill(10, 14 + reach as u32), &mut |_, _| {
                None
            })
        );
        assert!(s.predict(go(5), &mut fill(10, 14 + reach as u32), &mut |_, _| None));
        assert!(s.predict(go(5), &mut fill(10, 14 + reach as u32), &mut |_, _| None));
        assert_eq!(s.ahead(), reach + 10);
        assert_eq!(
            shows(&s, &lines(10..15))[0],
            Some(10 + reach as u32),
            "as far as its lines"
        );
        assert!(s.fill_in(&mut fill(10, 19 + reach as u32), &mut |_, _| None));
        assert_eq!(
            shows(&s, &lines(10..15))[0],
            Some(15 + reach as u32),
            "one more, as they come"
        );
        assert!(s.fill_in(&mut fill(10, 999), &mut |_, _| None));
        assert_eq!(shows(&s, &lines(10..15))[0], Some(20 + reach as u32));

        let mut s = Scroll::still((0, 5, 0, 1), false);
        assert!(s.predict(go(2), &mut fill(10, 18), &mut |_, _| None));
        assert!(s.predict(go(3), &mut fill(10, 18), &mut |_, _| None));
        assert!(s.predict(go(-1), &mut fill(10, 18), &mut |_, _| None));
        assert_eq!(s.ahead(), 4, "all three made");
        assert!(s.held());
        assert_eq!(
            shows(&s, &lines(10..15)),
            [12, 13, 14, 15, 16].map(Some),
            "the first drawn"
        );
        assert!(!s.fill_in(&mut fill(10, 18), &mut |_, _| None));
        assert!(s.fill_in(&mut fill(10, 99), &mut |_, _| None));
        assert!(!s.held());
        assert_eq!(shows(&s, &lines(10..15)), [14, 15, 16, 17, 18].map(Some));

        // Neovim's scroll may come first: it confirms the held scroll, and
        // the next is drawn from its lines.
        let mut s = Scroll::still((0, 5, 0, 1), false);
        assert!(s.predict(go(5), &mut fill(10, 16), &mut |_, _| None));
        assert!(s.predict(go(1), &mut fill(10, 16), &mut |_, _| None));
        assert_eq!(shows(&s, &lines(10..15)), [10, 11, 12, 13, 14].map(Some));
        s.scrolled(gone((0, 5, 0, 1), &lines(10..15), 5), 5, 1, true);
        assert_eq!(s.ahead(), 1);
        assert!(s.held(), "line 20 is still to come");
        assert!(s.fill_in(&mut fill(15, 20), &mut |_, _| None));
        assert_eq!(shows(&s, &lines(15..20)), [16, 17, 18, 19, 20].map(Some));
    }

    /// The landing is computed when drawn, from the wanted column after the
    /// previous scroll.
    #[test]
    fn the_cursor_lands_as_its_scroll_is_drawn() {
        let mut s = Scroll::still((0, 5, 0, 1), false);
        let mut seen = Vec::new();
        let at = |line| Land {
            line,
            first: false,
            insert: false,
        };
        let mut land = |l: Land, want: Option<usize>| {
            seen.push((l.line, want));
            (l.line != 22).then_some(Some((l.line as usize % 5, l.line as usize)))
        };
        let landing = |line| Ahead {
            land: Some(at(line)),
            ..go(5)
        };
        assert!(s.predict(landing(15), &mut fill(10, 99), &mut land));
        assert_eq!(s.shown_col(), Some((0, 15)));
        assert!(s.predict(landing(22), &mut fill(10, 99), &mut land));
        assert!(s.held(), "its line is yet to come");
        assert!(s.predict(landing(25), &mut fill(10, 99), &mut land));
        assert_eq!(s.shown_ahead(), 5);
        let mut land = |l: Land, want: Option<usize>| {
            seen.push((l.line, want));
            Some(Some((l.line as usize % 5 + 1, l.line as usize)))
        };
        assert!(s.fill_in(&mut fill(10, 99), &mut land));
        assert_eq!(s.shown_ahead(), 15);
        assert_eq!(s.shown_col(), Some((1, 25)));
        assert_eq!(
            seen,
            [
                (15, None),
                (22, Some(15)),
                (22, Some(15)),
                (22, Some(15)),
                (25, Some(22))
            ],
            "asked again till it can say, each from the one before"
        );
    }

    /// Dropped with the held scrolls after it; earlier ones stay drawn.
    #[test]
    fn a_scroll_that_moves_the_view_sideways_is_let_go() {
        let mut s = Scroll::still((0, 5, 0, 1), false);
        let landing = |line| Ahead {
            land: Some(Land {
                line,
                first: false,
                insert: false,
            }),
            ..go(5)
        };
        // Line 20 lands out of view.
        let mut land = |l: Land, _| Some((l.line != 20).then_some((0, 0)));
        assert!(s.predict(landing(15), &mut fill(10, 99), &mut land));
        assert!(!s.predict(landing(20), &mut fill(10, 99), &mut land));
        assert_eq!((s.ahead(), s.shown_ahead()), (5, 5));
        assert_eq!(shows(&s, &lines(10..15)), [15, 16, 17, 18, 19].map(Some));
        // Held for its lines with one after it: both are dropped when they
        // arrive.
        let mut s = Scroll::still((0, 5, 0, 1), false);
        assert!(s.predict(landing(15), &mut fill(10, 99), &mut land));
        assert!(s.predict(landing(20), &mut fill(10, 19), &mut land));
        assert!(s.predict(landing(25), &mut fill(10, 19), &mut land));
        assert_eq!((s.ahead(), s.shown_ahead()), (15, 5));
        assert!(!s.fill_in(&mut fill(10, 99), &mut land));
        assert_eq!((s.ahead(), s.shown_ahead()), (5, 5));
        assert!(!s.held());
    }

    #[test]
    fn the_rows_ahead_are_where_the_view_is_going() {
        let mut s = Scroll::still((0, 5, 0, 1), true);
        assert!(s.predict(go(3), &mut fill(10, 99), &mut |_, _| None));
        assert_ne!(
            shows(&s, &lines(10..15)),
            [13, 14, 15, 16, 17].map(Some),
            "sliding"
        );
        let ahead: Vec<_> = s
            .rows_ahead(&grid(&lines(10..15)))
            .expect("all known")
            .iter()
            .map(|r| r[0].hl)
            .collect();
        assert_eq!(ahead, [13, 14, 15, 16, 17]);
        s.settle();
        assert!(!s.moving());
        assert_eq!(shows(&s, &lines(10..15)), [13, 14, 15, 16, 17].map(Some));
    }

    /// What the view passed is kept, so it can scroll back over it.
    #[test]
    fn a_jump_past_the_view_made_ahead_lands_as_well() {
        let mut s = Scroll::still((0, 5, 0, 1), false);
        assert!(s.predict(go(8), &mut fill(10, 99), &mut |_, _| None));
        assert_eq!(shows(&s, &lines(10..15)), [18, 19, 20, 21, 22].map(Some));
        s.scrolled(gone((0, 5, 0, 1), &lines(10..15), 8), 8, 1, true);
        assert_eq!(s.ahead(), 0);
        assert_eq!(shows(&s, &lines(18..23)), [18, 19, 20, 21, 22].map(Some));
        assert!(s.predict(go(-8), &mut fill(18, 99), &mut |_, _| None));
        assert_eq!(
            shows(&s, &lines(18..23)),
            [10, 11, 12, 13, 14].map(Some),
            "back over what it passed"
        );
    }

    #[test]
    fn scrolls_on_a_slide_land_a_duration_after_the_last() {
        let mut s = Scroll::still((0, 5, 0, 1), true);
        let ms = |s: &mut Scroll, n: usize| {
            for _ in 0..n {
                s.step(0.001, 0.15);
            }
        };
        for _ in 0..8 {
            assert!(s.predict(go(5), &mut fill(0, 999), &mut |_, _| None));
            ms(&mut s, 50);
            assert!(s.moving(), "still sliding when the next comes");
        }
        ms(&mut s, 99);
        assert!(s.moving());
        ms(&mut s, 2);
        assert!(!s.moving(), "150 ms after the last");
        assert_eq!(shows(&s, &lines(0..5)), [40, 41, 42, 43, 44].map(Some));
    }

    #[test]
    fn a_jump_past_the_view_animates_only_far_rows() {
        let s = Scroll::new((0, 4, 0, 1), gone((0, 4, 0, 1), &lines(0..4), 50), 50, 2);
        assert_eq!(s.offset(), 2);
        assert_eq!(
            shows(&s, &lines(50..54)),
            vec![None, None, Some(50), Some(51)]
        );
        let s = Scroll::new((0, 4, 0, 1), gone((0, 4, 0, 1), &lines(0..4), 50), 50, 0);
        assert_eq!(s.offset(), 0);
    }
}
