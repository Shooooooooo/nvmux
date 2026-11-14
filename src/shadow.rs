//! A shadow copy of an attached session's screen, so it can be dissolved.
//!
//! [`crate::pty`] relays the client's bytes untouched, so nvmux does not know
//! what is on screen, yet a fade needs each cell's colour. A [`Shadow`] is a
//! `vt100` parser fed a copy of the session's output (never nvmux's own
//! overlays, such as [`crate::hint`], whose underlying cells it must
//! remember). It only watches: a misparse just makes a frame slightly wrong
//! until the server repaints. `[effects.fade] session = false` disables the
//! parse; a kept client (`[client] per_session`) parses anyway for its own
//! copy (see `pty::Attachment::paint_kept_screen`).
//!
//! `vt100` keeps one bit of a cell's underline, and nothing of its blink,
//! conceal, strikethrough, alternate font or overline. While the session
//! shows any of those, a twin parser is fed the same bytes with each SGR
//! replaced by one whose colours stand for what the first drops (see `Held`).
//! Its grid holds that cell for cell through every scroll and erase: behind
//! a floating screen a wavy underline stays wavy, and a line through a word
//! stays through it. A twin costs a second parse and a second grid, and a
//! screen that changes colour at every cell is then fed at a third of the
//! speed in a debug build. So it starts at the first SGR that needs one (see
//! `Watch`) and goes once nothing on screen does (see `TWIN_CHECK`).
//!
//! # Painters
//!
//! - [`Shadow::under`]: cross-fades an overlay rectangle with the cells under
//!   it, so the notice melts into the editor even if no repaint comes.
//! - [`Shadow::frame`]: every colour `t` toward the background, resolved
//!   through [`Palette`]. A default background stays `49`: the terminal may
//!   be transparent.
//! - [`Shadow::paint`]: the grid exactly as drawn, for a kept client
//!   returning without a fade; the server's `:mode` repaint lands on top.
//! - [`Shadow::frame_under`]: a floating screen and its hint row over the
//!   shaded session (see `crate::pty::panel`).
//!
//! A cell a terminal may draw at another width than the parser holds it
//! (`crate::client::screen::ambiwidth`) is painted as the client's writer
//! draws it: over two spaces if it takes two columns, the next cell placed
//! absolutely. A flag, which the parser holds in two cells, is painted as
//! one wide character (see `flag`).
//!
//! Frames are diffs (only `t` changes), so a whole fade costs about one full
//! repaint. Runs of empty cells are erased with `EL` or `ECH` under a pen
//! with no background, so they show the terminal's own background whether it
//! erases in the pen's or its own. Like the attach notice
//! ([`crate::announce`]), every frame is one synchronized update, places runs
//! absolutely, never writes a newline or CR (`OPOST` is off), and hides the
//! cursor while writing.
//!
//! # The cursor
//!
//! A fade in is nvmux's last write, and the repaint after it is not certain
//! (a busy server may not answer; an idle editor never redraws), so its last
//! frame restores the cursor itself ([`Cursor::Restored`]). A fade out ends
//! hidden; whatever takes the screen next shows it.

use std::io::Write;

use crate::client::screen::{self as writer, ambiwidth};
use crate::client::style::{self, Underline};
use crate::fade::{SYNC_BEGIN, SYNC_END};
use crate::falloff;
use crate::palette::{Palette, Rgb};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
const RESET_SGR: &[u8] = b"\x1b[0m";
/// `DECSC` / `DECRC`: one shared save slot, so only output between the
/// session's own frames may use it (see [`Shadow::under`]).
const SAVE_CURSOR: &[u8] = b"\x1b7";
const RESTORE_CURSOR: &[u8] = b"\x1b8";

/// Dissolve progress at which a cell with both an overlay glyph and a session
/// cell swaps which it shows: halfway, where both are equally dissolved.
const CROSSOVER: f32 = 0.5;

/// How far toward its background dim text is drawn once nvmux mixes its
/// colour (see [`resolve`]), in place of SGR 2: Windows Terminal halves a
/// dim colour toward black, past a background the colour is nearly at.
/// kitty's amount, and about a halving on a dark background.
const DIM: f32 = 0.6;

/// Where a frame leaves the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    /// Hidden, as while writing: every fade-out frame and all but the last
    /// fade-in frame.
    Hidden,
    /// Where the session had it, shown if it was: the last fade-in frame
    /// (see the module docs).
    Restored,
}

/// Minimum grid size given to the parser, in either dimension.
///
/// `vt100` 0.16 underflows on a one-row grid (or one column with a wide
/// character) when text wraps: `col_wrap` scrolls, then subtracts from row 0.
/// Neovim cannot run that small anyway.
const MIN_SIZE: u16 = 2;

/// Text nvmux drew over the session (the attach notice or the hint bar,
/// [`crate::hint`]) and the cells it covers.
///
/// Lives here, not in [`crate::announce`] (which computes the geometry),
/// because only the shadow can reach the cells underneath.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Over {
    /// Top-left cell, 0-based (the notice writes 1-based `CUP`s).
    pub top: u16,
    pub left: u16,
    /// Display width of every row, so a wide glyph counts as two.
    pub width: u16,
    /// A space is a covered cell with nothing drawn, which hides what is
    /// under it.
    pub rows: Vec<String>,
}

/// Session cells a dissolve leaves standing: their text stays at full colour
/// while all else dissolves. For a card nvmux's client drew (see
/// [`crate::client::cards::arrival_footprint`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Kept {
    /// `(row, first col, last col)`, 0-based.
    spans: Vec<(u16, u16, u16)>,
    /// The card's text, each char on its start cell: `glyphs` stay put,
    /// `moving` slides along its row with the card by up to `shift` columns.
    /// [`Shadow::shows`] checks the screen shows it.
    glyphs: Vec<(u16, u16, char)>,
    moving: Vec<(u16, u16, char)>,
    shift: i16,
}

impl Kept {
    /// `spans` are `(row, first col, last col)`, `glyphs` `(row, col, char)`,
    /// all 0-based.
    pub fn new(
        spans: impl IntoIterator<Item = (usize, usize, usize)>,
        glyphs: impl IntoIterator<Item = (usize, usize, char)>,
    ) -> Self {
        let cell = |n: usize| u16::try_from(n).ok();
        Self {
            spans: spans
                .into_iter()
                .filter_map(|(row, from, to)| Some((cell(row)?, cell(from)?, cell(to)?)))
                .collect(),
            glyphs: glyphs.into_iter().filter_map(Self::glyph).collect(),
            moving: Vec::new(),
            shift: 0,
        }
    }

    /// With `moving` on them too, anywhere from its start to `shift` columns
    /// along: a row of names on the move.
    pub fn moving(
        self,
        moving: impl IntoIterator<Item = (usize, usize, char)>,
        shift: isize,
    ) -> Self {
        Self {
            moving: moving.into_iter().filter_map(Self::glyph).collect(),
            shift: i16::try_from(shift).unwrap_or(0),
            ..self
        }
    }

    fn glyph((row, col, ch): (usize, usize, char)) -> Option<(u16, u16, char)> {
        Some((u16::try_from(row).ok()?, u16::try_from(col).ok()?, ch))
    }

    /// Where an arriving `card` stands (see
    /// [`crate::client::cards::arrival_footprint`]).
    pub fn arrival(card: &crate::card::Card, cols: u16, rows: u16) -> Self {
        let f = crate::client::cards::arrival_footprint(card, usize::from(cols), usize::from(rows));
        Self::new(f.spans, f.glyphs).moving(f.moving, f.shift)
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Whether the standing text moves: a row of names the client keeps
    /// redrawing as it slides.
    pub fn moves(&self) -> bool {
        !self.moving.is_empty() && self.shift != 0
    }

    pub fn spans(&self) -> &[(u16, u16, u16)] {
        &self.spans
    }

    fn holds(&self, row: u16, col: u16) -> bool {
        self.spans
            .iter()
            .any(|&(r, from, to)| r == row && (from..=to).contains(&col))
    }
}

/// What [`Shadow::frame_under`] does with a session cell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shade {
    /// Colours this far toward the terminal's background, through its
    /// palette; blank from [`falloff::EMPTIED`] on.
    Toward(f32),
    /// Faint in its own colours, when no palette is known. Faint dims text,
    /// not background.
    Faint,
    /// The terminal's own background.
    Blank,
}

/// A rectangle nvmux draws over the session in place of its cells: a
/// floating screen (settings, help, picker or naming prompt) or its hint row
/// on the last row (see `crate::pty::panel`), from the buffer the screen drew.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Laid {
    top: u16,
    left: u16,
    width: u16,
    height: u16,
    /// Per row, how many cells at each end rounded corners leave to the
    /// session ([`Laid::rounded`]); empty for none.
    cut: Vec<u16>,
    /// Every cell's text in one string, not one allocation per cell per
    /// frame.
    text: String,
    /// Per cell, row by row: its byte range in `text` and its pen; `None` for
    /// the second half of a wide character.
    cells: Vec<Option<(u32, u32, Pen)>>,
}

impl Laid {
    /// `buf`'s cells, placed at its area.
    pub fn from_buffer(buf: &ratatui::buffer::Buffer) -> Self {
        let area = buf.area;
        let mut cells = Vec::with_capacity(buf.content.len());
        let mut text = String::with_capacity(buf.content.len());
        for row in buf.content.chunks(usize::from(area.width).max(1)) {
            let mut skip = 0;
            for cell in row {
                if skip > 0 {
                    skip -= 1;
                    cells.push(None);
                    continue;
                }
                let symbol = match cell.symbol() {
                    "" => " ",
                    symbol => symbol,
                };
                skip = UnicodeWidthStr::width(symbol).max(1) - 1;
                let from = text.len() as u32;
                text.push_str(symbol);
                cells.push(Some((from, text.len() as u32, Pen::of_buffer(cell))));
            }
        }
        Self {
            top: area.y,
            left: area.x,
            width: area.width,
            height: area.height,
            cut: Vec::new(),
            text,
            cells,
        }
    }

    /// `buf`'s cells, placed at its area, but for those its corners rounded
    /// `radius` columns cut off ([`falloff::distance`]).
    pub fn rounded(buf: &ratatui::buffer::Buffer, radius: f32) -> Self {
        let a = buf.area;
        let off = |row, col| falloff::distance(col, row, a.x, a.y, a.width, a.height, radius) > 0.0;
        Self {
            cut: (a.y..a.bottom())
                .map(|row| {
                    (a.x..a.x + a.width / 2)
                        .take_while(|&col| off(row, col))
                        .count() as u16
                })
                .collect(),
            ..Self::from_buffer(buf)
        }
    }

    /// The cell at screen (`row`, `col`), if covered: its text and pen, or
    /// `None` for the second half of a wide character.
    fn at(&self, row: u16, col: u16) -> Option<Option<(&str, Pen)>> {
        if row < self.top || col < self.left {
            return None;
        }
        let (r, c) = (row - self.top, col - self.left);
        if r >= self.height || c >= self.width {
            return None;
        }
        let cut = self.cut.get(usize::from(r)).copied().unwrap_or(0);
        if c < cut || c >= self.width - cut {
            return None;
        }
        let cell = self
            .cells
            .get(usize::from(r) * usize::from(self.width) + usize::from(c))?;
        Some(cell.map(|(from, to, pen)| (&self.text[from as usize..to as usize], pen)))
    }

    fn holds(&self, row: u16, col: u16) -> bool {
        self.at(row, col).is_some()
    }
}

/// A cell as a frame paints it, compared against the last frame. Keeps every
/// attribute the parsers hold except inverse and, in a mixed colour, dim
/// (both folded into the colours, see [`resolve`]), or the fade would snap
/// shape at the end (italics dissolving upright).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Painted {
    fg: Rgb,
    /// `None` is the terminal's default background, left as `49`.
    bg: Option<Rgb>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: Underline,
    /// The underline's colour; `None` is the text's.
    sp: Option<Rgb>,
    flags: Flags,
}

impl Painted {
    /// A cell with nothing written: the background as foreground, no
    /// attributes; the same at any `t`.
    fn blank(palette: &Palette) -> Self {
        Painted {
            fg: palette.bg,
            bg: None,
            bold: false,
            dim: false,
            italic: false,
            underline: Underline::None,
            sp: None,
            flags: Flags::NONE,
        }
    }

    /// Whether a space in this pen is invisible: default background, no
    /// line under, through or over it.
    fn shows_nothing(&self) -> bool {
        self.bg.is_none() && self.underline == Underline::None && !self.flags.lined()
    }

    /// Whether a space in this pen looks as one in `other` does.
    fn looks_like(&self, other: &Self) -> bool {
        self == other || (self.shows_nothing() && other.shows_nothing())
    }
}

pub struct Shadow {
    parser: vt100::Parser<Watch>,
    /// `parser`'s twin (see the module docs), while it has one: each cell's
    /// colours stand for its [`Held`], and its pen's for the session's.
    twin: Option<vt100::Parser>,
    /// Bytes the twin has been fed since it was last looked over (see
    /// [`TWIN_CHECK`]).
    twin_fed: usize,
    /// Keeps the parsers' OSC buffers small: they ignore OSCs, and an
    /// `OSC 52` copy would otherwise stay buffered for the client's lifetime
    /// (see [`crate::ledger::OscCap`]).
    osc: crate::ledger::OscCap,
    /// A partial character or SGR at the end of the last feed, held back
    /// until the rest comes (see [`whole`]).
    cut: Vec<u8>,
    /// What the last frame painted, per cell in row-major order; empty when
    /// the screen changed and the next frame must paint every cell.
    painted: Vec<Painted>,
    /// Like `painted`, for [`Shadow::frame_under`]. Trusted only while
    /// `under_valid`, but kept to be overwritten in place.
    under: Vec<Drawn>,
    under_valid: bool,
    /// The parser panicked on session output, so its grid is untrusted:
    /// nothing more is fed, and frames paint nothing (see [`Shadow::feed`]).
    broken: bool,
}

impl std::fmt::Debug for Shadow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (rows, cols) = self.size();
        f.debug_struct("Shadow")
            .field("rows", &rows)
            .field("cols", &cols)
            .finish_non_exhaustive()
    }
}

impl Shadow {
    /// An empty screen of this size. No scrollback: only what is on screen can
    /// be faded.
    pub fn new(rows: u16, cols: u16) -> Self {
        let (rows, cols) = (rows.max(MIN_SIZE), cols.max(MIN_SIZE));
        Self {
            parser: vt100::Parser::new_with_callbacks(rows, cols, 0, Watch::default()),
            twin: None,
            twin_fed: 0,
            osc: crate::ledger::OscCap::default(),
            cut: Vec::new(),
            painted: Vec::new(),
            under: Vec::new(),
            under_valid: false,
            broken: false,
        }
    }

