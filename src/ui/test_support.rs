//! Shared helpers for the screen tests, which render to a `TestBackend`.
//! [`emitted`] renders through the binary's crossterm backend instead, for
//! what is only visible in the escape stream. Also the picker builders shared
//! by the picker and renderer tests.

use std::cell::RefCell;
use std::rc::Rc;

use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use unicode_width::UnicodeWidthStr;

use super::app::{App, Key};
use crate::session::Session;

/// Serialises the panel tests: each opens on what the session loop last handed
/// over and leaves its changes behind (see [`super::picker::hand`]).
pub(crate) static PANELS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Render one frame to a `w` x `h` `TestBackend` and return its cells.
pub(super) fn buffer(w: u16, h: u16, mut draw: impl FnMut(&mut Frame)) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
    terminal.draw(|f| draw(f)).expect("draw");
    terminal.backend().buffer().clone()
}

/// The rendered lines, trailing blanks trimmed. The filler cell after a wide
/// glyph is skipped so CJK text is not reported with extra columns.
pub(super) fn render(w: u16, h: u16, draw: impl FnMut(&mut Frame)) -> Vec<String> {
    let buf = buffer(w, h, draw);
    (0..buf.area.height)
        .map(|y| {
            let mut line = String::new();
            let mut skip = 0u16;
            for x in 0..buf.area.width {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                let sym = buf[(x, y)].symbol();
                skip = sym.width().saturating_sub(1) as u16;
                line.push_str(sym);
            }
            line.trim_end().to_string()
        })
        .collect()
}

/// The bytes a screen writes, through the binary's crossterm backend. Uses a
/// fixed viewport: [`Terminal::new`] queries the real terminal's size, which
/// fails under `cargo test`.
pub(super) fn emitted(w: u16, h: u16, mut draw: impl FnMut(&mut Frame)) -> String {
    // Keeps what was written: ratatui's backend hands its writer back only
    // under an unstable feature.
    #[derive(Clone)]
    struct Tap(Rc<RefCell<Vec<u8>>>);

    impl std::io::Write for Tap {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // As `Screen::open` does, so the stream matches the binary's.
    ratatui::crossterm::style::force_color_output(true);

    let tap = Tap(Rc::new(RefCell::new(Vec::new())));
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(tap.clone()),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, w, h)),
        },
    )
    .expect("terminal");
    terminal.draw(|f| draw(f)).expect("draw");

    let bytes = tap.0.borrow().clone();
    String::from_utf8(bytes).expect("the screens write UTF-8")
}

/// No screen may emit an SGR colour: screens inherit the terminal's palette,
/// so they honour `NO_COLOR` by construction (see [`super::draw`]).
pub(super) fn assert_no_colour(w: u16, h: u16, draw: impl FnMut(&mut Frame)) {
    let buf = buffer(w, h, draw);
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let cell = &buf[(x, y)];
            assert_eq!(
                cell.fg,
                Color::Reset,
                "cell ({x},{y}) set a foreground colour"
            );
            assert_eq!(
                cell.bg,
                Color::Reset,
                "cell ({x},{y}) set a background colour"
            );
        }
    }
}

/// Sizes every screen must survive: degenerate, one-row, and just big enough
/// for an off-by-one. The attach notice ([`crate::announce`]) uses them too.
pub(crate) const TINY_SIZES: &[(u16, u16)] =
    &[(1, 1), (2, 1), (1, 2), (0, 0), (80, 1), (3, 3), (10, 2)];

/// No screen draws a border: a box would have to line up with Neovim's.
pub(super) fn assert_no_borders(lines: &[String]) {
    for line in lines {
        for ch in "┌┐└┘─│├┤┬┴┼╭╮╰╯═║".chars() {
            assert!(!line.contains(ch), "found border char {ch:?} in {line:?}");
        }
    }
}

/// The round corners of a floating panel drawn as `buf` cut off only blank
/// cells (see [`super::float::RADIUS`]).
pub(super) fn assert_corners_blank(buf: &Buffer) {
    let area = buf.area;
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &buf[(x, y)];
            if super::float::distance(area, x, y) > 0.0 {
                assert!(
                    cell.symbol() == " "
                        && cell.bg == Color::Reset
                        && !cell
                            .modifier
                            .intersects(Modifier::REVERSED | Modifier::UNDERLINED),
                    "({x},{y}) is cut off: {cell:?}"
                );
            }
        }
    }
}

/// Left and right padding of a line on a `w`-column screen: bytes of leading
/// spaces, and columns of slack on the right.
pub(super) fn padding(line: &str, w: usize) -> (usize, usize) {
    (line.len() - line.trim_start().len(), w - line.width())
}

/// A picker of sessions with these names, numbered as
/// `transport::finish_listing` would, since nothing else sets it here.
pub(super) fn picker(names: &[&str]) -> App {
    App::new(
        names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let num = i as u32 + 1;
                let mut s = Session::new(format!("id{i:06}"), n.to_string(), 100 + i as u32, num);
                s.state.num = num;
                s
            })
            .collect(),
    )
}

/// [`picker`], with `query` typed into its filter.
pub(super) fn filtering(names: &[&str], query: &str) -> App {
    let mut a = picker(names);
    a.on_key(Key::Char('/'));
    for c in query.chars() {
        a.on_key(Key::Char(c));
    }
    a
}
