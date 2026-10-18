//! What the terminal shows, and the bytes that change it into a new frame.
//!
//! The client keeps the last frame it drew and writes only the cells that
//! differ from it — every one of them, when it has reason to think the
//! terminal no longer shows that frame: a resize, and Neovim clearing its
//! screen, which is how nvmux asks for a repaint after drawing over a session
//! (`:mode`, see `pty::repaint_through_server`).
//!
//! # Every frame stands alone
//!
//! A frame assumes nothing about where the terminal's cursor is or what its
//! pen is set to: the first cell it writes is moved to, and its colours set,
//! from nothing. Nor, where it moves down a row by LF, what scroll region is
//! set: it sets the whole screen first (see [`move_to`]). nvmux writes over the session between frames — the hint row,
//! the attach notice, the fade's frames — and puts the pen back as it was
//! where it can, but a frame that never relies on it cannot be caught out
//! where it cannot.
//!
//! # What the cells carry
//!
//! Every attribute Neovim's own TUI writes — bold, dim, italic, the
//! underlines and their colour, blink, reverse, conceal, strikethrough,
//! altfont, overline — and a highlight's hyperlink, as an OSC 8 around its
//! cells. A link is closed before every jump of the cursor and at the end of
//! the frame, as that TUI closes one, so it never runs on over cells that are
//! not its own.
//!
//! # In as few bytes as it takes
//!
//! With `[client] lean_output` on (see [`Screen::set_lean`]), a frame goes on
//! from where its last cell left the cursor and the pen: every move, and
//! every change of colour, made by the shortest of the ways a terminal
//! understands (see [`move_to`] and [`recolour`]), as Neovim's own TUI makes
//! them. Each of those is the same screen as the absolute move and the reset
//! would have been. A move is made from where the cursor is only where the
//! terminal cannot have put it anywhere else: after a character a terminal
//! may take for wider or narrower than Neovim does, or one written to the
//! last column, where terminals part ways on what comes next, the next move
//! is absolute again. Off, the default, every jump of the cursor is absolute
//! and every change of the pen a reset and the whole pen.
//!
//! # Synchronized
//!
//! A frame goes out inside a synchronized update (`?2026`), so the terminal
//! shows the whole of it or none of it — which is what lets an animation
//! repaint the same cells sixty times a second without a frame ever being
//! seen half drawn. `'termsync'` off leaves that out, as it does for Neovim's
//! own TUI; and the cursor is hidden while the frame is drawn and shown where
//! it belongs at the end, so a terminal that ignores `?2026` does not see it
//! run across the screen either. That is the same reset nvmux ends a span with
//! (see [`crate::fade::SYNC_END`]), and a client that brackets its own frames is
//! left to do it (see `boundary::Boundary::session_brackets_its_own_frames`).

use std::io::Write as _;

use super::compose::{Frame, Out};
use super::grid::Text;
use super::style::{Color, Style, Underline};
use crate::fade::{SYNC_BEGIN, SYNC_END};
use crate::palette::Rgb;

/// The terminal's own cursor, as a frame leaves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// Shown at all.
    pub visible: bool,
    pub row: usize,
    pub col: usize,
    /// `DECSCUSR`'s number: 1 to 6, a blinking or steady block, underline or
    /// bar.
    pub shape: u8,
    /// Its colour, or `None` for the terminal's own.
    pub color: Option<Rgb>,
}

/// The end of a hyperlink: OSC 8 with no target.
const LINK_END: &[u8] = b"\x1b]8;;\x1b\\";

/// `DECSTBM` with no margins: the scroll region the whole screen again, so
/// that an LF scrolls only at the screen's foot, and the cursor sent home.
/// What a frame that moves down a row by LF starts with (see [`move_to`]).
const WHOLE_SCROLL_REGION: &[u8] = b"\x1b[r";