    /// Parse a copy of session output. A trailing partial character or SGR
    /// waits for the rest (see [`whole`]); OSC payload past
    /// [`crate::ledger::OSC_SHOWN`] is dropped. A parser panic is caught and
    /// retires the shadow, so the session cuts out instead of dissolving.
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.broken {
            return;
        }
        let shown = self.osc.cap(bytes);
        // Prepend what the last feed ended with, cut off.
        let joined;
        let bytes = if self.cut.is_empty() {
            &shown[..]
        } else {
            joined = [std::mem::take(&mut self.cut).as_slice(), &shown].concat();
            joined.as_slice()
        };
        let end = whole(bytes);
        self.cut.extend_from_slice(&bytes[end..]);
        let bytes = &bytes[..end];
        let mut fed = self.twin_fed;
        self.guard(|parser, twin| track(parser, twin, &mut fed, bytes));
        self.twin_fed = fed;
        self.invalidate();
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if self.broken {
            return;
        }
        let (rows, cols) = (rows.max(MIN_SIZE), cols.max(MIN_SIZE));
        self.guard(|parser, twin| {
            parser.screen_mut().set_size(rows, cols);
            if let Some(twin) = twin {
                twin.screen_mut().set_size(rows, cols);
            }
        });
        self.invalidate();
    }

    fn guard(&mut self, f: impl FnOnce(&mut vt100::Parser<Watch>, &mut Option<vt100::Parser>)) {
        let (parser, twin) = (&mut self.parser, &mut self.twin);
        if crate::panics::caught(|| f(parser, twin)).is_err() {
            tracing::warn!("the shadow grid's parser panicked; the session will not dissolve");
            self.broken = true;
        }
    }

    /// `(rows, cols)`.
    pub fn size(&self) -> (u16, u16) {
        self.parser.screen().size()
    }

    fn screen(&self) -> Screen<'_> {
        Screen::of(&self.parser, &self.twin)
    }

    /// The session's mouse reporting mode, to restore after nvmux used the
    /// mouse over it (see `crate::pty::panel`). `None` once retired.
    pub fn mouse(&self) -> Option<crate::term::MouseReporting> {
        use crate::term::MouseReporting;
        use vt100::MouseProtocolMode;
        if self.broken {
            return None;
        }
        Some(match self.parser.screen().mouse_protocol_mode() {
            MouseProtocolMode::None => MouseReporting::Off,
            MouseProtocolMode::AnyMotion => MouseReporting::Motion,
            MouseProtocolMode::Press
            | MouseProtocolMode::PressRelease
            | MouseProtocolMode::ButtonMotion => MouseReporting::Buttons,
        })
    }

    /// Whether frames would paint anything: false once retired, so a fade can
    /// be skipped rather than slept through.
    pub fn is_usable(&self) -> bool {
        !self.broken
    }

    /// Whether the session has the terminal on the alternate screen, which a
    /// client enters as it starts. False once retired.
    pub fn on_alternate_screen(&self) -> bool {
        !self.broken && self.parser.screen().alternate_screen()
    }

    /// Whether any cell has a glyph. A first-paint hold waits for this before
    /// a lull counts, as a pause after startup queries is not a screen.
    pub fn has_contents(&self) -> bool {
        if self.broken {
            return false;
        }
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        (0..rows).any(|row| {
            (0..cols).any(|col| screen.cell(row, col).is_some_and(vt100::Cell::has_contents))
        })
    }

    /// The terminal was written behind the shadow's back, so the next frame
    /// paints every cell. The fade diff is freed, not cleared (16 bytes a
    /// cell while a kept client is parked); [`Shadow::frame_under`]'s is kept
    /// for reuse until [`Shadow::forget_under`].
    pub fn invalidate(&mut self) {
        self.painted = Vec::new();
        self.under_valid = false;
    }

    /// Free [`Shadow::frame_under`]'s diff once the floating screen closes.
    pub fn forget_under(&mut self) {
        self.under = Vec::new();
        self.under_valid = false;
    }

    /// The bytes that paint the screen `t` of the way to `palette.bg`, as a
    /// diff against the previous frame (in full after a [`feed`] or
    /// [`resize`]), leaving the cursor as `cursor` says. Always at least the
    /// sync span, cursor hide and a trailing SGR reset.
    ///
    /// [`feed`]: Shadow::feed
    /// [`resize`]: Shadow::resize
    pub fn frame(&mut self, t: f32, palette: &Palette, cursor: Cursor) -> Vec<u8> {
        self.frame_with(|_| t, palette, cursor, None, &Kept::default())
    }

    /// [`Shadow::frame`] with each row `dissolve(row)` of the way (an iris,
    /// see [`crate::fade::Iris`]), plus overlay `over` in `ink`, `ot`
    /// dissolved, in the same synchronized update. The overlay's rectangle is
    /// repainted every frame as [`Shadow::under`] does, so the diff stays
    /// true. Text on `kept` cells stays at full colour.
    pub fn frame_with(
        &mut self,
        dissolve: impl Fn(u16) -> f32,
        palette: &Palette,
        cursor: Cursor,
        over: Option<(&Over, Rgb, f32)>,
        kept: &Kept,
    ) -> Vec<u8> {
        let screen = self.screen();
        let (rows, cols) = if self.broken { (0, 0) } else { screen.size() };
        let total = usize::from(rows) * usize::from(cols);
        let diff = self.painted.len() == total;

        let mut next = Vec::with_capacity(total);
        let mut out = Vec::new();
        out.extend_from_slice(SYNC_BEGIN);
        out.extend_from_slice(HIDE_CURSOR);
        // The stream's current SGR (`None` until written), so it is written
        // only on change.
        let mut sgr: Option<Painted> = None;
        // Which of the row's cells show nothing (see `Run`).
        let mut bare = vec![false; usize::from(cols)];
        for row in 0..rows {
            let t = dissolve(row);
            let painted_at = |col| {
                let cell = screen.cell(row, col);
                if kept.holds(row, col) {
                    standing(cell, palette, t)
                } else {
                    resolve(cell, palette, t)
                }
            };
            let mut at = At::Off;
            let mut run = Run::default();
            // This cell was written with the one before: the blank that was
            // drawn over (see `pads`), or a flag's second half (see `flag`).
            let mut joined = false;
            for col in 0..cols {
                let cell = screen.cell(row, col);
                let index = next.len();
                let painted = painted_at(col);
                next.push(painted);
                if std::mem::take(&mut joined) {
                    bare[usize::from(col)] = false;
                    continue;
                }
                // The second half of a wide character, covered by the first.
                let wide_rest = cell.is_some_and(|c| c.is_wide_continuation());
                let (text, width) = match cell {
                    Some(c) if c.has_contents() => (c.contents(), if c.is_wide() { 2 } else { 1 }),
                    _ => (" ", 1),
                };
                let drifting = cell.is_some_and(drifts);
                let rest = drifting.then(|| flag(screen.cells, row, col)).flatten();
                let padded = drifting && cell.is_some_and(|c| pads(c, screen.cell(row, col + 1)));
                // The next cell's paint, if written with this one: either
                // changing writes both.
                let with = (padded || rest.is_some()).then(|| painted_at(col + 1));
                let same = diff
                    && self.painted[index] == painted
                    && with.is_none_or(|p| self.painted[index + 1] == p);
                bare[usize::from(col)] = !wide_rest && text == " " && painted.shows_nothing();
                // An owed run absorbs the unchanged blanks after it: one erase,
                // not several.
                if bare[usize::from(col)] && (!same || !run.is_empty()) {
                    run.push(col);
                    continue;
                }
                run.flush(&mut out, (row, cols), &mut at, |out| {
                    blank_sgr(out, &mut sgr, palette);
                });
                joined = with.is_some();
                if wide_rest || same {
                    continue;
                }
                if at != At::Col(col) {
                    let over = sgr.is_some_and(|p| p.shows_nothing());
                    move_to(&mut out, (row, col), at, over, &bare);
                }
                if drifting && (with.is_some() || (width == 2 && col + 2 <= cols)) {
                    // A pad in its own pen if spaces in the cell's show
                    // otherwise, as when only one of them stands.
                    let blank = with
                        .filter(|pad| padded && !pad.looks_like(&painted))
                        .unwrap_or(painted);
                    if sgr != Some(blank) {
                        write_sgr(&mut out, blank);
                        sgr = Some(blank);
                    }
                    clear_two(&mut out, col, cols);
                }
                if sgr != Some(painted) {
                    write_sgr(&mut out, painted);
                    sgr = Some(painted);
                }
                out.extend_from_slice(text.as_bytes());
                out.extend_from_slice(rest.unwrap_or_default().as_bytes());
                at = if drifting {
                    At::Astray
                } else {
                    At::Col(col + width)
                };
            }
            run.flush(&mut out, (row, cols), &mut at, |out| {
                blank_sgr(out, &mut sgr, palette);
            });
        }
        if let Some((over, ink, ot)) = over.filter(|_| !self.broken) {
            self.paint_under(&mut out, over, palette, ink, ot, &dissolve);
        }
        out.extend_from_slice(RESET_SGR);
        // In the same synchronized update, so screen and cursor appear
        // together. A cursor the session hid stays hidden.
        if cursor == Cursor::Restored && !self.broken && !screen.hide_cursor() {
            let (row, col) = screen.cursor_position();
            let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);
            out.extend_from_slice(SHOW_CURSOR);
        }
        out.extend_from_slice(SYNC_END);
        self.painted = next;
        self.under_valid = false;
        out
    }

    /// Whether the screen shows the `kept` text (moving glyphs all shifted
    /// alike, by up to `shift`), without which they must not stand.
    pub fn shows(&self, kept: &Kept) -> bool {
        if self.broken || (kept.glyphs.is_empty() && kept.moving.is_empty()) {
            return false;
        }
        let screen = self.parser.screen();
        let on = |row: u16, col: u16, ch: char| {
            screen
                .cell(row, col)
                .is_some_and(|c| c.contents().starts_with(ch))
        };
        let along = |k: i16| {
            kept.moving.iter().all(|&(row, col, ch)| {
                col.checked_add_signed(k)
                    .is_some_and(|col| on(row, col, ch))
            })
        };
        let (near, far) = if kept.shift < 0 {
            (kept.shift, 0)
        } else {
            (0, kept.shift)
        };
        kept.glyphs.iter().all(|&(row, col, ch)| on(row, col, ch))
            && (kept.moving.is_empty() || (near..=far).any(along))
    }

    /// The bytes that redraw the `kept` text as a dissolve leaves it, on a
    /// screen the session has left (see [`crate::handoff`]); cursor and pen
    /// are restored.
    pub fn kept_bytes(&self, kept: &Kept, palette: &Palette) -> Vec<u8> {
        if self.broken || kept.is_empty() {
            return Vec::new();
        }
        let screen = self.screen();
        let cols = screen.size().1;
        let mut out = SAVE_CURSOR.to_vec();
        let mut drawn = false;
        for &(row, from, to) in &kept.spans {
            // This cell was written with the one before (see
            // `Shadow::frame_with`).
            let mut joined = false;
            for col in from..=to {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                // A flag the span's start cuts is left, as a wide character
                // is.
                if std::mem::take(&mut joined)
                    || !cell.has_contents()
                    || cell.is_wide_continuation()
                    || (col == from && flag_end(screen.cells, row, col))
                {
                    continue;
                }
                let painted = Painted {
                    bg: None,
                    ..resolve(Some(cell), palette, 0.0)
                };
                let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);
                write_sgr(&mut out, painted);
                let drifting = drifts(cell);
                let rest = drifting.then(|| flag(screen.cells, row, col)).flatten();
                joined = rest.is_some()
                    || (drifting && col < to && pads(cell, screen.cell(row, col + 1)));
                if joined || (drifting && cell.is_wide() && col + 2 <= cols) {
                    clear_two(&mut out, col, cols);
                }
                out.extend_from_slice(cell.contents().as_bytes());
                out.extend_from_slice(rest.unwrap_or_default().as_bytes());
                drawn = true;
            }
        }
        if !drawn {
            return Vec::new();
        }
        out.extend_from_slice(RESTORE_CURSOR);
        out
    }

    /// The bytes that repaint every cell as the session drew it: no diff, no
    /// palette.
    ///
    /// Ends with the cursor on the session's cell and the session's pen
    /// current, since Neovim writes its next change without placement or
    /// colour when it thinks neither moved. Hyperlinks wait for the server's
    /// repaint.
    pub fn paint(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(SYNC_BEGIN);
        out.extend_from_slice(HIDE_CURSOR);
        if !self.broken {
            let screen = self.screen();
            let (rows, cols) = screen.size();
            let mut pen: Option<Pen> = None;
            for row in 0..rows {
                let _ = write!(out, "\x1b[{};1H", row + 1);
                // The last cell written may have left the cursor elsewhere on
                // the row.
                let mut astray = false;
                let mut col = 0;
                while col < cols {
                    let cell = screen.cell(row, col);
                    // Written by the first half; a lone one has nothing to
                    // write.
                    if cell.is_some_and(|c| c.is_wide_continuation()) {
                        col += 1;
                        continue;
                    }
                    if std::mem::take(&mut astray) {
                        column(&mut out, col);
                    }
                    let this = Pen::of(cell);
                    if pen != Some(this) {
                        this.write(&mut out);
                        pen = Some(this);
                    }
                    match cell {
                        // A wide character cut by the last column (left by a
                        // narrowing) is blanked: it would wrap, and on the
                        // last row scroll the screen.
                        Some(c) if c.has_contents() && !(c.is_wide() && col + 2 > cols) => {
                            astray = drifts(c);
                            let rest = astray.then(|| flag(screen.cells, row, col)).flatten();
                            // With the next cell (see `Shadow::frame_with`).
                            let joined =
                                rest.is_some() || (astray && pads(c, screen.cell(row, col + 1)));
                            if astray && (c.is_wide() || joined) {
                                clear_two(&mut out, col, cols);
                            }
                            out.extend_from_slice(c.contents().as_bytes());
                            out.extend_from_slice(rest.unwrap_or_default().as_bytes());
                            col += if c.is_wide() || joined { 2 } else { 1 };
                        }
                        _ => {
                            out.push(b' ');
                            col += 1;
                        }
                    }
                }
            }
        }
        out.extend_from_slice(&self.hand_back());
        if !self.broken && !self.parser.screen().hide_cursor() {
            out.extend_from_slice(SHOW_CURSOR);
        }
        out.extend_from_slice(SYNC_END);
        out
    }

    /// The bytes that restore the session's pen and cursor position (not its
    /// visibility) after nvmux's writes: the tail of [`Shadow::paint`], and
    /// what follows a fade in for a client that writes before its server
    /// repaints (see `pty::relay`). A retired shadow hands back a reset.
    pub fn hand_back(&self) -> Vec<u8> {
        if self.broken {
            return RESET_SGR.to_vec();
        }
        let screen = self.screen();
        let mut out = Vec::new();
        Pen::of_session(screen).write(&mut out);
        let (row, col) = screen.cursor_position();
        let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);
        out
    }

    /// The bytes that cross-fade `over` with the session's cells under it:
    /// overlay glyphs `t` dissolved from `ink` (see [`crate::theme`]), session
    /// cells `1 - t`. At `t = 0` the cells under the notice look like its own
    /// spaces; at `t = 1` the screen is as found, so the notice needs no
    /// repaint to go.
    ///
    /// Unlike [`Shadow::frame`], it paints over a session that owns the
    /// screen, so it leaves cursor and attributes as found (`DECSC`/`DECRC`,
    /// and no cursor hide, which would blink), keeps no diff, and never
    /// writes outside the rectangle. The shared save slot is safe as in
    /// [`crate::announce`]: output only goes between the session's sequences,
    /// never between its own `DECSC` and `DECRC` (see [`crate::boundary`]).
    /// Cells past the grid's end paint as blanks.
    pub fn under(&self, over: &Over, palette: &Palette, ink: Rgb, t: f32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(SYNC_BEGIN);
        out.extend_from_slice(SAVE_CURSOR);
        if !self.broken {
            self.paint_under(&mut out, over, palette, ink, t, &|_| 1.0 - t);
        }
        out.extend_from_slice(RESET_SGR);
        out.extend_from_slice(RESTORE_CURSOR);
        out.extend_from_slice(SYNC_END);
        out
    }

    /// The cells inside the envelope [`Shadow::under`] wrote.
    ///
    /// `beneath` is how far the session's cells are dissolved, per row: the
    /// overlay's opposite, for the notice; the frame's own shape, for a frame
    /// with an overlay.
    fn paint_under(
        &self,
        out: &mut Vec<u8>,
        over: &Over,
        palette: &Palette,
        ink: Rgb,
        t: f32,
        beneath: &dyn Fn(u16) -> f32,
    ) {
        let screen = self.screen();
        let cols = screen.size().1;
        // The overlay's glyphs while more visible, then the session's cells.
        let overlaid = t < CROSSOVER;
        let ink = Painted {
            fg: ink.lerp(palette.bg, t),
            bg: None,
            bold: false,
            dim: false,
            italic: false,
            underline: Underline::None,
            sp: None,
            flags: Flags::NONE,
        };
        // The stream's SGR, carried across rows as in `frame`.
        let mut sgr: Option<Painted> = None;
        for (i, row) in over.rows.iter().enumerate() {
            let Ok(line) = u16::try_from(i).map(|i| over.top + i) else {
                break;
            };
            let laid = laid_out(row, over.width);
            let glyph = |c: u16| {
                laid[usize::from(c)]
                    .as_deref()
                    .filter(|g| overlaid && !is_blank(g))
            };
            let mut at = At::Off;
            let mut c = 0u16;
            while c < over.width {
                let col = over.left + c;
                let mut drifting = false;
                // A flag's second half, written with its first.
                let mut rest = "";
                let (text, painted, width) = match glyph(c) {
                    // The overlay's glyph in its one colour; the notice sets
                    // no background, so the box keeps the reset's.
                    Some(g) => (g, ink, glyph_width(g)),
                    // The session's cell. A wide character or flag the
                    // rectangle cuts is blanked: half would be a broken
                    // glyph, and the other half is outside the rectangle.
                    None => {
                        let cell = screen.cell(line, col);
                        let painted = resolve(cell, palette, beneath(line));
                        // The next cell is the rectangle's, and not under a
                        // glyph of the overlay's.
                        let room = c + 1 < over.width && glyph(c + 1).is_none();
                        match cell {
                            Some(cell)
                                if cell.is_wide_continuation()
                                    || flag_end(screen.cells, line, col) =>
                            {
                                (" ", painted, 1)
                            }
                            Some(cell) if cell.is_wide() && c + 2 > over.width => (" ", painted, 1),
                            Some(cell) if cell.has_contents() => {
                                drifting = drifts(cell);
                                let half =
                                    drifting.then(|| flag(screen.cells, line, col)).flatten();
                                match half {
                                    Some(half) if room => {
                                        rest = half;
                                        (cell.contents(), painted, 2)
                                    }
                                    Some(_) => {
                                        drifting = false;
                                        (" ", painted, 1)
                                    }
                                    None => {
                                        let padded = drifting
                                            && room
                                            && pads(cell, screen.cell(line, col + 1));
                                        let w = if cell.is_wide() || padded { 2 } else { 1 };
                                        (cell.contents(), painted, w)
                                    }
                                }
                            }
                            _ => (" ", painted, 1),
                        }
                    }
                };
                place(out, (line, col), at);
                if sgr != Some(painted) {
                    write_sgr(out, painted);
                    sgr = Some(painted);
                }
                // In the cell's pen, which a pad's looks like here: the same
                // pen and dissolve.
                if drifting && width == 2 {
                    clear_two(out, col, cols);
                }
                out.extend_from_slice(text.as_bytes());
                out.extend_from_slice(rest.as_bytes());
                at = if drifting {
                    At::Astray
                } else {
                    At::Col(col + width)
                };
                c += width;
            }
        }
    }
}

impl Shadow {
    /// A frame of the session with each of `laid` (a floating screen and its
    /// hint row) over it and every other cell as `shade` says. With none laid,
    /// the cells where they go are shaded too, which is how they blank for the
    /// screen to come up on.
    ///
    /// A diff like [`Shadow::frame`]'s, in full after a [`feed`], [`resize`]
    /// or [`invalidate`] (so after any session output). Always at least the
    /// sync span, cursor hide and trailing SGR reset.
    ///
    /// A wide character cut by either edge of a rectangle is blanked.
    /// `Shade::Toward` without a palette is drawn faint.
    ///
    /// [`feed`]: Shadow::feed
    /// [`resize`]: Shadow::resize
    /// [`invalidate`]: Shadow::invalidate
    pub fn frame_under(
        &mut self,
        shade: &dyn Fn(u16, u16) -> Shade,
        palette: Option<&Palette>,
        laid: &[Laid],
        cursor: Cursor,
    ) -> Vec<u8> {
        // Not `self.screen()`: `under` is written while the screen is read.
        let screen = Screen::of(&self.parser, &self.twin);
        let (rows, cols) = if self.broken { (0, 0) } else { screen.size() };
        let total = usize::from(rows) * usize::from(cols);
        let diff = self.under_valid && self.under.len() == total;
        if self.under.len() != total {
            self.under.clear();
            self.under.resize(total, Drawn::NOTHING);
        }
        let mut out = Vec::new();
        out.extend_from_slice(SYNC_BEGIN);
        out.extend_from_slice(HIDE_CURSOR);
        let mut pen: Option<Pen> = None;
        // Which of the row's cells show nothing (see `Run`).
        let mut bare = vec![false; usize::from(cols)];
        for row in 0..rows {
            let mut at = At::Off;
            let mut run = Run::default();
            // The previous cell was a session wide character, or a flag's
            // first half (see `flag`), written whole; this is its second half.
            let mut wide_before = false;
            // This cell is the blank the one before was drawn over, written
            // with it (see `pads`).
            let mut padded = false;
            // The previous cell began a flag, whatever was drawn there.
            let mut flagged = false;
            // A drifting cell was just written beside a rectangle: drawn two
            // wide it ran over this cell, so this one is written again.
            let mut again = false;
            for col in 0..cols {
                let cell = screen.cell(row, col);
                let inside = laid.iter().find_map(|l| l.at(row, col));
                let rewrite = std::mem::take(&mut again);
                let second = std::mem::take(&mut flagged);
                let rest = if second {
                    None
                } else {
                    flag(screen.cells, row, col)
                };
                flagged = rest.is_some();
                let drawn = match inside {
                    Some(inside) => {
                        wide_before = false;
                        inside
                    }
                    None if second || cell.is_some_and(|c| c.is_wide_continuation()) => {
                        let written = std::mem::take(&mut wide_before);
                        (!written).then_some((" ", Pen::BLANK))
                    }
                    None => {
                        let wide = rest.is_some() || cell.is_some_and(|c| c.is_wide());
                        let cut =
                            wide && (col + 2 > cols || laid.iter().any(|l| l.holds(row, col + 1)));
                        let drawn = shaded(cell, shade(row, col), palette, cut);
                        // A flag's blank leaves its second half to blank too,
                        // as that is a cell of its own on some terminals.
                        wide_before = wide && !cut && (rest.is_none() || drawn.0 != " ");
                        Some(drawn)
                    }
                };
                let index = usize::from(row) * usize::from(cols) + usize::from(col);
                let same = diff && self.under[index].is(drawn) && !rewrite;
                if !same {
                    self.under[index] = Drawn::of(drawn);
                }
                if std::mem::take(&mut padded) {
                    bare[usize::from(col)] = false;
                    continue;
                }
                // A session cell drawn as its text.
                let drifting = inside.is_none()
                    && drawn.is_some_and(|(text, _)| text != " ")
                    && cell.is_some_and(drifts);
                let next = screen.cell(row, col + 1);
                let pad = (drifting
                    && !laid.iter().any(|l| l.holds(row, col + 1))
                    && cell.is_some_and(|c| pads(c, next)))
                .then(|| shaded(next, shade(row, col + 1), palette, false));
                let same = same && pad.is_none_or(|pad| self.under[index + 1].is(Some(pad)));
                padded = pad.is_some();
                bare[usize::from(col)] =
                    drawn.is_some_and(|(text, this)| text == " " && this.shows_nothing());
                // An owed run absorbs the unchanged blanks after it: one erase,
                // not several.
                if bare[usize::from(col)] && (!same || !run.is_empty()) {
                    run.push(col);
                    continue;
                }
                run.flush(&mut out, (row, cols), &mut at, |out| {
                    blank_pen(out, &mut pen)
                });
                let Some((text, this)) = drawn else {
                    continue;
                };
                if same {
                    continue;
                }
                if at != At::Col(col) {
                    let over = pen.is_some_and(|p| p.shows_nothing());
                    move_to(&mut out, (row, col), at, over, &bare);
                }
                // Blanked in the blank's own pen where spaces in the cell's
                // look otherwise, as shades differ cell to cell.
                let clear = match pad {
                    Some((_, blank)) if !blank.looks_like(&this) => Some(blank),
                    Some(_) => Some(this),
                    None => {
                        let two = rest.is_some() || cell.is_some_and(|c| c.is_wide());
                        (drifting && two).then_some(this)
                    }
                };
                if let Some(blank) = clear {
                    if pen != Some(blank) {
                        blank.write(&mut out);
                        pen = Some(blank);
                    }
                    clear_two(&mut out, col, cols);
                }
                if pen != Some(this) {
                    this.write(&mut out);
                    pen = Some(this);
                }
                out.extend_from_slice(text.as_bytes());
                if drifting {
                    out.extend_from_slice(rest.unwrap_or_default().as_bytes());
                    again = laid.iter().any(|l| l.holds(row, col + 1));
                }
                let width = u16::try_from(UnicodeWidthStr::width(text).max(1)).unwrap_or(1);
                at = if drifting {
                    At::Astray
                } else {
                    At::Col(col + width)
                };
            }
            run.flush(&mut out, (row, cols), &mut at, |out| {
                blank_pen(out, &mut pen)
            });
        }
        out.extend_from_slice(RESET_SGR);
        if cursor == Cursor::Restored && !self.broken && !screen.hide_cursor() {
            let (row, col) = screen.cursor_position();
            let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);
            out.extend_from_slice(SHOW_CURSOR);
        }
        out.extend_from_slice(SYNC_END);
        self.under_valid = true;
        // Written behind a fade's diff, which must start again.
        self.painted = Vec::new();
        out
    }
}

/// The session's screen as its parser holds it, and the twin if there is one.
#[derive(Clone, Copy)]
struct Screen<'a> {
    cells: &'a vt100::Screen,
    twin: Option<&'a vt100::Screen>,
}

impl<'a> Screen<'a> {
    fn of(parser: &'a vt100::Parser<Watch>, twin: &'a Option<vt100::Parser>) -> Self {
        Self {
            cells: parser.screen(),
            twin: twin.as_ref().map(vt100::Parser::screen),
        }
    }

    fn cell(self, row: u16, col: u16) -> Option<Cell<'a>> {
        Some(Cell {
            cell: self.cells.cell(row, col)?,
            twin: self.twin,
            at: (row, col),
        })
    }
}

impl std::ops::Deref for Screen<'_> {
    type Target = vt100::Screen;

    fn deref(&self) -> &vt100::Screen {
        self.cells
    }
}

/// A cell of the parser's, and where a twin has its own, looked up only when
/// asked for. `Deref` reaches `vt100`'s one bit of underline: ask
/// [`Cell::extra`].
#[derive(Clone, Copy)]
struct Cell<'a> {
    cell: &'a vt100::Cell,
    twin: Option<&'a vt100::Screen>,
    at: (u16, u16),
}

impl<'a> Cell<'a> {
    /// Borrowed from the grid, not from this copy as through `Deref`.
    fn contents(self) -> &'a str {
        self.cell.contents()
    }

    fn extra(self) -> Extra {
        let (row, col) = self.at;
        let twin = self.twin.and_then(|twin| twin.cell(row, col));
        let held = twin.map_or(Held::NONE, |c| Held::read(c.fgcolor(), c.bgcolor()));
        held.over(self.cell.underline())
    }
}

impl std::ops::Deref for Cell<'_> {
    type Target = vt100::Cell;

    fn deref(&self) -> &vt100::Cell {
        self.cell
    }
}

/// What `vt100` drops of a pen: the underline's style and its colour (the
/// default is the text's), and the [`Flags`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Extra {
    underline: Underline,
    sp: vt100::Color,
    flags: Flags,
}

impl Extra {
    const NONE: Extra = Extra {
        underline: Underline::None,
        sp: vt100::Color::Default,
        flags: Flags::NONE,
    };

    /// Whether a space shows it: a line under, through or over it.
    fn lined(self) -> bool {
        self.underline != Underline::None || self.flags.lined()
    }
}

/// What a twin holds for a cell or for the session's pen: only what `vt100`
/// cannot. So an underline's style is here only when a `4:n` set it, and
/// with nothing held the twin's pen is the default one.
///
/// The twin's foreground is the underline's colour. Its background is an
/// indexed colour, or the default for 0: in the low three bits the style as
/// SGR `4:n` numbers it, plus one, and the flags above them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Held {
    /// `None` leaves it to `vt100`'s bit: underlined by a plain `4`, or not.
    underline: Option<Underline>,
    sp: vt100::Color,
    flags: Flags,
}

/// The attributes `vt100` has no bit for, a bit each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Flags(u8);

impl Flags {
    const NONE: Flags = Flags(0);
    const BLINK: Flags = Flags(1);
    const CONCEAL: Flags = Flags(1 << 1);
    const STRIKETHROUGH: Flags = Flags(1 << 2);
    const ALTFONT: Flags = Flags(1 << 3);
    const OVERLINE: Flags = Flags(1 << 4);

    /// Each with the SGR parameter that sets it and the one that clears it.
    const ALL: [(Flags, u8, u8); 5] = [
        (Flags::BLINK, 5, 25),
        (Flags::CONCEAL, 8, 28),
        (Flags::STRIKETHROUGH, 9, 29),
        (Flags::ALTFONT, 11, 10),
        (Flags::OVERLINE, 53, 55),
    ];

    fn has(self, flag: Flags) -> bool {
        self.0 & flag.0 != 0
    }

    /// Whether a space shows them: a line through it or over it.
    fn lined(self) -> bool {
        self.has(Flags::STRIKETHROUGH) || self.has(Flags::OVERLINE)
    }

    /// After an SGR parameter.
    fn after(mut self, code: u32) -> Self {
        for (flag, set, clear) in Flags::ALL {
            if code == u32::from(set) {
                self.0 |= flag.0;
            } else if code == u32::from(clear) {
                self.0 &= !flag.0;
            }
        }
        self
    }

    /// As SGR parameters, each after a `;`.
    fn write(self, out: &mut Vec<u8>) {
        for (flag, set, _) in Flags::ALL {
            if self.has(flag) {
                let _ = write!(out, ";{set}");
            }
        }
    }
}

/// Underline styles by their number in SGR `4:n`.
const STYLES: [Underline; 6] = [
    Underline::None,
    Underline::Line,
    Underline::Double,
    Underline::Curl,
    Underline::Dotted,
    Underline::Dashed,
];

impl Held {
    const NONE: Held = Held {
        underline: None,
        sp: vt100::Color::Default,
        flags: Flags::NONE,
    };

    fn read(fg: vt100::Color, bg: vt100::Color) -> Self {
        let n = match bg {
            vt100::Color::Idx(n) => n,
            _ => 0,
        };
        let style = usize::from(n & 7).checked_sub(1);
        Held {
            underline: style.and_then(|n| STYLES.get(n).copied()),
            sp: fg,
            flags: Flags(n >> 3),
        }
    }

    fn of_pen(twin: &vt100::Screen) -> Self {
        Self::read(twin.fgcolor(), twin.bgcolor())
    }

    /// With `vt100`'s bit of underline, all it dropped. An underline's
    /// colour is kept only with an underline to draw in it.
    fn over(self, underlined: bool) -> Extra {
        let plain = if underlined {
            Underline::Line
        } else {
            Underline::None
        };
        let underline = self.underline.unwrap_or(plain);
        Extra {
            underline,
            sp: if underline == Underline::None {
                vt100::Color::Default
            } else {
                self.sp
            },
            flags: self.flags,
        }
    }

