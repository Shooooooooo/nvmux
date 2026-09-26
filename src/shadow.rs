//! A shadow of an attached session's screen, kept so the session can be
//! dissolved on its way out.
//!
//! nvmux never renders Neovim. The `--remote-ui` client draws, and
//! [`crate::pty`] copies its bytes to the terminal without looking at them —
//! which is why every terminal feature the editor negotiates keeps working,
//! and why nvmux has no idea what is on the screen. A fade needs to know: to
//! move a cell toward the background it has to know what colour the cell is.
//!
//! So a [`Shadow`] is a terminal emulator with no terminal: the `vt100` crate's
//! parser, fed a copy of every byte the *session* sends the real terminal,
//! keeping a grid of cells with their colours and attributes. **It only
//! watches.** Nothing reaching the terminal is changed, delayed or dropped on
//! its account; the parser sees the same bytes a moment later, and if it
//! misreads one the only consequence is a frame that is slightly wrong: a
//! fade's, or, for a kept client, the screen put back on its return, until the
//! server's own repaint lands. It costs a parse of the session's output while
//! a session is attached, which is why `[fade] session = false` turns it off
//! entirely rather than merely not using it. A kept client
//! (`[client] per_session`, the default) pays that parse anyway, for its own
//! copy of its screen (see `pty::Attachment::paint_kept_screen`).
//!
//! The session's bytes, and not nvmux's own. What nvmux draws over a session —
//! the attach notice and the hint bar ([`crate::hint`]) — is deliberately
//! withheld, because what the shadow has to remember there is precisely what
//! is *underneath* it.
//! The grid is the screen the client drew, which for the notice's rectangle is
//! the only copy of it anywhere: the terminal's has been written over, and
//! only the server could otherwise say what was there.
//!
//! # What a composite paints
//!
//! That is what [`Shadow::under`] is for. Given a rectangle nvmux has drawn on
//! and how far through its dissolve it is, it paints the two together: the
//! overlay's own glyphs fading out, the session's cells fading back in, each
//! cell showing whichever of the two is the more visible. The notice then
//! melts into the editor instead of leaving a hole for a repaint to fill —
//! and where that repaint never lands, the screen has already been put back.
//!
//! # What a frame paints
//!
//! [`Shadow::frame`] writes the grid back to the terminal with every cell's
//! colours moved `t` of the way to the terminal's background — the colours
//! resolved first, through [`Palette`], so that "the default foreground" is a
//! real colour the interpolation can start from. A background the editor left
//! as the terminal's default is left as the default (`49`), never painted
//! explicitly: the terminal may be transparent, and an opaque slab of the
//! background colour would pop where the real background did not.
//!
//! Frames are diffs. The screen does not change between them, only `t` does,
//! so a cell whose interpolated colours came out the same as last frame is not
//! written again — which is every blank cell on a default background, most of
//! any screen. The whole fade then costs about one full repaint plus the cells
//! that change colour, and nvmux already emits a full repaint on every resize.
//!
//! What a frame must get right is the same list as the attach notice's
//! ([`crate::announce`]): a synchronized-update span around the whole thing,
//! an absolute cursor position for every run of cells and never a newline or
//! carriage return (`OPOST` is off), and the cursor hidden while the cells are
//! written, so it is not seen chasing them across the screen.
//!
//! # What a paint paints
//!
//! [`Shadow::paint`] is the one painter that is not a fade: the grid as the
//! session drew it, every cell in the colours and attributes it was given —
//! indexed colours as indices, the default as the default — with no palette
//! and nothing interpolated. It is for a kept client coming back to the front
//! with no fade to bring it (see `pty::Attachment::paint_kept_screen`): the
//! screen is on the glass at once, and the server's own repaint — asked for
//! with `:mode`, a few round trips away, or later if the server is busy —
//! lands on top. The same rules as a frame: one synchronized
//! update, an absolute placement per row and never a newline, the cursor put
//! back where the session had it.
//!
//! # The cursor
//!
//! Hiding the cursor is a debt. The client's own frames pay it back — Neovim
//! ends every flush by showing the cursor — but a fade in is nvmux's last word
//! on the screen, and what follows it is *asked for*, not certain: a resumed
//! client is asked to repaint through its server, and a server that is busy or
//! waiting for a key does not answer. The cursor would then stay hidden until
//! the editor next drew something, and an editor sitting idle draws nothing.
//! So the last frame of a fade in puts the cursor back itself, on the cell the
//! session's bytes left it on and shown if they left it shown
//! ([`Cursor::Restored`]): the parser tracks both, so the shadow knows exactly
//! what the client would have had on screen. A fade out ends with the cursor
//! hidden, as every frame before it did; what takes the screen next — a picker,
//! a fresh client, a fade in — shows it for itself.

