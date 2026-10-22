//! A window scrolling: Neovide's smooth scroll, a row at a time.
//!
//! Neovim scrolls a window by sending its new lines — moving the ones it can
//! (`grid_scroll`), redrawing the rest — and saying in `win_viewport` how far
//! the view moved (`scroll_delta`). By then the lines the view left are gone
//! from the grid. So the client keeps, for each window scrolling, the lines
//! either side of what it shows — the rows each scroll took out of view,
//! taken from the grid just before it (see [`leaving`]) — and draws the window
//! from those and the grid's own, offset by a glide that starts at the delta
//! and ends at nought (see [`super::glide`]). What the eye sees is
//! the text sliding through the window until it is where Neovim put it. A
//! scroll made while one is still sliding takes what that one had left with
//! it: they end together, as long after the last as one alone would.
//!
//! The offset is whole rows — a terminal cannot draw text between them — and
//! it rounds towards where it is going, so the first row of a scroll moves at
//! once and a scroll of one row is no slower than none: there is nothing to
//! animate in one step, and a delay would be all there was to see.
//!
//! A jump further than the window is tall leaves a gap between the view that
//! was and the one that is, which nothing is known about. Only a row of such
//! a jump is animated, as Neovide's `scroll_animation_far_lines` has it by
//! default, and it is drawn blank.
//!
//! Without `ext_multigrid` there are no windows to speak of, only grid 1 —
//! but `grid_scroll` there names the window's rectangle, and the scroll is
//! the window's when its rows match the current window's `scroll_delta` (see
//! `super::Animator::before`). It is the same animation over that rectangle.

use std::collections::VecDeque;
use std::time::Instant;

use super::glide::Glide;
use crate::client::compose::Scrolling;
use crate::client::grid::{Cell, Grid};
use crate::client::model::Held;

/// A line of a scroll's rectangle out of view, or `None` for one nothing is
/// known of.
type Line = Option<Kept>;

/// A line kept either side of a view (see [`Scroll`]): its cells, and who
/// drew them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub cells: Vec<Cell>,
    pub by: Drawer,
}

/// Who drew a line kept either side of a view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drawer {
    /// Neovim, the cursor on that buffer line as it did, where the client
    /// knows which: what the cursor's line decides of a row — the numbers
    /// of `'relativenumber'`, the bar of `'cursorline'` — is as it had it
    /// there.
    Neovim(Option<i64>),
    /// The client, a line a scroll ahead of Neovim brought into view (see
    /// [`Fill`]).
    Client,
}

/// The rows a scroll of Neovim's took out of view, as they were (see
/// [`leaving`]), and the buffer line its cursor was on as it drew them,
/// where the client knows it.
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

/// The rows of `rect` a scroll of `delta` takes out of view, `top..bot` in
/// its grid: the top ones scrolling down the buffer, the bottom ones up,
/// every one past the view's height. What [`Scroll::scrolled`] is given of
/// the view as it was, which is all it keeps of it: the rows still in view
/// are the grid's own, and read from it as they are drawn.
pub fn leaving(rect: (usize, usize, usize, usize), delta: i64) -> (usize, usize) {
    let (top, bot, _, _) = rect;
    let n = usize::try_from(delta.unsigned_abs()).map_or(bot - top, |n| n.min(bot - top));
    if delta > 0 {
        (top, top + n)
    } else {
        (bot - n, bot)
    }
}

/// How many times as far as it is tall a view keeps the lines either side
/// of it, once Neovim has scrolled it — those Neovim drew, and those drawn
/// from the agent's — for a view scrolled ahead of Neovim coming back over
/// them, while the client predicts; and further, the lines the scrolls still
/// ahead of Neovim's show. While it does not predict, a view keeps only as
/// many as a slide shows: see [`Scroll::scrolled`].
const KEEP: i64 = 8;

