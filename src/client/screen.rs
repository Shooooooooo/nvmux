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
//! from nothing. nvmux writes over the session between frames — the hint row,
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

use super::compose::Frame;
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

/// What the terminal shows.
#[derive(Debug)]
pub struct Screen {
    shown: Option<Frame>,
    /// The cursor's shape and colour as last set, so they are only set again
    /// when they change — setting the shape restarts its blink.
    shape: Option<u8>,
    color: Option<Option<Rgb>>,
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
            shape: None,
            color: None,
        }
    }

    /// Forget what the terminal shows: the next frame paints every cell, and
    /// sets the cursor's shape and colour afresh.
    pub fn invalidate(&mut self) {
        self.shown = None;
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
        if sync {
            out.extend_from_slice(SYNC_BEGIN);
        }
        out.extend_from_slice(b"\x1b[?25l");
        let old = self
            .shown
            .take()
            .filter(|old| (old.width, old.height) == (frame.width, frame.height));
        let mut pen: Option<Style> = None;
        let mut at: Option<(usize, usize)> = None;
        // The hyperlink open, if one is. Closed before every jump, as
        // Neovim's TUI does, so that a link never runs on over cells between
        // where it was left and where the next is drawn.
        let mut link = 0;
        for r in 0..frame.height {
            let mut c = 0;
            while c < frame.width {
                let cell = &frame.cells[r * frame.width + c];
                let span = if cell.wide { 2 } else { 1 };
                let same = old.as_ref().is_some_and(|old| {
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
                    let _ = write!(out, "\x1b[{};{}H", r + 1, c + 1);
                }
                // The link is no part of the pen: it is the OSC 8's below.
                let style = Style {
                    link: 0,
                    ..cell.style
                };
                if pen != Some(style) {
                    sgr(&mut out, &style);
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
                cell.text.push_to(&mut out);
                c += span;
                at = Some((r, c));
            }
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
        out
    }
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
    let _ = match c {
        Color::Default => Ok(()),
        Color::Index(n) if n < 8 => write!(out, ";{}", base + n),
        Color::Index(n) if n < 16 => write!(out, ";{}", bright + n - 8),
        Color::Index(n) => write!(out, ";{ext};5;{n}"),
        Color::Rgb(Rgb(r, g, b)) => write!(out, ";{ext};2;{r};{g};{b}"),
    };
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
}
