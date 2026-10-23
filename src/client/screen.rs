//! What the terminal shows, and the bytes that turn it into a new frame.
//!
//! The client keeps the last frame and writes only differing cells, or all of
//! them after a resize or when Neovim clears its screen (how nvmux asks for a
//! repaint after drawing over a session; see `pty::repaint_through_server`).
//!
//! - Every frame stands alone: it assumes nothing about cursor, pen or scroll
//!   region (it resets the region before moving by LF; see [`move_to`]),
//!   because nvmux draws over the session between frames.
//! - Cells carry every attribute Neovim's TUI writes, plus OSC 8 hyperlinks.
//!   A link is closed before every cursor jump and at frame end, as the TUI
//!   does.
//! - Moves and colour changes use the shortest form ([`move_to`],
//!   [`recolour`]), as Neovim's TUI does. After an ambiguous-width character
//!   or a last-column write, the next move is absolute, since terminals
//!   disagree there. [`Screen::set_lean`] turns this off for tests.
//! - [`Screen::moving_rows`] has the terminal scroll moved rows first and diffs
//!   against the result ([`super::region`]); [`Screen::new`] rewrites them.
//! - Frames go inside a synchronized update (`?2026`) unless `'termsync'` is
//!   off, with the cursor hidden while drawing for terminals that ignore it
//!   (see [`crate::fade::SYNC_END`] and
//!   `boundary::Boundary::session_brackets_its_own_frames`).

use std::io::Write as _;

use super::compose::{Frame, Out};
use super::grid::Text;
use super::region::Bands;
use super::style::{Color, Style, Underline};
use crate::fade::{SYNC_BEGIN, SYNC_END};
use crate::palette::Rgb;

/// The terminal cursor as a frame leaves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub visible: bool,
    pub row: usize,
    pub col: usize,
    /// `DECSCUSR` value, 1 to 6.
    pub shape: u8,
    /// `None` for the terminal's default.
    pub color: Option<Rgb>,
}

/// OSC 8 with no target: ends a hyperlink.
const LINK_END: &[u8] = b"\x1b]8;;\x1b\\";

/// `DECSTBM` with no margins: full-screen scroll region (so LF scrolls only at
/// the bottom); also homes the cursor. Sent before a frame moves by LF.
pub(super) const WHOLE_SCROLL_REGION: &[u8] = b"\x1b[r";