/// How many times as far as it is tall a view may be scrolled ahead of
/// Neovim at all. Past the lines the client has, a scroll is made but held —
/// the next made from where it goes, Neovim's view moving on meanwhile, and
/// the agent's lines with it — and drawn once its lines have come, rather
/// than refused, which would leave every scroll after it waiting a round
/// trip for Neovim until the keys stopped. This only keeps a client that
/// somehow outran Neovim for good from growing without end. The agent keeps
/// lines less far either side than this (see
/// `crate::client::predict::Predictor::reach`).
pub const HOLD: i64 = 32;

/// What draws a line a view scrolled ahead uncovers, counted from the top
/// of the rectangle as Neovim has it: `h` is the first line below it, `-1`
/// the first above; `None` for one it cannot yet.
pub type Fill<'a> = dyn FnMut(i64) -> Option<Vec<Cell>> + 'a;

/// The line `i` rows down from a rectangle's top that `fill` draws, kept as
/// the client's.
fn filled(fill: &mut Fill, i: i64) -> Line {
    fill(i).map(|cells| Kept {
        cells,
        by: Drawer::Client,
    })
}

/// What works out the cursor's column where a scroll ahead lands it (see
/// [`Land`]), given the display column it wants, if a scroll before says:
/// the grid column and the display column it wants from then on — `None`
/// where Neovim moves the view sideways to show it there, which lets the
/// scroll go (see [`Scroll::predict`]) — or nothing yet, for a line not yet
/// sent.
pub type Lander<'a> = dyn FnMut(Land, Option<usize>) -> Option<Option<(usize, usize)>> + 'a;

/// Where a scroll ahead leaves the cursor, its column to be worked out
/// from its line's text, once the scrolls before it are drawn: see
/// [`Scroll::fill_in`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Land {
    /// The buffer line, counted from nought.
    pub line: i64,
    /// Its first non-blank, not the column the cursor wants.
    pub first: bool,
    /// In Insert mode, where the cursor may sit past the end of the line.
    pub insert: bool,
}

/// A scroll the client makes ahead of Neovim: see [`Scroll::predict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ahead {
    /// How far: above nought down the buffer, as `scroll_delta` counts.
    pub rows: i64,
    /// How many lines it moves the cursor, down the buffer above nought —
    /// before `'scrolloff'` has its say, which [`super::Animator::cursor_at`]
    /// lets it have. For one a step of the cursor's made (see
    /// [`super::chain`]), from where the step before it left the cursor:
    /// where every step scrolled, from Neovim's cursor, as for a scroll
    /// alone.
    pub cursor: i64,
    /// The grid column it leaves the cursor in, if not the one it was in;
    /// and the display column the cursor wants to be in from then on, as
    /// Neovim's `curswant` keeps it.
    pub col: Option<(usize, usize)>,
    /// Where `col` is yet to be worked out: see [`Land`].
    pub land: Option<Land>,
    /// When it is given up on, if Neovim has not made it by then — put off
    /// while Neovim is still at work on the keys typed ahead of it (see
    /// [`Scroll::defer`]).
    pub until: Instant,
    /// `'scrolloff'`: how near the window's edges Neovim lets the cursor be.
    pub scrolloff: usize,
    /// The client's number for the key or the turn of the wheel that made
    /// it, where the agent says what Neovim made of each (see
    /// `crate::client::predict::Sync`).
    pub seq: Option<u64>,
}

/// One rectangle scrolling — or scrolled ahead of Neovim, on the client's
/// own say-so.
#[derive(Debug)]
pub struct Scroll {
    /// `top..bot` × `left..right`, in its grid.
    rect: (usize, usize, usize, usize),
    /// The lines above the rectangle, nearest first. Its own are the grid's:
    /// see [`Scrolling::row`].
    above: VecDeque<Line>,
    /// The lines below it, nearest first.
    below: VecDeque<Line>,
    /// How many rows the view is behind: above nought it shows lines above
    /// the rectangle's top — it has scrolled down the buffer and is catching
    /// up — below nought lines below its bottom.
    glide: Glide,
    shown: i64,
    /// Whether it slides: off, a scroll is only ever ahead of Neovim, and
    /// jumps to wherever it is.
    animate: bool,
    /// Scrolls the client has made ahead of Neovim and Neovim has not yet
    /// made, oldest first. What their rows add up to is how far the view is
    /// ahead of Neovim's. See [`crate::client::predict`].
    ahead: VecDeque<Ahead>,
    /// How many of those the view shows, oldest first. A scroll that
    /// uncovers a line the client has yet to be sent is made all the same —
    /// the next is made from where it goes — but drawn only once the line
    /// comes, and those after it with it.
    drawn: usize,
}

