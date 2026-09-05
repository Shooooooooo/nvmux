//! The key-binding help — a third screen, shown by `Ctrl-t ?`.
//!
//! This is a whole screen rather than a popup over the editor, and for a
//! harder reason than taste. What a popup would sit on top of is Neovim's
//! screen, and [`crate::pty`] never writes into the child's output, so nothing
//! can be drawn over the live editor. The one thing nvmux can do is what
//! `Ctrl-t c` already does: take the terminal the way the prompt takes it,
//! clear it, draw, and hand the same client back to be repainted on resume. It
//! keeps the picker's visual language all the same: content centred, one dim
//! hint row on the last line, no borders, no title, and no colour ever set.
//!
//! # Nothing here knows what a key does
//!
//! Every command row comes from [`keys::BINDINGS`], the table
//! [`keys::Prefix::feed`] itself runs on, so this screen cannot say something
//! the machine does not do. The two rows that are not commands — `Ctrl-t
//! Ctrl-t` is a literal, and `Ctrl-t` plus anything else is replayed — are the
//! machine's `else` branches rather than `Action`s, so they are spelled here
//! and pinned to the machine by a test. The lone-prefix timeout (a `Ctrl-t`
//! followed by silence is a literal one) is deliberately not a row: it is not
//! a keybinding, and the README leaves it out for the same reason.
//!
//! # Why only named keys close it
//!
//! `Ctrl-t` reaches this screen as `Key::Char('t')`, because `translate` folds
//! the control modifier away for everything but `Ctrl-c`, `Ctrl-n` and
//! `Ctrl-p`. If any key closed the help, a user who read "Ctrl-t d" here and
//! typed it would close the screen on the `Ctrl-t` and send a bare `d` into
//! Neovim's normal mode. So `t`, `d` and `c` do nothing here, and only `Esc`,
//! `q`, `Enter`, `?` and `Ctrl-c` close it. Nothing typed on this screen is
//! ever forwarded.
//!
//! One boundary is shared with the prompt and the picker: keys crossterm has
//! already parsed when the screen closes never reach Neovim, and are seen by
//! whichever nvmux screen opens next. Human typing rarely lands two keys in
//! one read, and the relay reads the terminal directly, so there is no clean
//! fix from here.

use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::app::Key;
use super::draw;
use crate::error::Result;
use crate::keys::{self, PREFIX_LABEL};

/// Key then action, the picker's hint grammar. `q`, `Enter`, `?` and `Ctrl-c`
/// also close, unadvertised, the way the picker leaves `g`, `G`, `/` and
/// `Ctrl-c` off its own hint row.
const HINTS: &str = "esc back";

/// Columns between the key column and the description: the same three spaces
/// that separate groups on the hint line.
const GAP: usize = 3;

/// One line of the table.
#[derive(Debug)]
struct Row {
    keys: String,
    what: &'static str,
}

/// What one keypress meant.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    None,
    Close,
}

/// The command rows from [`keys::BINDINGS`], then the two rules that are not
/// commands.
fn rows() -> Vec<Row> {
    let mut rows: Vec<Row> = keys::BINDINGS
        .iter()
        .map(|b| Row {
            keys: format!("{PREFIX_LABEL} {}", b.key as char),
            what: b.help,
        })
        .collect();
    rows.push(Row {
        keys: format!("{PREFIX_LABEL} {PREFIX_LABEL}"),
        what: "send a literal Ctrl-t to Neovim",
    });
    rows.push(Row {
        keys: format!("{PREFIX_LABEL} other"),
        what: "send Ctrl-t and that key to Neovim",
    });
    rows
}

/// Named keys only. `t`, `d` and `c` — and therefore `Ctrl-t`, which arrives
/// as `Key::Char('t')` — are deliberately not here: a chord typed while the
/// help is open must do nothing, not close the help and forward its second
/// key.
fn on_key(key: Key) -> Step {
    match key {
        // Ctrl-c closes like esc. Quitting nvmux here would tear the user out
        // of a live session they only meant to read a key list in.
        Key::Char('q') | Key::Char('?') | Key::Esc | Key::Enter | Key::CtrlC => Step::Close,
        _ => Step::None,
    }
}

