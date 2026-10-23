//! Composes one frame of the screen from the grids.
//!
//! Order: grid 1 (tabline, status lines, separators, and everything when
//! `ext_multigrid` is off), then each window at its place, then floats and the
//! message area in Neovim's order (`zindex`, then `compindex`), blended with
//! what is under them where their highlight says.
//!
//! Animations decide where a grid is drawn ([`View::origin`]) and what a
//! scrolling window shows ([`View::scroll`]); their own drawing over the
//! windows goes on before the floats ([`View::over_windows`]).
//!
//! A wide character is drawn from its left cell; the right cell draws nothing.
//! When an overlay covers one half, [`Frame::mend`] blanks the other half in
//! its own colours, as Neovim's compositor does. It runs after every overlay.

use super::grid::{Cell, Grid, Text};
use super::model::{Model, Place};
use super::style::{self, Color, Style};
use crate::palette::Rgb;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Out {
    pub text: Text,
    pub style: Style,
    /// Two cells wide: the next cell is its right half.
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

    /// Like [`Frame::new`], reusing the buffer: a screen-sized frame is a few
    /// hundred KB, otherwise allocated and faulted in 60 times a second.
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

    /// A cell at a signed position that may be off the screen.
    pub fn at(&mut self, row: i64, col: i64) -> Option<&mut Out> {
        let row = usize::try_from(row).ok()?;
        let col = usize::try_from(col).ok()?;
        self.get_mut(row, col)
    }

    /// Blank any half of a wide character whose other half was overwritten.
    pub fn mend(&mut self) {
        for r in 0..self.height {
            self.mend_row(r);
        }
    }

    /// [`Frame::mend`] on the rows flagged in `rows` only.
    pub fn mend_rows(&mut self, rows: &[bool]) {
        for r in (0..self.height).filter(|&r| rows[r]) {
            self.mend_row(r);
        }
    }

    fn mend_row(&mut self, r: usize) {
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

/// Where the animations put things this frame.
pub trait View {
    /// Where grid `grid` is drawn, given where the editor put it.
    fn origin(&self, grid: u64, placed: (i64, i64)) -> (i64, i64);

    fn scroll(&self, grid: u64) -> Option<&dyn Scrolling>;

    /// Column offset of grid `grid`'s text from its place in the window while
    /// it moves in or out, or its grid narrows or widens (see `super::wide`).
    fn shift(&self, _grid: u64) -> i64 {
        0
    }

    /// Draw after the windows and before the floats: windows moving between
    /// layouts, a window fading between buffers.
    fn over_windows(&self, _frame: &mut Frame, _model: &Model) {}

    /// Runs of cells drawn over row `r` of grid `grid`, replacing its own or a
    /// scroll's: what Neovim will draw once it gets a key whose cursor move or
    /// typing the client already drew (see `super::anim::chain`).
    fn patches(&self, _grid: u64, _r: usize) -> &[Patch] {
        &[]
    }
}

/// Cells drawn over a grid row from column `col`; see [`View::patches`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    pub col: usize,
    pub cells: Vec<Cell>,
}

/// A scroll in progress.
pub trait Scrolling {
    /// The rows and columns that scroll: `top..bot` × `left..right`.
    fn rect(&self) -> (usize, usize, usize, usize);

    /// Row `r` of the rectangle (from its top) as shown now, of the
    /// rectangle's width, or `None` to draw it blank. `grid` is the grid as it
    /// is now; a row may be borrowed from it.
    fn row<'a>(&'a self, r: usize, grid: &'a Grid) -> Option<&'a [Cell]>;

    /// Which grid row of the rectangle row `r` shows now, both counted from
    /// its top; `None` for a line the scroll holds itself (kept, or drawn
    /// ahead of Neovim). Tails past a row's end follow grid rows (see
    /// `super::wide`), so such a line has none.
    fn own_row(&self, _r: usize) -> Option<usize> {
        None
    }
}

/// Nothing animating.
pub struct Still;

impl View for Still {
    fn origin(&self, _grid: u64, placed: (i64, i64)) -> (i64, i64) {
        placed
    }

    fn scroll(&self, _grid: u64) -> Option<&dyn Scrolling> {
        None
    }
}

pub fn compose(model: &Model, view: &dyn View, width: usize, height: usize) -> Frame {
    let mut frame = Frame::new(0, 0, Style::default());
    compose_into(&mut frame, model, view, width, height, true);
    frame
}

/// [`compose`] into `frame`'s buffer. A caller that paints over the frame and
/// mends afterwards passes `mend: false`, unless what it paints reads the
/// cells under it (see [`Frame::mend`]).
pub fn compose_into(
    frame: &mut Frame,
    model: &Model,
    view: &dyn View,
    width: usize,
    height: usize,
    mend: bool,
) {
    frame.reset(width, height, model.style(0));
    put_together(frame, model, view, mend, None);
}

/// Recompose and mend only the rows flagged in `rows`. `frame` must be the
/// previous frame, same size, differing only in those rows.
///
/// Nothing may move or be drawn over the windows in either frame (see
/// `super::anim::Animator::still`): animation drawing spans rows.
pub fn compose_rows(frame: &mut Frame, model: &Model, view: &dyn View, rows: &[bool]) {
    let blank = Out::blank(model.style(0));
    let width = frame.width;
    for r in (0..frame.height).filter(|&r| rows[r]) {
        for cell in &mut frame.cells[r * width..(r + 1) * width] {
            cell.clone_from(&blank);
        }
    }
    put_together(frame, model, view, true, Some(rows));
}

/// Shared by [`compose_into`] and [`compose_rows`]: everything after
/// blanking, limited to `rows` if given.
fn put_together(
    frame: &mut Frame,
    model: &Model,
    view: &dyn View,
    mend: bool,
    rows: Option<&[bool]>,
) {
    let height = frame.height;
    // Grid 1 is not drawn in window places: it holds stale content there
    // that a window sliding away would uncover.
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
        rows,
    );

    // Status lines (grid 1) are realigned over text drawn narrower than its
    // window (see `super::wide`). They should never land in a window's place,
    // but if one does, the places are blanked again.
    if status_lines_over_text(frame, model, view, rows) {
        let blank = Out::blank(model.style(0));
        for &(row, col, width, height) in &places {
            for r in (0..height).filter(|&r| wanted(rows, row + r)) {
                for c in 0..width {
                    if let Some(cell) = frame.at((row + r) as i64, (col + c) as i64) {
                        *cell = blank.clone();
                    }
                }
            }
        }
    }

    // Windows never overlap, so order does not matter. A grid narrower than
    // its place fills the middle.
    for (&grid, p) in &model.layout {
        if p.hidden || grid == 1 {
            continue;
        }
        if let Place::Window { .. } = p.place {
            let at = view.origin(grid, model.drawn_origin(grid));
            // Moving text is clipped to the window's place; tails past the
            // grid's edge are drawn within that place.
            let shift = view.shift(grid);
            let place = model.window_cells(grid).map(|(_, _, width, _)| {
                let left = at.1 - model.inset(grid) as i64;
                (left, left + width as i64)
            });
            let within = Within {
                cols: place.filter(|_| shift != 0),
                place,
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
                rows,
            );
        }
    }
    if rows.is_none() {
        view.over_windows(frame, model);
    }

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
            // A float anchored to a window moves with the window's text.
            Place::Float { .. } => {
                let at = (at.0, at.1 + view.shift(grid));
                draw(
                    frame,
                    model,
                    view,
                    grid,
                    at,
                    Within::default(),
                    true,
                    &[],
                    rows,
                );
            }
            Place::Message { scrolled, sep, .. } => {
                // Clipped to the screen bottom, whatever the grid's height.
                let below = (height as i64 - at.0).max(0) as usize;
                if *scrolled && at.0 > 0 && wanted(rows, (at.0 - 1) as usize) {
                    separator(frame, model, at.0 - 1, sep);
                }
                let within = Within {
                    rows: Some(below),
                    ..Within::default()
                };
                draw(frame, model, view, grid, at, within, true, &[], rows);
            }
            Place::Window { .. } => {}
        }
    }

    match rows {
        Some(rows) => frame.mend_rows(rows),
        None if mend => frame.mend(),
        None => {}
    }
}