    /// The SGR that makes the twin's pen stand for it.
    fn write(self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b[0");
        write_colour(out, self.sp, 30, 90, 38);
        let style = self
            .underline
            .and_then(|style| STYLES.iter().position(|s| *s == style))
            .map_or(0, |n| n as u8 + 1);
        let n = style | self.flags.0 << 3;
        if n > 0 {
            write_colour(out, vt100::Color::Idx(n), 40, 100, 48);
        }
        out.push(b'm');
    }

    /// As the pen holds it after an SGR with these parameters (the bytes
    /// between `CSI` and `m`). Reads both forms of a colour, `58:2::r:g:b`
    /// and `58;2;r;g;b`.
    fn after(mut self, params: &[u8]) -> Self {
        let mut params = params.split(|&b| b == b';');
        while let Some(param) = params.next() {
            let mut subs = param.split(|&b| b == b':').map(number);
            let code = subs.next().unwrap_or(0);
            match (code, subs.next()) {
                (0, None) => self = Held::NONE,
                // `vt100`'s own to hold, from here.
                (4 | 24, None) => self.underline = None,
                (4, Some(n)) => {
                    let style = STYLES.get(n as usize).copied();
                    self.underline = Some(style.unwrap_or(Underline::Line));
                }
                (59, None) => self.sp = vt100::Color::Default,
                (58, Some(kind)) => {
                    let mut arguments = [0; 4];
                    let mut n = 0;
                    for (to, sub) in arguments.iter_mut().zip(subs) {
                        *to = sub;
                        n += 1;
                    }
                    // Red, green and blue may follow a colour space.
                    let from = usize::from(kind == 2 && n == 4);
                    self.sp = colour(kind, &arguments[from..n]).unwrap_or(self.sp);
                }
                // A colour's arguments are no parameters of their own: the 4
                // of `38;5;4` underlines nothing. `vt100` gives up on the SGR
                // at a colour it cannot read.
                (38 | 48 | 58, None) => {
                    let Some(colour) = colour_arguments(&mut params) else {
                        return self;
                    };
                    if code == 58 {
                        self.sp = colour;
                    }
                }
                (code, None) => self.flags = self.flags.after(code),
                _ => {}
            }
        }
        self
    }
}

/// A decimal SGR parameter, digits only; empty is 0.
fn number(digits: &[u8]) -> u32 {
    digits.iter().fold(0u32, |n, digit| {
        n.saturating_mul(10).saturating_add(u32::from(digit - b'0'))
    })
}

/// A colour of `kind` 5, by its index, or 2, by its red, green and blue.
fn colour(kind: u32, arguments: &[u32]) -> Option<vt100::Color> {
    let byte = |n: u32| u8::try_from(n).ok();
    match (kind, arguments) {
        (5, &[n]) => Some(vt100::Color::Idx(byte(n)?)),
        (2, &[r, g, b]) => Some(vt100::Color::Rgb(byte(r)?, byte(g)?, byte(b)?)),
        _ => None,
    }
}

/// The colour a `38`, `48` or `58` takes from the parameters after it: `5;n`
/// or `2;r;g;b`.
fn colour_arguments<'a>(params: &mut impl Iterator<Item = &'a [u8]>) -> Option<vt100::Color> {
    let mut next = || params.next().filter(|p| !p.contains(&b':')).map(number);
    let kind = next()?;
    let mut arguments = [0; 3];
    let n = match kind {
        5 => 1,
        2 => 3,
        _ => return None,
    };
    for argument in &mut arguments[..n] {
        *argument = next()?;
    }
    colour(kind, &arguments[..n])
}

/// A parser's colour as the client's writer takes one.
fn styled(colour: vt100::Color) -> style::Color {
    match colour {
        vt100::Color::Default => style::Color::Default,
        vt100::Color::Idx(n) => style::Color::Index(n),
        vt100::Color::Rgb(r, g, b) => style::Color::Rgb(Rgb(r, g, b)),
    }
}

/// Where each SGR in `bytes` starts and where its `m` is. Only `CSI`,
/// parameters, `m`: with a private marker or an intermediate byte it is
/// something else (`CSI > 4 ; 2 m` sets a key mode).
fn sgrs(bytes: &[u8]) -> impl Iterator<Item = (usize, usize)> + '_ {
    memchr::memchr_iter(0x1b, bytes).filter_map(move |esc| {
        let params = bytes[esc + 1..].strip_prefix(b"[")?;
        let len = params.iter().position(|b| !is_parameter(*b))?;
        (params[len] == b'm').then_some((esc, esc + 2 + len))
    })
}

fn is_parameter(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b';' | b':')
}

/// How much the twin is fed between looks at whether it still holds
/// anything. A look reads every cell at most, a few hundredths of what
/// feeding this much costs.
const TWIN_CHECK: usize = 1 << 20;

/// Watches the parser for the first SGR that sets what only a twin can hold,
/// and keeps the screen as that SGR found it, for the twin to start from.
#[derive(Default)]
struct Watch {
    /// Whether there is no twin.
    armed: bool,
    first: Option<First>,
}

/// The first SGR a twin is needed for.
struct First {
    /// The parser's screen, part way through the SGR.
    screen: vt100::Screen,
    /// Its parameters as [`canonical`] writes them.
    params: Vec<u8>,
}

impl vt100::Callbacks for Watch {
    /// Called for an SGR with a parameter `vt100` does not know, once for
    /// each such parameter, with all of the SGR's.
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        private: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        if !self.armed || self.first.is_some() || c != 'm' || private.is_some() {
            return;
        }
        let params: Vec<String> = params
            .iter()
            .map(|subs| {
                let subs: Vec<String> = subs.iter().map(u16::to_string).collect();
                subs.join(":")
            })
            .collect();
        let params = params.join(";").into_bytes();
        // One that only clears, such as `59`, needs no twin.
        if Held::NONE.after(&params) != Held::NONE {
            self.first = Some(First {
                screen: screen.clone(),
                params,
            });
        }
    }
}

impl First {
    /// The twin from this SGR on, given the feed that held it and the
    /// parser's screen after that feed.
    fn twin(self, bytes: &[u8], after: &vt100::Screen) -> vt100::Parser {
        // No earlier SGR has these parameters, or it would have been first.
        let found = sgrs(bytes).find(|&(start, m)| canonical(&bytes[start + 2..m]) == self.params);
        let Some((start, _)) = found else {
            // Longer than `SGR_MAX` and cut by a read, so its start is not
            // in this feed. The twin starts after the feed with the pen it
            // left, and what the feed drew stays without until drawn again.
            let pen = sgrs(bytes).fold(Held::NONE.after(&self.params), |pen, (start, m)| {
                pen.after(&bytes[start + 2..m])
            });
            let mut twin = blank_twin(after.clone());
            let mut sgr = Vec::new();
            pen.write(&mut sgr);
            twin.process(&sgr);
            return twin;
        };
        let mut twin = blank_twin(self.screen);
        follow(&mut twin, &bytes[start..]);
        twin
    }
}

/// SGR parameters with each number as `vt100` reads it, to compare.
fn canonical(params: &[u8]) -> Vec<u8> {
    let params: Vec<String> = params
        .split(|&b| b == b';')
        .map(|param| {
            let subs: Vec<String> = param
                .split(|&b| b == b':')
                .map(|sub| u16::try_from(number(sub)).unwrap_or(u16::MAX).to_string())
                .collect();
            subs.join(":")
        })
        .collect();
    params.join(";").into_bytes()
}

/// A parser in `screen`'s state with every cell blank, as none has yet had
/// what a twin holds.
fn blank_twin(screen: vt100::Screen) -> vt100::Parser {
    let (rows, cols) = screen.size();
    let alternate = screen.alternate_screen();
    let mut twin = vt100::Parser::new(rows, cols, 0);
    *twin.screen_mut() = screen;
    // Erased under a default pen: this screen, then the primary one behind
    // an alternate. `?47` switches without moving either cursor.
    twin.process(b"\x1b[0m\x1b[2J");
    if alternate {
        twin.process(b"\x1b[?47l\x1b[2J\x1b[?47h");
    }
    twin
}

/// Whether neither the twin's pen nor any cell with text holds anything.
fn holds_nothing(twin: &vt100::Screen) -> bool {
    let plain = |fg, bg| fg == vt100::Color::Default && bg == vt100::Color::Default;
    let (rows, cols) = twin.size();
    plain(twin.fgcolor(), twin.bgcolor())
        && (0..rows).all(|row| {
            (0..cols).all(|col| {
                twin.cell(row, col)
                    .is_none_or(|c| !c.has_contents() || plain(c.fgcolor(), c.bgcolor()))
            })
        })
}

/// Feed `bytes` to the parser, and to its twin while it needs one.
fn track(
    parser: &mut vt100::Parser<Watch>,
    twin: &mut Option<vt100::Parser>,
    fed: &mut usize,
    bytes: &[u8],
) {
    parser.callbacks_mut().armed = twin.is_none();
    parser.process(bytes);
    let Some(there) = twin else {
        if let Some(first) = parser.callbacks_mut().first.take() {
            let born = first.twin(bytes, parser.screen());
            // Neovim's client sets a wavy underline and draws nothing, to
            // ask the terminal whether it can.
            *twin = Some(born).filter(|twin| !holds_nothing(twin.screen()));
            *fed = 0;
        }
        return;
    };
    follow(there, bytes);
    *fed += bytes.len();
    if *fed >= TWIN_CHECK {
        *fed = 0;
        if holds_nothing(there.screen()) {
            *twin = None;
        }
    }
}

/// Feed the twin `bytes`, but for each SGR the one that makes its pen stand
/// for what the session's pen has of [`Held`] after it.
fn follow(twin: &mut vt100::Parser, bytes: &[u8]) {
    let mut fed = 0;
    let mut sgr = Vec::new();
    for (start, m) in sgrs(bytes) {
        twin.process(&bytes[fed..start]);
        fed = m + 1;
        let was = Held::of_pen(twin.screen());
        let now = was.after(&bytes[start + 2..m]);
        // Only the twin's own SGRs set its pen, so it stands for `was`.
        if now != was {
            sgr.clear();
            now.write(&mut sgr);
            twin.process(&sgr);
        }
    }
    twin.process(&bytes[fed..]);
}

/// A session cell as [`Shadow::frame_under`] draws it: its text (blank if
/// `cut`, a wide character a rectangle's or the screen's edge cuts) and its
/// pen as `shade` says.
fn shaded<'a>(
    cell: Option<Cell<'a>>,
    shade: Shade,
    palette: Option<&Palette>,
    cut: bool,
) -> (&'a str, Pen) {
    let text = match cell {
        Some(c) if c.has_contents() && !cut => c.contents(),
        _ => " ",
    };
    // A space `resolve` would give no background or line at any `t`: most
    // cells, which end as the blank pen, so skip resolving colours.
    let bare = text == " "
        && cell.is_none_or(|c| {
            c.bgcolor() == vt100::Color::Default
                && !c.inverse()
                && !(c.has_contents() && c.extra().lined())
        });
    match (shade, palette) {
        (Shade::Blank, _) => (" ", Pen::BLANK),
        (Shade::Toward(t), _) if t >= falloff::EMPTIED => (" ", Pen::BLANK),
        (Shade::Toward(_), Some(_)) if bare => (" ", Pen::BLANK),
        (Shade::Toward(t), Some(palette)) => {
            let pen = Pen::painted(resolve(cell, palette, t));
            // A space without a line or background shows no colour, so use
            // the blank pen rather than an SGR per falloff shade.
            let bare = text == " " && pen.shows_nothing();
            (text, if bare { Pen::BLANK } else { pen })
        }
        // Faint, or no palette: the cell's own colours, its text faint.
        (Shade::Toward(_) | Shade::Faint, _) => {
            let mut pen = Pen::of(cell);
            pen.dim |= cell.is_some_and(|c| c.has_contents());
            (text, pen)
        }
    }
}

/// A cell's colours and attributes as the session gave them, as
/// [`Shadow::paint`] writes them; compared to skip rewriting the SGR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pen {
    fg: vt100::Color,
    bg: vt100::Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: Underline,
    /// The underline's colour; the default is the text's.
    sp: vt100::Color,
    flags: Flags,
    inverse: bool,
}

impl Pen {
    /// Default colours, no attributes: a blank cell.
    const BLANK: Pen = Pen {
        fg: vt100::Color::Default,
        bg: vt100::Color::Default,
        bold: false,
        dim: false,
        italic: false,
        underline: Underline::None,
        sp: vt100::Color::Default,
        flags: Flags::NONE,
        inverse: false,
    };

    /// Whether a space in this pen is invisible: default background, not
    /// inverse, no line under, through or over it.
    fn shows_nothing(&self) -> bool {
        self.bg == vt100::Color::Default
            && self.underline == Underline::None
            && !self.flags.lined()
            && !self.inverse
    }

    /// Whether a space in this pen looks as one in `other` does.
    fn looks_like(&self, other: &Self) -> bool {
        self == other || (self.shows_nothing() && other.shows_nothing())
    }

    /// A fade cell in its interpolated colours, a default background left to
    /// the terminal.
    fn painted(p: Painted) -> Self {
        let rgb = |c: Rgb| vt100::Color::Rgb(c.0, c.1, c.2);
        Pen {
            fg: rgb(p.fg),
            bg: p.bg.map_or(vt100::Color::Default, rgb),
            bold: p.bold,
            dim: p.dim,
            italic: p.italic,
            underline: p.underline,
            sp: p.sp.map_or(vt100::Color::Default, rgb),
            flags: p.flags,
            inverse: false,
        }
    }

    /// A cell of an nvmux screen as ratatui drew it: modifiers, and any true
    /// colours a theme or fade gave it.
    fn of_buffer(cell: &ratatui::buffer::Cell) -> Self {
        use ratatui::style::{Color, Modifier};
        let colour = |c: Color| match c {
            Color::Reset => vt100::Color::Default,
            Color::Rgb(r, g, b) => vt100::Color::Rgb(r, g, b),
            Color::Indexed(n) => vt100::Color::Idx(n),
            Color::Black => vt100::Color::Idx(0),
            Color::Red => vt100::Color::Idx(1),
            Color::Green => vt100::Color::Idx(2),
            Color::Yellow => vt100::Color::Idx(3),
            Color::Blue => vt100::Color::Idx(4),
            Color::Magenta => vt100::Color::Idx(5),
            Color::Cyan => vt100::Color::Idx(6),
            Color::Gray => vt100::Color::Idx(7),
            Color::DarkGray => vt100::Color::Idx(8),
            Color::LightRed => vt100::Color::Idx(9),
            Color::LightGreen => vt100::Color::Idx(10),
            Color::LightYellow => vt100::Color::Idx(11),
            Color::LightBlue => vt100::Color::Idx(12),
            Color::LightMagenta => vt100::Color::Idx(13),
            Color::LightCyan => vt100::Color::Idx(14),
            Color::White => vt100::Color::Idx(15),
        };
        let m = cell.modifier;
        Pen {
            fg: colour(cell.fg),
            bg: colour(cell.bg),
            bold: m.contains(Modifier::BOLD),
            dim: m.contains(Modifier::DIM),
            italic: m.contains(Modifier::ITALIC),
            underline: if m.contains(Modifier::UNDERLINED) {
                Underline::Line
            } else {
                Underline::None
            },
            sp: vt100::Color::Default,
            flags: if m.contains(Modifier::CROSSED_OUT) {
                Flags::STRIKETHROUGH
            } else {
                Flags::NONE
            },
            inverse: m.contains(Modifier::REVERSED),
        }
    }

    /// The cell's own, or the terminal's defaults for a cell past the grid.
    fn of(cell: Option<Cell>) -> Self {
        let Some(c) = cell else {
            return Pen::BLANK;
        };
        let extra = c.extra();
        Pen {
            fg: c.fgcolor(),
            bg: c.bgcolor(),
            bold: c.bold(),
            dim: c.dim(),
            italic: c.italic(),
            underline: extra.underline,
            sp: extra.sp,
            flags: extra.flags,
            inverse: c.inverse(),
        }
    }

    /// The session's own, as its parsers have it.
    fn of_session(screen: Screen) -> Self {
        let held = screen.twin.map_or(Held::NONE, Held::of_pen);
        let extra = held.over(screen.underline());
        Pen {
            fg: screen.fgcolor(),
            bg: screen.bgcolor(),
            bold: screen.bold(),
            dim: screen.dim(),
            italic: screen.italic(),
            underline: extra.underline,
            sp: extra.sp,
            flags: extra.flags,
            inverse: screen.inverse(),
        }
    }

    /// One SGR from a reset, so default colours go unmentioned and indexed
    /// ones stay indices for the terminal's palette; then the underline's
    /// style and colour, as the client writes them.
    fn write(self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b[0");
        for (on, code) in [
            (self.bold, 1),
            (self.dim, 2),
            (self.italic, 3),
            (self.underline != Underline::None, 4),
            (self.inverse, 7),
        ] {
            if on {
                let _ = write!(out, ";{code}");
            }
        }
        self.flags.write(out);
        write_colour(out, self.fg, 30, 90, 38);
        write_colour(out, self.bg, 40, 100, 48);
        out.push(b'm');
        writer::underline(out, self.underline, styled(self.sp));
    }
}

/// What [`Shadow::frame_under`] last painted in a cell: its text inline, and
/// its pen. Kept per cell across frames, so a frame allocates nothing per
/// cell.
#[derive(Debug, Clone, Copy)]
struct Drawn {
    /// Bytes of `text` used; [`Drawn::NONE`] for nothing written,
    /// [`Drawn::LONG`] if it did not fit.
    len: u8,
    /// As much as a `vt100` cell holds, so all session text fits.
    text: [u8; DRAWN_BYTES],
    pen: Pen,
}

/// The most text a `vt100` cell holds.
const DRAWN_BYTES: usize = 22;

impl Drawn {
    /// The second half of a wide character, which its first half wrote.
    const NONE: u8 = u8::MAX;
    /// Text too long to keep (only a [`Laid`] cluster could be); never
    /// matches, so it is always written.
    const LONG: u8 = u8::MAX - 1;
    const NOTHING: Drawn = Drawn {
        len: Self::NONE,
        text: [0; DRAWN_BYTES],
        pen: Pen::BLANK,
    };

    fn of(cell: Option<(&str, Pen)>) -> Self {
        let Some((text, pen)) = cell else {
            return Self::NOTHING;
        };
        let mut kept = [0; DRAWN_BYTES];
        let len = match kept.get_mut(..text.len()) {
            Some(room) => {
                room.copy_from_slice(text.as_bytes());
                text.len() as u8
            }
            None => Self::LONG,
        };
        Drawn {
            len,
            text: kept,
            pen,
        }
    }

    fn is(&self, cell: Option<(&str, Pen)>) -> bool {
        match cell {
            None => self.len == Self::NONE,
            Some((text, pen)) => {
                usize::from(self.len) == text.len()
                    && self.pen == pen
                    && self.text.get(..text.len()) == Some(text.as_bytes())
            }
        }
    }
}

/// Shortest run of empty cells erased with `ECH` rather than written as
/// spaces: an erase leaves the cursor in place, and the `CUP` the next cell
/// then needs costs about this many spaces.
const ECH_FROM: u16 = 12;

/// Most empty cells written over as spaces to reach the next cell, rather
/// than a `CUP`, which costs about as much.
const GAP: u16 = 7;

/// Where a painter has left the cursor on the row it paints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum At {
    /// Not on the row yet.
    Off,
    /// On the row, after a cell a terminal may draw at another width
    /// ([`drifts`]).
    Astray,
    /// At this column.
    Col(u16),
}

/// Empty cells a frame owes the terminal: start column and count, flushed
/// when a visible cell comes or the row ends (see [`Run::flush`]). The pen is
/// first set to one with no background, so an erase gives the terminal's own
/// background however the terminal erases.
#[derive(Debug, Default)]
struct Run(Option<(u16, u16)>);

impl Run {
    /// Extend the run by the next column, or start one at `col`.
    fn push(&mut self, col: u16) {
        match &mut self.0 {
            Some((_, n)) => *n += 1,
            None => self.0 = Some((col, 1)),
        }
    }

    fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    /// Write what is owed on `row` (screen `cols` wide) from the cursor at
    /// `at`, updating `at`: spaces if short, else erased with `EL` to the
    /// row's end or `ECH` short of it, neither moving the cursor. `blank`
    /// first sets a pen that shows nothing.
    fn flush(
        &mut self,
        out: &mut Vec<u8>,
        (row, cols): (u16, u16),
        at: &mut At,
        blank: impl FnOnce(&mut Vec<u8>),
    ) {
        let Some((start, n)) = self.0.take() else {
            return;
        };
        place(out, (row, start), *at);
        blank(out);
        if start + n == cols && n > 3 {
            out.extend_from_slice(b"\x1b[K");
            *at = At::Col(start);
        } else if n >= ECH_FROM {
            let _ = write!(out, "\x1b[{n}X");
            *at = At::Col(start);
        } else {
            out.extend(std::iter::repeat_n(b' ', usize::from(n)));
            *at = At::Col(start + n);
        }
    }
}

/// Move the cursor from `at` to `col` on `row`: by writing spaces over the
/// cells between if they all show nothing (`bare`), the pen does too
/// (`over`), and there are at most [`GAP`]; otherwise as [`place`] does.
fn move_to(out: &mut Vec<u8>, (row, col): (u16, u16), at: At, over: bool, bare: &[bool]) {
    match at {
        At::Col(from)
            if over
                && from < col
                && col - from <= GAP
                && bare[usize::from(from)..usize::from(col)].iter().all(|&b| b) =>
        {
            out.extend(std::iter::repeat_n(b' ', usize::from(col - from)));
        }
        _ => place(out, (row, col), at),
    }
}

/// Put the cursor at `col` on `row`: nothing if `at` is there, by CHA from
/// [`At::Astray`], else by CUP.
fn place(out: &mut Vec<u8>, (row, col): (u16, u16), at: At) {
    match at {
        At::Col(c) if c == col => {}
        At::Astray => column(out, col),
        _ => {
            let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);
        }
    }
}

/// CHA: column `col` of the cursor's row, also ending a pending wrap.
fn column(out: &mut Vec<u8>, col: u16) {
    let _ = write!(out, "\x1b[{}G", col + 1);
}

/// Whether a terminal may draw `cell` at another width than the shadow holds
/// it, as the client's writer tests it ([`ambiwidth`]). Painters then place
/// the next cell absolutely, as the writer does.
fn drifts(cell: Cell) -> bool {
    cell.has_contents() && ambiwidth(cell.contents(), cell.is_wide())
}

/// Whether `next` is the blank a narrow drifting `cell` was drawn over: a
/// blank in its pen, as the writer draws a cell Neovim makes wide but vt100
/// counts narrow (❤️). Painters write the two together, the blank first, as
/// a space over the right half of a `cell` drawn wide would break it.
fn pads(cell: Cell, next: Option<Cell>) -> bool {
    !cell.is_wide()
        && next.is_some_and(|next| {
            !next.is_wide_continuation()
                && (!next.has_contents() || next.contents() == " ")
                && Pen::of(Some(next)) == Pen::of(Some(cell))
        })
}

