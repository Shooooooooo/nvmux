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

    /// Made [`Frame::new`] again, `width` by `height` and every cell blank in
    /// `blank`, in the buffer this one has: a frame a screen big is a few
    /// hundred kilobytes, asked of the allocator — and faulted in — afresh
    /// sixty times a second otherwise.
    pub fn reset(&mut self, width: usize, height: usize, blank: Style) {
        let blank = Out::blank(blank);
        let n = width * height;
        self.width = width;
        self.height = height;
        self.cells.truncate(n);
        for cell in &mut self.cells {
            cell.clone_from(&blank);
        }
        self.cells.resize(n, blank);
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

    /// How far grid `grid`'s text is drawn from where it goes in its window,
    /// in columns, on its way there: moved in or out, its grid made narrower
    /// or wider with the window where it was (see `super::wide`).
    fn shift(&self, _grid: u64) -> i64 {
        0
    }

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
    let mut frame = Frame::new(0, 0, Style::default());
    compose_into(&mut frame, model, view, width, height, true);
    frame
}

/// [`compose`], in the buffer `frame` already has, and mended only where
/// `mend` says: a caller that paints over it before it goes out, mending it
/// after, mends it once (see [`Frame::mend`]) — before, too, only where what
/// it paints reads the cells under it.
pub fn compose_into(
    frame: &mut Frame,
    model: &Model,
    view: &dyn View,
    width: usize,
    height: usize,
    mend: bool,
) {
    frame.reset(width, height, model.style(0));
    // The windows' places, where grid 1 has whatever Neovim drew there
    // before the window was — nothing meant to be seen, and what a window
    // sliding away from its place would uncover. Blank, as the frame starts:
    // grid 1 is not drawn there.
    let places: Vec<_> = model
        .layout
        .iter()
        .filter(|(g, p)| !p.hidden && **g != 1)
        .filter_map(|(g, _)| model.window_cells(*g))
        .collect();
    draw(
        frame,
        model,
        view,
        1,
        (0, 0),
        Within::default(),
        false,
        &places,
    );

    // A window's status line stands over its text, where that is drawn
    // narrower than the window (see `super::wide`). It is grid 1's, under
    // the window, and never in a window's place; but should one be laid over
    // one, the place is blanked again, as the whole of it always was.
    if status_lines_over_text(frame, model, view) {
        let blank = Out::blank(model.style(0));
        for &(row, col, width, height) in &places {
            for r in 0..height {
                for c in 0..width {
                    if let Some(cell) = frame.at((row + r) as i64, (col + c) as i64) {
                        *cell = blank.clone();
                    }
                }
            }
        }
    }

    // Windows next: they never overlap one another, so their order is
    // nobody's business. Each fills its place, of which a grid narrower than
    // it fills the middle.
    for (&grid, p) in &model.layout {
        if p.hidden || grid == 1 {
            continue;
        }
        if let Place::Window { .. } = p.place {
            let at = view.origin(grid, model.drawn_origin(grid));
            // Its text on its way across the window, should it be, and kept
            // to the window as it goes.
            let shift = view.shift(grid);
            let within = Within {
                cols: model
                    .window_cells(grid)
                    .filter(|_| shift != 0)
                    .map(|(_, _, width, _)| {
                        let left = at.1 - model.inset(grid) as i64;
                        (left, left + width as i64)
                    }),
                ..Within::default()
            };
            draw(
                frame,
                model,
                view,
                grid,
                (at.0, at.1 + shift),
                within,
                false,
                &[],
            );
        }
    }
    view.over_windows(frame, model);

    let mut layers: Vec<_> = model
        .layout
        .iter()
        .filter(|(g, p)| !p.hidden && **g != 1)
        .filter_map(|(g, p)| p.layer().map(|z| (z, *g, p)))
        .collect();
    layers.sort_by_key(|(z, g, _)| (*z, *g));
    for (_, grid, p) in layers {
        let at = view.origin(grid, model.drawn_origin(grid));
        match &p.place {
            // One anchored to a window goes with the window's text.
            Place::Float { .. } => {
                let at = (at.0, at.1 + view.shift(grid));
                draw(frame, model, view, grid, at, Within::default(), true, &[]);
            }
            Place::Message { scrolled, sep, .. } => {
                // The message area covers the screen from its row down, and
                // no further whatever its grid's height.
                let rows = (height as i64 - at.0).max(0) as usize;
                if *scrolled && at.0 > 0 {
                    separator(frame, model, at.0 - 1, sep);
                }
                let within = Within {
                    rows: Some(rows),
                    ..Within::default()
                };
                draw(frame, model, view, grid, at, within, true, &[]);
            }
            Place::Window { .. } => {}
        }
    }

    if mend {
        frame.mend();
    }
}

