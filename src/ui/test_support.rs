//! Shared helpers for the screen tests, which all render to a `TestBackend`
//! and assert on the result the same way. [`emitted`] is the exception: one
//! property of the screens is only visible in the escape stream, so it renders
//! through the same crossterm backend the binary uses.

use std::cell::RefCell;
use std::rc::Rc;

use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use unicode_width::UnicodeWidthStr;

/// Reconstruct the rendered lines, skipping the filler cell that follows a
/// wide glyph so a CJK name is not reported as three columns per character.
/// Trailing blanks are trimmed from every line.
pub(super) fn render(w: u16, h: u16, mut draw: impl FnMut(&mut Frame)) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
    terminal.draw(|f| draw(f)).expect("draw");
    let buf = terminal.backend().buffer().clone();
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

/// The bytes a screen writes to a terminal, through the same crossterm backend
/// the binary uses. [`render`] asserts what a screen *means*; this is for what
/// it actually puts on the wire.
///
/// A fixed viewport rather than [`Terminal::new`], which asks the real terminal
/// for its size and fails under `cargo test`.
pub(super) fn emitted(w: u16, h: u16, mut draw: impl FnMut(&mut Frame)) -> String {
    /// A writer that keeps what was written, since ratatui's backend does not
    /// hand its own back on this version.
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

    // What `Screen::open` does, so the stream is the one the binary produces.
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

/// No screen may emit an SGR colour: each inherits the terminal's palette and
/// background and is correct under `NO_COLOR` by construction rather than by
/// remembering to check a flag at each call site (see [`super::draw`]).
pub(super) fn assert_no_colour(w: u16, h: u16, mut draw: impl FnMut(&mut Frame)) {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
    terminal.draw(|f| draw(f)).expect("draw");
    let buf = terminal.backend().buffer().clone();
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

/// Sizes every screen must survive: degenerate, one-row, and just big enough to
/// tempt an off-by-one. Shared so a case added for one screen covers them all —
/// `(10, 2)` was missing from one of the three copies this replaced.
pub(super) const TINY_SIZES: &[(u16, u16)] =
    &[(1, 1), (2, 1), (1, 2), (0, 0), (80, 1), (3, 3), (10, 2)];

/// No screen draws a border: they are centred text on the terminal's own
/// background, and a box would be the one thing that has to line up with
/// Neovim's.
pub(super) fn assert_no_borders(lines: &[String]) {
    for line in lines {
        for ch in "┌┐└┘─│├┤┬┴┼╭╮╰╯═║".chars() {
            assert!(!line.contains(ch), "found border char {ch:?} in {line:?}");
        }
    }
}