use std::io::Write;
use std::panic::{self, AssertUnwindSafe};

use crate::fade::{SYNC_BEGIN, SYNC_END};
use crate::palette::{Palette, Rgb};
use unicode_width::UnicodeWidthChar;

const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
const RESET_SGR: &[u8] = b"\x1b[0m";
/// `DECSC` / `DECRC`. A single shared save slot, which is why only something
/// written between the session's own frames may use it — see [`Shadow::under`].
const SAVE_CURSOR: &[u8] = b"\x1b7";
const RESTORE_CURSOR: &[u8] = b"\x1b8";

/// Where a cell holding both an overlay glyph and a session cell swaps which
/// of the two it shows. Halfway, because that is where the two are equally
/// dissolved and the swap is the least that it can be.
const CROSSOVER: f32 = 0.5;

/// Where a frame leaves the cursor once its cells are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    /// Hidden, as it was while the cells were written. For every frame of a
    /// fade out and all but the last of a fade in: another frame follows, or
    /// the next owner of the screen shows the cursor for itself.
    Hidden,
    /// Put back where the session had it — on its cell, and shown if the
    /// session had it shown. For the last frame of a fade in, after which
    /// nothing else is certain to touch the screen (see the module docs).
    Restored,
}

/// The smallest grid the parser is given, in either dimension.
///
/// `vt100` 0.16 underflows on a one-row grid (and on a one-column one with a
/// wide character) when text wraps: `col_wrap` scrolls, then subtracts the
/// scroll from a row that is already zero. No real terminal is one row, and
/// Neovim cannot run in one, so the shadow is simply never that small; the
/// frame it paints then has a row the terminal clamps, which at that size is
/// nothing anyone can see.
const MIN_SIZE: u16 = 2;

/// A block of text nvmux has drawn over the session's screen — the attach
/// notice, or the hint bar ([`crate::hint`]) — and the rectangle of cells it
/// covers.
///
/// Held here rather than in [`crate::announce`], which works the geometry out,
/// because this is the module that composites it: only the shadow can reach
/// the cells underneath. The dependency then runs one way, from the thing
/// being drawn to the screen it is drawn on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Over {
    /// The top-left cell, 0-based, in the grid's own coordinates. The notice
    /// writes 1-based `CUP`s; the parser indexes from zero.
    pub top: u16,
    pub left: u16,
    /// The width in columns — measured, so a wide glyph in the label counts
    /// for the two cells it occupies — which every row is.
    pub width: u16,
    /// The rows of text. A space is a cell the overlay covers without drawing
    /// on, which is most of the notice and is why it hides what is under it.
    pub rows: Vec<String>,
}

/// A cell as a frame paints it: its interpolated colours and the two
/// attributes that survive a fade. What one frame compares against the last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Painted {
    fg: Rgb,
    /// `None` is the terminal's default background, left as `49`.
    bg: Option<Rgb>,
    bold: bool,
    dim: bool,
}

