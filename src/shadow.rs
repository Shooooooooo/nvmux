//! A shadow of an attached session's screen, kept so the session can be
//! dissolved on its way out.
//!
//! The relay never renders Neovim. The client on the pty draws, and
//! [`crate::pty`] copies its bytes to the terminal without looking at them —
//! which is why every terminal feature the editor negotiates keeps working, and
//! why nvmux has no idea what is on the screen. A fade needs to know: to move a
//! cell toward the background it has to know what colour the cell is.
//!
//! So a [`Shadow`] is a terminal emulator with no terminal: the `vt100` crate's
//! parser, fed a copy of every byte the *session* sends the real terminal,
//! keeping a grid of cells with their colours and attributes. **It only
//! watches.** Nothing reaching the terminal is changed, delayed or dropped on
//! its account; the parser sees the same bytes a moment later, and if it
//! misreads one the only consequence is a frame that is slightly wrong: a
//! fade's, or, for a kept client, the screen put back on its return, until the
//! server's own repaint lands. It costs a parse of the session's output while
//! a session is attached, which is why `[effects.fade] session = false` turns it off
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
//! And a cell that shows nothing — blank, or a space with no background and
//! no underline — is not written a space at a time where there are many of
//! them together: a stretch of them is erased, to the row's end with `EL` and
//! short of it with `ECH`, neither of which moves the cursor, and under a pen
//! with no background, so that the cells come out the terminal's own
//! background whether the terminal erases in the pen's or in its own. A few
//! are written as spaces, and a few the cursor has to cross to reach the next
//! cell are written over rather than jumped. A row of 200 blank columns was
//! 200 spaces; it is an `EL`.
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
//! # What a frame under the settings paints
//!
//! [`Shadow::frame_under`] is a frame of the session with the settings
//! floating over it (see `crate::pty::panel`): the panel's own cells where it
//! stands, as the settings screen drew them, and every other cell as the
//! caller says — taken toward the background through the palette, each cell
//! as far as the falloff round the panel takes it; drawn faint where there is
//! no palette; or emptied. A diff, as a fade's frames are, and the same rules
//! as theirs.
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

use crate::fade::{SYNC_BEGIN, SYNC_END};
use crate::falloff;
use crate::palette::{Palette, Rgb};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

/// Cells of the session's screen a dissolve leaves standing: what is written
/// on them stays at its full colour on every frame, while all else — their
/// backgrounds among it — dissolves. For a card nvmux's own client drew on the
/// session's screen (see [`crate::client::cards::arrival_footprint`]), which then
/// stays as it is while the screen under it comes and goes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Kept {
    /// On a row, the first column and the last, 0-based.
    spans: Vec<(u16, u16, u16)>,
    /// What the client writes on them that stays where it is, each character
    /// with the cell it starts on; and what moves along its row as the card
    /// does, each character on the cell it starts from, and how many columns
    /// along it goes: how the screen shows the card is there to stand (see
    /// [`Shadow::shows`]).
    glyphs: Vec<(u16, u16, char)>,
    moving: Vec<(u16, u16, char)>,
    shift: i16,
}

impl Kept {
    /// The cells of `spans`, each a row, its first column and its last, with
    /// `glyphs` written on them.
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

    /// These cells, with `moving` written on them too, anywhere from where it
    /// starts to `shift` columns along: a row of names on the move.
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

    /// Where `card` stands on a screen `cols` wide and `rows` high, if it is
    /// an arrival (see [`crate::client::cards::arrival_footprint`]).
    pub fn arrival(card: &crate::card::Card, cols: u16, rows: u16) -> Self {
        let f = crate::client::cards::arrival_footprint(card, usize::from(cols), usize::from(rows));
        Self::new(f.spans, f.glyphs).moving(f.moving, f.shift)
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Whether what stands moves as it stands: a row of names a step moves
    /// along, which the client goes on drawing as it moves.
    pub fn moves(&self) -> bool {
        !self.moving.is_empty() && self.shift != 0
    }

    /// The spans, each a row, its first column and its last.
    pub fn spans(&self) -> &[(u16, u16, u16)] {
        &self.spans
    }

    fn holds(&self, row: u16, col: u16) -> bool {
        self.spans
            .iter()
            .any(|&(r, from, to)| r == row && (from..=to).contains(&col))
    }
}

/// What a frame with the settings over the session does with one of the
/// session's cells (see [`Shadow::frame_under`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shade {
    /// Its colours this far toward the terminal's background, as a fade takes
    /// them, through the terminal's palette; emptied from
    /// [`falloff::EMPTIED`] on.
    Toward(f32),
    /// Drawn faint, in its own colours: where nvmux knows no colours to take
    /// it toward. Faint dims a cell's text and not its background.
    Faint,
    /// Emptied: the terminal's own background.
    Blank,
}

