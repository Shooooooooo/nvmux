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
//! # Painters
//!
//! - [`Shadow::under`]: cross-fades an overlay rectangle with the cells under
//!   it, so the notice melts into the editor even if no repaint comes.
//! - [`Shadow::frame`]: every colour `t` toward the background, resolved
//!   through [`Palette`]. A default background stays `49`: the terminal may
//!   be transparent.
//! - [`Shadow::paint`]: the grid exactly as drawn, for a kept client
//!   returning without a fade; the server's `:mode` repaint lands on top.
//! - [`Shadow::frame_under`]: the floating settings over the shaded session
//!   (see `crate::pty::panel`).
//!
//! Frames are diffs (only `t` changes), so a whole fade costs about one full
//! repaint. Runs of empty cells are erased with `EL` or `ECH` under a pen
//! with no background. Like the attach notice ([`crate::announce`]), every
//! frame is one synchronized update, places runs absolutely, never writes a
//! newline or CR (`OPOST` is off), and hides the cursor while writing.
//!
//! # The cursor
//!
//! A fade in is nvmux's last write, and the repaint after it is not certain
//! (a busy server may not answer; an idle editor never redraws), so its last
//! frame restores the cursor itself ([`Cursor::Restored`]). A fade out ends
//! hidden; whatever takes the screen next shows it.

use std::io::Write;

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

/// What a frame with the settings over the session does with a session cell
/// (see [`Shadow::frame_under`]).
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

/// A rectangle nvmux draws over the session in place of its cells: the
/// floating settings (see `crate::pty::panel`), from the buffer their screen
/// drew.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Laid {
    top: u16,
    left: u16,
    width: u16,
    height: u16,
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
            text,
            cells,
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
/// attribute the parser holds except inverse (folded into the colours), or
/// the fade would snap shape at the end (italics dissolving upright).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Painted {
    fg: Rgb,
    /// `None` is the terminal's default background, left as `49`.
    bg: Option<Rgb>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
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
            underline: false,
        }
    }

    /// Whether a space in this pen is invisible: default background, no
    /// underline.
    fn shows_nothing(&self) -> bool {
        self.bg.is_none() && !self.underline
    }
}