/// The second half of the flag whose first is at (`row`, `col`), if one is.
///
/// Neovim's grid has a flag in one wide cell, and vt100 in two narrow ones,
/// one regional indicator each (a lone one has a pad, see [`pads`]).
/// Painters treat the two as one wide character, written with nothing
/// between its halves, as WezTerm joins code points only within a run of
/// text. Indicators pair from the start of a run, as Unicode pairs them.
fn flag(screen: &vt100::Screen, row: u16, col: u16) -> Option<&str> {
    let indicator = |col| {
        screen.cell(row, col).filter(|cell| {
            let mut chars = cell.contents().chars();
            chars
                .next()
                .is_some_and(|c| ('\u{1f1e6}'..='\u{1f1ff}').contains(&c))
                && chars.next().is_none()
        })
    };
    let rest = indicator(col + 1)?;
    indicator(col)?;
    let before = (0..col)
        .rev()
        .take_while(|&c| indicator(c).is_some())
        .count();
    (before % 2 == 0).then(|| rest.contents())
}

/// Whether (`row`, `col`) holds the second half of a flag ([`flag`]).
fn flag_end(screen: &vt100::Screen, row: u16, col: u16) -> bool {
    col.checked_sub(1)
        .is_some_and(|first| flag(screen, row, first).is_some())
}

/// Two spaces from `col`, the cursor's column, and back: a cell then drawn
/// there narrower than two columns leaves no stale right half.
fn clear_two(out: &mut Vec<u8>, col: u16, cols: u16) {
    out.extend_from_slice(b"  ");
    // Up to the last column leaves a wrap pending, where terminals disagree
    // on BS.
    if cols.saturating_sub(col) > 2 {
        out.extend_from_slice(b"\x08\x08");
    } else {
        column(out, col);
    }
}

/// Set the stream's SGR to one a fade's blank cells can be written or erased
/// in, unless it already shows nothing.
fn blank_sgr(out: &mut Vec<u8>, sgr: &mut Option<Painted>, palette: &Palette) {
    if !sgr.is_some_and(|p| p.shows_nothing()) {
        let blank = Painted::blank(palette);
        write_sgr(out, blank);
        *sgr = Some(blank);
    }
}

/// Set the pen to one that blank cells in a [`Shadow::frame_under`] frame can
/// be written or erased in, unless it already shows nothing.
fn blank_pen(out: &mut Vec<u8>, pen: &mut Option<Pen>) {
    if !pen.is_some_and(|p| p.shows_nothing()) {
        Pen::BLANK.write(out);
        *pen = Some(Pen::BLANK);
    }
}

/// A colour as SGR parameters: the eight as `base + n`, the bright eight as
/// `bright + n`, the rest of the 256 as `extended;5;n`, and a true colour as
/// `extended;2;r;g;b`. Nothing for the default.
fn write_colour(out: &mut Vec<u8>, colour: vt100::Color, base: u8, bright: u8, extended: u8) {
    match colour {
        vt100::Color::Default => {}
        vt100::Color::Idx(n) if n < 8 => {
            let _ = write!(out, ";{}", base + n);
        }
        vt100::Color::Idx(n) if n < 16 => {
            let _ = write!(out, ";{}", bright + n - 8);
        }
        vt100::Color::Idx(n) => {
            let _ = write!(out, ";{extended};5;{n}");
        }
        vt100::Color::Rgb(r, g, b) => {
            let _ = write!(out, ";{extended};2;{r};{g};{b}");
        }
    }
}

/// A cell a dissolve leaves standing: its text at full colour, its
/// background `t` of the way to `palette.bg`.
fn standing(cell: Option<Cell>, palette: &Palette, t: f32) -> Painted {
    Painted {
        bg: resolve(cell, palette, t).bg,
        ..resolve(cell, palette, 0.0)
    }
}

/// A cell `t` of the way to the background. Blank cells (nothing written, or
/// a space with no background and no line on it) get the background as
/// foreground and no attributes, so the diff skips them; Neovim underlines
/// blanks with written spaces, so those are glyphs. Inverse is applied here,
/// so the terminal never inverts an interpolated pair. So is dim once
/// `t > 0` (see [`DIM`]); at 0 it is the terminal's, as the session drew it.
fn resolve(cell: Option<Cell>, palette: &Palette, t: f32) -> Painted {
    let Some(cell) = cell else {
        return Painted::blank(palette);
    };
    // `None` for the default, which is not the same colour for each.
    let seen = |colour| match colour {
        vt100::Color::Default => None,
        vt100::Color::Idx(n) => Some(palette.index(n)),
        vt100::Color::Rgb(r, g, b) => Some(Rgb(r, g, b)),
    };
    let fg = seen(cell.fgcolor()).unwrap_or(palette.fg);
    let bg = seen(cell.bgcolor());
    let dim = cell.dim() && t <= 0.0;
    // Before inverse, as a terminal dims the foreground it is given.
    let fg = if cell.dim() && !dim {
        fg.lerp(bg.unwrap_or(palette.bg), DIM)
    } else {
        fg
    };
    let (fg, bg) = if cell.inverse() {
        (bg.unwrap_or(palette.bg), Some(fg))
    } else {
        (fg, bg)
    };
    let extra = if cell.has_contents() {
        cell.extra()
    } else {
        Extra::NONE
    };
    let blank = !cell.has_contents() || (cell.contents() == " " && bg.is_none() && !extra.lined());
    if blank {
        return Painted {
            bg: bg.map(|b| b.lerp(palette.bg, t)),
            ..Painted::blank(palette)
        };
    }
    Painted {
        fg: fg.lerp(palette.bg, t),
        bg: bg.map(|b| b.lerp(palette.bg, t)),
        bold: cell.bold(),
        dim,
        italic: cell.italic(),
        underline: extra.underline,
        sp: seen(extra.sp).map(|sp| sp.lerp(palette.bg, t)),
        flags: extra.flags,
    }
}

/// The text starting at each column of an overlay row (`None` for wide
/// characters' second halves and unreached columns). Combining marks stay
/// with their glyph, so decomposed characters in session names survive.
fn laid_out(row: &str, width: u16) -> Vec<Option<String>> {
    let mut out: Vec<Option<String>> = vec![None; usize::from(width)];
    let mut start: Option<usize> = None;
    let mut col = 0usize;
    for ch in row.chars() {
        let Some(w) = UnicodeWidthChar::width(ch).filter(|w| *w > 0) else {
            if let Some(g) = start.and_then(|c| out[c].as_mut()) {
                g.push(ch);
            }
            continue;
        };
        if col >= out.len() {
            break;
        }
        out[col] = Some(ch.to_string());
        start = Some(col);
        col += w;
    }
    out
}

/// The longest SGR a read may cut and still be held back for its end, in
/// bytes: twice the client's longest.
const SGR_MAX: usize = 128;

/// How much of `bytes` is whole: its characters, and short of an SGR its end
/// cuts off, as [`sgrs`] finds one only in one piece.
fn whole(bytes: &[u8]) -> usize {
    let n = whole_characters(bytes);
    let tail = &bytes[n.saturating_sub(SGR_MAX)..n];
    // An escape sequence with nothing after its parameters yet.
    let cut = memchr::memrchr(0x1b, tail).filter(|esc| match tail[esc + 1..].split_first() {
        None => true,
        Some((b'[', params)) => params.iter().all(|b| is_parameter(*b)),
        Some(_) => false,
    });
    cut.map_or(n, |esc| n - tail.len() + esc)
}

/// How much of `bytes` is whole characters. `vte` completes a partial
/// character with the next bytes and drops any it skips: a read ending inside
/// the `é` of `é ─` loses the space, shifting the rest of the row a cell left.
fn whole_characters(bytes: &[u8]) -> usize {
    let n = bytes.len();
    // The last character's first byte, at most four back.
    for back in 1..=n.min(4) {
        let b = bytes[n - back];
        if b & 0xc0 == 0x80 {
            continue;
        }
        let len = match b {
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => 1,
        };
        return if back < len { n - back } else { n };
    }
    n
}

/// An overlay glyph's width, never zero (layout by width guarantees one).
fn glyph_width(glyph: &str) -> u16 {
    glyph
        .chars()
        .filter_map(UnicodeWidthChar::width)
        .find(|w| *w > 0)
        .and_then(|w| u16::try_from(w).ok())
        .unwrap_or(1)
}

/// Whether an overlay leaves this cell unmarked: the notice's padding and box
/// interior, which hide the session's cells by leaving them to the dissolve.
fn is_blank(glyph: &str) -> bool {
    glyph.chars().all(char::is_whitespace)
}