/// A rectangle nvmux draws over the session in place of what is there: the
/// settings, floating over it (see `crate::pty::panel`), as their screen drew
/// them into a buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Laid {
    top: u16,
    left: u16,
    width: u16,
    height: u16,
    /// Every cell's text, one after another: one string for them all rather
    /// than one each, which a frame would make over again every time.
    text: String,
    /// Row by row, where in `text` each cell's is, and in what pen; `None`
    /// for the second half of a wide character, which its first half writes.
    cells: Vec<Option<(u32, u32, Pen)>>,
}

impl Laid {
    /// `buf`'s cells, where its area is on the screen. Its colours are the
    /// true ones a theme or a fade gave it, or the terminal's own.
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

    /// The cell at (`row`, `col`) of the screen, if the rectangle covers it:
    /// what it shows and in what, or `None` for the second half of a wide
    /// character.
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

/// A cell as a frame paints it: its interpolated colours and the attributes
/// that change how its glyph is drawn. What one frame compares against the
/// last.
///
/// The attributes are every one the parser holds, inverse aside, which is
/// resolved into the colours. A fade that dropped one would draw the screen
/// in the wrong shape for its whole length and snap back to the right one
/// when the client's own bytes landed — italic text dissolving in upright,
/// say, then leaning over as the fade ended.
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
    /// A cell nothing is written on: the background as its foreground, since
    /// it has none to speak of, and no attributes — the same at any `t`.
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

    /// Whether a space shows nothing at all in this pen: the terminal's own
    /// background, and no underline, are all there is of it to see.
    fn shows_nothing(&self) -> bool {
        self.bg.is_none() && !self.underline
    }
}

