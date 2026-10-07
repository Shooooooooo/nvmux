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

/// A scroll the client makes ahead of Neovim: see [`Scroll::predict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ahead {
    /// How far: above nought down the buffer, as `scroll_delta` counts.
    pub rows: i64,
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
    /// made, oldest first, each with when it is given up on: above nought
    /// down the buffer, as `scroll_delta` counts. What they add up to is how
    /// far the view is ahead of Neovim's. See [`crate::client::predict`].
    ahead: VecDeque<(i64, Instant)>,
    /// `'scrolloff'`, as the last of them found it.
    scrolloff: usize,
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
            scrolloff: 0,
        }
    }

    pub fn rect(&self) -> (usize, usize, usize, usize) {
        self.rect
    }

    /// How many rows the view is ahead of Neovim's.
    pub fn ahead(&self) -> i64 {
        self.ahead.iter().map(|(n, _)| n).sum()
    }

    pub fn scrolloff(&self) -> usize {
        self.scrolloff
    }

    /// When the oldest prediction is given up on, if there is one.
    pub fn until(&self) -> Option<Instant> {
        self.ahead.front().map(|(_, until)| *until)
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
            .is_some_and(|(n, _)| n.signum() != delta.signum())
        {
            self.give_up();
        }
        // What is left of `delta` once the scrolls ahead it confirms are
        // taken off: a scroll the client did not make, which slides.
        let mut left = delta;
        while left != 0 {
            match self.ahead.front_mut() {
                Some((n, _)) if n.signum() == left.signum() => {
                    if n.abs() <= left.abs() {
                        left -= *n;
                        self.ahead.pop_front();
                    } else {
                        *n -= left;
                        left = 0;
                    }
                }
                _ => break,
            }
        }
        let h = self.current.len();
        let d = delta.unsigned_abs() as usize;
        let old = std::mem::replace(&mut self.current, after);
        if d > h {
            // Past the whole view: nothing is known between the two, and
            // only `far` rows of it are animated.
            self.above.clear();
            self.below.clear();
            self.ahead.clear();
            let rows = if self.animate { far.min(h) as f32 } else { 0.0 };
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
        if self.animate {
            self.spring.position =
                (self.spring.position + left as f32).clamp(-(h as f32), h as f32);
        }
        // Kept as far as the view could reach, and no further: a view ahead
        // by as much as it is tall, behind by as much again.
        self.above.truncate(2 * h + 1);
        self.below.truncate(2 * h + 1);
        self.shown = self.spring.cells();
    }

    /// Neovim redrew the rectangle without scrolling it.
    pub fn refresh(&mut self, after: Vec<Vec<Cell>>) {
        if after.len() == self.current.len() {
            self.current = after;
        }
    }

    /// Scroll ahead of Neovim as `go` says. `fill` draws a line the view
    /// uncovers, counted from the top of the rectangle as Neovim has it: `h`
    /// is the first line below it, `-1` the first above; `None` for one it
    /// cannot.
    ///
    /// No further than the rectangle is tall, nor than `fill` can draw. Says
    /// how far it went.
    pub fn predict(&mut self, go: Ahead, fill: &mut dyn FnMut(i64) -> Option<Vec<Cell>>) -> i64 {
        let Ahead {
            rows,
            until,
            scrolloff,
        } = go;
        let h = self.current.len() as i64;
        let ahead = self.ahead();
        let mut target = (ahead + rows).clamp(-h, h);
        if target > 0 {
            while (self.below.len() as i64) < target {
                match fill(h + self.below.len() as i64) {
                    Some(line) => self.below.push_back(Some(line)),
                    None => break,
                }
            }
            target = target.min(self.below.iter().take_while(|l| l.is_some()).count() as i64);
        } else if target < 0 {
            while (self.above.len() as i64) < -target {
                match fill(-1 - self.above.len() as i64) {
                    Some(line) => self.above.push_back(Some(line)),
                    None => break,
                }
            }
            target = target.max(-(self.above.iter().take_while(|l| l.is_some()).count() as i64));
        }
        let went = target - ahead;
        if went == 0 || went.signum() != rows.signum() {
            return 0;
        }
        self.ahead.push_back((went, until));
        self.scrolloff = scrolloff;
        if self.animate {
            self.spring.position += went as f32;
            self.shown = self.spring.cells();
        }
        went
    }

    /// Stop being ahead of Neovim: the view goes back to Neovim's, sliding
    /// there if it slides.
    pub fn give_up(&mut self) {
        let ahead = self.ahead();
        self.ahead.clear();
        if self.animate {
            self.spring.position -= ahead as f32;
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
        let i = r as i64 - self.shown + self.ahead();
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
            until: Instant::now() + std::time::Duration::from_secs(60),
            scrolloff: 0,
        }
    }

    /// A prediction shows at once, and Neovim's scroll, when it comes, lands
    /// on it: nothing moves.
    #[test]
    fn a_prediction_shows_at_once_and_neovim_lands_on_it() {
        let mut s = Scroll::still((0, 5, 0, 1), lines(10..15), false);
        assert_eq!(s.predict(go(2), &mut fill(10, 99)), 2);
        assert_eq!(shows(&s), [12, 13, 14, 15, 16].map(Some));
        s.scrolled(lines(12..17), 2, 1);
        assert_eq!(s.ahead(), 0);
        assert_eq!(shows(&s), [12, 13, 14, 15, 16].map(Some));
        assert!(!s.step(0.01, 0.0), "nothing left to do");
        // Up as well.
        let mut s = Scroll::still((0, 5, 0, 1), lines(10..15), false);
        assert_eq!(s.predict(go(-3), &mut fill(10, 99)), -3);
        assert_eq!(shows(&s), [7, 8, 9, 10, 11].map(Some));
    }

    /// Two turns of the wheel Neovim makes in one redraw are both confirmed
    /// by it; one it has yet to make stays ahead.
    #[test]
    fn neovim_confirms_the_oldest_first() {
        let mut s = Scroll::still((0, 8, 0, 1), lines(0..8), false);
        for _ in 0..3 {
            assert_eq!(s.predict(go(2), &mut fill(0, 99)), 2);
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
        s.predict(go(3), &mut fill(10, 99));
        s.predict(go(-3), &mut fill(10, 99));
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
        s.predict(go(3), &mut fill(10, 99));
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
        let go = Ahead {
            rows: 2,
            until,
            scrolloff: 0,
        };
        s.predict(go, &mut fill(10, 99));
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

    /// No further than the lines it can draw, nor than the window is tall.
    #[test]
    fn a_prediction_goes_no_further_than_it_can_draw() {
        let mut s = Scroll::still((0, 5, 0, 1), lines(10..15), false);
        assert_eq!(s.predict(go(3), &mut fill(10, 16)), 2);
        assert_eq!(s.predict(go(3), &mut fill(10, 16)), 0);
        let mut s = Scroll::still((0, 5, 0, 1), lines(10..15), false);
        assert_eq!(s.predict(go(9), &mut fill(10, 99)), 5);
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