/// One SGR for a cell, from a reset, so a default background goes
/// unmentioned; then its underline's style and colour, as the client writes
/// them.
fn write_sgr(out: &mut Vec<u8>, p: Painted) {
    out.extend_from_slice(b"\x1b[0");
    if p.bold {
        out.extend_from_slice(b";1");
    }
    if p.dim {
        out.extend_from_slice(b";2");
    }
    if p.italic {
        out.extend_from_slice(b";3");
    }
    if p.underline != Underline::None {
        out.extend_from_slice(b";4");
    }
    p.flags.write(out);
    let _ = write!(out, ";38;2;{};{};{}", p.fg.0, p.fg.1, p.fg.2);
    if let Some(bg) = p.bg {
        let _ = write!(out, ";48;2;{};{};{}", bg.0, bg.1, bg.2);
    }
    out.push(b'm');
    let sp = p.sp.map_or(style::Color::Default, style::Color::Rgb);
    writer::underline(out, p.underline, sp);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::contains;
    use std::panic;

    fn palette() -> Palette {
        Palette {
            fg: Rgb(200, 200, 200),
            bg: Rgb(0, 0, 0),
            ansi: [
                Rgb(0, 0, 0),
                Rgb(200, 0, 0),
                Rgb(0, 200, 0),
                Rgb(200, 200, 0),
                Rgb(0, 0, 200),
                Rgb(200, 0, 200),
                Rgb(0, 200, 200),
                Rgb(200, 200, 200),
                Rgb(100, 100, 100),
                Rgb(255, 0, 0),
                Rgb(0, 255, 0),
                Rgb(255, 255, 0),
                Rgb(0, 0, 255),
                Rgb(255, 0, 255),
                Rgb(0, 255, 255),
                Rgb(255, 255, 255),
            ],
        }
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8(bytes.to_vec()).expect("utf-8")
    }

    /// Every CSI in a frame as `(parameters, final byte)`, in order.
    fn csis(bytes: &[u8]) -> Vec<(String, char)> {
        text(bytes)
            .split('\x1b')
            .filter_map(|piece| {
                let body = piece.strip_prefix('[')?;
                let end = body.find(|c: char| c.is_ascii_alphabetic())?;
                Some((body[..end].to_string(), body[end..].chars().next()?))
            })
            .collect()
    }

    /// The `(row, col)` of every cursor placement in a frame, in order.
    fn placements(bytes: &[u8]) -> Vec<(usize, usize)> {
        csis(bytes)
            .into_iter()
            .filter(|(_, fin)| *fin == 'H')
            .filter_map(|(params, _)| {
                let (row, col) = params.split_once(';')?;
                Some((row.parse().ok()?, col.parse().ok()?))
            })
            .collect()
    }

    /// Every SGR in a frame, without its `ESC [` and `m`.
    fn sgrs(bytes: &[u8]) -> Vec<String> {
        csis(bytes)
            .into_iter()
            .filter(|(_, fin)| *fin == 'm')
            .map(|(params, _)| params)
            .collect()
    }

    /// One `((row, col), sgr, text)` per glyph a frame writes, 1-based. An SGR
    /// that does not reset joins the one before it, after a space.
    fn painted_cells(bytes: &[u8]) -> Vec<((usize, usize), String, String)> {
        let mut out = vec![];
        let mut at: Option<(usize, usize)> = None;
        let mut sgr = String::new();
        for piece in text(bytes).split('\x1b') {
            let Some(body) = piece.strip_prefix('[') else {
                continue;
            };
            let Some(end) = body.find(|c: char| c.is_ascii_alphabetic()) else {
                continue;
            };
            let fin = body[end..].chars().next().expect("a final byte");
            match fin {
                'H' => {
                    let (row, col) = body[..end].split_once(';').expect("row;col");
                    at = Some((row.parse().expect("row"), col.parse().expect("col")));
                }
                'm' if end == 0 || body.starts_with('0') => sgr = body[..end].to_string(),
                'm' => sgr = format!("{sgr} {}", &body[..end]),
                _ => {}
            }
            for ch in body[end + fin.len_utf8()..].chars() {
                let (row, col) = at.expect("a cell is placed before it is drawn");
                out.push(((row, col), sgr.clone(), ch.to_string()));
                at = Some((row, col + UnicodeWidthChar::width(ch).unwrap_or(1)));
            }
        }
        out
    }

    /// An 8-column box at the grid's row 2, column 3 (0-based).
    fn over() -> Over {
        Over {
            top: 2,
            left: 3,
            width: 8,
            rows: vec!["╭──────╮".into(), "│ hi   │".into(), "╰──────╯".into()],
        }
    }

    /// A dim log with a card's bold name on its second row.
    fn carded() -> (Shadow, Kept) {
        let mut shadow = Shadow::new(6, 20);
        for row in 1..=6 {
            shadow.feed(
                format!("\x1b[{row};1H\x1b[0;38;2;90;90;90;48;2;20;20;20mabcdefghijklmnopqrst")
                    .as_bytes(),
            );
        }
        shadow.feed(b"\x1b[2;13H\x1b[0;1;38;2;220;220;220;48;2;20;20;20m  blog\x1b[0m");
        let glyphs = [(1, 14, 'b'), (1, 15, 'l'), (1, 16, 'o'), (1, 17, 'g')];
        (shadow, Kept::new([(1, 12, 17)], glyphs))
    }

    #[test]
    fn what_stands_keeps_its_colour_as_the_screen_dissolves() {
        let (mut shadow, kept) = carded();
        let frame = shadow.frame_with(|_| 0.5, &palette(), Cursor::Hidden, None, &kept);
        let cells = painted_cells(&frame);
        let at = |row, col| {
            cells
                .iter()
                .find(|(at, _, _)| *at == (row, col))
                .map(|(_, sgr, text)| (sgr.clone(), text.clone()))
                .expect("painted")
        };
        assert_eq!(
            at(2, 15),
            (
                "0;1;38;2;220;220;220;48;2;10;10;10".to_string(),
                "b".to_string()
            ),
            "the card at full, its background half way"
        );
        assert_eq!(
            at(1, 15),
            ("0;38;2;45;45;45;48;2;10;10;10".to_string(), "o".to_string()),
            "the log half way"
        );
        assert_eq!(at(2, 12).0, "0;38;2;45;45;45;48;2;10;10;10", "beside it");
    }

    #[test]
    fn a_card_stands_only_where_the_screen_shows_it() {
        let (shadow, kept) = carded();
        assert!(shadow.shows(&kept));
        let (_, without) = carded();
        let mut bare = Shadow::new(6, 20);
        bare.feed(b"\x1b[2;1Habcdefghijklmnopqrst");
        assert!(!bare.shows(&without), "not drawn yet");
        assert!(
            !shadow.shows(&Kept::new([(1, 12, 17)], [])),
            "nothing to look for"
        );
    }

    #[test]
    fn a_card_on_the_move_stands_anywhere_along_its_way() {
        let blog = |col: usize| {
            (col..col + 4)
                .zip("blog".chars())
                .map(|(col, ch)| (1, col, ch))
        };
        let kept = Kept::new([(1, 8, 17)], []).moving(blog(14), -4);
        assert!(kept.moves());
        let at = |cols: &[u16]| {
            let mut shadow = Shadow::new(6, 20);
            shadow.feed(b"\x1b[2;1Habcdefghijklmnopqrst");
            for &col in cols {
                shadow.feed(format!("\x1b[2;{}Hblog", col + 1).as_bytes());
            }
            shadow.shows(&kept)
        };
        assert!(at(&[14]), "where it starts");
        assert!(at(&[12]), "on its way");
        assert!(at(&[10]), "where it ends");
        assert!(!at(&[9]), "not past its end");
        assert!(!at(&[]), "not drawn");
        assert!(
            !Kept::new([(1, 8, 17)], blog(14)).moves(),
            "nor what stands still"
        );
    }

    #[test]
    fn what_stands_is_drawn_again_as_it_stood() {
        let (shadow, kept) = carded();
        let bytes = shadow.kept_bytes(&kept, &palette());
        assert!(bytes.starts_with(SAVE_CURSOR) && bytes.ends_with(RESTORE_CURSOR));
        let drawn: Vec<_> = painted_cells(&bytes)
            .into_iter()
            .filter(|(_, _, text)| text != " ")
            .collect();
        let bold = "0;1;38;2;220;220;220".to_string();
        assert_eq!(
            drawn,
            [
                ((2, 15), bold.clone(), "b".to_string()),
                ((2, 16), bold.clone(), "l".to_string()),
                ((2, 17), bold.clone(), "o".to_string()),
                ((2, 18), bold, "g".to_string()),
            ]
        );
        assert!(shadow.kept_bytes(&Kept::default(), &palette()).is_empty());
    }

    /// A letter in every cell the box covers, so leaks show.
    fn under() -> Shadow {
        let mut shadow = Shadow::new(8, 20);
        for row in 3..=5 {
            shadow.feed(format!("\x1b[{row};1HABCDEFGHIJKLMNOPQRST").as_bytes());
        }
        shadow
    }

    /// Just the glyphs a composite wrote, in order.
    fn glyphs(bytes: &[u8]) -> String {
        painted_cells(bytes)
            .into_iter()
            .map(|(_, _, text)| text)
            .collect()
    }

    /// The editor's glyphs are written fully dissolved, like the notice's own
    /// spaces.
    #[test]
    fn a_box_at_rest_hides_the_screen_it_is_over() {
        let p = palette();
        let box_glyphs = "╭─╮│╰╯hi";
        let drawn = format!("0;38;2;{};{};{}", p.fg.0, p.fg.1, p.fg.2);
        let gone = format!("0;38;2;{};{};{}", p.bg.0, p.bg.1, p.bg.2);
        for (at, sgr, text) in painted_cells(&under().under(&over(), &p, p.fg, 0.0)) {
            if box_glyphs.contains(&text) {
                assert_eq!(sgr, drawn, "the box was not whole at {at:?}");
            } else {
                assert_eq!(sgr, gone, "{text:?} showed through the box at {at:?}");
            }
        }
    }

    /// So the notice can leave without a repaint.
    #[test]
    fn a_fully_dissolved_box_is_the_screen_underneath() {
        let frame = under().under(&over(), &palette(), palette().fg, 1.0);
        // Columns 3..11 of each row, 0-based: D through K.
        assert_eq!(glyphs(&frame), "DEFGHIJKDEFGHIJKDEFGHIJK");
        let want = format!(
            "0;38;2;{};{};{}",
            palette().fg.0,
            palette().fg.1,
            palette().fg.2
        );
        for (at, sgr, _) in painted_cells(&frame) {
            assert_eq!(sgr, want, "cell {at:?} was not at full colour");
        }
    }

    #[test]
    fn a_cell_holding_both_swaps_at_the_crossover() {
        let shadow = under();
        assert!(glyphs(&shadow.under(&over(), &palette(), palette().fg, 0.49)).starts_with('╭'));
        assert_eq!(
            glyphs(&shadow.under(&over(), &palette(), palette().fg, 0.51)),
            "DEFGHIJKDEFGHIJKDEFGHIJK"
        );
    }

    #[test]
    fn the_two_layers_dissolve_in_opposite_directions() {
        let shadow = under();
        let ink = |t: f32| {
            painted_cells(&shadow.under(&over(), &palette(), palette().fg, t))
                .first()
                .map(|(_, sgr, _)| sgr.clone())
                .expect("a cell")
        };
        // The box, dissolving toward the background as `t` rises.
        assert_eq!(ink(0.0), "0;38;2;200;200;200");
        assert_eq!(ink(0.25), "0;38;2;150;150;150");
        // Past the crossover it is the screen, rising back out of it.
        assert_eq!(ink(0.75), "0;38;2;150;150;150");
        assert_eq!(ink(1.0), "0;38;2;200;200;200");
    }

    #[test]
    fn the_box_is_in_its_ink_and_the_screen_in_its_own_colours() {
        let p = palette();
        let ink = Rgb(0x7a, 0xa2, 0xf7);
        let sgr = |c: Rgb| format!("0;38;2;{};{};{}", c.0, c.1, c.2);
        let box_glyphs = "╭─╮│╰╯hi";
        for t in [0.0, 0.25] {
            for (at, got, text) in painted_cells(&under().under(&over(), &p, ink, t)) {
                if box_glyphs.contains(&text) {
                    assert_eq!(got, sgr(ink.lerp(p.bg, t)), "at {at:?}, t = {t}");
                }
            }
        }
        for (at, got, _) in painted_cells(&under().under(&over(), &p, ink, 1.0)) {
            assert_eq!(got, sgr(p.fg), "the screen took the ink at {at:?}");
        }
    }

    /// The picker's handed-off name rides the session's fade-in frames.
    #[test]
    fn an_overlay_rides_inside_the_sessions_own_frames() {
        let p = palette();
        let name = Over {
            top: 3,
            left: 4,
            width: 4,
            rows: vec!["name".into()],
        };
        // Each cell's last paint, 1-based.
        let last = |bytes: &[u8]| {
            let mut cells = std::collections::BTreeMap::new();
            for (at, sgr, glyph) in painted_cells(bytes) {
                cells.insert(at, (sgr, glyph));
            }
            cells
        };
        let mut shadow = under();

        let first = shadow.frame_with(
            |_| 1.0,
            &p,
            Cursor::Hidden,
            Some((&name, p.fg, 0.0)),
            &Kept::default(),
        );
        let wire = text(&first);
        assert_eq!(
            wire.matches("\x1b[?2026h").count(),
            1,
            "one update: {wire:?}"
        );
        assert!(wire.ends_with("\x1b[?2026l"), "{wire:?}");
        let cells = last(&first);
        let drawn = format!("0;38;2;{};{};{}", p.fg.0, p.fg.1, p.fg.2);
        let shown: String = (5..=8).map(|col| cells[&(4, col)].1.clone()).collect();
        assert_eq!(shown, "name");
        assert!(
            (5..=8).all(|col| cells[&(4, col)].0 == drawn),
            "drawn whole"
        );
        assert_eq!(cells[&(4, 9)].1, "I", "beside it, the session's own cell");

        let end = shadow.frame_with(
            |_| 0.0,
            &p,
            Cursor::Hidden,
            Some((&name, p.fg, 1.0)),
            &Kept::default(),
        );
        let cells = last(&end);
        let back: String = (5..=8).map(|col| cells[&(4, col)].1.clone()).collect();
        assert_eq!(back, "EFGH", "the session's cells, where the name was");
        assert!(
            (5..=8).all(|col| cells[&(4, col)].0 == drawn),
            "at full colour"
        );
    }

    /// How an iris opens a session out of one line.
    #[test]
    fn a_frame_takes_each_row_as_far_as_it_is_told() {
        let p = palette();
        let mut shadow = under();
        // 0-based rows 2 and 3 are 3 and 4 on the wire.
        let shape = |row: u16| if row == 3 { 0.0 } else { 1.0 };
        let frame = shadow.frame_with(shape, &p, Cursor::Hidden, None, &Kept::default());
        let full = format!("0;38;2;{};{};{}", p.fg.0, p.fg.1, p.fg.2);
        let gone = format!("0;38;2;{};{};{}", p.bg.0, p.bg.1, p.bg.2);
        let cells = painted_cells(&frame);
        let row = |r: usize| {
            cells
                .iter()
                .filter(move |(at, _, glyph)| at.0 == r && glyph != " ")
                .map(|(_, sgr, _)| sgr.clone())
                .collect::<Vec<_>>()
        };
        assert!(
            !row(4).is_empty() && row(4).iter().all(|sgr| *sgr == full),
            "the row it opens out of"
        );
        assert!(
            !row(3).is_empty() && row(3).iter().all(|sgr| *sgr == gone),
            "a row still waiting"
        );
    }

    #[test]
    fn the_composite_leaves_the_cursor_and_the_editors_attributes_alone() {
        let frame = under().under(&over(), &palette(), palette().fg, 0.5);
        let mut head = Vec::new();
        head.extend_from_slice(SYNC_BEGIN);
        head.extend_from_slice(SAVE_CURSOR);
        let mut tail = Vec::new();
        tail.extend_from_slice(RESET_SGR);
        tail.extend_from_slice(RESTORE_CURSOR);
        tail.extend_from_slice(SYNC_END);
        assert!(frame.starts_with(&head), "{:?}", text(&frame));
        assert!(frame.ends_with(&tail), "{:?}", text(&frame));
        for hidden in [HIDE_CURSOR, SHOW_CURSOR] {
            assert!(
                !contains(&frame, hidden),
                "the composite touched the cursor"
            );
        }
        assert!(!frame.contains(&b'\n') && !frame.contains(&b'\r'));
    }

    #[test]
    fn the_composite_never_writes_outside_its_rectangle() {
        let over = over();
        let shadow = under();
        for t in [0.0, 0.25, 0.5, 0.75, 1.0] {
            for (at, _, text) in painted_cells(&shadow.under(&over, &palette(), palette().fg, t)) {
                let (row, col) = at;
                assert!(
                    (usize::from(over.top) + 1..usize::from(over.top) + 4).contains(&row),
                    "row {row} is outside the box at t={t}"
                );
                let width = UnicodeWidthChar::width(text.chars().next().expect("a char"))
                    .expect("a printable");
                assert!(
                    col > usize::from(over.left)
                        && col + width <= usize::from(over.left + over.width) + 1,
                    "column {col} ({text:?}) is outside the box at t={t}"
                );
            }
        }
    }

    /// The repaint after the notice puts it back whole.
    #[test]
    fn a_wide_character_cut_by_an_edge_is_blanked_rather_than_broken() {
        let mut shadow = Shadow::new(8, 20);
        // 日 straddles the left edge (0-based columns 2 and 3) and 本 the
        // right (columns 10 and 11); the box covers 3..11.
        shadow.feed("\x1b[3;3H日......本".as_bytes());
        let frame = shadow.under(&over(), &palette(), palette().fg, 1.0);
        let row: String = painted_cells(&frame)
            .into_iter()
            .filter(|((r, _), _, _)| *r == 3)
            .map(|(_, _, text)| text)
            .collect();
        assert_eq!(row, " ...... ", "{row:?}");
        assert!(!text(&frame).contains('日') && !text(&frame).contains('本'));
    }

    #[test]
    fn a_rectangle_off_the_grid_paints_blanks() {
        let over = Over {
            top: 6,
            left: 16,
            width: 8,
            rows: vec!["        ".into(), "        ".into(), "        ".into()],
        };
        let frame = under().under(&over, &palette(), palette().fg, 1.0);
        assert_eq!(glyphs(&frame), " ".repeat(24));
    }

    #[test]
    fn a_retired_shadow_composites_nothing() {
        let mut shadow = under();
        shadow.broken = true;
        assert!(painted_cells(&shadow.under(&over(), &palette(), palette().fg, 1.0)).is_empty());
    }

    /// A frame that paints nothing: only the wrapper.
    fn wrapper_only(bytes: &[u8]) -> bool {
        let mut want = Vec::new();
        want.extend_from_slice(SYNC_BEGIN);
        want.extend_from_slice(HIDE_CURSOR);
        want.extend_from_slice(RESET_SGR);
        want.extend_from_slice(SYNC_END);
        bytes == want
    }

    #[test]
    fn a_frame_is_one_synchronized_update_with_no_newlines() {
        let mut s = Shadow::new(3, 8);
        s.feed(b"hello\r\nworld");
        let f = s.frame(0.5, &palette(), Cursor::Hidden);
        assert!(f.starts_with(SYNC_BEGIN));
        assert!(f[SYNC_BEGIN.len()..].starts_with(HIDE_CURSOR));
        assert!(f.ends_with(SYNC_END));
        let body = &f[..f.len() - SYNC_END.len()];
        assert!(body.ends_with(RESET_SGR), "{:?}", text(&f));
        assert!(!f.contains(&b'\n'), "a newline reached the terminal");
        assert!(
            !f.contains(&b'\r'),
            "a carriage return reached the terminal"
        );
    }

    #[test]
    fn the_first_frame_places_every_row() {
        let mut s = Shadow::new(4, 5);
        s.feed(b"x");
        let f = s.frame(0.3, &palette(), Cursor::Hidden);
        let rows: Vec<usize> = placements(&f).iter().map(|(r, _)| *r).collect();
        assert_eq!(rows, vec![1, 2, 3, 4], "{:?}", text(&f));
        assert!(placements(&f).iter().all(|(_, c)| *c == 1));
    }

    #[test]
    fn a_foreground_is_interpolated_toward_the_background() {
        let mut s = Shadow::new(1, 4);
        s.feed(b"\x1b[31mhi");
        let f = s.frame(0.5, &palette(), Cursor::Hidden);
        assert!(
            sgrs(&f).iter().any(|sgr| sgr == "0;38;2;100;0;0"),
            "{:?}",
            sgrs(&f)
        );
        let f = s.frame(1.0, &palette(), Cursor::Hidden);
        assert!(
            sgrs(&f).iter().any(|sgr| sgr == "0;38;2;0;0;0"),
            "fully dissolved is the background: {:?}",
            sgrs(&f)
        );
    }

    #[test]
    fn a_default_background_is_left_to_the_terminal() {
        let mut s = Shadow::new(1, 6);
        s.feed(b"ab\x1b[44mcd");
        let f = s.frame(0.5, &palette(), Cursor::Hidden);
        let sgrs = sgrs(&f);
        assert!(
            sgrs.iter().any(|sgr| !sgr.contains("48;2;")),
            "the default-background cells set no background: {sgrs:?}"
        );
        assert!(
            sgrs.iter().any(|sgr| sgr.ends_with(";48;2;0;0;100")),
            "the blue cells are half blue: {sgrs:?}"
        );
        assert!(
            !text(&f).contains("[49m") && !text(&f).contains(";49"),
            "no explicit 49 is needed after a reset"
        );
    }

    #[test]
    fn a_repeated_frame_paints_nothing_and_blanks_are_painted_once() {
        let mut s = Shadow::new(2, 10);
        s.feed(b"hi");
        let first = s.frame(0.25, &palette(), Cursor::Hidden);
        assert!(!wrapper_only(&first));
        let again = s.frame(0.25, &palette(), Cursor::Hidden);
        assert!(wrapper_only(&again), "{:?}", text(&again));
        // A later `t` repaints only the two glyphs: one placement.
        let later = s.frame(0.5, &palette(), Cursor::Hidden);
        assert_eq!(placements(&later), vec![(1, 1)], "{:?}", text(&later));
    }

    #[test]
    fn output_and_resizes_make_the_next_frame_a_full_one() {
        let mut s = Shadow::new(2, 4);
        s.feed(b"a");
        let _ = s.frame(0.5, &palette(), Cursor::Hidden);
        s.feed(b"b");
        assert_eq!(
            placements(&s.frame(0.5, &palette(), Cursor::Hidden)).len(),
            2
        );
        let _ = s.frame(0.5, &palette(), Cursor::Hidden);
        s.resize(3, 4);
        assert_eq!(s.size(), (3, 4));
        assert_eq!(
            placements(&s.frame(0.5, &palette(), Cursor::Hidden)).len(),
            3
        );
    }

    #[test]
    fn a_wide_character_is_written_once_and_advances_two_columns() {
        let mut s = Shadow::new(1, 6);
        s.feed("日x".as_bytes());
        let f = s.frame(0.5, &palette(), Cursor::Hidden);
        let t = text(&f);
        assert_eq!(t.matches('日').count(), 1, "{t:?}");
        let on_first_row: Vec<(usize, usize)> = placements(&f)
            .into_iter()
            .filter(|(row, _)| *row == 1)
            .collect();
        assert_eq!(
            on_first_row,
            vec![(1, 1)],
            "one run, no re-placement: {t:?}"
        );
        // 日, then x, then blanks, with no filler after the wide glyph.
        let after = &t[t.find('日').expect("the glyph") + '日'.len_utf8()..];
        assert!(
            after.starts_with('x') || after.starts_with('\x1b'),
            "{after:?}"
        );
        assert!(after.contains('x'));
    }

    #[test]
    fn combining_marks_stay_with_their_base() {
        let mut s = Shadow::new(1, 4);
        s.feed("e\u{301}!".as_bytes());
        let t = text(&s.frame(0.5, &palette(), Cursor::Hidden));
        assert!(t.contains("e\u{301}!"), "{t:?}");
    }

    #[test]
    fn inverse_video_swaps_the_resolved_colours() {
        let mut s = Shadow::new(1, 4);
        s.feed(b"\x1b[7mx");
        let sgrs = sgrs(&s.frame(0.5, &palette(), Cursor::Hidden));
        // fg 200 -> 100 halfway becomes the bg; the default bg (0,0,0)
        // becomes the fg, already at the background.
        assert!(
            sgrs.iter()
                .any(|sgr| sgr == "0;38;2;0;0;0;48;2;100;100;100"),
            "{sgrs:?}"
        );
        assert!(!text(&s.frame(0.5, &palette(), Cursor::Hidden)).contains(";7"));
    }

    #[test]
    fn every_attribute_the_grid_holds_survives_a_fade() {
        let mut s = Shadow::new(1, 16);
        s.feed(b"\x1b[1mb\x1b[0;3mi\x1b[0;4mu\x1b[0;3;4mx");
        s.feed(b"\x1b[0;5mk\x1b[0;8mc\x1b[0;9ms\x1b[0;11ma\x1b[0;53mo\x1b[0;3;9;53my");
        for t in [1.0, 0.5, 0.0] {
            s.invalidate();
            let cells = painted_cells(&s.frame(t, &palette(), Cursor::Hidden));
            let sgr = |glyph: &str| {
                cells
                    .iter()
                    .find(|(_, _, text)| text == glyph)
                    .map(|(_, sgr, _)| sgr.clone())
                    .unwrap_or_else(|| panic!("{glyph:?} not painted at {t}: {cells:?}"))
            };
            assert!(sgr("b").starts_with("0;1;38;"), "bold at {t}");
            assert!(sgr("i").starts_with("0;3;38;"), "italic at {t}");
            assert!(sgr("u").starts_with("0;4;38;"), "underline at {t}");
            assert!(sgr("x").starts_with("0;3;4;38;"), "both at {t}");
            assert!(sgr("k").starts_with("0;5;38;"), "blink at {t}");
            assert!(sgr("c").starts_with("0;8;38;"), "conceal at {t}");
            assert!(sgr("s").starts_with("0;9;38;"), "strikethrough at {t}");
            assert!(
                sgr("a").starts_with("0;11;38;"),
                "the alternate font at {t}"
            );
            assert!(sgr("o").starts_with("0;53;38;"), "overline at {t}");
            assert!(sgr("y").starts_with("0;3;9;53;38;"), "three at {t}");
        }
    }

    /// Light text on a background that is not black, where a colour halved
    /// toward black is darker than the background.
    fn slate() -> Palette {
        Palette {
            fg: Rgb(240, 240, 240),
            bg: Rgb(40, 40, 60),
            ..palette()
        }
    }

    #[test]
    fn dim_is_drawn_into_a_fading_colour_and_left_to_the_terminal_at_rest() {
        let mut s = Shadow::new(1, 8);
        s.feed(b"\x1b[2md\x1b[0;2;7mr\x1b[0;2;38;2;200;0;0;48;2;0;0;100mc");
        let mut at = |t| {
            s.invalidate();
            s.frame(t, &slate(), Cursor::Hidden)
        };
        assert_eq!(pen_of(&at(0.0), "d"), "0;2;38;2;240;240;240");
        let half = at(0.5);
        assert_eq!(pen_of(&half, "d"), "0;38;2;80;80;96", "from its dim colour");
        assert_eq!(
            pen_of(&half, "r"),
            "0;38;2;40;40;60;48;2;80;80;96",
            "reversed, its bar"
        );
        assert_eq!(
            pen_of(&half, "c"),
            "0;38;2;60;20;60;48;2;20;20;80",
            "first toward its own background"
        );
        assert_eq!(pen_of(&at(1.0), "d"), "0;38;2;40;40;60", "gone, no darker");
    }

    #[test]
    fn dim_text_behind_a_floating_screen_fades_toward_the_background() {
        let mut s = Shadow::new(1, 4);
        s.feed(b"\x1b[2md");
        let beside = |_, _| Shade::Toward(0.9);
        let dimmed = s.frame_under(&beside, Some(&slate()), &[], Cursor::Hidden);
        assert_eq!(pen_of(&dimmed, "d"), "0;38;2;48;48;67");
        s.invalidate();
        let faint = s.frame_under(&|_, _| Shade::Faint, None, &[], Cursor::Hidden);
        assert_eq!(pen_of(&faint, "d"), "0;2", "no palette: the terminal's");
    }

    /// Neovim underlines blanks by writing spaces; terminals never underline
    /// an erase. The grid keeps the pen on erased cells too, so the frame
    /// must tell them apart.
    #[test]
    fn an_underline_is_painted_under_written_spaces_and_not_under_an_erase() {
        let mut s = Shadow::new(1, 4);
        s.feed(b"\x1b[3;4m  \x1b[K");
        let cells = painted_cells(&s.frame(0.5, &palette(), Cursor::Hidden));
        let at = |col: usize| {
            cells
                .iter()
                .find(|(at, _, _)| *at == (1, col))
                .map(|(_, sgr, _)| sgr.clone())
                .unwrap_or_else(|| panic!("column {col} not painted: {cells:?}"))
        };
        assert!(at(1).starts_with("0;3;4;38;"), "a written space: {}", at(1));
        assert_eq!(at(3), "0;38;2;0;0;0", "an erased cell");
    }

    /// A wavy red underline as the client writes one.
    const WAVY: &str = "\x1b[0;4;38;2;200;200;200m\x1b[4:3m\x1b[58:2::250:0:0m";

    /// The pen `frame` writes `glyph` in.
    fn pen_of(frame: &[u8], glyph: &str) -> String {
        painted_cells(frame)
            .into_iter()
            .find(|(_, _, text)| text == glyph)
            .map(|(_, sgr, _)| sgr)
            .unwrap_or_else(|| panic!("{glyph:?} not painted: {:?}", text(frame)))
    }

    /// What the twin holds of the cell at 0-based (`row`, `col`).
    fn extra_at(s: &Shadow, row: u16, col: u16) -> Extra {
        s.screen().cell(row, col).expect("a cell").extra()
    }

    fn underlined(underline: Underline, sp: vt100::Color) -> Extra {
        Extra {
            underline,
            sp,
            flags: Flags::NONE,
        }
    }

    fn flagged(flags: &[Flags]) -> Extra {
        Extra {
            flags: Flags(flags.iter().fold(0, |all, flag| all | flag.0)),
            ..Extra::NONE
        }
    }

    #[test]
    fn a_wavy_underline_stays_wavy_behind_a_floating_screen() {
        let mut s = Shadow::new(2, 12);
        s.feed(format!("{WAVY}wavy\x1b[0;4m flat").as_bytes());
        let half = |_, _| Shade::Toward(0.5);
        let dimmed = s.frame_under(&half, Some(&palette()), &[], Cursor::Hidden);
        assert_eq!(
            pen_of(&dimmed, "w"),
            "0;4;38;2;100;100;100 4:3 58:2::125:0:0",
            "its colour dimmed as the text is"
        );
        assert_eq!(pen_of(&dimmed, "f"), "0;4;38;2;100;100;100");
        s.invalidate();
        let faint = s.frame_under(&|_, _| Shade::Faint, None, &[], Cursor::Hidden);
        assert_eq!(
            pen_of(&faint, "w"),
            "0;2;4;38;2;200;200;200 4:3 58:2::250:0:0"
        );
        assert_eq!(pen_of(&faint, "f"), "0;2;4");
    }

    #[test]
    fn what_vt100_has_no_bit_for_shows_behind_a_floating_screen() {
        let mut s = Shadow::new(2, 20);
        s.feed(b"\x1b[0;9ms\x1b[0;53mo\x1b[0;5mb\x1b[0;8mc\x1b[0;11ma\x1b[0;1;7;5;9mx");
        let half = |_, _| Shade::Toward(0.5);
        let dimmed = s.frame_under(&half, Some(&palette()), &[], Cursor::Hidden);
        let faint = s.frame_under(&|_, _| Shade::Faint, None, &[], Cursor::Hidden);
        for (glyph, code) in [("s", 9), ("o", 53), ("b", 5), ("c", 8), ("a", 11)] {
            assert_eq!(pen_of(&dimmed, glyph), format!("0;{code};38;2;100;100;100"));
            assert_eq!(pen_of(&faint, glyph), format!("0;2;{code}"));
        }
        assert_eq!(pen_of(&s.paint(), "x"), "0;1;7;5;9", "with the rest");
    }

    #[test]
    fn an_underlines_style_and_colour_survive_a_fade() {
        let mut s = Shadow::new(1, 12);
        s.feed(format!("{WAVY}w\x1b[0;4m\x1b[4:5m\x1b[58:5:9md\x1b[0;4:2mt").as_bytes());
        let frame = s.frame(0.5, &palette(), Cursor::Hidden);
        assert_eq!(
            pen_of(&frame, "w"),
            "0;4;38;2;100;100;100 4:3 58:2::125:0:0"
        );
        assert_eq!(
            pen_of(&frame, "d"),
            "0;4;38;2;100;100;100 4:5 58:2::128:0:0",
            "an indexed colour through the palette"
        );
        assert_eq!(
            pen_of(&frame, "t"),
            "0;4;38;2;100;100;100 4:2",
            "in the text's colour"
        );
    }

    #[test]
    fn an_underline_is_read_in_every_form_it_is_written() {
        use vt100::Color::{Default, Idx, Rgb};
        use Underline::{Curl, Dashed, Dotted, Double, Line};
        let after = |sgrs: &[&str]| {
            let mut s = Shadow::new(1, 4);
            for sgr in sgrs {
                s.feed(format!("\x1b[{sgr}m").as_bytes());
            }
            s.feed(b"x");
            extra_at(&s, 0, 0)
        };
        for (sgrs, expected) in [
            (&["4"][..], underlined(Line, Default)),
            (&["4:1"], underlined(Line, Default)),
            (&["4", "4:3"], underlined(Curl, Default)),
            // Neovim's own client sends no `4` first.
            (&["4:3"], underlined(Curl, Default)),
            (&["4:2"], underlined(Double, Default)),
            (&["4:4"], underlined(Dotted, Default)),
            (&["4:5"], underlined(Dashed, Default)),
            // A plain `4` is a straight one again.
            (&["4:3", "4"], underlined(Line, Default)),
            (&["4:3;4"], underlined(Line, Default)),
            (&["4", "4:0"], Extra::NONE),
            (&["4", "4:0", "4"], underlined(Line, Default)),
            (&["4:3", "4:0"], Extra::NONE),
            (&["4:3", "24"], Extra::NONE),
            (&["4:3", "0"], Extra::NONE),
            (&["4:3", ""], Extra::NONE),
            (&["4:3;1"], underlined(Curl, Default)),
            (&["4;58:2::1:2:3"], underlined(Line, Rgb(1, 2, 3))),
            (&["4;58:2:1:2:3"], underlined(Line, Rgb(1, 2, 3))),
            (&["4;58;2;1;2;3"], underlined(Line, Rgb(1, 2, 3))),
            (&["4;58:5:9"], underlined(Line, Idx(9))),
            (&["4;58;5;9"], underlined(Line, Idx(9))),
            (&["4;58;5;9;4:3"], underlined(Curl, Idx(9))),
            (&["4;58:5:9", "59"], underlined(Line, Default)),
            (&["4;58:5:9", "38;5;1;48;5;2"], underlined(Line, Idx(9))),
            // The colour waits for its underline.
            (&["58:5:9"], Extra::NONE),
            (&["58:5:9", "4"], underlined(Line, Idx(9))),
            // A colour's arguments are not parameters.
            (&["38;5;4"], Extra::NONE),
            (&["38;2;4;4;4"], Extra::NONE),
            (&["48;5;4"], Extra::NONE),
            (&["38;5;4;4"], underlined(Line, Default)),
            (&["4:3", "38;5;0"], underlined(Curl, Default)),
            (&["4:3", "38;2;0;0;0;48;2;0;0;0"], underlined(Curl, Default)),
        ] {
            assert_eq!(after(sgrs), expected, "{sgrs:?}");
        }
    }

    #[test]
    fn each_attribute_vt100_drops_is_set_and_cleared_by_its_parameters() {
        use Flags as F;
        let after = |sgrs: &[&str]| {
            let mut s = Shadow::new(1, 4);
            for sgr in sgrs {
                s.feed(format!("\x1b[{sgr}m").as_bytes());
            }
            s.feed(b"x");
            extra_at(&s, 0, 0)
        };
        for (sgrs, expected) in [
            (&["5"][..], flagged(&[F::BLINK])),
            (&["8"], flagged(&[F::CONCEAL])),
            (&["9"], flagged(&[F::STRIKETHROUGH])),
            (&["11"], flagged(&[F::ALTFONT])),
            (&["53"], flagged(&[F::OVERLINE])),
            (&["5", "25"], Extra::NONE),
            (&["8", "28"], Extra::NONE),
            (&["9", "29"], Extra::NONE),
            (&["11", "10"], Extra::NONE),
            (&["53", "55"], Extra::NONE),
            // As the client writes them: all in one sequence.
            (
                &["0;1;2;3;4;5;7;8;9;11;53;38;5;200;48;2;1;2;3"],
                Extra {
                    underline: Underline::Line,
                    ..flagged(&[
                        F::BLINK,
                        F::CONCEAL,
                        F::STRIKETHROUGH,
                        F::ALTFONT,
                        F::OVERLINE,
                    ])
                },
            ),
            // As Neovim's own client does: one to a sequence.
            (&["", "9", "53"], flagged(&[F::STRIKETHROUGH, F::OVERLINE])),
            (&["9;53", "29"], flagged(&[F::OVERLINE])),
            (
                &["9;53", "1;38;5;4"],
                flagged(&[F::STRIKETHROUGH, F::OVERLINE]),
            ),
            (&["9;53", "0"], Extra::NONE),
            (&["9;53", ""], Extra::NONE),
            (
                &["4:3;9"],
                Extra {
                    underline: Underline::Curl,
                    ..flagged(&[F::STRIKETHROUGH])
                },
            ),
            // A colour's arguments are not parameters.
            (&["38;5;9"], Extra::NONE),
            (&["48;5;53"], Extra::NONE),
            (&["38;2;5;8;9"], Extra::NONE),
            (&["58;5;9"], Extra::NONE),
            (&["38;5;9;9"], flagged(&[F::STRIKETHROUGH])),
        ] {
            assert_eq!(after(sgrs), expected, "{sgrs:?}");
        }
    }

    /// `CSI > 4 ; 2 m` is xterm's modifyOtherKeys, which Neovim's client
    /// sets.
    #[test]
    fn a_key_mode_underlines_nothing() {
        let mut s = Shadow::new(1, 4);
        s.feed(b"\x1b[>4;2mx");
        assert_eq!(extra_at(&s, 0, 0), Extra::NONE);
    }

    #[test]
    fn an_sgr_read_in_two_is_taken_whole() {
        let written = format!("{WAVY}x");
        let written = written.as_bytes();
        for k in 1..written.len() {
            let mut s = Shadow::new(1, 4);
            s.feed(&written[..k]);
            s.feed(&written[k..]);
            assert_eq!(
                extra_at(&s, 0, 0),
                underlined(Underline::Curl, vt100::Color::Rgb(250, 0, 0)),
                "read in two at byte {k}"
            );
            assert_eq!(s.parser.screen().contents(), "x");
        }
    }

    /// Why the twin is a parser: what it holds of a cell moves with it.
    #[test]
    fn what_the_twin_holds_goes_where_its_cell_goes() {
        let mut s = Shadow::new(4, 10);
        s.feed(b"\x1b[3;3H\x1b[4:3;9mab\x1b[0m\x1b[4;1Hcd");
        let wavy_struck = Extra {
            underline: Underline::Curl,
            ..flagged(&[Flags::STRIKETHROUGH])
        };
        // 0-based cells the twin has as `wavy_struck`, and those holding `a`
        // or `b`.
        let marked_and_lettered = |s: &Shadow| {
            let screen = s.screen();
            let cells = |want: &dyn Fn(Cell) -> bool| -> Vec<(u16, u16)> {
                (0..screen.size().0)
                    .flat_map(|row| (0..screen.size().1).map(move |col| (row, col)))
                    .filter(|&(row, col)| screen.cell(row, col).is_some_and(want))
                    .collect()
            };
            (
                cells(&|c| c.extra() == wavy_struck),
                cells(&|c| matches!(c.contents(), "a" | "b")),
            )
        };
        let at = |s: &Shadow, cells: [(u16, u16); 2]| {
            assert_eq!(marked_and_lettered(s), (cells.to_vec(), cells.to_vec()));
        };
        at(&s, [(2, 2), (2, 3)]);
        s.feed(b"\x1b[S");
        at(&s, [(1, 2), (1, 3)]);
        s.feed(b"\x1b[2;1H\x1b[2@");
        at(&s, [(1, 4), (1, 5)]);
        s.feed(b"\x1b[1;1H\x1b[2L");
        at(&s, [(3, 4), (3, 5)]);
        s.feed(b"\x1b[2;4r\x1b[4;1H\n\x1b[r");
        at(&s, [(2, 4), (2, 5)]);
        s.resize(6, 12);
        at(&s, [(2, 4), (2, 5)]);
        s.feed(b"\x1b[3;6H\x1b[K");
        assert_eq!(marked_and_lettered(&s), (vec![(2, 4)], vec![(2, 4)]));
    }

    /// Other drawing that leaves the first row alone, in reads of 8 KiB.
    fn draw_elsewhere(s: &mut Shadow, bytes: usize) {
        let drawn = b"\x1b[3;1H0123456789".repeat(bytes / 16 + 1);
        for read in drawn.chunks(8192) {
            s.feed(read);
        }
    }

    /// With a twin from the start, a relay under a screen that changes colour
    /// at every cell took ten times as long to pass a paste on.
    #[test]
    fn a_screen_with_nothing_for_a_twin_has_none() {
        let mut s = Shadow::new(4, 20);
        // As Neovim's client writes a cell, and as nvmux's own does.
        s.feed(b"\x1b(B\x1b[m\x1b[38;5;167m\x1b[48;5;19m1");
        s.feed(b"\x1b[0;1;3;4;7;38;5;9;48;2;5;8;53mu");
        // Parameters that only clear what a twin would hold.
        s.feed(b"\x1b[59m\x1b[25;28;29;10;55mx");
        assert!(s.twin.is_none());
        assert_eq!(
            extra_at(&s, 0, 1),
            underlined(Underline::Line, vt100::Color::Default),
            "underlined by vt100's own bit"
        );
    }

    /// The twin takes over from the screen as the SGR found it: what the
    /// same read drew before it, the scroll region and the alternate screen.
    #[test]
    fn a_twin_starts_at_the_first_sgr_that_needs_one() {
        let mut s = Shadow::new(5, 10);
        s.feed(b"\x1b[?1049h\x1b[2;4r\x1b[1;1H\x1b[4mline\x1b[0m");
        assert!(s.twin.is_none());
        // Text, the SGR, text under it, and a line feed that scrolls rows 2
        // to 4, all in one read.
        s.feed(b"\x1b[3;1Hbefore\x1b[4;1H\x1b[9mstruck\x1b[0m\x1b[4;1H\n");
        let twin = s.twin.as_ref().expect("a twin").screen();
        assert!(twin.alternate_screen());
        assert_eq!(twin.cursor_position(), s.parser.screen().cursor_position());
        let row = |row: u16, cols: u16| -> (String, Vec<Extra>) {
            let cells = (0..cols).map(|col| s.screen().cell(row, col).expect("a cell"));
            (
                cells.clone().map(|c| c.contents()).collect(),
                cells.map(|c| c.extra()).collect(),
            )
        };
        let line = underlined(Underline::Line, vt100::Color::Default);
        assert_eq!(row(0, 4), ("line".into(), vec![line; 4]));
        assert_eq!(row(1, 6), ("before".into(), vec![Extra::NONE; 6]));
        assert_eq!(
            row(2, 6),
            ("struck".into(), vec![flagged(&[Flags::STRIKETHROUGH]); 6])
        );
    }

    /// The twin starts as a copy of the parser's screen, whose cells' own
    /// colours would read as what a twin holds: an indexed background as a
    /// style of underline.
    #[test]
    fn what_was_drawn_before_a_twin_holds_nothing_in_it() {
        let coloured = b"\x1b[38;5;9;48;5;3mab\x1b[0m";
        let mut s = Shadow::new(2, 10);
        s.feed(coloured);
        s.feed(b"\x1b[?1049h");
        s.feed(coloured);
        s.feed(b"\x1b[2;1H\x1b[4:3mwavy\x1b[0m");
        assert!(s.twin.is_some());
        assert_eq!(extra_at(&s, 0, 0), Extra::NONE, "on the alternate screen");
        s.feed(b"\x1b[?1049l");
        assert_eq!(extra_at(&s, 0, 0), Extra::NONE, "and on the one behind it");
    }

    /// `vt100` reads `004:03` as 4:3, and the SGR is found again by that.
    #[test]
    fn the_sgr_a_twin_starts_at_is_found_as_vt100_read_it() {
        let mut s = Shadow::new(1, 8);
        s.feed(b"a\x1b[;004:03;01mb");
        assert_eq!(extra_at(&s, 0, 0), Extra::NONE);
        assert_eq!(
            extra_at(&s, 0, 1),
            underlined(Underline::Curl, vt100::Color::Default)
        );
    }

    #[test]
    fn a_twin_goes_once_nothing_on_screen_needs_it() {
        let mut s = Shadow::new(4, 20);
        s.feed(b"\x1b[1;1H\x1b[9mstruck\x1b[0m");
        draw_elsewhere(&mut s, 2 * TWIN_CHECK);
        assert!(s.twin.is_some(), "the word is still on screen");
        assert_eq!(extra_at(&s, 0, 0), flagged(&[Flags::STRIKETHROUGH]));

        s.feed(b"\x1b[1;1H\x1b[4mplain \x1b[0m");
        draw_elsewhere(&mut s, TWIN_CHECK);
        assert!(s.twin.is_none());
        assert_eq!(
            extra_at(&s, 0, 0),
            underlined(Underline::Line, vt100::Color::Default)
        );

        s.feed(b"\x1b[2;1H\x1b[4:3mwavy\x1b[0m");
        assert!(s.twin.is_some(), "and comes back");
        assert_eq!(
            extra_at(&s, 1, 0),
            underlined(Underline::Curl, vt100::Color::Default)
        );
    }

    /// A pen that holds something keeps the twin, for what it draws next.
    #[test]
    fn a_twin_stays_while_the_pen_needs_it() {
        let mut s = Shadow::new(4, 20);
        s.feed(b"\x1b[9m");
        draw_elsewhere(&mut s, 2 * TWIN_CHECK);
        s.feed(b"\x1b[1;1Hx");
        assert_eq!(extra_at(&s, 0, 0), flagged(&[Flags::STRIKETHROUGH]));
    }

    /// Neovim's client asks whether the terminal can curl an underline by
    /// setting one and a DECRQSS, and resets its pen before it draws.
    #[test]
    fn neovims_question_about_wavy_underlines_leaves_no_twin() {
        let asked =
            b"\x1b[?1049h\x1b[0m\x1b[4:3m\x1bP$qm\x1b\\\x1b[?u\x1b[c\x1b(B\x1b[m\x1b[H\x1b[2Jtext";
        let mut s = Shadow::new(4, 20);
        s.feed(asked);
        assert!(s.twin.is_none());

        // Read in two before the reset, the twin lasts until it is looked
        // over.
        let reset = asked.len() - b"\x1b(B\x1b[m\x1b[H\x1b[2Jtext".len();
        let mut s = Shadow::new(4, 20);
        s.feed(&asked[..reset]);
        assert!(s.twin.is_some());
        s.feed(&asked[reset..]);
        draw_elsewhere(&mut s, TWIN_CHECK);
        assert!(s.twin.is_none());
        assert_eq!(extra_at(&s, 0, 0), Extra::NONE);
    }

    /// An SGR longer than [`SGR_MAX`] is not held back for its end.
    #[test]
    fn a_twin_starts_after_the_read_at_an_sgr_cut_too_long_to_hold() {
        let sgr = format!("\x1b[{};4:3m", "0".repeat(SGR_MAX + 20));
        let (head, tail) = sgr.as_bytes().split_at(SGR_MAX + 10);
        let mut s = Shadow::new(1, 8);
        s.feed(head);
        s.feed(&[tail, b"ab"].concat());
        s.feed(b"cd");
        let wavy = underlined(Underline::Curl, vt100::Color::Default);
        let held: Vec<Extra> = (0..4).map(|col| extra_at(&s, 0, col)).collect();
        assert_eq!(held, [Extra::NONE, Extra::NONE, wavy, wavy]);
    }

    #[test]
    fn a_restored_cursor_brings_back_what_its_pen_had() {
        let mut s = Shadow::new(1, 4);
        s.feed(b"\x1b[4:3;9m\x1b7\x1b[0ma\x1b8b");
        assert_eq!(
            extra_at(&s, 0, 0),
            Extra {
                underline: Underline::Curl,
                ..flagged(&[Flags::STRIKETHROUGH])
            },
            "b, over the a"
        );
    }

    /// A terminal never underlines an erase, wavy or not.
    #[test]
    fn a_wavy_underline_is_painted_under_written_spaces_and_not_under_an_erase() {
        let mut s = Shadow::new(1, 4);
        s.feed(format!("{WAVY}  \x1b[K").as_bytes());
        let cells = painted_cells(&s.frame(0.5, &palette(), Cursor::Hidden));
        let pens: Vec<_> = cells
            .iter()
            .map(|(at, sgr, _)| (at.1, sgr.as_str()))
            .collect();
        assert_eq!(
            pens,
            [
                (1, "0;4;38;2;100;100;100 4:3 58:2::125:0:0"),
                (2, "0;4;38;2;100;100;100 4:3 58:2::125:0:0"),
                (3, "0;38;2;0;0;0"),
                (4, "0;38;2;0;0;0"),
            ]
        );
    }

    /// A line through a space or over it shows, as one under it does; a
    /// space that blinks or is concealed shows nothing, nor does an erase.
    #[test]
    fn a_space_is_painted_for_the_line_on_it() {
        let mut s = Shadow::new(1, 16);
        s.feed(b"\x1b[0;9m \x1b[0;53m \x1b[0;5m \x1b[0;8m \x1b[0;9;53m\x1b[K");
        for frame in [
            s.frame(0.5, &palette(), Cursor::Hidden),
            s.frame_under(
                &|_, _| Shade::Toward(0.5),
                Some(&palette()),
                &[],
                Cursor::Hidden,
            ),
        ] {
            let cells = painted_cells(&frame);
            let pens: Vec<_> = cells
                .iter()
                .map(|(at, sgr, _)| (at.1, sgr.as_str()))
                .collect();
            assert_eq!(
                pens,
                [(1, "0;9;38;2;100;100;100"), (2, "0;53;38;2;100;100;100")],
                "{:?}",
                text(&frame)
            );
        }
    }

    /// Neovim may write on without setting its pen again.
    #[test]
    fn a_paint_hands_back_what_vt100_drops_of_the_pen() {
        let mut s = Shadow::new(2, 8);
        s.feed(b"text\x1b[0;4;9;53m\x1b[4:3m\x1b[58:5:9m");
        let mut again = Shadow::new(2, 8);
        again.feed(&s.paint());
        again.feed(b"x");
        assert_eq!(
            extra_at(&again, 0, 4),
            Extra {
                underline: Underline::Curl,
                sp: vt100::Color::Idx(9),
                ..flagged(&[Flags::STRIKETHROUGH, Flags::OVERLINE])
            }
        );
        assert_eq!(extra_at(&again, 0, 0), Extra::NONE);
    }

    /// Each underline in each kind of colour, and each attribute `vt100` has
    /// no bit for, from the client's own writer.
    #[test]
    fn every_painter_draws_what_vt100_drops_as_the_client_did() {
        use crate::client::compose::Frame;
        use crate::client::grid::Text;
        use crate::client::screen::{Cursor as Shown, Screen as Client};
        use crate::client::style::{Color, Style};

        let plain = Style::default();
        let sps = [
            Color::Default,
            Color::Index(9),
            Color::Rgb(Rgb(250, 100, 0)),
        ];
        let mut rows: Vec<Vec<(Style, Extra)>> = sps
            .iter()
            .map(|&sp| {
                STYLES[1..]
                    .iter()
                    .map(|&underline| {
                        let style = Style {
                            underline,
                            sp,
                            ..plain
                        };
                        (style, underlined(underline, styled_back(sp)))
                    })
                    .collect()
            })
            .collect();
        rows.push(
            [
                (
                    Style {
                        blink: true,
                        ..plain
                    },
                    Flags::BLINK,
                ),
                (
                    Style {
                        conceal: true,
                        ..plain
                    },
                    Flags::CONCEAL,
                ),
                (
                    Style {
                        strikethrough: true,
                        ..plain
                    },
                    Flags::STRIKETHROUGH,
                ),
                (
                    Style {
                        altfont: true,
                        ..plain
                    },
                    Flags::ALTFONT,
                ),
                (
                    Style {
                        overline: true,
                        ..plain
                    },
                    Flags::OVERLINE,
                ),
            ]
            .map(|(style, flag)| (style, flagged(&[flag])))
            .to_vec(),
        );
        // Each styled cell with a plain one after it.
        let (w, h) = (rows[0].len() * 2, rows.len());
        let mut f = Frame::new(w, h, plain);
        for (r, row) in rows.iter().enumerate() {
            for (c, (style, _)) in row.iter().enumerate() {
                let cell = &mut f.cells[r * w + c * 2];
                cell.text = Text::Char('u');
                cell.style = *style;
            }
        }
        let hidden = Shown {
            visible: false,
            row: 0,
            col: 0,
            shape: 2,
            color: None,
        };
        let mut s = Shadow::new(h as u16, w as u16);
        s.feed(&Client::new().draw(f, &hidden, false, &[]));

        // What the twin holds of each cell, an underline's colour through
        // the palette if `shown`, as a painter that shades writes it. `None`
        // for a blank with no line on it, whose pen a painter is free with.
        let held = |s: &Shadow, shown: bool| -> Vec<Option<Extra>> {
            (0..h as u16)
                .flat_map(|row| (0..w as u16).map(move |col| (row, col)))
                .map(|(row, col)| {
                    let cell = s.screen().cell(row, col).expect("a cell");
                    let extra = cell.extra();
                    if cell.contents().trim().is_empty() && !extra.lined() {
                        return None;
                    }
                    Some(match extra.sp {
                        vt100::Color::Idx(n) if shown => {
                            let Rgb(r, g, b) = palette().index(n);
                            Extra {
                                sp: vt100::Color::Rgb(r, g, b),
                                ..extra
                            }
                        }
                        _ => extra,
                    })
                })
                .collect()
        };
        let drawn: Vec<Option<Extra>> = rows
            .iter()
            .flatten()
            .flat_map(|(_, extra)| [Some(*extra), None])
            .collect();
        assert_eq!(held(&s, false), drawn, "what the shadow holds");

        let over = Over {
            top: 0,
            left: 0,
            width: w as u16,
            rows: vec![" ".repeat(w); h],
        };
        let kept = Kept::new((0..h).map(|r| (r, 0, w - 1)), []);
        let lit = |_, _| Shade::Toward(0.0);
        for (name, bytes, shown) in [
            ("paint", s.paint(), false),
            ("fade", s.frame(0.0, &palette(), Cursor::Hidden), true),
            (
                "faint",
                s.frame_under(&|_, _| Shade::Faint, None, &[], Cursor::Hidden),
                false,
            ),
            (
                "under",
                s.frame_under(&lit, Some(&palette()), &[], Cursor::Hidden),
                true,
            ),
            (
                "crossed",
                s.under(&over, &palette(), Rgb(9, 9, 9), 1.0),
                true,
            ),
            ("kept", s.kept_bytes(&kept, &palette()), true),
        ] {
            let mut terminal = Shadow::new(h as u16, w as u16);
            terminal.feed(&bytes);
            assert_eq!(
                held(&terminal, false),
                held(&s, shown),
                "{name}: {:?}",
                text(&bytes)
            );
        }
    }

    /// The client's colour as a parser holds one.
    fn styled_back(colour: style::Color) -> vt100::Color {
        match colour {
            style::Color::Default => vt100::Color::Default,
            style::Color::Index(n) => vt100::Color::Idx(n),
            style::Color::Rgb(Rgb(r, g, b)) => vt100::Color::Rgb(r, g, b),
        }
    }

    #[test]
    fn the_alternate_screen_is_what_gets_painted() {
        let mut s = Shadow::new(2, 6);
        s.feed(b"shell");
        s.feed(b"\x1b[?1049h\x1b[2J\x1b[Hnvim");
        let t = text(&s.frame(0.5, &palette(), Cursor::Hidden));
        assert!(t.contains("nvim"), "{t:?}");
        assert!(!t.contains("shell"), "{t:?}");
    }

    #[test]
    fn the_grid_is_never_one_row_or_one_column() {
        let s = Shadow::new(1, 1);
        assert_eq!(s.size(), (MIN_SIZE, MIN_SIZE));
        let mut s = Shadow::new(24, 80);
        s.resize(0, 1);
        assert_eq!(s.size(), (MIN_SIZE, MIN_SIZE));
        s.feed(b"wrap wrap wrap wrap\r\nand scroll\r\n");
        let _ = s.frame(0.5, &palette(), Cursor::Hidden);
    }

    #[test]
    fn has_contents_means_a_glyph_somewhere() {
        let mut s = Shadow::new(4, 8);
        assert!(!s.has_contents());
        s.feed(b"\x1b[44m\x1b[2J");
        assert!(
            !s.has_contents(),
            "an erase with a background is not a glyph"
        );
        s.feed(b"\x1b[3;5H~");
        assert!(s.has_contents());
    }

    /// A replay the terminal saw but the diff did not.
    #[test]
    fn invalidating_makes_the_next_frame_a_full_one() {
        let mut s = Shadow::new(2, 4);
        s.feed(b"a");
        let _ = s.frame(0.5, &palette(), Cursor::Hidden);
        assert!(wrapper_only(&s.frame(0.5, &palette(), Cursor::Hidden)));
        s.invalidate();
        assert_eq!(
            placements(&s.frame(0.5, &palette(), Cursor::Hidden)).len(),
            2
        );
    }

    /// The bytes between the final SGR reset and the sync end.
    fn cursor_tail(frame: &[u8]) -> &[u8] {
        let body = &frame[..frame.len() - SYNC_END.len()];
        let at = body
            .windows(RESET_SGR.len())
            .rposition(|w| w == RESET_SGR)
            .expect("a trailing SGR reset");
        &body[at + RESET_SGR.len()..]
    }

    #[test]
    fn a_restored_cursor_is_placed_on_its_cell_and_shown() {
        let mut s = Shadow::new(4, 8);
        s.feed(b"\x1b[3;5Hab");
        let hidden = s.frame(0.5, &palette(), Cursor::Hidden);
        assert!(!contains(&hidden, SHOW_CURSOR), "{:?}", text(&hidden));
        assert!(cursor_tail(&hidden).is_empty(), "{:?}", text(&hidden));

        let restored = s.frame(0.0, &palette(), Cursor::Restored);
        assert_eq!(
            cursor_tail(&restored),
            b"\x1b[3;7H\x1b[?25h",
            "{:?}",
            text(&restored)
        );
        assert!(restored.ends_with(SYNC_END));
        assert!(
            restored[SYNC_BEGIN.len()..].starts_with(HIDE_CURSOR),
            "still hidden while the cells go down: {:?}",
            text(&restored)
        );
    }

    /// A busy editor hides it on purpose; restoring it would second-guess
    /// that.
    #[test]
    fn a_cursor_the_session_hid_stays_hidden() {
        let mut s = Shadow::new(4, 8);
        s.feed(b"\x1b[?25lx");
        let restored = s.frame(0.0, &palette(), Cursor::Restored);
        assert!(!contains(&restored, SHOW_CURSOR), "{:?}", text(&restored));
        assert!(cursor_tail(&restored).is_empty(), "{:?}", text(&restored));
        // And shown again once the session shows it.
        s.feed(b"\x1b[?25h");
        assert_eq!(
            cursor_tail(&s.frame(0.0, &palette(), Cursor::Restored)),
            b"\x1b[1;2H\x1b[?25h"
        );
    }

    /// The parser follows `DECSC`/`DECRC` around an overlay, or a resume would
    /// put the cursor in its corner.
    #[test]
    fn a_saved_and_restored_cursor_stays_on_the_editors_cell() {
        let mut s = Shadow::new(6, 20);
        s.feed(b"\x1b[2;3Hedit");
        s.feed(
            b"\x1b[?2026h\x1b7\x1b[4;5H\x1b[0m\xe2\x95\xad\xe2\x94\x80\xe2\x95\xae\x1b8\x1b[?2026l",
        );
        assert_eq!(
            cursor_tail(&s.frame(0.0, &palette(), Cursor::Restored)),
            b"\x1b[2;7H\x1b[?25h"
        );
    }

    #[test]
    fn a_long_clipboard_write_leaves_the_grid_alone() {
        let mut s = Shadow::new(4, 10);
        s.feed(b"ab");
        let mut osc = b"\x1b]52;c;".to_vec();
        osc.extend(std::iter::repeat_n(b'Q', 100_000));
        osc.extend_from_slice(b"\x1b\\cd");
        for chunk in osc.chunks(4095) {
            s.feed(chunk);
        }
        assert_eq!(s.parser.screen().contents(), "abcd");
        assert_eq!(s.parser.screen().cursor_position(), (0, 4));
    }

    /// Neither the OSC cap nor holding back partial characters changes the
    /// screen.
    #[test]
    fn what_the_parser_is_spared_changes_nothing_on_screen() {
        for seed in 0..200 {
            let (stream, reads) = crate::test_support::osc_noise(seed);
            let mut s = Shadow::new(6, 20);
            let mut at = 0;
            for read in reads {
                s.feed(&stream[at..at + read]);
                at += read;
            }
            let mut whole = vt100::Parser::new(6, 20, 0);
            whole.process(&stream);
            assert!(s.is_usable(), "seed {seed}");
            assert!(
                s.parser.screen().state_formatted() == whole.screen().state_formatted(),
                "seed {seed}: the shadow's screen is not the whole stream's"
            );
        }
    }

    #[test]
    fn an_invalidated_shadow_paints_every_cell_again() {
        let mut s = Shadow::new(4, 10);
        s.feed(b"abc");
        let first = s.frame(0.5, &palette(), Cursor::Hidden);
        assert!(s.painted.capacity() > 0);
        s.invalidate();
        assert_eq!(s.painted.capacity(), 0);
        assert_eq!(s.frame(0.5, &palette(), Cursor::Hidden), first);
    }

    #[test]
    fn a_retired_shadow_restores_no_cursor() {
        let mut s = Shadow::new(4, 4);
        s.feed(b"ok");
        crate::test_support::quietly(|| s.guard(|_, _| panic!("a parser bug")));
        assert!(wrapper_only(&s.frame(0.0, &palette(), Cursor::Restored)));
    }

    #[test]
    fn a_panicking_parser_retires_the_shadow() {
        let mut s = Shadow::new(4, 4);
        s.feed(b"ok");
        crate::test_support::quietly(|| s.guard(|_, _| panic!("a parser bug")));
        assert!(s.broken);
        assert!(!s.is_usable());
        s.feed(b"more");
        assert!(wrapper_only(&s.frame(0.5, &palette(), Cursor::Hidden)));
    }

    /// Plain `vte` loses the space of `é ─` read in two inside the `é`,
    /// shifting the rest of the row a cell left.
    #[test]
    fn a_character_read_in_two_is_taken_whole() {
        let written = "漢字 é ─ ab".as_bytes();
        let screen = |reads: &[&[u8]]| {
            let mut s = Shadow::new(2, 20);
            for read in reads {
                s.feed(read);
            }
            s.parser.screen().rows_formatted(0, 20).collect::<Vec<_>>()
        };
        let at_once = screen(&[written]);
        for k in 1..written.len() {
            assert_eq!(
                screen(&[&written[..k], &written[k..]]),
                at_once,
                "read in two at byte {k}"
            );
        }
    }

    #[test]
    fn tiny_screens_do_not_panic() {
        for &(cols, rows) in crate::ui::test_support::TINY_SIZES {
            let mut s = Shadow::new(rows, cols);
            s.feed(b"\x1b[31mhello\r\nworld\x1b[0m");
            let _ = s.frame(0.5, &palette(), Cursor::Hidden);
            let _ = s.frame(1.0, &palette(), Cursor::Hidden);
            s.resize(rows, cols);
            let _ = s.frame(0.5, &palette(), Cursor::Hidden);
        }
    }

    #[test]
    fn a_paint_puts_every_cell_back_as_it_was_drawn() {
        let mut s = Shadow::new(6, 30);
        s.feed(
            b"\x1b[1;1H\x1b[1;31mred bold\x1b[0m \x1b[3;4;38;5;208mitalic under\x1b[0m\
              \x1b[2;3H\x1b[7;48;2;10;20;30mreversed\x1b[0m \x1b[92;104mbright\x1b[0m\
              \x1b[3;1H\x1b[4m\x1b[4:3m\x1b[58:2::255:0:0mwavy\x1b[0m \x1b[4:2mtwice\x1b[0m\
              \x1b[9m gone\x1b[0m \x1b[5;8;11;53mrest\x1b[0m\
              \x1b[4;1H\x1b[2mdim\x1b[0m \xe5\xad\x97\xe5\xad\x97 e\xcc\x81\
              \x1b[5;7H",
        );
        let painted = s.paint();

        let mut again = Shadow::new(6, 30);
        again.feed(&painted);
        let (was, now) = (s.screen(), again.screen());
        // Unwritten and space cells look the same, and a paint writes every
        // cell.
        let seen = |c: Cell| {
            let glyph = if c.has_contents() { c.contents() } else { " " };
            (glyph.to_string(), Pen::of(Some(c)), c.is_wide())
        };
        for row in 0..6 {
            for col in 0..30 {
                assert_eq!(
                    was.cell(row, col).map(seen),
                    now.cell(row, col).map(seen),
                    "cell ({row}, {col}) came back different"
                );
            }
        }
        assert_eq!(
            now.cursor_position(),
            (4, 6),
            "the cursor is not on its cell"
        );
        assert!(!now.hide_cursor(), "the cursor was left hidden");
    }

    #[test]
    fn a_paint_ends_with_the_sessions_pen_and_cursor() {
        let mut s = Shadow::new(4, 20);
        s.feed(b"\x1b[2;3Hsome text\x1b[?25l\x1b[3;7H\x1b[1;33;44m");
        let mut again = vt100::Parser::new(4, 20, 0);
        again.process(&s.paint());
        let now = again.screen();
        assert_eq!(
            now.cursor_position(),
            (2, 6),
            "a hidden cursor is still placed"
        );
        assert!(now.hide_cursor(), "and still hidden");
        assert!(now.bold(), "the session's pen was not handed back");
        assert_eq!(now.fgcolor(), vt100::Color::Idx(3));
        assert_eq!(now.bgcolor(), vt100::Color::Idx(4));
    }

    #[test]
    fn a_paint_leaves_indexed_colours_to_the_terminal() {
        let mut s = Shadow::new(2, 10);
        s.feed(b"\x1b[33ma\x1b[93mb\x1b[38;5;200mc\x1b[44md");
        let sgrs = sgrs(&s.paint());
        assert!(sgrs.contains(&"0;33".to_string()), "{sgrs:?}");
        assert!(sgrs.contains(&"0;93".to_string()), "{sgrs:?}");
        assert!(sgrs.contains(&"0;38;5;200".to_string()), "{sgrs:?}");
        assert!(sgrs.contains(&"0;38;5;200;44".to_string()), "{sgrs:?}");
    }

    /// `OPOST` is off, and a line feed on the last row would scroll.
    #[test]
    fn a_paint_is_one_synchronized_update_with_no_newlines() {
        let mut s = Shadow::new(4, 8);
        s.feed(b"one\r\ntwo\r\nthree");
        let painted = s.paint();
        assert!(painted.starts_with(SYNC_BEGIN));
        assert!(painted.ends_with(SYNC_END));
        assert!(!painted.contains(&b'\n') && !painted.contains(&b'\r'));
        let rows: Vec<usize> = placements(&painted).iter().map(|&(r, _)| r).collect();
        assert_eq!(&rows[..4], &[1, 2, 3, 4], "every row is placed");
    }

    #[test]
    fn a_wide_character_cut_by_the_last_column_is_blanked() {
        let mut s = Shadow::new(4, 11);
        s.feed("\x1b[1;1Hone\x1b[2;1Htwo\x1b[3;1Hthree\x1b[4;1Hlast row \u{5b57}".as_bytes());
        s.resize(4, 10);
        let mut again = vt100::Parser::new(4, 10, 0);
        again.process(&s.paint());
        let rows: Vec<String> = again
            .screen()
            .rows(0, 10)
            .map(|r| r.trim_end().to_string())
            .collect();
        assert_eq!(
            rows,
            ["one", "two", "three", "last row"],
            "the screen scrolled"
        );
    }

    #[test]
    fn a_paint_keeps_a_hidden_cursor_hidden_and_a_retired_shadow_paints_nothing() {
        let mut s = Shadow::new(4, 8);
        s.feed(b"text\x1b[?25l");
        let painted = s.paint();
        assert!(!contains(&painted, SHOW_CURSOR), "{:?}", text(&painted));

        s.broken = true;
        assert_eq!(
            s.paint(),
            [SYNC_BEGIN, HIDE_CURSOR, RESET_SGR, SYNC_END].concat()
        );
    }

    /// `text` on the first row of a `width` x `height` buffer at 0-based
    /// (`left`, `top`).
    fn laid(left: u16, top: u16, width: u16, height: u16, text: &str) -> Laid {
        let area = ratatui::layout::Rect::new(left, top, width, height);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        buf.set_string(left, top, text, ratatui::style::Style::default());
        Laid::from_buffer(&buf)
    }

    /// Each cell's last `(sgr, glyph)` by 1-based position.
    fn screen_of(bytes: &[u8]) -> std::collections::BTreeMap<(usize, usize), (String, String)> {
        painted_cells(bytes)
            .into_iter()
            .map(|(at, sgr, glyph)| (at, (sgr, glyph)))
            .collect()
    }

    /// A screen of the alphabet in a grey on the terminal's background.
    fn lettered() -> Shadow {
        let mut shadow = Shadow::new(6, 20);
        for row in 1..=6 {
            shadow.feed(
                format!("\x1b[{row};1H\x1b[0;38;2;100;100;100mabcdefghijklmnopqrst").as_bytes(),
            );
        }
        shadow
    }

    #[test]
    fn the_settings_stand_over_the_session_taken_toward_the_background() {
        let mut shadow = lettered();
        let panel = laid(5, 2, 6, 2, "panel");
        let shade = |row: u16, _col: u16| Shade::Toward(if row == 0 { 0.5 } else { 0.25 });
        let frame = shadow.frame_under(
            &shade,
            Some(&palette()),
            std::slice::from_ref(&panel),
            Cursor::Hidden,
        );
        let cells = screen_of(&frame);
        let shown: String = (6..=11).map(|col| cells[&(3, col)].1.clone()).collect();
        assert_eq!(shown, "panel ");
        assert!(
            (6..=11).all(|col| cells[&(3, col)].0 == "0"),
            "in the terminal's own"
        );
        let under: String = (6..=11).map(|col| cells[&(4, col)].1.clone()).collect();
        assert_eq!(under, "      ", "nothing of the session under it");
        assert_eq!(
            cells[&(1, 1)],
            ("0;38;2;50;50;50".into(), "a".into()),
            "half way"
        );
        assert_eq!(
            cells[&(3, 5)],
            ("0;38;2;75;75;75".into(), "e".into()),
            "a quarter"
        );
        assert_eq!(cells[&(3, 12)].1, "l", "the session again past its edge");
    }

    #[test]
    fn the_session_shows_in_the_corners_a_round_panel_cuts_off() {
        let mut shadow = lettered();
        let area = ratatui::layout::Rect::new(2, 0, 16, 6);
        let panel = Laid::rounded(&ratatui::buffer::Buffer::empty(area), 4.0);
        let frame = shadow.frame_under(
            &|_, _| Shade::Toward(0.0),
            Some(&palette()),
            std::slice::from_ref(&panel),
            Cursor::Hidden,
        );
        let cells = screen_of(&frame);
        let row = |row| -> String {
            (1..=20)
                .map(|col| cells.get(&(row, col)).map_or(" ", |c| c.1.as_str()))
                .collect()
        };
        assert_eq!(row(1), "abcdef        opqrst");
        assert_eq!(row(2), "abc              rst");
        assert_eq!(row(3), "ab                st");
        assert_eq!(row(5), "abc              rst");
        assert_eq!(row(6), "abcdef        opqrst");
    }

    /// The hint row is laid the full width, so none of the session's last row
    /// shows either side of the hints.
    #[test]
    fn the_settings_and_their_hint_row_stand_over_the_session_together() {
        let mut shadow = lettered();
        let laid = [laid(5, 1, 6, 2, "panel"), laid(0, 5, 20, 1, "    esc back")];
        let shade = |_row: u16, _col: u16| Shade::Toward(0.5);
        let frame = shadow.frame_under(&shade, Some(&palette()), &laid, Cursor::Hidden);
        let terminal = over_marks(&frame, 6, 20);
        let (shown, _) = shown_on(&terminal, 1);
        let panel: String = shown
            .iter()
            .filter(|(col, _)| (5..11).contains(col))
            .map(|(_, text)| text.as_str())
            .collect();
        assert_eq!(panel, "panel");
        let screen = terminal.screen();
        let last: String = (0..20)
            .map(|col| match screen.cell(5, col).map(vt100::Cell::contents) {
                Some(text) if !text.is_empty() => text.to_string(),
                _ => " ".to_string(),
            })
            .collect();
        assert_eq!(last, "    esc back        ");
        assert!(
            shown_on(&terminal, 5).1,
            "nothing of the session's row either side"
        );
        let (between, _) = shown_on(&terminal, 3);
        assert_eq!(between.len(), 20, "the session between them: {between:?}");
    }

    #[test]
    fn a_cell_is_emptied_or_drawn_faint() {
        let mut shadow = Shadow::new(2, 6);
        // An erase in a background leaves cells with nothing written on them.
        let session = b"\x1b[31;44mab\x1b[0m\x1b[2;1H\x1b[44m\x1b[K\x1b[0m";
        shadow.feed(session);
        // Whether every cell, after `frame` over the session, is textless on
        // the default background.
        let emptied = |frame: &[u8]| {
            let mut terminal = vt100::Parser::new(2, 6, 0);
            terminal.process(session);
            terminal.process(frame);
            let screen = terminal.screen();
            (0..2).all(|row| {
                (0..6).all(|col| {
                    screen.cell(row, col).is_some_and(|c| {
                        c.contents().trim().is_empty()
                            && c.bgcolor() == vt100::Color::Default
                            && !c.inverse()
                            && !c.underline()
                    })
                })
            })
        };
        let blank = shadow.frame_under(&|_, _| Shade::Blank, None, &[], Cursor::Hidden);
        assert!(emptied(&blank));
        let faint = shadow.frame_under(&|_, _| Shade::Faint, None, &[], Cursor::Hidden);
        let cells = screen_of(&faint);
        assert_eq!(
            cells[&(1, 1)],
            ("0;2;31;44".into(), "a".into()),
            "text faint"
        );
        assert_eq!(
            cells[&(2, 1)],
            ("0;44".into(), " ".into()),
            "a background is not"
        );
        // The same frame as the last, so a diff would paint nothing.
        shadow.invalidate();
        let toward = shadow.frame_under(&|_, _| Shade::Toward(0.5), None, &[], Cursor::Hidden);
        assert_eq!(
            screen_of(&toward)[&(1, 2)].0,
            "0;2;31;44",
            "no palette: faint"
        );
        let gone = shadow.frame_under(
            &|_, _| Shade::Toward(falloff::EMPTIED),
            Some(&palette()),
            &[],
            Cursor::Hidden,
        );
        assert!(emptied(&gone), "emptied");
    }

    /// A `rows` x `cols` terminal of `#` on a background with `frame` written
    /// over it, so any cell the frame missed shows `#`.
    fn over_marks(frame: &[u8], rows: u16, cols: u16) -> vt100::Parser {
        let mut terminal = vt100::Parser::new(rows, cols, 0);
        terminal.process(b"\x1b[41m");
        for row in 1..=rows {
            terminal.process(format!("\x1b[{row};1H{}", "#".repeat(usize::from(cols))).as_bytes());
        }
        terminal.process(b"\x1b[0m");
        terminal.process(frame);
        terminal
    }

    /// A row's visible cells as `(column, text)`, and whether all others show
    /// nothing (no text, default background, no underline or inverse).
    fn shown_on(terminal: &vt100::Parser, row: u16) -> (Vec<(u16, String)>, bool) {
        let screen = terminal.screen();
        let mut text = Vec::new();
        let mut rest_blank = true;
        for col in 0..screen.size().1 {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if !cell.contents().trim().is_empty() {
                text.push((col, cell.contents().to_string()));
            } else if cell.bgcolor() != vt100::Color::Default || cell.inverse() || cell.underline()
            {
                rest_blank = false;
            }
        }
        (text, rest_blank)
    }

    #[test]
    fn a_stretch_of_blank_cells_is_erased() {
        let mut shadow = Shadow::new(3, 40);
        // A word and nothing after it; a word, twenty blanks and a word; and
        // two words three written spaces apart.
        shadow.feed(b"\x1b[1;1Hab\x1b[2;1Hx\x1b[2;22Hy\x1b[3;1Ha   b");
        let fade = shadow.frame(0.5, &palette(), Cursor::Hidden);
        let under = shadow.frame_under(
            &|_, _| Shade::Toward(0.5),
            Some(&palette()),
            &[],
            Cursor::Hidden,
        );
        for frame in [fade, under] {
            let csi = csis(&frame);
            assert!(csi.contains(&(String::new(), 'K')), "{:?}", text(&frame));
            assert!(csi.contains(&("20".into(), 'X')), "{:?}", text(&frame));
            assert!(contains(&frame, b"a   b"), "a short stretch written");
            let terminal = over_marks(&frame, 3, 40);
            let ab = vec![(0, "a".into()), (1, "b".into())];
            assert_eq!(shown_on(&terminal, 0), (ab, true));
            assert_eq!(
                shown_on(&terminal, 1),
                (vec![(0, "x".into()), (21, "y".into())], true)
            );
            assert_eq!(
                shown_on(&terminal, 2),
                (vec![(0, "a".into()), (4, "b".into())], true)
            );
        }
    }

    #[test]
    fn an_erase_is_never_made_in_a_background() {
        let mut shadow = Shadow::new(2, 30);
        // A word on a background, nothing after it, and the same in the
        // middle of a row.
        shadow.feed(b"\x1b[1;1H\x1b[44mab\x1b[0m\x1b[2;1H\x1b[44mcd\x1b[0m\x1b[2;30Hz");
        let fade = shadow.frame(0.25, &palette(), Cursor::Hidden);
        // Under the settings, a panel's own cells on a background with blank
        // ones after them, and the session's.
        let area = ratatui::layout::Rect::new(0, 0, 20, 1);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        buf.set_string(
            0,
            0,
            "on-blue",
            ratatui::style::Style::default().bg(ratatui::style::Color::Blue),
        );
        let panel = Laid::from_buffer(&buf);
        let under = shadow.frame_under(
            &|_, _| Shade::Toward(0.25),
            Some(&palette()),
            std::slice::from_ref(&panel),
            Cursor::Hidden,
        );
        for frame in [fade, under] {
            let terminal = over_marks(&frame, 2, 30);
            assert!(shown_on(&terminal, 0).1, "{:?}", text(&frame));
            assert!(shown_on(&terminal, 1).1, "{:?}", text(&frame));
        }
    }

    #[test]
    fn a_cell_drawn_under_the_settings_is_known_again() {
        let pen = Pen::BLANK;
        let kept = Drawn::of(Some(("é", pen)));
        assert!(kept.is(Some(("é", pen))));
        assert!(!kept.is(Some(("e", pen))));
        assert!(!kept.is(Some(("é", Pen { bold: true, ..pen }))));
        assert!(!kept.is(None));
        assert!(Drawn::of(None).is(None));
        let long = format!("e{}", "\u{301}".repeat(11));
        let long = long.as_str();
        assert!(long.len() > DRAWN_BYTES);
        assert!(!Drawn::of(Some((long, pen))).is(Some((long, pen))));
    }

    #[test]
    fn what_the_settings_were_drawn_against_goes_with_them() {
        let mut shadow = lettered();
        let shade = |_: u16, _: u16| Shade::Toward(0.5);
        shadow.frame_under(&shade, Some(&palette()), &[], Cursor::Hidden);
        assert!(shadow.under.capacity() > 0);
        shadow.forget_under();
        assert_eq!(shadow.under.capacity(), 0);
        let again = shadow.frame_under(&shade, Some(&palette()), &[], Cursor::Hidden);
        assert_eq!(screen_of(&again).len(), 6 * 20);
    }

    #[test]
    fn a_frame_under_the_settings_paints_only_what_changed() {
        let mut shadow = lettered();
        let shade = |_: u16, _: u16| Shade::Toward(0.5);
        let p = palette();
        let first =
            shadow.frame_under(&shade, Some(&p), &[laid(5, 2, 6, 2, "one")], Cursor::Hidden);
        assert_eq!(screen_of(&first).len(), 6 * 20);
        let again =
            shadow.frame_under(&shade, Some(&p), &[laid(5, 2, 6, 2, "one")], Cursor::Hidden);
        assert!(wrapper_only(&again), "{:?}", text(&again));
        let changed =
            shadow.frame_under(&shade, Some(&p), &[laid(5, 2, 6, 2, "ono")], Cursor::Hidden);
        let cells = screen_of(&changed);
        assert_eq!(cells.len(), 1, "the last letter: {cells:?}");
        shadow.feed(b"\x1b[1;1Hz");
        let after =
            shadow.frame_under(&shade, Some(&p), &[laid(5, 2, 6, 2, "ono")], Cursor::Hidden);
        assert_eq!(screen_of(&after).len(), 6 * 20, "all of it again");
    }

    #[test]
    fn a_wide_character_the_settings_cut_is_blanked() {
        let mut shadow = Shadow::new(2, 12);
        shadow.feed("ab漢cd漢".as_bytes());
        // Columns 3..6, 0-based: one 漢 at 2 and 3, the other at 6 and 7.
        let panel = laid(3, 0, 3, 1, "xyz");
        let frame = shadow.frame_under(
            &|_, _| Shade::Toward(0.0),
            Some(&palette()),
            std::slice::from_ref(&panel),
            Cursor::Hidden,
        );
        let cells = screen_of(&frame);
        assert_eq!(cells[&(1, 3)].1, " ", "cut by the left edge");
        assert_eq!(cells[&(1, 4)].1, "x");
        assert_eq!(cells[&(1, 7)].1, "漢", "clear of it, whole");
        assert!(
            !cells.contains_key(&(1, 8)),
            "its second half is not written"
        );
    }

    #[test]
    fn a_frame_under_the_settings_can_put_the_cursor_back() {
        let mut shadow = lettered();
        shadow.feed(b"\x1b[4;7H");
        let hidden = shadow.frame_under(&|_, _| Shade::Blank, None, &[], Cursor::Hidden);
        assert!(!text(&hidden).contains("\x1b[?25h"));
        let back = shadow.frame_under(
            &|_, _| Shade::Toward(0.0),
            Some(&palette()),
            &[],
            Cursor::Restored,
        );
        assert!(
            text(&back).ends_with("\x1b[4;7H\x1b[?25h\x1b[?2026l"),
            "{:?}",
            text(&back)
        );
    }

    #[test]
    fn the_settings_are_laid_in_their_modifiers_and_colours() {
        use ratatui::style::{Color, Modifier, Style};
        let area = ratatui::layout::Rect::new(0, 0, 4, 1);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        buf.set_string(
            0,
            0,
            "ab",
            Style::new()
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
                .fg(Color::Rgb(1, 2, 3)),
        );
        buf.set_string(
            2,
            0,
            "c",
            Style::new().add_modifier(Modifier::DIM | Modifier::UNDERLINED),
        );
        buf.set_string(3, 0, "d", Style::new().add_modifier(Modifier::CROSSED_OUT));
        let mut shadow = Shadow::new(1, 4);
        let frame = shadow.frame_under(
            &|_, _| Shade::Blank,
            None,
            &[Laid::from_buffer(&buf)],
            Cursor::Hidden,
        );
        let cells = screen_of(&frame);
        assert_eq!(cells[&(1, 1)], ("0;1;7;38;2;1;2;3".into(), "a".into()));
        assert_eq!(cells[&(1, 3)], ("0;2;4".into(), "c".into()));
        assert_eq!(cells[&(1, 4)], ("0;9".into(), "d".into()));
    }

    /// A terminal drawing a VS16 sequence two columns wide, as kitty,
    /// WezTerm and foot do, and a regional indicator, alone or in a flag, as
    /// WezTerm does; vt100 draws each in one. As in WezTerm, code points join
    /// a cell only within a run of text. Text only.
    struct Graphemes {
        rows: usize,
        cols: usize,
        /// Row-major; "" for a wide character's right half.
        cells: Vec<String>,
        at: (usize, usize),
        /// Written to the last column, the cursor left there.
        pending: bool,
        /// The cell written last, which a zero-width code point or a second
        /// regional indicator joins.
        last: Option<usize>,
    }

    impl Graphemes {
        fn new(rows: usize, cols: usize) -> Self {
            Self {
                rows,
                cols,
                cells: vec![" ".into(); rows * cols],
                at: (0, 0),
                pending: false,
                last: None,
            }
        }

        /// Blank cell `i`, and the rest of a wide character it is half of.
        fn erase(&mut self, i: usize) {
            if self.cells[i].is_empty() {
                self.cells[i - 1] = " ".into();
            } else if self.cells.get(i + 1).is_some_and(String::is_empty) {
                self.cells[i + 1] = " ".into();
            }
            self.cells[i] = " ".into();
        }

        /// The cursor to `col` of its row, or kept at the last column.
        fn go(&mut self, col: usize) {
            self.pending = col >= self.cols;
            self.at.1 = col.min(self.cols - 1);
        }

        fn line_feed(&mut self) {
            if self.at.0 + 1 == self.rows {
                self.cells.drain(..self.cols);
                self.cells.resize(self.rows * self.cols, " ".into());
            } else {
                self.at.0 += 1;
            }
        }

        fn put(&mut self, ch: char) {
            let indicator = |c: char| ('\u{1f1e6}'..='\u{1f1ff}').contains(&c);
            // Into a flag with a lone indicator before it.
            let pairs = indicator(ch)
                && self.last.is_some_and(|i| {
                    let mut held = self.cells[i].chars();
                    held.next().is_some_and(indicator) && held.next().is_none()
                });
            let width = if indicator(ch) {
                Some(2)
            } else {
                UnicodeWidthChar::width(ch)
            };
            let Some(width) = width else {
                return;
            };
            if width == 0 || pairs {
                let Some(i) = self.last else {
                    return;
                };
                self.cells[i].push(ch);
                let col = i % self.cols;
                let narrow = !self.cells.get(i + 1).is_some_and(String::is_empty);
                if narrow && col + 1 < self.cols && UnicodeWidthStr::width(&*self.cells[i]) == 2 {
                    self.erase(i + 1);
                    self.cells[i + 1] = String::new();
                    self.go(col + 2);
                }
                return;
            }
            if self.pending || (width == 2 && self.at.1 + 1 == self.cols) {
                self.line_feed();
                self.go(0);
            }
            let i = self.at.0 * self.cols + self.at.1;
            self.erase(i);
            self.cells[i] = ch.to_string();
            if width == 2 {
                self.erase(i + 1);
                self.cells[i + 1] = String::new();
            }
            self.last = Some(i);
            self.go(self.at.1 + width);
        }

        /// Text, CR, LF, BS, CUP, CHA, CUF, EL, ECH and `DECSTBM`'s home;
        /// other sequences change no text.
        fn process(&mut self, bytes: &[u8]) {
            let mut chars = std::str::from_utf8(bytes).expect("utf-8").chars();
            while let Some(ch) = chars.next() {
                if ch != '\x1b' {
                    match ch {
                        '\r' => self.go(0),
                        '\n' => self.line_feed(),
                        '\x08' => self.go(self.at.1.saturating_sub(1)),
                        ch => self.put(ch),
                    }
                    if ch.is_control() {
                        self.last = None;
                    }
                    continue;
                }
                self.last = None;
                match chars.next() {
                    Some('[') => {}
                    Some(']') => {
                        while let Some(c) = chars.next() {
                            if c == '\x07' || (c == '\x1b' && chars.next().is_some()) {
                                break;
                            }
                        }
                        continue;
                    }
                    _ => continue,
                }
                let mut params = String::new();
                let Some(fin) = chars.by_ref().find(|c| {
                    let fin = ('\x40'..='\x7e').contains(c);
                    if !fin {
                        params.push(*c);
                    }
                    fin
                }) else {
                    return;
                };
                let n: Vec<usize> = params
                    .split(';')
                    .map(|p| p.parse().unwrap_or(1).max(1))
                    .collect();
                let row = self.at.0 * self.cols;
                match fin {
                    'H' => {
                        self.at.0 = (n[0] - 1).min(self.rows - 1);
                        self.go((n.get(1).map_or(0, |c| c - 1)).min(self.cols - 1));
                    }
                    'G' => self.go((n[0] - 1).min(self.cols - 1)),
                    'C' => self.go((self.at.1 + n[0]).min(self.cols - 1)),
                    'K' => (self.at.1..self.cols).for_each(|c| self.erase(row + c)),
                    'X' => (self.at.1..(self.at.1 + n[0]).min(self.cols))
                        .for_each(|c| self.erase(row + c)),
                    'r' => {
                        self.at.0 = 0;
                        self.go(0);
                    }
                    _ => {}
                }
            }
        }

        fn rows(&self) -> Vec<String> {
            self.cells
                .chunks(self.cols)
                .map(|row| row.concat().trim_end().to_string())
                .collect()
        }
    }

    /// Each row's text as vt100 shows it.
    fn rows_on_vt100(bytes: &[u8], rows: u16, cols: u16) -> Vec<String> {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(bytes);
        parser
            .screen()
            .rows(0, cols)
            .map(|row| row.trim_end().to_string())
            .collect()
    }

    /// The client draws ❤️, two cells to Neovim, over two spaces, placing
    /// the next cell by CHA; vt100 holds it in one column, the blank after.
    const HEART: &str = "\x1b[1;1H\x1b[0ma  \x08\x08\u{2764}\u{fe0f}\x1b[4Gbcdef\x1b[2;1HABCDEFGH";

    /// Written adjacent, the rest of the row would land a column right on a
    /// terminal drawing ❤️ two wide, and the blank after it would break it.
    #[test]
    fn an_emoji_is_painted_where_the_grid_has_it_however_wide_it_is_drawn() {
        let heart = "\u{2764}\u{fe0f}";
        let mut s = Shadow::new(2, 8);
        s.feed(HEART.as_bytes());
        let mut fade = s.frame(0.0, &palette(), Cursor::Hidden);
        fade.extend(s.frame(0.5, &palette(), Cursor::Hidden));
        let mut under = s.frame_under(&|_, _| Shade::Faint, None, &[], Cursor::Hidden);
        let falloff = |_, col| Shade::Toward(f32::from(col) / 10.0);
        under.extend(s.frame_under(&falloff, Some(&palette()), &[], Cursor::Hidden));
        let over = Over {
            top: 0,
            left: 0,
            width: 8,
            rows: vec![" ".repeat(8); 2],
        };
        let crossed = s.under(&over, &palette(), Rgb(9, 9, 9), 1.0);
        let kept = s.kept_bytes(&Kept::new([(0, 0, 7), (1, 0, 7)], []), &palette());
        for (name, bytes) in [
            ("paint", s.paint()),
            ("fade", fade),
            ("under", under),
            ("crossed", crossed),
            ("kept", kept),
        ] {
            assert_eq!(
                rows_on_vt100(&bytes, 2, 8),
                [format!("a{heart} bcdef"), "ABCDEFGH".into()],
                "{name}, one column wide"
            );
            let mut two = Graphemes::new(2, 8);
            two.process(&bytes);
            assert_eq!(
                two.rows(),
                [format!("a{heart}bcdef"), "ABCDEFGH".into()],
                "{name}, two columns wide"
            );
        }
    }

    /// Drawn two wide, ❤️ runs over the floating screen's first cell, which a
    /// frame that changes only the emoji would leave unwritten.
    #[test]
    fn a_floating_screen_beside_an_emoji_keeps_its_first_cell() {
        let over = [laid(2, 0, 4, 2, "WXYZ")];
        let first = |_, _| Shade::Toward(0.2);
        let second = |_, col| Shade::Toward(if col == 1 { 0.5 } else { 0.2 });
        let mut s = Shadow::new(2, 8);
        s.feed(HEART.as_bytes());
        let mut bytes = s.frame_under(&first, Some(&palette()), &over, Cursor::Hidden);
        let diff = s.frame_under(&second, Some(&palette()), &over, Cursor::Hidden);
        assert!(text(&diff).contains('\u{2764}'), "{:?}", text(&diff));
        bytes.extend(diff);
        let mut fresh = Shadow::new(2, 8);
        fresh.feed(HEART.as_bytes());
        let whole = fresh.frame_under(&second, Some(&palette()), &over, Cursor::Hidden);

        let (mut two, mut want) = (Graphemes::new(2, 8), Graphemes::new(2, 8));
        two.process(&bytes);
        want.process(&whole);
        assert_eq!(two.rows(), want.rows(), "two columns wide");
        assert_eq!(two.rows()[0], "a WXYZef");
        assert_eq!(
            rows_on_vt100(&bytes, 2, 8),
            rows_on_vt100(&whole, 2, 8),
            "one column wide"
        );
    }

    /// A terminal drawing East Asian Ambiguous characters wide would push
    /// the rest of the row along.
    #[test]
    fn the_cell_after_an_ambiguous_character_is_placed_again() {
        let mut s = Shadow::new(2, 8);
        s.feed("a\u{2500}b".as_bytes());
        let over = Over {
            top: 0,
            left: 0,
            width: 8,
            rows: vec![" ".repeat(8)],
        };
        for bytes in [
            s.paint(),
            s.frame(0.0, &palette(), Cursor::Hidden),
            s.frame_under(&|_, _| Shade::Faint, None, &[], Cursor::Hidden),
            s.under(&over, &palette(), Rgb(9, 9, 9), 1.0),
        ] {
            let text = text(&bytes);
            assert!(text.contains("a\u{2500}\x1b[3Gb"), "{text:?}");
        }
    }

    /// The client's bytes for a screen given cell by cell, "" for the right
    /// half of the wide cell before it.
    fn client_bytes(rows: &[&[&str]]) -> Vec<u8> {
        use crate::client::compose::Frame;
        use crate::client::grid::Text;
        use crate::client::screen::{Cursor as Shown, Screen};
        use crate::client::style::Style;

        let cols = rows[0].len();
        let mut f = Frame::new(cols, rows.len(), Style::default());
        for (r, row) in rows.iter().enumerate() {
            for (c, text) in row.iter().enumerate() {
                f.cells[r * cols + c].text = Text::new(text);
                if text.is_empty() {
                    f.cells[r * cols + c - 1].wide = true;
                }
            }
        }
        let hidden = Shown {
            visible: false,
            row: 0,
            col: 0,
            shape: 2,
            color: None,
        };
        Screen::new().draw(f, &hidden, false, &[])
    }

    /// vt100 holds a flag in two cells. WezTerm draws each half alone, two
    /// wide, if anything comes between them.
    #[test]
    fn a_flag_is_written_whole_by_every_painter() {
        let (jp, us, lone) = ("\u{1f1ef}\u{1f1f5}", "\u{1f1fa}\u{1f1f8}", "\u{1f1fa}");
        let drawn = client_bytes(&[
            &["a", jp, "", "b", lone, "", "c", "d"],
            &[jp, "", us, "", lone, "", "x", "y"],
        ]);
        let mut two = Graphemes::new(2, 8);
        two.process(&drawn);
        assert_eq!(
            two.rows(),
            [format!("a{jp}b{lone}cd"), format!("{jp}{us}{lone}xy")]
        );
        let want = (rows_on_vt100(&drawn, 2, 8), two.rows());
        let mut s = Shadow::new(2, 8);
        s.feed(&drawn);
        let mut fade = s.frame(0.0, &palette(), Cursor::Hidden);
        fade.extend(s.frame(0.5, &palette(), Cursor::Hidden));
        let mut under = s.frame_under(&|_, _| Shade::Faint, None, &[], Cursor::Hidden);
        let falloff = |_, col| Shade::Toward(f32::from(col) / 10.0);
        under.extend(s.frame_under(&falloff, Some(&palette()), &[], Cursor::Hidden));
        let over = Over {
            top: 0,
            left: 0,
            width: 8,
            rows: vec![" ".repeat(8); 2],
        };
        let crossed = s.under(&over, &palette(), Rgb(9, 9, 9), 1.0);
        let kept = s.kept_bytes(&Kept::new([(0, 0, 7), (1, 0, 7)], []), &palette());
        for (name, bytes) in [
            ("paint", s.paint()),
            ("fade", fade),
            ("under", under),
            ("crossed", crossed),
            ("kept", kept),
        ] {
            let text = text(&bytes);
            assert!(text.contains(jp) && text.contains(us), "{name}: {text:?}");
            let mut two = Graphemes::new(2, 8);
            two.process(&bytes);
            let got = (rows_on_vt100(&bytes, 2, 8), two.rows());
            assert_eq!(got, want, "{name}: {text:?}");
        }
    }

    /// Half a flag would show a lone indicator, as half a wide character
    /// shows a broken glyph.
    #[test]
    fn a_flag_a_rectangle_cuts_is_blanked() {
        let (first, second) = ("\u{1f1ef}", "\u{1f1f5}");
        let mut s = Shadow::new(2, 8);
        s.feed(&client_bytes(&[
            &["a", "\u{1f1ef}\u{1f1f5}", "", "b", "c", "d", "e", "f"],
            &["g"; 8],
        ]));
        let notice = |left: u16, row: &str, t| {
            let over = Over {
                top: 0,
                left,
                width: row.chars().count() as u16,
                rows: vec![row.into()],
            };
            s.under(&over, &palette(), Rgb(9, 9, 9), t)
        };
        // The notice from the second half, up to the first, or with a glyph
        // on either; what stands from the second half.
        let mut cut = vec![
            (notice(2, "   ", 1.0), second),
            (notice(0, "  ", 1.0), first),
            (notice(0, "  x ", 0.0), first),
            (notice(0, " x  ", 0.0), second),
            (
                s.kept_bytes(&Kept::new([(0, 2, 5)], []), &palette()),
                second,
            ),
        ];
        // A floating screen over either half.
        for (over, half) in [
            (laid(2, 0, 3, 1, "xyz"), first),
            (laid(0, 0, 2, 1, "xy"), second),
        ] {
            let faint = |_, _| Shade::Faint;
            cut.push((s.frame_under(&faint, None, &[over], Cursor::Hidden), half));
        }
        for (bytes, half) in cut {
            let text = text(&bytes);
            assert!(!text.contains(half), "{text:?}");
        }
    }

    /// Where ❤️ is drawn one column wide, its pad shows in its own pen.
    #[test]
    fn a_pad_is_written_again_in_its_own_pen_when_only_it_changes() {
        let heart = "\u{2764}\u{fe0f}";
        // Underlined, so the pad's fade shows; only the emoji stands.
        let mut s = Shadow::new(2, 8);
        s.feed(format!("\x1b[1;1H\x1b[0ma\x1b[4m  \x08\x08{heart}\x1b[0m\x1b[4Gbcdef").as_bytes());
        let kept = Kept::new([(0, 0, 1)], []);
        s.frame_with(|_| 0.0, &palette(), Cursor::Hidden, None, &kept);
        let later = text(&s.frame_with(|_| 0.5, &palette(), Cursor::Hidden, None, &kept));
        let pads = format!("\x1b[0;4;38;2;100;100;100m  \x08\x08\x1b[0;4;38;2;200;200;200m{heart}");
        assert!(later.contains(&pads), "{later:?}");

        // In a background, so the pad's shade shows; only its shade changes.
        let mut s = Shadow::new(2, 8);
        s.feed(format!("\x1b[1;1H\x1b[0;44ma  \x08\x08{heart}\x1b[4Gbcdef").as_bytes());
        let pad_at = |t| move |_, col| Shade::Toward(if col == 2 { t } else { 0.0 });
        s.frame_under(&pad_at(0.2), Some(&palette()), &[], Cursor::Hidden);
        let later = text(&s.frame_under(&pad_at(0.4), Some(&palette()), &[], Cursor::Hidden));
        let pads = format!(
            "\x1b[0;38;2;120;120;120;48;2;0;0;120m  \x08\x08\x1b[0;38;2;200;200;200;48;2;0;0;200m{heart}"
        );
        assert!(later.contains(&pads), "{later:?}");

        // A pad that shows nothing takes no pen of its own.
        let mut s = Shadow::new(2, 8);
        s.feed(HEART.as_bytes());
        let fade = text(&s.frame(0.0, &palette(), Cursor::Hidden));
        assert!(fade.contains(&format!("a  \x08\x08{heart}")), "{fade:?}");
        let shaded = |_, col| Shade::Toward(f32::from(col) / 10.0);
        let under = text(&s.frame_under(&shaded, Some(&palette()), &[], Cursor::Hidden));
        assert!(
            under.contains(&format!("\x1b[0;38;2;180;180;180m  \x08\x08{heart}")),
            "{under:?}"
        );
    }

    /// A terminal drawing the halves in a cell each keeps the second under a
    /// space over the first.
    #[test]
    fn a_flag_its_shade_blanks_is_blanked_whole() {
        let mut s = Shadow::new(2, 8);
        s.feed(&client_bytes(&[
            &["a", "\u{1f1ef}\u{1f1f5}", "", "b", "c", "d", "e", "f"],
            &["g"; 8],
        ]));
        let lit = |_, _| Shade::Toward(0.0);
        let mut bytes = s.frame_under(&lit, Some(&palette()), &[], Cursor::Hidden);
        let first_blank = |_, col| {
            if col == 1 {
                Shade::Blank
            } else {
                Shade::Toward(0.0)
            }
        };
        bytes.extend(s.frame_under(&first_blank, Some(&palette()), &[], Cursor::Hidden));
        assert_eq!(
            rows_on_vt100(&bytes, 2, 8)[0],
            "a  bcdef",
            "{:?}",
            text(&bytes)
        );
    }

    /// vt100 holds 😀 wide; a terminal may draw it in one column.
    #[test]
    fn a_wide_emoji_is_painted_over_two_spaces() {
        let grin = "\u{1f600}";
        let mut s = Shadow::new(2, 8);
        s.feed(&client_bytes(&[
            &["a", grin, "", "b", "c", "d", "e", "f"],
            &["g"; 8],
        ]));
        let over = Over {
            top: 0,
            left: 0,
            width: 8,
            rows: vec![" ".repeat(8)],
        };
        for bytes in [
            s.paint(),
            s.frame(0.0, &palette(), Cursor::Hidden),
            s.frame_under(&|_, _| Shade::Faint, None, &[], Cursor::Hidden),
            s.under(&over, &palette(), Rgb(9, 9, 9), 1.0),
            s.kept_bytes(&Kept::new([(0, 0, 7)], []), &palette()),
        ] {
            let text = text(&bytes);
            assert!(text.contains(&format!("  \x08\x08{grin}")), "{text:?}");
        }
    }

    /// Random screens as the client draws them, one over another, through
    /// every painter (a fade and a frame under a floating screen twice, the
    /// second a diff): each shows what the client's bytes do, on vt100 and
    /// on a terminal drawing ❤️ and flags two wide. Keycaps are left out:
    /// one cell to Neovim, such a terminal shows one broken by a blank the
    /// client writes after it, which painters write under it instead.
    #[test]
    fn every_painter_shows_what_the_client_drew() {
        use crate::client::compose::Frame;
        use crate::client::grid::Text;
        use crate::client::screen::{Cursor as Shown, Screen};
        use crate::client::style::{Color, Style};

        let styles = [
            Style::default(),
            Style {
                fg: Color::Index(1),
                ..Style::default()
            },
            Style {
                bg: Color::Index(4),
                ..Style::default()
            },
        ];
        let letters = [
            " ",
            " ",
            " ",
            "a",
            "é",
            "─",
            "\u{a9}",
            "中",
            "\u{2764}\u{fe0f}",
            "\u{1f1ef}\u{1f1f5}",
            "\u{1f1fa}",
        ];
        let wide = ["中", "\u{2764}\u{fe0f}", "\u{1f1ef}\u{1f1f5}", "\u{1f1fa}"];
        // Fixed-seed xorshift: the same screens every run.
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let (w, h) = (11, 4);
        let hidden = Shown {
            visible: false,
            row: 0,
            col: 0,
            shape: 2,
            color: None,
        };
        let over = Over {
            top: 0,
            left: 0,
            width: w as u16,
            rows: vec![" ".repeat(w); h],
        };
        let falloff = |_, col| Shade::Toward(f32::from(col % 5) / 10.0);
        let mut random = || {
            let mut f = Frame::new(w, h, Style::default());
            for r in 0..h {
                let mut c = 0;
                while c < w {
                    let at = r * w + c;
                    let text = letters[next(letters.len())];
                    f.cells[at].text = Text::new(text);
                    f.cells[at].style = styles[next(styles.len())];
                    if wide.contains(&text) {
                        if c + 1 == w {
                            f.cells[at].text = Text::Char('x');
                        } else {
                            f.cells[at].wide = true;
                            f.cells[at + 1].text = Text::Half;
                            f.cells[at + 1].style = f.cells[at].style;
                            c += 1;
                        }
                    }
                    c += 1;
                }
            }
            f
        };
        for round in 0..200 {
            let mut client = Screen::new();
            let mut drawn = Vec::new();
            for step in ["fresh", "over it"] {
                drawn.extend(client.draw(random(), &hidden, false, &[]));
                let mut s = Shadow::new(h as u16, w as u16);
                s.feed(&drawn);
                let mut two = Graphemes::new(h, w);
                two.process(&drawn);
                let want = (rows_on_vt100(&drawn, h as u16, w as u16), two.rows());

                let mut fade = s.frame_with(
                    |r| f32::from(r % 2) / 4.0,
                    &palette(),
                    Cursor::Hidden,
                    None,
                    &Kept::default(),
                );
                fade.extend(s.frame_with(
                    |r| f32::from(r % 3) / 4.0,
                    &palette(),
                    Cursor::Hidden,
                    None,
                    &Kept::default(),
                ));
                let mut under = s.frame_under(&|_, _| Shade::Faint, None, &[], Cursor::Hidden);
                under.extend(s.frame_under(&falloff, Some(&palette()), &[], Cursor::Hidden));
                let kept = Kept::new((0..h).map(|r| (r, 0, w - 1)), []);
                for (name, bytes) in [
                    ("paint", s.paint()),
                    ("fade", fade),
                    ("under", under),
                    ("crossed", s.under(&over, &palette(), Rgb(9, 9, 9), 1.0)),
                    ("kept", s.kept_bytes(&kept, &palette())),
                ] {
                    let mut two = Graphemes::new(h, w);
                    two.process(&bytes);
                    let got = (rows_on_vt100(&bytes, h as u16, w as u16), two.rows());
                    assert_eq!(
                        got,
                        want,
                        "round {round} {step}, {name}: {:?}",
                        text(&drawn)
                    );
                }
            }
        }
    }

    #[test]
    fn the_mouse_is_as_the_session_last_set_it() {
        use crate::term::MouseReporting;
        let mut s = Shadow::new(2, 4);
        assert_eq!(s.mouse(), Some(MouseReporting::Off));
        s.feed(b"\x1b[?1002h\x1b[?1006h");
        assert_eq!(s.mouse(), Some(MouseReporting::Buttons));
        s.feed(b"\x1b[?1003h");
        assert_eq!(s.mouse(), Some(MouseReporting::Motion));
        s.feed(b"\x1b[?1003l");
        assert_eq!(
            s.mouse(),
            Some(MouseReporting::Off),
            "one off is all of it off"
        );
    }
}
