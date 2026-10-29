//! Cursor steps predicted ahead of Neovim (see `crate::client::predict`).
//!
//! Normal-mode motions whose result the client can compute
//! (`crate::client::moves`) move the drawn cursor at once. Each key becomes a
//! [`Step`] holding a buffer position (line, byte, wanted column, view), not a
//! screen cell, which a view change or a frame drawn mid-sequence would make
//! wrong. The cursor is drawn at the newest step whose view matches the
//! screen's.
//!
//! The agent reports where each key left the cursor. A matching step is
//! dropped when Neovim's frame for it arrives; a mismatch drops all steps, and
//! so does a timeout once Neovim has taken the key (see
//! `super::Animator::idle`). Hidden steps, made only to be checked while a
//! kind of prediction regains trust (`crate::client::predict::Trust`), are
//! never drawn, nor any after them.
//!
//! Insert-mode steps (`crate::client::typing`) also keep the edited line's text
//! and cells, drawn over Neovim's row while the step is drawn; Neovim's ack
//! checks the text too. Steps that change mode draw the cursor in the new
//! mode's shape (`super::Animator`).
//!
//! Cursor-line decorations (`'relativenumber'`, `'cursorline'`) are redrawn
//! over Neovim's rows while the cursor is drawn on a line other than Neovim's
//! (`crate::client::App::repaint`). Motions past `'scrolloff'` and scroll keys
//! also scroll the view ahead (`super::scroll`); [`Chain::top`] is the result.
//! All steps and scrolls are dropped when a Neovim frame moves the view any
//! other way, or changes the text of a row showing a visited line (an unseen
//! edit, inline virtual text). A line scrolled into view must be drawn as the
//! client laid it out, give or take what is drawn over its blanks
//! ([`shows`]). Changes to other rows or to gutters, and a typed line drawn as
//! a step typed it, keep the steps.

use std::collections::VecDeque;
use std::time::Instant;

use crate::client::grid::{Cell, Text};
use crate::client::predict::{Pos, Proof};

/// A visited line as drawn when first visited; `None` if it was not drawn.
pub type Drawn = Option<Seen>;

/// A visited line's text past the gutter: Neovim's whole row (`whole`), or,
/// for a line a predicted scroll brings into view, the client's layout of its
/// text only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub text: Vec<Text>,
    pub whole: bool,
}

impl Seen {
    /// Whether Neovim's row `now` still matches: equal, or (not `whole`)
    /// starting with the client's layout as [`shows`] reads it. `None` (out
    /// of view) agrees.
    pub fn agrees(&self, now: Option<&[Text]>) -> bool {
        now.is_none_or(|t| {
            if self.whole {
                t == self.text.as_slice()
            } else {
                t.len() >= self.text.len()
                    && self
                        .text
                        .iter()
                        .zip(t)
                        .all(|(laid, drawn)| shows(laid, drawn))
            }
        })
    }
}

/// Whether a cell Neovim drew as `drawn` shows the client's layout `laid`:
/// the same, or anything over a blank. An overlay drawn by a decoration
/// provider, such as snacks.nvim's indent guides, is no extmark the agent
/// can report, and moves no text.
pub fn shows(laid: &Text, drawn: &Text) -> bool {
    laid == drawn || *laid == Text::Char(' ')
}

/// Max keys predicted ahead of Neovim in a window, only a bound on growth:
/// a key held while Neovim is busy chains as far as the lines sent ahead go
/// (see `crate::client::predict::Predictor::reach`). Under
/// `crate::client::predict`'s `QUEUE`, past which the keys are in doubt and
/// every step is dropped.
pub const LONGEST: usize = 256;

/// Predicted steps in the cursor's window, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    /// Window, gutter width and leftmost display column the steps were
    /// computed with; a different gutter or horizontal scroll misplaces them.
    pub win: i64,
    pub textoff: usize,
    pub left: usize,
    /// Lines the steps visit, each as drawn when first visited.
    pub lines: Vec<(i64, Drawn)>,
    /// Top line once Neovim catches up: its view plus every predicted scroll,
    /// as the newest step left it. See [`Chain::views`].
    pub top: i64,
    steps: VecDeque<Step>,
}