#[derive(Debug)]
pub struct Screen {
    shown: Option<Frame>,
    /// A previous frame, reused as the next frame's buffer.
    spare: Option<Frame>,
    /// Cursor shape and colour as last set; resent only on change, since
    /// setting the shape restarts its blink.
    shape: Option<u8>,
    color: Option<Option<Rgb>>,
    /// The cursor as the last frame left it (see [`Screen::draw_into`]).
    cursor: Option<Cursor>,
    lean: bool,
    /// Moved-row detection; `None` unless [`Screen::set_regions`].
    bands: Option<Bands>,
    /// Per row, whether `shown` differs from `spare` (for
    /// [`Screen::take_like_shown`]). Empty when unknown.
    differs: Vec<bool>,
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
            cursor: None,
            lean: true,
            bands: None,
            differs: Vec::new(),
        }
    }

    /// A client's screen: moved rows are scrolled by the terminal.
    pub fn moving_rows() -> Self {
        let mut screen = Self::new();
        screen.set_regions(true);
        screen
    }

    /// With `lean` off (tests only), every cursor jump is absolute and every
    /// pen change a full reset. The result on screen is the same.
    #[cfg(test)]
    pub fn set_lean(&mut self, lean: bool) {
        self.lean = lean;
    }

    /// Have the terminal scroll moved rows instead of rewriting them (see
    /// [`super::region`]). Off by default.
    pub fn set_regions(&mut self, on: bool) {
        if on != self.bands.is_some() {
            self.bands = on.then(Bands::default);
        }
    }

    /// A spare frame to compose the next into (see
    /// [`super::compose::compose_into`]); empty at first.
    pub fn take_spare(&mut self) -> Frame {
        self.spare
            .take()
            .unwrap_or_else(|| Frame::new(0, 0, Style::default()))
    }

    /// The spare frame updated to equal the shown one, copying only differing
    /// rows, as a base for [`super::compose::compose_rows`]. `None` if either
    /// is missing or not `width` x `height`.
    pub fn take_like_shown(&mut self, width: usize, height: usize) -> Option<Frame> {
        let size = |f: &Frame| (f.width, f.height) == (width, height);
        let shown = self.shown.as_ref().filter(|f| size(f))?;
        if !self.spare.as_ref().is_some_and(size) || self.differs.len() != height {
            return None;
        }
        let mut spare = self.spare.take()?;
        for r in (0..height).filter(|&r| self.differs[r]) {
            let row = r * width..(r + 1) * width;
            spare.cells[row.clone()].clone_from_slice(&shown.cells[row]);
        }
        Some(spare)
    }

    /// Forget the cursor, so the next frame always writes something.
    pub fn forget_cursor(&mut self) {
        self.cursor = None;
    }

    /// The next frame repaints every cell and resets cursor shape and colour.
    pub fn invalidate(&mut self) {
        if let Some(shown) = self.shown.take() {
            self.spare = Some(shown);
        }
        if let Some(bands) = self.bands.as_mut() {
            bands.forget();
        }
        self.differs.clear();
        self.shape = None;
        self.color = None;
        self.cursor = None;
    }

    /// Bytes that turn the screen into `frame` with `cursor`. Hyperlink targets
    /// come from `links` (see [`Style::link`]).
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

    /// [`Screen::draw`], appended to `out`.
    ///
    /// A frame that changes no cell or cursor writes nothing, rather than an
    /// envelope that wakes the relay and terminal for nothing (e.g. animation
    /// frames that round to the last, 40 times a second). Safe, since nvmux
    /// restores the cursor after drawing over the session.
    pub fn draw_into(
        &mut self,
        out: &mut Vec<u8>,
        frame: Frame,
        cursor: &Cursor,
        sync: bool,
        links: &[Box<str>],
    ) {
        self.write(out, frame, cursor, sync, links, None);
    }

    /// [`Screen::draw_into`], comparing only the rows flagged in `rows` (see
    /// [`Screen::take_like_shown`]).
    pub fn draw_rows_into(
        &mut self,
        out: &mut Vec<u8>,
        frame: Frame,
        cursor: &Cursor,
        sync: bool,
        links: &[Box<str>],
        rows: &[bool],
    ) {
        self.write(out, frame, cursor, sync, links, Some(rows));
    }

    /// Shared by [`Screen::draw_into`] and [`Screen::draw_rows_into`]; if
    /// given, only rows flagged in `changed` are compared.
    fn write(
        &mut self,
        out: &mut Vec<u8>,
        frame: Frame,
        cursor: &Cursor,
        sync: bool,
        links: &[Box<str>],
        changed: Option<&[bool]>,
    ) {
        // Truncated back to here if the frame changes nothing.
        let start = out.len();
        let mut drew = false;
        if sync {
            out.extend_from_slice(SYNC_BEGIN);
        }
        out.extend_from_slice(b"\x1b[?25l");
        // Where the scroll region reset goes if the frame moves by LF: before
        // its first move, a CUP.
        let head = out.len();
        let shown = self.shown.take();
        let fits = |old: &Frame| (old.width, old.height) == (frame.width, frame.height);
        // Scroll moved rows first; `bands` then models the screen they leave
        // (see `super::region`), so it knows which rows are already right.
        let mut moved = false;
        let mut known = false;
        if let Some(bands) = self.bands.as_mut() {
            match shown.as_ref().filter(|old| fits(old)) {
                Some(old) => {
                    moved = bands.moved(out, old, &frame, changed);
                    known = true;
                }
                None => bands.forget(),
            }
        }
        drew |= moved;
        let bands = self.bands.as_ref().filter(|_| known);
        let old = shown.as_ref().filter(|old| fits(old));
        // Rows differing from the shown frame, for `take_like_shown`; all of
        // them if the size changed.
        let mut differs = std::mem::take(&mut self.differs);
        differs.clear();
        differs.resize(frame.height, true);
        let mut pen: Option<Style> = None;
        let mut at: Option<(usize, usize)> = None;
        // Whether the cursor is surely at `at`, so a relative move is safe:
        // lean, and only plain ASCII written since the last CUP, none in the
        // last column. Neovim's TUI also re-places the cursor after an
        // ambiguous-width character.
        let mut sure = false;
        let mut fed = false;
        // The open hyperlink, closed before every jump (as Neovim's TUI does)
        // so it never spans skipped cells.
        let mut link = 0;
        for r in 0..frame.height {
            // Most rows are unchanged; skip them whole.
            let row = r * frame.width..(r + 1) * frame.width;
            // What the terminal shows on this row, if known.
            let was = match bands {
                Some(bands) => old.map(|old| bands.was(old, r)),
                None => old.map(|old| &old.cells[row.clone()]),
            };
            let same = match bands {
                Some(bands) => {
                    differs[r] = !bands.equal()[r];
                    bands.same()[r]
                }
                None => {
                    let same = old.is_some()
                        && (changed.is_some_and(|changed| !changed[r])
                            || was.is_some_and(|was| *was == frame.cells[row]));
                    differs[r] = !same;
                    same
                }
            };
            if same {
                continue;
            }
            let mut c = 0;
            while c < frame.width {
                let cell = &frame.cells[r * frame.width + c];
                let span = if cell.wide { 2 } else { 1 };
                let same = was.is_some_and(|was| {
                    (c..(c + span).min(frame.width))
                        .all(|k| was[k] == frame.cells[r * frame.width + k])
                });
                // A right half is drawn with its left.
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
                    fed |= move_to(out, &frame, from, (r, c), pen.as_ref(), fed || moved);
                    sure = self.lean;
                }
                // The link is handled by OSC 8 below, not the pen.
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
                        // An id, so hovering a link split by a float
                        // underlines all of it.
                        Some(url) => {
                            let _ = write!(out, "\x1b]8;id=nvmux-{};{url}\x1b\\", cell.style.link);
                        }
                        None => out.extend_from_slice(LINK_END),
                    }
                    link = cell.style.link;
                }
                cell.text.push_to(out);
                drew = true;
                c += span;
                at = Some((r, c));
                sure &= c < frame.width && plain(cell);
            }
        }
        // Moving rows already reset the region.
        if fed && !moved {
            out.splice(head..head, WHOLE_SCROLL_REGION.iter().copied());
        }
        if known {
            if let Some(bands) = self.bands.as_mut() {
                bands.shown();
            }
        }
        // nvmux's own cells (hint row, notice) must not inherit pen or link.
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
        if !drew && self.cursor == Some(*cursor) {
            out.truncate(start);
        }
        self.cursor = Some(*cursor);
        self.shown = Some(frame);
        self.spare = shown;
        self.differs = differs;
    }
}

