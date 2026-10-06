//! A window that shows another buffer fading from one to the other:
//! animate.nvim's switch fade, `through` the window's background.
//!
//! Neovim does not tell a UI which buffer a window shows. The client's agent
//! in the editor does (see `crate::client::App`): a buffer entering a window
//! that showed another one is a switch — `:bnext`, `:edit`, `<C-^>`, a picker
//! opening a file — and the first batch after it is the one that draws it.
//! What the window showed is kept from just before that batch; then, over
//! `switch_ms`, eased in and out, the old text dims into the window's
//! background, and the new text comes up out of it.
//!
//! Only a window still where it was, the same size, fades, and only when a
//! few switch at once: `:windo bnext`, or a session loading, switches every
//! window, and fades none (animate.nvim's `max_burst`).

use super::layout::{background, fade_cell, percent, Easing};
use crate::client::compose::{Frame, Out};
use crate::client::grid::{Cell, Text};
use crate::client::model::{Model, Place};

/// More windows than this switching at once fade none of them.
pub const MAX_BURST: usize = 3;

/// A window about to show another buffer, as it was drawn.
#[derive(Debug)]
pub struct Was {
    grid: u64,
    rows: Vec<Vec<Cell>>,
    at: (i64, i64),
    size: (usize, usize),
}

impl Was {
    /// Split window `grid` as the model has it, if it is on show.
    pub fn take(model: &Model, grid: u64) -> Option<Was> {
        let p = model
            .layout
            .get(&grid)
            .filter(|p| !p.hidden && matches!(p.place, Place::Window { .. }))?;
        let g = model.grids.get(&grid)?;
        Some(Was {
            grid,
            rows: g.lines(0, g.height(), 0, g.width()),
            at: p.origin(),
            size: (g.width(), g.height()),
        })
    }

    /// Whether the window is where it was, the same size, on show.
    fn still(&self, model: &Model) -> bool {
        let placed = model.layout.get(&self.grid).is_some_and(|p| {
            !p.hidden && matches!(p.place, Place::Window { .. }) && p.origin() == self.at
        });
        let sized = model
            .grids
            .get(&self.grid)
            .is_some_and(|g| (g.width(), g.height()) == self.size);
        placed && sized
    }
}

/// One window's fade.
#[derive(Debug)]
pub struct Switch {
    was: Was,
    /// The window's background, as a highlight: what the old text fades into
    /// and the new one out of.
    bg: u32,
    elapsed: f32,
    duration: f32,
}

impl Switch {
    /// Fade from what `was` showed over `duration` seconds — if the window is
    /// still where it was.
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

    /// Whether its window is still where it was: one moved or resized since
    /// is not faded any more.
    pub fn still(&self, model: &Model) -> bool {
        self.was.still(model)
    }

    /// Move on `dt` seconds. Says whether it is still going.
    pub fn step(&mut self, dt: f32) -> bool {
        self.elapsed += dt;
        self.elapsed < self.duration
    }

    /// Draw the fade over the window as the compositor drew it: the old text
    /// dimmed for the first half, the new one coming up for the second.
    pub fn paint(&self, frame: &mut Frame, model: &Model) {
        let e = Easing::InOutQuad.at(self.elapsed / self.duration);
        let colors = model.colors();
        let to = colors.visual_bg(&model.style(self.bg));
        let (top, left) = self.was.at;
        if e < 0.5 {
            let keep = percent(1.0 - 2.0 * e);
            for (r, row) in self.was.rows.iter().enumerate() {
                for (c, cell) in row.iter().enumerate() {
                    let Some(o) = frame.at(top + r as i64, left + c as i64) else {
                        continue;
                    };
                    *o = Out {
                        text: cell.text.clone(),
                        style: model.style(cell.hl),
                        wide: row.get(c + 1).is_some_and(|n| n.text == Text::Half),
                    };
                    fade_cell(&colors, o, to, keep);
                }
            }
        } else {
            let keep = percent(2.0 * e - 1.0);
            let (w, h) = self.was.size;
            for r in 0..h as i64 {
                for c in 0..w as i64 {
                    if let Some(o) = frame.at(top + r, left + c) {
                        fade_cell(&colors, o, to, keep);
                    }
                }
            }
        }
    }
}