/// One predicted key: where it leaves the cursor and, if it or an earlier
/// step typed, the edited line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The key's sequence number (see `crate::client::predict::Sync`).
    pub seq: u64,
    /// Cursor and view as the key's ack should report them, and the cursor's
    /// display column.
    pub pos: Pos,
    pub vcol: usize,
    /// Prediction kind, and whether a confirmed one counts toward its kind's
    /// trust (`crate::client::predict::Trust`): a motion that moved the
    /// cursor, a non-blank typed, or a deletion.
    pub proof: Proof,
    pub credited: bool,
    /// `b:changedtick` and line count of the buffer the step was computed
    /// over. The tick is what the ack reports, except after typed steps,
    /// whose tick the client cannot know.
    pub tick: i64,
    pub line_count: i64,
    /// The line typed into, as the steps leave it.
    pub echo: Option<Echo>,
    /// Where `<C-w>` and `<C-u>` stop in Insert mode, if known (see
    /// `crate::client::predict::Stop`).
    pub insstart: Option<crate::client::predict::Stop>,
    /// Made only to be checked against Neovim; never drawn.
    pub hidden: bool,
    /// Dropped at this time if Neovim has not answered for the key, once it
    /// has taken the key (see `super::Animator::idle`).
    pub until: Instant,
}

/// A line typed into ahead of Neovim (see `crate::client::typing`): its text
/// and the cells Neovim will draw past the gutter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Echo {
    pub line: i64,
    pub text: Box<str>,
    pub cells: Vec<Cell>,
}

impl Chain {
    /// `line` is the first step's line and `drawn` how Neovim draws it.
    pub fn new((win, textoff, left): (i64, usize, usize), line: i64, drawn: Drawn) -> Self {
        Self {
            win,
            textoff,
            left,
            lines: vec![(line, drawn)],
            top: 0,
            steps: VecDeque::new(),
        }
    }

    pub fn push(&mut self, step: Step) {
        self.steps.push_back(step);
    }

    /// Record that a step visits `line`; `drawn` is called on the first visit
    /// only.
    pub fn onto(&mut self, line: i64, drawn: impl FnOnce() -> Drawn) {
        if !self.lines.iter().any(|(l, _)| *l == line) {
            self.lines.push((line, drawn()));
        }
    }

    /// Whether Neovim's row for `line` (`now`, past the gutter) shows the line
    /// as some step typed it, as Neovim's frames for keys typed ahead do.
    pub fn typed(&self, line: i64, now: Option<&[Text]>) -> bool {
        let Some(texts) = now else {
            return false;
        };
        self.steps
            .iter()
            .filter_map(|s| s.echo.as_ref())
            .any(|e| e.line == line && e.cells.iter().map(|c| &c.text).eq(texts))
    }

    pub fn types_into(&self, line: i64) -> bool {
        self.steps
            .iter()
            .any(|s| s.echo.as_ref().is_some_and(|e| e.line == line))
    }

    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// The newest step, which the next key starts from.
    pub fn head(&self) -> Option<&Step> {
        self.steps.back()
    }

    /// The step to draw for a screen view at (`top`, `left`): the newest one
    /// with that view, before the first hidden step.
    pub fn shown(&self, top: i64, left: usize) -> Option<&Step> {
        self.steps
            .iter()
            .take_while(|s| !s.hidden)
            .filter(|s| (s.pos.top, s.pos.left) == (top, left))
            .last()
    }

    /// Whether view top `top` (Neovim's plus predicted scrolls) is one the
    /// steps assume: [`Chain::top`], or a hidden step's, since hidden steps do
    /// not scroll ahead and Neovim's frame moves the view for them.
    pub fn views(&self, top: i64) -> bool {
        top == self.top || self.steps.iter().any(|s| s.hidden && s.pos.top == top)
    }

    pub fn step_for(&self, seq: u64) -> Option<&Step> {
        self.steps.iter().find(|s| s.seq == seq)
    }

    /// The newest step for a key before `seq`.
    pub fn before(&self, seq: u64) -> Option<&Step> {
        self.steps.iter().rev().find(|s| s.seq < seq)
    }

    /// Drop the steps up to `seq`, whose frame from Neovim has arrived, and
    /// the lines only they visited.
    pub fn drop_upto(&mut self, seq: u64) {
        self.steps.retain(|s| s.seq > seq);
        let steps = &self.steps;
        self.lines
            .retain(|(line, _)| steps.iter().any(|s| s.pos.line == *line));
    }

    pub fn seqs(&self) -> Vec<u64> {
        self.steps.iter().map(|s| s.seq).collect()
    }

    /// When the first step for a key up to `idle` (one Neovim has taken)
    /// expires unconfirmed.
    pub fn until(&self, idle: u64) -> Option<Instant> {
        self.steps
            .iter()
            .filter(|s| s.seq <= idle)
            .map(|s| s.until)
            .min()
    }

    /// When the first step for a later key passes its deadline, and the
    /// client asks Neovim whether it has taken the key.
    pub fn waiting(&self, idle: u64) -> Option<Instant> {
        self.steps
            .iter()
            .filter(|s| s.seq > idle)
            .map(|s| s.until)
            .min()
    }