impl Scroll {
    /// A scroll of `delta` rows over `rect`, which took `gone` out of view:
    /// the rows [`leaving`] names, as they were (see [`Gone`]).
    pub fn new(
        rect: (usize, usize, usize, usize),
        gone: impl Into<Gone>,
        delta: i64,
        far: usize,
    ) -> Self {
        let mut s = Self::still(rect, true);
        // A view just set scrolling has no more to keep than the lines it
        // left, which are never more than a slide shows.
        s.scrolled(gone, delta, far, false);
        s
    }

    /// Nothing moving yet over `rect`: what a prediction starts from.
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

    /// How many rows the rectangle has.
    fn height(&self) -> i64 {
        let (top, bot, _, _) = self.rect;
        (bot - top) as i64
    }

    /// Every line it keeps holds its highlights: see
    /// [`crate::client::model::Model::reclaim`].
    pub fn hold(&self, held: &mut Held) {
        for line in self.above.iter().chain(&self.below).flatten() {
            held.cells(&line.cells);
        }
    }

    /// How many rows the view is ahead of Neovim's, drawn or not.
    pub fn ahead(&self) -> i64 {
        self.ahead.iter().map(|a| a.rows).sum()
    }

    /// How many lines the cursor is ahead of Neovim's, drawn or not.
    pub fn cursor(&self) -> i64 {
        self.ahead.iter().map(|a| a.cursor).sum()
    }

    /// The column the cursor has been put in ahead of Neovim, and the one it
    /// wants: the last scroll's to say so, drawn or not.
    pub fn col(&self) -> Option<(usize, usize)> {
        self.ahead.iter().rev().find_map(|a| a.col)
    }

    /// The scrolls ahead the view shows.
    fn shows(&self) -> impl DoubleEndedIterator<Item = &Ahead> {
        self.ahead.iter().take(self.drawn)
    }

    /// How many rows the view is drawn ahead of Neovim's.
    pub fn shown_ahead(&self) -> i64 {
        self.shows().map(|a| a.rows).sum()
    }

    /// How many lines the cursor is drawn ahead of Neovim's.
    pub fn shown_cursor(&self) -> i64 {
        self.shows().map(|a| a.cursor).sum()
    }

    /// The column the cursor is drawn in ahead of Neovim, and the one it
    /// wants.
    pub fn shown_col(&self) -> Option<(usize, usize)> {
        self.shows().rev().find_map(|a| a.col)
    }

    /// `'scrolloff'`, as the last prediction drawn found it.
    pub fn scrolloff(&self) -> usize {
        self.shows().next_back().map_or(0, |a| a.scrolloff)
    }

    /// Whether a scroll ahead made by input `k` of the client's, or one
    /// before it, is still to be made by Neovim.
    pub fn unmade(&self, k: u64) -> bool {
        self.ahead.iter().any(|a| a.seq.is_some_and(|s| s <= k))
    }

    /// The client's numbers for the inputs the scrolls ahead were made of,
    /// where it numbers them.
    pub fn seqs(&self) -> Vec<u64> {
        self.ahead.iter().filter_map(|a| a.seq).collect()
    }

    /// Whether there are scrolls ahead the view does not show yet.
    pub fn held(&self) -> bool {
        self.drawn < self.ahead.len()
    }

    /// When the oldest prediction is given up on, if there is one.
    pub fn until(&self) -> Option<Instant> {
        self.ahead.front().map(|a| a.until)
    }