/// Max gap cells rewritten instead of crossed; more cost more than a CUF.
const REWRITE_AT_MOST: usize = 3;

/// Move the cursor from `at` (`None` if unknown) to `to` in the fewest bytes:
///
/// - along a row: rewrite a few plain ASCII cells already in `pen`, or CUF;
/// - to the next row: CR LF, then as above;
/// - otherwise a CUP.
///
/// Returns whether it moved by LF. LF is never used from the last row, and the
/// frame first resets the scroll region ([`WHOLE_SCROLL_REGION`]) so LF never
/// scrolls: Neovim's own TUI sets a region for each scroll, and a switch can
/// land before its reset. `fed` says whether the reset was already paid for
/// (by an earlier LF or by moved rows); if not, the LF must save its cost.
fn move_to(
    out: &mut Vec<u8>,
    frame: &Frame,
    at: Option<(usize, usize)>,
    to: (usize, usize),
    pen: Option<&Style>,
    fed: bool,
) -> bool {
    let (r, c) = to;
    // CUP length: ESC [ row ; col H.
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

/// Move along row `r` from `from` to `to` in fewer than `limit` bytes, by
/// rewriting the cells between (few, plain ASCII, in `pen`) or by CUF.
/// Returns whether it moved.
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

/// Printable ASCII: one column wide on every terminal.
fn plain(cell: &Out) -> bool {
    !cell.wide && matches!(cell.text, Text::Char(' '..='~'))
}

/// Whether `was` and `now` differ only in fg/bg.
fn only_colours_differ(was: &Style, now: &Style) -> bool {
    Style {
        fg: now.fg,
        bg: now.bg,
        ..*was
    } == *now
}

/// One SGR of only the changed colours, no reset (default as `39`/`49`).
/// Requires [`only_colours_differ`].
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
    // Turn the first `;` into the `[`.
    out[open] = b'[';
    out.push(b'm');
}