/// Show the bindings until the user dismisses them.
///
/// Runs its own terminal, entering and leaving the alternate screen exactly as
/// the prompt does, and handing the session back untouched afterwards.
pub fn run() -> Result<()> {
    // Same reasoning as the picker: `draw` never sets a colour, and crossterm's
    // own NO_COLOR handling would rewrite one into a full SGR reset that wipes
    // the dim this screen relies on.
    ratatui::crossterm::style::force_color_output(true);

    let mut terminal = ratatui::try_init()?;
    // The session's own alternate screen is still showing, and a second "enter
    // alternate screen" does not clear it on every terminal: xterm and kitty
    // treat it as a no-op. ratatui's first draw only paints the cells that
    // differ from an empty buffer, so without this the table would land in
    // the middle of the editor's last frame.
    terminal.clear()?;
    let outcome = run_loop(&mut terminal);
    // Restore before propagating anything: an error that leaves the terminal in
    // raw mode with no echo is far worse than the error itself.
    ratatui::try_restore()?;
    outcome
}

fn run_loop(terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
    let rows = rows();
    loop {
        terminal.draw(|f| draw(f, &rows))?;

        if !event::poll(super::TICK)? {
            continue;
        }
        let key = match event::read()? {
            // Press only: with the kitty protocol pushed by Neovim, a release
            // would otherwise count as a second keypress.
            Event::Key(k) if k.kind == KeyEventKind::Press => super::translate(k),
            // A resize just redraws on the next pass.
            _ => continue,
        };

        if on_key(key) == Step::Close {
            return Ok(());
        }
    }
}

fn draw(frame: &mut Frame, rows: &[Row]) {
    let area = frame.area();
    if area.height == 0 || area.width == 0 {
        return;
    }

    // The last row is the hint line; everything above it is the table. Same
    // split as the picker and the prompt, so the three screens line up.
    let body = Rect {
        height: area.height.saturating_sub(1),
        ..area
    };
    let bottom = Rect {
        y: area.y + area.height - 1,
        height: 1,
        ..area
    };

    draw_table(frame, rows, body);
    draw_hints(frame, bottom);
}

/// One left-aligned block, centred on both axes.
///
/// Keys are padded to the widest key by *display* width rather than `{:<w$}`,
/// so a non-ASCII key label would still line up. Rows are truncated to the
/// block and never wrapped, which is what keeps the hint row where it is.
fn draw_table(frame: &mut Frame, rows: &[Row], area: Rect) {
    if area.height == 0 || area.width == 0 || rows.is_empty() {
        return;
    }
    let key_width = rows.iter().map(|r| r.keys.width()).max().unwrap_or(0);
    let what_width = rows.iter().map(|r| r.what.width()).max().unwrap_or(0);
    let block = draw::centre(
        area,
        (key_width + GAP + what_width) as u16,
        rows.len() as u16,
    );

    let lines: Vec<Line> = rows
        .iter()
        .take(block.height as usize)
        .map(|r| {
            let pad = " ".repeat(key_width - r.keys.width() + GAP);
            Line::from(draw::truncate(
                &format!("{}{pad}{}", r.keys, r.what),
                block.width as usize,
            ))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), block);
}

