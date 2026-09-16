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
//! parser, fed a copy of every byte the real terminal is sent, keeping a grid
//! of cells with their colours and attributes. **It only watches.** Nothing
//! reaching the terminal is changed, delayed or dropped on its account; the
//! parser sees the same bytes a moment later, and if it misreads one the only
//! consequence is a fade frame that is slightly wrong. It costs a parse of the
//! session's output while a session is attached, which is why `[fade]
//! session = false` turns it off entirely rather than merely not using it.
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
//! carriage return (`OPOST` is off), and the cursor hidden — the next owner of
//! the screen shows it again.

use std::io::Write;
use std::panic::{self, AssertUnwindSafe};

use crate::fade::{SYNC_BEGIN, SYNC_END};
use crate::palette::{Palette, Rgb};

const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const RESET_SGR: &[u8] = b"\x1b[0m";

/// The smallest grid the parser is given, in either dimension.
///
/// `vt100` 0.16 underflows on a one-row grid (and on a one-column one with a
/// wide character) when text wraps: `col_wrap` scrolls, then subtracts the
/// scroll from a row that is already zero. No real terminal is one row, and
/// Neovim cannot run in one, so the shadow is simply never that small; the
/// frame it paints then has a row the terminal clamps, which at that size is
/// nothing anyone can see.
const MIN_SIZE: u16 = 2;

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
    /// had already seen, replayed — so the next frame must paint every cell
    /// rather than trust what the last one left.
    pub fn invalidate(&mut self) {
        self.painted.clear();
    }

    /// The bytes that paint the screen `t` of the way to `palette.bg`, as a
    /// diff against the frame before (or in full, after a [`feed`] or
    /// [`resize`]). Always at least the sync span, the cursor hide and a
    /// trailing SGR reset.
    ///
    /// [`feed`]: Shadow::feed
    /// [`resize`]: Shadow::resize
    pub fn frame(&mut self, t: f32, palette: &Palette) -> Vec<u8> {
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
        out.extend_from_slice(SYNC_END);
        self.painted = next;
        out
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
        let f = s.frame(0.5, &palette());
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
        let f = s.frame(0.3, &palette());
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
        let f = s.frame(0.5, &palette());
        assert!(
            sgrs(&f).iter().any(|sgr| sgr == "0;38;2;100;0;0"),
            "{:?}",
            sgrs(&f)
        );
        let f = s.frame(1.0, &palette());
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
        let f = s.frame(0.5, &palette());
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
        let first = s.frame(0.25, &palette());
        assert!(!wrapper_only(&first));
        let again = s.frame(0.25, &palette());
        assert!(wrapper_only(&again), "{:?}", text(&again));
        // A later frame repaints the two glyphs and nothing else: one
        // placement, on the first row.
        let later = s.frame(0.5, &palette());
        assert_eq!(placements(&later), vec![(1, 1)], "{:?}", text(&later));
    }

    /// Output from the session or a resize invalidates the diff: the next
    /// frame paints everything again.
    #[test]
    fn output_and_resizes_make_the_next_frame_a_full_one() {
        let mut s = Shadow::new(2, 4);
        s.feed(b"a");
        let _ = s.frame(0.5, &palette());
        s.feed(b"b");
        assert_eq!(placements(&s.frame(0.5, &palette())).len(), 2);
        let _ = s.frame(0.5, &palette());
        s.resize(3, 4);
        assert_eq!(s.size(), (3, 4));
        assert_eq!(placements(&s.frame(0.5, &palette())).len(), 3);
    }

    /// A wide character is written once, and the cell after it is placed two
    /// columns on rather than one — the continuation cell is not a cell.
    #[test]
    fn a_wide_character_is_written_once_and_advances_two_columns() {
        let mut s = Shadow::new(1, 6);
        s.feed("日x".as_bytes());
        let f = s.frame(0.5, &palette());
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
        let t = text(&s.frame(0.5, &palette()));
        assert!(t.contains("e\u{301}!"), "{t:?}");
    }

    /// Inverse video is resolved here, not left to the terminal: the
    /// interpolated foreground becomes the background and the (default)
    /// background becomes the foreground.
    #[test]
    fn inverse_video_swaps_the_resolved_colours() {
        let mut s = Shadow::new(1, 4);
        s.feed(b"\x1b[7mx");
        let sgrs = sgrs(&s.frame(0.5, &palette()));
        // fg 200 -> 100 halfway becomes the bg; the default bg (0,0,0) is the
        // fg, and is at the background already.
        assert!(
            sgrs.iter()
                .any(|sgr| sgr == "0;38;2;0;0;0;48;2;100;100;100"),
            "{sgrs:?}"
        );
        assert!(!text(&s.frame(0.5, &palette())).contains(";7"));
    }

    /// Bold and dim survive; italic and underline do not, since a fade frame
    /// carries colours, not decoration.
    #[test]
    fn bold_and_dim_survive_and_underline_does_not() {
        let mut s = Shadow::new(1, 8);
        s.feed(b"\x1b[1mb\x1b[0;2md\x1b[0;4mu");
        let sgrs = sgrs(&s.frame(0.5, &palette()));
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
        let t = text(&s.frame(0.5, &palette()));
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
        let _ = s.frame(0.5, &palette());
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
        let _ = s.frame(0.5, &palette());
        assert!(wrapper_only(&s.frame(0.5, &palette())));
        s.invalidate();
        assert_eq!(placements(&s.frame(0.5, &palette())).len(), 2);
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
        assert!(wrapper_only(&s.frame(0.5, &palette())));
    }

    /// Nothing about a frame depends on the size being sane.
    #[test]
    fn tiny_screens_do_not_panic() {
        for &(cols, rows) in crate::ui::test_support::TINY_SIZES {
            let mut s = Shadow::new(rows, cols);
            s.feed(b"\x1b[31mhello\r\nworld\x1b[0m");
            let _ = s.frame(0.5, &palette());
            let _ = s.frame(1.0, &palette());
            s.resize(rows, cols);
            let _ = s.frame(0.5, &palette());
        }
    }
}