/// What the terminal shows.
#[derive(Debug)]
pub struct Screen {
    shown: Option<Frame>,
    /// A frame done with — the one shown before `shown`, or one forgotten —
    /// kept for the buffer the next is put together in (see
    /// [`Screen::take_spare`]).
    spare: Option<Frame>,
    /// The cursor's shape and colour as last set, so they are only set again
    /// when they change — setting the shape restarts its blink.
    shape: Option<u8>,
    color: Option<Option<Rgb>>,
    /// Each frame in as few bytes as it takes: see [`Screen::set_lean`].
    lean: bool,
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen {
    pub fn new() -> Self {
        Self {
            shown: None,
            spare: None,
            shape: None,
            color: None,
            lean: false,
        }
    }

    /// Write each frame from here on in as few bytes as it takes, `lean`, or
    /// with every jump of the cursor absolute and every change of the pen a
    /// reset (see the module docs). The screen is the same either way; each
    /// frame stands alone, so the next is the first written the new way.
    pub fn set_lean(&mut self, lean: bool) {
        self.lean = lean;
    }

    /// A frame done with, to put the next together in its buffer (see
    /// [`super::compose::compose_into`]); an empty one at first.
    pub fn take_spare(&mut self) -> Frame {
        self.spare
            .take()
            .unwrap_or_else(|| Frame::new(0, 0, Style::default()))
    }

    /// Forget what the terminal shows: the next frame paints every cell, and
    /// sets the cursor's shape and colour afresh.
    pub fn invalidate(&mut self) {
        if let Some(shown) = self.shown.take() {
            self.spare = Some(shown);
        }
        self.shape = None;
        self.color = None;
    }

    /// The bytes that turn what the terminal shows into `frame`, with the
    /// cursor left as `cursor` says, and the cells of a hyperlink made one
    /// with an OSC 8 to the target `links` gives it (see [`Style::link`]).
    pub fn draw(
        &mut self,
        frame: Frame,
        cursor: &Cursor,
        sync: bool,
        links: &[Box<str>],
    ) -> Vec<u8> {
        let mut out = Vec::with_capacity(4096);
        self.draw_into(&mut out, frame, cursor, sync, links);
        out
    }

    /// [`Screen::draw`], onto the end of `out`.
    pub fn draw_into(
        &mut self,
        out: &mut Vec<u8>,
        frame: Frame,
        cursor: &Cursor,
        sync: bool,
        links: &[Box<str>],
    ) {
        if sync {
            out.extend_from_slice(SYNC_BEGIN);
        }
        out.extend_from_slice(b"\x1b[?25l");
        // Where a frame that moves down by LF resets the scroll region: ahead
        // of its first move, which is a CUP.
        let head = out.len();
        let shown = self.shown.take();
        let old = shown
            .as_ref()
            .filter(|old| (old.width, old.height) == (frame.width, frame.height));
        let mut pen: Option<Style> = None;
        let mut at: Option<(usize, usize)> = None;
        // Whether the terminal's cursor is surely where `at` says, so that a
        // move may be made from there: lean, and every cell written since the
        // last CUP a plain ASCII character, one column wide on every terminal
        // whatever it makes of anything else, and none of them in the last
        // column. Neovim's TUI places the cursor afresh after a character of
        // ambiguous width for the same reason.
        let mut sure = false;
        // Whether the frame has moved down a row by LF.
        let mut fed = false;
        // The hyperlink open, if one is. Closed before every jump, as
        // Neovim's TUI does, so that a link never runs on over cells between
        // where it was left and where the next is drawn.
        let mut link = 0;
        for r in 0..frame.height {
            // A row as it was is nothing to write, as none of its cells is:
            // told apart at once, as most rows of most frames are.
            let row = r * frame.width..(r + 1) * frame.width;
            if old.is_some_and(|old| old.cells[row.clone()] == frame.cells[row]) {
                continue;
            }
            let mut c = 0;
            while c < frame.width {
                let cell = &frame.cells[r * frame.width + c];
                let span = if cell.wide { 2 } else { 1 };
                let same = old.is_some_and(|old| {
                    (c..(c + span).min(frame.width))
                        .all(|k| old.cells[r * frame.width + k] == frame.cells[r * frame.width + k])
                });
                // A right half is drawn with its left; one on its own is
                // nothing to draw.
                if same || cell.text == Text::Half {
                    c += 1;
                    continue;
                }
                if at != Some((r, c)) {
                    if link != 0 {
                        out.extend_from_slice(LINK_END);
                        link = 0;
                    }
                    let from = at.filter(|_| sure);
                    fed |= move_to(out, &frame, from, (r, c), pen.as_ref(), fed);
                    sure = self.lean;
                }
                // The link is no part of the pen: it is the OSC 8's below.
                let style = Style {
                    link: 0,
                    ..cell.style
                };
                if pen != Some(style) {
                    match pen {
                        Some(was) if self.lean && only_colours_differ(&was, &style) => {
                            recolour(out, &was, &style);
                        }
                        _ => sgr(out, &style),
                    }
                    pen = Some(style);
                }
                if cell.style.link != link {
                    let target = (cell.style.link as usize)
                        .checked_sub(1)
                        .and_then(|i| links.get(i));
                    match target {
                        // Its own id, so that a terminal hovering over one
                        // cut by a float underlines all of it.
                        Some(url) => {
                            let _ = write!(out, "\x1b]8;id=nvmux-{};{url}\x1b\\", cell.style.link);
                        }
                        None => out.extend_from_slice(LINK_END),
                    }
                    link = cell.style.link;
                }
                cell.text.push_to(out);
                c += span;
                at = Some((r, c));
                sure &= c < frame.width && plain(cell);
            }
        }
        if fed {
            out.splice(head..head, WHOLE_SCROLL_REGION.iter().copied());
        }
        // Whatever the frame left the pen as is not what a cell of nvmux's own
        // — the hint row, the notice — should inherit, and nor is a link.
        if link != 0 {
            out.extend_from_slice(LINK_END);
        }
        if pen.is_some() {
            out.extend_from_slice(b"\x1b[0m");
        }
        if cursor.visible {
            if self.shape != Some(cursor.shape) {
                let _ = write!(out, "\x1b[{} q", cursor.shape);
                self.shape = Some(cursor.shape);
            }
            if self.color != Some(cursor.color) {
                match cursor.color {
                    Some(Rgb(r, g, b)) => {
                        let _ = write!(out, "\x1b]12;#{r:02x}{g:02x}{b:02x}\x07");
                    }
                    None => out.extend_from_slice(b"\x1b]112\x07"),
                }
                self.color = Some(cursor.color);
            }
            let _ = write!(out, "\x1b[{};{}H\x1b[?25h", cursor.row + 1, cursor.col + 1);
        }
        if sync {
            out.extend_from_slice(SYNC_END);
        }
        self.shown = Some(frame);
        self.spare = shown;
    }
}

/// The most cells of a gap written again, rather than moved over: more than
/// three cost more than the CUF that crosses them.
const REWRITE_AT_MOST: usize = 3;

/// Move the cursor from `at` — where it surely is, or `None` where that is not
/// known: first in a frame, and after a cell that leaves it unsure (see
/// `sure` in [`Screen::draw_into`]) — to `to`, by the fewest bytes:
///
/// - forward along a row: the cells in between written again, where they are
///   a few plain ASCII characters drawn in the pen already, or else a CUF;
/// - to the next row: CR LF, and on from its first column as above;
/// - otherwise, and from nowhere known, a CUP.
///
/// Says whether it moved down by LF. The LF is never asked of the last row,
/// and the frame it is in resets the scroll region ([`WHOLE_SCROLL_REGION`])
/// before anything else, so that it moves down within the screen and never
/// scrolls it: nothing nvmux writes sets a region, but a session drawn by
/// Neovim's own TUI sets one for every scroll it makes, and a switch can
/// come between that and its reset. That reset comes with the first LF of a
/// frame, `fed` says whether it has, and that LF has to save more than it
/// costs.
fn move_to(
    out: &mut Vec<u8>,
    frame: &Frame,
    at: Option<(usize, usize)>,
    to: (usize, usize),
    pen: Option<&Style>,
    fed: bool,
) -> bool {
    let (r, c) = to;
    // At least 6: ESC, `[`, a digit, `;`, a digit, `H`.
    let absolute = 4 + digits(r + 1) + digits(c + 1);
    match at {
        Some((was_r, was_c)) if was_r == r && was_c < c => {
            if cross(out, frame, r, was_c, c, pen, absolute) {
                return false;
            }
        }
        Some((was_r, _)) if was_r + 1 == r => {
            let toll = if fed { 0 } else { WHOLE_SCROLL_REGION.len() };
            let start = out.len();
            out.extend_from_slice(b"\r\n");
            if c == 0 || cross(out, frame, r, 0, c, pen, absolute - 2 - toll) {
                return true;
            }
            out.truncate(start);
        }
        _ => {}
    }
    out.extend_from_slice(b"\x1b[");
    push_dec(out, r + 1);
    out.push(b';');
    push_dec(out, c + 1);
    out.push(b'H');
    false
}

/// Move along row `r` from column `from` to `to`, if it can be done in fewer
/// than `limit` bytes, and say whether it was: by writing the cells between
/// again, where there are few enough of them and each is a plain ASCII
/// character in `pen`, and otherwise by CUF.
fn cross(
    out: &mut Vec<u8>,
    frame: &Frame,
    r: usize,
    from: usize,
    to: usize,
    pen: Option<&Style>,
    limit: usize,
) -> bool {
    let gap = to - from;
    let cells = &frame.cells[r * frame.width + from..r * frame.width + to];
    let written_again = gap <= REWRITE_AT_MOST
        && pen.is_some_and(|pen| cells.iter().all(|cell| plain(cell) && cell.style == *pen));
    if written_again {
        if gap >= limit {
            return false;
        }
        for cell in cells {
            cell.text.push_to(out);
        }
        return true;
    }
    let forward = if gap == 1 { 3 } else { 3 + digits(gap) };
    if forward >= limit {
        return false;
    }
    out.extend_from_slice(b"\x1b[");
    if gap > 1 {
        push_dec(out, gap);
    }
    out.push(b'C');
    true
}

/// Whether `cell` is a plain ASCII character: one column wide on every
/// terminal there is, whatever it makes of the width of anything else.
fn plain(cell: &Out) -> bool {
    !cell.wide && matches!(cell.text, Text::Char(' '..='~'))
}

/// Whether the pen can go from `was` to `now` by its colours alone: every
/// other attribute is as it was, the underline's style and colour among them.
fn only_colours_differ(was: &Style, now: &Style) -> bool {
    Style {
        fg: now.fg,
        bg: now.bg,
        ..*was
    } == *now
}

/// Change the pen from `was` to `now`, which differ in their colours and in
/// nothing else (see [`only_colours_differ`]): one SGR of the colours that
/// changed, with no reset — the default spelt as `39` or `49`.
fn recolour(out: &mut Vec<u8>, was: &Style, now: &Style) {
    out.push(0x1b);
    let open = out.len();
    if was.fg != now.fg {
        match now.fg {
            Color::Default => out.extend_from_slice(b";39"),
            fg => colour(out, fg, 30, 90, 38),
        }
    }
    if was.bg != now.bg {
        match now.bg {
            Color::Default => out.extend_from_slice(b";49"),
            bg => colour(out, bg, 40, 100, 48),
        }
    }
    // The first colour's `;` is where the sequence opens.
    out[open] = b'[';
    out.push(b'm');
}

/// How many digits `n` takes.
fn digits(n: usize) -> usize {
    n.checked_ilog10().map_or(1, |d| d as usize + 1)
}

/// `n` in decimal.
fn push_dec(out: &mut Vec<u8>, mut n: usize) {
    let mut buf = [0u8; 20];
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[at..]);
}

