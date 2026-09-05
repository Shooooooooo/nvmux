//! Shared helpers for the screen tests, which all render to a `TestBackend`
//! and assert on the result the same way.

use ratatui::backend::TestBackend;
use ratatui::style::Color;
use ratatui::{Frame, Terminal};
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
