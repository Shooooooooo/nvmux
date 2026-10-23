//! animate.nvim's switch fade: when a window shows another buffer, the old
//! text dims into the window's background and the new text comes up out of it.
//!
//! Neovim does not tell a UI which buffer a window shows; the client's agent in
//! the editor reports it (see `crate::client::App`). None fade when more than
//! [`MAX_BURST`] switch at once (`:windo bnext`, a session loading).

use super::layout::{background, fade_cell, percent, Easing};
use crate::client::compose::{Frame, Out};
use crate::client::grid::{Cell, Text};
use crate::client::model::{Held, Model, Place};

/// animate.nvim's `max_burst`.
pub const MAX_BURST: usize = 3;

#[derive(Debug)]
pub struct Was {
    grid: u64,
    rows: Vec<Vec<Cell>>,
    at: (i64, i64),
    size: (usize, usize),
}

impl Was {
    /// `None` unless `grid` is a visible split window.
    pub fn take(model: &Model, grid: u64) -> Option<Was> {
        model
            .layout
            .get(&grid)
            .filter(|p| !p.hidden && matches!(p.place, Place::Window { .. }))?;
        let g = model.grids.get(&grid)?;
        Some(Was {
            grid,
            rows: g.lines(0, g.height(), 0, g.width()),
            at: model.drawn_origin(grid),
            size: (g.width(), g.height()),
        })
    }

    fn still(&self, model: &Model) -> bool {
        let placed = model.layout.get(&self.grid).is_some_and(|p| {
            !p.hidden
                && matches!(p.place, Place::Window { .. })
                && model.drawn_origin(self.grid) == self.at
        });
        let sized = model
            .grids
            .get(&self.grid)
            .is_some_and(|g| (g.width(), g.height()) == self.size);
        placed && sized
    }
}

#[derive(Debug)]
pub struct Switch {
    was: Was,
    /// The window's background highlight, which the fade passes through.
    bg: u32,
    elapsed: f32,
    duration: f32,
}

impl Switch {
    /// `None` if `duration` is not positive or the window has moved.
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

    /// Returns whether the fade is still running.
    pub fn step(&mut self, dt: f32) -> bool {
        self.elapsed += dt;
        self.elapsed < self.duration
    }

    /// Old text dims for the first half, new text comes up for the second.
    pub fn paint(&self, frame: &mut Frame, model: &Model, cursor: (i64, i64)) {
        let e = Easing::InOutQuad.at(self.elapsed / self.duration);
        let colors = model.colors();
        let to = colors.visual_bg(&model.style(self.bg));
        let (top, left) = self.was.at;
        if e < 0.5 {
            let keep = percent(1.0 - 2.0 * e);
            for (r, row) in self.was.rows.iter().enumerate() {
                for (c, cell) in row.iter().enumerate() {
                    let at = (top + r as i64, left + c as i64);
                    let Some(o) = frame.at(at.0, at.1) else {
                        continue;
                    };
                    *o = Out {
                        text: cell.text.clone(),
                        style: model.style(cell.hl),
                        wide: row.get(c + 1).is_some_and(|n| n.text == Text::Half),
                    };
                    fade_cell(&colors, o, to, keep, at == cursor);
                }
            }
        } else {
            let keep = percent(2.0 * e - 1.0);
            let (w, h) = self.was.size;
            for r in 0..h as i64 {
                for c in 0..w as i64 {
                    let at = (top + r, left + c);
                    if let Some(o) = frame.at(at.0, at.1) {
                        fade_cell(&colors, o, to, keep, at == cursor);
                    }
                }
            }
        }
    }
}