    /// Keep every step until at least `until`, while Neovim reports it is
    /// still busy scrolling (see `super::Animator::defer`).
    pub fn defer(&mut self, until: Instant) {
        for s in &mut self.steps {
            s.until = s.until.max(until);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::predict::{Rest, Want};
    use std::time::Duration;

    fn step(seq: u64, byte: usize, top: i64, hidden: bool, until: Instant) -> Step {
        Step {
            seq,
            pos: Pos {
                win: 1000,
                line: 0,
                byte,
                want: Want::Col(byte),
                top,
                left: 0,
                rest: Rest::Normal,
            },
            vcol: byte,
            proof: Proof::Move,
            credited: true,
            tick: 1,
            line_count: 10,
            echo: None,
            insstart: None,
            hidden,
            until,
        }
    }

    fn texts(s: &str) -> Vec<Text> {
        s.chars().map(Text::Char).collect()
    }

    #[test]
    fn a_line_still_agrees_with_what_neovim_draws_over_its_blanks() {
        let laid = Seen {
            text: texts("  foo"),
            whole: false,
        };
        assert!(laid.agrees(Some(&texts("  foo  "))));
        assert!(
            laid.agrees(Some(&texts("▏ foo x"))),
            "a guide, text past the end"
        );
        assert!(!laid.agrees(Some(&texts("  fox  "))));
        assert!(!laid.agrees(Some(&texts(" foo   "))), "moved");
        assert!(!laid.agrees(Some(&texts("  fo"))), "cut short");
        assert!(laid.agrees(None), "out of view");
        let whole = Seen {
            text: texts("  foo  "),
            whole: true,
        };
        assert!(
            !whole.agrees(Some(&texts("▏ foo  "))),
            "Neovim's own row, redrawn otherwise"
        );
    }

    #[test]
    fn the_newest_step_the_screen_shows_is_drawn() {
        let t = Instant::now();
        let mut c = Chain::new((1000, 0, 0), 0, None);
        assert_eq!(c.shown(0, 0), None);
        c.push(step(1, 1, 0, false, t));
        c.push(step(2, 2, 0, false, t));
        assert_eq!(c.shown(0, 0).map(|s| s.seq), Some(2));
        c.push(step(3, 3, 1, false, t));
        assert_eq!(c.shown(0, 0).map(|s| s.seq), Some(2), "another view");
        assert_eq!(c.shown(1, 0).map(|s| s.seq), Some(3));
        assert_eq!(c.head().map(|s| s.seq), Some(3));
        assert_eq!(c.step_for(2).map(|s| s.pos.byte), Some(2));
        assert_eq!(c.step_for(4), None);
    }

    #[test]
    fn hidden_steps_and_those_after_them_are_not_drawn() {
        let t = Instant::now();
        let mut c = Chain::new((1000, 0, 0), 0, None);
        c.push(step(1, 1, 0, false, t));
        c.push(step(2, 2, 0, true, t));
        c.push(step(3, 3, 0, false, t));
        assert_eq!(c.shown(0, 0).map(|s| s.seq), Some(1));
        let mut c = Chain::new((1000, 0, 0), 0, None);
        c.push(step(1, 1, 0, true, t));
        assert_eq!(c.shown(0, 0), None);
        assert_eq!(c.head().map(|s| s.seq), Some(1), "but moved on from");
    }

    #[test]
    fn steps_are_let_go_up_to_a_key() {
        let t = Instant::now();
        let mut c = Chain::new((1000, 0, 0), 0, None);
        for seq in 1..=4 {
            c.push(step(seq, seq as usize, 0, false, t));
        }
        c.drop_upto(2);
        assert_eq!(c.seqs(), [3, 4]);
        c.drop_upto(9);
        assert!(c.is_empty());
    }

    #[test]
    fn a_chain_lapses_at_its_deadline() {
        let t = Instant::now();
        let ms = Duration::from_millis;
        let mut c = Chain::new((1000, 0, 0), 0, None);
        assert_eq!(c.until(9), None);
        c.push(step(1, 1, 0, false, t + ms(500)));
        c.push(step(2, 2, 0, false, t + ms(530)));
        assert_eq!(c.until(9), Some(t + ms(500)));
        c.drop_upto(1);
        assert_eq!(c.until(9), Some(t + ms(530)));
    }

    #[test]
    fn steps_for_keys_neovim_has_not_taken_wait_for_it() {
        let t = Instant::now();
        let ms = Duration::from_millis;
        let mut c = Chain::new((1000, 0, 0), 0, None);
        c.push(step(1, 1, 0, false, t + ms(500)));
        c.push(step(2, 2, 0, false, t + ms(530)));
        assert_eq!((c.until(0), c.waiting(0)), (None, Some(t + ms(500))));
        assert_eq!(
            (c.until(1), c.waiting(1)),
            (Some(t + ms(500)), Some(t + ms(530)))
        );
        assert_eq!((c.until(2), c.waiting(2)), (Some(t + ms(500)), None));
    }
}
