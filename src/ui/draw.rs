//! Rendering. Pure functions of [`App`] state, so the whole screen can be
//! asserted against a `TestBackend` without a terminal.
//!
//! # Colour
//!
//! Nothing here sets a foreground or background colour. Every distinction is
//! carried by a *modifier* — reversed and bold for the selection, dim for the
//! hint line — which means the picker uses the terminal's own palette, inherits
//! its background, and is `NO_COLOR`-correct by construction rather than by
//! remembering to check a flag at each call site.
//!
//! This matters more than it looks. crossterm's own `NO_COLOR` handling turns
//! `SetForegroundColor(c)` into a bare `ESC[m`, which is a *full SGR reset*: it
//! wipes bold, reverse and dim mid-line. Any design that sets colours and then
//! relies on that flag renders incorrectly for exactly the users who asked for
//! no colour.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::app::{App, Mode};

/// The marker on the selected row. Unselected rows are indented to match, so
/// names stay in one column and nothing shifts as the selection moves.
const MARKER: &str = "▸ ";
const INDENT: &str = "  ";

/// The prompt cursor.
const CURSOR: &str = "▋";

/// Only a floor for the degenerate case of every name being empty; the block is
/// otherwise sized to its content. A minimum wider than the content would push
/// short names visibly left of centre, because names are left-aligned *within*
/// the block so they line up with each other.
const MIN_LIST_WIDTH: u16 = 4;
const MAX_LIST_WIDTH: u16 = 48;

const HINTS: &str = "↑↓ move   ⏎ attach   c new   r rename   x kill   q quit";
const EMPTY: &str = "no sessions — press c to create one";

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.height == 0 || area.width == 0 {
        return;
    }

    // The last row is the hint/prompt line; everything above it is the list.
    let list_area = Rect {
        height: area.height.saturating_sub(1),
        ..area
    };
    let bottom = Rect {
        y: area.y + area.height - 1,
        height: 1,
        ..area
    };

    draw_list(frame, app, list_area);
    draw_bottom(frame, app, bottom);
}

fn draw_list(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let visible = app.visible();

    if visible.is_empty() {
        // A single dimmed line, centred, and nothing else.
        let text = truncate(EMPTY, area.width as usize);
        let para = Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().add_modifier(Modifier::DIM),
        )))
        .alignment(Alignment::Center);
        frame.render_widget(para, centre_vertically(area, 1));
        return;
    }

    // Width fits the longest visible name, plus the marker, within sane bounds.
    let widest = visible.iter().map(|s| s.name.width()).max().unwrap_or(0) as u16;
    let width = (widest + MARKER.width() as u16)
        .clamp(MIN_LIST_WIDTH, MAX_LIST_WIDTH)
        .min(area.width);

    // Height fits the entries, or as many as there is room for.
    let height = (visible.len() as u16).min(area.height);
    let block = centre(area, width, height);

    // Scroll only when the list does not fit, keeping the selection in view.
    let offset = scroll_offset(app.selected_index(), visible.len(), height as usize);

    let rows: Vec<Line> = visible
        .iter()
        .enumerate()
        .skip(offset)
        .take(height as usize)
        .map(|(i, session)| {
            let selected = i == app.selected_index();
            let prefix = if selected { MARKER } else { INDENT };
            let style = if selected {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default()
            };
            let text = truncate(&format!("{prefix}{}", session.name), block.width as usize);
            Line::from(Span::styled(text, style))
        })
        .collect();

    frame.render_widget(Paragraph::new(rows), block);
}

fn draw_bottom(frame: &mut Frame, app: &App, area: Rect) {
    // Prompts replace the hint line in place — never a popup, never a border.
    let (text, dim) = match app.mode() {
        Mode::Create { input } => (format!("new session: {input}{CURSOR}"), false),
        Mode::Rename { input, .. } => (format!("rename to: {input}{CURSOR}"), false),
        Mode::Confirm { prompt, .. } => (prompt.clone(), false),
        Mode::Filter => (format!("/{}{CURSOR}", app.filter()), false),
        Mode::Normal => match app.message() {
            Some(msg) => (msg.to_string(), false),
            None if !app.filter().is_empty() => {
                // A filter applied from Normal mode is still worth showing, or
                // the list silently looks shorter than it is.
                (format!("/{}", app.filter()), true)
            }
            None => (HINTS.to_string(), true),
        },
    };

    // Truncate rather than wrap: the hint line owns exactly one row, and
    // wrapping would push the list up and make the layout jump.
    let text = truncate(&text, area.width as usize);
    let style = if dim {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default()
    };
    let para = Paragraph::new(Line::from(Span::styled(text, style))).alignment(Alignment::Center);
    frame.render_widget(para, area);
}

/// Keep `selected` visible within a window of `height` rows.
fn scroll_offset(selected: usize, total: usize, height: usize) -> usize {
    if height == 0 || total <= height {
        return 0;
    }
    if selected < height {
        return 0;
    }
    (selected + 1 - height).min(total - height)
}