fn draw_hints(frame: &mut Frame, area: Rect) {
    let text = draw::truncate(HINTS, area.width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().add_modifier(Modifier::DIM),
        )))
        .alignment(Alignment::Center),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{Prefix, PREFIX};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Reconstruct the rendered lines, skipping the filler cell that follows a
    /// wide glyph so a CJK name is not reported as three columns per character.
    fn render(w: u16, h: u16) -> Vec<String> {
        let rows = rows();
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        terminal.draw(|f| draw(f, &rows)).expect("draw");
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

    /// The line a row was drawn on. Matched on the *start* of the line rather
    /// than anywhere in it: "send a literal Ctrl-t to Neovim" contains the
    /// text `Ctrl-t t`, so `contains` would find the picker row twice.
    fn line_for<'a>(lines: &'a [String], row: &Row) -> &'a String {
        lines
            .iter()
            .find(|l| l.trim_start().starts_with(&row.keys))
            .unwrap_or_else(|| panic!("no line for {row:?} in {lines:#?}"))
    }

    #[test]
    fn q_question_mark_esc_enter_and_ctrl_c_all_close_it() {
        for key in [
            Key::Char('q'),
            Key::Char('?'),
            Key::Esc,
            Key::Enter,
            Key::CtrlC,
        ] {
            assert_eq!(on_key(key), Step::Close, "{key:?} should close");
        }
    }

    /// Ctrl-C quits the picker. Doing that here would exit nvmux out from under
    /// a session the user is still in, so it closes like esc instead.
    #[test]
    fn ctrl_c_closes_rather_than_quitting() {
        assert_eq!(on_key(Key::CtrlC), Step::Close);
    }

    /// `Ctrl-t` arrives here as `Key::Char('t')`. If it closed the help, a
    /// chord typed from this screen would be half-forwarded: the help would
    /// close on the `Ctrl-t` and the next key would land in Neovim's normal
    /// mode.
    #[test]
    fn the_prefix_and_its_command_letters_do_nothing_here() {
        for key in [
            Key::Char('t'),
            Key::Char('d'),
            Key::Char('c'),
            Key::Up,
            Key::Backspace,
            Key::Other,
        ] {
            assert_eq!(on_key(key), Step::None, "{key:?} must be ignored");
        }
    }

    /// The rendered rows are checked against what the machine actually does,
    /// not against the table both of them read.
    #[test]
    fn every_row_matches_what_the_prefix_machine_does() {
        let rows = rows();
        for b in 0u8..=255 {
            let acts = Prefix::new()
                .feed(&[PREFIX, b])
                .iter()
                .any(|s| matches!(s, keys::Step::Act(_)));
            // Equality, not `starts_with`: `o` would otherwise match the
            // `Ctrl-t other` row.
            let listed = rows
                .iter()
                .any(|r| r.keys == format!("{PREFIX_LABEL} {}", b as char));
            assert_eq!(
                acts, listed,
                "byte {b:#04x}: the machine acts = {acts}, the help lists it = {listed}"
            );
        }
    }

    #[test]
    fn every_binding_is_listed_with_its_description() {
        let lines = render(80, 24);
        for (b, row) in keys::BINDINGS.iter().zip(rows()) {
            let line = line_for(&lines, &row);
            assert!(
                line.contains(b.help),
                "{:?} row lacks its description: {line:?}",
                row.keys
            );
        }
    }

    #[test]
    fn rows_follow_the_table_order_then_the_two_fixed_rows() {
        let lines = render(80, 24);
        let body: Vec<&str> = lines[..23]
            .iter()
            .filter(|l| !l.is_empty())
            .map(|l| l.trim_start())
            .collect();
        let want: Vec<String> = keys::BINDINGS
            .iter()
            .map(|b| format!("{PREFIX_LABEL} {}", b.key as char))
            .collect();
        assert_eq!(body.len(), want.len() + 2, "body rows: {body:#?}");
        for (line, key) in body.iter().zip(&want) {
            assert!(
                line.starts_with(key.as_str()),
                "expected {key:?} at the start of {line:?}"
            );
        }
        let literal = body[want.len()];
        assert!(literal.starts_with("Ctrl-t Ctrl-t"), "got {literal:?}");
        assert!(literal.contains("literal"), "got {literal:?}");
        let other = body[want.len() + 1];
        assert!(other.starts_with("Ctrl-t other"), "got {other:?}");
        assert!(other.contains("that key"), "got {other:?}");
    }

    #[test]
    fn descriptions_line_up_in_one_column() {
        let lines = render(80, 24);
        let cols: Vec<usize> = rows()
            .iter()
            .map(|r| {
                let line = line_for(&lines, r);
                let byte = line.find(r.what).expect("description present");
                line[..byte].width()
            })
            .collect();
        assert!(
            cols.windows(2).all(|w| w[0] == w[1]),
            "descriptions start at columns {cols:?}"
        );
    }

    #[test]
    fn the_whole_screen_is_a_centred_table_and_one_hint_line() {
        let lines = render(62, 11);
        assert_eq!(lines.len(), 11);

        // The hint line is the last row, and nothing else is on it.
        assert_eq!(lines[10].trim(), HINTS, "hints should be on the last row");

        // Six rows in a 10-row body: centred means rows 2 to 7.
        let occupied: Vec<usize> = lines[..10]
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.is_empty())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            occupied,
            vec![2, 3, 4, 5, 6, 7],
            "table is not vertically centred: {lines:#?}"
        );

        // And horizontally: the gap on the left should match the gap on the right.
        let row = &lines[2];
        let left = row.len() - row.trim_start().len();
        let right = 62 - row.width();
        assert!(
            left.abs_diff(right) <= 2,
            "not horizontally centred: {left} left, {right} right, row {row:?}"
        );

        // No borders, no title, no header, no box drawing anywhere.
        for line in &lines {
            for ch in "┌┐└┘─│├┤┬┴┼╭╮╰╯═║".chars() {
                assert!(!line.contains(ch), "found border char {ch:?} in {line:?}");
            }
        }
    }

    #[test]
    fn the_hint_row_is_dim_and_the_table_is_not() {
        let rows = rows();
        let mut terminal = Terminal::new(TestBackend::new(62, 11)).expect("terminal");
        terminal.draw(|f| draw(f, &rows)).expect("draw");
        let buf = terminal.backend().buffer().clone();
        let last = buf.area.height - 1;
        for x in 0..buf.area.width {
            let cell = &buf[(x, last)];
            if cell.symbol() != " " {
                assert!(
                    cell.modifier.contains(Modifier::DIM),
                    "hint cell ({x},{last}) is not dim"
                );
            }
        }
        for y in 0..last {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                assert!(
                    !cell
                        .modifier
                        .intersects(Modifier::DIM | Modifier::BOLD | Modifier::REVERSED),
                    "table cell ({x},{y}) carries a modifier"
                );
            }
        }
    }

    #[test]
    fn a_narrow_terminal_truncates_rows_rather_than_wrapping() {
        for (w, h) in [(40u16, 10u16), (20, 5)] {
            let lines = render(w, h);
            assert_eq!(lines.len(), h as usize);
            for line in &lines {
                assert!(line.width() <= w as usize, "{w}x{h}: overflowed: {line:?}");
            }
            // The hint stays alone on the last row, and nothing wrapped onto it.
            let last = &lines[h as usize - 1];
            assert_eq!(last.trim(), HINTS, "{w}x{h}: {lines:#?}");
            assert!(
                !lines[..h as usize - 1].iter().any(|l| l.contains(HINTS)),
                "{w}x{h}: the hint wrapped upward: {lines:#?}"
            );
        }
        // 20x5 has a four-row body, so four table rows show, cut to the width.
        let lines = render(20, 5);
        let body: Vec<&String> = lines[..4].iter().filter(|l| !l.is_empty()).collect();
        assert_eq!(body.len(), 4, "{lines:#?}");
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        for (w, h) in [(1, 1), (2, 1), (1, 2), (0, 0), (80, 1), (3, 3), (10, 2)] {
            let _ = render(w.max(1), h.max(1));
        }
    }

    /// Same rule as the picker: no SGR colour, so the screen inherits the
    /// terminal's palette and is correct under NO_COLOR without testing for it.
    #[test]
    fn nothing_sets_a_colour() {
        use ratatui::style::Color;
        let rows = rows();
        let mut terminal = Terminal::new(TestBackend::new(62, 11)).expect("terminal");
        terminal.draw(|f| draw(f, &rows)).expect("draw");
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
}