/// The session's screen, as far as its bytes have said.
pub struct Shadow {
    parser: vt100::Parser,
    /// What keeps the parser's OSC buffer small: the parser does nothing with
    /// an OSC, and an `OSC 52` copy would otherwise stay in it for as long
    /// as the client lives (see [`crate::ledger::OscCap`]).
    osc: crate::ledger::OscCap,
    /// The start of a character the bytes last fed ended part way through,
    /// held back from the parser till the rest of it comes: see [`whole`].
    cut: Vec<u8>,
    /// What the last frame painted, one entry per cell in row-major order —
    /// or empty, when the screen has changed since and the next frame must
    /// paint every cell.
    painted: Vec<Painted>,
    /// What the last frame with the settings over the session painted (see
    /// [`Shadow::frame_under`]), one entry per cell, as `painted` is for a
    /// fade's frames: to be trusted only while `under_valid` says so, and
    /// kept when it does not, for the next frame to write over in place. Let
    /// go of once the settings are down ([`Shadow::forget_under`]).
    under: Vec<Drawn>,
    under_valid: bool,
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
            osc: crate::ledger::OscCap::default(),
            cut: Vec::new(),
            painted: Vec::new(),
            under: Vec::new(),
            under_valid: false,
            broken: false,
        }
    }

    /// Watch these bytes go by. Whatever they were — a frame, half a frame, a
    /// query the terminal will answer — the parser takes them as the terminal
    /// did, but for the start of a character they end part way through,
    /// which waits for the rest of it (see [`whole`]), and the payload of an
    /// OSC past [`crate::ledger::OSC_SHOWN`], which it would only store.
    ///
    /// The one thing the shadow must never do is take the relay down. The
    /// parser is somebody else's code fed the whole of what an editor can
    /// write, so a panic in it is caught here and retires the shadow: the
    /// session then cuts out instead of dissolving, and nothing else changes.
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.broken {
            return;
        }
        let shown = self.osc.cap(bytes);
        // After the character the bytes before cut in two, which these finish.
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
        self.invalidate();
    }

    /// Run `f` on the parser, and retire the shadow if it panics.
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

    /// What the session has the terminal report of the mouse, as far as its
    /// bytes have said: what to put back after nvmux has had the mouse for
    /// something of its own over the session (see `crate::pty::panel`).
    /// Nothing, for a retired shadow, which can no longer say.
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

    /// Whether a frame from this shadow would paint anything: false once the
    /// parser has been retired, so a fade can be skipped rather than slept
    /// through.
    pub fn is_usable(&self) -> bool {
        !self.broken
    }

    /// Whether the session has the terminal on its alternate screen, as far
    /// as its bytes have said: where a client draws once it has entered it,
    /// which a fresh one does as it starts. False for a retired shadow, which
    /// can no longer say.
    pub fn on_alternate_screen(&self) -> bool {
        !self.broken && self.parser.screen().alternate_screen()
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
    ///
    /// A fade's diff is let go of rather than emptied: each of its frames
    /// builds its own, so what emptying would keep is never used again —
    /// eleven bytes a cell, for as long as a kept client stays parked. The
    /// settings' is kept for their next frame to write over in place, until
    /// they are down ([`Shadow::forget_under`]).
    pub fn invalidate(&mut self) {
        self.painted = Vec::new();
        self.under_valid = false;
    }

    /// The settings are down: what their frames were diffed against goes
    /// with them, rather than stay as long as the client does — a kept one
    /// for the rest of the run.
    pub fn forget_under(&mut self) {
        self.under = Vec::new();
        self.under_valid = false;
    }

    /// The bytes that paint the screen `t` of the way to `palette.bg`, as a
    /// diff against the frame before (or in full, after a [`feed`] or
    /// [`resize`]), leaving the cursor as `cursor` says. Always at least the
    /// sync span, the cursor hide and a trailing SGR reset.
    ///
    /// [`feed`]: Shadow::feed
    /// [`resize`]: Shadow::resize
    pub fn frame(&mut self, t: f32, palette: &Palette, cursor: Cursor) -> Vec<u8> {
        self.frame_with(|_| t, palette, cursor, None, &Kept::default())
    }

    /// [`Shadow::frame`], with each row `dissolve(row)` of the way to
    /// `palette.bg` rather than all of them the same — the shape an iris gives
    /// a fade (see [`crate::fade::Iris`]) — and with an overlay `over` painted
    /// over it, its glyphs in `ink` and `ot` of the way dissolved, inside the
    /// same synchronized update, so the terminal never presents the screen
    /// without it. The overlay's rectangle is painted as [`Shadow::under`]
    /// paints it: its glyphs while they are the more visible, the session's
    /// cells after.
    ///
    /// Those cells are painted again on every frame whatever the diff says,
    /// since the overlay is drawn over them on every frame; so the diff,
    /// which records the session's cells, stays true of everything the
    /// overlay does not cover, and of the rectangle too once the overlay has
    /// dissolved and paints the session's own cells there.
    ///
    /// What is written on the `kept` cells is painted at its full colour
    /// whatever `dissolve` says, on a background that dissolves with the rest.
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
        // What the stream's SGR currently says, so it is only written on a
        // change, and where the cursor is known to be after the last cell
        // written on the row — `None` until one is.
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
                // The second half of the wide character before it, which the
                // first half already covered.
                let wide_rest = cell.is_some_and(vt100::Cell::is_wide_continuation);
                let (text, width) = match cell {
                    Some(c) if c.has_contents() => (c.contents(), if c.is_wide() { 2 } else { 1 }),
                    _ => (" ", 1),
                };
                let same = diff && self.painted[index] == painted;
                bare[usize::from(col)] = !wide_rest && text == " " && painted.shows_nothing();
                // Once a run is owed it takes the unchanged blank cells after
                // it as well: one erase rather than several.
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
        self.under_valid = false;
        out
    }

    /// Whether the screen has on it what the `kept` cells are kept for: each of
    /// their glyphs that stays where it is on its cell, and those that move
    /// all the same way along, as far as they go or less. A dissolve leaves
    /// them standing only then — otherwise they would stand with whatever the
    /// session has there.
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

    /// The bytes that draw what the `kept` cells have written on them, as a
    /// dissolve leaves it standing — at its full colour, on the terminal's own
    /// background — and leave the cursor and the pen as they were: to draw it
    /// again on a screen the session has gone from (see [`crate::handoff`]).
    /// Nothing where nothing is written, or the grid cannot be trusted.
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
    /// the overlay's own glyphs `t` of the way dissolved from `ink` — what
    /// nvmux draws its own text in, the terminal's foreground unless a theme
    /// says otherwise (see [`crate::theme`]) — and this shadow's cells — what
    /// the session drew there, and what the overlay is covering — `1 - t` of
    /// the way.
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

    /// The cells themselves, inside the envelope [`Shadow::under`] wrote.
    ///
    /// `beneath` is how far the session's cells under it are dissolved, row by
    /// row: the overlay's opposite everywhere, for the notice; the frame's own
    /// shape, for a frame with an overlay over it.
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
        // The overlay's glyphs while they are the more visible of the two; the
        // session's cells once they are not.
        let overlaid = t < CROSSOVER;
        let ink = Painted {
            fg: ink.lerp(palette.bg, t),
            bg: None,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
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
    /// A frame of the session with `laid` over it — the settings floating
    /// over a session (see `crate::pty::panel`) — and every other cell as
    /// `shade` says: taken toward the background through `palette`, drawn
    /// faint, or emptied. With no `laid` the session's cells under where it
    /// goes are shaded too, which is how they empty for it to come up on.
    ///
    /// A diff against the frame before, as [`Shadow::frame`]'s are, and in
    /// full after a [`feed`], a [`resize`] or an [`invalidate`] — which is
    /// every frame after the session has drawn, since what it drew reached the
    /// terminal behind this one's back. Always at least the sync span, the
    /// cursor hide and a trailing SGR reset, and the cursor left as `cursor`
    /// says.
    ///
    /// A wide character either edge of the rectangle cuts is blanked rather
    /// than written in half. `Shade::Toward` with no palette is drawn faint:
    /// there is nothing to take it toward.
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
            // Whether the cell before was a wide character of the session's
            // written whole, whose second half is this one.
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
                // Once a run is owed it takes the unchanged blank cells after
                // it as well: one erase rather than several.
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

/// A session's cell as a frame with the settings over it draws it (see
/// [`Shadow::frame_under`]): its text, blanked where `cut` — a wide
/// character the rectangle's edge cuts — and its pen as `shade` says.
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
    // A space that would come out of `resolve` with no background and no
    // underline, whatever `t`: most of the cells of a screen, and the blank
    // pen below for every one of them, so there is no colour to work out.
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
            // A space shows no colour of its own, unless it is underlined or
            // has a background: so many cells of a screen are written spaces
            // that each in its own shade of the falloff would cost a colour
            // apiece for nothing.
            let bare = text == " " && !pen.underline && pen.bg == vt100::Color::Default;
            (text, if bare { Pen::BLANK } else { pen })
        }
        // Faint, or toward a background nvmux does not know: the cell's own
        // colours, its text faint.
        (Shade::Toward(_) | Shade::Faint, _) => {
            let mut pen = Pen::of(cell);
            pen.dim |= cell.is_some_and(vt100::Cell::has_contents);
            (text, pen)
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
    /// The terminal's own colours and no attributes: an emptied cell.
    const BLANK: Pen = Pen {
        fg: vt100::Color::Default,
        bg: vt100::Color::Default,
        bold: false,
        dim: false,
        italic: false,
        underline: false,
        inverse: false,
    };

    /// Whether a space shows nothing at all in this pen: the terminal's own
    /// background, not swapped with the foreground, and no underline.
    fn shows_nothing(&self) -> bool {
        self.bg == vt100::Color::Default && !self.underline && !self.inverse
    }

    /// A cell as a fade paints it, in its interpolated colours: the
    /// background left to the terminal where it is the terminal's.
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

    /// A cell of a screen of nvmux's own, as ratatui drew it: modifiers, and
    /// the true colours a theme or a fade gave it, if any.
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

/// What a frame with the settings over the session last painted in a cell
/// (see [`Shadow::frame_under`]): its text, kept in place rather than in a
/// string of its own, and its pen. Kept for every cell from one frame to the
/// next, so a frame allocates nothing per cell.
#[derive(Debug, Clone, Copy)]
struct Drawn {
    /// How many bytes of `text` are the cell's; [`Drawn::NONE`] for nothing
    /// written, [`Drawn::LONG`] for more than there is room for.
    len: u8,
    /// As much as a `vt100` cell holds, which is all the session's text.
    text: [u8; DRAWN_BYTES],
    pen: Pen,
}

/// The most text a `vt100` cell holds.
const DRAWN_BYTES: usize = 22;

impl Drawn {
    /// The second half of a wide character, which its first half wrote.
    const NONE: u8 = u8::MAX;
    /// Text too long to keep — a cluster of the settings' own, if any is —
    /// which no cell is taken to be the same as, so it is always written.
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

    /// Whether `cell` is what was painted here.
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

/// The fewest cells that show nothing erased with `ECH` rather than written
/// as spaces: an erase leaves the cursor where it began, and the `CUP` the
/// next cell written then needs costs about this many spaces with it.
const ECH_FROM: u16 = 12;

/// The most cells that show nothing written over as spaces to reach the next
/// cell on the row, rather than jumped with a `CUP`, which costs about as
/// much.
const GAP: u16 = 7;

/// Cells a frame owes the terminal that show nothing — a space with no
/// background and no underline, which is most of a screen — and has yet to
/// write: from a column, how many, one after another along the row. Written
/// together, once a cell that shows something comes or the row ends (see
/// [`Run::flush`]): where there are many, as one erase.
///
/// An erase fills the cells with the pen's background, so the pen is first
/// made one with none, whatever it was: the terminal's own background is
/// what goes in them either way, and it is the same on a terminal that
/// erases in the pen's background as on one that erases in its own.
#[derive(Debug, Default)]
struct Run(Option<(u16, u16)>);

impl Run {
    /// The column after the run owed too, or the first of a new one.
    fn push(&mut self, col: u16) {
        match &mut self.0 {
            Some((_, n)) => *n += 1,
            None => self.0 = Some((col, 1)),
        }
    }

    fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    /// Write what is owed on `row` of a screen `cols` wide, from `at`, where
    /// the cursor is, leaving `at` where it then is: as spaces where it is
    /// short, and otherwise erased — to the row's end with `EL`, short of it
    /// with `ECH` — neither of which moves the cursor. `blank` makes the pen
    /// one that shows nothing first.
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

/// Put the cursor, at `at` on `row`, on its column `col`: by writing spaces
/// over the cells between where they all show nothing, as the pen does
/// (`over`), and there are no more than [`GAP`] of them; otherwise with a
/// `CUP`. `bare` says which of the row's cells show nothing.
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

/// The stream's SGR, made one a fade's blank cells can be written or erased
/// in — as it is, if it shows nothing already.
fn blank_sgr(out: &mut Vec<u8>, sgr: &mut Option<Painted>, palette: &Palette) {
    if !sgr.is_some_and(|p| p.shows_nothing()) {
        let blank = Painted::blank(palette);
        write_sgr(out, blank);
        *sgr = Some(blank);
    }
}

/// The pen, made one blank cells under the settings can be written or erased
/// in — as it is, if it shows nothing already.
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

/// What a cell is, `t` of the way to the background.
///
/// A blank cell has no foreground to speak of, so it is given the background
/// as one, and no attributes: then its `Painted` does not change from frame to
/// frame and the diff leaves it alone. Blank is a cell nothing was written to
/// since it was erased, and a space the session wrote that shows nothing
/// either: no background, no underline. A space with one is a glyph like any
/// other, which is how an underline under one is kept — Neovim writes spaces
/// for an underlined stretch, since terminals do not underline an erase.
///
/// Inverse video is applied here — the cell's colours swapped, a default
/// background standing in as the terminal's background colour — so the
/// terminal is never asked to invert an interpolated pair.
/// A cell a dissolve leaves standing, `t` of the way: what is written on it
/// at its full colour, on its background `t` of the way to `palette.bg`.
fn standing(cell: Option<&vt100::Cell>, palette: &Palette, t: f32) -> Painted {
    Painted {
        bg: resolve(cell, palette, t).bg,
        ..resolve(cell, palette, 0.0)
    }
}

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

/// How much of `bytes` is whole characters: all of it but the start of a
/// character it ends part way through, which waits for the rest. `vte`, under
/// `vt100`, finishes such a character with up to three bytes of what comes
/// next, and should that run on into the start of another, drops whatever
/// came between the two: a read of the session's output that ends inside the
/// `é` of `é ─` loses the space, and the shadow has the rest of the row a cell
/// to the left of where the terminal has it.
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
    /// A screen with a card of nvmux's on it: a dim log on the editor's
    /// background, and on its second row the name a switch landed in, bold.
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

    /// What a card writes on the cells it stands on stays at its full colour
    /// as the screen dissolves, on a background that goes with the rest.
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

    /// A card stands only where the screen shows it: a screen without it, or
    /// a stand with nothing it would write, stands nothing.
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

    /// A card on the move stands wherever along its way the screen shows it,
    /// all of what moves the same way along — and nowhere past its end.
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

    /// What stands is drawn again as it stood: at its full colour, on the
    /// terminal's own background, between a cursor save and restore.
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
        for (at, sgr, text) in painted_cells(&under().under(&over(), &p, p.fg, 0.0)) {
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

    /// The crossover: a cell holding both shows the box while the box is the
    /// more visible of the two, and the screen once it is not. Either side of
    /// halfway, so the swap lands where the two are equally dissolved.
    #[test]
    fn a_cell_holding_both_swaps_at_the_crossover() {
        let shadow = under();
        assert!(glyphs(&shadow.under(&over(), &palette(), palette().fg, 0.49)).starts_with('╭'));
        assert_eq!(
            glyphs(&shadow.under(&over(), &palette(), palette().fg, 0.51)),
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

    /// The box is drawn in the ink it is handed — nvmux's own colour, a
    /// theme's when there is one — and dissolves from it; the screen under it
    /// comes back in the terminal's colours whatever the ink, since its cells
    /// are the editor's.
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

    /// The name the picker hands off rides the session's own fade-in frames:
    /// drawn whole over the still-dissolved screen on the first, inside the
    /// one synchronized update, so the terminal never shows the frame without
    /// it; and replaced by the session's own cells on the last.
    #[test]
    fn an_overlay_rides_inside_the_sessions_own_frames() {
        let p = palette();
        let name = Over {
            top: 3,
            left: 4,
            width: 4,
            rows: vec!["name".into()],
        };
        // What each cell was last painted with, 1-based as the wire has it.
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

    /// A frame can be shaped row by row: each row as far into the background
    /// as `dissolve` says for it, which is how an iris opens a session out of
    /// one line.
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

    /// Drawn over a session that still owns the screen, so unlike a frame it
    /// must give the cursor and the editor's own attributes back exactly as it
    /// found them — and never hide the cursor, which for the second the box is
    /// up would be seen as the editor's cursor going out.
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

    /// A cell the notice does not cover is the session's, and a frame of the
    /// notice's dissolve has no business writing on it — at any `t`, and
    /// whatever is in the way at the edges.
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
        let frame = shadow.under(&over(), &palette(), palette().fg, 1.0);
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
        let frame = under().under(&over, &palette(), palette().fg, 1.0);
        assert_eq!(glyphs(&frame), " ".repeat(24));
    }

    /// A retired parser has no grid worth reading, so its composite is the
    /// envelope and nothing else — and the caller, which checks `is_usable`
    /// first, never asks for one.
    #[test]
    fn a_retired_shadow_composites_nothing() {
        let mut shadow = under();
        shadow.broken = true;
        assert!(painted_cells(&shadow.under(&over(), &palette(), palette().fg, 1.0)).is_empty());
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

    /// Bold, dim, italic and underline survive, on every frame of a fade: one
    /// dropped would have the text dissolve in the wrong shape and snap to the
    /// right one as the client's own bytes landed — italic text coming in
    /// upright, say, and leaning over as the fade ended.
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

    /// An underline is painted under the spaces the session wrote with it —
    /// how Neovim underlines a stretch of blanks — and not under an erase,
    /// which a terminal never underlines whatever the pen says. The grid
    /// keeps the pen on an erased cell all the same, so it is the frame that
    /// has to tell the two apart.
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

    /// A clipboard write far longer than the parser is shown leaves the grid
    /// as it would have been, the text after it included.
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

    /// Whatever a client writes and however it is read, the shadow's screen
    /// is the one a parser shown all of it at once has: what the cap keeps
    /// from the parser is only ever what it would have stored, and what is
    /// held back of a cut character only waits.
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

    /// Letting go of a fade's diff is all an invalidation does to it: the
    /// next frame paints every cell, as it would after any other.
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

    /// A retired shadow has no cursor to speak of either.
    #[test]
    fn a_retired_shadow_restores_no_cursor() {
        let mut s = Shadow::new(4, 4);
        s.feed(b"ok");
        crate::test_support::quietly(|| s.guard(|_| panic!("a parser bug")));
        assert!(wrapper_only(&s.frame(0.0, &palette(), Cursor::Restored)));
    }

    /// A parser that panics retires the shadow rather than the relay: nothing
    /// more is parsed, and a frame paints nothing.
    #[test]
    fn a_panicking_parser_retires_the_shadow() {
        let mut s = Shadow::new(4, 4);
        s.feed(b"ok");
        // Quiet the panic message this test provokes on purpose.
        crate::test_support::quietly(|| s.guard(|_| panic!("a parser bug")));
        assert!(s.broken);
        assert!(!s.is_usable());
        s.feed(b"more");
        assert!(wrapper_only(&s.frame(0.5, &palette(), Cursor::Hidden)));
    }

    /// A character the session's output is read in two inside goes to the
    /// parser whole, so the shadow is the screen the terminal drew however
    /// the reads fell: `vte` on its own loses the space of `é ─` read in two
    /// inside the `é`, and has the rest of the row a cell to the left.
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

    /// The settings as their screen drew them: `text` on the first row of a
    /// rectangle `width` by `height` at 0-based (`left`, `top`), the rest of
    /// it spaces.
    fn laid(left: u16, top: u16, width: u16, height: u16, text: &str) -> Laid {
        let area = ratatui::layout::Rect::new(left, top, width, height);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        buf.set_string(left, top, text, ratatui::style::Style::default());
        Laid::from_buffer(&buf)
    }

    /// Every cell of a frame by where it is, 1-based: what it was last
    /// painted with, and with what.
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

    /// The settings stand where they are, in what their screen drew them in,
    /// and the session's cells everywhere else as far toward the background
    /// as each is told; nothing of the session shows through them.
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

    /// Emptied cells are the terminal's own background; faint ones keep the
    /// session's own colours, as indices where it gave indices, and dim only
    /// their text; and taken toward a background nvmux does not know, a cell
    /// is drawn faint.
    #[test]
    fn a_cell_is_emptied_or_drawn_faint() {
        let mut shadow = Shadow::new(2, 6);
        // An erase in a background leaves cells with nothing written on them.
        let session = b"\x1b[31;44mab\x1b[0m\x1b[2;1H\x1b[44m\x1b[K\x1b[0m";
        shadow.feed(session);
        // Every cell of a terminal showing the session, after `frame`: its
        // text gone, and its background the terminal's own.
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

    /// A terminal `rows` by `cols` showing `frame`, written over a screen of
    /// `#` on a background: a cell the frame did not see to shows as one.
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

    /// The cells of a terminal's row that show something, as `(column,
    /// text)`; and whether every other cell of the row shows nothing — no
    /// text, the terminal's own background, nothing under or inverted.
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

    /// A stretch of cells that show nothing is erased rather than written a
    /// space at a time — to the row's end with `EL`, short of it with `ECH` —
    /// and a short one written as spaces. A frame from the settings writes
    /// them as a fade's does; either way the terminal shows them blank.
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

    /// An erase fills with the pen's background, so a pen with one is put
    /// aside first, and the cells come out the terminal's own background —
    /// as they do on a terminal that erases in its own and not the pen's.
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

    /// What a frame under the settings keeps of a cell, to know it next time:
    /// the text and pen it drew, and text too long to keep never taken to be
    /// the same.
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

    /// The settings down, what their frames were diffed against goes, and
    /// the next frame under them paints every cell again.
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

    /// The frames are diffs: the same again paints nothing, the settings
    /// changing paints what changed, and what the session draws repaints it
    /// all — it reached the terminal behind the frame's back.
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

    /// A wide character the rectangle's edge cuts is blanked, and one it does
    /// not is written once.
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

    /// A frame under the settings can put the cursor back where the session
    /// had it, as the last of a fade in does.
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

    /// What the settings' screen drew is laid as it drew it: its modifiers,
    /// and a theme's colour where it gave one.
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

    /// The mouse as the session last had the terminal report it: off, its
    /// buttons, or every motion too.
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