pub struct Shadow {
    parser: vt100::Parser,
    /// Keeps the parser's OSC buffer small: it ignores OSCs, and an `OSC 52`
    /// copy would otherwise stay buffered for the client's lifetime (see
    /// [`crate::ledger::OscCap`]).
    osc: crate::ledger::OscCap,
    /// A partial character at the end of the last feed, held back until the
    /// rest comes (see [`whole`]).
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
        Self {
            parser: vt100::Parser::new(rows.max(MIN_SIZE), cols.max(MIN_SIZE), 0),
            osc: crate::ledger::OscCap::default(),
            cut: Vec::new(),
            painted: Vec::new(),
            under: Vec::new(),
            under_valid: false,
            broken: false,
        }
    }

    /// Parse a copy of session output. A trailing partial character waits for
    /// the rest (see [`whole`]); OSC payload past [`crate::ledger::OSC_SHOWN`]
    /// is dropped. A parser panic is caught and retires the shadow, so the
    /// session cuts out instead of dissolving.
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.broken {
            return;
        }
        let shown = self.osc.cap(bytes);
        // Prepend the partial character the last feed ended with.
        let joined;
        let bytes = if self.cut.is_empty() {
            &shown[..]
        } else {
            joined = [std::mem::take(&mut self.cut).as_slice(), &shown].concat();
            joined.as_slice()
        };
        let end = whole(bytes);
        self.cut.extend_from_slice(&bytes[end..]);
        self.guard(|parser| parser.process(&bytes[..end]));
        self.invalidate();
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if self.broken {
            return;
        }
        self.guard(|parser| {
            parser
                .screen_mut()
                .set_size(rows.max(MIN_SIZE), cols.max(MIN_SIZE));
        });
        self.invalidate();
    }

    fn guard(&mut self, f: impl FnOnce(&mut vt100::Parser)) {
        let parser = &mut self.parser;
        if crate::panics::caught(|| f(parser)).is_err() {
            tracing::warn!("the shadow grid's parser panicked; the session will not dissolve");
            self.broken = true;
        }
    }

    /// `(rows, cols)`.
    pub fn size(&self) -> (u16, u16) {
        self.parser.screen().size()
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
    /// paints every cell. The fade diff is freed, not cleared (11 bytes a
    /// cell while a kept client is parked); the settings' is kept for reuse
    /// until [`Shadow::forget_under`].
    pub fn invalidate(&mut self) {
        self.painted = Vec::new();
        self.under_valid = false;
    }

    /// Free the settings' diff once they close.
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
        let screen = self.parser.screen();
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
            let mut at: Option<u16> = None;
            let mut run = Run::default();
            for col in 0..cols {
                let cell = screen.cell(row, col);
                let index = next.len();
                let painted = if kept.holds(row, col) {
                    standing(cell, palette, t)
                } else {
                    resolve(cell, palette, t)
                };
                next.push(painted);
                // The second half of a wide character, covered by the first.
                let wide_rest = cell.is_some_and(vt100::Cell::is_wide_continuation);
                let (text, width) = match cell {
                    Some(c) if c.has_contents() => (c.contents(), if c.is_wide() { 2 } else { 1 }),
                    _ => (" ", 1),
                };
                let same = diff && self.painted[index] == painted;
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
                if wide_rest || same {
                    continue;
                }
                if at != Some(col) {
                    let over = sgr.is_some_and(|p| p.shows_nothing());
                    move_to(&mut out, (row, col), at, over, &bare);
                }
                if sgr != Some(painted) {
                    write_sgr(&mut out, painted);
                    sgr = Some(painted);
                }
                out.extend_from_slice(text.as_bytes());
                at = Some(col + width);
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
        let screen = self.parser.screen();
        let mut out = SAVE_CURSOR.to_vec();
        let mut drawn = false;
        for &(row, from, to) in &kept.spans {
            for col in from..=to {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                if !cell.has_contents() || cell.is_wide_continuation() {
                    continue;
                }
                let painted = Painted {
                    bg: None,
                    ..resolve(Some(cell), palette, 0.0)
                };
                let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);
                write_sgr(&mut out, painted);
                out.extend_from_slice(cell.contents().as_bytes());
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
    /// colour when it thinks neither moved. Attributes beyond bold or dim,
    /// italic, underline and inverse wait for the server's repaint.
    pub fn paint(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(SYNC_BEGIN);
        out.extend_from_slice(HIDE_CURSOR);
        if !self.broken {
            let screen = self.parser.screen();
            let (rows, cols) = screen.size();
            let mut pen: Option<Pen> = None;
            for row in 0..rows {
                let _ = write!(out, "\x1b[{};1H", row + 1);
                let mut col = 0;
                while col < cols {
                    let cell = screen.cell(row, col);
                    // Written by the first half; a lone one has nothing to
                    // write.
                    if cell.is_some_and(vt100::Cell::is_wide_continuation) {
                        col += 1;
                        continue;
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
                            out.extend_from_slice(c.contents().as_bytes());
                            col += if c.is_wide() { 2 } else { 1 };
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
        let mut out = RESET_SGR.to_vec();
        if self.broken {
            return out;
        }
        let screen = self.parser.screen();
        // A reset, and then the session's pen from there.
        out.extend_from_slice(&screen.attributes_formatted());
        let (row, col) = screen.cursor_position();
        let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);
        out
    }

    /// The bytes that cross-fade `over` with the session's cells under it:
    /// overlay glyphs `t` dissolved from `ink` (see [`crate::theme`]), session
    /// cells `1 - t`. At `t = 0` it looks like the notice's own spaces; at
    /// `t = 1` the screen is as found, so the notice needs no repaint to go.
    ///
    /// Unlike [`Shadow::frame`], it paints over a session that owns the
    /// screen, so it leaves cursor and attributes as found (`DECSC`/`DECRC`,
    /// and no cursor hide, which would blink), keeps no diff, and never
    /// writes outside the rectangle. The shared save slot is safe as in
    /// [`crate::announce`]: output only goes between the session's sequences
    /// (see [`crate::boundary`]). Cells past the grid's end paint as blanks.
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
        let screen = self.parser.screen();
        // The overlay's glyphs while more visible, then the session's cells.
        let overlaid = t < CROSSOVER;
        let ink = Painted {
            fg: ink.lerp(palette.bg, t),
            bg: None,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
        };
        // The stream's SGR, carried across rows as in `frame`.
        let mut sgr: Option<Painted> = None;
        for (i, row) in over.rows.iter().enumerate() {
            let Ok(line) = u16::try_from(i).map(|i| over.top + i) else {
                break;
            };
            let laid = laid_out(row, over.width);
            let mut at: Option<u16> = None;
            let mut c = 0u16;
            while c < over.width {
                let col = over.left + c;
                let glyph = laid[usize::from(c)]
                    .as_deref()
                    .filter(|g| overlaid && !is_blank(g));
                let (text, painted, width) = match glyph {
                    // The overlay's glyph in its one colour; the notice sets
                    // no background, so the box keeps the reset's.
                    Some(g) => (g, ink, glyph_width(g)),
                    // The session's cell. A wide character the rectangle cuts
                    // is blanked: half would be a broken glyph, and the other
                    // half is outside the rectangle.
                    None => {
                        let cell = screen.cell(line, col);
                        let painted = resolve(cell, palette, beneath(line));
                        match cell {
                            Some(cell) if cell.is_wide_continuation() => (" ", painted, 1),
                            Some(cell) if cell.is_wide() && c + 2 > over.width => (" ", painted, 1),
                            Some(cell) if cell.has_contents() => {
                                let w = if cell.is_wide() { 2 } else { 1 };
                                (cell.contents(), painted, w)
                            }
                            _ => (" ", painted, 1),
                        }
                    }
                };
                if at != Some(col) {
                    let _ = write!(out, "\x1b[{};{}H", line + 1, col + 1);
                }
                if sgr != Some(painted) {
                    write_sgr(out, painted);
                    sgr = Some(painted);
                }
                out.extend_from_slice(text.as_bytes());
                at = Some(col + width);
                c += width;
            }
        }
    }
}

impl Shadow {
    /// A frame of the session with `laid` (the floating settings) over it and
    /// every other cell as `shade` says. Without `laid`, its cells are shaded
    /// too, which is how they blank for it to come up on.
    ///
    /// A diff like [`Shadow::frame`]'s, in full after a [`feed`], [`resize`]
    /// or [`invalidate`] (so after any session output). Always at least the
    /// sync span, cursor hide and trailing SGR reset.
    ///
    /// A wide character cut by either edge of the rectangle is blanked.
    /// `Shade::Toward` without a palette is drawn faint.
    ///
    /// [`feed`]: Shadow::feed
    /// [`resize`]: Shadow::resize
    /// [`invalidate`]: Shadow::invalidate
    pub fn frame_under(
        &mut self,
        shade: &dyn Fn(u16, u16) -> Shade,
        palette: Option<&Palette>,
        laid: Option<&Laid>,
        cursor: Cursor,
    ) -> Vec<u8> {
        let screen = self.parser.screen();
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
            let mut at: Option<u16> = None;
            let mut run = Run::default();
            // The previous cell was a session wide character written whole;
            // this is its second half.
            let mut wide_before = false;
            for col in 0..cols {
                let cell = screen.cell(row, col);
                let drawn = match laid.and_then(|l| l.at(row, col)) {
                    Some(inside) => {
                        wide_before = false;
                        inside
                    }
                    None if cell.is_some_and(vt100::Cell::is_wide_continuation) => {
                        let written = std::mem::take(&mut wide_before);
                        (!written).then_some((" ", Pen::BLANK))
                    }
                    None => {
                        let wide = cell.is_some_and(vt100::Cell::is_wide);
                        let cut =
                            wide && (col + 2 > cols || laid.is_some_and(|l| l.holds(row, col + 1)));
                        wide_before = wide && !cut;
                        Some(shaded(cell, shade(row, col), palette, cut))
                    }
                };
                let index = usize::from(row) * usize::from(cols) + usize::from(col);
                let same = diff && self.under[index].is(drawn);
                if !same {
                    self.under[index] = Drawn::of(drawn);
                }
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
                if at != Some(col) {
                    let over = pen.is_some_and(|p| p.shows_nothing());
                    move_to(&mut out, (row, col), at, over, &bare);
                }
                if pen != Some(this) {
                    this.write(&mut out);
                    pen = Some(this);
                }
                out.extend_from_slice(text.as_bytes());
                let width = u16::try_from(UnicodeWidthStr::width(text).max(1)).unwrap_or(1);
                at = Some(col + width);
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

/// A session cell as a settings-over-session frame draws it (see
/// [`Shadow::frame_under`]): its text (blank if `cut`, a wide character the
/// rectangle's edge cuts) and its pen as `shade` says.
fn shaded<'a>(
    cell: Option<&'a vt100::Cell>,
    shade: Shade,
    palette: Option<&Palette>,
    cut: bool,
) -> (&'a str, Pen) {
    let text = match cell {
        Some(c) if c.has_contents() && !cut => c.contents(),
        _ => " ",
    };
    // A space `resolve` would give no background or underline at any `t`:
    // most cells, which end as the blank pen, so skip resolving colours.
    let bare = text == " "
        && cell.is_none_or(|c| {
            c.bgcolor() == vt100::Color::Default
                && !c.inverse()
                && !(c.underline() && c.has_contents())
        });
    match (shade, palette) {
        (Shade::Blank, _) => (" ", Pen::BLANK),
        (Shade::Toward(t), _) if t >= falloff::EMPTIED => (" ", Pen::BLANK),
        (Shade::Toward(_), Some(_)) if bare => (" ", Pen::BLANK),
        (Shade::Toward(t), Some(palette)) => {
            let pen = Pen::painted(resolve(cell, palette, t));
            // A space without underline or background shows no colour, so
            // use the blank pen rather than an SGR per falloff shade.
            let bare = text == " " && !pen.underline && pen.bg == vt100::Color::Default;
            (text, if bare { Pen::BLANK } else { pen })
        }
        // Faint, or no palette: the cell's own colours, its text faint.
        (Shade::Toward(_) | Shade::Faint, _) => {
            let mut pen = Pen::of(cell);
            pen.dim |= cell.is_some_and(vt100::Cell::has_contents);
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
    underline: bool,
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
        underline: false,
        inverse: false,
    };

    /// Whether a space in this pen is invisible: default background, not
    /// inverse, no underline.
    fn shows_nothing(&self) -> bool {
        self.bg == vt100::Color::Default && !self.underline && !self.inverse
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
            underline: m.contains(Modifier::UNDERLINED),
            inverse: m.contains(Modifier::REVERSED),
        }
    }

    /// The cell's own, or the terminal's defaults for a cell past the grid.
    fn of(cell: Option<&vt100::Cell>) -> Self {
        match cell {
            Some(c) => Pen {
                fg: c.fgcolor(),
                bg: c.bgcolor(),
                bold: c.bold(),
                dim: c.dim(),
                italic: c.italic(),
                underline: c.underline(),
                inverse: c.inverse(),
            },
            None => Pen {
                fg: vt100::Color::Default,
                bg: vt100::Color::Default,
                bold: false,
                dim: false,
                italic: false,
                underline: false,
                inverse: false,
            },
        }
    }

    /// One SGR from a reset, so default colours go unmentioned and indexed
    /// ones stay indices for the terminal's palette.
    fn write(self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b[0");
        for (on, code) in [
            (self.bold, 1),
            (self.dim, 2),
            (self.italic, 3),
            (self.underline, 4),
            (self.inverse, 7),
        ] {
            if on {
                let _ = write!(out, ";{code}");
            }
        }
        write_colour(out, self.fg, 30, 90, 38);
        write_colour(out, self.bg, 40, 100, 48);
        out.push(b'm');
    }
}

/// What a settings-over-session frame last painted in a cell (see
/// [`Shadow::frame_under`]): its text inline, and its pen. Kept per cell
/// across frames, so a frame allocates nothing per cell.
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
    /// Text too long to keep (only a settings cluster could be); never
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
        at: &mut Option<u16>,
        blank: impl FnOnce(&mut Vec<u8>),
    ) {
        let Some((start, n)) = self.0.take() else {
            return;
        };
        if *at != Some(start) {
            let _ = write!(out, "\x1b[{};{}H", row + 1, start + 1);
        }
        blank(out);
        if start + n == cols && n > 3 {
            out.extend_from_slice(b"\x1b[K");
            *at = Some(start);
        } else if n >= ECH_FROM {
            let _ = write!(out, "\x1b[{n}X");
            *at = Some(start);
        } else {
            out.extend(std::iter::repeat_n(b' ', usize::from(n)));
            *at = Some(start + n);
        }
    }
}

/// Move the cursor from `at` to `col` on `row`: by writing spaces over the
/// cells between if they all show nothing (`bare`), the pen does too
/// (`over`), and there are at most [`GAP`]; otherwise with a `CUP`.
fn move_to(out: &mut Vec<u8>, (row, col): (u16, u16), at: Option<u16>, over: bool, bare: &[bool]) {
    let gap = at.filter(|&from| {
        over && from < col
            && col - from <= GAP
            && bare[usize::from(from)..usize::from(col)].iter().all(|&b| b)
    });
    match gap {
        Some(from) => out.extend(std::iter::repeat_n(b' ', usize::from(col - from))),
        None => {
            let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);
        }
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

/// Set the pen to one blank cells under the settings can be written or
/// erased in, unless it already shows nothing.
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
fn standing(cell: Option<&vt100::Cell>, palette: &Palette, t: f32) -> Painted {
    Painted {
        bg: resolve(cell, palette, t).bg,
        ..resolve(cell, palette, 0.0)
    }
}

/// A cell `t` of the way to the background. Blank cells (nothing written, or
/// a space with no background or underline) get the background as foreground
/// and no attributes, so the diff skips them; Neovim underlines blanks with
/// written spaces, so those are glyphs. Inverse is applied here, so the
/// terminal never inverts an interpolated pair.
fn resolve(cell: Option<&vt100::Cell>, palette: &Palette, t: f32) -> Painted {
    let Some(cell) = cell else {
        return Painted::blank(palette);
    };
    let fg = match cell.fgcolor() {
        vt100::Color::Default => palette.fg,
        vt100::Color::Idx(n) => palette.index(n),
        vt100::Color::Rgb(r, g, b) => Rgb(r, g, b),
    };
    let bg = match cell.bgcolor() {
        vt100::Color::Default => None,
        vt100::Color::Idx(n) => Some(palette.index(n)),
        vt100::Color::Rgb(r, g, b) => Some(Rgb(r, g, b)),
    };
    let (fg, bg) = if cell.inverse() {
        (bg.unwrap_or(palette.bg), Some(fg))
    } else {
        (fg, bg)
    };
    let blank =
        !cell.has_contents() || (cell.contents() == " " && bg.is_none() && !cell.underline());
    Painted {
        fg: if blank {
            palette.bg
        } else {
            fg.lerp(palette.bg, t)
        },
        bg: bg.map(|b| b.lerp(palette.bg, t)),
        bold: cell.bold() && !blank,
        dim: cell.dim() && !blank,
        italic: cell.italic() && !blank,
        underline: cell.underline() && !blank,
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

/// How much of `bytes` is whole characters. `vte` completes a partial
/// character with the next bytes and drops any it skips: a read ending inside
/// the `é` of `é ─` loses the space, shifting the rest of the row a cell left.
fn whole(bytes: &[u8]) -> usize {
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
/// unmentioned.
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
    if p.underline {
        out.extend_from_slice(b";4");
    }
    let _ = write!(out, ";38;2;{};{};{}", p.fg.0, p.fg.1, p.fg.2);
    if let Some(bg) = p.bg {
        let _ = write!(out, ";48;2;{};{};{}", bg.0, bg.1, bg.2);
    }
    out.push(b'm');
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

    /// One `((row, col), sgr, text)` per glyph a frame writes, 1-based.
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
                'm' => sgr = body[..end].to_string(),
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
        let mut s = Shadow::new(1, 8);
        s.feed(b"\x1b[1mb\x1b[0;2md\x1b[0;3mi\x1b[0;4mu\x1b[0;3;4mx");
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
            assert!(sgr("d").starts_with("0;2;38;"), "dim at {t}");
            assert!(sgr("i").starts_with("0;3;38;"), "italic at {t}");
            assert!(sgr("u").starts_with("0;4;38;"), "underline at {t}");
            assert!(sgr("x").starts_with("0;3;4;38;"), "both at {t}");
        }
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
        crate::test_support::quietly(|| s.guard(|_| panic!("a parser bug")));
        assert!(wrapper_only(&s.frame(0.0, &palette(), Cursor::Restored)));
    }

    #[test]
    fn a_panicking_parser_retires_the_shadow() {
        let mut s = Shadow::new(4, 4);
        s.feed(b"ok");
        crate::test_support::quietly(|| s.guard(|_| panic!("a parser bug")));
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
              \x1b[4;1H\x1b[2mdim\x1b[0m \xe5\xad\x97\xe5\xad\x97 e\xcc\x81\
              \x1b[5;7H",
        );
        let painted = s.paint();

        let mut again = vt100::Parser::new(6, 30, 0);
        again.process(&painted);
        let (was, now) = (s.parser.screen(), again.screen());
        // Unwritten and space cells look the same, and a paint writes every
        // cell.
        let seen = |c: &vt100::Cell| {
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
        let frame = shadow.frame_under(&shade, Some(&palette()), Some(&panel), Cursor::Hidden);
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
        let blank = shadow.frame_under(&|_, _| Shade::Blank, None, None, Cursor::Hidden);
        assert!(emptied(&blank));
        let faint = shadow.frame_under(&|_, _| Shade::Faint, None, None, Cursor::Hidden);
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
        let toward = shadow.frame_under(&|_, _| Shade::Toward(0.5), None, None, Cursor::Hidden);
        assert_eq!(
            screen_of(&toward)[&(1, 2)].0,
            "0;2;31;44",
            "no palette: faint"
        );
        let gone = shadow.frame_under(
            &|_, _| Shade::Toward(falloff::EMPTIED),
            Some(&palette()),
            None,
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
            None,
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
            Some(&panel),
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
        shadow.frame_under(&shade, Some(&palette()), None, Cursor::Hidden);
        assert!(shadow.under.capacity() > 0);
        shadow.forget_under();
        assert_eq!(shadow.under.capacity(), 0);
        let again = shadow.frame_under(&shade, Some(&palette()), None, Cursor::Hidden);
        assert_eq!(screen_of(&again).len(), 6 * 20);
    }

    #[test]
    fn a_frame_under_the_settings_paints_only_what_changed() {
        let mut shadow = lettered();
        let shade = |_: u16, _: u16| Shade::Toward(0.5);
        let p = palette();
        let first = shadow.frame_under(
            &shade,
            Some(&p),
            Some(&laid(5, 2, 6, 2, "one")),
            Cursor::Hidden,
        );
        assert_eq!(screen_of(&first).len(), 6 * 20);
        let again = shadow.frame_under(
            &shade,
            Some(&p),
            Some(&laid(5, 2, 6, 2, "one")),
            Cursor::Hidden,
        );
        assert!(wrapper_only(&again), "{:?}", text(&again));
        let changed = shadow.frame_under(
            &shade,
            Some(&p),
            Some(&laid(5, 2, 6, 2, "ono")),
            Cursor::Hidden,
        );
        let cells = screen_of(&changed);
        assert_eq!(cells.len(), 1, "the last letter: {cells:?}");
        shadow.feed(b"\x1b[1;1Hz");
        let after = shadow.frame_under(
            &shade,
            Some(&p),
            Some(&laid(5, 2, 6, 2, "ono")),
            Cursor::Hidden,
        );
        assert_eq!(screen_of(&after).len(), 6 * 20, "all of it again");
    }

    #[test]
    fn a_wide_character_the_settings_cut_is_blanked() {
        let mut shadow = Shadow::new(2, 12);
        shadow.feed("ab漢cd漢".as_bytes());
        // Columns 3..6, 0-based: the first 漢 is at 2 and 3, the second at 6 and 7.
        let panel = laid(3, 0, 3, 1, "xyz");
        let frame = shadow.frame_under(
            &|_, _| Shade::Toward(0.0),
            Some(&palette()),
            Some(&panel),
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
        let hidden = shadow.frame_under(&|_, _| Shade::Blank, None, None, Cursor::Hidden);
        assert!(!text(&hidden).contains("\x1b[?25h"));
        let back = shadow.frame_under(
            &|_, _| Shade::Toward(0.0),
            Some(&palette()),
            None,
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
        let mut shadow = Shadow::new(1, 4);
        let frame = shadow.frame_under(
            &|_, _| Shade::Blank,
            None,
            Some(&Laid::from_buffer(&buf)),
            Cursor::Hidden,
        );
        let cells = screen_of(&frame);
        assert_eq!(cells[&(1, 1)], ("0;1;7;38;2;1;2;3".into(), "a".into()));
        assert_eq!(cells[&(1, 3)], ("0;2;4".into(), "c".into()));
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