/// Centre a `width` x `height` block inside `area`, both axes.
pub(super) fn centre(area: Rect, width: u16, height: u16) -> Rect {
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

fn centre_vertically(area: Rect, height: u16) -> Rect {
    Rect {
        y: area.y + (area.height.saturating_sub(height)) / 2,
        height: height.min(area.height),
        ..area
    }
}

/// Truncate to a display width, counting grapheme width rather than bytes or
/// `char`s so CJK names and emoji do not overflow the block they were measured
/// into.
pub(super) fn truncate(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.to_string().width();
        if w + cw > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Reconstruct the rendered lines.
    ///
    /// A wide glyph occupies two cells: the symbol lands in the first and the
    /// second is a filler. Concatenating every cell would therefore report a CJK
    /// name as three columns per character instead of two, so the filler cell
    /// following a wide symbol is skipped.
    fn render(app: &App, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        terminal.draw(|f| draw(f, app)).expect("draw");
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

    fn app(names: &[&str]) -> App {
        App::new(
            names
                .iter()
                .enumerate()
                .map(|(i, n)| Session::new(format!("id{i:06}"), n.to_string(), 100 + i as u32))
                .collect(),
        )
    }

    #[test]
    fn the_whole_screen_is_a_centred_list_and_one_hint_line() {
        let lines = render(
            &app(&["api-server", "dotfiles", "scratch", "notes"]),
            62,
            11,
        );

        // The hint line is the last row, and nothing else is on it.
        assert!(
            lines[10].contains("attach") && lines[10].contains("quit"),
            "hints should be on the last row, got {:?}",
            lines[10]
        );

        // Exactly the four names appear, and nothing else.
        let content: Vec<&String> = lines[..10].iter().filter(|l| !l.is_empty()).collect();
        assert_eq!(content.len(), 4, "expected 4 rows, got {content:?}");

        // No borders, no title, no header, no box drawing anywhere.
        for line in &lines {
            for ch in "┌┐└┘─│├┤┬┴┼╭╮╰╯═║".chars() {
                assert!(!line.contains(ch), "found border char {ch:?} in {line:?}");
            }
        }
    }

    #[test]
    fn the_selection_is_marked_and_others_are_indented_to_match() {
        let lines = render(&app(&["aaa", "bbb"]), 40, 6);
        let rows: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("aa") || l.contains("bb"))
            .collect();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].contains("▸ aaa"), "selected row: {:?}", rows[0]);
        assert!(rows[1].contains("  bbb"), "unselected row: {:?}", rows[1]);

        // Names line up in one column, so nothing shifts as the selection
        // moves. Measured in display columns: "▸" is three bytes but one column,
        // so byte offsets would differ here even when the layout is correct.
        let col = |l: &str, name: &str| {
            let byte = l.find(name).expect("name present");
            l[..byte].width()
        };
        assert_eq!(col(rows[0], "aaa"), col(rows[1], "bbb"));
    }

    #[test]
    fn the_list_is_centred_on_both_axes() {
        let lines = render(&app(&["one", "two"]), 60, 21);
        let occupied: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(i, l)| *i < 20 && !l.is_empty())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(occupied.len(), 2);

        // Two rows in a 20-row list area: centred means rows 9 and 10.
        assert_eq!(occupied, vec![9, 10], "list is not vertically centred");

        // And horizontally: the gap on the left should match the gap on the right.
        let row = &lines[9];
        let left = row.len() - row.trim_start().len();
        let right = 60 - row.width();
        assert!(
            left.abs_diff(right) <= 2,
            "not horizontally centred: {left} left, {right} right, row {row:?}"
        );
    }

    #[test]
    fn the_empty_state_is_one_dimmed_line_with_hints_still_below() {
        let lines = render(&app(&[]), 60, 9);
        let content: Vec<&String> = lines[..8].iter().filter(|l| !l.is_empty()).collect();
        assert_eq!(content.len(), 1, "expected one line, got {content:?}");
        assert!(content[0].contains("no sessions"), "got {:?}", content[0]);
        assert!(content[0].contains("press c to create one"));
        assert!(lines[8].contains("quit"), "hints should still be present");
    }

    #[test]
    fn prompts_replace_the_hint_line_in_place() {
        let mut a = app(&["dotfiles"]);
        a.on_key(super::super::app::Key::Char('c'));
        for c in "my-project".chars() {
            a.on_key(super::super::app::Key::Char(c));
        }
        let lines = render(&a, 60, 8);
        assert!(
            lines[7].contains("new session: my-project▋"),
            "prompt should be on the bottom row: {:?}",
            lines[7]
        );
        assert!(
            !lines[7].contains("quit"),
            "hints should be replaced, not appended"
        );
        // The list is untouched above it — no popup, no border, no shift.
        assert!(lines[..7].iter().any(|l| l.contains("dotfiles")));
    }

    #[test]
    fn the_kill_confirm_names_the_session() {
        let mut a = app(&["dotfiles"]);
        a.on_key(super::super::app::Key::Char('x'));
        let lines = render(&a, 60, 6);
        assert!(
            lines[5].contains(r#"kill "dotfiles"? [y/N]"#),
            "got {:?}",
            lines[5]
        );
    }

    #[test]
    fn the_filter_prompt_shows_the_query_with_a_cursor() {
        let mut a = app(&["api-server", "dotfiles"]);
        a.on_key(super::super::app::Key::Char('/'));
        for c in "dot".chars() {
            a.on_key(super::super::app::Key::Char(c));
        }
        let lines = render(&a, 60, 6);
        assert!(lines[5].contains("/dot▋"), "got {:?}", lines[5]);
        // ...and the list really has narrowed.
        assert!(lines[..5].iter().any(|l| l.contains("dotfiles")));
        assert!(!lines[..5].iter().any(|l| l.contains("api-server")));
    }

    #[test]
    fn a_narrow_terminal_truncates_the_hints_rather_than_wrapping() {
        let lines = render(&app(&["one"]), 20, 5);
        assert_eq!(lines.len(), 5);
        // The hint line still occupies exactly one row: row 3 must stay part of
        // the list area, not become spillover from the hints.
        assert!(
            lines[4].width() <= 20,
            "hint row overflowed: {:?}",
            lines[4]
        );
        assert!(
            !lines[3].contains("quit"),
            "hints wrapped upward: {:?}",
            lines[3]
        );
    }

    #[test]
    fn long_names_are_truncated_to_the_block() {
        let long = "a-really-quite-extraordinarily-long-session-name-that-goes-on";
        let lines = render(&app(&[long]), 40, 5);
        for line in &lines {
            assert!(line.width() <= 40, "line overflowed the terminal: {line:?}");
        }
    }

    #[test]
    fn wide_characters_do_not_overflow() {
        // CJK names are two columns per character; a byte- or char-based width
        // calculation would overflow the terminal here.
        let lines = render(&app(&["日本語のセッション名前", "notes"]), 30, 6);
        for line in &lines {
            assert!(
                line.width() <= 30,
                "overflowed: {line:?} ({} cols)",
                line.width()
            );
        }
    }

    #[test]
    fn a_list_longer_than_the_screen_scrolls_to_keep_the_selection_visible() {
        let names: Vec<String> = (0..30).map(|i| format!("session-{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut a = app(&refs);
        a.on_key(super::super::app::Key::Char('G')); // last entry

        let lines = render(&a, 40, 10);
        assert!(
            lines.iter().any(|l| l.contains("▸ session-29")),
            "the selection scrolled out of view: {lines:#?}"
        );
    }

    #[test]
    fn scroll_offset_keeps_the_selection_in_the_window() {
        assert_eq!(scroll_offset(0, 30, 10), 0);
        assert_eq!(scroll_offset(5, 30, 10), 0, "no scroll while it still fits");
        assert_eq!(scroll_offset(9, 30, 10), 0);
        assert_eq!(scroll_offset(10, 30, 10), 1);
        assert_eq!(scroll_offset(29, 30, 10), 20, "last item, last window");
        assert_eq!(scroll_offset(5, 5, 10), 0, "shorter than the window");
        assert_eq!(scroll_offset(0, 0, 0), 0, "degenerate case must not panic");
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        for (w, h) in [(1, 1), (2, 1), (1, 2), (0, 0), (80, 1), (3, 3)] {
            let _ = render(&app(&["one", "two"]), w.max(1), h.max(1));
            let _ = render(&app(&[]), w.max(1), h.max(1));
        }
    }

    /// The picker must never emit an SGR colour, so it inherits the terminal's
    /// palette and background and is correct under NO_COLOR without needing to
    /// test for it.
    #[test]
    fn nothing_sets_a_colour() {
        use ratatui::style::Color;
        let mut a = app(&["one", "two", "three"]);
        a.on_key(super::super::app::Key::Char('j'));
        let mut terminal = Terminal::new(TestBackend::new(50, 8)).expect("terminal");
        terminal.draw(|f| draw(f, &a)).expect("draw");
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

    #[test]
    fn the_selected_row_is_reversed_and_bold() {
        let a = app(&["one", "two"]);
        let mut terminal = Terminal::new(TestBackend::new(40, 6)).expect("terminal");
        terminal.draw(|f| draw(f, &a)).expect("draw");
        let buf = terminal.backend().buffer().clone();
        let marked = (0..buf.area.height)
            .find(|y| (0..buf.area.width).any(|x| buf[(x, *y)].symbol() == "▸"))
            .expect("a marked row");
        let cell = (0..buf.area.width)
            .map(|x| &buf[(x, marked)])
            .find(|c| c.symbol() == "▸")
            .expect("marker cell");
        assert!(cell.modifier.contains(Modifier::REVERSED));
        assert!(cell.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn truncate_counts_display_width() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 3), "hel");
        assert_eq!(truncate("日本語", 4), "日本", "two columns per character");
        assert_eq!(truncate("日本語", 3), "日", "must not split a wide char");
        assert_eq!(truncate("", 0), "");
    }
}
