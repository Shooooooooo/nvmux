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

use super::spring::Spring;
use crate::client::compose::Scrolling;
use crate::client::grid::Cell;

/// A line of a scroll's rectangle, or `None` for one nothing is known of.
type Line = Option<Vec<Cell>>;

/// How many times as far as it is tall a view may be scrolled ahead of
/// Neovim: as many pages as are typed in a round trip of a slow link, a key
/// held down and repeating among them. The agent sends lines as far ahead
/// of a view that moves (see `super::super::AGENT_LUA`).
pub const REACH: i64 = 8;

/// What draws a line a view scrolled ahead uncovers, counted from the top
/// of the rectangle as Neovim has it: `h` is the first line below it, `-1`
/// the first above; `None` for one it cannot yet.
pub type Fill<'a> = dyn FnMut(i64) -> Option<Vec<Cell>> + 'a;

/// What works out the cursor's column where a scroll ahead lands it (see
/// [`Land`]), given the display column it wants, if a scroll before says:
/// the grid column and the display column it wants from then on — `None`
/// for one outside the window — or nothing yet, for a line not yet sent.
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
    /// lets it have.
    pub cursor: i64,
    /// The grid column it leaves the cursor in, if not the one it was in;
    /// and the display column the cursor wants to be in from then on, as
    /// Neovim's `curswant` keeps it.
    pub col: Option<(usize, usize)>,
    /// Where `col` is yet to be worked out: see [`Land`].
    pub land: Option<Land>,
    /// When it is given up on, if Neovim has not made it by then.
    pub until: Instant,
    /// `'scrolloff'`: how near the window's edges Neovim lets the cursor be.
    pub scrolloff: usize,
}