    /// The view moved `delta` more rows, taking `gone` out of it — the rows
    /// [`leaving`] names, as they were (see [`Gone`]): they are kept either
    /// side, as Neovim's, and the offset grows by as much — less what the
    /// client had already scrolled ahead, which this confirms.
    ///
    /// The scrolls ahead are taken oldest first, as Neovim makes them, and
    /// as many as `delta` covers: Neovim may make two in one redraw. One the
    /// other way is a scroll the client did not see coming, and the view goes
    /// back to Neovim's before it moves on.
    ///
    /// `predicting` is whether the client scrolls ahead of Neovim (see
    /// [`crate::client::predict::Predictor`]): only then may a view come back
    /// over more lines than a slide shows.
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
        // What is left of `delta` once the scrolls ahead it confirms are
        // taken off: a scroll the client did not make.
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
            // Past the whole view, and not foreseen: nothing is known between
            // the two, and only `far` rows of it are animated — from a row
            // further, the first of which a glide shows gone at once.
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
        // Every line known, in order — those above from the furthest, the
        // view as it was, those below — and where the view is now among
        // them: the lines either side of it are kept either side, however
        // far it went, a jump further than the view is tall that the client
        // made ahead of Neovim as well. Of the view as it was, only the rows
        // that left it can be kept either side, so only those are known: the
        // rest are still in view.
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
        // Kept as far as a view is usually ahead, and behind by as much again,
        // while the client predicts; otherwise as far as a slide shows them,
        // which is never further than the view is tall, since the glide is
        // held to that below. Further, either way, as far as the scrolls still
        // ahead go, which may already be drawn — and no further.
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
        // What slides: as far as the view moved, less how far it had been
        // drawn ahead, which it no longer is — a scroll the client made but
        // had yet to draw slides as one it did not make.
        let slide = delta - (shown - self.shown_ahead());
        if self.animate && slide != 0 {
            let to = self.glide.position() + slide as f32;
            self.glide.set(to.clamp(-(h as f32), h as f32));
        }
        self.shown = self.glide.cells();
        // Neovim's own lines may be the ones a scroll ahead was waiting for.
        self.advance(&mut |_, _| None);
    }

    /// Scroll ahead of Neovim as `go` says, no further than [`HOLD`] times
    /// the rectangle is tall. `fill` draws the lines it uncovers, and `land`
    /// works out where it leaves the cursor; where either cannot yet, it is
    /// made but not drawn till [`Scroll::fill_in`] can. Says whether it was
    /// made: not where it, or one before it held back till now, leaves the
    /// cursor where Neovim moves the view sideways to show it — a view moved
    /// as a scroll up or down cannot draw it — which is let go, and those
    /// after it with it, for Neovim to draw.
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

    /// Draw what the scrolls ahead uncovered that `fill` could not before,
    /// and the scrolls it was holding back, as far as it can now, letting go
    /// of one that moves the view sideways as [`Scroll::predict`] does. Says
    /// whether the view shows more of them.
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

    /// The lines either side as far as the scrolls ahead go, from `fill`.
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

    /// How far above the rectangle's top the scrolls ahead go, at most —
    /// nought or below — and how far below it, nought or above.
    fn extent(&self) -> (i64, i64) {
        let (mut lo, mut hi, mut at) = (0, 0, 0);
        for a in &self.ahead {
            at += a.rows;
            (lo, hi) = (lo.min(at), hi.max(at));
        }
        (lo, hi)
    }

    /// Whether the lines a view ahead by `rows` shows are all known.
    fn known(&self, rows: i64) -> bool {
        let side = if rows > 0 { &self.below } else { &self.above };
        let n = rows.unsigned_abs() as usize;
        side.len() >= n && side.iter().take(n).all(Option::is_some)
    }

    /// Draw the scrolls ahead the view does not show yet, oldest first, as
    /// far as their lines are known and `land` can say where the cursor
    /// goes: each from the column the cursor wants after the one before.
    /// Says whether it let one go, and those after it (see
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
                // A scroll Neovim's word will judge, where the client draws
                // the cursor's line and what it decides of the rows as the
                // steps have them (see `crate::client::App::repaint`), sets
                // off with nothing of the slides before it left that shows:
                // what is left of them under a row would hold its first row
                // back while it went, the cursor drawn over a line it is not
                // on, the line it is on out of view, and its bar with it.
                if a.seq.is_some() && self.glide.cells() == 0 {
                    self.glide = Glide::default();
                }
                self.glide.add(a.rows as f32);
            }
        }
        self.shown = self.glide.cells();
        let_go
    }

    /// The rows the view shows ahead of Neovim's, as they are once it has
    /// stopped sliding — those still in view read from `grid`, its own, as
    /// [`Scrolling::row`] reads them: `None` for one not known.
    pub fn rows_ahead(&self, grid: &Grid) -> Option<Vec<Vec<Cell>>> {
        let at = self.shown_ahead();
        (0..self.height())
            .map(|r| self.line(r + at, grid).map(<[Cell]>::to_vec))
            .collect()
    }

    /// Stop sliding: the view is where it is going at once.
    pub fn settle(&mut self) {
        self.glide = Glide::default();
        self.shown = 0;
    }

    /// The line `i` rows down from the rectangle's top as Neovim has it: of
    /// those kept either side, or `grid`'s own for one in view. `None` for
    /// one not known.
    fn line<'a>(&'a self, i: i64, grid: &'a Grid) -> Option<&'a [Cell]> {
        if (0..self.height()).contains(&i) {
            let (top, _, left, right) = self.rect;
            let row = grid.row(top + i as usize);
            row.get(left.min(row.len())..right.min(row.len()))
        } else {
            self.kept(i).map(|k| k.cells.as_slice())
        }
    }

    /// The line `i` rows down from the rectangle's top as Neovim has it, of
    /// those kept either side of it: `None` for one in view, or not known.
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

    /// Stop being ahead of Neovim: the view goes back to Neovim's, sliding
    /// there if it slides.
    pub fn give_up(&mut self) {
        let shown = self.shown_ahead();
        self.ahead.clear();
        self.drawn = 0;
        if self.animate {
            self.glide.add(-shown as f32);
            self.shown = self.glide.cells();
        }
    }

    /// Give up on what Neovim has not confirmed in time.
    pub fn expire(&mut self, now: Instant) {
        if self.until().is_some_and(|until| until <= now) {
            self.give_up();
        }
    }

    /// Give up on none of the scrolls ahead before `until`: Neovim is still
    /// at work on the keys that make them.
    pub fn defer(&mut self, until: Instant) {
        for a in &mut self.ahead {
            a.until = a.until.max(until);
        }
    }

    /// Move on `dt` seconds of a scroll that settles in `duration`. Says
    /// whether it is still wanted: going, or ahead of Neovim.
    ///
    /// A slide is over once the offset is under a row: a glide that does
    /// not overshoot has nothing left to show after that.
    pub fn step(&mut self, dt: f32, duration: f32) -> bool {
        self.glide.step(dt, duration);
        self.shown = self.glide.cells();
        self.shown != 0 || !self.ahead.is_empty()
    }

    /// Whether a frame is wanted for it: the view is sliding.
    pub fn moving(&self) -> bool {
        self.glide.moving()
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

    /// The grid of a rectangle a column wide as Neovim has drawn it: `view`,
    /// from its top.
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

    /// Which line each row shows of a scroll over the grid that holds `view`.
    fn shows(s: &Scroll, view: &[Vec<Cell>]) -> Vec<Option<u32>> {
        let g = grid(view);
        (0..view.len())
            .map(|r| s.row(r, &g).and_then(|l| l.first()).map(|c| c.hl))
            .collect()
    }

    /// What a scroll of `delta` over `rect` takes out of `view`, as the
    /// animator takes it from the grid: see [`leaving`].
    fn gone(rect: (usize, usize, usize, usize), view: &[Vec<Cell>], delta: i64) -> Vec<Vec<Cell>> {
        let (top, bot) = leaving(rect, delta);
        view[top - rect.0..bot - rect.0].to_vec()
    }

    /// Scrolled down three: at once the first row has gone — no frame shows
    /// the view as it was — and it slides up a row at a time until it is the
    /// new one.
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

    /// Of the rows of a view sliding, each it lends from the grid is the
    /// grid's own row it names, and one it keeps a line of its own for names
    /// none: what runs past a row's end goes with the grid's rows (see
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

    /// What a scroll takes out of view is all it keeps of the view as it
    /// was: the rows it moves past, the top ones going down the buffer and
    /// the bottom ones up, and no more than there are.
    #[test]
    fn a_scroll_takes_out_of_view_the_rows_it_moves_past() {
        let rect = (2, 7, 0, 1);
        assert_eq!(leaving(rect, 3), (2, 5), "down the buffer");
        assert_eq!(leaving(rect, -2), (5, 7), "up it");
        assert_eq!(leaving(rect, 9), (2, 7), "past the view");
        assert_eq!(leaving(rect, -9), (2, 7));
    }

    /// Not predicting, a view keeps no more of the lines it passed than a
    /// slide shows, however long the scroll goes on — and the slide shows
    /// them all; predicting, it keeps them for a scroll ahead to come back
    /// over.
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

    /// Up as well as down, the first row at once.
    #[test]
    fn scrolling_up_slides_the_other_way() {
        let s = Scroll::new((0, 4, 0, 1), gone((0, 4, 0, 1), &lines(10..14), -2), -2, 1);
        assert_eq!(shows(&s, &lines(8..12)), [9, 10, 11, 12].map(Some));
    }

    /// A row's scroll is no slower than none: it is shown at once.
    #[test]
    fn a_single_row_is_shown_at_once() {
        let mut s = Scroll::new((0, 4, 0, 1), gone((0, 4, 0, 1), &lines(0..4), 1), 1, 1);
        s.step(0.001, 0.3);
        assert_eq!(shows(&s, &lines(1..5)), [1, 2, 3, 4].map(Some));
    }

    /// A second scroll before the first has settled carries on from where
    /// the view had got to: a row on, the first having gone at once.
    #[test]
    fn a_scroll_on_top_of_a_scroll_carries_on() {
        let mut s = Scroll::new((0, 4, 0, 1), gone((0, 4, 0, 1), &lines(0..4), 2), 2, 1);
        assert_eq!(shows(&s, &lines(2..6)), [1, 2, 3, 4].map(Some));
        s.scrolled(gone((0, 4, 0, 1), &lines(2..6), 2), 2, 1, false);
        assert_eq!(shows(&s, &lines(4..8)), [1, 2, 3, 4].map(Some));
        // And back again: what the first scroll put above is found below.
        let mut s = Scroll::new((0, 4, 0, 1), gone((0, 4, 0, 1), &lines(0..4), 2), 2, 1);
        s.scrolled(gone((0, 4, 0, 1), &lines(2..6), -2), -2, 1, false);
        assert_eq!(s.offset(), 0);
        assert_eq!(shows(&s, &lines(0..4)), [0, 1, 2, 3].map(Some));
    }

    /// Lines `n` on from the rectangle's top, as `Scroll::predict` asks for
    /// them, where the top shows line `top`; none past `last`.
    fn fill(top: u32, last: u32) -> impl FnMut(i64) -> Option<Vec<Cell>> {
        move |i| {
            let n = u32::try_from(i64::from(top) + i).ok()?;
            (n <= last).then(|| lines(n..n + 1).remove(0))
        }
    }

    /// `rows` ahead, given up on long after any test is over.
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

    /// A prediction shows at once, and Neovim's scroll, when it comes, lands
    /// on it: nothing moves.
    #[test]
    fn a_prediction_shows_at_once_and_neovim_lands_on_it() {
        let mut s = Scroll::still((0, 5, 0, 1), false);
        assert!(s.predict(go(2), &mut fill(10, 99), &mut |_, _| None));
        assert_eq!(shows(&s, &lines(10..15)), [12, 13, 14, 15, 16].map(Some));
        s.scrolled(gone((0, 5, 0, 1), &lines(10..15), 2), 2, 1, true);
        assert_eq!(s.ahead(), 0);
        assert_eq!(shows(&s, &lines(12..17)), [12, 13, 14, 15, 16].map(Some));
        assert!(!s.step(0.01, 0.0), "nothing left to do");
        // Up as well.
        let mut s = Scroll::still((0, 5, 0, 1), false);
        assert!(s.predict(go(-3), &mut fill(10, 99), &mut |_, _| None));
        assert_eq!(shows(&s, &lines(10..15)), [7, 8, 9, 10, 11].map(Some));
    }

    /// A row scrolled ahead while the slide before it settles shows at once
    /// where Neovim's word will judge it — what was left of that slide is
    /// less than a row, and shows nothing — and is held back by it where
    /// not, as it always was.
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

    /// Two turns of the wheel Neovim makes in one redraw are both confirmed
    /// by it; one it has yet to make stays ahead.
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

    /// Down and back up before Neovim has made either: the view is where it
    /// started, and stays there as Neovim makes both.
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

    /// Neovim scrolling the other way is a scroll the client did not see
    /// coming: its predictions go, and the view is Neovim's.
    #[test]
    fn a_scroll_the_other_way_takes_the_predictions_with_it() {
        let mut s = Scroll::still((0, 5, 0, 1), false);
        s.predict(go(3), &mut fill(10, 99), &mut |_, _| None);
        s.scrolled(gone((0, 5, 0, 1), &lines(10..15), -2), -2, 1, true);
        assert_eq!(s.ahead(), 0);
        assert_eq!(shows(&s, &lines(8..13)), [8, 9, 10, 11, 12].map(Some));
    }

    /// A prediction Neovim never confirms is given up on in time, and the view
    /// slides back to Neovim's.
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

    /// Neovim heard still at work on the keys typed ahead of it, its
    /// predictions are given up on no sooner than it says — never sooner than
    /// they would have been — and then all the same, and all together.
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

    /// No further than [`HOLD`] times the window is tall; and a scroll
    /// whose lines are yet to come is made but drawn only once they have,
    /// those after it with it.
    #[test]
    fn a_prediction_waits_for_its_lines() {
        let mut s = Scroll::still((0, 5, 0, 1), false);
        let far = 5 * HOLD;
        assert!(!s.predict(go(far + 1), &mut fill(10, 999), &mut |_, _| None));
        assert!(s.predict(go(far), &mut fill(10, 999), &mut |_, _| None));
        assert_eq!(shows(&s, &lines(10..15))[0], Some(10 + far as u32));

        // Further than lines have come: held, not refused, the scrolls after
        // it made from where it goes; drawn as the lines come.
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

        // Neovim's own scroll may come first: it lands the scroll held as
        // well, and the next is drawn from its lines.
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

    /// Where a scroll leaves the cursor is worked out as it is drawn, from
    /// the column the cursor wants after the scroll before.
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

    /// A scroll whose landing Neovim moves the view sideways for is let go
    /// — not made, if it is the one being made — and those held back after
    /// it with it; those before it stay drawn.
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
        // Held back for its lines, and one after it: both go when they come.
        let mut s = Scroll::still((0, 5, 0, 1), false);
        assert!(s.predict(landing(15), &mut fill(10, 99), &mut land));
        assert!(s.predict(landing(20), &mut fill(10, 19), &mut land));
        assert!(s.predict(landing(25), &mut fill(10, 19), &mut land));
        assert_eq!((s.ahead(), s.shown_ahead()), (15, 5));
        assert!(!s.fill_in(&mut fill(10, 99), &mut land));
        assert_eq!((s.ahead(), s.shown_ahead()), (5, 5));
        assert!(!s.held());
    }

    /// The rows a view sliding to where it is ahead of Neovim shows once it
    /// is there; and stopped, it is there at once.
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

    /// A jump further than the window is tall, made ahead of Neovim, is
    /// landed on as any other: nothing moves when Neovim makes it, and what
    /// the view passed is kept either side.
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

    /// Scrolls made one after another while the view still slides from the
    /// ones before land together the slide's duration after the last, and
    /// no later, however many there were.
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

    /// Past the whole view, only `far` rows are animated, and drawn blank.
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
