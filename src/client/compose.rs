//! One frame of the screen, put together from the grids.
//!
//! Grid 1 first — the tabline, the status lines, the separators, and with
//! `ext_multigrid` off everything else too — then each window in the layout at
//! its place, then the floats and the message area over them in the order
//! Neovim composes them (`zindex`, then `compindex`), mixed with what is under
//! them where their highlight blends.
//!
//! Where a grid is drawn is where an animation says it is this frame
//! ([`View::origin`]), and a window in the middle of scrolling shows the lines
//! the scroll has reached rather than its own ([`View::scroll`]): the
//! compositor knows that things move, and nothing of how. What an animation
//! draws of its own over the windows — split windows between two layouts, a
//! buffer fading into another — goes on before the floats do
//! ([`View::over_windows`]).
//!
//! # Wide characters
//!
//! A character two cells wide is drawn from its left cell, and the right one
//! draws nothing. Anything drawn over one half of such a pair — a float's
//! edge, the cursor's trail, a spark — leaves the other half meaning nothing,
//! so [`Frame::mend`] turns that half into a blank in its own colours, which is
//! what Neovim's compositor does too. It runs after every overlay.

use super::grid::{Cell, Text};
use super::model::{Model, Place};
use super::style::{self, Color, Style};
use crate::palette::Rgb;

/// One cell of a frame: what the terminal is to show there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Out {
    pub text: Text,
    pub style: Style,
    /// The text is two cells wide: the next cell is its right half.
    pub wide: bool,
}

impl Out {
    pub fn blank(style: Style) -> Self {
        Self {
            text: Text::Char(' '),
            style,
            wide: false,
        }
    }
}

/// A whole screen of cells, row-major.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub cells: Vec<Out>,
}

impl Frame {
    pub fn new(width: usize, height: usize, blank: Style) -> Self {
        Self {
            width,
            height,
            cells: vec![Out::blank(blank); width * height],
        }
    }

    pub fn get(&self, row: usize, col: usize) -> Option<&Out> {
        (row < self.height && col < self.width).then(|| &self.cells[row * self.width + col])
    }

    pub fn get_mut(&mut self, row: usize, col: usize) -> Option<&mut Out> {
        (row < self.height && col < self.width).then(|| &mut self.cells[row * self.width + col])
    }

    /// A cell at a position that may be off the screen.
    pub fn at(&mut self, row: i64, col: i64) -> Option<&mut Out> {
        let row = usize::try_from(row).ok()?;
        let col = usize::try_from(col).ok()?;
        self.get_mut(row, col)
    }

    /// Make every wide character whole again or blank: see the module docs.
    pub fn mend(&mut self) {
        for r in 0..self.height {
            let row = &mut self.cells[r * self.width..(r + 1) * self.width];
            for c in 0..row.len() {
                let has_right = row.get(c + 1).is_some_and(|n| n.text == Text::Half);
                let has_left = c > 0 && row[c - 1].wide;
                let cell = &mut row[c];
                if cell.wide && !has_right {
                    cell.text = Text::Char(' ');
                    cell.wide = false;
                } else if cell.text == Text::Half && !has_left {
                    cell.text = Text::Char(' ');
                }
            }
        }
    }