/// Set the pen to `s`, from nothing: a reset and every attribute after it, in
/// one sequence the oldest terminal reads — and, after it, the two things only
/// newer ones do, each in a sequence of its own so that a terminal that cannot
/// read one loses that and nothing else: an underline's style (`4:3` and
/// family, over the plain `4` already set) and its colour (`58`).
pub fn sgr(out: &mut Vec<u8>, s: &Style) {
    out.extend_from_slice(b"\x1b[0");
    for (on, n) in [
        (s.bold, &b";1"[..]),
        (s.dim, b";2"),
        (s.italic, b";3"),
        (s.underline != Underline::None, b";4"),
        (s.blink, b";5"),
        (s.reverse, b";7"),
        (s.conceal, b";8"),
        (s.strikethrough, b";9"),
        (s.altfont, b";11"),
        (s.overline, b";53"),
    ] {
        if on {
            out.extend_from_slice(n);
        }
    }
    colour(out, s.fg, 30, 90, 38);
    colour(out, s.bg, 40, 100, 48);
    out.push(b'm');
    let style = match s.underline {
        Underline::Curl => Some(3),
        Underline::Double => Some(2),
        Underline::Dotted => Some(4),
        Underline::Dashed => Some(5),
        Underline::Line | Underline::None => None,
    };
    if let Some(n) = style {
        let _ = write!(out, "\x1b[4:{n}m");
    }
    if s.underline != Underline::None {
        match s.sp {
            Color::Rgb(Rgb(r, g, b)) => {
                let _ = write!(out, "\x1b[58:2::{r}:{g}:{b}m");
            }
            Color::Index(n) => {
                let _ = write!(out, "\x1b[58:5:{n}m");
            }
            Color::Default => {}
        }
    }
}

