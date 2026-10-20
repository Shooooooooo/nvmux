//! The cursor moved ahead of Neovim: see `crate::client::predict`.
//!
//! A key that moves the cursor along its line in Normal mode — `l`, `$`,
//! `^` and their kind — moves it on the screen at once, before Neovim has
//! heard of it, where the client can work out exactly where Neovim will
//! put it (see `crate::client::moves`). What the client keeps of each such
//! key is where it leaves the cursor in the buffer's terms — its line, its
//! byte, the column it wants, the view — and not a cell of the screen: a
//! view moved under it, ahead of Neovim or by Neovim, and a frame Neovim
//! draws part way through the keys typed, would leave a cell wrong, and
//! every key after it with it. The cursor is drawn where the newest of them
//! the screen's view agrees with puts it.
//!
//! Neovim has the last word on each: the agent says where each key it has
//! taken left the cursor, and the client judges what it worked out against
//! that, key by key — let go of once Neovim's frame for it has come, drawn
//! as it was, and all of them where Neovim did otherwise, or said nothing in
//! time. One worked out only to be judged, while what drew one wrong proves
//! itself again, is never drawn, nor any after it (see
//! `crate::client::predict::Trust`).
//!
//! A motion along a line changes nothing of what the window shows but the
//! cursor. So a frame of Neovim's that moves the view, or changes the text
//! of the row the line is drawn on — an edit the client has yet to hear of,
//! virtual text put between its characters — is one the steps were not
//! worked out over: the cursor drawn on a step in it could be a sight
//! Neovim never drew, and they are all let go of. What it draws on the
//! other rows — a plugin's virtual text, its signs, a diagnostic — leaves
//! them as they are.

use std::collections::VecDeque;
use std::time::Instant;

use crate::client::grid::Text;
use crate::client::predict::Pos;

/// How many keys the cursor may be moved ahead of Neovim in a window: more,
/// and Neovim is not keeping up, or not saying so.
pub const LONGEST: usize = 64;

/// Where the cursor is drawn ahead of Neovim in the window that has it, a
/// key at a time, oldest first: see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    /// The window, its gutter's width and the first display column Neovim
    /// shows it from, as the steps were worked out with them: a gutter
    /// wider or narrower puts every step a column out, and a view Neovim
    /// moved sideways is not the one they were drawn in.
    pub win: i64,
    pub textoff: usize,
    pub left: usize,
    /// The line the steps move the cursor along, and what Neovim had drawn
    /// of it when the first was worked out: the view's first line, and the
    /// text of the row the line was on.
    pub line: i64,
    pub drawn: Option<(i64, Vec<Text>)>,
    steps: VecDeque<Step>,
}

/// Where a key the client predicted leaves the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    /// The client's number for the key (see `crate::client::predict::Sync`).
    pub seq: u64,
    /// Where it leaves the cursor and the view, as the ack for it will say
    /// if Neovim makes it as the client did; and the display column the
    /// cursor is drawn in there.
    pub pos: Pos,
    pub vcol: usize,
    /// Whether it moved the cursor, or left it where it was: one that did is
    /// what proves a buffer (see `crate::client::predict::Trust`).
    pub moved: bool,
    /// The buffer it was worked out over, as it was: its `b:changedtick` and
    /// how many lines it had.
    pub tick: i64,
    pub line_count: i64,
    /// Worked out only to be judged: see the module docs.
    pub hidden: bool,
    /// When it is let go of, Neovim having said nothing of its key.
    pub until: Instant,
}

impl Chain {
    pub fn new(
        (win, textoff, left): (i64, usize, usize),
        line: i64,
        drawn: Option<(i64, Vec<Text>)>,
    ) -> Self {
        Self {
            win,
            textoff,
            left,
            line,
            drawn,
            steps: VecDeque::new(),
        }
    }

    pub fn push(&mut self, step: Step) {
        self.steps.push_back(step);
    }

    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// The newest step: what the next key moves the cursor on from.
    pub fn head(&self) -> Option<&Step> {
        self.steps.back()
    }

    /// The step drawn, the screen's view being from line `top` and column
    /// `left`: the newest of those before any hidden one that has the view
    /// there. One that has it elsewhere is not drawn, the screen not being
    /// what the step was worked out over; nor is one after a hidden one,
    /// which is worked out from it.
    pub fn shown(&self, top: i64, left: usize) -> Option<&Step> {
        self.steps
            .iter()
            .take_while(|s| !s.hidden)
            .filter(|s| (s.pos.top, s.pos.left) == (top, left))
            .last()
    }

    /// The step for the client's key `seq`, if it made one.
    pub fn step_for(&self, seq: u64) -> Option<&Step> {
        self.steps.iter().find(|s| s.seq == seq)
    }

    /// Let go of the steps for the keys up to `seq`, Neovim's frame for
    /// them having come.
    pub fn drop_upto(&mut self, seq: u64) {
        self.steps.retain(|s| s.seq > seq);
    }

    /// The client's numbers for the keys of every step.
    pub fn seqs(&self) -> Vec<u64> {
        self.steps.iter().map(|s| s.seq).collect()
    }

    /// When the oldest step is let go of, unconfirmed.
    pub fn until(&self) -> Option<Instant> {
        self.steps.iter().map(|s| s.until).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::predict::{Rest, Want};
    use std::time::Duration;

    /// A step for key `seq`, the cursor at byte `byte` of line 0 in a view
    /// from line `top`.
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
            moved: true,
            tick: 1,
            line_count: 10,
            hidden,
            until,
        }
    }

    /// The newest step whose view is the screen's is drawn: one worked out
    /// over another view is not.
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

    /// A hidden step is never drawn, nor any after it, worked out from it.
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

    /// Neovim's frame for a key lets go of its step and those before it.
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

    /// A chain lapses when its oldest step does.
    #[test]
    fn a_chain_lapses_at_its_deadline() {
        let t = Instant::now();
        let ms = Duration::from_millis;
        let mut c = Chain::new((1000, 0, 0), 0, None);
        assert_eq!(c.until(), None);
        c.push(step(1, 1, 0, false, t + ms(500)));
        c.push(step(2, 2, 0, false, t + ms(530)));
        assert_eq!(c.until(), Some(t + ms(500)));
        c.drop_upto(1);
        assert_eq!(c.until(), Some(t + ms(530)));
    }
}