    /// The frame as text, a row to a line, for the tests.
    #[cfg(test)]
    pub fn text(&self) -> String {
        (0..self.height)
            .map(|r| {
                let mut line = Vec::new();
                for cell in &self.cells[r * self.width..(r + 1) * self.width] {
                    cell.text.push_to(&mut line);
                }
                String::from_utf8(line).expect("utf-8")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// What the animations say about where things are this frame.
pub trait View {
    /// Where grid `grid` is drawn, given where the editor put it.
    fn origin(&self, grid: u64, placed: (i64, i64)) -> (i64, i64);

    /// The scroll in progress on grid `grid`, if one is.
    fn scroll(&self, grid: u64) -> Option<&dyn Scrolling>;

    /// Draw over the windows, once they are all drawn and before the floats
    /// are: split windows on their way from one layout to another, a window
    /// fading from one buffer to the next.
    fn over_windows(&self, _frame: &mut Frame, _model: &Model) {}
}

/// A scroll part way through, as the compositor asks after it.
pub trait Scrolling {
    /// The rows and columns that scroll: `top..bot` × `left..right`.
    fn rect(&self) -> (usize, usize, usize, usize);

    /// What row `r` of the rectangle shows now, counted from its top: a line
    /// of the rectangle's width, or `None` for a row the scroll has nothing
    /// for, which is drawn blank.
    fn row(&self, r: usize) -> Option<&[Cell]>;
}

/// Nothing moving: everything where the editor put it.
pub struct Still;

impl View for Still {
    fn origin(&self, _grid: u64, placed: (i64, i64)) -> (i64, i64) {
        placed
    }

    fn scroll(&self, _grid: u64) -> Option<&dyn Scrolling> {
        None
    }
}

/// Put the frame together. See the module docs.
pub fn compose(model: &Model, view: &dyn View, width: usize, height: usize) -> Frame {
    let mut frame = Frame::new(width, height, model.style(0));
    draw(&mut frame, model, view, 1, (0, 0), None, false);

    // Windows next: they never overlap one another, so their order is
    // nobody's business. Under each, grid 1 has whatever Neovim drew there
    // before the window was — nothing meant to be seen, and what a window
    // sliding away from its place would uncover — so it is blanked first.
    let blank = Out::blank(model.style(0));
    for (&grid, p) in &model.layout {
        let (Place::Window { row, col }, Some(g), false) =
            (&p.place, model.grids.get(&grid), p.hidden || grid == 1)
        else {
            continue;
        };
        for r in 0..g.height() {
            for c in 0..g.width() {
                if let Some(cell) = frame.at((row + r) as i64, (col + c) as i64) {
                    *cell = blank.clone();
                }
            }
        }
    }
    for (&grid, p) in &model.layout {
        if p.hidden || grid == 1 {
            continue;
        }
        if let Place::Window { .. } = p.place {
            let at = view.origin(grid, p.origin());
            draw(&mut frame, model, view, grid, at, None, false);
        }
    }
    view.over_windows(&mut frame, model);

    let mut layers: Vec<_> = model
        .layout
        .iter()
        .filter(|(g, p)| !p.hidden && **g != 1)
        .filter_map(|(g, p)| p.layer().map(|z| (z, *g, p)))
        .collect();
    layers.sort_by_key(|(z, g, _)| (*z, *g));
    for (_, grid, p) in layers {
        let at = view.origin(grid, p.origin());
        match &p.place {
            Place::Float { .. } => draw(&mut frame, model, view, grid, at, None, true),
            Place::Message { scrolled, sep, .. } => {
                // The message area covers the screen from its row down, and
                // no further whatever its grid's height.
                let rows = (height as i64 - at.0).max(0) as usize;
                if *scrolled && at.0 > 0 {
                    separator(&mut frame, model, at.0 - 1, sep);
                }
                draw(&mut frame, model, view, grid, at, Some(rows), true);
            }
            Place::Window { .. } => {}
        }
    }

    frame.mend();
    frame
}

/// Draw grid `grid` with its top left at `at`, its first `rows` rows if that
/// is given, mixing it into what is under it where `blends` and its
/// highlights say to.
fn draw(
    frame: &mut Frame,
    model: &Model,
    view: &dyn View,
    grid: u64,
    at: (i64, i64),
    rows: Option<usize>,
    blends: bool,
) {
    let Some(g) = model.grids.get(&grid) else {
        return;
    };
    let colors = model.colors();
    let scroll = view.scroll(grid);
    let blank = Cell::default();
    let height = rows.map_or(g.height(), |n| n.min(g.height()));
    for r in 0..height {
        let y = at.0 + r as i64;
        if y < 0 || y >= frame.height as i64 {
            continue;
        }
        let own = g.row(r);
        // Inside a scroll's rectangle the row is the scroll's; outside it, the
        // grid's own.
        let scrolled = scroll.and_then(|s| {
            let (top, bot, left, right) = s.rect();
            (top..bot)
                .contains(&r)
                .then(|| (left, right, s.row(r - top)))
        });
        for c in 0..g.width() {
            let x = at.1 + c as i64;
            if x < 0 || x >= frame.width as i64 {
                continue;
            }
            let (cell, wide) = match scrolled {
                Some((left, right, line)) if (left..right).contains(&c) => {
                    let i = c - left;
                    let line = line.unwrap_or(&[]);
                    let cell = line.get(i).unwrap_or(&blank);
                    let wide = line.get(i + 1).is_some_and(|n| n.text == Text::Half);
                    (cell, wide)
                }
                _ => (
                    &own[c],
                    own.get(c + 1).is_some_and(|n| n.text == Text::Half),
                ),
            };
            let style = model.style(cell.hl);
            let blend = if blends { model.blend(cell.hl) } else { 0 };
            let Some(under) = frame.at(y, x) else {
                continue;
            };
            if blend == 0 {
                *under = Out {
                    text: cell.text.clone(),
                    style,
                    wide,
                };
                continue;
            }
            // A blank lets what is under it through — unless what is under
            // it is the right half of something wide, which has nothing of
            // its own to show.
            if cell.text.is_blank() && under.text != Text::Half {
                under.style = style::blend_through(&colors, &under.style, &style, blend);
            } else {
                under.style = style::blend_over(&colors, &under.style, &style, blend);
                under.text = cell.text.clone();
                under.wide = wide;
            }
        }
    }
}

/// The row above messages that have pushed up over the windows: `sep` across
/// the whole width, in `MsgSeparator`.
fn separator(frame: &mut Frame, model: &Model, row: i64, sep: &str) {
    let style = model
        .groups
        .get("MsgSeparator")
        .map_or_else(|| model.style(0), |id| model.style(*id));
    let text = Text::new(sep);
    let text = if text == Text::Half {
        Text::Char(' ')
    } else {
        text
    };
    for c in 0..frame.width as i64 {
        if let Some(cell) = frame.at(row, c) {
            *cell = Out {
                text: text.clone(),
                style,
                wide: false,
            };
        }
    }
}

/// A style moved towards `to`, keeping `keep` percent of its colours: at 0,
/// text and all are `to`.
pub fn fade_to(colors: &style::Colors, s: &mut Style, to: Rgb, keep: u8) {
    let fg = colors.visual_fg(s);
    let bg = colors.visual_bg(s);
    if s.sp != Color::Default {
        s.sp = Color::Rgb(style::mix(keep, colors.visual_sp(s), to));
    }
    s.fg = Color::Rgb(style::mix(keep, fg, to));
    s.bg = Color::Rgb(style::mix(keep, bg, to));
    s.reverse = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::model::Changes;
    use crate::client::redraw::{Event, LineCell};
    use crate::client::style::Attrs;
    use crate::palette::Palette;

    fn model(events: Vec<Event>) -> Model {
        let mut m = Model::new(Palette {
            fg: Rgb(255, 255, 255),
            bg: Rgb(0, 0, 0),
            ansi: [Rgb(0, 0, 0); 16],
        });
        let mut changes = Changes::default();
        m.apply(
            Event::OptionSet {
                name: "termguicolors".into(),
                value: crate::client::redraw::OptionValue::Bool(true),
            },
            &mut changes,
        );
        for e in events {
            m.apply(e, &mut changes);
        }
        m.refresh_styles();
        m
    }

    fn resize(grid: u64, width: usize, height: usize) -> Event {
        Event::GridResize {
            grid,
            width,
            height,
        }
    }

    fn line(grid: u64, row: usize, text: &str, hl: u32) -> Event {
        Event::GridLine {
            grid,
            row,
            col: 0,
            cells: text
                .chars()
                .map(|c| LineCell {
                    text: Text::new(&c.to_string()),
                    hl: Some(hl),
                    repeat: 1,
                })
                .collect(),
        }
    }

    fn float(grid: u64, row: i64, col: i64, zindex: i64) -> Event {
        Event::WinFloatPos {
            grid,
            win: Some(1000 + grid as i64),
            mouse: true,
            zindex,
            compindex: 0,
            row,
            col,
        }
    }

    /// Grid 1, a window at its place, and a float over both.
    #[test]
    fn grids_are_stacked_where_the_editor_put_them() {
        let m = model(vec![
            resize(1, 6, 3),
            line(1, 2, "status", 0),
            resize(2, 6, 2),
            line(2, 0, "aaaaaa", 0),
            line(2, 1, "bbbbbb", 0),
            Event::WinPos {
                grid: 2,
                win: Some(1000),
                row: 0,
                col: 0,
                width: 6,
                height: 2,
            },
            resize(4, 2, 1),
            line(4, 0, "FF", 0),
            float(4, 1, 2, 50),
        ]);
        let f = compose(&m, &Still, 6, 3);
        assert_eq!(f.text(), "aaaaaa\nbbFFbb\nstatus");
    }

    /// Higher zindex on top, whatever order they were placed in.
    #[test]
    fn floats_stack_by_zindex() {
        let m = model(vec![
            resize(1, 4, 1),
            resize(4, 2, 1),
            line(4, 0, "hi", 0),
            float(4, 0, 0, 100),
            resize(5, 4, 1),
            line(5, 0, "lowe", 0),
            float(5, 0, 0, 50),
        ]);
        assert_eq!(compose(&m, &Still, 4, 1).text(), "hiwe");
    }

    /// A hidden window is not drawn; a message area that scrolled up draws
    /// its separator above it and covers the screen from there down.
    #[test]
    fn hidden_windows_and_scrolled_messages() {
        let m = model(vec![
            resize(1, 3, 4),
            resize(2, 3, 4),
            line(2, 0, "ooo", 0),
            Event::WinPos {
                grid: 2,
                win: Some(1000),
                row: 0,
                col: 0,
                width: 3,
                height: 4,
            },
            Event::WinHide { grid: 2 },
            resize(3, 3, 4),
            line(3, 0, "m1 ", 0),
            line(3, 1, "m2 ", 0),
            line(3, 2, "zzz", 0),
            Event::MsgSetPos {
                grid: 3,
                row: 2,
                scrolled: true,
                sep: "-".into(),
                zindex: 200,
                compindex: 1,
            },
        ]);
        assert_eq!(compose(&m, &Still, 3, 4).text(), "   \n---\nm1 \nm2 ");
    }

    /// A blank cell of a blended float lets the text under it show; a
    /// character of its own replaces it.
    #[test]
    fn a_blended_float_shows_through_its_blanks() {
        let m = model(vec![
            Event::HlAttr {
                id: 1,
                rgb: Attrs {
                    bg: Some(0x0000ff),
                    blend: 50,
                    ..Attrs::default()
                },
                cterm: Attrs::default(),
            },
            resize(1, 3, 1),
            line(1, 0, "abc", 0),
            resize(4, 3, 1),
            line(4, 0, " X ", 1),
            float(4, 0, 0, 50),
        ]);
        let f = compose(&m, &Still, 3, 1);
        assert_eq!(f.text(), "aXc");
        // Half way between the terminal's black and the float's blue.
        assert_eq!(f.cells[0].style.bg, Color::Rgb(Rgb(0, 0, 127)));
    }

    /// A float drawn over the right half of a wide character leaves its left
    /// half as a blank.
    #[test]
    fn a_wide_character_cut_by_a_float_is_mended() {
        let m = model(vec![
            resize(1, 4, 1),
            Event::GridLine {
                grid: 1,
                row: 0,
                col: 0,
                cells: vec![
                    LineCell {
                        text: Text::Char('中'),
                        hl: Some(0),
                        repeat: 1,
                    },
                    LineCell {
                        text: Text::Half,
                        hl: None,
                        repeat: 1,
                    },
                    LineCell {
                        text: Text::Char('x'),
                        hl: None,
                        repeat: 2,
                    },
                ],
            },
            resize(4, 1, 1),
            line(4, 0, "F", 0),
            float(4, 0, 1, 50),
        ]);
        let f = compose(&m, &Still, 4, 1);
        assert_eq!(f.text(), " Fxx");
        assert!(!f.cells[0].wide);
    }

    /// What grid 1 has under a window is never seen, not even when the
    /// window is drawn somewhere else for a frame.
    #[test]
    fn what_grid_1_has_under_a_window_stays_hidden() {
        struct Away;
        impl View for Away {
            fn origin(&self, _grid: u64, placed: (i64, i64)) -> (i64, i64) {
                (placed.0, placed.1 + 2)
            }
            fn scroll(&self, _grid: u64) -> Option<&dyn Scrolling> {
                None
            }
        }
        let m = model(vec![
            resize(1, 4, 1),
            line(1, 0, "|old", 0),
            resize(2, 2, 1),
            line(2, 0, "ww", 0),
            Event::WinPos {
                grid: 2,
                win: Some(1000),
                row: 0,
                col: 0,
                width: 2,
                height: 1,
            },
        ]);
        assert_eq!(compose(&m, &Still, 4, 1).text(), "wwld");
        assert_eq!(compose(&m, &Away, 4, 1).text(), "  ww");
    }

    /// Where an animation says a grid is this frame is where it is drawn.
    #[test]
    fn a_grid_is_drawn_where_the_view_says() {
        struct Shifted;
        impl View for Shifted {
            fn origin(&self, grid: u64, placed: (i64, i64)) -> (i64, i64) {
                if grid == 4 {
                    (placed.0, placed.1 + 1)
                } else {
                    placed
                }
            }
            fn scroll(&self, _grid: u64) -> Option<&dyn Scrolling> {
                None
            }
        }
        let m = model(vec![
            resize(1, 3, 1),
            resize(4, 1, 1),
            line(4, 0, "F", 0),
            float(4, 0, 0, 50),
        ]);
        assert_eq!(compose(&m, &Shifted, 3, 1).text(), " F ");
    }
}