/// One colour of an SGR, as Neovim's TUI spells it on a 256-colour terminal:
/// the eight and their bright eight by their own numbers, the rest as
/// `38;5`/`48;5`, and RGB as `38;2`/`48;2`.
fn colour(out: &mut Vec<u8>, c: Color, base: u8, bright: u8, ext: u8) {
    let mut code = |n: u8| {
        out.push(b';');
        push_dec(out, usize::from(n));
    };
    match c {
        Color::Default => {}
        Color::Index(n) if n < 8 => code(base + n),
        Color::Index(n) if n < 16 => code(bright + n - 8),
        Color::Index(n) => {
            code(ext);
            code(5);
            code(n);
        }
        Color::Rgb(Rgb(r, g, b)) => {
            code(ext);
            code(2);
            code(r);
            code(g);
            code(b);
        }
    }
}

/// Every cell of `frame` that differs from `blank`, for the tests: a quick
/// way to say what a frame drew without spelling out every blank.
#[cfg(test)]
pub fn marks(frame: &Frame, blank: &super::compose::Out) -> Vec<(usize, usize)> {
    let mut v = Vec::new();
    for r in 0..frame.height {
        for c in 0..frame.width {
            if frame.get(r, c) != Some(blank) {
                v.push((r, c));
            }
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::compose::Out;

    fn frame(rows: &[&str]) -> Frame {
        let mut f = Frame::new(rows[0].chars().count(), rows.len(), Style::default());
        for (r, row) in rows.iter().enumerate() {
            for (c, ch) in row.chars().enumerate() {
                f.cells[r * f.width + c].text = Text::Char(ch);
            }
        }
        f
    }

    fn hidden() -> Cursor {
        Cursor {
            visible: false,
            row: 0,
            col: 0,
            shape: 2,
            color: None,
        }
    }

    /// What a terminal ends up showing after the bytes, read back by the same
    /// parser the relay tests trust.
    fn shown(bytes: &[u8], w: u16, h: u16) -> String {
        let mut p = vt100::Parser::new(h, w, 0);
        p.process(bytes);
        p.screen().contents()
    }

    #[test]
    fn the_first_frame_draws_everything_and_the_next_only_what_changed() {
        let mut s = Screen::new();
        let first = s.draw(frame(&["ab", "cd"]), &hidden(), true, &[]);
        assert_eq!(shown(&first, 2, 2), "ab\ncd");
        let second = s.draw(frame(&["ab", "cX"]), &hidden(), true, &[]);
        let text = String::from_utf8_lossy(&second);
        assert!(text.contains("\x1b[2;2H"), "{text:?}");
        assert!(
            !text.contains('a'),
            "an unchanged cell is not written: {text:?}"
        );
        let mut both = first.clone();
        both.extend_from_slice(&second);
        assert_eq!(shown(&both, 2, 2), "ab\ncX");
    }

    #[test]
    fn invalidating_paints_every_cell_again() {
        let mut s = Screen::new();
        s.draw(frame(&["ab"]), &hidden(), false, &[]);
        s.invalidate();
        let again = s.draw(frame(&["ab"]), &hidden(), false, &[]);
        assert_eq!(shown(&again, 2, 1), "ab");
    }

    /// A frame the same as the one shown writes nothing but the cursor's
    /// hiding: no cell, no move, no colour.
    #[test]
    fn a_frame_as_it_was_writes_no_cell() {
        let mut s = Screen::new();
        s.draw(frame(&["ab", "cd"]), &hidden(), false, &[]);
        let again = s.draw(frame(&["ab", "cd"]), &hidden(), false, &[]);
        assert_eq!(again, b"\x1b[?25l");
    }

    /// The frame shown before the last is kept for its buffer, and so is one
    /// forgotten; drawn onto the end of what is already to be written.
    #[test]
    fn a_frame_done_with_is_kept_for_the_next() {
        let mut s = Screen::new();
        assert_eq!(s.take_spare().cells.len(), 0, "none at first");
        let mut out = b"said".to_vec();
        s.draw_into(&mut out, frame(&["ab"]), &hidden(), false, &[]);
        assert!(out.starts_with(b"said"), "{out:?}");
        assert_eq!(shown(&out[4..], 2, 1), "ab");
        s.draw(frame(&["abc", "def"]), &hidden(), false, &[]);
        let spare = s.take_spare();
        assert_eq!((spare.width, spare.height), (2, 1), "the one before");
        s.invalidate();
        let spare = s.take_spare();
        assert_eq!((spare.width, spare.height), (3, 2), "the one forgotten");
    }

    /// The frame is bracketed when asked, and the cursor is hidden while it
    /// is drawn and shown where it belongs after.
    #[test]
    fn a_frame_is_synchronized_and_ends_with_the_cursor_in_place() {
        let mut s = Screen::new();
        let cursor = Cursor {
            visible: true,
            row: 1,
            col: 0,
            shape: 6,
            color: Some(Rgb(1, 2, 3)),
        };
        let bytes = s.draw(frame(&["ab", "cd"]), &cursor, true, &[]);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("\x1b[?2026h\x1b[?25l"), "{text:?}");
        assert!(
            text.ends_with("\x1b[6 q\x1b]12;#010203\x07\x1b[2;1H\x1b[?25h\x1b[?2026l"),
            "{text:?}"
        );
        // The same shape and colour again are not set again.
        let bytes = s.draw(frame(&["ab", "cd"]), &cursor, false, &[]);
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(" q") && !text.contains("]12"), "{text:?}");
        assert!(!text.contains("2026"), "{text:?}");
    }

    /// A wide character is written once, from its left cell.
    #[test]
    fn a_wide_character_is_written_from_its_left_half() {
        let mut f = frame(&["xxx"]);
        f.cells[0] = Out {
            text: Text::Char('中'),
            style: Style::default(),
            wide: true,
        };
        f.cells[1].text = Text::Half;
        let mut s = Screen::new();
        let bytes = s.draw(f, &hidden(), false, &[]);
        assert_eq!(shown(&bytes, 3, 1), "中x");
    }

    #[test]
    fn colours_are_spelt_as_neovims_tui_spells_them() {
        let mut out = Vec::new();
        sgr(
            &mut out,
            &Style {
                fg: Color::Index(3),
                bg: Color::Index(12),
                bold: true,
                ..Style::default()
            },
        );
        assert_eq!(out, b"\x1b[0;1;33;104m");
        out.clear();
        sgr(
            &mut out,
            &Style {
                fg: Color::Index(200),
                bg: Color::Rgb(Rgb(1, 2, 3)),
                underline: Underline::Curl,
                sp: Color::Rgb(Rgb(255, 0, 0)),
                ..Style::default()
            },
        );
        assert_eq!(
            out,
            b"\x1b[0;4;38;5;200;48;2;1;2;3m\x1b[4:3m\x1b[58:2::255:0:0m"
        );
        out.clear();
        sgr(
            &mut out,
            &Style {
                dim: true,
                blink: true,
                conceal: true,
                overline: true,
                ..Style::default()
            },
        );
        assert_eq!(out, b"\x1b[0;2;5;8;53m");
    }

    /// A hyperlink's cells are written inside an OSC 8 to its target, which
    /// is closed before the cursor jumps and at the end of the frame — never
    /// left open over cells that are not its own.
    #[test]
    fn a_hyperlink_is_opened_over_its_cells_and_closed_after() {
        let mut f = frame(&["see here", "  and x "]);
        for c in 4..8 {
            f.cells[c].style.link = 1;
        }
        f.cells[f.width + 6].style.link = 1;
        f.cells[f.width + 7].style.link = 1;
        let links: Vec<Box<str>> = vec!["https://neovim.io".into()];
        let mut s = Screen::new();
        let bytes = s.draw(f, &hidden(), false, &links);
        let text = String::from_utf8_lossy(&bytes);
        let open = "\x1b]8;id=nvmux-1;https://neovim.io\x1b\\";
        let close = "\x1b]8;;\x1b\\";
        assert!(
            text.contains(&format!("see {open}here{close}\x1b[2;1H")),
            "closed before the jump to the next row: {text:?}"
        );
        assert!(
            text.contains(&format!("{open}x {close}\x1b[0m")),
            "and at the end of the frame: {text:?}"
        );
        assert_eq!(text.matches(open).count(), 2, "{text:?}");
        assert_eq!(text.matches(close).count(), 2, "{text:?}");
        assert_eq!(shown(&bytes, 8, 2), "see here\n  and x ");
    }

    /// What the second of two frames writes over the first, as text, with
    /// `[client] lean_output` on.
    fn over(first: Frame, second: Frame) -> String {
        let mut s = Screen::new();
        s.set_lean(true);
        s.draw(first, &hidden(), false, &[]);
        String::from_utf8_lossy(&s.draw(second, &hidden(), false, &[])).into_owned()
    }

    /// Along a row, the cursor crosses an unchanged gap by writing its cells
    /// again where they are a few ASCII characters in the pen, and by CUF
    /// where they are many or anything else; the first move of a frame is a
    /// CUP, wherever the last left the cursor.
    #[test]
    fn a_move_along_a_row_is_made_in_the_fewest_bytes() {
        let text = over(frame(&["abcdefghij"]), frame(&["XbcYefghiZ"]));
        assert_eq!(
            text, "\x1b[?25l\x1b[1;1H\x1b[0mXbcY\x1b[5CZ\x1b[0m",
            "two cells written again, five crossed"
        );

        let text = over(frame(&["abc"]), frame(&["aXc"]));
        assert_eq!(text, "\x1b[?25l\x1b[1;2H\x1b[0mX\x1b[0m");

        let mut before = frame(&["aéc"]);
        let mut after = frame(&["XéZ"]);
        // The same cell either way, and not ASCII: crossed, not written again.
        before.cells[1].text = Text::Char('é');
        after.cells[1].text = Text::Char('é');
        assert_eq!(
            over(before, after),
            "\x1b[?25l\x1b[1;1H\x1b[0mX\x1b[CZ\x1b[0m"
        );

        let before = frame(&["abc"]);
        let mut after = frame(&["XbZ"]);
        let mut red = after.cells[1].style;
        red.fg = Color::Index(1);
        // In a pen of its own, unchanged: crossed, not written in the other.
        let mut was = before;
        was.cells[1].style = red;
        after.cells[1].style = red;
        assert_eq!(over(was, after), "\x1b[?25l\x1b[1;1H\x1b[0mX\x1b[CZ\x1b[0m");
    }

    /// The next row is reached by CR LF, and from its first column on as
    /// along a row; a row further on is a CUP. A frame that has an LF resets
    /// the scroll region first, which its first LF has to be worth.
    #[test]
    fn the_next_row_is_reached_by_cr_lf() {
        let rows = || frame(&["abc", "def", "ghi"]);
        let text = over(rows(), frame(&["aXc", "Yef", "ghi"]));
        assert_eq!(text, "\x1b[?25l\x1b[r\x1b[1;2H\x1b[0mX\r\nY\x1b[0m");
        let text = over(rows(), frame(&["aXc", "def", "gYi"]));
        assert_eq!(text, "\x1b[?25l\x1b[1;2H\x1b[0mX\x1b[3;2HY\x1b[0m");
        let text = over(rows(), frame(&["aXc", "Yef", "gZi"]));
        assert_eq!(
            text, "\x1b[?25l\x1b[r\x1b[1;2H\x1b[0mX\r\nY\r\ngZ\x1b[0m",
            "and across, the reset paid for"
        );
        let text = over(rows(), frame(&["aXc", "dYf", "ghi"]));
        assert_eq!(
            text, "\x1b[?25l\x1b[1;2H\x1b[0mX\x1b[2;2HY\x1b[0m",
            "but not where an LF and the reset cost as much as the CUP"
        );
    }

    /// The cursor is moved from where it is only where the terminal cannot
    /// have it anywhere else: after a character that is not ASCII, which a
    /// terminal may draw wider or narrower than Neovim says, or one in the
    /// last column, after which terminals differ on where it is, the next
    /// move is a CUP.
    #[test]
    fn a_move_is_absolute_where_the_cursor_may_be_elsewhere() {
        let mut after = frame(&["XécdefYh"]);
        after.cells[1].text = Text::Char('é');
        let text = over(frame(&["abcdefgh"]), after);
        assert_eq!(text, "\x1b[?25l\x1b[1;1H\x1b[0mXé\x1b[1;7HY\x1b[0m");

        let mut after = frame(&["Xxcdefgh", "ijklmnop"]);
        after.cells[1] = Out {
            text: Text::Char('中'),
            style: Style::default(),
            wide: true,
        };
        after.cells[2].text = Text::Half;
        after.cells[after.width].text = Text::Char('Y');
        let text = over(frame(&["abcdefgh", "ijklmnop"]), after);
        assert_eq!(text, "\x1b[?25l\x1b[1;1H\x1b[0mX中\x1b[2;1HY\x1b[0m");

        let text = over(frame(&["ab", "cd"]), frame(&["aX", "Yd"]));
        assert_eq!(text, "\x1b[?25l\x1b[1;2H\x1b[0mX\x1b[2;1HY\x1b[0m");
    }

    /// Off, the default, every jump of the cursor is a CUP and every change
    /// of the pen a reset and the whole pen.
    #[test]
    fn off_every_move_is_absolute_and_every_pen_whole() {
        let red = Style {
            fg: Color::Index(1),
            ..Style::default()
        };
        let mut after = frame(&["XbcY", "Zfgh"]);
        after.cells[3].style = red;
        let mut s = Screen::new();
        s.draw(frame(&["abcd", "efgh"]), &hidden(), false, &[]);
        let text = String::from_utf8_lossy(&s.draw(after, &hidden(), false, &[])).into_owned();
        assert_eq!(
            text,
            "\x1b[?25l\x1b[1;1H\x1b[0mX\x1b[1;4H\x1b[0;31mY\x1b[2;1H\x1b[0mZ\x1b[0m"
        );
    }

    /// A change of colour alone is an SGR of the colours that changed and
    /// nothing else, the default spelt as such; a change of anything else is
    /// a reset and the whole pen, as the first is.
    #[test]
    fn a_change_of_colour_alone_says_only_the_colour() {
        let plain = Style::default();
        let red = Style {
            fg: Color::Index(1),
            ..plain
        };
        let teal_on_red = Style {
            fg: Color::Rgb(Rgb(0, 128, 128)),
            bg: Color::Index(9),
            ..plain
        };
        let bold = Style { bold: true, ..red };
        let mut out = Vec::new();
        recolour(&mut out, &plain, &red);
        assert_eq!(out, b"\x1b[31m");
        out.clear();
        recolour(&mut out, &red, &teal_on_red);
        assert_eq!(out, b"\x1b[38;2;0;128;128;101m");
        out.clear();
        recolour(&mut out, &teal_on_red, &plain);
        assert_eq!(out, b"\x1b[39;49m");
        assert!(only_colours_differ(&plain, &teal_on_red));
        assert!(!only_colours_differ(&red, &bold));

        let mut f = frame(&["abc"]);
        f.cells[1].style = red;
        f.cells[2].style = bold;
        let mut s = Screen::new();
        s.set_lean(true);
        let text = String::from_utf8_lossy(&s.draw(f, &hidden(), false, &[])).into_owned();
        assert_eq!(
            text,
            "\x1b[?25l\x1b[1;1H\x1b[0ma\x1b[31mb\x1b[0;1;31mc\x1b[0m"
        );
    }

    /// The frames of every test above, and of anything else, come out as the
    /// screen a fresh draw of the same frame does — moves, colours and all —
    /// whatever the frame was drawn over, lean or not: many random frames, of
    /// ASCII, letters that are not, wide characters, colours and attributes,
    /// each drawn over another both ways and compared, through vt100, with
    /// itself drawn on a screen that showed nothing.
    #[test]
    fn a_frame_drawn_over_another_shows_what_a_fresh_one_does() {
        let styles = [
            Style::default(),
            Style {
                fg: Color::Index(1),
                ..Style::default()
            },
            Style {
                fg: Color::Rgb(Rgb(10, 200, 30)),
                bg: Color::Index(4),
                ..Style::default()
            },
            Style {
                bg: Color::Index(12),
                ..Style::default()
            },
            Style {
                bold: true,
                fg: Color::Index(1),
                ..Style::default()
            },
            Style {
                italic: true,
                underline: Underline::Curl,
                sp: Color::Rgb(Rgb(255, 0, 0)),
                ..Style::default()
            },
        ];
        let letters = [' ', ' ', ' ', 'a', 'b', 'é', '─', '中'];
        // A small, fixed generator: the same frames on every run.
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let (w, h) = (13, 6);
        let mut random = |like: Option<&Frame>| {
            let mut f = Frame::new(w, h, Style::default());
            for r in 0..h {
                let mut c = 0;
                while c < w {
                    let at = r * w + c;
                    // Most cells as they were, as most of a frame is.
                    if let Some(like) = like.filter(|_| next(4) != 0) {
                        if like.cells[at].text != Text::Half && !like.cells[at].wide {
                            f.cells[at] = like.cells[at].clone();
                            c += 1;
                            continue;
                        }
                    }
                    let ch = letters[next(letters.len())];
                    f.cells[at].text = Text::Char(ch);
                    f.cells[at].style = styles[next(styles.len())];
                    if ch == '中' && c + 1 < w {
                        f.cells[at].wide = true;
                        f.cells[at + 1].text = Text::Half;
                        f.cells[at + 1].style = f.cells[at].style;
                        c += 2;
                    } else {
                        if ch == '中' {
                            f.cells[at].text = Text::Char('x');
                        }
                        c += 1;
                    }
                }
            }
            f
        };
        // Every cell, with what the parser keeps of its attributes; not the
        // cursor, which is hidden, and left wherever the last cell drawn was.
        let screen_of = |bytes: &[u8]| {
            let mut p = vt100::Parser::new(h as u16, w as u16, 0);
            p.process(bytes);
            p.screen().rows_formatted(0, w as u16).collect::<Vec<_>>()
        };
        for round in 0..300 {
            let first = random(None);
            let second = random(Some(&first));
            let fresh = screen_of(&Screen::new().draw(second.clone(), &hidden(), false, &[]));
            for lean in [false, true] {
                let mut s = Screen::new();
                s.set_lean(lean);
                let mut over = s.draw(first.clone(), &hidden(), false, &[]);
                over.extend(s.draw(second.clone(), &hidden(), false, &[]));
                assert_eq!(
                    screen_of(&over),
                    fresh,
                    "round {round}, lean {lean}: drawn over another, the frame shows something else"
                );
            }
        }
    }

    /// A scroll region left set — by a session Neovim's own TUI draws, left
    /// in the middle of a scroll — does not make an LF of a frame's a scroll.
    #[test]
    fn a_scroll_region_left_set_scrolls_nothing() {
        let mut s = Screen::new();
        s.set_lean(true);
        let mut bytes = s.draw(frame(&["abc", "def", "ghi", "jkl"]), &hidden(), false, &[]);
        bytes.extend_from_slice(b"\x1b[1;2r");
        let next = s.draw(frame(&["aXc", "Yef", "Zhi", "jkl"]), &hidden(), false, &[]);
        assert!(next.windows(2).any(|w| w == b"\r\n"), "{next:?}");
        bytes.extend(next);
        assert_eq!(shown(&bytes, 3, 4), "aXc\nYef\nZhi\njkl");
    }
}