/// One rectangle scrolling — or scrolled ahead of Neovim, on the client's
/// own say-so.
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
    /// A scroll of `delta` rows over `rect`, from the lines it showed before
    /// to the ones it shows now.
    pub fn new(
        rect: (usize, usize, usize, usize),
        before: Vec<Vec<Cell>>,
        after: Vec<Vec<Cell>>,
        delta: i64,
        far: usize,
    ) -> Self {
        let mut s = Self::still(rect, before, true);
        s.scrolled(after, delta, far);
        s
    }

    /// Nothing moving yet over `rect`, which shows `current`: what a
    /// prediction starts from.
    pub fn still(
        rect: (usize, usize, usize, usize),
        current: Vec<Vec<Cell>>,
        animate: bool,
    ) -> Self {
        Self {
            rect,
            above: VecDeque::new(),
            current,
            below: VecDeque::new(),
            spring: Spring::default(),
            shown: 0,
            animate,
            ahead: VecDeque::new(),
            drawn: 0,
        }
    }

    pub fn rect(&self) -> (usize, usize, usize, usize) {
        self.rect
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

    /// Whether there are scrolls ahead the view does not show yet.
    pub fn held(&self) -> bool {
        self.drawn < self.ahead.len()
    }

    /// When the oldest prediction is given up on, if there is one.
    pub fn until(&self) -> Option<Instant> {
        self.ahead.front().map(|a| a.until)
    }

    /// The view moved `delta` more rows, and its rectangle now holds
    /// `after`: the lines that left it are kept either side, and the offset
    /// grows by as much — less what the client had already scrolled ahead,
    /// which this confirms.
    ///
    /// The scrolls ahead are taken oldest first, as Neovim makes them, and
    /// as many as `delta` covers: Neovim may make two in one redraw. One the
    /// other way is a scroll the client did not see coming, and the view goes
    /// back to Neovim's before it moves on.
    pub fn scrolled(&mut self, after: Vec<Vec<Cell>>, delta: i64, far: usize) {
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
        let h = self.current.len() as i64;
        let old = std::mem::replace(&mut self.current, after);
        if delta.abs() > h && left == delta {
            // Past the whole view, and not foreseen: nothing is known between
            // the two, and only `far` rows of it are animated.
            self.above.clear();
            self.below.clear();
            self.ahead.clear();
            self.drawn = 0;
            let rows = if self.animate {
                far.min(h as usize) as f32
            } else {
                0.0
            };
            self.spring = Spring::at(if delta > 0 { rows } else { -rows });
            self.shown = self.spring.cells();
            return;
        }
        // Every line known, in order — those above from the furthest, the
        // view as it was, those below — and where the view is now among
        // them: the lines either side of it are kept either side, however
        // far it went, a jump further than the view is tall that the client
        // made ahead of Neovim as well.
        let above = self.above.len() as i64;
        let mut known: Vec<Line> = std::mem::take(&mut self.above)
            .into_iter()
            .rev()
            .chain(old.into_iter().map(Some))
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
        // Kept as far as the view could reach, and no further: a view ahead
        // by [`REACH`] times as much as it is tall, behind by as much again.
        let keep = 2 * REACH * h + 1;
        self.above = (1..=keep)
            .map(|k| at - k)
            .take_while(|i| *i >= 0)
            .map(&mut take)
            .collect();
        self.below = (0..keep)
            .map(|k| at + h + k)
            .take_while(|i| *i < n)
            .map(&mut take)
            .collect();
        // What slides: as far as the view moved, less how far it had been
        // drawn ahead, which it no longer is — a scroll the client made but
        // had yet to draw slides as one it did not make.
        let slide = delta - (shown - self.shown_ahead());
        if self.animate {
            self.spring.position =
                (self.spring.position + slide as f32).clamp(-(h as f32), h as f32);
        }
        self.shown = self.spring.cells();
        // Neovim's own lines may be the ones a scroll ahead was waiting for.
        self.advance(&mut |_, _| None);
    }

    /// Neovim redrew the rectangle without scrolling it.
    pub fn refresh(&mut self, after: Vec<Vec<Cell>>) {
        if after.len() == self.current.len() {
            self.current = after;
        }
    }

    /// Scroll ahead of Neovim as `go` says, no further than [`REACH`] times
    /// the rectangle is tall. `fill` draws the lines it uncovers, and `land`
    /// works out where it leaves the cursor; where either cannot yet, it is
    /// made but not drawn till [`Scroll::fill_in`] can. Says whether it was
    /// made.
    pub fn predict(&mut self, go: Ahead, fill: &mut Fill, land: &mut Lander) -> bool {
        let h = self.current.len() as i64;
        let target = self.ahead() + go.rows;
        if go.rows == 0 || target.abs() > REACH * h {
            return false;
        }
        self.ahead.push_back(go);
        self.reach(fill);
        self.advance(land);
        true
    }

    /// Draw what the scrolls ahead uncovered that `fill` could not before,
    /// and the scrolls it was holding back, as far as it can now (see
    /// [`Scroll::predict`]). Says whether the view shows more of them.
    pub fn fill_in(&mut self, fill: &mut Fill, land: &mut Lander) -> bool {
        let h = self.current.len() as i64;
        for (k, line) in self.below.iter_mut().enumerate() {
            if line.is_none() {
                *line = fill(h + k as i64);
            }
        }
        for (k, line) in self.above.iter_mut().enumerate() {
            if line.is_none() {
                *line = fill(-1 - k as i64);
            }
        }
        self.reach(fill);
        let drawn = self.drawn;
        self.advance(land);
        self.drawn != drawn
    }

    /// The lines either side as far as the scrolls ahead go, from `fill`.
    fn reach(&mut self, fill: &mut Fill) {
        let h = self.current.len() as i64;
        let (mut lo, mut hi, mut at) = (0, 0, 0);
        for a in &self.ahead {
            at += a.rows;
            (lo, hi) = (lo.min(at), hi.max(at));
        }
        while (self.below.len() as i64) < hi {
            let line = fill(h + self.below.len() as i64);
            self.below.push_back(line);
        }
        while (self.above.len() as i64) < -lo {
            let line = fill(-1 - self.above.len() as i64);
            self.above.push_back(line);
        }
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
    fn advance(&mut self, land: &mut Lander) {
        let mut at = self.shown_ahead();
        let mut want = self.shown_col().map(|(_, want)| want);
        while let Some(a) = self.ahead.get(self.drawn).copied() {
            if !self.known(at + a.rows) {
                break;
            }
            let mut col = a.col;
            if let Some(l) = a.land {
                let Some(landed) = land(l, want) else {
                    break;
                };
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
                self.spring.position += a.rows as f32;
            }
        }
        self.shown = self.spring.cells();
    }

    /// Stop being ahead of Neovim: the view goes back to Neovim's, sliding
    /// there if it slides.
    pub fn give_up(&mut self) {
        let shown = self.shown_ahead();
        self.ahead.clear();
        self.drawn = 0;
        if self.animate {
            self.spring.position -= shown as f32;
            self.shown = self.spring.cells();
        }
    }

    /// Give up on what Neovim has not confirmed in time.
    pub fn expire(&mut self, now: Instant) {
        if self.until().is_some_and(|until| until <= now) {
            self.give_up();
        }
    }

    /// Move on `dt` seconds of a scroll that settles in `duration`. Says
    /// whether it is still wanted: going, or ahead of Neovim.
    ///
    /// A slide is over once the offset is under a row: a spring that does
    /// not overshoot has nothing left to show after that.
    pub fn step(&mut self, dt: f32, duration: f32) -> bool {
        self.spring.step(dt, duration);
        self.shown = self.spring.cells();
        self.shown != 0 || !self.ahead.is_empty()
    }

    /// Whether a frame is wanted for it: the view is sliding.
    pub fn moving(&self) -> bool {
        self.spring.moving()
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
        let i = r as i64 - self.shown + self.shown_ahead();
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
        }
    }

    /// A prediction shows at once, and Neovim's scroll, when it comes, lands
    /// on it: nothing moves.
    #[test]
    fn a_prediction_shows_at_once_and_neovim_lands_on_it() {
        let mut s = Scroll::still((0, 5, 0, 1), lines(10..15), false);
        assert!(s.predict(go(2), &mut fill(10, 99), &mut |_, _| None));
        assert_eq!(shows(&s), [12, 13, 14, 15, 16].map(Some));
        s.scrolled(lines(12..17), 2, 1);
        assert_eq!(s.ahead(), 0);
        assert_eq!(shows(&s), [12, 13, 14, 15, 16].map(Some));
        assert!(!s.step(0.01, 0.0), "nothing left to do");
        // Up as well.
        let mut s = Scroll::still((0, 5, 0, 1), lines(10..15), false);
        assert!(s.predict(go(-3), &mut fill(10, 99), &mut |_, _| None));
        assert_eq!(shows(&s), [7, 8, 9, 10, 11].map(Some));
    }

    /// Two turns of the wheel Neovim makes in one redraw are both confirmed
    /// by it; one it has yet to make stays ahead.
    #[test]
    fn neovim_confirms_the_oldest_first() {
        let mut s = Scroll::still((0, 8, 0, 1), lines(0..8), false);
        for _ in 0..3 {
            assert!(s.predict(go(2), &mut fill(0, 99), &mut |_, _| None));
        }
        assert_eq!(shows(&s)[0], Some(6));
        s.scrolled(lines(4..12), 4, 1);
        assert_eq!(s.ahead(), 2);
        assert_eq!(shows(&s)[0], Some(6), "still where the client put it");
        s.scrolled(lines(6..14), 2, 1);
        assert_eq!(s.ahead(), 0);
        assert_eq!(shows(&s)[0], Some(6));
    }

    /// Down and back up before Neovim has made either: the view is where it
    /// started, and stays there as Neovim makes both.
    #[test]
    fn down_and_back_up_never_echoes() {
        let mut s = Scroll::still((0, 5, 0, 1), lines(10..15), false);
        s.predict(go(3), &mut fill(10, 99), &mut |_, _| None);
        s.predict(go(-3), &mut fill(10, 99), &mut |_, _| None);
        assert_eq!(shows(&s), [10, 11, 12, 13, 14].map(Some));
        s.scrolled(lines(13..18), 3, 1);
        assert_eq!(shows(&s), [10, 11, 12, 13, 14].map(Some));
        s.scrolled(lines(10..15), -3, 1);
        assert_eq!(shows(&s), [10, 11, 12, 13, 14].map(Some));
        assert_eq!(s.ahead(), 0);
    }

    /// Neovim scrolling the other way is a scroll the client did not see
    /// coming: its predictions go, and the view is Neovim's.
    #[test]
    fn a_scroll_the_other_way_takes_the_predictions_with_it() {
        let mut s = Scroll::still((0, 5, 0, 1), lines(10..15), false);
        s.predict(go(3), &mut fill(10, 99), &mut |_, _| None);
        s.scrolled(lines(8..13), -2, 1);
        assert_eq!(s.ahead(), 0);
        assert_eq!(shows(&s), [8, 9, 10, 11, 12].map(Some));
    }

    /// A prediction Neovim never confirms is given up on in time, and the view
    /// slides back to Neovim's.
    #[test]
    fn an_unconfirmed_prediction_slides_back() {
        let t0 = Instant::now();
        let until = t0 + std::time::Duration::from_millis(500);
        let mut s = Scroll::still((0, 5, 0, 1), lines(10..15), true);
        let go = Ahead { until, ..go(2) };
        s.predict(go, &mut fill(10, 99), &mut |_, _| None);
        assert_eq!(
            shows(&s),
            [10, 11, 12, 13, 14].map(Some),
            "from where it was"
        );
        while s.step(0.01, 0.3) && s.moving() {}
        assert_eq!(
            shows(&s),
            [12, 13, 14, 15, 16].map(Some),
            "to the prediction"
        );
        s.expire(until - std::time::Duration::from_millis(1));
        assert_eq!(s.ahead(), 2, "not yet");
        s.expire(until);
        assert_eq!(s.ahead(), 0);
        assert_eq!(shows(&s), [12, 13, 14, 15, 16].map(Some), "back from there");
        while s.step(0.01, 0.3) {}
        assert_eq!(shows(&s), [10, 11, 12, 13, 14].map(Some));
    }

    /// No further than [`REACH`] times the window is tall; and a scroll
    /// whose lines are yet to come is made but drawn only once they have,
    /// those after it with it.
    #[test]
    fn a_prediction_waits_for_its_lines() {
        let mut s = Scroll::still((0, 5, 0, 1), lines(10..15), false);
        let far = 5 * REACH;
        assert!(!s.predict(go(far + 1), &mut fill(10, 99), &mut |_, _| None));
        assert!(s.predict(go(far), &mut fill(10, 99), &mut |_, _| None));
        assert_eq!(shows(&s)[0], Some(10 + far as u32));

        let mut s = Scroll::still((0, 5, 0, 1), lines(10..15), false);
        assert!(s.predict(go(2), &mut fill(10, 18), &mut |_, _| None));
        assert!(s.predict(go(3), &mut fill(10, 18), &mut |_, _| None));
        assert!(s.predict(go(-1), &mut fill(10, 18), &mut |_, _| None));
        assert_eq!(s.ahead(), 4, "all three made");
        assert!(s.held());
        assert_eq!(shows(&s), [12, 13, 14, 15, 16].map(Some), "the first drawn");
        assert!(!s.fill_in(&mut fill(10, 18), &mut |_, _| None));
        assert!(s.fill_in(&mut fill(10, 99), &mut |_, _| None));
        assert!(!s.held());
        assert_eq!(shows(&s), [14, 15, 16, 17, 18].map(Some));

        // Neovim's own scroll may come first: it lands the scroll held as
        // well, and the next is drawn from its lines.
        let mut s = Scroll::still((0, 5, 0, 1), lines(10..15), false);
        assert!(s.predict(go(5), &mut fill(10, 16), &mut |_, _| None));
        assert!(s.predict(go(1), &mut fill(10, 16), &mut |_, _| None));
        assert_eq!(shows(&s), [10, 11, 12, 13, 14].map(Some));
        s.scrolled(lines(15..20), 5, 1);
        assert_eq!(s.ahead(), 1);
        assert!(s.held(), "line 20 is still to come");
        assert!(s.fill_in(&mut fill(15, 20), &mut |_, _| None));
        assert_eq!(shows(&s), [16, 17, 18, 19, 20].map(Some));
    }

    /// Where a scroll leaves the cursor is worked out as it is drawn, from
    /// the column the cursor wants after the scroll before.
    #[test]
    fn the_cursor_lands_as_its_scroll_is_drawn() {
        let mut s = Scroll::still((0, 5, 0, 1), lines(10..15), false);
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

    /// A jump further than the window is tall, made ahead of Neovim, is
    /// landed on as any other: nothing moves when Neovim makes it, and what
    /// the view passed is kept either side.
    #[test]
    fn a_jump_past_the_view_made_ahead_lands_as_well() {
        let mut s = Scroll::still((0, 5, 0, 1), lines(10..15), false);
        assert!(s.predict(go(8), &mut fill(10, 99), &mut |_, _| None));
        assert_eq!(shows(&s), [18, 19, 20, 21, 22].map(Some));
        s.scrolled(lines(18..23), 8, 1);
        assert_eq!(s.ahead(), 0);
        assert_eq!(shows(&s), [18, 19, 20, 21, 22].map(Some));
        assert!(s.predict(go(-8), &mut fill(18, 99), &mut |_, _| None));
        assert_eq!(
            shows(&s),
            [10, 11, 12, 13, 14].map(Some),
            "back over what it passed"
        );
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
