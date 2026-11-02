//! animate.nvim's switch fade: when a window shows another buffer, the old
//! text dims into the window's background and the new text comes up out of it.
//!
//! Neovim does not tell a UI which buffer a window shows; the client's agent in
//! the editor reports it (see `crate::client::App`). None fade when more than
//! [`MAX_BURST`] switch at once (`:windo bnext`, a session loading).
//!
//! The fade covers the window's whole place, so the new text may be centred
//! at another width than the old (see `crate::client::wide`): the old dims
//! where it was and the new comes up where it goes, never sliding between.
//! A window Neovim has yet to draw wider, a round trip after the switch,
//! stays blank until it has ([`Switch::step`]).

use super::layout::{background, fade_cell, percent, Easing};
use crate::client::compose::{Frame, Out};
use crate::client::grid::{Cell, Text};
use crate::client::model::{Held, Model, Place};

/// animate.nvim's `max_burst`.
pub const MAX_BURST: usize = 3;

/// The most a fade waits blank for its window to be drawn wider, in seconds:
/// a slow link's round trip.
pub const WAIT: f32 = 0.5;

/// A window's cells from just before the batch that draws its switch.
#[derive(Debug)]
pub struct Was {
    grid: u64,
    rows: Vec<Vec<Cell>>,
    /// Where `rows` were drawn.
    at: (i64, i64),
    /// The window's place: row, column, width, height.
    place: (usize, usize, usize, usize),
}

impl Was {
    /// `None` unless `grid` is a visible split window, its text drawn `shift`
    /// columns from where it goes (see `super::Animator::insets`).
    pub fn take(model: &Model, grid: u64, shift: i64) -> Option<Was> {
        model
            .layout
            .get(&grid)
            .filter(|p| !p.hidden && matches!(p.place, Place::Window { .. }))?;
        let g = model.grids.get(&grid)?;
        let (row, col) = model.drawn_origin(grid);
        Some(Was {
            grid,
            rows: g.lines(0, g.height(), 0, g.width()),
            at: (row, col + shift),
            place: model.window_cells(grid)?,
        })
    }

    /// Whether the window is where it was, the same size, on show, however
    /// wide its text is now.
    fn still(&self, model: &Model) -> bool {
        let shown = model
            .layout
            .get(&self.grid)
            .is_some_and(|p| !p.hidden && matches!(p.place, Place::Window { .. }));
        shown && model.window_cells(self.grid) == Some(self.place)
    }

    /// The old cell drawn at screen cell `at`, if any.
    fn out(&self, model: &Model, at: (i64, i64)) -> Option<Out> {
        let r = usize::try_from(at.0 - self.at.0).ok()?;
        let c = usize::try_from(at.1 - self.at.1).ok()?;
        let row = self.rows.get(r)?;
        let cell = row.get(c)?;
        Some(Out {
            text: cell.text.clone(),
            style: model.style(cell.hl),
            wide: row.get(c + 1).is_some_and(|n| n.text == Text::Half),
        })
    }
}

#[derive(Debug)]
pub struct Switch {
    was: Was,
    /// The window's background highlight, which the fade passes through.
    bg: u32,
    elapsed: f32,
    duration: f32,
    /// How long it has waited at its midpoint.
    waited: f32,
}

impl Switch {
    /// `None` if `duration` is not positive or the window moved, resized or
    /// hid.
    pub fn start(was: Was, model: &Model, duration: f32) -> Option<Switch> {
        if duration <= 0.0 || !was.still(model) {
            return None;
        }
        let g = model.grids.get(&was.grid)?;
        let bg = background((0..g.height()).flat_map(|r| g.row(r)));
        Some(Switch {
            was,
            bg,
            elapsed: 0.0,
            duration,
            waited: 0.0,
        })
    }

    pub fn grid(&self) -> u64 {
        self.was.grid
    }

    /// Keeps the fade's highlights from being reclaimed (see
    /// [`crate::client::model::Model::reclaim`]).
    pub fn hold(&self, held: &mut Held) {
        for row in &self.was.rows {
            held.cells(row);
        }
        held.mark(self.bg);
    }

    pub fn still(&self, model: &Model) -> bool {
        self.was.still(model)
    }

    /// Returns whether the fade is still running. While `wait`, it stops at
    /// its midpoint, the window blank, for up to [`WAIT`].
    pub fn step(&mut self, dt: f32, wait: bool) -> bool {
        let half = self.duration / 2.0;
        let mut next = self.elapsed + dt;
        if wait && self.elapsed <= half && next > half {
            let over = next - half;
            let held = over.min(WAIT - self.waited).max(0.0);
            self.waited += held;
            next = half + (over - held);
        }
        self.elapsed = next;
        self.elapsed < self.duration
    }

    /// Whether nothing of the window's new text shows yet.
    pub fn hiding(&self) -> bool {
        self.elapsed * 2.0 <= self.duration
    }

    fn eased(&self) -> f32 {
        Easing::InOutQuad.at(self.elapsed / self.duration)
    }

    /// Old text dims for the first half, where it was drawn, with nothing
    /// beside it; the whole window comes up for the second.
    pub fn paint(&self, frame: &mut Frame, model: &Model, cursor: (i64, i64)) {
        let e = self.eased();
        let old = e < 0.5;
        let keep = percent(if old { 1.0 - 2.0 * e } else { 2.0 * e - 1.0 });
        let colors = model.colors();
        let to = colors.visual_bg(&model.style(self.bg));
        // Past the old grid: a margin beside centred text, blank as composed.
        let blank = Out::blank(model.style(0));
        let (row, col, width, height) = self.was.place;
        for r in row..row + height {
            for c in col..col + width {
                let at = (r as i64, c as i64);
                let Some(o) = frame.at(at.0, at.1) else {
                    continue;
                };
                if old {
                    *o = self.was.out(model, at).unwrap_or_else(|| blank.clone());
                }
                fade_cell(&colors, o, to, keep, at == cursor);
            }
        }
    }
}