/// Whether frame row `row` is to be composed; all are when `rows` is `None`.
fn wanted(rows: Option<&[bool]>, row: usize) -> bool {
    rows.is_none_or(|rows| rows.get(row).copied().unwrap_or(false))
}

/// What part of a grid [`draw`] puts on the frame: rows from its top and the
/// frame columns it may cover (all when `None`); for a window, its place's
/// columns, which tails past its grid's edge may cover (see `super::wide`).
#[derive(Debug, Clone, Copy, Default)]
struct Within {
    rows: Option<usize>,
    cols: Option<(i64, i64)>,
    place: Option<(i64, i64)>,
}

/// Draw grid `grid` with its top left at `at`, limited by `within`, skipping
/// the frame rectangles in `skip` (top row, first column, columns, rows),
/// blending where `blends` and its highlights say, and only on the rows
/// flagged in `rows` if given.
///
/// Columns are computed once per row and highlight colours once per run: a
/// frame is tens of thousands of cells, redrawn whole when the cursor moves.
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
    rows: Option<&[bool]>,
) {
    let Some(g) = model.grids.get(&grid) else {
        return;
    };
    let colors = model.colors();
    let scroll = view.scroll(grid);
    let blank = Cell::default();
    let height = within.rows.map_or(g.height(), |n| n.min(g.height()));
    // The grid's columns that land on the frame, within `within.cols`.
    let width = frame.width as i64;
    let (mut lo, mut hi) = ((-at.1).max(0), (g.width() as i64).min(width - at.1));
    if let Some((from, to)) = within.cols {
        lo = lo.max(from - at.1);
        hi = hi.min(to - at.1);
    }
    if lo >= hi {
        return;
    }
    // What runs past the grid's right edge, on the window's place.
    let tails = within
        .place
        .and_then(|place| Some((place, model.tails.get(&grid)?)));
    // Last highlight resolved, with its style and blend.
    let mut last: Option<(u32, Style, u8)> = None;
    let (mut runs, mut cut) = (Vec::with_capacity(4), Vec::with_capacity(4));
    for r in 0..height {
        let y = at.0 + r as i64;
        if y < 0 || y >= frame.height as i64 || !wanted(rows, y as usize) {
            continue;
        }
        let y = y as usize;
        let own = g.row(r);
        // Inside a scroll's rectangle the row is the scroll's.
        let scrolled = scroll.and_then(|s| {
            let (top, bot, left, right) = s.rect();
            (top..bot)
                .contains(&r)
                .then(|| (left, right, s.row(r - top, g)))
        });
        // This row's columns, minus the skipped rectangles.
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
        let patches = view.patches(grid, r);
        for &(s, e) in &runs {
            for c in s as usize..e as usize {
                let patch = patches
                    .iter()
                    .find(|p| (p.col..p.col + p.cells.len()).contains(&c));
                let (cell, wide) = match (patch, scrolled) {
                    (Some(p), _) => {
                        let i = c - p.col;
                        let wide = p.cells.get(i + 1).is_some_and(|n| n.text == Text::Half);
                        (&p.cells[i], wide)
                    }
                    (None, Some((left, right, line))) if (left..right).contains(&c) => {
                        let i = c - left;
                        let line = line.unwrap_or(&[]);
                        let cell = line.get(i).unwrap_or(&blank);
                        let wide = line.get(i + 1).is_some_and(|n| n.text == Text::Half);
                        (cell, wide)
                    }
                    (None, _) => (
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
                // A blank lets what is under it through, unless that is the
                // right half of a wide character, which shows nothing itself.
                if cell.text.is_blank() && under.text != Text::Half {
                    under.style = style::blend_through(&colors, &under.style, &style, blend);
                } else {
                    under.style = style::blend_over(&colors, &under.style, &style, blend);
                    under.text = cell.text.clone();
                    under.wide = wide;
                }
            }
        }
        // What Neovim draws past the line's end, cut at the grid's edge (see
        // `super::wide`): the tail of the grid row shown here, its own or one
        // a scroll brought; none for a line the scroll holds itself.
        let Some(((from, to), tails)) = tails else {
            continue;
        };
        let shows = match scroll.map(|s| (s, s.rect())) {
            Some((s, (top, bot, left, right))) if (top..bot).contains(&r) => (left == 0
                && right >= g.width())
            .then(|| s.own_row(r - top))
            .flatten()
            .map(|i| top + i),
            _ => Some(r),
        };
        let Some((start, cells)) = shows
            .and_then(|s| Some((s, tails.get(&s)?)))
            .and_then(|(s, tail)| super::wide::past(g.row(s), tail))
        else {
            continue;
        };
        // Only columns past the grid's edge, within the place and the frame.
        let (from, to) = (from.max(at.1 + g.width() as i64).max(0), to.min(width));
        for (i, cell) in cells.iter().enumerate() {
            let x = at.1 + (start + i) as i64;
            if x < from || x >= to {
                continue;
            }
            let style = match last {
                Some((hl, style, _)) if hl == cell.hl => style,
                _ => {
                    let style = model.style(cell.hl);
                    last = Some((cell.hl, style, 0));
                    style
                }
            };
            let out = &mut row[x as usize];
            out.text.clone_from(&cell.text);
            out.style = style;
            out.wide = cells.get(i + 1).is_some_and(|n| n.text == Text::Half);
        }
    }
}

/// Realign each window's status line over its text where its grid is drawn
/// narrower than the window (see [`super::wide::realign`]). Which windows have
/// one follows `'laststatus'`: all at 2; at 1, all when there are two or more;
/// at 0, those with a window below. None at 3 (one global line) or when
/// unknown. Returns whether any was realigned.
fn status_lines_over_text(
    frame: &mut Frame,
    model: &Model,
    view: &dyn View,
    rows: Option<&[bool]>,
) -> bool {
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
        // Text offset, including any animated shift.
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
        if !has || below >= frame.height || col >= frame.width || !wanted(rows, below) {
            continue;
        }
        let start = below * frame.width + col;
        let end = start + width.min(frame.width - col);
        super::wide::realign(&mut frame.cells[start..end], inset as usize, g.width());
        laid = true;
    }
    laid
}

/// The row above messages scrolled up over the windows: `sep` across the
/// whole width, in `MsgSeparator`.
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

/// Move a style towards `to`, keeping `keep` percent of its colours; at 0,
/// everything is `to`.
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

    /// Recomposing just the changed row over the previous frame matches a full
    /// compose, for each row in turn. Covers grid 1, a window under a blending
    /// float that cuts a wide character, and a float over cells no grid covers.
    #[test]
    fn the_rows_a_change_touched_put_together_again_are_the_whole_frame() {
        let blend = Event::HlAttr {
            id: 3,
            rgb: Attrs {
                bg: Some(0x5a_0000),
                blend: 50,
                ..Attrs::default()
            },
            cterm: Attrs::default(),
        };
        let mut m = model(vec![
            blend,
            resize(1, 8, 5),
            line(1, 4, "status..", 0),
            resize(2, 8, 4),
            line(2, 0, "aaaaaaaa", 0),
            line(2, 1, "b中bbbbb", 0),
            line(2, 2, "cccccccc", 0),
            line(2, 3, "dddddddd", 0),
            Event::WinPos {
                grid: 2,
                win: Some(1000),
                row: 0,
                col: 0,
                width: 8,
                height: 4,
            },
            resize(3, 3, 2),
            line(3, 0, "f f", 3),
            line(3, 1, "ggg", 3),
            float(3, 1, 2, 50),
            resize(4, 3, 2),
            line(4, 0, "k k", 3),
            line(4, 1, "lll", 3),
            float(4, 2, 7, 60),
        ]);
        let mut shown = compose(&m, &Still, 10, 5);
        for (grid, row, text, hl, y) in [
            (2, 1, "x中xxxxx", 0, 1),
            (2, 2, "y yyyyyy", 0, 2),
            (3, 1, "hhh", 3, 2),
            (4, 1, "m m", 3, 3),
            (1, 4, "STATUS..", 0, 4),
        ] {
            let mut changes = Changes::default();
            m.apply(line(grid, row, text, hl), &mut changes);
            let mut rows = vec![false; 5];
            rows[y] = true;
            compose_rows(&mut shown, &m, &Still, &rows);
            assert_eq!(shown, compose(&m, &Still, 10, 5), "grid {grid}, row {row}");
        }
    }

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

    /// A scrolled message area draws its separator above it and covers the
    /// screen below.
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
        // Halfway between the terminal's black and the float's blue.
        assert_eq!(f.cells[0].style.bg, Color::Rgb(Rgb(0, 0, 127)));
    }

    /// A float over a wide character's right half blanks its left half.
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

    /// Either way round, the half outside the place is blanked.
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
        // Right half in the place, then left half.
        let f = compose(&model(wide(2)), &Still, 4, 1);
        assert_eq!(f.text(), "x ww");
        assert!(!f.cells[1].wide);
        let f = compose(&model(wide(0)), &Still, 4, 1);
        assert_eq!(f.text(), "ww y");
    }

    /// Grid 1 does not show either side of a centred status line, also when
    /// only the status row is recomposed.
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

        let mut shown = compose(&m, &Still, 6, 3);
        m.apply(line(1, 2, "ef  gh", 0), &mut Changes::default());
        compose_rows(&mut shown, &m, &Still, &[false, false, true]);
        assert_eq!(shown, compose(&m, &Still, 6, 3));
        assert_eq!(shown.text(), " WWWW \n WWWW \n efgh ");
    }

    /// Not even when the window is drawn elsewhere for a frame.
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

    /// Shifted text is clipped to its window, not drawn over the separator.
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