fn digits(n: usize) -> usize {
    n.checked_ilog10().map_or(1, |d| d as usize + 1)
}

pub(super) fn push_dec(out: &mut Vec<u8>, mut n: usize) {
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

/// Set the pen to `s` from scratch: reset plus all attributes in one
/// old-terminal-safe SGR, then underline style (`4:3` etc.) and colour (`58`)
/// each in their own sequence, so a terminal that rejects one loses only that.
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

/// One SGR colour as Neovim's TUI writes it on a 256-colour terminal: 0–15 by
/// their own codes (`base`/`bright`), others as `ext;5`, RGB as `ext;2`.
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

/// Cells of `frame` that differ from `blank`, for tests.
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

    /// The screen after `bytes`, via vt100 as in the relay tests.
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

    /// `take_like_shown` copies only differing rows, and `draw_rows_into`
    /// writes only flagged rows.
    #[test]
    fn the_frame_done_with_is_made_the_one_shown() {
        let mut s = Screen::new();
        assert!(s.take_like_shown(2, 2).is_none(), "nothing shown");
        let first = s.draw(frame(&["ab", "cd"]), &hidden(), true, &[]);
        assert!(s.take_like_shown(2, 2).is_none(), "nothing done with yet");
        let second = s.draw(frame(&["ab", "cX"]), &hidden(), true, &[]);
        assert!(s.take_like_shown(3, 2).is_none(), "another size");
        let mut like = s.take_like_shown(2, 2).expect("the frame done with");
        assert_eq!(like, frame(&["ab", "cX"]));
        like.cells[0].text = Text::Char('Y');
        let mut third = Vec::new();
        s.draw_rows_into(&mut third, like, &hidden(), true, &[], &[true, false]);
        let text = String::from_utf8_lossy(&third);
        assert!(text.contains("\x1b[1;1H\x1b[0mY"), "{text:?}");
        assert!(!text.contains('X'), "the row said not to change: {text:?}");
        let mut all = first;
        all.extend_from_slice(&second);
        all.extend_from_slice(&third);
        assert_eq!(shown(&all, 2, 2), "Yb\ncX");
    }

    #[test]
    fn invalidating_paints_every_cell_again() {
        let mut s = Screen::new();
        s.draw(frame(&["ab"]), &hidden(), false, &[]);
        s.invalidate();
        let again = s.draw(frame(&["ab"]), &hidden(), false, &[]);
        assert_eq!(shown(&again, 2, 1), "ab");
    }

    /// A cursor change alone writes only the cursor; bytes already in `out`
    /// are kept when the frame writes nothing.
    #[test]
    fn a_frame_that_changes_nothing_writes_nothing() {
        let at = |row, col| Cursor {
            visible: true,
            row,
            col,
            shape: 2,
            color: None,
        };
        let mut s = Screen::new();
        s.draw(frame(&["ab", "cd"]), &at(0, 1), true, &[]);
        assert_eq!(s.draw(frame(&["ab", "cd"]), &at(0, 1), true, &[]), b"");

        let mut out = b"said".to_vec();
        s.draw_into(&mut out, frame(&["ab", "cd"]), &at(0, 1), true, &[]);
        assert_eq!(out, b"said", "nothing of its own");

        let moved = s.draw(frame(&["ab", "cd"]), &at(1, 0), false, &[]);
        assert_eq!(
            moved, b"\x1b[?25l\x1b[2;1H\x1b[?25h",
            "the cursor, and no cell"
        );
        let gone = s.draw(frame(&["ab", "cd"]), &hidden(), false, &[]);
        assert_eq!(gone, b"\x1b[?25l", "hidden, and no cell");
        assert_eq!(s.draw(frame(&["ab", "cd"]), &hidden(), false, &[]), b"");

        s.invalidate();
        let again = s.draw(frame(&["ab", "cd"]), &hidden(), false, &[]);
        assert_eq!(shown(&again, 2, 2), "ab\ncd", "every cell again");
    }

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
        // Unchanged shape and colour are not resent.
        let bytes = s.draw(frame(&["ab", "cd"]), &cursor, false, &[]);
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(" q") && !text.contains("]12"), "{text:?}");
        assert!(!text.contains("2026"), "{text:?}");
    }

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

    /// The OSC 8 is closed before every cursor jump and at frame end.
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

    /// What `second` writes over `first`, as text.
    fn over(first: Frame, second: Frame) -> String {
        let mut s = Screen::new();
        s.draw(first, &hidden(), false, &[]);
        String::from_utf8_lossy(&s.draw(second, &hidden(), false, &[])).into_owned()
    }

    /// A gap of a few ASCII cells in the pen is rewritten, otherwise CUF. A
    /// frame's first move is always a CUP.
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
        // Unchanged non-ASCII: crossed, not rewritten.
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
        // Unchanged in a different pen: crossed, not rewritten.
        let mut was = before;
        was.cells[1].style = red;
        after.cells[1].style = red;
        assert_eq!(over(was, after), "\x1b[?25l\x1b[1;1H\x1b[0mX\x1b[CZ\x1b[0m");
    }

    /// Next row by CR LF, further by CUP. A frame with an LF resets the scroll
    /// region first, and its first LF must be worth that cost.
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

    /// After non-ASCII (width may differ from Neovim's) or a last-column write
    /// (terminals disagree on the cursor), the next move is a CUP.
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

    #[test]
    fn off_every_move_is_absolute_and_every_pen_whole() {
        let red = Style {
            fg: Color::Index(1),
            ..Style::default()
        };
        let mut after = frame(&["XbcY", "Zfgh"]);
        after.cells[3].style = red;
        let mut s = Screen::new();
        s.set_lean(false);
        s.draw(frame(&["abcd", "efgh"]), &hidden(), false, &[]);
        let text = String::from_utf8_lossy(&s.draw(after, &hidden(), false, &[])).into_owned();
        assert_eq!(
            text,
            "\x1b[?25l\x1b[1;1H\x1b[0mX\x1b[1;4H\x1b[0;31mY\x1b[2;1H\x1b[0mZ\x1b[0m"
        );
    }

    /// A change to anything but colours is a full reset.
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
        let text =
            String::from_utf8_lossy(&Screen::new().draw(f, &hidden(), false, &[])).into_owned();
        assert_eq!(
            text,
            "\x1b[?25l\x1b[1;1H\x1b[0ma\x1b[31mb\x1b[0;1;31mc\x1b[0m"
        );
    }

    /// Random frames drawn over another, lean or not, match a fresh draw.
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
        // Fixed-seed xorshift: the same frames every run.
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
                    // Keep most cells, as real frames do.
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
        // Cells and attributes; not the cursor, which is hidden and wherever
        // the last cell left it.
        let screen_of = |bytes: &[u8]| {
            let mut p = vt100::Parser::new(h as u16, w as u16, 0);
            p.process(bytes);
            p.screen().rows_formatted(0, w as u16).collect::<Vec<_>>()
        };
        for round in 0..300 {
            let first = random(None);
            let second = random(Some(&first));
            let mut whole = Screen::new();
            whole.set_lean(false);
            let fresh = screen_of(&whole.draw(second.clone(), &hidden(), false, &[]));
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

    /// A region left set mid-scroll by Neovim's TUI must not make LF scroll.
    #[test]
    fn a_scroll_region_left_set_scrolls_nothing() {
        let mut s = Screen::new();
        let mut bytes = s.draw(frame(&["abc", "def", "ghi", "jkl"]), &hidden(), false, &[]);
        bytes.extend_from_slice(b"\x1b[1;2r");
        let next = s.draw(frame(&["aXc", "Yef", "Zhi", "jkl"]), &hidden(), false, &[]);
        assert!(next.windows(2).any(|w| w == b"\r\n"), "{next:?}");
        bytes.extend(next);
        assert_eq!(shown(&bytes, 3, 4), "aXc\nYef\nZhi\njkl");
    }
}