/// The session's screen, as far as its bytes have said.
pub struct Shadow {
    parser: vt100::Parser,
    /// What the last frame painted, one entry per cell in row-major order —
    /// or empty, when the screen has changed since and the next frame must
    /// paint every cell.
    painted: Vec<Painted>,
    /// The parser panicked on something the session wrote, so its grid can no
    /// longer be trusted: nothing more is fed to it, and a frame paints
    /// nothing. See [`Shadow::feed`].
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
            painted: Vec::new(),
            broken: false,
        }
    }

    /// Watch these bytes go by. Whatever they were — a frame, half a frame, a
    /// query the terminal will answer — the parser takes them as the terminal
    /// did.
    ///
    /// The one thing the shadow must never do is take the relay down. The
    /// parser is somebody else's code fed the whole of what an editor can
    /// write, so a panic in it is caught here and retires the shadow: the
    /// session then cuts out instead of dissolving, and nothing else changes.
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.broken {
            return;
        }
        self.guard(|parser| parser.process(bytes));
        self.painted.clear();
    }

    /// The terminal changed size, so the grid does too; the client is about
    /// to repaint it.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        if self.broken {
            return;
        }
        self.guard(|parser| {
            parser
                .screen_mut()
                .set_size(rows.max(MIN_SIZE), cols.max(MIN_SIZE));
        });
        self.painted.clear();
    }

    /// Run `f` on the parser, and retire the shadow if it panics.
    fn guard(&mut self, f: impl FnOnce(&mut vt100::Parser)) {
        let parser = &mut self.parser;
        if panic::catch_unwind(AssertUnwindSafe(|| f(parser))).is_err() {
            tracing::warn!("the shadow grid's parser panicked; the session will not dissolve");
            self.broken = true;
        }
    }

    /// `(rows, cols)`.
    pub fn size(&self) -> (u16, u16) {
        self.parser.screen().size()
    }

    /// Whether a frame from this shadow would paint anything: false once the
    /// parser has been retired, so a fade can be skipped rather than slept
    /// through.
    pub fn is_usable(&self) -> bool {
        !self.broken
    }

    /// Whether anything has been drawn on the screen: a glyph in any cell.
    /// What a hold on a session's first paint waits for before a lull counts
    /// as the paint being over — a client's startup queries come first, and a
    /// pause after them is not a finished screen.
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

    /// The terminal was written to behind the shadow's back — held bytes it
    /// had already seen, replayed, or a frame of nvmux's own painted over the
    /// screen — so the next frame must paint every cell rather than trust what
    /// the last one left.
    pub fn invalidate(&mut self) {
        self.painted.clear();
    }

    /// The bytes that paint the screen `t` of the way to `palette.bg`, as a
    /// diff against the frame before (or in full, after a [`feed`] or
    /// [`resize`]), leaving the cursor as `cursor` says. Always at least the
    /// sync span, the cursor hide and a trailing SGR reset.
    ///
    /// [`feed`]: Shadow::feed
    /// [`resize`]: Shadow::resize
    pub fn frame(&mut self, t: f32, palette: &Palette, cursor: Cursor) -> Vec<u8> {
        let screen = self.parser.screen();
        let (rows, cols) = if self.broken { (0, 0) } else { screen.size() };
        let total = usize::from(rows) * usize::from(cols);
        let diff = self.painted.len() == total;

        let mut next = Vec::with_capacity(total);
        let mut out = Vec::new();
        out.extend_from_slice(SYNC_BEGIN);
        out.extend_from_slice(HIDE_CURSOR);
        // What the stream's SGR currently says, so it is only written on a
        // change, and where the cursor is known to be after the last cell
        // written on the row — `None` until one is.
        let mut sgr: Option<Painted> = None;
        for row in 0..rows {
            let mut at: Option<u16> = None;
            for col in 0..cols {
                let cell = screen.cell(row, col);
                let index = next.len();
                let painted = resolve(cell, palette, t);
                next.push(painted);
                // The second half of the wide character before it, which the
                // first half already covered.
                if cell.is_some_and(vt100::Cell::is_wide_continuation) {
                    continue;
                }
                if diff && self.painted[index] == painted {
                    continue;
                }
                if at != Some(col) {
                    let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);
                }
                if sgr != Some(painted) {
                    write_sgr(&mut out, painted);
                    sgr = Some(painted);
                }
                let (text, width) = match cell {
                    Some(c) if c.has_contents() => (c.contents(), if c.is_wide() { 2 } else { 1 }),
                    _ => (" ", 1),
                };
                out.extend_from_slice(text.as_bytes());
                at = Some(col + width);
            }
        }
        out.extend_from_slice(RESET_SGR);
        // Inside the same synchronized update as the cells, so the screen and
        // its cursor appear together. A cursor the session itself hid stays
        // hidden: the hide at the top of the frame is then also the session's.
        if cursor == Cursor::Restored && !self.broken && !screen.hide_cursor() {
            let (row, col) = screen.cursor_position();
            let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);
            out.extend_from_slice(SHOW_CURSOR);
        }
        out.extend_from_slice(SYNC_END);
        self.painted = next;
        out
    }

    /// The bytes that paint the whole screen back as the session drew it: see
    /// the module docs. Every cell, with no diff and no palette. A retired
    /// shadow paints no cells.
    ///
    /// It ends where the session's own last write did, which is where the
    /// client believes the terminal to be: the cursor on the session's cell —
    /// shown if it was, and placed there even if not — and the session's own
    /// pen as the current one, rather than a reset. Neovim writes its next
    /// change on that belief, with no placement and no colour of its own when
    /// it thinks neither has moved, so a paint that left either anywhere else
    /// would have that change land in the wrong place or the wrong colour.
    ///
    /// Short of the client's own rendering in whatever the parser does not
    /// hold: of the attributes, it keeps bold or dim (one or the other),
    /// italic, underline, inverse and the colours, and loses undercurl and the
    /// other underline styles, underline colour, strikethrough, blink,
    /// conceal and overline — which the server's repaint that follows puts
    /// right.
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
                    // The second half of a wide character is written by its
                    // first; one on its own has nothing to write.
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
                        // A wide character the grid's last column cuts — one
                        // a narrowing left behind — is blanked: written, it
                        // would wrap, and on the last row scroll the screen.
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

    /// The bytes that leave the terminal where the session's own last write
    /// left it, whatever nvmux has written since: the session's pen as the
    /// current one, and the cursor on the session's cell — placed there even
    /// if hidden, and neither shown nor hidden here. The tail of
    /// [`Shadow::paint`], and what follows a fade in for a client that will
    /// write again before its server repaints it (see `pty::relay`): Neovim
    /// writes a change with no placement and no colour of its own when it
    /// believes neither has moved. A retired shadow hands back a reset.
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

    /// The bytes that cross-fade `over` with the session's screen under it:
    /// the overlay's own glyphs `t` of the way dissolved, and this shadow's
    /// cells — what the session drew there, and what the overlay is covering —
    /// `1 - t` of the way.
    ///
    /// This is what lets the attach notice dissolve into the editor rather
    /// than into a hole. The notice's own renderer fills its box with spaces,
    /// which erase; at `t = 0` this paints the same thing, because a cell
    /// fully dissolved is the background. What it can do that spaces cannot is
    /// come back: at `t = 1` the rectangle is the session's own cells at full
    /// colour, so the box's last frame leaves the screen as it found it.
    ///
    /// Three things set it apart from [`Shadow::frame`], all of them because
    /// this paints *over* a session that still owns the screen rather than
    /// taking the screen for a fade:
    ///
    /// * it leaves the cursor and the editor's own attributes exactly as it
    ///   found them — `DECSC`/`DECRC` around everything, and no cursor hide,
    ///   which would be seen as a blink for as long as the notice is up. The
    ///   shared save slot is safe here for the reason it is safe in
    ///   [`crate::announce`]: nothing is written except between the session's
    ///   own sequences, and never between a save of its own and the restore
    ///   that takes it back (see [`crate::boundary`]);
    /// * it keeps no diff. `frame`'s is whole-screen and keyed by length, and
    ///   a rectangle cannot share it; every cell is painted every time, which
    ///   costs a kilobyte or so a frame and makes this a pure function of the
    ///   grid;
    /// * it never writes outside the rectangle, whatever is in the way. A cell
    ///   the notice does not cover is the session's, and a fade frame has no
    ///   business touching it.
    ///
    /// Cells past the end of the grid — a terminal that grew before the shadow
    /// was told — resolve to blanks, so a rectangle that hangs off the edge
    /// paints spaces rather than nothing.
    pub fn under(&self, over: &Over, palette: &Palette, t: f32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(SYNC_BEGIN);
        out.extend_from_slice(SAVE_CURSOR);
        if !self.broken {
            self.paint_under(&mut out, over, palette, t);
        }
        out.extend_from_slice(RESET_SGR);
        out.extend_from_slice(RESTORE_CURSOR);
        out.extend_from_slice(SYNC_END);
        out
    }

    /// The cells themselves, inside the envelope [`Shadow::under`] wrote.
    fn paint_under(&self, out: &mut Vec<u8>, over: &Over, palette: &Palette, t: f32) {
        let screen = self.parser.screen();
        let beneath = 1.0 - t;
        // The overlay's glyphs while they are the more visible of the two; the
        // session's cells once they are not.
        let overlaid = t < CROSSOVER;
        let ink = Painted {
            fg: palette.fg.lerp(palette.bg, t),
            bg: None,
            bold: false,
            dim: false,
        };
        // What the stream's SGR says, carried across rows as `frame` carries
        // it, and where the cursor is known to be after the last cell written.
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
                    // The overlay's own, in the one colour the whole of it is
                    // drawn in — the notice sets no background, so the box's
                    // interior stays whatever the reset put back.
                    Some(g) => (g, ink, glyph_width(g)),
                    // The session's cell, as far as it fits. A wide character
                    // the rectangle cuts is blanked rather than written: half
                    // of one is a broken glyph, and the other half is outside
                    // the rectangle and not ours to repair.
                    None => {
                        let cell = screen.cell(line, col);
                        let painted = resolve(cell, palette, beneath);
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

/// A cell's colours and attributes as the session gave them: what
/// [`Shadow::paint`] writes, and compares to know when to write it again.
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

    /// One SGR that says all of it, from a reset — so a default colour is
    /// whatever is not mentioned, and an indexed one is written as the index
    /// it was, for the terminal's own palette to resolve.
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

/// What a cell is, `t` of the way to the background.
///
/// A blank cell has no foreground to speak of, so it is given the background
/// as one: then its `Painted` does not change from frame to frame and the
/// diff leaves it alone. Inverse video is applied here — the cell's colours
/// swapped, a default background standing in as the terminal's background
/// colour — so the terminal is never asked to invert an interpolated pair.
fn resolve(cell: Option<&vt100::Cell>, palette: &Palette, t: f32) -> Painted {
    let Some(cell) = cell else {
        return Painted {
            fg: palette.bg,
            bg: None,
            bold: false,
            dim: false,
        };
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
    let blank = !cell.has_contents();
    Painted {
        fg: if blank {
            palette.bg
        } else {
            fg.lerp(palette.bg, t)
        },
        bg: bg.map(|b| b.lerp(palette.bg, t)),
        bold: cell.bold() && !blank,
        dim: cell.dim() && !blank,
    }
}

/// Where each column of an overlay row starts: the text drawn there, or `None`
/// for the second half of a wide character and for columns the row does not
/// reach.
///
/// A row is `width` *columns*, not `width` characters, so it cannot be indexed
/// by either the grid's coordinates or the string's. Combining marks carry no
/// width of their own and ride along with the glyph they belong to, which is
/// how a decomposed character in a session name survives the composite.
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

/// What an overlay's glyph occupies, never zero — it was laid out by width, so
/// the only way here is a character that has one.
fn glyph_width(glyph: &str) -> u16 {
    glyph
        .chars()
        .filter_map(UnicodeWidthChar::width)
        .find(|w| *w > 0)
        .and_then(|w| u16::try_from(w).ok())
        .unwrap_or(1)
}

/// Whether an overlay draws nothing here: a cell it covers without marking.
/// The notice's padding and its box's interior, which hide the session's cells
/// by leaving them to the dissolve rather than by drawing over them.
fn is_blank(glyph: &str) -> bool {
    glyph.chars().all(char::is_whitespace)
}

/// One SGR that says everything about a cell, from a reset: the reset puts
/// the background back to the default, so a default background is whatever
/// is not mentioned.
fn write_sgr(out: &mut Vec<u8>, p: Painted) {
    out.extend_from_slice(b"\x1b[0");
    if p.bold {
        out.extend_from_slice(b";1");
    }
    if p.dim {
        out.extend_from_slice(b";2");
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

    /// Every CSI in a frame as `(parameters, final byte)`, in order. A piece
    /// between two escapes is the sequence and then whatever text followed it,
    /// so the sequence ends at its final byte rather than at the piece's end.
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

    /// What a composite frame puts on the screen: one entry per glyph written,
    /// as `((row, col), sgr, text)` in the terminal's own 1-based coordinates.
    /// Runs share a placement, so the column is tracked as the terminal would.
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

    /// A box eight columns across at the grid's row 2, column 3 — 0-based, as
    /// `Over` counts.
    fn over() -> Over {
        Over {
            top: 2,
            left: 3,
            width: 8,
            rows: vec!["╭──────╮".into(), "│ hi   │".into(), "╰──────╯".into()],
        }
    }

    /// A screen with a letter in every cell the box will cover, so anything
    /// showing through can be told from anything that is not.
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

    /// The box at rest covers what it is over. The editor's glyphs are still
    /// written — they have to be, or they could not come back a frame at a
    /// time — but fully dissolved, which is the background colour on the
    /// background: nothing of the text can be read through the box. This is
    /// the frame the dissolve starts from, and it looks exactly like the
    /// spaces the notice's own renderer fills its interior with.
    #[test]
    fn a_box_at_rest_hides_the_screen_it_is_over() {
        let p = palette();
        let box_glyphs = "╭─╮│╰╯hi";
        let drawn = format!("0;38;2;{};{};{}", p.fg.0, p.fg.1, p.fg.2);
        let gone = format!("0;38;2;{};{};{}", p.bg.0, p.bg.1, p.bg.2);
        for (at, sgr, text) in painted_cells(&under().under(&over(), &p, 0.0)) {
            if box_glyphs.contains(&text) {
                assert_eq!(sgr, drawn, "the box was not whole at {at:?}");
            } else {
                assert_eq!(sgr, gone, "{text:?} showed through the box at {at:?}");
            }
        }
    }

    /// Fully dissolved, the rectangle is the session's own cells at full
    /// colour: the box has not been erased, it has been replaced by what it
    /// was covering. This is what lets the notice leave without a repaint
    /// having to put the screen back.
    #[test]
    fn a_fully_dissolved_box_is_the_screen_underneath() {
        let frame = under().under(&over(), &palette(), 1.0);
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

    /// The crossover: a cell holding both shows the box while the box is the
    /// more visible of the two, and the screen once it is not. Either side of
    /// halfway, so the swap lands where the two are equally dissolved.
    #[test]
    fn a_cell_holding_both_swaps_at_the_crossover() {
        let shadow = under();
        assert!(glyphs(&shadow.under(&over(), &palette(), 0.49)).starts_with('╭'));
        assert_eq!(
            glyphs(&shadow.under(&over(), &palette(), 0.51)),
            "DEFGHIJKDEFGHIJKDEFGHIJK"
        );
    }

    /// Both layers dissolve together, so what the eye sees hand over rather
    /// than cut: the box's glyphs move toward the background as the screen's
    /// move away from it.
    #[test]
    fn the_two_layers_dissolve_in_opposite_directions() {
        let shadow = under();
        let ink = |t: f32| {
            painted_cells(&shadow.under(&over(), &palette(), t))
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

    /// Drawn over a session that still owns the screen, so unlike a frame it
    /// must give the cursor and the editor's own attributes back exactly as it
    /// found them — and never hide the cursor, which for the second the box is
    /// up would be seen as the editor's cursor going out.
    #[test]
    fn the_composite_leaves_the_cursor_and_the_editors_attributes_alone() {
        let frame = under().under(&over(), &palette(), 0.5);
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

    /// A cell the notice does not cover is the session's, and a frame of the
    /// notice's dissolve has no business writing on it — at any `t`, and
    /// whatever is in the way at the edges.
    #[test]
    fn the_composite_never_writes_outside_its_rectangle() {
        let over = over();
        let shadow = under();
        for t in [0.0, 0.25, 0.5, 0.75, 1.0] {
            for (at, _, text) in painted_cells(&shadow.under(&over, &palette(), t)) {
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

    /// A wide character the rectangle cuts in half is blanked rather than
    /// written: half a glyph is a broken glyph, and the other half is outside
    /// the rectangle and not the notice's to repair. The repaint that follows
    /// the notice is what puts it back whole.
    #[test]
    fn a_wide_character_cut_by_an_edge_is_blanked_rather_than_broken() {
        let mut shadow = Shadow::new(8, 20);
        // 日 straddles the left edge (0-based columns 2 and 3) and 本 the
        // right (columns 10 and 11); the box covers 3..11.
        shadow.feed("\x1b[3;3H日......本".as_bytes());
        let frame = shadow.under(&over(), &palette(), 1.0);
        let row: String = painted_cells(&frame)
            .into_iter()
            .filter(|((r, _), _, _)| *r == 3)
            .map(|(_, _, text)| text)
            .collect();
        assert_eq!(row, " ...... ", "{row:?}");
        assert!(!text(&frame).contains('日') && !text(&frame).contains('本'));
    }

    /// A terminal that grew before the shadow was told has cells the grid does
    /// not reach. They are blanks, not a gap: the notice covered them, and
    /// something has to be put back.
    #[test]
    fn a_rectangle_off_the_grid_paints_blanks() {
        let over = Over {
            top: 6,
            left: 16,
            width: 8,
            rows: vec!["        ".into(), "        ".into(), "        ".into()],
        };
        let frame = under().under(&over, &palette(), 1.0);
        assert_eq!(glyphs(&frame), " ".repeat(24));
    }

    /// A retired parser has no grid worth reading, so its composite is the
    /// envelope and nothing else — and the caller, which checks `is_usable`
    /// first, never asks for one.
    #[test]
    fn a_retired_shadow_composites_nothing() {
        let mut shadow = under();
        shadow.broken = true;
        assert!(painted_cells(&shadow.under(&over(), &palette(), 1.0)).is_empty());
    }

    /// A frame that paints nothing at all: the wrapper, and only the wrapper.
    fn wrapper_only(bytes: &[u8]) -> bool {
        let mut want = Vec::new();
        want.extend_from_slice(SYNC_BEGIN);
        want.extend_from_slice(HIDE_CURSOR);
        want.extend_from_slice(RESET_SGR);
        want.extend_from_slice(SYNC_END);
        bytes == want
    }

    /// The list every raw overlay has to get right (see `announce`): one
    /// synchronized frame, the cursor hidden, no newline and no carriage
    /// return, a reset at the end.
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

    /// The first frame paints every row, each from an absolute position.
    #[test]
    fn the_first_frame_places_every_row() {
        let mut s = Shadow::new(4, 5);
        s.feed(b"x");
        let f = s.frame(0.3, &palette(), Cursor::Hidden);
        let rows: Vec<usize> = placements(&f).iter().map(|(r, _)| *r).collect();
        assert_eq!(rows, vec![1, 2, 3, 4], "{:?}", text(&f));
        assert!(placements(&f).iter().all(|(_, c)| *c == 1));
    }

    /// A foreground is moved toward the background by `t`: an ANSI red at
    /// halfway is half red.
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

    /// A default background is never painted: it stays `49`, and a transparent
    /// terminal stays transparent. An explicit one is interpolated like a
    /// foreground.
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

    /// Between two frames only `t` changes, so a second frame at the same `t`
    /// has nothing to paint, and the blanks on a default background are never
    /// painted twice at any `t`.
    #[test]
    fn a_repeated_frame_paints_nothing_and_blanks_are_painted_once() {
        let mut s = Shadow::new(2, 10);
        s.feed(b"hi");
        let first = s.frame(0.25, &palette(), Cursor::Hidden);
        assert!(!wrapper_only(&first));
        let again = s.frame(0.25, &palette(), Cursor::Hidden);
        assert!(wrapper_only(&again), "{:?}", text(&again));
        // A later frame repaints the two glyphs and nothing else: one
        // placement, on the first row.
        let later = s.frame(0.5, &palette(), Cursor::Hidden);
        assert_eq!(placements(&later), vec![(1, 1)], "{:?}", text(&later));
    }

    /// Output from the session or a resize invalidates the diff: the next
    /// frame paints everything again.
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

    /// A wide character is written once, and the cell after it is placed two
    /// columns on rather than one — the continuation cell is not a cell.
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
        // The run reads 日, then x, then the blanks — in that order, with no
        // filler between the wide glyph and what follows it.
        let after = &t[t.find('日').expect("the glyph") + '日'.len_utf8()..];
        assert!(
            after.starts_with('x') || after.starts_with('\x1b'),
            "{after:?}"
        );
        assert!(after.contains('x'));
    }

    /// Combining marks travel with their base character.
    #[test]
    fn combining_marks_stay_with_their_base() {
        let mut s = Shadow::new(1, 4);
        s.feed("e\u{301}!".as_bytes());
        let t = text(&s.frame(0.5, &palette(), Cursor::Hidden));
        assert!(t.contains("e\u{301}!"), "{t:?}");
    }

    /// Inverse video is resolved here, not left to the terminal: the
    /// interpolated foreground becomes the background and the (default)
    /// background becomes the foreground.
    #[test]
    fn inverse_video_swaps_the_resolved_colours() {
        let mut s = Shadow::new(1, 4);
        s.feed(b"\x1b[7mx");
        let sgrs = sgrs(&s.frame(0.5, &palette(), Cursor::Hidden));
        // fg 200 -> 100 halfway becomes the bg; the default bg (0,0,0) is the
        // fg, and is at the background already.
        assert!(
            sgrs.iter()
                .any(|sgr| sgr == "0;38;2;0;0;0;48;2;100;100;100"),
            "{sgrs:?}"
        );
        assert!(!text(&s.frame(0.5, &palette(), Cursor::Hidden)).contains(";7"));
    }

    /// Bold and dim survive; italic and underline do not, since a fade frame
    /// carries colours, not decoration.
    #[test]
    fn bold_and_dim_survive_and_underline_does_not() {
        let mut s = Shadow::new(1, 8);
        s.feed(b"\x1b[1mb\x1b[0;2md\x1b[0;4mu");
        let sgrs = sgrs(&s.frame(0.5, &palette(), Cursor::Hidden));
        assert!(sgrs.iter().any(|sgr| sgr.starts_with("0;1;38")), "{sgrs:?}");
        assert!(sgrs.iter().any(|sgr| sgr.starts_with("0;2;38")), "{sgrs:?}");
        assert!(!sgrs.iter().any(|sgr| sgr.contains(";4;")), "{sgrs:?}");
    }

    /// What Neovim does on the way in — the alternate screen, a clear, a
    /// scroll region — is tracked, so the frame is of the screen the user sees.
    #[test]
    fn the_alternate_screen_is_what_gets_painted() {
        let mut s = Shadow::new(2, 6);
        s.feed(b"shell");
        s.feed(b"\x1b[?1049h\x1b[2J\x1b[Hnvim");
        let t = text(&s.frame(0.5, &palette(), Cursor::Hidden));
        assert!(t.contains("nvim"), "{t:?}");
        assert!(!t.contains("shell"), "{t:?}");
    }

    /// The grid is never smaller than the parser can cope with: text that
    /// wraps on a one-row terminal would otherwise underflow inside it.
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

    /// Nothing drawn is nothing drawn, however the cells got their colours;
    /// one glyph anywhere is enough.
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

    /// A replay the terminal saw but the diff did not: the next frame paints
    /// everything again.
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

    /// The bytes after the cells, up to the end of the sync span: the cursor
    /// placement and show, or nothing.
    fn cursor_tail(frame: &[u8]) -> &[u8] {
        let body = &frame[..frame.len() - SYNC_END.len()];
        let at = body
            .windows(RESET_SGR.len())
            .rposition(|w| w == RESET_SGR)
            .expect("a trailing SGR reset");
        &body[at + RESET_SGR.len()..]
    }

    /// The last frame of a fade in hands the screen back with the cursor on
    /// the cell the session left it on and shown — inside the sync span, so
    /// the screen and its cursor appear together. Every other frame leaves it
    /// hidden, with no show anywhere in it.
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

    /// A cursor the session hid is the session's to show: a busy editor hides
    /// it on purpose, and putting it back would second-guess that. The
    /// restore is then nothing at all, and the frame is a hidden one.
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

    /// `DECSC`/`DECRC` around an overlay leave the cursor where they found it,
    /// and the parser follows both.
    ///
    /// The attach notice is no longer fed to the shadow — the grid has to hold
    /// what is *under* the box, not the box (see `pty`'s
    /// `write_over_session`) — so this is no longer about the notice. It is
    /// about the envelope: [`Shadow::under`] writes the same pair, and
    /// anything else drawn over a session has to, or a resume would put the
    /// cursor back in an overlay's corner rather than on the editor's cell.
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

    /// A retired shadow has no cursor to speak of either.
    #[test]
    fn a_retired_shadow_restores_no_cursor() {
        let mut s = Shadow::new(4, 4);
        s.feed(b"ok");
        let hook = panic::take_hook();
        panic::set_hook(Box::new(|_| {}));
        s.guard(|_| panic!("a parser bug"));
        panic::set_hook(hook);
        assert!(wrapper_only(&s.frame(0.0, &palette(), Cursor::Restored)));
    }

    /// A parser that panics retires the shadow rather than the relay: nothing
    /// more is parsed, and a frame paints nothing.
    #[test]
    fn a_panicking_parser_retires_the_shadow() {
        let mut s = Shadow::new(4, 4);
        s.feed(b"ok");
        // Quiet the panic message this test provokes on purpose.
        let hook = panic::take_hook();
        panic::set_hook(Box::new(|_| {}));
        s.guard(|_| panic!("a parser bug"));
        panic::set_hook(hook);
        assert!(s.broken);
        assert!(!s.is_usable());
        s.feed(b"more");
        assert!(wrapper_only(&s.frame(0.5, &palette(), Cursor::Hidden)));
    }

    /// Nothing about a frame depends on the size being sane.
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

    /// A paint is the screen back as it was drawn: fed to a terminal of the
    /// same size, it leaves every cell as the session left it — glyph,
    /// colours as given, and every attribute the parser holds — and the cursor
    /// on the session's cell.
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
        // A cell never written and one written with a space look the same,
        // and a paint writes every cell.
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

    /// And it leaves the terminal where the session's last write left it: the
    /// session's pen current, and the cursor on its cell even when hidden —
    /// the two things the client's next write takes for granted.
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

    /// Colours go back as the session wrote them — an index as an index — so
    /// the terminal's own palette resolves them, as it did the first time.
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

    /// The same rules a frame keeps: one synchronized update, a placement for
    /// every row and never a newline or a carriage return (`OPOST` is off,
    /// and a line feed on the last row would scroll the screen).
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

    /// A wide character a narrowing left in the last column is blanked, not
    /// written: it would wrap, and on the last row scroll the whole screen.
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

    /// A cursor the session hid stays hidden, and a retired shadow paints
    /// nothing but the envelope.
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
}