/// How much of a grid [`draw`] puts on the frame: rows from its top, and the
/// columns of the frame it may cover — all of either where not said.
#[derive(Debug, Clone, Copy, Default)]
struct Within {
    rows: Option<usize>,
    cols: Option<(i64, i64)>,
}

/// Draw grid `grid` with its top left at `at`, as much of it as `within`
/// says and none of it on the cells of `skip` — rectangles of the frame, its
/// top row, first column, and how many of each — mixing it into what is
/// under it where `blends` and its highlights say to.
///
/// Row by row: the columns of a row that land on the frame are worked out
/// once for it, and the colours of a highlight once a run of it — a frame is
/// tens of thousands of cells, and redrawn whole for a cursor that moved.
#[allow(clippy::too_many_arguments)]
fn draw(
    frame: &mut Frame,
    model: &Model,
    view: &dyn View,
    grid: u64,
    at: (i64, i64),
    within: Within,
    blends: bool,
    skip: &[(usize, usize, usize, usize)],
) {
    let Some(g) = model.grids.get(&grid) else {
        return;
    };
    let colors = model.colors();
    let scroll = view.scroll(grid);
    let blank = Cell::default();
    let height = within.rows.map_or(g.height(), |n| n.min(g.height()));
    // The grid's columns on the frame, and within what it is kept to.
    let width = frame.width as i64;
    let (mut lo, mut hi) = ((-at.1).max(0), (g.width() as i64).min(width - at.1));
    if let Some((from, to)) = within.cols {
        lo = lo.max(from - at.1);
        hi = hi.min(to - at.1);
    }
    if lo >= hi {
        return;
    }
    // The last highlight resolved, and its style and blend.
    let mut last: Option<(u32, Style, u8)> = None;
    let (mut runs, mut cut) = (Vec::with_capacity(4), Vec::with_capacity(4));
    for r in 0..height {
        let y = at.0 + r as i64;
        if y < 0 || y >= frame.height as i64 {
            continue;
        }
        let y = y as usize;
        let own = g.row(r);
        // Inside a scroll's rectangle the row is the scroll's; outside it, the
        // grid's own.
        let scrolled = scroll.and_then(|s| {
            let (top, bot, left, right) = s.rect();
            (top..bot)
                .contains(&r)
                .then(|| (left, right, s.row(r - top)))
        });
        // The grid's columns that land on this row, less those of the
        // rectangles skipped across it.
        runs.clear();
        runs.push((lo, hi));
        for &(top, left, cols, rows) in skip {
            if y < top || y >= top + rows {
                continue;
            }
            let (a, b) = (left as i64 - at.1, (left + cols) as i64 - at.1);
            cut.clear();
            for &(s, e) in &runs {
                if b <= s || a >= e {
                    cut.push((s, e));
                    continue;
                }
                if s < a {
                    cut.push((s, a));
                }
                if b < e {
                    cut.push((b, e));
                }
            }
            std::mem::swap(&mut runs, &mut cut);
        }
        let row = &mut frame.cells[y * frame.width..(y + 1) * frame.width];
        for &(s, e) in &runs {
            for c in s as usize..e as usize {
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
                let (style, blend) = match last {
                    Some((hl, style, blend)) if hl == cell.hl => (style, blend),
                    _ => {
                        let style = model.style(cell.hl);
                        let blend = if blends { model.blend(cell.hl) } else { 0 };
                        last = Some((cell.hl, style, blend));
                        (style, blend)
                    }
                };
                let under = &mut row[(at.1 + c as i64) as usize];
                if blend == 0 {
                    under.text.clone_from(&cell.text);
                    under.style = style;
                    under.wide = wide;
                    continue;
                }
                // A blank lets what is under it through — unless what is
                // under it is the right half of something wide, which has
                // nothing of its own to show.
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
}

/// Each window's status line, where its grid is drawn narrower than the
/// window, closed up to stand over its text (see [`super::wide::realign`]):
/// the windows that have one of their own, as `'laststatus'` says — every
/// window at 2; at 1 every window where there are two or more; and at 0 the
/// ones with another window under them. None where it is 3, one line across
/// the whole screen, or where it is not known. Says whether it laid any.
fn status_lines_over_text(frame: &mut Frame, model: &Model, view: &dyn View) -> bool {
    let Some(laststatus) = model.laststatus.filter(|l| *l != 3) else {
        return false;
    };
    let mut laid = false;
    let windows: Vec<(u64, (usize, usize, usize, usize))> = model
        .layout
        .iter()
        .filter(|(g, p)| **g != 1 && !p.hidden && matches!(p.place, Place::Window { .. }))
        .filter_map(|(g, _)| Some((*g, model.window_cells(*g)?)))
        .collect();
    let bottom = windows
        .iter()
        .map(|(_, (row, _, _, height))| row + height)
        .max()
        .unwrap_or(0);
    for &(grid, (row, col, width, height)) in &windows {
        // Where its text is drawn, on its way there or not.
        let inset = model.inset(grid) as i64 + view.shift(grid);
        let Some(g) = model.grids.get(&grid).filter(|_| inset > 0) else {
            continue;
        };
        let below = row + height;
        let has = match laststatus {
            2 => true,
            1 => windows.len() > 1 || below < bottom,
            _ => below < bottom,
        };
        if !has || below >= frame.height || col >= frame.width {
            continue;
        }
        let start = below * frame.width + col;
        let end = start + width.min(frame.width - col);
        super::wide::realign(&mut frame.cells[start..end], inset as usize, g.width());
        laid = true;
    }
    laid
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
            anchor: None,
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

    /// A frame put together in the buffer of another, of any size and
    /// whatever it held, is the frame put together afresh.
    #[test]
    fn a_frame_put_together_in_another_s_buffer_is_the_same() {
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
        let fresh = compose(&m, &Still, 6, 3);
        for (w, h) in [(9, 5), (6, 3), (2, 1)] {
            let mut frame = compose(&m, &Still, w, h);
            frame.cells[0].text = Text::Char('?');
            compose_into(&mut frame, &m, &Still, 6, 3, true);
            assert_eq!(frame, fresh, "in the buffer of a {w}x{h} frame");
        }
    }

    /// A wide character of grid 1 that a window's place cuts in two, either
    /// way, leaves the half outside it as a blank.
    #[test]
    fn a_wide_character_cut_by_a_windows_place_is_mended() {
        let wide = |col| {
            let cell = |text, n| LineCell {
                text,
                hl: Some(0),
                repeat: n,
            };
            vec![
                resize(1, 4, 1),
                Event::GridLine {
                    grid: 1,
                    row: 0,
                    col: 0,
                    cells: vec![
                        cell(Text::Char('x'), 1),
                        cell(Text::Char('中'), 1),
                        cell(Text::Half, 1),
                        cell(Text::Char('y'), 1),
                    ],
                },
                resize(2, 2, 1),
                line(2, 0, "ww", 0),
                Event::WinPos {
                    grid: 2,
                    win: Some(1000),
                    row: 0,
                    col,
                    width: 2,
                    height: 1,
                },
            ]
        };
        // Its right half in the place, then its left.
        let f = compose(&model(wide(2)), &Still, 4, 1);
        assert_eq!(f.text(), "x ww");
        assert!(!f.cells[1].wide);
        let f = compose(&model(wide(0)), &Still, 4, 1);
        assert_eq!(f.text(), "ww y");
    }

    /// A window's status line laid over its text, narrower than the window
    /// and in its middle, leaves the rest of the window's place blank: none
    /// of what grid 1 has there shows either side of the text.
    #[test]
    fn a_status_line_over_centred_text_leaves_the_place_blank() {
        let mut m = model(vec![
            resize(1, 6, 3),
            line(1, 0, "######", 0),
            line(1, 1, "######", 0),
            line(1, 2, "ab  cd", 0),
            resize(2, 4, 2),
            line(2, 0, "WWWW", 0),
            line(2, 1, "WWWW", 0),
            Event::WinPos {
                grid: 2,
                win: Some(1000),
                row: 0,
                col: 0,
                width: 6,
                height: 2,
            },
        ]);
        m.laststatus = Some(2);
        assert_eq!(compose(&m, &Still, 6, 3).text(), " WWWW \n WWWW \n abcd ");
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

    /// A window's text on its way across its window is drawn as far from
    /// where it goes as the view says, and kept to the window: what it has
    /// past the window's edge is not drawn over the separator beside it.
    #[test]
    fn text_on_its_way_across_its_window_stays_in_it() {
        struct Across;
        impl View for Across {
            fn origin(&self, _grid: u64, placed: (i64, i64)) -> (i64, i64) {
                placed
            }
            fn scroll(&self, _grid: u64) -> Option<&dyn Scrolling> {
                None
            }
            fn shift(&self, grid: u64) -> i64 {
                if grid == 2 {
                    2
                } else {
                    0
                }
            }
        }
        let m = model(vec![
            resize(1, 6, 1),
            line(1, 0, "   |  ", 0),
            resize(2, 3, 1),
            line(2, 0, "abc", 0),
            Event::WinPos {
                grid: 2,
                win: Some(1000),
                row: 0,
                col: 0,
                width: 3,
                height: 1,
            },
        ]);
        assert_eq!(compose(&m, &Still, 6, 1).text(), "abc|  ");
        assert_eq!(
            compose(&m, &Across, 6, 1).text(),
            "  a|  ",
            "two columns on its way, and cut at the window's edge"
        );
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
